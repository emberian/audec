# Workbench event channel and one freshness guard (cycle 2, lane C2-Channel)

Resolves `ARCHITECTURE_RESIDUE.md` #2, #3, #4. Tree quiet in the owned files
while this lane runs; nothing else edits `src/ui.rs`, `src/ui/workbench_*.rs`,
`src/pane_audio.rs`, `src/sample_actions.rs`.

## Today

`Workbench` (`src/ui.rs`) holds thirteen mailboxes, each an
`Arc<Mutex<Vec<T>>>` (or `Rc<RefCell<Vec<T>>>`) cloned into a view factory
and drained by name from the 33 ms ticker in `src/ui/workbench_lifecycle.rs`:

```
arrangement_events            Vec<PendingArrangementEvent{source, event}>
arrangement_timeline_events   Vec<PendingArrangementTimelineEvent{source, event}>
sample_actions                Vec<PendingSampleRequest{request, completion, source}>
sample_focuses                Vec<PendingSampleFocus{source, focus}>
object_reveals                Vec<PendingObjectReveal{receipt, diagnostics, headline}>   (also duplicated on DawWorkspace)
reverse_surface_events        Vec<ReverseSurfaceViewEvent>
reverse_analysis_result_events Vec<ReverseAnalysisResultEvent>
explanation_workbench_events  Vec<PendingExplanationWorkbenchEvent{source, event}>
control_actions               Vec<PendingControlAction{editor_session, action}>
pattern_workflows             Vec<PendingPatternWorkflow{request, completion}>
pattern_auditions             Vec<PendingPatternAudition{request, owner}>
reading_query_effects         Vec<PendingReadingQueryEffect{source, effect}>   (Rc<RefCell>)
asset_events                  Vec<AssetBrowserEvent>
pending_pane_context_menus    Vec<(WorkspaceViewId, Point)>                    (Rc<RefCell>, DawWorkspace)
```

The ticker calls eleven `handle_*` drains in a fixed order. Teardown
(`prepare_for_document_install`) resets each by hand. Eleven `u64`
generation counters (`spectrogram_generation`, `open_generation`,
`component_analysis_generation`, `save_generation`, `spectrum_generation`,
`hpss_generation`, `rhythm_generation`, `loom_generation`,
`document_generation`, `project_generation`, `editor_session`) each guard one
async path with its own drop rule; a missed bump silently applies a stale
result. `ActionContextSignature`/`ContextEpoch` (`ui.rs` ~2091,
`ui/shell_actions.rs` ~98) is the one gate that is right: one signature, one
comparison, one bump, one named refusal.

## Target

```rust
/// Everything a view or a background task can tell the Workbench.
pub(super) enum WorkbenchEvent {
    Arrangement { source: Option<WorkspaceViewId>, event: ArrangementViewEvent },
    ArrangementTimeline { source: Option<WorkspaceViewId>, event: ArrangementTimelineEvent },
    SampleRequest { source: Option<WorkspaceViewId>, request: SampleActionRequest, completion: Option<SampleCompletionTarget> },
    SampleFocus { source: Option<WorkspaceViewId>, focus: SampleResultFocus },
    ObjectReveal { receipt: RevealReceipt, diagnostics: Vec<RevealDiagnostic>, headline: String },
    ReverseSurface(ReverseSurfaceViewEvent),
    ReverseAnalysisResult(ReverseAnalysisResultEvent),
    ExplanationWorkbench { source: WorkspaceViewId, event: ExplanationWorkbenchEvent },
    Control { editor_session: u64, action: ControlAction },
    PatternWorkflow { request: PatternWorkflowRequest, completion: Entity<SequencerEditor> },
    PatternAudition { request: PatternAuditionRequest, owner: AuditionOwner },
    ReadingQueryEffect { source: WorkspaceViewId, effect: ReadingQueryViewEffect },
    Asset(AssetBrowserEvent),
    PaneContextMenu { view: WorkspaceViewId, at: Point<Pixels> },
}

pub(super) struct WorkbenchInbox {
    sender: WorkbenchSender,            // Clone + Send; what factories capture
    receiver: Receiver<WorkbenchEvent>, // drained once per tick, in arrival order
}
pub(super) type WorkbenchSender = std::sync::mpsc::Sender<WorkbenchEvent>; // or a small Arc<Mutex<VecDeque>> with the same two methods
```

