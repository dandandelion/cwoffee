# Testing

The rule this file exists to enforce:

> **No automated test may spend the user's prompt cache.**

The reference setup on this machine is a single-slot llama.cpp server holding a ~200k-token
conversation. Every request the harness sends there evicts that context and then has to re-ingest
it, so a test run used to cost minutes of prefill and left the next real conversation cold. Tests
that cost the user's working session get skipped, and skipped tests rot.

So: the suites run against a **scripted endpoint**, and the real endpoint is a separate, deliberate,
loudly-labelled choice.

```bash
./scripts/selftest.sh                                  # scripted (default)
./scripts/selftest.sh --real-endpoint http://127.0.0.1:8090/v1   # pays prefill, on purpose
./scripts/dev.sh --mock                                # dev build against the scripted endpoint
```

The dev path is worth having: poking at the real UI by hand against the mock costs nothing, and the
mock logs every prompt it serves, so you can watch what the engine actually sent.

## What is covered where

| Suite | Command | Model involved | What it can prove |
|---|---|---|---|
| Engine unit tests | `cargo test -p cwoffee-engine --lib` | none (in-process scripted providers) | prompt building, repair, store, sandbox policy, approvals, tool semantics |
| Engine end-to-end | `cargo test -p cwoffee-engine --test mock_endpoint` | **scripted HTTP** | the whole loop over a real socket: SSE parsing, tool-call fragmentation, repair, ask_user sets, file stamping, telemetry, error paths |
| Markdown | `cd ui && npx tsx scripts/md_test.ts` | none | block/inline decisions, including while streaming |
| Types | `cd ui && npx svelte-check --workspace . --threshold error` | none | the UI compiles against the event wire types |
| In-app harness | `./scripts/selftest.sh`, or `./scripts/selftest.sh cup code` for one group | **scripted HTTP** | the real webview: reveal, commit, follow, markdown mid-stream, cards, popovers, sessions, gateway |
| Gateway | `node scripts/gateway-smoke.mjs` | none | auth, roles, workspace rules, boot shape from a phone |
| Gateway outliving the window | `node scripts/gateway-handoff.mjs` | **scripted HTTP** | the "keep devices working after this window closes" promise: the standalone gateway takes the port after the app exits, authenticates from the token file (never from argv), gets the session's config/data/state and the pi extension in its unit environment, and the stop sentence the app prints actually stops it. `--core pi`, `--lan`, `--app <installed binary>`; `--omit-ext` is the negative control that proves the environment check has teeth |
| Search | `node scripts/search-smoke.mjs` | none (HTTP to the search provider) | provider switching and the managed SearXNG container |
| Sandbox | `./scripts/sandbox-probe.sh` | none | bwrap still isolates after a kernel update |
| Egress | `./scripts/egress-probe.sh` | none (real HTTPS to registries) | the three `[sandbox] network` modes: an allowlisted `npm install` works, an unlisted host is refused by name, `off` has no route, `open` is open |
| Tool probes | `cargo run -p cwoffee-engine --example exec_probe -- proxy <dir> "cmd"`, `--example sheet_probe -- <dir> <file> <json>` | none | one real tool call, driven by hand: the same `ExecCfg` and bwrap arguments the agent uses, so a line here is a line the model would get |
| Budgets | `./scripts/memory.sh cwoffee-app 400`, `./scripts/gpu-check.sh cwoffee-app` | — | PSS ceiling, GPU pinned to the panel's iGPU |

### What the egress probe covers, and what it does not

`egress-probe.sh` checks the sandbox path end to end: a command inside bwrap, with the
environment the tool really sets, reaching (or not reaching) a socket. It proves the
allowlist, the byte accounting and the refusal text. It cannot prove enforcement
against a program that ignores `HTTP_PROXY` and opens its own route — that needs a
network namespace with `nftables`/`tc` and root, which Cwoffee does not take. Treat the
proxy as a guardrail that makes the common case work and every refusal legible.

## Running part of the harness

A full pass costs minutes, and most days the thing being checked is one label or one line of CSS. So
the page-side chain is cut into named groups, and a run can ask for a few of them:

```bash
./scripts/selftest.sh --list        # every group, with the label its section comment carries
./scripts/selftest.sh cup           # the cup groups (id OR any word of the label matches)
./scripts/selftest.sh code cup icon # several terms; bare words and --only a,b are the same thing
CWOFFEE_CORE=pi ./scripts/selftest.sh follow   # a group against the pi core
```

The mechanism is deliberately small. Each check block in `App.svelte` is a bare `{ ... }` scope
already; `scripts/selftest-groups.py` turns `      {` into `      if (want('cup', 'the cup does not
travel while the model writes')) {`, which is the one edit that cannot change the chain's structure,
and gives a block two names — an id to type and the human label to remember. The filter reaches the
page through `CWOFFEE_SELFTEST_ONLY` → `selftest_config` (a command gated on the same arming as the
rest of the harness, so an ordinary run of the app answers "every group": nothing outside a
self-test can switch checks off).

Two honesty rules come with it:

- **A targeted green is not a green.** The run says `TARGETED` in its banner and puts the count of
  groups that ran, and the names of the ones that did not, on the verdict line
  (`SELFTEST ok ... [TARGETED 2 named group(s) ran, 34 not run: dims, drafts, ...]`).
- **Blocks with no section comment always run.** Those are the chain's setup — boot, model list,
  session housekeeping, the sweep and the settings restore at the end. Skipping them is how a
  narrowed run ends up failing for a reason that has nothing to do with the change under test.

If a narrowed run fails in a group you did not touch, do not assume it is a coincidence of ordering:
run that group plus the one whose state it leans on (`--list` plus the section comments say which is
which), and if the pairing is real, put the dependency in the comment where the next person will
find it.

## The scripted endpoint

`scripts/mock-llm.mjs` (Node, stdlib only) answers with llama.cpp's exact wire shape:

- `GET /v1/models` with `meta.n_ctx` and `architecture.input_modalities`
- `GET /props` with `model_alias`, `build_info`, and a `chat_template` carrying the
  `reasoning_effort not in (...)` branch the UI reads to build the thinking menu
