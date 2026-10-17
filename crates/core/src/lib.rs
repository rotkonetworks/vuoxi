//! The vocabulary and the seams of vuoxi.
//!
//! Every component is a [`Service`] over a request enum; every policy is a
//! [`Layer`]. Concrete detectors, stores, transports and archives live in their
//! own crates and meet only at the traits below, so tests run the same engine
//! over in-memory fakes.

#![forbid(unsafe_code)]

mod path;
mod version;

pub use path::{MAX_REL_PATH, PathError, RelPath};
pub use tower_layer::Layer;
pub use tower_service::Service;
pub use version::{Causality, VersionVector};

/// A file as the sync engine sees it: a hash of (folder, [`RelPath`]), so every
/// device derives the same id. See [`PathId::of`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PathId(pub u64);

/// A content-defined chunk, named by its BLAKE3 digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkId(pub [u8; 32]);

/// A device, named by its public key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerId(pub [u8; 32]);

/// File metadata that travels with a manifest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Meta {
    pub size: u64,
    /// Permission bits only (`0o7777`); file type is always regular.
    pub mode: u32,
    /// Modification time, nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// A tombstone: the file was deleted at this version.
    pub deleted: bool,
}

/// What one device believes a file is: where it lives, its causal history,
/// its metadata and its chunks in order.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Manifest {
    pub path: PathId,
    pub rel: RelPath,
    pub version: VersionVector,
    pub meta: Meta,
    pub chunks: Vec<ChunkId>,
}

impl Manifest {
    /// Same content and metadata, ignoring history: the test that stops a
    /// device re-announcing a file it has just written from a peer.
    pub fn same_content(&self, other: &Manifest) -> bool {
        self.rel == other.rel && self.meta == other.meta && self.chunks == other.chunks
    }
}

/// Everything the engine can be asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// The detector saw a local change.
    Changed(PathId),
    /// A peer advertises its view of a file.
    Offer(PeerId, Manifest),
    /// Pull one chunk from a peer.
    Fetch(PeerId, ChunkId),
    /// Exchange manifests with a peer until both agree.
    Reconcile(PeerId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    Done,
    /// Accepted but parked by policy (battery, heat, budget); it will run later.
    Deferred,
    Manifest(Manifest),
    Chunk(Vec<u8>),
    /// Concurrent edits: both versions are kept, nothing is overwritten.
    Conflict {
        ours: Manifest,
        theirs: Manifest,
    },
}

#[derive(Debug)]
pub enum Error {
    /// Data from a peer did not hash to what it claimed.
    Integrity(ChunkId),
    Io(std::io::Error),
    Transport(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Integrity(c) => write!(f, "chunk {:02x?} failed verification", &c.0[..4]),
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Transport(e) => write!(f, "transport: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// The engine: anything that answers [`Request`]s. Layers wrap it.
pub trait Engine: Service<Request, Response = Response, Error = Error> {}
impl<S> Engine for S where S: Service<Request, Response = Response, Error = Error> {}

/// Wire messages a transport carries between peers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wire {
    Offer(Manifest),
    Want(ChunkId),
    Chunk(ChunkId, Vec<u8>),
    /// Ask for the sender's journal after `since`. `epoch` names the journal the
    /// cursor belongs to; a mismatch means the cursor is meaningless.
    Pull {
        epoch: u64,
        since: u64,
        limit: u32,
    },
    /// Answer to [`Wire::Pull`]: manifests changed after the cursor, in journal
    /// order, and the journal head. A different `epoch` than asked tells the
    /// puller to restart from 0. `head` is the cursor to ask from next.
    Batch {
        epoch: u64,
        head: u64,
        manifests: Vec<Manifest>,
    },
}

/// Moves [`Wire`] messages to one peer; answers with the peer's reply, if any.
pub trait Transport: Service<(PeerId, Wire), Response = Option<Wire>, Error = Error> {}
impl<T> Transport for T where T: Service<(PeerId, Wire), Response = Option<Wire>, Error = Error> {}

/// Durable local state: the change journal and the manifest index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreOp {
    Append(PathId),
    Get(PathId),
    Put(Manifest),
    Since { cursor: u64, limit: usize },
    Root,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreReply {
    /// `Append`: the new journal sequence number.
    Seq(u64),
    /// `Get`: the manifest, if any. `Put`: `None`.
    Manifest(Option<Manifest>),
    /// `Since`: `(seq, path)` in journal order.
    Entries(Vec<(u64, PathId)>),
    /// `Root`: the index's Merkle root.
    Root([u8; 32]),
}

pub trait Store: Service<StoreOp, Response = StoreReply, Error = Error> {}
impl<T> Store for T where T: Service<StoreOp, Response = StoreReply, Error = Error> {}
