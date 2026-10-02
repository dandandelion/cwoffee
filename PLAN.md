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

| | Milestone | Status | Acceptance |
|---|---|---|---|
| M0 | Skeleton: workspace, Tauri hello, Svelte+Tailwind wired, `Channel` proven with a fake streamer | **done** | a token stream paints in the webview with no jank |
| M1 | **Engine + TUI**: adapters, SSE, tool-call normalization, JSON repair, agent loop, append-only `PromptBuilder`, SQLite sessions, `exec` in bwrap, approvals, ratatui client | **done** | real BYOM tool-calling conversation from a terminal |
| M2 | **Chat surface**: sidebar, imperative transcript, markdown on stream end, tool cards, composer + pickers, session switching | **done** | the webview drives the real engine |
| M3 | **Doc tools**: xlsx writer, docx writer, LibreOffice→PDF | **done** — typst templates not started; image attachments now land in the composer | "spreadsheet + PDF report" works end to end |
| M4 | **Research**: `web_search` + `web_fetch` → readability → markdown | **done**, inline citations not started | cited multi-source answer |
| M5 | **Shell & polish**: custom titlebar, themes, shortcuts, packaging | **partly done** — RPM builds, shortcuts and external links done; themes pending | matches the mock; binary ~12 MB |

**Total ~6–8 weeks solo.** M1 and M2 are the ones that slip; guard them.

### M0 — DONE (2026-10-01), measured on the dev box

Frameless window + custom titlebar, Svelte 5 + Tailwind v4 wired, and a scripted
token stream delivered over `tauri::ipc::Channel<EngineEvent>` and painted with
plain-text appends batched on rAF.

| Gate | Result |
|---|---|
| GPU: renderD128 only, no NVIDIA | ✅ `cowry-app` holds **renderD128** only |
| Mesa pin actually applied | ✅ `[cowry] EGL vendor pinned to: .../50_mesa.json` |
| Memory (debug build, vite dev server) | **233.2 MB PSS** across 3 processes |
| — cowry-app host | 89.4 MB |
| — WebKitWebProcess | 108.9 MB |
| — WebKitNetworkProcess | 34.9 MB |
| RSS would have said | 438.3 MB — a **false failure** at the 400 MB gate |
| Page actually loaded | ✅ WebKitNetworkProcess has an ESTAB connection to vite:5173 |
| Engine unit tests | 6 passed |

Three processes, ~233 MB, debug profile. Qoder is 14 processes and ~700–850 MB
PSS. Release build + production bundle should land materially below 233 MB.

**Still needs human eyes:** whether the window visually painted correctly. Every
machine-checkable gate passed; a blank-webview failure would not show up in any
of them.

### M0 correction — the gates above were not sufficient

The user's screenshot showed a **stale vite error overlay covering the whole
window**. Process liveness, PSS, and the WebKit→vite socket all said "healthy"
while the app was visibly broken. So the M0 section above over-claimed.

Two real bugs were hiding behind silent failure:

1. **Empty `catch {}` blocks swallowed every `invoke` error.** A field-name
   mismatch made `render_report` fail on every call and nothing surfaced it.
2. **Tauri argument casing.** Command *argument names* are camelCase
   (`{ report }`), but *struct fields* keep serde defaults, so they are
   **snake_case on the wire** (`stream_el`, `body_bg`). Same rule as
   `EngineEvent`. Mixing these up yields `invalid args ... missing field`.

Fixing the class of problem, not the instance — a **page-side log that does not
depend on Tauri IPC**:

- `ui/src/lib/pagelog.ts` → `POST /__cowry_log`, a vite middleware that prints
  into the same stdout as the Rust side, so page and host output interleave
  chronologically. Dev-only; compiled out in prod (`import.meta.env.PROD`).
- `window.onerror` + `unhandledrejection` forward to it, so a blank page can
  never be silent again.
- vite middleware logs every module the webview requests — distinguishes "blank
  page" from "loaded main.ts then threw".
- `render_report` command: the UI reports what it actually rendered
  (titlebar/sidebar/footer/stream present, overlay absent, text length, computed
  bg, dpr) and Rust logs `render OK` or `render SUSPECT`.
