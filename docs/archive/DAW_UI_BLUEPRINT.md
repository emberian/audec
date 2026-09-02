# DAW UI blueprint: a pleasant construction surface for audec

This document turns the model and engine direction in
[`DAW_ARCHITECTURE.md`](DAW_ARCHITECTURE.md) into a concrete GPUI interaction
contract. It is deliberately specific about ownership, gestures, focus,
commands, persistence, and acceptance tests so that the DAW surface does not
become decorative controls around the current single-file analyzer.

The reference workflow is the useful everyday loop of an LMMS-class DAW:
create tracks, browse or record material, place clips and patterns, edit notes
and steps, automate parameters, route and mix, loop a region, and render. Audec
adds a second loop that LMMS does not have: turn evidence into editable
production structure and continuously compare the explanation with the source
and residual. The UI should make both loops fast without pretending an
inference is an authored fact.

This is an audit of the audec tree on 2026-08-31 and LMMS revision `4e677cb`.
It is an implementation blueprint, not a claim that the listed editors already
exist.

## 1. Current UI: what is real and what is not

The current application is already a coherent forensic listening prototype:

- `ui::Workbench` in `src/ui.rs` owns one `Analysis`, one `AudioHost`, a
  sample-addressed `TimelineViewport`, time selection, a loop range, transport
  polling, and the workbench's spectral images.
- `Workbench::render_header`, `render_sidebar`, `render_timeline`, and
  `render_inspector` make a fixed four-region window: 54 px global header,
  220 px material/layer sidebar, center analysis timeline, and 220 px analysis
  inspector.
- The center timeline supports pointer selection/seeking, command-wheel zoom
  around the pointer, horizontal/shift-wheel pan, follow, fit, selection-to-loop,
  and resolution-aware spectral refinement. `timeline::TimelineViewport` is a
  good independent navigation primitive.
- `ui::Visualizer` implements Waterfall, Rhythm, Components, Separation, and
  Loom as separate native GPUI windows. It retains local time/frequency ranges,
  spectral settings, HPSS state, and Loom state while a window lives.
- `audio::Transport` and `audio_host::AudioHost` provide exact project-frame
  playback and a separate audition bus. This is the correct beginning for a
  global transport and browser/device preview.
- `session::Session` has typed arrangement IDs, tracks, lanes, clips, events,
  clusters, object/time selection, snapping, labeled atomic changes, and
  undo/redo. `project::ProjectDocument` seeds that model from analysis and
  preserves AIR identity links.
- `mixer::MixerGraph` has buses, inserts, sends, mute/solo, parameters, routing
  validation, commands, and latency plans.
- `workspace::WorkspaceModel` and the Guise bridge model a split/tab tree,
  stable built-in IDs, native floating placements, dock-back, and JSON
  snapshots. `docs/WORKSPACE.md` contains a compile-checked integration plan.
- `lens.rs` captures substantially richer time, frequency, channel, trigger,
  scope, spectrum, waterfall, and vectorscope parameters than the current UI
  exposes.
- `persistence.rs` can atomically persist a versioned project manifest with
  material, workspaces, lens records, and artifacts.

The important gaps are architectural, not cosmetic:

1. `Workbench` is still the main window root and owns project/audio/view state.
   `workspace::create_pane_group` is compiled and tested but not mounted.
2. Opening an analysis view calls `Workbench::open_visualizer`, constructing a
   fresh `Visualizer` and native window every time. There is no activate,
   dock, move, reuse, or dock-back lifecycle in the running app.
3. The rendered center is not `session::Arrangement`. It is a fixed stack of
   source-derived waveform/spectrum/features with a time-range overlay. No
   tracks or clips can be selected or manipulated there.
4. `ProjectDocument`, `Session`, `MixerGraph`, and `WorkspaceModel` are not the
   UI's authoritative state. A DAW control added directly to `Workbench` today
   would create another shadow model.
5. There is no asset browser, pattern library, note/step/automation model or
   editor, mixer surface, parameter inspector, action registry, menu model,
   contextual focus system, serializable project-wide command vocabulary, or
   saved editor targeting.
6. The current global key contexts (`"Audec"` and `"AudecLens"`) bind the same
   small action list. `Cmd-1` through `Cmd-5` open forensic windows, conflicting
   with the editor switching convention proposed in `DAW_ARCHITECTURE.md`.
7. The current left/right arrows seek five seconds. In an editor, unmodified
   arrows must primarily operate on the focused selection/edit cursor; transport
   seeking needs explicit global actions.
8. Current visualizers use local normalized time coordinates and in several
   cases regenerate whole-source images. Workspace integration must not freeze
   this implementation detail into the DAW editor contract.

The implementation order below resolves these ownership conflicts before
building editor surfaces.

## 2. Product interaction invariants

These are user-visible invariants, complementing the engine invariants in
`DAW_ARCHITECTURE.md`.

1. **One song, many views.** Arrangement, piano roll, drum editor, automation,
   mixer, and forensic lenses are projections of one `ProjectDocument`. Moving
   a tab or window never clones musical truth.
2. **Transport is global; navigation is local.** Playing, recording, looping,
   punching, tempo, and the canonical playhead are shared. Each editor retains
   its own viewport and follow policy. Panning never seeks.
3. **A gesture previews, then commits once.** During a drag, the canvas renders
   a transient edit preview. Pointer-up dispatches one labeled transaction;
   Escape cancels it; undo never walks through individual mouse-move events.
4. **Selection is explicit and visible.** Object selection, time range, focused
   editor target, hover, edit cursor, loop, punch, and playhead use distinct
   colors/shapes and cannot silently substitute for each other.
5. **Tools are predictable.** The pointer tool can perform the most common
   move/trim gesture by hit zone; draw, knife, erase, audition, and spectral
   probe are explicit modes. Temporary modifiers never permanently change the
   selected tool.
6. **Double-click descends; Escape ascends.** Double-click a pattern clip to
   open its definition in the appropriate editor. Escape first cancels a
   gesture, then clears a subordinate selection, then returns focus/target to
   the parent arrangement.
7. **Every parameter has one address.** Inspector, automation editor, mixer,
   hardware control, and context menu resolve the same `ParameterAddress` and
   descriptor. A visual knob is never the identity of a parameter.
