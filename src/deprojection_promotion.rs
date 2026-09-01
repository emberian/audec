//! Revision-pinned promotion of source programs into ordinary DAW objects.
//!
//! Compilation is pure and allocates identities only against an immutable
//! project snapshot. Execution submits exactly one aggregate command envelope
//! through [`ProjectSession`], so publication, journaling, undo, and rendering
//! use the same boundary as hand-authored music.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::arrangement::{
    self, ArrangementEditor, ArrangementOperation, Frame, FrameRange, SourceRange, TrackKind,
};
use crate::assets::{self, AssetFrameRange, SampleFrames};
use crate::automation::{
    self, AutomationCommand, LaneChange, ParameterAddress, SegmentShape, TimeDomain, TimePosition,
};
use crate::command::{claims_for_commands, BindingCommand, CommandEnvelope, DomainCommand};
use crate::deprojection_program::{
    CurveTarget, DeprojectionCandidateId, DeprojectionPromotionRequest, Derivation, EditableTerm,
    EditableTermId, EditableTermKind, EvidenceRef, SourceClaimId, SourceProgram, VoiceTerm,
};
use crate::live_project::LiveProjectSnapshot;
use crate::mixer::{BusId, BusKind, MixerCommand};
use crate::project_session::{ProjectEditReceipt, ProjectSession, ProjectSessionError};
use crate::sample_kit::{
    SampleKit, SampleKitPut, SamplePad, SampleRouteIntent, SampleTargetRef, SampleZone,
};
use crate::sample_material::{
    DerivationScope, SampleMaterialProvenance, ScopedEvidenceRef, ScopedProposalRef,
    SourceMaterialRef, VirtualSliceRef,
};
use crate::sequencer::{
    self, BeatDuration, BeatTime, PatternClip, PatternContent, PatternDefinition, PatternOrigin,
    PatternTermHash, SequencerCommand, StepEvent, StepLane, StepPattern, TriggerTarget, PPQ,
};

/// Explicit bridge from an analytic source-claim coordinate system to a media
/// pool asset. `claim_frame_zero` is the asset frame corresponding to claim
/// offset zero; it keeps whole-file literals distinct from extracted outputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedSourceAsset {
    pub asset: assets::AssetId,
    pub claim_frame_zero: u64,
    pub frame_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedPresetInstrument {
    pub instrument: u64,
    pub key: u8,
}

/// All resolution choices which search deliberately leaves outside a
/// `SourceProgram`. No friendly/model-authored label is accepted as a binding.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PromotionBindings {
    pub source_assets: BTreeMap<SourceClaimId, ResolvedSourceAsset>,
    pub preset_instruments: BTreeMap<crate::artifact_catalog::ArtifactId, ResolvedPresetInstrument>,
    pub curve_targets: BTreeMap<EditableTermId, ParameterAddress>,
    /// Required for a standalone note term; preset-backed pattern voices use
    /// their own explicit binding instead.
    pub note_instrument: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromotionPlacement {
    pub start_frame: i64,
    /// Default length for note gestures. Pattern terms carry their analyzed
    /// cycle explicitly and never inherit this placement default.
    pub cycle: BeatDuration,
    pub curve_resolution_frames: u64,
}

impl Default for PromotionPlacement {
    fn default() -> Self {
        Self {
            start_frame: 0,
            cycle: BeatDuration((PPQ * 4) as u64),
            curve_resolution_frames: 64,
        }
    }
}

/// Compact UI-neutral request. The full immutable program is carried so
/// support terms referenced by pattern voices cannot disappear in transit.
#[derive(Clone, Debug, PartialEq)]
pub struct PromotionRequest {
    pub candidate: DeprojectionCandidateId,
    pub expected_project_revision: u64,
    pub program: SourceProgram,
    pub bindings: PromotionBindings,
    pub placement: PromotionPlacement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromotedTermRole {
    SampleSlice,
    Pattern,
    Curve,
    NotesAsInstrumentTriggers,
    PresetBinding,
    ExactAudioFallback,
}

/// The exact symbolic identity retained at the editable-object boundary.
/// Exact audio is a separate variant, never presented as a pattern/curve
/// explanation merely because it can also be promoted and edited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetainedExpression {
    Pattern {
        canonical_source: String,
        term_hash: PatternTermHash,
        cycle: BeatDuration,
        seed: u64,
        initial_cycle_index: u64,
    },
    Curve {
        canonical_source: String,
        target: CurveTarget,
        source_span: (u64, u64),
    },
    ExactAudioFallback {
        source: SourceClaimId,
        span: crate::rhythm::SampleSpan,
    },
}

/// Domain identities are never collapsed to raw integers in a receipt.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CreatedObject {
    ArrangementTrack(arrangement::TrackId),
    AudioClip(arrangement::ClipId),
    ExactAudioFallbackClip(arrangement::ClipId),
    ArrangementPatternClip(arrangement::ClipId),
    ArrangementAutomationClip(arrangement::ClipId),
    SequencerPattern(sequencer::PatternId),
    SequencerPatternClip(sequencer::PatternClipId),
    SequencerLane(sequencer::StepLaneId),
    AutomationLane(automation::AutomationLaneId),
    SampleKit(crate::sample_kit::KitId),
    SamplePad(crate::sample_kit::PadId),
    SampleZone(SampleTargetRef),
    MixerBus(BusId),
}

