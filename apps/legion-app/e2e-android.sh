#!/usr/bin/env bash
#
# End-to-end two-node Android emulator test for Legion over real SSH + legion-link.
#
#   ./apps/legion-app/e2e-android.sh
#   ./apps/legion-app/e2e-android.sh --node-ref HEAD
#   ./apps/legion-app/e2e-android.sh --shots-dir ./shots
#   ./apps/legion-app/e2e-android.sh --skip-tablet
#   ./apps/legion-app/e2e-android.sh --keep-running
#
# Builds a release `legion` binary and `legion-link` example CLI from `--node-ref` (default
# `HEAD`) in a cache checkout under `${XDG_CACHE_HOME:-$HOME/.cache}/legion-app/`, brings up two
# throwaway loopback nodes (`tower` and `pi`) connected in a mutual SSH fleet, installs a
# WebView-debuggable x86_64 build on `legion-test`, and drives the real UI over `adb forward` +
# Chrome DevTools Protocol. Never touches ~/.ssh, the system sshd, ~/.config/legion, or /tmp.
#
set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "$HERE/../.." && pwd -P)"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/legion-app"

NODE_REF="${LEGION_E2E_NODE_REF:-HEAD}"
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
command -v ssh >/dev/null 2>&1 || die "ssh is not on PATH."
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
SSHD_A_PIDFILE=""
SSHD_B_PIDFILE=""
STARTED_PHONE=0
STARTED_TABLET=0

stop_pidfile() {
	local pidfile="$1"
	if [ -n "$pidfile" ] && [ -f "$pidfile" ]; then
		local pid
		pid="$(tr -d '[:space:]' < "$pidfile" || true)"
		if [ -n "$pid" ]; then
			pkill -KILL -P "$pid" 2>/dev/null || true
			kill -TERM "$pid" 2>/dev/null || true
			wait "$pid" 2>/dev/null || true
		fi
		rm -f "$pidfile"
	fi
}

