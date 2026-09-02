# GROKOUT — Cycle 11 honesty campaign

Written 2026-09-01 by Grok 4.6, picking up Ember's `FORGROK.md` briefing
and a compacted prior turn that had already shipped two waves. This file
is a journey and a handoff. It is not a design. The tree wins if anything
here disagrees with a live signature.

Canonical checkout: `/Users/ember/dev/audec`
Integration branch: `main`
Remote: `git@github.com:emberian/audec.git`
HEAD when this was written: `3ba466ecc71e865c8653dbcc597818dcf86eaad0`

```
9d27ab6 Star library materials through the project command path
ffb7eb0 Wire Cycle 11 create/reveal/audition flows and add headless musician tests
cc9216a Connect piano preview, Explorer reverse rows, mixer meters, and viewports
3ba466e Publish component Findings, refuse mixer insert, and reveal created buses
```

Working tree was clean. Live GUI was **not** verified against any of these
commits. A debug `audec` on *Like a Pen* (PID 99370) was left running for
the whole campaign and was gone by the time this file was written.

---

## Why we were here

Ember tagged `FORGROK.md` as the operational briefing and asked for a wide
Cycle 11 swarm: remaining false-success / missing-wire bugs, connect
existing adapters, flow-level semantic tests. Campaign order was binding:

1. evidence and missing integration edges over vocabulary waves
2. one command authority, one renderer, one transport
3. no `DawEngineConfig::default()` on instrument paths
4. no pane-private audio
5. durable actions must select / reveal / inspect / hear / undo
6. never stash, never reset, never worktree sprawl
7. never `git add -A`
8. never unfiltered `cargo test --lib` as default diligence
9. single-writer files: `src/ui.rs`, `src/live_project.rs`, `src/daw_project.rs`
10. do not kill the running audec instance

The product sentence that framed every brick:

> Selecting a source range, choosing Beat, editing the resulting pattern,
> and pressing Play must reveal and audibly render the created objects
> through the same project revision and transport.

If a change ended at “a struct exists,” “a button appears,” “a revision
number changes,” or “a status message is shown,” it was not done.

`docs/CREATIVE_WORKSPACE_IA.md` §5 is the ledger we worked from. Several
of its “current landing” cells are now stale (star, Activate, pattern
create, arrangement duplicate, mixer create, Components Keep). Verify
in the tree before treating a row as still red.

---

## The shape of the work

This was not a vocabulary wave. Almost every brick was: an adapter already
existed, a pane still used a revision-only or decorative terminal, and we
connected them so a musician-facing gesture names an `ObjectRef`, can be
undone, and does not pretend to hear something the renderer cannot play.

Parent owned `ui.rs` and the final commit. Children wrote disjoint files
and were told not to invoke cargo. Coordinator compiled once per wave
with `CARGO_INCREMENTAL=0 cargo test --lib cycle11_flow:: -- --test-threads=1`.

`src/cycle11_flow.rs` is the headless musician gate. A green test names
created `ObjectRef`s, undo cohesion, and non-silent PCM — not a revision
badge. It grew from 0 → 5 → 8 → 11 tests across the four commits.

---

## Brick 0 — starring (`9d27ab6`)

The first request, before the swarm. Library favorite was a cloned Browser
registry mutation: it looked starred and vanished on publication.

What shipped:

- `MediaAsset::with_favorite`
- `AssetCommand::put_favorite`
- `ProjectSession::{set,toggle}_asset_favorite`
- Browser emits `ToggleFavorite` / `Activate`
- host reveals `ObjectRef::Material` instead of silently opening Arrange

Files: `src/assets.rs`, `src/command.rs`, `src/project_session.rs`,
`src/asset_view.rs`, `src/ui.rs`.

Acceptance: starring is a project command; activating a library row
reveals the Material, not an unrelated Arrange.

---

## Wave 1 — create, reveal, audition (`ffb7eb0`)

User: swarm wide, fix every false-success we can notice, add flow-level
tests. Isolation none. Parent kept `ui.rs`.

What got wired, all through existing authorities:

