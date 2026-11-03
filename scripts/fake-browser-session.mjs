// A stand-in for `browser/session.mjs`, for tests that need the *seam* rather than a
// browser: the ndjson framing, who-opened-it bookkeeping, the idle clock, and what a
// caller is told when the browser dies mid-request.
//
// It answers the same commands as the real driver and is deliberately literal about page
// state: it echoes the url it was given as `url_after`, and it *drives the events itself*
// (`nav`, `console`, `closed`) so the hub's reader path is exercised rather than stubbed.
//
// Magic urls/acts, so a test can ask for a failure without a flag per failure:
//   open "fail:…"        -> {ok:false, why:"the fake page refused"}
//   open "crash:…"       -> the browser dies mid-open (closed event, then exit)
//   act  {do:"die"}      -> the browser dies during an act
//   act  {do:"nope"}     -> {ok:false, why:"nothing matches"}
//   act  {do:"hang"}     -> never answers, so the caller's timeout is what fires
//   act  {do:"echo"}     -> {ok:true, value:"5"}
//   act  {do:"click_xy", x, y} -> ok, and the point is echoed back so a test can grade the
//                                  arithmetic the panel did (a click at the wrong page pixel is a
//                                  click on the wrong thing, and nothing else in the answer says so)
//   act  {do:"type_keys", text} -> ok, echoing how many characters it was handed
//
// The live view is exercised too: `watch` makes the fake *volunteer* a frame event a moment later,
// which is the shape of the real thing — a frame with no command in front of it, from a repaint.

import readline from 'node:readline'
import fs from 'node:fs'

const send = (o) => fs.writeSync(1, JSON.stringify(o) + '\n')
let nav = ''
let opened = 0
let acts = 0
let frames = 0
let watching = false
// The id a page view's own records carry. Real drivers count up from 1; the fake counts the same
// way so a test can assert that a reader who has seen id N is shown only what came after it.
let diagSeq = 1
let vp = { width: 1280, height: 800 }

send({ ev: 'ready', pid: process.pid, idle_ms: 5_000, viewport: { width: 1280, height: 800 } })

