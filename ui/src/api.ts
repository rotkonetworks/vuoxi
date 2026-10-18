export type Mode = 'live' | 'batched' | 'frugal' | 'hold'
export const MODES: Mode[] = ['live', 'batched', 'frugal', 'hold']

export interface Folder {
  id: string; root: string; tag: string; files: number; bytes: number; deleted: number
  head: number; epoch: string; root_hash: string; peers: string[]
  read_only_peers: string[]; trash_bytes: number; conflicts: number
}
export interface Peer { name: string; id: string; addrs: string[]; relay: boolean; last_seen_s: number | null }
export interface Status {
  device: { id: string; mode: Mode; watcher: boolean; uptime_s: number }
  folders: Folder[]; peers: Peer[]
}
export interface Transfer {
  id: number; folder: string; rel: string; dir: 'down' | 'up'; peer: string
  size: number; done: number; state: 'active' | 'done' | 'failed'; started_s: number; error: string | null
}
export interface FileEntry {
  rel: string; size: number; mtime_ns: number; mode: number; deleted: boolean
  version: Record<string, number>
}
export interface FilesPage { total: number; items: FileEntry[] }
export interface Change { seq: number; rel: string; size: number; deleted: boolean; mtime_ns: number }
export interface Changes { head: number; next: number | null; items: Change[] }
export interface TrashItem { stamp: number; rel: string; size: number }
export interface Conflict { rel: string; of: string; size: number; mtime_ns: number }

export interface TreeDir { name: string; files: number; bytes: number }
export interface TreeFile { name: string; size: number; mtime_ns: number; mode: number }
export interface Tree { dir: string; dirs: TreeDir[]; files: TreeFile[] }
export const TAGS = ['code', 'secrets', 'photos', 'media', 'notes', 'archive']
export interface Offer { folder: string; from: string; tag: string; members: string[]; path: string }
export interface AddFolder { id: string; path: string; tag: string | null; peers: string[]; ro_peers: string[] }
export interface Sharing { peers: string[]; ro_peers: string[]; tag?: string | null }

export class ApiError extends Error {
  constructor(public status: number, msg: string) { super(msg) }
}

async function req<T>(path: string, body?: unknown): Promise<T> {
  const post = body !== undefined
  const r = await fetch(path, {
    method: post ? 'POST' : 'GET',
    credentials: 'same-origin',
    headers: post ? { 'X-Vuoxi': '1', 'Content-Type': 'application/json' } : undefined,
    body: post ? JSON.stringify(body) : undefined,
  })
  if (!r.ok) {
    let m = r.statusText
    try { m = (await r.json()).error || m } catch { /* not json */ }
    throw new ApiError(r.status, m)
  }
  return r.json()
}

const f = (id: string) => `api/folders/${encodeURIComponent(id)}`

export interface FilesQuery {
  q?: string; offset?: number; limit?: number
  sort?: 'rel' | 'size' | 'mtime'; desc?: boolean; deleted?: boolean
}

export const api = {
  status: () => req<Status>('api/status'),
  transfers: () => req<{ items: Transfer[] }>('api/transfers'),
  files: (id: string, o: FilesQuery) => {
    const p = new URLSearchParams({
      q: o.q ?? '', offset: String(o.offset ?? 0), limit: String(o.limit ?? 100),
      sort: o.sort ?? 'rel', desc: o.desc ? '1' : '0', deleted: o.deleted ? '1' : '0',
    })
    return req<FilesPage>(`${f(id)}/files?${p}`)
  },
  changes: (id: string, since = 0, limit = 100) =>
    req<Changes>(`${f(id)}/changes?since=${since}&limit=${limit}`),
  trash: (id: string) => req<{ items: TrashItem[] }>(`${f(id)}/trash`),
  restore: (id: string, stamp: number, rel: string) =>
    req<{ ok: boolean }>(`${f(id)}/trash/restore`, { stamp, rel }),
  conflicts: (id: string) => req<{ items: Conflict[] }>(`${f(id)}/conflicts`),
  tree: (id: string, dir: string) => req<Tree>(`${f(id)}/tree?dir=${encodeURIComponent(dir)}`),
  addFolder: (b: AddFolder) => req<{ ok: boolean }>('api/folders', b),
  setSharing: (id: string, b: Sharing) => req<{ ok: boolean }>(`${f(id)}/sharing`, b),
  removeFolder: (id: string) => req<{ ok: boolean }>(`${f(id)}/remove`, {}),
  offers: () => req<{ items: Offer[] }>('api/offers'),
  acceptOffer: (o: { folder: string; from: string; path: string }) => req<{ ok: boolean }>('api/offers/accept', o),
  dismissOffer: (o: { folder: string; from: string }) => req<{ ok: boolean }>('api/offers/dismiss', o),
  browse: (path: string) => req<{ dirs: string[] }>(`api/browse?path=${encodeURIComponent(path)}`),
  setMode: (mode: Mode) => req<{ mode: Mode }>('api/mode', { mode }),
  sync: () => req<{ ok: boolean }>('api/sync', {}),
}