8. **Evidence remains legible.** Signal, inference, and authored construction
   have stable visual grammars. Confidence is an overlay, not a fake volume or
   velocity. Accepted deprojection remains linked to its evidence and residual.
9. **Keyboard operation is complete.** Anything available only by a tiny drag
   handle is unfinished. Actions, menus, shortcuts, command palette, and
   accessibility invoke the same command path.
10. **The UI stays calm while work is asynchronous.** Analysis, decoding,
    graph compilation, model inference, plugin scan, and render show bounded
    progress/cancel/failure state without modalizing ordinary editing.

## 3. Target ownership and GPUI entity graph

The first structural patch should establish these ownership boundaries before
adding editor widgets:

```text
ApplicationController                         proposed src/app.rs
├── CommandRegistry / Keymap                  proposed src/commands.rs
├── WorkspaceWindowRegistry                  proposed src/workspace_ui.rs
└── ProjectSession entity                    proposed src/project_session.rs
    ├── ProjectDocument                      extend src/project.rs
    ├── ProjectHistory                       replace split histories
    ├── ProjectAudioController               extend src/audio_host.rs
    ├── Analysis/Asset/Graph workers         existing foundations
    ├── GlobalTransportState                 bridge src/audio.rs + session.rs
    ├── GlobalSelectionState                 evolve session::Selection
    └── ViewLinkRegistry                     use lens types + link facets

WorkspaceRoot                                proposed src/workspace_ui.rs
├── Entity<ProjectSession>
├── GlobalTransportBar                       proposed src/ui/transport.rs
├── Entity<PaneGroup>                        bridge in src/workspace.rs
└── WorkspaceItemRegistry
    ├── ArrangementEditor                    proposed src/ui/arrangement.rs
    ├── AssetBrowser                         proposed src/ui/browser.rs
    ├── Inspector                            proposed src/ui/inspector.rs
    ├── PianoRollEditor                      proposed src/ui/piano_roll.rs
    ├── DrumEditor                           proposed src/ui/drum_editor.rs
    ├── AutomationEditor                     proposed src/ui/automation.rs
    ├── MixerEditor                          proposed src/ui/mixer.rs
    └── AnalysisLensAdapter                  extract from src/ui.rs Visualizer
```

`ProjectSession` is a GPUI-friendly interaction façade, not a second project.
It owns the only mutable `ProjectDocument`, applies `ProjectCommand`, publishes
lightweight revisioned read snapshots, and coordinates audio graph compilation.
Editor entities store stable target IDs, viewport/tool state, and transient
gesture state only. They must not store editable copies of clips, patterns,
automation, mixer buses, AIR objects, or assets.

`Workbench` should be dismantled incrementally:

- Move analysis/material/audio/transport operations to `ProjectSession`.
- Extract the existing center rendering into an `ArrangementEditor` analysis
  track renderer so no working visualization is thrown away.
- Adapt each `Visualizer` into a persistent `AnalysisLensAdapter` entity.
- Make `WorkspaceRoot`, rather than `Workbench`, the root opened by `main.rs`.
- Keep a compatibility constructor that imports the command-line audio path
  into the new session during the migration.

Risk: GPUI entity update leases make cycles and reentrant updates easy to
create. Follow `docs/WORKSPACE.md`: workspace owns child entity handles;
analysis lenses may own `Entity<ProjectSession>`; Guise render/title closures
capture a weak workspace entity; `ProjectSession` never owns view entities.

## 4. Main window and visual hierarchy

The default editing workspace should be immediately useful at 1440×900 while
remaining responsive down to 1024×700:

```text
┌──────────────────────────────────────────────────────────────────────────┐
│ menu/title      tools  snap    transport/time/tempo       CPU/xrun/export│ 48
├───────────────┬──────────────────────────────────────────┬───────────────┤
│ Browser       │ tab strip: Arrangement | Waterfall ...  │ Inspector     │
│               ├──────────────────────────────────────────┤               │
│ Project       │ ruler / markers / loop / punch           │ Context       │
│ Places        │ ┌ track headers ┬ arrangement canvas ┐   │ properties    │
│ Samples       │ │               │                    │   │ routing       │
│ Presets       │ │               │                    │   │ provenance    │
│ AIR           │ └───────────────┴────────────────────┘   │               │
├───────────────┴──────────────────────────────────────────┴───────────────┤
│ status: hover help · snap · selection summary · worker/graph diagnostics │ 24
└──────────────────────────────────────────────────────────────────────────┘
```

The browser and inspector are ordinary dock items and may be tabbed, collapsed,
or floated. The transport is outside `PaneGroup`: changing or closing a tab may
not hide global playback state. At narrow widths, inspector collapses first,
then browser. An editor's own toolbar lives immediately below its tab strip so
tool state is visually local.

### Visual grammar

Use the existing `src/ui.rs` palette as named theme tokens rather than raw
constants scattered across editors:

| Plane/state | Default treatment | Existing source |
| --- | --- | --- |
| Background/chrome | `BACKGROUND`, `PANEL`, `PANEL_ALT`, `BORDER` | `src/ui.rs` constants |
| Signal/material | cyan/neutral filled waveform or spectral energy | current track timeline |
| Inference | magenta/amber outline or hatched fill; confidence opacity | current rhythm/components |
| Authored construction | solid user/track color | new arrangement/pattern editors |
| Selected | bright 2 px outline plus selection fill, never color alone | new shared selection theme |
| Hover | 1 px light outline; no model change | new canvas hit-test layer |
| Muted/disabled | reduced saturation and opacity, retained outline | session/mixer mute states |
| Residual | lime with signed/difference visual motif | Loom/HPSS precedent |
| Error/missing | warning stripe plus icon/text | persistence/plugin/asset states |

Use an 8 px base spacing grid, 28–32 px control heights, 44 px minimum touch
target where a control stands alone, 11–12 px secondary text, and 13–14 px
primary labels. Dense editors may use smaller visual marks but must expose a
larger hit target. Track colors tint clips; they do not replace semantic
signal/inference/authored markings.

The global status bar is not decoration. It reports the action under the
pointer, exact time/value, active snap, drag delta, disabled-action reason,
background task state, graph rebuilding, cache misses, xruns, and dropped RT
events. This prevents transport and analysis failure from becoming invisible.

