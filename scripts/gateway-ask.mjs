// ask_user over the wire: one tool call, its questions as one set, answers delivered back
// into the tool's own words.
//
// Why this is a gate and not just a page check: the page check exercises the Svelte card,
// which is one client's idea of a card. What has to hold for *every* client is the event
// shape and the round trip — a question that arrives as two unrelated cards, or a card
// whose options lost their one-line trade-offs, or picks that close the card without
// reaching the tool, all look fine from inside one UI.
//
// Run:  node scripts/gateway-ask.mjs [--port 8164] [--token ASKTOK] [--core pi]
// It starts its own mock endpoint, gateway, workspace and stores, so it needs nothing
// from a running app and leaves nothing behind.
import { spawn } from 'node:child_process'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const REPO = path.join(HERE, '..')
const arg = (name, dflt) => {
  const i = process.argv.indexOf(`--${name}`)
  return i >= 0 ? process.argv[i + 1] : dflt
}
const CORE = arg('core', 'pi')
const PORT = Number(arg('port', 8164))
const MPORT = Number(arg('mock-port', 8194))
const TOKEN = arg('token', 'ASKTOK')
// This checkout's own target dir, by default. The neighbour checkout packages the same
// names (`cwoffee-gateway`, and the `cwtool` this gate mounts as its tool host), so
// reaching across the tree buys an artifact built from sources that are not here — and a
// gate that grades the wrong vintage reports red about code this tree does not have.
const TGT = arg('target', path.join(REPO, 'target'))
const PI_BIN = arg('pi', '/home/dandandelion/.local/share/pi-node/node-v22.23.2-linux-x64/bin/pi')
const DIR = arg('dir', '/tmp/gw-ask')
const t = (ms) => new Promise((r) => setTimeout(r, ms))

fs.rmSync(DIR, { recursive: true, force: true })
for (const d of ['agent', 'sess', 'ws', 'config', 'data']) fs.mkdirSync(path.join(DIR, d), { recursive: true })
// The DB trio goes with the directory: leaving a -wal behind next to a deleted database is
// how the next run reports "disk I/O error" instead of a real failure.
for (const f of fs.readdirSync(DIR)) if (f.includes('.sqlite')) fs.rmSync(path.join(DIR, f), { force: true })
fs.writeFileSync(path.join(DIR, 'ws', 'README.md'), '# scratch\n\nNothing here is precious.\n')

const fail = []
const notes = []
const questions = []
const answersSent = []
const kinds = {}
// A turn stopped with a card open: the card has to close with the turn. `stopPhase` keeps
// this client from answering, so the question stays outstanding when Stop is pressed.
let stopPhase = false
let stoppedQ = null
const stopQs = []
const expired = []
let toolOut = ''
let sid = 0
let rpcId = 0
const rpc = (ws, cmd, args = {}) => ws.send(JSON.stringify({ id: ++rpcId, cmd, args }))
const kids = []

const mock = spawn(process.execPath, [path.join(REPO, 'scripts', 'mock-llm.mjs'), '--port', String(MPORT), '--speed', '1'], { stdio: ['ignore', 'pipe', 'pipe'] })
kids.push(mock)
let mockLog = ''
mock.stderr.on('data', (b) => { mockLog += b.toString() })
mock.stdout.on('data', (b) => { mockLog += b.toString() })
await t(900)

const gwArgs = ['--db', path.join(DIR, 'g.sqlite'), '--token', TOKEN, '--port', String(PORT), '--core', CORE,
  '--pi', PI_BIN, '--pi-version', '1.2.0',
  '--pi-ext', path.join(REPO, 'pi-tools', 'cwoffee-tools.ts'),
  '--pi-tools', '-read,-bash,-edit,-write',
  '--pi-agent-dir', path.join(DIR, 'agent'), '--pi-session-dir', path.join(DIR, 'sess')]
console.log(`  artifacts from ${TGT}`)
const gw = spawn(path.join(TGT, 'release', 'cwoffee-gateway'), gwArgs, {
  stdio: ['ignore', 'pipe', 'pipe'],
  env: {
    ...process.env,
    XDG_CONFIG_HOME: path.join(DIR, 'config'),
    XDG_DATA_HOME: path.join(DIR, 'data'),
    CWOFFEE_WORKSPACE: path.join(DIR, 'ws'),
    CWOFFEE_URL: `http://127.0.0.1:${MPORT}/v1`,
    CWOFFEE_MODEL: 'cwoffee-mock',
    CWOFFEE_MODE: 'ask',
    CWOFFEE_CWTOOL: process.env.CWOFFEE_CWTOOL ?? path.join(TGT, 'release', 'cwtool'),
  },
})
kids.push(gw)
gw.stdout.on('data', (b) => process.stdout.write(String(b)))
gw.stderr.on('data', (b) => process.stderr.write(String(b)))
await t(3500)

