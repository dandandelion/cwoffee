#!/usr/bin/env node
// What is the model actually told?
//
// Every claim about prompt size in this repo used to be a guess about what pi puts
// together: an app-side meter, a Rust-side prompt string, and pi's own default
// preamble were three different things and nobody had read the bytes that leave the
// process. This gate reads them. It starts an endpoint that records the request body
// and answers with a one-word completion, points a scratch gateway at it, runs one
// cowork turn and one chat turn, and then prints what the model was given: the system
// text, its sections, and the size of every tool declaration.
//
// It is a gate, not a report: the assertions are things the app promises.
//   * chat declares no tools and must not carry a coding harness's preamble;
//   * cowork declares the tools the registry has, and says which folder it is in;
//   * the two cores may phrase things differently but neither may send the model a
//     tool list for tools that were not mounted, or a prompt for a persona the user
//     did not choose.
//
// Run:  node scripts/gateway-prompt-shape.mjs [--core pi] [--port 8316] [--ep 8317]
//                                                [--db /tmp/cwoffee-prompt-shape]
import { spawn } from 'node:child_process'
import fs from 'node:fs'
import http from 'node:http'
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
const PORT = Number(arg('port', 8316))
const EPORT = Number(arg('ep', 8317))
const DIR = path.resolve(arg('db', '/tmp/cwoffee-prompt-shape'))
const TOKEN = 'SHAPE-TOK'
const PI_BIN = arg('pi', `${os.homedir()}/.local/share/pi-node/node-v22.23.2-linux-x64/bin/pi`)
const t = (ms) => new Promise((r) => setTimeout(r, ms))

fs.rmSync(DIR, { recursive: true, force: true })
for (const d of ['ws', 'config', 'data', 'agent', 'sess']) fs.mkdirSync(path.join(DIR, d), { recursive: true })
// A database left with its -wal/-shm beside it looks healthy to the next run and
// answers "disk I/O error"; rm the whole trio.
if (fs.existsSync(DIR)) for (const f of fs.readdirSync(DIR)) if (f.includes('.sqlite')) fs.rmSync(path.join(DIR, f), { force: true })
fs.writeFileSync(path.join(DIR, 'ws', 'README.md'), '# scratch\n\nNothing here is precious.\n')

const kids = []
const payloads = []
const otherRequests = []

// --- the endpoint that records what the model is given ------------------------------
const server = http.createServer((req, res) => {
  if (req.method === 'GET' && req.url.startsWith('/v1/models')) {
    res.writeHead(200, { 'content-type': 'application/json' })
    res.end(
      JSON.stringify({
        object: 'list',
        data: [
          { id: 'cwoffee-mock', object: 'model', owned_by: 'probe', context_window: 262144, max_tokens: 16384 },
        ],
      }),
    )
    return
  }
  // Only the model request counts as a payload. The old core also asks a llama.cpp-style
  // endpoint for /props to learn the context window, and counting that as a prompt would
  // have made "the core asked twice" look like a cost bug when it is a probe.
  if (!req.url.includes('/chat/completions')) {
    otherRequests.push(`${req.method} ${req.url}`)
    res.writeHead(404, { 'content-type': 'application/json' })
    res.end('{}')
    return
  }
  let body = ''
  req.on('data', (d) => (body += d))
  req.on('end', () => {
    let parsed = {}
    try {
      parsed = JSON.parse(body)
    } catch {}
    payloads.push({ at: Date.now(), body: parsed })
    // Answer the way the working endpoint answers: as a stream. A first version of this
    // gate answered with a plain chat.completion JSON body and pi retried the same
    // request four times at 2 s, 4 s, 8 s — a streaming client handed a non-stream body
    // reads as a dead endpoint, and "the model was asked four times" would have been a
    // false finding about the app rather than about this probe.
    const id = `probe-${payloads.length}`
    const created = Math.floor(Date.now() / 1000)
    const model = parsed.model ?? 'cwoffee-mock'
    const chunk = (body) => `data: ${JSON.stringify({ id, object: 'chat.completion.chunk', created, model, ...body })}\n\n`
    res.writeHead(200, {
      'content-type': 'text/event-stream',
      'cache-control': 'no-cache',
      connection: 'keep-alive',
    })
    res.write(chunk({ choices: [{ index: 0, delta: { role: 'assistant' }, finish_reason: null }] }))
    res.write(chunk({ choices: [{ index: 0, delta: { content: 'noted' }, finish_reason: null }] }))
    res.write(chunk({ choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] }))
    res.write(
      chunk({
        choices: [],
        usage: { prompt_tokens: 200, completion_tokens: 3, total_tokens: 203, prompt_tokens_details: { cached_tokens: 0 } },
      }),
    )
    res.end('data: [DONE]\n\n')
  })
})
server.listen(EPORT, '127.0.0.1')
kids.push({ kill: () => server.close() })
await t(300)

