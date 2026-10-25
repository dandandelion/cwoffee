#!/usr/bin/env bash
# Install a desktop entry + hicolor icons + a launcher for a *locally built* cwoffee,
# and then prove the launcher opens a window.
#
# Why this exists: the RPM/AppImage carry their own .desktop file and icons, but when you
# run target/release/cwoffee-app directly the window manager has nothing to look up, so
# GNOME shows a generic gear in the dock and the overview. The window itself also gets an
# icon at runtime (set_icon in main.rs); this covers the taskbar/dock half, which is a
# desktop-file lookup, not a window property.
#
# Why it is more than a file copy now: an earlier version of this script verified the
# *artwork* by hash and then announced "installed" — and a launcher that installs
# beautifully while refusing to open is exactly what it was blamed for. Two ways that
# happens, and neither one is visible to a script that only checks files:
#
#   * The entry's `Exec=` pointed straight at `target/release/cwoffee-app`, i.e. into a
#     build tree. `cargo build --release` (and every board run of `scripts/selftest.sh`)
#     deletes and rewrites that file; a click during that window — which lasts minutes on
#     this box — is a click that does nothing at all, with no error anywhere.
#   * An app started from the apps menu inherits the **session** environment, not your
#     shell's. Anything your terminal launcher exports (a toolchain on PATH, the pi core's
#     extension and tool host, a workspace) is simply not there, and the app's way of
#     reporting that can be as quiet as a window that never maps.
#
# So the install puts a **copy** of the binary and the UI under the user's own data dir,
# writes a small launcher that sets what the app needs, and finishes by launching the
# installed entry the way the desktop launches it (`gio launch`) and reading the app's own
# evidence that a window exists. `cwoffee-app` prints `render OK: titlebar=true …` once the
# page has painted, and `SUSPECT: no render report after 8s` when the window is blank;
# those two lines are the difference between "installed" and "opens".
#
# Installs into the user's own directories - no root, no system paths.
#
# Usage:
#   scripts/install-desktop.sh [--bin PATH] [--dist PATH] [--core rust|pi]
#                              [--pi-bin PATH] [--link] [--no-launch-check]
#                              [--check-only] [--allow-test-bundle] [--remove] [-h]
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
bin="${CWOFFEE_BIN:-$here/target/release/cwoffee-app}"
dist="${CWOFFEE_UI_DIST_SRC:-$here/ui/dist}"
icons="$here/crates/tauri-app/icons"
core="auto"
pi_bin=""
mode_copy=1
launch_check=1
check_only=0
remove=0
allow_test=0

usage() {
  # The header comment *is* the help: one place that says why this script exists, rather
  # than a second copy that goes out of date the first time the reasoning changes.
  sed -n '2,/^set -euo/p' "$0" | sed 's/^#\{0,1\}//'
  cat <<'EOF'

Options
  --bin PATH        the binary to install        (default target/release/cwoffee-app)
  --dist PATH       the ui/dist to install       (default <repo>/ui/dist)
  --core rust|pi|auto  what the launcher records for CWOFFEE_CORE (default: auto, which
                    reads your config: persona = chat can only run on the pi core, and a
                    menu launch has no environment of its own to get it from)
  --pi-bin PATH     the pi executable to record for --core pi (default: `pi` on PATH)
  --link            symlink the binary instead of copying it (development: the launcher
                    then follows whatever the next cargo build leaves there, including
                    the moment when nothing is there)
  --no-launch-check skip the "does a window actually appear" check
  --check-only      do not install; only run the checks against what is installed now
  --allow-test-bundle  accept a ui/dist that carries the self-test bundle (the board
                    overwrites ui/dist constantly, so the default is to refuse it)
  --remove          remove the entry, the icons and the installed copy
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --bin) bin="$2"; shift 2 ;;
    --dist) dist="$2"; shift 2 ;;
    --core) core="$2"; shift 2 ;;
    --pi-bin) pi_bin="$2"; shift 2 ;;
    --link) mode_copy=0; shift ;;
    --no-launch-check) launch_check=0; shift ;;
    --check-only) check_only=1; shift ;;
    --allow-test-bundle) allow_test=1; shift ;;
    --remove) remove=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1 (try --help)" >&2; exit 2 ;;
  esac
done

case "$core" in rust|pi|auto) ;; *) echo "--core takes rust, pi or auto, got $core" >&2; exit 2 ;; esac

