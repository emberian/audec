#!/bin/zsh
# usage: autosave_and_tempo.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR,
#        AUDEC_CONTROL_SOCKET, AUDEC_CACHE_ROOT, AUDEC_RECOVERY_ROOT)
#
# Four things a musician has to be able to trust, driven on the desktop:
#   A. a project that was never saved is still autosaved. The document has no
#      package, so it is given one under the recovery root, and the status line
#      says AUTOSAVED (and that the project is still unsaved) instead of
#      borrowing the RECOVERY AVAILABLE alarm.
#   B. the tempo is a number you can type, at the segment the playhead is in,
#      and a tempo point can be removed again. The origin is not a point
#      anyone placed, so removing it is refused in the map's own words.
#   C. the metronome is in what you hear and not in a bounce: the export taken
#      with the click running is byte-identical to the one taken before it was
#      turned on, and the export that asks for the click differs.
#   D. a tail is what the file actually carries: the part the compiled render
#      can still sound, plus the silence it cannot, named separately.
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}
export AUDEC_RECOVERY_ROOT=${AUDEC_RECOVERY_ROOT:-$LIVE/recovery}
rm -rf $AUDEC_RECOVERY_ROOT

io() {
  ctl '{"op":"status"}' | python3 -c '
import sys, json
r = json.loads(sys.stdin.readline())["result"]
print("  io:", r.get("io"), " dirty:", r.get("dirty"), " metronome:", r.get("metronome"))
print("  musical_time:", r.get("musical_time"))
print("  notice:", r.get("notice"))
'
}

wait_export() {
  local label=$1 target=$2
  for i in {1..300}; do
    st=$(ctl '{"op":"status"}')
    echo "$st" | grep -qE "EXPORTED (·|\\u00b7) $target" && { echo "  $label exported after ${i}s ($(stat -f%z $target) bytes)"; return 0; }
    echo "$st" | grep -q 'FILE ERROR' && { echo "  $label FAILED: $st"; return 1; }
    sleep 1
  done
  echo "  $label timed out"; return 1
}

echo "########## phase A: a project nobody saved is still autosaved"
launch_audec "$MATERIAL" || exit 1
ctl '{"op":"stop"}' >/dev/null
echo "=== before any edit"
io
echo "=== one undoable edit (tempo +1 BPM) makes the document dirty"
ctl '{"op":"action","id":"audec.tempo.increase"}' >/dev/null
io
echo "=== waiting for the 30 s autosave tick"
for i in {1..60}; do
  ctl '{"op":"status"}' | grep -q 'AUTOSAVED' && break
  sleep 1
done
io
echo "=== what is on disk under the recovery root"
find $AUDEC_RECOVERY_ROOT -name 'project.json' -o -name '*.json' -path '*recovery*' | sed 's/^/  /'

echo
echo "########## phase B: typing a tempo, and removing a tempo point"
echo "=== set the project tempo to 140 by number"
ctl '{"op":"tempo","bpm":140}'
io
echo "=== mark a tempo point at bar 1 of 60 s and set that section to 96"
ctl '{"op":"seek","seconds":60}' >/dev/null
ctl '{"op":"action","id":"audec.tempo.mark_at_playhead"}' >/dev/null
ctl '{"op":"tempo","bpm":96}' >/dev/null
io
echo "=== at 0 s the opening tempo is still 140"
ctl '{"op":"seek","seconds":0}' >/dev/null
io
echo "=== removing the origin is refused in the map's own words"
ctl '{"op":"action","id":"audec.tempo.remove_at_playhead"}' >/dev/null
io
echo "=== back at 60 s, the point is removed and 140 reaches through again"
ctl '{"op":"seek","seconds":60}' >/dev/null
ctl '{"op":"action","id":"audec.tempo.remove_at_playhead"}' >/dev/null
io

