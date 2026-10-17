use crate::{Clock, Coalesce, GateFuture};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tower_layer::Layer;
use tower_service::Service;
use vuoxi_core::{Error, PathId, Request, Response};

/// Parks `Changed` requests in a [`Coalesce`] queue and answers
/// [`Response::Deferred`]; everything else passes through. Clones share one
/// queue.
#[derive(Clone, Debug)]
pub struct CoalesceLayer<C> {
    window: Duration,
    clock: C,
}

impl<C> CoalesceLayer<C> {
    pub fn new(window: Duration, clock: C) -> Self {
        Self { window, clock }
    }
}

impl<S, C: Clone> Layer<S> for CoalesceLayer<C> {
    type Service = CoalesceService<S, C>;

    fn layer(&self, inner: S) -> Self::Service {
        CoalesceService {
            inner,
            queue: Arc::new(Mutex::new(Coalesce::new(self.window))),
            clock: self.clock.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CoalesceService<S, C> {
    inner: S,
    queue: Arc<Mutex<Coalesce>>,
    clock: C,
}

impl<S, C> CoalesceService<S, C> {
    fn queue(&self) -> std::sync::MutexGuard<'_, Coalesce> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// When [`CoalesceService::flush`] is next due; `None` while idle, so the
    /// caller arms no timer.
    pub fn deadline(&self) -> Option<Instant> {
        self.queue().deadline()
    }
}

impl<S, C> CoalesceService<S, C>
where
    S: Service<Request, Response = Response, Error = Error>,
{
    /// Once the window has closed, hands the batch to the inner service as one
    /// `Changed` per distinct path and returns the futures for the caller to
    /// drive. Empty if the window is still open. The caller must have seen
    /// `poll_ready` succeed. The batch enters below any outer layers, which
    /// already admitted these requests when they were parked.
    pub fn flush(&mut self, now: Instant) -> Vec<S::Future> {
        let batch: Vec<PathId> = self.queue().flush(now).unwrap_or_default();
        batch
            .into_iter()
            .map(|p| self.inner.call(Request::Changed(p)))
            .collect()
    }
}

impl<S, C> Service<Request> for CoalesceService<S, C>
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
        match req {
            Request::Changed(path) => {
                let now = self.clock.now();
                self.queue().push(path, now);
                GateFuture::Deferred
            }
            other => GateFuture::Inner {
                fut: self.inner.call(other),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ManualClock;
    use crate::testutil::{Counter, offer, poll_now, run};

    #[test]
    fn changes_wait_for_flush_and_collapse() {
        let t0 = Instant::now();
        let clock = ManualClock::new(t0);
        let count = Counter::default();
        let mut svc =
            CoalesceLayer::new(Duration::from_secs(10), clock.clone()).layer(count.clone());
        assert_eq!(svc.deadline(), None);

        for _ in 0..20 {
            assert_eq!(
                run(&mut svc, Request::Changed(PathId(7))),
                Response::Deferred
            );
        }
        run(&mut svc, Request::Changed(PathId(3)));
        assert_eq!(count.calls(), 0);
        assert_eq!(svc.deadline(), Some(t0 + Duration::from_secs(10)));

        assert!(svc.flush(t0 + Duration::from_secs(9)).is_empty());
        clock.advance(Duration::from_secs(10));
        let futs = svc.flush(t0 + Duration::from_secs(10));
        assert_eq!(futs.len(), 2, "20 saves of one file plus another = 2");
        for f in futs {
            assert_eq!(poll_now(f), Response::Done);
        }
        assert_eq!(count.calls(), 2);
        assert_eq!(svc.deadline(), None);
    }

    #[test]
    fn other_requests_pass_through() {
        let count = Counter::default();
        let mut svc = CoalesceLayer::new(Duration::from_secs(1), ManualClock::new(Instant::now()))
            .layer(count.clone());
        assert_eq!(run(&mut svc, offer()), Response::Done);
        assert_eq!(count.calls(), 1);
        assert_eq!(svc.deadline(), None);
    }
}
