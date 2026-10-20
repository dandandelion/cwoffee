#!/usr/bin/env node
// Can the coworker remember what it said in another conversation?
//
// The feature is a tool the model decides to call: nothing in the app pushes history at it.
// So the gate has to prove three separate things, and the interesting one is the third:
//
//   1. the tool ran and its answer came out of the app's own store (the other conversation's
//      title and its distinctive word appear in the tool's output, not in the model's prose);
//   2. the answer reached the model — the scripted final step carries `require` strings that
//      mock-llm checks against the tool output in the prompt, so an answer that never made it
//      back fails there rather than here;
//   3. the conversation that is *asking* was excluded, and the tool says so in words. On the
//      pi core that claim travels a long way: the bridge has to hand the model process the
//      database path and the session map, the extension has to hand the tool host pi's own
//      session file, and cwtool has to resolve it back to a cwoffee row. If any link is
//      missing, the output says "this host cannot say which conversation is asking" — an
//      honest sentence, but the one this gate exists to catch.
//
// Run: node scripts/gateway-recall.mjs [--core pi] [--port 8320] [--mock 8321]
//                                      [--db /tmp/cwoffee-recall-gate]
import { spawn } from 'node:child_process'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const TGT = process.env.CARGO_TARGET_DIR ?? path.join(REPO, 'target')
const argv = process.argv.slice(2)
const arg = (n, d) => {
  const i = argv.indexOf(`--${n}`)
  return i >= 0 ? argv[i + 1] : d
}
const CORE = arg('core', 'pi')
const PORT = Number(arg('port', 8320))
const MPORT = Number(arg('mock', 8321))
const DIR = path.resolve(arg('db', '/tmp/cwoffee-recall-gate'))
const TOKEN = 'RECALL-TOK'
const PI_BIN = arg('pi', `${os.homedir()}/.local/share/pi-node/node-v22.23.2-linux-x64/bin/pi`)
const t = (ms) => new Promise((r) => setTimeout(r, ms))

fs.rmSync(DIR, { recursive: true, force: true })
for (const d of ['ws', 'config', 'data', 'agent', 'sess']) fs.mkdirSync(path.join(DIR, d), { recursive: true })
fs.writeFileSync(path.join(DIR, 'ws', 'README.md'), '# scratch\n\nNothing here is precious.\n')

const kids = []
const fails = []
const notes = []

const mock = spawn(process.execPath, [path.join(REPO, 'scripts', 'mock-llm.mjs'), '--port', String(MPORT), '--speed', '1'], {
  stdio: ['ignore', 'pipe', 'pipe'],
})
kids.push(mock)
let mockLog = ''
mock.stdout.on('data', (b) => (mockLog += String(b)))
mock.stderr.on('data', (b) => (mockLog += String(b)))
await t(900)

const gwArgs = [
  '--db', path.join(DIR, 'g.sqlite'), '--token', TOKEN, '--port', String(PORT), '--core', CORE,
  '--pi', PI_BIN, '--pi-version', '1.2.0',
  '--pi-ext', path.join(REPO, 'pi-tools', 'cwoffee-tools.ts'),
  '--pi-tools', '-read,-bash,-edit,-write',
  '--pi-agent-dir', path.join(DIR, 'agent'), '--pi-session-dir', path.join(DIR, 'sess'),
]
console.log(`recall gate on the ${CORE} core (mock :${MPORT}, gateway :${PORT})`)
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
let gwLog = ''
gw.stdout.on('data', (b) => (gwLog += String(b)))
gw.stderr.on('data', (b) => (gwLog += String(b)))
await t(3500)

const ws = new globalThis.WebSocket(`ws://127.0.0.1:${PORT}/ws`)
const reply = new Map()
let authed = false
ws.addEventListener('open', () => ws.send(JSON.stringify({ type: 'auth', token: TOKEN })))
ws.addEventListener('message', (ev) => {
  const v = JSON.parse(ev.data)
  if (v.type === 'auth_ok') authed = true
  if (v.id) reply.set(v.id, v)
})
let seq = 0
const call = async (cmd, args = {}) => {
  const id = ++seq
  ws.send(JSON.stringify({ id, cmd, args }))
  for (let i = 0; i < 400; i++) {
    if (reply.has(id)) return reply.get(id)
    await t(50)
  }
  throw new Error(`${cmd} never answered`)
}
const settle = async (timeout = 90000) => {
  for (let i = 0; i < 200; i++) {
    const h = (await call('session_state')).data ?? {}
    if (h.busy) break
    await t(100)
  }
  const t0 = Date.now()
  for (;;) {
    const h = (await call('session_state')).data ?? {}
    if (!h.busy) return
    if (Date.now() - t0 > timeout) throw new Error('the turn never settled')
    await t(200)
  }
}

