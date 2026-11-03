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
| The kept browser, in the window | `./target/debug/cwoffee-app --page-probe` (or release) | a display, and this app's own WebKit | no harness: the app opens a page in its own window, asks for a rectangle, and refuses to go on until the widget's **allocation** matches that box — then drives it (open / click / eval / state / frame refused because the page is already on screen / diagnostics / a11y) and walks a second address in the same document, back, and a reload that must not count as a second page. Prints the fixed's real geometry and each child's allocation when a check fails. A step that clicks a link is graded too: the reply has to say it navigated, with the address it left and the one it arrived at, and the page's *own* copy of the address has to agree. The probe writes its page into the folder the conversation is in **now** — the folder the page box confines local pages to — because writing it into the boot folder would grade a guard that refuses a page it should allow. The run registers a scratch folder, so it also proves the other half — `prune_dead_workspaces` at boot forgets registered folders that are gone and hold no sessions, or fourteen probe runs leave a sidebar full of `ws` rows nobody added |
| The kept browser, live | `node scripts/browser-live-probe.mjs` | **a real Chromium** (Playwright's, already installed for `browser/`) | the built UI bundle over a stubbed Tauri IPC whose browser verbs are answered by the **real** driver process: a page repainting on its own repaints the panel's picture, the picture in the panel is the browser's own pixels at the page's own size, a press on the picture of a known box arrives at that box's page point, the column spans the window's height and the picture area takes the slack, full window reshapes the page to the box it is drawn in, hiding the panel stops the stream, and the idle clock is still on show through all of it. Fourteen claims; see `docs/BROWSER.md` |
| The terminal itself | group `tui` of `./scripts/selftest.sh`, or `node scripts/tui-gate.mjs` through `cargo-guard` | **scripted HTTP** | the built binary on its own pty: `/files` names a path, `/undo` leaves the file holding the words it held before (read off disk), `/branch` copies and hands the words back without sending, a press beside a live approval card queues and answers nothing, Stop returns the waiting words, `explore mcp:` answers from the store, and `/mcp` reports the hub and the dead server |
| The light palette | group `theme`, run twice — once per palette (`--theme light`) | **scripted HTTP** | the stored value reaches `config.toml` and the host payload, a window override says the word and rewrites nothing, and every floating surface is graded in both palettes by separation *and* by ink-on-surface contrast |
| The servers page | group `mcp-ui`, run twice — the board's env arm and the `mcp write` arm (`scripts/selftest.sh mcp`) | **scripted HTTP** | the door above Settings opens the page; the running list is counted against the hub and the file list against the file; an environment-supplied list makes every control `disabled` and says why; in the write arm a server is added, switched off, back on and removed, with each step graded in the host's own `config.toml` **and** in the hub, and its tools withheld from the model throughout |
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

## Scripted scenarios: the mock matches the newest message first, so every prompt needs its own

`scenarioFor` walks the conversation's user messages newest-first and serves the first scenario
whose `match` phrase appears in one. The fallback to older messages is load-bearing — the engine
appends request-only control lines ("continue where you stopped", "you are out of tool budget") that
no scenario names, and a matcher that looked only at the newest message would call a perfectly
scripted turn unscripted.

The fallback has a cost, and `composer-type` paid it: that group steers `What is 6 times 7?` into a
running turn (spending that scenario's only step) and then, after the answer finished, types
`answer in one line` and presses. Nothing matched the new words, so the mock fell back to the older
ask in the same conversation, had no step 1 for it, and answered `400 EXHAUSTED`. The page reported a
provider error, the group never reached its own assertions, and — because a fatal stops the pass —
every group after it went unmeasured. The fix is a scenario for the second prompt
(`one-line-answer`), not a change to the matcher.

The rule for anyone adding a check: **if a group sends more than one prompt in one conversation, each
prompt needs a scenario of its own**, even a one-line one. The mock's summary at the end of a run
(`no unmatched prompts: every turn the harness sent had a scenario`) is the place that lies if you
forget, because an older message matching by accident is not reported as a mismatch at all.

A `match` phrase is a **substring test over every user message in the conversation**, newest-first,
so it has to belong to exactly one group: a generic phrase like `answer in one line` can sit inside
another group's prompt and silently answer that group's turn instead. No run has been bitten by that
yet — the phrase was narrowed before any collision was observed — but the mock cannot report it,
because a message that matches *some* scenario is not counted as a miss. `composer-type` now types
`the line typed after the turn`, a phrase no other prompt in the suite contains.

## Two checks that were holding the wrong handle, and what they said instead

**The chrome rule measured two columns when the shell had three.** `chrome-height` asked how much
window sat below "both the sidebar and the card", which was true of the shell until the browser
panel arrived as a right-hand column: with a page on screen at the right, the card's 10px gutter was
read as 43px of window "going to a gap". The rule now takes the bottom-most of *every* column
(sidebar, card, `[data-browser-pane]`), says which column ends where when it complains, and the group
opens the panel itself and re-measures — a column that stops short of the window's bottom edge is a
column with dead paint under it, and that is only visible while the column is open. The group closes
the panel again, because every later group measured its numbers without it.

**`paste-send` pressed a chip that had already been replaced.** The check reopens a conversation,
finds the pasted picture's chip and presses it, then waited on `chip.querySelector('img')` — on the
node it had found. The view is painted from a store read, and a resync (`resync: conversation 53
looked empty and was not`) can hand the row to a new element between finding it and pressing it: the
picture then loads into the *live* chip while the check polls a detached one, and the failure claimed
"the picture is in the record but unreachable from the transcript" — a capability gap that did not
exist. It now re-queries the chip each attempt, re-presses against the live row, and reports
`N press(es) on M chip instance(s)` so a genuine dead chip still reads as one.

The underlying fact both checks tripped on is worth recording on its own account: on the Rust core,
`session_messages(id)` has been read returning **0 rows for a conversation whose turn was in flight
and had just finished** (`history: 0 row(s) for session 46`, `resync: conversation 53 looked empty and
was not — 2 row(s) came back on read 2`). The page notices this and has two guards for it —
`conversation N reads empty while its turn is in flight; keeping what is on screen`, and the resync
that repaints when rows come back — so the person does not lose the answer. What it means for the
board is that a check which queries the DOM once, a beat after a turn, is racing a read that has been
seen to disagree with the host. Re-read, repaint, and look again before blaming the feature; and when
the second look still says nothing, say which of the two it is (no card, or a record with no rows).

Both are the same lesson: **a check that holds a handle to a painted thing is checking the paint
cycle, not the feature.** Ask the live DOM what it holds, and put in the failure text what was seen
(instances, presses, ends) so the next reader can tell a race from a hole.

## A mid-turn press has two clocks, and one sample loses both

`composer-type` presses Enter inside a running turn and used to decide four claims from one
300 ms sample. It flipped, in different runs, in opposite directions:

* the running turn **ended** inside the sample window, and the check read the host's `turns` counter
  as "a second turn started" — but `turns` is one counter for the whole app (`state.turns += 1` at
  turn end in `host.rs`), not per conversation, so any session finishing moves it;
* the step boundary **took** the queued message before the sample ran, so the waiting line was
  already gone and the press looked like a lost keystroke.

The group now samples every 20 ms for 1.5 s and keeps what it saw, and it grades "not a second turn"
off the transcript — a steer lands inside the running turn, so `document.querySelectorAll('.turn')`
must not grow — rather than off a counter that measures something else. A check that reads a proxy
for a fact is a check whose failure message is a guess: this one's failure text ("started a second
turn instead of steering") described a bug that never happened.

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
| `mcp-lazy` | explore → a third-party call → an answer that must quote the server's own words. `require` pins `MCP server fake`, `mcp__fake__write_note`, `wrote ` and the file name in the tool output the model actually saw, so a reveal that declared a name the model could not call, or a call that only produced a plausible card, fails the turn. |
| `mcp-again` | the same third-party call a second time, after a grant was already given for the conversation. Step 2 requires nothing on purpose: this run's call is the one the check denies, so demanding the server's words here would make the endpoint refuse a turn whose job is to show a refusal. |
| `mcp-dead` | one call into a server that answered `tools/list` and then exited. `require` pins `MCP server dead` in the tool result, so a call that hung on a closed pipe, or answered with an empty string, fails the turn instead of timing the run out. |
| `mcp-none` | `explore` with `mcp:` when nothing is mounted — a run with `--no-mcp`, or a hub no tool process can reach. `require` pins `No MCP server is running`, so a `discover()` that rebuilt a registry from config and invented a catalog fails the turn. The harness configures two servers, so the group does not send this prompt; it stays for a run started without them. |
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

## Third-party servers: the fake is a real process, and the claims read the disk

`cargo test -p cwoffee-engine --lib mcp::` also drives the hub's socket, because the pi core's calls
go through it: one call down two roads must answer identically (a tool body with two arms is one
behaviour), a server whose process went away must be described in the same sentence either way, a
request that is not a tool call must be refused by name, a host that is gone must answer in words
and in under two seconds, and a catalog reached over the socket must still declare nothing until it
is named. Those tests run their client calls on the blocking pool, which is how `cwtool` uses them;
running them inline on a single-threaded runtime would park the worker and measure a deadlock
instead of the feature.

`scripts/fake-mcp-server.mjs` is a stdio JSON-RPC server, not a stubbed client: cwoffee's MCP hub
spawns it, handshakes it, and calls it over real pipes. It offers `t1..tN`, `boom` (an `isError`
result), `write_note` (writes a real file), a tool whose name is not a valid tool name, and a block
that is not text. Flags: `--tools N`, `--writes <dir>`, `--noise` (unrequested lines between
answers), `--slow <ms>` (never answers), `--dialect <v>` (a protocol version it will not agree to),
`--die`.

`--die` exits immediately after `tools/list`, with no timer. A delay would let a fast machine win the
race and report a dead server as live; the exit is what makes "the pipes are closed" deterministic.

The selftest supplies its servers through `CWOFFEE_MCP_SERVERS`, never by writing a config it did not
own: a run that put `[mcp]` into the reader's real config would leave that person's MCP servers changed
after a green run. The variable also replaces the file's `[mcp.servers]` list wholesale, which makes the
servers page read-only in the main run — exactly the state a window with an environment-supplied list
has to be in, so the main run grades that refusal. The page's **write** path is graded by a separate
launch (the `mcp write` arm of `scripts/selftest.sh`) whose servers come from a `config.toml` in a
scratch home the run deletes: an environment arm cannot grade a write, because the file it would write
is a file that start ignores. `--no-mcp` skips both, which is how the "nothing is running" claim is
tested. Two servers are configured by default: `fake` (live, writes into the scratch workspace) and
`dead` (answers the list, then exits).

What the `mcp` group refuses to take on faith:

- **Prompt cost, measured where the core can see it.** On the Rust core `tool_inventory` must show no
  `mcp__` name in `declared` before anything is looked up, and must show `mcp__fake__t1` in `hidden`.
  Leaking hidden tools into `Registry::defs()` fails as
  `hidden MCP tools leaked into the declared list before anything asked: mcp__dead__t1, ...`. The pi
  core decides its declared list inside the model process, so the same claim is graded on the wire
  instead — `scripts/gateway-prompt-shape.mjs --core pi` reads the request bodies and fails on any
  `mcp__` declaration before something asked for it, and on a `<mcp>` block over its ceiling or
  missing altogether. The desktop group asserts the pi page prints the hub's catalog count rather
  than a withheld list that core cannot observe.
- **The catalog's arithmetic, not just its size.** The page's two numbers are compared against the
  engine's own, and `catalog_bytes == catalog_body_bytes + catalog_lead_bytes` must hold; only the
  body is held to the 1024-byte budget. An earlier version graded the whole block against the budget
  and failed a correct engine: `the catalog costs 1051B against a 1024B budget`.
- **The file, not the card.** After the approval is answered, the group reads `selftest_mcp_note.txt`
  through the confined harness reader and requires the server's words in it. A tool result that only
  looked right on screen fails as `the MCP call reported success but ... holds "nothing"`.
- **The ask is per call.** The card's first answer is *Allow for session*, so the grant genuinely
  exists; the second call must still ask. Setting `per_call: false` fails as
  `the second MCP call ran without asking, after a yes for the same tool — a session grant covered a
  third-party call`. Denying it must leave the file untouched.
- **A dead server is a sentence, not a hang.** The call into the exited server must come back naming
  that server within its timeout, and `mcp_status` must then report it as dead rather than keeping
  the live status it earned at startup.
- **What a core cannot see, it refuses to report.** On the pi core the same group asserts that
  `tool_inventory` errors instead of answering with two empty lists — pi decides its declared list
  inside the model process, and two zeroes would claim nothing is hidden about a thing this side
  cannot observe. What it does assert positively is the road: `mcp_status.mounted` and the
  `route` the settings page prints, which must name the app's hub rather than leave a reader to
  infer that the tool host reaches it over a socket.
- **The catalog reaches the model, on either core.** `mcp-lazy`'s `require` includes `<mcp>`, and
  the mock endpoint checks `require` strings against the bytes the app actually sent. A pi-backed
  run whose extension never asked the hub for the block fails that turn: the model would have no
  way to know a server exists, and would answer from the config file it was never shown.

## The servers page is graded as an actor, in two arms

Group `mcp-ui` asks what happens when a person *uses* the page, and it is run twice because one launch
cannot be in both states: the board's run supplies its servers through `CWOFFEE_MCP_SERVERS` (so the
page must be read-only), and the `mcp write` arm launches a second window whose servers come from a
`config.toml` in a scratch home (so the page may write).

The write arm's servers are configured the way a person's are — a `[mcp.servers.fake]` table in
`$XDG_CONFIG_HOME/cwoffee/config.toml`, inside a scratch home the run deletes — not by environment. That
path matters: the file the app reads at boot is `$XDG_CONFIG_HOME/cwoffee/config.toml`, and a table
written to `$XDG_CONFIG_HOME/config.toml` is a file nothing reads. The arm then boots with no servers
while every settings save happily writes to the correct file, and the page's own check reports
`no running server in the file to point a new entry at` — which reads like a broken page and is a broken
arm.

Both arms:

- **The door's position is measured.** `compareDocumentPosition` against the Settings entry, not a
  screenshot: `the MCP door is not above Settings in the sidebar footer`. And `aria-expanded` has to be
  false before the press and true while the page shows — a door that lies about being open is the same
  class of bug as a switch that lies about being on.
- **The two lists are counted against two sources.** `[data-mcp-server]` rows against
  `mcp_status.servers` (the hub), `[data-mcp-file]` rows against `mcp_status.file.servers` (the file).
  A single merged list could pass a wrong answer twice; two counts cannot.
- **The search is a lookup by tool name.** Searching `mcp__fake__t1` must leave exactly one running row,
  and must add a file row only where the file actually lists that server — inventing a row for a server
  that came from the environment would be inventing a config entry. A list filtered to nothing has to
  say it was filtered: `an empty filtered list does not say it is filtered — it reads as "you have no
  servers"`.

The read-only arm (the board's own run, where `env_lists` is true and `writable` false):

- the page must show the two servers that are running *while the file lists none*: a page that read the
  file would report "nothing is configured" about servers that are answering right now;
- the explanation must name `CWOFFEE_MCP_SERVERS`, not just say "read-only";
- every switch, Edit and Remove on the rows, and the master switch, must be `disabled`, and there must
  be **no** Add button — `the page offers to add a server to a file this start ignores`.

The write arm:

- **Add** writes the file the next start will read — graded by `selftest_config_text`, which returns the
  host's *own* path, so the check cannot pass by writing a file it guessed at — and the page's sentence
  must contain both halves: `written to [mcp.servers.uitest]` and `live (N tool(s))`. A file written
  beside a program that failed to start is a visible failure, not a green row.
- **The hub agrees** before the row counts: the new server must read `live` in `mcp_status.servers`, and
  the running list must show the tool names it offered (`[data-mcp-tools-for]`), because "does it have a
  write tool?" is not answerable from a count.
- **Nothing is handed to the model.** On the Rust core, `tool_inventory` must show the new server's
  names in `hidden` and none of them in `declared` — the whole economy of the feature, measured after a
  mid-session add rather than only at boot: `adding a server declared its tools to the model without
  anything asking for them`.
- **A switch moves two things.** The file must hold `enabled = false` *under that server's table* (a
  regex over the region, because `enabled` appears in three tables), and the hub must report
  `off in config` with no tools attached.
- **An edit keeps what the form never showed.** The row holds environment *names*; the Edit form
  prefills bare names, and the check first fails if the form prefilled a value
  (`the edit form prefilled a value (…) — this page does not hold values to show, and saving it back
  would blank the token`) and then fails if saving an argument change wrote the key back empty
  (`the edit wrote the environment key without the value the form never showed`). The file is read back
  for both halves: the new argument line present, the old one gone, and `CWOFFEE_MCP_ARM = "1"` still
  there. The hub has to say `live` afterwards — a changed command that leaves the program stopped is a
  file write, not an edit.
- **Remove needs two presses.** The first must put `armed` on the button, or the check fails with
  `a single press would un-configure somebody else's program`. Afterwards the file mentions the name
  nowhere — neither table — the hub has no such server, the withheld list has dropped its names, and the
  file is back to the server count it started with: `the round trip left the config changed`. That third
  fact is the one a config-only page would miss, and it is what `Registry::forget_stale_mcp` is for: a
  server removed while the app runs must stop being callable **in the session that removed it**, not at
  the next start.

What neither arm claims: no phone is attached to this run, so the gateway's `writable: false` is graded
by the engine's payload test over the wire (`status_payload` is built once for both transports), not by
a device. The TUI's `/mcp` door stays read-only and says so in `docs/TUI.md`.

## The browser: the endpoint counts the fetches, so a narrated run cannot pass

`./scripts/selftest.sh browse` sends a real browser at a real page that the scripted endpoint itself
serves (`GET /selftest-page.html`, plus the script it loads). The scenario `browse-page` has the
model click a button and evaluate an inline script, and its `require` list makes the endpoint return
400 unless the tool's own output contains:

* the page's own title (`Cwoffee browse selftest`) — proof the DOM was read, not described;
* `click "More" ok` — proof the walk happened in the page rather than in the tool's imagination;
* `eval ok -> 5` — proof a value came back out of the page's JS environment;
* `uncaught errors` and `Cannot read properties of undefined` — Chrome's own words for the error the
  page's script throws on purpose, which no paraphrase produces.

Then the check asks the endpoint `GET /selftest-hits`, which counts fetches of the page and its
script. That counter is the part a tool that only *reported* a browser session cannot move, and it is
why this group is not graded on the model's summary of the result.

Two branches, both graded. If no driver is installed here, the group instead requires the settings
line (`browser_status`) to say the tool is **not mounted** and to name both install commands
(`npm --prefix browser install`, `npx playwright install chromium`) — so a machine without Playwright
runs a real check rather than skipping, and a machine with one that failed to mount the tool fails.
Nothing in the suite installs a browser: a ~150 MB download onto this machine is a decision a person
makes by running a command.

The mode claim is cross-checked, not restated: the card's `browser: headed (a real window)` /
`browser: headless` must agree with what `browser_status` says this start will do for the model that
is answering, because that is a claim about a window appearing on the user's desktop and two answers
would mean two rules. The scenario's URL uses `$base`, which the mock fills with its own origin, so
the URL stays true whatever `MOCK_PORT` is.

Unit-level rules live in `cargo test -p cwoffee-engine --lib browser::` (the decision table, the
absence of any `headed` argument, the `file://` confinement including a `file:///etc/hostname` that
must stay outside, the no-driver answer carrying the fix, and the picture being withheld from a model
that cannot look) and `provider::` (the picture's placement inside an Anthropic `tool_result` against
a following `user` message on the OpenAI shape, and a picture-less tool result staying one message).
`host::tests::the_browsers_mode_follows_the_config_row_and_not_the_boot_snapshot` is the timing case:
the model row is written *after* the host is up (which is what a probe does), and the host's published
`browser_vision` and the tool's copy must both move; then the model is switched to one recorded as
text-only, and both must move back. A value read once at boot fails this test, which is how the
feature's own selftest found it.

## The kept browser: one page seen twice, and a clock that has to fire

A kept browser is graded in **two shapes**, and a start is one shape or the other: the relayed
picture column (`browser-panel`, `browser-idle`) and the page painted in this window
(`browser-inpage`, which boots with `CWOFFEE_BROWSER_PAGE` on). `selftest.sh` says which shape the
board it launched is grading, and asks for the in-window group on its own — a process cannot be both,
and a group that graded a picture for a claim about a widget would be measuring the wrong thing.

`./scripts/selftest.sh browser-panel` grades the column on the right of the transcript. The claim it
exists to protect is the one a screenshot cannot make on its own: **the page in the panel is the page
the model acted on.** So the group is written as one page, two screens, and a set of comparisons
between them:

* the button on the transcript card opens a **column**, and the column's bottom edge may not pass the
  window's — a panel that adds height instead of taking a column of it is the failure mode that
  `chrome-height` spent a whole run chasing once already, so it is graded here rather than hoped for.
  The column's top and bottom are then compared against `.panecard` itself: standing beside the
  transcript means ending on the same two lines, and a column a few pixels out of true reads as one
  window drawn badly;
* with nothing ever open the page area **invites a start** — the browser's mark, `Start browsing`, and
  the other way in — on a page's own surface rather than the desk colour, and the panel does not say
  "no browser is open" a second time underneath. Where a browser *was* open and was closed, that row
  stays and names the reason: the invitation replaces the quiet shape, not the news;
* a person types an address and gets `data-browser-frame="yes"` with `naturalWidth > 8` — an `<img>`
  with no bytes behind it looks fine and shows nothing, which is this panel's characteristic failure;
* the page's **own title** has to reach the panel, from the page, not from the prompt;
* `file:///etc/hostname` is refused, the refusal says what the panel does open, and — graded, because
  it is the easy mistake — the refused address must not replace the page that was open;
* a reload by the person is recorded with `data-browser-actor="person"` and takes another picture
  (`frames_taken` must move); then the model's turn flips it to `model`;
* the model's turn runs the `browse-session` scenario, whose **second** call names no url at all.
  That is the load-bearing step: it can only succeed if the browser survived between calls, and the
  mock refuses the turn unless the tool's own output contains `acted in the browser that was already
  open at`, `it gave back: cwoffee selftest` and the click's own reply;
* the panel's url must be a url the transcript's card itself names — the check that the column is
  not a picture of some other browser;
* **pointing at the picture points at the page.** The page's own click target sits at a position the
  server publishes (`aim` on `/selftest-hits`), the check presses the `<img>` at the pixel that
  position maps to in *that* window, and the witness is a counter the page raises by fetching its own
  endpoint. A column that scaled its picture by the wrong factor presses the wrong spot and the count
  never moves. The same argument is then made in the other two ways a person points: a wheel over the
  picture must move the page's `scrollY` (read out of the browser, not out of the column), and keys
  typed in the column must appear in the page's own `<input>` after the check clicked that field
  through the picture. The page's numbers come from the server that served the page, so the check
  never trusts the harness's own memory of a layout;
* **the icons still have names.** "Icons instead of words" is only an upgrade if the words exist where
  a keyboard and a screen reader can reach them, so the group counts the column's icon-only buttons
  (there must be at least the six it shows), and refuses any that carry no `aria-label` or nothing in
  `title`. A glyph with no name behind it is a control nobody but a mouse can use;
* Escape must drop the armed close **and leave the browser open** — a key that answers the question
  should not also perform the thing the question was about;
* the middle view has to show what the page said about itself: the fixture page's own script throws
  once, and the column must repeat that error rather than show a screenshot of a page that is quietly
  broken;
* the status line must be the engine's sentence, not one the column wrote. It is compared with the
  digits normalised: that sentence contains a live countdown, so a fresh read lands a second after the
  column's last poll and the number moves. Comparing the words and letting the number float is the
  honest form of this claim — and the number itself is graded by the idle arm, which waits the span
  out rather than comparing two clocks.
* closing is two presses: the first arms (`aria-pressed="true"`) and must **not** close, the second
  closes, and afterwards no `data-browser-frame="yes"` may remain. A frame left above a line saying
  the browser closed is a page that still exists.
* once nothing is open, the page area is **a shape, not a hole**: it must draw the outline of a page
  (`.bpy-sil`) over words that name what is missing. A blank box is read as "the page failed to load",
  which is a different fact, and this is the one moment in the suite where the empty state is on screen
  to look at — the claim exists so a later redraw cannot quietly turn it back into a rectangle.

`./scripts/selftest.sh browser-idle` runs in a **second launch** over a scratch `config.toml` holding
`[browser] idle_secs = 4`, the same shape the `mcp write` arm uses. Two rules it enforces, both about
not lying to itself:

- it fails if `idle_secs > 30`, because an arm whose scratch config never reached the start would be
  grading a five-minute wait it did not configure (and the scratch config lives at
  `$XDG_CONFIG_HOME/cwoffee/config.toml` — the same trap the MCP arm documented);
- it fails unless the panel's own sentence names **that number** — "closed itself after 4 seconds".
  A check that accepts "closed itself after 5 minutes" for a four-second wait passes by describing a
  setting nobody chose.

After the app is ended the arm asks the process table whether any `browser/session.mjs` is still
alive. An app that closes its browser on a timeout and leaks one on exit has not answered the
objection that the whole feature has to answer, and "the driver exits when its stdin closes" is only
worth writing down if something checks it.

`--no-browser` sets the person's own switch (`CWOFFEE_BROWSER_KEEP=no`) and makes `browser-panel`
give up its happy path for the other arm: the panel must say keeping browsers is switched off and
name `keep = false`, rather than offer an address box that can only fail. On a machine with no
`browser/session.mjs` the same group instead insists the panel names the missing file and both
install commands. Three states, three sentences, one arm each — "no browser" is not one answer.

The unit side is `cargo test -p cwoffee-engine --lib browser_hub::`: real ndjson pipes to a fake
driver (`scripts/fake-browser-session.mjs`), because a hub tested against a mock object never had to
deal with a driver that dies mid-line. It grades the open/act actor bookkeeping, the page's own
events arriving without polling, a page that refuses being reported as the page's refusal, a driver
that dies answering with a reason instead of hanging the caller, the idle clock firing and saying the
number, frames being bytes for the watcher, a risk check reading the open session **without awaiting
a lock**, and the three "why is there no browser" sentences being different from each other.

## The page in the window: the rectangle is the claim

`./scripts/selftest.sh browser-inpage` grades the other shape, and it is written around the fact a
screenshot cannot establish: **the page is where the panel's box is.** The board boots it with
`CWOFFEE_BROWSER_PAGE` on, so the page is a widget over this document rather than a picture of one,
and every later claim is downstream of the rectangle — a working page in the wrong place is still a
broken panel. What it insists on:

* the panel's `[data-browser-pagearea]` and the widget's **allocation** agree on position and size,
  and the panel says so out loud if they do not (`[data-browser-placedoff]` has to be absent, after
  giving the panel and the widget a moment to agree, because the box is re-stated when the panel's own
  layout changes);
* the page's own viewport is the size of that box — the page's report, not the request;
* full window moves the box with the column, and a hidden panel sends a box of none so no page is
  left floating over the transcript;
* both sides act on one page: a click by the model is counted by the page's own endpoint
  (`0 → 1`) and leaves the paragraph saying what only a real click can say, and the panel records the
  person and the model as the two actors of the same session;
* what the page reported about itself is in the drawer as records — the arm's own throw and the page's
  own bug, a network failure and a broken resource, each with its kind, and reading them does not
  count as using the browser;
* an address typed at a page that is already open reuses that page: a `#fragment` moves the panel's
  address without moving the page count, `back` answers only once the page has moved, and a reload is
  one page and not two;
* no picture of it anywhere: `frame` refused by name, `live` false, and the column's About view
  saying "in page" rather than "live".

**The drawer's log is graded in both shapes, and it caught a hole.** The row list is `[data-browser-
log]`, and the panel's root used to carry an attribute of the same name — so `querySelector('[data-browser-log]')`
matched the *root*, passed on a drawer that had never opened, and stayed green while the relayed
browser's log held nothing but the app's own navigation records. The flat vocabulary (`console`,
`pageerror`, `requestfailed`) filled the panel's bags and never landed a record, so the person saw the
error and `page_log` said the page was silent. The selector is scoped now (`[data-browser-drawer]
[data-browser-log]`), the button's `aria-pressed` is graded, and the claim that survives is the one
that matters: the page's own complaint is in the records in the shape where a real Chromium runs.

## Four things the kept browser checks caught by being written honestly

**The title witness reads the endpoint, not the page.** The panel must show the page's own `<title>`, and
the self-test page's `<title>` is `cwoffee selftest` while its `<h1>` is `Cwoffee browse selftest`. The
first version of the check compared the panel against the `<h1>` remembered from the harness and failed a
panel that was working (`the panel's title is "cwoffee selftest" — the page's own title never got there`).
The obvious repair — fetch the page and read its `<title>` — failed next, because the app's webview is a
different origin from the mock and an html body it is not allowed to read comes back as an empty string:
`the selftest page serves no <title>, so the title check cannot run`. So `mock-llm.mjs` keeps the two
strings it builds the page from in constants (`SELFTEST_TITLE`, `SELFTEST_H1`) and publishes them on
`/selftest-hits`, which already carries CORS for exactly this kind of witness. A check that cannot read
its witness says so rather than reporting the feature broken.

**A reload owes the viewer a picture.** `browser-panel` now *waits* for `frames_taken` to move after a
person presses Reload, instead of reading the count immediately: the panel pulls on its own schedule and
only when the page changed, so an immediate read grades the poll's timing rather than the reload. That
wait found a real bug — the desktop compared the caller's *picture* sequence against the *state*
sequence, so a reload (state bumped, no picture taken) was answered with "nothing newer" and the panel
kept drawing the page from before the reload. The rule now lives once in the engine
(`needs_frame`) with a unit test, and the same check on the same page now passes on both cores.

**A rename can compile and wire the wrong thing.** The panel's error line was fed from `browserErr` — a
different, same-typed variable that belongs to the Settings browser-status block — after the panel's own
state was renamed to `paneErr`. `svelte-check` reported nothing, because a string is a string, and the
group's `file://` refusal check failed with an empty error line twice before the wiring was read rather
than assumed. When a rename leaves a neighbour of the same type behind, check the *call site* is in the
diff.

