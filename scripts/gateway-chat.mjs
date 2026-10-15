#!/usr/bin/env node
// Chat mode, proven by what the same prompt does on either side of the switch.
//
//   node scripts/gateway-chat.mjs --port 8153 --token CHATOK
//
// The scenario is one the scripted endpoint answers with a tool call. In chat mode
// nothing executes and no card opens; switch back to cowork and the SAME prompt opens
// the card. If chat mode were cosmetic — a label with the tools still mounted — the two
// halves would behave the same, and this fails.
//
// It also asserts `session_state.persona` tracks the switch instead of the UI's memory.
import { DatabaseSync } from 'node:sqlite'
const PORTS = { 8090: 'cwoffee-mock' }
const MOCK = process.env.MOCK_URL ?? 'http://127.0.0.1:8090/v1'
const args = process.argv.slice(2)
const flag = (name, d) => { const i = args.indexOf(name); return i >= 0 && args[i + 1] ? args[i + 1] : d }
const PORT = +flag('--port', '8153')
const TOKEN = flag('--token', 'SEED')
const DB = flag('--db', '')
const ws = new WebSocket(`ws://127.0.0.1:${PORT}/ws`)
let seq = 0
const pending = new Map()
const events = []
let authed
const auth = new Promise((res, rej) => { authed = { res, rej } })
ws.addEventListener('open', () => ws.send(JSON.stringify({ type: 'auth', token: TOKEN })))
ws.addEventListener('message', (e) => {
  const v = JSON.parse(e.data)
  if (v.type === 'auth_ok') return authed.res(v.role ?? 'drive')
  if (v.type === 'auth_error') return authed.rej(new Error(String(v.error)))
  if (v.type === 'event') return events.push(v.d)
  const p = pending.get(v.id)
  if (p) { pending.delete(v.id); p(v) }
})
ws.addEventListener('error', (e) => authed.rej(new Error(e.message ?? 'ws error')))
function call(cmd, a = {}) {
  const id = ++seq
  return new Promise((res, rej) => {
    pending.set(id, (v) => (v.ok ? res(v.data) : rej(new Error(String(v.error)))))
    ws.send(JSON.stringify({ id, cmd, args: a }))
    setTimeout(() => pending.has(id) && (pending.delete(id), rej(new Error('timeout ' + cmd))), 15_000)
  })
}
function seen(pred) { const i = events.findIndex(pred); return i < 0 ? null : events.splice(i, 1)[0] }
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
function wait4(pred, what, ms = 60_000) {
  return new Promise((res, rej) => {
    const t = setInterval(() => { const f = seen(pred); if (f) { clearTimeout(t); res(f) } }, 30)
    const to = setTimeout(() => { clearInterval(t); rej(new Error(`no ${what}`)) }, ms)
    t.on = () => { clearInterval(t); clearTimeout(to) }
  })
}
const fail = (m) => { console.error(`\nCHAT GATE FAILED: ${m}`); process.exit(1) }
const persona = () => call('session_state').then((s) => s?.persona ?? null)

// One turn, and a tally of what the tools did during it.
async function turn(label, text) {
  events.length = 0
  await call('start_turn', { text, session: null })
  // EngineEvent serialises as {kind, d} with camelCase variant names — `turnEnd`,
  // `execEnd`, `approvalRequested`. Matching the Rust spelling on the wire finds
  // nothing and reports a clean run.
  await wait4((d) => d.kind === 'turnEnd', `turnEnd for ${label}`).catch((e) => fail(e.message))
  const cards = events.filter((d) => /approval/i.test(d.kind ?? ''))
  const ran = events.filter((d) => /execEnd/i.test(d.kind ?? '') && (d.d?.ok ?? true))
  console.log(`  ${label}: cards=${cards.length} toolsRan=${ran.length}`)
  return { cards, ran }
}

;(async () => {
  await auth
  if (await persona() !== 'chat') {
    // Not this gateway's boot persona: ask for it, so the gate works against a
    // coworker-started gateway too.
    await call('set_persona', { persona: 'chat' })
    await sleep(400)
  }
  console.log('persona at start:', await persona())
  const TRIGGER = 'Use read_file to read the file /etc/hostname'
  const chat = await turn('chat mode, tool-triggering prompt', TRIGGER)
  if (chat.cards.length) fail(`chat mode opened ${chat.cards.length} approval card(s) — a chat with tools is still the coworker`)
  if (chat.ran.length) fail(`chat mode ran ${chat.ran.length} tool(s): ${JSON.stringify(chat.ran.map((e) => e.d))}`)
  if (!(await persona() === 'chat')) fail('persona did not stay chat after a turn')

  // Switch back. The note is the switch saying what it cost: the process restarts and
  // the prefix cache starts cold.
  events.length = 0
  await call('set_persona', { persona: 'cowork' })
  const note = await wait4((d) => d.kind === 'note' && /mode|tool|folder|cache/.test(JSON.stringify(d.d ?? '')), 'a note about the switch', 10_000)
    .catch(() => null)
  console.log('  switch note:', JSON.stringify(note?.d?.message ?? '(none)'))
  if (!(await persona() === 'cowork')) fail(`persona is ${await persona()} after asking for cowork`)
  const back = await turn('cowork again, same prompt', TRIGGER)
  if (!back.cards.length) fail('the same prompt opened no card after switching back — the switch is cosmetic in one direction')
  await call('respond_approval', { id: back.cards[0].d.id, answer: 'deny' })
  await wait4((d) => d.kind === 'turnEnd', 'turn end after deny').catch(() => {})

  if (DB) {
    const db = new DatabaseSync(DB, { readOnly: true })
    const rows = db.prepare('select role, count(*) n from messages where session_id=1 group by role').all()
    console.log('  stored rows by role:', JSON.stringify(rows))
  }
  console.log('\nCHAT GATE OK (tools are absent in chat, present in cowork, and the state says which)')
  process.exit(0)
})().catch((e) => fail(e.message))
