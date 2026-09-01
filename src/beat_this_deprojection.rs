//! App-side interpretation of verified Beat This worker evidence.
//!
//! The isolated worker publishes measurements and event maps; it does not
//! compile project objects. This adapter joins those immutable bytes to the
//! broker receipt, constructs one anonymous competing rhythm hypothesis, and
//! hands it to Audec's existing explicit rhythm-promotion chooser. It refuses
//! to infer instrument names, accept a hypothesis, or mutate project state.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::fs;

use serde::Deserialize;

use crate::beat_this::{BeatThisRhythmEvidence, RhythmRelationship, LOGIT_FRAME_RATE_HZ, MODEL_ID};
use crate::model_claim::{ModelClaimBundle, ModelClaimId};
use crate::model_store::StoredResult;
use crate::model_task_service::{ModelTaskId, ModelTaskService, VerifiedModelCompletion};
use crate::project_controller::{
    RhythmPromotionChooser, RhythmPromotionChooserError, RhythmPromotionIntent,
};
use crate::project_session::ProjectSession;
use crate::rhythm::{
    AnalysisStatus, BeatPhaseHypothesis, DownbeatHypothesis, EventFamilyHypothesis, HitObservation,
    MedoidSampleReference, PatternHypothesis, PatternOccurrence, RhythmDeprojection, SampleSpan,
    TempoHypothesis, TempoRelation,
};
use crate::rhythm_explanation::{explain_rhythm, ExplainBudget, PatternExplanationSet};
use crate::worker_runtime::broker::CompletionReceipt;

const EVENT_SCHEMA: &str = "audec.beat-this.events.v1";
const MAX_EVENT_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

/// Stable identity for this interpretation recipe applied to exact worker
/// evidence. It remains distinct from the underlying model claim so a future
/// interpretation recipe can coexist rather than rewriting history.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BeatThisRhythmProposalId(String);

impl BeatThisRhythmProposalId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Broker facts copied into the proposal. Inspecting the proposal therefore
/// never depends on mutable scheduler state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeatThisCompletionWitness {
    pub job_id: String,
    pub generation: u64,
    pub cache_key: String,
    pub model_manifest_sha256: String,
    pub result_sha256: String,
    pub completed_at_millis: u64,
    pub receipt_sequence: u64,
    pub artifact_count: usize,
    pub measurement_count: usize,
    pub total_artifact_bytes: u64,
}

/// Exact source citation for one model output. Event frames are local to the
/// claimed source range, matching `RhythmDeprojection`; the absolute source
/// origin remains explicit beside them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeatThisEvidenceSpan {
    pub output_name: String,
    pub artifact_sha256: String,
    pub source_start_frame: u64,
    pub source_frame_count: u64,
    pub event_frames: Vec<u64>,
}

/// Inert reverse-to-forward proposal compiled from one verified completion.
/// Rank is evidence ordering only. Promotion always goes through a chooser
/// whose selected alternative is initially empty.
#[derive(Clone, Debug, PartialEq)]
pub struct BeatThisRhythmProposal {
    pub id: BeatThisRhythmProposalId,
    pub claim_id: ModelClaimId,
    pub witness: BeatThisCompletionWitness,
    pub evidence: BeatThisRhythmEvidence,
    pub evidence_spans: Vec<BeatThisEvidenceSpan>,
    pub rhythm: RhythmDeprojection,
    pub explanations: PatternExplanationSet,
    pub relationship_to_native_rhythm: RhythmRelationship,
    pub caveats: Vec<String>,
}

