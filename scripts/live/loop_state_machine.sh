#!/bin/zsh
# usage: loop_state_machine.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR)
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}
launch_audec "$MATERIAL"
# Live loop-vs-click scenario through the real pointer kernel.
SR=44100; s() { echo $(( $1 * SR )); }
echo "1. drag 60-68 with no loop -> selection only, no loop"; ctl_status "{\"op\":\"drag\",\"start\":$(s 60),\"end\":$(s 68)}"
echo "2. loop from selection (action) -> loop 60-68 enabled"; ctl_status '{"op":"action","id":"audec.loop.from_selection"}' '{"op":"status"}' | tail -1
echo "3. play immediately (host may not be open yet), then wait 10s -> playing inside 60-68"; ctl_status '{"op":"play"}'; sleep 10; ctl_status '{"op":"status"}'
echo "4. drag 100-108 while looping+playing -> loop replaced by 100-108, still playing, playhead near 100"; ctl_status "{\"op\":\"drag\",\"start\":$(s 100),\"end\":$(s 108)}"; sleep 1; ctl_status '{"op":"status"}'
echo "5. click at 104 inside loop -> seek to 104, loop kept, selection cleared"; ctl_status "{\"op\":\"click\",\"sample\":$(s 104)}"
echo "6. click at 200 outside loop -> loop disabled (bounds kept), playhead 200, still playing"; ctl_status "{\"op\":\"click\",\"sample\":$(s 200)}"; sleep 1; ctl_status '{"op":"status"}'
echo "7. pause; drag 30-34 with loop disabled -> selection only, loop stays disabled"; ctl_status '{"op":"pause"}' >/dev/null; ctl_status "{\"op\":\"drag\",\"start\":$(s 30),\"end\":$(s 34)}"
echo "8. alt-drag 40-44 -> loop 40-44 enabled"; ctl_status "{\"op\":\"drag\",\"start\":$(s 40),\"end\":$(s 44),\"alt\":true}"
echo "9. toggle loop action twice"; ctl_status '{"op":"action","id":"audec.loop.toggle"}' '{"op":"status"}' | tail -1; ctl_status '{"op":"action","id":"audec.loop.toggle"}' '{"op":"status"}' | tail -1
ctl '{"op":"stop"}' >/dev/null
