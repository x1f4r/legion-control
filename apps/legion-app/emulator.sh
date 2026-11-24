#!/usr/bin/env bash
#
# Headless Android emulator harness for Legion.
#
#   ./apps/legion-app/emulator.sh [--avd <name>] start
#   ./apps/legion-app/emulator.sh [--avd <name>] install <apk>
#   ./apps/legion-app/emulator.sh [--avd <name>] launch
#   ./apps/legion-app/emulator.sh [--avd <name>] shot <file.png>
#   ./apps/legion-app/emulator.sh [--avd <name>] back
#   ./apps/legion-app/emulator.sh [--avd <name>] tap <x> <y>
#   ./apps/legion-app/emulator.sh [--avd <name>] text '<string>'
#   ./apps/legion-app/emulator.sh [--avd <name>] theme <dark|light>
#   ./apps/legion-app/emulator.sh [--avd <name>] logcat [adb-logcat-args...]
#   ./apps/legion-app/emulator.sh [--avd <name>] stop
#
# Default AVD is `legion-test`. Pass `--avd legion-tablet` for the tablet AVD.
# The emulator always runs headless and loopback-only.
#
set -euo pipefail

AVD="${LEGION_AVD:-legion-test}"
APP_ID="com.x1f4r.legion"
ACTIVITY="$APP_ID/.MainActivity"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/legion-app"

die() { printf 'ERROR: %s\n' "$1" >&2; exit 1; }
say() { printf '%s\n' "$1"; }

usage() {
	awk 'NR>1 && /^#/ { sub(/^# ?/, ""); print; next } NR>1 { exit }' "${BASH_SOURCE[0]}"
}

# Parse --avd before or after the subcommand.
ARGS=()
while [[ $# -gt 0 ]]; do
	case "$1" in
		--avd)
			[ $# -ge 2 ] || die "--avd requires a value."
			AVD="$2"
			shift 2
			;;
		--avd=*)
			AVD="${1#--avd=}"
			shift
			;;
		-h|--help)
			usage
			exit 0
			;;
		*)
			ARGS+=("$1")
			shift
			;;
	esac
done

[ "${#ARGS[@]}" -ge 1 ] || { usage >&2; exit 2; }
CMD="${ARGS[0]}"
REST=("${ARGS[@]:1}")

# Resolve SDK tools if not already on PATH.
SDK_DIR="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-}}"
if [ -z "$SDK_DIR" ] && [ -d "$HOME/Android/Sdk" ]; then
	SDK_DIR="$HOME/Android/Sdk"
fi
if [ -n "$SDK_DIR" ]; then
	export ANDROID_HOME="$SDK_DIR"
	export ANDROID_SDK_ROOT="$SDK_DIR"
	for _dir in "$SDK_DIR/platform-tools" "$SDK_DIR/emulator"; do
		case ":$PATH:" in
			*":$_dir:"*) ;;
			*) [ -d "$_dir" ] && export PATH="$_dir:$PATH" ;;
		esac
	done
fi

command -v adb >/dev/null 2>&1 || die "adb is not on PATH. Set ANDROID_HOME or source your Android environment."

find_serial_for_avd() {
	local target="$1"
	local serial name
	while IFS= read -r serial; do
		[ -n "$serial" ] || continue
		name="$(adb -s "$serial" emu avd name 2>/dev/null | tr -d '\r' | head -n 1 || true)"
		if [ "$name" = "$target" ]; then
			printf '%s\n' "$serial"
			return 0
		fi
	done < <(adb devices | awk 'NR>1 && $1 ~ /^emulator-/ && $2 == "device" { print $1 }')
	return 1
}

require_serial() {
	local serial
	serial="$(find_serial_for_avd "$AVD" || true)"
	[ -n "$serial" ] || die "no running emulator for AVD '$AVD'. Run '$0 --avd $AVD start' first."
	printf '%s\n' "$serial"
}

wait_for_ready() {
	local serial="$1"
	local deadline=$(( $(date +%s) + 180 ))

	adb -s "$serial" wait-for-device >/dev/null
	while true; do
		local boot
		boot="$(adb -s "$serial" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r' || true)"
		if [ "$boot" = "1" ]; then
			break
		fi
		[ "$(date +%s)" -lt "$deadline" ] || die "timed out waiting for $AVD ($serial) to finish booting."
		sleep 1
	done

	while true; do
		if adb -s "$serial" shell pm path android >/dev/null 2>&1; then
			break
		fi
		[ "$(date +%s)" -lt "$deadline" ] || die "timed out waiting for package manager on $AVD ($serial)."
		sleep 1
	done

	while true; do
		local addrs
		addrs="$(adb -s "$serial" shell ip -4 addr show 2>/dev/null || true)"
		if printf '%s\n' "$addrs" | grep -q '10\.0\.2\.'; then
			break
		fi
		[ "$(date +%s)" -lt "$deadline" ] || die "timed out waiting for network interface (10.0.2.x) on $AVD ($serial)."
		sleep 1
	done
}

