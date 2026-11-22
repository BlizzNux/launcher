#!/usr/bin/env bash
# Runs the real parse_args, the fix command and fetch_umu from bin/blizznux-run and checks what the
# wrapper accepts, what it refuses, and what it leaves behind (issues #78 and #80).
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
# The fix command is internal (the app runs it through pkexec): its step ids come straight after it,
# and the command itself checks them, so nothing after "fix" is read as an option of the script.
accepts "fix keeps its ids" "fix||auto|pacman-python apt-python3" fix pacman-python apt-python3
accepts "fix --print keeps its flag" "fix||auto|--print pacman-python" fix --print pacman-python
accepts "fix after --offload" "fix||off|apt-update" --offload off fix apt-update

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

# ---- the fix command (issue #78) --------------------------------------------
# The app runs a whole fix as one "pkexec blizznux-run fix ID...". The ids are all the privileged
# side takes, and each id maps to one fixed command, so nothing here can become shell syntax.
# pacman, dnf, apt-get, dpkg and sed are stubs that record their argv: nothing is installed.
stub=$work/fix-bin
fixlog=$work/fix.log
mkdir -p "$stub" "$work/home"
for prog in pacman dnf apt-get dpkg sed; do
	cat > "$stub/$prog" <<'STUB'
#!/bin/sh
{ printf '%s' "${0##*/}"; for a in "$@"; do printf '\t%s' "$a"; done; printf '\n'; } >> "$FIX_LOG"
[ "${FIX_FAIL:-}" = "${0##*/}" ] && exit 7
exit 0
STUB
	chmod +x "$stub/$prog"
done

# runfix: the real wrapper, the stubbed programs, no terminal and no files of the user.
runfix() {
	HOME=$work/home XDG_CONFIG_HOME=$work/home/.config XDG_CACHE_HOME=$work/home/.cache \
		FIX_LOG=$fixlog FIX_FAIL=${FIX_FAIL:-} BLIZZNUX_NVIDIA_VERSION=${BLIZZNUX_NVIDIA_VERSION:-} \
		PATH=$stub:/usr/bin:/bin \
		bash bin/blizznux-run fix "$@"
}

# fixrun NAME WANT_RC WANT_OUT WANT_LOG ARGS...: one fix run. WANT_OUT is the whole output, WANT_LOG
# the whole list of commands run (one per line, argv tab separated); "-" means nothing at all.
fixrun() {
	local name=$1 rc=$2 out=$3 log=$4; shift 4
	local got gotrc=0 ran
	: > "$fixlog"
	got=$(runfix "$@" 2>&1) || gotrc=$?
	ran=$(cat "$fixlog")
	if [[ $gotrc != "$rc" ]]; then fail "$name" "want exit $rc, got $gotrc" "output: $got"; return; fi
	if [[ $out == - && -n $got ]]; then fail "$name" "want no output, got:" "$got"; return; fi
	if [[ $out != - && $got != "$out" ]]; then fail "$name" "want output:" "$out" "got:" "$got"; return; fi
	if [[ $log == - && -n $ran ]]; then fail "$name" "want nothing run, got:" "$ran"; return; fi
	if [[ $log != - && $ran != "$log" ]]; then fail "$name" "want commands:" "$log" "got:" "$ran"; return; fi
	ok "$name"
}

# fixrefuse NAME WANT_RC WANT_MESSAGE ARGS...: the run stops with a message, and nothing ran.
fixrefuse() {
	local name=$1 rc=$2 msg=$3; shift 3
	local got gotrc=0 ran
	: > "$fixlog"
	got=$(runfix "$@" 2>&1) || gotrc=$?
	ran=$(cat "$fixlog")
	if [[ $gotrc != "$rc" ]]; then fail "$name" "want exit $rc, got $gotrc" "output: $got"; return; fi
	if [[ $got != *"$msg"* ]]; then fail "$name" "no message '$msg':" "$got"; return; fi
	if [[ -n $ran ]]; then fail "$name" "nothing may run before the refusal, got:" "$ran"; return; fi
	ok "$name"
}

fixrun "fix: a package step runs its one fixed command" 0 \
	"+ pacman -S --needed --noconfirm python" \
	"$(printf 'pacman\t-S\t--needed\t--noconfirm\tpython')" pacman-python
fixrun "fix: a package name with an architecture keeps its own argv word" 0 \
	"+ apt-get install -y mesa-vulkan-drivers:i386" \
	"$(printf 'apt-get\tinstall\t-y\tmesa-vulkan-drivers:i386')" apt-vulkan-mesa
# The one step with a shell's punctuation in it: one argv word, quoted for display only.
fixrun "fix: the multilib sed script is one argument" 0 \
	"+ sed -i '/^#\[multilib\]/,/^#Include/ s/^#//' /etc/pacman.conf" \
	"$(printf 'sed\t-i\t/^#\\[multilib\\]/,/^#Include/ s/^#//\t/etc/pacman.conf')" sed-multilib
