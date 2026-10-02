# Build, run, and the traps in between

Measured on this box (Fedora, Intel iGPU + NVIDIA, llama.cpp on :8090).

## Dev loop

```bash
./scripts/dev.sh                       # vite + tauri dev, GPU policy applied
VITE_AUTOTURN=1 ./scripts/dev.sh       # streams a turn on launch, no clicking
VITE_AUTOTURN_PROMPT='...' ./scripts/dev.sh
VITE_SELFTEST=1 ./scripts/dev.sh       # exercises session create/rename/delete/search end-to-end
```

`dev.sh` is the only supported way to run the dev build. It exports the Mesa pin
before anything touches EGL; running `tauri dev` by hand gets an NVIDIA context.

## Release

```bash
cargo build --release -p cowry-app -p cowry-tui
./target/release/cowry-app
./target/release/cowry --url http://127.0.0.1:8090/v1

# package
cd ui && npx tauri build --bundles rpm
# -> target/release/bundle/rpm/cowry-0.0.0-1.x86_64.rpm
```

`--bundles`, not `--bundle`. `dpkg-deb` and `appimagetool` are not installed here,
so rpm is the only bundle that works on this box.

## Trap 1 — a debug binary run by itself shows a blank window

A **debug** build loads `build.devUrl` (`http://localhost:5173`). Run
`./target/debug/cowry-app` with no vite server and the window is blank, with no
error anywhere: the engine logs look healthy, WebKit processes are up, memory is
fine, and nothing paints.

```
[cowry] engine up: url=... model=... workspace=...     <-- looks alive
(no "render OK" line at all)                            <-- this is the tell
```

The render self-report is what makes this visible. `render OK` / `render SUSPECT`
is printed by Rust after the page reports its own DOM state; **silence means the
page never ran**, which process liveness, PSS and socket checks all fail to catch.

Use the release binary, or run vite alongside it. Do not conclude "it works" from
a debug binary that printed no render line.

## Trap 2 — frontend changes do not reliably reach the binary

Assets are embedded by `build.rs` (`tauri_build::build()`). `cargo build -p
cowry-app` after a `vite build` can finish in 0.25s and keep the old asset set.
`tauri dev` and `tauri build` handle this correctly; a bare `cargo build` may not.

If embedded assets look stale: `touch crates/tauri-app/build.rs && cargo build -p
cowry-app`.

Related: `ui/dist/index.html` has been observed missing while `dist/assets/` was
present. Check `ls ui/dist` before trusting a build.

## Trap 3 — Tauri argument casing

Command **argument names** are camelCase on the wire; **struct fields** keep their
serde names. Getting this wrong is a runtime error, not a compile error:

```
invalid args `report` for command `render_report`: missing field `stream_el`
```

## Trap 4 — the engine needs a tokio reactor

`SessionHost::spawn` calls `tokio::spawn`. From bare `main` it panics:

```
there is no reactor running, must be called from the context of a Tokio 1.x runtime
```

Construct it inside Tauri's runtime:

```rust
let (mut host, adopted) = tauri::async_runtime::block_on(async { ... });
```

## Trap 5 — killing test processes

```bash
pkill -f 'target/debug/cowry-app'   # matches this shell's own cmdline; kills the shell (143)
```

Kill by exact name (`pkill -x cowry-app`) or by PID from `ps`. Likewise
`pkill -f mock_llm.py`.

## Trap 6 — memory probes

Gate on **PSS**, and watch the whole descendant tree. RSS double-counts shared
pages: the app is 248 MB PSS / 419 MB RSS, so an RSS gate would fail a build that
is actually inside budget. Matching only the main binary under-reports by ~60%
because `WebKitWebProcess` and `WebKitNetworkProcess` are separate processes.

For anything that might allocate, use a PSS watchdog that SIGKILLs on threshold.
`RLIMIT_AS` is the wrong instrument: tokio/reqwest binaries legitimately map huge
VmSize at small PSS, and the limit produces allocation failures that are not real.

```bash
./scripts/memory.sh cowry-app 400
./scripts/gpu-check.sh cowry-app
./scripts/sandbox-probe.sh
```

## Trap 7 — the TUI needs a real terminal

A pty with no winsize reports `0x0`. The TUI refuses it rather than rendering:

```
Error: terminal reports 0x0 - Cowry needs a real terminal size.
```

This is not cosmetic. An unbounded wrap loop at width 0 once grew the process to
8.7 GB PSS and pushed this box into swap while the inference server was running.
