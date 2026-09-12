#!/bin/zsh
# usage: playback_before_completion.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR, AUDEC_CACHE_ROOT)
#
# An edit used to be inaudible until its last tile existed: readiness was
# all-or-nothing, so a tile cohort could not play until every one of its tiles
# was rendered. Now the running render offers what it has, the renderer plays
# each slot from the newest cohort that covers it and the previous cohort under
# the rest, and `status.readiness` says which is which.
#
# What this scenario shows, while transport rolls:
#   1. before any edit: one complete cohort, `readiness.missing == 0`;
#   2. make beat inside the loop, then sample `status` every 200 ms;
#   3. at least one sample has `playing: true` with `readiness.missing > 0`,
#      and a seek away from the loop while it is priming is served from the
#      previous cohort (`readiness.from_previous_frames > 0`) without a single
#      starved frame -- the tiles under the playhead are rendered first, so the
#      fallback is what covers the rest of the timeline meanwhile;
#   4. it converges: the last sample is complete again;
#   5. `status.memory` reports the resident render-product bytes against the
#      budget that bounds them, and the retired revision as receipts.
#
# The tile CAS is the musician's own store unless one is named: set
# AUDEC_PRIVATE_HOME to a scratch directory and this scenario runs against a
# cache of its own, which is what you want while the cache format is moving.
if [ -n "$AUDEC_PRIVATE_HOME" ]; then
  mkdir -p "$AUDEC_PRIVATE_HOME"
  export HOME="$AUDEC_PRIVATE_HOME"
fi
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}
LOOP_START=${2:-2646000}   # 60 s at 44.1 kHz
LOOP_END=${3:-2998800}     # 68 s

launch_audec "$MATERIAL" || exit 1

# "Ready" is the analysis; the first bounce is still arriving. An edit made
# before any cohort exists has nothing to play under it, so wait for one.
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
wait_for_cohort || exit 1

field() {
  ctl '{"op":"status"}' | python3 -c "
import sys, json
r = json.loads(sys.stdin.readline())['result']
print(json.dumps(r.get('$1')))
"
}

echo "=== before the edit: one complete cohort"
ctl "{\"op\":\"select\",\"start\":$LOOP_START,\"end\":$LOOP_END}" \
    "{\"op\":\"loop\",\"start\":$LOOP_START,\"end\":$LOOP_END}" >/dev/null
sleep 2
ctl '{"op":"play"}' >/dev/null
sleep 2
echo "  playing:   $(field playing)"
echo "  readiness: $(field readiness)"
echo "  memory:    $(field memory)"

echo "=== make beat inside the loop, sampling readiness while it renders"
ctl '{"op":"action","id":"audec.sample.make_beat"}' >/dev/null
python3 - <<'PY'
import json, os, socket, sys, time
path = os.environ['AUDEC_CONTROL_SOCKET']
def status():
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(30); s.connect(path)
    f = s.makefile('rw')
    f.write('{"op":"status"}\n'); f.flush()
    reply = json.loads(f.readline())['result']
    s.close()
    return reply
def send(request):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(30); s.connect(path)
    f = s.makefile('rw')
    f.write(json.dumps(request) + '\n'); f.flush()
    reply = json.loads(f.readline())
    s.close()
    return reply
samples = []
seeked = False
deadline = time.time() + 120
while time.time() < deadline:
    r = status()
    ready = r.get('readiness') or {}
    samples.append((r.get('playhead_sample'), bool(r.get('playing')), ready.get('required'),
                    ready.get('covered'), ready.get('missing'),
                    ready.get('from_previous_frames'), ready.get('starved_frames')))
    # Tiles are rendered outward from the playhead the edit was made at, so
    # the loop is the new revision within one pass. Move to the far end of the
    # timeline, which the scheduler reaches last: that is where the previous
    # cohort is doing the covering, and it must be audio, not silence.
    if not seeked and (ready.get('missing') or 0) > 0 and (ready.get('covered') or 0) > 0:
        total = r.get('total_samples') or 0
        send({"op": "seek", "sample": max(0, total - 441000)})
        send({"op": "play"})
        seeked = True
    if len(samples) > 6 and samples[-1][4] == 0 and any(s[4] for s in samples):
        break
    time.sleep(0.2)
print('  playhead playing required covered missing from_previous starved')
for t, playing, req, cov, miss, prev, starved in samples[:40]:
    print(f'  {t:<7} {playing!s:<7} {req!s:<8} {cov!s:<7} {miss!s:<7} {prev!s:<13} {starved}')
priming = [s for s in samples if s[1] and (s[4] or 0) > 0]
heard = [s for s in samples if (s[5] or 0) > 0]
print(f'  samples: {len(samples)}')
print(f'  playing with missing > 0:        {len(priming)}')
print(f'  frames served from the previous: {max((s[5] or 0) for s in samples) if samples else 0}')
print(f'  starved frames (must stay 0):    {max((s[6] or 0) for s in samples) if samples else 0}')
print(f'  final readiness: required={samples[-1][2]} covered={samples[-1][3]} missing={samples[-1][4]}')
ok = bool(priming) and bool(heard) and samples[-1][4] == 0 and all((s[6] or 0) == 0 for s in samples)
print('  VERDICT:', 'playback before completion, audible and honest' if ok else 'NOT SHOWN')
sys.exit(0 if ok else 1)
PY
verdict=$?

echo "=== after: readiness and memory"
echo "  readiness: $(field readiness)"
echo "  memory:    $(field memory)"
echo "  notice:    $(field notice)"
echo "  audio_error: $(field audio_error)"

ctl '{"op":"quit"}' >/dev/null 2>&1 || kill $(cat $LIVE/audec.pid) 2>/dev/null
exit $verdict
