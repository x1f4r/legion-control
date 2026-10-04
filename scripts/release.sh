#!/usr/bin/env bash
#
# Cut a release of Legion Control: four clients, one version, one tag, one GitHub release.
#
#   ./scripts/release.sh 1.4.0                   bump, build, tag, publish
#   ./scripts/release.sh 1.4.0 --tag-head        release HEAD as it is, its projects already at 1.4.0
#   ./scripts/release.sh 1.4.0 --resume          finish a release that stopped after it was tagged
#   ./scripts/release.sh 1.4.0 --dry-run         everything except commit, tag, push and publish
#   ./scripts/release.sh 1.4.0 --notes FILE      take the release notes from a file
#   ./scripts/release.sh 1.4.0 --notes-from-stdin
#
# With none of the notes options it opens $EDITOR on a template and waits.
#
# A release contains four platform clients, the shared agent, and a signed manifest that names the
# commit they were built from. It is cut from main exactly as origin has it, once CI passed on that
# commit, and only for a version above every one already out. Checks, artifact version validation
# and integrity verification finish before publication. Then every asset goes up to a draft
# release, and only once all of them are there are main and the tag pushed and the draft published,
# so a failed upload leaves nothing anyone else can see. A run that stops after it tagged says so,
# and --resume picks it up where it stopped.
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
LINUX_ARM_ARCHIVE="$DIST/Legion-Control-linux-arm64.tar.gz"
WINDOWS_ARCHIVE="$DIST/Legion-Control-windows-x64.zip"
MAC_ZIP="$DIST/Legion-Control-macos-arm64.zip"
ANDROID_APK="$DIST/Legion-Control-android-arm64.apk"
# The notes outlive the run in dist/, so a run that stopped before its draft existed can be resumed
# without writing them again.
KEPT_NOTES="$DIST/release-notes.md"
PLIST_BUDDY="/usr/libexec/PlistBuddy"
RELEASE_STATE="$ROOT/scripts/release-state.mjs"

VERSION=""
DRY_RUN=0
TAG_HEAD=0
RESUME=0
NOTES_FILE=""
NOTES_FROM_STDIN=0

die() { printf '\nERROR: %s\n' "$1" >&2; exit 1; }
say() { printf '%s\n' "$1"; }
step() { printf '\n== %s\n' "$1"; }

# A real run stops at any of these. A dry run says what a real run would have stopped at and builds
# anyway, so a release can be tried before its commit is pushed or CI has finished.
REFUSALS=0
refuse() {
	[ "$DRY_RUN" -eq 1 ] || die "$1"
	printf 'A real run would stop here: %s\n' "$1" >&2
	REFUSALS=$((REFUSALS + 1))
}

# ---------------------------------------------------------------------------
# Arguments
# ---------------------------------------------------------------------------

while [ $# -gt 0 ]; do
	case "$1" in
		--dry-run) DRY_RUN=1 ;;
		--tag-head) TAG_HEAD=1 ;;
		--resume) RESUME=1 ;;
		--notes) NOTES_FILE="${2:-}"; [ -n "$NOTES_FILE" ] || die "--notes needs a file"; shift ;;
		--notes-from-stdin) NOTES_FROM_STDIN=1 ;;
		-h|--help) awk 'NR>1 && /^#/ { sub(/^# ?/, ""); print; next } NR>1 { exit }' "${BASH_SOURCE[0]}"; exit 0 ;;
		-*) die "unknown option: $1" ;;
		*) [ -z "$VERSION" ] || die "give one version, not two"; VERSION="$1" ;;
	esac
	shift
done

[ -n "$VERSION" ] || die "which version? Usage: ./scripts/release.sh X.Y.Z [--tag-head | --resume] [--dry-run]"
printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' \
	|| die "\"$VERSION\" is not an X.Y.Z version. The tag, the plist and the gradle file all have to agree on it, and they only can if it is three numbers."
TAG="v$VERSION"

if [ "$NOTES_FROM_STDIN" -eq 1 ] && [ -n "$NOTES_FILE" ]; then
	die "--notes and --notes-from-stdin are two answers to the same question."
fi
if [ "$RESUME" -eq 1 ] && { [ "$DRY_RUN" -eq 1 ] || [ "$TAG_HEAD" -eq 1 ]; }; then
	die "--resume finishes $TAG the way it was started, so it takes neither --dry-run nor --tag-head."
fi

# ---------------------------------------------------------------------------
# The tree has to be somewhere a release can come from
# ---------------------------------------------------------------------------

step "Checking the working tree"

command -v git >/dev/null 2>&1 || die "no git."
command -v node >/dev/null 2>&1 || die "no node, and every check and signature below runs on it."
[ -x "$PLIST_BUDDY" ] || die "no PlistBuddy at $PLIST_BUDDY, so the Mac version cannot be read or written."
[ -f "$PLIST" ] || die "no $PLIST_REL."
[ -f "$GRADLE" ] || die "no $GRADLE_REL."

