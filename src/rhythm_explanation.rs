//! Deterministic synthesis of rhythm evidence into editable pattern terms.
//!
//! This module ranks descriptions of observations; it does not identify an
//! instrument, declare a winning hypothesis, or turn fit/coverage into a
//! correctness score. Family bindings stay anonymous. Generator alternatives
//! are compiled through `pattern_lang` and `pattern_authoring`; a literal-audio
//! escape hatch is represented by a different enum variant so it cannot be
//! mistaken for a successful symbolic explanation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use crate::artifact_catalog::{sha256_content, ArtifactId, ContentDigest};
#[cfg(test)]
use crate::aspect::FrameSpan;
use crate::aspect::{Aspect, ConcreteAspect};
use crate::daw_render::RenderCancellation;
use crate::explanation::{
    CompiledExplanation, ExplanationCompiler, ExplanationDefinition, ExplanationDependencyPin,
    ExplanationError, ExplanationEvidenceRef, ExplanationId, ExplanationScope,
    FrozenExplanationRenderer,
};
use crate::ontology::Provenance;
use crate::pattern_authoring::{self, DivergedOverwrite, PatternAuthoringError};
use crate::pattern_lang::{
    self, Element, PatternEvalDiagnostic, PatternEvalError, PatternExpr, Ratio, Step,
    MAX_EXACT_STEPS,
};
use crate::rhythm::{PatternHypothesis, RhythmDeprojection, SampleSpan};
use crate::sequencer::{
    BeatDuration, PatternContent, PatternDefinition, PatternId, PatternOrigin, StepPattern,
    TriggerTarget, PPQ,
};

const SIXTEENTH_TICKS: u64 = (PPQ as u64) / 4;
const FALLBACK_STORAGE_STEPS: u32 = 16;

/// Bounded, deterministic search controls. `fit_penalty_bytes` expresses the
/// maximum description-length surcharge for poor timing/energy agreement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExplainBudget {
    pub maximum_terms: usize,
    pub cycle_sixteenths: Option<u32>,
    pub fit_penalty_bytes: u32,
    pub include_exact_audio_fallback: bool,
}

impl Default for ExplainBudget {
    fn default() -> Self {
        Self {
            maximum_terms: 24,
            cycle_sixteenths: None,
            fit_penalty_bytes: 96,
            include_exact_audio_fallback: true,
        }
    }
}