cleanup() {
	local rc=$?
	stop_pidfile "$SSHD_A_PIDFILE"
	stop_pidfile "$SSHD_B_PIDFILE"
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
# Build release legion binary and legion-link example from NODE_REF in ~/.cache
# ---------------------------------------------------------------------------

mkdir -p "$CACHE_DIR" "$SHOTS_DIR"
NODE_SRC="$CACHE_DIR/e2e-node-src"
NODE_TARGET="$CACHE_DIR/e2e-node-target"

say "Preparing legion node binary and link helper from $NODE_REF"
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
CARGO_TARGET_DIR="$NODE_TARGET" cargo build \
	--manifest-path "$NODE_SRC/Cargo.toml" \
	--release \
	-p legion-link \
	--all-features \
	--example link \
	--locked

LEGION_BIN="$NODE_TARGET/release/legion"
LINK_BIN="$NODE_TARGET/release/examples/link"
[ -x "$LEGION_BIN" ] || die "expected built legion binary at $LEGION_BIN"
[ -x "$LINK_BIN" ] || die "expected built link helper at $LINK_BIN"

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
# Two throwaway nodes (tower + pi) with mutual SSH fleet on 127.0.0.1
# ---------------------------------------------------------------------------

RUN_DIR="$(mktemp -d "$CACHE_DIR/e2e-run-XXXXXX")"
chmod 700 "$RUN_DIR"

NODE_A_HOME="$RUN_DIR/node-a"
NODE_B_HOME="$RUN_DIR/node-b"
FILES_A_DIR="$RUN_DIR/files-a"
FILES_B_DIR="$RUN_DIR/files-b"
AUTH_KEYS_A="$RUN_DIR/authorized_keys_a"
AUTH_KEYS_B="$RUN_DIR/authorized_keys_b"
FLEET_AUTH_KEYS_A="$RUN_DIR/fleet_authorized_keys_a"
FLEET_AUTH_KEYS_B="$RUN_DIR/fleet_authorized_keys_b"
HOST_KEY_A="$RUN_DIR/host_a_ed25519"
HOST_KEY_B="$RUN_DIR/host_b_ed25519"
FLEET_KEY="$RUN_DIR/fleet_ed25519"
FLEET_KNOWN_HOSTS="$RUN_DIR/fleet_known_hosts"
FLEET_SSH_CFG="$RUN_DIR/fleet_ssh_config"
SSHD_A_CFG="$RUN_DIR/sshd_a_config"
SSHD_B_CFG="$RUN_DIR/sshd_b_config"
SSHD_A_LOG="$RUN_DIR/sshd_a.log"
SSHD_B_LOG="$RUN_DIR/sshd_b.log"
SSHD_A_PIDFILE="$RUN_DIR/sshd_a.pid"
SSHD_B_PIDFILE="$RUN_DIR/sshd_b.pid"

mkdir -p "$NODE_A_HOME" "$NODE_B_HOME" "$FILES_A_DIR/subdir" "$FILES_B_DIR/subdir"
printf 'hello from tower\n' > "$FILES_A_DIR/hello.txt"
printf 'sample note\n' > "$FILES_A_DIR/notes.md"
printf 'nested file\n' > "$FILES_A_DIR/subdir/item.txt"
printf 'hello from pi\n' > "$FILES_B_DIR/pi.txt"
printf 'nested on pi\n' > "$FILES_B_DIR/subdir/item.txt"

ssh-keygen -q -t ed25519 -N '' -C 'legion-e2e-host-a' -f "$HOST_KEY_A"
ssh-keygen -q -t ed25519 -N '' -C 'legion-e2e-host-b' -f "$HOST_KEY_B"
ssh-keygen -q -t ed25519 -N '' -C 'legion-e2e-fleet' -f "$FLEET_KEY"

: > "$AUTH_KEYS_A"
: > "$AUTH_KEYS_B"
chmod 600 "$AUTH_KEYS_A" "$AUTH_KEYS_B"

FLEET_PUB="$(cat "$FLEET_KEY.pub")"
printf 'command="env LEGION_HOME=%s %s rpc",restrict %s\n' "$NODE_A_HOME" "$LEGION_BIN" "$FLEET_PUB" > "$FLEET_AUTH_KEYS_A"
printf 'command="env LEGION_HOME=%s %s rpc",restrict %s\n' "$NODE_B_HOME" "$LEGION_BIN" "$FLEET_PUB" > "$FLEET_AUTH_KEYS_B"
chmod 600 "$FLEET_AUTH_KEYS_A" "$FLEET_AUTH_KEYS_B"

read -r PORT_A PORT_B <<< "$(node -e '
const net = require("node:net");
const s1 = net.createServer();
const s2 = net.createServer();
s1.listen(0, "127.0.0.1", () => {
  s2.listen(0, "127.0.0.1", () => {
    console.log(s1.address().port, s2.address().port);
    s2.close(() => s1.close());
  });
});
')"

CURRENT_USER="$(id -un)"

{
	printf '[127.0.0.1]:%s %s\n' "$PORT_A" "$(cat "$HOST_KEY_A.pub")"
	printf '[127.0.0.1]:%s %s\n' "$PORT_B" "$(cat "$HOST_KEY_B.pub")"
} > "$FLEET_KNOWN_HOSTS"
chmod 600 "$FLEET_KNOWN_HOSTS"

cat > "$FLEET_SSH_CFG" <<EOF
Host tower
  HostName 127.0.0.1
  Port $PORT_A
  User $CURRENT_USER
  IdentityFile $FLEET_KEY
  IdentitiesOnly yes
  UserKnownHostsFile $FLEET_KNOWN_HOSTS
  StrictHostKeyChecking yes
  BatchMode yes
  ConnectTimeout 3

Host pi
  HostName 127.0.0.1
  Port $PORT_B
  User $CURRENT_USER
  IdentityFile $FLEET_KEY
  IdentitiesOnly yes
  UserKnownHostsFile $FLEET_KNOWN_HOSTS
  StrictHostKeyChecking yes
  BatchMode yes
  ConnectTimeout 3
EOF
chmod 600 "$FLEET_SSH_CFG"

LEGION_HOME="$NODE_A_HOME" "$LEGION_BIN" init --name tower >/dev/null
LEGION_HOME="$NODE_B_HOME" "$LEGION_BIN" init --name pi >/dev/null

write_policy() {
	local home="$1"
	local files_dir="$2"
	cat > "$home/config/policy.toml" <<EOF
[roles.observer]
allow = ["effect:read"]

[roles.editor]
allow = ["effect:read", "effect:write"]
paths = ["$files_dir/**"]

[roles.operator]
allow = ["effect:read", "effect:write", "effect:exec", "effect:power"]
paths = ["$files_dir/**"]
EOF
}

write_policy "$NODE_A_HOME" "$FILES_A_DIR"
write_policy "$NODE_B_HOME" "$FILES_B_DIR"

cat >> "$NODE_A_HOME/config/node.toml" <<EOF

[devices]
hosts = ["127.0.0.1", "10.0.2.2"]
port = $PORT_A
user = "$CURRENT_USER"
authorized_keys = "$AUTH_KEYS_A"
host_keys = ["$HOST_KEY_A.pub"]
EOF

cat >> "$NODE_B_HOME/config/node.toml" <<EOF

[devices]
hosts = ["127.0.0.1", "10.0.2.2"]
port = $PORT_B
user = "$CURRENT_USER"
authorized_keys = "$AUTH_KEYS_B"
host_keys = ["$HOST_KEY_B.pub"]
EOF

cat > "$NODE_A_HOME/config/fleet.toml" <<EOF
[machines.pi]
[[machines.pi.systems]]
name = "linux"
command = ["ssh", "-F", "$FLEET_SSH_CFG", "pi", "legion", "rpc"]
EOF

cat > "$NODE_B_HOME/config/fleet.toml" <<EOF
[machines.tower]
[[machines.tower.systems]]
name = "linux"
command = ["ssh", "-F", "$FLEET_SSH_CFG", "tower", "legion", "rpc"]
EOF

PENALTIES_LINE=""
if "$SSHD_BIN" -T -o PerSourcePenalties=no -h "$HOST_KEY_A" >/dev/null 2>&1; then
	PENALTIES_LINE="PerSourcePenalties no"
fi

write_sshd_cfg() {
	local cfg="$1"
	local port="$2"
	local host_key="$3"
	local auth_keys="$4"
	local fleet_auth_keys="$5"
	local node_home="$6"
	cat > "$cfg" <<EOF
ListenAddress 127.0.0.1
Port $port
HostKey "$host_key"
AuthorizedKeysFile $auth_keys $fleet_auth_keys
PidFile none
UsePAM no
StrictModes no
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
$PENALTIES_LINE
AllowUsers "$CURRENT_USER"
SetEnv "LEGION_HOME=$node_home"
Subsystem sftp internal-sftp
LogLevel VERBOSE
EOF
}

write_sshd_cfg "$SSHD_A_CFG" "$PORT_A" "$HOST_KEY_A" "$AUTH_KEYS_A" "$FLEET_AUTH_KEYS_A" "$NODE_A_HOME"
write_sshd_cfg "$SSHD_B_CFG" "$PORT_B" "$HOST_KEY_B" "$AUTH_KEYS_B" "$FLEET_AUTH_KEYS_B" "$NODE_B_HOME"

start_sshd() {
	local name="$1"
	local cfg="$2"
	local log="$3"
	local port="$4"
	local pidfile="$5"
	say "Starting throwaway sshd for $name on 127.0.0.1:$port"
	"$SSHD_BIN" -D -e -f "$cfg" -E "$log" </dev/null >/dev/null 2>&1 &
	local pid=$!
	printf '%s\n' "$pid" > "$pidfile"
	for _i in $(seq 1 50); do
		kill -0 "$pid" 2>/dev/null || { cat "$log" >&2 || true; die "throwaway sshd for $name exited early"; }
		if node -e '
const net = require("node:net");
const s = net.connect(Number(process.argv[1]), "127.0.0.1");
s.on("connect", () => { s.destroy(); process.exit(0); });
s.on("error", () => process.exit(1));
' "$port" 2>/dev/null; then
			return 0
		fi
		sleep 0.1
	done
	die "timed out waiting for throwaway sshd ($name) on 127.0.0.1:$port"
}

start_sshd tower "$SSHD_A_CFG" "$SSHD_A_LOG" "$PORT_A" "$SSHD_A_PIDFILE"
start_sshd pi "$SSHD_B_CFG" "$SSHD_B_LOG" "$PORT_B" "$SSHD_B_PIDFILE"

# Confirm mutual fleet connectivity before booting any emulator.
verify_fleet_ping() {
	local from_name="$1"
	local from_home="$2"
	local peer_name="$3"
	local peer_log="$4"
	local out
	out="$(LEGION_HOME="$from_home" "$LEGION_BIN" fleet.ping --json 2>&1 || true)"
	if ! printf '%s' "$out" | node -e '
const fs = require("node:fs");
const data = JSON.parse(fs.readFileSync(0, "utf8"));
const ok = Array.isArray(data.results) && data.results.some((r) => r.machine === process.argv[1] && r.reachable === true);
process.exit(ok ? 0 : 1);
' "$peer_name" 2>/dev/null; then
		printf 'fleet.ping from %s output:\n%s\n' "$from_name" "$out" >&2
		printf '%s sshd.log:\n' "$peer_name" >&2
		cat "$peer_log" >&2 || true
		die "$from_name failed to ping $peer_name over mutual loopback SSH fleet"
	fi
}

verify_fleet_ping tower "$NODE_A_HOME" pi "$SSHD_B_LOG"
verify_fleet_ping pi "$NODE_B_HOME" tower "$SSHD_A_LOG"
say "Verified mutual loopback SSH fleet between tower ($PORT_A) and pi ($PORT_B)"

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
	# Enable the debug-only loopback endpoint switch so the two-node loopback harness
	# (127.0.0.1 + 10.0.2.2) can learn peer endpoints. Release builds ignore this marker.
	adb -s "$serial" shell "run-as '$APP_ID' touch allow-loopback-endpoints"
	# Android 17 (API 37) Local Network Protection gates 10.0.2.2 behind ACCESS_LOCAL_NETWORK.
	adb -s "$serial" shell pm grant "$APP_ID" android.permission.ACCESS_LOCAL_NETWORK >/dev/null 2>&1 || true
	"$HERE/emulator.sh" --avd "$avd" launch

	E2E_ADB_SERIAL="$serial" \
	E2E_SHOTS_DIR="$SHOTS_DIR" \
	E2E_RUN_DIR="$RUN_DIR" \
	E2E_LEGION_BIN="$LEGION_BIN" \
	E2E_LINK_BIN="$LINK_BIN" \
	E2E_SSHD_BIN="$SSHD_BIN" \
	E2E_NODE_A_HOME="$NODE_A_HOME" \
	E2E_PORT_A="$PORT_A" \
	E2E_SSHD_A_CFG="$SSHD_A_CFG" \
	E2E_SSHD_A_LOG="$SSHD_A_LOG" \
	E2E_SSHD_A_PIDFILE="$SSHD_A_PIDFILE" \
	E2E_NODE_B_HOME="$NODE_B_HOME" \
	E2E_PORT_B="$PORT_B" \
	E2E_SSHD_B_CFG="$SSHD_B_CFG" \
	E2E_SSHD_B_LOG="$SSHD_B_LOG" \
	E2E_SSHD_B_PIDFILE="$SSHD_B_PIDFILE" \
	E2E_MODE="$mode" \
	node "$HERE/e2e-android-driver.mjs"
}

# ---------------------------------------------------------------------------
# Run phone two-node suite, then tablet wide paired shot
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
say "End-to-end two-node Android suite passed. Screenshots saved in: $SHOTS_DIR"
