#!/bin/zsh
# usage: editors_and_windows.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR)
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}
launch_audec "$MATERIAL" || exit 1
# Live: float the active pane into a native window and dock it back; report window counts and any failure notice.
st() { ctl '{"op":"status"}' | python3 -c 'import sys,json; r=json.loads(sys.stdin.readline())["result"]; print({k:r.get(k) for k in ("windows","active_view","notice","io")})'; }
act() { ctl "{\"op\":\"action\",\"id\":\"$1\"}"; }
echo "0. baseline"; st
echo "1. float active pane"; act audec.workspace.float_dock; sleep 2; st
echo "2. dock it back"; act audec.workspace.float_dock; sleep 2; st
echo "3. open dynamic analysis panes"; act audec.analysis.waterfall; act audec.analysis.rhythm; sleep 2; st
echo "4. next tab (activate the new analysis pane), then float it"; act audec.workspace.next; sleep 1; st; act audec.workspace.float_dock; sleep 2; st; echo "4b. dock it back"; act audec.workspace.float_dock; sleep 2; st
echo "5. open editors via actions"; for a in audec.editor.arrangement audec.editor.mixer audec.editor.piano_roll audec.editor.sampler audec.editor.assets audec.editor.automation audec.editor.drums; do act $a; done; sleep 2; st
echo "=== app log ==="; grep -v 'control socket listening' $LIVE/app.log | head -20
