// The browser half of cwoffee's `browse` tool.
//
// Why this is a separate one-shot program rather than a browser the app keeps open:
// cwoffee's tool host on the pi core is one process per tool call (see
// docs/MCP-SERVERS.md, which hits the same wall for MCP servers). A browser owned by
// the tool host would be a new browser on every call. A browser owned by the app would
// work, but it would also mean a window sitting on the user's screen between turns with
// nobody deciding what it is looking at. So the unit of work is *one walk*: open a page,
// run a short scripted sequence, report what the page said and what its scripts did,
// close. A multi-step flow is a `steps` array in one call, not a session across calls —
// which keeps the risk decision (a card naming one URL) in one piece.
//
// Why Playwright rather than a hand-rolled CDP client: `docs.rs` hand-rolls OOXML
// because a docx crate dragged in 88 transitive crates for five XML parts. A browser
// automation protocol is the opposite case — it is a protocol whose whole point is
// fidelity to a real browser, and writing a quarter of it would produce a "browser"
// that quietly disagrees with the one the user will actually open the page in.
//
// Request: one JSON object on stdin. Response: one JSON object on stdout. The screenshot
// is written to a path the caller chose, so the bytes never ride a pipe.

import { chromium } from 'playwright-core'
import fs from 'node:fs'

const LIMITS = {
  // The tool caps what reaches the model as well; these caps exist so a page that is a
  // 4 MB wall of text does not make this process hold 4 MB of strings first.
  text: 24_000,
  snapshot: 24_000,
  console: 4_000,
  consoleLines: 60,
  failed: 20,
  steps: 40,
}

const readRequest = () => {
  const raw = fs.readFileSync(0, 'utf8')
  if (!raw.trim()) fail('the browser driver got no request on stdin')
  try {
    return JSON.parse(raw)
  } catch (e) {
    fail(`the browser driver's request is not JSON: ${e instanceof Error ? e.message : e}`)
  }
}

function fail(why, extra = {}) {
  out({ ok: false, why, ...extra })
  process.exit(0)
}

function out(obj) {
  fs.writeSync(1, JSON.stringify(obj))
}

// Where the browser binary is. Playwright's own registry lookup wants the browser
// *it* was released against; cwoffee would rather use what is already on this machine,
// so the caller may name a build and we also look for one ourselves.
function findBrowser() {
  const named = (process.env.CWOFFEE_BROWSER_PATH ?? '').trim()
  if (named) return { path: named, source: 'CWOFFEE_BROWSER_PATH' }
  const root = (process.env.PLAYWRIGHT_BROWSERS_PATH || `${process.env.HOME ?? ''}/.cache/ms-playwright`).trim()
  const wantHeadless = WANT_HEADLESS
  const dirs = (() => {
    try {
      return fs.readdirSync(root).sort().reverse()
    } catch {
      return []
    }
  })()
  // Newest build first: a machine with two builds installed is a machine where the newer
  // one was installed on purpose.
  const pick = (prefix, rel) => {
    for (const d of dirs) {
      if (!d.startsWith(prefix)) continue
      const p = `${root}/${d}/${rel}`
      if (fs.existsSync(p)) return { path: p, source: `playwright build ${d}` }
    }
    return null
  }
  const full = pick('chromium-', 'chrome-linux64/chrome') ?? pick('chromium-', 'chrome-linux/chrome')
  const shell = pick('chromium_headless_shell-', 'chrome-headless-shell-linux64/chrome-headless-shell') ?? pick('chromium_headless_shell-', 'chrome-linux/headless_shell')
  if (wantHeadless) return shell ?? full ?? { path: null, source: 'none' }
  // Headed needs the real browser: a headless shell cannot paint a window at all, and
  // pretending otherwise would show the user nothing while claiming they were shown something.
  return full ?? { path: null, source: wantHeadless ? 'none' : 'no full chromium build (a headless shell cannot open a window)' }
}

const req = readRequest()
const WANT_HEADLESS = req.headless !== false
const overallMs = Math.max(1_000, Math.min(Number(req.timeout_ms ?? 45_000), 300_000))
const stepMs = Math.max(200, Math.min(Number(req.step_timeout_ms ?? 8_000), 60_000))
const deadline = Date.now() + overallMs

