// The tools must work in the folder the conversation lives in.
//
// This gate exists because of a specific recorded failure: a session opened in
// `~/Downloads/cashflow` had its tools pointed at the app's boot folder instead, so the
// model was told one root by its own prompt while the user was looking at another in the
// sidebar. Every read came back refused and the user's transcript says so. The Rust core
// rebuilds its tool context from the session row; a pi process cannot change the cwd of a
// live session, so the bridge has to treat the move as process lifecycle. This script is
// the regression test for that rule, and it runs against whichever core is up.
//
//   node scripts/gateway-workspace.mjs --port 8122 --token DESKTOK \
//        --workspace "$HOME/Downloads/cashflow" --file PROJECT_STATUS.md
import fs from 'node:fs'
const argv = process.argv.slice(2)
const get = (f, d) => {
  const i = argv.indexOf(f)
  return i >= 0 ? argv[i + 1] : d
}
const port = get('--port', '8122')
const token = get('--token', 'DESKTOK')
const workspace = get('--workspace', process.env.HOME + '/cwoffee-workspace')
const file = get('--file', 'README.md')
const turnTimeoutMs = Number(get('--turn-timeout', '180000'))
const abs = `${workspace.replace(/\/$/, '')}/${file}`
// What the tool must bring back. Picking a slice of the real file makes the assertion
// about bytes on disk, not about a model's phrasing.
const want = fs.readFileSync(abs, 'utf8').replace(/\s+/g, ' ').trim().slice(0, 40)
const ws = new WebSocket(`ws://127.0.0.1:${port}/ws`)
const t = (ms) => new Promise((r) => setTimeout(r, ms))
let id = 0
const rpc = (cmd, args = {}) => ws.send(JSON.stringify({ id: ++id, cmd, args }))
let sid = 0
const seen = { tools: 0, notes: [], errors: [], turnEnd: null, cards: 0 }
ws.addEventListener('open', () => ws.send(JSON.stringify({ type: 'auth', token })))
ws.addEventListener('message', (ev) => {
  const v = JSON.parse(ev.data)
  if (v.type === 'auth_ok') rpc('new_session', { title: 'workspace follows the conversation', workspace })
  if (v.ok === true && v.id === 1 && !sid) {
    sid = v.data
    rpc('start_turn', {
      session: sid,
      text: `Use the read_file tool to read the file "${file}" in this folder. Read it — do not guess at its contents, and do not read any other path.`,
    })
  }
  if (v.type === 'event') {
    const e = v.d ?? {}
    const kind = e.kind ?? ''
    const p = e.d ?? e
    if (/approval/i.test(kind)) {
      seen.cards++
      rpc('respond_approval', { id: p.id, answer: 'allow' })
    } else if (/tool/i.test(kind)) seen.tools++
    else if (kind === 'note') seen.notes.push(p.message)
    else if (kind === 'error') seen.errors.push(p.message)
    else if (kind === 'turnEnd') seen.turnEnd = p.finish
  }
})
for (let i = 0; i < turnTimeoutMs / 250 && !seen.turnEnd; i++) await t(250)
ws.send(JSON.stringify({ id: ++id, cmd: 'session_messages', args: { id: sid } }))
let rows = []
await new Promise((res) => {
  ws.addEventListener('message', (ev) => {
    const v = JSON.parse(ev.data)
    if (v.ok === true && v.id === id) {
      rows = v.data?.messages ?? v.data ?? []
      res()
    }
  })
})
// Compare against the *stored contents*, not their JSON encoding: a stored `\n` and an
// escaped quote both defeat any assertion written against the wire form.
const transcript = rows.map((r) => (typeof r?.content === 'string' ? r.content : '')).join('\n')
const fail = []
if (seen.turnEnd == null) fail.push(`the turn did not end within ${turnTimeoutMs} ms`)
if (!seen.tools) fail.push('no tool events: the model never reached for the file')
// The bytes must be there. A polite apology from the model is not a read.
if (!transcript.replace(/\s+/g, ' ').includes(want)) fail.push(`the transcript does not contain the first 40 chars of ${abs}`)
if (!/"ok"\s*:\s*true/.test(transcript)) fail.push('no successful tool read in the transcript')
// A refusal *of this file* is the bug under test. Refusals of other paths (the model
// probing somewhere else) are information, not failure — the confinement is working.
// A refusal *of this file* is the bug under test. Refusals of other paths (the model
// probing somewhere else) are information, not failure — the confinement is working.
const refusals = (transcript.match(/refused/gi) ?? []).length
const refusedThis = (transcript.match(new RegExp('refused[\\s\\S]{0,160}' + file, 'gi')) ?? []).length
if (refusedThis) fail.push(`the tool refused ${file} — that is the workspace-confinement bug`)
console.log(`workspace gate: session ${sid} in ${workspace}; file=${file} cards=${seen.cards} toolEvents=${seen.tools} finish=${seen.turnEnd} refused=${refusals} refusedThis=${refusedThis} rows=${Array.isArray(rows) ? rows.length : '?'}`)
for (const n of seen.notes) console.log(`  note: ${n}`)
for (const e of seen.errors) console.log(`  error: ${e}`)
if (fail.length) {
  for (const f of fail) console.log(`  FAIL ${f}`)
  console.log('WORKSPACE GATE FAILED')
  process.exit(1)
}
console.log('WORKSPACE GATE OK')
ws.close()
process.exit(0)
