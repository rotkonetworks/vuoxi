//! What syncs under a folder root.
//!
//! Decisions follow `docs/DESIGN.md` ("What to sync" and "Tags"). For a
//! directory, first match wins: `keep` patterns, `CACHEDIR.TAG`, the
//! rebuildable table, `ignore` patterns, `.git`, then `.gitignore` if the
//! effective policy says `exclude`. Files skip the two build-output checks.
//!
//! Notes on choices the design leaves open:
//! - A `CACHEDIR.TAG` only counts with the standard signature line, so a stray
//!   file of that name cannot hide a directory.
//! - `secrets` and `archive` folders have no skip rules, so the `CACHEDIR.TAG`
//!   and rebuildable checks do not apply under them.
//! - Secret-looking files are never dropped by `.gitignore` exclusion: they are
//!   what `.gitignore` most often lists and what cannot be rebuilt.
//! - `notes` skips editor swap files and `photos`/`media` skip thumbnail
//!   caches, as `Reason::Ignore("tag:...")`.

#![forbid(unsafe_code)]

mod detect;
mod secret;
mod vuoxi_file;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::{Deserialize, Serialize};
use vuoxi_core::RelPath;

pub use detect::{Detection, Signals, detect};
pub use secret::is_secret_file;
use vuoxi_file::Loaded;

pub const VUOXI_FILE: &str = ".vuoxi";

/// Signature line every valid `CACHEDIR.TAG` starts with.
pub const CACHEDIR_SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tag {
    Code,
    Secrets,
    Photos,
    Media,
    Notes,
    Archive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitignorePolicy {
    Keep,
    Exclude,
    Ask,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Urgent,
    Normal,
    Bulk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub tag: Tag,
    pub gitignore: GitignorePolicy,
    pub priority: Priority,
    /// May be stored on relays (for `archive`: the archive tier).
    pub relays: bool,
    /// May be read by agents holding a read capability.
    pub agents_read: bool,
    /// Hash whole files instead of content-defined chunks.
    pub whole_file_hash: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    Keep(String),
    CacheDirTag,
    Rebuildable(&'static str),
    Ignore(String),
    GitDir,
    Gitignored(String),
    Default,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Sync(Reason),
    Skip(Reason),
}

impl Decision {
    pub fn is_sync(&self) -> bool {
        matches!(self, Decision::Sync(_))
    }
    pub fn reason(&self) -> &Reason {
        match self {
            Decision::Sync(r) | Decision::Skip(r) => r,
        }
    }
}

/// The tag table from the design doc.
pub fn tag_defaults(tag: Tag) -> Policy {
    use GitignorePolicy::Keep;
    use Priority::*;
    let (priority, relays, agents_read, whole_file_hash) = match tag {
        Tag::Code => (Normal, true, true, false),
        Tag::Secrets => (Urgent, false, false, false),
        Tag::Photos | Tag::Media => (Bulk, true, false, true),
        Tag::Notes => (Urgent, true, true, false),
        Tag::Archive => (Bulk, true, false, false),
    };
    Policy {
        tag,
        gitignore: Keep,
        priority,
        relays,
        agents_read,
        whole_file_hash,
    }
}

/// A directory that is rebuildable by a tool, recognised by name and, when
/// `beside` is non-empty, by one of those manifests in the same parent.
#[derive(Debug)]
pub struct RebuildableRule {
    pub dir: &'static str,
    pub beside: &'static [&'static str],
    pub label: &'static str,
}

/// Built-in rebuildable table; extended as tools are met.
pub const REBUILDABLE: &[RebuildableRule] = &[
    RebuildableRule {
        dir: "target",
        beside: &["Cargo.toml"],
        label: "target/ beside Cargo.toml",
    },
    RebuildableRule {
        dir: "node_modules",
        beside: &["package.json"],
        label: "node_modules/ beside package.json",
    },
    RebuildableRule {
        dir: "__pycache__",
        beside: &[],
        label: "__pycache__",
    },
    RebuildableRule {
        dir: ".zig-cache",
        beside: &["build.zig"],
        label: ".zig-cache beside build.zig",
    },
    RebuildableRule {
        dir: "zig-cache",
        beside: &["build.zig"],
        label: "zig-cache beside build.zig",
    },
    RebuildableRule {
        dir: "zig-out",
        beside: &["build.zig"],
        label: "zig-out beside build.zig",
    },
    RebuildableRule {
        dir: ".next",
        beside: &["package.json"],
        label: ".next beside package.json",
    },
    RebuildableRule {
        dir: "dist",
        beside: &["package.json"],
        label: "dist/ beside package.json",
    },
    RebuildableRule {
        dir: "build",
        beside: &["package.json"],
        label: "build/ beside package.json",
    },
    RebuildableRule {
        dir: ".gradle",
        beside: &[],
        label: ".gradle",
    },
    RebuildableRule {
        dir: ".direnv",
        beside: &[],
        label: ".direnv",
    },
];

/// A directory containing this file is a Python virtualenv.
pub const VENV_MARKER: &str = "pyvenv.cfg";
const VENV_LABEL: &str = "virtualenv (contains pyvenv.cfg)";

/// A `.vuoxi` problem, kept so `vuoxi status` can show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleError {
    /// Directory holding the `.vuoxi`; `None` is the root.
    pub dir: Option<RelPath>,
    pub message: String,
}

