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
| WebKitGTK **runtime** | 2.52.5 (`webkit2gtk4.1`) | pre-existing → the webview will *run* |

`~/.cargo/bin` is on PATH via rustup's env; add to your shell if it isn't:
```bash
echo 'source "$HOME/.cargo/env"' >> ~/.bashrc
```

## ⚠️ Needs your password (I can't sudo)

Tauri v2 on Linux needs **dev headers** to *compile* the webview. The runtime is present but the `-devel` packages are not. This blocks **M0 only** — M1 (engine + TUI) is completely unaffected, so we can start M1 immediately.

```bash
sudo dnf install -y \
  webkit2gtk4.1-devel \
  javascriptcoregtk-4.1-devel \
  libsoup3-devel \
  gtk3-devel \
  glib2-devel \
  openssl-devel \
  rhash \
  libxkbcommon-devel \
  libxkbcommon-x11-devel
```

Verify afterwards:
```bash
for p in webkit2gtk-4.1 javascriptcoregtk-4.1 libsoup-3.0 gtk+-3.0; do
  printf "%-24s %s\n" "$p" "$(pkg-config --modversion $p 2>/dev/null || echo MISSING)"
done
```

## Optional (not blocking)

```bash
sudo dnf install -y pandoc sqlite-typst    # optional converters
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
cargo +stable --version && node -v   # sanity
```
Then say the word and I'll scaffold **M1** (engine + TUI) — which needs none of the sudo'd packages.
