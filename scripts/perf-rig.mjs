#!/usr/bin/env node
/**
 * perf-rig — measure what a long transcript costs to scroll, in the app's own engine.
 *
 * The report this exists for: "when text accumulates, scrolling slows down by A LOT" in
 * the desktop window, while the same page in a browser tab is fine. A claim like that is
 * worth exactly what the measurement is worth, so:
 *
 *   1. ui/perf/ builds the real transcript renderer + the real stylesheet into a scratch
 *      bundle that is NOT part of ui/dist (the app never ships the probe),
 *   2. scripts/webview-probe.py loads it in WebKitGTK 4.1 — the same library
 *      `ldd target/release/cwoffee-app` names — in a visible window,
 *   3. the page posts back JSON: DOM shape, per-frame scroll cost, expand cost.
 *
 * It touches no endpoint, no session store and no config: the conversation is synthetic,
 * built through the same `loadHistory` call a session switch makes. So a run costs no
 * prefill and can be run as often as the numbers warrant.
 *
 * Usage:
 *   scripts/perf-rig.mjs                         # 40 turns of a model that thinks a lot
 *   scripts/perf-rig.mjs --turns 12 --reason 2000
 *   scripts/perf-rig.mjs --ab                    # same page, deferral on and off
 *   scripts/perf-rig.mjs --keep                  # leave the scratch bundle behind
 */
import { spawn, spawnSync } from 'node:child_process'
import http from 'node:http'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const argv = process.argv.slice(2)
const flags = { ab: false, keep: false, nocollapse: false }
const params = { turns: 40, reason: 8000, tools: 3, out: 5000, ans: 1200, mode: 'live', cpframe: 240 }
let port = 8191
for (let i = 0; i < argv.length; i++) {
  const a = argv[i]
  if (a === '--ab') flags.ab = true
  else if (a === '--nocollapse') flags.nocollapse = true
  else if (a === '--collapse') params.collapse = argv[++i]
  else if (a === '--mode') params.mode = argv[++i]
  else if (a === '--keep') flags.keep = true
  else if (a === '-h' || a === '--help') {
    console.log(
      [
        'usage: scripts/perf-rig.mjs [--turns N] [--reason N] [--tools N] [--out N] [--ans N]',
        '                            [--mode live|history] [--cpframe N]',
        '                            [--ab | --nocollapse] [--port N] [--keep]',
        '',
        '  --ab          run the same page twice: deferral on, then off (the negative control)',
        '  --nocollapse  run the same page twice with the per-word reveal spans left in',
        '                place after a turn settles — the control for the scroll fix',
        '  --port        which loopback port serves the scratch bundle (default 8191)',
      ].join('\n'),
    )
    process.exit(0)
  } else if (a.startsWith('--port')) port = Number(a.split('=')[1] ?? argv[++i])
  else if (a.startsWith('--')) params[a.slice(2)] = Number(a.includes('=') ? a.split('=')[1] : argv[++i])
}

const out = fs.mkdtempSync(path.join(os.tmpdir(), 'cwoffee-perf-'))
const log = (m) => process.stderr.write(`[perf-rig] ${m}\n`)

function build() {
  log('building ui/perf into a scratch bundle (the app bundle is not touched)')
  const r = spawnSync('npx', ['vite', 'build', '--config', 'perf/vite.config.ts', '--outDir', out, '--emptyOutDir'], {
    cwd: path.join(ROOT, 'ui'),
    encoding: 'utf8',
  })
  if (r.status !== 0) {
    log(`the perf bundle failed to build:\n${r.stdout}\n${r.stderr}`)
    process.exit(2)
  }
}
build()
if (!fs.existsSync(path.join(out, 'probe.html'))) {
  log(`the build produced no probe.html in ${out}`)
  process.exit(2)
}

const TYPES = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml' }
const server = http.createServer((req, res) => {
  const rel = decodeURIComponent((req.url || '/').split('?')[0])
  const f = path.join(out, rel === '/' ? 'probe.html' : rel)
  log(`req ${req.url} -> ${f}`)
  if (!f.startsWith(out) || !fs.existsSync(f) || fs.statSync(f).isDirectory()) {
    res.writeHead(404).end('not found')
    return
  }
  res.writeHead(200, { 'content-type': TYPES[path.extname(f)] ?? 'application/octet-stream' })
  res.end(fs.readFileSync(f))
})
server.on('error', (e) => {
  // A stack trace here reads like a bug in the rig. What it usually is: a previous run
  // that got interrupted and left its scratch server on the port.
  if (e.code === 'EADDRINUSE') {
    log(`port ${port} is taken by something else. Check with: ss -ltnp "sport = :${port}" — and end that one process by its pid, do not pattern-kill by name.`)
    log('Or pick another port: scripts/perf-rig.mjs --port 8291')
    process.exit(2)
  }
  log(`the scratch static server failed: ${e.message}`)
  process.exit(2)
})
await new Promise((r) => server.listen(port, '127.0.0.1', r))

