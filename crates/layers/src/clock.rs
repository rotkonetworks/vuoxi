use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where layers learn the time. Production reads the OS; the simulator and the
/// tests inject their own.
pub trait Clock {
    fn now(&self) -> Instant;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A clock that moves only when told to; clones share the time.
#[derive(Clone, Debug)]
pub struct ManualClock(Arc<Mutex<Instant>>);

impl ManualClock {
    pub fn new(start: Instant) -> Self {
        Self(Arc::new(Mutex::new(start)))
    }

    pub fn advance(&self, by: Duration) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}
