use std::collections::BTreeSet;
use std::time::{Duration, Instant};
use vuoxi_core::PathId;

/// Collapses bursts of changes: a file saved fifty times inside one window is
/// handed on once. Time is passed in, never read, so the simulator can drive it
/// deterministically and an idle queue arms no timer.
#[derive(Debug)]
pub struct Coalesce {
    window: Duration,
    opened: Option<Instant>,
    pending: BTreeSet<PathId>,
}

impl Coalesce {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            opened: None,
            pending: BTreeSet::new(),
        }
    }

    pub fn push(&mut self, path: PathId, now: Instant) {
        self.opened.get_or_insert(now);
        self.pending.insert(path);
    }

    /// When the caller should next call [`Coalesce::flush`]; `None` while empty,
    /// so an idle engine sleeps indefinitely.
    pub fn deadline(&self) -> Option<Instant> {
        self.opened.map(|t| t + self.window)
    }

    /// Everything collected, once the window has closed.
    pub fn flush(&mut self, now: Instant) -> Option<Vec<PathId>> {
        if self.deadline()? > now {
            return None;
        }
        self.opened = None;
        Some(std::mem::take(&mut self.pending).into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bursts_collapse_into_one_flush() {
        let t0 = Instant::now();
        let mut c = Coalesce::new(Duration::from_secs(10));
        assert_eq!(c.deadline(), None);

        for i in 0..50 {
            c.push(PathId(7), t0 + Duration::from_millis(i * 100));
        }
        c.push(PathId(3), t0 + Duration::from_secs(5));

        assert_eq!(c.flush(t0 + Duration::from_secs(9)), None);
        assert_eq!(
            c.flush(t0 + Duration::from_secs(10)),
            Some(vec![PathId(3), PathId(7)])
        );
        assert_eq!(c.deadline(), None, "an empty queue arms nothing");
    }
}
