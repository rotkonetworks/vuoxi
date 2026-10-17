//! The change journal and the manifest index, as a [`vuoxi_core::Store`].
//!
//! The journal is append-only and is the reason vuoxi never rescans: every
//! change the watcher saw is here, so a restart replays the journal instead of
//! walking the tree. The manifest index is on NOMT, whose Merkle root doubles as
//! the folder state peers compare. The journal is a small file journal (see
//! [`journal`] for why not commonware's).
//!
//! The store has an **epoch**: a random id fixed when its directory is created.
//! A peer's cursor is only meaningful against the same epoch; a store that was
//! wiped and recreated gets a new one, and peers restart from 0 instead of
//! trusting a cursor past the end of a journal that no longer exists.

#![forbid(unsafe_code)]

mod codec;
pub mod index;
pub mod journal;

pub use index::Index;
pub use journal::Journal;

use std::future::{Ready, ready};
use std::io;
use std::path::Path;
use std::task::{Context, Poll};
use vuoxi_core::{Error, PathId, Service, StoreOp, StoreReply};

/// Journal plus index in one directory.
pub struct LocalStore {
    journal: Journal,
    index: Index,
    epoch: u64,
}

fn epoch(dir: &Path) -> io::Result<u64> {
    let p = dir.join("epoch");
    match std::fs::read(&p) {
        Ok(b) => b
            .try_into()
            .map(u64::from_le_bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "corrupt epoch file")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut b = [0u8; 8];
            io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut b)?;
            let tmp = dir.join("epoch.tmp");
            std::fs::write(&tmp, b)?;
            std::fs::File::open(&tmp)?.sync_all()?;
            std::fs::rename(&tmp, &p)?;
            Ok(u64::from_le_bytes(b))
        }
        Err(e) => Err(e),
    }
}

impl LocalStore {
    pub fn open(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            epoch: epoch(dir)?,
            journal: Journal::open(&dir.join("journal"))?,
            index: Index::open(&dir.join("index"))?,
        })
    }

    /// The journal's identity; see the crate docs.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Appends one change and makes it durable. Returns its sequence number.
    pub fn append(&mut self, path: PathId) -> io::Result<u64> {
        let seq = self.journal.append(path)?;
        self.journal.commit()?;
        Ok(seq)
    }

    /// Appends a burst under a single fsync; returns the last sequence number.
    pub fn append_batch(&mut self, paths: &[PathId]) -> io::Result<u64> {
        for p in paths {
            self.journal.append(*p)?;
        }
        self.journal.commit()?;
        Ok(self.journal.head())
    }

    pub fn since(&self, cursor: u64, limit: usize) -> io::Result<Vec<(u64, PathId)>> {
        self.journal.since(cursor, limit)
    }

    pub fn head(&self) -> u64 {
        self.journal.head()
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    pub fn root(&self) -> [u8; 32] {
        self.index.root()
    }
}

impl Service<StoreOp> for LocalStore {
    type Response = StoreReply;
    type Error = Error;
    type Future = Ready<Result<Self::Response, Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, op: StoreOp) -> Self::Future {
        ready(
            match op {
                StoreOp::Append(p) => self.append(p).map(StoreReply::Seq),
                StoreOp::Get(p) => self.index.get(p).map(StoreReply::Manifest),
                StoreOp::Put(m) => self.index.put(m).map(|_| StoreReply::Manifest(None)),
                StoreOp::Since { cursor, limit } => {
                    self.since(cursor, limit).map(StoreReply::Entries)
                }
                StoreOp::Root => Ok(StoreReply::Root(self.root())),
            }
            .map_err(Error::Io),
        )
    }
}

/// A fresh, empty directory under the workspace `target/`.
#[cfg(test)]
pub(crate) fn testdir(name: &str) -> std::path::PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/test-store")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use vuoxi_core::{ChunkId, Manifest, Meta, RelPath, VersionVector};

    #[test]
    fn service_roundtrip() {
        let mut s = LocalStore::open(&testdir("service")).unwrap();
        let m = Manifest {
            path: PathId(5),
            rel: RelPath::new("five").unwrap(),
            version: VersionVector::new(),
            meta: Meta::default(),
            chunks: vec![ChunkId([4; 32])],
        };
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(s.poll_ready(&mut cx).is_ready());
        let mut f = std::pin::pin!(s.call(StoreOp::Put(m.clone())));
        assert!(matches!(
            f.as_mut().poll(&mut cx),
            Poll::Ready(Ok(StoreReply::Manifest(None)))
        ));
        let mut f = std::pin::pin!(s.call(StoreOp::Get(PathId(5))));
        assert!(
            matches!(f.as_mut().poll(&mut cx), Poll::Ready(Ok(StoreReply::Manifest(Some(x)))) if x == m)
        );
        let mut f = std::pin::pin!(s.call(StoreOp::Append(PathId(5))));
        assert!(matches!(
            f.as_mut().poll(&mut cx),
            Poll::Ready(Ok(StoreReply::Seq(1)))
        ));
        let mut f = std::pin::pin!(s.call(StoreOp::Since {
            cursor: 0,
            limit: 10
        }));
        assert!(
            matches!(f.as_mut().poll(&mut cx), Poll::Ready(Ok(StoreReply::Entries(e))) if e == vec![(1, PathId(5))])
        );
    }

    #[test]
    fn epoch_is_stable_until_the_store_is_recreated() {
        let d = testdir("epoch");
        let a = LocalStore::open(&d).unwrap().epoch();
        assert_eq!(LocalStore::open(&d).unwrap().epoch(), a);
        std::fs::remove_dir_all(&d).unwrap();
        assert_ne!(LocalStore::open(&d).unwrap().epoch(), a);
    }
}
