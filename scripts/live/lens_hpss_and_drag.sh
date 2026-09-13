#!/bin/zsh
# usage: lens_hpss_and_drag.sh <material.flac>  (env: AUDEC_BIN, AUDEC_LIVE_DIR, AUDEC_CACHE_ROOT)
#
# Two audit rows, driven from outside the window.
#
#   row 5  separation beyond 30 s and its kernels: the span bound is a memory
#          budget, so the lens is asked for a longer span and for narrower and
#          wider median kernels, and every answer is read back from
#          status.lenses[*].settings and measured against resident memory.
#   row 9  a drag in a lens: the lens's own pointer mapping is driven through
#          the painted plot (pointer-drag@a:b, pointer-alt-drag@a:b,
#          pointer-click@a) and the result is compared with what the same
#          gesture through the overview's kernel (the `drag` / `click` verbs)
#          produces for the same samples.
#
# RSS is `ps -o rss=` in KiB on the app's own pid: the number a debug build
# actually holds. status.memory is the render-product catalog and is printed
# beside it precisely because HPSS does not enter it -- that is the point of
# giving the separation its own bound.
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}
# Remembered choices are the subject of row 19, so this scenario keeps its own
# home: `dirs::config_dir()` follows HOME, and so do the caches and stores that
# would otherwise be shared with every other lane running live right now.
[ -n "$LIVE" ] || { echo "common.sh did not set LIVE" >&2; exit 1; }
export HOME=$LIVE/home
rm -rf "$LIVE/home"; mkdir -p "$HOME"
PREFS="$HOME/Library/Application Support/software.ember.audec/preferences.json"
launch_audec "$MATERIAL" || exit 1
PID=$(cat $LIVE/audec.pid)

