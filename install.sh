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
	rm -f "$bin/blizznux-run" "$bin/blizznux" "$apps/$id.desktop" "$icons"/*/apps/$id.png
	command -v update-desktop-database >/dev/null && update-desktop-database "$apps" 2>/dev/null || true
	echo "blizznux-run removed (your prefix and config were left alone)"
	exit 0
fi

mkdir -p "$bin" "$apps"
install -m 755 "$here/bin/blizznux-run" "$bin/blizznux-run"
app="$here/src-tauri/target/release/blizznux"
if [[ -x $app ]]; then
	# Desktop app built with `npm run build` / `cargo build --release`: the menu entry opens it.
	install -m 755 "$app" "$bin/blizznux"
	install -m 644 "$here/share/applications/$id.desktop" "$apps/$id.desktop"
else
	# Script only: the menu entry launches Battle.net directly.
	sed 's/^Exec=blizznux$/Exec=blizznux-run/' "$here/share/applications/$id.desktop" > "$apps/$id.desktop"
	chmod 644 "$apps/$id.desktop"
fi
for d in "$here"/share/icons/hicolor/*/apps; do
	size=$(basename "$(dirname "$d")")
	mkdir -p "$icons/$size/apps"
	install -m 644 "$d/$id.png" "$icons/$size/apps/$id.png"
done
command -v update-desktop-database >/dev/null && update-desktop-database "$apps" 2>/dev/null || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t "$data/icons/hicolor" 2>/dev/null || true
if [[ -x $bin/blizznux ]]; then echo "installed: BlizzNux app ($bin/blizznux) + blizznux-run; app menu entry 'BlizzNux' opens the app"; else echo "installed: $bin/blizznux-run; app menu entry 'BlizzNux' launches Battle.net (build the app for the full launcher)"; fi
case ":$PATH:" in *":$bin:"*) ;; *) echo "note: $bin is not on your PATH; the app menu entry still works" ;; esac
