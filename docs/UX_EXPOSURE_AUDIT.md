# UX exposure audit

What the code can do that no UI surface reaches, and (second section, when
present) what UI controls do that differs from their label. 2026-09-02.

## Capabilities with no UI path

| capability | where it lives | closest surface | why unreachable | cost |
|---|---|---|---|---|
| Create an automation lane on a fresh project | `control_views.rs:2570` + button `:3334` | Workspace ▸ Automation | the shell refused to open the pane without a lane, and the only lane-creating button is inside it (**fixed 2026-09-02: lane 0 opens the editor empty**) | S |
| Automate anything but a mixer knob (clip gain/pan/pitch/rate/fades/reverse, residual mix, hypothesis blend, component gain/pan, lens params) | `automation.rs:127-208` | Automation `+ Lane` | the only descriptor source is `discover_mixer_parameters` | M |
| ✅ Lane algebra: simplify, time-scale, value-scale, copy/paste | `automation.rs:615-860` | Automation toolbar | no `AutomationAction` variant; test-only callers | M |
| ✅ Stem / bus / track export | `render_plan.rs:210` `RenderScope` | File ▸ Export Audio | `start_export_to` hardcodes `RenderScope::Master` | S |
| ✅ Export bit depth / dither / gain; export a loop or selection range | `export.rs:116-130`, `render.rs:45-95` | Export dialog | `export_wav` prompts only for a path; range is always the project | S |
| Plugin inserts: add, remove, reorder, expose params | `mixer.rs:777-922` | Mixer `+ insert` | `RequestInsert` answers `PluginHostNotConnected` by design; no remove/reorder variant | L |
| Tempo changes over time; time signature | `sequencer.rs:885 SetTempoMap` | transport ±1 BPM | only writer rewrites the single tempo at beat 0 | M |
| ✅ (all but the fader) Track rename / delete / reorder / lock; per-track fader | `arrangement.rs:691 PutTrack` | arrangement track header | `ArrangementAction` has only `CreateTrack`; header M/S are badges, not buttons | M |
| Re-route a track to another bus; per-clip bus override | `command.rs:128,133` | Mixer OUTPUT (buses only) | only written at track creation; overrides are only cleared or copied | S |
| Drop onto a mixer bus / pattern library | `ui_drag.rs:101,104` | arrangement and pad drops | no view binds these drop targets | S |
| Residual guide (coverage hotspots → query + audition queue) | `reading_query_workbench.rs:740`, `reading_query_view.rs:520` | Reading Query pane | pub fn with zero callers | S |
| Publish a coverage artifact | `comparison_runtime.rs:277` | Explanation workbench Excess channel | test-only caller | M |
| Export / semantically diff a portable reading | `reading_workflow.rs:219,409,599` | Reading Query ▸ PLAN IMPORT / DIFF | import wired, export not | S |
| AIR relations and modulation | `command.rs:186,194` | — | no producer anywhere | L |

Order to expose: automation editor (done), export options and range, stem
export, track header controls, non-mixer automation targets, residual
guide, mixer-bus drop target, track→bus rerouting.

## Catalog entries that reach nothing (resolved 2026-09-04: the product catalog is the live registry)

Twelve ids have adapters and sit in `PRODUCT_MENU_LAYOUT` but are registered
in neither `builtins()` nor `audec_action_registry`, so palette, menus, and
the socket refuse them: `audec.file.quit`, `audec.tempo.decrease`,
`audec.tempo.increase`, `audec.loop.clear`, `audec.workspace.focus`,
`.activate`, `.reopen`, `.float_or_dock`, `.next_tab`, `.previous_tab`,
`.next_pane`, `.previous_pane`. Four of them work under different
registered strings (`audec.workspace.float_dock`, `.next`, `.previous`);
tempo ±1 and clear-loop survive as direct buttons. `PRODUCT_MENU_LAYOUT`
itself is dead as a menu: the live macOS menu is `projected_app_menus`, and
it drops the whole Sample menu, so Make Sample / Slice to Kit / Make Beat
are reachable only by key, palette, and Explorer buttons.

Shortcut collision: the surface keymap binds Export to `cmd-e`, which is
also Split Clip's default; user bindings outrank defaults, so Split Clip
has no working key.

## Controls that do not do what their label says

✅ marks rows resolved in cycle 1 (2026-09-04); the assets half of REVEAL and the
`+ Auto` track remain. Classes: (a) acknowledges but changes nothing; (b) pane-local where the
label implies a project change; (c) claims success before it happened;
(d) present but permanently disabled or gated; (e) duplicate or fall-through
entry; (f) text that lies or is stale.

