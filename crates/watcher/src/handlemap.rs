//! Turn watcher [`Record`]s into folder-relative paths without privilege.
//!
//! Opening a handle (`open_by_handle_at`) needs `CAP_DAC_READ_SEARCH`, which
//! the daemon does not have. But `name_to_handle_at(2)` does not, so the
//! daemon walks its folder once and remembers each directory's handle. A
//! record's `parent` handle then maps to a directory path, and the entry is
//! `map[parent]/name`. Directories are kept up to date from the events
//! themselves: creates add, deletes drop a subtree, moves rewrite one.
//!
//! Paths are `/`-separated strings relative to the root (`""` is the root).
//! Entry names that are not valid UTF-8 are treated as [`Resolved::Outside`]:
//! they cannot be expressed as a relative path string, so they are not synced.
//!
//! Directories rejected by the `skip` callback (build output and the like) are
//! recorded as *skipped*: not descended into, and every event under them
//! resolves to [`Resolved::Outside`] with one hash lookup.
//!
//! # Move pairing
//!
//! fanotify reports a rename as `MovedFrom` then `MovedTo`, adjacent in the
//! queue. The `MovedFrom` is held (nothing is emitted for it) until the next
//! record: if that is a `MovedTo` of the same kind of entry (same `object`
//! handle when both carry one) the two become one [`Resolved::Moved`];
//! otherwise the held record is emitted as [`Resolved::Removed`] first (the
//! entry left the tree). A trailing held `MovedFrom` (a move out of the root
//! with no later event) is released by [`HandleMap::flush`], which the caller
//! must invoke at the end of every read batch. If a pair is ever split by a
//! batch boundary the result is `Removed` + `Changed`, which converges to the
//! same state.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::record::{Handle, Kind, Record};
use crate::sys;

/// What one record means for the folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// Created or modified.
    Changed {
        rel: String,
        is_dir: bool,
    },
    Removed {
        rel: String,
        is_dir: bool,
    },
    Moved {
        from: String,
        to: String,
        is_dir: bool,
    },
    /// Events were lost; reconcile, and call [`HandleMap::rebuild`].
    Overflow,
    /// Not under the root (or in a skipped directory, other filesystem,
    /// non-UTF-8 name): ignore.
    Outside,
}

struct Node {
    rel: String,
    skipped: bool,
}

struct Pending {
    object: Option<Handle>,
    rel: String,
    is_dir: bool,
}

impl Pending {
    fn matches(&self, rec: &Record) -> bool {
        self.is_dir == rec.is_dir
            && match (&self.object, &rec.object) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    }
}

/// Directory handles of one folder; see the module docs.
pub struct HandleMap {
    root: PathBuf,
    fsid: [u8; 8],
    by_handle: HashMap<Handle, Node>,
    /// Inverse of `by_handle`, ordered so a subtree is one range.
    by_path: BTreeMap<String, Handle>,
    pending: Option<Pending>,
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

impl HandleMap {
    /// Walk `root`, recording every directory's handle. `skip(rel_dir)` is true
    /// for directories not to descend into; they are recorded as skipped.
    /// Directories on another filesystem are not entered.
    pub fn build(root: &Path, skip: impl FnMut(&str) -> bool) -> io::Result<HandleMap> {
        let root = fs::canonicalize(root)?;
        let mut map = HandleMap {
            fsid: sys::fs_id(&root)?,
            root,
            by_handle: HashMap::new(),
            by_path: BTreeMap::new(),
            pending: None,
        };
        map.rebuild(skip)?;
        Ok(map)
    }

