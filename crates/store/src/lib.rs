//! The change journal and the manifest index, as a [`vuoxi_core::Store`].
//!
//! The journal is append-only and is the reason vuoxi never rescans: every
//! change the watcher saw is here, so a restart replays the journal instead of
//! walking the tree. The manifest index is on NOMT, whose Merkle root doubles as
//! the folder state peers compare. The journal is a small file journal for now
//! (see [`journal`] for why not commonware's).

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
use vuoxi_core::{Error, Manifest, PathId, Service, StoreOp};

/// Journal plus index in one directory.
pub struct LocalStore {
    journal: Journal,
    index: Index,
}

impl LocalStore {
    pub fn open(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            journal: Journal::open(&dir.join("journal"))?,
            index: Index::open(&dir.join("index"))?,
        })
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

// core: needs `StoreOp::Append` to answer with the sequence number (the
// response type is `Option<Manifest>`, so it is dropped here; use
// `LocalStore::append`), and `Since(cursor, limit)` / `Root` ops for peers.
impl Service<StoreOp> for LocalStore {
    type Response = Option<Manifest>;
    type Error = Error;
    type Future = Ready<Result<Self::Response, Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, op: StoreOp) -> Self::Future {
        ready(
            match op {
                StoreOp::Append(p) => self.append(p).map(|_| None),
                StoreOp::Get(p) => self.index.get(p),
                StoreOp::Put(m) => self.index.put(m).map(|_| None),
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
    use vuoxi_core::{ChunkId, VersionVector};

    #[test]
    fn service_roundtrip() {
        let mut s = LocalStore::open(&testdir("service")).unwrap();
        let m = Manifest {
            path: PathId(5),
            version: VersionVector::new(),
            chunks: vec![ChunkId([4; 32])],
        };
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(s.poll_ready(&mut cx).is_ready());
        let mut f = std::pin::pin!(s.call(StoreOp::Put(m.clone())));
        assert!(matches!(f.as_mut().poll(&mut cx), Poll::Ready(Ok(None))));
        let mut f = std::pin::pin!(s.call(StoreOp::Get(PathId(5))));
        assert!(matches!(f.as_mut().poll(&mut cx), Poll::Ready(Ok(Some(x))) if x == m));
        let mut f = std::pin::pin!(s.call(StoreOp::Append(PathId(5))));
        assert!(matches!(f.as_mut().poll(&mut cx), Poll::Ready(Ok(None))));
        assert_eq!(s.head(), 1);
        assert_eq!(s.since(0, 10).unwrap(), vec![(1, PathId(5))]);
    }
}
