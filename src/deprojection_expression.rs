//! Session-facing reverse-to-forward expression adoption.
//!
//! This layer deliberately accepts a rendered evaluation rather than a loose
//! quality label. The residual, excess, and description cost shown beside the
//! adopted construction are therefore pinned to the exact `SourceProgram`
//! compiled by the atomic promotion layer.

use std::fmt;

use crate::artifact_catalog::ContentDigest;
use crate::comparison::ExactRenderDigest;
use crate::deprojection_evaluation::{
    source_program_identity, RenderedEvaluation, RenderedEvaluationDigests,
};
use crate::deprojection_execution::promotion::{
    promote, PromotionBindings, PromotionError, PromotionPlacement, PromotionResult,
    RetainedExpression,
};
use crate::deprojection_program::DeprojectionCandidate;
use crate::project_session::ProjectSession;

/// Render-aligned score carried into adoption. It is intentionally small, but
/// preserves the exact render identities needed to audit what was scored.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AlignedExpressionScore {
    pub program_identity: ContentDigest,
    pub residual_ratio: f64,
    pub excess_ratio: f64,
    pub description_bytes: u64,
    pub objective: f64,
    pub source_render: ExactRenderDigest,
    pub construction_render: ExactRenderDigest,
    pub residual_render: ExactRenderDigest,
    pub coverage: ContentDigest,
}

impl From<&RenderedEvaluation> for AlignedExpressionScore {
    fn from(evaluation: &RenderedEvaluation) -> Self {
        let RenderedEvaluationDigests {
            source,
            construction,
            residual,
            coverage,
        } = evaluation.digests;
        Self {
            program_identity: evaluation.structural.program_identity,
            residual_ratio: evaluation.score.residual_ratio,
            excess_ratio: evaluation.score.excess_ratio,
            description_bytes: evaluation.score.description_bytes,
            objective: evaluation.score.objective,
            source_render: source,
            construction_render: construction,
            residual_render: residual,
            coverage,
        }
    }
}

/// Narrow UI-neutral request. Bindings remain explicit, so anonymous evidence
/// families never turn into project instruments merely from a label.
#[derive(Clone, Debug, PartialEq)]
pub struct ExplainAsExpressionRequest {
    pub candidate: DeprojectionCandidate,
    pub expected_project_revision: u64,
    pub bindings: PromotionBindings,
    pub placement: PromotionPlacement,
    pub score: AlignedExpressionScore,
}

#[derive(Clone, Debug)]
pub struct ExplainAsExpressionResult {
    pub promotion: PromotionResult,
    pub score: AlignedExpressionScore,
    /// Canonical symbolic roots, with exact audio retained as its own variant.
    pub retained_roots: Vec<RetainedExpression>,
}