data="${XDG_DATA_HOME:-$HOME/.local/share}"
cfg="${XDG_CONFIG_HOME:-$HOME/.config}/cwoffee/config.toml"

# Which core the launcher must record is not a style choice: `persona = chat` cannot run on
# the Rust core at all, and main() exits before a window exists - a click on the icon then
# looks like a launcher that does nothing, with the reason only on a stderr nobody sees.
# This box has a journal entry that is exactly that: an app launched by gnome-shell, three
# boot lines, and "persona: chat is not available on the rust core" as the last word.
# Read the config the app will read. Not the environment of this shell: CWOFFEE_PERSONA in
# a terminal says nothing about what the desktop will pass, which is the whole problem.
persona=""
if [[ -f "$cfg" ]]; then
  persona="$(awk '
    /^[[:space:]]*#/ { next }
    /^[[:space:]]*\[/ { exit }            # top-level keys only, like Config::load
    /^[[:space:]]*persona[[:space:]]*=/ {
      sub(/^[[:space:]]*persona[[:space:]]*=/, ""); gsub(/[[:space:]]/, ""); print; exit
    }' "$cfg" | tr -d "\"'")"
fi
core_reason=""
if [[ "$core" == "auto" ]]; then
  # State on disk is evidence of which core this machine actually uses: those directories
  # are what a pi-core boot leaves behind (the default agent dir, and the ones
  # scripts/run-pi-app.sh names explicitly). A launcher should start the app that is in use.
  pi_state=""
  for d in "$data/cwoffee/pi" "$data/cwoffee/pi-agent" "$data/cwoffee/pi-sessions"; do
    [[ -d "$d" ]] && { pi_state="$d"; break; }
  done
  if [[ "$persona" == "chat" ]]; then
    core="pi"
    core_reason="$cfg says persona = chat, and chat exists only on the pi core: a launcher that"
    core_reason+=" recorded the Rust core would start an app that exits at boot."
  elif [[ -n "$pi_state" ]] && [[ -n "${CWOFFEE_PI:-$(command -v pi || true)}" ]]; then
    core="pi"
    core_reason="pi is installed and $pi_state holds pi-core state: the menu launcher starts the"
    core_reason+=" core this machine already runs, which is also the one that handles both personas."
  else
    core="rust"
    core_reason="$cfg says persona = ${persona:-cowork (the default)} and no pi-core state was found:"
    core_reason+=" the Rust core is what the binary is without an environment."
  fi
fi
if [[ "$core" == "rust" && "$persona" == "chat" ]]; then
  echo "--core rust while $cfg says persona = \"chat\": the app exits at boot with" >&2
  echo "\"chat is not available on the rust core\" and no window ever appears - a launcher" >&2
  echo "that installs cleanly and does nothing. Fix one side: --core pi, or set" >&2
  echo "persona = cowork in $cfg." >&2
  exit 1
fi
apps="$data/applications"
ico="$data/icons/hicolor"
app_id="dev.cwoffee.app"
entry="$apps/cwoffee.desktop"
# Program files, deliberately not under $data/cwoffee: that directory is the app's own -
# sessions.db, pi state, the pi agent dir - and a user clearing their data would delete the
# binary under a launcher that then silently does nothing. That is the exact failure this
# script is meant to make loud, so do not create it.
root="$data/cwoffee-desktop"       # installed copy: bin/ + ui/ as siblings
app_bin="$root/bin/cwoffee-app"    # ... which is why ../ui/dist resolves for it
wrapper="$root/cwoffee"            # what Exec= runs
gw_bin="$root/bin/cwoffee-gateway" # the standalone gateway a hand-off at window close starts
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

remove_all() {
  rm -f "$entry"
  local n bad=0
  for n in 16 24 32 48 64 96 128 256 512; do
    for name in cwoffee "$app_id"; do
      f="$ico/${n}x${n}/apps/${name}.png"
      [[ -f "$f" ]] && { rm -f "$f" || bad=$((bad + 1)); }
    done
  done
  # Only this script's own tree: never the user's sessions, store or config.
  if [[ -d "$root" ]]; then
    case "$root" in
      "$data/cwoffee-desktop") rm -rf "$root" ;;
      *) echo "refusing to delete $root" >&2; bad=$((bad + 1)) ;;
    esac
  fi
  command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$apps" || true
  command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t -f "$ico" || true
  echo "removed $entry, the $app_id/cwoffee icons and $root"
  (( bad == 0 )) || { echo "ERROR: $bad file(s) could not be removed" >&2; exit 1; }
  exit 0
}
[[ $remove -eq 1 ]] && remove_all

