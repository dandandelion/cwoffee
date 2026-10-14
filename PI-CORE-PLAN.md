# pi-cwoffee — `pi` as the core, cwoffee as the coworker surface

> **Living plan + progress log.** Goal: replace cwoffee's own agent core (the part
> that is not yet battle-tested) with **`pi` running as an RPC subprocess**, while
> keeping everything that *is* the product: the Svelte/Tauri UI, the gateway, the
> sandbox, and the Rust coworker tools.
>
> **How to use this file:** read `STATUS` + `PROGRESS JOURNAL` first, then the
> `ROADMAP` checkboxes. Keep them updated every session. Newest journal entry on top.
>
> **Companion doc:** [`docs/PI-CORE-PROTOCOL.md`](docs/PI-CORE-PROTOCOL.md) — frame-by-frame
> mapping, verified against the installed pi 1.2.0. **Proof, not intention, lives in
> [`spike/`](spike/README.md)** — a working end-to-end spike you can run.

---

## STATUS (update every session)

- **Phase:** 1 — **S1.2 is DONE on both paths.** `cwoffee-gateway --core pi` passes
  `gateway-smoke.mjs` + `gateway-approval.mjs`, and the **desktop app opens with pi as its
  core** (`CWOFFEE_CORE=pi`), in-process, answering approval cards from the normal UI, with
  **zero `ui/` edits**. `master` still defaults to the Rust core; nothing is cut over yet.
- **Last updated:** 2026-10-15T03:00Z (assistant, session 5)
- **Verdict:** feasible, and cheaper than it looks — because the UI and the tools both
  survive untouched. Measured, not assumed. Three real costs: **+~90 MB PSS**, **two
  languages in the runtime**, and **pi gives you no sandbox** (that stays ours to own).
- **The one number that decides it:** desktop release core is 248.2 MB PSS today; a live
  pi core sits at **73–77 MB settled, ~90 MB typical peak, 110 MB worst observed** (one
  extension, one tool loop, three runs). Total lands ≈ **320–358 MB** against a **400 MB**
  ceiling — passes, but headroom drops from ~150 MB to ~45–80 MB. See `MEMORY BUDGET`.
- **Why we are not doing the other thing we first considered** (using pi as a *UI*
  library over the Rust engine): `@earendil-works/pi-tui` is standalone (only 2 runtime
  deps) and would work, but it does not solve the actual complaint. The complaint is the
  un-proven agent loop, and that is exactly what this plan deletes.

---

## GOAL (final shape)

The same app, with a different middle:

```
Svelte 5 + Tailwind desktop UI (13,066 LOC)  ─┐
phone / browser client over /ws              ├── Rust **pi-host bridge** ══ JSONL ══▶ pi --mode rpc ──▶ Rust coworker tools
(Ratatui TUI: deleted)                       ─┘        ▲ gateway re-points here         ▲ extensions/*.ts = thin shims
```

- User-visible behaviour is **unchanged**: same composer, same transcript reveal, same
  approval cards, same sidebar, same phone client. The webview must **never learn** the
  core changed — the bridge speaks the gateway's existing frames.
- What gets deleted from `crates/engine`: `prompt.rs` (append-only PromptBuilder),
  `provider/` (openai + anthropic adapters), `sse.rs`, `stream.rs`, `agent.rs` (the loop),
  `store.rs` sessions half of it, `legacy.rs` — roughly **15k of 28,952 LOC**: precisely
  the half that is not yet battle-tested and that is getting to us.
- What stays ours, and is the product: `tools/*` (exec-in-bwrap, files, docs, net,
  sheets/xlsx, sheet_sql, searxng, ask), the sandbox, `tauri-app`, `ui/`, `gateway`,
  the self-test fixture (`scripts/mock-llm.mjs` drove pi **unmodified** — see S0.1).
- "Coworker, not coder" is a switch, not a fork: launch pi with its built-in coding tools
  **off** and only our tools on (`--tools "-read,-bash,-edit,-write,+sheet_query,..."`).
- Free on arrival (all unfinished in `PLAN.md`, done in pi): MCP, skills, prompt
  templates, session tree/branching, compaction, HTML export, model discovery, and
  `/llama` (pi drives the llama.cpp router: GGUF discovery + on-demand load), which is
  what `CWOFFEE_URL` hand-rolls today.

## NON-GOALS

- **Not** a UI rewrite. `ui/` does not change (that is the whole point of the bridge).
- **Not** a TS rewrite of the tools. Guardrails stay in Rust; the extension is a pipe.
- **Not** keeping the Rust engine as a permanent second core. Two cores is a liability;
  the Rust engine is kept only as (a) fallback until Phase 4 and (b) the tool implementations.
- **Not** the ratatui TUI with a nicer skin. `crates/tui` (1,189 LOC) is deleted at cutover;
  it was a dev harness, and the bridge supersedes it.
- **Not** pi's coding-agent surface in the product (built-in read/bash/edit/write off).

---

## REPOS (all local)

| Path | What |
|---|---|
| `/home/dandandelion/AI/Harness/cwoffee` | the Rust product, upstream. `origin` of this clone. |
| **`/home/dandandelion/AI/Harness/pi-cwoffee`** | **this repo.** clone of cwoffee @ `ffa1a9a`, `master`. |
| `/home/dandandelion/AI/Harness/pi` | pi monorepo source (v1.2.0). Docs: `packages/coding-agent/docs/`. |
| `~/.local/share/pi-node/node-v22.23.2-linux-x64/bin/pi` | installed pi **1.2.0** — the core we spike against. |

pi package sizes (LOC, `src/*.ts`), for scale of what we stop maintaining:
`coding-agent 76,000 · ai 27,015 · durable 25,188 · tui 19,540 · chord 12,302 ·
codemode 3,037 · mcp 3,277 · agent 2,519 · env 2,346 · telemetry 935`.

Our side today: `engine 28,952 · ui 13,066 · gateway 2,658 · tauri-app 2,454 · tui 1,189`.

---

## ARCHITECTURE MAP (target)

```
┌ ui/ (Svelte, unchanged) ── tauri IPC ──┐        ┌ phone (browser) ── /ws ──┐
│                                        ▼        ▼                          │
│                            [crates/bridge  ~1k LOC, new]                  │
│   · spawns + supervises  `pi --mode rpc` per workspace                    │
│   · JSONL framing (LF, binary read), id correlation, deadlines            │
│   · pi events  ──▶ EngineEvent-shaped frames the UI/gateway already emit  │
│   · pi extension_ui_request ──▶ respond_approval / respond_question       │
│   · session-identity assertion (the invariant pi has no notion of)        │
│   · telemetry pi does not emit: TTFT, live tok/s, TurnFiles              │
└───────────────┬──────────────────────────────────────────┬───────────────┘
                │ stdin/stdout JSONL                       │ spawn
                ▼                                          ▼
        pi --mode rpc  (pinned 1.2.0)          extensions/*.ts (thin) ──▶ Rust tool binaries
        tools: -read -bash -edit -write         sheet_query, exec(bwrap), ask, docs, net, xlsx
        sessions: JSONL tree  ──▶ our SQLite FTS index (derived, rebuildable)
```