- `POST /v1/chat/completions` as SSE: role delta → `reasoning_content` deltas → content deltas →
  `tool_calls` with **fragmented** `function.arguments` → finish chunk → a chunk with `usage` and
  `timings` → `[DONE]`

That is the point of doing it over HTTP rather than with an in-process fake: the engine's provider,
`SseParser`, tool-argument accumulator, repair path and telemetry all run unmodified. A Rust fake
would test the loop and leave the socket untested, and the socket is where the interesting failures
live.

### Scenarios

Behaviour lives in `tests/scenarios/scenarios.json`. Selection is stateless:

- **scenario** = the first entry whose `match` substring appears in the last user message;
- **step** = the number of `role: tool` messages after that user message, so a tool loop advances
  by itself, which incidentally proves the prompt builder stays append-only (each request is a
  superset of the previous one);
- `$path` in a tool argument expands to the last path-shaped token of the prompt, so a harness can
  interpolate its own temp paths and the canned call still names the right file.

Each step may set `reasoning`, `content`, `tool_calls`, `finish` (`stop` | `tool_calls` | `abort`),
`chunk_chars`, `delay_ms`, `arg_fragments`, and `require`.

`require` is a list of strings that must all appear in the tool results accumulated since the
scenario's user message. It exists because a tool loop that advances while the tools say nothing
useful is indistinguishable from a tool loop that works, and a step that runs against the wrong
state still produces a green check. In the last batch it caught three mistakes in the fixture
rather than in the app:

- a fill scenario wrote the same file three times, so the engine said `overwrote 9892 bytes`, not
  `created`, and step 1's `require: ["created"]` was a claim about the fixture's own naming;
- the fixture's content was rewritten from two rows to two hundred, and the reads still required
  the old `row one`;
- step 1 required text that only the *first read* can return, one step too early.

All three failed loudly, with the engine's actual tool output quoted in the error. That is the
point: the needles are assertions, and an assertion that fails because the fixture drifted is the
harness doing its job.

Three deliberate behaviours:

- **An unmatched prompt is an HTTP 400 naming the prompt**, not a plausible filler. A stub that is
  almost right turns a broken test into a passing one. `--allow-unknown` exists for experiments.
- **An exhausted scenario is an error.** If the model is asked to continue a tool loop with no
  scripted next step, that is either a spinning loop or a scenario that needs another step; either
  way it should not be answered with shrugs.
- **`enable_thinking: false` suppresses reasoning deltas.** A mock that ignored that flag would
  stop testing it.

`GET /mock/state` reports requests, turns, per-scenario hits, misses, and how often thinking was off.
`selftest.sh` prints that table at the end: *no unmatched prompts* is a load-bearing line, because a
harness prompt that drifted out of its scenario would otherwise fail somewhere far away.

### What the mock cannot prove

Do not let anyone, including future-you, read these suites as evidence about model quality:

- that a real model calls the right tool for a task;
- how often a quantisation mangles tool JSON (the repair path is tested by *feeding* it garbage);
- prefill and decode rates, TTFT, or cache-hit rates — the `timings` the mock sends are invented, and
  the telemetry bar is showing you a shape, not a measurement;
- that prompt bytes are byte-stable across turns against a real cache (the mock's `cached_tokens` is
  arithmetic).

Those need the real endpoint. Run `--real-endpoint` when you mean it: it prints the cost first, and
it is the only path in the repo that touches a configured model.

## Scratch homes

`selftest.sh` exports `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `CWOFFEE_WORKSPACE` into a `mktemp -d`
and uses its own gateway port. Consequences worth knowing:

- probe conversations never enter your sessions list (they used to, and were swept by id afterwards —
  which is why a sweep bug was invisible);
- the run cannot see or damage your `config.toml`, so it is free to change mode, reasoning and model
  mid-harness;
- two instances coexist, so the script never `pkill`s your app — it kills only the PID it started.

`--keep` leaves the scratch dirs behind when you want to inspect the DB or the workspace a test wrote.

### The second key: a self-test bundle is inert unless the process is armed

`VITE_SELFTEST=1` is baked into `ui/dist`, and the release binary embeds `ui/dist` as it stands. At
the file level, the binary `selftest.sh` produces is indistinguishable from a production build - so
if you start that binary yourself, which is what a production gate does, a harness bundle drives
*your* endpoint, *your* `config.toml` and *your* session store.

That happened once in this batch. Two sessions appeared in the real store (`selftest`, `selftest
approval`), `mode` and `reasoning` were rewritten in the real config file, and turns went to the
live model with 99 % cache hits, which is how it looked normal in the log. Nothing in the harness was
wrong; the launch was.

So the harness now needs two keys: the build-time flag *and* `CWOFFEE_SELFTEST=1` in the environment
of the process that starts the app (`commands.rs::selftest_armed`). `selftest.sh` sets it; a
production start does not, and an unarmed bundle says so out loud rather than going quiet:

```
[cwoffee/page] boot: main.ts executing (self-test bundle embedded; it stays inert unless the host is armed)
[cwoffee/page] self-test bundle, harness NOT armed: set CWOFFEE_SELFTEST=1 to run the checks. Nothing
               else happens here - the checks drive the configured endpoint, the config file and the
               session store, which is not what a production start should touch.
