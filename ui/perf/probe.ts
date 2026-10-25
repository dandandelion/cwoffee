/**
 * Transcript scroll probe — see ui/perf/probe.html for why this exists.
 *
 * What it does: mounts the real transcript renderer with the real stylesheet, paints a
 * synthetic conversation of a chosen size through `loadHistory` (the same call a session
 * switch makes), then measures what scrolling that conversation costs.
 *
 * It measures the page, not the model: nothing here talks to an endpoint, the store or
 * the host. So a run costs no prefill and touches no session, and the numbers are about
 * DOM cost and nothing else — which is exactly the thing the report was about.
 *
 * URL knobs:
 *   ?turns=40        turns in the synthetic conversation
 *   ?reason=8000     characters of thinking per turn
 *   ?tools=3         tool calls per turn
 *   ?out=5000        characters of tool output per call
 *   ?ans=1200        characters of answer prose per turn
 *   ?mode=live       stream the conversation the way a real turn does (transcript.apply)
 *                    instead of painting it from the store (loadHistory)
 *   ?cpframe=240     characters fed per animation frame in live mode (a fast local model)
 *   ?steps=0         scroll steps (0 = enough to cross the whole transcript at 120px)
 *   ?nodefer=1       do not defer/unrender off-screen content (the negative control)
 *   ?collapse=off    leave each turn's per-word reveal spans in the DOM after the
 *                    turn settles (the negative control for the scroll fix)
 */
import '../src/app.css'
import { Transcript } from '../src/lib/transcript'
import type { StoredMessage } from '../src/lib/events'

declare global {
  interface Window {
    // The host bridge WebKitGTK installs for registered script message handlers.
    webkit?: { messageHandlers?: Record<string, { postMessage(m: string): void }> }
    __cwNoDefer?: boolean
  }
}

function say(msg: string) {
  console.log(msg)
  try {
    window.webkit?.messageHandlers?.probe?.postMessage(msg)
  } catch {
    /* no host bridge (a plain browser): the console and document.title are the report */
  }
  document.title = msg.slice(0, 200)
}

// Anything that stops the probe before it reports has to say so, or the rig's only
// signal is a timeout that looks like a slow page.
window.addEventListener('error', (e) => say(`PROBE THREW: ${e.message} @ ${e.filename}:${e.lineno}`))
window.addEventListener('unhandledrejection', (e) => say(`PROBE REJECTED: ${String((e as PromiseRejectionEvent).reason)}`))
say('probe: module executing')

const q = new URLSearchParams(location.search)
const n = (k: string, d: number) => {
  const raw = q.get(k)
  if (raw === null) return d
  const v = Number(raw)
  return Number.isFinite(v) && v >= 0 ? v : d
}
const TURNS = n('turns', 40)
const REASON = n('reason', 8000)
const TOOLS = n('tools', 3)
const OUT = n('out', 5000)
const ANS = n('ans', 1200)
const STEPS = n('steps', 0)
const MODE = q.get('mode') === 'live' ? 'live' : 'history'
const CPFRAME = n('cpframe', 240)
const COLLAPSE = q.get('collapse') !== 'off'
if (q.get('nodefer') === '1') {
  // The negative control: the same page with the optimisation switched off. A fix
  // that is not measured against its own off switch is a guess that got faster
  // for some other reason.
  ;(window as unknown as { __cwNoDefer: boolean }).__cwNoDefer = true
}

// ---- synthetic conversation ------------------------------------------------

function words(n: number, seed: number): string {
  // Prose, not lorem: markdown parsing of real-ish sentences exercises list/heading
  // detection, and a single giant word measures a different thing.
  const pool =
    'the model reports what it found and decides the next call from that result so that the ' +
    'answer at the end of the turn carries the fact rather than the search that led to it'
  const w = pool.split(' ')
  const out: string[] = []
  for (let i = 0; i < n; i++) out.push(w[(i + seed) % w.length])
  return out.join(' ')
}

