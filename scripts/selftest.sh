#!/usr/bin/env bash
# selftest.sh - run the in-app harness against a scripted endpoint.
#
# Why this exists: the harness used to talk to the user's own llama.cpp server.
# With one slot and a 200k window, every self-test run threw away a warm prefix
# cache and then paid to rebuild it, so "run the tests" cost minutes of prefill
# and made the next real conversation slow. A test suite has no right to spend
# somebody else's cache.
#
# Default: scripts/mock-llm.mjs on 127.0.0.1:8190, a throwaway config/data/state/workspace,
# its own pi home, and its own gateway port. Nothing touches ~/.config/cwoffee,
# ~/.local/share/cwoffee, ~/cwoffee-workspace, or :8090. For the pi core that means the pi
# home too — it is a fourth store, and it lives under XDG_STATE_HOME by default, so a run
# that exports only config and data writes into the user's real pi sessions, models.json and
# session map. See the scratch block below.
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
  # Answer "is this server current?", not "is something listening here?". An earlier
  # version refused any listener, which told the operator to kill a mock that had just
  # been started with the current file - and told them to pattern-kill it, which is the
  # kind of instruction that once ended an unrelated process with the same name.
  state=$(curl -fsS "http://127.0.0.1:${MOCK_PORT}/mock/state" 2>/dev/null || true)
  if [ -n "$state" ]; then
    want=$(node -e 'const c=require("node:crypto"),f=require("node:fs");process.stdout.write(c.createHash("sha256").update(f.readFileSync(process.argv[1])).digest("hex").slice(0,12))' tests/scenarios/scenarios.json)
    got=$(node -e 'const s=JSON.parse(process.argv[1]||"{}");process.stdout.write(String(s.fingerprint ?? ""))' "$state")
    if [ -n "$got" ] && [ "$got" = "$want" ]; then
      note "port ${MOCK_PORT} answers as a mock-llm with the CURRENT scenarios.json (fingerprint $got) - reusing it"
    else
      pid=$(ss -ltnp "sport = :${MOCK_PORT}" 2>/dev/null | grep -o 'pid=[0-9]*' | head -1 | cut -d= -f2)
      die "port ${MOCK_PORT} answers as a mock-llm with a stale or unknown scenario set (it says ${got:-no fingerprint}, the file says $want). End that one server by its pid (${pid:-not found from ss}) — do not pattern-kill by name, something else may answer to it — or set MOCK_PORT=…"
    fi
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
  mkdir -p "$SCRATCH/config" "$SCRATCH/data" "$SCRATCH/state" "$SCRATCH/pi" "$SCRATCH/ws"
  export XDG_CONFIG_HOME="$SCRATCH/config"
  export XDG_DATA_HOME="$SCRATCH/data"
  # A fourth store, and the one this script used to miss. The pi core keeps its agent dir —
  # models.json, settings.json, its own sessions, the cwoffee session map and the child
  # ledger — under XDG_STATE_HOME unless told otherwise, so a run with only config/data set
  # wrote into the user's real pi home and then deleted the workspace those sessions named.
  # The next run inherited them and pi answered "Stored session working directory does not
  # exist", which surfaced as a model process that "did not answer get_state": a harness
  # poisoning its own next run, one deleted scratch dir at a time.
  export XDG_STATE_HOME="$SCRATCH/state"
  export CWOFFEE_PI_AGENT_DIR="$SCRATCH/pi"
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
note "state dir  : ${XDG_STATE_HOME:-$HOME/.local/share}"
if [ "${CWOFFEE_CORE:-rust}" = pi ]; then
  # Say it, because the claim "this run touches nothing of yours" is only worth what the
  # reader can check from the header.
  note "pi home    : ${CWOFFEE_PI_AGENT_DIR:-unset — the pi core would use the real one}"
