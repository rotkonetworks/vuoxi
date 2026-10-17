//! One device's state and the pure protocol rules.
//!
//! Durable (survives a crash): `heads`, `journal`, `conflicts`.
//! Volatile (lost on a crash): `cursors`.
//!
//! The protocol in four sentences:
//! 1. Every state change a device makes (its own edit, or a version it accepted
//!    from a peer) appends `(path, origin)` to its journal; the position is the
//!    sequence number.
//! 2. A device remembers, per peer, the highest peer-journal sequence it has
//!    applied, and asks only for entries after it.
//! 3. An offered manifest is compared by version vector: it is taken if it is
//!    newer, ignored if we are equal or ahead, and kept *beside* ours if the
//!    two are concurrent (a recorded conflict, never an overwrite).
//! 4. The cursor moves only after the whole batch is applied, so a crash
//!    mid-transfer repeats work instead of skipping it.

use std::collections::{BTreeMap, BTreeSet};
use vuoxi_core::{Causality, ChunkId, Manifest, PathId, PeerId, Response, VersionVector};

/// One journal line. The sequence number is its position (1-based).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: PathId,
    /// Who supplied the change; `None` for a local edit. Entries are not
    /// served back to their origin, which would only echo its own data.
    pub origin: Option<PeerId>,
}

/// A peer's answer to "what changed after my cursor?".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changes {
    /// Journal length when answered: the requester's next cursor.
    pub head: u64,
    pub manifests: Vec<Manifest>,
}

#[derive(Clone, Debug)]
pub struct Replica {
    id: PeerId,
    /// Per path, the mutually concurrent versions we hold, sorted by chunks.
    /// One entry is the normal case; more than one is a conflict. `[0]` is the
    /// deterministic "visible" version, so replicas that hold the same set
    /// show the same file.
    // TODO: sort by the `Ord` core now derives on `VersionVector`/`Manifest`.
    heads: BTreeMap<PathId, Vec<Manifest>>,
    journal: Vec<Entry>,
    conflicts: Vec<(Manifest, Manifest)>,
    cursors: BTreeMap<PeerId, u64>,
}

/// A chunk id unique to one edit. Real chunks come from BLAKE3 of content;
/// the model only needs "different edits, different bytes".
fn chunk_for(path: PathId, editor: PeerId, n: u64) -> ChunkId {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&path.0.to_le_bytes());
    b[8..16].copy_from_slice(&n.to_le_bytes());
    b[16..].copy_from_slice(&editor.0[..16]);
    ChunkId(b)
}

fn dominates(newer: &Manifest, older: &Manifest) -> bool {
    matches!(
        newer.version.compare(&older.version),
        Causality::Equal | Causality::Ahead
    )
}

impl Replica {
    pub fn new(id: PeerId) -> Self {
        Self {
            id,
            heads: BTreeMap::new(),
            journal: Vec::new(),
            conflicts: Vec::new(),
            cursors: BTreeMap::new(),
        }
    }

    pub fn id(&self) -> PeerId {
        self.id
    }

    pub fn heads(&self) -> &BTreeMap<PathId, Vec<Manifest>> {
        &self.heads
    }

    /// The version a file browser would show.
    pub fn manifest(&self, path: PathId) -> Option<&Manifest> {
        self.heads.get(&path).and_then(|h| h.first())
    }

    /// Versions kept beside the visible one.
    pub fn conflicting(&self, path: PathId) -> &[Manifest] {
        self.heads.get(&path).map_or(&[], |h| &h[1.min(h.len())..])
    }

    pub fn conflict_paths(&self) -> usize {
        self.heads.values().filter(|h| h.len() > 1).count()
    }

    /// Every conflict this device has ever detected, in order.
    pub fn conflict_log(&self) -> &[(Manifest, Manifest)] {
        &self.conflicts
    }

    pub fn journal(&self) -> &[Entry] {
        &self.journal
    }

    pub fn journal_len(&self) -> u64 {
        self.journal.len() as u64
    }

    pub fn cursor(&self, peer: PeerId) -> u64 {
        self.cursors.get(&peer).copied().unwrap_or(0)
    }

