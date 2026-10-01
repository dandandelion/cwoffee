# Cowry

A local-first, BYOM co-work agent for Linux. Light, fast, and beautiful.

> **Thesis.** Qoder runs ~1.3 GB across 14 processes and talks to the cloud.
> Cowry runs ~300 MB in one process against *your* model on *your* machine.
> We are not a cheaper clone — we are the private, fast one.

---

## 1. Non-negotiables

These are architectural, not aspirational. Every one of them has a CI check or a unit test.

1. **Memory: PSS ≤ 400 MB hard ceiling**, ~250–350 MB steady state target, measured with `smaps_rollup` (PSS, not RSS — RSS double-counts shared pages and is not a defensible metric).
2. **Append-only, byte-stable prompt construction.** The prompt prefix must be byte-identical across turns so the server's prefix cache hits. Unit-tested. See §5.
3. **Virtualized message list.** ~15 messages in the DOM at any time; history lives as rendered HTML in SQLite. A 500-message session must not grow the DOM.
4. **No `invoke()` per token.** Streaming goes over `tauri::ipc::Channel` with rAF batching.
5. **The engine knows nothing about the UI.** `crates/engine` compiles without Tauri. The TUI and the desktop app are peers.

---

## 2. Stack lock

| Layer | Choice |
|---|---|
| Agent engine | Rust lib crate `crates/engine` (tokio) |
| Desktop shell | Tauri v2, links the engine **in-process** (one process, not two) |
| Webview | WebKitGTK 4.1 (already on the system) |
| Frontend | Svelte 5 + Tailwind v4 + Vite |
| Components | hand-rolled primitives; no component kits (they blow the memory budget) |
| State | Svelte runes |
| Storage | SQLite via `rusqlite` (bundled) + FTS5 |
| Markdown | `pulldown-cmark` in Rust → HTML → `DOMPurify` → DOM |
| Highlighting | `highlight.js`, ≤6 languages, on idle (not Shiki — grammar loading is a memory hog) |
| Excel | `rust_xlsxwriter` (write) + `calamine` (read) |
| PDF out | `typst` crate + bundled `.typ` templates |
| PDF in | `pdf-extract`, fallback to system `pdftotext` (present) |
| Exec sandbox | `tokio::process` + `portable-pty` inside `bwrap` (present) |
| Diff | `similar` |

---

## 3. Architecture

```
cowry/
  Cargo.toml                    # workspace
  crates/
    engine/                     # THE product. zero UI knowledge.
      provider/   mod.rs llama.rs openai.rs anthropic.rs
      agent/      loop.rs prompt.rs context.rs approvals.rs repair.rs
      tools/      exec.rs webfetch.rs websearch.rs fs.rs docs.rs
      store/      db.rs sessions.rs fts.rs
      docs/       typst.rs ooxml.rs templates/
      telemetry.rs              # TurnTelemetry / Live / Rolling
      ipc.rs                    # EngineEvent enum — shared by UI + TUI
    tauri-app/                  # thin host: commands + Channel plumbing
    tui/                        # ratatui client — permanent debug harness
  ui/                           # Svelte 5 + Vite
    src/lib/{chat,composer,sidebar,tools,telemetry}/
  assets/                       # icon, illustrations
  scripts/                      # memory.sh, bench.sh
```

Three binaries off one library: `cowry` (desktop), `cowry-tui` (dev + debug), and later `cowryd` (headless).

**Provider abstraction** — the most important 40 lines in the repo:

```rust
trait Provider {
    fn stream(&self, req: ChatRequest) -> BoxStream<'static, Result<Delta>>;
}

enum Delta {
    Text(String),
    Reasoning(String),          // llama.cpp `reasoning_content` — non-standard
    ToolCallBegin { id: String, name: String },
    ToolCallArgs(String),       // fragmented; accumulate then repair+parse
    Usage(TurnTelemetry),
    Done { finish: FinishReason },
}
```

