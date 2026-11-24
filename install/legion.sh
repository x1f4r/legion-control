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

# Pinned Ed25519 public key from contract/release-public-key.pem (same key pinned in legion).
RELEASE_PUBLIC_KEY_PEM='-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAB8q1DNamFF0ShMi6Wzps/0UIGEkLmHDDvQqOvuv98zg=
-----END PUBLIC KEY-----'

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

die() {
  printf 'install/legion.sh: %s\n' "$1" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: install/legion.sh [--version <version>] [--from <file>] [--insecure-skip-verify]

Install the legion binary to ~/.local/bin/legion.

Options:
  --version <ver>         Install a specific release version (for example 0.1.0 or legion-v0.1.0).
  --from <file>           Install a legion binary already on disk instead of downloading.
  --insecure-skip-verify  Skip release provenance/signature verification (SHA-256 is still checked).
  -h, --help              Show this help message.
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

require_hardened_wget() {
  wget_help="$(wget --help 2>&1 || true)"
  case "$wget_help" in
    *--https-only*--secure-protocol*|*--secure-protocol*--https-only*) ;;
    *)
      die "This wget does not support --https-only and --secure-protocol (for example BusyBox wget); install curl or GNU wget, or pass --from <file>."
      ;;
  esac
}

fetch_to_file() {
  url="$1"
  out_path="$2"
  if command -v curl >/dev/null 2>&1; then
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 \
      --fail --silent --show-error --location \
      --retry 2 --connect-timeout 15 --max-time 120 \
      -H 'User-Agent: legion-installer' \
      -o "$out_path" "$url"
  elif command -v wget >/dev/null 2>&1; then
    require_hardened_wget
    wget -q --https-only --secure-protocol=TLSv1_2 --timeout=15 --tries=3 \
      -U 'legion-installer' -O "$out_path" "$url"
  else
    die "Neither curl nor wget is installed; install one of them or pass --from <file>."
  fi
}

fetch_text() {
  url="$1"
  if command -v curl >/dev/null 2>&1; then
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 \
      --fail --silent --show-error --location \
      --retry 2 --connect-timeout 15 --max-time 30 \
      -H 'Accept: application/vnd.github+json' \
      -H 'User-Agent: legion-installer' \
      "$url"
  elif command -v wget >/dev/null 2>&1; then
    require_hardened_wget
    wget -qO- --https-only --secure-protocol=TLSv1_2 --timeout=15 --tries=3 \
      --header='Accept: application/vnd.github+json' \
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

openssl_supports_ed25519_rawin() {
  pub_pem="$1"
  command -v openssl >/dev/null 2>&1 || return 1
  case "$(openssl version 2>/dev/null || true)" in
    OpenSSL*) ;;
    *) return 1 ;;
  esac
  openssl pkey -pubin -in "$pub_pem" -noout >/dev/null 2>&1 || return 1
  openssl pkeyutl -help 2>&1 | grep -q -- '-rawin' || return 1
}

decode_base64_file() {
  in_path="$1"
  out_path="$2"
  if command -v base64 >/dev/null 2>&1; then
    if base64 -d "$in_path" >"$out_path" 2>/dev/null; then
      return 0
    fi
    base64 -D -i "$in_path" -o "$out_path" 2>/dev/null && return 0
  fi
  openssl base64 -d -A -in "$in_path" -out "$out_path" 2>/dev/null
}

verify_release_trust() {
  bin_path="$1"
  sums_path="$2"
  base_url="$3"
  tag="$4"

  if [ "$INSECURE_SKIP_VERIFY" -eq 1 ]; then
    printf 'Warning: skipping release provenance/signature verification (--insecure-skip-verify); SHA-256 was still verified.\n' >&2
    return 0
  fi

  if command -v gh >/dev/null 2>&1; then
    gh attestation verify "$bin_path" \
      --repo "$REPO_SLUG" \
      --signer-workflow "${REPO_SLUG}/.github/workflows/legion-release.yml" \
      --source-ref "refs/tags/${tag}" >/dev/null \
      || die "GitHub build provenance verification failed for ${tag}; refusing to install."
    return 0
  fi

  pub_pem="${TMP_DIR}/release-public-key.pem"
  printf '%s\n' "$RELEASE_PUBLIC_KEY_PEM" >"$pub_pem"

  if openssl_supports_ed25519_rawin "$pub_pem"; then
    sig_b64="${TMP_DIR}/SHA256SUMS.sig"
    sig_clean="${TMP_DIR}/SHA256SUMS.sig.clean"
    sig_raw="${TMP_DIR}/SHA256SUMS.sig.bin"
    fetch_to_file "${base_url}/SHA256SUMS.sig" "$sig_b64" \
      || die "Failed to download ${base_url}/SHA256SUMS.sig; refusing unsigned release."
    tr -d ' \t\r\n' <"$sig_b64" >"$sig_clean"
    decode_base64_file "$sig_clean" "$sig_raw" \
      || die "SHA256SUMS.sig is not valid base64; refusing to install."
    raw_len="$(wc -c <"$sig_raw" | tr -d '[:space:]')"
    if [ "$raw_len" -ne 64 ]; then
      die "SHA256SUMS.sig did not decode to a 64-byte Ed25519 signature."
    fi
    openssl pkeyutl -verify -pubin -inkey "$pub_pem" -rawin \
      -in "$sums_path" -sigfile "$sig_raw" >/dev/null 2>&1 \
      || die "Ed25519 signature verification failed for ${tag}; refusing to install."
    return 0
  fi

  cat >&2 <<EOF
install/legion.sh: cannot verify who published ${tag}: neither 'gh' (for attestation verification)
nor OpenSSL with Ed25519 -rawin support is installed (macOS ships LibreSSL).

Verify manually before installing:
  1. Install GitHub CLI (gh) and re-run this script, or verify the binary with:
       gh attestation verify <file> --repo ${REPO_SLUG} \\
         --signer-workflow ${REPO_SLUG}/.github/workflows/legion-release.yml \\
         --source-ref refs/tags/${tag}
  2. Or install OpenSSL 3 (e.g. 'brew install openssl') to verify SHA256SUMS.sig offline
     against the pinned Ed25519 public key in contract/release-public-key.pem.
  3. Or re-run with --insecure-skip-verify to install with SHA-256 verification only.
EOF
  exit 1
}

main() {
  trap cleanup EXIT HUP INT TERM

  VERSION=''
  FROM_FILE=''
  INSECURE_SKIP_VERIFY=0

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
      --insecure-skip-verify)
        INSECURE_SKIP_VERIFY=1
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

  if [ -e "$INSTALL_DIR" ] && [ ! -d "$INSTALL_DIR" ]; then
    die "$INSTALL_DIR exists and is not a directory."
  fi
  if [ -d "$DEST" ]; then
    die "$DEST is a directory; refusing to overwrite it."
  fi

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
    verify_release_trust "$DOWNLOADED_BIN" "$SUMS_FILE" "$BASE_URL" "$TAG"
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
}

main "$@"
