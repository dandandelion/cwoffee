// The two Playwright drivers, driven the way the app drives them, against real Chromium.
//
// Why this exists next to `browser-bridge-probe.mjs`: cwoffee has three browsers a model can be
// talking to — the app's own web view (graded by the bridge probe), the kept browser the sidebar
// panel shows (`browser/session.mjs`), and the one-shot walk (`browser/driver.mjs`). The tool's
// schema offers one verb list and one target language to all three. Every time one of them learns a
// verb late — `wait_for` that checks once instead of polling, `a11y` that answers `ok` and goes
// silent, a `wait` that is a no-op — the model is left holding a difference between three browsers
// it was told were the same. This script speaks the ndjson both halves really use and grades the
// answers, so the dialects are only allowed to stay the same on purpose.
//
//   node scripts/browser-session-probe.mjs [port]
//
// Needs `browser/node_modules` (`npm --prefix browser install`) and a Chromium build for Playwright.

import { spawn } from 'node:child_process'
import fs from 'node:fs'
import http from 'node:http'
import path from 'node:path'
import os from 'node:os'

const PORT = Number(process.argv[2] ?? 8931)
const ROOT = path.resolve(path.dirname(new URL(import.meta.url).pathname), '..')

let bad = 0
const check = (ok, msg) => {
  console.log(`${ok ? 'ok' : 'FAIL'} — ${msg}`)
  if (!ok) bad++
}

// A page that does something on its own schedule. Every wait in here is a wait for something the page
// has not finished doing, which is the only kind of wait that tells you whether a driver really waits.
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'cwoffee-session-probe-'))
fs.writeFileSync(
  path.join(dir, 'late.html'),
  `<!doctype html><html><head><title>Late page</title></head><body>
   <h1>Late</h1><button id="go">Start</button>
   <label for="pick">Roast</label><select id="pick"><option value="l">Light</option><option value="m">Medium</option></select>
   <script>
     console.log("late: log at load");
     setTimeout(() => { const b = document.createElement('button'); b.id = 'late'; b.textContent = 'Late button'; document.body.appendChild(b) }, 1200);
     fetch("/gone").catch(() => {});
   </script></body></html>`,
)
fs.writeFileSync(path.join(dir, 'second.html'), '<!doctype html><title>Second</title><h1>Second page</h1>')
// The same page for the one-shot walk, without the request that goes nowhere. A connection the server
// pulls out from under Chromium leaves a socket Chromium wants to retry, and the walk's browser then
// has a slow time of it closing — which is a fact about this fixture, not about the walk.
fs.writeFileSync(
  path.join(dir, 'walk.html'),
  `<!doctype html><html><head><title>Walk page</title></head><body>
   <h1>Walk</h1><button id="go">Start</button>
   <label for="pick">Roast</label><select id="pick"><option value="l">Light</option><option value="m">Medium</option></select>
   <script>setTimeout(() => { const b = document.createElement('button'); b.id = 'late'; b.textContent = 'Late button'; document.body.appendChild(b) }, 1200);</script></body></html>`,
)

