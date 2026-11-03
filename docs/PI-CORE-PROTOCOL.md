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
| `QueueChanged {session, items}` / `SteerLanded {…}` / `QueueReturned {…}` | `queue_update`, plus the echoed user record a delivered message arrives as | ⚠ **bridge-synthesised**: pi's queue is a list of strings with no ids, no kinds and no landing receipt, so the bridge keeps cwoffee's mirror and reconciles it against `queue_update` (survivors counted from the back, because pi removes the earliest match). `queue_update` used to be swallowed with the pings — which is how a mid-turn press on this core looked like a lost keystroke |
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
| `Send {text, attachments, session}` | `prompt {message}` | ⚠ the `session: Option<i64>` **assertion is ours** — and it is *addressing*, not a mismatch check: naming a conversation **moves the bridge to it** (the Rust core does exactly this, and its `resume` is what makes it work), while an id with no stored row is refused. A silent no-op is the failure mode to design against: two clients would otherwise edit each other's conversation. pi has no such notion at all. Attachments: pi handles images/files via message content + its own image limits — **not wired in S1.1**, which refuses them with a note. |
| `Cancel` | `abort` | |
| `queue_mid_turn` / `withdraw_queued` (mid-turn press, and taking it back) | `steer`, `follow_up`, `clear_queue` | Both cores have this now (`crates/engine/src/steer.rs`, [`docs/MID-TURN-STEERING.md`](MID-TURN-STEERING.md)). The Rust core keeps its queue in a shared handle polled at the agent's step boundary — its mailbox is parked inside the turn, so a command sent mid-turn would be read after it. On this core the press is a command, because the bridge's `select!` answers commands during a turn. `queue_update` paints the pending lines; pi's queue carries text and no ids, so cwoffee's mirror owns the ids withdrawal is addressed by, and `clear_queue` + re-queue is how a single take-back is done. |
| `SetMode {ask,auto,allow}` | `tool_call` handler + `ctx.ui.confirm` in our extension | our three-way mode lives in the bridge/extension, not in pi |
| `SetReasoning {off..xhigh}` | `set_thinking_level` (+ `cycle_*`, `get_available_thinking_levels`) | availability is model-dependent; ours was static |
| `SetModel {model,persist}` | `set_model` (+ `cycle_model`, `get_available_models`) | observed `get_state` returned the model incl. `contextWindow:128000`, `reasoningLevels`, `baseUrl` |
| `SetWorkspace {path}` | ⚠ **no RPC equivalent** (no `cwd` verb — verified against `rpc-commands.md`) | one pi process per workspace; our 5 workspace verbs become process lifecycle (decision 7) |
| `ClearSession` / `NewSession` / `SwitchSession` | `new_session` / `switch_session` (+ `fork`, `clone`, `get_tree`) | pi's session **tree/branching** is the mechanism `branch_conversation` runs on: `fork` before an entry, `clone` at the leaf. Deferred while a turn is running, like every verb that moves the thread |
| `Shutdown` | close stdin | |
| — (new) | `get_state`, `get_messages`, `get_entries`, `get_session_stats`, `compact`, `set_auto_compaction`, `set_auto_retry`, `abort_retry`, `set_session_name`, `get_commands`, `export_html`, `get_last_assistant_text`, `bash`, `abort_bash` | `get_session_stats` feeds the context bar; `compact` maps to a manual `/compact`; `bash`/`abort_bash` is **client-side shell, not model-callable** — decide deliberately whether the desktop exposes it |

`set_steering_mode` / `set_follow_up_mode` control how queued messages are injected, and cwoffee
settles both to `all` on every process start (`steer::PI_QUEUE_MODE`): the Rust core drains every
pending message of a kind at one boundary, and leaving pi at its `one-at-a-time` default would give
three presses during one tool batch two different answers on the two cores. The *kind* a press is —
`steer` or `follow_up` — is cwoffee's `mid_turn` setting, read from `config.toml` and published on
`HostState` so every window announces the same promise.

---

## 4. Gateway verbs ⇄ bridge (the cutover surface)

The gateway (`crates/gateway/src/lib.rs`) speaks `{cmd,args,id}` → response, plus pushed
`{type:"event"}`, `resync`, `ready`, `auth*`. Its verbs, and who answers them after the swap:

