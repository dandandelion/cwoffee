# Build, run, and the traps in between

Measured on this box (Fedora, Intel iGPU + NVIDIA, llama.cpp on :8090).

## Dev loop

```bash
./scripts/dev.sh                       # vite + tauri dev, GPU policy applied
VITE_AUTOTURN=1 ./scripts/dev.sh       # streams a turn on launch, no clicking
VITE_AUTOTURN_PROMPT='...' ./scripts/dev.sh
VITE_SELFTEST=1 ./scripts/dev.sh       # exercises session create/rename/delete/search end-to-end
./scripts/dev.sh --mock                # same, against scripts/mock-llm.mjs: no model, no prefill
./scripts/selftest.sh                  # the full harness: builds, runs, prints the verdict
```

The self-test defaults to the scripted endpoint (`docs/TESTING.md`). Point it at a real model with
`--real-endpoint URL`, and it will tell you what that costs before doing it.

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
node scripts/gateway-smoke.mjs
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

## Trap 59: "markdown only appears at the end" — the append-only rule read too strictly

The rule *do not parse markdown per token* was written to stop a stutter, and the
implementation obeyed it literally: the whole answer streamed as plain text and one
markdown pass ran at stream end. A 500-word story spent fifteen seconds on screen as
`**The Quiet Frequency**`, and a table was pipe characters until the model stopped.

Blocks, not tokens, are the right unit. `splitCommitted(src)` returns the offset up to
which the source cannot change again — a block ends at a blank line, and an unclosed
fence keeps everything after it unfinished — and each finished block is rendered to
markdown exactly once and inserted before the still-raw tail. Nothing already rendered
is re-parsed, so the append-only property survives; the tail is committed once at stream
end because a final paragraph never gets its blank line.

Two traps inside the fix:

- The rendered nodes go **as children of `.blk-body`**, not into a wrapper div. The
  markdown styles are written `.blk-body > p`, so a wrapper silently unstyles the whole
  answer: no list bullets, no heading sizes, and it looks like the commit never happened.
- The count of *committed blocks* has to be tracked alongside the committed offset. Slice
  from the wrong one and a paragraph is rendered twice on screen; recompute from the
  offset with a stale value (the old code did exactly that, updating the count before
  assigning the offset) and it happens one block later, which is worse because it looks
  intermittent.

`white-space: pre-wrap` moved from the body to the tail span for the same reason: the
parser's own newlines between block elements would otherwise render as blank lines.

## Trap 60: the transcript did not stream, it *lurched* — an autoscroll threshold

"Blinking is worse" was not the letter animation. Filming the user's recording at 20fps
and tracking one line of text frame by frame: it held one position for ~100 frames (five
seconds), then moved **112px**, then 92px, then 46px. The whole answer block was being
dragged up the screen in paragraph-sized steps.

The rule was `if (scrollHeight - scrollTop - clientHeight < 160) scrollTop = scrollHeight`,
called from block-creation sites — not from the paint path. So text accumulated *below the
fold* while the view sat still, and the next event that happened to call the function
teleported the view to wherever the model had got to. Note what the frame diff said: mean
difference 12/255 with 170k changed pixels and *no* shift vector that explained it — a
whole-viewport rewrite looks nothing like a scroll in a naive diff, and the shift search
was the step that identified it.

The fix is boring and continuous: follow on **every** paint (including the ones where
nothing committed — a paragraph that has not ended still grows a line at a time), pin only
while the reader is at the bottom, unpin on their own scroll, re-pin when they come back,
and compensate `scrollTop` by the height delta when a committed block changes the height
under a reader who scrolled up. Measured live: worst step 24px, 0px from the bottom for a
whole turn. The turn-end action row is appended after the last paint, so it follows too —
otherwise the last 30px of a finished turn sit below the fold, which is where the user
looks first.

## Trap 61: the reveal's own stagger built the grey tail it was blamed for

Blur-in was measured working (spans caught mid-flight at opacity 0.2 with `blur(1.7px)`),
and the user still said they could not see it. What they were seeing instead was the
*tail*: with a 300ms duration ceiling and a 168ms per-word stagger cap, a fast model left
the newest line permanently at half opacity while the model was already two lines further
down. A row of grey words that re-darkens as the stream passes is flicker, whatever the
keyframes say.

Duration ceiling 170ms, stagger cap 90ms, and the harness fails if more than 16 spans are
half-drawn at once (a live run measures five). An animation that is provably running is
not the same as an animation that is not in the way.

## Trap 62: the host showed the model a folder the tools were not using

`<env>` — the volatile block naming the workspace, appended to the newest user message at
wire time — was built once from the startup workspace and then *re-installed* from that
snapshot on `NewSession`, `ClearSession` and the draft path. A session stored in
`~/Projects` therefore told the model `workspace: ~/cwoffee-workspace` while `exec` ran with
`pwd` in `~/Projects`. The model, being asked to list files, typed absolute paths into the
folder it had been *told* about and got an empty sandbox in return.

The symptom is unusually clean: the composer chip, the stored session row and the sandbox
all agree, and only the model's own report of its environment disagrees. Ask it to quote
its env block and to run `pwd` in the same turn — the pair is the test.

`HostConfig::env_block` became `env_override: Option<String>`; `None` (every real caller)
means derive from `agent.workspace()` immediately before the turn, after any session
adoption or workspace move. One site, one order — the pinning test captures the wire
message in `Scripted::seen_wire` and asserts it names the new folder *and not* the old one.
The general shape: a volatile block that is refreshed by a snapshot of when the process
started is a stale value with a fresh timestamp.

## Trap 63: committing a paragraph moved the paragraph above it

The block-granular commit (Trap 59) fixed the *content* problem - markdown now
appears while the model is still writing - and left a *layout* problem behind.
Committing removed the paragraph's word spans and inserted the parser's nodes in
their place. Those two representations of the same sentence are not the same box:
the spans sat in a `<span>` with the body's own line rhythm, the parser produced a
`<p>` with block margins, and the difference - one line's worth of blank space -
landed on everything already on screen. The reader watched a sentence they had
finished reading slide sideways-ish a quarter of a line. Reported, correctly, as
"it still blinks a bit while generating".

The fix is to stop treating a commit as a replacement. **Each block gets its own
element while it is being written**, and when its blank line lands the element is
closed *in place*:

- a plain paragraph is already the parser's output - word spans inside a `<p>`
  carrying the parser's own margins - so it keeps every node. Nothing is
  rewritten, nothing re-rasters, nothing moves;
- only blocks the parser would change (`needsMarkdown`: markup characters or a
  block-level opener) are rendered, and each is rendered exactly once.

Two supporting details make the box identical before and after:

- A paragraph's **soft line breaks are joined to spaces by the paint**, because
  the parser joins them anyway. Painting them as breaks reserves a line box that
  vanishes at commit, which is a shift of exactly one line height - the jump
  people were seeing. Lists, tables, quotes and code keep their newlines, because
  there the line structure is the content (`white-space: pre-wrap` lives on
  `.live-para.fenced`, not on the body).
- The closing blank line is painted as spaces, so it costs no box; the margin the
  parser would have added is already there.

Measured in the app's own webview, by identity and by geometry rather than by
screenshot: a committed paragraph keeps **15 of 15** word spans connected, and the
paragraph above it moves **0.0px**. The check fails if a single span is thrown away.

