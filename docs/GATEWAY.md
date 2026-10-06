# LAN gateway

Goal: open Cwoffee from a phone on the same network to watch progress and answer
approvals when you are not at the machine.

## Which conversation a turn belongs to

`start_turn` takes an optional `session` id, and clients are expected to send it:

```json
{ "id": 7, "cmd": "start_turn", "args": { "text": "…", "session": 512 } }
```

One host serves several clients - the desktop, a phone tab, a harness. A turn with no
address runs in *whichever conversation the last switch left active*, which from the
other person's chair is indistinguishable from the transcript editing itself. So the
number is an assertion, and a mismatch is refused:

```
the turn asked for session 512, but the host is on session 508: nothing was sent.
Open that conversation, or start a new one, and try again.
```

That arrives as an `error` **event**, not an rpc error: the command was well formed and
was understood; the engine declined to run it. Clients must render it - the desktop
shows it as a toast.

`session: 0` means "my view is a draft", and the host opens a new conversation instead
of appending to whatever it had open.

A client whose view and the host disagree fixes itself before sending: the desktop moves
the host to the conversation it is displaying and logs `send: host is on X, moving it to
the viewed session Y`. That is not a reroute - the message was always meant for what the
user is looking at - and the engine's refusal remains for a turn addressed to a session
nobody is displaying.

Related, and the other half of the same complaint: switching sessions during a running
turn is handled by the *view*. The host's command loop is inside the turn, so a
`switch_session` issued while it thinks queues behind it; the desktop fires it and
moves its own view immediately rather than awaiting it, and the sidebar shows a
spinner on the session that is working.

What is built now

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
  `search_messages`, `migration_notes`, `list_workspaces`. Read-only.
* **drive** - everything that changes state: `start_turn`, `cancel_turn`,
  `respond_approval`, `respond_question`, `set_mode`, `set_reasoning`, `new_session`,
  `switch_session`, `rename_session`, `delete_session`, `clear_session`, `set_model`,
  `set_workspace`, `add_workspace`, `remove_workspace`, `set_workspace_label`.

`new_session` takes an optional `workspace`, which the engine validates before the
session is created; a browser can therefore start a session inside a named folder
rather than inheriting wherever the host happened to be pointing. `add_workspace`
registers a folder for the sidebar (validated the same way) and `remove_workspace`
drops that registration only - it never deletes sessions, and a folder that still
contains sessions keeps its row in the sidebar because hiding a session is worse
than showing a folder twice.

Two workspace refusals are deliberate, and a client gets the reason rather than a
generic error:

- **A session keeps the folder it started in.** `set_workspace` for a session that
  already has a real folder returns `this session works in <path>; a session keeps
  its folder, so start a new session to work somewhere else`. Letting a phone move a
  running session would make the sidebar grouping decoration: the row would name the
  old folder while `write_file` wrote into the new one. A session with no folder yet -
  or one stored in the temp root - can still be given one.
- **The temp root is not a workspace.** `/tmp`, `/var/tmp` and `/private/tmp` are where
  sessions live before anyone chooses a folder, so they are refused by `set_workspace`
  and `add_workspace`; a folder *inside* them (`/tmp/my-project`) is a normal folder.

### Answering an approval from a phone

`respond_approval` takes `{ id, answer }`, and `answer` is the engine enum serialized
by serde, not a string the gateway interprets by hand: `allow`, `allowSession`, `deny`
(`allow_session` is accepted for older clients). Any other spelling is an error naming
the three that work. An earlier gateway parsed these with a hand-written match that
listed `allow_session` while the UI sent `allowSession`, so every approval answered
from a phone except a plain "allow" came back as `unknown answer` and the parked tool
sat until it timed out - the tool appeared to ignore the user. The enum is the
authority on its own wire form; there is a Rust test pinning the three spellings and a
test that drives all three through a fake client.

Every drive command is logged server-side with the peer that asked and the shape of
its arguments - `172.20.10.2:51044 set_workspace (path len 24)`,
`start_turn (attachments unset text=len 142)` - and never the content. This exists
because the socket can run a shell on this machine, and "the agent ran a command I
did not ask for" has to be answerable from the log.

