#!/usr/bin/env bash
# Verify a legion Linux package inside a clean Docker container for one distribution.
#
# Usage:
#   scripts/legion-distro-test.sh <docker image> <path to .deb|.pkg.tar.* or 'pkgbuild'>
#
# Examples:
#   scripts/legion-distro-test.sh debian:12 target/packages/legion_0.1.0-1_amd64.deb
#   scripts/legion-distro-test.sh ubuntu:24.04 target/packages/legion_0.1.0-1_amd64.deb
#   scripts/legion-distro-test.sh archlinux:latest pkgbuild
#
# Optional environment variables:
#   LEGION_CARGO_CACHE_DIR  Host directory mounted as the cargo cache during Arch pkgbuild runs.
#   LEGION_STATIC_BIN       Optional static musl binary; when set in Arch pkgbuild mode, also
#                           builds and namcap-checks packaging/arch/legion-bin/PKGBUILD.
set -euo pipefail

die() {
  printf 'legion-distro-test.sh: %s\n' "$1" >&2
  exit 1
}

usage() {
  cat >&2 <<'EOF'
Usage: scripts/legion-distro-test.sh <image> <package file or 'pkgbuild'>
EOF
  exit 2
}

[[ "$#" -eq 2 ]] || usage
IMAGE="$1"
TARGET="$2"

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"

# Use docker directly when the caller has access; fall back to non-interactive sudo docker.
DOCKER_CMD=(docker)
if ! docker info >/dev/null 2>&1; then
  if sudo -n docker info >/dev/null 2>&1; then
    DOCKER_CMD=(sudo docker)
  else
    die "Cannot reach the Docker daemon (neither 'docker' nor 'sudo -n docker' succeeded)."
  fi
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

DOCKER_MOUNTS=()
DOCKER_ENV=()

if [[ "$TARGET" == "pkgbuild" ]]; then
  [[ "$DISTRO_KIND" == "arch" ]] || die "'pkgbuild' mode is only supported with archlinux images."
  # Verify every source path required by `cargo build` and `package()` before launching the
  # container: `apps/ui` is embedded at compile time by `crates/legion-doors/build.rs` and
  # `contract/release-public-key.pem` is compiled into release verification.
  for rel in Cargo.toml Cargo.lock rust-toolchain.toml LICENSE crates contract packaging apps/ui; do
    [[ -e "${REPO_ROOT}/${rel}" ]] || die "Required source path is missing from checkout: ${rel}"
  done
  for rel in apps/ui/index.html contract/release-public-key.pem; do
    [[ -f "${REPO_ROOT}/${rel}" ]] || die "Required compile-time asset is missing from checkout: ${rel}"
  done

  HOST_BUILD_REV="${LEGION_BUILD_REV:-}"
  if [[ -z "$HOST_BUILD_REV" ]] && command -v git >/dev/null 2>&1 \
    && git -C "$REPO_ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    if [[ -n "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=no 2>/dev/null || true)" ]]; then
      SHORT_REV="$(git -C "$REPO_ROOT" rev-parse --short=12 HEAD 2>/dev/null || true)"
      if [[ -n "$SHORT_REV" ]]; then
        HOST_BUILD_REV="${SHORT_REV}, modified"
      fi
    else
      HOST_BUILD_REV="$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || true)"
    fi
  fi
  [[ -n "$HOST_BUILD_REV" ]] || die "Could not determine build revision (set LEGION_BUILD_REV or run inside a git checkout)."

  DOCKER_MOUNTS+=(-v "${REPO_ROOT}:/src:ro")
  DOCKER_ENV+=(-e "LEGION_TEST_MODE=pkgbuild" -e "LEGION_BUILD_REV=${HOST_BUILD_REV}" -e "LEGION_EXPECT_BUILD_REV=1")
  if [[ -n "${LEGION_CARGO_CACHE_DIR:-}" ]]; then
    mkdir -p "$LEGION_CARGO_CACHE_DIR"
    CACHE_ABS="$(cd -- "$LEGION_CARGO_CACHE_DIR" && pwd -P)"
    DOCKER_MOUNTS+=(-v "${CACHE_ABS}:/cargo-cache")
    DOCKER_ENV+=(-e "LEGION_HAS_CARGO_CACHE=1")
  fi
  if [[ -n "${LEGION_STATIC_BIN:-}" ]]; then
    [[ -f "$LEGION_STATIC_BIN" && -s "$LEGION_STATIC_BIN" ]] || die "LEGION_STATIC_BIN is not a readable non-empty file: $LEGION_STATIC_BIN"
    STATIC_DIR="$(cd -- "$(dirname -- "$LEGION_STATIC_BIN")" && pwd -P)"
    STATIC_BASE="$(basename -- "$LEGION_STATIC_BIN")"
    DOCKER_MOUNTS+=(-v "${STATIC_DIR}/${STATIC_BASE}:/static-bin/legion:ro")
    DOCKER_ENV+=(-e "LEGION_TEST_BIN_PKG=1")
  fi