## Trap 64: the live paint needed the *first* block boundary, not the last

`splitCommitted(src).cut` answers "how much of this source is finished" - the end
of the **last** complete block. The streaming paint asks a different question:
"where does the block I am writing end", because it wants one element per block.
Fed the wrong answer, the paint closed one element over *two* paragraphs, which
produced two bugs at once: a paragraph that had already been painted was treated
as unfinished and stayed open across its own blank line, so the next paragraph
merged into it; and a plain paragraph that should have kept its spans went
through the parser because the block it belonged to contained a `*` from its
neighbour.

`firstBlockEnd(src)` answers the real question, and returns `null` while the block
is open. `fenceOpen(src)` answers "am I inside a code fence" separately, because
the boundary function must *not* treat an unclosed fence as a finished block:
doing so rendered half a code block, then started a second one for the remainder,
and the second one lost the language - `data-lang=""`, no preview button, a code
block split in the middle.

The general shape: a helper written for one question is not free to answer a
related one. The names now say which question each answers.

## Trap 65: the reveal's blur re-rastered one word at a time

The reveal animated `filter: blur(1.6px) -> blur(0)`. A filter makes WebKit lift
the word onto its own layer, rasterize it there, and drop it back into the text
layer when the animation ends - so each word **pops** at the end of its own
animation. At the 30-45 tokens per second this endpoint produces, that is a pop
every 25ms, continuously, for the whole turn. Every earlier attempt to shorten or
stagger the animation was treating the schedule of a mechanism that should not
have been running at all.

A transform would have been cheap and composited, but transforms do not apply to
non-replaced inline boxes, and making the spans `inline-block` would change where
the line breaks. So the reveal is **opacity only**. It still reads as words
arriving - 110-170ms, staggered up to 90ms - and nothing re-rasters.

The same trap has a second, less visible half: the duration used to be written as
a custom property on the *paragraph* (`host.style.setProperty('--tin-dur', …)`) on
every paint. An attribute write on a node re-resolves the style of everything
inside it, so the animation's cost was paid by the settled text next door. The
duration now goes on each new span. A `MutationObserver` on the streaming
paragraph fails the self-test if it records a single `style` write - and had to
filter `attributeName` in JavaScript, because WebKitGTK delivers `class` records
even when `attributeFilter` is `['style']`.

## Trap 66: the label was aligned with a padding hack

`.blk > .blk-body.streaming { padding-top: 3px }` existed to make the label in the
gutter sit level with the first line of a streaming answer. What it actually did
was push the first line down *only while the turn was running*, so the name was
misaligned for the whole turn and snapped into place the moment `streaming` came
off - exactly the report: "the assistant label is misaligned while a token is
being generated, then fixes itself at the end".

Deleted. With the first block now being a real `<p>` (Trap 63), the streaming body
and the finished body have the same first-line geometry, which is what the padding
was faking. Alignment is measured in the real webview as the offset between the
*text* boxes of the label and the first line: **-1.7px while generating, -1.7px
once settled**. Measuring element boxes instead of text boxes compares chrome
rather than what the eye aligns - the label carries its own padding and the
paragraph sits inside block margins.

## Trap 67: the measurement watched the wrong element, and pixels lie about scrolls

Two harness traps from trying to prove "settled text does not move":

- `document.querySelector('.blk-answer .blk-body')` selects the **first answer
  block in a long transcript**, which is history. The probe reported markdown
  "already formatted at frame 1" and zero mutations, because it was watching a
  finished turn from an earlier session. Streaming measurements must select
  `.blk-body.streaming`, and be re-attached when the element changes.
- Diffing consecutive screencast frames by a crop taken from page-side geometry
  produces huge false positives: a frame arrives slightly before or after its
  paired geometry sample, and if the view scrolled by one line in between, nearly
  every glyph differs. The honest version searches vertical offsets and takes the
  best alignment - a shift that explains the diff is a scroll, not a redraw. With
  that in place, the only surviving whole-block changes were the two structural
  commits in the turn (a list and a table replacing their raw text), which is one
  reflow per such block, by design.

## Trap 68: an entry animation is a hiding mechanism, and it was running on painted content

"Almost every animated element blinks at some point" went through four suspect lists —
blur, `will-change`, fill mode, easing, WebKit itself — and every one of them was wrong.
The mechanism was found by animating a *solid square* in plain HTML inside a bare
WebKitGTK window and reading real compositor output:

| what the page did | frames with the element fully unpainted |
| --- | --- |
| fresh node, no animation | **0** |
| fresh node + `rise`, fill `both` / `none` / `backwards` | 14, 14, 14 (≈117ms at 120fps) |
| same node, animation class removed and re-added | 13-14 |
| fresh node + opacity-only fade | 17-23 (≈142-192ms) |

Fill mode changes nothing. `will-change` changes nothing. A row of 16 variants —
opacity-only, transform-only, `blur`-only, all combinations, opacity on a raw text node,
plain controls — animated cleanly across 1450 frames: **the engine does not blink.** The
artifact appears when an animation whose first keyframe is invisible is *started on
content that is already on screen*, either by re-creating the node or by re-applying the
class to the same one. While the animation runs the element is composited at the keyframe
value, i.e. transparent. Which means every entry animation in the app is a hide for its
full duration, and the CSS binds entry animations to *structure* — `.turn > *`, `.tin`,
`.pop`, `.filechip`, `.toast`, `.empty` — not to newness.

The consequence is specific and worth internalizing: `.turn > * { animation: cwoffee-rise … both }`
is only safe because `transcript.ts` never rebuilds a `.turn`. Rebuild it once — a `{#key}`,
a keyed-each reshuffle, remounting the transcript — and every row in the turn goes invisible
for 150-260ms at the same moment. That is a whole-transcript flash, and it is invisible in a
screenshot.

So the invariant is now measured rather than assumed. The self-test mirrors every
`animationstart` in the page against the insertion time of every element and fails if an
entrance runs twice on the same node, lands on a node that predates the check, or starts more
than 400ms after its node arrived; it also fails if the live turn's `.turn` element is
replaced or loses a child mid-turn, and fails vacuously if it saw fewer than 10 reveals. It
passes today: 56 entrances across a turn, a popover and a session round trip, oldest node at
start 99ms, 0 replays. In a real turn in the production UI: 593 `cwoffee-tin` starts, 411
ends, **0 nodes animated twice**.

What this also settles: the reveal *is* visible. Per-paint `getComputedStyle` sampling of a
production `.tin` span gives `0 → .04 → .17 → .39 → .59 → .74 → .85 → .92 → .97 → 1` across
10-12 paints at ~60fps (median paint gap 17ms, worst 48ms). If a fade reads as a pop, the
suspicion is frame pacing, not the keyframes.

## Trap 69: the screencast lies — variable frame rate, deduped frames, and an ink threshold

Three measurement traps, all of which produced confident wrong conclusions before they were
caught:

- **GNOME Shell's screencast is variable-frame-rate and dedupes identical frames.** The same
  recorder produced 2.9 fps on a mostly idle app page, 120 fps on a page full of looping
  animations and 26 fps during a model turn. A "frame 61 was blank" reading from that stream
  means nothing until the cadence is known; the container must be asked
  (`ffprobe -show_entries frame=pts_time`), never assumed constant.