# One click, three steps, one call: this is the one-prompt case.
fixrun "fix: three ids run all three in order" 0 \
	"$(printf '+ dpkg --add-architecture i386\n+ apt-get update\n+ apt-get install -y mesa-vulkan-drivers:i386')" \
	"$(printf 'dpkg\t--add-architecture\ti386\napt-get\tupdate\napt-get\tinstall\t-y\tmesa-vulkan-drivers:i386')" \
	dpkg-i386 apt-update apt-vulkan-mesa
fixrun "fix: --print shows the commands and runs nothing" 0 \
	"$(printf 'pacman -Sy\napt-get update')" - --print pacman-sy apt-update
fixrun "fix: --print quotes like a run does" 0 \
	"sed -i '/^#\[multilib\]/,/^#Include/ s/^#//' /etc/pacman.conf" - --print sed-multilib

# The first failing step ends the run, with its own status; the steps after it do not run.
FIX_FAIL=pacman fixrun "fix: the first failing step ends the run" 7 \
	"+ pacman -Sy" "$(printf 'pacman\t-Sy')" pacman-sy pacman-lib32-vulkan-radeon

fixrefuse "fix: an unknown id is refused" 2 "unknown fix step 'nosuch'" nosuch
# Nothing runs before a mistyped word is found, however many known ids come first.
fixrefuse "fix: a word after a known id is refused, and nothing runs" 2 "unknown fix step 'extra'" \
	pacman-python extra
fixrefuse "fix: --print refuses an unknown id too" 2 "unknown fix step 'nosuch'" --print nosuch

# The NVIDIA package name comes from the driver version, digits and dots only, and never from
# anything else on the line.
printf 'NVRM version: NVIDIA UNIX x86_64 Kernel Module  595.45.04; touch %s/pwned\nextra 1.2\n' "$work" \
	> "$work/nvidia-version"
BLIZZNUX_NVIDIA_VERSION=$work/nvidia-version fixrun "fix: the NVIDIA package name carries only the driver major" 0 \
	"+ apt-get install -y libnvidia-gl-595:i386" \
	"$(printf 'apt-get\tinstall\t-y\tlibnvidia-gl-595:i386')" apt-vulkan-nvidia
[[ ! -e $work/pwned ]] && ok "fix: the driver version line cannot run anything" \
	|| fail "fix: the driver version line cannot run anything" "$work/pwned exists"
printf 'no version here\n' > "$work/nvidia-none"
gotrc=0
out=$(HOME=$work/home BLIZZNUX_NVIDIA_VERSION=$work/nvidia-none PATH=$stub:/usr/bin:/bin \
	FIX_LOG=$fixlog bash bin/blizznux-run fix apt-vulkan-nvidia 2>&1) || gotrc=$?
if [[ $gotrc == 1 && $out == *"NVIDIA driver version could not be read"* ]]; then
	ok "fix: a missing NVIDIA driver version stops with a message"
else
	fail "fix: a missing NVIDIA driver version stops with a message" "want exit 1 and a message, got $gotrc:" "$out"
fi

# The id table is what the app may name: nothing else reaches a command, not even a program name.
fixrefuse "fix: a program name is not an id" 2 "unknown fix step 'sh'" sh
fixrefuse "fix: a shell flag is not an id" 2 "unknown fix step '-c'" -c

# The line the user reads: bare words stay bare, anything a shell would split or interpret gets
# single quotes, and a quote inside a word survives them. No step id has such a word today, so the
# helper itself is checked here.
quoteline() {
	DEFS=$defs bash -c 'set -euo pipefail; eval "$DEFS"; fix_line "$@"' -- "$@"
}
quote_case() {
	local name=$1 want=$2; shift 2
	local got
	got=$(quoteline "$@") || { fail "$name" "exited non-zero"; return; }
	[[ $got == "$want" ]] && ok "$name" || fail "$name" "want: $want" "got:  $got"
}
quote_case "fix_line: plain words stay bare" "apt-get install -y mesa-vulkan-drivers:i386" \
	apt-get install -y mesa-vulkan-drivers:i386
quote_case "fix_line: a word with a space is quoted" "echo 'a b'" echo "a b"
quote_case "fix_line: an empty word is quoted" "echo ''" echo ""
quote_case "fix_line: a quote inside a word is escaped" "echo 'it'\\''s'" echo "it's"
quote_case "fix_line: shell punctuation is quoted" "echo 'a;b'" echo "a;b"
quote_case "fix_line: a sed script with spaces and brackets" \
	"sed -i '/^#\[multilib\]/,/^#Include/ s/^#//' /etc/pacman.conf" \
	sed -i "/^#\[multilib\]/,/^#Include/ s/^#//" /etc/pacman.conf


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
