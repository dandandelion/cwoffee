#!/usr/bin/env node
// Where a download lands, proven on disk and in the model's own ears.
//
//   node scripts/gateway-scratch.mjs [--core pi] [--port 8340] [--mock 8339] [--files 8341]
//
// The complaint: every workspace cwoffee has touched is littered with the model's
// leftovers — a fetched PDF, a scratch .py, three CSVs — filed among the person's own
// files, and `git status` shows them as untracked work. The rule the app now has: a file
// the model wanted for itself goes under `<workspace>/.cws/`, and only a file the person
// asked for lands in the folder proper. The model makes that call; the default is the
// scratch folder.
//
// What this gate insists on, in this order:
//
//   1. the bytes are where the tool claimed they are — checked against the filesystem,
//      not against the tool's prose; the reference copy is under .cws/, the requested
//      file is at the workspace root, and neither has a twin in the other place;
//   2. the scratch folder is polite in a repository: it carries a `.gitignore` saying `*`,
//      so the model's leftovers never show up as the person's untracked work;
//   3. the placement is *said* to the model. A tool that hides where it put a file leaves
//      the model to guess, and a guessing model tells the person the wrong thing;
//   4. neither call needed a permission card. A new file — in the scratch folder or in the
//      workspace — is the job; replacing one is the risk. If a card shows up here, the
//      verdict stopped following the placement and every download becomes a prompt.
//
// The scenario drives both calls from the scripted endpoint, so the URLs are fixed: the
// file server below listens on --files (8341 by default) and the scenario in
// tests/scenarios/scenarios.json names those two paths.
import { spawn } from 'node:child_process'
import http from 'node:http'
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
const PORT = Number(arg('port', 8340))
const MPORT = Number(arg('mock', 8339))
const FPORT = Number(arg('files', 8341))
// The scripted scenario names its URLs outright, so the file server's port is part of the
// fixture, not a knob. Passing --files and expecting the scenario to follow it would produce
// a red gate that blames the app for a port.
if (FPORT !== 8341) {
  console.log(
    `the scratch scenario in tests/scenarios/scenarios.json downloads from 127.0.0.1:8341; run this gate with --files 8341 (or change both) — got ${FPORT}`,
  )
  process.exit(2)
}
const DIR = path.resolve(arg('db', '/tmp/cwoffee-scratch-gate'))
const TOKEN = 'SCRATCH-TOK'
const PI_BIN = arg('pi', `${os.homedir()}/.local/share/pi-node/node-v22.23.2-linux-x64/bin/pi`)
const t = (ms) => new Promise((r) => setTimeout(r, ms))

const REF = 'REFERENCE PAYLOAD 0123456789 — fetched for the model\'s own use\n'
const CSV = 'city,population\nLima,9752000\nOsaka,2750000\n'

fs.rmSync(DIR, { recursive: true, force: true })
for (const d of ['ws', 'config', 'data', 'agent', 'sess']) fs.mkdirSync(path.join(DIR, d), { recursive: true })
const WS = path.join(DIR, 'ws')
fs.writeFileSync(path.join(WS, 'README.md'), '# the person\'s own file\n\nThis was here before cwoffee touched it.\n')

const kids = []
const fails = []
const notes = []

