#!/bin/zsh
# Shared launcher for live scenarios. Usage from a scenario: source common.sh; launch_audec <material> [binary]
# Environment: AUDEC_LIVE_DIR (scratch dir, default $TMPDIR/audec-live), AUDEC_BIN (default target/debug/audec).
HERE=${0:A:h}
REPO=${HERE:h:h}
export LIVE=${AUDEC_LIVE_DIR:-${TMPDIR:-/tmp}/audec-live}
mkdir -p $LIVE
export AUDEC_CONTROL_SOCKET=${AUDEC_CONTROL_SOCKET:-${TMPDIR:-/tmp}/audec-control.sock}
ctl() { python3 $HERE/ctl.py "$@"; }
ctl_status() { python3 $HERE/ctl.py "$@" | python3 $HERE/fmt_status.py; }
launch_audec() {
  local material=$1; local bin=${2:-${AUDEC_BIN:-$REPO/target/debug/audec}}
  if [ -f $LIVE/audec.pid ]; then kill $(cat $LIVE/audec.pid) 2>/dev/null; sleep 1; fi
  rm -f $AUDEC_CONTROL_SOCKET
  (cd $REPO && RUST_BACKTRACE=1 nohup $bin "$material" > $LIVE/app.log 2>&1 &; echo $! > $LIVE/audec.pid)
  for i in {1..60}; do [ -S $AUDEC_CONTROL_SOCKET ] && break; sleep 1; done
  for i in {1..180}; do ctl '{"op":"status"}' 2>/dev/null | grep -q '"state": "ready"' && break; sleep 1; done
  echo "audec ready (pid $(cat $LIVE/audec.pid)) after ${i}s"
}
stop_audec() { ctl '{"op":"stop"}' >/dev/null 2>&1; }
