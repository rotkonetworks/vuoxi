//! Policy as tower layers.
//!
//! A power mode is not a flag checked throughout the code; it is a choice of
//! which layers wrap the engine. Each layer is testable alone and composes with
//! `ServiceBuilder` like any other.

#![forbid(unsafe_code)]

mod budget;
mod clock;
mod coalesce;
mod coalesce_layer;
mod gate;
mod power;
#[cfg(test)]
mod testutil;
mod thermal;

pub use budget::{Budgets, WakeBudget, WakeBudgetLayer};
pub use clock::{Clock, ManualClock, SystemClock};
pub use coalesce::Coalesce;
pub use coalesce_layer::{CoalesceLayer, CoalesceService};
pub use gate::{GateFuture, ModeGate, ModeGateLayer, ModeHandle};
pub use power::{Mode, PowerState};
pub use thermal::{DEFAULT_LIMIT_C, Hwmon, ManualTemp, TempSource, Thermal, ThermalLayer};

#[cfg(test)]
mod compose {
    use super::*;
    use crate::testutil::{Counter, fetch, offer, poll_now, run};
    use std::time::{Duration, Instant};
    use tower_layer::{Layer, Stack};
    use vuoxi_core::{PathId, Request, Response};

    #[test]
    fn deferred_requests_never_reach_the_engine() {
        let t0 = Instant::now();
        let clock = ManualClock::new(t0);
        let mode = ModeHandle::new(Mode::Batched);
        let temp = ManualTemp::new(50.0);
        let engine = Counter::default();

        let budgets = Budgets {
            batched: Some(2),
            ..Budgets::default()
        };
        // Stack::new(inner, outer): innermost first.
        let inner = Stack::new(
            CoalesceLayer::new(Duration::from_secs(30), clock.clone()),
            WakeBudgetLayer::new(mode.clone(), budgets, clock.clone()),
        );
        let layers = Stack::new(
            Stack::new(inner, ThermalLayer::new(temp.clone())),
            ModeGateLayer::new(mode.clone()),
        );
        let mut svc = layers.layer(engine.clone());

        // Offers spend the budget: two reach the engine, the third does not.
        assert_eq!(run(&mut svc, offer()), Response::Done);
        assert_eq!(run(&mut svc, offer()), Response::Done);
        assert_eq!(run(&mut svc, offer()), Response::Deferred);
        assert_eq!(engine.calls(), 2);

        // Hot: heavy work stops at the thermal layer.
        temp.set(95.0);
        assert_eq!(run(&mut svc, fetch()), Response::Deferred);
        assert_eq!(
            run(&mut svc, Request::Changed(PathId(1))),
            Response::Deferred
        );

        // Frugal: the gate refuses Changed before anything is queued.
        temp.set(50.0);
        mode.set(Mode::Frugal);
        assert_eq!(
            run(&mut svc, Request::Changed(PathId(2))),
            Response::Deferred
        );
        assert_eq!(engine.calls(), 2, "nothing deferred reached the engine");
    }

    #[test]
    fn coalesced_batch_reaches_the_engine_once_per_path() {
        let t0 = Instant::now();
        let engine = Counter::default();
        let mut svc = ThermalLayer::new(ManualTemp::new(40.0)).layer(
            CoalesceLayer::new(Duration::from_secs(5), ManualClock::new(t0)).layer(engine.clone()),
        );
        for _ in 0..10 {
            assert_eq!(
                run(&mut svc, Request::Changed(PathId(9))),
                Response::Deferred
            );
        }
        assert_eq!(engine.calls(), 0);
    }

    #[test]
    fn flush_delivers_the_batch_below_the_outer_layers() {
        let t0 = Instant::now();
        let engine = Counter::default();
        let mut co =
            CoalesceLayer::new(Duration::from_secs(5), ManualClock::new(t0)).layer(engine.clone());
        run(&mut co, Request::Changed(PathId(1)));
        run(&mut co, Request::Changed(PathId(1)));
        let futs = co.flush(t0 + Duration::from_secs(5));
        assert_eq!(futs.len(), 1);
        for f in futs {
            assert_eq!(poll_now(f), Response::Done);
        }
        assert_eq!(engine.calls(), 1);
    }
}
