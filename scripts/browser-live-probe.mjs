// The browser panel, driven against a real browser's real frames.
//
// What this grades, and why nothing else in the repo can:
//
//   1. **The live view reaches the picture.** Frames here come out of `browser/session.mjs` — the
//      real driver, the real Chromium, its own CDP screencast — and are handed to the panel one poll
//      at a time. A page that repaints with nobody acting on it must change the picture on screen.
//      A fake driver can answer "here is a frame"; it cannot *repaint*, and repainting is the claim.
//   2. **The column spans the window and the picture takes the slack**, so a tall window does not
//      end in a band of empty card.
//   3. **A press on the picture is a press on the page.** The mapping goes through the *drawn*
//      rectangle (`ui/src/lib/pagefit.ts`), not the element box; here the picture is a real page's
//      pixels, so the page itself can report whether the click arrived and where.
//   4. **Full window** gives the page the whole rectangle, and the page is asked to take its shape.
//   5. **The idle clock is still on show** — watching a browser is not using one, and the five-minute
//      rule is what keeps a kept browser from outliving the visit.
//
// The panel talks to a stubbed Tauri IPC (`window.__TAURI_INTERNALS__`) whose browser verbs are
// answered by the driver process, so no Rust binary is involved: this is the UI half of the seam,
// with the browser half real.
//
//   node scripts/browser-live-probe.mjs [dist-dir] [port]
//
// Needs `ui/dist` (npm --prefix ui run build) and a Chromium build for Playwright.

import { chromium } from '../browser/node_modules/playwright-core/index.mjs'
import { spawn } from 'node:child_process'
import http from 'node:http'
import fs from 'node:fs'
import path from 'node:path'

const DIST = path.resolve(process.argv[2] ?? 'ui/dist')
const PORT = Number(process.argv[3] ?? 8817)
const PAGE_PORT = PORT + 1

if (!fs.existsSync(DIST)) {
  console.log(`FAIL — no UI bundle at ${DIST} (run \`npm --prefix ui run build\`)`)
  process.exit(1)
}

/* --------------------------- the page under test --------------------------- */
// A fixed box at a published page point, which counts its own clicks and reports them to this
// process. The panel aims at a picture of the page; the page is the witness for whether the click
// landed where the picture showed it.
const AIM = { x: 620, y: 420, w: 200, h: 90 }
let pageClicks = 0
let pageLoads = 0
const TYPES = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.png': 'image/png', '.webp': 'image/webp', '.svg': 'image/svg+xml' }
const handler = (req, res) => {
  const rel = decodeURIComponent((req.url || '/').split('?')[0])
  if (rel === '/aim-click') {
    pageClicks++
    res.writeHead(200, { 'content-type': 'application/json' })
    return res.end(JSON.stringify({ ok: true, clicks: pageClicks }))
  }
  if (rel === '/probe-page.html') {
    pageLoads++
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' })
    return res.end(
      `<!doctype html><html><head><meta charset="utf-8"><title>live view probe</title></head>` +
        `<body style="margin:0;font:14px system-ui;background:#fff">` +
        `<h1>the page is moving</h1><div id=c style="font:28px monospace"></div>` +
        `<div id=aim style="position:fixed;left:${AIM.x - AIM.w / 2}px;top:${AIM.y - AIM.h / 2}px;width:${AIM.w}px;height:${AIM.h}px;` +
        `box-sizing:border-box;border:3px solid #0a7f5c;background:#eafff6;display:flex;align-items:center;justify-content:center">aim here</div>` +
        `<div style="height:2400px">tall page: a wheel has somewhere to go</div>` +
        `<script>let n=0;setInterval(()=>{document.getElementById('c').textContent='tick '+(n++)},110);` +
        `document.getElementById('aim').onclick=()=>fetch('/aim-click');</script></body></html>`,
    )
  }
  const file = path.join(DIST, rel === '/' ? 'index.html' : rel)
  if (!file.startsWith(DIST) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
    res.writeHead(404).end('not here')
    return
  }
  res.writeHead(200, { 'content-type': TYPES[path.extname(file)] ?? 'application/octet-stream' })
  fs.createReadStream(file).pipe(res)
}
// Two ports, one handler: the UI bundle and the page under test are served apart so the browser
// under test never loads the app's own files.
const appServer = http.createServer(handler)
const pageServer = http.createServer(handler)
await new Promise((r) => appServer.listen(PORT, '127.0.0.1', r))
await new Promise((r) => pageServer.listen(PAGE_PORT, '127.0.0.1', r))

