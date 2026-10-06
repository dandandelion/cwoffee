# Build, run, and the traps in between

Measured on this box (Fedora, Intel iGPU + NVIDIA, llama.cpp on :8090).

## Dev loop

```bash
./scripts/dev.sh                       # vite + tauri dev, GPU policy applied
VITE_AUTOTURN=1 ./scripts/dev.sh       # streams a turn on launch, no clicking
VITE_AUTOTURN_PROMPT='...' ./scripts/dev.sh
VITE_SELFTEST=1 ./scripts/dev.sh       # exercises session create/rename/delete/search end-to-end
```

`dev.sh` is the only supported way to run the dev build. It exports the Mesa pin
before anything touches EGL; running `tauri dev` by hand gets an NVIDIA context.

## Release

```bash
cargo build --release -p cwoffee-app -p cwoffee-tui
./target/release/cwoffee-app
./target/release/cwoffee --url http://127.0.0.1:8090/v1

# package
cd ui && npx tauri build --bundles rpm
# -> target/release/bundle/rpm/cwoffee-0.0.0-1.x86_64.rpm
```

`--bundles`, not `--bundle`. `dpkg-deb` and `appimagetool` are not installed here,
so rpm is the only bundle that works on this box.

## Trap 1 — `cargo build --release` does not produce a release app

This one cost the most time and invalidated a published benchmark.

Tauri's own build script computes `dev = !custom_protocol`, where `custom_protocol`
is the **cargo feature** `tauri/custom-protocol`. The Tauri CLI enables it during
`tauri build`; a bare `cargo build --release` does not. So unless the app crate
declares the feature, `cfg(dev)` is on in the "release" binary, which means:

- the app loads `build.devUrl` (`http://localhost:5173`) instead of embedded assets,
- **no assets are embedded at all**,
- run with no vite server, the window is blank and the page's JS never executes —
  no error, no log, engine healthy, WebKit up, memory fine.

The app crate must declare it, and it must be on by default:

```toml
[features]
default = ["custom-protocol"]
custom-protocol = ["tauri/custom-protocol"]
```

`tauri dev` re-runs cargo with `--no-default-features`, so the dev loop is
unaffected. Verify the feature actually took effect before trusting any release
measurement:

```bash
# must print nothing
grep -c 'rustc-cfg=dev' $(ls -dt target/release/build/cwoffee-app-*/ | head -1)/output
# must list the hashed filenames from ui/dist/assets
strings target/release/cwoffee-app | grep -oE 'index-[A-Za-z0-9_-]{8}\.(js|css)' | sort -u
```

## Trap 2 — a blank webview is invisible from the outside

Every conventional health signal says "fine": the process is alive, PSS is
normal, WebKit child processes exist, the GPU node is held. The only outward
tell is a **missing** `render OK` line.

Two mechanisms make it visible, and both are now in the code:

- `boot_log` — the page's first lines of JS report into the Rust log, in release
  as well as dev. If `boot: main.ts executing` never appears, the page never ran.
- a startup watchdog — if no `render_report` arrives within 8 s the app logs
  `SUSPECT: no render report after 8s - the window is probably blank`.

```bash
# healthy production run, no vite needed
[cwoffee] webview attached: page JS is running
[cwoffee/page] boot: main.ts executing
[cwoffee/page] boot: mount() returned
[cwoffee] render OK: titlebar=true sidebar=true footer=true stream=true overlay=false text=1280 bg=rgb(253, 252, 249) dpr=2.00
```

## Trap 3 — embedded assets can go stale

Assets are embedded by `build.rs` (`tauri_build::build()`). After a `vite build`,
a `cargo build -p cwoffee-app` can finish in 0.25s and keep the previous asset set.
If embedded assets look stale: `touch crates/tauri-app/build.rs && cargo build -p
cwoffee-app`. Also check `ls ui/dist` — `index.html` has been found missing while
`dist/assets/` was present.

## Trap 4 — Tauri argument casing

Command **argument names** are camelCase on the wire; **struct fields** keep their
serde names. Getting this wrong is a runtime error, not a compile error:

```
invalid args `report` for command `render_report`: missing field `stream_el`
```

## Trap 5 — the engine needs a tokio reactor

`SessionHost::spawn` calls `tokio::spawn`. From bare `main` it panics:

```
there is no reactor running, must be called from the context of a Tokio 1.x runtime
```

Construct it inside Tauri's runtime:

```rust
let (mut host, adopted) = tauri::async_runtime::block_on(async { ... });
```

## Trap 6 — killing test processes

```bash
pkill -f 'target/debug/cwoffee-app'   # matches this shell's own cmdline; kills the shell (143)
```

Kill by exact name (`pkill -x cwoffee-app`) or by PID from `ps`. Likewise
`pkill -f mock_llm.py`.

## Trap 7 — memory probes

Gate on **PSS**, and watch the whole descendant tree. RSS double-counts shared
pages: the app is 248 MB PSS / 419 MB RSS, so an RSS gate would fail a build that
is actually inside budget. Matching only the main binary under-reports by ~60%
because `WebKitWebProcess` and `WebKitNetworkProcess` are separate processes.

For anything that might allocate, use a PSS watchdog that SIGKILLs on threshold.
`RLIMIT_AS` is the wrong instrument: tokio/reqwest binaries legitimately map huge
VmSize at small PSS, and the limit produces allocation failures that are not real.

```bash
./scripts/memory.sh cwoffee-app 400
./scripts/gpu-check.sh cwoffee-app
./scripts/sandbox-probe.sh
```

## Trap 8 — the TUI needs a real terminal

A pty with no winsize reports `0x0`. The TUI refuses it rather than rendering:

```
Error: terminal reports 0x0 - Cwoffee needs a real terminal size.
```

This is not cosmetic. An unbounded wrap loop at width 0 once grew the process to
8.7 GB PSS and pushed this box into swap while the inference server was running.

## Trap 9 — moving or renaming the project directory breaks the Tauri build

Tauri's build script records absolute paths in its cached output. After
`mv ~/Projects/cowry ~/Projects/cwoffee`, the build fails with:

```text
failed to read plugin permissions: failed to read file
'/home/dandandelion/Projects/cowry/target/release/build/tauri-*/out/permissions/...'
```

The path in the error is the *old* one, which makes it look like a stray
reference in the source. It is not; it is a stale artifact.

```bash
cargo clean --release -p tauri -p tauri-build -p cwoffee-app
cargo clean -p tauri -p tauri-build -p cwoffee-app   # debug profile, separately
```

