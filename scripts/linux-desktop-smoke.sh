#!/usr/bin/env bash
# Bounded Linux/X11 startup smoke. This proves that the real Audec binary can
# create and keep a visible GPUI window alive; it deliberately does not claim
# portal-dialog, physical-audio-device, or Wayland coverage.
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "linux-desktop-smoke.sh must be run on Linux" >&2
  exit 2
fi

if [[ -z "${DISPLAY:-}" ]]; then
  echo "DISPLAY is unset; run this script inside Xvfb or an X11 session" >&2
  exit 2
fi

for command in xdotool xwininfo; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "$command is required for the Linux desktop smoke" >&2
    exit 2
  fi
done

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repository_root="$(cd -- "${script_dir}/.." && pwd -P)"
binary="${1:-${repository_root}/target/debug/audec}"
if [[ $# -gt 0 ]]; then
  shift
fi
if [[ ! -x "$binary" ]]; then
  echo "Audec binary is not executable: $binary" >&2
  exit 2
fi

log_path="${AUDEC_SMOKE_LOG:-${repository_root}/target/linux-desktop-smoke.log}"
mkdir -p -- "$(dirname -- "$log_path")"
: >"$log_path"

app_pid=""
cleanup() {
  if [[ "$app_pid" =~ ^[0-9]+$ ]] && kill -0 "$app_pid" 2>/dev/null; then
    kill -TERM "$app_pid" 2>/dev/null || true
    for _ in $(seq 1 50); do
      if ! kill -0 "$app_pid" 2>/dev/null; then
        break
      fi
      sleep 0.1
    done
    if kill -0 "$app_pid" 2>/dev/null; then
      # The process belongs to this disposable smoke invocation. Do not leave
      # it behind when GPUI ignores or cannot receive SIGTERM.
      kill -KILL "$app_pid" 2>/dev/null || true
    fi
    wait "$app_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT HUP INT TERM

"$binary" "$@" >"$log_path" 2>&1 &
app_pid=$!

window_id=""
window_name=""
for _ in $(seq 1 300); do
  if ! kill -0 "$app_pid" 2>/dev/null; then
    echo "Audec exited before creating a visible X11 window" >&2
    sed -n '1,240p' "$log_path" >&2
    exit 1
  fi
  while IFS= read -r candidate; do
    if [[ ! "$candidate" =~ ^[0-9]+$ ]]; then
      continue
    fi
    candidate_name="$(xdotool getwindowname "$candidate" 2>/dev/null || true)"
    if [[ "${candidate_name,,}" == *audec* ]]; then
      window_id="$candidate"
      window_name="$candidate_name"
      break
    fi
  done < <(xdotool search --onlyvisible --pid "$app_pid" 2>/dev/null || true)
  if [[ "$window_id" =~ ^[0-9]+$ ]]; then
    break
  fi
  sleep 0.1
done

if [[ ! "$window_id" =~ ^[0-9]+$ ]]; then
  echo "Audec stayed alive but exposed no titled, visible X11 window within 30 seconds" >&2
  sed -n '1,240p' "$log_path" >&2
  exit 1
fi

if ! xwininfo -id "$window_id" >/dev/null 2>&1; then
  echo "Audec's reported X11 window cannot be inspected" >&2
  sed -n '1,240p' "$log_path" >&2
  exit 1
fi

# Catch immediate render/event-loop failures after the first map.
sleep 2
if ! kill -0 "$app_pid" 2>/dev/null; then
  echo "Audec exited immediately after mapping its X11 window" >&2
  sed -n '1,240p' "$log_path" >&2
  exit 1
fi

echo "visible Audec X11 window: id=$window_id title=$window_name"
