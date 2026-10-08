#!/usr/bin/env node
/**
 * mock-llm — a scripted stand-in for llama.cpp's llama-server.
 *
 * Why this exists: the test harness used to drive a real local model, which means
 * every self-test run pushed a ~200k-token conversation through a single-slot
 * endpoint and invalidated the prompt cache the user is relying on. A self-test
 * must not cost anyone an afternoon of prefill.
 *
 * What it is NOT: a fake *model*. Nothing here proves that a real model follows
 * instructions, that a quantisation mangles tool JSON at some measurable rate, or
 * that prefill takes any particular time. Those need the real endpoint, and
 * `scripts/selftest.sh --real-endpoint` is the deliberate way to pay for them.
 *
 * What it IS: the wire, faithfully. The engine's own OpenAI/llama.cpp provider,
 * SSE parser, tool-call fragment accumulation, JSON repair, telemetry from
 * `timings`, and the whole streaming path in the UI run unmodified against it.
 * Frames are emitted in llama.cpp's shape: role delta, `reasoning_content`
 * deltas, `tool_calls` with split `function.arguments`, a finish chunk, then a
 * usage+`timings` chunk, then `[DONE]`.
 *
 * Behaviour comes from tests/scenarios/scenarios.json. Selection is stateless:
 * scenario = last user message matches `match`; step = number of tool results
 * already in the request. A prompt with no scenario is an HTTP 400 naming the
 * prompt, not a plausible-sounding filler - a stub that is almost right is worse
 * than an error, because the harness would then "pass" while testing nothing.
 *
 * A step may also carry `require`: strings that must appear in the tool results of
 * the request that plays it. The canned answer is not evidence that a tool ran; the
 * bytes the engine put back into the prompt are. So a scenario can insist, for
 * example, that `sheet_read` really returned a cell grid before it is allowed to
 * answer from one.
 *
 * Routes:
 *   GET  /health              -> ok
 *   GET  /v1/models           -> the mock model list (meta.n_ctx, input_modalities)
 *   GET  /props               -> llama.cpp-style props (model_alias, chat_template)
 *   POST /v1/chat/completions -> SSE, scripted
 *   GET  /mock/state          -> instrumentation: hits, misses, thinking flags seen
 *   POST /mock/reset          -> clear instrumentation
 */

import http from 'node:http'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const HERE = path.dirname(fileURLToPath(import.meta.url))

// ---- args ------------------------------------------------------------------

function arg(name, fallback = undefined) {
  const i = process.argv.indexOf(`--${name}`)
  if (i >= 0 && i + 1 < process.argv.length) return process.argv[i + 1]
  return fallback
}
const flag = (name) => process.argv.includes(`--${name}`)

const PORT = Number(arg('port', process.env.CWOFFEE_MOCK_PORT || 8190))
const HOST = arg('host', '127.0.0.1')
const SPEED = Math.max(0.05, Number(arg('speed', process.env.CWOFFEE_MOCK_SPEED || '1')))
const SCENARIOS = arg('scenarios', path.join(HERE, '..', 'tests', 'scenarios', 'scenarios.json'))
const ALLOW_UNKNOWN = flag('allow-unknown')
const RECORD = arg('record')

if (HOST !== '127.0.0.1' && HOST !== 'localhost') {
  // A scripted endpoint has no auth. It must not become a door.
  console.error(`mock-llm: refusing to bind ${HOST}: loopback only (a mock has no auth)`)
  process.exit(2)
}

// ---- scenario script -------------------------------------------------------

const script = JSON.parse(fs.readFileSync(SCENARIOS, 'utf8'))
const defaults = script.defaults ?? {}
const MODEL = script.model ?? 'cwoffee-mock'

const stats = {
  requests: 0,
  turns: 0,
  hits: {},
  misses: [],
  thinking_off: 0,
  thinking_on: 0,
  tools_requested: 0,
  sampling_seen: [],
  last: [],
}

const log = (...a) => console.error('[mock-llm]', ...a)

/**
 * The last user message's text. OpenAI multimodal style puts parts in an array;
 * llama.cpp also accepts a plain string.
 */
