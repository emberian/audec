# audec UX and workflow audit

Status: evidence-based audit of tracked `main` at `367a4ff` on 2026-08-31.
Incomplete parallel-agent working-tree changes were intentionally excluded;
they were not yet a stable, reachable application state.

> **Post-audit checkpoint:** the next integration slice addressed the largest
> ownership gap identified below. Opened material now creates one validated
> `LiveProject`; the arrangement, mixer, automation editor, media pool,
> transport, and audible render path share it, while the sequencer attaches to
> real project patterns when they exist. The findings below remain the record
> of the audited baseline and the outstanding workflow work, not a claim that
> every baseline defect is still present.

## Executive verdict

audec already has the beginnings of an excellent reverse-audio instrument: a
clear evidence atlas, independent viewports, resolution-aware source display,
honest language around inference, useful rhythm families, and unusually good
original/reconstruction/residual listening in Loom and HPSS. The screenshot in
`docs/screenshots/audec-arrangement.png` presents this material with strong
hierarchy and an information-dense but readable visual system.

It is not yet an 85%-of-LMMS production workflow. The visible application is
still organized as a source-audio analyzer with several DAW editor popouts,
not as one project whose arrangement, patterns, instruments, automation,
mixer, analysis claims, render, and residual all describe the same state.
This is an integration and interaction-model problem more than a shortage of
domain code: many capable foundations exist, but their ordinary launch paths
do not compose into a song-making loop.

The next product milestone should therefore be **one project, one transport,
one render, many placements**. New controls or analyzers should not outrun
that milestone.

## Audit scope and evidence

This audit inspected the actual tracked UI and project code, not just roadmap
claims:

- `src/ui.rs`: application shell, source transport, workbench atlas, lens
  popouts, rhythm, HPSS and Loom interaction;
- `src/workspace.rs` and `src/workspace_ui.rs`: Guise tab/split/tear-off state
  and native floating-window behavior;
- `src/arrangement_view.rs`, `src/sequencer_view.rs`,
  `src/control_views.rs`, and `src/asset_view.rs`: production editors;
- `src/daw_project.rs`, `src/daw_engine.rs`, `src/project_io.rs`,
  `src/reconstruction.rs`, `src/ontology.rs`, and `src/spectral_tiles.rs`:
  project, render, evidence, persistence, and resolution foundations;
- `README.md`, `docs/VISION.md`, `docs/DAW_ARCHITECTURE.md`, and
  `docs/LMMS_PARITY_ROADMAP.md`;
- the checked-in runtime screenshot
  `docs/screenshots/audec-arrangement.png` (1254 × 768).

“Current” below means reachable through the tracked application entry points,
not merely represented by a tested Rust type.

## Current-state findings