impl BeatThisRhythmProposal {
    /// Plan anonymous sample slices, pads, and a sequencer pattern against an
    /// exact source selection. The chooser retains all grid alternatives and
    /// requires a later explicit selection before preview or apply.
    pub fn promotion_chooser(
        &self,
        session: &ProjectSession,
        intent: RhythmPromotionIntent,
    ) -> Result<RhythmPromotionChooser, RhythmPromotionChooserError> {
        RhythmPromotionChooser::plan(session, &self.rhythm, intent, Some(&self.explanations))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventArtifactV1 {
    schema: String,
    source_start_frame: u64,
    source_sample_rate_hz: u32,
    times_seconds: Vec<f32>,
}

#[derive(Clone, Debug)]
struct ParsedEvents {
    frames: Vec<u64>,
}

/// Resolve and interpret a live, broker-attested task completion. A cache hit
/// deliberately has no such view because the task service will not fabricate
/// a receipt for a completion from an earlier process lifetime.
pub fn proposal_from_task(
    service: &ModelTaskService,
    task_id: ModelTaskId,
    budget: ExplainBudget,
) -> Result<BeatThisRhythmProposal, BeatThisDeprojectionError> {
    let completion = service
        .verified_completion(task_id)
        .ok_or(BeatThisDeprojectionError::CompletionUnavailable(task_id))?;
    proposal_from_verified_completion(completion, budget)
}

/// Interpret the generic verified-completion join without giving the task
/// service any model-specific or project-domain dependencies.
pub fn proposal_from_verified_completion(
    completion: VerifiedModelCompletion<'_>,
    budget: ExplainBudget,
) -> Result<BeatThisRhythmProposal, BeatThisDeprojectionError> {
    if completion.task.recipe.model_id != MODEL_ID {
        return Err(BeatThisDeprojectionError::WrongModel {
            expected: MODEL_ID.into(),
            actual: completion.task.recipe.model_id.clone(),
        });
    }
    rhythm_proposal_from_completion(
        completion.claim,
        completion.stored,
        completion.receipt,
        budget,
    )
}

/// Decode Beat This's model-specific event schema only after generic worker,
/// store, claim, and broker validation have completed.
pub fn rhythm_proposal_from_completion(
    claim: &ModelClaimBundle,
    stored: &StoredResult,
    receipt: &CompletionReceipt,
    budget: ExplainBudget,
) -> Result<BeatThisRhythmProposal, BeatThisDeprojectionError> {
    claim
        .validate()
        .map_err(|error| BeatThisDeprojectionError::Claim(error.to_string()))?;
    validate_completion_witness(claim, stored, receipt)?;

    let beat_events = read_events(claim, stored, "beat-events")?;
    let downbeat_events = read_events(claim, stored, "downbeat-events")?;
    let evidence_spans = vec![
        evidence_span(claim, stored, "beat-logits", Vec::new())?,
        evidence_span(claim, stored, "downbeat-logits", Vec::new())?,
        evidence_span(claim, stored, "beat-events", beat_events.frames.clone())?,
        evidence_span(
            claim,
            stored,
            "downbeat-events",
            downbeat_events.frames.clone(),
        )?,
    ];

    let (rhythm, mut caveats) = rhythm_from_events(claim, &beat_events, &downbeat_events)?;
    caveats.push(
        "Anonymous family 0 is a timing-linked mixture excerpt; no instrument identity is inferred"
            .into(),
    );
    caveats.push(
        "Beat This evidence competes with native rhythm hypotheses until explicitly selected"
            .into(),
    );
    let explanations = explain_rhythm(&rhythm, &[], budget)
        .map_err(|error| BeatThisDeprojectionError::Rhythm(error.to_string()))?;
    let evidence = BeatThisRhythmEvidence::from_claim(claim)
        .map_err(|error| BeatThisDeprojectionError::Claim(error.to_string()))?;
    let witness = completion_witness(receipt);
    let proposal_hash = crate::model_worker::sha256_bytes(
        &[
            b"audec.beat-this.rhythm-proposal.v1\0".as_slice(),
            claim.id.as_str().as_bytes(),
            b"\0",
            witness.result_sha256.as_bytes(),
            b"\0",
            evidence_spans[2].artifact_sha256.as_bytes(),
            b"\0",
            evidence_spans[3].artifact_sha256.as_bytes(),
        ]
        .concat(),
    )
    .to_string();

    Ok(BeatThisRhythmProposal {
        id: BeatThisRhythmProposalId(proposal_hash),
        claim_id: claim.id.clone(),
        witness,
        evidence,
        evidence_spans,
        rhythm,
        explanations,
        relationship_to_native_rhythm: RhythmRelationship::CompetingEvidence,
        caveats,
    })
}

fn validate_completion_witness(
    claim: &ModelClaimBundle,
    stored: &StoredResult,
    receipt: &CompletionReceipt,
) -> Result<(), BeatThisDeprojectionError> {
    if receipt.identity().cache_key() != claim.cache_key
        || receipt.identity().manifest_sha256() != claim.model_manifest_sha256
        || stored.result.cache_key != claim.cache_key
        || stored.result.job_id != receipt.identity().job_id()
    {
        return Err(BeatThisDeprojectionError::Receipt(
            "broker receipt, stored result, and model claim identities differ".into(),
        ));
    }
    let encoded = serde_json::to_vec(&stored.result)
        .map_err(|error| BeatThisDeprojectionError::Decode(error.to_string()))?;
    if crate::model_worker::sha256_bytes(&encoded).to_string() != receipt.result_sha256() {
        return Err(BeatThisDeprojectionError::Receipt(
            "stored result digest differs from broker completion receipt".into(),
        ));
    }
    let total_bytes = stored
        .result
        .artifacts
        .iter()
        .try_fold(0_u64, |sum, artifact| sum.checked_add(artifact.byte_len))
        .ok_or_else(|| BeatThisDeprojectionError::Receipt("artifact byte count overflow".into()))?;
    let output = receipt.output();
    if output.artifact_count != stored.result.artifacts.len()
        || output.measurement_count != stored.result.measurements.len()
        || output.total_bytes != total_bytes
    {
        return Err(BeatThisDeprojectionError::Receipt(
            "broker completion summary differs from stored output".into(),
        ));
    }
    Ok(())
}

fn read_events(
    claim: &ModelClaimBundle,
    stored: &StoredResult,
    output_name: &str,
) -> Result<ParsedEvents, BeatThisDeprojectionError> {
    let artifact = checked_artifact(claim, stored, output_name)?;
    if artifact.descriptor.byte_len > MAX_EVENT_ARTIFACT_BYTES {
        return Err(BeatThisDeprojectionError::Decode(format!(
            "{output_name} exceeds the bounded event decoder size"
        )));
    }
    let bytes = fs::read(stored.directory.join(&artifact.descriptor.relative_path))
        .map_err(|error| BeatThisDeprojectionError::Io(error.to_string()))?;
    if u64::try_from(bytes.len()).ok() != Some(artifact.descriptor.byte_len)
        || crate::model_worker::sha256_bytes(&bytes).to_string() != artifact.descriptor.sha256
    {
        return Err(BeatThisDeprojectionError::Receipt(format!(
            "{output_name} bytes no longer match the verified descriptor"
        )));
    }
    let parsed: EventArtifactV1 = serde_json::from_slice(&bytes)
        .map_err(|error| BeatThisDeprojectionError::Decode(error.to_string()))?;
    if parsed.schema != EVENT_SCHEMA
        || parsed.source_start_frame != claim.source.start_frame
        || parsed.source_sample_rate_hz != claim.source.sample_rate_hz
    {
        return Err(BeatThisDeprojectionError::Decode(format!(
            "{output_name} source geometry differs from its claim"
        )));
    }
    let duration = claim.source.frame_count as f64 / f64::from(claim.source.sample_rate_hz);
    let mut frames = Vec::with_capacity(parsed.times_seconds.len());
    for seconds in parsed.times_seconds {
        if !seconds.is_finite() || seconds < 0.0 || f64::from(seconds) > duration {
            return Err(BeatThisDeprojectionError::Decode(format!(
                "{output_name} contains an out-of-span or non-finite event"
            )));
        }
        let frame = (f64::from(seconds) * f64::from(claim.source.sample_rate_hz)).round() as u64;
        if frame >= claim.source.frame_count {
            return Err(BeatThisDeprojectionError::Decode(format!(
                "{output_name} contains an event at or beyond the exclusive source end"
            )));
        }
        if frames.last().is_some_and(|previous| frame <= *previous) {
            return Err(BeatThisDeprojectionError::Decode(format!(
                "{output_name} event frames must be strictly increasing"
            )));
        }
        frames.push(frame);
    }
    Ok(ParsedEvents { frames })
}

fn checked_artifact<'a>(
    claim: &'a ModelClaimBundle,
    stored: &StoredResult,
    output_name: &str,
) -> Result<&'a crate::model_claim::ModelClaimArtifact, BeatThisDeprojectionError> {
    let artifact = claim
        .artifact(output_name)
        .ok_or_else(|| BeatThisDeprojectionError::MissingOutput(output_name.into()))?;
    let stored_descriptor = stored
        .result
        .artifacts
        .iter()
        .find(|candidate| candidate.relative_path == artifact.descriptor.relative_path)
        .ok_or_else(|| BeatThisDeprojectionError::MissingOutput(output_name.into()))?;
    if stored_descriptor != &artifact.descriptor {
        return Err(BeatThisDeprojectionError::Receipt(format!(
            "{output_name} claim descriptor differs from the verified stored result"
        )));
    }
    Ok(artifact)
}