| surface | control | what it does | class | fix |
|---|---|---|---|---|
| whole shell | every refusal and success notice | written to `constructive_status`, rendered only by the sidebar the product shell hides (`workbench_render.rs:474`, `:970`) | f | **fixed 2026-09-02: rendered in the toolbar project row** |
| Explorer | Make sample / Slice to kit / Make beat | success and refusal both went to that invisible field | a | fixed with the above |
| ✅ toolbar | Save / Save as / Export WAV | bypass the projection the menu honours; Export opens a dialog then fails "arrangement is empty" (`workbench_project_io.rs:383`) | e | S |
| toolbar | "SAVED · path" | never cleared on edit, beside a live "· EDITED" chip | f | **fixed 2026-09-02** |
| ✅ Workspace menu | Next Pane / Previous Pane | dispatch NextTab: cycle tabs inside the pane (`ui.rs:603`) | f | S |
| ✅ menus | "Export Audio ⌘E", "Loop Selection ⌘L" | in the arrangement ⌘E splits a clip and ⌘L toggles loop (`arrangement_view.rs:347,364`) | f | S |
| menu/toolbar | Piano Roll / Pattern / Automation editor | open the first pattern / lane, ignoring selection (`shell_actions.rs:383,408`) | e | M |
| ✅ palette, context menu | disabled rows | still clickable, click errors (`shell_actions.rs:670,760`) | d | S |
| ✅ sampler | LOOP RANGE, PING PONG, Envelope | `SetLoop` falls to `ForwardZoneEdit`; `SampleZone` has no loop field; outcome swallowed as "Edit retained" (`constructive_controller.rs:543`, `sample_kit.rs:66-79`, `pane_audio.rs:723`) | c | L |
| ✅ sampler | TRIM 5% | error status overwritten by the success line (`sampler_view.rs:711-716`) | c | S |
| ✅ sampler | ‹ KIT / KIT › / CHOOSE EXISTING KIT | Workspace intent with no consumer in `ui/` | a | M |
| ✅ sampler, assets | REVEAL RESULT ↗ | returns false, discarded (`sampler_view.rs:1468`, `asset_view.rs:1440`) | a | S |
| ✅ sampler | "… request sent" | `emit` drops the action when no callback; the Err arm is empty (`sampler_view.rs:288-292,364`) | c | S |
| assets | PLAYABLE; PREVIEW; ★ | hardcoded for instrument rows; PREVIEW inert outside DetectOnsets; star has no click (`asset_view.rs:1069,1358,916`) | f, d | S |
| ✅ mixer | + insert; insert active/bypassed | `RequestInsert` is hardcoded `PluginHostNotConnected` yet says "sent"; the renderer bypasses every plugin (`control_actions.rs:783`, `daw_render.rs:66-68,671-690`) | d, f | L |
| ✅ mixer, automation | Undo ⌘Z / Redo | runs the whole project journal while saying "Mixer undo" (`control_actions.rs:1152`, `control_views.rs:1169`) | f | S |
| ✅ mixer | + send, OUTPUT | overwrite "not sent · no adapter" with "… intent sent" (`control_views.rs:790,603,1154`) | c | S |
| ✅ automation | write-mode button ("Read") | `set_writer_callback` has zero call sites; label never moves (`control_views.rs:2379,2707`) | d | M |
| ✅ mixer | + Channel / Group / Return | adopts a pre-computed bus id before the controller confirms (`control_views.rs:1874,583`) | c | M |
| ✅ arrangement | "Duplicated N clips", "Trimmed", "Split", "Delete sent" | past tense on hand-off; refusal invisible (`arrangement_view.rs:1865-2326`) | c | M |
| ✅ arrangement | "PROJECT TRUTH · revision 0 · SAVED" | editor rebuilt via `from_state`, always revision 0 / clean (`arrangement.rs:864-875`) | f | S |
| ✅ arrangement | ↶ / ↷; track M / S; + Auto | greyed forever yet fire; badges not buttons; automation tracks reject every clip kind (`arrangement_view.rs:2561,3929`, `arrangement_actions.rs:1354`) | d | S / S / L |
| lenses | Keep finding | `keep_reverse_finding` only re-reads; says "Finding kept" (`reverse_surface_adapter.rs:365-394`) | a, c | M |
| lens headers | "Open Findings · 7" | always index 0 (`lens_rhythm.rs:523` and siblings) | f | S |
| ✅ Loom | Mute / Gain / Event nudge; Template; Make Pattern | sketch-only, cleared on project change; Template ignores gain/mute; Make Pattern opens the first existing pattern (`lens_loom.rs:41-51,367-490`) | b, a, f | M |
| waterfall | FFT/CQT button label; FFT± in CQT; R± | label is the current mode and clicking leaves it; FFT size ignored by CQT; R± persists globally | f, a | S |
| separation | Analyze view | clamps the span to 30 s then reports "selected span is current" (`lens_hpss.rs:80-89,349`) | b, f | M |
| reverse surface | "{action} requested"; Compare | set on hand-off, reverts silently on failure; Compare renders nothing (`reverse_surface_view.rs:654`, `workbench_reverse.rs:144-162`) | c, a | M / L |
| reading query | "NO READINGS · load through the host" | no host path exists; `readings` is empty at both construction sites | d | L |
| explanation | Cancel | plan/execute/undo run synchronously; nothing to cancel (`workbench_reading.rs:432-483`) | a | M |

Worst for trust, in order: the invisible notice channel (fixed); sampler
loop/ping-pong/envelope acknowledging in green and persisting nothing;
arrangement past-tense receipts before validation; mixer Undo popping the
global stack; "+ insert" that can never succeed and "active" inserts the
renderer bypasses; the frozen "PROJECT TRUTH · revision 0 · SAVED"; "Keep
finding" keeping nothing; the automation write-mode button stuck on Read.
