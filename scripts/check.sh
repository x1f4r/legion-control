#!/usr/bin/env bash
# Run the checks for one component, or the complete suite on a configured Mac.
#
# CI runs each component on its own and release.sh runs all of them before it builds anything, so
# what is checked here is what a release is held to.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$ROOT"
component="${1:-all}"
case "$component" in all|agent|mac|android|desktop) ;; *) echo "Usage: $0 [all|agent|mac|android|desktop]" >&2; exit 2 ;; esac

if [[ "$component" == all || "$component" == agent ]]; then
  npm --prefix agent run check
  node scripts/check-versions.mjs
  npm --prefix agent test
  node --test tests/*.test.mjs
  # The contract: its documents against their schemas, the generated files against what the
  # generators write today, and the references and vectors against the contract's own tests.
  node contract/validate.mjs
  node contract/tools/generate.mjs --check
  node --test contract/test/*.test.mjs
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