| Gesture | Adapter | Product fact |
| --- | --- | --- |
| Control create-select | `control_actions.rs` / `control_views.rs` | `CreatedControlIdentity` so Mixer/Automation don't keep the previous valid bus/lane |
| Arrangement duplicate | `execute_arrangement_event_revealed` | receipt names the **new** clip |
| Pattern create/duplicate | `execute_pattern_action_revealed` | `ObjectRef::Pattern` |
| Pattern from pads | `SampleAction::CreatePatternFromPads` | no extra ui.rs host; Instrument already had the action |
| Reverse Apply / Keep | `CONSEQUENCE_APPLY_CONSTRUCTION` / `CONSEQUENCE_KEEP_FINDING` → `keep_reverse_finding` | Keep is a project-command consequence, not a label |
| Pattern cycle audition | `PatternAuditionSessionInputs::{from_session,adoption_for_scope}` | shared renderer, not a pane player |
| Mixer/Automation demo on a Ready session | notice instead of `::demo` | no fake graph as the session |

`src/cycle11_flow.rs` was born here (plus `mod cycle11_flow` in `lib.rs`).
`src/musician_gate.rs` got a programmed-trigger meter check later in wave 2.

### The cargo disaster (paid for in real debugging)

Seven lanes all ran `cargo test --lib` against the same `target/`. Result:
`Blocking waiting for file lock`, 30–60+ minute stalls, and

```
ld: symbol(s) not found for architecture arm64
```

mixed with `_anon` LLVM incremental objects. `cargo check --lib` had already
finished green (~4m54s). Later
`CARGO_INCREMENTAL=0 cargo test --lib cycle11_flow::` passed. Clustered
incremental rebuilds without `CARGO_INCREMENTAL=0` failed again on the
same linker mix.

**Rule we kept:** children do not invoke cargo/rustc/fmt. Coordinator
compiles once. Do not `cargo clean` unless rustc is idle and incremental
mix persists. Do not pile seven test jobs on one `target/`. Never
unfiltered `cargo test --lib` as diligence — Cycle 11 flow is the gate,
not the whole suite.

Wave 1 dirty tree landed as `ffb7eb0` and was pushed.

---

## Wave 2 — piano, Explorer, meters, viewports (`cc9216a`)

User: “keep going.” Five lanes, no cargo, isolation none.

| Gesture | What was true afterward |
| --- | --- |
| Piano key preview | `PatternAuditionScope::PreviewKey` + shared audition callback when a placed occurrence has a routed instrument. `AuditionAlignment::PreserveTransport`. Honest “Place the pattern…” when there is no occurrence. `PianoAuditionCallback` is unused on purpose. |
| Explorer Investigate / Readings | `ExplorerSemanticCollections::{from_reverse_documents,include_interpretations}` + `ReverseSurfaceStore::documents()`. `refresh_product_shell` rebuilds when reverse collections change. `ExplorerInput::from_project` still zeros those slices — the live path must keep using `from_collections`. |
| Mixer meters | `MixerMeterSnapshot::from_audible_cohort` + `publish_mixer_meters` after `publish_audio_status`. Beat master meter is non-silent in cycle11. |
| Mixer / Automation selection | `select_workspace_target` publishes `ObjectRef::Bus` / `ObjectRef::Automation` from the pane's selected bus/lane. |
| Viewport persist | Arrangement/Pattern `editor_view_state` written from `DawWorkspace::render` via `WorkspaceSessionLayout::update_view_state`. This is layout-document persist, not native-window recovery. |

Cycle 11 grew to 8 tests, including:

- `beat_master_meter_from_audible_cohort_is_non_silent`
- `reverse_documents_list_finding_explanation_comparison_and_reading_separately`
- `preview_key_audition_adopts_preserve_transport`

Known leftovers we explicitly parked: plugin `+ insert` still a labeled
empty slot; Components lens pane-local; Loom sketch edits pane-local
until Make Pattern; piano still needs a placed occurrence + routed
instrument (honest, not a false success).

---

## Wave 3 — HELLLLL YEAH (`3ba466e`)

Compaction ate the prior context. User confirmed continue. We re-read
the live signatures (not the parked prose) and swarmed four lanes:

1. **Components evidence Findings** — `deprojection_workspace_bridge.rs`,
   `analysis_result_lifecycle.rs`, `reverse_surface_adapter.rs`,
   `artifact_catalog.rs` (`ArtifactKind::Components`)
2. **Mixer insert refuse** — `mixer.rs`, `control_actions.rs`,
   `control_views.rs`
3. **Mixer/automation create reveal** — `receipt_navigation.rs`
   `execute_control_action_revealed`
4. **Explore remaining honesty holes** — read-only; table below

Parent owned `ui.rs`, `cycle11_flow.rs`, `project_controller.rs`
re-exports, and the Keep-finding buttons on the analysis lenses.