**The board asks for the headless shell, and the reason is the run itself.** `selftest.sh` exports
`CWOFFEE_BROWSER_SESSION_HEADLESS=1` *and* `CWOFFEE_BROWSER_HEADLESS_ONLY=1`. With full Chromium running
in its own headless mode, the run's own window stopped being composited: `requestAnimationFrame stayed
silent for 60400ms`, five geometry checks were skipped, and the verdict said the window had been
off-screen — a complaint about the desktop, caused by the feature under test. The headless-shell build
opens no X connection, and the panel still gets its frames, because the panel draws the page's pictures
rather than a window of its own. `[browser] headed` still wins for a person: this is the harness saying
"do not paint", not the product changing what it shows.

One related honesty fix came out of the same bad-compositing stretch: a pacing claim (`the table only
appeared 26ms before turnEnd — that is at turnEnd, not during`) is now written off as a skip when the
turn delivered almost no paints. "During the turn" is a claim about the frame clock, and a turn nobody
composited has no clock; the skip is counted in `blindSkips`, so a run that could not see its own window
still ends red — with the reason being the reason.

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

The board now also refuses to *start* a full pass while another self-test window is up. A run that is
killed part-way (a Ctrl-C during the build, a shell that dies) leaves its `target/release/cwoffee-app`
running, and that window - not the desktop - is what takes the compositor from the next run's window.
The next run then finishes 40 minutes later with `the window was off-screen for most of this run`,
which reads like a machine complaint and is a stray process. So the first thing the board does after
resolving its artifacts is look for a `target/release/cwoffee-app` that it did not start, by argv
(`argv[0..1]`, so the launching shell - which carries this script's text, path included - cannot match
itself), and die with the pid and the sentence `this script does not end processes it did not start`.
It looks only at this tree's artifact: a person's installed app under `~/.local/share/cwoffee-desktop`
is theirs to keep open, and the scratch config, data and state directories are precisely why two
instances can coexist. A developer running the app by hand from the target dir is allowed to be doing
that on purpose, which is why the message names a pid instead of reaping one.

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

> **Superseded by the inline loader (batch eight).** The transcript no longer carries a cup. The
> working status is a 20px picture inside the turn's own header
> (`.blk-run > .blk-head > .run-working`), or a loose one after the last block when a plain answer has
> no header (`.turn > .run-working.turn-tail`), and the composer's send button carries the same picture
> inside its own 32×32 box. The cup bullets below stay as the record of why that picture was built and
> of how a raster animation is measured — the traps in them (the page-wide `img, video { max-width:
> 100% }` reset resolving against a zero-wide row, re-arming a loop by rewriting the last frame's
> duration, a still carved out of a delta frame) are traps the loader runs into instead, and it has
> checks for each: `mark-visible`, `mark-art`, `send-loader`. `cup-art` still grades the opening
> screen's cup, which is still there. What ships is
> `ui/public/cwoffee_turn_working_inline_1.webp` (40×40 for a 20px box at DPR 2, 75 frames, a 2500ms
> lap, infinite) and `ui/public/cwoffee_turn_working_inline_still.webp` (one frame), both written from
> `assets/mark/cwoffee_turn_working_source_199.webp` by `scripts/mark-resize.py` — one resample for both
> files, and both verified with an independent decoder rather than only the library that wrote them.

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

- **The cup's seat, and the patience of its putting-away.** *(superseded: the transcript's cup band,
  `--cup-band`, `--cup-left` and `.transcript-cup` are gone, and the follow/jump checks now fail if the
  cup comes back; the numbers below are the record of how a pinned picture was measured, which is what
  the loader's geometry checks inherit.)* The picture sits
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
## The chrome row, the folder row a chat does not need, and a dot that is a record comparison

Three checks were added for the batch that shortened the titlebar, removed the chat mode's folder line,
and fixed unread conversations. Each one is written against the thing the report was actually about.

- **`chrome-height` grades where the height went, not the number on the bar.** The bar is checked against
  the window's own `--titlebar-h` as the browser resolves it (a token is a real height; matching the
  literal string in the file would grade the source), its controls are checked to sit inside it — a
  control a window's decorations clip is a control nobody can press — and then the claim that matters:
  the sidebar and the pane start on the bar's bottom edge, and nothing sits below both of them. That last
  number is printed, because "reduce the topbar and give the space to the list and the transcript" is
  answered by `0px spare below them` and not by `36px`. The card's own 10px gutter inside its pane is
  deliberately *not* charged to the chrome row; the first draft of this check failed on it. The drawer's
  narrow-width `top` and the settings dialog's height are read out of the shipped stylesheet and resolved,
  so a fifth hard-coded number somewhere else is a failure naming both values. And if a band *is* left
  below them, the check now also lists the shell's loose text nodes: the 96px that once went missing was a
  `/* … */` comment written in markup, which Svelte turns into a text node, and a text node in a flex
  column is a box — invisible to `element.children`, and perfectly able to eat the window. See
  `BUILDRUN.md`, trap 129.
- **`unread-dot` sets the record, then reads the dot.** The mark is a comparison between a conversation's
  stored message count and where that conversation was last read, so the check writes the marker through
  the app's own `SEEN_KEY` at 0 for a conversation whose row says more, and requires the dot. Then it
  opens the conversation and requires the dot to be gone and the marker to have moved to the row's count,
  and requires that a refresh of the session list does not put it back. Three things it has to get right
  or it lies: it looks away first (`showStartScreen()`), because it makes its own conversation with a
  turn and a turn moves the view to what it grew; it waits for the dot rather than grading the class the
  instant `viewId` changes, since the view name moves before the transcript lands; and it restores the
  person's marker and their view in `finally`, because a suite that leaves a read marker moved tells
  their next start that a conversation they never read had been read. If the sidebar has no conversation
  with messages, the check makes one rather than reporting "nothing here can be unread" — that sentence
  would be its own empty world, not a verdict.
- **`mark-visible` grades two homes.** A working mark lives in a turn's run header, or loose after the
  last block when the turn is a plain answer with no header at all. The group accepts either, requires
  never more than one per turn, and grades them differently: the header mark must sit above and clear of
  the answer's first line, the loose one below it and on the prose column. Both must be a real painted
  `img` at the declared box (the page-wide `img, video { max-width: 100% }` reset is what made an earlier
  mark measure `0x56`), must keep their own opacity and layer promotion against `.turn > *`'s entrance
  animation, and must be gone once the turn lands. If layout was measured but no frames were delivered,
  the group says it measured geometry only — a timer can prove a box; only paints prove a screen.
> **Superseded by the gutter (batch ten).** The header mark is no longer in the sentence and the loose
> one is no longer on the prose column: both hang in the gutter to the left of the block, and the
> claims below this one grade the band they now live in.
- **`mark-art` checks the bytes that ship, not the artwork on disk.** The CSS box the loader actually
  resolves to, the loop's frame count and lap read out of the shipped table, the canvas against that box
  at this window's DPR, and whether the reduced-motion still decodes in this engine. That last claim
  exists because Pillow accepted a still carved from a delta frame, `identify` called the same file
  `corrupt image`, and the app showed nothing for `prefers-reduced-motion`.
- **Two clocks, for two different questions.** `send-loader` and `reveal` both count real
  `requestAnimationFrame` paints and sample on a timer. Geometry without paints is a named note, not a
  pass; a single sample after `frame(150)` is not evidence about a 130ms fade, which is why `reveal` runs
  its own frame sampler and grades the peak number of mid-fade words rather than one reading.

- **`mark-visible` grades the gutter, not just the box.** The loader hangs left of its block's rule, so
  the geometry claims are about a band with two edges: the card's own edge clips anything hung too far
  out, and the block's rule stops it from reading as a glyph in the heading. Both are measured, because
  the first version of the offset put the mark outside the card and passed a claim that only asked
  "is it left of the text". Two more claims grade what a screenshot cannot: the header row has to span
  its own block (it used to inherit a block *label's* 58px column, which clipped the sentence), and the
  mark must still be the same element after a step lands — a mark removed and rebuilt looks identical
  to one that kept running, and a rebuilt raster restarts at frame zero. The narrow-width offsets are
  graded as shipped rules rather than measured, and the receipt says so: a selftest cannot resize the
  window it is running in.
- **The loose loader's ordering is graded by the append that adds a node.** Streamed prose continues the
  block already open — spans and box kept — so appending text adds nothing and proves nothing about
  order. Files are the mid-turn append that does create nodes, so the check delivers one and requires
  the mark to be the same element, still below the writing, and still nothing after it: a file strip
  beneath the mark is a hand-over line sitting under a spinner.
- **A hover state is graded twice, and the two halves are named.** Nothing in this suite can put a
  pointer on a button. The runtime half measures the rest state mid-turn (working mark lit, stop glyph
  at opacity 0, button still 32x32 and still labelled Stop); the reveal half reads the shipped rules for
  a rest rule that hides the glyph and a rule per reveal (`:hover`, `:focus-visible`) that brings it back
  *and* takes the mark away. A `:hover`-only reveal would satisfy the pixels and leave a keyboard user
  with no affordance, and revealing without dimming restacks the two glyphs for whoever hovers.
- **A claim that a line is gone is a claim about text.** The chat check rejects both the folder control
  and either sentence anywhere in the pane, because a copy of "Chat has no folder" under a new class name
  passes a selector check — and the opening screen keeps a second composer that a selector scoped to one
  of them would miss.

### Branching: two records, two memories, one check

`branch` is the group for a verb whose danger is invisible from the DOM. Copying transcript rows is
easy and a unit test can do it; the two things that make a branch a branch — the model answering out
of the copy, and the conversation it was copied from still being whole — are only visible from inside
a running app, and a rewind-shaped implementation gets both wrong while looking identical in the
sidebar.

- **The source is measured before and after, row by row.** Not its length: the check compares each
  row's content too, because a branch that cut the original and left the same count of rows behind
  would pass a length check. Sabotaging `create_row` to `truncate_from` the source after the copy
  produces `a branch is not a rewind: conversation 21 held 4 rows before the branch and 0 after it`,
  and the second claim (`answering in the branch grew conversation 21…`) catches the same bug from the
  other side — the two paths have to grow apart.
- **The composer is read from the painted textarea, not from state.** `prefill` is a promise about
  what the person can now do: branch off your own message and the words come back to be asked again;
  branch at an answer and nothing comes back, because an answer is not something to send. The view
  moving and the composer filling are waited for as two claims with two messages, because the handler
  opens the conversation before it writes the composer and a single predicate would grade whichever
  of the two settled second.
- **The host's own sentence is the receipt about memory, and it is graded per core.** A branch is only
  a branch if the model's memory followed it, and the two engines keep that memory in different
  places, so each has to say something different: pi names the thread it forked, the Rust core says it
  answers from the copied history now. The check asks `about` which core is running rather than
  reading `CWOFFEE_CORE` — the environment says what was asked for, the host says what is running.
  Emptying the pi arm of `memory_note` fails as
  `the pi core made a cwoffee row without saying it forked pi's thread, which is the memory that has
  to follow the copy`. What the check cannot see is whether the fork file's contents match the source
  branch; the bridge's own unit tests cover the comparison against `get_fork_messages` that guards
  that, and the receipt says "the core said what it did to the model's memory" rather than claiming
  the file was read.
- **The refusal claims measure the population they belong to.** A drifted `shown` must create no
  conversation *and* change no rows; a mid-turn refusal is measured while the turn is still running,
  because the running turn is going to grow the conversation by two rows on its own and a claim
  written as "nothing changed, after the turn" would blame the refusal for the turn. The turn then
  lands in the conversation it was sent from, which is what the refusal was protecting.
- **The click has to belong to the conversation the claim names.** Branch controls are painted inside
  the transcript, so a check that reloads a conversation and clicks "the last answer's branch" can be
  clicking a control built while a different conversation was on screen. `branch` asserts
  `transcript.sessionId` equals the conversation it is about to branch before clicking. That
  assertion is what found a real bug: `loadTranscript` painted a conversation's rows without telling
  the transcript which conversation they were, so Copy/Edit/Branch under freshly painted rows addressed
  whatever was on screen before the repaint. Turn actions now read the session at click time for the
  same reason.
- **`turn-buttons` keeps the disabled-control rule, pointed at the honest case.** Branch is built, so
  its button is live — but not under a turn the window streamed, which has no stored row to name.
  The group now requires that control to be inert *and* to say what it is waiting for
  (`once it is stored`), which is the same rule it was checking before: a disabled control that
  explains itself beats one that pretends.

- **The cost of the copy is claimed only when there is a copy.** A branch that carries rows says
  its first answer is assembled from them; a branch off the first message carries nothing and must
  not say so. The wording is a count, not "cold cache" — the engine cannot observe the endpoint's
  cache, and this app has a standing rule (and a test) against notes that assert cache state.

Three new mock scenarios (`branch-stem`, `branch-tail`, `branch-answer`) exist because
`scripts/mock-llm.mjs` answers an unmatched prompt with a 400, and a check whose turn 400s grades a
conversation that never ran. (`checkpoint-overwrite` and `checkpoint-slow` were added for the same
reason: the first is a turn that writes one file three times and edits it once, the second is slow
enough to still be running when the harness asks for a revert under it.)

### The tree: what a conversation is reading, and what moving that costs

`tree` is the group for a feature that lives in one column of `sessions` and is therefore invisible
from the DOM. What it has to prove is not that a card paints — it is that the answer the model gives
next comes from the node the person stood the leaf on, while nothing was copied and nothing was
deleted. A rewind-shaped implementation passes every selector check and gets that wrong.

- **The record is read as the model reads it, twice, for opposite facts.** After a leaf move the check
  asks `session_messages` what the next answer would be assembled from (zero rows, standing in front
  of the first message) and asks the sidebar's `message_count` what the conversation holds (unchanged).
  Either one alone is a trap: the first is what `truncate_from` also produces, and the second is what a
  conversation looks like when nothing happened. Sabotaging `set_leaf` to truncate fails as
  `moving the leaf deleted rows: conversation N held X before and Y after`.
- **The branch a leaf was left on grows, and the row the leaf left behind stops being read.** The
  message sent after the move is checked against the *branch* (`2 rows, 1 of them my ask, and not the
  row the leaf left behind`) and against the *whole record* (`+2 rows`). That is the claim a rewind
  cannot satisfy and a leaf move must.
- **Lineage is graded from what the creation wrote, not from what the UI can see.** Two branches cut at
  the same message are checked for parent id, role, row id, the quote captured *at creation*, and
  depth — plus the parent drawing both children and counting them. The quote matters as a stored
  fact: a branch cut at a message that is later edited has no live row matching what its card says, so
  a check that re-read the row for the quote would grade the wrong thing.
- **Both cores answer the same verb in different sentences, and each is graded for its own.** `about`
  which core is running picks the expectation: pi has to consult its thread, the Rust core has to say
  it read from the record. Emptying the pi arm of the tree note fails as
  `the pi core drew a tree without consulting pi's own thread, which is the record that says what its
  model is actually reading`.
- **The fixture is the pi-side half.** `crates/bridge/tests/fixtures/pi-tree-branched.json` is a
  captured `get_entries` answer whose *file order* is not its *tree order*: the branch is listed after
  the straight line and hangs off the first entry. A reader that walked the file would mark the
  straight line as the branch the model reads, which is the opposite of what pi's `leafId` says. The
  same fixture holds an entry no cwoffee row answers to, a piece of pi's own bookkeeping, and a branch
  nobody is reading, and the three are graded as three different facts — a line of the tree, not
  drift, and off-path. Standing the record's leaf at a node pi is not on has to read as disagreement
  about where the two records stand, not as an empty tree.
- **The refusals measure the population they belong to.** A drifted `shown` must move no leaf *and*
  change no rows; the mid-turn refusal is measured while the turn is still running, then the turn is
  checked to have landed in the conversation it was sent from. A tree read under the same turn is
  required to answer, because reading is what a watch tab does while the machine is busy.
- **`context-menu` now requires the tree control in the conversation's row menu.** It was added there
  because a conversation is what has a tree; a check that only looked for it on messages would let the
  sidebar grow a second, different way to open the same card.

Three mock scenarios (`tree-first-ask`, `tree-second-ask`, `tree-branch-ask`) exist for the same reason
the branch group's do: `scripts/mock-llm.mjs` answers an unmatched prompt with a 400, and a check whose
turn 400s grades a conversation that never ran — which is exactly how this group failed the first time
it ran.

### Checkpoints: the file, not the card, is what gets graded

`checkpoints` is the group for a feature whose whole product is a file on disk. Everything the UI can
report — the plan, the counts, the note — is the app's own arithmetic, so a revert that updated its own
card and left the folder alone would look correct from inside the app. The group therefore reads the
bytes through `selftest_file_bytes`, which goes through the same `confine()` the tools use and cannot
be pointed outside the conversation's folder, and it never takes the app's word for what the disk holds.

- **The turn has to actually write, or every claim below it is noise.** The file is seeded with words
  the harness wrote, and the first claim after the turn is that the file no longer holds them. A turn
  that wrote nothing would leave the group grading a change that never happened, which is why that
  check exists before any revert claim: `the turn wrote nothing: … still holds the words the harness
  seeded it with`.
- **`edited_since` is used as a disk witness, from both sides.** Before the revert it must be false —
  that is the engine saying the live file matches what the turn finished with. After the revert it must
  be true — the file no longer matches, which is the only way the app itself notices a file moving.
  Reading it in both directions is what stops a claim that only ever proves the flag is constant.
- **The card is graded as a decision, not a list.** It must name the one path, sit inside the reading
  card's clip box, say the transcript is untouched, and offer a button whose label matches the stakes:
  `Revert` when nothing was edited afterwards, `Revert anyway` when something was. Sabotaging `apply()`
  to report a restore it never performed fails as
  `pressing Revert left selftest_checkpoint_note.md holding "model draft two\nthird line\n" instead of
  the words it held before the turn`.
- **The messages are counted on both sides of the revert.** Undoing files must not delete the
  conversation, and the group compares row counts before and after rather than trusting the note that
  says so.
- **The mid-turn refusal is measured against a turn that is really running**, and the disk is read
  before and after the refused call: `the refused revert still moved …` is a different failure from
  `reverting during a turn answered …`, and the two used to be one claim. Removing the busy guard fails
  with the second message — and that sabotage is what surfaced a real wording bug in the refusal, which
  printed "accept that 1 it stay as they are". The engine now agrees in number with what it names, and
  both branches are unit-tested, because that clause is the one that says what did *not* happen.
- **A turn that touched no files must refuse rather than offer an empty plan.** The refusal is the same
  sentence the card would have shown, and the check asserts its wording; an undo control that opens a
  card with nothing in it reads as a bug in the card.
- **Scope is one decision per file.** Three writes to one file produce one line, naming the earliest
  copy, and widening to "this turn and after" still names one path. A per-write list would make a
  single rewritten file read as three separate things to approve.
- **The group owns its state.** Reasoning and approval mode are set for speed and restored in a
  `finally` that runs whether the claims passed or failed; the file it seeded is deleted through the
  model's own tool so the folder is left as it was found. It creates its own conversation by sending,
  rather than borrowing the suite's scratch session, because that row is deleted and re-created by the
  setup — an earlier version got `useSession(3): the host is still on 0` and then graded a turn that
  never started.

### Typing during a turn: the endpoint is the witness, not the card

`steer` is the group for a feature whose claim is about **a request**. A pending line on screen proves
the press reached the host; it does not prove the model read it. So the load-bearing assertions live in
`scripts/mock-llm.mjs`, which answers 400 when the bytes it received disagree with the scenario:

- **`require_steer`** — a needle that must appear in a user message positioned *after the last tool
  result* of the request that plays that step. That is the whole promise: into this turn, at its next
  step. A receipt painted for a message that never went out fails the turn, and the error quotes what
  the request actually held there.
- **`require_steer_absent`** — a needle that must not appear in any user message of the request at
  all. This is the withdrawal arm: "the card said it was taken back" is not evidence, and a leaked
  press makes the endpoint refuse.
- `/mock/state` now reports `steer_hits`, `steer_misses`, `steer_leaks` and `user_seen` (every user
  text the server was asked about), and `selftest.sh` prints that line under the activity table — a
  run that claimed a steer landed while the endpoint never saw one says so in numbers.
- The first steps of `steer-into-turn` and `steer-withdraw` **stream slowly before the tool call**.
  A fast first step meant the boundary came round before the harness could press Enter, and the press
  landed at the end of the answer instead of after the tool results — a scenario that grades the wrong
  moment. `checkpoint-slow` gained a second step because a steer makes that turn continue, and an
  exhausted scenario would 400 on the request that carries the pressed words.
- The card arm has a pi-core condition: the approval mode a pi process obeys is set when it is exec'd
  (`CWOFFEE_MODE` in the child's environment) and no RPC changes it afterwards, so the arm relaunches
  through a persona round trip and restores what it found. Without that it would measure a process
  running Auto and call the absence of a card a pass.

`composer-type` was flipped with the feature, not left beside it. Its mid-turn assertions used to prove
a refusal ("Enter does not send while cwoffee is answering"); they now prove a press — the placeholder
names the kind and where the words go, the box empties into a waiting line that says the message has
not been sent, no second turn starts, and the pressed words become a row of their own in the record.
The rule that a control must never be `disabled` mid-turn is unchanged, because that is the state that
made typed words invisible on this WebKitGTK.

### The file row starts where the answer's text starts

`.filestrip` carried `margin-left: 62px` from the layout where a block's label hung in a gutter and
its body was pushed past it. The gutter is gone — `.blk` is one column now — so the indent aligned
with nothing, and a turn's file row read as a stray margin while the hand-over line above it was
already flush-left. `one-attachment` measures the strip's left edge against the answer text's left
edge and logs the number (`file row aligned: its left edge is 0px from the answer text's left edge`),
so the claim is a measurement rather than a screenshot someone liked. Same rule for both cores, and
for both shapes: the card line and the working strip share the prose's edge.

