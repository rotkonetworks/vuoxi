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
use crate::seal::Seal;
use std::collections::{HashMap, HashSet};
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
use vuoxi_core::{ChunkId, Error, Manifest, PeerId, RelPath, Wire};
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
    /// Folder key; without one the folder is never sent to relays.
    pub key: Option<[u8; 32]>,
}

pub struct PeerSpec {
    pub id: PeerId,
    pub addrs: Vec<SocketAddr>,
    /// Folder ids this peer shares with us.
    pub folders: Vec<String>,
    /// Of `folders`, those where this peer only receives: its offers are
    /// ignored and we never pull from it.
    pub read_only: Vec<String>,
    /// A relay: it gets our folders sealed (see [`crate::seal`]) and we pull
    /// sealed manifests and chunks back from it.
    pub relay: bool,
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
    /// How long files a peer replaced or deleted stay in `.vuoxi-trash`.
    pub trash_keep: Duration,
}

struct Shared {
    folder: Mutex<Folder>,
    ops: AsyncMutex<()>,
}

struct Inner {
    me: PeerId,
    folders: HashMap<String, Arc<Shared>>,
    /// Which folders each peer may read.
    peers: HashMap<PeerId, Vec<String>>,
    /// Which of those it may also change here.
    writers: HashMap<PeerId, Vec<String>>,
    transport: IrohTransport,
    mode: ModeHandle,
    state_dir: PathBuf,
    wake: Notify,
    relays: HashSet<PeerId>,
    seals: HashMap<String, Seal>,
    /// When each peer last answered or asked us something.
    last_seen: Mutex<HashMap<PeerId, std::time::Instant>>,
    transfers: Mutex<Transfers>,
    /// Local changes to push to relays.
    relay_wake: Notify,
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
        let mut seals = HashMap::new();
        for f in cfg.folders {
            if let Some(k) = &f.key {
                seals.insert(f.id.clone(), Seal::new(&f.id, k));
            }
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
        let writers = cfg
            .peers
            .iter()
            .map(|p| {
                let w = p.folders.iter().filter(|f| !p.read_only.contains(f));
                (p.id, w.cloned().collect())
            })
            .collect();
        let mode = ModeHandle::new(match &cfg.power_root {
            Some(r) => PowerState::read(r).mode(),
            None => Mode::Live,
        });
        let inner = Arc::new(Inner {
            me,
            folders,
            peers,
            writers,
            relays: cfg.peers.iter().filter(|p| p.relay).map(|p| p.id).collect(),
            last_seen: Mutex::new(HashMap::new()),
            transfers: Mutex::new(Transfers::default()),
            seals,
            relay_wake: Notify::new(),
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

        for (peer, fs) in &inner.writers {
            for f in fs.iter().filter(|f| inner.folders.contains_key(*f)) {
                let (i, peer, f) = (inner.clone(), *peer, f.clone());
                let every = cfg.pull_every;
                if inner.relays.contains(&peer) {
                    if inner.seals.contains_key(&f) {
                        tasks.push(tokio::spawn(
                            async move { i.relay_loop(peer, f, every).await },
                        ));
                    }
                } else {
                    tasks.push(tokio::spawn(
                        async move { i.pull_loop(peer, f, every).await },
                    ));
                }
            }
        }

        let roots: Vec<PathBuf> = inner
            .folders
            .values()
            .map(|s| s.folder.lock().unwrap().root.clone())
            .collect();
        let keep = cfg.trash_keep;
        tasks.push(tokio::spawn(async move {
            loop {
                for r in &roots {
                    let r = r.clone();
                    let _ = tokio::task::spawn_blocking(move || vfs::prune_trash(&r, keep)).await;
                }
                tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
            }
        }));

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

    pub fn folder_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self.inner.folders.keys().cloned().collect();
        v.sort();
        v
    }

    /// Seconds since each known peer was last heard from (`None`: never).
    pub fn last_seen(&self) -> HashMap<PeerId, Option<Duration>> {
        let seen = self.inner.last_seen.lock().unwrap();
        self.inner
            .peers
            .keys()
            .map(|p| (*p, seen.get(p).map(|t| t.elapsed())))
            .collect()
    }

    pub fn is_relay(&self, p: &PeerId) -> bool {
        self.inner.relays.contains(p)
    }

    pub fn transfers(&self) -> Vec<Transfer> {
        self.inner.transfers.lock().unwrap().snapshot()
    }

    /// Process one event and wait for it; detectors use [`Node::events`].
    pub async fn apply_event(&self, folder: &str, ev: Event) -> io::Result<()> {
        self.inner.local_event(folder, ev).await
    }

    pub async fn shutdown(&self) {
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

    /// Devices (not relays) that share `folder`.
    fn peers_of(&self, folder: &str) -> Vec<PeerId> {
        self.peers
            .iter()
            .filter(|(p, _)| !self.relays.contains(p))
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
            s.folder.lock().unwrap().touched(&rel);
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
        self.relay_wake.notify_waiters();
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
                    vfs::to_trash(&root, &m.rel)?;
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
        let folder = s.folder.lock().unwrap().id.clone();
        let t = self
            .transfers
            .lock()
            .unwrap()
            .start(&folder, &m.rel, peer, m.meta.size);
        let res = self
            .assemble(peer, s, &folder, m, &mut staged, &mut offsets, t)
            .await;
        let res = res.and_then(|()| {
            if offsets.iter().map(|o| o.2 as u64).sum::<u64>() != m.meta.size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "assembled size differs from manifest",
                ));
            }
            vfs::to_trash(root, &m.rel)
        });
        if let Err(e) = res {
            staged.abort();
            self.transfers
                .lock()
                .unwrap()
                .finish(t, Some(e.to_string()));
            return Err(e);
        }
        staged.commit(m.meta)?;
        self.transfers.lock().unwrap().finish(t, None);
        Ok(offsets)
    }

