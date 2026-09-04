# One reveal identity (cycle 2, lane C2-Reveal, after C2-Channel)

Resolves `ARCHITECTURE_RESIDUE.md` #1 and carries the ledger's
`reverse_navigation` wire, "REVEAL RESULT ↗" in the assets pane, and durable
"Keep finding".

## Today

Twelve types name "a thing you can reveal", with no `From` between any
pair and about ten hand adapters:

| type | where | variants (abridged) |
|---|---|---|
| `ObjectRef` | `object_navigation.rs:118` | Material, Sample, Instrument, Pad, Pattern, PatternOccurrence, AudioClip, Track, Bus, Automation, AutomationOccurrence, Finding, Explanation, Comparison, Reading (777 sites) |
| `WorkspaceReveal` | `object_navigation.rs:274` | Activate, Create(NewWorkspaceView), Retarget, None, Unsupported, UpdateIfVisible |
| `ExplorerTarget` | `explorer_model.rs:119` | Mode, Category, Object(ObjectRef), Empty, MissingObject, StaleSelection, UnsupportedObject, FilterNoMatches, NotSelectable |
| `WorkbenchRevealTarget` | `explanation_workbench_view.rs:195` | Artifact, Evidence(EvidenceRef), Created(CreatedObject), Plan, Execute, Render, Capture, Undo, Cancel |
| `ReadingRevealSubject` | `reading_effect_bridge.rs:80` | Object(ObjectRef), Air(AirSelection) |
| `FocusTarget` | `product_input.rs:80` | ExplorerSurface, ExplorerSearch, ExplorerMode, ExplorerObject, InspectorSurface, InspectorSection, InspectorReveal, Timeline, ArrangementSurface, ArrangementTrack |
| `SelectableId` | `project_selection.rs:42` | Track, Clip, Pattern, Note, Step, AutomationLane, AutomationPoint, MixerBus, Asset, Air |
| `DeprojectionWorkspaceTarget` | `deprojection_workspace_bridge.rs:106` | Object(ObjectRef), View(WorkspaceViewId), Rhythm, Hpss |
| `EditorTarget` (typed) | `workspace_items.rs:41` | Project, Arrangement, Assets, Inspector, Pattern, AutomationLane(id), Mixer, Analysis, Explanation(proposal), Sampler(SamplerTarget) |
| `EditorTarget` (raw u64, serde) | `workspace_document.rs:148` | the same shapes with raw ids plus Render and Extension |
| receipts | `receipt_navigation.rs:268,291,363,949`, `project_reveal.rs:34` | ProjectMutationReceipt, ControlRevealReceipt, ArrangementRevealReceipt, InterpretationRevealReceipt, RevealReceipt |

## Target

Three types, all in `src/object_navigation.rs`:

```rust
/// The only durable identity of a thing in the product. Unchanged.
pub enum ObjectRef { ... }

/// The only way to ask for a reveal.
pub struct RevealRequest {
    pub object: ObjectRef,
    /// Where the asker would like it shown, if it cares.
    pub hint: RevealHint, // Anywhere | InView(WorkspaceViewId) | InspectorOnly | ExplorerOnly
    /// Why the asker wants it (for the receipt's headline and for freshness).
    pub origin: RevealOrigin, // Explorer | Pane(WorkspaceViewId) | Completion { of: CompletionKind } | Socket
}

/// The only answer.
pub struct RevealReceipt {
    pub request: RevealRequest,
    pub resolution: RevealResolution, // Shown { view, location } | Created { view } | Retargeted { view } | Refused(RevealRefusal)
    pub guard: Epoch,                 // from C2-Channel: the project epoch the receipt is valid for
}
pub enum RevealRefusal {
    MissingObject(ObjectRef),
    NoSurfaceCanShow { object: ObjectRef, kind: ObjectKind },
    Stale { requested: Epoch, current: Epoch },
    Unsupported { object: ObjectRef, reason: &'static str },
}
```

- Every surface implements `fn locate(&self, object: ObjectRef) ->
  Option<SurfaceLocation>` and nothing else; the host resolves a
  `RevealRequest` by asking surfaces in hint order and returns the receipt.
- **Delete** `ExplorerTarget::Object/…` duplicates in favour of `ObjectRef`
  plus explorer-local `ExplorerCursor { Mode | Category | Object(ObjectRef) }`
  (the diagnostics variants become `ExplorerDiagnostic`, which already
  exists); `WorkbenchRevealTarget::Artifact/Evidence/Created` lower to
  `ObjectRef` through `reverse_navigation::resolve_reverse_target` (now
  wired), the non-reveal variants (Plan/Execute/Render/Capture/Undo/Cancel)
  move to a `WorkbenchCommand` enum because they are not reveals;
  `ReadingRevealSubject` and `DeprojectionWorkspaceTarget` become
  `RevealRequest`s; `FocusTarget` stays (it is keyboard focus, not
  identity) but its `ExplorerObject`/`InspectorReveal` variants carry
  `ObjectRef`; `SelectableId` gains `From<SelectableId> for ObjectRef` and
  `TryFrom<ObjectRef> for SelectableId` so selection and reveal agree.
- **Merge the two `EditorTarget`s**: the typed one in `workspace_items.rs`
  is the domain type; `workspace_document.rs`'s raw form becomes
  `EditorTargetDto` used only by the document codec, with the existing
  `kind_accepts_target` rule ported once. The 45-line hand map in `ui.rs`
  (~2112) and the duplicated `object_from_promoted_created` go.
- **Receipts**: `ProjectMutationReceipt` (a mutation happened) stays;
  `ControlRevealReceipt`, `ArrangementRevealReceipt`,
  `InterpretationRevealReceipt` become `RevealReceipt` with the
  domain-specific payload in `resolution`'s `location`.
- **Keep finding** becomes durable here: `keep_reverse_finding` publishes
  the finding into project state (the kept-findings set the Explorer
  already lists) through a command envelope, so it survives reload; the
  receipt's resolution is `Shown` in the Explorer.
- **Assets pane REVEAL ↗** and the sampler's use the same `RevealRequest`;
  a refusal reaches the pane's status verbatim.

## Gate

Full suite; `scripts/live/*.sh`; new flow tests: reveal from an explanation
with an artifact-scoped evidence lands on the finding or names
`NoSurfaceCanShow`; keep a finding, save, reopen, it is listed; a reveal
requested at epoch N and answered at N+1 is `Stale`. Grep gate: exactly one
`enum EditorTarget` in `src/`, zero `WorkbenchRevealTarget`, zero
`ReadingRevealSubject`, zero `DeprojectionWorkspaceTarget`.

## Files owned

`src/object_navigation.rs`, `src/explorer_model.rs`,
`src/explanation_workbench_view.rs`, `src/reading_query_workbench.rs`,
`src/reading_effect_bridge.rs`, `src/product_input.rs`,
`src/project_selection.rs`, `src/deprojection_workspace_bridge.rs`,
`src/workspace_items.rs`, `src/workspace_document.rs`,
`src/reverse_navigation.rs`, `src/reverse_surface_adapter.rs`,
`src/receipt_navigation.rs`, `src/project_reveal.rs`, `src/ui/helpers.rs`,
`src/ui/workbench_reading.rs`, `src/ui/shell_explorer.rs`, `src/asset_view.rs`
(reveal call only), the reveal arm of `src/ui.rs` (~2112) by agreement with
C2-Channel (which must have landed first).
