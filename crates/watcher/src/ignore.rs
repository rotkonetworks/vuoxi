//! Drop build noise before it reaches the socket.
//!
//! The watcher sees handles and one name per event, not paths, and resolving
//! handles to paths would need `CAP_DAC_READ_SEARCH`. So directories are
//! learned as they appear: creating or moving in a directory whose name is
//! ignored marks its handle, and everything under a marked handle is dropped
//! and, if a directory, marked in turn. Directories that already existed when
//! the watcher started are not known; the daemon applies [`Ignore::is_ignored_path`]
//! to them after resolving paths. Failing open (an event slips through) is the
//! only failure mode.

use std::collections::HashSet;

use crate::record::{Handle, Kind, Record};

/// Path components that are never worth syncing.
pub const DEFAULT_NAMES: [&str; 3] = ["target", "node_modules", ".git"];

/// Cap on remembered directories; past it new ones are simply not tracked.
const MAX_TRACKED: usize = 1 << 16;

/// A set of ignored path components.
#[derive(Clone, Debug)]
pub struct Ignore {
    names: Vec<Vec<u8>>,
}

impl Default for Ignore {
    fn default() -> Self {
        Self::new(DEFAULT_NAMES.iter().map(|n| n.as_bytes()))
    }
}

impl Ignore {
    pub fn new<'a>(names: impl IntoIterator<Item = &'a [u8]>) -> Self {
        Self {
            names: names.into_iter().map(<[u8]>::to_vec).collect(),
        }
    }

    /// Is this single entry name ignored?
    pub fn is_ignored_name(&self, name: &[u8]) -> bool {
        self.names.iter().any(|n| n == name)
    }

    /// Does any `/`-separated component of `path` match?
    pub fn is_ignored_path(&self, path: &[u8]) -> bool {
        path.split(|&b| b == b'/').any(|c| self.is_ignored_name(c))
    }
}

type Key = ([u8; 8], Handle);

/// Stateful filter over the record stream; see the module docs.
#[derive(Debug, Default)]
pub struct IgnoreTracker {
    ignore: Ignore,
    dirs: HashSet<Key>,
}

impl IgnoreTracker {
    pub fn new(ignore: Ignore) -> Self {
        Self {
            ignore,
            dirs: HashSet::new(),
        }
    }

    /// Number of directories currently remembered as ignored.
    pub fn tracked(&self) -> usize {
        self.dirs.len()
    }

    /// Should `rec` be forwarded? Updates the remembered directories.
    pub fn admit(&mut self, rec: &Record) -> bool {
        let under_ignored = rec
            .parent
            .as_ref()
            .is_some_and(|p| self.dirs.contains(&(rec.fsid, p.clone())));
        let ignored = under_ignored || self.ignore.is_ignored_name(&rec.name);
        if !ignored {
            return true;
        }
        if let (true, Some(obj)) = (rec.is_dir, &rec.object) {
            let key = (rec.fsid, obj.clone());
            match rec.kind {
                Kind::Create | Kind::MovedTo if self.dirs.len() < MAX_TRACKED => {
                    self.dirs.insert(key);
                }
                Kind::Delete => {
                    self.dirs.remove(&key);
                }
                _ => {}
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u8) -> Handle {
        Handle {
            ty: 1,
            bytes: vec![n],
        }
    }

    fn rec(kind: Kind, is_dir: bool, obj: u8, parent: u8, name: &str) -> Record {
        Record {
            kind,
            is_dir,
            fsid: [0; 8],
            object: Some(h(obj)),
            parent: Some(h(parent)),
            name: name.as_bytes().to_vec(),
        }
    }

    #[test]
    fn names_and_paths() {
        let i = Ignore::default();
        assert!(i.is_ignored_name(b"target"));
        assert!(!i.is_ignored_name(b"targets"));
        assert!(i.is_ignored_path(b"/home/x/proj/node_modules/a/b.js"));
        assert!(i.is_ignored_path(b"proj/.git"));
        assert!(!i.is_ignored_path(b"/home/x/proj/src/target.rs"));
        assert!(!i.is_ignored_path(b""));
    }

    #[test]
    fn learns_ignored_dirs_and_their_descendants() {
        let mut t = IgnoreTracker::default();
        assert!(t.admit(&rec(Kind::Create, false, 2, 1, "main.rs")));
        // mkdir target: dropped, handle 10 remembered
        assert!(!t.admit(&rec(Kind::Create, true, 10, 1, "target")));
        // work inside it is dropped
        assert!(!t.admit(&rec(Kind::Create, false, 11, 10, "libfoo.rlib")));
        // nested dirs inherit the verdict
        assert!(!t.admit(&rec(Kind::Create, true, 12, 10, "debug")));
        assert!(!t.admit(&rec(Kind::Modify, false, 13, 12, "x.o")));
        // same name elsewhere in the tree is a different handle
        assert!(t.admit(&rec(Kind::Modify, false, 20, 2, "x.o")));
        assert_eq!(t.tracked(), 2);
    }

    #[test]
    fn rename_in_and_delete_forget() {
        let mut t = IgnoreTracker::default();
        assert!(!t.admit(&rec(Kind::MovedTo, true, 30, 1, ".git")));
        assert!(!t.admit(&rec(Kind::Modify, false, 31, 30, "index")));
        assert!(!t.admit(&rec(Kind::Delete, true, 30, 1, ".git")));
        assert_eq!(t.tracked(), 0);
        // handle 30 may be reused by the filesystem for something real
        assert!(t.admit(&rec(Kind::Modify, false, 32, 30, "a.txt")));
    }

    #[test]
    fn overflow_always_passes() {
        let mut t = IgnoreTracker::default();
        let r = Record {
            kind: Kind::Overflow,
            is_dir: false,
            fsid: [0; 8],
            object: None,
            parent: None,
            name: vec![],
        };
        assert!(t.admit(&r));
    }
}
