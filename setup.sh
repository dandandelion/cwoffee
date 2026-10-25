#!/usr/bin/env bash
# setup.sh - one run, from a fresh clone to an app you can open.
#
# Why this exists: the build needs a Rust toolchain, a GTK/WebKit development stack, node and a
# few runtime helpers. SETUP.md records which ones this machine has, verified by hand once, with a
# pkg-config transcript. That is a record, not a check: it cannot tell a fresh clone on a new box
# what is missing, and on a machine that is missing one -devel package the failure shows up as a
# linker error three screens deep in `cargo build` - the worst possible place to discover it. So
# the same probes are data here, run every time, and reported in one table with the package that
# provides each one. The build is what runs after, so "it built" and "the machine was ready" stop
# being two separate mysteries.
#
# Usage:
#   ./setup.sh                       check requirements, build the UI and the release binaries
#   ./setup.sh --tests               + the unit suites, the type checks, the wire-name grader
#   ./setup.sh --board               + the page board (scripts/selftest.sh) on both cores
#   ./setup.sh --desktop             + install the desktop entry and prove a window opens
#   ./setup.sh --deps                install the missing system packages first (asks, unless --yes)
#   ./setup.sh --yes --deps          do not ask
#   ./setup.sh --endpoint URL --model ID [--workspace PATH]
#                                    write a starter config, but only if you have none
#   ./setup.sh --skip-build          requirements and config only
#   ./setup.sh --max-mb 7000         memory ceiling for the cargo steps (see scripts/cargo-guard.sh)
#   ./setup.sh -h                    this text
#
# What it will not do:
#   - overwrite an existing config. It reads yours if it exists and says which file it read.
#   - sudo for anything unless you pass --deps.
#   - touch your prompt cache. The checks that talk to an endpoint use scripts/mock-llm.mjs; a
#     real endpoint is something you pass on purpose (docs/TESTING.md says why).

set -uo pipefail
cd "$(dirname "$0")"
ROOT=$PWD
# rustup puts cargo in ~/.cargo/bin, which a fresh login shell often does not have on PATH, and
# then `cargo build` says "command not found" on a machine that has a toolchain on disk. This run
# adds it either way; the flag records that it had to, so the requirements table can say so
# before PATH is changed and the fact becomes invisible.
CARGO_NOTE=0
if ! command -v cargo >/dev/null 2>&1 && [[ -x $HOME/.cargo/bin/cargo ]]; then CARGO_NOTE=1; fi
export PATH="$HOME/.cargo/bin:$PATH"

MAXMB=5200
DO_TESTS=0 DO_BOARD=0 DO_DESKTOP=0 DO_DEPS=0 SKIP_BUILD=0 ASSUME_YES=0
ENDPOINT='' MODEL='' WORKSPACE='' CORES='both'

usage() { sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'; }

while (( $# )); do
  case "$1" in
    --tests) DO_TESTS=1 ;;
    --board) DO_BOARD=1 ;;
    --desktop|--install-desktop) DO_DESKTOP=1 ;;
    --deps) DO_DEPS=1 ;;
    --yes|-y) ASSUME_YES=1 ;;
    --skip-build) SKIP_BUILD=1 ;;
    --endpoint) ENDPOINT=${2:-}; shift ;;
    --model) MODEL=${2:-}; shift ;;
    --workspace) WORKSPACE=${2:-}; shift ;;
    --core) CORES=${2:-both}; shift ;;
    --max-mb) MAXMB=${2:-5200}; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "setup.sh: I don't know $1 (try -h)" >&2; exit 2 ;;
  esac
  shift
done

say() { printf '\033[36m[setup]\033[0m %s\n' "$*"; }
bad() { printf '\033[31m[setup]\033[0m %s\n' "$*" >&2; }
warn() { printf '\033[33m[setup]\033[0m %s\n' "$*"; }
step() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

# ---- requirements --------------------------------------------------------------------
# Required means "the build or the window fails without it". Optional means "a feature is
# missing but the app runs", so those are reported and not fatal. Both kinds are checked by
# the thing the app actually consults - a pkg-config name, a binary on PATH, a version -
# because a list of packages is a guess about somebody else's machine and a probe is not.
# The same module list SETUP.md's transcript records, because those are what Tauri's build script
# asks pkg-config for. Missing ones fail at link time with a message that names none of them.
REQ_PC=(webkit2gtk-4.1 gtk+-3.0 libsoup-3.0 javascriptcoregtk-4.1 glib-2.0 gobject-2.0 gio-2.0 openssl xkbcommon)
MISS_PC=()
MISS_PKG=()
FAILED=0

