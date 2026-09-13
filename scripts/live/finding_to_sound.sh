#!/bin/zsh
# usage: finding_to_sound.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR, AUDEC_CACHE_ROOT)
#
# A finding is a claim about some seconds of the music. Until this lane the
# Explorer could only open one: the kept-finding record carried an address, a
# title and a revision, and nothing that said which seconds. This scenario
# drives the real desktop and reads back what the Explorer now says about
# every finding a lens published — the span, in the musician's units — which
# is the fact that "Hear" and "Make sample" on a Findings row stand on.
#
# What it cannot do yet, and says so: there is no socket verb that presses a
# lens header's "Keep finding", nor one that presses a row's Hear / Make
# sample. That verb is `finding {index|address, do: …}` (ANALYSIS_UX_AUDIT row
# 1, lane C5-Socket). The script probes for it and runs the whole arc when it
# lands; today it stops at the fact the rows carry.
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}
HERE=${0:A:h}
launch_audec "$MATERIAL" || exit 1

echo "1. open the lenses that publish findings"
ctl '{"op":"action","id":"audec.editor.reading_query"}' >/dev/null
ctl '{"op":"status"}' | python3 -c 'import sys,json; r=json.loads(sys.stdin.readline())["result"]; print("   lenses:", json.dumps(r.get("lenses")))'

echo "2. select a range so the lenses have something to be about"
ctl '{"op":"select","start":1323000,"end":1852200}' >/dev/null
sleep 8

echo "3. Investigate: every finding, with the span it is about"
ctl '{"op":"objects"}' | python3 $HERE/tree.py Investigate

echo "4. a Findings row now carries frames, not only an address"
ctl '{"op":"objects"}' > $LIVE/objects.json
python3 - "$LIVE/objects.json" <<'PY'
import sys, json
modes = json.loads(open(sys.argv[1]).readline())["result"]
rows = []
for mode in modes:
    if mode["target"].get("mode") != "Investigate":
        continue
    for category in mode["children"]:
        if category["target"].get("category") != "Findings":
            continue
        rows = category["children"]
if not rows:
    print("   no finding is published; a lens must run first")
    raise SystemExit(0)
with_span = [row for row in rows if not (row.get("detail") or "").startswith("span unknown")]
print(f"   {len(rows)} finding row(s), {len(with_span)} carrying a span")
for row in rows[:5]:
    print("   ·", row["label"], "→", row.get("detail"))
PY

echo "5. does a socket verb reach a finding yet?"
ctl '{"op":"finding","index":0,"do":"keep"}'
echo "   (an 'unknown op' above is the gap: ANALYSIS_UX_AUDIT row 1, lane C5-Socket."
echo "    With that verb this scenario continues: keep → Make sample → objects lists the sample.)"

echo "6. Library: the samples this project holds"
ctl '{"op":"objects"}' | python3 $HERE/tree.py Library

echo "=== app log ==="; grep -v 'control socket listening' $LIVE/app.log | head -20
ctl '{"op":"quit"}' >/dev/null 2>&1
