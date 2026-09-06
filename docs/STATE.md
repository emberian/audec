# audec: state of the tree

Written 2026-09-01 by Claude Fable 5.1 after taking ownership of the tree
with ember. This file is the current, verified state and the working
program. It supersedes the campaign narratives now in `docs/archive/`
(`GROKOUT.md`, `FORGROK.md`, `SWARM_CYCLES.md`, `NEXT_CAMPAIGN.md`, and
the rest) as the place to start; those remain as history. The tree wins over any prose.

## What audec is, measured

- Rust + GPUI desktop app. `src/` is ~282K lines in 209 files; the desktop
  app reaches ~236K of them. 1,373 library tests.
- Seven editable domains behind one command envelope
  (`daw_project::ProjectDomains`: arrangement, sequencer, automation,
  assets, mixer, sample_kits, air). `ProjectSession` is the single owner
  of a live project; `CommandEnvelope` is the only durable edit path.
- One audio truth: a frozen `DawEngineSchedule` lowers to a `CompiledGraph`;
  whole bounces and render tiles are partitions of the same kernel.
  Playback, audition, and export consume the same product.
- The shell (`src/ui.rs` and `src/ui/*`) hosts one `Workbench` (overview,
  analysis, transport, project io) inside a `DawWorkspace` (explorer,
  inspector, palette, action registry) over a persistent dock/tab
  workspace document. Five analysis lenses share one `Visualizer` type.

## What is verified on the desktop, not only headless

The app can be driven from outside through the control socket:

    AUDEC_CONTROL_SOCKET=$TMPDIR/audec-control.sock target/debug/audec material.flac

Newline-delimited JSON. Verbs: `status`, `actions`, `action {id}` (any
palette action id, e.g. `audec.sample.make_beat`, `audec.loop.toggle`),
`open {path}`, `seek {sample|seconds}`, `select {start,end}`, `click
{sample}`, `drag {start,end,alt}`, `loop {start,end,enabled}|{clear}`,
`play`/`pause`/`stop`, `export {path}`, `objects`, `lens {view, control}`, `quit`;
`status.lenses` reports each analysis lens's transform and whether it is computing. Every request
is answered on the main thread through the same authorities the palette
uses (`ExternalProtocol` origin), so nothing succeeds here that the UI
would refuse. `src/control_socket.rs` is toolkit-free; the host half is
`src/ui/shell_control.rs`.

Verified live on *Like a Pen* (44.1 kHz stereo, 6:13) on 2026-09-01:

- Overview loop state machine through the real pointer kernel: a drag
  with no loop selects only; loop-from-selection enables it; a drag while
  a loop is active replaces the loop and locates; a click inside an
  active loop seeks and keeps it; a click outside disables the loop and
  keeps its bounds; a drag while the loop is disabled selects only;
  alt-drag authors a loop; toggle re-enables the kept bounds.
- Make beat from a selection creates kit, pads, pattern, occurrence, and
  routes at one revision, the master renders through the native graph
  without an audio error, export completes, and the exported master
  differs from the pre-beat master exactly at the selection.

Screenshots from a scripted session need Screen Recording permission for
the terminal; without it, audio export plus `sox`/`numpy` is the eye.

## Bugs found and fixed by driving the app (2026-09-01)

1. Any project with an instrument could not compile audio on the desktop
   (`plan tileability Stateless does not cover native graph requirement
   BoundedHistory { lookbehind_frames: <whole extent> }`); playback kept the
   previous revision. Instrument nodes now declare their longest voice as
   lookbehind and the controller tightens the plan to the compiled
   graph's requirement (`Tileability::covering`). Regression:
   `cycle11_flow::made_beat_renders_audibly_through_the_native_controller_path`.
2. Export right after an edit answered "FILE ERROR · … not compiled yet";
   it now queues behind the render.
3. Play (and drag-to-loop locate) requested before the first bounce
   opened the audio host were dropped; the host now restores loop,
   playhead, and playback mode from the timeline kernel when it opens.
4. Make beat placed its pattern at bar 1; it now lands on the beat where
   the selected material sounds (`cycle11_flow::make_beat_places_the_pattern_at_the_selection_not_bar_one`).