fn evidence_span(
    claim: &ModelClaimBundle,
    stored: &StoredResult,
    output_name: &str,
    event_frames: Vec<u64>,
) -> Result<BeatThisEvidenceSpan, BeatThisDeprojectionError> {
    let artifact = checked_artifact(claim, stored, output_name)?;
    let exact_backlink = artifact.descriptor.source_backlinks.iter().any(|backlink| {
        backlink.material_sha256 == claim.source.material_sha256
            && backlink.start_frame == claim.source.start_frame
            && backlink.frame_count == claim.source.frame_count
    });
    if !exact_backlink {
        return Err(BeatThisDeprojectionError::Receipt(format!(
            "{output_name} does not cite the exact claimed source span"
        )));
    }
    Ok(BeatThisEvidenceSpan {
        output_name: output_name.into(),
        artifact_sha256: artifact.descriptor.sha256.clone(),
        source_start_frame: claim.source.start_frame,
        source_frame_count: claim.source.frame_count,
        event_frames,
    })
}

fn completion_witness(receipt: &CompletionReceipt) -> BeatThisCompletionWitness {
    let output = receipt.output();
    BeatThisCompletionWitness {
        job_id: receipt.identity().job_id().into(),
        generation: receipt.identity().generation(),
        cache_key: receipt.identity().cache_key().into(),
        model_manifest_sha256: receipt.identity().manifest_sha256().into(),
        result_sha256: receipt.result_sha256().into(),
        completed_at_millis: receipt.completed_at().as_millis(),
        receipt_sequence: receipt.receipt_sequence(),
        artifact_count: output.artifact_count,
        measurement_count: output.measurement_count,
        total_artifact_bytes: output.total_bytes,
    }
}

