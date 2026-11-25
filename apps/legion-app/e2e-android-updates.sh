#!/usr/bin/env bash
#
# One-command local Android emulator end-to-end self-update verification for Legion.
#
#   ./apps/legion-app/e2e-android-updates.sh
#   ./apps/legion-app/e2e-android-updates.sh --avd legion-test --shots-dir ./shots
#   ./apps/legion-app/e2e-android-updates.sh --keep-running
#
# Builds a throwaway preview signing key, a throwaway Android release keystore, a preview-keyed
# release `legion` node binary, and three signed release APKs (seq1 < seq2 < seq3) via
# `build-android.sh --emulator`. Packages seq2 and seq3 through `scripts/make-update-set.mjs`
# (exercising APK binary AXML versionCode checks), boots `legion-test`, pairs over loopback SSH
# (`10.0.2.2`), verifies seq1 -> seq2 update with unknown-sources grant and the quiet post-update
# row, then verifies background silent self-update seq2 -> seq3 on app backgrounding without
# bringing Legion back to the foreground.
#
# Never touches ~/.ssh, the system sshd, ~/.config/legion, ~/.legion-control/legion-app-release.jks,
# or /tmp. Cleans up processes, emulator, and scratch directories on exit.
#
set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "$HERE/../.." && pwd -P)"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/legion-app"

AVD="${LEGION_E2E_PHONE_AVD:-legion-test}"
SHOTS_DIR="${LEGION_E2E_SHOTS_DIR:-$CACHE_DIR/e2e-updates-shots}"
BUILD_DIR=""
EMU_PORT=""
KEEP_RUNNING=0
APP_ID="com.x1f4r.legion"

die() { printf '\nERROR: %s\n' "$1" >&2; exit 1; }
say() { printf '%s\n' "$1"; }

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
		--shots-dir)
			[ $# -ge 2 ] || die "--shots-dir requires a value."
			SHOTS_DIR="$2"
			shift 2
			;;
		--shots-dir=*)
			SHOTS_DIR="${1#--shots-dir=}"
			shift
			;;
		--build-dir)
			[ $# -ge 2 ] || die "--build-dir requires a value."
			BUILD_DIR="$2"
			shift 2
			;;
		--build-dir=*)
			BUILD_DIR="${1#--build-dir=}"
			shift
			;;
		--port)
			[ $# -ge 2 ] || die "--port requires a value."
			EMU_PORT="$2"
			shift 2
			;;
		--port=*)
			EMU_PORT="${1#--port=}"
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
command -v node >/dev/null 2>&1 || die "node is not on PATH."
command -v openssl >/dev/null 2>&1 || die "openssl is not on PATH."
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
[ -n "${JAVA_HOME:-}" ] && [ -x "$JAVA_HOME/bin/keytool" ] || die "JAVA_HOME must point to JDK 21."

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
SSHD_PIDFILE=""
EMU_SERIAL=""
EMU_PID=""

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
	stop_pidfile "$SSHD_PIDFILE"
	if [ "$KEEP_RUNNING" -eq 0 ] && [ -n "$EMU_SERIAL" ]; then
		adb -s "$EMU_SERIAL" emu kill >/dev/null 2>&1 || true
		if [ -n "$EMU_PID" ]; then
			for _i in $(seq 1 20); do
				kill -0 "$EMU_PID" 2>/dev/null || break
				sleep 0.5
			done
			kill -TERM "$EMU_PID" 2>/dev/null || true
		fi
	fi
	if [ -n "$RUN_DIR" ] && [ -d "$RUN_DIR" ]; then
		rm -rf "$RUN_DIR"
	fi
	exit "$rc"
}
trap cleanup EXIT INT TERM

mkdir -p "$CACHE_DIR" "$SHOTS_DIR"
RUN_DIR="$(mktemp -d "$CACHE_DIR/e2e-updates-run-XXXXXX")"
chmod 700 "$RUN_DIR"

ARTIFACTS_DIR="$RUN_DIR"
if [ -n "$BUILD_DIR" ]; then
	mkdir -p "$BUILD_DIR"
	chmod 700 "$BUILD_DIR"
	ARTIFACTS_DIR="$BUILD_DIR"
fi

