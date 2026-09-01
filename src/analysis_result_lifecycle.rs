//! UI-neutral lifecycle for temporary analysis-pane results.
//!
//! Rhythm, HPSS, Loom, and component/recurrence panes all present the same
//! durable verbs. Availability is derived from signal semantics and exact
//! controller bindings; a pane never invents a play/apply/sample effect merely
//! because it can draw a result. Durable completions become one typed receipt
//! with an exact product-level reveal. Audible requests can only be compiled
//! through [`AnalysisPaneBridge`], preserving the application's sole transport
//! and finite-preview authorities.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::artifact_catalog::{ArtifactDescriptor, ArtifactId, ArtifactKind};
use crate::comparison::ComparisonId;
use crate::explanation::{ExplanationId, HpssComponentKind};
use crate::project_audio_controller::AuditionAlignment;
use crate::project_controller::{
    recommend_constructive, ConstructivePublication, FindingKind, FindingLocalId, FindingRef,
    FindingScope, ObjectAction, ObjectActionRequest, ObjectAuditionSignal, ObjectRef, RevealIntent,
    RevealRequest, RhythmPromotionChoice, RhythmPromotionChoiceId,
};
use crate::project_session::deprojection_workspace_bridge::{
    DeprojectionCandidateDocumentSummary, DeprojectionWorkspaceTarget,
};
use crate::render_plan::{RenderFormat, RenderSpan};
use crate::render_runtime::AuditionOwner;
use crate::sample_actions::SampleSelection;
use crate::sample_material::DerivationScope;
use crate::{hpss::HpssResult, loom::SequenceSketch};

use super::{
    AnalysisPaneBridge, PaneAudioError, PaneAudioKind, PaneAudioRoute, PaneAuditionContext,
    PaneSourcePin, PreviewController, SamplePanePreviewEffect,
};

/// The semantic unit represented by one result card. Algorithm names are not
/// sufficient: a rhythm pattern and a phase-bearing family medoid have
/// different honest verbs even when they came from the same analysis job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnalysisResultKind {
    RhythmPattern,
    RhythmFamilyMedoid,
    HpssComponent(HpssComponentKind),
    LoomSequence,
    LoomTemplate,
    /// Recurrence/component magnitude factors without phase-bearing PCM.
    ComponentMagnitude,
}

impl AnalysisResultKind {
    pub const fn finding_kind(self) -> FindingKind {
        match self {
            Self::RhythmPattern | Self::RhythmFamilyMedoid => FindingKind::Rhythm,
            Self::HpssComponent(_) => FindingKind::Separation,
            Self::LoomSequence | Self::LoomTemplate => FindingKind::Loom,
            Self::ComponentMagnitude => FindingKind::Components,
        }
    }

    const fn allows_apply(self) -> bool {
        matches!(
            self,
            Self::RhythmPattern | Self::HpssComponent(_) | Self::LoomSequence
        )
    }

    const fn allows_compare(self) -> bool {
        matches!(
            self,
            Self::RhythmPattern | Self::HpssComponent(_) | Self::LoomSequence
        )
    }

    const fn allows_sample(self) -> bool {
        matches!(
            self,
            Self::RhythmFamilyMedoid | Self::HpssComponent(_) | Self::LoomTemplate
        )
    }

