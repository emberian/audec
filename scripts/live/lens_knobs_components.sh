#!/bin/zsh
# usage: lens_knobs_components.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR, AUDEC_CACHE_ROOT)
#
# The analysis half asked a different question, and said so.
#
#   1. the components lens opens with a question that can be read back
#   2. K+ changes it, and the refactor really recomputes the song: the header
#      count moves, and the Findings are republished
#   3. clicking a component selects the seconds it owns, by a stated rule
#   4. the finding cursor reaches the 2nd..Nth finding, and refuses past the end
#   5. constant-Q says how many bins per octave it is using, and FFT+/- step it
#   6. a transform that cannot run on this material is refused in words and the
#      preference is NOT overwritten by the fallback
#   7. all of it survives a relaunch
#
# HOME is redirected into the scenario's own scratch directory on purpose:
# preferences live under the platform config directory, and a scenario that
# proves persistence must not write the musician's real preferences file (nor
# race another lane that is reading it).
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}

export HOME=$LIVE/home
mkdir -p $HOME
PREFS="$HOME/Library/Application Support/software.ember.audec/preferences.json"
rm -f $PREFS

py() { python3 -c "$1" "${@:2}"; }
lens() { ctl "{\"op\":\"lens\",\"view\":$1,\"control\":\"$2\"}"; }
act() { ctl "{\"op\":\"action\",\"id\":\"$1\"}" >/dev/null; }
status() { ctl '{"op":"status"}'; }

# Read one path out of a status reply on stdin. Paths are python expressions
# over the parsed `result`.
pluck() { py 'import sys,json;r=json.loads(sys.stdin.readline())["result"];print(eval(sys.argv[1],{},{"r":r}))' "$1"; }
# The components lens object out of a status reply.
comp() { pluck "[l for l in r[\"lenses\"] if l[\"kind\"]==\"Components\"][0]$1"; }
water() { pluck "[l for l in r[\"lenses\"] if l[\"kind\"]==\"Waterfall\"][0]$1"; }
notice() { pluck 'r["notice"]'; }
# The error message of a lens reply, verbatim, or the settings if it worked.
reply_error() { py 'import sys,json;m=json.loads(sys.stdin.readline());print(m.get("error") or "(accepted)")'; }

wait_components() {  # wait_components <expected state> <seconds>
  local want=$1 limit=$2 i
  for i in {1..$limit}; do
    [ "$(status | comp '["state"]')" = "$want" ] && { echo "   components reached $want after ${i}s"; return 0; }
    sleep 1
  done
  echo "   components never reached $want within ${limit}s (it is $(status | comp '["state"]'))"
  return 1
}

launch_audec "$MATERIAL" || exit 1
VIEW_PREFS=$PREFS

echo
echo "=== 1. the question the lens opens with ==="
act audec.lens.components
sleep 2
CV=$(status | pluck 'r["active_view"]')
echo "   components lens is view $CV"
wait_components Ready 300 || { tail -20 $LIVE/app.log; exit 1; }
status | comp '["settings"]'
echo "   findings published: $(status | comp '["findings"]')"

echo
echo "=== 2. K+ twice, then Refactor ==="
lens $CV components-rank-up | reply_error
echo "   notice: $(status | notice)"
lens $CV components-rank-up | reply_error
echo "   notice: $(status | notice)"
echo "   asked: $(status | comp '["settings"]["rank"]') components, shown: $(status | comp '["settings"]["shown"]')"
BEFORE_FINDINGS=$(status | comp '["findings"]')
lens $CV refresh | reply_error
echo "   notice: $(status | notice)"
sleep 2
wait_components Ready 600 || { tail -20 $LIVE/app.log; exit 1; }
echo "   notice: $(status | notice)"
echo "   settings now: $(status | comp '["settings"]')"
echo "   findings: $BEFORE_FINDINGS -> $(status | comp '["findings"]')"
echo "   component finding titles:"
status | pluck '"\n".join("     "+f["title"]+"  ("+f["address"]+")" for f in r["findings"] if f["lens"]=="Components")'

echo
echo "=== 3. the seconds a component owns ==="
for k in 1 2; do
  lens $CV "component-span:$k" | reply_error
  echo "   C$k notice: $(status | notice)"
  echo "   C$k selection: $(status | pluck 'r["selection"]')"
  echo "   C$k playhead: $(status | pluck 'round(r["playhead_seconds"],3)') s"
done
echo "   refusal for a component that is not there:"
lens $CV "component-span:99" | reply_error
echo "   notice: $(status | notice)"
echo "   refusal for a component numbered from zero:"
lens $CV "component-span:0" | reply_error

