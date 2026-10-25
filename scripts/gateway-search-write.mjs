// A provider click has to survive the trip to the config file.
//
// The report: "can't switch search engine", with the app quoting
//
//   TOML parse error at line 44, column 11
//     44 | backend = auto
//        |           ^
//   invalid string expected `"`, `'`
//
// — and saving nothing. The pi core built the `[search]` lines by hand and wrote the value
// bare; the config writer, which re-parses edited text before replacing the file so that a
// settings click can never write a config the next boot cannot open, then refused the edit it
// had been asked to make. The user watched a control refuse, in the app's own words, to do
// the thing the user had just asked it to do.
//
// Unit tests on both cores quote-check the writer. This gate asks the question the way the
// panel does: over the wire, against a config file that already has a `[search]` section in
// it (that is where the line numbers in the report come from), and it finishes by parsing
// the file the click produced with a parser that belongs to neither core.
//
//   node scripts/gateway-search-write.mjs [--port 8168] [--core pi|rust]
//
// It starts its own mock endpoint, gateway, config, data dir and workspace, and touches
// nothing under ~/.config or ~/.local/share.

import { spawn } from 'node:child_process'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const TGT = process.env.CWOFFEE_TARGET_DIR ?? path.join(REPO, 'target')
const argv = process.argv.slice(2)
const get = (f, d) => {
  const i = argv.indexOf(f)
  return i >= 0 ? argv[i + 1] : d
}
const PORT = Number(get('--port', '8168'))
const MPORT = Number(get('--mock-port', String(PORT + 1)))
const TOKEN = get('--token', 'SEARCHWRITE')
const CORE = get('--core', 'pi')
const PI_BIN = process.env.CWOFFEE_PI_BIN ?? 'pi'

const t = (ms) => new Promise((r) => setTimeout(r, ms))
const DIR = fs.mkdtempSync(path.join(os.tmpdir(), 'cwoffee-searchwrite.'))
const kids = []
const cleanup = () => {
  for (const k of kids) {
    try {
      k.kill()
    } catch {}
  }
  fs.rmSync(DIR, { recursive: true, force: true })
}
process.on('exit', cleanup)
process.on('SIGINT', cleanup)
process.on('SIGTERM', cleanup)

const cfgDir = path.join(DIR, 'config', 'cwoffee')
fs.mkdirSync(cfgDir, { recursive: true })
fs.mkdirSync(path.join(DIR, 'ws'), { recursive: true })
fs.writeFileSync(path.join(DIR, 'ws', 'README.md'), '# scratch\n\nNothing here is precious.\n')
// The shape the user's file had: a `[search]` section that already exists, so an edit
// replaces lines inside it rather than appending a fresh section at the end of the file.
const CFG = path.join(cfgDir, 'config.toml')
fs.writeFileSync(
  CFG,
  ['persona = "cowork"', '', '[search]', 'backend = "auto"', 'max_results = 8', ''].join('\n'),
)

const fail = []
const notes = []

const mock = spawn(
  process.execPath,
  [path.join(REPO, 'scripts', 'mock-llm.mjs'), '--port', String(MPORT), '--speed', '1'],
  { stdio: ['ignore', 'pipe', 'pipe'] },
)
kids.push(mock)
await t(900)

const gwArgs = [
  '--db', path.join(DIR, 'g.sqlite'), '--token', TOKEN, '--port', String(PORT), '--core', CORE,
]
if (CORE === 'pi') {
  gwArgs.push(
    '--pi', PI_BIN, '--pi-version', '1.2.0',
    '--pi-ext', path.join(REPO, 'pi-tools', 'cwoffee-tools.ts'),
    '--pi-tools', '-read,-bash,-edit,-write',
    '--pi-agent-dir', path.join(DIR, 'agent'),
    '--pi-session-dir', path.join(DIR, 'sess'),
  )
}
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
gw.stdout.on('data', (b) => { gwLog += b.toString() })
gw.stderr.on('data', (b) => { gwLog += b.toString() })

// The gateway is up when it answers. Polling a port says only that something listens.
let ws = null
for (let i = 0; i < 60 && !ws; i++) {
  try {
    ws = new WebSocket(`ws://127.0.0.1:${PORT}/ws`)
  } catch {
    ws = null
  }
  if (ws) {
    await new Promise((res) => {
      ws.addEventListener('open', res, { once: true })
      ws.addEventListener('error', () => { ws = null; res() }, { once: true })
      setTimeout(res, 1000)
    })
    if (ws) break
  }
  await t(500)
}
if (!ws) {
  console.log(gwLog)
  console.log('SEARCH WRITE FAILED\n  the gateway never answered on port ' + PORT)
  process.exit(1)
}

