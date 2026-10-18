//! What the daemon does to the disk: read a path into a manifest candidate,
//! write a file atomically with its metadata, walk a folder.

use std::fs::{self, File, FileTimes, Permissions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use vuoxi_chunk::ChunkEntry;
use vuoxi_core::{Meta, RelPath};

/// Our in-flight writes: `.<name>.vuoxi-tmp` beside the target, never synced.
pub const TMP_SUFFIX: &str = ".vuoxi-tmp";

/// Where files replaced or deleted by a peer go, under the folder root:
/// `.vuoxi-trash/<unix seconds>/<rel>`. Same filesystem, so a rename; never synced.
pub const TRASH_DIR: &str = ".vuoxi-trash";

/// Is `rel` the trash directory itself (it only ever lives at the root)?
pub fn is_trash_dir(rel: &RelPath) -> bool {
    rel.as_str() == TRASH_DIR
}

/// Move `rel` into the trash before a peer's change replaces or deletes it.
/// Nothing to do if it is not there.
pub fn to_trash(root: &Path, rel: &RelPath) -> io::Result<()> {
    let src = root.join(rel.as_str());
    if !fs::symlink_metadata(&src).is_ok_and(|m| m.is_file()) {
        return Ok(());
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let dest = root
        .join(TRASH_DIR)
        .join(stamp.to_string())
        .join(rel.as_str());
    fs::create_dir_all(dest.parent().expect("under the stamp dir"))?;
    // A second replacement of the same path within one second keeps both.
    let mut d = dest.clone();
    let mut n = 1;
    while d.exists() {
        d = dest.with_file_name(format!("{}.{n}", rel.file_name()));
        n += 1;
    }
    fs::rename(src, d)
}

/// Drop trash older than `keep`. Returns how many stamp directories went.
pub fn prune_trash(root: &Path, keep: Duration) -> io::Result<usize> {
    let dir = root.join(TRASH_DIR);
    let cutoff = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        .saturating_sub(keep.as_secs());
    let mut gone = 0;
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    for e in entries.flatten() {
        let old = e
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u64>().ok())
            .is_some_and(|t| t < cutoff);
        if old {
            fs::remove_dir_all(e.path())?;
            gone += 1;
        }
    }
    Ok(gone)
}

pub fn is_tmp(rel: &RelPath) -> bool {
    rel.file_name().ends_with(TMP_SUFFIX)
}

pub fn meta_of(md: &fs::Metadata) -> Meta {
    Meta {
        size: md.len(),
        mode: md.mode() & 0o7777,
        mtime_ns: md.mtime() * 1_000_000_000 + md.mtime_nsec(),
        deleted: false,
    }
}

/// What a path holds right now.
#[derive(Debug)]
pub enum Scan {
    Missing,
    Dir,
    /// Symlinks, devices, sockets: not synced.
    Other,
    /// Size, mode and mtime match what the index already has: not re-read.
    Unchanged,
    File {
        meta: Meta,
        chunks: Vec<ChunkEntry>,
    },
}

/// Read `rel`. `known` is the index's metadata; a match skips hashing, which is
/// what keeps restarts and our own writes cheap.
pub fn scan(root: &Path, rel: &RelPath, known: Option<Meta>) -> io::Result<Scan> {
    let p = root.join(rel.as_str());
    let md = match fs::symlink_metadata(&p) {
        Ok(md) => md,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Scan::Missing),
        Err(e) => return Err(e),
    };
    if md.is_dir() {
        return Ok(Scan::Dir);
    }
    if !md.is_file() {
        return Ok(Scan::Other);
    }
    let meta = meta_of(&md);
    if known == Some(meta) {
        return Ok(Scan::Unchanged);
    }
    // Re-stat after reading: a file still being written is retried later.
    for _ in 0..3 {
        let chunks = vuoxi_chunk::chunk_reader(File::open(&p)?).map_err(|e| match e {
            vuoxi_core::Error::Io(e) => e,
            e => io::Error::other(e.to_string()),
        })?;
        let after = meta_of(&fs::symlink_metadata(&p)?);
        if after == meta {
            return Ok(Scan::File { meta, chunks });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "file kept changing while read",
    ))
}

/// Bytes of one chunk of a file on disk.
pub fn read_range(root: &Path, rel: &RelPath, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut f = File::open(root.join(rel.as_str()))?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

fn system_time(ns: i64) -> SystemTime {
    if ns >= 0 {
        UNIX_EPOCH + Duration::from_nanos(ns as u64)
    } else {
        UNIX_EPOCH - Duration::from_nanos(ns.unsigned_abs())
    }
}

/// A file being assembled beside its destination; [`Staged::commit`] makes it
/// appear whole or not at all.
pub struct Staged {
    tmp: PathBuf,
    dest: PathBuf,
    file: File,
}

impl Staged {
    pub fn new(root: &Path, rel: &RelPath) -> io::Result<Staged> {
        let dest = root.join(rel.as_str());
        let dir = dest.parent().expect("rel paths are under root");
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".{}{TMP_SUFFIX}", rel.file_name()));
        let file = File::create(&tmp)?;
        Ok(Staged { tmp, dest, file })
    }

    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)
    }

    pub fn commit(self, meta: Meta) -> io::Result<()> {
        self.file.sync_all()?;
        fs::set_permissions(&self.tmp, Permissions::from_mode(meta.mode))?;
        self.file
            .set_times(FileTimes::new().set_modified(system_time(meta.mtime_ns)))?;
        fs::rename(&self.tmp, &self.dest)
    }

    pub fn abort(self) {
        let _ = fs::remove_file(&self.tmp);
    }
}

/// Remove a file and any parent directories left empty, up to the root.
pub fn remove(root: &Path, rel: &RelPath) -> io::Result<()> {
    match fs::remove_file(root.join(rel.as_str())) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut dir = rel.parent();
    while let Some(d) = dir {
        if fs::remove_dir(root.join(d.as_str())).is_err() {
            break;
        }
        dir = d.parent();
    }
    Ok(())
}

/// Every regular file under `root` (or under `start`), skipping directories
/// the filter rejects. Non-UTF-8 names are skipped: they cannot be named on the wire.
pub fn walk(
    root: &Path,
    start: Option<&RelPath>,
    skip_dir: &mut dyn FnMut(&RelPath) -> bool,
    out: &mut Vec<RelPath>,
) -> io::Result<()> {
    let mut stack = vec![start.cloned()];
    while let Some(dir) = stack.pop() {
        let abs = dir
            .as_ref()
            .map_or(root.to_path_buf(), |d| root.join(d.as_str()));
        let entries = match fs::read_dir(&abs) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for e in entries {
            let e = e?;
            let Some(name) = e.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let rel = match &dir {
                Some(d) => d.join(&name),
                None => RelPath::new(&name),
            };
            let Ok(rel) = rel else { continue };
            let ft = e.file_type()?;
            if ft.is_dir() {
                if !skip_dir(&rel) {
                    stack.push(Some(rel));
                }
            } else if ft.is_file() {
                out.push(rel);
            }
        }
    }
    Ok(())
}