### A made file is a card, and a click resolves through the conversation

Two more claims about the same row, both graded in `one-attachment`:

- **A file the turn created wears the dashed card; a file it changed is a chip.** The fact is
  `OutputFile::created`, and it can only come from the tool: `write_file` checks the disk before it
  writes, and afterwards a created file and a replaced one are byte-indistinguishable. `kind_for`
  says `edit` for both, which is why "is this a delivery?" could not be answered from the label —
  the argument the screenshots turned into was really a missing fact. Engine test:
  `a_write_says_whether_it_made_the_file_or_replaced_it` (create → true, overwrite → false, append
  onto nothing → true, edit → never true). Past `ROLL_FILES_AT` made files the card gives way to the
  working roll, so one number decides where "a few things I made" stops being a design; the check
  loads five created files and asserts no card survives.
- **The Save control is the delivery, and nothing else is.** A card outside
  `.cws/assistant_attachments/` has no control at all — not a disabled one, because a greyed-out
  Save is still an offer the host refuses. `data-handover` on the unit is what the check reads, so a
  passing run proves the difference is visible rather than merely intended.
- **A click resolves through the conversation's own folder.** `open_file` used to resolve
  workspace-relative paths against the host's *current* folder, so an app showing a conversation from
  another workspace answered `three-feature-prompt-3.md no longer exists` about a file sitting intact
  in that conversation's folder. `scratch::turn_file_path` now tries the conversation's folder, then
  the host's, and a refusal names both folders it searched instead of claiming anything about
  whether the file still exists. Tests: `a_turn_file_is_found_in_the_folder_its_conversation_lives_in`,
  `a_missing_hand_over_says_which_folder_it_searched`.

