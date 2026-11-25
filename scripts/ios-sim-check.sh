#!/usr/bin/env bash
# Run the iOS/iPadOS simulator smoke verification for one device:
#   1. Boot simulator, install app, start throwaway SSH media fixture, and launch app
#   2. Wait for interface ready (independent clock)
#   3. Wait for pairing to finish (independent clock, starting after interface ready)
#   4. Wait for initial media & IPC isolation check (independent clock, starting after pairing)
#   5. Run post-resume media check (scripts/ios-sim-resume.sh)
#   6. Verify iCloud/iTunes backup exclusion and capture dark + light screenshots
#
# If attempt 1/2 fails at any stage before media was reached (boot, setup, launch, interface
# ready, or pairing), reset the app's data on the simulator and run attempt 2/2 once.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"

KIND=""
UDID=""
DEV_NAME=""
SIM_APP=""
LEGION_BIN=""
RUNNER_TEMP_DIR="${RUNNER_TEMP:-/tmp}"
SHOTS_DIR="ci-shots/ios"
APP_BUNDLE="com.x1f4r.legion"
BOOT_TIMEOUT=480
READY_TIMEOUT=90
PAIR_TIMEOUT=90
MEDIA_TIMEOUT=90
POLL_INTERVAL=1
RETRY_SLEEP=3
SCREENSHOT_SETTLE=2
RESUME_SCRIPT="${SCRIPT_DIR}/ios-sim-resume.sh"
FIXTURE_SCRIPT="${SCRIPT_DIR}/ci-media-fixture.mjs"
RESUME_CYCLES=3
RESUME_WAIT_SECS=30
RESUME_FRONTMOST_TIMEOUT=20

usage() {
  cat <<'EOF' >&2
Usage: ios-sim-check.sh --kind <iphone|ipad> --udid <udid> --dev-name <name> --sim-app <path> --legion-bin <path> [options]
Options:
  --runner-temp <dir>              Temporary directory for logs and fixtures (default: $RUNNER_TEMP or /tmp)
  --shots-dir <dir>                Screenshot output directory (default: ci-shots/ios)
  --app-bundle <id>                App bundle identifier (default: com.x1f4r.legion)
  --boot-timeout <secs>            Timeout for simulator bootstatus (default: 480)
  --ready-timeout <secs>           Timeout waiting for interface ready (default: 90)
  --pair-timeout <secs>            Timeout waiting for pairing after interface ready (default: 90)
  --media-timeout <secs>           Timeout waiting for initial media check after pairing (default: 90)
  --poll-interval <secs>           Sleep between stage polls (default: 1)
  --retry-sleep <secs>             Sleep before attempt 2/2 (default: 3)
  --screenshot-settle <secs>       Settle seconds before each screenshot (default: 2)
  --resume-script <path>           Path to ios-sim-resume.sh
  --fixture-script <path>          Path to ci-media-fixture.mjs
  --resume-cycles <n>              Background/return cycles passed to ios-sim-resume.sh (default: 3)
  --resume-wait-secs <n>           Per-cycle wait seconds passed to ios-sim-resume.sh (default: 30)
  --resume-frontmost-timeout <n>   Frontmost timeout passed to ios-sim-resume.sh (default: 20)
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --kind) KIND="$2"; shift 2 ;;
    --udid) UDID="$2"; shift 2 ;;
    --dev-name) DEV_NAME="$2"; shift 2 ;;
    --sim-app) SIM_APP="$2"; shift 2 ;;
    --legion-bin) LEGION_BIN="$2"; shift 2 ;;
    --runner-temp) RUNNER_TEMP_DIR="$2"; shift 2 ;;
    --shots-dir) SHOTS_DIR="$2"; shift 2 ;;
    --app-bundle) APP_BUNDLE="$2"; shift 2 ;;
    --boot-timeout) BOOT_TIMEOUT="$2"; shift 2 ;;
    --ready-timeout) READY_TIMEOUT="$2"; shift 2 ;;
    --pair-timeout) PAIR_TIMEOUT="$2"; shift 2 ;;
    --media-timeout) MEDIA_TIMEOUT="$2"; shift 2 ;;
    --poll-interval) POLL_INTERVAL="$2"; shift 2 ;;
    --retry-sleep) RETRY_SLEEP="$2"; shift 2 ;;
    --screenshot-settle) SCREENSHOT_SETTLE="$2"; shift 2 ;;
    --resume-script) RESUME_SCRIPT="$2"; shift 2 ;;
    --fixture-script) FIXTURE_SCRIPT="$2"; shift 2 ;;
    --resume-cycles) RESUME_CYCLES="$2"; shift 2 ;;
    --resume-wait-secs) RESUME_WAIT_SECS="$2"; shift 2 ;;
    --resume-frontmost-timeout) RESUME_FRONTMOST_TIMEOUT="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

