//! The dashboard's HTTP API (docs/API.md) and its static files.
//!
//! Loopback only, one small HTTP/1.1 exchange per connection, no framework.
//! Every `/api/` request needs the token (cookie or bearer); every `POST` also
//! needs `X-Vuoxi: 1`, which a cross-site form cannot set.

use crate::fs::{self as vfs, TRASH_DIR};
use crate::node::{Event, Node};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use vuoxi_core::{PeerId, RelPath};
use vuoxi_layers::Mode;

const MAX_HEAD: usize = 16 << 10;
const MAX_BODY: usize = 64 << 10;

pub struct PeerInfo {
    pub name: String,
    pub id: String,
    pub addrs: Vec<String>,
}

pub struct FolderInfo {
    pub tag: String,
    pub peers: Vec<String>,
    pub read_only_peers: Vec<String>,
}

/// A folder to add, as the dashboard describes it.
pub struct NewFolder {
    pub id: String,
    pub path: PathBuf,
    /// `None`: detect from the contents.
    pub tag: Option<String>,
    pub peers: Vec<String>,
    pub ro_peers: Vec<String>,
    /// `None`: generate one.
    pub key: Option<[u8; 32]>,
}

/// Changes to the configuration. The daemon's owner implements it: it edits
/// the config file and restarts the engine. Errors are shown to the user.
pub trait Control: Send + Sync {
    fn add_folder(&self, f: NewFolder) -> Result<(), String>;
    /// `tag`: `None` leaves it, `Some(None)` detects it again.
    fn set_sharing(
        &self,
        id: &str,
        peers: Vec<String>,
        ro_peers: Vec<String>,
        tag: Option<Option<String>>,
    ) -> Result<(), String>;
    fn remove_folder(&self, id: &str) -> Result<(), String>;
}

pub struct ApiCtx {
    pub token: String,
    pub device_id: String,
    pub ui_dir: PathBuf,
    pub peers: HashMap<PeerId, PeerInfo>,
    pub folders: HashMap<String, FolderInfo>,
    pub watcher: Arc<AtomicBool>,
    pub started: Instant,
    pub control: Arc<dyn Control>,
}

/// The token in `state_dir/ui-token`, created (0600) on first use.
pub fn token(state_dir: &Path) -> io::Result<String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let p = state_dir.join("ui-token");
    if let Ok(t) = std::fs::read_to_string(&p) {
        return Ok(t.trim().to_owned());
    }
    std::fs::create_dir_all(state_dir)?;
    let mut b = [0u8; 24];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    let t: String = b.iter().map(|x| format!("{x:02x}")).collect();
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&p)
        .and_then(|mut f| io::Write::write_all(&mut f, t.as_bytes()))?;
    Ok(t)
}

pub async fn serve(node: Arc<Node>, ctx: Arc<ApiCtx>, listen: SocketAddr) -> io::Result<()> {
    if !listen.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the dashboard API only listens on loopback",
        ));
    }
    let l = TcpListener::bind(listen).await?;
    loop {
        let (sock, _) = l.accept().await?;
        let (node, ctx) = (node.clone(), ctx.clone());
        tokio::spawn(async move {
            let _ = conn(sock, &node, &ctx).await;
        });
    }
}

struct Req {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn read_req(sock: &mut TcpStream) -> io::Result<Option<Req>> {
    let mut buf = Vec::with_capacity(4096);
    let head_end = loop {
        let mut chunk = [0u8; 4096];
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return Ok(None);
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let method = first.next().unwrap_or("").to_owned();
    let target = first.next().unwrap_or("/").to_owned();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return Ok(None);
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let mut chunk = vec![0u8; len - body.len()];
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    let (path, q) = target.split_once('?').unwrap_or((&target, ""));
    let query = q
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (unescape(k), unescape(v)))
        .collect();
    Ok(Some(Req {
        method,
        path: unescape(path),
        query,
        headers,
        body,
    }))
}

async fn respond(
    sock: &mut TcpStream,
    status: &str,
    ctype: &str,
    extra: &str,
    body: &[u8],
) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\nX-Frame-Options: DENY\r\n{extra}Connection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body).await?;
    sock.shutdown().await
}

async fn json_resp(sock: &mut TcpStream, status: &str, v: Value) -> io::Result<()> {
    respond(
        sock,
        status,
        "application/json",
        "",
        v.to_string().as_bytes(),
    )
    .await
}

