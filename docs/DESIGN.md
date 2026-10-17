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
                                            │    (journal)  (QUIC / iroh)       │
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

The core is generic over `Store`, `Transport` and `Archive`, so tests run the
same engine over in-memory fakes and the simulator.

### Crates

| crate | role |
|---|---|
| `core` | `Request`/`Response`, ids, `VersionVector`, the `Engine`/`Transport`/`Store` seams |
| `layers` | `ModeGate`, `Coalesce`, power state; `WakeBudget`, `Thermal` next |
| `watcher` | privileged fanotify helper |
| `store` | change journal (checksummed append-only file) + manifest index (NOMT) |
| `transport-iroh` | QUIC via iroh: the one transport |
| `archive` | ZODA erasure coding: relay batches (temporary) and the backup tier (durable) |
| `sim` | deterministic reference model: seeded scheduler, partitions, crashes, concurrent edits |

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

## What to sync: reproducibility, not lists

A developer's tree is mostly build output. In one 486-project tree, 265 GB sat
in build directories, and 55 of its 57 Rust `target/` directories carried a
`CACHEDIR.TAG`. vuoxi syncs what cannot be rebuilt and skips what can, with no
configuration in the common case. Rules, first match wins:

1. **`keep` patterns** in a `.vuoxi` file force a path to sync.
2. **`CACHEDIR.TAG`** (the cache-directory standard cargo and others write)
   skips its directory.
3. **Rebuildable next to its manifest:** `target/` beside `Cargo.toml`,
   `node_modules/` beside `package.json`, a venv containing `pyvenv.cfg`,
   `__pycache__`, `.zig-cache`, `.next`, `dist/` beside a JS manifest. A small
   built-in table, extended as tools are met.
4. **`ignore` patterns** in a `.vuoxi` file.
5. **`.git/`** is skipped; tracked source is synced.
6. Everything else syncs.

`.gitignore` is deliberately **not** an exclusion list. With build output
caught by rules 2–3, what remains in it is mostly what cannot be rebuilt:
`.env`, local databases, keys, notes.

A `.vuoxi` file (TOML, gitignore-style patterns) may sit in any directory; the
nearest applies:

```toml
ignore   = ["*.log", "coverage/"]
keep     = [".env", "data/*.db"]
priority = "urgent"   # urgent | normal | bulk (bulk waits for mains)
```

**Visible, not magic.** `vuoxi why <path>` names the rule that decided;
`vuoxi skipped --top 10` lists the largest skipped directories and the bytes
saved. A large gitignored directory that no rule recognises is flagged once,
rather than synced as gigabytes or dropped silently.

**Decided once, filtered at the source.** When the daemon decides a directory
is skipped it hands the directory's handle to the watcher, which then drops its
events before they cross the socket. A `cargo build` writing thousands of files
into `target/` costs nothing beyond the kernel. A new `target/` is recognised
the moment cargo writes its `CACHEDIR.TAG`.

## Tags: what a folder is

A folder carries one tag that sets its defaults, so the common case needs no
`.vuoxi` file. Subdirectories may override (`tag = "secrets"` in a `.vuoxi`),
and any single default can be overridden by name.

| tag | skip rules | `.gitignore` | priority | relays | agents | conflicts | history |
|---|---|---|---|---|---|---|---|
| `code` | build outputs (CACHEDIR.TAG, manifest table) | not an exclusion list | normal | yes | read | keep both | short |
| `secrets` | none | ignored entirely | urgent | **never**; own devices only | **none** unless granted | keep both, alert | long |
| `photos` | thumbnails, caches | n/a | bulk (mains) | yes | none | write-once, no merge | forever |
| `media` | caches | n/a | bulk (mains) | yes | none | write-once | none |
| `notes` | editor swap files | n/a | urgent | yes | read | keep both | long |
| `archive` | none | n/a | bulk | archive tier (ZODA) | none | none, append-only | forever |

The `.gitignore` policy is itself a setting, `gitignore = "keep" | "exclude" |
"ask"`, defaulting per tag as above.

**Chunking follows the tag.** `code`, `notes` and `secrets` use content-defined
chunks (small edits, small transfers). `photos` and `media` hash whole files:
they are written once, and CDC would only cost CPU.

**Detected, then confirmed.** When a folder is added, vuoxi samples it and
proposes a tag instead of applying one silently:

```
$ vuoxi add ~/code
detected: code (382 git repos, 55 cargo target dirs, 265 GB rebuildable)
tag as code? [Y/n]
```

