#!/bin/zsh
# usage: sampler_pane_teardown.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR)
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}
launch_audec "$MATERIAL" || exit 1
# Closing a pane must not leave its audition playing. `status.preview` is the
# finite preview bus by owner: `active` is what is sounding, `held_pads` are the
# pad gates the workbench still believes are down. Both stay empty here, before
# and after the close.
act() { ctl "{\"op\":\"action\",\"id\":\"$1\"}"; }
pv() { ctl '{"op":"status"}' | python3 -c 'import sys,json; r=json.loads(sys.stdin.readline())["result"]; print({"active_view": r.get("active_view"), "windows": r.get("windows"), "preview": r.get("preview")})'; }
# Opening an editor does not make its pane active, and the primary tab refuses
# Close, so walk the tabs until a Close is actually accepted. The reply's
# `notice` is the refusal when there is one.
close_a_tab() {
  for i in {1..6}; do
    act audec.workspace.next_tab >/dev/null; sleep 1
    local reply=$(act audec.workspace.close)
    echo "   $reply"
    echo "$reply" | grep -q '"notice": null' && return 0
    sleep 1
  done
  return 1
}
echo "0. baseline"; pv
echo "1. open a sampler pane by action"; act audec.editor.sampler; sleep 3; pv
echo "2. audition trigger available over the socket?"
ctl '{"op":"actions"}' | python3 -c '
import sys, json
rows = json.loads(sys.stdin.readline())["result"]
hits = [r["id"] for r in rows if any(w in r["id"] for w in ("audition", "pad", "preview"))]
print("   pad-audition action ids:", hits or "none registered")
'
echo "   (no socket verb or action starts a pad gate; the pane callback is the only"
echo "    trigger, so the audible half of teardown is proved by the unit test"
echo "    pane_audio::pane_teardown_releases_only_that_panes_pad_audition)"
echo "3. close a pane -> accepted, and nothing is playing, no pad held"
close_a_tab && echo "   closed" || echo "   no tab accepted Close"
sleep 2; pv
echo "4. open and close twice more, to show nothing accumulates"
for i in 1 2; do act audec.editor.sampler >/dev/null; sleep 2; close_a_tab >/dev/null; sleep 1; done; pv
echo "=== app log ==="; grep -v 'control socket listening' $LIVE/app.log | head -20
ctl '{"op":"stop"}' >/dev/null
