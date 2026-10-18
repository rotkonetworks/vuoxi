//! `vuoxi run`: the sync engine for the folders in the config.

use crate::args::Opts;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use vuoxi_config::{Config, DeviceKey};
use vuoxi_daemon::api;
use vuoxi_daemon::watch::{self, Watched};
use vuoxi_daemon::{Event, FolderSpec, Node, NodeConfig, PeerSpec, RulesFilter, parse_tag};

const DEFAULT_POLL_SECS: u64 = 60;

pub fn run(o: &Opts) -> i32 {
    match go(o) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("vuoxi run: {e}");
            1
        }
    }
}

fn go(o: &Opts) -> Result<(), String> {
    let path = o.config.clone().unwrap_or_else(Config::default_path);
    let cfg = Config::load(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let key_path = match &o.config {
        Some(p) => Config::key_path_beside(p),
        None => Config::key_path(),
    };
    let key =
        DeviceKey::load_or_create(&key_path).map_err(|e| format!("{}: {e}", key_path.display()))?;
    if let Some(dir) = &cfg.relay_dir {
        return relay(&cfg, key, dir);
    }
    if cfg.folders.is_empty() {
        return Err("no folders configured; see `vuoxi folder add`".into());
    }

    let tag_of = |t: &Option<String>| match t.as_deref() {
        None => Ok(vuoxi_rules_tag_default()),
        Some(s) => parse_tag(s).ok_or_else(|| format!("unknown tag {s:?}")),
    };
    let mut folders = Vec::new();
    let mut watched = Vec::new();
    for f in &cfg.folders {
        let tag = tag_of(&f.tag)?;
        folders.push(FolderSpec {
            id: f.id.clone(),
            root: f.path.clone(),
            filter: Box::new(RulesFilter::new(&f.path, tag)),
            key: f.key,
        });
        watched.push(Watched {
            id: f.id.clone(),
            root: f.path.clone(),
            tag,
        });
    }
    let peers = cfg
        .peers
        .iter()
        .map(|p| PeerSpec {
            id: p.id,
            addrs: p.addrs.clone(),
            folders: cfg
                .folders
                .iter()
                .filter(|f| f.peers.contains(&p.name))
                .map(|f| f.id.clone())
                .collect(),
            read_only: cfg
                .folders
                .iter()
                .filter(|f| f.read_only_peers.contains(&p.name))
                .map(|f| f.id.clone())
                .collect(),
            relay: p.relay,
        })
        .collect();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async move {
        let node = Node::start(NodeConfig {
            secret: key.secret,
            bind: cfg.listen.clone(),
            state_dir: cfg.state_dir.clone(),
            folders,
            peers,
            pull_every: Duration::from_secs(30),
            power_root: Some("/sys/class/power_supply".into()),
            trash_keep: Duration::from_secs(u64::from(cfg.trash_days) * 86400),
            relay_keep: Duration::from_secs(u64::from(cfg.trash_days) * 86400),
        })
        .await
        .map_err(|e| format!("start: {e}"))?;
        let node = Arc::new(node);
        eprintln!(
            "vuoxi: {} listening on {:?}; folders: {}",
            vuoxi_config::encode_id(&node.peer_id()),
            node.local_addrs(),
            cfg.folders
                .iter()
                .map(|f| format!("{}={}", f.id, f.path.display()))
                .collect::<Vec<_>>()
                .join(", ")
        );

        // The watcher drives changes when it is connected; otherwise a metadata
        // poll does, so a missing or crashed watcher degrades to slower sync,
        // never to none.
        let connected = Arc::new(AtomicBool::new(false));
        let _watch = match &cfg.watcher_socket {
            Some(sock) => {
                eprintln!(
                    "vuoxi: waiting for the watcher on {}; polling meanwhile",
                    sock.display()
                );
                Some(
                    watch::spawn(sock, watched, node.events(), connected.clone())
                        .map_err(|e| format!("watcher socket: {e}"))?,
                )
            }
            None => {
                eprintln!("vuoxi: no watcher socket configured; polling");
                None
            }
        };
        let secs = std::env::var("VUOXI_POLL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_POLL_SECS);
        let tx = node.events();
        let ids: Vec<String> = cfg.folders.iter().map(|f| f.id.clone()).collect();
        let watcher_up = connected.clone();
        tokio::spawn(async move {
            let connected = watcher_up;
            loop {
                tokio::time::sleep(Duration::from_secs(secs)).await;
                if !connected.load(Ordering::Relaxed) {
                    for id in &ids {
                        let _ = tx.send((id.clone(), Event::Overflow));
                    }
                }
            }
        });

        let ctx = api::ApiCtx {
            token: api::token(&cfg.state_dir).map_err(|e| format!("ui token: {e}"))?,
            device_id: vuoxi_config::encode_id(&node.peer_id()),
            ui_dir: ui_dir(),
            peers: cfg
                .peers
                .iter()
                .map(|p| {
                    let info = api::PeerInfo {
                        name: p.name.clone(),
                        id: vuoxi_config::encode_id(&p.id),
                        addrs: p.addrs.iter().map(|a| a.to_string()).collect(),
                    };
                    (p.id, info)
                })
                .collect(),
            folders: cfg
                .folders
                .iter()
                .map(|f| {
                    let info = api::FolderInfo {
                        tag: f.tag.clone().unwrap_or_else(|| "code".into()),
                        peers: f.peers.clone(),
                        read_only_peers: f.read_only_peers.clone(),
                    };
                    (f.id.clone(), info)
                })
                .collect(),
            watcher: connected.clone(),
            started: std::time::Instant::now(),
        };
        let (n, listen) = (node.clone(), cfg.ui_listen);
        tokio::spawn(async move {
            if let Err(e) = api::serve(n, Arc::new(ctx), listen).await {
                eprintln!("vuoxi: dashboard API on {listen}: {e}");
            }
        });
        eprintln!(
            "vuoxi: dashboard on http://{} (`vuoxi ui` prints the login link)",
            cfg.ui_listen
        );

        tokio::signal::ctrl_c().await.map_err(|e| e.to_string())?;
        eprintln!("vuoxi: stopping");
        node.shutdown().await;
        Ok(())
    })
}

/// Folders without a tag are treated as `code`: build output is skipped,
/// everything else syncs.
fn vuoxi_rules_tag_default() -> vuoxi_rules::Tag {
    vuoxi_rules::Tag::Code
}

/// Relay mode: serve the configured devices' sealed folders from `dir`.
/// No folders, no folder keys: this machine cannot read what it stores.
fn relay(cfg: &Config, key: DeviceKey, dir: &std::path::Path) -> Result<(), String> {
    use vuoxi_daemon::{RelayHandler, RelayStore};
    use vuoxi_transport_iroh::{Allowlist, Config as TransportConfig, IrohTransport};
    if !cfg.folders.is_empty() {
        return Err("a relay holds no folders; remove them or unset relay_dir".into());
    }
    let store = RelayStore::open(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async move {
        let mut tc =
            TransportConfig::new(key.secret, Allowlist::new(cfg.peers.iter().map(|p| p.id)));
        tc.bind = cfg.listen.clone();
        let t = IrohTransport::bind(tc).await.map_err(|e| e.to_string())?;
        let _serve = t.serve(RelayHandler(store));
        eprintln!(
            "vuoxi relay: {} listening on {:?}; storing in {}; devices: {}",
            vuoxi_config::encode_id(&t.peer_id()),
            t.local_addrs(),
            dir.display(),
            cfg.peers
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        tokio::signal::ctrl_c().await.map_err(|e| e.to_string())?;
        t.close().await;
        Ok(())
    })
}

/// Where the dashboard's files are: `VUOXI_UI_DIR`, else `$XDG_DATA_HOME/vuoxi/ui`.
pub fn ui_dir() -> std::path::PathBuf {
    if let Some(d) = std::env::var_os("VUOXI_UI_DIR") {
        return d.into();
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".local/share")
        });
    data.join("vuoxi/ui")
}

/// `vuoxi ui`: print the dashboard's login link.
pub fn ui_link(o: &Opts) -> i32 {
    let path = o.config.clone().unwrap_or_else(Config::default_path);
    match Config::load(&path)
        .map_err(|e| e.to_string())
        .and_then(|c| {
            api::token(&c.state_dir)
                .map(|t| (c, t))
                .map_err(|e| e.to_string())
        }) {
        Ok((c, t)) => {
            println!("http://{}/?t={t}", c.ui_listen);
            0
        }
        Err(e) => {
            eprintln!("vuoxi ui: {e}");
            1
        }
    }
}
