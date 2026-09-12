#!/bin/zsh
# usage: edit_during_render.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR, AUDEC_CACHE_ROOT)
#
# An edit that arrives while a render is in flight cancels that render. It used
# to cancel it and stop there: `status.audio_error` stuck at "project audio
# render was cancelled" for the rest of the session (nothing cleared it on a
# later success), and an export asked for in that window queued behind a cohort
# that nothing was building.
#
# What this scenario shows:
#   1. settled: one complete cohort, `audio_error` null;
#   2. make beat, then a second make beat 1.5 s later, while the first one's
#      render is still filling tiles -- the edit that cancels a render;
#   3. an export requested immediately after, with the newest revision still
#      rendering: `status.io` says which revision it is rendering *for the
#      export* while it waits, and the export completes;
#   4. `audio_error` is null the whole way through: a cancellation is a normal
#      event, not a failure;
#   5. the export taken mid-render is byte-identical to one taken after the
#      same revision has fully settled.
#
# Two make-beats rather than the make-beat/split pair: `audec.clip.split` needs
# a clip selected in a focused arrangement editor, and the socket has no verb
# that selects one. Make beat is the arrangement/sequencer edit that reliably
# lands from outside at this commit, and it cancels an in-flight render by the
# same path a split does.
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}
LOOP_A_START=${2:-2646000}   # 60 s at 44.1 kHz
LOOP_A_END=${3:-2998800}     # 68 s
LOOP_B_START=${4:-3087000}   # 70 s
LOOP_B_END=${5:-3439800}     # 78 s

launch_audec "$MATERIAL" || exit 1

field() {
  ctl '{"op":"status"}' | python3 -c "
import sys, json
r = json.loads(sys.stdin.readline())['result']
print(json.dumps(r.get('$1')))
"
}

# "Ready" is the analysis; the first bounce is still arriving.
wait_for_cohort() {
  for i in {1..180}; do
    ctl '{"op":"status"}' | python3 -c "
import sys, json
r = json.loads(sys.stdin.readline())['result']
sys.exit(0 if (r.get('readiness') or {}).get('required', 0) > 0 else 1)
" && { echo "  first cohort published after ${i}s"; return 0; }
    sleep 1
  done
  echo "  no cohort within 180s"; return 1
}

wait_for_settled() {
  for i in {1..300}; do
    ctl '{"op":"status"}' | python3 -c "
import sys, json
r = json.loads(sys.stdin.readline())['result']
ready = r.get('readiness') or {}
sys.exit(0 if ready.get('required', 0) > 0 and ready.get('missing', 1) == 0 else 1)
" && { echo "  settled after ${i}s"; return 0; }
    sleep 1
  done
  echo "  never settled"; return 1
}

wait_for_export() {
  local label=$1 limit=${2:-300}
  for i in $(seq 1 $limit); do
    local io=$(field io)
    case "$io" in
      *EXPORTED*) echo "  $label exported after ${i}s"; return 0 ;;
      *"FILE ERROR"*) echo "  $label FAILED: $io"; return 1 ;;
    esac
    sleep 1
  done
  echo "  $label never finished: $(field io)"; return 1
}

wait_for_cohort || exit 1
wait_for_settled || exit 1

echo "=== settled before any edit"
echo "  revision:    $(field revision)"
echo "  audio_error: $(field audio_error)"
echo "  readiness:   $(field readiness)"

echo "=== make beat, then a second edit 1.5 s into its render"
ctl "{\"op\":\"select\",\"start\":$LOOP_A_START,\"end\":$LOOP_A_END}" \
    "{\"op\":\"loop\",\"start\":$LOOP_A_START,\"end\":$LOOP_A_END}" >/dev/null
ctl '{"op":"action","id":"audec.sample.make_beat"}' | python3 -c "
import sys, json
print('  beat A:', json.loads(sys.stdin.readline())['result'].get('notice'))
"
sleep 1.5
ctl "{\"op\":\"select\",\"start\":$LOOP_B_START,\"end\":$LOOP_B_END}" \
    "{\"op\":\"loop\",\"start\":$LOOP_B_START,\"end\":$LOOP_B_END}" >/dev/null
ctl '{"op":"action","id":"audec.sample.make_beat"}' | python3 -c "
import sys, json
print('  beat B:', json.loads(sys.stdin.readline())['result'].get('notice'))
"
TARGET_REVISION=$(field revision)
echo "  newest revision after the cancelling edit: $TARGET_REVISION"

echo "=== export requested while that revision is still rendering"
ctl "{\"op\":\"export\",\"path\":\"$LIVE/during.wav\"}" >/dev/null
# The wait itself is the claim: `io` must name the revision it is rendering for
# and `audio_error` must stay null while the cancelled render is replaced.
waited_label=""
errors=""
for i in {1..300}; do
  io=$(field io)
  err=$(field audio_error)
  [ "$err" = "null" ] || errors="$errors\n  t+${i}s audio_error=$err"
  case "$io" in
    *"rendering revision"*) [ -n "$waited_label" ] || waited_label="$io"; echo "  t+${i}s $io" ;;
    *EXPORTED*) echo "  mid-render export finished after ${i}s"; break ;;
    *"FILE ERROR"*) echo "  mid-render export FAILED: $io"; break ;;
  esac
  sleep 1
done
echo "  waited on:   ${waited_label:-(the render was already compiled)}"
echo "  audio_error: $(field audio_error)"
echo "  readiness:   $(field readiness)"

echo "=== same revision, fully settled, exported again"
wait_for_settled || exit 1
ctl "{\"op\":\"export\",\"path\":\"$LIVE/settled.wav\"}" >/dev/null
wait_for_export "settled export" || exit 1
echo "  revision:    $(field revision)"
echo "  audio_error: $(field audio_error)"

echo "=== the two exports"
ls -l $LIVE/during.wav $LIVE/settled.wav 2>/dev/null | awk '{print "  " $9 " " $5 " bytes"}'
verdict=0
if [ "$(field revision)" != "$TARGET_REVISION" ]; then
  echo "  REVISION MOVED after the export: $TARGET_REVISION -> $(field revision)"
  verdict=1
fi
if cmp -s $LIVE/during.wav $LIVE/settled.wav; then
  echo "  BYTE IDENTICAL: the export that waited out the cancelled render is the settled master"
else
  echo "  DIFFER: the mid-render export is not the settled master"
  verdict=1
fi
if [ -n "$errors" ]; then
  echo "  audio_error was set while a cancellation was being recovered:$(printf "$errors")"
  verdict=1
else
  echo "  audio_error stayed null across the cancellation and both exports"
fi
[ "$(field audio_error)" = "null" ] || verdict=1

echo "=== VERDICT: $([ $verdict = 0 ] && echo 'a cancelled render is re-requested and the export is the settled master' || echo 'NOT SHOWN')"
ctl '{"op":"quit"}' >/dev/null 2>&1 || kill $(cat $LIVE/audec.pid) 2>/dev/null
exit $verdict