Three adapters: `llama.cpp`, `openai`, `anthropic`. Normalization is the whole job — OpenAI's fragmented `tool_calls[].function.arguments` deltas and Anthropic's `content_block_start` / `input_json_delta` both collapse into `Delta`. Everything downstream only ever sees `Delta`.

---

## 4. Target endpoint (verified 2026-10-01, `http://127.0.0.1:8090`)

```
server:  llama.cpp llama-server
model:   qwen3.8-flash-next-iq3_s
n_ctx:   131072          images: true        api_key: false
slots:   1               ← no concurrency; engine must serialize turns
decode:  ~50 tok/s       prefill: ~29–68 tok/s
spec-decode: on          (draft 48, accepted 42 → 88%)
prefix cache: ACTIVE     (prompt 308 → cached 303)
```

Verified behaviours:
- ✅ streaming tool calls work; `arguments` arrive fragmented (`"{"`, `"\"expr\":"`, `"2"`, `"4"`…), `finish_reason: "tool_calls"`
- ✅ `reasoning_content` is emitted as a separate delta field (an off-the-shelf OpenAI client drops this)
- ✅ `chat_template_kwargs: {"enable_thinking": false}` works → real thinking toggle
- ✅ `/health` + `/props` expose `n_ctx`, `images`, `total_slots`, model alias

### The number that shapes everything: ~30–68 tok/s prefill

At 40 tok/s, a 100k-token context costs **~40 minutes to re-prefill**. Therefore:

- System prompt + tool schemas serialize **once**, never re-ordered (ordered structs, not `HashMap`).
- History is append-only. Editing a past message **forks** the session; it never mutates.
- Tool output is truncated **at ingest**, and that truncation is never recomputed differently later.
- Compaction is rare, explicit, and rewrites only the oldest prefix.

### Consequences of a 3-bit quant

`iq3_s` will occasionally emit malformed tool-call JSON. So:
- **max 4 tools in v1**, tiny schemas (a 15-tool surface would crater this model)
- tolerant **JSON repair** on `arguments` before parse (unterminated strings, trailing commas, single quotes)
- **one** automatic retry that feeds the parse error back as a tool result

### Consequences of `total_slots: 1`

Engine allows exactly one in-flight turn. Queued work shows a **prefill ETA** (`prompt_tokens / avg_prefill_tps`), never a bare spinner. No parallel tool execution in v1.

---

## 5. v1 scope

**In:**
- sidebar: sessions, workspaces, FTS search
- composer: model picker, thinking on/off, approval-mode picker, attach (text + image)
- streaming chat: reasoning block, markdown, code blocks + copy, collapsible tool-call cards with live output
- 4 tools: `exec` (bwrap), `web_fetch`, `web_search`, `write_file` (xlsx / pdf-typst / md / csv / html) + `read_file`
- approval modes: **Ask** / **Auto for session** / **Full auto** (denylist always enforced)
- telemetry bar (§7)
- frameless window + custom titlebar
- light + dark theme, keyboard shortcuts, RPM/Flatpak/AppImage packaging

**Out (deferred):** browser module · Knowledge Center / RAG · Automations · Sites · voice dictation · credits heatmap · mobile · MCP (`rmcp`) — but the tool registry is *shaped* like MCP so adding it later is additive, not a refactor.

---

## 6. Milestones

| | Milestone | Est. | Acceptance |
|---|---|---|---|
| M0 | Skeleton: workspace, Tauri hello, Svelte+Tailwind wired, `Channel` proven with a fake streamer | 2–3 d | a token stream paints in the webview with no jank |
| M1 | **Engine + TUI**: 3 adapters, SSE, tool-call normalization, JSON repair, agent loop, append-only `PromptBuilder` + cache-stability tests, SQLite sessions, `exec` in bwrap, approvals, ratatui client | 1.5–2 w | real BYOM tool-calling conversation from a terminal |
| M2 | **Chat surface**: sidebar, virtualized list, streaming markdown, tool cards, composer + pickers, mid-stream session switching | 2 w | 40% of perceived quality — do not rush |
| M3 | **Doc tools**: typst→PDF (3 templates), xlsx writer, attachment chips, in-app preview | 1–1.5 w | "spreadsheet + PDF report" works end to end |
| M4 | **Research**: `web_search` + `web_fetch` → readability → markdown, inline citations | 3–4 d | cited multi-source answer |
| M5 | **Shell & polish**: custom titlebar, pastel sidebar, themes, shortcuts, packaging, release profile | 1–1.5 w | matches the mock; `cowry` binary ~10 MB |