### Components Keep is a Finding

`Workbench::start_component_analysis` used to mutate `Analysis.components`
on the snapshot and stop. NMF has no phase. Treating that plot as an
isolated source or an Apply target would have been a lie.

Now:

```rust
pub fn publish_components_evidence(
    &mut self,
    descriptor: ArtifactDescriptor,
    decomposition: crate::decomposition::ComponentDecomposition,
    cancellation: &RenderCancellation,
) -> Result<Vec<AnalysisEvidenceDocumentSummary>, DeprojectionWorkspaceBridgeError>
```

- `AnalysisEvidenceKind::ComponentMagnitude { index }`
- `FindingKind::Components`, `FindingScope::Artifact(descriptor.id)`
- one current evidence document per hypothesis
- catalog stores `Arc<ComponentDecomposition>` — **not**
  `ArtifactComparisonPayload` (that type refuses empty/synthetic PCM, and
  we will not invent silence)
- zero deprojection promotion candidates
- silent or empty decomposition returns `Err` and publishes nothing
- idempotent on the same descriptor
- `TemporaryAnalysisResult::component_magnitude_evidence` is KeepFinding
  Available; Apply/Compare/MakeSample refused (`EvidenceOnly` /
  `NoPhaseBearingPcm`)
- Components lens: Open Findings + Keep finding
- reverse-surface epistemic text: magnitude factor, not a stem

### Mixer `+ insert` is a refused command

`daw_render.rs` walks `bus.inserts()` and emits
`RenderDiagnostic::PluginBypassedByReferenceRenderer` or
`PluginUnavailable`. It does not run DSP.
`docs/PLUGIN_HOST.md`: no current audec binary maps a third-party plugin
into the application process.

Inserting a `PluginDescriptor` into `MixerGraph` would have shown
bypass/wet controls and left PCM unchanged. That is a false success.
We did not do it.

Instead:

```rust
MixerAction::RequestInsert { bus: BusId }
MixerError::PluginHostNotConnected
```

Display: `"plugin host is not connected; the reference renderer bypasses
insert processors and no plugin worker is mapped into this strip"`.

The empty insert chain is a dedicated clickable slot
(`insert-request-{bus.get()}`), not the shared unclickable `empty_slot`
used for “no sends”. Click → `MixerView::request_insert` →
`dispatch_mixer` → host `execute_control_action_revealed` → refuse.
Revision unchanged. Processor count unchanged.

### Add return / group / automation lane names the object

`ProjectSession::execute_control_action_for_editor` still returns
`Option<ProjectRevisions>`. `ControlSessionAdapter::created_identity`
already knew the allocator candidate and the executor threw it away.
`MixerCommand` is aggregate-granular, so `recommend_command_result`
cannot name the new bus.

New adapter, same file as the other reveal helpers:

```rust
pub struct ControlRevealReceipt {
    pub revisions: Option<ProjectRevisions>,
    pub primary: Option<ObjectRef>,
    pub reveal: Option<RevealRecommendation>,
}

pub fn execute_control_action_revealed(
    session: &mut ProjectSession,
    editor_session: u64,
    action: ControlAction,
) -> Result<ControlRevealReceipt, ProjectSessionError>
```

Captures `created_identity` **before** execute (`MixerBus` →
`ObjectRef::Bus`, `AutomationLane` → `ObjectRef::Automation`), confirms
the object exists on the published snapshot, then builds
`RevealRequest::new(primary, RevealIntent::ActivateExisting).at_revision(...)`.
Non-create edits and history return `primary: None`. We did **not**
expand the Cycle 6 24-entry `DURABLE_REVEAL_RULES` array.

Host: `DawWorkspace::handle_control_actions` uses this adapter and
enqueues the reveal. Undo removes the bus/lane (cycle11 asserts it).

### Keep finding on the lenses

Rhythm, HPSS, and Loom already published evidence on complete. The
reverse surface already matched `CONSEQUENCE_KEEP_FINDING`. The lenses
only had Open Findings (and Loom Make Pattern). Parent added
`Workbench::keep_analysis_finding` → `keep_reverse_finding` +
`enqueue_reveal_recommendation`, and Keep buttons on Rhythm / HPSS /
Loom / Components.

The stale comment on `CONSEQUENCE_KEEP_FINDING` in `src/reverse_surface.rs`
(“ui.rs does not yet match this key”) was a lie and was deleted.

