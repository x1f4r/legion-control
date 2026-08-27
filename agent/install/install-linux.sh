#!/usr/bin/env bash
#
# Legion Control: install the agent and its update timer on a Linux system.
#
# Run it as your user, NOT under sudo. Everything it touches lives in the home
# directory or in the systemd user manager, and a root run would create
# root-owned files in ~ that the agent then cannot write.
#
#   ssh <machine> 'bash ~/legion-control/agent/install/install-linux.sh'
#
# Safe to re-run. It reinstalls the agent tree, re-renders the units and only
# restarts a service when its unit definition actually changed.
#
# The agent alone looks after nothing in particular: what it manages comes from
# ~/.legion-control/config.json, which this script never writes. Pass --with-t3
# to also install the reference example, a T3 Code server as a systemd user unit
# that the legacy configuration (no "services" key at all) already describes.
#
# systemd user linger has to be on for this account, so the units keep running
# after logout and come up on boot without anyone signing in:
#   sudo loginctl enable-linger "$(id -un)"
#
set -euo pipefail

# ---------------------------------------------------------------------------
# Settings
# ---------------------------------------------------------------------------

# The install layout is part of the frozen agent contract: the controller apps
# invoke these paths over SSH unquoted, so none of them may ever contain a space.
BASE="$HOME/.legion-control"
AGENT_DIR="$BASE/agent"
NPM_PREFIX="${LEGION_NPM_PREFIX:-$HOME/.npm-global}"
NPM_BIN="$NPM_PREFIX/bin"
UNIT_DIR="$HOME/.config/systemd/user"

# The reference example, only used with --with-t3.
PORT="${LEGION_T3_PORT:-3773}"
CHANNEL="nightly"
# Packages in the T3 dependency tree that legitimately need to run install
# scripts. The agent needs the same list for its own updates and reads it from
# config.json; this copy only covers the very first install below.
ALLOW_SCRIPTS="node-pty,msgpackr-extract"

WITH_T3="${LEGION_WITH_T3:-0}"

# The update units used to be named after the one service they drove. Anything
# still carrying the old names is retired below.
OLD_UNITS=(legion-t3-update.timer legion-t3-update.service)

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
SRC_AGENT="$(cd -- "$SCRIPT_DIR/.." && pwd -P)"

MIN_NODE_MAJOR=22

# Collected for the summary at the end.
SUMMARY=()
note() { SUMMARY+=("$1"); printf '  %s\n' "$1" >&2; }
step() { printf '\n== %s\n' "$1" >&2; }
die()  { printf '\nERROR: %s\n' "$1" >&2; exit 1; }

for arg in "$@"; do
  case "$arg" in
    --with-t3) WITH_T3=1 ;;
    -h|--help)
      printf 'usage: install-linux.sh [--with-t3]\n\n' >&2
      printf '  --with-t3   also install the reference example service (T3 Code)\n' >&2
      exit 0
      ;;
    *) die "unknown argument: $arg" ;;
  esac
done

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------

step "Preflight"

if [ "$(id -u)" -eq 0 ]; then
  die "run this as your own user without sudo. It installs systemd USER units and writes into \$HOME."
fi

case "$BASE" in
  *[[:space:]]*) die "\$HOME contains a space ($HOME). The agent contract requires space free paths." ;;
esac

# The units hardcode an absolute node path, so prefer the system one and fall
# back to whatever is on PATH only if it is missing.
if [ -x /usr/bin/node ]; then
  NODE_BIN=/usr/bin/node
else
  NODE_BIN="$(command -v node || true)"
  [ -n "$NODE_BIN" ] || die "node is not on PATH and /usr/bin/node does not exist."
fi

NODE_VERSION="$("$NODE_BIN" -v)"           # e.g. v26.4.0
NODE_MAJOR="${NODE_VERSION#v}"
NODE_MAJOR="${NODE_MAJOR%%.*}"
case "$NODE_MAJOR" in
  ''|*[!0-9]*) die "could not parse the node version from '$NODE_VERSION'." ;;
esac
if [ "$NODE_MAJOR" -lt "$MIN_NODE_MAJOR" ]; then
  die "node $NODE_VERSION is too old. The agent needs node >= $MIN_NODE_MAJOR for node:sqlite."
fi
note "node $NODE_VERSION at $NODE_BIN"

if command -v npm >/dev/null 2>&1; then
  note "npm $(npm --version) with prefix $NPM_PREFIX"
elif [ "$WITH_T3" -eq 1 ]; then
  die "npm is not on PATH, and --with-t3 needs it. Install nodejs and npm first."
else
  note "npm is not on PATH; that only matters for services installed with npm"
fi

[ -f "$SRC_AGENT/src/index.mjs" ] || die "no agent source at $SRC_AGENT/src/index.mjs. Copy the repo across first."

if ! systemctl --user show-environment >/dev/null 2>&1; then
  die "no systemd user manager for this session. Check 'loginctl show-user $(id -un) -p Linger'."
fi

# ---------------------------------------------------------------------------
# The reference example service
# ---------------------------------------------------------------------------

