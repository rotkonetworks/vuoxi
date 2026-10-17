//! Rule-based tag suggestion. Offline, no model: decisive signals only, and
//! `tag: None` when they are weak or absent so the caller asks instead.

use std::collections::VecDeque;
use std::path::Path;

use vuoxi_core::RelPath;

use crate::secret::{is_env_file, is_secret_file};
use crate::{REBUILDABLE, Tag, has_cachedir_tag, rebuildable};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Signals {
    /// Entries (files and directories) looked at.
    pub sampled: usize,
    pub files: usize,
    pub git_dirs: usize,
    pub cargo_manifests: usize,
    pub other_manifests: usize,
    /// Key material: KeePass, `.age`/`.gpg`, private keys, wallets.
    pub secret_files: usize,
    /// `.env*` files: surfaced, but not enough to call a folder secrets.
    pub env_files: usize,
    pub dcim_dirs: usize,
    pub images: usize,
    pub videos: usize,
    pub text_files: usize,
    pub cachedir_tags: usize,
    /// Directories recognised as rebuildable (not descended into).
    pub rebuildable_dirs: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Detection {
    pub tag: Option<Tag>,
    /// 0.0 to 1.0; below about 0.6 the caller should ask.
    pub confidence: f32,
    pub signals: Signals,
}

const OTHER_MANIFESTS: &[&str] = &[
    "package.json",
    "pyproject.toml",
    "go.mod",
    "build.zig",
    "pom.xml",
    "build.gradle",
    "CMakeLists.txt",
    "Makefile",
    "flake.nix",
];
const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "heic", "heif", "png", "cr2", "cr3", "nef", "arw", "dng", "raf", "orf", "rw2",
];
const VIDEO_EXTS: &[&str] = &["mp4", "mkv", "mov", "avi", "webm", "m4v", "mts", "wmv"];
const TEXT_EXTS: &[&str] = &["md", "markdown", "txt", "org", "adoc", "rst"];

/// Sample up to `sample_limit` entries breadth-first and suggest a tag.
pub fn detect(root: &Path, sample_limit: usize) -> Detection {
    let mut s = Signals::default();
    let mut queue: VecDeque<std::path::PathBuf> = VecDeque::from([root.to_path_buf()]);
    'walk: while let Some(dir) = queue.pop_front() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            if s.sampled >= sample_limit {
                break 'walk;
            }
            s.sampled += 1;
            let Ok(ft) = entry.file_type() else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if ft.is_dir() {
                if name == ".git" {
                    s.git_dirs += 1;
                } else if has_cachedir_tag(&entry.path()) {
                    s.cachedir_tags += 1;
                    s.rebuildable_dirs += 1;
                } else if rebuildable(&entry.path(), &name).is_some()
                    || REBUILDABLE
                        .iter()
                        .any(|r| r.dir == name && r.beside.is_empty())
                {
                    s.rebuildable_dirs += 1;
                } else {
                    if name == "DCIM" {
                        s.dcim_dirs += 1;
                    }
                    queue.push_back(entry.path());
                }
            } else if ft.is_file() {
                s.files += 1;
                classify_file(&name, &mut s);
            }
        }
    }
    verdict(s)
}

fn classify_file(name: &str, s: &mut Signals) {
    if name == "Cargo.toml" {
        s.cargo_manifests += 1;
    } else if OTHER_MANIFESTS.contains(&name) {
        s.other_manifests += 1;
    }
    if let Ok(rel) = RelPath::new(name) {
        if is_env_file(&rel) {
            s.env_files += 1;
            return;
        }
        if is_secret_file(&rel) {
            s.secret_files += 1;
            return;
        }
    }
    let Some((_, ext)) = name.rsplit_once('.') else {
        return;
    };
    let ext = ext.to_ascii_lowercase();
    let ext = ext.as_str();
    if IMAGE_EXTS.contains(&ext) {
        s.images += 1;
    } else if VIDEO_EXTS.contains(&ext) {
        s.videos += 1;
    } else if TEXT_EXTS.contains(&ext) {
        s.text_files += 1;
    }
}

fn verdict(s: Signals) -> Detection {
    let files = s.files.max(1) as f32;
    let frac = |n: usize| n as f32 / files;
    let manifests = s.cargo_manifests + s.other_manifests;
    let (tag, confidence) = if s.secret_files > 0 && frac(s.secret_files) >= 0.5 {
        (
            Some(Tag::Secrets),
            if frac(s.secret_files) >= 0.8 {
                0.9
            } else {
                0.7
            },
        )
    } else if s.git_dirs > 0 || s.cargo_manifests > 0 {
        (
            Some(Tag::Code),
            if s.git_dirs > 0 && manifests > 0 {
                0.95
            } else {
                0.85
            },
        )
    } else if manifests > 0 || s.cachedir_tags > 0 {
        (Some(Tag::Code), 0.7)
    } else if s.dcim_dirs > 0 || (s.images >= 5 && frac(s.images) >= 0.6) {
        (Some(Tag::Photos), if s.dcim_dirs > 0 { 0.9 } else { 0.7 })
    } else if s.videos >= 3 && frac(s.videos) >= 0.5 {
        (Some(Tag::Media), 0.7)
    } else if s.text_files >= 3 && frac(s.text_files) >= 0.6 {
        (Some(Tag::Notes), 0.6)
    } else {
        (None, 0.0)
    };
    Detection {
        tag,
        confidence,
        signals: s,
    }
}