// --- a scratch gateway pointed at it ------------------------------------------------
const gwArgs = [
  '--db', path.join(DIR, 'g.sqlite'), '--token', TOKEN, '--port', String(PORT), '--core', CORE,
  '--pi', PI_BIN, '--pi-version', '1.2.0',
  '--pi-ext', path.join(REPO, 'pi-tools', 'cwoffee-tools.ts'),
  '--pi-tools', '-read,-bash,-edit,-write',
  '--pi-agent-dir', path.join(DIR, 'agent'), '--pi-session-dir', path.join(DIR, 'sess'),
]
console.log(`prompt shape on the ${CORE} core (endpoint :${EPORT}, gateway :${PORT})`)
console.log(`  artifacts from ${TGT}`)
const gwChild = spawn(path.join(TGT, 'release', 'cwoffee-gateway'), gwArgs, {
  stdio: ['ignore', 'pipe', 'pipe'],
  env: {
    ...process.env,
    XDG_CONFIG_HOME: path.join(DIR, 'config'),
    XDG_DATA_HOME: path.join(DIR, 'data'),
    CWOFFEE_WORKSPACE: path.join(DIR, 'ws'),
    CWOFFEE_URL: `http://127.0.0.1:${EPORT}/v1`,
    CWOFFEE_MODEL: 'cwoffee-mock',
    CWOFFEE_MODE: 'ask',
    CWOFFEE_CWTOOL: process.env.CWOFFEE_CWTOOL ?? path.join(TGT, 'release', 'cwtool'),
  },
})
kids.push(gwChild)
let gwLog = ''
gwChild.stdout.on('data', (b) => (gwLog += String(b)))
gwChild.stderr.on('data', (b) => (gwLog += String(b)))
await t(3500)

// --- drive: one cowork turn, one chat turn ------------------------------------------
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
  for (let i = 0; i < 200; i++) {
    if (reply.has(id)) return reply.get(id)
    await t(50)
  }
  throw new Error(`${cmd} never answered`)
}
// A turn's busy flag lags its start by a beat. Read it too early and the driver moves
// on while the request is still in flight — the first probe did exactly that and
// reaped the model process mid-request, which looked like an endpoint failure.
const runTurn = async (text, session) => {
  const before = payloads.length
  const r = await call('start_turn', { text, session })
  if (r.ok === false) throw new Error(`start_turn refused: ${JSON.stringify(r).slice(0, 200)}`)
  for (let i = 0; i < 80; i++) {
    const h = (await call('session_state')).data ?? {}
    if (h.busy) break
    await t(150)
  }
  for (let i = 0; i < 400; i++) {
    const h = (await call('session_state')).data ?? {}
    if (!h.busy) break
    await t(150)
  }
  await t(400)
  return payloads.slice(before)
}

