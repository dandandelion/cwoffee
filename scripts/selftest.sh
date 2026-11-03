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
#   scripts/selftest.sh --no-browser    no kept browser: the panel grades its honest "no browser" line
#   scripts/selftest.sh --speed 2       mock streams 2x faster
#   scripts/selftest.sh --keep          leave the temp dirs and the log behind
#   scripts/selftest.sh --list          name the check groups
#   scripts/selftest.sh cup             only the groups whose id or label matches (repeatable)
#
# A targeted run is the everyday one: most changes touch a line of CSS or one label, and a full pass
# costs minutes. `scripts/selftest.sh cup live-fence` runs those groups and nothing else; the filter
# also matches words from a group's own section label, so `cup` catches both cup checks and `code`
# catches the code-chrome and live-fence ones. What is *not* run is printed on the verdict line, and
# a targeted green is never a green for the suite.

set -uo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD

MOCK_PORT=${MOCK_PORT:-8190}
GW_PORT=${GW_PORT:-8199}
SPEED=1
NO_BUILD=0
# `--no-mcp` starts no third-party server. The `mcp` group needs both halves of its claim — a
# live server and a dead one — so turning this off is for debugging runs that have nothing to do
# with MCP, not for a cheaper green.
NO_MCP=0
# `--no-browser` starts no kept browser at all. The `browser-panel` group then grades the other
# half of the feature — the panel explaining, in words, that there is no browser here and what
# would put one back — so this is not a cheaper green, it is the second arm.
NO_BROWSER=0
KEEP=0
REAL=""
ONLY=""
LIST=0
# Which palette this run paints. Declared empty because the script runs under `set -u`: a flag nobody
# passed has to read as an empty value rather than an unbound variable, which is how a plain
# `./scripts/selftest.sh` died on its own theme check.
THEME=""
prev_arg=""
for a in "$@"; do
  case "$a" in
    --no-build) NO_BUILD=1 ;;
    --no-mcp) NO_MCP=1 ;;
    --no-browser) NO_BROWSER=1 ;;
    --keep) KEEP=1 ;;
    --speed) prev_arg="--speed" ; continue ;;
    --speed=*) SPEED=${a#--speed=} ;;
    --only) prev_arg="--only" ; continue ;;
    --only=*) ONLY=${a#--only=} ;;
    --list) LIST=1 ;;
    --real-endpoint) prev_arg="--real-endpoint" ; continue ;;
    --real-endpoint=*) REAL=${a#--real-endpoint=} ;;
    --mock-port) prev_arg="--mock-port" ; continue ;;
    --mock-port=*) MOCK_PORT=${a#--mock-port=} ;;
    --gateway-port) prev_arg="--gateway-port" ; continue ;;
    --gateway-port=*) GW_PORT=${a#--gateway-port=} ;;
    --theme) prev_arg="--theme" ; continue ;;
    --theme=*) THEME=${a#--theme=} ;;
    -h|--help) sed -n '2,34p' "$0"; exit 0 ;;
    --*) ;;
    # A bare word is a check group, which is what you type on a normal day: the section's own label
    # is what you remember ("cup"), not an id you have to look up. `--only cup code` works too.
    # A word that is the *value* of the flag before it is not a group: `--theme light` used to add a
    # group called "light" to the filter, so the run announced itself as TARGETED [theme,light] when
    # it had been asked for one thing.
    *)
      case "$prev_arg" in
        --theme) prev_arg=""; THEME="$a" ;;
        --speed) prev_arg=""; SPEED="$a" ;;
        --only) prev_arg=""; ONLY="${ONLY:+$ONLY,}$a" ;;
        --mock-port) prev_arg=""; MOCK_PORT="$a" ;;
        --gateway-port) prev_arg=""; GW_PORT="$a" ;;
        --real-endpoint) prev_arg=""; REAL="$a" ;;
        *) ONLY="${ONLY:+$ONLY,}$a" ;;
      esac
      ;;
  esac
  prev_arg=""
done
if [ "$LIST" = 1 ]; then
  echo "check groups (scripts/selftest.sh <id-or-word> ...):"
  grep -o "want('[a-z0-9-]*', '[^']*'" ui/src/App.svelte |
    tr -d "\"" | sed -e 's/^want(.//' -e "s/', '/  /" -e "s/.'$//" | sort -u
  exit 0
fi
case "$THEME" in
  light|dark|system) ;;
  "") ;;
  *) die "--theme must be light, dark or system (got '$THEME')" ;;
esac
if [ -n "$THEME" ]; then
  # Named on the board's own line: a green run graded in one palette says nothing about the other,
  # and a reader who cannot tell which palette ran would read it as both.
  export CWOFFEE_SELFTEST_THEME="$THEME"