echo
echo "=== 4. the finding cursor ==="
echo "   selected finding starts at $(status | comp '["settings"]["selected_finding"]')"
lens $CV components-finding-next | reply_error
echo "   after ▸ : $(status | comp '["settings"]["selected_finding"]')"
for i in {1..12}; do lens $CV components-finding-next >/dev/null; done
echo "   after twelve more ▸ : $(status | comp '["settings"]["selected_finding"]')"
echo "   one more ▸ :"
lens $CV components-finding-next | reply_error
lens $CV components-finding-previous >/dev/null
echo "   after ◂ : $(status | comp '["settings"]["selected_finding"]')"
echo "   Open the finding the cursor is on:"
ctl "{\"op\":\"finding\",\"index\":$(status | comp '["settings"]["selected_finding"]'),\"do\":\"open\"}" | py 'import sys,json;m=json.loads(sys.stdin.readline());print("     "+(m.get("error") or json.dumps(m["result"].get("notice"))))'
echo "   refusal when a waterfall is asked for a components control:"
act audec.lens.waterfall; sleep 2
WV=$(status | pluck 'r["active_view"]')
lens $WV components-rank-up | reply_error

echo
echo "=== 5. constant-Q says its pitch grid, and FFT+/- step it ==="
echo "   waterfall now: $(status | water '["settings"]')"
lens $WV spectral-transform | reply_error
sleep 3
echo "   after transform: $(status | water '["settings"]')"
lens $WV fft-size-up | reply_error
sleep 3
echo "   after FFT+: bins/oct $(status | water '["settings"]["cqt_bins_per_octave"]')"
echo "   FFT+ at the finest grid:"
lens $WV fft-size-up | reply_error
echo "   notice: $(status | notice)"
lens $WV fft-size-down >/dev/null; sleep 3
lens $WV fft-size-down >/dev/null; sleep 3
echo "   after FFT- twice: bins/oct $(status | water '["settings"]["cqt_bins_per_octave"]')"
echo "   FFT- at the coarsest grid:"
lens $WV fft-size-down | reply_error
echo "   notice: $(status | notice)"
lens $WV fft-size-up >/dev/null; sleep 3
echo "   left at bins/oct $(status | water '["settings"]["cqt_bins_per_octave"]'), transform $(status | water '["settings"]["transform"]')"
echo "   preferences on disk:"
cat "$PREFS" | py 'import sys,json;print("     "+json.dumps(json.load(sys.stdin),sort_keys=True))'

echo
echo "=== 6. the same choices after a relaunch ==="
launch_audec "$MATERIAL" || exit 1
act audec.lens.components; sleep 2
CV=$(status | pluck 'r["active_view"]')
wait_components Ready 600 || { tail -20 $LIVE/app.log; exit 1; }
echo "   components asked for: $(status | comp '["settings"]')"
act audec.lens.waterfall; sleep 2
WV=$(status | pluck 'r["active_view"]')
echo "   waterfall: $(status | water '["settings"]')"

echo
echo "=== 7. a transform this material cannot take ==="
# MAX_FREQUENCY is 16 kHz and constant-Q needs its top bin below Nyquist, so a
# 24 kHz material is exactly the case the fallback exists for.
NARROW=$LIVE/narrow.flac
[ -f $NARROW ] || sox "$MATERIAL" -r 24000 $NARROW trim 0 20
soxi $NARROW | sed -n '2,5p' | sed 's/^/   /'
launch_audec $NARROW || exit 1
act audec.lens.waterfall; sleep 2
WV=$(status | pluck 'r["active_view"]')
echo "   this lens opened remembering: $(status | water '["settings"]["transform"]') at $(status | water '["settings"]["cqt_bins_per_octave"]')/oct"
lens $WV refresh | reply_error
# The components product publishes its own notice a few seconds later, so read
# the refusal out of the notice channel as it appears rather than after.
for i in {1..40}; do
  N=$(status | notice)
  case "$N" in *"could not run"*) break;; esac
  sleep 1
done
echo "   notice: $N"
echo "   settings: $(status | water '["settings"]')"
echo "   the preference on disk is still:"
cat "$PREFS" | py 'import sys,json;print("     transform = "+json.load(sys.stdin)["spectrum"]["transform"])'

echo
echo "=== app log ==="; grep -v 'control socket listening' $LIVE/app.log | head -20
ctl '{"op":"quit"}' >/dev/null 2>&1
