#!/usr/bin/env bash
#
# Legion Control: deploy the agent onto this Mac and schedule its update
# check.
#
#   ./mac/install/install-mac-agent.sh              install or refresh
#   ./mac/install/install-mac-agent.sh --uninstall  unload and remove the job
#
# Usually reached through the wrapper, which builds the app first:
#
#   ./mac/install-mac.sh                     build the app, then run this
#   ./mac/install-mac.sh --updater-only      only run this
#   ./mac/install-mac.sh --uninstall-updater only run this, with --uninstall
#
# Run this as your own user, never under sudo. It installs a launchd USER agent
# and writes into $HOME; a root run would leave root-owned files in ~ that the
# agent can then not write, and the job would end up in the wrong domain besides.
#
# Safe to re-run. It refreshes the agent tree, re-renders the job and reloads it.
#
# What the agent looks after here, and what updating it means, are the agent
# config's business rather than this script's. That matters most on a Mac,
# because an update here can be nothing like an update on a server: a desktop app
# that downloads its own build has already fetched everything, and the only step
# left is the quit. The hard question is then when quitting is safe, which is the
# one question the agent's busy rule was written to answer.
#
set -euo pipefail

# ---------------------------------------------------------------------------
# Settings
# ---------------------------------------------------------------------------

# Part of the frozen agent contract: this path is invoked unquoted, so it may
# never contain a space. Same layout as install-linux.sh and install-windows.ps1
# produce, on purpose, so that one command line works on all three machines.
BASE="$HOME/.legion-control"
AGENT_DIR="$BASE/agent"
AGENT_ENTRY="$AGENT_DIR/src/index.mjs"
LOG_FILE="$BASE/launchd.log"

LABEL="com.x1f4r.legion-control.update"
# What this job used to be called, back when it only ever looked after one
# service. Booted out and deleted below if it is still there, because two jobs
# running the same cycle a quarter hour apart is the kind of thing nobody
# notices until an app quits twice.
OLD_LABEL="com.x1f4r.legion-control.t3-update"
LAUNCH_AGENTS="$HOME/Library/LaunchAgents"
PLIST_DEST="$LAUNCH_AGENTS/$LABEL.plist"
OLD_PLIST_DEST="$LAUNCH_AGENTS/$OLD_LABEL.plist"

# gui/<uid> and not user/<uid>: the job has to be able to talk to the login
# session, because the thing it eventually acts on is a running GUI app.
DOMAIN="gui/$(id -u)"
TARGET="$DOMAIN/$LABEL"
OLD_TARGET="$DOMAIN/$OLD_LABEL"

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
TEMPLATE="$SCRIPT_DIR/$LABEL.plist"

# Where the agent source is copied from. The override exists so the tree can be
# staged from somewhere else; normally it is the agent directory of this repo.
SRC_AGENT="${LEGION_CONTROL_AGENT_SRC:-$SCRIPT_DIR/../../agent}"

MIN_NODE_MAJOR=22

SUMMARY=()
note() { SUMMARY+=("$1"); printf '  %s\n' "$1" >&2; }
step() { printf '\n== %s\n' "$1" >&2; }
die()  { printf '\nERROR: %s\n' "$1" >&2; exit 1; }

UNINSTALL=0
for arg in "$@"; do
  case "$arg" in
    --uninstall) UNINSTALL=1 ;;
    -h|--help)
      printf 'Deploy the Legion Control agent to this Mac and schedule its update check.\n\n'
      printf '  %s              install or refresh, then verify\n' "${BASH_SOURCE[0]}"
      printf '  %s --uninstall  unload the job and remove its plist\n\n' "${BASH_SOURCE[0]}"
      printf 'Environment:\n'
      printf '  LEGION_NODE_BIN             node to write into the job, when the found one is wrong\n'
      printf '  LEGION_CONTROL_AGENT_SRC    agent tree to deploy, when it is not this repo\n\n'
      exit 0
      ;;
    *) die "unknown argument: $arg. This script takes --uninstall or nothing." ;;
  esac
done

if [ "$(id -u)" -eq 0 ]; then
  die "run this as your own user without sudo. It installs a launchd USER agent and writes into \$HOME."
fi

# ---------------------------------------------------------------------------
# Is the job running right now
# ---------------------------------------------------------------------------
#
# Asked before anything is touched, by both paths. Unloading a job kills it, and
# this particular job may be halfway through swapping an app bundle or holding
# the maintenance lock. Refreshing the agent tree is no better: rm -rf on a
# directory node is currently executing out of is asking for a confusing crash.
# Waiting one cycle costs nothing, so wait.

