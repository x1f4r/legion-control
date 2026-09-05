#!/usr/bin/env bash
#
# Cut a release of Legion Control: four clients, one version, one tag, one GitHub release.
#
#   ./scripts/release.sh 1.1.0                   bump, build, tag, publish
#   ./scripts/release.sh 1.1.0 --dry-run         everything except commit, tag, push and publish
#   ./scripts/release.sh 1.1.0 --notes FILE      take the release notes from a file
#   ./scripts/release.sh 1.1.0 --notes-from-stdin
#
# With none of the notes options it opens $EDITOR on a template and waits.
#
# A release contains four platform clients, the shared agent, and a signed manifest.
# Checks, artifact version validation and integrity verification finish before publication.
# Signing material lives outside the repository; see docs/security.md.
#
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
# Everything below is relative to the repository, so it does not matter where this was run from,
# and gh reads the repository out of the working directory rather than being told twice.
cd "$ROOT"
PLIST_REL="mac/Resources/Info.plist"
GRADLE_REL="android/app/build.gradle.kts"
PLIST="$ROOT/$PLIST_REL"
GRADLE="$ROOT/$GRADLE_REL"
DIST="$ROOT/dist"
DESKTOP_REL="desktop/LegionControl.Desktop/LegionControl.Desktop.csproj"
DESKTOP_PROJECT="$ROOT/$DESKTOP_REL"
LINUX_ARCHIVE="$DIST/Legion-Control-linux-x64.tar.gz"
WINDOWS_ARCHIVE="$DIST/Legion-Control-windows-x64.zip"
MAC_ZIP="$DIST/Legion-Control-macos-arm64.zip"
ANDROID_APK="$DIST/Legion-Control-android-arm64.apk"
PLIST_BUDDY="/usr/libexec/PlistBuddy"

VERSION=""
DRY_RUN=0
NOTES_FILE=""
NOTES_FROM_STDIN=0

die() { printf '\nERROR: %s\n' "$1" >&2; exit 1; }
say() { printf '%s\n' "$1"; }
step() { printf '\n== %s\n' "$1"; }

# ---------------------------------------------------------------------------
# Arguments
# ---------------------------------------------------------------------------

while [ $# -gt 0 ]; do
	case "$1" in
		--dry-run) DRY_RUN=1 ;;
		--notes) NOTES_FILE="${2:-}"; [ -n "$NOTES_FILE" ] || die "--notes needs a file"; shift ;;
		--notes-from-stdin) NOTES_FROM_STDIN=1 ;;
		-h|--help) awk 'NR>1 && /^#/ { sub(/^# ?/, ""); print; next } NR>1 { exit }' "${BASH_SOURCE[0]}"; exit 0 ;;
		-*) die "unknown option: $1" ;;
		*) [ -z "$VERSION" ] || die "give one version, not two"; VERSION="$1" ;;
	esac
	shift
done

[ -n "$VERSION" ] || die "which version? Usage: ./scripts/release.sh X.Y.Z [--dry-run]"
printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' \
	|| die "\"$VERSION\" is not an X.Y.Z version. The tag, the plist and the gradle file all have to agree on it, and they only can if it is three numbers."

if [ "$NOTES_FROM_STDIN" -eq 1 ] && [ -n "$NOTES_FILE" ]; then
	die "--notes and --notes-from-stdin are two answers to the same question."
fi

# ---------------------------------------------------------------------------
# The tree has to be somewhere a release can come from
# ---------------------------------------------------------------------------

step "Checking the working tree"

command -v git >/dev/null 2>&1 || die "no git."
[ -x "$PLIST_BUDDY" ] || die "no PlistBuddy at $PLIST_BUDDY, so the Mac version cannot be read or written."
[ -f "$PLIST" ] || die "no $PLIST_REL."
[ -f "$GRADLE" ] || die "no $GRADLE_REL."

BRANCH="$(git -C "$ROOT" rev-parse --abbrev-ref HEAD)"
[ "$BRANCH" = "main" ] || die "on branch $BRANCH. A release is cut from main, because the tag has to point at what everyone else will get."

[ -z "$(git -C "$ROOT" status --porcelain)" ] \
	|| die "the working tree has changes in it. The release commit is meant to be the version bump and nothing else, so commit or stash what is there first."

if git -C "$ROOT" rev-parse -q --verify "refs/tags/v$VERSION" >/dev/null; then
	die "tag v$VERSION already exists. Versions are not reused: a phone that already downloaded it would never see the new one."
