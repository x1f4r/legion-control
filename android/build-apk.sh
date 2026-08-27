#!/usr/bin/env bash
#
# Legion Control for Android: build a signed release APK.
#
#   ./android/build-apk.sh              build, sign, print where the APK landed
#   ./android/build-apk.sh --install    same, then install it on the connected phone
#   ./android/build-apk.sh --clean      throw the previous build output away first
#
# The signing key lives outside this repository, at ~/.legion-control/android-release.jks, with its
# password in ~/.legion-control/android-release.pw. Both are created on the first run, both are mode
# 600, and neither is ever printed by this script. Read android/keystore.md before you touch either,
# and back the keystore up: if it is lost, an already installed copy of the app can only be updated
# by uninstalling it first, which throws away its ssh key.
#
# The toolchain is not negotiable and the failure modes are ugly, so this script pins all of it:
# Gradle 9.5 through the wrapper checked into the repository, never the newer Gradle on PATH, because
# AGP 8.13 calls into a Gradle internal that 9.6 removed. And JDK 21, because the java on PATH is 26
# and AGP refuses to run on it. android/README.md has the long version.
#
set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"

JAVA_HOME_PINNED="${LEGION_ANDROID_JAVA_HOME:-/Library/Java/JavaVirtualMachines/temurin-21.jdk/Contents/Home}"
SDK_DEFAULT="$HOME/Library/Android/sdk"

KEY_DIR="$HOME/.legion-control"
KEYSTORE="$KEY_DIR/android-release.jks"
PW_FILE="$KEY_DIR/android-release.pw"
KEY_ALIAS="legion-control"
# Overridable, and deliberately bare: a self signed key for an app installed by hand needs a name
# and nothing else, and an organisation or a country in there is a claim rather than a fact.
KEY_DNAME="${LEGION_ANDROID_KEY_DNAME:-CN=Legion Control}"
KEY_VALIDITY_DAYS=9862 # about 27 years, so this never expires in practice

DO_INSTALL=0
DO_CLEAN=0

die() { printf '\nERROR: %s\n' "$1" >&2; exit 1; }
say() { printf '%s\n' "$1"; }

while [[ $# -gt 0 ]]; do
	case "$1" in
		--install) DO_INSTALL=1 ;;
		--clean) DO_CLEAN=1 ;;
		-h|--help) awk 'NR>1 && /^#/ { sub(/^# ?/, ""); print; next } NR>1 { exit }' "${BASH_SOURCE[0]}"; exit 0 ;;
		*) die "unknown option: $1" ;;
	esac
	shift
done

# ---------------------------------------------------------------------------
# Toolchain
# ---------------------------------------------------------------------------

[ -x "$HERE/gradlew" ] || die "no Gradle wrapper at $HERE/gradlew. The wrapper is what pins Gradle 9.5; do not fall back to the gradle on PATH, it is too new for AGP 8.13."
[ -f "$HERE/app/build.gradle.kts" ] || die "no app module at $HERE/app/build.gradle.kts."

[ -d "$JAVA_HOME_PINNED" ] || die "no JDK 21 at $JAVA_HOME_PINNED. Install Temurin 21, or point LEGION_ANDROID_JAVA_HOME at your own copy. The default java is 26 and AGP rejects it."
export JAVA_HOME="$JAVA_HOME_PINNED"

JAVA_MAJOR="$("$JAVA_HOME/bin/java" -XshowSettings:properties -version 2>&1 | sed -n 's/.*java\.specification\.version = //p' | head -1)"
[ "$JAVA_MAJOR" = "21" ] || die "$JAVA_HOME is Java ${JAVA_MAJOR:-unknown}, not 21. AGP 8.13 needs 21."

# The SDK location: whatever local.properties already says wins, so a hand edited path is respected.
# Otherwise take it from the environment, otherwise the standard Android Studio location, and write
# it down. local.properties is machine local and gitignored on purpose.
LOCAL_PROPS="$HERE/local.properties"
if [ -f "$LOCAL_PROPS" ]; then
	SDK_DIR="$(sed -n 's/^[[:space:]]*sdk\.dir=//p' "$LOCAL_PROPS" | tail -1)"
fi
if [ -z "${SDK_DIR:-}" ]; then
	SDK_DIR="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-$SDK_DEFAULT}}"
	printf 'sdk.dir=%s\n' "$SDK_DIR" >> "$LOCAL_PROPS"
	say "Wrote sdk.dir=$SDK_DIR into android/local.properties"
fi
[ -d "$SDK_DIR" ] || die "no Android SDK at $SDK_DIR. Install it, or fix sdk.dir in $LOCAL_PROPS."

# Newest build-tools wins. apksigner and zipalign are stable across versions, so this does not need
# to match compileSdk, it only needs to exist.
BUILD_TOOLS_DIR="$(ls -1 "$SDK_DIR/build-tools" 2>/dev/null | sort -V | tail -1)"
[ -n "$BUILD_TOOLS_DIR" ] || die "no build-tools under $SDK_DIR/build-tools. Install one with sdkmanager."
APKSIGNER="$SDK_DIR/build-tools/$BUILD_TOOLS_DIR/apksigner"
ZIPALIGN="$SDK_DIR/build-tools/$BUILD_TOOLS_DIR/zipalign"
[ -x "$APKSIGNER" ] || die "no apksigner in $SDK_DIR/build-tools/$BUILD_TOOLS_DIR."
[ -x "$ZIPALIGN" ] || die "no zipalign in $SDK_DIR/build-tools/$BUILD_TOOLS_DIR."

