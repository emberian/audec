//! GPUI-neutral presentation state for explain-as-pattern comparisons.
//!
//! The pane model owns rows, action identities, and addressed-delivery
//! receipts. It never owns a project, transport, renderer, audio device, or
//! `ComparisonController`. Mutating operations are returned as typed requests
//! for the session/controller boundary. Coverage and fit remain measurements,
//! never correctness or source-identity claims.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::aspect::{ExplanationRef, SignalLayer};
use crate::comparison::{
    ComparisonDefinition, ComparisonId, ComparisonMetrics, ComparisonObservation,
};
use crate::comparison_controller::{
    ComparisonChannel, ComparisonController, ComparisonControllerError, ComparisonControllerPhase,
    ComparisonControllerStatus, ComparisonExportPin, ComparisonInvalidation,
    ComparisonSelectionRequest,
};
use crate::coverage::CoverageSummary;
use crate::daw_project::ProjectRevisions;
use crate::pane_session_binding::PaneSemanticSelection;
use crate::pattern_lang;
use crate::project_session::{ProjectAudioStatus, ProjectPublication, ScopedAuditionStatus};
use crate::render_plan::{ExactDigest, ProjectRevisionStamp};
use crate::render_runtime::TimelineAuditionId;
use crate::rhythm_explanation::{
    ExactAudioFallbackReason, ExplanationFit, GridRealization, PatternAlternativeId,
    PatternExplanation, PatternExplanationRepresentation, PatternExplanationSet,
    RejectedPatternTerm, RhythmEvidenceRef, TermRejection,
};
use crate::sequencer::{PatternDefinition, PatternTermHash};
use crate::workspace_document::LinkGroupId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExplanationPaneId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneActionId {
    pub pane: ExplanationPaneId,
    pub sequence: u64,
}

