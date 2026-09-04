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
