//! Per-device append-only change log.
//!
//! // TODO: commonware journal. `commonware-storage`'s journal needs the
//! commonware async runtime, which would add an always-on executor to a daemon
//! whose goal is zero idle wakeups. Until that is measured, this is a small
//! file journal with the same contract: numbered records, durable on commit,
//! torn tail repaired on open.
//!
//! Layout: fixed 20-byte records, no header, record `n` (1-based) at byte
//! `(n - 1) * 20`:
//!
//! ```text
//! seq: u64 LE | path: u64 LE | check: u32 LE   (first 4 bytes of blake3(seq ‖ path))
//! ```
//!
//! Fixed width makes `since` a seek, and `seq == index + 1` makes a record that
//! landed out of order after a crash detectable.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use vuoxi_core::PathId;

const N: usize = 20;
const REC: u64 = N as u64;

fn check(seq: u64, path: u64) -> [u8; 4] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&seq.to_le_bytes());
    b[8..].copy_from_slice(&path.to_le_bytes());
    let h = blake3::hash(&b);
    [
        h.as_bytes()[0],
        h.as_bytes()[1],
        h.as_bytes()[2],
        h.as_bytes()[3],
    ]
}

fn encode(seq: u64, path: PathId) -> [u8; N] {
    let mut r = [0u8; N];
    r[..8].copy_from_slice(&seq.to_le_bytes());
    r[8..16].copy_from_slice(&path.0.to_le_bytes());
    r[16..].copy_from_slice(&check(seq, path.0));
    r
}

/// Decodes record `index` (0-based); `None` if torn, corrupt or out of order.
fn decode(index: u64, r: &[u8]) -> Option<(u64, PathId)> {
    let seq = u64::from_le_bytes(r[..8].try_into().ok()?);
    let path = u64::from_le_bytes(r[8..16].try_into().ok()?);
    (seq == index + 1 && r[16..20] == check(seq, path)).then_some((seq, PathId(path)))
}

pub struct Journal {
    file: File,
    /// Sequence number of the last record; 0 when empty.
    head: u64,
    /// Highest sequence known to be fsynced.
    durable: u64,
}

impl Journal {
    /// Opens or creates the journal, verifying every record and truncating at
    /// the first one that is torn, corrupt or out of sequence.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let len = file.metadata()?.len();
        let mut head = 0u64;
        let mut buf = vec![0u8; N * 4096];
        'scan: while (head + 1) * REC <= len {
            let want = (len - head * REC).min(buf.len() as u64) as usize;
            let n = read_full(&file, &mut buf[..want], head * REC)?;
            for r in buf[..n].as_chunks::<N>().0.iter() {
                if decode(head, r).is_none() {
                    break 'scan;
                }
                head += 1;
            }
            if n < want {
                break;
            }
        }
        if head * REC != len {
            file.set_len(head * REC)?;
            file.sync_all()?;
        }
        Ok(Self {
            file,
            head,
            durable: head,
        })
    }

    /// Writes one record and returns its sequence number. Not durable until
    /// [`commit`](Self::commit); a batch of appends shares one fsync.
    pub fn append(&mut self, path: PathId) -> io::Result<u64> {
        let seq = self.head + 1;
        self.file
            .write_all_at(&encode(seq, path), self.head * REC)?;
        self.head = seq;
        Ok(seq)
    }

    /// fsync. Returns the sequence number that is now durable.
    pub fn commit(&mut self) -> io::Result<u64> {
        if self.durable != self.head {
            self.file.sync_data()?;
            self.durable = self.head;
        }
        Ok(self.durable)
    }

    /// Records with sequence greater than `cursor`, at most `limit`.
    pub fn since(&self, cursor: u64, limit: usize) -> io::Result<Vec<(u64, PathId)>> {
        let n = (self.head.saturating_sub(cursor)).min(limit as u64) as usize;
        let mut buf = vec![0u8; n * N];
        let got = read_full(&self.file, &mut buf, cursor * REC)?;
        if got != buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "journal shrank",
            ));
        }
        buf.as_chunks::<N>()
            .0
            .iter()
            .enumerate()
            .map(|(i, r)| {
                decode(cursor + i as u64, r)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "journal corrupt"))
            })
            .collect()
    }

    /// Sequence number of the newest record; 0 if the journal is empty.
    pub fn head(&self) -> u64 {
        self.head
    }
}

fn read_full(f: &File, buf: &mut [u8], mut off: u64) -> io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        match f.read_at(&mut buf[done..], off) {
            Ok(0) => break,
            Ok(n) => {
                done += n;
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir;

    #[test]
    fn append_since_reopen() {
        let dir = testdir("journal-basic");
        let p = dir.join("j");
        let mut j = Journal::open(&p).unwrap();
        assert_eq!(j.head(), 0);
        for i in 0..10 {
            assert_eq!(j.append(PathId(100 + i)).unwrap(), i + 1);
        }
        assert_eq!(j.commit().unwrap(), 10);
        assert_eq!(
            j.since(7, 100).unwrap(),
            vec![(8, PathId(107)), (9, PathId(108)), (10, PathId(109))]
        );
        assert_eq!(j.since(0, 2).unwrap().len(), 2);
        assert!(j.since(10, 5).unwrap().is_empty());
        drop(j);

        let mut j = Journal::open(&p).unwrap();
        assert_eq!(j.head(), 10);
        assert_eq!(j.append(PathId(1)).unwrap(), 11);
        assert_eq!(j.since(10, 5).unwrap(), vec![(11, PathId(1))]);
    }

    #[test]
    fn torn_tail_is_truncated() {
        let dir = testdir("journal-torn");
        let p = dir.join("j");
        let mut j = Journal::open(&p).unwrap();
        for i in 0..5 {
            j.append(PathId(i)).unwrap();
        }
        j.commit().unwrap();
        drop(j);

        // Half a record of garbage after the last good one.
        let f = OpenOptions::new().write(true).open(&p).unwrap();
        f.write_all_at(&[0xAB; 11], 5 * REC).unwrap();
        drop(f);
        let mut j = Journal::open(&p).unwrap();
        assert_eq!(j.head(), 5);
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 5 * REC);
        assert_eq!(j.append(PathId(9)).unwrap(), 6);
        j.commit().unwrap();
        drop(j);

        // A full-size but corrupt record in the middle cuts everything after it.
        let f = OpenOptions::new().write(true).open(&p).unwrap();
        f.write_all_at(&[0xFF; 20], 3 * REC).unwrap();
        drop(f);
        let j = Journal::open(&p).unwrap();
        assert_eq!(j.head(), 3);
        assert_eq!(j.since(0, 10).unwrap().len(), 3);
    }
}