if [ -z "$KIND" ] || [ -z "$UDID" ] || [ -z "$DEV_NAME" ] || [ -z "$SIM_APP" ] || [ -z "$LEGION_BIN" ]; then
  usage
  exit 2
fi

mkdir -p "$SHOTS_DIR" "$RUNNER_TEMP_DIR"

# A simulator on a loaded runner can take well over half a minute to hand back a picture.
# Each attempt is bounded, and three failed attempts fail the job with a clear message.
shoot() {
  local udid="$1" out="$2" attempt
  for attempt in 1 2 3; do
    if perl -e 'alarm 90; exec @ARGV' xcrun simctl io "$udid" screenshot "$out" && [ -s "$out" ]; then
      return 0
    fi
    echo "screenshot attempt $attempt for $out did not finish in time; trying again" >&2
    rm -f "$out"
    if [ "$SCREENSHOT_SETTLE" != "0" ]; then
      sleep 5
    fi
  done
  echo "could not take $out after three attempts" >&2
  return 1
}

read_pair_status() {
  local media_out="$1"
  if [ ! -f "$media_out" ]; then
    printf 'pending\n'
    return 0
  fi
  node -e '
const fs = require("node:fs");
try {
  const r = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
  if (r && (r.pair_ok === true || r.ok === true || r.stage === "media" || r.stage === "resume" || r.stage === "done")) {
    console.log("pass");
  } else if (r && r.ok === false && r.error) {
    console.log("fail:" + r.error);
  } else {
    console.log("pending");
  }
} catch {
  console.log("pending");
}
' "$media_out"
}

read_media_status() {
  local media_out="$1"
  if [ ! -f "$media_out" ]; then
    printf 'pending\n'
    return 0
  fi
  node -e '
const fs = require("node:fs");
try {
  const r = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
  if (r && r.ok === true && r.image_width > 0 && r.video_width > 0 && r.ipc_isolation_ok === true) {
    console.log("pass");
  } else if (r && r.ok === false && r.error) {
    console.log("fail:" + r.error);
  } else {
    console.log("pending");
  }
} catch {
  console.log("pending");
}
' "$media_out"
}

stop_fixture_sshd() {
  local media_work="$1"
  if [ -f "$media_work/sshd.pid" ]; then
    kill "$(cat "$media_work/sshd.pid")" 2>/dev/null || true
  fi
}

reset_app_data() {
  local attempt="$1"
  local existing_dir
  echo "Resetting app data for ${APP_BUNDLE} on ${KIND} simulator before attempt ${attempt}/2..."
  xcrun simctl terminate "$UDID" "$APP_BUNDLE" 2>/dev/null || true
  existing_dir="$(xcrun simctl get_app_container "$UDID" "$APP_BUNDLE" data 2>/dev/null || true)"
  if [ -n "$existing_dir" ] && [ -d "$existing_dir" ]; then
    rm -rf \
      "$existing_dir/Library/Application Support/$APP_BUNDLE" \
      "$existing_dir/Library/Caches" \
      "$existing_dir/Library/Preferences" \
      "$existing_dir/tmp" || true
  fi
  xcrun simctl uninstall "$UDID" "$APP_BUNDLE" 2>/dev/null || true
}

