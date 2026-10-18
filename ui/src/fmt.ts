const U = ['B', 'KiB', 'MiB', 'GiB', 'TiB']
export function bytes(n: number): string {
  let i = 0
  while (n >= 1024 && i < U.length - 1) { n /= 1024; i++ }
  return (i === 0 ? String(n) : n.toFixed(n >= 100 ? 0 : 1)) + ' ' + U[i]
}
export function ago(s: number | null): string {
  if (s == null) return 'never'
  if (s < 5) return 'just now'
  if (s < 60) return `${Math.floor(s)} s ago`
  if (s < 3600) return `${Math.floor(s / 60)} min ago`
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`
  return `${Math.floor(s / 86400)} d ago`
}
const p2 = (n: number) => String(n).padStart(2, '0')
export function date(ms: number): string {
  if (!ms) return '-'
  const d = new Date(ms)
  return `${d.getFullYear()}-${p2(d.getMonth() + 1)}-${p2(d.getDate())} ${p2(d.getHours())}:${p2(d.getMinutes())}`
}
export const dateNs = (ns: number) => date(ns / 1e6)
export const dateS = (s: number) => date(s * 1000)
export const dur = (s: number) => ago(s)

/** First 8 and last 4 characters of a device id; the full id goes in a title. */
export function shortId(id: string): string {
  return id.length > 14 ? `${id.slice(0, 8)}…${id.slice(-4)}` : id;
}