BRANCH="$(git -C "$ROOT" rev-parse --abbrev-ref HEAD)"
require_main() {
	[ "$BRANCH" = "main" ] || die "on branch $BRANCH. A release is cut from main, because the tag has to point at what everyone else will get."
}
# --resume asks once it knows whether main is still to be pushed: a release origin already has the
# tag of is finished from the tag, wherever main is by then.
[ "$RESUME" -eq 1 ] || require_main

[ -z "$(git -C "$ROOT" status --porcelain)" ] \
	|| die "the working tree has changes in it. The release commit is meant to be the version bump and nothing else, so commit or stash what is there first."

if [ "$RESUME" -eq 0 ] && git -C "$ROOT" rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
	die "tag $TAG already exists. Versions are not reused: a phone that already downloaded it would never see the new one. If a release of it stopped half way, finish it with --resume."
fi

HAVE_GH=1
if ! command -v gh >/dev/null 2>&1; then
	[ "$DRY_RUN" -eq 1 ] || die "no gh, so the release cannot be published. Install it, or use --dry-run to build and check the artifacts without publishing them."
	refuse "no gh, so neither CI nor the releases already out were asked about."
	HAVE_GH=0
elif ! gh auth status >/dev/null 2>&1; then
	[ "$DRY_RUN" -eq 1 ] || die "gh is not signed in. Run: gh auth login"
	refuse "gh is not signed in, so neither CI nor the releases already out were asked about."
	HAVE_GH=0
fi

plist_get() { "$PLIST_BUDDY" -c "Print :$1" "$PLIST"; }
gradle_get() { sed -n -E "s/^[[:space:]]*$1 = \"?([^\"]*)\"?$/\1/p" "$GRADLE" | head -1; }

# The tag on origin, as the object it names: the tag object for an annotated tag, nothing at all
# when origin has no such tag.
remote_tag() { git -C "$ROOT" ls-remote origin "refs/tags/$TAG" | awk -v ref="refs/tags/$TAG" '$2 == ref { print $1 }'; }

# ---------------------------------------------------------------------------
# What each finished artifact thinks it is, and the manifest that signs them
# ---------------------------------------------------------------------------
#
# Not paranoia: an incremental build that reused a stale bundle, or a Gradle daemon holding an old
# manifest, both produce an artifact that is fine except for the one number the whole release turns
# on. The apps compare against it to decide whether to offer an update at all.

check_artifacts() {
	step "Checking the artifacts"

	local verify_dir zipped_plist zipped_version zipped_build
	verify_dir="$(mktemp -d -t legion-control-verify)"
	ditto -x -k "$MAC_ZIP" "$verify_dir"
	zipped_plist="$verify_dir/Legion Control.app/Contents/Info.plist"
	[ -f "$zipped_plist" ] || { rm -rf "$verify_dir"; die "the Mac zip does not hold Legion Control.app."; }
	zipped_version="$("$PLIST_BUDDY" -c "Print :CFBundleShortVersionString" "$zipped_plist")"
	zipped_build="$("$PLIST_BUDDY" -c "Print :CFBundleVersion" "$zipped_plist")"
	rm -rf "$verify_dir"
	[ "$zipped_build" = "$BUILD_NUMBER" ] || die "the zipped app has build $zipped_build, not $BUILD_NUMBER."
	[ "$zipped_version" = "$VERSION" ] || die "the zipped app says $zipped_version, not $VERSION."
	say "The Mac zip holds Legion Control.app at $zipped_version."

	# aapt only exists when an Android SDK is installed, and the APK was just built by a script that
	# needs one. Refuse publication if the packaged version cannot be verified.
	local sdk_dir aapt="" candidate apk_version apk_build
	sdk_dir="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-$HOME/Library/Android/sdk}}"
	for candidate in "$sdk_dir"/build-tools/*/aapt2 "$sdk_dir"/build-tools/*/aapt; do
		if [ -x "$candidate" ]; then aapt="$candidate"; fi
	done
	[ -n "$aapt" ] || die "No aapt under $sdk_dir/build-tools; APK version verification is required."
	apk_version="$("$aapt" dump badging "$ANDROID_APK" | sed -n "s/.*versionName='\([^']*\)'.*/\1/p" | head -1)"
	apk_build="$("$aapt" dump badging "$ANDROID_APK" | sed -n "s/.*versionCode='\([^']*\)'.*/\1/p" | head -1)"
	[ "$apk_build" = "$BUILD_NUMBER" ] || die "the APK has build $apk_build, not $BUILD_NUMBER."
	[ "$apk_version" = "$VERSION" ] || die "the APK says $apk_version, not $VERSION."
	say "The APK reports $apk_version."

	node "$ROOT/scripts/verify-desktop-artifacts.mjs" "$VERSION" "$DIST"
}

