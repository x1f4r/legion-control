#!/usr/bin/env bash
#
# Cut a release of Legion Control: both apps, one version, one tag, one GitHub release.
#
#   ./scripts/release.sh 1.1.0                   bump, build, tag, publish
#   ./scripts/release.sh 1.1.0 --dry-run         everything except commit, tag, push and publish
#   ./scripts/release.sh 1.1.0 --notes FILE      take the release notes from a file
#   ./scripts/release.sh 1.1.0 --notes-from-stdin
#
# With none of the notes options it opens $EDITOR on a template and waits.
#
# The conventions this keeps, because the two apps read them back at runtime to find their own
# updates: the tag is vX.Y.Z, the release is named "Legion Control X.Y.Z", and exactly two assets
# are attached, Legion-Control-macos-arm64.zip and Legion-Control-android-arm64.apk. The Mac app
# picks the first asset whose name ends in .zip and the phone the first ending in .apk, so a third
# asset of either kind would be a coin toss.
#
# The two version numbers stay mirrored. There is one release, not a Mac one and an Android one, and
# a build number that means a different thing on each side is a number nobody can read. This refuses
# to run when they have drifted rather than quietly picking one.
#
# Nothing is committed, tagged or pushed until both artifacts exist and both have been opened up and
# asked what version they think they are. A tag pointing at a build that says something else is the
# one mistake here that cannot be taken back.
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
	|| die "the two apps disagree about the current version: the Mac says $OLD_VERSION and Android says $OLD_NAME. Put them back in step before releasing."
[ "$OLD_BUILD" = "$OLD_CODE" ] \
	|| die "the two apps disagree about the current build number: the Mac says $OLD_BUILD and Android says $OLD_CODE. Put them back in step before releasing."

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
		git -C "$ROOT" checkout -- "$PLIST_REL" "$GRADLE_REL" 2>/dev/null || true
		say ""
		say "The version numbers were put back to $OLD_VERSION ($OLD_BUILD)."
	fi
}
trap finish EXIT

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

say "$PLIST_REL and $GRADLE_REL now say $VERSION ($BUILD_NUMBER)."

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

rm -rf "$DIST"
mkdir -p "$DIST"

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
rm -rf "$VERIFY_DIR"
[ "$ZIPPED_VERSION" = "$VERSION" ] || die "the zipped app says $ZIPPED_VERSION, not $VERSION."
say "The Mac zip holds Legion Control.app at $ZIPPED_VERSION."

# aapt only exists when an Android SDK is installed, and the APK was just built by a script that
# needs one, so this normally runs. It is a check and not a requirement, so a missing tool says so
# and moves on rather than stopping a release over its own inability to look.
SDK_DIR="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-$HOME/Library/Android/sdk}}"
AAPT=""
for candidate in "$SDK_DIR"/build-tools/*/aapt2 "$SDK_DIR"/build-tools/*/aapt; do
	if [ -x "$candidate" ]; then AAPT="$candidate"; fi
done
if [ -n "$AAPT" ]; then
	APK_VERSION="$("$AAPT" dump badging "$ANDROID_APK" | sed -n "s/.*versionName='\([^']*\)'.*/\1/p" | head -1)"
	[ "$APK_VERSION" = "$VERSION" ] || die "the APK says $APK_VERSION, not $VERSION."
	say "The APK reports $APK_VERSION."
else
	say "No aapt under $SDK_DIR/build-tools, so the APK's version was not checked. Everything else was."
fi

# ---------------------------------------------------------------------------
# Publish
# ---------------------------------------------------------------------------

if [ "$DRY_RUN" -eq 1 ]; then
	step "Dry run, stopping here"
	say "Both artifacts are in dist/ and both say $VERSION."
	say "What a real run would do from here:"
	say "    git commit -m \"Release $VERSION\" -- $PLIST_REL $GRADLE_REL"
	say "    git tag -a v$VERSION -m \"Legion Control $VERSION\""
	say "    git push origin main && git push origin v$VERSION"
	say "    gh release create v$VERSION --title \"Legion Control $VERSION\" --notes-file <notes> dist/*.zip dist/*.apk"
	exit 0
fi

step "Committing, tagging and pushing"

git -C "$ROOT" add -- "$PLIST_REL" "$GRADLE_REL"
git -C "$ROOT" commit -m "Release $VERSION"
COMMITTED=1
git -C "$ROOT" tag -a "v$VERSION" -m "Legion Control $VERSION"
git -C "$ROOT" push origin main
git -C "$ROOT" push origin "v$VERSION"

step "Publishing the release"

URL="$(gh release create "v$VERSION" \
	--title "Legion Control $VERSION" \
	--notes-file "$NOTES" \
	"$MAC_ZIP" "$ANDROID_APK")"

say ""
say "Legion Control $VERSION is out."
say "$URL"