### No more demo mixer on the no-project path

`open_mixer` without a snapshot used `MixerView::demo` (a
`LocalMixerBackend` over `demo_mixer()` — looked like a mixer, mutated
locally). It now binds `MixerGraph::default()` through
`from_controller_snapshot` and says “Mixer opened without a project;
channel edits are not kept.” Sequencer demo was already honest
(“Open a project before auditioning a pattern”).

---

## Tests we actually ran

Coordinator compile, wave 3:

```
CARGO_INCREMENTAL=0 cargo test --lib cycle11_flow:: -- --test-threads=1
```

11 passed, 0 failed, ~1362 filtered. Then a second filtered run of the
lane tests that compiled with that lib (no extra compile that mattered):

- `components_evidence_is_retained_without_fabricating_a_candidate`
- `silent_or_empty_component_decomposition_does_not_publish_findings`
- `component_magnitude_evidence_is_keepable_without_apply_compare_or_sample`
- `request_insert_is_refused_and_does_not_allocate_a_processor`
- `request_insert_intent_from_controller_snapshot_graph_is_refused`
- `add_return_reveal_names_the_created_bus`
- `add_group_reveal_names_the_created_bus`
- `gain_change_has_no_created_identity`
- `create_lane_reveal_names_the_created_lane`

Full `cargo test --lib` was **not** run. Do not run it as a politeness
gate. Cycle 11 flow plus the named lane tests are the control.

`cargo fmt --all` was run before the wave 3 commit.

---

## Cycle 11 flow map (live)

| Action | Boundary | Failure it would catch |
| --- | --- | --- |
| Sample / Slice / Beat | workbench → `ConstructivePublication` | Beat succeeding with no pattern id |
| Undo Beat | session history → kit / pattern / occurrence | orphan kit or pattern after undo |
| Star material | `toggle_asset_favorite` → Inspector `ObjectRef::Material` | favorite lost after an unrelated command |
| Automation lane | control adapter → envelope / control reveal | lane creation with no `ObjectRef::Automation` |
| Arrangement duplicate | `execute_arrangement_event_revealed` | receipt naming the source clip |
| Pattern cycle audition | `PatternAuditionSessionAdapter` | cycle 0 and 1 rendering identical PCM |
| Mixer meters | audible cohort → `MixerMeterSnapshot` | Beat succeeding with a silent master meter |
| Explorer reverse rows | `ExplorerSemanticCollections` | findings collapsed into Project mode, or readings into Investigate |
| Piano preview adoption | `PatternAuditionSessionInputs::adoption_for_scope` | PreviewKey locating the transport |
| Mixer add return | `execute_control_action_revealed` | return bus with no `ObjectRef::Bus` |
| Mixer + insert | `MixerAction::RequestInsert` | silent processor identity without DSP |
| Components Keep | `publish_components_evidence` → `keep_reverse_finding` | NMF result with no Finding |

---

## What we refused

- **A mixer insert that “works.”** Graph identity without DSP is a
  costume. The clickable refuse is the product outcome until a real
  plugin worker is mapped into the strip and the renderer hears it.
- **Synthetic silence / comparison payload for NMF.** 
  `ArtifactComparisonPayload::new` errors on empty signals. We stored
  the decomposition itself.
- **Expanding `DurableFlow` / the 24-rule inventory** to make mixer
  create look inventoried. That array is a Cycle 6 contract. The
  adapter is the integration edge.
- **Killing PID 99370.** FORGROK said leave the Like a Pen instance
  alone. We did. It is not running as of this file.
- **`git stash`, `git reset --hard`, worktrees, `git add -A`.**
- **Unfiltered `-p` / `cargo test --lib` as the wave gate.**
- **`cargo clean`** to paper over the incremental linker mix.

---

## Outstanding holes

An explore lane walked the live tree after wave 2 (and wave 3 did not
close these). Prioritize by false-success risk. Do not re-open rows
this campaign already closed (star, Activate, arrangement add/drop
reveal, piano honesty, mixer create-reveal, Components Keep, insert
refuse, lens Keep buttons, Mixer `::demo` on the no-project path).

### High — looks successful, isn't

