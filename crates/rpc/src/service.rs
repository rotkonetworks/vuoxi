//! Verbs as services, on the standard tower traits.
//!
//! Every future here is `Ready` at construction, so the transport drives a
//! call with one no-op-waker poll: the tower contract without a runtime.

use crate::types::{ErrorCode, RpcRequest, RpcResponse};
use std::convert::Infallible;
use std::future::{Future, Ready, ready};
use std::task::{Context, Poll, Waker};

pub use tower_layer::Layer;
pub use tower_service::Service;

/// The uniform future type of every vuoxi-rpc service and layer.
pub type RpcFuture = Ready<Result<RpcResponse, Infallible>>;

/// Object-safe alias for a boxed verb or composed stack.
pub type BoxRpc = Box<
    dyn Service<RpcRequest, Response = RpcResponse, Error = Infallible, Future = RpcFuture> + Send,
>;

/// Lift a plain function into a Service. The function is the server.
pub struct Handler<F>(F);

pub fn handler<F>(f: F) -> Handler<F>
where
    F: FnMut(RpcRequest) -> RpcResponse,
{
    Handler(f)
}

impl<F> Service<RpcRequest> for Handler<F>
where
    F: FnMut(RpcRequest) -> RpcResponse,
{
    type Response = RpcResponse;
    type Error = Infallible;
    type Future = RpcFuture;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RpcRequest) -> RpcFuture {
        ready(Ok((self.0)(req)))
    }
}

/// Method-name router; itself a Service, so layers wrap it like any other.
#[derive(Default)]
pub struct Router {
    routes: Vec<(&'static str, BoxRpc)>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn route(
        mut self,
        method: &'static str,
        svc: impl Service<RpcRequest, Response = RpcResponse, Error = Infallible, Future = RpcFuture>
        + Send
        + 'static,
    ) -> Self {
        self.routes.push((method, Box::new(svc)));
        self
    }

    pub fn methods(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.routes.iter().map(|(m, _)| *m)
    }
}

impl Service<RpcRequest> for Router {
    type Response = RpcResponse;
    type Error = Infallible;
    type Future = RpcFuture;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RpcRequest) -> RpcFuture {
        for (m, svc) in &mut self.routes {
            if *m == req.method {
                return svc.call(req);
            }
        }
        ready(Ok(RpcResponse::err(
            req.id,
            ErrorCode::MethodNotFound,
            format!("unknown method: {}", req.method),
        )))
    }
}

/// Capability layer: only the listed verbs get through; the rest answer
/// `capability_denied` without reaching the inner service.
#[derive(Clone, Debug)]
pub struct AllowLayer(Vec<String>);

impl AllowLayer {
    pub fn new<I, S>(verbs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self(verbs.into_iter().map(Into::into).collect())
    }

    /// The read verbs: everything except `plan`, `apply` and `revert`.
    pub fn read_only() -> Self {
        Self::new(["status", "changes", "conflicts"])
    }
}

pub struct Allow<S> {
    inner: S,
    verbs: Vec<String>,
}

impl<S> Layer<S> for AllowLayer {
    type Service = Allow<S>;

    fn layer(&self, inner: S) -> Allow<S> {
        Allow {
            inner,
            verbs: self.0.clone(),
        }
    }
}

impl<S> Service<RpcRequest> for Allow<S>
where
    S: Service<RpcRequest, Response = RpcResponse, Error = Infallible, Future = RpcFuture>,
{
    type Response = RpcResponse;
    type Error = Infallible;
    type Future = RpcFuture;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: RpcRequest) -> RpcFuture {
        if self.verbs.contains(&req.method) {
            self.inner.call(req)
        } else {
            ready(Ok(RpcResponse::err(
                req.id,
                ErrorCode::CapabilityDenied,
                format!("not permitted: {}", req.method),
            )))
        }
    }
}

/// Drive one call to completion. Our futures are Ready by construction; a
/// Pending here is a bug in a service, not a runtime concern.
pub fn drive<S>(svc: &mut S, req: RpcRequest) -> RpcResponse
where
    S: Service<RpcRequest, Response = RpcResponse, Error = Infallible>,
{
    let mut cx = Context::from_waker(Waker::noop());
    match svc.poll_ready(&mut cx) {
        Poll::Ready(Ok(())) => {}
        Poll::Ready(Err(e)) => match e {},
        Poll::Pending => unreachable!("vuoxi-rpc services are always ready"),
    }
    let mut fut = std::pin::pin!(svc.call(req));
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(resp)) => resp,
        Poll::Ready(Err(e)) => match e {},
        Poll::Pending => unreachable!("vuoxi-rpc futures are Ready by construction"),
    }
}
