//! Stable binary encoding of a [`Manifest`], the value stored in the index.
//!
//! ```text
//! path: u64 LE
//! n_peers: u32 LE, then n_peers × (peer: [u8;32], count: u64 LE), peers ascending
//! n_chunks: u32 LE, then n_chunks × [u8;32]
//! ```
//!
//! Equal manifests encode to equal bytes, which is what makes the index root a
//! function of contents alone.

use std::io;
use vuoxi_core::{ChunkId, Manifest, PathId, PeerId, VersionVector};

fn bad(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn entries(v: &VersionVector) -> Vec<(PeerId, u64)> {
    v.iter().collect()
}

fn rebuild(es: &[(PeerId, u64)]) -> io::Result<VersionVector> {
    Ok(es.iter().copied().collect())
}

pub fn encode(m: &Manifest) -> io::Result<Vec<u8>> {
    let es = entries(&m.version);
    let mut out = Vec::with_capacity(16 + es.len() * 40 + m.chunks.len() * 32);
    out.extend_from_slice(&m.path.0.to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(es.len())
            .map_err(|_| bad("too many peers"))?
            .to_le_bytes(),
    );
    for (p, n) in &es {
        out.extend_from_slice(&p.0);
        out.extend_from_slice(&n.to_le_bytes());
    }
    out.extend_from_slice(
        &u32::try_from(m.chunks.len())
            .map_err(|_| bad("too many chunks"))?
            .to_le_bytes(),
    );
    for c in &m.chunks {
        out.extend_from_slice(&c.0);
    }
    Ok(out)
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(bad("truncated manifest"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> io::Result<usize> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()) as usize)
    }
    fn id(&mut self) -> io::Result<[u8; 32]> {
        Ok(self.take(32)?.try_into().unwrap())
    }
}

pub fn decode(b: &[u8]) -> io::Result<Manifest> {
    let mut r = Reader(b);
    let path = PathId(r.u64()?);
    let np = r.u32()?;
    // Bound allocations by the input length, never by a claimed count.
    if np > b.len() / 40 {
        return Err(bad("peer count exceeds input"));
    }
    let mut es = Vec::with_capacity(np);
    for _ in 0..np {
        es.push((PeerId(r.id()?), r.u64()?));
    }
    let nc = r.u32()?;
    if nc > b.len() / 32 {
        return Err(bad("chunk count exceeds input"));
    }
    let mut chunks = Vec::with_capacity(nc);
    for _ in 0..nc {
        chunks.push(ChunkId(r.id()?));
    }
    if !r.0.is_empty() {
        return Err(bad("trailing bytes"));
    }
    Ok(Manifest {
        path,
        version: rebuild(&es)?,
        chunks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn sample(path: u64) -> Manifest {
        let mut version = VersionVector::new();
        version.bump(PeerId([9; 32]));
        version.bump(PeerId([3; 32]));
        version.bump(PeerId([3; 32]));
        Manifest {
            path: PathId(path),
            version,
            chunks: vec![ChunkId([1; 32]), ChunkId([2; 32])],
        }
    }

    #[test]
    fn roundtrip() {
        let m = sample(7);
        assert_eq!(decode(&encode(&m).unwrap()).unwrap(), m);
        let empty = Manifest {
            path: PathId(0),
            version: VersionVector::new(),
            chunks: vec![],
        };
        assert_eq!(decode(&encode(&empty).unwrap()).unwrap(), empty);
    }

    #[test]
    fn rejects_garbage() {
        let b = encode(&sample(7)).unwrap();
        assert!(decode(&b[..b.len() - 1]).is_err());
        let mut long = b.clone();
        long.push(0);
        assert!(decode(&long).is_err());
        let mut huge = b;
        huge[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&huge).is_err());
    }
}
