# Cwoffee

A local-first, BYOM co-work agent for Linux: chat, tools, spreadsheets, PDF, docs, image
input and sandboxed command execution against **your** model — any OpenAI-compatible,
Anthropic-compatible or llama.cpp-shaped endpoint. One process tree, ~230–285 MB PSS, no
cloud, no telemetry. The TUI, which is the same engine without a browser, is **7.0 MB PSS**
([`docs/BENCHMARKS.md`](docs/BENCHMARKS.md)).

Two words about the name. `cwoffee` is the product; `pi` is the model driver it can run on
top of. The repo directory is `pi-cwoffee` because it is the experiment where
[`pi`](https://github.com/badlogic/pi-mono)'s RPC mode became the core and cwoffee stayed the
surface — the plan, the protocol map and the working spike are in
[`PI-CORE-PLAN.md`](PI-CORE-PLAN.md), [`docs/PI-CORE-PROTOCOL.md`](docs/PI-CORE-PROTOCOL.md)
and [`spike/README.md`](spike/README.md).

## The core is a switch

Both shells take the choice, and swapping cores needed no `ui/` edits:

| | pi core | Rust core |
|---|---|---|
| desktop | `CWOFFEE_CORE=pi` | `CWOFFEE_CORE=rust` |
| gateway | `--core pi` | `--core rust` |
| what runs | `pi --mode rpc` behind the same `HostOps` | `crates/engine` in-process |

The **installed app runs the pi core** — that is what the launcher records, because pi is
what this machine already has state for. The Rust engine stays the fallback and the other
half of every A/B: where behaviour is supposed to be identical, both cores are tested
against the same check.

## Install, run, build

```bash
./setup.sh                        # probes the toolchain, prints what is missing, then builds
./scripts/install-desktop.sh      # release build + install: binary, UI bundle, gateway, launcher
~/.local/share/cwoffee-desktop/cwoffee   # the installed app (pi core)
```

```bash
./scripts/dev.sh                  # hot-reload dev build
./scripts/run-pi-app.sh           # desktop on the pi core against a local endpoint
cargo run -p cwoffee-tui          # terminal client, same engine, 7 MB
cargo run -p cwoffee-gateway -- --core pi --port 8199   # remote devices
./scripts/gpu-check.sh            # why a window maps but never paints
```

A debug build embeds no UI; the release build embeds `ui/dist`, so **the UI you run is the
UI you built**. `scripts/install-desktop.sh` refuses to install a self-test bundle unless you
say you mean it (`--allow-test-bundle`), because an armed bundle is inert without the arm and
a "working" app with no checks is the worst of both.

## Configuration

`~/.config/cwoffee/config.toml` (or `--config`, or `XDG_CONFIG_HOME`). Environment beats the
file, and the file is strict: a key the app has not declared is an error at startup rather
than a silently ignored setting.

```toml
workspace = "~/cwoffee-workspace"
mode      = "ask"          # ask | session | auto
persona   = "cowork"       # cowork | chat
reasoning = "medium"       # off | low | medium | high | xhigh
theme     = "light"        # light | dark | system
mid_turn  = "steer"        # steer | follow_up: what Enter does during a turn

[provider]
kind     = "llama.cpp"     # llama.cpp | openai | anthropic
base_url = "http://127.0.0.1:8090/v1"
model    = ""              # empty => adopt the first id from /v1/models

[context]
max_tool_output = 8000

[sandbox]
enabled = true
timeout_secs = 120
network = "proxy"          # off | proxy | open
max_mb_per_call = 512

[search]
backend = "auto"
max_results = 8
```

Also top-level: `sampling`, `providers`, `models`, `max_output_tokens`, `max_tool_iters`,
`tool_exposure`, `approval_timeout_secs`, `question_timeout_secs`, `compact_timeout_secs`.
`api_key = "env:VARNAME"` keeps the secret off disk.

Every environment knob has the `CWOFFEE_` prefix and an older `COWRY_` alias that still
works: `CWOFFEE_URL`, `_MODEL`, `_KEY`, `_WORKSPACE`, `_MODE`, `_CONTEXT`,
`_SEARXNG_URL`, `_APPROVAL_TIMEOUT_SECS`. **The desktop layers these over the file at boot —
any code that re-reads the file must apply them too.** Two bugs of exactly that shape are
pinned by tests; one of them made the app ask the reader's own model server for a title.

A config file left over from a **removed** feature no longer bricks a start: retired keys are
commented out in place, with the reason beside the line. A key that is merely misspelled
still fails loudly, because that one is a setting somebody means to be using.

## Endpoints and models (Settings › Models)

Discover lists what the endpoint serves and what it claims about itself (context window,
tool calling, vision); a probe writes a `[models."provider/id"]` row so the answer survives a
restart, and the settings screen shows where each claim came from. Model rows written by a
probe are the strongest claim available from that machine; an unprobed model falls back to
what its dialect documents, which is stated as the dialect's values and not as a claim about
the server.

**Thinking effort is checked against the model, not against the settings screen.** The host
publishes `reasoning_levels` — the levels this model says it can answer for: pi's own
`get_available_thinking_levels` (a question to the model process, not a call to the model) on
the pi core, probed-or-documented levels on the Rust core. The Settings note then says, in one
sentence, either that the level in the dial is among them, that it is not (*"the dial is set,
but the level is not reaching the wire"*), or that nobody has asked. This is the fix for
"changing reasoning effort does nothing": pi clamps a level onto what the model entry
declares, an entry that declares no thinking answers only `off`, and every surface still
reported the level you had picked.

Settings edits the **default**; a conversation's composer edits **that conversation** and
keeps it for the rest of it. Two different asks, two fields
(`reasoning_default` / `reasoning` + `reasoning_overridden`), and the chip says which is in
force.

## What a conversation is called

New conversations start as **New Session**. Before the first answer, the app asks the model
that is about to answer it for a name — one request, in its own context:

```text
Title this conversation in under ~10 words and say nothing more: <very first user message>
```

No system prompt, no tools, thinking off, never added to the history, never counted in
telemetry. The name types itself into the sidebar row as it arrives (`TitleDelta` deltas are
cumulative). The ask is bounded (8 s): a slow or silent endpoint delays a turn, it does not
end one, and the row falls back to the opening line rather than staying `New Session`
forever. A name you typed into the row menu is never overwritten. There is no second model,
no helper script, no weights to keep warm and nothing to configure — the earlier borrowed
titler and its eight config keys are gone (its removal is documented in
[`docs/TITLE-THINKING-DECODE-DELETE.md`](docs/TITLE-THINKING-DECODE-DELETE.md), because
deleting a feature is a breaking change to every config file written while it existed).

## Where the model's files go

The workspace is the model's working surface; a conversation's folder under it is where its
output belongs. Files the model wants you to have go in
`.cws/assistant_attachments/` and are the only files the transcript offers to **Save…**, which
asks you where to put them. Everything else it wrote is still reachable — a file the turn
*made* is a card you can open, a file it merely *changed* is a line in the working block — but
none of it is offered for handing over: a folder is a decision, a Save control is a delivery.

The model is told this rather than left to guess it: the system prompt says there is *only one
route* to handing you a file — write it under `.cws/assistant_attachments/`, by passing
`place: "attach"`, which puts it there whatever path it names — and that a file left anywhere else
shows up as work the turn touched, with no way for you to take it away. Both cores read the same
sentence, and the tool schema repeats it.

Three places enforce the folder rule, because it used to be enforced in one. The kind used to come
from the tool's `place` argument, so a `download_file` with `place: "attach"` and a workspace-root
or absolute `path` was labelled an attachment while its bytes sat outside the folder — and the card
drew around it, offering a **Save…** the host then refused. Now `place: "attach"` *relocates* (the
destination is always inside the folder, keeping only the file name), the kind is read from where
the bytes landed, and the transcript re-tests the path before it draws the control. The path test is
the engine's own — the `.cws` component followed by `assistant_attachments`, not a substring — so
somebody's `src/assistant_attachments/` directory is still not hand-over material.

### What a file row looks like

Two facts pick the shape, and neither one is a label:

- **Did this turn make it?** A file the turn created is shown as a card — dashed hairline, icon,
  name, size — because it is something the turn produced for you. A file it changed on the way
  through a codebase is a chip in a list, and once there are more than a few of those they roll into
  one line, `38 files written`. The "made it" fact has to come from the tool: `write_file` looks at
  the disk before it writes, and afterwards a created file and a replaced one are indistinguishable.
  A row written before the fact existed renders the way it always did rather than being guessed at.
- **Is it in the hand-over folder?** Only then does the card carry the **Save…** control, the one
  part that promises a copy somewhere you choose. A card without it opens the file where it sits —
  no greyed-out control, because a disabled offer is still an offer the host refuses.

Clicking either one resolves the path against the workspace *that conversation lives in*, then the
folder the app is working in, and a miss names both folders. The old rule ("resolve against wherever
we are now") used to answer `no longer exists` about a file sitting intact in the conversation's own
folder, which is a lie about your disk and not a fact about the file.


## The transcript

- One accordion per turn. All the working — tool calls, files, thinking — folds into a single
  run block at the top of the turn; the answer follows, and your message sits below the
  divider that opens the turn.
- Code and markup blocks render **while the model is still writing them**. The icon buttons
  (copy, fold, render inline) wait for the closing fence; a block arrives folded only if it
  has a rendered form to read. A live fence is written, not typeset: no inline markup, no
  per-word reveal spans in a code body.
- Thinking blocks are inert: a header with a word count, then a duration. The prose is stored
  for the model and never put in the DOM.
- The view follows a growing answer, it does not lurch. Following is a lock, not a magnet: a
  reader who scrolled up is not yanked back down, and a sticky button offers the way back.
- A loop is a note, not a stop: a repeated phrase at constant distance gets one toast naming
  it. Stop remains the only thing that ends a turn. Detection is cheap — no model calls, no
  regex over the whole buffer per token ([`scripts/loop-probe.mjs`](scripts/loop-probe.mjs) is
  its benchmark and its design log).
- One cup for the whole transcript, pinned in a reserved band at the bottom of the pane. It
  fades rather than vanishing, and hides only when the transcript is empty. `reduced-motion`
  gets a still file and a `src` swap, not a style: a picture that already animates is not
  re-animated in CSS.
- A message typed **while a turn is running** is a mid-turn press, not a queued second turn. By
  default it is *steered into that turn*: it goes in after the current step's tool results and before
  the next request, so the model reads it in the turn already running. `mid_turn` in the config file
  makes the other choice — a follow-up, asked after this turn — and the composer says which kind the
  next press is. Either way the line says "not sent to the model yet" until a request carries it;
  `Take back` returns the words while they are waiting, Stop hands back whatever had not landed, and
  a message that landed cannot be unsent (the row stays). What steering does not unlock: branch, leaf
  switch, workspace move, revert and forget still refuse mid-turn, and a press cannot answer an
  approval card ([`docs/MID-TURN-STEERING.md`](docs/MID-TURN-STEERING.md)).
- A conversation has a **tree**, and the card says what is in it: which conversation it was cut
  out of and at which message, what was cut out of *it*, every node of it, and which node the next
  answer reads through. `Tree` is on the message's own control row and in the conversation's row
  menu. Standing the leaf somewhere else copies nothing and deletes nothing — the rows past that
  node stay in the record on a branch that is not being read, the composer says what the next send
  joins, and a message sent there joins that branch. Both cores answer in their own terms: the Rust
  core re-reads the branch from the record, the pi core forks pi's own thread and says which one it
  is now on ([`docs/SESSION-TREE.md`](docs/SESSION-TREE.md)).
- A conversation has a **file ledger**: which turn wrote which path, what it did to it, and what can
  still be put back. `Files` is on the answer's own control row once the turn is stored, and the card
  reads the whole conversation rather than the turn it was opened from — created against modified
  against deleted, a file edited afterwards, a copy the limit discarded, a copy somebody deleted
  outside the app, each said as the thing it is. It keeps two byte numbers apart on purpose: what the
  index rows add to, and what the content-addressed store actually holds, with the bytes no row
  points at named separately. `Forget copies` is drive-only, audited, two presses, refused mid-turn,
  and it leaves blobs another conversation still needs
  ([`docs/FILES-LEDGER.md`](docs/FILES-LEDGER.md)).
- The coworker can **open a page and look at it**. `browse` drives a real browser through a short
  scripted walk and reports the page's own account of itself: what it rendered, the controls on it,
  every console error and uncaught exception its own code threw, requests that failed, and a
  screenshot. Whether that picture reaches the model — and whether a window is painted on your
  desktop while it runs — is decided by what the config file records about *this* model's inputs,
  not by what the model asks for: there is no `headed` argument, and an endpoint that never stated
  its modalities gets a headless run and is told that was because of silence. A `file://` page
  inside the workspace is an ordinary workspace read; one outside it is an escape named by folder.
  The tool is mounted only where a driver actually exists, and the answer when one does not carries
  the two commands that would install it ([`docs/BROWSER.md`](docs/BROWSER.md)).
- There is a **browser panel** on the right of the transcript, and it is a window onto one browser
  the app holds — not a second browser pretending to be the first. The button in the top-right corner
  of the transcript card opens the column, which stands beside that card and ends on its same top and
  bottom lines. With nothing open the column is a page waiting to be written on: `Start browsing`, and
  under it the other way in. The model can start a page in that same browser with
  `browse` and `"session": true`, and what it does there shows in the column with the page's title,
  who opened it, who touched it last and how long it has left. **By default the page in that column is
  a real page this app paints** — an address typed into the field, or a page the model opens, appears
  in the box the column leaves for it, and the person and the model are then working on one document:
  the same url, the same scroll, the same cookies, the same form half-filled. The panel measures that
  box, the app places the page in it, the page says the size it was actually given, and if the box the
  panel asked for and the box the page holds disagree, the column says so instead of letting a page sit
  in the wrong place. The address field takes a host (`example.com` becomes `https://example.com`), an
  address you paste, or a page inside the workspace folder — an absolute path or one relative to that
  folder; a file elsewhere is refused by name, as are `javascript:` and `data:`, because a page box has
  no per-call path approval. What the page said, threw and failed to load is kept as records — console,
  errors with where they came from, requests with their status, files it would not download — readable
  in the drawer and by the model as a tool, not mined out of a screenshot. `CWOFFEE_BROWSER_PAGE=0`
  asks for the other shape instead, described below: a browser in a child process whose pictures are
  relayed. Either way, a page that is already on screen has no picture of itself, and the column says
  "the page is the page" rather than showing a frame that could not disagree with the truth.
  In the relayed shape, while the column is open that picture is **live**: the browser is asked for its own frames as it
  paints, so a page repainting with nothing acting on it — a video, a spinner, a clock — moves on
  screen in front of you, and the panel says *live* only while the driver reports a running stream,
  not because the panel asked. Asking costs the browser work, so hiding the column, hiding the
  window or closing the app stops it; the frames themselves are one JPEG at a time, **sized to the box
  that shows them** — the ask carries the column's width, so a 400px column is sent about 2 KB a
  picture where the page's own width would be about a hundred — and a poll that finds nothing newer
  answers with a number and no bytes. The page does not shrink with the picture: the browser reports
  the viewport the page has, and that is what a press on the picture is scaled against. The column is shaped like a browser — an address field, Back, Forward, Reload, a page, and a
  footer holding the one destructive thing in it — and its controls are icons that carry their names
  twice (an accessible label, and a tooltip that says what the control does rather than what it is
  called). It runs the full height of the window, and the control at its right edge takes the whole
  window for the page: in that mode the page is asked to fill the box it is shown in, so a tall window
  shows a tall page instead of a wide page stranded in a band with slack under it. The picture is a
  control too: clicking it sends a click at the page pixel it stands for, the wheel over it scrolls
  the page under the pointer, and a keys row types into whatever the page already has focus — which is
  why the panel reports the size the browser *has* rather than the size the page was asked for, and
  why the scaling maps a press onto the drawn picture rather than onto the box it sits in. The page's
  own complaints — an error its script threw, a request that failed, a file it would not download —
  are shown rather than hidden behind a screenshot.
  Two rules keep that honest. In the relayed shape the panel is **not** a webview — a picture cannot
  have its own cookies and its own clicks, and cannot look identical while quietly being a second
  browser. In the in-window shape the honesty is the other way round: there is one document, so it
  cannot drift, and what is admitted instead is the **engine**. That page is this system's WebKit, not
  the Playwright Chromium the one-shot `browse` walk uses: no actionability waits, no network
  interception, no screenshots for a model with eyes. A model driving the page in the window reads it
  — text, structure, its own records — and the column says so in its About view rather than claiming a
  live picture it has no way to take. And a `session: true` call is asked for **every time** — "yes to browse" was a decision about a throwaway browser that closed when the
  call ended, not about clicking inside a page you have open. It is ephemeral by construction: it
  closes itself after `[browser] idle_secs` (5 minutes) of nobody touching it — watching it is not
  touching it — the panel counts that
  down, closing the app closes it, and `[browser] keep = false` makes the feature absent and says
  so. Closing it by hand is two presses. A device tab on the gateway can **watch** that browser and
  is told in the same payload that it cannot drive it — the browser belongs to the machine that runs
  the tools ([`docs/BROWSER.md`](docs/BROWSER.md)).
- The **terminal** has the same doors as the window, opened through the same functions: `/branch`,
  `/tree`, `/leaf`, `/files`, `/undo`, `/forget` and `/mcp`, each calling `branch::apply_and_open`,
  `tree::move_leaf`, `tree::read`, `ledger::read`, `checkpoint::preview`/`revert` and
  `ledger::forget` rather than a second implementation of them. It starts the `[mcp]` hub from the
  config like the other shells do, so `explore` there answers about servers that actually exist.
  What it still cannot do: no file **diff** of a stored copy, no attachments, no gateway for
  devices, no settings screens, no chat persona, and no `browse` — and its `/help` and the gate
  both say so ([`docs/TUI.md`](docs/TUI.md)).
- **Theme is a setting, not a preference three windows happen to share.** `theme = light | dark |
  system` in the config file is one value, read at start by the window, a device tab on the gateway
  and the terminal's palette; `set_theme` writes it and is drive-only, because a phone tab that could
  repaint the desktop is the bug this split closes. A window may still carry its **own** override, and
  only the screen that sets it says the word *override* — it never rewrites the stored value, and
  About prints which of the two the reader is looking at. `system` is a subscription rather than a
  palette: each surface follows its own system's colour scheme through a live media-query listener, so
  a phone and a desktop can disagree while both obey the one choice. Light is a theme, not a
  fallback: the floating surfaces — menu, notices, approval card, preview overlay, revert card, tree
  panel, ledger, drawer — wear `--color-raise` plus a per-theme lift, and are graded in **both**
  palettes by two numbers each: how far they sit from the surface behind them, and whether that
  palette's ink is legible on them (under 4.5:1 fails). Never by colour names — a hard-coded dark
  surface passes a separation check beautifully and fails the contrast one, which is the sabotage
  [`docs/THEME.md`](docs/THEME.md) records. A palette change during a turn waits for the turn to end
  and says so.
- The status line reports what it actually measured. Decode tokens/second is **generation
  time**, not turn time: gaps between deltas, each capped at 500 ms, re-anchored after a tool
  run and when a dialog resolves, so a turn parked on a person is never billed as decoding.

## Testing

```bash
./scripts/selftest.sh                    # the whole page-level suite, against the mock endpoint
./scripts/selftest.sh --list             # named groups
./scripts/selftest.sh cup titling delete # a targeted pass (the verdict says what was not run)
./scripts/selftest.sh tui             # the terminal itself: the built binary on its own pty
./scripts/selftest.sh theme --theme=light # the light arm: one run paints one palette, so both take two
node scripts/pi-reasoning-gate.mjs       # pi's own clamping rules, applied to our model entry
node scripts/loop-probe.mjs              # loop detector: cost and false positives on real prose
```

Two rules, both of them hard-won.

- **No automated test may spend your prompt cache.** Suites run against
  [`scripts/mock-llm.mjs`](scripts/mock-llm.mjs) — scripted scenarios, `tests/scenarios/scenarios.json`.
  Anything that reaches `http://127.0.0.1:8090` from a test is a bug, not a slow test; that
  endpoint is somebody's real model.
- **A geometry check needs a page the compositor is painting.** This is a Wayland/GNOME
  desktop, and mutter owns the frame clock: unfocused means no `requestAnimationFrame`, and an
  app asking for its own focus is refused as focus stealing. `WEBKIT_DISABLE_COMPOSITING_MODE=1`
  is set for armed self-test runs only (production never sees it); the harness waits up to 90 s
  for focus, saying plainly that clicking the window is the only lever; a silent clock is waited
  on before any check is allowed to call it blind. Checks that cannot be measured are logged as
  blind notes with their own numbers in them, never graded.

Keep [`scripts/idlekeep.py`](scripts/idlekeep.py) running during a long run (it asks GNOME's
screensaver to stand down; `--check` reports), and end a background process by the pid it
printed — never by pattern-killing a name, and never with an X client that injects input: on
this desktop that is a remote-desktop request and raises a consent dialog.

Cargo runs go through the memory guard on a tight machine:

```bash
CARGO_BUILD_JOBS=1 ./scripts/cargo-guard.sh --max-mb 2600 cargo test -p cwoffee-engine --lib -- --test-threads=1
```

Current counts: engine 682, bridge 65, gateway 54, `svelte-check` 0 errors 0 warnings.
`docs/TESTING.md` is the long version — what each check proves, and the harness bugs found
while getting a suite green.

## Layout

```
crates/engine      the Rust core: agent loop, tools, sandbox, store, titling, telemetry
crates/bridge      the pi core: pi --mode rpc, translation to the same events
crates/gateway     the always-on process phones connect to (token-guarded, see docs/GATEWAY.md)
crates/tauri-app   the desktop shell
crates/tui         the terminal client
ui/                Svelte 5 + TypeScript
scripts/           build, install, probes, mock endpoint, self-test
docs/              protocol, testing, benchmarks, gateway, build/run, work-in-progress notes
```

## Docs

| | |
|---|---|
| [`SETUP.md`](SETUP.md), [`docs/BUILDRUN.md`](docs/BUILDRUN.md) | what a machine needs, and how it gets built |
| [`PI-CORE-PLAN.md`](PI-CORE-PLAN.md), [`docs/PI-CORE-PROTOCOL.md`](docs/PI-CORE-PROTOCOL.md) | the pi core, and the RPC protocol it speaks |
| [`docs/TESTING.md`](docs/TESTING.md) | what the harness proves, and what it refuses to grade |
| [`docs/SESSION-TREE.md`](docs/SESSION-TREE.md) | lineage, the leaf, and why moving one is neither a rewind nor a branch |
| [`docs/MID-TURN-STEERING.md`](docs/MID-TURN-STEERING.md) | typing during a turn: the two promises, the step boundary, and what Stop gives back |
| [`docs/GATEWAY.md`](docs/GATEWAY.md) | devices, tokens, what a phone may ask for |
| [`docs/MCP-SERVERS.md`](docs/MCP-SERVERS.md) | third-party tools: the catalog budget, `explore` as the only door, the per-call gate, and the socket the pi core calls through |
| [`docs/BROWSER.md`](docs/BROWSER.md) | the built-in browsers: the one-shot walk, the kept browser behind the panel, who decides headless versus headed, and why the model is not consulted |
| [`docs/TUI.md`](docs/TUI.md) | the terminal's doors, the pty gate that drives it, and what it still cannot do |
| [`docs/THEME.md`](docs/THEME.md) | theme as a setting: the stored choice and the window override, `system` as a subscription, and the light arm that makes a hard-coded dark token fail |
| [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) | measured memory, token rates, tool-block bytes |
| [`docs/TITLE-THINKING-DECODE-DELETE.md`](docs/TITLE-THINKING-DECODE-DELETE.md), [`docs/UI-TASKS.md`](docs/UI-TASKS.md) | the four most recent asks, with the user's wording kept |

## Third-party tools (MCP)

Servers are declared in the config file, in `CWOFFEE_MCP_SERVERS` as one JSON object, or from the app
itself — the **`MCP servers`** door in the sidebar, just above Settings:

```toml
[mcp]
enabled = true
timeout_secs = 20

[mcp.servers.jira]
command = "jira-mcp"
args = ["--stdio"]
```

```sh
CWOFFEE_MCP_SERVERS='{"jira":{"command":"jira-mcp","args":["--stdio"]}}'
```

Three things follow from that, and they are the point of the feature:

- **Their tools are not in the prompt.** `jira`'s tools are registered and hidden. The model sees a
  short sentence in the environment block saying which servers are up and what they offer, within a
  1024-byte budget for the tool list; the sentence says what was left out rather than presenting a
  cut-off list as complete. Registering a server costs no prompt bytes until someone asks.
- **`explore` is the only door.** `explore` with `need: "mcp: …"` searches the servers and, for the
  tools it returns, cwoffee declares those exact names on the next request of the same turn — the
  tool becomes callable from the *next* call on. A tool nobody looked up cannot be called, so a
  guessed name never reaches a third-party program.
- **Every call asks.** A server is somebody else's program on your machine, so MCP calls are
  approved per call: an "allow for this conversation" you gave `exec` does not cover it, and an
  argument naming a path outside the workspace says so on the card. A server's own
  `readOnlyHint` can lower the bar to workspace-sensitive; it can never make a call harmless.

The **`MCP servers`** page answers two different questions in two different lists, because they have
different answers more often than a merged row can hold: **Running now** is what this app started and
what each one answered, with the tool names it offered spelled out (the catalog is budgeted and can
leave names out, so a count cannot answer "does it have a write tool?"); **In the config file** is what
this app is allowed to start, with a switch, an Edit and a two-press Remove on each row. A switch writes
the file *and* starts or stops the program in the same act, and the sentence that comes back is the
engine's — `written to [mcp.servers.jira] … live (3 tool(s))` — so a file written next to a program that
refused to start is visible as that rather than as a green row. A server added, switched or removed
while the app is open takes effect in this session: the model is not told the tool exists until
`explore` names it, and the withheld list follows the file. If you edit the file in an editor instead,
**Re-read the file** is what makes the running set match what you typed — the app does not watch the
file, and a page that only ever offered its own form would leave a hand edit invisible until the next
boot. Two refusals are said rather than hidden: when `CWOFFEE_MCP_SERVERS` names the servers (it
replaces the file's list wholesale, so writing the file would report a success that the next start
ignores), and on a phone tab, where the programs belong to the machine that started them — the page
says which of the two it is instead of showing controls that would be refused. The search box looks
through server names, commands *and* tool names, because "what did the thing called jira give me?" is a
tool-name question; a filtered list that matches nothing says it is filtered rather than reading as
"you have no servers". An edit form never shows an environment **value** — only the names — and a bare
`KEY` line there means "keep what the file holds", so fixing a command line cannot blank a token that
was never on screen. A server removed while
the app runs stops being callable in that session, not at the next start: the registry drops the
third-party names the hub no longer offers, so the model is never handed an approval card for a program
that is gone.

Both cores mount them, and only the road differs. On the Rust core the hub lives in the app and the
tools call it in process. On the **pi core** it still lives in the app: the tool host there is one
process per call, so a server it spawned would restart on every call and a stateful one would answer
correctly exactly once. The app therefore owns the servers there too and hands the tool process a
unix socket in its own state directory; a call, the catalog, and the status lines go over it, and
nothing else does — that socket cannot start a turn, change a model, read a conversation, or
approve anything. The page prints which road a call takes, so a row cannot read as "mounted" while no
call can get there, and the page itself is the same on both cores because the writes go through the
app, not through the model process. [`docs/MCP-SERVERS.md`](docs/MCP-SERVERS.md) is the whole
shape, including what happens when the socket did not open.

## Not built

Knowledge Center. On the browser side, what is missing is specific: the panel is a **window onto**
the kept browser, not a control surface for it — you can type an address, go back, reload, close, and
press into the page it is showing (a click at a page point, the wheel, a keys row), but there are no
tabs, no bookmarks, no downloads and no page history of the kept browser's own. The panel opens
`http(s)` addresses
only (a page box with no approval card behind it has nothing to answer a `file://` request with), a
device tab on the gateway can watch that browser but not drive it, and the terminal has neither
`browse` nor the panel. Live following of `theme = system` is graded by asking the
engine to synthesize a `MediaQueryListEvent` and reading the paint: on this machine that was
**observed**, and the run's ok-line says so. On an engine that refuses the synthetic event the check
prints a skip note and grades only the listener's presence plus the explicit Light / Dark / System /
override arms, so the claim on this page is conditional in the same way the check is. The terminal has no
**diff** of a stored copy: `/files` lists paths and `/undo` previews and applies a revert, but
comparing a copy against the live file is a window thing. Nothing else described above is unwired:
each is running and covered by a check that fails when it stops.
