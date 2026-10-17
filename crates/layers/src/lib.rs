//! Policy as tower layers.
//!
//! A power mode is not a flag checked throughout the code; it is a choice of
//! which layers wrap the engine. Each layer is testable alone and composes with
//! `ServiceBuilder` like any other.

#![forbid(unsafe_code)]

mod coalesce;
mod gate;
mod power;

pub use coalesce::Coalesce;
pub use gate::{ModeGate, ModeGateLayer, ModeHandle};
pub use power::{Mode, PowerState};