**Total ~6–8 weeks solo.** M1 and M2 are the ones that slip; guard them.

---

## 7. Telemetry bar

llama.cpp gives **per-request aggregates only** (`timings`), so "current" is measured client-side.

```rust
struct TurnTelemetry {           // per turn, from server `timings` + client timestamps
    prompt_tokens: usize, cached_tokens: usize, completion_tokens: usize,
    ttft_ms: u64,                // request sent → first delta
    server_prefill_tps: f64,     // timings.prompt_per_second
    server_decode_tps: f64,      // timings.predicted_per_second
    draft_n: usize, draft_accepted: usize,
}
struct Live {                    // sliding 1s window over arriving deltas
    inst_decode_tps: f64, inst_prefill_tps: f64,
}
struct Rolling {                 // mean of LAST 10 turns, not lifetime
    avg_decode_tps: f64, avg_prefill_tps: f64, avg_cache_hit_pct: f64,
}
```

**Prefill subtlety — two numbers, never merged:**
- **effective prefill** = `prompt_tokens / ttft` → what the user experiences; huge on cache hits
- **raw prefill** = `timings.prompt_per_second` → true compute rate; the number that warns you before a bad turn

Showing only the effective number is accidentally dishonest and local-model users will catch it immediately.

**Bar layout:**
```
⚡ cache 98% (303/308) · prefill 68 t/s (avg 61) · decode 52 t/s (avg 47) · spec 88% · ctx 4.1k/131k
```
Click → popover with per-turn history, sparklines, and a cold/warm split. Turn 1 of a session is excluded from averages (cold cache).

---

## 8. Memory budget

| | naive RSS sum | PSS (the metric we use) |
|---|---|---|
| Qoder (measured, 14 procs) | ~1,333 MB | ~700–850 MB (est.) |
| **Cowry target** | ~300 MB | **250–350 MB** |
| CI failure threshold | 450 MB | **400 MB** |

Breakdown: WebKitGTK web process 140–240 MB · Tauri host + engine 60–110 MB.
The win is structural: **one web process instead of fourteen Chromium processes.**

**Calibration (measured on this box, 2026-10-01):** Firefox showed **97.9 MB RSS but 24.3 MB PSS** — a 4× inflation from shared pages. This is why we gate on PSS, and it supports the ~700–850 MB PSS estimate for Qoder's 1,333 MB RSS sum. Never quote the RSS number in public; it's indefensible.

`scripts/memory.sh` samples PSS at a fixed scenario (200-message session, 3 attachments) and fails CI over 400 MB.

```bash
for p in $(pgrep -f cowry); do
  awk '/^Pss:/{s+=$2} END{print s}' /proc/$p/smaps_rollup
done | paste -sd+ | bc
```

---

## 9. Perf rules (Tauri/WebKit specific)

1. Virtualize the chat list — biggest lever by far
2. `Channel<T>` + rAF batching; **plain text while streaming, full markdown on stream end** (re-parsing markdown per token is the classic stutter)
3. Persist rendered HTML in SQLite → reopening a session is instant
4. One webview, no iframes, no devtools in release
5. Heatmap = one `<canvas>`, not 371 divs; lazy-load everything off the composer
6. Subset fonts (~200 KB), inline critical CSS, JS bundle < 250 KB
7. `WEBKIT_DISABLE_COMPOSITING_MODE=1` if GPU memory creeps on this driver
   — **this box is the risky case, see §10 "NVIDIA + Wayland"**