os_id() { . /etc/os-release 2>/dev/null && printf '%s %s' "${ID:-}" "${ID_LIKE:-}"; }
OSW=$(os_id)
PM=''
case "$OSW" in
  *fedora* | *rhel* | *centos*) PM=dnf ;;
  *debian* | *ubuntu*) PM=apt ;;
  *arch*) PM=pacman ;;
esac

# The package names for each pkg-config module. The Fedora column is not a guess: it is what
# `rpm -qf /usr/lib64/pkgconfig/<mod>.pc` answers on the box this project is developed on.
# The others are the names upstream Tauri documents; they have not been run through a package
# manager here, so a Debian box may still need one extra name.
pkg_for() {
  case "$PM:$1" in
    dnf:webkit2gtk-4.1) echo webkit2gtk4.1-devel ;;
    dnf:javascriptcoregtk-4.1) echo javascriptcoregtk4.1-devel ;;
    dnf:gtk+-3.0) echo gtk3-devel ;;
    dnf:libsoup-3.0) echo libsoup3-devel ;;
    dnf:glib-2.0 | dnf:gio-2.0 | dnf:gobject-2.0) echo glib2-devel ;;
    dnf:xkbcommon) echo libxkbcommon-devel ;;
    dnf:openssl) echo openssl-devel ;;
    apt:webkit2gtk-4.1) echo libwebkit2gtk-4.1-dev ;;
    apt:javascriptcoregtk-4.1) echo libjavascriptcoregtk-4.1-dev ;;
    apt:gtk+-3.0) echo libgtk-3-dev ;;
    apt:libsoup-3.0) echo libsoup-3.0-dev ;;
    apt:glib-2.0 | apt:gio-2.0 | apt:gobject-2.0) echo libglib2.0-dev ;;
    apt:xkbcommon) echo libxkbcommon-dev ;;
    apt:openssl) echo libssl-dev ;;
    pacman:webkit2gtk-4.1) echo webkit2gtk-4.1 ;;
    pacman:javascriptcoregtk-4.1) echo javascriptcoregtk-4.1 ;;
    pacman:gtk+-3.0) echo gtk3 ;;
    pacman:libsoup-3.0) echo libsoup3 ;;
    pacman:glib-2.0 | pacman:gio-2.0 | pacman:gobject-2.0) echo glib2 ;;
    pacman:xkbcommon) echo libxkbcommon ;;
    pacman:openssl) echo openssl ;;
    *) echo '' ;;
  esac
}

# The same for the command-line tools, by the package that provides the binary on each distro.
tool_pkg() {
  case "$PM:$1" in
    dnf:cargo) echo cargo ;;
    apt:cargo) echo cargo ;;
    pacman:cargo) echo rust ;;
    dnf:rustc) echo rust ;;
    apt:rustc) echo rustc ;;
    pacman:rustc) echo rust ;;
    dnf:node) echo nodejs ;;
    apt:node) echo nodejs ;;
    pacman:node) echo nodejs ;;
    dnf:npm|dnf:npx) echo npm ;;
    apt:npm|apt:npx) echo npm ;;
    pacman:npm|pacman:npx) echo npm ;;
    dnf:cc) echo gcc ;;
    apt:cc) echo build-essential ;;
    pacman:cc) echo gcc ;;
    dnf:pkg-config) echo pkgconf-pkg-config ;;
    apt:pkg-config) echo pkg-config ;;
    pacman:pkg-config) echo pkgconf ;;
    *) echo "$1" ;;
  esac
}

step "requirements"
say "os: ${OSW:-unknown}   package manager: ${PM:-none found}"
say "these are the probes SETUP.md's transcript records for one machine, run on this one"

have() { command -v "$1" >/dev/null 2>&1; }

if (( CARGO_NOTE )); then
  warn "cargo is in ~/.cargo/bin but not on your PATH; this run uses it anyway."
  warn "add this to your shell profile so your own shell finds it:"
  warn '  . "$HOME/.cargo/env"     # or: export PATH="$HOME/.cargo/bin:$PATH"'
fi
for tool in cargo rustc node npm npx cc pkg-config git; do
  if have "$tool"; then
    v=$("$tool" --version 2>/dev/null | head -1)
    printf '  \033[32mok\033[0m      %-11s %s\n' "$tool" "$v"
  else
    printf '  \033[31mmissing\033[0m  %-11s (required)\n' "$tool"
    p=$(tool_pkg "$tool")
    [[ -n $p ]] && MISS_PKG+=("$p")
    FAILED=1
  fi
done