fi
note "workspace  : ${CWOFFEE_WORKSPACE:-default}"
note "gateway    : 127.0.0.1:${GW_PORT} (not 8099: the running app keeps its gateway)"
# The board's meaning depends entirely on this line. A run with no CWOFFEE_CORE exercises the
# rust engine, and its results say nothing at all about pi parity — which is how a missing
# pi-core feature stayed green for two sessions (gotchas 85, 86). Say the core, and say out
# loud when the pi core has no toolhost: without cwtool the model is told "tool not found"
# for every call, and half the board turns into a core-shape accident.
note "core       : ${CWOFFEE_CORE:-rust} (from the environment; check /proc/<app>/environ if unsure)"
if [ "${CWOFFEE_CORE:-rust}" = pi ] && [ -z "${CWOFFEE_CWTOOL:-}" ]; then
  note "toolhost   : not set — this run will build and use its own target/release/cwtool"
elif [ "${CWOFFEE_CORE:-rust}" = pi ]; then
  note "toolhost   : ${CWOFFEE_CWTOOL} (from the environment; this run does NOT rebuild it)"
fi
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

# One client, two hosts: `invoke('cmd', {...})` reaches either a Tauri command or the gateway's
# dispatch for the same verb, and `transport.ts` passes the payload through unchanged. Tauri
# spells command parameters one way by default and the gateway reads the engine's own snake_case
# off the frame, so an argument name is a promise made in three places at once. That has broken
# twice for real: once an approval answer a phone could not deliver (`allow_session` against
# `allowSession`), and once every multi-word argument on the endpoint screen answering
# "invalid args `baseUrl` ... missing required key baseUrl" on the desktop while the Rust tests —
# which spoke the server's own dialect — stayed green. Check the names before spending ten
# minutes building a board that cannot press the buttons it grades.
if [ -f scripts/wire-names.mjs ] && command -v node >/dev/null 2>&1; then
  wn=$(node scripts/wire-names.mjs 2>&1) || die "the client's argument names and the hosts' do not agree:
$wn"
  note "wire names : $(printf '%s' "$wn" | head -1)"
else
  note "wire names : SKIPPED (no node, or scripts/wire-names.mjs is missing)"
fi

