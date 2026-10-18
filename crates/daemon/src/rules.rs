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

    fn touched(&mut self, rel: &RelPath) {
        if rel.file_name() == VUOXI_FILE {
            self.0.invalidate(rel.parent().as_ref());
        }
    }
}
