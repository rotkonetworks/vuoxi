//! Content-defined chunking and hashing: file bytes in, chunk lists out.
//!
//! Boundaries come from FastCDC, so an insertion shifts bytes without moving
//! the boundaries after it and only nearby chunks change. Each chunk is named
//! by its BLAKE3 digest. Every byte is hashed exactly once.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::io::Read;

use fastcdc::v2020::{FastCDC, StreamCDC};
use vuoxi_core::{ChunkId, Error};

/// Smallest chunk (the final chunk of a file may be shorter).
pub const MIN_SIZE: usize = 16 * 1024;
/// Target average chunk size.
pub const AVG_SIZE: usize = 64 * 1024;
/// Largest chunk; also the streaming buffer bound.
pub const MAX_SIZE: usize = 256 * 1024;

/// One chunk of a file: its name, byte offset and length.
pub type ChunkEntry = (ChunkId, u64, usize);

/// Name `bytes` by its BLAKE3 digest.
pub fn hash(bytes: &[u8]) -> ChunkId {
    ChunkId(*blake3::hash(bytes).as_bytes())
}

/// Hash many chunks in one call; results are in input order.
pub fn hash_many<'a, I>(chunks: I) -> Vec<ChunkId>
where
    I: IntoIterator<Item = &'a [u8]>,
{
    let mut hasher = blake3::Hasher::new();
    chunks
        .into_iter()
        .map(|c| {
            hasher.reset();
            hasher.update(c);
            ChunkId(*hasher.finalize().as_bytes())
        })
        .collect()
}

/// Chunk an in-memory buffer.
pub fn chunk_bytes(data: &[u8]) -> Vec<ChunkEntry> {
    FastCDC::new(data, MIN_SIZE, AVG_SIZE, MAX_SIZE)
        .map(|c| {
            let end = c.offset + c.length;
            (hash(&data[c.offset..end]), c.offset as u64, c.length)
        })
        .collect()
}

/// Chunk a reader without loading it whole: memory is bounded by [`MAX_SIZE`].
pub fn chunk_reader<R: Read>(reader: R) -> Result<Vec<ChunkEntry>, Error> {
    let mut out = Vec::new();
    for c in StreamCDC::new(reader, MIN_SIZE, AVG_SIZE, MAX_SIZE) {
        let c = c.map_err(|e| match e {
            fastcdc::v2020::Error::IoError(e) => Error::Io(e),
            other => Error::Io(std::io::Error::other(other.to_string())),
        })?;
        out.push((hash(&c.data), c.offset, c.length));
    }
    Ok(out)
}

/// Chunks of `new` that `old` lacks: what must be sent. Each id appears once,
/// in first-seen order.
pub fn diff(old: &[ChunkEntry], new: &[ChunkEntry]) -> Vec<ChunkEntry> {
    let mut seen: HashSet<ChunkId> = old.iter().map(|c| c.0).collect();
    new.iter().copied().filter(|c| seen.insert(c.0)).collect()
}

/// Check that `bytes` is the chunk named `id`.
pub fn verify(id: ChunkId, bytes: &[u8]) -> Result<(), Error> {
    if hash(bytes) == id {
        Ok(())
    } else {
        Err(Error::Integrity(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// xorshift64*: deterministic filler, no rand dep.
    fn prng(len: usize, mut s: u64) -> Vec<u8> {
        let mut v = Vec::with_capacity(len + 8);
        while v.len() < len {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            v.extend_from_slice(&s.wrapping_mul(0x2545F4914F6CDD1D).to_le_bytes());
        }
        v.truncate(len);
        v
    }

    #[test]
    fn covers_input_within_bounds() {
        let data = prng(4 << 20, 1);
        let chunks = chunk_bytes(&data);
        let mut pos = 0u64;
        for (i, &(id, off, len)) in chunks.iter().enumerate() {
            assert_eq!(off, pos);
            assert!(len <= MAX_SIZE);
            if i + 1 < chunks.len() {
                assert!(len >= MIN_SIZE);
            }
            verify(id, &data[off as usize..off as usize + len]).unwrap();
            pos += len as u64;
        }
        assert_eq!(pos, data.len() as u64);
    }

    #[test]
    fn empty_and_tiny() {
        assert!(chunk_bytes(&[]).is_empty());
        assert!(chunk_reader(Cursor::new(Vec::new())).unwrap().is_empty());
        let c = chunk_bytes(b"hi");
        assert_eq!(c, vec![(hash(b"hi"), 0, 2)]);
    }

    #[test]
    fn stream_matches_slice() {
        let data = prng(3 << 20, 7);
        assert_eq!(
            chunk_reader(Cursor::new(&data)).unwrap(),
            chunk_bytes(&data)
        );
    }

    #[test]
    fn stream_survives_short_reads() {
        struct Dribble<'a>(&'a [u8]);
        impl Read for Dribble<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.0.len().min(buf.len()).min(1000);
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        let data = prng(1 << 20, 9);
        assert_eq!(chunk_reader(Dribble(&data)).unwrap(), chunk_bytes(&data));
    }

    #[test]
    fn stream_propagates_io_error() {
        struct Bad;
        impl Read for Bad {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("boom"))
            }
        }
        assert!(matches!(chunk_reader(Bad), Err(Error::Io(_))));
    }

    #[test]
    fn middle_insert_changes_few_chunks() {
        let old = prng(4 << 20, 42);
        let mut new = old.clone();
        new.insert(old.len() / 2, 0xAB);

        let (a, b) = (chunk_bytes(&old), chunk_bytes(&new));
        assert!(a.len() > 30, "expected many chunks, got {}", a.len());
        let sent = diff(&a, &b);
        assert!(!sent.is_empty());
        assert!(
            sent.len() <= 4,
            "{} of {} chunks changed",
            sent.len(),
            b.len()
        );
        // and the reverse direction too
        assert!(diff(&b, &a).len() <= 4);
    }

    #[test]
    fn diff_identical_is_empty_and_dedups() {
        let data = prng(1 << 20, 3);
        let a = chunk_bytes(&data);
        assert!(diff(&a, &a).is_empty());
        let x = (hash(b"x"), 0, 1);
        assert_eq!(diff(&[], &[x, x]), vec![x]);
    }

    #[test]
    fn verify_rejects_tampering() {
        let id = hash(b"chunk");
        assert!(verify(id, b"chunk").is_ok());
        assert!(matches!(verify(id, b"chunK"), Err(Error::Integrity(i)) if i == id));
    }

    #[test]
    fn hash_many_matches_hash() {
        let parts: [&[u8]; 3] = [b"", b"abc", &[7u8; 100_000]];
        let ids = hash_many(parts.iter().copied());
        assert_eq!(ids, parts.iter().map(|p| hash(p)).collect::<Vec<_>>());
    }
}
