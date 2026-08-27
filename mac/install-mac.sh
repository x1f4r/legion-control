#!/usr/bin/env bash
#
# Legion Control: set this Mac up. Build the app, check that it actually landed,
# then deploy the agent and schedule its update check.
#
#   ./mac/install-mac.sh
#
# This is a thin wrapper on purpose. The real work is in two places: mac/build.sh
# owns the compile and the install into /Applications, and mac/install/install-mac-agent.sh
# owns the agent tree and the launchd job. The only thing added here is the part
# that is easy to skip and annoying to miss: proving afterwards that a bundle
# really is sitting in /Applications, that it is the one this run just produced,
# and printing which version it is. A build script that exits 0 while the copy
# silently failed is exactly the failure this catches.
#
# Its own flags, which are consumed here and never reach build.sh:
#
#   --skip-updater        build the app only, leave the launchd job alone
#   --updater-only        skip the build, only deploy the agent and the job
#   --uninstall-updater   skip the build, unload the job and remove its plist
#
# Everything else is handed straight through to build.sh, so its flags all work:
#
#   ./mac/install-mac.sh                    build, sign, install, verify, schedule
#   ./mac/install-mac.sh --debug            same, debug configuration
#   ./mac/install-mac.sh --install-dir DIR  install and verify in DIR
#
# --no-install is refused rather than passed on, because it leaves the bundle in
# mac/build and there would then be nothing installed for this script to check.
#
# Override the expected bundle name if it ever changes:
#   LEGION_CONTROL_APP_NAME="Something Else" ./mac/install-mac.sh
#
set -euo pipefail

APP_NAME="${LEGION_CONTROL_APP_NAME:-Legion Control}"
INSTALL_DIR="/Applications"

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
BUILD_SH="$SCRIPT_DIR/build.sh"
AGENT_SH="$SCRIPT_DIR/install/install-mac-agent.sh"

die() { printf '\nERROR: %s\n' "$1" >&2; exit 1; }

[ -f "$BUILD_SH" ] || die "no build script at $BUILD_SH."
[ -f "$AGENT_SH" ] || die "no agent installer at $AGENT_SH."

# Split the arguments in two: the ones this script owns are consumed here, and
# everything else is forwarded to build.sh untouched. --install-dir is read but
# not consumed, so that the verification afterwards looks in the place build.sh
# will actually install to.
DO_BUILD=1
DO_UPDATER=1
UPDATER_ARGS=()
FORWARD=()

args=("$@")
i=0
while [ "$i" -lt "${#args[@]}" ]; do
  arg="${args[$i]}"
  case "$arg" in
    --skip-updater)
      DO_UPDATER=0
      ;;
    --updater-only)
      DO_BUILD=0
      ;;
    --uninstall-updater)
      DO_BUILD=0
      UPDATER_ARGS+=(--uninstall)
      ;;
    --install-dir)
      FORWARD+=("$arg")
      i=$((i + 1))
      [ "$i" -lt "${#args[@]}" ] || die "--install-dir needs a path."
      INSTALL_DIR="${args[$i]}"
      FORWARD+=("${args[$i]}")
      ;;
    --no-install)
      die "--no-install leaves the bundle in mac/build, so there is nothing for this script to verify. Run build.sh directly for that."
      ;;
    -h|--help)
      printf 'Legion Control, Mac side. Flags this script owns:\n\n'
      printf '  --skip-updater        build the app only, leave the launchd job alone\n'
      printf '  --updater-only        skip the build, only deploy the agent and the job\n'
      printf '  --uninstall-updater   skip the build, unload the job and remove its plist\n\n'
      printf 'Everything below is build.sh, and is forwarded to it.\n\n'
      exec bash "$BUILD_SH" --help
      ;;
    *)
      FORWARD+=("$arg")
      ;;
  esac
  i=$((i + 1))
done

# --skip-updater and --uninstall-updater together is a contradiction, and it is
# the kind that quietly does nothing rather than complaining.
if [ "$DO_UPDATER" -eq 0 ] && [ "$DO_BUILD" -eq 0 ]; then
  die "--skip-updater cancels out --updater-only and --uninstall-updater. Pick one."
fi

APP_PATH="$INSTALL_DIR/$APP_NAME.app"

# ---------------------------------------------------------------------------
# Agent only
# ---------------------------------------------------------------------------

# Nothing below this point applies when there is no build: hand straight over.
if [ "$DO_BUILD" -eq 0 ]; then
  if [ "${#FORWARD[@]}" -gt 0 ]; then
    die "--updater-only and --uninstall-updater take no build flags, but got: ${FORWARD[*]}"
  fi
  if [ "${#UPDATER_ARGS[@]}" -gt 0 ]; then
    exec bash "$AGENT_SH" "${UPDATER_ARGS[@]}"
  fi
  exec bash "$AGENT_SH"
fi

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

# A marker to compare bundle timestamps against afterwards. Anything older than
# this was not produced by this run.
STAMP_DIR="$(mktemp -d)"
trap 'rm -rf "$STAMP_DIR"' EXIT
STAMP="$STAMP_DIR/stamp"
touch "$STAMP"

# Back the marker off by a second. When Swift has nothing to recompile the whole
# build and install finishes inside the same second the marker was created, and
# a same second comparison is not strictly newer, so a perfectly good install
# would be reported as leftover. Losing a second of precision costs nothing here
# because a genuinely stale bundle is minutes or hours old, not milliseconds.
touch -A -01 "$STAMP" 2>/dev/null || true

