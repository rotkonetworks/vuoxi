//! Raw fanotify event bytes into [`Record`]s. Pure and bounds-checked: the
//! buffer comes from the kernel, but a parser bug here would run with privilege.
//!
//! Layout (`fanotify(7)`): `fanotify_event_metadata` (24 bytes) followed by
//! info records up to `event_len`. With `FAN_REPORT_DFID_NAME_TARGET` an event
//! carries a `DFID_NAME` record (parent handle + NUL-terminated name) and a
//! `FID` record (the object's own handle).

use std::fmt;

use crate::record::{Handle, Kind, MAX_HANDLE, MAX_NAME, Record};

pub const METADATA_VERSION: u8 = 3;
const METADATA_LEN: usize = 24;
const NOFD: i32 = -1;

pub const FAN_MODIFY: u64 = 0x0000_0002;
pub const FAN_MOVED_FROM: u64 = 0x0000_0040;
pub const FAN_MOVED_TO: u64 = 0x0000_0080;
pub const FAN_CREATE: u64 = 0x0000_0100;
pub const FAN_DELETE: u64 = 0x0000_0200;
pub const FAN_Q_OVERFLOW: u64 = 0x0000_4000;
pub const FAN_ONDIR: u64 = 0x4000_0000;
/// Events the watcher asks for (overflow is always delivered).
pub const WANTED: u64 =
    FAN_MODIFY | FAN_MOVED_FROM | FAN_MOVED_TO | FAN_CREATE | FAN_DELETE | FAN_ONDIR;

const INFO_FID: u8 = 1;
const INFO_DFID_NAME: u8 = 2;
const INFO_DFID: u8 = 3;

