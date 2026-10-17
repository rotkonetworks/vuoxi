//! The simulated world: replicas, links, and an engine per replica that
//! speaks the real `vuoxi_core::Request`/`Response` vocabulary, with the real
//! `ModeGate` in front. Later the real store and transports replace `Replica`
//! and `Net::pull`; the scenarios stay.

use crate::replica::Replica;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::future::{Future, Ready, ready};
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use vuoxi_core::{Causality, Error, Manifest, PathId, PeerId, Request, Response, Service};
use vuoxi_layers::{Mode, ModeGate, ModeGateLayer, ModeHandle};

/// Counters a scenario can assert on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub edits: u64,
    /// Requests the gate answered `Deferred`.
    pub deferred: u64,
    /// Edits merged into an already-parked change to the same path.
    pub coalesced: u64,
    /// Manifests sent in answer to pulls (what cursors are meant to minimise).
    pub transferred: u64,
    pub conflicts_reported: u64,
    pub crashes: u64,
    pub mid_transfer_crashes: u64,
    pub pulls_failed: u64,
}

/// Everything shared by the engines. Never exposed mutably across a call.
pub struct Net {
    pub replicas: Vec<Replica>,
    pub modes: Vec<ModeHandle>,
    down: BTreeSet<(usize, usize)>,
    /// Changes the gate deferred; durable (they live in the journal in the
    /// real system) and replayed when the mode allows.
    // core: needs a contract for who replays `Response::Deferred`.
    pub parked: Vec<BTreeSet<PathId>>,
    /// Oracle: every version ever created by a local edit.
    pub ledger: Vec<Manifest>,
    pub stats: Stats,
    /// Armed for the next pull: crash the puller after applying this many manifests.
    pub crash_after: Option<usize>,
}

fn link(a: usize, b: usize) -> (usize, usize) {
    (a.min(b), a.max(b))
}

impl Net {
    pub fn linked(&self, a: usize, b: usize) -> bool {
        !self.down.contains(&link(a, b))
    }

    fn index_of(&self, id: PeerId) -> Option<usize> {
        self.replicas.iter().position(|r| r.id() == id)
    }

    fn offer(&mut self, me: usize, from: PeerId, m: Manifest) -> Response {
        let r = self.replicas[me].offer(from, m);
        if matches!(r, Response::Conflict { .. }) {
            self.stats.conflicts_reported += 1;
        }
        r
    }

    /// `me` pulls from `peer`: ask for entries after my cursor, apply them,
    /// and only then move the cursor.
    fn pull(&mut self, me: usize, peer: PeerId) -> Result<Response, Error> {
        let j = self
            .index_of(peer)
            .ok_or_else(|| Error::Transport("unknown peer".into()))?;
        if !self.linked(me, j) {
            return Err(Error::Transport("link down".into()));
        }
        if self.modes[j].get() == Mode::Hold {
            return Err(Error::Transport("peer is on hold and sends nothing".into()));
        }
        let my_id = self.replicas[me].id();
        let cursor = self.replicas[me].cursor(peer);
        let changes = self.replicas[j].changes_since(my_id, cursor);
        self.stats.transferred += changes.manifests.len() as u64;
        let crash_after = self.crash_after.take();
        for (n, m) in changes.manifests.into_iter().enumerate() {
            if crash_after == Some(n) {
                self.replicas[me].crash();
                self.stats.crashes += 1;
                self.stats.mid_transfer_crashes += 1;
                return Err(Error::Transport("crashed mid-transfer".into()));
            }
            self.offer(me, peer, m);
        }
        self.replicas[me].set_cursor(peer, changes.head);
        Ok(Response::Done)
    }
}

/// The engine of one replica: a `Service<Request>` over the shared `Net`.
pub struct Engine {
    net: Rc<RefCell<Net>>,
    me: usize,
}

impl Service<Request> for Engine {
    type Response = Response;
    type Error = Error;
    type Future = Ready<Result<Response, Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let mut net = self.net.borrow_mut();
        let me = self.me;
        ready(match req {
            Request::Changed(path) => {
                let m = net.replicas[me].local_edit(path);
                net.ledger.push(m.clone());
                net.stats.edits += 1;
                Ok(Response::Manifest(m))
            }
            Request::Offer(from, m) => Ok(net.offer(me, from, m)),
            // Chunk bytes are not modelled: a manifest is accepted whole.
            Request::Fetch(..) => Err(Error::Transport("chunks are not modelled".into())),
            // One direction only; a symmetric sync is a `Reconcile` on each side.
            // core: `Request::Reconcile` docs say "until both agree", which is two pulls.
            Request::Reconcile(peer) => net.pull(me, peer),
        })
    }
}