fi

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
  want=$(node -e 'const c=require("node:crypto"),f=require("node:fs");process.stdout.write(c.createHash("sha256").update(f.readFileSync(process.argv[1])).digest("hex").slice(0,12))' tests/scenarios/scenarios.json)
  fp_of() {
    node -e 'const s=JSON.parse(process.argv[1]||"{}");process.stdout.write(String(s.fingerprint ?? ""))' "$1"
  }
  state=$(curl -fsS "http://127.0.0.1:${MOCK_PORT}/mock/state" 2>/dev/null || true)
  if [ -n "$state" ]; then
    got=$(fp_of "$state")
    if [ -n "$got" ] && [ "$got" = "$want" ]; then
      # Ask twice. A run that is being ended right now still answers its first probe, then its
      # server goes away with its parent's cleanup - and the next run, having "reused" it, fails on
      # the first model list with `list_models returned nothing`. Measured twice in one afternoon,
      # both times caused by ending a run by its pid and starting another four seconds later.
      sleep 1
      state2=$(curl -fsS "http://127.0.0.1:${MOCK_PORT}/mock/state" 2>/dev/null || true)
      # Three probes over ~2.5s, because a server that is draining answers while it dies. Anything
      # that stops answering in the middle of that window is not ours to reuse.
      sleep 1.5
      state3=$(curl -fsS "http://127.0.0.1:${MOCK_PORT}/mock/state" 2>/dev/null || true)
      if [ "$(fp_of "$state2")" = "$want" ] && [ "$(fp_of "$state3")" = "$want" ]; then
        note "port ${MOCK_PORT} answers as a mock-llm with the CURRENT scenarios.json (fingerprint $got) - reusing it"
        MOCK_REUSED=1
      else
        note "port ${MOCK_PORT} answered once and then stopped answering - that server was on its way out; starting our own"
      fi
    else
      pid=$(ss -ltnp "sport = :${MOCK_PORT}" 2>/dev/null | grep -o 'pid=[0-9]*' | head -1 | cut -d= -f2)
      die "port ${MOCK_PORT} answers as a mock-llm with a stale or unknown scenario set (it says ${got:-no fingerprint}, the file says $want). End that one server by its pid (${pid:-not found from ss}) — do not pattern-kill by name, something else may answer to it — or set MOCK_PORT=…"
    fi
  fi
  if [ "${MOCK_REUSED:-0}" != 1 ]; then
    # Two runs back to back, or a run started while an older one is still winding down, and the port
    # is taken by a server this script does not own. Refusing to listen is not fatal - the health
    # probe answers happily from the *other* server, which is exactly the failure that let one run
    # test against scenarios it had never read. So: start ours, and only accept the port if the
    # answer carries the fingerprint of the file we are looking at. If it does not, move up a port.
    for try in 0 1 2 3; do
      port=$((MOCK_PORT + try))
      [ "$try" -gt 0 ] && note "port ${port} - trying it (the one before answers with someone else's scenarios)"
      note "starting mock endpoint on 127.0.0.1:${port} (speed x${SPEED})"
      node scripts/mock-llm.mjs --port "$port" --speed "$SPEED" >>"$log" 2>&1 &
      MOCK_PID=$!
      for _ in $(seq 1 25); do
        kill -0 "$MOCK_PID" 2>/dev/null || break
        curl -fsS "http://127.0.0.1:${port}/health" >/dev/null 2>&1 && break
        sleep 0.2
      done
      state=$(curl -fsS "http://127.0.0.1:${port}/mock/state" 2>/dev/null || true)
      if [ -n "$state" ] && [ "$(fp_of "$state")" = "$want" ] && kill -0 "$MOCK_PID" 2>/dev/null; then
        MOCK_PORT=$port
        ENDPOINT="http://127.0.0.1:${MOCK_PORT}/v1"
        break
      fi
      # Not ours, or never bound. End the process this loop started, by its pid, and move on.
      kill "$MOCK_PID" 2>/dev/null
      MOCK_PID=""
    done
    [ -n "$MOCK_PID" ] || die "no port from ${MOCK_PORT} on would host this run's mock endpoint (see $log)"
  fi
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
  # A third-party MCP server for the `mcp` group, and it is deliberately *this* run's own:
  # scripts/fake-mcp-server.mjs is cwoffee's test double, and `--writes` points at the scratch
  # workspace so a group can read the file the server wrote off disk instead of trusting a card.
  # `dead` is the second one: it answers tools/list and exits, so the catalog is real and the
  # *call* is what fails — which is the state a real server reaches when it crashes later.
  # Env rather than the config file, so the suite never has to write a config just to get a
  # server - and because the variable replaces the file's [mcp.servers] list wholesale, it makes
  # the servers page read-only, which is what a window whose list comes from the environment has to
  # be. The page's write path is graded by the "mcp write" arm below, in its own scratch config.
  # With neither the app starts no servers, so nothing outside this scratch run can inherit them.
  if [ "$NO_MCP" = 0 ]; then
    export CWOFFEE_MCP_SERVERS=$(python3 -c "
import json, os
print(json.dumps({
    'fake': {'command': 'node', 'args': ['$ROOT/scripts/fake-mcp-server.mjs', '--tools', '3', '--writes', '$SCRATCH/ws'], 'enabled': True},
    'dead': {'command': 'node', 'args': ['$ROOT/scripts/fake-mcp-server.mjs', '--tools', '2', '--die'], 'enabled': True},
}))")
  fi
  TOKEN=$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')
else
  SCRATCH=""
  TOKEN=${CWOFFEE_GATEWAY_TOKEN:-}
  [ -n "$TOKEN" ] || die "--real-endpoint runs against your real config; set CWOFFEE_GATEWAY_TOKEN to reuse the running gateway, or unset CWOFFEE_GATEWAY_PORT"
fi
# The kept browser runs **headless** on this board. Not because the panel needs headless — a
# screenshot comes from the page either way — but because a run that measures window geometry has
# no business putting a second real window on the desktop and then explaining the numbers it got.
# On a person's desktop the same feature opens a real window; `browser_status`/`browser_panel` say
# which mode a start is in, so the board is not pretending.
export CWOFFEE_BROWSER_SESSION_HEADLESS=1
# ...and ask for the headless-shell build rather than full Chromium in its headless mode: the
# shell build never opens an X connection, so a kept browser cannot disturb the compositing of the
# very window this run is measuring. `[browser] headed` still wins for a person; this is the
# harness saying "do not paint".
export CWOFFEE_BROWSER_HEADLESS_ONLY=1
# `--no-browser` turns keeping one off, in the person's own switch (`[browser] keep = false`),
# which is the arm that grades the panel saying so instead of offering a box that can only fail.
if [ "$NO_BROWSER" = 1 ]; then export CWOFFEE_BROWSER_KEEP=no; fi
# The kept browser has two shapes, and one process can only be in one of them: a column showing
# pictures of a browser that runs in another process, or a page this app paints into the panel's own
# box. The groups name which they grade — `browser-panel` the first, `browser-inpage` the second — so
# the board starts the shape the requested groups need and says which it chose. A full pass runs the
# column shape on the main board and relaunches a second board for the in-page arm, the same way the
# idle arm gets a config of its own.
INPAGE_ONLY=0
if [ -n "$ONLY" ]; then
  other=0; inpage=0
  for g in $ONLY; do case "$g" in browser-inpage) inpage=1 ;; *) other=1 ;; esac; done
  [ "$inpage" = 1 ] && [ "$other" = 0 ] && INPAGE_ONLY=1
fi
if [ "$NO_BROWSER" = 0 ] && [ "$INPAGE_ONLY" = 1 ]; then
  export CWOFFEE_BROWSER_PAGE=yes
else
  export CWOFFEE_BROWSER_PAGE=no
  # ... and this launch stands down the in-page group, whatever else it was asked for. Failing it
  # here was the accident that made a full pass red and then skipped the relaunch below, which is
  # the launch that actually grades it: a check cannot be allowed to fail a shape it was never
  # started in.
  INPAGE_WANT=0
  if [ -n "$ONLY" ]; then
    for g in $ONLY; do case "$g" in browser-inpage) INPAGE_WANT=1 ;; esac; done
  else
    INPAGE_WANT=1
  fi
  if [ "$INPAGE_WANT" = 1 ]; then export CWOFFEE_SELFTEST_SKIP=browser-inpage; fi
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
if [ "$NO_MCP" = 0 ]; then
  note "mcp servers: fake (live, writes into $SCRATCH/ws) + dead (answers the list, then exits)"
else
  note "mcp servers: none (--no-mcp; the mcp group grades the honest no-server path only)"
