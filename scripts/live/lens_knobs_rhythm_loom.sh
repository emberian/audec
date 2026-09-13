#!/bin/zsh
# usage: lens_knobs_rhythm_loom.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR, AUDEC_CACHE_ROOT)
#
# The two analysis lenses that had no parameters. A musician asks the onset
# detector a different question (SENS, PULSE) and Loom a different one (WINDOW,
# LEN), refreshes, and reads back what changed: the number of hits and families
# the rhythm deprojection found, and the template length Loom's header names.
# Then the plots are pressed: a press on a painted rhythm hit seeks to that hit,
# and a press on a painted Loom event chooses the event the edits land on.
# Every refusal is printed in the app's own words.
#
# Rhythm deprojection reads the whole material: 280-510 s for a 6-minute song
# on a debug build. This scenario cuts a 40 s excerpt with sox so the knob can
# be turned three times inside a few minutes.
#
# A knob is remembered in the preferences file, and `dirs::config_dir()` has no
# environment override, so a scenario that turns one would write the musician's
# real file -- shared with every other lane running live at the same time. This
# run gets its own HOME instead, so its preferences, tile cache and default
# store are all its own and nobody else's move under it.
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}
HERE=${0:A:h}

export HOME=$LIVE/home
mkdir -p $HOME
PREFS=$HOME/Library/Application\ Support/software.ember.audec/preferences.json
echo "this run has its own HOME: $HOME"

EXCERPT=$LIVE/excerpt-40s.wav
if [ ! -f $EXCERPT ]; then
  echo "cutting a 40 s excerpt from $(basename $MATERIAL) with sox (a whole-song rhythm deprojection is minutes on a debug build)"
  sox "$MATERIAL" $EXCERPT trim 60 40 || exit 1
fi
soxi -d $EXCERPT

launch_audec "$EXCERPT" || exit 1