| gateway verb | answered by |
|---|---|
| `bootstrap`, `session_state`, `list_sessions`, `session_messages` | bridge: pi `get_state`/`get_messages` + our session index |
| `new_session`, `switch_session`, `rename_session`, `delete_session`, `clear_session` | bridge: pi `new_session`/`switch_session`/`set_session_name` + session-dir file ops |
| `start_turn`, `cancel_turn` | bridge: `prompt` (with assertion), `abort` — and `clear_queue` before the abort on Stop, so a stopped turn hands the waiting words back instead of sending them |
| `steer_turn`, `withdraw_steer`, `queue_state`, `set_mid_turn` | bridge: `steer` / `follow_up`, `clear_queue` + re-queue, cwoffee's mirror, and `mid_turn` in the config file. Drive verbs except `queue_state`, which a watch tab may read |
| `set_theme`, `theme_words` | **no pi verb** — pi drives a model, not a screen. `set_theme` writes `theme` in `config.toml`, which every surface reads at start, and `theme_words` answers About's line from the stored value plus whatever override the asking client is showing (an argument, never a write). Published on `HostState` as `theme` / `theme_note` / `theme_about`, in the engine's own words (`crate::theme`), so switching cores cannot change the colour underneath a window or paraphrase which home a choice lives in |
| `respond_approval`, `respond_question` | bridge: `extension_ui_response` (keep the id ⇄ pending map) |
| `set_mode` | bridge/extension: our approval mode |
| `set_reasoning`, `set_model`, `list_models` | bridge: `set_thinking_level`, `set_model`, `get_available_models` |
| `set_workspace`, `pick_workspace`, `add_workspace`, `remove_workspace`, `set_workspace_label` | **bridge process pool** (no pi verb) |
| `search_messages`, `search_settings` | our SQLite index (pi has no cross-session search) |
| `edit_message` | ours: the cwoffee store row, then a **rewind** — bridge rewrites pi's session file and reopens it (see below) |
| `branch_conversation` | ours: copy the rows, then a **fork** — bridge compares cwoffee's user rows against pi `get_fork_messages`, forks before that entry (or `clone`s at the leaf), records the new thread (see below) |
| `session_tree` | ours: the cwoffee rows and lineage, plus pi's thread read live with **`get_entries`** and mapped onto the rows (see below — and why not `get_tree`) |
| `switch_leaf` | ours: the record's leaf, and on the pi core a **fork to that prefix** so pi's thread moves with it; on the Rust core the prompt is re-seeded from the branch |
| `checkpoint_preview`, `checkpoint_revert` | ours on both cores. The copies are taken inside `cwtool`, which is the only thing that sees a file before the tool overwrites it; the marks ride back in the tool's JSON envelope, and the bridge writes the index rows (see below) |
| `files_ledger`, `file_diff`, `snapshot_stats`, `forget_copies` | ours on both cores, and core-blind: they read the cwoffee checkpoint index and the content-addressed store, which the pi core fills through the same `cwtool` capture. `forget_copies` asks the **live host** whether a turn is running, so on this core it consults `PiHost`'s state — a store-only check would forget copies while a tool call is taking them |
| `mcp_status` | ours on both cores, from the hub the app started. Both cores report `mounted` when servers are running; `route` says how a call reaches them — in process here, over the app's hub socket on the pi core (see below) |
| `tool_inventory` | rust core: its registry. pi core: an **error**, because pi decides the tool list inside the model process |
| `browser_status` | ours on both cores, and the same function either way: it reads the driver off disk and the model's modalities out of the config file. The pi core's `cwtool` reads that same file per call, so "will this run paint a window" has one answer on either core (see below) |
| `share_markdown`, `render_report`, `gpu_report`, `boot_log`, `open_file`, `search_probe`, `set_search_backend`, `migration_notes` | ours — product features, not core features (OPEN QUESTION 3) |

**Also still ours, unchanged:** watch vs drive tokens, token-not-in-URL, loopback default,
approvals failing closed on the engine timeout. Those are cwoffee's security posture and pi
does not touch them.

### `edit_message`: the record and pi's memory are two ledgers

Correcting a message the person already sent has to change **both** places the conversation lives.
The cwoffee SQLite rows are the transcript every screen reads. pi's JSONL session file is what the
model actually answers from. Neither knows about the other, and a feature that edits one and not
the other produces a transcript whose next answer refers to messages nobody can see.

The order is fixed, and it is the whole feature (`crates/engine/src/edit.rs`): check → cut the
record → hand the host a rewind (pi: rewrite the JSONL as a prefix, then `switch_session` on the same
path so pi rereads it; the Rust core: rebuild its prompt from the trimmed history) → put the
corrected words through the ordinary send path. Re-sending before the rewind would answer from the
stretch that was just deleted.

