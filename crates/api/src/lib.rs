//! The typed contract of vuoxi.
//!
//! Every request, response and error that crosses the RPC, CLI, MCP or web
//! boundary is a type here. Wire JSON, JSON Schema and the web UI's types are
//! generated from them. Every top-level value is an [`Envelope`] carrying
//! `kind` and `schema`; errors carry a stable [`ErrorCode`].
//!
//! Evolution rule: adding a field or an optional variant of a nested enum is
//! non-breaking (readers ignore unknown fields, new fields have defaults);
//! anything else bumps [`SCHEMA`].
//!
//! Identifiers are plain integers and lowercase hex strings. Raw paths never
//! appear: files are named by opaque id, as everywhere else in vuoxi.

#![forbid(unsafe_code)]

mod act;
mod error;
mod message;
mod page;
mod read;
mod render;

pub use act::{Action, Applied, Effect, Keep, Plan, PlanId, PlanState, Reverted};
pub use error::{ApiError, ErrorCode};
pub use message::{Envelope, Request, Response};
pub use page::{DEFAULT_LIMIT, MAX_LIMIT, Page, resolve_limit};
pub use read::{
    ChangeEntry, ChangeOp, ChangePage, Conflict, FolderId, Peer, PeerRole, PeerState, PowerMode,
    Side, Status,
};
pub use render::Render;

/// Wire schema version carried by every [`Envelope`].
pub const SCHEMA: u32 = 1;

/// Lowercase hex of a 32-byte id (roots, peers, chunks).
pub fn hex32(bytes: &[u8; 32]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}
