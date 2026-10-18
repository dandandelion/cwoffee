#!/usr/bin/env bash
# selftest.sh - run the in-app harness against a scripted endpoint.
#
# Why this exists: the harness used to talk to the user's own llama.cpp server.
# With one slot and a 200k window, every self-test run threw away a warm prefix
# cache and then paid to rebuild it, so "run the tests" cost minutes of prefill
# and made the next real conversation slow. A test suite has no right to spend
# somebody else's cache.
#
# Default: scripts/mock-llm.mjs on 127.0.0.1:8190, a throwaway config/data/workspace,
# and its own gateway port. Nothing touches ~/.config/cwoffee, ~/.local/share/cwoffee,
# ~/cwoffee-workspace, or :8090.
#
# Real endpoint (deliberate, and it says what it costs):
#   scripts/selftest.sh --real-endpoint http://127.0.0.1:8090/v1
#
# Usage:
#   scripts/selftest.sh                 build UI+app, run, print verdict
#   scripts/selftest.sh --no-build      use the binaries as they are
#   scripts/selftest.sh --speed 2       mock streams 2x faster
#   scripts/selftest.sh --keep          leave the temp dirs and the log behind

set -uo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD

MOCK_PORT=${MOCK_PORT:-8190}
GW_PORT=${GW_PORT:-8199}
SPEED=1
NO_BUILD=0
KEEP=0
REAL=""
for a in "$@"; do
  case "$a" in
    --no-build) NO_BUILD=1 ;;
    --keep) KEEP=1 ;;
    --speed) : ;;
    --speed=*) SPEED=${a#--speed=} ;;
    --real-endpoint) : ;;
    --real-endpoint=*) REAL=${a#--real-endpoint=} ;;
    --mock-port) : ;;
    --mock-port=*) MOCK_PORT=${a#--mock-port=} ;;
    -h|--help) sed -n '2,26p' "$0"; exit 0 ;;
  esac
done
# `--real-endpoint <url>` (space-separated) as well as `=` form
if [ -z "$REAL" ]; then
  prev=""
  for a in "$@"; do
    [ "$prev" = "--real-endpoint" ] && REAL=$a
    prev=$a
  done
fi

log=/tmp/cwoffee-selftest-$(date +%Y%m%d-%H%M%S).log
export PATH="$HOME/.cargo/bin:$PATH"

note() { printf '\033[36m[selftest.sh]\033[0m %s\n' "$*"; }
die() { printf '\033[31m[selftest.sh] %s\033[0m\n' "$*" >&2; exit 2; }

# ---- endpoint choice --------------------------------------------------------
if [ -n "$REAL" ]; then
  case "$REAL" in
    http://127.0.0.1:8190*|http://localhost:8190*) die "--real-endpoint must not be the mock" ;;
  esac
  ENDPOINT=$REAL
  note "REAL endpoint $ENDPOINT - this WILL invalidate a warm prefix cache"
  note "and re-prefill whatever the harness sends. Only run it when you mean it."
  MOCK_PID=""
else
  ENDPOINT="http://127.0.0.1:${MOCK_PORT}/v1"
  command -v node >/dev/null || die "node is required for the mock endpoint"
  [ -f scripts/mock-llm.mjs ] || die "missing scripts/mock-llm.mjs"
  node --check scripts/mock-llm.mjs || die "scripts/mock-llm.mjs does not parse"
  # A mock left over from an earlier run answers on its port with the scenario file
  # it read at start-up. Testing against that stale copy silently hides every change
  # made since - which is how one run reported that a scenario existed while the
  # server answering it had never seen it.
  if curl -fsS "http://127.0.0.1:${MOCK_PORT}/mock/state" >/dev/null 2>&1; then
    die "port ${MOCK_PORT} is already answering as a mock-llm. It holds an old copy of scenarios.json; kill it (pkill -f 'mock-llm.mjs --port ${MOCK_PORT}') or set MOCK_PORT=…"
  fi
  note "starting mock endpoint on 127.0.0.1:${MOCK_PORT} (speed x${SPEED})"
  node scripts/mock-llm.mjs --port "$MOCK_PORT" --speed "$SPEED" >>"$log" 2>&1 &
  MOCK_PID=$!
  for _ in $(seq 1 40); do
    if curl -fsS "http://127.0.0.1:${MOCK_PORT}/health" >/dev/null 2>&1; then break; fi
    sleep 0.2
  done
  curl -fsS "http://127.0.0.1:${MOCK_PORT}/health" >/dev/null 2>&1 || die "mock endpoint did not come up (see $log)"