/* ------------------------------ the real browser ---------------------------- */
// The hub's picture bookkeeping, held here so the stub answers the way `browser_hub` does: every
// frame the browser volunteers gets an id of its own, and the newest picture records the page state
// it was drawn against. Declared before the driver starts, because its first frames can land at any
// moment after `watch`.
let hubPic = 0
let hubFrameSeq = 0
let liveFrameSeq = 1
let latest = null
let askedWidth = 0
const driver = spawn(process.env.CWOFFEE_NODE ?? 'node', [path.join('browser', 'session.mjs')], {
  env: { ...process.env, CWOFFEE_BROWSER_HEADLESS_ONLY: '1', CWOFFEE_BROWSER_VIEW_MIN_MS: '60' },
})
const events = []
const answers = new Map()
let stdout = ''
driver.stdout.on('data', (d) => {
  stdout += d
  let i
  while ((i = stdout.indexOf('\n')) >= 0) {
    const line = stdout.slice(0, i)
    stdout = stdout.slice(i + 1)
    let v
    try {
      v = JSON.parse(line)
    } catch {
      continue
    }
    if (v.id !== undefined) answers.get(v.id)?.(v)
    else {
      events.push(v)
      // A frame the browser volunteered is a new picture, with an id of its own — this is the part
      // of the live view a fake cannot produce, and the whole reason the probe runs a real browser.
      if (v.ev === 'frame' && v.data) {
        latest = v
        hubPic++
        hubFrameSeq = liveFrameSeq
      }
    }
  }
})
driver.stderr.on('data', (d) => process.stderr.write(`[driver] ${d}`))
let nextId = 1
const ask = (req, ms = 30_000) =>
  new Promise((resolve, reject) => {
    const id = nextId++
    const t = setTimeout(() => reject(new Error(`the driver did not answer ${req.cmd} in ${ms}ms`)), ms)
    answers.set(id, (v) => {
      clearTimeout(t)
      answers.delete(id)
      resolve(v)
    })
    driver.stdin.write(JSON.stringify({ ...req, id }) + '\n')
  })
const waitFor = async (ev, ms) => {
  const t0 = Date.now()
  while (Date.now() - t0 < ms) {
    const hit = events.find((e) => e.ev === ev)
    if (hit) return hit
    await new Promise((r) => setTimeout(r, 50))
  }
  return null
}

const ready = await waitFor('ready', 8_000)
if (!ready) {
  console.log('FAIL — the kept-browser driver never said ready')
  process.exit(1)
}
const opened = await ask({ cmd: 'open', url: `http://127.0.0.1:${PAGE_PORT}/probe-page.html` })
if (!opened.ok) {
  console.log(`FAIL — the probe page did not open: ${opened.why} (is something already on port ${PAGE_PORT}?)`)
  process.exit(1)
}
await ask({ cmd: 'watch' })