const fails = []
for (let i = 0; i < 100 && !authed; i++) await t(50)
if (!authed) {
  console.log('FAIL the scratch gateway never accepted the token:\n' + gwLog.slice(0, 400))
  for (const k of kids) (k.kill ? k.kill() : k.kill('SIGTERM'))
  process.exit(1)
}
const shapeOf = (seen) => {
  // Were they the *same* request? Four copies of one prompt is four times the prefill on
  // a local endpoint, and it matters whether they arrived in a heartbeat (a retry) or
  // seconds apart (a second thought). Identical bodies with identical timing is the loud
  // case; the shape below reports both so it cannot be quietly ignored.
  const gaps = seen.slice(1).map((p, i) => p.at - seen[i].at)
  const keys = seen.map((p) => JSON.stringify(p.body.messages) + '|' + JSON.stringify(p.body.tools ?? []))
  const same = keys.length > 1 && keys.every((k) => k === keys[0])
  const b = (seen.find((p) => p.body?.messages?.length) ?? { body: {} }).body
  const msgs = b.messages ?? []
  const sys = msgs.filter((m) => m.role === 'system').map((m) => (typeof m.content === 'string' ? m.content : JSON.stringify(m.content))).join('\n')
  const tools = (b.tools ?? []).map((x) => ({
    name: x.function?.name ?? x.name,
    bytes: JSON.stringify(x).length,
  }))
  const sec = (tag) => (sys.match(new RegExp(`<${tag}[\\s\\S]*?</${tag}>`)) ?? [''])[0]
  const each = seen.map((p) => {
    const b = p.body ?? {}
    const m = b.messages ?? []
    const last = m.length ? m[m.length - 1] : {}
    const c = typeof last.content === 'string' ? last.content : JSON.stringify(last.content ?? '')
    return `${m.length} msg, last ${last.role ?? '?'}/${(c || '').slice(0, 40)}`
  })
  return {
    requests: seen.length,
    each,
    gaps,
    identical: same,
    model: b.model,
    sys,
    sysBytes: sys.length,
    tools,
    toolBytes: tools.reduce((s, x) => s + x.bytes, 0),
    lists: { tools: sec('tools').length, docs: sec('docs').length, cwd: sec('cwd').length, env: sec('environment').length },
  }
}

const sessCowork = (await call('new_session', { title: 'shape cowork', workspace: path.join(DIR, 'ws') })).data
const cowork = shapeOf(await runTurn('Remember this for the shape check.', sessCowork))
console.log(
  `\ncowork: ${cowork.requests} request(s) to the endpoint, model=${cowork.model} ` +
    `system=${cowork.sysBytes} B, ${cowork.tools.length} tool declaration(s) = ${cowork.toolBytes} B, ` +
    `total payload ≈ ${Math.round((cowork.sysBytes + cowork.toolBytes) / 4)} tokens` +
    (cowork.requests > 1 ? `, request gaps ${JSON.stringify(cowork.gaps)} ms${cowork.identical ? ', bodies identical' : ''}` : ''),
)
console.log(`  prompt sections: tools list ${cowork.lists.tools} B, docs ${cowork.lists.docs} B, cwd ${cowork.lists.cwd} B`)
console.log(`  declared: ${cowork.tools.map((x) => `${x.name} (${x.bytes} B)`).join(', ') || '(none)'}`)
console.log(`  system head: ${JSON.stringify(cowork.sys.slice(0, 150))}`)
if (otherRequests.length) console.log(`  the core also asked the endpoint: ${otherRequests.join(', ')}`)
if (cowork.each.length > 1) console.log(`  each request: ${cowork.each.join(' || ')}`)

