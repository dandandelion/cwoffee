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
| In-app harness | `./scripts/selftest.sh` | **scripted HTTP** | the real webview: reveal, commit, follow, markdown mid-stream, cards, popovers, sessions, gateway |
| Gateway | `node scripts/gateway-smoke.mjs` | none | auth, roles, workspace rules, boot shape from a phone |
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

## What the newest checks measure

Three checks exist because a user read a symptom, not because a number moved:

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
