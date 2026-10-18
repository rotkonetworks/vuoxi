//! [`vuoxi_core::Transport`] over iroh QUIC.
//!
//! One request is one bidirectional stream: the caller writes a framed
//! [`Wire`] and finishes; the peer answers with one frame, or with nothing for
//! "no reply" (`Ok(None)`). Streams are cheap, so many requests share one
//! connection per peer.
//!
//! # Network posture
//!
//! Devices are expected to sit on a static-address mesh. The endpoint is built
//! from iroh's `Minimal` preset: no address lookup (DNS/pkarr), no relays, no
//! port mapping. Peers are dialled by public key plus the socket addresses
//! registered with [`IrohTransport::add_peer`]. Relays are an explicit opt-in
//! through [`Config::relays`].
//!
//! # Trust
//!
//! Only [`PeerId`]s in the [`Allowlist`] get in. The check runs in an iroh
//! `after_handshake` hook, so a stranger's connection is closed before an
//! ALPN handler, a stream, or any of this crate's parsing sees it. (The QUIC/TLS
//! handshake itself still runs; it authenticates the key, and carries no
//! application data.)
//!
//! # Idle timers and battery
//!
//! An open iroh connection is **not** idle on the wire: iroh sets a QUIC
//! keep-alive PING every 5 s on the connection and on each path, and the path
//! keep-alive cannot be raised above 5 s (iroh clamps it and ignores larger
//! values), so the timers cannot be turned off while a connection is open.
//! Without connections the endpoint holds no periodic work of ours (no
//! discovery, no relay, no portmapper), though iroh's own socket actors stay
//! parked.
//!
//! The lever is therefore to not keep connections open: [`Config::idle_close`]
//! (default 10 s) closes a peer connection that has carried no request for
//! that long, which silences the keep-alives until the next request redials.
//! Set it to `None` to keep connections up. [`IrohTransport::disconnect_all`]
//! closes them on demand, e.g. when entering a frugal power mode. The idle
//! timer itself is a single sleeping task per open connection.

#![forbid(unsafe_code)]

pub mod codec;

pub use codec::{CodecError, MAX_FRAME, decode_frame, encode_frame};

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex, RwLock},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use iroh::{
    Endpoint, EndpointAddr, PublicKey, RelayMode, SecretKey,
    endpoint::{
        AfterHandshakeOutcome, Connection, EndpointHooks, QuicTransportConfig, VarInt, presets,
    },
};
use tower_service::Service;
use vuoxi_core::{Error, PeerId, Wire};

/// The single ALPN. The trailing number is the protocol version.
pub const ALPN: &[u8] = b"vuoxi/0";

const CLOSE_IDLE: u32 = 0;
const CLOSE_DENIED: u32 = 403;
const CLOSE_PROTOCOL: u32 = 400;
const MAX_STREAMS: u32 = 16;

fn terr(e: impl std::fmt::Display) -> Error {
    Error::Transport(e.to_string())
}

/// The peers allowed to connect, shared and editable while running (pairing).
#[derive(Clone, Debug, Default)]
pub struct Allowlist(Arc<RwLock<HashSet<PeerId>>>);

impl Allowlist {
    pub fn new(peers: impl IntoIterator<Item = PeerId>) -> Self {
        Self(Arc::new(RwLock::new(peers.into_iter().collect())))
    }
    pub fn allow(&self, p: PeerId) {
        self.0.write().unwrap().insert(p);
    }
    pub fn revoke(&self, p: &PeerId) {
        self.0.write().unwrap().remove(p);
    }
    pub fn contains(&self, p: &PeerId) -> bool {
        self.0.read().unwrap().contains(p)
    }
}

#[derive(Debug)]
struct AllowHook(Allowlist);

impl EndpointHooks for AllowHook {
    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        if self.0.contains(&PeerId(*conn.remote_id().as_bytes())) {
            AfterHandshakeOutcome::accept()
        } else {
            AfterHandshakeOutcome::accept().reject(VarInt::from_u32(CLOSE_DENIED), b"unknown peer")
        }
    }
}

/// Endpoint configuration.
#[derive(Clone, Debug)]
pub struct Config {
    /// Ed25519 secret key; its public key is this device's [`PeerId`].
    pub secret_key: [u8; 32],
    /// Local addresses to bind. Empty means iroh's default (all interfaces,
    /// random port); set explicit addresses to confine the endpoint to the mesh
    /// interface or to loopback.
    pub bind: Vec<SocketAddr>,
    /// Relay URLs. Empty (the default) means no relay is ever contacted.
    pub relays: Vec<iroh::RelayUrl>,
    /// Close a connection after this long without a request. See the crate docs.
    pub idle_close: Option<Duration>,
    /// Who may connect.
    pub allow: Allowlist,
}