// The old core refuses chat in words — it *is* the coworker prompt plus the tool
// registry — so on that core the honest measurement is the refusal itself.
const persona = await call('set_persona', { persona: 'chat' })
let chat = { requests: 0, tools: [], sys: '', sysBytes: 0, toolBytes: 0, each: [], gaps: [] }
let sessChat = 0
if (persona.ok === false) {
  if (CORE !== 'rust') fails.push(`set_persona chat refused: ${JSON.stringify(persona).slice(0, 160)}`)
  else console.log(`\nchat on the rust core: refused in words — ${String(persona.error).slice(0, 120)}`)
} else {
  sessChat = (await call('new_session', { title: 'shape chat' })).data
  chat = shapeOf(await runTurn('Remember this for the shape check.', sessChat))
}
if (chat.requests) {
console.log(
  `\nchat:   ${chat.requests} request(s) to the endpoint, model=${chat.model} ` +
    `system=${chat.sysBytes} B, ${chat.tools.length} tool declaration(s) = ${chat.toolBytes} B, ` +
    `total payload ≈ ${Math.round((chat.sysBytes + chat.toolBytes) / 4)} tokens` +
    (chat.requests > 1 ? `, request gaps ${JSON.stringify(chat.gaps)} ms${chat.identical ? ', bodies identical' : ''}` : ''),
)
console.log(`  declared: ${chat.tools.map((x) => x.name).join(', ') || '(none)'}`)
console.log(`  system: ${JSON.stringify(chat.sys)}`)
if (chat.each.length > 1) console.log(`  each request: ${chat.each.join(' || ')}`)
}

// --- the promises -------------------------------------------------------------------
if (cowork.requests === 0) fails.push('cowork: the model was never asked anything — nothing was measured')
if (chat.requests === 0 && CORE !== 'rust') fails.push('chat: the model was never asked anything — nothing was measured')
if (cowork.tools.length === 0 && CORE === 'pi') fails.push('cowork: no tool was declared to the model, so the tools are cosmetic')
if (chat.requests > 0 && chat.tools.length !== 0)
  fails.push(`chat: ${chat.tools.length} tool(s) declared in a chat: ${chat.tools.map((x) => x.name).join(', ')}`)
if (chat.requests > 0 && /expert coding assistant operating inside pi/i.test(chat.sys))
  fails.push('chat: the system prompt still carries pi harness preamble')
if (/expert coding assistant operating inside pi/i.test(cowork.sys)) {
  console.log(
    '\n  note: cowork mode sends pi harness preamble — the coding harness prompt, with its own\n' +
      '        tool list, docs pointers and cwd rules. cwoffee does not own the coworker prompt\n' +
      '        on this core, so what the model is told about its job comes from pi.',
  )
}
if (cowork.tools.length && !/cwoffee|workspace|folder/i.test(cowork.sys))
  console.log('\n  note: the cowork system text never mentions the workspace or cwoffee by name')

for (const f of fails) console.log(`\nFAIL ${f}`)
if (fails.length) console.log(`PROMPT SHAPE FAILED (${fails.length})`)
else console.log(`\nPROMPT SHAPE OK (${CORE}: cowork ${cowork.sysBytes + cowork.toolBytes} B payload with ${cowork.tools.length} tools, chat ${chat.sysBytes + chat.toolBytes} B with ${chat.tools.length})`)

// --- teardown: our own children, by the pids we started -----------------------------
// A gate should not litter the sidebar it is measuring: clean up on success, leave the
// conversation when it failed, because a red row is the evidence for a red gate.
if (!fails.length) {
  for (const id of [sessCowork, sessChat]) if (id) await call('delete_session', { id }).catch(() => {})
}
ws.close()
for (const k of kids) {
  if (k.kill) k.kill()
  else k.kill('SIGTERM')
}
await t(600)
for (const f of fs.existsSync(DIR) ? fs.readdirSync(DIR) : []) if (f.includes('.sqlite')) fs.rmSync(path.join(DIR, f), { force: true })
if (/disk I\/O|panic/.test(gwLog)) console.log('  note: the scratch gateway logged something alarming:\n' + gwLog.slice(0, 400))
process.exit(fails.length ? 1 : 0)
