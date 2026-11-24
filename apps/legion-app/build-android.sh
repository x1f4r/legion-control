#!/usr/bin/env bash
#
# Legion for Android: build and sign a size-optimised release APK.
#
#   ./apps/legion-app/build-android.sh              build arm64-v8a release APK and sign it
#   ./apps/legion-app/build-android.sh --emulator   include x86_64 alongside arm64-v8a
#   ./apps/legion-app/build-android.sh --clean      clean Gradle build outputs first
#   ./apps/legion-app/build-android.sh --install    install the signed APK on the connected device
#
# The release keystore lives outside the repository at:
#   ${LEGION_APP_KEYSTORE:-$HOME/.legion-control/legion-app-release.jks}
# with its password in a sibling .pw file. Both are created on first run with mode 600, and the
# password is never printed or passed on a command line. Read apps/legion-app/SIGNING.md and back
# both files up.
#
set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
ANDROID_DIR="$HERE/src-tauri/gen/android"

KEYSTORE="${LEGION_APP_KEYSTORE:-$HOME/.legion-control/legion-app-release.jks}"
case "$KEYSTORE" in
	*.jks) PW_FILE="${KEYSTORE%.jks}.pw" ;;
	*.keystore) PW_FILE="${KEYSTORE%.keystore}.pw" ;;
	*) PW_FILE="${KEYSTORE}.pw" ;;
esac
KEY_ALIAS="${LEGION_APP_KEY_ALIAS:-legion-app}"
KEY_DNAME="${LEGION_APP_KEY_DNAME:-CN=Legion}"
KEY_VALIDITY_DAYS=9862

WITH_EMULATOR=0
DO_CLEAN=0
DO_INSTALL=0

die() { printf '\nERROR: %s\n' "$1" >&2; exit 1; }
say() { printf '%s\n' "$1"; }

while [[ $# -gt 0 ]]; do
	case "$1" in
		--emulator) WITH_EMULATOR=1 ;;
		--clean) DO_CLEAN=1 ;;
		--install) DO_INSTALL=1 ;;
		-h|--help)
			awk 'NR>1 && /^#/ { sub(/^# ?/, ""); print; next } NR>1 { exit }' "${BASH_SOURCE[0]}"
			exit 0
			;;
		*) die "unknown option: $1" ;;
	esac
	shift
done

# ---------------------------------------------------------------------------
# Toolchain
# ---------------------------------------------------------------------------

[ -d "$ANDROID_DIR" ] || die "no Android project at $ANDROID_DIR. Run 'cargo tauri android init' first."
command -v cargo >/dev/null 2>&1 || die "cargo is not on PATH."
cargo tauri --version >/dev/null 2>&1 || die "cargo-tauri is not installed or not on PATH."
command -v openssl >/dev/null 2>&1 || die "openssl is required to generate the keystore password."

if [ -z "${JAVA_HOME:-}" ]; then
	if [ -d "/usr/lib/jvm/java-21-openjdk" ]; then
		export JAVA_HOME="/usr/lib/jvm/java-21-openjdk"
	elif [ -d "/Library/Java/JavaVirtualMachines/temurin-21.jdk/Contents/Home" ]; then
		export JAVA_HOME="/Library/Java/JavaVirtualMachines/temurin-21.jdk/Contents/Home"
	fi
fi
[ -n "${JAVA_HOME:-}" ] && [ -x "$JAVA_HOME/bin/keytool" ] \
	|| die "JAVA_HOME must point to a JDK with keytool (JDK 21)."

SDK_DIR="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-}}"
if [ -z "$SDK_DIR" ]; then
	if [ -d "$HOME/Android/Sdk" ]; then
		SDK_DIR="$HOME/Android/Sdk"
	elif [ -d "$HOME/Library/Android/sdk" ]; then
		SDK_DIR="$HOME/Library/Android/sdk"
	fi
fi
[ -n "$SDK_DIR" ] && [ -d "$SDK_DIR" ] \
	|| die "Android SDK not found. Set ANDROID_HOME or ANDROID_SDK_ROOT."
export ANDROID_HOME="$SDK_DIR"
export ANDROID_SDK_ROOT="$SDK_DIR"

if [ -z "${NDK_HOME:-}" ] && [ -z "${ANDROID_NDK_HOME:-}" ]; then
	NDK_LATEST="$(find "$SDK_DIR/ndk" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort -V | tail -n 1 || true)"
	[ -n "$NDK_LATEST" ] || die "Android NDK not found under $SDK_DIR/ndk. Set NDK_HOME."
	export NDK_HOME="$NDK_LATEST"
	export ANDROID_NDK_HOME="$NDK_LATEST"
else
	export NDK_HOME="${NDK_HOME:-$ANDROID_NDK_HOME}"
	export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$NDK_HOME}"
fi

BUILD_TOOLS_VER="$(find "$SDK_DIR/build-tools" -mindepth 1 -maxdepth 1 -type d -exec basename {} \; 2>/dev/null | sort -V | tail -n 1 || true)"
[ -n "$BUILD_TOOLS_VER" ] || die "no build-tools under $SDK_DIR/build-tools."
APKSIGNER="$SDK_DIR/build-tools/$BUILD_TOOLS_VER/apksigner"
ZIPALIGN="$SDK_DIR/build-tools/$BUILD_TOOLS_VER/zipalign"
AAPT2="$SDK_DIR/build-tools/$BUILD_TOOLS_VER/aapt2"
[ -x "$APKSIGNER" ] || die "apksigner not found at $APKSIGNER."
[ -x "$ZIPALIGN" ] || die "zipalign not found at $ZIPALIGN."
[ -x "$AAPT2" ] || die "aapt2 not found at $AAPT2."

