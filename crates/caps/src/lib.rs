//! Capabilities and the per-folder membership log.
//!
//! Two layers, deliberately separate:
//!
//! * [`token`]: attenuable biscuit capabilities. The folder's original seeder
//!   mints the root; holders (people, devices, agents) may only narrow.
//!   Anyone can verify a token offline against the seeder's public key.
//! * [`log`]: the signed, replayable membership log (who was introduced, what
//!   they were granted, which relays serve the folder). [`log::verify_log`]
//!   decides, entry by entry, whose word counts.
//!
//! Only signatures live here. Folder-key wrapping is a later step, see [`keys`].

#![forbid(unsafe_code)]

pub mod keys;
pub mod log;
pub mod token;

use std::fmt;

pub use biscuit_auth::Biscuit;
// core: PeerId is the right type for devices and for agents (an agent holds its
// own key). core: needs a `Principal` (person/agent = named set of PeerId) to
// express "devices of person X"; until then scopes are path prefixes only.
pub use vuoxi_core::PeerId;

/// A right a capability can carry. Also the verb of an authorization request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Right {
    Read,
    Write,
    /// Add members within one's own scope.
    Introduce,
    /// Hold ciphertext for the folder, within retention limits.
    Relay,
}

impl Right {
    pub const ALL: [Right; 4] = [Right::Read, Right::Write, Right::Introduce, Right::Relay];

    pub fn as_str(self) -> &'static str {
        match self {
            Right::Read => "read",
            Right::Write => "write",
            Right::Introduce => "introduce",
            Right::Relay => "relay",
        }
    }

    fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

impl fmt::Display for Right {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of [`Right`]s.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rights(u8);

impl Rights {
    pub const NONE: Rights = Rights(0);

    pub fn of(rights: &[Right]) -> Self {
        Rights(rights.iter().fold(0, |m, r| m | r.bit()))
    }

    pub fn contains(self, r: Right) -> bool {
        self.0 & r.bit() != 0
    }

    pub fn is_subset_of(self, other: Rights) -> bool {
        self.0 & !other.0 == 0
    }

    pub fn union(self, other: Rights) -> Rights {
        Rights(self.0 | other.0)
    }

    pub fn iter(self) -> impl Iterator<Item = Right> {
        Right::ALL.into_iter().filter(move |r| self.contains(*r))
    }

    pub(crate) fn bits(self) -> u8 {
        self.0
    }

    pub(crate) fn from_bits(b: u8) -> Option<Self> {
        (b < 1 << Right::ALL.len()).then_some(Rights(b))
    }
}

impl fmt::Debug for Rights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// A folder-relative path prefix: empty (the whole folder) or `a/b/` with a
/// trailing slash, so `photos/` can never match `photos-private/`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Scope(String);

impl Scope {
    /// The whole folder.
    pub fn root() -> Self {
        Scope(String::new())
    }

    pub fn new(prefix: &str) -> Result<Self, Error> {
        let clean = !prefix
            .chars()
            .any(|c| c.is_control() || c == '"' || c == '\\');
        let shaped = prefix.is_empty()
            || (prefix.ends_with('/')
                && !prefix.starts_with('/')
                && prefix[..prefix.len() - 1]
                    .split('/')
                    .all(|seg| !matches!(seg, "" | "." | "..")));
        let ok = clean && shaped;
        if ok {
            Ok(Scope(prefix.to_owned()))
        } else {
            Err(Error::BadScope(prefix.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True if everything under `other` is also under `self`.
    pub fn covers(&self, other: &Scope) -> bool {
        other.0.starts_with(&self.0)
    }
}

/// Retention a relay may be asked to provide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelayLimit {
    pub max_bytes: u64,
    pub max_days: u32,
}

#[derive(Debug)]
pub enum Error {
    BadScope(String),
    /// A `relay` grant needs retention limits.
    MissingRelayLimit,
    /// A number does not fit the token language (signed 64-bit).
    OutOfRange,
    Token(String),
    Decode(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadScope(s) => write!(f, "invalid scope {s:?}"),
            Error::MissingRelayLimit => f.write_str("relay grant without retention limits"),
            Error::OutOfRange => f.write_str("number out of range"),
            Error::Token(e) => write!(f, "token: {e}"),
            Error::Decode(e) => write!(f, "decode: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<biscuit_auth::error::Token> for Error {
    fn from(e: biscuit_auth::error::Token) -> Self {
        Error::Token(e.to_string())
    }
}

/// Why a request was refused.
#[derive(Debug)]
pub struct Denied(pub String);

impl fmt::Display for Denied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "denied: {}", self.0)
    }
}

impl std::error::Error for Denied {}

/// A device or agent key that can sign. Wraps biscuit's ed25519 pair, so one
/// key is both a token root and a log author.
pub struct Signer(biscuit_auth::KeyPair);

impl Signer {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Signer(
            biscuit_auth::KeyPair::from_bytes(
                &seed,
                biscuit_auth::builder::Algorithm::Ed25519.into(),
            )
            .expect("32 bytes is a valid ed25519 seed"),
        )
    }

    pub fn generate() -> Self {
        Signer(biscuit_auth::KeyPair::new())
    }

    pub fn peer_id(&self) -> PeerId {
        let pk = self.0.public().to_bytes();
        PeerId(pk.try_into().expect("ed25519 public keys are 32 bytes"))
    }

    pub(crate) fn pair(&self) -> &biscuit_auth::KeyPair {
        &self.0
    }
}

pub(crate) fn public_key(peer: &PeerId) -> Result<biscuit_auth::PublicKey, Error> {
    biscuit_auth::PublicKey::from_bytes(&peer.0, biscuit_auth::builder::Algorithm::Ed25519)
        .map_err(|_| Error::Decode("bad public key"))
}

pub(crate) fn hex(peer: &PeerId) -> String {
    peer.0.iter().map(|b| format!("{b:02x}")).collect()
}
