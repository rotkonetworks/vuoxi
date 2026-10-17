//! The vocabulary and the seams of vuoxi.
//!
//! Every component is a [`Service`] over a request enum; every policy is a
//! [`Layer`]. Concrete detectors, stores, transports and archives live in their
//! own crates and meet only at the traits below, so swapping iroh for
//! commonware, or plain copies for ZODA shards, is a type parameter.

#![forbid(unsafe_code)]

mod version;

pub use tower_layer::Layer;
pub use tower_service::Service;
pub use version::{Causality, VersionVector};

/// A file as the sync engine sees it. Opaque on purpose: raw paths never cross
/// a trust boundary, so a peer cannot name `../../etc/passwd`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PathId(pub u64);

/// A content-defined chunk, named by its BLAKE3 digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkId(pub [u8; 32]);

/// A device, named by its public key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerId(pub [u8; 32]);

/// What one device believes a file is: its causal history and its chunks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub path: PathId,
    pub version: VersionVector,
    pub chunks: Vec<ChunkId>,
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
}

pub trait Store: Service<StoreOp, Response = Option<Manifest>, Error = Error> {}
impl<T> Store for T where T: Service<StoreOp, Response = Option<Manifest>, Error = Error> {}