for mod in "${REQ_PC[@]}"; do
  if pkg-config --exists "$mod" 2>/dev/null; then
    printf '  \033[32mok\033[0m      %-22s %s\n' "$mod" "$(pkg-config --modversion "$mod")"
  else
    printf '  \033[31mmissing\033[0m  %-22s (required: pkg-config --exists %s)\n' "$mod" "$mod"
    p=$(pkg_for "$mod")
    [[ -n $p ]] && MISS_PKG+=("$p")
    FAILED=1
  fi
done

# Optional: each of these is something the app does at run time, named by the code that does it.
opt() { # name  present  what breaks without it
  if [[ $2 == 1 ]]; then printf '  \033[32mthere\033[0m    %-22s %s\n' "$1" "$3"
  else printf '  \033[33mno\033[0m        %-22s %s\n' "$1" "$4"; fi
}
PI_BIN=${CWOFFEE_PI:-}
if [[ -z $PI_BIN ]]; then PI_BIN=$(command -v pi || true); fi
opt cargo-guard $([[ -x scripts/cargo-guard.sh ]] && echo 1 || echo 0) \
  'builds and tests get a memory ceiling' 'cargo may outgrow RAM on a small box (it is in the repo, so this should never be missing)'
opt bwrap $(have bwrap && echo 1 || echo 0) \
  'the exec sandbox (Settings > Tools)' 'commands run unsandboxed; the app says so in the log'
opt systemd-run $(have systemd-run && echo 1 || echo 0) \
  'the gateway outliving the window as its own transient unit' 'falls back to setsid, and a hand-off child is harder to stop cleanly'
opt python3 $(have python3 && echo 1 || echo 0) \
  'local title generation and the search helper' 'titles come from the endpoint instead, which costs a request per conversation'
opt podman $(have podman && echo 1 || echo 0) \
  'the managed SearXNG container (Settings > Search)' 'web search needs a URL you run yourself'
opt pdftotext $(have pdftotext && echo 1 || echo 0) \
  'reading a PDF that was dragged in (poppler-utils)' 'PDF attachments are refused with a sentence saying why'
opt ninja $(have ninja && echo 1 || echo 0) \
  'some C deps in the graph (ring, zstd)' 'cargo usually falls back to a bundled build; rare' 
opt pi $([[ -n $PI_BIN ]] && echo 1 || echo 0) \
  'the pi core (CWOFFEE_CORE=pi)' 'the pi core cannot run; the Rust core is unaffected'
if [[ -n $PI_BIN ]]; then
  piv=$("$PI_BIN" --version 2>/dev/null | head -1 | tr -d '\r')
  pin=$(grep -m1 'PINNED_PI' crates/bridge/src/process.rs | sed -E 's/.*"([^"]+)".*/\1/')
  if [[ $piv == *"$pin"* ]]; then
    printf '           pi version %s (this build pins %s)\n' "$piv" "$pin"
  else
    warn "pi is version ${piv:-?} and this build pins $pin: the RPC handshake may refuse. Set CWOFFEE_PI_VERSION to override, deliberately."
  fi
fi

if (( ${#MISS_PKG[@]} )) && (( DO_DEPS )); then
  step "system packages"
  case "$PM" in
    dnf) INSTALL=(sudo dnf install -y) ;;
    apt) INSTALL=(sudo apt-get update -y) ; APT=(sudo apt-get install -y) ;;
    pacman) INSTALL=(sudo pacman -S --needed --noconfirm) ;;
    *) bad "I don't know the package manager on this box, so I won't guess. Install these by hand:"; printf '  %s\n' "${MISS_PKG[@]}"; exit 1 ;;
  esac
  mapfile -t MISS_PKG < <(printf '%s\n' "${MISS_PKG[@]}" | awk '!seen[$0]++')
  printf 'about to install:\n'
  printf '  %s\n' "${MISS_PKG[@]}"
  if ! have sudo; then
    bad "there is no sudo on this box, so I cannot install anything. Run the line above as root:"
    printf '  %s %s\n' "${PM}" "${MISS_PKG[*]}"
    exit 1
  fi
  if (( ASSUME_YES == 0 )); then
    printf 'that needs sudo. type y to go ahead: '
    read -r ans
    [[ $ans == y || $ans == Y ]] || { bad 'not installing anything; run the checks again once you have them'; exit 1; }
  fi
  if [[ $PM == apt ]]; then
    "${INSTALL[@]}" || exit 1
    "${APT[@]}" "${MISS_PKG[@]}" || exit 1
  else
    "${INSTALL[@]}" "${MISS_PKG[@]}" || exit 1
  fi
  # Re-check rather than assume the install worked: a failed transaction that leaves one
  # required module out would otherwise be discovered by the linker again.
  FAILED=0
  for mod in "${REQ_PC[@]}"; do
    pkg-config --exists "$mod" 2>/dev/null || { bad "$mod still missing after the install"; FAILED=1; }
  done
  have cargo && have rustc && have node || FAILED=1
  MISS_PKG=()
