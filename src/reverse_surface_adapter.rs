//! Session-owned deprojection objects projected into reverse-surface documents,
//! and the thin execution adapters for explicit Apply/Keep consequences.
//!
//! The deprojection workspace remains the authority for artifacts, Findings,
//! explanations, comparisons, and freshness. Hydration never reruns analysis
//! or invents an instrument identity. Apply compiles one existing deprojection
//! promotion envelope; Keep confirms current analysis retention and returns a
//! reveal recommendation. Unknown keys are a typed refusal. Invalidated
//! Findings remain inspectable without an executable control.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::artifact_catalog::{ArtifactCatalog, ArtifactId};
use crate::artifact_promotion_bridge::{
    plan_artifact_promotion_comparison, ArtifactPromotionBridgeError,
    ArtifactPromotionComparisonResult,
};
use crate::comparison::ComparisonId;
use crate::daw_project::ProjectRevisions;
use crate::daw_render::RenderCancellation;
use crate::deprojection_execution::promotion::CreatedObject;
use crate::explanation::ExplanationId;
use crate::explorer_model::ExplorerSemanticCollections;
use crate::interpretation::InterpretationStore;
use crate::project_controller::{
    FindingRef, FindingScope, InstrumentRef, ObjectRef, PadRef, RevealIntent, RevealRecommendation,
    RevealRequest,
};
use crate::project_session::deprojection_workspace_bridge::{
    AnalysisEvidenceDocumentSummary, AnalysisEvidenceKind, DeprojectionCandidateDocumentSummary,
    DeprojectionCandidateFreshness, DeprojectionWorkspaceBridgeError, DeprojectionWorkspaceTarget,
};
use crate::project_session::{ProjectSession, ProjectSessionError};
use crate::reverse_surface::{
    consequence_host_binding, ComparisonSurfaceDocument, ConsequenceHostBinding,
    FindingSurfaceDocument, ReverseSurfaceDocument, ReverseSurfaceError, ReverseSurfaceStore,
    SurfaceEditConsequence, SurfaceEvidence, CONSEQUENCE_APPLY_CONSTRUCTION,
    CONSEQUENCE_KEEP_FINDING,
};

