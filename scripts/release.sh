#!/usr/bin/env bash
# Builds signed release bundles, writes the updater manifest, and publishes a GitHub release.
# Usage: scripts/release.sh <version> [notes-file]
# Needs: the updater signing key (TAURI_SIGNING_PRIVATE_KEY or ~/.config/blizznux-dev/signing.key),
#        gh logged in with push access to BlizzNux/launcher, npm deps installed.
set -euo pipefail
cd "$(dirname "$0")/.."
ver=${1:?version, e.g. 0.2.1}
notes=${2:-}
key=${TAURI_SIGNING_PRIVATE_KEY:-$HOME/.config/blizznux-dev/signing.key}
[[ -f $key ]] || { echo "signing key not found: $key" >&2; exit 1; }

# 1. version in the three manifests
sed -i "s/^version = \".*\"/version = \"$ver\"/" src-tauri/Cargo.toml
python3 - "$ver" <<'PY'
import json, pathlib, sys
v = sys.argv[1]
for f in ("src-tauri/tauri.conf.json", "package.json"):
    p = pathlib.Path(f); d = json.loads(p.read_text()); d["version"] = v; p.write_text(json.dumps(d, indent=2) + "\n")
PY
(cd src-tauri && cargo generate-lockfile --offline >/dev/null 2>&1 || true)

# 2. signed bundles (AppImage gets a .sig; deb/rpm are built too)
TAURI_SIGNING_PRIVATE_KEY="$key" TAURI_SIGNING_PRIVATE_KEY_PASSWORD="" npx tauri build
b=src-tauri/target/release/bundle
app=$(ls "$b"/appimage/*_"$ver"_*.AppImage | head -1); sig="$app.sig"
deb=$(ls "$b"/deb/*_"$ver"_*.deb | head -1); rpm=$(ls "$b"/rpm/*-"$ver"-*.rpm | head -1)
[[ -f $app && -f $deb && -f $rpm ]] || { echo "bundles for $ver not found under $b" >&2; exit 1; }
[[ -f $sig ]] || { echo "no signature produced for $app" >&2; exit 1; }

# 3. updater manifest the app checks: releases/latest/download/latest.json
url="https://github.com/BlizzNux/launcher/releases/download/v$ver/$(basename "$app")"
python3 - "$ver" "$sig" "$url" "$notes" > "$b/latest.json" <<'PY'
import json, sys, datetime, pathlib
ver, sig, url, notes = sys.argv[1:5]
body = pathlib.Path(notes).read_text() if notes and pathlib.Path(notes).is_file() else f"BlizzNux Launcher v{ver}"
print(json.dumps({
    "version": ver,
    "notes": body,
    "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "platforms": {"linux-x86_64": {"signature": pathlib.Path(sig).read_text().strip(), "url": url}},
}, indent=2))
PY

# 4. commit the version bump, tag, publish
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/tauri.conf.json package.json
git -c commit.gpgsign=false commit -q -m "Release v$ver" || true
git push -q origin HEAD
args=(--title "v$ver")
[[ -n $notes && -f $notes ]] && args+=(--notes-file "$notes") || args+=(--generate-notes)
gh release create "v$ver" "$app" "$sig" "$deb" "$rpm" "$b/latest.json" "${args[@]}"
echo "published v$ver: $(gh release view "v$ver" --json url --jq .url)"
