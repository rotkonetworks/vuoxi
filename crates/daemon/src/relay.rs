//! A relay: stores folders it cannot read and serves them to the devices that can.
//!
//! Per folder (named by an opaque tag, see [`crate::seal`]):
//!
//! ```text
//! <dir>/<tag>/epoch           random u64, fixed when the folder first appears
//! <dir>/<tag>/log             records: len:u32 LE ‖ sealed manifest
//! <dir>/<tag>/chunks/<hex>    sealed chunk, named by its relay id
//! ```
//!
//! The log is append-only and fsynced per `Put`; a torn tail is cut on open.
//! Devices pull it with the same cursor-and-epoch rules as from a peer.

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

impl Log {
    fn open(dir: &Path) -> io::Result<Log> {
        fs::create_dir_all(dir.join("chunks"))?;
        let ep = dir.join("epoch");
        let epoch = match fs::read(&ep) {
            Ok(b) if b.len() == 8 => u64::from_le_bytes(b.try_into().unwrap()),
            _ => {
                let mut b = [0u8; 8];
                File::open("/dev/urandom")?.read_exact(&mut b)?;
                fs::write(&ep, b)?;
                u64::from_le_bytes(b)
            }
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

    /// Chunks are stored for every folder in one place: the relay id is keyed
    /// per folder already, and one directory keeps lookups simple.
    fn chunk_path(&self, id: &ChunkId) -> PathBuf {
        let h = hex(id);
        self.dir.join("chunks").join(&h[..2]).join(h)
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
            Wire::Has(ids) => Some(Wire::Missing(
                ids.into_iter()
                    .filter(|id| !self.chunk_path(id).is_file())
                    .collect(),
            )),
            Wire::Chunk(id, bytes) if bytes.len() <= MAX_BLOB => {
                let p = self.chunk_path(&id);
                if !p.is_file() {
                    fs::create_dir_all(p.parent().unwrap())?;
                    let tmp = p.with_extension("tmp");
                    let mut f = File::create(&tmp)?;
                    f.write_all(&bytes)?;
                    f.sync_data()?;
                    fs::rename(tmp, &p)?;
                }
                Some(Wire::Ack(0))
            }
            Wire::Want(id) => fs::read(self.chunk_path(&id))
                .ok()
                .map(|b| Wire::Chunk(id, b)),
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

    #[test]
    fn chunks_and_bad_tags() {
        let s = RelayStore::open(&dir("chunks")).unwrap();
        let id = ChunkId([5; 32]);
        assert_eq!(
            s.handle(Wire::Has(vec![id])).unwrap(),
            Some(Wire::Missing(vec![id]))
        );
        s.handle(Wire::Chunk(id, b"sealed".to_vec())).unwrap();
        assert_eq!(
            s.handle(Wire::Has(vec![id])).unwrap(),
            Some(Wire::Missing(vec![]))
        );
        assert_eq!(
            s.handle(Wire::Want(id)).unwrap(),
            Some(Wire::Chunk(id, b"sealed".to_vec()))
        );
        assert!(
            s.handle(Wire::Put {
                folder: "../etc".into(),
                blob: vec![]
            })
            .is_err()
        );
    }
}