- **An ink threshold is not an alpha meter.** Counting pixels below luma 150 in a box makes a
  correct 200ms linear fade read as "invisible for 115ms, then snap", because at 50% opacity
  the text is above the threshold. Measuring mean luma and normalizing it to alpha showed the
  same frames as a clean ramp. Any fade measurement must be a *ratio*, not a count.
- **Frames captured off the compositor are not the app's paint rate.** The page's own
  `requestAnimationFrame` clock (a bar whose width advances one step per paint, decoded out of
  the recording) showed 59.7fps while the recording carried 26fps of samples. Trust the
  in-page counter for pacing and use the film for what actually reached the screen.

When presentation-level truth matters, skip the recorder: `WebKit2.WebView.get_snapshot`
returns the engine's own committed output (a `cairo.ImageSurface`, and note the callback takes
three arguments — `view, result, user_data` — and `SnapshotOptions` is not nullable). Sampling
the production UI at 11.6 snapshots/s for 40s across boot, a full streaming turn and two
session switches found **zero** frames where already-drawn text stopped being drawn. Two
tools that made this cheap and are worth keeping: a `Gtk`+`WebKit2` window that loads any URL
and dumps an in-page probe over the message handler, and an in-page canvas film that draws one
column per paint — a self-recording strip, immune to whatever the recorder does with frames.

## Trap 70: a self-test that asks a local model to cooperate will fail on a good day

`SELFTEST FAIL: no file attachment rendered for a turn that wrote a file` appeared once and
nothing in the app had changed. The check sends "use write_file to create selftest-note.md",
waits for the turn, then looks for the chip. `qwen3.8-flash-next-iq3_s` sometimes answers
"the path is …" **without calling the tool**, and then there is no file, no chip, and a
failure that blames the app for the model's choice.

It now asks the store what actually happened
(`session_messages` → any row with `files`, or `tool_calls_json` naming `write_file`) and
branches:

- file was written, no chip → **fail**, that is the product bug the check exists for.
- no file was written → log `selftest note: the model answered without calling write_file,
  so there was no file to attach; the chip check needs that premise and skipped`.

Two rules fell out of it, and they are harness rules rather than taste:

- **State the premise or drop the check.** Any check whose input depends on a model decision
  must verify that decision from the store before asserting anything about its consequences.
- **Never print "ok" after a FAIL.** The replay watch used to log
  `no entrance replays ok (…)` on the line below its own `SELFTEST FAIL`, which is how a
  harness trains you not to read its last line. Checks now capture `fails` first and log the
  success line only if their own assertions were clean.

Corollary for new guards: run a **negative control** before trusting one. The replay watch was
validated by putting a word on screen, letting it settle 520ms, re-applying its entrance class,
and reading `SELFTEST FAIL: 1 entrance(s) restarted on the same node: span.tin restarted
cwoffee-tin (start #2)` — then removing the control. A guard nobody has seen fail is decoration.

## Trap 71: following on every paint is still a lurch, and the user's own recording proved it

After the reveal, the commit and the entrance replay were all fixed, the report came back as
"the blinking is still there but minimal". The culprit was scroll, and the previous fix to it.

The rule was *pin to the bottom on every paint*, which replaced a 112px jump with what looked
like smooth motion. But a paint happens every `LIVE_RENDER_MS` = 120ms, so the view moved by
however much text arrived in 120ms — two or three lines — **in one frame**. The user's screencast
(`~/Videos/Screencasts/blinking.mp4`, 201 frames at ~26fps, cropped to the window) shows it
exactly: aligning each recorded frame to the one before it by searching for the best vertical
offset, most frames align at offset 0, and then 4.94s, 5.72s, 6.17s and 7.50s align at 66px,
46px, 44px and 40px device pixels — 23-33 CSS px — with nothing moving either side. Stop,
teleport, stop. The residual after alignment at those frames was 12-24 (text arriving), and where
the residual was 0.4 the whole band had moved rigidly: a pure scroll, and a visible one.

The fix is motion instead of a rule: a frame-clocked follow that steps at most 6px/frame, closes
a seventh of any remaining gap, ramps its speed at 6px/frame², and puts the view (rather than
walking it) when the gap is more than a screen and a half — which is a session opening, not text
arriving.

Two things that came out of the first attempt at that fix, both worth remembering:

- **A catch-up snap is its own lurch.** `if (gap > 150) scrollTop = bottom` was written to handle
  a committed table, and the self-test immediately caught it moving the page 204px in one frame,
  because a harness can *also* fall 204px behind. Fractional catch-up with no threshold has no
  discontinuity to hide.
- **One step per frame, whichever caller asked for it.** Paints, tool events and turn-end all
  call `follow()`. If each moves the page immediately, three callers inside one frame is three
  steps, and an 8px step becomes a 24px jump. Events now only *arm* the loop; the loop is the
  only thing that scrolls.

## Trap 72: a free-running sampler measures the check's own scrolls

The follow check sampled `scrollTop` from a `requestAnimationFrame` chain started next to the
feeding loop. It reported `worst frame step 204px` while the actual worst step was 16px, because
a frame queued before `sampling = false` fired during a later `await` and scored the `scrollTop
-= 220` that the check itself performs two statements further down.

Sample inside the chain that drives the input: `await pump()` once per frame, where `pump` reads
`scrollTop` and resolves. Deterministic, one sample per frame, and the numbers matched the
diagnostics in `follow()` immediately.

Related: the check was also feeding text at ~1200 chars/s, eight times what this endpoint
produces, which measured the harness's own queue. 96 characters per ~250ms is the real rate, and
the bound that follows from the design (≤24px in a frame, ≤8px/frame change of speed, newest line
within 220px) is a property of the app rather than of a stress test.

## Trap 73: GNOME 50 renamed the screencast D-Bus API, and 19fps cannot see 50ms of motion

Two things about the recording path on this box:

- `org.gnome.Shell.Screencast` no longer has `RecStart(dir, name, opts)`. It is
  `Screencast(s stem, a{sv} options) -> (b, s)` and `StopScreencast() -> b`; the caller passes the
  output path without an extension, and mutter appends `.mp4`. The client still has to hold the
  D-Bus connection for the whole recording, and the enum is `Gio.BusType.SESSION`.
- The result is variable frame rate: 17.9fps for a mostly-static 62s take, 28fps while the
  composer caret blinked, 26-29fps during a turn. A follow that glides 22px over three painted
  frames (50ms at 60fps) is therefore recorded as *one* frame displaced by 22px — the film cannot
  resolve it. Frame diffs on a VFR recording measure **velocity**, not smoothness: shift ÷ Δt
  between recorded frames, and compare that against the design's velocity cap (6px/frame ≈
  360px/s steady, more during a ramped catch-up). Measured after the change: median 277 CSS px/s,
  p95 575, max 873 while the header row of a new turn was landing.

What the film *can* prove is whether a fade is happening where it should. Normalised alpha
(`1 - mean(luma)/paper`) per frame in the frontier band climbs smoothly from .040 to .063 as a
line fills in, and every apparent "dip" lands on a frame that the shift search explains as a
scroll. In the user-bubble band, alpha sits at .057-.060 across the three frames where the band
moved 15px each: a scroll moves pixels, a blink changes alpha.

## Trap 74: it blinks in the native window and not in the browser, so it was never in the page

