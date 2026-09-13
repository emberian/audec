#!/bin/zsh
# usage: descriptor_rewrite.sh <material.flac>   (env: AUDEC_BIN, AUDEC_LIVE_DIR)
source ${0:A:h}/common.sh
MATERIAL=${1:?material path}
launch_audec "$MATERIAL" || exit 1
# Rewriting a pane's descriptor while a *different* pane holds focus used to make
# the layout authority re-enter its own publication. The ReplaceDocument that
# carries the rewrite queues a native activation for the pane that still holds
# focus; the FocusPane that follows it queues one for the rewritten pane; and
# answering the first — already superseded by the time GPUI delivers it —
# publishes the document again and queues both once more. The main thread spun at
# 100% of a core and the app never answered this socket again.
#
# `audec.editor.drums` over an open piano roll on the same pattern is the rewrite
# that is still reachable by a verb: both are PatternEditor descriptors on the
# same target, so the shell reuses the one pane and rewrites its kind
# (`DawWorkspace::activate_or_create_dynamic`). Every step here is a socket round
# trip against a 20 s deadline, so being answered at all is the assertion.
act() { timeout 20 python3 $HERE/ctl.py "{\"op\":\"action\",\"id\":\"$1\"}" || { echo "   FAIL: no answer within 20 s after $1 — the main thread is spinning" >&2; exit 1; }; }
# `status.active_view` is the Workbench's mirror of the layout's focused pane and
# it is refreshed when an action runs, not when status is asked, so it reports the
# focus as of the *previous* action (a cycle-3 follow-up, not this lane's). Where
# the settled focus is the claim, re-assert the same editor first: that is a
# no-op for the layout and refreshes the mirror.
av() { timeout 20 python3 $HERE/ctl.py '{"op":"status"}' | python3 -c 'import sys,json; print(json.loads(sys.stdin.readline())["result"].get("active_view"))' 2>/dev/null || echo "NO ANSWER"; }
# Re-assert `$1` (a no-op for the layout) and report the focus it settles on.
# Not a command substitution: `act` must be able to end the run from here.
report_settled() { act "$1" >/dev/null; sleep 1; echo "   settled on view $(av)${2}"; }
cpu() { ps -o %cpu= -p $(cat $LIVE/audec.pid) | tr -d ' '; }
echo "0. baseline: active_view=$(av)"
echo "1. a pattern to edit (the pattern editors refuse without one)"
ctl '{"op":"select","start":2646000,"end":2998800}' '{"op":"loop","start":2646000,"end":2998800}' >/dev/null
sleep 2; act audec.sample.make_beat; sleep 3
echo "2. open the piano roll on it"; act audec.editor.piano_roll; sleep 2
report_settled audec.editor.piano_roll
echo "3. leave it: focus a different pane"
act audec.workspace.next_tab >/dev/null; sleep 1
report_settled audec.workspace.next_pane
echo "4. THE REWRITE: drums on the same pattern, with another pane focused"
act audec.editor.drums; sleep 3
echo "   answered; cpu=$(cpu)%"
report_settled audec.editor.drums "   (the rewritten pattern editor)"
echo "5. rewrite it back and forth, each time from another pane, and keep answering"
for i in 1 2 3; do
  act audec.workspace.next_tab >/dev/null; sleep 1
  act audec.editor.piano_roll >/dev/null; sleep 1
  act audec.workspace.next_tab >/dev/null; sleep 1
  act audec.editor.drums >/dev/null; sleep 1
  echo "   round $i:"; report_settled audec.editor.drums ", cpu=$(cpu)%"
done
echo "6. a command whose transition carries no focus effect"
# guise`s `PaneGroup::restore` rebuilds the tree and parks focus on leaves[0]
# with only a `cx.notify()`, no event. Most commands hide that because their
# transition ends in a `NativeWindowEffect::Focus` that puts focus back.
# `ReplaceWindowLayout` does not — `replace_window_layout` finishes with no
# effects at all — and that is what a divider drag and a native split lower to
# (`DynamicWorkspaceRoot::sync_layout`). There is no socket verb for either:
# both are pointer gestures on the pane group, and the shipped main window is a
# single `DockLayout::Pane` (ui.rs `initial_tabs`), so leaves[0] is the only
# leaf and the disagreement would not be visible from here even if there were.
# Driving it needs a socket verb that splits a dock pane or moves a divider.
# The headless proof is workspace_ui::tests::
# a_divider_drag_leaves_the_native_focus_where_the_layout_records_it.
# What IS reachable is `close_tab`, whose main-window transition also carries no
# focus effect: the focus restatement now runs on it, so the app must still
# settle on the pane the layout records.
act audec.workspace.close >/dev/null; sleep 1
report_settled audec.editor.drums "   (after a command with no focus effect)"
echo "7. still answering:"; ctl '{"op":"ping"}'
echo "=== app log ==="; grep -v 'control socket listening' $LIVE/app.log | head -20
ctl '{"op":"stop"}' >/dev/null