# ---- premise of the checks: what is installed, and what is the app's own evidence ----
# The app says when its page has painted. Everything below reads that line rather than
# trusting that a spawned process equals a window on screen.
RENDER_LINE='\[cwoffee\] render OK'
BLANK_LINE='SUSPECT: no render report'

launch_check_run() {
  # Launch the installed entry the way the desktop does, and grade what the app reports.
  local log="$tmp/launch.log" method="" spawner="" i verdict="" newpid=""
  if [[ -z "${WAYLAND_DISPLAY:-}${DISPLAY:-}" ]]; then
    echo "launch check: SKIPPED - no display in this environment (wayland/x11 both unset)"
    echo "  the entry was written; whether it opens a window is not something I can prove here."
    return 0
  fi
  pgrep -f -- "$app_bin" | sort > "$tmp/pids_before" || true
  # The silent case, made loud: an entry whose TryExec is missing is refused by the
  # desktop without a word, and a click then looks identical to a broken launcher. This
  # is what happened when the entry pointed into target/release during a cargo build.
  if [[ ! -x "$app_bin" ]]; then
    echo "launch check: FAILED - the entry runs $app_bin, and that file is not there"
    echo "  TryExec=$app_bin makes the desktop refuse to start it, silently: clicking the"
    echo "  icon looks exactly like a launcher that does nothing. Re-run this script"
    echo "  without --check-only to install the binary again."
    return 1
  fi
  if [[ ! -d "$root/ui/dist" ]]; then
    echo "launch check: FAILED - the launcher sets CWOFFEE_UI_DIST=$root/ui/dist, which is not a directory"
    echo "  the window itself is unaffected (a release binary embeds its page); this breaks"
    echo "  the gateway and the browser path, which can only serve files from a real one."
    echo "  Re-run this script to install the bundle again."
    return 1
  fi
  # gio launch blocks until the application exits, and the application is a window, so
  # both launch paths go to the background and the verdict is read from what the app
  # prints. A spawned process is not the promise; a painted window is.
  if command -v gio >/dev/null 2>&1; then
    method="gio launch $entry"
    # `gio launch` is the desktop-file launch path (GAppInfo): the same one the app grid
    # uses, so it honours the Exec=, Path= and quoting we wrote instead of reconstructing
    # what the menu might do.
    gio launch "$entry" >"$log" 2>&1 &
  else
    method="$wrapper (direct, no gio: a weaker check than the desktop's own)"
    ( cd "$HOME" && exec "$wrapper" ) >"$log" 2>&1 &
  fi
  spawner=$!

  for ((i = 0; i < 30; i++)); do
    if grep -q "$RENDER_LINE" "$log" 2>/dev/null; then verdict="painted"; break; fi
    if grep -q "$BLANK_LINE" "$log" 2>/dev/null; then verdict="blank"; break; fi
    sort <(pgrep -f -- "$app_bin" || true) > "$tmp/pids_now"
    comm -13 "$tmp/pids_before" "$tmp/pids_now" > "$tmp/pids_new" || true
    if [[ ! -s "$tmp/pids_new" && $i -ge 3 && -s "$log" ]]; then verdict="exited"; break; fi
    sleep 0.5
  done
  sort <(pgrep -f -- "$app_bin" || true) > "$tmp/pids_now"
  comm -13 "$tmp/pids_before" "$tmp/pids_now" > "$tmp/pids_new" || true
  newpid="$(head -1 "$tmp/pids_new")"

  local rc=0
  case "$verdict" in
    painted)
      echo "launch check: OK ($method)"
      # The engine line is printed by the app, not inferred: it names the core, persona and
      # endpoint this launcher actually starts, which is the one thing a .desktop file cannot
      # say for itself.
      grep -m1 '\[cwoffee\] engine up' "$log" | sed 's/^/  app says: /'
      grep -m1 "$RENDER_LINE" "$log" | sed 's/^/  app says: /'
      ;;
    blank)
      echo "launch check: FAILED - the process runs but the window never painted"
      rc=1
      ;;
    exited)
      echo "launch check: FAILED - the launcher started a process that exited before painting"
      rc=1
      ;;
    *)
      echo "launch check: FAILED - no render report within 15s of launching the entry"
      rc=1
      ;;
  esac
  if [[ -n "$newpid" ]]; then
    # Only the instance this check started. Killing a cwoffee the user already has open
    # would be a rude way to report a green check.
    kill -TERM "$newpid" 2>/dev/null || true
    for ((i = 0; i < 8; i++)); do
      kill -0 "$newpid" 2>/dev/null || break
      sleep 0.5
    done
    kill -KILL "$newpid" 2>/dev/null || true
    echo "  (the check's window was closed; the instance it started was $newpid)"
  fi
  if (( rc )); then
    echo "  the first lines it printed:"
    head -14 "$log" | sed 's/^/  | /'
    [[ -s "$log" ]] || echo "  | (nothing at all - the process never got as far as its own boot line)"
    echo "  a window that maps but never paints is the render path, not the UI bundle: a"
    echo "  release binary embeds its own page. Run scripts/gpu-check.sh, and look above for"
    echo "  the leaked-NVIDIA warning - the GPU policy is the first thing main() sets up."
    echo "  when you click the icon yourself there is no terminal for these lines to reach:"
    echo "  the same output goes to the journal - journalctl --user -b | grep cwoffee.desktop"
  fi
  return $rc
}

