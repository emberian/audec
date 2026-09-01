//! Deterministic structural and exact-render scoring for source programs.
//!
//! Structural scores guide bounded search before audio exists. Final scores are
//! created only from a sample-aligned DAW render plus the shared coverage
//! implementation. The two phases are different types so a novelty/span proxy
//! cannot accidentally masquerade as reconstruction coverage.

use std::fmt;

use crate::artifact_catalog::{sha256_content, ContentDigest};
use crate::comparison::{ExactRenderDigest, RenderedComparison};
use crate::coverage::{compute_coverage, CoverageError, CoverageField, CoverageRecipe};
use crate::daw_render::RenderCancellation;
use crate::deprojection_program::{
    score_deprojection, DeprojectionError, DeprojectionScore, ScorePolicy, SourceProgram,
};

const STRUCTURAL_DOMAIN: &[u8] = b"audec:deprojection-structural-score:v2";
const RENDERED_DOMAIN: &[u8] = b"audec:deprojection-rendered-score:v1";

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StructuralScorePolicy {
    pub description_byte_weight: f64,
    pub free_parameter_weight: f64,
    pub unresolved_term_weight: f64,
    pub observation_penalty_weight: f64,
    pub evidence_credit: f64,
}

impl Default for StructuralScorePolicy {
    fn default() -> Self {
        Self {
            description_byte_weight: 1.0 / 8_192.0,
            free_parameter_weight: 1.0 / 128.0,
            unresolved_term_weight: 0.25,
            observation_penalty_weight: 1.0,
            evidence_credit: 0.1,
        }
    }
}

/// Search-only observation penalties. Missing measurements remain `None` and
/// are not fabricated as a perfect fit.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ObservationFit {
    pub onset_rms_frames: Option<f64>,
    pub onset_max_frames: Option<f64>,
    pub pitch_rms_cents: Option<f64>,
    pub voiced_recall: Option<f64>,
    pub activation_cosine: Option<f64>,
}

