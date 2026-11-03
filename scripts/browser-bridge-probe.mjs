// The page bridge, graded against a real page instead against a model's patience.
//
// cwoffee's kept browser is a page in the app's own window, driven by `page_bridge.js` through the
// line protocol in `page_view.rs`. Nobody could check that bridge without running the app: the
// engine's tests drive a *fake* driver (the seam), and the live probe drives the *panel* (the UI
// half). Nothing drove the page's own verbs. The result was a session of the model discovering by
// hand that `wait` answered `ok` without waiting, that `wait_for` never looked twice, that `fill`
// on a `<select>` came back with an engine error, and that `a11y`/`health` reported success and
// then said nothing.
//
// So this script is the missing seam: it installs the real bridge in a real browser, talks to it in
// exactly the shape `page_view.rs` does — `__cw.run(job, tag)` and then poll `__cwR[tag]`, with the
// same `a|`/`b|` prefixes and the same per-step budget — and asserts each claim a model is told
// about a page. Every `bad()` line below is a claim that was being made and was not true.
//
// What it grades:
//   1. **A wait waits.** The page's own clock is the witness, not the driver's word, and the number
//      in the answer is measured rather than echoed back.
//   2. **`wait_for` keeps looking** for the time it was given, and says how long it looked.
//   3. **The page's event loop turns** while a step is in flight — timers, intervals and animation
//      frames included — because every async assertion in a page depends on that.
//   4. **A name finds the element a person meant**, including plain visible text, and an ambiguous
//      name names the candidates instead of guessing.
//   5. **A field is written the way that element accepts it** — text input, textarea, and a `<select>`
//      chosen from its options rather than typed into.
//   6. **Every verb answers in text the tool can print.** A step that answers with an object and is
//      rendered as nothing is a step that lies about working.
//   7. **A promise the page hands back is waited for**, and a rejection is a sentence.
//   8. **History moves are answered**, and a step whose answer is destroyed by the navigation it
//      caused is reported as that rather than as a timeout.
//   9. **Tab moves focus**, because a keyboard test is a focus test.
//
//   node scripts/browser-bridge-probe.mjs
//
// Needs a Chromium build for Playwright (`npx playwright install chromium`).

import { chromium } from '../browser/node_modules/playwright-core/index.mjs'
import http from 'node:http'
import fs from 'node:fs'
import path from 'node:path'

const BRIDGE = path.resolve('crates/tauri-app/src/page_bridge.js')
const PORT = Number(process.argv[2] ?? 8831)

if (!fs.existsSync(BRIDGE)) {
  console.log(`FAIL — no bridge at ${BRIDGE}`)
  process.exit(1)
}
const bridgeSrc = fs.readFileSync(BRIDGE, 'utf8')

const ok = []
const fails = []
const note = (m) => ok.push(m)
const bad = (m) => fails.push(m)
const check = (cond, m) => (cond ? note(m) : bad(m))

/* ------------------------------- the pages under test ------------------------------- */
// The fixture the model wrote by hand while it was testing the sidebar browser, kept here so the
// same claims are graded the next time somebody changes the bridge. Every part of it exists to make
// one verb testable: the console/error/resource riggers, the async work that finishes 600ms after
// it was asked for, the form with a `<select>` and a checkbox, the tall stretch for scrolling, and
// the two-page history for Back.
const FIXTURE = `<!doctype html><html><head><meta charset="utf-8"><title>bridge probe</title>
<style>body{font:14px system-ui;margin:24px}#tall{height:1600px;background:linear-gradient(#eef,#fff)}</style>
</head><body>
  <h1>Bridge Probe</h1>
  <p id="status">ready</p>
  <label for="name">Name</label><input id="name" placeholder="Your name">
  <button id="go">Greet</button><p id="greeting"></p>
  <button id="boom">Trigger uncaught error</button>
  <button id="delayed">Async work (600ms)</button><p id="async-result"></p>
  <img id="missing" src="./definitely-not-here.png" alt="missing image">
  <link rel="stylesheet" href="./also-not-here.css">
  <script src="./not-here.js"></script>
  <form id="f">
    <select id="pick"><option value="a">Alpha</option><option value="b">Beta</option></select>
    <input type="checkbox" id="chk"><textarea id="note" placeholder="Note"></textarea>
    <button type="submit" id="send">Send</button>
  </form>
  <p id="out">no submit yet</p>
  <div id="tall">scroll target below</div>
  <p id="bottom-marker">BOTTOM-MARKER</p>
  <a id="navlink" href="/second.html">Go to the second page</a>
  <a id="dl" href="/probe.html" download>Save this page</a>
  <script>
    console.log("probe: log at load"); console.info("probe: info at load"); console.warn("probe: warn at load");
    document.getElementById("go").addEventListener("click", () => {
      document.getElementById("greeting").textContent = "Hello, " + (document.getElementById("name").value || "stranger") + "!";
      console.log("probe: greet clicked");
    });
    document.getElementById("boom").addEventListener("click", () => { window.nope.deep(); });
    document.getElementById("delayed").addEventListener("click", () => {
      document.getElementById("status").textContent = "working";
      setTimeout(() => { document.getElementById("async-result").textContent = "async done"; console.log("probe: async finished"); }, 600);
    });
    document.getElementById("f").addEventListener("submit", (e) => {
      e.preventDefault();
      document.getElementById("out").textContent =
        "submitted pick=" + document.getElementById("pick").value + " chk=" + document.getElementById("chk").checked +
        " note=" + document.getElementById("note").value;
    });
  </script>
</body></html>`