# ---------------------------------------------------------------------------
# Signing key
# ---------------------------------------------------------------------------

# The directory is shared with the agent tree the Mac installer deploys, so its mode is left alone
# and the two files are locked down individually instead.
mkdir -p "$KEY_DIR"

if [ ! -f "$PW_FILE" ]; then
	say "Creating a signing key password at $PW_FILE"
	( umask 077; openssl rand -hex 24 > "$PW_FILE" )
fi
chmod 600 "$PW_FILE"
[ -s "$PW_FILE" ] || die "$PW_FILE is empty. Delete it and run again to get a fresh password, but note that this only works if the keystore does not exist yet."

if [ ! -f "$KEYSTORE" ]; then
	say "Creating a release signing key at $KEYSTORE (RSA 4096, valid $KEY_VALIDITY_DAYS days)"
	say "Back this file up. Read android/keystore.md for why that matters."
	# PKCS12 rather than the old JKS format: it is the standard, apksigner and Gradle both read it,
	# and keytool nags about JKS. The .jks name is kept because that is what everything calls it.
	# ed25519 is not an option here, the APK signature schemes do not accept it, so RSA 4096 it is.
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

RELEASE_DIR="$HERE/app/build/outputs/apk/release"

if [ "$DO_CLEAN" -eq 1 ]; then
	say "Cleaning"
	( cd "$HERE" && ./gradlew --no-daemon clean )
fi

say "Building the release APK with the wrapper (Gradle $( sed -n 's/.*gradle-\([0-9.]*\)-bin\.zip/\1/p' "$HERE/gradle/wrapper/gradle-wrapper.properties" 2>/dev/null | head -1 ), JDK 21)"
( cd "$HERE" && ./gradlew --no-daemon assembleRelease )

# What AGP leaves behind depends on whether app/build.gradle.kts declares a signingConfig for the
# release build type. Without one it writes app-release-unsigned.apk and this script signs it. With
# one it writes an already signed app-release.apk and this script only verifies it. Both are fine;
# what is never fine is shipping a debug APK and calling it a release.
UNSIGNED="$RELEASE_DIR/app-release-unsigned.apk"
SIGNED="$RELEASE_DIR/app-release.apk"
ALIGNED="$RELEASE_DIR/app-release-aligned.apk"

if [ -f "$UNSIGNED" ]; then
	say "Aligning and signing"
	rm -f "$ALIGNED" "$SIGNED"
	"$ZIPALIGN" -p -f 4 "$UNSIGNED" "$ALIGNED"
	# No --key-pass, and that is deliberate rather than an oversight. The keystore is PKCS12, where
	# the key password is the store password, and apksigner already knows that. Passing --key-pass
	# with the same file: source does not work: apksigner reads both passwords from one open stream
	# and the second read hits end of file, which it reports as "Failed to read Key password".
	"$APKSIGNER" sign \
		--ks "$KEYSTORE" \
		--ks-pass "file:$PW_FILE" \
		--ks-key-alias "$KEY_ALIAS" \
		--out "$SIGNED" \
		"$ALIGNED"
	rm -f "$ALIGNED" "$SIGNED.idsig"
elif [ -f "$SIGNED" ]; then
	say "The app module signed the APK itself, verifying that rather than re-signing it"
else
	die "assembleRelease produced no APK in $RELEASE_DIR. Look at the Gradle output above."
fi

say ""
say "Signature:"
"$APKSIGNER" verify --print-certs --verbose "$SIGNED"

say ""
say "APK: $SIGNED"
say "Size: $(du -h "$SIGNED" | cut -f1)"

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------

[ "$DO_INSTALL" -eq 1 ] || exit 0

ADB="$SDK_DIR/platform-tools/adb"
[ -x "$ADB" ] || die "no adb at $ADB, so --install cannot work."

DEVICES="$("$ADB" devices | sed -n '2,$p' | grep -c "[[:space:]]device$" || true)"
if [ "$DEVICES" -eq 0 ]; then
	printf '\nERROR: no device is connected, so nothing was installed. The APK is still at:\n  %s\n' "$SIGNED" >&2
	printf 'A phone pairs over the LAN. Find it and reconnect with:\n  %s mdns services   # look for _adb-tls-connect._tcp\n  %s connect <host:port>\n' "$ADB" "$ADB" >&2
	exit 1
fi
if [ "$DEVICES" -gt 1 ]; then
	die "$DEVICES devices are connected and adb would not know which one you mean. Disconnect the others, or install by hand with: $ADB -s <serial> install -r \"$SIGNED\""
fi

say ""
say "Installing"
if ! OUT="$("$ADB" install -r "$SIGNED" 2>&1)"; then
	printf '%s\n' "$OUT" >&2
	case "$OUT" in
		*UPDATE_INCOMPATIBLE*|*INCONSISTENT_CERTIFICATES*)
			die "the copy already on the phone was signed with a different key, most likely a debug build. Uninstall it first and install again. That wipes the app's ssh key, so the new one has to be added to authorized_keys on every system it talks to." ;;
		*) die "adb install failed, see above." ;;
	esac
fi
printf '%s\n' "$OUT"
say "Installed on $("$ADB" shell getprop ro.product.model | tr -d '\r')"