#[derive(Clone, Debug, PartialEq)]
pub struct TermPromotionProvenance {
    pub term: EditableTermId,
    pub role: PromotedTermRole,
    pub evidence: Vec<EvidenceRef>,
    pub derivation: Derivation,
    pub expression: Option<RetainedExpression>,
    pub created: Vec<CreatedObject>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetainedExpressionState {
    InSync,
    Diverged,
    Missing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedExpressionStatus {
    pub term: EditableTermId,
    pub expression: RetainedExpression,
    pub state: RetainedExpressionState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromotionDiagnostic {
    PatternPlacementRounded {
        requested_frame: i64,
        actual_frame: i64,
    },
    NotePitchCurveRetainedAsEvidence {
        term: EditableTermId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PromotionRefusal {
    UnresolvedPatternVoice {
        term: EditableTermId,
        binding: String,
        family: usize,
    },
    MissingSourceAsset {
        source: SourceClaimId,
    },
    MissingSupportTerm {
        pattern: EditableTermId,
        support: EditableTermId,
    },
    UnsupportedSampleSupport {
        pattern: EditableTermId,
        support: EditableTermId,
    },
    MissingPresetInstrument {
        artifact: crate::artifact_catalog::ArtifactId,
    },
    PresetHasNoExecutableConsumer {
        term: EditableTermId,
    },
    MissingNoteInstrument {
        term: EditableTermId,
    },
    MissingCurveTarget {
        term: EditableTermId,
    },
    UnknownCurveTarget {
        term: EditableTermId,
        target: ParameterAddress,
    },
    EvidenceOnlyCurve {
        term: EditableTermId,
    },
    MalformedPatternTerm {
        term: EditableTermId,
    },
    NonCanonicalPatternTerm {
        term: EditableTermId,
    },
    InvalidPatternCycle {
        term: EditableTermId,
    },
    UnsupportedInitialCycleIndex {
        term: EditableTermId,
        initial_cycle_index: u64,
    },
    EmptyNotes {
        term: EditableTermId,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct PromotionCommandPlan {
    pub candidate: DeprojectionCandidateId,
    pub envelope: CommandEnvelope,
    pub provenance: BTreeMap<EditableTermId, TermPromotionProvenance>,
    pub created: Vec<CreatedObject>,
    pub diagnostics: Vec<PromotionDiagnostic>,
}

impl PromotionCommandPlan {
    pub fn base_revision(&self) -> u64 {
        self.envelope.base_revision
    }
}

#[derive(Clone, Debug)]
pub struct PromotionResult {
    pub candidate: DeprojectionCandidateId,
    pub project: ProjectEditReceipt,
    pub provenance: BTreeMap<EditableTermId, TermPromotionProvenance>,
    pub created: Vec<CreatedObject>,
    pub diagnostics: Vec<PromotionDiagnostic>,
}

impl PromotionResult {
    /// Undo only while this promotion is still the top aggregate edit. This
    /// refuses to undo an unrelated later edit on the caller's behalf.
    pub fn undo(
        &self,
        session: &mut ProjectSession,
    ) -> Result<ProjectEditReceipt, PromotionExecutionError> {
        let actual = session.project_snapshot()?.revisions().aggregate;
        let expected = self.project.publication.revisions.aggregate;
        let expected_label = self.project.history.undo_label.as_deref();
        let current_label = session.history_status()?.undo_label;
        if actual != expected && current_label.as_deref() != expected_label {
            return Err(PromotionExecutionError::NotUndoHead { expected, actual });
        }
        session
            .undo_with_receipt()?
            .ok_or(PromotionExecutionError::UndoUnavailable)
    }

    /// Compare the retained expression identities with ordinary project
    /// objects at any later revision. Pattern edits use the sequencer's
    /// durable divergence bit; curve/fallback edits are compared with the
    /// exact committed promotion snapshot.
    pub fn retained_expression_status(
        &self,
        snapshot: &LiveProjectSnapshot,
    ) -> Vec<RetainedExpressionStatus> {
        let baseline = self.project.publication.snapshot.project.state();
        let current = snapshot.project.state();
        self.provenance
            .values()
            .filter_map(|provenance| {
                let expression = provenance.expression.clone()?;
                let state = match &expression {
                    RetainedExpression::Pattern { .. } => provenance
                        .created
                        .iter()
                        .find_map(|created| match created {
                            CreatedObject::SequencerPattern(id) => Some(*id),
                            _ => None,
                        })
                        .map_or(RetainedExpressionState::Missing, |id| {
                            match current.domains.sequencer.patterns().get(id) {
                                None => RetainedExpressionState::Missing,
                                Some(pattern) if pattern.origin.diverged() => {
                                    RetainedExpressionState::Diverged
                                }
                                Some(pattern)
                                    if baseline.domains.sequencer.patterns().get(id)
                                        == Some(pattern) =>
                                {
                                    RetainedExpressionState::InSync
                                }
                                Some(_) => RetainedExpressionState::Diverged,
                            }
                        }),
                    RetainedExpression::Curve { .. } => provenance
                        .created
                        .iter()
                        .find_map(|created| match created {
                            CreatedObject::AutomationLane(id) => Some(*id),
                            _ => None,
                        })
                        .map_or(RetainedExpressionState::Missing, |id| {
                            match (
                                baseline.domains.automation.lane(id),
                                current.domains.automation.lane(id),
                            ) {
                                (_, None) => RetainedExpressionState::Missing,
                                (Some(before), Some(after)) if before == after => {
                                    RetainedExpressionState::InSync
                                }
                                _ => RetainedExpressionState::Diverged,
                            }
                        }),
                    RetainedExpression::ExactAudioFallback { .. } => provenance
                        .created
                        .iter()
                        .find_map(|created| match created {
                            CreatedObject::ExactAudioFallbackClip(id) => Some(*id),
                            _ => None,
                        })
                        .map_or(RetainedExpressionState::Missing, |id| {
                            match (
                                baseline.domains.arrangement.clip(id),
                                current.domains.arrangement.clip(id),
                            ) {
                                (_, None) => RetainedExpressionState::Missing,
                                (Some(before), Some(after)) if before == after => {
                                    RetainedExpressionState::InSync
                                }
                                _ => RetainedExpressionState::Diverged,
                            }
                        }),
                };
                Some(RetainedExpressionStatus {
                    term: provenance.term,
                    expression,
                    state,
                })
            })
            .collect()
    }
}

#[derive(Debug)]
pub enum PromotionCompileError {
    RevisionConflict { expected: u64, actual: u64 },
    CandidateMismatch,
    Refused(Vec<PromotionRefusal>),
    Invalid(String),
}

impl fmt::Display for PromotionCompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RevisionConflict { expected, actual } => write!(
                formatter,
                "promotion was compiled for project revision {expected}, current revision is {actual}"
            ),
            Self::CandidateMismatch => formatter.write_str("promotion candidate identity mismatch"),
            Self::Refused(reasons) => write!(formatter, "promotion refused for {} typed reason(s)", reasons.len()),
            Self::Invalid(detail) => write!(formatter, "invalid promotion plan: {detail}"),
        }
    }
}

impl std::error::Error for PromotionCompileError {}

#[derive(Debug)]
pub enum PromotionExecutionError {
    Session(ProjectSessionError),
    NotUndoHead { expected: u64, actual: u64 },
    UndoUnavailable,
    ReceiptMismatch(String),
}

impl fmt::Display for PromotionExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(error) => error.fmt(formatter),
            Self::NotUndoHead { expected, actual } => write!(
                formatter,
                "promotion revision {expected} is not the undo head (current revision {actual})"
            ),
            Self::UndoUnavailable => formatter.write_str("promotion undo is unavailable"),
            Self::ReceiptMismatch(detail) => {
                write!(formatter, "promotion receipt mismatch: {detail}")
            }
        }
    }
}

impl std::error::Error for PromotionExecutionError {}

impl From<ProjectSessionError> for PromotionExecutionError {
    fn from(value: ProjectSessionError) -> Self {
        Self::Session(value)
    }
}

/// Preserve the existing Cycle-8 request seam while filling the richer
/// resolver/placement inputs required by executable promotion.
pub fn adapt_promotion_request(
    request: DeprojectionPromotionRequest,
    source: crate::deprojection_program::MaterialSpan,
    bindings: PromotionBindings,
    mut placement: PromotionPlacement,
) -> Result<PromotionRequest, PromotionCompileError> {
    match request {
        DeprojectionPromotionRequest::Reconstruction {
            candidate,
            expected_project_revision,
            program,
        } => Ok(PromotionRequest {
            candidate,
            expected_project_revision,
            program,
            bindings,
            placement,
        }),
        DeprojectionPromotionRequest::Pattern {
            candidate,
            expected_project_revision,
            term,
            place_at_frame,
        } => {
            if let Some(frame) = place_at_frame {
                placement.start_frame = i64::try_from(frame).map_err(|_| {
                    PromotionCompileError::Invalid(
                        "pattern placement does not fit project-frame time".into(),
                    )
                })?;
            }
            let program = single_term_program(term, source)?;
            Ok(PromotionRequest {
                candidate,
                expected_project_revision,
                program,
                bindings,
                placement,
            })
        }
        DeprojectionPromotionRequest::Curve {
            candidate,
            expected_project_revision,
            term,
            parameter_hint: _,
        } => {
            let program = single_term_program(term, source)?;
            Ok(PromotionRequest {
                candidate,
                expected_project_revision,
                program,
                bindings,
                placement,
            })
        }
    }
}

fn single_term_program(
    term: EditableTerm,
    source: crate::deprojection_program::MaterialSpan,
) -> Result<SourceProgram, PromotionCompileError> {
    SourceProgram::new(source, vec![term.clone()], vec![term.id])
        .map_err(|error| PromotionCompileError::Invalid(error.to_string()))
}

pub fn compile_promotion(
    snapshot: &LiveProjectSnapshot,
    request: PromotionRequest,
) -> Result<PromotionCommandPlan, PromotionCompileError> {
    let actual = snapshot.revisions().aggregate;
    if actual != request.expected_project_revision {
        return Err(PromotionCompileError::RevisionConflict {
            expected: request.expected_project_revision,
            actual,
        });
    }
    validate_refusals(snapshot, &request)?;
    Compiler::new(snapshot, request)?.compile()
}

pub fn execute_promotion(
    session: &mut ProjectSession,
    plan: PromotionCommandPlan,
) -> Result<PromotionResult, PromotionExecutionError> {
    let PromotionCommandPlan {
        candidate,
        envelope,
        provenance,
        created,
        diagnostics,
    } = plan;
    let project = session.execute_envelope(envelope)?;
    verify_created(&project.publication.snapshot, &created)?;
    Ok(PromotionResult {
        candidate,
        project,
        provenance,
        created,
        diagnostics,
    })
}

pub fn promote(
    session: &mut ProjectSession,
    request: PromotionRequest,
) -> Result<PromotionResult, PromotionError> {
    let snapshot = session.project_snapshot()?.clone();
    let plan = compile_promotion(&snapshot, request)?;
    execute_promotion(session, plan).map_err(Into::into)
}

#[derive(Debug)]
pub enum PromotionError {
    Compile(PromotionCompileError),
    Execute(PromotionExecutionError),
    Session(ProjectSessionError),
}

impl From<PromotionCompileError> for PromotionError {
    fn from(value: PromotionCompileError) -> Self {
        Self::Compile(value)
    }
}
impl From<PromotionExecutionError> for PromotionError {
    fn from(value: PromotionExecutionError) -> Self {
        Self::Execute(value)
    }
}
impl From<ProjectSessionError> for PromotionError {
    fn from(value: ProjectSessionError) -> Self {
        Self::Session(value)
    }
}

impl fmt::Display for PromotionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compile(error) => error.fmt(formatter),
            Self::Execute(error) => error.fmt(formatter),
            Self::Session(error) => error.fmt(formatter),
        }
    }
}
impl std::error::Error for PromotionError {}