/// One step of the tag chain printed by `vuoxi why`.
#[derive(Clone, Debug)]
pub struct ChainLink {
    pub dir: Option<RelPath>,
    /// False for the root when it has no `.vuoxi` (the tag is the folder's).
    pub from_vuoxi: bool,
    /// Effective policy at this directory, after its own `.vuoxi`.
    pub policy: Policy,
    /// Safety settings this `.vuoxi` explicitly loosened.
    pub unsealed: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Why {
    pub decision: Decision,
    pub is_dir: bool,
    pub secret_file: bool,
    /// Policy the path itself falls under (tightened for secret files).
    pub policy: Policy,
    /// Root first; only the root and directories with a `.vuoxi` appear.
    pub chain: Vec<ChainLink>,
}

struct DirInfo {
    abs: PathBuf,
    vuoxi: Option<Loaded>,
    gitignore: OnceLock<Option<Gitignore>>,
}

impl DirInfo {
    fn gitignore(&self) -> Option<&Gitignore> {
        self.gitignore
            .get_or_init(|| {
                let f = self.abs.join(".gitignore");
                if !f.is_file() {
                    return None;
                }
                let mut b = GitignoreBuilder::new(&self.abs);
                let _ = b.add(&f);
                b.build().ok()
            })
            .as_ref()
    }
}

pub struct Rules {
    root: PathBuf,
    root_tag: Tag,
    dirs: HashMap<String, Arc<DirInfo>>,
    policies: HashMap<String, Policy>,
    errors: HashMap<String, String>,
}

/// Ancestor directories of `rel`, nearest first, with the byte offset at
/// which the path relative to that ancestor begins.
fn ancestors(rel: &str) -> Vec<(&str, usize)> {
    let mut out = Vec::new();
    let mut end = rel.len();
    while let Some(i) = rel[..end].rfind('/') {
        out.push((&rel[..i], i + 1));
        end = i;
    }
    out.push(("", 0));
    out
}

fn key(rel: Option<&RelPath>) -> &str {
    rel.map_or("", RelPath::as_str)
}

impl Rules {
    pub fn new(root: &Path, root_tag: Tag) -> Rules {
        Rules {
            root: root.to_path_buf(),
            root_tag,
            dirs: HashMap::new(),
            policies: HashMap::new(),
            errors: HashMap::new(),
        }
    }

    pub fn errors(&self) -> Vec<RuleError> {
        let mut v: Vec<_> = self
            .errors
            .iter()
            .map(|(k, m)| RuleError {
                dir: RelPath::new(k).ok(),
                message: m.clone(),
            })
            .collect();
        v.sort_by(|a, b| a.dir.cmp(&b.dir));
        v
    }

    pub fn invalidate(&mut self, rel_dir: Option<&RelPath>) {
        let k = key(rel_dir);
        self.dirs.remove(k);
        self.errors.remove(k);
        // Policies below this directory derive from it.
        self.policies.clear();
    }