function lastUserText(messages) {
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i]
    if (m.role !== 'user') continue
    if (typeof m.content === 'string') return m.content
    if (Array.isArray(m.content))
      return m.content
        .map((p) => (typeof p === 'string' ? p : (p?.text ?? '')))
        .join('\n')
    return ''
  }
  return ''
}

function textOf(m) {
  if (typeof m.content === 'string') return m.content
  if (Array.isArray(m.content))
    return m.content.map((p) => (typeof p === 'string' ? p : (p?.text ?? ''))).join('\n')
  return ''
}

/**
 * Which scenario to serve, and which user message it was asked in.
 *
 * The newest user message is tried first, then older ones. That is not a
 * convenience: the engine appends request-only control lines ("continue where you
 * stopped", "you have used up your tool budget") that the user never typed, and a
 * matcher that only looks at the newest message would report those as unscripted
 * and fail a turn that was scripted perfectly.
 */
function scenarioFor(messages) {
  const indexed = []
  for (let i = 0; i < messages.length; i++)
    if (messages[i].role === 'user') indexed.push({ i, text: textOf(messages[i]) })
  for (let k = indexed.length - 1; k >= 0; k--) {
    const sc = pickScenario(indexed[k].text)
    if (sc) return { scenario: sc, at: indexed[k].i, text: indexed[k].text }
  }
  const last = indexed.length ? indexed[indexed.length - 1] : { i: -1, text: '' }
  return { scenario: null, at: last.i, text: last.text }
}

/**
 * How far into the scenario this request is. The unit is an **assistant message
 * after the matched user message**, not a tool result: a tool round appends one
 * assistant message, and so does an answer that was cut off at an output limit and
 * picked back up. Counting tool results made the continuation re-serve step 0,
 * which would repeat the same truncated half-answer forever.
 */
function stepsSince(messages, atIndex) {
  if (atIndex < 0) return 0
  return messages.slice(atIndex + 1).filter((m) => m.role === 'assistant').length
}

/**
 * Expand `$path` to the last path-shaped token of the prompt. The harness
 * interpolates its own temp paths into a prompt, and a canned tool call that
 * names a hard-coded path would exercise nothing.
 */
