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
cargo build --release -p cwoffee-app -p cwoffee-tui
./target/release/cwoffee-app
./target/release/cwoffee --url http://127.0.0.1:8090/v1

# package
cd ui && npx tauri build --bundles rpm
# -> target/release/bundle/rpm/cwoffee-0.0.0-1.x86_64.rpm
```

`--bundles`, not `--bundle`. `dpkg-deb` and `appimagetool` are not installed here,
so rpm is the only bundle that works on this box.

## Trap 1 — `cargo build --release` does not produce a release app

This one cost the most time and invalidated a published benchmark.

Tauri's own build script computes `dev = !custom_protocol`, where `custom_protocol`
is the **cargo feature** `tauri/custom-protocol`. The Tauri CLI enables it during
`tauri build`; a bare `cargo build --release` does not. So unless the app crate
declares the feature, `cfg(dev)` is on in the "release" binary, which means:

- the app loads `build.devUrl` (`http://localhost:5173`) instead of embedded assets,
- **no assets are embedded at all**,
- run with no vite server, the window is blank and the page's JS never executes —
  no error, no log, engine healthy, WebKit up, memory fine.

The app crate must declare it, and it must be on by default:

```toml
[features]
default = ["custom-protocol"]
custom-protocol = ["tauri/custom-protocol"]
```

`tauri dev` re-runs cargo with `--no-default-features`, so the dev loop is
unaffected. Verify the feature actually took effect before trusting any release
measurement:

```bash
# must print nothing
grep -c 'rustc-cfg=dev' $(ls -dt target/release/build/cwoffee-app-*/ | head -1)/output
# must list the hashed filenames from ui/dist/assets
strings target/release/cwoffee-app | grep -oE 'index-[A-Za-z0-9_-]{8}\.(js|css)' | sort -u
```

## Trap 2 — a blank webview is invisible from the outside

Every conventional health signal says "fine": the process is alive, PSS is
normal, WebKit child processes exist, the GPU node is held. The only outward
tell is a **missing** `render OK` line.

Two mechanisms make it visible, and both are now in the code:

- `boot_log` — the page's first lines of JS report into the Rust log, in release
  as well as dev. If `boot: main.ts executing` never appears, the page never ran.
- a startup watchdog — if no `render_report` arrives within 8 s the app logs
  `SUSPECT: no render report after 8s - the window is probably blank`.

```bash
# healthy production run, no vite needed
[cwoffee] webview attached: page JS is running
[cwoffee/page] boot: main.ts executing
[cwoffee/page] boot: mount() returned
[cwoffee] render OK: titlebar=true sidebar=true footer=true stream=true overlay=false text=1280 bg=rgb(253, 252, 249) dpr=2.00
```

## Trap 3 — embedded assets can go stale

Assets are embedded by `build.rs` (`tauri_build::build()`). After a `vite build`,
a `cargo build -p cwoffee-app` can finish in 0.25s and keep the previous asset set.
If embedded assets look stale: `touch crates/tauri-app/build.rs && cargo build -p
cwoffee-app`. Also check `ls ui/dist` — `index.html` has been found missing while
`dist/assets/` was present.

## Trap 4 — Tauri argument casing

Command **argument names** are camelCase on the wire; **struct fields** keep their
serde names. Getting this wrong is a runtime error, not a compile error:

```
invalid args `report` for command `render_report`: missing field `stream_el`
```

## Trap 5 — the engine needs a tokio reactor

`SessionHost::spawn` calls `tokio::spawn`. From bare `main` it panics:

```
there is no reactor running, must be called from the context of a Tokio 1.x runtime
```

Construct it inside Tauri's runtime:

```rust
let (mut host, adopted) = tauri::async_runtime::block_on(async { ... });
```

## Trap 6 — killing test processes

```bash
pkill -f 'target/debug/cwoffee-app'   # matches this shell's own cmdline; kills the shell (143)
```

Kill by exact name (`pkill -x cwoffee-app`) or by PID from `ps`. Likewise
`pkill -f mock_llm.py`.

## Trap 7 — memory probes

Gate on **PSS**, and watch the whole descendant tree. RSS double-counts shared
pages: the app is 248 MB PSS / 419 MB RSS, so an RSS gate would fail a build that
is actually inside budget. Matching only the main binary under-reports by ~60%
because `WebKitWebProcess` and `WebKitNetworkProcess` are separate processes.

For anything that might allocate, use a PSS watchdog that SIGKILLs on threshold.
`RLIMIT_AS` is the wrong instrument: tokio/reqwest binaries legitimately map huge
VmSize at small PSS, and the limit produces allocation failures that are not real.

```bash
./scripts/memory.sh cwoffee-app 400
./scripts/gpu-check.sh cwoffee-app
./scripts/sandbox-probe.sh
```

## Trap 8 — the TUI needs a real terminal

A pty with no winsize reports `0x0`. The TUI refuses it rather than rendering:

```
Error: terminal reports 0x0 - Cwoffee needs a real terminal size.
```

This is not cosmetic. An unbounded wrap loop at width 0 once grew the process to
8.7 GB PSS and pushed this box into swap while the inference server was running.

## Trap 9 — moving or renaming the project directory breaks the Tauri build

Tauri's build script records absolute paths in its cached output. After
`mv ~/Projects/cowry ~/Projects/cwoffee`, the build fails with:

```text
failed to read plugin permissions: failed to read file
'/home/dandandelion/Projects/cowry/target/release/build/tauri-*/out/permissions/...'
```

The path in the error is the *old* one, which makes it look like a stray
reference in the source. It is not; it is a stale artifact.

```bash
cargo clean --release -p tauri -p tauri-build -p cwoffee-app
cargo clean -p tauri -p tauri-build -p cwoffee-app   # debug profile, separately
```

`cargo clean -p` is per-profile: without `--release` it leaves the release
artifacts stale, so the debug build succeeds and the release build still fails.

## Trap 10 — a top-level config key written after a `[table]` is silently ignored

TOML assigns a bare key to whichever table is currently open. This parses fine
and does nothing:

```toml
[provider]
base_url = "http://127.0.0.1:8090/v1"
workspace = "~/somewhere"      # parsed as provider.workspace -> ignored
```

The shipped example config had exactly this shape, so `workspace`, `mode` and
`approval_timeout_secs` in a user's file had never been read. Every config
struct now uses `#[serde(deny_unknown_fields)]`, and the error carries a hint:

```text
unknown field `workspace`, expected one of `kind`, `base_url`, `model`, `api_key`

  hint: `workspace` is a top-level key. In TOML it belongs to whichever
  [table] is open, so it must appear before the first [table] header.
```

Top-level keys go before the first `[table]` header.

## Packaging

Both artifacts build from `crates/`, never from `ui/`:

```bash
cd crates
../ui/node_modules/.bin/tauri build --bundles rpm        # 4.79 MiB, system webkit
../ui/node_modules/.bin/tauri build --bundles appimage   # 112 MiB, bundles webkit
```

The AppImage needs FUSE to mount itself. On a machine without it (or inside a
container) run:

```bash
./cwoffee_0.0.0_amd64.AppImage --appimage-extract-and-run
```

Verified: the AppImage renders with no dev server, and the GPU pinning survives
its `AppRun` wrapper — `cwoffee-app` still holds `renderD128` only.
