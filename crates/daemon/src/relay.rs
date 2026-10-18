//! A relay: stores folders it cannot read and serves them to the devices that can.
//!
//! Per folder (named by an opaque tag, see [`crate::seal`]):
//!
//! ```text
//! <dir>/<tag>/epoch                random u64, new on every `Reset`
//! <dir>/<tag>/log                  records: len:u32 LE ‖ sealed manifest
//! <dir>/<tag>/chunks/<2hex>/<hex>  sealed chunk, named by its relay id
//! ```
//!
//! The log is append-only and fsynced per `Put`; a torn tail is cut on open.
//! Devices pull it with the same cursor-and-epoch rules as from a peer.
//!
//! The relay cannot tell which data is still needed, so devices tell it:
//! a chunk's mtime is the last time a device stored it or named it in a
//! `Has`, and `Sweep` drops chunks older than the age a device asks for.
//! `Reset` empties the log under a new epoch; devices then push what they
//! hold again.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tower_service::Service;
use vuoxi_core::{ChunkId, Error, PeerId, Wire};

/// Blobs per pull answer; a reply also stops early near this many bytes.
const MAX_BLOBS: usize = 256;
const MAX_REPLY_BYTES: usize = 2 << 20;
/// Largest sealed chunk or manifest accepted (a 256 KiB chunk plus overhead, or a big manifest).
const MAX_BLOB: usize = 3 << 20;

struct Log {
    file: File,
    /// Byte offset of each record, record `n` (1-based) at index `n - 1`.
    offsets: Vec<u64>,
    end: u64,
    epoch: u64,
}

fn valid_tag(t: &str) -> bool {
    (16..=64).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_hexdigit())
}

fn hex(id: &ChunkId) -> String {
    id.0.iter().map(|b| format!("{b:02x}")).collect()
}

