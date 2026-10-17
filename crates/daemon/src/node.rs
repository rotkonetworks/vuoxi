//! A device: its folders, its peers, and the tasks that move manifests and
//! chunks between them.
//!
//! - **Local:** an [`Event`] for a path is scanned outside any lock and turned
//!   into a version; the new manifest is offered to the folder's peers at once.
//! - **Remote:** a peer's manifest is planned against ours, chunks are taken
//!   from our own disk when we have them and fetched otherwise, the file is
//!   written atomically, then recorded.
//! - **Catch-up:** each peer's journal is pulled from a durable cursor. The
//!   cursor only moves past manifests that were applied; a different epoch
//!   restarts it at 0.
//!
//! Work on one folder is serialised by its `ops` lock, so a local scan and a
//! remote write of the same path never interleave.

use crate::folder::{Filter, Folder, Plan};
use crate::fs::{self as vfs, Scan, Staged};
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc};
use tower_service::Service;
use vuoxi_chunk::ChunkEntry;
use vuoxi_core::{Error, Manifest, PeerId, RelPath, Wire};
use vuoxi_layers::{Mode, ModeHandle, PowerState};
use vuoxi_transport_iroh::{Allowlist, Config as TransportConfig, IrohTransport, Serve};

const PULL_LIMIT: u32 = 256;

/// A change the detector saw, relative to a folder root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Created or modified; a directory means "look inside".
    Changed(RelPath),
    Removed(RelPath),
    Moved {
        from: RelPath,
        to: RelPath,
    },
    /// Events were lost: reconcile the whole folder.
    Overflow,
}

pub struct FolderSpec {
    pub id: String,
    pub root: PathBuf,
    pub filter: Box<dyn Filter>,
}

pub struct PeerSpec {
    pub id: PeerId,
    pub addrs: Vec<SocketAddr>,
    /// Folder ids this peer shares with us.
    pub folders: Vec<String>,
}

pub struct NodeConfig {
    pub secret: [u8; 32],
    pub bind: Vec<SocketAddr>,
    pub state_dir: PathBuf,
    pub folders: Vec<FolderSpec>,
    pub peers: Vec<PeerSpec>,
    /// Catch-up pull interval in Live mode; Batched and Frugal stretch it.
    pub pull_every: Duration,
    /// Read the power supply to choose the mode; tests pin it instead.
    pub power_root: Option<PathBuf>,
}

struct Shared {
    folder: Mutex<Folder>,
    ops: AsyncMutex<()>,
}

struct Inner {
    me: PeerId,
    folders: HashMap<String, Arc<Shared>>,
    /// Which folders each peer may touch.
    peers: HashMap<PeerId, Vec<String>>,
    transport: IrohTransport,
    mode: ModeHandle,
    state_dir: PathBuf,
    wake: Notify,
}

/// A running device. Dropping it stops serving; [`Node::shutdown`] also closes connections.
pub struct Node {
    inner: Arc<Inner>,
    events: mpsc::UnboundedSender<(String, Event)>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    _serve: Serve,
}

impl Node {
    /// Open every folder, reconcile it with the disk (first import, or the gap
    /// since the last run), then start serving and pulling.
    pub async fn start(cfg: NodeConfig) -> io::Result<Node> {
        let transport_err = |e: Error| io::Error::other(e.to_string());
        let allow = Allowlist::new(cfg.peers.iter().map(|p| p.id));
        let mut tc = TransportConfig::new(cfg.secret, allow);
        tc.bind = cfg.bind.clone();
        let transport = IrohTransport::bind(tc).await.map_err(transport_err)?;
        let me = transport.peer_id();
        for p in &cfg.peers {
            transport.add_peer(p.id, p.addrs.clone());
        }

        let mut folders = HashMap::new();
        for f in cfg.folders {
            std::fs::create_dir_all(&f.root)?;
            let folder = Folder::open(&f.id, &f.root, me, &cfg.state_dir, f.filter)?;
            folders.insert(
                f.id.clone(),
                Arc::new(Shared {
                    folder: Mutex::new(folder),
                    ops: AsyncMutex::new(()),
                }),
            );
        }
        let peers = cfg
            .peers
            .iter()
            .map(|p| (p.id, p.folders.clone()))
            .collect();
        let mode = ModeHandle::new(match &cfg.power_root {
            Some(r) => PowerState::read(r).mode(),
            None => Mode::Live,
        });
        let inner = Arc::new(Inner {
            me,
            folders,
            peers,
            transport,
            mode,
            state_dir: cfg.state_dir,
            wake: Notify::new(),
        });

        for id in inner.folders.keys() {
            inner.reconcile(id, None).await?;
        }

        let serve = inner.transport.serve(Handler(inner.clone()));
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, Event)>();
        let mut tasks = Vec::new();

