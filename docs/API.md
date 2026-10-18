# vuoxi local HTTP API

Served by `vuoxi run` for the dashboard. Bound to `127.0.0.1:41642` only (setting
`ui_listen`). JSON in and out, UTF-8. All sizes in bytes, times in nanoseconds
since the Unix epoch unless named `_s`.

## Auth

A random token is created in the state directory (`ui-token`, mode 0600).
`vuoxi ui` prints `http://127.0.0.1:41642/?t=<token>`. A request to `/?t=<token>`
sets an `HttpOnly; SameSite=Strict` cookie `vuoxi=<token>` and redirects to `/`.
Every `/api/*` request needs that cookie or `Authorization: Bearer <token>`;
otherwise `401 {"error":"unauthorized"}`. `POST` requests also need header
`X-Vuoxi: 1` (blocks cross-site form posts).

Errors: non-2xx with `{"error": "<message>"}`.

## GET /api/status

```json
{
  "device": { "id": "huux…2ola", "mode": "live", "watcher": true, "uptime_s": 3600 },
  "folders": [
    {
      "id": "notes", "root": "/home/me/notes", "tag": "code",
      "files": 1234, "bytes": 56789012, "deleted": 17,
      "head": 4021, "epoch": "9f2c…", "root_hash": "a3b1…",
      "peers": ["laptop"], "read_only_peers": [], "trash_bytes": 1024, "conflicts": 0
    }
  ],
  "peers": [
    { "name": "laptop", "id": "6zvp…quqa", "addrs": ["[2001:db8::2]:41641"],
      "relay": false, "last_seen_s": 12 }
  ]
}
```
`last_seen_s` is seconds since the last successful exchange, `null` if never.

## GET /api/transfers

Like a torrent client's transfer list: in-flight first, then the most recent
completed (at most 200).

```json
{ "items": [
  { "id": 17, "folder": "notes", "rel": "a/b.pdf", "dir": "down", "peer": "laptop",
    "size": 8000000, "done": 2621440, "state": "active", "started_s": 3, "error": null }
] }
```
`dir`: `"down"` (we write a file from a peer) or `"up"` (a peer fetched chunks
from us). `state`: `"active" | "done" | "failed"`. `started_s`: seconds ago.

## GET /api/folders/{id}/files?q=&offset=0&limit=100&sort=rel|size|mtime&desc=0&deleted=0

```json
{ "total": 1234, "items": [
  { "rel": "a/b.pdf", "size": 8000000, "mtime_ns": 1760000000000000000,
    "mode": 420, "deleted": false, "version": { "huux…2ola": 3, "6zvp…quqa": 1 } }
] }
```
`q` is a case-insensitive substring of `rel`. `limit` ≤ 500. `deleted=1`
includes tombstones.

## GET /api/folders/{id}/changes?since=0&limit=100

Journal entries after `since`, oldest first; `next` is the cursor for more or `null`.

```json
{ "head": 4021, "next": null, "items": [
  { "seq": 4020, "rel": "a/b.pdf", "size": 8000000, "deleted": false, "mtime_ns": 1760000000000000000 }
] }
```

## GET /api/folders/{id}/trash

Files a peer replaced or deleted here, newest first.

```json
{ "items": [ { "stamp": 1760000000, "rel": "a/b.pdf", "size": 1234 } ] }
```

## POST /api/folders/{id}/trash/restore

Body `{"stamp": 1760000000, "rel": "a/b.pdf"}`. Moves it back to its path (the
current file, if any, goes to the trash first). `200 {"ok": true}`.

## GET /api/folders/{id}/conflicts

```json
{ "items": [ { "rel": "x.txt.vuoxi-conflict-1760000000-3e09664e", "of": "x.txt", "size": 12, "mtime_ns": 0 } ] }
```

## POST /api/mode

Body `{"mode": "live" | "batched" | "frugal" | "hold"}`. Overrides the
power-derived mode until the next power change or restart. `200 {"mode": "hold"}`.

## POST /api/sync

Pull from every peer now. `200 {"ok": true}`.

## Static files

Everything outside `/api/` is the dashboard, served from the directory in
`VUOXI_UI_DIR` (default `$XDG_DATA_HOME/vuoxi/ui`, i.e. `~/.local/share/vuoxi/ui`),
`index.html` for unknown paths. Build it with `cd ui && pnpm build` and copy
`ui/dist/` there.

`GET /api/folders/{id}/changes` also takes `desc=1`: the newest `limit`
entries, newest first. Transfers currently report downloads only.

## GET /api/folders/{id}/tree?dir=a/b

One directory of the folder's current files (no tombstones): subdirectories
first with totals of everything under them, then files; both sorted by name.
`dir` empty or absent = the folder root.

```json
{ "dir": "a/b",
  "dirs":  [ { "name": "src", "files": 120, "bytes": 834221 } ],
  "files": [ { "name": "README.md", "size": 1200, "mtime_ns": 1760000000000000000, "mode": 420 } ] }
```

## Managing folders

These edit the config file and then restart the sync engine inside the daemon
(a second or two; a metadata walk of every folder). The API is briefly
unavailable during the restart: clients should retry. Each answers
`200 {"ok": true}` before the restart.

- `POST /api/folders` `{"id": "notes", "path": "~/notes", "tag": null | "code" | "secrets" | "photos" | "media" | "notes" | "archive", "peers": ["laptop", "relay"], "ro_peers": []}`.
  `tag: null` detects it from the contents. `path` may start with `~/`; it is
  created if missing. A relay key is generated. `peers` are peer names from
  `/api/status`; `ro_peers` must be a subset of `peers`.
- `POST /api/folders/{id}/sharing` `{"peers": [...], "ro_peers": [...], "tag": optional}`.
- `POST /api/folders/{id}/remove` stops syncing it; the files stay on disk.

## Folder offers (introducing)

A device sharing a folder with a peer that does not have it yet offers it
(with its relay key, over the encrypted device link) every pull interval. The
peer lists the offer until it is accepted or dismissed.

- `GET /api/offers` →
  `{"items": [{"folder": "notes", "from": "laptop", "tag": "code", "members": ["laptop", "relay"], "path": "/home/me/notes"}]}`.
  `members`: the folder's other members by our peer name where we know them
  (relays included), else a short id. `path`: suggested location.
- `POST /api/offers/accept` `{"folder": "notes", "from": "laptop", "path": "~/notes"}`:
  adds the folder shared with the offering device and every member we know.
- `POST /api/offers/dismiss` `{"folder": "notes", "from": "laptop"}`: hide it
  until the daemon restarts.

## GET /api/browse?path=~/no

Directory completion for path fields: if `path` is a directory, its
subdirectories; otherwise the subdirectories of its parent that start with its
last component. Hidden directories only when the prefix starts with `.`.
At most 50.

```json
{ "dirs": ["/home/me/notes", "/home/me/notebooks"] }
```