| Area | What is genuinely current | Evidence and consequence |
| --- | --- | --- |
| Source orientation | Strong. The screenshot shows a legible global transport, source identity, layered atlas, exact selection, loop state, playhead metrics, and a useful separation of material, display, and inspection. | `Workbench::render_header`, `render_sidebar`, `render_timeline`, and `render_inspector` in `src/ui.rs`; screenshot. |
| Time navigation | Substantially repaired. The atlas and lenses have local pan/zoom/fit/follow behavior, and the atlas requests fresh pixel-aware spectral detail for its visible sample range. | `Workbench::refresh_spectrogram_detail` and `TimelineViewport` usage in `src/ui.rs`; `src/spectral_tiles.rs`. |
| Spectral parameterization | Partly current. The Waterfall exposes FFT size, window, frequency range, dB ceiling and dB range. The typed settings/lens models are richer than the visible controls. | `Visualizer::rerun_spectrum` and `render_waterfall` in `src/ui.rs`; `src/settings.rs`; `src/lens.rs`. |
| Rhythm deprojection | Strong provisional inference. It runs asynchronously, shows ranked pulse alternatives, phase/meter/pattern counts, exact hit spans, anonymous families, cohesion, and medoid audition without pretending families are isolated instruments. | `Visualizer::refresh_rhythm` and `render_rhythm` in `src/ui.rs`; `src/rhythm.rs`. |
| Reconstruction listening | Strong vertical slices. HPSS offers mix/sustained/transient/null audition. Loom exposes editable family/event gain, timing and enablement plus mix/render/residual/template audition and fit metrics. | `render_separation`, `render_loom`, and `rebuild_loom_audio` in `src/ui.rs`. |
| Arrangement editor | A real command-backed editor exists and the loaded source is instantiated as a real audio clip. It supports selection, nudge, trim, split, duplicate, delete, track creation, snap, pan/zoom, and provenance-oriented inspection. | `Workbench::open_arrangement_editor` in `src/ui.rs`; `src/arrangement_view.rs`. |
| Pattern editors | Capable component, wrong ordinary ownership. The piano/step view can add, move, resize and delete events, quantize, swing, pan/zoom and undo, but `Workbench::open_sequencer_editor` launches `SequencerEditor::demo`. | `src/sequencer_view.rs`; `src/ui.rs:899-908`. |
| Mixer and automation | Capable component, wrong ordinary ownership. Shared backends now exist, but the application still launches `MixerView::demo` and `AutomationView::demo`. Automation write modes explicitly report that transport writer hookup is pending; mixer meters/plugins are not audible DSP. | `src/control_views.rs`; `src/ui.rs:910-929`. |
| Asset pool | The loaded FLAC becomes a real fingerprinted registry asset with metadata, provenance, usage, search/filter/sort, favorite, audition and activation. It is still a popout and not an import/drag/relink production browser. | `Workbench::install_source_asset`, `open_assets`, and `src/asset_view.rs`. |
| Workspace | Real Guise tab/split/tear-off machinery exists for six singleton analysis views and preserves an entity across dock/floating placement. The screenshot shows all six in one tab row. | `BuiltinView::ALL` in `src/workspace.rs`; `create_workspace` in `src/ui.rs`; screenshot. |
| Workspace integration | Incomplete. Arrangement, sequencer, mixer, automation, assets and inspectors are not workspace descriptors. Sidebar actions open separate native windows. The `+` tab request and context-menu events have no application hook because `create_workspace` installs `WorkspaceHooks::default()`. Snapshot updates are therefore discarded by the application. | `src/ui.rs:4907-4956`; `src/workspace_ui.rs`. |
| Project authority | Not current. `Workbench` owns source analysis, raw-source `AudioHost`, local editor entities, asset registry, selection and loop. `DawProject` is not the UI’s authoritative mutable object. | `Workbench` fields and `install_analysis` in `src/ui.rs`; `src/daw_project.rs`. |
| Audible construction | Core only. `compile_daw_engine` can freeze a validated aggregate project and render clips, automation, routing, sends and explicitly routed built-in instruments, but the workbench opens `AudioHost` directly on retained source PCM. Visible editor edits therefore do not change the main transport. | `src/daw_engine.rs`; `Workbench::install_analysis` in `src/ui.rs`. |
| Save, recovery and export | Core only. Portable envelopes, autosave/recovery contracts, deterministic rendering and WAV encoding exist, but there are no project Save/Open/Recover/Export application actions or dialogs. | `src/project_io.rs`; `src/render.rs`; absence from `src/ui.rs`. |
| Evidence ontology | Rich core, nearly invisible workflow. AIR can represent evidence, alternatives, transforms, provenance, pitch/modulation and authored/inferred distinctions, but the workbench inspector is a fixed playhead metric list and the arrangement inspector is clip metadata rather than a navigable claim/evidence graph. | `src/ontology.rs`; `src/reconstruction.rs`; inspectors in `src/ui.rs` and `src/arrangement_view.rs`. |
| Legacy flexibility | Modeled but not restored. Scope zero-crossing/amplitude controls, vectorscope fade/brightness, capture/trigger policies, channel projections, timebases and other lens parameters exist in typed modules but are not first-class UI lenses or parameter inspectors. | `src/settings.rs`; `src/lens.rs`; six-item `BuiltinView`. |

## What the screenshot says about the product

The screenshot is a good representation of audec’s current strength. The
center canvas gives most pixels to the sound, the left column makes source and
lens choices obvious, and the right column answers “what is happening here?”
without covering evidence. Color usage is consistent enough to build a durable
visual grammar.