const SECOND = `<!doctype html><html><head><meta charset="utf-8"><title>second page</title></head>
<body><h1>Second page</h1><p id="here">you are on the second page</p>
<a id="backlink" href="/probe.html">Back to the probe</a></body></html>`

const server = http.createServer((req, res) => {
  const url = (req.url || '/').split('?')[0]
  if (url === '/probe.html') {
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' })
    return res.end(FIXTURE)
  }
  if (url === '/second.html') {
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' })
    return res.end(SECOND)
  }
  res.writeHead(404, { 'content-type': 'text/plain' })
  res.end('not here')
})
await new Promise((r) => server.listen(PORT, '127.0.0.1', r))
const base = `http://127.0.0.1:${PORT}`

/* --------------------------------- the driver's side -------------------------------- */
// The same two evals `page_view.rs` issues: hand the page a job, then poll for its answer. The
// `a|` / `b|` prefixes and the "the document changed under the step" branch are the protocol both
// halves implement, so grading them here grades the contract rather than one side of it.
const browser = await chromium.launch({ headless: true })
const context = await browser.newContext({ viewport: { width: 900, height: 700 } })
await context.addInitScript(bridgeSrc)
let page = await context.newPage()
page.on('dialog', (d) => d.dismiss().catch(() => {}))

let seq = 0
async function step(page, req, budgetMs = 20_000) {
  const tag = `j${++seq}`
  const job = { timeout_ms: budgetMs, ...req }
  const boot0 = await boot(page)
  let issued = false
  try {
    issued = await page.evaluate(
      ({ job, tag }) => (window.__cw ? (window.__cw.run(job, tag), true) : false),
      { job, tag }
    )
  } catch {
    // The page went away between the boot read and the hand-over. Same conclusion as above.
    return { lost: true, wall: 0 }
  }
  if (!issued) return { err: 'no bridge in the page' }
  const pollScript =
    `window.__cwR && '${tag}' in window.__cwR ? (function(){var r = window.__cwR['${tag}']; window.__cwDone('${tag}'); ` +
    `return 'a|' + (typeof r === 'string' ? r : JSON.stringify(r))})() : (window.__cw ? 'b|' + window.__cw.boot : '')`
  const started = Date.now()
  for (;;) {
    // A poll that gets no answer at all is not a dead page: the usual reason is that the document it
    // was aimed at is being replaced underneath it, which is what `page_view.rs` treats as "ask the
    // new page who it is before deciding".
    let raw = null
    try {
      raw = await page.evaluate(pollScript)
    } catch {
      raw = null
    }
    if (typeof raw === 'string' && raw.startsWith('a|')) {
      return { answer: JSON.parse(raw.slice(2)), wall: Date.now() - started }
    }
    let nowBoot = raw && typeof raw === 'string' && raw.startsWith('b|') ? raw.slice(2) : await boot(page)
    if (boot0 && nowBoot && nowBoot !== boot0) {
      return { lost: true, wall: Date.now() - started }
    }
    if (Date.now() - started > budgetMs) return { timed_out: true, wall: Date.now() - started }
    await new Promise((r) => setTimeout(r, 25))
  }
}
const boot = (page) => page.evaluate("window.__cw ? window.__cw.boot : ''").catch(() => '')
const ev = (page, code) => page.evaluate(`(() => { ${code} })()`).catch((e) => String(e))