/// Build the complete semantic surface catalog for one project session.
///
/// Callers should pass the summaries returned by
/// `list_deprojection_workspace_candidates`, not a pane-local selection. That
/// keeps already-open surfaces inspectable after a newer analysis invalidates
/// their executable cohort.
pub fn project_reverse_surface_documents<'a>(
    summaries: impl IntoIterator<Item = &'a DeprojectionCandidateDocumentSummary>,
    evidence_summaries: impl IntoIterator<Item = &'a AnalysisEvidenceDocumentSummary>,
    artifacts: &ArtifactCatalog,
    interpretations: &InterpretationStore,
) -> Result<Vec<ReverseSurfaceDocument>, ReverseSurfaceAdapterError> {
    let mut documents = BTreeMap::<String, ReverseSurfaceDocument>::new();
    for summary in summaries {
        let descriptor = artifacts.descriptor(summary.artifact).cloned().ok_or(
            ReverseSurfaceAdapterError::MissingArtifact(summary.artifact),
        )?;
        if summary.finding.scope != FindingScope::Artifact(descriptor.id) {
            return Err(ReverseSurfaceAdapterError::FindingArtifactMismatch {
                finding: summary.finding,
                artifact: descriptor.id,
            });
        }

        let explanation = interpretations
            .explanation(summary.explanation)
            .cloned()
            .ok_or(ReverseSurfaceAdapterError::MissingExplanation(
                summary.explanation,
            ))?;
        let comparison = interpretations
            .comparison(summary.comparison)
            .cloned()
            .ok_or(ReverseSurfaceAdapterError::MissingComparison(
                summary.comparison,
            ))?;
        if comparison.explanation != summary.explanation {
            return Err(ReverseSurfaceAdapterError::ComparisonExplanationMismatch {
                comparison: comparison.id,
                expected: summary.explanation,
                actual: comparison.explanation,
            });
        }
        if !explanation.scope.artifacts().contains(&descriptor.id)
            && !explanation.evidence.contains(
                &crate::explanation::ExplanationEvidenceRef::Artifact(descriptor.id),
            )
        {
            return Err(ReverseSurfaceAdapterError::ExplanationArtifactMismatch {
                explanation: explanation.id,
                artifact: descriptor.id,
            });
        }

        let comparison_document = ComparisonSurfaceDocument {
            definition: comparison,
            observation: interpretations.observation(summary.comparison).cloned(),
            coverage: None,
        };
        let finding_object = ObjectRef::Finding(summary.finding);
        let explanation_object = ObjectRef::Explanation(summary.explanation);
        let comparison_object = ObjectRef::Comparison(summary.comparison);
        let current = summary.freshness == DeprojectionCandidateFreshness::Current;
        let finding = ReverseSurfaceDocument::finding(
            FindingSurfaceDocument {
                finding: summary.finding,
                label: summary.label.clone(),
                artifact: Some(descriptor.clone()),
                extent: Some(descriptor.extent),
                statements: vec![
                    format!(
                        "{:?} evidence over project frames {}..{}",
                        descriptor.kind, descriptor.extent.start, descriptor.extent.end
                    ),
                    if current {
                        "The candidate is pinned to the current project, selection, and artifact cohort."
                            .into()
                    } else {
                        "A later project, selection, or artifact publication invalidated executable promotion; the evidence remains inspectable."
                            .into()
                    },
                ],
            },
            vec![
                SurfaceEvidence {
                    key: "artifact".into(),
                    label: format!("{:?} analysis artifact", descriptor.kind),
                    object: None,
                    extent: Some(descriptor.extent),
                    derivation: Vec::new(),
                },
                SurfaceEvidence {
                    key: "explanation".into(),
                    label: "Construction recipe".into(),
                    object: Some(explanation_object.clone()),
                    extent: Some(descriptor.extent),
                    derivation: vec![finding_object.clone()],
                },
                SurfaceEvidence {
                    key: "comparison".into(),
                    label: "Source / construction / residual experiment".into(),
                    object: Some(comparison_object.clone()),
                    extent: Some(comparison_document.definition.source.project_span),
                    derivation: vec![explanation_object.clone()],
                },
            ],
            current
                .then(|| {
                    vec![
                        SurfaceEditConsequence::apply_construction(
                            vec![comparison_object.clone()],
                            vec![finding_object.clone(), explanation_object.clone()],
                        ),
                        SurfaceEditConsequence::keep_finding(vec![
                            finding_object.clone(),
                            explanation_object.clone(),
                        ]),
                    ]
                })
                .unwrap_or_default(),
            vec![comparison_document.clone()],
        )?;

        insert_document(&mut documents, finding)?;
        let extra_explanation_consequences = current
            .then(|| {
                vec![
                    SurfaceEditConsequence::apply_construction(
                        vec![comparison_object.clone()],
                        vec![finding_object.clone(), explanation_object.clone()],
                    ),
                    SurfaceEditConsequence::keep_finding(vec![finding_object, explanation_object]),
                ]
            })
            .unwrap_or_default();
        insert_document(
            &mut documents,
            ReverseSurfaceDocument::explanation_with_consequences(
                explanation,
                vec![comparison_document.clone()],
                extra_explanation_consequences,
            )?,
        )?;
        insert_document(
            &mut documents,
            ReverseSurfaceDocument::from_comparison(comparison_document)?,
        )?;
    }
    for summary in evidence_summaries {
        let descriptor = artifacts.descriptor(summary.artifact).cloned().ok_or(
            ReverseSurfaceAdapterError::MissingArtifact(summary.artifact),
        )?;
        if summary.finding.scope != FindingScope::Artifact(descriptor.id) {
            return Err(ReverseSurfaceAdapterError::FindingArtifactMismatch {
                finding: summary.finding,
                artifact: descriptor.id,
            });
        }
        let current = summary.freshness == DeprojectionCandidateFreshness::Current;
        let (subject, epistemic_statement) = match summary.kind {
            AnalysisEvidenceKind::HpssComponent(
                crate::explanation::HpssComponentKind::Harmonic,
            ) => (
                "tonally sustained",
                "HPSS estimates persistence across time and breadth across frequency; it does not identify an instrument, performer, or causal source.",
            ),
            AnalysisEvidenceKind::HpssComponent(
                crate::explanation::HpssComponentKind::Percussive,
            ) => (
                "transient",
                "HPSS estimates persistence across time and breadth across frequency; it does not identify an instrument, performer, or causal source.",
            ),
            AnalysisEvidenceKind::LoomSequence => (
                "editable recurrence sequence",
                "Loom groups aligned recurring excerpts and renders their event sequence; clusters remain anonymous mixed-signal hypotheses.",
            ),
            AnalysisEvidenceKind::LoomTemplate { .. } => (
                "anonymous recurrence template",
                "This phase-bearing template is aligned from repeated mixed-signal excerpts; overlapping voices and effects may remain in it.",
            ),
            AnalysisEvidenceKind::ComponentMagnitude { .. } => (
                "recurring mixed-signal magnitude factor",
                "NMF factors recurring mixed-audio magnitude shapes. Phase was not retained; this is not an isolated source, stem, or instrument identity.",
            ),
        };
        let finding_object = ObjectRef::Finding(summary.finding);
        let finding = ReverseSurfaceDocument::finding(
            FindingSurfaceDocument {
                finding: summary.finding,
                label: summary.label.clone(),
                artifact: Some(descriptor.clone()),
                extent: Some(descriptor.extent),
                statements: vec![
                    format!(
                        "Phase-bearing {subject} evidence over project frames {}..{}.",
                        descriptor.extent.start, descriptor.extent.end
                    ),
                    epistemic_statement.into(),
                    if current {
                        "The evidence is pinned to the current project, selection, and artifact cohort."
                            .into()
                    } else {
                        "A later project, selection, or analysis publication invalidated executable actions; the evidence remains inspectable."
                            .into()
                    },
                ],
            },
            vec![SurfaceEvidence {
                key: "artifact".into(),
                label: format!("{subject} signal"),
                object: None,
                extent: Some(descriptor.extent),
                derivation: vec![finding_object.clone()],
            }],
            current
                .then(|| vec![SurfaceEditConsequence::keep_finding(vec![finding_object])])
                .unwrap_or_default(),
            Vec::new(),
        )?;
        insert_document(&mut documents, finding)?;
    }
    Ok(documents.into_values().collect())
}

/// Investigate/Readings identities from reverse-surface documents plus any
/// interpretation recipes that do not yet have a surface document.
pub fn explorer_semantic_collections(
    store: &ReverseSurfaceStore,
    interpretations: &InterpretationStore,
) -> ExplorerSemanticCollections {
    ExplorerSemanticCollections::from_reverse_documents(store.documents())
        .include_interpretations(interpretations)
}

fn insert_document(
    documents: &mut BTreeMap<String, ReverseSurfaceDocument>,
    document: ReverseSurfaceDocument,
) -> Result<(), ReverseSurfaceAdapterError> {
    let key = document.object.address();
    if let Some(existing) = documents.get(&key) {
        if existing != &document {
            return Err(ReverseSurfaceAdapterError::Surface(
                ReverseSurfaceError::DocumentConflict(document.object),
            ));
        }
        return Ok(());
    }
    documents.insert(key, document);
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReverseSurfaceEditKind {
    Applied,
    Kept,
}

/// One command's durable identities plus the reveal the host should issue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReverseSurfaceEditOutcome {
    pub kind: ReverseSurfaceEditKind,
    pub primary: ObjectRef,
    pub related: Vec<ObjectRef>,
    pub created: Vec<ObjectRef>,
    pub revision: u64,
    pub reveal: RevealRecommendation,
}

