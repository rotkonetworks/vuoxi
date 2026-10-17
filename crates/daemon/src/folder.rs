//! One synced folder: its catalog, its store, and the rules for turning local
//! scans and remote manifests into new versions. Everything here is
//! synchronous and short; disk and network work happens outside the lock.

use crate::fs::{self as vfs, Scan};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use vuoxi_chunk::ChunkEntry;
use vuoxi_core::{
    Causality, ChunkId, Manifest, Meta, PathId, PeerId, RelPath, VersionVector, Wire,
};
use vuoxi_store::LocalStore;

/// Which paths are not synced. The rules crate implements the full policy;
/// [`BasicFilter`] is the built-in floor.
pub trait Filter: Send {
    fn skip_dir(&mut self, root: &Path, rel: &RelPath) -> bool;
    fn skip_file(&mut self, root: &Path, rel: &RelPath) -> bool;
}

/// `.git`, cache directories that say so (`CACHEDIR.TAG`), and our own temp files.
#[derive(Default)]
pub struct BasicFilter;

impl Filter for BasicFilter {
    fn skip_dir(&mut self, root: &Path, rel: &RelPath) -> bool {
        rel.file_name() == ".git" || root.join(rel.as_str()).join("CACHEDIR.TAG").is_file()
    }
    fn skip_file(&mut self, _: &Path, rel: &RelPath) -> bool {
        vfs::is_tmp(rel)
    }
}

/// What to do with a peer's manifest.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    Nothing,
    /// The disk already matches; only the version changes.
    Adopt(Manifest),
    /// Fetch the chunks and write the file.
    Write(Manifest),
    Delete(Manifest),
    /// Edited on both sides. Every device picks the same winner (newer mtime,
    /// then larger content), keeps it at the path with a version dominating
    /// both, and writes the loser beside it under a name derived from the
    /// loser alone, so all devices create the same copy and converge.
    Conflict {
        main: Manifest,
        main_is_theirs: bool,
        copy: Manifest,
    },
    /// The disk differs from the index: scan it first, then plan again.
    LocalFirst(RelPath),
}

pub struct Folder {
    pub id: String,
    pub root: PathBuf,
    me: PeerId,
    store: LocalStore,
    catalog: HashMap<PathId, Manifest>,
    /// One file known to hold each chunk, for serving and local reuse.
    chunk_refs: HashMap<ChunkId, PathId>,
    /// Chunk offsets of files we have read or written.
    offsets: HashMap<PathId, Vec<ChunkEntry>>,
    filter: Box<dyn Filter>,
}

impl Folder {
    pub fn open(
        id: &str,
        root: &Path,
        me: PeerId,
        state: &Path,
        filter: Box<dyn Filter>,
    ) -> io::Result<Folder> {
        let store = LocalStore::open(&state.join(id))?;
        let mut f = Folder {
            id: id.to_owned(),
            root: root.to_owned(),
            me,
            store,
            catalog: HashMap::new(),
            chunk_refs: HashMap::new(),
            offsets: HashMap::new(),
            filter,
        };
        let mut seen = HashSet::new();
        for (_, p) in f.store.since(0, usize::MAX)? {
            if seen.insert(p)
                && let Some(m) = f.store.index().get(p)?
            {
                f.index_chunks(&m);
                f.catalog.insert(p, m);
            }
        }
        Ok(f)
    }

    pub fn epoch(&self) -> u64 {
        self.store.epoch()
    }

    pub fn head(&self) -> u64 {
        self.store.head()
    }

    pub fn root_hash(&self) -> [u8; 32] {
        self.store.root()
    }

    pub fn get(&self, rel: &RelPath) -> Option<&Manifest> {
        self.catalog.get(&PathId::of(&self.id, rel))
    }

    pub fn live(&self) -> impl Iterator<Item = &Manifest> {
        self.catalog.values().filter(|m| !m.meta.deleted)
    }

    /// Is this manifest's path ours? Peers cannot smuggle an id for another folder.
    pub fn owns(&self, m: &Manifest) -> bool {
        PathId::of(&self.id, &m.rel) == m.path
    }

    pub fn skip_dir(&mut self, rel: &RelPath) -> bool {
        self.filter.skip_dir(&self.root, rel)
    }