fn new_epoch(dir: &Path) -> io::Result<u64> {
    let mut b = [0u8; 8];
    File::open("/dev/urandom")?.read_exact(&mut b)?;
    let tmp = dir.join("epoch.tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(&b)?;
    f.sync_data()?;
    fs::rename(tmp, dir.join("epoch"))?;
    Ok(u64::from_le_bytes(b))
}

impl Log {
    fn open(dir: &Path) -> io::Result<Log> {
        fs::create_dir_all(dir.join("chunks"))?;
        let epoch = match fs::read(dir.join("epoch")) {
            Ok(b) if b.len() == 8 => u64::from_le_bytes(b.try_into().unwrap()),
            _ => new_epoch(dir)?,
        };
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(dir.join("log"))?;
        let mut all = Vec::new();
        file.read_to_end(&mut all)?;
        let (mut offsets, mut at) = (Vec::new(), 0usize);
        while let Some(len) = all.get(at..at + 4) {
            let len = u32::from_le_bytes(len.try_into().unwrap()) as usize;
            if at + 4 + len > all.len() {
                break;
            }
            offsets.push(at as u64);
            at += 4 + len;
        }
        if at < all.len() {
            file.set_len(at as u64)?;
        }
        Ok(Log {
            file,
            offsets,
            end: at as u64,
            epoch,
        })
    }

    fn append(&mut self, blob: &[u8]) -> io::Result<u64> {
        let mut rec = (blob.len() as u32).to_le_bytes().to_vec();
        rec.extend_from_slice(blob);
        self.file.write_all(&rec)?;
        self.file.sync_data()?;
        self.offsets.push(self.end);
        self.end += rec.len() as u64;
        Ok(self.offsets.len() as u64)
    }

    /// Empty the log under a new epoch. The epoch is written first: a crash
    /// in between leaves an old log under a new epoch, which devices simply
    /// pull again.
    fn reset(&mut self, dir: &Path) -> io::Result<u64> {
        self.epoch = new_epoch(dir)?;
        self.file.set_len(0)?;
        self.file.sync_data()?;
        self.offsets.clear();
        self.end = 0;
        Ok(self.epoch)
    }

    fn since(&self, since: u64, limit: usize) -> io::Result<(u64, Vec<Vec<u8>>)> {
        use std::os::unix::fs::FileExt;
        let mut out = Vec::new();
        let (mut seq, mut bytes) = (since, 0);
        for (i, &off) in self.offsets.iter().enumerate().skip(since as usize) {
            if out.len() >= limit || bytes >= MAX_REPLY_BYTES {
                break;
            }
            let mut len = [0u8; 4];
            self.file.read_exact_at(&mut len, off)?;
            let mut b = vec![0u8; u32::from_le_bytes(len) as usize];
            self.file.read_exact_at(&mut b, off + 4)?;
            bytes += b.len();
            out.push(b);
            seq = i as u64 + 1;
        }
        Ok((seq, out))
    }
}

pub struct RelayStore {
    dir: PathBuf,
    logs: Mutex<HashMap<String, Arc<Mutex<Log>>>>,
}

impl RelayStore {
    pub fn open(dir: &Path) -> io::Result<Arc<RelayStore>> {
        fs::create_dir_all(dir)?;
        Ok(Arc::new(RelayStore {
            dir: dir.to_owned(),
            logs: Mutex::new(HashMap::new()),
        }))
    }

    fn log(&self, tag: &str) -> io::Result<Arc<Mutex<Log>>> {
        if !valid_tag(tag) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bad folder tag",
            ));
        }
        let mut logs = self.logs.lock().unwrap();
        if let Some(l) = logs.get(tag) {
            return Ok(l.clone());
        }
        let l = Arc::new(Mutex::new(Log::open(&self.dir.join(tag))?));
        logs.insert(tag.to_owned(), l.clone());
        Ok(l)
    }

    fn chunks_dir(&self, tag: &str) -> io::Result<PathBuf> {
        if !valid_tag(tag) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bad folder tag",
            ));
        }
        Ok(self.dir.join(tag).join("chunks"))
    }

    fn chunk_path(&self, tag: &str, id: &ChunkId) -> io::Result<PathBuf> {
        let h = hex(id);
        Ok(self.chunks_dir(tag)?.join(&h[..2]).join(h))
    }

    /// Mark a chunk as still in use; false if it is not here.
    fn touch(p: &Path) -> bool {
        File::open(p)
            .and_then(|f| f.set_modified(std::time::SystemTime::now()))
            .is_ok()
    }

    fn sweep(&self, tag: &str, older_than: std::time::Duration) -> io::Result<u64> {
        let cutoff = std::time::SystemTime::now() - older_than;
        let mut gone = 0;
        let Ok(shards) = fs::read_dir(self.chunks_dir(tag)?) else {
            return Ok(0);
        };
        for shard in shards.flatten() {
            for f in fs::read_dir(shard.path())?.flatten() {
                let old = f
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t < cutoff);
                if old && fs::remove_file(f.path()).is_ok() {
                    gone += 1;
                }
            }
        }
        Ok(gone)
    }

    fn handle(&self, w: Wire) -> io::Result<Option<Wire>> {
        Ok(match w {
            Wire::Put { folder, blob } if blob.len() <= MAX_BLOB => {
                let seq = self.log(&folder)?.lock().unwrap().append(&blob)?;
                Some(Wire::Ack(seq))
            }
            Wire::Pull {
                folder,
                epoch,
                since,
                limit,
            } => {
                let log = self.log(&folder)?;
                let log = log.lock().unwrap();
                let since = if epoch == log.epoch { since } else { 0 };
                let (head, blobs) = log.since(since, (limit as usize).min(MAX_BLOBS))?;
                Some(Wire::Blobs {
                    epoch: log.epoch,
                    head,
                    blobs,
                })
            }
            Wire::Has { folder, ids } => {
                let mut missing = Vec::new();
                for id in ids {
                    if !Self::touch(&self.chunk_path(&folder, &id)?) {
                        missing.push(id);
                    }
                }
                Some(Wire::Missing(missing))
            }
            Wire::Stash { folder, id, bytes } if bytes.len() <= MAX_BLOB => {
                let p = self.chunk_path(&folder, &id)?;
                if !Self::touch(&p) {
                    fs::create_dir_all(p.parent().unwrap())?;
                    let tmp = p.with_extension("tmp");
                    let mut f = File::create(&tmp)?;
                    f.write_all(&bytes)?;
                    f.sync_data()?;
                    fs::rename(tmp, &p)?;
                }
                Some(Wire::Ack(0))
            }
            Wire::Fetch { folder, id } => fs::read(self.chunk_path(&folder, &id)?)
                .ok()
                .map(|b| Wire::Chunk(id, b)),
            Wire::Sweep {
                folder,
                older_than_s,
            } => Some(Wire::Ack(
                self.sweep(&folder, std::time::Duration::from_secs(older_than_s))?,
            )),
            Wire::Reset { folder } => {
                let log = self.log(&folder)?;
                let epoch = log.lock().unwrap().reset(&self.dir.join(&folder))?;
                Some(Wire::Ack(epoch))
            }
            _ => None,
        })
    }
}

/// The transport's view of a relay. Only allowlisted devices reach it.
#[derive(Clone)]
pub struct RelayHandler(pub Arc<RelayStore>);

