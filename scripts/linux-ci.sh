#!/usr/bin/env bash
# Linux CI entry points. These commands intentionally never open a GPUI window
# or an ALSA device unless the explicit desktop-smoke lane is selected.
set -euo pipefail

case "${1:-}" in
  workers)
    cargo build --locked \
      --bin audec-fake-worker \
      --bin audec-fake-plugin \
      --bin audec-clap-worker
    # The real model target is source-pinned and artifact-free at build time.
    # Compiling it here catches RTen/Linux drift without downloading weights or
    # claiming that an inference golden ran on the hosted worker.
    cargo check --locked \
      --features beat-this-rten-worker \
      --bin audec-beat-this-worker
    cargo test --locked \
      --test plugin_host_process \
      --test clap_worker_process
    ;;
  app)
    cargo build --locked --bin audec
    # There is no library/core-only target yet: main owns the domain modules.
    # Compile every unit-test harness without trying to connect to a display or
    # audio device on a headless GitHub-hosted runner.
    cargo test --locked --bin audec --no-run
    ;;
  desktop-smoke)
    # The app lane builds this exact binary first. The smoke process is bounded
    # and checks a real visible X11 window under the caller-provided display.
    scripts/linux-desktop-smoke.sh target/debug/audec
    ;;
  *)
    echo "usage: $0 {workers|app|desktop-smoke}" >&2
    exit 2
    ;;
esac