const ws = new globalThis.WebSocket(`ws://127.0.0.1:${PORT}/ws`)
let turnDone = false
let rows = []
ws.addEventListener('open', () => ws.send(JSON.stringify({ type: 'auth', token: TOKEN })))
ws.addEventListener('message', (ev) => {
  const v = JSON.parse(ev.data)
  if (v.type === 'auth_ok') rpc(ws, 'new_session', { title: 'ask round trip', workspace: path.join(DIR, 'ws') })
  if (v.ok === true && v.id === 1 && !sid) {
    sid = v.data
    rpc(ws, 'start_turn', { session: sid, text: 'Ask me two questions before writing the report, then write it.' })
  }
  if (v.ok === false) fail.push(`rpc refused: ${JSON.stringify(v).slice(0, 200)}`)
  if (v.type === 'event') {
    const env = v.d ?? {}
    const kind = env.kind ?? ''
    const p = env.d ?? env
    kinds[kind] = (kinds[kind] ?? 0) + 1
    if (kind === 'questionRequested' || kind === 'question_requested') {
      const q = typeof p === 'object' ? p : {}
      questions.push(q)
      console.log(`  question event: id=${q.id} set=${JSON.stringify(q.set ?? null)} rec=${q.recommended} allow_custom=${q.allow_custom} multi=${q.multi} opts=${JSON.stringify((q.options ?? []).map((o) => o.label))}`)
      if (stopPhase) {
        // Recorded, not answered: this is the card Stop has to close.
        stoppedQ = stoppedQ ?? q
        stopQs.push(q)
        return
      }
      // Answer each question in turn: first pick for the first, second pick for the second,
      // which is what the scenario's `require` strings are written against.
      // Which pick of *this answer* — counted from the answers this client actually sent,
      // not from the questions that arrived: a stopped turn leaves questions unanswered, and
      // counting those would hand the next call the wrong half of the pair.
      const which = answersSent.length % 2
      const label = q.options?.[which === 0 ? 0 : 1]?.label ?? ''
      if (!label) {
        fail.push(`question ${which + 1} arrived with no options to pick: ${JSON.stringify(q).slice(0, 200)}`)
        return
      }
      answersSent.push(`${q.question} -> ${label}`)
      // Flat, not nested: the gateway reads `text` and `options` off the args themselves.
      // A client that nests them gets an empty answer, and the tool then reports — correctly
      // — that nobody answered, which is a confusing way to debug your own client.
      rpc(ws, 'respond_question', { id: q.id, text: label, options: [which === 0 ? 0 : 1] })
    } else if (kind === 'questionExpired' || kind === 'question_expired') {
      expired.push({ id: typeof p === 'object' ? p?.id : p, why: p?.why })
    } else if (/toolExec/i.test(kind)) {
      const text = typeof p === 'string' ? p : JSON.stringify(p ?? '')
      if (text.length > toolOut.length) toolOut = text
    } else if (kind === 'note' || kind === 'error') {
      notes.push(`${kind}: ${typeof p === 'string' ? p : JSON.stringify(p)}`)
    }
    if (kind === 'turnEnd' || kind === 'turn_end' || kind === 'turnDone') turnDone = true
  }
  if (v.ok === true && v.id === 90) rows = v.data?.messages ?? v.data ?? []
})

const turnEnds = () => (kinds.turnEnd ?? 0) + (kinds.turn_end ?? 0) + (kinds.turnDone ?? 0)
await t(45000)
// A second ask_user call in the same conversation: the engine numbers a fresh set in every
// `cwtool` process, so its set id is 1 for the first set of every call. A bridge that keyed
// its card off that number would draw the second call's questions onto the first call's
// card, and one conversation would silently become a two-question card with four questions.
const setA = questions[0]?.set?.id ?? null
const after1 = turnEnds()
const before2 = questions.length
if (sid && after1 > 0) {
  rpc(ws, 'start_turn', { session: sid, text: 'Ask me two questions before writing the report, then write it.' })
  for (let i = 0; i < 90 && turnEnds() < after1 + 1; i++) await t(500)
}
console.log(`  second call: ${questions.length - before2} question event(s), set ${JSON.stringify(questions[before2]?.set ?? null)} (first call set was ${setA})`)