- **One sender** is captured by every view factory and background task
  in place of thirteen mailbox clones. `Entity`-carrying variants keep the
  `!Send` payloads on the main thread by construction: those senders are
  handed only to main-thread callbacks (the same places that push today).
- **One drain**: the ticker takes everything and dispatches by variant to
  the existing `handle_*` bodies, which become `fn on_arrangement(&mut self,
  source, event, cx)` etc. Arrival order replaces the fixed drain order;
  if a real ordering dependency exists between two handlers today, name it
  in the report and keep it with an explicit two-phase drain, do not
  reintroduce per-type queues.
- **Teardown**: `prepare_for_document_install` replaces the inbox (a new
  channel) instead of resetting fourteen fields; a late event from a dead
  view is dropped by the receiver, not by a hand-written reset.
- **Freshness**: `pub(super) struct Epoch(u64)` per authority and
  `pub(super) struct Fresh<T> { epoch: Epoch, value: T }`. Authorities:
  `document` (workspace document install), `project` (aggregate
  revision/publication), `analysis` (material load), `selection`. Every
  async result carries the epoch it was requested at; the acceptance site
  is one method `Workbench::accept<T>(&self, fresh: Fresh<T>, authority)
  -> Result<T, Stale>` that returns a named `Stale { authority, requested,
  current }` the caller logs or surfaces. The eleven counters collapse to
  these four; `editor_session` stays only if a pane needs an identity
  distinct from `WorkspaceViewId` (it should not; say why if it must).
- **Sample pipeline** (#4): `SampleViewOutcome` is deleted;
  `SampleActionOutcome` is what the pane receives, mapped to status text
  inside the pane. `PendingSampleRequest → PreparedSampleAction →
  SampleActionOutcome → SampleResultFocus → ObjectReveal` becomes three
  events on one channel; the request/completion pair stays typed.
- **DawWorkspace** stops holding its own copy of `object_reveals`; it
  receives `WorkbenchEvent::ObjectReveal` like everyone else.

## Not in scope

Changing what any handler does. This is plumbing: after the lane, every
existing behaviour test passes unchanged, the three live scenarios pass,
and a new scenario proves teardown: open a sampler pane, start a pad
audition through the pane's callback, close the pane, and `status` shows no
preview playing.

## Gate

Full suite; `scripts/live/*.sh`; the teardown scenario above (add it under
`scripts/live/`); a grep gate in the report: zero `Arc<Mutex<Vec<Pending`
in `src/ui.rs`, at most four `Epoch` fields on `Workbench`.

## Files owned

`src/ui.rs`, `src/ui/workbench_lifecycle.rs`, `src/ui/workbench_events.rs`,
`src/ui/workbench_publication.rs`, `src/ui/workbench_sampling.rs`,
`src/ui/workbench_panes.rs`, `src/ui/workbench_editors.rs`,
`src/ui/workbench_reading.rs`, `src/ui/workbench_reverse.rs`,
`src/ui/workbench_timeline.rs`, `src/ui/workbench_transport.rs`,
`src/pane_audio.rs`, `src/sample_actions.rs`, and the factory callback
sites in `src/arrangement_view.rs`, `src/sampler_view.rs`,
`src/control_views.rs`, `src/asset_view.rs`, `src/reverse_surface_view.rs`,
`src/explanation_workbench_view.rs`, `src/reading_query_view.rs`,
`src/sequencer_view.rs` (callback signatures only).