job_pid() {
  # "pid = 1234" only appears in launchctl print while an instance is alive.
  launchctl print "$1" 2>/dev/null | sed -n 's/^[[:space:]]*pid = \([0-9][0-9]*\).*/\1/p' | head -1
}

RUNNING_LABEL="$LABEL"
RUNNING_PID="$(job_pid "$TARGET" || true)"
if [ -z "$RUNNING_PID" ]; then
  # The job under its previous name counts too. It runs the same cycle out of
  # the same tree, so refreshing that tree while it works is just as unwise.
  RUNNING_PID="$(job_pid "$OLD_TARGET" || true)"
  [ -z "$RUNNING_PID" ] || RUNNING_LABEL="$OLD_LABEL"
fi
if [ -n "$RUNNING_PID" ]; then
  die "$RUNNING_LABEL is running right now (pid $RUNNING_PID). It finishes in seconds to minutes; re-run then. Watch it with: tail -f $LOG_FILE"
fi

# ---------------------------------------------------------------------------
# Uninstall
# ---------------------------------------------------------------------------

if [ "$UNINSTALL" -eq 1 ]; then
  step "Uninstall"

  if launchctl bootout "$TARGET" 2>/dev/null; then
    note "unloaded $TARGET"
  else
    note "$TARGET was not loaded"
  fi

  if [ -f "$PLIST_DEST" ]; then
    rm -f "$PLIST_DEST"
    note "removed $PLIST_DEST"
  else
    note "no plist at $PLIST_DEST"
  fi

  # The label this job had before it was named for the machine rather than for
  # the one service it used to know about.
  if launchctl bootout "$OLD_TARGET" 2>/dev/null; then
    note "unloaded the previously named job $OLD_TARGET"
  fi
  if [ -f "$OLD_PLIST_DEST" ]; then
    rm -f "$OLD_PLIST_DEST"
    note "removed $OLD_PLIST_DEST"
  fi

  printf '\n== Legion Control, Mac update job removed\n\n'
  for line in "${SUMMARY[@]}"; do printf '  %s\n' "$line"; done
  printf '\n  The agent tree and its state were left in place, the Mac app still uses them:\n'
  printf '    %s\n' "$BASE"
  printf '\n  Remove those too, if that is what you want:\n'
  printf '    rm -rf %s\n' "$BASE"
  printf '\n  Put the job back:\n'
  printf '    %s\n\n' "$SCRIPT_DIR/install-mac-agent.sh"
  exit 0
fi

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------

step "Preflight"

case "$BASE" in
  *[[:space:]]*) die "\$HOME contains a space ($HOME). The agent contract requires space free paths." ;;
esac

[ -f "$TEMPLATE" ] || die "missing the job template at $TEMPLATE."

SRC_AGENT="$(cd -- "$SRC_AGENT" 2>/dev/null && pwd -P)" \
  || die "no agent source at ${LEGION_CONTROL_AGENT_SRC:-$SCRIPT_DIR/../../agent}."
[ -f "$SRC_AGENT/src/index.mjs" ] || die "no agent entry point at $SRC_AGENT/src/index.mjs."

# Node, found rather than assumed. /usr/bin/node does not exist on this machine,
# node is Homebrew's, and a launchd job does not get the Homebrew prefix on PATH,
# so the plist needs a real absolute path written into it at install time.
#
# The Homebrew symlink is preferred over its target on purpose. /opt/homebrew/bin/node
# points into /opt/homebrew/Cellar/node/<version>/bin/node, and baking that
# resolved path in would leave the job pointing at a directory that the next
# "brew upgrade node" deletes.
NODE_BIN=""
for candidate in \
  "${LEGION_NODE_BIN:-}" \
  "$(command -v node 2>/dev/null || true)" \
  /opt/homebrew/bin/node \
  /usr/local/bin/node \
  /usr/bin/node