dump_diagnostics() {
  local data_dir="$1"
  local media_work="$2"
  local media_out="$3"
  local stdout_log="$4"
  local stderr_log="$5"
  local latest_crash

  echo "--- ${KIND} stdout ---"
  cat "$stdout_log" 2>/dev/null || true
  echo "--- ${KIND} stderr ---"
  cat "$stderr_log" 2>/dev/null || true
  if [ -f "$media_out" ]; then
    echo "--- ${KIND} media-result.json ---"
    cat "$media_out" || true
  fi
  if [ -f "$media_work/sshd.log" ]; then
    cat "$media_work/sshd.log" || true
  fi
  if [ -n "$data_dir" ] && [ -d "$data_dir/Library/Application Support" ]; then
    ls -la "$data_dir/Library/Application Support" || true
  fi
  xcrun simctl io "$UDID" screenshot "${SHOTS_DIR}/${KIND}-timeout.png" 2>/dev/null || true
  if [ -d "$HOME/Library/Logs/DiagnosticReports" ]; then
    echo "--- DiagnosticReports ---"
    latest_crash="$(node -e '
const fs = require("node:fs");
const path = require("node:path");
const dir = process.argv[1];
try {
  const files = fs.readdirSync(dir)
    .filter((f) => f.startsWith("Legion"))
    .map((f) => ({ p: path.join(dir, f), m: fs.statSync(path.join(dir, f)).mtimeMs }))
    .sort((a, b) => b.m - a.m);
  if (files[0]) console.log(files[0].p);
} catch {}
' "$HOME/Library/Logs/DiagnosticReports")"
    if [ -n "$latest_crash" ]; then
      echo "=== Crash report: ${latest_crash} ==="
      head -n 160 "$latest_crash" || true
    fi
  fi
  echo "--- CoreSimulator logs ---"
  xcrun simctl spawn "$UDID" log show --last 3m --style compact --predicate "eventMessage CONTAINS \"Legion\" OR eventMessage CONTAINS \"${APP_BUNDLE}\"" 2>/dev/null | tail -n 100 || true
}