Three weeks of in-page instrumentation — animation-start guards, settled-span rewrite counts,
append-only reveal checks, layout-drift probes, rAF deltas — all reported clean while the user
kept seeing a flash. The line that ended it was a comparison rather than a measurement: the same
bundle, served by the same process over the gateway, opened in a real browser, **does not blink**.

Same bytes, same session, same server, different engine ⇒ the artifact is below the DOM. It also
explains why the instruments were right *and* useless: they observe what the page draws. A frame
that the engine produced and the compositor never presented is invisible to every one of them,
and a page-side guard can never fail on it — a guard that cannot fail is decoration.

The engine here is WebKitGTK 2.54.0 on Wayland with accelerated compositing and WebKit's zero-copy
DMABUF frame renderer enabled by default. A/B on identical builds, with the user as the instrument:

| `WEBKIT_DISABLE_DMABUF_RENDERER` | verdict |
|---|---|
| unset (WebKit's default path) | blink on every streaming turn |
| `1` (copy path) | "OMG it's complete gone" |

Product response, in this order:

- `gpu.rs::apply_webkit_mode()` reads `CWOFFEE_WEBKIT_MODE = default | no-dmabuf | no-compositing`
  and sets the WebKit variables **before the webview is created**, because the web process inherits
  the environment at fork; setting them after `WebviewWindowBuilder` does nothing.
- The shipped default is `no-dmabuf`, and `DEFAULT_WEBKIT_MODE` is asserted in a unit test so a
  future cleanup that "restores WebKit's default" has to be a decision.
- An unknown mode is an error, not a silent fallback: `CWOFFEE_WEBKIT_MODE=no-comp` must not run
  the blinking path and look like it was fixed.
- The boot log names the mode (`[cwoffee] webkit render path: no-dmabuf`), because "it blinks on
  my machine" is unanswerable in eight months without it, and `apply_webkit_mode` respects an
  explicitly-set variable (including treating an empty value as unset).

Cost is measured, not assumed free — see `docs/BENCHMARKS.md`: the 60Hz median is identical in both
modes (17.0ms), the p95 goes 18ms → 32ms. A dropped frame is visible; a 32ms paint tail is not.

## Trap 75: the streaming queue is routed by the block kind at *paint* time

Symptom, from a self-test failure that reproduced two runs in four: the markdown table was in the
store, `document.querySelector('.transcript table')` found nothing, the thinking block was empty,
and the answer block began with the model's thinking sentence.

Cause: deltas accumulate in `pending` and one `flush()` per frame joins them and routes the joined
string by `this.textKind` **at paint time**. If the last thinking delta and the first answer delta
arrive in the same frame, the switch to the answer block happens *before* the drain, so the
thinking prose is painted into the answer and the table is glued to the end of it — one paragraph,
no blank line, nothing for the parser to work with, forever literal.

Fix: `push()` calls `flushNow()` at the top of the kind-switch branch, so everything queued belongs
to the block being left. Proven both ways in this engine (a WebKitGTK window loading a scratch
page that replays the collision):

| | thinking block | answer block | tables |
|---|---|---|---|
| with the drain | keeps its sentence | `name value alpha 1 beta 2` | 1 |
| drain commented out | **empty** | begins "The user wants just a table…" | **0** |

The self-test now states the collision directly (`switch drain ok`), with no model in the loop;
the earlier version of this investigation replayed a recorded real turn instead, and that stream
happened to have a benign final thinking delta, so it passed *with the bug present*. A repro that
cannot fail is not a repro.

## Trap 76: "never close a block at a trailing blank line" keeps a table literal forever

`firstBlockEnd` deliberately refuses to treat a blank line at the very end of the stream as a
terminator: the model may still be writing that paragraph, and closing early rewrites settled text.
The consequence nobody noticed: a table that ends at the end of the stream — the common case, since
tables usually close an answer — never commits while streaming and only becomes a table at `turnEnd`,
which is the "markdown appears at the end" complaint in a new costume.

A table is the one block for which the conservative rule is wrong: markdown ends a table at a blank
line, so committing there is exact, not a guess. `firstBlockEnd` now makes the trailing-blank
exception only when the block's first line opens a table.

Do **not** generalise the exception, and do not "fix" `splitCommitted` to match: its trailing empty
slice is the newline the source ended with, not a blank line, and making it a terminator breaks
"a table with no blank line stays in the tail" (which is exactly what happens when rows are still
arriving). Two rules, two functions, one md_test case each.

## Trap 77: a frame-cadence probe measures window management too

The first run of the cadence probe reported `worst 7932ms` — eight seconds with no animation
frame. The page was not frozen; it was **occluded**. WebKitGTK stops the frame loop when its
surface is not on screen, and a locked or covered window produces exactly the gap a jank detector
is looking for.

The probe now records `document.hidden` at the start of each gap, drops gaps that began while
hidden, and prints `off-screen spans N` in the log line so a sample taken off screen is visible
as one rather than as a rendering regression.

## Trap 78: a share that writes into the workspace becomes someone else's output

"Share as Markdown" wrote the conversation into `<workspace>/exports/` and returned an
`OutputFile`, so the UI could show the usual clickable chip. Every part of that was reasonable
until a turn actually followed it: the model, asked to clean up the files from a research turn,
found the export sitting in the folder and reported *it* as one of the things it had made. A file
the human created by pressing a button had become model output.

Two rules came out of it:

- **A share writes nothing.** `share::share_document` returns `{ name, markdown }`; the desktop
  asks a person through the native save dialog (`tauri-plugin-dialog`, `save_file`, whose result
  may legitimately be "cancelled"), and a browser downloads a Blob. Nothing lands in the
  workspace, so nothing can be mistaken for output. `export_markdown` is gone rather than
  deprecated, and a gateway test asks for it by name and asserts `unknown command`.
- **A turn owns its files.** `Transcript.turnFiles` was never reset at `turnStart`, so the next
  `addFiles` in a later turn removed the old strip and re-appended the *previous* turn's list to
  the new one. That is the "the cleanup turn got attached to the files it deleted" report: the
  store was clean (`files = []` on those rows), the live transcript was not. Resetting at
  `turnStart` is the invariant, and the self-test asserts both halves: the downloading turn keeps
  its one chip, the cleanup turn has none.

Note which side the evidence was on. The DB said "no files on the cleanup message" and the user
said "the cleanup message has files" - both were true, of different objects. Checking only the
store would have closed the bug report as a misremembering.

## Trap 79: the live dot re-laid-out the label it was decorating

The answer's name hangs in the scroller's left gutter: `.blk > .blk-head` is absolutely
positioned, **58px wide, `text-align: right`, `white-space: nowrap`**. The "still working" marker
was an inline 6px dot with a 5px margin, *inside that box*. 53px of "CWOFFEE" plus 11px of dot is
64px of line in a 58px box, and **`text-align` has no effect on a line wider than its own box** -
the line falls back to its start edge and runs into the content column. So while the model was
thinking and writing, the label sat flush against the answer's first word, and its gutter
reappeared the instant the dot did at turn end. "Not properly aligned, no margin, fixes itself at
turn end" is that one mechanism.

Fix: the marker is `position: absolute` in the gutter (reasoning keeps it inline, because its head
is a static full-width line). Then the label's box never changes, live or settled. Pinned by
measuring the *text* ranges both while generating and after settling, for a paragraph answer and a
heading answer: gutter must be ≥6px while the dot is up, and must move <1px at turn end.

The general shape: a decoration that participates in the layout of the thing it decorates will
change that thing's layout. This is the same class as the streaming `padding-top` that moved the
label at turn end, and the reveal duration written on the paragraph instead of the span.

## Trap 80: markdown that waits for a blank line is markdown that arrives late

Block-granular streaming commits a block when `firstBlockEnd` says it is finished, and it was
conservative in one direction only: no boundary at a blank line that ends the source. Correct for
a paragraph, which may still be arriving; wrong for the blocks markdown itself finishes with a
newline.

- An ATX heading is one line. `## The Copy\n` is a *complete* block at that newline, and holding it
  for a blank line left literal `##` on screen while the model typed the sentence below it. A
  thematic break is the same.
- A table ends at the first line that is not a row, so that is the boundary - *not* the newline
  that may be followed by more rows, which would split one table into two stacked tables.
- A table's trailing blank line at the end of the stream is a terminator (markdown has no other
  reading), which is why `splitCommitted` and `firstBlockEnd` differ here on purpose.