## 5. Dock root, tabs, and native tear-offs

### Runtime item identity

`workspace::BuiltinView` is sufficient for the six current singletons but not
for multiple piano rolls, automation lanes, lens forks, or A/B views. Replace
the enum-only registry at the persistence boundary with descriptors:

```rust
pub struct WorkspaceItemDescriptor {
    pub id: WorkspaceViewId,
    pub kind: WorkspaceItemKind,
    pub target: EditorTarget,
    pub title_override: Option<String>,
    pub link_group: LinkGroupId,
    pub state: EditorViewState,
}

pub enum WorkspaceItemKind {
    Arrangement,
    Browser,
    Inspector,
    PianoRoll,
    DrumEditor,
    Automation,
    Mixer,
    AnalysisLens(LensKind),
}
```

Keep IDs 1–6 reserved exactly as `workspace::BuiltinView::id` defines them.
Allocate new persisted item IDs monotonically from a serialized counter.
`BuiltinItemIds` currently depends on deterministic Guise allocation order;
extend the bridge with a stable `WorkspaceViewId ↔ ItemId` runtime map rather
than assuming every future item has a fixed enum index.

### Dock behavior

Mount `workspace::create_pane_group_with_dots` in `WorkspaceRoot` and wire all
`PaneGroupEvent`s described in `docs/WORKSPACE.md`. The host policies are:

- Arrangement is pinned in the default workspace but can be duplicated as a
  second view with independent navigation. The last project-bearing editor may
  not be closed while it is the only way to recover hidden dock items.
- Browser and Inspector close by becoming hidden; `View > Browser/Inspector`
  restores the same entity and state.
- Activating an already-open singleton focuses it instead of creating a copy.
- “New View” explicitly duplicates eligible editor/lens descriptors and assigns
  a new view ID. Duplicate defaults to a new viewport link group.
- Tab status dots communicate dirty local parameter forks, active analysis,
  missing targets, or unread failure. They do not mean project dirty state.
- Dragging tabs inside the main window uses Guise. “Float to New Window” is
  always available even if native-window drag negotiation is not.

### Native tear-off contract

Use the state-preserving flow already designed in `docs/WORKSPACE.md` and
modeled by `WorkspaceModel::float_view` / `dock_back`:

1. Guise detaches the item and emits `TearOff`.
2. `WorkspaceRoot` defers opening a GPUI window containing the same editor
   entity, not a newly constructed editor.
3. The floating root renders a thin local title bar with Dock Back, link-group,
   always-on-top, and window menu controls. Global transport remains reachable
   through a compact transport strip or global shortcuts.
4. Dock Back inserts the same entity into the last main pane (or primary pane
   if unavailable), focuses it, then closes the native window.
5. Closing a floating window docks by default. “Close View” is explicit and
   separately undoable only for persistent view descriptors, not project data.
6. Shutdown uses a guard so close callbacks do not overwrite the saved floating
   layout as docked.

Acceptance requires two concurrent lens windows playing against one session,
independent zoom, optional linked selection/time, uninterrupted transport, and
preserved in-flight analysis through float/dock-back.

## 6. Arrangement editor

The arrangement is the default center and the first genuinely productive
vertical slice. It should replace the current “whole source plus derived rows”
without losing those rows.

### Structure

```text
ArrangementEditor
├── ArrangementToolbar       pointer/draw/knife/erase, snap, grid, follow
├── MarkerRuler              bars/beats or time, markers, loop, punch
├── TrackHeaderColumn        scroll-locked vertically with canvas
├── ArrangementCanvas        clips/events/automation/analysis lanes
├── HorizontalScrollbar      independent of transport
└── OverviewMinimap          optional when project exceeds several screens
```

The first imported project uses `project::ProjectLayout`: “Source audio” and
“Analysis hypotheses” become actual rows. Existing source waveform,
log-frequency field, feature lines, onsets, and beat marks become renderers for
`session::Track`/`Lane` content rather than a hard-coded stack. The fixed
`ARRANGEMENT_GUTTER` in `src/ui.rs` becomes the initial track-header width,
resizable and persisted per view.

`timeline::TimelineViewport` should become a signed `ProjectFrame` viewport or
a generic `TimeViewport`; its current unsigned coordinates cannot display
preroll even though `session::Sample` allows negative time. Keep its tested
pointer-anchor zoom, clamped pan, and follow-safe-region behavior.

### Clip geometry and hit zones

Every visible clip has one body and stable sub-hit zones, scaled so each edge
handle remains at least 6 physical pixels:

- top label/header: name, pattern-link badge, inferred/authored badge;
- body: waveform, notes, steps, automation preview, or analysis marks;
- left/right 6 px: trim handles;
- lower body with Option held: slip source while placement stays fixed;
- upper corners: fade-in/fade-out handles for audio;
- repeat boundary: loop extent handle for pattern/audio loop mode;
- warning corner: missing asset/device/stale render diagnostics.

The pointer tool chooses move, trim, fade, or loop by hit zone and changes the
cursor before pointer-down. It must never reinterpret a body drag as trim after
crossing a boundary.

### Required interactions

| Gesture/action | Preview | Commit target | Current foundation / gap |
| --- | --- | --- | --- |
| Click clip | object selection; Shift toggles; Cmd adds/removes | ephemeral selection | `session::Selection.clips`; not wired to UI |
| Drag empty space | marquee by default; drag ruler selects time | selection only | workbench has only time drag |
| Drag body | ghost clip(s), delta label, snap guides | `EditClips(Move)` transaction | `Clip.timeline`; no edit helper/UI |
| Drag edge | source/timeline trim preview | `EditClips(Trim)` | `source_start` only; needs explicit source end/mapping |
| Option-drag body | source ghost and offset | `EditClips(Slip)` | model incomplete |
| Knife click / Cmd-E | split preview | `SplitClips` transaction | no clip split command/renderer |
| Cmd-D / Option-drag | linked duplicate | `DuplicateClips` | ID allocator exists; command absent |
| Drag fade handle | curve overlay and dB tooltip | `EditClipFades` | model absent |
| Drag between tracks | valid target highlight/routing hint | move lane/track transaction | track/lane model exists, no command/UI |
| Double-click audio | open clip/source inspector or waveform lens | view target only | lens foundation exists |
| Double-click pattern | open piano roll/drum editor for definition | view target only | pattern model absent |
| Space while dragging | audition preview from gesture start | no project commit | `AudioHost` audition bus exists |