impl ObservationFit {
    pub fn normalized_penalty(self) -> Result<f64, EvaluationError> {
        let optional = [
            self.onset_rms_frames,
            self.onset_max_frames,
            self.pitch_rms_cents,
            self.voiced_recall,
            self.activation_cosine,
        ];
        if optional
            .into_iter()
            .flatten()
            .any(|value| !value.is_finite())
        {
            return Err(EvaluationError::Invalid(
                "observation fit contains a non-finite value".into(),
            ));
        }
        if self.onset_rms_frames.is_some_and(|value| value < 0.0)
            || self.onset_max_frames.is_some_and(|value| value < 0.0)
            || self.pitch_rms_cents.is_some_and(|value| value < 0.0)
            || self
                .voiced_recall
                .is_some_and(|value| !(0.0..=1.0).contains(&value))
            || self
                .activation_cosine
                .is_some_and(|value| !(-1.0..=1.0).contains(&value))
        {
            return Err(EvaluationError::Invalid(
                "observation fit is outside its declared domain".into(),
            ));
        }

        let mut sum = 0.0;
        let mut count = 0_u32;
        if let Some(value) = self.onset_rms_frames {
            sum += value / (value + 32.0);
            count += 1;
        }
        if let Some(value) = self.onset_max_frames {
            sum += value / (value + 128.0);
            count += 1;
        }
        if let Some(value) = self.pitch_rms_cents {
            sum += value / (value + 50.0);
            count += 1;
        }
        if let Some(value) = self.voiced_recall {
            sum += 1.0 - value;
            count += 1;
        }
        if let Some(value) = self.activation_cosine {
            sum += (1.0 - value) * 0.5;
            count += 1;
        }
        Ok(if count == 0 {
            0.0
        } else {
            sum / f64::from(count)
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeprojectionRankKey {
    /// Signed millionths of the floating objective, saturated explicitly.
    pub objective_microunits: i64,
    pub residual_parts_per_million: u64,
    pub excess_parts_per_million: u64,
    pub description_bytes: u64,
    pub identity: ContentDigest,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StructuralEvaluation {
    pub program_identity: ContentDigest,
    pub observation: ObservationFit,
    pub observation_penalty: f64,
    pub description_bytes: u64,
    pub free_parameters: u32,
    pub unresolved_terms: u32,
    pub evidence_support: f64,
    /// Search phase only. No residual/excess fields exist on this type.
    pub objective: f64,
    pub rank: DeprojectionRankKey,
}

pub fn evaluate_structure(
    program: &SourceProgram,
    observation: ObservationFit,
    evidence_support: f64,
    policy: StructuralScorePolicy,
) -> Result<StructuralEvaluation, EvaluationError> {
    if !evidence_support.is_finite() || !(0.0..=1.0).contains(&evidence_support) {
        return Err(EvaluationError::Invalid(
            "evidence support must be finite and in [0, 1]".into(),
        ));
    }
    let policy_values = [
        policy.description_byte_weight,
        policy.free_parameter_weight,
        policy.unresolved_term_weight,
        policy.observation_penalty_weight,
        policy.evidence_credit,
    ];
    if policy_values
        .iter()
        .any(|value| !value.is_finite() || *value < 0.0)
    {
        return Err(EvaluationError::Invalid(
            "structural score weights must be finite and non-negative".into(),
        ));
    }
    let observation_penalty = observation.normalized_penalty()?;
    let description_bytes = program.terms.values().fold(0_u64, |total, term| {
        total.saturating_add(term.description_bytes)
    });
    let free_parameters = program.terms.values().fold(0_u32, |total, term| {
        total.saturating_add(term.free_parameters)
    });
    let unresolved_terms = program.compile_refusals().len() as u32;
    let objective = description_bytes as f64 * policy.description_byte_weight
        + f64::from(free_parameters) * policy.free_parameter_weight
        + f64::from(unresolved_terms) * policy.unresolved_term_weight
        + observation_penalty * policy.observation_penalty_weight
        - evidence_support * policy.evidence_credit;
    let program_identity = source_program_identity(program);
    let rank = DeprojectionRankKey {
        objective_microunits: quantize_signed(objective, 1_000_000.0),
        residual_parts_per_million: u64::MAX,
        excess_parts_per_million: u64::MAX,
        description_bytes,
        identity: program_identity,
    };
    Ok(StructuralEvaluation {
        program_identity,
        observation,
        observation_penalty,
        description_bytes,
        free_parameters,
        unresolved_terms,
        evidence_support,
        objective,
        rank,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderedEvaluationDigests {
    pub source: ExactRenderDigest,
    pub construction: ExactRenderDigest,
    pub residual: ExactRenderDigest,
    pub coverage: ContentDigest,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenderedEvaluation {
    pub structural: StructuralEvaluation,
    pub coverage: CoverageField,
    pub score: DeprojectionScore,
    pub digests: RenderedEvaluationDigests,
    pub rank: DeprojectionRankKey,
}

#[allow(clippy::too_many_arguments)]
pub fn evaluate_rendered(
    program: &SourceProgram,
    structural: StructuralEvaluation,
    rendered: &RenderedComparison,
    coverage_recipe: CoverageRecipe,
    assumption_penalty: f64,
    policy: ScorePolicy,
    cancellation: &RenderCancellation,
) -> Result<RenderedEvaluation, EvaluationError> {
    if structural.program_identity != source_program_identity(program) {
        return Err(EvaluationError::ProgramChangedSinceStructuralScore);
    }
    if rendered.metrics.quarantined_source_samples != 0
        || rendered.metrics.quarantined_construction_samples != 0
    {
        return Err(EvaluationError::NonFinitePcm {
            source_samples: rendered.metrics.quarantined_source_samples,
            construction_samples: rendered.metrics.quarantined_construction_samples,
        });
    }
    let coverage = compute_coverage(rendered, coverage_recipe, cancellation)?;
    let score = score_deprojection(
        coverage.summary,
        structural.description_bytes,
        structural.free_parameters,
        structural.evidence_support,
        assumption_penalty,
        policy,
    )?;
    let digests = RenderedEvaluationDigests {
        source: crate::comparison_runtime::exact_audio_digest(
            &rendered.source,
            rendered.origin_frame,
        )?,
        construction: crate::comparison_runtime::exact_audio_digest(
            &rendered.construction,
            rendered.origin_frame,
        )?,
        residual: crate::comparison_runtime::exact_audio_digest(
            &rendered.residual,
            rendered.origin_frame,
        )?,
        coverage: coverage_digest(&coverage),
    };
    let rank = DeprojectionRankKey {
        objective_microunits: quantize_signed(score.objective, 1_000_000.0),
        residual_parts_per_million: quantize_unsigned(score.residual_ratio, 1_000_000.0),
        excess_parts_per_million: quantize_unsigned(score.excess_ratio, 1_000_000.0),
        description_bytes: structural.description_bytes,
        identity: structural.program_identity,
    };
    Ok(RenderedEvaluation {
        structural,
        coverage,
        score,
        digests,
        rank,
    })
}

pub fn rank_structural(evaluations: &mut [StructuralEvaluation]) {
    evaluations.sort_by_key(|evaluation| evaluation.rank);
}

pub fn rank_rendered(evaluations: &mut [RenderedEvaluation]) {
    evaluations.sort_by_key(|evaluation| evaluation.rank);
}

/// Stable identity shared by structural evaluation and revision-pinned
/// expression promotion. A rendered score may accompany a promotion only
/// when this exact identity still matches.
pub fn source_program_identity(program: &SourceProgram) -> ContentDigest {
    let mut canonical = Vec::new();
    push_bytes(&mut canonical, program.source.material_sha256.as_bytes());
    push_u64(&mut canonical, program.source.start_frame);
    push_u64(&mut canonical, program.source.frame_count);
    push_u64(&mut canonical, u64::from(program.source.sample_rate_hz));
    push_u64(&mut canonical, u64::from(program.source.channels));
    push_u64(&mut canonical, program.roots.len() as u64);
    for root in &program.roots {
        push_digest(&mut canonical, root.0);
    }
    push_u64(&mut canonical, program.terms.len() as u64);
    for (id, term) in &program.terms {
        push_digest(&mut canonical, id.0);
        push_term_kind(&mut canonical, &term.kind);
        push_u64(&mut canonical, term.evidence.len() as u64);
        for evidence in &term.evidence {
            push_evidence(&mut canonical, evidence);
        }
        push_derivation(&mut canonical, &term.derivation);
        push_u64(&mut canonical, term.description_bytes);
        push_u64(&mut canonical, u64::from(term.free_parameters));
    }
    sha256_content(STRUCTURAL_DOMAIN, &[&canonical])
}

fn push_term_kind(bytes: &mut Vec<u8>, kind: &crate::deprojection_program::EditableTermKind) {
    use crate::deprojection_program::EditableTermKind;
    match kind {
        EditableTermKind::SampleSlice {
            source,
            span,
            onset_offset_frames,
        } => {
            bytes.push(0);
            push_digest(bytes, source.0);
            push_u64(bytes, span.start as u64);
            push_u64(bytes, span.end as u64);
            push_u64(bytes, *onset_offset_frames);
        }
        EditableTermKind::Pattern {
            source,
            execution,
            voices,
        } => {
            bytes.push(1);
            push_bytes(bytes, source.as_bytes());
            push_u64(bytes, execution.cycle.0);
            push_u64(bytes, execution.seed);
            push_u64(bytes, execution.initial_cycle_index);
            push_u64(bytes, voices.len() as u64);
            for (name, voice) in voices {
                push_bytes(bytes, name.as_bytes());
                push_voice(bytes, voice);
            }
        }
        EditableTermKind::Curve {
            target,
            expression,
            source_span,
        } => {
            bytes.push(2);
            push_curve_target(bytes, *target);
            push_curve_expr(bytes, expression);
            push_u64(bytes, source_span.0);
            push_u64(bytes, source_span.1);
        }
        EditableTermKind::Notes { gestures } => {
            bytes.push(3);
            push_u64(bytes, gestures.len() as u64);
            for gesture in gestures {
                push_u64(bytes, gesture.start_frame);
                push_u64(bytes, gesture.duration_frames);
                bytes.push(gesture.midi_key);
                bytes.extend_from_slice(&gesture.velocity.to_bits().to_le_bytes());
                push_u64(bytes, gesture.pitch_curve_cents.len() as u64);
                for (frame, cents) in &gesture.pitch_curve_cents {
                    push_u64(bytes, *frame);
                    bytes.extend_from_slice(&cents.to_bits().to_le_bytes());
                }
            }
        }
        EditableTermKind::PresetCandidate {
            format,
            artifact,
            editable_parameters,
        } => {
            bytes.push(4);
            push_bytes(bytes, format.as_bytes());
            push_digest(bytes, artifact.0);
            push_u64(bytes, editable_parameters.len() as u64);
            for parameter in editable_parameters {
                push_bytes(bytes, parameter.as_bytes());
            }
        }
        EditableTermKind::ExactAudioReference { source, span } => {
            bytes.push(5);
            push_digest(bytes, source.0);
            push_u64(bytes, span.start as u64);
            push_u64(bytes, span.end as u64);
        }
    }
}

fn push_voice(bytes: &mut Vec<u8>, voice: &crate::deprojection_program::VoiceTerm) {
    use crate::deprojection_program::VoiceTerm;
    match voice {
        VoiceTerm::UnresolvedFamily { family } => {
            bytes.push(0);
            push_u64(bytes, *family as u64);
        }
        VoiceTerm::Sample(term) => {
            bytes.push(1);
            push_digest(bytes, term.0);
        }
        VoiceTerm::AudioClaim(claim) => {
            bytes.push(2);
            push_digest(bytes, claim.0);
        }
        VoiceTerm::Preset(artifact) => {
            bytes.push(3);
            push_digest(bytes, artifact.0);
        }
    }
}

fn push_curve_target(bytes: &mut Vec<u8>, target: crate::deprojection_program::CurveTarget) {
    use crate::deprojection_program::CurveTarget;
    bytes.push(match target {
        CurveTarget::Gain => 0,
        CurveTarget::PitchCents => 1,
        CurveTarget::Pan => 2,
        CurveTarget::Brightness => 3,
        CurveTarget::FilterCutoff => 4,
        CurveTarget::StereoWidth => 5,
        CurveTarget::TailLevel => 6,
    });
}

fn push_curve_expr(bytes: &mut Vec<u8>, expression: &crate::curve_lang::CurveExpr) {
    use crate::curve_lang::{CurveExpr, LfoShape};
    match expression {
        CurveExpr::Const(value) => {
            bytes.push(0);
            bytes.extend_from_slice(&value.to_bits().to_le_bytes());
        }
        CurveExpr::Line { from, to } => {
            bytes.push(1);
            bytes.extend_from_slice(&from.to_bits().to_le_bytes());
            bytes.extend_from_slice(&to.to_bits().to_le_bytes());
        }
        CurveExpr::Lfo {
            shape,
            rate_hz,
            depth,
            phase,
        } => {
            bytes.push(2);
            bytes.push(match shape {
                LfoShape::Sine => 0,
                LfoShape::Triangle => 1,
                LfoShape::Square => 2,
                LfoShape::Saw => 3,
            });
            for value in [rate_hz, depth, phase] {
                bytes.extend_from_slice(&value.to_bits().to_le_bytes());
            }
        }
        CurveExpr::Env {
            attack,
            decay,
            sustain,
            release,
        } => {
            bytes.push(3);
            for value in [attack, decay, sustain, release] {
                bytes.extend_from_slice(&value.to_bits().to_le_bytes());
            }
        }
        CurveExpr::Sum(expressions) => {
            bytes.push(4);
            push_u64(bytes, expressions.len() as u64);
            for expression in expressions {
                push_curve_expr(bytes, expression);
            }
        }
        CurveExpr::Scale {
            input,
            multiply,
            add,
        } => {
            bytes.push(5);
            push_curve_expr(bytes, input);
            bytes.extend_from_slice(&multiply.to_bits().to_le_bytes());
            bytes.extend_from_slice(&add.to_bits().to_le_bytes());
        }
        CurveExpr::Clamp { input, min, max } => {
            bytes.push(6);
            push_curve_expr(bytes, input);
            bytes.extend_from_slice(&min.to_bits().to_le_bytes());
            bytes.extend_from_slice(&max.to_bits().to_le_bytes());
        }
        CurveExpr::FromEvidence(evidence) => {
            bytes.push(7);
            push_u64(bytes, evidence.get());
        }
    }
}

fn push_derivation(bytes: &mut Vec<u8>, derivation: &crate::deprojection_program::Derivation) {
    push_bytes(bytes, derivation.rule.as_bytes());
    push_digest(bytes, derivation.recipe);
    push_u64(bytes, derivation.premises.len() as u64);
    for premise in &derivation.premises {
        push_evidence(bytes, premise);
    }
}

fn push_evidence(bytes: &mut Vec<u8>, evidence: &crate::deprojection_program::EvidenceRef) {
    use crate::deprojection_program::EvidenceRef;
    use crate::rhythm_explanation::RhythmEvidenceRef;
    match evidence {
        EvidenceRef::Artifact(artifact) => {
            bytes.push(0);
            push_digest(bytes, artifact.0);
        }
        EvidenceRef::SourceClaim(claim) => {
            bytes.push(1);
            push_digest(bytes, claim.0);
        }
        EvidenceRef::PatternAlternative(pattern) => {
            bytes.push(2);
            push_digest(bytes, pattern.0);
        }
        EvidenceRef::Rhythm(reference) => {
            bytes.push(3);
            let (tag, value) = match reference {
                RhythmEvidenceRef::Pattern(value) => (0, *value),
                RhythmEvidenceRef::Hit(value) => (1, *value),
                RhythmEvidenceRef::Family(value) => (2, *value),
                RhythmEvidenceRef::Tempo(value) => (3, *value),
                RhythmEvidenceRef::BeatPhase(value) => (4, *value),
            };
            bytes.push(tag);
            push_u64(bytes, value as u64);
        }
        EvidenceRef::NativeLocator {
            analyzer,
            version,
            locator,
        } => {
            bytes.push(4);
            push_bytes(bytes, analyzer.as_bytes());
            push_bytes(bytes, version.as_bytes());
            push_bytes(bytes, locator.as_bytes());
        }
    }
}

fn push_digest(bytes: &mut Vec<u8>, digest: ContentDigest) {
    bytes.push(match digest.algorithm {
        crate::artifact_catalog::DigestAlgorithm::Sha256 => 0,
        crate::artifact_catalog::DigestAlgorithm::Blake3 => 1,
        crate::artifact_catalog::DigestAlgorithm::StableNonCryptographic => 2,
    });
    bytes.extend_from_slice(&digest.bytes);
}

fn push_bytes(target: &mut Vec<u8>, value: &[u8]) {
    push_u64(target, value.len() as u64);
    target.extend_from_slice(value);
}

fn push_u64(target: &mut Vec<u8>, value: u64) {
    target.extend_from_slice(&value.to_le_bytes());
}

fn coverage_digest(field: &CoverageField) -> ContentDigest {
    let mut canonical = Vec::new();
    canonical.extend_from_slice(&field.origin_frame.to_le_bytes());
    canonical.extend_from_slice(&field.sample_rate.to_le_bytes());
    canonical.extend_from_slice(&field.channels.to_le_bytes());
    canonical.extend_from_slice(&field.frame_count.to_le_bytes());
    canonical.extend_from_slice(&(field.recipe.fft_size as u64).to_le_bytes());
    canonical.extend_from_slice(&(field.recipe.hop_size as u64).to_le_bytes());
    canonical.extend_from_slice(&field.recipe.power_floor.to_bits().to_le_bytes());
    for values in [
        &field.source_power,
        &field.construction_power,
        &field.residual_power,
        &field.explained,
        &field.excess,
    ] {
        canonical.extend_from_slice(&(values.len() as u64).to_le_bytes());
        for value in values {
            canonical.extend_from_slice(&value.to_bits().to_le_bytes());
        }
    }
    sha256_content(RENDERED_DOMAIN, &[&canonical])
}

fn quantize_signed(value: f64, scale: f64) -> i64 {
    let scaled = (value * scale).round();
    if scaled <= i64::MIN as f64 {
        i64::MIN
    } else if scaled >= i64::MAX as f64 {
        i64::MAX
    } else {
        scaled as i64
    }
}

fn quantize_unsigned(value: f64, scale: f64) -> u64 {
    if !value.is_finite() {
        return u64::MAX;
    }
    let scaled = (value.max(0.0) * scale).round();
    if scaled >= u64::MAX as f64 {
        u64::MAX
    } else {
        scaled as u64
    }
}

#[derive(Debug)]
pub enum EvaluationError {
    Invalid(String),
    ProgramChangedSinceStructuralScore,
    NonFinitePcm {
        source_samples: u64,
        construction_samples: u64,
    },
    Coverage(CoverageError),
    Deprojection(DeprojectionError),
    Comparison(crate::comparison::ComparisonError),
    ComparisonRuntime(crate::comparison_runtime::ComparisonRuntimeError),
}

impl fmt::Display for EvaluationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => formatter.write_str(message),
            Self::ProgramChangedSinceStructuralScore => {
                formatter.write_str("source program changed after structural scoring")
            }
            Self::NonFinitePcm {
                source_samples,
                construction_samples,
            } => write!(
                formatter,
                "render comparison quarantined {source_samples} source and {construction_samples} construction samples"
            ),
            Self::Coverage(error) => error.fmt(formatter),
            Self::Deprojection(error) => error.fmt(formatter),
            Self::Comparison(error) => error.fmt(formatter),
            Self::ComparisonRuntime(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for EvaluationError {}

impl From<CoverageError> for EvaluationError {
    fn from(value: CoverageError) -> Self {
        Self::Coverage(value)
    }
}

impl From<DeprojectionError> for EvaluationError {
    fn from(value: DeprojectionError) -> Self {
        Self::Deprojection(value)
    }
}

impl From<crate::comparison::ComparisonError> for EvaluationError {
    fn from(value: crate::comparison::ComparisonError) -> Self {
        Self::Comparison(value)
    }
}

impl From<crate::comparison_runtime::ComparisonRuntimeError> for EvaluationError {
    fn from(value: crate::comparison_runtime::ComparisonRuntimeError) -> Self {
        Self::ComparisonRuntime(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::artifact_catalog::{ContentDigest, DigestAlgorithm};
    use crate::audio::{AudioFormat, ProjectAudio};
    use crate::comparison::render_comparison;
    use crate::deprojection_program::{
        Derivation, EditableTerm, EditableTermId, EditableTermKind, MaterialSpan,
        PatternExecutionSemantics, VoiceTerm,
    };
    use crate::explanation::RenderedExplanation;

    fn digest(byte: u8) -> ContentDigest {
        ContentDigest::new(DigestAlgorithm::Sha256, [byte; 32])
    }

    fn program(unresolved: bool) -> SourceProgram {
        let id = EditableTermId(digest(1));
        let kind = if unresolved {
            EditableTermKind::Pattern {
                source: "fam1 ~".into(),
                execution: PatternExecutionSemantics {
                    cycle: crate::sequencer::BeatDuration((crate::sequencer::PPQ * 4) as u64),
                    seed: 0,
                    initial_cycle_index: 0,
                },
                voices: BTreeMap::from([(
                    "fam1".into(),
                    VoiceTerm::UnresolvedFamily { family: 1 },
                )]),
            }
        } else {
            EditableTermKind::Notes {
                gestures: Vec::new(),
            }
        };
        SourceProgram::new(
            MaterialSpan {
                material_sha256: "11".repeat(32),
                start_frame: 0,
                frame_count: 32,
                sample_rate_hz: 8_000,
                channels: 1,
            },
            vec![EditableTerm {
                id,
                kind,
                evidence: Vec::new(),
                derivation: Derivation {
                    rule: "test".into(),
                    recipe: digest(2),
                    premises: Vec::new(),
                },
                description_bytes: 16,
                free_parameters: 1,
            }],
            vec![id],
        )
        .unwrap()
    }

    fn rendered(gain: f32) -> RenderedComparison {
        let format = AudioFormat::new(8_000, 1).unwrap();
        let source = (0..32)
            .map(|index| if index % 2 == 0 { 1.0 } else { -1.0 })
            .collect::<Vec<_>>();
        let construction = source.iter().map(|value| value * gain).collect();
        render_comparison(
            0,
            ProjectAudio::from_interleaved(format, source).unwrap(),
            RenderedExplanation {
                origin_frame: 0,
                audio: ProjectAudio::from_interleaved(format, construction).unwrap(),
            },
        )
        .unwrap()
    }

    #[test]
    fn structural_phase_has_no_fake_residual_and_penalizes_unresolved_voices() {
        let resolved = evaluate_structure(
            &program(false),
            ObservationFit::default(),
            0.5,
            StructuralScorePolicy::default(),
        )
        .unwrap();
        let unresolved = evaluate_structure(
            &program(true),
            ObservationFit::default(),
            0.5,
            StructuralScorePolicy::default(),
        )
        .unwrap();
        assert_eq!(resolved.unresolved_terms, 0);
        assert_eq!(unresolved.unresolved_terms, 1);
        assert!(resolved.objective < unresolved.objective);
        assert_eq!(resolved.rank.residual_parts_per_million, u64::MAX);
    }

    #[test]
    fn final_phase_uses_exact_rendered_residual_and_excess() {
        let program = program(false);
        let structural = evaluate_structure(
            &program,
            ObservationFit::default(),
            0.5,
            StructuralScorePolicy::default(),
        )
        .unwrap();
        let evaluation = evaluate_rendered(
            &program,
            structural,
            &rendered(2.0),
            CoverageRecipe {
                fft_size: 8,
                hop_size: 4,
                power_floor: 1.0e-12,
            },
            0.0,
            ScorePolicy::default(),
            &RenderCancellation::new(),
        )
        .unwrap();
        assert_eq!(evaluation.rank.residual_parts_per_million, 1_000_000);
        assert!(evaluation.rank.excess_parts_per_million > 0);
        assert_ne!(evaluation.digests.residual, evaluation.digests.source);
        assert_ne!(evaluation.digests.construction, evaluation.digests.source);
    }

    #[test]
    fn changed_program_cannot_reuse_a_structural_score() {
        let scored = program(false);
        let structural = evaluate_structure(
            &scored,
            ObservationFit::default(),
            0.5,
            StructuralScorePolicy::default(),
        )
        .unwrap();
        let changed = program(true);
        assert!(matches!(
            evaluate_rendered(
                &changed,
                structural,
                &rendered(1.0),
                CoverageRecipe {
                    fft_size: 8,
                    hop_size: 4,
                    power_floor: 1.0e-12,
                },
                0.0,
                ScorePolicy::default(),
                &RenderCancellation::new(),
            ),
            Err(EvaluationError::ProgramChangedSinceStructuralScore)
        ));
    }
}
