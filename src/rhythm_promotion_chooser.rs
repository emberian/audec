//! UI-neutral choice state for reverse-to-forward rhythm promotion.
//!
//! Planning may rank competing grid readings, but rank is not acceptance. This
//! model keeps every plan inert until a caller selects its scoped proposal,
//! pins preview/apply work to both project and semantic-selection revisions,
//! and delegates publication and undo to [`ProjectSession`]. It does not name
//! anonymous event families or turn fit into a correctness claim.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::aspect::{Aspect, SignalLayer};
use crate::constructive::{ConstructiveCause, ConstructiveEditPlan};
use crate::daw_project::ProjectRevisions;
use crate::project_controller::{
    ConstructivePublication, RhythmGridHypothesis, RhythmPromotionDiagnostic,
    RhythmPromotionIntent, RhythmPromotionSet,
};
use crate::project_selection::SelectionAspectError;
use crate::project_session::{ProjectSession, ProjectSessionError};
use crate::rhythm::RhythmDeprojection;
use crate::rhythm_explanation::{
    PatternAlternativeId, PatternExplanationRepresentation, PatternExplanationSet,
    RhythmEvidenceRef,
};
use crate::sample_actions::SampleSelection;
use crate::sample_material::{ScopedEvidenceRef, ScopedProposalRef};

/// Stable chooser identity. Reconstruction-local IDs are never sufficient;
/// the derivation scope is part of the identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RhythmPromotionChoiceId(pub ScopedProposalRef);

