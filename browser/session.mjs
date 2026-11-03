// A browser the app *keeps*, driven one command per line.
//
// `driver.mjs` is one-shot on purpose: open, walk, report, close, so the approval decision
// stays in one piece. That is the wrong shape for two things this app now does: a person
// watching a page from a sidebar, and an agent that wants to look at a page, change
// something, and look again. Both need the *same* browser to still be there: the same
// cookies, the same logged-in SPA, the same scrolled-to row. So this program owns one
// browser for as long as somebody cares about it, and answers commands on stdin.
//
// Why the app owns it and not the tool process: on the pi core the tool host is one
// process per call (`docs/MCP-SERVERS.md` hits the identical wall for MCP servers), so a
// browser held there would be a new browser every call. The app holds it, and a tool call
// reaches it over a socket. One browser, two hands on it — and the sidebar is a window
// onto *that* browser, never a second view that would disagree about what page is open.
//
// Why Playwright and not a hand-rolled CDP client: see driver.mjs. A browser automation
// protocol is a protocol whose whole point is fidelity to a real browser.
//
// Protocol, one JSON object per line in each direction:
//   in:  {id, cmd: "open"|"act"|"state"|"frame"|"watch"|"unwatch"|"viewport"|"close", ...}
//   out: {id, ok, ...}                                   — the answer to one command
//   out: {ev: "ready"|"nav"|"console"|"pageerror"|"requestfailed"|"download"|"closed"|"frame"}
//
// `watch` turns on the *live* stream: Chromium's own screencast over CDP, which sends a frame
// whenever the page repaints — the model scrolls, an animation runs, a video plays, a hover menu
// opens. `frame` is a still. A panel built only out of stills is a slideshow of a browser, and
// what a person asked for was the browser; the screencast is the real pixels of the real page out
// of the real browser process, which is the most honest form of "a window onto that browser"
// there is. It is also the only one that survives a page nobody commanded: a CSS animation
// repaints with no act in front of it, and a still stream shows a page that never moves.
//
// The browser is *not* the person's browser: no profile, no extensions, no restored
// session, downloads refused. It holds nothing from a previous session, and says so.

import { chromium } from 'playwright-core'
import readline from 'node:readline'
import fs from 'node:fs'

const LIMITS = {
  console: 2_000,
  consoleLines: 60,
  failed: 20,
  snapshot: 120,
  value: 2_000,
}

// The backstop. The app's hub has its own idle clock and *reports* the death; this one
// exists so a hub that hangs or dies cannot leave a browser on somebody's desktop. It is
// deliberately longer than the hub's, so the sentence a person reads is the hub's.
const DRIVER_IDLE_MS = Math.max(60_000, Number(process.env.CWOFFEE_BROWSER_DRIVER_IDLE_MS ?? 8 * 60_000))
let VIEWPORT = clampViewport(process.env.CWOFFEE_BROWSER_VIEWPORT)

// How often a frame may be pushed at a watcher. The screencast fires on every repaint, which on a
// page with a canvas or a video is every vsync; the panel is drawn by a webview that cannot use
// more than ~12 frames a second, and each frame is tens of KB of JPEG through a pipe. The gate is
// a floor on smoothness, not on correctness: a dropped frame is followed by the next repaint.
const VIEW_MIN_MS = Math.max(40, Number(process.env.CWOFFEE_BROWSER_VIEW_MIN_MS ?? 85))
const VIEW_QUALITY = Math.min(Math.max(Number(process.env.CWOFFEE_BROWSER_VIEW_QUALITY ?? 55), 20), 90)

/** How wide a picture the watcher is offered. Chromium's screencast sizes itself to the *page*, and a
 *  page is 1280-ish wide while the panel that shows it is often a 400px column — which is a hundred
 *  kilobytes of JPEG per picture travelling to a box that will draw a third of it. So the panel says
 *  how wide its box is, and the stream is asked for that width: the page stays the page (clicks are
 *  still page points, and `state` still reports the page's own viewport), only the picture is sized
 *  to where it is shown. 0 means "no hint, give me the page". */
let castWidth = 0

/** The picture size for the current page and hint: never wider than the page, never narrower than a
 *  thumbnail, and the height follows the page's aspect so there is no letterbox inside the picture. */