fi
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
if [ "$NO_BROWSER" = 0 ]; then
  if [ "${CWOFFEE_BROWSER_PAGE:-no}" = yes ]; then
    note "browser    : this board keeps the page **in the window** (CWOFFEE_BROWSER_PAGE=yes). 'browser-panel' would be grading a picture column that is not running here; run it separately."
  else
    note "browser    : this board keeps a browser in another process and shows its pictures. The page-in-the-window shape is graded by 'browser-inpage', which gets its own launch; this launch stands it down rather than failing it."
  fi
fi
if [ "${CWOFFEE_CORE:-rust}" = pi ] && [ -z "${CWOFFEE_CWTOOL:-}" ]; then
  note "toolhost   : not set — this run will build and use its own target/release/cwtool"
elif [ "${CWOFFEE_CORE:-rust}" = pi ]; then
  note "toolhost   : ${CWOFFEE_CWTOOL} (from the environment; this run does NOT rebuild it)"
fi
note "log        : $log"
# Which palette this run paints, said on the header. One run can only be in one palette, so "the
# floating surfaces are graded in both" is two arms: the detached probes inside the `theme` group
# resolve both palettes' tokens in either run, and the *live* paint claim needs the run to be in the
# palette it is asserting. A full pass therefore launches the light arm of that group afterwards.
if [ -n "$THEME" ]; then
  note "palette    : this run paints '$THEME' (CWOFFEE_SELFTEST_THEME); the live-paint claims are about that palette only"
elif [ -z "$ONLY" ]; then
  note "palette    : this run paints whatever the host stores; the light arm of the surface groups runs afterwards as its own launch"
fi
if [ -n "$ONLY" ]; then
  export CWOFFEE_SELFTEST_ONLY="$ONLY"
  note "checks     : TARGETED [$ONLY] - the other groups are NOT running, so a green here covers these groups only"
  note "           : the whole suite is: scripts/selftest.sh (no arguments); names: scripts/selftest.sh --list"
else
  note "checks     : every group (a full pass)"
fi

cleanup() {
  # Only the instance this script started. The user may be running the real app
  # right now, and a test script has no business closing it; the scratch config
  # and data dirs are why two instances can coexist.
  # The idle keeper goes back before anything is deleted: it restores the session's own
  # idle-delay on this signal, and a keeper that is left running is a desktop that never
  # blanks. Ended by the pid it printed, never by a name pattern - the launching shell has
  # the script's text in its own command line.
  if [ -n "${KEEPER_PID:-}" ]; then
    kill "$KEEPER_PID" >/dev/null 2>&1
    for i in 1 2 3 4 5; do kill -0 "$KEEPER_PID" >/dev/null 2>&1 || break; sleep 0.2; done
  fi
  [ -n "${APP_PID:-}" ] && kill "$APP_PID" >/dev/null 2>&1
  [ -n "${MOCK_PID:-}" ] && kill "$MOCK_PID" >/dev/null 2>&1
  # Wait for the mock this run started. It drains in-flight requests before exiting, so the port can
  # keep answering for a moment after the kill - and the next run's fingerprint probe then "reuses"
  # a server that is on its way out, launches an app, and fails on the first model list with
  # `list_models returned nothing`. Seen twice: both times a run was ended by its pid and another
  # started seconds later.
  if [ -n "${MOCK_PID:-}" ]; then
    for i in 1 2 3 4 5 6 7 8 9 10; do
      kill -0 "$MOCK_PID" >/dev/null 2>&1 || break
      sleep 0.3
    done
  fi
  # The clipboard goes back before anything is deleted: a test that quietly eats
  # what you had copied is a test that costs you an afternoon of work.
  if [ "${CLIP_ARMED:-0}" = 1 ] && [ -d "${SCRATCH:-}/clip" ]; then
    if [ -s "$SCRATCH/clip/image.png" ]; then
      timeout 6 wl-copy --type image/png < "$SCRATCH/clip/image.png" >/dev/null 2>&1
    elif [ -s "$SCRATCH/clip/text.txt" ]; then
      timeout 6 wl-copy < "$SCRATCH/clip/text.txt" >/dev/null 2>&1
    else
      timeout 6 wl-copy --clear >/dev/null 2>&1
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
  now_hash=$(timeout 5 wl-paste -n -t image/png 2>/dev/null | sha256sum | cut -d' ' -f1)
  if [ -n "$now_hash" ] && [ "$now_hash" = "$fixture_hash" ]; then
    if [ -s "$prev_img" ]; then
      timeout 6 wl-copy --type image/png < "$prev_img" >/dev/null 2>&1
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
# The terminal is a third artifact, and the gate below runs *this tree's* binary: the same stale
# -artifact trap that once made a board grade hours-old bridge code would make a TUI gate grade
# whatever `cwoffee` was left in target/release.
tui_inputs() { printf '%s\n' crates/tui crates/engine Cargo.toml Cargo.lock; }
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
  # `cwoffee-tui` rides along: the `tui` group drives target/release/cwoffee, and a board that
  # builds only the app would grade whatever terminal binary an older run left in the target dir.
  CARGO_BUILD_JOBS=1 ./scripts/cargo-guard.sh --max-mb 3000 cargo build --release -p cwoffee-app -p cwoffee-gateway -p cwoffee-bridge -p cwoffee-tui -j 1 >>"$log" 2>&1 \
    || die "cargo build failed (see $log)"
  # "Finished" is not evidence. Cargo has been seen to recompile the engine and the bridge
  # and leave `cwoffee-app` alone, so the app keeps an embedded bridge from hours ago and the
  # board reports red for behaviour this tree no longer has. Compare the artifact against the
  # sources, force it once if it lost, and refuse to run if it loses twice.
  stale=$(newer_than target/release/cwoffee-app $(app_inputs))
  if [ -n "$stale" ]; then
    note "target/release/cwoffee-app is older than $stale — the build said finished without relinking the app; forcing one relink"
    touch crates/tauri-app/src/main.rs
    CARGO_BUILD_JOBS=1 ./scripts/cargo-guard.sh --max-mb 3000 cargo build --release -p cwoffee-app -p cwoffee-gateway -p cwoffee-bridge -p cwoffee-tui -j 1 >>"$log" 2>&1 \
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

# ---- group tui: the terminal's own gate -------------------------------------
# The board above grades a window. The terminal shares the same engine and had no check of any
# kind, which is how `checkpoint_store: true` came to mean "this turn copies files nobody can list"
# and `[mcp]` came to mean "read by nobody in the TUI". `scripts/tui-gate.mjs` drives the built
# binary over its own pty and grades the disk, the store and the mock endpoint's request log.
#
# Is the filter asking for it? A bare word matches the group, and a full pass runs it too.
TUI_WANT=1
if [ -n "$ONLY" ]; then
  case ",$ONLY," in
    *,*tui*,*) TUI_WANT=1 ;;
    *) TUI_WANT=0 ;;
  esac
