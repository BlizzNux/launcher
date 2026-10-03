#!/usr/bin/env bash
# Builds the bundles inside the Ubuntu 22.04 build image (scripts/build-env/Dockerfile), so the
# AppImage, deb and rpm run on older distros too and not only on ones as new as this machine.
# Usage: scripts/container-build.sh [tauri build arguments...]
# Needs: docker, npm deps installed (node_modules). Set TAURI_SIGNING_PRIVATE_KEY to the key
#        file to get a signed AppImage.
# Output: src-tauri/target/container/release (the binary) and .../release/bundle (the bundles).
set -euo pipefail
cd "$(dirname "$0")/.."
image=blizznux-build:jammy
docker image inspect "$image" >/dev/null 2>&1 || docker build -t "$image" scripts/build-env

cache=${XDG_CACHE_HOME:-$HOME/.cache}/blizznux-build
mkdir -p "$cache/cargo" "$cache/home"
run=(--rm -u "$(id -u):$(id -g)" -w /src -v "$PWD:/src" -v "$cache/cargo:/cargo" -v "$cache/home:/home/build"
	-e HOME=/home/build -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/src/src-tauri/target/container
	# The AppImage tools are AppImages themselves and a container has no FUSE to mount them.
	-e APPIMAGE_EXTRACT_AND_RUN=1)
key=${TAURI_SIGNING_PRIVATE_KEY:-}
if [[ -n $key ]]; then
	[[ -f $key ]] || { echo "signing key not found: $key" >&2; exit 1; }
	run+=(-v "$key:/run/signing.key:ro" -e TAURI_SIGNING_PRIVATE_KEY=/run/signing.key -e TAURI_SIGNING_PRIVATE_KEY_PASSWORD=)
fi
docker run "${run[@]}" "$image" npx tauri build "$@"