fn authorized(r: &Req, token: &str) -> bool {
    let bearer = r
        .headers
        .get("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| ct_eq(t, token));
    let cookie = r.headers.get("cookie").is_some_and(|c| {
        c.split(';')
            .filter_map(|kv| kv.trim().split_once('='))
            .any(|(k, v)| k == "vuoxi" && ct_eq(v, token))
    });
    bearer || cookie
}

/// Constant-time comparison for the token.
fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

async fn conn(mut sock: TcpStream, node: &Node, ctx: &ApiCtx) -> io::Result<()> {
    let Some(r) = read_req(&mut sock).await? else {
        return Ok(());
    };
    if r.method == "GET"
        && r.path == "/"
        && let Some(t) = r.query.get("t")
    {
        if ct_eq(t, &ctx.token) {
            let cookie = format!(
                "Set-Cookie: vuoxi={}; HttpOnly; SameSite=Strict; Path=/\r\nLocation: /\r\n",
                ctx.token
            );
            return respond(&mut sock, "302 Found", "text/plain", &cookie, b"").await;
        }
        return json_resp(
            &mut sock,
            "401 Unauthorized",
            json!({"error": "unauthorized"}),
        )
        .await;
    }
    if !r.path.starts_with("/api/") {
        return static_file(&mut sock, &ctx.ui_dir, &r.path).await;
    }
    if !authorized(&r, &ctx.token) {
        return json_resp(
            &mut sock,
            "401 Unauthorized",
            json!({"error": "unauthorized"}),
        )
        .await;
    }
    if r.method == "POST" && r.headers.get("x-vuoxi").map(String::as_str) != Some("1") {
        return json_resp(
            &mut sock,
            "403 Forbidden",
            json!({"error": "missing X-Vuoxi header"}),
        )
        .await;
    }
    match route(&r, node, ctx).await {
        Ok(v) => json_resp(&mut sock, "200 OK", v).await,
        Err((code, msg)) => json_resp(&mut sock, code, json!({"error": msg})).await,
    }
}

type ApiErr = (&'static str, String);
type ApiResult = Result<Value, (&'static str, String)>;

fn not_found(what: impl std::fmt::Display) -> (&'static str, String) {
    ("404 Not Found", format!("{what}"))
}

fn bad(what: impl std::fmt::Display) -> (&'static str, String) {
    ("400 Bad Request", format!("{what}"))
}

fn num(r: &Req, k: &str, default: u64) -> Result<u64, (&'static str, String)> {
    r.query.get(k).map_or(Ok(default), |v| {
        v.parse().map_err(|_| bad(format!("{k} must be a number")))
    })
}

fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

async fn route(r: &Req, node: &Node, ctx: &ApiCtx) -> ApiResult {
    let parts: Vec<&str> = r.path.trim_matches('/').split('/').collect();
    match (r.method.as_str(), parts.as_slice()) {
        ("GET", ["api", "status"]) => Ok(status(node, ctx)),
        ("GET", ["api", "transfers"]) => Ok(transfers(node, ctx)),
        ("POST", ["api", "sync"]) => {
            node.sync_now();
            Ok(json!({"ok": true}))
        }
        ("POST", ["api", "mode"]) => {
            let v: Value = serde_json::from_slice(&r.body).map_err(bad)?;
            let m = match v.get("mode").and_then(Value::as_str) {
                Some("live") => Mode::Live,
                Some("batched") => Mode::Batched,
                Some("frugal") => Mode::Frugal,
                Some("hold") => Mode::Hold,
                _ => return Err(bad("mode must be live, batched, frugal or hold")),
            };
            node.mode().set(m);
            node.sync_now();
            Ok(json!({"mode": mode_name(m)}))
        }
        ("GET", ["api", "offers"]) => Ok(offers(node, ctx)),
        ("POST", ["api", "offers", "accept"]) => accept(node, ctx, r),
        ("POST", ["api", "offers", "dismiss"]) => {
            let v: Value = serde_json::from_slice(&r.body).map_err(bad)?;
            let (folder, from) = offer_key(&v, ctx)?;
            node.offers().dismiss(&folder, from);
            Ok(json!({"ok": true}))
        }
        ("GET", ["api", "browse"]) => Ok(browse(r.query.get("path").map_or("", String::as_str))),
        ("POST", ["api", "folders"]) => {
            let v: Value = serde_json::from_slice(&r.body).map_err(bad)?;
            let id = v
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("id"))?;
            let path = v
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("path"))?;
            let (peers, ro_peers) = peer_lists(&v, ctx)?;
            ctx.control
                .add_folder(NewFolder {
                    id: id.to_owned(),
                    path: expand(path),
                    tag: v.get("tag").and_then(Value::as_str).map(str::to_owned),
                    peers,
                    ro_peers,
                    key: None,
                })
                .map_err(bad)?;
            Ok(json!({"ok": true}))
        }
        ("POST", ["api", "folders", id, "sharing"]) => {
            let v: Value = serde_json::from_slice(&r.body).map_err(bad)?;
            let (peers, ro_peers) = peer_lists(&v, ctx)?;
            let tag = v.get("tag").map(|t| t.as_str().map(str::to_owned));
            ctx.control
                .set_sharing(id, peers, ro_peers, tag)
                .map_err(bad)?;
            Ok(json!({"ok": true}))
        }
        ("POST", ["api", "folders", id, "remove"]) => {
            ctx.control.remove_folder(id).map_err(bad)?;
            Ok(json!({"ok": true}))
        }
        ("GET", ["api", "folders", id, what]) => {
            if !node.folder_ids().iter().any(|f| f == id) {
                return Err(not_found(format!("no folder {id}")));
            }
            match *what {
                "files" => files(node, id, r),
                "tree" => tree(node, id, r),
                "changes" => changes(node, id, r),
                "trash" => trash(node, id),
                "conflicts" => conflicts(node, id),
                _ => Err(not_found(r.path.clone())),
            }
        }
        ("POST", ["api", "folders", id, "trash", "restore"]) => restore(node, id, r).await,
        _ => Err(not_found(r.path.clone())),
    }
}

/// `~/x` → `$HOME/x`.
fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/").or((p == "~").then_some("")) {
        Some(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(p),
    }
}

