// Dev-only fetch shim: enabled with ?mock=1 under `pnpm dev`. Not part of the production bundle.
const NOW = Date.now()
const dev = 'huux…' + '2ola'
const status = {
  device: { id: dev, mode: 'live', watcher: true, uptime_s: 3600 },
  folders: [
    { id: 'notes', root: '/data/notes', tag: 'code', files: 1234, bytes: 56789012, deleted: 17, head: 4021, epoch: '9f2c', root_hash: 'a3b1', peers: ['laptop'], read_only_peers: [], trash_bytes: 1024, conflicts: 1 },
    { id: 'photos', root: '/data/photos', tag: 'media', files: 8421, bytes: 48e9, deleted: 3, head: 900, epoch: '11aa', root_hash: 'b2c3', peers: ['laptop', 'phone'], read_only_peers: ['phone'], trash_bytes: 0, conflicts: 0 },
  ],
  peers: [
    { name: 'laptop', id: '6zvp…quqa', addrs: ['[2001:db8::2]:41641'], relay: false, last_seen_s: 12 },
    { name: 'phone', id: 'k3dm…w9xe', addrs: ['[2001:db8::3]:41641'], relay: true, last_seen_s: 1800 },
    { name: 'old-pc', id: 'p0aa…zz12', addrs: [], relay: false, last_seen_s: null },
  ],
}
const SIZE = 8_000_000
let done = 2_621_440
const t0 = Date.now()
const exts = ['pdf', 'txt', 'md', 'jpg', 'rs']
const allFiles = Array.from({ length: 357 }, (_, i) => ({
  rel: `${i % 13 === 0 ? '' : `dir${i % 7}/`}${i % 13 !== 0 && i % 2 ? `sub${i % 3}/` : ''}file-${String(i).padStart(4, '0')}.${exts[i % 5]}`,
  size: ((i * 7919) % 5_000_000) + 100,
  mtime_ns: (NOW - i * 3_600_000) * 1e6,
  mode: 420, deleted: i % 29 === 0, version: { [dev]: 3, '6zvp…quqa': 1 },
}))
const changes = Array.from({ length: 250 }, (_, i) => ({
  seq: i + 1, rel: allFiles[i % allFiles.length].rel, size: 1000 * (i + 1), deleted: i % 11 === 0,
  mtime_ns: (NOW - (250 - i) * 60_000) * 1e6,
}))
let trash = [
  { stamp: Math.floor(NOW / 1000) - 86400, rel: 'a/b.pdf', size: 1234 },
  { stamp: Math.floor(NOW / 1000) - 7200, rel: 'old/report.txt', size: 99000 },
]

const HOME = '/home/me'
const expand = (p: string) => p.replace(/^~(?=\/|$)/, HOME)
const homeDirs = ['notes', 'notebooks', 'photos', 'projects', 'docs', '.config']
let offers = [{ folder: 'docs', from: 'laptop', tag: 'notes', members: ['laptop', 'phone'], path: HOME + '/docs' }]
let downUntil = 0 // simulated engine restart after a mutation
class HttpErr { constructor(public status: number, public error: string) {} }
const restart = () => { downUntil = Date.now() + 4000; return { ok: true } }
function addFolder(id: string, path: string, tag: string | null, peers: string[], ro: string[]) {
  if (!/^[a-z0-9-]{1,64}$/.test(id)) throw new HttpErr(400, 'invalid folder id')
  if (status.folders.some(f => f.id === id)) throw new HttpErr(409, `folder ${id} already exists`)
  if (ro.some(p => !peers.includes(p))) throw new HttpErr(400, 'ro_peers must be a subset of peers')
  status.folders.push({ id, root: expand(path).replace(/\/$/, ''), tag: tag ?? 'code', files: 0, bytes: 0, deleted: 0, head: 0, epoch: 'ffff', root_hash: '0000', peers, read_only_peers: ro, trash_bytes: 0, conflicts: 0 })
}

function tree(dir: string) {
  const pre = dir ? dir + '/' : ''
  const dirs = new Map<string, { name: string; files: number; bytes: number }>()
  const files: { name: string; size: number; mtime_ns: number; mode: number }[] = []
  for (const f of allFiles) {
    if (f.deleted || !f.rel.startsWith(pre)) continue
    const rest = f.rel.slice(pre.length), i = rest.indexOf('/')
    if (i < 0) files.push({ name: rest, size: f.size, mtime_ns: f.mtime_ns, mode: f.mode })
    else {
      const d = dirs.get(rest.slice(0, i)) ?? { name: rest.slice(0, i), files: 0, bytes: 0 }
      d.files++; d.bytes += f.size; dirs.set(d.name, d)
    }
  }
  const by = (a: { name: string }, b: { name: string }) => (a.name < b.name ? -1 : 1)
  return { dir, dirs: [...dirs.values()].sort(by), files: files.sort(by) }
}