act() { ctl "{\"op\":\"action\",\"id\":\"$1\"}"; }
lens_view() { ctl '{"op":"status"}' | python3 -c "
import sys, json
for lens in json.loads(sys.stdin.readline())['result']['lenses']:
    if lens['kind'] == '$1':
        print(lens['view']); break
"; }
lens_row() { ctl '{"op":"status"}' | python3 -c "
import sys, json
for lens in json.loads(sys.stdin.readline())['result']['lenses']:
    if lens['kind'] != '$1':
        continue
    s = dict(lens.get('settings') or {})
    found = s.pop('result', None)
    print('   %-11s %-9s findings=%-3s knobs=%s' % (
        lens['kind'], lens['state'], lens['findings'],
        json.dumps(s, sort_keys=True)))
    print('   %-11s what it found: %s' % ('', json.dumps(found, sort_keys=True)))
    if lens.get('failure'):
        print('   failure: %s' % lens['failure'])
"; }
# The one number the audit row is about: how many exact hits the detector found.
hits() { ctl '{"op":"status"}' | python3 -c "
import sys, json
for lens in json.loads(sys.stdin.readline())['result']['lenses']:
    if lens['kind'] == 'Rhythm':
        found = (lens.get('settings') or {}).get('result') or {}
        print(found.get('hits', '-')); break
"; }
# A lens control, printing the app's own reply or its refusal verbatim.
knob() { ctl "{\"op\":\"lens\",\"view\":$1,\"control\":\"$2\"}" | python3 -c '
import sys, json
d = json.loads(sys.stdin.readline())
if not d.get("ok"):
    print("   REFUSED: %s" % d["error"]); raise SystemExit(0)
r = d["result"]
s = dict(r.get("settings") or {})
s.pop("result", None)
print("   %s %s knobs=%s" % (r["kind"], r["state"], json.dumps(s, sort_keys=True)))
'; }
notice() { ctl '{"op":"status"}' | python3 -c 'import sys,json; print("   notice:", json.loads(sys.stdin.readline())["result"]["notice"])'; }
playhead() { ctl '{"op":"status"}' | python3 -c 'import sys,json; r=json.loads(sys.stdin.readline())["result"]; print("   playhead: %.3f s (sample %s)" % (r["playhead_seconds"], r["playhead_sample"]))'; }
wait_for_state() {  # wait_for_state <Kind> <view> <polls>
  local kind=$1 polls=${3:-400}
  for i in $(seq 1 $polls); do
    local state=$(ctl '{"op":"status"}' | python3 -c "
import sys, json
for lens in json.loads(sys.stdin.readline())['result']['lenses']:
    if lens['kind'] == '$kind':
        print(lens['state']); break
")
    [[ "$state" == "Ready" || "$state" == "Failed" ]] && { echo "   $kind reached $state after about ${i}s"; return 0; }
    sleep 1
  done
  echo "   $kind never settled within $polls polls"; return 1
}
# The family-row text the lens draws, read through the deprojection the socket
# reports: hits in view, and how many families got a row.
findings_count() { ctl '{"op":"status"}' | python3 -c "
import sys, json
print(sum(1 for row in json.loads(sys.stdin.readline())['result']['findings'] if row['lens'] == '$1'))
"; }

echo "1. name the rhythm lens; it deprojects at the knobs it was created with"
act audec.lens.rhythm >/dev/null
sleep 1
RH=$(lens_view Rhythm)
echo "   rhythm lens is view $RH"
wait_for_state Rhythm $RH 400
lens_row Rhythm
BEFORE=$(hits)
echo "   exact hits at SENS 2.8x: $BEFORE"
notice

echo "2. ask the detector a more sensitive question: SENS - twice"
knob $RH rhythm-sens-down
notice
knob $RH rhythm-sens-down
notice
echo "   the knob moved but the deprojection on screen has not; settings.stale says so"
lens_row Rhythm

echo "3. refresh: the same audio, a different question"
knob $RH refresh
wait_for_state Rhythm $RH 400
lens_row Rhythm
AFTER=$(hits)
echo "   exact hits at SENS 2.8x: $BEFORE   at SENS 2.0x: $AFTER"
notice

echo "4. the pulse window cycles, and says which one it is on"
knob $RH rhythm-bpm-range
notice
knob $RH rhythm-bpm-range
notice

echo "5. the rhythm lens published one Finding, and says so instead of cycling"
knob $RH finding-next
notice

echo "6. press a painted rhythm hit: it seeks to that hit, not to the x"
ctl '{"op":"seek","seconds":0}' >/dev/null
playhead
for POS in 0.10,0.10 0.30,0.10 0.50,0.30 0.70,0.50; do
  echo "   press at ($POS) of the plot:"
  knob $RH "rhythm-press:$POS"
  notice
  playhead
done

echo "7. rhythm refusals, verbatim"
echo "7a. a Loom control on the rhythm lens"
knob $RH loom-len-up
echo "7b. a press outside the plot"
knob $RH "rhythm-press:1.4,0.2"
echo "7c. a press that is not a pair of fractions"
knob $RH "rhythm-press:middle"
echo "7d. SENS below the range the detector is offered"
for i in {1..6}; do knob $RH rhythm-sens-down >/dev/null; done
notice
knob $RH rhythm-sens-down
notice

echo "8. Loom: name it, infer at the knobs it has"
act audec.lens.loom >/dev/null
sleep 1
LM=$(lens_view Loom)
echo "   Loom lens is view $LM"
knob $LM refresh
wait_for_state Loom $LM 300
lens_row Loom
notice

echo "9. a longer template: LEN + twice takes 240 ms to 1000 ms"
knob $LM loom-len-up
notice
knob $LM loom-len-up
notice
echo "10. a longer lookbehind: WINDOW +"
knob $LM loom-window-up
notice

echo "10b. reach the 2nd of Loom's four Findings: the header opened index 0 always"
knob $LM finding-next
notice
lens_row Loom
knob $LM finding-next
knob $LM finding-prev
notice

echo "11. reinfer, and read the length the header now names"
knob $LM refresh
wait_for_state Loom $LM 300
lens_row Loom
notice

echo "11b. a longer template is a longer template: template_samples is pre-roll + LEN"
python3 - <<'PY2'
rate = 44100
for ms in (240, 1000):
    print("   %4d ms at %d Hz -> %d pre-roll + %d post-roll = %d samples" % (
        ms, rate, round(rate * 0.008), round(rate * ms / 1000.0),
        round(rate * 0.008) + round(rate * ms / 1000.0)))
PY2

echo "12. press a painted Loom event: it becomes the event the edits land on"
for POS in 0.20,0.20 0.40,0.20 0.60,0.50; do
  echo "   press at ($POS) of the event plot:"
  knob $LM "loom-press:$POS"
  notice
done
echo "   an edit now says which event it is obeying, and it is the pressed one"
knob $LM loom-event-toggle
notice
knob $LM loom-event-toggle
notice
echo "   cycling clusters gives the target back to the playhead"
knob $LM loom-cluster-next
knob $LM loom-event-toggle
notice
knob $LM loom-event-toggle >/dev/null

echo "13. Loom refusals, verbatim"
echo "13a. a rhythm control on the Loom lens"
knob $LM rhythm-sens-up
echo "13b. WINDOW past the longest the lens offers"
knob $LM loom-window-up
notice
echo "13c. LEN past the longest the lens offers"
knob $LM loom-len-up
notice
echo "13d. a Finding step on a lens that publishes none"
WF=$(lens_view Waterfall)
knob $WF finding-next
echo "13e. a control no lens has"
knob $LM loom-len-sideways

echo "14. the knobs are remembered, in this run's own preferences file"
python3 - "$PREFS" <<'PREFS_EOF'
import json, sys, pathlib
path = pathlib.Path(sys.argv[1])
print("   %s" % path)
if path.exists():
    data = json.loads(path.read_text())
    print("   rhythm: %s" % json.dumps(data.get("rhythm")))
    print("   loom:   %s" % json.dumps(data.get("loom")))
    print("   spectrum, which this lane never writes: %s" % json.dumps(data.get("spectrum")))
else:
    print("   (no preferences file)")
PREFS_EOF

echo "=== app log ==="; grep -v 'control socket listening' $LIVE/app.log | head -20
ctl '{"op":"quit"}' >/dev/null 2>&1
