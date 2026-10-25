# Cwoffee

> **This clone is `pi-cwoffee`: the experiment where `pi` becomes the core and cwoffee
> stays the surface.** The plan, the protocol map, a working spike, and the real bridge
> (`crates/bridge`) are in
> [`PI-CORE-PLAN.md`](PI-CORE-PLAN.md), [`docs/PI-CORE-PROTOCOL.md`](docs/PI-CORE-PROTOCOL.md)
> and [`spike/`](spike/README.md). **The core is now a switch, on both shells:**
> `CWOFFEE_CORE=pi` (desktop) and `--core pi` (gateway) run `pi --mode rpc` behind the same
> `HostOps` the UI already talks to — same windows, same approval cards, and the core swap
> itself needed no `ui/` edits. Product features have since been added on top of that seam
> (Chat mode, a settings screen, sidebar folding and multi-select delete), so `ui/` differs
> from upstream; the *protocol* did not change to accommodate them. `master` still defaults to
> the Rust engine, which stays the fallback and the other half of every A/B.
> Upstream: `origin` → `../cwoffee`.

*Cwoffee — a coworker that happens to run on your machine. Same warm palette in
light and dark, now with a cup instead of a cow.*

**A local-first, BYOM co-work agent for Linux. One process tree, ~230–285 MB, your model, your machine.**

Chat, tools, and research — Excel, PDF, docs, image input, sandboxed command
execution — against any OpenAI-compatible, Anthropic-compatible, or custom endpoint.

```
Qoder:  ~1.3 GB across 14 processes, cloud
Cwoffee:  230–285 MB PSS, one process tree, local
```

