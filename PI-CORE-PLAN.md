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

- **Phase:** 1 done on both paths, and the product surface is now **on** the pi core: eleven
  Rust tools mounted from the registry, approval cards, workspace-following, mid-turn switching,
  search panel, attachments, metrics with a probed window, **Chat mode**, and a settings screen
  with categories. `cwoffee-gateway --core pi` and `CWOFFEE_CORE=pi` on the desktop; `master`
  still defaults to the Rust core, which stays the fallback and the A/B half.
- **Last updated:** 2026-10-16T07:10Z (assistant, session 10) — conversations are now
  named by a **230M model running on this machine** (`LiquidAI/LFM2.5-230M`, CPU only),
  after the first turn, out of process, opt-in; live on the pi core ("hey, I want to go
  over the quarterly cashflow numbers…" → **"Quarterly cashflow numbers"**).
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

### D12 — a conversation gets its name from a small local model, after the turn, out of
process, opt-in, and never over a name a human chose (locked 2026-10-16)

The store's automatic title has always been `derive_title`: the opening line, whitespace
collapsed, cut at 48 characters. It is honest, and useless in exactly one place — a sidebar
of forty rows that begin "hey can you", "can you look", "I want to". Nobody finds a
conversation by reading those.

Locked shape:

* **Model:** `LiquidAI/LFM2.5-230M`, CPU-only bf16, ~460 MB of weights, 4 threads. Chosen
  because it is untied-small, already on this class of machine, and needs no endpoint:
  titling must not spend a round trip on the expensive model, and it must work offline.
* **Out of process, one shot.** `scripts/titlegen.py` reads the opening message on stdin,
  prints one line on stdout, JSON stats on stderr, and exits. Measured: **2.6–2.8 s to
  load, 0.4–0.7 s to generate, 861–864 MB peak RSS**. That single number is why it is not
  a library and not a resident server: 870 MB is more than the whole desktop tree normally
  costs (290 MB with no model up), and it comes straight back. `--serve` exists for a caller
  with an idle reaper, and nobody has one yet.
* **After the turn, never on its path.** Both cores call `titlegen::maybe_name` at settle.
  The row already has an honest title the instant the first message is stored, so a name is
  always an improvement on something readable; the conversation never waits for decoration.
* **Eligibility is a comparison, not a flag.** Offer the helper only a row whose stored title
  still equals `derive_title(first user message)` (or `new session`). That is what "nobody
  has named this" means, and it makes a hand-named row untouchable by construction.
* **Refusal is a first-class result.** The helper *rejects* output that answers instead of
  naming ("Sure, here's a summary…"), and then the automatic title stays and the reason is
  said — in the log, and on screen when the feature was switched on by hand. A wrong title
  is a false claim about the conversation; a dull true one is not.
* **Opt-in.** `titlegen = true` (or `CWOFFEE_TITLEGEN=1`) in the config. The default is off
  because the cost is a ~870 MB spike and this is an app people leave open on a laptop. The
  Titles section of Settings quotes that cost next to the model, the paths and what is
  missing, from one second's work rather than a model load.

Gated by `scripts/gateway-titlegen.mjs`: a scripted helper proves the app honours a name and
changes *nothing* on a refusal; the real model then names a real sentence
("…quarterly cashflow numbers, they look short to me" → "Check your quarterly cashflow
numbers"), and `titlegen.py --check` answers availability without loading weights.

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
- [x] **S2.2** `ask_user` as a tool that parks the turn — **BUILT ON BOTH CORES**, gated by
      `scripts/gateway-ask.mjs` on each. On the pi core the tool runs in a separate process, so
      the questions are read out of the engine (`cwtool questions`, the tool's own parse), asked
      in the host's UI through pi's dialog primitives, and handed back with `--host-answers` —
      a flag, never `args`, because `args` is what the model wrote. One call, its questions as
      one set, and the picks land in the engine's own restatement (gotchas 89-92).
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
| **NEW** local titling | `node scripts/gateway-titlegen.mjs` | the name arrives after the turn, a refusal changes nothing, and the real model names a real sentence |
| **NEW** tool questions | `node scripts/gateway-ask.mjs --core pi` and `--core rust` | one call, its questions as one set (id/index/total, options with trade-offs, the recommendation), the picks back in the tool's own words, and the scripted endpoint only answers once they reach the prompt |

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

29. **A `select!` loop obeys a click the engine would have queued.** The Rust core queues `SwitchSession` / `NewSession` / `ClearSession` until the turn finishes; the bridge's model task handled them the instant they arrived, so clicking another conversation mid-turn wrote the rest of A's turn - tool result and closing answer - into B. The user saw it as scrambled history on reopen. `Engine::deferred` holds those three while a turn runs and re-sends them at the settled boundary. Gate: `scripts/gateway-midturn-switch.mjs` (fails on pi, clean on rust, before and after). It does not repair sessions the old code already wrote.
30. **A conversation and its folder travel together.** `pi` has no verb to change `cwd` mid-session, so `PiHost::follow_workspace` treats a workspace move as process lifecycle: reap, relaunch in that folder, push `CWOFFEE_WORKSPACE` into the child. If a turn is running elsewhere, the move is refused *and* the turn that asked for it is refused. A successful move must not bump `refusal_seq` - it did, and the panel said "the move was refused" while the folder had in fact moved.
31. **A spike guardrail a user cannot tell apart from the real one is a bug.** The demo `read_file` allowed only `/etc/*`, so a session in `~/Downloads/cashflow` recorded ten `read_file refused:` rows for real project files and the model dutifully tabulated them. Confine to the conversation's workspace and name the root in the refusal.
32. **Mount the tools from the registry, not from a copy.** `cwtool specs | check | run` is the single owner of definitions, schemas, risk verdicts and summaries; the pi extension holds no policy. Descriptions and schemas are prompt bytes: two copies means two prefix caches and one guaranteed drift. `cwtool run` serialises the whole engine `ToolResult` - hand-picking fields silently loses `error`, `files` (produced-file cards) and `changes` (+N/-M).
33. **The tool output cap is one number shared by both cores.** `SessionHost` truncates at `24 * 1024`; `cwtool` started at 64 KiB, which would make pi and rust differ in the context meter, the prefix cache and memory. Same for `FALLBACK_BUDGET`, now `pub` so both cores draw the same meter.
34. **Whoever starts a container has to outlive it.** `AutoSearXNG`'s idle reaper lives in the process that started it; a `cwtool` process exits immediately, so `cwoffee-searxng` would have run forever. The bridge adopts the container, counts searches (`web_search`, `web_fetch`, `download_file`, plus panel probes) and reaps after the configured idle. Related: a settings panel's placeholder text is a claim about the world - the bridge kept saying "this bridge does not mount the tool extensions yet" and "no podman on this machine" after both were false. `scripts/gateway-search.mjs` now fails on the old wording.
35. **A gate that fetches its own evidence can be the bug.** Adding a `message` listener per retry hit Node's 11-listener cap and the gate saw nothing. One listener with a reply map, then assert the *stored* transcript in the tool's own output shape: engine `read_file` numbers lines, paginates long files, and the row is JSON-escaped inside SQLite, so first-40-character assertions fail for reasons that have nothing to do with the feature.
36. **`--tools=x` is not `--tools x`.** pi 1.2.0 puts the `=` form into its unknown-flag path; a parser that tolerated it would leave the built-in coding tools enabled and nothing would say so. The boot line prints the tools list for exactly that reason.
37. **The window embeds its UI; the phone reads it off disk - found near the executable, that is the wrong disk.** `frontendDist` is baked into the binary at compile time, while the gateway/browser path walked up from the executable and landed on another checkout's `ui/dist` (Oct 12). Now `build.rs` records the absolute path (`CWOFFEE_UI_DIST_BAKED`) and `ui_dist_dir()` prefers it, warning when that directory is gone. Two habits follow: `cargo build -p cwoffee-app` does *not* rebuild the UI, and after a harness run (`VITE_SELFTEST=1 npx vite build`, which `selftest.sh` does) the release binary embeds the harness bundle - rebuild the product UI and the app before handing it back.
38. **Fold state is per-origin, and the port is part of the origin.** Collapsed folders live in `localStorage` (`cwoffee.wsCollapsed.v1`) keyed by path; `:8122` and `:8123` are different sidebars, and the desktop's own store read as `[]` - a preference never set is indistinguishable from one lost. Values there are UTF-16 blobs, so read them with `strings -el` or a hex dump. The app now logs `sidebar: N folder(s) remembered closed` once per load and prunes paths that no longer exist so a deleted folder comes back open when re-added. Durable fix: app-side UI prefs over `StoreOps`.
39. **A probe that fires nothing can report a clean pass.** A same-origin parent page driving the app in an iframe with synthetic `MouseEvent`/`PointerEvent` dispatch got no response from the iframe - not even a plain click on a folder chevron that works for a real user - while printing rows and zeros as if it had tested something. Deleted. Verification goes through the app's own page harness instead: `VITE_SELFTEST=1` build *plus* runtime arming via the `selftest_armed` verb (`CWOFFEE_SELFTEST=1` in the host), because a build-time flag alone once ran the checks against the user's real endpoint, config and store. Note `selftest_armed` is a Tauri command only: the page harness cannot be armed through the gateway.
40. **A settings screen must hold the same controls, not copies of them.** The sections render the popups' own body snippet, so a setting cannot mean one thing in the menu and another in the screen; the screen is addressable (`#search`) because a place you cannot link to is a place a phone cannot go to directly.

41. **Chat mode changes the process, not the label.** Tools and the system prompt are chosen
    when pi is exec'd — there is no RPC to take them away afterwards. So switching is process
    lifecycle: reap the child, start the next one with `--no-tools`, no extension mounted, and
    `--system-prompt "You are cwoffee, a helpful assistant"`, and refuse to follow a
    conversation into a folder while in chat. A switch asked for during a turn is deferred by the
    same queue as conversation retargeting (gotcha 29). The Rust core refuses in words: that core
    *is* the coworker prompt plus the registry, and a "chat" that still has eleven tools is a
    different product wearing the same label.
42. **An extension that fails to load is silent in RPC mode.** `pi-tools/cwoffee-tools.ts`
    prints `[cwoffee-tools] no tools: CWOFFEE_CWTOOL is not set` on stderr — and in RPC mode that
    print never reaches us; pi keeps its child output to itself. The session then answers
    "Tool read_file not found" with nothing in the log explaining it. The bridge now says it at
    launch, in the log that *does* reach the operator. Rule: complain where the log goes, not
    where the failure is.
43. **A line about the child must be printed by whoever owns the child.** The gateway's boot line
    claimed `1 extension(s); tools -read,-bash,-edit,-write` from its own config, and once the
    mode could change per process that became a false claim about a guardrail. The bridge logs
    every launch with the flags that process actually received; the shells state only what they
    know (pinned version, persona, extensions staged).
44. **Two nulls are not a comparison.** The first chat gate "passed" its chat half because the
    trigger phrase did not match any scripted scenario, so *neither* mode ran a tool: cards=0,
    toolsRan=0 on both sides. The gate now asserts the baseline (cowork must open a card for the
    same prompt) before it is allowed to be impressed by the absence.
45. **`HostState` has a hand-written mirror in `ui/src/lib/events.ts`.** Adding a field to the
    Rust struct (`persona`) does not add it to the UI's type; svelte-check catches it only if the
    UI reads it. Until something ties the mirror to the struct, every new state field needs both
    edits — and a selftest that reads it is the cheapest tie.
46. **A percentage needs a denominator somebody stated.** The dev provider entry we write for pi
    was `{"id": model}` — no `contextWindow` — so pi had no window, the panel had no limit, and
    the bridge carried a pinned 32k while the Rust core derived 80% of the window. Now the shells
    probe the endpoint (as the Rust core already did), write `contextWindow` into pi's model
    entry, and take the ceiling from `mount::context_budget` → the engine's `auto_budget`.
    Measured on the pi core against the mock: window 262144, budget 209715.
47. **Attachment validation belongs to the engine's function, and the proof belongs at the
    endpoint.** The bridge now runs `check_attachments` (same mime list, count, byte cap, base64
    shape as the Rust core) and forwards pi's `ImageContent`. The assertion is the scripted
    endpoint reporting the blocks it received (`Attachment seen: 1 image(s): image/png`) — a
    check against the store alone passes for a core that saves the file and never sends it.
48. **Report what was echoed, and say when the echo is short.** pi knows nothing about the
    client's filename, so the stored row keeps the *client's* name and byte count; if pi echoes
    fewer image blocks than the bridge sent, the bridge emits a note rather than filing the
    difference. An attachment that never reached the model is the one failure a user cannot see.
49. **`pkill -f` matched its own command line twice and killed the harness.** The pattern
    (`… 8153`) also appeared in the *shell* that was running the kill, because the same command
    line started the gateway. Self-inflicted exit 143, twice, each costing a rerun. Use
    `pkill -x <process-name>`, or put the kill in a command that does not also start the thing.

50. **pi omits image blocks when the model entry says nothing about modalities.** The dev
    provider entry we write carried `{"id": model, "contextWindow": …}`; pi classified the model
    as text-only and dropped the picture **before the request**, with no error and no event. The
    entry now declares `"input": ["text", "image"]` (`mount::write_dev_provider`). Proof of the
    whole path is `scripts/gateway-attach.mjs`: the scripted endpoint reports
    `Attachment seen: 1 image(s): image/png`, and the stored row keeps the client's `probe.png`.
51. **The gateway used to drop `attachments` on the floor, in silence.** `start_turn` carried a
    commented decision not to accept them, and the client was never told — a phone that attached
    a picture watched the answer a turn as if it had been shown one. A decision that is not
    communicated to the person who made the gesture is not a decision, it is a disappearance.
    `start_turn` now parses them and applies the engine's own `check_attachments` +
    `AttachmentLimits::defaults()` at the boundary (mime list, count, 2 MiB each), so both cores
    refuse the same way: `attachment refused: attachment 1: mime "application/x-msdownload" is
    not allowed (…)`.
52. **A helper that is never called is the same bug as a fake guardrail.** `mock-llm.mjs` had
    `renderStep(step, messages)` — written, correct, and wired into nothing, so `{images}` came
    back literally and the check "passed" while proving nothing. Grep for the call site, not the
    definition.
53. **A harness block must not print success after failing.** `fail()` in the page harness records
    and *continues*, so a block that fails halfway and reaches its own `pageLog("… ok")` produces
    a log line that is false. The mode block now guards its tail on an `ok` flag; earlier it
    logged "mode toggle ok" on the same run that had just failed twice.
54. **Two assertions the metrics change forced, learned the slow way:** the context row now leads
    with the *share* of the window the endpoint states, with the exact counts in the tooltip — so
    the selftest asserts both halves (`%` on screen, `context_tokens` in the `title`) rather than
    the count it used to read off the row.

55. **Never signal a process by name, and never take over a process that has a parent.** While
    cleaning up leftover model processes I ran a `kill` on pids chosen from `pgrep -x pi`, and one
    of them was the interactive pi session I was working inside. A name is not an identity. The
    bridge's adopt step now requires three things before any signal, as a pure predicate with
    tests: `ppid == 1` (the kernel's own definition of an orphan — a live parent means somebody
    else owns that process and its idle clock, including a second gateway sharing the agent dir),
    a `--mode rpc` marker in the process line (only a driven pi is a bridge's child), and the
    launcher's own executable name (pid reuse is real; a recycled pid inherits no old claim). The
    tests include "a pi somebody is talking through is not a target".
56. **A half-deleted database is a corrupt database.** Removing `foo.sqlite` and leaving
    `foo.sqlite-wal` / `-shm` behind makes the next gateway exit with `sqlite: disk I/O error`.
    Throwaway gates must use a fresh path, or delete the trio.
57. **`kill_on_drop` is not a reaper.** It runs on a normal Rust drop, not when the process is
    SIGKILLed. Two pi processes from dead hosts were measured holding 205 MB between them, with
    no idle clock anywhere; hence the ledger (`<agent_dir>/pi-children.txt`) that the next bridge
    reads and adopts.

58. **Half the window is the budget, not 80%.** Two ceilings were cutting people off. pi's provider
    composer defaults a model's max output tokens to `16384` unless the entry declares
    `maxTokens` (`definition.maxTokens ?? 16384`), and cwoffee's `auto_budget` gave the prompt 80%
    of the window, leaving a fifth for the answer. A chat asked for an SVG of a pelican on a
    bicycle and the markup was cut off mid-drawing with no error anywhere — a ceiling that
    truncates an answer is not a ceiling, it is a truncation. Now `auto_budget` is **half** the
    probed window, and the generated model entry declares `maxTokens = clamp(window/2)` — never
    below pi's default, never above the window minus the prompt, and **no probed window means no
    cap claimed**. The harness had to change too: it asserted `budget ≥ 75% of window` to catch a
    32 000 fallback sitting under a 262 144 window, which is a different failure than the one it
    was written to catch; it now asserts a derived share (0.25 ≤ share ≤ 1, and not the fallback
    constant) instead of one core's constant.
59. **A tool card that is empty on one core and full on the other is one transcript telling two
    stories.** cwoffee renders a *live* card from `toolExecOutput` chunks and a *reloaded* card
    from the stored row's content. pi reports a tool's result once, at the end, and the bridge
    streamed nothing — so the live card was an empty box (the sheet cells, the query grid, the
    "which row named the columns" line all invisible) while the same session reopened from the
    store showed the text. The bridge now emits the tool's own output as one chunk before
    `toolExecEnd`, in the shape the Rust host uses, and nothing else: the words are the tool's.
    The same fix carries `changes` onto the row (the card's `+N -M` is a store column,
    `tool_meta_json`) and stamps `files` with the store's own `set_message_files`, which is what
    the file chips and "what did this run make?" read.
60. **`[data-x]` matches `[data-x="none"]`.** The check "chat mode offers no approvals control"
    matched its own *no-approvals-here* marker and failed a run that was correct. Name the control
    you mean: `[data-approvals]:not([data-approvals="none"])`.
61. **Recording a fact on a row is half the feature.** The chat list is built from the same
    in-memory session rows as the folder list, so after writing `persona` the UI has to re-read
    the list — otherwise the chat you just started is missing from the list made for it.
62. **Compaction is a model call, not a flag.** pi's `{"type":"compact"}` answers with
    `tokensBefore` / `estimatedTokensAfter` / `firstKeptEntryId`, so the note quotes pi's numbers
    instead of an estimate invented in the UI. The verb refuses while a turn is running (folding
    history under a run that is reading it) and refuses when no model process is up (there is no
    thread to fold), and it gets a 300-second budget because a 30-second one would report a
    timeout as though compaction were broken. The compaction counter is *not* bumped in the verb:
    pi also emits `compaction_start`, and counting in both places counts twice.
63. **A conversation's mode is a fact about the conversation, not about the window.**
    `sessions.persona` (schema v11, default `'cowork'` by the schema so every caller need not
    remember it) says which mode made a row. Chat mode swaps the sidebar's folder tree for a flat
    chat list built from that column, newest first, and a `persona='chat'` row never appears in a
    folder group: filing a chat under a workspace makes the sidebar claim something about it that
    is not true. The composer says so where the folder line used to be, and offers no approvals
    control where nothing can need approving.

64. **Compaction's ceiling is a config key, not a guess.** A ~200 k-token fold is a model
    call over the whole conversation and on a local endpoint it runs for many minutes —
    300 s was a number that had never been measured against a real session. So
    `compact_timeout_secs` (default 1800) sizes the wait, the bridge's own outer wait is
    that plus 15 s so the side that holds the token counts and the reassurance answers
    first, and a timeout states what is actually known: nothing was thrown away, pi may
    still be summarizing, the context meter after the next turn is the fact, and the knob
    is named. A ceiling that fires early does not make compaction quicker — it makes a
    working compaction look broken and charges for the summary twice.
65. **A rule one core states and the other silently violates is two rules.** The harness
    reported three failures — a session's folder moved, the refusal had no reason, the
    refused move still landed — and they were one cause: the bridge never had the folder
    lock the rust core has had for a long time. `host::folder_lock` is that sentence now,
    in the engine, called by both cores. Two ordering faults rode along: the path was
    checked *after* the model process was reaped, so a refusal cost a process and a warm
    prefix cache, and the reaping came before the store's own probe. **A refusal must
    change nothing.**
66. **pi retries a dead endpoint without telling the user, and the bridge translated
    none of it.** Captured off the wire: an endpoint that closes the socket mid-answer
    produces an assistant `message_end` with `stopReason: "error"` and
    `errorMessage: "terminated"`, then `auto_retry_start` ×3 (waits 2 s, 4 s, 8 s), then
    `auto_retry_end{success:false, finalError}`. Naive translation stacks four error cards
    on one half-answer; ignoring it leaves a sentence that simply stops. The translator
    holds the error, notes the retry (attempt N of 3, waiting M s), and fails once when the
    run closes: *"the endpoint closed the connection in the middle of the answer: terminated
    (after 3 attempts)"*. The half-answer stays stored, because it happened.
67. **pi repairs truncated tool arguments silently.** For a call whose streamed JSON is
    missing its closing brace, pi's parser hands over a finished object and runs it. The
    rust core repairs the same way but says so — a call something else finished is not the
    call the model wrote. pi leaves no trace of the decision, so the bridge compares the
    streamed `toolcall_delta` fragments (which carry no id: the id comes from the
    `toolcall_start` that opened the block) with what actually ran, and when the wire text
    does not parse it says `agent::repaired_args_note`'s sentence.
68. **Files stored on a tool row are invisible.** The flood case stopped failing on "0
    files" and started failing on "no produced-files strip": the bridge was faithfully
    stamping each tool's output on its own tool row, and the transcript builds a strip only
    under an **answer** — which is where the rust core puts the whole run's output ("a
    reload shows the same attachments the live turn showed"). Collect across the run, stamp
    once at settle, on the assistant row that closed the turn; and a run that made files
    but stored no answer says so instead of losing them quietly.
69. **An icon's shape is a claim.** The Settings button used a circle with rays — the
    glyph every UI in the world uses for brightness — so in dark theme the sidebar footer
    appeared to hold a light-mode switch. A cog, stroked in `currentColor`, follows the
    theme instead of assuming one.
70. **A perf gate measures the machine, not just the page.** The frame-cadence check
    failed once with "53 frames in 1407 ms while the clock idle was 17 ms" — while cargo
    builds were running beside it. Before believing a stalled-page report, look at what
    else was on the box; the fix for a load-induced failure is sequencing, not a relaxed
    threshold.
71. **"The title changed" is not an assertion — the store already renamed it.** The row
    leaves `new session` the moment its first message lands, so a gate that waits for
    `title !== 'new session'` passes before the title helper is even asked, and then passes
    forever while the model does nothing. Assert the row moved *off* `derive_title(text)` —
    which is the same comparison the app uses to decide whether it is allowed to name it.
72. **A 230M model will answer you, and few-shot examples make it worse.** Asked cold for
    "a short title", LFM2.5-230M returns sentences about itself ("I can't access external
    files…", "Sorry for the confusion"). Few-shot examples in the prompt raised verbosity,
    leaked the examples' language into the answer, and sometimes copied an example. What
    worked: one strict instruction ("2 to 6 words… a noun phrase, never a sentence"),
    **prime the answer with an opening quote** so it generates inside quotation marks (a
    noun phrase is what sits inside quotes), and use the closing quote as a stop signal —
    then reject what is left with words, because the model is small and will occasionally
    answer anyway.
73. **Availability has to be answerable without paying for the thing.** A Settings panel
    that reports "the weights are not installed" by loading the weights freezes for three
    seconds and spends 870 MB to introduce itself. `titlegen.py --check` answers in ~1 s
    (versions from `importlib.metadata`, weights by walking the cache), and the Rust side
    answers the same question from the filesystem alone — no python spawn at all.
74. **A bare program name is a path question, and `is_file()` says "no".** `titlegen_python
    = "python3"` means the one on PATH; a presence check that stats the literal string
    reported "no python" beside a working interpreter. Resolve through `$PATH` first, and
    then the check means what it says. Same class as gotcha 22 (a CLI list spelled wrong
    parses as nothing): a knob that silently fails is worse than a missing knob.
75. **A gate that mispoints the real thing looks like a broken feature.** The title gate's
    "real model" phase first failed because the app was passing the gate's *placeholder*
    cache (an empty `.safetensors` for the scripted helper) to the real helper, which
    correctly refused to download 460 MB. The failure read as "the model did not name it".
    When a real-thing phase fails, read which paths the caller handed it before believing
    the thing itself.
76. **What the model reads and what the card reads are two different claims, and the tool
    extension was making one claim do both jobs.** The pi tool extension used to hand the
    model `JSON.stringify(envelope)`. So the model read a spreadsheet grid as one escaped
    line — `Region\nUnits\nWest\t26` — which costs more prompt tokens, reads worse to a
    small model, and broke the mock endpoint's `require` strings, which look for a real tab.
    pi already has the right seam: `AgentToolResult.content` is model-facing, `details` is
    "kept out of the prompt, for logs and UI". Model gets the engine's own `render_result`
    bytes (exported, because *what a model reads for a tool result is engine policy* — two
    cores wording it differently is two behaviours); the envelope travels in `details`, where
    the bridge reads `files` and the `+N -M`.
77. **Keep both vintages when the shape of a tool result moves.** Fixtures captured before
    the change put the envelope in the model-facing text; a bridge that only understands the
    new shape loses the file chips and the diff badge *silently*. `envelope_of()` prefers
    `details` and falls back to parsing the text, and both are tested.
78. **Your own message has to be in the store before the host asks anything of a model.**
    The rust core records a send as a fact, then asks; the pi bridge let pi's echo create
    the row, so while the bridge was opening a pi thread for a conversation the message you
    had just sent was not on screen — the oldest "my message disappeared" complaint, reborn
    as a 4-second window. Now `on_send` appends the user row (attachments included) before
    `ensure_child`, tells the translator to skip pi's echo row, and the echo still does its
    reconciliation work — the suppression is one message, not one session, and a store
    failure falls back to the echo rather than losing the message.
79. **A note only one core says is a hole, not a style difference.** The rust core explains
    what a resumed conversation costs ("resumed session N: … message(s), ~N tokens …"); the
    pi core said nothing, so the same click on the same row was described differently
    depending on what was behind it, and the harness noticed. The bridge now says it once per
    session per process, claiming the stored transcript's size always, a token figure only
    once a turn has reported one, and never a cache state — which stays the endpoint's
    business.

80. **A gate that asserts a wire shape stops testing the product the moment the shape
    moves.** `gateway-workspace.mjs` asked for `"ok":true` in the stored transcript, and
    `gateway-approval.mjs` asked for the phrase "outside the workspace root". Both were
    descriptions of one core's encoding, not of the claim being tested. When the tool's own
    words became the model-facing text (gotcha 76), the stored row stopped containing JSON
    at all, and a run in which the model had plainly read the file came back as "no
    successful tool read in the transcript". Say what the product promises: a read stores
    numbered lines and a "showing N of M" note, a refusal says which workspace it is
    confined to, and an error row starts `ERROR (exit …)`.

81. **A gate may not assume scratch state it did not create.** Two gates died this session
    for reasons that were not the app: the approval gate expected `/tmp/cw-smoke-ws` to
    already exist (the gateway correctly refused `"/tmp/cw-smoke-ws": is not a directory`
    and the gate reported "no approval card ever arrived"), and the workspace gate read
    `README.md` out of the user's real workspace and died on an ENOENT stack with no
    verdict at all. Both now create their folder and, when the fixture file is missing,
    write their own — and say in the log that they wrote it, so nobody reads a seeded
    fixture as a file of their own.

82. **A flattened `\` continuation writes its whitespace into the sentence.** 39 runs of
    4-20 spaces were baked into prose the model and the user read — including a compaction
    timeout note, the confinement refusal ("…outside the workspace /tmp/x.                     Use a
    path…"), the truncation-continuation prompt, and the "no answer, the model spent its
    tokens thinking" note. Sweeping them out is a one-line regex, and the regex is where the
    danger is: three fixtures *use* wide spacing as the thing under test — the denylist
    surviving `sudo    umount`, `read_file`'s `"2:     if x:"` line numbering, and Python
    indentation inside `"def f():\n    return 1"`. Those were restored and annotated.
    Fix prose, then read every hunk; do not trust a substitution over test fixtures.

83. **Ask whether a server is current, not whether a port is busy.** The harness refused to
    run because a mock endpoint was answering on 8190 — a mock it had itself been handed,
    holding the *current* scenario file. Worse, its advice was `pkill -f 'mock-llm.mjs
    --port 8190'`, which is exactly the pattern-kill habit that once ended an unrelated
    process in this project. `mock-llm.mjs` now reports a sha256 fingerprint of the
    scenarios file it loaded; `selftest.sh` reuses a mock whose fingerprint matches the file
    on disk, and when it does not, it names the single pid from `ss` and says to end that
    one rather than anything answering to a name.

84. **Naming eligibility, proven on live rows.** The gates create their sessions with an
    explicit title (`"excel workflow"`, `"workspace follows the conversation"`), so the 230M
    helper leaves those rows alone — which is comparison-based eligibility working on real
    data, not just in a fixture. The flip side is stated rather than hidden: rows that have
    not settled since titles were turned on keep their automatic title forever. There is no
    backfill pass, and "name this conversation" as an explicit verb is not built yet.

85. **Prove which core a harness run was on, or its board means nothing.** `selftest.sh`
    builds the self-test UI bundle, builds `cwoffee-app` into the checkout's own `target/`,
    makes scratch config/data/workspace dirs, starts its own mock endpoint and gateway, and
    **launches its own app**. It has no `--url`, no `--token`, no `--page-log`: a recipe that
    pre-launches an app and passes those flags is not driving that app. Which core the
    instance it *does* launch uses comes entirely from the inherited environment
    (`CWOFFEE_CORE`, `CWOFFEE_CWTOOL`, `CWOFFEE_PI_*`) — and every run before this one was
    launched without `CWOFFEE_CORE`, so it ran the **rust** core. The tell was `ask set ok`:
    the scripted question set cannot produce a card on the pi core at all (gotcha 86), so a
    green ask line is positive evidence of a rust-core run. Verify with
    `tr '\0' '\n' < /proc/<pid>/environ | grep CWOFFEE_CORE` before believing any parity
    number, and print the core in the harness header so the log carries it.

86. **A missing feature can look like a passing test for two sessions.** The pi core has no
    `ask_user` road: `cwtool` builds `ToolCtx` with `ask: None`, and the extension never calls
    `ctx.ui.select`/`input`. The bridge half was built long ago for approvals (`ui_request()`
    understands `select` and `input`; `on_ui_request` raises `QuestionRequested` and answers
    the extension), so the two missing ends are small — and I had written in the plan that the
    feature was built and gated by a script that does not exist. The rule is the one this file
    already has: do not record a claim a gate does not prove, and when a claim comes from
    memory rather than from a file, check the file first.

87. **A test script must run the binaries this tree builds.** Two page-harness checks — the
    sheet-query grid and the header-row note — stayed red through a whole session. On an
    isolated endpoint the same scenario passed in one run, because the needles the scripted
    endpoint demands (`West` + tab + `26`, `# columns: rownum | region | units | price`) are searched
    for in the *prompt bytes*, and the harness had been running a `cwtool` lying around in
    `target/release` from before the tool host started exporting the engine's rendered
    sentence. With JSON in the prompt instead of the grid, the tab between the region and the
    total arrives as a backslash and a `t`, the needle never matches, and the endpoint refuses
    to play its final step —
    which surfaces as "the grid reached the card but not the answer", a sentence about the UI
    that was really about a stale binary. `selftest.sh` now builds `-p cwoffee-bridge --bin
    cwtool` on the same line as the app and points the run at its own copy unless the caller
    named one on purpose.

88. **Isolate the endpoint you are testing, or you are testing somebody else's run.** A
    scratch gateway that does not set its own `XDG_DATA_HOME` writes its provider entry into
    the shared `~/.local/share/cwoffee/pi/models.json`, whose `baseUrl` belongs to the running
    desktop app. `write_dev_provider` then refuses to overwrite an entry it does not own —
    correct, that is another install's configuration — and the run *continues against the other
    endpoint*, which answers plausibly. Two scratch runs this session drove the live app's mock
    on port 8193 without ever touching the mock they started. A scratch harness needs
    `XDG_CONFIG_HOME` *and* `XDG_DATA_HOME`; `selftest.sh` has always had both, my throwaway
    reproduction scripts did not.

89. **pi's dialog primitives are narrower than cwoffee's question card.** `ctx.ui.select`
    carries a title and a list of bare labels; `ctx.ui.input` carries a title and a
    placeholder. Nothing in that shape can hold an option's one-line trade-off, which option
    the model recommended, whether several may be picked, or that this is question 2 of 3 —
    and those are exactly the parts that make `ask_user` worth calling. So the engine's own
    `Question` rides in the title after the `CWOFFEEQ ` marker, and the bridge re-numbers it:
    the question ids a client answers back are this process's, and the set id the UI groups by
    is this process's too, because `cwtool` numbers a fresh set in every process and two
    conversations' cards would otherwise merge into one. When a payload will not parse the
    bridge says so out loud — a silently flattened card is a feature that rotted, and the next
    person would read it as a feature that was never there.

90. **A person's answers do not travel in model-authored arguments.** `ask_user`'s point is
    that the model cannot answer its own question. `args` is the model's text, so answers come
    back to the tool host as a separate flag, `--host-answers`, matched to the questions by
    position. The extension asks from `cwtool questions` — the tool's own parse, the same one
    the call will do — so nothing in TypeScript decides what to ask, and a null entry (nobody
    answered) is carried by dropping the reply sender, which is how the engine already says
    "nobody answered" instead of being handed an empty sentence.

91. **An answer the gateway cannot see is an answer nobody gave.** `respond_question` reads
    `text` and `options` off the args themselves — `{ id, text, options }`, flat. A client that
    nests them (`answer: { text, options }`) gets a successful RPC and an empty answer, and the
    tool then reports — correctly — that the questions were cancelled. That is the second time
    this session a *correct* product refusal was really a bug in my own test client, and both
    times the fix was to read the handler before doubting the feature (gotcha 80 is the same
    lesson from the other side: assert the product's words, and know the protocol's shape).

92. **A question that was answered should say so, on both cores.** `EngineEvent::
    QuestionAnswered` has been in the enum the whole time and nothing in the Rust core emitted
    it — only the pi bridge did. So on the rust core, a card answered in one window stayed
    interactive in every other window until each of them was clicked. Now `ask_user` emits it
    itself where the tool runs in-process, and the bridge emits it where the tool runs in
    another process: exactly one announcement per question either way, because two windows
    closing a card twice is the same bug wearing a different coat as one never closing.


93. **A hoisted helper can read a neighbour `let` before it exists, and the page dies without a
    verdict.** `frame()` sat below `blind('at the start')`; `blind` recovers a drifted view
    through `ensureTranscriptMounted → reopen → frame(0)`, and `frame` increments `framePaints`.
    Function declarations are callable before the `let` next to them runs, so the *recovery*
    path — the one that only fires when something already went wrong — raised
    `Cannot access 'Re' before initialization`, the harness stopped measuring, and the board sat
    out its watchdog. Two fixes: declare the shared helpers above their earliest caller, and give
    a page-side throw its own verdict (`verdict=throw`, quoting the `UNCAUGHT:` line) so a dead
    harness is reported as the harness's own failure rather than as a mystery silence.

94. **Do not edit a script a running board is executing.** bash reads a script as it goes, so
    adding a few lines to `scripts/selftest.sh` under a live run produced `line 274: r: command
    not found` and `syntax error near unexpected token 'done'` in the middle of an otherwise
    healthy board. That second red has nothing to do with the app. Check that no board is
    running before touching the runner — and prefer editing the harness source, which is
    compiled before the run starts.

95. **A caller's key is not an identity.** The bridge groups a question set by the key the
    extension sends, and every plausible key turned out to be reusable: the engine numbers the
    first set of every call at 1 (`cwtool` is a fresh process per call), and pi's tool-call id is
    whatever the endpoint put in the tool call — a replayed scenario hands out `mock-ask-1` twice.
    Ask the same conversation the same thing twice and the second call's questions landed on the
    first call's card. A key now maps to a card **while that card is open**: `QSet` counts what the
    set promised, what has arrived and what has closed, and a fully closed set leaves the map.

96. **A `require` string in a scenario is the endpoint judging the tool, and its refusals have to
    be visible.** `ask-two-questions` step 1 refuses to answer until the prompt carries the
    engine's restatement of the user's picks. When that failed, the only symptom was a missing
    string three layers away. `mock-llm` now logs `SERVE <scenario> step n/m` and `REQUIRE …
    missing`, so "which step did the endpoint answer, and what needle did it reject" is one line
    in the log instead of a guess.

97. **Parity on an event means both cores emit it, not that both declare it.**
    `QuestionAnswered` sat unused in the engine enum for the whole port; the pi bridge emitted it,
    the Rust core never did, so answered cards stayed live in every other window on the old core.
    Grepping *who sends* an event is a cheaper parity check than grepping who can.

98. **`--bin` is a target filter over *every* selected package, and cargo will not tell you.**
    `cargo build --release -p cwoffee-app -p cwoffee-bridge --bin cwtool` builds the bridge's
    `cwtool` and **no binary of `cwoffee-app`** — the app has no bin by that name. Cargo compiles
    the app's dependencies, prints `Finished`, exits 0, and with `-vv` never mentions the package
    at all. The executable on disk is then whatever older build last wrote it: a whole board ran
    an app from three hours earlier and reported reds about code that no longer exists. Never let
    a board trust an exit code: compare the artifact against the sources (`find crates ui/dist
    -type f -newer target/release/cwoffee-app`), force one relink when it loses, and refuse to run
    when it loses twice.

99. **A board must say which tree its artifacts came from, and refuse to share a target dir with a
    tree that packages the same names.** Upstream cwoffee and this clone both define `cwoffee-app`
    and `cwoffee-gateway`; cargo keeps freshness per workspace, so one shared target dir can hold a
    binary linked from the other tree's sources while this tree's build says it is done.
    `scripts/selftest.sh` now dies if `CARGO_TARGET_DIR` points outside the checkout, and prints
    the artifact timestamps and the target dir it used. This is a guard, not the cause of 98 — the
    same skip reproduced in the clone's own `./target`.

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
6. ~~**`ask_user` on the pi core**~~ **ANSWERED and BUILT on both cores**, and the road is the
   one written below, in the order the pieces exist. It stayed "not built" here for a session on
   purpose: the board that said otherwise was the rust core (gotchas 85, 86), and the honest
   board was red. What was added at the two ends: `Tool::host_questions` in the registry (so
   nothing in TypeScript decides what to ask), `cwtool questions` / `--host-answers`, the
   extension's `askOne` over pi's `select`/`input`, and the bridge re-numbering the engine's
   `Question` — which arrives in the dialog title, because pi's primitives cannot carry option
   details, a recommendation, multi, or "2 of 3" (gotchas 89-92).
   The shape of the card was decided rather than left open: **one card holding every question**.
   * the **extension** asks, because it is the only party that can: pi runs the tool inside the
     extension, so the bridge first learns of the call at `tool_execution_start`, which is too
     late to ask first. `ctx.ui.confirm/select/input` already round-trips to the bridge
     (approvals use it) and RPC-mode extension UI works with a timeout that auto-resolves —
     the same fail-closed posture as approvals.
   * the **bridge** turns that request into cwoffee's own question card (`QuestionRequested`)
     and answers the extension when the client responds or the clock runs out, so there is
     still one card with one owner.
   * the **answers reach `cwtool` as a flag, never in the args**: `cwtool run ask_user
     --host-answers <json>`, building a `ToolCtx.ask` that returns them. Args are
     model-authored, so answers arriving there would let the model answer its own question —
     which is the entire point of the tool.
   * the **engine keeps the wording**: `ask_user` formats its own result ("nobody answered",
     the answer list) so both cores say the same thing to the model.
   The card the page harness asserts is the card both cores send: `.ask-count`, one
   `.ask-submit`, `.ask-back`/`.ask-next`, selections kept when you go back. Sequential dialogs
   would have been twenty lines and would make the phone show three popups in a row for one tool
   call. What is left here is not the road but the traffic: `multi` answers travel through pi's
   one-label `select` as a joined string and are split back into picks — good enough for the
   engine's "few words" labels, and worth revisiting if an option label ever grows a pipe.
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

### Session 12 — the tools' words reach the model for real, and the gates get held to the same standard ✅

**Claim:** the Excel workflow works on the pi core against the user's own workbook, and the
three reds left in the page harness are now about the harness, not the app.

- Ran `gateway-excel.mjs` against the running app (real endpoint, `~/Downloads/cashflow`,
  `BES Cash Flow.xlsx`, sheet `Local Fund`): 14 tool calls (`sheet_read`/`sheet_query`), one
  approval card answered, and the answer contains **946,010**. That number can only come from
  the file, which is what the gate is for. `EXCEL GATE OK`.
- `WORKSPACE GATE OK`, `MIDTURN SWITCH OK`, approval **allow** and **deny** both OK, title gate
  OK. The allow half is worth restating because it is subtle: answering *allow* does not lift
  the tool's own confinement — the Rust tool still refuses `/etc/hostname` and names the
  workspace it is confined to, and under `mode=auto` the escape is carried as a permit, which
  is why the same call succeeds there. Two different decisions, both correct, both asserted.
- Live titles, on the running app: session 1352 reads **"Quarterly cashflow numbers"** (the
  230M model's phrase, not the truncated first line), while gate-made rows kept the titles
  their gates chose. Memory with a model process up: **331 MB PSS** for the whole tree — app
  91 MB, pi 88 MB, the two WebKit processes 160 MB. The children ledger holds exactly one pid.
- Then the gate clean-up (gotchas 80-83): three gates were asserting encodings or assuming
  scratch folders, and the mock-endpoint staleness check was answering the wrong question.
- 39 baked-in space runs removed from prose (gotcha 82), with three fixtures restored because
  their spacing *was* the test.

- **A claim I had to take back.** Before writing the journal I had put in the plan that
  `ask_user` was built on both cores and gated by `scripts/gateway-ask.mjs`. That script does
  not exist; `crates/bridge/src/bin/cwtool.rs` builds `ToolCtx` with `ask: None`; the
  extension has no question call at all. I believed it because the page harness prints
  `ask set ok`, and I assumed that board was the pi core. It was not: `selftest.sh` launches
  its own app, and none of my recent recipes exported `CWOFFEE_CORE` — so the "6 red → 2 red"
  numbers in sessions 9-11 are **rust-core** numbers. The run started today has its core
  verified from `/proc/<pid>/environ` (`CWOFFEE_CORE=pi`, `CWOFFEE_CWTOOL=…`), and the ask
  case is expected to fail on it. That failure is the honest board (gotchas 85, 86).

**Tests:** engine 431 + 10, bridge 9 + 12, gateway 31, `svelte-check` 0 errors.

**What is still open:** the first proven pi-core harness board (running now — the ask case and
anything else the rust core did for itself will show up as red), and the frame-cadence case,
which needs a quiet machine to be believed either way.

### Session 11 — the model reads a tool's words again, and a send is stored before it is asked

Ran the page harness on the pi core: **6 red** (4 carried over, plus a message that vanished
from the transcript and a geometry skip because a second app window was covering the run).
Three fixes, each of which was a claim the app was making wrongly rather than a missing
feature:

- **The tool extension was handing the model a JSON envelope.** pi's `content` is what the
  model reads; `details` is what the UI and the log read. The extension put
  `JSON.stringify(envelope)` in `content`, so a spreadsheet grid arrived as one escaped line
  (`West\t26` became `West\t26` with literal backslashes) — more prompt bytes, a worse
  table for a 230M-parameter reader, and the mock endpoint's `require` strings stopped
  matching, which is why the sheet-query and titled-header turns "stopped before their last
  step". Now `cwtool` carries the engine's own `render_result` text (made public — wording a
  tool result is engine policy, not the bridge's) and the extension puts *those bytes* in
  `content` and the envelope in `details`. The bridge reads metadata from `details` first
  and still parses the older shape, so the captured fixtures keep their authority (gotchas
  76-77). Three new bridge tests, one per vintage plus the dedupe.
- **A send is a fact the user made, so it is stored before anything is asked.** The rust
  core appends the user row, then calls the provider; the bridge let pi's echo create it, so
  while the bridge was opening a pi thread the message you had just sent was missing from
  the transcript. `on_send` stores the row (with attachments) before `ensure_child` and
  suppresses pi's echo row only — reconciliation and its note still run (gotcha 78).
- **The resumed-history note existed on one core only.** Same click, different story.
  Bridge says it once per session per process, with the numbers it can actually see
  (gotcha 79).
- Docs also caught up with a stale claim in the README: it still described the context
  budget as `window * 0.72 - 2048` after the budget became half the window (131,072 of
  262,144 measured). A formula in a doc that the code no longer implements is a false claim
  with better grammar than a bug.

### Session 10 — titles: a 230M model on this machine names the conversation ✅

The ask was small and specific: stop titling conversations with their first line, use
`LiquidAI/LFM2.5-230M` on CPU. What that turned out to touch, in order:

- **The helper, measured before it was wired.** venv with CPU torch 2.14.1 + transformers
  5.20.0, weights in `~/.local/share/cwoffee/titlegen/models` (443 MB, untied, not gated).
  One-shot: **2.6–2.8 s load, 0.4–0.7 s generate, 861–864 MB peak RSS, ~1 s of CPU per
  title.** That cost is the design: out of process, once per new conversation, never
  resident, and never on the turn's path.
- **The prompt was the hard part, not the integration.** Gotchas 72. Ending: strict
  one-line instruction + generating *inside an opening quote* + rejecting answer-shaped
  output with words. Selftest titles it now produces: "Cashflow shortfall analysis",
  "Rename photos by exif date".
- **Eligibility is `still_automatic(stored, first)`** — stored title equals
  `derive_title(first)` or `new session`. A row you renamed yourself is never touched, and
  that is a comparison rather than a flag someone has to remember to set.
- **Both cores ask after settle** (`titlegen::maybe_name` from the rust host loop and from
  the bridge at `agent_settled`), one helper at a time process-wide (`claim()`), rename via
  the store's `rename_session`, announce with `SessionChanged` so the row moves without a
  reload. Failures: log always, on-screen note when the feature was explicitly on.
- **Settings got a Titles section** fed by one read verb (`titlegen_status`, both
  transports) that quotes the model, the paths, what is missing, the config file to edit,
  and the measured 870 MB. `in_flight` is `Option<bool>` — the gateway genuinely does not
  know, and saying `false` would be a placeholder wearing a fact (gotcha 43 family).
- **Gate** `scripts/gateway-titlegen.mjs`: scripted helper names a row (rename + event +
  the helper was handed the conversation's own first message); a refusing helper changes
  nothing and says why in both the transcript and the log; the **real** model then names a
  real sentence on the real paths. First run red on the real phase — gotchas 74 and 75 were
  both real bugs found by that gate, one in the app and one in the gate.
- **Live on the user's install, pi core, after restart:** session 1352 opened with
  "hey, I want to go over the quarterly cashflow numbers, they look short to me" and the
  sidebar row became **"Quarterly cashflow numbers"**. `titlegen_status` reported
  enabled/script/python/weights all true, `in_flight:null`.
- **Cost stated where it is chosen:** `titlegen = true` now sits in the user config with a
  comment quoting 870 MB, because an opt-in that hides its price is a surprise later.
- **Still open:** the four page-harness parity failures from session 9 (sheet grid/header
  note into the answer, the resumed-history note, the scripted `ask_user` card — `cwtool`
  still passes `ask = None`, so a tool's question has no road back to a card), and a real
  compaction of a ~200k conversation end-to-end.
### 2026-10-16T04:30Z — session 7 — chat mode, pictures, a meter with a ceiling, and a settings page with categories ✅

- **Chat mode is real on the pi core.** `--persona chat` / config `persona = "chat"` / the Cowork–Chat
  tab in the composer all reach one verb, `set_persona`, and the bridge treats it as process
  lifecycle: no extension mounted, `--no-tools`, `--system-prompt` set to the one line
  `"You are cwoffee, a helpful assistant"`, and no folder followed. `session_state.persona` is what
  the tab reads back, so a second window sees the same truth and a refused switch cannot leave the
  tab claiming a mode the host never took.
- **Proof is an A/B on one prompt.** `scripts/gateway-chat.mjs`: the scripted endpoint answers
  `Use read_file to read the file /etc/hostname` with a tool call on both halves. Chat: 0 cards,
  0 tools. Switch to cowork (note says the process restarts and the prefix cache starts cold):
  1 card for the same prompt. `CHAT GATE OK`. The Rust core refuses at boot
  (`--persona chat --core rust` exits 1 with the reason) and at runtime (`set_persona` → refusal,
  state stays `cowork`).
- **Attachments reach the model on the pi core.** The bridge runs the engine's own
  `check_attachments` (`AttachmentLimits::defaults()` shared by both cores) and sends pi's
  `images` blocks; the translator keeps the client's filename and size in the stored row and notes
  when pi echoes fewer blocks than were sent. Proven endpoint-side: the mock now reports the image
  parts it received, and the harness asserts `Attachment seen: 1 image(s): image/png` plus the
  stored name.
- **Metrics finally have a ceiling, and the number leads with the ratio.** Context is shown as a
  percentage of the window the endpoint states (exact counts moved to the tooltip), pi's model
  entry carries `contextWindow`, and the ceiling comes from the engine's own `auto_budget`
  through `mount::context_budget`. Measured on the pi core: window 262144, budget 209715, tokens
  517 after a seeded turn — the same formula the Rust core uses.
- **Settings**: the entry point moved to the bottom of the sidebar (`aside-foot`, outside the
  scrolling conversation list — a control that scrolls away with the conversations is not findable
  when the list is what you are lost in), the rail is grouped (Answering / Working / Reaching out /
  This app), and there is a **Tools** section listing what `tools_view` returns — the engine's
  registry, so the list a user reads and the tools the model is offered are one thing. It says in
  plain words that nothing there is switchable yet, because per-tool permission is not wired.
- **Harness**: three new page cases — attachment round trip, the mode toggle through the actual
  tab (with an honest skip-and-check-the-refusal branch for the Rust core), and the Tools section
  asserting `read_file`/`sheet_query` arrive from the registry. svelte-check 0 errors/0 warnings.
- **Found and fixed on the way:** the gateway's boot line was lying about extensions and tools
  once the mode could change per process (gotcha 43); a missing `CWOFFEE_CWTOOL` made the
  extension register nothing and the model answered "Tool read_file not found" with no log line
  explaining it (gotcha 42).
- **Attachments: fixed twice over.** The harness said `Attachment seen: 0 image(s): none`, and the
  two causes were in series, which is why each looked like the other: the **gateway** dropped
  `attachments` from `start_turn` (a documented decision that never told the client — gotcha 51),
  and **pi** silently omitted image blocks because the model entry declared no modalities
  (gotcha 50). With both fixed, `scripts/gateway-attach.mjs` shows the endpoint receiving
  `1 image(s): image/png` and the stored row keeping `probe.png`; a non-image is refused in the
  engine's own words at the boundary, on either core.
- **What the pi-core harness actually says today (honest count, not a green checkmark).**
  `scripts/selftest.sh` on the pi core: **15 failures**, and they are a parity backlog, not noise:
  output files never attached to stored rows (`TurnFiles` not emitted by the bridge); the flood
  turn's 6 files missing (same cause); the `sheet_read` card showing no cells and the query grid
  empty (tool metadata not carried); "which row named the columns" note missing; the uncapped-turn
  note missing (that guard lives in the Rust engine, not in pi); `useSession(17): the host is on
  18` (addressing race); the resumed-history note never reaching the screen; `ask_user`'s scripted
  card never appearing (`ToolCtx.ask = None` in `cwtool`); a braceless tool call running without
  the engine saying so (`ToolCallParseError` not translated); an endpoint that dropped mid-stream
  surfacing nothing. Also fixed since that run: the attachment case above. This list is the next
  piece of work, and it is exactly what the dual-core A/B was always meant to produce.

- **Orphans, and a lesson learned the hard way.** A pi whose host died keeps running: `kill_on_drop`
  needs a normal drop, and two leftovers from killed runs measured 205 MB between them. The bridge
  now writes a ledger of the model processes it starts (`<agent_dir>/pi-children.txt`), removes the
  line when the process goes, and on startup adopts what the ledger names — under a three-part
  predicate (`ppid == 1`, `--mode rpc`, our own executable name) that is unit-tested, including the
  case that bit me: a pi with a live parent is somebody's session and is never a target. While I
  was writing that guard I killed pids chosen by name and ended the very pi session this work runs
  in; the user lost a session to my cleanup, and the predicate exists so the code never does that.
- **Attachments, release build, same proof:** `gateway-attach.mjs` against the release gateway says
  `ATTACH GATE OK (bytes reached the model …)` with `endpoint receipt: "Attachment seen: 1 image(s):
  image/png"`. The workspace gate also had to be fixed: its refusal check was a ±160-character
  window around the filename, which matched the model's own prose and failed a run in which every
  read of the file had succeeded; it now reads the path out of the refusal's own shape
  (`refused: <path>: <reason>`) and compares the basename.

### Session 8 — three product asks, and the parity list gets started

The user asked for three things and then said to start immediately on the failure list plus
compaction. All three landed, and five of the fifteen failures went with the first pass at parity.

- **Budget.** `auto_budget` is half the probed window (was 80%), and the pi model entry declares
  `maxTokens` derived from the window (gotcha 58). Measured on the pi core against the mock:
  `budget=131072 window=262144`. This was the pelican-SVG truncation.
- **Chat is a mode with its own list.** `sessions.persona` (v11) + `set_session_persona` on both
  shells; the sidebar in chat mode is a flat chat list, not a folder tree; the composer's folder
  line says "Chat has no folder, and no tools to use one with" and the approvals control is gone
  (gotcha 63). The page harness has a block for it, and that block found two of its own bugs
  (gotchas 60, 61) before it found a real one — it now re-reads the list it is checking.
- **Compaction.** `HostOps::compact` (async, default refuses in words so a core with no manual
  lever does not grow a dead button), the bridge's verb, the RPC verb `compact`, the Tauri command,
  and a `compact` control on the context row of the metrics panel. Refuses mid-turn and with no
  process up; quotes pi's token counts (gotcha 62).
- **Parity fixes (5 of the 15/17):** the tool's own output now reaches the live card as
  `toolExecOutput` (sheet cells, query grid, the header-row note); `changes` ride on the tool row
  so `+N -M` survives a reload; produced files are stamped with `set_message_files` (the espresso
  chip and the six-file flood); the uncapped-turn note is said by the bridge too, at the engine's
  own count and in the engine's own sentence (`agent::SOFT_ITERATION_NOTE`, `agent::uncapped_note`)
  so a claim about a long run is made by both cores or neither; and the attachment case passes on
  both cores. Two new bridge tests (`a_finished_tool_carries_its_own_words_and_what_it_made`,
  `a_long_uncapped_run_says_it_is_uncapped_on_either_core`) and one store test
  (`a_row_remembers_which_mode_made_it`, which also migrates a pre-v11 store).

Still open from the list, in the order they are worth attacking: `ask_user` cards (`cwtool` runs
tools with `ToolCtx.ask = None`, so the scripted question never reaches the App), the mid-stream
endpoint-drop surfacing nothing (`pi`'s provider error never becomes a cwoffee `Error`), a
braceless tool call running without the engine saying so (`ToolCallParseError` is a Rust-core
concept; pi parses its own arguments), `useSession(N): the host is on M` (an addressing race under
the harness's rapid switching), and the resumed-history note not reaching the screen.

### Session 9 — a longer compaction ceiling, a cog, and the first real cuts into the failure list

**The page harness went 17 → 13 → 12 → 4 failures.** Confirmed fixed by the harness itself:
`scripted repair ok (repaired malformed arguments for exec: the arguments as streamed did
not parse as JSON (42 bytes)…)` and `scripted dead stream ok (the endpoint closed the
connection in the middle of the answer: terminated (after 3 attempts); turn ended=true)`;
the workspace-move trio; and the flood, which now lists its six files and renders the strip
with the standalone page as its own chip. The `useSession(N): the host is on M` race did not
reproduce in that run; `useSession` now waits up to 2 s for the host to name the row it was
asked for and prints how long a switch took, so a switch that lands late and a switch that
never lands are different failures — that change is not yet gated.

Four left, in the order they are worth attacking: the scripted sheet loop stopping before
its last step (the grid and the header-row note reach the *card* but never the *answer*),
the resumed-history note not reaching the screen, and `ask_user` producing no card because
`cwtool` runs tools with `ToolCtx.ask = None` — that one needs a real channel from the Rust
tool back through the extension to the bridge, which is the largest remaining piece.

On the live app after this batch: `persona=cowork budget=131072 window=262144`,
`tools_view` 11, `compact` refuses in words with no process up (`nothing to compact yet:
this conversation has no running model process`), the search panel answers and owns the
managed container (`up=true exists=true`, probe 3.0 s / 5 hits), and the tree measured
**290 MB PSS** with no model process up yet (a live turn adds pi's ~75-90 MB). The app
reaped its own pi child on exit and removed its ledger line, which is the adoption path
being exercised for real.

Three asks: compaction's 300 s was too short for a ~200 k-token fold; the Settings glyph
was the light-theme icon; and go on with the harness plus the parity list.

- Numbers on the list: **17 → 13 → 12 → (run in flight)**. Gone so far: the budget
  assertion, the chat list and approvals chip in chat mode, attachments, the sheet cells /
  query grid / header-row note on the live card, the uncapped-turn note, the output file
  missing from the stored row, and — this session, waiting on the run — the workspace-move
  trio, the braceless tool call, the dead mid-stream endpoint, and the flood's missing
  strip.
- The list is being cut by **root cause, not by symptom**. The trio (move adopted, refusal
  silent, move landed) was one missing rule (gotcha 65). The flood's "0 files" becoming
  "no strip" was the difference between *stored* and *rendered* (gotcha 68). The dead
  stream and the repaired call were both cases where pi does something on its own that the
  bridge never surfaced (gotchas 66, 67) — both now have wire-captured fixtures, and the
  capture script can drive a scenario by its trigger text (`PROMPT`, `MODELS_JSON`) and
  point at its own endpoint, because two captures on one port read each other's traffic.
- Two of the four fixes in this session came from **reading pi's actual records** rather
  than reasoning about them: the retry sequence and the silent repair are both invisible in
  the RPC docs' examples. That is the argument for `pi-event-capture.mjs` staying part of
  the routine: a translation written against a doc example passes whether or not pi behaves
  that way.

- **Two loose ends from this batch, named so they are not forgotten.** (1) The file-row pairing
  fix is tested only at the translator half; the page harness's two file checks (the espresso chip
  and the six-file flood) are the gate for the pairing itself, and they were still failing when
  this was committed. (2) The app's boot line prints `[cwoffee/pi] core pi up … 1 extension(s);
  tools -read,-bash,-edit,-write` even when the persona is chat — the *per-process* line the bridge
  prints at spawn is correct, so this is wording on a line that reads like a fact about the process.
  It should say which persona the process will start with, or be dropped: a log line about a child
  that has not started is a prediction, and predictions printed as facts are how "the guardrail was
  mounted" got believed when the child never had it.
- **`ask_user` is the biggest thing left on the parity list.** A Rust tool asks by calling
  back into the host through `ToolCtx.ask`; `cwtool` hands it `None`, because a subprocess
  that has already printed its answer has no way back. Wiring it means a line protocol
  between `cwtool` and the extension, and the extension asking pi (which asks the bridge,
  which already owns the card and its timeout). Until that exists, every scripted question a
  tool asks silently goes unanswered — which is the same class of bug as a card that never
  arrives: the model waits, the user sees nothing, nobody is at fault.
- **Owed after this batch:** `/compact` and MCP are still not surfaced in the UI; `ask_user` still
  has `ToolCtx.ask = None`, so the tool says nobody can answer instead of raising a card; `exec`
  output is not streamed as `ToolExecOutput`; `CWOFFEE_MODE` is frozen for an already-spawned pi
  child; the settings sections for sandbox / `base_url` / core / idle-reap are still read-only for
  want of a write verb; the UI's `HostState` mirror is hand-written (gotcha 45); and the tree was
  **414 MB PSS** at the end of last session — over the 400 MB ceiling — and has not been re-measured
  since this batch landed.


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

### D13 — a tool's question crosses the pi boundary in the engine's own words, and the
answer comes back as a host flag, never as model text (locked 2026-10-19)

The old core ran `ask_user` in-process, so the question travelled as an `EngineEvent` and
the answer came back over an mpsc channel. On pi the tool runs inside an extension, and a
tool cannot hold a card open from inside a short-lived `cwtool` call.

Locked shape:

* **Ask before running.** `Tool::host_questions(&args)` (default `None`) lets a remote host
  ask the questions the tool *would* have asked. `ask_user` implements it. The extension
  asks `spec.host_ask`, which `cwtool specs` sets from the same registry — nothing in
  TypeScript decides which tool asks a person or what its options look like.
* **The answers are an argument to the run, not part of the tool's input.**
  `cwtool run ask_user --host-answers '[…]'`. A model that can put its own answers in
  `args` can answer its own question, which is the whole thing `ask_user` exists to prevent.
  The engine rejects any `ask_user` call whose arguments contain `answer`/`answers`/`answer_id`
  for the same reason; a test asserts it.
* **A payload rides in the dialog title.** `CWOFFEEQ {…}` carries the engine's `Question`
  through pi's title-and-labels primitives, and the bridge rebuilds it: the trade-off line
  under each option, the recommended option, "pick more than one", and "question 1 of 2".
  Unparsable payload → the bridge **refuses to flatten silently**: it says so and shows the
  flattened version, because silently dropping the model's trade-offs is worse than a loud
  degraded card.
* **Set ids are this process's to give.** The engine numbers the first set of every call at
  1, and even pi's tool-call id repeats when a scripted endpoint replays the same scenario.
  So the bridge keys a card on the caller's key *while that card is open*: once every
  question the set promised has arrived and then been answered or expired, the key is
  released and the next call gets a card of its own. `mark_set_closed` implements "closed",
  and an answered question and an expired one are the same event to it.
* **Both cores emit `QuestionAnswered`.** It used to be a bridge event only, so on the old
  core an answered card stayed live in a second window — a control still wired to a question
  the user had already settled. `AskUserTool` emits it where the answer is actually taken.

---

## Journal — session 18 (ask_user on the pi core, and a harness that died without a verdict)

Two things were proven this session: that `ask_user` — the one cwoffee feature most assumed
to be Rust-core-only — works on pi, and that a red board can be red about the board.

ask_user. The machinery was in place (`questions_for`, `--host-answers`, the `CWOFFEEQ`
payload, `questions` in the gateway, cards in the UI) and every layer looked right in
isolation. The gate still failed, and the interesting part was where it failed:

* the *endpoint* was refusing the step — `ask-two-questions` step 1 `require`s the engine's
  own restatement of the user's picks, and it was being answered with step 0's filler. The
  mock had no logging for "which step am I serving, and why did I refuse", so the failure
  appeared at the far end of the chain as a missing string. `SERVE` / `REQUIRE` lines now
  print the step and the missing needle.
* **the gateway takes `respond_question { id, text, options }` flat**, and the gate sent
  `{ id, answer: { … } }`. The handler answered "unknown question id" — and a card that
  refuses an answer leaves the tool waiting until the engine's own timeout says nobody
  answered. Same wire shape as `respond_approval`, and nothing said so until it was wrong.
* **the set key has to be the bridge's business.** Asking the same conversation twice — the
  obvious thing a person does — put the second call's two questions on the first call's
  card, because both calls' sets were numbered 1 by their own `cwtool` process and pi's
  tool-call id was identical across the two turns of a replayed scenario. The card is now
  keyed on "is this set still open", not on the caller's number.

Then the harness. It crashed inside two seconds of the first turn with
`ReferenceError: Cannot access 'Re' before initialization`, printed no verdict, and the board
would have sat out the full 900 s watchdog. The cause was mine from an earlier session: the
rAF-floor helpers sit after `blind('at the start')`, and `blind` recovers a drifted view by
calling `ensureTranscriptMounted → reopen → frame(0)`. A hoisted function is callable before
the `let` next to it is initialised, and reading one is not a warning, it is a dead run. The
declarations moved above their earliest caller.

Two harness lessons came out of that:

* A page-side throw is now its own verdict (`verdict=throw`) that quotes the `UNCAUGHT:`
  line, instead of a timeout. A harness that dies says so; the red belongs to the harness and
  the board should say which.
* **Do not edit a script while it runs.** bash reads a script as it goes: editing
  `scripts/selftest.sh` under a running board produced `line 274: r: command not found` and
  `syntax error near unexpected token 'done'` in the *middle* of a healthy run. That red was
  a second, unrelated, self-inflicted failure. Check for a running board before editing the
  runner.

Gates added or changed: `scripts/gateway-ask.mjs` now drives **two** ask_user calls in one
conversation and asserts they stay two cards with different set ids, that every question
carries a marker, and that answers come back for every question; both `--core pi` and
`--core rust` pass, and the pi run shows `questionRequested:4, questionAnswered:4` with the
endpoint's `require` satisfied by the tool's own words.

### It went green: the page harness passes on the pi core

With the artifacts proven current and the two harness bugs (the frame TDZ, and the ask check
looking for a tool card the renderer deliberately does not draw) fixed, the board ran
end-to-end on `--core pi` and printed a verdict for the first time:

```
[selftest.sh] artifacts  : app 10-19 13:33:45, target dir /home/dandandelion/AI/Harness/pi-cwoffee/target (built from this tree)
[cwoffee/page] scripted ask set ok (one tool call, 2 questions, one card; settled "answered 2 of 2";
               the stored transcript carries both restated picks)
[cwoffee/page] clipboard paste ok (97 bytes of image/png read off the desktop selection,
               named clipboard-133453.png, previewed in the composer)
[cwoffee/page] scripted bullets ok (4 of 4 were structure while streaming, counts 0->1->2->3->4)
[cwoffee/page] frame cadence ok
[cwoffee/page] SELFTEST ok: sessions 0->0, fts-safe query -> 0 hits, models=1 picked=cwoffee-mock
[selftest.sh] verdict: ok
```

Two of the fixes were about the harness measuring the machine rather than the app:

* **`wl-copy` does not exit.** It forks a resident process that owns the selection, so
  capturing its stdout and waiting held a pipe open forever: the paste check's request to
  re-stage the fixture never returned, the harness printed no verdict, and the board died on
  its 900 s watchdog. The verb now sends stdout to null and stderr to a file, and the page
  races the request against 4 s so a helper that never returns costs one honest skip.
* **A window that is not being composited has no mid-stream.** In one run every sample of the
  live-bullet check saw zero `<li>` while the turn streamed, and the turn's own `frames=0`
  said why: the incremental renderer writes nothing between paints. The check now skips that
  half — with the reason and the frame count in the log — when the window delivered no paints,
  and still fails when it did.

Live desktop after the rebuild (own artifacts, product bundle, `harness-in-js=0`):

```
session_state: persona=cowork budget=131072 window=262144 tokens=0 compactions=0
tools_view: 11 -> exec, read_file, write_file, write_xlsx, web_fetch, web_search,
             download_file, edit_file, ask_user, sheet_read, sheet_query
titlegen: enabled=true model=LiquidAI/LFM2.5-230M script_found=true python_found=true weights=true
compact -> refused: nothing to compact yet: this conversation has no running model process.
tree PSS with no model process up: 259.1 MB (app 89.4, WebKitNetwork 28.7, WebKitWeb 141.0)
```

The long run of spaces in the read-outside-the-workspace refusal — still open at the start of
this session — turned out to be another stale-`cwtool` artifact: the current tool host answers
"…which is outside the workspace /tmp/cw-smoke-ws. Use a path relative to the workspace
root." with one space, and the gate that saw the word salad was mounting an old binary.

Tests: engine 432+10, bridge 11+16, gateway 31, svelte-check 0, page harness green on pi core.

### And then the board itself was lying: `--bin` filters targets away from every package

The h18 board's ask_user red — a page that received `questionRequested` events still carrying
the raw `CWOFFEEQ {…}` payload — was not the bridge failing to parse. The app it launched was
**built at 01:56**, hours before the question work existed: `strings` finds no `CWOFFEEQ` in
`target/release/cwoffee-app`, while the `cwtool` next to it is from 09:34.

The cause was a build line I wrote this morning:

```
cargo build --release -p cwoffee-app -p cwoffee-bridge --bin cwtool
```

**A target filter applies to every selected package.** `--bin cwtool` selects "the bin named
cwtool" across the whole selection, and `cwoffee-app` has no such bin — so cargo builds the
bridge's tool host and **no binary of the app at all**, compiles the app's *dependencies* on
the way, prints `Finished`, exits 0, and says nothing about what it left unlinked. The
artifact on disk keeps whatever vintage last wrote it. `-vv` makes it plain: the log never
mentions `cwoffee-app` — no `Fresh`, no `Dirty`.

Two things were guessed wrong on the way and are worth keeping as warnings:

* The first theory was artifact collision: the upstream checkout defines packages with the
  same names, and I had been building the clone into *its* target dir. Plausible, and the new
  board refuses that setup anyway (it is a real trap) — but the same skip reproduced in this
  checkout's own `./target`, so the collision was not the cause.
* An mtime-only `touch` of a dependency also leaves the app alone, which is correct behaviour
  and made the bug look like cargo being lazy rather than cargo being told.

Fixes, in `scripts/selftest.sh`:

* The build line names the packages with **no target filter** (`-p cwoffee-app -p
  cwoffee-gateway -p cwoffee-bridge`), so every one of their binaries is a build target.
* **The proof is the artifact, not the exit code.** After building, any source newer than
  `target/release/cwoffee-app` means the build did not produce what the board would run: say
  which file, force one relink, re-check, and *die* if it is still behind. The same check runs
  for `cwtool` on the pi core, since the tools, their guardrails and their answer wording all
  live in that binary.
* The header now prints the artifact timestamps and the target dir the build used.

Consequence for the pi-core triage: the ask_user red and the cascading
"pi refused prompt: Agent is already processing" that followed it both came from grading code
this tree no longer has, and the h18 run's other numbers (`grown transcript` cadence included)
describe the 01:56 app. Nothing about the current tree's page-harness state is known until the
next board runs with the proof passing.
