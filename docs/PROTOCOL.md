# vuoxi protocol

What two devices say to each other and what each does with it, as implemented
in `crates/daemon`. The reference model in `crates/sim` follows the same rules.

## Identity

- A **device** is an ed25519 key; its public key is its `PeerId`. Connections
  are QUIC (iroh, ALPN `vuoxi/0`); a peer not in the allowlist is refused during
  the handshake, before any message is parsed.
- A **folder** has an id (`[a-z0-9-]{1,64}`) shared by the devices that sync it.
- A **file** is named on the wire by its `RelPath`: relative, no `.`/`..`/empty
  components, no NUL, at most 4096 bytes. Every decoder re-validates it.
  `PathId = blake3(len(folder) ‖ folder ‖ rel)[..8]`, so all devices derive the
  same id, and a manifest whose `path` does not match its `rel` for a folder is
  not that folder's.

## Manifest

```
path, rel, version (version vector), meta { size, mode & 0o7777, mtime_ns, deleted }, chunks [ChunkId]
```

Chunks are content-defined (FastCDC, 16/64/256 KiB) and named by BLAKE3. A
tombstone has `deleted = true` and no chunks. A rename is a tombstone plus a new
file; its chunks are already present and are not re-sent.

## Journal and epoch

Each device keeps, per folder, an append-only journal of the `PathId`s it has
recorded, numbered from 1. Every recorded manifest — local or received — is
appended, so a device relays what it learned. The journal has an **epoch**, a
random u64 fixed when its store is created.

## Messages

| message | answer | meaning |
|---|---|---|
| `Offer(m)` | none | "this is my newest version of `m.path`" — sent on every local change |
| `Pull { folder, epoch, since, limit }` | `Batch { epoch, head, manifests }` | "your journal after `since`" |
| `Want(chunk)` | `Chunk(chunk, bytes)` or none | one chunk's bytes |

`Batch` lists the current manifest of each path in journal entries
`since+1 ..= head` (deduplicated, at most `limit` entries, `limit ≤ 256`).

## Cursors

A device keeps, per peer and folder, a durable cursor `(epoch, seq)`.

- It pulls with its cursor. If the answer's `epoch` differs, the peer's store
  was recreated: the cursor is meaningless, and the answer already starts at 0.
- It advances the cursor to `head` **only after every manifest in the batch
  was applied**. A manifest that could not be applied now (power mode `Hold`, a
  file still being written) stops the batch; the same entries come again on
  the next pull.

## Applying a peer's manifest

Compare the peer's version with ours:

| ours vs theirs | action |
|---|---|
| no entry | write theirs (or record the tombstone) |
| equal / ahead | nothing |
| behind, same content | record their version |
| behind | write theirs, or delete for a tombstone |
| concurrent, same content | record the merged version |
| concurrent, ours deleted | write theirs with the merged version (an edit beats a delete) |
| concurrent, theirs deleted | keep ours, merged version plus our bump |
| concurrent, both edited | **conflict** |

**Before any of this**, if the file on disk differs from our recorded metadata
(a local edit not yet scanned), it is scanned and versioned first, then the
peer's manifest is planned again.

**Conflicts** are decided the same way on every device: the winner is the
larger `(mtime_ns, chunks, mode)`. The winner stays at the path with a version
dominating both (plus our bump if the winner is ours). The loser is written
beside it as `<name>.vuoxi-conflict-<mtime seconds>-<first chunk, 4 bytes hex>`
with a fresh version; every device names the copy identically, so the copies
merge instead of multiplying.

**Writing** assembles the file from our own chunks where we have them and the
peer's otherwise (each verified against its BLAKE3 name), in a temp file beside
the destination; then fsync, mode, mtime, and an atomic rename. The assembled
size must equal `meta.size`.

## Local changes

A change to a path is scanned outside any lock: if size, mode and mtime equal
the recorded ones, nothing is read (this is also what keeps our own writes from
coming back as new versions). Otherwise the file is chunked; if the chunks and
metadata equal the recorded ones, nothing is versioned. Otherwise our counter is
bumped, the manifest recorded, journaled, and offered to the folder's peers.

## Deferred work

The disk is the source of truth; deferral only delays reading it.

- In `Hold`, local changes are still versioned and journaled but not offered,
  and nothing is pulled or applied. Peers pull the journal when the mode allows.
- Changes the detector missed (watcher down, queue overflow, daemon not
  running) are found by a metadata reconcile on start, on every watcher
  (re)connection, and on overflow; it hashes only files whose metadata changed.
- A pulled manifest that is not applied is not acknowledged: the cursor stays.

## Not synced

Symlinks, devices, sockets, empty directories, our temp files
(`.<name>.vuoxi-tmp`), and whatever the folder's rules skip (see DESIGN.md:
`CACHEDIR.TAG`, build output beside its manifest, `.git`, `.vuoxi` patterns).

## Relays

A relay stores a folder it cannot read (see `crates/daemon/src/seal.rs`). It
knows the folder only by a keyed tag; it stores sealed manifests in an
append-only log and sealed chunks named by a keyed hash. Messages, all scoped by
that tag:

| message | answer | |
|---|---|---|
| `Put{folder, blob}` | `Ack(head)` | append a sealed manifest |
| `Pull{folder, epoch, since, limit}` | `Blobs{epoch, head, blobs}` | read the log, same cursor rules as a peer |
| `Has{folder, ids}` | `Missing(ids)` | which chunks are absent; present ones count as in use |
| `Stash{folder, id, bytes}` | `Ack(0)` | store a sealed chunk |
| `Fetch{folder, id}` | `Chunk(id, bytes)` | read a sealed chunk |
| `Sweep{folder, older_than_s}` | `Ack(removed)` | drop chunks unused for that long |
| `Reset{folder}` | `Ack(epoch)` | empty the log under a new epoch |

A device pulls the relay's log, then pushes its journal since its push cursor:
chunks first (`Has`, then `Stash` for the missing), then the manifest. The push
cursor is stored with the relay epoch it belongs to, so after a `Reset` (or a
relay that lost everything) the device pushes its whole current state again.

**Retention.** The relay cannot tell old data from current, so devices tell it.
At most daily (and every `relay_keep / 4`), a device names every chunk of its
current files in `Has`, re-uploading any the relay lost, then sends
`Sweep{older_than_s: relay_keep}` (`trash_days`, default 30 days). A relay
therefore holds the current files plus up to `relay_keep` of replaced chunks;
a chunk another device stored a moment ago is fresh and survives. When the log
has more than four records per file, the device sends `Reset`; every device
then pushes its current state, and superseded manifests are gone.

## Offering folders

Every pull interval a device probes each device it shares a folder with
(`Pull` with `limit: 0`). A device that lacks the folder answers nothing, and is
sent `Invite{folder, tag, key, members}`: the folder's tag, its relay key and
its other members' ids. The receiver lists it as an offer (never from a relay,
never for a folder it has) until accepted or dismissed; accepting adds the
folder shared with the inviter and the members it knows. The probe stops once
the device serves the folder. The key only travels over the authenticated,
encrypted link between allowlisted devices.
