#!/usr/bin/env bash
# Verify a Linux desktop legion-app package (.deb or Arch .pkg.tar.*) and optional .AppImage
# inside a clean Docker container for one distribution.
#
# Usage:
#   scripts/legion-app-distro-test.sh <docker image> <path to .deb|.pkg.tar.*> [path to .AppImage]
#
# Examples:
#   scripts/legion-app-distro-test.sh debian:12 apps/legion-app/src-tauri/target/release/bundle/deb/legion-app_*.deb
#   scripts/legion-app-distro-test.sh ubuntu:22.04 bundle/deb/legion-app.deb bundle/appimage/legion-app.AppImage
#   scripts/legion-app-distro-test.sh archlinux:latest target/packages/legion-app-0.1.0-1-x86_64.pkg.tar.zst
set -euo pipefail

die() {
  printf 'legion-app-distro-test.sh: %s\n' "$1" >&2
  exit 1
}

usage() {
  cat >&2 <<'EOF'
Usage: scripts/legion-app-distro-test.sh <docker image> <package file> [AppImage file]
EOF
  exit 2
}

[[ "$#" -ge 2 && "$#" -le 3 ]] || usage
IMAGE="$1"
PKG_PATH="$2"
APPIMAGE_PATH="${3:-}"

[[ -f "$PKG_PATH" && -s "$PKG_PATH" ]] || die "Package file does not exist or is empty: $PKG_PATH"
if [[ -n "$APPIMAGE_PATH" ]]; then
  [[ -f "$APPIMAGE_PATH" && -s "$APPIMAGE_PATH" ]] || die "AppImage file does not exist or is empty: $APPIMAGE_PATH"
fi

case "$IMAGE" in
  debian:*|ubuntu:*)
    DISTRO_KIND="deb"
    ;;
  archlinux:*|archlinux)
    DISTRO_KIND="arch"
    ;;
  *)
    die "Unsupported image '$IMAGE' (expected debian:*, ubuntu:*, or archlinux:*)."
    ;;
esac

DOCKER_CMD=(docker)
if ! docker info >/dev/null 2>&1; then
  if sudo -n docker info >/dev/null 2>&1; then
    DOCKER_CMD=(sudo docker)
  else
    die "Cannot reach the Docker daemon (neither 'docker' nor 'sudo -n docker' succeeded)."
  fi
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
[[ -f "${SCRIPT_DIR}/retry-fetch.sh" ]] || die "Missing helper script: ${SCRIPT_DIR}/retry-fetch.sh"

PKG_DIR="$(cd -- "$(dirname -- "$PKG_PATH")" && pwd -P)"
PKG_BASE="$(basename -- "$PKG_PATH")"
DOCKER_MOUNTS=(
  -v "${SCRIPT_DIR}/retry-fetch.sh:/opt/retry-fetch.sh:ro"
  -v "${PKG_DIR}/${PKG_BASE}:/art/${PKG_BASE}:ro"
)
DOCKER_ENV=(
  -e "LEGION_APP_IMAGE=${IMAGE}"
  -e "LEGION_APP_DISTRO_KIND=${DISTRO_KIND}"
  -e "LEGION_APP_PKG=/art/${PKG_BASE}"
)

if [[ -n "$APPIMAGE_PATH" ]]; then
  IMG_DIR="$(cd -- "$(dirname -- "$APPIMAGE_PATH")" && pwd -P)"
  IMG_BASE="$(basename -- "$APPIMAGE_PATH")"
  DOCKER_MOUNTS+=(-v "${IMG_DIR}/${IMG_BASE}:/art/${IMG_BASE}:ro")
  DOCKER_ENV+=(-e "LEGION_APP_APPIMAGE=/art/${IMG_BASE}")
fi

printf '=== Verifying legion-app on %s (%s) ===\n' "$IMAGE" "$PKG_BASE"

CONTAINER_SCRIPT="$(cat <<'CONTAINER_EOF'
set -euo pipefail

step() {
  printf '[%s] %s\n' "$LEGION_APP_IMAGE" "$1"
}

fail() {
  printf '[%s] FAIL: %s\n' "$LEGION_APP_IMAGE" "$1" >&2
  exit 1
}

if [[ "$LEGION_APP_DISTRO_KIND" == "deb" ]]; then
  export DEBIAN_FRONTEND=noninteractive
  printf '#!/bin/sh\nexit 101\n' > /usr/sbin/policy-rc.d
  chmod 0755 /usr/sbin/policy-rc.d

  step "Installing legion-app .deb with apt-get (resolving dependencies cleanly)"
  sh /opt/retry-fetch.sh -- apt-get update -qq
  sh /opt/retry-fetch.sh --quiet -- apt-get install -y -qq "$LEGION_APP_PKG"
  rm -f /usr/sbin/policy-rc.d