# ---------------------------------------------------------------------------
# Signing key
# ---------------------------------------------------------------------------

KEY_DIR="$(dirname -- "$KEYSTORE")"
mkdir -p "$KEY_DIR"

if [ ! -f "$PW_FILE" ]; then
	say "Creating a signing key password at $PW_FILE"
	( umask 077; openssl rand -hex 24 > "$PW_FILE" )
fi
chmod 600 "$PW_FILE"
[ -s "$PW_FILE" ] || die "$PW_FILE is empty. Remove it before generating a new keystore."

if [ ! -f "$KEYSTORE" ]; then
	say "Creating a release signing key at $KEYSTORE (RSA 4096, valid $KEY_VALIDITY_DAYS days)"
	say "Back this file and its .pw file up. Read apps/legion-app/SIGNING.md for why."
	( umask 077; "$JAVA_HOME/bin/keytool" -genkeypair \
		-keystore "$KEYSTORE" \
		-storetype PKCS12 \
		-storepass:file "$PW_FILE" \
		-keypass:file "$PW_FILE" \
		-alias "$KEY_ALIAS" \
		-keyalg RSA -keysize 4096 -sigalg SHA256withRSA \
		-validity "$KEY_VALIDITY_DAYS" \
		-dname "$KEY_DNAME" ) >/dev/null
fi
chmod 600 "$KEYSTORE"

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

if [ "$DO_CLEAN" -eq 1 ]; then
	say "Cleaning Gradle outputs"
	( cd "$ANDROID_DIR" && ./gradlew --no-daemon clean )
fi

TARGETS=(--target aarch64)
OUT_LABEL="arm64"
if [ "$WITH_EMULATOR" -eq 1 ]; then
	TARGETS+=(--target x86_64)
	OUT_LABEL="arm64+x86_64"
fi

say "Building release APK ($OUT_LABEL)"
( cd "$HERE" && cargo tauri android build --apk "${TARGETS[@]}" )

APK_OUT_DIR="$ANDROID_DIR/app/build/outputs/apk/universal/release"
UNSIGNED="$APK_OUT_DIR/app-universal-release-unsigned.apk"
if [ ! -f "$UNSIGNED" ]; then
	UNSIGNED="$(find "$ANDROID_DIR/app/build/outputs/apk" -path '*/release/*-unsigned.apk' -type f | sort | head -n 1 || true)"
fi
[ -n "$UNSIGNED" ] && [ -f "$UNSIGNED" ] \
	|| die "no unsigned release APK found under $ANDROID_DIR/app/build/outputs/apk."

OUT_DIR="$(dirname -- "$UNSIGNED")"
if [ "$WITH_EMULATOR" -eq 1 ]; then
	SIGNED="$OUT_DIR/legion-release-universal.apk"
else
	SIGNED="$OUT_DIR/legion-release-arm64.apk"
fi
ALIGNED="$OUT_DIR/.legion-release-aligned.apk"

say "Aligning and signing"
rm -f "$ALIGNED" "$SIGNED" "$SIGNED.idsig"
"$ZIPALIGN" -p -f 4 "$UNSIGNED" "$ALIGNED"
# PKCS12 uses the store password for the key entry; passing --key-pass with the same file:
# source causes apksigner to hit EOF on the second read.
"$APKSIGNER" sign \
	--ks "$KEYSTORE" \
	--ks-pass "file:$PW_FILE" \
	--ks-key-alias "$KEY_ALIAS" \
	--out "$SIGNED" \
	"$ALIGNED"
rm -f "$ALIGNED" "$SIGNED.idsig"

VERIFY_OUTPUT="$("$APKSIGNER" verify --print-certs --verbose "$SIGNED")"
BADGING_OUTPUT="$("$AAPT2" dump badging "$SIGNED")"
if printf '%s\n' "$BADGING_OUTPUT" | grep -q '^application-debuggable'; then
	rm -f "$SIGNED"
	die "release APK has android:debuggable=true ($SIGNED); refusing to ship."
fi
CERT_SHA256="$(printf '%s\n' "$VERIFY_OUTPUT" | sed -n 's/^.*certificate SHA-256 digest: //p' | head -n 1)"
SIZE_BYTES="$(wc -c < "$SIGNED" | tr -d '[:space:]')"
SIZE_HUMAN="$(du -h "$SIGNED" | cut -f1)"

say ""
printf '%s\n' "$VERIFY_OUTPUT"
say ""
say "APK: $SIGNED"
say "Size: $SIZE_HUMAN ($SIZE_BYTES bytes)"
say "SHA-256: $CERT_SHA256"

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------

[ "$DO_INSTALL" -eq 1 ] || exit 0

ADB="$SDK_DIR/platform-tools/adb"
[ -x "$ADB" ] || die "adb not found at $ADB."

say ""
say "Installing"
"$ADB" install -r "$SIGNED"