`cargo clean -p` is per-profile: without `--release` it leaves the release
artifacts stale, so the debug build succeeds and the release build still fails.

## Trap 10 — a top-level config key written after a `[table]` is silently ignored

TOML assigns a bare key to whichever table is currently open. This parses fine
and does nothing:

```toml
[provider]
base_url = "http://127.0.0.1:8090/v1"
workspace = "~/somewhere"      # parsed as provider.workspace -> ignored
```

The shipped example config had exactly this shape, so `workspace`, `mode` and
`approval_timeout_secs` in a user's file had never been read. Every config
struct now uses `#[serde(deny_unknown_fields)]`, and the error carries a hint:

```text
unknown field `workspace`, expected one of `kind`, `base_url`, `model`, `api_key`

  hint: `workspace` is a top-level key. In TOML it belongs to whichever
  [table] is open, so it must appear before the first [table] header.
```

Top-level keys go before the first `[table]` header.

## Packaging

Both artifacts build from `crates/`, never from `ui/`:

```bash
cd crates
../ui/node_modules/.bin/tauri build --bundles rpm        # 4.79 MiB, system webkit
../ui/node_modules/.bin/tauri build --bundles appimage   # 112 MiB, bundles webkit
```

The AppImage needs FUSE to mount itself. On a machine without it (or inside a
container) run:

```bash
./cwoffee_0.0.0_amd64.AppImage --appimage-extract-and-run
```

Verified: the AppImage renders with no dev server, and the GPU pinning survives
its `AppRun` wrapper — `cwoffee-app` still holds `renderD128` only.

## Trap 11: reading the WebSocket handshake before `accept_async`

The first gateway read the HTTP request head to decide whether the connection was
a WebSocket upgrade, then handed the *already drained* `TcpStream` to
`accept_async`. Every browser connection hung open with no response: the
handshake parser was waiting for a request that had been consumed.

Use `stream.peek(&mut buf)` for the decision. `peek` leaves the bytes in the
socket, so `accept_async` still gets its own request.

Symptom worth recognising: static files serve fine, `curl` on `/ws` gets `101
Switching Protocols`, and the browser UI loads and looks alive but every RPC
silently does nothing.

## Trap 12: Tauri `Channel` is serialized by `toJSON`, and the constructor throws in a browser

Two separate traps in one type:

* `new Channel()` from `@tauri-apps/api/core` calls `transformCallback` in its
  constructor, so it **throws in an ordinary browser tab**. A transport shim that
  re-exports Tauri's `Channel` breaks the gateway build at `new Channel()` before
  any socket code runs.
* A channel reaches Rust as the string `"__CHANNEL__:<id>"`, produced by
  `toJSON()`. If you define your own channel type and pass *that* to
  `tauriInvoke`, Rust sees a JSON object and says
  `invalid args 'onEvent' ... invalid type: map, expected a string`.

So the shim owns its own `Channel` type and, on the Tauri path, swaps in a real
`TauriChannel` before calling `invoke('attach')`. The swap has to happen **before**
any blanket `if (isTauri) return tauriInvoke(cmd, args)` passthrough, or the
passthrough wins and the desktop app cannot receive events at all.

## Trap 13: Tauri wraps command arguments by name

`invoke('render_report', { report })` arrives as `{ "report": { ... } }`, not as
the flattened fields. A gateway re-implementing a command has to read the nested
key. Logging the command name and its argument keys turns "the browser shows
nothing" into a findable shape mismatch.

## Trap 14: a stub response shaped *almost* right is worse than an error

The gateway had no GPU to report, so `gpu_report` returned `{note: "..."}`. The
call succeeded, the UI stored it, and the telemetry bar dereferenced
`gpu.render_nodes.join(...)` during render - which threw inside Svelte's flush and
blanked the app before it could load a session. Symptom: the page renders its
chrome, the session list appears, and the transcript never does, with a
`Cannot read properties of undefined (reading 'join')` in the console.

Two rules:

* a command that cannot be honoured should return an error, not a plausible object;
* anything rendered from a remote struct gets guarded field by field. One missing
  key must not take down the whole view.

## Trap 15: a connect promise that never settles invents a false diagnosis

`connect()` resolved on `auth_ok`/`ready` and rejected on `auth_fail`, but **not**
when the socket simply closed first. So if the upgrade never completed - a 404 on
`/ws`, a proxy, a captive portal, a server that went away - `await connect()` hung
forever. `loadModels()` never returned, `modelErr` stayed empty, and the model menu
rendered the `{:else}` branch: *"This endpoint does not list models. Using —"*.

That is the interesting kind of bug: the UI was not wrong about what it had, it was
reporting the absence of a value it never got, and the message blamed a llama.cpp
server that was running fine. Two rules:

* every promise that represents a connection must settle on **all** terminal paths,
  including plain close;
* a UI branch that means "the server said no" must be distinguished from "we never
  asked". Guard the branch on connection state, not just on an empty result.

Related: one `peek()` is not enough to classify a request. A handshake split across
segments leaves a first peek holding only `GET /ws HTTP/1.1`, which is not the
upgrade header. Decide on the header *or* the request-line path, after retrying the
peek a few times.

## Trap 16: peeking for a request head has to wait for the head

Classifying a connection from a `peek()` needs the header block, not a sample of
it. The first version tried four peeks with 40/80/120ms backoff - the last peek
landed around 240ms - so a client whose first TCP segment held only
`GET /ws HTTP/1.1
` was routed to the websocket layer with an incomplete head,
and `accept_async` answered `Junk after client request` and closed.

It now waits up to 2s for `\r\n\r\n`, and the request-line fallback means a slow
first segment still routes correctly: `accept_async` reads the rest. Verified with
a client that sends the request line, waits 2.5s, then sends the remaining headers:
`101 Switching Protocols`.

When you reproduce a handshake by hand, be careful that your split does not insert
a second `\r\n` - that terminates the header block early and produces a bogus
"Junk after client request" that looks like a server bug and is a test bug.

## Trap 17: a blocking client read inside `#[tokio::test]` starves the server task

`#[tokio::test]` runs on a **current-thread** runtime. A test that spawns the
connection handler and then calls `client.read_to_string(...)` blocks the only
thread, so the handler never runs, the read times out, and the failure reads as
"the server sent zero bytes". It had in fact sent everything - after the test
gave up.

Use `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]` when the test
drives a real socket from blocking std code, or move the client I/O into
`spawn_blocking`.

## Trap 18: keep-alive must not discard bytes past the request head

