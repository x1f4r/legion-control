#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
MODE="${1:-run}"
case "$MODE" in run|--debug|--logs|--telemetry|--verify) ;; *) echo "Usage: $0 [--debug|--logs|--telemetry|--verify]" >&2; exit 2 ;; esac
APP="$ROOT/mac/build/Legion Control.app"
BINARY="$APP/Contents/MacOS/LegionControl"

# Match the built copy exactly so the installed app can keep running.
find_pid() { ps -axo pid=,comm= | awk -v binary="$BINARY" 'substr($0,index($0,$2)) == binary {print $1}'; }
while IFS= read -r pid; do
  [[ -z "$pid" ]] || kill -TERM "$pid"
done < <(find_pid)
for _ in {1..25}; do
  [[ -n "$(find_pid)" ]] || break
  sleep 0.2
done
[[ -z "$(find_pid)" ]] || { echo "The previous development app has not exited." >&2; exit 1; }
build_args=(--no-install)
[[ "$MODE" != --debug ]] || build_args+=(--debug)
"$ROOT/mac/build.sh" "${build_args[@]}"
open_args=(-n "$APP")
[[ -z "${LEGION_CONTROL_HOME:-}" ]] || open_args+=(--env "LEGION_CONTROL_HOME=$LEGION_CONTROL_HOME")
/usr/bin/open "${open_args[@]}"
case "$MODE" in
  --debug)
    for _ in {1..20}; do pid="$(find_pid | head -1)"; [[ -z "$pid" ]] || break; sleep 0.2; done
    [[ -n "${pid:-}" ]] || { echo "The app did not launch." >&2; exit 1; }
    exec lldb -p "$pid" ;;
  --logs|--telemetry)
    exec /usr/bin/log stream --info --style compact --predicate 'process == "LegionControl"' ;;
  --verify)
    for _ in {1..20}; do
      if [[ -n "$(find_pid)" ]]; then
        sleep 1
        [[ -z "$(find_pid)" ]] || exit 0
        break
      fi
      sleep 0.2
    done
    echo "The app did not remain running." >&2; exit 1 ;;
esac
