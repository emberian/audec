#!/bin/zsh
# usage: tempo_and_routing.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR)
#
# Musical time a musician can place, on the desktop, through the control socket:
#   1. audec.tempo.mark_at_playhead puts a tempo point at the playhead's bar;
#   2. audec.tempo.increase then bends that segment only - the tempo at 60 s
#      moves, the tempo at 0 s does not;
#   3. audec.meter.cycle_at_playhead changes the time signature at that bar,
#      or shows the sequencer's own refusal;
#   4. a beat is made in 60-68 s and every mixer bus is exported: the beat's
#      bus carries the beat and nothing else, which is exactly the read that
#      ArrangementAction::RouteTrackToBus feeds (the header's bus button is a
#      pointer control; no action id reaches it from outside).
# Phases 1-3 and phase 4 each start the app fresh, so a tempo edit never
# rewrites the beat the export comparison is measuring.
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}

mt() {
  ctl '{"op":"status"}' | python3 -c '
import sys, json
r = json.loads(sys.stdin.readline())["result"]
print("  playhead", round(r.get("playhead_seconds") or 0.0, 3), "s  playing", r.get("playing"), "  musical_time", r.get("musical_time"))
print("  notice:", r.get("notice"))
'
}

# The io status keeps the last EXPORTED path, so a wait must name the path it
# is waiting for or it will accept the previous export.
wait_export() {
  # Never name a local `path` in zsh: it is tied to PATH and assigning a
  # string to it empties the command search path for the rest of the run.
  local label=$1 target=$2
  for i in {1..300}; do
    st=$(ctl '{"op":"status"}')
    echo "$st" | grep -qE "EXPORTED (·|\\u00b7) $target" && { echo "  $label exported after ${i}s ($(stat -f%z $target 2>/dev/null) bytes)"; return 0; }
    echo "$st" | grep -q 'FILE ERROR' && { echo "  $label FAILED: $st"; return 1; }
    sleep 1
  done
  echo "  $label timed out"; return 1
}

echo "########## phase A: tempo and meter at the playhead"
launch_audec "$MATERIAL" || exit 1
echo "=== stop the transport and seek to 60 s"
# A moving playhead is a moving target: every one of these verbs acts where
# the playhead is, so the scenario parks it first.
ctl '{"op":"stop"}' >/dev/null
ctl '{"op":"seek","seconds":60}' >/dev/null
mt
echo "=== audec.tempo.mark_at_playhead"
ctl '{"op":"action","id":"audec.tempo.mark_at_playhead"}' >/dev/null
mt
echo "=== the same mark again is a no-op that says so"
ctl '{"op":"action","id":"audec.tempo.mark_at_playhead"}' >/dev/null
mt
echo "=== audec.tempo.increase x10, one line each"
for i in {1..10}; do
  ctl '{"op":"action","id":"audec.tempo.increase"}' >/dev/null
  mt
done
echo "=== at 0 s the opening tempo is untouched"
ctl '{"op":"seek","seconds":0}' >/dev/null
mt
echo "=== back at 60 s"
ctl '{"op":"seek","seconds":60}' >/dev/null
mt
echo "=== audec.meter.cycle_at_playhead"
ctl '{"op":"action","id":"audec.meter.cycle_at_playhead"}' >/dev/null
mt
echo "=== and again: 3/4 -> 6/8"
ctl '{"op":"action","id":"audec.meter.cycle_at_playhead"}' >/dev/null
mt
echo "=== undo puts the last change back"
ctl '{"op":"action","id":"audec.edit.undo"}' >/dev/null
mt

echo
echo "########## phase B: a beat, and what each mixer bus carries"
launch_audec "$MATERIAL" || exit 1
ctl '{"op":"select","start":2646000,"end":2998800}' '{"op":"loop","start":2646000,"end":2998800}' >/dev/null
sleep 3
ctl '{"op":"action","id":"audec.sample.make_beat"}' >/dev/null
sleep 2
ctl '{"op":"status"}' | python3 -c 'import sys,json;r=json.loads(sys.stdin.readline())["result"];print("  after make_beat:",{k:r.get(k) for k in ("revision","audio_error","notice")})'
rm -f $LIVE/master.wav $LIVE/bus_*.wav
ctl "{\"op\":\"export\",\"path\":\"$LIVE/master.wav\"}" >/dev/null
wait_export master $LIVE/master.wav
BUSES=()
for bus in 1 2 3 4 5 6; do
  reply=$(ctl "{\"op\":\"export\",\"path\":\"$LIVE/bus_$bus.wav\",\"scope\":\"bus:$bus\"}")
  if echo "$reply" | grep -q '"error"'; then
    echo "  bus:$bus is not an exportable scope: $reply"
    continue
  fi
  if wait_export "bus:$bus" $LIVE/bus_$bus.wav; then BUSES+=$bus; fi
done
echo "exported buses: $BUSES"
python3 - <<'PY'
import numpy as np, subprocess, os, glob
S = os.environ["LIVE"]
sr = 44100
def load(p):
    raw = subprocess.run(["sox", p, "-t", "raw", "-e", "float", "-b", "32", "-c", "2", "-"],
                         capture_output=True).stdout
    return np.frombuffer(raw, dtype=np.float32).reshape(-1, 2)
def rms(x): return float(np.sqrt(np.mean(x ** 2))) if len(x) else 0.0
for path in [f"{S}/master.wav"] + sorted(glob.glob(f"{S}/bus_*.wav")):
    if not os.path.exists(path):
        continue
    a = load(path)
    print(f"{os.path.basename(path):12s} frames {len(a):9d}  whole {rms(a):.5f}"
          f"  in 60-68s {rms(a[60*sr:68*sr]):.5f}"
          f"  outside 100-110s {rms(a[100*sr:110*sr]):.5f}")
PY
ctl '{"op":"quit"}' >/dev/null 2>&1
echo "done"