Once a connection answers more than one request, a single `read()` can return the
head of request 1 *and* part of request 2. The first keep-alive version did
`buf.truncate(pos + 4)`, which threw request 2 away: the client sat waiting for a
response that would never be written. Keep the buffer across iterations and
`drain(..head_end)` instead of truncating.

The test that catches it pipelines two requests in one write. Sequential requests
(a browser, `curl`, `http.client`) all pass without it, so pipelining has to be
tested explicitly.

## Trap 19: a Rust `\` line continuation written from a Python heredoc vanishes

Rust strips a trailing `\` in a string literal *and* the leading whitespace of the
next line. Python does exactly the same thing to a `\` at the end of a line inside
a normal (non-raw) triple-quoted string. So a patch script that writes a wrapped
Rust format string produces one long line with the indentation baked into the
message:

```text
it must be an existing,                      writable directory
```

That is only cosmetic here, which is why it survived review, but it lands in user
visible error text. Either write the Rust string on one line, or use a raw Python
string (`r"""..."""`) so the backslash survives. The same trap already bit this
project with `\u{...}` escapes - see the note on raw strings above.

## Trap 20: a release webview loads embedded assets, not `ui/dist`

`npm run build` changes what the **gateway** serves (it reads `ui_dist_dir()` from
disk on every request), but the Tauri window serves the frontend **embedded in the
binary** when built with `custom-protocol`. So the usual "rebuild the UI, reload"
loop silently tests the old UI in the desktop window while the browser tab shows
the new one.

The case that made this visible: a `VITE_SELFTEST=1` build written to
`ui/dist-selftest` and pointed at with `CWOFFEE_UI_DIST` ran no self-test at all,
because `frontendDist` is a compile-time value. To exercise a UI-flag build in the
desktop window, temporarily point `build.frontendDist` at the alternate directory
and rebuild the binary — which also keeps the flag build away from the `ui/dist`
the gateway is serving to a phone.

The reverse foot-gun is the one that cost an hour of chasing a phantom: build the
selftest UI into `ui/dist`, then `cargo build --release`, and the *production*
binary ships the self-test. The app then runs turns by itself a minute after
launch — "Use the exec tool to run `echo cwoffee-ok`", an approval card, an answer
about an image that was never attached — and the log lines say
`[cwoffee/page] turn 1 start` with no send in front of them. **Order matters:
`npm run build` (no flags) and then `cargo build --release`.** The audit line in
`start_turn` (see Trap 32) is what settles arguments like this: a turn with no
`start_turn (desktop)` or gateway `start_turn` line in front of it did not come
from a user.

## Trap 21: `std::fs::read("/dev/urandom")` never returns

urandom is not a file with a size; `fs::read` reads until EOF, which does not
arrive. A token generator written that way hangs the caller. Read a fixed buffer:

```rust
let mut buf = [0u8; 24];
let mut f = std::fs::File::open("/dev/urandom")?;
f.read_exact(&mut buf)?;
```

## Trap 22: `pkill -f` matches the shell that runs it

`pkill -f XDG_DATA_HOME` to kill a test instance also matches the command line of
the shell issuing it, and the harness dies with 143 halfway through a script —
leaving the app in an unknown state. Kill by exact name or by pid:

```bash
pgrep -x cwoffee-app            # names only
for p in $(pgrep -x cwoffee-app); do [ "$p" = "$KEEP" ] || kill "$p"; done
```

## Trap 23: the streaming reveal must not re-parse the tail in halves


( Necessary, and not sufficient: it fixed the structure and the effect was still
invisible. See Trap 25 for the part that actually mattered. )

Animating "the text that just arrived" looks like it has a cheap implementation:
render what was already shown and the new suffix separately, and wrap the second
one. It does not survive contact with the parser. Each call opens its own block
elements, so a growing paragraph became two paragraphs with a gap, and a growing
bullet lost its continuation outside the list — both only noticed by screenshotting
the running app.

The working shape is: parse the tail **once**, then wrap the trailing characters of
the last *text run* in an inline `<span>`. Inline keeps structure, and the
guarantee is stated as a test invariant — strip the span and the animated paint is
byte-identical to the plain paint of the same state (`ui/scripts/md_test.ts`
asserts that for prose, bullets, code, tables and mixed documents, and separately
asserts that the reveal actually ran, because a reveal that silently switched
itself off also passes the equality test).

Two related details: the wrapper must skip trailing inter-tag whitespace
(`</li>\n</ul>` is a "text run" that animates nothing, which quietly disabled the
effect for every list), and the animation is `opacity` + `filter: blur()` only —
transforms do not apply to non-replaced inline boxes.

## Trap 24: `max-height: none` cannot be transitioned

Collapsing tool output and thinking blocks is a `max-height` toggle. Transitioning
to `none` does nothing, so expansion stayed instant. Use a large concrete value
(`2400px`); the easing front-loads the growth so the visible part still lands in
about 200ms.

## Trap 25: an animation shorter than the repaint cadence is not an animation

Trap 23 fixed the reveal's *structure* and left it invisible anyway. The user's
report was blunt: "I don't see it." Measured, it was right not to.

The reveal wrapped the newest characters in `<span class="tin">` inside the
rendered string, and the live block is repainted every 120ms by replacing
`innerHTML`. So every span lived one paint. The animation was 170ms on
`cubic-bezier(.16,.84,.24,1)`, which is ease-out: by 33ms opacity was already
0.70 and blur 0.65px. Three soft frames, then the text was replaced by an
identical, un-animated copy of itself. It was working, computing, and
reporting correct computed styles - and doing nothing a person could see.

The unit has to be a **node that outlives the paint**, not a substring:

- the parser hands over `head` (settled text) plus `chunks` (arrivals since the
  last paint, each with a stable id),
- the caller appends one span per chunk and never rewrites it,
- a chunk folds back into `head` only after `REVEAL_HOLD_MS` (520ms), which is
  comfortably past the 320ms animation, so the fold is invisible.

After: median chunk lifetime 574ms, up to 4 chunks in flight, and a chunk spends
~150ms between opacity 0.15/blur 2.5px and 0.78/0.66px. Visible in a 95ms
filmstrip without squinting.

Three traps inside the fix:

1. **`committedChars` reset to 0 on a repaint-whole.** The head is
   `para.slice(0, committedChars)`, so the characters painted *before* the reset
   disappeared from the DOM: every answer lost its opening words the moment its
   first chunk was cut. A reset means "everything seen so far is static", i.e.
   `committedChars = para.length`.
2. **The first paint of any answer is a block paint**, because the window has to
   be established before it can be diffed. Tests written as "every paint is
   inline" fail for a correct reason; assert one block paint, not zero.
3. **`matchMedia` at module scope silently disables the effect in a test
   runner** (no `matchMedia`, so the guard evaluated false and every paint was
   block). Any capability probe belongs inside the function that uses it.

The gate is also narrower now: only prose reveals. A tail holding a list marker,
fence, table, heading or raw HTML paints whole, because chunk-level inline
rendering cannot reproduce a `<li>` and a wrong structure is worse than no
effect. `paraOnly()` is the gate and has its own tests, so the structure tests
cannot pass by accident.

## Trap 26: an `Arc<AtomicUsize>` counter that is only ever decremented

The gateway pill displayed `18446744073709552000 devices`. That is
`usize::MAX`: `ClientGuard`'s `Drop` did `fetch_sub(1)` and nothing ever did
`fetch_add`. The count went negative on the first disconnect and wrapped.

Two fixes, both structural rather than a clamp:

- the increment moved into `ClientGuard::attach`, so holding a guard *is* being
  counted and there is no way to construct the uncounted case;
- the counter became a table keyed by peer address, so the pill can name the
  devices it claims. It now reads "2 devices: 100.86.160.103, 172.20.10.2
  (3 connections)" - a number a person can check by looking at the list.

The self-test asserts the count is finite and plausible. The engine tests cover
both directions (attach to a named peer, release back to zero) and the case that
mattered for privacy: **a socket that never authenticated is not a device.**

## Trap 27: adding a tool invalidates every prefix cache behind it

`ToolRegistry::names()` is asserted in `prompt.rs` on purpose. Inserting a tool
in the middle of `register_builtins()` reorders every later tool definition,
which changes the prompt bytes and throws away the llama.cpp prefix cache for
every existing session. `edit_file` and `ask_user` were therefore **appended**,
and the expected order updated. When that test fails it is the cost of the
change being surfaced, not a bug in the test.

## Trap 28: `ask_user` renders as a question card, not a tool card

`ask_user` is an ordinary tool call, so it arrives through the same tool stream
as `exec` - and the engine also emits `questionRequested`. Without a special
case the user sees a collapsed `ask_user` card *and* the question card.
`openTool()` returns null for `ask_user`.

The reply sender is parked in each shell keyed by the call id, which is the same
id the card is drawn with, so `respond_question(id)` addresses exactly what is on
screen. Cancellation drops the parked senders: the tool wakes immediately with a
"cancelled" tool_result instead of holding one of the four tool workers until the
question timeout expires.

## Trap 29: the reveal flashed because settled text was rewritten every paint

Trap 25 made the animation long enough to see and introduced a new artefact: the
newest words blinked once more ~520ms after arriving, just as the turn ended.

Two things happened on *every* paint (~125ms), not just at fold time:

- `els.head.innerHTML = r.head` re-parsed and re-rastered the entire settled
  paragraph, every frame, to add a few words at its end;
- a chunk finishing its animation left the compositing layer that the `filter`
  had put it in, and text rasterises differently inside a layer than outside it.

Fix: **fold, don't rewrite.** Moving the finished span's child nodes into the
settled run touches only those nodes. The head rewrite stays, but now fires only
when the accumulated markup genuinely disagrees with a single pass over the text
- which happens when inline markdown straddles a chunk boundary, not on every
  frame. `.tin-para` gets `transform: translateZ(0)` so the whole streaming
  paragraph is one layer and text never changes rasterisation mode.

Measured, in the running app over a whole turn: **0** text nodes removed from the
settled run (previously one full rewrite per paint), and the first settled line
was **pixel-identical across 14 frames** sampled during streaming - while 17
chunks still animated from opacity 0 / blur 3px upward and `**bold**`/`` `code` ``
still rendered correctly.

## Trap 30: a nested lock on `state_bg` inside a host command arm

`SessionHost` runs one task that owns the agent, the store and the approval map.
The command loop holds `state_bg` for part of several arms. Adding "follow this
session's workspace" to `SwitchSession` looked like a three-line change and
deadlocked the host loop, because the code called `state_bg.lock()` again while
the same arm already held it:

```
test host::tests::switching_a_session_moves_the_tools_to_that_sessions_folder has been running for over 60 seconds
```

Nothing else in the app recovers from that: the host task is the only thing that
drains the command queue, so from the outside the app looks like a UI that stopped
answering, not a deadlock. Read the workspace off the store **before** taking the
state lock, apply the change inside the guard that already exists, and leave a
comment at the site — the shape is invisible in the diff.

Related, same file: `check_workspace` distinguishes "is not a directory" from "is
not writable; the tools could not create a file there" because they are different
mistakes with different fixes, and one merged message made both of them unreadable
(and broke a test that asserts on the substring).

## Trap 31: `.blk` is a two-column grid — append to it and your content gets 74px

> Superseded by Trap 42: the grid is gone. The lesson about append order inside a block
> still stands; only the column width does not.

`buildToolCard`, the reasoning block and the answer block all append a `.blk-head`
and then a body element to a `.blk` card, whose grid is `74px 1fr`. The rewritten
question card appended `.ask-q`, `.ask-opts`, `.ask-custom` and `.ask-actions`
straight onto the card, so the option list landed in the **label column** — 74px —
and every sentence wrapped at about five characters:

```
  ○  (2) a csv
     of three
     espresso
     drinks
