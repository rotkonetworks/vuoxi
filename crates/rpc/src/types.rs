//! Wire envelope and verb payloads.
//!
//! TODO: replace with vuoxi-api. The result types below are shaped as that
//! crate's will be (a `kind` tag and `schema` version added by the envelope,
//! never stored in the struct), so the swap is a change of `use` path.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Contract version carried by every result and error.
pub const SCHEMA: u32 = 1;
/// Page size when the caller gives none.
pub const DEFAULT_LIMIT: u64 = 50;
/// Hard cap on a page; larger requests are clamped, not refused.
pub const MAX_LIMIT: u64 = 200;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

impl RpcRequest {
    pub fn new(id: u64, method: &str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(Value::from(id)),
            method: method.into(),
            params,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// JSON-RPC error object. `data.code` is the stable, matchable name.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    pub data: ErrorData,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorData {
    pub kind: String,
    pub schema: u32,
    pub code: ErrorCode,
}

/// Stable error names. Add variants; never rename.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    InvalidRequest,
    MethodNotFound,
    CapabilityDenied,
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::InvalidRequest => "invalid_request",
            Self::MethodNotFound => "method_not_found",
            Self::CapabilityDenied => "capability_denied",
            Self::Internal => "internal",
        }
    }

    /// The JSON-RPC numeric code (standard where one exists).
    pub fn rpc_code(self) -> i32 {
        match self {
            Self::InvalidRequest => -32602,
            Self::MethodNotFound => -32601,
            Self::Internal => -32603,
            Self::NotFound => -32004,
            Self::CapabilityDenied => -32003,
        }
    }

    /// Process exit status for the CLI; 12 (unreachable) is the CLI's own.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::InvalidRequest | Self::MethodNotFound => 2,
            Self::NotFound => 3,
            Self::CapabilityDenied => 4,
            Self::Internal => 1,
        }
    }
}

/// A verb failure: a stable code plus a short human detail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub code: ErrorCode,
    pub detail: String,
}

impl Error {
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl RpcResponse {
    /// Success: stamps `kind` and `schema` onto the result object.
    pub fn ok<T: Serialize + Kind>(id: Option<Value>, body: &T) -> Self {
        let mut v = serde_json::to_value(body).expect("verb results serialize");
        if let Some(o) = v.as_object_mut() {
            o.insert("kind".into(), T::KIND.into());
            o.insert("schema".into(), SCHEMA.into());
        }
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(v),
            error: None,
        }
    }

    pub fn err(id: Option<Value>, code: ErrorCode, detail: impl Into<String>) -> Self {
        Self::err_with(id, code.rpc_code(), code, detail)
    }

    pub fn err_with(
        id: Option<Value>,
        rpc_code: i32,
        code: ErrorCode,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(RpcError {
                code: rpc_code,
                message: detail.into(),
                data: ErrorData {
                    kind: "error".into(),
                    schema: SCHEMA,
                    code,
                },
            }),
        }
    }
}

/// The `kind` tag of a result type.
pub trait Kind {
    const KIND: &'static str;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    /// Journal head: the newest change cursor.
    pub head: u64,
    pub folders: u64,
    pub files: u64,
    pub bytes: u64,
    pub conflicts: u64,
    pub peers: u64,
    pub mode: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Change {
    pub seq: u64,
    /// `create`, `modify` or `delete`.
    pub op: String,
    /// Opaque path id, never a raw path.
    pub path: String,
    pub bytes: u64,
    pub peer: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Changes {
    pub head: u64,
    pub items: Vec<Change>,
    /// Pass as `since` for the next page; null when exhausted.
    pub next_cursor: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Conflict {
    pub seq: u64,
    pub path: String,
    pub ours: String,
    pub theirs: String,
    pub peer: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Conflicts {
    pub items: Vec<Conflict>,
    pub next_cursor: Option<u64>,
}

/// What a plan would do, without doing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Effect {
    pub files: u64,
    pub bytes: u64,
    pub peers: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub effect: Effect,
    /// `pending`, `applied` or `reverted`.
    pub state: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Applied {
    pub plan: String,
    pub effect: Effect,
    /// Journal head after the apply.
    pub head: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reverted {
    pub plan: String,
    /// Journal head after the rollback.
    pub head: u64,
}

macro_rules! kinds {
    ($($t:ty => $k:literal),*) => { $(impl Kind for $t { const KIND: &'static str = $k; })* };
}
kinds!(Status => "status", Changes => "changes", Conflicts => "conflicts",
       Plan => "plan", Applied => "applied", Reverted => "reverted");
