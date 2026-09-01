//! Atomic application of an explicitly selected reconstruction proposal.
//!
//! Reconstruction identities are analytic identities, not instrument names.
//! This bridge therefore creates anonymous, editable DAW material and returns
//! typed provenance bindings.  Where a constructive domain cannot express a
//! proposal losslessly, the exact observation is retained in the arrangement
//! or the application metadata and a typed diagnostic is emitted.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::arrangement::{self, ArrangementEditor, Frame, FrameRange, SourceRange, TrackKind};
use crate::assets::{self, AssetAvailability, AssetFrameRange, AssetUsageOwner, SampleFrames};
use crate::automation::{
    self, ParameterAddress, ParameterDescriptor, ParameterUnit, SegmentShape, SmoothingPolicy,
    TimeDomain, TimePosition, ValueMapping,
};
use crate::constructive::{
    self, ConstructiveCause, ConstructiveEditPlan, ConstructiveFocus, KitMutation,
    PatternPlacementIntent, PatternSeed, PlannedPattern, PlannedPatternId, PlannedStep,
};
use crate::daw_project::{
    BridgeError, DawProject, PreparedProjectTransaction, ProjectDomain, ProjectState,
};
use crate::mixer;
use crate::reconstruction::{
    self, AutomationInterpolation, AutomationProposal, AutomationProposalId, AutomationTarget,
    EditableTrackProposal, EffectProposal, LatentComponentProposal, ModulationProposal,
    PitchChoice, PitchChoiceSelection, PitchedEventId, PitchedRenderingModel,
    ReconstructionEvidence, ReconstructionEvidenceId, ReconstructionProposal,
    ReconstructionProposalId, ReconstructionSelection, ReconstructionSet, ReconstructionTrackId,
    ReconstructionTrackKind, ResidualRenderMode, SampleSliceId, SourceFrameRange, TriggerId,
};
use crate::sample_kit::{
    KitId, SampleKit, SamplePad, SampleRouteIntent, SampleTargetRef, SampleZone,
};
use crate::sample_material::{
    DerivationScope, SampleMaterialProvenance, ScopedEvidenceRef, ScopedProposalRef,
    SourceMaterialRef, VirtualSliceRef,
};
use crate::sequencer::{
    self, Articulation, BeatDuration, BeatTime, ExpressionPoint, NoteEvent, NotePattern, NotePitch,
    PatternContent, PatternDefinition, PerNoteExpression, SequencerCommand,
};

/// Severity is deliberately separate from the diagnostic code.  Warnings
/// preserve a lossy boundary while still allowing the safe fallback to apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplicationDiagnosticSeverity {
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplicationDiagnosticCode {
    SourceAssetUnavailable,
    SampleSliceTargetRequiresSamplerZone,
    StepCollision,
    MicrotimingRoundedToTick,
    MissingMusicalTiming,
    PitchChoiceRequired,
    AutomationDestinationUnresolved,
    EmptyAutomation,
    ModulationImplementationUnresolved,
    ModulationImplementationRequiresDsp,
    EffectImplementationUnresolved,
    EffectImplementationRequiresDsp,
    LatentComponentRequiresRenderer,
    ResidualSubtractionUnavailable,
    TempoPhaseRequiresTimelineOffset,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplicationDiagnostic {
    pub severity: ApplicationDiagnosticSeverity,
    pub code: ApplicationDiagnosticCode,
    pub path: String,
    pub message: String,
}

impl ApplicationDiagnostic {
    fn warning(
        code: ApplicationDiagnosticCode,
        path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: ApplicationDiagnosticSeverity::Warning,
            code,
            path: path.into(),
            message: message.into(),
        }
    }
}