Snapping combines grid, markers, clip edges, loop/punch edges, transients,
events, and playhead. Show the winning guide and label; hold Cmd to suppress snap
temporarily and Shift for fine movement. Snap comparisons occur in screen-space
tolerance but commit exact sample/beat coordinates.

### Track headers

Each row provides name/color, type/device badge, arm, monitor, mute, solo,
gain/pan mini-controls, routing summary, collapse, and lane disclosure. Track
height has compact/normal/tall and free-resize. Modifier-click solo means
exclusive solo; modifier-click mute applies to selected tracks. Track reorder
shows a full-width insertion rule and commits one ordered-list command.

The current `session::Track` has gain/mute/solo but no color, bus, height,
record state, device, or stable presentation model. Mixer truth belongs in
`mixer::MixerGraph`; track header controls dispatch parameter/mixer commands
rather than maintaining a second fader value.

## 7. Asset and deprojection browser

The left browser is how audec becomes fast to make music with, not merely an
Open Audio dialog.

### Sections

- Project: used assets, patterns, instruments, buses, renders, missing items.
- Places: favorites, recent folders, user collections.
- Samples: indexed audio with duration, channels, loudness, detected tempo/key
  labeled as hypotheses, tags, and waveform.
- Devices: native Sampler, Drum Rack, Basic Synth, processors, plugins.
- Presets: versioned instrument/effect presets.
- AIR: events, recurring families, components, hypotheses, residual regions.
- Searches: saved queries and smart collections.

`persistence::MaterialReference` and `ProjectManifest.artifacts` are only a
starting point. Add the `AssetRegistry` specified in `DAW_ARCHITECTURE.md` and
keep the replaceable browser index outside project truth.

### Interactions

- Single click selects and shows metadata/provenance in Inspector.
- Space or speaker button auditions through `AudioHost`'s independent preview
  bus; the project transport keeps its state. Up/down changes the previewed
  result without moving focus to transport.
- Drag sample to arrangement creates an audio asset/clip transaction; drag to
  a drum pad creates or remaps a Sampler target; drag a recurrent AIR family
  creates a deprojection preview before commit.
- Return opens the item; double-click inserts at the edit cursor only when the
  destination is unambiguous.
- Search is incremental and keyboard-focused with Cmd-F when browser is active.
- A/B and residual audition buttons are visible for inferred assets/templates.

Risks: indexing must never run on GPUI or audio threads; browser preview must
not steal the main transport; a path is not asset identity; inferred tempo/key
must not be rendered as authoritative metadata.

## 8. Piano roll

The piano roll edits a `PatternDefinition::Notes`, not AIR pitch observations
and not instrument runtime voices. Its target bar shows pattern name, linked
instance count, active instrument, loop length, scale, grid, and a prominent
Make Unique action when editing a linked definition from one arrangement clip.

### Layout and behavior

```text
toolbar: pointer draw erase knife | grid swing | scale fold | quantize/legato
piano keyboard | beat ruler + note grid
               | velocity / release / probability / expression lane stack
```

- Vertical wheel scrolls pitch; horizontal/Shift-wheel pans time; Cmd-wheel
  zooms time; Cmd-Shift-wheel zooms pitch height.
- Clicking the keyboard auditions the track instrument; dragging glissandos.
- Draw drag sets start/duration; single click may insert the configured default
  length. Right drag erases only in explicit erase mode or temporary erase
  modifier, never as a surprising default context-menu conflict.
- Pointer drag moves notes; edge drag resizes; Alt/Option duplicates; Shift
  constrains to one axis; Cmd suppresses snap.
- Marquee and lasso select; Cmd-A scopes to the focused pattern, not the whole
  project. Notes outside the clip instance remain editable but visibly shaded.
- Velocity stems and expression curves use the same selected notes; drawing a
  ramp previews exact values. Per-note MPE pitch/pressure/timbre are separate
  lanes.
- Ghost notes from sibling patterns are view-local overlays with explicit
  source colors and click-to-target policy.
- Quantize is a non-destructive dialog/popover with strength, start/end policy,
  and preview; humanize exposes its seed so rerenders are reproducible.

No current source type represents authored notes or patterns. Implement the
`NoteEvent`, `NotePattern`, `PatternDefinition`, and beat-time types from
`DAW_ARCHITECTURE.md` under new model modules before the editor. Reuse
`pitch.rs` and AIR pitch trajectories only through an explicit
`NotesFromPitch` deprojection command with alternatives and source ghosts.

## 9. Drum and step editor

The drum editor combines the useful immediacy of an LMMS pattern editor with
audec's recurrence evidence. It targets `PatternDefinition::Steps` and a
`DrumRack`/trigger mapping.

### Layout

```text
pattern selector / length / resolution / swing / follow
lane header: pad, audition, name, source badge, choke, mute/solo | step cells…
parameter lane: velocity | probability | microtiming | gate | ratchet | pitch | pan
bottom: source waveform/template and explanation/residual compare
```

Click toggles a step at default velocity. Drag across cells paints one state;
dragging back over the same cell in the same gesture does not toggle it twice.
Shift-drag sets velocity, Option-drag probability, and an expanded cell popover
edits all parameters accessibly. Cmd-drag copies a range. Bracket actions shift
selected steps by one subdivision; duplicate-length grows the pattern while
preserving step IDs where possible.

Drag a browser sample or Loom template to a lane target. Drag an AIR recurring
event family into empty space to open a deprojection preview showing proposed
pad/template, events, tempo/phase alternatives, quantization error, explained
audio, and residual. Accept creates an ordinary editable step pattern,
instrument mapping, arrangement instance, and semantic backlinks in one
transaction. “Accept” never renames a mixed family “kick” or “snare” without
authored input.

`loom::SequenceSketch` and `session::{Cluster, Event}` provide reusable event
families and editable occurrences; `rhythm.rs` provides hypotheses. They are
not yet a persistent beat-relative step pattern, pad mapping, or command path.
The UI must not bind directly to the temporary `Visualizer::loom_state`.