fi
TUI_FAIL=0
if [ "$TUI_WANT" = 1 ]; then
  [ -x target/release/cwoffee ] || die "no target/release/cwoffee for the tui group (built above, in the same line as the app)"
  stale=$(newer_than target/release/cwoffee $(tui_inputs))
  [ -z "$stale" ] || die "target/release/cwoffee is older than $stale: the gate would grade a terminal this tree did not build. Rebuild: cargo build --release -p cwoffee-tui"
  note "tui binary : $(date -r target/release/cwoffee '+%m-%d %H:%M:%S') ($PWD/target/release/cwoffee)"
  note "tui gate   : driving target/release/cwoffee on its own pty against its own mock endpoint on $((MOCK_PORT + 40)) (never 8090; the board's mock on ${MOCK_PORT} is left alone)"
  if [ "$NO_MCP" = 1 ]; then
    note "           : --no-mcp, so the gate grades the no-servers arm only and says so"
  fi
  tui_out=$(CWOFFEE_MCP_SERVERS="" CWOFFEE_GATEWAY_PORT="" CWOFFEE_GATEWAY_TOKEN="" \
    CARGO_BUILD_JOBS=1 ./scripts/cargo-guard.sh --max-mb 3000 \
    node scripts/tui-gate.mjs --bin "$PWD/target/release/cwoffee" --mock "$((MOCK_PORT + 40))" \
      --db /tmp/cwoffee-tui-gate.$$ --speed "$SPEED" 2>&1)
  printf '%s\n' "$tui_out" >>"$log"
  if printf '%s\n' "$tui_out" | grep -q '^TUI GATE ok$'; then
    note "tui gate   : ok - $(printf '%s\n' "$tui_out" | grep -c '^  ok: ') claim(s) graded (the lines are in $log)"
  else
    TUI_FAIL=1
    printf '\033[31m[selftest.sh] tui gate FAILED:\033[0m\n' >&2
    printf '%s\n' "$tui_out" | grep -E '^  FAIL:|^TUI GATE' >&2
  fi
  # `tui` alone means "grade the terminal", and nothing else on this board says anything about it.
  # Launching a window to print a verdict about groups that were not selected would take minutes
  # and report on code the run was never asked to exercise.
  if [ -n "$ONLY" ]; then
    page_only=1
    for tok in $(printf '%s' "$ONLY" | tr ',' ' '); do
      case "$tok" in *tui*) ;; *) page_only=0 ;; esac
    done
    if [ "$page_only" = 1 ]; then
      if [ "$TUI_FAIL" = 0 ]; then
        note "verdict: tui ok (the page groups did not run; this says nothing about the window)"
        exit 0
      fi
      note "verdict: tui FAILED (the page groups did not run)"
      exit 1
    fi
  fi
fi

# No pkill here: this run owns its own config, data dir, workspace and gateway
# port, so it coexists with an app the user already has open.

# ---- keep the desktop painting for the length of the run -------------------
# The page checks pace themselves on requestAnimationFrame. When the session goes idle the
# screen blanks, the compositor stops painting the app window, and rAF goes silent: every
# frame-paced check then falls back to its timeout floor, the run stretches to twice its
# length, and the checks that need a real paint skip instead of asserting. This run hit that:
# sixteen minutes of silence after the loop check, and the cup-band check never ran.
#
# The lever is the desktop's own idle switch through the desktop's own API. See the header of
# scripts/idlekeep.py for why it is not an X client (on this Wayland session an X client that
# fakes input is a remote-desktop request, and the portal answers with a consent dialog in
# front of the window the script was trying to keep painted). DPMS is not touched, the lock
# screen is not touched, and no input is injected. The keeper restores the value it took down,
# including when a previous keeper was killed and never got to.
KEEPER_PID=""
if command -v gsettings >/dev/null 2>&1; then
  python3 -u scripts/idlekeep.py >"$SCRATCH/idlekeep.out" 2>&1 &
  for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
    KEEPER_PID=$(sed -n 's/^pid \([0-9]\{1,9\}\)$/\1/p' "$SCRATCH/idlekeep.out" | head -1)
    [ -n "$KEEPER_PID" ] && break
    sleep 0.2
  done
  note "idle keeper: $(grep -v '^pid ' "$SCRATCH/idlekeep.out" | head -1)"
  [ -n "$KEEPER_PID" ] && note "idle keeper: pid $KEEPER_PID (ended by that number, and it puts idle-delay back on the way out)"
else
  note "idle keeper: no gsettings on this desktop; the session's own blanking is left alone"
fi

# One desktop, one visible window. The app re-activates its own window when it loses focus during
# a self-test (docs/TESTING.md), and on Wayland no script can raise another client's window - so a
# `target/release/cwoffee-app` left running by an earlier, killed run keeps the compositor, and
# this run ends 40 minutes later with "N geometry checks were skipped: the window was off-screen
# for most of this run". That message blames the desktop for somebody's stray process, so the
# process gets named here instead, before the run spends 40 minutes on it.
#
# Only this tree's own artifact is looked for. A person's installed app
# (~/.local/share/cwoffee-desktop/bin/cwoffee-app) is theirs to keep open, and the board is built
# to coexist with it - the scratch config, data and state dirs are why two instances can run at
# once. And nothing here is *killed*: a process this script did not start is not this script's to
# end, and a developer running the app by hand from the target dir is doing exactly that on purpose.
leftover=$(python3 - <<'PYS' 2>/dev/null
import os
me, out = os.getpid(), []
for e in os.listdir('/proc'):
    if not e.isdigit() or int(e) == me:
        continue
    try:
        argv = open('/proc/%s/cmdline' % e, 'rb').read().split(b'\0')
    except OSError:
        continue
    # argv[0..1] only: the launching shell carries this script's text - including this very path -
    # in its own command line, and scanning the whole line would find that and report itself.
    for a in argv[:2]:
        v = a.decode(errors='replace')
        if v.endswith('target/release/cwoffee-app'):
            out.append(int(e))
            break
print(' '.join(str(x) for x in sorted(set(out))))
PYS
)
if [ -n "$leftover" ]; then
  die "an app from target/release is already running (pid ${leftover// /, }) and this run did not start it. Two self-test windows on one desktop cannot both be the painted one: the other window holds the compositor, and this run would end with 'the window was off-screen for most of this run' and no layout proved. End that pid (kill ${leftover%% *}) or point the board at another desktop; this script does not end processes it did not start."
