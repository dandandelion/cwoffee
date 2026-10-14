# pi-core protocol map — cwoffee frames ⇄ pi RPC

Verified against **pi 1.2.0** (`pi --version` → `1.2.0`) on 2026-10-14, using the spike in
[`../spike/`](../spike/). Anything marked **observed** came out of a real run on this box;
anything marked **doc** comes from pi's own protocol docs
(`pi/packages/coding-agent/docs/`: `rpc.md`, `rpc-commands.md`, `json.md`,
`rpc-extension-ui.md`, `session-format.md`).

Rule of the design: **the UI and the gateway keep speaking cwoffee frames.** pi's protocol is
the bridge's problem, not the webview's. See `PI-CORE-PLAN.md` decision 3.

---

## 1. Process and framing

| Item | Value (observed / doc) |
|---|---|
| Launch | `pi --mode rpc --no-session --provider <p> --model <m> [-e ext.ts]` |
| Transport | JSONL: one JSON object per line on stdin/stdout, **LF** terminated |
| Correlation | optional `id` string on a command is echoed by its `response`; correlate by id, not order |
| Readiness | `get_state` answered **176–311 ms** after spawn (3 runs, **observed**) — spawn per workspace is affordable |
| Idle footprint | **72.5–74.6 MB PSS** with one extension loaded, 11 threads, VmHWM 114.6 MB (**observed**) |
| Stderr | diagnostics only; never parse it as protocol |
| Shutdown | close stdin ⇒ orderly disposal (doc). Handle unexpected exit + `extension_error`. |

**Framing hazard (doc, and a trap we nearly fell into):** do not use Node `readline` — it also
splits on `U+2028`/`U+2029`, which are valid inside JSON strings. Read bytes, split on `\n`,
strip an optional `\r`.

**Run lifecycle (doc + observed):** a successful `prompt` response means *accepted*, not done —
`data.disposition` was `{"disposition":"started"}`. `agent_end` closes one low-level run;
**wait for `agent_settled`** (`{"aborted":false}` observed) before calling the turn finished,
because retries, compaction recovery, and queued messages can follow. Our `HostState.busy`
must key off `agent_settled`.

---

## 2. `EngineEvent` ⇄ pi (what the UI renders)

Our event enum lives in `crates/engine/src/event.rs`. Left column is what `ui/` and `gateway`
already consume; right column is where the bridge gets it from — **⚠ = we must synthesise it,
pi does not emit it.**

| cwoffee `EngineEvent` | pi source | Notes |
|---|---|---|
| `TurnStart { model, session }` | `turn_start` + bridge state | ⚠ pi's `turn_start` carries **no fields**; model id + session id come from bridge state (`get_state`, and our session-identity map). The UI labels the reply with it and refuses another conversation's thinking — keep that. |
| `Text(String)` | `message_update.assistantMessageEvent` `text_delta` | observed `delta:"1\n2"`, `"\n3\n"` … |
| `Reasoning(String)` | …`thinking_delta` | observed; keep it out of `Text`, collapsible block only |
| `TurnEnd` | `turn_end` (`message`, `toolResults`) then `agent_settled` | markdown commit happens here, once, per PLAN §9 rule 3 |
| `ToolCallBegin {name}` | `assistantMessageEvent.type = toolcall_start` (`contentIndex`,`id`,`toolName`) | observed id was **`mock-read-1`** — pi preserves the provider's tool-call id. Tool cards key on it. |
| `ToolCallArgs(String)` | …`toolcall_delta` (`delta` = arg JSON fragment) | accumulate client-side as today |
| `ToolCallReady` | …`toolcall_end` (`toolCall` complete) | our repair-and-parse step becomes a **fallback**, since `toolcall_end` is authoritative |
| `ToolCallParseError` | ⚠ bridge-only | only reachable via our repair path on `toolcall_delta` accumulation |
| `ToolExecStart {name}` | `tool_execution_start {toolCallId,toolName,args}` | observed |
| `ToolExecOutput {stream,chunk}` | `tool_execution_update {partialResult}` | ⚠ our `Stdout`/`Stderr` split must come from the tool's own envelope, not pi |
| `ToolExecEnd {ok,exit,duration}` | `tool_execution_end {isError,durationMs,result}` | observed `isError:false, durationMs:403–405` — dominated by the spike's 400 ms approval delay; the Rust shim is single-digit ms |
| diff badge `+N -M` / `TurnFiles` | ⚠ bridge/tool envelope | pi reports results, not workspace diffs; our Rust tools keep reporting their own changes |
| `ApprovalRequested {id,…}` | `extension_ui_request {method:"confirm"/"select"/"input", timeout}` | ⚠ our `id` is ours: keep a map extension-UI-id ⇄ gateway approval id so `respond_approval` resolves the right one |
| `QuestionRequested {… question_set}` | `select` / `input` (one at a time) | ⚠ multi-question cards: OPEN QUESTION 6 |
| `QuestionAnswered` / `QuestionExpired` | `extension_ui_response`, or pi's `timeout` auto-resolve | pi auto-resolves to `undefined` on timeout — that *is* our fail-closed expiry |
| `SessionChanged {id}` | `switch_session` response + `session_info_changed {name}` | keep our ordering rule: the UI reloads on this event, it does not infer the switch |
| `TurnStats(TurnStats)` | ⚠ **bridge-synthesised** | pi's `usage` has **no `ttft`** (verified: no `ttft` in `packages/ai/src/types.ts` nor `api/openai-completions.ts`; our probe read `null`). Bridge computes TTFT from first delta arrival + character-derived rate |
| `Usage(TurnTelemetry)` | `message_end.message.usage` | ⚠ math changes: see §5 |
| `Note(String)` / `Error{…}` | `extension_error {extensionPath,event,error}`, provider errors in the message stream, `auto_retry_*` | map pi retries to our `Warn`-style notes |
| `FinishReason {Stop,ToolCalls,Length,Cancelled,Error,Other}` | `assistantMessageEvent.type = done/error` (`reason`) | `Length` matters: pi treats an early `stopReason:"length"` as a compact-and-retry trigger, so our "turn ran out" note must not double-report it |