// ---- a turn stopped with a card open has to close that card ----------------
// A question is a parked turn now, not a failing one: nothing times out. The only event
// that closes an unanswered card is the turn ending, and pressing Stop *is* that event.
// A card left in "waiting for you" after a Stop offers a choice nothing will ever take,
// and the conversation looks wedged from the user's side.
const stopTurnEnds = turnEnds()
const stopQBefore = questions.length
const expBefore = expired.length
stopPhase = true
if (sid) rpc(ws, 'start_turn', { session: sid, text: 'Ask me two questions before writing the report, then write it.' })
for (let i = 0; i < 40 && questions.length < stopQBefore + 1; i++) await t(500)
if (stoppedQ) rpc(ws, 'cancel_turn', {})
// The turn settling is part of the claim: a card can be closed while the session stays
// busy, which is the same wedged conversation wearing a different hat.
for (let i = 0; i < 60 && (expired.length < expBefore + 1 || turnEnds() < stopTurnEnds + 1); i++) await t(500)
// Give any second dialog of that set (a set's questions are in flight together) a chance to
// turn up and be recorded unanswered before the next call starts asking.
await t(1500)
stopPhase = false
const stoppedSet = stoppedQ?.set?.id ?? null

// ---- and the conversation still works afterwards ---------------------------
// The stopped call held a card key in the bridge. If stopping did not release it, the next
// ask_user call in the same conversation draws its questions onto the dead card.
const recTurnEnds = turnEnds()
const recQBefore = questions.length
if (sid) rpc(ws, 'start_turn', { session: sid, text: 'Ask me two questions before writing the report, then write it.' })
for (let i = 0; i < 90 && turnEnds() < recTurnEnds + 1; i++) await t(500)
const recQ = questions.slice(recQBefore)
ws.send(JSON.stringify({ id: 90, cmd: 'session_messages', args: { id: sid } }))
await t(1500)

// ---- what was claimed ------------------------------------------------------
const stored = rows
  .map((r) => (typeof r.content === 'string' ? r.content : JSON.stringify(r.content ?? '')))
  .join('\n')

if (questions.length < 2) fail.push(`expected 2 questions, got ${questions.length}`)
// The two answered calls this gate opens first are the pair the assertions below are
// written against. The stopped turn and the one after it add their own question events,
// and grading them with the pair's arithmetic would report a merged card that never merged.
const paired = questions.slice(0, 4)
if (paired.length !== 4)
  fail.push(`two ask_user calls in the same conversation produced ${paired.length} question event(s), not 4; a card merged into another or never opened`)
else {
  const second = paired.slice(2)
  if (second[0].set?.id !== second[1].set?.id)
    fail.push(`the second call's two questions carry different set ids (${second[0].set?.id}, ${second[1].set?.id})`)
  if (second[0].set?.id === setA)
    fail.push(`the second call reused set id ${setA} — one conversation would show one card with four questions`)
}
const sets = paired.map((q) => q.set).filter(Boolean)
const allSets = questions.map((q) => q.set).filter(Boolean)
if (allSets.length !== questions.length)
  fail.push(`every question of a set call should carry the marker; got ${allSets.length}/${questions.length} set markers`)
if (sets.length !== paired.length)
  fail.push(`the two answered calls lost their set markers: ${sets.length}/${paired.length}`)
else {
  // Grouping is per call: the two questions of one call share one id ("question 1 of 2" on
  // one card), and the two calls must not share an id with each other.
  for (let c = 0; c + 1 < sets.length; c += 2) {
    if (sets[c].id !== sets[c + 1].id)
      fail.push(`two questions of one call carry different set ids (${sets[c].id}, ${sets[c + 1].id}) — the UI would draw two cards`)
    if (sets[c].total !== 2 || sets[c + 1].total !== 2) fail.push(`the set does not say it holds two questions: ${JSON.stringify(sets)}`)
    if (sets[c].index === sets[c + 1].index) fail.push(`two questions claim the same position: ${JSON.stringify(sets)}`)
  }
}
const q1 = questions[0] ?? {}
if (!(q1.options ?? []).length) fail.push('the first question arrived without options')
else if (!q1.options.some((o) => (o.detail ?? '').length > 0))
  fail.push(`the options lost the engine's one-line trade-offs: ${JSON.stringify(q1.options).slice(0, 200)}`)
