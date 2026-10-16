#!/usr/bin/env node
// The title feature, end to end: a conversation's name arrives after the turn, from a
// helper process, and the app either honours it or says why it did not.
//
// Two things are being proved, and they are proved separately on purpose:
//
//   * THE PLUMBING (always runs). The app must ask the helper with the conversation's own
//     first message, must move the sidebar row when the name comes back, must leave a
//     refused name exactly as it was, and must say the reason out loud rather than
//     silently keeping the automatic title. A scripted helper proves the app's behaviour
//     without waiting on a 460 MB download; the placeholder weight file exists only to
//     satisfy the presence check, and the log says so.
//   * THE REAL HELPER (runs when it is installed). `titlegen.py --check` answers, in one
//     second and without loading torch, whether the venv and the weights are really
//     there — which is the question a settings panel would otherwise have to ask by
//     loading a model.
//
// Nothing here may run on the user's config: the endpoint is the fixture, the database is
// a temp file, and the title helper is whatever this script wrote.
//
//   node scripts/gateway-titlegen.mjs            # needs target/release/cwoffee-gateway
//   GW=path/to/gateway MOCK=path/to/mock-llm.mjs node scripts/gateway-titlegen.mjs

import { spawn } from 'node:child_process'
import net from 'node:net'
import os from 'node:os'
import path from 'node:path'
import fs from 'node:fs'
import { fileURLToPath } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))
const root = path.join(here, '..')
const GW = process.env.GW ?? path.join(root, 'target/release/cwoffee-gateway')
const MOCK = process.env.MOCK ?? path.join(here, 'mock-llm.mjs')
const realScript0 = path.join(here, 'titlegen.py')
const REAL_PY = process.env.CWOFFEE_TITLEGEN_REAL_PYTHON ??
  path.join(os.homedir(), '.local/share/cwoffee/titlegen/venv/bin/python')
const T = Number(process.env.TITLEGEN_GATE_TIMEOUT ?? 25_000)

let fails = 0
const check = (c, m) => {
  if (c) console.log(`  ok   ${m}`)
  else {
    fails++
    console.log(`  FAIL ${m}`)
  }
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

async function freePort() {
  return new Promise((res, rej) => {
    const s = net.createServer()
    s.on('error', rej)
    // Hand the number back and let go of the socket: a probe that keeps the port is a
    // probe that tells the real server it is already in use.
    s.listen(0, '127.0.0.1', () => {
      const p = s.address().port
      s.close(() => res(p))
    })
  })
}

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'cwoffee-titlegen-'))
const owned = []
const spawnTracked = (cmd, args, opts) => {
  const p = spawn(cmd, args, { stdio: ['ignore', 'pipe', 'pipe'], ...opts })
  owned.push(p)
  return p
}
function cleanup() {
  for (const p of owned) {
    try {
      p.kill('SIGKILL')
    } catch {}
  }
  // A sqlite file without its -wal and -shm is a half file the next opener calls a
  // disk I/O error, so all three go together.
  for (const suffix of ['', '-wal', '-shm']) {
    try {
      fs.rmSync(path.join(tmp, 'gw.sqlite' + suffix), { force: true })
    } catch {}
  }
  try {
    fs.rmSync(tmp, { recursive: true, force: true })
  } catch {}
}
process.on('exit', cleanup)

// --- the fixture endpoint, on a port of our own -------------------------------
const mockPort = await freePort()
const mock = spawnTracked('node', [MOCK, '127.0.0.1', String(mockPort)])
const mockLines = []
mock.stderr.on('data', (d) => mockLines.push(String(d)))
for (let i = 0; i < 60 && !mockLines.some((l) => /cwoffee-mock|listening/i.test(l)); i++) await sleep(100)

// --- the helper, scripted -----------------------------------------------------
// The real helper loads torch and a model; this one answers, or refuses, so the app's
// side of the bargain can be watched without a GPU, a download, or a 870 MB spike.
const asked = path.join(tmp, 'asked.txt')
const okHelper = path.join(tmp, 'fake_ok.py')
const refuseHelper = path.join(tmp, 'fake_refuse.py')
fs.writeFileSync(
  okHelper,
  `import json, sys
text = sys.stdin.read()
open(${JSON.stringify(asked)}, "a").write(text + "\\n")
print(json.dumps({"title": "Quarterly cashflow gap", "lang": "en", "quoted": True, "load_ms": 3800,
                 "gen_ms": 310, "peak_rss_mb": 861, "thread_cap": 4}), file=sys.stderr)
print("Quarterly cashflow gap")
`,
)
fs.writeFileSync(
  refuseHelper,
  `import json, sys
sys.stdin.read()
print(json.dumps({"title": "", "candidate": "Sure, here is a summary of your cashflow",
                  "rejected": "that is an answer, not a name", "quoted": False}), file=sys.stderr)
`,
)
// The presence check the app runs before spawning anything: a directory with a
// .safetensors in it. Placeholder on purpose — this phase is about the app asking, not
// about the weights, and the real weights are checked separately below.
const cache = path.join(tmp, 'cache')
const placeholder = path.join(cache, 'models--LiquidAI--LFM2.5-230M', 'snapshots', 'ab12cd')
fs.mkdirSync(placeholder, { recursive: true })
fs.writeFileSync(path.join(placeholder, 'model.safetensors'), '')

