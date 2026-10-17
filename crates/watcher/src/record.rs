//! The record the watcher writes to the daemon, and its binary encoding.
//!
//! Records carry kernel file handles and raw name bytes, nothing interpreted:
//! no paths, no UTF-8, no peer data. A stream starts with [`HELLO`], then
//! frames of `u32 LE body length | body`. Body:
//!
//! ```text
//! kind u8 | flags u8 | fsid [8] | object? | parent? | name_len u16 LE | name
//! handle = type i32 LE | len u8 | bytes
//! flags: 1 = is a directory, 2 = object present, 4 = parent present
//! ```

use std::fmt;

/// Stream preamble: magic plus format version.
pub const HELLO: [u8; 4] = *b"vxw\x01";
/// Kernel limit (`MAX_HANDLE_SZ`).
pub const MAX_HANDLE: usize = 128;
/// Kernel limit (`NAME_MAX`).
pub const MAX_NAME: usize = 255;
/// Largest legal body: header, two handles, name.
pub const MAX_BODY: usize = 1 + 1 + 8 + 2 * (4 + 1 + MAX_HANDLE) + 2 + MAX_NAME;

const F_DIR: u8 = 1;
const F_OBJECT: u8 = 2;
const F_PARENT: u8 = 4;

/// What happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    Create = 1,
    Modify = 2,
    Delete = 3,
    MovedFrom = 4,
    MovedTo = 5,
    /// The kernel queue overflowed: events were lost, the daemon must reconcile.
    Overflow = 6,
}

impl Kind {
    fn from_u8(b: u8) -> Option<Kind> {
        Some(match b {
            1 => Kind::Create,
            2 => Kind::Modify,
            3 => Kind::Delete,
            4 => Kind::MovedFrom,
            5 => Kind::MovedTo,
            6 => Kind::Overflow,
            _ => return None,
        })
    }
}

/// An opaque kernel file handle (`struct file_handle` without the length word).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Handle {
    pub ty: i32,
    pub bytes: Vec<u8>,
}

/// One change. `object` is the file or directory itself, `parent` the
/// directory holding it, `name` its entry name in `parent`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub kind: Kind,
    pub is_dir: bool,
    /// Filesystem id, raw bytes as the kernel reported them.
    pub fsid: [u8; 8],
    pub object: Option<Handle>,
    pub parent: Option<Handle>,
    pub name: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum EncodeError {
    HandleTooLong,
    NameTooLong,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Length prefix beyond [`MAX_BODY`].
    FrameTooLarge(usize),
    BadKind(u8),
    /// Body shorter or longer than its own fields say.
    Malformed,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HandleTooLong => f.write_str("handle longer than MAX_HANDLE"),
            Self::NameTooLong => f.write_str("name longer than MAX_NAME"),
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameTooLarge(n) => write!(f, "frame of {n} bytes exceeds limit"),
            Self::BadKind(k) => write!(f, "unknown kind {k}"),
            Self::Malformed => f.write_str("malformed frame"),
        }
    }
}

impl std::error::Error for EncodeError {}
impl std::error::Error for DecodeError {}

fn put_handle(out: &mut Vec<u8>, h: &Handle) -> Result<(), EncodeError> {
    if h.bytes.len() > MAX_HANDLE {
        return Err(EncodeError::HandleTooLong);
    }
    out.extend_from_slice(&h.ty.to_le_bytes());
    out.push(h.bytes.len() as u8);
    out.extend_from_slice(&h.bytes);
    Ok(())
}

