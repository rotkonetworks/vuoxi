//! Per-folder content keys, wrapped per recipient. Not implemented.
//!
//! TODO: pick the AEAD and the key-agreement (X25519 from the recipient's
//! device key is the obvious candidate), derive a fresh folder key on every
//! revocation, and add a `RotateKey` membership entry that carries one
//! [`WrappedKey`] per remaining member. Until then folders are signed but not
//! encrypted, and `Revoke` does not cut off past ciphertext.

use crate::PeerId;

/// Counts key rotations; a revocation starts a new epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyEpoch(pub u64);

/// A folder key sealed for one recipient. Opaque until the wrapping scheme exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrappedKey {
    pub recipient: PeerId,
    pub epoch: KeyEpoch,
    pub sealed: Vec<u8>,
}
