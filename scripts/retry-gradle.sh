#!/usr/bin/env bash
# Run a Gradle-invoking command and retry only when it fails due to an upstream repository rate
# limit (HTTP 429) or transient network/5xx fetch error. Real compilation, packaging, and test
# failures exit immediately on the first attempt.
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage: retry-gradle.sh [--attempts N] [--delays CSV] [--log PATH] -- <command...>
EOF
  exit 2
}

attempts=4
delays_csv="15,30,60"
log_path=""

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
    --log)
      [ "$#" -ge 2 ] || usage
      log_path="$2"
      shift 2
      ;;
    --)
      shift
      break
      ;;
    -h|--help)
      usage
      ;;
    *)
      echo "retry-gradle: unknown option: $1" >&2
      usage
      ;;
  esac
done

[ "$#" -gt 0 ] || usage

case "$attempts" in
  ''|*[!0-9]*)
    echo "retry-gradle: --attempts must be a positive integer (got '$attempts')" >&2
    exit 2
    ;;
esac
if [ "$attempts" -lt 1 ]; then
  echo "retry-gradle: --attempts must be at least 1 (got '$attempts')" >&2
  exit 2
fi

IFS=',' read -r -a delays <<<"$delays_csv"
if [ "${#delays[@]}" -eq 0 ]; then
  echo "retry-gradle: --delays must not be empty" >&2
  exit 2
fi
for d in "${delays[@]}"; do
  case "$d" in
    ''|*[!0-9]*)
      echo "retry-gradle: delay must be a non-negative integer in seconds (got '$d')" >&2
      exit 2
      ;;
  esac
done

tmp_log=""
cleanup() {
  if [ -n "$tmp_log" ]; then
    rm -f "$tmp_log"
  fi
}
trap cleanup EXIT

if [ -n "$log_path" ]; then
  mkdir -p "$(dirname "$log_path")"
  active_log="$log_path"
else
  tmp_log="$(mktemp "${TMPDIR:-/tmp}/retry-gradle.XXXXXX")"
  active_log="$tmp_log"
fi

is_retryable_gradle_error() {
  local file="$1"
  local section
  # When Gradle reports a structured failure block, classify only from that block onward so
  # earlier build output or noisy test logs can never trigger a retry on a real compile/test bug.
  if grep -Fq "FAILURE: Build failed with an exception." "$file"; then
    section="$(awk '/FAILURE: Build failed with an exception\./ { found=1 } found { print }' "$file")"
  else
    section="$(cat "$file")"
  fi

  # Require both Gradle/repository remote-fetch context and a rate-limit, 5xx, or transient
  # transport error so local compilation errors, 404s, and unit test failures never retry.
  grep -Eqi \
    -e "could not (get|head|download|get resource|resolve)" \
    -e "downloading https?://" \
    -e "server returned http response code:" \
    -e "received status code (429|50[0234]) from server" \
    <<<"$section" || return 1

  grep -Eqi \
    -e "(received )?(status code|http response code:|http) (429|500|502|503|504)([^0-9]|$)" \
    -e "too many requests" \
    -e "java\\.net\\.(sockettimeoutexception|connectexception|unknownhostexception|noroutetohostexception|socketexception)" \
    -e "javax\\.net\\.ssl\\.sslexception:.*(connection reset|unexpected eof|remote host terminated)" \
    -e "org\\.apache\\.http\\.(nohttpresponseexception|connectionclosedexception)" \
    -e "premature end of content-length delimited message body" \
    -e "read timed out" \
    -e "connect timed out" \
    -e "connection reset" \
    -e "temporary failure in name resolution" \
    -e "name or service not known" \
    -e "bad gateway" \
    -e "service unavailable" \
    -e "gateway timeout" \
    -e "unexpected end of file from server" \
    <<<"$section"
}

attempt=1
while true; do
  : > "$active_log"
  set +e
  "$@" 2>&1 | tee "$active_log"
  rc=${PIPESTATUS[0]}
  set -e

  if [ "$rc" -eq 0 ]; then
    exit 0
  fi

  if ! is_retryable_gradle_error "$active_log"; then
    echo "retry-gradle: failure on attempt $attempt/$attempts is not a transient Gradle network or rate-limit error; not retrying." >&2
    exit "$rc"
  fi

  if [ "$attempt" -ge "$attempts" ]; then
    echo "retry-gradle: attempt $attempt/$attempts failed with transient Gradle network or rate-limit error; giving up." >&2
    exit "$rc"
  fi

  delay_idx=$((attempt - 1))
  if [ "$delay_idx" -ge "${#delays[@]}" ]; then
    delay_idx=$((${#delays[@]} - 1))
  fi
  delay="${delays[$delay_idx]}"
  echo "retry-gradle: attempt $attempt/$attempts failed with transient Gradle network or rate-limit error; retrying in ${delay}s..." >&2
  if [ "$delay" -gt 0 ]; then
    sleep "$delay"
  fi
  attempt=$((attempt + 1))
done
