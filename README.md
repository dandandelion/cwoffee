# Cowry

**A local-first, BYOM co-work agent for Linux. One process, ~300 MB, your model, your machine.**

Chat, tools, and research — Excel, PDF, docs, sandboxed command execution — against any
OpenAI-compatible, Anthropic-compatible, or custom endpoint.

```
Qoder:  ~1.3 GB across 14 processes, cloud
Cowry:  ~300 MB, one process, local
```

> Status: pre-implementation. Read [`PLAN.md`](PLAN.md) before touching code.

## Docs

| File | What |
|---|---|
| [`PLAN.md`](PLAN.md) | thesis, architecture, v1 scope, milestones, memory budget, perf rules, risks |
| [`SETUP.md`](SETUP.md) | what's installed, what still needs `sudo`, endpoint details |
| [`config/cowry.toml.example`](config/cowry.toml.example) | providers, models, context budget, sandbox policy |

## Layout

```
crates/engine      the product — agent loop, providers, tools, storage (no UI knowledge)
crates/tauri-app   thin desktop host (links the engine in-process)
crates/tui         ratatui client — the permanent dev/debug harness
ui/                Svelte 5 + Tailwind v4
scripts/           memory.sh (the PSS gate), bench.sh
```

## The three rules that make it fast

1. **Append-only, byte-stable prompts** — so the server's prefix cache always hits
2. **Virtualized message list** — ~15 messages in the DOM, history as HTML in SQLite
3. **Plain text while streaming, markdown on stream end** — never re-parse per token

## Name

A cowrie shell — one of the oldest currencies, i.e. *value exchanged between people*.
It keeps the **cow** of *cowork* without saying it.
