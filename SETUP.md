# Environment setup — Cwoffee

Status as of 2026-10-01. Machine: Fedora 44 Workstation, x86_64, 20 cores, 62 GB RAM.

**This file is the record of one machine. [`../setup.sh`](../setup.sh) is the check that runs on
yours.** It probes the same pkg-config modules and command-line tools listed below, prints them in
one table with the package that provides each one, and can install what is missing (`--deps`, after
asking). It then builds the UI and the release binaries; `--tests`, `--board` and `--desktop` add
the suites, the page board on both cores, and a launcher that is verified by launching it. If a
number in this file disagrees with what `./setup.sh` prints, believe `./setup.sh`.

## ✅ Installed (done for you, no sudo)

| What | Version | How |
|---|---|---|
| rustc / cargo / rustup | 1.98.1 | `rustup`, user-local at `~/.cargo` |
| crates.io reachability | verified | test crate built successfully |
| node / npm | 22.23.2 / 10.9.8 | pre-existing |
| gcc / g++ | 16.2.1 | pre-existing |
| cmake / pkg-config | 4.3.0 / 2.5.1 | pre-existing |
| **bwrap** (bubblewrap) | `/usr/bin/bwrap` | pre-existing → the `exec` sandbox |
| **pdftotext** (poppler) | `/usr/bin/pdftotext` | pre-existing → PDF-in fallback |
| WebKitGTK **runtime** | 2.54.0 (`webkit2gtk4.1`) | pre-existing, bumped by the `-devel` install |

`~/.cargo/bin` is on PATH via rustup's env; add to your shell if it isn't:
```bash
echo 'source "$HOME/.cargo/env"' >> ~/.bashrc
```

## ✅ System packages — VERIFIED INSTALLED (2026-10-01)

`pkg-config` probes (what Tauri's build script actually checks):

```
webkit2gtk-4.1           2.54.0     javascriptcoregtk-4.1    2.54.0
libsoup-3.0              3.6.6      gtk+-3.0                 3.24.52
glib-2.0                 2.88.3     gobject-2.0 / gio-2.0    2.88.3
openssl                  3.5.8      xkbcommon                1.13.1
```

Note: installing `-devel` upgraded the webkit runtime **2.52.5 → 2.54.0**. Fine, and newer is better here.

### Compile proof (not just package presence)

Built `webkit2gtk v2.0.2` + `webkit2gtk-sys` in a scratch crate — this runs pkg-config, bindgen and `cc` against the real system headers:

```
Compiling webkit2gtk-sys v2.0.2
Checking  webkit2gtk v2.0.2
Finished dev profile in 18.10s        EXIT=0
```

**The webview toolchain works.** M0 is unblocked.

## ⚠️ GPU policy: pin Cwoffee to the Intel iGPU

```
renderD128  pci=0000:00:02.0  i915    <- Intel Iris Xe
renderD129  pci=0000:01:00.0  nvidia  <- RTX 3080 Ti Laptop
connected panel: card1-eDP-1  -> display is driven by INTEL
NVIDIA VRAM: 15993 / 16384 MiB used  -> 44 MiB FREE
EGL vendor order: 10_nvidia.json, 50_mesa.json  -> NVIDIA preferred by default
```

So the default EGL vendor is NVIDIA, on a GPU that is 97.6% full from inference. Force Mesa:

```bash
export __EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/50_mesa.json
export __GLX_VENDOR_LIBRARY_NAME=mesa
```

Verify with `./scripts/gpu-check.sh cwoffee` — cwoffee should hold **renderD128 only**.

Full reasoning, the three places to set it, and fallbacks: **PLAN.md §12**.

## Optional (not blocking)

```bash
sudo dnf install -y pandoc typst sqlite    # optional converters / CLI
```
- `typst` → we vendor it as a **crate**, so the CLI is not required.
- `pandoc` → optional DOCX fallback only.
- `sqlite3` CLI → nice for poking at the DB; `rusqlite` bundles its own libsqlite.

## Model endpoint (verified working)

```
base_url:  http://127.0.0.1:8090/v1
model:     qwen3.8-flash-next-iq3_s
n_ctx:     131072      images: true      api_key: false      slots: 1
```

Health / introspection (the app reads these to auto-build the model picker):
```bash
curl -s http://127.0.0.1:8090/health
curl -s http://127.0.0.1:8090/props
curl -s http://127.0.0.1:8090/v1/models
```

Verified: streaming tool calls ✅ · `reasoning_content` deltas ✅ · `chat_template_kwargs.enable_thinking` ✅

## Config location

```
~/.config/cwoffee/config.toml      # providers, models, approval defaults
~/.local/share/cwoffee/cwoffee.db    # SQLite: sessions, messages, FTS, rendered HTML
```
See `config/cwoffee.toml.example`.

## Build, run, and the traps

See **`docs/BUILDRUN.md`**. It covers the dev loop, the release build, packaging,
and seven traps that cost real time: the debug binary rendering a blank window,
embedded assets going stale, Tauri argument casing, the tokio reactor panic,
`pkill -f` killing the invoking shell, PSS-vs-RSS gating, and the TUI refusing a
`0x0` terminal.

Everything needed for M0-M2 is installed. `cargo-tauri` is not installed on
purpose - the Tauri CLI comes from `ui/node_modules/.bin/tauri` via npm, which
avoids a several-minute Rust CLI build on every iteration.