cmd_start() {
	command -v emulator >/dev/null 2>&1 || die "emulator binary is not on PATH."

	local existing
	existing="$(find_serial_for_avd "$AVD" || true)"
	if [ -n "$existing" ]; then
		wait_for_ready "$existing"
		say "Emulator '$AVD' is already running ($existing)."
		return 0
	fi

	mkdir -p "$CACHE_DIR"
	local log_file="$CACHE_DIR/emulator-${AVD}.log"

	say "Starting headless emulator '$AVD' (log: $log_file)"
	if command -v setsid >/dev/null 2>&1; then
		setsid emulator \
			-avd "$AVD" \
			-no-window \
			-no-audio \
			-gpu swiftshader_indirect \
			-no-boot-anim \
			-no-snapshot-save \
			>"$log_file" 2>&1 </dev/null &
	else
		nohup emulator \
			-avd "$AVD" \
			-no-window \
			-no-audio \
			-gpu swiftshader_indirect \
			-no-boot-anim \
			-no-snapshot-save \
			>"$log_file" 2>&1 </dev/null &
	fi
	local emu_pid=$!

	local deadline=$(( $(date +%s) + 180 ))
	local serial=""
	while true; do
		if ! kill -0 "$emu_pid" 2>/dev/null; then
			tail -n 30 "$log_file" >&2 || true
			die "emulator process for '$AVD' exited before coming online."
		fi
		serial="$(find_serial_for_avd "$AVD" || true)"
		if [ -n "$serial" ]; then
			break
		fi
		[ "$(date +%s)" -lt "$deadline" ] || die "timed out waiting for adb to see '$AVD'."
		sleep 1
	done

	wait_for_ready "$serial"
	say "Emulator '$AVD' ready ($serial)."
}

cmd_install() {
	[ "${#REST[@]}" -eq 1 ] || die "usage: $0 [--avd <name>] install <apk>"
	local apk="${REST[0]}"
	[ -f "$apk" ] || die "APK not found: $apk"
	local serial
	serial="$(require_serial)"
	adb -s "$serial" install -r "$apk"
}

cmd_launch() {
	local serial
	serial="$(require_serial)"
	adb -s "$serial" shell am start -W -n "$ACTIVITY"
}

cmd_shot() {
	[ "${#REST[@]}" -eq 1 ] || die "usage: $0 [--avd <name>] shot <file.png>"
	local out="${REST[0]}"
	mkdir -p "$(dirname -- "$out")"
	local serial
	serial="$(require_serial)"
	adb -s "$serial" exec-out screencap -p > "$out"
	[ -s "$out" ] || die "screenshot file is empty: $out"
	say "Saved screenshot: $out"
}

cmd_back() {
	local serial
	serial="$(require_serial)"
	adb -s "$serial" shell input keyevent KEYCODE_BACK
}

cmd_tap() {
	[ "${#REST[@]}" -eq 2 ] || die "usage: $0 [--avd <name>] tap <x> <y>"
	local serial
	serial="$(require_serial)"
	adb -s "$serial" shell input tap "${REST[0]}" "${REST[1]}"
}

cmd_text() {
	[ "${#REST[@]}" -eq 1 ] || die "usage: $0 [--avd <name>] text '<string>'"
	local serial raw escaped
	serial="$(require_serial)"
	raw="${REST[0]}"
	# Android's `input text` interprets `%s` as space and passes the string through /system/bin/sh.
	escaped="${raw// /%s}"
	escaped="$(printf '%q' "$escaped")"
	adb -s "$serial" shell "input text $escaped"
}

cmd_theme() {
	[ "${#REST[@]}" -eq 1 ] || die "usage: $0 [--avd <name>] theme <dark|light>"
	local serial mode
	serial="$(require_serial)"
	case "${REST[0]}" in
		dark) mode="yes" ;;
		light) mode="no" ;;
		*) die "theme must be 'dark' or 'light'." ;;
	esac
	adb -s "$serial" shell cmd uimode night "$mode"
}

cmd_logcat() {
	local serial
	serial="$(require_serial)"
	if [ "${#REST[@]}" -eq 0 ]; then
		adb -s "$serial" logcat -d -v time -t 200
	else
		adb -s "$serial" logcat "${REST[@]}"
	fi
}

cmd_stop() {
	local serial
	serial="$(find_serial_for_avd "$AVD" || true)"
	if [ -z "$serial" ]; then
		say "Emulator '$AVD' is not running."
		return 0
	fi
	adb -s "$serial" emu kill >/dev/null 2>&1 || true
	local deadline=$(( $(date +%s) + 30 ))
	while [ -n "$(find_serial_for_avd "$AVD" || true)" ]; do
		[ "$(date +%s)" -lt "$deadline" ] && sleep 1 || break
	done
	say "Stopped emulator '$AVD' ($serial)."
}

case "$CMD" in
	start) cmd_start ;;
	install) cmd_install ;;
	launch) cmd_launch ;;
	shot) cmd_shot ;;
	back) cmd_back ;;
	tap) cmd_tap ;;
	text) cmd_text ;;
	theme) cmd_theme ;;
	logcat) cmd_logcat ;;
	stop) cmd_stop ;;
	*) die "unknown subcommand: $CMD" ;;
esac
