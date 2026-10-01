#!/usr/bin/env bash
# dev.sh — run cowry in dev mode with the GPU policy from PLAN.md §12 applied.
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

echo "cowry dev — EGL pinned to: ${__EGL_VENDOR_LIBRARY_FILENAMES}"

# M0 will replace this with: npm run tauri dev
# (uses @tauri-apps/cli via npm rather than cargo-tauri, to avoid a slow Rust CLI rebuild)
if [ -f ui/package.json ]; then
  ( cd ui && npm run tauri dev )
else
  echo "ui/ not scaffolded yet (M0). Engine/TUI scaffold is M1."
  echo "GPU check in the meantime:"
  ./scripts/gpu-check.sh cowry || true
fi
