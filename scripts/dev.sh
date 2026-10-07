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

# --- --mock: a scripted endpoint, so dev runs cost no prefill ------------------
# The dev build otherwise uses the endpoint in ~/.config/cwoffee/config.toml,
# which on this box is a single-slot 200k-token llama.cpp. Pointing it at
# scripts/mock-llm.mjs makes `VITE_SELFTEST=1 ./scripts/dev.sh --mock` a harness
# run that touches neither the model nor the prompt cache.
MOCK_PID=""
cleanup() { [ -n "$MOCK_PID" ] && kill "$MOCK_PID" >/dev/null 2>&1 || true; }
trap cleanup EXIT INT TERM

ARGS=()
for a in "$@"; do
  if [ "$a" = "--mock" ]; then
    MOCK_PORT=${CWOFFEE_MOCK_PORT:-8190}
    node "$(dirname "$0")/mock-llm.mjs" --port "$MOCK_PORT" >/dev/null 2>&1 &
    MOCK_PID=$!
    for _ in $(seq 1 40); do
      curl -fsS "http://127.0.0.1:${MOCK_PORT}/health" >/dev/null 2>&1 && break
      sleep 0.2
    done
    curl -fsS "http://127.0.0.1:${MOCK_PORT}/health" >/dev/null 2>&1 \
      || { echo "dev.sh: the scripted endpoint did not come up on ${MOCK_PORT}"; exit 1; }
    export CWOFFEE_URL="http://127.0.0.1:${MOCK_PORT}/v1"
    export CWOFFEE_MODEL=cwoffee-mock
    echo "cwoffee dev — scripted endpoint on 127.0.0.1:${MOCK_PORT} (no prefill; scenarios in tests/scenarios/scenarios.json)"
  else
    ARGS+=("$a")
  fi
done

echo "cwoffee dev — EGL pinned to: ${__EGL_VENDOR_LIBRARY_FILENAMES}"

# `tauri dev` runs from crates/tauri-app so tauri.conf.json is the config root.
# The CLI lives in ui/node_modules (installed via npm) to avoid a slow
# cargo-tauri rebuild on every version bump.
TAURI_BIN="$(cd "$(dirname "$0")/.." && pwd)/ui/node_modules/.bin/tauri"

if [ -n "${CWOFFEE_DEV_DRYRUN:-}" ]; then
  echo "dev.sh (dry run): would exec: (cd crates/tauri-app && $TAURI_BIN dev ${ARGS[*]-})"
  exit 0
fi

if [ -x "$TAURI_BIN" ]; then
  ( cd crates/tauri-app && "$TAURI_BIN" dev "${ARGS[@]+"${ARGS[@]}"}" )
else
  echo "dev.sh: tauri CLI not found at $TAURI_BIN"
  echo "        run: (cd ui && npm install)"
  echo "        GPU check in the meantime:"
  ./scripts/gpu-check.sh cwoffee || true
  exit 1
fi