type Gated = ModeGate<Engine>;

/// Drive a future that must already be ready (every model service is).
fn poll_ready_now<F: Future>(f: F) -> F::Output {
    let mut cx = Context::from_waker(Waker::noop());
    match pin!(f).poll(&mut cx) {
        Poll::Ready(v) => v,
        Poll::Pending => unreachable!("model services never pend"),
    }
}

/// N replicas, each behind its own `ModeGate`, plus the links between them.
pub struct Sim {
    net: Rc<RefCell<Net>>,
    gates: Vec<Gated>,
}

pub fn peer_id(i: usize) -> PeerId {
    PeerId([i as u8 + 1; 32])
}

impl Sim {
    pub fn new(n: usize) -> Self {
        let modes: Vec<_> = (0..n).map(|_| ModeHandle::new(Mode::Live)).collect();
        let net = Rc::new(RefCell::new(Net {
            replicas: (0..n).map(|i| Replica::new(peer_id(i))).collect(),
            modes: modes.clone(),
            down: BTreeSet::new(),
            parked: vec![BTreeSet::new(); n],
            ledger: Vec::new(),
            stats: Stats::default(),
            crash_after: None,
        }));
        let gates = modes
            .into_iter()
            .enumerate()
            .map(|(me, m)| {
                use vuoxi_core::Layer;
                ModeGateLayer::new(m).layer(Engine {
                    net: net.clone(),
                    me,
                })
            })
            .collect();
        Self { net, gates }
    }

    pub fn len(&self) -> usize {
        self.gates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.gates.is_empty()
    }