fn names(v: &Value, k: &str) -> Result<Vec<String>, (&'static str, String)> {
    match v.get(k) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| x.as_str().map(str::to_owned).ok_or_else(|| bad(k)))
            .collect(),
        _ => Err(bad(format!("{k} must be a list of peer names"))),
    }
}

/// `peers` and `ro_peers`, checked against the configured peers.
fn peer_lists(
    v: &Value,
    ctx: &ApiCtx,
) -> Result<(Vec<String>, Vec<String>), ApiErr> {
    let (peers, ro) = (names(v, "peers")?, names(v, "ro_peers")?);
    for n in peers.iter().chain(&ro) {
        if !ctx.peers.values().any(|p| &p.name == n) {
            return Err(bad(format!("no peer named {n}")));
        }
    }
    if let Some(n) = ro.iter().find(|n| !peers.contains(n)) {
        return Err(bad(format!("{n} is read-only but not sharing the folder")));
    }
    Ok((peers, ro))
}

fn peer_name(ctx: &ApiCtx, id: &PeerId) -> Option<String> {
    ctx.peers.get(id).map(|p| p.name.clone())
}

fn offer_key(v: &Value, ctx: &ApiCtx) -> Result<(String, PeerId), (&'static str, String)> {
    let folder = v
        .get("folder")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("folder"))?;
    let from = v
        .get("from")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("from"))?;
    let id = ctx
        .peers
        .iter()
        .find(|(_, p)| p.name == from)
        .map(|(id, _)| *id)
        .ok_or_else(|| bad(format!("no peer named {from}")))?;
    Ok((folder.to_owned(), id))
}

fn offers(node: &Node, ctx: &ApiCtx) -> Value {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let items: Vec<Value> = node
        .offers()
        .list()
        .into_iter()
        .filter_map(|o| {
            let from = peer_name(ctx, &o.from)?;
            let members: Vec<String> = o
                .members
                .iter()
                .map(|m| peer_name(ctx, m).unwrap_or_else(|| format!("{}…", short(m))))
                .collect();
            Some(json!({
                "folder": o.folder, "from": from, "tag": o.tag,
                "members": members, "path": home.join(&o.folder),
            }))
        })
        .collect();
    json!({ "items": items })
}

