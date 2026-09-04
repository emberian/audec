# Loom: sketch edits become project edits (cycle 2, lane C2-Loom)

Resolves the Loom rows of `UX_EXPOSURE_AUDIT.md`: mute / gain / event nudge
are sketch-only and vanish on project change; Template audition ignores
gain and mute; Make Pattern opens the first existing pattern instead of the
one it made.

## Today

`LoomViewResult.sketch: SequenceSketch` (`src/ui/lens_loom.rs`) is edited by
`toggle_loom_cluster`, `adjust_loom_cluster_gain`, `edit_nearest_loom_event`
(enable, gain, offset). Each edit re-inserts a construction product into the
Workbench's per-artifact map and notifies. `apply_loom_sequence` calls
`Workbench::execute_loom_result_construction(artifact, finding, cx)`, which
builds `LoomConstructionIntent { artifact, finding, source_span, sketch,
label, diverged_from_evidence, created_unix_ms, target_bus }`
(`src/constructive_controller.rs:180`) and publishes a kit + pattern +
occurrence through the constructive plan; then it calls
`open_sequencer_editor` (the first pattern). Nothing binds the pane to what
it created; later sketch edits change nothing in the project.

## Target

Two phases, both honest:

1. **Before Make pattern** the header says `SKETCH · not in the project
   until Make pattern`, and the Template audition renders from the sketch
   as edited (gain and mute applied), so what you hear is what Make pattern
   will make.
2. **After Make pattern** the pane binds to the construction it created:
   `LoomBinding { kit: KitId, pattern: PatternId, clusters: BTreeMap<ClusterId, PadId>, events: BTreeMap<EventId, StepAddress> }`,
   taken from the publication (`ConstructivePublication { kit, pattern,
   created_pads, ... }` plus the plan's cluster→pad and event→step mapping,
   which the lowering already computes; expose it on the outcome rather than
   recomputing). From then on:
   - cluster mute → `SampleKitPut` toggling the pad's zones' enabled state
     (or pad gain −inf if no enabled flag exists; say which), cluster gain →
     pad/zone `gain_db`, both through `edit_kit` as one undoable revision;
   - event enable / gain / offset → the pattern's step edits through the
     existing pattern commands (`pattern_workflow` / `SequencerCommand`
     step put), one undoable revision each, quantised the way the lowering
     quantised them;
   - the header says `BOUND · pattern "<name>" · kit "<name>"`, and the
     status line follows the requested → committed receipt rule.
   - If the bound pattern or kit disappears (undo past Make pattern, or a
     delete), the pane returns to phase 1 and says so.
3. **Make pattern opens the pattern it made**: use the publication's
   `pattern` id; if the host has no "open this pattern" entry point that the
   lane owns, add the pattern id to the existing focus path
   (`ConstructivePublishedFocus::Pattern(id)` already exists) instead of
   calling `open_sequencer_editor`.

## Files owned

`src/ui/lens_loom.rs`, the Loom arms of `src/constructive_controller.rs`
(`execute_loom_result_construction`, `plan_loom_construction` and their
helpers), `src/loom_construction.rs` if it exists (verify), pattern step
command lowering in `src/pattern_workflow.rs` (only what the step edits
need), new tests in `src/cycle11_flow.rs`. NOT yours: `src/ui.rs`,
`src/ui/workbench_*.rs`, `src/sequencer_view.rs`, `src/sample_kit.rs`
(ask if a pad enabled flag is needed and report instead).

## Gate

Flow test: make a Loom pattern from a sketch, then mute one cluster through
the bound path and assert the kit changed and undo restores it; nudge one
event and assert the pattern step moved. Live: `scripts/live/*.sh` still
pass; a Loom-specific socket drive is not available (the lens controls are
pane buttons), so the flow test is the proof; say so.
