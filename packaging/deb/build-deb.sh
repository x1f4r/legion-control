#!/usr/bin/env bash
# Build a policy-conforming Debian/Ubuntu .deb package around a static legion binary.
#
# Usage:
#   packaging/deb/build-deb.sh <path to static legion binary> <arch: amd64|arm64> <version> <out dir>
set -euo pipefail

PKG_NAME="legion"
MAINTAINER="x1f4r <106302360+x1f4r@users.noreply.github.com>"
HOMEPAGE="https://github.com/x1f4r/legion-control"

die() {
  printf 'build-deb.sh: %s\n' "$1" >&2
  exit 1
}

usage() {
  cat >&2 <<'EOF'
Usage: packaging/deb/build-deb.sh <binary> <arch: amd64|arm64> <version> <out dir>

Build a Debian/Ubuntu .deb package from a compiled static legion binary using dpkg-deb.
EOF
  exit 2
}

if [[ "$#" -ne 4 ]]; then
  usage
fi

BINARY_PATH="$1"
ARCH="$2"
RAW_VERSION="$3"
OUT_DIR="$4"

[[ -f "$BINARY_PATH" && -s "$BINARY_PATH" ]] || die "Binary does not exist or is empty: $BINARY_PATH"

case "$ARCH" in
  amd64|arm64) ;;
  *) die "Unsupported architecture '$ARCH' (expected amd64 or arm64)." ;;
esac

UPSTREAM_VERSION="${RAW_VERSION#legion-v}"
UPSTREAM_VERSION="${UPSTREAM_VERSION#v}"
[[ -n "$UPSTREAM_VERSION" ]] || die "Version cannot be empty."
if [[ ! "$UPSTREAM_VERSION" =~ ^[0-9][0-9A-Za-z.+~-]*$ ]]; then
  die "Invalid version '$RAW_VERSION'."
fi

# Map SemVer pre-release hyphens to Debian's `~` so pre-releases sort below the final release,
# and add a Debian revision (`-1`) so dpkg treats this as a standard non-native package.
DEB_VERSION="${UPSTREAM_VERSION//-/\~}-1"

command -v dpkg-deb >/dev/null 2>&1 || die "dpkg-deb is required to build a .deb package."
command -v gzip >/dev/null 2>&1 || die "gzip is required to compress the man page and changelog."
command -v md5sum >/dev/null 2>&1 || die "md5sum is required to generate DEBIAN/md5sums."

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd -P)"

UNIT_SRC="${REPO_ROOT}/packaging/systemd/legion-serve.service"
MAN_SRC="${REPO_ROOT}/packaging/man/legion.1"
LICENSE_SRC="${REPO_ROOT}/LICENSE"

[[ -f "$UNIT_SRC" ]] || die "Missing systemd user unit: $UNIT_SRC"
[[ -f "$MAN_SRC" ]] || die "Missing man page source: $MAN_SRC"
[[ -f "$LICENSE_SRC" ]] || die "Missing LICENSE file: $LICENSE_SRC"

mkdir -p "$OUT_DIR"
OUT_DIR_ABS="$(cd -- "$OUT_DIR" && pwd -P)"
STAGE="$(mktemp -d "${OUT_DIR_ABS}/.${PKG_NAME}-deb-stage.XXXXXX")"
chmod 0755 "$STAGE"
cleanup() {
  rm -rf "$STAGE"
}
trap cleanup EXIT

install -d -m 0755 \
  "${STAGE}/DEBIAN" \
  "${STAGE}/usr/bin" \
  "${STAGE}/usr/lib/${PKG_NAME}" \
  "${STAGE}/usr/lib/systemd/user" \
  "${STAGE}/usr/share/doc/${PKG_NAME}" \
  "${STAGE}/usr/share/man/man1"

install -m 0755 "$BINARY_PATH" "${STAGE}/usr/bin/legion"
printf 'deb\n' > "${STAGE}/usr/lib/${PKG_NAME}/managed-by"
chmod 0644 "${STAGE}/usr/lib/${PKG_NAME}/managed-by"

install -m 0644 "$UNIT_SRC" "${STAGE}/usr/lib/systemd/user/legion-serve.service"