```

Sane-looking CSS (`display: flex`, `grid-template-columns: 20px 1fr`, sensible
padding) cannot fix a child that was placed in the wrong grid cell. One wrapper
element with `grid-column: 1 / -1` put the whole card back in one continuous
column. The check that catches it is a width assertion, not a screenshot:
`.ask-opts` must be within a few px of the card's inner width.

## Trap 32: a question nobody answered, and the shells that cannot say so

`ask_user` parks on a `oneshot::Sender<Answer>`. The desktop, the gateway and the
TUI each keep that sender in a map so the UI can answer later. When the tool's
timeout expires, the **receiver** is gone — and a `oneshot::Sender` whose receiver
has vanished stays quiet until someone tries to `send()` on it. So:

- the card stays live in every window, still offering Send;
- pressing Send aims an answer at a tool call that already returned;
- nobody can tell from the shells that the question is over.

The tool is the only party that knows, so the tool now emits
`EngineEvent::QuestionExpired {id}` on the timeout and cancel paths (via
`ToolCtx::events`, which the agent fills in per call), and every front end closes
the card as "no answer in time". Two tests pin it: an unanswered question emits the
event exactly once, and an **answered** one emits nothing — a second "closed"
signal after an answer reads as a contradiction.

The gateway side gained the matching audit line (`AUDITED` in `dispatch`): drive
commands log the peer and argument *shapes*, never message text.

## Trap 33: the engine renamed a wire enum and TypeScript did not notice

Risk went from `readOnly | mutating | exec` to `harmless | workspace | sensitive`,
`Answer.option: number` became `Answer.options: number[]`, and `Question` gained
`multi`. `cargo check` was green the whole time and `svelte-check` stayed green
until the handler types were mirrored by hand, because engine events cross the
boundary as JSON and the UI's event union is a *copy* of the Rust enum, not a
generated type.

The rules that keep this from recurring: the Rust doc comment in `event.rs` states
the wire contract (adjacent tagging, camelCase variant names, snake_case fields);
`ui/src/lib/events.ts` mirrors it and comments each field; and the desktop
self-test asserts on the *strings* — that an approval card renders the risk label,
that answering a multi question sends `options: [0]` and not `option: 0`. The old
`respond_question(id, text, option)` signature survived in the gateway as a tolerated
legacy alias, and only there: one place accepts the old shape, and it is the place
where an old client is actually expected.

## Trap 34: a scripted patch is a code review you skipped

Two self-inflicted wounds from patching Rust with Python string surgery:

- a `\` line continuation inside a `format!` written from a heredoc swallowed the
  newline and the leading spaces, so the emitted text lost its formatting and the
  diff looked fine (Trap 19, and it came back);
- a script that inserted `events: None,` into every `ToolCtx { ... }` literal
  corrupted the ones that end in `..Default::default()` on a single line, producing
  `error: expected ;, found events` in three files.

Both were caught by compiling immediately after patching, which is the point: a
scripted edit that is not compiled in the same breath is a claim, not a change.
When a patch touches a struct literal, patch and `cargo check` in one command.
## Trap 35: renaming the workspace folder strands sessions, and the sidebar tells the truth about it

`~/cowry-workspace` became `~/cwoffee-workspace` in the rebrand, but a session row
stores its folder **as a string**. Twelve sessions kept pointing at a directory that
no longer existed, which produced two honest-but-useless symptoms: a sidebar group
named `cowry-workspace` for a folder that is not on disk, and every tool call in those
sessions being refused, because the workspace follows the session and that workspace is
gone.

`legacy::rehome_session_workspaces` rewrites the prefix on `sessions.workspace` and
`workspaces.path` when, and only when, the rename signature holds: the old folder is
absent and the new one exists. If both exist it does nothing — someone may genuinely be
using the old name (a second install, a symlink), and moving their files under a folder
they never chose is a worse bug than a phantom group. Subfolders follow by prefix, so a
session in `old/repo/` becomes `new/repo/`.

Two things to expect afterwards, both correct:

- `list_workspaces()` unions registered rows with folders that hold sessions, so a
  subfolder shows as its own group. That is the feature, not a duplicate row.
- A session in `/tmp` used to get a group of its own, because that is where its tools
  really did write. It no longer does: the temp root is now the *no workspace* bucket and
  buckets under `Default` with the sessions that never chose a folder. See Trap 40.

## Trap 36: a gateway stub that returns the empty value is a claim, not an absence

`migration_notes` over the gateway answered `[]` unconditionally. Same mistake as the
`gpu_report` stub in Trap 22, only quieter: an empty list *reads* as "nothing needed
migrating" when the truth is that this process never knew. The notes now travel into
`EngineHandle` from whichever process started the gateway (desktop or standalone), and
the test asserts both directions — empty when nothing was reported, present when
something was.

## Trap 37: an approval answer spelled one way in the UI and another in the gateway

`ApprovalAnswer` is a Rust enum with `#[serde(rename_all = "camelCase")]`, so its wire
forms are `allow`, `allowSession`, `deny`. The UI sends exactly those. The gateway parsed
the answer by hand — `"allow" | "allow_session" | "deny"` — so every approval answered
from a phone as anything other than plain `allow` came back as
`unknown answer "allowSession"` and the parked tool sat there until it timed out. The
tool looked like it ignored the user.