await page.goto(`${base}/probe.html`, { waitUntil: 'load' })
if (!(await boot(page))) {
  console.log('FAIL — the bridge never came up in the page')
  process.exit(1)
}
note('the bridge installs in a real page before its own code runs')

/* 1. a wait actually waits, and says how long it waited */
let r = await step(page, { do: 'eval', code: 'window.__c = Date.now(); "clock set"' })
check(r.answer?.ok && r.answer.value === 'clock set', 'eval answers its completion value')
r = await step(page, { do: 'wait', ms: 700 })
const waited = r.answer?.waited ?? -1
check(r.answer?.ok && waited >= 650, `wait(700) took ${waited}ms in the page, not the caller's word for it`)
r = await step(page, { do: 'eval', code: '"page clock moved " + (Date.now() - window.__c) + "ms"' })
check(Number((r.answer?.value || '').match(/\d+/)?.[0]) >= 650, `the page's own clock moved with it: ${r.answer?.value}`)

/* 2. the page's event loop turns while a step is in flight */
await step(page, { do: 'eval', code: 'window.__iv = 0; window.__t0 = 0; window.__ivId = setInterval(() => window.__iv++, 100); window.__raf = 0; (function f(){ window.__raf++; if (window.__raf < 200) requestAnimationFrame(f) })(); "armed"' })
await step(page, { do: 'wait', ms: 700 })
r = await step(page, { do: 'eval', code: '"interval ticks=" + window.__iv + " raf frames=" + window.__raf + " timeoutFired=" + (window.__t0 = 1)' })
const ticks = Number(r.answer?.value?.match(/ticks=(\d+)/)?.[1] ?? 0)
const frames = Number(r.answer?.value?.match(/frames=(\d+)/)?.[1] ?? 0)
check(ticks >= 4, `a 100ms interval ticked ${ticks} times across a 700ms wait — the page is alive (${JSON.stringify(r.answer?.value ?? r.answer?.why)})`)
check(frames >= 2, `requestAnimationFrame advanced ${frames} frames on its own — a page that animates, animates (${JSON.stringify(r.answer?.value ?? r.answer?.why)})`)

/* 3. wait_for keeps looking until the page catches up */
await step(page, { do: 'click', target: 'Async work (600ms)' })
r = await step(page, { do: 'wait_for', target: '#async-result', ms: 3000 })
check(
  r.answer?.ok && (r.answer?.waited ?? 0) >= 400,
  `wait_for "#async-result" found it after ${r.answer?.waited}ms of work that takes 600ms — it polls, it does not guess`
)
r = await step(page, { do: 'wait_for', target: '#never-appears', ms: 900 })
check(
  !r.answer?.ok && (r.answer?.waited ?? 0) >= 850 && /looked for/.test(r.answer?.why || ''),
  `wait_for on a thing that never appears spends the time it was given (${r.answer?.waited}ms) and says so`
)

/* 4. a person's words name the element */
for (const [target, what] of [['Bridge Probe', 'the page heading'], ['scroll target below', 'a plain block in the middle of the page']]) {
  r = await step(page, { do: 'wait_for', target, ms: 300 })
  check(r.answer?.ok, `wait_for "${target}" matches ${what}: ${r.answer?.hit ?? r.answer?.why}`)
}
r = await step(page, { do: 'click', target: 'Greet' })
check(r.answer?.ok, 'click by the words on a button')
r = await step(page, { do: 'click', target: 'e' })
check(!r.answer?.ok && /elements match/.test(r.answer?.why || ''), `an ambiguous name refuses and names candidates: ${(r.answer?.why || '').slice(0, 90)}`)
r = await step(page, { do: 'wait_for', target: 'no-such-thing-at-all', ms: 200 })
check(!r.answer?.ok, 'a name that matches nothing is not a silent success')

