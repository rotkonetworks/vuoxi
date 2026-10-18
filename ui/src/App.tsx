import {
  For, Show, createEffect, createMemo, createResource, createSignal, on, onCleanup, onMount,
} from 'solid-js'
import { api, ApiError, MODES, type Mode, type Peer, type Status, type Transfer } from './api'
import { ago, bytes, date, dateNs, dateS, shortId } from './fmt'

type Tab = 'transfers' | 'files' | 'changes' | 'trash' | 'conflicts'
const TABS: [Tab, string][] = [
  ['transfers', 'Transfers'], ['files', 'Files'], ['changes', 'Changes'],
  ['trash', 'Trash'], ['conflicts', 'Conflicts'],
]

function errText(e: unknown): string {
  if (e instanceof ApiError && e.status === 401) return 'Open the link printed by `vuoxi ui`'
  return e instanceof Error ? e.message : String(e)
}

export default function App() {
  const [status, setStatus] = createSignal<Status>()
  const [transfers, setTransfers] = createSignal<Transfer[]>([])
  const [err, setErr] = createSignal('')
  const [tab, setTab] = createSignal<Tab>('transfers')
  const [folder, setFolder] = createSignal<string>('') // '' = all

  async function poll() {
    try {
      const [s, t] = await Promise.all([api.status(), api.transfers()])
      setStatus(s); setTransfers(t.items); setErr('')
    } catch (e) { setErr(errText(e)) }
  }
  onMount(() => {
    poll()
    const iv = setInterval(() => { if (document.visibilityState === 'visible') poll() }, 2000)
    const vis = () => { if (document.visibilityState === 'visible') poll() }
    document.addEventListener('visibilitychange', vis)
    onCleanup(() => { clearInterval(iv); document.removeEventListener('visibilitychange', vis) })
  })

  const act = async (f: () => Promise<unknown>) => {
    try { await f(); setErr(''); poll() } catch (e) { setErr(errText(e)) }
  }

  // folder-scoped tabs need a concrete folder
  const fid = createMemo(() => folder() || status()?.folders[0]?.id || '')
  const total = createMemo(() => {
    const fs = status()?.folders ?? []
    return { files: fs.reduce((a, f) => a + f.files, 0), bytes: fs.reduce((a, f) => a + f.bytes, 0) }
  })
  const active = () => transfers().filter(t => t.state === 'active').length

  return (
    <>
      <h1>
        vuoxi
        <small>{status()?.device.id ?? '...'}</small>
        <span class="sp" />
        <label>mode{' '}
          <select value={status()?.device.mode ?? 'live'}
            onChange={e => act(() => api.setMode(e.currentTarget.value as Mode))}>
            <For each={MODES}>{m => <option value={m} selected={status()?.device.mode === m}>{m}</option>}</For>
          </select>
        </label>
        <button onClick={() => act(api.sync)}>Sync now</button>
        <span>watcher: <b>{status() ? (status()!.device.watcher ? 'on' : 'off') : '?'}</b></span>
      </h1>
      <h2>Own devices sync</h2>
      <Show when={err()}><div class="status err">{err()}</div></Show>

      <div class="layout">
        <div class="side">
          <h2>Folders</h2>
          <table>
            <thead><tr><th>id</th><th class="r">files</th><th class="r">size</th></tr></thead>
            <tbody>
              <tr class="click" classList={{ sel: folder() === '' }} onClick={() => setFolder('')}>
                <td>(all)</td><td class="r">{total().files}</td><td class="r">{bytes(total().bytes)}</td>
              </tr>
              <For each={status()?.folders}>{f =>
                <tr class="click" classList={{ sel: folder() === f.id }} onClick={() => setFolder(f.id)}>
                  <td>{f.id} <span class="tag">{f.tag}</span></td>
                  <td class="r">{f.files}</td><td class="r">{bytes(f.bytes)}</td>
                </tr>}
              </For>
            </tbody>
          </table>
          <h2>Peers</h2>
          <table>
            <thead><tr><th>name</th><th>last seen</th></tr></thead>
            <tbody>
              <For each={status()?.peers} fallback={<tr><td colspan="2" class="muted">none</td></tr>}>{p =>
                <tr>
                  <td>{p.name}{p.relay && <span class="relay">relay</span>}<br /><span class="mono muted" title={p.id}>{shortId(p.id)}</span></td>
                  <td><span class={'dot ' + seen(p)} />{ago(p.last_seen_s)}</td>
                </tr>}
              </For>
            </tbody>
          </table>
        </div>

        <div class="main">
          <div class="tabs">
            <For each={TABS}>{([t, label]) =>
              <a classList={{ on: tab() === t }} onClick={() => setTab(t)}>{label}</a>}
            </For>
          </div>
          <Show when={tab() === 'transfers'}><Transfers items={transfers()} folder={folder()} /></Show>
          <Show when={tab() !== 'transfers' && !fid()}><div class="status warn">No folders.</div></Show>
          <Show when={fid()}>
            <Show when={tab() !== 'transfers'}>
              <Show when={!folder()}><div class="status warn">Showing folder <b>{fid()}</b>; select one on the left.</div></Show>
            </Show>
            <Show when={tab() === 'files'}><Files id={fid()} onErr={e => setErr(errText(e))} /></Show>
            <Show when={tab() === 'changes'}><Changes id={fid()} onErr={e => setErr(errText(e))} /></Show>
            <Show when={tab() === 'trash'}><Trash id={fid()} onErr={e => setErr(errText(e))} /></Show>
            <Show when={tab() === 'conflicts'}><Conflicts id={fid()} onErr={e => setErr(errText(e))} /></Show>
          </Show>
        </div>
      </div>

      <div class="statusbar">
        <span>{total().files} files, {bytes(total().bytes)}</span>
        <span>mode: {status()?.device.mode ?? '?'}</span>
        <span>{active()} active transfer{active() === 1 ? '' : 's'}</span>
      </div>
    </>
  )
}

