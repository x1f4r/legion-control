#!/bin/sh
# Rendered by install-linux.sh into <LEGIONCTL_HOME>/bin/legionctl.
set -eu
BIN_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
LEGIONCTL_HOME=$(CDPATH= cd -- "$BIN_DIR/.." && pwd -P)
export LEGIONCTL_HOME
exec @NODE_SHELL@ "$BIN_DIR/launcher.mjs" "$@"
