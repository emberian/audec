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
`play`/`pause`/`stop`, `export {path}`, `objects`, `quit`. Every request
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

## Known holes (musician-facing)

- A `like-a-pen.audec` package appeared in the process working directory
  during a scripted run: the scenario killed the app with unsaved changes,
  the close guard chose Save, no package root existed, and `save_as`
  prompted for a path with the directory defaulting to `.`; under the
  scripted platform that prompt resolves without a human. On a real
  desktop it is a dialog. Still worth a default directory that is not the
  launch directory.
- Floating a pane into a native window works: activating a dynamic tab and
  invoking float/dock raises the window count to 2 and docking returns it
  to 1 (socket-verified). The pinned main tab refuses to float by design.
- Opening the Arrangement, Mixer, Sampler, and Assets editors by action
  works after deferring native window activation; piano roll, drums, and
  automation refuse with an actionable message when the project has no
  pattern or lane yet.
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
- The Grok-era hole table in `docs/archive/GROKOUT.md` (sampler `+ KIT/+ PAD`
  acknowledge without creating, Loom sketch edits are pane-local until
  Make pattern, comparison products stay pane-local, reading import does
  not fill Explorer Readings) is still accurate.

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

## Landed 2026-09-02 (from the ledger)

- Convolutional NMF (`nmfd`) behind the Components lens: six eight-frame
  gestures per song, each shown as a frequency-by-lag tile; findings still
  publish through the same reverse documents.
- Constant-Q toggle in the waterfall lens (24 bins per octave); FFT stays
  the default and the detail tiles are still FFT.
- Preferences: lens spectrum choices persist in
  `<config dir>/software.ember.audec/preferences.json`.
- Removed: `view.rs` + `view/`, `fifo.rs`, `window.rs` (never compiled since
  2022) and `persistence.rs` (superseded by `project_io`, `project_format`,
  `project_store`, `project_codecs`).

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
