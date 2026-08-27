#!/usr/bin/env bash
#
# Builds "Legion Control.app" from the SwiftPM executable and installs it into /Applications so the
# Raycast launcher finds it by name. Safe to run over and over, and never needs sudo: /Applications is
# writable by admin users.
#
#   ./build.sh                 build, sign, install into /Applications
#   ./build.sh --no-install    build and sign only, leave the bundle in mac/build
#   ./build.sh --install-dir D install into D instead of /Applications
#
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_NAME="Legion Control"
EXECUTABLE="LegionControl"
CONFIGURATION="release"
INSTALL_DIR="/Applications"
DO_INSTALL=1

while [[ $# -gt 0 ]]; do
	case "$1" in
		--no-install) DO_INSTALL=0 ;;
		--install-dir) INSTALL_DIR="${2:?--install-dir needs a path}"; shift ;;
		--debug) CONFIGURATION="debug" ;;
		-h|--help) sed -n '2,10p' "${BASH_SOURCE[0]}" | cut -c3-; exit 0 ;;
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
	osascript -e "tell application \"$APP_NAME\" to quit" >/dev/null 2>&1 || true
	sleep 1
fi

mkdir -p "$INSTALL_DIR"
rsync -a --delete "$STAGE/" "$DEST/"
codesign --verify --strict "$DEST"

LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
[[ -x "$LSREGISTER" ]] && "$LSREGISTER" -f "$DEST" >/dev/null 2>&1 || true

echo "Installed $DEST"