echo
echo "########## phase C: the metronome is heard, not bounced"
launch_audec "$MATERIAL" || exit 1
ctl '{"op":"stop"}' >/dev/null
# Every export here is a short range on purpose: the click and the tail are
# about what a file contains, not about how long a bounce takes.
ctl '{"op":"select","start":2646000,"end":2998800}' '{"op":"loop","start":2646000,"end":2998800}' >/dev/null
sleep 3
rm -f $LIVE/quiet.wav $LIVE/click_off.wav $LIVE/click_on.wav $LIVE/tail_loop.wav $LIVE/tail_end.wav
echo "=== the loop, before the click exists"
ctl "{\"op\":\"export\",\"path\":\"$LIVE/quiet.wav\",\"range\":\"loop\"}" >/dev/null
wait_export "before the click" $LIVE/quiet.wav || exit 1
echo "=== turn the metronome on and wait for the render that carries it"
ctl '{"op":"action","id":"audec.transport.metronome"}' >/dev/null
for i in {1..120}; do
  ctl '{"op":"status"}' | grep -q '"metronome": true' && break
  sleep 1
done
io
echo "=== the ordinary export is the project, click or no click"
ctl "{\"op\":\"export\",\"path\":\"$LIVE/click_off.wav\",\"range\":\"loop\"}" >/dev/null
wait_export "click off" $LIVE/click_off.wav
echo "=== and the export that asks for the click carries it"
ctl "{\"op\":\"export\",\"path\":\"$LIVE/click_on.wav\",\"range\":\"loop\",\"metronome\":true}"
wait_export "click on" $LIVE/click_on.wav
cmp -s $LIVE/quiet.wav $LIVE/click_off.wav \
  && echo "  BYTE-IDENTICAL: the bounce with the metronome running is the bounce without it" \
  || echo "  DIFFER: the metronome leaked into an ordinary bounce"
cmp -s $LIVE/quiet.wav $LIVE/click_on.wav \
  && echo "  UNCHANGED: metronome:true wrote no click" \
  || echo "  DIFFERS: metronome:true wrote the click"
echo "=== metronome off again"
ctl '{"op":"action","id":"audec.transport.metronome"}' >/dev/null
for i in {1..120}; do
  ctl '{"op":"status"}' | grep -q '"metronome": false' && break
  sleep 1
done

echo
echo "########## phase D: a tail that says what it is"
echo "=== the loop plus 2 s: the project is still sounding there, so the tail is rendered"
ctl "{\"op\":\"export\",\"path\":\"$LIVE/tail_loop.wav\",\"range\":\"loop\",\"tail_seconds\":2.0}"
wait_export "loop tail" $LIVE/tail_loop.wav
echo "=== the last 3 s of the project plus 2 s: nothing is compiled past the last clip"
ctl "{\"op\":\"export\",\"path\":\"$LIVE/tail_end.wav\",\"range\":[16336404,16468704],\"tail_seconds\":2.0}"
wait_export "project-end tail" $LIVE/tail_end.wav
for f in quiet click_on tail_loop tail_end; do
  [ -f $LIVE/$f.wav ] && printf "  %-12s %s\n" $f "$(sox --i -d $LIVE/$f.wav) ($(sox --i -s $LIVE/$f.wav) samples)"
done
python3 - <<'PY2'
import numpy as np, subprocess, os
S = os.environ["LIVE"]
def load(p):
    raw = subprocess.run(["sox", p, "-t", "raw", "-e", "float", "-b", "32", "-c", "2", "-"],
                         capture_output=True).stdout
    return np.frombuffer(raw, dtype=np.float32).reshape(-1, 2)
def rms(x): return float(np.sqrt(np.mean(x ** 2))) if len(x) else 0.0
sr = 44100
plain, tailed = load(f"{S}/quiet.wav"), load(f"{S}/tail_loop.wav")
print(f"  loop body is the tailed export's head, sample for sample: "
      f"{np.array_equal(plain, tailed[:len(plain)])}")
print(f"  rendered tail RMS {rms(tailed[len(plain):]):.5f} over "
      f"{(len(tailed)-len(plain))/sr:.2f} s  (the project still sounding after the loop)")
end = load(f"{S}/tail_end.wav")
print(f"  project-end tail RMS {rms(end[-2*sr:]):.8f} over 2.00 s  (silence: nothing is compiled there)")
click = load(f"{S}/click_on.wav")
print(f"  click RMS {rms(click):.5f} against the same loop without it {rms(plain):.5f}")
PY2
ctl '{"op":"quit"}' >/dev/null 2>&1
echo "done"
