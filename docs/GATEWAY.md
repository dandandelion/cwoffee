# LAN gateway (not built yet)

Goal: open Cowry from a phone on the same network to watch progress and answer
approvals when you are not at the machine.

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

1. `cowry-gateway` binary: serves `ui/dist` over HTTP and upgrades one path
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

Cowry runs commands in a sandbox and writes files in a workspace. An unauthenticated
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
