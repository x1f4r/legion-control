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
# THIS SCRIPT NEVER INTERPRETS THAT STRING. It does not echo it, test it, or pass
# it as an argument — it is left in the environment, where it cannot be word-split
# or expanded, and the agent parses it with its own grammar. Every `exec` and
# assignment below is on values this file chose.
#
# POSIX sh rather than bash: this has to work on a Raspberry Pi, a Mac and a
# BusyBox container without asking which shell is installed.

set -eu

# Installers copy this beside the stable launcher. Keeping the forced command
# outside the replaceable agent tree means a crash during self-update cannot
# strand the restricted key at a missing path.
INSTALL_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
LAUNCHER="$INSTALL_DIR/legionctl"
if [ ! -x "$LAUNCHER" ]; then
  printf '{"ok":false,"contract":3,"reasonCode":"internal","message":"the stable agent launcher is not installed at %s"}\n' "$LAUNCHER"
  exit 1
fi

# This describes the access path. It is not an authenticated key identity.
LEGIONCTL_RESTRICTED=1
LEGIONCTL_CLIENT=restricted-ssh
export LEGIONCTL_RESTRICTED LEGIONCTL_CLIENT

exec "$LAUNCHER" dispatch