/// Dispatch a reverse-surface consequence key through its session core.
/// Apply and Keep execute; every other key is a typed visible refusal.
pub fn execute_reverse_surface_consequence(
    session: &mut ProjectSession,
    document: &ObjectRef,
    key: &str,
    requested_at: Option<ProjectRevisions>,
    cancellation: &RenderCancellation,
) -> Result<ReverseSurfaceEditOutcome, ReverseSurfaceAdapterError> {
    match consequence_host_binding(key) {
        ConsequenceHostBinding::Unavailable(reason) => {
            return Err(ReverseSurfaceAdapterError::ConsequenceUnavailable {
                key: key.into(),
                reason,
            });
        }
        ConsequenceHostBinding::Executable => {}
    }
    match key {
        CONSEQUENCE_APPLY_CONSTRUCTION => {
            apply_reverse_construction(session, document, requested_at, cancellation)
        }
        CONSEQUENCE_KEEP_FINDING => keep_reverse_finding(session, document, requested_at),
        _ => Err(ReverseSurfaceAdapterError::ConsequenceUnavailable {
            key: key.into(),
            reason: "This edit consequence has no executable host adapter.",
        }),
    }
}

/// Compile and commit one deprojection promotion for a current reverse object.
pub fn apply_reverse_construction(
    session: &mut ProjectSession,
    document: &ObjectRef,
    requested_at: Option<ProjectRevisions>,
    cancellation: &RenderCancellation,
) -> Result<ReverseSurfaceEditOutcome, ReverseSurfaceAdapterError> {
    refuse_stale_receipt(session, requested_at)?;
    let resolved = session
        .resolve_deprojection_workspace_request(DeprojectionWorkspaceTarget::Object(
            document.clone(),
        ))
        .map_err(map_workspace_apply_error)?;
    let plan = plan_artifact_promotion_comparison(
        session,
        session.deprojection_workspace_artifacts(),
        resolved.request,
        cancellation,
    )?;
    let result = plan.execute(session, cancellation)?;
    Ok(outcome_from_promotion(&result))
}

/// Confirm a current Finding (or its Explanation/Comparison) is still retained.
pub fn keep_reverse_finding(
    session: &ProjectSession,
    document: &ObjectRef,
    requested_at: Option<ProjectRevisions>,
) -> Result<ReverseSurfaceEditOutcome, ReverseSurfaceAdapterError> {
    refuse_stale_receipt(session, requested_at)?;
    let retained = retained_finding(session, document)?;
    let primary = ObjectRef::Finding(retained.finding);
    let mut related = Vec::new();
    if let Some(explanation) = retained.explanation {
        related.push(ObjectRef::Explanation(explanation));
    }
    if let Some(comparison) = retained.comparison {
        related.push(ObjectRef::Comparison(comparison));
    }
    let reveal = RevealRecommendation {
        request: RevealRequest::new(primary.clone(), RevealIntent::ActivateExisting)
            .at_revision(retained.retention_revision)
            .with_related(related.clone()),
        diagnostics: Vec::new(),
    };
    Ok(ReverseSurfaceEditOutcome {
        kind: ReverseSurfaceEditKind::Kept,
        primary,
        related,
        created: Vec::new(),
        revision: retained.retention_revision,
        reveal,
    })
}

fn refuse_stale_receipt(
    session: &ProjectSession,
    requested_at: Option<ProjectRevisions>,
) -> Result<(), ReverseSurfaceAdapterError> {
    let Some(requested) = requested_at else {
        return Ok(());
    };
    let current = session.project_snapshot()?.revisions();
    if requested != current {
        return Err(ReverseSurfaceAdapterError::StaleProjectReceipt { requested, current });
    }
    Ok(())
}

struct RetainedFinding {
    finding: FindingRef,
    explanation: Option<ExplanationId>,
    comparison: Option<ComparisonId>,
    retention_revision: u64,
}

fn retained_finding(
    session: &ProjectSession,
    document: &ObjectRef,
) -> Result<RetainedFinding, ReverseSurfaceAdapterError> {
    let mut saw_invalidated = false;
    for summary in session
        .list_deprojection_workspace_candidates()
        .map_err(ReverseSurfaceAdapterError::Workspace)?
    {
        if !document_matches_candidate(document, &summary) {
            continue;
        }
        if summary.freshness != DeprojectionCandidateFreshness::Current {
            saw_invalidated = true;
            continue;
        }
        return Ok(RetainedFinding {
            finding: summary.finding,
            explanation: Some(summary.explanation),
            comparison: Some(summary.comparison),
            retention_revision: summary.pin.catalog_generation.max(1),
        });
    }
    if let ObjectRef::Finding(finding) = document {
        for summary in session
            .list_analysis_evidence_findings()
            .map_err(ReverseSurfaceAdapterError::Workspace)?
        {
            if summary.finding != *finding {
                continue;
            }
            if summary.freshness != DeprojectionCandidateFreshness::Current {
                saw_invalidated = true;
                continue;
            }
            return Ok(RetainedFinding {
                finding: summary.finding,
                explanation: None,
                comparison: None,
                retention_revision: summary.pin.catalog_generation.max(1),
            });
        }
    }
    if saw_invalidated {
        Err(ReverseSurfaceAdapterError::NotCurrent)
    } else {
        Err(ReverseSurfaceAdapterError::FindingNotRetained)
    }
}

fn document_matches_candidate(
    document: &ObjectRef,
    summary: &DeprojectionCandidateDocumentSummary,
) -> bool {
    match document {
        ObjectRef::Finding(finding) => summary.finding == *finding,
        ObjectRef::Explanation(id) => summary.explanation == *id,
        ObjectRef::Comparison(id) => summary.comparison == *id,
        _ => false,
    }
}

