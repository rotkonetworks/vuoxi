# vuoxi

File sync that is told what changed instead of looking for it, and that knows
whether it is running on mains, battery, or a hot CPU.

Syncthing rescans whole folders at start and every hour; on a large tree that
is minutes of hashing on every device. vuoxi listens to one fanotify mark,
journals every change, and never rescans. Power state chooses which policy
layers wrap the engine: Live on mains, Batched on battery, Frugal when low,
Hold when paused.

**Status:** design and core types. Not usable yet.

- Architecture, power modes, security model: [docs/DESIGN.md](docs/DESIGN.md)
- Every component is a `tower::Service`, every policy a `tower::Layer`.
- Only the watcher runs privileged; everything else is `#![forbid(unsafe_code)]`.

```
cargo test --workspace
```

Needs Rust 1.88+.

## License

MIT OR Apache-2.0.
