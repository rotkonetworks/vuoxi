//! The vuoxi RPC surface: JSON-RPC 2.0, newline-framed, over a unix socket.
//!
//! Verbs are plain functions lifted into [`Service`]s; the [`Router`] is itself
//! a Service, so authentication, audit and rate limits are [`Layer`]s that wrap
//! it. No async runtime: every future is `Ready` at construction.

#![forbid(unsafe_code)]

pub mod client;
pub mod server;
pub mod service;
pub mod types;
pub mod verbs;

pub use service::{Layer, Router, Service, drive, handler};
pub use types::{ErrorCode, RpcError, RpcRequest, RpcResponse, SCHEMA};
