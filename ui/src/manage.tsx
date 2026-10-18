import { For, Show, createSignal, onCleanup } from 'solid-js'
import { api, TAGS, type Folder, type Offer, type Peer } from './api'

let seq = 0

/** Text input with directory completion from /api/browse (debounced). */
export function PathInput(props: { value: string; onInput: (v: string) => void; placeholder?: string }) {
  const id = 'dl' + seq++
  const [dirs, setDirs] = createSignal<string[]>([])
  let tm: number | undefined
  onCleanup(() => clearTimeout(tm))
  const on = (v: string) => {
    props.onInput(v)
    clearTimeout(tm)
    tm = window.setTimeout(() => api.browse(v).then(r => setDirs(r.dirs), () => setDirs([])), 200)
  }
  return (
    <>
      <input type="text" class="w" list={id} value={props.value} placeholder={props.placeholder}
        onInput={e => on(e.currentTarget.value)} onFocus={e => on(e.currentTarget.value)} />
      <datalist id={id}><For each={dirs()}>{d => <option value={d + '/'} />}</For></datalist>
    </>
  )
}

const ID_RE = /^[a-z0-9-]{1,64}$/

/** Add-folder form (no `edit`) or edit-sharing form (`edit` = existing folder). */
export function FolderForm(props: {
  peers: Peer[]; edit?: Folder; onDone: () => void
  submit: (id: string, path: string, tag: string | null, peers: string[], ro: string[]) => Promise<unknown>
}) {
  const e = props.edit
  const [id, setId] = createSignal(e?.id ?? '')
  const [path, setPath] = createSignal('')
  const [touched, setTouched] = createSignal(false)
  const [tag, setTag] = createSignal(e?.tag ?? '')
  const [sel, setSel] = createSignal<Record<string, 'rw' | 'ro' | undefined>>(
    Object.fromEntries([...(e?.peers ?? []).map(p => [p, 'rw']), ...(e?.read_only_peers ?? []).map(p => [p, 'ro'])]))
  const [err, setErr] = createSignal('')
  const [busy, setBusy] = createSignal(false)
  const set = (p: string, v: 'rw' | 'ro' | undefined) => setSel({ ...sel(), [p]: v })
  const ok = () => (e || (ID_RE.test(id()) && path().trim() !== ''))
  const go = async () => {
    const names = Object.keys(sel()).filter(p => sel()[p])
    setBusy(true); setErr('')
    try {
      await props.submit(id(), path().trim(), tag() || null, names, names.filter(p => sel()[p] === 'ro'))
      props.onDone()
    } catch (x) { setErr(x instanceof Error ? x.message : String(x)); setBusy(false) }
  }
  return (
    <div class="form">
      <Show when={!e}>
        <div class="row"><b>id</b>
          <input type="text" value={id()} placeholder="a-z 0-9 -" maxlength="64" onInput={ev => {
            setId(ev.currentTarget.value); if (!touched()) setPath('~/' + ev.currentTarget.value)
          }} />
          <Show when={id() && !ID_RE.test(id())}><span style="color:#800">only a-z, 0-9, - (max 64)</span></Show>
        </div>
        <div class="row"><b>path</b>
          <PathInput value={path()} placeholder="~/folder" onInput={v => { setPath(v); setTouched(true) }} />
        </div>
      </Show>
      <div class="row"><b>tag</b>
        <select value={tag()} onChange={ev => setTag(ev.currentTarget.value)}>
          <option value="" selected={!tag()}>auto</option>
          <For each={TAGS}>{t => <option value={t} selected={tag() === t}>{t}</option>}</For>
        </select>
      </div>
      <table>
        <thead><tr><th>peer</th><th>share</th><th>read-only</th></tr></thead>
        <tbody>
          <For each={props.peers} fallback={<tr><td colspan="3" class="muted">no peers</td></tr>}>{p =>
            <tr>
              <td>{p.name}{p.relay && <span class="relay">relay</span>}</td>
              <td><input type="checkbox" checked={!!sel()[p.name]} onChange={ev => set(p.name, ev.currentTarget.checked ? 'rw' : undefined)} /></td>
              <td><input type="checkbox" checked={sel()[p.name] === 'ro'} disabled={!sel()[p.name]}
                onChange={ev => set(p.name, ev.currentTarget.checked ? 'ro' : 'rw')} /></td>
            </tr>}
          </For>
        </tbody>
      </table>
      <Show when={err()}><div class="status err">{err()}</div></Show>
      <div class="row">
        <button disabled={!ok() || busy()} onClick={go}>{e ? 'Save' : 'Add folder'}</button>
        <button onClick={props.onDone}>Cancel</button>
      </div>
    </div>
  )
}

export function OfferStrip(props: {
  o: Offer; accept: (path: string) => Promise<unknown>; dismiss: () => Promise<unknown>
}) {
  const [path, setPath] = createSignal(props.o.path)
  const [err, setErr] = createSignal('')
  const run = (f: () => Promise<unknown>) => f().catch(x => setErr(x instanceof Error ? x.message : String(x)))
  return (
    <div class="status warn offer">
      <span><b>{props.o.from}</b> wants to share folder <b>{props.o.folder}</b> ({props.o.tag}) with {props.o.members.join(', ') || 'you'}</span>
      <PathInput value={path()} onInput={setPath} />
      <button onClick={() => run(() => props.accept(path().trim()))} disabled={!path().trim()}>Accept</button>
      <button onClick={() => run(props.dismiss)}>Ignore</button>
      <Show when={err()}><span style="color:#800">{err()}</span></Show>
    </div>
  )
}