## 10. Automation editor and inline lanes

Automation appears both inline under an arrangement track and in a full editor.
Both edit the same lane bound to the universal `ParameterAddress` specified in
`DAW_ARCHITECTURE.md`.

### Full editor

- Header: parameter breadcrumb, unit-aware current value, time domain, binding
  mode, interpolation, grid, automation read/touch/latch/write.
- Canvas: points, segments, tangents, loop boundaries, ghost measured feature,
  and optional modulator result.
- Lower lane list: multiple selected parameters for aligned editing; values are
  normalized for geometry but labels and snapping use plain units.

Pointer selects/moves points, draw creates a sampled/coalesced curve, knife
adds a point on the evaluated curve, and tangent mode adjusts Bezier handles.
Box selection may transform time and value independently. Flip, scale, offset,
thin, smooth, quantize, and convert-measurement operations preview and commit
one command. Stepped parameters visibly prohibit curved interpolation.

AIR curves in `ontology.rs` describe evidence and object-relative transforms;
`mixer::ProcessorParameter` exposes normalized plugin values. Neither is yet a
project-wide automation lane. Add descriptors, mappings, smoothing, and stable
addresses before UI binding. Converting an AIR modulation feature creates an
authored automation approximation while retaining the measured curve as a
locked ghost and semantic link.

## 11. Mixer and contextual inspector

### Mixer

The mixer is a dockable editor with horizontally scrollable channel strips and
a pinned master strip. It renders `mixer::MixerGraph` directly through project
snapshots:

- source/type/color/name and route destination;
- input/monitor/arm when supported;
- pre/post meters, peak hold, clipping and reset;
- insert slots with bypass, wet, latency, missing/crashed state;
- sends with pre/post indicator and target;
- pan, fader, numerical value, mute, solo, PFL;
- group/VCA and sidechain affordances only after their model semantics exist.

Fader drags have fine mode, double-click reset, exact text entry, and
accessibility increment/decrement. Multi-selected channels move relatively;
Option applies the same absolute value. Meter repaint subscribes to a bounded
audio snapshot and never makes project revisions. Control changes dispatch
`ProjectCommand::EditMixer`; structural routing requests graph recompilation.

`MixerGraph`, `MixerCommand`, effective solo state, routing validation, and
latency plans in `src/mixer.rs` are strong foundations. Gaps are document
ownership, track-to-bus binding, meters, sidechain/channel semantics, universal
parameter binding, compiled DSP, and UI.

### Inspector

The right inspector is contextual and stable under keyboard traversal. Its
sections depend on selection:

- no object: project/tempo/export summary;
- track: identity, device, routing, gain/pan, provenance/analysis role;
- audio clip: placement, source range, stretch, gain, fades, channels, asset;
- pattern clip: definition, repeats, transpose, overrides, Make Unique;
- note/step/event: exact time, pitch/velocity/gain, probability, provenance;
- automation: parameter address/unit/interpolation/binding;
- AIR hypothesis: evidence, alternatives, confidence, producer/model/license,
  accept/reject/deproject and audition/residual;
- mixer bus/device: route, latency, parameters, state diagnostics.

The current `Workbench::render_inspector` is playhead metrics only. Preserve
those metrics as an “At Playhead” inspector section and add an inspectable
selection priority: explicit object > time range > track > playhead. Every
field indicates whether it is editable, measured, inferred, or derived.

## 12. Global transport and ruler

The transport bar is always present in the main window and compact floating
windows. It includes:

- jump start/previous marker, rewind, stop, play/pause, record, forward/next;
- loop, punch, metronome, count-in, follow and sync state;
- canonical time display switchable among bars/beats/ticks, seconds, and
  samples; direct entry seeks;
- tempo and meter with explicit detected-versus-authored status;
- master level/clip indicator, audio device state, CPU/xrun and graph-building
  status;
- render/export entry point.

Space toggles play globally except while text entry or a modal capture consumes
it. Stop once returns to the last start position; stop twice returns to project
start, configurable. Record is disabled with an accessible reason until a
valid target/input exists. Loop and punch are half-open ranges with separate
handles in every time ruler.

Bridge the exact `audio::TransportHandle`/`TransportSnapshot` to a single
ephemeral transport controller. Retire `Workbench.playhead_seconds` as stored
truth; format seconds from the exact atomic frame on repaint. The current 33 ms
polling task can remain the UI cadence. `session::Transport` should either be
the control-side façade over that same handle or be removed as duplicate state.
The present `−5s/+5s` controls may remain as transport actions but should not
own the arrow keys.

Tempo shown from `analysis::RhythmAnalysis` must be labeled “candidate” until
the user accepts or authors a tempo map. The transport needs the explicit
`TempoMap` and beat/frame conversion designed in `DAW_ARCHITECTURE.md` before
bars/beats editing is enabled.

## 13. Focus, actions, shortcuts, and command palette

### Focus tree

Every window has one focused workspace item; every editor has one focused
region (toolbar, ruler, header list, canvas, parameter lane, text field). GPUI
`FocusHandle`s remain window-local, while `ProjectSession` stores only semantic
targets and selection. Focus movement must not mutate selection unless an
action explicitly says so.

Use hierarchical key contexts instead of the current two flat contexts:

```text
Global
  Workspace
    Arrangement | PianoRoll | DrumEditor | Automation | Mixer | AnalysisLens
      Canvas | TrackHeaders | Ruler | ParameterLane
  Browser
  Inspector
  TextInput
  Modal
```

TextInput suppresses destructive/editor letter keys but permits global Escape,
save, and transport only according to platform convention. Modal captures only
the actions it owns. A floating lens gets the same editor context as when
docked.

### Action registry

Replace the monolithic `actions!(audec, [...])` in `src/ui.rs` with stable
action IDs registered independently from default key bindings. Each action
reports label, category, enabled/checked state, disabled reason, default keys,
and handler scope. Buttons, menus, context menus, shortcuts, accessibility,
and command palette dispatch the same action ID.

Recommended baseline:

| Action | Default | Scope |
| --- | --- | --- |
| `audec.transport.toggle` | Space | Global except TextInput |
| `audec.transport.stop` | Shift-Space | Global |
| `audec.transport.record` | R | Global/editor when valid |
| `audec.edit.undo` / `redo` | Cmd-Z / Cmd-Shift-Z | Project |
| `audec.edit.delete` | Delete | focused editor selection |
| `audec.edit.duplicate` | Cmd-D | focused editor |
| `audec.edit.select_all` | Cmd-A | focused editor target |
| `audec.clip.split` | Cmd-E | Arrangement |
| `audec.clip.make_unique` | Cmd-U | Arrangement/pattern |
| `audec.loop.from_selection` | Cmd-L | time-capable editor |
| `audec.snap.toggle` | S | canvas context, not TextInput |
| `audec.tool.pointer/draw/knife/erase` | 1/2/3/4 | canvas context |
| `audec.view.fit` / `follow` | 0 / F | focused time editor |
| `audec.view.zoom_selection` | Z | focused time editor |
| `audec.editor.arrangement` | Cmd-1 | Workspace |
| `audec.editor.piano_roll` | Cmd-2 | Workspace |
| `audec.editor.drums` | Cmd-3 | Workspace |
| `audec.editor.automation` | Cmd-4 | Workspace |
| `audec.editor.mixer` | Cmd-5 | Workspace |
| `audec.palette.open` | Cmd-Shift-P | Global |

Move Waterfall/Rhythm/Components/Separation/Loom to palette/menu actions and
assign optional defaults after the editor keys. Preserve user-configurable
bindings by stable action ID. On macOS, expose application, File, Edit, View,
Transport, Track, Clip, Pattern, Analysis, Window, and Help menus from the same
registry.

Arrow behavior is contextual: arrange/piano/drum/automation canvases nudge the
selection or edit cursor; ruler focus moves the edit cursor; Browser changes
the result; Mixer changes focused channel/control. Explicit transport actions
use J/K/L or configured bindings rather than stealing arrows from editing.

## 14. Selection, edit cursor, and tool semantics

Evolve `session::Selection` into a UI selection snapshot with a primary object
and ordered domains while retaining its typed sets:

```rust
pub struct ProjectSelection {
    pub primary: Option<SelectableId>,
    pub time: Option<ProjectTimeRange>,
    pub tracks: BTreeSet<TrackId>,
    pub clips: BTreeSet<ClipId>,
    pub notes: BTreeSet<NoteId>,
    pub steps: BTreeSet<StepId>,
    pub automation_points: BTreeSet<AutomationPointId>,
    pub air: BTreeSet<AirSelectableId>,
}
```

Selection remains ephemeral and shared only through an explicit link facet.
The edit cursor is a separate exact time position used for paste/insert. The
playhead is transport state. A time selection may coexist with object
selection. Clicking empty canvas clears object selection but only clears time
selection when the gesture is a time-select action.

Default modifier contract on macOS:

- Shift: extend/toggle range or constrain a drag axis, context dependent.
- Cmd: toggle object membership; during drag temporarily suppress snap.
- Option: duplicate on move; slip inside audio; fine semantics are shown in
  status help before pointer-down.
- Control-click/right-click: context menu, never destructive editing.
- Escape: cancel gesture → dismiss transient UI → clear sub-selection → ascend.

Tools are editor-local view state persisted in `EditorViewState`, not project
state. Temporary tool overrides restore on key-up/focus loss. Pointer capture
must end safely when a native window loses focus; cancel previews if pointer-up
cannot be proven.

## 15. Context menus and direct manipulation fallback

Context menus are generated from the action registry using the semantic target
under the pointer, not ad hoc closures. They must be reachable from keyboard
(Menu key or Shift-F10) and announce disabled reasons.

Minimum menus:

- Clip: Open, Cut/Copy/Duplicate/Delete, Split at cursor, Make Unique, Loop,
  Reverse, Consolidate, Bounce in Place, Reveal Asset, Deprojection/Evidence,
  Color, Mute/Lock, Properties.
- Track header: Add/Rename/Duplicate/Delete, lane operations, arm/monitor,
  mute/solo, route, freeze, render stem, color/height.
- Note: Cut/Copy/Delete, duplicate, quantize, legato, humanize, transpose,
  velocity/expression, reveal source evidence.
- Step/pad: toggle/clear/fill, probability/ratchet/microtiming, choke, replace or
  reveal sample, deprojection evidence.
- Automation point/lane: interpolation, bind/rebind, thin/smooth/flip, convert
  or reveal measured feature, clear range.
- Mixer strip/slot/send: routing, insert/device, bypass/wet, latency, automation,
  remove, missing/crashed diagnostics.
- Workspace tab: Close, Close Others, Duplicate View, Split Right/Down, Float,
  Dock Back, Move to Main, Link Group, Reset View/Layout, Equalize.
- Browser item: Preview/Stop, Insert, Load in Sampler, Reveal, favorite/tag,
  relink, digest/provenance.

Every menu action must also exist in the command palette or Inspector. Tiny
canvas handles are accelerators, not the only way to perform an operation.

## 16. Project commands, preview state, and undo

`session::SessionCommand` and `mixer::MixerCommand` prove deterministic inverse
commands but are separate and not serializable. The DAW UI requires one
vocabulary under `ProjectDocument`, as specified in `DAW_ARCHITECTURE.md`.

Add three layers:

1. `ActionInvocation`: user intent plus focused semantic target and modifiers.
2. `EditGesture`: editor-local original snapshot, current preview patches,
   snap result, and validation diagnostics. Never persisted.
3. `ProjectCommand`: serializable, validated, atomic document mutation with
   inverse and `ChangeSet` used for targeted UI/audio invalidation.

The command dispatcher resolves an expected base revision, validates all
cross-domain effects, applies all or none, appends one history/journal record,
and publishes the revision. Structural audio impact schedules graph compile;
parameter-only impact uses bounded RT commands; view-only actions never dirty
the project.

Coalescing rules are explicit:

- one pointer/key-repeat gesture is one command;
- successive text edits coalesce until focus/target changes or a short timeout;
- a fader/knob drag is one command, while audible preview streams ephemerally;
- automation write from touch-down to release is one curve command;
- accepting deprojection is one transaction across assets, instruments,
  patterns, arrangement, mixer, AIR links, and workspace targeting;
- no-op previews create no revision.