fi

note "launching the app"
# The paste check needs an image on the selection. Whatever was there is saved
# first and put back by cleanup(), in the same order it was found.
if [ "$CLIP_TOOL" = 1 ]; then
  mkdir -p "$SCRATCH/clip"
  # Every wl-paste call here gets a timeout. `wl-paste` blocks forever when the selection's owner is
  # gone - which is exactly the state a killed self-test app leaves behind, because the app that had the
  # clipboard died without giving it up. Without the timeout the run stops *before launching the app*,
  # and the log's last line is "launching the app": a harness stall that reads like an app crash.
  timeout 3 wl-paste -l 2>/dev/null | grep -qx 'image/png' && timeout 5 wl-paste -n -t image/png > "$SCRATCH/clip/image.png" 2>/dev/null
  timeout 3 wl-paste -l 2>/dev/null | grep -qx 'text/plain' && timeout 5 wl-paste -n -t text/plain > "$SCRATCH/clip/text.txt" 2>/dev/null
  # Outlive $SCRATCH: cleanup() deletes that directory, and a run that survives to
  # cleanup() does not need this copy. A run that does not reach cleanup() does.
  cp "$SCRATCH/clip/image.png" "$CLIP_BACKUP_IMG" 2>/dev/null || true
  cp "$SCRATCH/clip/text.txt" "$CLIP_BACKUP_TXT" 2>/dev/null || true
  # The check compares the pasted bytes against a file at an agreed path. The app
  # cannot see the harness fixture directory, so the fixture is staged where both
  # sides know to look - otherwise the byte comparison silently does nothing.
  cp tests/fixtures/clipboard.png /tmp/cwoffee-selftest-fixture.png 2>/dev/null || true
  if timeout 6 wl-copy --type image/png < tests/fixtures/clipboard.png 2>/dev/null; then
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
    # Not armed, so the app is told there is no fixture (`CWOFFEE_SELFTEST_CLIP_IMG` stays empty)
    # and the check reports its own skip. A desktop whose clipboard is wedged - a killed app still
    # owning the selection does exactly this - must cost one check, not the run.
    note "clipboard  : wl-copy would not take the fixture (or the clipboard is wedged); the paste check will say it was skipped"
  fi
fi
# Two keys arm the harness: a self-test bundle *and* an armed process. See
# commands.rs::selftest_armed - the release binary embeds whatever ui/dist holds,
# so a production start right after this script unarms the same bundle and must
# not drive the user's endpoint, config or session store.
# WebKitGTK's accelerated compositing drives `requestAnimationFrame` off the compositor's frame
# callbacks, and mutter stops sending those as soon as it decides it is not showing the window. On
# this Wayland desktop an app has no legal way to insist on being shown: a plain present is refused,
# a minimize/restore is refused, `set_always_on_top` is ignored, and the log says `focused=false`
# however often we ask. Measured in the same run both ways: accelerated compositing gave 0 frames
# across 76 frame waits and forced 13 layout checks to be skipped; with compositing disabled the run
# had 0 stalls and every layout check ran. The frame-paced numbers then describe JS and layout cost
# instead of paint scheduling, which is what those checks are actually claiming. This is set for the
# armed run only - a production start never sees it.
CWOFFEE_SELFTEST=1 CWOFFEE_SELFTEST_CLIP_IMG="${CLIP_FIXTURE:-}" CWOFFEE_SELFTEST_ONLY="${CWOFFEE_SELFTEST_ONLY:-}" WEBKIT_DISABLE_COMPOSITING_MODE=1 ./target/release/cwoffee-app >>"$log" 2>&1 &
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
  if grep -qE "UNCAUGHT:|REJECTION:" "$log" 2>/dev/null; then verdict=throw; break; fi
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
        // This block reports; it must not be able to end the run. An empty stdin means curl got
        // nothing (the mock is already down, or the port moved), and an uncaught throw here used to
        // take the script down before it printed the verdict - a truncated run that reads as a hang.
        let j;
        try { j = JSON.parse(s) } catch (e) {
          console.log(`  the mock state endpoint answered with nothing parseable (${JSON.stringify(s.slice(0, 90))}); its counts are unknown, not zero`);
          return;
        }
        const hits=Object.entries(j.hits??{}).sort((a,b)=>b[1]-a[1]);
        console.log(`  requests ${j.requests}, turns served ${j.turns}, thinking-off ${j.thinking_off}, thinking-on ${j.thinking_on}`);
        // The mid-turn witness: what the app actually put in front of the model as a user row, and
        // what the steer assertions found. A green `steer` group means these moved; a run that
        // claimed a steer landed while the endpoint never saw one shows up here.
        console.log(
          `  mid-turn presses: ${j.steer_hits?.length ?? 0} landed, ` +
            `${j.steer_misses?.length ?? 0} required and missing, ${j.steer_leaks?.length ?? 0} withdrawn-but-sent, ` +
            `${j.user_seen?.length ?? 0} user message(s) seen`
        );
        for (const m of j.steer_misses ?? []) console.log(`    STEER MISSING: ${m}`);
        for (const l of j.steer_leaks ?? []) console.log(`    STEER LEAKED: ${l}`);
        for (const [n,c] of hits) console.log(`  ${String(c).padStart(3)}x ${n}`);
        if ((j.misses??[]).length) { console.log(`  MISSES (${j.misses.length}):`); for (const m of j.misses) console.log(`    - ${m}`); }
        else console.log("  no unmatched prompts: every turn the harness sent had a scenario");
      });' >>"$log" 2>&1
  sed -n '/mock endpoint activity:/,$p' "$log"
fi

