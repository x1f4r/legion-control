#!/usr/bin/env bash
#
# End-to-end Android emulator test for Legion over real SSH + legion-link.
#
#   ./apps/legion-app/e2e-android.sh
#   ./apps/legion-app/e2e-android.sh --node-ref origin/link/pairing
#   ./apps/legion-app/e2e-android.sh --shots-dir ./shots
#   ./apps/legion-app/e2e-android.sh --skip-tablet
#   ./apps/legion-app/e2e-android.sh --keep-running
#
# Builds a release `legion` binary from `--node-ref` (default `origin/link/pairing`) in a cache
# checkout under `${XDG_CACHE_HOME:-$HOME/.cache}/legion-app/`, brings up a throwaway `LEGION_HOME`
# node named `tower` and a loopback-only `sshd` on 127.0.0.1, installs a WebView-debuggable x86_64
# build on `legion-test`, and drives the real UI over `adb forward` + Chrome DevTools Protocol.
# Never touches ~/.ssh, the system sshd, ~/.config/legion, or /tmp.
#
set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "$HERE/../.." && pwd -P)"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/legion-app"

NODE_REF="${LEGION_E2E_NODE_REF:-origin/link/pairing}"
SHOTS_DIR="${LEGION_E2E_SHOTS_DIR:-$CACHE_DIR/e2e-shots}"
PHONE_AVD="${LEGION_E2E_PHONE_AVD:-legion-test}"
TABLET_AVD="${LEGION_E2E_TABLET_AVD:-legion-tablet}"
SKIP_TABLET=0
KEEP_RUNNING=0

APP_ID="com.x1f4r.legion.debug"
export LEGION_APP_ID="$APP_ID"

die() { printf '\nERROR: %s\n' "$1" >&2; exit 1; }
say() { printf '%s\n' "$1"; }

while [[ $# -gt 0 ]]; do
	case "$1" in
		--node-ref)
			[ $# -ge 2 ] || die "--node-ref requires a value."
			NODE_REF="$2"
			shift 2
			;;
		--node-ref=*)
			NODE_REF="${1#--node-ref=}"
			shift
			;;
		--shots-dir)
			[ $# -ge 2 ] || die "--shots-dir requires a value."
			SHOTS_DIR="$2"
			shift 2
			;;
		--shots-dir=*)
			SHOTS_DIR="${1#--shots-dir=}"
			shift
			;;
		--skip-tablet)
			SKIP_TABLET=1
			shift
			;;
		--keep-running)
			KEEP_RUNNING=1
			shift
			;;
		-h|--help)
			awk 'NR>1 && /^#/ { sub(/^# ?/, ""); print; next } NR>1 { exit }' "${BASH_SOURCE[0]}"
			exit 0
			;;
		*)
			die "unknown option: $1"
			;;
	esac
done

# ---------------------------------------------------------------------------
# Toolchain
# ---------------------------------------------------------------------------

command -v cargo >/dev/null 2>&1 || die "cargo is not on PATH."
command -v git >/dev/null 2>&1 || die "git is not on PATH."
command -v node >/dev/null 2>&1 || die "node is not on PATH."
command -v npm >/dev/null 2>&1 || die "npm is not on PATH."
command -v ssh-keygen >/dev/null 2>&1 || die "ssh-keygen is not on PATH."
cargo tauri --version >/dev/null 2>&1 || die "cargo-tauri is not installed or not on PATH."

SSHD_BIN=""
for _cand in /usr/sbin/sshd /usr/bin/sshd /sbin/sshd; do
	if [ -x "$_cand" ]; then
		SSHD_BIN="$_cand"
		break
	fi
done
if [ -z "$SSHD_BIN" ]; then
	SSHD_BIN="$(command -v sshd || true)"
fi
[ -n "$SSHD_BIN" ] || die "sshd not found. Install OpenSSH server to run the loopback SSH test."

