//! The bridge from the privileged watcher to the engine.
//!
//! The watcher connects to our socket and streams fanotify records for a whole
//! filesystem. Each folder keeps a [`HandleMap`] (directory handle → path,
//! built with the unprivileged `name_to_handle_at`), which turns a record into
//! a path under its root or into "not ours". Paths become [`Event`]s.
//!
//! A (re)connection means events may have been missed, so every folder gets
//! one [`Event::Overflow`]: a metadata-only reconcile that hashes nothing that
//! did not change.

use crate::node::Event;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc::UnboundedSender;
use vuoxi_core::RelPath;
use vuoxi_rules::{Decision, Rules, Tag};
use vuoxi_watcher::record::HELLO;
use vuoxi_watcher::{HandleMap, Record, Resolved};

pub struct Watched {
    pub id: String,
    pub root: PathBuf,
    pub tag: Tag,
}

struct Slot {
    id: String,
    rules: Rules,
    map: HandleMap,
}

fn skipper(rules: &mut Rules) -> impl FnMut(&str) -> bool + '_ {
    |r: &str| RelPath::new(r).is_ok_and(|p| matches!(rules.dir(&p), Decision::Skip(_)))
}

fn to_event(r: Resolved) -> Option<Event> {
    let rel = |s: String| RelPath::new(&s).ok();
    Some(match r {
        Resolved::Changed { rel: r, .. } => Event::Changed(rel(r)?),
        Resolved::Removed { rel: r, .. } => Event::Removed(rel(r)?),
        Resolved::Moved { from, to, .. } => match (rel(from), rel(to)) {
            (Some(from), Some(to)) => Event::Moved { from, to },
            (None, Some(to)) => Event::Changed(to),
            (Some(from), None) => Event::Removed(from),
            (None, None) => return None,
        },
        Resolved::Overflow => Event::Overflow,
        Resolved::Outside => return None,
    })
}

/// Listen on `socket` for the watcher, forever, on a thread of its own.
pub fn spawn(
    socket: &Path,
    folders: Vec<Watched>,
    events: UnboundedSender<(String, Event)>,
) -> io::Result<std::thread::JoinHandle<()>> {
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;

    let mut slots = Vec::new();
    for f in folders {
        let mut rules = Rules::new(&f.root, f.tag);
        let map = HandleMap::build(&f.root, skipper(&mut rules))?;
        slots.push(Slot {
            id: f.id,
            rules,
            map,
        });
    }

    Ok(std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let mut hello = [0u8; 4];
            if conn.read_exact(&mut hello).is_err() || hello != HELLO {
                continue;
            }
            for s in &mut slots {
                let _ = s.map.rebuild(skipper(&mut s.rules));
                let _ = events.send((s.id.clone(), Event::Overflow));
            }
            if pump(&mut conn, &mut slots, &events).is_err() {
                eprintln!("vuoxi: watcher stream ended badly; waiting for it to reconnect");
            }
        }
    }))
}

fn pump(
    conn: &mut impl Read,
    slots: &mut [Slot],
    events: &UnboundedSender<(String, Event)>,
) -> io::Result<()> {
    let mut buf = Vec::with_capacity(1 << 16);
    let mut chunk = vec![0u8; 1 << 16];
    let mut out = Vec::new();
    loop {
        let n = conn.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        let mut at = 0;
        while let Some((rec, used)) = Record::decode(&buf[at..])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))?
        {
            at += used;
            for s in slots.iter_mut().filter(|s| s.map.fsid() == rec.fsid) {
                let Slot { rules, map, id } = s;
                map.resolve(&rec, skipper(rules), &mut out);
                deliver(id, map, rules, &mut out, events);
            }
        }
        buf.drain(..at);
        for s in slots.iter_mut() {
            s.map.flush(&mut out);
            let Slot { rules, map, id } = s;
            deliver(id, map, rules, &mut out, events);
        }
    }
}

fn deliver(
    id: &str,
    map: &mut HandleMap,
    rules: &mut Rules,
    out: &mut Vec<Resolved>,
    events: &UnboundedSender<(String, Event)>,
) {
    for r in out.drain(..) {
        if matches!(r, Resolved::Overflow) {
            let _ = map.rebuild(skipper(rules));
        }
        if let Some(ev) = to_event(r) {
            let _ = events.send((id.to_owned(), ev));
        }
    }
}
