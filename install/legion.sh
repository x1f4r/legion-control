#!/bin/sh
# Install the legion binary into ~/.local/bin/legion without requiring root.
#
# Usage:
#   sh install/legion.sh [--version <version>] [--from <file>]
#
# By default, downloads the latest published legion-v* release matching this machine's OS and
# architecture, verifies its SHA-256 checksum against SHA256SUMS, and installs atomically.
# Pass --from <file> to install a binary already on disk (air-gapped installs and CI).
set -eu

REPO_URL='https://github.com/x1f4r/legion-control'
REPO_SLUG="${REPO_URL#https://github.com/}"

TMP_DIR=''
STAGED_FILE=''

cleanup() {
  if [ -n "$STAGED_FILE" ] && [ -e "$STAGED_FILE" ]; then
    rm -f "$STAGED_FILE"
  fi
  if [ -n "$TMP_DIR" ] && [ -d "$TMP_DIR" ]; then
    rm -rf "$TMP_DIR"
  fi
}

trap cleanup EXIT HUP INT TERM

die() {
  printf 'install/legion.sh: %s\n' "$1" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: install/legion.sh [--version <version>] [--from <file>]

Install the legion binary to ~/.local/bin/legion.

Options:
  --version <ver>   Install a specific release version (for example 0.1.0 or legion-v0.1.0).
  --from <file>     Install a legion binary already on disk instead of downloading.
  -h, --help        Show this help message.
EOF
}

detect_target() {
  os_name="$(uname -s)"
  arch_name="$(uname -m)"
  case "$os_name" in
    Linux)
      case "$arch_name" in
        x86_64|amd64) printf '%s' 'x86_64-unknown-linux-musl' ;;
        aarch64|arm64) printf '%s' 'aarch64-unknown-linux-musl' ;;
        armv7*|armv6*|arm|armhf|armel)
          die "32-bit ARM ($arch_name) is not supported; Legion requires 64-bit Linux (x86_64 or aarch64)."
          ;;
        *)
          die "Unsupported Linux architecture: $arch_name (supported: x86_64, aarch64)."
          ;;
      esac
      ;;
    Darwin)
      case "$arch_name" in
        arm64|aarch64) printf '%s' 'aarch64-apple-darwin' ;;
        x86_64) printf '%s' 'x86_64-apple-darwin' ;;
        *)
          die "Unsupported macOS architecture: $arch_name (supported: arm64, x86_64)."
          ;;
      esac
      ;;
    *)
      die "Unsupported operating system: $os_name (use install/legion.ps1 on Windows)."
      ;;
  esac
}

normalize_version() {
  raw="$1"
  raw="${raw#legion-v}"
  raw="${raw#v}"
  if [ -z "$raw" ]; then
    die "Version cannot be empty."
  fi
  case "$raw" in
    *[!0-9A-Za-z._+-]*)
      die "Invalid version '$1': only letters, digits, '.', '_', '+', and '-' are allowed."
      ;;
  esac
  printf '%s' "$raw"
}

fetch_to_file() {
  url="$1"
  out_path="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 2 --connect-timeout 15 \
      -H 'User-Agent: legion-installer' \
      -o "$out_path" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -q --timeout=15 -U 'legion-installer' -O "$out_path" "$url"
  else
    die "Neither curl nor wget is installed; install one of them or pass --from <file>."
  fi
}

fetch_text() {
  url="$1"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 2 --connect-timeout 15 \
      -H 'Accept: application/vnd.github+json' \
      -H 'User-Agent: legion-installer' \
      "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- --timeout=15 --header='Accept: application/vnd.github+json' \
      -U 'legion-installer' "$url"
  else
    die "Neither curl nor wget is installed; install one of them or pass --from <file>."
  fi
}

