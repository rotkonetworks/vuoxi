//! The rules crate as the daemon's [`Filter`].

use crate::folder::Filter;
use crate::fs::{is_tmp, is_trash_dir};
use std::path::Path;
use vuoxi_core::RelPath;
use vuoxi_rules::{Decision, Rules, Tag, VUOXI_FILE};

pub struct RulesFilter(Rules);

impl RulesFilter {
    pub fn new(root: &Path, tag: Tag) -> RulesFilter {
        RulesFilter(Rules::new(root, tag))
    }
}

/// `code`, `secrets`, … as written in config and `.vuoxi` files.
pub fn parse_tag(s: &str) -> Option<Tag> {
    Some(match s {
        "code" => Tag::Code,
        "secrets" => Tag::Secrets,
        "photos" => Tag::Photos,
        "media" => Tag::Media,
        "notes" => Tag::Notes,
        "archive" => Tag::Archive,
        _ => return None,
    })
}

impl Filter for RulesFilter {
    fn skip_dir(&mut self, _: &Path, rel: &RelPath) -> bool {
        is_trash_dir(rel) || matches!(self.0.dir(rel), Decision::Skip(_))
    }

    fn skip_file(&mut self, _: &Path, rel: &RelPath) -> bool {
        is_tmp(rel) || matches!(self.0.file(rel), Decision::Skip(_))
    }

    fn relay_ok(&mut self, rel: &RelPath) -> bool {
        self.0.why(rel, false).policy.relays
    }

    fn touched(&mut self, rel: &RelPath) {
        if rel.file_name() == VUOXI_FILE {
            self.0.invalidate(rel.parent().as_ref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_files_stay_off_relays() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-rules-relay");
        std::fs::create_dir_all(&root).unwrap();
        let mut f = RulesFilter::new(&root, Tag::Code);
        let ok = |f: &mut RulesFilter, p: &str| f.relay_ok(&RelPath::new(p).unwrap());
        assert!(ok(&mut f, "src/main.rs"));
        assert!(!ok(&mut f, "app/.env"));
        assert!(!ok(&mut f, "keys/id_ed25519"));
        assert!(
            !RulesFilter::new(&root, Tag::Secrets).relay_ok(&RelPath::new("notes.txt").unwrap())
        );
    }
}
