//! Pure reverse-to-forward planning for detected rhythm patterns.
//!
//! A promotion turns one explicitly chosen pattern hypothesis into ordinary
//! sampler pads, step events, and an arrangement placement. Competing
//! tempo/phase readings remain separate plans: this module never collapses
//! them into a silently asserted grid, and anonymous event families never
//! acquire guessed instrument names.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::assets::{AssetFrameRange, ContentFingerprint, SampleFrames};
use crate::constructive::{
    ConstructiveCause, ConstructiveEditPlan, ConstructiveFocus, KitMutation, MaterialReusePolicy,
    PatternPlacementIntent, PatternSeed, PlannedMaterial, PlannedPattern, PlannedPatternId,
    PlannedStep,
};
use crate::live_project::ProjectController;
use crate::mixer::BusId;
use crate::rhythm::{
    BeatPhaseHypothesis, EventFamilyHypothesis, PatternHypothesis, RhythmDeprojection,
    TempoRelation,
};
use crate::sample_actions::SampleSelection;
use crate::sample_kit::{SampleKit, SamplePad, SampleRouteIntent, SampleZone};
use crate::sample_material::{
    extract_virtual_slice, DerivationScope, SampleMaterialProvenance, ScopedEvidenceRef,
    ScopedProposalRef, SourceMaterialRef, VirtualSliceRef,
};
use crate::sequencer::{BeatDuration, BeatTime, PPQ};

use super::constructive_controller::{
    choose_output_bus, ConstructiveControllerError, ConstructiveSourceSnapshot,
};

const PATTERN_STEPS_PER_QUARTER: u16 = 4;
const FNV_OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
const FNV_PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

/// User intent for compiling one analyzer pattern into editable material.
/// Analysis coordinates are relative to `source`; an exact range match is
/// required so durable citations can be translated to asset coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RhythmPromotionIntent {
    pub source: SampleSelection,
    pub pattern_index: usize,
    pub target_bus: Option<BusId>,
}

/// One retained interpretation of musical time. `phase_source_frame` uses the
/// analysis/selection coordinate system; exact event frames remain separately
/// attached to every planned step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RhythmGridHypothesis {
    pub beat_phase_index: usize,
    pub tempo_rank: usize,
    pub bpm: f32,
    pub phase_source_frame: i64,
    pub support: f32,
    pub tempo_evidence: Option<f32>,
    pub tempo_relation: Option<TempoRelation>,
    pub steps_per_quarter: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RhythmPromotionDiagnosticCode {
    CompetingGridHypothesesRetained,
    TempoWithoutBeatPhase,
    RepresentativeOccurrenceChosen,
    ProjectTempoDiffers,
    MicrotimingRoundedToTicks,
    NegativeSourcePlacementClamped,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RhythmPromotionDiagnostic {
    pub code: RhythmPromotionDiagnosticCode,
    pub path: String,
    pub message: String,
}

impl RhythmPromotionDiagnostic {
    fn new(
        code: RhythmPromotionDiagnosticCode,
        path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            path: path.into(),
            message: message.into(),
        }
    }
}

/// A complete, mutually exclusive application choice. Alternatives are
/// planned against the same aggregate revision and intentionally claim the
/// same next project IDs, so applying one makes all siblings stale.
#[derive(Clone, Debug)]
pub struct RhythmPromotionAlternative {
    pub grid: RhythmGridHypothesis,
    pub plan: ConstructiveEditPlan,
    pub diagnostics: Vec<RhythmPromotionDiagnostic>,
}

#[derive(Clone, Debug)]
pub struct RhythmPromotionSet {
    pub pattern_index: usize,
    pub pattern_evidence: f32,
    pub occurrence_index: usize,
    pub alternatives: Vec<RhythmPromotionAlternative>,
    pub diagnostics: Vec<RhythmPromotionDiagnostic>,
}

impl RhythmPromotionSet {
    /// Evidence-ranked is a preview affordance, not a user selection.
    pub fn evidence_preferred(&self) -> Option<&RhythmPromotionAlternative> {
        self.alternatives.first()
    }

    pub fn alternative_for_phase(
        &self,
        beat_phase_index: usize,
    ) -> Option<&RhythmPromotionAlternative> {
        self.alternatives
            .iter()
            .find(|alternative| alternative.grid.beat_phase_index == beat_phase_index)
    }
}