fi

if [ "$DRY_RUN" -eq 0 ]; then
	command -v gh >/dev/null 2>&1 || die "no gh, so the release cannot be published. Install it, or use --dry-run to build the artifacts and publish them by hand."
	gh auth status >/dev/null 2>&1 || die "gh is not signed in. Run: gh auth login"
fi

# ---------------------------------------------------------------------------
# Where the numbers are now
# ---------------------------------------------------------------------------

plist_get() { "$PLIST_BUDDY" -c "Print :$1" "$PLIST"; }
gradle_get() { sed -n -E "s/^[[:space:]]*$1 = \"?([^\"]*)\"?$/\1/p" "$GRADLE" | head -1; }

OLD_VERSION="$(plist_get CFBundleShortVersionString)"
OLD_BUILD="$(plist_get CFBundleVersion)"
OLD_NAME="$(gradle_get versionName)"
OLD_CODE="$(gradle_get versionCode)"

[ -n "$OLD_VERSION" ] || die "CFBundleShortVersionString is missing from $PLIST_REL."
[ -n "$OLD_NAME" ] || die "versionName is missing from $GRADLE_REL."
printf '%s' "$OLD_BUILD" | grep -Eq '^[0-9]+$' || die "CFBundleVersion in $PLIST_REL is \"$OLD_BUILD\", which cannot be incremented."
printf '%s' "$OLD_CODE" | grep -Eq '^[0-9]+$' || die "versionCode in $GRADLE_REL is \"$OLD_CODE\", which cannot be incremented."

[ "$OLD_VERSION" = "$OLD_NAME" ] \
	|| die "macOS and Android disagree about the current version: the Mac says $OLD_VERSION and Android says $OLD_NAME. Put them back in step before releasing."
[ "$OLD_BUILD" = "$OLD_CODE" ] \
	|| die "macOS and Android disagree about the current build number: the Mac says $OLD_BUILD and Android says $OLD_CODE. Put them back in step before releasing."

OLD_DESKTOP_VERSION="$(sed -n 's:.*<Version>\(.*\)</Version>.*:\1:p' "$DESKTOP_PROJECT" | head -1)"
[ "$OLD_DESKTOP_VERSION" = "$OLD_VERSION" ] || die "desktop says $OLD_DESKTOP_VERSION while macOS and Android say $OLD_VERSION."
BUILD_NUMBER=$((OLD_BUILD + 1))

say "Currently $OLD_VERSION ($OLD_BUILD), releasing $VERSION ($BUILD_NUMBER)."
if [ "$DRY_RUN" -eq 1 ]; then
	say "Dry run: the artifacts are built and checked, then the version numbers are put back and"
	say "nothing is committed, tagged, pushed or published."
fi

# ---------------------------------------------------------------------------
# Release notes, asked for before anything is built
# ---------------------------------------------------------------------------
#
# The editor comes first on purpose. Backing out of a release is free while it is still a blank
# template and expensive once two toolchains have run.

step "Release notes"

NOTES="$(mktemp -t legion-control-release-notes)"
cleanup_notes() { rm -f "$NOTES"; }

if [ -n "$NOTES_FILE" ]; then
	[ -f "$NOTES_FILE" ] || { cleanup_notes; die "no notes file at $NOTES_FILE."; }
	cat "$NOTES_FILE" > "$NOTES"
elif [ "$NOTES_FROM_STDIN" -eq 1 ]; then
	cat > "$NOTES"
else
	cat > "$NOTES" <<-EOF
		Say what changed, in prose, the way the last seven releases do. The first paragraph is
		the one both apps show in their update row, so it is the sentence that has to stand on
		its own.

		# Lines starting with a # are dropped. An empty file cancels the release.
		# Since $OLD_VERSION:
		$(git -C "$ROOT" log --pretty='#   %s' "v$OLD_VERSION..HEAD" 2>/dev/null || true)
	EOF
	"${EDITOR:-vi}" "$NOTES" || { cleanup_notes; die "the editor exited without saving."; }
	# Only the template's own lines are dropped, and only here. A file handed in with --notes is
	# taken exactly as written: a leading # there is a Markdown heading, not a comment.
	sed -i '' -e '/^#/d' "$NOTES"
fi

grep -q '[^[:space:]]' "$NOTES" \
	|| { cleanup_notes; die "the release notes are empty, so nothing was released."; }