- `ipc ✓/✗` badge in the footer — IPC health is never inferred from silence.
- `getCurrentWindow()` is resolved inside `try`; at component-init it would
  blank the entire page with no visible error.

### M0 final, measured

```
[cowry] render OK: titlebar=true sidebar=true footer=true stream=true
        overlay=false text=494 bg=rgb(253, 252, 249) dpr=2.00
[cowry/page] turn: end chars=1443 frames=209 dropped=0 worst=18ms decode=49.0/s
```

- **0 dropped frames** across 209 frames during a 1443-char stream; worst frame
  18 ms. The rAF-batched text-node append path is sound.
- Client-measured **49.0 tok/s** against a scripted 50 — the `LiveWindow`
  measurement is honest.
- Memory after a turn: **264.4 MB PSS** / 3 processes (RSS 470 MB).
- GPU: still `renderD128` only.

Dev harness: `VITE_AUTOTURN=1 ./scripts/dev.sh` streams a turn on launch, which
proves the whole `Channel → rAF → text node` path without anyone clicking.

### Config footgun found in M0

Tauri v2 resolves these two things **relative to different directories**:

| Key | Relative to |
|---|---|
| `build.frontendDist` | the **config file** (`crates/tauri-app/`) → `"../../ui/dist"` |
| `build.beforeDevCommand` / `beforeBuildCommand` | the **CWD of the CLI**, which for us is `crates/` → `"npm --prefix ../ui run dev"` |

Verified empirically (`beforeDevCommand: pwd` printed `.../cowry/crates`). Getting
this wrong produces a confusing `ENOENT .../Projects/ui/package.json`. `dev.sh`
always runs the CLI from `crates/tauri-app`, so the values above are stable.

### M1 + M2 — done, measured against the live model

Engine: `410b1d3 … 25be6a8`. Desktop wiring: `cb7ead0 … ea8f9d5`.

What the live runs proved, rather than what unit tests prove:

```
prefix cache      turn1 prompt=324 cached=0    raw_prefill=119/s
                  turn2 prompt=347 cached=319  eff_prefill=452/s (92% hit)
agent + tools     model emitted exec(ls -la) -> repaired args -> approval
                  round-trip -> bwrap -> result fed back -> second round-trip
fail closed       ask mode + nobody answers -> 12s timeout -> tool end ok=false
                  -> loop still received a result and finished the turn
documents         write_xlsx -> numeric cells 1200/300/90 confirmed in the package
                  write_file(.pdf) -> summary.pdf, read back with pdftotext
                  the model then ran `exec ls -l` to check its own output
memory            release desktop app 248.2 MB PSS total; TUI 6.8 MB PSS
gpu               renderD128 only, debug and release
packaging         target/release/bundle/rpm/cowry-0.0.0-1.x86_64.rpm (4.77 MiB)
```

Three bugs worth remembering because they were only visible at runtime:

- `SessionHost::spawn` calls `tokio::spawn`, which panics with "no reactor running"
  from bare `main()`. Tauri's async runtime *is* tokio, so the host is constructed
  inside `block_on`.
- The workspace defaulted to the process CWD, which under `tauri dev` is
  `crates/tauri-app`. A GUI app launched by a window manager inherits an arbitrary
  CWD; an agent silently editing the wrong tree is a bad class of failure.
- A TUI wrapping bug looped forever on a 0-width pane and reached **8.7 GB PSS**,
  pushing the box into swap mid-inference. `0x0` geometry is now refused at startup
  rather than defended at every call site.

The M0 lesson held: every gate below is a *render* or *round-trip* assertion, not a
liveness check. Process alive + memory in budget + socket open all passed while the
window was visibly showing a stale error overlay.


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
   — implemented and measured in M0: deltas are buffered into an array and flushed as **one** `appendChild(textNode)` per animation frame
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
| **GPU: NVIDIA has 44 MiB VRAM free; it's the default EGL vendor** | pin Mesa — see §12. Verify with `scripts/gpu-check.sh` on **day 1 of M0** |
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

## 12. GPU policy: Cowry renders on the Intel iGPU (mandatory on this box)

Measured on the dev box, 2026-10-01:

```
renderD128  pci=0000:00:02.0  driver=i915     <- Intel Iris Xe
renderD129  pci=0000:01:00.0  driver=nvidia   <- RTX 3080 Ti Laptop

connected panel:  card1-eDP-1  -> the display is driven by INTEL

NVIDIA VRAM:  16384 MiB total / 15993 MiB used / 44 MiB FREE

EGL vendors, in libglvnd preference order:
  10_nvidia.json   <- preferred by default
  50_mesa.json

Existing GTK apps (ptyxis, gnome-shell, gnome-software) hold BOTH render nodes.
```

### Why this is mandatory, not a preference

1. **NVIDIA has 44 MiB of VRAM free.** llama.cpp owns the rest. If WebKitGTK grabs an NVIDIA EGL context it is allocating into a nearly-full GPU — that produces allocation failures, blank webviews and crashes that will look like *our* bugs. This is almost certainly the real mechanism behind the generic "WebKitGTK + NVIDIA is broken" folklore on a box like this one.
2. **NVIDIA is the default EGL vendor** (`10_nvidia.json` sorts first), so we get NVIDIA unless we explicitly say otherwise. Existing GTK apps on this machine already hold renderD129 — proof the drift is real.
3. **The panel is on Intel** (`card1-eDP-1` connected). So app-on-Intel + scanout-on-Intel is **same-vendor DMABUF**, which is the configuration that avoids the cross-vendor DMABUF hazard entirely. Forcing Intel doesn't just free VRAM, it likely removes §12's original problem.
4. It keeps the whole 16 GB of VRAM for inference — which is the point of this machine.

Iris Xe is entirely sufficient for a 2D web compositor. There is no perf downside.

### The fix

Pin the EGL vendor to Mesa so libglvnd cannot hand us an NVIDIA context:

```bash
__EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/50_mesa.json
__GLX_VENDOR_LIBRARY_NAME=mesa          # only matters for the X11/GLX path
```

Both `libEGL_mesa.so.0` and `libEGL_nvidia.so.0` are present, so the pin resolves.

Note: `DRI_PRIME` is **not** the right lever — it's Mesa's PRIME offload selector for multi-Mesa setups and does nothing against the proprietary NVIDIA driver.

Apply it in **three** places, or the packaged app will break while the dev build works:
1. `crates/tauri-app/src/main.rs` — set at the very top of `main()`, **before** any GTK/EGL initialisation. (In edition 2024 `std::env::set_var` is `unsafe`; wrap it, or use a `#[ctor]`/constructor so it runs pre-`main`.)
2. the `.desktop` `Exec=` line (a wrapper script or `env VAR=... cowry`)
3. `scripts/dev.sh` for `tauri dev`

### Verification (M0 day 1, one command)

```bash
./scripts/gpu-check.sh cowry
```

Expected: cowry's processes hold **renderD128 only**. If renderD129 appears, the pin didn't take — fix that before writing any UI code.

### Fallbacks, in order
1. pin Mesa (above) — expected to be sufficient
2. `WEBKIT_DISABLE_DMABUF_RENDERER=1` (WebKitGTK 2.54 supports it)
3. `WEBKIT_DISABLE_COMPOSITING_MODE=1`
4. `GDK_BACKEND=x11` — works, costs Wayland niceties
5. `LIBGL_ALWAYS_SOFTWARE=1` — last resort, kills the perf story

There is also `webkit2gtk-nvidia-quirk` (crate, v2.2.0) if we ever must coexist with an NVIDIA context. With Mesa pinned we expect not to need it.

**Residual risk, stated honestly:** `gnome-shell` holds *both* render nodes, so Mutter may composite on NVIDIA while scanning out on Intel. If that turns out to matter, we'd be importing Intel buffers into an NVIDIA compositor — the reverse hazard. `gpu-check.sh` on `gnome-shell` plus a visual check at M0 day 1 settles it. We are not assuming our way past this.

---

## 13. Open questions

1. `dnf` devel packages need a password — see `SETUP.md` (blocks M0's Tauri build only; M1 is unaffected)
2. `typst` and `pandoc` not installed — `typst` we vendor as a crate, `pandoc` stays optional
3. Search backend for `web_search`: self-hosted SearXNG instance, or DuckDuckGo HTML endpoint?
4. License: if we want MCP/`rmcp` and Slint-free but GPL-adjacent deps later, decide now (Apache-2.0 assumed)