function castSize() {
  const hint = castWidth > 0 ? Math.min(castWidth, VIEWPORT.width) : VIEWPORT.width
  const w = Math.max(240, Math.min(hint, VIEWPORT.width))
  if (w >= VIEWPORT.width) return { width: VIEWPORT.width, height: VIEWPORT.height }
  return { width: w, height: Math.max(120, Math.round((VIEWPORT.height * w) / VIEWPORT.width)) }
}

function clampViewport(raw) {
  const [w, h] = String(raw ?? '').split(/[x,]/).map((n) => Number(n))
  if (!w || !h) return { width: 1280, height: 800 }
  return { width: Math.min(Math.max(w, 320), 1920), height: Math.min(Math.max(h, 240), 1440) }
}

const send = (obj) => fs.writeSync(1, JSON.stringify(obj) + '\n')

let browser = null
let context = null
let page = null
let dead = ''
let idleTimer = null
let framesTaken = 0
// The live view: a CDP session attached to the page, and whether it is streaming. One stream per
// page, because a second screencast of the same page is a second copy of the same pixels.
let cdp = null
let casting = false
let castNote = ''
let lastPushAt = 0

const consoleTail = []
const pageErrors = []
const failedReqs = []
// Which document each record belongs to. Bumped on every main-frame navigation; `health` counts only
// the records made since the page now on screen arrived.
let docGen = 0
const downloadsBlocked = []

function noteDead(why) {
  if (!dead) dead = why
  clearIdle()
  send({ ev: 'closed', why: dead })
}

function armIdle() {
  clearTimeout(idleTimer)
  idleTimer = setTimeout(() => {
    noteDead(`no command for ${Math.round(DRIVER_IDLE_MS / 1000)}s`)
    shutdown(0)
  }, DRIVER_IDLE_MS)
  // This timer must not be the reason the process stays alive.
  idleTimer.unref?.()
}

function clearIdle() {
  clearTimeout(idleTimer)
  idleTimer = null
}

async function shutdown(code = 0) {
  try {
    await context?.close()
  } catch {}
  try {
    await browser?.close()
  } catch {}
  context = null
  page = null
  browser = null
  process.exit(code)
}

// Where the browser binary is — the same lookup driver.mjs uses, so one installed build
// serves both the one-shot walks and the kept browser.
function findBrowser() {
  const named = (process.env.CWOFFEE_BROWSER_PATH ?? '').trim()
  if (named) return { path: named, source: 'CWOFFEE_BROWSER_PATH' }
  const root = (process.env.PLAYWRIGHT_BROWSERS_PATH || `${process.env.HOME ?? ''}/.cache/ms-playwright`).trim()
  const dirs = (() => {
    try {
      return fs.readdirSync(root).sort().reverse()
    } catch {
      return []
    }
  })()
  const pick = (prefix, rel) => {
    for (const d of dirs) {
      if (!d.startsWith(prefix)) continue
      const p = `${root}/${d}/${rel}`
      if (fs.existsSync(p)) return { path: p, source: `playwright build ${d}` }
    }
    return null
  }
  const headlessShell = process.env.CWOFFEE_BROWSER_HEADLESS_ONLY === '1'
  const full = pick('chromium-', 'chrome-linux64/chrome') ?? pick('chromium-', 'chrome-linux/chrome')
  const shell = pick('chromium_headless_shell-', 'chrome-headless-shell-linux64/chrome-headless-shell') ?? pick('chromium_headless_shell-', 'chrome-linux/headless_shell')
  if (headlessShell) return shell ?? full ?? { path: null, source: 'none' }
  // A kept browser is watched by a person, so it needs the real browser: a headless shell
  // can still be screenshotted, but it cannot paint a window, and the promise of a visible
  // browser would be a lie while the sidebar happily showed frames.
  return full ?? shell ?? { path: null, source: 'no chromium build found' }
}

/** The live view on, against the page that is open. Idempotent — and if the watcher's box changed
 *  width since it started, the stream is restarted at the new size rather than carrying on sending
 *  pictures for a box that no longer exists. */