rss() { ps -o rss= -p $PID | tr -d ' '; }
mb() { python3 -c "import sys; print('%.1f MB' % (int(sys.argv[1]) / 1024.0))" $1; }
act() { ctl "{\"op\":\"action\",\"id\":\"$1\"}" >/dev/null; }
lens() { ctl "{\"op\":\"lens\",\"view\":$1,\"control\":\"$2\"}"; }
st() { ctl '{"op":"status"}'; }
field() { python3 -c 'import sys,json; d=json.loads(sys.stdin.readline())["result"]
for key in sys.argv[1].split("."):
    if d is None: break
    d = d[int(key)] if key.isdigit() else d.get(key)
print(json.dumps(d))' "$1"; }
notice() { st | field notice; }
sep_settings() { st | python3 -c 'import sys,json
lenses=json.loads(sys.stdin.readline())["result"]["lenses"]
for l in lenses:
    if l["kind"]=="Separation": print(json.dumps(l["settings"])); break'; }
sep_state() { st | python3 -c 'import sys,json
lenses=json.loads(sys.stdin.readline())["result"]["lenses"]
for l in lenses:
    if l["kind"]=="Separation": print("%s %s %s" % (l["state"], json.dumps(l["span"]), l["findings"])); break'; }

# Watch RSS while a separation runs, and report its peak. The lens verb
# returns as soon as the work is queued, so a single reading after the call
# measures an idle app.
watch_separation() {
  local peak=0 ticks=0
  while [ $ticks -lt 600 ]; do
    local now=$(rss); [ -z "$now" ] && break
    [ "$now" -gt "$peak" ] && peak=$now
    local state=$(sep_state | cut -d' ' -f1)
    case "$state" in
      Ready|Failed) [ $ticks -gt 4 ] && break ;;
    esac
    ticks=$((ticks + 1)); sleep 0.5
  done
  echo "$peak $(rss) $((ticks / 2))"
}

echo
echo "=== 0. the separation lens as its pane opens ==="
act audec.lens.separation; sleep 3
VIEW=$(st | field active_view)
echo "separation pane is view $VIEW"
echo "settings: $(sep_settings)"
echo "state:    $(sep_state)"
echo "status.memory: $(st | field memory)"
OPEN_RSS=$(rss); echo "RSS at open: $(mb $OPEN_RSS)  ($OPEN_RSS KiB)"

echo
echo "=== 1. row 18: a gesture says where it was placed from, and why ==="
echo "a drag named in fractions of the plot, before the pane has ever painted:"
lens $VIEW 'pointer-drag@0.20:0.60'
echo "notice: $(notice)"
echo "selection: $(st | field selection)"

echo
echo "=== 2. row 5: wait for the analysis the pane started itself ==="
watched=($(watch_separation))
echo "the lens settled at $(mb ${watched[2]}) after ${watched[3]}s"
echo "state:    $(sep_state)"
SETTLED=$(rss)

echo
echo "=== 3. row 5: the whole song is framed, H+ asks a different question ==="
lens $VIEW view-fit >/dev/null
lens $VIEW hpss-time-median-up
echo "notice:   $(notice)"
echo "settings: $(sep_settings)"
BEFORE=$(rss)
lens $VIEW refresh >/dev/null
echo "notice at the clamp: $(notice)"
watched=($(watch_separation))
echo "30 s separation: peak RSS $(mb ${watched[1]})  settled $(mb ${watched[2]})  peak over the RSS just before it $(mb $((${watched[1]} - BEFORE)))  (${watched[3]}s)"
echo "state:    $(sep_state)"
echo "notice:   $(notice)"
echo "status.memory: $(st | field memory)"
BASE_RSS=$(rss)

echo
echo "=== 4. row 5: a narrower percussive kernel, and the bound in words ==="
lens $VIEW hpss-frequency-median-down
echo "notice:   $(notice)"
echo "settings: $(sep_settings)"
for i in 1 2 3; do lens $VIEW hpss-frequency-median-down >/dev/null; done
echo "after three more P-: $(sep_settings)"
echo "notice:   $(notice)"
echo "one more P- at the floor:"
lens $VIEW hpss-frequency-median-down
echo "notice:   $(notice)"

echo
echo "=== 5. row 5: a longer span, allowed only because it fits the budget ==="
lens $VIEW hpss-span-up
echo "notice:   $(notice)"
echo "settings: $(sep_settings)"
lens $VIEW view-fit >/dev/null
BEFORE=$(rss)
lens $VIEW refresh >/dev/null
echo "notice at the clamp: $(notice)"
watched=($(watch_separation))
echo "60 s separation: peak RSS $(mb ${watched[1]})  settled $(mb ${watched[2]})  peak over the RSS just before it $(mb $((${watched[1]} - BEFORE)))  (${watched[3]}s)"
echo "state:    $(sep_state)"
echo "notice:   $(notice)"
echo "status.memory: $(st | field memory)"

echo
echo "=== 6. row 5: the next rung is refused, with the number ==="
lens $VIEW hpss-span-up
echo "notice:   $(notice)"
echo "settings: $(sep_settings)"

echo
echo "=== 7-9. row 9: three gestures in the lens, each against the overview's own verb ==="
# Each pair starts from the same cleared state, so the only difference between
# the two runs is which road the gesture took to the one timeline authority.
reset() { ctl '{"op":"loop","clear":true}' >/dev/null; ctl '{"op":"select"}' >/dev/null; }
snap() { st | python3 -c 'import sys,json
s=json.loads(sys.stdin.readline())["result"]
print(json.dumps({"playhead_sample": s["playhead_sample"], "selection": s["selection"], "loop": s["loop"]}, sort_keys=True))'; }
window() { st | python3 -c 'import sys,json
for l in json.loads(sys.stdin.readline())["result"]["lenses"]:
    if l["kind"]=="Separation": print(l["span"]["start"], l["span"]["end"]); break'; }
sample_at() { python3 -c 'import sys; a,b,f = int(sys.argv[1]), int(sys.argv[2]), float(sys.argv[3]); print(round(a + f*(b-a)))' $1 $2 $3; }

read -r WSTART WEND <<< "$(window)"
echo "the window the Separation lens is drawing: [$WSTART, $WEND]"
D1=$(sample_at $WSTART $WEND 0.20); D2=$(sample_at $WSTART $WEND 0.60)
A1=$(sample_at $WSTART $WEND 0.30); A2=$(sample_at $WSTART $WEND 0.50)
C1=$(sample_at $WSTART $WEND 0.80)
echo "0.20-0.60 of it is samples $D1-$D2 · 0.30-0.50 is $A1-$A2 · 0.80 is $C1"

echo
echo "-- a drag selects --"
reset; lens $VIEW 'pointer-drag@0.20:0.60' >/dev/null; LENS_DRAG=$(snap)
reset; ctl "{\"op\":\"drag\",\"start\":$D1,\"end\":$D2,\"alt\":false}" >/dev/null; OVER_DRAG=$(snap)
echo "lens     $LENS_DRAG"
echo "overview $OVER_DRAG"

echo
echo "-- an alt-drag authors a loop --"
reset; lens $VIEW 'pointer-alt-drag@0.30:0.50' >/dev/null; LENS_ALT=$(snap)
reset; ctl "{\"op\":\"drag\",\"start\":$A1,\"end\":$A2,\"alt\":true}" >/dev/null; OVER_ALT=$(snap)
echo "lens     $LENS_ALT"
echo "overview $OVER_ALT"

echo
echo "-- a click outside an active loop locates and disables it --"
reset; ctl "{\"op\":\"drag\",\"start\":$A1,\"end\":$A2,\"alt\":true}" >/dev/null
lens $VIEW 'pointer-click@0.80' >/dev/null; LENS_CLICK=$(snap)
reset; ctl "{\"op\":\"drag\",\"start\":$A1,\"end\":$A2,\"alt\":true}" >/dev/null
ctl "{\"op\":\"click\",\"sample\":$C1}" >/dev/null; OVER_CLICK=$(snap)
echo "lens     $LENS_CLICK"
echo "overview $OVER_CLICK"

echo
python3 - "$LENS_DRAG" "$OVER_DRAG" "$LENS_ALT" "$OVER_ALT" "$LENS_CLICK" "$OVER_CLICK" <<'SAME'
import sys
for label, lens, overview in [("drag", sys.argv[1], sys.argv[2]),
                              ("alt-drag", sys.argv[3], sys.argv[4]),
                              ("click", sys.argv[5], sys.argv[6])]:
    print("%-9s: %s" % (label, "the lens gesture and the overview verb left the app identical"
                        if lens == overview else "DIFFER\n  lens     %s\n  overview %s" % (lens, overview)))
SAME

echo
echo "=== 10. row 18: a knob and a gesture a lens cannot take, said in words ==="
act audec.lens.waterfall; sleep 2
WVIEW=$(st | field active_view)
echo "waterfall pane is view $WVIEW; a separation knob there:"
lens $WVIEW hpss-span-up
echo "an unknown control is still named:"
lens $WVIEW 'pointer-wiggle@0.1'
echo "a waterfall knob on the separation lens:"
lens $VIEW fft-size-up
act audec.lens.separation; sleep 2

echo
echo "=== 10b. found on the way: the same evidence cannot be published twice ==="
echo "Analyze view twice with nothing changed, which is what a musician does when"
echo "the view has moved and they are not sure the result is current:"
lens $VIEW refresh >/dev/null; watched=($(watch_separation))
echo "first:  $(sep_state | cut -d' ' -f1)"
lens $VIEW refresh >/dev/null; watched=($(watch_separation))
echo "second: $(sep_state)"
echo "failure: $(st | python3 -c 'import sys,json
for l in json.loads(sys.stdin.readline())["result"]["lenses"]:
    if l["kind"]=="Separation": print(json.dumps(l["failure"])); break')"

echo
echo "=== 11. row 19: the choices outlive the process ==="
echo "preferences on disk: $(cat $PREFS 2>/dev/null | tr -d '\n' || echo '(none)')"
launch_audec "$MATERIAL" || exit 1
PID=$(cat $LIVE/audec.pid)
act audec.lens.separation; sleep 3
VIEW=$(st | field active_view)
echo "after relaunch, the separation lens reads: $(sep_settings)"

echo
echo "=== 12. every knob, read back one last time ==="
echo "settings: $(sep_settings)"
echo "state:    $(sep_state)"
FINAL=$(rss); echo "RSS at the end: $(mb $FINAL)  (open was $(mb $OPEN_RSS))"

echo
echo "=== app log ==="; grep -v 'control socket listening' $LIVE/app.log | head -25
ctl '{"op":"quit"}' >/dev/null 2>&1