/// A source citation copied into the application receipt.  The media-pool
/// asset remains immutable; the range is the proposed reusable slice.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedSliceBinding {
    pub source_asset: assets::AssetId,
    pub source: SourceFrameRange,
    pub anonymous_family_id: Option<usize>,
    pub evidence: Vec<ReconstructionEvidenceId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppliedTrackBinding {
    pub arrangement_track: arrangement::TrackId,
    pub mixer_bus: mixer::BusId,
    pub analytic_kind: ReconstructionTrackKind,
    pub evidence: Vec<ReconstructionEvidenceId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppliedTriggerBinding {
    /// Exact, source-ranged fallback. It remains inspectable even when a
    /// playable step abstraction mutes it to avoid rendering the hit twice.
    pub audio_clip: arrangement::ClipId,
    pub slice: SampleSliceId,
    /// Present when an editable step abstraction was also authored.
    pub step_pattern: Option<sequencer::PatternId>,
    pub step_lane: Option<sequencer::StepLaneId>,
    pub step_index: Option<u32>,
    pub gain_linear: f32,
    pub original_micro_offset_frames: i64,
    pub evidence: Vec<ReconstructionEvidenceId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppliedPatternBinding {
    pub sequencer_pattern: sequencer::PatternId,
    pub arrangement_pattern: arrangement::PatternId,
    /// Exact placed occurrence when this promotion authored one. Pitched
    /// pattern extraction may intentionally remain library-only.
    pub occurrence: Option<AppliedPatternOccurrence>,
    /// Proposal time corresponding to pattern tick zero after normalization.
    pub source_origin_tick: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppliedPatternOccurrence {
    pub sequencer_clip: sequencer::PatternClipId,
    pub arrangement_clip: arrangement::ClipId,
    pub arrangement_track: arrangement::TrackId,
}

/// Exact sampler objects authored while promoting one analytic track. Slice
/// IDs remain analytic keys; pad/zone IDs remain constructive identities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedSampleKitBinding {
    pub kit: KitId,
    pub targets: BTreeMap<SampleSliceId, SampleTargetRef>,
    pub output_bus: mixer::BusId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppliedPitchedEventBinding {
    pub pattern: Option<sequencer::PatternId>,
    pub note: Option<sequencer::NoteId>,
    pub selected_choice: Option<usize>,
    pub alternatives: Vec<PitchChoice>,
    pub original_micro_offset_frames: i64,
    pub evidence: Vec<ReconstructionEvidenceId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppliedAutomationBinding {
    pub lane: automation::AutomationLaneId,
    pub arrangement_parameter: arrangement::ParameterId,
    pub arrangement_clip: Option<arrangement::ClipId>,
    pub target: AutomationTarget,
    pub evidence: Vec<ReconstructionEvidenceId>,
}

/// Kept as unresolved metadata until a user chooses a mechanism and target.
/// The alternatives are never converted into an asserted synth/effect type.
#[derive(Clone, Debug, PartialEq)]
pub struct UnresolvedModulationBinding {
    pub track: ReconstructionTrackId,
    pub proposal: ModulationProposal,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UnresolvedEffectBinding {
    pub track: ReconstructionTrackId,
    pub proposal: EffectProposal,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UnresolvedLatentComponentBinding {
    pub track: ReconstructionTrackId,
    pub proposal: LatentComponentProposal,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppliedResidualBinding {
    pub track: ReconstructionTrackId,
    pub audio_clip: arrangement::ClipId,
    pub source: SourceFrameRange,
    pub applied_mode: ResidualRenderMode,
    pub preferred_mode: ResidualRenderMode,
    pub evidence: Vec<ReconstructionEvidenceId>,
}

/// Typed anti-corruption map between reconstruction IDs and independent DAW
/// ID spaces.  It is returned on commit and is suitable for persistence by a
/// higher-level document format without conflating any raw IDs.
#[derive(Clone, Debug, PartialEq)]
pub struct ReconstructionApplicationBindings {
    pub proposal: ReconstructionProposalId,
    pub source_asset: assets::AssetId,
    pub tracks: BTreeMap<ReconstructionTrackId, AppliedTrackBinding>,
    pub slices: BTreeMap<SampleSliceId, AppliedSliceBinding>,
    pub triggers: BTreeMap<TriggerId, AppliedTriggerBinding>,
    pub patterns: BTreeMap<ReconstructionTrackId, AppliedPatternBinding>,
    pub sample_kits: BTreeMap<ReconstructionTrackId, AppliedSampleKitBinding>,
    pub pitched_events: BTreeMap<PitchedEventId, AppliedPitchedEventBinding>,
    pub automations: BTreeMap<AutomationProposalId, AppliedAutomationBinding>,
    pub unresolved_modulations:
        BTreeMap<reconstruction::ModulationProposalId, UnresolvedModulationBinding>,
    pub unresolved_effects: BTreeMap<reconstruction::EffectProposalId, UnresolvedEffectBinding>,
    pub unresolved_latent_components:
        BTreeMap<ReconstructionTrackId, UnresolvedLatentComponentBinding>,
    pub residual: Option<AppliedResidualBinding>,
    pub evidence: BTreeMap<ReconstructionEvidenceId, ReconstructionEvidence>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReconstructionApplicationReceipt {
    pub project_revision: u64,
    pub derivation_scope: DerivationScope,
    pub bindings: ReconstructionApplicationBindings,
    pub diagnostics: Vec<ApplicationDiagnostic>,
}

#[derive(Clone, Debug)]
pub struct ReconstructionApplicationPlan {
    pub expected_project_revision: u64,
    pub proposal: ReconstructionProposal,
    pub source_asset: assets::AssetId,
    pub source_frame_count: u64,
    pub sample_rate: u32,
    pub evidence: BTreeMap<ReconstructionEvidenceId, ReconstructionEvidence>,
    pub diagnostics: Vec<ApplicationDiagnostic>,
    /// Stable namespace paired with reconstruction-local proposal/evidence
    /// IDs when they become durable constructive provenance.
    pub derivation_scope: DerivationScope,
}

impl ReconstructionApplicationPlan {
    pub fn prepare(
        self,
        project: &DawProject,
    ) -> Result<PreparedReconstructionApplication, ReconstructionApplyError> {
        if project.revisions().aggregate != self.expected_project_revision {
            return Err(ReconstructionApplyError::RevisionConflict {
                expected: self.expected_project_revision,
                actual: project.revisions().aggregate,
            });
        }

        let mut candidate = project.state().clone();
        let mut diagnostics = self.diagnostics.clone();
        let bindings = apply_to_candidate(&mut candidate, &self, &mut diagnostics)?;
        let touched = touched_domains(&bindings);
        let prepared = project.prepare_transaction(
            format!("Apply reconstruction proposal {}", self.proposal.id.get()),
            self.expected_project_revision,
            touched,
            move |state| {
                *state = candidate;
                Ok::<(), &'static str>(())
            },
        )?;
        Ok(PreparedReconstructionApplication {
            prepared,
            bindings,
            diagnostics,
            derivation_scope: self.derivation_scope,
        })
    }
}

#[derive(Debug)]
pub struct PreparedReconstructionApplication {
    prepared: PreparedProjectTransaction,
    bindings: ReconstructionApplicationBindings,
    diagnostics: Vec<ApplicationDiagnostic>,
    derivation_scope: DerivationScope,
}

impl PreparedReconstructionApplication {
    pub fn base_revision(&self) -> u64 {
        self.prepared.base_revision()
    }

    pub fn bindings(&self) -> &ReconstructionApplicationBindings {
        &self.bindings
    }

    pub fn diagnostics(&self) -> &[ApplicationDiagnostic] {
        &self.diagnostics
    }

    /// The project remains unchanged until this explicit publication step.
    pub fn commit(
        self,
        project: &mut DawProject,
    ) -> Result<ReconstructionApplicationReceipt, ReconstructionApplyError> {
        let project_revision = project.commit_prepared(self.prepared)?;
        Ok(ReconstructionApplicationReceipt {
            project_revision,
            derivation_scope: self.derivation_scope,
            bindings: self.bindings,
            diagnostics: self.diagnostics,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ReconstructionPlanError {
    InvalidReconstruction(Vec<reconstruction::ReconstructionValidationIssue>),
    SelectionUnresolved,
    SelectionRejected,
    SelectedProposalMissing(ReconstructionProposalId),
    SourceAssetMissing(assets::AssetId),
    SampleRateMismatch {
        reconstruction: u32,
        project: u32,
        asset: u32,
    },
    SourceRangeOutsideAsset {
        required_frames: u64,
        asset_frames: u64,
    },
    InvalidApplicationShape {
        path: String,
        message: String,
    },
}

impl fmt::Display for ReconstructionPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidReconstruction(issues) => {
                write!(formatter, "reconstruction has {} validation issue(s)", issues.len())
            }
            Self::SelectionUnresolved => write!(formatter, "reconstruction selection is unresolved"),
            Self::SelectionRejected => write!(formatter, "all reconstruction proposals were rejected"),
            Self::SelectedProposalMissing(id) => {
                write!(formatter, "selected reconstruction proposal {} is missing", id.get())
            }
            Self::SourceAssetMissing(id) => write!(formatter, "source asset {} is missing", id.0),
            Self::SampleRateMismatch {
                reconstruction,
                project,
                asset,
            } => write!(
                formatter,
                "sample-rate mismatch: reconstruction {reconstruction}, project {project}, asset {asset}"
            ),
            Self::SourceRangeOutsideAsset {
                required_frames,
                asset_frames,
            } => write!(
                formatter,
                "reconstruction requires {required_frames} source frames but asset has {asset_frames}"
            ),
            Self::InvalidApplicationShape { path, message } => {
                write!(formatter, "cannot apply {path}: {message}")
            }
        }
    }
}

impl Error for ReconstructionPlanError {}

#[derive(Debug)]
pub enum ReconstructionApplyError {
    RevisionConflict { expected: u64, actual: u64 },
    Domain(String),
    Bridge(BridgeError),
}

impl fmt::Display for ReconstructionApplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RevisionConflict { expected, actual } => {
                write!(
                    formatter,
                    "project revision conflict: expected {expected}, actual {actual}"
                )
            }
            Self::Domain(message) => formatter.write_str(message),
            Self::Bridge(error) => error.fmt(formatter),
        }
    }
}

impl Error for ReconstructionApplyError {}

impl From<BridgeError> for ReconstructionApplyError {
    fn from(value: BridgeError) -> Self {
        Self::Bridge(value)
    }
}

/// Validate the complete reconstruction set, resolve its explicit selection,
/// and inspect source/project compatibility without mutating the project.
pub fn plan_selected_reconstruction(
    project: &DawProject,
    set: &ReconstructionSet,
    source_asset: assets::AssetId,
) -> Result<ReconstructionApplicationPlan, ReconstructionPlanError> {
    let validation = set.validate();
    if !validation.is_empty() {
        return Err(ReconstructionPlanError::InvalidReconstruction(validation));
    }
    let selected = match set.selection {
        ReconstructionSelection::EvidencePreferred(id)
        | ReconstructionSelection::UserSelected(id) => id,
        ReconstructionSelection::Unresolved => {
            return Err(ReconstructionPlanError::SelectionUnresolved)
        }
        ReconstructionSelection::UserRejectedAll => {
            return Err(ReconstructionPlanError::SelectionRejected)
        }
    };
    let proposal = set
        .proposals
        .iter()
        .find(|proposal| proposal.id == selected)
        .cloned()
        .ok_or(ReconstructionPlanError::SelectedProposalMissing(selected))?;
    validate_application_shape(&proposal)?;
    let asset = project
        .state()
        .domains
        .assets
        .get(source_asset)
        .ok_or(ReconstructionPlanError::SourceAssetMissing(source_asset))?;
    let project_rate = project.state().domains.arrangement.sample_rate;
    let asset_rate = asset.metadata().sample_rate_hz;
    if set.sample_rate != project_rate || set.sample_rate != asset_rate {
        return Err(ReconstructionPlanError::SampleRateMismatch {
            reconstruction: set.sample_rate,
            project: project_rate,
            asset: asset_rate,
        });
    }
    if asset.metadata().frame_count.0 < set.source_frame_count {
        return Err(ReconstructionPlanError::SourceRangeOutsideAsset {
            required_frames: set.source_frame_count,
            asset_frames: asset.metadata().frame_count.0,
        });
    }

    let mut diagnostics = inspect_lossy_boundaries(&proposal);
    if matches!(asset.availability(), AssetAvailability::Missing { .. }) {
        diagnostics.push(ApplicationDiagnostic::warning(
            ApplicationDiagnosticCode::SourceAssetUnavailable,
            "source_asset",
            "source references were authored, but the media asset is currently missing",
        ));
    }
    Ok(ReconstructionApplicationPlan {
        expected_project_revision: project.revisions().aggregate,
        proposal,
        source_asset,
        source_frame_count: set.source_frame_count,
        sample_rate: set.sample_rate,
        evidence: set
            .evidence
            .iter()
            .cloned()
            .map(|evidence| (evidence.id, evidence))
            .collect(),
        diagnostics,
        derivation_scope: reconstruction_scope(set, source_asset, selected),
    })
}

fn validate_application_shape(
    proposal: &ReconstructionProposal,
) -> Result<(), ReconstructionPlanError> {
    let mut slices = BTreeSet::new();
    let mut triggers = BTreeSet::new();
    let mut pitched_events = BTreeSet::new();
    let mut automations = BTreeSet::new();
    let mut modulations = BTreeSet::new();
    let mut effects = BTreeSet::new();
    for (track_index, track) in proposal.tracks.iter().enumerate() {
        let path = format!("proposal.tracks[{track_index}]");
        for slice in &track.sample_slices {
            if !slices.insert(slice.id) {
                return shape_error(
                    format!("{path}.sample_slices"),
                    "slice IDs must be unique across the selected proposal",
                );
            }
        }
        let local_triggers: BTreeSet<_> = track.triggers.iter().map(|item| item.id).collect();
        for trigger in &track.triggers {
            if !triggers.insert(trigger.id) {
                return shape_error(
                    format!("{path}.triggers"),
                    "trigger IDs must be unique across the selected proposal",
                );
            }
        }
        for event in &track.pitched_events {
            if !pitched_events.insert(event.id) {
                return shape_error(
                    format!("{path}.pitched_events"),
                    "pitched-event IDs must be unique across the selected proposal",
                );
            }
            if event.pitch_curve.iter().any(|point| {
                point.offset_frames > event.source.len()
                    || !point.cents_from_preferred.is_finite()
                    || !point.confidence.is_finite()
                    || !(0.0..=1.0).contains(&point.confidence)
            }) || !event
                .pitch_curve
                .windows(2)
                .all(|pair| pair[0].offset_frames <= pair[1].offset_frames)
            {
                return shape_error(
                    format!("{path}.pitched_events[{}].pitch_curve", event.id.get()),
                    "pitch-curve points must be ordered, finite, supported, and inside the event",
                );
            }
        }
        for automation in &track.automations {
            if !automations.insert(automation.id) {
                return shape_error(
                    format!("{path}.automations"),
                    "automation IDs must be unique across the selected proposal",
                );
            }
        }
        for modulation in &track.modulations {
            if !modulations.insert(modulation.id) {
                return shape_error(
                    format!("{path}.modulations"),
                    "modulation IDs must be unique across the selected proposal",
                );
            }
        }
        for effect in &track.effects {
            if !effects.insert(effect.id) {
                return shape_error(
                    format!("{path}.effects"),
                    "effect IDs must be unique across the selected proposal",
                );
            }
        }
        if let Some(step_pattern) = &track.step_pattern {
            let divisions = i64::from(step_pattern.steps_per_quarter);
            if divisions == 0 || divisions > sequencer::PPQ || sequencer::PPQ % divisions != 0 {
                return shape_error(
                    format!("{path}.step_pattern.steps_per_quarter"),
                    "step resolution must divide the sequencer PPQ",
                );
            }
            if !step_pattern.tempo.bpm.is_finite() || step_pattern.tempo.bpm <= 0.0 {
                return shape_error(
                    format!("{path}.step_pattern.tempo"),
                    "step tempo must be finite and positive",
                );
            }
            for (placement_index, placement) in step_pattern.placements.iter().enumerate() {
                if !local_triggers.contains(&placement.trigger)
                    || !placement.velocity.is_finite()
                    || !(0.0..=1.0).contains(&placement.velocity)
                    || !placement.confidence.is_finite()
                    || !(0.0..=1.0).contains(&placement.confidence)
                {
                    return shape_error(
                        format!("{path}.step_pattern.placements[{placement_index}]"),
                        "placement must reference this track's trigger and contain finite unit values",
                    );
                }
            }
        }
    }
    Ok(())
}

fn shape_error<T>(
    path: impl Into<String>,
    message: impl Into<String>,
) -> Result<T, ReconstructionPlanError> {
    Err(ReconstructionPlanError::InvalidApplicationShape {
        path: path.into(),
        message: message.into(),
    })
}

fn inspect_lossy_boundaries(proposal: &ReconstructionProposal) -> Vec<ApplicationDiagnostic> {
    let mut diagnostics = Vec::new();
    if proposal
        .tempo
        .is_some_and(|tempo| tempo.phase_source_frame != 0)
    {
        diagnostics.push(ApplicationDiagnostic::warning(
            ApplicationDiagnosticCode::TempoPhaseRequiresTimelineOffset,
            "proposal.tempo.phase_source_frame",
            "the tempo map has no beat-phase origin; exact frame placements retain the phase",
        ));
    }
    for (track_index, track) in proposal.tracks.iter().enumerate() {
        let path = format!("proposal.tracks[{track_index}]");
        for (event_index, event) in track.pitched_events.iter().enumerate() {
            if matches!(event.selection, PitchChoiceSelection::Unresolved) {
                diagnostics.push(ApplicationDiagnostic::warning(
                    ApplicationDiagnosticCode::PitchChoiceRequired,
                    format!("{path}.pitched_events[{event_index}].selection"),
                    "no note was asserted because the pitch alternatives remain unresolved",
                ));
            }
            if event.musical_start_tick.is_none() {
                diagnostics.push(ApplicationDiagnostic::warning(
                    ApplicationDiagnosticCode::MissingMusicalTiming,
                    format!("{path}.pitched_events[{event_index}].musical_start_tick"),
                    "musical timing will be derived from the current project tempo map",
                ));
            }
        }
        for (index, automation) in track.automations.iter().enumerate() {
            diagnostics.push(ApplicationDiagnostic::warning(
                ApplicationDiagnosticCode::AutomationDestinationUnresolved,
                format!("{path}.automations[{index}].target"),
                format!(
                    "{:?} was preserved in an editable reconstruction lane without claiming a plugin or instrument destination",
                    automation.target
                ),
            ));
            if automation.points.is_empty() {
                diagnostics.push(ApplicationDiagnostic::warning(
                    ApplicationDiagnosticCode::EmptyAutomation,
                    format!("{path}.automations[{index}].points"),
                    "the empty lane is retained as metadata and has no arrangement clip",
                ));
            }
        }
        for (index, modulation) in track.modulations.iter().enumerate() {
            let (code, message) = if modulation.selected_implementation.is_none() {
                (
                    ApplicationDiagnosticCode::ModulationImplementationUnresolved,
                    "implementation alternatives remain unresolved metadata",
                )
            } else {
                (
                    ApplicationDiagnosticCode::ModulationImplementationRequiresDsp,
                    "the selected mechanism has no lossless synth/effect-domain representation",
                )
            };
            diagnostics.push(ApplicationDiagnostic::warning(
                code,
                format!("{path}.modulations[{index}]"),
                message,
            ));
        }
        for (index, effect) in track.effects.iter().enumerate() {
            let (code, message) = if effect.selected_implementation.is_none() {
                (
                    ApplicationDiagnosticCode::EffectImplementationUnresolved,
                    "effect alternatives remain unresolved metadata",
                )
            } else {
                (
                    ApplicationDiagnosticCode::EffectImplementationRequiresDsp,
                    "the selected effect has no lossless processor-domain representation",
                )
            };
            diagnostics.push(ApplicationDiagnostic::warning(
                code,
                format!("{path}.effects[{index}]"),
                message,
            ));
        }
        if track.latent_component.is_some() {
            diagnostics.push(ApplicationDiagnostic::warning(
                ApplicationDiagnosticCode::LatentComponentRequiresRenderer,
                format!("{path}.latent_component"),
                "the spectral template remains typed metadata; no component synthesizer was invented",
            ));
        }
        if track.residual.as_ref().is_some_and(|residual| {
            residual.preferred_mode == ResidualRenderMode::SubtractConstructiveRender
        }) {
            diagnostics.push(ApplicationDiagnostic::warning(
                ApplicationDiagnosticCode::ResidualSubtractionUnavailable,
                format!("{path}.residual.preferred_mode"),
                "subtractive rendering is unavailable; the original source safety layer was applied",
            ));
        }
    }
    diagnostics
}

fn apply_to_candidate(
    state: &mut ProjectState,
    plan: &ReconstructionApplicationPlan,
    diagnostics: &mut Vec<ApplicationDiagnostic>,
) -> Result<ReconstructionApplicationBindings, ReconstructionApplyError> {
    let arrangement_asset = state
        .bindings
        .bind_media_asset(plan.source_asset)
        .map_err(domain)?;
    let mut editor =
        ArrangementEditor::from_state(state.domains.arrangement.clone()).map_err(domain)?;
    let mut bindings = ReconstructionApplicationBindings {
        proposal: plan.proposal.id,
        source_asset: plan.source_asset,
        tracks: BTreeMap::new(),
        slices: BTreeMap::new(),
        triggers: BTreeMap::new(),
        patterns: BTreeMap::new(),
        sample_kits: BTreeMap::new(),
        pitched_events: BTreeMap::new(),
        automations: BTreeMap::new(),
        unresolved_modulations: BTreeMap::new(),
        unresolved_effects: BTreeMap::new(),
        unresolved_latent_components: BTreeMap::new(),
        residual: None,
        evidence: plan.evidence.clone(),
    };

    for (track_index, proposed) in plan.proposal.tracks.iter().enumerate() {
        let track_name = anonymous_track_name(&proposed.kind);
        let arrangement_track = editor
            .create_track(track_name.clone(), TrackKind::Hybrid)
            .map_err(domain)?;
        let mixer_bus = state
            .domains
            .mixer
            .add_bus(mixer::BusKind::Source, track_name)
            .map_err(domain)?;
        state
            .bindings
            .mixer
            .tracks
            .insert(arrangement_track, mixer_bus);
        bindings.tracks.insert(
            proposed.id,
            AppliedTrackBinding {
                arrangement_track,
                mixer_bus,
                analytic_kind: proposed.kind.clone(),
                evidence: proposed.evidence.clone(),
            },
        );

        for slice in &proposed.sample_slices {
            bindings.slices.insert(
                slice.id,
                AppliedSliceBinding {
                    source_asset: plan.source_asset,
                    source: slice.source,
                    anonymous_family_id: slice.anonymous_family_id,
                    evidence: slice.evidence.clone(),
                },
            );
        }

        apply_exact_triggers(
            state,
            &mut editor,
            plan,
            proposed,
            arrangement_track,
            arrangement_asset,
            &mut bindings,
        )?;

        if let Some(step_pattern) = &proposed.step_pattern {
            // The constructive lowerer works against the authoritative
            // aggregate candidate, while this bridge batches arrangement
            // operations in an editor clone. Publish the clone into the
            // candidate before lowering, then continue from its result so the
            // new pattern placement cannot be overwritten at loop end.
            state.domains.arrangement = editor.state().clone();
            apply_step_pattern(
                state,
                plan,
                proposed,
                step_pattern,
                &mut bindings,
                diagnostics,
                track_index,
            )?;
            editor =
                ArrangementEditor::from_state(state.domains.arrangement.clone()).map_err(domain)?;
        }

        if matches!(
            proposed.kind,
            ReconstructionTrackKind::PitchedStream {
                rendering: PitchedRenderingModel::DiscreteNotes,
                ..
            }
        ) {
            apply_note_pattern(
                state,
                plan,
                proposed,
                &mut bindings,
                diagnostics,
                track_index,
            )?;
        } else {
            for event in &proposed.pitched_events {
                let selected_choice = selected_pitch_choice(event.selection);
                bindings.pitched_events.insert(
                    event.id,
                    AppliedPitchedEventBinding {
                        pattern: None,
                        note: None,
                        selected_choice,
                        alternatives: event.alternatives.clone(),
                        original_micro_offset_frames: event.micro_offset_frames,
                        evidence: event.evidence.clone(),
                    },
                );
            }
        }

        for (automation_index, automation) in proposed.automations.iter().enumerate() {
            apply_automation(
                state,
                &mut editor,
                plan,
                proposed,
                automation,
                arrangement_track,
                &mut bindings,
                automation_index,
            )?;
        }

        for modulation in &proposed.modulations {
            bindings.unresolved_modulations.insert(
                modulation.id,
                UnresolvedModulationBinding {
                    track: proposed.id,
                    proposal: modulation.clone(),
                },
            );
        }
        for effect in &proposed.effects {
            bindings.unresolved_effects.insert(
                effect.id,
                UnresolvedEffectBinding {
                    track: proposed.id,
                    proposal: effect.clone(),
                },
            );
        }
        if let Some(component) = &proposed.latent_component {
            bindings.unresolved_latent_components.insert(
                proposed.id,
                UnresolvedLatentComponentBinding {
                    track: proposed.id,
                    proposal: component.clone(),
                },
            );
        }
        if let Some(residual) = &proposed.residual {
            let placement = exact_frame_range(residual.source)?;
            let source = exact_source_range(residual.source)?;
            let clip = editor
                .create_audio_clip(
                    arrangement_track,
                    "original source safety layer",
                    placement,
                    arrangement_asset,
                    source,
                )
                .map_err(domain)?;
            add_clip_usage(
                state,
                plan.source_asset,
                clip,
                residual.source,
                "residual safety layer",
            )?;
            bindings.residual = Some(AppliedResidualBinding {
                track: proposed.id,
                audio_clip: clip,
                source: residual.source,
                applied_mode: ResidualRenderMode::OriginalSafetyLayer,
                preferred_mode: residual.preferred_mode,
                evidence: residual.evidence.clone(),
            });
        }
    }

    state.domains.arrangement = editor.state().clone();
    // Arrangement clips store decibels while reconstruction triggers store a
    // linear factor. Apply it after the editor has completed its atomic clone.
    for trigger in bindings.triggers.values() {
        if let Some(clip) = state.domains.arrangement.clips.get_mut(&trigger.audio_clip) {
            clip.gain_db = linear_to_db(trigger.gain_linear);
        }
    }
    state.domains.arrangement.validate().map_err(domain)?;
    Ok(bindings)
}

fn apply_exact_triggers(
    state: &mut ProjectState,
    editor: &mut ArrangementEditor,
    plan: &ReconstructionApplicationPlan,
    track: &EditableTrackProposal,
    arrangement_track: arrangement::TrackId,
    arrangement_asset: arrangement::AssetId,
    bindings: &mut ReconstructionApplicationBindings,
) -> Result<(), ReconstructionApplyError> {
    let slices: BTreeMap<_, _> = track
        .sample_slices
        .iter()
        .map(|slice| (slice.id, slice))
        .collect();
    for trigger in &track.triggers {
        let slice = slices
            .get(&trigger.slice)
            .expect("validated slice reference");
        let onset = i128::from(trigger.source_onset_frame);
        let preroll = i128::from(slice.onset_offset_frames);
        let start = (onset - preroll).clamp(i64::MIN as i128, i64::MAX as i128) as i64;
        let placement =
            FrameRange::from_start_and_len(Frame(start), slice.source.len()).map_err(domain)?;
        let source = exact_source_range(slice.source)?;
        let clip = editor
            .create_audio_clip(
                arrangement_track,
                format!("anonymous trigger {}", trigger.id.get()),
                placement,
                arrangement_asset,
                source,
            )
            .map_err(domain)?;
        add_clip_usage(
            state,
            plan.source_asset,
            clip,
            slice.source,
            "reconstruction trigger slice",
        )?;
        bindings.triggers.insert(
            trigger.id,
            AppliedTriggerBinding {
                audio_clip: clip,
                slice: trigger.slice,
                step_pattern: None,
                step_lane: None,
                step_index: None,
                gain_linear: trigger.gain,
                original_micro_offset_frames: trigger.micro_offset_frames,
                evidence: trigger.evidence.clone(),
            },
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_step_pattern(
    state: &mut ProjectState,
    plan: &ReconstructionApplicationPlan,
    track: &EditableTrackProposal,
    proposal: &reconstruction::StepPatternProposal,
    bindings: &mut ReconstructionApplicationBindings,
    diagnostics: &mut Vec<ApplicationDiagnostic>,
    track_index: usize,
) -> Result<(), ReconstructionApplyError> {
    if proposal.placements.is_empty() {
        return Ok(());
    }
    let resolution = u64::try_from(sequencer::PPQ / i64::from(proposal.steps_per_quarter))
        .map_err(|_| ReconstructionApplyError::Domain("invalid step resolution".into()))?;
    let origin_step = proposal
        .placements
        .iter()
        .map(|item| item.step)
        .min()
        .unwrap_or(0);
    let maximum_step = proposal
        .placements
        .iter()
        .map(|item| item.step)
        .max()
        .unwrap_or(origin_step);
    let span_steps = maximum_step.saturating_sub(origin_step).saturating_add(1);
    let length = u64::try_from(span_steps)
        .ok()
        .and_then(|steps| steps.checked_mul(resolution))
        .ok_or_else(|| ReconstructionApplyError::Domain("step-pattern length overflow".into()))?;
    let trigger_lookup: BTreeMap<_, _> = track
        .triggers
        .iter()
        .map(|trigger| (trigger.id, trigger))
        .collect();
    let output_bus = bindings
        .tracks
        .get(&track.id)
        .expect("track route was created before its pattern")
        .mixer_bus;
    let mut ids = state.domains.sample_kits.clone();
    let kit_id = ids.allocate_kit_id().map_err(domain)?;
    let mut kit = SampleKit::new(
        kit_id,
        anonymous_track_name(&track.kind),
        SampleRouteIntent::new(output_bus).map_err(domain)?,
    );
    let mut pad_by_slice = BTreeMap::new();
    let mut zone_by_slice = BTreeMap::new();
    for slice in track.sample_slices.iter() {
        let pad_id = ids.allocate_pad_id().map_err(domain)?;
        let zone_id = ids.allocate_zone_id().map_err(domain)?;
        let mut pad = SamplePad::new(pad_id, format!("anonymous slice {}", slice.id.get()));
        pad.zone_order.push(zone_id);
        let material = SourceMaterialRef::VirtualSlice(
            VirtualSliceRef::new(
                plan.source_asset,
                AssetFrameRange::new(
                    SampleFrames(slice.source.start),
                    SampleFrames(slice.source.end),
                )
                .map_err(domain)?,
            )
            .map_err(domain)?,
        );
        let evidence = scoped_evidence(plan, &slice.evidence);
        let mut zone = SampleZone::new(zone_id, pad_id, material);
        zone.provenance = SampleMaterialProvenance::Deprojection {
            proposal: ScopedProposalRef {
                scope: plan.derivation_scope,
                local: plan.proposal.id.get(),
            },
            evidence: evidence.clone(),
        };
        zone.evidence = evidence.into_iter().collect();
        kit.pad_order.push(pad_id);
        kit.pads.insert(pad_id, pad);
        kit.zones.insert(zone_id, zone);
        pad_by_slice.insert(slice.id, pad_id);
        zone_by_slice.insert(slice.id, zone_id);
    }

    let planned_steps = proposal
        .placements
        .iter()
        .map(|placement| {
            let trigger = trigger_lookup
                .get(&placement.trigger)
                .expect("validated trigger reference");
            let normalized = placement.step.saturating_sub(origin_step);
            let at = normalized
                .checked_mul(resolution as i64)
                .ok_or_else(|| ReconstructionApplyError::Domain("step time overflow".into()))?;
            Ok(PlannedStep {
                pad: pad_by_slice[&trigger.slice],
                at: BeatTime(at),
                gate: BeatDuration(resolution),
                velocity: placement.velocity,
                probability: 1.0,
                ratchets: 1,
                pitch_semitones: 0.0,
                pan: 0.0,
                micro_offset_ticks: frames_to_ticks(
                    placement.micro_offset_frames,
                    f64::from(proposal.tempo.bpm),
                    plan.sample_rate,
                ),
                original_micro_offset_frames: Some(placement.micro_offset_frames),
                exact_source_onset_frame: Some(trigger.source_onset_frame),
                evidence: scoped_evidence(plan, &trigger.evidence),
            })
        })
        .collect::<Result<Vec<_>, ReconstructionApplyError>>()?;
    let symbolic_bindings = pad_by_slice
        .iter()
        .map(|(slice, pad)| (format!("slice_{}", slice.get()), *pad))
        .collect();
    let planned_pattern = PlannedPattern {
        id: PlannedPatternId::from_raw(track.id.get()),
        name: anonymous_track_name(&track.kind),
        cycle: BeatDuration(length.max(resolution)),
        seed: PatternSeed::Deprojected {
            proposal: ScopedProposalRef {
                scope: plan.derivation_scope,
                local: plan.proposal.id.get(),
            },
            resolution: BeatDuration(resolution),
            expression: None,
            diverged: false,
        },
        bindings: symbolic_bindings,
        steps: planned_steps,
    };
    let constructive_plan = ConstructiveEditPlan::new(
        "Apply anonymous reconstruction steps",
        plan.expected_project_revision,
        vec![ConstructiveCause::Deprojection {
            proposal: ScopedProposalRef {
                scope: plan.derivation_scope,
                local: plan.proposal.id.get(),
            },
            evidence: scoped_evidence(plan, &proposal.evidence),
        }],
        Vec::new(),
        KitMutation {
            before: None,
            after: kit,
        },
        Some(planned_pattern),
        Some(PatternPlacementIntent {
            pattern: PlannedPatternId::from_raw(track.id.get()),
            start: BeatTime(origin_step.saturating_mul(resolution as i64)),
            length: BeatDuration(length.max(resolution)),
            pattern_offset: BeatTime::ZERO,
            looped: false,
            transpose_semitones: 0.0,
            gain: 1.0,
        }),
        ConstructiveFocus::Pattern(PlannedPatternId::from_raw(track.id.get())),
    )
    .map_err(domain)?;
    let applied =
        constructive::apply_to_project_state(state, &constructive_plan).map_err(domain)?;
    let pattern_id = applied.pattern.expect("planned pattern was applied");
    let arrangement_pattern = applied
        .arrangement_pattern
        .expect("planned arrangement pattern was bound");
    bindings.sample_kits.insert(
        track.id,
        AppliedSampleKitBinding {
            kit: applied.kit,
            targets: pad_by_slice
                .iter()
                .map(|(slice, pad)| {
                    (
                        *slice,
                        SampleTargetRef {
                            kit: applied.kit,
                            pad: *pad,
                            zone: zone_by_slice[slice],
                        },
                    )
                })
                .collect(),
            output_bus: applied.output_bus,
        },
    );
    for (placement_index, placement) in proposal.placements.iter().enumerate() {
        let trigger = trigger_lookup
            .get(&placement.trigger)
            .expect("validated trigger reference");
        let normalized = placement.step.saturating_sub(origin_step);
        let step_index = u32::try_from(normalized)
            .map_err(|_| ReconstructionApplyError::Domain("step index overflow".into()))?;
        let micro_ticks = frames_to_ticks(
            placement.micro_offset_frames,
            f64::from(proposal.tempo.bpm),
            plan.sample_rate,
        );
        if let Some(binding) = bindings.triggers.get_mut(&trigger.id) {
            binding.step_pattern = Some(pattern_id);
            binding.step_lane = applied.planned_step_lanes.get(placement_index).copied();
            binding.step_index = Some(step_index);
            if let Some(clip) = state.domains.arrangement.clips.get_mut(&binding.audio_clip) {
                // The source-ranged clip is the lossless fallback and receipt
                // anchor. Once the same hit has a routed sampler abstraction,
                // keeping both audible would double its energy.
                clip.muted = true;
            }
        }
        if !micro_offset_is_exact(
            placement.micro_offset_frames,
            micro_ticks,
            f64::from(proposal.tempo.bpm),
            plan.sample_rate,
        ) {
            diagnostics.push(ApplicationDiagnostic::warning(
                ApplicationDiagnosticCode::MicrotimingRoundedToTick,
                format!("proposal.tracks[{track_index}].triggers[{}]", trigger.id.get()),
                "sequencer microtiming was rounded to PPQ ticks; the exact frame placement remains in the audio clip binding",
            ));
        }
    }
    bindings.patterns.insert(
        track.id,
        AppliedPatternBinding {
            sequencer_pattern: pattern_id,
            arrangement_pattern,
            occurrence: Some(AppliedPatternOccurrence {
                sequencer_clip: applied
                    .sequencer_clip
                    .expect("planned pattern placement has a sequencer clip"),
                arrangement_clip: applied
                    .arrangement_clip
                    .expect("planned pattern placement has an arrangement clip"),
                arrangement_track: applied
                    .arrangement_track
                    .expect("planned pattern placement has an arrangement track"),
            }),
            source_origin_tick: origin_step.saturating_mul(resolution as i64),
        },
    );
    Ok(())
}

fn apply_note_pattern(
    state: &mut ProjectState,
    plan: &ReconstructionApplicationPlan,
    track: &EditableTrackProposal,
    bindings: &mut ReconstructionApplicationBindings,
    diagnostics: &mut Vec<ApplicationDiagnostic>,
    track_index: usize,
) -> Result<(), ReconstructionApplyError> {
    if track.pitched_events.is_empty() {
        return Ok(());
    }
    let starts: Vec<_> = track
        .pitched_events
        .iter()
        .filter(|event| selected_pitch_choice(event.selection).is_some())
        .map(|event| event_start_tick(state, event))
        .collect();
    let origin_tick = starts.iter().copied().min().unwrap_or(0).min(0);
    let pattern_id = state.domains.sequencer.allocate_pattern_id();
    let mut notes = BTreeMap::new();
    let mut maximum_end = 1_u64;

    for (event_index, event) in track.pitched_events.iter().enumerate() {
        let selected = selected_pitch_choice(event.selection);
        let Some(choice_index) = selected else {
            bindings.pitched_events.insert(
                event.id,
                AppliedPitchedEventBinding {
                    pattern: Some(pattern_id),
                    note: None,
                    selected_choice: None,
                    alternatives: event.alternatives.clone(),
                    original_micro_offset_frames: event.micro_offset_frames,
                    evidence: event.evidence.clone(),
                },
            );
            continue;
        };
        let choice = &event.alternatives[choice_index];
        let absolute_start = event_start_tick(state, event);
        let start = absolute_start.saturating_sub(origin_tick).max(0);
        let duration = event
            .musical_duration_ticks
            .unwrap_or_else(|| frame_duration_to_ticks(state, event.source).max(1));
        let micro_ticks = frames_to_ticks(
            event.micro_offset_frames,
            plan.proposal
                .tempo
                .map(|tempo| f64::from(tempo.bpm))
                .unwrap_or_else(|| {
                    state
                        .domains
                        .sequencer
                        .tempo_map()
                        .tempo_at(BeatTime(absolute_start))
                        .bpm()
                }),
            plan.sample_rate,
        );
        let note_id = state.domains.sequencer.allocate_note_id();
        let pitch_curve = normalized_pitch_expression(event);
        notes.insert(
            note_id,
            NoteEvent {
                id: note_id,
                start: BeatTime(start),
                duration: BeatDuration(duration),
                pitch: NotePitch {
                    midi_key: choice.midi_key,
                    cents: choice.cents,
                },
                velocity: event.velocity,
                release_velocity: 0.0,
                pan: 0.0,
                probability: 1.0,
                micro_offset: micro_ticks,
                channel: 0,
                instrument: None,
                articulation: Articulation::Normal,
                expression: PerNoteExpression {
                    pitch_cents: pitch_curve,
                    pressure: Vec::new(),
                    timbre: Vec::new(),
                },
            },
        );
        maximum_end = maximum_end.max((start as u64).saturating_add(duration));
        bindings.pitched_events.insert(
            event.id,
            AppliedPitchedEventBinding {
                pattern: Some(pattern_id),
                note: Some(note_id),
                selected_choice: Some(choice_index),
                alternatives: event.alternatives.clone(),
                original_micro_offset_frames: event.micro_offset_frames,
                evidence: event.evidence.clone(),
            },
        );
        let bpm = plan
            .proposal
            .tempo
            .map(|tempo| f64::from(tempo.bpm))
            .unwrap_or_else(|| {
                state
                    .domains
                    .sequencer
                    .tempo_map()
                    .tempo_at(BeatTime(absolute_start))
                    .bpm()
            });
        if !micro_offset_is_exact(
            event.micro_offset_frames,
            micro_ticks,
            bpm,
            plan.sample_rate,
        ) {
            diagnostics.push(ApplicationDiagnostic::warning(
                ApplicationDiagnosticCode::MicrotimingRoundedToTick,
                format!("proposal.tracks[{track_index}].pitched_events[{event_index}]"),
                "note microtiming was rounded to PPQ ticks; original frame timing remains in typed provenance",
            ));
        }
    }
    state
        .domains
        .sequencer
        .execute(
            "Apply reconstruction notes",
            vec![SequencerCommand::PutPattern {
                before: None,
                after: Some(PatternDefinition {
                    id: pattern_id,
                    name: anonymous_track_name(&track.kind),
                    length: BeatDuration(maximum_end.max(1)),
                    content: PatternContent::Notes(NotePattern { notes }),
                    origin: crate::sequencer::PatternOrigin::Deprojected {
                        proposal: plan.proposal.id,
                        diverged: false,
                    },
                    revision: 1,
                }),
            }],
        )
        .map_err(domain)?;
    let arrangement_pattern = state
        .bindings
        .bind_pattern_definition(pattern_id)
        .map_err(domain)?;
    bindings.patterns.insert(
        track.id,
        AppliedPatternBinding {
            sequencer_pattern: pattern_id,
            arrangement_pattern,
            occurrence: None,
            source_origin_tick: origin_tick,
        },
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_automation(
    state: &mut ProjectState,
    editor: &mut ArrangementEditor,
    plan: &ReconstructionApplicationPlan,
    track: &EditableTrackProposal,
    proposal: &AutomationProposal,
    arrangement_track: arrangement::TrackId,
    bindings: &mut ReconstructionApplicationBindings,
    automation_index: usize,
) -> Result<(), ReconstructionApplyError> {
    let address = ParameterAddress::Custom {
        namespace: "audec.reconstruction".into(),
        entity: format!(
            "proposal/{}/track/{}",
            plan.proposal.id.get(),
            track.id.get()
        ),
        parameter: format!("{:?}/{}", proposal.target, proposal.id.get()),
    };
    let (minimum, maximum, default) = automation_range(proposal);
    state
        .domains
        .automation
        .register_parameter(ParameterDescriptor {
            address: address.clone(),
            name: format!("reconstruction {:?}", proposal.target),
            unit: automation_unit(&proposal.target),
            minimum,
            maximum,
            default,
            mapping: ValueMapping::Linear,
            smoothing: SmoothingPolicy::None,
        })
        .map_err(domain)?;
    let lane = state
        .domains
        .automation
        .create_lane(
            format!("anonymous reconstruction lane {automation_index}"),
            address,
            TimeDomain::Frames,
        )
        .map_err(domain)?;
    for point in deduplicated_automation_points(proposal) {
        let coordinate = i64::try_from(point.source_frame)
            .map_err(|_| ReconstructionApplyError::Domain("automation frame overflow".into()))?;
        state
            .domains
            .automation
            .insert_point(
                lane,
                TimePosition::Frames(automation::ProjectFrame(coordinate)),
                f64::from(point.value),
                interpolation_shape(proposal.interpolation),
            )
            .map_err(domain)?;
    }
    let parameter = state.bindings.bind_automation_lane(lane).map_err(domain)?;
    let arrangement_clip = if let (Some(first), Some(last)) =
        (proposal.points.first(), proposal.points.last())
    {
        let start = i64::try_from(first.source_frame)
            .map_err(|_| ReconstructionApplyError::Domain("automation frame overflow".into()))?;
        let end_u64 = last
            .source_frame
            .saturating_add(1)
            .max(first.source_frame.saturating_add(1));
        let end = i64::try_from(end_u64)
            .map_err(|_| ReconstructionApplyError::Domain("automation frame overflow".into()))?;
        let clip = editor
            .create_automation_clip(
                arrangement_track,
                format!("reconstruction {:?}", proposal.target),
                FrameRange::new(Frame(start), Frame(end)).map_err(domain)?,
                parameter,
            )
            .map_err(domain)?;
        Some(clip)
    } else {
        None
    };
    bindings.automations.insert(
        proposal.id,
        AppliedAutomationBinding {
            lane,
            arrangement_parameter: parameter,
            arrangement_clip,
            target: proposal.target.clone(),
            evidence: proposal.evidence.clone(),
        },
    );
    Ok(())
}

fn add_clip_usage(
    state: &mut ProjectState,
    asset: assets::AssetId,
    clip: arrangement::ClipId,
    source: SourceFrameRange,
    label: &str,
) -> Result<(), ReconstructionApplyError> {
    state
        .domains
        .assets
        .add_usage(
            asset,
            AssetUsageOwner::AudioClip {
                persistent_id: clip.get(),
            },
            Some(
                AssetFrameRange::new(SampleFrames(source.start), SampleFrames(source.end))
                    .map_err(domain)?,
            ),
            label,
        )
        .map_err(domain)?;
    Ok(())
}

fn exact_frame_range(source: SourceFrameRange) -> Result<FrameRange, ReconstructionApplyError> {
    let start = i64::try_from(source.start)
        .map_err(|_| ReconstructionApplyError::Domain("source frame exceeds timeline".into()))?;
    let end = i64::try_from(source.end)
        .map_err(|_| ReconstructionApplyError::Domain("source frame exceeds timeline".into()))?;
    FrameRange::new(Frame(start), Frame(end)).map_err(domain)
}

fn exact_source_range(source: SourceFrameRange) -> Result<SourceRange, ReconstructionApplyError> {
    SourceRange::new(source.start, source.end).map_err(domain)
}

fn selected_pitch_choice(selection: PitchChoiceSelection) -> Option<usize> {
    match selection {
        PitchChoiceSelection::EvidencePreferred(index)
        | PitchChoiceSelection::UserSelected(index) => Some(index),
        PitchChoiceSelection::Unresolved => None,
    }
}

fn event_start_tick(state: &ProjectState, event: &reconstruction::PitchedEventProposal) -> i64 {
    event.musical_start_tick.unwrap_or_else(|| {
        state
            .domains
            .sequencer
            .tempo_map()
            .frame_to_beat_floor(sequencer::ProjectFrame(
                event.source.start.min(i64::MAX as u64) as i64,
            ))
            .0
    })
}

fn frame_duration_to_ticks(state: &ProjectState, source: SourceFrameRange) -> u64 {
    let tempo = state.domains.sequencer.tempo_map();
    let start = tempo.frame_to_beat_floor(sequencer::ProjectFrame(
        source.start.min(i64::MAX as u64) as i64,
    ));
    let end = tempo.frame_to_beat_floor(sequencer::ProjectFrame(
        source.end.min(i64::MAX as u64) as i64
    ));
    end.0.saturating_sub(start.0).max(1) as u64
}

fn normalized_pitch_expression(
    event: &reconstruction::PitchedEventProposal,
) -> Vec<ExpressionPoint> {
    let duration = event.source.len().max(1) as f32;
    event
        .pitch_curve
        .iter()
        .map(|point| ExpressionPoint {
            position: (point.offset_frames as f32 / duration).clamp(0.0, 1.0),
            value: point.cents_from_preferred,
        })
        .collect()
}

fn frames_to_ticks(frames: i64, bpm: f64, sample_rate: u32) -> i32 {
    let ticks = frames as f64 * bpm * sequencer::PPQ as f64 / (sample_rate as f64 * 60.0);
    ticks.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

fn scoped_evidence(
    plan: &ReconstructionApplicationPlan,
    evidence: &[ReconstructionEvidenceId],
) -> Vec<ScopedEvidenceRef> {
    evidence
        .iter()
        .map(|id| ScopedEvidenceRef {
            scope: plan.derivation_scope,
            local: id.get(),
        })
        .collect()
}

fn reconstruction_scope(
    set: &ReconstructionSet,
    source_asset: assets::AssetId,
    proposal: ReconstructionProposalId,
) -> DerivationScope {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    fn part(hash: &mut u128, bytes: &[u8]) {
        const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
        for byte in bytes {
            *hash ^= u128::from(*byte);
            *hash = hash.wrapping_mul(PRIME);
        }
    }
    let mut hash = OFFSET;
    part(&mut hash, b"audec.reconstruction-scope.v1\0");
    part(&mut hash, &source_asset.0.to_le_bytes());
    part(&mut hash, &set.sample_rate.to_le_bytes());
    part(&mut hash, &set.source_frame_count.to_le_bytes());
    part(&mut hash, &proposal.get().to_le_bytes());
    let mut evidence = set.evidence.iter().collect::<Vec<_>>();
    evidence.sort_by_key(|item| item.id);
    for item in evidence {
        part(&mut hash, &item.id.get().to_le_bytes());
        part(&mut hash, item.provenance.analyzer.as_bytes());
        part(&mut hash, &[0]);
        part(&mut hash, item.provenance.version.as_bytes());
        part(&mut hash, &[0]);
        if let Some(revision) = &item.provenance.source_revision {
            part(&mut hash, revision.as_bytes());
        }
        part(&mut hash, &[0]);
        part(&mut hash, item.provenance.locator.as_bytes());
        part(&mut hash, &[0]);
    }
    DerivationScope(hash)
}

fn micro_offset_is_exact(frames: i64, ticks: i32, bpm: f64, sample_rate: u32) -> bool {
    if !bpm.is_finite() || bpm <= 0.0 {
        return false;
    }
    let reconstructed = ticks as f64 * sample_rate as f64 * 60.0 / (bpm * sequencer::PPQ as f64);
    (reconstructed - frames as f64).abs() < 0.5
}

fn linear_to_db(gain: f32) -> f32 {
    if gain <= 0.0 {
        -144.0
    } else {
        (20.0 * gain.log10()).clamp(-144.0, 48.0)
    }
}

fn deduplicated_automation_points(
    proposal: &AutomationProposal,
) -> Vec<reconstruction::AutomationProposalPoint> {
    let mut by_frame = BTreeMap::new();
    for point in &proposal.points {
        by_frame.insert(point.source_frame, *point);
    }
    by_frame.into_values().collect()
}

fn automation_range(proposal: &AutomationProposal) -> (f64, f64, f64) {
    let mut minimum = f64::INFINITY;
    let mut maximum = f64::NEG_INFINITY;
    for point in &proposal.points {
        let value = f64::from(point.value);
        minimum = minimum.min(value);
        maximum = maximum.max(value);
    }
    if !minimum.is_finite() {
        return (0.0, 1.0, 0.0);
    }
    if minimum == maximum {
        let extent = minimum.abs().max(1.0) * 0.01;
        return (minimum - extent, maximum + extent, minimum);
    }
    (minimum, maximum, minimum.clamp(minimum, maximum))
}

fn automation_unit(target: &AutomationTarget) -> ParameterUnit {
    match target {
        AutomationTarget::Gain
        | AutomationTarget::Brightness
        | AutomationTarget::SpectralActivity
        | AutomationTarget::StereoWidth
        | AutomationTarget::TailLevel => ParameterUnit::Normalized,
        AutomationTarget::PitchCents => ParameterUnit::Custom("cents".into()),
        AutomationTarget::Custom(name) => ParameterUnit::Custom(name.clone()),
    }
}

fn interpolation_shape(interpolation: AutomationInterpolation) -> SegmentShape {
    match interpolation {
        AutomationInterpolation::Hold => SegmentShape::Hold,
        AutomationInterpolation::Linear => SegmentShape::Linear,
        AutomationInterpolation::Smooth => SegmentShape::Smooth,
    }
}

fn anonymous_track_name(kind: &ReconstructionTrackKind) -> String {
    match kind {
        ReconstructionTrackKind::AnonymousHitFamily { upstream_family_id } => {
            format!("anonymous hit family {upstream_family_id}")
        }
        ReconstructionTrackKind::UnclusteredHits => "unclustered hit observations".into(),
        ReconstructionTrackKind::PitchedStream {
            upstream_track_index,
            rendering,
        } => format!("pitched stream {upstream_track_index} ({rendering:?})"),
        ReconstructionTrackKind::LatentComponent {
            upstream_component_index,
        } => format!("latent component {upstream_component_index}"),
        ReconstructionTrackKind::Residual => "unexplained source residual".into(),
    }
}

fn touched_domains(bindings: &ReconstructionApplicationBindings) -> BTreeSet<ProjectDomain> {
    let mut touched = BTreeSet::from([
        ProjectDomain::Arrangement,
        ProjectDomain::Assets,
        ProjectDomain::Mixer,
        ProjectDomain::Bindings,
    ]);
    if !bindings.patterns.is_empty() {
        touched.insert(ProjectDomain::Sequencer);
        touched.insert(ProjectDomain::SampleKits);
    }
    if !bindings.automations.is_empty() {
        touched.insert(ProjectDomain::Automation);
    }
    touched
}

fn domain(error: impl fmt::Display) -> ReconstructionApplyError {
    ReconstructionApplyError::Domain(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        ContentFingerprint, DecodedAudioMetadata,
    };
    use crate::reconstruction::{
        AnalysisProvenance, EditableTrackProposal, ProposalScore, ReconstructionEvidenceKind,
        ReconstructionTrackKind, ResidualAudioProposal, SampleSliceProposal, SliceRole,
        StepPatternProposal, StepPlacement, TempoChoice,
    };

    fn source_asset(project: &mut DawProject, frames: u64) -> assets::AssetId {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/tmp/reconstruction.wav").unwrap()),
            None,
        )
        .unwrap();
        let registration = AssetRegistration {
            name: "source".into(),
            location: location.clone(),
            metadata: DecodedAudioMetadata {
                sample_rate_hz: 48_000,
                channels: 2,
                frame_count: SampleFrames(frames),
                container: Some("wav".into()),
                codec: Some("pcm".into()),
                bit_depth: Some(24),
            },
            content: ContentFingerprint::from_bytes(b"source"),
            provenance: AssetProvenance::new(
                0,
                AssetOrigin::ImportedFile {
                    importer: "test".into(),
                },
                location,
            ),
            tags: BTreeSet::new(),
            favorite: false,
        };
        let mut state = project.state().clone();
        let id = state.domains.assets.register(registration).unwrap();
        project
            .transact(
                "register source",
                project.revisions().aggregate,
                BTreeSet::from([ProjectDomain::Assets]),
                move |candidate| {
                    *candidate = state;
                    Ok::<(), &'static str>(())
                },
            )
            .unwrap();
        id
    }

    fn set(selection: ReconstructionSelection) -> ReconstructionSet {
        let evidence_id = ReconstructionEvidenceId::from_raw(1);
        let tempo = TempoChoice {
            bpm: 120.0,
            phase_source_frame: 0,
            upstream_tempo_rank: 0,
            upstream_beat_phase_index: 0,
            support: 0.9,
        };
        let slice = SampleSliceProposal {
            id: SampleSliceId::from_raw(1),
            source: SourceFrameRange {
                start: 90,
                end: 140,
            },
            onset_offset_frames: 10,
            role: SliceRole::AnonymousFamilyRepresentative,
            anonymous_family_id: Some(7),
            representative_event_index: 0,
            confidence: 0.9,
            evidence: vec![evidence_id],
        };
        let trigger = reconstruction::SampleTriggerProposal {
            id: TriggerId::from_raw(1),
            slice: slice.id,
            source_onset_frame: 100,
            gain: 0.8,
            musical_tick: Some(0),
            micro_offset_frames: 100,
            confidence: 0.9,
            evidence: vec![evidence_id],
        };
        let hit = EditableTrackProposal {
            id: ReconstructionTrackId::from_raw(1),
            label: "this label must not become an instrument".into(),
            kind: ReconstructionTrackKind::AnonymousHitFamily {
                upstream_family_id: 7,
            },
            sample_slices: vec![slice],
            triggers: vec![trigger],
            pitched_events: Vec::new(),
            step_pattern: Some(StepPatternProposal {
                steps_per_quarter: 4,
                placements: vec![StepPlacement {
                    trigger: TriggerId::from_raw(1),
                    step: 0,
                    micro_offset_frames: 100,
                    velocity: 0.8,
                    confidence: 0.9,
                }],
                tempo,
                evidence: vec![evidence_id],
            }),
            automations: Vec::new(),
            modulations: Vec::new(),
            effects: Vec::new(),
            latent_component: None,
            residual: None,
            confidence: 0.9,
            evidence: vec![evidence_id],
        };
        let residual = EditableTrackProposal {
            id: ReconstructionTrackId::from_raw(2),
            label: "residual".into(),
            kind: ReconstructionTrackKind::Residual,
            sample_slices: Vec::new(),
            triggers: Vec::new(),
            pitched_events: Vec::new(),
            step_pattern: None,
            automations: Vec::new(),
            modulations: Vec::new(),
            effects: Vec::new(),
            latent_component: None,
            residual: Some(ResidualAudioProposal {
                source: SourceFrameRange {
                    start: 0,
                    end: 1_000,
                },
                preferred_mode: ResidualRenderMode::SubtractConstructiveRender,
                fallback_mode: ResidualRenderMode::OriginalSafetyLayer,
                estimated_fraction: 0.5,
                evidence: vec![evidence_id],
            }),
            confidence: 1.0,
            evidence: vec![evidence_id],
        };
        ReconstructionSet {
            schema_version: reconstruction::RECONSTRUCTION_SCHEMA_VERSION,
            sample_rate: 48_000,
            source_frame_count: 1_000,
            evidence: vec![ReconstructionEvidence {
                id: evidence_id,
                kind: ReconstructionEvidenceKind::AnonymousEventFamily { family_id: 7 },
                strength: 0.9,
                provenance: AnalysisProvenance {
                    analyzer: "test".into(),
                    version: "1".into(),
                    source_revision: Some("abc".into()),
                    locator: "family/7".into(),
                },
            }],
            proposals: vec![ReconstructionProposal {
                id: ReconstructionProposalId::from_raw(1),
                rank: 0,
                label: "proposal".into(),
                tempo: Some(tempo),
                pitch_model: PitchedRenderingModel::DiscreteNotes,
                tracks: vec![hit, residual],
                score: ProposalScore {
                    observation_support: 0.9,
                    pattern_support: 0.8,
                    editability: 0.9,
                    estimated_coverage: 0.5,
                    assumption_penalty: 0.1,
                    total: 0.8,
                },
                evidence: vec![evidence_id],
                caveats: Vec::new(),
            }],
            selection,
        }
    }

    #[test]
    fn unresolved_selection_cannot_be_planned() {
        let mut project = DawProject::new("test", 48_000, 120.0).unwrap();
        let asset = source_asset(&mut project, 1_000);
        assert_eq!(
            plan_selected_reconstruction(
                &project,
                &set(ReconstructionSelection::Unresolved),
                asset
            )
            .unwrap_err(),
            ReconstructionPlanError::SelectionUnresolved
        );
    }

    #[test]
    fn preparation_is_atomic_and_commit_preserves_exact_slices_and_residual() {
        let mut project = DawProject::new("test", 48_000, 120.0).unwrap();
        let asset = source_asset(&mut project, 1_000);
        let revision = project.revisions().aggregate;
        let plan = plan_selected_reconstruction(
            &project,
            &set(ReconstructionSelection::UserSelected(
                ReconstructionProposalId::from_raw(1),
            )),
            asset,
        )
        .unwrap();
        let derivation_scope = plan.derivation_scope;
        let prepared = plan.prepare(&project).unwrap();
        assert_eq!(project.revisions().aggregate, revision);
        assert!(project.state().domains.arrangement.clips.is_empty());

        let receipt = prepared.commit(&mut project).unwrap();
        assert_eq!(receipt.derivation_scope, derivation_scope);
        assert_eq!(receipt.bindings.triggers.len(), 1);
        let promoted_track = ReconstructionTrackId::from_raw(1);
        let kit = &receipt.bindings.sample_kits[&promoted_track];
        assert_eq!(kit.targets.len(), 1);
        let pattern = &receipt.bindings.patterns[&promoted_track];
        let occurrence = pattern.occurrence.expect("step pattern is placed");
        assert_eq!(
            project
                .state()
                .bindings
                .patterns
                .placements
                .get(&occurrence.arrangement_clip)
                .copied(),
            Some(occurrence.sequencer_clip)
        );
        assert_eq!(
            receipt.bindings.slices[&SampleSliceId::from_raw(1)].source,
            SourceFrameRange {
                start: 90,
                end: 140
            }
        );
        let trigger_clip = project
            .state()
            .domains
            .arrangement
            .clip(receipt.bindings.triggers[&TriggerId::from_raw(1)].audio_clip)
            .unwrap();
        let arrangement::ClipContent::Audio(region) = &trigger_clip.content else {
            panic!("audio")
        };
        assert_eq!(
            region.source,
            SourceRange {
                start: 90,
                end: 140
            }
        );
        let residual = receipt.bindings.residual.as_ref().unwrap();
        assert_eq!(
            residual.applied_mode,
            ResidualRenderMode::OriginalSafetyLayer
        );
        assert!(!receipt.diagnostics.iter().any(|diagnostic| diagnostic.code
            == ApplicationDiagnosticCode::SampleSliceTargetRequiresSamplerZone));
        assert!(trigger_clip.muted);
        assert!(project.validate().is_empty());
    }

    #[test]
    fn generated_names_do_not_claim_an_instrument_identity() {
        let mut project = DawProject::new("test", 48_000, 120.0).unwrap();
        let asset = source_asset(&mut project, 1_000);
        let plan = plan_selected_reconstruction(
            &project,
            &set(ReconstructionSelection::EvidencePreferred(
                ReconstructionProposalId::from_raw(1),
            )),
            asset,
        )
        .unwrap();
        plan.prepare(&project)
            .unwrap()
            .commit(&mut project)
            .unwrap();
        let names: Vec<_> = project
            .state()
            .domains
            .arrangement
            .tracks
            .values()
            .map(|track| track.name.as_str())
            .collect();
        assert!(names.contains(&"anonymous hit family 7"));
        assert!(!names
            .iter()
            .any(|name| name.contains("kick") || name.contains("snare")));
    }
}