async function startCast(width = 0) {
  if (!page) return { ok: false, why: 'no page is open' }
  const want = Math.max(0, Math.min(Number(width) || 0, 4096))
  if (casting && cdp && want === castWidth) return { ok: true, live: true, note: castNote }
  if (casting && want !== castWidth) await stopCast()
  castWidth = want
  const size = castSize()
  try {
    if (!cdp) cdp = await context.newCDPSession(page)
    cdp.removeAllListeners?.('Page.screencastFrame')
    cdp.on('Page.screencastFrame', (f) => {
      // The ack is owed for every frame or Chromium stops sending them, gated or not.
      cdp?.send('Page.screencastFrameAck', { sessionId: f.sessionId }).catch(() => {})
      const now = Date.now()
      if (now - lastPushAt < VIEW_MIN_MS) return
      lastPushAt = now
      framesTaken++
      const bytes = Math.ceil(((f.data?.length ?? 0) * 3) / 4)
      // `viewport` stays the *page's* size: the panel scales its presses by that, and a picture
      // smaller than the page changes the picture, not the page.
      send({ ev: 'frame', data: f.data, mime: 'image/jpeg', bytes, viewport: VIEWPORT, pic_width: size.width })
    })
    await cdp.send('Page.startScreencast', {
      format: 'jpeg',
      quality: VIEW_QUALITY,
      maxWidth: size.width,
      maxHeight: size.height,
      everyNthFrame: 1,
    })
    casting = true
    castNote = ''
    return { ok: true, live: true, picture: size, note: '' }
  } catch (e) {
    // An older Chromium with no screencast, or a CDP session the page threw away. The panel keeps
    // working off stills, so this is reported as "no live stream here" rather than as a failure.
    castNote = String(e?.message ?? e).split('\n')[0]
    casting = false
    return { ok: true, live: false, why: castNote }
  }
}

async function stopCast() {
  if (!casting) return { ok: true, live: false }
  try {
    await cdp?.send('Page.stopScreencast')
  } catch {
    /* the page went away with it */
  }
  casting = false
  return { ok: true, live: false }
}
async function ensureBrowser() {
  if (browser) return { ok: true }
  const found = findBrowser()
  if (!found.path) {
    return {
      ok: false,
      why:
        'no browser to open this with. Install one for Playwright (`npx playwright install chromium`), ' +
        'or point CWOFFEE_BROWSER_PATH at a Chrome/Chromium binary.',
      browser: found.source,
    }
  }
  // Headed by default: a kept browser exists because somebody is looking at it. The hub can
  // ask for headless (a model-only session on a machine where the picture is only ever a
  // frame stream), and a headed launch that the display refuses falls back — reported.
  const wantHeaded = process.env.CWOFFEE_BROWSER_SESSION_HEADLESS !== '1'
  let note = ''
  for (const mode of wantHeaded ? [false, true] : [true]) {
    try {
      browser = await chromium.launch({
        executablePath: found.path,
        headless: mode,
        args: ['--no-first-run', '--no-default-browser-check', '--disable-sync', '--disable-extensions'],
        timeout: 20_000,
      })
      if (mode && wantHeaded) note = 'running headless: a visible window would not open here'
      break
    } catch (e) {
      const why = String(e?.message ?? e).split('\n')[0]
      if (mode === false) {
        note = `a visible window would not open (${why}); running headless instead`
        continue
      }
      return { ok: false, why: `the browser would not start: ${why}`, browser: found.source, path: found.path }
    }
  }
  browser.on('disconnected', () => noteDead('the browser process went away'))
  context = await browser.newContext({ viewport: VIEWPORT, acceptDownloads: false })
  page = await context.newPage()
  page.on('console', (m) => {
    if (consoleTail.length >= LIMITS.consoleLines) consoleTail.shift()
    const t = m.text()
    consoleTail.push({ type: m.type(), text: t.length > LIMITS.console ? `${t.slice(0, LIMITS.console)}…` : t, doc: docGen })
    send({ ev: 'console', type: m.type(), text: t.length > LIMITS.console ? `${t.slice(0, LIMITS.console)}…` : t })
  })
  page.on('pageerror', (e) => {
    const msg = String(e?.message ?? e).split('\n')[0]
    if (pageErrors.length >= LIMITS.consoleLines) pageErrors.shift()
    pageErrors.push({ text: msg, doc: docGen })
    send({ ev: 'pageerror', text: msg })
  })
  page.on('requestfailed', (r) => {
    if (failedReqs.length >= LIMITS.failed) failedReqs.shift()
    failedReqs.push({ url: r.url(), method: r.method(), why: r.failure()?.errorText ?? 'failed', doc: docGen })
    send({ ev: 'requestfailed', url: r.url(), why: r.failure()?.errorText ?? 'failed' })
  })
  page.on('download', (d) => {
    if (downloadsBlocked.length < LIMITS.failed) downloadsBlocked.push(d.suggestedFilename() || 'a file')
    send({ ev: 'download', name: d.suggestedFilename() || 'a file' })
    d.cancel().catch(() => {})
  })
  page.on('framenavigated', (f) => {
    if (f !== page.mainFrame()) return
    // A new document starts the count. Records made by the page that is being replaced belong to that
    // page, and a `health` read that mixes them in reports numbers about a document nobody is looking
    // at — which is the same mistake as reading the old page's title off a new one.
    docGen++
    send({ ev: 'nav', url: f.url() })
  })
  return { ok: true, note, browser: found }
}

