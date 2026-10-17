//! `vuoxi run`: the sync engine for the folders in the config.

use crate::args::Opts;
use std::time::Duration;
use vuoxi_config::{Config, DeviceKey};
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
        })
        .await
        .map_err(|e| format!("start: {e}"))?;
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

        let _watch = match &cfg.watcher_socket {
            Some(sock) => {
                eprintln!("vuoxi: waiting for the watcher on {}", sock.display());
                Some(
                    watch::spawn(sock, watched, node.events())
                        .map_err(|e| format!("watcher socket: {e}"))?,
                )
            }
            None => {
                let secs = std::env::var("VUOXI_POLL_SECS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(DEFAULT_POLL_SECS);
                eprintln!("vuoxi: no watcher socket configured; reconciling every {secs}s");
                let tx = node.events();
                let ids: Vec<String> = cfg.folders.iter().map(|f| f.id.clone()).collect();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(secs)).await;
                        for id in &ids {
                            let _ = tx.send((id.clone(), Event::Overflow));
                        }
                    }
                });
                None
            }
        };

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
