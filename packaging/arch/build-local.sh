#!/usr/bin/env bash
# Build the Arch source or -bin package from the local checkout using makepkg.
#
# Usage:
#   packaging/arch/build-local.sh source <out dir>
#   packaging/arch/build-local.sh bin <path to static legion binary> <out dir>
#
# Pre-populates the source archive or files that the PKGBUILD expects so makepkg runs
# offline against the current working tree without needing a published GitHub release tag.
set -euo pipefail

die() {
  printf 'build-local.sh: %s\n' "$1" >&2
  exit 1
}

usage() {
  cat >&2 <<'EOF'
Usage:
  packaging/arch/build-local.sh source <out dir>
  packaging/arch/build-local.sh bin <static legion binary> <out dir>
EOF
  exit 2
}

[[ "$#" -ge 2 ]] || usage
MODE="$1"
shift

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd -P)"

command -v makepkg >/dev/null 2>&1 || die "makepkg is required (run on Arch Linux as a non-root user)."

case "$MODE" in
  source)
    [[ "$#" -eq 1 ]] || usage
    OUT_DIR="$1"
    PKGBUILD_SRC="${SCRIPT_DIR}/PKGBUILD"
    ;;
  bin)
    [[ "$#" -eq 2 ]] || usage
    STATIC_BIN="$1"
    OUT_DIR="$2"
    [[ -f "$STATIC_BIN" && -s "$STATIC_BIN" ]] || die "Binary does not exist or is empty: $STATIC_BIN"
    PKGBUILD_SRC="${SCRIPT_DIR}/legion-bin/PKGBUILD"
    ;;
  *)
    usage
    ;;
esac

PKGVER="$(awk -F= '/^pkgver=/{print $2; exit}' "$PKGBUILD_SRC")"
[[ -n "$PKGVER" ]] || die "Could not read pkgver from $PKGBUILD_SRC."

mkdir -p "$OUT_DIR"
OUT_DIR_ABS="$(cd -- "$OUT_DIR" && pwd -P)"
WORK="$(mktemp -d "${OUT_DIR_ABS}/.legion-arch-build.XXXXXX")"
cleanup() {
  rm -rf "$WORK"
}
trap cleanup EXIT

cp "$PKGBUILD_SRC" "${WORK}/PKGBUILD"

if [[ "$MODE" == "source" ]]; then
  ARCHIVE="${WORK}/legion-${PKGVER}.tar.gz"
  PREFIX="legion-control-legion-v${PKGVER}/"
  # Archive the workspace files needed to build and package legion, including uncommitted edits,
  # while skipping target/ and other build artifacts. apps/ui is the interface that legion serve
  # embeds at compile time.
  tar -C "$REPO_ROOT" \
    --transform "s,^,${PREFIX}," \
    -czf "$ARCHIVE" \
    Cargo.toml Cargo.lock rust-toolchain.toml LICENSE crates contract packaging apps/ui
  HASH="$(sha256sum "$ARCHIVE" | awk '{print $1}')"
  sed -i "s/^sha256sums=.*/sha256sums=('${HASH}')/" "${WORK}/PKGBUILD"
else
  CARCH="$(uname -m)"
  cp "${REPO_ROOT}/packaging/systemd/legion-serve.service" "${WORK}/legion-serve.service-${PKGVER}"
  cp "${REPO_ROOT}/packaging/man/legion.1" "${WORK}/legion.1-${PKGVER}"
  cp "${REPO_ROOT}/LICENSE" "${WORK}/LICENSE-${PKGVER}"
  cp "$STATIC_BIN" "${WORK}/legion-${PKGVER}-${CARCH}-unknown-linux-musl"

  H_UNIT="$(sha256sum "${WORK}/legion-serve.service-${PKGVER}" | awk '{print $1}')"
  H_MAN="$(sha256sum "${WORK}/legion.1-${PKGVER}" | awk '{print $1}')"
  H_LIC="$(sha256sum "${WORK}/LICENSE-${PKGVER}" | awk '{print $1}')"
  H_BIN="$(sha256sum "${WORK}/legion-${PKGVER}-${CARCH}-unknown-linux-musl" | awk '{print $1}')"
  sed -i "s/^sha256sums=.*/sha256sums=('${H_UNIT}' '${H_MAN}' '${H_LIC}')/" "${WORK}/PKGBUILD"
  sed -i "s/^sha256sums_${CARCH}=.*/sha256sums_${CARCH}=('${H_BIN}')/" "${WORK}/PKGBUILD"
fi

(
  cd "$WORK"
  PKGDEST="$OUT_DIR_ABS" makepkg --clean --force --nodeps
)

find "$OUT_DIR_ABS" -maxdepth 1 -type f -name 'legion*.pkg.tar*' -print | LC_ALL=C sort