PREVIEW_KEY="$ARTIFACTS_DIR/preview.key"
PREVIEW_PUB="$ARTIFACTS_DIR/preview.pub.pem"
TEST_KEYSTORE="$ARTIFACTS_DIR/test-release.jks"
NODE_TARGET="$CACHE_DIR/e2e-updates-node-target"
META_ENV="$ARTIFACTS_DIR/build-meta.env"
APK1="$ARTIFACTS_DIR/Legion-seq1.apk"
APK2="$ARTIFACTS_DIR/Legion-seq2.apk"
APK3="$ARTIFACTS_DIR/Legion-seq3.apk"
SET2_DIR="$ARTIFACTS_DIR/set-2"
SET3_DIR="$ARTIFACTS_DIR/set-3"
LEGION_BIN="$NODE_TARGET/release/legion"

if [ -f "$META_ENV" ] && [ -f "$PREVIEW_KEY" ] && [ -f "$PREVIEW_PUB" ] && [ -f "$APK1" ] && [ -f "$APK2" ] && [ -f "$APK3" ] && [ -f "$SET2_DIR/update.json" ] && [ -f "$SET3_DIR/update.json" ] && [ -x "$LEGION_BIN" ]; then
	say "Reusing prebuilt APKs, update sets, and legion binary from $ARTIFACTS_DIR"
	# shellcheck source=/dev/null
	source "$META_ENV"
else
	say "Generating throwaway preview signing keypair"
	node "$REPO_ROOT/scripts/make-update-set.mjs" keygen \
		--key "$PREVIEW_KEY" \
		--public-key "$PREVIEW_PUB"

	say "Building preview-keyed release legion node binary"
	LEGION_PREVIEW_UPDATE_KEY="$PREVIEW_PUB" \
	CARGO_TARGET_DIR="$NODE_TARGET" \
		cargo build --manifest-path "$REPO_ROOT/Cargo.toml" --release --locked --bin legion
	[ -x "$LEGION_BIN" ] || die "expected legion binary at $LEGION_BIN"

	read -r SEQ1 SEQ2 SEQ3 VC1 VC2 VC3 <<< "$(node -e '
import("./scripts/make-update-set.mjs").then((m) => {
  const now = Date.now();
  const s1 = m.currentSequence(new Date(now - 20 * 60_000));
  let s2 = m.currentSequence(new Date(now - 10 * 60_000));
  if (s2 <= s1) s2 = s1 + 1;
  let s3 = m.currentSequence(new Date(now));
  if (s3 <= s2) s3 = s2 + 1;
  console.log(
    s1,
    s2,
    s3,
    m.androidVersionCode(s1),
    m.androidVersionCode(s2),
    m.androidVersionCode(s3),
  );
});
')"
	say "Sequences: seq1=$SEQ1 (vc=$VC1), seq2=$SEQ2 (vc=$VC2), seq3=$SEQ3 (vc=$VC3)"

	RELEASE_APK="$HERE/src-tauri/gen/android/app/build/outputs/apk/universal/release/legion-release-universal.apk"

	build_apk() {
		local seq="$1"
		local out="$2"
		say "Building signed emulator release APK for sequence $seq"
		LEGION_APP_KEYSTORE="$TEST_KEYSTORE" \
		LEGION_PREVIEW_UPDATE_KEY="$PREVIEW_PUB" \
		LEGION_BUILD_REV="seq-$seq" \
			"$HERE/build-android.sh" --emulator --sequence "$seq"
		[ -f "$RELEASE_APK" ] || die "missing built APK at $RELEASE_APK"
		cp "$RELEASE_APK" "$out"
	}

	build_apk "$SEQ1" "$APK1"
	build_apk "$SEQ2" "$APK2"
	build_apk "$SEQ3" "$APK3"

	rm -rf "$SET2_DIR" "$SET3_DIR"
	say "Packaging and signing preview update sets (verifying embedded APK versionCode)"
	node "$REPO_ROOT/scripts/make-update-set.mjs" create \
		--out "$SET2_DIR" \
		--channel preview \
		--version 0.1.0 \
		--build "seq-$SEQ2" \
		--sequence "$SEQ2" \
		--notes "Android E2E preview update to seq2." \
		--key "$PREVIEW_KEY" \
		--artifact "$APK2:program=app,os=android,arch=universal,kind=apk,name=Legion-android-universal.apk"

	node "$REPO_ROOT/scripts/make-update-set.mjs" create \
		--out "$SET3_DIR" \
		--channel preview \
		--version 0.1.0 \
		--build "seq-$SEQ3" \
		--sequence "$SEQ3" \
		--notes "Android E2E background silent update to seq3." \
		--key "$PREVIEW_KEY" \
		--artifact "$APK3:program=app,os=android,arch=universal,kind=apk,name=Legion-android-universal.apk"

	cat > "$META_ENV" <<EOF