        let i = inner.clone();
        tasks.push(tokio::spawn(async move {
            while let Some((folder, ev)) = rx.recv().await {
                if let Err(e) = i.local_event(&folder, ev).await {
                    eprintln!("vuoxi: {folder}: {e}");
                }
            }
        }));

        for (peer, fs) in &inner.peers {
            for f in fs.iter().filter(|f| inner.folders.contains_key(*f)) {
                let (i, peer, f) = (inner.clone(), *peer, f.clone());
                let every = cfg.pull_every;
                tasks.push(tokio::spawn(
                    async move { i.pull_loop(peer, f, every).await },
                ));
            }
        }

        if let Some(root) = cfg.power_root {
            let i = inner.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    let m = PowerState::read(&root).mode();
                    if m != i.mode.get() {
                        i.mode.set(m);
                        i.wake.notify_waiters();
                    }
                }
            }));
        }

        Ok(Node {
            inner,
            events: tx,
            tasks,
            _serve: serve,
        })
    }

    pub fn peer_id(&self) -> PeerId {
        self.inner.me
    }

    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.inner.transport.local_addrs()
    }

    /// Where detectors deliver events.
    pub fn events(&self) -> mpsc::UnboundedSender<(String, Event)> {
        self.events.clone()
    }

    pub fn mode(&self) -> &ModeHandle {
        &self.inner.mode
    }

    /// Pull from every peer now instead of at the next interval.
    pub fn sync_now(&self) {
        self.inner.wake.notify_waiters();
    }

    /// Run a closure against a folder's state (status, tests).
    pub fn with_folder<R>(&self, id: &str, f: impl FnOnce(&Folder) -> R) -> Option<R> {
        let s = self.inner.folders.get(id)?;
        Some(f(&s.folder.lock().unwrap()))
    }

    /// Process one event and wait for it; detectors use [`Node::events`].
    pub async fn apply_event(&self, folder: &str, ev: Event) -> io::Result<()> {
        self.inner.local_event(folder, ev).await
    }

    pub async fn shutdown(self) {
        for t in &self.tasks {
            t.abort();
        }
        self.inner.transport.close().await;
    }
}

impl Inner {
    fn shared(&self, folder: &str) -> io::Result<&Arc<Shared>> {
        self.folders
            .get(folder)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no folder {folder}")))
    }

    fn peers_of(&self, folder: &str) -> Vec<PeerId> {
        self.peers
            .iter()
            .filter(|(_, fs)| fs.iter().any(|f| f == folder))
            .map(|(p, _)| *p)
            .collect()
    }

    /// Walk `under` (or the whole folder), version what changed, tombstone what vanished.
    async fn reconcile(&self, folder: &str, under: Option<&RelPath>) -> io::Result<()> {
        let s = self.shared(folder)?.clone();
        let _ops = s.ops.lock().await;
        let root = s.folder.lock().unwrap().root.clone();
        let mut files = Vec::new();
        {
            let mut f = s.folder.lock().unwrap();
            vfs::walk(&root, under, &mut |d| f.skip_dir(d), &mut files)?;
            files.retain(|r| !f.skip_file(r));
        }
        let mut changed = Vec::new();
        for rel in files {
            if let Some(m) = self.scan_commit(&s, &root, &rel).await? {
                changed.push(m);
            }
        }
        let gone = s.folder.lock().unwrap().missing_under(under);
        for rel in gone {
            if let Some(m) = s.folder.lock().unwrap().commit_local(&rel, Scan::Missing)? {
                changed.push(m);
            }
        }
        drop(_ops);
        self.offer(folder, changed);
        Ok(())
    }

    async fn scan_commit(
        &self,
        s: &Shared,
        root: &Path,
        rel: &RelPath,
    ) -> io::Result<Option<Manifest>> {
        let known = s.folder.lock().unwrap().known_meta(rel);
        let (root, r) = (root.to_owned(), rel.clone());
        let scan = tokio::task::spawn_blocking(move || vfs::scan(&root, &r, known))
            .await
            .map_err(io::Error::other)?;
        let scan = match scan {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            x => x?,
        };
        s.folder.lock().unwrap().commit_local(rel, scan)
    }

