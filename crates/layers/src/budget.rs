use crate::{Clock, GateFuture, Mode, ModeHandle};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tower_layer::Layer;
use tower_service::Service;
use vuoxi_core::{Error, Request, Response};

/// Inner calls allowed per window in each mode; `None` is unlimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budgets {
    pub window: Duration,
    pub live: Option<u32>,
    pub batched: Option<u32>,
    pub frugal: Option<u32>,
    pub hold: Option<u32>,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(60),
            live: None,
            batched: Some(6),
            frugal: Some(2),
            hold: Some(0),
        }
    }
}

impl Budgets {
    fn limit(&self, mode: Mode) -> Option<u32> {
        match mode {
            Mode::Live => self.live,
            Mode::Batched => self.batched,
            Mode::Frugal => self.frugal,
            Mode::Hold => self.hold,
        }
    }
}

#[derive(Debug)]
struct Window {
    start: Instant,
    used: u32,
}

/// Caps how often the engine is woken: over budget answers
/// [`Response::Deferred`] without calling inner. Fixed windows, starting at the
/// first call after the previous one closed. Clones share the count.
#[derive(Clone, Debug)]
pub struct WakeBudgetLayer<C> {
    mode: ModeHandle,
    budgets: Budgets,
    clock: C,
}

impl<C> WakeBudgetLayer<C> {
    pub fn new(mode: ModeHandle, budgets: Budgets, clock: C) -> Self {
        Self {
            mode,
            budgets,
            clock,
        }
    }
}

impl<S, C: Clone> Layer<S> for WakeBudgetLayer<C> {
    type Service = WakeBudget<S, C>;

    fn layer(&self, inner: S) -> Self::Service {
        WakeBudget {
            inner,
            mode: self.mode.clone(),
            budgets: self.budgets,
            clock: self.clock.clone(),
            window: Arc::new(Mutex::new(None)),
        }
    }
}

#[derive(Clone, Debug)]
pub struct WakeBudget<S, C> {
    inner: S,
    mode: ModeHandle,
    budgets: Budgets,
    clock: C,
    window: Arc<Mutex<Option<Window>>>,
}

impl<S, C: Clock> WakeBudget<S, C> {
    fn admit(&self) -> bool {
        let Some(limit) = self.budgets.limit(self.mode.get()) else {
            return true;
        };
        let now = self.clock.now();
        let mut slot = self.window.lock().unwrap_or_else(|e| e.into_inner());
        let w = match &mut *slot {
            Some(w) if now < w.start + self.budgets.window => w,
            other => other.insert(Window {
                start: now,
                used: 0,
            }),
        };
        if w.used < limit {
            w.used += 1;
            true
        } else {
            false
        }
    }
}

impl<S, C> Service<Request> for WakeBudget<S, C>
where
    S: Service<Request, Response = Response, Error = Error>,
    C: Clock,
{
    type Response = Response;
    type Error = Error;
    type Future = GateFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        if self.admit() {
            GateFuture::Inner {
                fut: self.inner.call(req),
            }
        } else {
            GateFuture::Deferred
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ManualClock;
    use crate::testutil::{Counter, offer, run};

    fn setup(
        mode: Mode,
    ) -> (
        ModeHandle,
        ManualClock,
        Counter,
        impl Service<Request, Response = Response, Error = Error>,
    ) {
        let m = ModeHandle::new(mode);
        let clock = ManualClock::new(Instant::now());
        let count = Counter::default();
        let svc = WakeBudgetLayer::new(
            m.clone(),
            Budgets {
                window: Duration::from_secs(60),
                live: None,
                batched: Some(3),
                frugal: Some(1),
                hold: Some(0),
            },
            clock.clone(),
        )
        .layer(count.clone());
        (m, clock, count, svc)
    }

    #[test]
    fn live_is_unlimited() {
        let (_, _, count, mut svc) = setup(Mode::Live);
        for _ in 0..1000 {
            assert_eq!(run(&mut svc, offer()), Response::Done);
        }
        assert_eq!(count.calls(), 1000);
    }

    #[test]
    fn batched_spends_then_defers_then_refills() {
        let (_, clock, count, mut svc) = setup(Mode::Batched);
        for _ in 0..3 {
            assert_eq!(run(&mut svc, offer()), Response::Done);
        }
        assert_eq!(run(&mut svc, offer()), Response::Deferred);
        clock.advance(Duration::from_secs(59));
        assert_eq!(run(&mut svc, offer()), Response::Deferred);
        clock.advance(Duration::from_secs(1));
        assert_eq!(run(&mut svc, offer()), Response::Done);
        assert_eq!(count.calls(), 4);
    }

    #[test]
    fn budget_follows_the_mode() {
        let (mode, _, count, mut svc) = setup(Mode::Frugal);
        assert_eq!(run(&mut svc, offer()), Response::Done);
        assert_eq!(run(&mut svc, offer()), Response::Deferred);
        mode.set(Mode::Hold);
        assert_eq!(run(&mut svc, offer()), Response::Deferred);
        mode.set(Mode::Live);
        assert_eq!(run(&mut svc, offer()), Response::Done);
        assert_eq!(count.calls(), 2);
    }
}