if [ "$WITH_T3" -eq 1 ]; then
  step "T3 Code (reference example)"

  # Read the version straight out of the installed package rather than running
  # "t3 --version", which is slow and would fail confusingly when nothing is
  # installed at all.
  installed_t3_version() {
    local pkg="$NPM_PREFIX/lib/node_modules/t3/package.json"
    [ -f "$pkg" ] || return 1
    "$NODE_BIN" -e 'process.stdout.write(String(require(process.argv[1]).version || ""))' "$pkg"
  }

  mkdir -p "$NPM_PREFIX/lib" "$NPM_BIN"

  if T3_VERSION="$(installed_t3_version)" && [ -n "$T3_VERSION" ]; then
    note "T3 already installed: $T3_VERSION (left alone, the agent owns updates from here)"
  else
    note "T3 not installed, pulling the current $CHANNEL"
    # --allow-scripts is not optional here. npm 12 blocks package install scripts
    # by default, and T3 depends on node-pty, which ships no linux-x64 prebuild
    # and so has to compile pty.node from source in its install script. Without
    # this the install "succeeds", then the server dies on every start with
    # NodePtyModuleLoadError and systemd restart-loops it forever.
    # An explicit allowlist rather than a blanket allow: these two are the only
    # packages in the tree that need to build, and letting arbitrary dependencies
    # run postinstall scripts is exactly the supply-chain risk npm 12 added this
    # default to avoid. If a future release adds another native dependency the
    # update will fail its health check and roll back, which is the safe
    # direction.
    npm install --global --prefix "$NPM_PREFIX" --no-audit --no-fund \
      --allow-scripts="$ALLOW_SCRIPTS" "t3@$CHANNEL" >&2
    T3_VERSION="$(installed_t3_version)" || die "npm reported success but $NPM_PREFIX/lib/node_modules/t3 has no package.json."
    note "T3 installed: $T3_VERSION"
  fi

  [ -x "$NPM_BIN/t3" ] || die "no t3 launcher at $NPM_BIN/t3 after install. Check the npm prefix."
else
  note "skipping the reference example service; pass --with-t3 to install it too"
fi

# ---------------------------------------------------------------------------
# Agent tree
# ---------------------------------------------------------------------------

step "Agent"

mkdir -p "$BASE"

if [ "$SRC_AGENT" = "$AGENT_DIR" ]; then
  # Running the installer out of the deployed copy. Nothing to copy, and
  # wiping the destination would delete the script under its own feet.
  note "agent source is already $AGENT_DIR, skipping the copy"
else
  rm -rf "$AGENT_DIR"
  mkdir -p "$AGENT_DIR"
  cp -R "$SRC_AGENT/src" "$AGENT_DIR/src"
  if [ -d "$SRC_AGENT/install" ]; then
    # Ship the installer along so the box can be re-provisioned from itself.
    cp -R "$SRC_AGENT/install" "$AGENT_DIR/install"
  fi
  note "agent copied to $AGENT_DIR"
fi

AGENT_ENTRY="$AGENT_DIR/src/index.mjs"
[ -f "$AGENT_ENTRY" ] || die "agent entry point missing at $AGENT_ENTRY."

# config.json and state.json are deliberately NOT written here. The agent falls
# back to its own defaults when they are absent, and an installer that stamped a
# fresh config over the top would silently flip auto update back on every time
# it ran.
if [ -f "$BASE/config.json" ]; then
  note "config.json is already there and was left untouched"
else
  note "no config.json; the agent falls back to its defaults until you write one"
fi

# ---------------------------------------------------------------------------
# systemd user units
# ---------------------------------------------------------------------------

step "Units"

mkdir -p "$UNIT_DIR"

UNIT_TEMPLATE_DIR="$SCRIPT_DIR"
T3_UNIT_CHANGED=0

render_unit() {
  local name="$1"
  local src="$UNIT_TEMPLATE_DIR/$name"
  local dest="$UNIT_DIR/$name"
  local tmp="$dest.tmp.$$"

  [ -f "$src" ] || die "missing unit template $src."

  sed \
    -e "s|@NPM_BIN@|$NPM_BIN|g" \
    -e "s|@NODE@|$NODE_BIN|g" \
    -e "s|@AGENT_DIR@|$AGENT_DIR|g" \
    -e "s|@PORT@|$PORT|g" \
    -e "s|@UID@|$(id -u)|g" \
    "$src" > "$tmp"

  # Catch a placeholder that was added to a template but never added to the sed
  # list above, which would otherwise install a unit with a literal token in it.
  # Comment lines are skipped: the templates talk about placeholders in prose.
  if grep -v '^[[:space:]]*#' "$tmp" | grep -Eq '@[A-Z_]+@'; then
    rm -f "$tmp"
    die "unsubstituted placeholder left in $name. Add the missing token to render_unit."
  fi

  if [ -f "$dest" ] && cmp -s "$tmp" "$dest"; then
    rm -f "$tmp"
    note "$name unchanged"
    return 0
  fi

  mv "$tmp" "$dest"
  if [ "$name" = "t3-code.service" ]; then
    T3_UNIT_CHANGED=1
  fi
  note "$name written"
  return 0
}