Never exposed over the socket at all: `open_file`, `pick_workspace`, `gpu_report`,
and the window controls. A browser tab cannot open a file or a dialog on your
desktop through this. `set_workspace` *is* exposed, drive-only, and is the widest
command in the list - see "What a drive token can reach" below.

### The browser shows the host, not a local session

There is no client-side state worth speaking of: the session list, the transcript,
the model and the workspace all come from the running desktop process. On load the
page opens the session the host is on; if the host is on a fresh draft it shows the
most recent real conversation. That is a **view**, not a `switch_session` - a
watch-only client cannot move the host's session by loading a page. The sidebar
highlights what you are looking at (`viewId`), not what the host last switched to.

The workspace chip shows the host's workspace and is read-only in the browser; the
tooltip says so and points at the desktop to change it.

### Model list empty on a phone

`This endpoint does not list models. Using —` in the model menu does not mean the
endpoint has no `/models`. It meant the socket never authenticated: `connect()`
left its promise unsettled when the socket closed before `auth_ok`, so the model
fetch never returned and the menu fell through to its "server said no" branch.
The menu now checks connection state first and says **Not connected to the host**
with the actual reason, and the banner's `retry` re-fetches state, sessions and
the model list without a page reload.

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

### Why the first load took a while

Reported from a phone: the page paints quickly, but the session list and model
name lag behind it. Measured, the server was never the problem:

| step | before | after |
|---|---|---|
| TCP + WebSocket handshake | ~1ms | ~1ms |
| auth round trip | **~41ms** | ~0.5ms |
| boot RPCs | 4 serial calls | 1 `bootstrap` call |
| static assets | 3 requests, 3 TCP connections | 3 requests, 1 connection |
| asset re-fetch on every visit | yes (`no-store`) | no (`immutable`) |

Four separate causes, all invisible in a server log:

1. **Nagle on the accepted socket.** The protocol is a chain of tiny messages
   (hello, auth_ok, ready, one reply per RPC), and Nagle plus delayed ACK made
   each back-to-back write wait for the previous acknowledgement. 41ms between
   `auth_ok` and `ready`, on every connection. Fixed with `set_nodelay(true)`.
2. **Serial boot.** `session_state`, `list_sessions`, `list_models` and
   `session_messages` were awaited one after another, and a `gpu_report` call
   that always fails over the gateway sat in the middle of it.
3. **`Connection: close` on static serving.** Every asset cost a fresh TCP
   handshake. On the measured path to the iPhone (direct on the LAN per
   `tailscale status`, but 200-400ms and 25% loss because iOS parks the Wi-Fi
   radio) that is several round trips before the app can even run.
4. **`Cache-Control: no-store` everywhere.** The JS/CSS names are content-hashed
   and can never change under the same name, yet every visit re-fetched ~148K.
   `/assets/*` is now `immutable`; `index.html` stays `no-cache` so a rebuilt app
   is never invisible.

The lesson that generalises: on a high-RTT link the **number of round trips** is
the user-perceived latency, not the work. Server-side the whole boot protocol now
measures ~5ms; the phone still pays roughly one RTT each for TCP, the WebSocket
handshake, auth, and `bootstrap`. That is why boot is one RPC now rather than
four.

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

## Turning it on and off from the app

The footer has a `gateway` pill: grey dot when off, green when something is
attached, with the port and a live device count (`:8099 · 2 devices`). Clicking it
opens a panel with a switch, a scope choice, and copyable URLs for every
non-loopback address on the machine.

What that implies, in order of how likely it is to matter:

- **The desktop app owns the listener.** `gateway_ctl::GatewayCtl` holds the task
  and the bound address, so status is not a guess. The boot-time
  `CWOFFEE_GATEWAY_PORT` path starts through the same controller, which is why the
  pill tells the truth about an environment-started gateway.
- **Scope is a bind address, so changing it restarts the listener.** "This machine"
  is 127.0.0.1; "Any device" is 0.0.0.0. Switching while running stops and starts
  the accept loop; connected clients reconnect.