fn rhythm_from_events(
    claim: &ModelClaimBundle,
    beats: &ParsedEvents,
    downbeats: &ParsedEvents,
) -> Result<(RhythmDeprojection, Vec<String>), BeatThisDeprojectionError> {
    if beats.frames.len() < 2 {
        return Err(BeatThisDeprojectionError::Rhythm(
            "at least two beat events are required to propose musical time".into(),
        ));
    }
    let sample_frames = usize::try_from(claim.source.frame_count)
        .map_err(|_| BeatThisDeprojectionError::Rhythm("source span is too large".into()))?;
    let beat_frames = platform_frames(&beats.frames, "beat")?;
    let downbeat_frames = platform_frames(&downbeats.frames, "downbeat")?;

    let mut intervals = beat_frames
        .windows(2)
        .map(|pair| pair[1].saturating_sub(pair[0]))
        .filter(|interval| *interval != 0)
        .collect::<Vec<_>>();
    intervals.sort_unstable();
    let period = intervals[intervals.len() / 2].max(1);
    let bpm = (60.0 * f64::from(claim.source.sample_rate_hz) / period as f64) as f32;
    if !bpm.is_finite() || bpm <= 0.0 {
        return Err(BeatThisDeprojectionError::Rhythm(
            "event intervals do not imply a finite positive tempo".into(),
        ));
    }

    let tolerance = (claim.source.sample_rate_hz / 30).max(1) as usize;
    let downbeat_indices = downbeat_frames
        .iter()
        .filter_map(|downbeat| nearest_beat_index(&beat_frames, *downbeat, tolerance))
        .collect::<BTreeSet<_>>();
    let mut meter_candidates = downbeat_indices
        .iter()
        .copied()
        .collect::<Vec<_>>()
        .windows(2)
        .map(|pair| pair[1].saturating_sub(pair[0]))
        .filter(|beats| (2..=16).contains(beats))
        .collect::<Vec<_>>();
    meter_candidates.sort_unstable();
    let (meter, inferred_meter) = meter_candidates
        .get(meter_candidates.len() / 2)
        .copied()
        .map(|meter| (meter, true))
        .unwrap_or((beat_frames.len().min(4).max(2), false));

    let mut caveats = Vec::new();
    if !inferred_meter {
        caveats.push(
            "Downbeat spacing did not establish meter; a four-beat construction alternative is retained as an explicit fallback"
                .into(),
        );
    }
    if downbeat_indices.len() != downbeat_frames.len() {
        caveats.push(
            "Some downbeat events did not align to a beat event within the declared tolerance"
                .into(),
        );
    }

    let half_window = (period / 2)
        .min((claim.source.sample_rate_hz / 5) as usize)
        .max(1);
    let pre_roll = (claim.source.sample_rate_hz / 200) as usize;
    let hits = beat_frames
        .iter()
        .enumerate()
        .map(|(index, frame)| {
            let start = frame.saturating_sub(pre_roll);
            let end = frame
                .saturating_add(half_window)
                .min(sample_frames)
                .max(start + 1);
            HitObservation {
                span: SampleSpan { start, end },
                begins_at_input_boundary: start == 0,
                ends_at_input_boundary: end == sample_frames,
                onset_sample: *frame,
                novelty_peak_sample: *frame,
                peak_sample: *frame,
                onset_seconds: *frame as f64 / f64::from(claim.source.sample_rate_hz),
                duration_seconds: (end - start) as f32 / claim.source.sample_rate_hz as f32,
                novelty_strength: if downbeat_indices.contains(&index) {
                    1.0
                } else {
                    0.75
                },
                threshold_excess: 0.0,
                family: Some(0),
                family_similarity: 1.0,
                ..HitObservation::default()
            }
        })
        .collect::<Vec<_>>();

    let anchors = if downbeat_indices.is_empty() {
        (0..beat_frames.len()).step_by(meter).collect::<Vec<_>>()
    } else {
        downbeat_indices.iter().copied().collect::<Vec<_>>()
    };
    let mut occurrences = anchors
        .into_iter()
        .filter(|anchor| anchor.saturating_add(meter) <= hits.len())
        .map(|event_index| PatternOccurrence {
            event_index,
            start_sample: hits[event_index].onset_sample,
            beat_position: 0.0,
        })
        .collect::<Vec<_>>();
    if occurrences.is_empty() && hits.len() >= meter {
        caveats.push(
            "No reported downbeat began a complete retained measure; the first complete beat group is retained as a separate construction fallback"
                .into(),
        );
        occurrences.push(PatternOccurrence {
            event_index: 0,
            start_sample: hits[0].onset_sample,
            beat_position: 0.0,
        });
    }
    let medoid_index = occurrences
        .first()
        .map(|occurrence| occurrence.event_index)
        .unwrap_or(0);
    let medoid_excerpt = hits[medoid_index].span;

    Ok((
        RhythmDeprojection {
            status: AnalysisStatus::Complete,
            sample_rate: claim.source.sample_rate_hz,
            sample_frames,
            analysis_hop: usize::try_from(claim.source.sample_rate_hz / LOGIT_FRAME_RATE_HZ)
                .unwrap_or(1)
                .max(1),
            novelty: Vec::new(),
            band_novelty: Vec::new(),
            adaptive_threshold: Vec::new(),
            hits,
            tempogram: Vec::new(),
            tempo_hypotheses: vec![TempoHypothesis {
                rank: 0,
                bpm,
                period_frames: period as f32,
                periodicity: 1.0,
                evidence: 1.0,
                relation: TempoRelation::Independent,
            }],
            beat_phase_hypotheses: vec![BeatPhaseHypothesis {
                tempo_rank: 0,
                bpm,
                phase_seconds: beat_frames[0] as f64 / f64::from(claim.source.sample_rate_hz),
                score: 1.0,
                beat_samples: beat_frames,
            }],
            downbeat_hypotheses: vec![DownbeatHypothesis {
                beat_phase_index: 0,
                meter_beats: meter,
                downbeat_offset: 0,
                score: if inferred_meter { 1.0 } else { 0.0 },
                downbeat_samples: downbeat_frames,
            }],
            event_families: vec![EventFamilyHypothesis {
                id: 0,
                event_indices: (0..beats.frames.len()).collect(),
                medoid: MedoidSampleReference {
                    event_index: medoid_index,
                    excerpt: medoid_excerpt,
                },
                mean_medoid_similarity: 1.0,
                minimum_medoid_similarity: 1.0,
                evidence: 1.0,
            }],
            patterns: vec![PatternHypothesis {
                family_sequence: vec![0; meter],
                step_offsets: (0..meter)
                    .map(|beat| i32::try_from(beat.saturating_mul(4)).unwrap_or(i32::MAX))
                    .collect(),
                occurrences,
                evidence: 1.0,
            }],
            silent: false,
        },
        caveats,
    ))
}