The system prompt now states the folder rule as the only route to handing a file over
(`the_prompt_says_the_attachments_folder_is_the_only_way_to_hand_a_file_over`), because the model
otherwise learns it by being contradicted: the session this all started from wrote a task file at the
workspace root and had no reason to prefer the folder.

### The hand-over folder, and the label that outran it

README has said for a while that `.cws/assistant_attachments/` holds "the only files the transcript
offers to **Save…**". It was enforced in exactly one place — `scratch::attachment_path`, which
refuses a copy request for a path outside the folder — while the *shape* of the row came from
`FileKind`, and `download_file` set that from the tool's `place` argument rather than from where the
bytes landed. So `download_file` with `place: "attach"` and `path: "/abs/x.pdf"` (or any root path)
wrote outside the folder, was labelled `Attachment`, and the transcript drew the dashed unit with a
Save control whose action the host would refuse. The reader saw a delivery affordance on the model's
own working file.

Three arms now hold the claim, and `one-attachment` grades all three:

- **The write.** `destination_for(_, Place::Attach)` has no branch that returns a destination
  outside the folder — an absolute or root `path` keeps its file name and moves in, and `place_note`
  says so to the model. The unit test walks eight shapes (`report.pdf`, `./x`, `sub/x`, `/tmp/dl/x`,
  `/etc/passwd`, `.cws/x`, `docs/x`, empty) and asserts every result passes `is_attach_rel`.
  `Keep` and `Scratch` still hand an absolute path to the confinement layer untouched, because
  rewriting a destination the person was shown in an approval card is what that card exists to stop;
  the engine test asserts that half too, in both directions.