/* 5. fields are written the way each element accepts one */
r = await step(page, { do: 'fill', target: 'Your name', text: 'Cwoffee' })
check(r.answer?.ok && r.answer.focus === '#name', `fill focuses what it filled (focus=${r.answer?.focus})`)
r = await step(page, { do: 'fill', target: '#pick', text: 'Beta' })
check(r.answer?.ok && r.answer?.chose === 'Beta', `fill on a <select> chooses the option a person reads: ${JSON.stringify(r.answer?.chose ?? r.answer?.why)}`)
r = await step(page, { do: 'eval', code: 'document.getElementById("pick").value' })
check(r.answer?.value === 'b', 'and the page sees the value that goes with it')
r = await step(page, { do: 'fill', target: '#pick', text: 'Gamma' })
check(!r.answer?.ok && /offers 2 option/.test(r.answer?.why || ''), `a choice that is not on the list says what the list is: ${(r.answer?.why || '').slice(0, 110)}`)
r = await step(page, { do: 'fill', target: 'Note', text: 'typed note' })
check(r.answer?.ok && (await ev(page, 'return document.getElementById("note").value')) === 'typed note', 'fill writes a textarea')
r = await step(page, { do: 'click', target: 'Send' })
r = await step(page, { do: 'eval', code: 'document.getElementById("out").textContent' })
check(/submitted pick=b .*chk=false note=typed note/.test(r.answer?.value || ''), `the form saw the whole walk: ${r.answer?.value}`)

/* 6. keyboard steps move focus, because a keyboard test is a focus test */
await step(page, { do: 'fill', target: '#name', text: 'typed' })
r = await step(page, { do: 'press', key: 'Tab' })
check(r.answer?.ok && r.answer.focus && r.answer.focus !== '#name', `Tab moved focus to ${r.answer?.focus}`)
r = await step(page, { do: 'press', key: ' ' })
check(r.answer?.ok, `Space answered on the focused control (${r.answer?.focus})`)