fn platform_frames(frames: &[u64], kind: &str) -> Result<Vec<usize>, BeatThisDeprojectionError> {
    frames
        .iter()
        .copied()
        .map(|frame| {
            usize::try_from(frame).map_err(|_| {
                BeatThisDeprojectionError::Rhythm(format!(
                    "{kind} event frame does not fit this platform"
                ))
            })
        })
        .collect()
}

fn nearest_beat_index(beats: &[usize], target: usize, tolerance: usize) -> Option<usize> {
    beats
        .iter()
        .enumerate()
        .map(|(index, beat)| (index, beat.abs_diff(target)))
        .filter(|(_, distance)| *distance <= tolerance)
        .min_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)))
        .map(|(index, _)| index)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BeatThisDeprojectionError {
    CompletionUnavailable(ModelTaskId),
    WrongModel { expected: String, actual: String },
    MissingOutput(String),
    Claim(String),
    Io(String),
    Decode(String),
    Receipt(String),
    Rhythm(String),
}

impl fmt::Display for BeatThisDeprojectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CompletionUnavailable(id) => write!(
                f,
                "model task {} has no live broker-attested completion",
                id.get()
            ),
            Self::WrongModel { expected, actual } => {
                write!(f, "expected model {expected}, received {actual}")
            }
            Self::MissingOutput(output) => write!(f, "missing required output {output}"),
            Self::Claim(detail)
            | Self::Io(detail)
            | Self::Decode(detail)
            | Self::Receipt(detail)
            | Self::Rhythm(detail) => f.write_str(detail),
        }
    }
}

