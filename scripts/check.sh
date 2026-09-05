#!/usr/bin/env bash
# Run the checks for one component, or the complete suite on a configured Mac.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$ROOT"
component="${1:-all}"
case "$component" in all|agent|mac|android|desktop) ;; *) echo "Usage: $0 [all|agent|mac|android|desktop]" >&2; exit 2 ;; esac

if [[ "$component" == all || "$component" == agent ]]; then
  npm --prefix agent run check
  npm --prefix agent test
  node --test tests/*.test.mjs
  node contract/validate.mjs
fi
if [[ "$component" == all || "$component" == mac ]]; then
  swift test --package-path mac
  swift build --package-path mac -c release
fi
if [[ "$component" == all || "$component" == android ]]; then
  if [[ -n "${LEGION_ANDROID_JAVA_HOME:-}" ]]; then
    export JAVA_HOME="$LEGION_ANDROID_JAVA_HOME"
  elif [[ "$(uname -s)" == Darwin ]]; then
    export JAVA_HOME="$(/usr/libexec/java_home -v 21)"
  fi
  (cd android && ./gradlew :app:testDebugUnitTest :app:testReleaseUnitTest :app:lintDebug --console=plain)
fi
if [[ "$component" == all || "$component" == desktop ]]; then
  DOTNET="${LEGION_DOTNET:-dotnet}"
  "$DOTNET" test desktop/LegionControl.Desktop.Tests --configuration Release
  "$DOTNET" build desktop/LegionControl.Desktop --configuration Release
fi
