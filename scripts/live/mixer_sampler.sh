#!/bin/zsh
# usage: mixer_sampler.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR, AUDEC_CACHE_ROOT)
#
# Two rows of the DAW audit, on the desktop.
#
#   7  reorder inserts. `audec.mixer.insert_filter` then
#      `audec.mixer.insert_compressor` build a two-effect chain on the master
#      and `audec.mixer.move_insert_up` moves the compressor in front of the
#      filter; the receipts name each insert and the move. The *audible* half
#      of this row is proved where it is cheap: a compressor declares a
#      history bound no tile context can satisfy, so a master carrying one
#      renders whole bounces — minutes per export on a debug build — and
#      `engine_regression::the_order_of_two_inserts_changes_what_the_master_renders`
#      asserts the two orders differ, and that moving the chain back restores
#      the first order bit-for-bit.
#  10  reverse a sample, with the export diff. This half runs on a freshly
#      launched instance with no insert on the master, so the render tiles and
#      the two exports are a minute each: make a beat from a selection,
#      reverse its zone with `audec.sample.reverse_zone`, export again, and
#      the diff lands exactly on the pattern's hits.
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}

notice() {
  ctl '{"op":"status"}' | python3 -c '
import sys, json
r = json.loads(sys.stdin.readline())["result"]
print("  revision", r.get("revision"), " audio_error", r.get("audio_error"))
print("  notice:", r.get("notice"))
'
}

wait_export() {
  local label=$1 target=$2
  for i in {1..600}; do
    st=$(ctl '{"op":"status"}')
    echo "$st" | grep -q "EXPORTED · $target" && { echo "  $label exported after ${i}s ($(stat -f%z $target 2>/dev/null) bytes)"; return 0; }
    echo "$st" | grep -q 'FILE ERROR' && { echo "  $label FAILED: $st"; return 1; }
    sleep 1
  done
  echo "  $label timed out"; return 1
}

launch_audec "$MATERIAL" || exit 1
rm -f $LIVE/beat_fwd.wav $LIVE/beat_rev.wav

echo "=== the four actions this scenario needs are registered"
ctl '{"op":"actions"}' | python3 -c '
import sys, json
result = json.loads(sys.stdin.readline())["result"]
rows = result["actions"] if isinstance(result, dict) else result
wanted = {"audec.mixer.insert_filter", "audec.mixer.insert_compressor",
          "audec.mixer.move_insert_up", "audec.sample.reverse_zone"}
found = {row["id"] for row in rows} & wanted
for id in sorted(wanted):
    print("  ", id, "REGISTERED" if id in found else "MISSING")
'

echo
echo "### row 7: an insert chain the musician can reorder"
echo "=== with an empty chain there is nowhere to move to: refused by name"
ctl '{"op":"action","id":"audec.mixer.move_insert_up"}'

echo "=== one insert is still nowhere to move to"
ctl '{"op":"action","id":"audec.mixer.insert_filter"}'
ctl '{"op":"action","id":"audec.mixer.move_insert_up"}'

echo "=== a second insert, and now the chain has an order"
ctl '{"op":"action","id":"audec.mixer.insert_compressor"}'

echo "=== audec.mixer.move_insert_up: the compressor moves in front of the filter"
ctl '{"op":"action","id":"audec.mixer.move_insert_up"}'

echo "=== the action addresses whichever insert is last, so pressing it again"
echo "    moves the filter back in front and the chain is where it started"
ctl '{"op":"action","id":"audec.mixer.move_insert_up"}'

echo
echo "### row 10: a zone the musician can reverse, heard in the export"
echo "=== fresh instance, so the master has no insert and the render tiles"
launch_audec "$MATERIAL" || exit 1

echo "=== select 60-68 s, make a beat"
ctl '{"op":"select","start":2646000,"end":2998800}' '{"op":"loop","start":2646000,"end":2998800}' >/dev/null
sleep 3
ctl '{"op":"action","id":"audec.sample.make_beat"}'
sleep 2
notice

echo "=== export the beat, zone forwards"
ctl "{\"op\":\"export\",\"path\":\"$LIVE/beat_fwd.wav\"}" >/dev/null
wait_export beat_fwd $LIVE/beat_fwd.wav || exit 1

echo "=== audec.sample.reverse_zone"
ctl '{"op":"action","id":"audec.sample.reverse_zone"}'
sleep 2
notice

echo "=== export the beat, zone reversed"
ctl "{\"op\":\"export\",\"path\":\"$LIVE/beat_rev.wav\"}" >/dev/null
wait_export beat_rev $LIVE/beat_rev.wav || exit 1

echo "=== the reversed zone changes the master, and where"
LEFT=$LIVE/beat_fwd.wav RIGHT=$LIVE/beat_rev.wav LABEL="zone reverse" python3 ${0:A:h}/diff_pcm.py

ctl '{"op":"quit"}' >/dev/null 2>&1
echo "done"