For emphasis inside a paragraph that is still open, re-parsing is off the table: it rewrites spans
the reader already read, which is the flicker everything else here is arranged to avoid. So
`InlineStream` decides as characters arrive: at `**` it *opens* a `<strong>` into which the
following words are appended, at the matching `**` it closes it, and the marker characters are
never painted. Nothing existing changes - the bold words are born bold, the text after the
construct is appended after it - so the self-test can assert the boring version of the promise:
the word before the construct has not moved, the paragraph element is the same node, and no
asterisk or backtick is visible. It is deliberately narrower than CommonMark: no opener inside a
word (which keeps `snake_case` and `2**3` literal), nothing interpreted inside a code span, and a
marker run at the end of a chunk is *held* rather than guessed, so the asterisks never flash.

## Trap 81: an approval card that quotes a here-document is a wall you cannot read

`summarize_with("exec")` quoted the command verbatim. A model writing a text file through
`cat > notes.md <<'EOF' … 400 lines … EOF` produced an approval card taller than the window, with
Allow/Deny below the fold, inside a fixed strip above the composer - so there was no page scroll
to escape into, and the only way out was to let the approval fail closed on timeout.

Both sides got fixed, because either alone leaves the other half:

- **UI**: the card caps at `62vh` (`74vh` on a short window), the command body scrolls inside it
  with `overscroll-behavior: contain` so wheeling through it does not move the transcript behind
  it, and the button row never moves. The self-test raises a synthetic 300-line prompt and checks
  the pixels: body scrollable, Deny inside the viewport, transcript not moved by the scroll. A
  synthetic event is the point - waiting for a model to write a wall of text makes the check
  depend on a model's mood.
- **Engine**: `cmd_head` shows the first four lines plus `[+399 more lines, 18718 characters
  total] — writes 18694 characters into notes.txt`, and `exec` refuses oversized inline writes
  outright (`oversized_inline_write`: here-doc payload or long redirect over 1200 chars) with an
  error naming `write_file` and the path. It is a legibility limit, not a sandbox rule -
  `write_file` takes far more - and it is narrow on purpose: only a redirection in the command
  *head* counts as a target, so a `python3 - <<'PY'` body full of `>` is not mistaken for a file
  write, and `/dev/null` is not a file.

## Trap 82: `fs::read("/dev/urandom")` is not reading a file

A helper needed sixteen random bytes and used `std::fs::read("/dev/urandom")`. That call
reads **to EOF**, and a character device has no EOF: the Vec grew until the kernel looked
around for the largest other process and killed it. That process was `llama-server`, the
inference the person was actually using. The test suite was not slow; it ate roughly the
whole machine, and the app code path was reached from a unit test, not from the app.

- Read an exact count: `File::open("/dev/urandom")?.read_exact(&mut buf)`. Same for pipes and
  `/proc/*` files whose reported size is a lie in the other direction.
- The rule this broke was already written down: *anything spawned for testing on this machine
  gets a watchdog, because a model already owns most of RAM.* The build/test command itself
  was run without one. `scripts/cargo-guard.sh` now does it: runs the command in its own
  process group, samples the group's RSS every two seconds, and `kill -TERM` then `-KILL` the
  whole group (killing only the cargo parent leaves `rustc` holding the gigabytes) at a
  ceiling. Negative control: a loop that allocates 32 MB every 0.4 s is killed at 329 MB with a
  300 MB ceiling, exit 124. A guard that has never been observed firing is decoration.
- Prefer `cargo check -p <crate> --tests` when the goal is "does this compile". It cost
  532 MB peak and validated the whole test module including new tests; running them is a
  separate decision on a box this full.

## Trap 83: a setting that lives only in the running app is not a setting

The search provider panel had the failure mode every settings UI has: switch the live object,
show the new value, and let the next start read the old one from disk. Two rules came out of
building it:

- **Write the file first, then move the live tool.** The other order means a refused or failed
  write leaves the running app and `config.toml` disagreeing about which provider is on, which
  is indistinguishable from "it saved and then forgot".
- **Edit the file as text.** `Config::set_search_keys_in` replaces the key inside `[search]`
  (and a *commented* one, rather than adding a live twin next to it), appends the section when
  it is missing, and never touches a same-named key in another section. Re-serialising would
  have thrown away the comments people leave in their own config. Pinned by round-trip tests:
  what the panel wrote must parse back to the same backend.
- **The panel must report the engine, not the config it last wrote.** `describe()` and
  `AutoStatus` come from the live tool, so "auto with no container runtime" and "auto with a
  running instance" are different sentences. And when the managed provider fails, the tool
  result itself starts with a note naming the fallback, so the model cannot report "I searched
  the web" about a provider nobody chose.
- **`auto` does not uninstall anything when turned off.** Disabling a feature is not licence
  to delete a container the person may have started themselves; the idle reaper stops it, and
  that is all.

## Trap 84: a second question overwrote the first, and a detached transcript measured zero

Two failures that both came from assuming a thing that had already stopped being true.

**The terminal had one slot for a question.** `ask_user` can now ask 2-4 questions in one
call, and the engine sends them together. The TUI held `pending_q: Option<...>` and
assigned into it, so question two replaced question one and the dropped `oneshot::Sender`
read to the engine as "the user cancelled" - the whole set collapsed because the terminal
had nowhere to put the second question. Any client that keeps exactly one of something must
say what happens to the second one. Now: a `VecDeque` tail, one on screen at a time, a title
that admits more are waiting, and expiry that removes a *queued* question too, so the
terminal never prompts about a call that has already returned.

**The harness measured a transcript that was not in the document.** Synthetic blocks are
painted into the live transcript. When the view was a draft or the empty screen, that
transcript was detached, every `getBoundingClientRect()` read 0, and one slow turn turned
into seven failures about CSS: "expanded thinking block has no width: 0", "answer label is
no longer outdented: head 0 body 0". The premise of a geometry check is not just that the
window is on screen; it is that the element is in the document. Three changes:

- `offScreen()` now treats a *missing* `.transcript` as unmeasurable, and says which of the
  three causes it looks like (empty screen mounted over it, zero-size viewport, no layout pass).
- The run opens a session with history before the layout stretch (`ensureTranscriptMounted`).
- Each geometry check verifies the premise *where it measures* and logs a named skip; the
  summary refuses to call a run clean when 4+ checks were skipped.