| Component | New/keep | Responsibility |
|---|---|---|
| `crates/bridge` (or `crates/pihost`) | **new, ~1k LOC** | process supervision + protocol translation. The only genuinely new code. |
| `extensions/*.ts` | new, ~30 lines/tool | arg schema + spawn the Rust tool + return the JSON envelope. **No logic.** |
| `crates/engine::tools` | keep (move) | every tool, guardrails enforced in Rust, unchanged semantics. |
| `crates/gateway` | keep, re-point | same WS frames; its RPC verbs get answered by the bridge instead of `SessionHost`. |
| `crates/tauri-app` | keep, re-point | host window; owns bridge lifetime. |
| `crates/engine::{prompt,provider,sse,stream,agent,legacy}` | **delete at Phase 5** | pi does this, better and more battle-tested. |
| `crates/engine::store` | keep half | sessions become a **derived FTS index over pi's JSONL session dir**. |
| `crates/tui` | **delete at cutover** | dev harness, superseded by the bridge + a debug client. |
| `scripts/{memory,sandbox-probe,gpu-check}.sh` | keep, extend | `memory.sh` must sum the whole process tree incl. the pi child. |

---

## LOCKED DECISIONS

1. **Core = `pi --mode rpc`, subprocess, JSONL over stdio.** Never the in-process SDK:
   we are Rust, and process isolation is worth the pipe. (`docs/rpc.md`: built for
   "other languages, isolated processes, custom clients".)
2. **Tools stay Rust.** A pi extension is a shim: validate args → `spawn` the Rust binary
   → hand back its JSON envelope. Proven in S0.2, including a path guardrail that lives in
   the Rust and not in a tool description.
3. **The bridge emulates the existing gateway frames.** `bootstrap`, `event`, `resync`,
   `start_turn`, `respond_approval`, `respond_question`, … Cutover is a re-point, not a
   UI change. This is what keeps 13k LOC of UI out of the migration.
4. **pi's built-in coding tools off** via `--tools "-read,-bash,-edit,-write,+<ours>"`
   (`docs/cli.md`). A coworker does not ship an `edit` tool it never asked for.
5. **Sandbox and approvals stay ours.** pi states it has no sandbox (`SECURITY.md:50`);
   our bwrap `exec` and our `ApprovalMode` are assets. Approval round trip =
   `extension_ui_request{confirm|select|input}` → client answers → and **pi auto-denies on
   `timeout`**, which is the same fail-closed shape as our engine today (proven S0.3).
6. **Pin pi to 1.2.0**; add `scripts/rpc-conformance.mjs` that drives a scripted turn and
   **fails loudly on unknown command/event names**. pi will change; drift must be loud,
   not silent.
7. **One pi process per workspace.** pi cannot change `cwd` mid-session (no such RPC verb —
   verified), so our 5 workspace verbs (`set_workspace`, `pick_workspace`, `add_workspace`,
   `remove_workspace`, `set_workspace_label`) map to **process lifecycle**, not RPC. Pool
   policy (cap + idle kill) is OPEN QUESTION 1.
8. **pi's JSONL session files are canonical.** Our SQLite becomes a derived FTS index over
   the session dir (pi has no cross-session search). Rebuildable, disposable.
9. **Telemetry the core used to emit moves to the bridge.** pi's `usage` has **no `ttft`
   field at all** (verified: grep over `packages/ai/src` finds none; our probe read `null`).
   Our bar shows TTFT "the moment the first delta lands" and a character-derived rate —
   the bridge computes both from `message_update` arrival times.
10. **Nothing is deleted until Phase 5.** The Rust engine stays buildable as the fallback.
11. **Workspace pool policy** (user decision, 2026-10-14): **no process cap**; idle pi
    processes are reaped on a timeout. A workspace/session switch **never disturbs an
    in-flight turn**: each workspace owns its own pi process, so "switching" is a routing
    decision in the bridge — which process's event stream this client is looking at — not
    an interrupt. Safe because every `EngineEvent` already carries its conversation
    (`TurnStart { session }` exists precisely so a second client cannot confuse "not mine"
    with "mine"), and the send address is what keeps a stale tab from writing into
    another conversation. **Corrected against the Rust core's real behaviour, not my
    paraphrase:** naming a conversation in `Send{session: Some(id)}` *moves the host to
    it* and runs the turn there; what is refused is a name that matches no stored row
    (`SessionHost::a_turn_moves_the_host_to_the_conversation_it_names` is the spec, and the
    bridge reproduces it). Concurrent turns are the user's choice and we do
    not pay for their restraint — but the UI must **show how many turns are running**, so
    the cost is visible rather than mysterious.
12. **The desktop joins in-process, not over a second socket** (user decision,
    2026-10-14). The bridge is a library in the app binary implementing the same
    `HostOps`/`StoreOps` traits the gateway already consumes; `gateway_ctl` mounts that
    same handle as a task, as it does today. A localhost-WS desktop would add a hop, a
    second fan-out, and a process to the PSS total, and it would break the invariant this
    repo's own gateway doc defends: one core, so a phone answers the approval the desktop
    is waiting on.
13. **Pin pi 1.2.0, and keep upstream fixes cheap** (user decision, 2026-10-14).
    Consume the **published** `@earendil-works/pi-coding-agent` at an exact version +
    lockfile hash — never a vendored copy. Everything that is *ours* (tools, extensions,
    bridge) lives in this repo, so a pi bump never touches it. The bridge checks
    `pi --version` at spawn and **refuses to run on a mismatch** — a silent protocol drift
    is worse than a refused start. Upgrade path: bump pin → `rpc-conformance.mjs` →
    `selftest.sh` on both cores → event-stream diff → ship. If a fix genuinely belongs in
    pi, it is a fork branch rebased onto an upstream tag consumed as a tarball alias, and
    it gets re-verified by the same gates.
14. **Idle reaper: 600 s, and reaping is lossless** (settled 2026-10-15, implemented in
    `crates/bridge`). A pi process is closed when it has been idle **10 minutes**, is
    **not** mid-turn, and has **no dialog outstanding** anywhere. The next turn respawns
    it against the same pi session file (`get_state` reports `sessionFile`; the bridge
    pairs it with the cwoffee row id in `cwoffee-pi-sessions.json`), so the model keeps
    everything it knew. Measured: respawn + ready = **190–290 ms**, and the resumed turn
    showed `prompt=1072 cached=506` — the earlier context came back and hit the prefix
    cache. Reaping is therefore a memory decision, not a user-visible interruption; the
    transcript says so in a `Note` either way.
15. **Quit while busy: refuse, then confirm, then a bounded kill** (settled 2026-10-15;
    **not yet implemented** — it belongs to the desktop quit path, S5). A close/quit
    with a turn running does not silently kill it: the app stays up and says *N turns are
    still running — stop them and quit, or keep working*. On confirm: `abort` every busy
    session, wait up to **2 s** for `agent_settled`, let the store flush (it persists per
    message, so a partial answer is already durable), then exit; if a session has not
    settled in 2 s, SIGKILL the pi children and exit, logging what was killed. The reason
    for the refusal rather than the silent kill is the same reason the approval gate fails
    closed: a UI that disappears must not leave an agent acting unattended, and a turn
    truncated without saying so is indistinguishable from one that finished.

---

## MEMORY BUDGET (measured 2026-10-14, pi 1.2.0, this box)

PSS from `/proc/<pid>/smaps_rollup`, same method as `scripts/memory.sh` (PSS, never RSS).