    async fn local_event(&self, folder: &str, ev: Event) -> io::Result<()> {
        let s = self.shared(folder)?.clone();
        let rels = match ev {
            Event::Overflow => return self.reconcile(folder, None).await,
            Event::Changed(r) | Event::Removed(r) => vec![r],
            Event::Moved { from, to } => vec![from, to],
        };
        for rel in rels {
            if s.folder.lock().unwrap().skip_file(&rel) {
                continue;
            }
            let root = s.folder.lock().unwrap().root.clone();
            let p = root.join(rel.as_str());
            let is_dir = p.is_dir();
            let was_dir = !p.exists()
                && !s
                    .folder
                    .lock()
                    .unwrap()
                    .missing_under(Some(&rel))
                    .is_empty();
            if is_dir || was_dir {
                if !s.folder.lock().unwrap().skip_dir(&rel) {
                    self.reconcile(folder, Some(&rel)).await?;
                }
                continue;
            }
            let _ops = s.ops.lock().await;
            let m = self.scan_commit(&s, &root, &rel).await?;
            drop(_ops);
            self.offer(folder, m.into_iter().collect());
        }
        Ok(())
    }

    /// Tell peers about new versions now; pulls catch up whatever is missed.
    fn offer(&self, folder: &str, ms: Vec<Manifest>) {
        if ms.is_empty() || self.mode.get() == Mode::Hold {
            return;
        }
        for peer in self.peers_of(folder) {
            let mut t = self.transport.clone();
            let ms = ms.clone();
            tokio::spawn(async move {
                for m in ms {
                    if t.call((peer, Wire::Offer(m))).await.is_err() {
                        break;
                    }
                }
            });
        }
    }

    /// Bring a peer's manifest into our folder. `Ok(false)` means "not now"
    /// (power mode): the caller must not advance its cursor.
    async fn apply_remote(&self, peer: PeerId, folder: &str, theirs: Manifest) -> io::Result<bool> {
        if self.mode.get() == Mode::Hold {
            return Ok(false);
        }
        let s = self.shared(folder)?.clone();
        if !s.folder.lock().unwrap().owns(&theirs) {
            return Ok(true);
        }
        let _ops = s.ops.lock().await;
        let root = s.folder.lock().unwrap().root.clone();
        let mut local_first = 0;
        loop {
            let plan = s.folder.lock().unwrap().plan_remote(&theirs);
            match plan {
                Plan::Nothing => return Ok(true),
                Plan::LocalFirst(rel) => {
                    local_first += 1;
                    if local_first > 2 {
                        return Ok(false);
                    }
                    let m = self.scan_commit(&s, &root, &rel).await?;
                    self.offer(folder, m.into_iter().collect());
                }
                Plan::Adopt(m) => {
                    s.folder.lock().unwrap().commit_remote(m, None)?;
                    return Ok(true);
                }
                Plan::Delete(m) => {
                    vfs::remove(&root, &m.rel)?;
                    s.folder.lock().unwrap().commit_remote(m, None)?;
                    return Ok(true);
                }
                Plan::Write(m) => {
                    let offsets = self.fetch_write(peer, &s, &root, &m).await?;
                    s.folder.lock().unwrap().commit_remote(m, Some(offsets))?;
                    return Ok(true);
                }
                Plan::Conflict {
                    main,
                    main_is_theirs,
                    copy,
                } => {
                    // The copy first: when ours lost, its chunks are still at the path.
                    let copy_offsets = self.fetch_write(peer, &s, &root, &copy).await?;
                    let main_offsets = if main_is_theirs {
                        Some(self.fetch_write(peer, &s, &root, &main).await?)
                    } else {
                        None
                    };
                    {
                        let mut f = s.folder.lock().unwrap();
                        f.commit_remote(copy.clone(), Some(copy_offsets))?;
                        f.commit_remote(main.clone(), main_offsets)?;
                    }
                    self.offer(folder, vec![main, copy]);
                    return Ok(true);
                }
            }
        }
    }