    /// Read access for assertions.
    pub fn net(&self) -> std::cell::Ref<'_, Net> {
        self.net.borrow()
    }

    pub fn stats(&self) -> Stats {
        self.net().stats.clone()
    }

    /// Send a request to replica `i` through its gate.
    pub fn request(&mut self, i: usize, req: Request) -> Result<Response, Error> {
        let gate = &mut self.gates[i];
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(gate.poll_ready(&mut cx), Poll::Ready(Ok(()))));
        let r = poll_ready_now(gate.call(req));
        if matches!(r, Ok(Response::Deferred)) {
            self.net.borrow_mut().stats.deferred += 1;
        }
        r
    }

    /// A local change at `i`. If the gate defers it, it is parked and replayed
    /// when the mode allows (see [`Sim::set_mode`]).
    pub fn edit(&mut self, i: usize, path: PathId) -> Response {
        let r = self
            .request(i, Request::Changed(path))
            .expect("edits never fail");
        if r == Response::Deferred {
            let mut net = self.net.borrow_mut();
            if !net.parked[i].insert(path) {
                net.stats.coalesced += 1;
            }
        }
        r
    }

    /// `i` pulls from `j` (a `Reconcile` on `i`'s engine).
    pub fn pull(&mut self, i: usize, j: usize) -> Result<Response, Error> {
        let r = self.request(i, Request::Reconcile(peer_id(j)));
        if r.is_err() {
            self.net.borrow_mut().stats.pulls_failed += 1;
        }
        r
    }

    /// Symmetric sync: each side pulls from the other.
    pub fn sync(&mut self, a: usize, b: usize) {
        let _ = self.pull(a, b);
        let _ = self.pull(b, a);
    }

    /// Eager push: `from` offers its versions of `path` to `to`'s engine
    /// (through `to`'s gate). Best effort; cursors are the safety net.
    pub fn push(&mut self, from: usize, to: usize, path: PathId) {
        let (ok, manifests) = {
            let net = self.net.borrow();
            (
                net.linked(from, to) && net.modes[from].get() != Mode::Hold,
                net.replicas[from]
                    .heads()
                    .get(&path)
                    .cloned()
                    .unwrap_or_default(),
            )
        };
        if ok {
            for m in manifests {
                let _ = self.request(to, Request::Offer(peer_id(from), m));
            }
        }
    }

    pub fn set_link(&mut self, a: usize, b: usize, up: bool) {
        let mut net = self.net.borrow_mut();
        if up {
            net.down.remove(&link(a, b));
        } else {
            net.down.insert(link(a, b));
        }
    }

    pub fn isolate(&mut self, i: usize) {
        for j in (0..self.len()).filter(|&j| j != i) {
            self.set_link(i, j, false);
        }
    }

    pub fn heal_links(&mut self) {
        self.net.borrow_mut().down.clear();
    }

    pub fn crash(&mut self, i: usize) {
        let mut net = self.net.borrow_mut();
        net.replicas[i].crash();
        net.stats.crashes += 1;
    }

    /// `i` pulls from `j` but crashes after applying `after` manifests.
    pub fn pull_then_crash(&mut self, i: usize, j: usize, after: usize) {
        self.net.borrow_mut().crash_after = Some(after);
        let _ = self.pull(i, j);
        self.net.borrow_mut().crash_after = None;
    }

    /// Switch `i`'s power mode, then replay whatever the gate had parked.
    pub fn set_mode(&mut self, i: usize, mode: Mode) {
        self.net.borrow().modes[i].set(mode);
        self.drain(i);
    }

    /// Replay parked changes; the ones still deferred stay parked.
    pub fn drain(&mut self, i: usize) {
        let paths = std::mem::take(&mut self.net.borrow_mut().parked[i]);
        for p in paths {
            if self
                .request(i, Request::Changed(p))
                .expect("edits never fail")
                == Response::Deferred
            {
                self.net.borrow_mut().parked[i].insert(p);
            }
        }
    }

    /// Links up, every replica `Live`, parked changes replayed.
    pub fn heal(&mut self) {
        self.heal_links();
        for i in 0..self.len() {
            self.set_mode(i, Mode::Live);
        }
    }

    /// One full round: every replica pulls from every other.
    pub fn sync_round(&mut self) {
        for i in 0..self.len() {
            for j in (0..self.len()).filter(|&j| j != i) {
                let _ = self.pull(i, j);
            }
        }
    }

    /// Do all replicas hold exactly the same versions of every file?
    pub fn converged(&self) -> bool {
        let net = self.net();
        net.replicas
            .windows(2)
            .all(|w| w[0].heads() == w[1].heads())
    }

    /// Structural invariants of every replica plus cursor sanity.
    pub fn check_invariants(&self) -> Result<(), String> {
        let net = self.net();
        for (i, r) in net.replicas.iter().enumerate() {
            r.check().map_err(|e| format!("replica {i}: {e}"))?;
            for (peer, c) in r.cursors() {
                let j = net.index_of(peer).ok_or("cursor for unknown peer")?;
                if c > net.replicas[j].journal_len() {
                    return Err(format!("replica {i}: cursor {c} beyond peer {j}'s journal"));
                }
            }
        }
        Ok(())
    }

    /// Snapshot of every replica's state, to check monotonicity across an op.
    pub fn snapshot(&self) -> Vec<BTreeMap<PathId, Vec<Manifest>>> {
        self.net()
            .replicas
            .iter()
            .map(|r| r.heads().clone())
            .collect()
    }

    /// Nothing is ever rolled back: every version a replica held before is
    /// still covered (equal or superseded) after.
    pub fn check_monotonic(
        &self,
        before: &[BTreeMap<PathId, Vec<Manifest>>],
    ) -> Result<(), String> {
        let net = self.net();
        for (i, old) in before.iter().enumerate() {
            for (p, olds) in old {
                let new = net.replicas[i].heads().get(p).map_or(&[][..], |v| &v[..]);
                for o in olds {
                    let covered = new.iter().any(|n| {
                        matches!(
                            n.version.compare(&o.version),
                            Causality::Equal | Causality::Ahead
                        )
                    });
                    if !covered {
                        return Err(format!("replica {i}: {p:?} lost a version"));
                    }
                }
            }
        }
        Ok(())
    }

    /// The end-of-run oracle. The correct final state of a file is the set of
    /// *maximal* versions among everything ever edited: an edit may disappear
    /// only under a later edit that had seen it, and edits that never saw each
    /// other must both be present as a conflict. Every replica must hold
    /// exactly that.
    pub fn check_oracle(&self) -> Result<(), String> {
        let net = self.net();
        if net.parked.iter().any(|p| !p.is_empty()) {
            return Err("changes still parked after healing".into());
        }
        let mut expected: BTreeMap<PathId, Vec<Manifest>> = BTreeMap::new();
        for m in &net.ledger {
            let superseded = net
                .ledger
                .iter()
                .any(|o| o.path == m.path && m.version.compare(&o.version) == Causality::Behind);
            if !superseded {
                expected.entry(m.path).or_default().push(m.clone());
            }
        }
        for v in expected.values_mut() {
            v.sort_by(|a, b| a.chunks.cmp(&b.chunks));
        }
        for (i, r) in net.replicas.iter().enumerate() {
            if *r.heads() != expected {
                return Err(format!(
                    "replica {i} differs from the oracle:\n  has      {:?}\n  expected {:?}",
                    r.heads(),
                    expected
                ));
            }
        }
        Ok(())
    }
}
