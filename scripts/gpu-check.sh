#!/usr/bin/env bash
# gpu-check.sh — which GPU is a process actually rendering on, and is it free to?
#
# On a hybrid Optimus laptop, "which GPU did my app get?" is not answerable from
# env vars — libglvnd dispatches to a vendor at runtime. The only honest answer is
# "which /dev/dri/renderD* does the process have open". This script answers that.
#
#   usage: ./scripts/gpu-check.sh [pattern]
#   example: ./scripts/gpu-check.sh cwoffee
#
# Run it on a known-good app first (e.g. `firefox`) to calibrate, then on cwoffee.

set -uo pipefail

PATTERN="${1:-}"

# --- render node -> PCI device map (authoritative; card numbering lies) --------
echo "=== render nodes ==="
for r in /sys/class/drm/renderD*; do
  [ -e "$r" ] || continue
  pci=$(basename "$(readlink -f "$r/device" 2>/dev/null)")
  drv=$(basename "$(readlink -f "$r/device/driver" 2>/dev/null)")
  printf "  %-12s pci=%-14s driver=%s\n" "$(basename "$r")" "$pci" "$drv"
done
echo "  (Intel iGPU is normally pci=0000:00:02.0 ; dGPU is pci=0000:01:00.0)"

# --- which panel is actually driven -------------------------------------------
echo
echo "=== connected display outputs ==="
found=0
for c in /sys/class/drm/card*-*/status; do
  [ "$(cat "$c" 2>/dev/null)" = "connected" ] || continue
  printf "  %s\n" "$(basename "$(dirname "$c")")"; found=1
done
[ "$found" -eq 0 ] && echo "  (none - headless?)"
echo "  ^ the card driving the connected panel is the one the compositor scans out from."

# --- VRAM headroom ------------------------------------------------------------
if command -v nvidia-smi >/dev/null 2>&1; then
  echo
  echo "=== NVIDIA VRAM headroom ==="
  nvidia-smi --query-gpu=name,memory.total,memory.used,memory.free \
             --format=csv,noheader 2>/dev/null | sed 's/^/  /'
  echo "  ^ if free is small, an app that grabs an NVIDIA EGL context is fighting"
  echo "    your inference workload for VRAM. Force Mesa/Intel instead."
fi

# --- EGL vendor dispatch order ------------------------------------------------
echo
echo "=== EGL vendors libglvnd will try (sorted order = preference) ==="
ls -1 /usr/share/glvnd/egl_vendor.d/ /etc/glvnd/egl_vendor.d/ 2>/dev/null | grep -E '\.json$' | sed 's/^/  /'
echo "  current: __EGL_VENDOR_LIBRARY_FILENAMES=${__EGL_VENDOR_LIBRARY_FILENAMES:-<unset>}"
echo "  to pin Mesa/Intel:  export __EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/50_mesa.json"

# --- per-process render node usage -------------------------------------------
if [ -n "$PATTERN" ]; then
  echo
  echo "=== processes matching '$PATTERN' and the render nodes they hold ==="
  hit=0
  for p in $(pgrep -f -- "$PATTERN" 2>/dev/null); do
    [ "$p" = "$$" ] && continue
    [ -d "/proc/$p" ] || continue          # process may have exited mid-scan
    nodes=$(ls -l "/proc/$p/fd" 2>/dev/null | grep -o 'renderD[0-9]*' | sort -u | tr '\n' ' ')
    comm=$(cat "/proc/$p/comm" 2>/dev/null | tr -d '\0' || echo '?')
    printf "  pid %-8s %-22s %s\n" "$p" "${comm:0:22}" "${nodes:-<no render node open>}"
    hit=1
  done
  [ "$hit" -eq 0 ] && echo "  (no matching process)"
fi