async function phase(name, helper, expect, python, turnText, cacheDir) {
  const ask = turnText ?? 'What is 6 times 7?'
  console.log(`\n--- ${name} ---`)
  const port = await freePort()
  const gw = spawnTracked(GW, ['--port', String(port), '--db', path.join(tmp, 'gw.sqlite'),
    '--url', `http://127.0.0.1:${mockPort}/v1`, '--model', 'cwoffee-mock',
    '--workspace', tmp, '--token', 'TG'], {
    env: {
      ...process.env,
      // A gate must not read the developer's config for this: the environment decides.
      CWOFFEE_TITLEGEN: '1',
      CWOFFEE_TITLEGEN_PYTHON: python ?? process.env.PY ?? 'python3',
      CWOFFEE_TITLEGEN_SCRIPT: helper,
      CWOFFEE_TITLEGEN_CACHE: cacheDir ?? cache,
      CWOFFEE_TITLEGEN_MODEL: 'LiquidAI/LFM2.5-230M',
      CWOFFEE_TITLEGEN_TIMEOUT_SECS: '20',
    },
  })
  const gwErr = []
  gw.stderr.on('data', (d) => gwErr.push(String(d)))
  const url = `ws://127.0.0.1:${port}/ws`
  let ws = null
  for (let i = 0; i < 60 && !ws; i++) {
    try {
      ws = new WebSocket(url)
      await new Promise((res, rej) => {
        ws.onopen = res
        ws.onerror = rej
        setTimeout(() => rej(new Error('connect timeout')), 2000)
      })
    } catch {
      ws = null
      await sleep(300)
    }
  }
  if (!ws) throw new Error(`gateway never answered on :${port}\n${gwErr.join('')}`)

  let seq = 0
  const pending = new Map()
  const events = []
  ws.onmessage = (ev) => {
    const f = JSON.parse(ev.data)
    if (f.id && pending.has(f.id)) {
      const p = pending.get(f.id)
      pending.delete(f.id)
      p(f)
    } else events.push(f)
  }
  const call = (cmd, args = {}) =>
    new Promise((res, rej) => {
      const id = ++seq
      pending.set(id, (v) => (v.ok ? res(v.data) : rej(new Error(String(v.error)))))
      ws.send(JSON.stringify({ id, cmd, args }))
      setTimeout(() => pending.has(id) && (pending.delete(id), rej(new Error('timeout ' + cmd))), 20_000)
    })
  // Auth is a frame of its own, not an RPC verb: it happens before the socket is
  // allowed to speak the command language at all.
  ws.send(JSON.stringify({ type: 'auth', token: 'TG' }))
  const authed = await new Promise((res) => {
    const t = setInterval(() => {
      const i = events.findIndex((f) => f.type === 'auth_ok')
      if (i >= 0) {
        clearInterval(t)
        clearTimeout(to)
        res(true)
      }
    }, 30)
    const to = setTimeout(() => {
      clearInterval(t)
      res(false)
    }, 5000)
  })
  if (!authed) throw new Error(`no auth_ok on :${port} — frames: ${JSON.stringify(events.slice(0, 4))}`)

  const id = await call('new_session', { title: 'new session', workspace: tmp, model: 'cwoffee-mock' })
  events.length = 0
  await call('start_turn', { session: id, text: ask })
  // The store names the row from its own first line the moment that line lands, so
  // "the title is not 'new session' any more" is not the assertion — it is true before the
  // helper is even asked. What matters is whether the row moved *off* the automatic title.
  const long = ask.length > 44
  const isAuto = (t) => !t || t === 'new session' || t === ask || (long && t.startsWith(ask.slice(0, 44)))
  const settle = expect === 'refused' ? 10_000 : T
  const deadline = Date.now() + settle
  let title = null
  while (Date.now() < deadline) {
    const rows = await call('list_sessions', { limit: 50 })
    const row = rows.find((r) => r.id === id)
    title = row?.title ?? null
    if (title && (expect === 'refused' ? title !== 'new session' : !isAuto(title))) break
    await sleep(200)
  }
  if (expect === 'refused') {
    // The point of this phase is that nothing arrives. Read once more after the window,
    // in case something lands late.
    await sleep(1500)
    const rows = await call('list_sessions', { limit: 50 })
    title = rows.find((r) => r.id === id)?.title ?? title
  }
  const notes = events.filter((f) => /note/i.test(f.kind ?? '') || /note/i.test(f.d?.kind ?? ''))
  const moved = events.filter((f) => /sessionChanged/i.test(f.kind ?? '') || /sessionChanged/i.test(f.d?.kind ?? ''))
  const text = fs.existsSync(asked) ? fs.readFileSync(asked, 'utf8') : ''

  if (expect === 'named-real') {
    // The real model, the real weights, a real sentence. This is the half a scripted
    // helper cannot prove: that a 230M parameter model on four threads actually hands
    // back a phrase worth reading in a sidebar, rather than a sentence about itself.
    check(
      typeof title === 'string' && title.length > 0 && !isAuto(title),
      `the real model renamed the row to ${JSON.stringify(title)} (automatic: ${JSON.stringify(ask.slice(0, 44))})`,
    )
  } else if (expect === 'named') {
    check(title === 'Quarterly cashflow gap', `the sidebar row now reads ${JSON.stringify(title)}`)
    check(text.includes(ask), 'the helper was handed the conversation’s own first message')
    check(moved.length > 0, 'the rename is announced, so the row moves without a reload')
    check(
      notes.every((f) => !/did not name/i.test(JSON.stringify(f))),
      'a successful naming writes no failure note',
    )
  } else {
    const kept = isAuto(title) && title !== 'new session'
    check(kept, `a refused name leaves the automatic title alone (row: ${JSON.stringify(title)})`)
    check(
      notes.some((f) => /did not name this conversation/i.test(JSON.stringify(f))),
      'the refusal is said out loud, with the helper’s reason',
    )
    check(
      gwErr.some((l) => /keeps its automatic title/i.test(l)),
      'the reason is in the log too, for whoever is debugging the install',
    )
  }
  try {
    ws.close()
  } catch {}
  gw.kill('SIGTERM')
  await sleep(400)
  return { gwErr }
}

