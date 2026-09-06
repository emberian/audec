#!/bin/zsh
# usage: inserts.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR)
#
# The strip's inserts stopped saying "not rendered". On the desktop:
#   1. export the master with no insert;
#   2. audec.mixer.insert_filter adds one native Filter to the master strip
#      ("+ insert" from outside the pane, same action the picker sends);
#   3. status reports the insert as active, not "not rendered";
#   4. export the master again and compare: sox stat and a spectral centroid
#      from the two files show a low-pass, not a mute and not a no-op.
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
  for i in {1..300}; do
    st=$(ctl '{"op":"status"}')
    echo "$st" | grep -q "EXPORTED · $target" && { echo "  $label exported after ${i}s ($(stat -f%z $target 2>/dev/null) bytes)"; return 0; }
    echo "$st" | grep -q 'FILE ERROR' && { echo "  $label FAILED: $st"; return 1; }
    sleep 1
  done
  echo "  $label timed out"; return 1
}

launch_audec "$MATERIAL" || exit 1
rm -f $LIVE/dry.wav $LIVE/wet.wav

echo "=== the action is registered and offered"
ctl '{"op":"actions"}' | python3 -c '
import sys, json
result = json.loads(sys.stdin.readline())["result"]
rows = result["actions"] if isinstance(result, dict) else result
for row in rows:
    if row["id"] == "audec.mixer.insert_filter":
        print("  ", row)
        break
else:
    print("   audec.mixer.insert_filter is NOT in the catalog")
'

echo "=== export the master with no insert"
ctl "{\"op\":\"export\",\"path\":\"$LIVE/dry.wav\"}" >/dev/null
wait_export dry $LIVE/dry.wav || exit 1
notice

echo "=== audec.mixer.insert_filter"
ctl '{"op":"action","id":"audec.mixer.insert_filter"}'
notice

echo "=== export the master again"
ctl "{\"op\":\"export\",\"path\":\"$LIVE/wet.wav\"}" >/dev/null
wait_export wet $LIVE/wet.wav || exit 1

echo "=== sox stat, dry then wet"
sox $LIVE/dry.wav -n stat 2>&1 | sed 's/^/  dry  /'
sox $LIVE/wet.wav -n stat 2>&1 | sed 's/^/  wet  /'

echo "=== spectral centroid of each export"
python3 - <<'PY'
import numpy as np, subprocess, os
S = os.environ["LIVE"]
def load(p):
    raw = subprocess.run(["sox", p, "-t", "raw", "-e", "float", "-b", "32", "-c", "2", "-"],
                         capture_output=True).stdout
    return np.frombuffer(raw, dtype=np.float32).reshape(-1, 2)[:, 0]
def centroid(x, sr=44100, n=4096):
    frames = len(x) // n
    x = x[: frames * n].reshape(frames, n) * np.hanning(n)
    mag = np.abs(np.fft.rfft(x, axis=1))
    freq = np.fft.rfftfreq(n, 1 / sr)
    total = mag.sum()
    return float((mag * freq).sum() / total) if total else 0.0
def rms(x): return float(np.sqrt(np.mean(x ** 2)))
dry, wet = load(f"{S}/dry.wav"), load(f"{S}/wet.wav")
n = min(len(dry), len(wet))
dry, wet = dry[:n], wet[:n]
print(f"  dry  centroid {centroid(dry):9.1f} Hz   rms {rms(dry):.5f}")
print(f"  wet  centroid {centroid(wet):9.1f} Hz   rms {rms(wet):.5f}")
print(f"  centroid ratio wet/dry {centroid(wet) / max(centroid(dry), 1e-9):.4f}"
      f"   differing samples {int((dry != wet).sum())} of {n}")
PY

ctl '{"op":"quit"}' >/dev/null 2>&1
echo "done"