/// Persistent comparison and latest exact measurement associated with one
/// search alternative. Coverage is a compact summary, not a retained field or
/// PCM buffer.
#[derive(Clone, Debug, PartialEq)]
pub struct AlternativeComparisonBinding {
    pub alternative: PatternAlternativeId,
    pub definition: ComparisonDefinition,
    pub observation: Option<ComparisonObservation>,
    pub coverage: Option<CoverageSummary>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromotePatternActionIdentity {
    pub alternative: PatternAlternativeId,
    pub term: PatternTermHash,
    pub bindings: PatternTermHash,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PromotePatternRequest {
    pub action: PaneActionId,
    pub identity: PromotePatternActionIdentity,
    pub pattern: PatternDefinition,
    pub evidence: Vec<RhythmEvidenceRef>,
    pub requested_at: ProjectRevisions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneChannelRequestKind {
    /// Source, construction, and residual are exact aligned PCM requests.
    AuditionSignal,
    /// Excess has magnitude/power but no phase-preserving PCM definition.
    InspectCoverage,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PaneChannelRequest {
    pub action: PaneActionId,
    pub alternative: PatternAlternativeId,
    pub comparison: ComparisonId,
    pub channel: ComparisonChannel,
    pub kind: PaneChannelRequestKind,
    pub controller: ComparisonSelectionRequest,
}

pub struct PaneExportPin {
    pub action: PaneActionId,
    pub alternative: PatternAlternativeId,
    pub comparison: ComparisonId,
    pub channel: ComparisonChannel,
    pub pin: ComparisonExportPin,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExplanationPaneSnapshot {
    pub pane: ExplanationPaneId,
    pub rows: Vec<ExplanationAlternativeRow>,
    pub refusals: Vec<RefusalPresentation>,
    pub publication: Option<PanePublicationReceipt>,
    pub audio: Option<PaneAudioReceipt>,
    pub semantic: Option<PaneSemanticReceipt>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExplanationAlternativeRow {
    pub alternative: PatternAlternativeId,
    pub rank: usize,
    pub kind: AlternativeKindPresentation,
    pub mdl_fit: MdlFitPresentation,
    pub families: Vec<AnonymousFamilyPresentation>,
    pub evidence: Vec<EvidencePresentation>,
    pub refusals: Vec<RefusalPresentation>,
    pub comparison: ComparisonPresentation,
    pub promote: Option<PromotePatternActionIdentity>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AlternativeKindPresentation {
    GenerativeTerm {
        source: String,
        grid: GridRealization,
        diagnostics: Vec<String>,
        binding_count: usize,
    },
    ExactAudioFallback {
        start_frame: usize,
        end_frame: usize,
        estimated_literal_bytes: u64,
        reasons: Vec<ExactAudioFallbackReason>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MdlFitPresentation {
    pub description_bytes: u64,
    pub fit_penalty_millibytes: u64,
    pub total_millibytes: u64,
    pub timing_rms_frames: f64,
    pub timing_max_frames: f64,
    pub timing_fit: f32,
    /// Rhythm novelty-strength fit. Exact audio energy appears below in the
    /// comparison strip instead of being inferred from this proxy.
    pub onset_strength_rms: f64,
    pub onset_strength_fit: f32,
    pub combined_fit: f32,
}

impl From<(&ExplanationFit, crate::rhythm_explanation::DescriptionRank)> for MdlFitPresentation {
    fn from(
        (fit, description): (&ExplanationFit, crate::rhythm_explanation::DescriptionRank),
    ) -> Self {
        Self {
            description_bytes: description.description_bytes,
            fit_penalty_millibytes: description.fit_penalty_millibytes,
            total_millibytes: description.total_millibytes,
            timing_rms_frames: fit.timing_rms_frames,
            timing_max_frames: fit.timing_max_frames,
            timing_fit: fit.timing_fit,
            onset_strength_rms: fit.energy_rms,
            onset_strength_fit: fit.energy_fit,
            combined_fit: fit.combined_fit,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnonymousFamilyPresentation {
    pub family: usize,
    pub binding: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidencePresentation {
    pub reference: RhythmEvidenceRef,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusalPresentation {
    pub source: Option<String>,
    pub code: RefusalCode,
    pub detail: String,
    pub evidence: Vec<EvidencePresentation>,
    pub derivation_rule: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RefusalCode {
    MalformedPattern,
    NegativeOffset,
    OffsetOutsideCycle,
    DuplicateFamilyStep,
    Collision,
    Evaluation,
    ArithmeticOverflow,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ComparisonPresentation {
    pub comparison: Option<ComparisonId>,
    pub explanation: Option<crate::explanation::ExplanationId>,
    pub phase: PaneComparisonPhase,
    pub metrics: Option<ComparisonMetrics>,
    pub channels: Vec<ComparisonChannelPresentation>,
    pub export: ExportPinPresentation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneComparisonPhase {
    Unbound,
    AwaitingObservation,
    Available,
    Controller(ComparisonControllerPhase),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelSemantic {
    /// The exact cited source PCM.
    Source,
    /// The frozen explanation rendered alone.
    Construction,
    /// Sample-aligned `source - construction`, without gain fitting.
    ExactResidual,
    /// Nonnegative time-frequency construction-power surplus. No PCM is
    /// implied because phase was not retained.
    SpectralExcess,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ChannelMeasurement {
    SampleEnergy(f64),
    SpectralExcess {
        ratio: f64,
        source_power: f64,
        construction_power: f64,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ComparisonChannelPresentation {
    pub channel: ComparisonChannel,
    pub semantic: ChannelSemantic,
    pub request_kind: PaneChannelRequestKind,
    pub selected: bool,
    pub active: bool,
    pub can_request: bool,
    pub auditionable: bool,
    pub exportable: bool,
    pub measurement: Option<ChannelMeasurement>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportPinPresentation {
    Unavailable,
    ReadyButNotAudible,
    Available {
        audition: TimelineAuditionId,
        pcm: ExactDigest,
        producing_revision: ProjectRevisionStamp,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanePublicationReceipt {
    pub generation: u64,
    pub revisions: ProjectRevisions,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneAudioReceipt {
    pub scoped_audition: Option<ScopedAuditionStatus>,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneSemanticReceipt {
    pub group: LinkGroupId,
    pub link_revision: u64,
    pub alternative: Option<PatternAlternativeId>,
    pub channel: Option<ComparisonChannel>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticDeliveryDisposition {
    Applied,
    SuppressedOlderOrEcho,
    Unresolved,
}

pub struct ExplanationPaneModel {
    pane: ExplanationPaneId,
    next_action: u64,
    alternatives: Vec<PatternExplanation>,
    refusals: Vec<RejectedPatternTerm>,
    comparisons: BTreeMap<PatternAlternativeId, AlternativeComparisonBinding>,
    comparison_ids: BTreeMap<ComparisonId, PatternAlternativeId>,
    controller: Option<ComparisonControllerStatus>,
    publication: Option<PanePublicationReceipt>,
    audio: Option<PaneAudioReceipt>,
    semantic: Option<PaneSemanticReceipt>,
    accepted_links: BTreeMap<LinkGroupId, u64>,
    local_selection: Option<(PatternAlternativeId, ComparisonChannel)>,
}

impl ExplanationPaneModel {
    pub fn new(pane_local: u64) -> Result<Self, ExplanationPaneError> {
        if pane_local == 0 {
            return Err(ExplanationPaneError::ZeroPane);
        }
        Ok(Self {
            pane: ExplanationPaneId(pane_local),
            next_action: 1,
            alternatives: Vec::new(),
            refusals: Vec::new(),
            comparisons: BTreeMap::new(),
            comparison_ids: BTreeMap::new(),
            controller: None,
            publication: None,
            audio: None,
            semantic: None,
            accepted_links: BTreeMap::new(),
            local_selection: None,
        })
    }

    pub const fn pane(&self) -> ExplanationPaneId {
        self.pane
    }

    pub fn install(
        &mut self,
        mut explanations: PatternExplanationSet,
        bindings: Vec<AlternativeComparisonBinding>,
    ) -> Result<(), ExplanationPaneError> {
        explanations.alternatives.sort_by(|left, right| {
            left.rank
                .cmp(&right.rank)
                .then_with(|| left.id.cmp(&right.id))
        });
        let ids = explanations
            .alternatives
            .iter()
            .map(|alternative| alternative.id)
            .collect::<BTreeSet<_>>();
        if ids.len() != explanations.alternatives.len() {
            return Err(ExplanationPaneError::DuplicateAlternative);
        }
        let mut comparisons = BTreeMap::new();
        let mut comparison_ids = BTreeMap::new();
        for binding in bindings {
            if !ids.contains(&binding.alternative) {
                return Err(ExplanationPaneError::UnknownAlternative(
                    binding.alternative,
                ));
            }
            binding
                .definition
                .validate()
                .map_err(|error| ExplanationPaneError::InvalidComparison(error.to_string()))?;
            if binding.coverage.is_some() && binding.observation.is_none() {
                return Err(ExplanationPaneError::CoverageWithoutObservation(
                    binding.definition.id,
                ));
            }
            if comparisons
                .insert(binding.alternative, binding.clone())
                .is_some()
            {
                return Err(ExplanationPaneError::DuplicateAlternativeBinding(
                    binding.alternative,
                ));
            }
            if comparison_ids
                .insert(binding.definition.id, binding.alternative)
                .is_some()
            {
                return Err(ExplanationPaneError::DuplicateComparison(
                    binding.definition.id,
                ));
            }
        }
        if self
            .local_selection
            .is_some_and(|(alternative, _)| !ids.contains(&alternative))
        {
            self.local_selection = None;
        }
        self.alternatives = explanations.alternatives;
        self.refusals = explanations.rejected_terms;
        self.comparisons = comparisons;
        self.comparison_ids = comparison_ids;
        Ok(())
    }

    /// Consume the project publication and forward only its invalidation fact
    /// to the external comparison controller. The snapshot itself is never
    /// retained by this pane.
    pub fn observe_project_publication(
        &mut self,
        publication: &ProjectPublication,
        controller: &mut ComparisonController,
    ) -> Option<ComparisonInvalidation> {
        self.publication = Some(PanePublicationReceipt {
            generation: publication.generation,
            revisions: publication.revisions,
        });
        let invalidation = controller.observe_publication(publication);
        self.controller = Some(controller.status());
        invalidation
    }

    /// Consume the shared audio receipt. The controller remains authoritative
    /// for whether a ready comparison product is actually audible.
    pub fn observe_audio_status(
        &mut self,
        status: &ProjectAudioStatus,
        controller: &mut ComparisonController,
    ) {
        controller.observe_audio_status(status);
        self.controller = Some(controller.status());
        self.audio = Some(PaneAudioReceipt {
            scoped_audition: status.scoped_audition,
            diagnostic: status.diagnostic.clone(),
        });
    }

    pub fn observe_controller(&mut self, controller: &ComparisonController) {
        self.controller = Some(controller.status());
    }

    /// Accept an addressed semantic-selection delivery without publishing it
    /// again. Only the resolved row/channel and link stamp are retained.
    pub fn observe_semantic_selection(
        &mut self,
        delivery: &PaneSemanticSelection,
    ) -> SemanticDeliveryDisposition {
        if self
            .accepted_links
            .get(&delivery.group)
            .is_some_and(|revision| *revision >= delivery.link_revision)
        {
            return SemanticDeliveryDisposition::SuppressedOlderOrEcho;
        }
        self.accepted_links
            .insert(delivery.group, delivery.link_revision);
        let resolved = self.resolve_semantic_focus(delivery);
        self.local_selection = resolved;
        self.semantic = Some(PaneSemanticReceipt {
            group: delivery.group,
            link_revision: delivery.link_revision,
            alternative: resolved.map(|(alternative, _)| alternative),
            channel: resolved.map(|(_, channel)| channel),
        });
        if resolved.is_some() {
            SemanticDeliveryDisposition::Applied
        } else {
            SemanticDeliveryDisposition::Unresolved
        }
    }

    pub fn request_channel(
        &mut self,
        alternative: PatternAlternativeId,
        channel: ComparisonChannel,
        controller: &mut ComparisonController,
    ) -> Result<PaneChannelRequest, ExplanationPaneError> {
        let revisions = self
            .publication
            .ok_or(ExplanationPaneError::MissingPublication)?
            .revisions;
        let (comparison, request) =
            {
                let binding = self
                    .comparisons
                    .get(&alternative)
                    .ok_or(ExplanationPaneError::UnboundAlternative(alternative))?;
                let observation = binding.observation.as_ref().ok_or(
                    ExplanationPaneError::MissingObservation(binding.definition.id),
                )?;
                let request = controller
                    .select(&binding.definition, observation, revisions, channel)
                    .map_err(ExplanationPaneError::Controller)?;
                (binding.definition.id, request)
            };
        let action = self.allocate_action()?;
        self.local_selection = Some((alternative, channel));
        self.controller = Some(controller.status());
        Ok(PaneChannelRequest {
            action,
            alternative,
            comparison,
            channel,
            kind: if channel == ComparisonChannel::Excess {
                PaneChannelRequestKind::InspectCoverage
            } else {
                PaneChannelRequestKind::AuditionSignal
            },
            controller: request,
        })
    }

    /// Pin only the exact signal the shared controller reports as active. A
    /// ready-but-not-audible product and spectral excess are both refused.
    pub fn request_export_pin(
        &mut self,
        alternative: PatternAlternativeId,
        controller: &ComparisonController,
        status: &ProjectAudioStatus,
    ) -> Result<PaneExportPin, ExplanationPaneError> {
        let comparison = self
            .comparisons
            .get(&alternative)
            .ok_or(ExplanationPaneError::UnboundAlternative(alternative))?
            .definition
            .id;
        let controller_status = controller.status();
        let selection = controller_status
            .selection
            .as_ref()
            .ok_or(ExplanationPaneError::NoControllerSelection)?;
        if selection.comparison != comparison {
            return Err(ExplanationPaneError::ControllerSelectionMismatch);
        }
        if selection.channel == ComparisonChannel::Excess {
            return Err(ExplanationPaneError::ExcessHasNoExportPcm);
        }
        let pin = controller
            .pin_audible_export_from_status(status)
            .map_err(ExplanationPaneError::Controller)?;
        let action = self.allocate_action()?;
        Ok(PaneExportPin {
            action,
            alternative,
            comparison,
            channel: selection.channel,
            pin,
        })
    }

    /// Emit a content-identified promotion request. The caller chooses the
    /// command/envelope and allocates durable project IDs; this pane never
    /// mutates the project or treats acceptance as analytic correctness.
    pub fn request_promote(
        &mut self,
        alternative: PatternAlternativeId,
    ) -> Result<PromotePatternRequest, ExplanationPaneError> {
        let requested_at = self
            .publication
            .ok_or(ExplanationPaneError::MissingPublication)?
            .revisions;
        let explanation = self
            .alternatives
            .iter()
            .find(|candidate| candidate.id == alternative)
            .ok_or(ExplanationPaneError::UnknownAlternative(alternative))?;
        let term = explanation
            .term()
            .ok_or(ExplanationPaneError::LiteralAudioCannotPromote)?;
        let identity = promotion_identity(explanation)
            .ok_or(ExplanationPaneError::LiteralAudioCannotPromote)?;
        let pattern = term.pattern.clone();
        let evidence = explanation.evidence.clone();
        let action = self.allocate_action()?;
        Ok(PromotePatternRequest {
            action,
            identity,
            pattern,
            evidence,
            requested_at,
        })
    }

    pub fn snapshot(&self) -> ExplanationPaneSnapshot {
        ExplanationPaneSnapshot {
            pane: self.pane,
            rows: self
                .alternatives
                .iter()
                .map(|alternative| self.row(alternative))
                .collect(),
            refusals: self.refusals.iter().map(refusal_presentation).collect(),
            publication: self.publication,
            audio: self.audio.clone(),
            semantic: self.semantic,
        }
    }

    fn row(&self, alternative: &PatternExplanation) -> ExplanationAlternativeRow {
        let kind = match &alternative.representation {
            PatternExplanationRepresentation::Term(term) => {
                AlternativeKindPresentation::GenerativeTerm {
                    source: term.source.clone(),
                    grid: term.grid.clone(),
                    diagnostics: term
                        .diagnostics
                        .iter()
                        .copied()
                        .map(crate::pattern_authoring::format_diagnostic)
                        .collect(),
                    binding_count: term.bindings.len(),
                }
            }
            PatternExplanationRepresentation::ExactAudio(fallback) => {
                AlternativeKindPresentation::ExactAudioFallback {
                    start_frame: fallback.source_span.start,
                    end_frame: fallback.source_span.end,
                    estimated_literal_bytes: fallback.estimated_literal_bytes,
                    reasons: fallback.reasons.clone(),
                }
            }
        };
        let evidence = alternative
            .evidence
            .iter()
            .copied()
            .map(evidence_presentation)
            .collect::<Vec<_>>();
        let refusals = self
            .refusals
            .iter()
            .filter(|rejection| {
                rejection.evidence.is_empty()
                    || rejection
                        .evidence
                        .iter()
                        .any(|reference| alternative.evidence.contains(reference))
            })
            .map(refusal_presentation)
            .collect();
        ExplanationAlternativeRow {
            alternative: alternative.id,
            rank: alternative.rank,
            kind,
            mdl_fit: (&alternative.fit, alternative.description).into(),
            families: alternative
                .families
                .iter()
                .map(|(family, binding)| AnonymousFamilyPresentation {
                    family: *family,
                    binding: binding.clone(),
                })
                .collect(),
            evidence,
            refusals,
            comparison: self.comparison_presentation(alternative.id),
            promote: promotion_identity(alternative),
        }
    }

    fn comparison_presentation(&self, alternative: PatternAlternativeId) -> ComparisonPresentation {
        let Some(binding) = self.comparisons.get(&alternative) else {
            return ComparisonPresentation {
                comparison: None,
                explanation: None,
                phase: PaneComparisonPhase::Unbound,
                metrics: None,
                channels: comparison_channels(None, None, None, None),
                export: ExportPinPresentation::Unavailable,
            };
        };
        let selected = self.selected_for(alternative);
        let controller_applies = self.controller.as_ref().is_some_and(|status| {
            status
                .selection
                .as_ref()
                .is_some_and(|selection| selection.comparison == binding.definition.id)
        });
        let phase = if controller_applies {
            PaneComparisonPhase::Controller(
                self.controller
                    .as_ref()
                    .expect("checked controller")
                    .phase
                    .clone(),
            )
        } else if binding.observation.is_none() {
            PaneComparisonPhase::AwaitingObservation
        } else {
            PaneComparisonPhase::Available
        };
        let active_channel = self.controller.as_ref().and_then(|status| {
            (controller_applies && status.phase == ComparisonControllerPhase::Active)
                .then(|| status.selection.as_ref().map(|selection| selection.channel))
                .flatten()
        });
        let metrics = binding.observation.as_ref().map(|value| value.metrics);
        let channels = comparison_channels(metrics, binding.coverage, selected, active_channel);
        let export = export_presentation(self.controller.as_ref(), controller_applies, selected);
        ComparisonPresentation {
            comparison: Some(binding.definition.id),
            explanation: Some(binding.definition.explanation),
            phase,
            metrics,
            channels,
            export,
        }
    }

    fn selected_for(&self, alternative: PatternAlternativeId) -> Option<ComparisonChannel> {
        self.controller
            .as_ref()
            .and_then(|status| status.selection.as_ref())
            .and_then(|selection| {
                self.comparisons
                    .get(&alternative)
                    .is_some_and(|binding| binding.definition.id == selection.comparison)
                    .then_some(selection.channel)
            })
            .or_else(|| {
                self.local_selection
                    .filter(|(selected, _)| *selected == alternative)
                    .map(|(_, channel)| channel)
            })
    }

    fn resolve_semantic_focus(
        &self,
        delivery: &PaneSemanticSelection,
    ) -> Option<(PatternAlternativeId, ComparisonChannel)> {
        let find_reference = |reference: ExplanationRef| match reference {
            ExplanationRef::Definition(id) => self.alternatives.iter().find_map(|alternative| {
                let binding = self.comparisons.get(&alternative.id)?;
                (binding.definition.explanation.0 == id).then_some(alternative.id)
            }),
            ExplanationRef::Comparison(id) => self.comparison_ids.get(&ComparisonId(id)).copied(),
            ExplanationRef::Proposal(_) => None,
        };
        match delivery.signal {
            SignalLayer::Explanation(reference) => find_reference(reference)
                .map(|alternative| (alternative, ComparisonChannel::Construction)),
            SignalLayer::Residual(reference) => find_reference(reference)
                .map(|alternative| (alternative, ComparisonChannel::Residual)),
            SignalLayer::Source => {
                let alternative = delivery
                    .selection
                    .time
                    .and_then(|time| {
                        self.alternatives.iter().find_map(|alternative| {
                            let binding = self.comparisons.get(&alternative.id)?;
                            let span = binding.definition.source.project_span;
                            (time.start < span.end && span.start < time.end)
                                .then_some(alternative.id)
                        })
                    })
                    .or_else(|| self.local_selection.map(|(alternative, _)| alternative))
                    .or_else(|| {
                        self.alternatives
                            .iter()
                            .find(|alternative| self.comparisons.contains_key(&alternative.id))
                            .map(|alternative| alternative.id)
                    })?;
                Some((alternative, ComparisonChannel::Source))
            }
        }
    }

    fn allocate_action(&mut self) -> Result<PaneActionId, ExplanationPaneError> {
        let sequence = self.next_action;
        self.next_action = self
            .next_action
            .checked_add(1)
            .ok_or(ExplanationPaneError::ActionIdentityExhausted)?;
        Ok(PaneActionId {
            pane: self.pane,
            sequence,
        })
    }
}

fn comparison_channels(
    metrics: Option<ComparisonMetrics>,
    coverage: Option<CoverageSummary>,
    selected: Option<ComparisonChannel>,
    active: Option<ComparisonChannel>,
) -> Vec<ComparisonChannelPresentation> {
    [
        ComparisonChannel::Source,
        ComparisonChannel::Construction,
        ComparisonChannel::Residual,
        ComparisonChannel::Excess,
    ]
    .into_iter()
    .map(|channel| {
        let (semantic, request_kind, auditionable, exportable, measurement) = match channel {
            ComparisonChannel::Source => (
                ChannelSemantic::Source,
                PaneChannelRequestKind::AuditionSignal,
                true,
                true,
                metrics.map(|metrics| ChannelMeasurement::SampleEnergy(metrics.source_energy)),
            ),
            ComparisonChannel::Construction => (
                ChannelSemantic::Construction,
                PaneChannelRequestKind::AuditionSignal,
                true,
                true,
                metrics
                    .map(|metrics| ChannelMeasurement::SampleEnergy(metrics.construction_energy)),
            ),
            ComparisonChannel::Residual => (
                ChannelSemantic::ExactResidual,
                PaneChannelRequestKind::AuditionSignal,
                true,
                true,
                metrics.map(|metrics| ChannelMeasurement::SampleEnergy(metrics.residual_energy)),
            ),
            ComparisonChannel::Excess => (
                ChannelSemantic::SpectralExcess,
                PaneChannelRequestKind::InspectCoverage,
                false,
                false,
                coverage.map(|coverage| ChannelMeasurement::SpectralExcess {
                    ratio: coverage.excess_energy_ratio,
                    source_power: coverage.source_power,
                    construction_power: coverage.construction_power,
                }),
            ),
        };
        ComparisonChannelPresentation {
            channel,
            semantic,
            request_kind,
            selected: selected == Some(channel),
            active: active == Some(channel),
            can_request: metrics.is_some(),
            auditionable,
            exportable,
            measurement,
        }
    })
    .collect()
}

fn export_presentation(
    controller: Option<&ComparisonControllerStatus>,
    controller_applies: bool,
    selected: Option<ComparisonChannel>,
) -> ExportPinPresentation {
    let Some(controller) = controller.filter(|_| controller_applies) else {
        return ExportPinPresentation::Unavailable;
    };
    if selected == Some(ComparisonChannel::Excess) {
        return ExportPinPresentation::Unavailable;
    }
    match (
        &controller.phase,
        controller.audition,
        controller.pcm_digest,
        controller.producing_revision,
    ) {
        (
            ComparisonControllerPhase::Active,
            Some(audition),
            Some(pcm),
            Some(producing_revision),
        ) => ExportPinPresentation::Available {
            audition,
            pcm,
            producing_revision,
        },
        (ComparisonControllerPhase::Ready | ComparisonControllerPhase::Publishing, ..) => {
            ExportPinPresentation::ReadyButNotAudible
        }
        _ => ExportPinPresentation::Unavailable,
    }
}

fn promotion_identity(alternative: &PatternExplanation) -> Option<PromotePatternActionIdentity> {
    let term = alternative.term()?;
    Some(PromotePatternActionIdentity {
        alternative: alternative.id,
        term: pattern_lang::term_hash(&term.expr),
        bindings: pattern_lang::bindings_hash(&term.bindings),
    })
}

fn evidence_presentation(reference: RhythmEvidenceRef) -> EvidencePresentation {
    let label = match reference {
        RhythmEvidenceRef::Pattern(id) => format!("pattern hypothesis {id}"),
        RhythmEvidenceRef::Hit(id) => format!("rhythm hit {id}"),
        RhythmEvidenceRef::Family(id) => format!("anonymous family {id}"),
        RhythmEvidenceRef::Tempo(id) => format!("tempo hypothesis {id}"),
        RhythmEvidenceRef::BeatPhase(id) => format!("beat-phase hypothesis {id}"),
    };
    EvidencePresentation { reference, label }
}

fn refusal_presentation(rejection: &RejectedPatternTerm) -> RefusalPresentation {
    let (code, detail) = match &rejection.reason {
        TermRejection::MalformedPattern => (
            RefusalCode::MalformedPattern,
            "family and offset sequences have different lengths".to_owned(),
        ),
        TermRejection::NegativeOffset(offset) => (
            RefusalCode::NegativeOffset,
            format!("relative step offset {offset} is negative"),
        ),
        TermRejection::OffsetOutsideCycle {
            offset,
            cycle_steps,
        } => (
            RefusalCode::OffsetOutsideCycle,
            format!("step {offset} lies outside the {cycle_steps}-step cycle"),
        ),
        TermRejection::DuplicateFamilyStep { family, offset } => (
            RefusalCode::DuplicateFamilyStep,
            format!("family {family} has more than one event at step {offset}"),
        ),
        TermRejection::Collision { binding, tick } => (
            RefusalCode::Collision,
            format!("binding {binding} collides at tick {tick}"),
        ),
        TermRejection::Evaluation(message) => (RefusalCode::Evaluation, message.clone()),
        TermRejection::ArithmeticOverflow => (
            RefusalCode::ArithmeticOverflow,
            "term or grid arithmetic exceeded its bounded representation".to_owned(),
        ),
    };
    RefusalPresentation {
        source: rejection.source.clone(),
        code,
        detail,
        evidence: rejection
            .evidence
            .iter()
            .copied()
            .map(evidence_presentation)
            .collect(),
        derivation_rule: rejection.derivation.rule.clone(),
    }
}

#[derive(Debug)]
pub enum ExplanationPaneError {
    ZeroPane,
    ActionIdentityExhausted,
    DuplicateAlternative,
    UnknownAlternative(PatternAlternativeId),
    DuplicateAlternativeBinding(PatternAlternativeId),
    DuplicateComparison(ComparisonId),
    InvalidComparison(String),
    CoverageWithoutObservation(ComparisonId),
    MissingPublication,
    UnboundAlternative(PatternAlternativeId),
    MissingObservation(ComparisonId),
    NoControllerSelection,
    ControllerSelectionMismatch,
    ExcessHasNoExportPcm,
    LiteralAudioCannotPromote,
    Controller(ComparisonControllerError),
}

impl fmt::Display for ExplanationPaneError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroPane => formatter.write_str("explanation pane identity must be non-zero"),
            Self::ActionIdentityExhausted => formatter.write_str("pane action identity exhausted"),
            Self::DuplicateAlternative => {
                formatter.write_str("explanation set contains a duplicate alternative")
            }
            Self::UnknownAlternative(id) => write!(formatter, "unknown alternative {:?}", id.0),
            Self::DuplicateAlternativeBinding(id) => {
                write!(
                    formatter,
                    "alternative {:?} has two comparison bindings",
                    id.0
                )
            }
            Self::DuplicateComparison(id) => {
                write!(
                    formatter,
                    "comparison {} is bound to two alternatives",
                    id.0
                )
            }
            Self::InvalidComparison(message) => write!(formatter, "invalid comparison: {message}"),
            Self::CoverageWithoutObservation(id) => write!(
                formatter,
                "comparison {} has coverage without an observation",
                id.0
            ),
            Self::MissingPublication => {
                formatter.write_str("pane has not received a project publication")
            }
            Self::UnboundAlternative(id) => {
                write!(formatter, "alternative {:?} has no comparison", id.0)
            }
            Self::MissingObservation(id) => {
                write!(formatter, "comparison {} has not been rendered", id.0)
            }
            Self::NoControllerSelection => {
                formatter.write_str("comparison controller has no selection")
            }
            Self::ControllerSelectionMismatch => {
                formatter.write_str("controller selection belongs to another comparison")
            }
            Self::ExcessHasNoExportPcm => {
                formatter.write_str("spectral excess has no time-domain PCM export")
            }
            Self::LiteralAudioCannotPromote => {
                formatter.write_str("literal audio is not a promotable generator term")
            }
            Self::Controller(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ExplanationPaneError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::artifact_catalog::sha256_content;
    use crate::aspect::{ChannelMask, FrameSpan};
    use crate::assets::{AssetFrameRange, AssetId, SampleFrames};
    use crate::audio::{AudioFormat, ProjectAudio};
    use crate::comparison::{render_comparison, ExactRenderDigest};
    use crate::coverage::{compute_coverage, CoverageRecipe};
    use crate::daw_project::DawProject;
    use crate::daw_render::RenderCancellation;
    use crate::explanation::{ExplanationDependencyPin, ExplanationId, RenderedExplanation};
    use crate::live_project::LiveProject;
    use crate::ontology::{Producer, Provenance};
    use crate::project_selection::ProjectSelection;
    use crate::render_validation::GoldenFingerprint;
    use crate::rhythm::{
        BeatPhaseHypothesis, EventFamilyHypothesis, HitObservation, MedoidSampleReference,
        PatternHypothesis, PatternOccurrence, RhythmDeprojection, TempoHypothesis, TempoRelation,
    };
    use crate::rhythm_explanation::{explain_rhythm, ExplainBudget};

    fn provenance() -> Provenance {
        Provenance {
            producer: Producer::Human { name: None },
            created_unix_ms: None,
            source_revision: None,
            note: None,
        }
    }

    fn deprojection(offsets: Vec<i32>, family: usize) -> RhythmDeprojection {
        let hits = (0..4)
            .map(|index| HitObservation {
                peak_sample: index * 125,
                onset_sample: index * 125,
                onset_seconds: index as f64 * 0.125,
                novelty_strength: 1.0,
                family: Some(family),
                ..HitObservation::default()
            })
            .collect();
        RhythmDeprojection {
            sample_rate: 1_000,
            sample_frames: 4_000,
            hits,
            tempo_hypotheses: vec![TempoHypothesis {
                rank: 0,
                bpm: 120.0,
                period_frames: 500.0,
                periodicity: 1.0,
                evidence: 1.0,
                relation: TempoRelation::Independent,
            }],
            beat_phase_hypotheses: vec![BeatPhaseHypothesis {
                tempo_rank: 0,
                bpm: 120.0,
                phase_seconds: 0.0,
                score: 1.0,
                beat_samples: vec![0, 500],
            }],
            event_families: vec![EventFamilyHypothesis {
                id: family,
                event_indices: vec![0, 1, 2, 3],
                medoid: MedoidSampleReference::default(),
                mean_medoid_similarity: 1.0,
                minimum_medoid_similarity: 1.0,
                evidence: 1.0,
            }],
            patterns: vec![PatternHypothesis {
                family_sequence: vec![family; offsets.len()],
                step_offsets: offsets.clone(),
                occurrences: vec![PatternOccurrence {
                    event_index: 0,
                    start_sample: 0,
                    beat_position: 0.0,
                }],
                evidence: 1.0,
            }],
            ..RhythmDeprojection::default()
        }
    }

    fn fingerprint() -> GoldenFingerprint {
        GoldenFingerprint {
            version: GoldenFingerprint::VERSION,
            sample_rate: 8_000,
            channels: 1,
            frames: 4,
            first_active_offset: Some(0),
            last_active_offset: Some(3),
            peak_millionths: 1,
            rms_millionths: 1,
            dc_millionths: 0,
            block_energy_hash: 1,
        }
    }

    fn exact_measurements() -> (ComparisonMetrics, CoverageSummary) {
        let format = AudioFormat::new(8_000, 1).unwrap();
        let source = ProjectAudio::from_interleaved(format, vec![1.0, 2.0, 3.0, 4.0]).unwrap();
        let construction =
            ProjectAudio::from_interleaved(format, vec![1.0, 4.0, 3.0, 4.0]).unwrap();
        let rendered = render_comparison(
            20,
            source,
            RenderedExplanation {
                origin_frame: 20,
                audio: construction,
            },
        )
        .unwrap();
        assert_eq!(rendered.residual.interleaved(), &[0.0, -2.0, 0.0, 0.0]);
        let coverage = compute_coverage(
            &rendered,
            CoverageRecipe {
                fft_size: 4,
                hop_size: 1,
                power_floor: 1.0e-12,
            },
            &RenderCancellation::new(),
        )
        .unwrap();
        assert!(coverage.summary.excess_energy_ratio > 0.0);
        (rendered.metrics, coverage.summary)
    }

    fn binding(
        alternative: PatternAlternativeId,
        metrics: ComparisonMetrics,
        coverage: CoverageSummary,
    ) -> AlternativeComparisonBinding {
        let digest =
            |name: &'static [u8]| ExactRenderDigest::new(sha256_content(name, &[name])).unwrap();
        AlternativeComparisonBinding {
            alternative,
            definition: ComparisonDefinition {
                id: ComparisonId(1),
                label: "pattern comparison".to_owned(),
                source: crate::comparison::SourceCitation {
                    asset: AssetId(1),
                    source_range: AssetFrameRange::new(SampleFrames(0), SampleFrames(4)).unwrap(),
                    project_span: FrameSpan { start: 20, end: 24 },
                    channels: ChannelMask(1),
                },
                explanation: ExplanationId(1),
                provenance: provenance(),
            },
            observation: Some(ComparisonObservation {
                dependencies: ExplanationDependencyPin::default(),
                source_digest: digest(b"source"),
                construction_digest: digest(b"construction"),
                residual_digest: digest(b"residual"),
                construction_fingerprint: fingerprint(),
                residual_fingerprint: fingerprint(),
                metrics,
            }),
            coverage: Some(coverage),
        }
    }

    fn publication() -> ProjectPublication {
        let project = DawProject::new("pane", 8_000, 120.0).unwrap();
        let live = LiveProject::from_project(project, BTreeMap::new()).unwrap();
        let snapshot = live.snapshot().unwrap();
        ProjectPublication {
            generation: 1,
            revisions: snapshot.revisions(),
            snapshot,
            change_set: None,
        }
    }

    #[test]
    fn rows_keep_terms_literal_fallback_evidence_refusals_and_promote_identity() {
        let set = explain_rhythm(
            &deprojection(vec![0, 1], 9),
            &[9],
            ExplainBudget {
                cycle_sixteenths: Some(128),
                ..ExplainBudget::default()
            },
        )
        .unwrap();
        let mut model = ExplanationPaneModel::new(4).unwrap();
        model.install(set, Vec::new()).unwrap();
        let snapshot = model.snapshot();
        assert!(snapshot.rows.iter().any(|row| matches!(
            &row.kind,
            AlternativeKindPresentation::ExactAudioFallback { .. }
        )));
        assert!(snapshot
            .refusals
            .iter()
            .any(|refusal| refusal.code == RefusalCode::Collision));
        assert!(snapshot.rows.iter().all(|row| !row.evidence.is_empty()));
        assert!(snapshot.rows.iter().any(|row| row.promote.is_none()));

        let term_set = explain_rhythm(
            &deprojection(vec![0, 3, 6], 4),
            &[4],
            ExplainBudget {
                cycle_sixteenths: Some(8),
                ..ExplainBudget::default()
            },
        )
        .unwrap();
        model.install(term_set, Vec::new()).unwrap();
        let term_row = model
            .snapshot()
            .rows
            .into_iter()
            .find(|row| {
                matches!(
                    &row.kind,
                    AlternativeKindPresentation::GenerativeTerm { .. }
                )
            })
            .unwrap();
        assert!(term_row.promote.is_some());
        assert_eq!(term_row.families[0].binding, "fam4");
    }

    #[test]
    fn exact_residual_is_auditionable_but_spectral_excess_is_coverage_only() {
        let set = explain_rhythm(
            &deprojection(vec![0, 3, 6], 4),
            &[4],
            ExplainBudget {
                cycle_sixteenths: Some(8),
                ..ExplainBudget::default()
            },
        )
        .unwrap();
        let alternative = set
            .alternatives
            .iter()
            .find(|alternative| alternative.term().is_some())
            .unwrap()
            .id;
        let (metrics, coverage) = exact_measurements();
        let mut model = ExplanationPaneModel::new(7).unwrap();
        model
            .install(set, vec![binding(alternative, metrics, coverage)])
            .unwrap();
        let mut controller = ComparisonController::new(7).unwrap();
        model.observe_project_publication(&publication(), &mut controller);

        let residual = model
            .snapshot()
            .rows
            .iter()
            .find(|row| row.alternative == alternative)
            .unwrap()
            .comparison
            .channels
            .iter()
            .find(|channel| channel.channel == ComparisonChannel::Residual)
            .unwrap()
            .clone();
        assert_eq!(residual.semantic, ChannelSemantic::ExactResidual);
        assert!(residual.auditionable && residual.exportable);
        assert_eq!(
            residual.measurement,
            Some(ChannelMeasurement::SampleEnergy(metrics.residual_energy))
        );
        let request = model
            .request_channel(alternative, ComparisonChannel::Residual, &mut controller)
            .unwrap();
        assert_eq!(request.kind, PaneChannelRequestKind::AuditionSignal);
        assert_eq!(request.controller.channel, ComparisonChannel::Residual);

        let excess = model
            .snapshot()
            .rows
            .iter()
            .find(|row| row.alternative == alternative)
            .unwrap()
            .comparison
            .channels
            .iter()
            .find(|channel| channel.channel == ComparisonChannel::Excess)
            .unwrap()
            .clone();
        assert_eq!(excess.semantic, ChannelSemantic::SpectralExcess);
        assert!(!excess.auditionable && !excess.exportable);
        assert!(matches!(
            excess.measurement,
            Some(ChannelMeasurement::SpectralExcess { ratio, .. }) if ratio > 0.0
        ));
        let request = model
            .request_channel(alternative, ComparisonChannel::Excess, &mut controller)
            .unwrap();
        assert_eq!(request.kind, PaneChannelRequestKind::InspectCoverage);
        assert!(matches!(
            model.request_export_pin(alternative, &controller, &ProjectAudioStatus::default()),
            Err(ExplanationPaneError::ExcessHasNoExportPcm)
        ));
    }

    #[test]
    fn publication_audio_and_semantic_deliveries_update_receipts_without_owning_session() {
        let set = explain_rhythm(
            &deprojection(vec![0, 3, 6], 4),
            &[4],
            ExplainBudget {
                cycle_sixteenths: Some(8),
                ..ExplainBudget::default()
            },
        )
        .unwrap();
        let alternative = set
            .alternatives
            .iter()
            .find(|alternative| alternative.term().is_some())
            .unwrap()
            .id;
        let (metrics, coverage) = exact_measurements();
        let mut model = ExplanationPaneModel::new(8).unwrap();
        model
            .install(set, vec![binding(alternative, metrics, coverage)])
            .unwrap();
        let mut controller = ComparisonController::new(8).unwrap();
        let publication = publication();
        assert!(model
            .observe_project_publication(&publication, &mut controller)
            .is_none());
        model.observe_audio_status(&ProjectAudioStatus::default(), &mut controller);

        let delivery = PaneSemanticSelection {
            selection: ProjectSelection {
                time: Some(FrameSpan { start: 20, end: 24 }),
                signal: Some(SignalLayer::Residual(ExplanationRef::Definition(1))),
                ..ProjectSelection::default()
            },
            signal: SignalLayer::Residual(ExplanationRef::Definition(1)),
            group: LinkGroupId(2),
            link_revision: 3,
        };
        assert_eq!(
            model.observe_semantic_selection(&delivery),
            SemanticDeliveryDisposition::Applied
        );
        assert_eq!(
            model.observe_semantic_selection(&delivery),
            SemanticDeliveryDisposition::SuppressedOlderOrEcho
        );
        let snapshot = model.snapshot();
        assert_eq!(snapshot.publication.unwrap().generation, 1);
        assert!(snapshot.audio.is_some());
        assert_eq!(
            snapshot.semantic.unwrap().channel,
            Some(ComparisonChannel::Residual)
        );
        let row = snapshot
            .rows
            .iter()
            .find(|row| row.alternative == alternative)
            .unwrap();
        assert!(row
            .comparison
            .channels
            .iter()
            .any(|channel| channel.channel == ComparisonChannel::Residual && channel.selected));
    }
}