let rpcId = 0
const replies = new Map()
const rpc = (cmd, args = {}) => ws.send(JSON.stringify({ id: ++rpcId, cmd, args }))
ws.addEventListener('message', (ev) => {
  const v = JSON.parse(ev.data)
  if (v.ok === true) replies.set(v.id, v.data)
  else if (v.ok === false) replies.set(v.id, { error: v.error })
})
const wait = async (ids) => {
  for (let i = 0; i < 200 && !ids.every((n) => replies.has(n)); i++) await t(100)
  return ids.map((n) => replies.get(n))
}
// The socket is open by here: the loop above only breaks once the `open` event fired.
ws.send(JSON.stringify({ type: 'auth', token: TOKEN }))
for (let i = 0; i < 100 && !replies.size; i++) await t(100)

// 1. what the panel reads, before anything is clicked
rpc('search_settings', {})
const [before] = await wait([1])
if (!before || before.error) fail.push(`the panel cannot read its own search settings: ${JSON.stringify(before)}`)
else if (before.backend !== 'auto')
  fail.push(`a config that says backend = "auto" reads as ${JSON.stringify(before.backend)}`)

// 2. the click the user made: SearXNG, with the URL the panel asks for
rpc('set_search_backend', { backend: 'searxng', searxng_url: 'http://127.0.0.1:8080/search' })
const [setRes] = await wait([2])
if (setRes?.error) {
  fail.push(`the click was refused: ${setRes.error}`)
  notes.push('the refusal is the reported failure; the config file is printed below')
}
const file = fs.readFileSync(CFG, 'utf8')

// 3. what the panel reads afterwards — the click has to be visible on the next look
rpc('search_settings', {})
const [after] = await wait([3])
if (!after || after.error) fail.push(`the panel cannot read its settings after the click: ${JSON.stringify(after)}`)
else if (after.backend !== 'searxng')
  fail.push(`after clicking SearXNG the panel says ${JSON.stringify(after.backend)}; the click saved nothing`)
else if (after.searxng_url !== 'http://127.0.0.1:8080/search')
  fail.push(`the panel kept the choice but lost the URL (${JSON.stringify(after.searxng_url)})`)

// 4. the file itself has to be readable by the next boot. The core that wrote it re-parses it
// on the way in, which is the guard that produced the user's error text — so the check here
// is a parser that belongs to neither core, not a grep for a quote mark.
let parsed = null
try {
  const py = spawn('python3', ['-c', 'import sys,tomllib;d=tomllib.load(open(sys.argv[1],"rb"));print(__import__("json").dumps(d))', CFG])
  let out = ''
  let err = ''
  py.stdout.on('data', (b) => { out += b.toString() })
  py.stderr.on('data', (b) => { err += b.toString() })
  await new Promise((res) => py.on('exit', res))
  parsed = err.trim() ? { error: err.trim() } : JSON.parse(out)
} catch (e) {
  notes.push(`no TOML parser available to re-read the file: ${e}`)
}
if (parsed?.error) {
  fail.push(`the file the click wrote does not parse: ${parsed.error}`)
  console.log('--- config.toml as written ---\n' + file + '---')
} else if (parsed) {
  const s = parsed.search ?? {}
  if (s.backend !== 'searxng') fail.push(`the written file says backend=${JSON.stringify(s.backend)}`)
  if (s.searxng_url !== 'http://127.0.0.1:8080/search')
    fail.push(`the written file lost the URL: ${JSON.stringify(s)}`)
  if (s.max_results !== 8) fail.push(`the click rewrote a key nobody touched: max_results=${JSON.stringify(s.max_results)}`)
}

// 5. and back to a backend that needs no URL. The URL itself stays in the file and in the
// panel's box on purpose — a person who tried SearXNG and went back should not have to type
// their address again to try it once more — but the *choice* has to move, and the file has to
// still parse with both keys in it.
rpc('set_search_backend', { backend: 'duckduckgo', searxng_url: '' })
await wait([4])
rpc('search_settings', {})
const [back] = await wait([5])
if (back?.backend !== 'duckduckgo') fail.push(`switching back left the panel at ${JSON.stringify(back?.backend)}`)
const file2 = fs.readFileSync(CFG, 'utf8')
if (/^\s*backend\s*=\s*[^"]*[^"\s]\s*$/m.test(file2))
  fail.push(`the file now holds a bare value again:\n${file2}`)

console.log(`search write (${CORE} core): before=${before?.backend} clicked=searxng after=${after?.backend} back=${back?.backend}`)
if (after) console.log(`  panel after the click: url=${JSON.stringify(after.searxng_url)} auto=${JSON.stringify(after.auto?.status ?? after.auto)}`)
for (const n of notes) console.log(`  note: ${n}`)
if (fail.length) {
  for (const f of fail) console.log(`  FAIL ${f}`)
  console.log(gwLog.split('\n').filter((l) => /search|config|toml/i.test(l)).join('\n'))
  console.log('SEARCH WRITE FAILED')
  process.exit(1)
}
console.log('SEARCH WRITE OK (a provider click writes a file the next boot can read)')
