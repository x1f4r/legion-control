#!/usr/bin/env bash
# Background and foreground Legion on an iOS/iPadOS simulator with frontmost confirmation and
# bounded retry cycles until the post-resume media check reports pass or fail.
set -euo pipefail

KIND=""
UDID=""
MEDIA_OUT=""
APP_PID=""
APP_BUNDLE="com.x1f4r.legion"
OTHER_BUNDLE="com.apple.Preferences"
CYCLES=3
WAIT_SECS=30
FRONTMOST_TIMEOUT=20
POLL_INTERVAL=1
FRONTMOST_POLL_INTERVAL=1
SETTLE_SECS=1
CMD_TIMEOUT=10

usage() {
  cat <<'EOF' >&2
Usage: ios-sim-resume.sh --kind <iphone|ipad> --udid <udid> --media-out <file> [options]
Options:
  --app-pid <pid>                  Optional Legion PID to watch for unexpected exit
  --app-bundle <id>                App bundle identifier (default: com.x1f4r.legion)
  --other-bundle <id>              Backgrounding app bundle id (default: com.apple.Preferences)
  --cycles <n>                     Maximum background/return cycles (default: 3)
  --wait-secs <n>                  Seconds to wait for resume_ok per cycle (default: 30)
  --frontmost-timeout <n>          Seconds to wait for other app frontmost (default: 20)
  --poll-interval <secs>           Sleep between result polls (default: 1)
  --frontmost-poll-interval <secs> Sleep between frontmost polls (default: 1)
  --settle-secs <secs>             Brief settle after frontmost confirmation (default: 1)
  --cmd-timeout <secs>             Timeout per simctl log query (default: 10)
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --kind) KIND="$2"; shift 2 ;;
    --udid) UDID="$2"; shift 2 ;;
    --media-out) MEDIA_OUT="$2"; shift 2 ;;
    --app-pid) APP_PID="$2"; shift 2 ;;
    --app-bundle) APP_BUNDLE="$2"; shift 2 ;;
    --other-bundle) OTHER_BUNDLE="$2"; shift 2 ;;
    --cycles) CYCLES="$2"; shift 2 ;;
    --wait-secs) WAIT_SECS="$2"; shift 2 ;;
    --frontmost-timeout) FRONTMOST_TIMEOUT="$2"; shift 2 ;;
    --poll-interval) POLL_INTERVAL="$2"; shift 2 ;;
    --frontmost-poll-interval) FRONTMOST_POLL_INTERVAL="$2"; shift 2 ;;
    --settle-secs) SETTLE_SECS="$2"; shift 2 ;;
    --cmd-timeout) CMD_TIMEOUT="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

if [ -z "$KIND" ] || [ -z "$UDID" ] || [ -z "$MEDIA_OUT" ]; then
  usage
  exit 2
fi

read_resume_status() {
  if [ ! -f "$MEDIA_OUT" ]; then
    printf 'pending\n'
    return 0
  fi
  node -e '
const fs = require("node:fs");
try {
  const r = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
  if (r && r.ok === true && r.resume_ok === true) {
    console.log("pass:" + (r.resume_trigger || "unknown"));
  } else if (r && r.ok === false) {
    console.log("fail:" + (r.error || "unknown error"));
  } else {
    console.log("pending");
  }
} catch {
  console.log("pending");
}
' "$MEDIA_OUT"
}

check_other_frontmost() {
  local since_ts="$1"
  local predicate log_out
  predicate="subsystem == \"com.apple.runningboard\" AND (eventMessage CONTAINS \"${OTHER_BUNDLE}\" OR eventMessage CONTAINS \"${APP_BUNDLE}\")"
  log_out="$(perl -e 'alarm shift; exec @ARGV' "$CMD_TIMEOUT" \
    xcrun simctl spawn "$UDID" log show --start "$since_ts" --style compact --predicate "$predicate" 2>/dev/null || true)"
  if printf '%s\n' "$log_out" | grep -Eq "${OTHER_BUNDLE}.*running-active-Visible|${APP_BUNDLE}.*(running-suspended|NotVisible)"; then
    return 0
  fi
  return 1
}