else
  [[ -f "$TARGET" && -s "$TARGET" ]] || die "Package file does not exist or is empty: $TARGET"
  PKG_DIR="$(cd -- "$(dirname -- "$TARGET")" && pwd -P)"
  PKG_BASE="$(basename -- "$TARGET")"
  DOCKER_MOUNTS+=(-v "${PKG_DIR}/${PKG_BASE}:/pkg/${PKG_BASE}:ro")
  DOCKER_ENV+=(-e "LEGION_TEST_MODE=package" -e "LEGION_PKG_FILE=/pkg/${PKG_BASE}")
  if [[ -n "${LEGION_EXPECT_BUILD_REV:-}" ]]; then
    DOCKER_ENV+=(-e "LEGION_EXPECT_BUILD_REV=${LEGION_EXPECT_BUILD_REV}")
  fi
fi

DOCKER_ENV+=(-e "LEGION_DISTRO_KIND=${DISTRO_KIND}" -e "LEGION_IMAGE=${IMAGE}")

printf '=== Testing %s (%s) ===\n' "$IMAGE" "$TARGET"

# Read the container test script into a variable and pass it via `bash -c` rather than stdin so
# commands inside the container (such as `ssh` or `su`) can never consume the script from stdin.
CONTAINER_SCRIPT="$(cat <<'CONTAINER_EOF'
set -euo pipefail

step() {
  printf '[%s] %s\n' "$LEGION_IMAGE" "$1"
}

fail() {
  printf '[%s] FAIL: %s\n' "$LEGION_IMAGE" "$1" >&2
  exit 1
}

run_as_me() {
  su -s /bin/bash me -c "$1" </dev/null
}

EXPECTED_MANAGER=""
EXPECTED_HINT_NEEDLE=""
REINSTALL_PKG=""