It also makes the workflow ceiling visible:

- The top tab row contains only the six analysis singletons; all production
  editors are buttons in a separate “EDIT / RECONSTRUCT” launcher section.
- The atlas is a fixed stack of analysis lanes rather than a project
  arrangement. The “LAYERS” rows look like toggles but are rendered from
  hard-coded `active: true` values and have no interaction.
- The transport is expressed in elapsed time over the source. There is no
  authored tempo/meter, bar/beat position, metronome, record, project dirty
  state, render state, or engine diagnostic in the global shell.
- The inspector reports a 125.0 BPM pulse hypothesis while the arrangement
  editor initializes its grid to 120 BPM independently. There is no visible
  “adopt this candidate” action, so inference and authored musical time can
  silently disagree.
- There is no visible path from a rhythm family, HPSS estimate, NMF component,
  or Loom template into a track, pad, pattern, bus, or persistent claim.

The visual design should be preserved. The information architecture beneath
it should change.

## Top 20 workflow friction points

Impact is the cost to completing a real musical/reconstruction task. Effort is
relative architectural effort: **S** is a localized UI/backend change, **M**
crosses a few existing modules, **L** establishes a product-wide integration,
and **XL** changes the live audio/project contract.

| Rank | Friction | Impact | Effort | Concrete remedy / completion evidence |
| ---: | --- | :---: | :---: | --- |
| 1 | Editor edits do not change what the global transport plays. Playback is retained source PCM, not the compiled aggregate project. | Critical | XL | Make a project controller publish immutable `DawEngineSchedule`s to one online/offline render path; source-only playback becomes an explicit monitor mode. |
| 2 | There is no single authoritative live project or cross-domain transaction history. Workbench, arrangement, sequencer, mixer, automation and assets can diverge. | Critical | L | One controller owns project domains, selection, revision, dirty state and transaction journal; views keep only target IDs and ephemeral gestures. |
| 3 | Piano/steps, mixer and automation launch demo data during a real source session. This makes apparently successful edits semantically false. | Critical | M | Inject project-owned sequencer/mixer/automation backends and remove demo launch paths from normal use; show an honest empty/create state instead. |
| 4 | Save, reopen, autosave recovery and export have no application workflow. A user cannot safely invest in a project. | Critical | L | Add File actions, recent/recovery surfaces, full-domain codecs, save-state indication and master/selection/stem export UI. Pass a quit/reopen/relink test. |
| 5 | Inference cannot be promoted into ordinary construction. Rhythm families have no “deproject to pattern”; HPSS/NMF/Loom results have no “place/route as project source.” | Critical | L | Add explicit, reversible proposal application that creates project objects, bindings and evidence links while retaining alternatives and microtiming. |
| 6 | Analysis and production use two workspace systems: six dockable analysis singletons versus undocked editor windows. | High | M | Replace `BuiltinView` with descriptor/instance IDs covering editors, browsers, inspectors and targeted duplicate lenses; use one placement system for dock and native windows. |
| 7 | Original, reconstruction and residual are local lens buffers, not durable project comparison objects. Closing/reanalyzing loses the explanatory experiment. | High | L | Persist a comparison scope with source revision, proposal revision, render fingerprint, residual metrics and A/B routing; expose it anywhere the proposal appears. |
| 8 | Arrangement editing is mostly toolbar/keyboard nudge operations. There is no drag move/trim/slip/fade, marquee, vertical track management, clip waveform, crossfade or loop-handle workflow. | High | L | Implement direct manipulation with transient previews and one committed command per gesture; render source-aware waveform proxies and explicit handles. |
| 9 | There is no usable pattern lifecycle: create from project, place/repeat clips, double-click to edit, make unique, map samples/instruments, then return to arrangement. | High | L | Add project pattern browser/instances, selected-pattern binding, make-unique, repeat handles, editor descent and target routing. |
| 10 | Musical time has multiple unowned truths. The atlas can infer 125 BPM, arrangement starts at 120 BPM, and the demo sequencer owns another tempo map. | High | M | Put tempo/meter in the project shell; show inference as candidates; require explicit adoption; use the resulting map for every ruler, snap and render. |
| 11 | Built-in instruments and the asset audition bus exist, but there is no normal pad/note audition, instrument assignment or track-to-bus workflow. | High | L | Provide instrument slots/routing in track and pattern inspectors, note/pad preview, and deterministic online/offline scheduling with visible unroutable-event diagnostics. |
| 12 | Mixer and automation controls are not connected to audible DSP; meters and write modes cannot prove their effect. | High | XL | Bind controls to one parameter registry and schedule, display real post-DSP meters/graph rebuild state, and verify sample-offset automation parity in bounce. |
| 13 | Asset pool activation only opens the arrangement; users cannot drag/drop into a track or pad, import more media, preview a selected span, or repair missing files. | High | M | Dock the pool; add import/drop payloads, waveform/span audition, “reveal usages,” deterministic relink and drag-to-arrangement/pad affordances. |
| 14 | Workspace layout changes are not persisted by the app: hooks are default no-ops, while the `+` and context-menu events have no product behavior. | High | M | Store workspace snapshots in the project/user layout, implement add-view and tab context menus, restore bounds/docking/link groups, and surface recovery from invalid layouts. |
| 15 | Views share playback but not a first-class selection/aspect. A range or object chosen in one pane does not reliably target another pane’s analysis or editor. | High | L | Add typed shared Aspect/Selection state with independent link facets for transport, time range, frequency, object and parameters; let each placement opt in/out. |
| 16 | Loom edits “the selected cluster and its event nearest the shared playhead,” which is efficient for a demo but imprecise for editing a sequence; there is no direct event selection, multiselect or undo/revert. | Medium-high | M | Make plotted events selectable/editable directly, add history and original ghost timing, and support reject/revert of an applied proposal. |
| 17 | The atlas “LAYERS” controls are visual labels, not controls, and many legacy lens parameters remain model-only. | Medium | M | Make layer visibility/order/height configurable; add a typed parameter inspector with effect labels (presentation/projection/analysis/audio) and presets. |
| 18 | Resolution honesty is uneven. The atlas requests visible-range spectral tiles, but the standalone Waterfall reruns a whole-source projection then crops/scales that image for time zoom. | Medium-high | M | Put every spectral pane on the same cancellable tile/cache request API keyed by source, range, recipe and physical pixels; retain coarse tiles only as visibly refining placeholders. |
| 19 | Commands are hard to discover and focus semantics are fragile across docked and native windows. There are shortcut tables in README but no menus, command palette, shortcut help or consistent context actions. | Medium | M | Build one action registry with menus/palette, focused-target routing, searchable shortcuts and status feedback; test commands in every placement/focus state. |
| 20 | Engine limitations, dirty/revision state, background analysis, cache refinement, missing assets/models/plugins and recoverability are not unified into a persistent status surface. | Medium | M | Add a project health/activity center and nonmodal status strip with actionable diagnostics, cancellation, logs and provenance links. |

