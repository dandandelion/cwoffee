#!/usr/bin/env bash
# Launch cwoffee's desktop app with pi as its core.
#
# A script exists because five values decide whether this is the product or a demo, and
# getting one wrong does not stop the app from opening — it just quietly offers less:
#
#   CWOFFEE_PI_EXT    the extension that mounts cwoffee's tools. Without it the model has
#                     no tools at all and nothing says so.
#   CWOFFEE_CWTOOL    the Rust tool host behind them. Without it the extension registers
#                     nothing and says so on stderr.
#   CWOFFEE_PI_AGENT_DIR  where pi's extensions and state live. Put it in /tmp and every
#                     boot re-mounts and re-symlinks; put it in a cache dir and pi's
#                     prefix cache survives, which is what the meter in the footer is
#                     actually showing.
#   CWOFFEE_GATEWAY_PORT/TOKEN  what a phone needs to join. Printed on boot.
#   CWOFFEE_MODE      ask / session / auto — the approval ladder.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# Which tree the artifact came from is a fact about what runs, so say it. The neighbour
# checkout also defines packages named `cwoffee-app` / `cwoffee-gateway`, and cargo keeps
# its freshness bookkeeping per workspace: a shared target dir can hold a binary built
# entirely from the *other* tree's sources while this one's `cargo build` says "Finished".
# Look here first, and shout when we have to look there.
if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
  target=$CARGO_TARGET_DIR
elif [[ -x "$here/target/release/cwoffee-app" ]]; then
  target="$here/target"
elif [[ -x "$here/../cwoffee/target/release/cwoffee-app" ]]; then
  target="$here/../cwoffee/target"
  echo "[run-pi-app] nothing built in this checkout's ./target — running the artifact from $target," >&2
  echo "[run-pi-app] which was linked from ANOTHER checkout's sources. Build here, or expect this" >&2
  echo "[run-pi-app] run to test code that is not in this working tree." >&2
else
  target="$here/target"
fi

BIN=${CWOFFEE_BIN:-$target/release/cwoffee-app}
# Prefer the release tool host; fall back to a debug one rather than launching an app
# whose tools are missing, but say which one won.
CWTOOL=${CWOFFEE_CWTOOL:-}
if [[ -z "$CWTOOL" ]]; then
  if [[ -x "$target/release/cwtool" ]]; then CWTOOL="$target/release/cwtool"
  elif [[ -x "$target/debug/cwtool" ]]; then CWTOOL="$target/debug/cwtool"; echo "[run-pi-app] using the DEBUG cwtool (no release build found)"
  else echo "[run-pi-app] no cwtool built yet: cargo build --release -p cwoffee-bridge --bin cwtool"; exit 1; fi
fi
EXT=${CWOFFEE_PI_EXT:-$here/pi-tools/cwoffee-tools.ts}

export CWOFFEE_CORE=pi
export CWOFFEE_PI_EXT="$EXT"
export CWOFFEE_CWTOOL="$CWTOOL"
export CWOFFEE_PI_AGENT_DIR=${CWOFFEE_PI_AGENT_DIR:-"$HOME/.local/share/cwoffee/pi-agent"}
export CWOFFEE_PI_SESSION_DIR=${CWOFFEE_PI_SESSION_DIR:-"$HOME/.local/share/cwoffee/pi-sessions"}
export CWOFFEE_MODE=${CWOFFEE_MODE:-ask}
export CWOFFEE_GATEWAY_PORT=${CWOFFEE_GATEWAY_PORT:-8122}
# A token is only defaulted when the caller did not bring one: the gateway refuses
# connections without it, and a silent default is how a laptop's session ends up on a
# shared network.
if [[ -n "${CWOFFEE_GATEWAY_TOKEN:-}" ]]; then export CWOFFEE_GATEWAY_TOKEN; fi

for f in "$BIN" "$EXT"; do
  [[ -e "$f" ]] || { echo "[run-pi-app] missing $f"; exit 1; }
done

# Say what is mounted before the window opens, so the boot log can be compared against
# the panel: "1 extension(s)" plus the tools line is the proof the guardrail is on.
echo "[run-pi-app] app      $BIN"
echo "[run-pi-app] toolhost $CWTOOL ($("$CWTOOL" specs 2>/dev/null | python3 -c 'import json,sys;print(json.load(sys.stdin)["count"])' 2>/dev/null || echo '?') tools)"
echo "[run-pi-app] ext      $EXT"
echo "[run-pi-app] agent    $CWOFFEE_PI_AGENT_DIR"
echo "[run-pi-app] mode     $CWOFFEE_MODE   gateway :$CWOFFEE_GATEWAY_PORT"

exec "$BIN"