const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity })
rl.on('line', async (line) => {
  let r
  try {
    r = JSON.parse(line)
  } catch {
    send({ id: null, ok: false, why: 'not JSON' })
    return
  }
  switch (r.cmd) {
    case 'open': {
      const url = String(r.url ?? '')
      if (url.startsWith('crash:')) {
        send({ ev: 'closed', why: 'the fake browser went away mid-open' })
        process.exit(0)
      }
      if (url.startsWith('fail:')) {
        send({ id: r.id, ok: false, why: 'the fake page refused' })
        return
      }
      opened++
      nav = url
      send({ ev: 'nav', url: nav })
      send({ ev: 'console', type: 'error', text: 'undefined.x is not a function' })
      send({ ev: 'pageerror', text: 'Cannot read properties of undefined' })
      // The structured record a real page view sends as it goes. Kept alongside the old flat lines
      // on purpose: the hub still has to answer for a driver that only speaks the flat form, and a
      // fake that spoke only the new one would not test that.
      send({ ev: 'diag', diag: { seq: diagSeq++, kind: 'console', level: 'warning', text: 'the page warned about something' } })
      send({ ev: 'diag', diag: { seq: diagSeq++, kind: 'network', level: 'error', text: 'not found', url: nav + '/api/missing', status: 404, method: 'GET' } })
      send({ id: r.id, ok: true, url_after: nav, status: 200, title: 'Fake page' })
      return
    }
    case 'act': {
      acts++
      const what = String(r.do ?? '')
      if (what === 'die') {
        send({ ev: 'closed', why: 'the fake browser went away mid-act' })
        process.exit(0)
      }
      if (what === 'hang') return
      // A target the page does not have: refused, not guessed at. The real driver refuses
      // an ambiguous or missing match for the same reason — clicking the wrong of two
      // "Save" buttons while a person watches is the worst thing this can do.
      if (what === 'nope' || String(r.target ?? '').startsWith('Maybe')) {
        send({ id: r.id, ok: false, why: 'nothing on the page matches "Maybe"' })
        return
      }
      if (what === 'wait') {
        // A wait that reports what it actually spent, because the tool prints that number and a test
        // that never sees one cannot tell a real wait from a step that did nothing.
        const want = Math.min(Number(r.ms ?? 500), 2_000)
        const t0 = Date.now()
        await new Promise((res) => setTimeout(res, want))
        send({ id: r.id, ok: true, waited: Date.now() - t0, url_after: nav, title: 'Fake page' })
        return
      }
      if (what === 'wait_for') {
        // Look, and keep looking for the time this step was given — the contract every driver makes.
        const want = Math.max(0, Math.min(Number(r.ms ?? 600), 2_000))
        const t0 = Date.now()
        const found = String(r.target ?? '').toLowerCase() === 'more'
        while (!found && Date.now() - t0 < want) await new Promise((res) => setTimeout(res, 50))
        const looked = Date.now() - t0
        send(
          found
            ? { id: r.id, ok: true, waited: looked, url_after: nav, title: 'Fake page' }
            : {
                id: r.id,
                ok: false,
                waited: looked,
                why: `nothing on the page matches ${JSON.stringify(String(r.target ?? ''))} — looked for ${looked}ms and it never appeared`,
              },
        )
        return
      }
      if (/download/i.test(String(r.target ?? ''))) {
        // The one click whose outcome the page cannot report: the file is the app's business, so the
        // answer points at the records that can say.
        send({
          id: r.id,
          ok: true,
          note: 'this is a download link (invoice.pdf); the page stays where it is, and page_log with kind "download" says whether the app saved it',
          url_after: nav,
          title: 'Fake page',
        })
        return
      }
      if (what === 'a11y') {
        send({ id: r.id, ok: true, value: '8 element(s) a person can reach:\n[button] "More"\n[heading] "Session selftest"' })
        return
      }
      if (what === 'health') {
        send({ id: r.id, ok: true, value: 'complete at http://127.0.0.1:9/selftest-page.html — "Fake page": 2 console message(s), 1 uncaught error(s), 1 request(s) that did not load' })
        return
      }
      if (what === 'text') {
        send({ id: r.id, ok: true, value: 'Cwoffee session selftest\nclick "More" ok' })
        return
      }
      if (what === 'snapshot') {
        send({ id: r.id, ok: true, value: 'button "More"\nbutton "Settings"\ninput[search]' })
        return
      }
      if (what === 'eval') {
        send({ id: r.id, ok: true, value: '5' })
        return
      }
      if (what === 'click_xy') {
        // The point comes back so a test can grade the arithmetic the *caller* did: the panel
        // turns pixels of the picture into page pixels, and a click one scale factor off is a
        // click on something else, which no other field of the answer reveals.
        send({
          id: r.id,
          ok: true,
          at: [Number(r.x ?? -1), Number(r.y ?? -1)],
          url_after: nav,
          title: 'Fake page',
        })
        return
      }
      if (what === 'type_keys') {
        send({ id: r.id, ok: true, chars: String(r.text ?? '').length, url_after: nav, title: 'Fake page' })
        return
      }
      send({ id: r.id, ok: true, url_after: nav, title: 'Fake page' })
      return
    }
    case 'frame': {
      frames++
      // Three bytes of "picture". The point is the shape: seq, mime, bytes, and enough
      // base64 for the panel to set as an <img> src.
      send({ id: r.id, ok: true, b64: 'AAEC', mime: 'image/jpeg', bytes: 3 })
      return
    }
    case 'watch': {
      watching = true
      // The picture width the watcher asked for is echoed back: the hub's job is to carry the hint,
      // and a test that cannot see it cannot tell a sized stream from a page-width one.
      const pic = Math.max(0, Number(r.max_width) || 0)
      const width = pic > 0 ? Math.min(pic, vp.width) : vp.width
      // A frame nobody asked for, arriving on its own — the page repainted. The hub has to put it
      // where a poller will find it, with an id of its own, or the panel keeps the old picture.
      setTimeout(
        () => send({ ev: 'frame', data: 'AAEC', mime: 'image/jpeg', bytes: 3, viewport: vp, pic_width: width }),
        40,
      )
      send({ id: r.id, ok: true, live: true, picture: { width, height: Math.round((vp.height * width) / vp.width) } })
      return
    }
    case 'unwatch':
      watching = false
      send({ id: r.id, ok: true, live: false })
      return
    case 'viewport': {
      const next = {
        width: Math.min(1920, Math.max(320, Number(r.width) || vp.width)),
        height: Math.min(1440, Math.max(240, Number(r.height) || vp.height)),
      }
      const moved = next.width !== vp.width || next.height !== vp.height
      vp = next
      send({ id: r.id, ok: true, viewport: vp, moved })
      return
    }
    case 'state':
      send({
        id: r.id,
        ok: true,
        alive: true,
        url: nav,
        title: 'Fake page',
        console: [],
        page_errors: [],
        frames_taken: frames,
        live: watching,
        viewport: vp,
        opened,
        acts,
      })
      return
    case 'ping':
      send({ id: r.id, ok: true, alive: true, opened, acts, frames })
      return
    case 'close':
      send({ ev: 'closed', why: 'closed on request' })
      send({ id: r.id, ok: true })
      setTimeout(() => process.exit(0), 30)
      return
    default:
      send({ id: r.id, ok: false, why: `unknown command ${r.cmd}` })
  }
})
