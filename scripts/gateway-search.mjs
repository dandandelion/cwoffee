// The search panel has to answer questions, not recite a placeholder.
//
// A pi-backed app showed "no container runtime" and "this bridge does not mount the tool
// extensions yet" on a box with podman installed, while a search through the tool host
// started the managed container and answered fine. The panel was not lying on purpose: it
// had simply never been connected to the code that knows. This gate asks the panel what it
// knows and rejects the placeholders.
//
//   node scripts/gateway-search.mjs --port 8151 --token T
const argv = process.argv.slice(2)
const get = (f, d) => {
  const i = argv.indexOf(f)
  return i >= 0 ? argv[i + 1] : d
}
const port = get('--port', '8151')
const token = get('--token', 'T')
const ws = new WebSocket(`ws://127.0.0.1:${port}/ws`)
const t = (ms) => new Promise((r) => setTimeout(r, ms))
let id = 0
const rpc = (cmd, args = {}) => ws.send(JSON.stringify({ id: ++id, cmd, args }))
const replies = new Map()
ws.addEventListener('open', () => ws.send(JSON.stringify({ type: 'auth', token })))
ws.addEventListener('message', (ev) => {
  const v = JSON.parse(ev.data)
  if (v.ok === true) replies.set(v.id, v.data)
  if (v.ok === false) replies.set(v.id, { error: v.error })
  if (v.type === 'auth_ok') {
    rpc('search_settings', {})
    rpc('search_probe', { query: 'coffee roasting' })
  }
})
for (let i = 0; i < 200 && !(replies.has(1) && replies.has(2)); i++) await t(100)
const view = replies.get(1) ?? {}
const probe = replies.get(2) ?? {}
const fail = []
const stale = /does not mount|not a live tool|Phase 2/i
if (stale.test(view.describing ?? '')) fail.push(`the panel still recites a placeholder: ${view.describing}`)
if (!view.backend) fail.push('the panel does not say which backend is chosen')
if (!view.runtime) fail.push(`runtime is unknown on a box where the tool can start a container (runtime: ${JSON.stringify(view.runtime)})`)
if (view.backend === 'auto' && !view.auto) fail.push('the managed backend is chosen but the panel reports no status for it')
if (probe.error) fail.push(`the probe failed: ${probe.error}`)
if (!(probe.results ?? []).length) fail.push('the probe returned no results: it is not reaching a provider')
console.log(`search panel: backend=${view.backend} runtime=${view.runtime} describing="${view.describing}"`)
if (view.auto) console.log(`  managed: up=${view.auto.up} exists=${view.auto.container_exists} base=${view.auto.base}`)
if (probe.ms != null) console.log(`  probe: ${probe.ms}ms via "${probe.backend}" hits=${(probe.results ?? []).length} note="${probe.note ?? ''}"`)
for (const f of fail) console.log(`  FAIL ${f}`)
console.log(fail.length ? 'SEARCH PANEL FAILED' : 'SEARCH PANEL OK')
ws.close()
process.exit(fail.length ? 1 : 0)