run_simulator_once() {
  local attempt="$1"
  local data_dir="" ready_file media_work media_spec media_out stdout_log stderr_log
  local launch_out app_pid app_data_dir
  local found=0 pair_ok=0 media_ok=0 resume_ok=0 reached_media=0
  local missing_stage=""
  local stage_t0 elapsed status_line reason

  echo "=== Running on ${KIND} (${DEV_NAME} / ${UDID}) [attempt ${attempt}/2] ==="
  xcrun simctl boot "$UDID" || true
  if ! perl -e 'alarm shift; exec @ARGV' "$BOOT_TIMEOUT" xcrun simctl bootstatus "$UDID" -b; then
    echo "Failed ${KIND} at stage: boot (ready=0, pairing=0, media_check=0, resume=0)"
    xcrun simctl shutdown "$UDID" || true
    return 2
  fi

  if [ "$attempt" -gt 1 ]; then
    reset_app_data "$attempt"
  fi

  if ! xcrun simctl install "$UDID" "$SIM_APP"; then
    echo "Failed ${KIND} at stage: install (ready=0, pairing=0, media_check=0, resume=0)"
    xcrun simctl shutdown "$UDID" || true
    return 2
  fi

  if ! data_dir="$(xcrun simctl get_app_container "$UDID" "$APP_BUNDLE" data)" || [ -z "$data_dir" ]; then
    echo "Failed ${KIND} at stage: app container (ready=0, pairing=0, media_check=0, resume=0)"
    xcrun simctl shutdown "$UDID" || true
    return 2
  fi
  mkdir -p "$data_dir/tmp" || return 2
  ready_file="$data_dir/tmp/legion-ready.txt"
  media_work="$RUNNER_TEMP_DIR/ios-media-${KIND}"
  media_spec="$data_dir/tmp/media-check.json"
  media_out="$data_dir/tmp/media-result.json"
  stdout_log="$RUNNER_TEMP_DIR/legion-${KIND}-stdout.log"
  stderr_log="$RUNNER_TEMP_DIR/legion-${KIND}-stderr.log"
  rm -rf "$media_work" "$ready_file" "$media_spec" "$media_out" "$stdout_log" "$stderr_log"

  if ! node "$FIXTURE_SCRIPT" ssh \
    --work "$media_work" \
    --legion "$LEGION_BIN" \
    --host 127.0.0.1 \
    --verify-resume \
    --out "$media_out" >/dev/null; then
    echo "Failed ${KIND} at stage: fixture setup (ready=0, pairing=0, media_check=0, resume=0)"
    stop_fixture_sshd "$media_work"
    xcrun simctl shutdown "$UDID" || true
    return 2
  fi
  cp "$media_work/media-check.json" "$media_spec" || {
    stop_fixture_sshd "$media_work"
    xcrun simctl shutdown "$UDID" || true
    return 2
  }

  xcrun simctl ui "$UDID" appearance dark || true
  if ! launch_out="$(SIMCTL_CHILD_LEGION_APP_READY_FILE="$ready_file" \
    SIMCTL_CHILD_LEGION_APP_MEDIA_CHECK_FILE="$media_spec" \
    SIMCTL_CHILD_LEGION_APP_ALLOW_LOOPBACK_ENDPOINTS=1 \
    xcrun simctl launch --stdout="$stdout_log" --stderr="$stderr_log" "$UDID" "$APP_BUNDLE")"; then
    echo "Failed ${KIND} at stage: launch (ready=0, pairing=0, media_check=0, resume=0)"
    stop_fixture_sshd "$media_work"
    xcrun simctl shutdown "$UDID" || true
    return 2
  fi
  echo "$launch_out"
  app_pid="$(printf '%s\n' "$launch_out" | awk '{print $2}')"

  # Stage 1: Interface ready (own clock).
  stage_t0="$(date +%s)"
  while true; do
    if [ -f "$ready_file" ] && grep -q "legion-app: interface ready" "$ready_file"; then
      elapsed=$(( $(date +%s) - stage_t0 ))
      echo "${KIND} interface ready (file) after ${elapsed}s: $(cat "$ready_file")"
      found=1
      break
    elif [ -f "$stdout_log" ] && grep -q "legion-app: interface ready" "$stdout_log"; then
      elapsed=$(( $(date +%s) - stage_t0 ))
      echo "${KIND} interface ready (stdout) after ${elapsed}s"
      found=1
      break
    fi
    if [ -n "$app_pid" ] && ! kill -0 "$app_pid" 2>/dev/null; then
      elapsed=$(( $(date +%s) - stage_t0 ))
      echo "${KIND} app process ${app_pid} exited after ${elapsed}s before interface ready"
      missing_stage="app process exited before interface ready"
      break
    fi
    elapsed=$(( $(date +%s) - stage_t0 ))
    if [ "$elapsed" -ge "$READY_TIMEOUT" ]; then
      missing_stage="interface ready"
      break
    fi
    sleep "$POLL_INTERVAL"
  done

  # Stage 2: Pairing (own generous clock, starting only after interface ready).
  if [ "$found" -eq 1 ]; then
    stage_t0="$(date +%s)"
    while true; do
      status_line="$(read_pair_status "$media_out")"
      case "$status_line" in
        pass)
          elapsed=$(( $(date +%s) - stage_t0 ))
          echo "${KIND} pairing completed after ${elapsed}s"
          pair_ok=1
          reached_media=1
          break
          ;;
        fail:*)
          elapsed=$(( $(date +%s) - stage_t0 ))
          reason="${status_line#fail:}"
          echo "${KIND} pairing failed after ${elapsed}s: ${reason}"
          missing_stage="pairing"
          break
          ;;
      esac
      if [ -n "$app_pid" ] && ! kill -0 "$app_pid" 2>/dev/null; then
        elapsed=$(( $(date +%s) - stage_t0 ))
        echo "${KIND} app process ${app_pid} exited after ${elapsed}s during pairing"
        missing_stage="app process exited during pairing"
        break
      fi
      elapsed=$(( $(date +%s) - stage_t0 ))
      if [ "$elapsed" -ge "$PAIR_TIMEOUT" ]; then
        missing_stage="pairing"
        break
      fi
      sleep "$POLL_INTERVAL"
    done
  fi

  # Stage 3: Initial media check (own clock, starting only after pairing succeeded).
  if [ "$found" -eq 1 ] && [ "$pair_ok" -eq 1 ]; then
    stage_t0="$(date +%s)"
    while true; do
      status_line="$(read_media_status "$media_out")"
      case "$status_line" in
        pass)
          elapsed=$(( $(date +%s) - stage_t0 ))
          echo "${KIND} initial media check passed after ${elapsed}s"
          cat "$media_out"
          media_ok=1
          break
          ;;
        fail:*)
          elapsed=$(( $(date +%s) - stage_t0 ))
          reason="${status_line#fail:}"
          echo "${KIND} initial media check failed after ${elapsed}s: ${reason}"
          missing_stage="initial media check"
          break
          ;;
      esac
      if [ -n "$app_pid" ] && ! kill -0 "$app_pid" 2>/dev/null; then
        elapsed=$(( $(date +%s) - stage_t0 ))
        echo "${KIND} app process ${app_pid} exited after ${elapsed}s during initial media check"
        missing_stage="app process exited during initial media check"
        break
      fi
      elapsed=$(( $(date +%s) - stage_t0 ))
      if [ "$elapsed" -ge "$MEDIA_TIMEOUT" ]; then
        missing_stage="initial media check"
        break
      fi
      sleep "$POLL_INTERVAL"
    done
  fi

  # Stage 4: Post-resume check.
  if [ "$found" -eq 1 ] && [ "$pair_ok" -eq 1 ] && [ "$media_ok" -eq 1 ]; then
    if bash "$RESUME_SCRIPT" \
      --kind "$KIND" \
      --udid "$UDID" \
      --media-out "$media_out" \
      --app-pid "$app_pid" \
      --cycles "$RESUME_CYCLES" \
      --wait-secs "$RESUME_WAIT_SECS" \
      --frontmost-timeout "$RESUME_FRONTMOST_TIMEOUT"; then
      resume_ok=1
    else
      missing_stage="post-resume check"
    fi
  fi

  stop_fixture_sshd "$media_work"

  if [ "$found" -ne 1 ] || [ "$pair_ok" -ne 1 ] || [ "$media_ok" -ne 1 ] || [ "$resume_ok" -ne 1 ]; then
    if [ -z "$missing_stage" ]; then
      missing_stage="unknown"
    fi
    echo "Failed ${KIND} at stage: ${missing_stage} (ready=${found}, pairing=${pair_ok}, media_check=${media_ok}, resume=${resume_ok})"
    dump_diagnostics "$data_dir" "$media_work" "$media_out" "$stdout_log" "$stderr_log"
    xcrun simctl terminate "$UDID" "$APP_BUNDLE" || true
    xcrun simctl shutdown "$UDID" || true
    if [ "$reached_media" -eq 0 ]; then
      return 2
    fi
    return 1
  fi

  echo "--- ${KIND} stdout ---"
  cat "$stdout_log" 2>/dev/null || true
  echo "--- ${KIND} stderr ---"
  cat "$stderr_log" 2>/dev/null || true

  # Confirm the app data directory is excluded from iCloud/iTunes backups.
  app_data_dir="$data_dir/Library/Application Support/$APP_BUNDLE"
  test -d "$app_data_dir" || return 1
  swift - "$app_data_dir" <<'SWIFT' || return 1
