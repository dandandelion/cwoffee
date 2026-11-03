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
 * A step may also carry `require_steer`: strings that must appear in a user message positioned
 * **after the last tool result** of the request that plays it. That is how a mid-turn press is
 * graded — the claim is about the model's next request, and only the server that received it can
 * say the message was in it, in that place. `require_steer_absent` is the other half: strings that
 * must not appear in any user message of the request at all, which is what a withdrawal promises.
 *
 * Routes:
 *   GET  /health              -> ok
 *   GET  /v1/models           -> the mock model list (meta.n_ctx, input_modalities)
 *   GET  /props               -> llama.cpp-style props (model_alias, chat_template)
 *   POST /v1/chat/completions -> SSE, scripted
 *   GET  /mock/state          -> instrumentation: hits, misses, thinking flags seen
 *   POST /mock/reset          -> clear instrumentation
 */

import crypto from 'node:crypto'
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

const scenariosRaw = fs.readFileSync(SCENARIOS, 'utf8')
const script = JSON.parse(scenariosRaw)
// A fingerprint of the scenario file this process read at start-up. A gate that starts
// a mock and a gate that reuses one cannot otherwise tell whether the server answering
// on the port saw the scenarios.json that is on disk right now — and testing against a
// stale copy silently hides every scenario added since.
const FP = crypto.createHash('sha256').update(scenariosRaw).digest('hex').slice(0, 12)
const SCENARIO_COUNT = Object.keys(script.scenarios ?? script).length
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
  /// Every user message text this server was ever asked about, newest last. The mid-turn
  /// checks read it to prove a withdrawn message never reached a model: the witness is the
  /// request the app actually sent, not the card the UI painted.
  user_seen: [],
  /// Needles `require_steer` found in a user message after the last tool result, and the ones it
  /// did not. `steer_misses` is a 400 the harness failed on; `steer_hits` is the landing.
  steer_hits: [],
  steer_misses: [],
  /// Needles `require_steer_absent` found anyway: a message the person took back that went out.
  steer_leaks: [],
}

const log = (...a) => console.error('[mock-llm]', ...a)

/**
 * The last user message's text. OpenAI multimodal style puts parts in an array;
 * llama.cpp also accepts a plain string.
 */
/** A user message's text, in either shape: a plain string, or parts with an image among them. */
function userText(m) {
  const c = m?.content
  if (typeof c === 'string') return c
  if (Array.isArray(c)) return c.map((p) => (typeof p === 'string' ? p : String(p?.text ?? ''))).join('\n')
  return ''
}

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

/**
 * Images the request actually carried, in OpenAI's shape (`image_url` with a data:
 * URL) or pi's (`{ type: "image", mimeType, data }`). A scenario can ask for this with
 * `{images}` in its content: the endpoint then reports what it received, which is the
 * only proof that an attachment travelled from the client to the model rather than
 * being dropped one hop short.
 */
function imageSummary(messages) {
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i]
    if (!Array.isArray(m?.content)) continue
    const imgs = m.content.filter((p) => p && (p.type === 'image' || p.type === 'image_url' || p.image_url))
    if (!imgs.length) continue
    const mimes = imgs.map((p) => {
      if (p.mimeType) return p.mimeType
      const u = typeof p.image_url === 'string' ? p.image_url : (p.image_url?.url ?? '')
      const mm = /^data:([^;,]+);/.exec(u)
      return mm ? mm[1] : 'unknown'
    })
    return `${imgs.length} image(s): ${mimes.join(', ')}`
  }
  return '0 image(s): none'
}

