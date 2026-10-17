//! Binary encoding of [`Wire`] and length-prefixed framing.
//!
//! Layout, all integers little-endian unless noted:
//!
//! ```text
//! frame    = len:u32be  body            len = body length, <= MAX_FRAME
//! body     = tag:u8 payload
//! Offer    = 1  manifest
//! Want     = 2  chunk_id:[u8;32]
//! Chunk    = 3  chunk_id:[u8;32]  data (rest of body)
//! manifest = path:u64  n_vv:u32 (peer:[u8;32] counter:u64)*n_vv
//!            n_chunks:u32 (chunk_id:[u8;32])*n_chunks
//! ```
//!
//! Counts are validated against the bytes actually present before anything is
//! allocated, so a hostile length cannot reserve memory.

use vuoxi_core::{ChunkId, Manifest, PathId, PeerId, VersionVector, Wire};

/// Largest accepted frame body (tag included). A chunk plus its header must fit.
pub const MAX_FRAME: usize = 4 << 20;

#[derive(Debug, PartialEq, Eq)]
pub enum CodecError {
    TooLarge(usize),
    Truncated,
    Trailing,
    BadTag(u8),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(n) => write!(f, "frame of {n} bytes exceeds cap {MAX_FRAME}"),
            Self::Truncated => f.write_str("truncated message"),
            Self::Trailing => f.write_str("trailing bytes after message"),
            Self::BadTag(t) => write!(f, "unknown message tag {t}"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Encode `msg` as one frame (length prefix included).
pub fn encode_frame(msg: &Wire) -> Result<Vec<u8>, CodecError> {
    let mut body = Vec::new();
    match msg {
        Wire::Offer(m) => {
            body.push(1);
            put_manifest(&mut body, m);
        }
        Wire::Want(c) => {
            body.push(2);
            body.extend_from_slice(&c.0);
        }
        Wire::Chunk(c, data) => {
            body.push(3);
            body.extend_from_slice(&c.0);
            body.extend_from_slice(data);
        }
    }
    if body.len() > MAX_FRAME {
        return Err(CodecError::TooLarge(body.len()));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode exactly one frame; the buffer must hold nothing else.
pub fn decode_frame(buf: &[u8]) -> Result<Wire, CodecError> {
    let (len, rest) = buf.split_first_chunk::<4>().ok_or(CodecError::Truncated)?;
    let len = u32::from_be_bytes(*len) as usize;
    if len > MAX_FRAME {
        return Err(CodecError::TooLarge(len));
    }
    match rest.len().cmp(&len) {
        std::cmp::Ordering::Less => Err(CodecError::Truncated),
        std::cmp::Ordering::Greater => Err(CodecError::Trailing),
        std::cmp::Ordering::Equal => decode_body(rest),
    }
}

fn decode_body(body: &[u8]) -> Result<Wire, CodecError> {
    let mut r = Reader(body);
    let msg = match r.u8()? {
        1 => Wire::Offer(r.manifest()?),
        2 => Wire::Want(ChunkId(r.array()?)),
        3 => {
            let id = ChunkId(r.array()?);
            Wire::Chunk(id, std::mem::take(&mut r.0).to_vec())
        }
        t => return Err(CodecError::BadTag(t)),
    };
    if r.0.is_empty() {
        Ok(msg)
    } else {
        Err(CodecError::Trailing)
    }
}

fn put_manifest(out: &mut Vec<u8>, m: &Manifest) {
    out.extend_from_slice(&m.path.0.to_le_bytes());
    let entries = vv_entries(&m.version);
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (p, n) in entries {
        out.extend_from_slice(&p.0);
        out.extend_from_slice(&n.to_le_bytes());
    }
    out.extend_from_slice(&(m.chunks.len() as u32).to_le_bytes());
    for c in &m.chunks {
        out.extend_from_slice(&c.0);
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], CodecError> {
        let (head, tail) = self.0.split_at_checked(n).ok_or(CodecError::Truncated)?;
        self.0 = tail;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        Ok(self.take(N)?.try_into().expect("length checked"))
    }
    fn u32(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    /// A count of `elem`-byte items, checked against what remains.
    fn count(&mut self, elem: usize) -> Result<usize, CodecError> {
        let n = self.u32()? as usize;
        if n.checked_mul(elem).is_none_or(|b| b > self.0.len()) {
            return Err(CodecError::Truncated);
        }
        Ok(n)
    }
    fn manifest(&mut self) -> Result<Manifest, CodecError> {
        let path = PathId(self.u64()?);
        let n = self.count(40)?;
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            entries.push((PeerId(self.array()?), self.u64()?));
        }
        let n = self.count(32)?;
        let mut chunks = Vec::with_capacity(n);
        for _ in 0..n {
            chunks.push(ChunkId(self.array()?));
        }
        Ok(Manifest {
            path,
            version: vv_from_entries(&entries)?,
            chunks,
        })
    }
}

fn vv_entries(v: &VersionVector) -> Vec<(PeerId, u64)> {
    v.iter().collect()
}

fn vv_from_entries(entries: &[(PeerId, u64)]) -> Result<VersionVector, CodecError> {
    Ok(entries.iter().copied().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        let (a, b) = (PeerId([1; 32]), PeerId([200; 32]));
        let mut version = VersionVector::new();
        version.bump(a);
        version.bump(a);
        version.bump(b);
        Manifest {
            path: PathId(u64::MAX - 7),
            version,
            chunks: vec![ChunkId([9; 32]), ChunkId([0; 32])],
        }
    }

    fn roundtrip(w: Wire) {
        let f = encode_frame(&w).unwrap();
        assert_eq!(decode_frame(&f).unwrap(), w);
    }

    #[test]
    fn roundtrips() {
        roundtrip(Wire::Offer(manifest()));
        roundtrip(Wire::Offer(Manifest {
            path: PathId(0),
            version: VersionVector::new(),
            chunks: vec![],
        }));
        roundtrip(Wire::Want(ChunkId([7; 32])));
        roundtrip(Wire::Chunk(ChunkId([7; 32]), vec![]));
        roundtrip(Wire::Chunk(ChunkId([7; 32]), (0..=255).collect()));
    }

    #[test]
    fn stable_bytes() {
        let f = encode_frame(&Wire::Want(ChunkId([5; 32]))).unwrap();
        assert_eq!(&f[..5], &[0, 0, 0, 33, 2]);
        assert_eq!(f.len(), 37);
    }

    #[test]
    fn size_cap() {
        let big = Wire::Chunk(ChunkId([0; 32]), vec![0; MAX_FRAME]);
        assert_eq!(
            encode_frame(&big),
            Err(CodecError::TooLarge(MAX_FRAME + 33))
        );
        let ok = Wire::Chunk(ChunkId([0; 32]), vec![0; MAX_FRAME - 33]);
        roundtrip(ok);
        // a hostile length prefix is rejected before any allocation
        let mut f = ((MAX_FRAME + 1) as u32).to_be_bytes().to_vec();
        f.push(2);
        assert_eq!(decode_frame(&f), Err(CodecError::TooLarge(MAX_FRAME + 1)));
    }

    #[test]
    fn malformed() {
        let f = encode_frame(&Wire::Offer(manifest())).unwrap();
        for cut in 0..f.len() {
            assert!(decode_frame(&f[..cut]).is_err(), "cut {cut}");
        }
        let mut long = f.clone();
        long.push(0);
        assert_eq!(decode_frame(&long), Err(CodecError::Trailing));
        assert_eq!(decode_frame(&[0, 0, 0, 1, 9]), Err(CodecError::BadTag(9)));
        // claims a million chunks, carries none
        let mut body = vec![1u8];
        body.extend_from_slice(&0u64.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&1_000_000u32.to_le_bytes());
        let mut f = (body.len() as u32).to_be_bytes().to_vec();
        f.extend(body);
        assert_eq!(decode_frame(&f), Err(CodecError::Truncated));
    }
}