if [[ "$LEGION_DISTRO_KIND" == "deb" ]]; then
  EXPECTED_MANAGER="deb"
  EXPECTED_HINT_NEEDLE="apt-get install"
  export DEBIAN_FRONTEND=noninteractive

  # Prevent package postinst scripts from trying to start daemons inside the container.
  printf '#!/bin/sh\nexit 101\n' > /usr/sbin/policy-rc.d
  chmod 0755 /usr/sbin/policy-rc.d

  step "Installing test prerequisites (openssh-server, openssh-client, systemd, jq)"
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends \
    ca-certificates jq openssh-client openssh-server systemd >/dev/null

  step "Installing .deb with apt-get install"
  # List the archive once and search the text: `grep -q` leaving a pipe early kills the writer
  # with SIGPIPE, which `pipefail` then reports as a failure.
  PKG_CONTENTS="$(dpkg-deb --contents "$LEGION_PKG_FILE")"
  grep -q 'usr/share/man/man1/legion\.1\.gz$' <<<"$PKG_CONTENTS" || fail "package archive missing man page"
  grep -q 'usr/share/doc/legion/copyright$' <<<"$PKG_CONTENTS" || fail "package archive missing copyright"
  grep -q 'usr/share/doc/legion/changelog\.Debian\.gz$' <<<"$PKG_CONTENTS" || fail "package archive missing changelog"

  apt-get install -y -qq "$LEGION_PKG_FILE" >/dev/null
  REINSTALL_PKG="$LEGION_PKG_FILE"

  test -x /usr/bin/legion || fail "/usr/bin/legion not installed"
  test -f /usr/lib/legion/managed-by || fail "/usr/lib/legion/managed-by not installed"
  [[ "$(cat /usr/lib/legion/managed-by)" == "deb" ]] || fail "unexpected managed-by content"
  test -f /usr/lib/systemd/user/legion-serve.service || fail "systemd user unit not installed"
  test -f /usr/share/doc/legion/copyright || fail "copyright not installed"
  # Official minimized Ubuntu container images ship /etc/dpkg/dpkg.cfg.d/excludes which skips
  # extracting man pages and changelogs; verify them on disk whenever dpkg does not exclude them.
  if ! grep -qs 'path-exclude=/usr/share/man/\*' /etc/dpkg/dpkg.cfg.d/* 2>/dev/null; then
    test -f /usr/share/man/man1/legion.1.gz || fail "man page not installed"
  fi
  if ! grep -qs 'path-exclude=/usr/share/doc/\*' /etc/dpkg/dpkg.cfg.d/* 2>/dev/null; then
    test -f /usr/share/doc/legion/changelog.Debian.gz || fail "changelog.Debian.gz not installed"
  fi
  rm -f /usr/sbin/policy-rc.d

elif [[ "$LEGION_DISTRO_KIND" == "arch" ]]; then
  EXPECTED_MANAGER="pacman"
  EXPECTED_HINT_NEEDLE="pacman"

  step "Installing Arch prerequisites (base-devel, namcap, openssh, systemd, jq)"
  pacman -Syu --noconfirm --needed base-devel jq namcap openssh systemd >/dev/null

  if [[ "$LEGION_TEST_MODE" == "pkgbuild" ]]; then
    step "Installing rustup for PKGBUILD source build"
    pacman -S --noconfirm --needed rustup >/dev/null

    useradd -m -s /bin/bash builder
    mkdir -p /work/repo /work/out
    for rel in Cargo.toml Cargo.lock rust-toolchain.toml LICENSE crates contract packaging apps/ui; do
      [[ -e "/src/${rel}" ]] || fail "required source path missing in /src: ${rel}"
    done
    tar -C /src -cf - \
      Cargo.toml Cargo.lock rust-toolchain.toml LICENSE crates contract packaging apps/ui \
      | tar -C /work/repo -xf -
    chown -R builder:builder /work

    if [[ "${LEGION_HAS_CARGO_CACHE:-0}" == "1" ]]; then
      chown -R builder:builder /cargo-cache 2>/dev/null || true
    fi

    step "Checking PKGBUILD files with namcap"
    namcap /work/repo/packaging/arch/PKGBUILD
    namcap /work/repo/packaging/arch/legion-bin/PKGBUILD

    step "Building legion from source via makepkg as non-root user"
    su -s /bin/bash builder -c '
      set -euo pipefail
      if [[ "${LEGION_HAS_CARGO_CACHE:-0}" == "1" ]]; then
        export CARGO_HOME=/cargo-cache
      else
        export CARGO_HOME=/work/cargo
      fi
      cd /work/repo
      rustup toolchain install
      bash packaging/arch/build-local.sh source /work/out
    ' 2>&1 | tee /work/makepkg-source.log

    if grep -Fq 'Package contains reference to $srcdir' /work/makepkg-source.log; then
      fail "makepkg warned that the source package contains a reference to \$srcdir"
    fi

    BUILT_PKG="$(find /work/out -maxdepth 1 -type f -name 'legion-[0-9]*.pkg.tar*' ! -name '*-debug-*' | head -n 1)"
    [[ -n "$BUILT_PKG" ]] || fail "makepkg did not produce a legion package"

    step "Checking built source package with namcap"
    NAMCAP_OUT="$(namcap "$BUILT_PKG")"
    printf '%s\n' "$NAMCAP_OUT"
    if grep -q ' E: ' <<<"$NAMCAP_OUT"; then
      fail "namcap reported errors on source package"
    fi

    if [[ "${LEGION_TEST_BIN_PKG:-0}" == "1" ]]; then
      step "Building legion-bin via makepkg and checking with namcap"
      STATIC_VER="$(/static-bin/legion --version)"
      printf '  static binary version: %s\n' "$STATIC_VER"
      [[ "$STATIC_VER" =~ ^legion[[:space:]][0-9][^[:space:]]*[[:space:]]\([0-9a-f]{7,} ]] \
        || fail "static binary for legion-bin missing build revision: $STATIC_VER"
      mkdir -p /work/out-bin
      chown builder:builder /work/out-bin
      su -s /bin/bash builder -c '
        set -euo pipefail
        cd /work/repo
        bash packaging/arch/build-local.sh bin /static-bin/legion /work/out-bin
      '
      BUILT_BIN_PKG="$(find /work/out-bin -maxdepth 1 -type f -name 'legion-bin-*.pkg.tar*' | head -n 1)"
      [[ -n "$BUILT_BIN_PKG" ]] || fail "makepkg did not produce a legion-bin package"
      NAMCAP_BIN_OUT="$(namcap "$BUILT_BIN_PKG")"
      printf '%s\n' "$NAMCAP_BIN_OUT"
      if grep -q ' E: ' <<<"$NAMCAP_BIN_OUT"; then
        fail "namcap reported errors on legion-bin package"
      fi
    fi

    step "Installing built package with pacman -U"
    pacman -U --noconfirm "$BUILT_PKG" >/dev/null
    REINSTALL_PKG="$BUILT_PKG"

    step "Verifying built Arch binary contains no build directory or cargo home paths"
    BUILDER_HOME="$(getent passwd builder | cut -d: -f6)"
    for forbidden in /work/ /cargo-cache "$BUILDER_HOME"; do
      if [[ -n "$forbidden" ]] && strings /usr/bin/legion | grep -Fq "$forbidden"; then
        fail "/usr/bin/legion contains build host path: $forbidden"
      fi
    done
  else
    step "Installing package with pacman -U"
    pacman -U --noconfirm "$LEGION_PKG_FILE" >/dev/null
    REINSTALL_PKG="$LEGION_PKG_FILE"
  fi

  test -x /usr/bin/legion || fail "/usr/bin/legion not installed"
  test -f /usr/lib/legion/managed-by || fail "/usr/lib/legion/managed-by not installed"
  [[ "$(cat /usr/lib/legion/managed-by)" == "pacman" ]] || fail "unexpected managed-by content"
  test -f /usr/lib/systemd/user/legion-serve.service || fail "systemd user unit not installed"
  PACMAN_FILES="$(pacman -Ql legion)"
  grep -q '/usr/share/man/man1/legion\.1\.gz$' <<<"$PACMAN_FILES" || fail "package manifest missing man page"
  # Official archlinux container images set NoExtract for usr/share/man/* in /etc/pacman.conf;
  # verify the man page on disk whenever pacman does not exclude it.
  if ! grep -Eq '^[[:space:]]*NoExtract[[:space:]]*=.*usr/share/man/' /etc/pacman.conf 2>/dev/null; then
    test -f /usr/share/man/man1/legion.1.gz || fail "man page not installed"
  fi
  test -f /usr/share/licenses/legion/LICENSE || fail "license not installed"
fi

step "Creating normal user me"
if ! id me >/dev/null 2>&1; then
  useradd -m -s /bin/bash me
fi
# Unlock the account for SSH pubkey login (some distros lock new accounts with '!').
usermod -p '*' me

step "Running legion --version, init, meta.version, meta.doctor as me"
VER_LINE="$(run_as_me 'legion --version')"
printf '  version: %s\n' "$VER_LINE"
[[ "$VER_LINE" =~ ^legion[[:space:]][0-9] ]] || fail "unexpected --version output: $VER_LINE"
if [[ "${LEGION_EXPECT_BUILD_REV:-1}" != "0" ]]; then
  [[ "$VER_LINE" =~ ^legion[[:space:]][0-9][^[:space:]]*[[:space:]]\([0-9a-f]{7,} ]] \
    || fail "--version did not include a build revision: $VER_LINE"
fi

INIT_JSON="$(run_as_me 'legion init --name tower --json')"
printf '%s' "$INIT_JSON" | jq -e '.node == "tower"' >/dev/null || fail "init failed: $INIT_JSON"

META_VER_JSON="$(run_as_me 'legion meta.version --json')"
printf '%s' "$META_VER_JSON" | jq -e --arg mgr "$EXPECTED_MANAGER" \
  '.name == "legion" and .node == "tower" and .install == $mgr' >/dev/null \
  || fail "meta.version did not report install=$EXPECTED_MANAGER: $META_VER_JSON"
if [[ "${LEGION_EXPECT_BUILD_REV:-1}" != "0" ]]; then
  printf '%s' "$META_VER_JSON" | jq -e \
    '(.build | type == "string") and (.build | test("^[0-9a-f]{7,}"))' >/dev/null \
    || fail "meta.version did not report a build revision: $META_VER_JSON"
fi

DOCTOR_JSON="$(run_as_me 'legion meta.doctor --json')"
printf '%s' "$DOCTOR_JSON" | jq -e --arg mgr "$EXPECTED_MANAGER" \
  '.ok == true and (.checks | any(.name == "installation" and (.detail | contains($mgr))))' >/dev/null \
  || fail "meta.doctor failed or omitted installation check: $DOCTOR_JSON"

step "Running real tools (sys.info, fs.list, exec.run, detached job)"
SYS_JSON="$(run_as_me 'legion sys.info --json')"
printf '%s' "$SYS_JSON" | jq -e '(.hostname | length > 0) and (.os != null)' >/dev/null \
  || fail "sys.info failed: $SYS_JSON"

FS_JSON="$(run_as_me 'legion fs.list path=/home/me --json')"
printf '%s' "$FS_JSON" | jq -e '.entries | any(.name == ".config")' >/dev/null \
  || fail "fs.list failed: $FS_JSON"

EXEC_JSON="$(run_as_me 'legion exec.run cmd="printf distro-ok" --json')"
printf '%s' "$EXEC_JSON" | jq -e '.exit_code == 0 and .stdout == "distro-ok"' >/dev/null \
  || fail "exec.run failed: $EXEC_JSON"

JOB_JSON="$(run_as_me 'legion job.start cmd="echo first; while [ ! -f /home/me/.job-gate ]; do sleep 0.1; done; echo second" name=distro-job --json')"
JOB_ID="$(printf '%s' "$JOB_JSON" | jq -r '.id')"
[[ -n "$JOB_ID" && "$JOB_ID" != "null" ]] || fail "job.start failed: $JOB_JSON"

GOT_FIRST=0
for _ in $(seq 1 40); do
  LOGS_JSON="$(run_as_me "legion job.logs id=${JOB_ID} --json")"
  if printf '%s' "$LOGS_JSON" | jq -e '.content | contains("first")' >/dev/null; then
    GOT_FIRST=1
    break
  fi
  sleep 0.1
done
[[ "$GOT_FIRST" -eq 1 ]] || fail "job.logs never showed first line"

STATUS_JSON="$(run_as_me "legion job.status id=${JOB_ID} --json")"
printf '%s' "$STATUS_JSON" | jq -e '.state == "running" or .state == "starting"' >/dev/null \
  || fail "detached job did not outlive job.start: $STATUS_JSON"

run_as_me 'touch /home/me/.job-gate'
WAIT_JSON="$(run_as_me "legion job.wait id=${JOB_ID} timeout_s=30 --json")"
printf '%s' "$WAIT_JSON" | jq -e '.state == "succeeded" and .exit_code == 0 and .timed_out == false' >/dev/null \
  || fail "job.wait failed: $WAIT_JSON"

FINAL_LOGS="$(run_as_me "legion job.logs id=${JOB_ID} --json")"
printf '%s' "$FINAL_LOGS" | jq -e '(.content | contains("first")) and (.content | contains("second"))' >/dev/null \
  || fail "job.logs missing expected output: $FINAL_LOGS"

step "Starting loopback OpenSSH server and testing real SSH fleet calls + legion cp"
mkdir -p /run/sshd /etc/ssh
chmod 0755 /run/sshd
if [[ ! -f /etc/ssh/ssh_host_ed25519_key ]]; then
  ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key
fi
cat > /etc/ssh/sshd_config_legion_test <<'SSHD_EOF'
ListenAddress 127.0.0.1
Port 2222
HostKey /etc/ssh/ssh_host_ed25519_key
PidFile /run/sshd-legion.pid
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin no
UsePAM no
AllowUsers me
Subsystem sftp internal-sftp
SSHD_EOF

SSHD_BIN="$(command -v sshd || printf '/usr/sbin/sshd')"
"$SSHD_BIN" -t -f /etc/ssh/sshd_config_legion_test
"$SSHD_BIN" -f /etc/ssh/sshd_config_legion_test -E /var/log/sshd-legion.log
stop_sshd() {
  if [[ -f /run/sshd-legion.pid ]]; then
    kill "$(cat /run/sshd-legion.pid)" 2>/dev/null || true
  fi
}
trap stop_sshd EXIT

HOST_PUB="$(awk '{print $1 " " $2}' /etc/ssh/ssh_host_ed25519_key.pub)"
run_as_me "
  set -euo pipefail
  mkdir -p ~/.ssh
  chmod 700 ~/.ssh
  ssh-keygen -q -t ed25519 -N '' -f ~/.ssh/id_ed25519
  cp ~/.ssh/id_ed25519.pub ~/.ssh/authorized_keys
  chmod 600 ~/.ssh/authorized_keys
  printf '[127.0.0.1]:2222 %s\n' '$HOST_PUB' > ~/.ssh/known_hosts
  chmod 600 ~/.ssh/known_hosts
  cat > ~/.ssh/config <<'SSH_CFG'
Host pi
  HostName 127.0.0.1
  Port 2222
  User me
  IdentityFile ~/.ssh/id_ed25519
  IdentitiesOnly yes
  StrictHostKeyChecking yes
SSH_CFG
  chmod 600 ~/.ssh/config
  ssh -T -o BatchMode=yes pi legion --version >/dev/null
"

ADD_JSON="$(run_as_me 'legion fleet.add name=pi ssh=pi --json')"
printf '%s' "$ADD_JSON" | jq -e '.added == true' >/dev/null || fail "fleet.add failed: $ADD_JSON"

REMOTE_SYS="$(run_as_me 'legion sys.info --on pi --json')"
printf '%s' "$REMOTE_SYS" | jq -e '(.hostname | length > 0) and (.os != null)' >/dev/null \
  || fail "sys.info --on pi failed: $REMOTE_SYS"

run_as_me '
  set -euo pipefail
  printf "hello across openssh on %s\n" "'"$LEGION_IMAGE"'" > /home/me/cp-src.txt
  legion cp /home/me/cp-src.txt pi:/home/me/cp-dst.txt --json | jq -e ".files_sent == 1" >/dev/null
  legion cp pi:/home/me/cp-dst.txt /home/me/cp-back.txt --json | jq -e ".files_sent == 1" >/dev/null
  cmp -s /home/me/cp-src.txt /home/me/cp-back.txt
'

step "Verifying fleet.install refuses package-managed remote machine"
set +e
FLEET_INSTALL_OUT="$(run_as_me 'legion fleet.install machine=pi dry_run=true --json')"
FLEET_INSTALL_RC=$?
set -e
[[ "$FLEET_INSTALL_RC" -eq 1 ]] || fail "fleet.install unexpectedly exited $FLEET_INSTALL_RC: $FLEET_INSTALL_OUT"
printf '%s' "$FLEET_INSTALL_OUT" | jq -e --arg mgr "$EXPECTED_MANAGER" --arg needle "$EXPECTED_HINT_NEEDLE" \
  '.code == "unsupported" and .details.managed_by == $mgr and (.hint | contains($needle))' >/dev/null \
  || fail "fleet.install refusal did not carry expected code/hint: $FLEET_INSTALL_OUT"

step "Verifying systemd user unit with systemd-analyze verify --user"
if command -v systemd-analyze >/dev/null 2>&1; then
  mkdir -p /run/user/0
  chmod 0700 /run/user/0
  XDG_RUNTIME_DIR=/run/user/0 systemd-analyze verify --user --man=no /usr/lib/systemd/user/legion-serve.service
else
  fail "systemd-analyze not available in container"
fi

step "Verifying meta.update refuses with package hint"
set +e
UPDATE_OUT="$(run_as_me 'legion meta.update dry_run=true --json')"
UPDATE_RC=$?
set -e
[[ "$UPDATE_RC" -eq 1 ]] || fail "meta.update unexpectedly exited $UPDATE_RC: $UPDATE_OUT"
printf '%s' "$UPDATE_OUT" | jq -e --arg mgr "$EXPECTED_MANAGER" --arg needle "$EXPECTED_HINT_NEEDLE" \
  '.code == "unsupported" and .details.managed_by == $mgr and (.hint | contains($needle))' >/dev/null \
  || fail "meta.update refusal did not carry expected code/hint: $UPDATE_OUT"

set +e
UPDATE_FROM_OUT="$(run_as_me 'legion meta.update from=/usr/bin/legion --json')"
UPDATE_FROM_RC=$?
set -e
[[ "$UPDATE_FROM_RC" -eq 1 ]] || fail "meta.update from= unexpectedly exited $UPDATE_FROM_RC: $UPDATE_FROM_OUT"
printf '%s' "$UPDATE_FROM_OUT" | jq -e --arg mgr "$EXPECTED_MANAGER" \
  '.code == "unsupported" and .details.managed_by == $mgr' >/dev/null \
  || fail "meta.update from= refusal did not carry expected code: $UPDATE_FROM_OUT"

step "Upgrading (reinstalling) and then removing package"
NODE_SHA_BEFORE="$(sha256sum /home/me/.config/legion/node.toml | awk '{print $1}')"
POLICY_SHA_BEFORE="$(sha256sum /home/me/.config/legion/policy.toml | awk '{print $1}')"

if [[ "$LEGION_DISTRO_KIND" == "deb" ]]; then
  apt-get install -y -qq --reinstall "$REINSTALL_PKG" >/dev/null
  run_as_me 'legion --version' >/dev/null
  apt-get remove -y -qq legion >/dev/null
  test ! -e /usr/share/doc/legion/copyright || fail "copyright remained after remove"
  test ! -e /usr/share/doc/legion/changelog.Debian.gz || fail "changelog remained after remove"
else
  pacman -U --noconfirm "$REINSTALL_PKG" >/dev/null
  run_as_me 'legion --version' >/dev/null
  pacman -R --noconfirm legion >/dev/null
  test ! -e /usr/share/licenses/legion/LICENSE || fail "license remained after remove"
fi

test ! -e /usr/bin/legion || fail "/usr/bin/legion remained after remove"
test ! -e /usr/lib/legion/managed-by || fail "/usr/lib/legion/managed-by remained after remove"
test ! -e /usr/lib/systemd/user/legion-serve.service || fail "systemd user unit remained after remove"
test ! -e /usr/share/man/man1/legion.1.gz || fail "man page remained after remove"

NODE_SHA_AFTER="$(sha256sum /home/me/.config/legion/node.toml | awk '{print $1}')"
POLICY_SHA_AFTER="$(sha256sum /home/me/.config/legion/policy.toml | awk '{print $1}')"
[[ "$NODE_SHA_BEFORE" == "$NODE_SHA_AFTER" ]] || fail "node.toml changed across upgrade/remove"
[[ "$POLICY_SHA_BEFORE" == "$POLICY_SHA_AFTER" ]] || fail "policy.toml changed across upgrade/remove"

step "All checks passed"
CONTAINER_EOF
)"

"${DOCKER_CMD[@]}" run --rm \
  "${DOCKER_ENV[@]}" \
  "${DOCKER_MOUNTS[@]}" \
  "$IMAGE" bash -c "$CONTAINER_SCRIPT"