fi

# ---- scratch homes ----------------------------------------------------------
# A self-test that writes probe conversations into the user's store leaves litter
# behind and changes what the next check sees. Everything here is disposable.
if [ -z "$REAL" ]; then
  SCRATCH=$(mktemp -d /tmp/cwoffee-selftest.XXXXXX)
  mkdir -p "$SCRATCH/config" "$SCRATCH/data" "$SCRATCH/ws"
  export XDG_CONFIG_HOME="$SCRATCH/config"
  export XDG_DATA_HOME="$SCRATCH/data"
  export CWOFFEE_WORKSPACE="$SCRATCH/ws"
  TOKEN=$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')
else
  SCRATCH=""
  TOKEN=${CWOFFEE_GATEWAY_TOKEN:-}
  [ -n "$TOKEN" ] || die "--real-endpoint runs against your real config; set CWOFFEE_GATEWAY_TOKEN to reuse the running gateway, or unset CWOFFEE_GATEWAY_PORT"
fi
export CWOFFEE_URL="$ENDPOINT"
export CWOFFEE_MODEL=cwoffee-mock
export CWOFFEE_GATEWAY_PORT="$GW_PORT"
export CWOFFEE_GATEWAY_HOST=127.0.0.1
export CWOFFEE_GATEWAY_TOKEN="$TOKEN"
# the GPU pinning this box needs (the iGPU drives the panel; see docs/BUILDRUN.md)
export __EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/50_mesa.json
export __GLX_VENDOR_LIBRARY_NAME=mesa

note "endpoint   : $ENDPOINT"
note "config dir : ${XDG_CONFIG_HOME:-$HOME/.config}"
note "data dir   : ${XDG_DATA_HOME:-$HOME/.local/share}"
note "workspace  : ${CWOFFEE_WORKSPACE:-default}"
note "gateway    : 127.0.0.1:${GW_PORT} (not 8099: the running app keeps its gateway)"
note "log        : $log"

cleanup() {
  # Only the instance this script started. The user may be running the real app
  # right now, and a test script has no business closing it; the scratch config
  # and data dirs are why two instances can coexist.
  [ -n "${APP_PID:-}" ] && kill "$APP_PID" >/dev/null 2>&1
  [ -n "${MOCK_PID:-}" ] && kill "$MOCK_PID" >/dev/null 2>&1
  # The clipboard goes back before anything is deleted: a test that quietly eats
  # what you had copied is a test that costs you an afternoon of work.
  if [ "${CLIP_ARMED:-0}" = 1 ] && [ -d "${SCRATCH:-}/clip" ]; then
    if [ -s "$SCRATCH/clip/image.png" ]; then
      wl-copy --type image/png < "$SCRATCH/clip/image.png" >/dev/null 2>&1
    elif [ -s "$SCRATCH/clip/text.txt" ]; then
      wl-copy < "$SCRATCH/clip/text.txt" >/dev/null 2>&1
    else
      wl-copy --clear >/dev/null 2>&1
    fi
    rm -f "$CLIP_MARKER" "$CLIP_BACKUP_IMG" "$CLIP_BACKUP_TXT"
  fi
  rm -f /tmp/cwoffee-selftest-fixture.png
  if [ -n "$SCRATCH" ] && [ "$KEEP" = 0 ]; then rm -rf "$SCRATCH"; fi
}
trap cleanup EXIT INT TERM