    #[allow(clippy::too_many_arguments)]
    async fn assemble(
        &self,
        peer: PeerId,
        s: &Shared,
        folder: &str,
        m: &Manifest,
        staged: &mut Staged,
        offsets: &mut Vec<ChunkEntry>,
        t: u64,
    ) -> io::Result<()> {
        let mut at = 0u64;
        for &id in &m.chunks {
            let local = s.folder.lock().unwrap().read_chunk(id);
            let bytes = match local {
                Some(b) => b,
                None => self.fetch_chunk(peer, folder, id).await?,
            };
            staged.write(&bytes)?;
            offsets.push((id, at, bytes.len()));
            at += bytes.len() as u64;
            self.transfers.lock().unwrap().progress(t, at);
        }
        Ok(())
    }

    fn seen(&self, peer: PeerId) {
        self.last_seen
            .lock()
            .unwrap()
            .insert(peer, std::time::Instant::now());
    }

    /// One chunk from a peer, verified; from a relay, unsealed and verified.
    async fn fetch_chunk(&self, peer: PeerId, folder: &str, id: ChunkId) -> io::Result<Vec<u8>> {
        let seal = self
            .relays
            .contains(&peer)
            .then(|| self.seals.get(folder))
            .flatten();
        let ask = seal.map_or(id, |s| s.relay_id(id));
        let reply = self
            .transport
            .clone()
            .call((peer, Wire::Want(ask)))
            .await
            .map_err(|e| io::Error::other(e.to_string()))?;
        let bad = || io::Error::other("peer did not supply a valid chunk");
        match (reply, seal) {
            (Some(Wire::Chunk(got, b)), None) if got == id => {
                vuoxi_chunk::verify(id, &b).map_err(|_| bad())?;
                Ok(b)
            }
            (Some(Wire::Chunk(got, b)), Some(s)) if got == ask => {
                s.open_chunk(id, &b).map_err(|_| bad())
            }
            _ => Err(bad()),
        }
    }