    /// Assemble `m` from our own chunks where we have them and the peer's
    /// otherwise, verifying every chunk, then move it into place.
    async fn fetch_write(
        &self,
        peer: PeerId,
        s: &Shared,
        root: &Path,
        m: &Manifest,
    ) -> io::Result<Vec<ChunkEntry>> {
        let mut staged = Staged::new(root, &m.rel)?;
        let mut offsets = Vec::with_capacity(m.chunks.len());
        let mut at = 0u64;
        for &id in &m.chunks {
            let local = s.folder.lock().unwrap().read_chunk(id);
            let bytes = match local {
                Some(b) => b,
                None => match self.transport.clone().call((peer, Wire::Want(id))).await {
                    Ok(Some(Wire::Chunk(got, b)))
                        if got == id && vuoxi_chunk::verify(id, &b).is_ok() =>
                    {
                        b
                    }
                    Ok(_) => {
                        staged.abort();
                        return Err(io::Error::other("peer did not supply a valid chunk"));
                    }
                    Err(e) => {
                        staged.abort();
                        return Err(io::Error::other(e.to_string()));
                    }
                },
            };
            if let Err(e) = staged.write(&bytes) {
                staged.abort();
                return Err(e);
            }
            offsets.push((id, at, bytes.len()));
            at += bytes.len() as u64;
        }
        if at != m.meta.size {
            staged.abort();
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "assembled size differs from manifest",
            ));
        }
        staged.commit(m.meta)?;
        Ok(offsets)
    }

    fn cursor_path(&self, peer: PeerId, folder: &str) -> PathBuf {
        let hex: String = peer.0.iter().map(|b| format!("{b:02x}")).collect();
        self.state_dir.join(folder).join(format!("cursor-{hex}"))
    }

    fn load_cursor(&self, peer: PeerId, folder: &str) -> (u64, u64) {
        std::fs::read_to_string(self.cursor_path(peer, folder))
            .ok()
            .and_then(|s| {
                let mut it = s.split_whitespace().map(|x| x.parse().ok());
                Some((it.next()??, it.next()??))
            })
            .unwrap_or((0, 0))
    }

    fn save_cursor(&self, peer: PeerId, folder: &str, c: (u64, u64)) -> io::Result<()> {
        let p = self.cursor_path(peer, folder);
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, format!("{} {}\n", c.0, c.1))?;
        std::fs::rename(tmp, p)
    }

    async fn pull_loop(&self, peer: PeerId, folder: String, every: Duration) {
        loop {
            if self.mode.get() != Mode::Hold
                && let Err(e) = self.pull_once(peer, &folder).await
            {
                let _ = e; // peer offline: try again later
            }
            let wait = match self.mode.get() {
                Mode::Live => every,
                Mode::Batched => every * 10,
                Mode::Frugal | Mode::Hold => every * 30,
            };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.wake.notified() => {}
            }
        }
    }

    async fn pull_once(&self, peer: PeerId, folder: &str) -> io::Result<()> {
        let mut cur = self.load_cursor(peer, folder);
        loop {
            let req = Wire::Pull {
                folder: folder.to_owned(),
                epoch: cur.0,
                since: cur.1,
                limit: PULL_LIMIT,
            };
            let reply = self
                .transport
                .clone()
                .call((peer, req))
                .await
                .map_err(|e| io::Error::other(e.to_string()))?;
            let Some(Wire::Batch {
                epoch,
                head,
                manifests,
            }) = reply
            else {
                return Ok(());
            };
            if epoch != cur.0 {
                // Their journal is not the one our cursor counts in: start over.
                cur = (epoch, 0);
                if head != 0 || !manifests.is_empty() {
                    // The reply already started from 0 for us; fall through and apply it.
                } else {
                    self.save_cursor(peer, folder, cur)?;
                    continue;
                }
            }
            let full = manifests.len() >= PULL_LIMIT as usize;
            for m in manifests {
                if !self.apply_remote(peer, folder, m).await? {
                    return Ok(());
                }
            }
            cur = (epoch, head);
            self.save_cursor(peer, folder, cur)?;
            if !full {
                return Ok(());
            }
        }
    }

    async fn handle(self: Arc<Self>, peer: PeerId, w: Wire) -> Option<Wire> {
        let allowed = |f: &str| {
            self.peers
                .get(&peer)
                .is_some_and(|fs| fs.iter().any(|x| x == f))
        };
        match w {
            Wire::Offer(m) => {
                let folder = self
                    .folders
                    .iter()
                    .find(|(id, s)| allowed(id) && s.folder.lock().unwrap().owns(&m))
                    .map(|(id, _)| id.clone())?;
                let me = self.clone();
                tokio::spawn(async move {
                    let _ = me.apply_remote(peer, &folder, m).await;
                });
                None
            }
            Wire::Want(id) => self
                .folders
                .iter()
                .filter(|(f, _)| allowed(f))
                .find_map(|(_, s)| s.folder.lock().unwrap().read_chunk(id))
                .map(|b| Wire::Chunk(id, b)),
            Wire::Pull {
                folder,
                epoch,
                since,
                limit,
            } if allowed(&folder) => {
                let s = self.folders.get(&folder)?;
                let limit = limit.min(PULL_LIMIT) as usize;
                s.folder.lock().unwrap().pull(epoch, since, limit).ok()
            }
            _ => None,
        }
    }
}

/// The transport's view of a node.
#[derive(Clone)]
struct Handler(Arc<Inner>);

impl Service<(PeerId, Wire)> for Handler {
    type Response = Option<Wire>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Option<Wire>, Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, (peer, w): (PeerId, Wire)) -> Self::Future {
        let inner = self.0.clone();
        Box::pin(async move { Ok(inner.handle(peer, w).await) })
    }
}