if [ "$WITH_T3" -eq 1 ]; then
  render_unit t3-code.service
fi
render_unit legion-control-update.service
render_unit legion-control-update.timer

systemctl --user daemon-reload

# Retire the units this pair used to be called. Left enabled they would keep
# firing an update cycle from a path that no longer exists.
for old in "${OLD_UNITS[@]}"; do
  if [ -f "$UNIT_DIR/$old" ] || systemctl --user cat "$old" >/dev/null 2>&1; then
    systemctl --user disable --now "$old" >/dev/null 2>&1 || true
    rm -f "$UNIT_DIR/$old"
    note "retired $old (superseded by ${old/legion-t3-update/legion-control-update})"
  fi
done
systemctl --user daemon-reload

# legion-control-update.service is intentionally not enabled. It is a oneshot
# and the timer is what pulls it in. Enabling it would only make it run once at
# login.
if [ "$WITH_T3" -eq 1 ]; then
  systemctl --user enable --now t3-code.service >&2
  note "t3-code.service enabled"
fi
systemctl --user enable --now legion-control-update.timer >&2
note "legion-control-update.timer enabled"

# ---------------------------------------------------------------------------
# Restart only when the definition really moved
# ---------------------------------------------------------------------------

# "enable --now" starts a stopped unit but leaves a running one on the old
# definition, so a changed unit needs an explicit restart. Ask the agent first:
# restarting on top of live work throws it away, and this script is not urgent
# enough to justify that.
#
# Fail closed, the same way the agent's own busy rule does: an answer we cannot
# read counts as busy. A restart on top of live work throws it away, while a
# needless deferral costs one manual command later.
agent_is_busy() {
  local out
  out="$("$NODE_BIN" "$AGENT_ENTRY" busy 2>/dev/null)" || return 0
  case "$out" in
    *'"busy"'*) ;;
    *) return 0 ;;
  esac
  printf '%s' "$out" | grep -Eq '"busy"[[:space:]]*:[[:space:]]*true'
}

if [ "$T3_UNIT_CHANGED" -eq 1 ]; then
  if agent_is_busy; then
    note "t3-code.service definition changed but the machine is busy (or its state could not be read), NOT restarting."
    note "run this once the machine is idle:  systemctl --user restart t3-code.service"
  else
    systemctl --user restart t3-code.service >&2
    note "t3-code.service restarted onto the new definition"
  fi
fi

# ---------------------------------------------------------------------------
# Health
# ---------------------------------------------------------------------------

if [ "$WITH_T3" -eq 1 ]; then
  step "Health"

  wait_for_health() {
    local deadline=$(( $(date +%s) + 60 ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
      if "$NODE_BIN" -e '
        const http = require("node:http");
        const req = http.get({ host: "127.0.0.1", port: Number(process.argv[1]), path: "/", timeout: 3000 }, (res) => {
          res.resume();
          process.exit(res.statusCode === 200 ? 0 : 1);
        });
        req.on("timeout", () => { req.destroy(); process.exit(1); });
        req.on("error", () => process.exit(1));
      ' "$PORT" >/dev/null 2>&1; then
        return 0
      fi
      sleep 2
    done
    return 1
  }

  if wait_for_health; then
    note "http://127.0.0.1:$PORT/ answered 200"
  else
    note "http://127.0.0.1:$PORT/ did NOT answer 200 within 60 s"
    note "check:  journalctl --user -u t3-code.service -n 60 --no-pager"
  fi
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

printf '\n== Legion Control, Linux side\n\n'
for line in "${SUMMARY[@]}"; do
  printf '  %s\n' "$line"
done

printf '\n  Unit states\n'
# Both of these exit non-zero for anything that is not enabled and running, and
# that is fine, the word they print is the answer. Only stdout is taken so one
# stderr line cannot wreck the alignment of the table.
unit_state() {
  local value
  value="$(systemctl --user "$1" "$2" 2>/dev/null | head -1 || true)"
  printf '%s' "${value:-unknown}"
}
UNITS=(legion-control-update.timer legion-control-update.service)
if [ "$WITH_T3" -eq 1 ]; then
  UNITS=(t3-code.service "${UNITS[@]}")
fi
for unit in "${UNITS[@]}"; do
  printf '    %-32s enabled=%-10s active=%s\n' \
    "$unit" \
    "$(unit_state is-enabled "$unit")" \
    "$(unit_state is-active "$unit")"
done
printf '    %s\n' "legion-control-update.service is inactive between runs by design, the timer starts it."

printf '\n  Next update run\n'
systemctl --user list-timers legion-control-update.timer --no-pager 2>/dev/null | sed 's/^/    /' || true

printf '\n  Agent:   %s %s status\n' "$NODE_BIN" "$AGENT_ENTRY"
printf '  Config:  %s\n' "$BASE/config.json"
printf '  Logs:    journalctl --user -u legion-control-update.service -n 50\n'
printf '           %s\n' "$BASE/legionctl.log"
printf '\n'