fn short(p: &PeerId) -> String {
    p.0[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// Join an offered folder: shared with the offering device and every other
/// member we know (a relay included), with the folder's key and tag.
fn accept(node: &Node, ctx: &ApiCtx, r: &Req) -> ApiResult {
    let v: Value = serde_json::from_slice(&r.body).map_err(bad)?;
    let (folder, from) = offer_key(&v, ctx)?;
    let o = node
        .offers()
        .get(&folder, from)
        .ok_or_else(|| not_found("no such offer"))?;
    let path = v
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("path"))?;
    let mut peers: Vec<String> = Vec::new();
    for id in std::iter::once(&o.from).chain(&o.members) {
        if let Some(n) = peer_name(ctx, id).filter(|n| !peers.contains(n)) {
            peers.push(n);
        }
    }
    ctx.control
        .add_folder(NewFolder {
            id: o.folder.clone(),
            path: expand(path),
            tag: (!o.tag.is_empty()).then(|| o.tag.clone()),
            peers,
            ro_peers: Vec::new(),
            key: o.key,
        })
        .map_err(bad)?;
    node.offers().take(&o.folder);
    Ok(json!({"ok": true}))
}

/// Directory completion for a path field.
fn browse(input: &str) -> Value {
    let p = expand(if input.is_empty() { "~" } else { input });
    let (dir, prefix) = if p.is_dir() {
        (p.clone(), String::new())
    } else {
        let prefix = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        (
            p.parent().map(Path::to_path_buf).unwrap_or_default(),
            prefix,
        )
    };
    let mut dirs: Vec<String> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let hidden_ok = !name.starts_with('.') || prefix.starts_with('.');
            (hidden_ok && name.starts_with(&prefix)).then(|| dir.join(name).display().to_string())
        })
        .collect();
    dirs.sort();
    dirs.truncate(50);
    json!({ "dirs": dirs })
}

/// One directory level of the folder's live files.
fn tree(node: &Node, id: &str, r: &Req) -> ApiResult {
    let dir = r.query.get("dir").map_or("", |d| d.trim_matches('/'));
    if !dir.is_empty() && RelPath::new(dir).is_err() {
        return Err(bad("dir must be a relative path"));
    }
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    node.with_folder(id, |f| {
        let mut dirs: std::collections::BTreeMap<&str, (u64, u64)> = Default::default();
        let mut files = Vec::new();
        for m in f.live() {
            let Some(rest) = m.rel.as_str().strip_prefix(&prefix) else {
                continue;
            };
            match rest.split_once('/') {
                Some((d, _)) => {
                    let e = dirs.entry(d).or_default();
                    e.0 += 1;
                    e.1 += m.meta.size;
                }
                None => files.push(m),
            }
        }
        files.sort_by(|a, b| a.rel.cmp(&b.rel));
        json!({
            "dir": dir,
            "dirs": dirs.into_iter().map(|(name, (n, b))| json!({"name": name, "files": n, "bytes": b})).collect::<Vec<_>>(),
            "files": files.into_iter().map(|m| json!({
                "name": m.rel.file_name(), "size": m.meta.size,
                "mtime_ns": m.meta.mtime_ns, "mode": m.meta.mode,
            })).collect::<Vec<_>>(),
        })
    })
    .ok_or_else(|| not_found(id))
}

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::Live => "live",
        Mode::Batched => "batched",
        Mode::Frugal => "frugal",
        Mode::Hold => "hold",
    }
}