if [ -z "${JAVA_HOME:-}" ]; then
	if [ -d "/usr/lib/jvm/java-21-openjdk" ]; then
		export JAVA_HOME="/usr/lib/jvm/java-21-openjdk"
	elif [ -d "/Library/Java/JavaVirtualMachines/temurin-21.jdk/Contents/Home" ]; then
		export JAVA_HOME="/Library/Java/JavaVirtualMachines/temurin-21.jdk/Contents/Home"
	fi
fi
[ -n "${JAVA_HOME:-}" ] && [ -x "$JAVA_HOME/bin/java" ] || die "JAVA_HOME must point to JDK 21."

SDK_DIR="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-}}"
if [ -z "$SDK_DIR" ] && [ -d "$HOME/Android/Sdk" ]; then
	SDK_DIR="$HOME/Android/Sdk"
fi
[ -n "$SDK_DIR" ] && [ -d "$SDK_DIR" ] || die "Android SDK not found. Set ANDROID_HOME or ANDROID_SDK_ROOT."
export ANDROID_HOME="$SDK_DIR"
export ANDROID_SDK_ROOT="$SDK_DIR"

if [ -z "${NDK_HOME:-}" ] && [ -z "${ANDROID_NDK_HOME:-}" ]; then
	NDK_LATEST="$(find "$SDK_DIR/ndk" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort -V | tail -n 1 || true)"
	[ -n "$NDK_LATEST" ] || die "Android NDK not found under $SDK_DIR/ndk."
	export NDK_HOME="$NDK_LATEST"
	export ANDROID_NDK_HOME="$NDK_LATEST"
else
	export NDK_HOME="${NDK_HOME:-$ANDROID_NDK_HOME}"
	export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$NDK_HOME}"
fi

for _dir in "$JAVA_HOME/bin" "$SDK_DIR/platform-tools" "$SDK_DIR/emulator"; do
	case ":$PATH:" in
		*":$_dir:"*) ;;
		*) [ -d "$_dir" ] && export PATH="$_dir:$PATH" ;;
	esac
done
unset _dir _cand

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------

RUN_DIR=""
SSHD_PID=""
STARTED_PHONE=0
STARTED_TABLET=0

cleanup() {
	local rc=$?
	if [ -n "$SSHD_PID" ]; then
		pkill -KILL -P "$SSHD_PID" 2>/dev/null || true
		kill -TERM "$SSHD_PID" 2>/dev/null || true
		wait "$SSHD_PID" 2>/dev/null || true
	fi
	if [ "$KEEP_RUNNING" -eq 0 ]; then
		if [ "$STARTED_TABLET" -eq 1 ]; then
			"$HERE/emulator.sh" --avd "$TABLET_AVD" stop >/dev/null 2>&1 || true
		fi
		if [ "$STARTED_PHONE" -eq 1 ]; then
			"$HERE/emulator.sh" --avd "$PHONE_AVD" stop >/dev/null 2>&1 || true
		fi
	fi
	if [ -n "$RUN_DIR" ] && [ -d "$RUN_DIR" ]; then
		rm -rf "$RUN_DIR"
	fi
	exit "$rc"
}
trap cleanup EXIT INT TERM

# ---------------------------------------------------------------------------
# Build release legion binary from NODE_REF inside ~/.cache
# ---------------------------------------------------------------------------

mkdir -p "$CACHE_DIR" "$SHOTS_DIR"
NODE_SRC="$CACHE_DIR/e2e-node-src"
NODE_TARGET="$CACHE_DIR/e2e-node-target"

say "Preparing legion node binary from $NODE_REF"
if [ ! -d "$NODE_SRC/.git" ]; then
	git clone --no-checkout "$REPO_ROOT" "$NODE_SRC" >/dev/null
fi
git -C "$NODE_SRC" fetch --force "$REPO_ROOT" "$NODE_REF" >/dev/null
git -C "$NODE_SRC" checkout --detach --force FETCH_HEAD >/dev/null