/** Fill `{images}` in a scripted step with what this request carried. */
function renderStep(step, messages) {
  if (!step) return step
  const out = { ...step }
  if (typeof out.content === 'string' && out.content.includes('{images}'))
    out.content = out.content.replaceAll('{images}', imageSummary(messages))
  return out
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
 *
 * Markup is not a path. The env block ends with `</mcp>` and `<env>`, and a token
 * containing a slash looks path-shaped to the filter above: on a pi-core run that
 * put `</mcp>` into `write_file`'s argument, so the fixture wrote a file nobody
 * read, the `require` needles failed, and a dozen checks reported tool behaviour
 * that had nothing to do with the change under test. A tag is a tag.
 */
function extractPath(text) {
  // The blocks the engine appends are not the person's prompt: `<env>` names the workspace
  // folder and `<mcp>` names servers, and both arrive *after* the words the harness typed. Taking
  // the last token of the whole message therefore picked the env block instead of the file the
  // check named, which is how a scripted turn "wrote" a path nobody asked for.
  const body = (text || '')
    .replace(/<env>[\s\S]*?<\/env>/g, ' ')
    .replace(/<mcp>[\s\S]*?<\/mcp>/g, ' ')
    .replace(/<tools>[\s\S]*?<\/tools>/g, ' ')
  const tokens = body
    .split(/[\s,;"'()]+/)
    .map((t) => t.replace(/[.,;:]+$/, ''))
    .filter((t) => !t.includes('<') && !t.includes('>'))
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

  // `ci` is the slot of this call within the assistant message - the field the
  // OpenAI wire format calls tool_calls[].index. It used to be 0 for every call,
  // so a step that batched six write_file calls arrived as one call holding six
  // concatenated argument strings. Real models batch; the harness has to be able
  // to say so.
  let ci = -1
  for (const call of step.tool_calls ?? []) {
    ci++
    res.write(
      chunk(id, created, {
        choices: [
          {
            index: 0,
            delta: {
              tool_calls: [{ index: ci, id: call.id ?? `mock-call-${stats.turns}-${ci}`, type: 'function', function: { name: call.name, arguments: '' } }],
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
            { index: 0, delta: { tool_calls: [{ index: ci, function: { arguments: p } }] }, finish_reason: null },
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

// How many times the selftest page (or its script) has been fetched. The browser check reads
// this after the turn: it is the only proof in the suite that the browser actually opened a
// socket to something, as opposed to the tool returning plausible text.
let pageHits = 0

// The two strings the selftest page is built from, published on `/selftest-hits` so a check can
// compare what a browser saw with what this server actually sent. `<title>` and `<h1>` differ on
// purpose: a check that matches the wrong one fails a feature that works. (The browser panel titles
// its page from `document.title`; the transcript quotes the heading, because that is what the page's
// own text contains.)
const SELFTEST_TITLE = 'cwoffee selftest'
const SELFTEST_H1 = 'Cwoffee browse selftest'

// Where the page's clickable target sits, in **page** pixels of the viewport the kept browser
// uses. Published on `/selftest-hits` because the point of the panel's picture-click is that the
// check does not have to trust the panel's arithmetic: it asks this server where the box is, turns
// that into a point in the picture the way the panel does, presses there, and then reads back a
// count only the page could have raised. A panel that scaled the picture wrongly misses the box
// and the count stays put.
const AIM = { x: 120, y: 200, w: 180, h: 56 }
// The page's own text field, also by its centre in page pixels: the panel's keyboard row is only
// worth having if a person can aim at a field first, and a check has to know where that field is
// without reading this file's CSS.
const INP = { x: 120, y: 200 + 56 / 2 + 10 + 12, w: 180, h: 24 }
let pageClicks = 0

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, 'http://localhost')
  keepAlive.set(req.socket.remoteAddress ?? '?', Date.now())

  if (req.method === 'GET' && url.pathname === '/health') {
    res.writeHead(200, { 'content-type': 'text/plain' })
    return res.end('ok')
  }
  if (req.method === 'GET' && url.pathname === '/mock/state') {
    res.writeHead(200, { 'content-type': 'application/json' })
    return res.end(
      JSON.stringify({ model: MODEL, speed: SPEED, scenarios: SCENARIO_COUNT, fingerprint: FP, ...stats }),
    )
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
    stats.user_seen = []
    stats.steer_hits = []
    stats.steer_misses = []
    stats.steer_leaks = []
    res.writeHead(200, { 'content-type': 'application/json' })
    return res.end(JSON.stringify({ ok: true }))
  }
  // A page for the browser selftest, served by the same process the test is already
  // talking to. It exists so the check can grade something the page did rather than
  // something the tool claimed: the page asks for a script, and the counter below reports
  // whether a real browser really asked. A tool that only ever returns a string can be
  // believed or disbelieved; a server that saw the request ends the argument.
  if (req.method === 'GET' && url.pathname === '/selftest-page.html') {
    pageHits++
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' })
    // `#aim` and `#inp` sit at a fixed, published place because a check that points at the page
    // through the panel's picture has to aim at something whose position it can know independently
    // of this file: the centre of the box is served on /selftest-hits, and a click that lands there
    // bumps `pageClicks` — which only the page itself can report, so the panel cannot fake it.
    return res.end(
      `<!doctype html><html><head><meta charset="utf-8"><title>${SELFTEST_TITLE}</title></head>` +
        `<body style="margin:0;font:14px system-ui"><h1 id="h">${SELFTEST_H1}</h1>` +
        `<p id="p">before</p>` +
        `<button id="b" onclick="document.getElementById('p').textContent='after'">More</button>` +
        `<div id="aim" style="position:fixed;left:${AIM.x - AIM.w / 2}px;top:${AIM.y - AIM.h / 2}px;width:${AIM.w}px;height:${AIM.h}px;` +
        `box-sizing:border-box;border:2px solid #0a7f5c;border-radius:8px;display:flex;align-items:center;justify-content:center;color:#0a7f5c` +
        `" onclick="document.getElementById('p').textContent='aimed';fetch('/selftest-click').catch(()=>{})">aim here</div>` +
        `<input id="inp" style="position:fixed;left:${INP.x - INP.w / 2}px;top:${INP.y - INP.h / 2}px;width:${INP.w}px;height:${INP.h}px" placeholder="type here">` +
        `<div style="height:1800px;padding:8px">tall page: the panel's wheel has somewhere to go</div>` +
        `<script src="/selftest-page.js"></script>` +
        `<script>window.cwoffeeProbe = 2 + 3;</script>` +
        `</body></html>`,
    )
  }
  if (req.method === 'GET' && url.pathname === '/selftest-click') {
    // Bumped by the page when its aim box is clicked, from inside the browser that was clicked in.
    pageClicks++
    res.writeHead(200, { 'content-type': 'application/json', 'access-control-allow-origin': '*' })
    return res.end(JSON.stringify({ ok: true, clicks: pageClicks }))
  }
  if (req.method === 'GET' && url.pathname === '/selftest-page.js') {
    pageHits++
    res.writeHead(200, { 'content-type': 'text/javascript; charset=utf-8', 'cache-control': 'no-store' })
    // The page's own code throws here on purpose. An uncaught error on the page is the single
    // most useful thing a browser tool can report, and the check below grades that the tool
    // reported *this one*, with this message, which no paraphrase of a fake result produces.
    return res.end(`console.log("selftest page loaded");\nundefined.x();\n`)
  }
  if (req.method === 'GET' && url.pathname === '/selftest-hits') {
    // Readable from the app's own webview: the check inside the page asks the server how many
    // times the browser knocked, which is the one fact the tool's own text cannot manufacture.
    res.writeHead(200, { 'content-type': 'application/json', 'access-control-allow-origin': '*' })
    return res.end(
      JSON.stringify({
        ok: true,
        page_hits: pageHits,
        page_title: SELFTEST_TITLE,
        page_h1: SELFTEST_H1,
        page_clicks: pageClicks,
        aim: AIM,
        inp: INP,
      }),
    )
  }
  if (req.method === 'GET' && url.pathname === '/selftest-hits/reset') {
    pageHits = 0
    res.writeHead(200, { 'content-type': 'application/json' })
    return res.end(JSON.stringify({ ok: true, page_hits: pageHits }))
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
  // What the app put in front of the model, as a user row. Capped, because a long conversation
  // re-sends its whole history on every round-trip and the list would otherwise grow with the
  // prompt rather than with the presses.
  for (const m of messages.filter((x) => x.role === 'user')) {
    const t = userText(m)
    if (t && !stats.user_seen.includes(t)) stats.user_seen.push(t)
  }
  stats.user_seen = stats.user_seen.slice(-80)

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
  // Which step this request plays, and what the step counter saw. The count is assistant
  // messages after the matched user message, so a core that appends one extra assistant
  // message per round shifts every later step of the script while still looking healthy:
  // the run finishes, the tools ran, and the last answer is simply not the scripted one.
  // Without this line that is undebuggable from outside the process.
  const asst = messages.filter((m) => m.role === 'assistant').length
  const tools = messages.filter((m) => m.role === 'tool').length
  log(`SERVE ${scenario.name} step ${stepNo}/${(scenario.steps ?? []).length - 1} (messages ${messages.length}, assistant ${asst}, tool ${tools})`)
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
    log(`REQUIRE ${scenario.name} step ${stepNo}: ${needs.length} needle(s) against ${toolText.length} chars of tool output`)
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

  // `require_steer`: strings that must appear in a **user message after the last tool result** of
  // this request. That is the mid-turn claim, and only the endpoint can make it: a steer is real
  // when the next request carries it in that position — after the tool results, before the answer.
  // A UI that painted a receipt for a message the model never received fails here, on the bytes it
  // actually sent.
  const steerNeeds = Array.isArray(step.require_steer) ? step.require_steer : []
  if (steerNeeds.length) {
    const tail = messages.slice(found.at + 1)
    const lastTool = tail.reduce((m, x, i) => (x.role === 'tool' ? i : m), -1)
    const after = lastTool >= 0 ? tail.slice(lastTool + 1).filter((m) => m.role === 'user').map(userText) : []
    const missing = steerNeeds.filter((needle) => !after.some((t) => t.includes(needle)))
    log(
      `STEER ${scenario.name} step ${stepNo}: ${steerNeeds.length} needle(s) against ${after.length} user ` +
        `message(s) after the last tool result`,
    )
    if (missing.length) {
      stats.steer_misses.push(...missing)
      const why =
        `mock-llm scenario "${scenario.name}" step ${stepNo} required ${JSON.stringify(missing)} in a user ` +
        `message after the tool results of THIS request, and did not find one. A mid-turn press must reach ` +
        `the model's next request, after the tool results and before the answer. What the request held there ` +
        `was: ${JSON.stringify(after.slice(0, 3))}`
      log(`STEER FAILED: ${scenario.name} step ${stepNo}: ${missing.join(', ')}`)
      res.writeHead(400, { 'content-type': 'application/json' })
      return res.end(JSON.stringify({ error: { message: why } }))
    }
    stats.steer_hits.push(...steerNeeds)
  }
  // `require_steer_absent`: strings that must not appear in any user message of this request at
  // all. This is the withdrawal half of the claim — a message the person took back must reach
  // neither the model nor the record — and "the card said it was taken back" is not evidence.
  const absentNeeds = Array.isArray(step.require_steer_absent) ? step.require_steer_absent : []
  if (absentNeeds.length) {
    const tail = messages.slice(found.at).map(userText)
    const foundOnes = absentNeeds.filter((needle) => tail.some((t) => t.includes(needle)))
    log(`STEER-ABSENT ${scenario.name} step ${stepNo}: ${absentNeeds.length} needle(s), ${foundOnes.length} hit(s)`)
    if (foundOnes.length) {
      stats.steer_leaks.push(...foundOnes)
      const why =
        `mock-llm scenario "${scenario.name}" step ${stepNo} was told these must not be on the wire at all ` +
        `and found them: ${JSON.stringify(foundOnes)}. A withdrawn mid-turn message reached the model, which ` +
        `is the failure the withdrawal exists to prevent.`
      log(`STEER LEAK: ${scenario.name} step ${stepNo}: ${foundOnes.join(', ')}`)
      res.writeHead(400, { 'content-type': 'application/json' })
      return res.end(JSON.stringify({ error: { message: why } }))
    }
  }

  stats.turns += 1
  stats.hits[scenario.name] = (stats.hits[scenario.name] ?? 0) + 1
  log(`#${stats.turns} ${scenario.name} step ${stepNo}${thinkingOff ? ' (thinking off)' : ''}`)

  // `$base` -> this server's own origin. The browser check needs a URL the *engine* can
  // fetch, and the only honest way to hand the model one is to say it here, where the port is
  // known: a scenario with a port baked into the JSON breaks the moment someone sets
  // MOCK_PORT, and a broken scenario reads like a broken browser.
  const wantBase = /\$base/.test(JSON.stringify(step))
  const base = wantBase ? `http://127.0.0.1:${PORT}` : ''
  if (wantBase && !base) log(`scenario ${scenario.name} asked for $base and got nothing`)
  // `$path` -> the path the harness named in its own prompt.
  const wantPath = /\$path/.test(JSON.stringify(step))
  const realPath = wantPath ? extractPath(prompt) : ''
  if (wantPath && !realPath) log(`scenario ${scenario.name} asked for $path but the prompt names none`)
  // `{images}` is filled from *this request*, which is the only way to prove an
  // attachment travelled all the way to the model instead of being dropped a hop short.
  const stepCopy = renderStep(
    JSON.parse(
      JSON.stringify(step)
        .replaceAll('$path', wantPath ? JSON.stringify(realPath).slice(1, -1) : '$path')
        .replaceAll('$base', base),
    ),
    messages,
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