pi has no "rewind the current branch" RPC, so the bridge prunes the file: keep the header and the
prompt scaffolding, keep entries up to the *n*th user entry, where *n* is the number of user rows
the cwoffee store still holds (`crates/bridge/src/rewind.rs`). Write to a temp name and rename, so
a crash cannot leave a half-written thread; then `switch_session`, then re-absorb state, then put the
conversation's thinking level back — the entry that recorded it may have been in the stretch that
just went, and pi applies thinking per session, not per conversation.

Two things that choice depends on, both worth knowing before this doc goes stale:

* **The cut walks the tree, not the file.** A pi session file is a tree: entries carry
  `id`/`parentId`, more than one entry can share a parent, and what the model was told is decided by
  `leafId` — context is built by walking from the leaf to the root. "Keep the first *n* user entries
  in line order" was the first implementation and it is wrong the moment anything branches a file:
  it keeps entries from a path the person is not on. The prune asks the child for `leafId`
  (`get_entries`), walks leaf→root, and cuts on that branch; when the child is down it takes the
  file's last entry and says so. `a_branch_is_cut_on_the_branch_being_kept_not_on_the_files_line_order`
  and `a_leaf_that_is_not_the_newest_entry_cuts_that_branch` pin both halves.
* **A cut keeps the exchange, not just the question.** The kept stretch ends with the answer to the
  last kept user message and that answer's tool results, because cwoffee's transcript ends there too.
  Cutting at the question would leave the model unable to remember an answer the person can still
  read.
* **A fold whose anchor is off the kept chain is refused, not repaired.** A compaction entry's
  `firstKeptEntryId` points at the message its summary starts from. If the cut drops that anchor,
  dropping the fold orphans the entries appended under it and repointing it keeps a summary of a path
  nobody is on, so the rewrite refuses and leaves the file alone.
* **Only pi's own user entries count.** Compaction and scaffolding entries are pi's, not the
  person's; counting them cuts the thread in the wrong place. The prune asserts on that.

### `branch_conversation`: pi owns a tree, so a branch is a fork

Branching is the case that made the tree above matter. cwoffee copies its rows; pi has to copy the
*memory*, and it does that with two RPCs (`crates/bridge/src/host.rs`, `branch_row`):

* **`{ "type": "fork", "entryId": "<id>" }`** — a new session file holding the path from `entryId`
  to the root, with pi as the parent. The bridge forks **before** the entry it was asked at for a
  person's message, so the branch does not contain the message being asked again; for an answer it
  forks before the *next* user entry, so the answer stays in the branch.
* **`{ "type": "clone" }`** — the current active branch copied at the current leaf. That is what
  "branch at the newest answer" is: a copy of the whole conversation.
* **`{ "type": "get_fork_messages" }`** — `{ messages: [{ entryId, text }] }`, the entries pi would
  accept as a fork point. The bridge compares that list against the cwoffee conversation's own user
  rows — count first, then whitespace-normalised words — and refuses if they disagree. A fork made at
  an entry the record does not describe is a second conversation with wrong history in it, and no
  later note can undo that.
* **`{ "type": "get_state" }`** — `sessionFile` names the thread the child is now on, which is what
  gets recorded in the thread map for the new conversation.

Two behaviours that are not obvious from the protocol, found by probing pi 1.2.0:

* **A forked file may not exist yet.** `fork` switches the child onto a new thread and writes the
  file when the first entry is appended to it. So a branch nobody has spoken into has a recorded path
  and no file, and the note says its file is written when the first message lands. Anything that
  hands `switch_session` a path pi cannot open fails the launch, so `enter_row` and the launch both
  filter the mapped path through `metadata()` and treat a missing file as a fresh thread — with a
  note that says *why* it is fresh, which is a different sentence for an imported conversation than
  for an unspoken branch.
* **A branch is adopted before it is forked.** If the child is holding another conversation, the
  bridge switches onto the source first: the fork point is an entry in *that* thread's file, and
  `get_fork_messages` answers about whatever is loaded. Order is adopt → fork → write the row → map
  the thread. If the row cannot be written the child goes back to the source thread and the note says
  an orphan thread file exists and is safe to delete, because a conversation the model is answering
  into that nothing lists is worse than a stray file.

The thinking effort the source conversation was at follows the branch: pi copied the entry that
recorded it, and the bridge copies its own per-row maps so a later reopen keeps it rather than
falling back to pi's default. `get_state` after the fork is re-absorbed, because what the forked
thread *runs at* is the answer, not what the source row asked for.