function transfers() {
  done = Math.min(SIZE, done + 180_000)
  if (done >= SIZE && Date.now() - t0 > 1e6) done = 2_621_440
  const active = done < SIZE
  return { items: [
    { id: 17, folder: 'notes', rel: 'a/b.pdf', dir: 'down', peer: 'laptop', size: SIZE, done, state: active ? 'active' : 'done', started_s: Math.floor((Date.now() - t0) / 1000) + 3, error: null },
    { id: 16, folder: 'photos', rel: 'IMG_0042.jpg', dir: 'up', peer: 'phone', size: 4_200_000, done: 4_200_000, state: 'done', started_s: 300, error: null },
    { id: 15, folder: 'photos', rel: 'big/video.mkv', dir: 'down', peer: 'phone', size: 900_000_000, done: 123_000_000, state: 'failed', started_s: 4000, error: 'peer went away' },
  ] }
}

function route(method: string, url: URL, body: any): unknown {
  const p = url.pathname.replace(/^.*\/api\//, '')
  const q = url.searchParams
  if (p === 'status') return status
  if (p === 'transfers') return transfers()
  if (p === 'mode' && method === 'POST') { status.device.mode = body.mode; return { mode: body.mode } }
  if (p === 'sync') return { ok: true }
  if (p === 'offers') return { items: offers }
  if (p === 'offers/dismiss') { offers = offers.filter(o => !(o.folder === body.folder && o.from === body.from)); return { ok: true } }
  if (p === 'offers/accept') {
    const o = offers.find(o => o.folder === body.folder && o.from === body.from)
    if (!o) throw new HttpErr(404, 'no such offer')
    if (!body.path) throw new HttpErr(400, 'path required')
    addFolder(o.folder, body.path, o.tag, [o.from, ...o.members.filter(m => m !== o.from)], [])
    offers = offers.filter(x => x !== o)
    return restart()
  }
  if (p === 'browse') {
    const x = expand(q.get('path') ?? ''), hidden = /\/\.[^/]*$/.test(x)
    const base = x.endsWith('/') ? x : x.slice(0, x.lastIndexOf('/') + 1), pre = x.slice(base.length)
    if (base !== HOME + '/') return { dirs: [] }
    return { dirs: homeDirs.filter(d => d.startsWith(pre) && (hidden || !d.startsWith('.'))).map(d => base + d) }
  }
  if (p === 'folders' && method === 'POST') { addFolder(body.id, body.path, body.tag, body.peers, body.ro_peers); return restart() }
  const m = p.match(/^folders\/([^/]+)\/(.*)$/)
  if (m) {
    const sub = m[2]
    const fo = status.folders.find(f => f.id === m[1])
    if (!fo) throw new HttpErr(404, 'no such folder')
    if (sub === 'tree') return tree(q.get('dir') ?? '')
    if (sub === 'sharing') {
      if ((body.ro_peers as string[]).some(p => !body.peers.includes(p))) throw new HttpErr(400, 'ro_peers must be a subset of peers')
      fo.peers = body.peers; fo.read_only_peers = body.ro_peers; if (body.tag) fo.tag = body.tag
      return restart()
    }
    if (sub === 'remove') { status.folders = status.folders.filter(f => f !== fo); return restart() }
    if (sub === 'files') {
      let l = allFiles.filter(f => (q.get('deleted') === '1' || !f.deleted) && f.rel.toLowerCase().includes((q.get('q') ?? '').toLowerCase()))
      const s = q.get('sort') ?? 'rel'
      const key = s === 'mtime' ? 'mtime_ns' : s
      l = [...l].sort((a: any, b: any) => (a[key] < b[key] ? -1 : a[key] > b[key] ? 1 : 0))
      if (q.get('desc') === '1') l.reverse()
      const o = +(q.get('offset') ?? 0), n = +(q.get('limit') ?? 100)
      return { total: l.length, items: l.slice(o, o + n) }
    }
    if (sub === 'changes') {
      const since = +(q.get('since') ?? 0), n = +(q.get('limit') ?? 100)
      const items = changes.filter(c => c.seq > since).slice(0, n)
      const last = items[items.length - 1]
      return { head: 250, next: last && last.seq < 250 ? last.seq : null, items }
    }
    if (sub === 'trash') return { items: trash }
    if (sub === 'trash/restore') { trash = trash.filter(t => !(t.stamp === body.stamp && t.rel === body.rel)); return { ok: true } }
    if (sub === 'conflicts') return { items: m[1] === 'notes' ? [{ rel: 'x.txt.vuoxi-conflict-1760000000-3e09664e', of: 'x.txt', size: 12, mtime_ns: 1760000000e9 }] : [] }
  }
  return undefined
}

export function install() {
  const real = window.fetch.bind(window)
  window.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url, location.href)
    if (!url.pathname.includes('/api/')) return real(input, init)
    const body = init?.body ? JSON.parse(String(init.body)) : undefined
    await new Promise(r => setTimeout(r, 40))
    if (Date.now() < downUntil) throw new TypeError('Failed to fetch')
    let out: unknown
    try { out = route(init?.method ?? 'GET', url, body) } catch (e) {
      if (e instanceof HttpErr) return new Response(JSON.stringify({ error: e.error }), { status: e.status })
      throw e
    }
    return out === undefined
      ? new Response(JSON.stringify({ error: 'not found' }), { status: 404 })
      : new Response(JSON.stringify(out), { headers: { 'Content-Type': 'application/json' } })
  }
}
