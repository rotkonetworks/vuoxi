//! Commands that touch only local files (key and config), never the daemon.

use crate::args::{Cmd, Opts};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use vuoxi_config::{Config, DeviceKey, FolderConfig, PeerConfig, decode_id, encode_id};

fn config_path(o: &Opts) -> PathBuf {
    o.config.clone().unwrap_or_else(Config::default_path)
}

/// The key lives beside the config, so `--config` moves both.
fn key_path(o: &Opts) -> PathBuf {
    match &o.config {
        Some(c) => Config::key_path_beside(c),
        None => Config::key_path(),
    }
}

fn emit(json_out: bool, v: Value, text: String) {
    if json_out {
        println!("{v}");
    } else {
        println!("{text}");
    }
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        "-".into()
    } else {
        items.join(",")
    }
}

pub fn run(cmd: Cmd, o: &Opts, json_out: bool) -> i32 {
    match go(cmd, o, json_out) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("vuoxi: {e}");
            1
        }
    }
}

fn go(cmd: Cmd, o: &Opts, json_out: bool) -> Result<(), String> {
    let cpath = config_path(o);
    let kpath = key_path(o);
    let key =
        || DeviceKey::load_or_create(&kpath).map_err(|e| format!("key {}: {e}", kpath.display()));
    match cmd {
        Cmd::ConfigPath => emit(
            json_out,
            json!({"kind": "config_path", "schema": 1, "path": cpath}),
            cpath.display().to_string(),
        ),
        Cmd::Init => {
            let key_new = !kpath.exists();
            let id = encode_id(&key()?.peer_id());
            let config_new = !cpath.exists();
            if config_new {
                Config::default()
                    .save(&cpath)
                    .map_err(|e| format!("save {}: {e}", cpath.display()))?;
            } else {
                Config::load(&cpath).map_err(|e| e.to_string())?;
            }
            let state = |new| if new { "created" } else { "kept" };
            emit(
                json_out,
                json!({"kind": "init", "schema": 1, "id": id,
                       "key": kpath, "key_created": key_new,
                       "config": cpath, "config_created": config_new}),
                format!(
                    "id={id} key={} config={}",
                    state(key_new),
                    state(config_new)
                ),
            );
        }
        Cmd::Id => {
            if !kpath.exists() {
                return Err(format!(
                    "no device key at {}; run `vuoxi init`",
                    kpath.display()
                ));
            }
            let id = encode_id(&key()?.peer_id());
            emit(
                json_out,
                json!({"kind": "device", "schema": 1, "id": id}),
                id,
            );
        }
        Cmd::Pair { name, id, addrs } => {
            let id = decode_id(&id).map_err(|e| e.to_string())?;
            let addrs = addrs
                .iter()
                .map(|a| {
                    a.parse::<SocketAddr>().map_err(|_| {
                        format!("bad address {a:?}; expected host:port, e.g. [2001:db8::1]:41641")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let mut c = Config::load(&cpath).map_err(|e| e.to_string())?;
            let old: Vec<_> = c
                .peers
                .iter()
                .filter(|p| p.name == name || p.id == id)
                .map(|p| p.name.clone())
                .collect();
            let change = if old.is_empty() { "added" } else { "replaced" };
            c.add_peer(PeerConfig {
                name: name.clone(),
                id,
                addrs: addrs.clone(),
            })
            .map_err(|e| e.to_string())?;
            // Folders naming a peer that was renamed away would dangle.
            for f in &mut c.folders {
                for p in &mut f.peers {
                    if old.contains(p) {
                        *p = name.clone();
                    }
                }
                f.peers.dedup();
            }
            save(&c, &cpath)?;
            let id = encode_id(&id);
            let addrs: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
            emit(
                json_out,
                json!({"kind": "pair", "schema": 1, "change": change,
                       "name": name, "id": id, "addrs": addrs}),
                format!("{change} {name} id={id} addrs={}", list(&addrs)),
            );
        }
        Cmd::FolderAdd { id, path } => {
            let path =
                std::path::absolute(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let mut c = Config::load(&cpath).map_err(|e| e.to_string())?;
            for p in &o.peers {
                if !c.peers.iter().any(|q| &q.name == p) {
                    return Err(format!("unknown peer {p:?}; `vuoxi pair` it first"));
                }
            }
            c.add_folder(FolderConfig {
                id: id.clone(),
                path: path.clone(),
                tag: o.tag.clone(),
                peers: o.peers.clone(),
            })
            .map_err(|e| e.to_string())?;
            save(&c, &cpath)?;
            emit(
                json_out,
                json!({"kind": "folder_added", "schema": 1, "id": id, "path": path,
                       "tag": o.tag, "peers": o.peers}),
                format!(
                    "added {id} path={} tag={} peers={}",
                    path.display(),
                    o.tag.as_deref().unwrap_or("-"),
                    list(&o.peers)
                ),
            );
        }
        Cmd::FolderList => {
            let c = Config::load(&cpath).map_err(|e| e.to_string())?;
            let items: Vec<Value> = c
                .folders
                .iter()
                .map(|f| json!({"id": f.id, "path": f.path, "tag": f.tag, "peers": f.peers}))
                .collect();
            let lines: Vec<String> = c
                .folders
                .iter()
                .map(|f| {
                    format!(
                        "{} path={} tag={} peers={}",
                        f.id,
                        f.path.display(),
                        f.tag.as_deref().unwrap_or("-"),
                        list(&f.peers)
                    )
                })
                .collect();
            emit(
                json_out,
                json!({"kind": "folders", "schema": 1, "items": items}),
                if lines.is_empty() {
                    "(none)".into()
                } else {
                    lines.join("\n")
                },
            );
        }
        _ => unreachable!("routed by main"),
    }
    Ok(())
}

fn save(c: &Config, path: &Path) -> Result<(), String> {
    c.save(path)
        .map_err(|e| format!("save {}: {e}", path.display()))
}