printf '\n== Building\n\n' >&2

# Invoked through bash rather than executed directly so a missing execute bit on
# a fresh clone is not a failure.
if [ "${#FORWARD[@]}" -gt 0 ]; then
  bash "$BUILD_SH" "${FORWARD[@]}"
else
  bash "$BUILD_SH"
fi

# ---------------------------------------------------------------------------
# Verify
# ---------------------------------------------------------------------------

printf '\n== Verifying\n\n' >&2

if [ ! -d "$APP_PATH" ]; then
  printf 'No bundle at %s\n\n' "$APP_PATH" >&2
  printf 'App bundles in %s that were touched by this run:\n' "$INSTALL_DIR" >&2
  found="$(find "$INSTALL_DIR" -maxdepth 1 -name '*.app' -newer "$STAMP" -print 2>/dev/null || true)"
  if [ -n "$found" ]; then
    printf '%s\n' "$found" | sed 's/^/  /' >&2
    printf '\nIf one of those is the app, set LEGION_CONTROL_APP_NAME to its name.\n' >&2
  else
    printf '  (none)\n\nbuild.sh exited cleanly but installed nothing into %s.\n' "$INSTALL_DIR" >&2
  fi
  die "the app did not land in $INSTALL_DIR."
fi

BINARY_DIR="$APP_PATH/Contents/MacOS"
[ -d "$BINARY_DIR" ] || die "$APP_PATH has no Contents/MacOS, that is not a usable bundle."

EXECUTABLE="$(find "$BINARY_DIR" -maxdepth 1 -type f -perm -111 -print -quit 2>/dev/null || true)"
[ -n "$EXECUTABLE" ] || die "$BINARY_DIR contains no executable file."

INFO_PLIST="$APP_PATH/Contents/Info.plist"
[ -f "$INFO_PLIST" ] || die "$APP_PATH has no Contents/Info.plist."

plist_value() {
  /usr/libexec/PlistBuddy -c "Print :$1" "$INFO_PLIST" 2>/dev/null || printf 'unknown'
}

SHORT_VERSION="$(plist_value CFBundleShortVersionString)"
BUILD_VERSION="$(plist_value CFBundleVersion)"
BUNDLE_ID="$(plist_value CFBundleIdentifier)"

# The executable is the probe, not the .app directory. build.sh restages the
# bundle from scratch and installs the binary fresh every time, so the binary
# always carries a current timestamp after a real install, whereas the bundle
# directory's own mtime survives an rsync that found nothing to change and would
# report a perfectly good build as stale.
if [ "$EXECUTABLE" -nt "$STAMP" ]; then
  FRESHNESS='installed by this run'
else
  # The bundle is there but nothing in it was written during this run, so it is
  # left over from an earlier one. Not fatal, and worth saying out loud rather
  # than reporting a success that belongs to a previous build.
  FRESHNESS='NOT written by this run, this is the bundle that was already there'
fi

# Informational only. A locally built app being ad hoc signed is the normal
# case here, it is not a failure, it just gets stated so nobody has to wonder.
# Note that codesign only prints the Authority lines at verbose 2 and above,
# and that it writes all of this to stderr.
codesign_summary() {
  local out authority
  out="$(codesign -dv --verbose=2 "$APP_PATH" 2>&1 || true)"
  authority="$(printf '%s\n' "$out" | sed -n 's/^Authority=//p' | head -1)"
  if [ -n "$authority" ]; then
    printf '%s' "$authority"
  elif printf '%s\n' "$out" | grep -q 'Signature=adhoc'; then
    printf 'ad hoc'
  elif printf '%s\n' "$out" | grep -q 'not signed'; then
    printf 'unsigned'
  else
    printf 'unknown'
  fi
}
SIGNATURE="$(codesign_summary)"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

printf '\n== Legion Control, Mac app\n\n'
printf '  Bundle:     %s\n' "$APP_PATH"
printf '  Executable: %s\n' "${EXECUTABLE#"$APP_PATH"/}"
printf '  Identifier: %s\n' "$BUNDLE_ID"
printf '  Version:    %s (%s)\n' "$SHORT_VERSION" "$BUILD_VERSION"
printf '  Signature:  %s\n' "$SIGNATURE"
printf '  Freshness:  %s\n' "$FRESHNESS"
# "open -a" resolves by name through Launch Services, which only knows about the
# standard locations, so a bundle parked somewhere else gets opened by path.
if [ "$INSTALL_DIR" = "/Applications" ]; then
  printf '\n  Launch it with:  open -a "%s"\n\n' "$APP_NAME"
else
  printf '\n  Launch it with:  open "%s"\n\n' "$APP_PATH"
fi

# ---------------------------------------------------------------------------
# Agent and update job
# ---------------------------------------------------------------------------

# Last, not first. The app is the part someone is waiting on, and the agent
# installer refuses to run while the update job is mid cycle, which would
# otherwise mean losing a perfectly good build to a scheduling collision.
if [ "$DO_UPDATER" -eq 1 ]; then
  bash "$AGENT_SH"
else
  printf '  Agent and update job skipped (--skip-updater). Install them with:\n'
  printf '    %s --updater-only\n\n' "${BASH_SOURCE[0]}"
fi