| What | PSS | Notes |
|---|---|---|
| bare `node` idle | 26.1 MB | the floor any JS runtime costs us |
| `pi --mode rpc --no-session`, no extension | **72.2 idle / 89.9 peak / 73.6 settled** | one streamed 15-line turn; 11 threads, VmHWM 114.6 MB |
| **pi + 1 extension + tool loop + approval** (3 runs, `spike/drive-rpc.mjs`) | **idle 72.5–74.6 · peak 89.6–92.6 · settled 73.3–76.9** | one outlier run peaked at **110.3** — budget off the max, not the mode |
| RPC usable (`get_state` answered) | **176–311 ms** after spawn | cold start is not a problem; spawn-per-workspace is affordable |
| `@earendil-works/pi-tui` standalone (not used here) | 40.6 | kept for reference |
| **our desktop app (release, today)** | **248.2 MB total** | engine+Tauri+GTK ≈ 88–107, WebKit 115–130 |
| **our ratatui TUI (today)** | **6.8–7.0 MB** | this number leaves the README at cutover |

PSS varies run to run by a few MB (shared pages), so gate on the **worst observed**, not the
average. The tool's own `durationMs` of ~405 ms is dominated by the 400 ms approval delay the
spike's fake UI inserts — the Rust shim itself is single-digit milliseconds.

**Proposed budget line, new core:** `pi core ≤ 110 MB` (worst observed peak), enforced by
`memory.sh` summing the **tree** (bridge + pi child + extension-spawned tool children).
248.2 + ~90 ⇒ **≈ 338 MB peak / ≈ 323 MB settled**, vs the **400 MB** ceiling: passes, but
headroom shrinks from ~150 MB to ~45–80 MB — and `PLAN.md` already flags long-session growth
as the thing that breaks memory first. So: **re-measure the 200-message/3-attachment scenario
before celebrating**, and expect to either trim UI or move the ceiling to 420 with eyes open.

---

## ROADMAP

### Phase 0 — spike: can pi core this product at all? ✅ **DONE 2026-10-14**

- [x] **S0.1** pi `--mode rpc` driven by **our own scripted endpoint**
      (`scripts/mock-llm.mjs`, `tests/scenarios/scenarios.json`, unmodified). Full
      lifecycle observed: `prompt → disposition:"started" → agent_start → turn_start →
      text_delta×N → text_end → message_end → turn_end → agent_end → agent_settled`.
- [x] **S0.2** our `read-file-approval` scenario dispatched `read_file{path}` to **our**
      extension, which spawned a **Rust binary** (built with `~/.cargo/bin/rustc` against
      cwoffee's cached `serde_json`): `{"ok":true,"engine":"rust"}`, `durationMs:405`.
      Guardrail (refuse non-`/etc`, refuse `..`) enforced in Rust.
- [x] **S0.3** approval round trip to the client:
      `extension_ui_request{method:"confirm",timeout:15000}` → `extension_ui_response
      {confirmed:true}` → tool runs → second model step. Timeout auto-denies = fail closed.
- [x] **S0.4** our prefix-cache telemetry survives: mock's 92% hit surfaced by pi as
      `usage {in:95, cacheRead:1081}`. pi reads `prompt_tokens_details.cached_tokens` **and**
      llama.cpp's `prompt_cache_hit_tokens`.
- [x] **S0.5** `thinking_delta` streaming works (`reasoning_content` → collapsible block),
      and memory measured (table above).
- [x] **S0.6** artifacts committed to this repo → [`spike/`](spike/README.md) (they were in
      `/tmp` and would have evaporated).

### Phase 1 — the bridge, and the gates that make it real

- [x] **S1.0 / Move 0 — the trait seam** (2026-10-14, 266 insertions / 28 deletions).
      `EngineHandle` now holds `Arc<dyn HostOps>` + `Arc<dyn StoreOps>` rather than
      `Arc<SessionHost>` / `Arc<Store>`; both traits are defined and implemented in
      `crates/engine`. Measured surface: **9 host verbs + 14 store verbs = 23**, not the
      "16" this plan first guessed (gotcha 13). Proved neutral rather than assumed: engine
      **427+10 tests** and gateway **31 tests** green, and `scripts/gateway-smoke.mjs`
      against a binary built from upstream HEAD versus one from this branch diffs **only
      the port in its header line** — after both were seeded identically by
      `scripts/gateway-seed.mjs` (session created, one turn, 2 transcript rows each).
- [x] **S1.1** `crates/bridge`: spawn/supervise `pi --mode rpc` per workspace; LF-binary
      JSONL framing (never a line reader that splits U+2028/9); id correlation; shutdown by
      closing stdin. **Actual size: ~2.8k lines, not the ~1k this plan guessed** — the
      extra went into addressing, approvals/questions, the idle reaper, resume, and
      telemetry synthesis, all of which turned out to be the part with the bugs in it.
      *Exit met:* `cwoffee-bridge-probe` streamed a real turn (`6 text`, `11 reasoning`
      deltas, one tool card, one approval card answered by the client, `usage` with
      `cached=491/1039`) and the store kept 4 rows.
- [ ] **S1.2** frame emulation: bridge emits the **existing** `event`/`resync`/`bootstrap`
      payloads; `ui/` untouched. *Exit:* desktop app runs a live turn with **zero UI edits**.
      **Met on both paths** — browser via the gateway, desktop in-process.
      `cwoffee-gateway --core pi` (new flags: `--core`, `--pi`, `--pi-ext`, `--pi-tools`,
      `--pi-agent-dir`, `--pi-idle-reap`, `--pi-version`, `--any-pi`) builds the same
      `EngineHandle` over `Arc<dyn HostOps>` with a `PiHost` in place of `SessionHost`, and
      `gateway-smoke.mjs` — auth refusal, bootstrap with sessions/models/folders/current
      session/transcript rows, workspace refusals with their explanations — prints
      **GATEWAY SMOKE OK** on a cold bridge and a warm one, with no edits to `ui/`.
      `scripts/gateway-approval.mjs` (new) drives one tool call through the gateway and
      answers the card as a client would; it passes **allow and deny on both cores**, and it
      is what found two real bridge bugs (gotchas 23–24).
      The desktop half: `AppState.host` became `Arc<dyn HostOps>` (three call sites had
      reached past the seam — `host.provider().list_models_info()` and two
      `settings::*` free functions — which is how a "neutral" shell quietly pinned itself
      to one core), `CWOFFEE_CORE=pi` swaps the engine at boot, and decision 15's
      quit-while-busy refusal is wired on `CloseRequested` with a `Note` so it needs no UI
      edit. **Proven against the user's own endpoint** (`llama.cpp`, model
      `qwen3.8-flash-next-iq3_s`, reasoning XHigh, sandbox on): one approval card naming
      `read_file` and `OUTSIDE the workspace: /etc/hostname`, allow and deny both green from
      a client, and a turn that ran *after* the 600 s idle reaper had closed pi — so
      reap-then-resume works in the shipped binary, not only in the probe.
- [x] **S1.2a** `bridge::mount`: extensions copied into the isolated agent dir with pi's own
      `node_modules` symlinked, and a dev-only `models.json`, shared by probe and gateway
      instead of written twice.
- [x] **S1.3** the send address, as the Rust core actually behaves it (see decision 11):
      a named conversation moves the bridge, an unknown id is refused, and the cwoffee
      row ↔ pi session file pairing makes switching and resume real. Proven by probe run B
      (`resume Some(...jsonl)` across a process restart, 6 rows, no duplicates). Not yet
      exercised through the gateway — that is S1.2.
- [x] **S1.4** approvals + questions: pi's `confirm` → `ApprovalRequested` (with the tool
      name from our `Allow <tool>?` title convention and the out-of-workspace paths
      listed verbatim), `select`/`input` → `QuestionRequested`, answers back as
      `extension_ui_response`. Four closure paths proven: allow, deny, silence (pi's own
      timeout denies), and a vanished viewer (dropped oneshot). In every non-allow case the
      Rust tool did **not** run. cwoffee's `ask_user` *tool* is still S2.2.