fn trash_items(root: &Path) -> Vec<(u64, String, u64)> {
    let mut out = Vec::new();
    let Ok(stamps) = std::fs::read_dir(root.join(TRASH_DIR)) else {
        return out;
    };
    for s in stamps.flatten() {
        let Some(stamp) = s.file_name().to_str().and_then(|x| x.parse::<u64>().ok()) else {
            continue;
        };
        let mut files = Vec::new();
        let base = s.path();
        let _ = vfs::walk(&base, None, &mut |_| false, &mut files);
        for f in files {
            let size = std::fs::metadata(base.join(f.as_str())).map_or(0, |m| m.len());
            out.push((stamp, f.as_str().to_owned(), size));
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    out
}

fn status(node: &Node, ctx: &ApiCtx) -> Value {
    let folders: Vec<Value> = node
        .folder_ids()
        .iter()
        .map(|id| {
            let info = ctx.folders.get(id);
            node.with_folder(id, |f| {
                let (mut files, mut bytes, mut deleted, mut conflicts) = (0u64, 0u64, 0u64, 0u64);
                for m in f.entries() {
                    if m.meta.deleted {
                        deleted += 1;
                    } else {
                        files += 1;
                        bytes += m.meta.size;
                        conflicts += u64::from(m.rel.file_name().contains(".vuoxi-conflict-"));
                    }
                }
                let trash_bytes: u64 = trash_items(&f.root).iter().map(|t| t.2).sum();
                json!({
                    "id": id,
                    "root": f.root,
                    "tag": info.map(|i| i.tag.as_str()).unwrap_or("code"),
                    "files": files, "bytes": bytes, "deleted": deleted,
                    "head": f.head(),
                    "epoch": format!("{:016x}", f.epoch()),
                    "root_hash": f.root_hash().iter().map(|b| format!("{b:02x}")).collect::<String>(),
                    "peers": info.map(|i| i.peers.clone()).unwrap_or_default(),
                    "read_only_peers": info.map(|i| i.read_only_peers.clone()).unwrap_or_default(),
                    "trash_bytes": trash_bytes,
                    "conflicts": conflicts,
                })
            })
            .unwrap_or(Value::Null)
        })
        .collect();
    let seen = node.last_seen();
    let mut peers: Vec<Value> = ctx
        .peers
        .iter()
        .map(|(pid, p)| {
            json!({
                "name": p.name, "id": p.id, "addrs": p.addrs,
                "relay": node.is_relay(pid),
                "last_seen_s": seen.get(pid).copied().flatten().map(|d| d.as_secs()),
            })
        })
        .collect();
    peers.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    json!({
        "device": {
            "id": ctx.device_id,
            "mode": mode_name(node.mode().get()),
            "watcher": ctx.watcher.load(Ordering::Relaxed),
            "uptime_s": ctx.started.elapsed().as_secs(),
        },
        "folders": folders,
        "peers": peers,
    })
}

fn transfers(node: &Node, ctx: &ApiCtx) -> Value {
    let items: Vec<Value> = node
        .transfers()
        .into_iter()
        .map(|t| {
            json!({
                "id": t.id, "folder": t.folder, "rel": t.rel, "dir": "down",
                "peer": ctx.peers.get(&t.peer).map_or("?", |p| p.name.as_str()),
                "size": t.size, "done": t.done,
                "state": match (&t.finished, &t.error) {
                    (None, _) => "active",
                    (Some(_), None) => "done",
                    (Some(_), Some(_)) => "failed",
                },
                "started_s": t.started.elapsed().as_secs(),
                "error": t.error,
            })
        })
        .collect();
    json!({ "items": items })
}

fn files(node: &Node, id: &str, r: &Req) -> ApiResult {
    let q = r
        .query
        .get("q")
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    let offset = num(r, "offset", 0)? as usize;
    let limit = num(r, "limit", 100)?.min(500) as usize;
    let desc = r.query.get("desc").is_some_and(|v| v == "1");
    let deleted = r.query.get("deleted").is_some_and(|v| v == "1");
    let sort = r.query.get("sort").map_or("rel", String::as_str).to_owned();
    node.with_folder(id, |f| {
        let mut v: Vec<_> = f
            .entries()
            .filter(|m| deleted || !m.meta.deleted)
            .filter(|m| q.is_empty() || m.rel.as_str().to_lowercase().contains(&q))
            .collect();
        match sort.as_str() {
            "size" => v.sort_by_key(|m| m.meta.size),
            "mtime" => v.sort_by_key(|m| m.meta.mtime_ns),
            _ => v.sort_by(|a, b| a.rel.cmp(&b.rel)),
        }
        if desc {
            v.reverse();
        }
        let total = v.len();
        let items: Vec<Value> = v
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|m| {
                let version: serde_json::Map<String, Value> = m
                    .version
                    .iter()
                    .map(|(p, n)| {
                        (
                            p.0[..4].iter().map(|b| format!("{b:02x}")).collect(),
                            json!(n),
                        )
                    })
                    .collect();
                json!({
                    "rel": m.rel.as_str(), "size": m.meta.size, "mtime_ns": m.meta.mtime_ns,
                    "mode": m.meta.mode, "deleted": m.meta.deleted, "version": version,
                })
            })
            .collect();
        json!({"total": total, "items": items})
    })
    .ok_or_else(|| not_found(id))
}

fn changes(node: &Node, id: &str, r: &Req) -> ApiResult {
    let limit = num(r, "limit", 100)?.min(500) as usize;
    let desc = r.query.get("desc").is_some_and(|v| v == "1");
    node.with_folder(id, |f| {
        let head = f.head();
        let since = if desc {
            head.saturating_sub(limit as u64)
        } else {
            num(r, "since", 0)?
        };
        let entries = f.since(since, limit).map_err(|e| bad(e.to_string()))?;
        let next = entries
            .last()
            .filter(|(s, _)| !desc && *s < head)
            .map(|(s, _)| *s);
        let mut items: Vec<Value> = entries
            .iter()
            .map(|(seq, p)| match f.manifest(*p) {
                Some(m) => json!({"seq": seq, "rel": m.rel.as_str(), "size": m.meta.size,
                                  "deleted": m.meta.deleted, "mtime_ns": m.meta.mtime_ns}),
                None => {
                    json!({"seq": seq, "rel": null, "size": 0, "deleted": false, "mtime_ns": 0})
                }
            })
            .collect();
        if desc {
            items.reverse();
        }
        Ok(json!({"head": head, "next": next, "items": items}))
    })
    .ok_or_else(|| not_found(id))?
}

fn trash(node: &Node, id: &str) -> ApiResult {
    let root = node
        .with_folder(id, |f| f.root.clone())
        .ok_or_else(|| not_found(id))?;
    let items: Vec<Value> = trash_items(&root)
        .into_iter()
        .map(|(stamp, rel, size)| json!({"stamp": stamp, "rel": rel, "size": size}))
        .collect();
    Ok(json!({"items": items, "now_s": now_s()}))
}

fn conflicts(node: &Node, id: &str) -> ApiResult {
    node.with_folder(id, |f| {
        let mut items: Vec<Value> = f
            .live()
            .filter_map(|m| {
                let name = m.rel.file_name();
                let (of, _) = name.split_once(".vuoxi-conflict-")?;
                let of = m
                    .rel
                    .parent()
                    .map_or(of.to_owned(), |p| format!("{p}/{of}"));
                Some(json!({"rel": m.rel.as_str(), "of": of, "size": m.meta.size,
                            "mtime_ns": m.meta.mtime_ns}))
            })
            .collect();
        items.sort_by(|a, b| a["rel"].as_str().cmp(&b["rel"].as_str()));
        json!({"items": items})
    })
    .ok_or_else(|| not_found(id))
}

async fn restore(node: &Node, id: &str, r: &Req) -> ApiResult {
    let v: Value = serde_json::from_slice(&r.body).map_err(bad)?;
    let stamp = v
        .get("stamp")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad("stamp"))?;
    let rel = v
        .get("rel")
        .and_then(Value::as_str)
        .and_then(|s| RelPath::new(s).ok())
        .ok_or_else(|| bad("rel must be a relative path"))?;
    let root = node
        .with_folder(id, |f| f.root.clone())
        .ok_or_else(|| not_found(id))?;
    let from = root
        .join(TRASH_DIR)
        .join(stamp.to_string())
        .join(rel.as_str());
    if !from.is_file() {
        return Err(not_found("no such file in the trash"));
    }
    vfs::to_trash(&root, &rel).map_err(|e| bad(e.to_string()))?;
    let to = root.join(rel.as_str());
    if let Some(d) = to.parent() {
        std::fs::create_dir_all(d).map_err(|e| bad(e.to_string()))?;
    }
    std::fs::rename(&from, &to).map_err(|e| bad(e.to_string()))?;
    let _ = node.events().send((id.to_owned(), Event::Changed(rel)));
    Ok(json!({"ok": true}))
}