echo
# ---- the light arm of the surface groups ------------------------------------
# One launch paints one palette. The `theme` group resolves both palettes' tokens from detached
# probes in either run, but its live-paint claim — the one a hard-coded dark token fails — needs the
# window to actually be painted in the palette it asserts. So a full pass, and a pass that asked for
# neither palette, gets a second launch narrowed to that group in the light palette.
THEME_FAIL=0
if [ -z "$THEME" ] && [ -z "$ONLY" ] && [ -x target/release/cwoffee-app ]; then
  # The main app goes first, by the pid this script printed. Two instances would share the scratch
  # config, the scratch record and the scratch gateway port, and the second one would be graded
  # against a store the first is still writing to.
  if [ -n "${APP_PID:-}" ]; then
    kill "$APP_PID" >/dev/null 2>&1
    for i in 1 2 3 4 5 6 7 8; do kill -0 "$APP_PID" >/dev/null 2>&1 || break; sleep 0.5; done
    note "theme arm  : the board's app (pid $APP_PID) is ended; the light arm needs the scratch homes to itself"
  fi
  note "theme arm  : relaunching for the light palette, group 'theme' only (one run paints one palette)"
  tllog=/tmp/cwoffee-selftest-theme-light-$$.log
  # Its own scratch homes, and its own gateway port. Pointing the arm at the board's directories
  # makes it inherit ~68 conversations from the run that just ended, and the chain's unnamed setup
  # blocks - history fill, the loop case, the sheet turns - then fail on a store they did not
  # start: `history fill pass 1 stored 3 rows (expected ~56): session 69 holds 3 rows total`. That
  # is not a red about the light palette; it is an arm graded against somebody else's record.
  ARM=$(mktemp -d /tmp/cwoffee-theme-arm.XXXXXX)
  mkdir -p "$ARM/config" "$ARM/data" "$ARM/state" "$ARM/ws" "$ARM/pi"
  XDG_CONFIG_HOME="$ARM/config" XDG_DATA_HOME="$ARM/data" XDG_STATE_HOME="$ARM/state" \
    CWOFFEE_WORKSPACE="$ARM/ws" CWOFFEE_PI_AGENT_DIR="$ARM/pi" \
    CWOFFEE_GATEWAY_PORT=$((GW_PORT + 1)) CWOFFEE_MCP_SERVERS="" \
    CWOFFEE_SELFTEST=1 CWOFFEE_SELFTEST_ONLY=theme CWOFFEE_SELFTEST_THEME=light \
    WEBKIT_DISABLE_COMPOSITING_MODE=1 ./target/release/cwoffee-app >>"$tllog" 2>&1 &
  THEME_PID=$!
  tv=""
  # The pi core takes longer to boot a model process and to run a group's turns than the in-process
  # engine does, so the arm's clock is the core's, not a constant. A timeout here is reported as a
  # red arm, and a red that is really "too slow to finish" is the failure this script already refuses
  # to print in the board's own verdict path.
  arm_wait=120
  [ "${CWOFFEE_CORE:-rust}" = pi ] && arm_wait=300
  for _ in $(seq 1 "$arm_wait"); do
    grep -q "SELFTEST ok\|SELFTEST FAILED" "$tllog" 2>/dev/null && { tv=$(grep -o "SELFTEST ok\|SELFTEST FAILED" "$tllog" | head -1); break; }
    kill -0 "$THEME_PID" 2>/dev/null || { tv="gone"; break; }
    sleep 1
  done
  kill "$THEME_PID" 2>/dev/null
  for i in 1 2 3 4 5 6; do kill -0 "$THEME_PID" >/dev/null 2>&1 || break; sleep 0.5; done
  # Ended by the pid the script printed, and the arm's log stays in /tmp for the reader.
  if [ "$tv" = "SELFTEST ok" ]; then
    note "theme arm  : ok - the light palette holds its own numbers ($(grep -c 'selftest note' "$tllog") note(s) in $tllog)"
  else
    THEME_FAIL=1
    printf '\033[31m[selftest.sh] the light palette arm %s (log: %s):\033[0m\n' "${tv:-timed out}" "$tllog" >&2
    grep -E "SELFTEST FAIL|theme ok|selftest note" "$tllog" | tail -8 >&2
  fi
  # Keep the log when the arm went red: the line above quotes the tail, and the reader needs the
  # whole thing to tell "the light palette is wrong" from "the arm never finished".
  if [ "$THEME_FAIL" = 0 ]; then rm -f "$tllog"; fi
  # The arm's scratch is its own, so it goes away here rather than waiting for cleanup(). Kept with
  # --keep, the same way the board's scratch is kept.
  [ "$KEEP" = 0 ] && rm -rf "$ARM"
fi

if [ "$THEME_FAIL" = 1 ] && [ "$verdict" = ok ]; then
  verdict=fail
  note "the page board is green, but the light-palette arm is red - see the 'theme arm' lines above"
fi

# The terminal's gate is part of the verdict and not a footnote: a board that printed "ok" while
# /undo left a file holding the turn's words would be a board grading one shell and calling it two.
if [ "$TUI_FAIL" = 1 ] && [ "$verdict" = ok ]; then
  verdict=fail
  note "the page board is green, but group tui is red - see the 'tui gate FAILED' lines above"
fi

# ---- the write arm of the servers page --------------------------------------
#
# The board's own run names its MCP servers through CWOFFEE_MCP_SERVERS, because the suite should not
# have to write a config file to get a server. That is the right choice for the `mcp` group - and it
# makes the servers page read-only *on purpose*: the variable replaces the file's [mcp.servers] list
# wholesale, so a form that wrote the file would report a success the next start ignores. The page has
# to refuse, and the main run grades that refusal.
#
# Which leaves the write path ungraded, and it is the half a person touches: add, switch off, switch
# on, remove. Those need a window whose servers come from a file, so a full pass (and any pass that
# asked for the servers by name) gets a second launch: its own scratch homes, its own gateway port,
# and a config.toml listing the same two test doubles. Everything it writes lands in a scratch tree
# that is deleted when the run ends - never in the config of whoever ran this script.
#
# What it grades is the pair, not one of them: the file the next start will read (read back through
# the host's own path) and the hub that answers a call, after each step. A page that only rewrote a
# file, and a page that only poked a running set, both fail it.
MCP_FAIL=0
mcp_want=0
if [ -z "$ONLY" ]; then mcp_want=1; else
  for g in $ONLY; do case "$g" in mcp*) mcp_want=1 ;; esac; done
fi
if [ "$NO_MCP" = 0 ] && [ "$mcp_want" = 1 ] && [ -x target/release/cwoffee-app ]; then
  if [ -n "${APP_PID:-}" ]; then
    kill "$APP_PID" >/dev/null 2>&1
    for i in 1 2 3 4 5 6 7 8; do kill -0 "$APP_PID" >/dev/null 2>&1 || break; sleep 0.5; done
    note "mcp write  : the board's app (pid $APP_PID) is ended; the write arm needs its own config and record"
  fi
  mllog=/tmp/cwoffee-selftest-mcp-write-$$.log
  MARM=$(mktemp -d /tmp/cwoffee-mcp-arm.XXXXXX)
  # Named once the directory exists: the script runs with `set -u`, and a note that quotes $MARM
  # before it is assigned kills the whole run at that line.
  note "mcp write  : relaunching with the servers in a scratch config, so the page may write (group 'mcp-ui' only)"
  # XDG_CONFIG_HOME, then the app's own folder inside it: the file the app reads at boot is
  # $XDG_CONFIG_HOME/cwoffee/config.toml, and the same one every settings write goes back to. Put it
  # anywhere else and the arm boots with no servers while still writing a file the *next* start reads
  # - which is a red that looks like a broken page instead of a broken arm.
  mkdir -p "$MARM/config/cwoffee" "$MARM/data" "$MARM/state" "$MARM/ws" "$MARM/pi"
  cat > "$MARM/config/cwoffee/config.toml" <<TOML