/* 7. every verb answers in something the tool can print */
r = await step(page, { do: 'a11y', max: 6 })
check(
  r.answer?.ok && typeof r.answer.value === 'string' && r.answer.value.includes('#go') && r.answer.value.split('\n').length <= 6,
  `a11y returns text (${(r.answer?.value || '').split('\n').length} lines of ${r.answer?.total} matches)`
)
r = await step(page, { do: 'health' })
check(
  r.answer?.ok && typeof r.answer.value === 'string' && /console message\(s\)/.test(r.answer.value),
  `health returns a sentence: ${(r.answer?.value || '').slice(0, 110)}`
)
r = await step(page, { do: 'snapshot' })
check(r.answer?.ok && /combobox #pick/.test(r.answer.value || ''), 'snapshot names the roles a person would hear')
r = await step(page, { do: 'text' })
check(r.answer?.ok && r.answer.value.includes('BOTTOM-MARKER'), 'text reads the page')
r = await step(page, { do: 'diagnostics' })
check(r.answer?.ok && typeof r.answer.value === 'string' && r.answer.value.includes('probe:'), 'diagnostics reads as text, not as an array')

/* 8. a promise the page hands back is waited for */
r = await step(page, { do: 'eval', code: 'new Promise(res => setTimeout(() => res("settled after " + Math.round(performance.now()) + "ms"), 300))' })
check(r.answer?.ok && /^settled after/.test(r.answer?.value || '') && (r.answer?.awaited ?? 0) >= 250, `eval awaits a promise (${r.answer?.awaited}ms): ${r.answer?.value}`)
r = await step(page, { do: 'eval', code: 'Promise.reject(new Error("the page said no"))' })
check(!r.answer?.ok && /the page said no/.test(r.answer?.why || ''), `a rejected promise is a sentence: ${(r.answer?.why || '').slice(0, 80)}`)
r = await step(page, { do: 'eval', code: 'new Promise(() => {})' , timeout_ms: 1200 })
check(!r.answer?.ok && /had not settled within/.test(r.answer?.why || ''), 'a promise that never settles is reported as that, not as a hang')

/* 9. a step that cannot answer inside its own budget says so */
r = await step(page, { do: 'wait', ms: 4000 }, 1000)
check(r.answer?.ok && r.answer.waited < 1000 && /only had/.test(r.answer?.note || ''), `a wait the step has no room for says so: ${JSON.stringify(r.answer?.note || r.answer?.why)}`)

/* 10. the page's own complaints arrive, with the kind that makes them mean something */
await step(page, { do: 'click', target: 'Trigger uncaught error' })
await new Promise((r) => setTimeout(r, 200))
const records = await page.evaluate('JSON.stringify(window.__cw.di.map(e => [e.kind, e.level, e.text, e.where || ""]))')
const recs = JSON.parse(records)
check(recs.some((r) => r[0] === 'resource' && /img could not load/.test(r[2])), 'a resource that did not load is its own kind of record')
check(recs.some((r) => r[0] === 'error' && /TypeError|nope/.test(r[2]) && /:\d+:\d+/.test(r[3])), `an uncaught error says what and where: ${JSON.stringify(recs.find((r) => r[0] === 'error') || 'none')}`)
check(recs.some((r) => r[0] === 'console' && r[1] === 'warning'), 'a console.warn keeps its level')

/* 10b. a click that starts a download is not a click that leaves the page */
const urlBefore = page.url()
r = await step(page, { do: 'click', target: 'Save this page' })
check(r.answer?.ok, 'the click itself is answered')
check(!r.answer?.may_navigate, `a download link does not promise a new document (${JSON.stringify(r.answer?.may_navigate)})`)
check(/download link/.test(r.answer?.note || ''), `and the answer says the file is not the page's to report: ${JSON.stringify(r.answer?.note || '')}`)
await new Promise((res) => setTimeout(res, 250))
check(page.url() === urlBefore, `and the page really is still there: ${page.url()} vs ${urlBefore}`)
const offers = JSON.parse(await page.evaluate('JSON.stringify(window.__cw.di.filter(e => e.kind === "download").map(e => [e.text, e.url, e.save]))'))
check(
  offers.some((o) => /offered/.test(o[0] || '') && /\/probe\.html$/.test(o[1] || '') && o[2] === true),
  `the offer is recorded with the address to fetch and the flag that asks the app to fetch it: ${JSON.stringify(offers)}`,
)

/* 11. history: answered, and answered *now* */
await step(page, { do: 'click', target: 'Go to the second page' })
await page.waitForLoadState('load')
check(page.url().endsWith('/second.html'), 'a click on a link moves the page')
const t0 = Date.now()
r = await step(page, { do: 'back' })
const backMs = Date.now() - t0
// A Back that lands answers in one of two ways: the page got its answer off before the move, or the
// move destroyed the document the answer was sitting in. `page_view.rs` turns the second into a
// success for exactly these verbs, because a new document is what Back was for — so either way the
// step must be *answered*, quickly, and never waited out.
check((r.answer?.ok || r.lost) && backMs < 3000, `Back is answered in ${backMs}ms (${r.lost ? 'the move outlived the answer, which the driver reports as the move' : 'answered by the page'})`)
await page.waitForTimeout(400)
check(page.url().endsWith('/probe.html'), `and the page really moved back to ${page.url().split('/').pop()}`)
const t1 = Date.now()
r = await step(page, { do: 'forward' })
check(r.answer?.ok || r.lost, `Forward is answered too (${Date.now() - t1}ms)`)
await page.waitForTimeout(400)
check(page.url().endsWith('/second.html'), 'and forward moved it')

/* 12. a step whose answer dies with the document it was asked in is reported as that */
await page.goto(`${base}/probe.html`, { waitUntil: 'load' })
await step(page, { do: 'eval', code: 'setTimeout(() => { location.href = "/second.html" }, 250); "leaving shortly"' })
const b0 = await boot(page)
r = await step(page, { do: 'wait', ms: 4000 }, 6000)
check(
  r.lost || r.answer?.lost_to_navigation === true,
  r.lost
    ? 'the driver stops polling a document that no longer exists (the bridge reports its new boot id)'
    : `the bridge itself said the page moved: ${JSON.stringify(r.answer?.why)}`
)
check((r.wall ?? 0) < 5000, `and it took ${r.wall}ms, not the step's whole budget`)
check((await boot(page)) !== b0, 'the page that is up now is a different document, as the driver claimed')

/* --------------------------------- verdict --------------------------------- */
await browser.close()
server.close()
for (const m of ok) console.log(`ok — ${m}`)
if (fails.length) {
  for (const f of fails) console.log(`FAIL — ${f}`)
  console.log(`browser-bridge-probe: ${fails.length} failure(s) / ${ok.length} ok`)
  process.exitCode = 1
} else {
  console.log(`browser-bridge-probe: ok (${ok.length} claims)`)
}
