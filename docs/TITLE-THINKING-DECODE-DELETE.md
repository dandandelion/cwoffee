# Four asks, 2026-10-25 (round 2) — working doc

Same discipline as `docs/UI-TASKS.md`: the user's wording stays here, each item's **Status** line is
updated as it moves, and nothing is deleted. Verify with targeted groups
(`./scripts/selftest.sh <group>`); a targeted green covers only the groups that ran.

| # | Item | Status | Groups that cover it |
|---|---|---|---|
| 1 | Kill the borrowed titler; name a conversation with a stalled first request | **done and green on both cores, page check included** | `titling`, `delete`, `drafts`, engine `titling::*` + `host::a_new_conversation…` |
| 2 | Changing reasoning effort appears to do nothing | **done: pi clamped every level to `off` because the model entry declared no thinking; the offered levels are now published and graded** | `thinking`, `wire-names`, `scripts/pi-reasoning-gate.mjs`, bridge `mount::the_pi_entry_declares…` |
| 3 | Decode rate counts tool execution, so it reads too slow | **done on both cores** | engine `agent::the_decode_window_is_generation_time_not_wall_time`, bridge `translate::the_decode_window_skips_a_tool_run…` |
| 4 | Deleting the open session leaves its transcript on screen | **done** | `delete` (page), engine `host::a_deleted_conversation_does_not_take_the_turn_that_raced_it` |

---

## 1. Titling — DONE

**User asked:** "Re-do our titler. We'll completely remove reusing another model to title our
conversation (completely remove, delete files and code). New title technique, stall first request by
attempting sending the first request for example like this: `Title this conversation in under ~10 words
and say nothing more: <very first user message>` params: thinking off (tho we need to take into account
models who doesn't support thinking efforts (or off) so we need to parse the response still to make
sure). So behavior will be, first request stalls to request for the title (no system prompt + at this
point, the new conversation created is titled "New Session"), then once we receive, we update the title
(animate it like a typing animation, with a caret or something?). Important: title request and very
first actual user request is a separate context."

### What was removed
`crates/engine/src/titlegen.rs`, `scripts/titlegen.py`, `scripts/gateway-titlegen.mjs`, the
`pub mod titlegen;` line, the seven `titlegen*` config keys and `Config::titlegen_cfg`, the
`resolve_exec` helper that only they used, the `titlegen_status` verb in both the gateway and the
desktop shell, the bridge's `TitleGen` field and its after-turn `maybe_name` call, the Settings
cost sheet (model, paths, weights, ~870 MB), `titleInfo`/`loadTitles`/`TitleStatus` in the page,
the boot warning, and the after-turn `maybe_name` call in the Rust host. `PI-CORE-PLAN.md` keeps
its measurement log under a **Superseded** note; `README.md` was rewritten to the new behaviour.

### What replaced it — `crates/engine/src/titling.rs`, used by both cores
* **Placeholder:** the store's `UNTITLED` constant is now `"New Session"`, and both cores create
  the row with it. The store no longer cuts the opening line into the title on `append_message`
  (`store::a_new_row_keeps_its_placeholder_until_someone_names_it`), because that write raced the
  animation and named the row before the conversation had been understood.
* **The ask:** `TITLE_ASK` = `"Title this conversation in under ~10 words and say nothing more:"`
  followed by the whitespace-collapsed first message. One `ChatRequest`: one user message, no
  system prompt, no tools, `Reasoning::Off`, `max_tokens 96`.
* **Separate context, proved rather than claimed:** the ask is never appended to the history, and
  it goes through the engine's own `Provider`, so the auth and chat-template knobs are the ones the
  real turn uses. `titling::the_ask_is_one_user_turn_with_no_system_prompt_and_thinking_off`
  asserts message count, tools, reasoning and prompt; `host::a_new_conversation_is_named_before_it_is_answered`
  asserts the order (name before `TurnStart`) and that the ask never becomes a stored row.
* **Parse anyway:** `parse_title(text, reasoning)` reads `content` first-line-first, then the
  thinking block **last**-line-first (a model that thinks anyway arrives at its title at the end),
  strips fences/thinking tags/`Title:`-style preambles/quotes/trailing punctuation, caps at 10
  words and 72 chars. `titling::a_model_that_thinks_anyway_still_names_the_conversation` drives an
  endpoint that answers *only* in `delta.reasoning_content`.
* **The stall is bounded:** `TITLE_BUDGET_MS = 8000`, a timeout is a `FellBack`, not an error.
* **Fallback, not silence:** on a miss the row takes `store::derive_title(first_user)` — the
  opening-line cut — because "a sidebar of identical rows says nothing" is still true, and the
  reason goes to `[cwoffee/titling]`. `host::a_title_that_never_arrives_falls_back_to_the_opening_line`
  covers it, through a fixture hook (`no title please` in the first message makes the scripted
  endpoint answer the ask with nothing).