### Full pi event inventory (doc: `json.md`)

`agent_start`, `agent_end {messages,willRetry}`, `agent_settled {aborted}`, `turn_start`,
`turn_end {message,toolResults}`, `message_start`, `message_update {usage,assistantMessageEvent}`,
`message_end`, `tool_execution_start`, `tool_execution_update`, `tool_execution_end`,
`queue_update {steering,followUp}`, `entry_appended`, `session_info_changed {name}`,
`thinking_level_changed {level}`, `compaction_start {reason}`,
`compaction_end {reason,result{summary,firstKeptEntryId,tokensBefore,estimatedTokensAfter,usage},aborted,willRetry}`,
`auto_retry_start {attempt,maxAttempts,delayMs,errorMessage}`, `auto_retry_end {success,attempt,finalError?}`,
`summarization_retry_scheduled`, `summarization_retry_attempt_start {source,reason?}`,
`summarization_retry_finished`, `bash_execution_update {id,delta}`,
`extension_error {extensionPath,event,error}`.

`assistantMessageEvent` subtypes: `start`, `text_start {contentIndex}`, `text_delta
{contentIndex,delta}`, `text_end {contentIndex,content}`, `thinking_start`, `thinking_delta`,
`thinking_end {content}`, `toolcall_start {contentIndex,id,toolName}`, `toolcall_delta {delta}`,
`toolcall_end {toolCall}`, `done {reason,message}`, `error {reason,error}`.

> Note `text_end`/`thinking_end`/`toolcall_end`/`message_end` carry **authoritative complete
> content**, while deltas are incremental. That matches our append-only reveal + commit-in-place
> contract almost one-to-one, and it is the strongest argument for the swap on UI-quality grounds.

---

## 3. `HostCmd` ⇄ pi RPC commands

| cwoffee `HostCmd` | pi RPC | Notes |
|---|---|---|
| `Send {text, attachments, session}` | `prompt {message}` (+ `steer`, `follow_up` — new capability we did not have) | ⚠ the `session: Option<i64>` **assertion is ours** — and it is *addressing*, not a mismatch check: naming a conversation **moves the bridge to it** (the Rust core does exactly this, and its `resume` is what makes it work), while an id with no stored row is refused. A silent no-op is the failure mode to design against: two clients would otherwise edit each other's conversation. pi has no such notion at all. Attachments: pi handles images/files via message content + its own image limits — **not wired in S1.1**, which refuses them with a note. |
| `Cancel` | `abort` | |
| — (new) | `steer`, `follow_up`, `clear_queue` | pi's mid-run steering/follow-up queue is a feature the Rust core never had; `queue_update` feeds the UI |
| `SetMode {ask,auto,allow}` | `tool_call` handler + `ctx.ui.confirm` in our extension | our three-way mode lives in the bridge/extension, not in pi |
| `SetReasoning {off..xhigh}` | `set_thinking_level` (+ `cycle_*`, `get_available_thinking_levels`) | availability is model-dependent; ours was static |
| `SetModel {model,persist}` | `set_model` (+ `cycle_model`, `get_available_models`) | observed `get_state` returned the model incl. `contextWindow:128000`, `reasoningLevels`, `baseUrl` |
| `SetWorkspace {path}` | ⚠ **no RPC equivalent** (no `cwd` verb — verified against `rpc-commands.md`) | one pi process per workspace; our 5 workspace verbs become process lifecycle (decision 7) |
| `ClearSession` / `NewSession` / `SwitchSession` | `new_session` / `switch_session` (+ `fork`, `clone`, `get_tree`) | pi's session **tree/branching** is a free feature we never had |
| `Shutdown` | close stdin | |
| — (new) | `get_state`, `get_messages`, `get_entries`, `get_session_stats`, `compact`, `set_auto_compaction`, `set_auto_retry`, `abort_retry`, `set_session_name`, `get_commands`, `export_html`, `get_last_assistant_text`, `bash`, `abort_bash` | `get_session_stats` feeds the context bar; `compact` maps to a manual `/compact`; `bash`/`abort_bash` is **client-side shell, not model-callable** — decide deliberately whether the desktop exposes it |

