#!/bin/zsh
# usage: drops.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR)
#
# The two drop targets the drag contract promises, on the desktop:
#   1. a beat is made in 60-68 s, so the project holds a master, the material's
#      channel and the beat's channel, and every one of them is exported;
#   2. audec.mixer.route_selected sends the selected channel (with none
#      selected, the first that is not the master) to the next destination the
#      graph accepts. It is the same decision a strip-on-strip drop makes -
#      `control_actions::route_bus_drop` - asked for by name, because the
#      socket cannot hold a mouse;
#   3. the buses are exported again: the destination channel now carries what
#      only the source carried, which is what "the audio moves" means, and the
#      master is unchanged because the sum reaching it is the same;
#   4. the pattern editor opens and its LIBRARY rail (the pattern-library drop
#      target, and the first drag source for a pattern) renders on the real
#      binary.
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}

# The io status keeps the last EXPORTED path, so a wait must name the path it
# is waiting for or it will accept the previous export.
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

notice() {
  ctl '{"op":"status"}' | python3 -c '
import sys, json
r = json.loads(sys.stdin.readline())["result"]
print("  revision", r.get("revision"), " audio_error", r.get("audio_error"))
print("  notice:", r.get("notice"))
'
}

export_buses() {
  local tag=$1
  # (N) is zsh's null-glob qualifier: a first run has nothing to remove.
  rm -f $LIVE/${tag}_master.wav $LIVE/${tag}_bus_*.wav(N)
  ctl "{\"op\":\"export\",\"path\":\"$LIVE/${tag}_master.wav\"}" >/dev/null
  wait_export "$tag master" $LIVE/${tag}_master.wav
  for bus in 1 2 3 4; do
    reply=$(ctl "{\"op\":\"export\",\"path\":\"$LIVE/${tag}_bus_$bus.wav\",\"scope\":\"bus:$bus\"}")
    if echo "$reply" | grep -q '"error"'; then
      echo "  bus:$bus is not an exportable scope: $reply"
      continue
    fi
    wait_export "$tag bus:$bus" $LIVE/${tag}_bus_$bus.wav
  done
}

echo "########## phase A: a beat, and what each channel carries before the route"
launch_audec "$MATERIAL" || exit 1
ctl '{"op":"select","start":2646000,"end":2998800}' '{"op":"loop","start":2646000,"end":2998800}' >/dev/null
sleep 3
ctl '{"op":"action","id":"audec.sample.make_beat"}' >/dev/null
sleep 2
notice
export_buses before

echo
echo "########## phase B: audec.mixer.route_selected (the strip drop, by name)"
ctl '{"op":"action","id":"audec.mixer.route_selected"}' >/dev/null
sleep 2
notice
export_buses after

echo
echo "########## what moved"
python3 - <<'PY'
import numpy as np, subprocess, os, glob
S = os.environ["LIVE"]
sr = 44100
def load(p):
    raw = subprocess.run(["sox", p, "-t", "raw", "-e", "float", "-b", "32", "-c", "2", "-"],
                         capture_output=True).stdout
    return np.frombuffer(raw, dtype=np.float32).reshape(-1, 2)
def rms(x): return float(np.sqrt(np.mean(x ** 2))) if len(x) else 0.0
names = sorted({os.path.basename(p)[len("before_"):] for p in glob.glob(f"{S}/before_*.wav")})
print(f"{'scope':12s} {'whole before':>13s} {'whole after':>12s} {'60-68s before':>14s} {'60-68s after':>13s} {'100-110s before':>16s} {'100-110s after':>15s}")
for name in names:
    b, a = f"{S}/before_{name}", f"{S}/after_{name}"
    if not (os.path.exists(b) and os.path.exists(a)):
        continue
    x, y = load(b), load(a)
    print(f"{name[:-4]:12s} {rms(x):13.5f} {rms(y):12.5f}"
          f" {rms(x[60*sr:68*sr]):14.5f} {rms(y[60*sr:68*sr]):13.5f}"
          f" {rms(x[100*sr:110*sr]):16.5f} {rms(y[100*sr:110*sr]):15.5f}")
PY

echo
echo "########## the same gate again: the OUTPUT rule walks on to the next destination"
ctl '{"op":"action","id":"audec.mixer.route_selected"}' >/dev/null
sleep 2
notice

echo
echo "########## phase C: the pattern library rail on the real binary"
ctl '{"op":"action","id":"audec.editor.drums"}' >/dev/null
sleep 2
ctl '{"op":"status"}' | python3 -c '
import sys, json
r = json.loads(sys.stdin.readline())["result"]
print("  active_view", r.get("active_view"), " notice:", r.get("notice"))
'
ctl '{"op":"objects"}' | python3 ${0:A:h}/tree.py 2>/dev/null | grep -iE "pattern|kit" | head -10
tail -3 $LIVE/app.log
ctl '{"op":"quit"}' >/dev/null 2>&1
echo "done"
