#!/bin/sh
# Run a package or tool download command and retry only on transient network,
# mirror, HTTP 429/5xx, or apt stale-index fetch failures. Real packaging,
# compilation, and local failures exit immediately on the first attempt.
#
# POSIX sh only so it runs unmodified inside minimal debian:*, ubuntu:*, and
# archlinux:* verification containers as well as on Linux, macOS, and Windows
# Git Bash runners.
set -eu

usage() {
  cat >&2 <<'EOF'
Usage: retry-fetch.sh [--attempts N] [--delays CSV] [--quiet] [--apt-update] [--verify-cmd NAME] [--] <command...>
EOF
  exit 2
}

attempts=4
delays_csv="10,30,60"
quiet=0
force_apt_update=0
verify_cmd=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --attempts)
      [ "$#" -ge 2 ] || usage
      attempts="$2"
      shift 2
      ;;
    --delays)
      [ "$#" -ge 2 ] || usage
      delays_csv="$2"
      shift 2
      ;;
    --quiet|-q)
      quiet=1
      shift
      ;;
    --apt-update)
      force_apt_update=1
      shift
      ;;
    --verify-cmd)
      [ "$#" -ge 2 ] || usage
      verify_cmd="$2"
      shift 2
      ;;
    --)
      shift
      break
      ;;
    -h|--help)
      usage
      ;;
    -*)
      printf 'retry-fetch: unknown option: %s\n' "$1" >&2
      usage
      ;;
    *)
      break
      ;;
  esac
done

[ "$#" -gt 0 ] || usage

case "$attempts" in
  ''|*[!0-9]*)
    printf 'retry-fetch: --attempts must be a positive integer (got %s)\n' "'$attempts'" >&2
    exit 2
    ;;
esac
if [ "$attempts" -lt 1 ]; then
  printf 'retry-fetch: --attempts must be at least 1 (got %s)\n' "'$attempts'" >&2
  exit 2
fi

case "$delays_csv" in
  ''|,*|*,|*,,*)
    printf 'retry-fetch: --delays must be a comma-separated list of non-negative integers (got %s)\n' "'$delays_csv'" >&2
    exit 2
    ;;
esac

old_ifs="$IFS"
IFS=','
set -f
for d in $delays_csv; do
  case "$d" in
    ''|*[!0-9]*)
      set +f
      IFS="$old_ifs"
      printf 'retry-fetch: delay must be a non-negative integer in seconds (got %s)\n' "'$d'" >&2
      exit 2
      ;;
  esac
done
set +f
IFS="$old_ifs"

# Detect apt / apt-get (optionally prefixed with sudo) so every apt invocation
# shares Acquire::Retries=3, apt update treats exit-0 stale-index warnings as a
# retryable failure, and apt install refreshes indexes before retrying.
apt_sudo=""
apt_bin=""
apt_subcmd=""
cmd1_base="$(basename -- "$1")"
if [ "$cmd1_base" = "sudo" ] && [ "$#" -ge 2 ]; then
  cmd2_base="$(basename -- "$2")"
  if [ "$cmd2_base" = "apt-get" ] || [ "$cmd2_base" = "apt" ]; then
    apt_sudo="$1"
    apt_bin="$2"
  fi
elif [ "$cmd1_base" = "apt-get" ] || [ "$cmd1_base" = "apt" ]; then
  apt_bin="$1"
fi

if [ -n "$apt_bin" ]; then
  has_acquire_retries=0
  for arg in "$@"; do
    case "$arg" in
      *Acquire::Retries*)
        has_acquire_retries=1
        break
        ;;
    esac
  done
  if [ "$has_acquire_retries" -eq 0 ]; then
    if [ -n "$apt_sudo" ]; then
      shift 2
      set -- "$apt_sudo" "$apt_bin" -o Acquire::Retries=3 "$@"
    else
      shift 1
      set -- "$apt_bin" -o Acquire::Retries=3 "$@"
    fi
  fi

  skip_count=1
  if [ -n "$apt_sudo" ]; then
    skip_count=2
  fi
  arg_idx=0
  expect_opt_val=0
  for arg in "$@"; do
    arg_idx=$((arg_idx + 1))
    if [ "$arg_idx" -le "$skip_count" ]; then
      continue
    fi
    if [ "$expect_opt_val" -eq 1 ]; then
      expect_opt_val=0
      continue
    fi
    case "$arg" in
      -o|-c|-t)
        expect_opt_val=1
        ;;
      -*)
        ;;
      *)
        apt_subcmd="$arg"
        break
        ;;
    esac
  done