    /// Forget everything and walk again; call after [`Resolved::Overflow`].
    pub fn rebuild(&mut self, mut skip: impl FnMut(&str) -> bool) -> io::Result<()> {
        if !fs::metadata(&self.root)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "root is not a directory",
            ));
        }
        self.by_handle.clear();
        self.by_path.clear();
        self.pending = None;
        self.put(sys::path_handle(&self.root)?, String::new(), false);
        self.walk("", &mut skip);
        Ok(())
    }

    pub fn fsid(&self) -> [u8; 8] {
        self.fsid
    }

    /// Number of directories remembered, skipped ones included.
    pub fn len(&self) -> usize {
        self.by_handle.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_handle.is_empty()
    }

    /// The relative path of a known, non-skipped directory.
    pub fn dir_path(&self, h: &Handle) -> Option<&str> {
        self.by_handle
            .get(h)
            .filter(|n| !n.skipped)
            .map(|n| n.rel.as_str())
    }

    /// Resolve one record, updating the map. Results are appended to `out`:
    /// usually one, none for a held `MovedFrom`, two when a held one is
    /// released by an unrelated record. See the module docs on move pairing.
    pub fn resolve(
        &mut self,
        rec: &Record,
        mut skip: impl FnMut(&str) -> bool,
        out: &mut Vec<Resolved>,
    ) {
        let skip = &mut skip;
        if rec.kind == Kind::Overflow {
            self.flush(out);
            out.push(Resolved::Overflow);
            return;
        }
        if let Some(p) = self.pending.take() {
            if rec.kind == Kind::MovedTo && p.matches(rec) {
                self.finish_move(p, rec, skip, out);
                return;
            }
            self.release(p, out);
        }
        let Some(rel) = self.locate(rec) else {
            out.push(Resolved::Outside);
            return;
        };
        let is_dir = rec.is_dir;
        match rec.kind {
            Kind::Create | Kind::Modify if !is_dir => out.push(Resolved::Changed { rel, is_dir }),
            Kind::Modify => out.push(if self.is_skipped(&rel) {
                Resolved::Outside
            } else {
                Resolved::Changed { rel, is_dir }
            }),
            Kind::Create => {
                let skipped = skip(&rel);
                self.drop_subtree(&rel);
                if let Some(h) = self.handle_of(rec, &rel) {
                    self.put(h, rel.clone(), skipped);
                }
                out.push(if skipped {
                    Resolved::Outside
                } else {
                    Resolved::Changed { rel, is_dir }
                });
            }
            Kind::Delete => {
                let was_skipped = is_dir && self.is_skipped(&rel);
                if is_dir {
                    self.drop_subtree(&rel);
                }
                out.push(if was_skipped {
                    Resolved::Outside
                } else {
                    Resolved::Removed { rel, is_dir }
                });
            }
            Kind::MovedFrom => {
                self.pending = Some(Pending {
                    object: rec.object.clone(),
                    rel,
                    is_dir,
                });
            }
            Kind::MovedTo => {
                // moved in from outside the root (or an unpaired half)
                if !is_dir {
                    out.push(Resolved::Changed { rel, is_dir });
                } else if self.adopt(rec, &rel, skip) {
                    out.push(Resolved::Changed { rel, is_dir });
                } else {
                    out.push(Resolved::Outside);
                }
            }
            Kind::Overflow => unreachable!(),
        }
    }

    /// Release a held `MovedFrom` as `Removed`. Call at the end of each read
    /// batch (and when idle).
    pub fn flush(&mut self, out: &mut Vec<Resolved>) {
        if let Some(p) = self.pending.take() {
            self.release(p, out);
        }
    }

    fn release(&mut self, p: Pending, out: &mut Vec<Resolved>) {
        let was_skipped = p.is_dir && self.is_skipped(&p.rel);
        if p.is_dir {
            self.drop_subtree(&p.rel);
        }
        out.push(if was_skipped {
            Resolved::Outside
        } else {
            Resolved::Removed {
                rel: p.rel,
                is_dir: p.is_dir,
            }
        });
    }

    fn finish_move(
        &mut self,
        p: Pending,
        rec: &Record,
        skip: &mut dyn FnMut(&str) -> bool,
        out: &mut Vec<Resolved>,
    ) {
        let Some(to) = self.locate(rec) else {
            return self.release(p, out); // moved out of the root
        };
        let from = p.rel;
        if !p.is_dir {
            out.push(Resolved::Moved {
                from,
                to,
                is_dir: false,
            });
            return;
        }
        let was_skipped = self.is_skipped(&from);
        let moving = self.take_subtree(&from);
        self.drop_subtree(&to); // renamed over an empty directory
        let head = moving
            .iter()
            .find(|(rel, _, _)| *rel == from)
            .map(|(_, h, _)| h.clone())
            .or_else(|| self.handle_of(rec, &to));
        if skip(&to) {
            if let Some(h) = head {
                self.put(h, to, true);
            }
            out.push(if was_skipped {
                Resolved::Outside
            } else {
                Resolved::Removed {
                    rel: from,
                    is_dir: true,
                }
            });
        } else if was_skipped || moving.is_empty() {
            // never walked: learn its subdirectories now
            if let Some(h) = head {
                self.put(h, to.clone(), false);
                self.walk(&to, skip);
            }
            out.push(if was_skipped {
                Resolved::Changed {
                    rel: to,
                    is_dir: true,
                }
            } else {
                Resolved::Moved {
                    from,
                    to,
                    is_dir: true,
                }
            });
        } else {
            for (rel, h, skipped) in moving {
                self.put(h, format!("{to}{}", &rel[from.len()..]), skipped);
            }
            out.push(Resolved::Moved {
                from,
                to,
                is_dir: true,
            });
        }
    }

    /// Register a directory that appeared with its contents (moved in): its
    /// own handle, then its subdirectories. False if it is skipped.
    fn adopt(&mut self, rec: &Record, rel: &str, skip: &mut dyn FnMut(&str) -> bool) -> bool {
        let skipped = skip(rel);
        self.drop_subtree(rel);
        if let Some(h) = self.handle_of(rec, rel) {
            self.put(h, rel.to_owned(), skipped);
            if !skipped {
                self.walk(rel, skip);
            }
        }
        !skipped
    }

    /// Path of the entry a record names, or `None` for anything we ignore.
    fn locate(&self, rec: &Record) -> Option<String> {
        if rec.fsid != self.fsid {
            return None;
        }
        let parent = self.by_handle.get(rec.parent.as_ref()?)?;
        if parent.skipped {
            return None;
        }
        let name = std::str::from_utf8(&rec.name).ok()?;
        if name.is_empty() {
            return None;
        }
        Some(join(&parent.rel, name))
    }

    /// The directory's own handle: from the record if present, else asked of
    /// the kernel by path.
    fn handle_of(&self, rec: &Record, rel: &str) -> Option<Handle> {
        rec.object
            .clone()
            .or_else(|| sys::path_handle(&self.abs(rel)).ok())
    }

    fn abs(&self, rel: &str) -> PathBuf {
        if rel.is_empty() {
            self.root.clone()
        } else {
            self.root.join(rel)
        }
    }

    fn is_skipped(&self, rel: &str) -> bool {
        self.by_path
            .get(rel)
            .and_then(|h| self.by_handle.get(h))
            .is_some_and(|n| n.skipped)
    }

    /// Insert keeping `by_handle` and `by_path` exact inverses.
    fn put(&mut self, h: Handle, rel: String, skipped: bool) {
        let node = Node {
            rel: rel.clone(),
            skipped,
        };
        if let Some(old) = self.by_handle.insert(h.clone(), node)
            && old.rel != rel
            && self.by_path.get(&old.rel) == Some(&h)
        {
            self.by_path.remove(&old.rel);
        }
        if let Some(other) = self.by_path.insert(rel, h.clone())
            && other != h
        {
            self.by_handle.remove(&other);
        }
    }

    /// Remove `rel` and everything below it, returning what was removed.
    fn take_subtree(&mut self, rel: &str) -> Vec<(String, Handle, bool)> {
        let mut keys: Vec<String> = Vec::new();
        if self.by_path.contains_key(rel) {
            keys.push(rel.to_owned());
        }
        // '0' is the character after '/'
        keys.extend(
            self.by_path
                .range(format!("{rel}/")..format!("{rel}0"))
                .map(|(k, _)| k.clone()),
        );
        keys.into_iter()
            .filter_map(|k| {
                let h = self.by_path.remove(&k)?;
                let node = self.by_handle.remove(&h)?;
                Some((k, h, node.skipped))
            })
            .collect()
    }

    fn drop_subtree(&mut self, rel: &str) {
        if !rel.is_empty() {
            self.take_subtree(rel);
        }
    }

    /// Record the subdirectories below `rel` (already registered). Entries that
    /// vanish or cannot be read mid-walk are skipped; the daemon reconciles.
    fn walk(&mut self, rel: &str, skip: &mut dyn FnMut(&str) -> bool) {
        let mut stack = vec![rel.to_owned()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = fs::read_dir(self.abs(&dir)) else {
                continue;
            };
            for e in rd.flatten() {
                if !e.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let Some(name) = e.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let child = join(&dir, &name);
                let path = e.path();
                let (Ok(h), Ok(fsid)) = (sys::path_handle(&path), sys::fs_id(&path)) else {
                    continue;
                };
                if fsid != self.fsid {
                    continue;
                }
                if skip(&child) {
                    self.put(h, child, true);
                } else {
                    self.put(h, child.clone(), false);
                    stack.push(child);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Tmp {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/handlemap-tests")
                .join(format!(
                    "{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(&p).unwrap();
            Tmp(fs::canonicalize(p).unwrap())
        }
        fn root(&self) -> PathBuf {
            let r = self.0.join("root");
            fs::create_dir_all(&r).unwrap();
            r
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn no_skip(_: &str) -> bool {
        false
    }
    fn skip_build(rel: &str) -> bool {
        rel.rsplit('/').next() == Some("build")
    }

    fn h(p: &Path) -> Handle {
        sys::path_handle(p).unwrap()
    }

    fn rec(kind: Kind, is_dir: bool, parent: &Path, name: &str, object: Option<&Path>) -> Record {
        Record {
            kind,
            is_dir,
            fsid: sys::fs_id(parent).unwrap(),
            object: object.map(h),
            parent: Some(h(parent)),
            name: name.as_bytes().to_vec(),
        }
    }

    fn res(m: &mut HandleMap, r: &Record, skip: impl FnMut(&str) -> bool) -> Vec<Resolved> {
        let mut out = Vec::new();
        m.resolve(r, skip, &mut out);
        out
    }

    fn changed(rel: &str, is_dir: bool) -> Resolved {
        Resolved::Changed {
            rel: rel.into(),
            is_dir,
        }
    }
    fn removed(rel: &str, is_dir: bool) -> Resolved {
        Resolved::Removed {
            rel: rel.into(),
            is_dir,
        }
    }
    fn moved(from: &str, to: &str, is_dir: bool) -> Resolved {
        Resolved::Moved {
            from: from.into(),
            to: to.into(),
            is_dir,
        }
    }

    fn tree(t: &Tmp) -> PathBuf {
        let r = t.root();
        fs::create_dir_all(r.join("a/b")).unwrap();
        fs::create_dir_all(r.join("a/c")).unwrap();
        fs::create_dir_all(r.join("d")).unwrap();
        fs::create_dir_all(r.join("a/build/deep")).unwrap();
        r
    }

    #[test]
    fn build_records_handles_that_match_name_to_handle_at() {
        let t = Tmp::new();
        let r = tree(&t);
        let m = HandleMap::build(&r, skip_build).unwrap();
        assert_eq!(m.fsid(), sys::fs_id(&r).unwrap());
        assert_eq!(m.dir_path(&h(&r)), Some(""));
        for rel in ["a", "a/b", "a/c", "d"] {
            assert_eq!(m.dir_path(&h(&r.join(rel))), Some(rel), "{rel}");
        }
        // root, a, a/b, a/c, d, a/build (skipped); a/build/deep never visited
        assert_eq!(m.len(), 6);
        assert_eq!(m.dir_path(&h(&r.join("a/build"))), None);
        assert!(!m.by_handle.contains_key(&h(&r.join("a/build/deep"))));
    }

    #[test]
    fn create_file_and_modify() {
        let t = Tmp::new();
        let r = tree(&t);
        let mut m = HandleMap::build(&r, no_skip).unwrap();
        fs::write(r.join("a/b/f.txt"), b"x").unwrap();
        let mut c = rec(Kind::Create, false, &r.join("a/b"), "f.txt", None);
        c.object = Some(h(&r.join("a/b/f.txt")));
        assert_eq!(res(&mut m, &c, no_skip), [changed("a/b/f.txt", false)]);
        c.kind = Kind::Modify;
        assert_eq!(res(&mut m, &c, no_skip), [changed("a/b/f.txt", false)]);
        let top = rec(Kind::Create, false, &r, "top", None);
        assert_eq!(res(&mut m, &top, no_skip), [changed("top", false)]);
    }

    #[test]
    fn create_dir_then_file_inside_resolves() {
        let t = Tmp::new();
        let r = t.root();
        let mut m = HandleMap::build(&r, no_skip).unwrap();
        fs::create_dir(r.join("new")).unwrap();
        fs::write(r.join("new/f"), b"").unwrap();
        // with and without the object handle (then asked of the kernel by path)
        for with_object in [true, false] {
            let mut c = rec(Kind::Create, true, &r, "new", Some(&r.join("new")));
            if !with_object {
                c.object = None;
            }
            assert_eq!(res(&mut m, &c, no_skip), [changed("new", true)]);
            let f = rec(Kind::Create, false, &r.join("new"), "f", None);
            assert_eq!(res(&mut m, &f, no_skip), [changed("new/f", false)]);
            let d = rec(Kind::Delete, true, &r, "new", None);
            assert_eq!(res(&mut m, &d, no_skip), [removed("new", true)]);
            assert_eq!(res(&mut m, &f, no_skip), [Resolved::Outside]);
        }
    }

    #[test]
    fn delete_dir_drops_subtree() {
        let t = Tmp::new();
        let r = tree(&t);
        let mut m = HandleMap::build(&r, no_skip).unwrap();
        let inside = rec(Kind::Create, false, &r.join("a/b"), "f", None);
        assert_eq!(res(&mut m, &inside, no_skip), [changed("a/b/f", false)]);
        let d = rec(Kind::Delete, true, &r, "a", None);
        assert_eq!(res(&mut m, &d, no_skip), [removed("a", true)]);
        assert_eq!(res(&mut m, &inside, no_skip), [Resolved::Outside]);
        assert_eq!(m.len(), 2); // root and d
    }

    #[test]
    fn move_dir_rewrites_subtree() {
        let t = Tmp::new();
        let r = tree(&t);
        let mut m = HandleMap::build(&r, skip_build).unwrap();
        let (hb, hc) = (h(&r.join("a/b")), h(&r.join("a/c")));
        fs::rename(r.join("a"), r.join("d/z")).unwrap();
        let from = rec(Kind::MovedFrom, true, &r, "a", Some(&r.join("d/z")));
        assert_eq!(res(&mut m, &from, skip_build), []);
        let to = rec(Kind::MovedTo, true, &r.join("d"), "z", Some(&r.join("d/z")));
        assert_eq!(res(&mut m, &to, skip_build), [moved("a", "d/z", true)]);
        assert_eq!(m.dir_path(&hb), Some("d/z/b"));
        assert_eq!(m.dir_path(&hc), Some("d/z/c"));
        let f = rec(Kind::Create, false, &r.join("d/z/b"), "f", None);
        assert_eq!(res(&mut m, &f, skip_build), [changed("d/z/b/f", false)]);
        // the skipped dir travelled with it and still silences its contents
        let junk = rec(Kind::Create, false, &r.join("d/z/build"), "o", None);
        assert_eq!(res(&mut m, &junk, skip_build), [Resolved::Outside]);
        assert!(!m.by_path.contains_key("a") && !m.by_path.contains_key("a/b"));
    }

    #[test]
    fn move_file_pairs() {
        let t = Tmp::new();
        let r = tree(&t);
        let mut m = HandleMap::build(&r, no_skip).unwrap();
        fs::write(r.join("a/f"), b"").unwrap();
        fs::rename(r.join("a/f"), r.join("d/g")).unwrap();
        let o = r.join("d/g");
        let from = rec(Kind::MovedFrom, false, &r.join("a"), "f", Some(&o));
        let to = rec(Kind::MovedTo, false, &r.join("d"), "g", Some(&o));
        assert_eq!(res(&mut m, &from, no_skip), []);
        assert_eq!(res(&mut m, &to, no_skip), [moved("a/f", "d/g", false)]);
    }

    #[test]
    fn move_out_of_root_is_removed() {
        let t = Tmp::new();
        let r = tree(&t);
        let outside = t.0.join("outside");
        fs::create_dir(&outside).unwrap();
        let mut m = HandleMap::build(&r, no_skip).unwrap();
        let hb = h(&r.join("a/b"));
        fs::rename(r.join("a"), outside.join("a")).unwrap();
        let from = rec(Kind::MovedFrom, true, &r, "a", Some(&outside.join("a")));
        let to = rec(Kind::MovedTo, true, &outside, "a", Some(&outside.join("a")));
        // paired: the destination is unknown, so the source was removed
        assert_eq!(res(&mut m, &from, no_skip), []);
        assert_eq!(res(&mut m, &to, no_skip), [removed("a", true)]);
        assert_eq!(m.dir_path(&hb), None);

        // unpaired: a following unrelated record releases it, ahead of itself
        let from = rec(Kind::MovedFrom, true, &r, "d", Some(&outside.join("a")));
        assert_eq!(res(&mut m, &from, no_skip), []);
        let next = rec(Kind::Create, false, &r, "x", None);
        assert_eq!(
            res(&mut m, &next, no_skip),
            [removed("d", true), changed("x", false)]
        );

        // trailing: released by flush
        let from = rec(Kind::MovedFrom, false, &r, "y", None);
        assert_eq!(res(&mut m, &from, no_skip), []);
        let mut out = Vec::new();
        m.flush(&mut out);
        assert_eq!(out, [removed("y", false)]);
        m.flush(&mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn move_into_root_walks_the_dir() {
        let t = Tmp::new();
        let r = t.root();
        let outside = t.0.join("outside");
        fs::create_dir_all(outside.join("x/y/z")).unwrap();
        fs::create_dir_all(outside.join("x/build")).unwrap();
        let mut m = HandleMap::build(&r, skip_build).unwrap();
        fs::rename(outside.join("x"), r.join("x")).unwrap();
        let from = rec(Kind::MovedFrom, true, &outside, "x", Some(&r.join("x")));
        assert_eq!(res(&mut m, &from, skip_build), [Resolved::Outside]);
        let to = rec(Kind::MovedTo, true, &r, "x", Some(&r.join("x")));
        assert_eq!(res(&mut m, &to, skip_build), [changed("x", true)]);
        assert_eq!(m.dir_path(&h(&r.join("x/y/z"))), Some("x/y/z"));
        assert_eq!(m.dir_path(&h(&r.join("x/build"))), None);
        assert!(m.by_handle.contains_key(&h(&r.join("x/build"))));
        // a moved-in file is just a change
        let f = rec(Kind::MovedTo, false, &r.join("x/y"), "f", None);
        assert_eq!(res(&mut m, &f, skip_build), [changed("x/y/f", false)]);
    }

    #[test]
    fn skipped_dirs_resolve_outside() {
        let t = Tmp::new();
        let r = tree(&t);
        let mut m = HandleMap::build(&r, skip_build).unwrap();
        let inside = rec(Kind::Create, false, &r.join("a/build"), "x.o", None);
        assert_eq!(res(&mut m, &inside, skip_build), [Resolved::Outside]);
        // a freshly created skipped dir is learned as skipped
        fs::create_dir(r.join("d/build")).unwrap();
        let c = rec(Kind::Create, true, &r.join("d"), "build", None);
        assert_eq!(res(&mut m, &c, skip_build), [Resolved::Outside]);
        let inside = rec(Kind::Modify, false, &r.join("d/build"), "x.o", None);
        assert_eq!(res(&mut m, &inside, skip_build), [Resolved::Outside]);
        // deleting it is not news either
        let d = rec(Kind::Delete, true, &r.join("d"), "build", None);
        assert_eq!(res(&mut m, &d, skip_build), [Resolved::Outside]);
        assert_eq!(res(&mut m, &inside, skip_build), [Resolved::Outside]);
    }

    #[test]
    fn renaming_into_and_out_of_skipped_names() {
        let t = Tmp::new();
        let r = tree(&t);
        let mut m = HandleMap::build(&r, skip_build).unwrap();
        // d -> build: leaves the synced set
        fs::rename(r.join("d"), r.join("build")).unwrap();
        let o = r.join("build");
        let from = rec(Kind::MovedFrom, true, &r, "d", Some(&o));
        let to = rec(Kind::MovedTo, true, &r, "build", Some(&o));
        assert_eq!(res(&mut m, &from, skip_build), []);
        assert_eq!(res(&mut m, &to, skip_build), [removed("d", true)]);
        assert_eq!(m.dir_path(&h(&o)), None);
        // build -> src: enters it, subdirectories learned
        fs::create_dir(r.join("build/sub")).unwrap();
        fs::rename(r.join("build"), r.join("src")).unwrap();
        let o = r.join("src");
        let from = rec(Kind::MovedFrom, true, &r, "build", Some(&o));
        let to = rec(Kind::MovedTo, true, &r, "src", Some(&o));
        assert_eq!(res(&mut m, &from, skip_build), []);
        assert_eq!(res(&mut m, &to, skip_build), [changed("src", true)]);
        assert_eq!(m.dir_path(&h(&r.join("src/sub"))), Some("src/sub"));
    }

    #[test]
    fn other_fsid_nonutf8_and_unknown_parent_are_outside() {
        let t = Tmp::new();
        let r = tree(&t);
        let mut m = HandleMap::build(&r, no_skip).unwrap();
        let mut c = rec(Kind::Create, false, &r, "f", None);
        c.fsid = [0xAA; 8];
        assert_eq!(res(&mut m, &c, no_skip), [Resolved::Outside]);

        let mut c = rec(Kind::Create, false, &r, "f", None);
        c.name = b"caf\xe9".to_vec();
        assert_eq!(res(&mut m, &c, no_skip), [Resolved::Outside]);

        let other = t.0.join("other");
        fs::create_dir(&other).unwrap();
        let c = rec(Kind::Create, false, &other, "f", None);
        assert_eq!(res(&mut m, &c, no_skip), [Resolved::Outside]);

        let mut c = rec(Kind::Create, false, &r, "f", None);
        c.parent = None;
        assert_eq!(res(&mut m, &c, no_skip), [Resolved::Outside]);
    }

    #[test]
    fn overflow_flushes_and_rebuild_relearns() {
        let t = Tmp::new();
        let r = tree(&t);
        let mut m = HandleMap::build(&r, no_skip).unwrap();
        let from = rec(Kind::MovedFrom, false, &r, "y", None);
        assert_eq!(res(&mut m, &from, no_skip), []);
        let ov = Record {
            kind: Kind::Overflow,
            is_dir: false,
            fsid: [0; 8],
            object: None,
            parent: None,
            name: vec![],
        };
        assert_eq!(
            res(&mut m, &ov, no_skip),
            [removed("y", false), Resolved::Overflow]
        );
        fs::create_dir(r.join("late")).unwrap();
        assert_eq!(m.dir_path(&h(&r.join("late"))), None);
        m.rebuild(no_skip).unwrap();
        assert_eq!(m.dir_path(&h(&r.join("late"))), Some("late"));
    }
}