CARGO_TARGET_DIR="$NODE_TARGET" cargo build \
	--manifest-path "$NODE_SRC/Cargo.toml" \
	--release \
	-p legion-cli \
	--locked
LEGION_BIN="$NODE_TARGET/release/legion"
[ -x "$LEGION_BIN" ] || die "expected built legion binary at $LEGION_BIN"

# ---------------------------------------------------------------------------
# Ensure playwright-core is installed in apps/ui/test
# ---------------------------------------------------------------------------

if [ ! -d "$REPO_ROOT/apps/ui/test/node_modules/playwright-core" ]; then
	say "Installing playwright-core in apps/ui/test"
	( cd "$REPO_ROOT/apps/ui/test" && npm ci --no-audit --no-fund )
fi

# ---------------------------------------------------------------------------
# Build debug x86_64 APK (WebView-debuggable for CDP; never shipped)
# ---------------------------------------------------------------------------

say "Building WebView-debuggable x86_64 APK for emulator e2e"
mkdir -p "$HERE/dist"
( cd "$HERE" && cargo tauri android build --debug --apk --target x86_64 )

DEBUG_APK="$HERE/src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk"
if [ ! -f "$DEBUG_APK" ]; then
	DEBUG_APK="$(find "$HERE/src-tauri/gen/android/app/build/outputs/apk" -path '*/debug/*.apk' -type f | sort | head -n 1 || true)"
fi
[ -n "$DEBUG_APK" ] && [ -f "$DEBUG_APK" ] || die "no debug APK found after cargo tauri android build --debug"

# ---------------------------------------------------------------------------
# Throwaway LEGION_HOME + loopback sshd
# ---------------------------------------------------------------------------

RUN_DIR="$(mktemp -d "$CACHE_DIR/e2e-run-XXXXXX")"
chmod 700 "$RUN_DIR"

NODE_HOME="$RUN_DIR/legion-home"
FILES_DIR="$RUN_DIR/files"
AUTH_KEYS="$RUN_DIR/authorized_keys"
HOST_KEY="$RUN_DIR/host_ed25519"
SSHD_CFG="$RUN_DIR/sshd_config"
SSHD_LOG="$RUN_DIR/sshd.log"

mkdir -p "$NODE_HOME" "$FILES_DIR/subdir"
printf 'hello from tower\n' > "$FILES_DIR/hello.txt"
printf 'sample note\n' > "$FILES_DIR/notes.md"
printf 'nested file\n' > "$FILES_DIR/subdir/item.txt"

ssh-keygen -q -t ed25519 -N '' -C 'legion-e2e-host' -f "$HOST_KEY"
: > "$AUTH_KEYS"
chmod 600 "$AUTH_KEYS"

LEGION_HOME="$NODE_HOME" "$LEGION_BIN" init --name tower >/dev/null

cat > "$NODE_HOME/config/policy.toml" <<EOF
[principals.phone]
role = "operator"

[roles.operator]
allow = ["effect:read", "effect:write", "effect:exec", "effect:power"]
paths = ["$FILES_DIR/**"]
EOF

cat >> "$NODE_HOME/config/node.toml" <<EOF

[devices]
authorized_keys = "$AUTH_KEYS"
host_keys = ["$HOST_KEY.pub"]
EOF

SSHD_PORT="$(node -e '
const net = require("node:net");
const s = net.createServer();
s.listen(0, "127.0.0.1", () => {
  console.log(s.address().port);
  s.close();
});
')"

PENALTIES_LINE=""
if "$SSHD_BIN" -T -o PerSourcePenalties=no -h "$HOST_KEY" >/dev/null 2>&1; then
	PENALTIES_LINE="PerSourcePenalties no"
fi

CURRENT_USER="$(id -un)"
cat > "$SSHD_CFG" <<EOF
ListenAddress 127.0.0.1
Port $SSHD_PORT
HostKey "$HOST_KEY"
AuthorizedKeysFile "$AUTH_KEYS"
PidFile none
UsePAM no
StrictModes no
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
$PENALTIES_LINE
AllowUsers "$CURRENT_USER"
SetEnv "LEGION_HOME=$NODE_HOME"
Subsystem sftp internal-sftp
LogLevel VERBOSE
EOF