The same reasoning applies to the frame clock: a page with nothing streaming is the machine's
own ceiling, so the cadence check now samples that baseline too and, when the box is starved
(a model on nine cores), judges streaming against idling instead of against 60 Hz. Absolute
thresholds on a contended machine measure the contention.

---

### Trap 85: a test suite that spends the user's prompt cache is not a free test

The in-app harness drove the real llama.cpp endpoint. On this box that means one slot and a
~200k-token conversation, so every self-test run evicted a warm prefix cache and then paid to
re-ingest it: minutes of prefill, a cold next conversation, and measured prefill rates in the
telemetry check falling to ~17 tok/s because the harness was competing with whatever the user was
doing. Predictably, the suite started being skipped, and a skipped suite stops protecting anything.

`scripts/selftest.sh` now defaults to `scripts/mock-llm.mjs`: the same SSE wire shape, no model.
Two things it does at the same time, both of which the old setup got wrong:

- **scratch homes.** `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `CWOFFEE_WORKSPACE` and the gateway port
  go into a `mktemp -d`. Probe turns used to land in the real `sessions.db` and be swept afterwards
  by an id floor — a sweep bug was therefore invisible by construction, and one bad run left 40
  sessions in the sidebar.
- **it never `pkill`s your app.** It kills the PID it started; the two instances coexist because
  neither shares a data dir.

The real endpoint is still reachable and still has to be chosen with eyes open:

```bash
./scripts/selftest.sh --real-endpoint http://127.0.0.1:8090/v1   # prints the cost first
```

### Trap 86: a mock that answers anything turns a broken test into a passing one

The first version of a scripted endpoint that "just returns something sensible" is worse than no
mock: the harness would keep passing while the prompt it sent matched nothing it was written for.
Three rules in `scripts/mock-llm.mjs` exist for that:

- **no scenario → HTTP 400 naming the prompt**, and the engine surfaces it as a turn error. The
  engine integration test for this is `an_unscripted_prompt_is_an_error_rather_than_a_plausible_stub`.
- **scenario exhausted → 400.** If the loop asks for step 3 of a two-step scenario, something is
  spinning; answering with prose hides it.
- **`enable_thinking: false` actually suppresses reasoning deltas.** A mock that ignored the flag
  would silently stop testing the flag.

`GET /mock/state` prints per-scenario hits and misses at the end of every run, and *no unmatched
prompts* is the line that catches a harness prompt that drifted.

The mock also caught a real mistake in my own scenario file: `ask_user` options are
`{"label": "...", "note": "..."}` objects, not bare strings. Against the real model I would never
have seen it — the model formats its own options — but the scripted call reported
`ask_user: every option needs a non-empty "label"` as a tool result, and the turn carried on as if
nothing had happened. Which is its own lesson: **a canned scenario's tool arguments are a wire
contract test**, and they have to satisfy the same schema a model's do.

### Trap 87: the harness's own premise-fixing can break the invariant it is checking

`SELFTEST FAIL: opening a session moved the host: 3 -> 4` looked like a regression in view/host
ownership, which is a real invariant (a click must not relocate where the next turn goes). It wasn't.
The check first needs *another session with messages* to open; on a scratch store there isn't one, so
the check now makes one — and making one moves the host. The baseline (`session_id` before opening)
had been captured before that step, so the check compared the host against where it used to be and
reported its own setup as a bug.

Rule: **re-read the baseline after you create the thing the check needs.** Same shape as the geometry
checks measuring nodes their own recovery had detached — a premise fix is a state change, and state
changes move baselines.

Two smaller ones from the same batch:

- **A mock left listening is a mock with stale data.** `selftest.sh` reuses port 8190; an earlier
  manual instance answered health checks while serving the `scenarios.json` it had read an hour
  before, so a scenario added in between did not exist as far as the run was concerned. The script
  now refuses to start if something already answers on its port, and the mock refuses to bind at all
  rather than fail with a stack trace.
- **A scratch store breaks "there is obviously some session around" assumptions.** Several checks
  quietly depended on the user's history being present. With an empty store they have to create the
  state they need, which is slower to write and much harder to get wrong later.
### Trap 88: a markdown marker can arrive as half a marker

Deltas do not respect syntax. A chunk ended with `-` and the space came in the next one,
and the live painter resolved that two-character state as "not a list": it painted the
bullet as text, started a **second** `<ul>` (because the list-continuation test also read
the half line), and the final close re-parsed the remainder into a third structure. The
visible symptom was `the bullet list has 2 items, expected 3` with `ul.live-list ul.live-list`
in the diagnostic.

Rule: when a construct's opener has not finished arriving, the answer is *wait one delta*,
never guess. `maybeListMarker("-")` is true, `listMarker("-")` is false, and the painter
breaks out of its loop until one of them is decidable. Cost: one delta of latency. Benefit:
no syntax on screen, no rebuilt box, no rewrite of settled text.

The same rule in the mock: `firstItemEnd` will not close an item on a half-arrived next
marker.

### Trap 89: a CSS class is not a type, and a viewport rect is not an element's position

Two near-misses in one change:

- `.fenced` means "this unfinished block keeps its newlines". It is **not** "this is a code
  block", and using `host.classList.contains('fenced')` to decide whether inline markup
  applies put a literal `**` on screen: a paragraph gains and loses that class between
  paints, so the guard turned off mid-construct. The honest input is `fenceOpen(rest)`, which
  is computed from the source, not from a class an earlier paint happened to set.
- A self-test that checks "did this element move" with `getBoundingClientRect().top` inside a
  transcript that is **being scrolled by the stream** measures the scroll. It reported
  `the first bullet moved 165.4px - settled text was rewritten` for an element that had not
  moved at all. Compare against a stable ancestor (`li.top - ul.top`), or add the scroll
  offset; never compare viewport coordinates across time in a scrolling view.

### Trap 90: the insertion point is not the text host

`appendReveal` decided whether to run the inline stream with `host === this.inlineHost`.
That comparison was always going to fail the moment a construct opened: `**bold` makes
the insertion point the `<strong>`, while the next paint still hands in the `<li>` or
`<p>` it is writing. The fallback was "paint it as plain text", so the closing marker and
everything after it landed on screen as source — the user's `**What it is**:`.

Two different questions were being asked with one variable:

- **which block am I writing?** — `liveTextHost`, a `<p>` or `<li>`, set when the block's
  tail is created and cleared when the block or item ends;
- **where does the next character go?** — `inlineHost`, which may be nested inside a
  `<strong>`, `<em>` or `<code>` the inline stream opened.

Guard on the first; insert at the second. The negative control is the whole point of this
trap: put the old comparison back and the self-test prints the user's screenshot as text.

### Trap 91: a new field on a stateful renderer needs a reset audit, not a compile

`liveTable` was added to the streaming transcript and worked — until the next answer in
the same session behaved oddly. Per-block live state (`liveTail`, `livePainted`,
`blockStart`, `inlineHost`, and now `liveTable`, `liveTextHost`, `liveList`,
`tailStructured`) is cleared in **two** places: the turn reset and `beginLive`. A field
left out of either survives into the next block, and the next block then paints into a
box the code that owns it has already forgotten. The compiler cannot see this; the only
defense when adding live state is to grep for both reset sites and add the field.

### Trap 92: a check that waits on the store is not a check on the page

A new self-test turn polled `session_state.turns` to decide the turn was over. The store
records a turn before the page has finished painting it, so the next check started
streaming into a transcript whose previous answer block had not closed — and an unrelated
check failed two steps later with `the heading never became a heading`, on a body that
began mid-word (`ods`). Nothing was wrong with the renderer; the harness had interleaved
two turns.

- UI checks wait on the UI's own `turnEnd` (`waitTurnDone`), not the engine's counter.
- When a structural check fails, print the shape of the body over time. The ring of
  `179ms [P TABLE] ods Method Grind Time...` made this diagnosable from one log line;
  "the heading never became a heading" alone sent two full runs past the real cause.

### Trap 93: waiting may delay, it may never drop

Holding back a pipe-led first line until its delimiter row arrives is right (painting it
as prose and then building the table below it shows the header twice). But a block that
was deliberately not painted still has to reach the screen when the stream ends, and the
final path used to return early when there was no live element. A rule that says "wait"
has to be paired with the sentence "and if we never learn more, render it anyway" —
otherwise the fix for a duplication becomes a missing-sentence bug.

### Trap 94: an XML reader that drops entity references rewrites the user's cells

`sheet_read` printed `Header` for a cell whose stored text was `Hea&amp;der`, and
`inline  free` for `inline & free`. quick-xml 0.42 does not fold `&amp;` into the
surrounding text: it ends the text run at the reference and emits
`Event::GeneralRef` separately. A SAX loop that handles `Text` and ignores that
event is not lossy in a visible way - it *edits the data*. Handle the ref, resolve
the five XML built-ins and numeric refs, and leave anything unresolvable written
out as `&name;` so it looks wrong rather than reading wrong. Attribute values are
the other half: they arrive still escaped, so a sheet named `Q&A` needs
`unescape` or the model is told the wrong tab name.

### Trap 95: an assertion that prints `output` prints nothing when the tool failed

Every engine test in `sheets.rs` failed with a blank message. `ToolResult::err`
puts the text in `error` and leaves `output` empty, and the assertions printed
`"{}", r.output`. So a tool that was returning a real, useful error looked
identical to a test framework swallowing the reason. Print *what the tool said*,
ok or not (`fn why(r)` in that module), and the same 20 minutes spent chasing a
parser returns to chasing the actual bug.

### Trap 96: Excel's date epoch has a day that never existed

Serial `1` is 1900-01-01, and serial `60` is 1900-02-29, which is not a date:
Excel inherited the bug and keeps it. Anything at or above `61` therefore sits one
above a naive day count, and the offset is `-25567` epoch days, not the round
number a comment suggests. The failure mode is quiet - every date six days off
looks like a plausible date - so the unit test pins `59`, `60` and `61` explicitly
rather than one modern date. And the fixture written from memory (`45678` is
2025-01-15) was the thing that was wrong: the code was right. Compute fixture
expectations, do not recall them.

### Trap 97: `<mergeCells count="2">` and its two children are the same two merges

OOXML states a count *and* contains the elements. Reading the parent's `count`
attribute and then incrementing per child reports twice as many merged ranges,
which is exactly the number a later edit pass would use to decide a file is too
dangerous to touch. Take the max of declared and counted, never the sum. Same
shape as any format that carries a length prefix next to the payload.

### Trap 98: a paging hint computed from the request loses rows the model never saw

`sheet_read` stops at a cell cap and then tells the model which range to ask for
next. Deriving that range from the *requested* rectangle ("rows 1-4, stopped at
row 2, so ask for row 5") makes the model skip rows 3 and 4 forever, and it will
do it silently. Derive it from the last row actually painted, print that row in
the message (`last row shown 2`), and assert in the test that the hint continues
from the maximum row prefix present in the output. The UI check makes the same
cross-check from the other side, on the tool card.

### Trap 99: `nope` is a column name unless you bound the grid

Range parsing that accepts any run of letters accepts `nope` as column NOPE, and
a model that typed a sheet name into `range` gets an enormous, empty,
confidently-formatted answer. Excel's own limits (`XFD`, 1048576 rows) turn that
into an error naming the format. Boundaries in a parser are where "silently
nonsense" and "an error you can act on" part.

### Trap 100: when the model starts writing its own tool, that is a gap report

A real session asked the agent to describe `01 Retail Inventory.xlsx`. `sheet_read`
worked: structure, then a bounded block of values. Then the model needed a column
total, could not get one, and did something no prompt tweak fixes - it wrote its
own spreadsheet reader: `write_file` 111 lines of Python that unzips the package
and parses `sharedStrings.xml` by hand, then a second 62-line script for the
aggregates. Seven tool calls, ~9k characters of model-authored parser in the
prompt, and two calls wasted first on assumptions (`import openpyxl`, and that
`/tmp` survives between `exec` calls).

The tell is not an error message; it is generated code that reimplements a file
format. A capability gap should be read as a feature request against the tool
surface. The fix here was `sheet_query` - load the sheet into an in-memory SQLite
table and let one `SELECT` answer the question - and the description of
`sheet_read` now points at it, because a tool the model does not know about is a
tool it will re-implement.

### Trap 101: SQLite's progress handler interrupts when the callback returns *true*

`progress_handler(nops, || elapsed >= budget)`. Writing the intuitive
`elapsed < budget` compiles, runs, and never interrupts anything: a cartesian
product over 400 rows (`FROM t a, t b, t c, t d`) sits there until it finishes or
the process is killed. Every deadline guard needs a test that would fail if the
predicate were inverted - the slow-query test exists for exactly that reason.

### Trap 102: "every cell parses as an integer" is not the test for a numeric column

`01234` parses as 1234. A column of codes, postcodes or account numbers coerced to
`INTEGER` loses the thing that made it an identifier, and the loss happens between
the read and any later write, where nobody is looking. Require the digits to
round-trip (`i.to_string() == v`) before calling a column numeric; otherwise it is
`TEXT` and the original bytes stay in the cell. The test asserts `01234` survives
a `SELECT`, next to a test that `SUM(qty)` still works on a genuinely numeric
column.

### Trap 103: the header row is a name, not a record

The first pass loaded row 1 as both the column names and a data row, so
`COUNT(*)` over a 1000-row sheet returned 1001 and a `GROUP BY` grew a
`"category"` group. Off-by-one aggregates read as "the file has junk in it", which
is a complaint about the user's data rather than about the tool. Exclude the header
from the rows, and count what was actually inserted.

### Trap 104: a capability line that is always true is decoration

`sheet_read` reported "an edit here would have to preserve: only the cells" for a
workbook with an AutoFilter, bold white-on-blue headers and a custom `$#,##0.00`
number format. The package scanner only looked at charts, pivots and macros. Now
styles and defined names are counted too - and the test that used to assert
"only the cells" was *wrong*, because a real writer always emits `styles.xml`; the
counter-case (a hand-built package with no styles and no names) became its own
test so the phrase still means something.

