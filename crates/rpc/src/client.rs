//! Blocking client: one connection, one request, one response line.

use crate::types::{RpcRequest, RpcResponse};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

const MAX_RESPONSE: u64 = 16 << 20;

#[derive(Debug)]
pub enum ClientError {
    /// Nothing is listening on the socket.
    Unreachable(io::Error),
    Io(io::Error),
    Protocol(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(e) => write!(f, "cannot connect: {e}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Protocol(m) => write!(f, "protocol: {m}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// Call `method` and return the raw response (which may carry an error).
pub fn call(
    socket: &Path,
    method: &str,
    params: Option<serde_json::Value>,
) -> Result<RpcResponse, ClientError> {
    let stream = UnixStream::connect(socket).map_err(ClientError::Unreachable)?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(ClientError::Io)?;
    let req = RpcRequest::new(1, method, params);
    let mut line = serde_json::to_vec(&req).expect("requests serialize");
    line.push(b'\n');
    (&stream).write_all(&line).map_err(ClientError::Io)?;
    let mut buf = Vec::new();
    BufReader::new(&stream)
        .take(MAX_RESPONSE)
        .read_until(b'\n', &mut buf)
        .map_err(ClientError::Io)?;
    if buf.is_empty() {
        return Err(ClientError::Protocol("connection closed".into()));
    }
    serde_json::from_slice(&buf).map_err(|e| ClientError::Protocol(e.to_string()))
}
