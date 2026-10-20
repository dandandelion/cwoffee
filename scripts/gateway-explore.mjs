#!/usr/bin/env node
// Can the coworker reach a tool it was never shown?
//
// Lean mode declares two tools instead of twelve and pays back ~10 KB of every prompt. It is
// only safe if the model can get the rest, and the chain that has to work is long: the model
// calls `explore`, the Rust registry searches itself, the extension makes the found tool
// callable in pi, and only then can the model call it. Any link failing, the failure the user
// sees is "that tool doesn't exist" - so this gate drives the whole chain and asserts each
// claim in the tool's own words:
//
//   * the discovery answer ("tool(s) fit ...") and the availability promise;
//   * the host's own line ("made callable from your next call"), which is the extension
//     signing for the part of the promise it owns;
//   * text that only the discovered tool can produce, read from the fixture - proof the
//     second call actually ran, not proof the model believed it did.
//
// The scenario's `require` strings check the first two against the prompt mock-llm was
// actually shown, so an answer that never reached the model fails there rather than here.
//
// Run: node scripts/gateway-explore.mjs [--core pi] [--port 8324] [--mock 8325]
//                                       [--db /tmp/cwoffee-explore-gate] [--mode lean]
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
const DIR = path.resolve(arg('db', '/tmp/cwoffee-explore-gate'))
const MODE = arg('mode', 'lean')
const TOKEN = 'EXPLORE-TOK'
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
console.log(`explore gate on the ${CORE} core, tool exposure ${MODE} (mock :${MPORT}, gateway :${PORT})`)
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
    CWOFFEE_PI_TOOL_MODE: MODE,
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

// One conversation, one turn: the model is told about two tools and asked to read a file.
// read_file is not among the two, so the only way the turn can succeed is by discovering it.
const sess = (await call('new_session', { title: 'Explore gate', workspace: path.join(DIR, 'ws') })).data
await call('start_turn', {
  session: sess,
  text: 'EXPLORE GATE: read the README.md in this folder and tell me what it says. If the tool you need is not in your list, find it.',
})
await settle()

const stored = (await call('session_messages', { id: sess, limit: 40 })).data ?? {}
const rows = stored.messages ?? []
// Read the tool rows by their shape, not by the dialect one core uses to name them: a tool
// row is a tool row, and `tool_calls_json` is not a promise the bridge makes about its
// storage. The engine's own opening lines are what identify each answer.
const toolRows = rows.filter((m) => m.role === 'tool').map((m) => m.content ?? '')
const foundOut = toolRows.find((c) => /tool\(s\) fit|tool\(s\) in this app|Nothing in the tool set/.test(c)) ?? ''
const readOut = toolRows.filter((c) => /:.*\S/.test(c) && !/tool\(s\)/.test(c)).map((c) => c).join('\n')
const assistantSays = rows.filter((m) => m.role === 'assistant').map((m) => m.content ?? '')

if (MODE === 'lean' && !/will declare only|lean: declaring|defaultTools/.test(gwLog)) {
  notes.push('nothing in the boot log says the declared set was narrowed - check that mount_settings wrote defaultTools')
}
if (!foundOut) fails.push('no tool output for `explore` — the model never reached the discovery tool, or its answer never arrived')
else {
  if (!/tool\(s\) fit/.test(foundOut)) fails.push(`the discovery answer is not a search answer: ${JSON.stringify(foundOut.slice(0, 160))}`)
  if (!foundOut.includes('read_file')) fails.push('the discovery answer does not name read_file for a request to read a file')
  if (!foundOut.includes('made callable from your next call'))
    fails.push('the host never signed for making the found tools callable — the model was promised something nobody claimed to deliver')
}
if (MODE === 'lean' && !readOut) {
  fails.push('the discovered tool never ran: `read_file` is not declared at the start in lean mode, so an activation that did not take shows up exactly like this')
} else if (readOut) {
  if (!/Nothing here is precious/.test(readOut))
    fails.push(`read_file ran but did not return the fixture: ${JSON.stringify(readOut.slice(0, 160))}`)
  else notes.push('the discovered tool ran and read the file')
}
if (/REQUIRE FAILED/.test(mockLog))
  fails.push(`the answer never reached the model's prompt: ${mockLog.split('\n').filter((l) => /REQUIRE FAILED/.test(l)).join('\n')}`)
if (!assistantSays.some((a) => a.includes('EXPLORE DONE')))
  fails.push(`the scripted final answer never came (assistant said: ${JSON.stringify(assistantSays.slice(-1))})`)

console.log('')
for (const f of fails) console.log(`FAIL ${f}`)
if (fails.length) {
  console.log('EXPLORE GATE FAILED')
  if (foundOut) console.log(`  explore said:\n    ${foundOut.split('\n').join('\n    ')}`)
  if (readOut) console.log(`  read_file said:\n    ${readOut.split('\n').join('\n    ')}`)
} else {
  console.log(`EXPLORE GATE OK (${CORE}, ${MODE}: found it, the host made it callable, and the tool itself read the file${notes.length ? '; ' + notes.join('; ') : ''})`)
  await call('delete_session', { id: sess }).catch(() => {})
}

if (process.env.DUMP_GW_LOG) console.log('--- gateway log ---\n' + gwLog)
ws.close()
for (const k of kids) k.kill('SIGTERM')
await t(700)
for (const f of fs.existsSync(DIR) ? fs.readdirSync(DIR) : []) if (f.includes('.sqlite')) fs.rmSync(path.join(DIR, f), { force: true })
process.exit(fails.length ? 1 : 0)
