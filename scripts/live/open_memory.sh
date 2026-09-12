#!/bin/zsh
# usage: open_memory.sh <material> [passes]   (env: AUDEC_BIN, AUDEC_LIVE_DIR)
# What one open costs: seconds from launch to status.state == "ready" and the
# app's resident memory once it is there. Runs twice by default because the
# second open of the same material must be a decoded-image cache hit.
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}
PASSES=${2:-2}
CACHE_DIR=${AUDEC_CACHE_ROOT:-$HOME/Library/Caches/software.ember.audec}/decoded-material
for pass in $(seq 1 $PASSES); do
  started=$(python3 -c 'import time; print(time.time())')
  launch_audec "$MATERIAL" || exit 1
  ready=$(python3 -c 'import time; print(time.time())')
  pid=$(cat $LIVE/audec.pid)
  rss_kb=$(ps -o rss= -p $pid | tr -d ' ')
  seconds=$(python3 -c "print(f'{$ready - $started:.1f}')")
  rss_mb=$(python3 -c "print(f'{$rss_kb / 1024:.0f}')")
  # Ready is not quiet: the component factorization and the first render keep
  # allocating. Settle, then report what the open actually costs to hold.
  sleep ${AUDEC_SETTLE_SECONDS:-10}
  settled_kb=$(ps -o rss= -p $pid | tr -d ' ')
  settled_mb=$(python3 -c "print(f'{$settled_kb / 1024:.0f}')")
  # Resident bytes include clean, evictable pages of a mapped image; the
  # physical footprint is what the operating system charges the process.
  footprint=$(vmmap --summary $pid 2>/dev/null | grep -i 'Physical footprint:' | head -1 | awk '{print $3}')
  echo "open $pass: ${seconds}s to ready, ${rss_mb} MB RSS at ready, ${settled_mb} MB settled, ${footprint:-n/a} footprint (pid $pid)"
  echo "open $pass phases: $(grep -c 'open phase' $LIVE/app.log 2>/dev/null || echo 0) logged"
  grep 'open phase' $LIVE/app.log 2>/dev/null | tail -8
  stop_audec; sleep 1; kill -9 $pid 2>/dev/null
done
if [ -d $CACHE_DIR ]; then
  echo "decoded-material cache: $(du -sh $CACHE_DIR | awk '{print $1}') in $CACHE_DIR"
  ls -la $CACHE_DIR | tail -5
else
  echo "decoded-material cache: absent ($CACHE_DIR)"
fi