- **The kind.** `download_kind(rel, place)` reads the landed path: inside the folder means
  `Attachment`; a `Keep` outside it is `Artifact`; anything else is `Scratch`. The old
  `match place { Attach => Attachment }` is the bug, and the test asserts `download_kind("report.pdf",
  Attach) != Attachment` so it cannot come back.
- **The paint.** The transcript re-tests the path before it draws `.attachunit` or `.attach-save`,
  because rows are re-read from the record and a session written before the fix keeps the label it
  was given. A mislabelled row becomes a chip — under the scratch group if it is loose in `.cws/`, in
  the deliverable strip if it is in the person's own folder — plus one `.attach-note` line naming the
  paths it refused to offer. `one-attachment` loads exactly that fixture: `report.pdf` at the root and
  `.cws/ref.pdf`, both still claiming `kind: 'attachment'`, and asserts no unit, no Save control, both
  named in the note, and that a row *inside* the folder keeps both.

The path test mirrors `kind_for` rather than prefix-matching: `.cws` immediately followed by
`assistant_attachments`, as components. A substring test would call somebody's
`src/assistant_attachments/` folder hand-over material, which is the mistake the engine's comment on
`kind_for` already names — a directory of source files presented as things meant to reach you.

### Two pi-core MCP claims that were true on one core only