* **One ask at a time:** `TitleSlot` (an `AtomicBool`) — no longer guarding 900 MB, now guarding
  prompt cost and the number of conversations that can be stalled at once.
* **The typing animation:** new event `EngineEvent::TitleDelta { session_id, text, done }` where
  `text` is **cumulative**. The live paint goes through the same `parse_title` as the settled name,
  so a row never shows a `Title:` preamble that later disappears. The page keeps the in-flight name
  in `namingId`/`namingText` rather than writing it into `sessions`, because a `loadSessions()`
  landing mid-name would put "New Session" back and the animation would read as a glitch. The caret
  is `.sess-title .type-caret` (`@keyframes typecaret`), static under reduced motion, and it
  outlives the last token by 1.4 s so a settled name does not look cut off mid-word.
* **Mock support:** `conversation-title` is the **first** scenario in `tests/scenarios/scenarios.json`
  (first `match` wins), replying `"Selftest title, typed in"` in 7-char chunks with 30 ms delays so
  the page sees several paints. No run spends the reader's cache.

### The two cores
The pi core cannot hand the ask to pi — pi would answer it inside the conversation's own context.
So the bridge builds the endpoint itself (`Config::build_provider` from cwoffee's own config) and
calls `titling::ask_title` in `Engine::name_conversation`, mirroring the Rust host: same prompt,
same parser, same fallback, same events. `title_for(0, …)` now returns the placeholder instead of a
60-character opening line.

## 3. Decode rate — DONE on both cores

**User asked:** "Decode seems off … I think it accounts even tool calls that take time (actual
execution) — so decode speed looks slower than the engine actually is."

The rule, in both cores: the window a decode rate is divided by is **generation time**, built where
the deltas land, not `end − first delta`.
* Engine (`agent.rs`): sum the gaps between consecutive text/reasoning deltas, each capped by
  `GEN_GAP_CAP_MS = 500`, then `st.derive(gen_ms)`. The cap is the whole argument: a decoder has to
  fall under 2 tok/s before a legitimate inter-token gap reaches 500 ms, while a tool run and the
  next round's prefill are multiples of a second.
* Bridge (`translate.rs`): same window, plus the anchor is moved at `tool_execution_end` (pi reports
  a tool result once, at the end, so that is the boundary) and at `dialog_resolved`, and a run
  parked on a person adds nothing at all. `server_decode_tps` for the pi core is now
  `completion_tokens / gen_ms` — it used to be `completion_tokens / (end − first_delta − parked)`,
  which charged the tool run *and* the next prefill to the model's typing speed. This was the
  number the user was looking at, because the installed app runs the pi core.
* Tests: engine `the_decode_window_is_generation_time_not_wall_time` (40 deltas around a 1.5 s sit:
  honest ~84 tok/s, broken ~28, and the window must stay under 900 ms); bridge
  `the_decode_window_skips_a_tool_run_and_the_next_rounds_prefill` (3 s of tool + 2 s of prefill on
  ~1.9 s of generation: honest ~33 tok/s, broken ~12) and `a_run_parked_on_a_person_is_not_generation`.
* `TurnStats::derive` now takes `gen_ms` and says so in its doc: passing a turn's wall clock there
  reports the sandbox's runtime as the model's typing speed. `ttft_ms` is unchanged and still wall
  time, because that is the number the reader is waiting for.

## 4. Deleting the conversation on screen — DONE

**User asked:** "Delete doesn't remove its transcript / delete the current/selected/loaded session."

* Page: `removeSession` and `deleteRows` now let go of a deleted conversation that was being viewed
  (`showStartScreen()`), instead of leaving a transcript painted from rows that no longer exist with
  the composer still addressed at that id.
* Both cores check the row *just before the turn*, which is where the race actually lands: the
  desktop's delete moves the host off the row with a **queued** command, so a send that raced it
  finds the host standing in a deleted row. Engine: `store.session(session_id)` missing → say so, and
  start a new conversation rather than writing into one nothing lists. Bridge: same test on `self.row()`
  before creating.
* Engine test `a_deleted_conversation_does_not_take_the_turn_that_raced_it` asserts the turn lands in
  a live conversation, that the message survives, and that the reader is told where it went.
* Page group `delete` was rewritten so its premise is a **loaded** transcript: it sends a turn, opens
  the row, checks a turn block is on screen, deletes through the row menu, and then requires the
  transcript to be empty and `viewId` to be null.

---

## 2. Reasoning effort "doesn't take effect" — root cause found, fixed on the pi core

**User asked:** "Changing reasoning effort in [the dial] doesn't take effect? (maybe reasoning is off
entirely when reasoning is turned off? idk if it's only not taking effect in `off` mode)"

It was not an `off`-mode quirk, and it was not the Rust core. **The dial was wired to nothing on the
pi core — at every level.**

1. The installed app runs the pi core. The pi core asks pi to set a thinking level; pi answers with
   what it settled on, and cwoffee reports that back as applied.