    pub fn skip_file(&mut self, rel: &RelPath) -> bool {
        self.filter.skip_file(&self.root, rel)
            || rel.parent().is_some_and(|p| self.under_skipped(&p))
    }

    fn under_skipped(&mut self, dir: &RelPath) -> bool {
        let mut d = Some(dir.clone());
        while let Some(x) = d {
            if self.filter.skip_dir(&self.root, &x) {
                return true;
            }
            d = x.parent();
        }
        false
    }

    /// The index's metadata for a live file, for the scan shortcut.
    pub fn known_meta(&self, rel: &RelPath) -> Option<Meta> {
        self.get(rel).filter(|m| !m.meta.deleted).map(|m| m.meta)
    }

    fn index_chunks(&mut self, m: &Manifest) {
        for c in &m.chunks {
            self.chunk_refs.insert(*c, m.path);
        }
    }

    fn record(&mut self, m: Manifest) -> io::Result<()> {
        self.store.index().put(m.clone())?;
        self.store.append(m.path)?;
        self.index_chunks(&m);
        self.catalog.insert(m.path, m);
        Ok(())
    }

    /// Turn a scan of `rel` into a new local version, if anything changed.
    pub fn commit_local(&mut self, rel: &RelPath, scan: Scan) -> io::Result<Option<Manifest>> {
        let path = PathId::of(&self.id, rel);
        let old = self.catalog.get(&path);
        let (meta, chunks) = match scan {
            Scan::Unchanged | Scan::Dir | Scan::Other => return Ok(None),
            Scan::Missing => match old {
                Some(o) if !o.meta.deleted => (
                    Meta {
                        deleted: true,
                        ..Meta::default()
                    },
                    Vec::new(),
                ),
                _ => return Ok(None),
            },
            Scan::File { meta, chunks } => {
                let ids: Vec<ChunkId> = chunks.iter().map(|c| c.0).collect();
                let same = old.is_some_and(|o| o.meta == meta && o.chunks == ids);
                self.offsets.insert(path, chunks);
                if same {
                    return Ok(None);
                }
                (meta, ids)
            }
        };
        let mut version = old.map(|o| o.version.clone()).unwrap_or_default();
        version.bump(self.me);
        let m = Manifest {
            path,
            rel: rel.clone(),
            version,
            meta,
            chunks,
        };
        self.record(m.clone())?;
        Ok(Some(m))
    }

    /// Live catalog entries at or under `rel` that are gone from disk.
    pub fn missing_under(&self, rel: Option<&RelPath>) -> Vec<RelPath> {
        let prefix = rel.map(|r| format!("{}/", r.as_str()));
        self.live()
            .filter(|m| match (&prefix, rel) {
                (Some(p), Some(r)) => m.rel == *r || m.rel.as_str().starts_with(p.as_str()),
                _ => true,
            })
            .filter(|m| !self.root.join(m.rel.as_str()).exists())
            .map(|m| m.rel.clone())
            .collect()
    }