Two rules follow:

- Parse a wire value with the same `serde` impl that defines it
  (`serde_json::from_value::<ApprovalAnswer>`), plus the legacy spelling if old clients
  exist. A hand-written match on strings is a second definition of the enum, and it will
  drift the moment someone renames a variant.
- Pin the wire spelling with a Rust test (`the_wire_spelling_of_an_approval_answer_is_pinned`)
  *and* drive it end to end from a fake client
  (`an_approval_answered_from_the_phone_reaches_the_engine`). The first catches a rename,
  the second catches a transport that forgets to parse.

## Trap 38: `animation-fill-mode: both` holds a filter forever

The streaming reveal was reported as a blink "at the end of the token transition", which
is the opposite of what a fade-in should feel like. The chunk span kept the last keyframe
forever — `filter: blur(0)` — and any filter puts an element in its own raster layer. So
the text existed twice: the layer the animation held, and the layer the folded, static
paragraph was drawn into. Swapping between them is a re-raster of glyphs, and at the end
of every chunk that swap is a visible pop.

Use `backwards`: the animation owns only the *pre*-animation state, and at the end the
span is ordinary text again, so folding it moves glyphs that were already rasterised.

Related, and easy to get wrong twice: `transform: translateZ(0)` on an inline span does
nothing (transforms do not apply to non-replaced inline boxes). The layer hint belongs on
the block that contains it — `.blk-reasoning .blk-body`, `.tin-para`.

## Trap 39: a `<div>` per streamed delta makes the paragraph grow down the block

Thinking deltas were appended with the transcript's `el()` helper, which builds a `<div>`.
Every delta therefore began on a new line: the thinking block was a stack of one-clause
lines, each spanning the full width, and it got taller on every single paint. Two
complaints came from that one line of code — "tokens grow vertically instead of filling
the width" and "the whole thing flashes", because the transcript pins to the tail and so
nudged the entire view on each delta.

Chunks that belong inside a paragraph have to be inline. Measured in the running app:
thinking body 780px, chunk width 768px each — the numbers that made it obvious. The
self-test now asserts `getComputedStyle(chunk).display === 'inline'` and that the run's
bounding width is at least 70% of the block's, which fails exactly the way the bug looks.

## Trap 40: the harness leaned on `/tmp` as a workspace

