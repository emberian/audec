#!/bin/zsh
# usage: editors_and_windows.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR)
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}
launch_audec "$MATERIAL" || exit 1
# Live: float the active pane into a native window and dock it back; report window counts and any failure notice.
st() { ctl '{"op":"status"}' | python3 -c 'import sys,json; r=json.loads(sys.stdin.readline())["result"]; print({k:r.get(k) for k in ("windows","active_view","notice","io")})'; }
act() { ctl "{\"op\":\"action\",\"id\":\"$1\"}"; }
av() { ctl '{"op":"status"}' | python3 -c 'import sys,json; print(json.loads(sys.stdin.readline())["result"].get("active_view"))'; }
close_row() { ctl '{"op":"actions"}' | python3 -c '
import sys, json
row = next(r for r in json.loads(sys.stdin.readline())["result"] if r["id"] == "audec.workspace.close")
print("   audec.workspace.close enabled=%s reason=%s" % (row["enabled"], row["disabled_reason"]))
'; }
echo "0. baseline"; st
echo "0b. Close is projected as the workspace would answer it (overview active)"; close_row
echo "1. float active pane"; act audec.workspace.float_or_dock; sleep 2; st
echo "2. dock it back"; act audec.workspace.float_or_dock; sleep 2; st
echo "3. open dynamic analysis panes"; act audec.analysis.waterfall; act audec.analysis.rhythm; sleep 2; st
echo "4. next pane (activate the new analysis pane), then float it"; act audec.workspace.next_tab; sleep 1; st; act audec.workspace.float_or_dock; sleep 2; st; echo "4b. dock it back"; act audec.workspace.float_or_dock; sleep 2; st
echo "5. open editors via actions"; for a in audec.editor.arrangement audec.editor.mixer audec.editor.piano_roll audec.editor.sampler audec.editor.assets audec.editor.automation audec.editor.drums; do act $a; done; sleep 2; st
# Opening an editor is a request to work in it, and Next Pane means panes: both
# must move `active_view`, or every workspace verb aims at the wrong pane.
echo "6. opening an editor activates its pane"
before=$(av); act audec.editor.sampler >/dev/null; sleep 2; after=$(av)
echo "   audec.editor.sampler: active_view $before -> $after"
[[ "$before" != "$after" ]] && echo "   OK: the sampler pane is active" || echo "   FAIL: active_view did not follow the editor"
echo "   Close now offered for the sampler pane:"; close_row
echo "7. next pane moves the active pane"
before=$after; act audec.workspace.next_pane >/dev/null; sleep 1; after=$(av)
echo "   audec.workspace.next_pane: active_view $before -> $after"
[[ "$before" != "$after" ]] && echo "   OK: active_view followed the focused pane" || echo "   FAIL: active_view did not move"
echo "8. previous pane comes back"
before=$after; act audec.workspace.previous_pane >/dev/null; sleep 1; after=$(av)
echo "   audec.workspace.previous_pane: active_view $before -> $after"
echo "=== app log ==="; grep -v 'control socket listening' $LIVE/app.log | head -20