async fn static_file(sock: &mut TcpStream, dir: &Path, path: &str) -> io::Result<()> {
    let rel = Path::new(path.trim_start_matches('/'));
    let safe = rel.components().all(|c| matches!(c, Component::Normal(_)));
    let mut p = if safe && !path.ends_with('/') && path != "/" {
        dir.join(rel)
    } else {
        dir.join("index.html")
    };
    if !p.is_file() {
        p = dir.join("index.html");
    }
    let Ok(body) = tokio::fs::read(&p).await else {
        let msg = format!(
            "vuoxi: the dashboard is not installed in {} (set VUOXI_UI_DIR)",
            dir.display()
        );
        return respond(sock, "404 Not Found", "text/plain", "", msg.as_bytes()).await;
    };
    let ctype = match p.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    };
    let csp = "Content-Security-Policy: default-src 'self'; style-src 'self' 'unsafe-inline'; \
               frame-ancestors 'none'\r\n";
    respond(sock, "200 OK", ctype, csp, &body).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes() {
        assert_eq!(unescape("a%20b+c%2Fd"), "a b c/d");
        assert_eq!(unescape("100%"), "100%");
        assert_eq!(unescape("%zz"), "%zz");
    }

    #[test]
    fn constant_time_eq() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "abcd"));
    }
}