The first full pi-core pass since the MCP work went in said red twice, and both reds were real
features rather than harness noise:

- **The card's body.** The bridge wrapped `cwtool check`'s sentence as `"<tool>: <summary>"`, while
  the Rust core prints the summary alone (`approval::summarize_with`). For a third-party call that
  put the tool name where the *server* should lead — `mcp__fake__write_note: MCP server fake: …` —
  and the group's `/^MCP server fake:/i` assertion went red on pi and green on rust. The headline
  already carries the tool on both cores (`ApprovalRequest::tool`), so the prefix named the call
  twice on one card. The bridge stopped adding it, and its unit test now asserts the server leads.
- **The per-call prompt.** `permit_covers` only asked whether the call escaped the workspace, so a
  yes given once for `mcp__fake__write_note` covered the next one. On the Rust core the gate refuses
  that (`CallRisk::per_call`); on the pi core the gate is the app answering pi's dialog, and it had
  no third-party arm. It now refuses any name the engine's own `mcp::server_of` recognises — the
  naming rule, not a copied `starts_with`.

Both are the same class of bug the pi-core protocol keeps reporting: a claim written from the Rust
core's shape, true on that core, and unmeasured on the other until a full pass ran there. The
targeted steer/theme passes this session were green on pi and said nothing about MCP, which is the
difference between "the group ran" and "the suite ran".

