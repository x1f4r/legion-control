#!/usr/bin/env bash
# Build portable Linux desktop .deb and .AppImage bundles inside an ubuntu:22.04 container
# (glibc 2.35 baseline) so the shipped artifacts run on Debian 12+, Ubuntu 22.04+, and Arch
# without requiring newer glibc symbols from the host or CI runner.
#
# Usage:
#   scripts/build-linux-app-bundles.sh [--sequence <YYYYMMDDHHMM>] [--build-log <path>] [--preview-key <pubkey.pem>]
#
# Optional environment variables:
#   LEGION_BUILD_SEQUENCE   Positive update sequence (overridden by --sequence).
#   LEGION_BUILD_REV        Git revision embedded in the app (defaults to git HEAD).
#   LEGION_APP_U22_CACHE    Host cache directory for the container's tauri-cli binary
#                           (default: ${XDG_CACHE_HOME:-$HOME/.cache}/legion-app-u22).
#
# Preview update keys are only forwarded when explicitly passed via --preview-key so
# ordinary release-keyed builds can never inherit LEGION_PREVIEW_UPDATE_KEY from the shell.
set -euo pipefail

die() {
  printf 'build-linux-app-bundles.sh: %s\n' "$1" >&2
  exit 1
}

SEQUENCE="${LEGION_BUILD_SEQUENCE:-}"
BUILD_LOG=""
PREVIEW_KEY=""

while [[ "$#" -gt 0 ]]; do
  case "$1" in
    --sequence)
      [[ "$#" -ge 2 ]] || die "--sequence requires a value."
      SEQUENCE="$2"
      shift 2
      ;;
    --sequence=*)
      SEQUENCE="${1#--sequence=}"
      shift
      ;;
    --build-log)
      [[ "$#" -ge 2 ]] || die "--build-log requires a path."
      BUILD_LOG="$2"
      shift 2
      ;;
    --build-log=*)
      BUILD_LOG="${1#--build-log=}"
      shift
      ;;
    --preview-key)
      [[ "$#" -ge 2 ]] || die "--preview-key requires a path."
      PREVIEW_KEY="$2"
      shift 2
      ;;
    --preview-key=*)
      PREVIEW_KEY="${1#--preview-key=}"
      shift
      ;;
    -h|--help)
      cat <<'EOF'
Usage: scripts/build-linux-app-bundles.sh [--sequence <YYYYMMDDHHMM>] [--build-log <path>] [--preview-key <pubkey.pem>]
EOF
      exit 0
      ;;
    *)
      die "Unknown argument: $1"
      ;;
  esac
done

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"

DOCKER_CMD=(docker)
if ! docker info >/dev/null 2>&1; then
  if sudo -n docker info >/dev/null 2>&1; then
    DOCKER_CMD=(sudo docker)
  else
    die "Cannot reach the Docker daemon (neither 'docker' nor 'sudo -n docker' succeeded)."
  fi
fi

RUST_CHANNEL="$(awk -F'"' '/^channel[[:space:]]*=/ { print $2; exit }' "${REPO_ROOT}/rust-toolchain.toml")"
[[ -n "$RUST_CHANNEL" ]] || die "Could not read toolchain channel from rust-toolchain.toml."

RUSTUP_HOME_HOST="${RUSTUP_HOME:-$HOME/.rustup}"
TOOLCHAIN_DIR="${RUSTUP_HOME_HOST}/toolchains/${RUST_CHANNEL}-x86_64-unknown-linux-gnu"
[[ -x "${TOOLCHAIN_DIR}/bin/rustc" ]] || die "Expected Rust toolchain at ${TOOLCHAIN_DIR}; run 'rustup toolchain install' first."

CARGO_HOME_HOST="${CARGO_HOME:-$HOME/.cargo}"
mkdir -p "${CARGO_HOME_HOST}/registry"

U22_CACHE="${LEGION_APP_U22_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/legion-app-u22}"
mkdir -p "${U22_CACHE}/tauri-cli" "${U22_CACHE}/cargo-home"

BUILD_REV="${LEGION_BUILD_REV:-}"
if [[ -z "$BUILD_REV" ]] && command -v git >/dev/null 2>&1 \
  && git -C "$REPO_ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  BUILD_REV="$(git -C "$REPO_ROOT" rev-parse --short=12 HEAD 2>/dev/null || true)"
fi

BASE_VER="$(awk -F'"' '/^version[[:space:]]*=/ { print $2; exit }' "${REPO_ROOT}/apps/legion-app/src-tauri/Cargo.toml")"
[[ -n "$BASE_VER" ]] || die "Could not read version from apps/legion-app/src-tauri/Cargo.toml."

TAURI_CFG=""
if [[ -n "$SEQUENCE" ]]; then
  TAURI_CFG="$(node "${REPO_ROOT}/scripts/make-update-set.mjs" tauri-config --os linux --version "$BASE_VER" --sequence "$SEQUENCE")"
fi

HOST_UID="$(id -u)"
HOST_GID="$(id -g)"

rm -rf "${REPO_ROOT}/apps/legion-app/src-tauri/target/release/bundle"

DOCKER_MOUNTS=(
  -v "${REPO_ROOT}:/workspace/legion-control"
  -v "${TOOLCHAIN_DIR}:/opt/rust:ro"
  -v "${CARGO_HOME_HOST}/registry:/opt/host-registry"
  -v "${U22_CACHE}:/u22-cache"
)
if [[ -d "${CARGO_HOME_HOST}/git" ]]; then
  DOCKER_MOUNTS+=(-v "${CARGO_HOME_HOST}/git:/opt/host-git")
