# NOMT idle gate

Question (DESIGN.md, open questions): sized for ~2 M keys with one I/O worker,
does NOMT 1.0.5 stay asleep when idle?

**Verdict: pass.** 0.22 context switches per second across the whole process,
10 threads, nothing runnable on a timer.

## Method

`crates/store/examples/nomt_idle.rs` opens a NOMT database under
`target/nomt-idle-db`, commits one batch in a single session, waits 10 s for
commit threads to settle, then sleeps 60 s. It sums `voluntary_ctxt_switches` +
`nonvoluntary_ctxt_switches` over `/proc/self/task/*/status` before and after,
per thread id, so threads that exit in between do not skew the delta.

```
cargo run --release -p vuoxi-store --example nomt_idle -- [buckets] [idle_secs] [keys]
```

Settings: `hashtable_buckets(64_000)` (the run used the default; see sizing
below), `io_workers(1)`, `commit_concurrency(1)`, `preallocate_ht(false)`,
`page_cache_size(32)`, `leaf_cache_size(32)`, rollback off, Blake3 hasher.
Release build, Linux 7.x, io_uring available.

## Results (60 s idle)

| keys committed | threads | switches in 60 s | switches/s |
|---|---|---|---|
| 10 000 | 10 | 13 | 0.217 |
| 2 000 000 | 10 | 13 | 0.217 |

Both runs: 12 switches came from one kernel io_uring worker (`iou-wrk`), 1 from
the example's own main thread waking to sample. The NOMT threads themselves
(`io-worker`, two `beatree-sync`, two `bitbox-sync`, two `nomt-fsyncer-*`,
`nomt-commit`) recorded zero switches. The `iou-wrk` worker's 0.2/s is the
only unexplained wakeup; it has not been attributed further. The optional
drowse cross-check was not run: it attributes wakeups of a live daemon, and
the example exits after printing.

Pool threads that a commit spawns (27-36 right after the commit) are gone
10 s later; steady state is 10.

## Size on disk

| keys | apparent | allocated |
|---|---|---|
| 10 000 | 314 MiB | ~1.6 MiB |
| 200 000 | 314 MiB | 40 MiB |
| 2 000 000 | 538 MiB | 261 MiB |

With `preallocate_ht(false)` the 250 MiB hashtable file is sparse, so a small
folder costs almost nothing on disk. Memory is bounded by the two cache sizes
(32 MiB each here, versus 256 MiB each by default).

## Sizing

Hashtable utilization after committing keys (capacity 64 000 buckets):
10 k keys, 65 buckets; 200 k keys, 4 161; 2 M keys, 4 191. Hashtable buckets
hold the upper trie pages; leaves live in the beatree. 2 M keys need roughly
a tenth of the default, so the store opens with 16 384 buckets. The bucket
count is fixed when the database is created (`grow_hashtable` exists for later).

## Not measured

- Idle behavior while a commit is in flight, or the fsync threads after a
  large commit (all quiet within the 10 s settle used here).
- Steady-state RSS.
- Cold-start cost of reopening a 2 M key database.