say "Notes:"
sed -e 's/^/    /' "$NOTES"

# ---------------------------------------------------------------------------
# The bump, which is undone again unless it makes it all the way to a commit
# ---------------------------------------------------------------------------

BUMPED=0
COMMITTED=0

finish() {
	cleanup_notes
	if [ "$BUMPED" -eq 1 ] && [ "$COMMITTED" -eq 0 ]; then
		git -C "$ROOT" checkout -- "$PLIST_REL" "$GRADLE_REL" "$DESKTOP_REL" 2>/dev/null || true
		say ""
		say "The version numbers were put back to $OLD_VERSION ($OLD_BUILD)."
	fi
}
trap finish EXIT

step "Running the complete verification suite"
"$ROOT/scripts/check.sh" all

step "Bumping to $VERSION ($BUILD_NUMBER)"

BUMPED=1
"$PLIST_BUDDY" -c "Set :CFBundleShortVersionString $VERSION" "$PLIST"
"$PLIST_BUDDY" -c "Set :CFBundleVersion $BUILD_NUMBER" "$PLIST"
sed -i '' -E "s/^([[:space:]]*versionCode = )[0-9]+\$/\\1$BUILD_NUMBER/" "$GRADLE"
sed -i '' -E "s/^([[:space:]]*versionName = \")[^\"]*(\")\$/\\1$VERSION\\2/" "$GRADLE"

[ "$(plist_get CFBundleShortVersionString)" = "$VERSION" ] || die "the plist did not take the new version."
[ "$(plist_get CFBundleVersion)" = "$BUILD_NUMBER" ] || die "the plist did not take the new build number."
[ "$(gradle_get versionName)" = "$VERSION" ] || die "$GRADLE_REL did not take the new versionName."
[ "$(gradle_get versionCode)" = "$BUILD_NUMBER" ] || die "$GRADLE_REL did not take the new versionCode."

python3 - "$DESKTOP_PROJECT" "$VERSION" <<'PYVERSION'
import pathlib, re, sys
p = pathlib.Path(sys.argv[1]); text = p.read_text()
if not re.search(r'<Version>[^<]+</Version>', text):
    raise SystemExit('Desktop project needs an explicit Version property.')
p.write_text(re.sub(r'<Version>[^<]+</Version>', '<Version>' + sys.argv[2] + '</Version>', text))
PYVERSION
say "All client projects now say $VERSION ($BUILD_NUMBER)."

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

mkdir -p "$DIST"

step "Packaging and signing the bundled control agent"
node "$ROOT/scripts/package-agent.mjs"

step "Building the Mac app"

"$ROOT/mac/build.sh" --no-install
MAC_APP="$ROOT/mac/build/Legion Control.app"
[ -d "$MAC_APP" ] || die "mac/build.sh left no bundle at $MAC_APP."

# ditto -c -k --keepParent, and nothing else: it is the only archiver on the system that keeps the
# resource forks, the symlinks and the code signature a bundle needs to still be a signed bundle on
# the other side. A zip made any other way arrives as an app that will not launch.
ditto -c -k --keepParent "$MAC_APP" "$MAC_ZIP"
say "Wrote $(basename "$MAC_ZIP") ($(du -h "$MAC_ZIP" | cut -f1))"

step "Building the Android app"

"$ROOT/android/build-apk.sh"
BUILT_APK="$ROOT/android/app/build/outputs/apk/release/app-release.apk"
[ -f "$BUILT_APK" ] || die "android/build-apk.sh left no APK at $BUILT_APK."
cp "$BUILT_APK" "$ANDROID_APK"
say "Wrote $(basename "$ANDROID_APK") ($(du -h "$ANDROID_APK" | cut -f1))"

step "Building Linux and Windows desktop apps"
"$ROOT/desktop/build.sh"
[ -f "$LINUX_ARCHIVE" ] || die "desktop/build.sh left no Linux archive."
[ -f "$WINDOWS_ARCHIVE" ] || die "desktop/build.sh left no Windows archive."

# ---------------------------------------------------------------------------
# Ask the artifacts what they think they are
# ---------------------------------------------------------------------------
#
# Not paranoia: an incremental build that reused a stale bundle, or a Gradle daemon holding an old
# manifest, both produce an artifact that is fine except for the one number the whole release turns
# on. The apps compare against it to decide whether to offer an update at all.

step "Checking the artifacts"

