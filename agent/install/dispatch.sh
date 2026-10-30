#!/bin/sh
# The forced command for a restricted Legion Control key.
#
# In authorized_keys:
#
#   command="/home/<you>/.legion-control/bin/dispatch.sh",restrict,no-pty ssh-ed25519 AAAA... phone
#
# `restrict` turns off port forwarding, agent forwarding, X11 and pty allocation;
# the forced command means the key cannot run anything else, whatever it asks
# for. What it asked for arrives in SSH_ORIGINAL_COMMAND, and the agent's own
# `dispatch` decides whether it is allowed.
#
# THIS SCRIPT NEVER INTERPRETS THAT STRING. It does not echo it, test it, or
# split it; it copies it, as one field, into a fixed-name assignment for the
# agent, which parses it with its own grammar. Every `exec` and assignment
# below is on names this file chose.
#
# The agent starts with an environment built from a fixed list and nothing
# else. A key's session can carry variables the client sent (sshd's AcceptEnv),
# and some of them run code: NODE_OPTIONS=--import=data:… would execute
# whatever it names before the agent's first line, and the agent's own checks
# could not stop what runs before them.
#
# POSIX sh rather than bash: this has to work on a Raspberry Pi, a Mac and a
# BusyBox container without asking which shell is installed.

set -eu

# Installers copy this beside the stable launcher. Keeping the forced command
# outside the replaceable agent tree means a crash during self-update cannot
# strand the restricted key at a missing path.
INSTALL_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P)
LAUNCHER="$INSTALL_DIR/legionctl"
if [ ! -x "$LAUNCHER" ]; then
  printf '{"ok":false,"contract":3,"reasonCode":"internal","message":"the stable agent launcher is not installed at %s"}\n' "$LAUNCHER"
  exit 1
fi

# What the agent needs from the session: where things are, the locale, the
# connection it answers, and the command itself. XDG_RUNTIME_DIR and
# DBUS_SESSION_BUS_ADDRESS are how `systemctl --user` finds the user's service
# manager, which status and restart ask about services. Only a variable that
# is set is copied; `eval` sees a name from this list, never a value.
set --
for name in PATH HOME USER LOGNAME SHELL LANG LC_ALL LC_CTYPE LC_MESSAGES TZ TMPDIR \
  SSH_CONNECTION SSH_CLIENT SSH_ORIGINAL_COMMAND XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS; do
  eval "[ -n \"\${$name+set}\" ]" || continue
  eval "set -- \"\$@\" \"$name=\${$name}\""
done

# This describes the access path. It is not an authenticated key identity.
exec env -i "$@" LEGIONCTL_RESTRICTED=1 LEGIONCTL_CLIENT=restricted-ssh "$LAUNCHER" dispatch