### `switch_leaf`: pi has no `set_leaf`, so the leaf moves by forking

The third verb that moves a thread stands the conversation's own leaf somewhere it already read — no
new conversation, no deleted rows. pi's RPC offers no command for that: `navigateTree` lives in an
extension context, which the RPC parent cannot reach, and asked mid-turn it answers *"Wait for the
current response to finish before navigating the session tree."* So the bridge does what branching
does, with the fork taken at the node the person pressed:

* a person's message → `fork` **before** that entry, so the branch ends at its parent and the words
  come back for the composer (`prefill`);
* an answer with a message after it → `fork` before that next user entry, so the answer is inside the
  branch;
* an answer at the leaf → `clone`, the whole active branch copied.

The guard is `get_fork_messages` again — count and words against the record's user rows — and the
order is the branch's order: adopt the source row, fork, write the record's leaf, re-map the thread.
If the write or `get_state` fails, the child goes back to the thread it was on, because a record
pointing at a path pi is not reading is the exact failure this verb exists to avoid.

**The old thread is not deleted.** pi keeps it, and it holds the messages the leaf walked past. The
receipt says so, since "nothing was deleted" is otherwise a claim about cwoffee's rows only.

### Reading pi's tree: `get_entries`, not `get_tree`

`session_tree` asks the live child for its thread and maps the entries onto cwoffee's rows. It uses
**`get_entries`** — `{ entries: [{ id, parentId, type, message | modelId | … }], leafId }`, a flat list
in file order — and rebuilds the tree on the Rust side, marking the branch by walking up from `leafId`.