## Proposed cohesive workspace model

### Ownership

```text
Application
└── ProjectController (one per open .audec document)
    ├── Project domains
    │   ├── arrangement + tempo/meter + patterns + instruments
    │   ├── automation + mixer + assets
    │   └── AIR claims/evidence/provenance + reconstruction branches
    ├── Project services
    │   ├── transaction journal + save/autosave/recovery
    │   ├── immutable render compiler + online/offline engine
    │   ├── analysis/model job registry + artifact cache
    │   └── audition/comparison router
    ├── Shared interaction state
    │   ├── transport, loop, punch and authored musical time
    │   ├── Aspect/Selection and named link groups
    │   └── command context + project diagnostics
    └── Workspace model
        └── ViewPlacement instances (dock tab, split or native window)
            └── target IDs + local viewport + local parameters + UI caches
```

No window or view owns project truth. A pane may hold a snapshot for rendering,
but commands return to the controller and publish one validated revision. A
native window is a placement of the same view model, not a parallel editor.

### View identity

Replace the six-value singleton roster with two identities:

```text
ViewDescriptorId: arrangement | piano-roll | steps | mixer | automation
                  | assets | inspector | atlas | waterfall | rhythm | ...

ViewInstanceId: a stable project/workspace ID for one targeted placement
```