for (let i = 0; i < 100 && !authed; i++) await t(50)
if (!authed) {
  console.log('FAIL the scratch gateway never accepted the token:\n' + gwLog.slice(0, 500))
  process.exit(1)
}

// ---- seed a conversation worth remembering ------------------------------------------
const seedTitle = 'Memory gate alpha'
const sessA = (await call('new_session', { title: seedTitle, workspace: path.join(DIR, 'ws') })).data
await call('start_turn', { session: sessA, text: 'SEED THE MEMORY: remember the word mangosteen for later.' })
await settle()
console.log(`  seeded conversation ${sessA} ("${seedTitle}") with a distinctive word`)

// ---- ask from a different conversation ----------------------------------------------
const askTitle = 'Memory gate beta'
const sessB = (await call('new_session', { title: askTitle, workspace: path.join(DIR, 'ws') })).data
await call('start_turn', {
  session: sessB,
  text: 'RECALL GATE: look up what I have said about mangosteen in another conversation, then say RECALL DONE.',
})
await settle()

// ---- read what the tool actually said -------------------------------------------------
const stored = (await call('session_messages', { id: sessB, limit: 40 })).data ?? {}
const rows = stored.messages ?? []
const toolRow = rows.find((m) => m.role === 'tool' && (m.tool_calls_json || '').includes('"recall"') === false && /recall|match|conversation/i.test(m.content ?? ''))
  ?? rows.find((m) => m.role === 'tool' && /passage|mangosteen|cannot say/i.test(m.content ?? ''))
const toolOut = toolRow?.content ?? ''
const assistantSays = rows.filter((m) => m.role === 'assistant').map((m) => m.content ?? '')
if (toolOut) console.log(`\n  recall said:\n    ${toolOut.split('\n').join('\n    ')}`)
else fails.push('no tool output row for `recall` in the transcript — the tool never ran, or its answer never arrived')

if (toolOut) {
  if (!toolOut.includes('mangosteen')) fails.push('the tool output does not even carry the word that was searched')
  if (!toolOut.includes(seedTitle))
    fails.push(`the other conversation is not named in the tool's own output: ${JSON.stringify(toolOut.slice(0, 160))}`)
  if (toolOut.includes(`from "${askTitle}"`))
    fails.push('the asking conversation quoted itself back — the exclusion did not happen')
  if (!/itself is left out/i.test(toolOut)) {
    const honest = /cannot say which conversation is asking/i.test(toolOut)
    fails.push(
      honest
        ? 'the tool could not tell which conversation was asking: the store path, the session map or the pi session file never reached the tool host'
        : `the tool output makes no claim about which conversation was excluded: ${JSON.stringify(toolOut.slice(-160))}`,
    )
  } else {
    notes.push('the asking conversation was named and left out')
  }
  if (!/passage\(s\) matching/.test(toolOut))
    fails.push(`the tool answered, but not as a search: ${JSON.stringify(toolOut.slice(0, 160))}`)
}

// The scripted last step carries require strings; mock-llm fails the turn if the engine's
// own tool output never reached the prompt. A gate that only reads the store would miss the
// half of the feature that is the model actually being told.
if (/REQUIRE FAILED/.test(mockLog)) fails.push(`the answer never reached the model's prompt: ${mockLog.split('\n').filter((l) => /REQUIRE FAILED/.test(l)).join('\n')}`)
if (!assistantSays.some((a) => a.includes('RECALL DONE')))
  fails.push(`the scripted final answer never came (assistant said: ${JSON.stringify(assistantSays.slice(-1))})`)

// And the store really was read: the seeded conversation is what should be found.
const seeded = (await call('session_messages', { id: sessA, limit: 10 })).data ?? {}
if (!JSON.stringify(seeded.messages ?? []).includes('mangosteen'))
  fails.push('the seeded conversation does not contain the word — the gate measured nothing')

console.log('')
for (const f of fails) console.log(`FAIL ${f}`)
if (fails.length) {
  console.log('RECALL GATE FAILED')
  if (/disk I\/O|panic|no scenario/.test(gwLog + mockLog))
    console.log('  first lines of what the processes said:\n' + (mockLog + gwLog).split('\n').filter(Boolean).slice(0, 6).join('\n'))
} else {
  console.log(
    `RECALL GATE OK (${CORE}: another conversation's words found in the tool's own output, ${notes.join('; ')})`,
  )
  for (const id of [sessA, sessB]) await call('delete_session', { id }).catch(() => {})
}

ws.close()
for (const k of kids) k.kill('SIGTERM')
await t(700)
for (const f of fs.existsSync(DIR) ? fs.readdirSync(DIR) : []) if (f.includes('.sqlite')) fs.rmSync(path.join(DIR, f), { force: true })
process.exit(fails.length ? 1 : 0)
