/**
 * Transport shim: the same UI runs in the Tauri webview and in a browser.
 *
 * The point is that `App.svelte` does not change. It calls `invoke(cmd, args)`
 * and registers a `Channel` for events; this file decides whether that goes
 * over Tauri IPC or a WebSocket to the gateway. Two front ends would otherwise
 * drift, and the desktop/browser split is exactly where that kind of duplication
 * starts paying rent forever.
 *
 * Gateway protocol (see crates/gateway/src/lib.rs):
 *   client -> { type: 'auth', token }
 *   client -> { id, cmd, args }
 *   server -> { type: 'hello' | 'auth_ok' | 'auth_fail' | 'ready' }
 *   server -> { id, ok, data?, error? }
 *   server -> { type: 'event', d: EngineEvent }
 */
import { invoke as tauriInvoke, Channel as TauriChannel } from '@tauri-apps/api/core'
import { pageLog } from './pagelog'
import type { EngineEvent } from './events'

/**
 * Our own Channel type, not Tauri's.
 *
 * Tauri's `Channel` constructor calls into `__TAURI_INTERNALS__` immediately, so
 * `new Channel()` throws in an ordinary browser tab - which is exactly what the
 * gateway build is. The real Tauri channel is created inside `invoke('attach')`
 * when there is a Tauri to talk to, and wired to this one.
 */
export class Channel<T = unknown> {
  onmessage: ((ev: T) => void) | null = null
}

const isTauri =
  typeof window !== 'undefined' &&
  ((window as any).__TAURI_INTERNALS__ !== undefined || (window as any).__TAURI__ !== undefined)

export const transportKind: 'tauri' | 'gateway' = isTauri ? 'tauri' : 'gateway'

const TOKEN_KEY = 'cwoffee.token.v1'

/**
 * The token is never put in the WebSocket URL - query strings end up in proxy
 * logs and browser history. It may arrive once via `?token=` purely to bootstrap
 * localStorage, and is stripped from the address immediately.
 */
function readToken(): string {
  try {
    const u = new URL(location.href)
    const t = u.searchParams.get('token')
    if (t) {
      localStorage.setItem(TOKEN_KEY, t)
      u.searchParams.delete('token')
      history.replaceState(null, '', u.pathname + (u.search ? u.search : '') + u.hash)
      return t
    }
    return localStorage.getItem(TOKEN_KEY) ?? ''
  } catch {
    return ''
  }
}

type Pending = { resolve: (v: unknown) => void; reject: (e: unknown) => void }

let ws: WebSocket | null = null
let role: 'drive' | 'watch' | null = null
let nextId = 1
const pending = new Map<number, Pending>()
let eventHandler: ((ev: EngineEvent) => void) | null = null
let reconnect: ReturnType<typeof setTimeout> | null = null
let backoff = 500
let wantSocket = false
// Notified after a *re*connection, so the UI can re-fetch what it had lost. A
// phone that drops wifi should come back to its session, not to a blank page.
let reconnectedCb: (() => void) | null = null
let everConnected = false
let timeouts = 0
// The in-flight handshake, shared. Without this, two concurrent callers both
// called connect(); the second saw readyState CONNECTING and resolved early, then
// `ws.send()` fired on a socket that was not open yet - the RPC was silently
// dropped, and on a slow phone link that looked like "the gateway ignores me".
let connecting: Promise<void> | null = null

function wsUrl(): string {
  return `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`
}

function failAll(msg: string) {
  for (const [, p] of pending) p.reject(new Error(msg))
  pending.clear()
}

