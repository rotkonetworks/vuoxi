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
//! Pull     = 4  folder_len:u8 folder:utf8 epoch:u64 since:u64 limit:u32
//! Batch    = 5  epoch:u64 head:u64 n:u32 manifest*n
//! Put      = 6  folder_len:u8 folder:utf8 blob (rest of body)
//! Ack      = 7  value:u64
//! Has      = 8  folder_len:u8 folder:utf8 n:u32 chunk_id*n
//! Missing  = 9  n:u32 chunk_id*n
//! Blobs    = 10 epoch:u64 head:u64 n:u32 (len:u32 bytes)*n
//! Stash    = 11 folder_len:u8 folder:utf8 chunk_id:[u8;32] data (rest of body)
//! Fetch    = 12 folder_len:u8 folder:utf8 chunk_id:[u8;32]
//! Sweep    = 13 folder_len:u8 folder:utf8 older_than_s:u64
//! Reset    = 14 folder_len:u8 folder:utf8
//! Invite   = 15 folder_len:u8 folder:utf8 tag_len:u8 tag:utf8 has_key:u8 [key:[u8;32]]
//!               n:u32 peer:[u8;32]*n
//! manifest = path:u64  rel_len:u32 rel:utf8  size:u64 mode:u32 mtime_ns:i64
//!            deleted:u8  n_vv:u32 (peer:[u8;32] counter:u64)*n_vv
//!            n_chunks:u32 (chunk_id:[u8;32])*n_chunks
//! ```
//!
//! Counts are validated against the bytes actually present before anything is
//! allocated, so a hostile length cannot reserve memory. Paths are re-validated
//! as [`RelPath`]s, so a peer cannot name a file outside the folder.

use vuoxi_core::{ChunkId, Manifest, Meta, PathId, PeerId, RelPath, VersionVector, Wire};

/// Smallest encoded manifest, for bounding counts.
const MIN_MANIFEST: usize = 8 + 4 + 1 + 8 + 4 + 8 + 1 + 4 + 4;

/// Largest accepted frame body (tag included). A chunk plus its header must fit.
pub const MAX_FRAME: usize = 4 << 20;