    async fn relay_loop(&self, relay: PeerId, folder: String, every: Duration) {
        let mut failures = 0u32;
        loop {
            if self.mode.get() != Mode::Hold {
                let ok = self.relay_push(relay, &folder).await.is_ok()
                    & self.relay_pull(relay, &folder).await.is_ok();
                if ok {
                    self.seen(relay);
                }
                failures = if ok { 0 } else { failures + 1 };
            }
            let wait = retry_wait(self.interval(every), failures);
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.wake.notified() => {}
                _ = self.relay_wake.notified() => {}
            }
        }
    }

    fn push_cursor_path(&self, relay: PeerId, folder: &str) -> PathBuf {
        let hex: String = relay.0.iter().map(|b| format!("{b:02x}")).collect();
        self.state_dir.join(folder).join(format!("pushed-{hex}"))
    }

    /// Send what our journal recorded since the last push: missing chunks
    /// first, then the sealed manifest, so a reader never sees a manifest
    /// whose chunks are not there.
    async fn relay_push(&self, relay: PeerId, folder: &str) -> io::Result<()> {
        let (Some(seal), Ok(s)) = (self.seals.get(folder), self.shared(folder)) else {
            return Ok(());
        };
        let cpath = self.push_cursor_path(relay, folder);
        let mut cur: u64 = std::fs::read_to_string(&cpath)
            .ok()
            .and_then(|x| x.trim().parse().ok())
            .unwrap_or(0);
        let call = |w: Wire| {
            let mut t = self.transport.clone();
            async move {
                t.call((relay, w))
                    .await
                    .map_err(|e| io::Error::other(e.to_string()))
            }
        };
        loop {
            let entries = s.folder.lock().unwrap().since(cur, PULL_LIMIT as usize)?;
            let Some(&(last, _)) = entries.last() else {
                return Ok(());
            };
            let mut seen = HashSet::new();
            for (_, p) in entries {
                if !seen.insert(p) {
                    continue;
                }
                let Some(m) = s.folder.lock().unwrap().manifest(p) else {
                    continue;
                };
                let rids: Vec<ChunkId> = m.chunks.iter().map(|c| seal.relay_id(*c)).collect();
                let missing = match call(Wire::Has(rids.clone())).await? {
                    Some(Wire::Missing(ids)) => ids,
                    _ => return Err(io::Error::other("relay did not answer Has")),
                };
                let mut complete = true;
                for (plain, rid) in m.chunks.iter().zip(&rids) {
                    if !missing.contains(rid) {
                        continue;
                    }
                    // The file may have changed since: a newer journal entry covers it.
                    let Some(b) = s.folder.lock().unwrap().read_chunk(*plain) else {
                        complete = false;
                        break;
                    };
                    if !matches!(
                        call(Wire::Chunk(*rid, seal.seal_chunk(*plain, &b))).await?,
                        Some(Wire::Ack(_))
                    ) {
                        return Err(io::Error::other("relay refused a chunk"));
                    }
                }
                if complete {
                    let put = Wire::Put {
                        folder: seal.folder_tag().to_owned(),
                        blob: seal.seal_manifest(&m),
                    };
                    if !matches!(call(put).await?, Some(Wire::Ack(_))) {
                        return Err(io::Error::other("relay refused a manifest"));
                    }
                }
            }
            cur = last;
            let tmp = cpath.with_extension("tmp");
            std::fs::write(&tmp, format!("{cur}\n"))?;
            std::fs::rename(&tmp, &cpath)?;
        }
    }

    /// Like [`Inner::pull_once`], from a relay's sealed log.
    async fn relay_pull(&self, relay: PeerId, folder: &str) -> io::Result<()> {
        let Some(seal) = self.seals.get(folder) else {
            return Ok(());
        };
        let mut cur = self.load_cursor(relay, folder);
        loop {
            let req = Wire::Pull {
                folder: seal.folder_tag().to_owned(),
                epoch: cur.0,
                since: cur.1,
                limit: PULL_LIMIT,
            };
            let reply = self
                .transport
                .clone()
                .call((relay, req))
                .await
                .map_err(|e| io::Error::other(e.to_string()))?;
            let Some(Wire::Blobs { epoch, head, blobs }) = reply else {
                return Ok(());
            };
            if epoch != cur.0 {
                cur = (epoch, 0);
            }
            let more = head > cur.1 && !blobs.is_empty();
            for b in blobs {
                // A blob that does not open is not ours to apply; skip it.
                let Ok(m) = seal.open_manifest(&b) else {
                    continue;
                };
                if !self.apply_remote(relay, folder, m).await? {
                    return Ok(());
                }
            }
            cur = (epoch, head);
            self.save_cursor(relay, folder, cur)?;
            if !more {
                return Ok(());
            }
        }
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

    fn interval(&self, every: Duration) -> Duration {
        match self.mode.get() {
            Mode::Live => every,
            Mode::Batched => every * 10,
            Mode::Frugal | Mode::Hold => every * 30,
        }
    }

    async fn pull_loop(&self, peer: PeerId, folder: String, every: Duration) {
        let mut failures = 0u32;
        loop {
            if self.mode.get() != Mode::Hold {
                // An offline peer is normal; retry sooner at first, then settle.
                failures = match self.pull_once(peer, &folder).await {
                    Ok(()) => {
                        self.seen(peer);
                        0
                    }
                    Err(_) => failures + 1,
                };
            }
            let wait = retry_wait(self.interval(every), failures);
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
        self.seen(peer);
        let writes = |f: &str| {
            self.writers
                .get(&peer)
                .is_some_and(|fs| fs.iter().any(|x| x == f))
        };
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
                    .find(|(id, s)| writes(id) && s.folder.lock().unwrap().owns(&m))
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

/// One file being written from a peer, for the dashboard.
#[derive(Clone, Debug)]
pub struct Transfer {
    pub id: u64,
    pub folder: String,
    pub rel: String,
    pub peer: PeerId,
    pub size: u64,
    pub done: u64,
    pub started: std::time::Instant,
    pub finished: Option<std::time::Instant>,
    pub error: Option<String>,
}

/// Active transfers plus the most recent finished ones.
#[derive(Default)]
struct Transfers {
    next: u64,
    items: std::collections::VecDeque<Transfer>,
}

const KEEP_FINISHED: usize = 200;

impl Transfers {
    fn start(&mut self, folder: &str, rel: &RelPath, peer: PeerId, size: u64) -> u64 {
        self.next += 1;
        self.items.push_front(Transfer {
            id: self.next,
            folder: folder.to_owned(),
            rel: rel.as_str().to_owned(),
            peer,
            size,
            done: 0,
            started: std::time::Instant::now(),
            finished: None,
            error: None,
        });
        while self.items.len() > KEEP_FINISHED
            && self.items.back().is_some_and(|t| t.finished.is_some())
        {
            self.items.pop_back();
        }
        self.next
    }

    fn progress(&mut self, id: u64, done: u64) {
        if let Some(t) = self.items.iter_mut().find(|t| t.id == id) {
            t.done = done;
        }
    }

    fn finish(&mut self, id: u64, error: Option<String>) {
        if let Some(t) = self.items.iter_mut().find(|t| t.id == id) {
            t.finished = Some(std::time::Instant::now());
            if error.is_none() {
                t.done = t.size;
            }
            t.error = error;
        }
    }

    fn snapshot(&self) -> Vec<Transfer> {
        let mut v: Vec<Transfer> = self.items.iter().cloned().collect();
        v.sort_by_key(|t| (t.finished.is_some(), std::cmp::Reverse(t.id)));
        v
    }
}

/// After `failures` failed rounds in a row: 2 s, 4 s, 8 s … capped at the
/// normal interval, so a peer that just came up is found quickly and an
/// absent one costs no more than usual.
fn retry_wait(normal: Duration, failures: u32) -> Duration {
    if failures == 0 {
        return normal;
    }
    normal.min(Duration::from_secs(1 << failures.min(10)))
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