impl ExplainBudget {
    fn validate(self) -> Result<Self, RhythmExplanationError> {
        if self.maximum_terms == 0 {
            return Err(RhythmExplanationError::InvalidBudget(
                "maximum_terms must be non-zero",
            ));
        }
        if self
            .cycle_sixteenths
            .is_some_and(|steps| steps == 0 || steps > 4_096)
        {
            return Err(RhythmExplanationError::InvalidBudget(
                "cycle_sixteenths must be in 1..=4096",
            ));
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PatternAlternativeId(pub ContentDigest);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RhythmEvidenceRef {
    Pattern(usize),
    Hit(usize),
    Family(usize),
    Tempo(usize),
    BeatPhase(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternDerivation {
    pub rule: String,
    pub premises: Vec<RhythmEvidenceRef>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ExplanationFit {
    pub timing_rms_frames: f64,
    pub timing_max_frames: f64,
    pub timing_fit: f32,
    /// Relative onset-strength error using rhythm novelty as the native
    /// energy proxy. Exact audio-domain energy is measured later by
    /// `ComparisonRuntime`; this field does not pretend the analyzer retained
    /// source PCM.
    pub energy_rms: f64,
    pub energy_fit: f32,
    /// Ranking convenience only. It is not confidence or correctness.
    pub combined_fit: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DescriptionRank {
    pub description_bytes: u64,
    pub fit_penalty_millibytes: u64,
    pub total_millibytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GridRealization {
    Exact {
        steps: u32,
    },
    Fallback {
        exact_steps_required: u32,
        storage_steps: u32,
        nonzero_residues: usize,
        maximum_residue_ticks: u32,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct PatternTermExplanation {
    pub expr: PatternExpr,
    pub source: String,
    pub bindings: BTreeMap<String, TriggerTarget>,
    /// Cycle-zero realization produced by `pattern_authoring::apply_expression`.
    pub pattern: PatternDefinition,
    pub diagnostics: Vec<PatternEvalDiagnostic>,
    pub grid: GridRealization,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExactAudioFallbackReason {
    RequestedLiteralReference,
    NoAdmissibleTerm,
    TermCollision,
    GridOrArithmeticRefusal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExactAudioFallback {
    pub source_span: SampleSpan,
    pub estimated_literal_bytes: u64,
    pub reasons: Vec<ExactAudioFallbackReason>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PatternExplanationRepresentation {
    Term(PatternTermExplanation),
    ExactAudio(ExactAudioFallback),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PatternExplanation {
    pub id: PatternAlternativeId,
    pub rank: usize,
    pub representation: PatternExplanationRepresentation,
    pub families: BTreeMap<usize, String>,
    pub fit: ExplanationFit,
    pub description: DescriptionRank,
    pub evidence: Vec<RhythmEvidenceRef>,
    pub derivations: Vec<PatternDerivation>,
}

impl PatternExplanation {
    /// Deterministic local claim key for an artifact-qualified model-claim
    /// scope. The artifact remains the global authority; this value is not a
    /// source or instrument identity.
    pub fn claim_id(&self) -> u64 {
        nonzero_u64(&self.id.0.bytes[..8])
    }

    pub fn term(&self) -> Option<&PatternTermExplanation> {
        match &self.representation {
            PatternExplanationRepresentation::Term(term) => Some(term),
            PatternExplanationRepresentation::ExactAudio(_) => None,
        }
    }

    pub fn persistent_definition(
        &self,
        id: ExplanationId,
        rhythm_artifact: ArtifactId,
        extent: Aspect,
        provenance: Provenance,
    ) -> ExplanationDefinition {
        let label = match &self.representation {
            PatternExplanationRepresentation::Term(term) => {
                format!("Pattern term: {}", term.source)
            }
            PatternExplanationRepresentation::ExactAudio(_) => {
                "Exact-audio rhythm reference".to_owned()
            }
        };
        ExplanationDefinition {
            id,
            label,
            scope: ExplanationScope::ModelClaim {
                artifact: rhythm_artifact,
                claim: self.claim_id(),
            },
            extent,
            evidence: vec![ExplanationEvidenceRef::Artifact(rhythm_artifact)],
            provenance,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RejectedPatternTerm {
    pub source: Option<String>,
    pub reason: TermRejection,
    pub evidence: Vec<RhythmEvidenceRef>,
    pub derivation: PatternDerivation,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TermRejection {
    MalformedPattern,
    NegativeOffset(i32),
    OffsetOutsideCycle { offset: i32, cycle_steps: u32 },
    DuplicateFamilyStep { family: usize, offset: i32 },
    Collision { binding: String, tick: i64 },
    Evaluation(String),
    ArithmeticOverflow,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PatternExplanationSet {
    /// All retained competing alternatives in deterministic rank order.
    pub alternatives: Vec<PatternExplanation>,
    /// Failed symbolic attempts remain inspectable; they are not converted to
    /// silent quantization or last-writer-wins behavior.
    pub rejected_terms: Vec<RejectedPatternTerm>,
}

/// Synthesize ranked generator terms from anonymous rhythm-family evidence.
/// An empty `families` slice means all families present in retained pattern
/// hypotheses. No candidate is selected or promoted by this function.
pub fn explain_rhythm(
    deprojection: &RhythmDeprojection,
    families: &[usize],
    budget: ExplainBudget,
) -> Result<PatternExplanationSet, RhythmExplanationError> {
    let budget = budget.validate()?;
    if deprojection.sample_rate == 0 {
        return Err(RhythmExplanationError::InvalidDeprojection(
            "sample rate is zero",
        ));
    }

    let mut available = deprojection
        .patterns
        .iter()
        .flat_map(|pattern| pattern.family_sequence.iter().copied())
        .collect::<BTreeSet<_>>();
    available.extend(deprojection.event_families.iter().map(|family| family.id));
    available.extend(deprojection.hits.iter().filter_map(|hit| hit.family));
    let selected = if families.is_empty() {
        available.clone()
    } else {
        families.iter().copied().collect::<BTreeSet<_>>()
    };
    if let Some(missing) = selected.iter().find(|family| !available.contains(family)) {
        return Err(RhythmExplanationError::MissingFamily(*missing));
    }

    let mut candidates = BTreeMap::<String, CandidateDraft>::new();
    let mut rejected_terms = Vec::new();
    for (pattern_index, hypothesis) in deprojection.patterns.iter().enumerate() {
        let evidence = evidence_for_pattern(deprojection, pattern_index, hypothesis, &selected);
        if hypothesis.family_sequence.len() != hypothesis.step_offsets.len() {
            rejected_terms.push(RejectedPatternTerm {
                source: None,
                reason: TermRejection::MalformedPattern,
                evidence: evidence.clone(),
                derivation: derivation("rhythm.pattern.malformed", evidence),
            });
            continue;
        }
        let events = pattern_events(deprojection, hypothesis, &selected);
        if events.is_empty() {
            continue;
        }
        let maximum_offset = events.iter().map(|event| event.offset).max().unwrap_or(0);
        if maximum_offset < 0 {
            rejected_terms.push(RejectedPatternTerm {
                source: None,
                reason: TermRejection::NegativeOffset(maximum_offset),
                evidence: evidence.clone(),
                derivation: derivation("rhythm.pattern.negative-offset", evidence),
            });
            continue;
        }
        let cycle_steps = budget
            .cycle_sixteenths
            .unwrap_or_else(|| infer_cycle_sixteenths(hypothesis, maximum_offset as u32));
        if cycle_steps > 4_096 {
            rejected_terms.push(RejectedPatternTerm {
                source: None,
                reason: TermRejection::ArithmeticOverflow,
                evidence: evidence.clone(),
                derivation: derivation("rhythm.pattern.cycle-budget", evidence),
            });
            continue;
        }
        if let Some(event) = events
            .iter()
            .find(|event| event.offset < 0 || event.offset as u32 >= cycle_steps)
        {
            rejected_terms.push(RejectedPatternTerm {
                source: None,
                reason: if event.offset < 0 {
                    TermRejection::NegativeOffset(event.offset)
                } else {
                    TermRejection::OffsetOutsideCycle {
                        offset: event.offset,
                        cycle_steps,
                    }
                },
                evidence: evidence.clone(),
                derivation: derivation("rhythm.pattern.outside-cycle", evidence),
            });
            continue;
        }

        let timing = timing_fit(deprojection, hypothesis, &events);
        let mut proposals = Vec::<(String, PatternExpr, BTreeMap<(usize, i32), f32>)>::new();
        if let Some((expr, velocities)) = direct_sequence_expr(&events, cycle_steps) {
            proposals.push((
                "rhythm.pattern.direct-sequence".to_owned(),
                expr,
                velocities,
            ));
        }
        match family_stack_expr(&events, cycle_steps) {
            Ok((expr, velocities)) => {
                proposals.push(("rhythm.pattern.family-stack".to_owned(), expr, velocities))
            }
            Err(reason) => rejected_terms.push(RejectedPatternTerm {
                source: None,
                reason,
                evidence: evidence.clone(),
                derivation: derivation("rhythm.pattern.family-stack", evidence.clone()),
            }),
        }
        if let Some((expr, velocities)) = euclidean_stack_expr(&events, cycle_steps) {
            proposals.push(("rhythm.pattern.euclidean".to_owned(), expr, velocities));
        }

        for (rule, expr, velocities) in proposals {
            let derivation = derivation(&rule, evidence.clone());
            match compile_term(expr, cycle_steps, &events) {
                Ok(term) => {
                    let fit = finish_fit(timing, energy_fit(&events, &velocities));
                    let source = term.source.clone();
                    let draft = CandidateDraft {
                        representation: PatternExplanationRepresentation::Term(term),
                        families: family_names(&events),
                        fit,
                        evidence: evidence.clone(),
                        derivations: vec![derivation],
                    };
                    if let Some(existing) = candidates.get_mut(&source) {
                        merge_draft(existing, draft);
                    } else {
                        candidates.insert(source, draft);
                    }
                }
                Err((source, reason)) => rejected_terms.push(RejectedPatternTerm {
                    source,
                    reason,
                    evidence: evidence.clone(),
                    derivation,
                }),
            }
        }
    }

    let mut alternatives = candidates
        .into_values()
        .map(|draft| finish_draft(draft, budget.fit_penalty_bytes))
        .collect::<Vec<_>>();
    alternatives.sort_by(compare_alternatives);
    alternatives.truncate(budget.maximum_terms);

    if budget.include_exact_audio_fallback {
        let mut reasons = vec![ExactAudioFallbackReason::RequestedLiteralReference];
        if alternatives.is_empty() {
            reasons.push(ExactAudioFallbackReason::NoAdmissibleTerm);
        }
        if rejected_terms
            .iter()
            .any(|rejection| matches!(&rejection.reason, TermRejection::Collision { .. }))
        {
            reasons.push(ExactAudioFallbackReason::TermCollision);
        }
        if rejected_terms.iter().any(|rejection| {
            matches!(
                &rejection.reason,
                TermRejection::Evaluation(_) | TermRejection::ArithmeticOverflow
            )
        }) {
            reasons.push(ExactAudioFallbackReason::GridOrArithmeticRefusal);
        }
        reasons.sort();
        reasons.dedup();
        let evidence = all_evidence(deprojection, &selected);
        let literal_bytes = (deprojection.sample_frames as u64).saturating_mul(4);
        let draft = CandidateDraft {
            representation: PatternExplanationRepresentation::ExactAudio(ExactAudioFallback {
                source_span: SampleSpan {
                    start: 0,
                    end: deprojection.sample_frames,
                },
                estimated_literal_bytes: literal_bytes,
                reasons,
            }),
            families: selected
                .iter()
                .copied()
                .map(|family| (family, anonymous_binding(family)))
                .collect(),
            fit: ExplanationFit {
                timing_fit: 1.0,
                energy_fit: 1.0,
                combined_fit: 1.0,
                ..ExplanationFit::default()
            },
            evidence: evidence.clone(),
            derivations: vec![derivation("rhythm.exact-audio-reference", evidence)],
        };
        alternatives.push(finish_draft(draft, budget.fit_penalty_bytes));
        alternatives.sort_by(compare_alternatives);
    }

    for (rank, alternative) in alternatives.iter_mut().enumerate() {
        alternative.rank = rank;
    }
    let mut claims = BTreeSet::new();
    if let Some(collision) = alternatives
        .iter()
        .map(PatternExplanation::claim_id)
        .find(|claim| !claims.insert(*claim))
    {
        return Err(RhythmExplanationError::ClaimCollision(collision));
    }
    rejected_terms.sort_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then_with(|| left.reason.cmp(&right.reason))
            .then_with(|| left.evidence.cmp(&right.evidence))
    });
    Ok(PatternExplanationSet {
        alternatives,
        rejected_terms,
    })
}

#[derive(Clone, Debug)]
struct PatternEvent {
    family: usize,
    offset: i32,
    velocity: f32,
    sequence_index: usize,
}

fn pattern_events(
    deprojection: &RhythmDeprojection,
    hypothesis: &PatternHypothesis,
    selected: &BTreeSet<usize>,
) -> Vec<PatternEvent> {
    let mut events = hypothesis
        .family_sequence
        .iter()
        .copied()
        .zip(hypothesis.step_offsets.iter().copied())
        .enumerate()
        .filter(|(_, (family, _))| selected.contains(family))
        .map(|(sequence_index, (family, offset))| PatternEvent {
            family,
            offset,
            velocity: mean_event_velocity(deprojection, hypothesis, sequence_index),
            sequence_index,
        })
        .collect::<Vec<_>>();
    let maximum = events
        .iter()
        .map(|event| event.velocity)
        .filter(|value| value.is_finite())
        .fold(0.0_f32, f32::max)
        .max(f32::EPSILON);
    for event in &mut events {
        event.velocity = (event.velocity / maximum).clamp(0.0, 1.0);
    }
    events
}

fn mean_event_velocity(
    deprojection: &RhythmDeprojection,
    hypothesis: &PatternHypothesis,
    sequence_index: usize,
) -> f32 {
    let values = hypothesis
        .occurrences
        .iter()
        .filter_map(|occurrence| {
            deprojection
                .hits
                .get(occurrence.event_index.checked_add(sequence_index)?)
                .map(|hit| hit.novelty_strength.max(0.0))
        })
        .filter(|value| value.is_finite())
        .collect::<Vec<_>>();
    if values.is_empty() {
        return 1.0;
    }
    (values.iter().sum::<f32>() / values.len() as f32).max(0.0)
}

fn direct_sequence_expr(
    events: &[PatternEvent],
    cycle_steps: u32,
) -> Option<(PatternExpr, BTreeMap<(usize, i32), f32>)> {
    let mut by_offset = BTreeMap::<i32, &PatternEvent>::new();
    for event in events {
        if by_offset.insert(event.offset, event).is_some() {
            return None;
        }
    }
    let steps = (0..cycle_steps)
        .map(|offset| match by_offset.get(&(offset as i32)) {
            Some(event) => named_step(event.family, event.velocity),
            None => rest_step(),
        })
        .collect();
    let velocities = events
        .iter()
        .map(|event| ((event.family, event.offset), event.velocity))
        .collect();
    Some((PatternExpr::Seq(steps), velocities))
}

fn family_stack_expr(
    events: &[PatternEvent],
    cycle_steps: u32,
) -> Result<(PatternExpr, BTreeMap<(usize, i32), f32>), TermRejection> {
    let mut families = BTreeMap::<usize, BTreeMap<i32, f32>>::new();
    for event in events {
        let lane = families.entry(event.family).or_default();
        if lane.insert(event.offset, event.velocity).is_some() {
            return Err(TermRejection::DuplicateFamilyStep {
                family: event.family,
                offset: event.offset,
            });
        }
    }
    let mut members = Vec::new();
    let mut velocities = BTreeMap::new();
    for (family, lane) in families {
        let steps = (0..cycle_steps)
            .map(|offset| match lane.get(&(offset as i32)) {
                Some(velocity) => named_step(family, *velocity),
                None => rest_step(),
            })
            .collect();
        velocities.extend(
            lane.into_iter()
                .map(|(offset, velocity)| ((family, offset), velocity)),
        );
        members.push(PatternExpr::Seq(steps));
    }
    Ok((
        if members.len() == 1 {
            members.pop().expect("one family")
        } else {
            PatternExpr::Stack(members)
        },
        velocities,
    ))
}

fn euclidean_stack_expr(
    events: &[PatternEvent],
    cycle_steps: u32,
) -> Option<(PatternExpr, BTreeMap<(usize, i32), f32>)> {
    let mut families = BTreeMap::<usize, Vec<&PatternEvent>>::new();
    for event in events {
        families.entry(event.family).or_default().push(event);
    }
    let mut members = Vec::new();
    let mut velocities = BTreeMap::new();
    for (family, events) in families {
        let positions = events
            .iter()
            .map(|event| event.offset as u32)
            .collect::<BTreeSet<_>>();
        if positions.len() != events.len() || positions.is_empty() {
            return None;
        }
        let hits = positions.len() as u32;
        let rotation = (0..cycle_steps as i32)
            .find(|rotation| euclidean_positions(hits, cycle_steps, *rotation) == positions)?;
        let mean = events.iter().map(|event| event.velocity).sum::<f32>() / events.len() as f32;
        let base = PatternExpr::Euclid {
            hits,
            slots: cycle_steps,
            rotation,
            element: Element::Name {
                binding: anonymous_binding(family),
                variant: None,
            },
        };
        let expr = if mean.to_bits() == 1.0_f32.to_bits() {
            base
        } else {
            PatternExpr::Gain {
                linear: mean,
                inner: Box::new(base),
            }
        };
        for event in events {
            velocities.insert((family, event.offset), mean);
        }
        members.push(expr);
    }
    Some((
        if members.len() == 1 {
            members.pop().expect("one Euclidean family")
        } else {
            PatternExpr::Stack(members)
        },
        velocities,
    ))
}

fn euclidean_positions(hits: u32, slots: u32, rotation: i32) -> BTreeSet<u32> {
    (0..slots)
        .filter(|slot| {
            let rotated =
                (i64::from(*slot) - i64::from(rotation)).rem_euclid(i64::from(slots)) as u64;
            (rotated * u64::from(hits)) % u64::from(slots) < u64::from(hits)
        })
        .collect()
}

fn named_step(family: usize, velocity: f32) -> Step {
    Step {
        element: Element::Name {
            binding: anonymous_binding(family),
            variant: None,
        },
        width: Ratio::ONE,
        replicate: 1,
        repeat: 1,
        probability: None,
        velocity: Some(velocity.clamp(0.0, 1.0)),
    }
}

fn rest_step() -> Step {
    Step {
        element: Element::Rest,
        width: Ratio::ONE,
        replicate: 1,
        repeat: 1,
        probability: None,
        velocity: None,
    }
}

fn compile_term(
    expr: PatternExpr,
    cycle_steps: u32,
    events: &[PatternEvent],
) -> Result<PatternTermExplanation, (Option<String>, TermRejection)> {
    let source = pattern_lang::print(&expr);
    let bindings = family_names(events)
        .into_iter()
        .map(|(family, name)| {
            let target = u64::try_from(family)
                .ok()
                .and_then(|family| family.checked_add(1))
                .map(TriggerTarget::AnalysisTemplate)
                .ok_or(TermRejection::ArithmeticOverflow)?;
            Ok((name, target))
        })
        .collect::<Result<BTreeMap<_, _>, TermRejection>>()
        .map_err(|reason| (Some(source.clone()), reason))?;
    let length_ticks = u64::from(cycle_steps)
        .checked_mul(SIXTEENTH_TICKS)
        .ok_or_else(|| (Some(source.clone()), TermRejection::ArithmeticOverflow))?;
    let term_digest = sha256_content(
        b"audec.pattern-explanation.term.v1",
        &[
            source.as_bytes(),
            &pattern_lang::bindings_hash(&bindings).0.to_le_bytes(),
        ],
    );
    let seed = PatternDefinition {
        id: PatternId::from_raw(nonzero_u64(&term_digest.bytes[8..16])),
        name: "Anonymous rhythm explanation".to_owned(),
        length: BeatDuration(length_ticks),
        content: PatternContent::Steps(StepPattern {
            resolution: BeatDuration(SIXTEENTH_TICKS),
            swing: 0.0,
            lanes: BTreeMap::new(),
        }),
        origin: PatternOrigin::Authored,
        revision: 0,
    };
    let application = pattern_authoring::apply_expression(
        &seed,
        &source,
        bindings.clone(),
        DivergedOverwrite::Confirmed,
    )
    .map_err(|error| (Some(source.clone()), rejection_from_authoring(error)))?;
    application.definition.validate().map_err(|error| {
        (
            Some(source.clone()),
            TermRejection::Evaluation(error.to_string()),
        )
    })?;
    let grid = grid_realization(&application.definition, cycle_steps, events);
    Ok(PatternTermExplanation {
        expr,
        source,
        bindings,
        pattern: application.definition,
        diagnostics: application.diagnostics,
        grid,
    })
}

fn rejection_from_authoring(error: PatternAuthoringError) -> TermRejection {
    match error {
        PatternAuthoringError::Evaluate(PatternEvalError::StepCollision { binding, tick }) => {
            TermRejection::Collision { binding, tick }
        }
        other => TermRejection::Evaluation(other.to_string()),
    }
}

fn grid_realization(
    definition: &PatternDefinition,
    cycle_steps: u32,
    events: &[PatternEvent],
) -> GridRealization {
    let mut divisor = u64::from(cycle_steps);
    for event in events {
        divisor = gcd(divisor, event.offset.unsigned_abs() as u64);
    }
    let exact_steps_required = u64::from(cycle_steps) / divisor.max(1);
    if exact_steps_required <= MAX_EXACT_STEPS {
        return GridRealization::Exact {
            steps: exact_steps_required as u32,
        };
    }
    let (nonzero_residues, maximum_residue_ticks) = match &definition.content {
        PatternContent::Steps(pattern) => pattern
            .lanes
            .values()
            .flat_map(|lane| lane.steps.values())
            .filter(|event| event.micro_offset != 0)
            .fold((0_usize, 0_u32), |(count, maximum), event| {
                (count + 1, maximum.max(event.micro_offset.unsigned_abs()))
            }),
        PatternContent::Notes(_) => (0, 0),
    };
    GridRealization::Fallback {
        exact_steps_required: exact_steps_required.min(u64::from(u32::MAX)) as u32,
        storage_steps: FALLBACK_STORAGE_STEPS,
        nonzero_residues,
        maximum_residue_ticks,
    }
}

fn infer_cycle_sixteenths(hypothesis: &PatternHypothesis, maximum_offset: u32) -> u32 {
    let occurrence_cycle = hypothesis
        .occurrences
        .windows(2)
        .filter_map(|pair| {
            let delta = (pair[1].beat_position - pair[0].beat_position) * 4.0;
            (delta.is_finite() && delta > 0.0).then(|| delta.round() as u32)
        })
        .filter(|steps| *steps > maximum_offset)
        .min();
    occurrence_cycle.unwrap_or_else(|| round_up(maximum_offset.saturating_add(1).max(1), 4))
}

fn round_up(value: u32, multiple: u32) -> u32 {
    value
        .saturating_add(multiple - 1)
        .checked_div(multiple)
        .unwrap_or(1)
        .saturating_mul(multiple)
        .max(1)
}

#[derive(Clone, Copy)]
struct TimingFit {
    rms: f64,
    maximum: f64,
    score: f32,
}

fn timing_fit(
    deprojection: &RhythmDeprojection,
    hypothesis: &PatternHypothesis,
    events: &[PatternEvent],
) -> TimingFit {
    let bpm = deprojection
        .tempo_hypotheses
        .iter()
        .find(|tempo| tempo.rank == 0)
        .map(|tempo| tempo.bpm)
        .filter(|bpm| bpm.is_finite() && *bpm > 0.0)
        .unwrap_or(120.0);
    let samples_per_sixteenth = deprojection.sample_rate as f64 * 60.0 / bpm as f64 / 4.0;
    let mut squared = 0.0;
    let mut maximum = 0.0_f64;
    let mut count = 0_u64;
    let Some(anchor) = events.iter().min_by_key(|event| event.sequence_index) else {
        return TimingFit {
            rms: samples_per_sixteenth,
            maximum: samples_per_sixteenth,
            score: 0.5,
        };
    };
    for occurrence in &hypothesis.occurrences {
        let Some(anchor_index) = occurrence.event_index.checked_add(anchor.sequence_index) else {
            continue;
        };
        let Some(base) = deprojection.hits.get(anchor_index) else {
            continue;
        };
        for event in events {
            let Some(hit_index) = occurrence.event_index.checked_add(event.sequence_index) else {
                continue;
            };
            let Some(hit) = deprojection.hits.get(hit_index) else {
                continue;
            };
            let predicted = base.peak_sample as f64
                + (event.offset - anchor.offset) as f64 * samples_per_sixteenth;
            let error = (hit.peak_sample as f64 - predicted).abs();
            squared += error * error;
            maximum = maximum.max(error);
            count += 1;
        }
    }
    let rms = if count == 0 {
        samples_per_sixteenth
    } else {
        (squared / count as f64).sqrt()
    };
    let tolerance = (samples_per_sixteenth * 0.5).max(1.0);
    TimingFit {
        rms,
        maximum,
        score: (1.0 / (1.0 + rms / tolerance)) as f32,
    }
}

fn energy_fit(events: &[PatternEvent], candidate: &BTreeMap<(usize, i32), f32>) -> (f64, f32) {
    if events.is_empty() {
        return (1.0, 0.0);
    }
    let squared = events
        .iter()
        .map(|event| {
            let predicted = candidate
                .get(&(event.family, event.offset))
                .copied()
                .unwrap_or(0.0);
            let error = f64::from(event.velocity - predicted);
            error * error
        })
        .sum::<f64>();
    let rms = (squared / events.len() as f64).sqrt();
    (rms, (1.0 / (1.0 + rms)) as f32)
}

fn finish_fit(timing: TimingFit, energy: (f64, f32)) -> ExplanationFit {
    ExplanationFit {
        timing_rms_frames: timing.rms,
        timing_max_frames: timing.maximum,
        timing_fit: timing.score,
        energy_rms: energy.0,
        energy_fit: energy.1,
        combined_fit: (timing.score + energy.1) * 0.5,
    }
}

#[derive(Clone)]
struct CandidateDraft {
    representation: PatternExplanationRepresentation,
    families: BTreeMap<usize, String>,
    fit: ExplanationFit,
    evidence: Vec<RhythmEvidenceRef>,
    derivations: Vec<PatternDerivation>,
}

fn merge_draft(existing: &mut CandidateDraft, mut incoming: CandidateDraft) {
    existing.evidence.append(&mut incoming.evidence);
    existing.evidence.sort();
    existing.evidence.dedup();
    existing.derivations.append(&mut incoming.derivations);
    existing.derivations.sort_by(|left, right| {
        left.rule
            .cmp(&right.rule)
            .then_with(|| left.premises.cmp(&right.premises))
    });
    existing.derivations.dedup();
    if incoming
        .fit
        .combined_fit
        .total_cmp(&existing.fit.combined_fit)
        .is_gt()
    {
        existing.fit = incoming.fit;
    }
}

fn finish_draft(draft: CandidateDraft, fit_penalty_bytes: u32) -> PatternExplanation {
    let description_bytes = match &draft.representation {
        PatternExplanationRepresentation::Term(term) => term.source.len() as u64,
        PatternExplanationRepresentation::ExactAudio(fallback) => fallback.estimated_literal_bytes,
    };
    let fit_loss = (1.0_f64 - f64::from(draft.fit.combined_fit.clamp(0.0, 1.0)))
        * f64::from(fit_penalty_bytes)
        * 1_000.0;
    let fit_penalty_millibytes = fit_loss.round().max(0.0) as u64;
    let description = DescriptionRank {
        description_bytes,
        fit_penalty_millibytes,
        total_millibytes: description_bytes
            .saturating_mul(1_000)
            .saturating_add(fit_penalty_millibytes),
    };
    let digest = alternative_digest(&draft.representation, &draft.evidence, draft.fit);
    PatternExplanation {
        id: PatternAlternativeId(digest),
        rank: 0,
        representation: draft.representation,
        families: draft.families,
        fit: draft.fit,
        description,
        evidence: draft.evidence,
        derivations: draft.derivations,
    }
}

fn compare_alternatives(
    left: &PatternExplanation,
    right: &PatternExplanation,
) -> std::cmp::Ordering {
    left.description
        .total_millibytes
        .cmp(&right.description.total_millibytes)
        .then_with(|| right.fit.combined_fit.total_cmp(&left.fit.combined_fit))
        .then_with(|| {
            left.description
                .description_bytes
                .cmp(&right.description.description_bytes)
        })
        .then_with(|| {
            representation_order(&left.representation)
                .cmp(&representation_order(&right.representation))
        })
        .then_with(|| left.id.cmp(&right.id))
}

fn representation_order(representation: &PatternExplanationRepresentation) -> u8 {
    match representation {
        PatternExplanationRepresentation::Term(_) => 0,
        PatternExplanationRepresentation::ExactAudio(_) => 1,
    }
}

fn alternative_digest(
    representation: &PatternExplanationRepresentation,
    evidence: &[RhythmEvidenceRef],
    fit: ExplanationFit,
) -> ContentDigest {
    let mut evidence_bytes = Vec::with_capacity(evidence.len() * 9);
    for premise in evidence {
        let (kind, id) = match premise {
            RhythmEvidenceRef::Pattern(id) => (0_u8, *id),
            RhythmEvidenceRef::Hit(id) => (1, *id),
            RhythmEvidenceRef::Family(id) => (2, *id),
            RhythmEvidenceRef::Tempo(id) => (3, *id),
            RhythmEvidenceRef::BeatPhase(id) => (4, *id),
        };
        evidence_bytes.push(kind);
        evidence_bytes.extend_from_slice(&(id as u64).to_le_bytes());
    }
    let mut fit_bytes = Vec::with_capacity(32);
    fit_bytes.extend_from_slice(&fit.timing_rms_frames.to_bits().to_le_bytes());
    fit_bytes.extend_from_slice(&fit.timing_max_frames.to_bits().to_le_bytes());
    fit_bytes.extend_from_slice(&fit.timing_fit.to_bits().to_le_bytes());
    fit_bytes.extend_from_slice(&fit.energy_rms.to_bits().to_le_bytes());
    fit_bytes.extend_from_slice(&fit.energy_fit.to_bits().to_le_bytes());
    fit_bytes.extend_from_slice(&fit.combined_fit.to_bits().to_le_bytes());
    match representation {
        PatternExplanationRepresentation::Term(term) => sha256_content(
            b"audec.pattern-explanation.alternative.term.v1",
            &[
                term.source.as_bytes(),
                &pattern_lang::bindings_hash(&term.bindings).0.to_le_bytes(),
                &evidence_bytes,
                &fit_bytes,
            ],
        ),
        PatternExplanationRepresentation::ExactAudio(fallback) => sha256_content(
            b"audec.pattern-explanation.alternative.exact-audio.v1",
            &[
                &(fallback.source_span.start as u64).to_le_bytes(),
                &(fallback.source_span.end as u64).to_le_bytes(),
                &fallback.estimated_literal_bytes.to_le_bytes(),
                &evidence_bytes,
                &fit_bytes,
            ],
        ),
    }
}

fn evidence_for_pattern(
    deprojection: &RhythmDeprojection,
    pattern_index: usize,
    hypothesis: &PatternHypothesis,
    selected: &BTreeSet<usize>,
) -> Vec<RhythmEvidenceRef> {
    if !hypothesis
        .family_sequence
        .iter()
        .any(|family| selected.contains(family))
    {
        return Vec::new();
    }
    let mut evidence = vec![RhythmEvidenceRef::Pattern(pattern_index)];
    evidence.extend(
        hypothesis
            .family_sequence
            .iter()
            .copied()
            .filter(|family| selected.contains(family))
            .map(RhythmEvidenceRef::Family),
    );
    for occurrence in &hypothesis.occurrences {
        for (sequence_index, family) in hypothesis.family_sequence.iter().enumerate() {
            if !selected.contains(family) {
                continue;
            }
            if let Some(index) = occurrence.event_index.checked_add(sequence_index) {
                if index < deprojection.hits.len() {
                    evidence.push(RhythmEvidenceRef::Hit(index));
                }
            }
        }
    }
    if let Some(tempo) = deprojection
        .tempo_hypotheses
        .iter()
        .find(|tempo| tempo.rank == 0)
    {
        evidence.push(RhythmEvidenceRef::Tempo(tempo.rank));
    }
    if let Some((index, _)) = deprojection
        .beat_phase_hypotheses
        .iter()
        .enumerate()
        .find(|(_, phase)| phase.tempo_rank == 0)
    {
        evidence.push(RhythmEvidenceRef::BeatPhase(index));
    }
    evidence.sort();
    evidence.dedup();
    evidence
}

fn all_evidence(
    deprojection: &RhythmDeprojection,
    selected: &BTreeSet<usize>,
) -> Vec<RhythmEvidenceRef> {
    let mut evidence = selected
        .iter()
        .copied()
        .map(RhythmEvidenceRef::Family)
        .collect::<Vec<_>>();
    evidence.extend(
        deprojection
            .hits
            .iter()
            .enumerate()
            .filter(|(_, hit)| hit.family.is_some_and(|family| selected.contains(&family)))
            .map(|(index, _)| RhythmEvidenceRef::Hit(index)),
    );
    for (index, pattern) in deprojection.patterns.iter().enumerate() {
        evidence.extend(evidence_for_pattern(deprojection, index, pattern, selected));
    }
    evidence.sort();
    evidence.dedup();
    evidence
}

fn derivation(rule: impl Into<String>, mut premises: Vec<RhythmEvidenceRef>) -> PatternDerivation {
    premises.sort();
    premises.dedup();
    PatternDerivation {
        rule: rule.into(),
        premises,
    }
}

fn family_names(events: &[PatternEvent]) -> BTreeMap<usize, String> {
    events
        .iter()
        .map(|event| (event.family, anonymous_binding(event.family)))
        .collect()
}

fn anonymous_binding(family: usize) -> String {
    format!("fam{family}")
}

fn nonzero_u64(bytes: &[u8]) -> u64 {
    let mut value = [0_u8; 8];
    value.copy_from_slice(bytes);
    u64::from_be_bytes(value).max(1)
}

fn gcd(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

/// Backend boundary for the actual construction renderer. Production wiring
/// should freeze the existing DAW engine's output; analysis code does not grow
/// a second sampler/transport. Exact-audio alternatives travel through the
/// same boundary but remain distinguishable by their representation variant.
pub trait PatternConstructionBackend: Send + Sync {
    fn freeze(
        &self,
        alternative: &PatternExplanation,
        extent: &ConcreteAspect,
        cancellation: &RenderCancellation,
    ) -> Result<Arc<dyn FrozenExplanationRenderer>, RhythmExplanationError>;
}

#[derive(Clone)]
struct RegisteredPatternExplanation {
    alternative: PatternExplanation,
    definition: ExplanationDefinition,
    extent: ConcreteAspect,
    dependencies: ExplanationDependencyPin,
}

/// `ExplanationCompiler` adapter consumed directly by `ComparisonRuntime`.
/// Registration binds project-local durable IDs to content-addressed search
/// alternatives; compile freezes the construction through the supplied
/// backend and returns the common compiled-explanation product.
pub struct PatternExplanationCompiler {
    backend: Arc<dyn PatternConstructionBackend>,
    entries: BTreeMap<ExplanationId, RegisteredPatternExplanation>,
}

impl PatternExplanationCompiler {
    pub fn new(backend: Arc<dyn PatternConstructionBackend>) -> Self {
        Self {
            backend,
            entries: BTreeMap::new(),
        }
    }

    pub fn register(
        &mut self,
        alternative: PatternExplanation,
        definition: ExplanationDefinition,
        extent: ConcreteAspect,
        dependencies: ExplanationDependencyPin,
    ) -> Result<(), RhythmExplanationError> {
        let mut normalized = definition.clone();
        normalized
            .normalize_and_validate()
            .map_err(RhythmExplanationError::Explanation)?;
        if normalized != definition {
            return Err(RhythmExplanationError::DefinitionMismatch(
                "definition is not canonical",
            ));
        }
        let ExplanationScope::ModelClaim { claim, .. } = &definition.scope else {
            return Err(RhythmExplanationError::DefinitionMismatch(
                "generated pattern definitions must use ModelClaim scope",
            ));
        };
        if *claim != alternative.claim_id() {
            return Err(RhythmExplanationError::DefinitionMismatch(
                "model-claim key does not match alternative content",
            ));
        }
        if extent.is_empty() {
            return Err(RhythmExplanationError::DefinitionMismatch(
                "compiled extent is empty",
            ));
        }
        if self.entries.contains_key(&definition.id) {
            return Err(RhythmExplanationError::DuplicateExplanation(definition.id));
        }
        self.entries.insert(
            definition.id,
            RegisteredPatternExplanation {
                alternative,
                definition,
                extent,
                dependencies,
            },
        );
        Ok(())
    }

    pub fn alternative(&self, id: ExplanationId) -> Option<&PatternExplanation> {
        self.entries.get(&id).map(|entry| &entry.alternative)
    }
}

impl ExplanationCompiler for PatternExplanationCompiler {
    fn compile(
        &self,
        definition: &ExplanationDefinition,
        cancellation: &RenderCancellation,
    ) -> Result<CompiledExplanation, ExplanationError> {
        if cancellation.is_cancelled() {
            return Err(ExplanationError::Cancelled);
        }
        let entry = self
            .entries
            .get(&definition.id)
            .ok_or(ExplanationError::MissingDefinition(definition.id))?;
        if &entry.definition != definition {
            return Err(ExplanationError::Unresolvable(
                "registered generated definition differs from durable store".to_owned(),
            ));
        }
        let renderer = self
            .backend
            .freeze(&entry.alternative, &entry.extent, cancellation)
            .map_err(|error| ExplanationError::Render(error.to_string()))?;
        CompiledExplanation::new(
            entry.definition.clone(),
            entry.extent.clone(),
            entry.dependencies.clone(),
            renderer,
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RhythmExplanationError {
    InvalidBudget(&'static str),
    InvalidDeprojection(&'static str),
    MissingFamily(usize),
    ClaimCollision(u64),
    DuplicateExplanation(ExplanationId),
    DefinitionMismatch(&'static str),
    Explanation(ExplanationError),
    Backend(String),
}

impl fmt::Display for RhythmExplanationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBudget(message) => write!(formatter, "invalid explain budget: {message}"),
            Self::InvalidDeprojection(message) => {
                write!(formatter, "invalid rhythm deprojection: {message}")
            }
            Self::MissingFamily(family) => write!(formatter, "rhythm family {family} is absent"),
            Self::ClaimCollision(claim) => write!(
                formatter,
                "generated pattern alternatives collide on local claim key {claim}"
            ),
            Self::DuplicateExplanation(id) => {
                write!(formatter, "explanation {} is already registered", id.0)
            }
            Self::DefinitionMismatch(message) => {
                write!(
                    formatter,
                    "generated explanation definition mismatch: {message}"
                )
            }
            Self::Explanation(error) => error.fmt(formatter),
            Self::Backend(message) => write!(formatter, "pattern construction backend: {message}"),
        }
    }
}

impl std::error::Error for RhythmExplanationError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_catalog::{ContentDigest, DigestAlgorithm};
    use crate::aspect::{BandSpan, ChannelMask, ConcreteRegion, SignalLayer};
    use crate::assets::{AssetFrameRange, AssetId, SampleFrames};
    use crate::audio::{AudioFormat, ProjectAudio};
    use crate::comparison::{ComparisonDefinition, ComparisonId, SourceCitation};
    use crate::comparison_runtime::{
        ComparisonRuntime, ComparisonRuntimeError, ComparisonSourceResolver,
        ResolvedComparisonSource,
    };
    use crate::coverage::CoverageRecipe;
    use crate::daw_project::ProjectRevisions;
    use crate::explanation::PcmExplanationRenderer;
    use crate::interpretation::{InterpretationCommand, InterpretationStore};
    use crate::ontology::Producer;
    use crate::rhythm::{
        BeatPhaseHypothesis, EventFamilyHypothesis, HitObservation, MedoidSampleReference,
        PatternOccurrence, TempoHypothesis, TempoRelation,
    };

    fn provenance() -> Provenance {
        Provenance {
            producer: Producer::Human { name: None },
            created_unix_ms: None,
            source_revision: None,
            note: None,
        }
    }

    fn deprojection(offsets: Vec<i32>, families: Vec<usize>) -> RhythmDeprojection {
        let hit_count = families.len().max(1) * 2;
        let hits = (0..hit_count)
            .map(|index| HitObservation {
                peak_sample: index * 125,
                onset_sample: index * 125,
                onset_seconds: index as f64 * 0.125,
                novelty_strength: 1.0 - (index % families.len().max(1)) as f32 * 0.1,
                family: Some(families[index % families.len().max(1)]),
                ..HitObservation::default()
            })
            .collect::<Vec<_>>();
        let unique_families = families.iter().copied().collect::<BTreeSet<_>>();
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
                beat_samples: vec![0, 500, 1_000],
            }],
            event_families: unique_families
                .into_iter()
                .map(|id| EventFamilyHypothesis {
                    id,
                    event_indices: Vec::new(),
                    medoid: MedoidSampleReference::default(),
                    mean_medoid_similarity: 1.0,
                    minimum_medoid_similarity: 1.0,
                    evidence: 1.0,
                })
                .collect(),
            patterns: vec![PatternHypothesis {
                family_sequence: families.clone(),
                step_offsets: offsets,
                occurrences: vec![
                    PatternOccurrence {
                        event_index: 0,
                        start_sample: 0,
                        beat_position: 0.0,
                    },
                    PatternOccurrence {
                        event_index: families.len(),
                        start_sample: families.len() * 125,
                        beat_position: 4.0,
                    },
                ],
                evidence: 1.0,
            }],
            ..RhythmDeprojection::default()
        }
    }

    #[test]
    fn deterministic_rank_keeps_competing_terms_and_anonymous_evidence() {
        let rhythm = deprojection(vec![0, 3, 6], vec![4, 4, 4]);
        let first = explain_rhythm(
            &rhythm,
            &[4],
            ExplainBudget {
                cycle_sixteenths: Some(8),
                ..ExplainBudget::default()
            },
        )
        .unwrap();
        let second = explain_rhythm(
            &rhythm,
            &[4],
            ExplainBudget {
                cycle_sixteenths: Some(8),
                ..ExplainBudget::default()
            },
        )
        .unwrap();
        assert_eq!(first, second);
        assert!(first
            .alternatives
            .iter()
            .filter_map(PatternExplanation::term)
            .any(|term| term.source.contains("e(3, 8, fam4)")));
        assert!(first.alternatives.len() >= 2);
        for alternative in &first.alternatives {
            assert_eq!(
                alternative.families.get(&4).map(String::as_str),
                Some("fam4")
            );
            assert!(alternative.evidence.contains(&RhythmEvidenceRef::Family(4)));
            assert!(!alternative.derivations.is_empty());
        }
        for term in first
            .alternatives
            .iter()
            .filter_map(PatternExplanation::term)
        {
            assert!(matches!(
                &term.pattern.origin,
                PatternOrigin::Expression { .. }
            ));
            term.pattern.validate().unwrap();
        }
    }

    #[test]
    fn fallback_grid_is_explicit_and_collision_keeps_literal_alternative() {
        let fallback = explain_rhythm(
            &deprojection(vec![0, 67], vec![2, 2]),
            &[2],
            ExplainBudget {
                cycle_sixteenths: Some(68),
                ..ExplainBudget::default()
            },
        )
        .unwrap();
        let term = fallback
            .alternatives
            .iter()
            .filter_map(PatternExplanation::term)
            .next()
            .unwrap();
        assert!(matches!(
            &term.grid,
            GridRealization::Fallback {
                exact_steps_required: 68,
                storage_steps: 16,
                nonzero_residues: 1,
                ..
            }
        ));

        let collision = explain_rhythm(
            &deprojection(vec![0, 1], vec![9, 9]),
            &[9],
            ExplainBudget {
                cycle_sixteenths: Some(128),
                ..ExplainBudget::default()
            },
        )
        .unwrap();
        assert!(collision
            .rejected_terms
            .iter()
            .any(|rejection| matches!(&rejection.reason, TermRejection::Collision { .. })));
        assert!(collision.alternatives.iter().any(|alternative| matches!(
            &alternative.representation,
            PatternExplanationRepresentation::ExactAudio(_)
        )));
    }

    struct StaticBackend {
        origin: i64,
        audio: ProjectAudio,
    }

    impl PatternConstructionBackend for StaticBackend {
        fn freeze(
            &self,
            _: &PatternExplanation,
            _: &ConcreteAspect,
            cancellation: &RenderCancellation,
        ) -> Result<Arc<dyn FrozenExplanationRenderer>, RhythmExplanationError> {
            if cancellation.is_cancelled() {
                return Err(RhythmExplanationError::Backend("cancelled".into()));
            }
            Ok(Arc::new(PcmExplanationRenderer {
                origin_frame: self.origin,
                audio: self.audio.clone(),
            }))
        }
    }

    struct StaticSource {
        origin: i64,
        audio: ProjectAudio,
    }

    impl ComparisonSourceResolver for StaticSource {
        fn resolve_source(
            &self,
            _: SourceCitation,
            _: &RenderCancellation,
        ) -> Result<ResolvedComparisonSource, ComparisonRuntimeError> {
            Ok(ResolvedComparisonSource {
                origin_frame: self.origin,
                audio: self.audio.clone(),
            })
        }
    }

    #[test]
    fn generated_term_flows_through_comparison_runtime_with_residual_and_excess() {
        let set = explain_rhythm(
            &deprojection(vec![0], vec![3]),
            &[3],
            ExplainBudget {
                cycle_sixteenths: Some(4),
                ..ExplainBudget::default()
            },
        )
        .unwrap();
        let alternative = set
            .alternatives
            .iter()
            .find(|alternative| alternative.term().is_some())
            .unwrap()
            .clone();
        let artifact = ArtifactId(ContentDigest::new(DigestAlgorithm::Sha256, [7; 32]));
        let definition = alternative.persistent_definition(
            ExplanationId(1),
            artifact,
            Aspect::Time(FrameSpan { start: 20, end: 24 }),
            provenance(),
        );
        let extent = ConcreteAspect::new(
            vec![ConcreteRegion {
                time: FrameSpan { start: 20, end: 24 },
                band: BandSpan::new(0.0, 4_000.0).unwrap(),
                channels: ChannelMask(1),
            }],
            SignalLayer::Source,
        )
        .unwrap();
        let format = AudioFormat::new(8_000, 1).unwrap();
        let source_audio =
            ProjectAudio::from_interleaved(format, vec![1.0, 2.0, 3.0, 4.0]).unwrap();
        let construction =
            ProjectAudio::from_interleaved(format, vec![1.0, 4.0, 3.0, 4.0]).unwrap();
        let mut compiler = PatternExplanationCompiler::new(Arc::new(StaticBackend {
            origin: 20,
            audio: construction,
        }));
        compiler
            .register(
                alternative,
                definition.clone(),
                extent,
                ExplanationDependencyPin::from_dependencies(
                    ProjectRevisions::default(),
                    [],
                    [artifact],
                ),
            )
            .unwrap();
        let comparison = ComparisonDefinition {
            id: ComparisonId(1),
            label: "generated rhythm term".to_owned(),
            source: SourceCitation {
                asset: AssetId(1),
                source_range: AssetFrameRange::new(SampleFrames(0), SampleFrames(4)).unwrap(),
                project_span: FrameSpan { start: 20, end: 24 },
                channels: ChannelMask(1),
            },
            explanation: definition.id,
            provenance: provenance(),
        };
        let mut interpretations = InterpretationStore::new();
        interpretations
            .apply(&[
                InterpretationCommand::PutExplanation {
                    before: None,
                    after: Some(definition),
                },
                InterpretationCommand::PutComparison {
                    before: None,
                    after: Some(comparison.clone()),
                },
            ])
            .unwrap();
        let sources = StaticSource {
            origin: 20,
            audio: source_audio,
        };
        let execution = ComparisonRuntime {
            interpretations: &interpretations,
            explanations: &compiler,
            sources: &sources,
        }
        .execute(
            &comparison,
            CoverageRecipe {
                fft_size: 4,
                hop_size: 1,
                power_floor: 1.0e-12,
            },
            &RenderCancellation::new(),
        )
        .unwrap();
        assert_eq!(
            execution.rendered.residual.interleaved(),
            &[0.0, -2.0, 0.0, 0.0]
        );
        assert!(execution.coverage.summary.excess_energy_ratio > 0.0);
        assert_eq!(
            execution.compiled.evidence(),
            &[ExplanationEvidenceRef::Artifact(artifact)]
        );
    }
}