// ---- the file server the scenario downloads from ----------------------------------
const filesrv = http.createServer((req, res) => {
  if (req.url === '/reference.pdf') {
    res.writeHead(200, { 'content-type': 'application/pdf', 'content-length': Buffer.byteLength(REF) })
    res.end(REF)
  } else if (req.url === '/report.csv') {
    res.writeHead(200, { 'content-type': 'text/csv', 'content-length': Buffer.byteLength(CSV) })
    res.end(CSV)
  } else {
    res.writeHead(404).end()
  }
})
filesrv.listen(FPORT, '127.0.0.1')
await t(300)

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
console.log(`scratch gate on the ${CORE} core (mock :${MPORT}, files :${FPORT}, gateway :${PORT})`)
const gw = spawn(path.join(TGT, 'release', 'cwoffee-gateway'), gwArgs, {
  stdio: ['ignore', 'pipe', 'pipe'],
  env: {
    ...process.env,
    XDG_CONFIG_HOME: path.join(DIR, 'config'),
    XDG_DATA_HOME: path.join(DIR, 'data'),
    CWOFFEE_WORKSPACE: WS,
    CWOFFEE_URL: `http://127.0.0.1:${MPORT}/v1`,
    CWOFFEE_MODEL: 'cwoffee-mock',
    // ask mode on purpose: the gate counts the cards that open, and a card for a new file
    // in the person's own folder is exactly the noise this feature is meant to stop.
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
const events = []
let cards = 0
const cardSeen = []
ws.addEventListener('open', () => ws.send(JSON.stringify({ type: 'auth', token: TOKEN })))
ws.addEventListener('message', (ev) => {
  const v = JSON.parse(ev.data)
  if (v.type === 'auth_ok') return void (authed = true)
  if (v.type === 'event') {
    events.push(v.d)
    if (/approval/i.test(v.d?.kind ?? '')) {
      cards += 1
      const p = v.d.d ?? {}
      cardSeen.push(`${p.tool}: ${String(p.summary ?? '').slice(0, 70)}`)
      ws.send(JSON.stringify({ id: 9000 + cards, cmd: 'respond_approval', args: { id: p.id, answer: 'allow' } }))
    }
    return
  }
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
const settle = async (timeout = 120000) => {
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
  console.log('FAIL the scratch gateway never accepted the token:\n' + gwLog.slice(0, 600))
  process.exit(1)
}

const sess = (await call('new_session', { title: 'Scratch gate', workspace: WS })).data
await call('start_turn', {
  session: sess,
  text: `SCRATCH GATE: fetch the reference PDF at http://127.0.0.1:${FPORT}/reference.pdf for your own use, then get http://127.0.0.1:${FPORT}/report.csv as report.csv because I asked for that one, then say SCRATCH DONE.`,
})
await settle()

// ---- the filesystem is the authority, not the tool's prose -------------------------
const where = (p) => path.join(WS, p)
const readIf = (p) => {
  try {
    return fs.readFileSync(p, 'utf8')
  } catch {
    return null
  }
}
const scratchRef = readIf(where('.cws/reference.pdf'))
const rootRef = readIf(where('reference.pdf'))
const rootCsv = readIf(where('report.csv'))
const scratchCsv = readIf(where('.cws/report.csv'))

console.log('')
console.log(`  workspace root: ${fs.readdirSync(WS).sort().join(', ')}`)
console.log(`  .cws:           ${fs.existsSync(where('.cws')) ? fs.readdirSync(where('.cws')).sort().join(', ') : '(absent)'}`)

if (scratchRef !== REF)
  fails.push(`the reference download is not under .cws/ with the bytes it fetched: got ${JSON.stringify(scratchRef?.slice(0, 40) ?? null)}`)
if (rootRef !== null)
  fails.push('a file the model fetched for itself also landed in the person\'s folder — the placement is not the default')
if (rootCsv !== CSV)
  fails.push(`the file the person asked for did not land in the workspace proper: got ${JSON.stringify(rootCsv?.slice(0, 40) ?? null)}`)
if (scratchCsv !== null)
  fails.push('the requested file went into the scratch folder — `place: "keep"` did not reach the tool, or was ignored')

const ig = readIf(where('.cws/.gitignore'))
if (ig !== '*\n')
  fails.push(`.cws has no self-ignoring .gitignore (${JSON.stringify(ig)}): the model's leftovers would show up as the person's untracked work`)
else notes.push('.cws ignores itself in git')

// the person's own file is untouched by all of this
if (!readIf(where('README.md'))?.includes('before cwoffee touched it'))
  fails.push('the workspace\'s own file disappeared during the gate')

// ---- what the tool said, and what the model was told --------------------------------
const stored = (await call('session_messages', { id: sess, limit: 40 })).data ?? {}
const rows = stored.messages ?? []
const toolRows = rows.filter((m) => m.role === 'tool').map((m) => m.content ?? '')
const outs = toolRows.filter((c) => /saved|cws|report\.csv/i.test(c))
if (outs.length < 2)
  fails.push(`expected two download answers in the transcript, saw ${outs.length}: ${JSON.stringify(toolRows).slice(0, 220)}`)
for (const line of outs) console.log(`  tool said: ${JSON.stringify(line.slice(0, 120))}`)

const both = outs.join('\n')
if (both && !/\.cws\/reference\.pdf/.test(both))
  fails.push('the tool never said where the reference copy went — the model cannot tell the person what it cannot hear')
if (both && !/scratch folder/i.test(both))
  fails.push('the tool placed the file but gave no reason: a path is not a decision the model can reuse')
if (both && !/workspace proper/i.test(both))
  fails.push('the `keep` placement is unexplained: the model has no way to learn when to ask for one')

// A file that is not replacing anything is the job, not the risk. If this count moves,
// the risk verdict stopped following the destination and every fetch is a prompt.
if (cards)
  fails.push(
    `${cards} approval card(s) opened for creating new files — the verdict no longer matches the placement: ${JSON.stringify(cardSeen)}`,
  )
else notes.push('no card for a download that replaces nothing')

if (/REQUIRE FAILED/.test(mockLog))
  fails.push(`the placement never reached the model's prompt: ${mockLog.split('\n').filter((l) => /REQUIRE FAILED/.test(l)).join('\n')}`)
const assistant = rows.filter((m) => m.role === 'assistant').map((m) => m.content ?? '')
if (!assistant.some((a) => a.includes('SCRATCH DONE')))
  fails.push(`the scripted final answer never came (assistant said: ${JSON.stringify(assistant.slice(-1))})`)

console.log('')
for (const f of fails) console.log(`FAIL ${f}`)
if (fails.length) {
  console.log('SCRATCH GATE FAILED')
  if (/disk I\/O|panic|no scenario/.test(gwLog + mockLog))
    console.log('  first lines of what the processes said:\n' + (mockLog + gwLog).split('\n').filter(Boolean).slice(0, 8).join('\n'))
  if (process.env.DUMP_GW_LOG) console.log(gwLog)
} else {
  console.log(
    `SCRATCH GATE OK (${CORE}: reference copy under .cws/, requested file in the folder proper, ${notes.join('; ')}, and the placement stated in the tool's own words)`,
  )
  await call('delete_session', { id: sess }).catch(() => {})
}

ws.close()
for (const k of kids) k.kill('SIGTERM')
filesrv.close()
await t(700)
