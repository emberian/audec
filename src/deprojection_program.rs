//! Model-neutral programs for turning measured audio into editable alternatives.
//!
//! This module composes existing evidence systems; it does not run a separator,
//! identify an instrument, choose a winning reading, or mutate the DAW. A
//! deprojection program is an immutable DAG from exact source material through
//! claims and native analyses to editable terms, rendered comparisons, and
//! residual/excess measurements. Project publication remains a command-layer
//! operation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::artifact_catalog::{sha256_content, ArtifactId, ContentDigest};
use crate::coverage::CoverageSummary;
use crate::curve_lang::{CurveExpr, LfoShape};
use crate::daw_render::RenderCancellation;
use crate::model_claim::{ClaimConfidenceKind, ModelClaimArtifact, ModelClaimBundle, ModelClaimId};
use crate::model_wire::{AdditivityDeclaration, ArtifactKind as WorkerArtifactKind};
use crate::pitch::{ModulationEvidence, PitchAnalysis};
use crate::rhythm::SampleSpan;
use crate::rhythm_explanation::{
    PatternAlternativeId, PatternExplanationRepresentation, PatternExplanationSet,
    RhythmEvidenceRef,
};
use crate::sequencer::BeatDuration;

const PLAN_DOMAIN: &[u8] = b"audec:deprojection-plan:v1";
const NODE_DOMAIN: &[u8] = b"audec:deprojection-node:v1";
const SOURCE_DOMAIN: &[u8] = b"audec:deprojection-source:v1";
const TERM_DOMAIN: &[u8] = b"audec:deprojection-term:v1";
const ALTERNATIVE_DOMAIN: &[u8] = b"audec:deprojection-alternative:v1";
const CANDIDATE_DOMAIN: &[u8] = b"audec:deprojection-candidate:v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceClaimId(pub ContentDigest);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeprojectionNodeId(pub ContentDigest);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EditableTermId(pub ContentDigest);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeprojectionAlternativeId(pub ContentDigest);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeprojectionCandidateId(pub ContentDigest);

/// Exact material geometry. The digest is over the decoded representation
/// supplied to analyzers, never a filename or mutable project-local handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterialSpan {
    pub material_sha256: String,
    pub start_frame: u64,
    pub frame_count: u64,
    pub sample_rate_hz: u32,
    pub channels: u16,
}

impl MaterialSpan {
    pub fn validate(&self) -> Result<(), DeprojectionError> {
        validate_sha256("material_sha256", &self.material_sha256)?;
        if self.frame_count == 0 || self.start_frame.checked_add(self.frame_count).is_none() {
            return Err(DeprojectionError::Invalid(
                "material span must be non-empty and non-overflowing".into(),
            ));
        }
        if self.sample_rate_hz == 0 || self.channels == 0 {
            return Err(DeprojectionError::Invalid(
                "material sample rate and channel count must be non-zero".into(),
            ));
        }
        Ok(())
    }

    pub fn end_frame(&self) -> u64 {
        self.start_frame + self.frame_count
    }

    pub fn content_id(&self) -> SourceClaimId {
        SourceClaimId(sha256_content(
            SOURCE_DOMAIN,
            &[
                self.material_sha256.as_bytes(),
                &self.start_frame.to_le_bytes(),
                &self.frame_count.to_le_bytes(),
                &self.sample_rate_hz.to_le_bytes(),
                &self.channels.to_le_bytes(),
                b"literal-material",
            ],
        ))
    }
}

/// Mathematical contract of one source estimate. Labels such as `drums` or
/// `vocals` remain metadata on the claim and never choose this enum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceEstimateContract {
    /// Exact citation into the input material; no source separation implied.
    LiteralMaterial,
    /// Outputs were jointly produced and measured to sum to the input.
    JointAdditive {
        maximum_error_parts_per_million: u32,
    },
    /// One target plus a named residual reconstructs the input.
    AdditiveTargetWithResidual {
        residual_output: String,
        maximum_error_parts_per_million: u32,
    },
    /// Estimates may explain the same energy and must not be mixer-summed.
    Overlapping,
    /// The waveform is a plausible generated recreation, not extracted PCM.
    Generative,
    /// Event maps, embeddings, labels, or other non-audio evidence.
    Measurement,
}

impl SourceEstimateContract {
    pub fn can_join_linear_construction(&self) -> bool {
        matches!(
            self,
            Self::LiteralMaterial
                | Self::JointAdditive { .. }
                | Self::AdditiveTargetWithResidual { .. }
        )
    }
}

/// One immutable analyzer output in project-frame coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceClaim {
    pub id: SourceClaimId,
    pub source: MaterialSpan,
    pub artifact: Option<ArtifactId>,
    pub output_digest: ContentDigest,
    pub producer: String,
    pub producer_recipe: ContentDigest,
    pub authored_label: Option<String>,
    pub contract: SourceEstimateContract,
    /// Exact worker claim/output pair when the producer was isolated ML.
    pub worker_claim: Option<(ModelClaimId, String)>,
}

impl SourceClaim {
    pub fn literal(
        source: MaterialSpan,
        output_digest: ContentDigest,
    ) -> Result<Self, DeprojectionError> {
        source.validate()?;
        let id = source.content_id();
        Ok(Self {
            id,
            source,
            artifact: None,
            output_digest,
            producer: "audec.material".into(),
            producer_recipe: sha256_content(b"audec:literal-material:v1", &[]),
            authored_label: None,
            contract: SourceEstimateContract::LiteralMaterial,
            worker_claim: None,
        })
    }