#[derive(Debug)]
pub enum ExplainAsExpressionError {
    ScoreProgramMismatch {
        scored: ContentDigest,
        requested: ContentDigest,
    },
    DescriptionCostMismatch {
        scored: u64,
        requested: u64,
    },
    InvalidScore(&'static str),
    NoRetainedExpressionRoot,
    Promotion(PromotionError),
}

impl fmt::Display for ExplainAsExpressionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScoreProgramMismatch { .. } => {
                formatter.write_str("rendered score belongs to a different source program")
            }
            Self::DescriptionCostMismatch { scored, requested } => write!(
                formatter,
                "rendered description cost {scored} does not match program cost {requested}"
            ),
            Self::InvalidScore(field) => write!(formatter, "invalid rendered score field: {field}"),
            Self::NoRetainedExpressionRoot => formatter.write_str(
                "candidate has no canonical pattern, curve, or exact-audio fallback root",
            ),
            Self::Promotion(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ExplainAsExpressionError {}

impl From<PromotionError> for ExplainAsExpressionError {
    fn from(value: PromotionError) -> Self {
        Self::Promotion(value)
    }
}

/// Compile and publish one candidate as one atomic ordinary project edit.
pub fn explain_as_expression(
    session: &mut ProjectSession,
    request: ExplainAsExpressionRequest,
) -> Result<ExplainAsExpressionResult, ExplainAsExpressionError> {
    validate_score(&request)?;
    let candidate = request.candidate;
    let roots = candidate.program.roots.clone();
    let score = request.score;
    let promotion = promote(
        session,
        crate::deprojection_execution::promotion::PromotionRequest {
            candidate: candidate.id,
            expected_project_revision: request.expected_project_revision,
            program: candidate.program,
            bindings: request.bindings,
            placement: request.placement,
        },
    )?;
    let retained_roots = roots
        .iter()
        .filter_map(|root| promotion.provenance.get(root)?.expression.clone())
        .collect::<Vec<_>>();
    debug_assert!(
        !retained_roots.is_empty(),
        "preflight retained an expression root but promotion omitted it"
    );
    Ok(ExplainAsExpressionResult {
        promotion,
        score,
        retained_roots,
    })
}

fn validate_score(request: &ExplainAsExpressionRequest) -> Result<(), ExplainAsExpressionError> {
    let program = &request.candidate.program;
    let requested = source_program_identity(program);
    if request.score.program_identity != requested {
        return Err(ExplainAsExpressionError::ScoreProgramMismatch {
            scored: request.score.program_identity,
            requested,
        });
    }
    let description_bytes = program.terms.values().fold(0_u64, |total, term| {
        total.saturating_add(term.description_bytes)
    });
    if request.score.description_bytes != description_bytes
        || request.candidate.structural_score.description_bytes != description_bytes
    {
        return Err(ExplainAsExpressionError::DescriptionCostMismatch {
            scored: request.score.description_bytes,
            requested: description_bytes,
        });
    }
    for (name, value) in [
        ("residual_ratio", request.score.residual_ratio),
        ("excess_ratio", request.score.excess_ratio),
    ] {
        if !value.is_finite() || value < 0.0 {
            return Err(ExplainAsExpressionError::InvalidScore(name));
        }
    }
    // Evidence credit can legitimately make the ranking objective negative.
    if !request.score.objective.is_finite() {
        return Err(ExplainAsExpressionError::InvalidScore("objective"));
    }
    let has_retained_root = program.roots.iter().any(|root| {
        matches!(
            program.terms.get(root).map(|term| &term.kind),
            Some(
                crate::deprojection_program::EditableTermKind::Pattern { .. }
                    | crate::deprojection_program::EditableTermKind::Curve { .. }
                    | crate::deprojection_program::EditableTermKind::ExactAudioReference { .. }
            )
        )
    });
    if !has_retained_root {
        return Err(ExplainAsExpressionError::NoRetainedExpressionRoot);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_catalog::{ContentDigest, DigestAlgorithm};
    use crate::deprojection_program::{
        CandidateOrigin, Derivation, EditableTerm, EditableTermId, EditableTermKind, EvidenceRef,
        MaterialSpan, SourceClaim, SourceProgram, StructuralScorePolicy,
    };
    use crate::rhythm::SampleSpan;

    fn digest(byte: u8) -> ContentDigest {
        ContentDigest::new(DigestAlgorithm::Sha256, [byte; 32])
    }

    fn request() -> ExplainAsExpressionRequest {
        let source = MaterialSpan {
            material_sha256: "22".repeat(32),
            start_frame: 0,
            frame_count: 16,
            sample_rate_hz: 48_000,
            channels: 1,
        };
        let claim = SourceClaim::literal(source.clone(), digest(2)).unwrap();
        let term = EditableTerm {
            id: EditableTermId(digest(3)),
            kind: EditableTermKind::ExactAudioReference {
                source: claim.id,
                span: SampleSpan { start: 0, end: 16 },
            },
            evidence: vec![EvidenceRef::SourceClaim(claim.id)],
            derivation: Derivation {
                rule: "test.exact-audio.v1".into(),
                recipe: digest(4),
                premises: vec![EvidenceRef::SourceClaim(claim.id)],
            },
            description_bytes: 37,
            free_parameters: 0,
        };
        let program = SourceProgram::new(source, vec![term.clone()], vec![term.id]).unwrap();
        let identity = source_program_identity(&program);
        let candidate = DeprojectionCandidate::new(
            "Exact rhythm audio".into(),
            CandidateOrigin::NativePitch {
                analyzer_version: "test".into(),
                track: 0,
                modulation: 0,
            },
            program,
            vec![claim],
            500_000,
            StructuralScorePolicy::default(),
            Vec::new(),
        )
        .unwrap();
        ExplainAsExpressionRequest {
            candidate,
            expected_project_revision: 7,
            bindings: PromotionBindings::default(),
            placement: PromotionPlacement::default(),
            score: AlignedExpressionScore {
                program_identity: identity,
                residual_ratio: 0.125,
                excess_ratio: 0.25,
                description_bytes: 37,
                objective: -0.01,
                source_render: ExactRenderDigest(digest(5)),
                construction_render: ExactRenderDigest(digest(6)),
                residual_render: ExactRenderDigest(digest(7)),
                coverage: digest(8),
            },
        }
    }

    #[test]
    fn score_alignment_accepts_negative_objective_but_rejects_identity_or_cost_drift() {
        let valid = request();
        validate_score(&valid).unwrap();

        let mut identity_drift = valid.clone();
        identity_drift.score.program_identity = digest(99);
        assert!(matches!(
            validate_score(&identity_drift),
            Err(ExplainAsExpressionError::ScoreProgramMismatch { .. })
        ));

        let mut cost_drift = valid;
        cost_drift.score.description_bytes += 1;
        assert!(matches!(
            validate_score(&cost_drift),
            Err(ExplainAsExpressionError::DescriptionCostMismatch {
                scored: 38,
                requested: 37,
            })
        ));
    }
}