# Sign the exact final bytes before publication. A commit is named once there is one: in a dry run
# of a bump there is not, and the manifest says nothing about one rather than something untrue.
sign_release() {
	if [ -n "${1:-}" ]; then
		node "$ROOT/scripts/sign-release.mjs" "$VERSION" "$DIST" --commit "$1"
	else
		node "$ROOT/scripts/sign-release.mjs" "$VERSION" "$DIST"
	fi
}

# ---------------------------------------------------------------------------
# Publication: draft, push, publish
# ---------------------------------------------------------------------------
#
# The order is the point. Uploading half a gigabyte is the step most likely to fail, so it happens
# while the release is a draft nobody else sees and nothing has been pushed. Only once the draft
# holds every asset at its built size do main and the tag go to origin, both in one atomic push,
# and only then is the draft published, against the tag that push just put there. Each step can be
# taken again: --resume works out which of them a stopped run got through.

PUBLISHED=0
# Set once origin has the tag. From then on the version is taken whatever happens to the release:
# the only way left is to finish it.
PUSHED=0
# Set by --resume when origin has the tag and its main does not have the release commit yet,
# because the tag got there on its own rather than in the atomic push below.
MAIN_BEHIND=0
# Set by --resume when dist/ has been cleared since the draft was filled. The draft is then all
# there is of the assets, and it is published as it is.
DIST_GONE=0

# Every asset of the release: what the builds leave in dist/, then the release manifest signed over
# them, which is the one part --resume makes again.
release_assets() {
	AGENT_VERSION="$(node -p 'JSON.parse(require("node:fs").readFileSync("agent/package.json", "utf8")).version')"
	AGENT_ARCHIVE="$DIST/legionctl-agent-$AGENT_VERSION.tgz"
	BUILT=("$MAC_ZIP" "$ANDROID_APK" "$LINUX_ARCHIVE" "$LINUX_ARM_ARCHIVE" "$WINDOWS_ARCHIVE" "$AGENT_ARCHIVE"
		"$DIST/Legion-Control-agent-manifest.json" "$DIST/Legion-Control-agent-manifest.json.sig")
	ASSETS=("${BUILT[@]}" "$DIST/Legion-Control-manifest.json" "$DIST/Legion-Control-manifest.json.sig")
}

# Whether the draft holds every asset: each file in dist/ at the size it was built (assets FILE...),
# or, once dist/ no longer has the files to measure, each name uploaded in full (uploaded NAME...).
draft_holds_everything() {
	local view
	view="$(gh release view "$TAG" --json assets)" || { DRAFT_SAYS="gh could not read the draft release $TAG."; return 1; }
	DRAFT_SAYS="$(printf '%s' "$view" | node "$RELEASE_STATE" "$@")"
}

publish() {
	release_assets

	step "Putting every asset on a draft release"
	# Without dist/ there is nothing here to upload, and --resume has found every asset on the draft
	# already.
	if [ "$DIST_GONE" -eq 0 ]; then
		if [ "$STATE" = "none" ]; then
			# --target only because gh refuses a tag that exists here and not yet on origin. The tag is
			# pushed before the draft is published, and publishing checks it is there, so main's head
			# is never what the release ends up at.
			gh release create "$TAG" --draft --target main \
				--title "Legion Control $VERSION" \
				--notes-file "$NOTES" \
				"${ASSETS[@]}" >/dev/null
		fi
		if ! draft_holds_everything assets "${ASSETS[@]}"; then
			say "Uploading again, because $DRAFT_SAYS"
			gh release upload "$TAG" --clobber "${ASSETS[@]}"
			draft_holds_everything assets "${ASSETS[@]}" || die "$DRAFT_SAYS"
		fi
	fi
	say "$DRAFT_SAYS"

	step "Pushing main and $TAG"
	if [ "$PUSHED" -eq 0 ]; then
		# Atomic: origin takes both refs or neither, so it never has the release commit without its tag.
		git -C "$ROOT" push --atomic origin "refs/heads/main:refs/heads/main" "refs/tags/$TAG:refs/tags/$TAG"
		PUSHED=1
	elif [ "$MAIN_BEHIND" -eq 1 ]; then
		# The tag got to origin on its own. Main follows it now, by the one commit --resume found it
		# short of, so the bump is on main before anyone is offered the release.
		git -C "$ROOT" push origin "$RELEASE_COMMIT:refs/heads/main"
		say "origin had $TAG already, and main has $RELEASE_COMMIT now as well."
	else
		# Pushing main again would be refused once it has moved on, and there is nothing to push:
		# --resume found the release commit on origin's main.
		say "origin has $TAG already, and main has $RELEASE_COMMIT."
	fi
	[ "$(remote_tag)" = "$(git -C "$ROOT" rev-parse "refs/tags/$TAG")" ] \
		|| die "origin does not have the tag $TAG that was just pushed, so the draft was not published."

	step "Publishing the release"
	gh release edit "$TAG" --tag "$TAG" --draft=false --verify-tag >/dev/null
	PUBLISHED=1
	URL="$(gh release view "$TAG" --json url --jq .url)"

	say ""
	say "Legion Control $VERSION is out."
	say "$URL"
}