```

For anyone running gates by hand: after `selftest.sh`, `target/release/cwoffee-app` is a harness
build. Rebuild the product bundle (`npx vite build` in `ui/`, then `cargo build --release -p
cwoffee-app`) before restarting the app.

## Adding a behaviour the harness needs

1. Add the prompt to the harness, and a scenario for it in `tests/scenarios/scenarios.json` with the
   `match` substring stable under the harness's interpolation.
2. If a test would otherwise pass on filler, the mock will 400 and the harness will fail with the
   prompt named. Add the scenario rather than relaxing the check. When a step should not run until
   a tool has really said something, put those strings in that step's `require`: the mock checks
   them against the tool results in the request and 400s with what the engine actually returned.
   A scripted answer about a file proves the script, not the tool.
3. Keep the pacing honest: chunk sizes and delays decide whether mid-stream checks see structure
   arrive *during* the turn. `--speed` scales all delays; the engine integration test uses `0.2`.
4. When the claim is about **the request** rather than the answer, ask the mock what it received:
   `GET /mock/state` reports `thinking_off`/`thinking_on`, `tools_requested` and `sampling_seen`
   (which sampling keys arrived, with what values, for the last five requests). Absence is a
   claim too — `[sampling]` being off by default is only verifiable if a test can see that no
   key was sent.

## Scripted scenarios worth knowing about

| scenario | what it forces, and the fault it pins |
|---|---|
| `truncation-check` | step 0 stops with `finish_reason: "length"` mid-sentence, step 1 finishes the sentence. Pins that the engine notices the cap, says so, resumes, and keeps both halves in history. |
| `thinking-ate-the-budget` | reasoning deltas and then `length`, with no answer text at all. Pins the error that names thinking and the knob to turn, instead of a "Done" chip above an empty answer. |
| `bullets-live` | four bullets streamed 9 characters at a time, one containing a link. Pins that items become `<li>` one at a time (`counts 0->1->2->3->4`), that one list stays one `<ul>`, that `-` is never painted, and that the first bullet does not move while the rest arrive. |
| `bold-split-live` | the same answer in 3-character chunks with a 130 ms gap, i.e. one bold construct split across two paints. Pins that no literal `*` reaches the screen and that no words are dropped from a construct whose opener landed in an earlier paint than its closer. |
| `fake-table-head` | a line that opens with a pipe and is *not* a table, then prose. Pins that waiting on an ambiguous first line may delay, but may never drop: the words have to be on screen at the end. |
| `sheet-read-csv` | the model writes a delimited file, then reads it back with `sheet_read` under a 6-cell cap. Pins that the tool card shows the engine's own grid, that the cap is honoured, and that the continuation range continues from the last row actually shown. The step also carries `require`, so a tool result that never reached the prompt fails the turn. |
| `sheet-query-csv` | the model writes a delimited file and then totals a column with `sheet_query`. `require` names the engine's own result grid (`West\t26`, the column list), so a query tool that ran but returned nothing useful fails the run. This is the gap that made a real session write a 111-line Python xlsx parser. |
| `sheet-titled-header` | the model writes a file whose first two rows are a school name and a treasurer's name, then queries it. `require` pins `# header: sheet row 3` and the aggregate (`Books\t250`), so a detector that names columns from a title block - which is what made `sheet_query` useless on the user's real cash-flow workbook - fails the turn. |
| `conversation-title` | the one-line ask the titler sends (`Title this conversation in under`), answered in 7-character chunks. Pins that the name is asked for in its own context before the answer, that it arrives in several paints rather than swapping in, and - because scenario selection is first `match` wins - that the title request is served by the title scenario even though the same turn later asks the mock a normal question. It is the first scenario in the file for that reason. |
| `long-turn-past-the-old-cap` | one turn that makes seventeen `read_file` calls and then answers. It cannot complete while any cap below 17 exists, which is exactly how its negative control fails (`only 14 read_file calls reached the transcript`). Also pins that an uncapped turn says once, at the iteration where the old build stopped, that it is uncapped. |

Scenario selection is the `match` substring of the newest user message that has a
scenario, then **assistant messages since that message** as the step index.
Counting tool results instead breaks the continuation case: a resumed answer
appends an assistant message and no tool result, so the mock would serve step 0
again and the model would repeat the same truncated half-answer forever.

Control lines the engine appends to a request only ("continue where you stopped",
"you have used up your tool budget") are deliberately not scenario anchors: the
matcher walks back to the older user message that does match. A matcher that only
looked at the newest message would report a perfectly scripted turn as unscripted.

## Machine contention

This box usually runs a 50 GB model on nine cores, so `selftest.sh` keeps two honesty rules:

- the frame-cadence check compares streaming against an **idle baseline** taken in the same run, and
  attributes long tasks to the page or to the machine (`docs/BENCHMARKS.md` has both rows);
- geometry checks verify that the page is actually being laid out before measuring rects, and count
  the skips. Four skipped geometry checks fail the run: a run that measured nothing is not a pass.
- a cadence or layout check states **what is on screen** before it measures it: mounted messages,
  block count, and the character count of the transcript. A "big transcript" check that is really
  holding 80 small rows measures nothing, and the run that added that assertion is the run that
  found the second `Load history` bug - the transcript was losing 319 KB of loaded history when a
  turn started, which no cadence number would ever have reported.

Neither rule exists to make failures disappear. Both exist because a failure that is really about the
machine gets ignored, and an ignored suite is a suite that stops protecting anything.

One more machine condition costs runs, and it does not look like a failure at all: **the page-check
pass is paced by `requestAnimationFrame`, so it needs a window the compositor is actually painting.**
An unattended session blanks the screen part-way through a run, rAF goes silent, every frame-paced
check falls back to its timeout floor, and a suite that is doing everything right sits past its 1500 s
limit with no verdict. The evidence is in the app's own log, not the harness's stdout:
`requestAnimationFrame stayed silent for 440ms`, and then `skipped the reveal-visibility and
settled-word claims; this check saw 0 real paint(s)`. The app already un-minimises, pins itself on
top and takes focus when a self-test is armed, and that is not enough, for two separate reasons.
Nothing in that list resets the session's idle timer; and on Wayland, `set_always_on_top` does not
exist for a normal toplevel - mutter has no such layer, so the pin is accepted and ignored. What
mutter does honour is *activate* (focus), which raises. So the run now keeps its own window in two
places: the frame sampler notices the clock go silent for 700ms and asks the app for the window (a
page with no frames cannot notice through rAF, so a timer watches the silence), and an armed process
re-activates its window when it has lost focus - one activation per twenty seconds, thirty in a run, because a
run that fights the user for the keyboard every five seconds is its own kind of bug. A hard cap of
three was tried first and spent all three inside the first minute on a window that had barely lost
focus during startup, leaving nothing for the silence twenty minutes later - which is the one that
costs a run its geometry checks.