async function run(extraQuery, label) {
  const qs = new URLSearchParams(Object.entries(params).map(([k, v]) => [k, String(v)]))
  for (const [k, v] of Object.entries(extraQuery)) qs.set(k, String(v))
  const url = `http://127.0.0.1:${port}/probe.html?${qs}`
  log(`${label}: ${url}`)
  // `spawn`, never `spawnSync`: the page is served by THIS process, and a synchronous
  // spawn blocks the event loop, so the webview's requests sit in the socket backlog
  // while the rig waits for a report that can never arrive. That is how one run sat
  // out its whole timeout without a single request reaching the server.
  const p = await new Promise((res) => {
    const child = spawn('python3', [path.join(ROOT, 'scripts', 'webview-probe.py'), url, '150'])
    let stdout = ''
    let stderr = ''
    child.stdout.on('data', (d) => {
      stdout += d
      for (const l of String(d).split('\n')) if (l.trim() && !l.trim().startsWith('{')) process.stderr.write(`  ${l}\n`)
    })
    child.stderr.on('data', (d) => {
      stderr += d
      for (const l of String(d).split('\n'))
        if (l.trim() && !l.includes('CONSOLE LOG')) process.stderr.write(`  ${l}\n`)
    })
    child.on('close', (code) => res({ stdout, stderr, code }))
  })
  const outLines = `${p.stdout ?? ''}${p.stderr ?? ''}`.trim()
  const line = (p.stdout ?? '').split('\n').map((s) => s.trim()).filter(Boolean).pop() ?? ''
  let report = null
  try {
    report = JSON.parse(line)
  } catch {
    /* not JSON: a throw, a timeout or a load failure — the text is the report */
  }
  if (!report) {
    log(`${label}: no report. The rig said:\n${outLines}`)
    return null
  }
  console.log(`\n=== ${label} ===`)
  console.log(`engine        ${report.engine}`)
  console.log(`shape         ${JSON.stringify(report.params)}`)
  console.log(`mode          ${report.mode} (${report.mode === 'live' ? 'streamed through transcript.apply' : 'painted from the store'})`)
  console.log(`paint         ${report.paintMs} ms of synthetic conversation`)
  console.log(`DOM before    ${report.before.nodes} nodes (${report.before.tinSpans} reveal spans), ${report.before.visibleText} visible chars`)
  console.log(`held text     reasoning ${report.before.reasoningBodies} blocks / ${report.before.reasoningChars} chars; ` +
    `tool bodies ${report.before.toolBodies} blocks / ${report.before.toolChars} chars`)
  console.log(`content       ${report.before.scrollHeight} px of transcript`)
  console.log(`${report.frames}`)
  console.log(`${report.scroll}`)
  console.log(`expand run    ${report.expand ? `${report.expand.openMs.toFixed(1)} ms to reveal ${report.expand.revealedChars} chars` : 'no runs'}`)
  console.log(`DOM after     ${report.after.nodes} nodes`)
  return report
}

const control = flags.nocollapse ? { collapse: 'off' } : { nodefer: 1 }
const controlName = flags.nocollapse ? 'reveal-span collapse' : 'deferral'
const twice = flags.ab || flags.nocollapse
const a = await run({}, twice ? 'deferral/collapse ON' : 'as built')
const b = twice ? await run(control, `${controlName} OFF (negative control)`) : null
server.close()
if (!flags.keep) fs.rmSync(out, { recursive: true, force: true })
else log(`scratch bundle kept at ${out}`)

if (a && b) {
  const dNode = (b.after?.nodes ?? 0) - (a.after?.nodes ?? 0)
  const dSpan = (b.after?.tinSpans ?? 0) - (a.after?.tinSpans ?? 0)
  console.log('\n=== verdict ===')
  console.log(`worst frame gap   on ${a.worstGap.toFixed(0)}ms  off ${b.worstGap.toFixed(0)}ms  (noise floor is ~2ms; do not read more into it than that)`)
  console.log(`DOM nodes         on ${a.after.nodes}  off ${b.after.nodes}  (${dNode})`)
  console.log(`reveal spans      on ${a.after.tinSpans}  off ${b.after.tinSpans}  (${dSpan})`)
  console.log(
    a.worstGap > 0 && a.worstGap - b.worstGap <= 2 && dNode <= 0
      ? `${controlName}: text and geometry identical, DOM smaller, nothing measured slower. ` +
        `That is a claim about cost held, not about smoothness felt — say which one you measured.`
      : `${controlName} did NOT hold its claim: check the numbers above before believing the fix`,
  )
}
process.exit(a ? 0 : 1)