function paragraphs(chars: number, seed: number, blocks = 6): string {
  const per = Math.max(40, Math.floor(chars / blocks))
  const parts: string[] = []
  for (let b = 0; b < blocks; b++) {
    const wc = Math.ceil(per / 6)
    parts.push(words(wc, seed + b * 7))
  }
  return parts.join('\n\n')
}

function rows(): StoredMessage[] {
  const out: StoredMessage[] = []
  let id = 1
  for (let t = 0; t < TURNS; t++) {
    out.push({
      id: id++,
      session_id: 1,
      role: 'user',
      content: `${words(40, t)} (turn ${t + 1})`,
      reasoning: '',
      tool_call_id: null,
      tool_calls_json: '',
      reasoning_signature: '',
      reasoning_ms: 4200,
      files: [],
      created_at: 0,
      attachments: [],
    })
    const calls = []
    for (let c = 0; c < TOOLS; c++) {
      const name = ['exec', 'read_file', 'web_search', 'write_file'][c % 4]
      calls.push({
        id: `c${t}-${c}`,
        name,
        arguments: JSON.stringify(
          name === 'exec' ? { command: `find . -name '*.rs' | head -40 # turn ${t}` } : { path: `src/module${t}/file${c}.rs` },
        ),
      })
    }
    out.push({
      id: id++,
      session_id: 1,
      role: 'assistant',
      content: paragraphs(ANS, t, 3),
      reasoning: paragraphs(REASON, t + 100),
      tool_call_id: null,
      tool_calls_json: JSON.stringify(calls),
      reasoning_signature: '',
      reasoning_ms: 4200,
      files: [],
      created_at: 0,
      attachments: [],
    })
    for (let c = 0; c < TOOLS; c++) {
      out.push({
        id: id++,
        session_id: 1,
        role: 'tool',
        content: paragraphs(OUT, t * 3 + c, 4),
        reasoning: '',
        tool_call_id: `c${t}-${c}`,
        tool_calls_json: '',
        reasoning_signature: '',
        reasoning_ms: 0,
        files: [],
        created_at: 0,
        attachments: [],
      })
    }
  }
  return out
}

// ---- the app's own shell shape --------------------------------------------

const wrap = document.createElement('div')
wrap.className = 'scrollerwrap'
wrap.style.cssText = 'height:100vh;min-height:0;display:flex;position:relative'
const shell = document.createElement('div')
shell.className = 'scroller'
shell.style.cssText = 'flex:1 1 auto;min-height:0;overflow-y:auto'
const mid = document.createElement('div')
mid.style.cssText = 'margin:0 auto;width:100%;max-width:780px'
const root = document.createElement('div')
root.className = 'transcript'
mid.append(root)
shell.append(mid)
wrap.append(shell)
document.body.style.margin = '0'
document.body.append(wrap)

const t = new Transcript(root)
t.sessionId = 1
if (!COLLAPSE) {
  // The negative control: identical page, identical stream, only the collapse of a
  // settled turn's reveal spans is switched off. If the node count does not change
  // between this and the default run, the collapse is not what fixed the scrolling.
  t.collapseSettled = false
}

// ---- the live path: stream it the way a real turn does -------------------

/**
 * Feed the conversation through `transcript.apply`, paced one animation frame at a
 * time, which is what a real turn looks like from inside the page.
 *
 * This matters because the two render paths are not the same shape. History is
 * `textContent` and `renderMarkdown` once per row. A live turn reveals word by word
 * (`<span class="tin">` each, with an animation), grows a clamped thinking body while
 * it thinks, and folds its run when the answer lands. A transcript built by history
 * cannot reproduce what a long *live* session accumulates, and the report was about a
 * session that had been running.
 */