The instance stores a target such as a pattern ID, bus ID, claim ID, source
Aspect, or comparison branch. This permits two Waterfalls with different FFT
recipes, two piano rolls on different patterns, or an evidence inspector beside
the clip created from that evidence. Docking, floating and closing change only
placement.

### Default workspace

Keep the screenshot’s excellent density, but make the default layout a working
desk rather than a tab inventory:

```text
┌ global project/transport/tempo/loop/render/status bar ───────────────────┐
├ browser stack ┬ main editor stack ─────────────────────┬ inspector stack ┤
│ Assets        │ Arrangement                             │ Selection       │
│ Patterns      │ tabs: Atlas / Piano / Steps / Auto     │ Evidence        │
│ Claims/jobs   │ split-bottom: Waterfall / Rhythm / Loom │ Routing/Params  │
├───────────────┴─────────────────────────────────────────┴─────────────────┤
│ dockable Mixer / comparison strip: Original | Construction | Residual    │
└───────────────────────────────────────────────────────────────────────────┘
```

This is a default, not a fixed layout. Every region can tab, split, float or
close. The primary arrangement is no longer artificially pinned; closing its
placement does not close the project. A compact “return to default desk” action
is always available.

### Interaction contracts

1. **Selection makes a target; commands make state.** Clicking a rhythm family
   selects a typed inference and its source spans. “Deproject to pattern” is an
   explicit command that previews then commits ordinary pattern/track objects.
2. **Inference and authorship never silently merge.** Candidate tempo, pitch,
   family, source, modulation and effect claims remain alternatives. Adoption
   creates an authored choice linked back to the exact claim and transform.
3. **Every accepted explanation gets a comparison.** The controller can route
   original, current construction and residual for the same exact frame range,
   with gain-matched switching and stored metrics.
4. **Viewport facets are independently linkable.** Transport and selection are
   linked by default; time range, frequency range, parameters and cursor can be
   local or assigned to named groups. Manual navigation disables follow only
   for that placement.
5. **Every operation has a visible project consequence.** Adding a step changes
   the arrangement render; moving a mixer fader changes playback and bounce;
   changing an analysis recipe creates/refines evidence but does not dirty the
   authored mix unless a claim depending on it is adopted.

## Explicit workflow acceptance tests

These tests are intentionally end-to-end. Model tests remain necessary, but a
gate passes only through the shipped application on a fresh user profile.

### A. One authoritative project

**Given** a new project with one imported FLAC, **when** the user creates an
audio track, one note pattern, one step pattern, a mixer route and an automation
lane, **then** every open view reports the same project revision and targets the
same stable IDs. Editing in one placement updates another placement without
reopening it. No visible editor contains demo objects.

### B. Source import and audio arrangement

**Given** FLAC, WAV and AIFF fixtures, **when** each is imported, dragged from
the asset pool into an audio track, split, trimmed, slipped, faded, duplicated
and looped, **then** the asset appears once by content identity, clip waveforms
and source ranges remain sample-consistent, and undo/redo restores exact ranges.
The live monitor and offline render match at declared sample checkpoints.

### C. Drum pattern to audible arrangement

**Given** four one-shot samples, **when** the user maps them to pads, programs
velocity, probability, ratchets and microtiming, places and repeats the pattern
in the arrangement, and routes it to a mixer bus, **then** transport and bounce
produce the same seeded event schedule and audible PCM. Every silent/unroutable
event has a visible diagnostic.

### D. Notes and owned instrument

**Given** an empty note pattern and the built-in synth or sampler, **when** the
user draws, moves and resizes notes, edits velocity/expression, quantizes and
humanizes, then duplicates the arrangement instance, **then** both editors
reference one pattern definition until Make Unique is invoked. Note audition,
live playback and deterministic bounce all use the assigned instrument.

### E. Authored musical time from inferred evidence

**Given** a recording with ranked 62.5/125 BPM candidates, **when** the user
previews and explicitly adopts 125 BPM with a meter/phase choice, **then** the
global shell, arrangement, piano/step rulers, snap, metronome and render share
one tempo map. The discarded/alternate candidates remain inspectable, and undo
restores the prior authored map without deleting evidence.