function connect(): Promise<void> {
  if (ws && ws.readyState === WebSocket.OPEN) return Promise.resolve()
  if (connecting) return connecting
  wantSocket = true
  const p: Promise<void> = new Promise<void>((resolve, reject) => {
    let sock: WebSocket
    try {
      sock = new WebSocket(wsUrl())
    } catch (e) {
      reject(e)
      return
    }
    ws = sock

    let settled = false
    // The ceiling lives inside the executor, not around it. A Promise.race on the
    // outside left the real socket half-open and `connecting` populated with the
    // timed-out promise, so every later call reused that dead promise and failed
    // instantly without ever trying a new socket - the page sat on "connecting…"
    // and a refresh was the only way out.
    const timer = setTimeout(() => {
      if (!settled) {
        try {
          sock.close()
        } catch {
          /* already gone */
        }
        reject(new Error('gateway did not answer the handshake within 15s'))
      }
    }, 15000)
    const settle = (fn: () => void) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      connecting = null
      fn()
    }

    sock.onopen = () => {
      backoff = 500
      sock.send(JSON.stringify({ type: 'auth', token: readToken() }))
    }

    sock.onmessage = (m) => {
      let v: any
      try {
        v = JSON.parse(String(m.data))
      } catch {
        return
      }
      switch (v.type) {
        case 'auth_ok':
          role = v.role
          settle(() => {
            // Fired on every successful auth. The first boot may itself have
            // failed, so "only on reconnect" would leave a phone that connected
            // on the second attempt staring at a blank page.
            reconnectedCb?.()
            everConnected = true
            resolve()
          })
          break
        case 'auth_fail':
          // The server says which half of "auth failed" this is. A missing token
          // and a wrong token need completely different fixes, and a phone on
          // Safari cannot open devtools to find out.
          settle(() =>
            reject(new Error(v.reason ?? 'gateway rejected the token')),
          )
          failAll(v.reason ?? 'gateway rejected the token')
          break
        case 'ready':
          role = v.role
          settle(() => {
            // Fired on every successful auth. The first boot may itself have
            // failed, so "only on reconnect" would leave a phone that connected
            // on the second attempt staring at a blank page.
            reconnectedCb?.()
            everConnected = true
            resolve()
          })
          break
        case 'event':
          // Engine events are pushed, not polled. The transcript already batches
          // text by requestAnimationFrame, so nothing here needs throttling.
          try {
            eventHandler?.(v.d as EngineEvent)
          } catch (e) {
            pageLog(`gateway event handler threw: ${String(e)}`)
          }
          break
        default:
          if (typeof v.id === 'number') {
            const p = pending.get(v.id)
            if (!p) return
            pending.delete(v.id)
            if (v.ok) p.resolve(v.data)
            else p.reject(new Error(v.error ?? 'gateway error'))
          }
      }
    }

    sock.onclose = () => {
      ws = null
      role = null
      connecting = null
      failAll('gateway connection closed')
      // If the socket dies before auth_ok, the connect promise has to settle.
      // It used not to: `await connect()` then hung forever, loadModels() never
      // returned, modelErr stayed empty, and the model menu reported "this
      // endpoint does not list models" when the truth was "there is no
      // connection" - which is exactly what was reported from a phone.
      if (!settled) {
        reject(new Error('gateway closed the connection before authenticating'))
      }
      // Reconnect unless the page is going away. A phone that drops wifi should
      // pick the session back up when it returns rather than sit on a dead view.
      if (wantSocket && reconnect == null) {
        reconnect = setTimeout(() => {
          reconnect = null
          connect().catch(() => {})
        }, backoff)
        backoff = Math.min(backoff * 2, 8000)
      }
    }
    sock.onerror = () => {
      /* onclose follows and handles it */
    }
  })
  connecting = p
  // The stored promise may be awaited by nobody if the first caller already
  // gave up; swallow the orphan rejection so it does not surface as an
  // unhandled error while still propagating to real awaiters.
  p.catch(() => {})
  return p
}

export async function invoke<T = unknown>(
  cmd: string,
  args?: Record<string, unknown>,
): Promise<T> {
  // `attach` is how the caller registers its event Channel. It has to be handled
  // before the Tauri passthrough: the caller passes *our* Channel type, which has
  // no `toJSON`, so handing it straight to tauriInvoke makes Rust see a JSON
  // object where it wants a channel id - the desktop app then fails with
  // `invalid args 'onEvent' ... invalid type: map, expected a string`.
  if (cmd === 'attach') {
    const ch = args?.onEvent as Channel<EngineEvent> | undefined
    if (isTauri) {
      // Swap in a real Tauri channel that forwards into ours.
      const real = new TauriChannel<EngineEvent>((ev) => ch?.onmessage?.(ev))
      return (tauriInvoke as (c: string, a?: object) => Promise<T>)('attach', { onEvent: real })
    }
    // Over the socket the subscription is implicit; just remember the handler.
    eventHandler = (ev) => ch?.onmessage?.(ev)
    await connect()
    return undefined as T
  }

  if (isTauri) return (tauriInvoke as (c: string, a?: object) => Promise<T>)(cmd, args)

  await connect()
  if (!ws || ws.readyState !== WebSocket.OPEN) {
    throw new Error('gateway socket is not open')
  }
  return new Promise<T>((resolve, reject) => {
    const id = nextId++
    pending.set(id, { resolve: (v) => resolve(v as T), reject })
    ws!.send(JSON.stringify({ id, cmd, args: args ?? {} }))
    setTimeout(() => {
      if (pending.has(id)) {
        pending.delete(id)
        reject(new Error(`gateway did not answer ${cmd}`))
        // Two unanswered calls in a row means the socket is a lie: iOS Safari
        // suspends a backgrounded tab and the peer can vanish without the browser
        // ever reporting a close. readyState still says OPEN, so nothing else would
        // ever reconnect. Drop it so the next call starts from a fresh socket.
        timeouts += 1
        if (timeouts >= 2) {
          timeouts = 0
          try {
            ws?.close()
          } catch {
            /* already gone */
          }
          ws = null
          connecting = null
        }
      } else {
        timeouts = 0
      }
    }, 20000)
  })
}

/** Which capability the current socket has, for disabling controls in the UI. */
export function gatewayRole(): 'drive' | 'watch' | null {
  return isTauri ? 'drive' : role
}

/** Register a callback fired after the socket reconnects (not on first connect). */
export function onReconnect(cb: () => void): void {
  reconnectedCb = cb
}