    fn info(&mut self, k: &str) -> Arc<DirInfo> {
        if let Some(i) = self.dirs.get(k) {
            return i.clone();
        }
        let abs = self.root.join(k);
        let (vuoxi, errs) = Loaded::load(&abs);
        if !errs.is_empty() {
            self.errors.insert(k.to_owned(), errs.join("; "));
        }
        let i = Arc::new(DirInfo {
            abs,
            vuoxi,
            gitignore: OnceLock::new(),
        });
        self.dirs.insert(k.to_owned(), i.clone());
        i
    }

    pub fn policy(&mut self, rel_dir: Option<&RelPath>) -> Policy {
        let k = key(rel_dir);
        if let Some(p) = self.policies.get(k) {
            return *p;
        }
        let base = match rel_dir {
            None => tag_defaults(self.root_tag),
            Some(r) => {
                let parent = r.parent();
                self.policy(parent.as_ref())
            }
        };
        let info = self.info(k);
        let p = match &info.vuoxi {
            Some(v) => v.file.apply(base),
            None => base,
        };
        self.policies.insert(k.to_owned(), p);
        p
    }

    /// Policy for a file: its directory's, except secret-looking files are
    /// `secrets` wherever they sit.
    pub fn file_policy(&mut self, rel: &RelPath) -> Policy {
        let p = self.policy(rel.parent().as_ref());
        if is_secret_file(rel) {
            secret_tighten(p)
        } else {
            p
        }
    }

    pub fn is_secret_file(rel: &RelPath) -> bool {
        is_secret_file(rel)
    }

    pub fn dir(&mut self, rel: &RelPath) -> Decision {
        self.decide(rel, true)
    }

    pub fn file(&mut self, rel: &RelPath) -> Decision {
        self.decide(rel, false)
    }

    fn decide(&mut self, rel: &RelPath, is_dir: bool) -> Decision {
        if let Some(p) = self.keep_match(rel, is_dir) {
            return Decision::Sync(Reason::Keep(p));
        }
        let parent = rel.parent();
        let pol = self.policy(parent.as_ref());
        let has_skip_rules = !matches!(pol.tag, Tag::Secrets | Tag::Archive);
        if is_dir && has_skip_rules {
            let abs = self.root.join(rel.as_str());
            if has_cachedir_tag(&abs) {
                return Decision::Skip(Reason::CacheDirTag);
            }
            if let Some(label) = rebuildable(&abs, rel.file_name()) {
                return Decision::Skip(Reason::Rebuildable(label));
            }
        }
        if let Some(p) = self.ignore_match(rel, is_dir) {
            return Decision::Skip(Reason::Ignore(p));
        }
        if let Some(why) = tag_skip(pol.tag, rel.file_name(), is_dir) {
            return Decision::Skip(Reason::Ignore(format!("tag:{why}")));
        }
        if rel.components().any(|c| c == ".git") {
            return Decision::Skip(Reason::GitDir);
        }
        if pol.gitignore != GitignorePolicy::Keep
            && let Some(p) = self.gitignore_match(rel, is_dir)
        {
            match pol.gitignore {
                GitignorePolicy::Exclude if is_dir || !is_secret_file(rel) => {
                    return Decision::Skip(Reason::Gitignored(p));
                }
                GitignorePolicy::Ask => return Decision::Sync(Reason::Gitignored(p)),
                _ => {}
            }
        }
        Decision::Sync(Reason::Default)
    }

    fn keep_match(&mut self, rel: &RelPath, is_dir: bool) -> Option<String> {
        for (dir, off) in ancestors(rel.as_str()) {
            let info = self.info(dir);
            if let Some(v) = &info.vuoxi
                && let Match::Ignore(g) = v
                    .keep
                    .matched_path_or_any_parents(&rel.as_str()[off..], is_dir)
            {
                return Some(g.original().to_owned());
            }
        }
        None
    }