`scripts/idlekeep.py` runs beside a self-test and turns the desktop's own blanking off for its
duration, then puts the setting back on exit, on error and on signal. That is all it does, and the
first version of it did a great deal more, all of it wrong on this machine.

The old keeper was an X client: it faked a pointer move through XTEST, reset the X screen saver, and
raised the app window. This session is **Wayland**, so `:0` is Xwayland, and an X client that injects
input makes mutter ask the desktop for a virtual input device through the Remote Desktop portal -
which answers by putting a "Remote Desktop / Allow Remote Interaction" dialog in front of the user,
once per keeper pass. That dialog stole focus from the very window the keeper existed to keep
painted, and since nobody consents to it, the injected input never arrived: the keeper produced a
popup and achieved nothing. Raising another client's window is not something a script can do on
Wayland either. It is deleted rather than fixed, and the rule it leaves behind is worth keeping:

> On a Wayland session, an X client that fakes input is a remote-desktop request. Do not reach for
> XTEST, `XRaiseWindow` or the X screen saver to keep a window painted - use the desktop's own
> setting, `org.gnome.desktop.session idle-delay`, and let the app present its own window.

```
python3 -u scripts/idlekeep.py > /tmp/idlekeep.log 2>&1 &
CWOFFEE_CORE=pi ./scripts/selftest.sh
# end exactly the keeper. Take the pid from the first line it printed rather than from `$!`: how a
# background job's pid survives depends on the shell doing the launching, and a `$!` that pointed
# somewhere else left one keeper running and one session with blanking switched off.
kill -TERM "$(sed -n 's/^pid //p' /tmp/idlekeep.log)"
```

It prints `idle-delay is already 0: this session never blanks, so there is nothing to keep` and exits
when there is nothing to do - which is the answer on this box, so a run here needs no helper at all.
It refuses to start while another copy runs (two stack, and the second saves the value the first
changed), and it writes the value it took down to `/tmp/idlekeep-saved-$UID.json` so a keeper that is
killed hard still gets undone by the next one. It touches no input, no DPMS and no lock screen;
`--check` reports the current value and whether a keeper is running.

If a run still reports `the window was off-screen for most of this run`, something else took the
foreground - close whatever it is and re-run. A run that skips twelve geometry checks has measured
nothing about layout, whatever else it says. The asks are logged: `selftest window (the frame clock
went silent, ask 3): ...` says when the run asked and what the window manager reported back.

### `idle-delay=0` is not the whole of "never blank"

The keeper used to make one promise - `idle-delay=0` - and keep it. A run this morning still ended with
96 frame-clock stalls and 17 skipped geometry checks, and asking GNOME Shell afterwards answered
`GetActive` with `true`: the shell's own blank monitor had engaged anyway. A blank screen is the single
most damaging thing that can happen to a page-check run: the compositor stops painting the window,
`requestAnimationFrame` goes silent, every paced check falls back to its timeout floor, and the run
fails with "keep the window visible" when the window was there the whole time.

`scripts/idlekeep.py` now does two things on a 20-second lap instead of one: it re-asserts
`idle-delay=0` (a settings-daemon restart can put it back), and it asks `org.gnome.ScreenSaver` to
`SetActive false` when the shell reports itself active. Both are the desktop's own D-Bus API - no input
is injected, no window is raised, no portal consent is requested, so the Remote-Desktop dialog that the
old X-based keeper provoked stays absent. `--check` now prints the screensaver state too:

    idle-delay=300s; screensaver active: False; keepers running: none

### The clipboard probe can stall a run before it starts

`wl-paste` blocks forever when the selection's owner is gone - and a killed self-test app is exactly
that: the window that held the clipboard died without giving it up. The harness saves whatever was on
the selection before staging its PNG fixture, and that save used to be an unguarded `wl-paste -l`. The
run then stops *before the app is launched*, and the last line in the log is `launching the app`: a
harness stall that reads like an app crash, with no process to inspect. Every `wl-paste` call in the
script now carries a timeout (3s to list the types, 5s to read the content), and the block moves on
when the clipboard is unreachable rather than waiting for a client that no longer exists.

## What the newest checks measure

The newest checks exist because a user read a symptom, not because a number moved:

- **A turn says "working" once, in a drawing, not in five dots.** The report was "the dot animation
  and the assistant name are not aligned with the block header", and then "remove the dot on every
  block - including beside the small grouped actions inside the folded group - and put two animated
  cups under the name instead". The check walks every label in a shipped turn (ten of them: the
  folded group's header, each card, your own message, the name row) and fails if any still paints
  or animates a generated `::before`. Then it measures what the complaint was actually about - the
  name's text centre against the folded group's header text centre, which came out **0.0px**. That
  number is why the offset is 13px and not 12px: at 2px the name floated 11px above the header line,
  at 23px it sat 10px below it. The check reads the published custom property rather than assuming
  the row and the word share a line, which is also what stopped the sticky-name check from failing
  when the offset moved.
- **One cup per page, at the end of the content, and it does not multiply.** The cup under each
  turn's name came back as "on multi small turns cwoffee gets duplicated and is quite small - remove
  it from below the name, put it at the bottom of the content below the Copy | Share buttons,
  enlarged". A cup per turn is the turn-per-label mistake the name had already had, and a picture
  small enough to fit a 58px gutter cannot read as a drawing. So the transcript owns exactly one
  `<img class="transcript-cup">` and *moves* it to the end of the content on every new turn
  (`placeCup`); it is 136px, which paints an ~85px cup against the opening screen's 152px box, and
  it hides itself when the transcript is empty because that screen has its own, bigger one. The check
  counts cups rather than looking for them: one on the page, the transcript's last element child,
  below the turn's button row, at least 120px wide - and after three more one-line turns, still
  exactly one. Measured, not asserted: **one cup across 5 turns**.
  One trap survives from the gutter version: the page-wide `img, video { max-width: 100% }` reset
  deleted a picture hung off the zero-wide name row, because 100% of that row is 0px while the height
  went unhurt. In flow inside the transcript the reset is harmless, which is why the rule no longer
  lifts it - and why the trap is written down here instead of in a CSS comment.
