//! `vuoxi run`: the sync engine for the folders in the config.

use crate::args::Opts;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use vuoxi_config::{Config, DeviceKey, FolderConfig};
use vuoxi_daemon::api;
use vuoxi_daemon::watch::{self, Watched};
use vuoxi_daemon::{Event, FolderSpec, Node, NodeConfig, Offers, PeerSpec, RulesFilter, parse_tag};

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

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async move {
        // These outlive engine restarts: pending folder offers, the watcher's
        // socket (so the privileged watcher stays connected), the uptime.
        let offers = Arc::new(Offers::default());
        let reload = Arc::new(Notify::new());
        let control: Arc<dyn api::Control> = Arc::new(Ctl {
            path: path.clone(),
            reload: reload.clone(),
            lock: Mutex::new(()),
        });
        let connected = Arc::new(AtomicBool::new(false));
        let mut watch: Option<watch::Watch> = None;
        let started = std::time::Instant::now();
        loop {
            let cfg = Config::load(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let stop = engine(
                &cfg, &key, &offers, &control, &connected, &mut watch, started, &reload,
            )
            .await?;
            if stop {
                return Ok(());
            }
            eprintln!("vuoxi: config changed; restarting the engine");
        }
    })
}

/// Run the engine for `cfg` until ctrl-c (true) or a config change (false).
#[allow(clippy::too_many_arguments)]
async fn engine(
    cfg: &Config,
    key: &DeviceKey,
    offers: &Arc<Offers>,
    control: &Arc<dyn api::Control>,
    connected: &Arc<AtomicBool>,
    watch: &mut Option<watch::Watch>,
    started: std::time::Instant,
    reload: &Notify,
) -> Result<bool, String> {
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
            tag: f.tag.clone().unwrap_or_default(),
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
        offers: offers.clone(),
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
    match (watch.as_ref(), &cfg.watcher_socket) {
        (Some(w), _) => w
            .retarget(watched, node.events())
            .map_err(|e| format!("watcher: {e}"))?,
        (None, Some(sock)) => {
            eprintln!(
                "vuoxi: waiting for the watcher on {}; polling meanwhile",
                sock.display()
            );
            *watch = Some(
                watch::spawn(sock, watched, node.events(), connected.clone())
                    .map_err(|e| format!("watcher socket: {e}"))?,
            );
        }
        (None, None) => eprintln!("vuoxi: no watcher socket configured; polling"),
    }
    let secs = std::env::var("VUOXI_POLL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_POLL_SECS);
    let tx = node.events();
    let ids: Vec<String> = cfg.folders.iter().map(|f| f.id.clone()).collect();
    let watcher_up = connected.clone();
    let poll = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            if !watcher_up.load(Ordering::Relaxed) {
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
        started,
        control: control.clone(),
    };
    let (n, listen) = (node.clone(), cfg.ui_listen);
    let serve = tokio::spawn(async move {
        if let Err(e) = api::serve(n, Arc::new(ctx), listen).await {
            eprintln!("vuoxi: dashboard API on {listen}: {e}");
        }
    });
    eprintln!(
        "vuoxi: dashboard on http://{} (`vuoxi ui` prints the login link)",
        cfg.ui_listen
    );

    let stop = tokio::select! {
        r = tokio::signal::ctrl_c() => { r.map_err(|e| e.to_string())?; true }
        _ = reload.notified() => false,
    };
    // Everything holding the node goes before the next one binds the ports.
    poll.abort();
    serve.abort();
    let _ = poll.await;
    let _ = serve.await;
    node.shutdown().await;
    if Arc::into_inner(node).is_none() {
        eprintln!("vuoxi: engine still referenced at restart");
    }
    if stop {
        eprintln!("vuoxi: stopping");
    }
    Ok(stop)
}

/// The dashboard's config changes: edit the file, then restart the engine.
struct Ctl {
    path: std::path::PathBuf,
    reload: Arc<Notify>,
    lock: Mutex<()>,
}

impl Ctl {
    fn edit(&self, f: impl FnOnce(&mut Config) -> Result<(), String>) -> Result<(), String> {
        let _g = self.lock.lock().unwrap();
        let mut c = Config::load(&self.path).map_err(|e| e.to_string())?;
        f(&mut c)?;
        c.save(&self.path).map_err(|e| e.to_string())?;
        // Let the API answer before the engine (and the API) restart.
        let reload = self.reload.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            reload.notify_one();
        });
        Ok(())
    }
}

fn tag_name(t: vuoxi_rules::Tag) -> &'static str {
    use vuoxi_rules::Tag::*;
    match t {
        Code => "code",
        Secrets => "secrets",
        Photos => "photos",
        Media => "media",
        Notes => "notes",
        Archive => "archive",
    }
}

fn detect_tag(path: &std::path::Path) -> String {
    tag_name(
        vuoxi_rules::detect(path, 2000)
            .tag
            .unwrap_or(vuoxi_rules::Tag::Code),
    )
    .into()
}

fn check_tag(t: &str) -> Result<(), String> {
    parse_tag(t)
        .map(|_| ())
        .ok_or_else(|| format!("unknown tag {t:?}"))
}

impl api::Control for Ctl {
    fn add_folder(&self, f: api::NewFolder) -> Result<(), String> {
        if let Some(t) = &f.tag {
            check_tag(t)?;
        }
        if !f.path.is_absolute() {
            return Err("path must be absolute or start with ~/".into());
        }
        self.edit(|c| {
            // Validate before touching the disk.
            let mut probe = c.clone();
            let fc = FolderConfig {
                id: f.id.clone(),
                path: f.path.clone(),
                tag: Some("code".into()),
                peers: f.peers.clone(),
                read_only_peers: f.ro_peers.clone(),
                key: None,
            };
            probe.add_folder(fc.clone()).map_err(|e| e.to_string())?;
            std::fs::create_dir_all(&f.path).map_err(|e| format!("{}: {e}", f.path.display()))?;
            let tag = f.tag.clone().unwrap_or_else(|| detect_tag(&f.path));
            c.add_folder(FolderConfig {
                tag: Some(tag),
                key: Some(f.key.unwrap_or_else(|| DeviceKey::generate().secret)),
                ..fc
            })
            .map_err(|e| e.to_string())
        })
    }

    fn set_sharing(
        &self,
        id: &str,
        peers: Vec<String>,
        ro_peers: Vec<String>,
        tag: Option<Option<String>>,
    ) -> Result<(), String> {
        if let Some(Some(t)) = &tag {
            check_tag(t)?;
        }
        self.edit(|c| {
            let f = c
                .folders
                .iter_mut()
                .find(|f| f.id == id)
                .ok_or_else(|| format!("no folder {id}"))?;
            f.peers = peers;
            f.read_only_peers = ro_peers;
            match tag {
                Some(Some(t)) => f.tag = Some(t),
                Some(None) => f.tag = Some(detect_tag(&f.path)),
                None => {}
            }
            Ok(())
        })
    }

    fn remove_folder(&self, id: &str) -> Result<(), String> {
        self.edit(|c| {
            let n = c.folders.len();
            c.folders.retain(|f| f.id != id);
            if c.folders.len() == n {
                return Err(format!("no folder {id}"));
            }
            Ok(())
        })
    }
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