### F. Project automation and mixing

**Given** eight tracks, one group and one return, **when** the user routes
tracks, changes gain/pan/mute/solo/sends and automates a send plus an instrument
parameter across a tempo change, **then** real meters respond, audible playback
changes immediately, and offline render receives identical parameter events at
the expected sample offsets with latency/tail accounting.

### G. Safe save, reopen, recovery and relink

**Given** a dirty project with docked and floating panes, analysis claims,
patterns, automation, mixer state and one asset, **when** the user saves, quits,
moves the asset and reopens, **then** all project identities and workspace
placements restore, the asset is visibly missing, content-identity relink repairs
it, and playback/render becomes valid without duplicating the asset. Repeating
the test after an interrupted autosave offers a clearly labeled recoverable copy.

### H. Workspace and multiwindow behavior

**Given** Arrangement, Rhythm, Mixer and two differently parameterized
Waterfalls, **when** the user splits, tabs, tears off, resizes, closes, docks
back and restarts, **then** each targeted view retains selection, viewport,
parameters and native bounds. Closing the main arrangement placement does not
stop transport or destroy the project. The `+` action can create every
descriptor and tab context actions work.

### I. Resolution-honest spectral navigation

**Given** a six-minute source, **when** a Waterfall is zoomed from whole-song to
one second and its native window is doubled in physical width, **then** a new
tile request is made for the exact range and pixel LOD, the UI visibly reports
coarse/refining/final state, cancellation prevents stale completion from
winning, and the final raster is not an enlarged crop of the whole-song image.

### J. Deproject a mixed beat

**Given** an electronic mix with overlapping transients, **when** rhythm
analysis completes, **then** the user can compare tempo/phase/meter alternatives,
audition each anonymous family’s exact source examples, select occurrences,
and preview “Deproject to pattern.” Accept creates ordinary editable lanes with
original microtiming, confidence/evidence links and an immediate reject/revert
path; no instrument name is asserted without separate evidence.

### K. Test an explanation by ear

**Given** a Loom, HPSS, NMF or optional-model proposal over a selected span,
**when** it is placed/routed into the project and edited, **then** one gain-matched
comparison strip switches among exact original, construction and residual.
Residual is recomputed from the same source and project revisions, metrics and
render fingerprints are stored, and every generated object navigates back to
source frames, transform settings, implementation/model provenance and caveats.

### L. Signal, inference and authorship grammar

**Given** a measured onset, a family hypothesis and an accepted authored step
at the same time, **when** they appear in atlas, arrangement, inspector and
saved/reopened project, **then** they retain three distinct visual/semantic
classes. Confidence belongs only to the inference; edit state belongs to the
authored object; the measurement remains immutable. Rejecting the hypothesis
does not delete the measurement or authored work without an explicit command.

### M. Failure remains useful

**Given** a missing asset, missing optional model, missing/crashed plugin and a
cancelled analysis job, **when** the project is opened, **then** it remains
inspectable and saveable, placeholders preserve identities/state, no untrusted
code executes during open, and the health surface offers specific repair or
retry actions. Supported material continues to play and export with explicit
diagnostics rather than a plausible silent fallback.

## Recommended delivery order

1. Establish the project controller, shared selection/transport, revision and
   injected editor backends; delete normal demo launch paths.
2. Make the aggregate engine the audible online/offline source and expose its
   diagnostics; then add Save/Open/Recover/Export.
3. Generalize the workspace to view descriptor/instance IDs and dock all DAW
   editors, browsers, inspectors and comparison strips; persist the layout.
4. Finish the ordinary pattern/instrument/mixer/automation loop until a small
   electronic track can be completed and reopened.
5. Add reversible proposal application and persistent original/construction/
   residual comparison. This is the point where audec becomes a reverse DAW
   rather than a DAW-shaped analyzer.
6. Restore the old lens flexibility through the shared parameter inspector,
   then layer optional ML workers and plugin hosting onto the already coherent
   evidence/project/render contracts.

The design criterion remains concise: a user should be able to point at any
sound, mark, clip or control and answer **what is it, what evidence supports
it, what project object realizes it, what do I hear if I change it, and what
remains unexplained?**