impl Record {
    /// Append this record as one frame. On error `out` is left unchanged.
    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), EncodeError> {
        if self.name.len() > MAX_NAME {
            return Err(EncodeError::NameTooLong);
        }
        let start = out.len();
        out.extend_from_slice(&[0; 4]);
        let mut flags = 0;
        if self.is_dir {
            flags |= F_DIR;
        }
        if self.object.is_some() {
            flags |= F_OBJECT;
        }
        if self.parent.is_some() {
            flags |= F_PARENT;
        }
        out.push(self.kind as u8);
        out.push(flags);
        out.extend_from_slice(&self.fsid);
        let res = (|| {
            for h in [&self.object, &self.parent].into_iter().flatten() {
                put_handle(out, h)?;
            }
            Ok(())
        })();
        if let Err(e) = res {
            out.truncate(start);
            return Err(e);
        }
        out.extend_from_slice(&(self.name.len() as u16).to_le_bytes());
        out.extend_from_slice(&self.name);
        let body = (out.len() - start - 4) as u32;
        out[start..start + 4].copy_from_slice(&body.to_le_bytes());
        Ok(())
    }

    /// Decode one frame from the front of `buf`. `Ok(None)` means more bytes
    /// are needed; otherwise the record and the bytes consumed.
    pub fn decode(buf: &[u8]) -> Result<Option<(Record, usize)>, DecodeError> {
        let Some(len) = buf.first_chunk::<4>() else {
            return Ok(None);
        };
        let len = u32::from_le_bytes(*len) as usize;
        if len > MAX_BODY {
            return Err(DecodeError::FrameTooLarge(len));
        }
        let Some(body) = buf[4..].get(..len) else {
            return Ok(None);
        };
        Ok(Some((Self::decode_body(body)?, 4 + len)))
    }

    fn decode_body(body: &[u8]) -> Result<Record, DecodeError> {
        let mut r = Reader(body);
        let kind_b = r.u8()?;
        let kind = Kind::from_u8(kind_b).ok_or(DecodeError::BadKind(kind_b))?;
        let flags = r.u8()?;
        if flags & !(F_DIR | F_OBJECT | F_PARENT) != 0 {
            return Err(DecodeError::Malformed);
        }
        let fsid = r.take(8)?.try_into().unwrap();
        let object = if flags & F_OBJECT != 0 {
            Some(r.handle()?)
        } else {
            None
        };
        let parent = if flags & F_PARENT != 0 {
            Some(r.handle()?)
        } else {
            None
        };
        let name_len = u16::from_le_bytes(r.take(2)?.try_into().unwrap()) as usize;
        if name_len > MAX_NAME {
            return Err(DecodeError::Malformed);
        }
        let name = r.take(name_len)?.to_vec();
        if !r.0.is_empty() {
            return Err(DecodeError::Malformed);
        }
        Ok(Record {
            kind,
            is_dir: flags & F_DIR != 0,
            fsid,
            object,
            parent,
            name,
        })
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let (head, tail) = self.0.split_at_checked(n).ok_or(DecodeError::Malformed)?;
        self.0 = tail;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    fn handle(&mut self) -> Result<Handle, DecodeError> {
        let ty = i32::from_le_bytes(self.take(4)?.try_into().unwrap());
        let len = self.u8()? as usize;
        if len > MAX_HANDLE {
            return Err(DecodeError::Malformed);
        }
        Ok(Handle {
            ty,
            bytes: self.take(len)?.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Record {
        Record {
            kind: Kind::Create,
            is_dir: true,
            fsid: [1, 2, 3, 4, 5, 6, 7, 8],
            object: Some(Handle {
                ty: 1,
                bytes: vec![9; 12],
            }),
            parent: Some(Handle {
                ty: 1,
                bytes: vec![7; 8],
            }),
            name: b"caf\xc3\xa9\xff".to_vec(),
        }
    }

    #[test]
    fn round_trip() {
        let r = sample();
        let mut buf = Vec::new();
        r.encode_into(&mut buf).unwrap();
        let (back, n) = Record::decode(&buf).unwrap().unwrap();
        assert_eq!(back, r);
        assert_eq!(n, buf.len());
    }

    #[test]
    fn round_trip_minimal() {
        let r = Record {
            kind: Kind::Overflow,
            is_dir: false,
            fsid: [0; 8],
            object: None,
            parent: None,
            name: vec![],
        };
        let mut buf = Vec::new();
        r.encode_into(&mut buf).unwrap();
        assert_eq!(Record::decode(&buf).unwrap().unwrap().0, r);
    }

    #[test]
    fn stream_of_frames_and_partial_input() {
        let mut buf = Vec::new();
        sample().encode_into(&mut buf).unwrap();
        let mut second = sample();
        second.kind = Kind::Delete;
        second.encode_into(&mut buf).unwrap();
        for cut in 0..buf.len() {
            // never an error on a truncated prefix, never a bogus record
            if let Some((r, n)) = Record::decode(&buf[..cut]).unwrap() {
                assert_eq!(r, sample());
                assert!(n <= cut);
            }
        }
        let (a, n) = Record::decode(&buf).unwrap().unwrap();
        let (b, m) = Record::decode(&buf[n..]).unwrap().unwrap();
        assert_eq!((a.kind, b.kind), (Kind::Create, Kind::Delete));
        assert_eq!(n + m, buf.len());
    }

    #[test]
    fn rejects_garbage() {
        let mut buf = Vec::new();
        sample().encode_into(&mut buf).unwrap();

        let mut bad = buf.clone();
        bad[4] = 99;
        assert_eq!(Record::decode(&bad), Err(DecodeError::BadKind(99)));

        let mut bad = buf.clone();
        bad[5] |= 0x80;
        assert_eq!(Record::decode(&bad), Err(DecodeError::Malformed));

        // trailing byte inside the declared body
        let mut bad = buf.clone();
        bad.push(0);
        let n = u32::from_le_bytes(bad[..4].try_into().unwrap()) + 1;
        bad[..4].copy_from_slice(&n.to_le_bytes());
        assert_eq!(Record::decode(&bad), Err(DecodeError::Malformed));

        let huge = (MAX_BODY as u32 + 1).to_le_bytes();
        assert_eq!(
            Record::decode(&huge),
            Err(DecodeError::FrameTooLarge(MAX_BODY + 1))
        );
    }

    #[test]
    fn encode_limits_leave_buffer_untouched() {
        let mut r = sample();
        r.name = vec![b'a'; MAX_NAME + 1];
        let mut buf = vec![42];
        assert_eq!(r.encode_into(&mut buf), Err(EncodeError::NameTooLong));
        r = sample();
        r.parent.as_mut().unwrap().bytes = vec![0; MAX_HANDLE + 1];
        assert_eq!(r.encode_into(&mut buf), Err(EncodeError::HandleTooLong));
        assert_eq!(buf, [42]);
    }

    #[test]
    fn max_sized_record_fits_limit() {
        let r = Record {
            kind: Kind::Modify,
            is_dir: false,
            fsid: [0; 8],
            object: Some(Handle {
                ty: 1,
                bytes: vec![0; MAX_HANDLE],
            }),
            parent: Some(Handle {
                ty: 1,
                bytes: vec![0; MAX_HANDLE],
            }),
            name: vec![b'x'; MAX_NAME],
        };
        let mut buf = Vec::new();
        r.encode_into(&mut buf).unwrap();
        assert_eq!(buf.len(), 4 + MAX_BODY);
        assert_eq!(Record::decode(&buf).unwrap().unwrap().0, r);
    }
}