else
  step "Installing legion-app package with pacman -U"
  sh /opt/retry-fetch.sh --quiet -- pacman -Syu --noconfirm
  sh /opt/retry-fetch.sh --quiet -- pacman -U --noconfirm "$LEGION_APP_PKG"
fi

test -x /usr/bin/legion-app || fail "/usr/bin/legion-app not installed"
test -f /usr/share/applications/legion-app.desktop || fail "desktop entry not installed"
grep -qx 'Name=Legion' /usr/share/applications/legion-app.desktop || fail "desktop entry missing Name=Legion"

step "Checking ldd on /usr/bin/legion-app"
LDD_OUT="$(ldd /usr/bin/legion-app 2>&1)"
if grep -Eq 'not found|GLIBC_' <<<"$LDD_OUT"; then
  printf '%s\n' "$LDD_OUT" >&2
  fail "ldd reported missing libraries or glibc version mismatches"
fi

if ! id me >/dev/null 2>&1; then
  useradd -m -s /bin/bash me
fi

step "Running /usr/bin/legion-app --version as non-root user me"
VER_OUT="$(su -s /bin/bash me -c 'DISPLAY="" WAYLAND_DISPLAY="" /usr/bin/legion-app --version' 2>&1)"
printf '  %s\n' "$VER_OUT"
[[ "$VER_OUT" =~ ^legion-app[[:space:]][0-9] ]] || fail "unexpected --version output: $VER_OUT"
if grep -q 'GLIBC_' <<<"$VER_OUT"; then
  fail "legion-app --version printed a GLIBC loader warning: $VER_OUT"
fi

step "Verifying /usr/bin/legion-app without a display reaches GTK init (no loader error)"
set +e
NO_DISP_OUT="$(su -s /bin/bash me -c 'DISPLAY="" WAYLAND_DISPLAY="" timeout 5 /usr/bin/legion-app' 2>&1)"
NO_DISP_RC=$?
set -e
[[ "$NO_DISP_RC" -ne 0 ]] || fail "expected non-zero exit without a display"
if grep -Eq 'cannot open shared object file|version `GLIBC_|not found' <<<"$NO_DISP_OUT"; then
  printf '%s\n' "$NO_DISP_OUT" >&2
  fail "legion-app failed with a dynamic loader error instead of missing display"
fi
if ! grep -Eqi 'gtk|display' <<<"$NO_DISP_OUT"; then
  printf '%s\n' "$NO_DISP_OUT" >&2
  fail "legion-app failed before GTK/display initialization"
fi

if [[ -n "${LEGION_APP_APPIMAGE:-}" ]]; then
  step "Verifying AppImage --version without loader or glibc errors"
  cp "$LEGION_APP_APPIMAGE" /home/me/legion-app.AppImage
  chmod 0755 /home/me/legion-app.AppImage
  chown me:me /home/me/legion-app.AppImage
  IMG_VER_OUT="$(su -s /bin/bash me -c 'APPIMAGE_EXTRACT_AND_RUN=1 DISPLAY="" WAYLAND_DISPLAY="" /home/me/legion-app.AppImage --version' 2>&1)"
  printf '  AppImage: %s\n' "$IMG_VER_OUT"
  [[ "$IMG_VER_OUT" =~ ^legion-app[[:space:]][0-9] ]] || fail "unexpected AppImage --version output: $IMG_VER_OUT"
  if grep -Eq 'GLIBC_|cannot open shared object file' <<<"$IMG_VER_OUT"; then
    fail "AppImage emitted loader/GLIBC errors: $IMG_VER_OUT"
  fi
fi

step "Removing legion-app package cleanly"
if [[ "$LEGION_APP_DISTRO_KIND" == "deb" ]]; then
  apt-get remove -y -qq legion-app >/dev/null
else
  pacman -R --noconfirm legion-app >/dev/null
fi
test ! -e /usr/bin/legion-app || fail "/usr/bin/legion-app remained after remove"
test ! -e /usr/share/applications/legion-app.desktop || fail "desktop entry remained after remove"

step "All legion-app checks passed"
CONTAINER_EOF
)"

"${DOCKER_CMD[@]}" run --rm \
  "${DOCKER_ENV[@]}" \
  "${DOCKER_MOUNTS[@]}" \
  "$IMAGE" bash -c "$CONTAINER_SCRIPT"
