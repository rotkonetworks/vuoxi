//! What a relay is allowed to see of a folder: nothing it can read.
//!
//! Every key below is derived from the folder key, which all of one person's
//! devices hold and relays never do.
//!
//! - The folder is named on the relay by a keyed hash of its id.
//! - A chunk is stored under a keyed hash of its plaintext id, so the relay
//!   cannot match chunks to known content, and encrypted with
//!   XChaCha20-Poly1305 under a nonce derived from that id: the same chunk
//!   always seals to the same bytes, so it is uploaded once.
//! - A manifest (path, size, times, history, chunk list) is sealed under a
//!   random nonce.
//!
//! Opening checks the AEAD tag and then, for chunks, the plaintext BLAKE3, so
//! a relay that alters or swaps anything is caught.

use chacha20poly1305::aead::{Aead, KeyInit, OsRng};
use chacha20poly1305::{AeadCore, XChaCha20Poly1305, XNonce};
use vuoxi_core::{ChunkId, Error, Manifest, Wire};
use vuoxi_transport_iroh::codec::{decode_frame, encode_frame};

const CTX: &str = "vuoxi 2026-10 relay";

#[derive(Clone)]
pub struct Seal {
    folder_tag: String,
    id_key: [u8; 32],
    nonce_key: [u8; 32],
    aead: XChaCha20Poly1305,
}

fn sub(key: &[u8; 32], what: &str) -> [u8; 32] {
    blake3::derive_key(&format!("{CTX} {what}"), key)
}

impl Seal {
    pub fn new(folder: &str, key: &[u8; 32]) -> Seal {
        let tag = blake3::keyed_hash(&sub(key, "folder"), folder.as_bytes());
        Seal {
            folder_tag: tag.to_hex()[..32].to_string(),
            id_key: sub(key, "chunk id"),
            nonce_key: sub(key, "chunk nonce"),
            aead: XChaCha20Poly1305::new(&sub(key, "aead").into()),
        }
    }

    /// The folder's name on the relay.
    pub fn folder_tag(&self) -> &str {
        &self.folder_tag
    }

    /// A chunk's name on the relay.
    pub fn relay_id(&self, id: ChunkId) -> ChunkId {
        ChunkId(*blake3::keyed_hash(&self.id_key, &id.0).as_bytes())
    }

    fn chunk_nonce(&self, id: ChunkId) -> XNonce {
        let h = blake3::keyed_hash(&self.nonce_key, &id.0);
        *XNonce::from_slice(&h.as_bytes()[..24])
    }

    pub fn seal_chunk(&self, id: ChunkId, plain: &[u8]) -> Vec<u8> {
        self.aead
            .encrypt(&self.chunk_nonce(id), plain)
            .expect("encryption of an in-memory buffer")
    }

    /// Decrypt and check that the plaintext is the chunk we asked for.
    pub fn open_chunk(&self, id: ChunkId, sealed: &[u8]) -> Result<Vec<u8>, Error> {
        let plain = self
            .aead
            .decrypt(&self.chunk_nonce(id), sealed)
            .map_err(|_| Error::Integrity(id))?;
        vuoxi_chunk::verify(id, &plain)?;
        Ok(plain)
    }

    pub fn seal_manifest(&self, m: &Manifest) -> Vec<u8> {
        let plain = encode_frame(&Wire::Offer(m.clone())).expect("a manifest fits a frame");
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let mut out = nonce.to_vec();
        out.extend(
            self.aead
                .encrypt(&nonce, plain.as_slice())
                .expect("encryption of an in-memory buffer"),
        );
        out
    }

    pub fn open_manifest(&self, sealed: &[u8]) -> Result<Manifest, Error> {
        let bad = || Error::Transport("relay returned a blob that does not open".into());
        if sealed.len() < 24 {
            return Err(bad());
        }
        let (nonce, ct) = sealed.split_at(24);
        let plain = self
            .aead
            .decrypt(XNonce::from_slice(nonce), ct)
            .map_err(|_| bad())?;
        match decode_frame(&plain) {
            Ok(Wire::Offer(m)) => Ok(m),
            _ => Err(bad()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vuoxi_core::{Meta, PathId, RelPath, VersionVector};

    fn manifest() -> Manifest {
        let rel = RelPath::new("secret/plans.txt").unwrap();
        Manifest {
            path: PathId::of("f", &rel),
            rel,
            version: VersionVector::new(),
            meta: Meta::default(),
            chunks: vec![ChunkId([1; 32])],
        }
    }

    #[test]
    fn chunks_round_trip_and_hide() {
        let s = Seal::new("f", &[7; 32]);
        let plain = b"attack at dawn".to_vec();
        let id = vuoxi_chunk::hash(&plain);
        let sealed = s.seal_chunk(id, &plain);
        assert_eq!(
            sealed,
            s.seal_chunk(id, &plain),
            "deterministic: uploaded once"
        );
        assert!(!sealed.windows(6).any(|w| w == b"attack"));
        assert_ne!(
            s.relay_id(id),
            id,
            "the relay cannot look a chunk up by content"
        );
        assert_eq!(s.open_chunk(id, &sealed).unwrap(), plain);

        let mut bad = sealed.clone();
        bad[0] ^= 1;
        assert!(s.open_chunk(id, &bad).is_err(), "tampering is caught");
        let other = vuoxi_chunk::hash(b"x");
        assert!(s.open_chunk(other, &sealed).is_err(), "swapping is caught");
        assert!(Seal::new("f", &[8; 32]).open_chunk(id, &sealed).is_err());
    }

    #[test]
    fn manifests_round_trip_and_hide() {
        let s = Seal::new("f", &[7; 32]);
        let m = manifest();
        let a = s.seal_manifest(&m);
        assert_ne!(a, s.seal_manifest(&m), "random nonce");
        assert!(
            !a.windows(5).any(|w| w == b"plans"),
            "names are not visible"
        );
        assert_eq!(s.open_manifest(&a).unwrap(), m);
        assert!(Seal::new("f", &[8; 32]).open_manifest(&a).is_err());
        assert!(s.open_manifest(&a[..10]).is_err());
    }

    #[test]
    fn folder_tags_differ_by_key_and_id() {
        let t = |f: &str, k: u8| Seal::new(f, &[k; 32]).folder_tag().to_owned();
        assert_ne!(t("notes", 1), t("notes", 2));
        assert_ne!(t("notes", 1), t("docs", 1));
        assert!(t("notes", 1).bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
