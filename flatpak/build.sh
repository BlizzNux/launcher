#!/usr/bin/env bash
# Builds and installs the BlizzNux Flatpak for the current user.
# Needs flatpak-builder, the flathub remote, and a release build of the app
# (npm run build, or cargo build --release in src-tauri).
set -euo pipefail
cd "$(dirname "$0")"
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak-builder --user --install --install-deps-from=flathub --force-clean .build com.blizznux.launcher.yml
echo "run it with: flatpak run com.blizznux.launcher"