- [x] **S1.5** telemetry: TTFT and live tok/s from our own clock, `Usage` summed over pi's
      per-message `usage` with the cache terms added back, `context_tokens` from the real
      prompt size. pi reports no server timings and no speculative counters, so those
      fields stay 0 rather than being invented. One trap fixed: wall clock counted time
      parked on a human, so a 4 s approval made the turn read **2 tok/s**; it now reports
      the model's **119 tok/s**, and a test pins it.
- [ ] **S1.6** `scripts/rpc-conformance.mjs` — pinned-version protocol guard (fails on
      unknown names). Add to CI next to `selftest.sh`.
- [ ] **S1.7** `memory.sh` tree-summing + new `core ≤ 110 MB` line; report both cores.

### Phase 2 — coworker tools as shims

- [ ] **S2.1** `exec` (bwrap) + `files` + `sheet_query` shims; keep Rust semantics
      (`SQLITE_LIMIT_ATTACHED=0`, progress-handler wall clock, mime/count/byte guards).
- [ ] **S2.2** `ask_user` as a tool that parks the turn (our queue) — decide RPC dialogs vs
      custom card (OPEN QUESTION 6).
- [ ] **S2.3** `docs`, `net`, `xlsx/sheets`, `searxng` shims; `TurnFiles` from the tool's
      own file-diff report (pi does not produce our `TurnFiles` / `+N -M` badges).
- [ ] **S2.4** `--tools` allowlist wired into launch config; verify no built-in coding tool
      is reachable.
- [ ] **S2.5** extension packaging: real `package.json` + resolved deps (the spike used a
      `node_modules` symlink from the agent dir — do not ship that).

### Phase 3 — sessions, search, reports

- [ ] **S3.1** session dir per product dir; `list_sessions`/`switch_session`/`rename_session`/
      `delete_session` mapped to pi RPC + filesystem.
- [ ] **S3.2** SQLite FTS as a **derived** index over pi's JSONL (watcher + backfill);
      `search_messages` keeps working, index is disposable.
- [ ] **S3.3** sidebar titles from first message (pi `set_session_name`) — preserve the
      "lazy session row" behaviour.
- [ ] **S3.4** `share_markdown` / `render_report` decision (OPEN QUESTION 3) — pi's
      `export_html` vs our own renderer over our index.

### Phase 4 — packaging + cutover

- [ ] **S4.1** RPM and AppImage **bundle a pinned Node 22.19+ and pinned pi 1.2.0**
      (no user-side install). OPEN QUESTION 2 — this is also a size/attack-surface question.
- [ ] **S4.2** dual-core A/B: `CWOFFEE_CORE=rust|pi`, `selftest.sh` green on **both**,
      `gateway-smoke.mjs` green, `sandbox-probe.sh` green.
- [ ] **S4.3** switch default to `pi`; keep `rust` for one release.
- [ ] **S4.4** update README/PLAN claims honestly: memory story re-baselined, "7 MB TUI"
      line removed, "one process tree" becomes "two".

### Phase 5 — delete the old core

- [ ] **S5.1** delete `prompt/provider/sse/stream/agent/legacy` + `crates/tui` + the Rust
      session store half. Keep `tools/`, `approval.rs` glue, `telemetry.rs` shapes.
- [ ] **S5.2** rewrite `PLAN.md` §4–§5 (the loop is no longer ours) and `docs/TESTING.md`
      (what the scripted endpoint now exercises is *pi*, not our SSE parser).

---

## GATES (must stay green — these are why Phase 0 took an hour)

| Gate | Command | Still meaningful after the swap? |
|---|---|---|
| in-app streaming contract | `scripts/selftest.sh` | ✅ and it now covers **pi's** stream path — our mock endpoint drove pi unchanged |
| scripted endpoint | `scripts/mock-llm.mjs` | ✅ keep as the brain of CI; also drives the new conformance test |
| memory | `scripts/memory.sh` | ✅ but must sum the tree incl. the pi child |
| sandbox | `scripts/sandbox-probe.sh` | ✅ ours; pi has no sandbox to lose |
| remote/phone | `node scripts/gateway-smoke.mjs` | ✅ the frames the bridge must satisfy |
| gpu/render | `scripts/gpu-check.sh` | ✅ unchanged |
| **NEW** protocol drift | `node scripts/rpc-conformance.mjs` | the pin's teeth |

---

## GOTCHAS (measured — do not rediscover them)

1. **pi's `usage.input` excludes cache reads.** The mock reported 1,176 prompt tokens; pi
   reported `in:95, cacheRead:1081`. Total prompt = `input + cacheRead + cacheWrite`, or the
   status bar lies about context.
2. **No `ttft` anywhere in pi's usage.** Our live bar (TTFT + character-derived rate, marked
   `≈`) must be computed in the bridge. Nothing else produces it.
3. **No mid-session `cwd` change in RPC.** Verified: no `cwd` in `rpc-commands.md`. Workspace
   switching = process switching. Budget for a pool.
4. **`response` to `prompt` ≠ finished.** It means accepted (`disposition`). Wait for
   `agent_settled`, not `agent_end` — retries, compaction recovery and queued messages can
   follow. `busy` in `HostState` must key off `agent_settled`.
5. **Framing is LF-only.** Node's `readline` also splits on U+2028/U+2029, which are legal
   inside JSON strings. Read bytes, split on `\n`, tolerate a leading `\r`. Stdout is
   protocol only; diagnostics are on stderr — never parse stderr.
6. **RPC degrades parts of `ctx.ui`.** `custom()` returns `undefined`, `setTheme`/`setFooter`/
   `setHeader`/`addAutocompleteProvider` are no-ops, `getEditorText()` returns `""`
   (`docs/rpc-extension-ui.md`). Anything rich is ours to render in the webview. `ctx.mode`
   is `"rpc"`, `ctx.hasUI` is `true`; guard TUI-only features on `ctx.mode === "tui"`.
7. **Extension module resolution.** A `.ts` extension loads with no build step (Node 22 type
   stripping), but `import { Type } from "typebox"` resolved only because the spike symlinked
   `node_modules` from the agent dir. Ship a real package (S2.5).
8. **No session-identity notion in pi.** An untargeted prompt lands in whatever conversation
   the last switch left active. Our refusal is a product guarantee; it must live in the bridge.
9. **pi's built-in tools are unapproved-by-default** (they run with the process's OS
   permissions). Since we disable them (decision 4), approvals only concern our tools —
   simpler than the current engine, but only if the allowlist is right.
10. **`pi` sessions are JSONL trees** (`docs/session-format.md`), not SQLite: cross-session
    FTS is ours (S3.2).
11. **A JS gotcha of ours, for the record:** `String.prototype.split()` with no argument does
    **not** split on whitespace. It produced a fake "775 kB" PSS reading and a wrong-looking
    first measurement. Use `split(/\s+/)`.
12. **Our `TurnFiles`/`+N -M` diff badges are ours to invent.** pi reports tool results, not
    workspace diffs. The Rust tools must keep emitting their own change reports.
13. **Method-surface greps lie.** `grep -oE "h\.(host|store)\.[a-z_]+"` found 16 methods;
    the real surface was **23**, because Rust spreads call chains across lines
    (`h.store\n    .search(...)`). The compiler found the other seven. Measure a surface
    with `cargo check`; use grep only to orient.
