#!/bin/zsh
# usage: make_beat_audible.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR)
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}
launch_audec "$MATERIAL"
# Live desktop verification: open material, select+loop 60-68s, export rev N, make beat, export rev N+1, compare.
rm -f $SOCK $LIVE/master_a.wav $LIVE/master_b.wav
echo "launched $!"
# wait for ready
for i in {1..120}; do st=$(ctl '{"op":"status"}'); echo "$st" | grep -q '"state": "ready"' && break; sleep 1; done
echo "ready after ${i}s"
ctl '{"op":"select","start":2646000,"end":2998800}' '{"op":"loop","start":2646000,"end":2998800}' >/dev/null
# wait for audio compiled (audio_error null and not rendering): just poll status a few seconds
sleep 3
ctl "{\"op\":\"export\",\"path\":\"$LIVE/master_a.wav\"}" >/dev/null
for i in {1..180}; do st=$(ctl '{"op":"status"}'); echo "$st" | grep -q 'EXPORTED' && break; echo "$st" | grep -q 'FILE ERROR' && { echo "export A failed: $st"; break; }; sleep 1; done
echo "export A: ${i}s"; ls -la $LIVE/master_a.wav 2>/dev/null | awk '{print $5}'
ctl '{"op":"action","id":"audec.sample.make_beat"}'
sleep 2
ctl '{"op":"status"}' | python3 -c 'import sys,json; r=json.loads(sys.stdin.readline())["result"]; print({k:r.get(k) for k in ("revision","audio_error","notice","io")})'
ctl "{\"op\":\"export\",\"path\":\"$LIVE/master_b.wav\"}" >/dev/null
for i in {1..300}; do st=$(ctl '{"op":"status"}'); echo "$st" | grep -q 'EXPORTED' && break; echo "$st" | grep -q 'FILE ERROR' && { echo "export B failed: $st"; break; }; sleep 1; done
echo "export B: ${i}s"; ls -la $LIVE/master_b.wav 2>/dev/null | awk '{print $5}'
ctl '{"op":"status"}' | python3 -c 'import sys,json; r=json.loads(sys.stdin.readline())["result"]; print({k:r.get(k) for k in ("revision","audio_error","io","audio_device")})'
echo "=== compare (sox) ==="
sox --i $LIVE/master_a.wav | grep -E 'Sample Rate|Channels|Duration|Precision' | tr '\n' ' '; echo
sox --i $LIVE/master_b.wav | grep -E 'Duration' 
python3 - <<'PY'
import numpy as np, subprocess, sys, os
S=os.environ["LIVE"]
def load(p):
    raw=subprocess.run(["sox",p,"-t","raw","-e","float","-b","32","-c","2","-"],capture_output=True).stdout
    return np.frombuffer(raw,dtype=np.float32).reshape(-1,2)
a=load(f"{S}/master_a.wav"); b=load(f"{S}/master_b.wav")
n=min(len(a),len(b)); a=a[:n]; b=b[:n]; d=b-a
sr=44100
def rms(x): return float(np.sqrt(np.mean(x**2))) if len(x) else 0.0
print(f"frames {n}  rmsA {rms(a):.4f} rmsB {rms(b):.4f} rms(B-A) {rms(d):.4f}")
# where does B differ from A? report 2-second windows with the largest difference
win=2*sr
scores=[(i/sr, rms(d[i:i+win])) for i in range(0,n-win,win)]
scores.sort(key=lambda t:-t[1])
print("top diff windows (start_s, rms):", [(round(t,1), round(r,4)) for t,r in scores[:6]])
print("diff rms in loop 60-68s:", round(rms(d[60*sr:68*sr]),4), " outside 100-110s:", round(rms(d[100*sr:110*sr]),4))
PY
