#!/usr/bin/env bash
# Cuts one signed stable release containing everything merged to main since the last v* tag.
# Usage: scripts/release.sh [--dry-run] [version] [notes-file]
#   version     default: the next patch after the latest v* tag
#   notes-file  default: the titles of the pull requests merged since that tag, with links
#   --dry-run   (with an explicit version it also builds when nothing changed) work out the version and notes and do the full signed build, but create no
#               branch, pull request, merge, tag or release
# Exits 0 with "nothing to release" when no file that ends up in the bundles changed.
#
# Flow: release-v<ver> branch with the version bump -> pull request -> wait for its checks ->
# squash merge (--admin: the code-owner review rule is bypassed) -> build from the merged
# commit -> GitHub release v<ver> on that commit, as Latest. Nothing is ever pushed to main.
# Everything happens in .release-build/, a detached git worktree of origin/main, so the
# checkout this runs from is left alone.
#
# Needs: the updater signing key (TAURI_SIGNING_PRIVATE_KEY or ~/.config/blizznux-dev/signing.key),
#        gh logged in with write access to BlizzNux/launcher, git, python3, docker.
#        npm and cargo run inside the build image (scripts/container-build.sh).
set -euo pipefail
cd "$(dirname "$0")/.."
root=$PWD
repo=BlizzNux/launcher

dry=0
args=()
for a in "$@"; do
	case $a in
	--dry-run) dry=1 ;;
	-h | --help) sed -n '2,/^set -/p' "$0" | sed '$d;s/^# \{0,1\}//'; exit 0 ;;
	*) args+=("$a") ;;
	esac
