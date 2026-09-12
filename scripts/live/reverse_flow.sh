#!/bin/zsh
# usage: reverse_flow.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR, AUDEC_CACHE_ROOT)
#
# The reverse flow with no pane in the way. A lens is named, not addressed by a
# number; it publishes findings; one of them is kept and one is compared
# through the same event the reverse pane's RESULT ACTIONS emit; and the
# Explorer's Compare branch is read back. Every refusal is printed in the app's
# own words, because a refusal a script cannot see is a refusal a musician
# cannot trust.
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}
HERE=${0:A:h}
launch_audec "$MATERIAL" || exit 1

act() { ctl "{\"op\":\"action\",\"id\":\"$1\"}"; }
lens_rows() { ctl '{"op":"status"}' | python3 -c '
import sys, json
for lens in json.loads(sys.stdin.readline())["result"]["lenses"]:
    span = lens["span"]
    print("   view %-3s %-11s %-9s %8.2f-%-8.2f s [%s]  findings=%s%s" % (
        lens["view"], lens["kind"], lens["state"],
        span["start_seconds"], span["end_seconds"], span["basis"],
        lens["findings"],
        "  failure=" + lens["failure"] if lens.get("failure") else ""))
'; }
finding_rows() { ctl '{"op":"status"}' | python3 -c '
import sys, json
rows = json.loads(sys.stdin.readline())["result"]["findings"]
if not rows:
    print("   (no findings published)")
for row in rows:
    span = row["span"]
    print("   [%s] %s  %s  %.2f-%.2f s" % (row["index"], row["lens"], row["title"],
                                           span["start_seconds"], span["end_seconds"]))
    print("        address %s" % row["address"])
    for verb, state in row["actions"].items():
        detail = state.get("reason") or state.get("primary") or ""
        print("        %-8s %-10s %s" % (verb, state["state"], detail))
    for audition in row["auditions"]:
        print("        hear     %-20s %s" % (
            audition["kind"], audition["refusal"] or "available"))
'; }
reply() { ctl "$1" | python3 -c '
import sys, json
d = json.loads(sys.stdin.readline())
if not d.get("ok"):
    print("   REFUSED: %s" % d["error"]); raise SystemExit(0)
r = d["result"]
if "did" in r:
    print("   did %s on [%s] %s" % (r["did"], r["index"], r["address"]))
    print("   notice: %s" % r.get("notice"))
    if r.get("finding"):
        for verb, state in r["finding"]["actions"].items():
            print("        %-8s %-10s %s" % (verb, state["state"],
                                             state.get("reason") or state.get("primary") or ""))
else:
    print("   %s" % json.dumps(r))
'; }
lens_view() { ctl '{"op":"status"}' | python3 -c "
import sys, json
for lens in json.loads(sys.stdin.readline())['result']['lenses']:
    if lens['kind'] == '$1':
        print(lens['view']); break
"; }
# The index of the first finding a given lens published, or nothing.
finding_of_lens() { ctl '{"op":"status"}' | python3 -c "
import sys, json
for row in json.loads(sys.stdin.readline())['result']['findings']:
    if row['lens'] == '$1':
        print(row['index']); break
"; }
# Rhythm deprojection reads the whole song: about 8 minutes for a 6-minute
# track on a debug build, seconds on a short excerpt. REVERSE_FLOW_WAIT is the
# ceiling in polls (about a second each plus the round trip).
WAIT_POLLS=${REVERSE_FLOW_WAIT:-700}
wait_for_lens_findings() {
  for i in $(seq 1 $WAIT_POLLS); do
    local at=$(finding_of_lens $1)
    [ -n "$at" ] && { echo "   $1 published its first finding at index $at after about ${i}s"; return 0; }
    sleep 1
  done
  echo "   $1 published no finding within $WAIT_POLLS polls"; return 1
}
# The first finding whose named verb the lifecycle says is available. The
# reverse flow is about what a finding admits, not about which lens finished
# first, so the scenario asks the list instead of assuming an index.
finding_admitting() { ctl '{"op":"status"}' | python3 -c "
import sys, json
for row in json.loads(sys.stdin.readline())['result']['findings']:
    if row['actions']['$1']['state'] == 'available':
        print(row['index']); break
"; }
active_view() {
  ctl '{"op":"status"}' | python3 -c 'import sys,json; print("   active_view:", json.loads(sys.stdin.readline())["result"]["active_view"])'
}

echo "0. every lens says what it is doing, over which window, with how many findings"
lens_rows

echo "1. name the lens: audec.lens.rhythm (no numeric view, no second pane)"
act audec.lens.rhythm
sleep 2
lens_rows

echo "2. wait for the rhythm lens to publish"
wait_for_lens_findings Rhythm
lens_rows
active_view

echo "3. status.findings: what the analysis half published, and what each verb admits"
finding_rows

# Whichever finding admits Compare is the one the reverse flow is about.
RH=$(finding_admitting compare)
if [ -z "$RH" ]; then
  RH=$(finding_admitting keep)
  echo "   no published finding admits Compare; using [$RH] and printing its refusal"
fi

echo "3b. open finding $RH: the reverse surface, reached by verb"
reply "{\"op\":\"finding\",\"index\":$RH,\"do\":\"open\"}"
sleep 2
ctl '{"op":"status"}' | python3 -c 'import sys,json; r=json.loads(sys.stdin.readline())["result"]; print("   windows:", r["windows"], " notice:", r["notice"])'

echo "4. keep finding $RH — pane-less, through the reverse pane's own event"
reply "{\"op\":\"finding\",\"index\":$RH,\"do\":\"keep\"}"

echo "5. compare it — the Explorer branch that was empty live until now"
reply "{\"op\":\"finding\",\"index\":$RH,\"do\":\"compare\"}"
sleep 2

echo "6. the Explorer's Investigate tree"
ctl '{"op":"objects"}' | python3 $HERE/tree.py Investigate

echo "7. refusals, verbatim"
echo "7a. an index the list does not have"
reply '{"op":"finding","index":99,"do":"keep"}'
echo "7b. an address the list does not have"
reply '{"op":"finding","address":"finding:not-a-finding","do":"keep"}'
echo "7c. keeping the same finding twice"
reply "{\"op\":\"finding\",\"index\":$RH,\"do\":\"keep\"}"
echo "7d. a verb this kind of evidence does not admit"
reply "{\"op\":\"finding\",\"index\":$RH,\"do\":\"sample\"}"
echo "7e. an audition this finding does not offer"
reply "{\"op\":\"finding\",\"index\":$RH,\"do\":\"audition:Trumpet\"}"
echo "7f. a verb the protocol does not have"
reply '{"op":"finding","index":0,"do":"delete"}'
echo "7g. naming a finding twice over"
reply '{"op":"finding","index":0,"address":"x","do":"keep"}'
echo "7h. a components finding: magnitude factors admit nothing audible"
CM=$(finding_of_lens Components)
reply "{\"op\":\"finding\",\"index\":$CM,\"do\":\"audition:ComponentMagnitudeHypothesis\"}"

echo "8. the action verb carries parameters, and refuses the ones an id does not declare"
echo "8a. audec.workspace.activate {view: 1} — the overview, by name, with no pane walking"
reply '{"op":"action","id":"audec.workspace.activate","parameters":{"view":1}}'
active_view
echo "8b. a parameter the id does not declare"
reply '{"op":"action","id":"audec.transport.toggle","parameters":{"view":1}}'
ctl '{"op":"status"}' | python3 -c 'import sys,json; print("   notice:", json.loads(sys.stdin.readline())["result"]["notice"])'
echo "8c. a parameter value the vocabulary does not have"
reply '{"op":"action","id":"audec.workspace.activate","parameters":{"view":1.5}}'
echo "8d. a parameter of the wrong shape for the id that declares it"
reply '{"op":"action","id":"audec.workspace.activate","parameters":{"view":"overview"}}'
ctl '{"op":"status"}' | python3 -c 'import sys,json; print("   notice:", json.loads(sys.stdin.readline())["result"]["notice"])'

echo "9. the separation lens: named, refreshed, and its findings' own refusals"
act audec.lens.separation
sleep 1
SEP=$(lens_view Separation)
echo "   separation lens is view $SEP (naming it started its analysis; the lens verb can still rerun it)"
for i in {1..90}; do
  STATE=$(ctl '{"op":"status"}' | python3 -c "
import sys, json
for lens in json.loads(sys.stdin.readline())['result']['lenses']:
    if lens['kind'] == 'Separation':
        print(lens['state']); break
")
  [[ "$STATE" == "Ready" || "$STATE" == "Failed" ]] && break
  sleep 1
done
echo "   separation reached $STATE after ${i}s"
lens_rows
finding_rows
HP=$(ctl '{"op":"status"}' | python3 -c '
import sys, json
for row in json.loads(sys.stdin.readline())["result"]["findings"]:
    if row["lens"] == "Separation":
        print(row["index"]); break
')
if [ -n "$HP" ]; then
  echo "9a. compare an HPSS component: the refusal names the missing plan"
  reply "{\"op\":\"finding\",\"index\":$HP,\"do\":\"compare\"}"
  echo "9b. hear one: the audition the lens offers, asked for by name"
  reply "{\"op\":\"finding\",\"index\":$HP,\"do\":\"audition:HpssHarmonic\"}"
  ctl '{"op":"status"}' | python3 -c 'import sys,json; r=json.loads(sys.stdin.readline())["result"]; print("   playing:", r["playing"], " preview:", r["preview"]["active"]); print("   notice:", r["notice"])'
else
  echo "   no separation finding was published; nothing to refuse"
fi

echo "=== app log ==="; grep -v 'control socket listening' $LIVE/app.log | head -20
ctl '{"op":"quit"}' >/dev/null 2>&1