fn map_workspace_apply_error(
    error: DeprojectionWorkspaceBridgeError,
) -> ReverseSurfaceAdapterError {
    match error {
        DeprojectionWorkspaceBridgeError::UnknownObject(_)
        | DeprojectionWorkspaceBridgeError::NoExecutableCandidate => {
            ReverseSurfaceAdapterError::NoPromotionPlan
        }
        DeprojectionWorkspaceBridgeError::Invalidated(_) => ReverseSurfaceAdapterError::NotCurrent,
        other => ReverseSurfaceAdapterError::Workspace(other),
    }
}

fn outcome_from_promotion(result: &ArtifactPromotionComparisonResult) -> ReverseSurfaceEditOutcome {
    let revision = result.promotion.project.publication.revisions.aggregate;
    let mut created = result
        .promotion
        .created
        .iter()
        .filter_map(object_from_promoted_created)
        .collect::<Vec<_>>();
    created.sort_by_key(|object| (promotion_reveal_rank(object), object.address()));
    created.dedup();
    let primary = created
        .first()
        .cloned()
        .unwrap_or(ObjectRef::Comparison(result.target.comparison));
    let related = if created.is_empty() {
        Vec::new()
    } else {
        created.iter().skip(1).cloned().collect()
    };
    let reveal = RevealRecommendation {
        request: RevealRequest::new(primary.clone(), RevealIntent::ActivateExisting)
            .at_revision(revision)
            .with_related(related.clone()),
        diagnostics: Vec::new(),
    };
    ReverseSurfaceEditOutcome {
        kind: ReverseSurfaceEditKind::Applied,
        primary,
        related,
        created,
        revision,
        reveal,
    }
}

fn object_from_promoted_created(created: &CreatedObject) -> Option<ObjectRef> {
    match created {
        CreatedObject::ArrangementTrack(id) => Some(ObjectRef::Track(*id)),
        CreatedObject::AudioClip(id)
        | CreatedObject::ExactAudioFallbackClip(id)
        | CreatedObject::ArrangementPatternClip(id)
        | CreatedObject::ArrangementAutomationClip(id) => Some(ObjectRef::AudioClip(*id)),
        CreatedObject::SequencerPattern(id) => Some(ObjectRef::Pattern(*id)),
        CreatedObject::AutomationLane(id) => Some(ObjectRef::Automation(*id)),
        CreatedObject::SampleKit(id) => Some(ObjectRef::Instrument(InstrumentRef::SampleKit(*id))),
        CreatedObject::SampleZone(target) => Some(ObjectRef::Pad(PadRef {
            kit: target.kit,
            pad: target.pad,
            zone: Some(target.zone),
        })),
        CreatedObject::MixerBus(id) => Some(ObjectRef::Bus(*id)),
        CreatedObject::SequencerPatternClip(_)
        | CreatedObject::SequencerLane(_)
        | CreatedObject::SamplePad(_) => None,
    }
}

fn promotion_reveal_rank(object: &ObjectRef) -> u8 {
    match object {
        ObjectRef::PatternOccurrence(_) => 0,
        ObjectRef::AudioClip(_) => 1,
        ObjectRef::Pattern(_) => 2,
        ObjectRef::AutomationOccurrence(_) => 3,
        ObjectRef::Automation(_) => 4,
        ObjectRef::Instrument(_) => 5,
        ObjectRef::Pad(_) => 6,
        ObjectRef::Track(_) => 7,
        ObjectRef::Bus(_) => 8,
        ObjectRef::Material(_) | ObjectRef::Sample(_) => 9,
        ObjectRef::Finding(_)
        | ObjectRef::Explanation(_)
        | ObjectRef::Comparison(_)
        | ObjectRef::Reading(_) => 10,
    }
}

#[derive(Debug)]
pub enum ReverseSurfaceAdapterError {
    MissingArtifact(ArtifactId),
    MissingExplanation(ExplanationId),
    MissingComparison(ComparisonId),
    FindingArtifactMismatch {
        finding: crate::project_controller::FindingRef,
        artifact: ArtifactId,
    },
    ExplanationArtifactMismatch {
        explanation: ExplanationId,
        artifact: ArtifactId,
    },
    ComparisonExplanationMismatch {
        comparison: ComparisonId,
        expected: ExplanationId,
        actual: ExplanationId,
    },
    ConsequenceUnavailable {
        key: String,
        reason: &'static str,
    },
    StaleProjectReceipt {
        requested: ProjectRevisions,
        current: ProjectRevisions,
    },
    NotCurrent,
    FindingNotRetained,
    NoPromotionPlan,
    Session(ProjectSessionError),
    Workspace(DeprojectionWorkspaceBridgeError),
    Promotion(ArtifactPromotionBridgeError),
    Surface(ReverseSurfaceError),
}