# scripts/selftest.sh wrote this for the servers-page write arm, in a scratch home it deletes on
# exit. The board's own run uses CWOFFEE_MCP_SERVERS and grades the read-only arm; this one exists so
# the page's add / switch / remove path has a file to write that is not anybody's real config.
[mcp]
enabled = true
timeout_secs = 20

[mcp.servers.fake]
command = "node"
args = ["$ROOT/scripts/fake-mcp-server.mjs", "--tools", "3", "--writes", "$MARM/ws"]
enabled = true

[mcp.servers.dead]
command = "node"
args = ["$ROOT/scripts/fake-mcp-server.mjs", "--tools", "2", "--die"]
enabled = true
TOML
  note "mcp write  : $MARM/config/cwoffee/config.toml lists fake (live) and dead (answers, then exits); the same doubles the board's run supplies by environment"
  XDG_CONFIG_HOME="$MARM/config" XDG_DATA_HOME="$MARM/data" XDG_STATE_HOME="$MARM/state" \
    CWOFFEE_WORKSPACE="$MARM/ws" CWOFFEE_PI_AGENT_DIR="$MARM/pi" \
    CWOFFEE_GATEWAY_PORT=$((GW_PORT + 2)) CWOFFEE_MCP_SERVERS="" \
    CWOFFEE_SELFTEST=1 CWOFFEE_SELFTEST_ONLY=mcp-ui \
    WEBKIT_DISABLE_COMPOSITING_MODE=1 ./target/release/cwoffee-app >>"$mllog" 2>&1 &
  MCP_PID=$!
  mv=""
  # No turns in this group: it is verbs, page clicks and a round trip through a file. The clock is
  # still the core's, because the pi core boots a model process before it will run a group at all.
  mwait=150
  [ "${CWOFFEE_CORE:-rust}" = pi ] && mwait=300
  for _ in $(seq 1 "$mwait"); do
    grep -q "SELFTEST ok\|SELFTEST FAILED" "$mllog" 2>/dev/null && { mv=$(grep -o "SELFTEST ok\|SELFTEST FAILED" "$mllog" | head -1); break; }
    kill -0 "$MCP_PID" 2>/dev/null || { mv="gone"; break; }
    sleep 1
  done
  kill "$MCP_PID" 2>/dev/null
  for i in 1 2 3 4 5 6; do kill -0 "$MCP_PID" >/dev/null 2>&1 || break; sleep 0.5; done
  if [ "$mv" = "SELFTEST ok" ]; then
    note "mcp write  : ok - $(grep -c 'mcp page ok' "$mllog") arm(s) of the servers page graded against a config file it wrote ($(grep -o 'mcp page ok ([^)]*' "$mllog" | head -1))"
  else
    MCP_FAIL=1
    printf '\033[31m[selftest.sh] the servers write arm %s (log: %s):\033[0m\n' "${mv:-timed out}" "$mllog" >&2
    grep -E "SELFTEST FAIL|mcp page ok|selftest note" "$mllog" | tail -10 >&2
  fi
  [ "$MCP_FAIL" = 0 ] && rm -f "$mllog"
  [ "$KEEP" = 0 ] && rm -rf "$MARM"
fi

# ---------------------------------------------------------------------------
# The kept browser's idle clock, in its own launch.
#
# The span is a person's setting — `[browser] idle_secs`, 5 minutes if they say nothing — and a
# check that waits five minutes is a check nobody runs. So this arm boots a second window over a
# scratch config file that says four seconds, and grades what the panel says when the clock runs
# out: that it closed itself, naming the number the config file holds, and that the picture goes
# with it. The main run cannot grade this, because the board's own config is not this run's to
# rewrite — and "it closes itself" is exactly the claim that has to be graded, since the whole
# answer to "will this leave a browser running on my machine?" is that clock.
#
# After it ends, the arm asks the process table the same question in the other direction: is any
# `browser/session.mjs` still alive? An app that closes its browser on a timeout and leaks one on
# exit has not answered the objection.
# ---------------------------------------------------------------------------
BR_FAIL=0
br_want=0
if [ -z "$ONLY" ]; then br_want=1; else
  for g in $ONLY; do case "$g" in browser*) br_want=1 ;; esac; done
fi
# These arms get a start of their own, which is also why they do not wait for the main verdict: a
# board that went red for an unrelated reason (a window that spent the run off-screen) used to cancel
# every arm below it, and a suite that quietly skips its arms is a suite whose green means less than
# its red. Each arm reports its own result; the final line adds them up.
# The in-page arm needs a board whose page **is** the window. The main board is the column shape (it
# is the one `browser-panel` and the geometry groups are calibrated against), so this arm gets its
# own launch with the one switch that changes the shape — the same reason the idle arm gets a config
# with a short span in it.
INPAGE_WANT=0
if [ -z "$ONLY" ]; then INPAGE_WANT=1; else
  for g in $ONLY; do case "$g" in browser-inpage) INPAGE_WANT=1 ;; esac; done