function pageState() {
  return {
    alive: !!page && !dead,
    url: page ? page.url() : '',
    title: '',
    loading: !!page,
    can_back: false,
    can_forward: false,
    console: consoleTail.slice(-30),
    page_errors: pageErrors.slice(-10).map((e) => e.text),
    requests_failed: failedReqs.slice(-10),
    downloads_blocked: downloadsBlocked.slice(0, 10),
    frames_taken: framesTaken,
    viewport: VIEWPORT,
    live: casting,
    ...(castNote ? { live_why: castNote } : {}),
    ...(dead ? { why: dead } : {}),
  }
}

// A target is named the way a person names it, in the same order of confidence as
// driver.mjs, and an ambiguous match is refused rather than guessed at. Two "Save"
// buttons and a click on the wrong one is the worst thing this can do while a person is
// watching it happen.
//
// `any` is for the verbs that only look. Waiting for a thing to appear is answered by any of the
// things that match — nothing is touched, and no wrong element can be harmed — so refusing there
// costs a round trip to name a page the model has already read.
async function locate(target, { any = false } = {}) {
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
    if (n > 1 && any) return { loc: loc.first(), n }
    if (n > 1) {
      // Name the candidates. A model cannot write a narrower selector than the page can show it,
      // so "name it more narrowly" by itself just costs another round trip.
      const shown = []
      for (let i = 0; i < Math.min(n, 4); i++) {
        const one = await loc
          .nth(i)
          .evaluate((el) => {
            const cls = typeof el.className === 'string' && el.className.trim() ? '.' + el.className.trim().split(/\s+/).slice(0, 2).join('.') : ''
            const bits = []
            if (el.getAttribute('placeholder')) bits.push(`placeholder=${JSON.stringify(el.getAttribute('placeholder'))}`)
            const txt = (el.textContent || '').trim().replace(/\s+/g, ' ').slice(0, 40)
            if (txt) bits.push(JSON.stringify(txt))
            return `${el.tagName.toLowerCase()}${el.id ? `#${el.id}` : ''}${cls}${bits.length ? ' (' + bits.join(' ') + ')' : ''}`
          })
          .catch(() => null)
        if (one) shown.push(one)
      }
      return {
        err:
          `${n} elements match ${JSON.stringify(t)}${shown.length ? `: ${shown.join(' | ')}` : ''}` +
          (n > 4 ? ` (and ${n - 4} more)` : '') +
          `. Say which one — an id, a class, a placeholder, or the words in its label. Nothing was touched.`,
      }
    }
  }
  return { err: `nothing on the page matches ${JSON.stringify(t)}` }
}

async function cmdOpen(req) {
  const url = String(req.url ?? '').trim()
  if (!url) return { ok: false, why: 'no url to open' }
  let parsed
  try {
    parsed = new URL(url)
  } catch {
    return { ok: false, why: `"${url}" is not a URL the browser can open` }
  }
  // Same scheme discipline as driver.mjs: http/https, and file: only when the host — which
  // checked the path against the workspace and against what the person approved — says so.
  if (!['http:', 'https:'].includes(parsed.protocol) && !(req.allow_file === true && parsed.protocol === 'file:')) {
    return { ok: false, why: `refusing to open ${parsed.protocol}//… : the browser opens http and https, and a file: URL only with the host's permission` }
  }
  const e = await ensureBrowser()
  if (!e.ok) return e
  const t = Math.max(1_000, Math.min(Number(req.timeout_ms ?? 30_000), 120_000))
  try {
    const resp = await page.goto(url, {
      waitUntil: ['load', 'networkidle', 'domcontentloaded'].includes(req.wait_until) ? req.wait_until : 'load',
      timeout: t,
    })
    return { ok: true, url_after: page.url(), status: resp ? resp.status() : null, title: await page.title().catch(() => ''), note: e.note ?? '' }
  } catch (err) {
    return { ok: false, why: String(err?.message ?? err).split('\n')[0], url_after: page.url() }
  }
}

