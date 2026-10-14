// The approval round trip through the gateway, with a real tool behind it.
//
// `gateway-smoke.mjs` never answers an approval card — it has no such check — so the
// path "a phone says allow, and the tool that was waiting runs" was covered only by
// unit tests on the gateway side and by the bridge probe on the core side. This closes
// the loop between them, against whichever core the gateway is running: same script,
// `--core rust` and `--core pi`, and the assertions are about the *tool*, not the wire.
//
//   node scripts/gateway-approval.mjs --port 8106 --token T-PI --answer allow
//   node scripts/gateway-approval.mjs --port 8106 --token T-PI --answer deny
const argv = process.argv.slice(2)
const get = (f, d) => {
  const i = argv.indexOf(f)
  return i >= 0 ? argv[i + 1] : d
}
const port = get('--port', '8106')
const token = get('--token', 'T-PI')
const answer = get('--answer', 'allow') // allow | deny | allowSession
const ws = new WebSocket(`ws://127.0.0.1:${port}/ws`)
const t = (ms) => new Promise((r) => setTimeout(r, ms))
let id = 0
const rpc = (cmd, args = {}) => ws.send(JSON.stringify({ id: ++id, cmd, args }))

const seen = { card: null, cards: 0, notes: [], toolEvents: 0, execEnd: 0, kinds: {}, turnEnd: null, errors: [] }
ws.addEventListener('open', () => ws.send(JSON.stringify({ type: 'auth', token })))
ws.addEventListener('message', (ev) => {
  const v = JSON.parse(ev.data)
  if (v.type === 'auth_ok') rpc('new_session', { title: 'approval round trip', workspace: '/tmp/cw-smoke-ws' })
  if (v.ok === true && v.id === 1 && !sid) {
    sid = v.data
    rpc('start_turn', {
      text: 'Use read_file to read the file /etc/hostname',
      session: sid,
    })
  }
  if (v.type === 'event') {
    // EngineEvent serialises as {kind, d}: the tag is `kind`, not `type`, and the
    // payload rides under `d` — which the UI knows and a hand-written client finds
    // out once, the hard way.
    const env = v.d ?? {}
    const kind = env.kind ?? ''
    const p = env.d ?? env
    if (/approval/i.test(kind)) {
      seen.cards++
      seen.card = p
      // Answer on the same tick the card arrives: the point is the round trip, not a
      // demonstration of the timeout.
      rpc('respond_approval', { id: p.id, answer })
    } else if (kind === 'note' || kind === 'error') {
      ;(kind === 'note' ? seen.notes : seen.errors).push(typeof p === 'string' ? p : p.message)
    } else if (/tool/i.test(kind)) {
      seen.toolEvents++
      seen.kinds[kind] = (seen.kinds[kind] ?? 0) + 1
      if (/execEnd|toolResult|toolEnd/i.test(kind)) seen.execEnd++
    } else if (kind === 'turnEnd') {
      seen.turnEnd = p.finish
    }
  }
})
let sid = 0

await t(2000)
if (!sid) {
  console.error('FAIL never created a session (auth or new_session failed)')
  process.exit(1)
}
// The scenario's own latency is ~800 ms per round trip plus the tool; 12 s is generous
// without being a test that hides a hang.
for (let i = 0; i < 60 && !seen.turnEnd; i++) await t(200)

const fail = []
if (!seen.card) fail.push('no approval card ever arrived')
if (seen.card && seen.card.tool !== 'read_file') fail.push(`card named the wrong tool: ${seen.card?.tool}`)
// `escapes` never reaches the wire on either core: `approval_event` deliberately drops
// it so the desktop, the gateway and the TUI render one shape, and the escape rides in
// `summary`. So the assertion is about the words a person actually reads.
const summary = seen.card?.summary ?? ''
if (!summary.includes('/etc/hostname')) fail.push(`card did not name the file it wants: ${summary || '(none)'}`)
if (!/OUTSIDE the workspace/i.test(summary)) fail.push(`card did not say the path leaves the workspace: ${summary || '(none)'}`)
if (seen.cards > 1) fail.push(`${seen.cards} cards for one tool call: the same decision shown twice is a decision nobody can trust`)
if (seen.turnEnd == null) fail.push('the turn never ended')
if (!seen.toolEvents) fail.push('no tool events at all: the call never reached the client')

// Ask the store what actually happened, because a client that saw a card proves
// nothing about the tool. The Rust shim reports its own result, and a denial must not.
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
const transcript = JSON.stringify(rows)
// What allow has to prove, stated so it holds on either core: the call reached the
// tool and finished, and nothing on the way reported a denial. (An earlier version
// grepped for the bridge's own `engine:"rust"` envelope, which only the pi core emits
// — a gate that flatters one core is not a gate.)
if (answer === 'allow') {
  if (seen.execEnd < 1) fail.push(`answered allow, but no tool-result event arrived (kinds: ${JSON.stringify(seen.kinds)})`)
  if (/denied|blocked|refus/i.test(transcript + JSON.stringify(seen.notes))) fail.push('answered allow, yet something reports a denial')
}
if (answer === 'deny') {
  if (/\"ok\":true/.test(transcript)) fail.push('answered deny, yet the tool reported success')
  if (!/denied|blocked|refus/i.test(transcript + JSON.stringify(seen.notes))) {
    fail.push('answered deny, and nothing says so — a silent denial is indistinguishable from a slow one')
  }
}

console.log(`approval ${answer}: ${seen.cards} card(s), tool=${seen.card?.tool}, summary=${JSON.stringify(summary)}, toolEvents=${seen.toolEvents} ${JSON.stringify(seen.kinds)}, execEnd=${seen.execEnd}, finish=${seen.turnEnd}, rows=${rows.length}`)
for (const n of seen.notes) console.log(`  note: ${n}`)
for (const e of seen.errors) console.log(`  error: ${e}`)
if (fail.length) {
  for (const f of fail) console.log(`  FAIL ${f}`)
  console.log(`GATEWAY APPROVAL FAILED (${answer})`)
  process.exit(1)
}
console.log(`GATEWAY APPROVAL OK (${answer})`)
ws.close()
process.exit(0)