# ---- can this machine do the clipboard check at all? ------------------------
# One check pastes an image that lives in the desktop selection. That needs
# wl-clipboard; without it the build records the fact, and the check says out loud
# that it was skipped rather than printing an ok it did not earn.
CLIP_TOOL=0
if command -v wl-copy >/dev/null 2>&1 && command -v wl-paste >/dev/null 2>&1 \
   && { [ -n "${WAYLAND_DISPLAY:-}" ] || [ -n "${DISPLAY:-}" ]; }; then
  CLIP_TOOL=1
fi
if [ "$CLIP_TOOL" = 0 ]; then
  export VITE_SELFTEST_NOCLIP=1
  note "clipboard  : no wl-clipboard here; the paste check will say it was skipped"
fi

# A run that dies mid-flight (SIGKILL, a crash, a closed terminal) never reaches
# cleanup(), and the last thing it did to your desktop was put a test image on the
# clipboard. Nothing else will put yours back, so the previous run leaves a receipt
# saying what it saved and what it installed, and the next run honours it before
# touching anything.
CLIP_MARKER=/tmp/cwoffee-clipboard.marker
CLIP_BACKUP_IMG=/tmp/cwoffee-clipboard-backup.png
CLIP_BACKUP_TXT=/tmp/cwoffee-clipboard-backup.txt
if [ "$CLIP_TOOL" = 1 ] && [ -f "$CLIP_MARKER" ]; then
  prev_img=$(sed -n '1p' "$CLIP_MARKER")
  fixture_hash=$(sed -n '2p' "$CLIP_MARKER")
  now_hash=$(wl-paste -n -t image/png 2>/dev/null | sha256sum | cut -d' ' -f1)
  if [ -n "$now_hash" ] && [ "$now_hash" = "$fixture_hash" ]; then
    if [ -s "$prev_img" ]; then
      wl-copy --type image/png < "$prev_img" >/dev/null 2>&1
      note "clipboard  : a previous run left its fixture on the selection; the image you had went back"
    else
      wl-copy --clear >/dev/null 2>&1
      note "clipboard  : a previous run left its fixture on the selection and its backup is gone; cleared rather than leaving a test image where your paste lives"
    fi
  fi
  rm -f "$CLIP_MARKER" "$CLIP_BACKUP_IMG" "$CLIP_BACKUP_TXT"
fi

# ---- build ------------------------------------------------------------------
if [ "$NO_BUILD" = 0 ]; then
  note "building the self-test UI bundle (the release binary embeds it)"
  ( cd ui && VITE_SELFTEST=1 npx vite build ) >>"$log" 2>&1 || die "ui build failed (see $log)"
  note "building cwoffee-app (release, guarded); this is the slow part"
  CARGO_BUILD_JOBS=1 ./scripts/cargo-guard.sh --max-mb 3000 cargo build --release -p cwoffee-app -j 1 >>"$log" 2>&1 \
    || die "cargo build failed (see $log)"
fi
[ -x target/release/cwoffee-app ] || die "no target/release/cwoffee-app"

# No pkill here: this run owns its own config, data dir, workspace and gateway
# port, so it coexists with an app the user already has open.

note "launching the app"
# The paste check needs an image on the selection. Whatever was there is saved
# first and put back by cleanup(), in the same order it was found.
if [ "$CLIP_TOOL" = 1 ]; then
  mkdir -p "$SCRATCH/clip"
  wl-paste -l 2>/dev/null | grep -qx 'image/png' && wl-paste -n -t image/png > "$SCRATCH/clip/image.png" 2>/dev/null
  wl-paste -l 2>/dev/null | grep -qx 'text/plain' && wl-paste -n -t text/plain > "$SCRATCH/clip/text.txt" 2>/dev/null
  # Outlive $SCRATCH: cleanup() deletes that directory, and a run that survives to
  # cleanup() does not need this copy. A run that does not reach cleanup() does.
  cp "$SCRATCH/clip/image.png" "$CLIP_BACKUP_IMG" 2>/dev/null || true
  cp "$SCRATCH/clip/text.txt" "$CLIP_BACKUP_TXT" 2>/dev/null || true
  # The check compares the pasted bytes against a file at an agreed path. The app
  # cannot see the harness fixture directory, so the fixture is staged where both
  # sides know to look - otherwise the byte comparison silently does nothing.
  cp tests/fixtures/clipboard.png /tmp/cwoffee-selftest-fixture.png 2>/dev/null || true
  if wl-copy --type image/png < tests/fixtures/clipboard.png 2>/dev/null; then
    # The receipt is written only once the fixture is actually installed: a marker
    # for a run that never touched the clipboard would "restore" something nobody
    # took.
    { printf '%s\n' "$CLIP_BACKUP_IMG"; sha256sum tests/fixtures/clipboard.png | cut -d' ' -f1; } \
      > "$CLIP_MARKER" 2>/dev/null || true
    CLIP_ARMED=1
    note "clipboard  : a $(wc -c < tests/fixtures/clipboard.png)-byte PNG is on the selection; the old content goes back after"
  else
    note "clipboard  : wl-copy refused the fixture; the paste check will report the failure"
  fi
