//! Session-owned deprojection objects projected into reverse-surface documents.
//!
//! The deprojection workspace remains the authority for artifacts, Findings,
//! explanations, comparisons, and freshness. This adapter only joins those
//! already-published records into the immutable documents consumed by GPUI.
//! It never reruns analysis, invents an instrument identity, or applies a
//! project edit. Current Findings expose one explicit promotion consequence;
//! invalidated Findings remain inspectable without an executable control.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::artifact_catalog::{ArtifactCatalog, ArtifactId};
use crate::comparison::ComparisonId;
use crate::daw_project::ProjectDomain;
use crate::explanation::ExplanationId;
use crate::interpretation::InterpretationStore;
use crate::project_controller::{FindingScope, ObjectRef};
use crate::project_session::deprojection_workspace_bridge::{
    DeprojectionCandidateDocumentSummary, DeprojectionCandidateFreshness,
};
use crate::reverse_surface::{
    ComparisonSurfaceDocument, EditAuthority, FindingSurfaceDocument, ReverseSurfaceDocument,
    ReverseSurfaceError, SurfaceEditConsequence, SurfaceEvidence,
};

/// Build the complete semantic surface catalog for one project session.
///
/// Callers should pass the summaries returned by
/// `list_deprojection_workspace_candidates`, not a pane-local selection. That
/// keeps already-open surfaces inspectable after a newer analysis invalidates
/// their executable cohort.
pub fn project_reverse_surface_documents<'a>(
    summaries: impl IntoIterator<Item = &'a DeprojectionCandidateDocumentSummary>,
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
        if !explanation.scope.artifacts().contains(&descriptor.id) {
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
                .then(|| SurfaceEditConsequence {
                    key: "apply-construction".into(),
                    label: "Apply as editable construction…".into(),
                    authority: EditAuthority::ProjectCommand,
                    invalidates: vec![comparison_object.clone()],
                    creates: Vec::new(),
                    retains_evidence: vec![finding_object, explanation_object],
                    // The precise subset is known only after the candidate program
                    // is lowered. This conservative write envelope makes the
                    // consequence honest before that plan exists.
                    affected_domains: constructive_project_domains(),
                })
                .into_iter()
                .collect(),
            vec![comparison_document.clone()],
        )?;

        insert_document(&mut documents, finding)?;
        insert_document(
            &mut documents,
            ReverseSurfaceDocument::explanation(explanation, vec![comparison_document.clone()])?,
        )?;
        insert_document(
            &mut documents,
            ReverseSurfaceDocument::from_comparison(comparison_document)?,
        )?;
    }
    Ok(documents.into_values().collect())
}

fn constructive_project_domains() -> BTreeSet<ProjectDomain> {
    BTreeSet::from([
        ProjectDomain::Arrangement,
        ProjectDomain::Sequencer,
        ProjectDomain::Automation,
        ProjectDomain::Assets,
        ProjectDomain::Mixer,
        ProjectDomain::SampleKits,
        ProjectDomain::Bindings,
    ])
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
            Self::Surface(error) => error.fmt(formatter),
        }
    }
}

impl Error for ReverseSurfaceAdapterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
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
        assert_eq!(finding.edit_consequences.len(), 1);
        assert_eq!(
            finding.edit_consequences[0].authority,
            EditAuthority::ProjectCommand
        );
        assert!(documents
            .iter()
            .any(|document| { document.object == ObjectRef::Explanation(summary.explanation) }));
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
    fn interpretation_cross_link_mismatch_is_refused() {
        let (mut summary, artifacts, interpretations) =
            fixture(DeprojectionCandidateFreshness::Current);
        summary.explanation = ExplanationId(99);
        assert!(matches!(
            project_reverse_surface_documents(
                std::iter::once(&summary),
                &artifacts,
                &interpretations,
            ),
            Err(ReverseSurfaceAdapterError::MissingExplanation(
                ExplanationId(99)
            ))
        ));
    }
}