impl ProjectController {
    /// Plan every retained grid alternative without mutating project state.
    /// The caller must explicitly choose an alternative and submit its plan
    /// through `execute_constructive_plan`.
    pub fn plan_rhythm_promotion(
        &self,
        rhythm: &RhythmDeprojection,
        intent: RhythmPromotionIntent,
    ) -> Result<RhythmPromotionSet, RhythmPromotionError> {
        let source = self
            .constructive_source_snapshot(intent.source)
            .map_err(RhythmPromotionError::Controller)?;
        validate_source(rhythm, &source)?;
        let pattern = rhythm
            .patterns
            .get(intent.pattern_index)
            .ok_or(RhythmPromotionError::MissingPattern(intent.pattern_index))?;
        validate_pattern_shape(pattern)?;
        let (occurrence_index, event_indices) = representative_occurrence(rhythm, pattern)?;
        let output_bus = choose_output_bus(self.snapshot(), intent.target_bus, "Rhythm reading")
            .map_err(RhythmPromotionError::Controller)?;
        let source_content = self
            .snapshot()
            .project
            .state()
            .domains
            .assets
            .get(intent.source.asset)
            .expect("constructive_source_snapshot already resolved this asset")
            .content();
        let scope = rhythm_scope(rhythm, &source, source_content, intent.pattern_index);

        let mut indexed_phases = rhythm
            .beat_phase_hypotheses
            .iter()
            .enumerate()
            .collect::<Vec<_>>();
        indexed_phases.sort_by(|(left_index, left), (right_index, right)| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.tempo_rank.cmp(&right.tempo_rank))
                .then_with(|| left_index.cmp(right_index))
        });
        if indexed_phases.is_empty() {
            return Err(RhythmPromotionError::MissingGridHypothesis);
        }

        let shared = build_shared_material(
            self,
            rhythm,
            pattern,
            &source,
            output_bus,
            scope,
            intent.pattern_index,
        )?;
        let project_bpm = self
            .snapshot()
            .project
            .state()
            .domains
            .sequencer
            .tempo_map()
            .tempo_at(BeatTime::ZERO)
            .bpm();
        let mut alternatives = Vec::with_capacity(indexed_phases.len());
        for (phase_index, phase) in indexed_phases {
            alternatives.push(build_alternative(
                rhythm,
                pattern,
                intent.pattern_index,
                occurrence_index,
                &event_indices,
                phase_index,
                phase,
                &source,
                &shared,
                scope,
                project_bpm,
            )?);
        }

        let mut diagnostics = Vec::new();
        if alternatives.len() > 1 {
            diagnostics.push(RhythmPromotionDiagnostic::new(
                RhythmPromotionDiagnosticCode::CompetingGridHypothesesRetained,
                "rhythm.beat_phase_hypotheses",
                format!(
                    "retained {} tempo/phase interpretations as separate constructive plans",
                    alternatives.len()
                ),
            ));
        }
        if pattern.occurrences.len() > 1 {
            diagnostics.push(RhythmPromotionDiagnostic::new(
                RhythmPromotionDiagnosticCode::RepresentativeOccurrenceChosen,
                format!("rhythm.patterns[{}].occurrences", intent.pattern_index),
                format!(
                    "editable event details use exact occurrence {occurrence_index}; all {} detected occurrences remain analysis evidence",
                    pattern.occurrences.len()
                ),
            ));
        }
        for tempo in &rhythm.tempo_hypotheses {
            if !rhythm
                .beat_phase_hypotheses
                .iter()
                .any(|phase| phase.tempo_rank == tempo.rank)
            {
                diagnostics.push(RhythmPromotionDiagnostic::new(
                    RhythmPromotionDiagnosticCode::TempoWithoutBeatPhase,
                    format!("rhythm.tempo_hypotheses[rank={}]", tempo.rank),
                    "the pulse remains analysis evidence, but no beat-phase origin exists from which to author a grid alternative",
                ));
            }
        }
        Ok(RhythmPromotionSet {
            pattern_index: intent.pattern_index,
            pattern_evidence: pattern.evidence,
            occurrence_index,
            alternatives,
            diagnostics,
        })
    }
}

#[derive(Clone)]
struct SharedMaterial {
    materials: Vec<PlannedMaterial>,
    kit: SampleKit,
    pad_by_family: BTreeMap<usize, crate::sample_kit::PadId>,
}

