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

for n in 16 32 48 64 128 256 512; do
  src="$icons/${n}x${n}.png"
  [[ -f "$src" ]] || continue
  mkdir -p "$ico/${n}x${n}/apps"
  cp "$src" "$ico/${n}x${n}/apps/cwoffee.png"
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
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t -f "$ico" || true

echo "installed $apps/cwoffee.desktop"
echo "  Exec=$bin"
echo "  Icon=cwoffee (hicolor, $(ls -1 "$ico"/*/apps/cwoffee.png 2>/dev/null | wc -l) sizes)"
echo
echo "If the dock still shows a gear, check what the window actually reports:"
echo "  xprop -class cwoffee-app WM_CLASS   # must match StartupWMClass"
