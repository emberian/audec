#!/usr/bin/env bash
# Development-only Linux launcher. It does not package the application or
# claim a compositor/audio-device compatibility guarantee.
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "launch-linux.sh must be run on Linux" >&2
  exit 2
fi

profile="${AUDEC_PROFILE:-debug}"
case "$profile" in
  debug|release) ;;
  *)
    echo "AUDEC_PROFILE must be debug or release" >&2
    exit 2
    ;;
esac

# Resolve the development binary relative to this repository, but deliberately
# retain the caller's working directory.  That way `launch-linux.sh song.flac`
# keeps resolving a relative audio/project argument exactly as the caller
# expects when the launcher is invoked outside the checkout root.
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repository_root="$(cd -- "${script_dir}/.." && pwd -P)"
binary="${repository_root}/target/${profile}/audec"
if [[ ! -x "$binary" ]]; then
  if [[ "$profile" == "release" ]]; then
    hint="cd '$repository_root' && cargo build --release --bin audec"
  else
    hint="cd '$repository_root' && cargo build --bin audec"
  fi
  echo "$binary does not exist; run: $hint" >&2
  exit 1
fi

if [[ -z "${WAYLAND_DISPLAY:-}" && -z "${DISPLAY:-}" ]]; then
  echo "no Wayland or X11 display is available; Linux runtime smoke coverage is not configured" >&2
  exit 1
fi

exec "$binary" "$@"
