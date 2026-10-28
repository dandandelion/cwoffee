# Task: ship three harness features — Branch, turn checkpoints with revert, MCP via lazy exposure

Repo: pi-cwoffee (Cwoffee). Both cores (pi core in `crates/bridge`, Rust core in `crates/engine`) must behave identically wherever behaviour is supposed to be identical, and both are tested against the same check. Work one feature at a time, in the order below; each ends green with `./scripts/selftest.sh` plus targeted cargo tests, before the next starts.

Ground rules that are non-negotiable (they live in `prompt.rs`, `tools/mod.rs`, `docs/TESTING.md`, README):
- Nothing volatile in the system prompt; history is append-only; compaction stays explicit and logged.
- No automated test may touch `http://127.0.0.1:8090` — suites run against `scripts/mock-llm.mjs` with scenarios in `tests/scenarios/scenarios.json`.
- Tool output is truncated by the tool, never the caller; UI-only metadata (files, changes) never reaches the model.
- Honesty over capability: a button says what exists. Update tooltips and status text when "not built yet" stops being true — and never claim a capability a check doesn't enforce.
- Cargo runs go through `./scripts/cargo-guard.sh` on this machine; end background processes by their printed pid, never by pattern-kill.

## 1. Branch

The button already exists, disabled and honest: `ui/src/lib/transcript.ts` ~line 3567 (`btn('branch', 'Branch', 'Branch this conversation - not built yet', …)`, `branch.disabled = true`). `crates/bridge/src/rewind.rs` documents the seam: pi exposes `fork`/`clone` RPC (see `docs/PI-CORE-PROTOCOL.md`), and rewind's linear-prefix assumption ("the file order *is* the conversation order, last line is the leaf") was written **only** because branching didn't exist. Branching breaks that assumption, so it must be fixed in the same change:

- Rewind becomes tree-aware: walk from the pi session's `leafId`, handle `firstKeptEntryId` (compaction entries), keep the header line. Pin the old assumption's removal with a fixture test — extend `crates/bridge/tests/pi_fixtures.rs` with a branched session file where line order ≠ conversation order.
- Pi core: Branch → `fork` (branch point) / `clone` (whole session); the new cwoffee session row gets its own id, title ("<original> (branch)"), workspace, and its own pi session file. Adopt the forked session like `switch_session` does today.
- Rust core: fork = SQLite copy of the session row + message rows into a new session id; new `PromptBuilder` seeded from the copy. State plainly in the UI that the branch starts cold-cache (compaction already surfaces its cache cost the same way — same principle).
- UX: branch from any user message (the row menu), not only the last turn. After branching, the sidebar shows both rows, the original continues untouched, and the branch is selected. A manual title the user typed is never overwritten (the titling rule already guarantees this — make sure the branch's auto-title passes through the same path).
- Selftest: enable the existing `turn-buttons` branch assertions (`branch.disabled` must now be false); add a scenario that branches mid-conversation, sends a turn on the branch, and asserts the original transcript and the model's context are unchanged. Run the same scenario on both cores against the mock endpoint.

## 2. Turn-level file checkpoints + revert

`crates/engine/src/tools/changes.rs` already computes per-call and per-turn `+N -M` and diffs; what is missing is restoring. Build the undo half:

- Before a call that mutates a file that already exists (i.e. `Risk::Workspace` or a write/edit/docs/xlsx/sheet op on an existing path), snapshot the pre-call content. Content-addressed store under `.cws/snapshots/` keyed by hash (dedupe: second touch of unchanged content reuses the blob), plus an index row in the store: session id, turn id, path, hash, deleted-vs-modified flag. Snapshots of files *created* by the turn are recorded as "remove to revert".
- Revert granularity: a "Revert files from this turn" action on the turn card, and "Revert files to before turn N" from the row menu. Revert restores files on disk only; it never touches the transcript — the transcript's own undo is `edit_message`/rewind, and the two must not be conflated. Say what was restored, what was deleted, and what could not be restored (file locked, path gone) instead of partially reverting silently.
- Revert itself is `Sensitive` — it overwrites bytes the user may have edited by hand after the turn. Approval card lists the exact paths and the direction (turn-N content → disk). If a file's current bytes differ from the snapshot-time post-call bytes, the approval card says so ("edited since") and offers overwrite or skip.
- Retention: snapshots are pruned with the session (delete_session drops them) and capped per session; the cap eviction is logged in the store so the UI can say "older snapshots pruned".
- Both cores: snapshots belong in the engine/host layer both cores share, so pi-core turns get checkpoints too via `HostOps`. If the pi core cannot produce file-change events equivalent to `changes.rs`, reuse the existing change-stamping path (`a file is stamped on the row that made it`) rather than inventing a second mechanism.
- Selftest: a mock scenario that writes a file on turn 1, edits it on turn 2, reverts turn 2, and asserts disk content and the tool card; plus a revert-after-manual-edit case that must surface "edited since". Cargo tests for snapshot dedupe, prune, and revert-of-created-file.

## 3. MCP via lazy exposure

MCP stays "not built" until it costs the prompt almost nothing: third-party tools must ride the existing `explore`/`discover` seam (`crates/engine/src/tools/discover.rs`, `ToolResult.newTools`) — declared inactive until discovered, hydrated on lookup, "available from your next call" wording kept literally true (`setActiveToolsByName` semantics; PI-CORE-PLAN.md notes "changes take effect on the next agent turn" — the UI must keep saying exactly that).

- Config: `[mcp]` section in `config.rs` (strict keys, retired-key comment-out rules apply); servers as `command`+args (stdio) with optional per-server `enabled`. `setup.sh`/`SETUP.md` get the one paragraph needed to run a first server.
- Startup: spawn servers, list tools, keep only name + one-line description in a registry catalog (a few hundred bytes, appended to the env block, not the system prompt). Full schemas hydrate only after the model asks (`explore` with an `mcp:`-prefixed need, or a new `discover` mode) — reuse `newTools`, same registry-widening path.
- Safety: an MCP tool never defaults to `Risk::Harmless`. No risk declaration from the server ⇒ `Sensitive`. A server cannot grant itself path escapes: `CallRisk.escapes` still requires per-call approval for anything outside the workspace, and session-mode memory of "yes to exec" must not extend to MCP tool calls.
- Failure honesty: a dead server says so in `discover` output and on the tool card; a tool call into a dead server is a repairable error naming the server, not a hang. Timeouts come from the existing `approval_timeout_secs`/exec-timeout vocabulary, not a new magic number.
- Rust core first; the pi core already has its own tool-injection path (`setActiveToolsByName` on the model process) — expose MCP tools through it so the A/B rule holds. If pi's RPC cannot host external tools, say so in README's "Not built" table rather than half-shipping it.
- Tests: a scripted fake MCP server (stdio, tiny JSON-RPC) in `scripts/`, exercised by a scenario: `explore "mcp"` → tool appears next turn → call it → card shows the server name. Assert the prompt bytes delta with the server enabled-but-unused is bounded (a selftest gate: catalog ≤ a stated byte budget), since prefill cost is the whole reason this is lazy.

## Definition of done, per feature

- `./scripts/selftest.sh` green, cargo suites green through cargo-guard, `svelte-check` still 0 errors 0 warnings, README's feature table and the "Not built" list updated to the truth, a `docs/` note in the house style for anything non-obvious that broke or had an assumption removed (especially rewind's linear-prefix assumption). Current counts (engine 505, bridge 30, gateway 38+4) only go up.