### A self-test run reads its own script as it goes

`bash` does not read a shell script to the end before running it; it reads at a byte offset and
keeps going. Editing `scripts/selftest.sh` while a run is in flight shifts every offset after the
edit, and the run then re-parses text it has already passed. Twice in one session that produced
`syntax error near unexpected token '('` at a line that parsed fine before and after the edit, and
once it produced a nonsense diagnostic — `cwoffee-app is older than ` with an empty path — which
looked like a broken artifact proof and was really a run reading a file that had moved under it.

The rule: **no edits to `scripts/`, `crates/` or `ui/dist` while a run is going.** Docs are safe.
Ending a run by its printed pid is fine — but start the next one only after its cleanup has released
the mock endpoint, or the new run's fingerprint probe answers from a server that is on its way out
and the board fails with `list_models returned nothing`. `selftest.sh` now asks that port twice
before reusing it, and says out loud when the answer went away.

### Theme: two homes, one value each, and a light arm that has to bite

`theme` used to be a key the parser accepted and nobody read, with the palette living in one
browser's `localStorage` and `prefers-color-scheme` sampled once at boot. The group is written so
that each half of that failure has a red line:

- **The file is the value's home.** `set_theme` is followed by a read of the config file itself,
  through the armed-only `selftest_config_text`, which answers the **host's own** path rather than a
  path the page names: a check that could read any file would be a test that reads the user's.
  `hostState.theme` alone would not prove this, and neither would the settings row quoting it.
- **An override rewrites nothing.** The window takes an override, the paint flips, and the config
  file must still say what the host stores. `theme_words({ window })` composes About's line from the
  stored value plus the override *as an argument* — that is the distinction, and a client that phrased
  the sentence itself would be a second answer to "which palette is whose".
- **Both palettes, by numbers.** Every floating surface (`.pop`, `.toast`, `.approval-card`,
  `.pv-head`/`.pv-body`, `.revert-card`, `.tree-card`, `.ledger-card`, the narrow drawer) is read with
  the document painted in that palette, twice per palette: how far it sits from the reading surface
  behind it, and whether that palette's ink is legible on it (contrast under 4.5:1 fails). Both on one
  ruler — oklab L scaled to 0..255, because this engine answers a light `color-mix()` as
  `color(srgb ...)` and the dark palette's as `oklab(...)`, and comparing the two arithmetic systems
  grades arithmetic instead of ink. Colour names are never asserted: lesson 132 is that "lighter" is
  not a direction both themes share.
- **The ink has to be read in the same switch as the surface.** The first version measured the ink
  after putting the palette back, so the dark loop compared a dark surface against the light palette's
  ink and printed a `contrast 1.00` failure that meant nothing. Everything for one palette is read
  inside one `withPalette` switch now, and the failure it produced is the comment in the check.
- **The light arm bites.** `selftest.sh --theme light|dark|system` sets `CWOFFEE_SELFTEST_THEME`, the
  armed run reads it from `selftest_config`, and it applies as an *override* — the arm grades a paint,
  it does not change a setting. One launch paints one palette, so a full pass relaunches the `theme`
  group in the light palette and folds that arm into the verdict. Sabotage, and the reason the arm
  exists: `background: #1e1d1b` written into `.pop`'s rule — the dark palette's paper, hard-coded —
  and the light arm reported `contrast 1.98, under 4.5` on the menu's own surface while the dark run
  stayed green. Separation alone would have passed it: that surface sits 223 of lightness from a
  near-white page. A colour-name assertion would have caught none of it.
- **A repaint waits for a turn.** The reload-painting lesson in this file says a screen that changes
  mid-answer costs the answer its layout. The group sends a turn, changes the palette during it,
  asserts the turn is *still running*, asserts the note said the repaint was waiting, and asserts the
  paint landed when the turn ended.
- **Live following, when the engine allows it.** The arm dispatches a synthesized
  `MediaQueryListEvent` and reads the paint; on this machine it arrived, and the ok-line ends with
  "the media query was observed following". Where it cannot be synthesized, the check prints a skip
  note, grades the listener's presence (`data-theme-listening`) and the explicit arms, and the
  ok-line says which half it measured. README carries the same condition rather than a bare promise.
- **The retired keys.** `theme_loud` and `theme_aggressive` must still refuse, and the refusal must
  name the key — `prefs.rs`'s table is the whole vocabulary, and a drawer any client can write into
  is not a setting.

### The terminal: a pty, the disk, and no screen grading

