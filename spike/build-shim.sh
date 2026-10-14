#!/usr/bin/env bash
# Build the spike's Rust tool shim, reusing cwoffee's own cached serde_json so this
# needs no network and no new deps. If that cache is absent, use a cargo project instead
# (see the note at the bottom).
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
rustc="${RUSTC:-}"
if [ -z "$rustc" ]; then
  if command -v rustc >/dev/null 2>&1; then rustc="$(command -v rustc)"
  elif [ -x "$HOME/.cargo/bin/rustc" ]; then rustc="$HOME/.cargo/bin/rustc"
  else echo "no rustc found (put cargo on PATH or set RUSTC)"; exit 1; fi
fi

# where cwoffee's already-compiled deps live; override with CWOFFEE_TARGET
cwoffee_target="${CWOFFEE_TARGET:-$here/../../cwoffee/target/debug/deps}"
sj=$(ls "$cwoffee_target"/libserde_json-*.rlib 2>/dev/null | head -1)
if [ -z "$sj" ]; then
  echo "no cached serde_json in $cwoffee_target" >&2
  echo "either build cwoffee once (cargo build) or: cargo new shim && cd shim && cargo add serde_json" >&2
  exit 1
fi

"$rustc" --edition 2021 -O "$here/rust-shim.rs" -o "$here/cwshim" \
  --extern serde_json="$sj" -L dependency="$cwoffee_target" || exit 1

echo "built $here/cwshim"
printf '{"path":"/etc/hostname"}' | "$here/cwshim" read_file; echo
printf '{"path":"/etc/../proc/sys/kernel/hostname"}' | "$here/cwshim" read_file; echo
