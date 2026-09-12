#!/bin/zsh
# usage: lens_memory.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR, AUDEC_CACHE_ROOT)
#
# What every lens costs the app, measured rather than argued. Opens the
# material, then for each lens: opens its pane, reads resident memory, drives
# its refresh through the `lens` verb, waits for it to settle, and reads
# resident memory again. The waterfall is measured twice more, once for the
# constant-Q transform and once for a larger FFT, because those are the two
# controls that used to materialise the whole song.
#
# RSS is `ps -o rss=` in KiB on the app's own pid, so it is the number a debug
# build actually holds -- no release build required.
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}
launch_audec "$MATERIAL" || exit 1
PID=$(cat $LIVE/audec.pid)

rss() { ps -o rss= -p $PID | tr -d ' '; }
mb() { python3 -c "import sys; print('%.1f MB' % (int(sys.argv[1]) / 1024.0))" $1; }
act() { ctl "{\"op\":\"action\",\"id\":\"$1\"}" >/dev/null; }
lens() { ctl "{\"op\":\"lens\",\"view\":$1,\"control\":\"$2\"}"; }
av() { ctl '{"op":"status"}' | python3 -c 'import sys,json; print(json.loads(sys.stdin.readline())["result"].get("active_view"))'; }

# One sampler: watch RSS every 500 ms and remember the peak. A lens transform
# runs on a background executor, so the verb returns long before the work
# starts; stopping at the first quiet moment measures an idle app and misses
# the peak entirely (an earlier draft of this script reported 0.0 MB for a
# transform that allocates 66 MB). The rule is therefore conservative: watch
# for at least STILL_TICKS of unchanged RSS after at least MIN_TICKS of
# sampling, and give up at MAX_TICKS so a stuck transform is reported rather
# than waited on forever.
MIN_TICKS=60     # 30 s
STILL_TICKS=60   # 30 s of an unmoving RSS means nothing is running
MAX_TICKS=480    # 4 minutes
watch_transform() {
  local peak=0 last=-1 same=0 ticks=0
  while [ $ticks -lt $MAX_TICKS ]; do
    local now=$(rss)
    [ -z "$now" ] && break
    [ "$now" -gt "$peak" ] && peak=$now
    if [ "$now" = "$last" ]; then same=$((same + 1)); else same=0; fi
    last=$now
    ticks=$((ticks + 1))
    [ $same -ge $STILL_TICKS ] && [ $ticks -ge $MIN_TICKS ] && break
    sleep 0.5
  done
  echo "$peak $last $ticks"
}

open_rss=$(rss)
echo "0. open            RSS $(mb $open_rss)   ($open_rss KiB)"

measure() {  # measure <label> <action-id> <controls...>
  local label=$1 action=$2; shift 2
  act $action
  sleep 3
  local view=$(av)
  local before=$(rss)
  echo "$label pane open   RSS $(mb $before)   (view $view)"
  for control in "$@"; do
    local reply=$(lens $view $control)
    local watched=($(watch_transform))
    local peak=${watched[1]} after=${watched[2]} ticks=${watched[3]}
    echo "   $control: peak $(mb $peak)  settled $(mb $after)  peak-over-before $(mb $((peak - before)))  ($((ticks / 2))s)"
    case "$reply" in
      *'"error"'*) echo "      refused: $reply" ;;
    esac
    before=$after
  done
}

measure "1. waterfall  " audec.lens.waterfall refresh spectral-transform spectral-transform fft-size-up refresh
measure "2. rhythm     " audec.lens.rhythm refresh
measure "3. separation " audec.lens.separation refresh
measure "4. loom       " audec.lens.loom refresh

final=$(rss)
echo "5. all four lenses open  RSS $(mb $final)  (open was $(mb $open_rss))"
echo "=== app log ==="; grep -v 'control socket listening' $LIVE/app.log | head -25
ctl '{"op":"quit"}' >/dev/null 2>&1