async function cmdAct(req) {
  if (!page) return { ok: false, why: 'no page is open' }
  const what = String(req.do ?? '')
  const stepMs = Math.max(500, Math.min(Number(req.timeout_ms ?? 15_000), 60_000))
  let waitedMs = 0
  try {
    if (what === 'click') {
      const l = await locate(req.target)
      if (l.err) return { ok: false, why: l.err }
      await l.loc.click({ timeout: stepMs })
    } else if (what === 'fill') {
      const l = await locate(req.target)
      if (l.err) return { ok: false, why: l.err }
      const want = String(req.text ?? '')
      // **A `<select>` is chosen from, not typed into.** Playwright's own `fill` says no to one, and
      // all the model has are the words a person reads in the list, so the choice is matched against
      // the options' text and then their values. A name that is on no list says what the lists
      // offers — a guess at a value would either do nothing or pick the wrong thing silently.
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
          return {
            ok: false,
            why: `${JSON.stringify(want)} is not a choice on this list; it offers ${names.join(', ') || 'nothing'}`,
          }
        }
        return { ok: true, chose: picked.text || picked.value, ...(l.n > 1 ? { note: `${l.n} things matched, and the first one was chosen` } : {}) }
      }
      await l.loc.fill(want, { timeout: stepMs })
    } else if (what === 'press') {
      await page.keyboard.press(String(req.key ?? 'Enter'))
    } else if (what === 'click_xy') {
      // A click at a point rather than at a selector. This exists for the person's panel: the
      // picture it shows *is* the viewport at the size `state` reports, so a point in the picture
      // is a point in the page, and the panel can hand the click to the same browser the model
      // drives instead of pretending to click. The caller does the scaling; the driver does the
      // click, so the page sees a real mouse event with real coordinates.
      const x = Number(req.x)
      const y = Number(req.y)
      if (!Number.isFinite(x) || !Number.isFinite(y)) return { ok: false, why: 'click_xy needs x and y as numbers' }
      if (x < 0 || y < 0 || x > VIEWPORT.width || y > VIEWPORT.height)
        return { ok: false, why: `(${Math.round(x)},${Math.round(y)}) is outside the ${VIEWPORT.width}x${VIEWPORT.height} page` }
      await page.mouse.move(x, y)
      await page.mouse.click(x, y)
      // A click can start a navigation; the nav event arrives on its own, and the caller is owed
      // an answer for the click itself either way.
      await page.waitForTimeout(Math.min(250, stepMs))
    } else if (what === 'type_keys') {
      // Type into whatever the page already has focus, which is what the panel's keyboard row does
      // after clicking a field in the picture. `fill` needs a selector; a person aiming at a box
      // with their mouse has no selector to name, and neither has the picture.
      const t = String(req.text ?? '')
      if (!t) return { ok: false, why: 'type_keys needs text' }
      if (t.length > 2000) return { ok: false, why: 'type_keys takes at most 2000 characters at a time' }
      await page.keyboard.type(t, { delay: 12 })
    } else if (what === 'scroll') {
      // Wheel at a point when one is given: the panel scrolls the part of the picture the pointer
      // is over, which is the only scroll gesture that means anything to a person looking at it.
      if (Number.isFinite(Number(req.x)) && Number.isFinite(Number(req.y)))
        await page.mouse.move(Number(req.x), Number(req.y))
      await page.mouse.wheel(0, Number(req.px ?? 600))
    } else if (what === 'wait') {
      const want = Math.min(Number(req.ms ?? 500), 30_000)
      const t0 = Date.now()
      await page.waitForTimeout(want)
      // Measured, not echoed. A caller comparing two drivers (or two pages) can only trust a wait
      // whose number came from the clock.
      waitedMs = Date.now() - t0
    } else if (what === 'wait_for') {
      // Look, and keep looking for the time this step was given. One check is not a wait: the thing
      // being waited for is usually the thing the page has not finished doing, and answering in 3ms
      // about a 4-second wait tells the model the page failed at its own job.
      const want = Math.max(0, Math.min(Number(req.ms ?? stepMs), 30_000))
      const t0 = Date.now()
      let lastWhy = `nothing on the page matches ${JSON.stringify(String(req.target ?? ''))}`
      let many = 0
      for (;;) {
        const l = await locate(req.target, { any: true })
        if (!l.err) {
          many = l.n || 1
          try {
            await l.loc.waitFor({ state: 'visible', timeout: Math.min(200, Math.max(0, want)) })
            break
          } catch {
            lastWhy = `${JSON.stringify(String(req.target ?? ''))} is on the page but not visible`
          }
        }
        const looked = Date.now() - t0
        if (looked >= want) {
          return { ok: false, why: `${lastWhy} — looked for ${looked}ms and it never appeared`, waited: looked }
        }
        await page.waitForTimeout(60)
      }
      waitedMs = Date.now() - t0
      // Say how many matched, so "it appeared" is not read as "there was only one".
      return { ok: true, waited: waitedMs, ...(many > 1 ? { note: `${many} things matched, and the first one appeared` } : {}) }
    } else if (what === 'eval') {
      const v = await page.evaluate(String(req.code ?? 'document.title'))
      return { ok: true, value: typeof v === 'string' ? v.slice(0, LIMITS.value) : JSON.stringify(v)?.slice(0, LIMITS.value) }
    } else if (what === 'text') {
      const v = await page.evaluate(() => document.body?.innerText ?? '')
      return { ok: true, value: v.slice(0, 24_000), truncated: v.length > 24_000 }
    } else if (what === 'snapshot') {
      const items = await page.evaluate(() => {
        const out = []
        const sel = 'a,button,input,select,textarea,h1,h2,h3,[role=button],[role=tab],[role=heading],[aria-label]'
        for (const el of document.querySelectorAll(sel)) {
          if (out.length >= 120) break
          const label = (el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.getAttribute('name') || el.textContent || '').trim().replace(/\s+/g, ' ').slice(0, 60)
          const tag = el.tagName.toLowerCase()
          const role = el.getAttribute('role') || (tag === 'a' ? 'link' : tag === 'button' ? 'button' : tag === 'input' ? `input[${el.getAttribute('type') || 'text'}]` : tag)
          out.push(`${role}${el.id ? ' #' + el.id : ''}${label ? ` "${label}"` : ''}`)
        }
        return out
      })
      return { ok: true, value: items.join('\n') }
    } else if (what === 'text') {
      const raw = await page.evaluate('document.body ? document.body.innerText : ""').catch(() => '')
      const s = String(raw ?? '')
      return { ok: true, value: s.slice(0, 24_000), truncated: s.length > 24_000, url_after: page.url(), title: await page.title().catch(() => '') }
    } else if (what === 'snapshot' || what === 'a11y') {
      // The same shape of answer the page bridge gives, on purpose. A model that asked for `a11y` and
      // gets a different dialect depending on which browser the host happened to keep is how "the page
      // has no elements" comes to mean "this driver answers in a shape nobody renders".
      const want = what === 'a11y' ? Math.max(1, Math.min(Number(req.max ?? req.limit ?? 120), 400)) : 120
      const r = await describePage(want).catch((e) => ({ err: String(e?.message ?? e).split('\n')[0] }))
      if (r?.err) return { ok: false, why: `the page would not describe itself: ${r.err}` }
      const said = (r.items ?? []).map(describeLine).join('\n')
      return {
        ok: true,
        value: said || (what === 'a11y' ? 'nothing visible in this page to describe' : 'this page has no controls to describe'),
        total: r.total ?? (r.items ?? []).length,
        truncated: !!r.truncated,
        url_after: page.url(),
        title: await page.title().catch(() => ''),
      }
    } else if (what === 'health') {
      // One readable sentence, counted from the records this *document* made — the same rule the page
      // bridge follows, and the reason a `health` read after a navigation cannot report the numbers a
      // page that is gone once made.
      const mine = (a) => a.filter((x) => x.doc === docGen)
      const url = page.url()
      const title = await page.title().catch(() => '')
      const ready = await page.evaluate('document.readyState').catch(() => 'unknown')
      const bits = [
        `${mine(consoleTail).length} console message(s)`,
        `${mine(pageErrors).length} uncaught error(s)`,
        `${mine(failedReqs).length} request(s) that did not load`,
      ]
      const worst = mine(failedReqs).slice(-4).map((f) => `${f.url} (${f.why})`).join(' | ')
      return {
        ok: true,
        value:
          `${ready} at ${url}${title ? ` — "${title}"` : ''}: ` +
          bits.join(', ') +
          (worst ? `; the last things that went wrong: ${worst}` : '; nothing has gone wrong on this page'),
        counts: { console: mine(consoleTail).length, error: mine(pageErrors).length, network: mine(failedReqs).length },
        url_after: url,
        title,
      }
    } else if (what === 'navigate') {
      // Point the kept browser at another page. Same url rules as open: the workspace's file:// pages
      // are allowed because "open the html file you just made" is the commonest thing a walk starts with.
      const url = String(req.url ?? '')
      if (!/^https?:\/\//i.test(url) && !url.startsWith('file://')) {
        return { ok: false, why: `navigate wants an http(s) or file:// url, not ${JSON.stringify(url)}` }
      }
      await page.goto(url, { timeout: stepMs, waitUntil: 'domcontentloaded' }).catch(() => null)
    } else if (what === 'back' || what === 'forward') {
      // One history step, in the direction asked for. At the end of the list Playwright resolves
      // to null rather than throwing, so "Back" on a first page is reported as ok at the url it was
      // already at — which is what a person sees, and not a failure to explain.
      //
      // `domcontentloaded` rather than the default `load`: a history walk to a page with a slow
      // ad network used to hold the whole step open for every subresource on it, and the caller's
      // timeout fired on a page that had already changed. The step is about the document, not about
      // everything in it.
      const h = what === 'back' ? page.goBack({ timeout: stepMs, waitUntil: 'domcontentloaded' }) : page.goForward({ timeout: stepMs, waitUntil: 'domcontentloaded' })
      await h.catch(() => null)
    } else if (what === 'reload') {
      // Reload re-reads the url that is open, and nothing else. This used to share a branch with
      // back/forward and call `goBack()` first for all three, which walked the page one step
      // backwards every time somebody pressed Reload — a bug invisible until something compared
      // the url before and after.
      await page.reload({ timeout: stepMs, waitUntil: 'domcontentloaded' }).catch(() => null)
    } else if (what === 'stop') {
      // Playwright has no "stop loading"; dropping the pending navigation is what a person
      // means. Esc is the closest honest act, and the state line says what was done.
      await page.keyboard.press('Escape')
    } else {
      return { ok: false, why: `I do not know the act "${what}"` }
    }
    return { ok: true, url_after: page.url(), title: await page.title().catch(() => ''), ...(waitedMs ? { waited: waitedMs } : {}) }
  } catch (err) {
    return { ok: false, why: String(err?.message ?? err).split('\n')[0] }
  }
}

