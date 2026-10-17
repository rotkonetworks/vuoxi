use vuoxi_core::{PathId, Request, Response};
use vuoxi_layers::Mode;
use vuoxi_sim::{Config, Sim, peer_id, run};

const P: PathId = PathId(1);
const SEEDS: u64 = 200;

fn must(cfg: Config) -> vuoxi_sim::Report {
    run(&cfg).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn converges_over_many_seeds() {
    let (mut conflicted, mut crashes, mut mid, mut deferred_none) = (0, 0, 0, 0);
    let mut max_rounds = 0;
    for seed in 0..SEEDS {
        let r = must(Config::new(seed));
        conflicted += (r.conflicted_paths > 0) as u64;
        crashes += r.stats.crashes;
        mid += r.stats.mid_transfer_crashes;
        deferred_none += r.stats.deferred;
        max_rounds = max_rounds.max(r.rounds);
    }
    // The runs must actually exercise the hard cases, not pass vacuously.
    assert!(
        conflicted > SEEDS / 5,
        "only {conflicted} seeds ended in conflict"
    );
    assert!(
        crashes > SEEDS && mid > 20,
        "crashes {crashes}, mid-transfer {mid}"
    );
    assert_eq!(
        deferred_none, 0,
        "no gate flips configured, nothing may defer"
    );
    // One all-pairs round after healing is always enough.
    assert!(max_rounds <= 1, "needed {max_rounds} rounds");
}

#[test]
fn converges_with_a_gated_replica() {
    let (mut deferred, mut coalesced) = (0, 0);
    for seed in 0..SEEDS {
        let r = must(Config::new(seed).with_gate_flips());
        deferred += r.stats.deferred;
        coalesced += r.stats.coalesced;
    }
    assert!(
        deferred > SEEDS,
        "gate never deferred anything ({deferred})"
    );
    assert!(coalesced > 0, "no parked edits were coalesced");
}

#[test]
fn runs_are_deterministic() {
    for seed in [0, 1, 42, 9999] {
        assert_eq!(must(Config::new(seed)), must(Config::new(seed)));
    }
    assert_ne!(must(Config::new(1)).trace, must(Config::new(2)).trace);
}

#[test]
fn concurrent_edits_surface_as_conflicts_and_editing_resolves() {
    let mut sim = Sim::new(2);
    sim.edit(0, P);
    sim.sync(0, 1);
    assert!(sim.converged());

    // Partition; both sides edit the same file.
    sim.set_link(0, 1, false);
    sim.edit(0, P);
    sim.edit(1, P);
    assert!(sim.pull(0, 1).is_err(), "link is down");

    sim.heal_links();
    sim.sync(0, 1);
    assert!(sim.converged());
    {
        let net = sim.net();
        for r in &net.replicas {
            assert_eq!(r.heads()[&P].len(), 2, "both versions kept");
            assert_eq!(r.conflict_log().len(), 1, "each side detected it once");
        }
        // Same visible version everywhere.
        assert_eq!(net.replicas[0].manifest(P), net.replicas[1].manifest(P));
    }
    assert_eq!(sim.stats().conflicts_reported, 2);
    sim.check_oracle().unwrap();

    // An edit by a device that has seen both versions resolves the conflict.
    sim.edit(1, P);
    sim.sync(0, 1);
    let net = sim.net();
    assert!(net.replicas.iter().all(|r| r.heads()[&P].len() == 1));
    drop(net);
    sim.check_oracle().unwrap();
}

#[test]
fn partition_heals_through_a_relay_replica() {
    // 0 and 2 never talk directly; 1 carries 0's edit to 2.
    let mut sim = Sim::new(3);
    sim.set_link(0, 2, false);
    sim.edit(0, P);
    sim.sync(0, 1);
    sim.sync(1, 2);
    assert_eq!(sim.net().replicas[2].heads()[&P].len(), 1);
    sim.heal_links();
    sim.sync_round();
    assert!(sim.converged());
    sim.check_oracle().unwrap();
}

#[test]
fn cursor_sync_after_reconnect_sends_only_newer_entries() {
    let mut sim = Sim::new(2);
    for p in 0..100 {
        sim.edit(0, PathId(p));
    }
    sim.sync(0, 1);
    assert_eq!(
        sim.stats().transferred,
        100,
        "first sync is the whole journal"
    );
    assert_eq!(sim.net().replicas[1].cursor(peer_id(0)), 100);

    // No change: a pull costs nothing but the cursor.
    let before = sim.stats().transferred;
    sim.sync(0, 1);
    assert_eq!(
        sim.stats().transferred,
        before,
        "idle reconnect transfers nothing"
    );

    // Partition, three edits, reconnect: exactly three manifests cross.
    sim.set_link(0, 1, false);
    for p in [3, 7, 7, 50] {
        sim.edit(0, PathId(p));
    }
    sim.heal_links();
    sim.sync(0, 1);
    assert_eq!(
        sim.stats().transferred - before,
        3,
        "three distinct paths changed"
    );
    assert!(sim.converged());
    sim.check_oracle().unwrap();
}

#[test]
fn crash_loses_cursors_not_data_and_resend_is_idempotent() {
    let mut sim = Sim::new(2);
    for p in 0..10 {
        sim.edit(0, PathId(p));
    }
    sim.sync(0, 1);
    let state = sim.snapshot();

    sim.crash(1);
    assert_eq!(sim.net().replicas[1].cursor(peer_id(0)), 0);
    assert_eq!(sim.snapshot(), state, "the index is durable");

    let before = sim.stats().transferred;
    sim.sync(0, 1);
    assert_eq!(
        sim.stats().transferred - before,
        10,
        "no cursor: full journal again"
    );
    assert_eq!(
        sim.snapshot(),
        state,
        "replaying the journal changes nothing"
    );
}

#[test]
fn crash_mid_transfer_repeats_work_instead_of_skipping_it() {
    let mut sim = Sim::new(2);
    for p in 0..6 {
        sim.edit(0, PathId(p));
    }
    sim.pull_then_crash(1, 0, 3);
    {
        let net = sim.net();
        assert_eq!(net.replicas[1].heads().len(), 3, "half applied");
        assert_eq!(
            net.replicas[1].cursor(peer_id(0)),
            0,
            "cursor never advanced"
        );
    }
    sim.sync(0, 1);
    assert!(sim.converged());
    sim.check_oracle().unwrap();
}

#[test]
fn hold_defers_requests_and_replica_converges_after_live() {
    let mut sim = Sim::new(3);
    sim.edit(1, PathId(1));
    sim.sync_round();

    sim.set_mode(0, Mode::Hold);
    // Its own change is deferred, not applied, not lost.
    assert_eq!(sim.edit(0, PathId(2)), Response::Deferred);
    assert_eq!(sim.edit(0, PathId(2)), Response::Deferred, "coalesces");
    assert!(sim.net().replicas[0].manifest(PathId(2)).is_none());
    // It neither pulls nor answers pulls.
    assert_eq!(sim.pull(0, 1).unwrap(), Response::Deferred);
    assert!(sim.pull(1, 0).is_err());
    // Offers are deferred too (Hold is stricter than Frugal).
    let m = sim.net().replicas[1].manifest(PathId(1)).unwrap().clone();
    assert_eq!(
        sim.request(0, Request::Offer(peer_id(1), m)).unwrap(),
        Response::Deferred
    );
    // Meanwhile the others carry on and diverge from it.
    sim.edit(1, PathId(1));
    sim.sync(1, 2);
    sim.sync_round();
    assert!(!sim.converged());
    assert!(
        sim.check_oracle().is_err(),
        "parked work is not yet in the oracle's state"
    );

    // Frugal: metadata (Offer) flows, bulk work (Changed, Reconcile) waits.
    sim.set_mode(0, Mode::Frugal);
    assert_eq!(sim.net().parked[0].len(), 1, "still parked");
    let m = sim.net().replicas[1].manifest(PathId(1)).unwrap().clone();
    assert_eq!(
        sim.request(0, Request::Offer(peer_id(1), m)).unwrap(),
        Response::Done
    );
    assert_eq!(sim.pull(0, 1).unwrap(), Response::Deferred);

    // Back to Live: the parked edit is replayed and everyone converges.
    sim.set_mode(0, Mode::Live);
    assert!(sim.net().parked[0].is_empty());
    assert!(sim.net().replicas[0].manifest(PathId(2)).is_some());
    sim.sync_round();
    assert!(sim.converged());
    sim.check_oracle().unwrap();
    assert!(sim.stats().deferred >= 5);
}