function seen(p: Peer): string {
  const s = p.last_seen_s
  return s == null ? 'r' : s <= 60 ? 'g' : s <= 3600 ? 'y' : 'r'
}

function Transfers(props: { items: Transfer[]; folder: string }) {
  const rows = () => props.items.filter(t => !props.folder || t.folder === props.folder)
  return (
    <table>
      <thead><tr>
        <th>name</th><th>folder</th><th>dir</th><th>peer</th><th class="r">size</th>
        <th>progress</th><th>state</th><th>started</th>
      </tr></thead>
      <tbody>
        <For each={rows()} fallback={<tr><td colspan="8" class="muted">no transfers</td></tr>}>{t => {
          const pct = () => t.size ? Math.min(100, Math.floor(t.done * 100 / t.size)) : (t.state === 'done' ? 100 : 0)
          return (
            <tr>
              <td class="rel" title={t.error ?? ''}>{t.rel}</td><td>{t.folder}</td>
              <td>{t.dir === 'down' ? '↓' : '↑'}</td><td>{t.peer}</td>
              <td class="r">{bytes(t.size)}</td>
              <td><div class={'pb ' + t.state}><i style={{ width: pct() + '%' }} /><b>{pct()}%</b></div></td>
              <td>{t.state}{t.error ? `: ${t.error}` : ''}</td><td>{ago(t.started_s)}</td>
            </tr>)
        }}</For>
      </tbody>
    </table>
  )
}

interface TabProps { id: string; onErr: (e: unknown) => void }

function Files(props: TabProps) {
  const [q, setQ] = createSignal('')
  const [sq, setSq] = createSignal('')
  const [sort, setSort] = createSignal<'rel' | 'size' | 'mtime'>('rel')
  const [desc, setDesc] = createSignal(false)
  const [del, setDel] = createSignal(false)
  const [page, setPage] = createSignal(0)
  const LIM = 100
  let tm: number | undefined
  onCleanup(() => clearTimeout(tm))
  const onQ = (v: string) => { setQ(v); clearTimeout(tm); tm = window.setTimeout(() => { setSq(v); setPage(0) }, 250) }
  createEffect(on(() => props.id, () => setPage(0), { defer: true }))
  const [data] = createResource(
    () => ({ id: props.id, q: sq(), sort: sort(), desc: desc(), deleted: del(), offset: page() * LIM }),
    async k => { try { return await api.files(k.id, { ...k, limit: LIM }) } catch (e) { props.onErr(e); return undefined } },
  )
  const hd = (k: 'rel' | 'size' | 'mtime', label: string, r = false) =>
    <th class={'s' + (r ? ' r' : '')} onClick={() => { if (sort() === k) setDesc(!desc()); else { setSort(k); setDesc(false) } setPage(0) }}>
      {label}{sort() === k ? (desc() ? ' ▼' : ' ▲') : ''}
    </th>
  const total = () => data.latest?.total ?? 0
  return (
    <>
      <div class="bar">
        <input type="text" placeholder="search" value={q()} onInput={e => onQ(e.currentTarget.value)} />
        <label><input type="checkbox" checked={del()} onChange={e => { setDel(e.currentTarget.checked); setPage(0) }} /> show deleted</label>
        <span class="sp" style="flex:1" />
        <span>{total() ? `${page() * LIM + 1}-${Math.min(total(), (page() + 1) * LIM)} of ${total()}` : '0 files'}</span>
        <button disabled={page() === 0} onClick={() => setPage(page() - 1)}>prev</button>
        <button disabled={(page() + 1) * LIM >= total()} onClick={() => setPage(page() + 1)}>next</button>
      </div>
      <table>
        <thead><tr>{hd('rel', 'rel')}{hd('size', 'size', true)}{hd('mtime', 'modified')}</tr></thead>
        <tbody>
          <For each={data.latest?.items} fallback={<tr><td colspan="3" class="muted">{data.loading ? 'loading...' : 'no files'}</td></tr>}>{f =>
            <tr><td class={'rel' + (f.deleted ? ' del' : '')}>{f.rel}</td><td class="r">{bytes(f.size)}</td><td>{dateNs(f.mtime_ns)}</td></tr>}
          </For>
        </tbody>
      </table>
    </>
  )
}

