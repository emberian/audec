#!/bin/zsh
# usage: clip_edits.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR, AUDEC_CACHE_ROOT)
#
# The clip edits a musician reaches without a mouse, on the desktop, proved in
# the exported audio:
#   0. a split at 60 s makes two clips and moves no sample (byte-identical),
#      and undo puts the one clip back;
#   1. the material clip is selected and turned down 6 dB, one press per dB -
#      the export is half the amplitude everywhere, which is what -6 dB means;
#   2. a fade-in is grown from the clip's own edge - the export's first bar
#      ramps out of silence while the rest is unchanged;
#   3. the refusals are read back verbatim from `status.arrangement.status`:
#      a crossfade with one clip selected, a repeat on an audio occurrence;
#   4. a marker is put at the playhead and the next export is BYTE-IDENTICAL:
#      a marker reaches no renderer, so it invalidates nothing;
#   5. the selected asset is placed at the playhead without a drag, and the
#      export gains a second voice where it landed;
#   6. make beat still lands its pattern.
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}

wait_export() {
  local label=$1 target=$2
  for i in {1..300}; do
    st=$(ctl '{"op":"status"}')
    echo "$st" | grep -qE "EXPORTED (·|\\u00b7) $target" && { echo "  $label exported after ${i}s ($(stat -f%z $target 2>/dev/null) bytes)"; return 0; }
    echo "$st" | grep -q 'FILE ERROR' && { echo "  $label FAILED: $st"; return 1; }
    sleep 1
  done
  echo "  $label timed out"; return 1
}

export_master() {
  local tag=$1
  rm -f $LIVE/$tag.wav
  ctl "{\"op\":\"export\",\"path\":\"$LIVE/$tag.wav\"}" >/dev/null
  wait_export "$tag" $LIVE/$tag.wav
}

# An edit that arrives while a render is in flight cancels it and the render
# never restarts: every later export then waits forever on a cohort that will
# not complete (reproduced with `audec.clip.split` alone, so it predates these
# verbs - see the lane report). Every edit below therefore waits for the render
# to cover the whole project first.
settle() {
  local label=$1
  for i in {1..240}; do
    ctl '{"op":"status"}' | python3 -c '
import sys, json
r = json.loads(sys.stdin.readline())["result"]
d = r["readiness"]
err = r.get("audio_error")
if err:
    print("error", err); raise SystemExit(2)
print("done" if d["required"] and d["covered"] >= d["required"] and not d["priming"] else "waiting",
      d["covered"], "/", d["required"])
' > /tmp/settle.$$ 2>/dev/null
    read -r state rest < /tmp/settle.$$
    case $state in
      done) echo "  $label settled after ${i}s ($rest)"; rm -f /tmp/settle.$$; return 0 ;;
      error) echo "  $label FAILED: $(cat /tmp/settle.$$)"; rm -f /tmp/settle.$$; return 1 ;;
    esac
    sleep 1
  done
  echo "  $label did not settle"; rm -f /tmp/settle.$$; return 1
}

arr() {
  ctl '{"op":"status"}' | python3 -c '
import sys, json
r = json.loads(sys.stdin.readline())["result"]
a = r.get("arrangement")
if a is None:
    print("  arrangement: no pane open"); raise SystemExit
print("  revision", r.get("revision"), " clips", a["selected_clips"], " markers", a["markers"])
print("  pane says:", a["status"])
'
}

echo "########## phase A: open the arrangement and take the material clip down 6 dB"
# A debug binary on a box shared with other lanes can need longer than the
# launcher's 60 s to open its socket. Three attempts, then give up loudly.
launched=0
for attempt in 1 2 3; do
  launch_audec "$MATERIAL" && { launched=1; break; }
  echo "  launch attempt $attempt did not come up; retrying"
done
[ $launched = 1 ] || exit 1
settle "the project as opened" || exit 1
ctl '{"op":"action","id":"audec.editor.arrangement"}' >/dev/null; sleep 1
ctl '{"op":"action","id":"audec.clip.select_all"}' >/dev/null; sleep 1
arr
export_master master_a || exit 1

echo "-- split at 60 s: two clips, and the same samples"
ctl '{"op":"seek","seconds":60}' >/dev/null; sleep 0.5
ctl '{"op":"action","id":"audec.clip.split"}' >/dev/null; sleep 1
settle "the split" || exit 1
arr
export_master master_split || exit 1
if cmp -s $LIVE/master_a.wav $LIVE/master_split.wav; then
  echo "  BYTE-IDENTICAL to the unsplit export: a cut at a sample boundary moves no sample"
else
  echo "  DIFFERS from the unsplit export"
fi
ctl '{"op":"action","id":"audec.edit.undo"}' >/dev/null; sleep 1
settle "the undo" || exit 1
arr