/* -------------------------- the UI, over stubbed IPC ------------------------ */
// The panel's browser verbs are answered from the driver, in the shape `browser_hub` uses: picture
// ids, the `unchanged` no, the viewport the browser actually has. Everything else answers with an
// empty shape — this probe is about the panel, and a boot that needs a model would need a model.
const stub = `
const P = (o) => ({ mounted: true, driver: 'browser/session.mjs', node: 'node', keep_off: false,
  idle_secs: 300, idle_left: 271, alive: true, opened: true,
  url: 'http://127.0.0.1:${PAGE_PORT}/probe-page.html', title: 'live view probe',
  opened_by: 'person', last_actor: 'person', last_act: 'opened the probe page', why: '',
  console: [], page_errors: [], requests_failed: [], downloads_blocked: [],
  frames_taken: window.__s.frames, live: window.__s.live, live_why: '', frame_pic: window.__s.pic,
  viewport: { width: window.__s.vp.w, height: window.__s.vp.h }, seq: window.__s.seq,
  writable: true, route: 'this window', says: 'a browser is open, and it is pushing its own frames', ...o });
window.__calls = [];
window.__s = { alive: true, live: true, pic: 0, frameSeq: 0, seq: 1, frames: 0, vp: { w: 1280, h: 800 }, act: null, lastBytes: 0, askedW: 0 };
window.__cbid = 1; window.__cbs = {};
window.__TAURI_INTERNALS__ = {
  transformCallback(cb) { const id = window.__cbid++; window.__cbs[id] = cb; return id },
  unregisterCallback(id) { delete window.__cbs[id] },
  async invoke(cmd, args) {
    window.__calls.push({ cmd, args });
    const s = window.__s;
    if (cmd === 'browser_panel') return P({});
    if (cmd === 'browser_watch') {
      s.askedW = args.max_width ?? 0;
      const r = await window.__probe({ cmd: 'watch', on: !!args.on, max_width: args.max_width ?? 0 });
      s.live = !!args.on && r.live !== false; s.seq++;
      return P({ live: s.live });
    }
    if (cmd === 'browser_viewport') {
      const r = await window.__probe({ cmd: 'viewport', width: args.width, height: args.height });
      if (r.viewport) s.vp = { w: r.viewport.width, h: r.viewport.height };
      s.seq++; s.frameSeq = s.seq;   // the resize drew a picture of the new state
      return P({ moved: r.moved, viewport: r.viewport });
    }
    if (cmd === 'browser_frame') {
      // The hub's rule, mirrored: a volunteered frame is a newer *picture*, and a picture that does
      // not picture the newest state owes the viewer one whatever the ids say.
      const a = await window.__probe({ cmd: 'avail', seq: s.seq });
      s.pic = a.pic;
      if (a.pic > args.since && a.has) {
        const f = await window.__probe({ cmd: 'take' });
        s.frames++; s.lastBytes = f.bytes;
        return { unchanged: false, b64: f.b64, mime: f.mime, bytes: f.bytes, seq: s.seq, pic: a.pic, live: s.live };
      }
      if (a.framesState) return { unchanged: true, seq: s.seq, frame_pic: a.pic };
      const f = await window.__probe({ cmd: 'still', seq: s.seq });
      if (!f.ok) return { unchanged: true, why: f.why, seq: s.seq };
      s.frames++; s.pic = a.pic + 1; s.lastBytes = f.bytes;
      return { unchanged: false, b64: f.b64, mime: f.mime, bytes: f.bytes, seq: s.seq, pic: s.pic, live: s.live };
    }
    if (cmd === 'browser_act') {
      const r = await window.__probe({ cmd: 'act', act: args.act });
      s.act = args.act; s.seq++; s.frameSeq = 0;   // the page changed; no picture shows it yet
      return P({ act_ok: r.ok, act_why: r.why, act_value: r.value });
    }
    if (cmd === 'browser_close') { s.alive = false; s.live = false; s.pic = 0; return P({ alive: false, opened: false, live: false, why: 'closed from the panel' }) }
    if (/_list$|^sessions|^messages|_history$/.test(cmd)) return [];
    return {};
  },
};
`

