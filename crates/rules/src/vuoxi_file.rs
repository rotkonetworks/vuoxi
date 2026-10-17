use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::Deserialize;

use crate::{GitignorePolicy, Policy, Priority, Tag, tag_defaults};

/// Contents of a `.vuoxi` file. Unknown keys are tolerated so that older
/// peers can read files written by newer ones.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct VuoxiFile {
    pub tag: Option<Tag>,
    pub ignore: Vec<String>,
    pub keep: Vec<String>,
    pub priority: Option<Priority>,
    pub gitignore: Option<GitignorePolicy>,
    pub unseal: Vec<String>,
}

impl VuoxiFile {
    fn unseals(&self, what: &str) -> bool {
        self.unseal.iter().any(|u| match what {
            "agents_read" => u == "agents_read" || u == "agents",
            w => u == w,
        })
    }

    /// Nearest wins for ordinary settings; safety settings only tighten
    /// unless explicitly unsealed.
    pub fn apply(&self, cur: Policy) -> Policy {
        let mut p = cur;
        let unseal_relays = self.unseals("relays");
        let unseal_agents = self.unseals("agents_read");
        if let Some(t) = self.tag {
            let d = tag_defaults(t);
            p.tag = t;
            p.priority = d.priority;
            p.gitignore = d.gitignore;
            p.whole_file_hash = d.whole_file_hash;
            p.relays = if unseal_relays {
                d.relays
            } else {
                cur.relays && d.relays
            };
            p.agents_read = if unseal_agents {
                d.agents_read
            } else {
                cur.agents_read && d.agents_read
            };
        } else {
            p.relays |= unseal_relays;
            p.agents_read |= unseal_agents;
        }
        if let Some(x) = self.priority {
            p.priority = x;
        }
        if let Some(x) = self.gitignore {
            p.gitignore = x;
        }
        p
    }
}

pub(crate) struct Loaded {
    pub file: VuoxiFile,
    pub ignore: Gitignore,
    pub keep: Gitignore,
}

impl Loaded {
    /// `(None, errors)` when absent or malformed; absent is not an error.
    pub fn load(dir: &Path) -> (Option<Loaded>, Vec<String>) {
        let path = dir.join(crate::VUOXI_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (None, vec![]),
            Err(e) => return (None, vec![format!("read {}: {e}", crate::VUOXI_FILE)]),
        };
        let file: VuoxiFile = match toml::from_str(&text) {
            Ok(f) => f,
            Err(e) => return (None, vec![format!("{}: {e}", crate::VUOXI_FILE)]),
        };
        let mut errs = Vec::new();
        let ignore = build(dir, &file.ignore, "ignore", &mut errs);
        let keep = build(dir, &file.keep, "keep", &mut errs);
        (Some(Loaded { file, ignore, keep }), errs)
    }
}

fn build(dir: &Path, patterns: &[String], what: &str, errs: &mut Vec<String>) -> Gitignore {
    let mut b = GitignoreBuilder::new(dir);
    for p in patterns {
        if let Err(e) = b.add_line(None, p) {
            errs.push(format!("{what} pattern {p:?}: {e}"));
        }
    }
    b.build().unwrap_or_else(|_| Gitignore::empty())
}
