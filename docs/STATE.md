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

## Follow-ups carried out of cycle 3 (2026-09-06)

- Make beat places its pattern on the bar line *before* the loop start when the loop start is not on a bar (found by the audible diff: 1 s of null at 59–60 s). Decide: snap to the bar at or after the selection start (ceil), or to the nearest. Site: constructive_controller.rs placement_start (bar snap added 2026-09-02).
- Null lane: README hunk for scripts/live/README.md (status.diff + audition_diff.sh line) to apply at integration; commit 0e3410e on lane/null carries shell_control.rs diff_json.
- Disk: five cloned target dirs diverge by ~10 GB each as they rebuild; remove a lane's worktree as soon as its branch is harvested; keep CARGO_INCREMENTAL=0 for orchestrator gates.
- Automation lane doubt: reconstruction_apply::apply_automation now refuses every AutomationTarget except Gain; production proposals emit PitchCents / SpectralActivity, so a wired reconstruction path would refuse wholesale. No production caller of plan_selected_reconstruction today. When wiring: lower pitch onto a rendered address or skip with a diagnostic. Also: deprojection_promotion::add_curve and generative_lowering should check address_is_rendered up front (PromotionRefusal::UnknownCurveTarget is the home).
- Shell lane: Next Pane is a silent no-op with the shipped single-dock-pane layout (ui.rs); make it refuse by name or ship two panes. Closed: the startup active_view seed (cycle 3 follow-through) and the one-action lag of status.active_view (lane C5-Socket refreshes the projection before answering).
- Reverse lane: residual_guide names its subject with a ReconstructionProposalId that can collide with real proposals (take an ExplanationRef). Closed in cycle 5: loaded readings survive reopen (`audec.readings.v1`, lane C5-Finding: the Workbench is the one loader and records every load, the shell drains the records into the document); the Compare branch fills live through the `finding` verb (lane C5-Socket).
- Review (cycle 3 wave 1), not fixed yet: (F11) deprojection promotion's add_curve still creates a TrackKind::Automation track and an automation clip the renderer never reads (ensure_automation_track, create_automation_clip), and seed_demo seeds a "Spectral motion" automation track; decide whether promoted curves live only as lanes. (F15) cohort_null materialises ten span-length buffers on the main thread; rewrite as one subtraction over the product slices accumulating energy, reuse render_comparison's metrics, derive the audition id from the operands' digests. (F10c) reconstruction apply refuses PitchCents/SpectralActivity proposals wholesale once wired; multi-clip hit tracks refuse without a planning diagnostic.
- From lane C3-Tiling: `RemoveBus` and `RemoveSend` have the same shape of bug the insert cascade fixed (a bus with an automated insert or gain still fails validation on removal); the per-frame `CompiledAutomation::value_at` lookup is string-keyed for `Plugin` addresses and needs a resolver accessor to hoist.
- Closed by lane C5-Layout (2026-09-12): the descriptor-rewrite oscillation. A native activation is input only while the pane group still agrees with it, and a focus effect that restates what the surface already shows applies nothing (src/workspace_ui.rs `native_pane_event_is_current`). Scenario: scripts/live/descriptor_rewrite.sh. Left open by the same lane: `apply_authoritative_document` calls `panes.restore` on every accepted command, and guise's `restore` resets the native focus to the first leaf with no event, so a command whose transition carries no `Focus` effect (`ReplaceWindowLayout` from a divider drag or a split) leaves the native focus and the layout's record disagreeing; skip `restore` when the target snapshot equals the live one, or a guise API that restores without resetting focus. Pair with the multi-dock-pane work (Next Pane is still a no-op with one pane).
- Closed by lane C5-RenderRestart (2026-09-12): a cancelled render is re-requested for the newest revision, cancellation is never an error, `audio_error` clears on the next success, and an export asked for mid-render waits under a named bound (scripts/live/edit_during_render.sh). Left open: `audec.clip.split` cannot be driven from the socket because it needs a clip selected in a focused arrangement editor (a socket verb that selects an arrangement clip would let the lane's named repro run verbatim); an export that waits keeps the label and the 180 s bound it had at request time even if a newer edit lands (decide whether the bound re-arms per revision); `refresh_audible_export_audio()` renders a whole master export into `audition_audio` on every completion (a whole-image cost on the completion path).
- Found by the cycle-5 gate (2026-09-13), the top item for the next wave: **the
  app's start-up cost is the size of its render-product store.** `TileProductCache::open`
  (src/render_tiles.rs:436) walks every object in the store (`FsContentStore::inventory`),
  reads and verifies every tile receipt, and `adopt`s each one, pinning its manifest
  and its payload (two pin writes per receipt) — all on the main thread inside
  `create_workspace`, before the window paints and before the control socket is
  served. Measured with the release build on *Like a Pen*: a fresh store opens the
  socket in 0.29 s and reaches ready in 2.6 s; the scenario store after twenty
  scenarios (541,153 files, 3.3 GB) took 106 s to the socket and 225 s to ready,
  and a relaunch on the same store had not bound its socket after 200 s (the
  harness reported "did not open its control socket within 180 s" while the app
  was still adopting). The musician's default store at
  `~/Library/Caches/software.ember.audec/render-products` held 59,488 files
  (557 MB) at that moment, so every launch of the shipped app pays tens of seconds
  today, growing with use. The adopted entries also sit in memory (the 554 MB RSS
  of the same run against 267 MB on a fresh store). The design answer is not a
  faster walk: receipts should be adopted on demand when a render asks for a
  recipe (`TileProductCache::hydrate(&mut self, spec: &TileRenderSpec)` at
  src/render_tiles.rs:543 is the seam), with a small index for what the catalog
  needs at start, the walk moved off the main thread, and the socket served before
  the store is open. Until then, gate runs clear the scenario store's
  `render-products` first, so `open_memory` measures the app.
- From lane C5-Finding (2026-09-12), the hole that blocks every reopen: opening a saved package does not restore its material. `status.state` stays `empty` with `audio_error` "decoded metadata differs despite matching content fingerprint": the material asset's metadata is written at import (ui/workbench_lifecycle.rs, near `codec: Some("FLAC".into())`) with a hardcoded codec and a container from the file extension, while the resolver re-decodes through symphonia and names the codec itself (media_resolver.rs `identify`), and `metadata_matches` is an exact `==` on the whole `DecodedAudioMetadata`. Write the metadata the resolver would produce at import, or compare the fields that are identity. Until then a reopened project has no primary source material, so a reading record is read back and refused with `MissingSourceMaterial`, and `readings_and_compare.sh` can only prove the negative half.
- From lane C5-Finding: `ReverseSurfaceViewFactory::request_finding_sample` and lane C5-Socket's `begin_analysis_action` are two entrances to the same controller; give the first a `cx` and fold it into the second.
- From lane C5-Sequencer: the sidebar's EDIT / RECONSTRUCT block is unreachable in the shipped app (`render_sidebar` runs only when `!product_shell_hosted`, and the one window root sets it); Mixer / Automation / Media pool there still call the detached-window opens. Decide whether that sidebar comes back as a pane or goes. The pattern editors' clipboard is per editor (two pattern panes do not share one; a home that is neither pane is needed). `dispatch_focused_editor_action` still resolves Delete/Duplicate for pattern editors through `focus_handle.dispatch_action` (the last rendered frame); `src/ui/helpers.rs` has a fourth `eprintln!` (`hydrating pattern editor`).
- From lane C5-Transport: the export dialog offers the tail but not `metronome_in_export` (socket-only opt-in; the view needs one bool saying whether a click exists); unsaved-recovery packages accumulate one per edited-never-saved document (retention, or removal after the first real save, deserves its own decision); `Tempo` accepts up to 60 million BPM everywhere but the click list.
- From lane C5-Mixer: a compressor insert renders whole bounces (its history bound exceeds any tile context), so a live scenario that exports through one costs minutes per export on a debug build; the audible claim for insert order lives in `engine_regression` for that reason.
- From lane C5-Arrangement: per-clip colour (the audit says `color` on `Track` first, done) and a colour on `Marker` are not written by anything yet.
- From lane C5-Socket: `scripts/live/lens_memory.sh` was renamed to the lens ids but not rerun in the lane; naming a lens now activates the built-in pane instead of allocating a dynamic one, so its numbers may shift.
- Wave-2 review deferrals: (R11) per-frame value_at + coefficient recompute on ramping lanes (perf; lane C3-Tiling may take it); (R12) realtime seek pre-roll stall once the graph host is wired (compressor ~2 s per loop wrap; needs a cap or async pre-roll); (R14) MixerView deep-clones the graph per 33 ms tick and per click (use revision()/processor() directly; extract one nudge control).

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

## 2026-09-12: cycle 4 begins (streaming material)

- Non-FLAC material does not open today: `Workbench::load_path` calls
  `analyze_file_base`, which bails on any extension but `flac`; the media
  resolver's symphonia decoder serves assets and samples, never the
  project material. Commit `b591750` ("Decode every container and codec
  symphonia 0.5 offers") widened the decoder and the chooser, but its
  message's claim that an mp4 and an ogg "open and reach ready" was wrong:
  those readings came from an instance that had refused the file and
  shown an earlier project. Opening mp3/m4a/ogg as material landed with lane C4-Cache the same
  day (`design/STREAMING_MATERIAL.md`): the decoded image is the one open
  path for every container.
- The persistent render store's pin/GC gate (a directory made with
  `create_dir`) was left behind by a killed instance on September 1 and
  refused every later open with "content pins or garbage collection are
  changing; retry"; a gate older than a minute is now reclaimed, and
  `AUDEC_CACHE_ROOT` gives a scripted lane or a second instance its own
  store.
- Baseline for the cycle, release build at `98e372d`, *Like a Pen* (6:13,
  44.1 kHz stereo flac): 813 MB resident after open.
- **Cache** (lane C4-Cache, landed): one decoded image per material,
  mapped, keyed by the fingerprint of the encoded source bytes and the
  project rate (`src/material_image.rs`: `PcmSamples` = owned or a window
  over an `Arc<Mmap>`, `Deref<Target=[f32]>`, so `PcmAsset`, `ProjectAudio`
  and the pyramid changed backing without a consumer learning a new
  shape; a 128-byte header with a 16-byte-aligned payload; a header naming
  other bytes is refused by name and decoded again). Every container opens
  through the one symphonia path (`analyze_file_base` is no longer a FLAC
  special case; claxon is gone; symphonia's FLAC decode is bit-identical
  end to end through sampler, graph and encoder, proved by identical
  make-beat masters); an unreadable file is refused by what the decode
  said; a busy store is a bounded retry, never a route change; material
  opens with its native channel count. `Analysis::mono_pcm` is deleted:
  the pyramid is the one PCM truth and `mono_range_into` serves window
  reads without allocating. Live on the mp3 that could not open before:
  decode 0.61 s, cache hit 0.00 s on the second open, 267 MB RSS and
  103 MB footprint at ready; anonymous whole-file PCM (MALLOC_LARGE)
  266 → 108 MB with ~120 MB as evictable mapped pages; footprint on the
  integrated tree 325 MB against the 813 MB baseline. Wall-clock seconds
  to ready were unmeasurable under seven concurrent builds; the app's own
  phase log says the open is ~1.2 s and the old 54 s was process start
  plus the post-ready component factorisation.
- **Lenses** (lane C4-Lenses, landed): every lens reads through
  `Analysis::mono_range`; rhythm's novelty streams over chunks and is
  bit-identical to the batch result (every novelty, band, hit, peak,
  decay, centroid and tempo bit); the waterfall's FFT and constant-Q
  fields and the spectrogram detail go through the streamed tile with a
  `SpectralTileCache` on the Workbench (16 tiles / 48 MB), bit-identical;
  the constant-Q transform is `analyze_windowed` over a slice reader; loom
  templates come from a 60 s lookbehind around the selection and the
  header says so (`templates from 2:14–3:20`); every artifact descriptor
  used to build a four-bytes-per-sample image of the whole mono just to
  take a digest, and now streams the same bytes into the same SHA-256
  (identity proved). The lane's memory table turned out to be point
  samples taken about a second after ready, while open-time work was
  still running, so its absolute figures do not stand (its byte-identity
  and cost proofs do). Measured on main afterwards, *Like a Pen*, resident
  20 s after open on a private cache: 904 MB debug / 1129 MB release with
  the Lenses lane alone; 867 / 869 MB once the Render lane's bounded
  catalog and receipts landed. The large reduction is the Cache lane's
  (the mapped image), still in flight. Rhythm still materialises one
  whole-mono buffer for its `RenderedExplanation` (a comparison-hydration
  seam outside the lane).
- **Render** (lane C4-Render, landed): playback before completion; the
  `CohortRenderer` takes a priming cohort and serves each slot from the
  newest cohort that covers it, keeping the last *complete* cohort under
  it (a priming-over-priming first attempt produced 342,016 starved frames
  of silence, now a tested case; the scenario asserts starved frames stay
  0 while 41 of 60 samples play with tiles missing); the previous cohort
  is receipts rehydrated from the tile CAS, not a second master; the
  product catalog sits under one `PinnedLru` kernel with a 256 MiB budget
  (retention read from the `Arc` count, so nothing playing is evicted; an
  exceeded ceiling is reported, never enforced by discarding audio); the
  tile context ceiling is four tiles with an instrument-named diagnostic
  when a voice tail exceeds it; export reads the published cohort in slot
  order through a streaming WAV encoder (export cost 377 → 125 MB, 4.7 →
  1.8 s, byte-identical masters; the remaining whole image is the
  `ProjectAudio` handed across the session lifecycle, a follow-up);
  `status.readiness` and `status.memory`.

## Landed 2026-09-13: cycle 5, reach for both products

Eight Opus lanes cut from the two audits (`ANALYSIS_UX_AUDIT.md`,
`DAW_UX_AUDIT.md`), each on its own branch and worktree, integrated by
cherry-pick in the order Socket, Mixer, Arrangement, Layout, RenderRestart,
Sequencer, Finding, Transport. Every conflict was additive (two lanes adding
to the same catalog, socket verb list, or status object); the catalog count
contract moved 46 → 55.

- **Socket** (analysis rows 1, 2; DAW row 5): `audec.lens.{waterfall,
  rhythm,components,separation,loom}` (cmd-1…5) show the pane the workspace
  already holds for that lens, and `audec.analysis.*` (which stacked a pane
  per call) is gone; `finding {index|address, do: open|keep|compare|apply|
  sample|audition:<kind>}` acts on one published Finding with no reverse pane
  open, through the pane's own controller and event; `status.findings` lists
  every finding with each verb `available`/`pending`/`completed`/`refused`
  in the pane's words; `status.lenses[*]` carries `state`, `failure`, `span`
  (with its `basis`) and `findings`; `action {id, parameters}` with declared
  parameter names (`audec.workspace.activate {view}`); piano roll / drums on
  an empty project create the "+ NEW" pattern and open it. Live: the Compare
  branch of the Explorer is filled from a script for the first time
  (`reverse_flow.sh`), thirteen refusals verbatim; `status.active_view` no
  longer lags a verb. Timing datum: rhythm deprojection over the whole 6:13
  song is 280–510 s on a debug build.
- **Finding** (analysis rows 6, 11, 12, 16): a kept finding records its span
  (`FindingSpanRecord`, read from the finding's own reverse document when it
  is kept; an empty or inverted span is no span); Explorer finding rows get
  Hear (the span becomes the selection and the loop through the pointer
  kernel, then play) and Make sample (the same `AnalysisResultController`
  action the reverse pane begins); a Finding surface with no result renders a
  `FINDING SPAN` strip with the same pair; readings are durable
  (`audec.readings.v1`, path plus the manifest identity the load verified,
  replayed through the one loader on open and refused in the codec's words
  if the file changed); `+ WITHIN SELECTION` and `+ NOT EXPLAINED BY THIS
  COMPARISON` in the query builder, the selection asked for at the click;
  Findings and Samples rows drag the payloads the arrangement already
  accepts; evidence categories say why they cannot be renamed or deleted.
  Two shell bugs found by proving it live: a runtime republish of the
  workspace document erased every shell record (kept findings included),
  now carried across (`SHELL_DURABLE_EXTENSIONS`); and the product shell
  settled its records only while being painted (twice per scripted session),
  now `settle_shell` on every control request too. `save {path}` verb.
- **Sequencer** (DAW rows 1, 2, 13, 19, 20a): the rival editor-open path is
  deleted (`open_sequencer_editor`/`open_arrangement_editor` built editors
  with no audition source; the toolbar buttons now dispatch the catalog
  verbs), with the `arrangement_view`/`sequencer_view` singletons and their
  publication mirrors; the arrangement playhead now moves every hosted pane
  (it moved only the detached window); cmd-c/x/v in both pattern editors,
  offset by the paste count, drum steps carried by lane identity and name,
  every mismatch refused in words; `PatternEdit::SetLength` guarded by the
  pattern's own validation (`shortening to 1920 ticks would leave 1 event
  past the end`), refused by name for expression-generated patterns; swing
  for notes is an edit with a receipt (`Swing 25% · 4 notes delayed 60
  ticks`), not a level; `audec.pattern.audition`. Live: `pattern_edit.sh`
  hears the toolbar-opened editor (`Playing exact pattern audition`), which
  before this lane answered `Pattern audition requires a project workspace
  pane`.
- **Arrangement** (DAW rows 3, 4, 11, 12, 17): `SetClipGain`/`SetClipMuted`/
  `RenameClip` through one `put_clip_field` that refuses a value already
  stored and gain on a pattern occurrence by name; fade in/out, clear fades,
  crossfade, repeat and stretch reach the toolbar and keys through the
  existing planners; markers (`ArrangementState.markers`, the frame is the
  identity, `PutMarker`, ruler flags seek and remove, `SnapGuideKind::Marker`
  finally produced); track colour and one rename draft for track and clip;
  `audec.clip.place_selected_asset_at_playhead` lowers to the same
  `DropIntent::InsertAudio` a drag makes. Fixed on the way: the palette's
  clip verbs resolved through `FocusHandle::dispatch_action`, which finds
  its node in the last *rendered* frame, so a script that opened the
  arrangement and asked for a split got `dispatched` and no split; both
  routes now name one `ArrangementVerb` and ask the pane's entity.
  `status.arrangement`. Live (`clip_edits.sh`): a split is byte-identical
  to the unsplit export, six −1 dB presses measure −6.00 dB, a marker
  changes nothing, placement is heard only where it was placed.
- **Mixer** (DAW rows 7, 10, 14, 15, 24): `MoveInsertBefore { processor,
  before }` (an identity, not an index) with ↑/↓ on the strip and two
  action ids; `SampleZone.reverse` persists and plays through one
  `reflect_reverse_position` the render path shares; four ADSR rows replace
  the percussive toggle; loop handles on the zone's range bar; double-click
  resets a control to unity, shift runs a fine drag at 0.2; the meter reads
  the 50 ms of the cohort ending at the playhead and says so; the mixer
  refuses to open without a project and names what would give it one; the
  curve preview says which point was refused. `engine_regression` proves
  filter→compressor and compressor→filter differ and moving back restores
  the first order bit-for-bit. Live (`mixer_sampler.sh`): a reversed zone
  differs from the forward master in 0.26 % of frames, exactly 0.0 outside
  the beat.
- **Transport** (DAW rows 6, 8, 9a, 20, 21, 23): a never-saved project
  adopts a recovery package (`AUDEC_RECOVERY_ROOT`) and autosaves through
  the same path a saved one does, without becoming "saved"; `AUTOSAVED ·
  12s ago` (elapsed, deliberately: this build has no clock source, and a
  UTC hh:mm would be a wrong number); the BPM readout is a field; tempo and
  meter points can be removed (the origin refuses by name; later meter
  points are re-checked); a metronome compiled from the tempo map as a
  stateless `NativeNode::Metronome`, summed *after* the master bus's
  post-fader tap so `RenderScope::Master` carries the click and the bus
  tap is still exactly the project (tile concatenation over a clicking
  master stays byte-exact, tested); `tail_seconds` split honestly into
  rendered tail and silence with the status naming the split; the export
  doc's limiter sentence rewritten as an explicit non-goal; six alt-chords
  for hourly verbs; `tempo {bpm}`, `export {tail_seconds, metronome}`,
  `status.metronome`. Live (`autosave_and_tempo.sh`): click on vs off is
  byte-identical unless the export asks for it; a loop plus 2 s tail
  carries the project's own decay, the last 3 s plus 2 s carries the
  dither floor and the status said so.
- **Layout** (the descriptor-rewrite hang): two stale `Activated` echoes
  ping-ponging, not a recursive call. `activate_or_create_dynamic` issues
  `ReplaceDocument` then `FocusPane`, every accepted command's transition
  carries a `Focus` effect, and guise's `Pane::activate_item` reports
  `Activated` for a tab that was already active; the two echoes arrive
  after `actuating_authority` has dropped and each disagrees with the
  layout, so each is lowered into a new `FocusPane`. Fix: the authority
  does not believe its own echo (`native_pane_event_is_current`: a native
  activation is input only while the live group still agrees with it) and
  does not make one it does not need (`activate_unless_already_shown`).
  A headless test drives the real authority through the same pair against
  a twenty-line model of the pane group and settles in 3 commands, with a
  control that runs without the rule and is still going at 40. Live
  (`descriptor_rewrite.sh`): base binary spins at 158 % CPU and never
  answers; the fix settles and rewrites back and forth three more times.
- **RenderRestart** (the cancelled-render hang): three holes — the stale
  job's completion set `audio_error` unconditionally and nothing cleared
  it; the same closure cleared `audio_rendering` while the newer render
  ran; and when the *newest* render lost its token nothing re-requested it,
  so the export queued behind a cohort nobody was building. Now one
  authority: the desired target is the publication plus its recipe,
  `in_flight` names the one generation out, `is_cancellation()` recognises
  every layer's cancellation, `restart_target_if_idle()` re-issues the
  newest target through the first-request path (bounded at 3, then a named
  refusal), a superseded publication is cancelled without failing its
  target, `audio_error` clears on the next successful publication (this
  also closes the C3-Tiling insert case), and an export asked for
  mid-render says `rendering revision 4 for export` and waits under a
  180 s bound. Live (`edit_during_render.sh`): `audio_error` null across
  the cancellation; the export that waited is byte-identical to the
  settled master.
- **Integration** (orchestrator): hosted arrangement panes receive the
  placeable assets on every publication (the lane put them on the singleton
  mirror Sequencer deleted); `analysis::{spectral_field,
  constant_q_projection, spectral_projection}` are now delegates to the
  windowed forms in `spectral_tiles` (the Lenses lane's dedup hunk), with
  the whole-slice bodies moved into the tile tests as the oracle so the
  bit-identity tests stay non-vacuous; the arrangement's snap cycle moved
  from `s` to `alt-s` (the catalog's `s` is Make Sample; DAW row 21);
  `tempo_and_routing.sh` no longer aborts on zsh's glob no-match before any
  bus file exists.
- Gate (2026-09-13, release build, *Like a Pen* unless said): 874 of 876
  filtered tests pass (2 ignored; the union of every lane's filters plus
  `spectral_tiles::` and `analysis::`); all twenty live scenarios exit 0
  (`loop_state_machine`, `editors_and_windows`, `sampler_pane_teardown`,
  `descriptor_rewrite`, `make_beat_audible`, `audition_diff`,
  `playback_before_completion`, `edit_during_render`, `tempo_and_routing`,
  `autosave_and_tempo`, `readings_and_compare`, `finding_to_sound`, `inserts`,
  `drops`, `clip_edits`, `mixer_sampler`, `pattern_edit`, `lens_memory`,
  `open_memory`, `reverse_flow`), `finding_to_sound.sh` after its executable
  bit was set. Numbers: `lens_memory` 559 MB RSS at open, 929 MB with all
  four lenses open, a rhythm refresh 43 s and a loom refresh 54 s; the
  reverse flow's rhythm finding over the whole song published after 205 s;
  `readings_and_compare` steps 10–13 still show the reopen hole (the reading
  record is read back and refused with `MissingSourceMaterial`, see the
  follow-ups). The `open_memory` figures from this run (182 s to ready,
  554 MB RSS) are NOT the app's open cost: see the store-open finding below.

## Landed 2026-09-06: cycle 3, review of wave 2 (Tiling, and the shell)

- **Tiling** (lane C3-Tiling): the byte-exact contract for native inserts
  is now measured, not hoped. The history bound is the frames for the
  slowest reachable pole to decay 512 bits (merge tail measured over 3.3
  million random boundaries: p50 36, p99.999 174, max 232 bits; the old
  40-bit bound was a coin flip per boundary); a state floor (2⁻⁴⁰) applied
  to every retained state word makes a silent gap reach the same exact
  zero on both paths (39,999 of 40,000 frames differed without it, 0
  with it, and a 200,000-frame noise render is bit-identical floored and
  unfloored); the tile path supplies the declared history exactly once
  (`HistorySupply::Span`) and the regression test asserts each tile's
  context equals the declared bound; the EQ declares no bound any preroll
  could satisfy (a DF1 biquad was not observed to merge) and refuses to
  tile with the ceiling named; a compressor exceeds the tile at every
  reachable release; automating a filter's cutoff loses incremental
  rendering (70,285 > 65,536 frames) because the old bound only kept it
  by being wrong. Removing an automated insert is one reversible
  transaction that takes its lanes and descriptors with it. The reference
  renderer names every native insert it skips. Live: the same centroid
  drop as before (3843 → 1382 Hz); nothing audible changed.
- **Shell**: a stale document leaves the rhythm lens idle instead of
  "Analyzing" forever; the residual guide's comparison term steps with the
  pane's id verb; a redundant activation no longer republishes a pane's
  selection; promotion asks `insert_address_is_rendered` with the mixer;
  Next/Previous Pane refuse when there is nowhere to go.

## Landed 2026-09-06: cycle 3, wave 3 (Drops)

- **Drops**: one gate decides what a channel plays through
  (`route_bus_drop`, proving the route by building the mixer command, so
  cycles, master and return rules are refused in the graph's own words);
  the strip header is a drag source and the strip a drop target with a
  before-release verdict; the OUTPUT button's cycling rule sits on top of
  the same gate; `audec.mixer.route_selected` asks for it by name (live:
  the beat's bus, silent outside the loop, carries the whole song after the
  route, master unchanged). The pattern library is a rail above the
  sequencer grid: every definition a chip (retarget, drag), the rail the
  drop (a copy with ⌥, else a refusal that says so). `SampleActionOutcome::
  ForwardDrop` is deleted: there was never anyone to forward to. The
  arrangement's "not an arrangement drop" refusals stay, correctly.

## Landed 2026-09-06: cycle 3, wave 2 (Collapse-A, Collapse-B, Inserts)

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
- **Inserts**: `NativeNode::Insert` runs an in-tree effect (TPT state-variable
  filter, three-biquad EQ, feed-forward compressor; `src/effects.rs`)
  between the bus mix and the pre-fader tap, honouring the mixer's
  wet/dry and bypass laws; each effect declares its history bound as the
  frames for its slowest reachable pole to decay 40 bits (not the
  design's −120 dB, which is twenty ulps short of byte-exactness), and the
  engine proves whole-versus-tiled byte identity with a resonant filter
  across eight tile boundaries; a compressor's bound at the default
  release exceeds the tile context, so it falls back to whole bounces with
  the controller's named diagnostic. `+ insert` is a three-effect picker;
  insert rows show "active"/"bypassed" with draggable parameters; hosted
  CLAP descriptors keep a truthful offline-only label; `InsertWet`,
  `InsertBypass` and `Plugin` addresses are rendered and offered. Live on
  *Like a Pen*: a low-pass on the master drops the export's spectral
  centroid from 3843 Hz to 1382 Hz (`audec.mixer.insert_filter`,
  `scripts/live/inserts.sh`). CLAP hosting stays a later cycle
  (`design/NATIVE_INSERTS.md`).
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
  picker's descriptor nor stacks lanes. Also: a pane is active from the
  first frame (the projection reads the layout's focus when the mirror is
  empty).

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