async function stream(turns: number) {
  type Item = { run: () => void; chars: number }
  const q: Item[] = []
  const chunk = (s: string, size: number, emit: (piece: string) => void) => {
    for (let i = 0; i < s.length; i += size) {
      const piece = s.slice(i, i + size)
      q.push({ run: () => emit(piece), chars: piece.length })
    }
  }
  const op = (run: () => void) => q.push({ run, chars: 0 })
  for (let turn = 0; turn < turns; turn++) {
    op(() => t.apply({ kind: 'turnStart', d: { turn_id: turn + 1, model: 'probe-model', session_id: 1 } }))
    op(() => t.addUser(`${words(40, turn)} (turn ${turn + 1})`))
    // Two thinking blocks with tool calls between them: the shape a tool loop has.
    for (let phase = 0; phase < 2; phase++) {
      chunk(paragraphs(Math.round(REASON / 2), turn * 10 + phase, 5), 26, (piece) =>
        t.apply({ kind: 'reasoning', d: piece }),
      )
      for (let c = 0; c < Math.ceil(TOOLS / 2); c++) {
        const id = `c${turn}-${phase}-${c}`
        const name = ['exec', 'read_file', 'web_search', 'write_file'][(turn + phase + c) % 4]
        op(() => t.apply({ kind: 'toolCallBegin', d: { id, name } }))
        op(() =>
          t.apply({
            kind: 'toolCallReady',
            d: { id, args: name === 'exec' ? { command: `rg -n 'defer' crates | head -40 # turn ${turn}` } : { path: `src/module${turn}/file${c}.rs` } },
          }),
        )
        op(() => t.apply({ kind: 'toolExecStart', d: { id } }))
        chunk(paragraphs(Math.round(OUT / 2), turn * 7 + phase * 3 + c, 4), 400, (piece) =>
          t.apply({ kind: 'toolExecOutput', d: { id, stream: 'stdout', chunk: piece } }),
        )
        op(() =>
          t.apply({
            kind: 'toolExecEnd',
            d: { id, ok: true, exit_code: 0, changes: name === 'write_file' ? [{ path: `src/module${turn}/file${c}.rs`, added: 42, removed: 7, created: true }] : [] },
          }),
        )
      }
    }
    chunk(paragraphs(ANS, turn, 3), 30, (piece) => t.apply({ kind: 'text', d: piece }))
    op(() => t.apply({ kind: 'turnEnd', d: { turn_id: turn + 1, finish: 'stop' } }))
  }
  let i = 0
  // Where the cup sits on screen, frame by frame, measured against the viewport rather than the
  // document. The report is about the picture *moving while lines arrive*: a number in document
  // coordinates would simply follow the scroll and say nothing. A picture carried along by growth
  // travels one way; a picture that jumps back and forth reverses direction, and the reversal count
  // is what separates the two.
  const cupMove: number[] = []
  let cupPrev: number | null = null
  let cupDir = 0
  let cupReversals = 0
  let cupTravel = 0
  let cupFrames = 0
  const sampleCup = () => {
    const img = document.querySelector('.transcript-cup') as HTMLElement | null
    // A picture the transcript has put away (`.bare` for an empty page, `.cup-out` for a reader who
    // is mid-page) has no box at all; sampling it would report a 1,300px jump out of nothing.
    if (!img || img.classList.contains('bare') || img.classList.contains('cup-out')) {
      cupPrev = null
      return
    }
    const y = img.getBoundingClientRect().top - shell.getBoundingClientRect().top
    if (cupPrev !== null) {
      const d = y - cupPrev
      cupFrames++
      if (Math.abs(d) > 0.5) {
        cupMove.push(Math.abs(d))
        cupTravel += Math.abs(d)
        const dir = Math.sign(d)
        if (cupDir !== 0 && dir !== cupDir) cupReversals++
        cupDir = dir
      }
    }
    cupPrev = y
  }
  while (i < q.length) {
    let chars = 0
    const t0 = performance.now()
    while (i < q.length && chars < CPFRAME) {
      const item = q[i++]
      item.run()
      // Structural ops carry no characters. Counting each as a nominal 60 keeps the
      // per-frame budget honest without letting one frame run a thousand of them.
      chars += item.chars > 0 ? item.chars : 60
    }
    await frame()
    sampleCup()
    if (performance.now() - t0 > 400) say(`probe: live frame took >400ms at op ${i}/${q.length}`)
    if (i % 400 < 3) say(`probe: live ${i}/${q.length} ops`)
  }
  say(
    `probe cup motion: ${cupFrames} frames sampled, per-frame step p50=${pct(cupMove, 0.5).toFixed(1)}px ` +
      `p95=${pct(cupMove, 0.95).toFixed(1)}px max=${Math.max(...cupMove, 0).toFixed(1)}px, ` +
      `total travel ${Math.round(cupTravel)}px, direction reversals ${cupReversals}` +
      (shell ? ` ; scroller scrollTop=${Math.round(shell.scrollTop)}/${Math.round(shell.scrollHeight - shell.clientHeight)}` : ''),
  )
  // The band, stated in absolute numbers rather than as motion: how far the picture sits from its
  // containing block's bottom edge, and how that block relates to the pane. Written out because the
  // same claim measured in the app returned a seat of 524px, and "the cup is in the wrong place" does
  // not say whether the picture, its parent or the pane is the element that moved.
  {
    const img = document.querySelector('.transcript-cup') as HTMLElement | null
    shell.scrollTop = shell.scrollHeight
    await frame()
    if (!img) say('probe cup band: no cup on the page')
    else {
      const cs = getComputedStyle(img)
      const b = img.getBoundingClientRect()
      const host = img.parentElement!
      const hb = host.getBoundingClientRect()
      const col = document.querySelector('.scroller > div > .transcript') as HTMLElement | null
      say(
        `probe cup band: parent .${host.className} rect=${Math.round(hb.left)},${Math.round(hb.top)} ${Math.round(hb.width)}x${Math.round(hb.height)}; ` +
          `cup rect=${Math.round(b.left)},${Math.round(b.top)} ${Math.round(b.width)}x${Math.round(b.height)} ` +
          `computed pos=${cs.position} disp=${cs.display} top=${cs.top} bottom=${cs.bottom} left=${cs.left} --cup-left="${cs.getPropertyValue('--cup-left').trim()}"; ` +
          `seat=${Math.round(hb.bottom - b.bottom)}px, left-vs-column=${col ? Math.round(b.left - col.getBoundingClientRect().left) : 'n/a'}px; ` +
          `scroller padB=${getComputedStyle(shell).paddingBottom} ${Math.round(shell.scrollTop)}/${shell.scrollHeight}x${shell.clientHeight}; ` +
          `view=${window.innerWidth}x${window.innerHeight} dpr=${window.devicePixelRatio}`,
      )
    }
  }
  for (let k = 0; k < 5; k++) await frame()
}