- **The pause between the cups' laps is random, and the check reads it out of the bytes on screen.**
  While a turn writes, its cup is the generating loop at the artist's timing; once it settles, the
  cup is the resting loop, and the app re-arms that loop every lap by rewriting the last frame's
  duration in a copy of its own asset - three bytes and one object URL per cycle - drawing a fresh
  4-8s from `IDLE_HOLD` (`ui/src/lib/anipause.ts`). A claim about a pause is only worth what the
  rendered picture says, so the check fetches the blob the `<img>` actually points at, parses its
  frame table and fails if the hold is outside the range: **5817 ms** in the last run, **4658 ms**
  and **6234 ms** in the two before it. The same edit made in `scripts/webp-pause.py` is
  byte-identical to the shipped JS version (`cmp` clean), and the painted version was measured
  outside the app: `scripts/webview-cup.py` now reports *every* rest between laps rather than the
  longest one, because one number hides a random spread - **4008, 5204, 6807, 6808 ms** over four
  laps in the last run. Because the page owns one cup rather than one per turn, a transcript read
  back from the store shows the live resting loop as well; the earlier rule - stills for every stored
  turn, so a 40-turn session did not decode 40 rasters - is what made a loaded session's cup look
  frozen, and "the idle webp isn't animated" was that rule being read as a broken asset.
- **The opening screen's cup is one picture and one motion.** The cup is an animated WebP, and two
  things about it fail silently. If CSS animates the picture as well, the same artwork is stretched
  twice; and because a media query cannot pause a raster loop, the reduced-motion case needs a second
  file, which is easy to ship badly and easier to 404 quietly in a desktop app. The check loads both
  shipped files, reads the shipped `.cupwrap` rules, fails if anything in CSS animates the picture,
  fails if anything is drawn over it (the frame holds exactly one element), and requires the picture
  on show to match this session's motion setting. The shipped asset's baked 5042 ms hold is now only
  the fallback: the runtime arm overrides it per lap as above, and `render OK` says so - the installed
  build logs `cup=cwoffee_initial_idle_1.webp+random-hold:480x480` rather than a blob's UUID. Both
  cup assets are byte-for-byte reproducible from the artist's export with `scripts/webp-pause.py` and
  `scripts/webp-still.py`, neither of which re-encodes a frame.
- **A turn's files are split into deliverables and changes.** The scripted turn writes six files - five
  source modules under `src/` and one standalone HTML page at the workspace root - and the check reads
  the engine's own labels (`kind`) before it reads the DOM. It then requires the page to be its own
  chip, the five changes to be one expandable count, and every rolled path to still be reachable after
  one click. A collapse that costs access to a file is the failure this guards.
- **A note looks like a note.** The check measures the resumed-history note against the answer next to
  it: smaller type (11.5px against 14px in the last run), a tinted block, a rule at least 2px wide. It
  also fails if the note's own text claims a cache state - the engine cannot see the endpoint's prefix
  cache, so it is not allowed to say "cold", "warm" or "re-prefill".
- **An image copied by another program pastes.** The harness puts a 97-byte PNG on the desktop selection
  before launch (saving what was there and putting it back in `cleanup()`), stages the same file at an
  agreed path, and the check compares the pasted bytes against it. The PNG magic is read by code point,
  not by `startsWith`: the first byte is not printable, and an earlier version of the check failed on a
  payload that had arrived intact. Where wl-clipboard is missing the run says the check was skipped.

Two harness fixes came out of the check that needed them:

- the mock emitted `tool_calls[].index: 0` for every call, so a scripted step that batched six writes
  arrived as one call holding six concatenated argument strings. Real models batch; the harness could
  not express it, which is why the flood fixture produced one file instead of six.
- popover placement cleared the *pill* it opened from, not the footer bar the pill sits inside. The
  search panel's lower edge landed 4px inside the footer, and the check's tolerance had been 4px. It
  now measures the enclosing bar and the tolerance is 1px.
