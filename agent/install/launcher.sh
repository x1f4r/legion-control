#!/bin/sh
# Rendered by install-linux.sh into <LEGIONCTL_HOME>/bin/legionctl.
set -eu
BIN_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
LEGIONCTL_HOME=$(CDPATH= cd -- "$BIN_DIR/.." && pwd -P)
export LEGIONCTL_HOME
# Node reads these before the launcher's first line, and NODE_OPTIONS can name
# code to run (--import, --require). Nothing the agent needs comes from them.
unset NODE_OPTIONS NODE_PATH
exec @NODE_SHELL@ "$BIN_DIR/launcher.mjs" "$@"