- **No token, no LAN bind.** The refusal is stated in the panel rather than logged
  somewhere nobody reads. `~/.config/cwoffee/gateway.token` is read if present,
  and created with mode 0600 if it is not — so clicking "any device" on a laptop
  you intend to reach from a phone does the whole thing, and the panel names the
  file the token came from.
- **`gateway_status` is Tauri-only on purpose.** Its payload contains the token
  inside ready-to-open URLs. It is not in the gateway's RPC allowlist: a remote
  client learns the gateway is up from the fact that it is talking to it, never
  the secret. In a browser tab the same panel says "You are on it" and offers no
  switch — turning off the thing you are riding on is not a button worth having.
- **Stopping is an abort.** The listener and every connection on it drop. That is
  the right shape for "off", and it is why the client's reconnect path (see
  "Why the first load took a while") matters: a phone whose link blips while you
  restart the gateway comes back on its own.
- **Start failures reach the UI.** `serve` returns immediately on a bind failure,
  so `start` waits 600ms for the task: if it has finished, that error is the
  reason, and it is shown in the panel instead of the toggle flipping back by
  itself. A `#[tokio]` note: the state lock is never held across an await there,
  because a future carrying a `std::sync::MutexGuard` past a suspension point is
  not `Send` and fails to compile.

Covered by the in-app self-test (`VITE_SELFTEST=1`, desktop only): status →
start on a private port (8123, never the one in use) → status agrees → stop →
status agrees → a second stop must error rather than pretend.

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

### What a drive token can reach

A drive token can start turns, cancel them, answer approvals, switch sessions,
change the model, and **move the workspace** (`set_workspace`). That last one is
worth naming precisely, because it is the widest of them: the sandbox confines the
tools to the workspace, so whoever can move the workspace can aim `read_file` and
`exec` at any directory this user can write to — `~/.ssh`, mail, everything. The
engine validates that the path exists and is writable, and a watch token cannot do
it at all, but the drive token is still the only thing standing between a stranger
and your home directory.

So: a drive token is equivalent to "can read this user's files". Do not put it in
a URL you screenshot, do not leave it on a shared network, and treat `?token=` in
the address bar as a secret you have just written down. `pick_workspace` is not
exposed on purpose — a leaked token should not also be a directory browser.

## Things that will bite

- **Questions park the turn, in both directions.** When the agent calls
  `ask_user`, every attached client gets a `questionRequested` event and the turn
  waits. Answering is `respond_question` with the event's `id`, `text`, and
  `options` - the list of chosen indices, empty when the user typed instead. A
  single legacy `option` number is still accepted so an older page cannot silently
  send an empty answer. A watch-only client can see the question and cannot answer
  it. Cancelling releases the parked tool with a "cancelled" result instead of
  leaving one of the four tool workers idle until the question times out.
- **A question can close without an answer, and every client must hear about it.**
  If the timeout expires, the tool emits `questionExpired {id}`. The shells cannot
  report this themselves: they hold the `oneshot::Sender`, and a sender whose
  receiver is gone says nothing until someone tries to use it. A browser that never
  hears it keeps showing a card whose Send button fails forever, and the answer it
  eventually sends aims at a tool call that already returned.
- **"Devices" means addresses, not sockets.** The footer pill counts devices keyed
  by peer address and names them: `2 devices: 100.86.160.103, 172.20.10.2
  (3 connections)`. Two tabs on one laptop are one device and three connections,
  so both numbers are shown. A socket that has not authenticated yet is neither.
  If the address list is not what you expect, someone else is on your network -
  revoke the token. (The first version of this counter decremented a value nothing
  had incremented, and the pill cheerfully reported 18,446,744,073,709,551,615
  devices. See Trap 26 in `docs/BUILDRUN.md`.)
- **The workspace is runtime state, not a saved preference.** Changing it moves the
  tools for the rest of the session, and new sessions inherit the new value, but a
  restart reads `workspace` from the config file again. To make a move permanent,
  edit `~/.config/cwoffee/config.toml` (top-level key, before any `[table]`).

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