echo "-- six presses of -1 dB"
ctl '{"op":"action","id":"audec.clip.select_all"}' >/dev/null; sleep 1
for i in 1 2 3 4 5 6; do
  ctl '{"op":"action","id":"audec.clip.gain_down"}' >/dev/null
  sleep 1
  settle "gain step $i" || exit 1
done
arr
export_master master_gain || exit 1

echo "########## phase B: a fade-in grown from the clip's own edge"
for i in 1 2 3 4; do
  ctl '{"op":"action","id":"audec.clip.fade_in"}' >/dev/null
  sleep 1
  settle "fade step $i" || exit 1
done
arr
export_master master_fade || exit 1

echo "########## phase C: the refusals, verbatim"
echo "-- crossfade with one clip selected"
ctl '{"op":"action","id":"audec.clip.crossfade"}' >/dev/null; sleep 0.6; arr
echo "-- repeat on an audio occurrence"
ctl '{"op":"action","id":"audec.clip.repeat"}' >/dev/null; sleep 0.6; arr

echo "########## phase D: a marker changes no audio"
ctl '{"op":"seek","seconds":60}' >/dev/null; sleep 0.5
ctl '{"op":"action","id":"audec.marker.put_at_playhead"}' >/dev/null; sleep 1
settle "the marker" || exit 1
arr
export_master master_marker || exit 1
if cmp -s $LIVE/master_fade.wav $LIVE/master_marker.wav; then
  echo "  BYTE-IDENTICAL to the pre-marker export ($(stat -f%z $LIVE/master_marker.wav) bytes)"
else
  echo "  DIFFERS from the pre-marker export - a marker reached the renderer"
fi

echo "########## phase E: place the selected asset at the playhead, no mouse"
ctl '{"op":"action","id":"audec.clip.place_selected_asset_at_playhead"}' >/dev/null; sleep 2
settle "the placement" || exit 1
arr
export_master master_place || exit 1

echo "########## phase F: make beat still lands its pattern"
ctl '{"op":"select","start":2646000,"end":2998800}' '{"op":"loop","start":2646000,"end":2998800}' >/dev/null; sleep 1
ctl '{"op":"action","id":"audec.sample.make_beat"}' >/dev/null; sleep 3
settle "the beat" || exit 1
arr
export_master master_beat || exit 1

echo "########## sox + numpy: what each export carries"
python3 - <<'PY'
import numpy as np, subprocess, os
S = os.environ["LIVE"]
SR = 44100
def load(name):
    p = f"{S}/{name}.wav"
    if not os.path.exists(p):
        return None
    raw = subprocess.run(["sox", p, "-t", "raw", "-e", "float", "-b", "32", "-c", "2", "-"],
                         capture_output=True).stdout
    return np.frombuffer(raw, dtype=np.float32).reshape(-1, 2)
def rms(x):
    return float(np.sqrt(np.mean(x ** 2))) if len(x) else 0.0
def db(x, y):
    return 20 * np.log10(y / x) if x > 0 and y > 0 else float("nan")

a, g, f, m, p, b = (load(n) for n in
                    ("master_a", "master_gain", "master_fade", "master_marker",
                     "master_place", "master_beat"))
if a is None or g is None:
    raise SystemExit("missing exports")
n = min(len(a), len(g))
print(f"frames {n}")
print(f"  A (as opened)      rms {rms(a):.5f}")
print(f"  gain -6 dB         rms {rms(g):.5f}   measured {db(rms(a), rms(g)):+.2f} dB   (asked -6.00)")
if f is not None:
    # Four grid cells of fade at 125 BPM is about 1.9 s. Each window is the
    # faded export against the same window of the export before the fade, so
    # the ramp is read as a ratio and the material's own shape cancels.
    for lo, hi in ((0.0, 0.5), (0.5, 1.0), (1.0, 2.0), (2.0, 3.0), (30.0, 35.0)):
        a0, b0 = int(lo * SR), int(hi * SR)
        print(f"  fade {lo:>4.1f}-{hi:<4.1f} s   rms {rms(f[a0:b0]):.5f} vs {rms(g[a0:b0]):.5f} "
              f"({db(rms(g[a0:b0]), rms(f[a0:b0])):+.2f} dB)")
if p is not None and f is not None:
    k = min(len(p), len(f))
    d = p[:k] - f[:k]
    print(f"  placement          rms(place - fade) {rms(d):.5f}; "
          f"at 60-70 s {rms(d[60*SR:70*SR]):.5f}, at 10-20 s {rms(d[10*SR:20*SR]):.5f}")
if b is not None and p is not None:
    k = min(len(b), len(p))
    d = b[:k] - p[:k]
    print(f"  beat               rms(beat - place) in the loop 60-68 s {rms(d[60*SR:68*SR]):.5f}, "
          f"outside 100-110 s {rms(d[100*SR:110*SR]):.5f}")
PY
stop_audec
