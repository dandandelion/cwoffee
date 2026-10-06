#!/usr/bin/env bash
# Install a desktop entry + hicolor icons for a *locally built* cwoffee.
#
# Why this exists: the RPM/AppImage carry their own .desktop file and icons, but
# when you run target/release/cwoffee-app directly the window manager has nothing
# to look up, so GNOME shows a generic gear in the dock and the overview. The
# window itself also gets an icon at runtime (set_icon in main.rs); this covers
# the taskbar/dock half, which is a desktop-file lookup, not a window property.
#
# Installs into the user's own directories - no root, no system paths.
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
bin="${CWOFFEE_BIN:-$here/target/release/cwoffee-app}"
icons="$here/crates/tauri-app/icons"

if [[ ! -x "$bin" ]]; then
  echo "no binary at $bin - build it first (cargo build --release -p cwoffee-app)" >&2
  exit 1
fi

apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
ico="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor"
mkdir -p "$apps" "$ico"

# Two names per size. `cwoffee.png` is what the Icon= key below resolves to.
# `dev.cwoffee.app.png` is the app id GNOME sees on the running window, and GNOME's
# app-theming looks an icon up by that name - miss it and the dock shows a generic
# gear next to a perfectly good launcher.
app_id="dev.cwoffee.app"
sizes=()
for n in 16 24 32 48 64 96 128 256 512; do
  src="$icons/${n}x${n}.png"
  [[ -f "$src" ]] || continue
  mkdir -p "$ico/${n}x${n}/apps"
  cp "$src" "$ico/${n}x${n}/apps/cwoffee.png"
  cp "$src" "$ico/${n}x${n}/apps/${app_id}.png"
  sizes+=("$n")
done

cat > "$apps/cwoffee.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Cwoffee
GenericName=Co-work agent
Comment=Local-model co-work agent: chat, tools, sandboxed commands, and document output.
Exec=$bin
Icon=cwoffee
Terminal=false
Categories=Utility;
# Without this the WM cannot tie the running window back to this entry, and the
# dock keeps a generic icon plus a second launcher icon side by side.
StartupWMClass=cwoffee-app
EOF
chmod 644 "$apps/cwoffee.desktop"

command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$apps" || true
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -q -t -f "$ico"
else
  # No cache tool: at least make the theme directory look newer than its contents
  # so a shell that does cache icons re-reads it.
  touch "$ico"
fi

# Verify, because "we copied the files" and "the dock shows the new artwork" are
# different claims. This install used to succeed while every installed icon was the
# previous artwork, which is exactly the bug this script was blamed for.
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

echo "installed $apps/cwoffee.desktop"
echo "  Exec=$bin"
echo "  Icon=cwoffee (hicolor, ${#sizes[@]} sizes, also installed as $app_id)"
echo
echo "If the dock still shows the old artwork:"
echo "  * unpin and re-pin the launcher - GNOME caches the icon of a pinned entry"
echo "    against the .desktop file it was pinned with, and does not re-read it;"
echo "  * restart the app: a window keeps the icon it was given at map time;"
echo "  * check what the window reports and whether it matches StartupWMClass:"
echo "      xprop -class cwoffee-app WM_CLASS     # X11 / XWayland"
echo "      busctl --user list | grep -i cwoffee  # Wayland app id"
echo "  * drop any stale shell icon cache: rm -rf ~/.cache/icon-cache"
