#!/usr/bin/env bash
# Keeps an eye on the two things the launcher pins in bin/blizznux-run: Proton-CachyOS and umu-launcher.
# Usage: scripts/pin-watch.sh check             Move the pin in the working tree to the newest upstream
#                                               release, with its checksum. Prints one line per change
#                                               ("Proton-CachyOS OLD NEW"); prints nothing when both are current.
#        scripts/pin-watch.sh propose FILE      Put that change on a branch and propose it in public: as a
#                                               pull request, or as an issue with the link that opens one
#                                               where workflows may not open pull requests. FILE is the
#                                               output of "check".
# Run once a day by .github/workflows/pin-watch.yml. A proposal is only that: the pin moves in a release
# after Battle.net and the games ran on the new build.
# Needs: curl, python3, sha256sum, sha512sum; "propose" also git and gh (GH_TOKEN).
set -euo pipefail
cd "$(dirname "$0")/.."
run=bin/blizznux-run
flatpak=flatpak/com.blizznux.launcher.yml

auth=()
[[ -z ${GITHUB_TOKEN:-} ]] || auth=(-H "Authorization: Bearer $GITHUB_TOKEN")
latest() {
	curl -fsSL --retry 3 "${auth[@]}" "https://api.github.com/repos/$1/releases/latest" \
		| python3 -c 'import json, sys; print(json.load(sys.stdin)["tag_name"])'
}
pinned() { sed -n "s/^$1=//p" "$run" | head -1; }

check() {
	local tmp; tmp=$(mktemp -d)
	trap "rm -rf '$tmp'" EXIT
	local cur new name url sum mb

	cur=$(pinned PROTON_VERSION); new=$(latest CachyOS/proton-cachyos)
	if [[ $new != "$cur" ]]; then
		# The build for any distribution; its checksum file comes with the release.
		name=proton-$new-x86_64
		url=https://github.com/CachyOS/proton-cachyos/releases/download/$new/$name
		curl -fL --retry 3 -sS -o "$tmp/$name.tar.xz" "$url.tar.xz"
		curl -fL --retry 3 -sS -o "$tmp/$name.sha512sum" "$url.sha512sum"
		(cd "$tmp" && sha512sum -c --quiet "$name.sha512sum") >&2
		sum=$(sha256sum "$tmp/$name.tar.xz" | cut -d' ' -f1)
		mb=$(( ($(stat -c %s "$tmp/$name.tar.xz") + 1048575) / 1048576 ))
		sed -i "s/^PROTON_VERSION=.*/PROTON_VERSION=$new/; s/^PROTON_SHA256=.*/PROTON_SHA256=$sum/; s/^PROTON_MB=.*/PROTON_MB=$mb/" "$run"
		echo "Proton-CachyOS $cur $new"
	fi

	cur=$(pinned UMU_VERSION); new=$(latest Open-Wine-Components/umu-launcher)
	if [[ $new != "$cur" ]]; then
		url=https://github.com/Open-Wine-Components/umu-launcher/releases/download/$new/umu-launcher-$new-zipapp.tar
		curl -fL --retry 3 -sS -o "$tmp/umu.tar" "$url"
		tar -tf "$tmp/umu.tar" | grep -qx 'umu/umu-run' || { echo "umu-launcher $new: no umu/umu-run in its zipapp" >&2; exit 1; }
		sum=$(sha256sum "$tmp/umu.tar" | cut -d' ' -f1)
		sed -i "s/^UMU_VERSION=.*/UMU_VERSION=$new/; s/^UMU_SHA256=.*/UMU_SHA256=$sum/" "$run"
		# The Flatpak carries the same release: its address, and the checksum on the line after it.
		sed -i -E "/umu-launcher\/releases\/download\//{s#download/[^/]+/umu-launcher-.*-zipapp\.tar#download/$new/umu-launcher-$new-zipapp.tar#;n;s/sha256: .*/sha256: $sum/}" "$flatpak"
		echo "umu-launcher $cur $new"
	fi
}

propose() {
	local changes=${1:?usage: scripts/pin-watch.sh propose FILE}
	[[ -s $changes ]] || { echo "nothing to propose"; return; }
	local what old new title="" branch="" rows=""
	while read -r what old new; do
		title+="${title:+, }$what $new"
		branch+="${branch:+_}$new"
		rows+="| $what | \`$old\` | \`$new\` |"$'\n'
	done < "$changes"
	title="Move the pin: $title"
	branch=pin/$branch
	if git ls-remote --exit-code --heads origin "$branch" >/dev/null 2>&1; then
		echo "already proposed on $branch"
		return
	fi
	# The maintainer's own name, as on every commit of this repository.
	local name email
	name=$(git log -1 --format=%an); email=$(git log -1 --format=%ae)
	git checkout -q -b "$branch"
	git -c user.name="$name" -c user.email="$email" commit -q -a -m "$title" \
		-m "A newer upstream release than the pinned one, with the checksum of its download. The wrapper downloaded and unpacked it. Not yet run with Battle.net and the games."
	git push -q origin "$branch"

	local body; body=$(mktemp)
	cat > "$body" <<-BODY
	A newer release is out than the one the launcher pins.

	| | pinned | new |
	|---|---|---|
	$rows
	Checked by the job that opened this: the launcher's wrapper downloads the new release, verifies it against the checksum pinned here and unpacks it. For Proton, the download was also compared with the checksum published with the release.

	Not checked: whether Battle.net and the games run on it. That is tested on the maintainer's machine first, and the result is posted here. The pin only moves in a launcher release after that.
	BODY
	if gh pr create --base main --head "$branch" --title "$title" --body-file "$body"; then
		return
	fi
	# This repository does not let workflows open pull requests: an issue carries the link that opens it.
	local repo; repo=$(gh repo view --json nameWithOwner --jq .nameWithOwner)
	printf '\nThe change is on the branch `%s`. Open the pull request: https://github.com/%s/compare/main...%s?expand=1\n' \
		"$branch" "$repo" "$branch" >> "$body"
	gh issue create --title "$title" --body-file "$body"
}

case ${1:-} in
check) check ;;
propose) propose "${2:-}" ;;
*) sed -n '2,13p' "$0" >&2; exit 2 ;;
esac
