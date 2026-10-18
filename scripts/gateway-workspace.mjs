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

// The gate needs a file the tool can be asked to read. Seed one rather than reaching
// into somebody's real folder for a file that may not be there: a gate that dies on a
// missing README (an ENOENT stack, no verdict) reads as an app failure when it is a
// missing scratch directory. Same for the folder itself.
fs.mkdirSync(workspace, { recursive: true })
if (!fs.existsSync(abs)) {
  fs.writeFileSync(abs, 'Cwoffee workspace-gate fixture.\nThis file exists so read_file has something to read.\n')
  console.log(`  note: wrote ${abs} — the gate's own fixture, nothing of yours`)
}
// What the tool must bring back. Compare on whole *lines*, because the engine's
// read_file numbers them ("1\t# BESPOT — working checkpoint") and paginates long files:
// a raw 40-character window of the file straddles a line number and fails for the wrong
// reason. Two lines, one near the top and one past the fold, so a single first page is
// not mistaken for having read the file.
const text = fs.readFileSync(abs, 'utf8').split('\n')
const nonEmpty = text.map((l) => l.trim()).filter(Boolean)
// Compare a bounded slice: a very long line can be clipped by the tool's own byte
// budget, and demanding the whole thing fails on the tool being careful.
const want = [nonEmpty[0].slice(0, 60), nonEmpty[Math.floor(nonEmpty.length * 0.6)].slice(0, 60)]
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
const stored = rows.map((r) => (typeof r?.content === 'string' ? r.content : '')).join('\n')
// The tool's answer is stored as a JSON envelope, so its newlines and quotes arrive
// escaped. Match against the unescaped text, or a line containing a quotation mark
// "is not in the transcript" when it plainly is.
const transcript = stored.replace(/\\(["\\/nb])/g, '$1')
const fail = []
if (seen.turnEnd == null) fail.push(`the turn did not end within ${turnTimeoutMs} ms`)
if (!seen.tools) fail.push('no tool events: the model never reached for the file')
// The bytes must be there. A polite apology from the model is not a read.
for (const w of want) {
  if (!transcript.includes(w)) fail.push(`the transcript does not contain the line "${w.slice(0, 60)}…" from ${abs}`)
}
if (!/showing \d+/.test(transcript)) fail.push('no line-count report in the read: the tool answered without describing what it showed')
// Success, in the shape the transcript actually holds. A successful read stores the
// engine's own model-facing text — `N\tline…` plus a "showing N of M lines" note, both
// asserted above; a failed one starts `ERROR (exit …)`. An earlier version looked for
// `"ok": true`, which is a property of the JSON envelope: a wire shape on one core, not
// a stored fact. Once the tool's own words became the model-facing text that check was
// asserting nothing about the read, and it failed a run where the read had plainly
// worked (gotchas 55, 71, 77).
const errLine = transcript.match(/ERROR \(exit[^\n]{0,140}/)
if (errLine) fail.push(`the stored tool read is an error, not a read: ${errLine[0]}`)
// A refusal *of this file* is the bug under test. Refusals of other paths (the model
// probing somewhere else) are information, not failure — the confinement is working.
const refusals = (transcript.match(/refused/gi) ?? []).length
// Name the path the refusal actually names. A window of N characters around the file
// name matched prose in the model's own summary ("...refused to guess; CHECKPOINT.md
// says...") and failed a run where every read of the file succeeded: the shape of the
// engine's refusal is `refused: <path>: <reason>`, so read the path out of it and
// compare the basename.
let refusedThis = 0
for (const m of transcript.matchAll(/refused[:\s]*([^"\n\\]{0,200})/gi)) {
  const base = String(m[1] ?? '').trim().split('/').pop()
  if (base === file) refusedThis++
}
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
