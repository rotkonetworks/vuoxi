//! Newline-framed JSON-RPC over a unix socket. One thread per connection; the
//! stack behind it is shared under a mutex, since verbs are short.

use crate::service::{Service, drive};
use crate::types::{ErrorCode, RpcRequest, RpcResponse};
use std::convert::Infallible;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// Longest request line accepted.
pub const MAX_LINE: u64 = 1 << 20;
const IDLE: Duration = Duration::from_secs(60);

/// Default socket: `$XDG_RUNTIME_DIR/vuoxi/rpc.sock`, else under `$HOME`.
pub fn default_socket() -> PathBuf {
    let var = |k| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    match (var("XDG_RUNTIME_DIR"), var("HOME")) {
        (Some(d), _) => d.join("vuoxi/rpc.sock"),
        (None, Some(h)) => h.join(".local/state/vuoxi/rpc.sock"),
        _ => PathBuf::from("vuoxi-rpc.sock"),
    }
}

/// Bind `path` (mode 0600), replacing a stale socket but never a live one.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let listener = match UnixListener::bind(path) {
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).is_ok() {
                return Err(e);
            }
            std::fs::remove_file(path)?;
            UnixListener::bind(path)?
        }
        r => r?,
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// One request line to one response line. `None` for notifications (no id).
pub fn handle_line<S>(svc: &mut S, line: &str) -> Option<String>
where
    S: Service<RpcRequest, Response = RpcResponse, Error = Infallible>,
{
    let resp = match serde_json::from_str::<RpcRequest>(line) {
        Err(e) => RpcResponse::err_with(
            None,
            -32700,
            ErrorCode::InvalidRequest,
            format!("parse: {e}"),
        ),
        Ok(req) if req.jsonrpc != "2.0" => RpcResponse::err_with(
            req.id,
            -32600,
            ErrorCode::InvalidRequest,
            "jsonrpc must be \"2.0\"",
        ),
        Ok(req) => {
            let notification = req.id.is_none();
            let resp = drive(svc, req);
            if notification {
                return None;
            }
            resp
        }
    };
    Some(serde_json::to_string(&resp).expect("responses serialize"))
}

fn connection<S>(stream: UnixStream, svc: &Mutex<S>) -> io::Result<()>
where
    S: Service<RpcRequest, Response = RpcResponse, Error = Infallible>,
{
    stream.set_read_timeout(Some(IDLE))?;
    let mut out = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = (&mut reader)
            .take(MAX_LINE + 1)
            .read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Ok(());
        }
        let reply = if buf.len() as u64 > MAX_LINE {
            let r =
                RpcResponse::err_with(None, -32600, ErrorCode::InvalidRequest, "request too large");
            Some(serde_json::to_string(&r).expect("responses serialize"))
        } else {
            let line = String::from_utf8_lossy(&buf);
            let mut svc = svc.lock().unwrap_or_else(PoisonError::into_inner);
            handle_line(&mut *svc, line.trim())
        };
        if let Some(r) = reply {
            out.write_all(r.as_bytes())?;
            out.write_all(b"\n")?;
        }
        if buf.len() as u64 > MAX_LINE {
            return Ok(());
        }
    }
}

/// Serve forever. `svc` is the whole stack, layers included.
pub fn serve<S>(listener: UnixListener, svc: S) -> io::Result<()>
where
    S: Service<RpcRequest, Response = RpcResponse, Error = Infallible> + Send + 'static,
{
    let svc = Arc::new(Mutex::new(svc));
    for stream in listener.incoming() {
        let stream = stream?;
        let svc = Arc::clone(&svc);
        std::thread::spawn(move || {
            let _ = connection(stream, &svc);
        });
    }
    Ok(())
}