14. **`cargo check -p cwoffee-app` is already broken at upstream HEAD in this
    environment**, before any of our edits: the cached `tauri` build-script output under
    `target/` records absolute paths from an older location of this repo
    (`/home/dandandelion/Projects/cwoffee/...`), so it reads plugin permissions from a
    directory that no longer exists. Confirmed by running the identical command in
    `../cwoffee`. The fix is a dependency-artifact refresh (`cargo clean -p tauri`, then
    rebuild) and it is their call, because it costs a long rebuild. Until then the desktop
    crate cannot be typechecked here — which is why the gateway carries
    `handle_builds_from_concrete_engine_types_and_dispatches`: the same `EngineHandle`
    construction `crates/tauri-app/src/main.rs:386` uses, compiled and dispatched.
15. **Two environment notes that cost time:** `cargo`/`rustc` are not on `PATH` here
    (use `~/.cargo/bin/cargo`), and a fresh clone has **no `ui/dist`** (gitignored), so
    `cwoffee-gateway` refuses to start — point `--dist` at a built frontend, with an
    absolute path.
16. **Share `../cwoffee/target` with `CARGO_TARGET_DIR`** instead of building the clone
    from scratch (its target is 37 GB). `cargo check` writes metadata only and cannot
    overwrite their binaries; `cargo build` *does* write `target/debug/cwoffee-gateway`, so
    copy each artifact out before building the other core, and `cmp` the two binaries before
    believing a parity result — an unforced `cargo build` said "Finished" and left a stale
    binary in place, which would have made a fake comparison.
17. **pi's "turn" is not cwoffee's turn, and translating 1:1 breaks the UI.** pi emits
    `turn_start`/`turn_end` per *model round trip*: the recorded fixture's one prompt has
    **two** of each (tool round trip, then the answer). cwoffee's turn is one prompt
    including its tool loop, and `TurnEnd` is where markdown is parsed once and where
    `state().turns` advances. Mapping pi's pairs 1:1 would re-render a half-finished answer
    after every tool call and count one prompt as four turns. The bridge opens a turn at
    `agent_start` and closes it at `agent_settled`; pi's inner boundaries emit nothing.
    Pinned by `one_prompt_is_one_turn_even_when_pi_runs_two_of_its_own`.
18. **`--tools` needs the space form; the `=` form silently does nothing.** pi 1.2.0's
    argument parser is hand-rolled: it takes the *next token* for `--tools` (which is how a
    denylist like `-read,-bash` works at all), and it does **not** accept `--tools=x` — that
    lands in its unknown-flag path. Our first bridge run died with `Unknown option:
    --tools`; had pi been more forgiving, the flag that keeps pi's own `read`/`bash`/
    `edit`/`write` off would simply have been ignored. A guardrail that switches itself off
    quietly is the failure mode to design against, so the argv builder says why in a comment.
19. **Wall-clock rates lie when a turn parks on a person.** The first live run reported
    **2 tok/s** for a turn where a human had four seconds to answer an approval card. The
    fix is to accumulate time spent parked on a dialog (including a card still open when
    the run ends, because that wait ended too) and subtract it from the generation window.
    Same turn: 119 tok/s. This matters most for the S4.2 dual-core A/B, where a
    human-answered turn on one core would otherwise look 50× slower than the same turn on
    the other.
20. **pi's `confirm` dialog carries no tool name.** `extension_ui_request` has `title`,
    `message`, `timeout` — that is all. cwoffee's approval card needs the tool (header, and
    `AllowSession`, which is per tool), so our extensions own the convention: title
    `Allow <tool>?`. Anything else prints "the tool" rather than guessing. Escapes are
    derived from the message text, which is what makes the card answerable: `read_file —
    /etc/hostname escapes=["/etc/hostname"]` is a question a person can actually answer.
21. **Resume replays only new messages (verified, not assumed).** The worry was that
    `--session <file>` would re-stream the old conversation and store it twice. It does not:
    after a resume the store went from 4 rows to 6, not 4+4+2. Worth re-checking when pi is
    bumped, because the duplicate-index path would be silent and expensive.
22. **One engine test flaked under three-package load and I could not name it.** In the
    first combined `cargo test -p engine -p gateway -p bridge` run, one `cwoffee-engine`
    lib test failed (426 passed, 1 failed) and my grep lost the name. Five engine-only runs
    and two further combined runs were clean. Unresolved: a named flake is fixable, an
    unnamed one can hide a real bug. Next time, capture the name before re-running
    (`sed -n '/^failures:$/,/^$/p'` on the raw output, not `grep test result`).
23. **One card, one owner — and the bridge was showing it twice.** The bridge emitted
    `ApprovalRequested` *and* handed the same request to the approvals channel, which the
    gateway fan-out renders with the shared `approval_event`. Under the probe (which reads
    the channel) that looked fine; under the gateway the UI got **two identical cards for one
    decision**, and answering one leaves the other staring at you. Questions had the same
    double-send in two places. Now the bridge sends to the channel only, and renders itself
    solely when nobody took the receiver — in which case it answers `cancelled` rather than
    leaving pi waiting on a client that does not exist.
24. **A success path must not look like a refusal.** The bridge bumped `refusal_seq` on a
    *successful* workspace move, and `set_workspace_checked` reads that counter as "a refusal
    landed" — so the folder really moved to `/etc` while the client was told "the move was
    refused", and the smoke's "the host is still in the folder it started in" caught it. Two
    fixes: success clears the last refusal and says nothing new, and the bridge now runs the
    engine's own `check_workspace` instead of a private weaker version, which is why a
    pi-core refusal reads *"/tmp: is the system temp folder, which is where sessions with no
    workspace live…"* in exactly the Rust core's words.
25. **A cross-core gate must not quote one core's dialect.** My first approval script
    asserted the pi envelope (`engine:"rust"`) and an `escapes` field: the first is pi-only,
    and the second never reaches the wire at all — `approval_event` deliberately drops
    `escapes` so every shell renders one shape, and the escape rides inside `summary`. Both
    assertions failed on the Rust core and taught nothing. Now they are about what a person
    reads and what the tool did: the card names the file and says OUTSIDE the workspace;
    allow ends in a tool-result event with no denial anywhere; deny ends with no success and
    an explicit denial the user can see. Parity note found on the way: the pi core emits no
    `toolExecOutput` for this tool (our extension returns its result in one piece) where the
    Rust core streams one.
26. **RPC verbs need a live child, and a cold bridge has none.** `get_available_models`
    through pi means a process; bootstrap asks on every connect, including from a phone that
    will never send a prompt, so paying ~75 MB to answer it is wrong. With no child the
    bridge answers the menu from its own configuration — one entry, the model this
    conversation can run. A smaller lie than an empty menu, and the kind of thing that reads
    obvious in hindsight and invisible beforehand.
27. **A moved repo poisons its target dir, in *both* profiles.** The `tauri` build script
    caches absolute `OUT_DIR` paths, so after cwoffee moved from `~/Projects/cwoffee` to
    `~/AI/Harness/cwoffee`, every build of `cwoffee-app` failed with
    *failed to read plugin permissions: … /home/dandandelion/Projects/cwoffee/target/…* —
    the blocker that had made the desktop crate untypeable here for five sessions. It is
    not a code problem and `cargo check` will keep reporting it as one. Fix (both profiles,
    because each caches separately):
    `rm -rf target/{debug,release}/build/{tauri-*,cwoffee-app-*} target/{debug,release}/.fingerprint/{tauri*,cwoffee-app-*}`.
    Say so out loud in the log of any session that hits it, or the next one loses hours.