function extractPath(text) {
  const tokens = (text || '')
    .split(/[\s,;"'()]+/)
    .map((t) => t.replace(/[.,;:]+$/, ''))
    .filter((t) => t.length > 1 && (t.includes('/') || /\.[A-Za-z0-9]{1,5}$/.test(t)))
  return tokens.length ? tokens[tokens.length - 1] : ''
}

function pickScenario(text) {
  const lower = (text || '').toLowerCase()
  for (const sc of script.scenarios ?? []) {
    const m = (sc.match ?? '').toLowerCase()
    if (m && lower.includes(m)) return sc
  }
  return null
}

const sleep = (ms) => new Promise((r) => setTimeout(r, Math.max(0, ms * SPEED)))

// ---- SSE emission ----------------------------------------------------------

function chunk(id, created, body) {
  return `data: ${JSON.stringify({ id, object: 'chat.completion.chunk', created, model: MODEL, ...body })}\n\n`
}

function split(text, size) {
  const out = []
  for (let i = 0; i < text.length; i += size) out.push(text.slice(i, i + size))
  return out.length ? out : []
}

async function streamCompletion(res, step, opts) {
  const id = `chatcmpl-mock-${Date.now().toString(36)}`
  const created = Math.floor(Date.now() / 1000)
  res.writeHead(200, {
    'content-type': 'text/event-stream; charset=utf-8',
    'cache-control': 'no-cache',
    connection: 'keep-alive',
    'x-accel-buffering': 'no',
    'x-mock-scenario': opts.scenarioName ?? '',
  })
  res.write(chunk(id, created, { choices: [{ index: 0, delta: { role: 'assistant' }, finish_reason: null }] }))

  const chunkChars = step.chunk_chars ?? defaults.chunk_chars ?? 8
  const delay = step.delay_ms ?? defaults.delay_ms ?? 12
  const argChunk = step.arg_chunk_chars ?? defaults.arg_chunk_chars ?? 24

  // Reasoning first, because that is what a thinking model does. When the caller
  // asked for thinking off (`chat_template_kwargs.enable_thinking: false`) we must
  // not send any: a mock that ignores that flag stops testing the flag.
  if (step.reasoning && !opts.thinkingOff) {
    for (const p of split(step.reasoning, chunkChars)) {
      res.write(chunk(id, created, { choices: [{ index: 0, delta: { reasoning_content: p }, finish_reason: null }] }))
      await sleep(delay)
    }
  }

  if (step.content) {
    const parts = split(step.content, chunkChars)
    // An aborted stream stops without a finish_reason and without [DONE]: that is
    // what a dying server looks like from the client side. Fixed fraction, so the
    // cut lands on the same word every run.
    const cutAt = step.finish === 'abort' ? Math.max(1, Math.floor(parts.length * 0.7)) : -1
    for (let i = 0; i < parts.length; i++) {
      res.write(chunk(id, created, { choices: [{ index: 0, delta: { content: parts[i] }, finish_reason: null }] }))
      await sleep(delay)
      if (cutAt > 0 && i === cutAt) {
        log(`aborting mid-stream for scenario ${opts.scenarioName} after ${i + 1} of ${parts.length} chunks`)
        res.destroy()
        return
      }
    }
  }

  for (const call of step.tool_calls ?? []) {
    res.write(
      chunk(id, created, {
        choices: [
          {
            index: 0,
            delta: {
              tool_calls: [{ index: 0, id: call.id ?? `mock-call-${stats.turns}`, type: 'function', function: { name: call.name, arguments: '' } }],
            },
            finish_reason: null,
          },
        ],
      }),
    )
    // Arguments arrive fragmented - a real model streams them token by token, and
    // the accumulation + repair path is a large part of what this harness exists
    // to keep working.
    const fragN = step.arg_fragments ?? Math.max(1, Math.ceil((call.arguments ?? '').length / argChunk))
    const size = Math.max(1, Math.ceil((call.arguments ?? '').length / fragN))
    for (const p of split(call.arguments ?? '', size)) {
      res.write(
        chunk(id, created, {
          choices: [
            { index: 0, delta: { tool_calls: [{ index: 0, function: { arguments: p } }] }, finish_reason: null },
          ],
        }),
      )
      await sleep(delay)
    }
  }

  // 'length' has to survive: it is the difference between "the model finished" and
  // "an output cap bit it", and the engine behaves differently for each.
  const finish =
    step.finish === 'tool_calls' ? 'tool_calls' : step.finish === 'length' ? 'length' : 'stop'
  res.write(chunk(id, created, { choices: [{ index: 0, delta: {}, finish_reason: finish }] }))

  // llama.cpp puts timings on the stream; the telemetry bar reads them. The
  // numbers are invented, and that is fine as long as nobody reads "cache hit
  // 97%" from this server as a measurement of a real one.
  const promptN = opts.promptTokens
  const predictedN = ((step.content ?? '').length + (step.reasoning ?? '').length) / 4
  res.write(
    chunk(id, created, {
      choices: [],
      usage: {
        prompt_tokens: promptN,
        completion_tokens: Math.ceil(predictedN),
        total_tokens: promptN + Math.ceil(predictedN),
        prompt_tokens_details: { cached_tokens: opts.cachedTokens },
      },
      timings: {
        cache_n: opts.cachedTokens,
        prompt_n: promptN,
        prompt_ms: promptN * 1.1,
        prompt_per_second: 900,
        predicted_n: Math.ceil(predictedN),
        predicted_ms: predictedN * 22,
        predicted_per_second: 45,
        draft_n: 0,
        draft_n_accepted: 0,
      },
    }),
  )
  res.write('data: [DONE]\n\n')
  res.end()
}

// ---- server ----------------------------------------------------------------

const keepAlive = new Map() // ip -> last activity, for the "who talked to me" audit

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, 'http://localhost')
  keepAlive.set(req.socket.remoteAddress ?? '?', Date.now())

  if (req.method === 'GET' && url.pathname === '/health') {
    res.writeHead(200, { 'content-type': 'text/plain' })
    return res.end('ok')
  }
  if (req.method === 'GET' && url.pathname === '/mock/state') {
    res.writeHead(200, { 'content-type': 'application/json' })
    return res.end(JSON.stringify({ model: MODEL, speed: SPEED, ...stats }))
  }
  if (req.method === 'POST' && url.pathname === '/mock/reset') {
    stats.requests = 0
    stats.turns = 0
    stats.hits = {}
    stats.misses = []
    stats.thinking_off = 0
    stats.thinking_on = 0
    stats.tools_requested = 0
    stats.sampling_seen = []
    stats.last = []
    res.writeHead(200, { 'content-type': 'application/json' })
    return res.end(JSON.stringify({ ok: true }))
  }
  if (req.method === 'GET' && url.pathname === '/props') {
    res.writeHead(200, { 'content-type': 'application/json' })
    return res.end(JSON.stringify(script.props ?? {}))
  }
  if (req.method === 'GET' && (url.pathname === '/v1/models' || url.pathname === '/models')) {
    res.writeHead(200, {
      'content-type': 'application/json',
      'cache-control': 'no-store',
      connection: 'keep-alive',
    })
    return res.end(JSON.stringify({ object: 'list', data: script.models ?? [{ id: MODEL }] }))
  }

  if (req.method !== 'POST' || (url.pathname !== '/v1/chat/completions' && url.pathname !== '/chat/completions')) {
    res.writeHead(404, { 'content-type': 'application/json' })
    return res.end(JSON.stringify({ error: { message: `mock-llm: no route ${req.method} ${url.pathname}` } }))
  }

  let raw = ''
  for await (const part of req) raw += part
  stats.requests += 1
  if (RECORD) fs.appendFileSync(RECORD, raw + '\n')

  let body
  try {
    body = JSON.parse(raw)
  } catch (e) {
    res.writeHead(400, { 'content-type': 'application/json' })
    return res.end(JSON.stringify({ error: { message: `mock-llm: request body is not JSON: ${e.message}` } }))
  }

  const messages = body.messages ?? []
  const found = scenarioFor(messages)
  const scenario = found.scenario
  // `$path` comes from what the user actually just asked for, not from the older
  // message that happened to match a scenario.
  const prompt = lastUserText(messages) || found.text
  const kwargs = body.chat_template_kwargs ?? {}
  const thinkingOff = kwargs.enable_thinking === false
  if (thinkingOff) stats.thinking_off += 1
  else stats.thinking_on += 1
  if (Array.isArray(body.tools) && body.tools.length) stats.tools_requested += 1
  // Sampling: which keys the engine actually put on the wire. Cwoffee's contract
  // is "absent means the server decides", so a test has to see absence, not
  // infer it from an answer that happened to look calm.
  const SAMPLE_KEYS = ['temperature', 'top_p', 'top_k', 'min_p', 'seed']
  const seen = SAMPLE_KEYS.filter((k) => body[k] !== undefined && body[k] !== null)
  stats.sampling_seen.unshift({
    at: Date.now(),
    keys: seen,
    values: Object.fromEntries(seen.map((k) => [k, body[k]])),
  })
  stats.sampling_seen = stats.sampling_seen.slice(0, 5)
  stats.last.unshift({ prompt: prompt.slice(0, 120), at: Date.now() })
  stats.last = stats.last.slice(0, 12)

  if (!scenario) {
    stats.misses.push(prompt.slice(0, 160))
    const why =
      `mock-llm has no scenario for this prompt: "${prompt.slice(0, 160)}". ` +
      `Add it to ${path.relative(process.cwd(), SCENARIOS)} rather than letting the harness run on filler. ` +
      `(If this prompt is deliberate, pass --allow-unknown.)`
    log(`MISS: ${prompt.slice(0, 100)}`)
    res.writeHead(400, { 'content-type': 'application/json' })
    return res.end(JSON.stringify({ error: { message: why } }))
  }

  const stepNo = stepsSince(messages, found.at)
  const step = (scenario.steps ?? [])[stepNo]
  if (!step) {
    const why =
      `mock-llm scenario "${scenario.name}" has no step ${stepNo}: the model was asked to continue a ` +
      `tool loop it has no scripted answer for. Either the loop is spinning or the scenario needs another step.`
    log(`EXHAUSTED: ${scenario.name} step ${stepNo}`)
    res.writeHead(400, { 'content-type': 'application/json' })
    return res.end(JSON.stringify({ error: { message: why } }))
  }

  // `require`: strings the engine's own tool output must contain, checked against
  // this request. Without it, a scripted answer about a file proves only that the
  // scenario said so; with it, the tool result in the prompt is the evidence.
  const needs = Array.isArray(step.require) ? step.require : []
  if (needs.length) {
    const toolText = messages
      .slice(found.at + 1)
      .filter((m) => m.role === 'tool')
      .map((m) => String(m.content ?? ''))
      .join('\n---\n')
    const missing = needs.filter((needle) => !toolText.includes(needle))
    if (missing.length) {
      const why =
        `mock-llm scenario "${scenario.name}" step ${stepNo} required ${JSON.stringify(missing)} in the tool ` +
        `results and did not find them. The tools ran; what the engine actually said was: ` +
        `${JSON.stringify(toolText.slice(0, 700))}`
      log(`REQUIRE FAILED: ${scenario.name} step ${stepNo}: ${missing.join(', ')}`)
      res.writeHead(400, { 'content-type': 'application/json' })
      return res.end(JSON.stringify({ error: { message: why } }))
    }
  }

  stats.turns += 1
  stats.hits[scenario.name] = (stats.hits[scenario.name] ?? 0) + 1
  log(`#${stats.turns} ${scenario.name} step ${stepNo}${thinkingOff ? ' (thinking off)' : ''}`)

  // `$path` -> the path the harness named in its own prompt.
  const wantPath = /\$path/.test(JSON.stringify(step))
  const realPath = wantPath ? extractPath(prompt) : ''
  if (wantPath && !realPath) log(`scenario ${scenario.name} asked for $path but the prompt names none`)
  const stepCopy = JSON.parse(
    JSON.stringify(step).replaceAll('$path', wantPath ? JSON.stringify(realPath).slice(1, -1) : '$path'),
  )

  // Pretend the prefix cache hit for everything already in the request: the point
  // is that the telemetry bar has something plausible to show and that the cache
  // number moves, not that we measure llama.cpp.
  const promptTokens = Math.max(1, Math.ceil(messages.reduce((n, m) => n + String(m.content ?? '').length + 24, 0) / 4))
  const cachedTokens = stepNo > 0 ? Math.floor(promptTokens * 0.92) : 0

  try {
    await streamCompletion(res, stepCopy, {
      scenarioName: scenario.name,
      thinkingOff,
      promptTokens,
      cachedTokens,
    })
  } catch (e) {
    log(`stream failed: ${e.message}`)
    try {
      res.destroy()
    } catch {}
  }
})

// llama.cpp serves keep-alive plain HTTP too; a mock that closes every connection
// hides an entire class of gateway bug.
server.keepAliveTimeout = 8000
server.headersTimeout = 10000

server.on('error', (e) => {
  // Port in use is the common case, and "EADDRINUSE" buried in a stack trace is
  // how a run ends up talking to a mock that loaded its scenarios an hour ago.
  log(`cannot listen on ${HOST}:${PORT}: ${e.code ?? e.message}`)
  process.exit(2)
})

server.listen(PORT, HOST, () => {
  log(`listening on http://${HOST}:${PORT} (model ${MODEL}, speed x${SPEED})`)
  log(`scenarios: ${(script.scenarios ?? []).length} from ${SCENARIOS}`)
  log(`the real endpoint is not involved: nothing here costs a prefill`)
})

for (const sig of ['SIGINT', 'SIGTERM'])
  process.on(sig, () => {
    log('shutting down')
    server.close(() => process.exit(0))
    process.exit(0)
  })