fi

is_apt_update="$force_apt_update"
if [ "$apt_subcmd" = "update" ]; then
  is_apt_update=1
fi

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/retry-fetch.XXXXXX")"
tmp_log="$tmp_dir/output.log"
rc_file="$tmp_dir/rc"

cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

delay_for_retry() {
  target="$1"
  old_ifs="$IFS"
  IFS=','
  set -f
  idx=0
  last=0
  for d in $delays_csv; do
    idx=$((idx + 1))
    last="$d"
    if [ "$idx" -eq "$target" ]; then
      set +f
      IFS="$old_ifs"
      printf '%s\n' "$d"
      return 0
    fi
  done
  set +f
  IFS="$old_ifs"
  printf '%s\n' "$last"
}

is_non_fetch_failure() {
  file="$1"
  grep -Eqi \
    -e 'dpkg: error processing' \
    -e 'sub-process /usr/bin/dpkg returned an error code' \
    -e 'the following packages have unmet dependencies' \
    -e 'unable to correct problems, you have held broken packages' \
    -e 'unable to locate package' \
    -e 'has no installation candidate' \
    -e 'could not satisfy dependencies' \
    -e 'exists in filesystem' \
    -e 'failed to commit transaction \(conflicting files\)' \
    -e 'error: target not found:' \
    -e 'error: could not compile' \
    -e '^error\[E[0-9]+\]:' \
    -e 'npm (err!|error) code EUSAGE' \
    -e 'can only install packages when your package\.json and package-lock\.json' \
    -e 'failed to find package' \
    -e 'no available formula' \
    -e 'command line option .* is not understood' \
    "$file"
}

is_retryable_fetch_error() {
  file="$1"
  if is_non_fetch_failure "$file"; then
    return 1
  fi
  grep -Eqi \
    -e 'some index files failed to download' \
    -e 'failed to fetch' \
    -e 'unable to fetch some archives' \
    -e 'hash sum mismatch' \
    -e 'file has unexpected size' \
    -e 'mirror sync in progress' \
    -e 'writing more data than expected' \
    -e "clearsigned file isn't valid" \
    -e 'release file for .* is not valid yet' \
    -e 'failed retrieving file' \
    -e 'failed to synchronize' \
    -e 'failed to retrieve some files' \
    -e 'failed to download' \
    -e 'download failed' \
    -e 'error downloading' \
    -e 'download of .* failed' \
    -e 'could not download' \
    -e 'error sending request' \
    -e 'spurious network error' \
    -e 'fetch failed' \
    -e 'temporary failure resolving' \
    -e 'temporary failure in name resolution' \
    -e 'could not resolve' \
    -e 'name or service not known' \
    -e 'the remote name could not be resolved' \
    -e 'no such host' \
    -e 'dns error' \
    -e 'could not connect' \
    -e 'unable to connect' \
    -e 'connection timed out' \
    -e 'connect timed out' \
    -e 'read timed out' \
    -e 'operation timed out' \
    -e 'timed out' \
    -e 'connection reset' \
    -e 'connection refused' \
    -e 'connection failed' \
    -e 'connection closed' \
    -e 'connection was forcibly closed' \
    -e 'underlying connection was closed' \
    -e 'unexpected eof' \
    -e 'premature close' \
    -e 'socket hang up' \
    -e 'empty reply from server' \
    -e 'broken pipe' \
    -e 'network is unreachable' \
    -e 'no route to host' \
    -e 'operation too slow' \
    -e 'error reading from server' \
    -e 'invalid reply header' \
    -e 'undetermined error' \
    -e '(ETIMEDOUT|ECONNRESET|ECONNREFUSED|EAI_AGAIN|ENOTFOUND|ESOCKETTIMEDOUT|ERR_SOCKET_TIMEOUT|EHOSTUNREACH|ENETUNREACH)' \
    -e '(sockettimeoutexception|connectexception|unknownhostexception|sslhandshakeexception|sslexception)' \
    -e '(ssl|tls).*(reset|eof|timeout|timed out|connect|handshake|wrong version|unexpected)' \
    -e 'gnutls' \
    -e '(http(/[0-9.]+)?|status( code)?|returned error:?|response code:?|server returned)[[:space:]]*(429|500|502|503|504)([^0-9]|$)' \
    -e '(^|[^0-9])(429|500|502|503|504)[[:space:]]*\(?(too many requests|internal server error|bad gateway|service unavailable|gateway timeout)' \
    -e '(too many requests|bad gateway|service unavailable|gateway timeout)' \
    -e 'curl: \((6|7|18|28|35|52|55|56|92)\)' \
    "$file"
}

