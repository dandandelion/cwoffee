# Benchmarks

Every number here is measured on the dev box, not estimated. Reproduce with the
commands at the bottom; if a number regresses, update it here or explain why in
the commit that changed it.

## Memory budget

Target ~250–350 MB PSS, hard CI ceiling 400 MB, overall envelope ~500 MB.

**PSS, not RSS.** RSS double-counts shared pages: Firefox measured 97.9 MB RSS
against 24.3 MB PSS on this box. Gating on RSS would fail Cowry at 475 MB for a
process set that really occupies 248 MB.

### Desktop app (`cowry-app`), release build, embedded assets

| Process | PSS |
|---|---|
| `cowry-app` (engine + Tauri + GTK) | 105.6 MB |
| `WebKitWebProcess` | 102.1 MB |
| `WebKitNetworkProcess` | 40.6 MB |
| **total** | **248.2 MB** |

RSS for the same state is 419.4 MB — 69% higher, and the reason RSS is not the
gate.

Debug build + vite dev server measures 289.9 MB PSS. That is the number to
compare against when developing, not the shipping number.

### TUI (`cowry`), release build

**6.8 MB PSS**, flat over 10 s across a completed streamed turn.

This is the argument for the engine/UI split: the webview costs ~240 MB and the
agent costs ~7 MB. Anything that does not need a browser should not carry one.

### Binary size

| Binary | Size |
|---|---|
| `target/release/cowry` | 5.8 MB |
| `target/release/cowry-app` | 9.3 MB |

## Streaming smoothness

Measured in the webview with the dev harness, real model at ~50 tok/s:

```
turn end chars=1443 frames=209 dropped=0 worst=18ms
```

266–1418 frames per turn observed across runs, 0–3 dropped, worst frame 17–43 ms
at DPR 2.0. The 3-dropped case is a tool card being inserted mid-turn, which
changes layout; that is expected and bounded.

## Prefix caching (why the prompt builder is append-only)

Two turns against `qwen3.8-flash-next-iq3_s`, second turn reuses the first's
prefix:

```
turn1: prompt=324 cached=0   raw_prefill=119/s decode=20.5/s spec=40/75
turn2: prompt=347 cached=319 (92% hit) raw_prefill=38/s ttft=767ms eff_prefill=452/s
```

Raw prefill on this model is 20–120 tok/s. A cache miss is the difference between
a 0.8 s and a 9 s first token on a long transcript, which is why:

- the system prompt contains no volatile data (no clock, no cwd, no git status),
- volatile context is appended to the newest user message,
- history is append-only and byte-stable,
- tool definitions are registered in a fixed order,
- compaction is explicit, rare, counted, and surfaced in the UI.

## GPU

Cowry must render on the Intel iGPU (`renderD128`). The NVIDIA GPU is at ~15993 /
16384 MiB from local inference and its EGL vendor sorts first by default.

`scripts/gpu-check.sh cowry-app` must show Cowry processes holding `renderD128`
only. Verified for both debug and release builds.

## How to reproduce

```bash
# gates, against a running app
./scripts/gpu-check.sh cowry-app
./scripts/memory.sh cowry-app 400      # walks the whole descendant tree

# streaming + engine path, driven from a script
VITE_AUTOTURN=1 VITE_AUTOTURN_PROMPT='Say hello in five words.' ./scripts/dev.sh

# live provider smoke test
cargo run -q -p cowry-engine --example live
cargo run -q -p cowry-engine --example live_agent -- http://127.0.0.1:8090/v1

# sandbox isolation (re-run after any bubblewrap / kernel / distro upgrade)
./scripts/sandbox-probe.sh

# markdown sanitizer
(cd ui && node --experimental-strip-types scripts/md_test.ts)
```

## Safety note on probing

This box runs inference and has limited headroom. Any probe that spawns a process
must be watchdog-armed against **PSS**, not `RLIMIT_AS`. `RLIMIT_AS` caps virtual
address space; tokio/reqwest binaries legitimately map large VmSize at small PSS,
and the result is a misleading "memory allocation failed" that looks like an app
bug. A TUI regression once reached 8.7 GB PSS and pushed the machine into swap.