const srv = http.createServer((q, res) => {
  // A request the server pulls the rug out from under, so the page has a failed request of its own
  // to be counted for. A 404 would not do: that is a response, not a failure, and Playwright is right
  // to call it one.
  if (q.url === '/gone') {
    res.destroy()
    return
  }
  let f = null
  try {
    f = fs.readFileSync(path.join(dir, q.url.replace(/^\//, '')))
  } catch {}
  if (!f) {
    res.writeHead(404)
    res.end('nothing here')
    return
  }
  res.writeHead(200, { 'content-type': 'text/html' })
  res.end(f)
})
await new Promise((res) => srv.listen(PORT, '127.0.0.1', res))
const base = `http://127.0.0.1:${PORT}`

/* ------------------------------------------------------------------ the kept browser ---- */
const proc = spawn('node', ['session.mjs'], { cwd: path.join(ROOT, 'browser'), stdio: ['pipe', 'pipe', 'pipe'] })
const lines = []
proc.stdout.on('data', (d) => {
  for (const l of d.toString().split('\n')) {
    if (!l.trim()) continue
    try {
      lines.push(JSON.parse(l))
    } catch {}
  }
})
proc.stderr.on('data', (d) => process.stderr.write(`[session] ${d}`))

let seq = 0
const ask = async (o, ms = 20_000) => {
  const want = ++seq
  proc.stdin.write(JSON.stringify({ ...o, id: want }) + '\n')
  const t0 = Date.now()
  while (Date.now() - t0 < ms) {
    const hit = lines.find((l) => l.id === want)
    if (hit) return hit
    await new Promise((r) => setTimeout(r, 25))
  }
  return { ok: false, why: `the session browser never answered ${JSON.stringify(o)}` }
}
await new Promise((r) => setTimeout(r, 1500))

let r = await ask({ cmd: 'open', url: `${base}/late.html` })
check(r.ok && String(r.url_after).endsWith('late.html'), `the kept browser opens a page: ${JSON.stringify(r).slice(0, 120)}`)

// The complaint this whole file was written for: a wait that returns in three milliseconds.
r = await ask({ cmd: 'act', do: 'wait_for', target: 'Late button', ms: 4000 })
check(r.ok && (r.waited ?? 0) >= 1100, `wait_for looks until the page produces the thing (waited ${r.waited}ms)`)
r = await ask({ cmd: 'act', do: 'wait_for', target: 'A thing that never arrives', ms: 800 })
check(!r.ok && /looked for \d+ms and it never appeared/.test(r.why ?? ''), `a wait_for that gives up says for how long: ${JSON.stringify(r.why)}`)
r = await ask({ cmd: 'act', do: 'wait', ms: 300 })
check(r.ok && (r.waited ?? 0) >= 280, `wait reports the time it spent, measured (${r.waited}ms)`)

r = await ask({ cmd: 'act', do: 'navigate', url: `${base}/second.html` })
check(r.ok && String(r.url_after).endsWith('second.html'), `navigate moves this same browser to another page: ${JSON.stringify(r).slice(0, 120)}`)
r = await ask({ cmd: 'act', do: 'back' })
check(r.ok && String(r.url_after).endsWith('late.html'), `and back comes home without timing out`)

r = await ask({ cmd: 'act', do: 'text' })
check(r.ok && /Start/.test(String(r.value ?? '')), 'text reads the page as words')
r = await ask({ cmd: 'act', do: 'a11y', max: 5 })
check(r.ok && /button #go "Start"/.test(String(r.value ?? '')) && (r.total ?? 0) >= 2, `a11y returns text (${r.total} elements there)`)
r = await ask({ cmd: 'act', do: 'snapshot' })
check(r.ok && /button/.test(String(r.value ?? '')), 'snapshot names the controls')
// **A `<select>` is chosen from, not typed into.** The complaint was a fill that answered with an
// engine's internals ("the HTMLInputElement.value setter can only be used on instances of...") in
// place of a sentence about the page.
r = await ask({ cmd: 'act', do: 'fill', target: 'Roast', text: 'Medium' })
check(r.ok && /Medium/.test(String(r.chose ?? '')), `fill picks the option a person reads: ${JSON.stringify(r.chose ?? r.why)}`)
r = await ask({ cmd: 'act', do: 'fill', target: 'Roast', text: 'Burnt' })
check(!r.ok && /offers Light, Medium/.test(String(r.why ?? '')), `and a name that is not on the list says what the list offers: ${JSON.stringify(r.why)}`)
r = await ask({ cmd: 'act', do: 'eval', code: "console.log('from the probe'); 1+1" })
check(r.ok && String(r.value) === '2', 'eval answers its completion value')
r = await ask({ cmd: 'act', do: 'health' })
check(
  r.ok && /[1-9]\d* console message\(s\)/.test(String(r.value ?? '')) && /1 request\(s\) that did not load/.test(String(r.value ?? '')),
  `health reads as a sentence about this page: ${JSON.stringify(r.value).slice(0, 150)}`,
)
// The records belong to the document that made them: a page that has been replaced cannot still be
// contributing numbers to what is on screen now.
r = await ask({ cmd: 'act', do: 'navigate', url: `${base}/second.html` })
r = await ask({ cmd: 'act', do: 'health' })
check(
  r.ok && /0 console message\(s\)/.test(String(r.value ?? '')),
  `and a new document starts its own count: ${JSON.stringify(r.value).slice(0, 150)}`,
)

await ask({ cmd: 'act', do: 'nope' })
proc.stdin.write(JSON.stringify({ cmd: 'close' }) + '\n')
await new Promise((res) => setTimeout(res, 300))
proc.kill()

/* -------------------------------------------------------------- the one-shot walk ---- */
// The request goes in through a file, not through `input:`. The driver reads its request with
// `readFileSync(0)`, and a pipe that Node itself is holding never reports EOF to that read — a fact
// about how this script spawns, not about the driver, which the app feeds with a plain redirect.
const reqPath = path.join(dir, 'walk-request.json')
fs.writeFileSync(
  reqPath,
  JSON.stringify({
    url: `${base}/walk.html`,
    timeout_ms: 40_000,
    steps: [
      { do: 'wait_for', target: 'Late button', ms: 4000 },
      { do: 'wait_for', target: 'Never arrives', ms: 700 },
      { do: 'navigate', url: `${base}/second.html` },
      { do: 'text' },
      { do: 'snapshot' },
      { do: 'navigate', url: `${base}/walk.html` },
      { do: 'fill', target: 'Roast', text: 'Medium' },
      { do: 'fill', target: 'Roast', text: 'Burnt' },
      { do: 'a11y' },
      { do: 'health' },
      { do: 'reload' },
      { do: 'back' },
    ],
  }),
)
const one = spawn('node', ['driver.mjs'], {
  cwd: path.join(ROOT, 'browser'),
  stdio: [fs.openSync(reqPath, 'r'), 'pipe', 'pipe'],
})
one.stderr.on('data', (d) => process.stderr.write(`[driver] ${d}`))
const raw = await new Promise((res) => {
  let s = ''
  one.stdout.on('data', (d) => (s += d))
  one.on('close', () => res(s))
})
const j = JSON.parse(raw || '{}')
const steps = j.steps ?? []
check(steps[0]?.status === 'ok' && (steps[0]?.waited ?? 0) >= 1100, `the one-shot walk polls for a thing that appears (waited ${steps[0]?.waited}ms)`)
check(/looked for \d+ms and it never appeared/.test(steps[1]?.status ?? ''), `the one-shot walk says how long it looked: ${steps[1]?.status}`)
check(steps[2]?.status === 'ok', `the one-shot walk can move to another page mid-walk: ${steps[2]?.status}`)
check(String(j.text ?? '').includes('Second page'), 'and the report reads the page the walk ended on, not the one it started on')
check(String(j.url_after ?? '').endsWith('second.html'), `a walk that went backwards says where it ended up: ${j.url_after}`)
check(steps[3]?.status === 'ok' && /Second page/.test(String(steps[3]?.value ?? '')), `a text step reads the page in the middle of a walk: ${JSON.stringify(steps[3]?.value).slice(0, 60)}`)
check(steps[4]?.status === 'ok' && /h1/.test(String(steps[4]?.value ?? '')), `and a snapshot step names controls: ${JSON.stringify(steps[4]?.value).slice(0, 60)}`)
check(/chose "Medium"/.test(steps[6]?.status ?? ''), `a walk chooses a <select> option rather than typing into it: ${steps[6]?.status}`)
check(/not a choice on this list; it offers Light, Medium/.test(steps[7]?.status ?? ''), `and says what the list offers when the name is wrong: ${steps[7]?.status}`)
check(steps[8]?.status === 'ok' && /button|link|textbox/.test(String(steps[8]?.value ?? '')), `a walk answers a11y in the same readable shape the other browsers give: ${JSON.stringify(steps[8]?.value).slice(0, 60)}`)
check(steps[9]?.status === 'ok' && /console message\(s\)/.test(String(steps[9]?.value ?? '')), `and health as one sentence about this page: ${JSON.stringify(steps[9]?.value).slice(0, 70)}`)
check(steps[11]?.status === 'ok' && /second\.html/.test(steps[11]?.note ?? ''), `a walk has a history of its own to go back through: ${steps[11]?.note}`)

console.log(bad ? `browser-session-probe: ${bad} failure(s)` : `browser-session-probe: ok (${seq + steps.length} claims)`)
srv.close()
fs.rmSync(dir, { recursive: true, force: true })
process.exit(bad ? 1 : 0)