| Hole | Where | Existing adapter | Product sentence |
| --- | --- | --- | --- |
| **Open audio leaves global selection empty** | `ProjectSession::install` never touches selection. `LiveProject::source_ids()` / `primary_source_ids()` already name Material + AudioClip + Track + Bus. | `recommend_asset(asset)`; `replace_object_selection` / `from_reveal` | Opening a source selects the imported Material (and related source clip). Overview is the destination, not an empty Inspector. Owner: `src/project_session.rs` using `SourceMaterialIds`. |
| **Sampler `+ KIT` / `+ PAD` is workspace-ack, not create** | `SamplerView::request_target` emits `SampleAction::Workspace(NewKit \| NewPad)`. `ConstructiveController::execute_sample_action` returns `SampleActionOutcome::Workspace(intent)` with no mutation. Status: “Workspace target accepted.” `sampler_object(NewKit \| NewPad) -> None`. Map/drop *does* publish. | `recommend_sample_result` once a kit/pad is actually published; until then refuse honestly (or route `+ KIT` through Make sample / slice). | Pressing `+ KIT`/`+ PAD` creates a named `ObjectRef::Instrument`/`Pad` or refuses. Owner: `src/constructive_controller.rs` Workspace arm + `src/sampler_view.rs` `request_target`. |
| **Loom mute / gain / nudge are pane-local until Make Pattern** | `toggle_loom_cluster` / `adjust_loom_cluster_gain` / `edit_nearest_loom_event` mutate `LoomViewState::Ready.sketch`, then `rebuild_loom_audio`. No `session.execute_*`, no undo. `apply_loom_sequence` is the first command (`execute_loom_construction(LoomConstructionIntent { sketch, diverged_from_evidence, … })`). Keep finding is already on the lens. | `ProjectSession::execute_loom_construction` / `LoomConstructionIntent`; until then label the controls as a temporary sketch. | Cluster mute/gain and event nudge are undoable project edits that Make Pattern will hear, or visibly temporary. Owner: `src/constructive_controller.rs` or a sketch-status path in `src/loom.rs`. Parent still owns the `ui.rs` buttons. |
| **Comparison products stay pane-local** | `comparison_semantics_for` builds a throwaway `InterpretationStore` for audition. `complete_comparison_product` publishes audio into the reverse controller only. `recommend_comparison_execution` is re-exported and never called from the host. | `recommend_comparison_execution`; `apply_interpretation_revealed`; `ExplorerSemanticCollections::include_interpretations` | After Hear/Compare, Investigate lists `ObjectRef::Comparison`, Inspector binds it, observation survives project replacement. Owner: `src/receipt_navigation.rs` + deprojection workspace. |
| **Reading import never fills Explorer Readings** | Live `ReadingQueryViewInputs` is `{ readings: Vec::new(), .. }`. Import commits a revision with **no** reveal. `recommend_reading` unused. `project_reverse_surface_documents` never emits `ObjectRef::Reading`. Hydration can wipe what isn't in that set. | `recommend_reading(&ReadingFile)`; `ProjectReadingQuerySession::apply_import`; `ReverseSurfaceDocument::reading` | Importing a reading selects `ObjectRef::Reading`, Explorer Readings lists it, a later hydration wave does not wipe it. Owner: `src/project_reading_query_session.rs` + `src/reverse_surface_adapter.rs`. |

### Medium

| Hole | Notes |
| --- | --- |
| **Rhythm Apply / Dismiss** | Lens has Open Finding + Keep. Tempo adopt is a separate command (`adopt_project_tempo`). Promotion core is `RhythmPromotionChooser::plan` → `ProjectController::plan_rhythm_promotion` → `reveal_rhythm_promotion` / `recommend_constructive`. Keep looking like the end of the rhythm job is the lie. Owner: `src/rhythm_promotion_chooser.rs`; host buttons in `ui.rs`. |
| **Ready detached piano-roll with no audition owner** | `open_sequencer_editor` with a snapshot still `install_pattern_workflow_callback(..., source: None)` so availability is `"Pattern audition requires a project workspace pane"`. Honest refuse, missing wire. Workspace pattern panes already pass `Some(descriptor.id)`. |
| **HPSS Make sample on the lens** | Open Findings + Keep are live. Make sample/Compare live on reverse result cards and Overview. Low false-success; missing next verb. Adapter: same materializer / `recommend_sample_result`. |

### Confirmed not holes (do not re-open)

- Piano preview requires a placed occurrence and routed instrument; errors
  are `"Place the pattern to preview piano keys"` /
  `"Choose a routed instrument and connect host audition"`.