// The driver calls, run on this side: the UI's stub cannot reach a process, so the frames it shows
// are the ones the real browser sent.
const browser = await chromium.launch({ headless: true })
const page = await browser.newPage({ viewport: { width: 1280, height: 900 } })
await page.exposeFunction('__probe', async (req) => {
  if (typeof req.seq === 'number') liveFrameSeq = req.seq
  if (req.cmd === 'avail') return { pic: hubPic, has: !!latest, framesState: hubFrameSeq >= req.seq }
  if (req.cmd === 'take') {
    const f = latest
    latest = null
    return { b64: f.data, mime: f.mime ?? 'image/jpeg', bytes: f.bytes ?? 0 }
  }
  if (req.cmd === 'still') {
    const f = await ask({ cmd: 'frame', type: 'jpeg' })
    if (f.ok) {
      hubPic++
      hubFrameSeq = req.seq
      f.pic = hubPic
    }
    return f
  }
  if (req.cmd === 'watch') {
    askedWidth = Number(req.max_width) || 0
    return await ask({ cmd: req.on ? 'watch' : 'unwatch', max_width: askedWidth })
  }
  if (req.cmd === 'viewport') return await ask({ cmd: 'viewport', width: req.width, height: req.height })
  if (req.cmd === 'act') return await ask({ cmd: 'act', ...req.act })
  return { ok: false, why: `no probe for ${req.cmd}` }
})
await page.addInitScript(stub)
await page.addInitScript(() => {
  try {
    localStorage.setItem('cwoffee.browserpane', 'open')
  } catch {}
})
await page.goto(`http://127.0.0.1:${PORT}/index.html`, { waitUntil: 'domcontentloaded' })
await page.waitForTimeout(1500)

const fails = []
const ok = []
const note = (m) => ok.push(m)
const bad = (m) => fails.push(m)
const ev = (f) => page.evaluate(f)