# ---------------------------------------------------------------------------
# Release notes, and what happens to them and to the bump on the way out
# ---------------------------------------------------------------------------

NOTES="$(mktemp -t legion-control-release-notes)"
cleanup_notes() { rm -f "$NOTES"; }

BUMPED=0
COMMITTED=0
TAGGED=0

finish() {
	local status=$?
	cleanup_notes
	if [ "$BUMPED" -eq 1 ] && [ "$COMMITTED" -eq 0 ]; then
		git -C "$ROOT" checkout -- "$PLIST_REL" "$GRADLE_REL" "$DESKTOP_REL" 2>/dev/null || true
		say ""
		say "The version numbers were put back to $OLD_VERSION ($OLD_BUILD)."
	fi
	if [ "$status" -ne 0 ] && [ "$TAGGED" -eq 1 ] && [ "$PUBLISHED" -eq 0 ]; then
		if [ "$PUSHED" -eq 1 ]; then
			printf '\n%s is on origin and not published yet. Once what stopped it is put right, finish it with:\n' "$TAG" >&2
		else
			printf '\n%s is tagged here and not out yet. Once what stopped it is put right, finish it with:\n' "$TAG" >&2
		fi
		printf '    ./scripts/release.sh %s --resume\n' "$VERSION" >&2
	fi
}
trap finish EXIT

# ---------------------------------------------------------------------------
# --resume: finish a release that was tagged and stopped
# ---------------------------------------------------------------------------
#
# Nothing is built again. The release is the tag, the commit it is at and the artifacts in dist/;
# this checks they still belong together, signs the manifest again (the same bytes: Ed25519 is
# deterministic and so is the manifest), and takes whichever of draft, push and publish is left.
#
# Origin is asked first. Until it has the tag, nothing of the release is public, a release that
# cannot be finished can still be cut again, and one that can is held to the same checks against CI
# and the releases already out as a run that starts from scratch. Once it has it, the version is
# taken and the release is only ever finished: from the tag, whatever main has done since, checked
# out on its own if main has moved on here as well. Origin's main has to have the release commit
# all the same, since the bump is in it, and a tag that got to origin without it has main pushed
# after it. And once the draft holds every asset, it stands in for a dist/ that has been cleared
# since.