do
  [ -n "$candidate" ] || continue
  case "$candidate" in /*) ;; *) continue ;; esac
  [ -x "$candidate" ] || continue
  NODE_BIN="$candidate"
  break
done
[ -n "$NODE_BIN" ] || die "could not find node. Install it, or point LEGION_NODE_BIN at the binary."

NODE_VERSION="$("$NODE_BIN" -v)"           # e.g. v26.5.0
NODE_MAJOR="${NODE_VERSION#v}"
NODE_MAJOR="${NODE_MAJOR%%.*}"
case "$NODE_MAJOR" in
  ''|*[!0-9]*) die "could not parse the node version from '$NODE_VERSION'." ;;
esac
if [ "$NODE_MAJOR" -lt "$MIN_NODE_MAJOR" ]; then
  die "node $NODE_VERSION is too old. The agent needs node >= $MIN_NODE_MAJOR for node:sqlite."
fi
NODE_DIR="$(dirname "$NODE_BIN")"
note "node $NODE_VERSION at $NODE_BIN"

# A version manager hands out paths that contain the version number and vanish
# when that version is uninstalled. Not fatal, the job would just start failing
# quietly one day, so say it out loud now.
case "$NODE_BIN" in
  */.nvm/*|*/.fnm/*|*/.asdf/*|*/.volta/*|*/n/versions/*)
    note "WARNING: that node comes from a version manager. The path disappears when that version is removed; prefer a stable one via LEGION_NODE_BIN."
    ;;
esac

# ---------------------------------------------------------------------------
# Agent tree
# ---------------------------------------------------------------------------

step "Agent"

mkdir -p "$BASE"

if [ "$SRC_AGENT" = "$AGENT_DIR" ]; then
  # Running out of the deployed copy. Nothing to copy, and wiping the
  # destination would delete the source under its own feet.
  note "agent source is already $AGENT_DIR, skipping the copy"
else
  rm -rf "$AGENT_DIR"
  mkdir -p "$AGENT_DIR"
  cp -R "$SRC_AGENT/src" "$AGENT_DIR/src"
  if [ -d "$SRC_AGENT/install" ]; then
    # Same as the other two: ship the installers along so the machine can be
    # re-provisioned from its own copy.
    cp -R "$SRC_AGENT/install" "$AGENT_DIR/install"
  fi
  note "agent copied to $AGENT_DIR"
fi

[ -f "$AGENT_ENTRY" ] || die "agent entry point missing at $AGENT_ENTRY after the copy."

# config.json and state.json are deliberately not written here, exactly as on
# the Linux side. The agent falls back to its own defaults when they are absent,
# and an installer that stamped a fresh config over the top would silently turn
# auto update back on every single time it ran.

# ---------------------------------------------------------------------------
# launchd job
# ---------------------------------------------------------------------------

step "Job"

mkdir -p "$LAUNCH_AGENTS"

TMP_PLIST="$PLIST_DEST.tmp.$$"
cleanup() { rm -f "$TMP_PLIST"; }
trap cleanup EXIT

sed \
  -e "s|@NODE@|$NODE_BIN|g" \
  -e "s|@NODE_DIR@|$NODE_DIR|g" \
  -e "s|@AGENT_DIR@|$AGENT_DIR|g" \
  -e "s|@BASE@|$BASE|g" \
  "$TEMPLATE" > "$TMP_PLIST"

plutil -lint "$TMP_PLIST" >/dev/null 2>&1 || {
  plutil -lint "$TMP_PLIST" >&2 || true
  die "the rendered plist does not parse. Check the template at $TEMPLATE."
}

# Catch a placeholder that was added to the template but never added to the sed
# list above, which would otherwise install a job pointing at a literal token.
# The check runs on the converted output rather than the file, because plutil
# drops the XML comments, and the comments are allowed to talk about the
# placeholders without that counting as a hit.
if plutil -convert xml1 -o - "$TMP_PLIST" | grep -Eq '@[A-Z_]+@'; then
  die "unsubstituted placeholder left in the rendered plist. Add the missing token to the sed list in this script."
fi

if [ -f "$PLIST_DEST" ] && cmp -s "$TMP_PLIST" "$PLIST_DEST"; then
  PLIST_CHANGED=0
  note "$PLIST_DEST unchanged"
else
  PLIST_CHANGED=1
fi
mv "$TMP_PLIST" "$PLIST_DEST"
trap - EXIT
[ "$PLIST_CHANGED" -eq 0 ] || note "$PLIST_DEST written"

# bootout first so a re-run really does load the file that was just written:
# bootstrap on an already loaded label fails, and even where it does not, launchd
# keeps serving the definition it loaded the first time. The failure is ignored
# because "was not loaded" is the normal case on a first install and is not an
# error worth stopping for.
if launchctl bootout "$TARGET" 2>/dev/null; then
  note "unloaded the previous job"
fi

# The job was renamed when it stopped being about one particular service. An
# install that left the old label loaded would run the same cycle twice on its
# own schedule, so it is booted out and its plist removed.
if launchctl bootout "$OLD_TARGET" 2>/dev/null; then
  note "unloaded the previously named job $OLD_LABEL"
fi
if [ -f "$OLD_PLIST_DEST" ]; then
  rm -f "$OLD_PLIST_DEST"
  note "removed the previously named job's plist $OLD_PLIST_DEST"
fi

# A label that was disabled once stays disabled in launchd's override database,
# across reboots and across a bootout, and a bootstrap of a disabled label loads
# a job that then never fires. Clearing it here is what makes re-running this
# script actually repair that state instead of silently reproducing it.
launchctl enable "$TARGET" 2>/dev/null || true

launchctl bootstrap "$DOMAIN" "$PLIST_DEST" \
  || die "launchctl bootstrap $DOMAIN $PLIST_DEST failed. Check the plist and run: launchctl print $DOMAIN | grep legion"
note "bootstrapped $TARGET"

# ---------------------------------------------------------------------------
# Verify
# ---------------------------------------------------------------------------

step "Verify"

if PRINTED="$(launchctl print "$TARGET" 2>&1)"; then
  note "launchd knows the job"
else
  printf '%s\n' "$PRINTED" >&2
  die "launchctl bootstrap reported success but launchctl print cannot find $TARGET."
fi

job_field() {
  printf '%s\n' "$PRINTED" | sed -n "s/^[[:space:]]*$1 = \(.*\)$/\1/p" | head -1
}
JOB_STATE="$(job_field state)"
JOB_RUNS="$(job_field runs)"
note "state=${JOB_STATE:-unknown} runs=${JOB_RUNS:-0}"

# Read the decision, never make it. "busy" only reads whatever state the
# configured busy probes look at, it can neither quit nor start anything, which
# is what makes it safe to run from an installer. Deliberately not "launchctl
# kickstart": that would run the real update cycle, and an installer has no
# business quitting an app.
step "Decision"

DECISION="$("$NODE_BIN" "$AGENT_ENTRY" busy 2>/dev/null || true)"
if [ -z "$DECISION" ]; then
  note "the agent printed nothing for 'busy', which it should never do. Run it by hand: $NODE_BIN $AGENT_ENTRY busy"
else
  printf '  %s\n' "$DECISION" >&2
  case "$DECISION" in
    *'"busy":true'*|*'"busy": true'*)
      note "something is busy, so the next cycle will defer. That is the correct answer while work is live."
      ;;
    *'"busy":false'*|*'"busy": false'*)
      note "nothing is busy, so the next cycle is free to act if an update is available."
      ;;
    *'runs on linux and windows'*)
      note "NOTE: this build of the agent does not support macOS yet, so the job will print that error every cycle until it does. The job itself is installed and correct."
      ;;
    *)
      note "could not read a busy verdict out of that. Check it by hand: $NODE_BIN $AGENT_ENTRY busy"
      ;;
  esac
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

printf '\n== Legion Control, Mac side\n\n'
for line in "${SUMMARY[@]}"; do
  printf '  %s\n' "$line"
done

printf '\n  Agent:    %s\n' "$AGENT_ENTRY"
printf '  Job:      %s\n' "$PLIST_DEST"
printf '  Cadence:  every 15 minutes, not at login\n'
printf '  Log:      %s\n' "$LOG_FILE"

printf '\n  Inspect it\n'
printf '    launchctl print %s\n' "$TARGET"
printf '    launchctl list | grep legion-control\n'
printf '    plutil -lint %s\n' "$PLIST_DEST"
printf '    tail -f %s\n' "$LOG_FILE"

printf '\n  Ask the agent, without letting it act\n'
printf '    %s %s busy\n' "$NODE_BIN" "$AGENT_ENTRY"
printf '    %s %s status\n' "$NODE_BIN" "$AGENT_ENTRY"

printf '\n  Remove it again\n'
printf '    %s --uninstall\n' "$SCRIPT_DIR/install-mac-agent.sh"
printf '  which is exactly these two lines\n'
printf '    launchctl bootout %s\n' "$TARGET"
printf '    rm %s\n' "$PLIST_DEST"
printf '\n'