2. The model entry cwoffee writes into pi's `models.json` (`mount::write_dev_provider`) carried
   `id`, `input`, `contextWindow`, `maxTokens` — and nothing about thinking.
3. pi-ai: `getSupportedThinkingLevels(model)` returns `["off"]` for any model whose entry does not
   say `reasoning: true`, and `clampThinkingLevel` maps whatever was asked onto that — *before the
   request is built*. So `set_thinking_level` succeeded, the UI said `low`, and the endpoint never
   saw a thinking parameter at all.
4. The Rust core was fine: `Flavor::LlamaCpp` sends `chat_template_kwargs {enable_thinking,
   reasoning_effort}` straight from the level. Which is why the same dial worked in a Rust-core run
   and did nothing in the installed app.

### The fix: declare the dialect on the entry (`mount::reasoning_decl`)

* `llama.cpp | llamacpp | local` → `reasoning: true`, `compat.thinkingFormat: "chat-template"`,
  kwargs `enable_thinking = {$var: thinking.enabled}` and `reasoning_effort = {$var: thinking.effort,
  omitWhenOff: true}` — the same pair the Rust core sends, and `high → xhigh` because that is what
  this endpoint's template accepts, matching `llama_effort`'s folding. A conversation thinks the same
  amount whichever core is behind the window.
* `openai` → `reasoning: true`, `thinkingFormat: "openai"`, `supportsReasoningEffort: true`, which is
  pi's route to a top-level `reasoning_effort`.
* A level the endpoint never offered is written as `null`, which is pi's own "do not offer this" —
  better than sending a value the server 400s on.
* No probe data is not the same claim as no thinking: the dialect's documented values go in
  (low/medium/xhigh for a template, low/medium/high for OpenAI), stated as the dialect's values and
  not as a claim about this server.
* A kind cwoffee cannot speak (anthropic, unknown) gets no fields — and the boot line says pi will
  clamp every level to off for that model, rather than leaving it to be discovered as a dead dial.

### Pins

* `mount::tests::the_pi_entry_declares_the_thinking_levels_the_rust_core_would_send` compares what
  `write_dev_provider` writes against `tests/fixtures/pi-model-entry.json`.
* `scripts/pi-reasoning-gate.mjs` feeds that same fixture to **pi's own** functions and pi's own
  compat schema, and carries a negative control that asserts the old entry shape still clamps to
  `off` — so the premise of the fix cannot rot unnoticed. Green line:
  `pi reasoning gate ok (["off","minimal","low","medium","high","xhigh"] offered for cwoffee's entry;
  high→xhigh, effort omitted when thinking is off; compat validates against pi's own schema)`.

### Found on the way, and worse than the bug being chased

The pi core's **title** provider re-read the config with `Config::load` and stopped there.
`Config::load` does not apply the environment — the desktop calls `apply_env()` separately — so with
no config file present the endpoint was `Config::default()`, whose base url is the documented local
server `http://127.0.0.1:8090/v1`: the reader's own model. In a pi-core pass that stalled 42
conversations by 8 s each (`[cwoffee/titling] conversation N keeps a fallback name: no title inside
8000 ms`) and, on a normal machine, would have made 42 real requests to a model the reader pays for —
a test spending the reader's cache, which is the one thing this harness is not allowed to do. Fixed by
`title_provider_at()`: load → `apply_env()` → build, with the test
`the_title_endpoint_follows_the_environment_the_turns_follow` and a once-per-process log line naming
the endpoint that is asked for names.

### Also fixed: the settings screen was describing the wrong endpoint

The same skipped step, in the other direction: `provider_status_with` built its picture from the file
alone, so the Models page offered to discover, probe and rewrite config for
`http://127.0.0.1:8090/v1` while every turn of that run went to the mock on `:8190`. A reader who
clicked it would have pointed the app's own config at a server the app was not using. The picture now
applies the environment the way the desktop does at boot, with the test
`config::the_endpoint_picture_honours_the_environment_the_app_answers_through`.

### What answers "did it take effect?" from inside the app now

The host publishes `reasoning_levels`: the levels **this model says it can answer for** - pi's own
`get_available_thinking_levels` on the pi core (a question to the model process, not a model call, so
it costs nothing and spends no prefill), and the probed-or-documented levels on the Rust core
(`dial_levels`, whose test keeps "probed", "documented" and "unknown" from blurring into one
another). The Thinking effort note on the settings page now says, in one sentence, either that the
model offers this level, that it does not - *"the dial is set, but the level is not reaching the
wire"* - or that nobody has asked it. The `thinking` check fails if the dial offers a level the model
will not run, or if the conversation is set to one it does not offer.

### Still open

* The Rust core's outgoing body has never been inspected end to end against the real endpoint. The
  dialect is chosen by `provider.kind` in cwoffee's config, not by auto-detection, so a mislabelled
  kind picks the wrong branch silently; the settings screen does show the kind, and a wrong one now
  has a second symptom (pi clamping) that points at the same field.

---
