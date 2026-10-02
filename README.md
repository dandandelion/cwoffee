# Cowry

**A local-first, BYOM co-work agent for Linux. One process, 248 MB, your model, your machine.**

Chat, tools, and research — Excel, PDF, docs, sandboxed command execution — against any
OpenAI-compatible, Anthropic-compatible, or custom endpoint.

```
Qoder:  ~1.3 GB across 14 processes, cloud
Cowry:  248 MB PSS, one process tree, local
```

The 248 MB is measured, not estimated: 105.6 MB for the app (engine + Tauri + GTK),
102.1 MB WebKit web process, 40.6 MB WebKit network process. The same code without a
browser — the TUI — is **6.8 MB PSS**. See [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

> Status: M0–M2 built and running. The engine, both clients, the sandbox, the store
> and document output are live and verified against a real local model. Packaging
> produces an installable RPM. Not yet done: attachments, Knowledge Center, browser
> module.

## Run it

```bash
# desktop app against a local llama.cpp server
COWRY_URL=http://127.0.0.1:8090/v1 ./scripts/dev.sh

# terminal client (6.8 MB, same engine)
./target/debug/cowry --url http://127.0.0.1:8090/v1 --model qwen3.8-flash-next-iq3_s

# config file instead of env vars: ~/.config/cowry/config.toml
cowry --config /path/to/config.toml
```

Precedence is **CLI flags > environment > config file > defaults**.

Gates:

```bash
./scripts/memory.sh cowry-app 400      # PSS, includes WebKit children
./scripts/gpu-check.sh cowry-app       # must hold renderD128 only
./scripts/sandbox-probe.sh             # re-run after any bwrap/kernel upgrade
```

## Docs

| File | What |
|---|---|
| [`PLAN.md`](PLAN.md) | thesis, architecture, v1 scope, milestones, memory budget, perf rules, risks |
| [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) | every measured number and the command that reproduces it |
| [`SETUP.md`](SETUP.md) | what's installed, what still needs `sudo`, endpoint details |
| [`config/cowry.toml.example`](config/cowry.toml.example) | providers, models, context budget, sandbox policy |

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

## Name

A cowrie shell — one of the oldest currencies, i.e. *value exchanged between people*.
It keeps the **cow** of *cowork* without saying it.