fi

if (( FAILED )) && (( DO_DEPS == 0 )); then
  step "requirements are not met"
  if [[ -n $PM && ${#MISS_PKG[@]} ]]; then
    say "install them and run me again, or let me do it:"
    printf '  %s install %s\n' "$PM" "$(printf '%s\n' "${MISS_PKG[@]}" | awk '!seen[$0]++' | tr '\n' ' ')"
    say "(that is what ./setup.sh --deps would run, after asking)"
  else
    say "install the missing pieces above, then run me again."
    say "the toolchain: rustup (https://rustup.rs) and your distro's node package."
  fi
  exit 1
fi
if (( FAILED )); then
  step "requirements are still not met"
  exit 1
fi

# ---- the config ----------------------------------------------------------------------
step "config"
CFG=${XDG_CONFIG_HOME:-$HOME/.config}/cwoffee/config.toml
if [[ -f $CFG ]]; then
  say "found a config: $CFG"
  # Only report the two fields that make the app refuse to start, so this is a diagnosis and
  # not a second config editor.
  ep=$(grep -m1 'base_url' "$CFG" | sed -E 's/.*"([^"]*)".*/\1/')
  md=$(grep -m1 '^model' "$CFG" | sed -E 's/.*"([^"]*)".*/\1/')
  say "  endpoint ${ep:-<none in the provider block>}   model ${md:-<none in the provider block>}"
  if [[ -n $ENDPOINT || -n $MODEL ]]; then
    warn "you passed --endpoint/--model and a config already exists; nothing was written. Edit $CFG yourself - a setup script silently rewriting a working config is how people lose their settings."
  fi
elif [[ -n $ENDPOINT ]]; then
  mkdir -p "$(dirname "$CFG")"
  cat >"$CFG" <<TOML
# Written by setup.sh on $(date -u '+%Y-%m-%d %H:%M UTC'). Edit freely.
mode = "auto"

[provider]
kind = "openai"
base_url = "$ENDPOINT"
model = "${MODEL:-cwoffee-mock}"
api_key = "unused"
TOML
  say "wrote a starter config: $CFG"
  say "  endpoint $ENDPOINT   model ${MODEL:-cwoffee-mock}"
  say "a bare OpenAI-compatible config has no [sandbox], [search] or [context] block; the app fills those in with defaults and starts."
else
  warn "no config at $CFG. The build will still work; the app will refuse to start until it has an endpoint."
  say "either re-run with --endpoint http://127.0.0.1:8090/v1 --model your-model-id,"
  say "or write the file yourself; the smallest one that starts is:"
  cat <<TOML
  # $CFG
  mode = "auto"

  [provider]
  kind = "openai"
  base_url = "http://127.0.0.1:8090/v1"
  model = "your-model-id"
  api_key = "unused"
TOML
fi
[[ -n $WORKSPACE ]] && say "workspace you asked for: $WORKSPACE (the app remembers the last folder it opened in its store, not in this config)"

# ---- build ---------------------------------------------------------------------------
if (( SKIP_BUILD == 0 )); then
  step "the UI"
  if [[ ! -d ui/node_modules ]]; then
    if [[ -f ui/package-lock.json ]]; then
      say "npm ci (locked install, from ui/package-lock.json)"
      ( cd ui && npm ci --no-audit --no-fund ) || exit 1
    else
      warn "no package-lock.json; running npm install, which may resolve differently than this project was tested with"
      ( cd ui && npm install --no-audit --no-fund ) || exit 1
    fi
  else
    say "ui/node_modules is already there; skipping the install (delete it and re-run for a clean one)"
  fi
  say "vite build (the release binary embeds this bundle at compile time)"
  ( cd ui && npx vite build ) || exit 1

  step "the release binaries"
  say "cargo-guard keeps the build under ${MAXMB} MB; a 20-thread cargo on a laptop with other things open is how a build machine dies"
  ./scripts/cargo-guard.sh --max-mb "$MAXMB" \
    cargo build --release -p cwoffee-app -p cwoffee-gateway -p cwoffee-bridge -j 4 || exit 1
  for b in cwoffee-app cwoffee-gateway cwtool; do
    if [[ -x target/release/$b ]]; then
      printf '  \033[32mok\033[0m %-18s %s\n' "$b" "$(du -h "target/release/$b" | cut -f1)"
    else
      bad "cargo said it built and target/release/$b is not there"
      exit 1
    fi
  done
  # Not a string check. The binary has to be the one the build produced.
  say "the gateway binary answers for itself: target/release/cwoffee-gateway --help"
  target/release/cwoffee-gateway --help >/dev/null 2>&1 || { bad "target/release/cwoffee-gateway --help exited $?"; exit 1; }
fi

# ---- tests ---------------------------------------------------------------------------
if (( DO_TESTS )); then
  step "the suites"
  say "engine, bridge, gateway and the desktop app's own tests, four threads at a time (a wider test-threads setting peaks over 5 GB on this box)"
  # shellcheck disable=SC2015
  ./scripts/cargo-guard.sh --max-mb "$MAXMB" cargo test -p cwoffee-engine --lib -- --test-threads=4 || exit 1
  ./scripts/cargo-guard.sh --max-mb "$MAXMB" cargo test -p cwoffee-bridge --lib || exit 1
  ./scripts/cargo-guard.sh --max-mb "$MAXMB" cargo test -p cwoffee-gateway --lib || exit 1
  ./scripts/cargo-guard.sh --max-mb "$MAXMB" cargo test -p cwoffee-gateway --bin cwoffee-gateway || exit 1
  ./scripts/cargo-guard.sh --max-mb "$MAXMB" cargo test -p cwoffee-app --bin cwoffee-app || exit 1
  step "the screens"
  ( cd ui && npm run test:md ) || exit 1
  ( cd ui && npx svelte-check --tsconfig ./tsconfig.json ) || exit 1
  say "the wire-name grader: the verbs the client calls, the arms the host answers, the commands the desktop registers"
  node scripts/wire-names.mjs || exit 1
  say "what --tests does NOT run here: the pi core's integration suites (crates/bridge/tests, 25 cases + 5 fixture replays) and the in-app board. They need the pinned pi (this build pins $(grep -m1 'PINNED_PI' crates/bridge/src/process.rs | sed -E 's/.*\"([^\"]+)\".*/\1/')) and a longer leash: cargo test -p cwoffee-bridge, and ./setup.sh --board."
fi

if (( DO_BOARD )); then
  step "the page board"
  say "scripts/selftest.sh drives the real window against scripts/mock-llm.mjs. It rebuilds ui/dist as a test bundle - re-run ./setup.sh (or cd ui && npx vite build) before installing the desktop entry afterwards."
  ./scripts/selftest.sh || exit 1
  if [[ -n $PI_BIN && ( $CORES == pi || $CORES == both ) ]]; then
    say "the same board on the pi core"
    CWOFFEE_CORE=pi ./scripts/selftest.sh || exit 1
  elif [[ $CORES == pi ]]; then
    bad "--core pi was asked for and there is no pi on PATH (or at CWOFFEE_PI)"
    exit 1
  fi
fi

# ---- the desktop entry ---------------------------------------------------------------
if (( DO_DESKTOP )); then
  step "the desktop entry"
  # A board run leaves the self-test bundle in ui/dist, and install-desktop.sh refuses to install
  # that as a daily driver. Rather than make you read the refusal and run the command it names,
  # rebuild the production bundle here. The refusal still stands for anyone running the installer
  # by hand - it is the last line of defence, not the intended path.
  say "rebuilding the production UI bundle first (a board run leaves a test bundle in ui/dist)"
  ( cd ui && npx vite build ) || exit 1
  # The launcher is the thing that has broken silently before: an Exec= into the build tree,
  # or a persona the chosen core cannot serve, and the menu item starts a process that exits
  # before it paints. install-desktop.sh launches the entry itself and reads the app's boot
  # line, so this step grades the promise and not the artefacts.
  ./scripts/install-desktop.sh || exit 1
fi

step "done"
say "run it:"
printf '  %s\n' \
  "./target/release/cwoffee-app                              # the Rust core, this build" \
  "./scripts/run-pi-app.sh                                   # the pi core (needs pi on PATH${PI_BIN:+, which it found: $PI_BIN})" \
  "./target/release/cwoffee-gateway --port 8099 --dist ui/dist   # devices on your network, no window"
say "a menu launcher instead of a terminal: ./setup.sh --desktop"
say "other checks: unit suites ./setup.sh --tests - the page board ./setup.sh --board - the gateway outliving the window node scripts/gateway-handoff.mjs"