### Trap 105: a chip for a file the turn deleted is a button that opens nothing

The session in Trap 100 cleaned up after itself - `rm -rf _xlsx_extract` - and the
transcript still showed two chips for `analyze.py` and `analyze2.py`. Existence is
checked when the turn closes, not by watching for delete events: the tool surface
has no "unproduce" result, and a file the *user* removed mid-turn deserves the same
treatment. Both halves are tested: a file produced twice is one chip, and a file
the turn deleted is none.

### Trap 106: an unset key must stay unset, so "no defaults" has to be tested

Adding sampling looked like a five-line change until the first draft filled in
`temperature: 0.7` as a `Default`. That single number would have changed how every
user's model behaves - on an endpoint somebody else tuned - with nothing in the UI
or the log to say so. `Sampling` is therefore all-`Option`, `Default` is all-`None`,
and the test that matters is the negative one: with no `[sampling]` section the
endpoint must see **zero** sampling keys (`sampling_keys_reach_the_endpoint_only_when_they_were_written_down`,
half 1). A feature whose absence is unobservable cannot be trusted to be absent.

### Trap 107: the endpoint's defaults are not where you looked first

The first `/props` reader took `default_generation_settings.temperature` and printed
nothing, because a current llama.cpp keeps them one level deeper, under
`default_generation_settings.params`. Resolution is now **per key** across
`params` → `default_generation_settings` → root, because servers split the keys
between those places: the mock states `top_p` at the root and `temperature`/`top_k`
under `params`, and both are read. A stated `0.0` is information (`min_p: 0`) and
must not be treated as absence, which is why the reader tests `min_p = 0.0`
explicitly.

