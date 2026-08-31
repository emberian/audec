//! Typed navigation adapters for findings, explanations, comparisons, and readings.
//!
//! Reverse-analysis identities do not share one integer namespace. Analyzer
//! findings require a content/derivation scope, explanation and comparison
//! definitions are project-local, and reading entities remain qualified by
//! the reading that minted them. This module preserves those distinctions
//! while lowering revealable roots into the product-level navigation
//! contract. It never applies a project command or treats navigation as
//! acceptance of a claim.

use crate::artifact_catalog::ArtifactId;
use crate::comparison::ComparisonId;
use crate::comparison_runtime::ComparisonExecution;
use crate::explanation::{ExplanationDefinition, ExplanationId};
use crate::project_controller::{
    recommend_constructive, FindingKind, FindingLocalId, FindingRef, FindingScope, ObjectNavigator,
    ObjectRef, RevealIntent, RevealPlan, RevealRecommendation, RevealRequest,
    RhythmPromotionApplied,
};
use crate::reading::{QualifiedEntityId, ReadingFile, ReadingId, VerificationTier};
use crate::rhythm_explanation::{PatternAlternativeId, PatternExplanation};
use crate::sample_material::ScopedProposalRef;
use crate::workspace_document::WorkspaceDocument;

/// The durable identity currently available at a reverse-workflow boundary.
///
/// `PatternAlternative` deliberately retains its full content digest even
/// though the current finding surface addresses the artifact-qualified local
/// claim. `ReadingEntity` is retained intact and refused until the workspace
/// can address reading-local entities without dropping their namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReverseTargetDescriptor {
    Finding(FindingRef),
    PatternAlternative {
        artifact: Option<ArtifactId>,
        alternative: PatternAlternativeId,
        claim_id: u64,
    },
    Explanation(ExplanationId),
    Comparison {
        comparison: ComparisonId,
        explanation: ExplanationId,
    },
    PromotedConstruction {
        constructed: ObjectRef,
        evidence: FindingRef,
    },
    Reading {
        reading: ReadingId,
        revision: u64,
        verification: VerificationTier,
    },
    ReadingEntity(QualifiedEntityId),
}

impl ReverseTargetDescriptor {
    pub fn pattern_alternative(
        artifact: Option<ArtifactId>,
        explanation: &PatternExplanation,
    ) -> Self {
        Self::PatternAlternative {
            artifact,
            alternative: explanation.id,
            claim_id: explanation.claim_id(),
        }
    }

    pub fn explanation(definition: &ExplanationDefinition) -> Self {
        Self::Explanation(definition.id)
    }

    pub fn comparison(execution: &ComparisonExecution) -> Self {
        Self::Comparison {
            comparison: execution.comparison,
            explanation: execution.explanation,
        }
    }

    pub fn reading(reading: &ReadingFile, verification: VerificationTier) -> Self {
        Self::Reading {
            reading: reading.reading_id,
            revision: reading.revision,
            verification,
        }
    }
}