    fn audition_kinds(self, has_comparison: bool) -> Vec<PaneAudioKind> {
        match self {
            Self::RhythmPattern if has_comparison => vec![PaneAudioKind::RhythmConstruction],
            Self::RhythmPattern => Vec::new(),
            Self::RhythmFamilyMedoid => vec![PaneAudioKind::RhythmFamilyMedoid],
            Self::HpssComponent(HpssComponentKind::Harmonic) => vec![
                PaneAudioKind::HpssSource,
                PaneAudioKind::HpssHarmonic,
                PaneAudioKind::HpssResidual,
            ],
            Self::HpssComponent(HpssComponentKind::Percussive) => vec![
                PaneAudioKind::HpssSource,
                PaneAudioKind::HpssTransient,
                PaneAudioKind::HpssResidual,
            ],
            Self::LoomSequence => vec![
                PaneAudioKind::LoomSource,
                PaneAudioKind::LoomConstruction,
                PaneAudioKind::LoomResidual,
            ],
            Self::LoomTemplate => vec![PaneAudioKind::LoomTemplate],
            Self::ComponentMagnitude => vec![PaneAudioKind::ComponentMagnitudeHypothesis],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AnalysisDurableAction {
    KeepFinding,
    ApplyConstruction,
    Compare,
    MakeSample,
}

impl AnalysisDurableAction {
    pub const ALL: [Self; 4] = [
        Self::KeepFinding,
        Self::ApplyConstruction,
        Self::Compare,
        Self::MakeSample,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::KeepFinding => "Keep finding",
            Self::ApplyConstruction => "Apply as editable construction…",
            Self::Compare => "Compare",
            Self::MakeSample => "Make sample…",
        }
    }
}

/// Refusal is presentation data, not merely an execution error. It lets panes
/// replace dishonest or inert controls with a precise inspect-only reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnalysisActionRefusal {
    EvidenceOnly,
    NoPromotionPlan,
    NoComparisonPlan,
    NoSampleMaterialization,
    PatternIsNotASample,
    SequenceIsNotASample,
    TemplateIsNotAConstruction,
    MedoidIsNotAnInstrumentIdentity,
    NoPhaseBearingPcm,
}

impl AnalysisActionRefusal {
    pub const fn message(self) -> &'static str {
        match self {
            Self::EvidenceOnly => {
                "This result is measurement evidence; it has no executable construction."
            }
            Self::NoPromotionPlan => {
                "No exact promotion plan is bound to this result; keep it as evidence or inspect it."
            }
            Self::NoComparisonPlan => {
                "No source/construction comparison is pinned for this result."
            }
            Self::NoSampleMaterialization => {
                "No exact source range or phase-bearing artifact signal is bound for sample creation."
            }
            Self::PatternIsNotASample => {
                "A rhythm pattern is event/grid evidence, not one sound; choose a family medoid or source range."
            }
            Self::SequenceIsNotASample => {
                "A Loom sequence contains reusable events; choose one template to make a sample."
            }
            Self::TemplateIsNotAConstruction => {
                "One template is auditionable material, but it is not an editable sequence construction."
            }
            Self::MedoidIsNotAnInstrumentIdentity => {
                "A family medoid is one representative sound, not evidence of an instrument identity."
            }
            Self::NoPhaseBearingPcm => {
                "Magnitude factors do not retain phase-bearing PCM, so they cannot be heard, compared as an exact residual, or sampled."
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnalysisActionAvailability {
    Available,
    Refused(AnalysisActionRefusal),
}

impl AnalysisActionAvailability {
    pub const fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnalysisComparisonRef {
    pub comparison: ComparisonId,
    pub explanation: ExplanationId,
}

/// Exact authority bindings supplied by the workspace/session adapter. A
/// missing binding disables the corresponding verb instead of deferring an
/// ambiguous failure until after a click.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnalysisResultBindings {
    pub promotion: Option<AnalysisPromotionTarget>,
    pub comparison: Option<AnalysisComparisonRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnalysisPromotionTarget {
    Deprojection(DeprojectionWorkspaceTarget),
    RhythmChoice {
        choice: RhythmPromotionChoiceId,
        scoped_evidence: FindingRef,
    },
}

impl AnalysisResultBindings {
    pub fn from_workspace_candidate(
        summary: &DeprojectionCandidateDocumentSummary,
    ) -> Result<Self, AnalysisLifecycleError> {
        if !matches!(
            summary.freshness,
            crate::project_session::deprojection_workspace_bridge::DeprojectionCandidateFreshness::Current
        ) || summary.comparison.0 == 0
            || summary.explanation.0 == 0
        {
            return Err(AnalysisLifecycleError::WorkspaceCandidateInvalidated);
        }
        Ok(Self {
            promotion: Some(AnalysisPromotionTarget::Deprojection(
                DeprojectionWorkspaceTarget::Object(ObjectRef::Finding(summary.finding)),
            )),
            comparison: Some(AnalysisComparisonRef {
                comparison: summary.comparison,
                explanation: summary.explanation,
            }),
        })
    }
}

/// Material which the sample workflow can honestly receive. Artifact signals
/// are derived phase-bearing PCM; the intent deliberately references the
/// catalog product instead of pretending it was an original source slice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnalysisSampleSource {
    ExactSource(SampleSelection),
    ArtifactSignal {
        artifact: ArtifactId,
        signal: PaneAudioKind,
        span: RenderSpan,
    },
    DerivedPcm {
        artifact: ArtifactId,
        local_key: u64,
        content: crate::render_plan::ExactDigest,
        frames: u64,
        sample_rate: u32,
        channels: u16,
    },
}

#[derive(Clone, Debug)]
pub struct TemporaryAnalysisResult {
    pub descriptor: ArtifactDescriptor,
    pub finding: FindingRef,
    pub label: String,
    pub kind: AnalysisResultKind,
    pub source: PaneSourcePin,
    pub bindings: AnalysisResultBindings,
    pub sample_source: Option<AnalysisSampleSource>,
}

impl TemporaryAnalysisResult {
    pub fn new(
        descriptor: ArtifactDescriptor,
        finding: FindingRef,
        label: impl Into<String>,
        kind: AnalysisResultKind,
        source: PaneSourcePin,
        bindings: AnalysisResultBindings,
        sample_source: Option<AnalysisSampleSource>,
    ) -> Result<Self, AnalysisLifecycleError> {
        descriptor
            .validate()
            .map_err(|error| AnalysisLifecycleError::InvalidResult(error.to_string()))?;
        let label = label.into();
        if label.trim().is_empty() {
            return Err(AnalysisLifecycleError::InvalidResult(
                "analysis result needs a visible label".into(),
            ));
        }
        if finding.kind != kind.finding_kind()
            || finding.scope != FindingScope::Artifact(descriptor.id)
        {
            return Err(AnalysisLifecycleError::FindingScopeMismatch);
        }
        if source.span.start != descriptor.extent.start
            || source.span.end != descriptor.extent.end
            || source.source_format.sample_rate.get() != descriptor.sample_rate
        {
            return Err(AnalysisLifecycleError::SourcePinMismatch);
        }
        if let Some(AnalysisSampleSource::ArtifactSignal { artifact, span, .. }) = &sample_source {
            if *artifact != descriptor.id || *span != source.span {
                return Err(AnalysisLifecycleError::SampleSourceMismatch);
            }
        }
        if let Some(AnalysisSampleSource::DerivedPcm {
            artifact,
            content,
            frames,
            sample_rate,
            channels,
            ..
        }) = &sample_source
        {
            if *artifact != descriptor.id
                || content.is_zero()
                || *frames == 0
                || *sample_rate != descriptor.sample_rate
                || *channels == 0
            {
                return Err(AnalysisLifecycleError::SampleSourceMismatch);
            }
        }
        match kind {
            AnalysisResultKind::HpssComponent(_) if descriptor.kind != ArtifactKind::Hpss => {
                return Err(AnalysisLifecycleError::ArtifactKindMismatch)
            }
            AnalysisResultKind::LoomSequence | AnalysisResultKind::LoomTemplate
                if descriptor.kind != ArtifactKind::LoomSketch =>
            {
                return Err(AnalysisLifecycleError::ArtifactKindMismatch)
            }
            _ => {}
        }
        Ok(Self {
            descriptor,
            finding,
            label,
            kind,
            source,
            bindings,
            sample_source,
        })
    }

    /// Bind a selected rhythm grid alternative to the direct rhythm chooser,
    /// while the artifact candidate supplies the persistent comparison target.
    /// The derivation-scoped proposal remains a separate breadcrumb from the
    /// artifact-scoped Finding card.
    pub fn rhythm_pattern(
        descriptor: ArtifactDescriptor,
        summary: &DeprojectionCandidateDocumentSummary,
        source: PaneSourcePin,
        choice: &RhythmPromotionChoice,
    ) -> Result<Self, AnalysisLifecycleError> {
        validate_workspace_summary(&descriptor, summary, FindingKind::Rhythm)?;
        if choice.provenance.proposal != choice.id.0 {
            return Err(AnalysisLifecycleError::PromotionChoiceMismatch);
        }
        let scoped_evidence = FindingRef {
            kind: FindingKind::Rhythm,
            scope: FindingScope::Derivation(DerivationScope(choice.id.0.scope.0)),
            local: FindingLocalId::ReconstructionProposal(
                crate::reconstruction::ReconstructionProposalId::from_raw(choice.id.0.local),
            ),
        };
        let mut bindings = AnalysisResultBindings::from_workspace_candidate(summary)?;
        bindings.promotion = Some(AnalysisPromotionTarget::RhythmChoice {
            choice: choice.id,
            scoped_evidence,
        });
        Self::new(
            descriptor,
            summary.finding,
            summary.label.clone(),
            AnalysisResultKind::RhythmPattern,
            source,
            bindings,
            None,
        )
    }

    /// HPSS retains complex phase through resynthesis. The adapter proves the
    /// selected component, construction, and residual are full-span finite PCM
    /// before exposing Hear/Compare/Make sample.
    pub fn hpss_component(
        descriptor: ArtifactDescriptor,
        summary: &DeprojectionCandidateDocumentSummary,
        source: PaneSourcePin,
        result: &HpssResult,
        component: HpssComponentKind,
    ) -> Result<Self, AnalysisLifecycleError> {
        validate_workspace_summary(&descriptor, summary, FindingKind::Separation)?;
        let frames = usize::try_from(source.span.len())
            .map_err(|_| AnalysisLifecycleError::SignalShapeMismatch)?;
        if result.harmonic.len() != frames
            || result.percussive.len() != frames
            || result.residual.len() != frames
            || result
                .harmonic
                .iter()
                .chain(&result.percussive)
                .chain(&result.residual)
                .any(|sample| !sample.is_finite())
        {
            return Err(AnalysisLifecycleError::SignalShapeMismatch);
        }
        let signal = match component {
            HpssComponentKind::Harmonic => PaneAudioKind::HpssHarmonic,
            HpssComponentKind::Percussive => PaneAudioKind::HpssTransient,
        };
        Self::new(
            descriptor.clone(),
            summary.finding,
            summary.label.clone(),
            AnalysisResultKind::HpssComponent(component),
            source.clone(),
            AnalysisResultBindings::from_workspace_candidate(summary)?,
            Some(AnalysisSampleSource::ArtifactSignal {
                artifact: descriptor.id,
                signal,
                span: source.span,
            }),
        )
    }

    /// Bind an editable, phase-preserving Loom sketch. Cluster ids are checked
    /// against the sketch rather than accepted from a painted recurrence map.
    pub fn loom_sequence(
        descriptor: ArtifactDescriptor,
        summary: &DeprojectionCandidateDocumentSummary,
        source: PaneSourcePin,
        sketch: &SequenceSketch,
        clusters: &[usize],
    ) -> Result<Self, AnalysisLifecycleError> {
        validate_workspace_summary(&descriptor, summary, FindingKind::Loom)?;
        if sketch.sample_rate != descriptor.sample_rate
            || clusters.is_empty()
            || clusters
                .iter()
                .any(|cluster| sketch.cluster(*cluster).is_none())
        {
            return Err(AnalysisLifecycleError::SignalShapeMismatch);
        }
        Self::new(
            descriptor,
            summary.finding,
            summary.label.clone(),
            AnalysisResultKind::LoomSequence,
            source,
            AnalysisResultBindings::from_workspace_candidate(summary)?,
            None,
        )
    }

    /// Project one exact Loom cluster template as a previewable/sampleable
    /// material result. Its PCM digest and cluster id are retained; it does not
    /// inherit the sequence's Apply or full-span Compare affordances.
    pub fn loom_template(
        descriptor: ArtifactDescriptor,
        summary: &DeprojectionCandidateDocumentSummary,
        source: PaneSourcePin,
        sketch: &SequenceSketch,
        cluster: usize,
    ) -> Result<Self, AnalysisLifecycleError> {
        validate_workspace_summary(&descriptor, summary, FindingKind::Loom)?;
        let cluster = sketch
            .cluster(cluster)
            .ok_or(AnalysisLifecycleError::SignalShapeMismatch)?;
        if sketch.sample_rate != descriptor.sample_rate
            || cluster.template.samples.is_empty()
            || cluster
                .template
                .samples
                .iter()
                .any(|sample| !sample.is_finite())
        {
            return Err(AnalysisLifecycleError::SignalShapeMismatch);
        }
        let frames = cluster.template.samples.len() as u64;
        let sample_source = AnalysisSampleSource::DerivedPcm {
            artifact: descriptor.id,
            local_key: cluster.template.cluster_id as u64,
            content: crate::render_runtime::canonical_pcm_digest(&cluster.template.samples),
            frames,
            sample_rate: sketch.sample_rate,
            channels: 1,
        };
        Self::new(
            descriptor,
            summary.finding,
            format!(
                "{} · template {}",
                summary.label, cluster.template.cluster_id
            ),
            AnalysisResultKind::LoomTemplate,
            source,
            AnalysisResultBindings::default(),
            Some(sample_source),
        )
    }

    /// NMF/component recurrence is still a keepable, revealable Finding. The
    /// semantics deliberately produce inspect-only audition and refusal-grade
    /// Apply/Compare/Sample states because magnitude factors retain no phase.
    pub fn component_magnitude(
        descriptor: ArtifactDescriptor,
        finding: FindingRef,
        label: impl Into<String>,
        source: PaneSourcePin,
    ) -> Result<Self, AnalysisLifecycleError> {
        Self::new(
            descriptor,
            finding,
            label,
            AnalysisResultKind::ComponentMagnitude,
            source,
            AnalysisResultBindings::default(),
            None,
        )
    }

    pub fn action_availability(&self, action: AnalysisDurableAction) -> AnalysisActionAvailability {
        use AnalysisActionAvailability::{Available, Refused};
        use AnalysisActionRefusal::*;
        match action {
            AnalysisDurableAction::KeepFinding => Available,
            AnalysisDurableAction::ApplyConstruction => {
                let semantic_refusal = match self.kind {
                    AnalysisResultKind::ComponentMagnitude => Some(EvidenceOnly),
                    AnalysisResultKind::RhythmFamilyMedoid => Some(MedoidIsNotAnInstrumentIdentity),
                    AnalysisResultKind::LoomTemplate => Some(TemplateIsNotAConstruction),
                    _ if !self.kind.allows_apply() => Some(EvidenceOnly),
                    _ => None,
                };
                semantic_refusal.map_or_else(
                    || {
                        if self.bindings.promotion.is_some() {
                            Available
                        } else {
                            Refused(NoPromotionPlan)
                        }
                    },
                    Refused,
                )
            }
            AnalysisDurableAction::Compare => {
                if matches!(self.kind, AnalysisResultKind::ComponentMagnitude) {
                    Refused(NoPhaseBearingPcm)
                } else if !self.kind.allows_compare() {
                    Refused(NoComparisonPlan)
                } else if self.bindings.comparison.is_some() {
                    Available
                } else {
                    Refused(NoComparisonPlan)
                }
            }
            AnalysisDurableAction::MakeSample => {
                let semantic_refusal = match self.kind {
                    AnalysisResultKind::RhythmPattern => Some(PatternIsNotASample),
                    AnalysisResultKind::LoomSequence => Some(SequenceIsNotASample),
                    AnalysisResultKind::ComponentMagnitude => Some(NoPhaseBearingPcm),
                    _ if !self.kind.allows_sample() => Some(NoSampleMaterialization),
                    _ => None,
                };
                semantic_refusal.map_or_else(
                    || {
                        if self.sample_source.is_some() {
                            Available
                        } else {
                            Refused(NoSampleMaterialization)
                        }
                    },
                    Refused,
                )
            }
        }
    }

    pub fn audition_choices(&self) -> Vec<AnalysisAuditionChoice> {
        self.kind
            .audition_kinds(self.bindings.comparison.is_some())
            .into_iter()
            .map(|kind| {
                let availability = if kind.route() == PaneAudioRoute::EvidenceOnly {
                    AnalysisAuditionAvailability::Refused(AnalysisActionRefusal::NoPhaseBearingPcm)
                } else {
                    AnalysisAuditionAvailability::Available(kind.route())
                };
                AnalysisAuditionChoice {
                    kind,
                    label: audition_label(kind),
                    availability,
                }
            })
            .collect()
    }
}

fn validate_workspace_summary(
    descriptor: &ArtifactDescriptor,
    summary: &DeprojectionCandidateDocumentSummary,
    expected: FindingKind,
) -> Result<(), AnalysisLifecycleError> {
    if summary.artifact != descriptor.id
        || summary.finding.scope != FindingScope::Artifact(descriptor.id)
        || summary.finding.kind != expected
    {
        return Err(AnalysisLifecycleError::FindingScopeMismatch);
    }
    AnalysisResultBindings::from_workspace_candidate(summary)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnalysisAuditionAvailability {
    Available(PaneAudioRoute),
    Refused(AnalysisActionRefusal),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnalysisAuditionChoice {
    pub kind: PaneAudioKind,
    pub label: &'static str,
    pub availability: AnalysisAuditionAvailability,
}

fn audition_label(kind: PaneAudioKind) -> &'static str {
    match kind {
        PaneAudioKind::HpssSource | PaneAudioKind::LoomSource => "Hear source",
        PaneAudioKind::HpssHarmonic => "Hear harmonic component",
        PaneAudioKind::HpssTransient => "Hear transient component",
        PaneAudioKind::HpssResidual | PaneAudioKind::LoomResidual => "Hear residual",
        PaneAudioKind::LoomConstruction | PaneAudioKind::RhythmConstruction => "Hear construction",
        PaneAudioKind::RhythmFamilyMedoid => "Preview family medoid",
        PaneAudioKind::LoomTemplate => "Preview template",
        PaneAudioKind::ComponentMagnitudeHypothesis => "Inspect magnitude evidence",
        PaneAudioKind::ComparisonSource => "Hear source",
        PaneAudioKind::ComparisonConstruction => "Hear construction",
        PaneAudioKind::ComparisonResidual => "Hear residual",
        PaneAudioKind::AssetOneShot => "Audition sample",
        PaneAudioKind::PadGate => "Play pad",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnalysisActionTicket {
    pub finding: FindingRef,
    pub generation: u64,
    pub action: AnalysisDurableAction,
}

#[derive(Clone, Debug)]
pub enum AnalysisDurableIntent {
    KeepFinding {
        ticket: AnalysisActionTicket,
        descriptor: ArtifactDescriptor,
        finding: FindingRef,
    },
    ApplyConstruction {
        ticket: AnalysisActionTicket,
        target: AnalysisPromotionTarget,
        evidence: FindingRef,
    },
    Compare {
        ticket: AnalysisActionTicket,
        target: AnalysisComparisonRef,
        evidence: FindingRef,
    },
    MakeSample {
        ticket: AnalysisActionTicket,
        source: AnalysisSampleSource,
        evidence: FindingRef,
    },
}

impl AnalysisDurableIntent {
    pub const fn ticket(&self) -> AnalysisActionTicket {
        match self {
            Self::KeepFinding { ticket, .. }
            | Self::ApplyConstruction { ticket, .. }
            | Self::Compare { ticket, .. }
            | Self::MakeSample { ticket, .. } => *ticket,
        }
    }
}

#[derive(Clone, Debug)]
pub enum AnalysisDurableCompletion {
    Kept {
        ticket: AnalysisActionTicket,
        artifact: ArtifactId,
        finding: FindingRef,
        retention_revision: u64,
    },
    Applied {
        ticket: AnalysisActionTicket,
        publication: ConstructivePublication,
    },
    Compared {
        ticket: AnalysisActionTicket,
        target: AnalysisComparisonRef,
        interpretation_revision: u64,
    },
    Sampled {
        ticket: AnalysisActionTicket,
        publication: ConstructivePublication,
    },
}

impl AnalysisDurableCompletion {
    pub const fn ticket(&self) -> AnalysisActionTicket {
        match self {
            Self::Kept { ticket, .. }
            | Self::Applied { ticket, .. }
            | Self::Compared { ticket, .. }
            | Self::Sampled { ticket, .. } => *ticket,
        }
    }
}

/// Exact handoff after any durable result. Reverse objects use their native
/// identities; constructive publications retain their controller-selected
/// pad/pattern/occurrence focus. `reveal` is never a status-string substitute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalysisDurableReceipt {
    pub ticket: AnalysisActionTicket,
    pub artifact: ArtifactId,
    pub durable_revision: u64,
    pub primary: ObjectRef,
    pub related: Vec<ObjectRef>,
    pub reveal: RevealRequest,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnalysisPresentedActionState {
    Available,
    Pending(AnalysisActionTicket),
    Completed {
        primary: ObjectRef,
        durable_revision: u64,
    },
    Refused(AnalysisActionRefusal),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalysisPresentedAction {
    pub action: AnalysisDurableAction,
    pub label: &'static str,
    pub state: AnalysisPresentedActionState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalysisResultPresentation {
    pub label: String,
    pub finding: FindingRef,
    pub temporary: bool,
    pub actions: Vec<AnalysisPresentedAction>,
    pub auditions: Vec<AnalysisAuditionChoice>,
}

/// One controller per result card. It correlates async completions, prevents
/// duplicate durable commands, and is the presenter source of truth.
pub struct AnalysisResultController {
    result: TemporaryAnalysisResult,
    next_generation: u64,
    pending: Option<AnalysisActionTicket>,
    receipts: BTreeMap<AnalysisDurableAction, AnalysisDurableReceipt>,
}

impl AnalysisResultController {
    pub fn new(result: TemporaryAnalysisResult) -> Self {
        Self {
            result,
            next_generation: 1,
            pending: None,
            receipts: BTreeMap::new(),
        }
    }

    pub const fn result(&self) -> &TemporaryAnalysisResult {
        &self.result
    }

    pub fn presentation(&self) -> AnalysisResultPresentation {
        let actions = AnalysisDurableAction::ALL
            .into_iter()
            .map(|action| {
                let state = if let Some(receipt) = self.receipts.get(&action) {
                    AnalysisPresentedActionState::Completed {
                        primary: receipt.primary.clone(),
                        durable_revision: receipt.durable_revision,
                    }
                } else if self.pending.is_some_and(|ticket| ticket.action == action) {
                    AnalysisPresentedActionState::Pending(self.pending.expect("checked pending"))
                } else {
                    match self.result.action_availability(action) {
                        AnalysisActionAvailability::Available => {
                            AnalysisPresentedActionState::Available
                        }
                        AnalysisActionAvailability::Refused(reason) => {
                            AnalysisPresentedActionState::Refused(reason)
                        }
                    }
                };
                AnalysisPresentedAction {
                    action,
                    label: action.label(),
                    state,
                }
            })
            .collect();
        AnalysisResultPresentation {
            label: self.result.label.clone(),
            finding: self.result.finding,
            temporary: !self
                .receipts
                .contains_key(&AnalysisDurableAction::KeepFinding),
            actions,
            auditions: self.result.audition_choices(),
        }
    }

    pub fn begin(
        &mut self,
        action: AnalysisDurableAction,
    ) -> Result<AnalysisDurableIntent, AnalysisLifecycleError> {
        if let Some(ticket) = self.pending {
            return Err(AnalysisLifecycleError::ActionPending(ticket));
        }
        if self.receipts.contains_key(&action) {
            return Err(AnalysisLifecycleError::ActionAlreadyCompleted(action));
        }
        if let AnalysisActionAvailability::Refused(reason) = self.result.action_availability(action)
        {
            return Err(AnalysisLifecycleError::ActionRefused { action, reason });
        }
        let generation = self.next_generation;
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or(AnalysisLifecycleError::GenerationExhausted)?;
        let ticket = AnalysisActionTicket {
            finding: self.result.finding,
            generation,
            action,
        };
        let intent = match action {
            AnalysisDurableAction::KeepFinding => AnalysisDurableIntent::KeepFinding {
                ticket,
                descriptor: self.result.descriptor.clone(),
                finding: self.result.finding,
            },
            AnalysisDurableAction::ApplyConstruction => AnalysisDurableIntent::ApplyConstruction {
                ticket,
                target: self
                    .result
                    .bindings
                    .promotion
                    .clone()
                    .expect("available apply has a workspace binding"),
                evidence: self.result.finding,
            },
            AnalysisDurableAction::Compare => AnalysisDurableIntent::Compare {
                ticket,
                target: self
                    .result
                    .bindings
                    .comparison
                    .expect("available comparison has an exact target"),
                evidence: self.result.finding,
            },
            AnalysisDurableAction::MakeSample => AnalysisDurableIntent::MakeSample {
                ticket,
                source: self
                    .result
                    .sample_source
                    .clone()
                    .expect("available sample action has an exact materialization source"),
                evidence: self.result.finding,
            },
        };
        self.pending = Some(ticket);
        Ok(intent)
    }

    pub fn cancel(&mut self, ticket: AnalysisActionTicket) -> bool {
        if self.pending == Some(ticket) {
            self.pending = None;
            true
        } else {
            false
        }
    }

    pub fn complete(
        &mut self,
        completion: AnalysisDurableCompletion,
    ) -> Result<AnalysisDurableReceipt, AnalysisLifecycleError> {
        let ticket = completion.ticket();
        if self.pending != Some(ticket) {
            return Err(AnalysisLifecycleError::StaleCompletion {
                expected: self.pending,
                actual: ticket,
            });
        }
        if ticket.finding != self.result.finding {
            return Err(AnalysisLifecycleError::CompletionIdentityMismatch);
        }
        let completion_matches_action = matches!(
            (&completion, ticket.action),
            (
                AnalysisDurableCompletion::Kept { .. },
                AnalysisDurableAction::KeepFinding
            ) | (
                AnalysisDurableCompletion::Applied { .. },
                AnalysisDurableAction::ApplyConstruction
            ) | (
                AnalysisDurableCompletion::Compared { .. },
                AnalysisDurableAction::Compare
            ) | (
                AnalysisDurableCompletion::Sampled { .. },
                AnalysisDurableAction::MakeSample
            )
        );
        if !completion_matches_action {
            return Err(AnalysisLifecycleError::CompletionIdentityMismatch);
        }
        let receipt = match completion {
            AnalysisDurableCompletion::Kept {
                artifact,
                finding,
                retention_revision,
                ..
            } => {
                if artifact != self.result.descriptor.id
                    || finding != self.result.finding
                    || retention_revision == 0
                {
                    return Err(AnalysisLifecycleError::CompletionIdentityMismatch);
                }
                reverse_receipt(
                    ticket,
                    artifact,
                    retention_revision,
                    ObjectRef::Finding(finding),
                    Vec::new(),
                )
            }
            AnalysisDurableCompletion::Compared {
                target,
                interpretation_revision,
                ..
            } => {
                if self.result.bindings.comparison != Some(target)
                    || target.comparison.0 == 0
                    || target.explanation.0 == 0
                    || interpretation_revision == 0
                {
                    return Err(AnalysisLifecycleError::CompletionIdentityMismatch);
                }
                reverse_receipt(
                    ticket,
                    self.result.descriptor.id,
                    interpretation_revision,
                    ObjectRef::Comparison(target.comparison),
                    vec![
                        ObjectRef::Explanation(target.explanation),
                        ObjectRef::Finding(self.result.finding),
                    ],
                )
            }
            AnalysisDurableCompletion::Applied { publication, .. }
            | AnalysisDurableCompletion::Sampled { publication, .. } => {
                if publication.revision == 0 {
                    return Err(AnalysisLifecycleError::CompletionIdentityMismatch);
                }
                let promotion_evidence = (ticket.action
                    == AnalysisDurableAction::ApplyConstruction)
                    .then(|| self.result.bindings.promotion.as_ref())
                    .flatten()
                    .and_then(|target| match target {
                        AnalysisPromotionTarget::RhythmChoice {
                            scoped_evidence, ..
                        } => Some(*scoped_evidence),
                        AnalysisPromotionTarget::Deprojection(_) => None,
                    });
                constructive_receipt(
                    ticket,
                    self.result.descriptor.id,
                    publication,
                    self.result.finding,
                    promotion_evidence,
                )
            }
        };
        self.pending = None;
        self.receipts.insert(ticket.action, receipt.clone());
        Ok(receipt)
    }

    pub fn audition(
        &self,
        bridge: AnalysisPaneBridge,
        kind: PaneAudioKind,
    ) -> Result<AnalysisAuditionIntent, AnalysisLifecycleError> {
        let Some(choice) = self
            .result
            .audition_choices()
            .into_iter()
            .find(|choice| choice.kind == kind)
        else {
            return Err(AnalysisLifecycleError::AuditionUnavailable(kind));
        };
        if let AnalysisAuditionAvailability::Refused(reason) = choice.availability {
            return Err(AnalysisLifecycleError::AuditionRefused(reason));
        }
        Ok(AnalysisAuditionIntent {
            finding: self.result.finding,
            owner: bridge.owner(),
            kind,
            source: self.result.source.clone(),
        })
    }
}

fn reverse_receipt(
    ticket: AnalysisActionTicket,
    artifact: ArtifactId,
    durable_revision: u64,
    primary: ObjectRef,
    related: Vec<ObjectRef>,
) -> AnalysisDurableReceipt {
    let reveal = RevealRequest::new(primary.clone(), RevealIntent::ActivateExisting)
        .with_related(related.clone());
    AnalysisDurableReceipt {
        ticket,
        artifact,
        durable_revision,
        primary,
        related,
        reveal,
    }
}

fn constructive_receipt(
    ticket: AnalysisActionTicket,
    artifact: ArtifactId,
    publication: ConstructivePublication,
    finding: FindingRef,
    promotion_evidence: Option<FindingRef>,
) -> AnalysisDurableReceipt {
    let recommendation = recommend_constructive(&publication);
    let mut related = recommendation.request.related;
    related.push(ObjectRef::Finding(finding));
    related.extend(promotion_evidence.map(ObjectRef::Finding));
    let reveal = RevealRequest::new(
        recommendation.request.object.clone(),
        recommendation.request.intent,
    )
    .at_revision(publication.revision)
    .with_related(related.clone());
    AnalysisDurableReceipt {
        ticket,
        artifact,
        durable_revision: publication.revision,
        primary: reveal.object.clone(),
        related: reveal.related.clone(),
        reveal,
    }
}

/// An audible result request with no public raw-play escape hatch. Callers
/// provide PCM only when compiling it through the shared pane bridge.
#[derive(Clone, Debug)]
pub struct AnalysisAuditionIntent {
    finding: FindingRef,
    owner: AuditionOwner,
    kind: PaneAudioKind,
    source: PaneSourcePin,
}

impl AnalysisAuditionIntent {
    pub const fn finding(&self) -> FindingRef {
        self.finding
    }

    pub const fn owner(&self) -> AuditionOwner {
        self.owner
    }

    pub const fn kind(&self) -> PaneAudioKind {
        self.kind
    }

    /// Feed the same semantic audition through the universal object-action
    /// router before the reverse presenter resolves PCM. Short previews keep
    /// their more specific pane-audio kind and intentionally do not pretend to
    /// be a Source/Construction/Residual timeline layer.
    pub fn object_action_request(&self) -> Option<ObjectActionRequest> {
        let signal = match self.kind {
            PaneAudioKind::HpssSource
            | PaneAudioKind::LoomSource
            | PaneAudioKind::ComparisonSource => ObjectAuditionSignal::Source,
            PaneAudioKind::HpssResidual
            | PaneAudioKind::LoomResidual
            | PaneAudioKind::ComparisonResidual => ObjectAuditionSignal::Residual,
            PaneAudioKind::HpssHarmonic
            | PaneAudioKind::HpssTransient
            | PaneAudioKind::LoomConstruction
            | PaneAudioKind::ComparisonConstruction
            | PaneAudioKind::RhythmConstruction => ObjectAuditionSignal::Construction,
            PaneAudioKind::ComponentMagnitudeHypothesis
            | PaneAudioKind::RhythmFamilyMedoid
            | PaneAudioKind::LoomTemplate
            | PaneAudioKind::AssetOneShot
            | PaneAudioKind::PadGate => return None,
        };
        Some(ObjectActionRequest::new(
            ObjectRef::Finding(self.finding),
            ObjectAction::Audition(signal),
        ))
    }

    pub fn timeline_mono(
        self,
        bridge: AnalysisPaneBridge,
        output_format: RenderFormat,
        mono: Arc<[f32]>,
        alignment: AuditionAlignment,
    ) -> Result<super::PaneTimelineEffect, PaneAudioError> {
        require_owner(self.owner, bridge)?;
        bridge.timeline_mono(self.kind, self.source, output_format, mono, alignment)
    }

    pub fn short_preview_mono(
        self,
        bridge: AnalysisPaneBridge,
        previews: &mut PreviewController,
        current: &PaneAuditionContext,
        sample_rate: u32,
        mono: Arc<[f32]>,
    ) -> Result<SamplePanePreviewEffect, PaneAudioError> {
        require_owner(self.owner, bridge)?;
        bridge.short_preview_mono(
            previews,
            self.kind,
            &self.source,
            current,
            sample_rate,
            mono,
        )
    }
}

fn require_owner(
    expected: AuditionOwner,
    bridge: AnalysisPaneBridge,
) -> Result<(), PaneAudioError> {
    if expected == bridge.owner() {
        Ok(())
    } else {
        Err(PaneAudioError::MismatchedAnalysisAuditionOwner)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnalysisLifecycleError {
    InvalidResult(String),
    ArtifactKindMismatch,
    FindingScopeMismatch,
    SourcePinMismatch,
    SampleSourceMismatch,
    WorkspaceCandidateInvalidated,
    PromotionChoiceMismatch,
    SignalShapeMismatch,
    ActionRefused {
        action: AnalysisDurableAction,
        reason: AnalysisActionRefusal,
    },
    ActionPending(AnalysisActionTicket),
    ActionAlreadyCompleted(AnalysisDurableAction),
    GenerationExhausted,
    StaleCompletion {
        expected: Option<AnalysisActionTicket>,
        actual: AnalysisActionTicket,
    },
    CompletionIdentityMismatch,
    AuditionUnavailable(PaneAudioKind),
    AuditionRefused(AnalysisActionRefusal),
}

impl fmt::Display for AnalysisLifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidResult(detail) => formatter.write_str(detail),
            Self::ArtifactKindMismatch => {
                formatter.write_str("analysis result kind does not match its artifact kind")
            }
            Self::FindingScopeMismatch => formatter
                .write_str("analysis finding kind/scope does not match its artifact result"),
            Self::SourcePinMismatch => {
                formatter.write_str("analysis source pin does not match its artifact extent/rate")
            }
            Self::SampleSourceMismatch => {
                formatter.write_str("analysis sample source does not match its artifact result")
            }
            Self::WorkspaceCandidateInvalidated => formatter.write_str(
                "analysis workspace candidate is stale or has no exact comparison identity",
            ),
            Self::PromotionChoiceMismatch => {
                formatter.write_str("rhythm promotion choice does not match its scoped provenance")
            }
            Self::SignalShapeMismatch => {
                formatter.write_str("analysis signal shape does not match its pinned result")
            }
            Self::ActionRefused { action, reason } => {
                write!(
                    formatter,
                    "{} unavailable: {}",
                    action.label(),
                    reason.message()
                )
            }
            Self::ActionPending(ticket) => write!(
                formatter,
                "analysis action {:?} generation {} is still pending",
                ticket.action, ticket.generation
            ),
            Self::ActionAlreadyCompleted(action) => {
                write!(formatter, "analysis action {:?} already completed", action)
            }
            Self::GenerationExhausted => {
                formatter.write_str("analysis action generation counter exhausted")
            }
            Self::StaleCompletion { expected, actual } => write!(
                formatter,
                "stale analysis completion generation {}; expected {:?}",
                actual.generation,
                expected.map(|ticket| ticket.generation)
            ),
            Self::CompletionIdentityMismatch => {
                formatter.write_str("analysis completion does not match the requested exact result")
            }
            Self::AuditionUnavailable(kind) => {
                write!(formatter, "{kind:?} is not exposed by this analysis result")
            }
            Self::AuditionRefused(reason) => formatter.write_str(reason.message()),
        }
    }
}

impl Error for AnalysisLifecycleError {}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU16, NonZeroU32};

    use super::*;
    use crate::artifact_catalog::{sha256_content, ContentDigest, DigestAlgorithm};
    use crate::assets::AssetId;
    use crate::daw_project::ProjectRevisions;
    use crate::ontology::Provenance;
    use crate::project_controller::{FindingLocalId, InstrumentRef};
    use crate::render_plan::ExactDigest;
    use crate::rhythm::TempoRelation;
    use crate::sample_kit::KitId;
    use crate::sample_material::{DerivationScope, ScopedProposalRef};
    use crate::workspace_items::WorkspaceViewId;

    fn descriptor(kind: ArtifactKind, byte: u8) -> ArtifactDescriptor {
        let output = ContentDigest::new(DigestAlgorithm::Sha256, [byte; 32]);
        ArtifactDescriptor {
            id: ArtifactId(output),
            kind,
            source_digest: ContentDigest::new(DigestAlgorithm::Sha256, [1; 32]),
            recipe_digest: ContentDigest::new(DigestAlgorithm::Sha256, [2; 32]),
            output_digest: output,
            extent: crate::aspect::FrameSpan { start: 10, end: 14 },
            sample_rate: 48_000,
            channels: 1,
            provenance: Provenance::unknown(),
        }
    }

    fn source_pin(descriptor: &ArtifactDescriptor) -> PaneSourcePin {
        PaneSourcePin {
            document_generation: 1,
            publication_generation: 2,
            revisions: ProjectRevisions::default(),
            audible_cohort: None,
            span: RenderSpan::new(descriptor.extent.start, descriptor.extent.end).unwrap(),
            source_format: RenderFormat {
                sample_rate: NonZeroU32::new(descriptor.sample_rate).unwrap(),
                channels: NonZeroU16::new(descriptor.channels).unwrap(),
            },
            source_content: ExactDigest::new([7; 32]),
        }
    }

    fn finding(descriptor: &ArtifactDescriptor, kind: FindingKind) -> FindingRef {
        FindingRef {
            kind,
            scope: FindingScope::Artifact(descriptor.id),
            local: FindingLocalId::Claim(17),
        }
    }

    fn bindings() -> AnalysisResultBindings {
        AnalysisResultBindings {
            promotion: Some(AnalysisPromotionTarget::Deprojection(
                DeprojectionWorkspaceTarget::Object(ObjectRef::Comparison(ComparisonId(9))),
            )),
            comparison: Some(AnalysisComparisonRef {
                comparison: ComparisonId(9),
                explanation: ExplanationId(8),
            }),
        }
    }

    fn workspace_summary(
        descriptor: &ArtifactDescriptor,
        finding: FindingRef,
    ) -> DeprojectionCandidateDocumentSummary {
        DeprojectionCandidateDocumentSummary {
            id: crate::project_session::deprojection_workspace_bridge::DeprojectionCandidateDocumentId(1),
            artifact: descriptor.id,
            candidate: crate::deprojection_program::DeprojectionCandidateId(sha256_content(
                b"candidate",
                &[b"one"],
            )),
            finding,
            label: "Candidate A".into(),
            comparison: ComparisonId(2),
            explanation: ExplanationId(3),
            pin: crate::artifact_promotion_bridge::ArtifactPromotionWorkspacePin {
                document_generation: 1,
                publication_generation: 1,
                project_revisions: ProjectRevisions::default(),
                selection_revision: 1,
                catalog_generation: 1,
                catalog_digest: ContentDigest::new(DigestAlgorithm::Sha256, [9; 32]),
            },
            freshness:
                crate::project_session::deprojection_workspace_bridge::DeprojectionCandidateFreshness::Current,
        }
    }

    fn publication(revision: u64) -> ConstructivePublication {
        ConstructivePublication {
            revision,
            kit: KitId::from_raw(4),
            created_pads: Vec::new(),
            created_zones: Vec::new(),
            pad: None,
            pattern: None,
            sequencer_clip: None,
            arrangement_clip: None,
            arrangement_track: None,
            output_bus: None,
            focus: crate::project_controller::ConstructivePublishedFocus::Kit(KitId::from_raw(4)),
        }
    }

    #[test]
    fn component_result_is_useful_evidence_without_dishonest_sound_or_promotion() {
        let descriptor = descriptor(ArtifactKind::SpectralField, 3);
        let result = TemporaryAnalysisResult::component_magnitude(
            descriptor.clone(),
            finding(&descriptor, FindingKind::Components),
            "Recurring component 1",
            source_pin(&descriptor),
        )
        .unwrap();
        assert_eq!(
            result.action_availability(AnalysisDurableAction::KeepFinding),
            AnalysisActionAvailability::Available
        );
        assert!(matches!(
            result.action_availability(AnalysisDurableAction::ApplyConstruction),
            AnalysisActionAvailability::Refused(AnalysisActionRefusal::EvidenceOnly)
        ));
        assert!(matches!(
            result.action_availability(AnalysisDurableAction::MakeSample),
            AnalysisActionAvailability::Refused(AnalysisActionRefusal::NoPhaseBearingPcm)
        ));
        let choice = result.audition_choices()[0];
        assert_eq!(choice.label, "Inspect magnitude evidence");
        assert!(matches!(
            choice.availability,
            AnalysisAuditionAvailability::Refused(AnalysisActionRefusal::NoPhaseBearingPcm)
        ));
    }

    #[test]
    fn all_durable_verbs_finish_with_exact_receipts_and_reveals() {
        let descriptor = descriptor(ArtifactKind::Hpss, 4);
        let evidence = finding(&descriptor, FindingKind::Separation);
        let source = AnalysisSampleSource::ArtifactSignal {
            artifact: descriptor.id,
            signal: PaneAudioKind::HpssHarmonic,
            span: source_pin(&descriptor).span,
        };
        let result = TemporaryAnalysisResult::new(
            descriptor.clone(),
            evidence,
            "Harmonic component",
            AnalysisResultKind::HpssComponent(HpssComponentKind::Harmonic),
            source_pin(&descriptor),
            bindings(),
            Some(source),
        )
        .unwrap();
        let mut controller = AnalysisResultController::new(result);

        let keep = controller
            .begin(AnalysisDurableAction::KeepFinding)
            .unwrap();
        let keep_receipt = controller
            .complete(AnalysisDurableCompletion::Kept {
                ticket: keep.ticket(),
                artifact: descriptor.id,
                finding: evidence,
                retention_revision: 3,
            })
            .unwrap();
        assert_eq!(keep_receipt.primary, ObjectRef::Finding(evidence));
        assert_eq!(keep_receipt.reveal.object, keep_receipt.primary);

        let compare = controller.begin(AnalysisDurableAction::Compare).unwrap();
        let compare_receipt = controller
            .complete(AnalysisDurableCompletion::Compared {
                ticket: compare.ticket(),
                target: bindings().comparison.unwrap(),
                interpretation_revision: 4,
            })
            .unwrap();
        assert_eq!(
            compare_receipt.primary,
            ObjectRef::Comparison(ComparisonId(9))
        );
        assert!(compare_receipt
            .related
            .contains(&ObjectRef::Finding(evidence)));

        let apply = controller
            .begin(AnalysisDurableAction::ApplyConstruction)
            .unwrap();
        let apply_receipt = controller
            .complete(AnalysisDurableCompletion::Applied {
                ticket: apply.ticket(),
                publication: publication(5),
            })
            .unwrap();
        assert_eq!(
            apply_receipt.primary,
            ObjectRef::Instrument(InstrumentRef::SampleKit(KitId::from_raw(4)))
        );
        assert_eq!(apply_receipt.reveal.expected_project_revision, Some(5));
        assert!(apply_receipt
            .related
            .contains(&ObjectRef::Finding(evidence)));

        let sample = controller.begin(AnalysisDurableAction::MakeSample).unwrap();
        let sample_receipt = controller
            .complete(AnalysisDurableCompletion::Sampled {
                ticket: sample.ticket(),
                publication: publication(6),
            })
            .unwrap();
        assert_eq!(sample_receipt.reveal.object, sample_receipt.primary);
        assert_eq!(sample_receipt.durable_revision, 6);
        assert!(!controller.presentation().temporary);
    }

    #[test]
    fn audition_intents_can_only_compile_through_their_own_shared_owner() {
        let descriptor = descriptor(ArtifactKind::LoomSketch, 5);
        let result = TemporaryAnalysisResult::new(
            descriptor.clone(),
            finding(&descriptor, FindingKind::Loom),
            "Loom template 2",
            AnalysisResultKind::LoomTemplate,
            source_pin(&descriptor),
            AnalysisResultBindings::default(),
            Some(AnalysisSampleSource::ArtifactSignal {
                artifact: descriptor.id,
                signal: PaneAudioKind::LoomTemplate,
                span: source_pin(&descriptor).span,
            }),
        )
        .unwrap();
        let controller = AnalysisResultController::new(result);
        let owner = AnalysisPaneBridge::new(WorkspaceViewId(7)).unwrap();
        let other = AnalysisPaneBridge::new(WorkspaceViewId(8)).unwrap();
        let intent = controller
            .audition(owner, PaneAudioKind::LoomTemplate)
            .unwrap();
        let mut previews = PreviewController::default();
        let context = PaneAuditionContext {
            document_generation: 1,
            publication_generation: 2,
            revisions: ProjectRevisions::default(),
            audible_cohort: None,
        };
        assert!(matches!(
            intent.short_preview_mono(
                other,
                &mut previews,
                &context,
                48_000,
                Arc::from([0.1, 0.2, 0.3, 0.4])
            ),
            Err(PaneAudioError::MismatchedAnalysisAuditionOwner)
        ));
    }

    #[test]
    fn workspace_summary_binds_one_finding_to_apply_and_compare() {
        let descriptor = descriptor(ArtifactKind::ModelClaim, 8);
        let evidence = finding(&descriptor, FindingKind::Rhythm);
        let summary = workspace_summary(&descriptor, evidence);
        let bound = AnalysisResultBindings::from_workspace_candidate(&summary).unwrap();
        assert_eq!(
            bound.promotion,
            Some(AnalysisPromotionTarget::Deprojection(
                DeprojectionWorkspaceTarget::Object(ObjectRef::Finding(evidence))
            ))
        );
        assert_eq!(bound.comparison.unwrap().comparison, ComparisonId(2));
    }

    #[test]
    fn sample_source_selection_remains_exact_and_typed() {
        let selection = SampleSelection::whole_asset(AssetId(4));
        assert!(matches!(
            AnalysisSampleSource::ExactSource(selection),
            AnalysisSampleSource::ExactSource(actual) if actual == selection
        ));
    }

    #[test]
    fn hpss_adapter_proves_pcm_before_exposing_all_four_durable_verbs() {
        let descriptor = descriptor(ArtifactKind::Hpss, 10);
        let evidence = finding(&descriptor, FindingKind::Separation);
        let summary = workspace_summary(&descriptor, evidence);
        let hpss = crate::hpss::separate_harmonic_percussive(
            &[0.2, -0.1, 0.4, 0.0],
            crate::hpss::HpssSettings {
                fft_size: 4,
                hop_size: 2,
                soft_mask_power: 2.0,
                time_median_width: 3,
                frequency_median_width: 3,
            },
        )
        .unwrap();
        let result = TemporaryAnalysisResult::hpss_component(
            descriptor.clone(),
            &summary,
            source_pin(&descriptor),
            &hpss,
            HpssComponentKind::Harmonic,
        )
        .unwrap();
        for action in AnalysisDurableAction::ALL {
            assert_eq!(
                result.action_availability(action),
                AnalysisActionAvailability::Available,
                "{action:?}"
            );
        }
        assert_eq!(result.audition_choices().len(), 3);
    }

    #[test]
    fn loom_adapters_distinguish_sequence_actions_from_one_template_sample() {
        let descriptor = descriptor(ArtifactKind::LoomSketch, 11);
        let evidence = finding(&descriptor, FindingKind::Loom);
        let summary = workspace_summary(&descriptor, evidence);
        let sketch = SequenceSketch::infer(
            &[0.0, 0.1, 0.8, 0.2, 0.0, 0.1, 0.7, 0.2],
            48_000,
            &[crate::loom::EventObservation {
                sample_index: 2,
                cluster_id: 6,
                salience: 0.9,
                template_similarity: 0.8,
            }],
            crate::loom::TemplateBuildConfig {
                pre_roll_samples: 1,
                post_roll_samples: 3,
                alignment_radius_samples: 1,
                max_exemplars_per_cluster: 1,
            },
        )
        .unwrap();
        let sequence = TemporaryAnalysisResult::loom_sequence(
            descriptor.clone(),
            &summary,
            source_pin(&descriptor),
            &sketch,
            &[6],
        )
        .unwrap();
        assert!(sequence
            .action_availability(AnalysisDurableAction::ApplyConstruction)
            .is_available());
        assert!(matches!(
            sequence.action_availability(AnalysisDurableAction::MakeSample),
            AnalysisActionAvailability::Refused(AnalysisActionRefusal::SequenceIsNotASample)
        ));

        let template = TemporaryAnalysisResult::loom_template(
            descriptor,
            &summary,
            sequence.source.clone(),
            &sketch,
            6,
        )
        .unwrap();
        assert!(template
            .action_availability(AnalysisDurableAction::MakeSample)
            .is_available());
        assert!(matches!(
            template.sample_source,
            Some(AnalysisSampleSource::DerivedPcm { local_key: 6, .. })
        ));
        assert!(matches!(
            template.action_availability(AnalysisDurableAction::ApplyConstruction),
            AnalysisActionAvailability::Refused(AnalysisActionRefusal::TemplateIsNotAConstruction)
        ));
    }

    #[test]
    fn rhythm_adapter_keeps_artifact_finding_and_scoped_choice_distinct() {
        let descriptor = descriptor(ArtifactKind::ModelClaim, 12);
        let evidence = finding(&descriptor, FindingKind::Rhythm);
        let summary = workspace_summary(&descriptor, evidence);
        let proposal = ScopedProposalRef {
            scope: DerivationScope(44),
            local: 7,
        };
        let choice = RhythmPromotionChoice {
            id: RhythmPromotionChoiceId(proposal),
            evidence_rank: 0,
            grid: crate::project_controller::RhythmGridHypothesis {
                beat_phase_index: 0,
                tempo_rank: 0,
                bpm: 120.0,
                phase_source_frame: 0,
                support: 0.9,
                tempo_evidence: Some(0.8),
                tempo_relation: Some(TempoRelation::Independent),
                steps_per_quarter: 4,
            },
            diagnostics: Vec::new(),
            provenance: crate::project_controller::RhythmPromotionProvenance {
                proposal,
                evidence: Vec::new(),
                source: SampleSelection::whole_asset(AssetId(4)),
                pattern_index: 0,
                occurrence_index: 0,
            },
            explanation_links: Vec::new(),
        };
        let source = source_pin(&descriptor);
        let result =
            TemporaryAnalysisResult::rhythm_pattern(descriptor, &summary, source, &choice).unwrap();
        assert_eq!(result.finding, evidence);
        assert!(matches!(
            result.bindings.promotion,
            Some(AnalysisPromotionTarget::RhythmChoice {
                choice: actual,
                scoped_evidence: FindingRef {
                    scope: FindingScope::Derivation(DerivationScope(44)),
                    ..
                }
            }) if actual == choice.id
        ));
    }
}
