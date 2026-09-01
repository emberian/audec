//! Controller-facing intents emitted by audec's sample browser and pad editor.
//!
//! These values are deliberately project-model agnostic. Views may select and
//! inspect authoritative [`SampleKit`](crate::sample_kit::SampleKit) snapshots,
//! but every audible or authored consequence crosses this typed callback seam.
//! A controller is responsible for validating revisions, allocating IDs,
//! constructing commands, and publishing constructive plans atomically.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use crate::assets::{AssetFrameRange, AssetId, SampleFrames};
use crate::mixer::BusId;
use crate::sample_kit::{KitId, PadId, SampleTargetRef, ZoneId};
use crate::sample_material::{
    SampleMaterialProvenance, ScopedEvidenceRef, SourceMaterialRef, VirtualSliceRef,
};
use crate::sequencer::PatternId;
use crate::ui_drag::DropIntent;

#[path = "sample_runtime.rs"]
mod runtime;
#[allow(unused_imports)]
pub use runtime::{
    resolve_sample_audition, ResolvedSamplePreview, SamplePreviewClipRef, SamplePreviewCommand,
    SamplePreviewEffect, SamplePreviewError, SamplePreviewState, SamplePreviewTarget,
    SamplePreviewToken,
};

#[path = "sampler_pane.rs"]
mod pane;
#[allow(unused_imports)]
pub use pane::{
    ChopResultSelection, SamplerGateId, SamplerGatePress, SamplerInstrumentProjection,
    SamplerKitProjection, SamplerPadProjection, SamplerPaneError, SamplerPaneModel,
    SamplerPaneSelection, SamplerZoneProjection, SAMPLER_KEYBOARD_BANK_SIZE, SAMPLER_KEYBOARD_KEYS,
};

#[path = "sample_workflow.rs"]
mod workflow;
#[allow(unused_imports)]
pub use workflow::{
    named_sample_library, NamedSampleAsset, SampleInstrumentDestination, SampleSpanOrigin,
    SampleWorkflowActionDescriptor, SampleWorkflowAfter, SampleWorkflowCommand,
    SampleWorkflowLanding, SampleWorkflowNextAction, SampleWorkflowPlanIntent,
    SampleWorkflowPresentation, SampleWorkflowProduct, SampleWorkflowReceipt, SampleWorkflowSpec,
    SampleWorkflowValidationError, EXPECTED_SAMPLE_WORKFLOW_ACTIONS,
};

#[path = "material_pool.rs"]
mod material_pool;
#[allow(unused_imports)]
pub use material_pool::{
    MaterialPoolError, MaterialPoolItemId, MaterialPoolItemRef, MaterialPoolSnapshot,
};

/// The exact source material under the browser's playhead or range selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SampleSelection {
    pub asset: AssetId,
    pub source_range: Option<AssetFrameRange>,
}

impl SampleSelection {
    pub const fn whole_asset(asset: AssetId) -> Self {
        Self {
            asset,
            source_range: None,
        }
    }

    pub const fn material(self) -> SourceMaterialRef {
        match self.source_range {
            Some(source_range) => SourceMaterialRef::VirtualSlice(VirtualSliceRef {
                source_asset: self.asset,
                source_range,
            }),
            None => SourceMaterialRef::Asset(self.asset),
        }
    }
}

/// The audition engine receives semantic starts/stops rather than UI events.
/// A one-shot may naturally finish without a later stop; gate mode must stop
/// when its pointer/key is released or the view loses focus.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SampleAuditionIntent {
    MaterialOneShot {
        material: SourceMaterialRef,
        velocity: f32,
    },
    PadGate {
        kit: KitId,
        pad: PadId,
        velocity: f32,
        pressed: bool,
    },
}

/// How a selected span should become playable zones and initial beat events.
#[derive(Clone, Debug, PartialEq)]
pub enum SampleChopIntent {
    OneShot,
    EqualSlices {
        count: u16,
    },
    DetectOnsets {
        analyzer: String,
        sensitivity: f32,
        minimum_gap_frames: u64,
    },
}