#[derive(Debug, PartialEq, Eq)]
pub enum CodecError {
    TooLarge(usize),
    Truncated,
    Trailing,
    BadTag(u8),
    BadPath,
    BadFlag,
    BadFolder,
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(n) => write!(f, "frame of {n} bytes exceeds cap {MAX_FRAME}"),
            Self::Truncated => f.write_str("truncated message"),
            Self::Trailing => f.write_str("trailing bytes after message"),
            Self::BadTag(t) => write!(f, "unknown message tag {t}"),
            Self::BadPath => f.write_str("invalid relative path"),
            Self::BadFlag => f.write_str("invalid tombstone flag"),
            Self::BadFolder => f.write_str("invalid folder id"),
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
        Wire::Pull {
            folder,
            epoch,
            since,
            limit,
        } => {
            body.push(4);
            put_folder(&mut body, folder)?;
            body.extend_from_slice(&epoch.to_le_bytes());
            body.extend_from_slice(&since.to_le_bytes());
            body.extend_from_slice(&limit.to_le_bytes());
        }
        Wire::Put { folder, blob } => {
            body.push(6);
            put_folder(&mut body, folder)?;
            body.extend_from_slice(blob);
        }
        Wire::Ack(v) => {
            body.push(7);
            body.extend_from_slice(&v.to_le_bytes());
        }
        Wire::Has { folder, ids } => {
            body.push(8);
            put_folder(&mut body, folder)?;
            put_ids(&mut body, ids);
        }
        Wire::Missing(ids) => {
            body.push(9);
            put_ids(&mut body, ids);
        }
        Wire::Stash { folder, id, bytes } => {
            body.push(11);
            put_folder(&mut body, folder)?;
            body.extend_from_slice(&id.0);
            body.extend_from_slice(bytes);
        }
        Wire::Fetch { folder, id } => {
            body.push(12);
            put_folder(&mut body, folder)?;
            body.extend_from_slice(&id.0);
        }
        Wire::Sweep {
            folder,
            older_than_s,
        } => {
            body.push(13);
            put_folder(&mut body, folder)?;
            body.extend_from_slice(&older_than_s.to_le_bytes());
        }
        Wire::Reset { folder } => {
            body.push(14);
            put_folder(&mut body, folder)?;
        }
        Wire::Invite {
            folder,
            tag,
            key,
            members,
        } => {
            body.push(15);
            put_folder(&mut body, folder)?;
            put_folder(&mut body, tag)?;
            match key {
                Some(k) => {
                    body.push(1);
                    body.extend_from_slice(k);
                }
                None => body.push(0),
            }
            body.extend_from_slice(&(members.len() as u32).to_le_bytes());
            for p in members {
                body.extend_from_slice(&p.0);
            }
        }
        Wire::Blobs { epoch, head, blobs } => {
            body.push(10);
            body.extend_from_slice(&epoch.to_le_bytes());
            body.extend_from_slice(&head.to_le_bytes());
            body.extend_from_slice(&(blobs.len() as u32).to_le_bytes());
            for b in blobs {
                body.extend_from_slice(&(b.len() as u32).to_le_bytes());
                body.extend_from_slice(b);
            }
        }
        Wire::Batch {
            epoch,
            head,
            manifests,
        } => {
            body.push(5);
            body.extend_from_slice(&epoch.to_le_bytes());
            body.extend_from_slice(&head.to_le_bytes());
            body.extend_from_slice(&(manifests.len() as u32).to_le_bytes());
            for m in manifests {
                put_manifest(&mut body, m);
            }
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
        4 => {
            let folder = r.folder()?;
            Wire::Pull {
                folder,
                epoch: r.u64()?,
                since: r.u64()?,
                limit: r.u32()?,
            }
        }
        5 => {
            let (epoch, head) = (r.u64()?, r.u64()?);
            let n = r.count(MIN_MANIFEST)?;
            let mut manifests = Vec::with_capacity(n);
            for _ in 0..n {
                manifests.push(r.manifest()?);
            }
            Wire::Batch {
                epoch,
                head,
                manifests,
            }
        }
        6 => Wire::Put {
            folder: r.folder()?,
            blob: std::mem::take(&mut r.0).to_vec(),
        },
        7 => Wire::Ack(r.u64()?),
        8 => Wire::Has {
            folder: r.folder()?,
            ids: r.ids()?,
        },
        9 => Wire::Missing(r.ids()?),
        10 => {
            let (epoch, head) = (r.u64()?, r.u64()?);
            let n = r.count(4)?;
            let mut blobs = Vec::with_capacity(n);
            for _ in 0..n {
                let len = r.count(1)?;
                blobs.push(r.take(len)?.to_vec());
            }
            Wire::Blobs { epoch, head, blobs }
        }
        11 => Wire::Stash {
            folder: r.folder()?,
            id: ChunkId(r.array()?),
            bytes: std::mem::take(&mut r.0).to_vec(),
        },
        12 => Wire::Fetch {
            folder: r.folder()?,
            id: ChunkId(r.array()?),
        },
        13 => Wire::Sweep {
            folder: r.folder()?,
            older_than_s: r.u64()?,
        },
        14 => Wire::Reset {
            folder: r.folder()?,
        },
        15 => {
            let folder = r.folder()?;
            let tag = r.folder()?;
            let key = match r.u8()? {
                0 => None,
                1 => Some(r.array()?),
                _ => return Err(CodecError::BadFlag),
            };
            let n = r.count(32)?;
            let members = (0..n)
                .map(|_| Ok(PeerId(r.array()?)))
                .collect::<Result<_, CodecError>>()?;
            Wire::Invite {
                folder,
                tag,
                key,
                members,
            }
        }
        t => return Err(CodecError::BadTag(t)),
    };
    if r.0.is_empty() {
        Ok(msg)
    } else {
        Err(CodecError::Trailing)
    }
}

fn put_folder(out: &mut Vec<u8>, folder: &str) -> Result<(), CodecError> {
    let f = folder.as_bytes();
    if f.len() > u8::MAX as usize {
        return Err(CodecError::BadFolder);
    }
    out.push(f.len() as u8);
    out.extend_from_slice(f);
    Ok(())
}

fn put_ids(out: &mut Vec<u8>, ids: &[ChunkId]) {
    out.extend_from_slice(&(ids.len() as u32).to_le_bytes());
    for c in ids {
        out.extend_from_slice(&c.0);
    }
}