const url = String(req.url ?? '').trim()
if (!url) fail('no url to open')
let parsed
try {
  parsed = new URL(url)
} catch {
  fail(`"${url}" is not a URL the browser can open`)
}
// The scheme list is short on purpose. A page is data and somebody else's code; `file://`
// is the local filesystem wearing that costume, and `javascript:`/`data:` navigations run
// in the origin of whatever was open before. The Rust side decides whether a file:// walk
// is inside the workspace and passes `allow_file`; nothing here invents a permission.
if (!['http:', 'https:'].includes(parsed.protocol) && !(req.allow_file === true && parsed.protocol === 'file:')) {
  fail(
    `refusing to open ${parsed.protocol}//… : the browser tool opens http and https, and a file: URL only with the host's permission`,
    { scheme: parsed.protocol },
  )
}

const browser = findBrowser()
if (!browser.path) {
  fail(
    'no browser to open this with. Install one for Playwright (`npx playwright install chromium`), ' +
      'or point CWOFFEE_BROWSER_PATH at a Chrome/Chromium binary.',
    { browser: browser.source, wanted_headless: WANT_HEADLESS },
  )
}

const left = () => Math.max(200, deadline - Date.now())
const consoleLines = []
const pageErrors = []
const failedRequests = []
const downloadTries = []
const stepLog = []

let launched = null
let headlessActual = WANT_HEADLESS
let launchNote = ''
const t0 = Date.now()
for (const mode of WANT_HEADLESS ? [true] : [false, true]) {
  try {
    launched = await chromium.launch({
      executablePath: browser.path,
      headless: mode,
      // A browser started by an agent should not look like the user's browser: no default
      // profile, no extensions, no "restore pages" bubble, and no writes to the real cache.
      args: ['--no-first-run', '--no-default-browser-check', '--disable-sync', '--disable-extensions'],
      timeout: Math.min(launchMs(), left()),
    })
    headlessActual = mode
    break
  } catch (e) {
    const why = String(e && e.message ? e.message : e).split('\n')[0]
    if (mode === false) {
      // Headed failed - usually no display. Falling back to headless is honest only if we
      // say we did it, because "you can see this window" was the promise headed mode made.
      launchNote = `headed launch failed (${why}); fell back to headless`
      continue
    }
    fail(`the browser would not start: ${why}`, { browser: browser.source, path: browser.path })
  }
}
function launchMs() {
  return Math.min(stepMs * 4, left())
}

const context = await launched.newContext({
  viewport: req.viewport ?? { width: 1280, height: 900 },
  // Downloads are somebody else's program asking to write to this machine. The tool's card
  // covers the page, not a file drop, so the driver declines them and reports the attempt.
  acceptDownloads: false,
})
const page = await context.newPage()
page.on('console', (m) => {
  if (consoleLines.length >= LIMITS.consoleLines) return
  const t = m.text()
  consoleLines.push({ type: m.type(), text: t.length > LIMITS.console ? `${t.slice(0, LIMITS.console)}…` : t })
})
page.on('pageerror', (e) => {
  if (pageErrors.length >= LIMITS.consoleLines) return
  pageErrors.push(String(e && e.message ? e.message : e).split('\n')[0])
})
page.on('requestfailed', (r) => {
  if (failedRequests.length >= LIMITS.failed) return
  failedRequests.push({ url: r.url(), method: r.method(), why: r.failure()?.errorText ?? 'failed' })
})
page.on('download', (d) => {
  if (downloadTries.length < LIMITS.failed) downloadTries.push(d.suggestedFilename() || 'a file')
  d.cancel().catch(() => {})
})

const screenshotRequested = ['viewport', 'full', true].includes(req.screenshot)
const nav = { ok: false, why: '', status: null, url_after: '' }
const tNav = Date.now()
try {
  const resp = await page.goto(url, {
    waitUntil: ['load', 'networkidle', 'domcontentloaded'].includes(req.wait_until) ? req.wait_until : 'load',
    timeout: left(),
  })
  nav.ok = true
  nav.status = resp ? resp.status() : null
} catch (e) {
  nav.why = String(e && e.message ? e.message : e).split('\n')[0]
}
nav.url_after = page.url()
const navMs = Date.now() - tNav