impl Service<(PeerId, Wire)> for RelayHandler {
    type Response = Option<Wire>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Option<Wire>, Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, (_, w): (PeerId, Wire)) -> Self::Future {
        let store = self.0.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || store.handle(w))
                .await
                .map_err(|e| Error::Transport(e.to_string()))?
                .map_err(Error::Io)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-relay")
            .join(name);
        let _ = fs::remove_dir_all(&d);
        d
    }

    const TAG: &str = "00112233445566778899aabbccddeeff";

    #[test]
    fn log_appends_survives_reopen_and_cuts_torn_tail() {
        let d = dir("log");
        let s = RelayStore::open(&d).unwrap();
        for i in 0..3u8 {
            let r = s
                .handle(Wire::Put {
                    folder: TAG.into(),
                    blob: vec![i; 5],
                })
                .unwrap();
            assert_eq!(r, Some(Wire::Ack(u64::from(i) + 1)));
        }
        let pull = |s: &RelayStore, epoch, since| {
            s.handle(Wire::Pull {
                folder: TAG.into(),
                epoch,
                since,
                limit: 10,
            })
            .unwrap()
        };
        let Some(Wire::Blobs { epoch, head, blobs }) = pull(&s, 0, 0) else {
            panic!()
        };
        assert_eq!((head, blobs.len()), (3, 3));
        let Some(Wire::Blobs { head, blobs, .. }) = pull(&s, epoch, 2) else {
            panic!()
        };
        assert_eq!((head, blobs), (3, vec![vec![2; 5]]));

        drop(s);
        let log = d.join(TAG).join("log");
        let mut f = OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(&[9, 0, 0, 0, 1]).unwrap(); // a record cut short
        let s = RelayStore::open(&d).unwrap();
        let Some(Wire::Blobs {
            epoch: e2, head, ..
        }) = pull(&s, epoch, 0)
        else {
            panic!()
        };
        assert_eq!((e2, head), (epoch, 3), "same epoch, torn record dropped");
        assert_eq!(
            s.handle(Wire::Put {
                folder: TAG.into(),
                blob: vec![4]
            })
            .unwrap(),
            Some(Wire::Ack(4))
        );
    }

    fn has(s: &RelayStore, ids: Vec<ChunkId>) -> Option<Wire> {
        s.handle(Wire::Has {
            folder: TAG.into(),
            ids,
        })
        .unwrap()
    }

    fn age(s: &RelayStore, id: &ChunkId, secs: u64) {
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
        let f = File::open(s.chunk_path(TAG, id).unwrap()).unwrap();
        f.set_modified(t).unwrap();
    }

    #[test]
    fn chunks_and_bad_tags() {
        let s = RelayStore::open(&dir("chunks")).unwrap();
        let id = ChunkId([5; 32]);
        assert_eq!(has(&s, vec![id]), Some(Wire::Missing(vec![id])));
        s.handle(Wire::Stash {
            folder: TAG.into(),
            id,
            bytes: b"sealed".to_vec(),
        })
        .unwrap();
        assert_eq!(has(&s, vec![id]), Some(Wire::Missing(vec![])));
        assert_eq!(
            s.handle(Wire::Fetch {
                folder: TAG.into(),
                id
            })
            .unwrap(),
            Some(Wire::Chunk(id, b"sealed".to_vec()))
        );
        let other = "ffeeddccbbaa99887766554433221100";
        assert_eq!(
            s.handle(Wire::Fetch {
                folder: other.into(),
                id
            })
            .unwrap(),
            None,
            "chunks are per folder"
        );
        assert!(
            s.handle(Wire::Fetch {
                folder: "../etc".into(),
                id
            })
            .is_err()
        );
        assert!(
            s.handle(Wire::Put {
                folder: "../etc".into(),
                blob: vec![]
            })
            .is_err()
        );
    }

    #[test]
    fn sweep_drops_only_chunks_nobody_named() {
        let s = RelayStore::open(&dir("sweep")).unwrap();
        let (kept, dropped, fresh) = (ChunkId([1; 32]), ChunkId([2; 32]), ChunkId([3; 32]));
        for id in [kept, dropped, fresh] {
            s.handle(Wire::Stash {
                folder: TAG.into(),
                id,
                bytes: vec![0; 10],
            })
            .unwrap();
        }
        age(&s, &kept, 3600);
        age(&s, &dropped, 3600);
        has(&s, vec![kept]); // a device still uses it
        let sweep = |s: &RelayStore| {
            s.handle(Wire::Sweep {
                folder: TAG.into(),
                older_than_s: 600,
            })
            .unwrap()
        };
        assert_eq!(sweep(&s), Some(Wire::Ack(1)));
        assert_eq!(
            has(&s, vec![kept, dropped, fresh]),
            Some(Wire::Missing(vec![dropped]))
        );
        // storing a chunk that is already there marks it too
        age(&s, &fresh, 3600);
        s.handle(Wire::Stash {
            folder: TAG.into(),
            id: fresh,
            bytes: vec![0; 10],
        })
        .unwrap();
        assert_eq!(sweep(&s), Some(Wire::Ack(0)));
    }

    #[test]
    fn reset_starts_a_new_epoch() {
        let d = dir("reset");
        let s = RelayStore::open(&d).unwrap();
        let put = |s: &RelayStore| {
            s.handle(Wire::Put {
                folder: TAG.into(),
                blob: vec![1; 3],
            })
            .unwrap()
        };
        put(&s);
        put(&s);
        let Some(Wire::Ack(epoch)) = s.handle(Wire::Reset { folder: TAG.into() }).unwrap() else {
            panic!()
        };
        assert_eq!(put(&s), Some(Wire::Ack(1)));
        drop(s);
        let s = RelayStore::open(&d).unwrap();
        let Some(Wire::Blobs { epoch: e, head, .. }) = s
            .handle(Wire::Pull {
                folder: TAG.into(),
                epoch: 0,
                since: 0,
                limit: 10,
            })
            .unwrap()
        else {
            panic!()
        };
        assert_eq!((e, head), (epoch, 1));
    }
}