A workspace check in the self-test did `set_workspace('/tmp')` and asserted it landed,
because `/tmp` is always writable and needs no cleanup. The moment the engine started
treating the temp root as "no workspace" — it is where sessions live *before* anyone
chooses a folder, so drawing it as a folder group beside `No workspace` was the same
thing in two rows — the check failed, and because the call was not inside `try/catch`
the rejection escaped and aborted the whole suite ("aborted before the summary"), with
the engine's message as the only report.

So:

- Tests that need a real folder make one (`unique_dir(tag)` in the gateway tests, a
  registered project folder in the self-test), never the temp root.
- Every `set_workspace` in the harness is inside `try/catch` and asserts the *reason*
  (`/temp folder/`, `/keeps its folder/`), not just that it threw.
- The engine refuses `/tmp` in `check_workspace`, the UI buckets it as `Default`, and
  the UI surfaces the refusal as a toast rather than an inline error in a panel that is
  usually already closed.

## Trap 41: the label column was a tax on every block, and `$derived` reads ahead

The transcript's `.blk` grid gave every block a 74px label column. The assistant's text
started 74px right of nothing, the user's message started somewhere else, and the two
never lined up — which is why the request was "let the CWOFFEE label protrude to the
left". The fix is one column plus a gutter: the scroller gets the padding
(`.scroller { padding-left: 80px }`, written after the Tailwind import so it beats
`px-6` without `!important`), and `.blk > .blk-head` hangs in it with
`position: absolute; left: -66px`.

Two traps inside the fix:

- Any block that keeps its own full-width header row (thinking, tool cards) must opt out
  with `position: static`, or its header is outdented into the gutter and truncated at
  58px. The narrow-window media query does the same for every block, because below 860px
  there is no gutter to protrude into and the label would be clipped by the scroller.
- A `$derived` that reads a `let` declared further down the module is a temporal dead
  zone error, not a hoisted reference: `Block-scoped variable 'viewId' used before its
  declaration`. `$derived.by(() => ...)` is fine because the read happens on the first
  reactive update, after the module has finished initialising.

## Trap 42: a gate that waits on the model's whims fails for reasons that are not bugs

