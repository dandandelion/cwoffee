#!/usr/bin/env bash
# Run a build or test command under a resident-memory watchdog.
#
# Why this exists: the engine test suite once allocated without bound (a helper
# called fs::read("/dev/urandom"), which is an infinite stream, not a file) and
# the kernel chose the biggest other process on the box to kill - llama-server,
# i.e. the inference the person is actually working with. A build on this machine
# is not a private act: it competes with a model that already owns most of RAM, so
# it gets a ceiling and a kill switch instead of good intentions.
#
# Usage:
#   scripts/cargo-guard.sh cargo check -p cwoffee-engine --lib -j 1
#   scripts/cargo-guard.sh --max-mb 2500 cargo test -p cwoffee-gateway -- --test-threads=2
set -uo pipefail

MAX_MB=2500
while [ "${1:-}" = "--max-mb" ]; do MAX_MB="$2"; shift 2; done
if [ "$#" -eq 0 ]; then
  echo "usage: $0 [--max-mb N] <command> [args...]" >&2
  exit 2
fi

PEAK=0
# A new process group, so the kill takes rustc, linker and any test binary with
# it. Killing only the cargo parent leaves rustc running, holding its gigabytes.
setsid "$@" &
CMD=$!
PGID=$(ps -o pgid= -p "$CMD" 2>/dev/null | tr -d ' ')
[ -n "$PGID" ] || PGID=$CMD

while kill -0 "$CMD" 2>/dev/null; do
  sleep 2
  # Sum RSS across the whole group. PSS would be more exact but ps does not
  # report it; for a ceiling this is the right direction to err in.
  CUR=$(ps -o rss= -g "$PGID" 2>/dev/null | awk '{s+=$1} END {printf "%d", (s+0)/1024}')
  CUR=${CUR:-0}
  [ "$CUR" -gt "$PEAK" ] && PEAK=$CUR
  if [ "$CUR" -gt "$MAX_MB" ]; then
    echo
    echo "cargo-guard: ${CMD} tree reached ${CUR} MB (ceiling ${MAX_MB} MB) - killing it" >&2
    echo "cargo-guard: nothing else on this box should pay for a build; look at what allocated before rerunning" >&2
    kill -TERM -"$PGID" 2>/dev/null
    sleep 3
    kill -KILL -"$PGID" 2>/dev/null
    wait "$CMD" 2>/dev/null
    echo "cargo-guard: peak was ${PEAK} MB" >&2
    exit 124
  fi
done

wait "$CMD"
RC=$?
echo "cargo-guard: ok, peak ${PEAK} MB (ceiling ${MAX_MB} MB), exit ${RC}"
exit $RC
