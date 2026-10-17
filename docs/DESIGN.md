# vuoxi design

*vuoksi* (Finnish): tide; also the river that drains lake Saimaa. Sync that
flows when there is energy for it and rests when there is not.

## Problem

General-purpose file sync finds changes by looking. Syncthing watches every
directory and, as a safety net, rescans whole folders on a timer and at every
start. On a 1.8 million file working tree that is minutes of hashing and
`lstat` on 1–2 cores at each start and every hour, on every device.

Measured on a 16-core Zen 4 desktop: syncthing's scan threads land on the
preferred cores, holding one CCD at 91 °C while the other idles at 56 °C.
Pinning the same process to the cool CCD dropped the hot one to 58 °C. On a
laptop the same work is battery.

The cost is the design, not the language: a Rust port of the same loop would
run just as hot. vuoxi changes the loop.

## Principles

1. **Be told, never look.** One fanotify filesystem mark reports every change;
   an append-only journal remembers them. No periodic rescans. After a crash or
   reboot gap, one low-priority reconcile pass, rate-limited and off the hot cores.
2. **Energy is a first-class input.** Power state selects which policy layers
   wrap the engine. An idle vuoxi arms no timers.
3. **Services and layers.** Every component is a `tower::Service` over a request
   enum; every policy is a `tower::Layer`. Backpressure is `poll_ready`.
4. **Small privileged surface.** Only the watcher runs with privilege, and it
   has no network and no parser for peer data.
5. **Verify everything.** Chunks are named by BLAKE3 digest and checked before
   they touch disk. Peers name files by opaque id, never by path.

## Architecture

```
            ┌──────────────┐ opaque events  ┌────────────────────────────────────┐
 kernel ──▶ │ watcher      │ ─────────────▶ │ daemon (unprivileged)              │
 fanotify   │ CAP_SYS_ADMIN│  unix socket   │                                    │
            │ no net       │                │  ModeGate ▸ Coalesce ▸ WakeBudget  │
            │ seccomp      │                │  ▸ Thermal ▸ Reconciler            │
            └──────────────┘                │        │            │              │
                                            │      Store      Transport ◀──▶ peers
                                            │    (journal)  (iroh | commonware)  │
                                            │                     │              │
                                            │                  Archive ──▶ hosts │
                                            │                 (ZODA shards)      │
                                            └────────────────────────────────────┘
```

```rust
let engine = ServiceBuilder::new()
    .layer(ModeGateLayer::new(mode.clone()))  // Live / Batched / Frugal / Hold
    .layer(CoalesceLayer::new(window))        // 50 saves of one file → one request
    .layer(WakeBudgetLayer::new(budget))      // refuse work beyond the mode's budget
    .layer(ThermalLayer::new(sensors))        // defer hashing while the CPU is hot
    .service(Reconciler::new(store, transport));
```

The core is generic over `Store`, `Transport` and `Archive`. Swapping iroh for
commonware, or plain replicas for ZODA shards, is a type parameter, so we can
build both and measure instead of deciding in advance.

### Crates

| crate | role |
|---|---|
| `core` | `Request`/`Response`, ids, `VersionVector`, the `Engine`/`Transport`/`Store` seams |
| `layers` | `ModeGate`, `Coalesce`, power state; `WakeBudget`, `Thermal` next |
| `watcher` | privileged fanotify helper |
| `store` | append-only change journal + manifest index (commonware `storage::journal`) |
| `transport-iroh` | QUIC via iroh, `iroh-blobs` for verified BLAKE3 transfer |
| `transport-commonware` | `p2p::authenticated` among a fixed set of keyed peers |
| `archive` | ZODA erasure-coded backup tier |
| `sim` | deterministic scenarios on the commonware runtime |

## Power modes

| mode | when | behaviour |
|---|---|---|
| Live | on mains | sync within seconds |
| Batched | on battery | coalesce, one burst per window, ride existing radio wakeups |
| Frugal | battery < 20 % | metadata and urgent folders only; bulk waits for mains |
| Hold | paused / lid closed | journal changes, hash and send nothing |

Each mode carries a wakeup budget. drowse referees it: its `sleep_efficiency`
and `wakeup_offenders` verbs (JSON-RPC, or `drowse mcp-serve`) measure vuoxi's
package-sleep breaks, and CI fails a mode that exceeds its budget, the way
drowse already tests its own footprint.

## Data model

- A file is a `Manifest`: opaque `PathId`, a `VersionVector`, and an ordered
  list of `ChunkId`s.
- Chunks are content-defined (FastCDC) and named by BLAKE3, so an edit that
  shifts bytes resends only the chunks it touched.
- `VersionVector::compare` yields Equal, Behind, Ahead or Concurrent. Concurrent
  edits keep both versions; vuoxi never silently overwrites.

## Security

**Privilege separation.** The watcher is the only privileged process: ~200
lines, fanotify in, opaque records out over a unix socket. It drops every
capability but `CAP_SYS_ADMIN` before its loop, then installs a seccomp filter
for the syscalls that loop uses. It never opens a network socket and never
parses peer data. Unsafe code is confined to its fanotify bindings; every other
crate is `#![forbid(unsafe_code)]`.

**Peers.** Devices are identified by public key and connect over an existing
WireGuard mesh with static addresses, so there is no discovery server and no
relay. A newly paired device can read nothing until folders are granted to it.

**Data.** Everything received is verified against its BLAKE3 name before it is
written. Paths are resolved locally from opaque ids, so a peer cannot express
`../`. Dependencies are few, pinned, and audited with `cargo-deny`.

**Archive.** ZODA shards are checkable on arrival, so an archive host that is
compromised cannot return corrupted data unnoticed.

## Non-goals

- Phone sync. Phones keep using syncthing; vuoxi may speak BEP later.
- Internet discovery and NAT traversal. The mesh already solves addressing.
- Git's job. Source trees travel by git; vuoxi syncs what git does not.
- Non-Linux hosts, for now.

## Open questions

- **Transport:** iroh ships verified blob transfer; commonware brings the
  deterministic simulator and a peer model that matches a fixed mesh. Both sit
  behind `Transport`; measure before choosing.
- **Reboot gap on ext4:** nothing records changes while the watcher is down.
  The reconcile pass bounds the cost; filesystems with generation numbers
  (btrfs) could skip it.
- **Async runtime:** drowse avoids one to guarantee zero idle wakeups. iroh needs
  tokio. Whether an idle tokio runtime stays at zero wakeups is a drowse
  measurement, not an assumption.

## Milestones

1. Core vocabulary, mode policy, coalescing (this commit).
2. Watcher → journal: fanotify events land in the store; replay instead of rescan.
3. Two-peer sync over one transport, conflicts in the simulator.
4. Power layers wired to drowse budgets.
5. Second transport, then the ZODA archive tier.