gzip -n -9 -c "$MAN_SRC" > "${STAGE}/usr/share/man/man1/legion.1.gz"
chmod 0644 "${STAGE}/usr/share/man/man1/legion.1.gz"

# Machine-readable DEP-5 copyright file generated from the repository LICENSE.
{
  printf 'Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/\n'
  printf 'Upstream-Name: %s\n' "$PKG_NAME"
  printf 'Upstream-Contact: %s\n' "$MAINTAINER"
  printf 'Source: %s\n' "$HOMEPAGE"
  printf '\n'
  printf 'Files: *\n'
  printf 'Copyright: 2026 x1f4r\n'
  printf 'License: Expat\n'
  printf '\n'
  printf 'License: Expat\n'
  # Skip the title and copyright lines at the top of LICENSE and indent the body per DEP-5.
  awk '
    NR <= 4 { next }
    /^[[:space:]]*$/ { print " ."; next }
    { print " " $0 }
  ' "$LICENSE_SRC"
} > "${STAGE}/usr/share/doc/${PKG_NAME}/copyright"
chmod 0644 "${STAGE}/usr/share/doc/${PKG_NAME}/copyright"

# RFC 2822 timestamp for changelog.Debian.gz (reproducible when SOURCE_DATE_EPOCH or git is set).
if [[ -n "${SOURCE_DATE_EPOCH:-}" ]]; then
  CHANGELOG_DATE="$(LC_ALL=C date -u -d "@${SOURCE_DATE_EPOCH}" '+%a, %d %b %Y %H:%M:%S +0000')"
elif git -C "$REPO_ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  COMMIT_EPOCH="$(git -C "$REPO_ROOT" log -1 --format=%ct 2>/dev/null || true)"
  if [[ -n "$COMMIT_EPOCH" ]]; then
    CHANGELOG_DATE="$(LC_ALL=C date -u -d "@${COMMIT_EPOCH}" '+%a, %d %b %Y %H:%M:%S +0000')"
  else
    CHANGELOG_DATE="$(LC_ALL=C date -u '+%a, %d %b %Y %H:%M:%S +0000')"
  fi
else
  CHANGELOG_DATE="$(LC_ALL=C date -u '+%a, %d %b %Y %H:%M:%S +0000')"
fi

{
  printf '%s (%s) unstable; urgency=medium\n' "$PKG_NAME" "$DEB_VERSION"
  printf '\n'
  printf '  * Package legion %s static binary and systemd user unit.\n' "$UPSTREAM_VERSION"
  printf '\n'
  printf ' -- %s  %s\n' "$MAINTAINER" "$CHANGELOG_DATE"
} | gzip -n -9 > "${STAGE}/usr/share/doc/${PKG_NAME}/changelog.Debian.gz"
chmod 0644 "${STAGE}/usr/share/doc/${PKG_NAME}/changelog.Debian.gz"

INSTALLED_SIZE="$(du -sk "${STAGE}/usr" | awk '{print $1}')"

cat > "${STAGE}/DEBIAN/control" <<EOF
Package: ${PKG_NAME}
Version: ${DEB_VERSION}
Architecture: ${ARCH}
Maintainer: ${MAINTAINER}
Installed-Size: ${INSTALLED_SIZE}
Section: admin
Priority: optional
Homepage: ${HOMEPAGE}
Recommends: openssh-client
Suggests: openssh-server, tailscale
Description: Remote control node and CLI for managing machines
 Legion is a single binary that runs on Linux, macOS, and Windows to inspect
 machines, run commands, transfer files, manage detached background jobs, and
 reach other nodes over SSH or a loopback HTTP endpoint.
EOF
chmod 0644 "${STAGE}/DEBIAN/control"

(
  cd "$STAGE"
  find usr -type f -print0 | LC_ALL=C sort -z | xargs -0 md5sum > DEBIAN/md5sums
)
chmod 0644 "${STAGE}/DEBIAN/md5sums"

OUT_FILE="${OUT_DIR_ABS}/${PKG_NAME}_${DEB_VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group --build "$STAGE" "$OUT_FILE" >/dev/null
printf '%s\n' "$OUT_FILE"