fn put_manifest(out: &mut Vec<u8>, m: &Manifest) {
    out.extend_from_slice(&m.path.0.to_le_bytes());
    let rel = m.rel.as_str().as_bytes();
    out.extend_from_slice(&(rel.len() as u32).to_le_bytes());
    out.extend_from_slice(rel);
    out.extend_from_slice(&m.meta.size.to_le_bytes());
    out.extend_from_slice(&m.meta.mode.to_le_bytes());
    out.extend_from_slice(&m.meta.mtime_ns.to_le_bytes());
    out.push(u8::from(m.meta.deleted));
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
    fn folder(&mut self) -> Result<String, CodecError> {
        let n = self.u8()? as usize;
        std::str::from_utf8(self.take(n)?)
            .map(str::to_owned)
            .map_err(|_| CodecError::BadFolder)
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
    fn ids(&mut self) -> Result<Vec<ChunkId>, CodecError> {
        let n = self.count(32)?;
        (0..n).map(|_| Ok(ChunkId(self.array()?))).collect()
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
        let n = self.count(1)?;
        let rel = std::str::from_utf8(self.take(n)?).map_err(|_| CodecError::BadPath)?;
        let rel = RelPath::new(rel).map_err(|_| CodecError::BadPath)?;
        let meta = Meta {
            size: self.u64()?,
            mode: self.u32()?,
            mtime_ns: self.u64()? as i64,
            deleted: match self.u8()? {
                0 => false,
                1 => true,
                _ => return Err(CodecError::BadFlag),
            },
        };
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
            rel,
            version: vv_from_entries(&entries)?,
            meta,
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
            rel: RelPath::new("docs/ä.md").unwrap(),
            version,
            meta: Meta {
                size: 123,
                mode: 0o600,
                mtime_ns: -5,
                deleted: false,
            },
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
            rel: RelPath::new("x").unwrap(),
            version: VersionVector::new(),
            meta: Meta {
                deleted: true,
                ..Meta::default()
            },
            chunks: vec![],
        }));
        roundtrip(Wire::Pull {
            folder: "notes".into(),
            epoch: 9,
            since: 41,
            limit: 500,
        });
        roundtrip(Wire::Batch {
            epoch: 9,
            head: 44,
            manifests: vec![manifest(), manifest()],
        });
        roundtrip(Wire::Batch {
            epoch: 1,
            head: 0,
            manifests: vec![],
        });
        roundtrip(Wire::Put {
            folder: "ab12".into(),
            blob: vec![1, 2, 3],
        });
        roundtrip(Wire::Ack(u64::MAX));
        roundtrip(Wire::Has {
            folder: "ab12".into(),
            ids: vec![ChunkId([1; 32]), ChunkId([2; 32])],
        });
        roundtrip(Wire::Stash {
            folder: "ab12".into(),
            id: ChunkId([3; 32]),
            bytes: vec![9; 40],
        });
        roundtrip(Wire::Fetch {
            folder: "ab12".into(),
            id: ChunkId([3; 32]),
        });
        roundtrip(Wire::Sweep {
            folder: "ab12".into(),
            older_than_s: 86400,
        });
        roundtrip(Wire::Reset {
            folder: "ab12".into(),
        });
        roundtrip(Wire::Invite {
            folder: "notes".into(),
            tag: "code".into(),
            key: Some([4; 32]),
            members: vec![PeerId([1; 32]), PeerId([2; 32])],
        });
        roundtrip(Wire::Invite {
            folder: "notes".into(),
            tag: String::new(),
            key: None,
            members: vec![],
        });
        roundtrip(Wire::Missing(vec![]));
        roundtrip(Wire::Blobs {
            epoch: 3,
            head: 9,
            blobs: vec![vec![], vec![7; 100]],
        });
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
        assert_eq!(decode_frame(&[0, 0, 0, 1, 99]), Err(CodecError::BadTag(99)));
        // a batch claiming a million manifests, carrying none
        let mut body = vec![5u8];
        body.extend_from_slice(&0u64.to_le_bytes());
        body.extend_from_slice(&0u64.to_le_bytes());
        body.extend_from_slice(&1_000_000u32.to_le_bytes());
        let mut f = (body.len() as u32).to_be_bytes().to_vec();
        f.extend(body);
        assert_eq!(decode_frame(&f), Err(CodecError::Truncated));
    }

    #[test]
    fn rejects_path_escape() {
        let mut m = manifest();
        m.rel = RelPath::new("ab/cd").unwrap();
        let mut f = encode_frame(&Wire::Offer(m)).unwrap();
        let at = f.windows(5).position(|w| w == b"ab/cd").unwrap();
        f[at..at + 5].copy_from_slice(b"../cd");
        assert_eq!(decode_frame(&f), Err(CodecError::BadPath));
    }
}
