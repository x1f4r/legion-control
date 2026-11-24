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
  # Every path that `cargo build --release -p legion-cli` and `package()` read from the repo root:
  # `apps/ui` is embedded at compile time by `crates/legion-doors/build.rs`, and `contract` holds
  # the compiled-in release public key. Verify all required paths before archiving so a missing
  # directory or renamed asset fails immediately with a clear error.
  REQUIRED_PATHS=(
    Cargo.toml
    Cargo.lock
    rust-toolchain.toml
    LICENSE
    crates
    contract
    packaging
    apps/ui
  )
  REQUIRED_FILES=(
    apps/ui/index.html
    contract/release-public-key.pem
  )
  for rel in "${REQUIRED_PATHS[@]}"; do
    [[ -e "${REPO_ROOT}/${rel}" ]] || die "Required source path is missing from checkout: ${rel}"
  done
  for rel in "${REQUIRED_FILES[@]}"; do
    [[ -f "${REPO_ROOT}/${rel}" ]] || die "Required compile-time asset is missing from checkout: ${rel}"
  done

  BUILD_REV="${LEGION_BUILD_REV:-}"
  if [[ -z "$BUILD_REV" ]] && command -v git >/dev/null 2>&1 \
    && git -C "$REPO_ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    if [[ -n "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=no 2>/dev/null || true)" ]]; then
      SHORT_REV="$(git -C "$REPO_ROOT" rev-parse --short=12 HEAD 2>/dev/null || true)"
      if [[ -n "$SHORT_REV" ]]; then
        BUILD_REV="${SHORT_REV}, modified"
      fi
    else
      BUILD_REV="$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || true)"
    fi
  fi

  ARCHIVE="${WORK}/legion-${PKGVER}.tar.gz"
  PREFIX="legion-control-legion-v${PKGVER}/"
  # Archive the workspace files needed to build and package legion, including uncommitted edits,
  # while skipping target/ and other build artifacts. When building from a clean commit SHA, write
  # the same PAX `comment=<sha>` header that GitHub's tag archives carry so PKGBUILD extracts it
  # identically to a published release tag.
  TAR_ARGS=(-C "$REPO_ROOT" --transform "s,^,${PREFIX},")
  if [[ "$BUILD_REV" =~ ^[0-9a-fA-F]{40}$ ]]; then
    TAR_ARGS+=(--format=posix --pax-option="comment=${BUILD_REV,,}")
    MAKEPKG_ENV=(-u LEGION_BUILD_REV PKGDEST="$OUT_DIR_ABS")
  elif [[ -n "$BUILD_REV" ]]; then
    MAKEPKG_ENV=(PKGDEST="$OUT_DIR_ABS" LEGION_BUILD_REV="$BUILD_REV")
  else
    MAKEPKG_ENV=(PKGDEST="$OUT_DIR_ABS")
  fi
  tar "${TAR_ARGS[@]}" -czf "$ARCHIVE" "${REQUIRED_PATHS[@]}"
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
  MAKEPKG_ENV=(PKGDEST="$OUT_DIR_ABS")
fi

(
  cd "$WORK"
  env "${MAKEPKG_ENV[@]}" makepkg --clean --force --nodeps
)

find "$OUT_DIR_ABS" -maxdepth 1 -type f -name 'legion*.pkg.tar*' -print | LC_ALL=C sort