SEQ1=$SEQ1
SEQ2=$SEQ2
SEQ3=$SEQ3
VC1=$VC1
VC2=$VC2
VC3=$VC3
EOF
fi

# ---------------------------------------------------------------------------
# Throwaway node `tower` + loopback sshd on 127.0.0.1
# ---------------------------------------------------------------------------

NODE_HOME="$RUN_DIR/node"
AUTH_KEYS="$RUN_DIR/authorized_keys"
HOST_KEY="$RUN_DIR/host_ed25519"
SSHD_CFG="$RUN_DIR/sshd_config"
SSHD_LOG="$RUN_DIR/sshd.log"
SSHD_PIDFILE="$RUN_DIR/sshd.pid"

mkdir -p "$NODE_HOME"
ssh-keygen -q -t ed25519 -N '' -C 'legion-e2e-updates-host' -f "$HOST_KEY"
: > "$AUTH_KEYS"
chmod 600 "$AUTH_KEYS"

SSH_PORT="$(node -e '
const net = require("node:net");
const s = net.createServer();
s.listen(0, "127.0.0.1", () => {
  console.log(s.address().port);
  s.close();
});
')"

CURRENT_USER="$(id -un)"
LEGION_HOME="$NODE_HOME" "$LEGION_BIN" init --name tower >/dev/null
cat > "$NODE_HOME/config/policy.toml" <<'EOF'
[roles.observer]
allow = ["effect:read"]

[roles.operator]
allow = ["effect:read", "effect:write", "effect:exec", "effect:power"]
EOF

cat >> "$NODE_HOME/config/node.toml" <<EOF

[devices]
hosts = ["10.0.2.2"]
port = $SSH_PORT
user = "$CURRENT_USER"
authorized_keys = "$AUTH_KEYS"
host_keys = ["$HOST_KEY.pub"]
EOF

PENALTIES_LINE=""
if "$SSHD_BIN" -T -o PerSourcePenalties=no -h "$HOST_KEY" >/dev/null 2>&1; then
	PENALTIES_LINE="PerSourcePenalties no"
fi

cat > "$SSHD_CFG" <<EOF
ListenAddress 127.0.0.1
Port $SSH_PORT
HostKey "$HOST_KEY"
AuthorizedKeysFile $AUTH_KEYS
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

say "Starting throwaway sshd for tower on 127.0.0.1:$SSH_PORT"
"$SSHD_BIN" -D -e -f "$SSHD_CFG" -E "$SSHD_LOG" </dev/null >/dev/null 2>&1 &
SSHD_PID=$!
printf '%s\n' "$SSHD_PID" > "$SSHD_PIDFILE"
for _i in $(seq 1 50); do
	kill -0 "$SSHD_PID" 2>/dev/null || { cat "$SSHD_LOG" >&2 || true; die "throwaway sshd exited early"; }
	if node -e '
const net = require("node:net");
const s = net.connect(Number(process.argv[1]), "127.0.0.1");
s.on("connect", () => { s.destroy(); process.exit(0); });
s.on("error", () => process.exit(1));
' "$SSH_PORT" 2>/dev/null; then
		break
	fi
	sleep 0.1
done

INVITE_JSON="$(LEGION_HOME="$NODE_HOME" "$LEGION_BIN" pair --name phone --role operator --host 10.0.2.2 --port "$SSH_PORT" --json)"
INVITE="$(printf '%s' "$INVITE_JSON" | node -e 'const d = JSON.parse(require("node:fs").readFileSync(0,"utf8")); console.log(d.invite || "");')"
[ -n "$INVITE" ] || die "failed to create pairing invite: $INVITE_JSON"

# ---------------------------------------------------------------------------
# Boot isolated headless emulator on a free port (-read-only so it never collides)
# ---------------------------------------------------------------------------

command -v emulator >/dev/null 2>&1 || die "emulator binary is not on PATH."

pick_free_emu_port() {
	node -e '
const net = require("node:net");
const { execFileSync } = require("node:child_process");
const active = new Set(
  execFileSync("adb", ["devices"], { encoding: "utf8" })
    .split("\n")
    .map((l) => l.trim().split(/\s+/)[0])
    .filter((s) => /^emulator-\d+$/.test(s))
    .map((s) => Number(s.replace("emulator-", ""))),
);
function canBind(port) {
  return new Promise((resolve) => {
    const s = net.createServer();
    s.once("error", () => resolve(false));
    s.listen(port, "127.0.0.1", () => s.close(() => resolve(true)));
  });
}
(async () => {
  for (let p = 5562; p <= 5620; p += 2) {
    if (active.has(p)) continue;
    if ((await canBind(p)) && (await canBind(p + 1))) {
      console.log(p);
      return;
    }
  }
  process.exit(1);
})();
'
}