// Describe the page the way the page bridge does: role, id, name, value, and whether it is dead.
// Run inside the page, so it sees the same elements the person can see. Kept in step with
// `crates/tauri-app/src/page_bridge.js`'s `a11y` — two dialects here is how a page that is full of
// controls comes to look empty to the model.
async function describePage(limit) {
  const r = await page.evaluate((want) => {
    const sel = 'a,button,input,select,textarea,h1,h2,h3,[role],[aria-label],[tabindex],summary,form,[contenteditable=true]'
    const visible = (el) => {
      const b = el.getBoundingClientRect()
      const cs = getComputedStyle(el)
      return b.width > 0 && b.height > 0 && cs.visibility !== 'hidden' && cs.display !== 'none'
    }
    const roleOf = (el) => {
      const own = el.getAttribute('role')
      if (own) return own
      const t = el.tagName.toLowerCase()
      if (t === 'a' && el.hasAttribute('href')) return 'link'
      if (t === 'input') return `input[${el.getAttribute('type') || 'text'}]`
      if (t === 'select') return 'select'
      if (t === 'textarea') return 'textarea'
      if (t === 'form') return 'form'
      return t
    }
    const nameOf = (el) =>
      (
        el.getAttribute('aria-label') ||
        el.getAttribute('placeholder') ||
        (el.labels && el.labels[0] ? el.labels[0].textContent : '') ||
        el.textContent ||
        el.getAttribute('title') ||
        el.getAttribute('value') ||
        ''
      )
        .replace(/\s+/g, ' ')
        .trim()
        .slice(0, 120)
    const items = []
    let total = 0
    for (const el of document.querySelectorAll(sel)) {
      if (!visible(el)) continue
      total++
      if (items.length >= want) continue
      const b = el.getBoundingClientRect()
      items.push({
        role: roleOf(el),
        id: el.id || '',
        name: nameOf(el),
        value: 'value' in el && el.type !== 'password' ? String(el.value ?? '').slice(0, 120) : '',
        disabled: !!el.disabled,
        href: el.href ? String(el.href).slice(0, 300) : '',
        rect: { x: Math.round(b.x), y: Math.round(b.y), w: Math.round(b.width), h: Math.round(b.height) },
      })
    }
    return { items, total, truncated: total > items.length }
  }, limit)
  return r
}

