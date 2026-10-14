# Spike: pi as cwoffee's core, proven on this machine

Nothing here is production code. It exists to answer, with evidence rather than opinion:

> Can `pi` be the core of cwoffee — our UI, our endpoint, our tools, our approvals?

**Answer: yes.** Every pillar below was run, on 2026-10-14, against pi **1.2.0**.

| # | Question | Result |
|---|---|---|
| S0.1 | Does pi drive **our** scripted endpoint? | ✅ `scripts/mock-llm.mjs` + `tests/scenarios/scenarios.json`, **unmodified**. Full lifecycle: `prompt → disposition:"started" → agent_start → turn_start → text_delta×13 → text_end → message_end → turn_end → agent_end → agent_settled` |
| S0.2 | Do **our tools stay Rust**? | ✅ our `read-file-approval` scenario emitted `read_file{path:"/etc/hostname"}` → our extension → **Rust binary** → `{"ok":true,"engine":"rust"}`, `durationMs:405`. Guardrail (refuse non-`/etc`, refuse `..`) lives in the Rust |
| S0.3 | Does the **approval round trip** survive? | ✅ `extension_ui_request{method:"confirm",timeout:15000}` reached the client → `extension_ui_response{confirmed:true}` → tool ran. pi auto-denies on timeout ⇒ **fail closed**, like `ApprovalRequested` today |
| S0.4 | Does our **prefix-cache telemetry** survive? | ✅ mock's 92% hit surfaced as `usage {in:95, cacheRead:1081}`. pi reads `prompt_tokens_details.cached_tokens` **and** llama.cpp's `prompt_cache_hit_tokens` |
| S0.5 | `reasoning_content` / thinking? | ✅ `thinking_delta` streamed (9 fragments) |
| S0.6 | What does it **cost**? | 3 runs: **idle 72.5–74.6 MB · peak 89.6–92.6 MB · settled 73.3–76.9 MB PSS** (one outlier peaked at 110.3). RPC usable **176–311 ms** after spawn |

## Run it

```bash
# 0. deps: pi 1.2.0 on PATH, node ≥22.19, cargo/rustc only for step 1
cd <this repo>            # pi-cwoffee

# 1. build the Rust tool (links cwoffee's own cached serde_json)
./spike/build-shim.sh               # -> spike/cwshim

# 2. start OUR scripted endpoint, from the cwoffee repo, untouched
cd ../cwoffee && node scripts/mock-llm.mjs --port 8190      # leave running

# 3. isolated pi config pointing at it (writes nothing to ~/.pi)
cp spike/models.json /tmp/pi-probe/models.json    # see PI_CODING_AGENT_DIR below

# 4. drive a turn: streams, approval auto-answered, PSS sampled
node spike/drive-rpc.mjs
```

`drive-rpc.mjs` env overrides: `PI_BIN`, `PI_CODING_AGENT_DIR` (default `/tmp/pi-probe`),
`PI_WORKDIR` (default `/tmp/pi-ws`), `PROVIDER`/`MODEL` (default `cwoffee-mock`),
`PROMPT` (default the `read-file-approval` scenario text), `SPAWN_EXTENSION` (default on).

It writes the two pi dirs itself if missing, so step 3 is normally unnecessary.

## The interesting frames it prints

```
← READY      176–311ms after spawn (model cwoffee-mock, ctx 128000)
← APPROVAL  confirm {"title":"Allow read_file?","message":"/etc/hostname","timeout":15000}
→ APPROVAL  {"confirmed":true}                         (400 ms later, like a human would)
← TOOL START read_file {"path":"/etc/hostname"}
← TOOL END   read_file isError=false dur=404ms
             [{"type":"text","text":"{\"ok\":true,\"tool\":\"read_file\",\"engine\":\"rust\"…}"}]
usage: {in:1147, cacheRead:0,    out:21}     # turn 1 — nothing cached yet
usage: {in:95,   cacheRead:1081, out:8}      # turn 2 — 92% served from cache
```

`durationMs:404` is mostly the 400 ms approval delay this fake UI inserts; the Rust shim
itself is single-digit milliseconds.

## Reading of the result

- **Cheap because of the test fixture.** The self-test harness we already trust
  (`mock-llm.mjs`, which reproduces llama.cpp's wire shape: split `tool_calls` fragments,
  `reasoning_content`, usage+`timings`) re-targets the *new* core for free. Phase 0 needed an
  hour, not a quarter.
- **The extension is a pipe, not a port.** ~30 lines: TypeBox arg schema → spawn the Rust
  binary → return its JSON envelope. Tool logic and guardrails never move to TS.
- **The cost is memory, honestly stated:** +~90 MB of core (settled ~75, worst observed 110)
  against a 248.2 MB app and a 400 MB ceiling. `PI-CORE-PLAN.md` → MEMORY BUDGET.
- **What broke in our own assumptions, not in pi:** no `cwd` change mid-session (workspace
  switch ⇒ process pool), no session-identity notion (our refusal moves to the bridge), no
  `ttft` in `usage` (our live bar becomes bridge-synthesised), and `usage.input` excludes
  cached tokens (the context bar's math changes).

## Files

| File | What |
|---|---|
| `pi-extension-cwoffee-tools.ts` | the pi extension: registers `read_file`, asks the client for approval, spawns the Rust shim |
| `rust-shim.rs` | the Rust tool: arg validation + guardrails + JSON envelope. Built by `build-shim.sh` |
| `build-shim.sh` | compiles the shim with `~/.cargo/bin/rustc`, linking cwoffee's cached `serde_json` rlib |
| `drive-rpc.mjs` | the RPC client: spawns `pi --mode rpc`, speaks JSONL, auto-answers approvals, samples PSS from `smaps_rollup` |
| `models.json` | pi provider config for the scripted endpoint (`api:"openai-completions"`) |

## Known shortcuts in the spike (do not ship these)

1. `node_modules` is **symlinked** into the agent dir so `import { Type } from "typebox"`
   resolves. Real packaging is PI-CORE-PLAN S2.5.
2. The shim path is hardcoded (`/tmp/cwshim` or `CWOFFEE_SHIM`); production tools are named
   binaries, not a single probe.
3. The driver auto-approves everything after 400 ms — a fake UI, not a policy.
4. `--no-session`: no session files were exercised. Phase 3 (JSONL + our FTS index) is unproven.
