# LAN gateway

Goal: open Cwoffee from a phone on the same network to watch progress and answer
approvals when you are not at the machine.

## What is built now

`crates/gateway` is a small axum-free HTTP + WebSocket server that mirrors the
**same running `SessionHost`** as the desktop window. It is not a second engine:
a turn started from a phone shows up in the desktop transcript, and vice versa.

Start it by pointing the desktop app at a port:

```sh
CWOFFEE_GATEWAY_PORT=8099 cwoffee                     # loopback only, read+write
CWOFFEE_GATEWAY_HOST=0.0.0.0 CWOFFEE_GATEWAY_TOKEN=... cwoffee   # network-reachable
```

| env | meaning |
|---|---|
| `CWOFFEE_GATEWAY_PORT` | set at all to enable the gateway |
| `CWOFFEE_GATEWAY_HOST` | bind address, defaults to `127.0.0.1` |
| `CWOFFEE_GATEWAY_TOKEN` | the *drive* token: full control |
| `CWOFFEE_GATEWAY_WATCH_TOKEN` | separate read-only token |
| `CWOFFEE_GATEWAY_TRACE` | log every RPC command (debugging) |
| `CWOFFEE_UI_DIST` | override where `ui/dist` is read from |

Then open `http://<host>:8099/` in any browser. The frontend is the same Svelte
app; `ui/src/lib/transport.ts` picks the transport, so there is no second UI to
maintain. A `?token=` in the URL is consumed once, moved to `localStorage`, and
stripped from the address bar.

Two roles, enforced server-side per command:

* **watch** - `session_state`, `list_sessions`, `session_messages`,
  `search_messages`, `migration_notes`. Read-only.
* **drive** - everything that changes state: `start_turn`, `cancel_turn`,
  `respond_approval`, `set_mode`, `new_session`, `switch_session`, `rename_session`,
  `delete_session`, `clear_session`, `set_model`, `set_reasoning`.

Never exposed over the socket at all: `open_file`, `pick_workspace`,
`set_workspace`, `gpu_report`, and the window controls. A browser tab cannot reach
your filesystem through this.

### The browser shows the host, not a local session

There is no client-side state worth speaking of: the session list, the transcript,
the model and the workspace all come from the running desktop process. On load the
page opens the session the host is on; if the host is on a fresh draft it shows the
most recent real conversation. That is a **view**, not a `switch_session` - a
watch-only client cannot move the host's session by loading a page. The sidebar
highlights what you are looking at (`viewId`), not what the host last switched to.

The workspace chip shows the host's workspace and is read-only in the browser; the
tooltip says so and points at the desktop to change it.

### When auth fails

The server distinguishes the two cases, logs the peer without the token, and the
client renders the reason as a banner rather than only writing it to a console a
phone cannot open:

```
[cwoffee/gateway] 192.168.100.42 auth refused (no token, 0 chars sent)
no token supplied: open the URL with ?token=<CWOFFEE_GATEWAY_TOKEN>
```

A wrong token says `token does not match CWOFFEE_GATEWAY_TOKEN` instead. Before
this, both read as `no model (!)` plus a hint to edit `config.toml` - on the wrong
machine.

Verified behaviour, from the LAN address rather than from a unit test:

* no token while a drive token is configured -> `auth_fail` and the socket closes.
  It does **not** fall back to read-only. Fail closed.
* wrong token -> same.
* correct token -> `drive`, and `list_sessions` / `session_state` answer.
* a turn driven from the browser streams into the browser *and* the desktop window.

### The part that is not done

* **No TLS.** The startup line says so out loud. A bearer token over plain HTTP
  on a network you do not own is a bearer token anyone on that network holds.
  For anything beyond a trusted LAN, tunnel it (Tailscale, SSH, WireGuard) and
  bind loopback.
* **No rate limiting, no lockout, no per-role session expiry.**
* Approvals still **fail closed** on timeout, exactly as on the desktop.
* The gateway shares one `SessionHost`, so there is no multi-user story: two
  browsers drive the same agent.

## What exists today