any_frontmost_confirmed=0
cycle=1
while [ "$cycle" -le "$CYCLES" ]; do
  echo "Backgrounding and resuming Legion on ${KIND} (cycle ${cycle}/${CYCLES})..."
  since_ts="$(date '+%Y-%m-%d %H:%M:%S%z')"
  switch_t0="$(date +%s)"

  other_launch_out="$(xcrun simctl launch "$UDID" "$OTHER_BUNDLE" 2>&1 || true)"
  echo "${KIND} cycle ${cycle}/${CYCLES}: launched ${OTHER_BUNDLE} (${other_launch_out})"

  frontmost_confirmed=0
  frontmost_t0="$(date +%s)"
  while true; do
    if check_other_frontmost "$since_ts"; then
      frontmost_confirmed=1
      any_frontmost_confirmed=1
      frontmost_elapsed=$(( $(date +%s) - frontmost_t0 ))
      echo "${KIND} cycle ${cycle}/${CYCLES}: ${OTHER_BUNDLE} confirmed frontmost after ${frontmost_elapsed}s"
      break
    fi
    frontmost_elapsed=$(( $(date +%s) - frontmost_t0 ))
    if [ "$frontmost_elapsed" -ge "$FRONTMOST_TIMEOUT" ]; then
      echo "${KIND} cycle ${cycle}/${CYCLES}: ${OTHER_BUNDLE} not confirmed frontmost within ${FRONTMOST_TIMEOUT}s; returning to ${APP_BUNDLE}"
      break
    fi
    sleep "$FRONTMOST_POLL_INTERVAL"
  done

  if [ "$frontmost_confirmed" -eq 1 ] && [ "$SETTLE_SECS" != "0" ]; then
    sleep "$SETTLE_SECS"
  fi

  app_launch_out="$(xcrun simctl launch "$UDID" "$APP_BUNDLE" 2>&1 || true)"
  switch_elapsed=$(( $(date +%s) - switch_t0 ))
  echo "${KIND} cycle ${cycle}/${CYCLES}: returned to ${APP_BUNDLE} in ${switch_elapsed}s (${app_launch_out})"

  wait_t0="$(date +%s)"
  while true; do
    status_line="$(read_resume_status)"
    waited=$(( $(date +%s) - wait_t0 ))
    case "$status_line" in
      pass:*)
        trigger="${status_line#pass:}"
        echo "${KIND} post-resume loopback media check passed on cycle ${cycle}/${CYCLES} after ${waited}s (trigger=${trigger})"
        cat "$MEDIA_OUT"
        exit 0
        ;;
      fail:*)
        reason="${status_line#fail:}"
        echo "${KIND} failed at stage: post-resume media check (cycle ${cycle}/${CYCLES} after ${waited}s: ${reason})"
        cat "$MEDIA_OUT"
        exit 1
        ;;
    esac

    if [ -n "$APP_PID" ] && ! kill -0 "$APP_PID" 2>/dev/null; then
      echo "${KIND} failed at stage: post-resume check (app process ${APP_PID} exited during cycle ${cycle}/${CYCLES})"
      exit 1
    fi

    if [ "$waited" -ge "$WAIT_SECS" ]; then
      echo "${KIND} cycle ${cycle}/${CYCLES}: resume_ok still pending after ${waited}s (frontmost_confirmed=${frontmost_confirmed})"
      break
    fi
    sleep "$POLL_INTERVAL"
  done

  cycle=$(( cycle + 1 ))
done

if [ "$any_frontmost_confirmed" -eq 0 ]; then
  echo "${KIND} failed at stage: background switch (${OTHER_BUNDLE} was never confirmed frontmost across ${CYCLES} cycles; resume_ok stayed false)"
else
  echo "${KIND} failed at stage: post-resume result (resume_ok never turned true across ${CYCLES} cycles of ${WAIT_SECS}s)"
fi
exit 1
