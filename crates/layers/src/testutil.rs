use std::future::{Future, Ready, ready};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use tower_service::Service;
use vuoxi_core::{
    Error, Manifest, Meta, PathId, PeerId, RelPath, Request, Response, VersionVector,
};

/// Counts the calls that got through; clones share the count.
#[derive(Clone, Default)]
pub struct Counter(pub Arc<AtomicUsize>);

impl Counter {
    pub fn calls(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

impl Service<Request> for Counter {
    type Response = Response;
    type Error = Error;
    type Future = Ready<Result<Response, Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Request) -> Self::Future {
        self.0.fetch_add(1, Ordering::Relaxed);
        ready(Ok(Response::Done))
    }
}

pub fn poll_now<F: Future<Output = Result<Response, Error>>>(fut: F) -> Response {
    let mut cx = Context::from_waker(Waker::noop());
    let mut fut = std::pin::pin!(fut);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(r) => r.unwrap(),
        Poll::Pending => panic!("test services are always ready"),
    }
}

pub fn run<S: Service<Request, Response = Response, Error = Error>>(
    s: &mut S,
    r: Request,
) -> Response {
    poll_now(s.call(r))
}

pub fn offer() -> Request {
    let m = Manifest {
        path: PathId(1),
        rel: RelPath::new("one").unwrap(),
        version: VersionVector::new(),
        meta: Meta::default(),
        chunks: vec![],
    };
    Request::Offer(PeerId([0; 32]), m)
}

pub fn fetch() -> Request {
    Request::Fetch(PeerId([0; 32]), vuoxi_core::ChunkId([0; 32]))
}