    pub fn cursors(&self) -> impl Iterator<Item = (PeerId, u64)> + '_ {
        self.cursors.iter().map(|(p, c)| (*p, *c))
    }

    pub fn set_cursor(&mut self, peer: PeerId, seq: u64) {
        self.cursors.insert(peer, seq);
    }

    /// Power loss: the journal and index are durable, the cursors are not.
    pub fn crash(&mut self) {
        self.cursors.clear();
    }

    /// A local edit. The editor has seen every version it holds, so the new
    /// version supersedes all of them (editing is also how a conflict is
    /// resolved).
    pub fn local_edit(&mut self, path: PathId) -> Manifest {
        let heads = self.heads.entry(path).or_default();
        let mut version = VersionVector::new();
        for h in heads.iter() {
            version.merge(&h.version);
        }
        version.bump(self.id);
        let m = Manifest {
            path,
            chunks: vec![chunk_for(path, self.id, version.get(&self.id))],
            version,
        };
        *heads = vec![m.clone()];
        self.journal.push(Entry { path, origin: None });
        m
    }

    /// A peer advertises its version of a file.
    pub fn offer(&mut self, from: PeerId, m: Manifest) -> Response {
        let heads = self.heads.entry(m.path).or_default();
        let mut concurrent = None;
        for h in heads.iter() {
            match h.version.compare(&m.version) {
                // Already covered by what we hold: nothing to do. (Heads are
                // an antichain, so `m` cannot also dominate another head.)
                Causality::Equal | Causality::Ahead => return Response::Done,
                Causality::Behind => {}
                Causality::Concurrent => {
                    concurrent.get_or_insert_with(|| h.clone());
                }
            }
        }
        // Take theirs: drop every head it supersedes, keep the concurrent ones.
        heads.retain(|h| !dominates(&m, h));
        heads.push(m.clone());
        heads.sort_by(|a, b| a.chunks.cmp(&b.chunks));
        self.journal.push(Entry {
            path: m.path,
            origin: Some(from),
        });
        match concurrent {
            None => Response::Done,
            Some(ours) => {
                self.conflicts.push((ours.clone(), m.clone()));
                Response::Conflict { ours, theirs: m }
            }
        }
    }

    /// Serve a peer's pull: every version of every path changed after
    /// `cursor`, except changes that came from the requester itself.
    pub fn changes_since(&self, requester: PeerId, cursor: u64) -> Changes {
        let head = self.journal_len();
        // A cursor past our journal means we lost history the peer had seen:
        // resend everything rather than panic or skip.
        // core: needs a journal epoch/id in the cursor to detect this properly.
        let from = if cursor > head { 0 } else { cursor as usize };
        let paths: BTreeSet<PathId> = self.journal[from..]
            .iter()
            .filter(|e| e.origin != Some(requester))
            .map(|e| e.path)
            .collect();
        let manifests = paths
            .iter()
            .flat_map(|p| self.heads[p].iter().cloned())
            .collect();
        Changes { head, manifests }
    }

    /// Structural invariants that must hold after every operation.
    pub fn check(&self) -> Result<(), String> {
        for (p, heads) in &self.heads {
            if heads.is_empty() {
                return Err(format!("{p:?}: empty head list"));
            }
            for (i, a) in heads.iter().enumerate() {
                if a.path != *p {
                    return Err(format!("{p:?}: manifest filed under wrong path"));
                }
                for b in &heads[i + 1..] {
                    if a.version.compare(&b.version) != Causality::Concurrent {
                        return Err(format!("{p:?}: heads are not concurrent"));
                    }
                    if a.chunks >= b.chunks {
                        return Err(format!("{p:?}: heads not sorted/unique"));
                    }
                }
            }
        }
        if let Some(e) = self
            .journal
            .iter()
            .find(|e| !self.heads.contains_key(&e.path))
        {
            return Err(format!("journal names unknown {:?}", e.path));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: PeerId = PeerId([1; 32]);
    const B: PeerId = PeerId([2; 32]);
    const P: PathId = PathId(9);

    #[test]
    fn behind_ahead_equal_concurrent() {
        let (mut a, mut b) = (Replica::new(A), Replica::new(B));
        let v1 = a.local_edit(P);

        // We are behind (empty): take theirs.
        assert_eq!(b.offer(A, v1.clone()), Response::Done);
        assert_eq!(b.manifest(P), Some(&v1));
        // Equal: nothing happens, nothing is journaled.
        let seq = b.journal_len();
        assert_eq!(b.offer(A, v1.clone()), Response::Done);
        assert_eq!(b.journal_len(), seq);

        // b edits on top of v1; a is now behind b.
        let v2 = b.local_edit(P);
        assert_eq!(a.offer(B, v2.clone()), Response::Done);
        assert_eq!(a.manifest(P), Some(&v2));
        // Offering the stale v1 to b: we are ahead, keep ours.
        assert_eq!(b.offer(A, v1), Response::Done);
        assert_eq!(b.manifest(P), Some(&v2));

        // Both edit without seeing the other: conflict, both kept.
        let a3 = a.local_edit(P);
        let b3 = b.local_edit(P);
        match b.offer(A, a3.clone()) {
            Response::Conflict { ours, theirs } => {
                assert_eq!((ours, theirs), (b3.clone(), a3.clone()));
            }
            r => panic!("expected conflict, got {r:?}"),
        }
        assert_eq!(b.heads()[&P].len(), 2);
        // Re-offering the same conflict is not a new conflict.
        assert_eq!(b.offer(A, a3), Response::Done);
        assert_eq!(b.conflict_log().len(), 1);
        b.check().unwrap();
    }

    #[test]
    fn editing_resolves_a_conflict() {
        let (mut a, mut b) = (Replica::new(A), Replica::new(B));
        let ma = a.local_edit(P);
        let mb = b.local_edit(P);
        a.offer(B, mb);
        b.offer(A, ma);
        assert_eq!(a.heads(), b.heads(), "same set, same visible version");
        let r = a.local_edit(P);
        assert_eq!(a.heads()[&P], vec![r.clone()]);
        assert_eq!(b.offer(A, r.clone()), Response::Done);
        assert_eq!(b.heads()[&P], vec![r]);
    }

    #[test]
    fn changes_skip_cursor_and_origin() {
        let (mut a, mut b) = (Replica::new(A), Replica::new(B));
        a.local_edit(PathId(1));
        a.local_edit(PathId(2));
        let first = a.changes_since(B, 0);
        assert_eq!((first.head, first.manifests.len()), (2, 2));
        assert!(a.changes_since(B, 2).manifests.is_empty());

        for m in first.manifests {
            b.offer(A, m);
        }
        // b journaled two entries from A: serving them back to A is skipped.
        assert_eq!(b.journal_len(), 2);
        assert!(b.changes_since(A, 0).manifests.is_empty());
    }
}