    /// Bridge a verified worker publication into the model-neutral graph. The
    /// caller supplies the catalog artifact/digest because paths inside a job
    /// sandbox are not durable project identities.
    pub fn from_model_output(
        claim: &ModelClaimBundle,
        output_name: &str,
        artifact: ArtifactId,
        output_digest: ContentDigest,
        maximum_error_parts_per_million: Option<u32>,
    ) -> Result<Self, DeprojectionError> {
        let output = claim
            .artifact(output_name)
            .ok_or_else(|| DeprojectionError::MissingWorkerOutput(output_name.to_owned()))?;
        let contract = match &output.descriptor.additivity {
            AdditivityDeclaration::LinearSum => SourceEstimateContract::JointAdditive {
                maximum_error_parts_per_million: maximum_error_parts_per_million.unwrap_or(0),
            },
            AdditivityDeclaration::LinearSumWithResidual { residual_artifact } => {
                SourceEstimateContract::AdditiveTargetWithResidual {
                    residual_output: residual_artifact.clone(),
                    maximum_error_parts_per_million: maximum_error_parts_per_million.unwrap_or(0),
                }
            }
            AdditivityDeclaration::OverlappingEstimates => SourceEstimateContract::Overlapping,
            AdditivityDeclaration::Generative => SourceEstimateContract::Generative,
            AdditivityDeclaration::NonAudio => SourceEstimateContract::Measurement,
        };
        if !matches!(&contract, SourceEstimateContract::Measurement)
            && !matches!(output.descriptor.kind, WorkerArtifactKind::Audio)
        {
            return Err(DeprojectionError::Invalid(format!(
                "worker output {output_name} declares audio additivity but is {:?}",
                output.descriptor.kind
            )));
        }
        let source = MaterialSpan {
            material_sha256: claim.source.material_sha256.clone(),
            start_frame: claim.source.start_frame,
            frame_count: claim.source.frame_count,
            sample_rate_hz: claim.source.sample_rate_hz,
            channels: claim.source.channels,
        };
        source.validate()?;
        let recipe = parse_sha256_digest(&claim.model_manifest_sha256)?;
        let id = SourceClaimId(sha256_content(
            SOURCE_DOMAIN,
            &[
                claim.id.as_str().as_bytes(),
                output_name.as_bytes(),
                &output_digest.bytes,
            ],
        ));
        Ok(Self {
            id,
            source,
            artifact: Some(artifact),
            output_digest,
            producer: claim.runtime.worker_name.clone(),
            producer_recipe: recipe,
            authored_label: Some(output.output_name.clone()),
            contract,
            worker_claim: Some((claim.id.clone(), output_name.to_owned())),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EvidenceRef {
    Artifact(ArtifactId),
    SourceClaim(SourceClaimId),
    PatternAlternative(PatternAlternativeId),
    Rhythm(RhythmEvidenceRef),
    NativeLocator {
        analyzer: String,
        version: String,
        locator: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Derivation {
    pub rule: String,
    pub recipe: ContentDigest,
    pub premises: Vec<EvidenceRef>,
}

impl Derivation {
    pub fn normalized(mut self) -> Self {
        self.premises.sort();
        self.premises.dedup();
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CurveTarget {
    Gain,
    PitchCents,
    Pan,
    Brightness,
    FilterCutoff,
    StereoWidth,
    TailLevel,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NoteGesture {
    pub start_frame: u64,
    pub duration_frames: u64,
    pub midi_key: u8,
    pub velocity: f32,
    pub pitch_curve_cents: Vec<(u64, f32)>,
}

/// Execution facts retained with a canonical pattern explanation. Search and
/// promotion therefore evaluate the same cycle and initial alternation state;
/// `seed` remains explicit even though today's pattern language rolls no dice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PatternExecutionSemantics {
    pub cycle: BeatDuration,
    pub seed: u64,
    pub initial_cycle_index: u64,
}

/// Editable target languages. Exact audio is deliberately a distinct escape
/// hatch: it cannot be mistaken for successful symbolic decompilation.
#[derive(Clone, Debug, PartialEq)]
pub enum EditableTermKind {
    SampleSlice {
        source: SourceClaimId,
        span: SampleSpan,
        onset_offset_frames: u64,
    },
    Pattern {
        source: String,
        execution: PatternExecutionSemantics,
        /// Symbolic slots are resolved to project instruments only at the
        /// compiler/promotion boundary. Search never allocates project IDs.
        voices: BTreeMap<String, VoiceTerm>,
    },
    Curve {
        target: CurveTarget,
        expression: CurveExpr,
        source_span: (u64, u64),
    },
    Notes {
        gestures: Vec<NoteGesture>,
    },
    PresetCandidate {
        format: String,
        artifact: ArtifactId,
        editable_parameters: Vec<String>,
    },
    ExactAudioReference {
        source: SourceClaimId,
        span: SampleSpan,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoiceTerm {
    /// Honest intermediate state while family evidence has no executable
    /// sample, synth, or claim attached. Compilation refuses this voice.
    UnresolvedFamily {
        family: usize,
    },
    Sample(EditableTermId),
    AudioClaim(SourceClaimId),
    Preset(ArtifactId),
}

#[derive(Clone, Debug, PartialEq)]
pub struct EditableTerm {
    pub id: EditableTermId,
    pub kind: EditableTermKind,
    pub evidence: Vec<EvidenceRef>,
    pub derivation: Derivation,
    /// Canonical representation length used by minimum-description ranking.
    pub description_bytes: u64,
    /// Free knobs left to fit. This is separate from description bytes so a
    /// tiny opaque preset does not appear simpler than an explicit pattern.
    pub free_parameters: u32,
}

impl EditableTerm {
    pub fn pattern_from_explanation(
        alternative: &crate::rhythm_explanation::PatternExplanation,
    ) -> Option<Self> {
        let PatternExplanationRepresentation::Term(term) = &alternative.representation else {
            return None;
        };
        let mut evidence = vec![EvidenceRef::PatternAlternative(alternative.id)];
        evidence.extend(
            alternative
                .evidence
                .iter()
                .copied()
                .map(EvidenceRef::Rhythm),
        );
        evidence.sort();
        evidence.dedup();
        // Retain the language-owned canonical term, not presentation text
        // carried alongside the explanation.
        let canonical_source = crate::pattern_lang::print(&term.expr);
        let id = EditableTermId(sha256_content(
            TERM_DOMAIN,
            &[
                b"pattern",
                canonical_source.as_bytes(),
                &alternative.id.0.bytes,
            ],
        ));
        Some(Self {
            id,
            kind: EditableTermKind::Pattern {
                source: canonical_source,
                execution: PatternExecutionSemantics {
                    cycle: term.pattern.length,
                    seed: 0,
                    initial_cycle_index: 0,
                },
                voices: alternative
                    .families
                    .iter()
                    .map(|(family, binding)| {
                        (
                            binding.clone(),
                            VoiceTerm::UnresolvedFamily { family: *family },
                        )
                    })
                    .collect(),
            },
            evidence: evidence.clone(),
            derivation: Derivation {
                rule: "rhythm.minimum-description-pattern.v1".into(),
                recipe: alternative.id.0,
                premises: evidence,
            },
            description_bytes: alternative.description.description_bytes,
            free_parameters: alternative.families.len() as u32,
        })
    }

    pub fn exact_audio_from_explanation(
        alternative: &crate::rhythm_explanation::PatternExplanation,
        source: SourceClaimId,
    ) -> Option<Self> {
        let PatternExplanationRepresentation::ExactAudio(fallback) = &alternative.representation
        else {
            return None;
        };
        let evidence = vec![EvidenceRef::PatternAlternative(alternative.id)];
        let id = EditableTermId(sha256_content(
            TERM_DOMAIN,
            &[
                b"exact-audio",
                &source.0.bytes,
                &fallback.source_span.start.to_le_bytes(),
                &fallback.source_span.end.to_le_bytes(),
                &alternative.id.0.bytes,
            ],
        ));
        Some(Self {
            id,
            kind: EditableTermKind::ExactAudioReference {
                source,
                span: fallback.source_span,
            },
            evidence: evidence.clone(),
            derivation: Derivation {
                rule: "rhythm.exact-audio-fallback.v1".into(),
                recipe: alternative.id.0,
                premises: evidence,
            },
            description_bytes: fallback.estimated_literal_bytes,
            free_parameters: 0,
        })
    }
}

/// A source program is the explicit constructive sum submitted to the one DAW
/// renderer. Residual never appears here: it is always derived afterward from
/// `source - render(program)` so literal audio cannot manufacture a perfect
/// score by including its own residual.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceProgram {
    pub source: MaterialSpan,
    pub terms: BTreeMap<EditableTermId, EditableTerm>,
    pub roots: Vec<EditableTermId>,
    pub evidence: Vec<EvidenceRef>,
    pub derivations: Vec<Derivation>,
}

impl SourceProgram {
    pub fn new(
        source: MaterialSpan,
        terms: Vec<EditableTerm>,
        mut roots: Vec<EditableTermId>,
    ) -> Result<Self, DeprojectionError> {
        source.validate()?;
        let mut by_id = BTreeMap::new();
        for term in terms {
            if by_id.insert(term.id, term).is_some() {
                return Err(DeprojectionError::Invalid(
                    "source program contains a duplicate term identity".into(),
                ));
            }
        }
        roots.sort();
        roots.dedup();
        if roots.is_empty() || roots.iter().any(|root| !by_id.contains_key(root)) {
            return Err(DeprojectionError::Invalid(
                "source program roots must name retained terms".into(),
            ));
        }
        let mut evidence = by_id
            .values()
            .flat_map(|term| term.evidence.iter().cloned())
            .collect::<Vec<_>>();
        evidence.sort();
        evidence.dedup();
        let derivations = by_id
            .values()
            .map(|term| term.derivation.clone().normalized())
            .collect();
        Ok(Self {
            source,
            terms: by_id,
            roots,
            evidence,
            derivations,
        })
    }

    /// Reasons the program cannot yet lower into a frozen DAW schedule.
    /// Unresolved observations remain durable; they never turn into silence.
    pub fn compile_refusals(&self) -> Vec<ProgramCompileRefusal> {
        let mut refusals = Vec::new();
        for term in self.terms.values() {
            if let EditableTermKind::Pattern { voices, .. } = &term.kind {
                for (binding, voice) in voices {
                    if let VoiceTerm::UnresolvedFamily { family } = voice {
                        refusals.push(ProgramCompileRefusal::UnresolvedPatternVoice {
                            term: term.id,
                            binding: binding.clone(),
                            family: *family,
                        });
                    }
                }
            }
        }
        refusals.sort();
        refusals
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProgramCompileRefusal {
    UnresolvedPatternVoice {
        term: EditableTermId,
        binding: String,
        family: usize,
    },
}

/// Evaluate a measured curve in physical source time. This is the appropriate
/// path for Hz-valued evidence; `curve_lang::compile_curve` intentionally uses
/// nominal musical seconds and therefore cannot preserve measured 6 Hz motion
/// across tempo changes without a caller-supplied warp.
pub fn evaluate_curve_at_source_frame(
    expression: &CurveExpr,
    span: (u64, u64),
    frame: u64,
    sample_rate_hz: u32,
) -> Result<f64, DeprojectionError> {
    if span.0 >= span.1 || sample_rate_hz == 0 || frame < span.0 || frame > span.1 {
        return Err(DeprojectionError::Invalid(
            "physical curve evaluation needs an ordered span, non-zero rate, and in-span frame"
                .into(),
        ));
    }
    let elapsed = (frame - span.0) as f64 / f64::from(sample_rate_hz);
    let duration = (span.1 - span.0) as f64 / f64::from(sample_rate_hz);
    crate::curve_lang::evaluate_curve(expression, elapsed, duration)
        .map_err(|error| DeprojectionError::Invalid(error.to_string()))
}

/// Turn measured pitch modulation into editable curve candidates while
/// retaining the observation locator. No candidate is selected and no LFO is
/// claimed to have existed in the source patch.
pub fn curve_terms_from_pitch(pitch: &PitchAnalysis, analyzer_version: &str) -> Vec<EditableTerm> {
    let mut terms = Vec::new();
    for (track_index, track) in pitch.tracks.iter().enumerate() {
        for (modulation_index, modulation) in track.modulation.iter().enumerate() {
            let (target, expression, span, rule) = match modulation {
                ModulationEvidence::Vibrato {
                    start_offset_frames,
                    end_offset_frames,
                    rate_hz,
                    extent_semitones,
                    ..
                } => (
                    CurveTarget::PitchCents,
                    CurveExpr::Lfo {
                        shape: LfoShape::Sine,
                        rate_hz: f64::from(*rate_hz),
                        depth: f64::from(*extent_semitones) * 50.0,
                        phase: 0.0,
                    },
                    (*start_offset_frames, *end_offset_frames),
                    "pitch.vibrato-to-lfo.v1",
                ),
                ModulationEvidence::Glide {
                    start_offset_frames,
                    end_offset_frames,
                    extent_semitones,
                    direction,
                    ..
                } => {
                    let cents = f64::from(*extent_semitones) * 100.0;
                    let (from, to) = match direction {
                        crate::pitch::GlideDirection::Rising => (0.0, cents),
                        crate::pitch::GlideDirection::Falling => (0.0, -cents),
                    };
                    (
                        CurveTarget::PitchCents,
                        CurveExpr::Line { from, to },
                        (*start_offset_frames, *end_offset_frames),
                        "pitch.glide-to-line.v1",
                    )
                }
            };
            if span.0 >= span.1 {
                continue;
            }
            let locator = format!("track/{track_index}/modulation/{modulation_index}");
            let evidence = vec![EvidenceRef::NativeLocator {
                analyzer: "audec.pitch".into(),
                version: analyzer_version.to_owned(),
                locator: locator.clone(),
            }];
            let canonical = crate::curve_lang::print(&expression);
            let id = EditableTermId(sha256_content(
                TERM_DOMAIN,
                &[
                    b"curve",
                    canonical.as_bytes(),
                    &span.0.to_le_bytes(),
                    &span.1.to_le_bytes(),
                    locator.as_bytes(),
                ],
            ));
            terms.push(EditableTerm {
                id,
                kind: EditableTermKind::Curve {
                    target,
                    expression,
                    source_span: span,
                },
                evidence: evidence.clone(),
                derivation: Derivation {
                    rule: rule.into(),
                    recipe: sha256_content(rule.as_bytes(), &[analyzer_version.as_bytes()]),
                    premises: evidence,
                },
                description_bytes: canonical.len() as u64,
                free_parameters: 3,
            });
        }
    }
    terms.sort_by_key(|term| term.id);
    terms
}

/// Provenance of a program before it has been rendered and compared. This is
/// an adapter identity, not an assertion that the named producer recovered a
/// physical source.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CandidateOrigin {
    NativeRhythm {
        alternative: PatternAlternativeId,
    },
    NativePitch {
        analyzer_version: String,
        track: usize,
        modulation: usize,
    },
    ModelOutput {
        claim: ModelClaimId,
        output_name: String,
    },
}

impl CandidateOrigin {
    fn canonical_bytes(&self) -> Vec<u8> {
        match self {
            Self::NativeRhythm { alternative } => {
                let mut bytes = b"native-rhythm\0".to_vec();
                bytes.extend_from_slice(&alternative.0.bytes);
                bytes
            }
            Self::NativePitch {
                analyzer_version,
                track,
                modulation,
            } => format!("native-pitch\0{analyzer_version}\0{track}\0{modulation}").into_bytes(),
            Self::ModelOutput { claim, output_name } => {
                format!("model-output\0{}\0{output_name}", claim.as_str()).into_bytes()
            }
        }
    }
}

/// Integer-only policy for provisional, pre-render ordering. Final ordering
/// still uses audible residual/excess through `score_deprojection`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StructuralScorePolicy {
    pub description_unit_per_byte: u64,
    pub free_parameter_units: u64,
    pub unresolved_voice_units: u64,
    pub exact_audio_units: u64,
    pub generative_claim_units: u64,
    pub evidence_credit_units_per_ppm: u64,
}

impl Default for StructuralScorePolicy {
    fn default() -> Self {
        Self {
            description_unit_per_byte: 1_000,
            free_parameter_units: 8_000,
            unresolved_voice_units: 24_000,
            exact_audio_units: 1_000_000,
            generative_claim_units: 250_000,
            evidence_credit_units_per_ppm: 1,
        }
    }
}

/// Deterministic structural prior. `evidence_support_ppm` is deliberately a
/// bounded integer: analyzer floats are quantized once at their adapter edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StructuralScore {
    pub objective_units: u128,
    pub description_bytes: u64,
    pub free_parameters: u32,
    pub unresolved_voices: u32,
    pub exact_audio_terms: u32,
    pub generative_claims: u32,
    pub evidence_support_ppm: u32,
}

pub fn score_program_structure(
    program: &SourceProgram,
    source_claims: &[SourceClaim],
    evidence_support_ppm: u32,
    policy: StructuralScorePolicy,
) -> Result<StructuralScore, DeprojectionError> {
    if evidence_support_ppm > 1_000_000 {
        return Err(DeprojectionError::Invalid(
            "structural evidence support must be in 0..=1,000,000 ppm".into(),
        ));
    }
    let description_bytes = program
        .terms
        .values()
        .try_fold(0_u64, |sum, term| sum.checked_add(term.description_bytes))
        .ok_or_else(|| DeprojectionError::Invalid("structural description size overflow".into()))?;
    let free_parameters = program
        .terms
        .values()
        .try_fold(0_u32, |sum, term| sum.checked_add(term.free_parameters))
        .ok_or_else(|| DeprojectionError::Invalid("structural parameter count overflow".into()))?;
    let unresolved_voices = program
        .compile_refusals()
        .len()
        .try_into()
        .map_err(|_| DeprojectionError::Invalid("unresolved voice count overflow".into()))?;
    let exact_audio_terms = program
        .terms
        .values()
        .filter(|term| matches!(term.kind, EditableTermKind::ExactAudioReference { .. }))
        .count()
        .try_into()
        .map_err(|_| DeprojectionError::Invalid("exact-audio term count overflow".into()))?;
    let generative_claims = source_claims
        .iter()
        .filter(|claim| matches!(claim.contract, SourceEstimateContract::Generative))
        .count()
        .try_into()
        .map_err(|_| DeprojectionError::Invalid("generative claim count overflow".into()))?;

    let weighted = |count: u128, weight: u64| {
        count.checked_mul(u128::from(weight)).ok_or_else(|| {
            DeprojectionError::Invalid("structural score multiplication overflow".into())
        })
    };
    let components = [
        weighted(
            u128::from(description_bytes),
            policy.description_unit_per_byte,
        )?,
        weighted(u128::from(free_parameters), policy.free_parameter_units)?,
        weighted(u128::from(unresolved_voices), policy.unresolved_voice_units)?,
        weighted(u128::from(exact_audio_terms), policy.exact_audio_units)?,
        weighted(u128::from(generative_claims), policy.generative_claim_units)?,
    ];
    let positive = components.into_iter().try_fold(0_u128, |sum, component| {
        sum.checked_add(component)
            .ok_or_else(|| DeprojectionError::Invalid("structural score addition overflow".into()))
    })?;
    let evidence_credit = weighted(
        u128::from(evidence_support_ppm),
        policy.evidence_credit_units_per_ppm,
    )?;
    Ok(StructuralScore {
        objective_units: positive.saturating_sub(evidence_credit),
        description_bytes,
        free_parameters,
        unresolved_voices,
        exact_audio_terms,
        generative_claims,
        evidence_support_ppm,
    })
}

/// A pre-render source program. Structural rank is useful for bounded search,
/// but cannot replace the later residual/excess comparison.
#[derive(Clone, Debug, PartialEq)]
pub struct DeprojectionCandidate {
    pub id: DeprojectionCandidateId,
    pub label: String,
    pub origin: CandidateOrigin,
    pub program: SourceProgram,
    pub source_claims: Vec<SourceClaim>,
    pub structural_score: StructuralScore,
    pub caveats: Vec<String>,
}

impl DeprojectionCandidate {
    pub fn new(
        label: String,
        origin: CandidateOrigin,
        program: SourceProgram,
        mut source_claims: Vec<SourceClaim>,
        evidence_support_ppm: u32,
        policy: StructuralScorePolicy,
        caveats: Vec<String>,
    ) -> Result<Self, DeprojectionError> {
        if label.trim().is_empty() {
            return Err(DeprojectionError::Invalid(
                "a deprojection candidate needs a label".into(),
            ));
        }
        source_claims.sort_by_key(|claim| claim.id);
        if source_claims
            .windows(2)
            .any(|pair| pair[0].id == pair[1].id && pair[0] != pair[1])
        {
            return Err(DeprojectionError::Invalid(
                "candidate source-claim identity collision".into(),
            ));
        }
        source_claims.dedup_by_key(|claim| claim.id);
        if source_claims.is_empty() {
            return Err(DeprojectionError::Invalid(
                "a deprojection candidate needs at least one source claim".into(),
            ));
        }
        if source_claims
            .iter()
            .any(|claim| claim.source != program.source)
        {
            return Err(DeprojectionError::Invalid(
                "candidate source claims must address the program material span".into(),
            ));
        }
        let structural_score =
            score_program_structure(&program, &source_claims, evidence_support_ppm, policy)?;
        let origin_bytes = origin.canonical_bytes();
        let mut roots = Vec::with_capacity(program.roots.len() * 32);
        for root in &program.roots {
            roots.extend_from_slice(&root.0.bytes);
        }
        let source_id = program.source.content_id();
        let id = DeprojectionCandidateId(sha256_content(
            CANDIDATE_DOMAIN,
            &[&source_id.0.bytes, &origin_bytes, &roots],
        ));
        Ok(Self {
            id,
            label,
            origin,
            program,
            source_claims,
            structural_score,
            caveats,
        })
    }
}

pub fn rank_candidates(candidates: &mut [DeprojectionCandidate]) {
    candidates.sort_by(|left, right| {
        left.structural_score
            .cmp(&right.structural_score)
            .then_with(|| left.id.cmp(&right.id))
    });
}

/// Build one candidate per retained rhythm explanation. Exact-audio fallbacks
/// remain in the list and receive an explicit structural surcharge.
pub fn candidates_from_rhythm_explanations(
    source: MaterialSpan,
    literal_source: SourceClaim,
    explanations: &PatternExplanationSet,
    policy: StructuralScorePolicy,
) -> Result<Vec<DeprojectionCandidate>, DeprojectionError> {
    if literal_source.source != source {
        return Err(DeprojectionError::Invalid(
            "rhythm candidate source claim does not address the requested material span".into(),
        ));
    }
    let mut candidates = Vec::new();
    for alternative in &explanations.alternatives {
        let term = EditableTerm::pattern_from_explanation(alternative)
            .or_else(|| EditableTerm::exact_audio_from_explanation(alternative, literal_source.id))
            .expect("pattern explanation representations are exhaustive");
        let program = SourceProgram::new(source.clone(), vec![term.clone()], vec![term.id])?;
        let support = support_ppm(alternative.fit.combined_fit)?;
        let label = match &alternative.representation {
            PatternExplanationRepresentation::Term(term) => term.source.clone(),
            PatternExplanationRepresentation::ExactAudio(_) => "Exact rhythm audio".into(),
        };
        candidates.push(DeprojectionCandidate::new(
            label,
            CandidateOrigin::NativeRhythm {
                alternative: alternative.id,
            },
            program,
            vec![literal_source.clone()],
            support,
            policy,
            Vec::new(),
        )?);
    }
    rank_candidates(&mut candidates);
    Ok(candidates)
}

/// Build one independent candidate per measured pitch modulation. Independent
/// observations are not silently fused into one automation hypothesis.
pub fn candidates_from_pitch(
    source: MaterialSpan,
    source_claim: SourceClaim,
    pitch: &PitchAnalysis,
    analyzer_version: &str,
    policy: StructuralScorePolicy,
) -> Result<Vec<DeprojectionCandidate>, DeprojectionError> {
    if source_claim.source != source {
        return Err(DeprojectionError::Invalid(
            "pitch candidate source claim does not address the requested material span".into(),
        ));
    }
    let terms = curve_terms_from_pitch(pitch, analyzer_version);
    let mut candidates = Vec::with_capacity(terms.len());
    for term in terms {
        let EvidenceRef::NativeLocator { locator, .. } = &term.evidence[0] else {
            unreachable!("native pitch terms retain their locator")
        };
        let mut parts = locator.split('/');
        let track = parts
            .nth(1)
            .and_then(|part| part.parse::<usize>().ok())
            .ok_or_else(|| {
                DeprojectionError::Invalid("native pitch term has a malformed track locator".into())
            })?;
        let modulation = parts
            .nth(1)
            .and_then(|part| part.parse::<usize>().ok())
            .ok_or_else(|| {
                DeprojectionError::Invalid(
                    "native pitch term has a malformed modulation locator".into(),
                )
            })?;
        let support = modulation_support(&pitch.tracks[track].modulation[modulation]);
        let program = SourceProgram::new(source.clone(), vec![term.clone()], vec![term.id])?;
        candidates.push(DeprojectionCandidate::new(
            format!("Pitch modulation {}:{}", track + 1, modulation + 1),
            CandidateOrigin::NativePitch {
                analyzer_version: analyzer_version.to_owned(),
                track,
                modulation,
            },
            program,
            vec![source_claim.clone()],
            support_ppm(support)?,
            policy,
            vec!["Editable curve candidate; no original modulation source is asserted".into()],
        )?);
    }
    rank_candidates(&mut candidates);
    Ok(candidates)
}

/// Publication metadata needed to bridge a verified worker artifact into the
/// source-program layer. Catalog identity stays supplied by the catalog; the
/// worker sandbox path never becomes a durable reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublishedModelOutput {
    pub artifact: ArtifactId,
    pub output_digest: ContentDigest,
    pub maximum_error_parts_per_million: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelCandidateRefusal {
    EvidenceOnlyArtifact { kind: WorkerArtifactKind },
}

#[derive(Clone, Debug, PartialEq)]
pub struct AdaptedModelCandidate {
    pub source_claim: SourceClaim,
    pub candidate: Option<DeprojectionCandidate>,
    pub refusal: Option<ModelCandidateRefusal>,
}

/// Adapt one generic model output without decoding model-specific payloads.
/// Audio becomes an explicit audio-reference program, presets become editable
/// preset terms, and every other kind remains a typed evidence-only claim.
pub fn candidate_from_model_output(
    claim: &ModelClaimBundle,
    output_name: &str,
    published: PublishedModelOutput,
    policy: StructuralScorePolicy,
) -> Result<AdaptedModelCandidate, DeprojectionError> {
    let output = claim
        .artifact(output_name)
        .ok_or_else(|| DeprojectionError::MissingWorkerOutput(output_name.to_owned()))?;
    let source_claim = SourceClaim::from_model_output(
        claim,
        output_name,
        published.artifact,
        published.output_digest,
        published.maximum_error_parts_per_million,
    )?;
    let evidence = vec![
        EvidenceRef::Artifact(published.artifact),
        EvidenceRef::SourceClaim(source_claim.id),
    ];
    let maybe_term = match &output.descriptor.kind {
        WorkerArtifactKind::Audio => {
            let frame_count: usize = source_claim.source.frame_count.try_into().map_err(|_| {
                DeprojectionError::Invalid("model audio span does not fit this platform".into())
            })?;
            Some(EditableTerm {
                id: EditableTermId(sha256_content(
                    TERM_DOMAIN,
                    &[b"model-audio", &source_claim.id.0.bytes],
                )),
                kind: EditableTermKind::ExactAudioReference {
                    source: source_claim.id,
                    span: SampleSpan {
                        start: 0,
                        end: frame_count,
                    },
                },
                evidence: evidence.clone(),
                derivation: Derivation {
                    rule: "model.audio-output-reference.v1".into(),
                    recipe: source_claim.producer_recipe,
                    premises: evidence,
                },
                description_bytes: output.descriptor.byte_len,
                free_parameters: 0,
            })
        }
        WorkerArtifactKind::Preset => Some(EditableTerm {
            id: EditableTermId(sha256_content(
                TERM_DOMAIN,
                &[b"model-preset", &source_claim.id.0.bytes],
            )),
            kind: EditableTermKind::PresetCandidate {
                format: output.descriptor.media_type.clone(),
                artifact: published.artifact,
                editable_parameters: Vec::new(),
            },
            evidence: evidence.clone(),
            derivation: Derivation {
                rule: "model.preset-output-reference.v1".into(),
                recipe: source_claim.producer_recipe,
                premises: evidence,
            },
            description_bytes: output.descriptor.byte_len,
            free_parameters: 0,
        }),
        kind => {
            return Ok(AdaptedModelCandidate {
                source_claim,
                candidate: None,
                refusal: Some(ModelCandidateRefusal::EvidenceOnlyArtifact { kind: kind.clone() }),
            })
        }
    };
    let term = maybe_term.expect("audio and preset model outputs create terms");
    let program = SourceProgram::new(
        source_claim.source.clone(),
        vec![term.clone()],
        vec![term.id],
    )?;
    let candidate = DeprojectionCandidate::new(
        output.output_name.clone(),
        CandidateOrigin::ModelOutput {
            claim: claim.id.clone(),
            output_name: output_name.to_owned(),
        },
        program,
        vec![source_claim.clone()],
        model_output_support_ppm(output)?,
        policy,
        vec![format!(
            "Model-authored label; {} is not asserted as a physical source identity",
            output.output_name
        )],
    )?;
    Ok(AdaptedModelCandidate {
        source_claim,
        candidate: Some(candidate),
        refusal: None,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromotionIntent {
    Reconstruction,
    Pattern {
        term: EditableTermId,
        place_at_frame: Option<u64>,
    },
    Curve {
        term: EditableTermId,
        parameter_hint: Option<String>,
    },
}

/// UI-neutral handoff to the command/controller boundary. It allocates no
/// project IDs and is rejected if its optimistic revision or term identity is
/// stale at application time.
#[derive(Clone, Debug, PartialEq)]
pub enum DeprojectionPromotionRequest {
    Reconstruction {
        candidate: DeprojectionCandidateId,
        expected_project_revision: u64,
        program: SourceProgram,
    },
    Pattern {
        candidate: DeprojectionCandidateId,
        expected_project_revision: u64,
        term: EditableTerm,
        place_at_frame: Option<u64>,
    },
    Curve {
        candidate: DeprojectionCandidateId,
        expected_project_revision: u64,
        term: EditableTerm,
        parameter_hint: Option<String>,
    },
}

impl DeprojectionCandidate {
    pub fn promotion_request(
        &self,
        expected_project_revision: u64,
        intent: PromotionIntent,
    ) -> Result<DeprojectionPromotionRequest, DeprojectionError> {
        match intent {
            PromotionIntent::Reconstruction => Ok(DeprojectionPromotionRequest::Reconstruction {
                candidate: self.id,
                expected_project_revision,
                program: self.program.clone(),
            }),
            PromotionIntent::Pattern {
                term,
                place_at_frame,
            } => {
                let term = self
                    .program
                    .terms
                    .get(&term)
                    .ok_or(DeprojectionError::UnknownPromotionTerm(term))?;
                if !matches!(term.kind, EditableTermKind::Pattern { .. }) {
                    return Err(DeprojectionError::WrongPromotionTermKind {
                        term: term.id,
                        expected: "pattern",
                    });
                }
                Ok(DeprojectionPromotionRequest::Pattern {
                    candidate: self.id,
                    expected_project_revision,
                    term: term.clone(),
                    place_at_frame,
                })
            }
            PromotionIntent::Curve {
                term,
                parameter_hint,
            } => {
                let term = self
                    .program
                    .terms
                    .get(&term)
                    .ok_or(DeprojectionError::UnknownPromotionTerm(term))?;
                if !matches!(term.kind, EditableTermKind::Curve { .. }) {
                    return Err(DeprojectionError::WrongPromotionTermKind {
                        term: term.id,
                        expected: "curve",
                    });
                }
                Ok(DeprojectionPromotionRequest::Curve {
                    candidate: self.id,
                    expected_project_revision,
                    term: term.clone(),
                    parameter_hint,
                })
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScorePolicy {
    pub residual_weight: f64,
    pub excess_weight: f64,
    pub description_byte_weight: f64,
    pub free_parameter_weight: f64,
    pub assumption_weight: f64,
    pub evidence_credit: f64,
}

impl Default for ScorePolicy {
    fn default() -> Self {
        Self {
            residual_weight: 1.0,
            excess_weight: 1.0,
            description_byte_weight: 1.0 / 8_192.0,
            free_parameter_weight: 1.0 / 128.0,
            assumption_weight: 1.0,
            evidence_credit: 0.1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeprojectionScore {
    pub residual_ratio: f64,
    pub excess_ratio: f64,
    pub description_bytes: u64,
    pub free_parameters: u32,
    pub evidence_support: f64,
    pub assumption_penalty: f64,
    /// Minimum is better. It is a ranking objective, never probability or
    /// correctness, and must be shown alongside audible residual/excess.
    pub objective: f64,
}

pub fn score_deprojection(
    coverage: CoverageSummary,
    description_bytes: u64,
    free_parameters: u32,
    evidence_support: f64,
    assumption_penalty: f64,
    policy: ScorePolicy,
) -> Result<DeprojectionScore, DeprojectionError> {
    let values = [
        coverage.source_power,
        coverage.residual_power,
        coverage.excess_energy_ratio,
        evidence_support,
        assumption_penalty,
        policy.residual_weight,
        policy.excess_weight,
        policy.description_byte_weight,
        policy.free_parameter_weight,
        policy.assumption_weight,
        policy.evidence_credit,
    ];
    if values.iter().any(|value| !value.is_finite())
        || coverage.source_power < 0.0
        || coverage.residual_power < 0.0
        || coverage.excess_energy_ratio < 0.0
        || !(0.0..=1.0).contains(&evidence_support)
        || assumption_penalty < 0.0
    {
        return Err(DeprojectionError::Invalid(
            "score inputs must be finite and within their declared domains".into(),
        ));
    }
    let residual_ratio =
        coverage.residual_power / coverage.source_power.max(f64::from(f32::MIN_POSITIVE));
    let excess_ratio = coverage.excess_energy_ratio;
    let objective = residual_ratio * policy.residual_weight
        + excess_ratio * policy.excess_weight
        + description_bytes as f64 * policy.description_byte_weight
        + f64::from(free_parameters) * policy.free_parameter_weight
        + assumption_penalty * policy.assumption_weight
        - evidence_support * policy.evidence_credit;
    Ok(DeprojectionScore {
        residual_ratio,
        excess_ratio,
        description_bytes,
        free_parameters,
        evidence_support,
        assumption_penalty,
        objective,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeprojectionAlternative {
    pub id: DeprojectionAlternativeId,
    pub label: String,
    pub program: SourceProgram,
    pub source_claims: Vec<SourceClaimId>,
    pub comparison_artifact: ArtifactId,
    pub coverage_artifact: ArtifactId,
    pub score: DeprojectionScore,
    pub caveats: Vec<String>,
}

impl DeprojectionAlternative {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        label: String,
        program: SourceProgram,
        mut source_claims: Vec<SourceClaimId>,
        comparison_artifact: ArtifactId,
        coverage_artifact: ArtifactId,
        score: DeprojectionScore,
        caveats: Vec<String>,
    ) -> Result<Self, DeprojectionError> {
        if label.trim().is_empty() || program.terms.is_empty() {
            return Err(DeprojectionError::Invalid(
                "an alternative needs a label and at least one editable term".into(),
            ));
        }
        source_claims.sort();
        source_claims.dedup();
        let mut term_bytes = Vec::with_capacity(program.roots.len() * 32);
        for root in &program.roots {
            term_bytes.extend_from_slice(&root.0.bytes);
        }
        let mut claim_bytes = Vec::with_capacity(source_claims.len() * 32);
        for claim in &source_claims {
            claim_bytes.extend_from_slice(&claim.0.bytes);
        }
        let objective = score.objective.to_bits().to_le_bytes();
        let id = DeprojectionAlternativeId(sha256_content(
            ALTERNATIVE_DOMAIN,
            &[
                label.as_bytes(),
                &term_bytes,
                &claim_bytes,
                &comparison_artifact.0.bytes,
                &coverage_artifact.0.bytes,
                &objective,
            ],
        ));
        Ok(Self {
            id,
            label,
            program,
            source_claims,
            comparison_artifact,
            coverage_artifact,
            score,
            caveats,
        })
    }
}

pub fn rank_alternatives(alternatives: &mut [DeprojectionAlternative]) {
    alternatives.sort_by(|left, right| {
        left.score
            .objective
            .total_cmp(&right.score.objective)
            .then_with(|| {
                left.score
                    .residual_ratio
                    .total_cmp(&right.score.residual_ratio)
            })
            .then_with(|| left.score.excess_ratio.total_cmp(&right.score.excess_ratio))
            .then_with(|| {
                left.score
                    .description_bytes
                    .cmp(&right.score.description_bytes)
            })
            .then_with(|| left.id.cmp(&right.id))
    });
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeprojectionStage {
    /// Optional isolated worker claim. Model identity is pinned in `recipe`.
    ModelClaim {
        model_id: String,
        recipe: ContentDigest,
        output_names: Vec<String>,
    },
    NativeRhythm {
        recipe: ContentDigest,
    },
    NativePitch {
        recipe: ContentDigest,
    },
    NativeComponents {
        recipe: ContentDigest,
    },
    FuseEventFamilies {
        recipe: ContentDigest,
    },
    SynthesizePatterns {
        recipe: ContentDigest,
    },
    SynthesizeCurves {
        recipe: ContentDigest,
    },
    RenderConstruction {
        recipe: ContentDigest,
    },
    MeasureResidualAndExcess {
        recipe: ContentDigest,
    },
}

impl DeprojectionStage {
    fn tag(&self) -> &'static [u8] {
        match self {
            Self::ModelClaim { .. } => b"model-claim",
            Self::NativeRhythm { .. } => b"native-rhythm",
            Self::NativePitch { .. } => b"native-pitch",
            Self::NativeComponents { .. } => b"native-components",
            Self::FuseEventFamilies { .. } => b"fuse-event-families",
            Self::SynthesizePatterns { .. } => b"synthesize-patterns",
            Self::SynthesizeCurves { .. } => b"synthesize-curves",
            Self::RenderConstruction { .. } => b"render-construction",
            Self::MeasureResidualAndExcess { .. } => b"measure-residual-excess",
        }
    }

    fn recipe(&self) -> ContentDigest {
        match self {
            Self::ModelClaim { recipe, .. }
            | Self::NativeRhythm { recipe }
            | Self::NativePitch { recipe }
            | Self::NativeComponents { recipe }
            | Self::FuseEventFamilies { recipe }
            | Self::SynthesizePatterns { recipe }
            | Self::SynthesizeCurves { recipe }
            | Self::RenderConstruction { recipe }
            | Self::MeasureResidualAndExcess { recipe } => *recipe,
        }
    }
}

/// One content-addressed computation. Outputs are immutable artifact kinds;
/// execution and storage belong to the existing worker/artifact services.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeprojectionNode {
    pub id: DeprojectionNodeId,
    pub stage: DeprojectionStage,
    pub dependencies: Vec<DeprojectionNodeId>,
    pub source_claims: Vec<SourceClaimId>,
    pub output_kinds: Vec<String>,
}

impl DeprojectionNode {
    pub fn new(
        stage: DeprojectionStage,
        mut dependencies: Vec<DeprojectionNodeId>,
        mut source_claims: Vec<SourceClaimId>,
        mut output_kinds: Vec<String>,
    ) -> Result<Self, DeprojectionError> {
        dependencies.sort();
        if dependencies.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(DeprojectionError::Invalid(
                "node dependencies must not contain duplicates".into(),
            ));
        }
        source_claims.sort();
        source_claims.dedup();
        output_kinds.sort();
        output_kinds.dedup();
        if output_kinds.is_empty() {
            return Err(DeprojectionError::Invalid(
                "a deprojection node must declare an output kind".into(),
            ));
        }
        let mut deps = Vec::with_capacity(dependencies.len() * 32);
        for dependency in &dependencies {
            deps.extend_from_slice(&dependency.0.bytes);
        }
        let mut claims = Vec::with_capacity(source_claims.len() * 32);
        for claim in &source_claims {
            claims.extend_from_slice(&claim.0.bytes);
        }
        let outputs = output_kinds.join("\0");
        let id = DeprojectionNodeId(sha256_content(
            NODE_DOMAIN,
            &[
                stage.tag(),
                &stage.recipe().bytes,
                &deps,
                &claims,
                outputs.as_bytes(),
            ],
        ));
        Ok(Self {
            id,
            stage,
            dependencies,
            source_claims,
            output_kinds,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeprojectionPlan {
    pub id: ContentDigest,
    pub source: MaterialSpan,
    /// Topologically ordered. Independent nodes at the same frontier may run
    /// concurrently; ordering is stable for deterministic scheduling/logs.
    pub nodes: Vec<DeprojectionNode>,
}

impl DeprojectionPlan {
    pub fn new(
        source: MaterialSpan,
        nodes: Vec<DeprojectionNode>,
    ) -> Result<Self, DeprojectionError> {
        source.validate()?;
        let mut seen = BTreeSet::new();
        for node in &nodes {
            if !node
                .dependencies
                .iter()
                .all(|dependency| seen.contains(dependency))
            {
                return Err(DeprojectionError::Invalid(
                    "deprojection plan nodes must be topologically ordered".into(),
                ));
            }
            if !seen.insert(node.id) {
                return Err(DeprojectionError::Invalid(
                    "deprojection plan contains a duplicate node".into(),
                ));
            }
        }
        let mut node_bytes = Vec::with_capacity(nodes.len() * 32);
        for node in &nodes {
            node_bytes.extend_from_slice(&node.id.0.bytes);
        }
        let id = sha256_content(
            PLAN_DOMAIN,
            &[
                source.material_sha256.as_bytes(),
                &source.start_frame.to_le_bytes(),
                &source.frame_count.to_le_bytes(),
                &node_bytes,
            ],
        );
        Ok(Self { id, source, nodes })
    }

    pub fn ready_nodes(&self, completed: &BTreeSet<DeprojectionNodeId>) -> Vec<DeprojectionNodeId> {
        self.nodes
            .iter()
            .filter(|node| {
                !completed.contains(&node.id)
                    && node
                        .dependencies
                        .iter()
                        .all(|dependency| completed.contains(dependency))
            })
            .map(|node| node.id)
            .collect()
    }
}

/// Generation guard shared by native jobs and isolated worker jobs. Starting a
/// new run cancels the old generation; late completions stay cacheable but may
/// not publish into the current project/session.
#[derive(Clone, Debug)]
pub struct DeprojectionRunGuard {
    generation: u64,
    plan: ContentDigest,
    cancellation: RenderCancellation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeprojectionRunToken {
    pub generation: u64,
    pub plan: ContentDigest,
}

impl DeprojectionRunGuard {
    pub fn new(plan: ContentDigest) -> Self {
        Self {
            generation: 1,
            plan,
            cancellation: RenderCancellation::new(),
        }
    }

    pub fn token(&self) -> DeprojectionRunToken {
        DeprojectionRunToken {
            generation: self.generation,
            plan: self.plan,
        }
    }

    pub fn cancellation(&self) -> RenderCancellation {
        self.cancellation.clone()
    }

    pub fn supersede(&mut self, plan: ContentDigest) -> DeprojectionRunToken {
        self.cancellation.cancel();
        self.generation = self.generation.saturating_add(1);
        self.plan = plan;
        self.cancellation = RenderCancellation::new();
        self.token()
    }

    pub fn accepts(&self, token: DeprojectionRunToken) -> bool {
        token == self.token() && !self.cancellation.is_cancelled()
    }
}

/// Convenience bridge preserving all rhythm alternatives. Literal fallback
/// terms are retained instead of silently disappearing from the candidate set.
pub fn terms_from_rhythm_explanations(
    explanations: &PatternExplanationSet,
    source: SourceClaimId,
) -> Vec<EditableTerm> {
    let mut terms = explanations
        .alternatives
        .iter()
        .filter_map(|alternative| {
            EditableTerm::pattern_from_explanation(alternative)
                .or_else(|| EditableTerm::exact_audio_from_explanation(alternative, source))
        })
        .collect::<Vec<_>>();
    terms.sort_by_key(|term| term.id);
    terms
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeprojectionError {
    Invalid(String),
    MissingWorkerOutput(String),
    UnknownPromotionTerm(EditableTermId),
    WrongPromotionTermKind {
        term: EditableTermId,
        expected: &'static str,
    },
}

impl fmt::Display for DeprojectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => formatter.write_str(message),
            Self::MissingWorkerOutput(output) => {
                write!(formatter, "model claim has no output named {output}")
            }
            Self::UnknownPromotionTerm(term) => {
                write!(formatter, "promotion request names unknown term {term:?}")
            }
            Self::WrongPromotionTermKind { term, expected } => {
                write!(
                    formatter,
                    "promotion term {term:?} is not a {expected} term"
                )
            }
        }
    }
}

impl std::error::Error for DeprojectionError {}

fn validate_sha256(field: &str, value: &str) -> Result<(), DeprojectionError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(DeprojectionError::Invalid(format!(
            "{field} must be a lowercase SHA-256"
        )));
    }
    Ok(())
}

fn parse_sha256_digest(value: &str) -> Result<ContentDigest, DeprojectionError> {
    validate_sha256("digest", value)?;
    let mut bytes = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0]).expect("validated hex");
        let low = hex_nibble(pair[1]).expect("validated hex");
        bytes[index] = (high << 4) | low;
    }
    Ok(ContentDigest::new(
        crate::artifact_catalog::DigestAlgorithm::Sha256,
        bytes,
    ))
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn modulation_support(modulation: &ModulationEvidence) -> f32 {
    match modulation {
        ModulationEvidence::Glide { confidence, .. }
        | ModulationEvidence::Vibrato { confidence, .. } => *confidence,
    }
}

fn support_ppm(value: f32) -> Result<u32, DeprojectionError> {
    if !value.is_finite() {
        return Err(DeprojectionError::Invalid(
            "candidate evidence support must be finite".into(),
        ));
    }
    Ok((f64::from(value).clamp(0.0, 1.0) * 1_000_000.0).round() as u32)
}

fn model_output_support_ppm(output: &ModelClaimArtifact) -> Result<u32, DeprojectionError> {
    let mut support = Vec::new();
    for label in &output.labels {
        if matches!(
            label.confidence_kind,
            ClaimConfidenceKind::RelativeSupport | ClaimConfidenceKind::CalibratedProbability
        ) {
            if let Some(confidence) = label.confidence {
                support.push(support_ppm(confidence)?);
            }
        }
    }
    Ok(support.into_iter().max().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_catalog::{ContentDigest, DigestAlgorithm};
    use crate::model_claim::{ClaimSource, WorkerRuntimeProvenance};
    use crate::model_wire::{AdditivityDeclaration, ArtifactDescriptor, ArtifactKind};
    use crate::pitch::{PitchTrack, PitchTrackPoint};
    use crate::rhythm_explanation::{
        DescriptionRank, ExactAudioFallback, ExactAudioFallbackReason, ExplanationFit,
        PatternExplanation,
    };

    fn digest(byte: u8) -> ContentDigest {
        ContentDigest::new(DigestAlgorithm::Sha256, [byte; 32])
    }

    fn source() -> MaterialSpan {
        MaterialSpan {
            material_sha256: "11".repeat(32),
            start_frame: 100,
            frame_count: 48_000,
            sample_rate_hz: 48_000,
            channels: 2,
        }
    }

    fn literal_source() -> SourceClaim {
        SourceClaim::literal(source(), digest(1)).unwrap()
    }

    #[test]
    fn plan_identity_is_stable_and_dependency_order_is_explicit() {
        let source_claim = source().content_id();
        let rhythm = DeprojectionNode::new(
            DeprojectionStage::NativeRhythm { recipe: digest(2) },
            Vec::new(),
            vec![source_claim],
            vec!["rhythm-deprojection".into()],
        )
        .unwrap();
        let patterns = DeprojectionNode::new(
            DeprojectionStage::SynthesizePatterns { recipe: digest(3) },
            vec![rhythm.id],
            vec![source_claim],
            vec!["pattern-alternatives".into()],
        )
        .unwrap();
        let plan = DeprojectionPlan::new(source(), vec![rhythm.clone(), patterns.clone()]).unwrap();
        let same = DeprojectionPlan::new(source(), vec![rhythm.clone(), patterns.clone()]).unwrap();
        assert_eq!(plan.id, same.id);
        assert_eq!(plan.ready_nodes(&BTreeSet::new()), vec![rhythm.id]);
        assert_eq!(
            plan.ready_nodes(&BTreeSet::from([rhythm.id])),
            vec![patterns.id]
        );
        assert!(DeprojectionPlan::new(source(), vec![patterns, rhythm]).is_err());
    }

    #[test]
    fn curve_candidates_preserve_pitch_evidence_and_do_not_select_a_cause() {
        let pitch = PitchAnalysis {
            sample_rate: 48_000,
            frame_size: 2_048,
            hop_size: 256,
            frames: Vec::new(),
            tracks: vec![PitchTrack {
                points: Vec::<PitchTrackPoint>::new(),
                confidence: 0.8,
                voiced_points: 10,
                modulation: vec![ModulationEvidence::Vibrato {
                    start_offset_frames: 10,
                    end_offset_frames: 100,
                    rate_hz: 6.0,
                    extent_semitones: 0.28,
                    confidence: 0.7,
                }],
            }],
        };
        let terms = curve_terms_from_pitch(&pitch, "1");
        assert_eq!(terms.len(), 1);
        let EditableTermKind::Curve {
            target: CurveTarget::PitchCents,
            expression: CurveExpr::Lfo { depth, .. },
            ..
        } = &terms[0].kind
        else {
            panic!("vibrato should become an editable pitch LFO candidate");
        };
        assert!((*depth - 14.0).abs() < 1.0e-5);
        assert!(matches!(
            terms[0].evidence[0],
            EvidenceRef::NativeLocator { .. }
        ));
    }

    #[test]
    fn measured_lfo_is_evaluated_in_physical_source_time() {
        let curve = CurveExpr::Lfo {
            shape: LfoShape::Sine,
            rate_hz: 6.0,
            depth: 1.0,
            phase: 0.0,
        };
        let quarter_cycle =
            evaluate_curve_at_source_frame(&curve, (0, 48_000), 2_000, 48_000).unwrap();
        assert!((quarter_cycle - 1.0).abs() < 1.0e-12);
        let one_cycle = evaluate_curve_at_source_frame(&curve, (0, 48_000), 8_000, 48_000).unwrap();
        assert!(one_cycle.abs() < 1.0e-12);
    }

    #[test]
    fn unresolved_family_voice_is_retained_as_a_compile_refusal() {
        let term = EditableTerm {
            id: EditableTermId(digest(7)),
            kind: EditableTermKind::Pattern {
                source: "fam4 ~ fam4 ~".into(),
                execution: PatternExecutionSemantics {
                    cycle: BeatDuration((crate::sequencer::PPQ * 4) as u64),
                    seed: 0,
                    initial_cycle_index: 0,
                },
                voices: BTreeMap::from([(
                    "fam4".into(),
                    VoiceTerm::UnresolvedFamily { family: 4 },
                )]),
            },
            evidence: Vec::new(),
            derivation: Derivation {
                rule: "test".into(),
                recipe: digest(8),
                premises: Vec::new(),
            },
            description_bytes: 13,
            free_parameters: 1,
        };
        let program = SourceProgram::new(source(), vec![term.clone()], vec![term.id]).unwrap();
        assert_eq!(
            program.compile_refusals(),
            vec![ProgramCompileRefusal::UnresolvedPatternVoice {
                term: term.id,
                binding: "fam4".into(),
                family: 4,
            }]
        );
    }

    #[test]
    fn residual_excess_and_description_all_affect_ranking() {
        let policy = ScorePolicy::default();
        let compact = score_deprojection(
            CoverageSummary {
                source_power: 100.0,
                construction_power: 90.0,
                residual_power: 10.0,
                signed_explained_energy: 0.9,
                clamped_explained_energy: 0.9,
                excess_energy_ratio: 0.0,
            },
            64,
            2,
            0.8,
            0.0,
            policy,
        )
        .unwrap();
        let bloated = score_deprojection(
            CoverageSummary {
                source_power: 100.0,
                construction_power: 120.0,
                residual_power: 10.0,
                signed_explained_energy: 0.9,
                clamped_explained_energy: 0.9,
                excess_energy_ratio: 0.2,
            },
            4_096,
            64,
            0.8,
            0.0,
            policy,
        )
        .unwrap();
        assert!(compact.objective < bloated.objective);
        assert_eq!(compact.residual_ratio, bloated.residual_ratio);
    }

    #[test]
    fn run_generation_rejects_late_completions_after_supersession() {
        let mut guard = DeprojectionRunGuard::new(digest(4));
        let old = guard.token();
        let old_cancellation = guard.cancellation();
        let current = guard.supersede(digest(5));
        assert!(old_cancellation.is_cancelled());
        assert!(!guard.accepts(old));
        assert!(guard.accepts(current));
    }

    #[test]
    fn source_contract_refuses_to_sum_overlapping_or_generative_claims() {
        assert!(SourceEstimateContract::JointAdditive {
            maximum_error_parts_per_million: 2
        }
        .can_join_linear_construction());
        assert!(!SourceEstimateContract::Overlapping.can_join_linear_construction());
        assert!(!SourceEstimateContract::Generative.can_join_linear_construction());
    }

    #[test]
    fn rhythm_adapter_retains_literal_fallback_with_integer_structural_rank() {
        let alternative = PatternExplanation {
            id: PatternAlternativeId(digest(40)),
            rank: 0,
            representation: PatternExplanationRepresentation::ExactAudio(ExactAudioFallback {
                source_span: SampleSpan { start: 10, end: 30 },
                estimated_literal_bytes: 160,
                reasons: vec![ExactAudioFallbackReason::NoAdmissibleTerm],
            }),
            families: BTreeMap::new(),
            fit: ExplanationFit {
                combined_fit: 0.75,
                ..ExplanationFit::default()
            },
            description: DescriptionRank {
                description_bytes: 160,
                fit_penalty_millibytes: 0,
                total_millibytes: 160_000,
            },
            evidence: vec![RhythmEvidenceRef::Hit(2)],
            derivations: Vec::new(),
        };
        let explanations = PatternExplanationSet {
            alternatives: vec![alternative],
            rejected_terms: Vec::new(),
        };
        let first = candidates_from_rhythm_explanations(
            source(),
            literal_source(),
            &explanations,
            StructuralScorePolicy::default(),
        )
        .unwrap();
        let second = candidates_from_rhythm_explanations(
            source(),
            literal_source(),
            &explanations,
            StructuralScorePolicy::default(),
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(first[0].structural_score.evidence_support_ppm, 750_000);
        assert_eq!(first[0].structural_score.exact_audio_terms, 1);
        assert!(matches!(
            first[0].program.terms.values().next().unwrap().kind,
            EditableTermKind::ExactAudioReference { .. }
        ));
    }

    #[test]
    fn pitch_candidate_emits_revision_pinned_curve_promotion() {
        let pitch = PitchAnalysis {
            sample_rate: 48_000,
            frame_size: 2_048,
            hop_size: 256,
            frames: Vec::new(),
            tracks: vec![PitchTrack {
                points: Vec::new(),
                confidence: 0.8,
                voiced_points: 12,
                modulation: vec![ModulationEvidence::Glide {
                    start_offset_frames: 100,
                    end_offset_frames: 500,
                    direction: crate::pitch::GlideDirection::Rising,
                    semitones_per_second: 3.0,
                    extent_semitones: 1.5,
                    confidence: 0.625,
                }],
            }],
        };
        let candidates = candidates_from_pitch(
            source(),
            literal_source(),
            &pitch,
            "pitch-v4",
            StructuralScorePolicy::default(),
        )
        .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].structural_score.evidence_support_ppm, 625_000);
        let term = candidates[0].program.roots[0];
        let request = candidates[0]
            .promotion_request(
                91,
                PromotionIntent::Curve {
                    term,
                    parameter_hint: Some("lead.pitch".into()),
                },
            )
            .unwrap();
        assert!(matches!(
            request,
            DeprojectionPromotionRequest::Curve {
                expected_project_revision: 91,
                parameter_hint: Some(ref hint),
                ..
            } if hint == "lead.pitch"
        ));
        assert!(matches!(
            candidates[0].promotion_request(
                91,
                PromotionIntent::Pattern {
                    term,
                    place_at_frame: None,
                }
            ),
            Err(DeprojectionError::WrongPromotionTermKind { .. })
        ));
    }

    fn model_claim(kind: ArtifactKind, additivity: AdditivityDeclaration) -> ModelClaimBundle {
        ModelClaimBundle::new(
            "22".repeat(32),
            "33".repeat(32),
            ClaimSource {
                material_sha256: source().material_sha256,
                start_frame: source().start_frame,
                frame_count: source().frame_count,
                sample_rate_hz: source().sample_rate_hz,
                channels: source().channels,
            },
            WorkerRuntimeProvenance {
                worker_name: "test-worker".into(),
                runtime: "test-runtime".into(),
                adapter_sha256: None,
            },
            additivity.clone(),
            vec![crate::model_claim::ModelClaimArtifact {
                descriptor: ArtifactDescriptor {
                    relative_path: "output.bin".into(),
                    sha256: "44".repeat(32),
                    byte_len: 256,
                    kind,
                    media_type: "application/x-audec-test".into(),
                    schema_revision: 1,
                    time_base_hz: Some(48_000),
                    additivity,
                    source_backlinks: Vec::new(),
                },
                output_name: "candidate".into(),
                labels: Vec::new(),
            }],
        )
        .unwrap()
    }

    #[test]
    fn model_adapter_distinguishes_audible_programs_from_evidence_only_outputs() {
        let published = PublishedModelOutput {
            artifact: ArtifactId(digest(50)),
            output_digest: digest(51),
            maximum_error_parts_per_million: Some(8),
        };
        let audio = candidate_from_model_output(
            &model_claim(ArtifactKind::Audio, AdditivityDeclaration::LinearSum),
            "candidate",
            published,
            StructuralScorePolicy::default(),
        )
        .unwrap();
        assert!(audio.candidate.is_some());
        assert!(audio.refusal.is_none());
        assert!(matches!(
            audio.source_claim.contract,
            SourceEstimateContract::JointAdditive {
                maximum_error_parts_per_million: 8
            }
        ));

        let events = candidate_from_model_output(
            &model_claim(ArtifactKind::EventMap, AdditivityDeclaration::NonAudio),
            "candidate",
            published,
            StructuralScorePolicy::default(),
        )
        .unwrap();
        assert!(events.candidate.is_none());
        assert_eq!(
            events.refusal,
            Some(ModelCandidateRefusal::EvidenceOnlyArtifact {
                kind: ArtifactKind::EventMap
            })
        );
        assert_eq!(
            events.source_claim.contract,
            SourceEstimateContract::Measurement
        );
    }
}