VERIFY_DIR="$(mktemp -d -t legion-control-verify)"
ditto -x -k "$MAC_ZIP" "$VERIFY_DIR"
ZIPPED_PLIST="$VERIFY_DIR/Legion Control.app/Contents/Info.plist"
[ -f "$ZIPPED_PLIST" ] || { rm -rf "$VERIFY_DIR"; die "the Mac zip does not hold Legion Control.app."; }
ZIPPED_VERSION="$("$PLIST_BUDDY" -c "Print :CFBundleShortVersionString" "$ZIPPED_PLIST")"
ZIPPED_BUILD="$("$PLIST_BUDDY" -c "Print :CFBundleVersion" "$ZIPPED_PLIST")"
rm -rf "$VERIFY_DIR"
[ "$ZIPPED_BUILD" = "$BUILD_NUMBER" ] || die "the zipped app has build $ZIPPED_BUILD, not $BUILD_NUMBER."
[ "$ZIPPED_VERSION" = "$VERSION" ] || die "the zipped app says $ZIPPED_VERSION, not $VERSION."
say "The Mac zip holds Legion Control.app at $ZIPPED_VERSION."

# aapt only exists when an Android SDK is installed, and the APK was just built by a script that
# needs one. Refuse publication if the packaged version cannot be verified.
SDK_DIR="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-$HOME/Library/Android/sdk}}"
AAPT=""
for candidate in "$SDK_DIR"/build-tools/*/aapt2 "$SDK_DIR"/build-tools/*/aapt; do
	if [ -x "$candidate" ]; then AAPT="$candidate"; fi
done
if [ -n "$AAPT" ]; then
	APK_VERSION="$("$AAPT" dump badging "$ANDROID_APK" | sed -n "s/.*versionName='\([^']*\)'.*/\1/p" | head -1)"
	APK_BUILD="$("$AAPT" dump badging "$ANDROID_APK" | sed -n "s/.*versionCode='\([^']*\)'.*/\1/p" | head -1)"
	[ "$APK_BUILD" = "$BUILD_NUMBER" ] || die "the APK has build $APK_BUILD, not $BUILD_NUMBER."
	[ "$APK_VERSION" = "$VERSION" ] || die "the APK says $APK_VERSION, not $VERSION."
	say "The APK reports $APK_VERSION."
else
	die "No aapt under $SDK_DIR/build-tools; APK version verification is required."
fi

# ---------------------------------------------------------------------------
# Sign the exact final bytes before publication

node "$ROOT/scripts/verify-desktop-artifacts.mjs" "$VERSION" "$DIST"

node "$ROOT/scripts/sign-release.mjs" "$VERSION" "$DIST"
AGENT_VERSION="$(node -p 'JSON.parse(require("node:fs").readFileSync("agent/package.json", "utf8")).version')"
AGENT_ARCHIVE="$DIST/legionctl-agent-$AGENT_VERSION.tgz"

# ---------------------------------------------------------------------------
# Publish
# ---------------------------------------------------------------------------

if [ "$DRY_RUN" -eq 1 ]; then
	step "Dry run, stopping here"
	say "All platform artifacts and the signed manifest are in dist/."
	say "What a real run would do from here:"
	say "    git commit -m \"Release $VERSION\" -- $PLIST_REL $GRADLE_REL"
	say "    git tag -a v$VERSION -m \"Legion Control $VERSION\""
	say "    git push origin main && git push origin v$VERSION"
	say "    gh release create v$VERSION --title \"Legion Control $VERSION\" --notes-file <notes> dist/*.zip dist/*.apk"
	exit 0
fi

step "Committing, tagging and pushing"

git -C "$ROOT" add -- "$PLIST_REL" "$GRADLE_REL" "$DESKTOP_REL"
git -C "$ROOT" commit -m "Release $VERSION"
COMMITTED=1
git -C "$ROOT" tag -a "v$VERSION" -m "Legion Control $VERSION"
git -C "$ROOT" push origin main
git -C "$ROOT" push origin "v$VERSION"

step "Publishing the release"

URL="$(gh release create "v$VERSION" \
	--title "Legion Control $VERSION" \
	--notes-file "$NOTES" \
	"$MAC_ZIP" "$ANDROID_APK" "$LINUX_ARCHIVE" "$WINDOWS_ARCHIVE" "$AGENT_ARCHIVE" \
	"$DIST/Legion-Control-manifest.json" "$DIST/Legion-Control-manifest.json.sig")"

say ""
say "Legion Control $VERSION is out."
say "$URL"