if [ -z "$EMU_PORT" ]; then
	EMU_PORT="$(pick_free_emu_port)" || die "could not find a free emulator console port"
fi
EMU_SERIAL="emulator-$EMU_PORT"
EMU_LOG="$RUN_DIR/emulator-${EMU_PORT}.log"

say "Booting isolated emulator '$AVD' on port $EMU_PORT ($EMU_SERIAL)"
emulator \
	-avd "$AVD" \
	-port "$EMU_PORT" \
	-read-only \
	-no-window \
	-no-audio \
	-gpu swiftshader_indirect \
	-no-boot-anim \
	-no-snapshot-save \
	>"$EMU_LOG" 2>&1 </dev/null &
EMU_PID=$!

BOOT_DEADLINE=$(( $(date +%s) + 180 ))
while true; do
	if ! kill -0 "$EMU_PID" 2>/dev/null; then
		tail -n 40 "$EMU_LOG" >&2 || true
		die "emulator $EMU_SERIAL exited before coming online"
	fi
	if adb devices | awk -v s="$EMU_SERIAL" 'NR>1 && $1 == s && $2 == "device" { found=1 } END { exit !found }'; then
		break
	fi
	[ "$(date +%s)" -lt "$BOOT_DEADLINE" ] || { tail -n 40 "$EMU_LOG" >&2 || true; die "timed out waiting for $EMU_SERIAL to appear in adb"; }
	sleep 1
done

while true; do
	BOOT_DONE="$(adb -s "$EMU_SERIAL" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r' || true)"
	if [ "$BOOT_DONE" = "1" ] && adb -s "$EMU_SERIAL" shell pm path android >/dev/null 2>&1; then
		if adb -s "$EMU_SERIAL" shell ip -4 addr show 2>/dev/null | grep -q '10\.0\.2\.'; then
			break
		fi
	fi
	[ "$(date +%s)" -lt "$BOOT_DEADLINE" ] || { tail -n 40 "$EMU_LOG" >&2 || true; die "timed out waiting for $EMU_SERIAL boot completion"; }
	sleep 1
done

SERIAL="$EMU_SERIAL"
adb -s "$SERIAL" shell input keyevent KEYCODE_WAKEUP >/dev/null 2>&1 || true
adb -s "$SERIAL" shell wm dismiss-keyguard >/dev/null 2>&1 || true
adb -s "$SERIAL" shell svc power stayon true >/dev/null 2>&1 || true
adb -s "$SERIAL" uninstall "$APP_ID" >/dev/null 2>&1 || true
adb -s "$SERIAL" install -r "$APK1"
adb -s "$SERIAL" shell pm clear "$APP_ID" >/dev/null
adb -s "$SERIAL" shell pm grant "$APP_ID" android.permission.ACCESS_LOCAL_NETWORK >/dev/null 2>&1 || true
# Use unmetered emulator Wi-Fi so background auto-downloads over 5 MiB are not paused as metered.
adb -s "$SERIAL" shell svc wifi enable >/dev/null 2>&1 || true
adb -s "$SERIAL" shell svc data disable >/dev/null 2>&1 || true
adb -s "$SERIAL" shell cmd uimode night yes >/dev/null 2>&1 || true

E2E_ADB_SERIAL="$SERIAL" \
E2E_APP_ID="$APP_ID" \
E2E_SHOTS_DIR="$SHOTS_DIR" \
E2E_LEGION_BIN="$LEGION_BIN" \
E2E_NODE_HOME="$NODE_HOME" \
E2E_INVITE="$INVITE" \
E2E_SET2_DIR="$SET2_DIR" \
E2E_SET3_DIR="$SET3_DIR" \
E2E_SEQ1="$SEQ1" \
E2E_SEQ2="$SEQ2" \
E2E_SEQ3="$SEQ3" \
E2E_VC1="$VC1" \
E2E_VC2="$VC2" \
E2E_VC3="$VC3" \
	node "$HERE/e2e-android-updates-driver.mjs"

say ""
say "Android E2E self-update verification passed. Screenshots saved in: $SHOTS_DIR"