    pub fn plan_remote(&self, theirs: &Manifest) -> Plan {
        let disk = std::fs::symlink_metadata(self.root.join(theirs.rel.as_str()))
            .ok()
            .filter(|md| md.is_file())
            .map(|md| vfs::meta_of(&md));
        let ours = self.catalog.get(&theirs.path);
        let ours_live = ours.filter(|o| !o.meta.deleted);
        // A local edit not yet scanned must become a version before we compare.
        if disk != ours_live.map(|o| o.meta) {
            return Plan::LocalFirst(theirs.rel.clone());
        }
        let Some(ours) = ours else {
            return if theirs.meta.deleted {
                Plan::Adopt(theirs.clone())
            } else {
                Plan::Write(theirs.clone())
            };
        };
        let same = ours.same_content(theirs) || (ours.meta.deleted && theirs.meta.deleted);
        match ours.version.compare(&theirs.version) {
            Causality::Equal | Causality::Ahead => Plan::Nothing,
            Causality::Behind if same => Plan::Adopt(theirs.clone()),
            Causality::Behind if theirs.meta.deleted => Plan::Delete(theirs.clone()),
            Causality::Behind => Plan::Write(theirs.clone()),
            Causality::Concurrent => {
                let mut merged = ours.version.clone();
                merged.merge(&theirs.version);
                if same {
                    Plan::Adopt(Manifest {
                        version: merged,
                        ..ours.clone()
                    })
                } else if ours.meta.deleted {
                    // An edit beats a delete.
                    Plan::Write(Manifest {
                        version: merged,
                        ..theirs.clone()
                    })
                } else if theirs.meta.deleted {
                    merged.bump(self.me);
                    Plan::Adopt(Manifest {
                        version: merged,
                        ..ours.clone()
                    })
                } else {
                    let key = |m: &Manifest| (m.meta.mtime_ns, m.chunks.clone(), m.meta.mode);
                    let theirs_wins = key(theirs) > key(ours);
                    let (winner, loser) = if theirs_wins {
                        (theirs, ours)
                    } else {
                        (ours, theirs)
                    };
                    if !theirs_wins {
                        merged.bump(self.me);
                    }
                    Plan::Conflict {
                        main: Manifest {
                            version: merged,
                            ..winner.clone()
                        },
                        main_is_theirs: theirs_wins,
                        copy: self.conflict_copy(loser),
                    }
                }
            }
        }
    }

    fn conflict_copy(&self, loser: &Manifest) -> Manifest {
        let tag: String = loser
            .chunks
            .first()
            .map(|c| c.0[..4].iter().map(|b| format!("{b:02x}")).collect())
            .unwrap_or_else(|| "empty".into());
        let name = format!(
            "{}.vuoxi-conflict-{}-{tag}",
            loser.rel.file_name(),
            loser.meta.mtime_ns / 1_000_000_000
        );
        let rel = match loser.rel.parent() {
            Some(p) => p.join(&name),
            None => RelPath::new(&name),
        }
        .expect("a conflict name is a valid component");
        let mut version = VersionVector::new();
        version.bump(self.me);
        Manifest {
            path: PathId::of(&self.id, &rel),
            rel,
            version,
            ..loser.clone()
        }
    }

    /// Record a manifest whose file is now on disk as described.
    pub fn commit_remote(
        &mut self,
        m: Manifest,
        offsets: Option<Vec<ChunkEntry>>,
    ) -> io::Result<()> {
        if let Some(o) = offsets {
            self.offsets.insert(m.path, o);
        }
        self.record(m)
    }

    /// Answer a peer's [`Wire::Pull`].
    pub fn pull(&self, epoch: u64, since: u64, limit: usize) -> io::Result<Wire> {
        let since = if epoch == self.epoch() { since } else { 0 };
        let entries = self.store.since(since, limit)?;
        let head = entries.last().map_or(since, |e| e.0);
        let mut seen = HashSet::new();
        let manifests = entries
            .into_iter()
            .filter(|(_, p)| seen.insert(*p))
            .filter_map(|(_, p)| self.catalog.get(&p).cloned())
            .collect();
        Ok(Wire::Batch {
            epoch: self.epoch(),
            head,
            manifests,
        })
    }

    /// Where a chunk lives on our disk, if it is still there as recorded.
    pub fn chunk_location(&mut self, id: ChunkId) -> Option<(RelPath, u64, usize)> {
        let path = *self.chunk_refs.get(&id)?;
        let m = self.catalog.get(&path)?.clone();
        if m.meta.deleted || !m.chunks.contains(&id) {
            return None;
        }
        if !self.offsets.contains_key(&path) {
            match vfs::scan(&self.root, &m.rel, None) {
                Ok(Scan::File { meta, chunks }) if meta == m.meta => {
                    self.offsets.insert(path, chunks);
                }
                _ => return None,
            }
        }
        let (_, off, len) = *self.offsets.get(&path)?.iter().find(|c| c.0 == id)?;
        Some((m.rel, off, len))
    }

    /// The bytes of a chunk from our disk, verified.
    pub fn read_chunk(&mut self, id: ChunkId) -> Option<Vec<u8>> {
        let (rel, off, len) = self.chunk_location(id)?;
        let b = vfs::read_range(&self.root, &rel, off, len).ok()?;
        vuoxi_chunk::verify(id, &b).ok()?;
        Some(b)
    }
}