The Undo menu uses exact labels (“Undo Move 3 Clips”, “Undo Accept Rhythm
Hypothesis”), and undo/redo never changes transport, focus, viewport, or current
selection unless the selected object ceased to exist, in which case selection
is pruned predictably.

## 17. Persistence and recovery

Persist project truth and workspace/view state separately but coordinate them
at save:

- `ProjectDocument`: arrangement, patterns, assets, instruments, automation,
  mixer, AIR, identity links, tempo map, stable ID counters, limitations.
- `ProjectSession` ephemeral state: transport mode is not saved as playing;
  playhead, loop/punch, selection, edit cursor, and audition state may be saved
  as reopen convenience but never enter undo history.
- `WorkspaceSnapshotDto`: main/floating placements and split tree.
- `WorkspaceItemDescriptor`: kind, target IDs, view parameters, tool, viewport,
  track/header sizing, collapsed lanes, link group, local analysis recipe.
- User preferences: theme, keymap, devices, recent paths, default tools,
  autosave policy; never embedded as song truth.

Extend `persistence::ProjectManifest` rather than replacing its atomic-write and
unknown-record behavior. `persistence::LensRecord` already has floating,
geometry, visible sample range, and opaque settings; migrate it into typed,
versioned editor descriptors while preserving unknown fields. Reuse
`workspace::WorkspaceSnapshotDto` for layout and placement but add dynamic item
descriptors and serialized allocation counters.

Autosave journals only committed `ProjectCommand`s plus periodic deterministic
document checkpoints. View/layout changes use a separately debounced atomic
workspace save. Crash recovery offers the newer project journal without
silently replacing the last explicit save. A missing asset, plugin, model, or
editor target opens a visible placeholder retaining all opaque state and a
relink/recover path.

## 18. Accessibility contract

Accessibility is part of editor architecture, not a late set of labels:

- Every transport/control/action has a stable accessible name, role, current
  value/state, shortcut, and disabled reason.
- Track headers, clips, notes, steps, automation points, mixer strips, and
  browser results expose ordered semantic collections independent of canvas
  pixels. The accessibility tree virtualizes offscreen items without claiming
  they are visible.
- Canvas selections announce count, primary item, time/value, and drag delta.
  A keyboard “properties” action reaches exact numeric entry for every
  pointer-editable value.
- Focus rings are high contrast and distinct from object selection. Tabbing
  visits regions and controls, not thousands of raw canvas marks; arrows navigate
  within the focused collection.
- Color never carries signal/inference/authored, selected, mute, clipping, or
  confidence meaning by itself. Shapes, labels, line styles, and icons reinforce
  it.
- Waveform/spectral plots provide textual time/frequency/value readouts and a
  movable keyboard probe. Audition actions have start/stop state and do not rely
  on visual animation.
- Resizing, reduced motion, increased contrast, system font scaling, and Retina
  physical-pixel rendering are test configurations.
- Context menus, toolbar actions, and drag-only functions have keyboard paths.

GPUI controls in current `src/ui.rs` are mostly styled `div()` elements. As
they are componentized, each must adopt semantic roles/labels and shared focus
behavior; copying the current clickable-div pattern into dozens of editor
controls would create an inaccessible surface.

## 19. Staged implementation and acceptance tests

Each stage ends in a shippable interaction loop. Do not begin a later editor by
adding mock UI bound to local state.

### Stage UI-0 — shared document, actions, and shell

Implement `ApplicationController`, `ProjectSession`, stable action registry,
theme tokens, global transport, and `WorkspaceRoot`. Import command-line/opened
audio into `ProjectDocument`; make it the sole arrangement truth. Mount Guise
with existing Track/Waterfall/Rhythm/Components/Separation/Loom entities.

Acceptance:

- Opening one file yields one `ProjectDocument`, one exact transport, and no
  second decode or editable arrangement copy.
- Menu, button, shortcut, and palette invoke the same Open/Play/Loop/Undo action
  IDs with correct enabled/checked states.
- Existing five lenses dock, tab, split, float, dock back, and reuse state.
- Closing/floating the overview cannot stop or duplicate transport.
- Save/reopen restores main/floating bounds, split ratios, tabs, active targets,
  viewport, and lens parameters; corrupt workspace state falls back safely.
- Existing forensic playback, loop, selection, HPSS, Loom, and spectral
  refinement tests remain green.

### Stage UI-1 — productive audio arrangement

Implement multitrack canvas, signed viewport, asset registry, audio clip
content/mapping, clip render graph, track headers, inspector, and project-wide
clip commands.

Acceptance:

- Import three samples, create/reorder tracks, drag assets from browser, move,
  trim, slip, split, duplicate, loop, reverse, fade/crossfade, mute/solo, and
  delete entirely by mouse and entirely by keyboard/action palette.
- Pointer-up commits one command; Escape cancels every drag; undo/redo restores
  byte-equivalent document state across crossfades and track moves.
- Horizontal pan never seeks; manual navigation disables follow; seek reveals
  playhead; zoom preserves pointer anchor; negative preroll is visible.
- Waveform/spectrum choose visible-range physical-pixel LOD after zoom and
  resize; no settled image is a scaled low-resolution whole-song bitmap.
- Playback and master WAV export use the same arrangement graph and null within
  tolerance under seeks and loop wraps.

### Stage UI-2 — browser, patterns, drums, and deprojection

Implement pattern library/instances, native Sampler/Drum Rack, step editor,
project browser, Loom/rhythm-to-step acceptance, explanation and residual
monitor buses.

Acceptance:

- Create a beat from browser samples in under one minute without opening a
  modal dialog; arrange linked pattern instances; Make Unique affects only the
  selected instance.
- Paint/toggle steps, velocity, probability, microtiming, ratchets, pitch, pan,
  and choke groups with deterministic undo and keyboard numeric fallback.
- Drag one recurring onset family into the editor, compare tempo/phase/template
  alternatives, accept it as normal production entities, edit the result, and
  hear source/explanation/residual continuously.
- Every accepted hit links to exact source evidence and remains labeled mixed
  or uncertain unless the user assigns an instrument identity.

### Stage UI-3 — piano roll and authored pitch

Implement beat/tempo model, note patterns, instrument scheduling, Basic Synth,
piano roll, note audition, quantize/humanize, and pitch deprojection.