const first = await phase('a helper that answers moves the row', okHelper, 'named')
// A second gateway, a second database row, a helper that refuses. The title the store
// made up ("What is 6 times 7?") must still be there afterwards.
fs.writeFileSync(asked, '')
await phase('a helper that refuses changes nothing', refuseHelper, 'refused')

// A real sentence, named by the real model. Skipped, not faked, when the weights or the
// venv are not installed on this machine.
if (fs.existsSync(REAL_PY)) {
  fs.writeFileSync(asked, '')
  await phase(
    'the real model names a real conversation',
    realScript0,
    'named-real',
    REAL_PY,
    'hey can you look at my quarterly cashflow numbers, they seem short',
    // The real weights, in the real place. Pointing the real helper at this phase's
    // placeholder cache would make it refuse, and the failure would look like a model
    // problem when it is a gate problem.
    path.join(path.dirname(REAL_PY), '..', '..', 'models'),
  )
} else {
  console.log(`\n--- skip: no real venv at ${REAL_PY} ---`)
}

// --- the real helper, if it is installed --------------------------------------
console.log('\n--- the real helper ---')
const realScript = realScript0
if (fs.existsSync(REAL_PY) && fs.existsSync(realScript)) {
  const r = await new Promise((res) => {
    const p = spawn(REAL_PY, [realScript, '--check'], {
      env: { ...process.env, HF_HUB_OFFLINE: '1' },
      stdio: ['ignore', 'pipe', 'pipe'],
    })
    let out = ''
    p.stdout.on('data', (d) => (out += String(d)))
    p.on('exit', (code) => res({ code, out }))
  })
  let j = null
  try {
    j = JSON.parse(r.out.trim())
  } catch {}
  check(j && j.ok === true && j.weights_cached === true && j.torch_ok === true,
    `the real helper answers in one second: ${r.out.trim().slice(0, 160)}`)
  if (!j) console.log(`  (raw) ${r.out.trim()}`)
} else {
  console.log(`  skip  no real venv at ${REAL_PY} — the plumbing above still ran`)
}

console.log('')
if (fails) {
  console.log(`\nTITLE GATE FAILED (${fails})`)
  console.log((first.gwErr ?? []).join('').slice(-800))
  process.exit(1)
} else {
  console.log('TITLE GATE OK (the name arrives after the turn, and a refusal changes nothing)')
}
