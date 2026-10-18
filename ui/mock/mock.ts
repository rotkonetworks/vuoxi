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
  rel: `dir${i % 7}/file-${String(i).padStart(4, '0')}.${exts[i % 5]}`,
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
  const m = p.match(/^folders\/([^/]+)\/(.*)$/)
  if (m) {
    const sub = m[2]
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
    const out = route(init?.method ?? 'GET', url, body)
    await new Promise(r => setTimeout(r, 40))
    return out === undefined
      ? new Response(JSON.stringify({ error: 'not found' }), { status: 404 })
      : new Response(JSON.stringify(out), { headers: { 'Content-Type': 'application/json' } })
  }
}