# ---- build ------------------------------------------------------------------
# Which tree the artifacts come from decides what the board is actually testing, so it is
# settled before the build rather than guessed afterwards. The upstream checkout defines
# packages with the *same names* (`cwoffee-app`, `cwoffee-gateway`), and cargo keeps
# freshness per workspace: share one target dir between the two and a `cargo build` here can
# report "Finished" while the executable in that dir was linked from the other tree. That is
# not a slow build; it is a board that grades code it never compiled.
tgt=${CARGO_TARGET_DIR:-$PWD/target}
case "$tgt" in
  "$PWD"/target|"$PWD"/target/*) ;;
  *) die "refusing to build into $tgt: that target dir belongs to another tree, and both trees package cwoffee-app/cwoffee-gateway under the same names. Unset CARGO_TARGET_DIR (this checkout builds into ./target) or point it at a directory of this checkout's own." ;;
esac

# The proof, after the build: an artifact older than the sources it is supposed to contain.
# Which sources count is stated per artifact, because guessing is wrong in both directions:
# comparing `cwtool` to `ui/dist` or to the Tauri app's sources invents failures, and
# ignoring a package that really does feed an artifact hides a stale one. These are the
# packages each binary is linked from, plus the UI bundle the app embeds at build time.
app_inputs() { printf '%s\n' crates/tauri-app crates/gateway crates/bridge crates/engine Cargo.toml Cargo.lock ui/dist; }
tool_inputs() { printf '%s\n' crates/bridge crates/engine Cargo.toml Cargo.lock; }
newer_than() {
  local a=$1 f
  while IFS= read -r f; do
    local hit
    hit=$(find "$f" -type f -newer "$a" 2>/dev/null | head -1)
    [ -n "$hit" ] && { printf '%s' "$hit"; return 0; }
  done
  return 1
}

if [ "$NO_BUILD" = 0 ]; then
  note "building the self-test UI bundle (the release binary embeds it)"
  ( cd ui && VITE_SELFTEST=1 npx vite build ) >>"$log" 2>&1 || die "ui build failed (see $log)"
  note "building cwoffee-app (release, guarded); this is the slow part"
  # cwtool travels with the app: the pi core's tools, guardrails and answer wording all
  # come out of that binary, so a board that runs whatever cwtool was lying around in
  # target/release is testing an unknown vintage. One build line, one vintage.
  # No `--bin cwtool` here. A target filter applies to **every** selected package, so
  # `-p cwoffee-app -p cwoffee-bridge --bin cwtool` builds the bridge's cwtool and *no
  # binary of the app at all* — cargo does not warn, prints "Finished", and the board runs
  # whatever cwoffee-app was left in the target dir by an older build. That is how a board
  # graded a three-hour-old bridge while the tree had moved on. Build the packages, all of
  # their targets, and let the proof below check the artifacts.
  CARGO_BUILD_JOBS=1 ./scripts/cargo-guard.sh --max-mb 3000 cargo build --release -p cwoffee-app -p cwoffee-gateway -p cwoffee-bridge -j 1 >>"$log" 2>&1 \
    || die "cargo build failed (see $log)"
  # "Finished" is not evidence. Cargo has been seen to recompile the engine and the bridge
  # and leave `cwoffee-app` alone, so the app keeps an embedded bridge from hours ago and the
  # board reports red for behaviour this tree no longer has. Compare the artifact against the
  # sources, force it once if it lost, and refuse to run if it loses twice.
  stale=$(newer_than target/release/cwoffee-app $(app_inputs))
  if [ -n "$stale" ]; then
    note "target/release/cwoffee-app is older than $stale — the build said finished without relinking the app; forcing one relink"
    touch crates/tauri-app/src/main.rs
    CARGO_BUILD_JOBS=1 ./scripts/cargo-guard.sh --max-mb 3000 cargo build --release -p cwoffee-app -p cwoffee-gateway -p cwoffee-bridge -j 1 >>"$log" 2>&1 \
      || die "cargo rebuild failed (see $log)"
    stale=$(newer_than target/release/cwoffee-app $(app_inputs))
    [ -z "$stale" ] || die "target/release/cwoffee-app is STILL older than $stale: this board would be running an artifact it did not build. Any red it prints belongs to that older code, not to this tree."
  fi
fi
[ -x target/release/cwoffee-app ] || die "no target/release/cwoffee-app"
if [ "${CWOFFEE_CORE:-rust}" = pi ] && [ -z "${CWOFFEE_CWTOOL:-}" ]; then
  [ -x target/release/cwtool ] || die "the pi core needs a tool host and target/release/cwtool is missing (see $log)"
  # Same proof for the tool host: the tools, their guardrails and the wording of a tool's
  # answer all live in that binary, so a stale cwtool grades stale tool behaviour.
  stale=$(newer_than target/release/cwtool $(tool_inputs))
  [ -z "$stale" ] || die "target/release/cwtool is older than $stale: rebuild it (cargo build --release -p cwoffee-bridge --bin cwtool) before trusting a tool-shaped red"
  export CWOFFEE_CWTOOL="$PWD/target/release/cwtool"
fi
if [ "${CWOFFEE_CORE:-rust}" = pi ]; then
  # The extension is the other half of the tool story: cwtool carries the definitions and
  # the guardrails, and this file is what hands them to pi. A board that runs a pi core with
  # *no* extension sees "tool not found" for every call, and then prints a dozen red lines
  # that are one missing file — which is how a run once reported no approval cards, no
  # thinking block, no table and no file chips in the same breath, with a green-looking
  # settings section above them.
  if [ -z "${CWOFFEE_PI_EXT:-}" ]; then
    [ -f "$ROOT/pi-tools/cwoffee-tools.ts" ] || die "the pi core needs the tool extension and pi-tools/cwoffee-tools.ts is missing"
    export CWOFFEE_PI_EXT="$ROOT/pi-tools/cwoffee-tools.ts"
  fi
  [ -f "$CWOFFEE_PI_EXT" ] || die "CWOFFEE_PI_EXT does not name a file: $CWOFFEE_PI_EXT"
  note "tool ext   : $CWOFFEE_PI_EXT"
fi

note "artifacts  : app $(date -r target/release/cwoffee-app '+%m-%d %H:%M:%S'), target dir $tgt (built from this tree)"

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
    # Tell the app where the fixture lives so the paste check can ask for it to be put back
    # on the selection at the moment it is needed. The selection belongs to the desktop, not
    # to this run: a person copying something, or an earlier check copying an answer, replaces
    # the image with text, and the check then reports "nothing sendable" about a fixture that
    # stopped existing mid-run.
    CLIP_FIXTURE=/tmp/cwoffee-selftest-fixture.png
    note "clipboard  : a $(wc -c < tests/fixtures/clipboard.png)-byte PNG is on the selection; the old content goes back after"
  else
    note "clipboard  : wl-copy refused the fixture; the paste check will report the failure"
  fi
fi
# Two keys arm the harness: a self-test bundle *and* an armed process. See
# commands.rs::selftest_armed - the release binary embeds whatever ui/dist holds,
# so a production start right after this script unarms the same bundle and must
# not drive the user's endpoint, config or session store.
CWOFFEE_SELFTEST=1 CWOFFEE_SELFTEST_CLIP_IMG="${CLIP_FIXTURE:-}" ./target/release/cwoffee-app >>"$log" 2>&1 &
APP_PID=$!

# The premise, checked before anything is graded. The pi core prints how many extensions it
# mounted in its own boot line; zero means every tool-shaped case below is about a missing
# file rather than about this tree. Stopping with that said beats printing fifteen failures
# that share one cause.
if [ "${CWOFFEE_CORE:-rust}" = pi ]; then
  boot=""
  for _ in $(seq 1 30); do
    boot=$(grep -m1 'core pi .*extension' "$log" 2>/dev/null || true)
    [ -n "$boot" ] && break
    sleep 0.5
  done
  if [ -z "$boot" ]; then
    die "no 'core pi ... up' line in the log after 15 s: the board cannot tell what core it is grading, so its verdict would mean nothing"
  fi
  case "$boot" in
    *"0 extension"*) die "the pi core came up with no tool extension. Boot line: $boot" ;;
    *) note "boot line  : ${boot#cwoffee/pi] }" ;;
  esac
fi

# ---- wait for a verdict -----------------------------------------------------
# The board's own clock. It has grown as the page harness grew: the settings walk opens
# twenty-odd screens and the models round trip waits for a discovery, a test and a save.
# A cap that is too tight turns "slow" into "no verdict", which loses every piece of
# evidence the run already gathered - so the number is a knob, and the verdict line says
# how long the board actually took.
BOARD_WAIT=${CWOFFEE_SELFTEST_WAIT:-1500}
deadline=$((SECONDS + BOARD_WAIT))
verdict=""
while [ $SECONDS -lt $deadline ]; do
  if grep -q "SELFTEST ok" "$log" 2>/dev/null; then verdict=ok; break; fi
  if grep -q "SELFTEST FAILED" "$log" 2>/dev/null; then verdict=fail; break; fi
  # A page-side exception inside the harness kills the run mid-chain: no verdict is
  # printed, and the only signal would otherwise be a 900 s watchdog expiring with the
  # crash three seconds in. Say which line threw, because that is the whole diagnosis.
  if grep -q "UNCAUGHT:" "$log" 2>/dev/null; then verdict=throw; break; fi
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
    printf '\033[31mno verdict in %ss; last lines:\033[0m\n' "$BOARD_WAIT" >&2
    tail -25 "$log" >&2; exit 1 ;;
  throw)
    # Not "a check failed": the harness itself died, so nothing after the throw was ever
    # measured and the board says nothing about the app. Quote the throw, and say plainly
    # that the red is the harness's own.
    printf '\033[31mthe page harness threw an uncaught error; nothing after it ran:\033[0m\n' >&2
    grep -E "UNCAUGHT:" "$log" | tail -5 >&2
    tail -12 "$log" >&2
    exit 1 ;;
  *)
    grep -E "SELFTEST FAIL|selftest note" "$log" | tail -30 >&2
    exit 1 ;;
esac