`set_steering_mode` / `set_follow_up_mode` control how queued messages are injected.

---

## 4. Gateway verbs ⇄ bridge (the cutover surface)

The gateway (`crates/gateway/src/lib.rs`) speaks `{cmd,args,id}` → response, plus pushed
`{type:"event"}`, `resync`, `ready`, `auth*`. Its verbs, and who answers them after the swap:

| gateway verb | answered by |
|---|---|
| `bootstrap`, `session_state`, `list_sessions`, `session_messages` | bridge: pi `get_state`/`get_messages` + our session index |
| `new_session`, `switch_session`, `rename_session`, `delete_session`, `clear_session` | bridge: pi `new_session`/`switch_session`/`set_session_name` + session-dir file ops |
| `start_turn`, `cancel_turn` | bridge: `prompt` (with assertion), `abort` |
| `respond_approval`, `respond_question` | bridge: `extension_ui_response` (keep the id ⇄ pending map) |
| `set_mode` | bridge/extension: our approval mode |
| `set_reasoning`, `set_model`, `list_models` | bridge: `set_thinking_level`, `set_model`, `get_available_models` |
| `set_workspace`, `pick_workspace`, `add_workspace`, `remove_workspace`, `set_workspace_label` | **bridge process pool** (no pi verb) |
| `search_messages`, `search_settings` | our SQLite index (pi has no cross-session search) |
| `share_markdown`, `render_report`, `gpu_report`, `boot_log`, `open_file`, `search_probe`, `set_search_backend`, `migration_notes` | ours — product features, not core features (OPEN QUESTION 3) |

**Also still ours, unchanged:** watch vs drive tokens, token-not-in-URL, loopback default,
approvals failing closed on the engine timeout. Those are cwoffee's security posture and pi
does not touch them.

---

## 5. `usage` — the bar math changes (§ gotcha 1)

Observed against the mock (which reports llama.cpp-style
`prompt_tokens_details.cached_tokens = 92% of prompt` on tool-result steps):

| step | pi reported | real prompt |
|---|---|---|
| turn 1 | `{in:1147, cacheRead:0, cacheWrite:0, out:21}` | 1,147 |
| turn 2 (after tool result) | `{in:95, cacheRead:1081, cacheWrite:0, out:8}` | **1,176** |

So: **`total_prompt = input + cacheRead + cacheWrite`.** `usage.input` is *uncached only*.
A context bar bound to `usage.input` alone would under-report the second turn by ~10×, and
would read "cache miss" when the cache is doing 92% of the work. The cache-state line
(`prefix cache: ACTIVE (prompt 308 → cached 303)`) is still available — pi parses
`prompt_tokens_details.cached_tokens`, `prompt_cache_hit_tokens`, and top-level
`cached_tokens` (doc + observed).

---

## 6. Observed traces (abridged, real frames from `spike/drive-rpc.mjs`)

Streaming, no tools:

```
← {"type":"response","command":"prompt","success":true,"data":{"disposition":"started"}}
← {"type":"agent_start"}                      ← {"type":"turn_start"}
← message_start / message_end   (role:"system", sections.preamble…)
← message_start / message_end   (role:"user")
← message_start  (role:"assistant", provider:"cwoffee-mock", api:"openai-completions")
← message_update {assistantMessageEvent:{type:"text_delta",delta:"1\n2"}}   …×13
← message_update {assistantMessageEvent:{type:"text_end",content:"1\n2\n3…"}}
← message_end   → turn_end → agent_end → agent_settled {"aborted":false}
```

Tool call → approval → Rust tool → second step (our `read-file-approval` scenario):