Acceptance:

- Author, edit, loop, transpose, quantize, humanize, and render a polyphonic
  pattern with velocity and per-note expression.
- Linked patterns update every instance; Make Unique and per-instance overrides
  are visually unambiguous and undoable.
- Convert at least one AIR pitch trajectory through visible segmentation
  alternatives into notes, retain the measured trajectory as a ghost, and hear
  reconstruction/residual without relabeling observation as MIDI truth.
- Tempo edits move beat-anchored material but not sample-anchored audio; ruler,
  transport display, scheduling, and export agree at boundaries.

### Stage UI-4 — automation and mixer

Implement universal parameter addresses/descriptors, automation model/editor,
inline lanes, mixer surface, meters, active graph routing, modulators, freeze,
and stem export.

Acceptance:

- Bind and automate track, send, native instrument, plugin-placeholder, and AIR
  parameters from inspector/mixer/editor through identical addresses.
- Touch/latch/write gestures each create one appropriately thinned command;
  stepped and continuous mappings render and schedule correctly.
- Route buses/sends/sidechains, observe compensated latency and meters, solo/PFL,
  freeze/unfreeze, and export aligned stems that sum to master when mathematically
  possible; UI explains when nonlinear master processing prevents a null.
- Missing/crashed devices retain slots, state, parameters, automation, and a
  recover action.

### Stage UI-5 — forensic-production unification

Integrate masks/probes/annotations, dynamic analysis lens instances, link
groups, view forks, conversion of modulation/structure hypotheses, and richer
residual-guided editing.

Acceptance:

- Duplicate and A/B two Waterfall recipes; independently zoom/freeze; opt into
  linked time/selection but not frequency/settings; float/dock without reset.
- Select a time-frequency region, audition it and its complement, convert a
  measured feature into editable automation with backlinks, and compare the
  rendered explanation/residual.
- Every visual mark can expose evidence, producer/configuration, exact source
  range, uncertainty/alternatives, audition, and eligible deprojection actions.

### Cross-stage automated UI fixtures

Build deterministic GPUI interaction fixtures rather than relying only on
manual screenshots:

- action resolution for every key context, including TextInput suppression;
- hit-test zones at 1× and 2× scale and minimum sizes;
- pointer capture loss/Escape cancellation and one-command drag coalescing;
- marquee and modifier selection truth tables;
- scroll/zoom/follow invariants in every time editor;
- dock/float/dock-back identity and viewport retention;
- accessibility traversal and exact value entry;
- workspace/project golden snapshots, corrupt state, missing targets;
- screenshot goldens at 1024×700, 1440×900, and Retina scale for visual
  hierarchy, focus, selection, inference/authored distinction, and overflow.

## 20. Code map and critical risks

| Area | Reuse now | Extend or introduce | Principal risk |
| --- | --- | --- | --- |
| App root | `main.rs`, `ui::Workbench` startup | `app.rs`, `project_session.rs`, `workspace_ui.rs` | strong-reference cycles/reentrant GPUI updates |
| Docking | `workspace::{WorkspaceModel, create_pane_group_with_dots}`, `docs/WORKSPACE.md` | dynamic descriptors and runtime ID map | fixed built-in IDs cannot represent editor instances |
| Transport | `audio.rs`, `audio_host.rs`, 33 ms UI ticker | global transport controller/bar | duplicate `session::Transport` and seconds cache |
| Arrangement | `session::{Arrangement, Track, Lane, Clip, Selection}`, `timeline.rs`, current waveform/spectrum renderers | `ui/arrangement.rs`, richer clip/time/track model | UI currently renders analysis, not arrangement |
| Commands | `session::{ProjectChange, SessionCommand}`, `mixer::MixerCommand` | `commands.rs`, project-wide serializable dispatcher/history | split histories cannot commit cross-domain gestures |
| Browser/assets | `persistence::MaterialReference`, audition bus, retained PCM | asset registry/index and `ui/browser.rs` | blocking indexing, path-as-identity, preview transport theft |
| Patterns/drums | `loom.rs`, `rhythm.rs`, clusters/events | pattern/sampler models, `ui/drum_editor.rs` | conflating mixed event family with isolated instrument |
| Piano roll | `pitch.rs`, AIR pitch trajectories, lens beat timebase | note/pattern/tempo models, `ui/piano_roll.rs` | conflating measured pitch with authored note |
| Automation | AIR parameters/curves, mixer processor parameters | universal parameter registry/model, `ui/automation.rs` | unstable plugin IDs and incompatible parameter domains |
| Mixer | `mixer.rs` graph/commands/latency | document ownership, meters, `ui/mixer.rs`, compiled graph | UI controls ahead of real DSP/routing truth |
| Inspector | current `render_inspector`, AIR provenance/limitations | `ui/inspector.rs` contextual sections | hiding epistemic status or maintaining copied values |
| Analysis lenses | `ui::Visualizer`, `lens.rs`, spectral tiles, HPSS/Loom | persistent lens adapters and link groups | whole-source normalized view state and stale bitmaps |
| Persistence | `persistence.rs`, workspace JSON snapshots | full document records, editor descriptors, journal | ID migration and unknown-state loss |
| Accessibility | GPUI focus handles and actions | semantic components/accessibility adapters | styled clickable `div`s copied without roles |

The largest sequencing risk is building piano-roll, mixer, or clip widgets
before one authoritative `ProjectDocument` and command dispatcher exist. The
largest UX risk is copying LMMS's collection of separate windows without its
fast pattern workflow or audec's link semantics. The largest epistemic risk is
allowing inferred events, pitches, tempo, or sources to become ordinary DAW
objects without visible provenance and alternatives. The largest performance
risk is making editor repaint, asset indexing, analysis, graph compilation, or
metering touch the realtime thread.

The practical priority is therefore:

```text
one document/action path
→ real dock root with existing lenses
→ arrangement + browser + clip renderer
→ patterns/sampler/drums + residual
→ tempo/notes/piano roll
→ automation/mixer/graph
→ forensic-production editors everywhere
```

That sequence reaches the useful center of an LMMS-class workflow early, while
every subsequent production feature inherits audec's actual advantage: the
ability to show why a structure was proposed, turn it into something playable,
and keep what the reconstruction still fails to explain audible.
