#!/bin/zsh
# usage: readings_and_compare.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR)
#
# A reading is what this project claims about its material, in a file someone
# else can read. This scenario proves the round trip on the real desktop: the
# app writes one, reads it back, verifies it against the decoded source, and
# lists it in the Explorer under Readings. Then it proves the refusals: a
# manifest that does not match the bytes, and a reading of other material.
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}
HERE=${0:A:h}
launch_audec "$MATERIAL" || exit 1

READING=$LIVE/project.reading.json
TAMPERED=$LIVE/tampered.reading.json
FOREIGN=$LIVE/foreign.reading.json
rm -f $READING $TAMPERED $FOREIGN

echo "1. open the reading/query pane by action"
ctl '{"op":"action","id":"audec.editor.reading_query"}'
sleep 2

echo "2. export this project's own reading"
ctl "{\"op\":\"reading_export\",\"path\":\"$READING\"}" | tee $LIVE/export.json
DIGEST=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["result"]["manifest_digest"])' $LIVE/export.json)
echo "   manifest digest: $DIGEST"
echo "   file: $(wc -c < $READING) bytes"

echo "3. import it back, verified against the digest the export reported"
ctl "{\"op\":\"reading_import\",\"path\":\"$READING\",\"manifest_digest\":\"$DIGEST\"}"

echo "4. the Explorer lists it under Readings"
ctl '{"op":"objects"}' | python3 $HERE/tree.py Readings

echo "5. the pane pane's own status line"
ctl '{"op":"status"}' | python3 -c 'import sys,json; print("   notice:", json.loads(sys.stdin.readline())["result"]["notice"])'

echo "6. refusal: the bytes do not match the manifest identity the caller supplied"
python3 - "$READING" "$TAMPERED" <<'PY'
import json, sys
reading = json.load(open(sys.argv[1]))
for section in reading["sections"]:
    for entity in section["payload"].get("entities", []):
        entity["label"] = entity["label"] + " (edited)"
json.dump(reading, open(sys.argv[2], "w"), indent=2)
PY
ctl "{\"op\":\"reading_import\",\"path\":\"$TAMPERED\",\"manifest_digest\":\"$DIGEST\"}"

echo "7. refusal: a reading of other material"
python3 - "$READING" "$FOREIGN" <<'PY'
import json, sys
reading = json.load(open(sys.argv[1]))
reading["source"]["fingerprints"] = [{"algorithm": "sha256", "bytes": "00" * 32}]
json.dump(reading, open(sys.argv[2], "w"), indent=2)
PY
ctl "{\"op\":\"reading_import\",\"path\":\"$FOREIGN\"}"
ctl '{"op":"status"}' | python3 -c 'import sys,json; print("   notice:", json.loads(sys.stdin.readline())["result"]["notice"])'

echo "8. nothing was retained by a refused import: still one reading"
ctl '{"op":"objects"}' | python3 $HERE/tree.py Readings

echo "9. Investigate: findings, explanations, comparisons the session retains"
ctl '{"op":"objects"}' | python3 $HERE/tree.py Investigate

echo "=== app log ==="; grep -v 'control socket listening' $LIVE/app.log | head -20
ctl '{"op":"quit"}' >/dev/null 2>&1