function Changes(props: TabProps) {
  const [items, setItems] = createSignal<import('./api').Change[]>([])
  const [next, setNext] = createSignal<number | null>(null)
  const [head, setHead] = createSignal(0)
  async function load(since: number, reset: boolean) {
    try {
      const r = await api.changes(props.id, since, 100)
      setItems(reset ? r.items : [...items(), ...r.items]); setNext(r.next); setHead(r.head)
    } catch (e) { props.onErr(e) }
  }
  createEffect(on(() => props.id, () => { setItems([]); load(0, true) }))
  return (
    <>
      <div class="bar"><span>journal head: {head()}</span><span class="muted">oldest first</span></div>
      <table>
        <thead><tr><th class="r">seq</th><th>rel</th><th class="r">size</th><th>modified</th><th>op</th></tr></thead>
        <tbody>
          <For each={items()} fallback={<tr><td colspan="5" class="muted">no changes</td></tr>}>{c =>
            <tr><td class="r">{c.seq}</td><td class={'rel' + (c.deleted ? ' del' : '')}>{c.rel}</td>
              <td class="r">{bytes(c.size)}</td><td>{dateNs(c.mtime_ns)}</td><td>{c.deleted ? 'delete' : 'update'}</td></tr>}
          </For>
        </tbody>
      </table>
      <Show when={next() != null}><button onClick={() => load(next()!, false)}>load more</button></Show>
    </>
  )
}

function Trash(props: TabProps) {
  const [data, { refetch }] = createResource(() => props.id,
    async id => { try { return (await api.trash(id)).items } catch (e) { props.onErr(e); return [] } })
  const restore = async (stamp: number, rel: string) => {
    try { await api.restore(props.id, stamp, rel) } catch (e) { props.onErr(e) }
    refetch()
  }
  return (
    <table>
      <thead><tr><th>deleted</th><th>rel</th><th class="r">size</th><th /></tr></thead>
      <tbody>
        <For each={data.latest} fallback={<tr><td colspan="4" class="muted">trash is empty</td></tr>}>{t =>
          <tr><td>{dateS(t.stamp)}</td><td class="rel">{t.rel}</td><td class="r">{bytes(t.size)}</td>
            <td><button onClick={() => restore(t.stamp, t.rel)}>Restore</button></td></tr>}
        </For>
      </tbody>
    </table>
  )
}

function Conflicts(props: TabProps) {
  const [data] = createResource(() => props.id,
    async id => { try { return (await api.conflicts(id)).items } catch (e) { props.onErr(e); return [] } })
  return (
    <table>
      <thead><tr><th>conflict copy</th><th>of</th><th class="r">size</th><th>modified</th></tr></thead>
      <tbody>
        <For each={data.latest} fallback={<tr><td colspan="4" class="muted">no conflicts</td></tr>}>{c =>
          <tr><td class="rel">{c.rel}</td><td class="rel">{c.of}</td><td class="r">{bytes(c.size)}</td>
            <td>{c.mtime_ns ? dateNs(c.mtime_ns) : '-'}</td></tr>}
        </For>
      </tbody>
    </table>
  )
}

void date
