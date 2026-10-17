//! The seeded scheduler: a random but reproducible interleaving of edits,
//! partitions, crashes and sync rounds, then a heal phase and the oracle.

use crate::net::{Sim, Stats, peer_id};
use crate::rng::Rng;
use vuoxi_core::PathId;
use vuoxi_layers::Mode;

#[derive(Clone, Debug)]
pub struct Config {
    pub seed: u64,
    pub replicas: usize,
    /// Distinct files; few files means many concurrent edits.
    pub paths: u64,
    pub steps: usize,
    /// Randomly switch replica 0 between power modes (the `ModeGate` scenario).
    pub gate_flips: bool,
}

impl Config {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            replicas: 3 + (seed % 3) as usize,
            paths: 4,
            steps: 120,
            gate_flips: false,
        }
    }

    pub fn with_gate_flips(mut self) -> Self {
        self.gate_flips = true;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Edit {
        at: usize,
        path: PathId,
    },
    Link {
        a: usize,
        b: usize,
        up: bool,
    },
    Isolate(usize),
    HealLinks,
    Crash(usize),
    Sync {
        a: usize,
        b: usize,
    },
    /// `a` pulls from `b` and crashes part way through the batch.
    PullCrash {
        a: usize,
        b: usize,
        after: usize,
    },
    Push {
        from: usize,
        to: usize,
        path: PathId,
    },
    Mode(Mode),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub trace: Vec<Op>,
    pub stats: Stats,
    /// Full pull rounds needed after healing until all replicas agreed.
    pub rounds: usize,
    /// Files that ended with more than one surviving version.
    pub conflicted_paths: usize,
}

fn next_op(rng: &mut Rng, cfg: &Config) -> Op {
    let n = cfg.replicas;
    let path = PathId(rng.below(cfg.paths));
    match rng.below(100) {
        0..25 => Op::Edit {
            at: rng.index(n),
            path,
        },
        25..35 => {
            let (a, b) = rng.pair(n);
            Op::Link {
                a,
                b,
                up: rng.below(2) == 0,
            }
        }
        35..38 => Op::Isolate(rng.index(n)),
        38..43 => Op::HealLinks,
        43..49 => Op::Crash(rng.index(n)),
        49..79 => {
            let (a, b) = rng.pair(n);
            Op::Sync { a, b }
        }
        79..85 => {
            let (a, b) = rng.pair(n);
            Op::PullCrash {
                a,
                b,
                after: rng.index(3),
            }
        }
        85..95 => {
            let (from, to) = rng.pair(n);
            Op::Push { from, to, path }
        }
        _ if cfg.gate_flips => Op::Mode(
            *[Mode::Live, Mode::Batched, Mode::Frugal, Mode::Hold]
                .get(rng.index(4))
                .unwrap(),
        ),
        _ => {
            let (a, b) = rng.pair(n);
            Op::Sync { a, b }
        }
    }
}

fn apply(sim: &mut Sim, op: &Op) {
    match *op {
        Op::Edit { at, path } => {
            sim.edit(at, path);
        }
        Op::Link { a, b, up } => sim.set_link(a, b, up),
        Op::Isolate(i) => sim.isolate(i),
        Op::HealLinks => sim.heal_links(),
        Op::Crash(i) => sim.crash(i),
        Op::Sync { a, b } => sim.sync(a, b),
        Op::PullCrash { a, b, after } => sim.pull_then_crash(a, b, after),
        Op::Push { from, to, path } => sim.push(from, to, path),
        Op::Mode(m) => sim.set_mode(0, m),
    }
}

/// After healing, one all-pairs round must be enough: every replica pulls
/// directly from every other, and a pull returns everything newer than the
/// cursor. A second round is run only to prove nothing was left to find.
const MAX_ROUNDS: usize = 3;

/// Run one seed to the end. `Err` carries the seed and the failing step.
pub fn run(cfg: &Config) -> Result<Report, String> {
    let fail = |why: String, trace: &[Op]| {
        let tail = &trace[trace.len().saturating_sub(8)..];
        format!("seed {}: {why}\n  last ops: {tail:?}", cfg.seed)
    };
    let mut rng = Rng::new(cfg.seed);
    let mut sim = Sim::new(cfg.replicas);
    let mut trace = Vec::with_capacity(cfg.steps);

    for _ in 0..cfg.steps {
        let op = next_op(&mut rng, cfg);
        let before = sim.snapshot();
        apply(&mut sim, &op);
        trace.push(op);
        sim.check_invariants().map_err(|e| fail(e, &trace))?;
        sim.check_monotonic(&before).map_err(|e| fail(e, &trace))?;
    }

    sim.heal();
    let mut rounds = 0;
    while !sim.converged() {
        if rounds == MAX_ROUNDS {
            return Err(fail(
                format!("no convergence after {MAX_ROUNDS} rounds"),
                &trace,
            ));
        }
        sim.sync_round();
        rounds += 1;
    }
    sim.check_invariants().map_err(|e| fail(e, &trace))?;
    sim.check_oracle().map_err(|e| fail(e, &trace))?;

    let conflicted_paths = sim.net().replicas[0].conflict_paths();
    Ok(Report {
        trace,
        stats: sim.stats(),
        rounds,
        conflicted_paths,
    })
}

/// Peer id of replica `i`, for scenarios that need to name one.
pub fn id(i: usize) -> vuoxi_core::PeerId {
    peer_id(i)
}
