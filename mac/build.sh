#!/usr/bin/env bash
#
# Builds "Legion Control.app" from the SwiftPM executable and installs it into /Applications so the
# Raycast launcher finds it by name. Safe to run over and over, and never needs sudo: /Applications is
# writable by admin users.
#
#   ./build.sh                 build, sign, install into /Applications
#   ./build.sh --no-install    build and sign only, leave the bundle in mac/build
#   ./build.sh --install-dir D install into D instead of /Applications
#   ./build.sh --require-agent  fail unless the signed agent bundle is present (release builds)
#
# The signed agent bundle (dist/legionctl-agent-<version>.tgz plus its signed manifest and detached
# signature, produced by scripts/package-agent.mjs) is copied into Contents/Resources/agent when it
# is there. Without it the app still builds and still runs; its "install the agent" action is
# present and disabled, and says why. A release build passes --require-agent so that a release can
# never ship claiming an ability it does not have.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_NAME="Legion Control"
EXECUTABLE="LegionControl"
CONFIGURATION="release"
INSTALL_DIR="/Applications"
DO_INSTALL=1
REQUIRE_AGENT=0
DIST_DIR="$(cd "$HERE/.." && pwd)/dist"

while [[ $# -gt 0 ]]; do
	case "$1" in
		--no-install) DO_INSTALL=0 ;;
		--install-dir) INSTALL_DIR="${2:?--install-dir needs a path}"; shift ;;
		--require-agent) REQUIRE_AGENT=1 ;;
		--debug) CONFIGURATION="debug" ;;
		-h|--help) sed -n '2,12p' "${BASH_SOURCE[0]}" | cut -c3-; exit 0 ;;
		*) echo "Unknown option: $1" >&2; exit 2 ;;
	esac
	shift
done

echo "Building $APP_NAME ($CONFIGURATION)"
swift build -c "$CONFIGURATION" --package-path "$HERE"
BIN_PATH="$(swift build -c "$CONFIGURATION" --package-path "$HERE" --show-bin-path)"

STAGE="$HERE/build/$APP_NAME.app"
rm -rf "$STAGE"
mkdir -p "$STAGE/Contents/MacOS" "$STAGE/Contents/Resources"

install -m 0755 "$BIN_PATH/$EXECUTABLE" "$STAGE/Contents/MacOS/$EXECUTABLE"
install -m 0644 "$HERE/Resources/Info.plist" "$STAGE/Contents/Info.plist"
install -m 0644 "$HERE/Resources/AppIcon.icns" "$STAGE/Contents/Resources/AppIcon.icns"
printf 'APPL????' > "$STAGE/Contents/PkgInfo"

# The agent bundle, when the release machinery has produced one.
AGENT_MANIFEST="$DIST_DIR/Legion-Control-agent-manifest.json"
AGENT_SIGNATURE="$AGENT_MANIFEST.sig"
AGENT_ARCHIVE=""
if [[ -f "$AGENT_MANIFEST" ]]; then
	# The version comes out of the signed manifest, never out of a file name, so the app can never
	# claim to be shipping a version the signature does not cover.
	AGENT_VERSION="$(/usr/bin/plutil -extract agentVersion raw -o - "$AGENT_MANIFEST" 2>/dev/null || true)"
	if [[ -n "$AGENT_VERSION" ]]; then
		AGENT_ARCHIVE="$DIST_DIR/legionctl-agent-$AGENT_VERSION.tgz"
	fi
fi

if [[ -n "$AGENT_ARCHIVE" && -f "$AGENT_ARCHIVE" && -f "$AGENT_SIGNATURE" ]]; then
	mkdir -p "$STAGE/Contents/Resources/agent"
	install -m 0644 "$AGENT_ARCHIVE" "$STAGE/Contents/Resources/agent/$(basename "$AGENT_ARCHIVE")"
	install -m 0644 "$AGENT_MANIFEST" "$STAGE/Contents/Resources/agent/$(basename "$AGENT_MANIFEST")"
	install -m 0644 "$AGENT_SIGNATURE" "$STAGE/Contents/Resources/agent/$(basename "$AGENT_SIGNATURE")"
	echo "Embedded agent $AGENT_VERSION"
elif [[ "$REQUIRE_AGENT" -eq 1 ]]; then
	echo "No signed agent bundle in $DIST_DIR. Run scripts/package-agent.mjs first." >&2
	exit 1
else
	echo "No signed agent bundle in $DIST_DIR; the agent install action will be unavailable in this build."
fi

# Ad hoc signature: enough for a locally built app, and it keeps the local network prompt attached to a
# stable identity instead of asking again after every rebuild.
codesign --force --sign - --timestamp=none "$STAGE"
codesign --verify --strict "$STAGE"
echo "Built $STAGE"

if [[ "$DO_INSTALL" -eq 0 ]]; then
	echo "Skipping the install step."
	exit 0
fi

DEST="$INSTALL_DIR/$APP_NAME.app"
if pgrep -f "$DEST/Contents/MacOS/$EXECUTABLE" >/dev/null 2>&1; then
	echo "Quitting the running copy"
	# A plain TERM rather than an Apple event: this script has to work from a shell with no
	# automation permission, and the app has no unsaved state to negotiate about.
	pkill -TERM -f "$DEST/Contents/MacOS/$EXECUTABLE" >/dev/null 2>&1 || true
	for _ in 1 2 3 4 5 6 7 8 9 10; do
		pgrep -f "$DEST/Contents/MacOS/$EXECUTABLE" >/dev/null 2>&1 || break
		sleep 0.3
	done
fi

mkdir -p "$INSTALL_DIR"
rsync -a --delete "$STAGE/" "$DEST/"
codesign --verify --strict "$DEST"

LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
[[ -x "$LSREGISTER" ]] && "$LSREGISTER" -f "$DEST" >/dev/null 2>&1 || true

echo "Installed $DEST"