if [ "$RESUME" -eq 1 ]; then
	step "Picking up $TAG"

	git -C "$ROOT" rev-parse -q --verify "refs/tags/$TAG" >/dev/null \
		|| die "there is no tag $TAG here, so there is nothing to resume. A run that stops before it tags puts the version numbers back; run it again without --resume."
	RELEASE_COMMIT="$(git -C "$ROOT" rev-parse "refs/tags/$TAG^{commit}")"

	ORIGIN_TAG="$(remote_tag)" || die "cannot ask origin whether it has $TAG."
	if [ -n "$ORIGIN_TAG" ] && [ "$ORIGIN_TAG" != "$(git -C "$ROOT" rev-parse "refs/tags/$TAG")" ]; then
		die "origin has a tag $TAG that is not the one here. Find out where it came from before anything is published."
	fi
	[ -z "$ORIGIN_TAG" ] || PUSHED=1

	if [ "$RELEASE_COMMIT" != "$(git -C "$ROOT" rev-parse HEAD)" ]; then
		[ "$PUSHED" -eq 0 ] \
			|| die "HEAD is not $RELEASE_COMMIT, the commit $TAG is at. origin has $TAG already, so the version is taken and the release can only be finished, from the tag: git checkout $TAG, then run this again."
		die "HEAD is not $RELEASE_COMMIT, the commit $TAG is at. Go back to it, or delete the tag (and its draft, if there is one) and release again."
	fi
	[ "$PUSHED" -eq 1 ] || require_main

	VERSION_NOW="$(plist_get CFBundleShortVersionString)"
	BUILD_NUMBER="$(plist_get CFBundleVersion)"
	[ "$VERSION_NOW" = "$VERSION" ] || die "$TAG is at a commit whose projects say $VERSION_NOW, not $VERSION."
	node "$ROOT/scripts/check-versions.mjs"

	git -C "$ROOT" fetch --quiet --no-tags origin main || die "cannot fetch main from origin."
	if [ "$PUSHED" -eq 0 ]; then
		# What is left to push has to go on top of origin's main as it is now.
		git -C "$ROOT" merge-base --is-ancestor origin/main HEAD \
			|| die "origin/main has moved on to commits $TAG does not have, so the release commit cannot be pushed as it is. Delete the draft and the tag, update main and release again."
		AHEAD="$(git -C "$ROOT" rev-list --count origin/main..HEAD)"
		[ "$AHEAD" -le 1 ] || die "HEAD is $AHEAD commits ahead of origin/main. A release pushes one commit at most, its version bump."
	elif ! git -C "$ROOT" merge-base --is-ancestor "$RELEASE_COMMIT" origin/main; then
		# This script pushes main and the tag in one atomic push, so the tag got to origin some other
		# way, on its own. Main moving on since is no concern of this release, but main without the
		# release commit is: the bump is in it, and without it the next release from main would bump
		# to this one's build number again and be refused for it.
		git -C "$ROOT" merge-base --is-ancestor origin/main "$RELEASE_COMMIT" \
			|| die "origin has $TAG, but its main does not have $RELEASE_COMMIT, the commit $TAG is at, and has moved on to commits $TAG does not have. Merge the tag into origin's main and push that (git checkout --detach origin/main, git merge $TAG, git push origin HEAD:refs/heads/main), then run this again from the tag: git checkout $TAG."
		BEHIND="$(git -C "$ROOT" rev-list --count "origin/main..$RELEASE_COMMIT")"
		[ "$BEHIND" -le 1 ] \
			|| die "origin has $TAG, but its main is $BEHIND commits behind $RELEASE_COMMIT, the commit $TAG is at, and a release pushes one commit at most, its version bump. Find out where the others came from; if main is to have them all: git push origin $RELEASE_COMMIT:refs/heads/main, then run this again."
		say "origin has $TAG, and its main is one commit short of it: main follows once the draft holds every asset."
		MAIN_BEHIND=1
	fi

	RELEASES="$(gh release list --limit 1000 --json tagName,isDraft,isPrerelease)" || die "gh could not list the releases."
	STATE="$(printf '%s' "$RELEASES" | node "$RELEASE_STATE" state "$VERSION")"
	if [ "$STATE" = "published" ]; then
		say "$TAG is already out: $(gh release view "$TAG" --json url --jq .url)"
		PUBLISHED=1
		exit 0
	fi
	if [ "$STATE" = "draft" ]; then
		say "$TAG is at $RELEASE_COMMIT, and GitHub has a draft of it."
	else
		say "$TAG is at $RELEASE_COMMIT, and GitHub has no release of it yet."
	fi

	# Until origin has the tag, the release is held to what a real run is held to before it builds:
	# CI passed on origin's main, the commit it was cut from, the one commit on top of that, if there
	# is one, is the version bump and nothing else, and the version is above every one already out.
	# The run that tagged it checked all of that, but a tag made by hand over what a dry run left in
	# dist/ was never checked, and a release cut since may have gone past this one.
	if [ "$PUSHED" -eq 0 ]; then
		CUT_FROM="$(git -C "$ROOT" rev-parse origin/main)"
		if [ "$AHEAD" -eq 1 ]; then
			while IFS= read -r changed; do
				case "$changed" in
					"$PLIST_REL"|"$GRADLE_REL"|"$DESKTOP_REL") ;;
					*) die "$RELEASE_COMMIT, the commit $TAG is at, changes $changed, and a release adds nothing to the commit CI checked but its version bump. Nothing of $TAG is public yet: delete the tag (and its draft, if there is one) and release again." ;;
				esac
			done < <(git -C "$ROOT" diff --name-only "$CUT_FROM" HEAD)
		fi
		RUNS="$(gh run list --commit "$CUT_FROM" --limit 100 --json workflowName,headSha,status,conclusion,createdAt)" \
			|| die "gh could not list the CI runs of $CUT_FROM."
		CI="$(printf '%s' "$RUNS" | node "$RELEASE_STATE" ci "$CUT_FROM")" || die "$CI"
		say "$CI"
		REMOTE_TAGS="$(git -C "$ROOT" ls-remote --tags origin)" || die "cannot list the tags on origin."
		NEWEST="$(printf '%s' "$RELEASES" | node "$RELEASE_STATE" newer "$VERSION" "$REMOTE_TAGS")" \
			|| die "$NEWEST Nothing of $TAG is public yet: delete the tag (and its draft, if there is one) and release a higher version."
		say "The newest version out is ${NEWEST:-none}."
	fi

	# The artifacts are in dist/ until something clears it: another run of this script, a dry run of
	# another version included, or a git clean. Asked before the notes, which would be no use then.
	release_assets
	MISSING=()
	for asset in "${BUILT[@]}"; do
		[ -s "$asset" ] || MISSING+=("$(basename "$asset")")
	done
	if [ "${#MISSING[@]}" -gt 0 ]; then
		GONE="dist/ no longer holds ${MISSING[*]}, built for $TAG, and --resume builds nothing."
		if [ "$PUSHED" -eq 1 ]; then
			# The version is taken, so what is gone has to come from the draft, and it can once the
			# draft holds every asset: each was uploaded from dist/ after it was checked and signed for
			# the tag, the signed manifest with them, so all that is left is to publish it. Rebuilding
			# would only put new bytes in place of ones that were checked.
			if [ "$STATE" = "none" ]; then
				DRAFT_SAYS="GitHub has no draft of it to publish instead."
			elif draft_holds_everything uploaded "${ASSETS[@]##*/}"; then
				DIST_GONE=1
			fi
			[ "$DIST_GONE" -eq 1 ] \
				|| die "$GONE origin has $TAG already, so the version cannot be cut again, and $DRAFT_SAYS Build them at $TAG by hand into dist/, the way the Build section of scripts/release.sh does, then run this again."
			say "$GONE The draft holds every asset, so it is published as it is."
		else
			# Nothing of it is public, so it is cut again from where it started: the tag goes, the
			# draft if there is one, and the bump commit if the run made one, which is the one commit
			# HEAD can be ahead of origin/main by here.
			AGAIN="git tag -d $TAG"
			[ "$STATE" = "none" ] || AGAIN="$AGAIN, gh release delete $TAG"
			if [ "$AHEAD" -eq 1 ]; then
				AGAIN="$AGAIN, git reset --hard origin/main to drop the bump commit, then ./scripts/release.sh $VERSION"
			else
				AGAIN="$AGAIN, then ./scripts/release.sh $VERSION --tag-head"
			fi
			die "$GONE Nothing of $TAG is public yet, so cut it again: $AGAIN."
		fi
	fi

	if [ "$STATE" = "none" ]; then
		if [ -n "$NOTES_FILE" ]; then
			[ -f "$NOTES_FILE" ] || die "no notes file at $NOTES_FILE."
			cat "$NOTES_FILE" > "$NOTES"
		elif [ "$NOTES_FROM_STDIN" -eq 1 ]; then
			cat > "$NOTES"
		elif [ -f "$KEPT_NOTES" ]; then
			cat "$KEPT_NOTES" > "$NOTES"
		else
			die "the run that tagged $TAG left no notes in ${KEPT_NOTES#"$ROOT"/}. Give them again with --notes FILE."
		fi
		grep -q '[^[:space:]]' "$NOTES" || die "the release notes are empty."
	fi

	if [ "$DIST_GONE" -eq 0 ]; then
		check_artifacts
		sign_release "$RELEASE_COMMIT"
	fi
	# From here on a failure is one more --resume can get past; the ones above are not.
	TAGGED=1
	publish
	exit 0