// A target is written the way a person would name it: "Sign in", "email", "#save". The
// order below is the order of confidence - a labelled control, then a placeholder, then
// visible text, then a raw selector - and an ambiguous match is reported rather than
// guessed at, because clicking the wrong of two "Save" buttons and reporting success is
// the worst thing a test harness can do.
async function locate(target) {
  const t = String(target ?? '')
  if (!t) return { err: 'no target named' }
  const tries = [
    () => page.getByLabel(t),
    () => page.getByPlaceholder(t),
    () => page.getByRole('button', { name: t }),
    () => page.getByRole('link', { name: t }),
    () => page.getByRole('textbox', { name: t }),
    () => page.getByText(t),
    () => page.locator(t),
  ]
  for (const make of tries) {
    let loc
    try {
      loc = make()
    } catch {
      continue
    }
    const n = await loc.count().catch(() => 0)
    if (n === 1) return { loc }
    if (n > 1) return { err: `${n} elements match ${JSON.stringify(t)}; name it more narrowly (a label, a role, or a selector)` }
  }
  return { err: `nothing on the page matches ${JSON.stringify(t)}` }
}

const wantText = req.text !== false
let text = ''
let textTruncated = false
let snapshot = []
for (let i = 0; i < Math.min(Array.isArray(req.steps) ? req.steps.length : 0, LIMITS.steps); i++) {
  const st = req.steps[i] ?? {}
  const doWhat = String(st.do ?? '')
  const rec = { do: doWhat, target: st.target ?? null, status: 'ok' }
  try {
    if (doWhat === 'click') {
      const l = await locate(st.target)
      if (l.err) rec.status = l.err
      else await l.loc.click({ timeout: stepMs })
    } else if (doWhat === 'fill') {
      const l = await locate(st.target)
      if (l.err) rec.status = l.err
      else {
        const want = String(st.text ?? '')
        // A `<select>` is chosen from. Playwright's `fill` refuses one, and the words the model has
        // are the ones a person reads in the list, so match those; a name that is not there says
        // what the list offers rather than leaving the next guess to the model.
        const tag = await l.loc.evaluate((el) => el.tagName).catch(() => '')
        if (tag === 'SELECT') {
          const picked = await l.loc.evaluate((el, want) => {
            const opts = [...el.options]
            const norm = (s) => (s || '').trim().toLowerCase()
            const hit =
              opts.find((o) => o.value === want) ||
              opts.find((o) => (o.textContent || '').trim() === want) ||
              opts.find((o) => norm(o.textContent) === norm(want))
            if (!hit) return null
            el.value = hit.value
            el.dispatchEvent(new Event('input', { bubbles: true }))
            el.dispatchEvent(new Event('change', { bubbles: true }))
            return { value: hit.value, text: (hit.textContent || '').trim() }
          }, want)
          if (!picked) {
            const names = await l.loc
              .evaluate((el) => [...el.options].map((o) => (o.textContent || '').trim() || o.value).slice(0, 12))
              .catch(() => [])
            rec.status = `${JSON.stringify(want)} is not a choice on this list; it offers ${names.join(', ') || 'nothing'}`
          } else {
            rec.status = `chose ${JSON.stringify(picked.text || picked.value)}`
          }
        } else {
          await l.loc.fill(want, { timeout: stepMs })
        }
      }
    } else if (doWhat === 'press') {
      await page.keyboard.press(String(st.key ?? 'Enter'))
    } else if (doWhat === 'navigate') {
      // Point this browser at another page and keep the tab. The kept browser's whole promise is one
      // page you can walk, and a walk that could only ever leave it made the sequence "click a link,
      // come back, read the footer" impossible to write down.
      const url = String(st.url ?? '')
      if (!/^https?:\/\//i.test(url) && !url.startsWith('file://')) {
        rec.status = `navigate wants an http(s) or file:// url, not ${JSON.stringify(url)}`
      } else {
        try {
          await page.goto(url, { timeout: Math.max(2_000, stepMs), waitUntil: 'domcontentloaded' })
        } catch (e) {
          rec.status = `${url} did not finish loading: ${String(e.message ?? e).split('\n')[0]}`
        }
      }
    } else if (doWhat === 'text' || doWhat === 'snapshot') {
      // Reading the page is a step like any other, and the schema says so. Answering `I do not know
      // the step "text"` here while the kept browser answers the same word with the page's words is
      // the two-dialects problem again: the model is told one verb list and gets a different page
      // depending on which browser the host happened to keep.
      const raw = doWhat === 'text'
        ? await page.evaluate(() => document.body?.innerText ?? '').catch(() => '')
        : await describeWalk().catch(() => [])
      rec.value = String(Array.isArray(raw) ? raw.join('\n') : raw).slice(0, 8_000)
    } else if (doWhat === 'wait') {
      const t0 = Date.now()
      await page.waitForTimeout(Math.min(Number(st.ms ?? 500), Math.max(0, left())))
      // Measured here, so the number in the report came from a clock and not from what was asked for.
      rec.waited = Date.now() - t0
    } else if (doWhat === 'wait_for') {
      // Look, and keep looking for the time this step was given — the same contract the kept browser
      // and the page bridge make. One check would answer a 4-second wait in 3ms and call the page
      // broken for something that had simply not happened yet.
      const want = Math.max(0, Math.min(Number(st.ms ?? stepMs), Math.max(0, left())))
      const t0 = Date.now()
      let lastWhy = `nothing on the page matches ${JSON.stringify(String(st.target ?? ''))}`
      for (;;) {
        const l = await locate(st.target)
        if (!l.err) {
          try {
            await l.loc.waitFor({ state: 'visible', timeout: Math.min(200, Math.max(0, want)) })
            break
          } catch {
            lastWhy = `${JSON.stringify(String(st.target ?? ''))} is on the page but not visible`
          }
        }
        const looked = Date.now() - t0
        if (looked >= want) {
          rec.status = `${lastWhy} — looked for ${looked}ms and it never appeared`
          break
        }
        await page.waitForTimeout(60)
      }
      rec.waited = Date.now() - t0
    } else if (doWhat === 'scroll') {
      await page.mouse.wheel(0, Number(st.px ?? 600))
    } else if (doWhat === 'screenshot') {
      rec.status = screenshotRequested ? 'taken' : 'a screenshot was already requested for this walk'
    } else if (doWhat === 'eval') {
      // Deliberately narrow: an expression, evaluated in the page, returned as text. This is
      // how a model checks its own code ("what does my app's state say now"), and it is why
      // the tool's card names the origin it is running against before any of this happens.
      const v = await page.evaluate(String(st.code ?? 'document.title'))
      rec.value = typeof v === 'string' ? v.slice(0, 2_000) : JSON.stringify(v)?.slice(0, 2_000)
    } else if (doWhat === 'a11y') {
      // The same readable list the kept browser and the page in the window give. One verb list
      // across three browsers is the whole point of it: a model that asked for `a11y` and gets a
      // terse "I do not know the step" from this browser has been told the *page* is the problem.
      const items = await describeWalk().catch(() => [])
      rec.value = (items ?? []).join('\n') || 'nothing visible in this page to describe'
    } else if (doWhat === 'health') {
      // Counted from what this walk's own page made, which is the honest scope: a one-shot walk has
      // exactly one document, so there is nothing older to separate out.
      const ready = await page.evaluate('document.readyState').catch(() => 'unknown')
      const worst = failedRequests.slice(-4).map((f) => `${f.url} (${f.why})`).join(' | ')
      rec.value =
        `${ready} at ${page.url()}: ${consoleLines.length} console message(s), ` +
        `${pageErrors.length} uncaught error(s), ${failedRequests.length} request(s) that did not load` +
        (worst ? `; the last things that went wrong: ${worst}` : '; nothing has gone wrong on this page')
      rec.note = `${consoleLines.length} console, ${pageErrors.length} error(s), ${failedRequests.length} request(s) that did not load`
    } else if (doWhat === 'back' || doWhat === 'forward') {
      // History inside a walk: the walk has its own history now, because a `navigate` step or a
      // clicked link put pages in it. At the end of the list Playwright resolves to null rather
      // than throwing, so Back on a first page is ok at the url it was already at.
      const mv = doWhat === 'back'
        ? page.goBack({ timeout: Math.min(stepMs, left()), waitUntil: 'domcontentloaded' })
        : page.goForward({ timeout: Math.min(stepMs, left()), waitUntil: 'domcontentloaded' })
      await mv.catch(() => null)
      rec.note = `the walk is now at ${page.url()}`
    } else if (doWhat === 'reload') {
      await page.reload({ timeout: Math.min(stepMs, left()), waitUntil: 'domcontentloaded' }).catch(() => null)
      rec.note = `reloaded ${page.url()}`
    } else if (doWhat === 'stop') {
      // A walk has nobody pressing a stop button; Escape is the closest act that means something to
      // the page, and the walk's own deadline is what really stops a page that will not finish.
      await page.keyboard.press('Escape')
      rec.note = 'the page got Escape; this walk also stops by itself at its deadline'
    } else {
      rec.status = `I do not know the step "${doWhat}"`
    }
  } catch (e) {
    rec.status = String(e && e.message ? e.message : e).split('\n')[0]
  }
  stepLog.push(rec)
  if (left() <= 400) {
    stepLog.push({ do: 'stopped', target: null, status: `out of time after ${overallMs}ms; ${req.steps.length - i - 1} step(s) were not run` })
    break
  }
}