const pane = await ev(`(() => { const p = document.querySelector('[data-browser-pane]'); return p ? { alive: p.dataset.browserAlive, live: p.dataset.browserLive } : null })()`)
const until = async (fn, ms, what) => {
  const t0 = Date.now()
  while (Date.now() - t0 < ms) {
    if (await fn()) return true
    await page.waitForTimeout(150)
  }
  return false
}
if (!pane) {
  console.log('FAIL — the browser panel did not render over the stubbed IPC')
} else {
  if (pane.alive !== 'yes') bad(`the panel says alive=${pane.alive} with a browser open`)
  if (pane.live !== 'yes') bad(`the panel says live=${pane.live} while the driver is streaming`)
  else note('the panel calls this a live view, on the driver\'s answer rather than on its own ask')
  if (!(await ev(`window.__calls.some(c => c.cmd === 'browser_watch' && c.args.on === true)`)))
    bad('the panel never asked for the live view (no browser_watch{on:true})')
  else note('opening the panel asks the browser for its screencast')

  // 1. a repaint reaches the picture: the page's own timer moves it, nothing was asked of the page.
  const shotSel = `[data-browser-frame="yes"]`
  const srcOf = () => ev(`(document.querySelector(${JSON.stringify(shotSel)})?.getAttribute('src') ?? '').slice(2000, 2100)`)
  if (!(await until(async () => !!(await srcOf()), 8_000, 'a first picture')))
    bad('no picture of the page in the panel')
  const src1 = await srcOf()
  await page.waitForTimeout(1200)
  const src2 = await srcOf()
  if (!src2) bad('the picture went away while the browser was streaming')
  else if (src1 === src2) bad('the picture never changed while a real page was repainting — the live view is not reaching the img')
  else note('a real page repainting on its own repaints the picture, with no act in front of it')
  const decoded = await ev(`(() => { const i = document.querySelector(${JSON.stringify(shotSel)}); return i ? { w: i.naturalWidth, h: i.naturalHeight } : null })()`)
  if (!decoded || !decoded.w) bad('the picture the panel drew is not a decodable image')
  else note(`the picture is the browser's own pixels, decoded in the panel at ${decoded.w}x${decoded.h} — the box's width, not the page's`)
  const polls = await ev(`window.__calls.filter(c => c.cmd === 'browser_frame').length`)
  if (polls < 3) bad(`only ${polls} frame polls in ~3s — the live view is not on its own clock`)
  else note(`${polls} frame polls in ~3s, each answered with no bytes unless a picture is newer`)

  // 2. the height claim, in a window taller than the column's rows.
  const geo = await ev(`(() => { const p = document.querySelector('[data-browser-pane]').getBoundingClientRect();
    const c = document.querySelector('.panecard').getBoundingClientRect();
    const s = document.querySelector('[data-browser-stage]').getBoundingClientRect();
    const foot = document.querySelector('.bpy-foot').getBoundingClientRect();
    return { win: innerHeight, paneTop: Math.round(p.top), paneBottom: Math.round(p.bottom),
      cardTop: Math.round(c.top), cardBottom: Math.round(c.bottom),
      stage: Math.round(s.height), stageW: Math.round(s.width), underFoot: Math.round(innerHeight - foot.bottom) } })()`)
  // The column is level with the card it stands beside, and it still reaches the bottom of the
  // window's working area: same two lines, no band of dead paint under a page being kept open.
  if (Math.abs(geo.paneTop - geo.cardTop) > 2)
    bad(`the column starts ${geo.paneTop - geo.cardTop}px off the transcript card's top — the two columns should end on one line`)
  if (Math.abs(geo.paneBottom - geo.cardBottom) > 2)
    bad(`the column ends ${geo.paneBottom - geo.cardBottom}px off the transcript card's bottom — the two columns should end on one line`)
  if (geo.win - geo.paneBottom > 24) bad(`the column stops ${geo.win - geo.paneBottom}px above the bottom — a band of dead paint under a page being kept open`)
  else note(`the column spans the window: ${geo.paneTop}px..${geo.paneBottom}px of ${geo.win}px`)
  if (geo.underFoot > 40) bad(`${geo.underFoot}px of empty column under the footer — nothing takes the slack`)
  else note(`the picture area takes the slack: ${geo.stage}px of stage, ${geo.underFoot}px left under the footer`)

  // 2b. the stream is sized to the box that shows it, not to the page. A page is 1280-ish wide; the
  // column is a fraction of that, and page-width pictures sent to it are bytes nobody can see.
  const askedW = await ev(`window.__s.askedW`)
  if (!askedW) bad('the panel asked for a stream without saying how wide its box is')
  else if (Math.abs(askedW - geo.stageW) > 12)
    bad(`the panel asked for ${askedW}px pictures for a ${geo.stageW}px-wide box — the ask does not match the box it is in`)
  else note(`the panel states its box: a ${askedW}px picture for a ${geo.stageW}px-wide stage (the page itself is ${await ev('window.__s.vp.w')}px)`)
  const colBytes = await ev(`window.__s.lastBytes`)

  // 3. the press: aim at the drawn page, and the page reports what arrived.
  const vp = await ev(`window.__s.vp`)
  const box = await ev(`(() => { const i = document.querySelector(${JSON.stringify(shotSel)}); const r = i.getBoundingClientRect(); return { left: r.left, top: r.top, width: r.width, height: r.height } })()`)
  const scale = Math.min(box.width / vp.w, box.height / vp.h)
  const drawLeft = box.left + (box.width - vp.w * scale) / 2
  const drawTop = box.top + (box.height - vp.h * scale) / 2
  await page.mouse.click(drawLeft + AIM.x * scale, drawTop + AIM.y * scale)
  await page.waitForTimeout(900)
  const act = await ev(`window.__s.act`)
  if (!act || act.do !== 'click_xy') bad(`a press on the picture sent ${JSON.stringify(act)} instead of a click at a page point`)
  else if (Math.abs(act.x - AIM.x) > 4 || Math.abs(act.y - AIM.y) > 4)
    bad(`a press on the picture of the aim box became page ${act.x},${act.y}; the aim box is at ${AIM.x},${AIM.y} of a ${vp.w}x${vp.h} page — the picture/page mapping is the suspect`)
  else note(`a press on the picture of the aim box arrives at page ${act.x},${act.y} — where the aim box sits (${vp.w}x${vp.h} page)`)

  // 4. full window: the whole rectangle, and the page asked to fill it.
  await page.click('[data-browser-wide-btn]')
  await page.waitForTimeout(1200)
  const wide = await ev(`(() => { const p = document.querySelector('[data-browser-pane]'); const r = p.getBoundingClientRect();
    const s = document.querySelector('[data-browser-stage]').getBoundingClientRect();
    return { flag: p.dataset.browserWide, w: Math.round(r.width), h: Math.round(r.height), top: Math.round(r.top), win: innerHeight, stage: Math.round(s.height) } })()`)
  if (wide.flag !== 'yes') bad('the full-window control did not mark the pane wide')
  else if (Math.abs(wide.w - 1280) > 2 || Math.abs(wide.h - (900 - 36)) > 2)
    bad(`full window is ${wide.w}x${wide.h} at top ${wide.top}; expected 1280x864 under the 36px titlebar`)
  else note(`full window: ${wide.w}x${wide.h} under the titlebar, ${wide.stage}px of picture area`)
  // The idle clock, while the panel is still on screen: five minutes of nobody *using* the browser
  // still closes it, watching included.
  const clock = await ev(`(() => { const c = document.querySelector('[data-browser-clock]'); return c ? c.textContent.replace(/\\s+/g, ' ').trim() : '' })()`)
  if (!/closes in/.test(clock)) bad(`the panel no longer says when the browser closes itself: ${JSON.stringify(clock.slice(0, 80))}`)
  else note(`the idle clock is still on show: ${JSON.stringify(clock.slice(0, 50))}`)
  const asked = await ev(`window.__calls.filter(c => c.cmd === 'browser_viewport').pop()?.args`)
  if (!asked) bad('the page was never asked to take the shape of the full-window box')
  else note(`the page is asked for ${asked.width}x${asked.height} — the box's own shape, so no letterbox is left`)
  const vpNow = await ev(`window.__s.vp`)
  const wantAspect = 1280 / wide.stage
  const gotAspect = vpNow.w / vpNow.h
  if (Math.abs(gotAspect - wantAspect) / wantAspect > 0.2)
    bad(`the page is ${vpNow.w}x${vpNow.h} for a box of aspect ${wantAspect.toFixed(2)} — the empty space is back as a letterbox`)
  else note(`the page itself is now ${vpNow.w}x${vpNow.h}, the shape of the box it is in: no letterbox left`)

  // The stream follows the box too: a wider box is a bigger picture, which is the cost the ask
  // controls. A panel that asked once and never re-stated its width would show up here.
  const askedWide = await ev(`window.__s.askedW`)
  const wideBytes = await ev(`window.__s.lastBytes`)
  if (askedWide < askedW + 40)
    bad(`full window still asked for ${askedWide}px pictures in a ${wide.stage}px box (the column asked for ${askedW}px)`)
  else if (wideBytes < colBytes * 1.3)
    bad(`full window streamed ${wideBytes}B pictures against ${colBytes}B in the column — the stream did not follow the box`)
  else note(`the stream follows the box: ${colBytes}B per picture at ${askedW}px, ${wideBytes}B at ${askedWide}px`)

  // 5. Escape gives the column back; hiding the panel stops the stream.
  await page.keyboard.press('Escape')
  await page.waitForTimeout(300)
  if ((await ev(`document.querySelector('[data-browser-pane]').dataset.browserWide`)) === 'yes') bad('Escape did not give the column back')
  else note('Escape gives the column back')
  await page.click('[data-browser-close-pane]')
  await page.waitForTimeout(500)
  if (!(await ev(`window.__calls.some(c => c.cmd === 'browser_watch' && c.args.on === false)`)))
    bad('hiding the panel left the browser streaming frames for a panel that is gone')
  else note('hiding the panel stops the screencast: the browser pays for pixels only while somebody looks')
}

await browser.close()
await ask({ cmd: 'close' }).catch(() => {})
driver.kill()
appServer.close()
pageServer.close()
for (const m of ok) console.log(`ok — ${m}`)
if (fails.length) {
  for (const f of fails) console.log(`FAIL — ${f}`)
  console.log(`browser-live-probe: ${fails.length} failure(s) / ${ok.length} ok`)
  process.exitCode = 1
} else {
  console.log(`browser-live-probe: ok (${ok.length} claims)`)
}