impl Error for BeatThisDeprojectionError {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::beat_this::{INPUT_SAMPLE_RATE_HZ, SOURCE_TREE_SHA256, WORKER_NAME};
    use crate::model_claim::{ClaimSource, ModelClaimArtifact, WorkerRuntimeProvenance};
    use crate::model_wire::{
        AdditivityDeclaration, ArtifactDescriptor, ArtifactKind, SourceBacklink, WorkerResult,
    };
    use crate::worker_runtime::broker::{
        BrokerCapacity, BrokerTick, CompletionAttempt, ForegroundPressure, JobIdentity,
        JobPriority, JobTicket, OutputSummary, ResourceDemand, RuntimePolicy, WorkerBroker,
    };

    #[test]
    fn verified_events_become_anonymous_promotable_rhythm_evidence() {
        let root = std::env::temp_dir().join(format!(
            "audec-beat-this-deprojection-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let cache_key = "a1".repeat(32);
        let manifest = "b2".repeat(32);
        let source = ClaimSource {
            material_sha256: "c3".repeat(32),
            start_frame: 320,
            frame_count: u64::from(INPUT_SAMPLE_RATE_HZ) * 4,
            sample_rate_hz: INPUT_SAMPLE_RATE_HZ,
            channels: 1,
        };
        let payloads = [
            ("beat-logits", br#"{"schema":"opaque-logits"}"#.to_vec()),
            (
                "downbeat-logits",
                br#"{"schema":"opaque-logits"}"#.to_vec(),
            ),
            (
                "beat-events",
                format!(
                    "{{\"schema\":\"{EVENT_SCHEMA}\",\"source_start_frame\":320,\"source_sample_rate_hz\":{INPUT_SAMPLE_RATE_HZ},\"times_seconds\":[0.0,0.5,1.0,1.5,2.0,2.5,3.0,3.5]}}"
                )
                .into_bytes(),
            ),
            (
                "downbeat-events",
                format!(
                    "{{\"schema\":\"{EVENT_SCHEMA}\",\"source_start_frame\":320,\"source_sample_rate_hz\":{INPUT_SAMPLE_RATE_HZ},\"times_seconds\":[0.0,2.0]}}"
                )
                .into_bytes(),
            ),
        ];
        let descriptors = payloads
            .iter()
            .map(|(name, bytes)| {
                let relative_path = format!("{name}.json");
                fs::write(root.join(&relative_path), bytes).unwrap();
                ArtifactDescriptor {
                    relative_path,
                    sha256: crate::model_worker::sha256_bytes(bytes).to_string(),
                    byte_len: bytes.len() as u64,
                    kind: if name.ends_with("events") {
                        ArtifactKind::EventMap
                    } else {
                        ArtifactKind::Measurement
                    },
                    media_type: "application/vnd.audec.beat-this+json".into(),
                    schema_revision: 1,
                    time_base_hz: Some(if name.ends_with("events") {
                        INPUT_SAMPLE_RATE_HZ
                    } else {
                        LOGIT_FRAME_RATE_HZ
                    }),
                    additivity: AdditivityDeclaration::NonAudio,
                    source_backlinks: vec![SourceBacklink {
                        material_sha256: source.material_sha256.clone(),
                        start_frame: source.start_frame,
                        frame_count: source.frame_count,
                    }],
                }
            })
            .collect::<Vec<_>>();
        let result = WorkerResult {
            job_id: "beat-this-test-job".into(),
            cache_key: cache_key.clone(),
            artifacts: descriptors.clone(),
            measurements: Vec::new(),
        };
        let stored = StoredResult {
            directory: root.clone(),
            result: result.clone(),
        };
        let claim = ModelClaimBundle::new(
            manifest.clone(),
            cache_key.clone(),
            source,
            WorkerRuntimeProvenance {
                worker_name: WORKER_NAME.into(),
                runtime: "beat-this-test-runtime".into(),
                adapter_sha256: Some(SOURCE_TREE_SHA256.into()),
            },
            AdditivityDeclaration::NonAudio,
            descriptors
                .into_iter()
                .zip([
                    "beat-logits",
                    "downbeat-logits",
                    "beat-events",
                    "downbeat-events",
                ])
                .map(|(descriptor, output_name)| ModelClaimArtifact {
                    descriptor,
                    output_name: output_name.into(),
                    labels: Vec::new(),
                })
                .collect(),
        )
        .unwrap();
        let total_bytes = result
            .artifacts
            .iter()
            .map(|artifact| artifact.byte_len)
            .sum();
        let capacity = BrokerCapacity {
            cpu_slots: 2,
            memory_bytes: 1024,
            scratch_bytes: 1024,
            worker_slots: 1,
            accelerators: BTreeMap::new(),
            realtime_cpu_reserve: 0,
            realtime_memory_reserve: 0,
            render_cpu_reserve: 0,
            render_memory_reserve: 0,
        };
        let mut broker =
            WorkerBroker::new(capacity, RuntimePolicy::default(), Duration::from_secs(1)).unwrap();
        let identity = JobIdentity::new(result.job_id.clone(), 1, cache_key, manifest).unwrap();
        broker
            .submit(
                JobTicket {
                    identity: identity.clone(),
                    priority: JobPriority::UserInitiated,
                    demand: ResourceDemand {
                        cpu_slots: 1,
                        memory_bytes: 1,
                        scratch_bytes: 1,
                        expected_output_bytes: total_bytes,
                        accelerator: None,
                    },
                },
                BrokerTick::ZERO,
            )
            .unwrap();
        broker.schedule(BrokerTick::ZERO, ForegroundPressure::default());
        broker.observe_started(&identity, BrokerTick::ZERO).unwrap();
        let receipt = broker
            .accept_completion(
                CompletionAttempt {
                    identity,
                    result_sha256: crate::model_worker::sha256_bytes(
                        &serde_json::to_vec(&result).unwrap(),
                    )
                    .to_string(),
                    output: OutputSummary {
                        artifact_count: result.artifacts.len(),
                        measurement_count: 0,
                        total_bytes,
                    },
                },
                BrokerTick::from_millis(5),
            )
            .unwrap();

        let proposal =
            rhythm_proposal_from_completion(&claim, &stored, &receipt, ExplainBudget::default())
                .unwrap();
        assert_eq!(proposal.rhythm.tempo_hypotheses[0].bpm, 120.0);
        assert_eq!(proposal.rhythm.patterns[0].step_offsets, vec![0, 4, 8, 12]);
        assert!(proposal.rhythm.hits.iter().all(|hit| hit.family == Some(0)));
        assert_eq!(proposal.rhythm.event_families.len(), 1);
        assert!(!proposal.explanations.alternatives.is_empty());
        assert_eq!(
            proposal.relationship_to_native_rhythm,
            RhythmRelationship::CompetingEvidence
        );
        assert!(proposal
            .caveats
            .iter()
            .any(|caveat| caveat.contains("no instrument identity")));
        assert_eq!(proposal.witness.receipt_sequence, 0);

        fs::remove_dir_all(root).unwrap();
    }
}