// ---- measurement -----------------------------------------------------------

const frame = () =>
  new Promise<void>((r) => {
    // A webview that is not being composited never calls rAF. Waiting on it forever
    // turns "the page did not run" into a timeout with no diagnosis, so the probe gives
    // up on the frame and says so.
    let done = false
    const go = () => {
      if (done) return
      done = true
      r()
    }
    requestAnimationFrame(go)
    setTimeout(go, 2000)
  })
const pct = (a: number[], p: number) => (a.length ? a.slice().sort((x, y) => x - y)[Math.min(a.length - 1, Math.floor(a.length * p))] : 0)
const stats = (label: string, a: number[]) =>
  `${label} n=${a.length} p50=${pct(a, 0.5).toFixed(1)}ms p95=${pct(a, 0.95).toFixed(1)}ms worst=${Math.max(...a, 0).toFixed(1)}ms`

function domShape() {
  const all = document.querySelectorAll('*').length
  const text = document.body.innerText.length
  const reason = [...document.querySelectorAll('.blk-reasoning .blk-body')]
  const tool = [...document.querySelectorAll('.tool-out, .tool-args')]
  const charsIn = (els: HTMLElement[]) => els.reduce((s, e) => s + (e.textContent ?? '').length, 0)
  return {
    nodes: all,
    tinSpans: document.querySelectorAll('.tin').length,
    visibleText: text,
    reasoningBodies: reason.length,
    reasoningChars: charsIn(reason as HTMLElement[]),
    toolBodies: tool.length,
    toolChars: charsIn(tool as HTMLElement[]),
    scrollHeight: shell.scrollHeight,
  }
}