/// Whether the receipt represents a project edit that already happened.
/// Resolving or planning a reveal never performs that edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectMutationDisposition {
    None,
    AlreadyCommitted { revision: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReverseReveal {
    pub target: ReverseTargetDescriptor,
    pub request: RevealRequest,
    pub mutation: ProjectMutationDisposition,
}

impl ReverseReveal {
    pub fn plan(&self, document: &WorkspaceDocument) -> RevealPlan {
        ObjectNavigator::plan(document, self.request.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsupportedReverseReveal {
    pub target: ReverseTargetDescriptor,
    pub reason: UnsupportedReverseRevealReason,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnsupportedReverseRevealReason {
    /// A local model-claim key cannot identify a finding without the artifact
    /// whose content minted it.
    PatternAlternativeHasNoArtifactScope,
    /// The current ObjectRef vocabulary can address a reading root but cannot
    /// encode `(reading, kind, local)` without losing identity.
    ReadingQualifiedEntitySurfaceUnavailable,
    ZeroProjectLocalId,
    ZeroReadingRevision,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReverseRevealResolution {
    Ready(ReverseReveal),
    Unsupported(UnsupportedReverseReveal),
}

impl ReverseRevealResolution {
    pub fn ready(self) -> Option<ReverseReveal> {
        match self {
            Self::Ready(reveal) => Some(reveal),
            Self::Unsupported(_) => None,
        }
    }
}

/// Resolve a reverse identity into navigation-only intent. Comparison and
/// reading roots remain observational: this function cannot emit or apply a
/// project command.
pub fn resolve_reverse_target(
    target: ReverseTargetDescriptor,
    intent: RevealIntent,
) -> ReverseRevealResolution {
    let object_and_related = match &target {
        ReverseTargetDescriptor::Finding(finding) => Ok((ObjectRef::Finding(*finding), Vec::new())),
        ReverseTargetDescriptor::PatternAlternative {
            artifact, claim_id, ..
        } => match artifact {
            Some(artifact) => Ok((
                ObjectRef::Finding(FindingRef {
                    kind: FindingKind::Rhythm,
                    scope: FindingScope::Artifact(*artifact),
                    local: FindingLocalId::Claim(*claim_id),
                }),
                Vec::new(),
            )),
            None => Err(UnsupportedReverseRevealReason::PatternAlternativeHasNoArtifactScope),
        },
        ReverseTargetDescriptor::Explanation(id) if id.0 == 0 => {
            Err(UnsupportedReverseRevealReason::ZeroProjectLocalId)
        }
        ReverseTargetDescriptor::Explanation(id) => Ok((ObjectRef::Explanation(*id), Vec::new())),
        ReverseTargetDescriptor::Comparison {
            comparison,
            explanation,
        } if comparison.0 == 0 || explanation.0 == 0 => {
            Err(UnsupportedReverseRevealReason::ZeroProjectLocalId)
        }
        ReverseTargetDescriptor::Comparison {
            comparison,
            explanation,
        } => Ok((
            ObjectRef::Comparison(*comparison),
            vec![ObjectRef::Explanation(*explanation)],
        )),
        ReverseTargetDescriptor::PromotedConstruction {
            constructed,
            evidence,
        } => Ok((constructed.clone(), vec![ObjectRef::Finding(*evidence)])),
        ReverseTargetDescriptor::Reading { revision: 0, .. } => {
            Err(UnsupportedReverseRevealReason::ZeroReadingRevision)
        }
        ReverseTargetDescriptor::Reading { reading, .. } => {
            Ok((ObjectRef::Reading(*reading), Vec::new()))
        }
        ReverseTargetDescriptor::ReadingEntity(_) => {
            Err(UnsupportedReverseRevealReason::ReadingQualifiedEntitySurfaceUnavailable)
        }
    };
    match object_and_related {
        Ok((object, related)) => ReverseRevealResolution::Ready(ReverseReveal {
            target,
            request: RevealRequest::new(object, intent).with_related(related),
            mutation: ProjectMutationDisposition::None,
        }),
        Err(reason) => {
            ReverseRevealResolution::Unsupported(UnsupportedReverseReveal { target, reason })
        }
    }
}

/// Reveal result for a reverse-to-forward promotion. The construction is the
/// primary editable object. Its originating scoped proposal is retained both
/// as a related selection breadcrumb and as its own explicit reveal request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromotionReveal {
    pub construction: ReverseReveal,
    pub evidence_breadcrumb: ReverseReveal,
    pub receipt_diagnostics: Vec<crate::project_controller::RevealDiagnostic>,
}

impl PromotionReveal {
    pub fn construction_plan(&self, document: &WorkspaceDocument) -> RevealPlan {
        self.construction.plan(document)
    }

    pub fn evidence_plan(&self, document: &WorkspaceDocument) -> RevealPlan {
        self.evidence_breadcrumb.plan(document)
    }
}

pub fn reveal_rhythm_promotion(applied: &RhythmPromotionApplied) -> PromotionReveal {
    let recommendation = recommend_constructive(&applied.publication);
    reveal_promotion_receipt(
        recommendation,
        applied.choice.0,
        FindingKind::Rhythm,
        applied.revisions.aggregate,
    )
}

fn reveal_promotion_receipt(
    construction: RevealRecommendation,
    proposal: ScopedProposalRef,
    kind: FindingKind,
    revision: u64,
) -> PromotionReveal {
    let finding = FindingRef {
        kind,
        scope: FindingScope::Derivation(proposal.scope),
        local: FindingLocalId::ReconstructionProposal(
            crate::reconstruction::ReconstructionProposalId::from_raw(proposal.local),
        ),
    };
    let RevealRecommendation {
        request,
        diagnostics: receipt_diagnostics,
    } = construction;
    let evidence = ObjectRef::Finding(finding);
    let constructed = request.object.clone();
    let mut related = request.related.clone();
    related.push(evidence.clone());
    let request = request.with_related(related);
    let mutation = ProjectMutationDisposition::AlreadyCommitted { revision };
    PromotionReveal {
        construction: ReverseReveal {
            target: ReverseTargetDescriptor::PromotedConstruction {
                constructed: constructed.clone(),
                evidence: finding,
            },
            request,
            mutation,
        },
        evidence_breadcrumb: ReverseReveal {
            target: ReverseTargetDescriptor::Finding(finding),
            request: RevealRequest::new(evidence, RevealIntent::ActivateExisting)
                .with_related(std::iter::once(constructed)),
            mutation,
        },
        receipt_diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::artifact_catalog::{ContentDigest, DigestAlgorithm};
    use crate::daw_project::ProjectRevisions;
    use crate::project_controller::{
        ConstructivePublication, ConstructivePublishedFocus, WorkspaceReveal,
    };
    use crate::sample_kit::KitId;
    use crate::sequencer::PatternId;
    use crate::workspace_document::{EditorTarget, NewWorkspaceView, WorkspaceItemKind};

    fn artifact(byte: u8) -> ArtifactId {
        ArtifactId(ContentDigest::new(DigestAlgorithm::Sha256, [byte; 32]))
    }

    fn assert_analysis_surface(workspace: WorkspaceReveal) {
        let kind = match workspace {
            WorkspaceReveal::Create(view) => view.kind,
            WorkspaceReveal::Retarget { descriptor, .. } => descriptor.kind,
            // The default document already contains an untargeted analysis
            // lens; a finding may reuse it while its exact identity remains
            // in the selection/Inspector consequence.
            WorkspaceReveal::Activate { .. } => return,
            other => panic!("expected an analysis descriptor, got {other:?}"),
        };
        assert!(matches!(kind, WorkspaceItemKind::AnalysisLens { .. }));
    }

    #[test]
    fn scoped_pattern_alternative_maps_to_rhythm_finding_and_unscoped_is_refused() {
        let target = ReverseTargetDescriptor::PatternAlternative {
            artifact: Some(artifact(7)),
            alternative: PatternAlternativeId(ContentDigest::new(DigestAlgorithm::Sha256, [9; 32])),
            claim_id: 17,
        };
        let reveal = resolve_reverse_target(target, RevealIntent::ActivateExisting)
            .ready()
            .unwrap();
        assert!(matches!(
            reveal.request.object,
            ObjectRef::Finding(FindingRef {
                kind: FindingKind::Rhythm,
                scope: FindingScope::Artifact(actual),
                local: FindingLocalId::Claim(17),
            }) if actual == artifact(7)
        ));
        assert_analysis_surface(reveal.plan(&WorkspaceDocument::default()).workspace);

        let unsupported = resolve_reverse_target(
            ReverseTargetDescriptor::PatternAlternative {
                artifact: None,
                alternative: PatternAlternativeId(ContentDigest::new(
                    DigestAlgorithm::Sha256,
                    [9; 32],
                )),
                claim_id: 17,
            },
            RevealIntent::ActivateExisting,
        );
        assert!(matches!(
            unsupported,
            ReverseRevealResolution::Unsupported(UnsupportedReverseReveal {
                reason: UnsupportedReverseRevealReason::PatternAlternativeHasNoArtifactScope,
                ..
            })
        ));
    }

    #[test]
    fn explanation_comparison_and_reading_have_distinct_target_descriptors() {
        let document = WorkspaceDocument::default();
        let explanation = resolve_reverse_target(
            ReverseTargetDescriptor::Explanation(ExplanationId(4)),
            RevealIntent::ActivateExisting,
        )
        .ready()
        .unwrap();
        let comparison = resolve_reverse_target(
            ReverseTargetDescriptor::Comparison {
                comparison: ComparisonId(8),
                explanation: ExplanationId(4),
            },
            RevealIntent::ActivateExisting,
        )
        .ready()
        .unwrap();
        let reading = resolve_reverse_target(
            ReverseTargetDescriptor::Reading {
                reading: ReadingId::new([5; 16]).unwrap(),
                revision: 3,
                verification: VerificationTier::GraphOnly,
            },
            RevealIntent::ActivateExisting,
        )
        .ready()
        .unwrap();

        assert!(matches!(
            explanation.plan(&document).workspace,
            WorkspaceReveal::Create(NewWorkspaceView {
                kind: WorkspaceItemKind::Extension { ref name, .. },
                ..
            }) if name == "explanation"
        ));
        assert!(matches!(
            comparison.plan(&document).workspace,
            WorkspaceReveal::Create(NewWorkspaceView {
                kind: WorkspaceItemKind::Render,
                target: EditorTarget::Render {
                    comparison_id: Some(8)
                },
                ..
            })
        ));
        assert_eq!(
            comparison.request.related,
            vec![ObjectRef::Explanation(ExplanationId(4))]
        );
        assert!(matches!(
            reading.plan(&document).workspace,
            WorkspaceReveal::Create(NewWorkspaceView {
                kind: WorkspaceItemKind::Extension { ref name, .. },
                ..
            }) if name == "reading"
        ));
        assert_eq!(comparison.mutation, ProjectMutationDisposition::None);
        assert_eq!(reading.mutation, ProjectMutationDisposition::None);
    }

    #[test]
    fn qualified_reading_entity_is_not_collapsed_to_project_or_reading_root() {
        let qualified =
            QualifiedEntityId::new(ReadingId::new([6; 16]).unwrap(), "comparison", 11).unwrap();
        let resolution = resolve_reverse_target(
            ReverseTargetDescriptor::ReadingEntity(qualified.clone()),
            RevealIntent::ActivateExisting,
        );
        assert_eq!(
            resolution,
            ReverseRevealResolution::Unsupported(UnsupportedReverseReveal {
                target: ReverseTargetDescriptor::ReadingEntity(qualified),
                reason: UnsupportedReverseRevealReason::ReadingQualifiedEntitySurfaceUnavailable,
            })
        );
    }

    #[test]
    fn promotion_reveals_editable_construction_and_scoped_evidence_breadcrumb() {
        let pattern = PatternId::from_raw(13);
        let applied = RhythmPromotionApplied {
            choice: crate::project_controller::RhythmPromotionChoiceId(ScopedProposalRef {
                scope: crate::sample_material::DerivationScope(91),
                local: 7,
            }),
            publication: ConstructivePublication {
                revision: 18,
                kit: KitId::from_raw(2),
                pad: None,
                pattern: Some(pattern),
                arrangement_clip: None,
                focus: ConstructivePublishedFocus::Pattern(pattern),
            },
            revisions: ProjectRevisions {
                aggregate: 18,
                ..ProjectRevisions::default()
            },
        };
        let reveal = reveal_rhythm_promotion(&applied);
        assert_eq!(
            reveal.construction.request.object,
            ObjectRef::Pattern(pattern)
        );
        let evidence = ObjectRef::Finding(FindingRef {
            kind: FindingKind::Rhythm,
            scope: FindingScope::Derivation(crate::sample_material::DerivationScope(91)),
            local: FindingLocalId::ReconstructionProposal(
                crate::reconstruction::ReconstructionProposalId::from_raw(7),
            ),
        });
        assert!(reveal.construction.request.related.contains(&evidence));
        assert_eq!(reveal.evidence_breadcrumb.request.object, evidence);
        assert!(reveal
            .evidence_breadcrumb
            .request
            .related
            .contains(&ObjectRef::Pattern(pattern)));
        assert_eq!(
            reveal.construction.mutation,
            ProjectMutationDisposition::AlreadyCommitted { revision: 18 }
        );
        assert!(matches!(
            reveal
                .construction_plan(&WorkspaceDocument::default())
                .workspace,
            WorkspaceReveal::Create(NewWorkspaceView {
                kind: WorkspaceItemKind::PatternEditor { .. },
                target: EditorTarget::PatternDefinition { id: 13 },
                ..
            })
        ));
        assert_analysis_surface(
            reveal
                .evidence_plan(&WorkspaceDocument::default())
                .workspace,
        );
    }
}
