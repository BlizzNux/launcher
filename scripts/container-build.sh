#!/usr/bin/env bash
# Builds the bundles inside the Ubuntu 22.04 build image (scripts/build-env/Dockerfile), so the
# AppImage, deb and rpm run on older distros too and not only on ones as new as this machine.
# Usage: scripts/container-build.sh [tauri build arguments...]
# Needs: docker. Set TAURI_SIGNING_PRIVATE_KEY to the key file to get a signed AppImage.
#        node_modules is installed (npm ci) inside the image when it is missing.
# Inside a container (the weekly release job): docker bind mounts resolve on the Docker HOST,
#        so the paths are translated through /proc/self/mountinfo. Override with
#        BLIZZNUX_HOST_PATH_MAP="<path here>=<same path on the host>" (set but empty: no
#        translation). On a normal machine nothing is translated.
# Output: src-tauri/target/container/release (the binary) and .../release/bundle (the bundles).
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
cd "${BLIZZNUX_SRC:-$here}" # BLIZZNUX_SRC: build another checkout of the sources with these scripts
# The path the Docker daemon sees for a path in this process. Identity unless we run in a
# container (/.dockerenv) with a bind mount whose host source differs from its mount point.
host_path() {
	local p
	p=$(realpath -m "$1")
	if [[ -v BLIZZNUX_HOST_PATH_MAP ]]; then
		local from=${BLIZZNUX_HOST_PATH_MAP%%=*} to=${BLIZZNUX_HOST_PATH_MAP#*=}
		if [[ -n $BLIZZNUX_HOST_PATH_MAP && ( $p == "$from" || $p == "$from"/* ) ]]; then p=$to${p#"$from"}; fi
		printf '%s\n' "$p"
		return
	fi
	[[ -e /.dockerenv ]] || { printf '%s\n' "$p"; return; }
	awk -v p="$p" '
		{ for (i = 7; i <= NF && $i != "-"; i++) ; fs = $(i + 1); mp = $5; root = $4 }
		fs == "overlay" || fs == "tmpfs" || fs == "proc" || fs == "sysfs" { next }
		p == mp || index(p, mp == "/" ? "/" : mp "/") == 1 {
			if (length(mp) >= best) { best = length(mp); rest = substr(p, length(mp) + 1)
				src = (root == "/" ? "" : root) rest }
		}
		END { print (best > 0 && src != "") ? src : p }' /proc/self/mountinfo
}

image=blizznux-build:jammy
docker image inspect "$image" >/dev/null 2>&1 || docker build -t "$image" "$here/scripts/build-env"

cache=${XDG_CACHE_HOME:-$HOME/.cache}/blizznux-build
mkdir -p "$cache/cargo" "$cache/home"
run=(--rm -u "$(id -u):$(id -g)" -w /src -v "$(host_path "$PWD"):/src" -v "$(host_path "$cache/cargo"):/cargo" -v "$(host_path "$cache/home"):/home/build"
	-e HOME=/home/build -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/src/src-tauri/target/container
	# The AppImage tools are AppImages themselves and a container has no FUSE to mount them.
	-e APPIMAGE_EXTRACT_AND_RUN=1)
key=${TAURI_SIGNING_PRIVATE_KEY:-}
if [[ -n $key ]]; then
	[[ -f $key ]] || { echo "signing key not found: $key" >&2; exit 1; }
	run+=(-v "$(host_path "$key"):/run/signing.key:ro" -e TAURI_SIGNING_PRIVATE_KEY=/run/signing.key -e TAURI_SIGNING_PRIVATE_KEY_PASSWORD=)
fi
[[ -d node_modules ]] || docker run "${run[@]}" "$image" npm ci
docker run "${run[@]}" "$image" npx tauri build "$@"