let shot = { path: null, bytes: 0, why: '' }
if (screenshotRequested) {
  const where = String(req.screenshot_path ?? '')
  if (!where) shot.why = 'no screenshot path was given'
  else {
    try {
      const buf = await page.screenshot({ fullPage: req.screenshot === 'full' })
      fs.writeFileSync(where, buf)
      shot.path = where
      shot.bytes = buf.length
    } catch (e) {
      shot.why = String(e && e.message ? e.message : e).split('\n')[0]
    }
  }
}

if (wantText) {
  try {
    text = await page.evaluate(() => document.body?.innerText ?? '')
  } catch {
    text = ''
  }
  if (text.length > LIMITS.text) {
    text = text.slice(0, LIMITS.text)
    textTruncated = true
  }
  // A short structural read, so a model can aim its next step at a real control rather
  // than guessing at a selector from prose.
  try {
    snapshot = await page.evaluate(() => {
      const out = []
      const sel = 'a,button,input,select,textarea,h1,h2,h3,[role=button],[role=tab],[role=heading],[aria-label]'
      for (const el of document.querySelectorAll(sel)) {
        if (out.length >= 120) break
        const label = (el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.getAttribute('name') || el.textContent || '').trim().replace(/\s+/g, ' ').slice(0, 60)
        const tag = el.tagName.toLowerCase()
        const role = el.getAttribute('role') || (tag === 'a' ? 'link' : tag === 'button' ? 'button' : tag === 'input' ? `input[${el.getAttribute('type') || 'text'}]` : tag)
        const id = el.id ? `#${el.id}` : ''
        out.push(`${role}${id ? ' ' + id : ''}${label ? ` "${label}"` : ''}`)
      }
      return out
    })
  } catch {
    snapshot = []
  }
}