done
ver=${args[0]:-}
notes=${args[1]:-}
key=${TAURI_SIGNING_PRIVATE_KEY:-$HOME/.config/blizznux-dev/signing.key}
[[ -f $key ]] || { echo "signing key not found: $key" >&2; exit 1; }
[[ -z $notes || -f $notes ]] || { echo "notes file not found: $notes" >&2; exit 1; }
[[ -z $ver || $ver =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "version must look like 0.2.40, got: $ver" >&2; exit 1; }
notes_abs=${notes:+$(realpath "$notes")}

# 1. what is on main, and what the last release was
git fetch -q origin main --tags
base=$(git rev-parse origin/main)
last=$(git tag --list 'v*' --sort=-v:refname | head -n1)
[[ -n $last ]] || { echo "no v* tag found" >&2; exit 1; }
git merge-base --is-ancestor "$last" "$base" || { echo "$last is not an ancestor of origin/main" >&2; exit 1; }

# Files that end up in the bundles: the app (src-tauri, except cargo's audit settings), the
# web UI, the wrapper script it ships (bin/), and the dependency manifests. CI, docs, the
# flatpak recipe, scripts and the install script do not count.
mapfile -t changed < <(git diff --name-only "$last" "$base" -- src-tauri ui bin package.json package-lock.json ':!src-tauri/.cargo')
if ((${#changed[@]} == 0)) && ! { ((dry)) && [[ -n $ver ]]; }; then
	echo "nothing to release: no launcher files changed since $last ($(git rev-parse --short "$base"))"
	exit 0
fi
if ((${#changed[@]} == 0)); then
	echo "no launcher files changed since $last; building anyway because a version was given to --dry-run"
else
	echo "launcher files changed since $last:"
	printf '  %s\n' "${changed[@]}"
fi

# 2. version and notes
if [[ -z $ver ]]; then
	IFS=. read -r ma mi pa <<<"${last#v}"
	ver="$ma.$mi.$((pa + 1))"
fi
tag=v$ver
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then echo "tag $tag already exists" >&2; exit 1; fi
notesfile=$(mktemp)
trap 'rm -f "$notesfile"' EXIT
if [[ -n $notes_abs ]]; then
	cp "$notes_abs" "$notesfile"
else
	{
		echo "Changes since $last:"
		echo
		while IFS= read -r subj; do
			[[ $subj =~ ^Release\ v ]] && continue
			if [[ $subj =~ ^(.*)\ \(#([0-9]+)\)$ ]]; then
				echo "- ${BASH_REMATCH[1]} ([#${BASH_REMATCH[2]}](https://github.com/$repo/pull/${BASH_REMATCH[2]}))"
			else
				echo "- $subj"
			fi
		done < <(git log --first-parent --format=%s "$last..$base")
	} >"$notesfile"
fi
echo "version: $ver (after $last)"
echo "notes:"
sed 's/^/  | /' "$notesfile"

# 3. the build tree: a detached worktree that keeps node_modules and the cargo target between runs
bt=$root/.release-build
if [[ -e $bt/.git ]]; then
	git -C "$bt" checkout -q -f --detach "$base"
	git -C "$bt" clean -fdq
else
	git worktree prune
	git worktree add -q -f --detach "$bt" "$base"
fi

bump() { # version in the manifests, the lock file and the wrapper script
	local v=$1
	sed -i "s/^version = \".*\"/version = \"$v\"/" src-tauri/Cargo.toml
	sed -i "s/^VERSION=.*/VERSION=$v/" bin/blizznux-run
	python3 - "$v" <<'PY'
import json, pathlib, re, sys
v = sys.argv[1]
for f in ("src-tauri/tauri.conf.json", "package.json"):
    p = pathlib.Path(f); d = json.loads(p.read_text()); d["version"] = v; p.write_text(json.dumps(d, indent=2) + "\n")
name = re.search(r'^name = "(.*)"', pathlib.Path("src-tauri/Cargo.toml").read_text(), re.M).group(1)
lock = pathlib.Path("src-tauri/Cargo.lock")
s = re.sub(r'(\[\[package\]\]\nname = "%s"\nversion = ")[^"]*"' % re.escape(name), r'\g<1>%s"' % v, lock.read_text(), count=1)
lock.write_text(s)
PY
}

cd "$bt"
if ((dry)); then
	bump "$ver"
else
	branch=release-$tag
	if git ls-remote --exit-code --heads origin "$branch" >/dev/null 2>&1; then
		# left over from a failed run: close its pull request and drop the branch
		for n in $(gh pr list -R "$repo" --head "$branch" --state open --json number --jq '.[].number'); do
			gh pr close -R "$repo" "$n" >/dev/null
		done
		git push -q origin --delete "$branch"
	fi
	git checkout -q -B "$branch" "$base"
	bump "$ver"
	git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/tauri.conf.json package.json bin/blizznux-run
	git -c commit.gpgsign=false commit -q -m "Release $tag"
	git push -q origin "$branch"
	pr=$(gh pr create -R "$repo" --base main --head "$branch" --title "Release $tag" \
		--body "Version bump for $tag. Opened and merged by scripts/release.sh." | sed 's|.*/||')
	echo "opened pull request $pr, waiting for its checks"

	# wait until checks have appeared and none is pending; fail on any failure
	for ((i = 0; i < 270; i++)); do
		sleep 20
		out=$(gh pr checks "$pr" -R "$repo" --json name,bucket 2>/dev/null || true)
		[[ -n $out && $out != "[]" ]] || continue
		state=$(python3 -c '
import json, sys
b = [c["bucket"] for c in json.loads(sys.argv[1])]
print("fail" if any(x in ("fail", "cancel") for x in b) else "wait" if "pending" in b else "pass")' "$out")
		[[ $state == pass ]] && break
		if [[ $state == fail ]]; then echo "checks failed on pull request $pr; branch $branch left open" >&2; exit 1; fi
	done
	[[ ${state:-} == pass ]] || { echo "checks did not finish in time on pull request $pr" >&2; exit 1; }

	gh pr merge "$pr" -R "$repo" --squash --admin --delete-branch
	sha=$(gh pr view "$pr" -R "$repo" --json mergeCommit --jq .mergeCommit.oid)
	git fetch -q origin main
	git checkout -q -f --detach "$sha"
	echo "merged pull request $pr as $(git rev-parse --short "$sha")"
fi

# 4. signed bundles (AppImage gets a .sig; deb/rpm are built too), built on an old LTS base
#    so that they run on older distros and not only on ones as new as this machine
rm -rf src-tauri/target/container/release/bundle
# this checkout's build scripts, applied to the sources of the build tree
BLIZZNUX_SRC=$bt TAURI_SIGNING_PRIVATE_KEY="$key" "$root/scripts/container-build.sh"
b=src-tauri/target/container/release/bundle
app=$(ls "$b"/appimage/*_"$ver"_*.AppImage | head -1); sig="$app.sig"
deb=$(ls "$b"/deb/*_"$ver"_*.deb | head -1); rpm=$(ls "$b"/rpm/*-"$ver"-*.rpm | head -1)
[[ -f $app && -f $deb && -f $rpm ]] || { echo "bundles for $ver not found under $b" >&2; exit 1; }
[[ -f $sig ]] || { echo "no signature produced for $app" >&2; exit 1; }

# 5. updater manifest the app checks: releases/latest/download/latest.json
url="https://github.com/$repo/releases/download/$tag/$(basename "$app")"
python3 - "$ver" "$sig" "$url" "$notesfile" >"$b/latest.json" <<'PY'
import json, sys, datetime, pathlib
ver, sig, url, notes = sys.argv[1:5]
print(json.dumps({
    "version": ver,
    "notes": pathlib.Path(notes).read_text().strip(),
    "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "platforms": {"linux-x86_64": {"signature": pathlib.Path(sig).read_text().strip(), "url": url}},
}, indent=2))
PY

echo "built under $bt/$b:"
ls -l "$app" "$sig" "$deb" "$rpm" "$b/latest.json"

if ((dry)); then
	echo "would release $tag"
	exit 0
fi

# 6. publish, targeting the merged commit
gh release create "$tag" "$app" "$sig" "$deb" "$rpm" "$b/latest.json" -R "$repo" \
	--title "$tag" --notes-file "$notesfile" --target "$(git rev-parse HEAD)" --latest
echo "published $tag: $(gh release view "$tag" -R "$repo" --json url --jq .url)"