const describeLine = (it) =>
  `${it.role}${it.id ? ' #' + it.id : ''}${it.name ? ` "${it.name}"` : ''}${it.value ? ` = ${it.value}` : ''}${it.disabled ? ' (disabled)' : ''}`

async function cmdFrame(req) {
  if (!page) return { ok: false, why: 'no page is open' }
  const type = req.type === 'png' ? 'png' : 'jpeg'
  try {
    const buf = await page.screenshot({ type, quality: type === 'jpeg' ? Math.min(Math.max(Number(req.quality ?? 55), 20), 90) : undefined, timeout: 10_000 })
    framesTaken++
    return { ok: true, b64: buf.toString('base64'), bytes: buf.length, mime: `image/${type}`, viewport: VIEWPORT }
  } catch (err) {
    return { ok: false, why: String(err?.message ?? err).split('\n')[0] }
  }
}

async function cmdViewport(req) {
  if (!page) return { ok: false, why: 'no page is open' }
  const next = clampViewport(`${req.width ?? VIEWPORT.width}x${req.height ?? VIEWPORT.height}`)
  if (next.width === VIEWPORT.width && next.height === VIEWPORT.height) return { ok: true, viewport: VIEWPORT, moved: false }
  VIEWPORT = next
  try {
    await page.setViewportSize(VIEWPORT)
  } catch (e) {
    return { ok: false, why: String(e?.message ?? e).split('\n')[0], viewport: VIEWPORT }
  }
  // The screencast's own size was fixed when it started, so a resize restarts it — otherwise the
  // panel is handed a picture of the old page size and every click inside it lands elsewhere.
  if (casting) {
    await stopCast()
    // The hint the watcher gave survives the resize: the box it is showing did not change, the page
    // inside it did.
    await startCast(castWidth)
  }
  return { ok: true, viewport: VIEWPORT, moved: true }
}