28. **A shell that says it is core-neutral must not reach past the seam.** `AppState.host`
    was `Arc<SessionHost>`, and three commands used methods only the Rust core has
    (`host.provider().list_models_info()`, `settings::search_view(&state.host, …)`,
    `settings::search_probe(&state.host, …)`), so the type system was quietly siding with
    one core while the docs claimed a trait. On `Arc<dyn HostOps>` they are compile errors —
    which is the good outcome: the trait verbs (`models_info`, `search_view`,
    `search_probe`) answer for both cores, and pi's answer comes from pi or from the
    bridge's own configuration.

---

## OPEN QUESTIONS (answer in the journal as we decide)

1. ~~**Workspace pool policy**~~ **ANSWERED and BUILT** — no cap; idle reaper at **600 s**
   (never mid-turn, never with a card open), resume by pi session file, switch never
   interrupts an in-flight turn, concurrent turns allowed with the count made visible.
   See decision 14 for the measurements. Remaining *UI* work for this decision: the
   running-turn count in the composer/status area, so the cost of concurrency is visible.
2. ~~**Desktop: in-process vs embedded localhost gateway**~~ **ANSWERED for in-process**
   (decision 12). Sub-question left: does the AppImage still ship `cwoffee-gateway` as a
   separate binary for headless / phone-only use, over the same bridge crate?
