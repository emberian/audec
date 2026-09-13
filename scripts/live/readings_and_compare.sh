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

# Durability. A loaded reading used to be a bare Workbench field: it vanished
# the moment the project was reopened, and nothing on disk said it had ever
# been loaded. It is now an `audec.readings.v1` record in the workspace
# document — path plus manifest identity — and the reopen re-reads and
# re-verifies the file rather than trusting the record.
PACKAGE=$LIVE/durability.audec
rm -rf $PACKAGE

echo "10. save the project, workspace document and all"
ctl "{\"op\":\"save\",\"path\":\"$PACKAGE\"}"
for i in {1..30}; do [ -f $PACKAGE/project.json ] && break; sleep 1; done
echo "   package: $(ls $PACKAGE 2>/dev/null | tr '\n' ' ')"
python3 - "$PACKAGE/project.json" <<'MANIFEST'
import json, sys
try:
    package = json.load(open(sys.argv[1]))
except Exception as error:
    print("   the package manifest could not be read:", error)
    raise SystemExit(0)
blob = json.dumps(package)
print("   workspace document names audec.readings.v1:", "audec.readings.v1" in blob)
MANIFEST

echo "11. quit, and relaunch into the saved package"
ctl '{"op":"quit"}' >/dev/null 2>&1
sleep 2
launch_audec_package "$PACKAGE" || exit 1

# What this proves and what it cannot yet: the record survives, is read back,
# and the loader is asked for that exact file with that exact manifest. It is
# not re-listed here because a reading is verified against the project`s
# primary source material and a reopened project has none.
#
# Measured 2026-09-13 (lane C5b-Debts), correcting the earlier diagnosis in the
# follow-ups: the media DOES resolve on reopen. For this package the registered
# metadata and the resolver`s own decode agree field for field, the fingerprint
# matches, `hydrate_media` reports resolved=[AssetId(1)] unresolved=[], and
# `status.audio_error` is null. What is missing is the identity, not the audio:
# `LiveProject::from_project` (src/live_project.rs:447, the constructor
# src/project_session_lifecycle.rs:423 uses for every package open) sets
# `source: None`, so `primary_source_ids()` answers None and
# `project_local_source` refuses at its first branch. The notice below is that
# refusal, and a refused record is dropped so the app does not fail it forever.
echo "12. the reopen reads the record and asks the loader for that exact file"
ctl '{"op":"objects"}' | python3 $HERE/tree.py Readings
ctl '{"op":"status"}' | python3 -c 'import sys,json; print("   notice:", json.loads(sys.stdin.readline())["result"]["notice"])'

echo "13. re-verification is real: the file behind a recorded reading changed"
python3 - "$READING" <<'TAMPER'
import json, sys
reading = json.load(open(sys.argv[1]))
for section in reading["sections"]:
    for entity in section["payload"].get("entities", []):
        entity["label"] = entity["label"] + " (edited after it was recorded)"
json.dump(reading, open(sys.argv[1], "w"), indent=2)
TAMPER
ctl '{"op":"quit"}' >/dev/null 2>&1
sleep 2
launch_audec_package "$PACKAGE" || exit 1
ctl '{"op":"objects"}' | python3 $HERE/tree.py Readings
ctl '{"op":"status"}' | python3 -c 'import sys,json; print("   notice:", json.loads(sys.stdin.readline())["result"]["notice"])'

echo "=== app log ==="; grep -v 'control socket listening' $LIVE/app.log | head -20
ctl '{"op":"quit"}' >/dev/null 2>&1