const handlers = {
  open: cmdOpen,
  act: cmdAct,
  frame: cmdFrame,
  watch: async (req) => startCast(Number(req.max_width ?? (casting ? castWidth : 0)) || 0),
  unwatch: async () => stopCast(),
  viewport: cmdViewport,
  state: async () => ({ ok: true, ...(pageState() ?? {}) }),
  close: async () => {
    noteDead('closed on request')
    return { ok: true }
  },
  ping: async () => ({ ok: true, alive: !!page, dead }),
}

// Commands run one at a time. A person typing a URL while a click is in flight is not
// asking for the two to interleave, and a model that sends three acts expects them to land
// in the order it wrote them.
let queue = Promise.resolve()
const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity })
rl.on('line', (line) => {
  const raw = line.trim()
  if (!raw) return
  let req
  try {
    req = JSON.parse(raw)
  } catch {
    send({ id: null, ok: false, why: 'that is not JSON' })
    return
  }
  armIdle()
  queue = queue.then(async () => {
    const h = handlers[String(req.cmd ?? '')]
    if (!h) {
      send({ id: req.id ?? null, ok: false, why: `I do not know the command "${req.cmd}"` })
      return
    }
    // Every command after the browser is gone gets the same answer, in the hub's shape:
    // not "no such command", but "this browser is not alive, and here is why".
    if (dead && req.cmd !== 'state' && req.cmd !== 'ping') {
      send({ id: req.id ?? null, ok: false, why: `that browser is closed (${dead})`, dead: true })
      return
    }
    let out
    try {
      out = await h(req)
    } catch (e) {
      out = { ok: false, why: String(e?.message ?? e) }
    }
    send({ id: req.id ?? null, ...out })
  })
  queue = queue.catch(() => {})
})

// The hub is gone if stdin is gone. That is the backstop for a killed app: nobody should
// have to find a stray chromium with `ps` because a window closed uncleanly.
rl.on('close', () => {
  noteDead('the app stopped talking to it')
  shutdown(0)
})
process.on('SIGTERM', () => {
  noteDead('the app ended it')
  shutdown(0)
})
process.on('SIGINT', () => {
  noteDead('the app ended it')
  shutdown(0)
})
process.on('unhandledRejection', (e) => send({ ev: 'note', why: `unhandled: ${String(e?.message ?? e)}` }))

const found = findBrowser()
send({
  ev: 'ready',
  pid: process.pid,
  idle_ms: DRIVER_IDLE_MS,
  viewport: VIEWPORT,
  browser: found.path ? { source: found.source, path: found.path } : { source: found.source, path: null },
})
armIdle()