3. **`share_markdown` / `render_report`** — pi `export_html`, or our renderer over our index?
4. **Compaction parity** — pi's default `reserveTokens` 16,384 / `keepRecentTokens` 20k vs our
   `context_budget` and window reporting. Do we tune them to keep our append-only cache story?
   (pi is cache-aware — it has a cache-warmer and disables cache writes for summarisation —
   but "byte-identical prefix" was *our* invariant and is now pi's behaviour, not ours.)
5. **Does a coworker want pi's compaction summary tone?** It is written for coding sessions.
6. **`ask_user` multi-question cards** — sequential `select`/`input` dialogs (free, matches
   our `pending_q` deque) vs a custom tool + our own card protocol (Back/Next, one card).
7. **Do we keep `crates/tui` as a bridge-driven debug client?** (It becomes ~300 lines if the
   frames are already right; useful in the field, but it no longer shows 7 MB.)

---

## VALIDATION COMMANDS

```bash
# 1. the scripted endpoint, unchanged, on the port our selftest uses
node scripts/mock-llm.mjs --port 8190

# 2. pi pointed at it (isolated config + sessions, no network chatter, nothing written home)
export PI_CODING_AGENT_DIR=/tmp/pi-probe PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1
pi --mode rpc --no-session --provider cwoffee-mock --model cwoffee-mock \
   -e "$PWD/spike/pi-extension-cwoffee-tools.ts"

# 3. drive a turn, auto-approve, print the event trace + PSS samples
node spike/drive-rpc.mjs                     # repro of S0.1–S0.5, numbers on stdout

# 4. the Rust tool on its own (guardrails)
printf '{"path":"/etc/hostname"}' | spike/cwshim read_file

# 5. the gates that survive all of this
scripts/selftest.sh ; scripts/memory.sh cwoffee-app 400 ; scripts/sandbox-probe.sh
node scripts/gateway-smoke.mjs
```

### Parity check (what "no behavioural change" looks like, Move 0 onward)

```bash
# build both cores' gateway into the shared target dir, copying each artifact out
CARGO_TARGET_DIR=../cwoffee/target ~/.cargo/bin/cargo build --offline -p cwoffee-gateway
cp ../cwoffee/target/debug/cwoffee-gateway /tmp/gw-branch        # this tree
(cd ../cwoffee && ~/.cargo/bin/cargo clean -p cwoffee-gateway \
   && CARGO_TARGET_DIR=../cwoffee/target ~/.cargo/bin/cargo build --offline -p cwoffee-gateway)
cp ../cwoffee/target/debug/cwoffee-gateway /tmp/gw-upstream
cmp /tmp/gw-branch /tmp/gw-upstream || echo "binaries differ (expected)"   # do not skip this

# start each on its own port + fresh db, seed identically, run the same gate
for tag in upstream branch; do
  port=$([ $tag = upstream ] && echo 8101 || echo 8102)
  /tmp/gw-$tag --url http://127.0.0.1:8190/v1 --model cwoffee-mock --port $port \
     --token SMOKE-TOKEN-123 --workspace /tmp/cw-smoke-ws --db /tmp/gw-$tag.db \
     --dist "$PWD/../cwoffee/ui/dist" & gp=$!
  sleep 2
  node scripts/gateway-seed.mjs $port SMOKE-TOKEN-123          # one real turn per core
  node scripts/gateway-smoke.mjs --url ws://127.0.0.1:$port/ws --token SMOKE-TOKEN-123 \
       > /tmp/smoke-$tag.txt 2>&1
  kill $gp; sleep 1
done
diff /tmp/smoke-upstream.txt /tmp/smoke-branch.txt   # only the port line may differ
```

The mock endpoint (`../cwoffee/scripts/mock-llm.mjs --port 8190`) must be running, and
`cargo`/`rustc` are at `~/.cargo/bin/` here.

### The bridge probe (what a turn looks like with pi as the core)

```bash
# scenario fixtures, regenerated whenever pi is bumped
cd ../cwoffee && node scripts/mock-llm.mjs --port 8190
cd ../pi-cwoffee/spike && ./build-shim.sh && cd ..
SCENARIO=read-file-approval node scripts/pi-event-capture.mjs   # writes the test fixture

# one streamed turn, with the client answering the approval card
CWOFFEE_SHIM=$PWD/spike/cwshim cargo run --offline -p cwoffee-bridge --bin cwoffee-bridge-probe -- \
  --url http://127.0.0.1:8190/v1 --provider cwoffee-mock --model cwoffee-mock \
  --workspace /tmp/pi-ws --agent-dir /tmp/cw-bridge-probe --db /tmp/cw-bridge-probe/store.db \
  --ext spike/pi-extension-cwoffee-tools.ts --tools "-read,-bash,-edit,-write" \
  --prompt "Use read_file to read the file /etc/hostname" --approve allow

# the same conversation across a process restart (resume by pi session file)
... --switch 1 --prompt "What is 6 times 7?"
# the reaper closing an idle process mid-conversation, then resuming it
... --idle-reap-secs 1 --gap-secs 6 --prompt "What is 6 times 7?" --prompt "Use read_file to read the file /etc/hostname"
# fail-closed: denied, and unanswered two ways
... --approve deny
... --no-answer --approval-timeout-secs 4
... --drop-card  --approval-timeout-secs 4
```

The probe exits nonzero if no turn ended, which is what makes it usable as a gate rather
than a demo.

### The desktop app with pi as its core (what to click)

```bash
# one build, two cores. CWOFFEE_CORE is the switch; CWOFFEE_PI_EXT/CWOFFEE_SHIM mount the
# Rust tool shims, which is development packaging (S2.5) standing in for the real thing.
CWOFFEE_SHIM=$PWD/spike/cwshim \
CWOFFEE_PI_EXT=$PWD/spike/pi-extension-cwoffee-tools.ts \
CWOFFEE_PI_AGENT_DIR=/tmp/cw-app-pi \
CWOFFEE_CORE=pi CWOFFEE_MODE=ask CWOFFEE_GATEWAY_PORT=8122 CWOFFEE_GATEWAY_TOKEN=DESKTOK \
  target/release/cwoffee-app

# drive the live app as a client would, from the other side of the seam:
node scripts/gateway-approval.mjs --port 8122 --token DESKTOK \
  --workspace "$HOME/cwoffee-workspace" --answer allow
# and without pi: drop CWOFFEE_CORE/CWOFFEE_PI_*/CWOFFEE_SHIM/CWOFFEE_PI_EXT
```

`CWOFFEE_PI_KEEP_MODELS=1` leaves an existing provider file (and a real pi login) alone;
`CWOFFEE_PI_ANY` drops the version handshake for exploration only. A fresh clone has no
`ui/dist` (gitignored) — copy the upstream one or `npm --prefix ui run build`; the UI source
is byte-identical to upstream at this commit, which is the point of the zero-UI-edit claim.

### The gateway with a pi core, and the approval round trip

```bash
# the browser protocol, unchanged, with pi behind it
CWOFFEE_SHIM=$PWD/spike/cwshim cargo run --offline -p cwoffee-gateway -- \
  --core pi --mode ask --url http://127.0.0.1:8190/v1 --model cwoffee-mock \
  --port 8109 --token T-PI --workspace /tmp/cw-smoke-ws --db /tmp/gw-pi.db \
  --dist ../cwoffee/ui/dist --pi-agent-dir /tmp/gw-pi-agent \
  --pi-ext spike/pi-extension-cwoffee-tools.ts
node scripts/gateway-seed.mjs 8109 T-PI
node scripts/gateway-smoke.mjs --url ws://127.0.0.1:8109/ws --token T-PI

# the same gate on the Rust core: drop --core / --pi-* and CWOFFEE_SHIM.
# then the approval round trip, both ways, on whichever core is up:
node scripts/gateway-approval.mjs --port 8109 --token T-PI --answer allow
node scripts/gateway-approval.mjs --port 8109 --token T-PI --answer deny
```

`--mode ask` matters: the gateway inherits the desktop config's mode, and the first run of
this gate silently ran in Auto and answered itself ("allowed without asking") — faithful
behaviour, and exactly why the gate prints the mode it ran in.

---

## PROGRESS JOURNAL (newest on top)

### 2026-10-15T03:00Z — session 5 — the app opens, and pi is the thing behind it ✅

- **S1.2 desktop half met, and the blocker fell.** The desktop crate compiles here for the
  first time in five sessions — the failure was never code, it was absolute paths cached by
  `tauri`'s build script from the repo's *previous* location (gotcha 27; the same poison
  waited in the `release` profile and had to be cleaned twice).
- **Wired:** `AppState.host: Arc<dyn HostOps>` (three call sites had been reaching past the
  seam — gotcha 28), `CWOFFEE_CORE=rust|pi` at boot, `pi_host()` in `tauri-app` staging
  extensions + a provider file through `bridge::mount` (never overwriting an existing
  `models.json`), and decision 15's quit-while-busy refusal on `CloseRequested`, carried by
  a `Note` so the UI needed no edit: first close refuses and says so, a second within 8 s
  aborts, waits ≤2 s for settle, then exits. **The quit policy is compiled and wired but not
  yet exercised by a human or a test** — say that plainly rather than banking it.
- **Measured on the user's own machine, release build, real local model**
  (`llama.cpp` @ 127.0.0.1:8090, `qwen3.8-flash-next-iq3_s`, XHigh, sandbox on):
  one approval card, `read_file`, `OUTSIDE the workspace: /etc/hostname`;
  `gateway-approval` **allow and deny both OK** against the live app on 8122; a turn that
  ran after the 600 s reaper had closed pi, i.e. respawn works in the shipped binary.
- **Memory, whole tree, pi core live:** app 106 MB + WebKitWebProcess 129 MB +
  WebKitNetworkProcess 37 MB + **pi 79 MB** ≈ **351 MB**, against the 400 MB ceiling and the
  ≤110 MB pi-core budget. The web content process, not pi, is the largest single item in
  this picture — worth knowing before anyone blames the new core for the total.
- **Boots with what it mounted printed:** `core pi 1.2.0 up in ~/cwoffee-workspace; 1
  extension(s); tools -read,-bash,-edit,-write` — the guardrail line is in the log because a
  switch that is silently off is the failure mode this project keeps running into.
- **Debt taken on deliberately:** the pi-core construction now exists twice (gateway
  `main.rs`, tauri `main.rs`) with slightly different policy knobs. S4.2's dual-core A/B is
  the point where that duplication must be merged into one `bridge` constructor, because two
  cores that start differently cannot be compared.

### 2026-10-15T02:10Z — session 4 — the gateway serves a pi core, and the browser never noticed ✅

- **S1.2 (browser half) met.** `cwoffee-gateway --core pi` boots pi 1.2.0 behind the same
  `EngineHandle` Move 0 left behind and passes `gateway-smoke.mjs` — cold (no turn yet) and
  warm — with **no edits to `ui/`**. Boot prints what it mounted: provider, extension count,
  and `tools -read,-bash,-edit,-write`, because a guardrail that is silently off is the
  failure mode worth a log line.
- **New gate:** `scripts/gateway-approval.mjs` — one tool call through the gateway, one card
  answered by a client, and an assertion about the stored transcript. **allow and deny pass
  on both cores**, one card each, same `risk=sensitive`, same "OUTSIDE the workspace"
  wording. That is the security property the whole move hinged on, now measurable from a
  browser client instead of argued.
- **Three real bugs, all found by wiring rather than by reading:** the double approval card
  (gotcha 23), success reported as refusal (24), and `get_available_models` failing on a cold
  bridge (26). Plus a bug in the gate itself (25): it had quietly started asserting one
  core's dialect.
- **Shared rather than duplicated:** `bridge::mount` stages extensions and writes the dev
  `models.json` for probe *and* gateway; the gateway calls the engine's `check_workspace` so
  both cores refuse a workspace in the same words. `FALLBACK_BUDGET` became `pub` for the
  same reason — two cores, one context meter.
- **What was not covered before, said plainly:** `gateway-smoke.mjs` had zero matches for
  `respond_approval`, so the gateway's approval plumbing was unit-tested only. The new script
  closes the round trip; keep both in CI.
- **Still open for S1.2/S5:** the desktop half (the same handle in-process, plus the
  quit-while-busy policy of decision 15 — decided, unbuilt); attachments (refused with a
  note); `search_probe` (Phase 2 refusal); the running-turn count in the UI; extension
  packaging so a card carries a tool's real arguments instead of the dialog's title/message
  (S2.5).
- **Gates after this session:** engine 427 + 10, gateway 31, bridge 15 (7 unit incl. mount,
  7 fixture/replay incl. the parked-time one), `gateway-smoke` OK on both cores,
  `gateway-approval` OK both ways on both cores.

### 2026-10-15T00:40Z — session 3 — two numbers settled, and pi now runs as a cwoffee core ✅

- **The two open numbers are closed (decisions 14–15).** Idle reaper: **600 s**, never
  mid-turn, never with a dialog open anywhere, and *lossless* because a respawn passes
  `--session <file>` from the row↔file pairing. Quit while busy: refuse and say how many
  turns are running, then on confirm `abort` all, wait 2 s for `agent_settled`, exit;
  SIGKILL only as a bounded fallback. The quit path is **decided but not built** — it lives
  in the desktop shell (S5), and saying so here keeps it from looking finished.
- **S1.1 built:** `crates/bridge` — `process.rs` (spawn, LF framing, id correlation, version
  handshake, stdin-close shutdown), `translate.rs` (pi records → `EngineEvent` + store rows
  + telemetry), `host.rs` (`HostOps` over a pi child: addressing, dialogs, reaper,
  workspace move, session switch), `sessions.rs` (row↔pi-file pairing), plus
  `cwoffee-bridge-probe`, a client that talks only through `HostOps`/`StoreOps`. **~2.8k
  lines, against this plan's ~1k estimate** — the gap is the parts that turned out to hold
  the decisions, not the plumbing.
- **Recorded, not remembered:** `scripts/pi-event-capture.mjs` writes a real pi turn to
  `crates/bridge/tests/fixtures/pi-events-read-file-approval.jsonl` (47 records: 2 pi turns,
  5 messages, 25 streamed updates, a tool call, a `confirm` dialog, the usage trailer). The
  translator is tested against that, so a pi bump moves the test, not just the docs.
- **Gates:** engine 427 + 10, gateway 31, bridge **12** (5 unit + 7 fixture/replay) — 480
  passing. Gateway smoke still OK on the branch with the new crate in the workspace.
- **Six probe runs, all reproducible from the commands in §5:** allow (4 store rows, one
  approval card with `escapes=["/etc/hostname"]`, `cached=491/1039`); resume across a process
  restart (`resume Some(…jsonl)`, 4 rows → 6, no duplicates); reaper + resume (`prompt=1072
  cached=506` after a respawn at 191 ms); deny (tool row became `blocked: user denied
  approval`, the Rust shim never ran); silence (pi's own 4 s timeout denied — `ok=false`);
  dropped viewer (note said the tool did not run). Ready latency 190–290 ms per workspace.
- **Four bugs the tests caught before anything shipped:** (1) transcript rows were stored
  twice — once per message and again at settle, caught by the role-sequence assertion;
  (2) `message_start`/`response` counted as unknown records, which would have made the
  "pi sent something new" signal useless; (3) `--tools=…` is silently ignored by pi's
  parser, so the switch that disables pi's built-in coding tools would have been a no-op;
  (4) wall-clock rates counted time parked on a human as generation — 2 tok/s instead of
  119 for a turn with a 4 s approval wait, which would have poisoned the dual-core A/B.
- **Also found:** my own description of the send address was wrong. The Rust core does not
  "refuse a mismatched session"; it *moves* to the named conversation and refuses only an
  id with no row. The bridge reproduces the real behaviour, and decision 11 now says so.
- **Unresolved:** one engine test failed in the first three-package run and my grep lost the
  name (gotcha 22). Seven later runs were clean. Name it next time.
- **Next (S1.2):** construct `EngineHandle` from `PiHost` in `crates/gateway/src/main.rs`
  behind a `--core pi` flag — the gateway already takes `Arc<dyn HostOps>` plus the same
  three receivers the bridge hands out, so the browser client should work with **no UI
  edits at all**. That is the S1.2 exit criterion, and it is now a construction change.

### 2026-10-14T23:40Z — session 2 — three decisions locked, and Move 0 lands ✅

- **User decisions recorded as 11–13:** no workspace-process cap with an idle reaper;
  switching never interrupts an in-flight turn (it is bridge routing, not an interrupt —
  the per-event `session` on `TurnStart` plus the `Send` assertion are what make that
  safe, and the UI has to show the running-turn count so the cost is not hidden); the
  desktop joins **in-process** on the same traits; pi is pinned, consumed as a published
  package, with a version handshake that refuses to run on mismatch and a fork-rebased-on-
  tag route if a fix truly belongs upstream.
- **Move 0 done:** `HostOps` (9 verbs) + `StoreOps` (14 verbs) defined and implemented in
  `crates/engine`; `EngineHandle` holds `Arc<dyn …>`. 266 insertions / 28 deletions, and
  every changed call site is delegation, no logic.
- **My own earlier estimate was wrong and is corrected here:** the surface is **23 verbs,
  not 16** — the one-line grep could not see multi-line call chains, and the compiler found
  the other seven (`search`, `session`, `rename_session`, `set_workspace_label`, and the
  three live search-panel capabilities). The seam is still small; it is just not that small.
- **Proven neutral, not asserted:** `cwoffee-engine` 427 + 10 tests pass, `cwoffee-gateway`
  31 pass, and `gateway-smoke.mjs` against a binary from upstream HEAD versus one from this
  branch differs **only in the port it prints** — with both stores seeded by the same new
  `scripts/gateway-seed.mjs` (one real turn through the mock endpoint, 2 transcript rows
  each). That seed script is deliberately a client, not a test: it is the cheapest way to
  make the smoke gate stop reading an empty database.
- **Caught a pre-existing break, not ours:** `cargo check -p cwoffee-app` fails at upstream
  HEAD in this environment because the cached `tauri` build-script output still points at
  `/home/dandandelion/Projects/cwoffee/target/...`. Reproduced in `../cwoffee` to be sure.
  Consequence: the desktop crate can't be typechecked here, so the identical `EngineHandle`
  construction is asserted by a gateway test instead. Their call whether to pay the
  `cargo clean -p tauri` rebuild.
- **Next:** S1.1 `crates/bridge` implementing `HostOps`/`StoreOps` over a pi child process.
  The exit criterion is unchanged and still the real test: a live turn in the existing
  desktop UI with zero UI edits (S1.2). Two decisions worth making before S1.1 starts, both
  small: the idle-reap threshold, and what app-quit does while a turn is running.

### 2026-10-14T14:44Z — session 1 — clone + Phase 0 spike ✅

- Cloned `cwoffee` → `pi-cwoffee` @ `ffa1a9a` (`origin` = the Rust product, so upstream
  changes are still fetchable). Wrote this plan + `docs/PI-CORE-PROTOCOL.md`.
- Built and ran the Phase 0 spike end-to-end; committed it to `spike/` so it is not lost
  when `/tmp` clears. pi **1.2.0** installed at `~/.local/share/pi-node/…/bin/pi`.
- **It works with our own fixture.** Our `scenarios.json` + `mock-llm.mjs` drove pi with no
  edits. That is the whole reason this looks cheap: the streaming contract test we already
  trust re-targets the new core for free.
- **The tool loop stays Rust:** the `read_file` call went to our extension → a Rust binary →
  its envelope back into the transcript (`durationMs:405`). Nothing about the tools needs a TS
  rewrite; the extension is ~30 lines of arg schema + spawn.
- **The approval round trip survives and stays fail-closed**: `extension_ui_request` reached
  the client, 400 ms later we answered, and pi auto-denies at `timeout`. Same posture as
  `ApprovalRequested` today.
- **Telemetry math changed under us** (gotchas 1–2): `in:95 cacheRead:1081` — pi excludes
  cached tokens from `input`; and pi has no `ttft` at all. Both bars are now the bridge's job.
- **Memory measured, not guessed:** re-ran the committed spike 3× — idle 72.5–74.6 MB,
  peak 89.6–92.6 MB (one outlier 110.3), settled 73.3–76.9 MB; RPC ready in 176–311 ms.
  ≈338 MB peak total against a 400 MB ceiling — the headline product claim survives, but the
  headroom roughly halves. That is the price, and it is now a number we argue with.
- **Two structural surprises** that shape the plan: pi cannot change `cwd` mid-session (→
  workspace = process pool) and pi has no session-identity notion (→ our refusal moves into
  the bridge, decision S1.3).
- **Not done / not started:** everything in Phase 1+. `crates/engine` untouched; `master`
  still runs the Rust core. Next session: S1.1 the bridge, with the S1.2 exit criterion
  (a live turn in the existing UI, zero UI edits).
