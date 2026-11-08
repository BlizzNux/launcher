#!/usr/bin/env bash
# Runs the real gpu_env, labs_env and bnet_fps_config from bin/blizznux-run and checks the
# DXVK_CONFIG, PROTON_USE_XALIA and BLIZZNUX_LAB they leave behind.
# shellcheck disable=SC2016
set -euo pipefail
cd "$(dirname "$0")/.."

fails=0
# run NAME EXPECTED_DXVK_CONFIG [VAR=value ...]: Labs "no Xalia" and "15 FPS" both on.
run() {
	local name=$1 want=$2; shift 2
	local got
	got=$(env -u DXVK_CONFIG -u PROTON_USE_XALIA -u HYPRLAND_INSTANCE_SIGNATURE "$@" bash -c '
		set -euo pipefail
		eval "$(sed -n "/^gpu_env()/,/^}/p;/^bnet_fps_config()/,/^}/p;/^labs_env()/,/^}/p" bin/blizznux-run)"
		log() { :; }
		hybrid_nvidia() { return 0; }
		hybrid_display_gpu() { echo AMD; }
		custom_proton() { :; }
		custom_proton_source() { :; }
		CACHE_DIR=$(mktemp -d); OFFLOAD=on
		LAB_DIRECT_PROTON=0; LAB_WOW64=0; LAB_NO_XALIA=${T_XALIA:-1}; LAB_BNET_FPS=1
		gpu_env; labs_env
		printf "%s|%s|%s\n" "${DXVK_CONFIG:-}" "${PROTON_USE_XALIA:-}" "${BLIZZNUX_LAB:-}"')
	if [[ $got == "$want" ]]; then echo "ok   $name"; else echo "FAIL $name"; echo "  want: $want"; echo "  got:  $got"; fails=1; fi
}

# No user config on a machine without the NVIDIA driver (gpu_env sets nothing).
run "no user config" "[Battle.net.exe]; dxgi.maxFrameRate = 15|0|bnetfps,noxalia"
# The user's own config stays, the cap goes last in its own section.
run "user config" "dxgi.syncInterval = 0; [Battle.net.exe]; dxgi.maxFrameRate = 15|0|bnetfps,noxalia" DXVK_CONFIG="dxgi.syncInterval = 0;"
# A user section for another program is not extended: the cap gets its own section.
run "user config ending in another section" "[Game.exe]; dxgi.syncInterval = 0; [Battle.net.exe]; dxgi.maxFrameRate = 15|0|bnetfps,noxalia" DXVK_CONFIG="[Game.exe]; dxgi.syncInterval = 0"
# A user's Battle.net.exe section last: the cap joins it, no second section.
run "user Battle.net.exe section" "[Battle.net.exe]; dxgi.syncInterval = 0; dxgi.maxFrameRate = 15|0|bnetfps,noxalia" DXVK_CONFIG="[Battle.net.exe]; dxgi.syncInterval = 0"
# Hand-set PROTON_USE_XALIA=0 is the lab even with the toggle off.
run "hand-set Xalia off" "[Battle.net.exe]; dxgi.maxFrameRate = 15|0|bnetfps,noxalia" PROTON_USE_XALIA=0 T_XALIA=0
run "Xalia left alone" "[Battle.net.exe]; dxgi.maxFrameRate = 15||bnetfps" T_XALIA=0

# Hybrid NVIDIA under Hyprland, with the NVIDIA driver present: gpu_env's Hyprland section is
# detected first (the cap is added afterwards), and the cap joins that Battle.net.exe section.
if [[ -d /proc/driver/nvidia ]]; then
	run "hybrid Hyprland" "dxvk.trackPipelineLifetime = True; dxvk.deviceFilter = NVIDIA; [Battle.net.exe]; dxvk.deviceFilter = AMD; dxgi.maxFrameRate = 15|0|bnetfps,noxalia" HYPRLAND_INSTANCE_SIGNATURE=x
else
	echo "skip hybrid Hyprland (needs /proc/driver/nvidia); see the stubbed run below"
fi

# Same Hyprland check with the default config that gpu_env sets when the NVIDIA driver is there.
got=$(env -u PROTON_USE_XALIA HYPRLAND_INSTANCE_SIGNATURE=x DXVK_CONFIG="dxvk.trackPipelineLifetime = True" bash -c '
	set -euo pipefail
	eval "$(sed -n "/^gpu_env()/,/^}/p;/^bnet_fps_config()/,/^}/p;/^labs_env()/,/^}/p" bin/blizznux-run)"
	log() { :; }; hybrid_nvidia() { return 0; }; hybrid_display_gpu() { echo AMD; }
	custom_proton() { :; }; custom_proton_source() { :; }
	CACHE_DIR=$(mktemp -d); OFFLOAD=on; LAB_DIRECT_PROTON=0; LAB_WOW64=0; LAB_NO_XALIA=1; LAB_BNET_FPS=1
	gpu_env; labs_env; printf "%s" "$DXVK_CONFIG"')
want="dxvk.trackPipelineLifetime = True; dxvk.deviceFilter = NVIDIA; [Battle.net.exe]; dxvk.deviceFilter = AMD; dxgi.maxFrameRate = 15"
if [[ $got == "$want" ]]; then echo "ok   hybrid Hyprland (launcher default)"; else echo "FAIL hybrid Hyprland"; echo "  want: $want"; echo "  got:  $got"; fails=1; fi

exit $fails
