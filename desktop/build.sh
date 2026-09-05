#!/usr/bin/env bash
# Builds the desktop client for Linux and Windows, from any of the three.
#
# Self-contained on purpose: the machines this runs on are a Windows box with only the .NET runtime
# and a Linux box that may have no dotnet at all, so the binary carries its own. The two archives
# are named exactly as the signed release manifest names them, because the client picks its own
# update by exact name and never by shape.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/.." && pwd)"
project="$here/LegionControl.Desktop"
tests="$here/LegionControl.Desktop.Tests"
out="$root/dist"

# The pinned toolchain first, then whatever dotnet is on the path. Never a global install: the
# version this is built with is a decision, not an accident of the machine it ran on.
dotnet="${LEGION_DOTNET:-}"
if [ -z "$dotnet" ]; then
  for candidate in \
    "$HOME/.local/share/legion-control-toolchains/dotnet10/dotnet" \
    "$(command -v dotnet || true)"; do
    if [ -n "$candidate" ] && [ -x "$candidate" ]; then dotnet="$candidate"; break; fi
  done
fi
if [ -z "$dotnet" ]; then
  echo "No dotnet found. Set LEGION_DOTNET to the SDK to build with." >&2
  exit 1
fi

version="$(sed -n 's:.*<Version>\(.*\)</Version>.*:\1:p' "$project/LegionControl.Desktop.csproj" | head -1)"
agent_version="$(sed -n 's|.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*|\1|p' "$root/agent/package.json" | head -1)"
if [ -z "$agent_version" ]; then
  echo "Could not read the agent version from agent/package.json." >&2
  exit 1
fi
echo "Legion Control desktop $version, built with $("$dotnet" --version)"

# The signed agent bundle, when the release tooling has produced one. A normal build carries none
# and says so in the app rather than offering an action that cannot work.
if [ -f "$out/legionctl-agent-$agent_version.tgz" ]; then
  echo "Bundling the signed agent from $out"
else
  echo "No signed agent bundle in $out: the agent install action will be unavailable in this build."
fi

case "${1:-all}" in
  test)
    targets=""
    ;;
  linux)
    targets="linux-x64"
    ;;
  windows)
    targets="win-x64"
    ;;
  all)
    targets="linux-x64 win-x64"
    ;;
  *)
    echo "usage: build.sh [all|linux|windows|test]" >&2
    exit 1
    ;;
esac

echo "Testing"
"$dotnet" test "$tests" --nologo -v quiet

mkdir -p "$out"

for runtime in $targets; do
  echo "Publishing $runtime"
  staging="$here/artifacts/$runtime"
  rm -rf "$staging"
  "$dotnet" publish "$project" \
    --configuration Release \
    --runtime "$runtime" \
    --self-contained true \
    --nologo \
    -p:PublishSingleFile=false \
    -p:DebugType=none \
    --output "$staging"

  # Archives carry stable metadata as well as stable file bytes. 1980 is the earliest timestamp
  # zip can represent, and the generated staging tree is safe to normalise in place.
  find "$staging" -exec touch -t 198001010000 {} +
  chmod -R u=rwX,go=rX "$staging"

  case "$runtime" in
    linux-x64)
      archive="$out/Legion-Control-linux-x64.tar.gz"
      rm -f "$archive"
      # GNU tar and the BSD tar shipped by macOS spell ownership and recursion controls
      # differently. Explicitly list sorted entries, suppress Apple metadata, and let gzip omit
      # its own timestamp so the result is reproducible on either build host.
      if tar --version 2>&1 | grep -q 'GNU tar'; then
        (cd "$staging" && find . -mindepth 1 -print | LC_ALL=C sort \
          | tar --no-recursion --numeric-owner --owner=0 --group=0 \
              --mtime='1980-01-01 00:00:00Z' -cf - -T -) | gzip -n > "$archive"
      else
        (cd "$staging" && find . -mindepth 1 -print | LC_ALL=C sort \
          | COPYFILE_DISABLE=1 tar -n --uid 0 --gid 0 --uname root --gname root \
              --no-xattrs --no-mac-metadata -cf - -T -) | gzip -n > "$archive"
      fi
      ;;
    win-x64)
      archive="$out/Legion-Control-windows-x64.zip"
      rm -f "$archive"
      (cd "$staging" && zip -q -r -X "$archive" .)
      ;;
  esac
  echo "  $archive"
  if command -v shasum >/dev/null 2>&1; then shasum -a 256 "$archive"; fi
done

echo "Done."
