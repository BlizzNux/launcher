#!/usr/bin/env bash
# Runs the real hide_cpu_vulkan from bin/blizznux-run against fake Vulkan ICD folders and checks
# the VK_LOADER_DRIVERS_DISABLE it leaves behind.
set -euo pipefail
cd "$(dirname "$0")/.."

fails=0
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
# icd DIR FILE LIBRARY: a fake manifest.
icd() { mkdir -p "$1"; printf '{"file_format_version":"1.0.0","ICD":{"library_path":"%s","api_version":"1.3.0"}}\n' "$3" >"$1/$2"; }
# run NAME EXPECTED DIR [VAR=value ...]: EXPECTED is the resulting VK_LOADER_DRIVERS_DISABLE ("" = left unset).
run() {
	local name=$1 want=$2 dir=$3; shift 3
	local got
	got=$(env -u VK_LOADER_DRIVERS_DISABLE -u VK_DRIVER_FILES -u VK_ICD_FILENAMES -u VK_LOADER_DRIVERS_SELECT \
		BLIZZNUX_ICD_DIRS="$dir" XDG_DATA_HOME="$tmp/none" XDG_DATA_DIRS="" "$@" bash -c '
		set -euo pipefail
		eval "$(sed -n "/^hide_cpu_vulkan()/,/^}/p" bin/blizznux-run)"
		log() { :; }
		hide_cpu_vulkan
		printf "%s" "${VK_LOADER_DRIVERS_DISABLE:-}"')
	if [[ $got == "$want" ]]; then echo "ok   $name"; else echo "FAIL $name"; echo "  want: $want"; echo "  got:  $got"; fails=1; fi
}

icd "$tmp/a" lvp_icd.json libvulkan_lvp.so; icd "$tmp/a" radeon_icd.json libvulkan_radeon.so
run "lvp + radeon" '*lvp_icd*' "$tmp/a"
icd "$tmp/b" lvp_icd.json libvulkan_lvp.so
run "lvp only" "" "$tmp/b"
icd "$tmp/c" radeon_icd.json libvulkan_radeon.so
run "radeon only" "" "$tmp/c"
run "user set VK_DRIVER_FILES" "" "$tmp/a" VK_DRIVER_FILES=/x.json
run "user set VK_LOADER_DRIVERS_SELECT" "" "$tmp/a" VK_LOADER_DRIVERS_SELECT='*radeon*'
icd "$tmp/d" mystery.json /usr/lib/libvulkan_llvmpipe.so; icd "$tmp/d" nvidia_icd.json libGLX_nvidia.so.0
run "llvmpipe found by library_path" '*mystery*' "$tmp/d"
icd "$tmp/e" lvp_icd.x86_64.json libvulkan_lvp.so; icd "$tmp/e" lvp_icd.i686.json libvulkan_lvp.so; icd "$tmp/e" intel_icd.x86_64.json libvulkan_intel.so
run "32/64-bit manifests collapse to one glob" '*lvp_icd*' "$tmp/e"
icd "$tmp/f" lvp_icd.json libvulkan_lvp.so; icd "$tmp/g" lvp_icd.json libvulkan_lvp.so; icd "$tmp/g" radeon_icd.json libvulkan_radeon.so
run "same name in two folders counts once" '*lvp_icd*' "$tmp/f $tmp/g"
exit $fails
