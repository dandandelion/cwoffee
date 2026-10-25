// Is the sentence you just typed in the store before the model finishes?
//
// Why this probe exists: the two cores answer that question differently, and the
// answer decides what the front end is allowed to paint. The rust core persists a
// turn when it ends, so mid-turn the store has no row for the message you sent and the
// view has to supply its own copy. The pi bridge writes the user's row *before* the
// provider call — "a send is a fact before it is asked of a model",
// crates/bridge/src/host.rs — so mid-turn a history reload already contains it.
//
// That difference is the whole cause of "switch to another session and back and my
// first message is duplicated": the repaint loaded a page that already had the row and
// then painted `sentDraft` on top of it. So `storeHoldsSentDraft` in ui/src/App.svelte
// asks the page instead of assuming, and this script says which answer the live core
// actually gives, from the gateway's own `session_messages` — the same read the UI makes.
//
//   node scripts/gateway-user-row-early.mjs --port 8291 --token T
// node 22 provides WebSocket globally
const argv = process.argv.slice(2)
const get = (f, d) => {
  const i = argv.indexOf(f)
  return i >= 0 ? argv[i + 1] : d
}
const port = get('--port', '8291')
const token = get('--token', '')
const ws = new WebSocket(`ws://127.0.0.1:${port}/ws`)
const t = (ms) => new Promise((r) => setTimeout(r, ms))
let id = 0
const replies = new Map()
const rpc = (cmd, args = {}) => ws.send(JSON.stringify({ id: ++id, cmd, args }))
const call = async (cmd, args) => {
  const myId = ++id
  ws.send(JSON.stringify({ id: myId, cmd, args }))
  for (let i = 0; i < 200 && !replies.has(myId); i++) await t(50)
  if (!replies.has(myId)) throw new Error(`${cmd} was never answered`)
  return replies.get(myId)
}

let sess = null
let authed = false
let turnEnd = null
let turnStart = null
const events = []
ws.addEventListener('open', () => ws.send(JSON.stringify({ type: 'auth', token })))
ws.addEventListener('message', (ev) => {
  const v = JSON.parse(ev.data)
  if (v.ok === true) replies.set(v.id, v.data)
  if (v.type === 'auth_ok' || v.type === 'ready') authed = true
  if (v.type === 'event') {
    const e = v.d ?? {}
    events.push(e.kind)
    if (e.kind === 'turnStart') turnStart = e.d?.session_id
    if (/approval/i.test(e.kind)) rpc('respond_approval', { id: e.d?.id, answer: 'allow' })
    if (e.kind === 'turnEnd') turnEnd = e.d?.finish
  }
})
for (let i = 0; i < 100 && !authed; i++) await t(50)
if (!authed) {
  console.log('GATEWAY USER ROW FAILED: never authenticated (wrong --token?)')
  process.exit(2)
}

sess = await call('new_session', { title: 'probe early row', workspace: '/tmp/cw-probe-ws' })
const text = 'Count out loud from one to fifteen, one number per line.'
const sentAt = Date.now()
await call('start_turn', { session: sess, text })

// Poll the store while the turn is still running. This is exactly the read a session
// click makes, so whatever it says here is what the repaint has to reconcile with.
let firstUserAt = null
let sawAssistant = false
let midTurn = null
for (let i = 0; i < 200 && turnEnd === null; i++) {
  const page = await call('session_messages', { id: sess, limit: 80 })
  const rows = page?.messages ?? page ?? []
  const users = rows.filter((r) => r.role === 'user')
  const assistants = rows.filter((r) => r.role === 'assistant')
  if (assistants.length) sawAssistant = true
  if (users.length && firstUserAt === null) firstUserAt = Date.now() - sentAt
  if (turnEnd === null && users.length && !midTurn)
    midTurn = { rows: rows.length, users: users.length, assistants: assistants.length }
  await t(150)
}

const page = await call('session_messages', { id: sess, limit: 80 })
const rows = page?.messages ?? page ?? []
const dump = rows.map(
  (r) => `${r.role}:${String(r.content).replace(/\s+/g, ' ').slice(0, 28)}`,
)
console.log(`session ${sess}; turnStart named ${turnStart}; finish=${turnEnd}`)
console.log(`mid-turn store read: ${JSON.stringify(midTurn)}`)
console.log(`settled store: rows=${rows.length} ${JSON.stringify(dump)}`)
const dupes = dump.filter((d) => d.startsWith('user:')).length
const verdict =
  !midTurn
    ? 'the store had no user row while the turn ran (rust-core behaviour): the view must paint its own copy'
    : midTurn.assistants === 0
      ? `the store already holds the user's row ${firstUserAt}ms into the turn, with no answer beside it: a repaint that also paints its own copy shows the message twice`
      : `the store had ${midTurn.users} user / ${midTurn.assistants} assistant row(s) mid-turn`
console.log(verdict)
const ok =
  dupes === 1 &&
  rows.some((r) => r.role === 'user' && String(r.content).includes('Count out loud')) &&
  rows.some((r) => r.role === 'assistant')
console.log(ok ? 'USER ROW EARLY OK' : 'USER ROW EARLY FAILED')
ws.close()
process.exit(ok ? 0 : 1)