The memory figure is measured, not estimated, on the production build with the
frontend embedded. It is quoted as a range because the total moves with which WebKit
helpers are alive: 87.6–106.7 MB for the app (engine + Tauri + GTK), 114.7–129.5 MB
WebKit web process, 27.5–38.1 MB WebKit network process, plus 8 MB of WebKit's own
sandboxed image loaders in the heavier sample. The same code without a browser — the
TUI — is **7.0 MB PSS**. See [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

> Status: M0–M2 built and running. The engine, both clients, the sandbox, the store,
> document output, image attachments and the token-guarded remote-access gateway are
> live and verified against a real local model. Packaging produces an installable
> RPM and an AppImage. Not yet done: Knowledge Center, browser module, MCP.

## What the pi core answers today

Thirteen tools mounted from the Rust registry (`cwtool specs/check/run`, `pi-tools/cwoffee-tools.ts`
— no policy in the extension), two of them declared by default in **lean** mode with the rest
reachable through `explore` (`tool_exposure = "lean"`: 13,366 B of declarations becomes 1,524 B),
approval cards with the engine's own summary, workspace following,
attachments on the prompt, a context meter with a probed window, a settings screen in five
categories, and **Chat mode**: `persona = "chat"` gives the model nothing — no tools, no folder,
one line of system prompt — and the Cowork tab puts the tools back by restarting the model process
between turns.

```bash
# desktop, pi core, tools mounted
CWOFFEE_CORE=pi CWOFFEE_CWTOOL=$PWD/../cwoffee/target/release/cwtool \
  CWOFFEE_PI_EXT=$PWD/pi-tools/cwoffee-tools.ts ./scripts/run-pi-app.sh
# gateway, pi core, chat from the first turn
cwoffee-gateway --core pi --persona chat --pi-ext pi-tools/cwoffee-tools.ts …
```

Gates that must stay green (each asserts a user-visible fact, not one core's dialect):
`gateway-smoke`, `gateway-approval` (allow and deny), `gateway-workspace`,
`gateway-midturn-switch`, `gateway-excel`, `gateway-search`, **`gateway-chat`** (tools absent in
chat, card back in cowork), plus the in-app harness (`scripts/selftest.sh`) which now covers
attachments, the mode toggle and the Tools list.

### Deleting a folder

A folder row in the sidebar is a *registration*, not the folder's contents — and the sidebar
rebuilds its folder list from any path that still holds a conversation. So the folder menu offers
two different things:

* **Delete folder and its N conversations** — two presses. The count comes from the store, not
  from what the sidebar shows, because a folder group hides chat conversations and empty drafts
  that are filed under that folder's path. Conversations answering right now are left alone and
  called out, and the folder row is dropped only once the store says nothing is inside.
* **Take it off the sidebar** — forgets the registered row. Conversations stay, and the row comes
  back because they are still in the folder. This is stated on the menu item.

Bulk picking works the same way: picking a folder row means the folder *and* every conversation
filed in it, chats and drafts included.

### The settings screen

Settings opens as one screen with five categories in the rail — **Models**, **Workspace**,
**Devices**, **Conversation**, **Appearance** — and nothing else in the rail but those names.
Captions used to sit under each rail entry, eleven of them, which meant the rail was busy
explaining while the thing it is for was finding. The explaining moved into the page, under a
heading, where it is read next to the control it describes:

| Category | What lives there |
| --- | --- |
| **Models** | the model answering now, the endpoints and their models, thinking effort, the context ceiling |
| **Workspace** | the tool set the engine reports, approval mode, the folders tools may work in, web search |
| **Devices** | browsers and phones attached over the network, and what they may control |
| **Conversation** | what a conversation is called, and what this one is using: context, cache, speed |
| **Appearance** | how much of the working the transcript shows, the palette, the keys the app answers |

One thing did not move: the number of decisions. Every control that existed before exists now,
with the same persistence and the same host verbs behind it; the footer popups and the settings
page render *the same* blocks, so there is one approvals control, one ceiling row, one search
panel — not a copy that can drift. The popups are just denser.

The controls share measurements rather than being made to look alike: one height for text fields,
selects, buttons and segmented controls, four levels of emphasis (`primary`, plain, `danger`, and
quiet), the same hairlines and the same focus ring in both themes. Dropdowns are drawn by the app
rather than the toolkit, because a native menu in a webview arrives as a bright white panel in a
dark window — the part people actually look at was the part the design did not reach. A floating
menu gets its own surface token (`--color-raise`) so it separates from the page behind it: deeper
cream on a near-white screen, paler ink on a near-black one, because "lighter" is not a direction
both themes share.

Old links still land. `#search`, `#tools`, "this session", "thinking": the names written down
before this reshape are mapped to the page and section that hold them now, and the app's own page
harness walks all twenty-nine of them, failing if any lands on a page that does not contain the
section it claims. Narrow windows turn the rail into a strip along the top and let a row's control
drop below its text, so the settings screen does not have to be scrolled sideways.

The screen has a size of its own. It used to be `max-height`, so the frame was whatever the page
inside it happened to need: **Appearance** collapsed it and **Models** grew it, and changing page
moved the control you were aiming at out from under the pointer. It opens at a fixed size, opens
upward when you drag its corner, and keeps the size you left it at — stored in the browser's own
storage, because the size belongs to the window rather than to any conversation, and clamped back
into the window on the way out, because a laptop
unplugged from its monitor has fewer pixels than the last opening promised. One size for the whole
screen rather than one per page, because the pages are tabs of the same dialog and a frame that
changes size when you change tab is the behaviour that was complained about.

And the app behind it is out of reach while it is open. The dim is dark enough to read as a wall
and blurs what is under it, and the controls beneath it are taken out of the running twice over:
`inert` on everything that is not the dialog, and a stylesheet rule that switches off pointer
events for the same set. Either half is sufficient on an engine that supports it; the other is
there because a guarantee that rests on a single property is a guarantee that disappears quietly
when that property is unsupported. The self-test checks the promise rather than the recipe: it
puts a click where the metrics button is, and fails if anything other than the dim receives it —
and then fails the other way if the status bar is still inert after the dialog has closed, which
would leave an app you cannot use with no dialog on screen to blame.

### Endpoints and models (Settings › Models)

One model answers at a time. Below that, the screen lists every endpoint the app knows:

* **Test connection** asks the endpoint whether it is there, writing nothing.
* **Probe models** asks what each model can do — the window it states, whether it accepts tool
  calls, whether it takes images, which thinking levels it names back. On a box that is also
  running a model this takes a minute per model; the screen says so while it works.
* **Add this endpoint and N model(s)** registers the endpoint together with the models the probe
  found. A probe is what names the models, so adding an endpoint with nothing probed is refused
  rather than creating an empty row.

Each provider expands into its models; each model expands into fields: max context, output cap,
temperature, top_p, top_k, min_p, seed, thinking levels. Capability is shown as observed, with
"unknown" as its own state — never-asked is not the same claim as refused. An empty field means
"ask the endpoint", and saving it that way *deletes* the key: there is no number that means
unset. Everything lands in `config.toml` under `[providers."<name>"]` and `[models."<id>"]`,
written in place so comments and hand-written sections survive.

What a model's stored window does: it outranks the endpoint's claim, and the context meter, the
derived ceiling and pi's own fold threshold are all rebuilt from it. A model process already
running keeps the window it started with — the next one reads what you saved.



The screen drives the same verbs wherever it is drawn: discovery, add, save and remove go through
the desktop command and through the gateway frame with one spelling, the engine's. Saving one
edited field does not touch what a probe found — `supports_tools`, `supports_vision` and where the
numbers came from are kept unless the request states them, because the form has no field for them
and no authority to clear them. And the file written is the config **this** app was started with,
not whatever `~/.config` happens to hold: a settings verb that reaches for the default path writes
a file the running app never reads — which is how the gateway's own test suite once added a model
row to a developer's real settings file, where the app then cheerfully listed it.

### Where the model's own files go

Every folder cwoffee has ever worked in ends up with the model's leftovers in it: a fetched PDF,
a scratch `helper.py`, three CSVs it parsed on the way to an answer, filed among your own files
and showing up in `git status` like your work.

A file the model wants **for itself** now goes under `<workspace>/.cws/` — a normal, hidden
subdirectory of the workspace, with its own `.gitignore` saying `*` so none of it ever appears as
untracked work. A file you asked for lands where you asked for it. `download_file` defaults to the
scratch folder, and the model has to say `"place": "keep"` to put something in your view; the tool
answers in words either way, so you can see which decision was made:

```
saved 45 B to .cws/reference.pdf (application/pdf)
.cws/reference.pdf is under .cws/ — cwoffee's scratch folder, for files the model fetched for its
own use. If the person asked you for this file, move it into the workspace proper or fetch it
again with "place": "keep".
```

Reference files are kept out of the deliverable strip in the transcript and folded into one line —
`2 reference files` — which expands to chips you can still open. Hidden is not gone: reading them
back with `read_file` works by name, `ls -a` finds the folder, and nothing about the confinement
rules changes (`.cws` is inside the workspace, so a call reaching outside it is refused the same
way as any other escape).

The `.py` half of the clutter is the model's choice of path, so it is a rule in the prompt rather
than a wall in a tool: scaffolding you did not ask for — a script to run, a scratch dump — belongs
under `.cws/`. Nothing deletes `.cws` automatically yet; it is a folder you can empty whenever you
like.

### The working surface, folded

A turn that made nine tool calls used to print nine one-line cards, each with its own triangle,
and the answer you came for sat below all of them. Consecutive thinking and tool calls now share
**one accordion per run**, and the only thing that divides two accordions is the model's own
message text: a run is everything it did between two things it said.

The header is a live claim, not a label — `Thinking` while it reasons, `Working on your computer`
once tools start, and once the answer lands, `Thought, then worked on your computer · 6 steps`.
Open it and the entries are lines: the call, what it touched, whether it worked. Whether those
lines carry their contents is a setting, not a button on every accordion — **Settings ›
Appearance**, in its Transcript section, "Show what each step did" — because the question is
answered once per person, not once per run. While a call is still running its output peeks out on its own regardless, because a
header sitting motionless for forty seconds on a local model reads as a hang.

That setting lives in the app's own store (the same SQLite file as the conversations), not in the
browser's storage, so the desktop, a browser on another port and a phone read the same answer. The
screen renders its list from the app: a setting the app does not act on cannot appear there, an
unknown key or value is refused with the real list named, and a view-only connection cannot change
it — the store is shared, so pressing that switch changes what somebody else's screen draws.

Folding is CSS, not deletion: the arguments, output and diffs of a finished call are still in the
page, so a search finds them and the app's own harness can still assert what a tool said.

**The group is closed by default, and a live turn is no exception.** It used to be pinned open
while the model worked — "the streaming output is the feedback that the app is alive" — and on a
local model that pinned-open group is the biggest object on the screen for a minute, holding a
dozen lines most people never read. The header is what carries liveness now: it changes tense,
and it counts the steps as they land. What still forces a group open is the one block in a
transcript that is addressed to the reader rather than about the model: a question waiting for an
answer. Its header adds `· waiting for you`, and the fold that normally lands when an answer does
is refused while any card in the group is unanswered — hiding the one thing on the screen that is
for you is worse than a long turn.

A question card now sits *inside* the group that asked it, not in the conversation next to it. The
report was that questions and answers "clutter the conversation", and they did: an answered card
stayed a wide open block between the run and the reply for as long as the turn was on screen, and
nothing about it ever folded. The group opens for the question and closes back to exactly what it
was when the answer arrives — including the case where the reader had opened it by hand, which
stays open, because they put it there.

The stored transcript groups itself the same way a live turn does, and two rules keep that honest.
An assistant row with no text in it — the row a tool call leaves behind — paints nothing at all.
It used to paint a `CWOFFEE` label, an empty body and a `Copy | Share as markdown | Branch` line
under nothing, and that bare strip between two accordions is what made a finished turn look
ungrouped the instant it settled. And the footer line belongs to a turn, once, under what the turn
last said; it used to appear under every message.

## From a clone to a running app

```bash
./setup.sh                       # requirements, then the UI and the release binaries
./setup.sh --deps                # install the missing system packages first (asks, unless --yes)
./setup.sh --tests               # + the unit suites, svelte-check, the markdown test, wire-names
./setup.sh --board               # + the page board on both cores (scripted endpoint, no prefill)
./setup.sh --desktop             # + a launcher, verified by launching it and reading the window
./setup.sh --endpoint http://127.0.0.1:8090/v1 --model your-model-id   # only if you have no config
```

One run, from a fresh clone. It probes what the build needs — `pkg-config` modules
(`webkit2gtk-4.1`, `gtk+-3.0`, `libsoup-3.0`, `javascriptcoregtk-4.1`, glib, `openssl`, `xkbcommon`),
the toolchain (`cargo`, `node` ≥ 20, `cc`), and the run-time helpers the app consults (`bwrap` for
the sandbox, `systemd-run` for keeping a gateway alive, `python3` for local titles, `podman` for the
managed search container, `pi` for the pi core, pinned to the version in `crates/bridge/src/process.rs`)
— and prints them in one table, required versus optional, with the package that provides each.
Missing required ones stop it, with the `dnf`/`apt`/`pacman` line that would fix them.

Three things it deliberately does not do: rewrite a config you already have (`--endpoint` against an
existing `~/.config/cwoffee/config.toml` prints a warning and changes nothing), `sudo` anything
without `--deps`, or spend your prompt cache — the checks that talk to an endpoint use
`scripts/mock-llm.mjs`. `SETUP.md` is the record of one machine; this is the check that runs on yours.

## Run it

```bash
# desktop app against a local llama.cpp server
CWOFFEE_URL=http://127.0.0.1:8090/v1 ./scripts/dev.sh

# terminal client (6.8 MB, same engine)
./target/debug/cwoffee --url http://127.0.0.1:8090/v1 --model qwen3.8-flash-next-iq3_s

# config file instead of env vars: ~/.config/cwoffee/config.toml
cwoffee --config /path/to/config.toml
```

Precedence is **CLI flags > environment > config file > defaults**.

Sessions are created lazily: "new session" is a blank view, not a blank row. The
row appears when you send the first message, already titled by it.

Keyboard: `Ctrl+K` composer, `Ctrl+Enter` send, `Ctrl+N` new session, `Ctrl+B`
sidebar, `Alt+1-9` switch session. The `?` in the status bar lists them. Light and
dark themes; the choice persists, and the default follows the OS.

Images go in by picker, paste, or drag — same guardrails whichever way (mime
allow-list, count and byte caps, enforced by the engine rather than the UI).

Gates:

```bash
./scripts/selftest.sh                    # the in-app harness, against a scripted endpoint
./scripts/memory.sh cwoffee-app 400      # PSS, includes WebKit children
./scripts/gpu-check.sh cwoffee-app       # must hold renderD128 only
./scripts/sandbox-probe.sh             # re-run after any bwrap/kernel upgrade
node scripts/gateway-smoke.mjs         # what a phone can actually rely on; mutates nothing
node scripts/gateway-handoff.mjs       # the gateway outliving the window: port, token file, unit env
node scripts/wire-names.mjs            # argument names: the client vs the desktop and the gateway
```

The self-test is the other gate: build the UI with `VITE_SELFTEST=1`, run the binary, and read
the `[cwoffee/page]` lines. It asserts the streaming contract in the real webview — append-only
reveal, layout-neutral commit, markdown before `turnEnd`, follow without lurches, and that no
entry animation ever restarts on content already on screen.

The board starts with `scripts/wire-names.mjs`, which costs a second and earns its keep: the app
calls `invoke('cmd', {...})` and the payload goes to whichever host is running — a Tauri command
or the gateway's dispatch — unchanged. Those two hosts spell multi-word arguments differently by
default, so a settings button can work in a browser and answer `missing required key baseUrl` on
the desktop, which is exactly what happened to the endpoint screen until a page case pressed the
buttons. The gate reads the client's payloads and both hosts' required names, and fails on drift.

Some gates are hosts and start their own scratch gateway and fixture endpoint
(`gateway-explore`, `gateway-recall`, `gateway-scratch`,
`gateway-excel`, `gateway-ask`, `gateway-search-write`, `gateway-prompt-shape`,
`gateway-workspace`, `gateway-chat` with `--core`); others are ordinary clients and need a
gateway somebody else started (`gateway-smoke`, `gateway-approval`, `gateway-search`,
`gateway-chat`, `gateway-attach`,
`gateway-midturn-switch` — point them at the app with `--port 8122 --token DESKTOK`, or at a
scratch gateway; `gateway-search` reads the panel of a gateway that is already running, so
pointing it at a port nothing answers prints `backend=undefined`, which is a broken command
line rather than a broken app). One rule holds for both kinds: they assert what the model, the transcript or the
filesystem can show, never what a log line claims, because an extension's own console output does
not reach the parent process at all.

The board needs its window visible. WebKitGTK stops compositing an occluded window, and the
harness says so loudly — zero animation frames, zero block entrances, geometry checks skipped —
rather than grading a page it cannot see. If the product app is running at the same time, its
window will be the one on screen; stop it for the length of a board.

It runs against **a scripted endpoint by default**, not your model: `scripts/mock-llm.mjs` answers
from `tests/scenarios/scenarios.json` in llama.cpp's wire shape, so the SSE parser, tool loop,
repair, approvals and telemetry are all exercised while your 200k-token prefix cache sits untouched.
It also gets its own config dir, data dir, workspace and gateway port, so a test run leaves nothing
in your sessions list. The real endpoint is opt-in and says what it costs:
`./scripts/selftest.sh --real-endpoint http://127.0.0.1:8090/v1`. See
[`docs/TESTING.md`](docs/TESTING.md).
## Devices, and the gateway that stays up

The footer pill turns the gateway on: `:8099 · 2 devices`, loopback or LAN, token in
`~/.config/cwoffee/gateway.token`. Settings > Devices adds one switch that changes what happens when
you *close* the window: **"Keep devices working after this window closes"** — off by default, because
a gateway that answers after the window is gone is a door you did not know you left open. On, the
port is handed to `cwoffee-gateway`, a separate process with its own engine reading the same store
and the same token file; the phone reconnects to a fresh host with the same conversations, the model
and folder as they were at close, and no in-flight turn. The row says which binary would start, on
which port, and how to stop it (`systemctl --user stop cwoffee-gateway-8099`) — including the case
where it cannot be honoured because no standalone gateway is installed.
[`docs/GATEWAY.md`](docs/GATEWAY.md) has the mechanism, the systemd traps, and the check
(`node scripts/gateway-handoff.mjs`).


## Docs

| File | What |
|---|---|
| [`PLAN.md`](PLAN.md) | thesis, architecture, v1 scope, milestones, memory budget, perf rules, risks |
| [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) | every measured number and the command that reproduces it |
| [`docs/TESTING.md`](docs/TESTING.md) | what each suite covers, the scripted endpoint, and when to spend a real prefill |
| [`SETUP.md`](SETUP.md) | what's installed, what still needs `sudo`, endpoint details |
| [`config/cwoffee.toml.example`](config/cwoffee.toml.example) | providers, models, context budget, sandbox policy |

## Layout

```
crates/engine      the product — agent loop, providers, tools, storage (no UI knowledge)
crates/tauri-app   thin desktop host (links the engine in-process)
crates/tui         ratatui client — the permanent dev/debug harness
ui/                Svelte 5 + Tailwind v4
scripts/           memory.sh (the PSS gate), gpu-check.sh, sandbox-probe.sh,
                   gateway-smoke.mjs, check-docx.py
```

The engine has no idea a UI exists. Both clients drive one `SessionHost` over
channels, which is also the seam for a future daemon/remote mode.

## The three rules that make it fast

1. **Append-only, byte-stable prompts** — so the server's prefix cache always hits.
   Measured: 92% cache hit on turn two, 452 tok/s effective prefill against 38 tok/s raw.
2. **Imperative transcript, never a reactive list** — deltas become text nodes
   batched once per animation frame. 0 dropped frames over 209 frames at 50 tok/s.
3. **Markdown is parsed once per block, not once per token** — each block gets
   its own element while it streams, and is closed *in place* when its blank line
   arrives. A plain paragraph keeps its word spans and its box, so committing one
   moves nothing; only blocks the parser would change are rendered, once.

## What the status bar actually measures

The server reports token counts only when a turn finishes, so a bar bound to those
numbers sits at em-dashes through the entire turn the user is watching. The engine
therefore emits `TurnStats` a few times a second while a turn streams: TTFT the
moment the first delta lands, then a character-derived token rate, marked `≈` in
the bar and replaced by the server's own numbers when `Usage` arrives.

Measured against llama.cpp on this machine: live estimate 30.3/s then 28.0/s,
server-reported 28.2/s. The estimate is close enough to act on and is labelled as
an estimate rather than presented as a measurement.

The footer does not spell every number out: one **metrics** button carries the two
that explain most latency (cache hit, token rate, context used against the
endpoint's stated ceiling) and opens a popover with the rest — prefill effective
and raw, decode now and session average, first-token time, turn length,
speculative acceptance, tokens in and out, compactions, frame drops, IPC and GPU
state. Context gets a bar because "1,432" on its own answers no question.

Next to it sits the **gateway** pill: off / on with the port and a live device
count, and a panel that turns remote access on and off, chooses loopback or LAN,
and lists copyable URLs. `docs/GATEWAY.md` explains what that costs in security
terms, because it is the one control in the app that widens what a stranger could
reach.

That scope control is one switch, not a sequence. It used to show the *setting* rather than
the *bind*: after a start on 0.0.0.0 the segment still read "This machine", so "Any device"
looked pressable, did nothing, and the panel seemed broken until the other half had been
clicked first. It now shows what the running listener actually does — 127.0.0.1 or 0.0.0.0 —
and decides whether a click means anything against that, so the order of the two halves is
not part of the interface.

The bar also fits the window it is given, and it asks its *own* width rather than the
screen's. The report: "the footer items are too tiny and too long — on my phone the theme
and help buttons are off-screen". The row never wrapped and nothing in it could shrink, so a
narrow window cut it off after whatever was added last, and what was added last was the theme
toggle and the keyboard help — the two icons a person reaches for on a phone. Below 660px of
*bar* it is thumb-sized (34px controls in 46px of height); the two pills whose entire summary
is an address or a port lose that summary, because their panel carries it; the metrics pill
keeps its numbers, because those are the reason it exists, and ellipsises instead of pushing
its neighbours out of the window; and the diagnostic that reports the app's own round-trip
time goes away, since nobody reads it on a phone. Below 470px even the words "on" and "off"
cost too much and those pills keep their dot alone. Keying this to the bar rather than to the
viewport is not tidiness: a 700px-wide desktop window has the same problem as a phone and, key
to a viewport media query, got none of the fix — and it is the only way the self-test can
prove the rule, by narrowing the bar in place and asking whether every control is still
inside it and still big enough for a thumb.

### Thinking levels, measured

The picker also shows what the endpoint says about each model rather than a bare
id: context window, input modalities, tool calling, slots, and the model file and
server build when the server reveals them. Everything is optional and the UI says
"not reported" when the endpoint is silent — `unknown` is never rendered as `no`.
Tool support is the one inferred value (there is no capability flag anywhere; it is
detected from the served chat template), so the row carries a `*` and a footnote
saying so.

Thinking effort used to be one bit on llama.cpp (`enable_thinking`). It now sends
the template's own `reasoning_effort`, and at `high`/`xhigh` the system prompt adds
one line that this model actually responds to: reasoning went from 603 to 1426
characters on an identical question. It does **not** reach the 40k-token marathons
the server's own web UI produces, and the reason is in `docs/BENCHMARKS.md`: on this
checkpoint, *any* system prompt collapses thinking from "runs to the token cap" to
about 250 tokens. The web UI thinks more because it sends no system prompt at all.

### How much context is allowed

Compaction is what keeps prefill cheap on a local model, so the budget that triggers it
matters: too low and the app throws away history it did not need to, and tells you the
context is full when the model has room for six times the transcript.

Unpinned, the budget is derived from the window the **endpoint advertises for the loaded
model**: **half of it**, floored at 4096 - and it is re-derived whenever you switch models.
On this machine that is 131,072 tokens against a stated 262,144, and a 43k transcript sits
comfortably inside it. Half, rather than 72%: the other half is the answer's room. A prompt
that fills the window leaves the model nowhere to reply, and on a local endpoint the cost of
getting that wrong is a truncated tool call in the middle of a long turn. The same derivation
gives the generated model entry its `maxTokens` (never below pi's 16384 default, never above
the window minus the prompt), and an endpoint that states no window gets no claimed number. Pin `context.target_tokens` in the config only to keep
conversations short on purpose; a pin is a decision, and the engine honours it and says
"budget pinned in config" in the log rather than silently preferring its own arithmetic.

The old default was a hardcoded 8000 regardless of model, which compacted a 43k conversation
on a 262k model and reported "context full". That is why the number now comes from the server.

**You can change it without editing a file.** The telemetry panel (footer → *Session
telemetry*, and the same body under Settings › Models) has a **ceiling** row: it shows the number in
force and says whether that is *pinned by you* or *derived: half of 262,144*. A box pins a
new one, and **derive it** takes the pin off again — which is not a number, so freeing the
setting deletes the line from `config.toml` instead of writing a sentinel into it. The file
is written before the setting is applied, so a ceiling chosen on a phone is the ceiling the
desktop shows after a restart. Both cores refuse the same two nonsense values in the same
words: under 4096 tokens, and anything at or above the window the endpoint stated (a ceiling
the endpoint would undercut first is a gauge that can never fill).

On the pi core the model is told where the line is too. pi folds when its running count passes
`window − compaction.reserveTokens`, and its default reserve (16 384) was chosen for coding
sessions; cwoffee's meter filled against its own ceiling. Now cwoffee writes
`reserveTokens = window − ceiling` for *this model* into its own agent dir, merging with
whatever `settings.json` already holds and refusing to touch one it cannot parse. The boot
line states it:

```
[cwoffee/pi] pi will fold above 131072 of 262144 tokens (… compaction.modelOverrides["cwoffee/qwen3…"].reserveTokens = 131072)
```

Before that, the meter and the fold were two different numbers, and which one you believed
depended on which panel you opened. A model process that is already running keeps the
threshold it started with — pi reads its settings when the process starts — and the note says
so rather than implying the change is live inside a running process.

### What the model is actually told

Prompt-size claims used to be arithmetic about what each layer composes.
`scripts/gateway-prompt-shape.mjs` reads the request body an endpoint actually receives
instead: it starts an endpoint that logs the body and answers as a stream, points a scratch
gateway at it, runs one cowork turn and one chat turn, and prints the system text, its
sections and the byte cost of every tool declaration. Measured on this box:

| core / mode | system text | tool declarations | payload | ≈ tokens |
|---|---|---|---|---|
| pi core, cowork, full exposure | 1,103 B (`cwoffee-v2`) | 13 tools, 13,366 B | 14,469 B | 3,617 |
| pi core, cowork, **lean** exposure | 1,587 B (+ the discovery note) | 2 tools, 1,524 B | 3,111 B | **778** |
| pi core, chat | 233 B | none | 233 B | 58 |
| old core, cowork | 1,154 B | 13 tools, 13,366 B | 14,520 B | 3,630 |

Three things that table says, each invisible before anyone read the bytes:

* **Chat mode really is empty.** `--system-prompt` replaces pi's default prompt (the additive
  form is `--append-system-prompt`), and `--no-tools` covers extension tools as well as pi's
  own four, so the tools section is absent and the whole instruction is one line. pi does
  still append its own `<cwd>` block naming the workspace folder — 41 bytes, but a folder claim
  in a mode that has no file tools on purpose — so the chat prompt says outright that the mode
  has no tools and no folder to work in.
* **Cowork on the pi core used to speak with pi's voice, not cwoffee's.** The preamble defined
  the job as "an expert coding assistant operating inside pi", and 1,478 bytes of the prompt was
  pi telling the model to read pi's own docs and examples — files that are not in the user's
  workspace. The pi core now sends cwoffee's own 1,103 bytes, the same text the old core sends.
* **The declaration block, not the instructions, is the expensive part.** `ask_user` alone is
  2,107 B; `exec` 1,904 B; `sheet_query` 1,630 B; `sheet_read` 1,571 B; `download_file` 1,218 B;
  `write_xlsx` 758 B; `edit_file` 804 B; `explore` 853 B; `write_file` 672 B; `recall` 671 B;
  `read_file` 535 B; `web_fetch` 339 B; `web_search` 304 B. On a small local model that is real
  prefill on every single turn, for capabilities most turns never use.

### Which tools the model is shown (`tool_exposure`)

`tool_exposure = "lean"` in the config (or `CWOFFEE_PI_TOOL_MODE=lean` in the environment)
declares two tools and hides the rest: `explore` and `recall`, which is the set the Rust
registry names — `cwtool specs` answers `lean` and `discoverable`, so neither the extension nor
the UI holds a list of its own. What is withheld is the *declaration*: every mounted tool stays
active and callable for the whole session, and the system prompt gains one short note saying the
menu is a subset and that `explore` can name the rest. When `explore` answers, the tools it found
keep their names callable immediately and their parameter schemas come back into the prompt; the
host signs for that in the tool result rather than in a log nobody reads.

The saving, measured by the gate above: 13 declarations and 13,366 bytes become 2 declarations
and 1,524 bytes, and the first payload of a cowork turn drops from ~3,617 tokens to ~778 — with
the same thirteen tools reachable, because `read_file` discovered mid-turn still runs in that
turn (proved by `scripts/gateway-explore.mjs`, which fails if the discovered tool's own output
never arrives).

Full exposure is still the default. Lean is the option for a small local endpoint where those
11 KB of declarations are the difference between a fast first token and a slow one.

The gate is a gate, not a report: it fails if a chat declares tools, if cowork carries a coding
harness's preamble, if lean declares more than three tools, or if lean's declarations still cost
a kilo of bytes. It also prints the *timing* between requests, because the first version of that
endpoint answered with a non-streaming body and pi re-sent the same request four times at 2 s,
4 s and 8 s. A "the app quadruples prefill" finding that was really a probe bug is exactly what
the timing column is there to make impossible.

### What a conversation is called

A conversation is created as **New Session**. Before it answers its first message, the model
behind Cwoffee is asked, in one separate request of its own:

```text
Title this conversation in under ~10 words and say nothing more: <the very first message>
```

no system prompt, no tools, thinking off — and the name **types itself into the sidebar row** as
it arrives, caret and all. The first real answer starts after that. The stall is the trade: a
name that appears forty seconds late, on a page that has already moved on, is not a name.

There is no second model. The titler that used to do this — a python helper loading a
230M-parameter model on this machine, `titlegen = true`, 861–864 MB peak per new conversation,
naming the row *after* the turn had finished — is gone: script, config keys, wire verb and
Settings cost sheet.

Three rules survived the rewrite, and each one has a test:

* **A name you chose is never overwritten.** Only a row still carrying the placeholder (or one of
  the placeholders older versions used, including the opening-line cut) is ever offered to the
  model.
* **Separate contexts.** The title request is one user message: no system prompt, no tools,
  thinking off, and it is never appended to the conversation — so it cannot inflate a later turn
  and cannot disturb what the server has cached of the real prompt.
  `titling::the_ask_is_one_user_turn_with_no_system_prompt_and_thinking_off` asserts the request
  shape; `host::a_new_conversation_is_named_before_it_is_answered` asserts the order and that the
  ask never becomes a row.
* **A miss is not fatal, and not silent.** If the endpoint returns nothing usable — silence, a
  timeout, or a name left inside a thinking block by a model that ignored `enable_thinking=false`
  — the parser reads the thinking block too, and if there is still nothing the row falls back to
  your opening line rather than sitting on "New Session": a sidebar of identical rows says
  nothing. The reason goes to the log.

Both cores run the same code (`crates/engine/src/titling.rs`), so a conversation gets the same
name whichever core is behind the window.

### When the model runs out of tool budget

A turn may keep calling tools; the loop is bounded so a confused model cannot spin forever.
When the bound is reached, the engine does not stop mid-sentence and log a warning - it makes
one final request with **no tools offered at all** and asks for an account of the work: what
it did, what is unfinished, what it would do next. That summary streams and is stored like any
other answer.

The instruction that asks for it is transient: it goes into that one request and is never
pushed into the transcript, so the user does not get a fake message from themselves and the
append-only prefix stays intact. If the summary comes back empty the turn still ends with a
visible note rather than silence.

### What sampling the app sends (and what it deliberately does not)

Cwoffee sends **no sampling parameters unless you write them down**. `temperature`,
`top_p`, `top_k`, `min_p` and `seed` are all absent from a default request, so the
server behaves exactly as it does for any other client. That is not laziness about
a tuning knob: sampling decides what the model does, and a harness that sets
`temperature = 0.7` because it "seems sensible" changes the product invisibly, on a
codebase whose whole prompt strategy assumes the model behaves as it did yesterday.

The section exists so you *can* set them, and the boot log states both halves of
the answer - what this app sends, and what your endpoint says it uses when nothing
is sent:

```
[cwoffee] sampling: nothing set, the endpoint decides | endpoint defaults: temperature=1 top_p=0.95 top_k=20 min_p=0
```

Those defaults are read per key from wherever your llama.cpp build puts them
(`default_generation_settings.params`, `default_generation_settings`, or the root of
`/props`), and unstated keys stay unstated rather than becoming our numbers.

Why bother with the section at all: llama.cpp's default temperature is 1.0, which is
a fine creative setting and a poor one for tool-call JSON across a long agentic turn.
The symptom is far from the cause - repaired tool arguments, or a run of repeated
characters at the end of an answer. `[sampling]` with `temperature = 0.7`,
`top_p = 0.9`, `min_p = 0.05` is the shape that behaves well on Qwen-class models,
and `seed` is there so a tool loop can be reproduced while you debug it. Out-of-range
values (`temperature = 11`, `top_k = 0`) and misspelt keys stop the app at boot with
the key named, instead of being quietly ignored.

One asymmetry worth knowing: on Anthropic, thinking and sampling are mutually
exclusive - the API rejects `temperature`/`top_p`/`top_k` when thinking is on.
Thinking wins and the omission is printed once per process, because a 400 whose text
never mentions `[sampling]` is a worse diagnosis than one line in the log.

### How long a turn may run (`max_tool_iters`)

A turn keeps calling tools until the model stops asking for them. There is no
count it stops at unless you write one:

```toml
max_tool_iters = 40   # hard stop; 0 or absent means no cap
```

The default used to be 14, and it was described as a runaway guard. It was not
one - it was a work budget wearing a guard's clothes. A real session on an
8-sheet workbook spent all fourteen calls reading sheets, at which point the
engine cut the turn off and asked the model to summarise work it had not
finished. The user read a summary of a third of a spreadsheet; the model had
been two calls from the answer.

So the cap is a knob, uncapped by default, and a turn that goes past where the
old build would have stopped says so once:

```
15 tool calls so far and no cap set; set max_tool_iters in config.toml to bound a turn
```

That line is the guard now: you find out a turn is long by seeing it is long,
not by having it taken away. Cancel still works at any iteration, and a cap you
*do* set is honoured exactly - it forces a wrap-up instead of running out.

### Motion

Everything moves on one curve (`cubic-bezier(.2,.7,.2,1)`) with three durations, and
each transition answers a question: a new block eases in because it just arrived, a
tool card expands rather than snaps because you asked it to, the metrics bar slides
because a value changed, the send button's ring appears because you are about to
press it. `prefers-reduced-motion` turns all of it off. The one looping animation in
the app is the steam on the empty screen's cup.

Streaming text **arrives** rather than appearing. Each word that lands since the last
repaint becomes its own inline `<span>`, fades in from zero on opacity alone, and is **never
touched again**. Nothing about that word is re-laid-out, re-parsed or re-animated for the rest
of the turn: the block is append-only, exactly like the prompt that produced it. Duration is
not a constant either — it is derived from the observed word rate (`600/words_per_second`,
clamped to **110-170ms**) and the stagger across one arrival is capped at **90ms**.

Those two caps are not taste. With a 300ms ceiling and a 168ms stagger, a fast model leaves
a trailing row of words permanently at half opacity: the newest line is grey while the model
is already two lines further down, and the eye reads a grey tail as flicker. Frames from a
user recording showed exactly that, and a live run now measures at most five spans in flight
with blur still on them.

The unit is the **word**, not the chunk. Earlier designs animated whatever arrived since
the last paint, then folded finished chunks back into a re-written paragraph. Folding is a
rewrite, and a rewrite re-rasters: every already-settled word blinked again roughly half a
second after it landed, which reads as the animation "playing catch up" with the text. The
append-only version has no fold step to blink in.

**Markdown arrives with the answer, and committing it moves nothing.** A paragraph is closed
the moment its blank line lands: headings become headings, `**bold**` stops being asterisks,
lists become lists, all while the model is still talking. Only the last, unfinished block
stays raw text, and it is closed once at stream end. That is one markdown pass per block —
never one per token — and blocks that are already rendered are never re-parsed, which is the
same append-only rule the prompt builder follows.

The block being written has **its own element**, which is what makes the commit free: a plain
paragraph of word spans inside a `<p>` is already what the parser would have produced, so the
element just stops being "live" and keeps every node. A paragraph with markup in it, a heading,
a list, a table or a fence is rendered once, in place. A paragraph's soft line breaks are joined
as they are painted, because the parser joins them anyway — painting them as breaks reserves a
line box that disappears at commit, and that one-line collapse is the "blink" a reader sees two
paragraphs later.

The frame-by-frame version of the bug this fixes: a 500-word story streamed for fifteen
seconds with `**The Quiet Frequency**` on screen the entire time, and `#` markers and pipe
tables visible until the turn ended.

What the harness proves, in the app's own webview rather than in a screenshot: 28 word spans
for four streamed reasoning deltas, every one of them still connected to the document after
later paints, their `textContent` concatenating back to exactly what the model sent, and no
span block-level. That is the whole contract — an animation that is invisible in the computed
styles but rewrites settled text is still a bug, and a check on node identity catches it.

Three more checks sit next to it. A streamed answer must contain a real `h2`, a `strong` and
an `li` **before** `turnEnd` while its tail is still raw text. A committed plain paragraph must
keep **all** of its word spans connected and must not move the paragraph above it by more than
0.6px — measured at **0.0px**, with 15 of 15 spans kept. And a follow check fails if any single
animation frame moves the transcript more than 24px, if the follow changes speed by more than
8px/frame in one frame, or if the newest line is left more than 220px below the fold.

### The view follows, it does not lurch

Following the newest line has been wrong twice, in two different directions.

The first rule was *if the bottom is within 160px, jump to it*. Measuring the user's recording
line by line: one sentence sat at the same height for five seconds, then moved **112px**, then
92px, then 46px. Text that jumps a paragraph at a time reads as blinking, whatever the letter
animation does.

The first fix — *pin to the bottom after every paint* — was still wrong, and the user's own
screen recording is what proved it. A paint happens every 120ms, so a paint-granularity follow
moves the whole transcript by **23-33px in a single frame** (46px and 66px device pixels at
4.9s and 5.7s of their clip), with no movement in the frames either side: stop, teleport, stop.
Every word the reader had already read changed place at once. That is the "minimal blink" left
over after the reveal and the commit were fixed.

So the follow now runs on its own animation-frame clock, as a piece of motion rather than a
rule:

- **6px per frame** steady (≈360px/s), which is about three times the fastest this endpoint
  streams text, so text never outruns it.
- If the view has fallen behind — a table committing, a tool card landing, one slow frame on a
  busy machine — it closes a seventh of the gap per frame, and it **gathers speed** at 6px/frame²
  instead of starting at full speed. A 160px catch-up takes about a sixth of a second and has no
  discontinuity in it.
- A gap wider than a screen and a half is not text arriving, it is a view changing (a session
  opened, an older page loaded), and the view is placed rather than walked.
- One step per frame, no matter how many events asked for a follow. Three callers each taking an
  8px step inside one frame is a 24px jump, which is the bug being fixed; events now only arm the
  loop.
- Scrolling up unpins and the app stops moving the page; scrolling back to the bottom re-pins.
  Committing a finished block while a reader is scrolled up compensates `scrollTop` for the
  height change, so reformatting a paragraph cannot shove the text they are reading.

Measured: worst single-frame step **8px** and worst speed change **8px/frame** across 505 sampled
frames of a synthetic turn, newest line never more than **56px** below the fold, and the line a
scrolled-up reader stopped on moved **0.0px** when a block committed under them. On real turns the
catch-up logs 9-16px steps. The compositor film agrees: page velocity median **277px/s**, p95
**575px/s**, with frames aligned to their neighbours at residual ≈0.4 (a rigid glide, not a
rewrite), where the same measurement on the user's recording showed 23-33px displacements
concentrated in single frames.

Two details that paragraph depends on:

- A word span is **inline**. Built as a block element instead, every arrival starts a new
  line: the block grows straight down the page, gets taller on each paint, and the
  transcript — which pins to the tail — nudges the whole view with it. That is a flicker
  report waiting to happen, and it is why thinking once looked like it "grew vertically
  instead of filling the width".
- The reveal is a **fade with a little focus**: `opacity 0 → 1` and `blur(1.2px) → 0`, with the
  blur gone by 55% of the animation so the word is already sharp while it is still darkening.
  Duration 130-200ms by measured word rate, stagger inside an arrival capped at 100ms. An
  opacity-only fade of that length is, it turns out, barely visible: the entry reads as smooth
  and registers as nothing. Blur arriving first is what makes a word look like it *comes into
  focus* rather than appearing. What blur must not do is outlast the fade — `filter` lifts a
  word onto its own layer and drops it at the end of the animation, and if the last animated
  value differs from the unfiltered raster, the word pops when the layer goes away.
  `blur(0)` is identity, so the layer goes away mid-darkening and there is nothing to pop; past
  about 1.5px a five-letter word stops being a word while it arrives.
- The duration is written on **each span**, never as a custom property on the paragraph. An
  attribute write on the parent re-resolves the style of every settled span inside it, on every
  paint; a `MutationObserver` on the streaming paragraph fails the self-test if it sees one.
- The reveal is bounded in **both** directions, because either failure is visible: fewer than 4
  spans mid-fade at the busiest moment fails as "the reveal is invisible", more than 24 fails as
  "the newest line is grey for lines of reading", and any span still under 90% opacity 400ms after
  the last arrival fails as the ghost tail coming back.

### An entrance belongs to newness, not to the element

Every "it blinks" report came back to one mechanism, and it is not animation being
decorative: an entry animation whose first keyframe is `opacity: 0` is a **hiding mechanism**
for as long as it runs. Measured in plain HTML inside this same engine — no Svelte, no app
code, real compositor output — a fresh node given an entrance stays *fully unpainted* for 14
frames at 120fps (≈117ms), and re-applying the animation class to an element that already
finished it gives the same 13-14 blank frames. An opacity-only fade holds it invisible for
17-23 frames. A fresh node with **no** animation: zero blank frames. Fill mode (`both`,
`backwards`, `none`), `will-change`, `blur`, `transform` — none of them change it either way.

So the rule is not "animate less", it is: **an entrance may only run on a node that has never
been painted.** The CSS deliberately binds entrances to structure — `.turn > *`, `.tin`,
`.pop`, `.filechip`, `.toast`, `.empty` — which is safe only while nothing re-creates those
nodes. Re-create the `.turn` container and `.turn > *` replays the entrance of every row in it
at once, which is a whole-transcript flash; `transcript.ts` is the only code that builds
transcript DOM, and that convention is now pinned rather than trusted. The self-test watches
every `animationstart` in the page against the insertion time of every element, and fails if
an entrance runs a second time on the same node, lands on a node that predates the check, or
starts more than 400ms after the node arrived; it also fails if the live turn's container is
replaced or loses a child mid-turn.

Measured in the production UI running in this same WebKitGTK 2.54.0: **593 reveals across a
real turn, 0 of them a restart**, insertion-to-start age 100-135ms, and the reveal ramping through
`0 → .04 → .17 → .39 → .59 → .74 → .85 → .92 → .97 → 1` at ~60fps, median paint gap 17ms,
worst 48ms. Opening another session and back starts **no animation at all** — that is the
payoff of rendering stored transcripts outside `.turn`, so an old session does not play forty
entrances.

### It blinked in the window, and not in the browser

The report that ended the hunt: the same bundle, served by the same process over the gateway and
opened in a real browser, **does not blink**. Same bytes, same session, same server — so whatever
was flickering was not in the page.

That is what every in-page instrument had been saying for a while and it took a comparison to
hear it: the entrance guard counts **0** replays, the reveal check sees no settled span rewritten,
commits are append-only, the follow glides at 6px/frame. None of those can see a frame the engine
painted and the compositor never showed.

Engine facts here: WebKitGTK 2.54.0 on Wayland, accelerated compositing, and WebKit's zero-copy
**DMABUF frame renderer on by default**. The app is pinned to Mesa/i915 (the panel is driven by
i915, the NVIDIA card is ~97% full of inference), so the code path being exercised is same-vendor
buffer exchange rather than a hybrid-GPU handoff.

A/B, same build, same prompt, the user as the instrument:

| webkit path | what you see |
|---|---|
| WebKit's default (zero-copy DMABUF) | blink on every streaming turn |
| `WEBKIT_DISABLE_DMABUF_RENDERER=1` | gone, completely |

So the copy path is the shipped default, and it is chosen in code rather than in a shell wrapper:
`CWOFFEE_WEBKIT_MODE = default | no-dmabuf | no-compositing`, applied in `gpu.rs` **before the
webview exists** — the web process inherits the environment at fork — logged at boot
(`[cwoffee] webkit render path: no-dmabuf`) and pinned by a unit test, so a typo in the mode
cannot quietly fall back to the path that blinks and look like a fix.

What it costs, measured by a probe that samples the page's own animation clock while a real turn
streams (one sample per mode; the third would have been a screen lock, see below):

| webkit path | median frame gap | p95 | worst |
|---|---|---|---|
| zero-copy DMABUF | 17.0ms | 18.0ms | 24.0ms |
| copy path (shipped) | 17.0ms | 32.0ms | 54.0ms |

The steady 60Hz clock is untouched; the tail is roughly twice as long. For a chat window that is
the right way round: a dropped frame is visible, a 32ms paint tail is not. If your machine does
not drop frames, `CWOFFEE_WEBKIT_MODE=default` puts the zero-copy renderer back.

The probe brought its own trap: an occluded webview stops animating, and the first sample
reported a **7.9s stall** that was a locked screen. Off-screen spans are now excluded from the
statistics and counted in the log line (`off-screen spans 2`), because a check that measures
window management eventually fails on a screensaver.

### The stream is a queue, and blocks are its boundaries

- Deltas arrive faster than frames and are joined from a queue at paint time. The queue is routed
  by the block kind that is current *then*, so the last thinking delta and the first answer delta
  landing in one frame put the thinking prose **inside the answer block** and glued the answer's
  own text onto the end of it, where no blank line could ever separate them. A real turn did
  exactly this: the table sat correctly in the store, the thinking block was empty, the answer
  began "The user wants just a table…", and no `<table>` was ever rendered. Switching block kinds
  now drains the queue first, and a self-test case states the collision directly with no model
  involved — remove the drain and that case reports an empty thinking block and `tables=0`, which
  is how the drain was shown to be the fix rather than a plausible one.
- A block commits when a blank line ends it, and a blank line at the very end of the stream is
  *not* a terminator — a paragraph may still be being written. A table is the exception, because a
  table cannot survive a blank line in markdown: committing there is exact, and it is what lets a
  table become a table while the answer is still arriving instead of when it finishes.

## Files, and asking instead of guessing

The agent's file tools are `read_file`, `write_file`, `edit_file`, `write_xlsx`,
`sheet_read`, `sheet_query`, `download_file`. Four decisions there are worth knowing about:

**`edit_file` is search-and-replace, not a diff.** A 4-bit local model copying a
line it can see does far better than one generating hunks with correct line
numbers. An exact single match is applied. No match returns the line that *looks*
most like what was asked for, which turns the model's usual
"almost right, wrong indentation" failure into a one-iteration correction.
Multiple matches are refused unless the call says `replace_all`. Text is written
to a sibling temp file and renamed over the original, so a failure partway cannot
truncate your file. Anything that can be edited - text, markdown, code - should
go through `edit_file`, which changes a few lines instead of re-emitting the whole
document and rewriting your whole chat window for it.

**`sheet_read` is read-only, on purpose.** Until now the engine could make an
`.xlsx` and could not open one - `read_file` calls it binary, so the model was
blind to a workbook you already had. It now reads `.xlsx`, `.csv` and `.tsv`
through one tool: `mode: "structure"` lists sheets, extents and the things an
edit would have to preserve (charts, pivot tables, merges, filters, shared
formulas), `mode: "values"` returns a tab-separated grid for a range. Dates come
back as dates rather than as `45672`, formulas come back with the value Excel last
cached and a marker saying that value is a cache, and every read is bounded - the
default 2000 cells, hard cap 8000 - because an unbounded dump costs tens of
thousands of prefill tokens on a single-slot endpoint. Editing an `.xlsx` is a
separate tool and deliberately not here yet: re-serializing a workbook quietly
throws away the parts you did not ask about, so it will be a surgical zip edit
with a preservation test, or nothing.

Three things a real workbook forced into the structure output, because counts
alone left the model guessing:

- **the merge ranges themselves**, not just "1 merged ranges": `merges: A1:F1`
  says the sheet has a title block, which is the difference between a table and
  a form;
- **rows 1-3**, not just row 1: real sheets open with a school name and a
  treasurer's name, and the header is row 3. Showing only row 1 named nothing;
- **the flow between sheets**: `"Local Fund " -> "Canteen" (2 formula
  reference(s))`. Ask a model to "learn the flow" of a workbook and this is the
  question it is trying to answer; without it, it unzips the package and greps
  the XML.

A whole workbook is one call, not one call per sheet: `sheet: "*"` sweeps every
visible sheet and splits the cell budget between them, saying so (`# 8 sheets in
one call, 250 cells each`). Hidden sheets are skipped with a note naming how to
read one. For wide sheets, or sheets holding several blocks side by side, `cell_addresses: true`
prints `A1=First Canvassing C1=01/16/2026` instead of a tab grid - counting
empty tabs across 29 columns is how columns get mis-read, and a model in a real
session wrote a script to produce this exact format by hand.

One honest limit: shared formulas are read from the cell that defines them. A
formula that only inherits its text (`<f t="shared" si="3"/>`) carries no
expression, so cross-sheet flow sees the master, not all 284 members. The
structure output says how many formulas are shared for exactly that reason.

**`sheet_query` is there because the model asked for it by writing one.** After
`sheet_read` showed it a 1000-row workbook, the agent could see cells but could
not total a column, so it wrote its own: 111 lines of Python that unzip an .xlsx
and parse `sharedStrings.xml` by hand, then another script for the aggregates. So
the sheet becomes a read-only SQLite table (`t`, one row per row, `rownum` for the
sheet's own row number) and one `SELECT` answers the question - `GROUP BY`,
`SUM`, `ORDER BY`, top-N - with the result capped at 200 rows and a note saying how
to page. Read-only is engine-enforced: one statement, `SELECT`/`WITH`/`EXPLAIN`
only, and `SQLITE_LIMIT_ATTACHED=0` so the query cannot reach for a file. Blanks
are NULL, and a column is only numeric if every value round-trips through the
number: `01234` stays text, because silently turning a code into an integer is
corruption between the read and the write.

The header row is found, not assumed. `sheet_query` used to name its columns from
the first row that had anything in it, and on a real workbook that row is a
title: the cash-flow file's columns came out as `burgos_elementary_school | c2 |
c3`, which made the tool useless and sent the model back to the shell. Rows are
now scored - filled out, text rather than numbers, distinct labels, data beneath
- and the winner is reported where you can argue with it:

```
# header: sheet row 3 -> DATE | DESCRIPTION | CATEGORY | INCOME (In) | Expenses (Ex)
# note: rows 1-2 sit above the header (titles, names, dates) so they are not rows of t.
```

Rows above the header are not records, so `COUNT(*)` counts entries rather than
the school's name. `header_row` forces a row, `header: false` means "there is no
header" and gives you `c1..cN` with every row kept as data.

The notes about headers are generated from the sheet, and they only say what is
true. A real session passed `header: false` to every query after one bad guess, then
spent the turn addressing `c4` and counting `rownum` ranges while a named column sat
there unqueried; the tool also printed a note about columns `(c7, c8)` that no column
in that file had. So: the blank-header note lists the columns it actually produced,
never appears when there is no header row at all, and no longer suggests turning
headers off. `header: false` on a sheet where the detector can see a header gets one
line naming that row and its cells:

```
# header: none, so columns are c1..cN and every row is data
# note: header:false, so the columns are c1..cN and row 3 is data. Row 3 looks like a
#       header (DATE | DESCRIPTION | CATEGORY | INCOME (In) | Expenses (Ex) | Cash Flow)
#       - drop header:false, or pass header_row=3, to query the columns by name.
```

A row of pure numbers is not a header either: `1,2,3` over `4,5,6` now loads as data
with `c1..cN`, instead of naming the columns after the first row of the matrix.

**`ask_user` stops the agent guessing.** One question, one to six options, one
marked recommended, and a typed answer always available. The TUI answers with
number keys. The turn parks while it waits; cancel does not make the question
disappear but releases the parked tool with a "cancelled" result the model can act
on. Over the gateway, a watch-only client sees the question and cannot answer it.

A parked question has **no clock by default**: the card stays open until a person answers it
or the turn is stopped. It used to expire after 180 seconds, and the expiry was the failure —
the model was told nothing had been heard and answered its own question. The real turn that
motivated the knob is the same one that killed the default: the user could only answer by
asking someone else, and at three minutes the card closed on them. A pause is not a failure,
so a pause is what it is now. Put `question_timeout_secs = 900` in config.toml (15..=3600, or
`CWOFFEE_QUESTION_TIMEOUT_MS` for the pi core) and the clock comes back for the cases where a
stale card is worse than a parked turn; the app does not choose a budget on your behalf, and
the pi core omits the `timeout` key from pi's own dialog rather than passing a zero that means
different things in different versions. A set still gets one shared budget rather than one per
question, because a person answering three questions in one card takes longer than one
answering one.

With the clock gone, the ways a card closes without an answer are few enough to name, and each
gets its own sentence: **you stopped the turn** ("the turn was stopped; the tool did not run"),
**a written-down clock ran out** ("the clock ran out; the model was told it got none"), or
**the window holding the card went away**. The frame carries the reason for the same reason the
card does: after a Stop, "the model was told it got none" describes an event that never took
place, and it is the only trace a stopped question leaves in the transcript. Stopping has to
release the dialogs *before* asking the model to stop, on either core: pi's tool call is parked
on the card, and pi answers `abort` only once that call unwinds, so asking first means each side
waits on the other — the Stop that used to land 10002ms later, the abort timeout to the
millisecond, and then announce that the model process had not answered. Press Stop now and the
turn is over in 15ms, with the card closed and its run free to fold.

When one step needs several decisions, the model asks them in **one call with
`questions`** (2–4) instead of one call each. The card shows them one at a time —
"question 2 of 3", Back/Next, dots you can tap to jump — and **one submit answers
the set**. Navigating keeps what you typed: going back to check the first question
does not lose the words you put in the second. Submit says how much is answered
("Send 1 of 2 answers") and says what the rest will become ("1 question would be
sent as no answer"), because a silent partial answer is how a question gets
answered by absence. Once submitted, the card collapses to `answered 2 of 2` and
expands to a read-only list of what was asked and what you said. It never becomes
interactive again — the tool call that asked has already returned, and a live option
row on a settled card is a button aimed at nothing.

Three rules the question card follows, because a card that answers on contact is a
card that answers by accident:

- **Clicking an option selects it; Send answer sends it.** A misclick on a question
  about deleting something has a real cost, so the click is provisional and the
  submit is explicit.
- **The model decides whether several answers are allowed** (`multi`). When it does,
  the rows become checkboxes, second clicks untick, and the button counts what is
  going: "Send 3 answers".
- **The answer restates the question.** A quantized local model two tool calls
  later has no idea that "PDF" was an answer to anything, so the tool result carries
  the question and the options it offered alongside what you chose or typed.

A question that closes with no answer — the turn was stopped while it was parked, the reply
channel died with a window that went away, or a clock was written down and ran out — closes in
every window as "no answer — the model was told it got none and to decide for itself". Left
alone it would sit there offering Send, and the answer would aim at a tool call that has
already returned. The wording says what reaches the model, because "nobody answered" and
"nobody answered in time" are different facts and the model behaves differently about them.

## What you can do with a finished turn

Under a finished answer sit three controls, quiet on purpose: **Copy** puts that answer on the
clipboard as Markdown - the text the model wrote, not the rendering of it - and **Share as
Markdown** builds the whole conversation as a document and then asks **you** where it should go:
the desktop opens the native save dialog, a browser downloads. It used to write into
`exports/` in the conversation's workspace and show the result as a file chip. That was wrong
twice over: a share is not model output, so it should not appear in the conversation as a chip,
and a file dropped into the working folder turned up in later turns as something the model had
apparently produced - including being offered for clean-up. Now nothing is written unless a
person names the path, the receipt is a toast rather than a chip, and `share_markdown` is a read
over the gateway (a watch token can already read that conversation message by message).

The document is for a person. It names the session and the working folder, keeps the prose,
summarises each tool call in one English line ("wrote reports/q3.xlsx", "ran a command"), lists
the files produced, and leaves the reasoning out while saying how long it took. No tool-call
ids, no `<env>` plumbing, no chain of thought. The name it suggests is the session title slugged
plus a timestamp, so two shares of one conversation are two files rather than one overwrite.

**Branch** is there and disabled, with "not built yet" in its tooltip. A control that looks
available and does nothing teaches the user to distrust the rest of the interface.

Right-clicking anywhere replaces the browser's menu - Back, Forward, Reload, which in a
frameless window are three ways to damage the app - with one built from what is under the
pointer: open a file or copy its path on a chip, copy a code block or preview it, copy a
message, quote it into the composer, copy the selection. It is clamped to the viewport, closes
on Escape or a click away, and stays open when the transcript scrolls itself to follow the
stream. Inside the composer the **native** menu is deliberately left alone: the webview's own
Paste entry is wired to the clipboard in a way a page cannot reach, so suppressing it would
remove a capability rather than improve a menu. Inside a modal dialog it offers nothing at all:
this app's menu is built from the transcript the dialog deliberately put out of reach, so a menu
opened from the settings screen would act on something behind it.

A code block's header stays where the buttons are. A 200-line HTML or SVG answer used to carry
`copy` and `preview` at the *top of the block*, so half way through reading the code they had
scrolled off, and the buttons that make a long block usable were the one thing not to hand. The
header is sticky: it holds at the top of the transcript for exactly as long as its own block is on
screen, then leaves with it rather than floating over the next answer. The fix turned out to be one
word — the block's `overflow` is `clip`, not `hidden`. `hidden` makes the block a scroll container
of its own, and a sticky child measured against a box that can never scroll never moves; that is
how a header sat at the top of its own block, doing nothing, through several confident attempts at
`position: sticky`.

## When a call fails, the failure says what to do

Three gaps from one long session, all of them cases where the tool result was true
but not usable:

- **A policy block does not claim to be the user.** A denylist pattern that matched
  used to read `DENIED BY USER`, and the model spent its next two calls negotiating
  with a person who had not said anything. Now a denylist block says
  `BLOCKED BY THE DENYLIST (pattern "..." in every mode)`, and says what to do about
  the shape. A real refusal still says `DENIED BY USER`, and that one does mean stop.
  The matching itself is word-boundary aware, because `"mount "` used to match inside
  `amount` — a heredoc about cash amounts was blocked as a filesystem operation.
- **A failure the environment caused is labelled as one.** `npm error code
  ENETUNREACH`, `ModuleNotFoundError: No module named 'openpyxl'`, and
  `unexpected EOF while looking for matching '"'` each get one `# note:` line naming
  the cause and the tool or config line that answers it. The second one matters most:
  when a Python import fails, the next thing a model writes is a 111-line xlsx parser.
  The note is attached on the text, not the exit code, because `cmd | head -2` exits
  0 and hides the failure.
- **A near-miss edit quotes the file.** `old_text was not found ... Use read_file
  first` costs a call to obtain bytes the tool already had. The tool now quotes the
  file's own lines from where the snippet's first line sits — line numbers and leading
  spaces included, bounded to 12 lines and 200 characters per line — and the retry
  written from that quote applies.

## What an approval actually gates

Every tool call is classified per call, not per tool, because the same tool is not
equally dangerous twice:

What does **not** ask: fetching a file that is not there yet. A download that replaces nothing is
the tool doing its job, and the verdict says so by naming the destination it resolved — the URL's
file name, under `.cws/` unless `place: "keep"` was passed. The old verdict answered "a write,
somewhere" and produced a card reading `download http://example.com/a.pdf -> ???`, which asked a
person to approve a file whose name was withheld from them. A download that *would* replace an
existing file still asks, and a path that reaches outside the workspace still asks, whatever
`place` says.


| class | example | `ask` mode |
|---|---|---|
| `harmless` | reading a file inside the workspace, a new file, a web search, asking you a question | runs |
| `workspace` | overwriting an existing file, editing a file in place | asks |
| `sensitive` | `exec`, any path that leaves the workspace | asks |

The interesting column is the one that says *where*. A call that reaches outside the
workspace is reported with the resolved absolute path - "allow reading
/home/you/.ssh/id_ed25519?" - and the permission you grant covers **that call**, not
the rest of the session. "Allow for session" grants a tool; it has never granted an
escape, and that is deliberate: one yes to `read_file` should not be open access to
your home directory.

Approval prompts name the file rather than dumping the call: `edit in place:
notes.md (find and replace inside the file)`. The raw argument blob is the one thing
you cannot weigh a decision on.

**Stop beats an unanswered prompt.** A prompt the model raised and nobody answers -
a tab that went to sleep, a picker you never opened - would otherwise hold the turn
open for the whole approval timeout, so the Stop button looks dead for five minutes.
The engine lets a Stop press win over a pending approval, the tool gets a
`tool_result` saying the turn was stopped, and the model is not given another turn
after you pressed stop. A park is released rather than left to rot: that is also why
unanswered approvals fail closed instead of hanging.

## Workspaces

The sidebar groups sessions by the folder they live in, and the folder the tools
write into follows the session you open. That is not decoration: without it
`write_file` lands wherever the *last* session pointed, and a session that says it
lives in `~/Projects` quietly writes into `~/cwoffee-workspace`.

- A stored workspace that no longer exists (or is not writable) is **refused**: the
  engine says which session, which path and why, and the tools stay where they were.
  Silently falling back would write your files into the wrong tree.
- The engine is the authority on what a workspace may be: a path has to exist, be a
  directory, and be writable, and the writability is proved by writing a probe file
  rather than by trusting the mode bits.
- `+` on a workspace row opens a draft that already knows its folder. Nothing is
  written and no session row appears until you send the first message.
- The `+` in the Workspaces header opens the native folder picker immediately. A panel
  that makes you type a path before it will open the picker that picks it for you is a
  step that should not exist. (In the browser there is no picker to open — the folder
  is on the host, not on the phone — so a typed host path stays as the fallback there.)
- **A session keeps the folder it started in.** Moving one mid-conversation makes the
  grouping a lie: the sidebar row names the old folder while `write_file` lands in the
  new one. The engine refuses it, and the composer's folder control is disabled with
  the reason — a thing you cannot do should not invite a click. Working somewhere else
  means a new session in that folder, which the sidebar `+` does in one click; a draft
  that has not been sent yet can still choose its folder.
- `/tmp` is not a folder you work in, it is where sessions live before anyone chooses
  one. Sessions stored there group under **No workspace** — the same three words the
  composer chip uses for them and the same words the model is given — rather than getting a
  `tmp` row beside a `No workspace` row, and registering the temp root as a workspace is
  refused and said out loud in a toast rather than only written to a log. The scratch
  folder is still real, and the tools still run in it; what the app refuses to do is show
  its path where a folder name belongs, because a path in that slot reads like a choice
  somebody made.
- Renaming a group changes the label, never the path.
- **The model is told the folder the tools actually run in.** Each request carries a
  volatile `<env>` block with the workspace in it, and the host derives it per turn from
  the folder it settled on after any session adoption or move — not from a snapshot taken
  at startup. That snapshot used to be re-installed on new sessions and clears, so a
  session in `~/Projects` quoted an `<env>` block naming `~/cwoffee-workspace` while its
  own `pwd` said `~/Projects`, and the model then typed absolute paths for a tree it was
  not sandboxed in. Ask the model "what does your env block say?" and ask it to run `pwd`:
  if those two answers differ, this is the bug.
- **A conversation with no workspace is told it has none, and says `none`.** For those
  the `<env>` line reads `workspace: none (…a scratch folder that the user did not choose
  and does not keep)` and the scratch path is left out of it: the tools resolve relative
  paths against that folder themselves, so printing it only gives the model something to
  quote back as if it were a workspace. The pi core is given the same sentence in its
  system prompt, because pi appends a `<cwd>` block of its own naming that folder and
  would otherwise be the only thing the model has to go on. Two cores owe a person one
  answer, not two — and both now agree with the sidebar and the chip.

Every file a turn produces appears once as an attachment however many times it was
written, and file tools show the path they act on in the card header, so you can
see what a call touched without expanding it.

A file tool also reports the size of its own edit, in the header line, in English:
`Edited a file · notes.md · +1 −1 · OK`. Expanding the card shows a real unified
diff — hunk header, context lines, red removals, green additions. The counts come
from the lines that actually changed rather than from the model's description of
what it did, and they are stored with the message, so a session reopened next week
still says what that edit cost. For an edit the diff replaces the raw argument blob:
showing the same change twice, once as 400 characters of JSON, is noise.

### What a finished turn looks like

A collapsed transcript should read as a list of things that happened, not as a log of
API calls. So:

- Tool cards are headed in English, and tense carries state: `Running a command` while
  it runs, `Ran a command · OK` when it ends, `Could not edit that file` when the
  arguments never parsed. Raw ids (`write_file`, `exec`) appear nowhere in a transcript.
- A finished tool card is its header line and nothing else — no clamped block of JSON
  underneath it. Click for the arguments, the output and the diff. While a call is live
  the body peeks, because a command with nothing under it looks stuck.
- Thinking collapses the same way: while it runs you see three lines of it; once it
  settles the block is `Thought for 12s ›` and nothing more.
- Your own message is a right-aligned bubble, at most three quarters of the measure,
  text left-aligned inside it. The `CWOFFEE` label hangs in the left gutter instead of
  taking 74px off every block, so the answer, the thinking blocks and the tool cards
  all start on one edge — the edge your bubble ends on.

## Which conversation a turn belongs to

One engine, several screens. A desktop window, a phone tab and the app's own test harness
can be attached at once, so a turn carries the conversation it was written in: the composer
sends the session id you are looking at, and the host follows it. A turn addressed to `0`,
meaning a draft, opens a new conversation rather than appending to someone else's. An id
that matches no conversation sends nothing and says so, rather than falling back to whichever
session the engine happened to have open.

**Looking is not working.** Clicking a session in the sidebar moves the *view* only: it loads
that transcript and leaves the engine where it was. It used to run a full session switch,
which re-adopted that session's folder and re-checked its prompt cache, so reading someone
else's conversation printed a workspace-change line and a cold-cache warning as though you
had done something. The host moves when a turn is addressed to a conversation, not when a
mouse crosses a row - and the folder chip above the composer follows the conversation on
screen, not the one the engine is inside.

Switching sessions during a turn is allowed and instant: the view moves at once while the
host finishes the turn it is inside. In the sidebar, a turning arc means that session is
answering; a pulse means a turn finished there while you were looking elsewhere.

Coming back to a session that is answering rebuilds it from two sources - what the store
has, and what this window saw before it looked away - and they overlap. Your own message is
painted from whichever source has it, not both: the pi core files it in the moment you send
it, the rust core when the turn ends. Painting both is how one message ended up on screen
twice after a round trip through the sidebar.

### On a phone

A phone tab gets the same app, not a phone-shaped one. Below 768px the sidebar slides in as
a drawer over a dim that sits *behind* it (the settings screen dims with a different layer,
at a different level; sharing one class put the dim on top of the drawer and every touch
just closed it), the drawer is anchored to the window rather than the document, and the tap
targets are thumb-sized: 42px session rows, 38px controls, no 300ms double-tap hesitation.
The status bar along the bottom does the same for itself, and asks its own width rather than
the window's: it gives up a pill's summary before it gives up a control, and the two icons
that used to fall off the right edge on a phone — theme and keyboard help — are 34px targets
that cannot be displaced. See *What the status bar actually measures*.

The titlebar carries the drawer toggle, the mark, the wordmark, and the conversation on
screen. Nothing else. Window controls belong to the frameless desktop window and are not
drawn in a browser, where they had no window to act on; the model name lives on the
composer's chip, next to the control that changes it.

A phone that was minimised mid-turn comes back to a dead socket and a turn that already
ended without saying so. It now checks the connection against the gateway's 25-second beat,
drops a socket that has said nothing for twice that, and re-reads the host's state — which
outranks this window's memory of a turn it started. Before that, the composer stayed
disabled until you reloaded the page.

## The empty screen

The cup, the greeting, the tips and the composer are centred as one group - centring only
the composer left the cup glued to the top of the pane. The steam is drawn: three curves
whose feet sit on the coffee surface in the artwork's own coordinate space, each stroked
twice (a wide blurred body and a brighter thread inside) with a puff travelling up the
curve and a gradient that fades it out toward the top. Anchoring it to the artwork's
space rather than to pixel offsets is what keeps it coming out of the cup at any size.

At the top of the sidebar sits the **Cowork / Chat** tab, and it is the only way in or out of
Chat mode: `persona = "chat"` gives the model no tools, no folder and one line of system prompt,
and the Cowork tab puts the tools back by restarting the model process between turns. Because a
mode is a different *kind* of conversation rather than a filter on the same one, switching lands
you on the screen the app opens on — cup, greeting, composer — instead of leaving you inside the
other mode's last turn. What does not change is the conversation you came from: it keeps its rows,
keeps its place in the sidebar, and a turn that was still running there keeps writing into it. A
switch that left the old turn on screen reads like a switch that was refused, which is how "changing
mode should go back to the first screen" got reported.

`+ New session` survives as a small `+` here, plus Ctrl+N and the `+` on each workspace row. The
header's `+` — the one in line with the mode switch — asks for a conversation with **no folder
picked**: it does not inherit the folder a previous draft was aimed at, and the folder the host
will really use is named in the same words the sidebar and the model use, which for the scratch
folder is "No workspace" rather than `/tmp`. The `+` on a workspace row starts a conversation that
already knows its folder.


## Icons

Every icon is inline SVG at one stroke weight, coloured by `currentColor`: composer
controls, sidebar, file chips, code-block actions. No emoji and no icon font. An emoji
renders in full colour from a system font, so a two-tone app sprouts a blue picture frame
and a yellow folder, and renders differently on every platform - which is exactly what the
file chips were doing.


## Web search, and the container that comes with it

`web_search` has three providers, chosen in the footer's **search** popover (same panel on
the phone, and the same file either way): **DuckDuckGo** needs nothing and answers the first
few queries, then replies with a bot challenge; **SearXNG, already running** points at an
instance you operate; **SearXNG, run by Cwoffee** (`backend = "auto"`) starts its own on the
first search and stops it after it has been idle. Enabling the managed one configures it —
the config it mounts, the podman or docker run, the health check; turning it off only
disables it. No container is removed and nothing is uninstalled, because you may have
started that container yourself.

The managed instance is deliberately small and deliberately temporary: bound to
`127.0.0.1:8899` rather than the usual 8888 so an instance from elsewhere keeps its port,
`--memory 256m --cpus 1.0`, `--rm`, JSON API on and the bot limiter off (a limiter is exactly
what blocks an agent that runs four searches in one turn), and stopped when it has not been
used for two minutes. Its settings live in `~/.config/cwoffee/searxng/` where you can read
them, not in a cache directory.

Two promises the code keeps rather than implies:

- **A search says which search answered.** If the managed instance cannot start (no runtime,
  image pull failed), the tool result begins with `note: Cwoffee could not start its own
  SearXNG (…)` and the results come from DuckDuckGo. Otherwise the model reports "I searched
  the web" about a different provider than the one you configured.
- **The panel reports the engine, not the config.** "In use now" is the tool's own sentence,
  and a probe (`Try one search`) asks the provider rather than asking the model to demonstrate
  that it used a tool. A card that prints the config value cannot tell "auto with a container"
  from "auto on a machine with no container runtime", and those need different fixes.
- **A provider change is written the way the file is read.** Changing provider used to fail with
  the app quoting `TOML parse error at line 44 … backend = auto … invalid string expected "` and
  saving nothing: the pi core built the `[search]` lines by hand and left the value bare, and the
  config writer — which parses the edited text before it replaces the file, so that a settings
  click can never write a config the next boot cannot open — refused the edit it had been asked
  to make. The keys now come from one helper that quotes through the same writer the rust core
  uses, and the test that guards it re-parses the file the click produced instead of grepping for
  a quote mark. `scripts/gateway-search-write.mjs` asks the same question over the wire on either
  core: it seeds a config that already has a `[search]` section in it (which is where the line
  numbers in that error came from), clicks SearXNG with a URL, reads the panel again, and then
  parses the resulting file with a TOML reader that belongs to neither core — and checks that a
  key nobody touched (`max_results`) is still the value it was. The `searxng_url` box travels with
  that fix: it stays on screen while the choice is *offered* rather than only after it is accepted,
  which is how "SearXNG needs a URL, for example http://127.0.0.1:8080/search" arrived with no way
  to give it one.

## The sandbox's network is a setting, and the setting says what it guarantees

`[sandbox] network` takes three values. All three keep the rest of the sandbox
identical — read-only root, workspace-only writes, own user/PID/IPC namespaces,
deadline enforced inside the namespace. Only the network namespace moves.

| value | what a command in the sandbox can do | how you widen it |
|---|---|---|
| `off` | nothing: there is no route | — |
| `proxy` (**default**) | package managers and anything that reads `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` go through a proxy inside the Cwoffee process, which admits allowlisted hosts and refuses the rest **by name** | add the host to `allow = [...]` |
| `open` | the machine's own network, nothing in between | — |

`proxy` is a **guardrail, not a wall**, and the docs say so rather than implying
otherwise: it filters what a tool *chooses* to send. A program that ignores proxy
variables and opens its own route is not stopped, and stopping that would need
privileges Cwoffee does not take — a network namespace with a veth pair plus
`nftables`/`tc` rules, run as root, outside this process. What `proxy` buys is the
two things that matter in practice: an allowlisted `npm ci` works out of the box, and
an unlisted host produces a refusal the model can act on instead of a TLS mystery.

The default allowlist is package registries and open-source mirrors — npm, PyPI,
crates.io, GitHub, Go module proxies, RubyGems, Packagist, NuGet, Docker Hub,
Debian/Fedora/Arch mirrors — plus the local search container. Ports are 80 and 443
only; a proxy that tunnels port 25 is a VPN with extra steps. `max_mb_per_call`
(default 512 MB) cuts the pipe so a broken mirror cannot fill the workspace, and
`["*"]` admits every host with the proxy's log still running.

Two things the proxy appends to the tool result, because a 403 from an allowlist is
otherwise invisible:

```
curl: (56) CONNECT tunnel failed, response 403
# egress blocked: example.com - not in the sandbox allowlist. Add them to allow = [...]
# under [sandbox] in config.toml, or ask the user to run the command outside Cwoffee.
```

`./scripts/egress-probe.sh` proves all of this against real hosts: `npm install`
inside the sandbox, an allowlisted registry over TLS, an unlisted host refused by
name, `off` with no route, `open` reaching the internet. It drives the engine's own
`exec` tool with the same `ExecCfg` and the same bwrap arguments the agent uses.

## Three things that are security, not polish

- **`exec` runs inside `bwrap`** — read-only root, workspace-only writes, PID
  namespace, deadline enforced inside the namespace, and a network namespace that
  only `network = "off"` keeps. The denylist is a guardrail; the sandbox is the
  boundary, and `sandbox-probe.sh` plus `egress-probe.sh` prove it rather than
  trusting the flags.
- **Document writers escape before they emit any markup.** Model output quotes fetched
  web pages, so `<script>` and `javascript:` arrive in the text. A writer that passes
  them through is an injection path into a file the user opens in Word.
- **Links leave the webview, and attachments are refused at the engine boundary.**
  Rendered markdown links go to the OS through tauri's opener plugin (scope-checked),
  never by navigating the app window. Image attachments are validated by the engine —
  mime allow-list, count and byte caps, base64 shape — not by the composer, because
  the composer is not going to be the only client forever.

## Name

**Cwoffee** — *cowork* with the coffee in it. A co-work agent you drink with:
it sits at your desk, it uses your model, and the palette is a flat white in
light mode and an espresso in dark.

The mark is drawn by hand and lives at `assets/brand/cwoffee-source.png`. Nothing
else is hand-drawn: `scripts/make-icon.py` crops that one file to its own bounds and
resamples it into everything the app ships — the window icon embedded in the binary,
the hicolor set the dock looks up, the bundle icons for RPM/AppImage, `icon.ico`, the
mark in the titlebar and on the empty screen, and the favicon a gateway tab uses. If
an icon looks wrong, the fix is the source file or the crop rule in the script, never
a redraw somewhere else.

It used to be **Cowry** (a cowrie shell, one of the oldest currencies — value
exchanged between people, and the *cow* of *cowork*). The rename is cosmetic in
intent but real in the filesystem, so it migrates rather than orphans anything:

| Was | Now |
|---|---|
| `~/.config/cowry/` | `~/.config/cwoffee/` |
| `~/.local/share/cowry/sessions.db` | `~/.local/share/cwoffee/sessions.db` |
| `~/cowry-workspace` | `~/cwoffee-workspace` (yours was moved, not recreated) |
| `COWRY_URL`, `COWRY_MODEL`, … | `CWOFFEE_URL`, … — the old names still work |
| `cowry`, `cowry-app` binaries | `cwoffee`, `cwoffee-app` |
| localStorage `cowry.theme.v1` | `cwoffee.theme.v1` — old keys are read and re-saved |

The data migration runs at startup and is conservative: if both directories
exist it moves the legacy database only when the new one is empty, and keeps the
copy it displaced aside as `*.pre-cwoffee-<timestamp>` instead of deleting it.
Anything it will not decide for itself, it says out loud in the transcript
rather than in a log file you would never open.