5. The overview range gesture had been reverted to never replace an
   active loop, contradicting the musician gate; restored.
6. Saving any project with retained analysis failed on the AIR codec's
   lossless check (f32 fields re-parse as f64); autosave showed it as a
   FILE ERROR banner every 30 s. The check now compares numbers after
   f32 narrowing and still refuses dropped keys.
7. Opening an analysis pane by action panicked ("cannot read Workbench
   while it is already being updated"); lenses created inside the
   Workbench update are now seeded from `&self` with a deferred first
   refresh.
8. Float/dock, next, previous from the menu or shortcut answered "no
   application adapter" because the surface registered different id
   strings from the product intents; routed.
9. Opening the Arrangement or Sampler editor from inside the main window
   failed with "native workspace focus_main_window: window not found"
   (a nested update of the window being dispatched); activation is now
   deferred past the current update.

Why headless missed 1 and 3: every headless render used
`DawEngineSchedule::render_for_audition`, never `ProjectAudioController`,
and no test opened an audio host. Both now have controller-path tests,
and the live scenarios in the scratch harness are the real gate.

A fresh-eyes review of the night's commits (2026-09-02, 02:00) found and
fixed: stale kernel playhead on host-open replay (seeks bypassed the
kernel; a pre-host loop was disabled on open), pre-host selection not
restored, Make beat placement snapping to a tick instead of the bar,
mute/solo ignored when locating material, "+ KIT" retargeting the current
pane, constant-Q field 12 dB hotter than the FFT ceiling, a silently
refused constant-Q showing FFT under a CQT label, Blackman mislabelled as
Blackman-Harris in CQT mode, remembered preferences overwritten on
material load, the socket `lens` verb driving hidden controls on
non-waterfall lenses, sub-pixel gesture tiles, NMFD never exiting early,
and live scripts that could not report a failed launch.

## Known holes (musician-facing)

- A `like-a-pen.audec` package appeared in the process working directory
  during a scripted run: the scenario killed the app with unsaved changes,
  the close guard chose Save, no package root existed, and `save_as`
  prompted for a path with the directory defaulting to `.`; under the
  scripted platform that prompt resolves without a human. On a real
  desktop it is a dialog. Save As and Export now default next to the
  package, else the material, else Documents, never the launch directory.
- Floating a pane into a native window works: activating a dynamic tab and
  invoking float/dock raises the window count to 2 and docking returns it
  to 1 (socket-verified). The pinned main tab refuses to float by design.
- Opening the Arrangement, Mixer, Sampler, Assets, and Automation editors
  by action works (automation opens empty and creates its first lane);
  piano roll and drums refuse with an actionable message when the project
  has no pattern yet.
- Refusals and receipts are visible: the toolbar's project row renders the
  notice channel and audio errors (they were written to a field only the
  hidden sidebar drew).
- Lag reported by ember on a debug build while playing. A socket probe
  (status round trip and playhead advance sampled every 100 ms during
  loop playback of a fresh project) shows steady advance in both debug
  and release, round trips of 20 to 35 ms, and about 80 percent of one
  core in both builds. So the redraw at the 30 Hz transport tick is not
  free, but the probe did not reproduce a stall; the lag likely needs
  specific panes or the analysis workers running. Profile with editors
  open and components analysis in flight.
- ~2,000 dead-code warnings (485 structs never constructed, 467 functions
  never called): vocabulary-wave scaffolding. Purge is the next commit.
- The Grok-era hole table in `docs/archive/GROKOUT.md` is still accurate
  for: Loom sketch edits pane-local until Make pattern, comparison
  products pane-local, reading import not filling Explorer Readings.
  Sampler `+ KIT` / `+ PAD` now create (2026-09-02).

## How to work here

- One writer at a time in `src/ui.rs`, `src/live_project.rs`,
  `src/daw_project.rs`. The `src/ui/*` concern files can be owned per
  lane.
- Filter tests by mounted module path (`timeline::`, not
  `timeline_interaction`); check the `running N tests` line. The musician
  gate: `cargo test --lib -- 'cycle11_flow::' 'musician_gate::'
  'engine_regression::' --test-threads=1`.
- After any change to audio, transport, creation, or workspace flows, run
  the live scenarios in `scripts/live/` against real material and read
  what the app reports; export and compare audio if audibility is the
  claim.
- Never `git add -A`, never stash, commit messages via `git commit -F`.
  A stale incremental linker mix (`_anon…llvm` symbols not found) after an
  interrupted build is cured by `rm -rf target/debug/incremental/audec-*`.

## Landed 2026-09-06: cycle 3, wave 2 (Collapse-A, Collapse-B; Inserts in flight)

- **Collapse-A**: the four per-lens generation counters are `Freshness`
  values behind the shell's one epoch vocabulary (`Authority::Lens`), so a
  dropped lens result prints a `Stale` naming the lens and both epochs
  instead of returning silently; rhythm's material check is a document
  fact under `Authority::Document`. `SampleViewOutcome` is gone: one
  `SampleActionOutcome` with an honest manual `PartialEq` on
  `ConstructiveOutcome` (publication, operation, journal sequence; never
  the snapshot); the three acknowledgement sentences live with the
  feedback that renders them.
- **Collapse-B**: one `EditorTarget` (the durable one); the runtime catalog
  nobody ever built is deleted (521 → 47 lines) and the lossy
  `Render | Extension => Project` bridge with it; the deprojection bridge
  resolves `RevealRequest`s, with `resolve_selected(view)` for the deferred
  case, and its lookup errors are `RevealRefusal`s.
- **Review of wave 1** (fifteen findings, all fixed on main): the active-pane
  mirror re-seeds after a new document; a vanished floating window
  activates the main window's pane; the audible diff respects a disabled
  loop, never installs a whole-project loop, reports "nothing outside the
  span" as absent, and reuses its measurement; the residual guide asks in
  comparison terms (no fake proposal ids) and never replaces a pane's
  unsaved query; coverage provenance is per comparison (idempotent
  re-measure); one loaded revision per reading; multi-file loads keep every
  refusal; leftward nudges and end trims step by the cell that ends at the
  anchor; Beat snap counts the meter's beats from the bar; the ruler walks
  the map's bars and beats (7/8 works); promotion refuses unrendered curve
  targets at validation; the reconstruction fade neither pre-empts the
  picker's descriptor nor stacks lanes.

## Landed 2026-09-06: cycle 3, wave 1

- **Null** (`audec.transport.audition_diff`): the render runtime keeps the
  cohort each publication retires; the controller subtracts new minus old
  over the loop (bitwise through `render_comparison`, refusing by name on
  mismatch or with no previous render) and auditions it through the scoped
  audition path; `status.diff` reports the null's RMS inside and outside the
  span. Live on *Like a Pen*: 0.2116 inside the loop vs sox 0.1871 from the
  two exports (the export clamps at ±1.0 where the post-beat master clips),
  0.0084 outside vs 0.0079. The audible diff found that make beat lands its
  pattern on the bar line *before* the loop when the loop start is not on a
  bar (one second of null at 59–60 s); see the follow-ups.
- **Automation**: the address vocabulary says what the renderer reads
  (Decomposition, PerceptualLens, AirParameter and six ClipParameters
  deleted, one commit each; decoding a persisted address of a deleted kind
  is a named codec refusal); `create_lane` and the `CreateLane` lowering
  refuse an unrendered address; the reconstruction fade rides the clip-gain
  read; `engine_regression` hears a clip-gain fade (0.199 → 0.006 RMS
  halves) and removing the lane restores the bytes.
- **Time**: `audec.tempo.mark_at_playhead` and `audec.meter.cycle_at_playhead`
  place a tempo point / cycle the meter at the playhead's bar as one
  undoable `SetTempoMap`; ± tempo edits the segment under the playhead; the
  transport bar reads at the playhead; the arrangement grid snaps through
  the map (scalars deleted); `RouteTrackToBus` moves a track onto a free
  channel (a track owns its bus; master, returns, held and repeated
  destinations are refused by name); `+ Auto` deleted (it made a track no
  clip could fill and no renderer read). Live: per-bus exports show the
  beat's bus carrying energy only inside the loop, exactly zero outside.
- **Shell**: `audec.workspace.close` is offered exactly where the workspace
  accepts it (one `is_pinned` predicate behind the projection, the tab row
  and both refusal sites; "The overview stays open"); the layout's focused
  pane is what "active" means and the shell hears it once, so opening an
  editor by action activates its pane and the tab verbs move `active_view`;
  one action catalog (44 ids, one constructor); the two live scripts stop
  brute-forcing. Found on the way: `next_pane` is a silent no-op with the
  shipped single-dock-pane window, nothing is active at startup, and
  `status.active_view` lags one action (follow-ups).
- **Reverse**: a measured comparison is retained with its coverage artifact
  and its residual guide (the reading query pane shows hotspots with
  audition buttons); readings export from the project and import verified
  against the material (`reading_export` / `reading_import` socket verbs;
  refusals verbatim: manifest mismatch, other material); the Explorer lists
  loaded readings. Found on the way: serde_json's default float parser is not
  correctly rounded, so a reading could never verify its own manifest digest
  (fixed with `float_roundtrip`, pinned by an acceptance test). The Compare
  branch is still empty live: the reverse flow has no pane-less host path.
- `status` now carries `musical_time` (bpm, meter, bar at the playhead) and
  `diff`; the socket has `reading_import` / `reading_export`.

## Landed 2026-09-04: cycle 2

- **Reveal**: one `RevealRequest` / `RevealAnswer` / `RevealRefusal`
  vocabulary with `locate` on surfaces; `reverse_navigation` wired for the
  first time (the inline duplicate deleted); `WorkbenchRevealTarget` and
  `ReadingRevealSubject` gone; `SelectableId` ↔ `ObjectRef` convert; assets
  and sampler REVEAL show their refusal verbatim. Found on the way:
  findings, explanations, comparisons and readings could not be revealed
  at all (the revision guard refused every non-project object, and "Keep
  finding" painted success over that refusal); they are now
  document-scoped and reveal. "Keep finding" writes an
  `audec.kept-findings.v1` record in the workspace document and the
  Explorer lists it after reload. Left for cycle 3: the two `EditorTarget`s
  (blocked on `control_actions.rs`, `workspace_presenter.rs`),
  `DeprojectionWorkspaceTarget` (27 sites), and the three domain mutation
  receipts, which on inspection are mutation receipts rather than rival
  reveal types.

- **Channel**: one `WorkbenchInbox` replaces thirteen mailboxes and ten
  `Pending*` structs; three `Epoch` authorities (document, project,
  analysis) with `Fresh<T>`/`accept` replace the Workbench's staleness
  counters and a stale result is a named diagnostic; closing a pane now
  cancels its own audition (a real teardown bug; new
  `sampler_pane_teardown.sh` scenario and `status.preview`). Left: the four
  per-lens counters in `lens_*.rs`, and `SampleViewOutcome` (blocked on
  `PartialEq` down the constructive chain).
- **Automation**: the `+ Lane` picker offers every parameter the renderer
  actually applies, grouped by object (buses, sends, clip gain/pan);
  addresses no renderer reads are not offered and are listed in
  `address_is_rendered` for cycle 3 (render them or delete them); lane
  simplify / scale time / scale values / paste are undoable actions with
  receipts; the writer adapter is one tested function installed by the
  host.
- **Loom**: the pane says `SKETCH · not in the project until Make pattern`
  and auditions the sketch as edited; after Make pattern it is bound to the
  kit and pattern it made, and cluster mute/gain and event enable/gain/
  nudge are undoable kit and pattern edits with receipts; Make pattern
  focuses the pattern it created.

## Landed 2026-09-04: cycle 1 of the campaign (`PLAN.md`)

Seven Opus lanes on one tree, one writer per file, each gated headless and
live; combined gate 1402 tests green, warnings 2052 → 1950.

- **Catalog**: the product action catalog is now the live registry (twelve
  ids that palette and socket refused now work), the dead menu layout and
  its only consumer are gone, the live menu has Sample / Tempo / Loop /
  pane verbs, Export is `cmd-shift-e` and Loop Selection `cmd-shift-l`,
  Next/Previous Pane mean panes, disabled rows are inert.
- **Controls**: the compatibility half of the mixer/automation views is
  deleted (−1,176 lines, no local undo, no mode branches); edits read
  "requested" until a receipt says committed or refused; `+ Channel` adopts
  a bus only on receipt; `+ insert` says plugin hosting is not connected;
  the automation writer is installed and the write button only shows a
  mode the adapter granted.
- **Arrangement**: receipts flip on the real revision; PROJECT TRUTH is
  live (and a stale-token refusal after mixer-only edits is fixed); track
  M/S/lock/rename/reorder/delete are real; undo enablement reads project
  history; the never-set preview resolver is gone from 18 sites.
- **Export**: bit depth, dither, gain, loop/selection/custom range,
  master/bus/track scope, in the dialog and on the socket; verified by
  sox on 16-bit 8 s loop and 24-bit full exports and per-bus stems.
- **Sampler**: zones store a loop region and an envelope, edited as kit
  commands and voiced (forward wrap, ping-pong, ADSR; default envelope is
  the identity so existing renders are byte-stable); the pane reports
  requested / settled / not connected truthfully; REVEAL and KIT ‹ ›
  work.
- **Subtractions**: migrations for versions that never existed, the codec
  generic, dead journals, `FileActionHost`, five test-only traits, four
  one-value policy enums, the triple `PatternEditorMode`, the allow
  barrels (−1,045 lines across both lanes).

## Landed 2026-09-02 (from the ledger)

- Convolutional NMF (`nmfd`) behind the Components lens: six eight-frame
  gestures per song, each shown as a frequency-by-lag tile; findings still
  publish through the same reverse documents.
- Constant-Q toggle in the waterfall lens (24 bins per octave); FFT stays
  the default and the detail tiles are still FFT. Live timing on the
  6-minute test song, dev build: constant-Q field ready in about 2 s,
  FFT field in about 1 s (driven through the socket's `lens` verb).
- Preferences: lens spectrum choices persist in
  `<config dir>/software.ember.audec/preferences.json`.
- Sampler `+ KIT` / `+ PAD` create the object they name, undoably; Save As
  and Export default next to the package or material; the Investigate
  tree labels findings by title instead of identity strings.
- Removed: `view.rs` + `view/`, `fifo.rs`, `window.rs` (never compiled since
  2022) and `persistence.rs` (superseded by `project_io`, `project_format`,
  `project_store`, `project_codecs`).

## Audits (2026-09-02)

- `docs/UX_EXPOSURE_AUDIT.md`: capabilities the code has that no UI path
  reaches, catalog ids that reach nothing, and every control whose effect
  differs from its label, ranked by damage to trust.
- `docs/ARCHITECTURE_RESIDUE.md`: live structure that costs more than it
  pays (rival reveal-target types, the Workbench mailbox mesh, the
  compatibility half of control_views, migrations for versions that never
  existed, test-only trait seams), with delete / collapse / refactor
  verdicts.

## Integration backlog

`docs/INTEGRATION_LEDGER.md` audits the code the app never reaches: what
each piece intends (its tests), where it plugs in, what a musician gets,
and a verdict. First two items landed 2026-09-02: convolutional NMF in the
Components lens (recurring gestures instead of frozen spectra) and a
constant-Q toggle in the waterfall lens.

## Program

1. Work the integration ledger in order (Beat This worker, pane
   cohesion, settings persistence, render dependencies, then the
   verdict-DELETE items in one commit).
2. Sessions as text: the socket vocabulary becomes the one command
   language shared by palette, menus, keymap, socket, and a session
   journal; replay is bit-exact.
3. Audible diffs: play new-minus-old between cohorts after any edit.
4. Voice-bounded per-track tiles with change-set invalidation.
5. Subtractive audition and live residual inside the beat-making gesture.
6. Pattern and curve languages surfaced in the editors and the socket.
7. Then the shell refactor: a `Lens` trait per analysis view and one typed
   mailbox in place of the thirteen `Arc<Mutex<Vec<Pending*>>>` queues.