fi

# ---------------------------------------------------------------------------
# Where the numbers are now
# ---------------------------------------------------------------------------

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

if [ "$TAG_HEAD" -eq 1 ]; then
	# The versions were set in a commit that was reviewed and went through CI like any other, and
	# that commit is what gets tagged. Nothing is bumped and nothing is committed.
	[ "$OLD_VERSION" = "$VERSION" ] \
		|| die "--tag-head releases HEAD as it is, and its projects say $OLD_VERSION, not $VERSION. Set the versions in a commit of their own first, or leave --tag-head out and have them bumped here."
	BUILD_NUMBER="$OLD_BUILD"
	say "Releasing HEAD as it is: $VERSION ($BUILD_NUMBER)."
else
	[ "$OLD_VERSION" != "$VERSION" ] \
		|| die "the projects already say $VERSION. To release HEAD as it is, without a commit that bumps nothing, use --tag-head."
	BUILD_NUMBER=$((OLD_BUILD + 1))
	say "Currently $OLD_VERSION ($OLD_BUILD), releasing $VERSION ($BUILD_NUMBER)."
fi
if [ "$DRY_RUN" -eq 1 ]; then
	say "Dry run: the artifacts are built and checked, then the version numbers are put back and"
	say "nothing is committed, tagged, pushed or published."
fi

# ---------------------------------------------------------------------------
# Origin and GitHub have to agree that this is the next release
# ---------------------------------------------------------------------------
#
# Asked before the notes and the build, so a release that cannot go out stops while that is free.

step "Checking against origin and GitHub"

