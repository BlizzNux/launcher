#!/usr/bin/env bash
# Installs blizznux-run for the current user: binary, desktop entry and icon.
# Usage: ./install.sh          (install)      ./install.sh --uninstall
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
bin=${XDG_BIN_HOME:-$HOME/.local/bin}
data=${XDG_DATA_HOME:-$HOME/.local/share}
apps=$data/applications
icons=$data/icons/hicolor
id=com.blizznux.launcher

if [[ ${1:-} == --uninstall ]]; then
	rm -f "$bin/blizznux-run" "$apps/$id.desktop" "$icons"/*/apps/$id.png
	command -v update-desktop-database >/dev/null && update-desktop-database "$apps" 2>/dev/null || true
	echo "blizznux-run removed (your prefix and config were left alone)"
	exit 0
fi

mkdir -p "$bin" "$apps"
install -m 755 "$here/bin/blizznux-run" "$bin/blizznux-run"
install -m 644 "$here/share/applications/$id.desktop" "$apps/$id.desktop"
for d in "$here"/share/icons/hicolor/*/apps; do
	size=$(basename "$(dirname "$d")")
	mkdir -p "$icons/$size/apps"
	install -m 644 "$d/$id.png" "$icons/$size/apps/$id.png"
done
command -v update-desktop-database >/dev/null && update-desktop-database "$apps" 2>/dev/null || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t "$data/icons/hicolor" 2>/dev/null || true
echo "installed: $bin/blizznux-run, app menu entry 'BlizzNux'"
case ":$PATH:" in *":$bin:"*) ;; *) echo "note: $bin is not on your PATH; the app menu entry still works" ;; esac
