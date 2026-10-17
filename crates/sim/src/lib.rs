//! Deterministic scenario tests for vuoxi's sync protocol: partitions,
//! crashes and concurrent edits, replayed from a seed.
//!
//! This crate is a self-contained reference model of the protocol described
//! in `docs/DESIGN.md` ("Storage and reconciliation"). [`replica`] holds the
//! pure rules, [`net`] wires replicas to the real `Request`/`Response`
//! vocabulary and the real `ModeGate`, and [`scenario`] is the seeded
//! scheduler and oracle. The real store and transports plug in where
//! [`replica::Replica`] and the pull in [`net`] are.

#![forbid(unsafe_code)]

pub mod net;
pub mod replica;
pub mod rng;
pub mod scenario;

pub use net::{Sim, Stats, peer_id};
pub use replica::Replica;
pub use scenario::{Config, Op, Report, run};