Nothing in this suite used to touch `crates/tui`. It shares the engine, so every feature that grew a
door in the window grew none in the terminal — and no check could see the difference, because no
check was looking. Group `tui` is `scripts/tui-gate.mjs`: it drives `target/release/cwoffee` on a pty
(its own tmux server, on a socket named for the run, so it cannot reach anyone else's) against
`scripts/mock-llm.mjs`, and `selftest.sh` builds `-p cwoffee-tui` in the same cargo line as the app
and refuses to run the gate when the binary is older than `crates/tui`.

The rule for this group is that **the screen is not evidence**:

- the turn's file is read off disk before `/undo`, and again after it, and the after-bytes must be the
  words from *before the turn* — not what the terminal said it restored;
- `/branch` is graded by the store: one new session row, and the original conversation's rows
  byte-identical, so a branch that edited instead of copied cannot pass;
- the mid-turn press is graded by `/mock/state`'s `steer_hits` and by exactly one new user row;
- `explore`'s MCP answer is graded from the **tool row in the store** — what the model was given —
  because the viewport may have scrolled past it by the time the gate looks.

The one place the screen *is* the claim: `/files` must name the path, and the composer must hold the
words `/branch` handed back. Both are read from the listing's own block and the composer's own line.
Matching the transcript instead would grade the tool card, which quotes the same path in its
arguments, and would prove nothing about the door.

Three things this group exists because of:

- **tmux spells control keys `C-x`.** `send-keys ctrl-u` types the literal characters `ctrl-u`. The
  first version of the gate "cleared the composer" and left those bytes in it, so the card's `y`
  arrived with words in the box and did nothing, and the run graded a denial as an approval. The gate
  translates the names, and the failure it caused is why the composer assertions read the box rather
  than trusting that a keypress means a key.
- **A card that swallows typing is an unreachable queue.** The terminal handed every keystroke to a
  live approval card, so a message typed during a turn went nowhere — the opposite of the window,
  where typing goes to the composer and Enter queues beside the card. The gate presses Enter *with*
  a card up and asserts the card is still up, the press queued, `Ctrl-U` empties the box, and only
  then does `y` answer.
- **Engine notes on stderr corrupt a painting terminal.** Observed, not reasoned about: a boot note
  painted through the header and the pane read `Cwoffee  ccwoffee-mockt ·20sessionn0`. fd 2 is now
  parked in a log file while the screen is up and printed when the terminal closes.

Two claims the gate declines, recorded rather than assumed: the boot lines are not observable on a
pty (the alternate screen eats them), so the hub's state is graded from `/mcp`, and the gate prints a
note saying which half it measured; and there is no `/diff`, so the terminal's file comparison stays
in README's "Not built" instead of being implied by the door list.

### One scenario-expansion bug that cost a run twelve failures

`$path` expands to the last path-shaped token of the prompt, and the env block ends with `</mcp>` —
which contains a slash. On a pi-core run with MCP servers configured, `write_file` was handed
`</mcp>` as its path. The fixture then wrote a file nobody read: the `require` needles failed, chips
were missing, `read_file` loops stopped early, and the history-fill turns stored four rows instead of
fifty-six. Twelve failures, seven groups, one cause, and none of it about the change under test.
`extractPath` now strips `<env>`, `<mcp>` and `<tools>` before looking and refuses any token with
angle brackets in it. The general rule: **a fixture that reads the whole prompt reads the harness's
scaffolding as part of the person's words.**

### The file ledger: three ways a copy can be gone, and one byte total that can lie

`ledger` grades the read over the checkpoint machinery — the same index rows, the same
content-addressed store, the same `line_change` the tool card paints. Reading is the feature, so the
group's job is to make a ledger that agrees only with itself fail. Like the checkpoints group it reads
the bytes through `selftest_file_bytes` and never takes the app's word for what the disk holds.

- **Three turns that really write.** Turn 1 creates `selftest_ledger_note.md`, turn 2 edits it, turn 3
  overwrites `selftest_ledger_twin.md` — which the harness seeded with the *same bytes* the note held.
  That seeding is the point: turn 2's copy and turn 3's copy are then the same blob, so
  `snapshot_stats` has a dedupe to get right. `blobs` must be 1 while `row_bytes` is twice
  `blob_bytes`, and a stats that summed the rows fails as
  `the store counts 2 blob(s) for two copies of the same bytes`. Without the shared blob the dedupe
  claim grades nothing.
- **The kinds are read from the record, not inferred from a diff.** `created` on turn 1 and `modified`
  on turn 2 for one path, `modified` on turn 3 for the other, both in the payload and on the card as
  `data-kind` attributes — so a card that drew the flags in the wrong order or folded two turns into
  one line is caught by the DOM as well as the read.
- **The diff is graded against bytes the harness knows.** The stored side is what turn 1 wrote and the
  live side is what turn 2 wrote, both read from disk; the card's `.diff-del` must quote
  `-the shared bytes` and its `.diff-add` `+the edited bytes`. A `line_change` that diffed a copy
  against itself, or against the wrong row, fails here rather than looking tidy.
- **The identity guard is graded on a row the card just listed.** Asking for the note's row under the
  twin's path must refuse with "Nothing was diffed", and both files must still hold their bytes: a
  guard that returned the newest copy for that path would answer successfully and lie about which turn
  it described.
- **Both refusals that make a one-sided diff are refused, not drawn.** A row that *created* the file
  and a row whose blob was deleted out of band have one side each; printing the live file as a diff
  would read as every line being added by somebody. The engine tests cover the out-of-band arm, the
  cap arm and the forget arm — three reasons a copy is missing, told apart by
  `checkpoint_prunes.reason` (`cap` / `forget`) and by `copies_missing`, which is computed from the
  disk. Reaching the cap in the selftest would mean 200 file calls in one turn, so the cap is graded in
  `ledger::tests::an_evicted_row_leaves_a_line_that_offers_nothing` and the notice line is graded here
  through the forget path, which renders the same rows.
- **Deleting is graded as a decision with a witness.** The control needs two presses (`Press again to
  delete 3 copies`), refuses mid-turn with the record and folder unchanged, and on the second press
  must free bytes, leave `orphan_blobs` at 0, leave the workspace files holding their bytes, leave the
  message rows counted, make `checkpoint_preview` refuse, and leave the card reading "Nothing to
  forget" with a notice saying who removed the copies. Sabotaging `forget_checkpoints` to delete
  without the reach check fails as `conversation b's copy is still on disk` in
  `a_blob_another_conversation_points_at_is_not_freed`; summing row bytes instead of blob bytes fails
  as `two rows, one blob, so the freed bytes are one file's` in the gateway test.
- **The group owns its state.** Reasoning and approval mode are set for the tool turns and restored in
  a `finally` that runs whichever way the claims went, and the two files it made are deleted through
  the model's own `exec` so the folder is left as it was found — a check that leaves files behind in
  somebody's folder is the check doing the thing the feature exists to prevent.

Four mock scenarios back it (`ledger-write`, `ledger-edit`, `ledger-twin`, `ledger-slow`): the mock
answers an unmatched prompt with a 400, and a ledger group whose turns 400s would grade a conversation
that wrote no files at all.

### Looking at the page: how a design pass is actually seen

A suite that grades rectangles never shows anybody what the thing looks like. When the browser
column was redrawn — icons instead of words, an address field, tabs, an idle ring — the geometry
checks (`chrome-height`, `browser-panel`) said the column was the right height and every glyph had
a name; they said nothing about whether the empty state read as an empty page or as a bug.

The way to see it without touching the user's desktop:

```
node scripts/mock-llm.mjs --port 8795 --model cwoffee-mock &      # probe it a few times before trusting it
target/release/cwoffee-gateway --port 8199 --url http://127.0.0.1:8795/v1 \
    --model cwoffee-mock --workspace /tmp/gvis/ws --db /tmp/gvis/data/store.db --mode auto &
node browser/driver.mjs < request.json      # writes a PNG at screenshot_path
```

`request.json` is one JSON object: a `url`, a `steps` array (`fill` the composer by its CSS
selector, `press` Enter, `click` `[data-browser-toggle]`, `wait`), and
`"screenshot":"full","screenshot_path":"/tmp/gvis/panel.png"`. The driver is the same one-shot
browser the `browse` tool uses, so the picture is a real browser's rendering of the built page, not
a mock-up. Typing the scenario phrase into the composer makes the model open a kept browser, and
the column then holds a real page: header, address row, tabs, idle ring, page pixels.

Two honest caveats, both written down rather than worked around:

* **what this shows is the watcher.** The gateway's `browser_panel` payload sets `writable:false`, so
  the address field and Go are disabled and the footer says the assistant is the only one who can
  open a page. That is the phone-tab variant of the same component; the writable variant — address
  box live, keys row, two-press close — is graded by the board inside the desktop window, which this
  route cannot reach.
* **a photograph is not a check.** The PNG is for judgement, not for grading. Nothing in the suite
  compares pixels; the claims stay the geometry and the verbs, because a screenshot of a window at
  one size proves nothing about the next one. (There is no way to photograph the desktop window
  here either: GNOME on Wayland refuses `import -window root` and the Shell's screenshot D-Bus method
  answers `AccessDenied` for a caller like this. That is why the board measures rather than looks.)

### Typing in the composer: what a suite without pixels can honestly claim

The report was that after a turn ended, typed characters did not appear in the composer while the
send button still unlocked and the text still sent — words present in the control and in state,
absent from the screen. `composer-type` grades the state that caused it and everything a page-side
check can reach, and says what it cannot reach:

- **The control must never enter the state that mis-paints.** `disabled` is gone from the composer;
  the claim is that no point in the turn finds it disabled, before the turn, during it, and after it.
  Putting `disabled={busy}` back fails as `the composer went disabled during the turn — typed text is
  invisible from there`. A claim about a paint bug has to be a claim about the *condition*, because
  the pixels are not observable here.
- **DOM and state must agree every keystroke.** Typing is one character at a time with the `input`
  event a real key produces — assigning `value` once would miss a handler that overwrites the box
  mid-word — and each step compares `textarea.value` with `draft`. A split is the failure a user
  describes as "I typed it and nothing happened": `the box holds "abc" and the app holds "" — one of
  them is lying, and the screen shows the box`.
- **Layout growth is the closest witness to "drawn".** Six lines must make the box scroll
  (`scrollHeight > clientHeight`; the passing run reports `150px of 85px`). If the control never laid
  the words out, nothing could draw them. The numbers are captured at the moment of measurement: an
  earlier version sampled the emptied box afterwards and printed `85px of 85px` under a claim about
  six lines.
- **Mid-turn claims need a turn that is verifiably running.** `busy` is asserted before Enter is
  pressed, and the group refuses outright if it has already cleared: the first run used a 1.4s
  scenario, the turn ended during the typing loop, and four mid-turn failures fired against an idle
  composer.
- **Enter mid-turn refuses and explains itself.** No turn count change, the words still in `draft`,
  and a note that says they are kept — `Enter during a turn did nothing and said nothing — that reads
  as a lost keystroke` is what a silent refusal fails as.
- **The typed words reach the record.** After the turn the same box is typed into again, Enter sends,
  and `session_messages` is read: the row must hold exactly the composed text. The card of a send is
  not evidence; the row is.
- **Blind is said, not skipped.** A frame clock is run alongside the measurements; when the page was
  given no frames, the ok line marks the run geometry-only rather than implying it saw the screen.