HEAD_SHA="$(git -C "$ROOT" rev-parse HEAD)"
REMOTE_TAGS=""
PREVIOUS=""
PREVIOUS_BUILD=""
if git -C "$ROOT" fetch --quiet --no-tags origin main; then
	COUNTS="$(git -C "$ROOT" rev-list --left-right --count HEAD...origin/main)"
	AHEAD="${COUNTS%%[[:space:]]*}"
	BEHIND="${COUNTS##*[[:space:]]}"
	if [ "$AHEAD" -ne 0 ] || [ "$BEHIND" -ne 0 ]; then
		refuse "HEAD is $AHEAD commit(s) ahead of origin/main and $BEHIND behind it. A release is cut from the commit origin has and CI checked, so push or pull until the two are the same."
	fi
	REMOTE_TAGS="$(git -C "$ROOT" ls-remote --tags origin)" || die "cannot list the tags on origin."
	if printf '%s\n' "$REMOTE_TAGS" | cut -f2 | grep -qxF "refs/tags/$TAG"; then
		refuse "tag $TAG is already on origin. Versions are not reused."
	fi
else
	refuse "cannot fetch main from origin, so there is no telling whether HEAD is the commit origin has."
fi

if [ "$HAVE_GH" -eq 1 ]; then
	if RUNS="$(gh run list --commit "$HEAD_SHA" --limit 100 --json workflowName,headSha,status,conclusion,createdAt)"; then
		if CI="$(printf '%s' "$RUNS" | node "$RELEASE_STATE" ci "$HEAD_SHA")"; then say "$CI"; else refuse "$CI"; fi
	else
		refuse "gh could not list the CI runs of $HEAD_SHA."
	fi

	if RELEASES="$(gh release list --limit 1000 --json tagName,isDraft,isPrerelease)"; then
		case "$(printf '%s' "$RELEASES" | node "$RELEASE_STATE" state "$VERSION")" in
			draft) refuse "GitHub has a draft release $TAG, left by a run that stopped. Finish that run with --resume, or delete the draft." ;;
			published) refuse "$TAG is already released." ;;
		esac
		if PREVIOUS="$(printf '%s' "$RELEASES" | node "$RELEASE_STATE" newer "$VERSION" "$REMOTE_TAGS")"; then
			say "The newest version out is ${PREVIOUS:-none}."
		else
			refuse "$PREVIOUS"
			PREVIOUS=""
		fi
	else
		refuse "gh could not list the releases already out."
	fi
fi

# A build number above the one before it keeps the builds in the order the releases were cut,
# which matters most when the number was set by hand in a --tag-head commit.
if [ -n "$PREVIOUS" ] && [ -n "$REMOTE_TAGS" ]; then
	if git -C "$ROOT" fetch --quiet --no-tags origin "refs/tags/v$PREVIOUS" 2>/dev/null; then
		PREVIOUS_BUILD="$(git -C "$ROOT" show "FETCH_HEAD:$GRADLE_REL" 2>/dev/null | sed -n -E 's/^[[:space:]]*versionCode = ([0-9]+)[[:space:]]*$/\1/p' | head -1 || true)"
	fi
	if [ -z "$PREVIOUS_BUILD" ]; then
		refuse "cannot read the build number v$PREVIOUS went out with, so there is no telling whether $BUILD_NUMBER comes after it."
	elif [ "$BUILD_NUMBER" -le "$PREVIOUS_BUILD" ]; then
		refuse "build $BUILD_NUMBER is not above $PREVIOUS_BUILD, the build v$PREVIOUS went out with. Every release has a higher build number than the one before it."
	fi
fi

# ---------------------------------------------------------------------------
# Release notes, asked for before anything is built
# ---------------------------------------------------------------------------
#
# The editor comes first on purpose. Backing out of a release is free while it is still a blank
# template and expensive once two toolchains have run.

step "Release notes"

if [ -n "$NOTES_FILE" ]; then
	[ -f "$NOTES_FILE" ] || die "no notes file at $NOTES_FILE."
	cat "$NOTES_FILE" > "$NOTES"
elif [ "$NOTES_FROM_STDIN" -eq 1 ]; then
	cat > "$NOTES"
else
	SINCE="${PREVIOUS:-$OLD_VERSION}"
	cat > "$NOTES" <<-EOF
		Say what changed, in prose, the way the last seven releases do. The first paragraph is
		the one both apps show in their update row, so it is the sentence that has to stand on
		its own.

		# Lines starting with a # are dropped. An empty file cancels the release.
		# Since $SINCE:
		$(git -C "$ROOT" log --pretty='#   %s' "v$SINCE..HEAD" 2>/dev/null || true)
	EOF
	"${EDITOR:-vi}" "$NOTES" || die "the editor exited without saving."
	# Only the template's own lines are dropped, and only here. A file handed in with --notes is
	# taken exactly as written: a leading # there is a Markdown heading, not a comment.
	sed -i '' -e '/^#/d' "$NOTES"
fi

grep -q '[^[:space:]]' "$NOTES" \
	|| die "the release notes are empty, so nothing was released."

say "Notes:"
sed -e 's/^/    /' "$NOTES"

# ---------------------------------------------------------------------------
# The bump, which is undone again unless it makes it all the way to a commit
# ---------------------------------------------------------------------------