    /// Nearest `.vuoxi` with an opinion wins; a `!pattern` re-includes.
    fn ignore_match(&mut self, rel: &RelPath, is_dir: bool) -> Option<String> {
        for (dir, off) in ancestors(rel.as_str()) {
            let info = self.info(dir);
            if let Some(v) = &info.vuoxi {
                match v
                    .ignore
                    .matched_path_or_any_parents(&rel.as_str()[off..], is_dir)
                {
                    Match::Ignore(g) => return Some(g.original().to_owned()),
                    Match::Whitelist(_) => return None,
                    Match::None => {}
                }
            }
        }
        None
    }

    fn gitignore_match(&mut self, rel: &RelPath, is_dir: bool) -> Option<String> {
        for (dir, off) in ancestors(rel.as_str()) {
            let info = self.info(dir);
            if let Some(g) = info.gitignore() {
                match g.matched_path_or_any_parents(&rel.as_str()[off..], is_dir) {
                    Match::Ignore(g) => return Some(g.original().to_owned()),
                    Match::Whitelist(_) => return None,
                    Match::None => {}
                }
            }
        }
        None
    }

    pub fn why(&mut self, rel: &RelPath, is_dir: bool) -> Why {
        let decision = self.decide(rel, is_dir);
        let secret_file = !is_dir && is_secret_file(rel);
        // Directories whose `.vuoxi` governs this path's contents or settings.
        let mut governing: Vec<Option<RelPath>> = vec![None];
        let comps: Vec<&str> = rel.components().collect();
        let n = if is_dir { comps.len() } else { comps.len() - 1 };
        for i in 1..=n {
            governing.push(RelPath::new(&comps[..i].join("/")).ok());
        }
        let mut chain = Vec::new();
        for dir in governing {
            let info = self.info(key(dir.as_ref()));
            let from_vuoxi = info.vuoxi.is_some();
            if dir.is_some() && !from_vuoxi {
                continue;
            }
            let policy = self.policy(dir.as_ref());
            let unsealed = info
                .vuoxi
                .as_ref()
                .map(|v| v.file.unseal.clone())
                .unwrap_or_default();
            chain.push(ChainLink {
                dir,
                from_vuoxi,
                policy,
                unsealed,
            });
        }
        let base = chain.last().map(|c| c.policy).expect("root link");
        let policy = if secret_file {
            secret_tighten(base)
        } else {
            base
        };
        Why {
            decision,
            is_dir,
            secret_file,
            policy,
            chain,
        }
    }
}

fn secret_tighten(mut p: Policy) -> Policy {
    let s = tag_defaults(Tag::Secrets);
    p.tag = Tag::Secrets;
    p.relays = false;
    p.agents_read = false;
    p.priority = s.priority;
    p.whole_file_hash = s.whole_file_hash;
    p
}

fn has_cachedir_tag(dir: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(dir.join("CACHEDIR.TAG")) else {
        return false;
    };
    let mut buf = [0u8; CACHEDIR_SIGNATURE.len()];
    f.read_exact(&mut buf).is_ok() && buf == *CACHEDIR_SIGNATURE.as_bytes()
}

fn rebuildable(abs_dir: &Path, name: &str) -> Option<&'static str> {
    if abs_dir.join(VENV_MARKER).is_file() {
        return Some(VENV_LABEL);
    }
    let rule = REBUILDABLE.iter().find(|r| r.dir == name)?;
    if rule.beside.is_empty() {
        return Some(rule.label);
    }
    let parent = abs_dir.parent()?;
    rule.beside
        .iter()
        .any(|m| parent.join(m).is_file())
        .then_some(rule.label)
}

fn tag_skip(tag: Tag, name: &str, is_dir: bool) -> Option<&'static str> {
    match tag {
        Tag::Notes if !is_dir => (name.ends_with(".swp")
            || name.ends_with(".swo")
            || name.ends_with('~')
            || name.starts_with(".#"))
        .then_some("notes editor swap file"),
        Tag::Photos | Tag::Media => {
            if is_dir {
                matches!(name, ".thumbnails" | ".thumbs").then_some("thumbnail cache")
            } else {
                matches!(name, "Thumbs.db" | ".DS_Store").then_some("thumbnail cache")
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