Signals: `.git`, `Cargo.toml`, `package.json` → `code`; KeePass databases,
`.age`/`.gpg` files, SSH keys → `secrets`; `DCIM/`, dense JPEG/HEIC/RAW →
`photos`; video containers → `media`; mostly Markdown/text → `notes`. Mixed or
weak signals ask rather than guess. A `secrets` signal inside a `code` folder
(a stray `.env`, a key) is surfaced, never synced to relays by accident.

**Nested tags.** A subdirectory may carry its own tag; how settings combine
depends on what they protect:

- **Ordinary settings, nearest wins:** priority, chunking, skip rules, history.
  `code` at the top, `photos` in `site/photos`, each gets its own defaults.
- **Safety settings only tighten downward:** relays, agent access, share links.
  A subtree can be more private than its parent, never less: everything under
  `secrets` stays off relays and out of agents' reach even if a deeper `.vuoxi`
  says `tag = "code"`. Loosening takes an explicit `unseal = ["relays"]`, which
  `vuoxi why` and `vuoxi status` always show.
- **Secret-looking files are tagged per file.** `.env`, private keys, KeePass
  databases are `secrets` wherever they sit, including inside `code`.
- **Detection runs per subdirectory** and proposes overrides only where the
  signals clearly differ: "code; 2 subfolders look different: `vault` →
  secrets, `site/photos` → photos".