impl SampleChopIntent {
    pub fn is_previewable(&self) -> bool {
        matches!(self, Self::DetectOnsets { .. })
    }
}

/// An ephemeral, controller-computed onset preview. It is not authored kit
/// state and must be requested again if its exact source selection changes.
#[derive(Clone, Debug, PartialEq)]
pub struct OnsetChopPreview {
    pub source: SampleSelection,
    pub analyzer: String,
    /// Sorted decoded-frame boundaries strictly inside the selected material.
    pub boundaries: Vec<SampleFrames>,
    pub confidence: Option<f32>,
    pub diagnostic: Option<String>,
}

impl OnsetChopPreview {
    pub fn is_for(self: &Self, selection: SampleSelection) -> bool {
        self.source == selection
    }

    pub fn is_valid(&self) -> bool {
        if self.analyzer.trim().is_empty()
            || self
                .confidence
                .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
            || self.boundaries.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return false;
        }
        match self.source.source_range {
            Some(range) => self
                .boundaries
                .iter()
                .all(|boundary| *boundary > range.start && *boundary < range.end),
            None => self.boundaries.iter().all(|boundary| boundary.0 > 0),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChopPreviewIntent {
    pub source: SampleSelection,
    pub chop: SampleChopIntent,
}

/// Stable semantic target for a dynamic sampler workspace item.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SamplerTarget {
    NewKit,
    Kit(KitId),
    NewPad { kit: KitId },
    Pad { kit: KitId, pad: PadId },
}

impl SamplerTarget {
    pub const fn kit(self) -> Option<KitId> {
        match self {
            Self::NewKit => None,
            Self::Kit(kit) | Self::NewPad { kit } | Self::Pad { kit, .. } => Some(kit),
        }
    }

