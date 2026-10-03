#!/usr/bin/env bash
# dev.sh — run cwoffee in dev mode with the GPU policy from PLAN.md §12 applied.
#
# The env vars below must be set BEFORE any GTK/EGL initialisation, otherwise
# libglvnd hands us an NVIDIA EGL context (10_nvidia.json sorts first) on a GPU
# that is already ~97% full from inference. See PLAN.md §12.

set -euo pipefail

# --- GPU policy: force Mesa / Intel iGPU --------------------------------------
export __EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/50_mesa.json
export __GLX_VENDOR_LIBRARY_NAME=mesa

# Only needed if pinning Mesa turns out to be insufficient (PLAN.md §12 fallbacks).
# export WEBKIT_DISABLE_DMABUF_RENDERER=1
# export WEBKIT_DISABLE_COMPOSITING_MODE=1

# --- toolchain ----------------------------------------------------------------
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

cd "$(dirname "$0")/.."

echo "cwoffee dev — EGL pinned to: ${__EGL_VENDOR_LIBRARY_FILENAMES}"

# `tauri dev` runs from crates/tauri-app so tauri.conf.json is the config root.
# The CLI lives in ui/node_modules (installed via npm) to avoid a slow
# cargo-tauri rebuild on every version bump.
TAURI_BIN="$(cd "$(dirname "$0")/.." && pwd)/ui/node_modules/.bin/tauri"

if [ -x "$TAURI_BIN" ]; then
  ( cd crates/tauri-app && "$TAURI_BIN" dev "$@" )
else
  echo "dev.sh: tauri CLI not found at $TAURI_BIN"
  echo "        run: (cd ui && npm install)"
  echo "        GPU check in the meantime:"
  ./scripts/gpu-check.sh cwoffee || true
  exit 1
fi