fi
# Two keys arm the harness: a self-test bundle *and* an armed process. See
# commands.rs::selftest_armed - the release binary embeds whatever ui/dist holds,
# so a production start right after this script unarms the same bundle and must
# not drive the user's endpoint, config or session store.
CWOFFEE_SELFTEST=1 ./target/release/cwoffee-app >>"$log" 2>&1 &
APP_PID=$!

# ---- wait for a verdict -----------------------------------------------------
deadline=$((SECONDS + 900))
verdict=""
while [ $SECONDS -lt $deadline ]; do
  if grep -q "SELFTEST ok" "$log" 2>/dev/null; then verdict=ok; break; fi
  if grep -q "SELFTEST FAILED" "$log" 2>/dev/null; then verdict=fail; break; fi
  # "Is there a process slot" is not "is it running". A window the user closed leaves a
  # reaped-or-zombie pid that `kill -0` happily answers for, and the run then sits out the
  # whole 900 s before saying what happened in the first three seconds. Read the state.
  st=$(awk '{print $3}' "/proc/$APP_PID/stat" 2>/dev/null)
  [ -n "$st" ] && [ "$st" != Z ] || { verdict=crash; break; }
  sleep 1
done
[ -n "$verdict" ] || verdict=timeout

# What the mock saw. A miss means the harness sent a prompt no scenario covers:
# the turn failed visibly rather than being answered with filler.
if [ -z "$REAL" ]; then
  note "mock endpoint activity:"
  curl -fsS "http://127.0.0.1:${MOCK_PORT}/mock/state" 2>/dev/null \
    | node -e '
      let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{
        const j=JSON.parse(s);
        const hits=Object.entries(j.hits??{}).sort((a,b)=>b[1]-a[1]);
        console.log(`  requests ${j.requests}, turns served ${j.turns}, thinking-off ${j.thinking_off}, thinking-on ${j.thinking_on}`);
        for (const [n,c] of hits) console.log(`  ${String(c).padStart(3)}x ${n}`);
        if ((j.misses??[]).length) { console.log(`  MISSES (${j.misses.length}):`); for (const m of j.misses) console.log(`    - ${m}`); }
        else console.log("  no unmatched prompts: every turn the harness sent had a scenario");
      });' >>"$log" 2>&1
  sed -n '/mock endpoint activity:/,$p' "$log"
fi

echo
case $verdict in
  ok)
    grep -E "SELFTEST ok" "$log" | tail -1
    note "verdict: ok"
    exit 0 ;;
  crash)
    printf '\033[31mthe app exited before printing a verdict; last lines:\033[0m\n' >&2
    tail -25 "$log" >&2
    printf '\033[31mthe harness drives a real window: if that window was closed, minimised or\ncovered during the run, the run is not a failure of the app, it is a run that\nnever happened. Re-run with the window left alone.\033[0m\n' >&2
    exit 1 ;;
  timeout)
    printf '\033[31mno verdict in 900s; last lines:\033[0m\n' >&2
    tail -25 "$log" >&2; exit 1 ;;
  *)
    grep -E "SELFTEST FAIL|selftest note" "$log" | tail -30 >&2
    exit 1 ;;
esac