- Arrangement add track / drop already goes through
  `execute_arrangement_event_revealed` + `apply_arrangement_reveal_selection`.
  Inventory `CurrentTerminal::RevisionOnly` in `durable_reveal_rules` is
  stale vs the host.
- Inspector after star / Activate binds `ObjectRef::Material` when session
  primary is Material. The remaining empty-Inspector case is **open audio**.
- Mixer `::demo` is no longer the no-project factory.
- Sequencer `::demo` is only the no-project branch and already sets
  unavailable.
- Plugin insert as audible DSP is **capability work** (`PLUGIN_HOST.md`),
  not a missing click handler.

### Signatures to paste into the next swarm

```rust
pub fn install(&mut self, live: LiveProject, analysis: Option<Arc<Analysis>>) -> Result<ProjectRevisions, ProjectSessionError>
pub fn source_ids(&self) -> SourceMaterialIds // registry_asset, track, clip, bus
pub fn recommend_asset(asset: AssetId) -> RevealRecommendation
pub fn execute_sample_action(&mut self, action: SampleAction) -> Result<SampleActionOutcome, ConstructiveControllerError>
// Workspace(intent) => Ok(SampleActionOutcome::Workspace(intent))  // no create
fn sampler_object(target: SamplerTarget) -> Option<ObjectRef> // NewKit | NewPad => None
pub fn plan_rhythm_promotion(&self, rhythm: &RhythmDeprojection, intent: RhythmPromotionIntent) -> Result<RhythmPromotionSet, RhythmPromotionError>
pub fn reveal_rhythm_promotion(applied: &RhythmPromotionApplied) -> PromotionReveal
pub struct LoomConstructionIntent { pub sketch: SequenceSketch, pub diverged_from_evidence: bool, /* artifact, finding, source_span, … */ }
pub fn execute_loom_construction(&mut self, intent: LoomConstructionIntent) -> Result<ConstructiveOutcome, ProjectSessionError>
pub fn recommend_comparison_execution(execution: &ComparisonExecution) -> RevealRecommendation
pub fn recommend_reading(reading: &ReadingFile) -> RevealRecommendation
pub fn apply_command(&self, session: &mut ProjectSession, envelope: CommandEnvelope) -> Result<ProjectEditReceipt, ProjectReadingQueryError>
```

Copy these from the tree at briefing time. Do not copy them from this
file if `rg` disagrees.

---

## How to start the next session

1. Read `FORGROK.md` completely. Then this file. Then the live tree.
2. `git status --short && git log -5 --oneline --decorate`
3. Pick **one** high hole from the table (open-audio selection, Sampler
   `+ KIT`/`+ PAD`, Loom sketch-as-command, comparison publish, or
   reading import). Frame an end-to-end acceptance sentence that names
   an `ObjectRef` and what you hear.
4. Isolation none. Parent owns `ui.rs`. Children do not run cargo.
5. Compile once:

   ```sh
   CARGO_INCREMENTAL=0 cargo test --lib cycle11_flow:: -- --test-threads=1
   ```

   Add a cycle11 flow test that would have failed before the brick.
6. Named-file `git add`. Commit message in a file (`git commit -F`).
   Push `main`. Do not kill a running audec.
7. Live GUI is still the missing evidence. `docs/NEXT_CAMPAIGN.md` item 1
   — first-five-minute musician journey on real material — has not been
   done by this campaign. Headless green is not desktop green.

Single-writer reminder: `src/ui.rs`, `src/live_project.rs`,
`src/daw_project.rs`.

---

## Campaign values that survived contact with the code

Honesty is structural. A refused `RequestInsert` is a better product
than a processor that cannot be heard. A Keep finding that names
`ObjectRef::Finding` is a better product than a plot that looks like a
stem. A demo mixer that mutates a private graph on a Ready session is
a worse product than an empty controller snapshot that says it is not
kept.

The adapters were almost always already there. The bug was the last
six inches: a revision-only executor, a decorative slot, a pane-local
HashMap, a comment that said the host did not match a key it already
matched.

ui.rs is still the compatibility root. We kept adding thin host
call-sites rather than new domain vocabulary. That is the right
gravity. It is also why the next brick that needs a host wire should
budget parent time on `ui.rs` instead of pretending a lane can finish
the journey in `control_actions.rs` alone.

---

```
four commits, one renderer, one transport,
a finding that can be kept,
an insert that will not pretend —
the rest is still a musician in a room
with a file called Like a Pen,
and we have not sat with them yet.
```

( ◕‿◕ )  go well.