if (q1.recommended !== 0) fail.push(`the recommendation the model marked is missing: ${JSON.stringify(q1.recommended)}`)
if (q1.allow_custom !== false) fail.push(`the model said allow_custom=false and the card offers free text anyway: ${JSON.stringify(q1).slice(0, 200)}`)
if (answersSent.length !== questions.length - stopQs.length)
  fail.push(`${answersSent.length} answer(s) were sent back for ${questions.length - stopQs.length} question(s) that needed one (${stopQs.length} were left unanswered on purpose, to have something to stop)`)
if (!turnDone) fail.push('the turn never settled')
if (!toolOut.includes('User chose: 1: Engineers') || !toolOut.includes('User chose: 2: Two pages'))
  fail.push(`the picks never reached the tool's own words: ${JSON.stringify(toolOut).slice(0, 260)}`)
if (!stored.includes('User chose: 1: Engineers')) fail.push('the stored transcript does not carry the tool result with the picks')
if (!stored.includes('Written, with those two settled.'))
  fail.push(`the scripted answer never arrived — the endpoint refused the step, so the picks did not reach the prompt. mock says: ${mockLog.split('\n').filter((l) => /REQUIRE|EXHAUST|MISS/.test(l)).join(' / ') || '(no mock complaints)'}`)
if (notes.some((n) => /ask_user|question payload|nobody|cannot show|did not answer/.test(String(n))))
  fail.push(`the round trip produced complaints: ${JSON.stringify(notes.filter((n) => /ask_user|question payload|nobody|cannot show|did not answer/.test(String(n)))).slice(0, 400)}`)

// A stopped turn owes its card: the frame, and a session that is not stuck.
if (!stoppedQ) fail.push('the stopped turn never asked a question, so Stop had no card to close')
else if (!expired.slice(expBefore).some((e) => e.id === stoppedQ.id))
  fail.push(`Stop was pressed with question ${stoppedQ.id} unanswered and no frame closed the card — it would sit in "waiting for you" with buttons nothing will take`)
// The reason travels, because the sentence the card ends with is what the reader can act
// on later. A stopped turn must not be reported as a clock running out or as the model
// being told it got no answer: the reader stopped it, and the tool never ran.
else if (expired.slice(expBefore).find((e) => e.id === stoppedQ.id)?.why !== 'turnStopped')
  fail.push(`the card a Stop closed says ${JSON.stringify(expired.slice(expBefore).find((e) => e.id === stoppedQ.id))}; it has to say the turn was stopped`)
if (turnEnds() < stopTurnEnds + 1) fail.push('the stopped turn never settled: the session stayed busy after Stop')
if (recQ.length < 2)
  fail.push(`the ask_user call after a stopped turn produced ${recQ.length} question event(s), not 2 — the stopped call kept its card`)
else if (stoppedSet !== null && recQ[0].set?.id === stoppedSet)
  fail.push(`the call after a stopped turn drew onto the stopped call's card (set id ${stoppedSet} reused, and its questions were never closed)`)

ws.close()
for (const k of kids) k.kill('SIGTERM')
await t(400)

console.log(`ask round trip on the ${CORE} core: ${questions.length} question event(s), set ${JSON.stringify(sets[0] ?? null)}`)
console.log(`  answered: ${answersSent.join(' | ')}`)
console.log(`  tool said: ${JSON.stringify((toolOut.split('\n').pop() ?? '').slice(0, 90))}`)
console.log(`  turn settled=${turnDone} rows=${rows.length} events=${JSON.stringify(kinds)}`)
console.log(`  stopped turn: question ${stoppedQ?.id} of set ${stoppedSet}, closed by ${JSON.stringify(expired.slice(expBefore))}; the call after it asked ${recQ.length} question(s) on set ${JSON.stringify(recQ[0]?.set?.id ?? null)}`)
if (fail.length) {
  for (const f of fail) console.log(`  FAIL ${f}`)
  console.log(`ASK GATE FAILED (${CORE})`)
  process.exit(1)
}
console.log(`ASK GATE OK (${CORE}: one call, its questions as one set, and the picks land in the tool's own words)`)
