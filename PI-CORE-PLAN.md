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

- **Phase:** 0 — **spike DONE** (S0.1–S0.5 ✅). The bridge (Phase 1) is **not started**.
  Nothing in `crates/engine` has been deleted. `master` is still the Rust-core product.
- **Last updated:** 2026-10-14T14:44Z (assistant, session 1)
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

- [ ] **S1.1** `crates/bridge`: spawn/supervise `pi --mode rpc` per workspace; LF-binary
      JSONL framing (never a line reader that splits U+2028/9); id correlation; shutdown by
      closing stdin. ~1k LOC. *Exit:* a CLI debug client prints a real streamed turn.
- [ ] **S1.2** frame emulation: bridge emits the **existing** `event`/`resync`/`bootstrap`
      payloads; `ui/` untouched. *Exit:* desktop app runs a live turn with **zero UI edits**.
- [ ] **S1.3** session-identity assertion in the bridge (`Send{session:Some(id)}` refuses
      when the host moved on) — pi has no such notion; 20 lines here protects the
      desktop-vs-phone invariant.
- [ ] **S1.4** approvals + `ask_user` → `respond_approval` / `respond_question` mapping,
      incl. `question_set` grouping (multi-question cards: OPEN QUESTION 6).
- [ ] **S1.5** telemetry in the bridge: TTFT, live tok/s (`≈`), `Usage`, `context_tokens`,
      `compactions`, cache state from `usage.input + cacheRead (+cacheWrite)`.
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

---

## OPEN QUESTIONS (answer in the journal as we decide)

1. **Workspace pool policy** — cap live pi processes, idle-kill timeout, and what a
   workspace switch does to an in-flight turn (refuse? drain? fork the pi?).
2. **Packaging** — embed Node + pi in RPM/AppImage (size, updates, and the supply-chain
   surface of shipping a JS runtime next to a sandboxed Rust core). Vendor + hash, or
   system-node + clear refusal when absent?
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

---

## PROGRESS JOURNAL (newest on top)

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