The approval check asked the model to `exec echo cwoffee-ok` and waited 90s for an
approval request. It passed for months, then began failing — not because the approval
path broke (it does not: a live probe gets `read_file → sensitive → allowSession → ok`
in a few seconds) but because a quantized local model will sometimes spend minutes
emitting tool-call JSON that never completes. Three runs of the same suite measured
7.5k, 11k and 24k delta frames with `chars=0`: no tool call parsed, so no approval ever
arrived, and *every check after it* failed with a message about the previous turn
("cannot delete the active session while a turn is running", "a turn is running, so the
workspace change is queued behind it").

What made it survivable:

- **Ask for the cheapest thing that exercises the wire.** A `read_file` of a path
  outside the workspace is one line of arguments, is gated in `ask` mode, and asserts
  the risk class (`sensitive`) and the escape (`OUTSIDE the workspace: /etc/hostname`)
  in the same step. `exec` remains covered by the Rust tests and the sandbox probe.
- **Pin the reasoning level per step, and restore it.** The suite pins `medium` overall
  and `off` for the steps whose subject is wiring. Thinking is still asserted where it
  is the thing under test. A gate that costs twenty minutes of inference on a
  single-slot endpoint does not get run, and an unrun gate is not a gate.
- **Never leave a turn running into the next check.** `settleTurn(what, budget)` waits a
  bounded time and then cancels, logging that it did. Before it, one slow turn produced
  seven failures and the summary blamed the wrong step every time.
- Say what was waited for and for how long (`waitTurnDone: no turnEnd after 150000ms
  (turns=3->3)`, `settleTurn: before the move check was cancelled (attempt 1) instead of
  waited out`). A timeout that does not name itself is indistinguishable from a hang.

The two oracles the harness uses now, because the old ones were wrong:

- **`turnEnds`, not `HostState.turns`.** `turns` is a per-session counter, so a check
  that switched sessions waited on a counter belonging to a different conversation and
  reported "turn never ended" about a turn that had plainly finished and streamed. The
  webview counts `turnEnd` events; the store counter is kept only as a fallback for a
  turn that ended during a reconnect.
- **Idle must persist.** The first `busy == false` sample is not the end of a turn: a
  turn queued behind a cancelled one starts a moment later. `settleTurn` requires six
  consecutive idle polls (~1.5s) and cancels up to three times, because the queue
  re-fills.

## Trap 43: "Stop does nothing" - a turn parked on an approval outranks the Stop button

`cancel_turn` set the agent's cancel flag, and the flag interrupts the provider
stream. It did nothing to a tool sitting in `ApprovalGate::check`, waiting on a
`oneshot` whose sender lived in a front end. The Tauri command dropped parked
*questions* on cancel; nobody dropped parked *approvals*; the gateway dropped
neither. Result: press Stop on a turn that is waiting for you to answer a prompt, and
the host stays `busy` for the whole approval timeout - 300s by default. The desktop
self-test caught this by accident: an answered `read_file` prompt, then an unanswered
`exec` one, and six failures downstream all complaining that a turn was running.

The fix belongs in the engine, not in each front end, because the engine is the only
place that knows a cancel happened:

```rust
let gate = if already_stopped {
    // Asking now puts a card on screen that the user has already refused.
    GateOutcome::Deny { reason: "the turn was stopped before the approval was asked".into() }
} else {
    tokio::select! {
        biased;
        _ = Self::wait_cancel(cancel_wait) => GateOutcome::Deny {
            reason: "the turn was stopped while the approval was pending".into(),
        },
        o = self.gate.check(&call.name, &risk, &args) => o,
    }
};
```

`biased` matters: without it a prompt answered in the same tick as the Stop press can
win, which is the opposite of what a Stop press means. The front ends still drop
their senders on cancel - that releases the park on the spot instead of on the next
poll, and the count is worth logging - but they are no longer load-bearing.

The test that pins it (`a_stop_press_releases_a_parked_approval`) takes the prompt and
**holds the sender open**. Dropping it would let the "approval UI disconnected" branch
release the park and the test would pass while cancel did nothing.

## Trap 44: "my conversation is being mutated" - one host, no addressee

A harness run put its turns - including a 650-word essay and a file chip for it - into
the conversation the user had open. The transcript looked like it was editing itself.

There was no corruption. `HostCmd::Send` carried text and attachments and no session id,
so it ran in *whichever conversation the host happened to have active*. That is correct
for a single-window app and indefensible for one that also serves a phone tab and its own
test harness: every client was writing into whoever's turn was last.

The id is an **assertion**, not a hint, because a mismatch means the client and host
disagree about what is on screen:

| client says | host does |
|---|---|
| nothing | runs in the active session, as before |
| `0` ("my view is a draft") | drops to draft, so the send opens a new conversation |
| `512` | runs there, or refuses |

```
the turn asked for session 512, but the host is on session 508: nothing was sent.
```

Refuse rather than reroute: appending a message to a session the sender cannot see is
the bug with a nicer error string, and it also silently moves the host's active session
under everyone else's feet.

The refusal is an `error` **event**, not an rpc error: the command parsed and was
understood, the engine declined to execute it. Clients that only look at rpc errors -
which is every client here until this change - stay silent while the engine says no. The
desktop now renders those messages as a toast.

Tests: `a_turn_named_for_another_session_is_refused`, `a_draft_send_starts_its_own_conversation`,
and gateway `a_turn_for_a_stale_view_is_refused_over_the_wire` - which pins the event-not-error
part, because that is the half a client can get wrong on its own.

The harness was the perpetrator, so it also lost the habit: every `start_turn` in the
self-test names its session, the run captures the session that was open before it started,
and it switches back to that one at the end.

## Trap 45: "you can't switch sessions mid-conversation" - a UI waiting on a busy host

The click was never blocked by policy. The host runs **one** command loop, and while a
turn is running that loop is inside the turn, so `switch_session` queued behind it and the
`await` in `openSession()` froze the view until the model finished.

The view and the host are separate things and only the view has to move now: the command is
still sent, but not awaited while `busy`. The transcript is already addressed by session id
(so a running turn never paints into another session) and the sidebar row of the working
session gets a spinner.

Two sidebar markers that used to mean whatever the moment wanted: a pulse for *answering*
and a pulse for *unread*. They are different questions - "is it working?" vs "did something
arrive while I was away?" - and are now different marks: a turning arc while a turn is in
flight, the pulse only for a turn that ended while you were looking elsewhere.

## Trap 46: "the file wasn't attached" - a strip that belonged to the live turn

A turn wrote a 650-word paragraph, the engine recorded the file, the store kept it, and
the transcript showed nothing.

The live path was fine. The *history* path appended the chip row with `ensureTurn()`,
which attaches to the live `.turn` group - and on a reload that group is created before
the history fragment is appended, so the chips landed at the **top** of the transcript,
above every message, off screen. A session reopened after producing a file looked like a
session that had produced nothing.

Rendering against a detached fragment has to keep every node inside that fragment:

```ts
const strip = this.fileRow(m.files)
frag.insertBefore(strip, lastWrap ? lastWrap.nextSibling : null)
```

The assertion now checks DOM *order*, not existence: the chip must follow the answer
block that produced it. `document.querySelector('.filechip')` passes on the broken build.

## Trap 47: the harness was refused by its own safety rule

Addressing turns (`start_turn { session }`) is what stopped the harness writing into the
user's conversation. It then broke the harness in a way that looked like a hang:

```
the turn asked for session 601, but the host is on session 512: nothing was sent.
```

A previous step had opened a real session, moving the host. The harness had moved only its
*view* (`resyncView`), which by design never calls `switch_session` - a watch-only client
must not steal the host. So the harness sat in a session it was not addressed to and
waited 150s for a turn the engine had already declined.

Two changes, and they are different in kind:

* **harness**: `useSession(id)` - switch the host *and* the view, then assert they agree,
  before anything sends.
* **product**: `send()` moves the host to the session this window is looking at, and logs
  it. If you are staring at conversation X and press send, "the host moved because a
  phone tab switched it five minutes ago" is not a useful answer. The engine's refusal
  stays as the backstop for a turn that is *addressed* somewhere else.

## Trap 48: layout assertions on a hidden tree measure zero

```
the CWOFFEE label is not outdented ... (label 0, body 0, gutter 80)
```

`label 0` is the tell. Empty mode hides the scroller (`.empty-mode .scroller { display:
none }`), so every `getBoundingClientRect()` inside it is zero and a suite of layout
assertions - outdent, bubble width, bubble alignment - turns into theatre that fails for
reasons of state, not of CSS.

A layout check now pins the state it assumes:

```ts
await useSession(target.id)
if (getComputedStyle(document.querySelector('.scroller')).display === 'none')
  fail('the transcript is hidden (empty mode) while the layout checks ran')
```

and the outdent check reads the layout actually in force: in a narrow window the label
legitimately returns above its block, so it asserts the gutter it can see rather than the
gutter the design prefers.

## Trap 49: `HostState` grew fields the TypeScript type never heard of

The engine added `workspace_refusal` / `refusal_seq` and has shipped `reasoning: "xhigh"`
for a while; the UI's `HostState` had neither, so a diagnostic that printed `refusal_seq`
would not compile and a reasoning comparison against `xhigh` silently narrowed to
`never`. Wire shapes are one contract in two languages: `ReasoningLevel` is now exported
from `lib/events.ts` and used by the picker, the state type and the harness alike.

## Trap 50: the harness deleted the conversation it was built on

Seven failures in one run, and they looked like four product bugs:

```
the move check ran on a session with no stored folder
the move refusal does not explain itself: ""
a refused move still landed: /home/dandandelion/Projects
the attachment survived the turn but not the reload
the transcript is hidden (empty mode) while the layout checks ran
```

The sessions-CRUD test does `delete_session(target.id)` to prove deletion works, and every
later step kept naming `target.id` - a session row that no longer existed. Each symptom is
what that state looks like from one vantage point:

* the engine's folder lock reads the session row, finds nothing, and so has nothing to
  protect - the move applies;
* the view has no stored rows, so `session_messages` returns none and empty mode hides the
  scroller;
* every layout assertion then measures a hidden tree, where all rects are 0.

Fixes, in the order that matters: the suite re-opens a scratch session and re-anchors
(`useSession`) after that delete; `loadTranscript` names the session it loaded (a bare
"loaded 0 messages" cannot distinguish "wrong session" from "empty session"); and store
truth is asserted separately from screen truth, so a missing attachment says either "the
engine did not stamp it" or "it is in the store but not on screen" rather than one
ambiguous line.

Also learned the hard way: `list_sessions` is bounded (`LIMIT 50`), so "the row is not in
the list" is not evidence that the row does not exist - check the store directly.

## Trap 51: the suite wrote its probes into real conversations

`sqlite3 sessions.db "select ... where content like '%What colour is this image%'"`
returned 60+ hits across sessions the user actually cares about: every self-test run
before the addressed-turn fix appended its probe turns to whatever conversation the
host happened to be in. The user's report was literally "my conversation is being
mutated", and the turns that proved it were ours.

Cleaning that up is not one `DELETE`:

* a probe turn is a user message **plus** everything the engine appended after it
  until the next user message; deleting only the user row leaves answers with no
  question;
* `messages_fts` is a standalone fts5 table, not a content mirror, so its rows are
  removed by `message_id` - skip that and search still offers text that is gone;
* sessions deleted long ago by hand (`DELETE FROM sessions WHERE id IN (...)`) left
  198 orphan messages, and those still join into search results whose `session_id`
  has no row in the sidebar. `delete_session` deletes `messages`, `messages_fts` and
  `turns`; manual surgery has to do the same.

`scripts/probe-cleanup.py` does all three, prints what it would remove, and needs
`--apply`. Run it against a checkpointed database with the app stopped.

## Trap 52: hover lightened an ink button until the icon vanished

The send button is filled with `--color-ink` and drawn with a `--color-paper` glyph.
Its hover rule said `background: var(--color-ink-2)` - a mid grey - while the generic
`.cbtn:hover` repainted the glyph `--color-ink`. Fill and glyph swapped ends: a grey
square with a dark arrow at 2.6:1 contrast, which is the "hovering erases the button"
report. Two rules, one visual result, and neither is wrong in isolation.

Hover now keeps the fill solid and only warms it
(`color-mix(in srgb, var(--color-ink) 82%, var(--color-shell))`), restates
`color: var(--color-paper)` so the generic rule cannot win by specificity, and the
approval-waiting `.warn` variant has its own hover so it does not lose its meaning
under the cursor. Measured on the running app: normal 14:1, hover 9.7:1, glyph inside
the button box in both states. `clip-path`/`mask` are `none` in both states, which is
what distinguishes "faded" from "erased" when the complaint is a screenshot.

## Trap 53: "context full" at 43k on a 262k model — a budget that never looked at the model

The compaction budget was a constant: 8000 tokens, whatever the endpoint. The status bar said
`43k / 262k`, the footer said the context was full, and the engine dropped the oldest messages
of a conversation the model had room for six times over. A budget invented in a source file is
a guess about someone else's hardware.

It now comes from the server: `list_models_info()` for the loaded model, then
`window * 0.72 - 2048` floored at 4096, re-derived on every model switch. `context.target_tokens`
still wins when present — a pin is a decision — but the pin had leaked into the shipped example
config and therefore into the user's config, which is how the fix stayed invisible after it was
"implemented": the log said `budget pinned in config` while the config said `target_tokens = 8000`.

Two rules follow. A default that a template config overrides is not a default; check the file the
app actually reads. And the log line has to name the source of the number
(`derived` / `pinned in config`), because otherwise the only difference between "the model says
262k and we chose 186k" and "someone pinned 8000" is a file nobody opens.

## Trap 54: a right-click menu with no CSS — a patch script that died halfway

The menu rendered as a full-width transparent strip of text pinned to the bottom of the window:
`box [0, 876, 1440, 24]`, `background: rgba(0,0,0,0)`. The behaviour was right, the styling did
not exist. The CSS had been written by a script that asserted on a *later* edit in the same file
and raised before its `open(..., 'w')` — so the first edit in that script never reached disk.

A scripted edit that performs several replacements is a code review you skipped: every assert
that fires after a mutation but before the write silently reverts the earlier mutations. Write
per edit, or assert everything up front.

And a component's CSS is part of its contract, so the self-test now asserts it: menu positioned
`fixed`, non-transparent background, width between 120 and 340px. Those three lines would have
caught this, and they cost nothing on a menu that is already being opened for the copy-items
check.

## Trap 55: the menu closed because the *model* scrolled the page

The context menu had `window.addEventListener('scroll', close, true)`, copied from the habit of
native menus. But the transcript scrolls itself to follow the stream, and a capture-phase listener
on `document` cannot tell "the user scrolled" from "the engine moved the page". A menu opened
mid-turn vanished on its own before it could be used.

The menu is `position: fixed`, so it stays exactly where it was opened; scroll no longer closes
it. Escape, a click elsewhere, and window blur still do. The general shape: any listener that
closes a transient surface on a global event has to ask who caused that event, and in an app that
animates its own layout, "everything" is a common cause.

## Trap 56: measuring a collapsed block, and measuring the wrong `.wsrow`

Two harness mistakes in one run, both of which reported a product bug that did not exist.

A settled thinking block is `display: none` unless expanded — that is the feature. A geometry
assertion that asked for its left edge got `0`, and compared against the label's real `350` it
looked like the layout had broken. Expand the block before measuring it; a hidden element's rect
is not a layout claim.

The composer's folder chip was queried with `document.querySelector('.wsrow span')`. The sidebar
also draws `.wsrow` rows, earlier in document order, so the check compared the *host's* folder to
a session it had never opened and failed with "chip shows cwoffee-workspace while viewing
/home/dandandelion/Projects". Scope the selector to the region under test (`.cbox .wsrow span`)
and, when a failure message names two different things, suspect the measurement before the code.

## Trap 57: the harness asserted the old contract after browsing stopped moving the host

`openSession` used to run `switch_session`; the assertion tolerated the host landing on the opened
session or on nothing. Making browsing view-only (Trap 44's sequel) turned that assertion red with
"host moved to neither the old nor the opened session: 670" — which was the new behaviour working.

A behaviour change has to update the assertions that encoded the old behaviour, and the updated
check should assert the *new* invariant instead of merely tolerating it: opening a session leaves
`session_id` untouched, produces no note matching `prompt cache|workspace`, and the composer chip
follows the viewed session's folder. That is three product claims; the old check proved one weak
one.

## Trap 58: `export_markdown` and the workspace question it had to answer

Sharing a conversation writes a file on the host, which raises a question the tool layer already
answered elsewhere: *which* workspace? The host's current one is wrong — another client may be
working in a different folder, and the sidebar row for the exported session names that session's
own. So the export resolves the workspace **from the stored session row**, validates it through
`check_workspace` (which refuses the temp root), and confines the write to `<workspace>/exports/`.

The other details that are easy to get wrong and are now pinned by tests: drive-only (a watch
token must not write files), a nonexistent session id is an error rather than an empty document,
two exports of one session never clobber each other (`-2`, `-3` — the user may have edited the
first), and the document must survive a title that tries to escape (`../../etc/passwd` slugs to
`etc_passwd`). On the rendering side, a run of consecutive assistant rows is one speaker turn:
stored one row per tool call, they used to produce five `## Cwoffee` headings and three
"Tools used:" lists for a single paragraph of prose.