fn validate_refusals(
    snapshot: &LiveProjectSnapshot,
    request: &PromotionRequest,
) -> Result<(), PromotionCompileError> {
    let mut refusals = request
        .program
        .compile_refusals()
        .into_iter()
        .map(|refusal| match refusal {
            crate::deprojection_program::ProgramCompileRefusal::UnresolvedPatternVoice {
                term,
                binding,
                family,
            } => PromotionRefusal::UnresolvedPatternVoice {
                term,
                binding,
                family,
            },
        })
        .collect::<Vec<_>>();
    let consumed_presets = request
        .program
        .roots
        .iter()
        .filter_map(|root| match &request.program.terms[root].kind {
            EditableTermKind::Pattern { voices, .. } => Some(voices.values()),
            _ => None,
        })
        .flatten()
        .filter_map(|voice| match voice {
            VoiceTerm::Preset(artifact) => Some(*artifact),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    for root in &request.program.roots {
        let term = &request.program.terms[root];
        match &term.kind {
            EditableTermKind::SampleSlice { source, .. }
            | EditableTermKind::ExactAudioReference { source, .. } => {
                if !request.bindings.source_assets.contains_key(source) {
                    refusals.push(PromotionRefusal::MissingSourceAsset { source: *source });
                }
            }
            EditableTermKind::Pattern {
                source,
                execution,
                voices,
            } => {
                match crate::pattern_lang::parse(source) {
                    Err(_) => {
                        refusals.push(PromotionRefusal::MalformedPatternTerm { term: term.id })
                    }
                    Ok(expression) if crate::pattern_lang::print(&expression) != *source => {
                        refusals.push(PromotionRefusal::NonCanonicalPatternTerm { term: term.id })
                    }
                    Ok(_) => {}
                }
                if execution.cycle.0 == 0 || execution.cycle.0 > i64::MAX as u64 {
                    refusals.push(PromotionRefusal::InvalidPatternCycle { term: term.id });
                }
                // PatternDefinition currently advances expression cycles from
                // the first placement cycle. Refuse a shifted analytic term
                // instead of materializing cycle N and later replaying cycle 0.
                if execution.initial_cycle_index != 0 {
                    refusals.push(PromotionRefusal::UnsupportedInitialCycleIndex {
                        term: term.id,
                        initial_cycle_index: execution.initial_cycle_index,
                    });
                }
                for voice in voices.values() {
                    match voice {
                        VoiceTerm::Sample(support) => match request.program.terms.get(support) {
                            None => refusals.push(PromotionRefusal::MissingSupportTerm {
                                pattern: term.id,
                                support: *support,
                            }),
                            Some(EditableTerm {
                                kind: EditableTermKind::SampleSlice { source, .. },
                                ..
                            }) => {
                                if !request.bindings.source_assets.contains_key(source) {
                                    refusals.push(PromotionRefusal::MissingSourceAsset {
                                        source: *source,
                                    });
                                }
                            }
                            Some(_) => refusals.push(PromotionRefusal::UnsupportedSampleSupport {
                                pattern: term.id,
                                support: *support,
                            }),
                        },
                        VoiceTerm::AudioClaim(source) => {
                            if !request.bindings.source_assets.contains_key(source) {
                                refusals
                                    .push(PromotionRefusal::MissingSourceAsset { source: *source });
                            }
                        }
                        VoiceTerm::Preset(artifact) => {
                            if !request.bindings.preset_instruments.contains_key(artifact) {
                                refusals.push(PromotionRefusal::MissingPresetInstrument {
                                    artifact: *artifact,
                                });
                            }
                        }
                        VoiceTerm::UnresolvedFamily { .. } => {}
                    }
                }
            }
            EditableTermKind::Curve { expression, .. } => {
                let Some(target) = request.bindings.curve_targets.get(&term.id) else {
                    refusals.push(PromotionRefusal::MissingCurveTarget { term: term.id });
                    continue;
                };
                if snapshot
                    .project
                    .state()
                    .domains
                    .automation
                    .descriptors()
                    .all(|descriptor| &descriptor.address != target)
                {
                    refusals.push(PromotionRefusal::UnknownCurveTarget {
                        term: term.id,
                        target: target.clone(),
                    });
                }
                if contains_evidence_curve(expression) {
                    refusals.push(PromotionRefusal::EvidenceOnlyCurve { term: term.id });
                }
            }
            EditableTermKind::Notes { gestures } => {
                if gestures.is_empty() {
                    refusals.push(PromotionRefusal::EmptyNotes { term: term.id });
                }
                if request.bindings.note_instrument.is_none() {
                    refusals.push(PromotionRefusal::MissingNoteInstrument { term: term.id });
                }
            }
            EditableTermKind::PresetCandidate { artifact, .. } => {
                if !consumed_presets.contains(artifact) {
                    refusals
                        .push(PromotionRefusal::PresetHasNoExecutableConsumer { term: term.id });
                }
            }
        }
    }
    refusals.sort();
    refusals.dedup();
    if refusals.is_empty() {
        Ok(())
    } else {
        Err(PromotionCompileError::Refused(refusals))
    }
}

fn contains_evidence_curve(expression: &crate::curve_lang::CurveExpr) -> bool {
    use crate::curve_lang::CurveExpr;
    match expression {
        CurveExpr::FromEvidence(_) => true,
        CurveExpr::Sum(members) => members.iter().any(contains_evidence_curve),
        CurveExpr::Scale { input, .. } | CurveExpr::Clamp { input, .. } => {
            contains_evidence_curve(input)
        }
        _ => false,
    }
}

struct Compiler<'a> {
    snapshot: &'a LiveProjectSnapshot,
    request: PromotionRequest,
    state: crate::daw_project::ProjectState,
    commands: Vec<DomainCommand>,
    provenance: BTreeMap<EditableTermId, TermPromotionProvenance>,
    created: BTreeSet<CreatedObject>,
    diagnostics: Vec<PromotionDiagnostic>,
    audio_track: Option<(arrangement::TrackId, BusId)>,
    pattern_track: Option<(arrangement::TrackId, BusId)>,
    automation_track: Option<arrangement::TrackId>,
    kit: Option<SampleKit>,
    kit_aliases: BTreeMap<EditableTermId, TriggerTarget>,
}

impl<'a> Compiler<'a> {
    fn new(
        snapshot: &'a LiveProjectSnapshot,
        request: PromotionRequest,
    ) -> Result<Self, PromotionCompileError> {
        Ok(Self {
            state: snapshot.project.state().clone(),
            snapshot,
            request,
            commands: Vec::new(),
            provenance: BTreeMap::new(),
            created: BTreeSet::new(),
            diagnostics: Vec::new(),
            audio_track: None,
            pattern_track: None,
            automation_track: None,
            kit: None,
            kit_aliases: BTreeMap::new(),
        })
    }

    fn compile(mut self) -> Result<PromotionCommandPlan, PromotionCompileError> {
        let roots = self.request.program.roots.clone();
        for root in roots {
            let term = self.request.program.terms[&root].clone();
            match &term.kind {
                EditableTermKind::SampleSlice {
                    source,
                    span,
                    onset_offset_frames,
                } => {
                    let binding = self.request.bindings.source_assets[source];
                    let start = self
                        .request
                        .placement
                        .start_frame
                        .checked_add(
                            i64::try_from(*onset_offset_frames)
                                .map_err(|_| invalid("sample onset does not fit project time"))?,
                        )
                        .ok_or_else(|| invalid("sample placement overflow"))?;
                    let clip = self.add_audio_clip(
                        &term,
                        binding,
                        span.start as u64,
                        span.end as u64,
                        start,
                        false,
                    )?;
                    self.record(
                        &term,
                        PromotedTermRole::SampleSlice,
                        vec![CreatedObject::AudioClip(clip)],
                    );
                }
                EditableTermKind::ExactAudioReference { source, span } => {
                    let binding = self.request.bindings.source_assets[source];
                    let clip = self.add_audio_clip(
                        &term,
                        binding,
                        span.start as u64,
                        span.end as u64,
                        self.request.placement.start_frame,
                        true,
                    )?;
                    self.record(
                        &term,
                        PromotedTermRole::ExactAudioFallback,
                        vec![CreatedObject::ExactAudioFallbackClip(clip)],
                    );
                }
                EditableTermKind::Pattern { .. } => self.add_pattern(&term)?,
                EditableTermKind::Curve { .. } => self.add_curve(&term)?,
                EditableTermKind::Notes { .. } => self.add_notes(&term)?,
                EditableTermKind::PresetCandidate { .. } => {}
            }
        }
        self.flush_kit()?;
        if self.commands.is_empty() {
            return Err(invalid("promotion produced no executable commands"));
        }
        let created = self.created.into_iter().collect::<Vec<_>>();
        let envelope = CommandEnvelope {
            label: format!(
                "Promote construction {}",
                candidate_tag(self.request.candidate)
            ),
            base_revision: self.request.expected_project_revision,
            coalesce: None,
            id_claims: claims_for_commands(&self.commands),
            commands: self.commands,
        };
        Ok(PromotionCommandPlan {
            candidate: self.request.candidate,
            envelope,
            provenance: self.provenance,
            created,
            diagnostics: self.diagnostics,
        })
    }

    fn record(
        &mut self,
        term: &EditableTerm,
        role: PromotedTermRole,
        mut objects: Vec<CreatedObject>,
    ) {
        objects.sort();
        objects.dedup();
        self.created.extend(objects.iter().cloned());
        self.provenance.insert(
            term.id,
            TermPromotionProvenance {
                term: term.id,
                role,
                evidence: term.evidence.clone(),
                derivation: term.derivation.clone().normalized(),
                expression: retained_expression(term),
                created: objects,
            },
        );
    }

    fn ensure_bus_track(
        &mut self,
        kind: TrackKind,
    ) -> Result<(arrangement::TrackId, BusId), PromotionCompileError> {
        let cached = match kind {
            TrackKind::Audio => self.audio_track,
            TrackKind::Pattern => self.pattern_track,
            _ => None,
        };
        if let Some(value) = cached {
            return Ok(value);
        }
        let mut allocated = None;
        let name = format!(
            "Anonymous construction {}",
            candidate_tag(self.request.candidate)
        );
        let mixer = MixerCommand::build(
            "Create anonymous construction bus",
            &self.state.domains.mixer,
            |graph| {
                allocated = Some(graph.add_bus(BusKind::Source, name.clone())?);
                Ok(())
            },
        )
        .map_err(|error| invalid(error.to_string()))?;
        let bus = allocated.expect("mixer edit allocates a bus");
        self.state.domains.mixer = mixer.after().clone();
        self.commands.push(DomainCommand::Mixer(mixer));
        self.created.insert(CreatedObject::MixerBus(bus));

        let mut editor = ArrangementEditor::from_state(self.state.domains.arrangement.clone())
            .map_err(|error| invalid(error.to_string()))?;
        let track = editor
            .create_track(name, kind)
            .map_err(|error| invalid(error.to_string()))?;
        let after_track = editor
            .state()
            .track(track)
            .cloned()
            .expect("new track exists");
        let put_track = ArrangementOperation::PutTrack {
            before: None,
            after: Some(after_track),
        };
        self.state
            .domains
            .arrangement
            .apply_operations(std::slice::from_ref(&put_track))
            .map_err(|error| invalid(error.to_string()))?;
        self.commands.push(DomainCommand::Arrangement(put_track));
        let binding = BindingCommand::PutTrackBus {
            track,
            before: None,
            after: Some(bus),
        };
        self.state.bindings.mixer.tracks.insert(track, bus);
        self.commands.push(DomainCommand::Bindings(binding));
        self.created.insert(CreatedObject::ArrangementTrack(track));
        match kind {
            TrackKind::Audio => self.audio_track = Some((track, bus)),
            TrackKind::Pattern => self.pattern_track = Some((track, bus)),
            _ => {}
        }
        Ok((track, bus))
    }

    fn ensure_automation_track(&mut self) -> Result<arrangement::TrackId, PromotionCompileError> {
        if let Some(track) = self.automation_track {
            return Ok(track);
        }
        let name = format!(
            "Anonymous automation {}",
            candidate_tag(self.request.candidate)
        );
        let mut editor = ArrangementEditor::from_state(self.state.domains.arrangement.clone())
            .map_err(|error| invalid(error.to_string()))?;
        let track = editor
            .create_track(name, TrackKind::Automation)
            .map_err(|error| invalid(error.to_string()))?;
        let after_track = editor
            .state()
            .track(track)
            .cloned()
            .expect("new track exists");
        let put_track = ArrangementOperation::PutTrack {
            before: None,
            after: Some(after_track),
        };
        self.state
            .domains
            .arrangement
            .apply_operations(std::slice::from_ref(&put_track))
            .map_err(|error| invalid(error.to_string()))?;
        self.commands.push(DomainCommand::Arrangement(put_track));
        self.created.insert(CreatedObject::ArrangementTrack(track));
        self.automation_track = Some(track);
        Ok(track)
    }

    fn add_audio_clip(
        &mut self,
        term: &EditableTerm,
        source: ResolvedSourceAsset,
        local_start: u64,
        local_end: u64,
        start: i64,
        exact_fallback: bool,
    ) -> Result<arrangement::ClipId, PromotionCompileError> {
        if local_start >= local_end || local_end > source.frame_count {
            return Err(invalid("source term range lies outside its resolved claim"));
        }
        if self.state.domains.assets.get(source.asset).is_none() {
            return Err(invalid(format!(
                "resolved source asset {} is not registered",
                source.asset.0
            )));
        }
        let (track, _) = self.ensure_bus_track(TrackKind::Audio)?;
        let before_aliases = self.state.bindings.assets.arrangement_assets.clone();
        let alias = self
            .state
            .bindings
            .bind_media_asset(source.asset)
            .map_err(|error| invalid(error.to_string()))?;
        if !before_aliases.contains_key(&alias) {
            self.commands.push(DomainCommand::Bindings(
                BindingCommand::PutMediaAssetAlias {
                    alias,
                    before: None,
                    after: Some(source.asset),
                },
            ));
        }
        let source_start = source
            .claim_frame_zero
            .checked_add(local_start)
            .ok_or_else(|| invalid("source range overflow"))?;
        let source_end = source
            .claim_frame_zero
            .checked_add(local_end)
            .ok_or_else(|| invalid("source range overflow"))?;
        let placement = FrameRange::from_start_and_len(Frame(start), local_end - local_start)
            .map_err(|error| invalid(error.to_string()))?;
        let mut editor = ArrangementEditor::from_state(self.state.domains.arrangement.clone())
            .map_err(|error| invalid(error.to_string()))?;
        let clip = editor
            .create_audio_clip(
                track,
                anonymous_term_name(
                    term.id,
                    if exact_fallback {
                        "exact audio"
                    } else {
                        "sample"
                    },
                ),
                placement,
                alias,
                SourceRange::new(source_start, source_end)
                    .map_err(|error| invalid(error.to_string()))?,
            )
            .map_err(|error| invalid(error.to_string()))?;
        let after = editor.state().clip(clip).cloned().expect("new clip exists");
        let command = ArrangementOperation::PutClip {
            before: None,
            after: Some(after),
        };
        self.state
            .domains
            .arrangement
            .apply_operations(std::slice::from_ref(&command))
            .map_err(|error| invalid(error.to_string()))?;
        self.commands.push(DomainCommand::Arrangement(command));
        Ok(clip)
    }

    fn resolve_voice(
        &mut self,
        pattern: EditableTermId,
        binding: &str,
        voice: &VoiceTerm,
    ) -> Result<TriggerTarget, PromotionCompileError> {
        match voice {
            VoiceTerm::Preset(artifact) => {
                let resolved = self.request.bindings.preset_instruments[artifact];
                if let Some(preset) = self
                    .request
                    .program
                    .terms
                    .values()
                    .find(|candidate| {
                        matches!(
                            &candidate.kind,
                            EditableTermKind::PresetCandidate {
                                artifact: candidate_artifact,
                                ..
                            } if candidate_artifact == artifact
                        )
                    })
                    .cloned()
                {
                    if !self.provenance.contains_key(&preset.id) {
                        self.record(&preset, PromotedTermRole::PresetBinding, Vec::new());
                    }
                }
                Ok(TriggerTarget::InstrumentNote {
                    instrument: resolved.instrument,
                    key: resolved.key,
                })
            }
            VoiceTerm::Sample(term) => {
                if let Some(target) = self.kit_aliases.get(term) {
                    return Ok(target.clone());
                }
                let support = self.request.program.terms[term].clone();
                let EditableTermKind::SampleSlice { source, span, .. } = support.kind else {
                    return Err(invalid("validated sample support changed kind"));
                };
                let resolved = self.request.bindings.source_assets[&source];
                let target = self.add_zone(
                    support.id,
                    resolved,
                    span.start as u64,
                    span.end as u64,
                    binding,
                )?;
                let kit = self.kit.as_ref().expect("sample target owns a kit");
                let pad = *kit.pad_order.last().expect("sample target owns a pad");
                let zone = *kit.pads[&pad]
                    .zone_order
                    .last()
                    .expect("sample target owns a zone");
                self.record(
                    &support,
                    PromotedTermRole::SampleSlice,
                    vec![
                        CreatedObject::SampleKit(kit.id),
                        CreatedObject::SamplePad(pad),
                        CreatedObject::SampleZone(SampleTargetRef {
                            kit: kit.id,
                            pad,
                            zone,
                        }),
                    ],
                );
                self.kit_aliases.insert(*term, target.clone());
                Ok(target)
            }
            VoiceTerm::AudioClaim(source) => {
                let resolved = self.request.bindings.source_assets[source];
                self.add_zone(pattern, resolved, 0, resolved.frame_count, binding)
            }
            VoiceTerm::UnresolvedFamily { .. } => {
                Err(invalid("validated unresolved voice reached lowering"))
            }
        }
    }

    fn ensure_kit(&mut self) -> Result<&mut SampleKit, PromotionCompileError> {
        if self.kit.is_none() {
            let library = &mut self.state.domains.sample_kits;
            let kit_id = library
                .allocate_kit_id()
                .map_err(|error| invalid(error.to_string()))?;
            let output = self
                .pattern_track
                .map(|(_, bus)| bus)
                .ok_or_else(|| invalid("pattern bus not allocated"))?;
            self.kit = Some(SampleKit::new(
                kit_id,
                format!("Anonymous kit {}", candidate_tag(self.request.candidate)),
                SampleRouteIntent::new(output).map_err(|error| invalid(error.to_string()))?,
            ));
            self.created.insert(CreatedObject::SampleKit(kit_id));
        }
        Ok(self.kit.as_mut().expect("initialized above"))
    }

    fn add_zone(
        &mut self,
        owner: EditableTermId,
        resolved: ResolvedSourceAsset,
        local_start: u64,
        local_end: u64,
        binding_name: &str,
    ) -> Result<TriggerTarget, PromotionCompileError> {
        if local_start >= local_end || local_end > resolved.frame_count {
            return Err(invalid(
                "sample voice range lies outside its resolved claim",
            ));
        }
        if self.state.domains.assets.get(resolved.asset).is_none() {
            return Err(invalid(format!(
                "resolved source asset {} is not registered",
                resolved.asset.0
            )));
        }
        let pad_id = self
            .state
            .domains
            .sample_kits
            .allocate_pad_id()
            .map_err(|error| invalid(error.to_string()))?;
        let zone_id = self
            .state
            .domains
            .sample_kits
            .allocate_zone_id()
            .map_err(|error| invalid(error.to_string()))?;
        let source_range = AssetFrameRange {
            start: SampleFrames(resolved.claim_frame_zero + local_start),
            end: SampleFrames(resolved.claim_frame_zero + local_end),
        };
        let slice = VirtualSliceRef::new(resolved.asset, source_range)
            .map_err(|error| invalid(error.to_string()))?;
        let scope = derivation_scope(self.request.candidate);
        let evidence = self
            .request
            .program
            .terms
            .get(&owner)
            .map(|term| {
                term.evidence
                    .iter()
                    .map(|evidence| scoped_evidence(scope, evidence))
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let mut zone = SampleZone::new(zone_id, pad_id, SourceMaterialRef::VirtualSlice(slice));
        zone.provenance = SampleMaterialProvenance::Deprojection {
            proposal: ScopedProposalRef {
                scope,
                local: nonzero_u64(&owner.0.bytes),
            },
            evidence: evidence.iter().copied().collect(),
        };
        zone.evidence = evidence;
        let pad = SamplePad {
            id: pad_id,
            name: format!("voice {}", binding_name),
            choke_group: None,
            zone_order: vec![zone_id],
        };
        let kit = self.ensure_kit()?;
        kit.pads.insert(pad_id, pad);
        kit.pad_order.push(pad_id);
        kit.zones.insert(zone_id, zone);
        let target = SampleTargetRef {
            kit: kit.id,
            pad: pad_id,
            zone: zone_id,
        };
        let before = self.state.bindings.sample_targets.targets.clone();
        let alias = self
            .state
            .bindings
            .bind_sample_target(target)
            .map_err(|error| invalid(error.to_string()))?;
        if !before.contains_key(&alias) {
            self.commands.push(DomainCommand::Bindings(
                BindingCommand::PutSampleTargetAlias {
                    alias,
                    before: None,
                    after: Some(target),
                },
            ));
        }
        self.created.extend([
            CreatedObject::SamplePad(pad_id),
            CreatedObject::SampleZone(target),
        ]);
        Ok(TriggerTarget::Sample(alias))
    }

    fn flush_kit(&mut self) -> Result<(), PromotionCompileError> {
        let Some(kit) = self.kit.take() else {
            return Ok(());
        };
        kit.validate().map_err(|error| invalid(error.to_string()))?;
        self.commands.insert(
            0,
            DomainCommand::SampleKits(SampleKitPut {
                before: None,
                after: Some(kit),
            }),
        );
        Ok(())
    }

    fn add_pattern(&mut self, term: &EditableTerm) -> Result<(), PromotionCompileError> {
        let EditableTermKind::Pattern {
            source,
            execution,
            voices,
        } = &term.kind
        else {
            unreachable!()
        };
        let (_, _) = self.ensure_bus_track(TrackKind::Pattern)?;
        let mut bindings = BTreeMap::new();
        for (name, voice) in voices {
            bindings.insert(name.clone(), self.resolve_voice(term.id, name, voice)?);
        }
        let expr =
            crate::pattern_lang::parse(source).map_err(|error| invalid(error.to_string()))?;
        let output = crate::pattern_lang::eval_steps(
            &expr,
            &crate::pattern_lang::EvalContext {
                bindings: &bindings,
                cycle: execution.cycle,
                seed: execution.seed,
                cycle_index: execution.initial_cycle_index,
            },
        )
        .map_err(|error| invalid(error.to_string()))?;
        let mut lanes = BTreeMap::new();
        for mut lane in output.pattern.lanes.into_values() {
            let id = self.state.domains.sequencer.allocate_step_lane_id();
            lane.id = id;
            lanes.insert(id, lane);
            self.created.insert(CreatedObject::SequencerLane(id));
        }
        let content = PatternContent::Steps(StepPattern {
            resolution: output.pattern.resolution,
            swing: output.pattern.swing,
            lanes,
        });
        self.add_pattern_objects(
            term,
            content,
            PatternOrigin::Expression {
                source: source.clone(),
                term_hash: crate::pattern_lang::term_hash(&expr),
                bindings_hash: crate::pattern_lang::bindings_hash(&bindings),
                bindings,
                diverged: false,
            },
            PromotedTermRole::Pattern,
            execution.cycle,
        )
    }

    fn add_notes(&mut self, term: &EditableTerm) -> Result<(), PromotionCompileError> {
        let EditableTermKind::Notes { gestures } = &term.kind else {
            unreachable!()
        };
        self.ensure_bus_track(TrackKind::Pattern)?;
        let instrument = self.request.bindings.note_instrument.expect("validated");
        let tempo = self.state.domains.sequencer.tempo_map().clone();
        let base = self.request.placement.start_frame;
        let mut lanes = BTreeMap::new();
        let mut required_length = self.request.placement.cycle.0;
        for gesture in gestures {
            let lane_id = self.state.domains.sequencer.allocate_step_lane_id();
            let relative_start = i64::try_from(gesture.start_frame)
                .map_err(|_| invalid("note start does not fit project time"))?;
            let at = tempo
                .frame_to_beat_floor(sequencer::ProjectFrame(base.saturating_add(relative_start)))
                .0
                - tempo.frame_to_beat_floor(sequencer::ProjectFrame(base)).0;
            let end = tempo
                .frame_to_beat_floor(sequencer::ProjectFrame(
                    base.saturating_add(relative_start)
                        .saturating_add(i64::try_from(gesture.duration_frames).unwrap_or(i64::MAX)),
                ))
                .0
                - tempo.frame_to_beat_floor(sequencer::ProjectFrame(base)).0;
            let gate = BeatDuration(u64::try_from(end.saturating_sub(at)).unwrap_or(1).max(1));
            let resolution = BeatDuration(1);
            let index = u32::try_from(at.max(0))
                .map_err(|_| invalid("note tick does not fit step index"))?;
            let cents = gesture
                .pitch_curve_cents
                .first()
                .map_or(0.0, |(_, value)| *value)
                / 100.0;
            let event = StepEvent {
                velocity: gesture.velocity,
                probability: 1.0,
                micro_offset: 0,
                gate,
                ratchets: 1,
                pitch_semitones: cents,
                pan: 0.0,
            };
            lanes.insert(
                lane_id,
                StepLane {
                    id: lane_id,
                    name: format!("note {}", gesture.midi_key),
                    target: TriggerTarget::InstrumentNote {
                        instrument,
                        key: gesture.midi_key,
                    },
                    choke_group: None,
                    steps: BTreeMap::from([(index, event)]),
                },
            );
            self.created.insert(CreatedObject::SequencerLane(lane_id));
            if !gesture.pitch_curve_cents.is_empty() {
                self.diagnostics
                    .push(PromotionDiagnostic::NotePitchCurveRetainedAsEvidence { term: term.id });
            }
            required_length =
                required_length.max(u64::from(index).saturating_add(gate.0).saturating_add(1));
            let _ = resolution;
        }
        self.add_pattern_objects(
            term,
            PatternContent::Steps(StepPattern {
                resolution: BeatDuration(1),
                swing: 0.0,
                lanes,
            }),
            PatternOrigin::Deprojected {
                proposal: crate::reconstruction::ReconstructionProposalId::from_raw(nonzero_u64(
                    &self.request.candidate.0.bytes,
                )),
                diverged: false,
            },
            PromotedTermRole::NotesAsInstrumentTriggers,
            BeatDuration(required_length),
        )
    }

    fn add_pattern_objects(
        &mut self,
        term: &EditableTerm,
        content: PatternContent,
        origin: PatternOrigin,
        role: PromotedTermRole,
        pattern_length: BeatDuration,
    ) -> Result<(), PromotionCompileError> {
        let (track, _) = self.pattern_track.expect("pattern track allocated");
        let pattern_id = self.state.domains.sequencer.allocate_pattern_id();
        let definition = PatternDefinition {
            id: pattern_id,
            name: anonymous_term_name(term.id, "pattern"),
            length: pattern_length,
            content,
            origin,
            revision: 1,
        };
        let put_pattern = SequencerCommand::PutPattern {
            before: None,
            after: Some(definition),
        };
        self.state
            .domains
            .sequencer
            .apply_without_history(std::slice::from_ref(&put_pattern))
            .map_err(|error| invalid(error.to_string()))?;
        self.commands.push(DomainCommand::Sequencer(put_pattern));
        let before_defs = self.state.bindings.patterns.definitions.clone();
        let arrangement_pattern = self
            .state
            .bindings
            .bind_pattern_definition(pattern_id)
            .map_err(|error| invalid(error.to_string()))?;
        if !before_defs.contains_key(&arrangement_pattern) {
            self.commands.push(DomainCommand::Bindings(
                BindingCommand::PutPatternDefinitionAlias {
                    alias: arrangement_pattern,
                    before: None,
                    after: Some(pattern_id),
                },
            ));
        }
        let tempo = self.state.domains.sequencer.tempo_map().clone();
        let requested = self.request.placement.start_frame;
        let start_beat = tempo.frame_to_beat_floor(sequencer::ProjectFrame(requested));
        let actual = tempo.beat_to_frame(start_beat).0;
        if actual != requested {
            self.diagnostics
                .push(PromotionDiagnostic::PatternPlacementRounded {
                    requested_frame: requested,
                    actual_frame: actual,
                });
        }
        let seq_clip_id = self.state.domains.sequencer.allocate_clip_id();
        let seq_clip = PatternClip {
            id: seq_clip_id,
            pattern: pattern_id,
            start: start_beat,
            length: pattern_length,
            pattern_offset: BeatTime(0),
            looped: false,
            transpose_semitones: 0.0,
            gain: 1.0,
            muted: false,
        };
        let put_seq_clip = SequencerCommand::PutClip {
            before: None,
            after: Some(seq_clip),
        };
        self.state
            .domains
            .sequencer
            .apply_without_history(std::slice::from_ref(&put_seq_clip))
            .map_err(|error| invalid(error.to_string()))?;
        self.commands.push(DomainCommand::Sequencer(put_seq_clip));
        let end = tempo
            .beat_to_frame(BeatTime(
                start_beat
                    .0
                    .saturating_add(pattern_length.0.min(i64::MAX as u64) as i64),
            ))
            .0;
        let placement = FrameRange::new(Frame(actual), Frame(end))
            .map_err(|error| invalid(error.to_string()))?;
        let mut editor = ArrangementEditor::from_state(self.state.domains.arrangement.clone())
            .map_err(|error| invalid(error.to_string()))?;
        let arrangement_clip = editor
            .create_pattern_clip(
                track,
                anonymous_term_name(term.id, "pattern"),
                placement,
                arrangement_pattern,
            )
            .map_err(|error| invalid(error.to_string()))?;
        let clip = editor
            .state()
            .clip(arrangement_clip)
            .cloned()
            .expect("new clip exists");
        let put_arr_clip = ArrangementOperation::PutClip {
            before: None,
            after: Some(clip),
        };
        self.state
            .domains
            .arrangement
            .apply_operations(std::slice::from_ref(&put_arr_clip))
            .map_err(|error| invalid(error.to_string()))?;
        self.commands.push(DomainCommand::Arrangement(put_arr_clip));
        self.state
            .bindings
            .patterns
            .placements
            .insert(arrangement_clip, seq_clip_id);
        self.commands.push(DomainCommand::Bindings(
            BindingCommand::PutPatternPlacement {
                clip: arrangement_clip,
                before: None,
                after: Some(seq_clip_id),
            },
        ));
        let mut objects = vec![
            CreatedObject::SequencerPattern(pattern_id),
            CreatedObject::SequencerPatternClip(seq_clip_id),
            CreatedObject::ArrangementPatternClip(arrangement_clip),
            CreatedObject::ArrangementTrack(track),
        ];
        if let PatternContent::Steps(steps) = &self
            .state
            .domains
            .sequencer
            .patterns()
            .get(pattern_id)
            .expect("created pattern")
            .content
        {
            objects.extend(
                steps
                    .lanes
                    .keys()
                    .copied()
                    .map(CreatedObject::SequencerLane),
            );
        }
        self.record(term, role, objects);
        Ok(())
    }

    fn add_curve(&mut self, term: &EditableTerm) -> Result<(), PromotionCompileError> {
        let EditableTermKind::Curve {
            expression,
            source_span,
            ..
        } = &term.kind
        else {
            unreachable!()
        };
        let target = self.request.bindings.curve_targets[&term.id].clone();
        let descriptor = self
            .state
            .domains
            .automation
            .descriptors()
            .find(|descriptor| descriptor.address == target)
            .cloned()
            .expect("validated target");
        let lane_id = self
            .state
            .domains
            .automation
            .create_lane(
                anonymous_term_name(term.id, "curve"),
                target.clone(),
                TimeDomain::Frames,
            )
            .map_err(|error| invalid(error.to_string()))?;
        let span_len = source_span.1 - source_span.0;
        let mut offset = 0;
        while offset < span_len {
            let source_frame = source_span.0 + offset;
            let value = crate::deprojection_program::evaluate_curve_at_source_frame(
                expression,
                *source_span,
                source_frame,
                self.request.program.source.sample_rate_hz,
            )
            .map_err(|error| invalid(error.to_string()))?;
            self.state
                .domains
                .automation
                .insert_point(
                    lane_id,
                    TimePosition::Frames(automation::ProjectFrame(
                        self.request
                            .placement
                            .start_frame
                            .saturating_add(offset as i64),
                    )),
                    descriptor.constrain(value),
                    SegmentShape::Linear,
                )
                .map_err(|error| invalid(error.to_string()))?;
            offset = offset.saturating_add(self.request.placement.curve_resolution_frames.max(1));
        }
        let end_value = crate::deprojection_program::evaluate_curve_at_source_frame(
            expression,
            *source_span,
            source_span.1,
            self.request.program.source.sample_rate_hz,
        )
        .map_err(|error| invalid(error.to_string()))?;
        self.state
            .domains
            .automation
            .insert_point(
                lane_id,
                TimePosition::Frames(automation::ProjectFrame(
                    self.request
                        .placement
                        .start_frame
                        .saturating_add(span_len as i64),
                )),
                descriptor.constrain(end_value),
                SegmentShape::Linear,
            )
            .map_err(|error| invalid(error.to_string()))?;
        let lane = self
            .state
            .domains
            .automation
            .lane(lane_id)
            .cloned()
            .expect("created lane");
        let command = AutomationCommand {
            label: "Promote construction curve".into(),
            changes: vec![LaneChange {
                before: None,
                after: Some(lane),
            }],
        };
        // Rebuild graph to pre-command state; the command itself owns publication.
        self.state.domains.automation = self.snapshot.project.state().domains.automation.clone();
        for prior in self.commands.iter().filter_map(|command| match command {
            DomainCommand::Automation(command) => Some(command),
            _ => None,
        }) {
            self.state
                .domains
                .automation
                .apply(prior)
                .map_err(|error| invalid(error.to_string()))?;
        }
        self.state
            .domains
            .automation
            .apply(&command)
            .map_err(|error| invalid(error.to_string()))?;
        self.commands.push(DomainCommand::Automation(command));
        let before_aliases = self.state.bindings.automation.lanes.clone();
        let alias = self
            .state
            .bindings
            .bind_automation_lane(lane_id)
            .map_err(|error| invalid(error.to_string()))?;
        if !before_aliases.contains_key(&alias) {
            self.commands.push(DomainCommand::Bindings(
                BindingCommand::PutAutomationLaneAlias {
                    alias,
                    before: None,
                    after: Some(lane_id),
                },
            ));
        }
        let track = self.ensure_automation_track()?;
        let placement =
            FrameRange::from_start_and_len(Frame(self.request.placement.start_frame), span_len)
                .map_err(|error| invalid(error.to_string()))?;
        let mut editor = ArrangementEditor::from_state(self.state.domains.arrangement.clone())
            .map_err(|error| invalid(error.to_string()))?;
        let clip_id = editor
            .create_automation_clip(
                track,
                anonymous_term_name(term.id, "curve"),
                placement,
                alias,
            )
            .map_err(|error| invalid(error.to_string()))?;
        let clip = editor.state().clip(clip_id).cloned().expect("created clip");
        let put = ArrangementOperation::PutClip {
            before: None,
            after: Some(clip),
        };
        self.state
            .domains
            .arrangement
            .apply_operations(std::slice::from_ref(&put))
            .map_err(|error| invalid(error.to_string()))?;
        self.commands.push(DomainCommand::Arrangement(put));
        self.record(
            term,
            PromotedTermRole::Curve,
            vec![
                CreatedObject::AutomationLane(lane_id),
                CreatedObject::ArrangementAutomationClip(clip_id),
                CreatedObject::ArrangementTrack(track),
            ],
        );
        Ok(())
    }
}

fn invalid(detail: impl Into<String>) -> PromotionCompileError {
    PromotionCompileError::Invalid(detail.into())
}

fn retained_expression(term: &EditableTerm) -> Option<RetainedExpression> {
    match &term.kind {
        EditableTermKind::Pattern {
            source, execution, ..
        } => {
            crate::pattern_lang::parse(source)
                .ok()
                .map(|expression| RetainedExpression::Pattern {
                    canonical_source: source.clone(),
                    term_hash: crate::pattern_lang::term_hash(&expression),
                    cycle: execution.cycle,
                    seed: execution.seed,
                    initial_cycle_index: execution.initial_cycle_index,
                })
        }
        EditableTermKind::Curve {
            target,
            expression,
            source_span,
        } => Some(RetainedExpression::Curve {
            canonical_source: crate::curve_lang::print(expression),
            target: *target,
            source_span: *source_span,
        }),
        EditableTermKind::ExactAudioReference { source, span } => {
            Some(RetainedExpression::ExactAudioFallback {
                source: *source,
                span: *span,
            })
        }
        _ => None,
    }
}

fn candidate_tag(candidate: DeprojectionCandidateId) -> String {
    hex4(&candidate.0.bytes)
}
fn anonymous_term_name(term: EditableTermId, kind: &str) -> String {
    format!("Anonymous {kind} {}", hex4(&term.0.bytes))
}
fn hex4(bytes: &[u8; 32]) -> String {
    bytes[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn derivation_scope(candidate: DeprojectionCandidateId) -> DerivationScope {
    DerivationScope(
        u128::from_le_bytes(candidate.0.bytes[..16].try_into().expect("digest prefix")) | 1,
    )
}
fn nonzero_u64(bytes: &[u8; 32]) -> u64 {
    u64::from_le_bytes(bytes[..8].try_into().expect("digest prefix")) | 1
}
fn scoped_evidence(scope: DerivationScope, evidence: &EvidenceRef) -> ScopedEvidenceRef {
    let mut canonical = Vec::new();
    match evidence {
        EvidenceRef::Artifact(id) => {
            canonical.push(0);
            canonical.extend_from_slice(&id.0.bytes);
        }
        EvidenceRef::SourceClaim(id) => {
            canonical.push(1);
            canonical.extend_from_slice(&id.0.bytes);
        }
        EvidenceRef::PatternAlternative(id) => {
            canonical.push(2);
            canonical.extend_from_slice(&id.0.bytes);
        }
        EvidenceRef::Rhythm(reference) => {
            canonical.push(3);
            let (kind, index) = match reference {
                crate::rhythm_explanation::RhythmEvidenceRef::Pattern(index) => (0_u8, *index),
                crate::rhythm_explanation::RhythmEvidenceRef::Hit(index) => (1, *index),
                crate::rhythm_explanation::RhythmEvidenceRef::Family(index) => (2, *index),
                crate::rhythm_explanation::RhythmEvidenceRef::Tempo(index) => (3, *index),
                crate::rhythm_explanation::RhythmEvidenceRef::BeatPhase(index) => (4, *index),
            };
            canonical.push(kind);
            canonical.extend_from_slice(&(index as u64).to_le_bytes());
        }
        EvidenceRef::NativeLocator {
            analyzer,
            version,
            locator,
        } => {
            canonical.push(4);
            for part in [analyzer.as_bytes(), version.as_bytes(), locator.as_bytes()] {
                canonical.extend_from_slice(&(part.len() as u64).to_le_bytes());
                canonical.extend_from_slice(part);
            }
        }
    }
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in canonical {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    ScopedEvidenceRef {
        scope,
        local: hash | 1,
    }
}

fn verify_created(
    snapshot: &LiveProjectSnapshot,
    created: &[CreatedObject],
) -> Result<(), PromotionExecutionError> {
    let state = snapshot.project.state();
    for object in created {
        let present = match object {
            CreatedObject::ArrangementTrack(id) => state.domains.arrangement.track(*id).is_some(),
            CreatedObject::AudioClip(id) | CreatedObject::ExactAudioFallbackClip(id) | CreatedObject::ArrangementPatternClip(id) | CreatedObject::ArrangementAutomationClip(id) => state.domains.arrangement.clip(*id).is_some(),
            CreatedObject::SequencerPattern(id) => state.domains.sequencer.patterns().get(*id).is_some(),
            CreatedObject::SequencerPatternClip(id) => state.domains.sequencer.clips().any(|clip| clip.id == *id),
            CreatedObject::SequencerLane(id) => state.domains.sequencer.patterns().patterns().any(|pattern| matches!(&pattern.content, PatternContent::Steps(steps) if steps.lanes.contains_key(id))),
            CreatedObject::AutomationLane(id) => state.domains.automation.lane(*id).is_some(),
            CreatedObject::SampleKit(id) => state.domains.sample_kits.kits.contains_key(id),
            CreatedObject::SamplePad(id) => state.domains.sample_kits.kits.values().any(|kit| kit.pads.contains_key(id)),
            CreatedObject::SampleZone(target) => state.domains.sample_kits.kits.get(&target.kit).and_then(|kit| kit.zone_for_target(*target)).is_some(),
            CreatedObject::MixerBus(id) => state.domains.mixer.bus(*id).is_some(),
        };
        if !present {
            return Err(PromotionExecutionError::ReceiptMismatch(format!(
                "created object {object:?} is absent from committed revision"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;

    use super::*;
    use crate::artifact_catalog::{ContentDigest, DigestAlgorithm};
    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        ContentFingerprint, DecodedAudioMetadata,
    };
    use crate::audio::AudioFormat;
    use crate::automation::{
        AutomationPoint, AutomationPointId, ParameterDescriptor, ParameterUnit, SmoothingPolicy,
        ValueMapping,
    };
    use crate::daw_engine::{compile_daw_engine, DawEngineConfig};
    use crate::daw_render::{PcmAsset, RenderCancellation, RenderWindow};
    use crate::deprojection_program::{EditableTerm, EditableTermKind, MaterialSpan, SourceClaim};
    use crate::live_project::{LiveProject, SourceMaterialMetadata};
    use crate::rhythm::SampleSpan;

    fn digest(byte: u8) -> ContentDigest {
        ContentDigest::new(DigestAlgorithm::Sha256, [byte; 32])
    }

    fn curve_address() -> ParameterAddress {
        ParameterAddress::Custom {
            namespace: "test.deprojection".into(),
            entity: "anonymous".into(),
            parameter: "pitch-cents".into(),
        }
    }

    fn fixture() -> (ProjectSession, assets::AssetId, MaterialSpan, SourceClaimId) {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/audio/deprojection-promotion.wav").unwrap()),
            None,
        )
        .unwrap();
        let mut registry = assets::AssetRegistry::new();
        let asset = registry
            .register(AssetRegistration {
                name: "source".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: 48_000,
                    channels: 1,
                    frame_count: SampleFrames(8),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32le".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"deprojection promotion fixture"),
                provenance: AssetProvenance::new(
                    1,
                    AssetOrigin::ImportedFile {
                        importer: "test".into(),
                    },
                    location,
                ),
                tags: BTreeSet::new(),
                favorite: false,
            })
            .unwrap();
        let pcm = PcmAsset::new(
            AudioFormat::new(48_000, 1).unwrap(),
            Arc::from([0.0, 0.5, -0.25, 0.75, -0.5, 0.25, 0.0, 0.0]),
        )
        .unwrap();
        let live = LiveProject::from_source_material(
            SourceMaterialMetadata::new("Promotion", "Source"),
            registry,
            asset,
            pcm,
        )
        .unwrap();
        live.domains()
            .automation
            .lock()
            .unwrap()
            .register_parameter(ParameterDescriptor {
                address: curve_address(),
                name: "Anonymous pitch".into(),
                unit: ParameterUnit::Semitones,
                minimum: -2_400.0,
                maximum: 2_400.0,
                default: 0.0,
                mapping: ValueMapping::Linear,
                smoothing: SmoothingPolicy::LinearFrames(16),
            })
            .unwrap();
        let source = MaterialSpan {
            material_sha256: "11".repeat(32),
            start_frame: 0,
            frame_count: 8,
            sample_rate_hz: 48_000,
            channels: 1,
        };
        let claim = SourceClaim::literal(source.clone(), digest(9)).unwrap();
        let mut session =
            ProjectSession::new(crate::project_session::ProjectSessionId(91)).unwrap();
        session.install(live, None).unwrap();
        (session, asset, source, claim.id)
    }

    fn render(snapshot: &LiveProjectSnapshot) -> Vec<f32> {
        let cancellation = RenderCancellation::new();
        compile_daw_engine(
            &snapshot.project,
            &snapshot.pcm,
            RenderWindow::new(0, 8).unwrap(),
            &DawEngineConfig::default(),
            &cancellation,
        )
        .unwrap()
        .render_for_audition(&cancellation)
        .unwrap()
        .audio
        .interleaved()
        .to_vec()
    }

    #[test]
    fn exact_audio_promotion_renders_edits_expose_residual_change_and_undo_is_atomic() {
        let (mut session, asset, source, claim) = fixture();
        let term = EditableTerm {
            id: EditableTermId(digest(22)),
            kind: EditableTermKind::ExactAudioReference {
                source: claim,
                span: SampleSpan { start: 0, end: 8 },
            },
            evidence: vec![EvidenceRef::SourceClaim(claim)],
            derivation: Derivation {
                rule: "test.exact-fallback.v1".into(),
                recipe: digest(23),
                premises: vec![EvidenceRef::SourceClaim(claim)],
            },
            description_bytes: 32,
            free_parameters: 0,
        };
        let program = SourceProgram::new(source, vec![term.clone()], vec![term.id]).unwrap();
        let revision = session.project_snapshot().unwrap().revisions().aggregate;
        let result = promote(
            &mut session,
            PromotionRequest {
                candidate: DeprojectionCandidateId(digest(24)),
                expected_project_revision: revision,
                program,
                bindings: PromotionBindings {
                    source_assets: BTreeMap::from([(
                        claim,
                        ResolvedSourceAsset {
                            asset,
                            claim_frame_zero: 0,
                            frame_count: 8,
                        },
                    )]),
                    ..PromotionBindings::default()
                },
                placement: PromotionPlacement::default(),
            },
        )
        .unwrap();
        assert_eq!(
            result.provenance[&term.id].role,
            PromotedTermRole::ExactAudioFallback
        );
        assert_eq!(
            result.provenance[&term.id].expression,
            Some(RetainedExpression::ExactAudioFallback {
                source: claim,
                span: SampleSpan { start: 0, end: 8 },
            })
        );
        let promoted = render(session.project_snapshot().unwrap());
        let clip_id = result
            .created
            .iter()
            .find_map(|object| match object {
                CreatedObject::ExactAudioFallbackClip(id) => Some(*id),
                _ => None,
            })
            .unwrap();
        let before = session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .arrangement
            .clip(clip_id)
            .unwrap()
            .clone();
        let mut after = before.clone();
        after.muted = true;
        let commands = vec![DomainCommand::Arrangement(ArrangementOperation::PutClip {
            before: Some(before),
            after: Some(after),
        })];
        session
            .execute_envelope(CommandEnvelope {
                label: "Edit promoted construction".into(),
                base_revision: session.project_snapshot().unwrap().revisions().aggregate,
                coalesce: None,
                id_claims: claims_for_commands(&commands),
                commands,
            })
            .unwrap();
        let edited = render(session.project_snapshot().unwrap());
        let residual_change: f32 = promoted
            .iter()
            .zip(&edited)
            .map(|(before, after)| (before - after).abs())
            .sum();
        assert!(
            residual_change > 0.1,
            "editing must expose an audible residual change"
        );
        assert_eq!(
            result.retained_expression_status(session.project_snapshot().unwrap())[0].state,
            RetainedExpressionState::Diverged
        );
        session.undo_with_receipt().unwrap().unwrap();
        result.undo(&mut session).unwrap();
        assert!(session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .arrangement
            .clip(clip_id)
            .is_none());
    }

    #[test]
    fn canonical_pattern_uses_retained_cycle_and_reports_manual_divergence() {
        let (mut session, asset, source, claim) = fixture();
        let parsed = crate::pattern_lang::parse("fam0").unwrap();
        let canonical = crate::pattern_lang::print(&parsed);
        let cycle = BeatDuration((PPQ * 3) as u64);
        let evidence = EvidenceRef::NativeLocator {
            analyzer: "audec.rhythm".into(),
            version: "test-v1".into(),
            locator: "alternative/0/family/0".into(),
        };
        let term = EditableTerm {
            id: EditableTermId(digest(41)),
            kind: EditableTermKind::Pattern {
                source: canonical.clone(),
                execution: crate::deprojection_program::PatternExecutionSemantics {
                    cycle,
                    seed: 77,
                    initial_cycle_index: 0,
                },
                voices: BTreeMap::from([("fam0".into(), VoiceTerm::AudioClaim(claim))]),
            },
            evidence: vec![evidence.clone()],
            derivation: Derivation {
                rule: "rhythm.minimum-description-pattern.v1".into(),
                recipe: digest(42),
                premises: vec![evidence.clone()],
            },
            description_bytes: canonical.len() as u64,
            free_parameters: 1,
        };
        let program = SourceProgram::new(source, vec![term.clone()], vec![term.id]).unwrap();
        let revision = session.project_snapshot().unwrap().revisions().aggregate;
        let result = promote(
            &mut session,
            PromotionRequest {
                candidate: DeprojectionCandidateId(digest(43)),
                expected_project_revision: revision,
                program,
                bindings: PromotionBindings {
                    source_assets: BTreeMap::from([(
                        claim,
                        ResolvedSourceAsset {
                            asset,
                            claim_frame_zero: 0,
                            frame_count: 8,
                        },
                    )]),
                    ..PromotionBindings::default()
                },
                placement: PromotionPlacement {
                    // Deliberately differs: the term owns pattern execution.
                    cycle: BeatDuration(PPQ as u64),
                    ..PromotionPlacement::default()
                },
            },
        )
        .unwrap();
        assert_eq!(
            result.provenance[&term.id].expression,
            Some(RetainedExpression::Pattern {
                canonical_source: canonical.clone(),
                term_hash: crate::pattern_lang::term_hash(&parsed),
                cycle,
                seed: 77,
                initial_cycle_index: 0,
            })
        );
        assert_eq!(result.provenance[&term.id].evidence, vec![evidence]);
        let pattern_id = result.provenance[&term.id]
            .created
            .iter()
            .find_map(|created| match created {
                CreatedObject::SequencerPattern(id) => Some(*id),
                _ => None,
            })
            .unwrap();
        let before = session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .sequencer
            .patterns()
            .get(pattern_id)
            .unwrap()
            .clone();
        assert_eq!(before.length, cycle);
        assert!(matches!(
            &before.origin,
            PatternOrigin::Expression { source, diverged: false, .. } if source == &canonical
        ));
        assert_eq!(
            result.retained_expression_status(session.project_snapshot().unwrap())[0].state,
            RetainedExpressionState::InSync
        );

        let mut after = before.clone();
        let PatternContent::Steps(steps) = &mut after.content else {
            panic!("promoted pattern must be editable steps")
        };
        steps.swing = 0.25;
        // Authoring controllers persist the generated-origin divergence bit
        // with the edited realization, making the aggregate inverse exact.
        after.origin.mark_diverged();
        after.revision += 1;
        let commands = vec![DomainCommand::Sequencer(SequencerCommand::PutPattern {
            before: Some(before),
            after: Some(after),
        })];
        session
            .execute_envelope(CommandEnvelope {
                label: "Edit promoted rhythm expression".into(),
                base_revision: session.project_snapshot().unwrap().revisions().aggregate,
                coalesce: None,
                id_claims: claims_for_commands(&commands),
                commands,
            })
            .unwrap();
        assert_eq!(
            result.retained_expression_status(session.project_snapshot().unwrap())[0].state,
            RetainedExpressionState::Diverged
        );
        session.undo_with_receipt().unwrap().unwrap();
        result.undo(&mut session).unwrap();
        assert!(session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .sequencer
            .patterns()
            .get(pattern_id)
            .is_none());
    }

    #[test]
    fn canonical_curve_retains_term_and_detects_ordinary_lane_edits() {
        let (mut session, _, source, _) = fixture();
        let expression = crate::curve_lang::CurveExpr::Line {
            from: -100.0,
            to: 200.0,
        };
        let evidence = EvidenceRef::NativeLocator {
            analyzer: "audec.pitch".into(),
            version: "pitch-v4".into(),
            locator: "track/0/modulation/0".into(),
        };
        let term = EditableTerm {
            id: EditableTermId(digest(51)),
            kind: EditableTermKind::Curve {
                target: CurveTarget::PitchCents,
                expression: expression.clone(),
                source_span: (0, 8),
            },
            evidence: vec![evidence.clone()],
            derivation: Derivation {
                rule: "pitch.glide-to-line.v1".into(),
                recipe: digest(52),
                premises: vec![evidence.clone()],
            },
            description_bytes: crate::curve_lang::print(&expression).len() as u64,
            free_parameters: 2,
        };
        let program = SourceProgram::new(source, vec![term.clone()], vec![term.id]).unwrap();
        let revision = session.project_snapshot().unwrap().revisions().aggregate;
        let result = promote(
            &mut session,
            PromotionRequest {
                candidate: DeprojectionCandidateId(digest(53)),
                expected_project_revision: revision,
                program,
                bindings: PromotionBindings {
                    curve_targets: BTreeMap::from([(term.id, curve_address())]),
                    ..PromotionBindings::default()
                },
                placement: PromotionPlacement {
                    curve_resolution_frames: 4,
                    ..PromotionPlacement::default()
                },
            },
        )
        .unwrap();
        assert_eq!(
            result.provenance[&term.id].expression,
            Some(RetainedExpression::Curve {
                canonical_source: crate::curve_lang::print(&expression),
                target: CurveTarget::PitchCents,
                source_span: (0, 8),
            })
        );
        assert_eq!(result.provenance[&term.id].evidence, vec![evidence]);
        let lane_id = result.provenance[&term.id]
            .created
            .iter()
            .find_map(|created| match created {
                CreatedObject::AutomationLane(id) => Some(*id),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            result.retained_expression_status(session.project_snapshot().unwrap())[0].state,
            RetainedExpressionState::InSync
        );

        let before = session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .automation
            .lane(lane_id)
            .unwrap()
            .clone();
        let mut after = before.clone();
        after
            .insert_point(AutomationPoint {
                id: AutomationPointId::from_raw(999),
                position: TimePosition::Frames(automation::ProjectFrame(2)),
                value: 0.0,
                outgoing: SegmentShape::Linear,
            })
            .unwrap();
        let command = AutomationCommand::replace("Edit promoted curve", before, after).unwrap();
        let commands = vec![DomainCommand::Automation(command)];
        session
            .execute_envelope(CommandEnvelope {
                label: "Edit promoted curve".into(),
                base_revision: session.project_snapshot().unwrap().revisions().aggregate,
                coalesce: None,
                id_claims: claims_for_commands(&commands),
                commands,
            })
            .unwrap();
        assert_eq!(
            result.retained_expression_status(session.project_snapshot().unwrap())[0].state,
            RetainedExpressionState::Diverged
        );
        session.undo_with_receipt().unwrap().unwrap();
        result.undo(&mut session).unwrap();
        assert!(session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .automation
            .lane(lane_id)
            .is_none());
    }

    #[test]
    fn noncanonical_or_shifted_pattern_semantics_are_honest_refusals() {
        let (session, _, source, _) = fixture();
        let term = EditableTerm {
            id: EditableTermId(digest(61)),
            kind: EditableTermKind::Pattern {
                source: "  fam0  ".into(),
                execution: crate::deprojection_program::PatternExecutionSemantics {
                    cycle: BeatDuration((PPQ * 4) as u64),
                    seed: 9,
                    initial_cycle_index: 2,
                },
                voices: BTreeMap::from([(
                    "fam0".into(),
                    VoiceTerm::UnresolvedFamily { family: 0 },
                )]),
            },
            evidence: Vec::new(),
            derivation: Derivation {
                rule: "test".into(),
                recipe: digest(62),
                premises: Vec::new(),
            },
            description_bytes: 8,
            free_parameters: 1,
        };
        let program = SourceProgram::new(source, vec![term.clone()], vec![term.id]).unwrap();
        let error = compile_promotion(
            session.project_snapshot().unwrap(),
            PromotionRequest {
                candidate: DeprojectionCandidateId(digest(63)),
                expected_project_revision: session
                    .project_snapshot()
                    .unwrap()
                    .revisions()
                    .aggregate,
                program,
                bindings: PromotionBindings::default(),
                placement: PromotionPlacement::default(),
            },
        )
        .unwrap_err();
        let PromotionCompileError::Refused(reasons) = error else {
            panic!("expected typed refusals")
        };
        assert!(reasons.contains(&PromotionRefusal::NonCanonicalPatternTerm { term: term.id }));
        assert!(
            reasons.contains(&PromotionRefusal::UnsupportedInitialCycleIndex {
                term: term.id,
                initial_cycle_index: 2,
            })
        );
        assert!(reasons.contains(&PromotionRefusal::UnresolvedPatternVoice {
            term: term.id,
            binding: "fam0".into(),
            family: 0,
        }));
    }

    #[test]
    fn unresolved_pattern_and_evidence_curve_are_typed_refusals_without_commands() {
        let (session, _, source, _) = fixture();
        let pattern = EditableTerm {
            id: EditableTermId(digest(31)),
            kind: EditableTermKind::Pattern {
                source: "fam0".into(),
                execution: crate::deprojection_program::PatternExecutionSemantics {
                    cycle: BeatDuration((PPQ * 4) as u64),
                    seed: 0,
                    initial_cycle_index: 0,
                },
                voices: BTreeMap::from([(
                    "fam0".into(),
                    VoiceTerm::UnresolvedFamily { family: 0 },
                )]),
            },
            evidence: Vec::new(),
            derivation: Derivation {
                rule: "test".into(),
                recipe: digest(32),
                premises: Vec::new(),
            },
            description_bytes: 4,
            free_parameters: 1,
        };
        let program = SourceProgram::new(source, vec![pattern.clone()], vec![pattern.id]).unwrap();
        let error = compile_promotion(
            session.project_snapshot().unwrap(),
            PromotionRequest {
                candidate: DeprojectionCandidateId(digest(33)),
                expected_project_revision: session
                    .project_snapshot()
                    .unwrap()
                    .revisions()
                    .aggregate,
                program,
                bindings: PromotionBindings::default(),
                placement: PromotionPlacement::default(),
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            PromotionCompileError::Refused(ref reasons)
                if reasons == &vec![PromotionRefusal::UnresolvedPatternVoice {
                    term: pattern.id,
                    binding: "fam0".into(),
                    family: 0,
                }]
        ));
    }
}
