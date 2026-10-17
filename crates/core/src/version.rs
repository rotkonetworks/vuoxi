use crate::PeerId;
use std::collections::BTreeMap;

/// How two versions of a file relate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Causality {
    Equal,
    /// The other side has seen everything we have, and more.
    Behind,
    Ahead,
    /// Edited on both sides without seeing each other: a real conflict.
    Concurrent,
}

/// Per-device edit counters. Sparse: a device that never edited is absent.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VersionVector(BTreeMap<PeerId, u64>);

impl VersionVector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one local edit by `peer`.
    pub fn bump(&mut self, peer: PeerId) {
        *self.0.entry(peer).or_default() += 1;
    }

    /// Non-zero counters, ascending by peer.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (PeerId, u64)> + '_ {
        self.0.iter().map(|(p, n)| (*p, *n))
    }

    pub fn get(&self, peer: &PeerId) -> u64 {
        self.0.get(peer).copied().unwrap_or(0)
    }

    /// Pointwise max: the history that includes both.
    pub fn merge(&mut self, other: &Self) {
        for (p, &n) in &other.0 {
            let e = self.0.entry(*p).or_default();
            *e = (*e).max(n);
        }
    }

    pub fn compare(&self, other: &Self) -> Causality {
        let (mut less, mut more) = (false, false);
        for p in self.0.keys().chain(other.0.keys()) {
            let (a, b) = (self.get(p), other.get(p));
            less |= a < b;
            more |= a > b;
        }
        match (less, more) {
            (false, false) => Causality::Equal,
            (true, false) => Causality::Behind,
            (false, true) => Causality::Ahead,
            (true, true) => Causality::Concurrent,
        }
    }
}

/// Rebuild from stored entries; zero counters are dropped, repeated peers keep the max.
impl FromIterator<(PeerId, u64)> for VersionVector {
    fn from_iter<I: IntoIterator<Item = (PeerId, u64)>>(it: I) -> Self {
        let mut v = Self::new();
        for (p, n) in it.into_iter().filter(|(_, n)| *n > 0) {
            let e = v.0.entry(p).or_default();
            *e = (*e).max(n);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: PeerId = PeerId([1; 32]);
    const B: PeerId = PeerId([2; 32]);

    #[test]
    fn ordering() {
        let mut a = VersionVector::new();
        let b = a.clone();
        assert_eq!(a.compare(&b), Causality::Equal);

        a.bump(A);
        assert_eq!(a.compare(&b), Causality::Ahead);
        assert_eq!(b.compare(&a), Causality::Behind);
    }

    #[test]
    fn concurrent_edits_conflict_until_merged() {
        let (mut a, mut b) = (VersionVector::new(), VersionVector::new());
        a.bump(A);
        b.bump(B);
        assert_eq!(a.compare(&b), Causality::Concurrent);

        a.merge(&b);
        assert_eq!(a.compare(&b), Causality::Ahead);
        assert_eq!(b.compare(&a), Causality::Behind);
    }

    #[test]
    fn entries_round_trip() {
        let mut a = VersionVector::new();
        a.bump(B);
        a.bump(A);
        a.bump(B);
        let es: Vec<_> = a.iter().collect();
        assert_eq!(es, vec![(A, 1), (B, 2)]);
        assert_eq!(es.into_iter().collect::<VersionVector>(), a);
        assert_eq!(
            [(A, 0)].into_iter().collect::<VersionVector>(),
            VersionVector::new()
        );
    }
}
