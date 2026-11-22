#!/usr/bin/env bash
# Runs the real parse_args and fetch_umu from bin/blizznux-run and checks what the wrapper accepts,
# what it refuses, and what it leaves behind (issue #80).
# shellcheck disable=SC2016,SC2329
set -euo pipefail
cd "$(dirname "$0")/.."

fails=0
ok() { echo "ok   $1"; }
fail() {
	echo "FAIL $1"
	shift
	printf '  %s\n' "$@"
	fails=1
}

# defs: the wrapper's functions and defaults, up to the line that starts it (load_config). The tests
# run the real die, usage and parse_args, not copies of them.
defs=$(sed '/^load_config$/,$d' bin/blizznux-run)

# parse ARGS...: runs the wrapper's own parse_args and prints "command|game|offload|leftover args".
parse() {
	DEFS=$defs bash -c '
		set -euo pipefail
		eval "$DEFS"
		APP=blizznux; VERSION=test-version
		LOGGING=""
		cmd=; GAME=; OFFLOAD=auto; ARGS=()
		parse_args "$@"
		printf "%s|%s|%s|%s\n" "$cmd" "$GAME" "$OFFLOAD" "${ARGS[*]-}"' -- "$@"
}

# accepts NAME WANT ARGS...: the parser runs the command and leaves WANT ("command|game|offload|args").
accepts() {
	local name=$1 want=$2; shift 2
	local got
	got=$(parse "$@" 2>&1) || { fail "$name" "exited non-zero:" "$got"; return; }
	[[ $got == "$want" ]] && ok "$name" || fail "$name" "want: $want" "got:  $got"
}

# refuses NAME WANT_RC WANT_MESSAGE ARGS...: the parser stops with a message and the usage.
refuses() {
	local name=$1 rc=$2 want=$3; shift 3
	local got gotrc=0
	got=$(parse "$@" 2>&1) || gotrc=$?
	if [[ $gotrc != "$rc" ]]; then fail "$name" "want exit $rc, got $gotrc:" "$got"; return; fi
	if [[ $got != *"$want"* ]]; then fail "$name" "no message '$want':" "$got"; return; fi
	if [[ $got != *Usage:* || $got != *blizznux-run\ install* ]]; then fail "$name" "no usage:" "$got"; return; fi
	ok "$name"
}

# The paths every user takes, and the options the app passes (--game CODE, dpi N).
accepts "no arguments: a plain launch" "launch||auto|"
accepts "--game" "launch|WoW|auto|" --game WoW
accepts "--game with --offload" "launch|WTCG|off|" --game WTCG --offload off
accepts "--offload on" "launch||on|" --offload on
accepts "--offload auto" "launch||auto|" --offload auto
accepts "install" "install||auto|" install
accepts "doctor after --offload" "doctor||off|" --offload off doctor
accepts "dpi keeps its number" "dpi||auto|144" dpi 144
accepts "dpi without a number" "dpi||auto|" dpi
accepts "dpi after --offload" "dpi||off|144" --offload off dpi 144

# A mistyped option is not silently a launch, and --offload takes only its three values.
refuses "unknown long option" 2 "unknown option '--gmae'" --gmae WoW
refuses "unknown short option" 2 "unknown option '-x'" -x
refuses "bad --offload value" 2 "not 'of'" --offload of
refuses "--offload value is not upper case" 2 "not 'ON'" --offload ON
refuses "--offload without a value" 2 "--offload needs on|off|auto" --offload
refuses "--game without a code" 2 "--game needs a code" --game
refuses "--game with an empty code" 2 "--game needs a code" --game ""

# Neither a mistyped command nor extra words reach the command: they are refused, not ignored.
refuses "mistyped command" 2 "unexpected argument 'instal'" instal
refuses "launch takes no words" 2 "unexpected argument 'extra'" launch extra
refuses "install takes no words" 2 "unexpected argument 'extra'" install extra
refuses "dpi takes at most one number" 2 "unexpected argument '200'" dpi 144 200

# fetch_umu: every way out of the download removes its temp folder (TMPDIR here), and the success
# path still installs umu-run. curl, sha256sum and install are stubbed; nothing goes to the network.
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