`get_tree` is the tempting one and it is unusable: it returns a defensive copy of the tree as nested
`children` arrays, **one JSON level per entry**, so a sixty-message conversation exceeds the bridge
reader's recursion limit and the whole record is refused (`recursion limit exceeded at line 1 column
56463`). That read sits on the page's every-open path, so it also has to be the cheap one — a slow read
there queues behind the host's loop and other verbs time out waiting for it. Gotcha 168.

What the mapping says when the two records differ: an entry no row answers to is a node and a line of
drift; an entry on a branch nobody is reading is said as off-path; pi's own writing — `model_change`,
`thinking_level_change`, the system prompt it assembled for the request — is a node and **not** drift,
because the record holds messages, answers and tool results and never the prompt the engine built.

### Turn checkpoints: the copies come from `cwtool`, the index belongs to the bridge

A checkpoint is a copy of a file taken **before** a tool overwrote it, so that the transcript can
offer "undo this turn's files" later. Nothing about it is a model-facing thing: the bytes go to a
content-addressed store under `<workspace>/.cws/snapshots/`, and the index rows go to cwoffee's
SQLite, which is the only place that knows which turn made them.

On the pi core the file call happens in a **separate process**, so the copy and the index are written
by two different things and the seam has to be explicit:

* The bridge launches `cwtool` with `CWOFFEE_CHECKPOINTS=1` alongside `CWOFFEE_DB`, and the tool host
  turns that into `ToolCtx::checkpoint_store`. Both flags come from the same decision: a copy with no
  index row pointing at it is a private file in the workspace nobody can find or undo, so a tool host
  that cannot write the row should take no copy at all.
* `cwtool` returns the marks in the same JSON envelope that already carries `changes` and `files`.
  The bridge's translator parses them into `Output::checkpoints_by_call`, and `EngineEvent::ToolExecEnd`
  carries them so the live tool card and a reloaded transcript agree about what the call copied.
* The bridge writes the rows, because a row needs the **turn number**, and pi does not hand cwoffee
  one: the translator numbers turns (`Output::turn()`), and `apply()` records each call's marks against
  it. Recording is per call, not at the settle: a turn that was stopped partway still changed files,
  and those are the changes a person most wants undone.
* At the settle the rows are stamped with the answer row's id, so the transcript can put the control on
  that row instead of guessing at one — the same reason file chips are stamped, and the same reason it
  can only happen once the row exists.

The snapshot directory is derived from the workspace at the time the tool runs (`ToolCtx::snapshot_dir`),
never from a path stored earlier: a conversation whose folder moved would otherwise be told to restore
into a folder it no longer works in. Paths in the index are workspace-relative and `resolve()` refuses
any that escape the folder — a copy of a file outside the workspace could never be reverted by this
code, so recording it would promise an undo that cannot happen.

### MCP servers: the hub cannot live in the tool host, so the app owns it and hands out a road

Third-party MCP tools ride lazy exposure on both cores: registered as *hidden*, absent from
`Registry::defs()` so they cost no prompt bytes, searchable by `explore` with `need: "mcp: …"`,
declared on the next request of the same turn once `explore` names them, and approved per call.

What differs between the cores is only how a call reaches a server. An MCP server is a long-lived
child that answers JSON-RPC over its stdio for the life of the conversation, and **`cwtool` is one
process per tool call**: a server spawned inside it would be born and die around a single call,
re-handshaking every time with its session state, file handles and subprocesses gone between calls.
So on the pi core the **app** starts and owns the hub exactly as it does on the Rust core, and the
tool process asks it over a unix socket in the app's own state directory
(`PiHostConfig::mcp_sock`, passed to every child as `CWOFFEE_MCP_SOCK`).

| | rust core | pi core |
|---|---|---|
| the hub | the app's process | the app's process |
| a call | `Route::Hub` — `Hub::call` in process | `Route::Socket` — one line each way to the hub |
| the tools | `mcp::install_into` | `mcp::install_socket_into`, from the hub's catalog |
| the catalog's home | the `<env>` block on the newest user message | the extension re-reads the block from the hub before every request and appends it to that message |

`mcp::socket` answers `catalog`, `block`, `status` and `call`, and refuses anything else by name; it
cannot start a turn, change a model, read a conversation, or approve. The approval gate stays where
it already was on this core — the extension asks `cwtool check`, the card is the engine's own
summary, and the permits travel as `--permit` for that call only — so `per_call` semantics are
identical and a session grant covers a server on neither core.

That sentence was **not true** until the first full pi-core pass ran. Two drifts, both caught by the
`mcp` group on pi and green on the Rust core:

* the bridge wrapped `cwtool check`'s sentence as `"<tool>: <summary>"` for the card body, while the
  Rust core prints the summary alone. For a third-party call that buried the server behind a second
  copy of the tool name — the headline already carries it — and the check asserting the card *leads*
  with `MCP server fake:` went red. `approval_summary` stopped adding the prefix.
* `permit_covers` asked only whether the call escaped the workspace, so one "allow for this
  conversation" on `mcp__fake__write_note` silenced the next call. The engine's rule is
  `CallRisk::per_call`; the bridge now refuses any name `mcp::server_of` recognises — the engine's
  own naming parser, not a copied `starts_with`, because a second naming rule is a second rule that
  can drift. `McpTool::run` is one function whose
two arms are decided after risk, escapes, `per_call`, the truncation limit and the failure wording
have already been computed, which is what makes "one behaviour, two roads" a fact rather than a
claim.

Two things this cannot be:

* **A second runtime.** The serving task runs on the app's runtime. The servers are tokio children
  with tokio pipes; a hub driven from another runtime leaves them on a reactor nobody polls, and a
  live server would answer nothing and read as a dead one at the timeout.
* **A wider door.** If the socket could not be opened, the app says so at boot, hands out no path,
  and a call answers "the app's MCP hub is not reachable at …; nothing was called" instead of
  guessing. `mcp_status` carries that as `route`, and Settings prints it — a row reading *mounted*
  while no call can get there is the claim this prevents.

`tool_inventory` still answers with an **error** on this core, not `{declared: [], hidden: []}`: pi
decides its declared list inside the model process, and two empty lists would claim nothing is
hidden about something this side cannot observe. What it *can* state is the hub's catalog, which is
what is withheld, and the settings row uses that number on this core rather than a registry list it
cannot see.

### The browser: two processes, one policy about a person's desktop

`browse` drives a real browser for one walk — open, act, report, close — and one of its decisions is
whether a window is painted on the user's screen. That makes the *source* of the answer a protocol
question, because on this core the tool runs in a process the bridge starts per call.

There is exactly one function that decides (`browser::decide`), and exactly one place the inputs come
from: `[browser]` for the person's override and the driver, and `[models.<id>] supports_vision` for
what the model can see. The Rust core reads them in the host, per turn, against the model that is
answering. `cwtool` reads the same file for the call it is making; the bridge hands it
`CWOFFEE_MODEL` — the model's **name**, not a verdict about its eyes — precisely so the verdict stays
computed in one place. A bridge-side or TypeScript-side copy of the rule would drift the first time a
model changed mid-session, and the drift would be visible as a browser window appearing for a model
that cannot see it, or pixels withheld from one that can.

The same holds for the settings answer: `browser_status` on both cores calls the engine's own
`status_line`, and the tool's transcript output prints the same reason string. One policy, asked
three times, must answer three times the same way — the selftest's `browse` group grades the
transcript claim against the settings claim for that reason.

Registration is also shared: the tool is mounted only where a driver exists, and the pi core's
`cwtool` builds its registry with the same `Config::load` the desktop uses, so a machine without
Playwright does not pay ~1.1 KB of prompt on either core. The MCP-style `hidden` path is not used
here: unlike a third-party server, this tool's schema is a fixed ~1.1 KB and its risk is ours to
describe, so it is mounted or not mounted rather than withheld.

### Chat mode's Web grant: pi decides tools at exec, so a switch is a process lifecycle

A chat conversation starts with **no tools at all**. The composer's `Web` chip grants exactly two —
`web_search` and `web_fetch` — and nothing else, ever: a chat that can reach the web still cannot read
a file or run a command. Three things have to change together for the chip to be true, and pi will
not let the bridge change any of them on a live process:

| thing | what is set | why this half alone is a lie |
|---|---|---|
| what pi **declares** | `--tools web_search,web_fetch` | `--tools` with **plain names replaces** pi's whole selection; a denylist (`-read,-bash,…`) would leave pi's other tools live behind a prompt that says two |
| what the cwoffee extension **registers** | `CWOFFEE_TOOL_ALLOW=web_search,web_fetch` | registration is what makes a tool callable. Narrow the declaration and not the registration and the model is told about two tools while ten are reachable |
| what the model is **told** | `Persona::CHAT_WEB_PROMPT` instead of `CHAT_PROMPT` | a tool nobody describes is a tool the model does not reach for; and a denial with an exception bolted onto the same sentence is the shape that stops a model searching, so the two prompts are separate texts (`Persona::chat_prompt(web)`) |

Because pi reads all three at exec, `set_web` (and entering a conversation whose stored grant
differs) **reaps the child**: the next turn starts a new process mounted that way. That is paid for
once per switch with a cold prefix cache, and the note the app shows says so rather than letting the
person wonder why the next answer was slow. `HostState.web_tools` carries the receipt — `no tools at
all`, `web_search, web_fetch`, or the coworker's set — so a check can hold the chip and the actual
argv against each other instead of trusting that the code which mounts a tool also told the prompt.

The same `no-tools-at-exec` fact is why a `--no-tools` launch now clears `launch.tools` rather than
keeping the cloned cowork denylist: a field that holds a list pi was never given is a field the next
reader will believe.

### `provider_set_primary`: repointing the endpoint is also a re-launch

The app has no default endpoint any more, so first run goes through a setup flow that writes one. On
the pi core there is no RPC that re-points a running pi at a different server: the bridge writes the
top-level `[provider]` keys, re-mounts the provider rows pi reads from its endpoint registry, and
reaps the child so the next turn starts against the new address. `HostCmd::ReloadProvider` is deferred
while a turn is running — finishing a sentence begun somewhere else is not an answer — and the
transport layer refuses the verb outright while the host is busy. `CWOFFEE_URL` still layers over the
file at boot, which is why every "is an endpoint configured?" question goes through
`config::load_effective`: asking the file alone can tell a scripted run it is unconfigured while it is
busy answering.

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

### A turn's files: `kind` and `created` are the shape, and both cores must carry them

The transcript draws a file row from two facts on `OutputFile`, and both arrive on the tool
envelope rather than being re-derived in the shell:

- `kind` — `artifact | edit | scratch | attachment | unknown`, from the engine's `kind_for`
  (placement first, extension second) or, for a download, from where the bytes landed.
- `created` — did *this turn* make the file. Set by the tool that looked at the disk before
  writing (`write_file`, `xlsx`, `download_file`); an `edit_file` never sets it, and a row that
  predates the field decodes to `false` instead of inventing a claim about an old turn.

`created` is a claim about the turn, not about the bytes, so it survives the merge that everything
else loses: both cores keep it sticky when a path is written twice in one turn
(`agent::note_turn_file` on the Rust core, `merge_turn_file` on the bridge) — otherwise "wrote a
report, then edited it" would show a card on one core and a chip on the other, from the same
conversation.

What the two facts pick: a created file is a **card** (dashed hairline, name, size), a changed file
is a **chip** in the working list, and only a file under `.cws/assistant_attachments/` gets the
**Save…** control. That control is the only part of the row that promises a copy out, so it is
gated on the folder and never on a label: a row that claims `attachment` while sitting elsewhere
gets a chip and one line naming the paths the transcript refused to offer. The reason this is
written down here is that the shape used to come from the label alone, and a `download_file` with
`place: "attach"` and a workspace-root `path` produced a hand-over card for a file that was never in
the folder — a claim no check enforced, on both cores, because neither one made the decision.

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

## The output cap, and why pi's compaction arithmetic cannot be fixed by tuning a number

pi's provider composer always sends an output cap: a model entry that says nothing is answered by
`definition.maxTokens ?? 16384` ✓ so silence is a silent 16384-token truncation, not freedom ✓ cwoffee
has to choose a number. The number it used to choose was half the context window ✓ which a server can
refuse: a llama.cpp slot checks `prompt + max_tokens` against its own size ✓ so a request with a 250k
prompt and a 131072 cap errors even while the endpoint has room ✓ "the engine complains about a
ceiling while there is 160k free."

The rule now: **the endpoint's own recorded limit wins.** `mount::declared_max_tokens` ✓ reading
`[models.<id>] max_tokens` ✓ graded by
`the_output_cap_prefers_the_recorded_limit_over_half_the_window` ✓ Half the window survives only as the
fallback when no limit is recorded ✓ and no probed window still means no claim ✓.

Provenance, stated plainly because it matters: **our probe does not learn an output limit.** llama.cpp's
`/props` gives `n_ctx` ✓ the context ✓ which `provider/openai.rs:1016,1030` reads ✓ and nothing in the
codebase reads `n_predict` or `default_generation_settings` for a generation cap ✓ so a `max_tokens` in a
model row is either something a person wrote or a value an older build left behind ✓ Not an endpoint
fact ✓ What the change buys is that a *recorded* limit is honoured instead of being shadowed by a guess ✓
and if no limit is recorded ✓ the guess stays half the window ✓ If you want a real endpoint fact here ✓ the
place to read it is `/props`' `default_generation_settings.n_predict` ✓ which nothing does today ✓.

Compaction is a different problem and it is not solvable by shrinking `reserveTokens` ✓ because pi's
summarization caps are fractions of that reserve:

```js
// pi dist/core/compaction/compaction.js:512 — history summary
Math.min(Math.floor(0.8 * reserveTokens), model.maxTokens > 0 ? model.maxTokens : Infinity)
// pi dist/core/compaction/compaction.js:727 — turn-prefix summary for a split user-message span
Math.min(Math.floor(0.5 * reserveTokens), …)   // pi's comment: "Smaller budget for turn prefix"
```

A split span merges two summaries ✓ up to `1.3 × reserveTokens` of output ✓ and compaction fires when
`contextTokens > contextWindow - reserveTokens` ✓ so at that moment the prompt is already about
`window - reserve` ✓ giving `(window - reserve) + 1.3·reserve ≤ window` ⟺ `0.3·reserve ≤ 0` ✓
unsatisfiable for any positive reserve ✓ No setting makes pi's own compaction fit ✓ which is the
argument for moving the computation into the core ✓ two ways out ✓ both needing pi-side machinery that
is not built yet:

1. **cwoffee supplies the summary.** `session_before_compact` ✓ hands over `messagesToSummarize`,
   `turnPrefixMessages`, `previousSummary`, `tokensBefore`, `firstKeptEntryId` and `settings` ✓ and an
   extension can return `{ compaction: { summary, firstKeptEntryId, tokensBefore, usage } }` ✓ computed
   through cwoffee's own provider with a limit derived from the *actual* serialized prompt ✓ so pi's
   `0.8/0.5` arithmetic never runs ✓ pi keeps the trigger and its `firstKeptEntryId` bookkeeping ✓ so the
   record and pi's file still agree ✓ This needs the extension to ask the app for exactly one model
   call — and the hub socket answers only `ping, catalog, block, status, call` ✓ so a `summarize` verb
   there is a deliberate widening of that contract ✓ written as such ✓ not smuggled in ✓.
2. **pi does no compaction** (`compaction.enabled: false` ✓ `mount_settings` owns that file ✓) and
   cwoffee folds in its own core ✓ which already rebuilds prompt history wholesale for leaf moves and
   rewinds ✓ Cost: pi's overflow and length recovery stop firing — and length recovery is gated on
   `messageModel.maxTokens` (`dist/core/agent-session.js:2407`, `isRecoverableLength`) ✓ so it is
   already a recovery from a cap cwoffee invented ✓.

"Send no cap at all" is reachable only through a provider whose field name is absent: pi's OpenAI
composer takes `maxTokensField` ✓ and its llama extension sets it explicitly
(`dist/extensions/llama/provider.js:116` ✓ `maxTokensField: "max_tokens"` ✓) ✓ so a cwoffee-registered
provider with `maxTokensField` unset would omit the field ✓ but that means registering a provider from
a pi extension rather than writing `models.json` ✓ which also moves the API key ✓ headers ✓ and
thinking dialect ✓ — a bigger change than the cap bug warrants ✓.

## The kept browser: `cwoffee-browser.sock`, and the `session: true` rule

The pi core has the same wall for browsers that it has for MCP servers: `cwtool` is one process per
tool call, so anything it spawns is gone before the next call, and a browser spawned there is a
different browser every time — a different profile, different cookies, and no page the person ever
saw.

So the **app** holds one browser and the tool process reaches it, exactly as the MCP hub does:

| | the Rust core | the pi core |
|---|---|---|
| who owns the browser | the app (`browser_hub::Hub` in `AppState`) | the app (same type, same process) |
| how the tool reaches it | `BrowserRoute::in_process` | `BrowserRoute::over_socket` over `$CWOFFEE_BROWSER_SESSION_SOCK` |
| who starts the driver | the hub, on the first `open` | the hub, on the first `open` |
| who reaps it when idle | a reaper task in the app | the same task in the same app |

The socket is `agent_dir/cwoffee-browser.sock`, mode `0600`, served by
`browser_hub::socket::serve`; the bridge passes its path to every child as
`CWOFFEE_BROWSER_SESSION_SOCK` (`PiHostConfig::browser_sock`). The verbs on that socket are the
driver's verbs, unchanged — including the page-pixel ones the window's column sends (`click_xy`,
`type_keys`) — because a tool process and a person's column are two callers of one browser, not two
browsers; a translation layer there would be the second webview wearing different clothes. Like the MCP socket it can open a
page, act in it, ask for state and a frame, and close it — and nothing else: it cannot start a turn,
choose a model, read a conversation or approve anything. If it fails to open, `session: true` says
there is no kept browser to reach, and the panel says the same in its own sentence.

Two protocol facts worth stating, because both cores have to agree on them:

- **`peek()` does not await.** A risk check (`ToolCtx::risk_for`) has to answer "is this the browser
  somebody is looking at?" synchronously, so the hub mirrors its state into an `Arc<RwLock<Snapshot>>`
  and `cwtool` calls `refresh_blocking()` **once at tool startup**, never inside `peek()`. A risk
  check that takes a lock it might have to wait for is a risk check that either hangs or answers from
  a copy it did not check — the failure this app has already been bitten by once.
- **`session: true` is a per-call decision, on both cores.** `crates/engine/src/approval.rs` refuses
  to let a conversation grant cover any call marked `per_call`, and the browse tool marks every
  session call that way. The bridge does not re-implement that: the card wording comes from the
  engine's `approval_summary`, which says which browser is being used and that it stays open in the
  person's sidebar. A drift here (the bridge approving a session call because `browse` was approved
  once this session) would be invisible in a transcript and visible as a click inside a page the
  person was using.
- **A door is handed out, not stood in.** `browser_hub::socket::serve` binds the socket, spawns the
  accept loop and *returns*, because both shells call it while putting the app together. The first
  version ran its `loop { accept().await }` inline, and the consequence was specific to this core:
  the pi start printed `kept browser answers on …/cwoffee-browser.sock` and then never printed
  anything else — the app was mounted, mounted its browser, and never came up, so the board's "which
  core am I grading" probe timed out and the run could not say what it had failed. `mcp::socket::serve`
  already had the right shape for the same reason; a second socket helper is not a second chance to
  get that wrong.

One comparison both shells call rather than write twice:
`browser_hub::needs_frame(since, newest_state, newest_frame)`. `since` is the sequence of the
*picture* a viewer is drawing; `snap.seq` counts *page state*. A reload bumps the state without
producing a picture, so a shell that compares the caller's picture against the state answers "you
already hold the newest" to a panel still showing the page from before the reload. The desktop
command and the gateway arm both ask the engine, and the unit test is
`a_page_that_changed_without_a_picture_of_its_own_still_owes_the_viewer_one`.

`browser_panel` is read-only across the gateway on purpose (`writable: false`, and the payload's
`route` sentence says the browser belongs to the machine that runs the tools). A phone tab that
shows a Back button the transport will refuse is worse than one that says which machine it is
looking at.