#[allow(clippy::too_many_arguments)]
fn build_shared_material(
    controller: &ProjectController,
    rhythm: &RhythmDeprojection,
    pattern: &PatternHypothesis,
    source: &ConstructiveSourceSnapshot,
    output_bus: BusId,
    scope: DerivationScope,
    pattern_index: usize,
) -> Result<SharedMaterial, RhythmPromotionError> {
    let mut library = controller
        .snapshot()
        .project
        .state()
        .domains
        .sample_kits
        .clone();
    let kit_id = library.allocate_kit_id().map_err(domain)?;
    let mut kit = SampleKit::new(
        kit_id,
        format!("Rhythm reading {}", pattern_index + 1),
        SampleRouteIntent::new(output_bus).map_err(domain)?,
    );
    kit.revision = 1;
    let seed_proposal = ScopedProposalRef {
        scope,
        local: promotion_local(pattern_index, 0)?,
    };
    let mut pad_by_family = BTreeMap::new();
    let mut materials = Vec::new();
    for &family_id in &pattern.family_sequence {
        if pad_by_family.contains_key(&family_id) {
            continue;
        }
        let family = rhythm
            .event_families
            .iter()
            .find(|candidate| candidate.id == family_id)
            .ok_or(RhythmPromotionError::MissingFamily(family_id))?;
        validate_family(rhythm, family)?;
        let pad_id = library.allocate_pad_id().map_err(domain)?;
        let zone_id = library.allocate_zone_id().map_err(domain)?;
        let range = absolute_range(
            source,
            family.medoid.excerpt.start,
            family.medoid.excerpt.end,
        )?;
        let slice = VirtualSliceRef::new(source.selection.asset, range).map_err(material)?;
        let extracted = extract_virtual_slice(slice, &source.pcm).map_err(material)?;
        let evidence = vec![
            evidence_ref(scope, EvidenceKind::Family, family_id)?,
            evidence_ref(scope, EvidenceKind::Hit, family.medoid.event_index)?,
        ];
        let mut zone = SampleZone::new(zone_id, pad_id, SourceMaterialRef::VirtualSlice(slice));
        zone.decoded_pcm = Some(extracted.identity);
        zone.provenance = SampleMaterialProvenance::Deprojection {
            // Phase zero is a valid seed and is replaced when assembling
            // every other mutually exclusive grid alternative below.
            proposal: seed_proposal,
            evidence: evidence.clone(),
        };
        zone.evidence = evidence.into_iter().collect();
        let mut pad = SamplePad::new(pad_id, format!("Anonymous family {family_id}"));
        pad.zone_order.push(zone_id);
        kit.pad_order.push(pad_id);
        kit.pads.insert(pad_id, pad);
        kit.zones.insert(zone_id, zone);
        materials.push(PlannedMaterial {
            zone: zone_id,
            slice,
            decoded_pcm: extracted.identity,
            reuse: MaterialReusePolicy::RequireNew,
        });
        pad_by_family.insert(family_id, pad_id);
    }
    Ok(SharedMaterial {
        materials,
        kit,
        pad_by_family,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_alternative(
    rhythm: &RhythmDeprojection,
    pattern: &PatternHypothesis,
    pattern_index: usize,
    occurrence_index: usize,
    event_indices: &[usize],
    phase_index: usize,
    phase: &BeatPhaseHypothesis,
    source: &ConstructiveSourceSnapshot,
    shared: &SharedMaterial,
    scope: DerivationScope,
    project_bpm: f64,
) -> Result<RhythmPromotionAlternative, RhythmPromotionError> {
    if !phase.bpm.is_finite() || phase.bpm <= 0.0 || !phase.score.is_finite() {
        return Err(RhythmPromotionError::InvalidGridHypothesis(phase_index));
    }
    let tempo = rhythm
        .tempo_hypotheses
        .iter()
        .find(|tempo| tempo.rank == phase.tempo_rank);
    let phase_source_frame = seconds_to_frame(phase.phase_seconds, rhythm.sample_rate)?;
    let grid = RhythmGridHypothesis {
        beat_phase_index: phase_index,
        tempo_rank: phase.tempo_rank,
        bpm: phase.bpm,
        phase_source_frame,
        support: phase.score.clamp(0.0, 1.0),
        tempo_evidence: tempo.map(|tempo| tempo.evidence),
        tempo_relation: tempo.map(|tempo| tempo.relation),
        steps_per_quarter: PATTERN_STEPS_PER_QUARTER,
    };
    let proposal = ScopedProposalRef {
        scope,
        local: promotion_local(pattern_index, phase_index)?,
    };
    let mut kit = shared.kit.clone();
    for zone in kit.zones.values_mut() {
        if let SampleMaterialProvenance::Deprojection {
            proposal: zone_proposal,
            ..
        } = &mut zone.provenance
        {
            *zone_proposal = proposal;
        }
    }
    let resolution = (PPQ / i64::from(PATTERN_STEPS_PER_QUARTER)) as u64;
    let minimum_offset = pattern.step_offsets.iter().copied().min().unwrap_or(0);
    let maximum_offset = pattern.step_offsets.iter().copied().max().unwrap_or(0);
    let first_hit = &rhythm.hits[event_indices[0]];
    let base_grid_tick = quantized_tick(
        first_hit.onset_sample as u64,
        phase.bpm,
        phase_source_frame,
        rhythm.sample_rate,
        resolution as i64,
    );
    let placement_tick = base_grid_tick.saturating_add(
        i64::from(minimum_offset.saturating_sub(pattern.step_offsets[0]))
            .saturating_mul(resolution as i64),
    );
    let placement_start = BeatTime(placement_tick.max(0));
    let bar_steps = u64::from(PATTERN_STEPS_PER_QUARTER) * 4;
    let occupied_steps = u64::try_from(maximum_offset.saturating_sub(minimum_offset))
        .map_err(|_| RhythmPromotionError::TimingOverflow)?
        .saturating_add(1);
    let cycle_steps = occupied_steps.div_ceil(bar_steps).max(1) * bar_steps;
    let cycle = BeatDuration(
        cycle_steps
            .checked_mul(resolution)
            .ok_or(RhythmPromotionError::TimingOverflow)?,
    );
    let maximum_novelty = event_indices
        .iter()
        .map(|&index| rhythm.hits[index].novelty_strength.max(0.0))
        .fold(0.0_f32, f32::max)
        .max(f32::EPSILON);
    let mut diagnostics = Vec::new();
    if placement_tick < 0 {
        diagnostics.push(RhythmPromotionDiagnostic::new(
            RhythmPromotionDiagnosticCode::NegativeSourcePlacementClamped,
            format!("rhythm.patterns[{pattern_index}].occurrences[{occurrence_index}]"),
            "the hypothesized source-time placement precedes project tick zero; the editable clip starts at zero while exact source frames remain attached",
        ));
    }
    if (project_bpm - f64::from(phase.bpm)).abs() > 0.001 {
        diagnostics.push(RhythmPromotionDiagnostic::new(
            RhythmPromotionDiagnosticCode::ProjectTempoDiffers,
            format!("rhythm.beat_phase_hypotheses[{phase_index}]"),
            format!(
                "this reading is {:.3} BPM while the project begins at {:.3} BPM; choose/adopt musical time separately before expecting frame-exact sequencer playback",
                phase.bpm, project_bpm
            ),
        ));
    }

    let mut steps = Vec::with_capacity(event_indices.len());
    let mut cause_evidence = BTreeSet::new();
    cause_evidence.insert(evidence_ref(scope, EvidenceKind::Pattern, pattern_index)?);
    cause_evidence.insert(evidence_ref(scope, EvidenceKind::Tempo, phase.tempo_rank)?);
    cause_evidence.insert(evidence_ref(scope, EvidenceKind::Phase, phase_index)?);
    for (token_index, (&family_id, &event_index)) in pattern
        .family_sequence
        .iter()
        .zip(event_indices)
        .enumerate()
    {
        let hit = &rhythm.hits[event_index];
        let relative_steps = pattern.step_offsets[token_index].saturating_sub(minimum_offset);
        let at = i64::from(relative_steps)
            .checked_mul(resolution as i64)
            .ok_or(RhythmPromotionError::TimingOverflow)?;
        let predicted_tick = base_grid_tick.saturating_add(
            i64::from(pattern.step_offsets[token_index].saturating_sub(pattern.step_offsets[0]))
                .saturating_mul(resolution as i64),
        );
        let predicted_frame = tick_to_frame(
            predicted_tick,
            phase.bpm,
            phase_source_frame,
            rhythm.sample_rate,
        );
        let micro_frames = (hit.onset_sample as i128 - predicted_frame as i128)
            .clamp(i64::MIN as i128, i64::MAX as i128) as i64;
        let micro_ticks = frames_to_ticks(micro_frames, phase.bpm, rhythm.sample_rate);
        if !micro_offset_is_exact(micro_frames, micro_ticks, phase.bpm, rhythm.sample_rate) {
            diagnostics.push(RhythmPromotionDiagnostic::new(
                RhythmPromotionDiagnosticCode::MicrotimingRoundedToTicks,
                format!("rhythm.patterns[{pattern_index}].tokens[{token_index}]"),
                "sequencer microtiming is PPQ-rounded; the exact frame residual remains on the planned step",
            ));
        }
        let evidence = vec![
            evidence_ref(scope, EvidenceKind::Hit, event_index)?,
            evidence_ref(scope, EvidenceKind::Family, family_id)?,
            evidence_ref(scope, EvidenceKind::Pattern, pattern_index)?,
            evidence_ref(scope, EvidenceKind::Tempo, phase.tempo_rank)?,
            evidence_ref(scope, EvidenceKind::Phase, phase_index)?,
        ];
        cause_evidence.extend(evidence.iter().copied());
        let exact_source_onset_frame = source
            .source_range
            .start
            .0
            .checked_add(hit.onset_sample as u64)
            .ok_or(RhythmPromotionError::TimingOverflow)?;
        steps.push(PlannedStep {
            pad: shared.pad_by_family[&family_id],
            at: BeatTime(at),
            gate: BeatDuration(resolution),
            velocity: (hit.novelty_strength.max(0.0) / maximum_novelty)
                .sqrt()
                .clamp(0.0, 1.0),
            probability: 1.0,
            ratchets: 1,
            pitch_semitones: 0.0,
            pan: 0.0,
            micro_offset_ticks: micro_ticks,
            original_micro_offset_frames: Some(micro_frames),
            exact_source_onset_frame: Some(exact_source_onset_frame),
            evidence,
        });
    }

    let planned_id = PlannedPatternId::from_raw(1);
    let bindings = shared
        .pad_by_family
        .iter()
        .map(|(&family, &pad)| (format!("family_{family}"), pad))
        .collect();
    let plan = ConstructiveEditPlan::new(
        format!("Promote rhythm pattern {}", pattern_index + 1),
        source.project_revision,
        vec![ConstructiveCause::Deprojection {
            proposal,
            evidence: cause_evidence.into_iter().collect(),
        }],
        shared.materials.clone(),
        KitMutation {
            before: None,
            after: kit,
        },
        Some(PlannedPattern {
            id: planned_id,
            name: format!("Anonymous rhythm pattern {}", pattern_index + 1),
            cycle,
            seed: PatternSeed::Deprojected {
                proposal,
                resolution: BeatDuration(resolution),
                expression: None,
                diverged: false,
            },
            bindings,
            steps,
        }),
        Some(PatternPlacementIntent {
            pattern: planned_id,
            start: placement_start,
            length: cycle,
            pattern_offset: BeatTime::ZERO,
            looped: true,
            transpose_semitones: 0.0,
            gain: 1.0,
        }),
        ConstructiveFocus::Pattern(planned_id),
    )
    .map_err(domain)?;
    Ok(RhythmPromotionAlternative {
        grid,
        plan,
        diagnostics,
    })
}

fn validate_source(
    rhythm: &RhythmDeprojection,
    source: &ConstructiveSourceSnapshot,
) -> Result<(), RhythmPromotionError> {
    let source_frames = source.source_range.len().0;
    if rhythm.sample_rate == 0
        || rhythm.sample_rate != source.pcm.format.sample_rate.get()
        || rhythm.sample_frames as u64 != source_frames
    {
        return Err(RhythmPromotionError::SourceShapeMismatch {
            analysis_rate: rhythm.sample_rate,
            pcm_rate: source.pcm.format.sample_rate.get(),
            analysis_frames: rhythm.sample_frames as u64,
            selected_frames: source_frames,
        });
    }
    Ok(())
}

fn validate_pattern_shape(pattern: &PatternHypothesis) -> Result<(), RhythmPromotionError> {
    if pattern.family_sequence.is_empty()
        || pattern.family_sequence.len() != pattern.step_offsets.len()
        || pattern.occurrences.is_empty()
        || !pattern.evidence.is_finite()
    {
        return Err(RhythmPromotionError::InvalidPatternShape);
    }
    Ok(())
}

fn validate_family(
    rhythm: &RhythmDeprojection,
    family: &EventFamilyHypothesis,
) -> Result<(), RhythmPromotionError> {
    if family.medoid.event_index >= rhythm.hits.len()
        || family.medoid.excerpt.is_empty()
        || family.medoid.excerpt.end > rhythm.sample_frames
    {
        return Err(RhythmPromotionError::InvalidFamily(family.id));
    }
    Ok(())
}

fn representative_occurrence(
    rhythm: &RhythmDeprojection,
    pattern: &PatternHypothesis,
) -> Result<(usize, Vec<usize>), RhythmPromotionError> {
    let mut indexed = pattern.occurrences.iter().enumerate().collect::<Vec<_>>();
    indexed.sort_by_key(|(index, occurrence)| {
        (occurrence.start_sample, occurrence.event_index, *index)
    });
    for (occurrence_index, occurrence) in indexed {
        if occurrence.event_index >= rhythm.hits.len() {
            continue;
        }
        let limit = pattern
            .occurrences
            .iter()
            .filter_map(|candidate| {
                (candidate.event_index > occurrence.event_index).then_some(candidate.event_index)
            })
            .min()
            .unwrap_or(rhythm.hits.len())
            .min(rhythm.hits.len());
        let mut cursor = occurrence.event_index;
        let mut events = Vec::with_capacity(pattern.family_sequence.len());
        for &family in &pattern.family_sequence {
            let Some(relative) = rhythm.hits[cursor.min(limit)..limit]
                .iter()
                .position(|hit| hit.family == Some(family))
            else {
                events.clear();
                break;
            };
            cursor = cursor.saturating_add(relative);
            events.push(cursor);
            cursor = cursor.saturating_add(1);
        }
        if events.len() == pattern.family_sequence.len() {
            return Ok((occurrence_index, events));
        }
    }
    Err(RhythmPromotionError::PatternOccurrenceMismatch)
}

fn absolute_range(
    source: &ConstructiveSourceSnapshot,
    start: usize,
    end: usize,
) -> Result<AssetFrameRange, RhythmPromotionError> {
    let absolute_start = source
        .source_range
        .start
        .0
        .checked_add(start as u64)
        .ok_or(RhythmPromotionError::TimingOverflow)?;
    let absolute_end = source
        .source_range
        .start
        .0
        .checked_add(end as u64)
        .ok_or(RhythmPromotionError::TimingOverflow)?;
    if absolute_end > source.source_range.end.0 {
        return Err(RhythmPromotionError::SourceSpanOutsideSelection { start, end });
    }
    AssetFrameRange::new(SampleFrames(absolute_start), SampleFrames(absolute_end))
        .map_err(|_| RhythmPromotionError::SourceSpanOutsideSelection { start, end })
}

#[derive(Clone, Copy)]
enum EvidenceKind {
    Hit = 1,
    Family = 2,
    Pattern = 3,
    Tempo = 4,
    Phase = 5,
}

fn evidence_ref(
    scope: DerivationScope,
    kind: EvidenceKind,
    index: usize,
) -> Result<ScopedEvidenceRef, RhythmPromotionError> {
    let local = u64::try_from(index)
        .ok()
        .and_then(|index| index.checked_add(1))
        .filter(|index| *index < (1_u64 << 56))
        .ok_or(RhythmPromotionError::IdentityOverflow)?;
    Ok(ScopedEvidenceRef {
        scope,
        local: ((kind as u64) << 56) | local,
    })
}

fn promotion_local(pattern_index: usize, phase_index: usize) -> Result<u64, RhythmPromotionError> {
    let pattern = u32::try_from(pattern_index)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or(RhythmPromotionError::IdentityOverflow)?;
    let phase = u32::try_from(phase_index)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or(RhythmPromotionError::IdentityOverflow)?;
    Ok((u64::from(pattern) << 32) | u64::from(phase))
}

fn rhythm_scope(
    rhythm: &RhythmDeprojection,
    source: &ConstructiveSourceSnapshot,
    source_content: ContentFingerprint,
    pattern_index: usize,
) -> DerivationScope {
    fn part(hash: &mut u128, bytes: &[u8]) {
        for byte in bytes {
            *hash ^= u128::from(*byte);
            *hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    let mut hash = FNV_OFFSET;
    part(&mut hash, b"audec.rhythm-promotion.v1\0");
    part(&mut hash, &source.selection.asset.0.to_le_bytes());
    part(&mut hash, &source.source_range.start.0.to_le_bytes());
    part(&mut hash, &source.source_range.end.0.to_le_bytes());
    part(&mut hash, &source_content.id.0.to_le_bytes());
    part(&mut hash, &source_content.bytes_hashed.to_le_bytes());
    part(&mut hash, &rhythm.sample_rate.to_le_bytes());
    part(&mut hash, &(rhythm.sample_frames as u64).to_le_bytes());
    part(&mut hash, &(pattern_index as u64).to_le_bytes());
    for (event_index, hit) in rhythm.hits.iter().enumerate() {
        part(&mut hash, &(event_index as u64).to_le_bytes());
        part(&mut hash, &(hit.span.start as u64).to_le_bytes());
        part(&mut hash, &(hit.span.end as u64).to_le_bytes());
        part(&mut hash, &(hit.onset_sample as u64).to_le_bytes());
        part(&mut hash, &(hit.peak_sample as u64).to_le_bytes());
        part(
            &mut hash,
            &hit.family
                .map_or(u64::MAX, |family| family as u64)
                .to_le_bytes(),
        );
        part(&mut hash, &hit.novelty_strength.to_bits().to_le_bytes());
        part(&mut hash, &hit.family_similarity.to_bits().to_le_bytes());
    }
    for family in &rhythm.event_families {
        part(&mut hash, &(family.id as u64).to_le_bytes());
        part(&mut hash, &(family.medoid.event_index as u64).to_le_bytes());
        part(
            &mut hash,
            &(family.medoid.excerpt.start as u64).to_le_bytes(),
        );
        part(&mut hash, &(family.medoid.excerpt.end as u64).to_le_bytes());
        for event_index in &family.event_indices {
            part(&mut hash, &(*event_index as u64).to_le_bytes());
        }
        part(&mut hash, &family.evidence.to_bits().to_le_bytes());
    }
    if let Some(pattern) = rhythm.patterns.get(pattern_index) {
        for family in &pattern.family_sequence {
            part(&mut hash, &(*family as u64).to_le_bytes());
        }
        for offset in &pattern.step_offsets {
            part(&mut hash, &offset.to_le_bytes());
        }
        for occurrence in &pattern.occurrences {
            part(&mut hash, &(occurrence.event_index as u64).to_le_bytes());
            part(&mut hash, &(occurrence.start_sample as u64).to_le_bytes());
            part(&mut hash, &occurrence.beat_position.to_bits().to_le_bytes());
        }
        part(&mut hash, &pattern.evidence.to_bits().to_le_bytes());
    }
    for phase in &rhythm.beat_phase_hypotheses {
        part(&mut hash, &(phase.tempo_rank as u64).to_le_bytes());
        part(&mut hash, &phase.bpm.to_bits().to_le_bytes());
        part(&mut hash, &phase.phase_seconds.to_bits().to_le_bytes());
        part(&mut hash, &phase.score.to_bits().to_le_bytes());
    }
    DerivationScope(hash)
}

fn quantized_tick(
    frame: u64,
    bpm: f32,
    phase_frame: i64,
    sample_rate: u32,
    resolution: i64,
) -> i64 {
    let raw = frame_to_tick(frame, bpm, phase_frame, sample_rate);
    let lower = raw.div_euclid(resolution);
    let remainder = raw.rem_euclid(resolution);
    lower
        .saturating_add(i64::from(remainder.saturating_mul(2) >= resolution))
        .saturating_mul(resolution)
}

fn frame_to_tick(frame: u64, bpm: f32, phase_frame: i64, sample_rate: u32) -> i64 {
    let relative = frame as i128 - phase_frame as i128;
    let bpm_micros = (f64::from(bpm) * 1_000_000.0).round() as i128;
    let numerator = relative
        .saturating_mul(PPQ as i128)
        .saturating_mul(bpm_micros);
    let denominator = (sample_rate as i128).saturating_mul(60_000_000_i128);
    numerator
        .div_euclid(denominator.max(1))
        .clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn tick_to_frame(tick: i64, bpm: f32, phase_frame: i64, sample_rate: u32) -> i64 {
    let bpm_micros = (f64::from(bpm) * 1_000_000.0).round() as i128;
    let relative = (tick as i128)
        .saturating_mul(sample_rate as i128)
        .saturating_mul(60_000_000_i128)
        / (PPQ as i128).saturating_mul(bpm_micros.max(1));
    (relative + phase_frame as i128).clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn seconds_to_frame(seconds: f64, sample_rate: u32) -> Result<i64, RhythmPromotionError> {
    let frame = seconds * f64::from(sample_rate);
    if !frame.is_finite() || frame < i64::MIN as f64 || frame > i64::MAX as f64 {
        return Err(RhythmPromotionError::InvalidPhase);
    }
    Ok(frame.round() as i64)
}

fn frames_to_ticks(frames: i64, bpm: f32, sample_rate: u32) -> i32 {
    let ticks = frames as f64 * f64::from(bpm) * PPQ as f64 / (f64::from(sample_rate) * 60.0);
    ticks.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

fn micro_offset_is_exact(frames: i64, ticks: i32, bpm: f32, sample_rate: u32) -> bool {
    let reconstructed =
        f64::from(ticks) * f64::from(sample_rate) * 60.0 / (f64::from(bpm) * PPQ as f64);
    (reconstructed - frames as f64).abs() < 0.5
}

fn domain(error: impl fmt::Display) -> RhythmPromotionError {
    RhythmPromotionError::Constructive(error.to_string())
}

fn material(error: impl fmt::Display) -> RhythmPromotionError {
    RhythmPromotionError::Material(error.to_string())
}

#[derive(Debug)]
pub enum RhythmPromotionError {
    Controller(ConstructiveControllerError),
    MissingPattern(usize),
    MissingFamily(usize),
    InvalidFamily(usize),
    InvalidPatternShape,
    PatternOccurrenceMismatch,
    MissingGridHypothesis,
    InvalidGridHypothesis(usize),
    InvalidPhase,
    SourceShapeMismatch {
        analysis_rate: u32,
        pcm_rate: u32,
        analysis_frames: u64,
        selected_frames: u64,
    },
    SourceSpanOutsideSelection {
        start: usize,
        end: usize,
    },
    IdentityOverflow,
    TimingOverflow,
    Material(String),
    Constructive(String),
}

impl fmt::Display for RhythmPromotionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl Error for RhythmPromotionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        AssetRegistry, ContentFingerprint, DecodedAudioMetadata,
    };
    use crate::audio::AudioFormat;
    use crate::daw_engine::{compile_daw_engine, DawEngineConfig};
    use crate::daw_render::{PcmAsset, RenderCancellation, RenderWindow};
    use crate::live_project::{LiveProject, SourceMaterialMetadata};
    use crate::project_codecs::encode_constructive;
    use crate::rhythm::{
        AnalysisStatus, HitObservation, MedoidSampleReference, PatternOccurrence, SampleSpan,
        TempoHypothesis,
    };

    const RATE: u32 = 1_000;
    const FRAMES: usize = 600;

    /// Compare durable authored content while deliberately excluding the
    /// operational metadata that must advance even when a command is undone.
    /// Revisions and `next_*` identity watermarks are not creative content;
    /// rewinding them would allow a deleted identity to be reused.
    fn canonical_creative_content(
        project: &crate::daw_project::DawProject,
    ) -> crate::project_codecs::DomainPayloads {
        fn strip_operational_metadata(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(fields) => {
                    fields.retain(|name, _| name != "revision" && !name.starts_with("next_"));
                    for value in fields.values_mut() {
                        strip_operational_metadata(value);
                    }
                }
                serde_json::Value::Array(values) => {
                    for value in values {
                        strip_operational_metadata(value);
                    }
                }
                _ => {}
            }
        }

        let mut payloads = encode_constructive(project).unwrap();
        for bytes in payloads.0.values_mut() {
            let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
            strip_operational_metadata(&mut value);
            *bytes = serde_json::to_vec(&value).unwrap();
        }
        payloads
    }

    fn controller() -> (ProjectController, crate::assets::AssetId) {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/audio/rhythm-reading.wav").unwrap()),
            None,
        )
        .unwrap();
        let mut registry = AssetRegistry::new();
        let asset = registry
            .register(AssetRegistration {
                name: "rhythm reading source".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: RATE,
                    channels: 1,
                    frame_count: SampleFrames(FRAMES as u64),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32le".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"rhythm-promotion-fixture"),
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
            SourceMaterialMetadata::new("Rhythm", "Reading"),
            registry,
            asset,
            pcm,
        )
        .unwrap();
        (ProjectController::new(live).unwrap(), asset)
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

    /// Structurally mirrors analyzer output: two recurring anonymous families,
    /// two exact occurrences, and competing tempo/phase interpretations.
    fn real_shape_rhythm() -> RhythmDeprojection {
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

    #[test]
    fn synthetic_pattern_retains_grid_alternatives_and_exact_microtiming() {
        let (controller, asset) = controller();
        let set = controller
            .plan_rhythm_promotion(
                &real_shape_rhythm(),
                RhythmPromotionIntent {
                    source: SampleSelection::whole_asset(asset),
                    pattern_index: 0,
                    target_bus: None,
                },
            )
            .unwrap();
        assert_eq!(set.alternatives.len(), 2);
        assert_eq!(set.evidence_preferred().unwrap().grid.bpm, 120.0);
        assert!(set.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == RhythmPromotionDiagnosticCode::CompetingGridHypothesesRetained
        }));
        let plan = &set.alternatives[0].plan;
        assert!(plan
            .kit
            .after
            .pads
            .values()
            .all(|pad| pad.name.starts_with("Anonymous family ")));
        assert!(plan.kit.after.pads.values().all(|pad| {
            !pad.name.to_ascii_lowercase().contains("kick")
                && !pad.name.to_ascii_lowercase().contains("snare")
        }));
        let steps = &plan.pattern.as_ref().unwrap().steps;
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].exact_source_onset_frame, Some(10));
        assert_eq!(steps[1].exact_source_onset_frame, Some(135));
        assert_eq!(steps[0].original_micro_offset_frames, Some(0));
        assert_eq!(steps[1].original_micro_offset_frames, Some(0));
        assert!(steps.iter().all(|step| step.evidence.len() == 5));
    }

    #[test]
    fn real_shape_apply_is_audible_and_undo_restores_canonical_creative_bytes() {
        let (mut controller, asset) = controller();
        let before = canonical_creative_content(&controller.snapshot().project);
        let before_aggregate_revision = controller.revisions().aggregate;
        let before_kit_revision = controller
            .snapshot()
            .project
            .state()
            .domains
            .sample_kits
            .revision;
        let before_kit_allocators = {
            let kits = &controller.snapshot().project.state().domains.sample_kits;
            (kits.next_kit_id, kits.next_pad_id, kits.next_zone_id)
        };
        let before_sequencer_revision = controller
            .snapshot()
            .project
            .state()
            .domains
            .sequencer
            .revision();
        let set = controller
            .plan_rhythm_promotion(
                &real_shape_rhythm(),
                RhythmPromotionIntent {
                    source: SampleSelection::whole_asset(asset),
                    pattern_index: 0,
                    target_bus: None,
                },
            )
            .unwrap();
        let outcome = controller
            .execute_constructive_plan(set.evidence_preferred().unwrap().plan.clone())
            .unwrap();
        assert!(outcome.publication.pattern.is_some());
        assert_eq!(controller.snapshot().sample_pcm.len(), 2);
        let applied_kit_allocators = {
            let kits = &controller.snapshot().project.state().domains.sample_kits;
            (kits.next_kit_id, kits.next_pad_id, kits.next_zone_id)
        };
        let applied_sequencer_allocators = controller
            .snapshot()
            .project
            .state()
            .domains
            .sequencer
            .allocator_state();
        let applied_binding_allocators = controller
            .snapshot()
            .project
            .state()
            .bindings
            .allocator_state();
        let applied_arrangement_allocators = {
            let arrangement = &controller.snapshot().project.state().domains.arrangement;
            (arrangement.next_track_id, arrangement.next_clip_id)
        };
        assert!(applied_kit_allocators.0 > before_kit_allocators.0);
        assert!(applied_kit_allocators.1 > before_kit_allocators.1);
        assert!(applied_kit_allocators.2 > before_kit_allocators.2);

        let cancellation = RenderCancellation::new();
        let schedule = compile_daw_engine(
            &controller.snapshot().project,
            &controller.snapshot().pcm,
            RenderWindow::new(0, 400).unwrap(),
            &DawEngineConfig::default(),
            &cancellation,
        )
        .unwrap();
        assert!(
            schedule.engine_diagnostics().is_empty(),
            "{:?}",
            schedule.engine_diagnostics()
        );
        let rendered = schedule.render_for_audition(&cancellation).unwrap();
        assert!(rendered.render_diagnostics.is_empty());
        assert!(rendered
            .audio
            .interleaved()
            .iter()
            .any(|sample| sample.abs() > 0.01));

        controller
            .undo()
            .unwrap()
            .expect("promotion is one undo step");
        let after_undo = canonical_creative_content(&controller.snapshot().project);
        assert_eq!(after_undo, before);
        assert!(controller.snapshot().sample_pcm.is_empty());
        let state = controller.snapshot().project.state();
        assert!(controller.revisions().aggregate > before_aggregate_revision);
        assert!(state.domains.sample_kits.revision > before_kit_revision);
        assert!(state.domains.sequencer.revision() > before_sequencer_revision);
        assert_eq!(
            (
                state.domains.sample_kits.next_kit_id,
                state.domains.sample_kits.next_pad_id,
                state.domains.sample_kits.next_zone_id,
            ),
            applied_kit_allocators
        );
        assert_eq!(
            state.domains.sequencer.allocator_state(),
            applied_sequencer_allocators
        );
        assert_eq!(state.bindings.allocator_state(), applied_binding_allocators);
        assert_eq!(
            (
                state.domains.arrangement.next_track_id,
                state.domains.arrangement.next_clip_id,
            ),
            applied_arrangement_allocators
        );
    }
}