impl Config {
    pub fn new(secret_key: [u8; 32], allow: Allowlist) -> Self {
        Self {
            secret_key,
            bind: Vec::new(),
            relays: Vec::new(),
            idle_close: Some(Duration::from_secs(10)),
            allow,
        }
    }
}

struct Cached {
    conn: Connection,
    last_used: Arc<Mutex<Instant>>,
}

struct Inner {
    endpoint: Endpoint,
    allow: Allowlist,
    idle_close: Option<Duration>,
    addrs: Mutex<HashMap<PeerId, Vec<SocketAddr>>>,
    conns: tokio::sync::Mutex<HashMap<PeerId, Cached>>,
}

/// A [`vuoxi_core::Transport`] backed by an iroh endpoint. Cheap to clone.
#[derive(Clone)]
pub struct IrohTransport(Arc<Inner>);

impl IrohTransport {
    pub async fn bind(cfg: Config) -> Result<Self, Error> {
        let secret = SecretKey::from_bytes(&cfg.secret_key);
        let transport = QuicTransportConfig::builder()
            .max_concurrent_bidi_streams(VarInt::from_u32(MAX_STREAMS))
            .max_concurrent_uni_streams(VarInt::from_u32(0))
            .build();
        let mut b = Endpoint::builder(presets::Minimal)
            .secret_key(secret)
            .alpns(vec![ALPN.to_vec()])
            .transport_config(transport)
            .hooks(AllowHook(cfg.allow.clone()));
        if !cfg.bind.is_empty() {
            b = b.clear_ip_transports();
            for a in &cfg.bind {
                b = b.bind_addr(*a).map_err(terr)?;
            }
        }
        if !cfg.relays.is_empty() {
            b = b.relay_mode(RelayMode::custom(cfg.relays.clone()));
        }
        let endpoint = b.bind().await.map_err(terr)?;
        Ok(Self(Arc::new(Inner {
            endpoint,
            allow: cfg.allow,
            idle_close: cfg.idle_close,
            addrs: Default::default(),
            conns: Default::default(),
        })))
    }

    /// This device's identity.
    pub fn peer_id(&self) -> PeerId {
        PeerId(*self.0.endpoint.id().as_bytes())
    }

    /// Sockets the endpoint is actually bound to.
    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.0.endpoint.bound_sockets()
    }

    pub fn allowlist(&self) -> &Allowlist {
        &self.0.allow
    }

    /// Tell the transport where `peer` can be reached (replaces earlier addresses).
    pub fn add_peer(&self, peer: PeerId, addrs: Vec<SocketAddr>) {
        self.0.addrs.lock().unwrap().insert(peer, addrs);
    }

    /// Close every open connection (and with it every keep-alive timer).
    pub async fn disconnect_all(&self) {
        let all: Vec<_> = self.0.conns.lock().await.drain().collect();
        for (_, c) in all {
            c.conn.close(VarInt::from_u32(CLOSE_IDLE), b"disconnect");
        }
    }

    /// Close the endpoint.
    pub async fn close(&self) {
        self.disconnect_all().await;
        self.0.endpoint.close().await;
    }

    /// Answer incoming requests with `handler` until the returned handle drops.
    pub fn serve<S>(&self, handler: S) -> Serve
    where
        S: Service<(PeerId, Wire), Response = Option<Wire>, Error = Error> + Clone + Send + 'static,
        S::Future: Send,
    {
        let inner = self.0.clone();
        Serve(tokio::spawn(async move {
            while let Some(incoming) = inner.endpoint.accept().await {
                let (h, allow) = (handler.clone(), inner.allow.clone());
                tokio::spawn(async move {
                    if let Ok(conn) = incoming.await {
                        serve_conn(conn, h, allow).await;
                    }
                });
            }
        }))
    }

    async fn connection(&self, peer: PeerId, fresh: bool) -> Result<Connection, Error> {
        let mut conns = self.0.conns.lock().await;
        if let Some(c) = conns.get(&peer) {
            if !fresh && c.conn.close_reason().is_none() {
                *c.last_used.lock().unwrap() = Instant::now();
                return Ok(c.conn.clone());
            }
            conns.remove(&peer);
        }
        let ips = self.0.addrs.lock().unwrap().get(&peer).cloned();
        let ips = ips.ok_or_else(|| terr("no known address for peer"))?;
        let id = PublicKey::from_bytes(&peer.0).map_err(terr)?;
        let addr = EndpointAddr::new(id).with_addrs(ips.into_iter().map(iroh::TransportAddr::Ip));
        let conn = self.0.endpoint.connect(addr, ALPN).await.map_err(terr)?;
        let last_used = Arc::new(Mutex::new(Instant::now()));
        if let Some(idle) = self.0.idle_close {
            tokio::spawn(reap(conn.clone(), last_used.clone(), idle));
        }
        conns.insert(
            peer,
            Cached {
                conn: conn.clone(),
                last_used,
            },
        );
        Ok(conn)
    }

    async fn request(&self, peer: PeerId, msg: Wire) -> Result<Option<Wire>, Error> {
        if !self.0.allow.contains(&peer) {
            return Err(terr("peer not in allowlist"));
        }
        let frame = encode_frame(&msg).map_err(terr)?;
        let mut last = None;
        // A cached connection may have died since last use; retry once on a new one.
        for fresh in [false, true] {
            let conn = self.connection(peer, fresh).await?;
            match exchange(&conn, &frame).await {
                Ok(r) => return Ok(r),
                Err(e) => last = Some(e),
            }
        }
        Err(last.expect("loop ran"))
    }
}

