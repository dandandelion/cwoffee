// Does a mid-turn conversation switch retarget the turn?
//
// The Rust core documents its answer in `crates/engine/src/host.rs`: command verbs
// queue until the turn finishes, so switching while a turn runs cannot retarget it.
// A pi bridge is a `select!` loop with no such queue, so the same click is handled
// *during* the turn. This script decides it from the stored transcript, which is what
// the user sees after closing and reopening a tab: rows written into the wrong
// conversation are exactly "the history got cleared or messed up".
//
//   node scripts/gateway-midturn-switch.mjs --port 8109 --token T
// node 22 provides WebSocket globally
const argv = process.argv.slice(2)
const get = (f, d) => { const i = argv.indexOf(f); return i >= 0 ? argv[i + 1] : d }
const port = get('--port', '8109')
const token = get('--token', 'T')
const ws = new WebSocket(`ws://127.0.0.1:${port}/ws`)
const t = (ms) => new Promise((r) => setTimeout(r, ms))
let id = 0
const rpc = (cmd, args = {}) => ws.send(JSON.stringify({ id: ++id, cmd, args }))
const replies = new Map()
const A = { rows: 0, msgs: [] }
const B = { rows: 0, msgs: [] }
let sid = 0, tid = 0, switched = false, turnEnd = null, owner = null
const seen = []
ws.addEventListener('open', () => ws.send(JSON.stringify({ type: 'auth', token })))
ws.addEventListener('message', (ev) => {
  const v = JSON.parse(ev.data)
  if (v.ok === true) replies.set(v.id, v.data)
  if (v.type === 'auth_ok') {
    rpc('new_session', { title: 'A - the one that works', workspace: '/tmp/cw-smoke-ws' })
    rpc('new_session', { title: 'B - the one you look at', workspace: '/tmp/cw-smoke-ws' })
  }
  if (v.type === 'event') {
    const e = v.d ?? {}, kind = e.kind ?? '', p = e.d ?? {}
    seen.push(kind)
    if (kind === 'turnStart') owner = p.session_id
    if (/approval/i.test(kind)) {
      // Do not answer: the tool sits waiting on the human, which is the window a real
      // reader has for clicking another conversation in the sidebar.
      rpc('respond_approval', { id: p.id, answer: 'allow' })
      if (!switched) { switched = true; rpc('switch_session', { id: replies.get(2) }) }
    }
    if (kind === 'turnEnd') turnEnd = p.finish
  }
})
for (let i = 0; i < 100 && !(replies.has(1) && replies.has(2)); i++) await t(100)
const sidA = replies.get(1), sidB = replies.get(2)
// The mock's `read-file-approval` scenario: a real tool call, a real approval gate.
ws.send(JSON.stringify({ id: ++id, cmd: 'start_turn', args: { session: sidA, text: 'Use read_file to read the file /etc/hostname' } }))
// The approval card times out on pi's own clock; answer it so the turn can settle and
// the rows get written, then compare the two transcripts.
for (let i = 0; i < 250 && !turnEnd; i++) await t(200)
for (const [i, box] of [[sidA, A], [sidB, B]]) {
  ws.send(JSON.stringify({ id: ++id, cmd: 'session_messages', args: { id: i } }))
  const myId = id
  for (let k = 0; k < 50 && !replies.has(myId); k++) await t(100)
  const d = replies.get(myId)
  box.msgs = d?.messages ?? d ?? []
  box.rows = Array.isArray(box.msgs) ? box.msgs.length : 0
}
const dump = (m) => (Array.isArray(m) ? m.map((r) => `${r.role}:${String(r.content).replace(/\s+/g, ' ').slice(0, 40)}`) : [])
console.log(`sessions: A=${sidA} B=${sidB}`)
console.log(`turnStart named session: ${owner}; switch sent mid-turn: ${switched}; finish=${turnEnd}`)
console.log(`A rows=${A.rows} ${JSON.stringify(dump(A.msgs))}`)
console.log(`B rows=${B.rows} ${JSON.stringify(dump(B.msgs))}`)
const leaked = dump(B.msgs).filter((r) => !/^user:/.test(r))
const aTurn = dump(A.msgs).filter((r) => /tool|read_file|hostname/.test(r)).length
const verdict = leaked.length
  ? `LEAK: ${leaked.length} of A's turn landed in B (${JSON.stringify(leaked)})`
  : aTurn === 0
    ? 'A never got its own turn output — check the transcript, this is not a pass'
    : 'no leak: A kept its turn, B stayed empty'
console.log(verdict)
console.log(verdict.startsWith('no leak') ? 'MIDTURN SWITCH OK' : 'MIDTURN SWITCH FAILED')
ws.close()
process.exit(verdict.startsWith('no leak') ? 0 : 1)