const KINDS: [(u64, Kind); 5] = [
    (FAN_CREATE, Kind::Create),
    (FAN_MODIFY, Kind::Modify),
    (FAN_DELETE, Kind::Delete),
    (FAN_MOVED_FROM, Kind::MovedFrom),
    (FAN_MOVED_TO, Kind::MovedTo),
];

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Kernel and build disagree on the metadata layout.
    Version(u8),
    /// An event length that cannot be right.
    BadLength,
    /// The event came with an open file descriptor, which FID mode never does.
    UnexpectedFd(i32),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Version(v) => write!(f, "fanotify metadata version {v}"),
            Self::BadLength => f.write_str("bad fanotify event length"),
            Self::UnexpectedFd(fd) => write!(f, "event carried fd {fd}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse every event in `buf` (one `read(2)` worth), appending to `out`.
/// Events whose info records are malformed are skipped, not fatal.
pub fn parse_events(buf: &[u8], out: &mut Vec<Record>) -> Result<(), ParseError> {
    let mut rest = buf;
    while rest.len() >= METADATA_LEN {
        let event_len = u32::from_ne_bytes(rest[0..4].try_into().unwrap()) as usize;
        let vers = rest[4];
        let meta_len = u16::from_ne_bytes(rest[6..8].try_into().unwrap()) as usize;
        let mask = u64::from_ne_bytes(rest[8..16].try_into().unwrap());
        let fd = i32::from_ne_bytes(rest[16..20].try_into().unwrap());
        if vers != METADATA_VERSION {
            return Err(ParseError::Version(vers));
        }
        if event_len < METADATA_LEN || meta_len != METADATA_LEN || event_len > rest.len() {
            return Err(ParseError::BadLength);
        }
        if fd != NOFD {
            return Err(ParseError::UnexpectedFd(fd));
        }
        if let Some(infos) = parse_infos(&rest[meta_len..event_len]) {
            emit(mask, infos, out);
        }
        rest = &rest[event_len..];
    }
    Ok(())
}

#[derive(Default)]
struct Infos {
    fsid: [u8; 8],
    object: Option<Handle>,
    parent: Option<Handle>,
    name: Vec<u8>,
}

fn emit(mask: u64, i: Infos, out: &mut Vec<Record>) {
    if mask & FAN_Q_OVERFLOW != 0 {
        out.push(Record {
            kind: Kind::Overflow,
            is_dir: false,
            fsid: [0; 8],
            object: None,
            parent: None,
            name: vec![],
        });
    }
    for (bit, kind) in KINDS {
        if mask & bit != 0 {
            out.push(Record {
                kind,
                is_dir: mask & FAN_ONDIR != 0,
                fsid: i.fsid,
                object: i.object.clone(),
                parent: i.parent.clone(),
                name: i.name.clone(),
            });
        }
    }
}

fn parse_infos(mut rest: &[u8]) -> Option<Infos> {
    let mut infos = Infos::default();
    while !rest.is_empty() {
        let hdr = rest.get(..4)?;
        let (ty, len) = (hdr[0], u16::from_ne_bytes([hdr[2], hdr[3]]) as usize);
        let rec = rest.get(4..len)?; // len includes the header
        rest = &rest[len..];
        if !matches!(ty, INFO_FID | INFO_DFID_NAME | INFO_DFID) {
            continue;
        }
        let fsid: [u8; 8] = rec.get(..8)?.try_into().ok()?;
        let hb = u32::from_ne_bytes(rec.get(8..12)?.try_into().ok()?) as usize;
        let hty = i32::from_ne_bytes(rec.get(12..16)?.try_into().ok()?);
        if hb > MAX_HANDLE {
            return None;
        }
        let bytes = rec.get(16..16 + hb)?.to_vec();
        let handle = Handle { ty: hty, bytes };
        infos.fsid = fsid;
        match ty {
            INFO_FID => infos.object = Some(handle),
            INFO_DFID => infos.parent = Some(handle),
            _ => {
                let tail = &rec[16 + hb..];
                let end = tail.iter().position(|&b| b == 0)?;
                if end > MAX_NAME {
                    return None;
                }
                infos.name = tail[..end].to_vec();
                infos.parent = Some(handle);
            }
        }
    }
    Some(infos)
}

/// Build a kernel-shaped event; for tests and the sandbox self-test.
#[doc(hidden)]
pub fn build_event(
    mask: u64,
    fsid: [u8; 8],
    object: Option<&Handle>,
    parent: Option<(&Handle, &[u8])>,
) -> Vec<u8> {
    fn info(ty: u8, fsid: [u8; 8], h: &Handle, name: Option<&[u8]>) -> Vec<u8> {
        let mut b = vec![ty, 0, 0, 0];
        b.extend_from_slice(&fsid);
        b.extend_from_slice(&(h.bytes.len() as u32).to_ne_bytes());
        b.extend_from_slice(&h.ty.to_ne_bytes());
        b.extend_from_slice(&h.bytes);
        if let Some(n) = name {
            b.extend_from_slice(n);
            b.push(0);
        }
        while b.len() % 4 != 0 {
            b.push(0);
        }
        let l = (b.len() as u16).to_ne_bytes();
        b[2..4].copy_from_slice(&l);
        b
    }
    let mut body = Vec::new();
    if let Some((h, n)) = parent {
        body.extend(info(INFO_DFID_NAME, fsid, h, Some(n)));
    }
    if let Some(h) = object {
        body.extend(info(INFO_FID, fsid, h, None));
    }
    let mut ev = Vec::new();
    ev.extend_from_slice(&((METADATA_LEN + body.len()) as u32).to_ne_bytes());
    ev.push(METADATA_VERSION);
    ev.push(0);
    ev.extend_from_slice(&(METADATA_LEN as u16).to_ne_bytes());
    ev.extend_from_slice(&mask.to_ne_bytes());
    ev.extend_from_slice(&NOFD.to_ne_bytes());
    ev.extend_from_slice(&0i32.to_ne_bytes());
    ev.extend(body);
    ev
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u8, len: usize) -> Handle {
        Handle {
            ty: 1,
            bytes: vec![n; len],
        }
    }

    #[test]
    fn create_dir_event() {
        let (obj, par) = (h(5, 12), h(1, 8));
        let ev = build_event(
            FAN_CREATE | FAN_ONDIR,
            [7; 8],
            Some(&obj),
            Some((&par, b"target")),
        );
        let mut out = Vec::new();
        parse_events(&ev, &mut out).unwrap();
        assert_eq!(out.len(), 1);
        let r = &out[0];
        assert_eq!(r.kind, Kind::Create);
        assert!(r.is_dir);
        assert_eq!(r.fsid, [7; 8]);
        assert_eq!(r.object.as_ref(), Some(&obj));
        assert_eq!(r.parent.as_ref(), Some(&par));
        assert_eq!(r.name, b"target");
    }

    #[test]
    fn multi_bit_mask_and_back_to_back_events() {
        let (obj, par) = (h(5, 12), h(1, 8));
        let mut buf = build_event(
            FAN_CREATE | FAN_MODIFY,
            [0; 8],
            Some(&obj),
            Some((&par, b"a")),
        );
        buf.extend(build_event(
            FAN_DELETE,
            [0; 8],
            Some(&obj),
            Some((&par, b"bc")),
        ));
        let mut out = Vec::new();
        parse_events(&buf, &mut out).unwrap();
        let kinds: Vec<_> = out.iter().map(|r| r.kind).collect();
        assert_eq!(kinds, [Kind::Create, Kind::Modify, Kind::Delete]);
        assert_eq!(out[2].name, b"bc");
        assert!(!out[2].is_dir);
    }

    #[test]
    fn overflow() {
        let ev = build_event(FAN_Q_OVERFLOW, [0; 8], None, None);
        let mut out = Vec::new();
        parse_events(&ev, &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, Kind::Overflow);
    }

    #[test]
    fn rejects_bad_framing() {
        let ev = build_event(FAN_CREATE, [0; 8], None, None);
        let mut out = Vec::new();

        let mut bad = ev.clone();
        bad[4] = 2;
        assert_eq!(parse_events(&bad, &mut out), Err(ParseError::Version(2)));

        let mut bad = ev.clone();
        bad[0..4].copy_from_slice(&1000u32.to_ne_bytes());
        assert_eq!(parse_events(&bad, &mut out), Err(ParseError::BadLength));

        let mut bad = ev.clone();
        bad[16..20].copy_from_slice(&5i32.to_ne_bytes());
        assert_eq!(
            parse_events(&bad, &mut out),
            Err(ParseError::UnexpectedFd(5))
        );
        assert!(out.is_empty());
    }

    #[test]
    fn malformed_info_skips_event_only() {
        let (obj, par) = (h(5, 12), h(1, 8));
        let mut bad = build_event(FAN_CREATE, [0; 8], Some(&obj), Some((&par, b"x")));
        // claim a handle far longer than the record
        // that field sits after the 24-byte metadata and 4-byte info header
        let off = 24 + 4 + 8;
        bad[off..off + 4].copy_from_slice(&9999u32.to_ne_bytes());
        bad.extend(build_event(
            FAN_DELETE,
            [0; 8],
            Some(&obj),
            Some((&par, b"ok")),
        ));
        let mut out = Vec::new();
        parse_events(&bad, &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, b"ok");
    }

    #[test]
    fn name_without_nul_is_dropped() {
        let (obj, par) = (h(5, 12), h(1, 8));
        let mut ev = build_event(FAN_CREATE, [0; 8], Some(&obj), Some((&par, b"abc")));
        // overwrite the name and padding with non-zero bytes
        let name_at = 24 + 4 + 8 + 8 + 8;
        for b in &mut ev[name_at..name_at + 4] {
            *b = b'z';
        }
        let mut out = Vec::new();
        parse_events(&ev, &mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn truncated_tail_is_ignored() {
        let mut out = Vec::new();
        parse_events(&[1, 2, 3], &mut out).unwrap();
        assert!(out.is_empty());
    }
}