fi

DOCKER_ENVS=(
  -e "HOST_UID=${HOST_UID}"
  -e "HOST_GID=${HOST_GID}"
  -e "LEGION_BUILD_REV=${BUILD_REV}"
  -e "LEGION_BUILD_SEQUENCE=${SEQUENCE}"
  -e "TAURI_CFG=${TAURI_CFG}"
)
if [[ -n "$PREVIEW_KEY" ]]; then
  [[ -f "$PREVIEW_KEY" ]] || die "Preview public key file does not exist: ${PREVIEW_KEY}"
  PREVIEW_KEY_ABS="$(cd -- "$(dirname -- "$PREVIEW_KEY")" && pwd -P)/$(basename -- "$PREVIEW_KEY")"
  PREVIEW_KEY_SHA="$(sha256sum "$PREVIEW_KEY_ABS" | awk '{ print $1 }')"
  PREVIEW_KEY_CONTAINER="/opt/preview-keys/${PREVIEW_KEY_SHA}.pub.pem"
  DOCKER_MOUNTS+=(-v "${PREVIEW_KEY_ABS}:${PREVIEW_KEY_CONTAINER}:ro")
  DOCKER_ENVS+=(-e "LEGION_PREVIEW_UPDATE_KEY=${PREVIEW_KEY_CONTAINER}")
fi

CONTAINER_SCRIPT="$(cat <<'CONTAINER_EOF'
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
mkdir -p /home/me
cleanup_ownership() {
  chown -R "${HOST_UID}:${HOST_GID}" \
    /workspace/legion-control/apps/legion-app/src-tauri/target \
    /workspace/legion-control/apps/legion-app/dist \
    /opt/host-registry \
    /u22-cache 2>/dev/null || true
  if [[ -d /opt/host-git ]]; then
    chown -R "${HOST_UID}:${HOST_GID}" /opt/host-git 2>/dev/null || true
  fi
}
trap cleanup_ownership EXIT

sh /workspace/legion-control/scripts/retry-fetch.sh -- apt-get update -qq
sh /workspace/legion-control/scripts/retry-fetch.sh --quiet -- apt-get install -y -qq --no-install-recommends \
  build-essential \
  ca-certificates \
  curl \
  dpkg-dev \
  file \
  libayatana-appindicator3-dev \
  libgtk-3-dev \
  librsvg2-dev \
  libssl-dev \
  libwebkit2gtk-4.1-dev \
  libxdo-dev \
  patchelf \
  pkg-config \
  squashfs-tools \
  xdg-utils

export HOME=/home/me
export CARGO_HOME=/u22-cache/cargo-home
mkdir -p "${CARGO_HOME}"
# Reuse the host's registry cache without exposing host ~/.cargo/bin binaries
# (which may link against a newer host glibc than Ubuntu 22.04).
if [[ ! -L "${CARGO_HOME}/registry" ]]; then
  rm -rf "${CARGO_HOME}/registry"
  ln -s /opt/host-registry "${CARGO_HOME}/registry"
fi
if [[ -d /opt/host-git && ! -L "${CARGO_HOME}/git" ]]; then
  rm -rf "${CARGO_HOME}/git"
  ln -s /opt/host-git "${CARGO_HOME}/git"
fi
export PATH="/u22-cache/tauri-cli/bin:/opt/rust/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

if [[ "$(/u22-cache/tauri-cli/bin/cargo-tauri --version 2>/dev/null || true)" != "tauri-cli 2.12.6" ]]; then
  CARGO_HOME=/u22-cache/tauri-install-cargo sh /workspace/legion-control/scripts/retry-fetch.sh -- \
    cargo install tauri-cli --version =2.12.6 --locked --force --root /u22-cache/tauri-cli
fi

cd /workspace/legion-control/apps/legion-app
rm -rf src-tauri/target/release/bundle

export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--remap-path-prefix=${HOME}=/build/home --remap-path-prefix=${CARGO_HOME}=/cargo --remap-path-prefix=/workspace/legion-control=/build"
export APPIMAGE_EXTRACT_AND_RUN=1

BUILD_ARGS=(build --bundles deb,appimage)
if [[ -n "${TAURI_CFG:-}" ]]; then
  BUILD_ARGS+=(--config "$TAURI_CFG")
fi

/u22-cache/tauri-cli/bin/cargo-tauri "${BUILD_ARGS[@]}"

# Refuse any glibc symbol newer than Ubuntu 22.04's glibc 2.35 so older Debian/Ubuntu systems stay supported.
BAD_GLIBC="$(readelf -V src-tauri/target/release/legion-app | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | awk -F'[._]' '$2 > 2 || ($2 == 2 && $3 > 35)' | sort -u || true)"
if [[ -n "$BAD_GLIBC" ]]; then
  printf 'ERROR: legion-app binary requires glibc newer than 2.35:\n%s\n' "$BAD_GLIBC" >&2
  exit 1
fi
CONTAINER_EOF
)"

if [[ -n "$BUILD_LOG" ]]; then
  mkdir -p "$(dirname -- "$BUILD_LOG")"
  "${DOCKER_CMD[@]}" run --rm \
    "${DOCKER_MOUNTS[@]}" \
    "${DOCKER_ENVS[@]}" \
    ubuntu:22.04 bash -c "$CONTAINER_SCRIPT" 2>&1 | tee "$BUILD_LOG"
else
  "${DOCKER_CMD[@]}" run --rm \
    "${DOCKER_MOUNTS[@]}" \
    "${DOCKER_ENVS[@]}" \
    ubuntu:22.04 bash -c "$CONTAINER_SCRIPT"
fi
