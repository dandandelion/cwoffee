#!/usr/bin/env node
/*
 * search-smoke.mjs - drive the web-search settings through the gateway.
 *
 * The settings panel is a UI; this is the proof underneath it. It asks the running
 * host what search is configured, optionally switches it, runs one real search
 * through the tool (not through the model, so the result cannot be a hallucinated
 * "I searched the web"), and reports what the engine says about its own provider
 * afterwards. Then it puts the setting back.
 *
 *   node scripts/search-smoke.mjs                        # read-only
 *   node scripts/search-smoke.mjs --backend auto --probe "tokio worker threads"
 *   node scripts/search-smoke.mjs --backend searxng --url http://127.0.0.1:8080/search
 *   node scripts/search-smoke.mjs --backend auto --keep   # leave it enabled
 *
 * Why restore by default: a smoke script that silently changes which search a
 * person uses is the same class of bug as a self-test that flips their approval
 * mode. `--keep` says so on the command line instead.
 *
 * Exit codes: 0 ok, 1 the engine refused or the answer was empty, 2 could not
 * connect (wrong port, gateway not running, token missing).
 */
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

const argv = process.argv.slice(2)
const arg = (name, dflt = null) => {
  const i = argv.indexOf(`--${name}`)
  return i >= 0 ? (argv[i + 1] && !argv[i + 1].startsWith('--') ? argv[i + 1] : '') : dflt
}
const flag = (name) => argv.includes(`--${name}`)

const port = arg('port', process.env.CWOFFEE_GATEWAY_PORT || '8099')
const token =
  arg('token') ||
  process.env.CWOFFEE_GATEWAY_TOKEN ||
  (() => {
    const p = path.join(os.homedir(), '.config', 'cwoffee', 'gateway.token')
    try {
      return fs.readFileSync(p, 'utf8').trim()
    } catch {
      return ''
    }
  })()

if (!token) {
  console.error('no gateway token: set CWOFFEE_GATEWAY_TOKEN or pass --token')
  process.exit(2)
}

const wantBackend = arg('backend')
const probe = arg('probe')
const keep = flag('keep')
const url = arg('url')

const ws = new WebSocket(url || `ws://127.0.0.1:${port}/ws`)
let nextId = 1
const pending = new Map()
const rpc = (cmd, args = {}) =>
  new Promise((res, rej) => {
    const id = nextId++
    const t = setTimeout(() => {
      pending.delete(id)
      rej(new Error(`${cmd}: no reply in 60s`))
    }, 60_000)
    pending.set(id, { res, rej, t })
    ws.send(JSON.stringify({ id, cmd, args }))
  })

const timer = (label, fn) =>
  (async () => {
    const t0 = Date.now()
    const v = await fn()
    console.log(`  ${label}: ${Date.now() - t0}ms`)
    return v
  })()

const fail = (msg, code = 1) => {
  console.error(`search-smoke FAIL: ${msg}`)
  try {
    ws.close()
  } catch {}
  process.exit(code)
}

ws.onmessage = (e) => {
  let m
  try {
    m = JSON.parse(e.data)
  } catch {
    return
  }
  if (m.id && pending.has(m.id)) {
    const p = pending.get(m.id)
    pending.delete(m.id)
    clearTimeout(p.t)
    if (m.error) p.rej(new Error(m.error))
    else p.res(m.data ?? m.ok ?? true)
  }
}
ws.onclose = (e) => {
  if (pending.size) {
    for (const p of pending.values()) {
      clearTimeout(p.t)
      p.rej(new Error(`connection closed (${e.code} ${e.reason || 'no reason'})`))
    }
    pending.clear()
  }
}

// The gateway takes the drive token as its first frame; a missing one and a wrong
// one are different answers and this script should show which it got.
ws.onopen = async () => {
  try {
    ws.send(JSON.stringify({ type: 'auth', token }))
    const before = await rpc('search_settings')
    console.log(`configured: ${before.backend}  engine says: ${before.describing}`)
    if (before.backend === 'auto') {
      const a = before.auto || {}
      console.log(
        `  managed instance: runtime=${before.runtime || a.runtime || 'none'} port=${before.auto_port} up=${a.up} container=${a.container_exists}`,
      )
      if (a.note) console.log(`  status: ${a.note}`)
    }
    if (before.env_override) console.log(`  env override active: ${before.env_override}`)

    let changed = null
    if (wantBackend) {
      await rpc('set_search_backend', { backend: wantBackend, url: url || null })
      changed = before.backend
      const after = await rpc('search_settings')
      console.log(`switched: ${changed} -> ${after.backend} (${after.describing})`)
      if (after.backend !== wantBackend) fail(`engine kept ${after.backend} after being told ${wantBackend}`)
      if (after.backend !== changed) {
        const back = await rpc('search_settings')
        if (back.backend !== after.backend)
          fail(`the new backend did not stick: a fresh read says ${back.backend}`)
      }
    }

    if (probe) {
      const t0 = Date.now()
      const r = await rpc('search_probe', { query: probe })
      const ms = Date.now() - t0
      const hits = r.results || []
      console.log(`probe "${probe}": ${ms}ms, backend said: ${r.backend}, ${hits.length} hits`)
      for (const h of hits.slice(0, 3)) console.log(`  - ${h.title}  ${h.url}`)
      if (r.note) console.log(`  note: ${r.note}`)
      if (!hits.length) fail(`the probe returned no results: ${JSON.stringify(r).slice(0, 300)}`)
    }

    if (changed && !keep) {
      await rpc('set_search_backend', { backend: changed })
      const back = await rpc('search_settings')
      console.log(`restored: ${back.backend}`)
      if (back.backend !== changed) fail(`could not restore ${changed}, left on ${back.backend}`)
    } else if (changed && keep) {
      console.log(`left on ${wantBackend} (--keep)`)
    }
    console.log('search-smoke ok')
    ws.close()
    process.exit(0)
  } catch (e) {
    fail(e.message)
  }
}
ws.onerror = () => fail('cannot reach the gateway websocket', 2)
setTimeout(() => fail('timed out after 90s', 2), 90_000).unref?.()