fi
if [ "$NO_BROWSER" = 0 ] && [ "$INPAGE_WANT" = 1 ] && [ "${CWOFFEE_BROWSER_PAGE:-no}" != yes ] && [ -x target/release/cwoffee-app ]; then
  if [ -n "${APP_PID:-}" ]; then
    kill "$APP_PID" >/dev/null 2>&1
    for i in 1 2 3 4 5 6 7 8; do kill -0 "$APP_PID" >/dev/null 2>&1 || break; sleep 0.5; done
    note "in-page arm: the board's app (pid $APP_PID) is ended; the arm needs a start of its own shape"
  fi
  plog=/tmp/cwoffee-selftest-inpage-$$.log
  PARM=$(mktemp -d /tmp/cwoffee-inpage-arm.XXXXXX)
  mkdir -p "$PARM/config/cwoffee" "$PARM/data" "$PARM/state" "$PARM/ws" "$PARM/pi"
  note "in-page arm: relaunching with CWOFFEE_BROWSER_PAGE=yes (group 'browser-inpage' only)"
  XDG_CONFIG_HOME="$PARM/config" XDG_DATA_HOME="$PARM/data" XDG_STATE_HOME="$PARM/state" \
    CWOFFEE_WORKSPACE="$PARM/ws" CWOFFEE_PI_AGENT_DIR="$PARM/pi" \
    CWOFFEE_GATEWAY_PORT=$((GW_PORT + 5)) \
    CWOFFEE_BROWSER_PAGE=yes \
    CWOFFEE_SELFTEST=1 CWOFFEE_SELFTEST_ONLY=browser-inpage CWOFFEE_SELFTEST_SKIP= \
    WEBKIT_DISABLE_COMPOSITING_MODE=1 ./target/release/cwoffee-app >>"$plog" 2>&1 &
  PG_PID=$!
  pver=""
  for _ in $(seq 1 200); do
    grep -q "SELFTEST ok\|SELFTEST FAILED" "$plog" 2>/dev/null && { pver=$(grep -o "SELFTEST ok\|SELFTEST FAILED" "$plog" | head -1); break; }
    kill -0 "$PG_PID" 2>/dev/null || { pver="gone"; break; }
    sleep 1
  done
  kill "$PG_PID" 2>/dev/null
  for i in 1 2 3 4 5 6; do kill -0 "$PG_PID" >/dev/null 2>&1 || break; sleep 0.5; done
  if [ "$pver" = "SELFTEST ok" ]; then
    note "in-page arm: ok - $(grep -o 'the page in the window is .*' "$plog" | head -1)"
  else
    PAGE_FAIL=1
    printf '\033[31m[selftest.sh] the page-in-the-window arm %s (log: %s):\033[0m\n' "${pver:-timed out}" "$plog" >&2
    grep -E "SELFTEST FAIL|the page in the window is|selftest note" "$plog" | tail -12 >&2
  fi
  [ "$PAGE_FAIL" = 0 ] && rm -f "$plog"
  [ "$KEEP" = 0 ] && rm -rf "$PARM"
fi

if [ "$NO_BROWSER" = 0 ] && [ "$br_want" = 1 ] && [ -x target/release/cwoffee-app ]; then
  if [ -n "${APP_PID:-}" ]; then
    kill "$APP_PID" >/dev/null 2>&1
    for i in 1 2 3 4 5 6 7 8; do kill -0 "$APP_PID" >/dev/null 2>&1 || break; sleep 0.5; done
    note "browser idle: the board's app (pid $APP_PID) is ended; the idle arm needs its own config"
  fi
  blog=/tmp/cwoffee-selftest-browser-idle-$$.log
  BARM=$(mktemp -d /tmp/cwoffee-browser-arm.XXXXXX)
  mkdir -p "$BARM/config/cwoffee" "$BARM/data" "$BARM/state" "$BARM/ws" "$BARM/pi"
  cat > "$BARM/config/cwoffee/config.toml" <<TOML
# scripts/selftest.sh wrote this for the kept-browser idle arm, in a scratch home it deletes on
# exit. idle_secs is the point of the file: the arm is grading the clock, so it has to be a start
# whose own config names a short span. Every other [browser] value stays at its default, including
# the driver, which is found beside the one-shot driver the board already uses.
[browser]
idle_secs = 4
TOML
  note "browser idle: relaunching over $BARM/config/cwoffee/config.toml, which sets idle_secs = 4 (group 'browser-idle' only)"
  XDG_CONFIG_HOME="$BARM/config" XDG_DATA_HOME="$BARM/data" XDG_STATE_HOME="$BARM/state" \
    CWOFFEE_WORKSPACE="$BARM/ws" CWOFFEE_PI_AGENT_DIR="$BARM/pi" \
    CWOFFEE_GATEWAY_PORT=$((GW_PORT + 3)) \
    CWOFFEE_SELFTEST=1 CWOFFEE_SELFTEST_ONLY=browser-idle \
    WEBKIT_DISABLE_COMPOSITING_MODE=1 ./target/release/cwoffee-app >>"$blog" 2>&1 &
  BR_PID=$!
  bver=""
  bwait=150
  [ "${CWOFFEE_CORE:-rust}" = pi ] && bwait=300
  for _ in $(seq 1 "$bwait"); do
    grep -q "SELFTEST ok\|SELFTEST FAILED" "$blog" 2>/dev/null && { bver=$(grep -o "SELFTEST ok\|SELFTEST FAILED" "$blog" | head -1); break; }
    kill -0 "$BR_PID" 2>/dev/null || { bver="gone"; break; }
    sleep 1
  done
  kill "$BR_PID" 2>/dev/null
  for i in 1 2 3 4 5 6; do kill -0 "$BR_PID" >/dev/null 2>&1 || break; sleep 0.5; done
  # Give a driver that is shutting down a moment to finish: the check below asks whether anything
  # is left, and "still on its way out" is a different answer from "still running".
  sleep 2
  leaked=$(pgrep -a -f "browser/session.mjs" 2>/dev/null | grep -v "^$BR_PID" | head -3)
  if [ "$bver" = "SELFTEST ok" ]; then
    if [ -n "$leaked" ]; then
      BR_FAIL=1
      printf '\033[31m[selftest.sh] the idle arm passed, but a session driver is still running:\033[0m\n%s\n' "$leaked" >&2
    fi
    note "browser idle: ok - $(grep -o 'browser idle ok ([^)]*' "$blog" | head -1)"
  else
    BR_FAIL=1
    printf '\033[31m[selftest.sh] the kept-browser idle arm %s (log: %s):\033[0m\n' "${bver:-timed out}" "$blog" >&2
    grep -E "SELFTEST FAIL|browser idle ok|selftest note" "$blog" | tail -10 >&2
  fi
  [ "$BR_FAIL" = 0 ] && rm -f "$blog"
  [ "$KEEP" = 0 ] && rm -rf "$BARM"
fi

if [ "${PAGE_FAIL:-0}" = 1 ] && [ "$verdict" = ok ]; then
  verdict=fail
  note "the page board is green, but the page-in-the-window arm is red - see the 'in-page arm' lines above"
fi

if [ "$BR_FAIL" = 1 ] && [ "$verdict" = ok ]; then
  verdict=fail
  note "the page board is green, but the kept-browser idle arm is red - see the 'browser idle' lines above"
fi

if [ "$MCP_FAIL" = 1 ] && [ "$verdict" = ok ]; then
  verdict=fail
  note "the page board is green, but the servers write arm is red - see the 'mcp write' lines above"
fi

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
    grep -E "UNCAUGHT:|REJECTION:" "$log" | tail -5 >&2
    tail -12 "$log" >&2
    exit 1 ;;
  *)
    # The page board said FAILED. Say so in the same words the other branches use: a reader
    # grepping for "verdict" must not conclude the run never finished. It did finish; it lost.
    note "verdict: FAILED - the page board's own summary follows"
    grep -E "SELFTEST FAILED" "$log" | tail -2 >&2
    grep -E "SELFTEST FAIL" "$log" | tail -25 >&2
    exit 1 ;;
esac