let title = ''
try {
  title = await page.title()
} catch {
  title = ''
}

let endedAt = ''
try {
  endedAt = page.url()
} catch {
  endedAt = ''
}

// The same control list this program reports at the end of a walk, so a `snapshot` step in the middle
// of one means the same thing.
async function describeWalk() {
  return page.evaluate(() => {
    const out = []
    const sel = 'a,button,input,select,textarea,h1,h2,h3,[role=button],[role=tab],[role=heading],[aria-label]'
    for (const el of document.querySelectorAll(sel)) {
      const tag = el.tagName.toLowerCase()
      const role = el.getAttribute('role') || (tag === 'a' ? 'link' : tag === 'button' ? 'button' : tag === 'input' ? `input[${el.getAttribute('type') || 'text'}]` : tag)
      const id = el.id ? `#${el.id}` : ''
      const label = (el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.textContent || '').trim().slice(0, 60)
      out.push(`${role}${id ? ' ' + id : ''}${label ? ` "${label}"` : ''}`)
    }
    return out.slice(0, 60)
  })
}

await context.close().catch(() => {})
await launched.close().catch(() => {})

out({
  ok: nav.ok,
  ...(nav.ok ? {} : { why: nav.why || 'the page did not load' }),
  url,
  // Where this browser ended up, not where it was pointed. A walk that navigated leaves it somewhere
  // else, and the report's "the page is at" line has to name the page the next call would find.
  url_after: endedAt || nav.url_after,
  status: nav.status,
  title,
  text,
  text_truncated: textTruncated,
  snapshot,
  steps: stepLog,
  console: consoleLines,
  page_errors: pageErrors,
  requests_failed: failedRequests,
  downloads_blocked: downloadTries,
  screenshot: shot,
  headless: headlessActual,
  asked_headless: WANT_HEADLESS,
  ...(launchNote ? { note: launchNote } : {}),
  browser: { source: browser.source, path: browser.path },
  timings: { launch_ms: tNav - t0, nav_ms: navMs, total_ms: Date.now() - t0 },
})