- **A conversation names itself, and the name arrives before the answer.** The ask was "completely
  remove reusing another model to title our conversation" and then stall the first request for a
  title instead. `titling` drives the shipped path: a new row must read `New Session`; the mock's
  title must arrive with a caret (scoped by `[data-sess]`, so a folded folder or another
  conversation's row cannot be what is graded); the settled row must carry the model's words; and the
  title ask must not be among the stored rows. The ordering claim uses the shipped clocks, not pixels
  - first `titleDelta` at 19750ms against that turn's `turnStart` at 19860ms, first answer text
  19884ms - because a turn that paints in one batch cannot be timed by watching for its first word.
  `drafts` was retargeted rather than deleted: it used to require a row titled by cutting the opening
  line at 48 characters, and now requires the opposite (the model's name, and no row that reads
  "What is 6 times 7").
- **Deleting the conversation on screen takes its transcript with it.** `delete` now states its own
  premise: it sends a turn, unfolds the folders (a closed folder renders no rows - that is the list
  behaving, not failing), opens the row, waits for the stored blocks to paint, and only then deletes
  through the row's own menu. Afterwards it requires zero blocks in the pane and `viewId === null`.
  Stored history is counted as `.transcript .blk, .transcript .turn` rather than `.turn`: a stored
  conversation renders its blocks at the transcript root and only a live turn gets the `.turn`
  wrapper, so the check's first version reported a loaded conversation as empty.
- **An in-flight name never overwrites a name somebody chose.** `namingShownFor` paints the arriving
  title only while the row still carries a placeholder, or after the engine said `done` and the stored
  title matches that settled name (the caret's beat). Without the rule, a rename typed while a title
  was arriving disappeared for 1.4s and the row the delete check needed to click was still showing the
  in-flight text.
- **Decode is generation time, and both cores are held to it.** Engine:
  `the_decode_window_is_generation_time_not_wall_time` streams 20 deltas, sits for 1.5s, streams 20
  more, and requires the window to stay under 900ms (honest ≈84 tok/s, broken ≈28). Bridge:
  `the_decode_window_skips_a_tool_run_and_the_next_rounds_prefill` puts 3s of tool execution and 2s of
  prefill around ≈1.9s of generation (honest ≈33 tok/s, broken ≈12), and
  `a_run_parked_on_a_person_is_not_generation` keeps a dialog wait out of the window. A `TurnStats`
  fed with a turn's wall clock reports the sandbox's runtime as the model's typing speed - which is
  what the user was seeing as "decode seems off".

### Two that measure the reader's own motion

- **Scrolling a grown transcript costs a frame's worth of work, not more.** The probe steps the
  scroller 14 times at wheel size over the mounted history and reports the frame cadence, the page's
  long tasks, and how many steps ended more than 8px from where they were written. It writes the
  position rather than faking a wheel, for two reasons that are not the same: injecting input from
  outside is a remote-desktop request on this Wayland session (see above), and a synthetic
  `WheelEvent` inside the page scrolls nothing, so it would measure a handler that never runs. What
  is left is the cost both paths share - rows coming into and out of the viewport - plus the app's
  reaction to a scroll position it did not write, which is the regression the position-based
  attribution fixed.
- **A stuck name says what it is measured against.** The offset of an assistant name under the top of
  the transcript was measured against the folded group's header, and that is only true at the top of
  a turn: 400px into one, the header has scrolled away and the same constant hangs over empty gutter.
  The check now logs `the row it hangs from lands 0px below the line; none of the turn's 6 header(s)
  is beside it here` rather than grading against a number that is right at one scroll depth. Two fixes
  are possible - shrink the offset when the neighbour is gone, or anchor the label to the sticky row
  instead of to the turn's first block - and which one is right is a design call, not a regression, so
  it is stated rather than silently tuned.

### Asking the pi core what it decided

`CWOFFEE_PI_TRACE=1 ./scripts/selftest.sh` makes the pi-backed host narrate the two places its
thinking level is decided:

```
[bridge] thinking: persist=false row=5 slot=row sending off
[bridge] thinking: conversation 5 has a fresh thread; chosen=medium, settings default=medium, this row's stored=None
[bridge] thinking: pi reports off for conversation 5; publishing that as-is
```

Both runs of a thinking-level bug were found from lines like those rather than from the UI. The first
pair showed a composer pick going to a row the send then abandoned; the third showed why a level can
walk backwards on its own - pi's `get_state` answers with the level the *model* can run, which for a
model that cannot think is `off`, and that number used to be published over the one the person chose.
A conversation the process configured now keeps its own record (`applied_thinking`) instead of asking
pi again.

### Four more, from the report that came back with the numbers in it

- **The cup does not travel while the model writes.** "The animated webp in the transcript jumps too
  much when the assistant is generating new lines." As the last element in the content, the picture's
  place on screen was a function of every arriving line and every follow step. `ui/perf/probe.ts` now
  samples it against the viewport on every frame of a streamed conversation, and the run before the
  fix read: *per-frame step p50=35.1px p95=261.4px, total travel 1,348px, six reversals of direction*.
  After pinning it to a band the pane reserves: *0px, 0 reversals*. The app's own check repeats the
  measurement in the shipped page and also fails if the band is shallower than the picture, because a
  pinned cup that text can run under is a worse bug than a moving one. Reversals, not distance, are
  what the eye reads as jitter - which is why the count is printed even when it is zero.
- **Line numbers point at the right lines.** A code block's gutter is one text node of digits beside
  the `<code>`, not a node per line (a 200-line file would pay 200 boxes for the privilege). The check
  counts the numbers against the block's own line count, compares the two elements' leading, puts a
  Range's first-character rect on both sides to prove the first number is on the first line of code,
  and fails if the gutter is inside `<code>` (that would put the digits in the clipboard) or selectable.
- **An icon is in the middle of its button.** The mobile hamburger rule sets `display: inline-flex`,
  which replaces the shared icon button's `grid; place-items: center`; left to its defaults a flex
  container parks its child at the start of the main axis and stretches it on the other - the glyph in
  the top-left corner of a 38px tap target. The drawer cannot open at desktop width, so the check
  borrows the one thing the media query changes (the display mode), measures the real button with its
  real classes, and puts the styles back. The stylesheet now centres icon children with `margin: auto`,
  which centres in grid, flex and inline-flex alike: a rule that changes a display mode cannot move the
  glyph on the way.
- **The folder a new session will use, in one press.** The greeting used to name a folder only after
  one had been picked, and picking one meant a native dialog. The check opens the picker from the
  greeting's row, fails if the filter is not focused on open (typing would go nowhere), requires the two
  standing alternatives to be under the seam rather than inside the folder list, filters a registered
  folder away and back, and takes both endings of the story: choosing the folder and choosing none. It
  also fails if the panel runs off the window - the bottom row of a panel you cannot reach is not a
  choice.

The composer's clipboard button is gone, so the desktop screenshot check performs the gesture a reader
performs instead: a paste on the composer group. That is the shipped path - the handler is the one that
asks the shell what the selection holds, which is the whole thing the check exists for.

### The run keeps its own desktop painted, and asks for its window off the page's thread

Two stalls that looked identical from the outside - the page stops logging mid-run, the app's own
watcher thread keeps reporting, CPU idle in both processes - and one cause each.

- **The session blanked under the run.** `idle-delay` was 300s on this desktop and a full pass takes
  longer than that. When the screen blanks the compositor stops painting the app window, rAF goes
  silent, and every frame-paced check drops to its timeout floor; the run stretches, checks skip, and
  the verdict never arrives. `scripts/selftest.sh` now starts `scripts/idlekeep.py` before launching
  the app and ends it by the pid that script prints, and the keeper puts `idle-delay` back on SIGTERM
  (verified: `idle-delay=300s; keepers running: none` after a run). It is the desktop's own switch
  through the desktop's own API - no DPMS, no lock screen, no injected input, and nothing that could
  raise a remote-desktop dialog.
- **The page asked for its window and the ask never came back.** `present_selftest_window` was a
  synchronous command, so it ran on the main thread - and a Wayland window call round-trips through
  the compositor's event queue, which is drained by that same main loop. Ask from inside the IPC
  handler and the handler parks, the webview's message channel parks with it, and the page sits on an
  `await invoke(...)` forever. Both observed stalls were exactly there: the last page line was a check
  that had just finished, and the next thing the chain did was `blind()` - which asks for the window.
  The command now starts the ask on its own thread and answers immediately, which is the path the
  startup raise and the run's focus watcher already use successfully; the page waits 350ms after the
  answer before measuring again, because what it received was "started", not "done".

End the keeper by the number it printed. Ending `scripts/idlekeep.py` by a process-name pattern takes
out the launching shell too, which has the script's text in its own command line - one attempt at
cleanup did exactly that and killed its own harness mid-sentence.

### The frame clock belongs to the compositor, so the armed run stops asking for a window

For several runs the harness could not get a composited page: 0 frames over 76 frame waits, 13-25
layout checks forced to skip, and the verdict line "the window was off-screen for most of this run".
Every escalation an app can legally make on Wayland was tried, from inside the app, and the app's own
log recorded each refusal: a plain present (`set_focus`) - `focused=false`; a minimize/restore cycle -
`focused=false`; `set_always_on_top` - accepted and ignored; a fullscreen request - honoured, the
window went to 3840x2160, and rAF *still* delivered nothing. `xdg_toplevel` gives a client no way to
name a workspace or demand a stacking order, so the app cannot insist on being the window mutter
paints, and nothing outside the app may inject input or use `Shell unsafe mode` to force it.

The fix is not a window trick. WebKitGTK's accelerated compositing drives `requestAnimationFrame` off
the compositor's frame callbacks, so the frame clock dies with the window's visibility - and the
checks that use it are measuring JS and layout cost, not paint scheduling. `scripts/selftest.sh` now
launches the app with `WEBKIT_DISABLE_COMPOSITING_MODE=1`, which puts that clock on a timer instead.
Measured in the same run both ways, same build, same desktop state: accelerated compositing gave 0
frames/76 waits and 13 forced skips; compositing off gave **0 stalls, every layout check ran, and the
cup check reported 60 real streaming frames**. A production start never sets the variable, so the app
people use keeps GPU compositing - the harness gives up nothing about what it claims, only about how
the frames are scheduled.

What the window ask is still for is the state line the command prints (`focused=… visible=…
minimized=…`): that line is how a run says "the desktop was not showing me" in the app's own words,
which is what told us the fault was the frame clock rather than the layout.

Two things the same investigation killed, so they do not come back: the startup pin and the focus
watcher stay, but the `present_selftest_window` escalation (minimize/restore, fullscreen) was removed
again - it changes what the user sees on their desktop and changes nothing about the clock.

### A focus ring can only be measured on a page the desktop has focused

The settings-rail check reads `getComputedStyle(focused).boxShadow` and answered `none` on one core
and not the other, which is the shape of a CSS bug and was not one. The diagnostic says it plainly:
`active is the button: true, matches :focus: false, document.hasFocus false`. WebKit does not match
`:focus` — and so paints no ring — on a document whose window does not currently hold the desktop's
focus. The claim is only testable while the window is the active one, so the check asks for the
window, waits for `document.hasFocus()` up to 1.2s, and re-measures. If the desktop still will not
hand the window over, the check records a skipped claim (counted in the same tally that qualifies
"SELFTEST ok") and says which of the four faults it is looking at. What it never does is call an
unfocused page a stylesheet bug, or call a missing ring an ok.

Related, and measured rather than assumed: `.set-nav:focus` is written after `.set-nav.on` because the
selected item's own fill otherwise swallows the ring, and the ring is on `:focus` rather than
`:focus-visible` because the app moves focus into that rail itself (a deep link, a section jump) and
`:focus-visible` deliberately ignores focus that did not arrive from a key press.

### Two checks that were wrong about their own fixtures

- **The cup-away half of the band check scrolled to half the range.** On a long transcript that is far
  enough above the end; on a short one it is inside the band, and the picture is *correctly* lit there.
  The position now comes from the band (`range - band - 60`), and when the page cannot be pulled far
  enough above its own end to make the claim testable, the check says so instead of failing the feature.
- **The workspace picker's filter focus was read one frame after the click.** Focus is applied by the
  effect that mounts the panel, and on the pi core it lands a tick later than on the rust core. The
  check now waits up to 640ms for `document.activeElement` to be the field. It reads `activeElement`
  rather than `:focus` on purpose: `:focus` does not match at all when the window does not hold the
  desktop's focus, and a claim about where typing would go should not depend on which window is active.

### Four more, from the report that came back with the numbers in it

The user's screenshots are the spec; each fix carries the measurement that says it holds.

- **The cup's seat, and the patience of its putting-away.** The picture sits
  `8px` off the bottom of the transcript pane (`cup band ok (136px picture pinned 8px off the pane's
  bottom, 168px of the pane held out of the text, column-aligned; across 60 streaming frames it
  travelled 0.0px, worst frame 0.0px, 0 reversals, 1 lit/away toggle...)`). The seat is measured
  against `.scrollerwrap`, not against the content, because the pane reserves the band and the
  content stops above it. The hide has a threshold and a delay: the drawing is put away only when the
  reading position is more than `CUP_REST_PX = 24px` above the end of the content **and** has been for
  `CUP_HIDE_MS = 400ms`. 24 is geometry, not taste — the band is 168px deep and the picture plus its
  seat is 144, so past ~24px the pane's bottom edge stops showing padding and starts showing prose.
  400ms is because the follow's own catch-up leaves the view a few dozen px short of the bottom for a
  frame at a time; without the delay the cup blinks, and the check counts the toggles and fails above
  two (`the cup lit and put itself away N times during 60 streamed lines`).
- **Travel is measured only while the picture is lit.** A hidden element has no box; its top is 0.
  Sampling it anyway reported the return of the cup as a 379.9px jump of a pinned drawing. Same lesson
  as the probe in `ui/perf/probe.ts`.
- **The hand-over line.** An attachment now sits on its own `.attachline`, right-aligned to the text
  column, with the Save control's hover box inset inside the unit's padding instead of flush with its
  dashed edge. The check measures the line's right edge against the transcript column's (`> 4px` is a
  failure), and the button's box against the unit's padding — a control that cannot avoid its own
  border is a control whose hover fill lands on the border.
- **A dial is not a ruler.** A settings section is a flex column, which stretches its children, and a
  five-choice segmented control became a track the width of the panel with the choices parked at one
  end of nothing. Now the track hugs its labels and sits flush with the panel's right inner edge:
  `settings dial ok (5 choices, 271px of label in a 285px track, flush right)`. The check fails when
  the track is more than 14px wider than the sum of its buttons.

### Three claims that were really about the harness

- **"A turn that repeated itself 40 times left no mark" while the mark was there.** The check queried
  the run header once after a frame wait; the loop watch samples on the frame clock, so the finding can
  land a beat later — and on an un-composited page, "a beat" is whatever the timeout floor says. It
  polls now. The same block also used to print `loop visible: ... ok` after having already failed: its
  ok line came from the *last* sub-assertion, not from the block. Sub-block ok lines are gated on
  `fails === before` for that reason; a harness that logs FAIL and then prints ok is worse than no
  harness, because the last line is the one you read.
- **"Streaming runs at 14.0ms vs 0.0ms idle."** A baseline that sampled 0 frames grades nothing, and a
  ratio against it says the opposite of what it looks like. When the baseline has fewer than 8 frames
  the run says so, reports the streaming sample on its own, and keeps only a gross absolute guard.
  Similarly a thin streaming sample with an idle-rate median, no long task, and one big hole is the
  compositor stopping, not the streaming path costing frames — recorded as a blind spot.
- **A check that watches for animations needs paints, not layout.** `offScreen()` answers the layout
  question; the reveal-traffic and cup-put-away claims need the frame clock, so they ask `unpainted()`
  (`framePaints < 4 || rafStalls > framePaints * 3`) instead. A count of zero animations on a page that
  was never painting proves nothing in either direction.

## The desktop owns the frame clock, and only a person can give the window back

Three pi-core runs in a row failed the same way, and the code under test was not involved: about
twenty seconds in, the log said `this window had lost focus`, `requestAnimationFrame` went silent for
the rest of the run, and 30-odd geometry/motion/cadence checks were written off as blind. The state
line the app prints says the whole story: `focused=false visible=true minimized=false`. The window is
mapped; mutter is simply not painting it, and an app asking for its own focus on Wayland is refused as
focus stealing. `set_focus`, un-minimize/restore, always-on-top and fullscreen were all tried; none of
them restore the paint clock.

- **The run now waits, once, up front, and says what it is waiting for.** `selftest_window_state`
  (armed only, read-only) reports `focused/visible/minimized` without asking for anything, and the
  bootstrap polls it for up to 90 s, re-asking for the window every five seconds, logging
  `clicking the cwoffee window once does it`. A minute of waiting produces a measured run; pushing on
  produces a stack of notes that prove nothing and a verdict nobody can act on.
- **`frame()` waits before it calls a silent clock dead.** Fifteen hundred ms at a time, asking for
  the window, with a 60 s per-run budget so a doomed run stops paying the wait after the twentieth
  site. Before this, every paint-paced check settled on its timeout floor within 400 ms and the run
  "measured" an un-composited page for nine minutes.
- **A diagnostic is not allowed to claim what it did not do.** `present_selftest_window` used to
  report `pinned above others` after calling `set_always_on_top`, which this compositor ignores for a
  normal toplevel. Every log line asserting a stacking order the app does not have sent the next
  reader down a path that had already been ruled out. The call is gone; the state line stays.

## Nothing in the harness may reach the reader's model server

Two bugs of one shape: code that re-reads the config file and stops there, skipping `apply_env()` —
which the desktop applies over the file at boot.

- The pi core's **title** provider did exactly that, so with no config file present its endpoint was
  `Config::default()`'s documented local server, `http://127.0.0.1:8090/v1`: the reader's own model.
  In one pass that stalled 42 conversations by 8 s each; on a normal machine it is 42 real requests,
  i.e. a test spending the reader's cache, which is the one thing this harness is forbidden to do.
  `title_provider_at` loads, applies the environment, builds — and logs once per process which
  endpoint is being asked for names, because `no title inside 8000 ms` on its own could not tell
  "the model gave us nothing" from "we asked somewhere else".
- `provider_status_with` built the Settings picture from the file alone, so the Models page offered to
  discover and rewrite config for `:8090` while every turn went to the mock on `:8190`. A check that
  probes "the configured endpoint" is only testing what it thinks it is testing if the picture honours
  the environment too.

Both are pinned (`bridge::host::the_title_endpoint_follows_the_environment_the_turns_follow`,
`config::the_endpoint_picture_honours_the_environment_the_app_answers_through`), and the environment
touching tests in `config.rs` now share one mutex — `CWOFFEE_URL` is one variable for the whole test
binary, so two tests editing it decide the outcome by scheduling.

## Asking the model what it can run

The thinking dial "does nothing" complaint was a claim about the wire, and the wire is answered by the
model process, so the harness asks it. `get_available_thinking_levels` is a question to the process,
not a model call: it costs nothing and spends no prefill. The answer goes on the host state as
`reasoning_levels`, and the `thinking` check fails if the dial offers a level the model will not run,
or if the conversation is set to one it does not offer. An empty list is a finding rather than a skip:
either nobody asked, or the model entry declares no thinking and pi offers only `off` — which is
exactly the bug, and it used to be silent because `set_thinking_level` reports success either way.

The Rust core answers the same field without asking anything, and its test keeps three claims apart:
what this endpoint was *probed* to accept, what the *dialect* documents, and what a dial may therefore
offer. An unprobed model is not a model that cannot think, and reporting nothing would say that.