async fn exchange(conn: &Connection, frame: &[u8]) -> Result<Option<Wire>, Error> {
    let (mut tx, mut rx) = conn.open_bi().await.map_err(terr)?;
    tx.write_all(frame).await.map_err(terr)?;
    tx.finish().map_err(terr)?;
    let buf = rx.read_to_end(MAX_FRAME + 4).await.map_err(terr)?;
    if buf.is_empty() {
        return Ok(None);
    }
    decode_frame(&buf).map(Some).map_err(terr)
}

/// Close `conn` once it has been unused for `idle`; end when it closes.
async fn reap(conn: Connection, last_used: Arc<Mutex<Instant>>, idle: Duration) {
    let closed = conn.closed();
    tokio::pin!(closed);
    loop {
        let due = *last_used.lock().unwrap() + idle;
        tokio::select! {
            _ = tokio::time::sleep_until(due.into()) => {
                if last_used.lock().unwrap().elapsed() >= idle {
                    conn.close(VarInt::from_u32(CLOSE_IDLE), b"idle");
                    return;
                }
            }
            _ = &mut closed => return,
        }
    }
}

async fn serve_conn<S>(conn: Connection, svc: S, allow: Allowlist)
where
    S: Service<(PeerId, Wire), Response = Option<Wire>, Error = Error> + Clone + Send + 'static,
    S::Future: Send,
{
    let peer = PeerId(*conn.remote_id().as_bytes());
    // The hook already refuses strangers; this guards against a revoke in between.
    if !allow.contains(&peer) {
        conn.close(VarInt::from_u32(CLOSE_DENIED), b"unknown peer");
        return;
    }
    while let Ok((mut tx, mut rx)) = conn.accept_bi().await {
        let h = svc.clone();
        let c = conn.clone();
        tokio::spawn(async move {
            let Ok(buf) = rx.read_to_end(MAX_FRAME + 4).await else {
                return;
            };
            let Ok(msg) = decode_frame(&buf) else {
                c.close(VarInt::from_u32(CLOSE_PROTOCOL), b"bad frame");
                return;
            };
            let reply = match call(h, (peer, msg)).await {
                Ok(Some(r)) => match encode_frame(&r) {
                    Ok(f) => f,
                    Err(_) => return,
                },
                Ok(None) => Vec::new(),
                Err(_) => return, // reset by drop; the caller sees a failed request
            };
            if tx.write_all(&reply).await.is_ok() {
                let _ = tx.finish();
                // Let the reply drain before the stream handle goes away.
                let _ = tx.stopped().await;
            }
        });
    }
}

async fn call<S>(mut svc: S, req: (PeerId, Wire)) -> Result<Option<Wire>, Error>
where
    S: Service<(PeerId, Wire), Response = Option<Wire>, Error = Error>,
{
    std::future::poll_fn(|cx| svc.poll_ready(cx)).await?;
    svc.call(req).await
}

/// Keeps the accept loop alive; dropping it stops accepting.
pub struct Serve(tokio::task::JoinHandle<()>);

impl Drop for Serve {
    fn drop(&mut self) {
        self.0.abort();
    }
}

type BoxFut = Pin<Box<dyn Future<Output = Result<Option<Wire>, Error>> + Send>>;

/// Send `(peer, wire)`; the response is the peer's reply, if it sent one.
impl Service<(PeerId, Wire)> for IrohTransport {
    type Response = Option<Wire>;
    type Error = Error;
    type Future = BoxFut;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, (peer, msg): (PeerId, Wire)) -> Self::Future {
        let this = self.clone();
        Box::pin(async move { this.request(peer, msg).await })
    }
}

#[cfg(test)]
mod tests;