    pub const fn pad(self) -> Option<PadId> {
        match self {
            Self::Pad { pad, .. } => Some(pad),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SamplerViewDisposition {
    RetargetCurrent,
    OpenNew,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SamplerWorkspaceIntent {
    pub target: SamplerTarget,
    pub disposition: SamplerViewDisposition,
}

impl Default for SampleChopIntent {
    fn default() -> Self {
        Self::EqualSlices { count: 8 }
    }
}

/// Where a constructive adapter should publish the pads it creates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleKitDestination {
    NewKit,
    ExistingKit { kit: KitId, expected_revision: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MakeBeatResultFocus {
    Stay,
    Sampler(SamplerViewDisposition),
    PatternEditor,
    Arrangement,
}

/// Complete user intent behind “Sample selection & make beat”.
///
/// Timing, onset evidence, ID allocation, material fingerprints, and the
/// resulting `ConstructiveEditPlan` remain controller/constructive concerns.
#[derive(Clone, Debug, PartialEq)]
pub struct MakeBeatIntent {
    pub source: SampleSelection,
    pub chop: SampleChopIntent,
    pub kit: SampleKitDestination,
    pub target_bus: Option<BusId>,
    pub bars: u16,
    pub quantize_ticks: u64,
    /// Applied only after the constructive edit publishes successfully.
    pub result_focus: MakeBeatResultFocus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ZoneEditTarget {
    pub kit: KitId,
    pub pad: PadId,
    pub zone: ZoneId,
    pub expected_revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleLoopMode {
    Forward,
    PingPong,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SampleEnvelopeIntent {
    pub attack_frames: u64,
    pub decay_frames: u64,
    pub sustain: f32,
    pub release_frames: u64,
}

impl SampleEnvelopeIntent {
    pub const fn percussive() -> Self {
        Self {
            attack_frames: 64,
            decay_frames: 4_800,
            sustain: 0.0,
            release_frames: 1_200,
        }
    }

    pub fn is_valid(self) -> bool {
        self.sustain.is_finite() && (0.0..=1.0).contains(&self.sustain)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ZoneEditIntent {
    Trim {
        target: ZoneEditTarget,
        source_range: AssetFrameRange,
    },
    SetLoop {
        target: ZoneEditTarget,
        enabled: bool,
        source_range: Option<AssetFrameRange>,
        mode: SampleLoopMode,
    },
    SetEnvelope {
        target: ZoneEditTarget,
        envelope: SampleEnvelopeIntent,
    },
    SetPlayback {
        target: ZoneEditTarget,
        gain_db: f32,
        pan: f32,
        tuning_cents: f32,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SamplerDiagnosticSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SamplerDiagnostic {
    pub severity: SamplerDiagnosticSeverity,
    pub code: String,
    pub message: String,
    pub target: Option<SamplerTarget>,
}

/// A stable target for provenance/evidence disclosure in an inspector pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SampleInspectTarget {
    Material(SourceMaterialRef),
    Zone { kit: KitId, zone: ZoneId },
    Provenance(SampleMaterialProvenance),
    Evidence(ScopedEvidenceRef),
}

/// All semantic output from sampler-facing views.
#[derive(Clone, Debug, PartialEq)]
pub enum SampleAction {
    Audition(SampleAuditionIntent),
    /// A drag/drop interpretation retaining its exact source range.
    ApplyDrop(DropIntent),
    SetKitOutput {
        kit: KitId,
        bus: BusId,
        expected_revision: u64,
    },
    SetPadChoke {
        kit: KitId,
        pad: PadId,
        choke_group: Option<NonZeroU32>,
        expected_revision: u64,
    },
    RemoveZone {
        kit: KitId,
        pad: PadId,
        zone: ZoneId,
        expected_revision: u64,
    },
    Inspect(SampleInspectTarget),
    PreviewChop(ChopPreviewIntent),
    EditZone(ZoneEditIntent),
    Workspace(SamplerWorkspaceIntent),
    MakeBeat(MakeBeatIntent),
}

/// The musician-visible class of a request. This deliberately does not mirror
/// controller commands: it is small enough for a reusable status component and
/// stable across different session adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleActionKind {
    OneShot,
    PadAudition,
    ChopPreview,
    MakeBeat,
    Edit,
    Inspect,
    Workspace,
}

/// Scheduling contract for the session adapter. Background-planned actions
/// scan or extract arbitrarily long PCM and must not run on the GPUI ticker.
/// `Immediate` means the current controller path is used directly; it is not a
/// general hard-realtime guarantee for future action variants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleActionExecutionClass {
    Immediate,
    BackgroundPlanning,
}

impl SampleAction {
    pub const fn kind(&self) -> SampleActionKind {
        match self {
            Self::Audition(SampleAuditionIntent::MaterialOneShot { .. }) => {
                SampleActionKind::OneShot
            }
            Self::Audition(SampleAuditionIntent::PadGate { .. }) => SampleActionKind::PadAudition,
            Self::PreviewChop(_) => SampleActionKind::ChopPreview,
            Self::MakeBeat(_) => SampleActionKind::MakeBeat,
            Self::Inspect(_) => SampleActionKind::Inspect,
            Self::Workspace(_) => SampleActionKind::Workspace,
            Self::ApplyDrop(_)
            | Self::SetKitOutput { .. }
            | Self::SetPadChoke { .. }
            | Self::RemoveZone { .. }
            | Self::EditZone(_) => SampleActionKind::Edit,
        }
    }

    pub const fn execution_class(&self) -> SampleActionExecutionClass {
        match self {
            Self::PreviewChop(_) | Self::MakeBeat(_) => {
                SampleActionExecutionClass::BackgroundPlanning
            }
            Self::Audition(_)
            | Self::ApplyDrop(_)
            | Self::SetKitOutput { .. }
            | Self::SetPadChoke { .. }
            | Self::RemoveZone { .. }
            | Self::Inspect(_)
            | Self::EditZone(_)
            | Self::Workspace(_) => SampleActionExecutionClass::Immediate,
        }
    }

    pub fn result_provenance(&self) -> Option<SampleResultProvenance> {
        match self {
            Self::Audition(SampleAuditionIntent::MaterialOneShot { material, .. }) => {
                Some(SampleResultProvenance::Material(*material))
            }
            Self::PreviewChop(intent) => Some(SampleResultProvenance::Selection {
                source: intent.source,
                chop: Some(intent.chop.clone()),
            }),
            Self::MakeBeat(intent) => Some(SampleResultProvenance::Selection {
                source: intent.source,
                chop: Some(intent.chop.clone()),
            }),
            Self::Inspect(SampleInspectTarget::Material(material)) => {
                Some(SampleResultProvenance::Material(*material))
            }
            Self::Inspect(SampleInspectTarget::Provenance(provenance)) => {
                Some(SampleResultProvenance::Authored(provenance.clone()))
            }
            _ => None,
        }
    }
}

/// Where a successful constructive result wants the workspace to move next.
/// The view can retarget sampler results itself; pattern focus remains a typed
/// host callback because the sampler never owns editor navigation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleResultFocus {
    Stay,
    Kit(KitId),
    Pad {
        kit: KitId,
        pad: PadId,
    },
    Pattern(PatternId),
    Arrangement {
        arrangement_clip: crate::arrangement::ClipId,
        sequencer_clip: Option<crate::sequencer::PatternClipId>,
        pattern: Option<PatternId>,
    },
    Sampler {
        target: SamplerTarget,
        disposition: SamplerViewDisposition,
    },
}

impl SampleResultFocus {
    pub const fn sampler_retarget(self) -> Option<SamplerTarget> {
        match self {
            Self::Kit(kit) => Some(SamplerTarget::Kit(kit)),
            Self::Pad { kit, pad } => Some(SamplerTarget::Pad { kit, pad }),
            Self::Sampler {
                target,
                disposition: SamplerViewDisposition::RetargetCurrent,
            } => Some(target),
            Self::Stay
            | Self::Pattern(_)
            | Self::Arrangement { .. }
            | Self::Sampler {
                disposition: SamplerViewDisposition::OpenNew,
                ..
            } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SampleResultProvenance {
    Material(SourceMaterialRef),
    Selection {
        source: SampleSelection,
        chop: Option<SampleChopIntent>,
    },
    Authored(SampleMaterialProvenance),
}

/// Controller-neutral publication receipt. The session adapter produces this
/// only after the aggregate publication succeeds, so a success badge always
/// names the durable revision and the exact authored identities.
#[derive(Clone, Debug, PartialEq)]
pub struct SamplePublishedResult {
    pub revision: u64,
    pub kit: KitId,
    pub created_pads: Vec<PadId>,
    pub created_zones: Vec<SampleTargetRef>,
    pub pad: Option<PadId>,
    pub pattern: Option<PatternId>,
    pub sequencer_clip: Option<crate::sequencer::PatternClipId>,
    pub arrangement_clip: Option<crate::arrangement::ClipId>,
    pub arrangement_track: Option<crate::arrangement::TrackId>,
    pub output_bus: Option<BusId>,
    pub focus: SampleResultFocus,
    pub provenance: Option<SampleResultProvenance>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SampleViewOutcome {
    Audition(SampleAuditionIntent),
    ChopPreview(OnsetChopPreview),
    Published(SamplePublishedResult),
    Acknowledged {
        kind: SampleActionKind,
        message: String,
        provenance: Option<SampleResultProvenance>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SampleActionError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl SampleActionError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
        }
    }

    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }
}

pub type SampleActionResult = Result<SampleViewOutcome, SampleActionError>;

/// View-local correlation key for a session adapter request. The host routes a
/// later result back to the same view instance with this exact value.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SampleRequestId(pub u64);

#[derive(Clone, Debug, PartialEq)]
pub struct SampleActionRequest {
    pub id: SampleRequestId,
    pub action: SampleAction,
}

/// Immediate callback receipt. Expensive analysis may be accepted and finish
/// later, but acceptance is always reflected by visible in-flight state.
#[derive(Clone, Debug, PartialEq)]
pub enum SampleDispatchReceipt {
    Completed(SampleActionResult),
    Accepted {
        request_id: SampleRequestId,
        kind: SampleActionKind,
        provenance: Option<SampleResultProvenance>,
    },
}

impl SampleDispatchReceipt {
    pub fn accepted(request: &SampleActionRequest) -> Self {
        Self::Accepted {
            request_id: request.id,
            kind: request.action.kind(),
            provenance: request.action.result_provenance(),
        }
    }
}

pub type SampleActionCallback =
    Arc<dyn Fn(SampleActionRequest) -> SampleDispatchReceipt + Send + Sync + 'static>;

pub type SampleFocusCallback = Arc<dyn Fn(SampleResultFocus) + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SampleFeedbackTone {
    #[default]
    Idle,
    Pending,
    Success,
    Error,
}

/// Reusable presentation state shared by the browser selection actions and the
/// sampler editor. It carries no project truth and is fully replaced by each
/// callback receipt or correlated result.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SampleActionFeedback {
    pub tone: SampleFeedbackTone,
    pub kind: Option<SampleActionKind>,
    pub headline: String,
    pub detail: Option<String>,
    pub provenance: Option<SampleResultProvenance>,
}

impl SampleActionFeedback {
    pub fn pending(request: &SampleActionRequest) -> Self {
        Self {
            tone: SampleFeedbackTone::Pending,
            kind: Some(request.action.kind()),
            headline: format!("{} in progress", action_kind_label(request.action.kind())),
            detail: Some(format!("Request {} · waiting for session", request.id.0)),
            provenance: request.action.result_provenance(),
        }
    }

    pub fn disconnected(action: &SampleAction) -> Self {
        Self {
            tone: SampleFeedbackTone::Error,
            kind: Some(action.kind()),
            headline: "Sampling action is not connected".into(),
            detail: Some("Open a project session and try again".into()),
            provenance: action.result_provenance(),
        }
    }

    pub fn from_result(action: &SampleAction, result: &SampleActionResult) -> Self {
        match result {
            Err(error) => Self {
                tone: SampleFeedbackTone::Error,
                kind: Some(action.kind()),
                headline: error.message.clone(),
                detail: Some(format!(
                    "{}{}",
                    error.code,
                    if error.retryable {
                        " · retry available"
                    } else {
                        ""
                    }
                )),
                provenance: action.result_provenance(),
            },
            Ok(SampleViewOutcome::Audition(intent)) => Self {
                tone: SampleFeedbackTone::Success,
                kind: Some(action.kind()),
                headline: audition_feedback(*intent),
                detail: None,
                provenance: action.result_provenance(),
            },
            Ok(SampleViewOutcome::ChopPreview(preview)) => Self {
                tone: SampleFeedbackTone::Success,
                kind: Some(SampleActionKind::ChopPreview),
                headline: format!("Onset preview · {} boundaries", preview.boundaries.len()),
                detail: preview.diagnostic.clone().or_else(|| {
                    preview
                        .confidence
                        .map(|confidence| format!("{:.0}% confidence", confidence * 100.0))
                }),
                provenance: Some(SampleResultProvenance::Selection {
                    source: preview.source,
                    chop: match action {
                        SampleAction::PreviewChop(intent) => Some(intent.chop.clone()),
                        _ => None,
                    },
                }),
            },
            Ok(SampleViewOutcome::Published(receipt)) => Self {
                tone: SampleFeedbackTone::Success,
                kind: Some(action.kind()),
                headline: publication_feedback(receipt),
                detail: Some(format!("Published revision {}", receipt.revision)),
                provenance: receipt
                    .provenance
                    .clone()
                    .or_else(|| action.result_provenance()),
            },
            Ok(SampleViewOutcome::Acknowledged {
                kind,
                message,
                provenance,
            }) => Self {
                tone: SampleFeedbackTone::Success,
                kind: Some(*kind),
                headline: message.clone(),
                detail: None,
                provenance: provenance.clone().or_else(|| action.result_provenance()),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingSampleAction {
    pub request: SampleActionRequest,
}

/// Pure request correlation and feedback state. It tracks work already
/// accepted by the host, never a queue of actions waiting to be dispatched.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SampleActionTracker {
    next_request_id: u64,
    in_flight: BTreeMap<SampleRequestId, PendingSampleAction>,
    feedback: SampleActionFeedback,
}

impl SampleActionTracker {
    pub fn prepare(&mut self, action: SampleAction) -> SampleActionRequest {
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        SampleActionRequest {
            id: SampleRequestId(self.next_request_id),
            action,
        }
    }

    pub fn accept(
        &mut self,
        request: SampleActionRequest,
        receipt_id: SampleRequestId,
        kind: SampleActionKind,
        provenance: Option<SampleResultProvenance>,
    ) -> Result<(), SampleActionError> {
        if request.id != receipt_id || request.action.kind() != kind {
            let error = SampleActionError::new(
                "sample.request-id-mismatch",
                format!(
                    "Session accepted request {} as {:?}, expected {} as {:?}",
                    receipt_id.0,
                    kind,
                    request.id.0,
                    request.action.kind()
                ),
            );
            self.feedback = SampleActionFeedback::from_result(&request.action, &Err(error.clone()));
            return Err(error);
        }
        self.feedback = SampleActionFeedback::pending(&request);
        if provenance.is_some() {
            self.feedback.provenance = provenance;
        }
        self.in_flight
            .insert(request.id, PendingSampleAction { request });
        Ok(())
    }

    pub fn complete_now(&mut self, action: &SampleAction, result: &SampleActionResult) {
        self.feedback = SampleActionFeedback::from_result(action, result);
    }

    pub fn complete(
        &mut self,
        request_id: SampleRequestId,
        result: &SampleActionResult,
    ) -> Result<SampleAction, SampleActionError> {
        let Some(pending) = self.in_flight.remove(&request_id) else {
            return Err(SampleActionError::new(
                "sample.unknown-request",
                format!("No in-flight sampling request {}", request_id.0),
            ));
        };
        self.feedback = SampleActionFeedback::from_result(&pending.request.action, result);
        Ok(pending.request.action)
    }

    pub fn disconnect(&mut self, action: &SampleAction) {
        self.feedback = SampleActionFeedback::disconnected(action);
    }

    pub fn feedback(&self) -> &SampleActionFeedback {
        &self.feedback
    }

    pub fn pending(&self) -> impl Iterator<Item = &PendingSampleAction> {
        self.in_flight.values()
    }

    pub fn pending_count(&self) -> usize {
        self.in_flight.len()
    }
}

fn action_kind_label(kind: SampleActionKind) -> &'static str {
    match kind {
        SampleActionKind::OneShot => "One-shot audition",
        SampleActionKind::PadAudition => "Pad audition",
        SampleActionKind::ChopPreview => "Onset preview",
        SampleActionKind::MakeBeat => "Make beat",
        SampleActionKind::Edit => "Sampler edit",
        SampleActionKind::Inspect => "Inspector request",
        SampleActionKind::Workspace => "Workspace focus",
    }
}

pub fn sample_result_provenance_label(provenance: &SampleResultProvenance) -> String {
    match provenance {
        SampleResultProvenance::Material(material) => match material {
            SourceMaterialRef::Asset(asset) => format!("Asset {} · full source", asset.0),
            SourceMaterialRef::VirtualSlice(slice) => format!(
                "Asset {} · exact frames {}–{}",
                slice.source_asset.0, slice.source_range.start.0, slice.source_range.end.0
            ),
        },
        SampleResultProvenance::Selection { source, chop } => {
            let range = source.source_range.map_or_else(
                || "full source".into(),
                |range| format!("exact frames {}–{}", range.start.0, range.end.0),
            );
            match chop {
                Some(SampleChopIntent::OneShot) => {
                    format!("Asset {} · {range} · one shot", source.asset.0)
                }
                Some(SampleChopIntent::EqualSlices { count }) => {
                    format!("Asset {} · {range} · {count} equal slices", source.asset.0)
                }
                Some(SampleChopIntent::DetectOnsets { analyzer, .. }) => {
                    format!("Asset {} · {range} · onset {analyzer}", source.asset.0)
                }
                None => format!("Asset {} · {range}", source.asset.0),
            }
        }
        SampleResultProvenance::Authored(provenance) => match provenance {
            SampleMaterialProvenance::ExistingAsset => "Existing project asset".into(),
            SampleMaterialProvenance::ManualSelection => "Manual exact selection".into(),
            SampleMaterialProvenance::OnsetChop { analyzer, evidence } => {
                format!("Onset chop · {analyzer} · {} evidence", evidence.len())
            }
            SampleMaterialProvenance::Deprojection { proposal, evidence } => format!(
                "Deprojection {} · {} evidence",
                proposal.local,
                evidence.len()
            ),
            SampleMaterialProvenance::Consolidated(record) => format!(
                "Consolidated asset {} · frames {}–{}",
                record.derived_from.source_asset.0,
                record.derived_from.source_range.start.0,
                record.derived_from.source_range.end.0
            ),
        },
    }
}

fn audition_feedback(intent: SampleAuditionIntent) -> String {
    match intent {
        SampleAuditionIntent::MaterialOneShot { material, .. } => {
            format!("Auditioning asset {}", material.asset_id().0)
        }
        SampleAuditionIntent::PadGate { pad, pressed, .. } if pressed => {
            format!("Auditioning pad {}", pad.get())
        }
        SampleAuditionIntent::PadGate { pad, .. } => format!("Released pad {}", pad.get()),
    }
}

fn publication_feedback(receipt: &SamplePublishedResult) -> String {
    match (receipt.pad, receipt.pattern) {
        (Some(pad), Some(pattern)) => format!(
            "Created kit {} · pad {} · pattern {}",
            receipt.kit.get(),
            pad.get(),
            pattern.get()
        ),
        (Some(pad), None) => {
            format!("Updated kit {} · pad {}", receipt.kit.get(), pad.get())
        }
        (None, Some(pattern)) => {
            format!(
                "Created kit {} · pattern {}",
                receipt.kit.get(),
                pattern.get()
            )
        }
        (None, None) => format!("Updated kit {}", receipt.kit.get()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::SampleFrames;

    #[test]
    fn selection_material_preserves_an_exact_half_open_range() {
        let range = AssetFrameRange::new(SampleFrames(120), SampleFrames(960)).unwrap();
        let selection = SampleSelection {
            asset: AssetId(7),
            source_range: Some(range),
        };

        assert_eq!(
            selection.material(),
            SourceMaterialRef::VirtualSlice(VirtualSliceRef {
                source_asset: AssetId(7),
                source_range: range,
            })
        );
    }

    #[test]
    fn sampler_targets_retain_typed_kit_and_pad_identity() {
        let target = SamplerTarget::Pad {
            kit: KitId::from_raw(4),
            pad: PadId::from_raw(9),
        };
        assert_eq!(target.kit(), Some(KitId::from_raw(4)));
        assert_eq!(target.pad(), Some(PadId::from_raw(9)));
        assert_eq!(SamplerTarget::NewKit.kit(), None);
    }

    #[test]
    fn onset_preview_is_scoped_to_the_exact_selection() {
        let source = SampleSelection {
            asset: AssetId(3),
            source_range: Some(AssetFrameRange::new(SampleFrames(10), SampleFrames(100)).unwrap()),
        };
        let preview = OnsetChopPreview {
            source,
            analyzer: "test-onset".into(),
            boundaries: vec![SampleFrames(30), SampleFrames(60)],
            confidence: Some(0.9),
            diagnostic: None,
        };
        assert!(preview.is_for(source));
        assert!(preview.is_valid());
        assert!(!preview.is_for(SampleSelection::whole_asset(AssetId(3))));

        let invalid = OnsetChopPreview {
            boundaries: vec![SampleFrames(60), SampleFrames(30)],
            ..preview
        };
        assert!(!invalid.is_valid());
    }

    #[test]
    fn envelope_validation_rejects_non_normalized_sustain() {
        assert!(SampleEnvelopeIntent::percussive().is_valid());
        assert!(!SampleEnvelopeIntent {
            sustain: 1.2,
            ..SampleEnvelopeIntent::percussive()
        }
        .is_valid());
    }

    #[test]
    fn accepted_actions_are_visible_and_complete_by_exact_request_id() {
        let action = SampleAction::Audition(SampleAuditionIntent::MaterialOneShot {
            material: SourceMaterialRef::Asset(AssetId(8)),
            velocity: 0.8,
        });
        let mut tracker = SampleActionTracker::default();
        let request = tracker.prepare(action.clone());
        let SampleDispatchReceipt::Accepted {
            request_id,
            kind,
            provenance,
        } = SampleDispatchReceipt::accepted(&request)
        else {
            unreachable!()
        };
        tracker
            .accept(request, request_id, kind, provenance)
            .unwrap();

        assert_eq!(tracker.pending_count(), 1);
        assert_eq!(tracker.feedback().tone, SampleFeedbackTone::Pending);
        assert!(!tracker.feedback().headline.is_empty());

        let result = Ok(SampleViewOutcome::Audition(
            SampleAuditionIntent::MaterialOneShot {
                material: SourceMaterialRef::Asset(AssetId(8)),
                velocity: 0.8,
            },
        ));
        assert_eq!(tracker.complete(request_id, &result).unwrap(), action);
        assert_eq!(tracker.pending_count(), 0);
        assert_eq!(tracker.feedback().tone, SampleFeedbackTone::Success);
    }

    #[test]
    fn stale_completion_does_not_replace_current_feedback() {
        let mut tracker = SampleActionTracker::default();
        let action = SampleAction::Workspace(SamplerWorkspaceIntent {
            target: SamplerTarget::NewKit,
            disposition: SamplerViewDisposition::OpenNew,
        });
        tracker.complete_now(
            &action,
            &Ok(SampleViewOutcome::Acknowledged {
                kind: SampleActionKind::Workspace,
                message: "Opened sampler".into(),
                provenance: None,
            }),
        );
        let before = tracker.feedback().clone();
        assert!(tracker
            .complete(
                SampleRequestId(999),
                &Err(SampleActionError::new("late", "Late result"))
            )
            .is_err());
        assert_eq!(tracker.feedback(), &before);
    }

    #[test]
    fn publication_focus_only_retargets_the_current_sampler_when_requested() {
        let kit = KitId::from_raw(5);
        let pad = PadId::from_raw(7);
        assert_eq!(
            SampleResultFocus::Pad { kit, pad }.sampler_retarget(),
            Some(SamplerTarget::Pad { kit, pad })
        );
        assert_eq!(
            SampleResultFocus::Sampler {
                target: SamplerTarget::Kit(kit),
                disposition: SamplerViewDisposition::OpenNew,
            }
            .sampler_retarget(),
            None
        );
        assert_eq!(
            SampleResultFocus::Pattern(PatternId::from_raw(3)).sampler_retarget(),
            None
        );
    }

    #[test]
    fn pcm_scanning_actions_are_explicitly_background_planned() {
        let source = SampleSelection::whole_asset(AssetId(1));
        assert_eq!(
            SampleAction::PreviewChop(ChopPreviewIntent {
                source,
                chop: SampleChopIntent::DetectOnsets {
                    analyzer: "test".into(),
                    sensitivity: 0.5,
                    minimum_gap_frames: 1,
                },
            })
            .execution_class(),
            SampleActionExecutionClass::BackgroundPlanning
        );
        assert_eq!(
            SampleAction::Audition(SampleAuditionIntent::MaterialOneShot {
                material: source.material(),
                velocity: 1.0,
            })
            .execution_class(),
            SampleActionExecutionClass::Immediate
        );
    }
}