# Select the newest published (non-draft, non-prerelease) legion-v* release.
# The repository also carries non-legion releases under v* tags, so /releases/latest cannot be used.
resolve_latest_version() {
  api_url="https://api.github.com/repos/${REPO_SLUG}/releases?per_page=30"
  releases_json="$(fetch_text "$api_url")" || die "Could not query GitHub releases at $api_url."
  tag="$(printf '%s\n' "$releases_json" | tr ',' '\n' | awk '
    /"tag_name"[[:space:]]*:/ {
      if (tag != "" && draft == "false" && pre == "false") {
        print tag
        found = 1
        exit
      }
      tag = ""; draft = ""; pre = ""
      line = $0
      sub(/.*"tag_name"[[:space:]]*:[[:space:]]*"/, "", line)
      sub(/".*/, "", line)
      if (index(line, "legion-v") == 1) {
        tag = line
      }
    }
    /"draft"[[:space:]]*:/ {
      if (tag != "" && draft == "") {
        line = $0
        sub(/.*"draft"[[:space:]]*:[[:space:]]*/, "", line)
        sub(/[^a-z].*/, "", line)
        draft = line
      }
    }
    /"prerelease"[[:space:]]*:/ {
      if (tag != "" && pre == "") {
        line = $0
        sub(/.*"prerelease"[[:space:]]*:[[:space:]]*/, "", line)
        sub(/[^a-z].*/, "", line)
        pre = line
      }
    }
    END {
      if (!found && tag != "" && draft == "false" && pre == "false") {
        print tag
      }
    }
  ')"
  if [ -z "$tag" ]; then
    die "No published legion-v* release was found at ${REPO_URL}/releases."
  fi
  normalize_version "$tag"
}

sha256_of() {
  file_path="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file_path" | awk '{print $1}' | tr 'A-F' 'a-f'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file_path" | awk '{print $1}' | tr 'A-F' 'a-f'
  else
    die "Neither sha256sum nor shasum is installed to verify the downloaded binary."
  fi
}

verify_checksum() {
  bin_path="$1"
  sums_path="$2"
  asset_name="$3"

  expected="$(awk -v name="$asset_name" '
    {
      sub(/\r$/, "")
      if ($0 == "" || substr($0, 1, 1) == "#") next
      hash = $1
      file = $2
      sub(/^\*/, "", file)
      if (file == name) {
        print tolower(hash)
        exit
      }
    }
  ' "$sums_path")"

  if [ -z "$expected" ]; then
    die "SHA256SUMS does not contain an entry for $asset_name."
  fi
  case "$expected" in
    *[!0-9a-f]*)
      die "SHA256SUMS entry for $asset_name is not a valid SHA-256 hex digest."
      ;;
  esac
  if [ "${#expected}" -ne 64 ]; then
    die "SHA256SUMS entry for $asset_name is not a 64-character SHA-256 digest."
  fi

  actual="$(sha256_of "$bin_path")"
  if [ "$actual" != "$expected" ]; then
    die "Checksum mismatch for $asset_name (expected $expected, got $actual)."
  fi
}

VERSION=''
FROM_FILE=''

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      [ "$#" -ge 2 ] || die "--version requires an argument."
      VERSION="$2"
      shift 2
      ;;
    --version=*)
      VERSION="${1#--version=}"
      shift
      ;;
    --from)
      [ "$#" -ge 2 ] || die "--from requires a file path."
      FROM_FILE="$2"
      shift 2
      ;;
    --from=*)
      FROM_FILE="${1#--from=}"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "Unknown argument: $1 (run with --help for usage)."
      ;;
  esac
done

if [ -n "$FROM_FILE" ] && [ -n "$VERSION" ]; then
  die "Pass either --from or --version, not both."
fi

if [ -z "${HOME:-}" ]; then
  die "HOME is not set; cannot determine ~/.local/bin."
fi

INSTALL_DIR="${HOME}/.local/bin"
DEST="${INSTALL_DIR}/legion"

if [ -n "$FROM_FILE" ]; then
  if [ ! -f "$FROM_FILE" ]; then
    die "Source file does not exist or is not a regular file: $FROM_FILE"
  fi
  if [ ! -s "$FROM_FILE" ]; then
    die "Source file is empty: $FROM_FILE"
  fi
  SOURCE_BIN="$FROM_FILE"
else
  TARGET="$(detect_target)"
  if [ -n "$VERSION" ]; then
    VERSION="$(normalize_version "$VERSION")"
  else
    VERSION="$(resolve_latest_version)"
  fi
  TAG="legion-v${VERSION}"
  ASSET="legion-${VERSION}-${TARGET}"
  BASE_URL="${REPO_URL}/releases/download/${TAG}"

  TMP_DIR="$(mktemp -d)"
  SUMS_FILE="${TMP_DIR}/SHA256SUMS"
  DOWNLOADED_BIN="${TMP_DIR}/${ASSET}"

  fetch_to_file "${BASE_URL}/SHA256SUMS" "$SUMS_FILE" \
    || die "Failed to download ${BASE_URL}/SHA256SUMS."
  fetch_to_file "${BASE_URL}/${ASSET}" "$DOWNLOADED_BIN" \
    || die "Failed to download ${BASE_URL}/${ASSET}."

  verify_checksum "$DOWNLOADED_BIN" "$SUMS_FILE" "$ASSET"
  SOURCE_BIN="$DOWNLOADED_BIN"
fi

# Stage beside the destination so mv is an atomic rename on the same filesystem, leaving any
# existing installation untouched if copying or setting permissions fails.
mkdir -p "$INSTALL_DIR"
STAGED_FILE="$(mktemp "${INSTALL_DIR}/.legion.tmp.XXXXXX")"
cp "$SOURCE_BIN" "$STAGED_FILE"
chmod 0755 "$STAGED_FILE"
mv -f "$STAGED_FILE" "$DEST"
STAGED_FILE=''

INSTALLED_VERSION="$("$DEST" --version)" \
  || die "Installed $DEST, but running it with --version failed."
printf 'Installed %s to %s\n' "$INSTALLED_VERSION" "$DEST"

case ":${PATH:-}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    printf '\nNote: %s is not on your PATH.\n' "$INSTALL_DIR"
    printf 'Add it to your shell profile so legion can be run by name:\n'
    printf '  export PATH="$HOME/.local/bin:$PATH"\n'
    ;;
esac
