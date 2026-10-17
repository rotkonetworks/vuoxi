//! `vuoxi why PATH`: which rule decides whether a path syncs, and under which tag.

use crate::args::Opts;
use serde_json::json;
use std::io::Write;
use std::path::Path;
use vuoxi_config::Config;
use vuoxi_core::RelPath;
use vuoxi_daemon::parse_tag;
use vuoxi_rules::{Decision, Reason, Rules, Tag};

fn reason(r: &Reason) -> String {
    match r {
        Reason::Keep(p) => format!("keep pattern {p}"),
        Reason::CacheDirTag => "CACHEDIR.TAG".into(),
        Reason::Rebuildable(what) => format!("rebuildable: {what}"),
        Reason::Ignore(p) if p.starts_with("inside ") => p.clone(),
        Reason::Ignore(p) => format!("ignore pattern {p}"),
        Reason::GitDir => ".git".into(),
        Reason::Gitignored(p) => format!(".gitignore: {p}"),
        Reason::Default => "default".into(),
    }
}

fn tag_name(t: Tag) -> String {
    serde_json::to_value(t)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

pub fn run(path: &Path, o: &Opts, json_out: bool) -> i32 {
    let cfg_path = o.config.clone().unwrap_or_else(Config::default_path);
    let cfg = match Config::load(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("vuoxi why: {}: {e}", cfg_path.display());
            return 1;
        }
    };
    let abs = match std::path::absolute(path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("vuoxi why: {}: {e}", path.display());
            return 2;
        }
    };
    let Some(folder) = cfg.folders.iter().find(|f| abs.starts_with(&f.path)) else {
        eprintln!(
            "vuoxi why: {} is not under a configured folder",
            abs.display()
        );
        return 3;
    };
    let tag = folder
        .tag
        .as_deref()
        .and_then(parse_tag)
        .unwrap_or(Tag::Code);
    let rel_s = abs
        .strip_prefix(&folder.path)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    if rel_s.is_empty() {
        println!("{}: folder root, tag {}", folder.id, tag_name(tag));
        return 0;
    }
    let Ok(rel) = RelPath::new(&rel_s) else {
        eprintln!("vuoxi why: {rel_s:?} cannot be synced (not a valid relative path)");
        return 2;
    };
    let mut rules = Rules::new(&folder.path, tag);
    let w = rules.why(&rel, abs.is_dir());
    // A path inside a skipped directory is skipped whatever its own rule says.
    let mut skipped_by: Option<(RelPath, Reason)> = None;
    let mut dir = rel.parent();
    let mut ancestors = Vec::new();
    while let Some(d) = dir {
        dir = d.parent();
        ancestors.push(d);
    }
    for d in ancestors.into_iter().rev() {
        if let Decision::Skip(r) = rules.dir(&d) {
            skipped_by = Some((d, r));
            break;
        }
    }
    let inherited;
    let (verdict, r) = match (&skipped_by, &w.decision) {
        (Some((d, r)), _) => {
            inherited = Reason::Ignore(format!("inside {d}/: {}", reason(r)));
            ("skip", &inherited)
        }
        (None, Decision::Sync(r)) => ("sync", r),
        (None, Decision::Skip(r)) => ("skip", r),
    };
    let chain: Vec<_> = w
        .chain
        .iter()
        .map(|l| {
            json!({
                "dir": l.dir.as_ref().map(RelPath::as_str).unwrap_or(""),
                "tag": tag_name(l.policy.tag),
                "from_vuoxi": l.from_vuoxi,
                "unsealed": l.unsealed,
            })
        })
        .collect();
    let v = json!({
        "kind": "why",
        "schema": 1,
        "folder": folder.id,
        "path": rel.as_str(),
        "decision": verdict,
        "reason": reason(r),
        "tag": tag_name(w.policy.tag),
        "relays": w.policy.relays,
        "agents_read": w.policy.agents_read,
        "secret_file": w.secret_file,
        "chain": chain,
    });
    // Piped into `head`: a closed stdout is not an error worth a panic.
    let mut out = std::io::stdout().lock();
    if json_out {
        let _ = writeln!(out, "{v}");
    } else {
        let _ = writeln!(
            out,
            "{verdict} {}/{}  ({})  tag={} relays={} agents={}{}",
            folder.id,
            rel,
            reason(r),
            tag_name(w.policy.tag),
            w.policy.relays,
            w.policy.agents_read,
            if w.secret_file { " secret-file" } else { "" }
        );
        let links: Vec<String> = w
            .chain
            .iter()
            .map(|l| {
                let d = l.dir.as_ref().map_or("/".into(), |d| format!("{d}/"));
                let src = if l.from_vuoxi { ".vuoxi" } else { "folder" };
                format!("{d}={}({src})", tag_name(l.policy.tag))
            })
            .collect();
        let _ = writeln!(out, "chain {}", links.join(" > "));
    }
    0
}