if [[ $check_only -eq 1 ]]; then
  if [[ ! -f "$entry" ]]; then echo "nothing installed at $entry" >&2; exit 1; fi
  echo "checking $entry"
  command -v desktop-file-validate >/dev/null 2>&1 && {
    desktop-file-validate "$entry" && echo "  desktop-file-validate: clean"
  }
  launch_check_run
  exit $?
fi

# ---- what gets installed -------------------------------------------------------------
if [[ ! -x "$bin" ]]; then
  echo "no binary at $bin - build it first (cargo build --release -p cwoffee-app)" >&2
  exit 1
fi
if [[ ! -d "$dist" ]]; then
  echo "no UI bundle at $dist - build it first (cd ui && npx vite build)" >&2
  exit 1
fi
# The board rebuilds ui/dist with the self-test bundle inside, over and over. Installing
# that as the daily driver means the launcher's artwork and behaviour come and go with the
# last board run, and the bundle carries checks that drive the endpoint, the config file
# and the session store when armed. Refuse rather than explain it later.
if [[ $allow_test -eq 0 ]] && grep -rlq "self-test bundle embedded" "$dist"/assets/*.js 2>/dev/null; then
  echo "$dist carries the self-test bundle (built by scripts/selftest.sh)." >&2
  echo "Rebuild the production UI and install that:" >&2
  echo "  cd ui && npx vite build && cd .. && scripts/install-desktop.sh" >&2
  echo "(or pass --allow-test-bundle if you mean it)" >&2
  exit 1
fi

mkdir -p "$root/bin" "$root/ui"
if (( mode_copy )); then
  # A copy, not a pointer into the build tree: `cargo build --release` and every
  # scripts/selftest.sh run delete target/release/cwoffee-app before rewriting it, and a
  # launcher aimed at that path is a launcher that silently does nothing for minutes.
  if cmp -s "$bin" "$app_bin" 2>/dev/null; then
    echo "binary unchanged since the last install ($(sha256sum "$bin" | cut -c1-12))"
  else
    cp "$bin" "$root/bin/cwoffee-app.tmp"
    chmod 755 "$root/bin/cwoffee-app.tmp"
    mv -f "$root/bin/cwoffee-app.tmp" "$app_bin"
    echo "installed $(du -h "$app_bin" | cut -f1) binary -> $app_bin"
  fi
  rm -rf "$root/ui/dist"
  # The window's page is embedded in a release binary, so this copy is not what the window
  # renders: it is what the gateway serves to a browser on the LAN, and the only UI the
  # binary falls back to if its baked path is gone.
  cp -a "$dist" "$root/ui/dist"
  echo "installed UI bundle  -> $root/ui/dist ($(find "$root/ui/dist" -type f | wc -l) files)"
  # The gateway sibling. Settings > Devices offers "keep devices working after this window
  # closes", and that can only be honoured by a *separate* process: the gateway inside the app is
  # a task inside the app. The app looks for the binary next to itself first, so putting it here
  # is what makes the setting work from a launcher rather than only from a dev tree.
  gwsrc="${CWOFFEE_GATEWAY_BIN:-$(dirname "$bin")/cwoffee-gateway}"
  if [[ -x "$gwsrc" ]]; then
    cp "$gwsrc" "$root/bin/cwoffee-gateway.tmp"
    chmod 755 "$root/bin/cwoffee-gateway.tmp"
    mv -f "$root/bin/cwoffee-gateway.tmp" "$gw_bin"
    echo "installed gateway    -> $gw_bin"
  else
    echo "WARNING: no cwoffee-gateway at $gwsrc." >&2
    echo "         Build it (cargo build --release -p cwoffee-gateway) or the" >&2
    echo "         \"keep devices working after this window closes\" setting cannot be honoured:" >&2
    echo "         the gateway in this window is a task inside the window's own process." >&2
  fi
else
  mkdir -p "$(dirname "$app_bin")"
  ln -sfn "$bin" "$app_bin"
  rm -rf "$root/ui/dist"
  ln -sfn "$dist" "$root/ui/dist"
  gwsrc="${CWOFFEE_GATEWAY_BIN:-$(dirname "$bin")/cwoffee-gateway}"
  [[ -x "$gwsrc" ]] && ln -sfn "$gwsrc" "$gw_bin"
  echo "linked $app_bin -> $bin"
  echo "linked $root/ui/dist -> $dist   (the launcher breaks while a build is running)"
  echo "linked $gw_bin -> ${gwsrc:-nothing built yet}"
fi

# ---- the pi core's environment, resolved now ------------------------------------------
# The core is chosen by the environment (CWOFFEE_CORE), and a launcher started by the
# desktop has no environment to inherit. So the pi core's three moving parts get recorded
# here as absolute paths, found while this script runs - nothing can rediscover them later.
cwtool_dst=""
if [[ "$core" == "pi" ]]; then
  [[ -n "$pi_bin" ]] || pi_bin="$(command -v pi || true)"
  if [[ -z "$pi_bin" || ! -x "$pi_bin" ]]; then
    echo "the $core core needs a pi executable: pass --pi-bin PATH (nothing named pi is on PATH here)" >&2
    if [[ "$persona" == "chat" ]]; then
      echo "  auto picked pi because $cfg says persona = chat. If you meant the Rust core, set"
      echo "  persona = cowork there; a menu launch cannot bring a core with it in any other way."
    fi
    exit 1
  fi
  cwtool_src="${CWOFFEE_CWTOOL:-$here/target/release/cwtool}"
  if [[ -x "$cwtool_src" ]]; then
    # Copied, for the same reason as the app itself: the build tree is deleted and rewritten
    # by every cargo build, and a tool host that cannot be exec'd is a model that is told
    # "tool not found" for every call it makes.
    cp "$cwtool_src" "$root/bin/cwtool"
    chmod 755 "$root/bin/cwtool"
    cwtool_dst="$root/bin/cwtool"
    echo "installed tool host  -> $cwtool_dst"
  else
    echo "WARNING: no cwtool at $cwtool_src - build it (cargo build --release -p cwtool) and"
    echo "         re-run, or a pi-core turn will be told \"tool not found\" for every tool."
  fi
fi

# ---- the launcher -------------------------------------------------------------------
# Why this file exists: an app started from the apps menu gets the session environment,
# not your login shell's. `scripts/run-pi-app.sh` exports five things a menu launch cannot
# know about; recording them here is the only way the menu and the terminal start the same
# app. Recorded as absolute paths found at install time, because nothing can rediscover
# them later.
{
  echo "#!/usr/bin/env bash"
  echo "# Written by scripts/install-desktop.sh on $(date -u '+%Y-%m-%d %H:%M UTC')."
  echo "# Re-run that script after every release build: this launcher runs an installed copy"
  echo "# of the binary, not the one in the build tree."
  echo 'set -euo pipefail'
  echo "# A release binary embeds the page it shows, but the gateway serves a browser on the"
  echo "# LAN from a real directory, and ui/dist is also where the binary looks for its UI if"
  echo "# the path baked into it at build time is gone. Point at the installed copy: it is the"
  echo "# one thing that cannot be deleted by a cargo build."
  echo "export CWOFFEE_UI_DIST=\"$root/ui/dist\""
  if [[ "$core" == "pi" ]]; then
    echo "# The pi core: the model process, the extension that mounts cwoffee's tools, and the"
    echo "# Rust tool host behind them. Without the extension the model is told \"tool not"
    echo "# found\" for every call; without the tool host the extension registers tools that"
    echo "# cannot run. Absolute paths because the menu's PATH does not know this checkout."
    echo "export CWOFFEE_CORE=pi"
    echo "export CWOFFEE_PI=\"$pi_bin\""
    echo "export CWOFFEE_PI_EXT=\"$here/pi-tools/cwoffee-tools.ts\""
    if [[ -n "$cwtool_dst" ]]; then
      echo "export CWOFFEE_CWTOOL=\"$cwtool_dst\""
    fi
    echo "# pi's own shim finds its node by looking next to itself, and the extension shells"
    echo "# out to the tool host; keep pi's directory on PATH so both resolve the way they do"
    echo "# in a terminal."
    echo "export PATH=\"$(dirname "$pi_bin"):\$PATH\""
  else
    echo "# The Rust core needs nothing from the shell: the engine, the tools and the gateway"
    echo "# are inside this binary."
  fi
  echo "# A launcher's working directory is otherwise whatever the session hands it (often /),"
  echo "# which is a poor place for anything relative to resolve."
  echo "cd \"\$HOME\""
  echo "exec \"$app_bin\" \"\$@\""
} > "$root/cwoffee"
chmod 755 "$root/cwoffee"
echo "launcher          -> $root/cwoffee (core: $core)"
echo "  why: $core_reason"
if [[ "$core" == "rust" ]]; then
  echo "  a menu launch starts the Rust core: the core is chosen by the environment, and"
  echo "  nothing in the menu has an environment. Pass --core pi if that is what you run"
  echo "  from the terminal (scripts/run-pi-app.sh), or the two launches are different apps."
  echo "  the trap to know about: picking Chat in the window writes persona = chat into"
  echo "  $cfg, and the Rust core refuses to start chat - this launcher would then exit at"
  echo "  boot with no window, and the reason would only be in the journal. If you use Chat,"
  echo "  install with --core pi; --check-only re-runs the window check after a config change."
fi

# ---- icons ---------------------------------------------------------------------------
# Two names per size. `cwoffee.png` is what the Icon= key below resolves to.
# `dev.cwoffee.app.png` is the app id GNOME sees on the running window, and GNOME's
# app-theming looks an icon up by that name - miss it and the dock shows a generic
# gear next to a perfectly good launcher.
sizes=()
for n in 16 24 32 48 64 96 128 256 512; do
  src="$icons/${n}x${n}.png"
  [[ -f "$src" ]] || continue
  mkdir -p "$ico/${n}x${n}/apps"
  cp "$src" "$ico/${n}x${n}/apps/cwoffee.png"
  cp "$src" "$ico/${n}x${n}/apps/${app_id}.png"
  sizes+=("$n")
done

# ---- the desktop entry ---------------------------------------------------------------
# Quoted only when the path needs it: a value with a space in it is a single argument
# here, and unquoted GNOME splits it into "a command that does not exist".
q() { local s="$1"; [[ "$s" =~ [[:space:]] ]] && printf '"%s"' "$s" || printf '%s' "$s"; }
mkdir -p "$apps"
# No comment lines inside the group: they are legal, and this file used to carry them,
# but a launcher that has to be read by a parser is not the place for a paragraph.
cat > "$entry" <<EOF
[Desktop Entry]
Type=Application
Version=1.0
Name=Cwoffee
GenericName=Co-work agent
Comment=Local-model co-work agent: chat, tools, sandboxed commands, and document output.
Exec=$(q "$root/cwoffee")
TryExec=$(q "$app_bin")
Path=$HOME
Icon=cwoffee
Terminal=false
Categories=Development;
Keywords=agent;llm;chat;tools;local model;
StartupNotify=true
StartupWMClass=cwoffee-app
EOF
chmod 644 "$entry"

command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$apps" || true
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -q -t -f "$ico"
else
  # No cache tool: at least make the theme directory look newer than its contents
  # so a shell that does cache icons re-reads it.
  touch "$ico"
fi

# ---- verify --------------------------------------------------------------------------
# Three claims, graded separately: the entry parses, the icons are current, and the entry
# opens a window. An earlier version of this script graded the second and announced the
# third.
bad=0
printf '%-6s %-12s %s\n' size status icon
for n in ${sizes[@]+"${sizes[@]}"}; do
  r="$icons/${n}x${n}.png"
  i="$ico/${n}x${n}/apps/cwoffee.png"
  a="$ico/${n}x${n}/apps/${app_id}.png"
  hr=$(sha256sum "$r" | cut -c1-12)
  for pair in "cwoffee:$i" "$app_id:$a"; do
    name=${pair%%:*}; f=${pair#*:}
    if [[ ! -f "$f" ]]; then status="MISSING"; bad=$((bad + 1))
    elif [[ "$(sha256sum "$f" | cut -c1-12)" == "$hr" ]]; then status="current"
    else status="STALE"; bad=$((bad + 1)); fi
    printf '%-6s %-12s %s\n' "${n}x${n}" "$status" "$name"
  done
done
if (( bad )); then
  echo "ERROR: $bad installed icon(s) do not match $icons" >&2
  exit 1
fi

if command -v desktop-file-validate >/dev/null 2>&1; then
  if desktop-file-validate "$entry"; then
    echo "desktop-file-validate: clean"
  else
    echo "ERROR: the desktop entry does not parse cleanly" >&2
    exit 1
  fi
else
  echo "desktop-file-validate not installed: the entry was written but not parsed by anyone"
fi

# Another entry called Cwoffee wins or loses against ours depending on the search order,
# and a stale one whose Exec points at a deleted build is a launcher that appears to do
# nothing. Say which files the menu can pick from, because the symptom is indistinguishable
# from a broken install.
others=()
for d in /usr/share/applications /usr/local/share/applications "$apps"; do
  [[ -d "$d" ]] || continue
  for f in "$d"/*.desktop; do
    [[ -f "$f" && "$f" != "$entry" ]] || continue
    grep -q '^Name=Cwoffee$' "$f" 2>/dev/null && others+=("$f")
  done
done
if (( ${#others[@]} )); then
  echo "note: other entries are also called Cwoffee - the menu may be showing one of these:"
  for f in "${others[@]}"; do
    printf '  %s\n    ' "$f"
    grep -m1 '^Exec=' "$f" || echo "(no Exec)"
    echo
  done
  echo "  if one of them is an old install of this script, remove it: it points at a build tree."
fi

if (( launch_check )); then
  launch_check_run || exit 1
else
  echo "launch check: skipped (--no-launch-check). Nothing here has proved a window opens."
fi

echo
echo "installed $entry"
echo "  Exec=$root/cwoffee"
echo "  runs $app_bin with CWOFFEE_UI_DIST=$root/ui/dist"
echo "  Icon=cwoffee (hicolor, ${#sizes[@]} sizes, also installed as $app_id)"
if [[ -x "$gw_bin" ]]; then
  echo "  gateway sibling: $gw_bin - what Settings > Devices \"keep devices working after this"
  echo "  window closes\" starts instead of the window's own gateway (see scripts/gateway-handoff.mjs)"
fi
echo "  to update after a release build, run this script again; --remove uninstalls."
echo
echo "If the dock still shows the old artwork:"
echo "  * unpin and re-pin the launcher - GNOME caches the icon of a pinned entry"
echo "    against the .desktop file it was pinned with, and does not re-read it;"
echo "  * restart the app: a window keeps the icon it was given at map time;"
echo "  * check what the window reports and whether it matches StartupWMClass:"
echo "      xprop -class cwoffee-app WM_CLASS     # X11 / XWayland"
echo "      busctl --user list | grep -i cwoffee  # Wayland app id"
echo "  * drop any stale shell icon cache: rm -rf ~/.cache/icon-cache"
echo
echo "If the icon does nothing when clicked:"
echo "  * run the entry directly and read what the app prints:  gtk-launch cwoffee"
echo "      (or: $root/cwoffee)"
echo "  * check the entry the desktop sees, without guessing:"
echo "      grep -H '^Exec=' ~/.local/share/applications/cwoffee.desktop"
echo "      desktop-file-validate ~/.local/share/applications/cwoffee.desktop"
echo "  * re-run this script with --check-only to re-run the window check on its own."
echo "  * read what the app printed when the desktop started it - a launcher has no terminal,"
echo "    so its stderr lands in the journal:"
echo "      journalctl --user -b | grep -a cwoffee.desktop"