step "Running the complete verification suite"
"$ROOT/scripts/check.sh" all

if [ "$TAG_HEAD" -eq 1 ]; then
	step "Checking that every copy of the version says $VERSION ($BUILD_NUMBER)"
	node "$ROOT/scripts/check-versions.mjs"
else
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
	# The lines above read back the plist and the Gradle file. This reads every copy the way check.sh
	# does, the desktop project and the agent version included.
	node "$ROOT/scripts/check-versions.mjs"
	say "All client projects now say $VERSION ($BUILD_NUMBER)."
fi

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

# Everything in dist/ is built by this run. A leftover from an earlier one, the agent tarball of
# the previous version above all, would otherwise go out with this release: the desktop project
# embeds every legionctl-agent-*.tgz it finds there. dist/ is gitignored and holds nothing that
# cannot be built again.
rm -rf "$DIST"
mkdir -p "$DIST"
cp "$NOTES" "$KEPT_NOTES"

step "Packaging and signing the bundled control agent"
node "$ROOT/scripts/package-agent.mjs"

# Each client build is told to require the agent bundle just packaged. Without the flag they build
# without one and only say so, which is right for a build at a desk and wrong for a release that
# would then go out unable to install the agent it names.

step "Building the Mac app"

"$ROOT/mac/build.sh" --no-install --require-agent
MAC_APP="$ROOT/mac/build/Legion Control.app"
[ -d "$MAC_APP" ] || die "mac/build.sh left no bundle at $MAC_APP."

# ditto -c -k --keepParent, and nothing else: it is the only archiver on the system that keeps the
# resource forks, the symlinks and the code signature a bundle needs to still be a signed bundle on
# the other side. A zip made any other way arrives as an app that will not launch.
ditto -c -k --keepParent "$MAC_APP" "$MAC_ZIP"
say "Wrote $(basename "$MAC_ZIP") ($(du -h "$MAC_ZIP" | cut -f1))"

step "Building the Android app"

"$ROOT/android/build-apk.sh" --require-agent
BUILT_APK="$ROOT/android/app/build/outputs/apk/release/app-release.apk"
[ -f "$BUILT_APK" ] || die "android/build-apk.sh left no APK at $BUILT_APK."
cp "$BUILT_APK" "$ANDROID_APK"
say "Wrote $(basename "$ANDROID_APK") ($(du -h "$ANDROID_APK" | cut -f1))"

step "Building Linux and Windows desktop apps"
"$ROOT/desktop/build.sh" --require-agent
[ -f "$LINUX_ARCHIVE" ] || die "desktop/build.sh left no Linux archive."
[ -f "$LINUX_ARM_ARCHIVE" ] || die "desktop/build.sh left no Linux ARM64 archive."
[ -f "$WINDOWS_ARCHIVE" ] || die "desktop/build.sh left no Windows archive."

check_artifacts

# ---------------------------------------------------------------------------
# A dry run stops here
# ---------------------------------------------------------------------------

if [ "$DRY_RUN" -eq 1 ]; then
	if [ "$TAG_HEAD" -eq 1 ]; then sign_release "$HEAD_SHA"; else sign_release; fi
	step "Dry run, stopping here"
	say "Every artifact and a signed manifest are in dist/, to look at: a real run builds them again."
	if [ "$TAG_HEAD" -eq 0 ]; then
		say "They were built from a bump that is put back now, so the manifest names no commit."
	fi
	say "From here a real run would:"
	if [ "$TAG_HEAD" -eq 1 ]; then
		say "    tag HEAD as $TAG and sign the manifest naming it"
	else
		say "    commit the bump as \"Release $VERSION\", tag it $TAG and sign the manifest naming it"
	fi
	say "    upload all ten assets to a draft release $TAG"
	say "    push main and $TAG to origin together, then publish the draft"
	if [ "$REFUSALS" -gt 0 ]; then
		say "It would not get this far, though: it stops at what is marked \"A real run would stop here\" above."
	else
		say "To do that, run the same command again without --dry-run."
	fi
	exit 0
fi

# ---------------------------------------------------------------------------
# Commit, tag and sign, all of it here and nothing pushed yet
# ---------------------------------------------------------------------------

if [ "$TAG_HEAD" -eq 0 ]; then
	step "Committing the bump"
	git -C "$ROOT" add -- "$PLIST_REL" "$GRADLE_REL" "$DESKTOP_REL"
	git -C "$ROOT" commit -m "Release $VERSION"
	COMMITTED=1
fi

step "Tagging $TAG"
git -C "$ROOT" tag -a "$TAG" -m "Legion Control $VERSION"
TAGGED=1
RELEASE_COMMIT="$(git -C "$ROOT" rev-parse HEAD)"

step "Signing the manifest for $RELEASE_COMMIT"
sign_release "$RELEASE_COMMIT"

STATE="none"
publish