attempt=1
while true; do
  : > "$tmp_log"
  : > "$rc_file"

  if [ "$quiet" -eq 1 ]; then
    rc=0
    "$@" >"$tmp_log" 2>&1 || rc=$?
  else
    (
      sub_rc=0
      "$@" || sub_rc=$?
      printf '%s\n' "$sub_rc" > "$rc_file"
    ) 2>&1 | tee "$tmp_log"
    rc="$(cat "$rc_file")"
    case "$rc" in
      ''|*[!0-9]*) rc=1 ;;
    esac
  fi

  stale_apt_index=0
  if [ "$rc" -eq 0 ] && [ "$is_apt_update" -eq 1 ]; then
    if grep -Eqi 'some index files failed to download' "$tmp_log"; then
      stale_apt_index=1
      rc=100
    fi
  fi

  if [ "$rc" -eq 0 ] && [ -n "$verify_cmd" ]; then
    if ! command -v "$verify_cmd" >/dev/null 2>&1; then
      printf 'retry-fetch: expected command %s was not found on PATH after attempt %s/%s.\n' "'$verify_cmd'" "$attempt" "$attempts" >&2
      rc=1
    fi
  fi

  if [ "$rc" -eq 0 ]; then
    exit 0
  fi

  if [ "$stale_apt_index" -eq 0 ] && ! is_retryable_fetch_error "$tmp_log"; then
    if [ "$quiet" -eq 1 ] && [ -s "$tmp_log" ]; then
      cat "$tmp_log" >&2
    fi
    printf 'retry-fetch: failure on attempt %s/%s is not a transient fetch error; not retrying.\n' "$attempt" "$attempts" >&2
    exit "$rc"
  fi

  if [ "$attempt" -ge "$attempts" ]; then
    if [ "$quiet" -eq 1 ] && [ -s "$tmp_log" ]; then
      cat "$tmp_log" >&2
    fi
    printf 'retry-fetch: attempt %s/%s failed with transient fetch error; giving up.\n' "$attempt" "$attempts" >&2
    exit "$rc"
  fi

  delay="$(delay_for_retry "$attempt")"
  if [ "$stale_apt_index" -eq 1 ]; then
    printf 'retry-fetch: apt update reported failed index downloads on attempt %s/%s; retrying in %ss...\n' "$attempt" "$attempts" "$delay" >&2
  else
    printf 'retry-fetch: attempt %s/%s failed with transient fetch error; retrying in %ss...\n' "$attempt" "$attempts" "$delay" >&2
  fi
  if [ "$delay" -gt 0 ]; then
    sleep "$delay"
  fi

  # A mirror sync or package rotation between apt-get update and apt-get install
  # requires refreshing indexes before retrying the install.
  if [ "$apt_subcmd" = "install" ]; then
    if [ -n "$apt_sudo" ]; then
      "$apt_sudo" "$apt_bin" -o Acquire::Retries=3 update -qq >/dev/null 2>&1 || true
    else
      "$apt_bin" -o Acquire::Retries=3 update -qq >/dev/null 2>&1 || true
    fi
  fi

  attempt=$((attempt + 1))
done
