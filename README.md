# vuoxi

File sync that is told what changed instead of looking for it, and that knows
whether it is running on mains, battery, or a hot CPU.

Syncthing rescans whole folders at start and every hour; on a large tree that
is minutes of hashing on every device. vuoxi listens to one fanotify mark,
journals every change, and never rescans. Power state chooses which policy
layers wrap the engine: Live on mains, Batched on battery, Frugal when low,
Hold when paused.

**Status:** syncs a folder between devices (milestones 2–3): fanotify watcher
or metadata poll, journal + NOMT index, QUIC via iroh, atomic writes with mode
and mtime, tombstones, renames, deterministic conflicts, restart catch-up. In
trial; not yet a syncthing replacement for anything you cannot lose.

```
vuoxi init                                   # device key + config, prints the id
vuoxi pair laptop <its id> [2001:db8::2]:41641
vuoxi folder add notes ~/notes --peer laptop
vuoxi run                                    # or the units in packaging/
vuoxi why ~/notes/target/x.o                 # which rule decided
```

- Architecture, power modes, security model: [docs/DESIGN.md](docs/DESIGN.md)
- Wire protocol and apply rules: [docs/PROTOCOL.md](docs/PROTOCOL.md)
- Every component is a `tower::Service`, every policy a `tower::Layer`.
- Only the watcher runs privileged; everything else is `#![forbid(unsafe_code)]`.

```
cargo test --workspace
```

Needs Rust 1.88+.

## License

MIT OR Apache-2.0.
