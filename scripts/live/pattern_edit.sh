#!/bin/zsh
# usage: pattern_edit.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR, AUDEC_CACHE_ROOT)
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}
launch_audec "$MATERIAL" || exit 1
# DAW audit row 1, live: the editor the toolbar "Piano / drums" button now
# opens is the pane the menu opens, and an audition from it is heard.
#
# Where "heard" is read: status.preview is the *pane* preview bus (sampler pad
# gates); a pattern audition is not on it. It renders the exact placement cycle
# and installs it on the open audio host, and the Workbench says so only after
# that install returns Ok -- "Playing exact pattern audition". Every other
# outcome is a named refusal in the same channel. Before this lane the same
# sequence through the toolbar answered "Pattern audition requires a project
# workspace pane", because the toolbar built an editor with no audition source.
act() { ctl "{\"op\":\"action\",\"id\":\"$1\"}"; }
st() { ctl '{"op":"status"}' | python3 -c 'import sys,json; r=json.loads(sys.stdin.readline())["result"]; print("   ", {k:r.get(k) for k in ("revision","active_view","notice","audio_error")})'; }
pv() { ctl '{"op":"status"}' | python3 -c 'import sys,json; print("   preview:", json.loads(sys.stdin.readline())["result"].get("preview"))'; }
notice() { ctl '{"op":"status"}' | python3 -c 'import sys,json; print(json.loads(sys.stdin.readline())["result"].get("notice"))'; }
row() { ctl '{"op":"actions"}' | python3 -c "
import sys, json
rows = json.loads(sys.stdin.readline())['result']
hit = next((r for r in rows if r['id'] == '$1'), None)
print('   %s enabled=%s reason=%s' % ('$1', hit and hit['enabled'], hit and hit['disabled_reason']) if hit else '   $1 is not in the catalog')
"; }

echo "0. baseline"; st; pv
echo "1. with no pattern in the project, the drums editor refuses by name"
act audec.editor.drums; sleep 1; st
echo "   and so does the audition, because no pattern pane is active:"
row audec.pattern.audition
echo "2. make a beat from a selection (kit, pads, pattern, occurrence, routes)"
ctl '{"op":"select","start":2646000,"end":2998800}' '{"op":"loop","start":2646000,"end":2998800}' >/dev/null
sleep 2
act audec.sample.make_beat; sleep 4
echo "   waiting for the audio host to open (an audition with no host is refused);"
echo "   preview.bus_active is null until the host exists"
for i in {1..180}; do ctl '{"op":"status"}' | grep -q '"bus_active": [tf]' && break; sleep 1; done
echo "   audio host open after ${i}s"; st
echo "3. open the drums editor by the action the toolbar button now sends"
act audec.editor.drums; sleep 3; st
echo "   the audition verb is now offered:"; row audec.pattern.audition
echo "4. audition the placed cycle from that editor  <-- row 1 proof"
# The audition renders, then plays on the open host. The editor answers in its
# own status and the shell repeats that answer in the notice channel, so the
# whole round trip is readable here: Preparing -> Playing, or a named refusal.
# A first bounce still in flight answers "project audio is not ready"; ask again.
heard=no
for attempt in {1..20}; do
  act audec.pattern.audition >/dev/null
  for i in {1..40}; do
    n=$(notice)
    case "$n" in
      *"Playing exact pattern audition"*) heard=yes; break;;
      *"Pattern audition"*) break;;
    esac
    sleep 0.25
  done
  echo "   attempt $attempt -> $n"
  [[ $heard == yes ]] && break
  sleep 2
done
echo "   HEARD FROM THE TOOLBAR-OPENED EDITOR: $heard"; st
echo "5. the same from the piano roll pane"
act audec.editor.piano_roll; sleep 3; row audec.pattern.audition
act audec.pattern.audition >/dev/null
for i in {1..40}; do n=$(notice); [[ "$n" == *"Pattern audition"* || "$n" == *"Playing exact"* ]] && break; sleep 0.25; done
echo "   piano roll -> $n"
echo "6. copy/paste and pattern length are pointer/key verbs with no action id yet."
echo "   No socket verb reaches them: the actions listing carries no pattern edit"
echo "   but the audition, so they are gated headless instead --"
echo "     sequencer_view::step_workflow::tests::a_copied_batch_pastes_into_the_lane_it_came_from"
echo "     sequencer_view::step_workflow::tests::a_paste_over_an_occupied_cell_or_past_the_end_refuses_by_name"
echo "     sequencer_view::step_workflow::tests::a_paste_into_a_pattern_without_that_lane_names_the_lane"
echo "     sequencer_view::step_workflow::tests::a_lane_that_moved_identity_still_resolves_by_its_name"
echo "     sequencer_view::tests::a_clipboard_of_the_other_kind_refuses_by_name"
echo "     sequencer_view::tests::each_paste_from_one_clipboard_lands_after_the_last"
echo "     sequencer_view::tests::a_pattern_length_reads_in_the_largest_unit_it_divides"
echo "     pattern_controller::tests::a_pattern_can_be_made_longer_and_refuses_to_shorten_over_what_is_written"
echo "     pattern_controller::tests::a_generated_pattern_takes_its_length_from_its_expression"
echo "   The pattern edit ids that would close this are the DAW audits structural"
echo "   fix (parameterised action parameters), which belongs to the socket lane."
echo "=== app log ==="; grep -v 'control socket listening' $LIVE/app.log | head -20
ctl '{"op":"quit"}' >/dev/null 2>&1