say "Starting throwaway sshd on 127.0.0.1:$SSHD_PORT"
"$SSHD_BIN" -D -e -f "$SSHD_CFG" -E "$SSHD_LOG" </dev/null >/dev/null 2>&1 &
SSHD_PID=$!

for _i in $(seq 1 50); do
	kill -0 "$SSHD_PID" 2>/dev/null || { cat "$SSHD_LOG" >&2 || true; die "throwaway sshd exited early"; }
	if node -e '
const net = require("node:net");
const s = net.connect(Number(process.argv[1]), "127.0.0.1");
s.on("connect", () => { s.destroy(); process.exit(0); });
s.on("error", () => process.exit(1));
' "$SSHD_PORT" 2>/dev/null; then
		break
	fi
	sleep 0.1
done
unset _i

# ---------------------------------------------------------------------------
# Helper to find emulator serial for an AVD
# ---------------------------------------------------------------------------

serial_for_avd() {
	local avd="$1"
	local serial name
	while IFS= read -r serial; do
		[ -n "$serial" ] || continue
		name="$(adb -s "$serial" emu avd name 2>/dev/null | tr -d '\r' | head -n 1 || true)"
		if [ "$name" = "$avd" ]; then
			printf '%s\n' "$serial"
			return 0
		fi
	done < <(adb devices | awk 'NR>1 && $1 ~ /^emulator-/ && $2 == "device" { print $1 }')
	return 1
}

run_on_avd() {
	local avd="$1"
	local mode="$2"
	local serial

	say "Booting emulator '$avd' for $mode suite"
	"$HERE/emulator.sh" --avd "$avd" start
	serial="$(serial_for_avd "$avd")"
	[ -n "$serial" ] || die "could not find adb serial for AVD '$avd'"

	adb -s "$serial" uninstall "$APP_ID" >/dev/null 2>&1 || true
	"$HERE/emulator.sh" --avd "$avd" install "$DEBUG_APK"
	adb -s "$serial" shell pm clear "$APP_ID" >/dev/null
	# Android 17 (API 37) Local Network Protection gates 10.0.2.2 behind ACCESS_LOCAL_NETWORK.
	adb -s "$serial" shell pm grant "$APP_ID" android.permission.ACCESS_LOCAL_NETWORK >/dev/null 2>&1 || true
	"$HERE/emulator.sh" --avd "$avd" launch

	E2E_ADB_SERIAL="$serial" \
	E2E_SHOTS_DIR="$SHOTS_DIR" \
	E2E_LEGION_BIN="$LEGION_BIN" \
	E2E_NODE_HOME="$NODE_HOME" \
	E2E_SSHD_PORT="$SSHD_PORT" \
	E2E_SSHD_PID="$SSHD_PID" \
	E2E_SSHD_LOG="$SSHD_LOG" \
	E2E_MODE="$mode" \
	node "$HERE/e2e-android-driver.mjs"
}

# ---------------------------------------------------------------------------
# Run phone suite, then tablet wide paired shot
# ---------------------------------------------------------------------------

STARTED_PHONE=1
run_on_avd "$PHONE_AVD" phone
if [ "$KEEP_RUNNING" -eq 0 ]; then
	"$HERE/emulator.sh" --avd "$PHONE_AVD" stop
	STARTED_PHONE=0
fi

if [ "$SKIP_TABLET" -eq 0 ]; then
	STARTED_TABLET=1
	run_on_avd "$TABLET_AVD" tablet
	if [ "$KEEP_RUNNING" -eq 0 ]; then
		"$HERE/emulator.sh" --avd "$TABLET_AVD" stop
		STARTED_TABLET=0
	fi
fi

say ""
say "End-to-end Android suite passed. Screenshots saved in: $SHOTS_DIR"