import Foundation
let path = CommandLine.arguments[1]
let url = URL(fileURLWithPath: path)
let values = try url.resourceValues(forKeys: [.isExcludedFromBackupKey])
guard values.isExcludedFromBackup == true else {
    fputs("Expected isExcludedFromBackup == true for \(path), got \(String(describing: values.isExcludedFromBackup))\n", stderr)
    exit(1)
}
print("Verified isExcludedFromBackup == true for \(path)")
SWIFT

  if [ "$SCREENSHOT_SETTLE" != "0" ]; then
    sleep "$SCREENSHOT_SETTLE"
  fi
  shoot "$UDID" "${SHOTS_DIR}/${KIND}-dark.png" || return 1

  xcrun simctl ui "$UDID" appearance light || true
  if [ "$SCREENSHOT_SETTLE" != "0" ]; then
    sleep "$SCREENSHOT_SETTLE"
  fi
  shoot "$UDID" "${SHOTS_DIR}/${KIND}-light.png" || return 1

  xcrun simctl terminate "$UDID" "$APP_BUNDLE" || true
  xcrun simctl shutdown "$UDID" || true
  return 0
}

rc=0
run_simulator_once 1 || rc=$?
if [ "$rc" -eq 0 ]; then
  exit 0
fi
if [ "$rc" -eq 2 ]; then
  echo "Retrying ${KIND} simulator once (attempt 2/2) after pre-media failure; resetting app data on simulator..." >&2
  if [ "$RETRY_SLEEP" != "0" ]; then
    sleep "$RETRY_SLEEP"
  fi
  rc2=0
  run_simulator_once 2 || rc2=$?
  if [ "$rc2" -eq 0 ]; then
    exit 0
  fi
  exit 1
fi
exit "$rc"
