//! `PathId → Manifest` on NOMT. The trie root is the folder state peers compare.
//!
//! Key = blake3 of the `PathId` bytes, so the trie is balanced and its root
//! depends only on contents, not on insertion order.

use crate::codec;
use nomt::hasher::Blake3Hasher;
use nomt::{KeyReadWrite, Nomt, Options, SessionParams, WitnessMode};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use vuoxi_core::{Manifest, PathId};

pub struct Index {
    db: Nomt<Blake3Hasher>,
}

fn io_err(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::other(e)
}

fn key(p: PathId) -> [u8; 32] {
    blake3::hash(&p.0.to_le_bytes()).into()
}

impl Index {
    /// Opens or creates the index in `dir`, sized for ~2 M entries with the
    /// fewest threads NOMT allows (see `docs/nomt-idle.md`).
    pub fn open(dir: &Path) -> io::Result<Self> {
        let mut o = Options::new();
        o.path(dir);
        // 2 M keys occupied ~4 k buckets in measurement; 16 k leaves ample room.
        o.hashtable_buckets(16_384);
        o.io_workers(1);
        o.commit_concurrency(1);
        o.preallocate_ht(false);
        o.page_cache_size(32);
        o.leaf_cache_size(32);
        Ok(Self {
            db: Nomt::open(o).map_err(io_err)?,
        })
    }

    pub fn get(&self, path: PathId) -> io::Result<Option<Manifest>> {
        self.db
            .read(key(path))
            .map_err(io_err)?
            .map(|v| codec::decode(&v))
            .transpose()
    }

    pub fn put(&self, m: Manifest) -> io::Result<()> {
        self.put_batch([m])
    }

    /// One NOMT session and one new root for the whole batch; a later manifest
    /// for the same path replaces an earlier one.
    pub fn put_batch(&self, ms: impl IntoIterator<Item = Manifest>) -> io::Result<()> {
        let mut sorted = BTreeMap::new();
        for m in ms {
            sorted.insert(key(m.path), codec::encode(&m)?);
        }
        if sorted.is_empty() {
            return Ok(());
        }
        let actuals = sorted
            .into_iter()
            .map(|(k, v)| (k, KeyReadWrite::Write(Some(v))))
            .collect();
        let session = self
            .db
            .begin_session(SessionParams::default().witness_mode(WitnessMode::disabled()));
        session
            .finish(actuals)
            .map_err(io_err)?
            .commit(&self.db)
            .map_err(io_err)
    }

    /// Merkle root of the whole index; all zeros when empty.
    pub fn root(&self) -> [u8; 32] {
        self.db.root().into_inner()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir;
    use vuoxi_core::{ChunkId, Meta, PeerId, RelPath, VersionVector};

    fn m(path: u64, tag: u8) -> Manifest {
        let mut version = VersionVector::new();
        version.bump(PeerId([tag; 32]));
        Manifest {
            path: PathId(path),
            rel: RelPath::new(&format!("f{path}")).unwrap(),
            version,
            meta: Meta::default(),
            chunks: vec![ChunkId([tag; 32])],
        }
    }

    #[test]
    fn put_get_root_changes() {
        let idx = Index::open(&testdir("index-basic")).unwrap();
        let empty = idx.root();
        assert_eq!(idx.get(PathId(1)).unwrap(), None);
        idx.put(m(1, 1)).unwrap();
        let r1 = idx.root();
        assert_ne!(r1, empty);
        assert_eq!(idx.get(PathId(1)).unwrap(), Some(m(1, 1)));
        assert_eq!(idx.get(PathId(2)).unwrap(), None);

        idx.put(m(1, 2)).unwrap();
        assert_ne!(idx.root(), r1);
        assert_eq!(idx.get(PathId(1)).unwrap(), Some(m(1, 2)));
    }

    #[test]
    fn root_ignores_insertion_order() {
        let a = Index::open(&testdir("index-order-a")).unwrap();
        let b = Index::open(&testdir("index-order-b")).unwrap();
        let c = Index::open(&testdir("index-order-c")).unwrap();
        for i in 0..50 {
            a.put(m(i, i as u8)).unwrap();
        }
        for i in (0..50).rev() {
            b.put(m(i, i as u8)).unwrap();
        }
        c.put_batch((0..50).map(|i| m(i, i as u8))).unwrap();
        assert_eq!(a.root(), b.root());
        assert_eq!(a.root(), c.root());

        b.put(m(3, 99)).unwrap();
        assert_ne!(a.root(), b.root());
    }

    #[test]
    fn survives_reopen() {
        let dir = testdir("index-reopen");
        let root = {
            let idx = Index::open(&dir).unwrap();
            idx.put_batch((0..20).map(|i| m(i, 5))).unwrap();
            idx.root()
        };
        let idx = Index::open(&dir).unwrap();
        assert_eq!(idx.root(), root);
        assert_eq!(idx.get(PathId(19)).unwrap(), Some(m(19, 5)));
    }
}