```
← message_update {thinking_delta …} ×9                         (reasoning_content)
← toolcall_start {id:"mock-read-1", toolName:"read_file"}
← tool_execution_start {toolCallId, toolName:"read_file", args:{path:"/etc/hostname"}}
← {"type":"extension_ui_request","id":"…","method":"confirm",
   "title":"Allow read_file?","message":"/etc/hostname","timeout":15000}
→ {"type":"extension_ui_response","id":"…","confirmed":true}
← tool_execution_end {isError:false, durationMs:404,
   result:{content:[{type:"text",text:"{\"ok\":true,\"engine\":\"rust\"…}"}]}}
← turn_end {toolResults:[{role:"toolResult",toolCallId:"mock-read-1",…}]}
← usage {in:95, cacheRead:1081, out:8}   → turn_end → agent_end → agent_settled
```

The `engine:"rust"` in that result is the point of the whole document: the tool ran as a Rust
binary, its guardrails enforced in Rust, with pi only orchestrating.

## 7. Verified against pi 1.2.0 (fixture: `crates/bridge/tests/fixtures/pi-events-read-file-approval.jsonl`)

Everything below came from recording one real turn (`scripts/pi-event-capture.mjs`) against
cwoffee's own mock endpoint, not from the docs. Regenerate the fixture whenever the pin moves:
it is also the translator's test input, so a pi bump that changes the wire moves a failing
test rather than leaving a stale paragraph here.

**One prompt, two pi turns.** 47 records for one prompt that called one tool: `agent_start`,
then **two** `turn_start`/`turn_end` pairs (the tool round trip, then the answer after its
result), 5 `message_start`/`message_end` pairs (system, user, assistant, toolResult,
assistant), 25 `message_update`, one `extension_ui_request`, `agent_end`, `agent_settled`.
Hence the mapping rule in §2: cwoffee's turn spans `agent_start` → `agent_settled`, and pi's
inner boundaries emit nothing. Note the order: `agent_end` arrives **before**
`agent_settled`, so closing a turn on `agent_end` is off by whatever pi queues in between.

**Message shapes.** `message_start` carries the skeleton (`role`, empty `content`); blocks are
filled by `message_update.assistantMessageEvent` — `text_start/text_delta/text_end`,
`thinking_start/thinking_delta/thinking_end`, `toolCall_start/toolCall_delta/toolCall_end`,
with `contentIndex` as the block slot. `message_end` carries the **whole** assistant message
plus its `usage` trailer and `stopReason` (`toolUse` / `stop`). pi's `toolCall` blocks use
`name` + `args`; cwoffee's wire says `function.name` + `arguments` — the translator
translates, it does not forward.

`toolResult` messages are `{role:"toolResult", toolCallId, toolName, content:[{type:"text",…}],
isError, durationMs}`. Our Rust envelope (`{ok, tool, …}` as text) survives intact, so
`TurnFiles` can still come from a tool's own `changes`/`files` — with nothing synthesised
when a tool reports none.

**Dialogs.** `extension_ui_request` has only `id`, `method`, and per-method fields:
`confirm {title, message, timeout}`, `select {title, options[], timeout}`, `input {title,
placeholder, timeout}`. **No tool name, no risk, no escapes** — hence the `Allow <tool>?`
title convention for our extensions. It also names a real danger: an arbitrary extension can
ask a `confirm` about anything at all, and our UI would render it as an approval card. The
bridge marks such cards so they can be labelled differently before cutover.

**`get_state` fields that matter here:** `sessionFile`, `sessionId`, `model`,
`thinkingLevel`, `isStreaming`, `messageCount`, `pendingMessageCount`. `sessionFile` is what
makes resume — and therefore the idle reaper — lossless (PI-CORE-PLAN decision 14).

**Numbers worth keeping (this box, pi 1.2.0, mock endpoint, one extension):** ready
190–290 ms per workspace; respawn after an idle reap 191 ms; 4.1 s for a two-round-trip tool
turn (the mock simulates 800 ms of latency per round trip). Resume stores 4 rows → **6**, not
4+4+2, so replay does not duplicate the conversation. `usage` is per round trip, so a cwoffee
turn's totals are the sum over its pi turns — and `usage.input` **excludes** cached tokens:
prompt = `input + cacheRead + cacheWrite` (a measured turn: `prompt=1177 cached=617`).

**CLI traps.** `--tools` takes the *next token* (a denylist like `-read,-bash,-edit,-write`
only works that way) and **`--tools=x` is not accepted at all** — it falls into pi's
unknown-flag path. A guardrail flag that is silently ignored is worse than one that errors,
so the bridge's argv builder keeps the space form in code with a comment saying why.