- **Classification: rules first, a model for the leftovers.** Decisive signals
  (CACHEDIR.TAG, manifests, `.kdbx`, `DCIM/`) are free and offline. Ambiguous
  folders go to [jevx](https://github.com/rotkonetworks/jevx), a typed decision
  model whose calibrated probabilities map onto apply / ask / unknown. Names are
  sensitive, so jevx runs `--local` (a Unix-socket model, no hosted fallback) by
  default; a hosted backend is an explicit per-folder opt-in and is never used
  once a `secrets` signal has been seen. It runs on add and on new
  subdirectories, never per event, and waits for mains on battery. vuoxi calls
  the `jevx` CLI; without it, rules plus asking.
- **Resolved once per directory.** The effective tag is cached under the
  directory handle the watcher already reports, so an event costs one lookup,
  not a walk to the root. `vuoxi why <path>` prints the whole chain.

**Tags are also a vocabulary for agents and sharing.** An agent capability can
say "read `code`, nothing tagged `secrets`" without listing paths, and a share
link refuses any path under a `secrets` tag unless it is explicitly granted.

## Storage and reconciliation

```
journal   per-device change log, numbered 1, 2, 3, …     checksummed append-only file
index     PathId → Manifest; its root is the folder state  NOMT
```

**Normal case: one integer per peer.** Each device numbers its own changes in
its journal. A peer remembers "seen up to 48 211" and asks only for what is
newer. Reconnecting after sleep costs that one number.

**Repair: compare roots.** After a crash, a new device, or any doubt, peers
compare index roots. NOMT is a sparse Merkle trie over hashed keys, so its root
depends only on contents: equal roots mean equal folders, after one message.
Unequal roots are walked down to the differing entries, at a cost proportional
to the differences, not to the 1.8 million files.

**Why NOMT.** One embedded store holds the index *and* produces the root in the
same atomic commit, instead of a key-value store plus a hand-synchronised hash
tree. One Batched burst is one NOMT session and one new root. No RocksDB-style
background compaction. Rollback gives recent history for free.

**Patterns borrowed from cnidarium:** snapshot + copy-on-write overlay committed
atomically (a sync burst lands whole or not at all); a root per folder
(per-folder grants and priorities); verifiable state kept apart from
machine-local bookkeeping; changes pushed to subscribers, never polled.

## One surface: CLI, web, agents

Every query and action is a typed verb on one JSON-RPC surface, as in drowse.
The CLI, the web UI and agents are all thin clients of it; none has a private
path into the daemon.

- **Verbs are services.** The RPC router is a `Service`, so authentication,
  rate limits and audit are layers on it, shared by every client.
- **Read verbs:** folder status, roots, journal tail, conflicts, peer state,
  power mode and wakeup cost.
- **Act verbs follow plan → apply → revert.** `plan` returns the exact effect
  (files, bytes, peers) without doing it; `apply` takes the plan's id; `revert`
  undoes it via NOMT rollback. An agent can propose, a human or a policy can
  approve.
- **Transports:** a unix socket locally, HTTP/WebSocket for the web UI,
  `vuoxi mcp-serve` (stdio MCP) for agents.
- **CLI first, MCP as an adapter.** Agents with a shell use the CLI: it costs
  no context until called, and output can be filtered (`| head`, `| jq`) before
  it reaches the model. `mcp-serve` follows, a handful of tools over the same
  verbs, for agents without a shell and for per-tool approval.
- **One typed contract.** A `vuoxi-api` crate holds every request, response and
  error as serde types. The wire JSON, the JSON Schema (also the MCP tool
  schemas) and the web UI's TypeScript types are generated from it, never
  written by hand. Every value carries `kind` and `schema` (version); errors
  carry a stable `code`. Output defaults to a compact line format, short for
  humans and for model context; `--json` (or `VUOXI_FORMAT=json`) selects JSON.
  Both are renderings of the same typed value, so neither carries anything the
  other lacks. RPC, MCP and the web always speak JSON.
- **Agents are principals, not users borrowing a session.** Each agent holds its
  own capability, attenuated to the folders and verbs it needs, and every act
  is recorded in the journal under its name.
- **Token-efficient by construction.** An agent's context window is a budget
  like a battery: every response is sized for it.
  - Summaries first (counts, sizes, roots); detail on request by id.
  - Deltas, not dumps: verbs take the journal cursor (`since`), so an agent
    that already looked sees only what changed. The same cursor peers use.
  - Every list is paginated and capped; nothing returns 1.8 M rows.
  - Field selection, and a compact line format next to JSON.
  - Few MCP tools with short descriptions, grouped by verb, so the tool list
    itself stays cheap.
- **Browser-capable core.** `core` and `layers` take no async runtime and no
  unsafe, so they compile to WASM; a web client verifies roots and Merkle
  proofs itself instead of trusting the gateway.

## Sharing between parties

Today's peers are one person's devices. Long term, folders are shared with
other people and their agents, and publicly via links.

- **Identity.** A device is a key. A person or an agent is a named set of
  device keys.
- **Capabilities, not ACLs.** A grant is a signed, attenuable token (biscuit
  style): "read `photos/2026`, until Friday, no re-share". Holders can narrow a
  grant before passing it on, never widen it; anyone can verify it offline.
- **Encryption.** Each shared folder has its own content key, wrapped for each
  recipient. A share link carries a read capability and key in the URL
  fragment, which never reaches a server. Revocation rotates the folder key.
- **Reachability.** Off the mesh, peers reach each other through relays (iroh's)
  or a web gateway that sees only ciphertext.

**Where Ligerito fits.** Integrity of an ordinary shared file is a NOMT Merkle
proof plus a signed root; that needs nothing heavier. Ligerito (transparent,
hash-based polynomial commitments with a WASM verifier build) earns its place
where a party must check something about *a lot* of data without fetching it:

- **Archive audits:** a host proves it still holds its ZODA shards, answering
  random challenges with a small proof instead of shipping the data back.
- **Light verifiers:** a browser or an agent checks a claim about a large
  shared dataset from a ~150 KB proof.

It is opt-in, behind the `Archive` seam, never on the sync path. The default
audit is simpler: at upload the writer keeps a few random nonces and
`blake3(nonce ‖ shard)`, and later asks a relay for one.

## Relays: a personal DA layer

Sync between two devices normally needs both awake at once. Relays remove that:
servers with bandwidth but little storage hold recent data for a while, so a
laptop can push once and sleep, and the phone pulls when it wakes.

- **Upload once, erasure-coded.** The writer encrypts, then ZODA-encodes a batch
  into n + k shards and gives each relay a different slice. Any n shards
  rebuild the batch; the writer's upload is (n + k) / n of the data, not one
  copy per relay.
- **Untrusted by construction.** Relays hold ciphertext shards. Each shard is
  checked against the batch commitment on arrival, so a relay can drop data but
  cannot corrupt it unnoticed. Relays never hold a folder key.
- **Why ZODA and not plain Reed-Solomon.** With Reed-Solomon plus a Merkle
  commitment each shard is authentic, but a bad *encoding* only shows after n
  shards are downloaded and decoded, and different subsets can decode to
  different data. ZODA proves from one checked shard that the encoding is valid:
  every recipient will rebuild the same batch, known before the download. This
  matters whenever the encoder is not you (another member of a shared folder,
  their agent, a buggy peer) and stops a writer from equivocating between
  recipients. One code path: every relay batch is ZODA-encoded. Encoding costs
  CPU per batch, so in Batched mode small changes go directly to awake peers and
  only batches over a measured threshold are encoded.
- **The writer sleeps early.** ZODA's property is that one checked shard proves
  the batch was encoded validly. Once enough relays confirm their slices (and a
  few random samples come back), the writer knows the batch is retrievable and
  can sleep: personal data availability.
- **Download from everywhere.** A waking device fetches shards from whichever
  relays and peers answer fastest, in parallel, and stops at n.
- **Temporal by default.** A relay keeps a batch until every intended peer's
  journal cursor has passed it, or until its TTL expires, whichever is first.
  Retention is part of the relay's capability: "opaque blobs, up to 50 GB, up to
  7 days".
- **Audits.** Relays that claim to hold shards can be challenged with
  precomputed nonce/hash pairs; Ligerito proofs are an opt-in for audits by
  third parties.
- **Metadata.** Relays see sizes and timing, not names or contents. Padding
  batches to size classes is an option for folders that need it.

In the type system a relay is a peer with a role, behind the same `Transport`
seam; whether a device or a relay serves a shard is invisible to the engine.

## Introducers

Someone has to tell a new member which peers, relays and roots belong to a
folder.

- **Default: the original seeder.** The device that created a folder signs the
  root of its capability chain and is its introducer.
- **Introducing is a capability.** The seeder can delegate `introduce` like any
  other right, attenuated ("may introduce devices of person X, not other
  people"). Until it does, every introduction traces back to the seeder.
- **Membership is a journal.** Grants, revocations, relay lists and
  introductions are signed entries in a per-folder membership log, synced like
  data. Any member verifies every entry against the chain; an entry from
  someone without the right is ignored, not trusted.
- **More than one introducer** once a folder has several people: any holder of
  `introduce` may add members within their own scope. Policies such as
  "adding a person needs two introducers" are a later layer.
- **Losing the seeder** must not orphan a folder: delegate `introduce` to a
  second device early (the CLI suggests it when a folder gets its second device).

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
- Internet discovery and NAT traversal for one's own devices; the mesh already
  solves addressing. (Sharing with other parties will need relays; see above.)
- Git's job. Source trees travel by git; vuoxi syncs what git does not.
- Non-Linux hosts, for now.

## Open questions

- **Transport (decided):** QUIC via iroh. It covers the mesh (direct dial,
  no discovery or relays by default) and the long-term needs (relays for peers
  off the mesh, sharing with other people). A commonware transport was
  prototyped and kept on a branch; one transport keeps `core` to one message
  model. iroh's QUIC keep-alives cannot be disabled on an open connection, so
  idle connections are closed after a short timeout and redialled on demand.
- **btrfs subvolumes:** the kernel refuses a filesystem-wide fanotify mark on
  a btrfs subvolume (`EXDEV`), the usual layout for `/` and `/home` on btrfs.
  Such devices fall back to a metadata poll (no hashing unless metadata
  changed). Options: per-directory inode marks (inotify-like cost), or one mark
  per subvolume root if a kernel accepts it.
- **Reboot gap on ext4:** nothing records changes while the watcher is down.
  The reconcile pass bounds the cost; filesystems with generation numbers
  (btrfs) could skip it.
- **NOMT at rest:** its defaults preallocate ~256 MB and run 3 io_uring workers
  plus an fsync thread. Sized for ~2 M keys with one worker, do those threads
  stay asleep when idle? First drowse measurement.
- **NOMT tree walk:** repair needs node-by-node access to the trie; the public
  API is built around per-key proofs. May need a small addition upstream.
- **Async runtime:** drowse avoids one to guarantee zero idle wakeups. iroh needs
  tokio. Whether an idle tokio runtime stays at zero wakeups is a drowse
  measurement, not an assumption.

## Milestones

1. Core vocabulary, mode policy, coalescing (this commit).
2. Watcher → journal → NOMT index; replay instead of rescan. Gate: NOMT idle
   wakeups measured with drowse.
3. Two-peer sync over one transport: journal cursors, root-compare repair,
   conflicts in the simulator.
4. Power layers wired to drowse budgets.
5. RPC surface with plan/apply/revert; CLI, `mcp-serve`, then the web UI.
6. Capabilities, per-folder keys, membership journal with the seeder as
   introducer; sharing with another person's devices.
7. Relays: ZODA-encoded batches, retention by cursor/TTL, writer sleeps after
   availability is confirmed.
8. ZODA archive tier; opt-in Ligerito audits.
