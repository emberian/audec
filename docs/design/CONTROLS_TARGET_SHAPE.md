# Controls target shape (cycle 1, lane C1-Controls)

Resolves `ARCHITECTURE_RESIDUE.md` #5 and the mixer/automation rows of
`UX_EXPOSURE_AUDIT.md` (Undo label, `+ insert`/`+ send`/`OUTPUT` receipts,
write-mode button, `+ Channel` pre-adoption).

## Today

`MixerView` (`src/control_views.rs:328`) and `AutomationView` (`:2194`) each
carry a `Box<dyn Backend>` with its own command history, a
`controller_snapshot`, a `callback`, and an `integration_mode`
(`control_actions.rs:241`: `Controller` | `Compatibility`). The live
constructors build a Compatibility view around a `LocalBackend` clone of
the graph and then overwrite four fields to become Controller
(`:377-385`, `:2258-2267`). The backend is consulted in 8 places
(`self.backend.` at `:528,591,1171,1194,2436,2538,3077,3100`), and 23
branches ask which mode the view is in. `Compatibility` has exactly one
live construction: `AutomationView::from_graph(AutomationGraph::new())` in
`src/ui/workbench_editors.rs:277`, the "no lanes yet" fallback. The writer
callback (`set_writer_callback`, `:2379`) has no caller, so the write-mode
button never leaves Read.

## Target

```rust
pub struct MixerView {
    graph: MixerGraph,                       // read-only snapshot from the controller
    render_status: Option<ControlRenderStatus>,
    meter_readings: BTreeMap<BusId, MeterReading>,
    meter_sequence: u64,
    meter_source: Option<PlaybackCohortId>,
    callback: ControlActionCallback,         // always present
    selected_bus: Option<BusId>,
    rename_draft: Option<(BusId, String)>,
    gesture: Option<MixerGesture>,
    next_gesture_series: u64,
    pending: BTreeMap<u64 /*series*/, PendingIntent>, // what was asked, awaiting a receipt
    status: String,
    focus_handle: FocusHandle,
}
```

- **One constructor**: `MixerView::new(graph, target_bus, callback, cx)`.
  The fallback that wanted an editor with no lanes passes
  `AutomationGraph::new()` to the same constructor with a callback that
  still reaches the session; there is no second execution model.
- **Delete**: `MixerBackend`, `AutomationBackend`, `LocalMixerBackend`,
  `SharedMixerBackend`, `LocalAutomationBackend`,
  `SharedAutomationBackend`, `ControlIntegrationMode`, `integration_mode()`,
  `with_compatibility_backend`, `with_backend`, `demo`, `from_graph`,
  `from_shared_graph*`, and every `integration_mode` branch. Tests that
  drove a backend directly (`SharedMixerBackend` fixtures) drive the
  callback and assert on the emitted `ControlAction`s instead; the 23
  `control_views` tests already do this for gestures.
- **Undo/Redo** in both views emit `ControlAction::ProjectUndo` /
  `ProjectRedo` and are labelled "Undo (project)" / "Redo (project)". No
  local history remains. If a mixer-scoped undo is wanted later it is a
  project-controller feature, not a view feature.
- **Receipts**: an edit sets `status` to a *requested* form ("Add channel
  · requested") and records the intent under its gesture series in
  `pending`. `apply_control_receipt(receipt)` (called by the host when the
  controller answers; the host already receives `ControlActionEffect`s)
  replaces it with the committed form ("Channel 4 added · revision 12") or
  the refusal verbatim. Nothing is adopted before the receipt: `+ Channel`
  stops precomputing a bus id.
- **Plugins**: `+ insert` stays visible but labelled by capability:
  "+ insert (plugin hosting not connected in this build)" and its click
  reports exactly that; insert rows show "not rendered" beside
  active/bypassed until plugins are in the render path (cycle 3
  feasibility). No "sent" without a receipt.
- **Automation writer**: the host installs
  `set_writer_callback(Some(cb))` where `cb` lowers `AutomationWriterIntent`
  to the existing `AutomationCommand`s through the session (same path as
  the point edits). If the lane cannot land that in one cycle, the
  write-mode button is removed rather than left stuck on Read.
- **Meters**: `set_meter_snapshot` keeps its audibility check against
  `render_status`; the `Compatibility` arm of the match goes.

## Gate

`cargo test --lib -- 'control_views::' 'control_actions::' --test-threads=1`
plus live: `scripts/live/editors_and_windows.sh` extended with
`audec.editor.mixer` then `status` (no panic, no audio error), and a new
scenario that drives `+ Channel` through a socket `pane` verb if one is
added, else a unit test asserting the requested→committed status sequence
from a receipt.

## Files owned

`src/control_views.rs`, `src/control_actions.rs`, the mixer/automation arms
of `src/ui/workbench_editors.rs` and `src/ui/workbench_publication.rs`,
`src/ui/workbench_events.rs` only where control receipts are delivered.
