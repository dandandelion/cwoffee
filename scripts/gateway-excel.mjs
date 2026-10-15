// The Excel workflow, end to end, through whichever core is up.
//
// cwoffee's spreadsheet tools are the most valuable thing the product has: a workbook the
// user actually keeps, read without unzipping it, queried with SQL, and answered with real
// numbers. Under a pi core they are only as real as the tool host that mounts them, so this
// gate asks for a number that can only come from the file and insists the transcript
// contains it. A model that guesses gets a wrong number; a harness that mounted nothing
// gets no tool calls at all.
//
//   node scripts/gateway-excel.mjs --port 8124 --token DESKTOK \
//        --workspace "$HOME/Downloads/cashflow" --file "BES Cash Flow.xlsx" --expect 946010
const argv = process.argv.slice(2)
const get = (f, d) => {
  const i = argv.indexOf(f)
  return i >= 0 ? argv[i + 1] : d
}
const port = get('--port', '8124')
const token = get('--token', 'DESKTOK')
const workspace = get('--workspace', process.env.HOME + '/Downloads/cashflow')
const file = get('--file', 'BES Cash Flow.xlsx')
const sheet = get('--sheet', 'Local Fund')
const expect = get('--expect', '946010')
const ws = new WebSocket(`ws://127.0.0.1:${port}/ws`)
const t = (ms) => new Promise((r) => setTimeout(r, ms))
let id = 0
const rpc = (cmd, args = {}) => ws.send(JSON.stringify({ id: ++id, cmd, args }))
const replies = new Map()
const seen = { cards: 0, notes: [], errors: [], turnEnd: null, names: [], kinds: {} }
let sid = 0
ws.addEventListener('open', () => ws.send(JSON.stringify({ type: 'auth', token })))
// one rpc id at a time is the sequence here: id 1 is the new session, and the reply to it
// is the only safe moment to address a turn at it.
ws.addEventListener('message', (ev) => {
  const v = JSON.parse(ev.data)
  if (v.ok === true) replies.set(v.id, v.data)
  if (v.type === 'auth_ok') rpc('new_session', { title: 'excel workflow', workspace })
  if (v.ok === true && v.id === 1 && !sid) {
    sid = v.data
    rpc('start_turn', {
      session: sid,
      text: `Open the spreadsheet "${file}" in this folder using the spreadsheet tools, and tell me the total of the Cash Flow column on the "${sheet}" sheet. Use the tools to get it — do not guess and do not read the file as text.`,
    })
  }
  if (v.type === 'event') {
    const e = v.d ?? {}, kind = e.kind ?? '', p = e.d ?? {}
    seen.kinds[kind] = (seen.kinds[kind] ?? 0) + 1
    if (/approval/i.test(kind)) {
      seen.cards++
      rpc('respond_approval', { id: p.id, answer: 'allow' })
    } else if (kind === 'toolCallBegin') seen.names.push(p.name)
    else if (kind === 'note') seen.notes.push(p.message)
    else if (kind === 'error') seen.errors.push(p.message)
    else if (kind === 'turnEnd') seen.turnEnd = p.finish
  }
})
for (let i = 0; i < 100 && !sid; i++) await t(100)
for (let i = 0; i < 900 && !seen.turnEnd; i++) await t(250)
ws.send(JSON.stringify({ id: ++id, cmd: 'session_messages', args: { id: sid } }))
const fetchId = id
// One listener records every reply; polling it beats piling up a listener per attempt,
// which is the kind of thing that quietly drops the answer under load.
let rows = []
for (let i = 0; i < 100 && !rows.length; i++) {
  await t(100)
  const d = replies.get(fetchId)
  if (d) rows = d?.messages ?? d ?? []
}
const blob = rows.map((r) => (typeof r?.content === 'string' ? r.content : '')).join('\n')
const variants = [expect, expect.replace(/\B(?=(\d{3})+(?!\d))/g, ',')]
const fail = []
if (seen.turnEnd == null) fail.push('the turn never ended')
if (!seen.names.length) fail.push('no tool was called: the spreadsheet tools are not mounted')
if (!seen.names.some((n) => n.startsWith('sheet'))) fail.push(`no sheet_* tool ran (called: ${JSON.stringify(seen.names)})`)
if (!variants.some((x) => blob.includes(x))) fail.push(`the answer does not contain the workbook's ${expect}: the number cannot have come from the file`)
console.log(`excel gate: session ${sid} tools=${JSON.stringify(seen.names)} cards=${seen.cards} finish=${seen.turnEnd} rows=${rows.length}`)
console.log(`  expects one of ${JSON.stringify(variants)} in the transcript`)
for (const n of seen.notes.slice(0, 4)) console.log(`  note: ${n}`)
for (const e of seen.errors.slice(0, 4)) console.log(`  error: ${e}`)
if (fail.length) {
  for (const f of fail) console.log(`  FAIL ${f}`)
  console.log('EXCEL GATE FAILED')
  process.exit(1)
}
console.log('EXCEL GATE OK')
ws.close()
process.exit(0)