The frontend talks to the engine through exactly two mechanisms, both Tauri
specific:

```ts
invoke('start_turn', { text })          // request/response
new Channel<EngineEvent>(onEvent)       // server -> client push
```

Everything the UI does goes through those. There is no HTTP server and no other
transport, so a browser on a phone currently has nothing to connect to.

The engine itself is already transport-agnostic: `SessionHost` is driven entirely
by channels (`HostCmd` in, `EngineEvent` + approval requests out). That is the
seam a gateway plugs into — no engine changes are required.

## Smallest useful slice: monitor mode

Build this first, because it is the actual want ("see what it is doing") and it
is the safe half:

1. `cwoffee-gateway` binary: serves `ui/dist` over HTTP and upgrades one path
   (`/ws`) to WebSocket.
2. Client sends `{ id, cmd, args }`; server calls the same engine functions the
   Tauri commands call and replies `{ id, ok, result }`.
3. Server pushes every `EngineEvent` and every `ApprovalRequest` down the socket.
4. **Monitor mode = read-only by default.** The socket may call
   `session_state`, `list_sessions`, `session_messages`, `search_messages`.
   `start_turn`, `respond_approval`, `delete_session`, `set_mode` are refused
   until the connection is explicitly promoted.

Monitor mode alone answers most of the use case and cannot cause anything.

## Security — read this before binding 0.0.0.0

Cwoffee runs commands in a sandbox and writes files in a workspace. An unauthenticated
listener on every interface turns "my laptop, my agent" into "any device on my
network can drive an agent with shell access". The sandbox is the boundary, but it
is a *containment* boundary, not an *authorization* one.

Minimum before this ships:

- **Bind explicitly.** Default to `127.0.0.1`. `0.0.0.0` must be a deliberate
  config value, and the gateway should log the bound address at startup.
- **Token auth, required, no default token.** Refuse to start if none is set when
  the bind address is not loopback. Compare in constant time. Send it as a header
  on the upgrade, not a query parameter (query strings end up in logs).
- **Promotion needs a second, separate token.** Watching and driving are different
  privileges. A leaked watch token must not be able to approve an `exec`.
- **Approvals must carry their full context to the phone.** The phone is where the
  decision gets made away from the desk, so `ApprovalRequest.summary` has to be
  enough to decide from — full command, cwd, and which tool. Truncating it makes
  the remote approval meaningless.
- **Approval timeout still applies.** A turn waiting on a phone that has run out
  of battery must fail closed, exactly as it does when the desktop UI is gone.
- **Cleartext HTTP on a LAN is a real exposure** — the transcript, tool output and
  any secrets in them cross the network in the clear. Either terminate TLS
  (self-signed plus the phone trusting it is a support burden worth naming), or
  state plainly in the UI that the connection is unencrypted.

## Things that will bite

- **CSP.** The Tauri CSP allows `ipc:` and `http://ipc.localhost`. A browser build
  needs `connect-src` to allow the gateway origin over `ws:`/`wss:`.
- **`@tauri-apps/api` must not be imported at module scope** in a browser build.
  `pagelog.ts` already imports it lazily for exactly this reason; the same care
  applies to the transport.
- **Frameless window chrome is meaningless in a browser.** The titlebar and the
  min/max/close buttons need to disappear when not running under Tauri.
- **Viewport height on mobile browsers** is not stable — the URL bar resizing
  changes `100%`. `dvh` is the fix; `#app { height: 100% }` alone will misbehave.
- **Reconnect.** A phone sleeping drops the socket. The gateway needs to buffer or
  replay recent events, otherwise a reconnect loses the middle of a turn.
- **One host, many viewers.** `SessionHost::take_receivers()` hands the event
  stream to exactly one consumer. A gateway with a phone and a desktop open needs
  a fan-out broadcast, not a second `take`.

## Layout status

The frontend is already responsive: below 768px the sidebar is an overlay drawer
with a titlebar toggle and scrim, the telemetry bar drops spec/ttft/jank, and the
composer uses a 16px font so iOS does not zoom on focus. Verified at 390x844.
