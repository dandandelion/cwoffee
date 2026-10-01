# Environment setup — Cowry

Status as of 2026-10-01. Machine: Fedora 44 Workstation, x86_64, 20 cores, 62 GB RAM.

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

## ⚠️ Hardware hazard: NVIDIA + Wayland

```
GPU:     Intel Iris Xe (Alder Lake-P) + NVIDIA RTX 3080 Ti Laptop
Driver:  proprietary NVIDIA (/dev/nvidia0 present)
Session: Wayland (WAYLAND_DISPLAY=wayland-0)
```

WebKitGTK's DMABUF renderer is known to break with the proprietary NVIDIA driver under Wayland — blank/flickering webview or startup crash. **Test a solid-colour webview window on M0 day 1.** Fixes to try in order:

```bash
WEBKIT_DISABLE_DMABUF_RENDERER=1
WEBKIT_DISABLE_COMPOSITING_MODE=1
LIBGL_ALWAYS_SOFTWARE=1            # last resort
```

Or the `webkit2gtk-nvidia-quirk` crate (v2.2.0). Details in PLAN.md §12.

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
~/.config/cowry/config.toml      # providers, models, approval defaults
~/.local/share/cowry/cowry.db    # SQLite: sessions, messages, FTS, rendered HTML
```
See `config/cowry.toml.example`.

## Next command to run

```bash
cd ~/Projects/cowry
source "$HOME/.cargo/env" && cargo --version && node -v   # sanity
```

Everything needed for **both M0 and M1** is installed. `cargo-tauri` is not installed — we'll use `@tauri-apps/cli` via npm (faster to iterate, no 5-minute Rust CLI build).

**M0 day 1 is the NVIDIA/Wayland webview test.** Then the skeleton.