#[derive(Clone, Debug, PartialEq)]
pub struct RhythmPromotionSelectionContext {
    pub source: SampleSelection,
    pub selection_revision: u64,
    pub aspect: Option<Aspect>,
    pub signal: SignalLayer,
    /// Whether the selected asset set was empty (unconstrained) or included
    /// the promotion source. This is context for the view, not an acceptance
    /// gate: an Aspect can legitimately have been published by another lens.
    pub source_in_selection: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RhythmPromotionProvenance {
    pub proposal: ScopedProposalRef,
    pub evidence: Vec<ScopedEvidenceRef>,
    pub source: SampleSelection,
    pub pattern_index: usize,
    pub occurrence_index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RhythmPromotionExplanationKind {
    Term { source: String },
    ExactAudio,
}

/// A compact bridge to the explanation pane. The explanation remains an
/// alternative claim; linking it to a constructive choice does not accept it.
#[derive(Clone, Debug, PartialEq)]
pub struct RhythmPromotionExplanationLink {
    pub id: PatternAlternativeId,
    pub claim_id: u64,
    pub rank: usize,
    pub combined_fit: f32,
    pub kind: RhythmPromotionExplanationKind,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RhythmPromotionChoice {
    pub id: RhythmPromotionChoiceId,
    /// Evidence order from the planner, useful for display only.
    pub evidence_rank: usize,
    pub grid: RhythmGridHypothesis,
    pub diagnostics: Vec<RhythmPromotionDiagnostic>,
    pub provenance: RhythmPromotionProvenance,
    pub explanation_links: Vec<RhythmPromotionExplanationLink>,
}

#[derive(Clone, Debug)]
struct StoredChoice {
    view: RhythmPromotionChoice,
    plan: ConstructiveEditPlan,
}

/// Opaque permission to preview exactly one planned alternative. Consumers
/// may render `plan()` off-thread, then call `validate_preview_handle` before
/// presenting or committing work derived from it.
#[derive(Clone, Debug)]
pub struct RhythmPromotionPreviewHandle {
    pub choice: RhythmPromotionChoiceId,
    pub generation: u64,
    pub project_revision: u64,
    pub selection_revision: u64,
    pub provenance: RhythmPromotionProvenance,
    plan: ConstructiveEditPlan,
}

impl RhythmPromotionPreviewHandle {
    pub fn plan(&self) -> &ConstructiveEditPlan {
        &self.plan
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RhythmPromotionApplied {
    pub choice: RhythmPromotionChoiceId,
    pub publication: ConstructivePublication,
    pub revisions: ProjectRevisions,
}

#[derive(Clone, Debug)]
struct AppliedMarker {
    choice: RhythmPromotionChoiceId,
    revision: u64,
    undo_label: String,
}

/// Retained chooser state. Construction intentionally leaves `selected`
/// empty even when the planner has a highest-evidence alternative.
#[derive(Clone, Debug)]
pub struct RhythmPromotionChooser {
    base_revision: u64,
    pattern_index: usize,
    pattern_evidence: f32,
    occurrence_index: usize,
    context: RhythmPromotionSelectionContext,
    diagnostics: Vec<RhythmPromotionDiagnostic>,
    order: Vec<RhythmPromotionChoiceId>,
    choices: BTreeMap<RhythmPromotionChoiceId, StoredChoice>,
    selected: Option<RhythmPromotionChoiceId>,
    preview_generation: u64,
    applied: Option<AppliedMarker>,
}

impl RhythmPromotionChooser {
    /// Plan against the session's coherent project and selection publication.
    /// No alternative is selected or applied by this operation.
    pub fn plan(
        session: &ProjectSession,
        rhythm: &RhythmDeprojection,
        intent: RhythmPromotionIntent,
        explanations: Option<&PatternExplanationSet>,
    ) -> Result<Self, RhythmPromotionChooserError> {
        let controller = session
            .project_controller()
            .ok_or(RhythmPromotionChooserError::NoProject)?;
        let set = controller
            .plan_rhythm_promotion(rhythm, intent)
            .map_err(|error| RhythmPromotionChooserError::Planning(error.to_string()))?;
        Self::from_set(session, intent.source, set, explanations)
    }

    /// Wrap a previously prepared pure plan set while pinning it to the
    /// session publication. This supports background planning without giving
    /// the background worker mutation authority.
    pub fn from_set(
        session: &ProjectSession,
        source: SampleSelection,
        set: RhythmPromotionSet,
        explanations: Option<&PatternExplanationSet>,
    ) -> Result<Self, RhythmPromotionChooserError> {
        let project_revision = session.project_snapshot()?.revisions().aggregate;
        let mut selection = session.selection().selection.clone();
        selection
            .normalize_aspect_signal()
            .map_err(RhythmPromotionChooserError::SelectionAspect)?;
        let source_in_selection =
            selection.assets.is_empty() || selection.assets.contains(&source.asset);
        let signal = selection.selected_signal();
        let context = RhythmPromotionSelectionContext {
            source,
            selection_revision: session.selection().revision,
            aspect: selection.aspect,
            signal,
            source_in_selection,
        };

        let mut order = Vec::with_capacity(set.alternatives.len());
        let mut choices = BTreeMap::new();
        for (evidence_rank, alternative) in set.alternatives.into_iter().enumerate() {
            if alternative.plan.base_revision != project_revision {
                return Err(RhythmPromotionChooserError::StaleProjectRevision {
                    expected: alternative.plan.base_revision,
                    actual: project_revision,
                });
            }
            let (proposal, evidence) = deprojection_provenance(&alternative.plan)?;
            let id = RhythmPromotionChoiceId(proposal);
            let provenance = RhythmPromotionProvenance {
                proposal,
                evidence,
                source,
                pattern_index: set.pattern_index,
                occurrence_index: set.occurrence_index,
            };
            let view = RhythmPromotionChoice {
                id,
                evidence_rank,
                grid: alternative.grid,
                diagnostics: alternative.diagnostics,
                provenance,
                explanation_links: explanation_links(
                    explanations,
                    set.pattern_index,
                    alternative.grid.beat_phase_index,
                    alternative.grid.tempo_rank,
                ),
            };
            if choices
                .insert(
                    id,
                    StoredChoice {
                        view,
                        plan: alternative.plan,
                    },
                )
                .is_some()
            {
                return Err(RhythmPromotionChooserError::DuplicateChoice(id));
            }
            order.push(id);
        }
        if choices.is_empty() {
            return Err(RhythmPromotionChooserError::NoAlternatives);
        }
        Ok(Self {
            base_revision: project_revision,
            pattern_index: set.pattern_index,
            pattern_evidence: set.pattern_evidence,
            occurrence_index: set.occurrence_index,
            context,
            diagnostics: set.diagnostics,
            order,
            choices,
            selected: None,
            preview_generation: 0,
            applied: None,
        })
    }

    pub const fn base_revision(&self) -> u64 {
        self.base_revision
    }

    pub const fn pattern_index(&self) -> usize {
        self.pattern_index
    }

    pub const fn pattern_evidence(&self) -> f32 {
        self.pattern_evidence
    }

    pub const fn occurrence_index(&self) -> usize {
        self.occurrence_index
    }

    pub fn context(&self) -> &RhythmPromotionSelectionContext {
        &self.context
    }

    pub fn diagnostics(&self) -> &[RhythmPromotionDiagnostic] {
        &self.diagnostics
    }

    pub fn choices(&self) -> impl ExactSizeIterator<Item = &RhythmPromotionChoice> {
        self.order.iter().map(|id| {
            &self
                .choices
                .get(id)
                .expect("choice order is canonical")
                .view
        })
    }

    pub fn selected(&self) -> Option<&RhythmPromotionChoice> {
        self.selected
            .and_then(|id| self.choices.get(&id).map(|choice| &choice.view))
    }

    pub fn applied_choice(&self) -> Option<RhythmPromotionChoiceId> {
        self.applied.as_ref().map(|marker| marker.choice)
    }

    pub fn select(
        &mut self,
        id: RhythmPromotionChoiceId,
    ) -> Result<&RhythmPromotionChoice, RhythmPromotionChooserError> {
        if !self.choices.contains_key(&id) {
            return Err(RhythmPromotionChooserError::UnknownChoice(id));
        }
        self.selected = Some(id);
        Ok(&self
            .choices
            .get(&id)
            .expect("choice existence checked")
            .view)
    }

    pub fn clear_selection(&mut self) {
        self.selected = None;
    }

    pub fn preview_selected(
        &mut self,
        session: &ProjectSession,
    ) -> Result<RhythmPromotionPreviewHandle, RhythmPromotionChooserError> {
        self.validate_session(session)?;
        let id = self
            .selected
            .ok_or(RhythmPromotionChooserError::NoSelection)?;
        let stored = self
            .choices
            .get(&id)
            .ok_or(RhythmPromotionChooserError::UnknownChoice(id))?;
        self.preview_generation = self.preview_generation.wrapping_add(1).max(1);
        Ok(RhythmPromotionPreviewHandle {
            choice: id,
            generation: self.preview_generation,
            project_revision: self.base_revision,
            selection_revision: self.context.selection_revision,
            provenance: stored.view.provenance.clone(),
            plan: stored.plan.clone(),
        })
    }

    pub fn validate_preview_handle(
        &self,
        session: &ProjectSession,
        handle: &RhythmPromotionPreviewHandle,
    ) -> Result<(), RhythmPromotionChooserError> {
        self.validate_session(session)?;
        if handle.generation != self.preview_generation {
            return Err(RhythmPromotionChooserError::StalePreviewGeneration {
                expected: self.preview_generation,
                actual: handle.generation,
            });
        }
        if self.selected != Some(handle.choice)
            || handle.project_revision != self.base_revision
            || handle.selection_revision != self.context.selection_revision
        {
            return Err(RhythmPromotionChooserError::StalePreviewChoice(
                handle.choice,
            ));
        }
        Ok(())
    }

    /// Apply exactly the explicitly selected alternative. The plan is not
    /// regenerated from UI fields, and ProjectSession owns publication/history.
    pub fn apply_selected(
        &mut self,
        session: &mut ProjectSession,
    ) -> Result<RhythmPromotionApplied, RhythmPromotionChooserError> {
        self.validate_session(session)?;
        let id = self
            .selected
            .ok_or(RhythmPromotionChooserError::NoSelection)?;
        let stored = self
            .choices
            .get(&id)
            .ok_or(RhythmPromotionChooserError::UnknownChoice(id))?;
        let undo_label = stored.plan.label.clone();
        let outcome = session
            .execute_constructive_plan(stored.plan.clone())
            .map_err(RhythmPromotionChooserError::Session)?;
        let revisions = session.project_snapshot()?.revisions();
        self.applied = Some(AppliedMarker {
            choice: id,
            revision: revisions.aggregate,
            undo_label,
        });
        Ok(RhythmPromotionApplied {
            choice: id,
            publication: outcome.publication,
            revisions,
        })
    }

    /// Undo only this chooser's still-topmost application. Operational ID
    /// watermarks remain monotonic under the aggregate history doctrine.
    pub fn undo_applied(
        &mut self,
        session: &mut ProjectSession,
    ) -> Result<ProjectRevisions, RhythmPromotionChooserError> {
        let marker = self
            .applied
            .clone()
            .ok_or(RhythmPromotionChooserError::NothingApplied)?;
        let actual = session.project_snapshot()?.revisions().aggregate;
        if actual != marker.revision {
            return Err(RhythmPromotionChooserError::StaleAppliedRevision {
                expected: marker.revision,
                actual,
            });
        }
        let history = session.history_status()?;
        if history.undo_label.as_deref() != Some(marker.undo_label.as_str()) {
            return Err(RhythmPromotionChooserError::AppliedHistoryMoved);
        }
        let revisions = session
            .undo()?
            .ok_or(RhythmPromotionChooserError::AppliedHistoryMoved)?;
        self.applied = None;
        Ok(revisions)
    }

    fn validate_session(
        &self,
        session: &ProjectSession,
    ) -> Result<(), RhythmPromotionChooserError> {
        let actual_project = session.project_snapshot()?.revisions().aggregate;
        if actual_project != self.base_revision {
            return Err(RhythmPromotionChooserError::StaleProjectRevision {
                expected: self.base_revision,
                actual: actual_project,
            });
        }
        let actual_selection = session.selection().revision;
        if actual_selection != self.context.selection_revision {
            return Err(RhythmPromotionChooserError::StaleSelectionRevision {
                expected: self.context.selection_revision,
                actual: actual_selection,
            });
        }
        Ok(())
    }
}

fn deprojection_provenance(
    plan: &ConstructiveEditPlan,
) -> Result<(ScopedProposalRef, Vec<ScopedEvidenceRef>), RhythmPromotionChooserError> {
    let mut found = None;
    for cause in &plan.causes {
        if let ConstructiveCause::Deprojection { proposal, evidence } = cause {
            if found.is_some() {
                return Err(RhythmPromotionChooserError::AmbiguousProvenance);
            }
            let mut evidence = evidence.clone();
            evidence.sort();
            evidence.dedup();
            found = Some((*proposal, evidence));
        }
    }
    found.ok_or(RhythmPromotionChooserError::MissingProvenance)
}

fn explanation_links(
    explanations: Option<&PatternExplanationSet>,
    pattern_index: usize,
    phase_index: usize,
    tempo_rank: usize,
) -> Vec<RhythmPromotionExplanationLink> {
    let Some(explanations) = explanations else {
        return Vec::new();
    };
    explanations
        .alternatives
        .iter()
        .filter(|explanation| {
            let evidence = &explanation.evidence;
            evidence.contains(&RhythmEvidenceRef::Pattern(pattern_index))
                && (!evidence
                    .iter()
                    .any(|item| matches!(item, RhythmEvidenceRef::BeatPhase(_)))
                    || evidence.contains(&RhythmEvidenceRef::BeatPhase(phase_index)))
                && (!evidence
                    .iter()
                    .any(|item| matches!(item, RhythmEvidenceRef::Tempo(_)))
                    || evidence.contains(&RhythmEvidenceRef::Tempo(tempo_rank)))
        })
        .map(|explanation| RhythmPromotionExplanationLink {
            id: explanation.id,
            claim_id: explanation.claim_id(),
            rank: explanation.rank,
            combined_fit: explanation.fit.combined_fit,
            kind: match &explanation.representation {
                PatternExplanationRepresentation::Term(term) => {
                    RhythmPromotionExplanationKind::Term {
                        source: term.source.clone(),
                    }
                }
                PatternExplanationRepresentation::ExactAudio(_) => {
                    RhythmPromotionExplanationKind::ExactAudio
                }
            },
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub enum RhythmPromotionChooserError {
    NoProject,
    NoAlternatives,
    NoSelection,
    NothingApplied,
    UnknownChoice(RhythmPromotionChoiceId),
    DuplicateChoice(RhythmPromotionChoiceId),
    MissingProvenance,
    AmbiguousProvenance,
    Planning(String),
    SelectionAspect(SelectionAspectError),
    StaleProjectRevision { expected: u64, actual: u64 },
    StaleSelectionRevision { expected: u64, actual: u64 },
    StalePreviewGeneration { expected: u64, actual: u64 },
    StalePreviewChoice(RhythmPromotionChoiceId),
    StaleAppliedRevision { expected: u64, actual: u64 },
    AppliedHistoryMoved,
    Session(ProjectSessionError),
}

impl fmt::Display for RhythmPromotionChooserError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoProject => formatter.write_str("the project session has no live project"),
            Self::NoAlternatives => formatter.write_str("rhythm promotion has no alternatives"),
            Self::NoSelection => formatter.write_str("choose a rhythm promotion alternative first"),
            Self::NothingApplied => formatter.write_str("this chooser has no applied promotion"),
            Self::UnknownChoice(id) => write!(formatter, "unknown rhythm promotion choice {id:?}"),
            Self::DuplicateChoice(id) => write!(formatter, "duplicate rhythm promotion choice {id:?}"),
            Self::MissingProvenance => formatter.write_str("promotion plan has no deprojection provenance"),
            Self::AmbiguousProvenance => formatter.write_str("promotion plan has multiple deprojection causes"),
            Self::Planning(message) => write!(formatter, "rhythm promotion planning: {message}"),
            Self::SelectionAspect(error) => write!(formatter, "selected aspect: {error}"),
            Self::StaleProjectRevision { expected, actual } => write!(formatter, "rhythm chooser project revision is stale: expected {expected}, actual {actual}"),
            Self::StaleSelectionRevision { expected, actual } => write!(formatter, "rhythm chooser selection revision is stale: expected {expected}, actual {actual}"),
            Self::StalePreviewGeneration { expected, actual } => write!(formatter, "rhythm preview generation is stale: expected {expected}, actual {actual}"),
            Self::StalePreviewChoice(id) => write!(formatter, "rhythm preview no longer denotes selected choice {id:?}"),
            Self::StaleAppliedRevision { expected, actual } => write!(formatter, "applied rhythm promotion is no longer at history head: expected {expected}, actual {actual}"),
            Self::AppliedHistoryMoved => formatter.write_str("applied rhythm promotion is no longer the top undo entry"),
            Self::Session(error) => error.fmt(formatter),
        }
    }
}

impl Error for RhythmPromotionChooserError {}

impl From<ProjectSessionError> for RhythmPromotionChooserError {
    fn from(error: ProjectSessionError) -> Self {
        Self::Session(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use crate::aspect::{AnalysisRef, FrameSpan};
    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        AssetRegistry, ContentFingerprint, DecodedAudioMetadata, SampleFrames,
    };
    use crate::audio::AudioFormat;
    use crate::daw_render::PcmAsset;
    use crate::live_project::{LiveProject, SourceMaterialMetadata};
    use crate::project_selection::ProjectSelection;
    use crate::rhythm::{
        AnalysisStatus, BeatPhaseHypothesis, EventFamilyHypothesis, HitObservation,
        MedoidSampleReference, PatternHypothesis, PatternOccurrence, SampleSpan, TempoHypothesis,
        TempoRelation,
    };
    use crate::rhythm_explanation::{explain_rhythm, ExplainBudget};

    const RATE: u32 = 1_000;
    const FRAMES: usize = 600;

    fn installed_session() -> (ProjectSession, crate::assets::AssetId) {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/audio/electronic-beat.wav").unwrap()),
            None,
        )
        .unwrap();
        let mut registry = AssetRegistry::new();
        let asset = registry
            .register(AssetRegistration {
                name: "electronic beat".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: RATE,
                    channels: 1,
                    frame_count: SampleFrames(FRAMES as u64),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32le".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"chooser-electronic-beat"),
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
        let mut samples = vec![0.0_f32; FRAMES];
        samples[10..16].copy_from_slice(&[0.31, -0.47, 0.83, 0.22, -0.15, 0.04]);
        samples[135..141].copy_from_slice(&[0.12, 0.67, -0.24, 0.51, 0.09, -0.03]);
        let pcm = PcmAsset::new(AudioFormat::new(RATE, 1).unwrap(), Arc::from(samples)).unwrap();
        let live = LiveProject::from_source_material(
            SourceMaterialMetadata::new("Electronic", "Beat"),
            registry,
            asset,
            pcm,
        )
        .unwrap();
        let mut session =
            ProjectSession::new(crate::project_session::ProjectSessionId(91)).unwrap();
        session.install(live, None).unwrap();
        session.replace_selection(ProjectSelection {
            time: Some(FrameSpan::new(0, FRAMES as i64).unwrap()),
            assets: BTreeSet::from([asset]),
            aspect: Some(Aspect::Intersect(vec![
                Aspect::Time(FrameSpan::new(0, FRAMES as i64).unwrap()),
                Aspect::Family {
                    analysis: AnalysisRef {
                        source: 41,
                        recipe: 12,
                    },
                    id: 7,
                },
            ])),
            signal: Some(SignalLayer::Source),
            ..ProjectSelection::default()
        });
        (session, asset)
    }

    fn hit(onset: usize, family: usize, strength: f32, span: SampleSpan) -> HitObservation {
        HitObservation {
            span,
            onset_sample: onset,
            novelty_peak_sample: onset,
            peak_sample: onset,
            onset_seconds: onset as f64 / RATE as f64,
            duration_seconds: span.len() as f32 / RATE as f32,
            novelty_strength: strength,
            threshold_excess: strength * 0.8,
            family: Some(family),
            family_similarity: 0.9,
            ..HitObservation::default()
        }
    }

    fn electronic_beat() -> RhythmDeprojection {
        RhythmDeprojection {
            status: AnalysisStatus::Complete,
            sample_rate: RATE,
            sample_frames: FRAMES,
            analysis_hop: 8,
            novelty: vec![0.0; FRAMES / 8],
            band_novelty: vec![[0.0; 3]; FRAMES / 8],
            adaptive_threshold: vec![0.0; FRAMES / 8],
            hits: vec![
                hit(10, 7, 1.0, SampleSpan { start: 10, end: 16 }),
                hit(
                    135,
                    11,
                    0.8,
                    SampleSpan {
                        start: 135,
                        end: 141,
                    },
                ),
                hit(
                    260,
                    7,
                    0.9,
                    SampleSpan {
                        start: 260,
                        end: 266,
                    },
                ),
                hit(
                    385,
                    11,
                    0.7,
                    SampleSpan {
                        start: 385,
                        end: 391,
                    },
                ),
            ],
            tempo_hypotheses: vec![
                TempoHypothesis {
                    rank: 0,
                    bpm: 120.0,
                    period_frames: 500.0,
                    periodicity: 0.9,
                    evidence: 0.86,
                    relation: TempoRelation::Independent,
                },
                TempoHypothesis {
                    rank: 1,
                    bpm: 60.0,
                    period_frames: 1_000.0,
                    periodicity: 0.6,
                    evidence: 0.55,
                    relation: TempoRelation::HalfTimeOf(0),
                },
            ],
            beat_phase_hypotheses: vec![
                BeatPhaseHypothesis {
                    tempo_rank: 0,
                    bpm: 120.0,
                    phase_seconds: 0.01,
                    score: 0.9,
                    beat_samples: vec![10, 510],
                },
                BeatPhaseHypothesis {
                    tempo_rank: 1,
                    bpm: 60.0,
                    phase_seconds: 0.01,
                    score: 0.62,
                    beat_samples: vec![10],
                },
            ],
            event_families: vec![
                EventFamilyHypothesis {
                    id: 7,
                    event_indices: vec![0, 2],
                    medoid: MedoidSampleReference {
                        event_index: 0,
                        excerpt: SampleSpan { start: 10, end: 16 },
                    },
                    mean_medoid_similarity: 0.92,
                    minimum_medoid_similarity: 0.87,
                    evidence: 0.9,
                },
                EventFamilyHypothesis {
                    id: 11,
                    event_indices: vec![1, 3],
                    medoid: MedoidSampleReference {
                        event_index: 1,
                        excerpt: SampleSpan {
                            start: 135,
                            end: 141,
                        },
                    },
                    mean_medoid_similarity: 0.88,
                    minimum_medoid_similarity: 0.8,
                    evidence: 0.83,
                },
            ],
            patterns: vec![PatternHypothesis {
                family_sequence: vec![7, 11],
                step_offsets: vec![0, 1],
                occurrences: vec![
                    PatternOccurrence {
                        event_index: 0,
                        start_sample: 10,
                        beat_position: 0.0,
                    },
                    PatternOccurrence {
                        event_index: 2,
                        start_sample: 260,
                        beat_position: 0.5,
                    },
                ],
                evidence: 0.88,
            }],
            ..RhythmDeprojection::default()
        }
    }

    fn intent(asset: crate::assets::AssetId) -> RhythmPromotionIntent {
        RhythmPromotionIntent {
            source: SampleSelection::whole_asset(asset),
            pattern_index: 0,
            target_bus: None,
        }
    }

    #[test]
    fn alternatives_are_inert_and_preview_is_aspect_and_revision_pinned() {
        let (session, asset) = installed_session();
        let rhythm = electronic_beat();
        let explanations = explain_rhythm(&rhythm, &[], ExplainBudget::default()).unwrap();
        let mut chooser =
            RhythmPromotionChooser::plan(&session, &rhythm, intent(asset), Some(&explanations))
                .unwrap();

        assert_eq!(chooser.choices().len(), 2);
        assert!(chooser.selected().is_none());
        assert!(chooser.context().source_in_selection);
        assert!(matches!(
            chooser.context().aspect,
            Some(Aspect::Intersect(_))
        ));
        assert_eq!(chooser.context().signal, SignalLayer::Source);
        assert!(chooser.choices().any(|choice| choice
            .explanation_links
            .iter()
            .any(|link| matches!(link.kind, RhythmPromotionExplanationKind::Term { .. }))));

        let ids = chooser
            .choices()
            .map(|choice| choice.id)
            .collect::<Vec<_>>();
        chooser.select(ids[0]).unwrap();
        let first = chooser.preview_selected(&session).unwrap();
        let second = chooser.preview_selected(&session).unwrap();
        assert_eq!(second.plan().base_revision, chooser.base_revision());
        assert!(!second.provenance.evidence.is_empty());
        assert!(matches!(
            chooser.validate_preview_handle(&session, &first),
            Err(RhythmPromotionChooserError::StalePreviewGeneration { .. })
        ));
        chooser.validate_preview_handle(&session, &second).unwrap();
    }

    #[test]
    fn selected_plan_applies_through_session_and_is_one_undo_step() {
        let (mut session, asset) = installed_session();
        let rhythm = electronic_beat();
        let before_revision = session.project_snapshot().unwrap().revisions().aggregate;
        let before = {
            let state = session.project_snapshot().unwrap().project.state();
            (
                state.domains.sample_kits.kits.len(),
                state.domains.sequencer.patterns().patterns().count(),
                state.domains.arrangement.clips.len(),
            )
        };
        let mut chooser =
            RhythmPromotionChooser::plan(&session, &rhythm, intent(asset), None).unwrap();
        let id = chooser.choices().next().unwrap().id;
        chooser.select(id).unwrap();
        let applied = chooser.apply_selected(&mut session).unwrap();
        assert_eq!(applied.choice, id);
        assert!(applied.publication.pattern.is_some());
        assert!(applied.revisions.aggregate > before_revision);
        let state = session.project_snapshot().unwrap().project.state();
        assert_eq!(state.domains.sample_kits.kits.len(), before.0 + 1);
        assert_eq!(
            state.domains.sequencer.patterns().patterns().count(),
            before.1 + 1
        );
        assert_eq!(state.domains.arrangement.clips.len(), before.2 + 1);
        assert!(session.history_status().unwrap().can_undo);

        let undone = chooser.undo_applied(&mut session).unwrap();
        assert!(undone.aggregate > applied.revisions.aggregate);
        let state = session.project_snapshot().unwrap().project.state();
        assert_eq!(state.domains.sample_kits.kits.len(), before.0);
        assert_eq!(
            state.domains.sequencer.patterns().patterns().count(),
            before.1
        );
        assert_eq!(state.domains.arrangement.clips.len(), before.2);
    }

    #[test]
    fn project_and_selection_changes_stale_the_original_choice() {
        let (mut session, asset) = installed_session();
        let rhythm = electronic_beat();
        let mut old = RhythmPromotionChooser::plan(&session, &rhythm, intent(asset), None).unwrap();
        let old_id = old.choices().next().unwrap().id;
        old.select(old_id).unwrap();

        let mut winner =
            RhythmPromotionChooser::plan(&session, &rhythm, intent(asset), None).unwrap();
        let winner_id = winner.choices().nth(1).unwrap().id;
        winner.select(winner_id).unwrap();
        winner.apply_selected(&mut session).unwrap();
        assert!(matches!(
            old.apply_selected(&mut session),
            Err(RhythmPromotionChooserError::StaleProjectRevision { .. })
        ));

        let (mut other_session, other_asset) = installed_session();
        let mut selection_stale =
            RhythmPromotionChooser::plan(&other_session, &rhythm, intent(other_asset), None)
                .unwrap();
        let id = selection_stale.choices().next().unwrap().id;
        selection_stale.select(id).unwrap();
        other_session.replace_selection(ProjectSelection {
            aspect: Some(Aspect::Time(FrameSpan::new(50, 300).unwrap())),
            ..ProjectSelection::default()
        });
        assert!(matches!(
            selection_stale.apply_selected(&mut other_session),
            Err(RhythmPromotionChooserError::StaleSelectionRevision { .. })
        ));
    }
}
