# Cwoffee

*Cwoffee — a coworker that happens to run on your machine. Same warm palette in
light and dark, now with a cup instead of a cow.*

**A local-first, BYOM co-work agent for Linux. One process tree, ~230–285 MB, your model, your machine.**

Chat, tools, and research — Excel, PDF, docs, image input, sandboxed command
execution — against any OpenAI-compatible, Anthropic-compatible, or custom endpoint.

```
Qoder:  ~1.3 GB across 14 processes, cloud
Cwoffee:  230–285 MB PSS, one process tree, local
```

The memory figure is measured, not estimated, on the production build with the
frontend embedded. It is quoted as a range because the total moves with which WebKit
helpers are alive: 87.6–106.7 MB for the app (engine + Tauri + GTK), 114.7–129.5 MB
WebKit web process, 27.5–38.1 MB WebKit network process, plus 8 MB of WebKit's own
sandboxed image loaders in the heavier sample. The same code without a browser — the
TUI — is **7.0 MB PSS**. See [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

> Status: M0–M2 built and running. The engine, both clients, the sandbox, the store,
> document output and image attachments are live and verified against a real local
> model. Packaging produces an installable RPM. Not yet done: Knowledge Center,
> browser module, LAN gateway (designed in `docs/GATEWAY.md`, deliberately not built).

## Run it

```bash
# desktop app against a local llama.cpp server
CWOFFEE_URL=http://127.0.0.1:8090/v1 ./scripts/dev.sh

# terminal client (6.8 MB, same engine)
./target/debug/cwoffee --url http://127.0.0.1:8090/v1 --model qwen3.8-flash-next-iq3_s

# config file instead of env vars: ~/.config/cwoffee/config.toml
cwoffee --config /path/to/config.toml
```

Precedence is **CLI flags > environment > config file > defaults**.

Sessions are created lazily: "new session" is a blank view, not a blank row. The
row appears when you send the first message, already titled by it.

Keyboard: `Ctrl+K` composer, `Ctrl+Enter` send, `Ctrl+N` new session, `Ctrl+B`
sidebar, `Alt+1-9` switch session. The `?` in the status bar lists them. Light and
dark themes; the choice persists, and the default follows the OS.

Images go in by picker, paste, or drag — same guardrails whichever way (mime
allow-list, count and byte caps, enforced by the engine rather than the UI).

Gates:

```bash
./scripts/memory.sh cwoffee-app 400      # PSS, includes WebKit children
./scripts/gpu-check.sh cwoffee-app       # must hold renderD128 only
./scripts/sandbox-probe.sh             # re-run after any bwrap/kernel upgrade
```

## Docs

| File | What |
|---|---|
| [`PLAN.md`](PLAN.md) | thesis, architecture, v1 scope, milestones, memory budget, perf rules, risks |
| [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) | every measured number and the command that reproduces it |
| [`SETUP.md`](SETUP.md) | what's installed, what still needs `sudo`, endpoint details |
| [`config/cwoffee.toml.example`](config/cwoffee.toml.example) | providers, models, context budget, sandbox policy |

## Layout

```
crates/engine      the product — agent loop, providers, tools, storage (no UI knowledge)
crates/tauri-app   thin desktop host (links the engine in-process)
crates/tui         ratatui client — the permanent dev/debug harness
ui/                Svelte 5 + Tailwind v4
scripts/           memory.sh (the PSS gate), gpu-check.sh, sandbox-probe.sh, check-docx.py
```

The engine has no idea a UI exists. Both clients drive one `SessionHost` over
channels, which is also the seam for a future daemon/remote mode.

## The three rules that make it fast

1. **Append-only, byte-stable prompts** — so the server's prefix cache always hits.
   Measured: 92% cache hit on turn two, 452 tok/s effective prefill against 38 tok/s raw.
2. **Imperative transcript, never a reactive list** — deltas become text nodes
   batched once per animation frame. 0 dropped frames over 209 frames at 50 tok/s.
3. **Plain text while streaming, markdown on stream end** — never re-parse per token.

## Two things that are security, not polish

- **`exec` runs inside `bwrap`** — read-only root, workspace-only writes, no network,
  PID namespace, deadline enforced inside the namespace. The denylist is a guardrail;
  the sandbox is the boundary, and `sandbox-probe.sh` proves it rather than trusting
  the flags.
- **Document writers escape before they emit any markup.** Model output quotes fetched
  web pages, so `<script>` and `javascript:` arrive in the text. A writer that passes
  them through is an injection path into a file the user opens in Word.
- **Links leave the webview, and attachments are refused at the engine boundary.**
  Rendered markdown links go to the OS through tauri's opener plugin (scope-checked),
  never by navigating the app window. Image attachments are validated by the engine —
  mime allow-list, count and byte caps, base64 shape — not by the composer, because
  the composer is not going to be the only client forever.

## Name

**Cwoffee** — *cowork* with the coffee in it. A co-work agent you drink with:
it sits at your desk, it uses your model, and the palette is a flat white in
light mode and an espresso in dark.

It used to be **Cowry** (a cowrie shell, one of the oldest currencies — value
exchanged between people, and the *cow* of *cowork*). The rename is cosmetic in
intent but real in the filesystem, so it migrates rather than orphans anything:

| Was | Now |
|---|---|
| `~/.config/cowry/` | `~/.config/cwoffee/` |
| `~/.local/share/cowry/sessions.db` | `~/.local/share/cwoffee/sessions.db` |
| `~/cowry-workspace` | `~/cwoffee-workspace` (yours was moved, not recreated) |
| `COWRY_URL`, `COWRY_MODEL`, … | `CWOFFEE_URL`, … — the old names still work |
| `cowry`, `cowry-app` binaries | `cwoffee`, `cwoffee-app` |
| localStorage `cowry.theme.v1` | `cwoffee.theme.v1` — old keys are read and re-saved |

The data migration runs at startup and is conservative: if both directories
exist it moves the legacy database only when the new one is empty, and keeps the
copy it displaced aside as `*.pre-cwoffee-<timestamp>` instead of deleting it.
Anything it will not decide for itself, it says out loud in the transcript
rather than in a log file you would never open.