8. Hide rather than destroy the webview on window close → instant reopen
9. Release profile: `opt-level=3`, `lto=true`, `strip=true`, `panic="abort"`, `codegen-units=1`

---

## 10. Risks & kill criteria

| Risk | Mitigation / kill criterion |
|---|---|
| **NVIDIA + Wayland + WebKitGTK** (see below) | test on **day 1 of M0**, before anything else |
| Memory blows up on long sessions | virtualization; **if a 200-message session > 400 MB PSS, fix before adding features** |
| Native-UI fidelity gap | we chose Tauri precisely to avoid this; **if the composer isn't at mock fidelity within 3 days of starting M2, our component structure is wrong, not Tauri** |
| DOCX generation (weakest Rust area) | `docx-rs` is immature → generate OOXML with `zip`+`quick-xml`, or shell to `pandoc` if present. **Decide in M3, not later.** |
| Small quant breaks tool calling | repair + 1 retry; keep tool count at 4 |
| 131k ctx + slow prefill | context budget <8k typical; aggressive ingest truncation; visible context meter |
| WebKitGTK on this GPU driver | measure in M0, not M5 |

---

## 11. Brand

**Cowry** — keeps the "cow-" of *cowork*; a cowrie shell is one of the oldest currencies, i.e. *value exchanged between people*. Ownable, short, `cowry`/`cow` CLI-friendly.

**Icon:** hand-drawn watercolor **cowrie shell with a small face** — same warmth as Qoder's duck, but it's *our* object. Rounded shell, fine hatched ridge, two dot eyes, faint smile, wet-on-wet blue wash behind it (echoes the mock's illustration). Used at 96px in the hero and 16px in the titlebar — so it must survive both, which means the face has to read at 16px.

**Voice:** plain, unhurried, no "magic"/"supercharge" language. Numbers over adjectives.

---

## 12. Known hazard on the dev box: NVIDIA + Wayland + WebKitGTK

Verified hardware: **Intel Iris Xe + NVIDIA RTX 3080 Ti Laptop (proprietary driver, `/dev/nvidia0`), Wayland session.**

This is the single most common way a Tauri app dies on Linux: WebKitGTK's **DMABUF renderer** misbehaves with the proprietary NVIDIA driver under Wayland, producing a blank or flickering webview, or a crash on startup. It is a rendering-layer bug, not our bug, and it will look like our bug.

Mitigations, in the order to try (all are env vars, no code changes):

```bash
WEBKIT_DISABLE_DMABUF_RENDERER=1     # newest WebKitGTK (we have 2.54.0) - try first
WEBKIT_DISABLE_COMPOSITING_MODE=1    # the classic fallback
LIBGL_ALWAYS_SOFTWARE=1              # last resort; costs our perf story
```

There is also a purpose-built crate, **`webkit2gtk-nvidia-quirk`** (v2.2.0), which patches the DMABUF renderer quirk. If the env vars are flaky, that's the durable fix.

Set the var in the `.desktop` file's `Exec=` line and in `tauri.conf.json`'s env, not just the shell — otherwise the packaged app breaks while the dev build works.

**Fallback:** force X11 (`WAYLAND_DISPLAY=` unset / `GDK_BACKEND=x11`) — works, costs Wayland-specific niceties. Decide in M0, not later.

**M0 day-1 task:** render a solid-colour webview window on this box. If it's blank, we solve it before writing a line of product code.

---

## 13. Open questions

1. `dnf` devel packages need a password — see `SETUP.md` (blocks M0's Tauri build only; M1 is unaffected)
2. `typst` and `pandoc` not installed — `typst` we vendor as a crate, `pandoc` stays optional
3. Search backend for `web_search`: self-hosted SearXNG instance, or DuckDuckGo HTML endpoint?
4. License: if we want MCP/`rmcp` and Slint-free but GPL-adjacent deps later, decide now (Apache-2.0 assumed)