async function scrollSweep(steps: number) {
  // What the reader actually does: wheel the transcript. A wheel event is not
  // synthesisable at the compositor, so the probe drives scrollTop per frame —
  // which is what a wheel does once the compositor hands the scroll to the page —
  // and times both the frame it costs and the layout that follows it.
  const gaps: number[] = []
  const lays: number[] = []
  const h = shell.scrollHeight - shell.clientHeight
  const step = h / Math.max(1, steps)
  shell.scrollTop = h
  await frame()
  await frame()
  for (let i = steps; i >= 0; i--) {
    const t0 = performance.now()
    shell.scrollTop = i * step
    // Force the layout the scroll would trigger. Without this the measurement is a
    // rAF gap, which hides a synchronous style/layout cost behind the next frame.
    void shell.getBoundingClientRect()
    void root.scrollHeight
    const t1 = performance.now()
    lays.push(t1 - t0)
    await new Promise<void>((r) =>
      requestAnimationFrame((now) => {
        gaps.push(now - t1)
        r()
      }),
    )
  }
  return { steps, gaps, lays }
}

async function expandCost() {
  // Opening a run reveals its entries. If the entries' contents are held in the DOM,
  // this is the moment their cost is paid; if they are deferred, it is the moment the
  // deferral has to be paid back — and it has to stay under a frame's worth of work
  // for a click to feel instant.
  const runs = [...document.querySelectorAll('.blk-run')] as HTMLElement[]
  if (!runs.length) return null
  const run = runs[Math.floor(runs.length / 2)]
  const head = run.querySelector('.blk-head') as HTMLElement
  const t0 = performance.now()
  head.click()
  await frame()
  const t1 = performance.now()
  const revealed = (run.querySelector('.run-body')?.textContent ?? '').length
  head.click()
  await frame()
  return { openMs: t1 - t0, revealedChars: revealed, runs: runs.length }
}

async function main() {
  say('probe: start')
  const t0 = performance.now()
  if (MODE === 'live') {
    say(`probe: streaming ${TURNS} live turn(s) at ${CPFRAME} chars/frame`)
    await stream(TURNS)
  } else {
    const built = rows()
    say(`probe: ${built.length} synthetic rows built`)
    t.loadHistory(built)
    await frame()
  }
  say('probe: conversation painted')
  const paintMs = performance.now() - t0
  const shape = domShape()
  const steps = STEPS || Math.min(400, Math.max(80, Math.round(shape.scrollHeight / 120)))
  const sweep = await scrollSweep(steps)
  const exp = await expandCost()
  const after = domShape()
  const report = {
    engine: navigator.userAgent,
    mode: MODE,
    params: { TURNS, REASON, TOOLS, OUT, ANS, CPFRAME, defer: q.get('nodefer') !== '1', collapse: COLLAPSE },
    paintMs: +paintMs.toFixed(1),
    before: shape,
    after,
    scroll: stats('scroll-layout', sweep.lays),
    frames: stats('frame-gap', sweep.gaps),
    worstGap: sweep.gaps.length ? Math.max(...sweep.gaps) : 0,
    expand: exp,
  }
  say(JSON.stringify(report))
  // Keep the page alive so the numbers above can be inspected in the window itself.
  document.body.dataset.done = '1'
}

void main().catch((e) => say(`PROBE THREW: ${e instanceof Error ? e.stack : String(e)}`))
