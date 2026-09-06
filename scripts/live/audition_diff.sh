#!/bin/zsh
# usage: audition_diff.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR)
# Hear the edit: `audec.transport.audition_diff` plays the active render minus
# the one it retired, over the loop. This scenario makes exactly one edit
# (make beat inside the loop), exports the master on both sides of it, and
# checks the app's own `status.diff` numbers against what sox measures between
# the two exports. A second press must stop the null.
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}
launch_audec "$MATERIAL" || exit 1
LOOP_START=2646000   # 60 s at 44.1 kHz
LOOP_END=2998800     # 68 s
diff_json() {
  ctl '{"op":"status"}' | python3 -c 'import sys,json; print(json.dumps(json.loads(sys.stdin.readline())["result"].get("diff")))'
}
notice() {
  ctl '{"op":"status"}' | python3 -c 'import sys,json; print(json.loads(sys.stdin.readline())["result"].get("notice"))'
}

echo "=== before any edit: one cohort, nothing to subtract ==="
ctl "{\"op\":\"select\",\"start\":$LOOP_START,\"end\":$LOOP_END}" "{\"op\":\"loop\",\"start\":$LOOP_START,\"end\":$LOOP_END}" >/dev/null
sleep 3
echo "diff: $(diff_json)"
ctl '{"op":"action","id":"audec.transport.audition_diff"}' >/dev/null
echo "refusal: $(notice)"

echo "=== export the master before the edit ==="
ctl "{\"op\":\"export\",\"path\":\"$LIVE/null_a.wav\"}" >/dev/null
for i in {1..180}; do st=$(ctl '{"op":"status"}'); echo "$st" | grep -q 'EXPORTED' && break; echo "$st" | grep -q 'FILE ERROR' && { echo "export A failed: $st"; break; }; sleep 1; done
echo "export A: ${i}s  $(ls -la $LIVE/null_a.wav 2>/dev/null | awk '{print $5}') bytes"

echo "=== one edit: make beat inside the loop ==="
ctl '{"op":"action","id":"audec.sample.make_beat"}' >/dev/null
sleep 4
ctl "{\"op\":\"export\",\"path\":\"$LIVE/null_b.wav\"}" >/dev/null
for i in {1..300}; do st=$(ctl '{"op":"status"}'); echo "$st" | grep -q 'EXPORTED' && break; echo "$st" | grep -q 'FILE ERROR' && { echo "export B failed: $st"; break; }; sleep 1; done
echo "export B: ${i}s  $(ls -la $LIVE/null_b.wav 2>/dev/null | awk '{print $5}') bytes"
echo "diff before pressing: $(diff_json)"

echo "=== press audec.transport.audition_diff ==="
ctl '{"op":"action","id":"audec.transport.audition_diff"}' >/dev/null
echo "notice: $(notice)"
echo "diff after press 1: $(diff_json)"

echo "=== press it again ==="
ctl '{"op":"action","id":"audec.transport.audition_diff"}' >/dev/null
echo "notice: $(notice)"
echo "diff after press 2: $(diff_json)"

echo "=== the same null, measured from the two exports (sox + numpy) ==="
# These two measurements are of the same edit but not of the same numbers: the
# app subtracts the render graph's f32 masters, the exports are 24-bit integer
# PCM and clamp at +/-1.0. Where the post-edit master clips, the exported
# difference is smaller than the real one, so the clipped-sample counts below
# are part of reading the comparison, not a footnote.
python3 - <<'PY'
import numpy as np, subprocess, os
S=os.environ["LIVE"]
def load(p):
    raw=subprocess.run(["sox",p,"-t","raw","-e","float","-b","32","-c","2","-"],capture_output=True).stdout
    return np.frombuffer(raw,dtype=np.float32).reshape(-1,2)
a=load(f"{S}/null_a.wav"); b=load(f"{S}/null_b.wav")
n=min(len(a),len(b)); a=a[:n]; b=b[:n]; d=b-a
sr=44100; lo, hi = 60*sr, 68*sr
def rms(x): return float(np.sqrt(np.mean(x**2))) if len(x) else 0.0
outside=np.concatenate([d[:lo], d[hi:]])
print(f"frames {n}")
print(f"sox rms(B-A) in loop 60-68s: {rms(d[lo:hi]):.6f}")
print(f"sox rms(B-A) outside loop:   {rms(outside):.6f}")
print(f"export peaks: A {np.abs(a).max():.6f}  B {np.abs(b).max():.6f}")
print(f"clamped samples in B: {int((np.abs(b)>=0.999).sum())} total, "
      f"{int((np.abs(b[lo:hi])>=0.999).sum())} inside the loop")
# Where the null actually is, one second at a time. A non-zero
# rms_outside_loop should be readable as real material, not as drift.
win = sr
rows = [(i / sr, rms(d[i:i + win])) for i in range(0, len(d) - win, win)]
rows.sort(key=lambda row: -row[1])
print("loudest seconds of the null:",
      [(round(t, 1), round(r, 4)) for t, r in rows[:5]])
print("seconds outside the loop with any null at all:",
      sum(1 for t, r in rows if not (60 <= t < 68) and r > 0.0),
      "of", sum(1 for t, _ in rows if not (60 <= t < 68)))
PY
ctl '{"op":"quit"}' >/dev/null 2>&1 || kill $(cat $LIVE/audec.pid) 2>/dev/null