mkdir -p "$work/src/umu"
printf '#!/bin/sh\necho umu\n' > "$work/src/umu/umu-run"
tar -cf "$work/umu.tar" -C "$work/src" umu
printf 'not a tarball\n' > "$work/bad.tar"
sha=$(sha256sum "$work/umu.tar" | cut -d' ' -f1)

cat > "$work/fetch.sh" <<'CHILD'
set -euo pipefail
eval "$DEFS"
APP=blizznux
LOGGING=""
UMU_VERSION=9.9.9
UMU_DIR=$CHILD_DIR/umu
UMU_URL=https://example.invalid/umu.tar
UMU_SHA256=$WANT_SHA

fake_curl() {
	local out=""
	while (( $# )); do
		case $1 in
		-o) out=$2; shift 2 ;;
		*) shift ;;
		esac
	done
	cp "$ARCHIVE" "$out"
}
case $STUB in
download) curl() { return 22; } ;;
badfile) curl() { fake_curl "$@"; } ;;
badtar) curl() { fake_curl "$@"; }; sha256sum() { :; } ;;
badinstall) curl() { fake_curl "$@"; }; install() { return 1; } ;;
ok) curl() { fake_curl "$@"; } ;;
esac

fetch_umu
CHILD

last=""
cases=0
# umu NAME WANT_RC STUB WANT_MESSAGE [ARCHIVE]: prints nothing; checks the exit code ("any" when
# any failure will do), the message (or "-" for none) and that the temp folder is empty afterwards.
umu() {
	local name=$1 rc=$2 stub=$3 want=$4 archive=${5:-$work/umu.tar}
	local out gotrc=0 left problems=()
	last=$work/case-$((cases += 1))
	mkdir -p "$last/tmp"
	out=$(TMPDIR=$last/tmp CHILD_DIR=$last ARCHIVE=$archive WANT_SHA=$sha STUB=$stub DEFS=$defs \
		bash "$work/fetch.sh" 2>&1) || gotrc=$?
	left=$(find "$last/tmp" -mindepth 1)
	if [[ $rc == any ]]; then
		(( gotrc != 0 )) || problems+=("want a failure, got exit 0")
	else
		[[ $gotrc == "$rc" ]] || problems+=("want exit $rc, got $gotrc")
	fi
	[[ $want == - || $out == *"$want"* ]] || problems+=("no message '$want'")
	[[ -z $left ]] || problems+=("temp folder left behind: $left")
	if (( ${#problems[@]} )); then fail "$name" "${problems[@]}" "output: $out"; else ok "$name"; fi
}

umu "fetch_umu: a failed download leaves no temp files" 1 download "could not download umu-launcher"
umu "fetch_umu: a checksum mismatch leaves no temp files" 1 badfile "damaged (checksum mismatch)" "$work/bad.tar"
# A failed tar is not wrapped in die: set -e ends the script with tar's own status, so any failure counts.
umu "fetch_umu: a failed tar leaves no temp files" any badtar - "$work/bad.tar"
umu "fetch_umu: a failed install leaves no temp files" 1 badinstall -
umu "fetch_umu: success leaves no temp files" 0 ok -
if [[ -x $last/umu/umu-run && $(cat "$last/umu/version" 2>/dev/null) == 9.9.9 ]]; then
	ok "fetch_umu: success still installs umu-run"
else
	fail "fetch_umu: success still installs umu-run" "no executable $last/umu/umu-run or wrong version"
fi

# The path in the trap is not shell syntax: a space in TMPDIR still cleans up. Uses the tar path,
# where set -e comes out of fetch_umu first, so the trap cannot rely on the function's local.
space="$work/with space"
mkdir -p "$space/tmp"
gotrc=0
out=$(TMPDIR=$space/tmp CHILD_DIR=$space WANT_SHA=$sha STUB=badtar ARCHIVE=$work/bad.tar DEFS=$defs \
	bash "$work/fetch.sh" 2>&1) || gotrc=$?
left=$(find "$space/tmp" -mindepth 1)
if (( gotrc != 0 )) && [[ -z $left ]]; then
	ok "fetch_umu: a space in the temp path still cleans up"
else
	fail "fetch_umu: a space in the temp path still cleans up" "want a failure and nothing left, got exit $gotrc, left: $left" "output: $out"
fi

exit $fails
