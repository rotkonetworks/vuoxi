use crate::power::Mode;
use pin_project_lite::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::task::{Context, Poll};
use tower_layer::Layer;
use tower_service::Service;
use vuoxi_core::{Error, Request, Response};

/// Shared, cheaply cloned current mode. The power monitor writes it, every
/// gate reads it; no locks, no wakeups.
#[derive(Clone, Debug)]
pub struct ModeHandle(Arc<AtomicU8>);

impl ModeHandle {
    pub fn new(mode: Mode) -> Self {
        Self(Arc::new(AtomicU8::new(mode as u8)))
    }

    pub fn get(&self) -> Mode {
        Mode::from_u8(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, mode: Mode) {
        self.0.store(mode as u8, Ordering::Relaxed);
    }
}

/// The most frugal mode in which a request may still run.
fn allowed_up_to(req: &Request) -> Mode {
    match req {
        // Learning what a peer has is metadata only; keep doing it on low battery.
        Request::Offer(..) => Mode::Frugal,
        Request::Changed(_) | Request::Fetch(..) | Request::Reconcile(_) => Mode::Batched,
    }
}

/// Answers [`Response::Deferred`] without touching the inner service when the
/// current mode is too frugal for the request.
#[derive(Clone, Debug)]
pub struct ModeGate<S> {
    inner: S,
    mode: ModeHandle,
}

#[derive(Clone, Debug)]
pub struct ModeGateLayer(ModeHandle);

impl ModeGateLayer {
    pub fn new(mode: ModeHandle) -> Self {
        Self(mode)
    }
}

impl<S> Layer<S> for ModeGateLayer {
    type Service = ModeGate<S>;

    fn layer(&self, inner: S) -> ModeGate<S> {
        ModeGate {
            inner,
            mode: self.0.clone(),
        }
    }
}

impl<S> Service<Request> for ModeGate<S>
where
    S: Service<Request, Response = Response, Error = Error>,
{
    type Response = Response;
    type Error = Error;
    type Future = GateFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        if self.mode.get() > allowed_up_to(&req) {
            GateFuture::Deferred
        } else {
            GateFuture::Inner {
                fut: self.inner.call(req),
            }
        }
    }
}

pin_project! {
    #[project = GateProj]
    pub enum GateFuture<F> {
        Inner { #[pin] fut: F },
        Deferred,
    }
}

impl<F> Future for GateFuture<F>
where
    F: Future<Output = Result<Response, Error>>,
{
    type Output = Result<Response, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            GateProj::Inner { fut } => fut.poll(cx),
            GateProj::Deferred => Poll::Ready(Ok(Response::Deferred)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Counter, offer, run};
    use vuoxi_core::PathId;

    #[test]
    fn gate_follows_the_mode() {
        let mode = ModeHandle::new(Mode::Live);
        let count = Counter::default();
        let mut svc = ModeGateLayer::new(mode.clone()).layer(count.clone());

        assert_eq!(run(&mut svc, Request::Changed(PathId(1))), Response::Done);

        mode.set(Mode::Frugal);
        assert_eq!(
            run(&mut svc, Request::Changed(PathId(1))),
            Response::Deferred
        );
        assert_eq!(run(&mut svc, offer()), Response::Done);

        mode.set(Mode::Hold);
        assert_eq!(run(&mut svc, offer()), Response::Deferred);

        assert_eq!(count.calls(), 2, "deferred requests never reach the engine");
    }
}
