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
asks you where to put them. Anything else it wrote is shown as a path in the folded working
block, not as an attachment: a folder is a decision, an attachment is a delivery.

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
- The status line reports what it actually measured. Decode tokens/second is **generation
  time**, not turn time: gaps between deltas, each capped at 500 ms, re-anchored after a tool
  run and when a dialog resolves, so a turn parked on a person is never billed as decoding.

## Testing

```bash
./scripts/selftest.sh                    # the whole page-level suite, against the mock endpoint
./scripts/selftest.sh --list             # named groups
./scripts/selftest.sh cup titling delete # a targeted pass (the verdict says what was not run)
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

Current counts: engine 505, bridge 30, gateway 38 + 4, `svelte-check` 0 errors 0 warnings.
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
| [`docs/GATEWAY.md`](docs/GATEWAY.md) | devices, tokens, what a phone may ask for |
| [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) | measured memory, token rates, tool-block bytes |
| [`docs/TITLE-THINKING-DECODE-DELETE.md`](docs/TITLE-THINKING-DECODE-DELETE.md), [`docs/UI-TASKS.md`](docs/UI-TASKS.md) | the four most recent asks, with the user's wording kept |

## Not built

Knowledge Center, a browser module, MCP. `theme` is stored and not fully wired. Everything
described above is running and covered by a check that fails when it stops.