impl fmt::Display for ReverseSurfaceAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingArtifact(id) => write!(formatter, "analysis artifact {id:?} is missing"),
            Self::MissingExplanation(id) => {
                write!(formatter, "explanation {} is missing", id.0)
            }
            Self::MissingComparison(id) => write!(formatter, "comparison {} is missing", id.0),
            Self::FindingArtifactMismatch { finding, artifact } => write!(
                formatter,
                "finding {finding:?} does not address analysis artifact {artifact:?}"
            ),
            Self::ExplanationArtifactMismatch {
                explanation,
                artifact,
            } => write!(
                formatter,
                "explanation {} does not cite analysis artifact {artifact:?}",
                explanation.0
            ),
            Self::ComparisonExplanationMismatch {
                comparison,
                expected,
                actual,
            } => write!(
                formatter,
                "comparison {} references explanation {}, expected {}",
                comparison.0, actual.0, expected.0
            ),
            Self::ConsequenceUnavailable { key, reason } => {
                write!(formatter, "{key}: {reason}")
            }
            Self::StaleProjectReceipt { requested, current } => write!(
                formatter,
                "reverse edit was not applied because its project receipt is stale ({requested:?} vs {current:?})"
            ),
            Self::NotCurrent => formatter.write_str(
                "the reverse object is no longer current; inspect it, but do not apply or keep it",
            ),
            Self::FindingNotRetained => {
                formatter.write_str("no current analysis Finding is retained for this reverse object")
            }
            Self::NoPromotionPlan => formatter.write_str(
                "no exact promotion plan is bound to this reverse object; keep it as evidence or inspect it",
            ),
            Self::Session(error) => error.fmt(formatter),
            Self::Workspace(error) => error.fmt(formatter),
            Self::Promotion(error) => error.fmt(formatter),
            Self::Surface(error) => error.fmt(formatter),
        }
    }
}

impl Error for ReverseSurfaceAdapterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Session(error) => Some(error),
            Self::Workspace(error) => Some(error),
            Self::Promotion(error) => Some(error),
            Self::Surface(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ReverseSurfaceError> for ReverseSurfaceAdapterError {
    fn from(value: ReverseSurfaceError) -> Self {
        Self::Surface(value)
    }
}

impl From<ProjectSessionError> for ReverseSurfaceAdapterError {
    fn from(value: ProjectSessionError) -> Self {
        Self::Session(value)
    }
}

impl From<ArtifactPromotionBridgeError> for ReverseSurfaceAdapterError {
    fn from(value: ArtifactPromotionBridgeError) -> Self {
        Self::Promotion(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use crate::artifact_catalog::{sha256_content, ArtifactDescriptor, ArtifactKind};
    use crate::aspect::{Aspect, ChannelMask, FrameSpan};
    use crate::assets::{AssetFrameRange, AssetId, SampleFrames};
    use crate::comparison::{ComparisonDefinition, SourceCitation};
    use crate::deprojection_program::DeprojectionCandidateId;
    use crate::explanation::{ExplanationDefinition, ExplanationEvidenceRef, ExplanationScope};
    use crate::interpretation::InterpretationCommand;
    use crate::ontology::{Producer, Provenance};
    use crate::project_controller::{FindingKind, FindingLocalId, FindingRef};
    use crate::project_session::deprojection_workspace_bridge::{
        DeprojectionCandidateDocumentId, DeprojectionWorkspacePin,
    };

    fn provenance() -> Provenance {
        Provenance {
            producer: Producer::Analyzer {
                name: "surface-adapter-test".into(),
                version: "1".into(),
                configuration_digest: None,
            },
            created_unix_ms: None,
            source_revision: None,
            note: None,
        }
    }

    fn fixture(
        freshness: DeprojectionCandidateFreshness,
    ) -> (
        DeprojectionCandidateDocumentSummary,
        ArtifactCatalog,
        InterpretationStore,
    ) {
        let output = sha256_content(b"surface-adapter-output", &[b"one"]);
        let artifact = ArtifactId(output);
        let descriptor = ArtifactDescriptor {
            id: artifact,
            kind: ArtifactKind::ModelClaim,
            source_digest: sha256_content(b"surface-adapter-source", &[b"one"]),
            recipe_digest: sha256_content(b"surface-adapter-recipe", &[b"one"]),
            output_digest: output,
            extent: FrameSpan { start: 20, end: 84 },
            sample_rate: 48_000,
            channels: 2,
            provenance: provenance(),
        };
        let mut artifacts = ArtifactCatalog::new();
        artifacts
            .insert(descriptor.clone(), Arc::new(vec![1_u8, 2, 3]))
            .unwrap();

        let explanation = ExplanationDefinition {
            id: ExplanationId(4),
            label: "Candidate construction".into(),
            scope: ExplanationScope::ModelClaim { artifact, claim: 9 },
            extent: Aspect::Time(descriptor.extent),
            evidence: vec![ExplanationEvidenceRef::Artifact(artifact)],
            provenance: provenance(),
        };
        let comparison = ComparisonDefinition {
            id: ComparisonId(5),
            label: "Candidate comparison".into(),
            source: SourceCitation {
                asset: AssetId(1),
                source_range: AssetFrameRange::new(SampleFrames(20), SampleFrames(84)).unwrap(),
                project_span: descriptor.extent,
                channels: ChannelMask(3),
            },
            explanation: explanation.id,
            provenance: provenance(),
        };
        let mut interpretations = InterpretationStore::new();
        interpretations
            .apply(&[
                InterpretationCommand::PutExplanation {
                    before: None,
                    after: Some(explanation),
                },
                InterpretationCommand::PutComparison {
                    before: None,
                    after: Some(comparison),
                },
            ])
            .unwrap();

        let finding = FindingRef {
            kind: FindingKind::Rhythm,
            scope: FindingScope::Artifact(artifact),
            local: FindingLocalId::Claim(9),
        };
        let summary = DeprojectionCandidateDocumentSummary {
            id: DeprojectionCandidateDocumentId(7),
            artifact,
            candidate: DeprojectionCandidateId(sha256_content(
                b"surface-adapter-candidate",
                &[b"one"],
            )),
            finding,
            label: "Three-hit candidate".into(),
            comparison: ComparisonId(5),
            explanation: ExplanationId(4),
            pin: DeprojectionWorkspacePin {
                document_generation: 1,
                publication_generation: 1,
                project_revisions: Default::default(),
                selection_revision: 1,
                catalog_generation: 1,
                catalog_digest: sha256_content(b"surface-adapter-catalog", &[b"one"]),
            },
            freshness,
        };
        (summary, artifacts, interpretations)
    }

    #[test]
    fn current_candidate_hydrates_finding_explanation_and_comparison() {
        let (summary, artifacts, interpretations) =
            fixture(DeprojectionCandidateFreshness::Current);
        let documents = project_reverse_surface_documents(
            std::iter::once(&summary),
            std::iter::empty(),
            &artifacts,
            &interpretations,
        )
        .unwrap();

        assert_eq!(documents.len(), 3);
        let finding = documents
            .iter()
            .find(|document| document.object == ObjectRef::Finding(summary.finding))
            .unwrap();
        assert_eq!(finding.comparisons.len(), 1);
        assert_eq!(finding.evidence.len(), 3);
        assert_eq!(
            finding
                .edit_consequences
                .iter()
                .map(|consequence| consequence.key.as_str())
                .collect::<Vec<_>>(),
            vec![CONSEQUENCE_APPLY_CONSTRUCTION, CONSEQUENCE_KEEP_FINDING]
        );
        let explanation = documents
            .iter()
            .find(|document| document.object == ObjectRef::Explanation(summary.explanation))
            .unwrap();
        assert_eq!(
            explanation
                .edit_consequences
                .iter()
                .map(|consequence| consequence.key.as_str())
                .collect::<Vec<_>>(),
            vec![
                CONSEQUENCE_APPLY_CONSTRUCTION,
                crate::reverse_surface::CONSEQUENCE_EDIT_DEFINITION,
                CONSEQUENCE_KEEP_FINDING
            ]
        );
        assert!(documents
            .iter()
            .any(|document| document.object == ObjectRef::Comparison(summary.comparison)));
    }

    #[test]
    fn invalidated_candidate_remains_inspectable_but_not_executable() {
        let (summary, artifacts, interpretations) =
            fixture(DeprojectionCandidateFreshness::Invalidated);
        let documents = project_reverse_surface_documents(
            std::iter::once(&summary),
            std::iter::empty(),
            &artifacts,
            &interpretations,
        )
        .unwrap();
        let finding = documents
            .iter()
            .find(|document| document.object == ObjectRef::Finding(summary.finding))
            .unwrap();
        assert!(finding.edit_consequences.is_empty());
        assert!(matches!(
            &finding.body,
            crate::reverse_surface::ReverseSurfaceBody::Finding(body)
                if body.statements.iter().any(|statement| statement.contains("invalidated"))
        ));
    }

    #[test]
    fn explorer_collections_union_reverse_documents_and_interpretation_store() {
        let (summary, artifacts, interpretations) =
            fixture(DeprojectionCandidateFreshness::Current);
        let documents = project_reverse_surface_documents(
            std::iter::once(&summary),
            std::iter::empty(),
            &artifacts,
            &interpretations,
        )
        .unwrap();
        let mut store = ReverseSurfaceStore::new();
        for document in documents {
            store.insert(document).unwrap();
        }

        let empty =
            explorer_semantic_collections(&ReverseSurfaceStore::new(), &InterpretationStore::new());
        assert_eq!(empty, ExplorerSemanticCollections::default());

        let from_interpretations =
            explorer_semantic_collections(&ReverseSurfaceStore::new(), &interpretations);
        assert!(from_interpretations.findings.is_empty());
        assert_eq!(from_interpretations.explanations, vec![ExplanationId(4)]);
        assert_eq!(from_interpretations.comparisons, vec![ComparisonId(5)]);
        assert!(from_interpretations.readings.is_empty());

        let collections = explorer_semantic_collections(&store, &interpretations);
        assert_eq!(collections.findings, vec![summary.finding]);
        assert_eq!(collections.explanations, vec![ExplanationId(4)]);
        assert_eq!(collections.comparisons, vec![ComparisonId(5)]);
        assert!(collections.readings.is_empty());
    }

    #[test]
    fn interpretation_cross_link_mismatch_is_refused() {
        let (mut summary, artifacts, interpretations) =
            fixture(DeprojectionCandidateFreshness::Current);
        summary.explanation = ExplanationId(99);
        assert!(matches!(
            project_reverse_surface_documents(
                std::iter::once(&summary),
                std::iter::empty(),
                &artifacts,
                &interpretations,
            ),
            Err(ReverseSurfaceAdapterError::MissingExplanation(
                ExplanationId(99)
            ))
        ));
    }

    fn evidence_summary(
        freshness: DeprojectionCandidateFreshness,
        artifact: ArtifactId,
    ) -> AnalysisEvidenceDocumentSummary {
        AnalysisEvidenceDocumentSummary {
            id: crate::project_session::deprojection_workspace_bridge::AnalysisEvidenceDocumentId(
                3,
            ),
            artifact,
            finding: FindingRef {
                kind: FindingKind::Separation,
                scope: FindingScope::Artifact(artifact),
                local: FindingLocalId::Claim(11),
            },
            label: "Tonally sustained estimate".into(),
            kind: AnalysisEvidenceKind::HpssComponent(
                crate::explanation::HpssComponentKind::Harmonic,
            ),
            pin: DeprojectionWorkspacePin {
                document_generation: 1,
                publication_generation: 1,
                project_revisions: Default::default(),
                selection_revision: 1,
                catalog_generation: 1,
                catalog_digest: sha256_content(b"surface-adapter-catalog", &[b"one"]),
            },
            freshness,
        }
    }

    #[test]
    fn current_evidence_hydrates_keep_finding_without_apply() {
        let (candidate, artifacts, interpretations) =
            fixture(DeprojectionCandidateFreshness::Current);
        let summary = evidence_summary(DeprojectionCandidateFreshness::Current, candidate.artifact);
        let documents = project_reverse_surface_documents(
            std::iter::empty(),
            std::iter::once(&summary),
            &artifacts,
            &interpretations,
        )
        .unwrap();
        let finding = documents
            .iter()
            .find(|document| document.object == ObjectRef::Finding(summary.finding))
            .unwrap();
        assert_eq!(
            finding
                .edit_consequences
                .iter()
                .map(|consequence| consequence.key.as_str())
                .collect::<Vec<_>>(),
            vec![CONSEQUENCE_KEEP_FINDING]
        );
        assert!(finding.comparisons.is_empty());
    }

    #[test]
    fn unknown_consequence_is_a_typed_refusal() {
        let mut session =
            ProjectSession::new(crate::project_session::ProjectSessionId(901)).unwrap();
        assert!(matches!(
            execute_reverse_surface_consequence(
                &mut session,
                &ObjectRef::Finding(FindingRef {
                    kind: FindingKind::Rhythm,
                    scope: FindingScope::Derivation(crate::sample_material::DerivationScope(1)),
                    local: FindingLocalId::Claim(1),
                }),
                crate::reverse_surface::CONSEQUENCE_FORK_READING,
                None,
                &RenderCancellation::new(),
            ),
            Err(ReverseSurfaceAdapterError::ConsequenceUnavailable { key, .. })
                if key == crate::reverse_surface::CONSEQUENCE_FORK_READING
        ));
        assert!(matches!(
            execute_reverse_surface_consequence(
                &mut session,
                &ObjectRef::Finding(FindingRef {
                    kind: FindingKind::Rhythm,
                    scope: FindingScope::Derivation(crate::sample_material::DerivationScope(1)),
                    local: FindingLocalId::Claim(1),
                }),
                "invented-key",
                None,
                &RenderCancellation::new(),
            ),
            Err(ReverseSurfaceAdapterError::ConsequenceUnavailable { key, .. })
                if key == "invented-key"
        ));
    }

    fn published_rhythm_session() -> (
        ProjectSession,
        Vec<DeprojectionCandidateDocumentSummary>,
        ArtifactDescriptor,
        Vec<f32>,
    ) {
        let sample_rate = 8_000;
        let frame_count = 8_000;
        let mut samples = vec![0.0; frame_count];
        for onset in (400..frame_count).step_by(1_000) {
            for offset in 0..80 {
                samples[onset + offset] =
                    (1.0 - offset as f32 / 80.0) * if offset % 2 == 0 { 0.9 } else { -0.9 };
            }
        }
        let location = crate::assets::AssetLocation::new(
            Some(crate::assets::AbsolutePath::parse("/audio/reverse-surface-adapter.wav").unwrap()),
            None,
        )
        .unwrap();
        let mut registry = crate::assets::AssetRegistry::new();
        let asset = registry
            .register(crate::assets::AssetRegistration {
                name: "reverse surface rhythm".into(),
                location: location.clone(),
                metadata: crate::assets::DecodedAudioMetadata {
                    sample_rate_hz: sample_rate,
                    channels: 1,
                    frame_count: SampleFrames(frame_count as u64),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32le".into()),
                    bit_depth: Some(32),
                },
                content: crate::assets::ContentFingerprint::from_bytes(b"reverse surface rhythm"),
                provenance: crate::assets::AssetProvenance::new(
                    1,
                    crate::assets::AssetOrigin::ImportedFile {
                        importer: "test".into(),
                    },
                    location,
                ),
                tags: Default::default(),
                favorite: false,
            })
            .unwrap();
        let pcm = crate::daw_render::PcmAsset::new(
            crate::audio::AudioFormat::new(sample_rate, 1).unwrap(),
            Arc::from(samples.clone()),
        )
        .unwrap();
        let live = crate::live_project::LiveProject::from_source_material(
            crate::live_project::SourceMaterialMetadata::new("Live analysis", "Rhythm source"),
            registry,
            asset,
            pcm,
        )
        .unwrap();
        let mut session =
            ProjectSession::new(crate::project_session::ProjectSessionId(902)).unwrap();
        session.install(live, None).unwrap();
        let extent = FrameSpan::new(0, frame_count as i64).unwrap();
        session.replace_selection(crate::project_selection::ProjectSelection {
            time: Some(extent),
            aspect: Some(Aspect::Time(extent)),
            ..crate::project_selection::ProjectSelection::default()
        });
        let digest = |byte: u8| {
            crate::artifact_catalog::ContentDigest::new(
                crate::artifact_catalog::DigestAlgorithm::Sha256,
                [byte; 32],
            )
        };
        let descriptor = ArtifactDescriptor {
            id: ArtifactId(digest(0x44)),
            kind: ArtifactKind::ModelClaim,
            source_digest: digest(0x11),
            recipe_digest: digest(0x22),
            output_digest: digest(0x44),
            extent,
            sample_rate,
            channels: 1,
            provenance: provenance(),
        };
        let analysis = crate::rhythm::analyze_mono(
            &samples,
            descriptor.sample_rate,
            &crate::rhythm::RhythmConfig::default(),
        );
        let summaries = session
            .publish_deprojection_analysis(
                crate::project_session::deprojection_workspace_bridge::LiveDeprojectionAnalysis::from_rhythm(
                    descriptor.clone(),
                    analysis,
                    crate::rhythm_explanation::ExplainBudget::default(),
                    crate::explanation::RenderedExplanation {
                        origin_frame: descriptor.extent.start,
                        audio: crate::audio::ProjectAudio::from_interleaved(
                            crate::audio::AudioFormat::new(
                                descriptor.sample_rate,
                                descriptor.channels,
                            )
                            .unwrap(),
                            samples.clone(),
                        )
                        .unwrap(),
                    },
                ),
                &RenderCancellation::new(),
            )
            .unwrap();
        (session, summaries, descriptor, samples)
    }

    fn exact_audio_document(
        session: &ProjectSession,
        summaries: &[DeprojectionCandidateDocumentSummary],
    ) -> ObjectRef {
        summaries
            .iter()
            .find_map(|summary| {
                let resolved = session
                    .resolve_deprojection_workspace_request(DeprojectionWorkspaceTarget::Object(
                        ObjectRef::Comparison(summary.comparison),
                    ))
                    .ok()?;
                resolved
                    .request
                    .candidate
                    .program
                    .roots
                    .iter()
                    .any(|root| {
                        matches!(
                            resolved.request.candidate.program.terms[root].kind,
                            crate::deprojection_program::EditableTermKind::ExactAudioReference { .. }
                        )
                    })
                    .then_some(ObjectRef::Finding(summary.finding))
            })
            .expect("literal fallback candidate")
    }

    #[test]
    fn apply_construction_returns_created_identities_and_a_reveal() {
        let (mut session, summaries, _, _) = published_rhythm_session();
        let document = exact_audio_document(&session, &summaries);
        let requested_at = session.project_snapshot().unwrap().revisions();
        let outcome = apply_reverse_construction(
            &mut session,
            &document,
            Some(requested_at),
            &RenderCancellation::new(),
        )
        .unwrap();
        assert_eq!(outcome.kind, ReverseSurfaceEditKind::Applied);
        assert!(!outcome.created.is_empty());
        assert_eq!(outcome.primary, outcome.created[0]);
        assert_eq!(outcome.reveal.request.object, outcome.primary);
        assert_eq!(
            outcome.reveal.request.expected_project_revision,
            Some(outcome.revision)
        );
        assert!(outcome.revision > requested_at.aggregate);
        assert!(matches!(
            outcome.primary,
            ObjectRef::AudioClip(_) | ObjectRef::Pattern(_) | ObjectRef::Track(_)
        ));
    }

    #[test]
    fn apply_construction_on_an_explanation_uses_the_same_promotion_core() {
        let (mut session, summaries, _, _) = published_rhythm_session();
        let finding = exact_audio_document(&session, &summaries);
        let explanation = summaries
            .iter()
            .find(|summary| ObjectRef::Finding(summary.finding) == finding)
            .map(|summary| ObjectRef::Explanation(summary.explanation))
            .unwrap();
        let outcome = apply_reverse_construction(
            &mut session,
            &explanation,
            None,
            &RenderCancellation::new(),
        )
        .unwrap();
        assert_eq!(outcome.kind, ReverseSurfaceEditKind::Applied);
        assert!(!outcome.created.is_empty());
        assert_eq!(outcome.reveal.request.object, outcome.primary);
    }

    #[test]
    fn keep_finding_returns_the_retained_finding_identity() {
        let (session, summaries, _, _) = published_rhythm_session();
        let summary = summaries
            .iter()
            .find(|summary| summary.freshness == DeprojectionCandidateFreshness::Current)
            .unwrap();
        let outcome =
            keep_reverse_finding(&session, &ObjectRef::Finding(summary.finding), None).unwrap();
        assert_eq!(outcome.kind, ReverseSurfaceEditKind::Kept);
        assert_eq!(outcome.primary, ObjectRef::Finding(summary.finding));
        assert!(outcome.created.is_empty());
        assert_eq!(outcome.reveal.request.object, outcome.primary);
        assert_eq!(
            outcome.reveal.request.expected_project_revision,
            Some(outcome.revision)
        );
        assert!(outcome
            .related
            .contains(&ObjectRef::Explanation(summary.explanation)));

        let from_explanation =
            keep_reverse_finding(&session, &ObjectRef::Explanation(summary.explanation), None)
                .unwrap();
        assert_eq!(
            from_explanation.primary,
            ObjectRef::Finding(summary.finding)
        );
    }

    #[test]
    fn keep_and_apply_refuse_invalidated_or_unbound_objects() {
        let (mut session, summaries, _, _) = published_rhythm_session();
        let finding = ObjectRef::Finding(summaries[0].finding);
        session.set_edit_cursor(crate::project_selection::EditCursor { frame: 17 });
        assert!(matches!(
            keep_reverse_finding(&session, &finding, None),
            Err(ReverseSurfaceAdapterError::NotCurrent)
        ));
        assert!(matches!(
            apply_reverse_construction(&mut session, &finding, None, &RenderCancellation::new(),),
            Err(ReverseSurfaceAdapterError::NotCurrent)
        ));
        assert!(matches!(
            keep_reverse_finding(
                &session,
                &ObjectRef::Finding(FindingRef {
                    kind: FindingKind::Rhythm,
                    scope: FindingScope::Derivation(crate::sample_material::DerivationScope(99)),
                    local: FindingLocalId::Claim(99),
                }),
                None,
            ),
            Err(ReverseSurfaceAdapterError::FindingNotRetained)
        ));
    }

    #[test]
    fn keep_finding_retains_current_hpss_evidence() {
        let (mut session, _, mut descriptor, samples) = published_rhythm_session();
        descriptor.kind = ArtifactKind::Hpss;
        descriptor.id = ArtifactId(crate::artifact_catalog::ContentDigest::new(
            crate::artifact_catalog::DigestAlgorithm::Sha256,
            [0x46; 32],
        ));
        descriptor.output_digest = descriptor.id.0;
        let separated = crate::hpss::separate_harmonic_percussive(
            &samples,
            crate::hpss::HpssSettings::default(),
        )
        .unwrap();
        let evidence = session
            .publish_hpss_evidence(descriptor, separated, &RenderCancellation::new())
            .unwrap();
        let finding = ObjectRef::Finding(evidence[0].finding);
        let outcome = keep_reverse_finding(&session, &finding, None).unwrap();
        assert_eq!(outcome.kind, ReverseSurfaceEditKind::Kept);
        assert_eq!(outcome.primary, finding);
        assert!(matches!(
            apply_reverse_construction(&mut session, &finding, None, &RenderCancellation::new(),),
            Err(ReverseSurfaceAdapterError::NoPromotionPlan)
        ));
    }
}
