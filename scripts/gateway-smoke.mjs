#!/usr/bin/env node
// End-to-end check of the promises the gateway makes to a browser. No dependency
// beyond Node's own WebSocket, so it runs over a phone tether as easily as on a
// dev machine, and a phone has no devtools to fall back on.
//
// What it pins is what a phone actually depends on:
//
//   * auth says *why* it failed. "no token supplied" and "token does not match"
//     are different answers, and a rejected socket is never told the session is
//     ready - otherwise the browser sits on an empty screen inventing a cause.
//   * one `bootstrap` call brings state, sessions, models, workspaces and the
//     transcript. On the measured phone path each round trip costs 200-400ms, so
//     the number of calls, not the work, is what feels like load time.
//   * `set_workspace` answers with a reason when it cannot honour the request,
//     rather than a struct shaped almost right.
//
// Nothing here mutates the host: the two set_workspace calls are the ones the
// gateway is supposed to refuse.
//
//   node scripts/gateway-smoke.mjs [--url ws://127.0.0.1:8099/ws] [--token T]
//                                  [--token-file ~/.config/cwoffee/gateway.token]

import { readFileSync } from 'node:fs'
import { homedir } from 'node:os'

const argv = process.argv.slice(2)
const arg = (name, dflt) => {
  const i = argv.indexOf(`--${name}`)
  return i >= 0 ? argv[i + 1] : dflt
}
const url = arg('url', process.env.CWOFFEE_GATEWAY_URL ?? 'ws://127.0.0.1:8099/ws')
const tokenFile = arg('token-file', `${homedir()}/.config/cwoffee/gateway.token`)
const token =
  arg('token', process.env.CWOFFEE_GATEWAY_TOKEN ?? '') || readFileSync(tokenFile, 'utf8').trim()
const CMD_WAIT = Number(arg('wait', 10000))

let fails = 0
const check = (cond, msg) => {
  if (cond) console.log(`  ok   ${msg}`)
  else {
    fails++
    console.log(`  FAIL ${msg}`)
  }
}

const open = () =>
  new Promise((resolve, reject) => {
    const ws = new WebSocket(url)
    const timer = setTimeout(() => reject(new Error(`no answer from ${url} in 15s`)), 15000)
    ws.onopen = () => {
      clearTimeout(timer)
      resolve(ws)
    }
    ws.onerror = (e) => {
      clearTimeout(timer)
      reject(new Error(`cannot reach ${url}: ${e.message ?? 'websocket error'}`))
    }
  })

/** Send one frame, return the first frame whose `type` matches (or null). */
function expect(ws, type, send) {
  return new Promise((resolve) => {
    const at = setTimeout(() => {
      ws.removeEventListener('message', seen)
      resolve(null)
    }, CMD_WAIT)
    const seen = (m) => {
      const f = JSON.parse(m.data)
      if (f.type === type) {
        clearTimeout(at)
        ws.removeEventListener('message', seen)
        resolve(f)
      }
    }
    ws.addEventListener('message', seen)
    if (send) ws.send(JSON.stringify(send))
  })
}

/** One RPC: send {id, cmd, args}, resolve the reply that carries this id. */
function rpc(ws, id, cmd, args = {}) {
  return new Promise((resolve) => {
    const at = setTimeout(
      () => resolve({ id, ok: false, error: `gateway did not answer ${cmd} within ${CMD_WAIT}ms` }),
      CMD_WAIT,
    )
    const seen = (m) => {
      const f = JSON.parse(m.data)
      if (f.id === id) {
        clearTimeout(at)
        ws.removeEventListener('message', seen)
        resolve(f)
      }
    }
    ws.addEventListener('message', seen)
    ws.send(JSON.stringify({ id, cmd, args }))
  })
}

console.log(`gateway smoke: ${url}`)
try {
  // 1. a wrong token is refused, and the refusal names the mismatch
  {
    const ws = await open()
    const f = await expect(ws, 'auth_fail', { type: 'auth', token: 'definitely-not-the-token' })
    check(!!f, 'a wrong token is refused with auth_fail')
    check(/does not match/i.test(f?.reason ?? ''), `the refusal names the mismatch: ${JSON.stringify(f?.reason)}`)
    check((await expect(ws, 'ready')) === null, 'a rejected socket is never told the session is ready')
    ws.close()
  }

  // 2. no token at all is a different answer, so the client knows what to fix.
  // This is the browser with no ?token= in the URL: the frame goes out with an
  // empty token, which is a different failure from a mistyped one.
  {
    const ws = await open()
    const f = await expect(ws, 'auth_fail', { type: 'auth', token: '' })
    check(/no token supplied/i.test(f?.reason ?? ''),
      `an empty token is told what to send: ${JSON.stringify(f?.reason ?? 'nothing')}`)
    ws.close()
  }

  // 3. one round trip brings the whole first screen
  const ws = await open()
  const ok = await expect(ws, 'auth_ok', { type: 'auth', token })
  check(!!ok, 'the real token is accepted')
  const boot = await rpc(ws, 1, 'bootstrap')
  check(boot.ok === true, `bootstrap answered ok${boot.ok ? '' : `: ${boot.error}`}`)
  const d = boot.data ?? {}
  check(Array.isArray(d.sessions) && d.sessions.length > 0, `bootstrap carried ${d.sessions?.length ?? 0} session(s)`)
  check(Array.isArray(d.models) && d.models.length >= 1, `bootstrap carried ${d.models?.length ?? 0} model(s)`)
  check(Array.isArray(d.workspaces) && d.workspaces.length >= 1,
    `bootstrap carried the folder list: ${d.workspaces?.map((w) => w.path ?? w).join(', ')}`)
  check(typeof d.state?.workspace === 'string' && d.state.workspace.length > 0,
    `bootstrap named the host folder: ${d.state?.workspace}`)
  check(d.session_id != null, `bootstrap named the session it is showing: ${d.session_id}`)
  check(Array.isArray(d.page?.messages), `bootstrap carried ${d.page?.messages?.length ?? 0} transcript row(s)`)
  const seenModel = (d.models ?? [])[0]
  check(!!seenModel?.id, `the endpoint named its model: ${seenModel?.id ?? '(none)'}`)

  // 4. a workspace request that cannot be honoured comes back as a reason
  {
    const tmp = await rpc(ws, 2, 'set_workspace', { path: '/tmp' })
    check(tmp.ok === false, `set_workspace('/tmp') is refused rather than accepted: ${JSON.stringify(tmp.data ?? tmp.error)}`)
    check(/temp folder/i.test(tmp.error ?? ''), `the refusal explains itself: ${tmp.error}`)
    const etc = await rpc(ws, 3, 'set_workspace', { path: '/etc' })
    check(etc.ok === false, `set_workspace('/etc') is refused: ${etc.error ?? JSON.stringify(etc.data)}`)
    const after = await rpc(ws, 4, 'session_state')
    check(after.ok && after.data?.workspace === d.state?.workspace,
      `the host is still in the folder it started in: ${after.data?.workspace}`)
  }
  ws.close()
} catch (e) {
  fails++
  console.log(`  FAIL ${e?.message ?? e}`)
}

console.log(fails === 0 ? '\nGATEWAY SMOKE OK' : `\nGATEWAY SMOKE FAILED (${fails})`)
process.exit(fails === 0 ? 0 : 1)