### Trap 108: sampling and thinking cannot both be on, and the API does not say so

Anthropic rejects `temperature`, `top_p` and `top_k` when `thinking` is enabled, with
an error that never mentions `[sampling]`. Honouring the config would turn a working
setup into a 400 with no traceable cause, so thinking wins, the sampling keys are
dropped, and the omission is printed **once per process** (`Once`) rather than once
per request: per-request logging buries the log, silence buries the cause.

### Trap 109: a documented key that does not parse is a startup error for everyone

The example config ships `[sampling]` fully commented out, and
`parses_the_example_file` now asserts `example.sampling.is_empty()` - copying the
example must not change how the model behaves. `deny_unknown_fields` on `Sampling`
turns `topP = 0.9` into an error naming `topP` instead of a key that looks
configured and does nothing, and range checks (`temperature 0..=2`,
`top_p`/`min_p` `0..=1`, `top_k >= 1`) run at boot in every shell, not per request.

### Trap 110: a tool-iteration cap of 14 was a work budget, not a guard

The cash-flow session made **exactly fourteen** tool calls - one `ls`, eight
`sheet_read`, five `exec` - and then the engine cut it off and asked the model to
summarise work it had not finished. The user read a summary of a third of a
workbook. A guard stops runaway loops; this one stopped real ones, and it did so
silently, in the middle, with the worst possible artifact: a confident summary of
an unfinished investigation.

`max_tool_iters` is a config key now, and **absent or `0` means no cap**. What
replaced the guard is a line at the iteration where the old build would have
stopped ("15 tool calls so far and no cap set; set `max_tool_iters` in
config.toml to bound a turn"), so a turn that goes on long is visible instead of
truncated. A cap you do set is honoured exactly: the bounded path still forces a
wrap-up, and its note names the number (`tool budget spent (3 iterations)`).

Both halves are pinned in the harness. `long-turn-past-the-old-cap` makes
seventeen calls and cannot complete if a cap exists; its negative control -
putting `cfg.max_tool_iters = 14` back in the app - fails with
`only 14 read_file calls reached the transcript: the loop stopped early`. A guard
nobody has seen fail is decoration.

### Trap 111: the first row with content in it is not the header row

`sheet_query` named its columns from the first non-empty row. In a hand-made
workbook that row is a title, so the cash-flow file arrived as
`burgos_elementary_school | c2 | c3`, the real header became a data row, and
`COUNT(*)` counted the school's name. The tool was not wrong; it was useless, and
the model went back to `unzip`.

Detection now scores the early rows - filled across the sheet's width, text
rather than numbers, distinct labels, data underneath - and **prints the row it
chose** so a wrong guess is arguable rather than invisible:

```
# header: sheet row 3 -> DATE | DESCRIPTION | CATEGORY | INCOME (In) | Expenses (Ex)
# note: rows 1-2 sit above the header (titles, names, dates) so they are not rows of t.
```

`header_row` overrides the detector and a row that does not exist says so with
the row count and column count attached; `header: false` means "no header" and
gives `c1..cN` with every row as data. The detector has a negative control too:
score it out and the self-test fails naming `burgos_elementary_school, c2, c3`.

### Trap 112: "learn the flow" means merges and cross-sheet references, not counts

The same session was asked to learn how a workbook fits together. `sheet_read`
answered `1 merged ranges, 119 formula(s)` - true, and useless: a *count* tells
you nothing about which cell spans what or which sheet feeds which. So the model
unzipped the package and ran `awk` over `mergeCells` and `<f>` text, which is how
it found that the sheets are independent and the linkage is structural.

Structure mode now prints the merge ranges themselves (`merges: A1:F1`), the rows
under a title block (rows 1-3, not row 1), and a workbook-level flow graph
(`"Local Fund " -> "Canteen" (2 formula reference(s))`). Cross-sheet parsing
handles quoted names with spaces and apostrophes (`'Mr. and Ms. Burgos 2026'!B4`,
`'Q''s'!A1`), and deliberately does **not** resolve `[2]Budget!A1`: a reference to
another workbook is reported as one, because following it would mean inventing
data the file does not contain.

### Trap 113: shared formulas have no text to read, so say how many are shared

Of the cash-flow workbook's ~800 formulas, only 198 carry an expression: the rest
are `<f t="shared" si="3"/>`, inheriting text from a master cell elsewhere on the
sheet. A flow graph built from `<f>` text therefore sees the master and misses the
members, and would be wrong if it claimed otherwise. The structure line states the
split - `119 formula(s) (110 shared; they spill past the cell they sit in)` - and
the sweep test asserts that count of `<f>` elements is reported as-is rather than
silently lowered to the number with text.

### Trap 114: a tab-separated grid lies on a wide sheet

`FINAL CANVASSING` is 29 columns wide with four blocks side by side. Reading it as
`A\tB\tC` means counting empty fields, and the model in that session wrote a
script to print `A1='First Canvassing' | B1='' | ...` instead. When the model
builds a format by hand, that format is a feature request.

`cell_addresses: true` prints one line per row with only the cells that have a
value (`1: A1=First Canvassing C1=01/16/2026 E1=Second Canvassing`). The grid
stays the default - nothing that worked before changed shape - and the test
asserts both: cell mode contains no tabs at all, and the plain call still returns
`First Canvassing\t\t\tSecond Canvassing`.

### Trap 115: an eight-sheet workbook must not cost eight calls, but a sweep must say what it spent

Eight `sheet_read` calls is eight round trips of prefill on a single-slot
endpoint, and fourteen calls is what the old cap allowed. `sheet: "*"` sweeps the
visible sheets in one call, splitting the requested budget evenly (`250 cells
each`) and saying so, so nobody reads a truncated sweep as a complete one. Hidden
sheets are skipped with a note naming how to read one - silently including them
would put data the user hid into the prompt, and silently dropping them would lie
about coverage.

### Trap 116: an example binary built before the flag was threaded tests the old code

`sheet_probe` with `cell_addresses: true` printed the tab-separated header and the
first instinct was that the flag was being ignored. It was not: the example
binary predated the change. A probe is only as fresh as its last build, and a
diagnostic that lies about the code under test is worse than no diagnostic. Any
example run that decides whether a change works must be preceded by a build of
that example.
