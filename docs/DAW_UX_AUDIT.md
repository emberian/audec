# DAW audit (2026-09-12)

From the musician's chair, at main after cycle 4 wave 1. Evidence is
file:line at that HEAD. Companion to `ANALYSIS_UX_AUDIT.md`. Ranked by
musician value over cost.

| # | a musician wants | today | smallest correct change | files | size | value |
|---|---|---|---|---|---|---|
| 1 | hear the pattern I'm drawing | the toolbar's "Piano / drums" opens a sequencer with no audition: `open_sequencer_editor` passes `None` as the audition source (`ui/workbench_editors.rs:184`), so every audition says "Pattern audition requires a project workspace pane" / "Shared pattern audition callback is not connected"; the pane path (`:478`) works | delete `open_sequencer_editor`/`open_arrangement_editor`; the toolbar buttons use the same `PaneOpenIntent` the menu uses | workbench_editors, workbench_render | S | very high |
| 2 | copy a bar of notes and paste it | no copy/paste in the pattern editors; 28 chords, none `cmd-c/v/x`; only whole-pattern DUP | one selection clipboard on `SequencerEditor` emitting `PatternEdit::PutNote/PutStep` at a tick offset; `cmd-c/v` | sequencer_view, pattern_actions | M | very high |
| 3 | turn one clip down 3 dB | `Clip.gain_db` exists and is displayed read-only; `ArrangementAction` has no gain, mute or rename; the only way is an automation curve | `SetClipGain`, `SetClipMuted`; ± in the inspector | arrangement_view, arrangement_actions | S | very high |
| 4 | crossfade two clips; loop a clip; fade with a key | `plan_phrase_fade`, `plan_crossfade`, `plan_phrase_repeat`, `plan_phrase_stretch` exist with precise refusals and no production caller (`arrangement_keyboard.rs:461-635`); fades are a pointer-only corner drag | wire the four planners to toolbar buttons and keys through the `PhraseEditPlan` dispatch trim already uses (`arrangement_view.rs:2272`) | arrangement_view, arrangement_keyboard | S–M | very high |
| 5 | start a song by writing notes | piano roll/drums refuse on an empty project ("No pattern to edit yet · Make beat from a selection, or Place a pattern", `shell_actions.rs:523`) while the editor's own "+ NEW" would create one | on empty, create a default 4-bar pattern (the `CreatePatternIntent` "+ NEW" builds) and open it | shell_actions | S | very high |
| 6 | not lose an hour of work | a never-saved project is never autosaved (`workbench_project_io.rs:762`); a clean autosave shows "RECOVERY AVAILABLE · n" | autosave unsaved projects into the recovery root; "AUTOSAVED · hh:mm" for the clean case | workbench_project_io, ui.rs, project_store | M | very high |
| 7 | reorder inserts | `MixerGraph::move_processor` exists (`mixer.rs:900`); no `MixerAction` calls it | `MoveInsert { processor, index }`; ↑/↓ on the row | control_actions, control_views | S | high |
| 8 | set a section's tempo to 140 | ±1 BPM per press, no keys; mark-at-playhead places the current bpm; no numeric entry, tap, drag, or point removal (`TempoMap` has no remove) | a BPM text field planning one `TempoPointIntent` at the playhead's segment; `RemoveTempoPoint` | workbench_render, workbench_transport, musical_time_workflow, sequencer | M | high |
| 9 | record a take | nothing reaches the UI: `TransportMode::Recording`/`Session::record()` have no callers; MIDI-in is behind a feature whose only consumer is a bin; no metronome, count-in, punch or arm | in order: (a) a metronome click node from the tempo map, (b) MIDI-in → live pattern audition, (c) note-record-to-pattern, (d) audio input last | workbench_transport, midi_input, session | L (a is S) | high |
| 10 | reverse a sample; shape an envelope | "Reverse playback is not persisted by the sample-zone model" (`sampler_view.rs:1332`); envelope is a two-state toggle though the model stores ADSR; loop range not editable; no root note or key range | `reverse` on `SampleZone` + `ZoneEditIntent::SetReverse`; four ± rows for ADSR; drag handles on the range bar | sample_kit, sampler_view, pane_audio | M | high |
| 11 | markers and regions | none; `SnapGuideKind::Marker` has no producer | `markers` on `ArrangementState` with `PutMarker`, drawn on the ruler, feeding the snap guide | arrangement, arrangement_view, arrangement_interaction | M | high |
| 12 | name and colour clips | `Clip.name` drawn, no rename path; colour from kind only | `RenameClip` reusing the track-rename editor; `color` on `Track` first | arrangement_view, arrangement_actions | S | medium-high |
| 13 | change a pattern's length | `PatternEdit` has no `SetLength`; clip transpose/gain/muted written as defaults and edited nowhere | `PatternEdit::SetLength` with the existing validation; a length field in the header | pattern_actions, sequencer_view | S–M | medium-high |
| 14 | a fader I can type into; double-click to unity | drag-only plus ±1 dB buttons; no reset, no fine modifier, no keys | double-click → 0 dB / centre; shift-drag fine | control_views | S | medium-high |
| 15 | a moving meter | one peak/RMS per bus for the whole cohort, republished per tick | window the reduction to the playhead's last ~50 ms of the cohort | control_actions, workbench_publication | M | medium-high |
| 16 | bounce a track in place | nothing; `RenderScope::Track` already exports | "Bounce track to audio clip": export the track scope into the pool and place one clip | workbench_project_io, arrangement_actions | M | medium-high |
| 17 | place a clip without a mouse | every clip creation is a drop; "+ Audio/+ Pattern" create tracks | `audec.clip.place_selected_asset_at_playhead` | arrangement_view, ui_actions | S | medium |
| 18 | automate the way I bend it | Bezier unreachable; every point Linear; no lane under its track since "+ Auto" went | a per-track automation strip in the arrangement reusing `render_curve` | control_views, automation | M | medium |
| 19 | swing a melody; shorten a note by key | swing reaches step patterns only; `swing_notes` has no caller | route `EditorCycleSwing` through `swing_notes` for note content | sequencer_view, sequencer | S | medium |
| 20 | see why a window didn't open | three `eprintln!`s (`workbench_editors.rs:27,132`; `workbench_project_io.rs:412`) | set `constructive_status`/`project_io_status` beside each | as listed | S | medium |
| 21 | keys for hourly verbs | five chords; tempo ±, mark tempo, cycle meter, clear loop, insert, route, diff have none; arrangement `s` (Cycle Snap) collides with the catalog's `s` (Make Sample) | re-chord the arrangement's `s`; defaults for tempo ±, mark tempo, clear loop | ui.rs, ui_actions, arrangement_view | S | medium |
| 22 | sends from the master; pre/post | refusals correct and verbatim; in good shape | — | — | — | — |
| 23 | export with a tail / a limiter | strong; missing tail padding, normalise (string exists, no option), SRC, a limiter the doc comment promises | `tail_seconds` on `ExportOptions`; delete or implement the limiter sentence | export, workbench_project_io | S | medium |
| 24 | a mixer that keeps my edits | "Mixer opened without a project; channel edits are not kept" then lets you move faders | refuse to open, or disable the strips | workbench_editors | S | low-medium |

## Scriptability

Socket verbs: `ping status actions action open seek click drag select loop
play pause stop export objects reading_import reading_export lens quit`;
46 catalog ids. Scriptable today: file/transport/loop/undo verbs,
`audec.clip.split` (the only clip verb), editor opens, the three sample
verbs, tempo ±/mark, meter cycle, insert filter, route selected, audition
diff, the pane verbs. Not scriptable at all: every other clip edit, every
pattern edit, every sampler edit, every mixer edit but two, every
automation edit, track create/rename/reorder/delete/route, markers,
tempo-point removal. The cheapest structural fix: let `action` carry the
`ActionParameters` the registry already models (`ui_actions.rs:426-461`)
and register one parameterised id per domain (`audec.clip.set_gain`,
`audec.pattern.put_note`, `audec.mixer.set_gain`,
`audec.automation.put_point`); size M, converts roughly two-thirds of the
rows above from "untestable by script" to "gated".

Also: `control_views.rs:3883` discards `lane.insert_point(point)` while
building the curve preview, so the preview can disagree with the renderer.
