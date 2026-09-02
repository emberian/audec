//! Evidence-preserving reconstruction hypotheses for audec's reverse DAW.
//!
//! Analysis answers “what is observable?” while a DAW needs editable things:
//! sample slices, triggers, notes, patterns, controls, and audio that remains
//! unexplained.  This module is the deliberately uncertain bridge between
//! those worlds.  It produces several deterministic proposals and preserves
//! the evidence behind every edit.  In particular, an upstream recurrence
//! family remains an anonymous family; it is never silently renamed “kick”,
//! “snare”, “voice”, or any other physical-source identity.
//!
//! A reconstruction proposal is not a separated stem set.  Tracks may overlap
//! and the residual layer is part of the model, not a failure case.  Consumers
//! should keep alternatives inspectable and require an explicit user action
//! before treating one proposal as accepted.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::decomposition::ComponentDecomposition;
use crate::loom::SequenceSketch;
use crate::pitch::{GlideDirection, ModulationEvidence, PitchAnalysis, PitchTrack};
use crate::rhythm::{RhythmDeprojection, SampleSpan};

pub const RECONSTRUCTION_SCHEMA_VERSION: u32 = 1;
const PPQ: i64 = 960;

macro_rules! typed_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            pub const fn from_raw(raw: u64) -> Self {
                Self(raw)
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

typed_id!(ReconstructionProposalId);
typed_id!(ReconstructionTrackId);
typed_id!(ReconstructionEvidenceId);
typed_id!(SampleSliceId);
typed_id!(TriggerId);
typed_id!(PitchedEventId);
typed_id!(AutomationProposalId);
typed_id!(ModulationProposalId);
typed_id!(EffectProposalId);

/// Half-open source range in PCM frames.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceFrameRange {
    pub start: u64,
    pub end: u64,
}

impl SourceFrameRange {
    pub fn new(start: u64, end: u64) -> Option<Self> {
        (start < end).then_some(Self { start, end })
    }

    pub const fn len(self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }

    pub const fn contains(self, frame: u64) -> bool {
        self.start <= frame && frame < self.end
    }
}

/// Optional timing metadata for an NMF-like component matrix.
#[derive(Clone, Copy, Debug)]
pub struct TimedComponents<'a> {
    pub decomposition: &'a ComponentDecomposition,
    pub first_frame: u64,
    pub hop_frames: u64,
    pub analyzer_name: &'a str,
    pub analyzer_version: &'a str,
}

#[derive(Clone, Debug)]
pub struct ReconstructionInputs<'a> {
    pub sample_rate: u32,
    pub source_frame_count: u64,
    pub source_revision: Option<&'a str>,
    pub rhythm: Option<&'a RhythmDeprojection>,
    pub pitch: Option<&'a PitchAnalysis>,
    pub loom: Option<&'a SequenceSketch>,
    pub components: Option<TimedComponents<'a>>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReconstructionConfig {
    /// Maximum ranked alternatives, including the source-faithful proposal.
    pub maximum_proposals: usize,
    /// Maximum beat-phase choices expanded into grid-aware alternatives.
    pub maximum_beat_phases: usize,
    /// Pitch discontinuity that starts a new note in the discrete model.
    pub note_split_semitones: f32,
    /// Missing pitch frames tolerated inside a continuous event.
    pub maximum_pitch_gap_frames: usize,
    /// Quantization grid expressed as equal divisions of a quarter note.
    pub steps_per_quarter: u16,
    /// Minimum support retained as an alternate pitch explanation.
    pub minimum_pitch_alternative_support: f32,
}

impl Default for ReconstructionConfig {
    fn default() -> Self {
        Self {
            maximum_proposals: 9,
            maximum_beat_phases: 4,
            note_split_semitones: 2.5,
            maximum_pitch_gap_frames: 2,
            steps_per_quarter: 4,
            minimum_pitch_alternative_support: 0.12,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalysisProvenance {
    pub analyzer: String,
    pub version: String,
    pub source_revision: Option<String>,
    pub locator: String,
}

/// Evidence locators are stable references into upstream analysis results.
#[derive(Clone, Debug, PartialEq)]
pub enum ReconstructionEvidenceKind {
    RhythmHit {
        event_index: usize,
    },
    AnonymousEventFamily {
        family_id: usize,
    },
    Tempo {
        tempo_rank: usize,
    },
    BeatPhase {
        beat_phase_index: usize,
    },
    PitchTrack {
        track_index: usize,
    },
    PitchFrames {
        first_frame: usize,
        last_frame: usize,
    },
    PitchModulation {
        track_index: usize,
        index: usize,
    },
    LoomTemplate {
        anonymous_cluster_id: usize,
    },
    LatentComponent {
        component_index: usize,
    },
    /// Records the need to retain source audio not explained by constructive
    /// tracks. It does not claim a physical residual source.
    ResidualCoverage {
        range: SourceFrameRange,
    },
    Derived {
        method: String,
        premises: Vec<ReconstructionEvidenceId>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReconstructionEvidence {
    pub id: ReconstructionEvidenceId,
    pub kind: ReconstructionEvidenceKind,
    /// Ranking strength in `[0, 1]`, not a calibrated posterior probability.
    pub strength: f32,
    pub provenance: AnalysisProvenance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PitchedRenderingModel {
    /// Split a trajectory on sufficiently large pitch jumps.
    DiscreteNotes,
    /// Preserve pitch motion as long continuous voiced gestures.
    ContinuousPitch,
}

/// Track identity remains perceptual/analytic rather than instrumental.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconstructionTrackKind {
    AnonymousHitFamily {
        upstream_family_id: usize,
    },
    UnclusteredHits,
    PitchedStream {
        upstream_track_index: usize,
        rendering: PitchedRenderingModel,
    },
    LatentComponent {
        upstream_component_index: usize,
    },
    Residual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliceRole {
    AnonymousFamilyRepresentative,
    UnclusteredEvent,
}

/// A reusable, exact citation into source PCM. No separation is implied.
#[derive(Clone, Debug, PartialEq)]
pub struct SampleSliceProposal {
    pub id: SampleSliceId,
    pub source: SourceFrameRange,
    pub onset_offset_frames: u64,
    pub role: SliceRole,
    pub anonymous_family_id: Option<usize>,
    pub representative_event_index: usize,
    pub confidence: f32,
    pub evidence: Vec<ReconstructionEvidenceId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SampleTriggerProposal {
    pub id: TriggerId,
    pub slice: SampleSliceId,
    pub source_onset_frame: u64,
    pub gain: f32,
    /// Original timing is always retained, even in a quantized proposal.
    pub musical_tick: Option<i64>,
    pub micro_offset_frames: i64,
    pub confidence: f32,
    pub evidence: Vec<ReconstructionEvidenceId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PitchInterpretation {
    TrackedFundamentalCandidate,
    PeriodicityCandidate,
    HarmonicSpectrumCandidate,
    CombinedMeasurement,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PitchChoice {
    pub hz: f32,
    pub midi_key: u8,
    pub cents: f32,
    pub interpretation: PitchInterpretation,
    pub support: f32,
    pub evidence: Vec<ReconstructionEvidenceId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PitchChoiceSelection {
    Unresolved,
    EvidencePreferred(usize),
    UserSelected(usize),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PitchCurvePoint {
    pub offset_frames: u64,
    pub cents_from_preferred: f32,
    pub confidence: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PitchedEventProposal {
    pub id: PitchedEventId,
    pub source: SourceFrameRange,
    pub musical_start_tick: Option<i64>,
    pub musical_duration_ticks: Option<u64>,
    pub micro_offset_frames: i64,
    pub velocity: f32,
    pub alternatives: Vec<PitchChoice>,
    pub selection: PitchChoiceSelection,
    pub pitch_curve: Vec<PitchCurvePoint>,
    pub confidence: f32,
    pub evidence: Vec<ReconstructionEvidenceId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StepPlacement {
    pub trigger: TriggerId,
    pub step: i64,
    pub micro_offset_frames: i64,
    pub velocity: f32,
    pub confidence: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StepPatternProposal {
    pub steps_per_quarter: u16,
    pub placements: Vec<StepPlacement>,
    pub tempo: TempoChoice,
    pub evidence: Vec<ReconstructionEvidenceId>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AutomationTarget {
    Gain,
    PitchCents,
    Brightness,
    SpectralActivity,
    StereoWidth,
    TailLevel,
    Custom(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutomationInterpolation {
    Hold,
    Linear,
    Smooth,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutomationProposalPoint {
    pub source_frame: u64,
    pub value: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AutomationProposal {
    pub id: AutomationProposalId,
    pub target: AutomationTarget,
    pub interpolation: AutomationInterpolation,
    pub points: Vec<AutomationProposalPoint>,
    pub confidence: f32,
    pub evidence: Vec<ReconstructionEvidenceId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModulationPhenomenon {
    PitchGlide,
    PeriodicPitchMotion,
}

/// These are implementation candidates, not claims about the source patch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModulationImplementation {
    DirectPitchAutomation,
    PitchLfo,
    FrequencyModulation,
    ModulatedDelay,
    Unspecified,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModulationImplementationCandidate {
    pub implementation: ModulationImplementation,
    pub support: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModulationProposal {
    pub id: ModulationProposalId,
    pub phenomenon: ModulationPhenomenon,
    pub rate_hz: Option<f32>,
    pub extent_semitones: f32,
    pub implementations: Vec<ModulationImplementationCandidate>,
    /// `None` is intentional: evidence of motion does not identify a synth or
    /// effect mechanism.
    pub selected_implementation: Option<usize>,
    pub confidence: f32,
    pub evidence: Vec<ReconstructionEvidenceId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectPhenomenon {
    DecayingTail,
    DiffuseOrNoisyTail,
    StereoSpread,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectImplementation {
    Envelope,
    Reverberation,
    Convolution,
    DelayNetwork,
    StereoProcessor,
    SourceLayer,
    Unspecified,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EffectImplementationCandidate {
    pub implementation: EffectImplementation,
    pub support: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EffectProposal {
    pub id: EffectProposalId,
    pub phenomenon: EffectPhenomenon,
    pub implementations: Vec<EffectImplementationCandidate>,
    pub selected_implementation: Option<usize>,
    pub confidence: f32,
    pub evidence: Vec<ReconstructionEvidenceId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResidualRenderMode {
    /// Render `source - constructive reconstruction` when a compatible
    /// subtractive renderer is available.
    SubtractConstructiveRender,
    /// Preserve the original range as an explicit safety layer. This may
    /// double content until the user mutes overlapping proposed tracks.
    OriginalSafetyLayer,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResidualAudioProposal {
    pub source: SourceFrameRange,
    pub preferred_mode: ResidualRenderMode,
    pub fallback_mode: ResidualRenderMode,
    /// Expected unexplained fraction, not measured stem energy.
    pub estimated_fraction: f32,
    pub evidence: Vec<ReconstructionEvidenceId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LatentComponentProposal {
    pub upstream_component_index: usize,
    pub spectral_template: Vec<f32>,
    pub first_frame: u64,
    pub hop_frames: u64,
    pub confidence: f32,
    pub evidence: Vec<ReconstructionEvidenceId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EditableTrackProposal {
    pub id: ReconstructionTrackId,
    pub label: String,
    pub kind: ReconstructionTrackKind,
    pub sample_slices: Vec<SampleSliceProposal>,
    pub triggers: Vec<SampleTriggerProposal>,
    pub pitched_events: Vec<PitchedEventProposal>,
    pub step_pattern: Option<StepPatternProposal>,
    pub automations: Vec<AutomationProposal>,
    pub modulations: Vec<ModulationProposal>,
    pub effects: Vec<EffectProposal>,
    pub latent_component: Option<LatentComponentProposal>,
    pub residual: Option<ResidualAudioProposal>,
    pub confidence: f32,
    pub evidence: Vec<ReconstructionEvidenceId>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TempoChoice {
    pub bpm: f32,
    pub phase_source_frame: i64,
    pub upstream_tempo_rank: usize,
    pub upstream_beat_phase_index: usize,
    pub support: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProposalScore {
    pub observation_support: f32,
    pub pattern_support: f32,
    pub editability: f32,
    pub estimated_coverage: f32,
    pub assumption_penalty: f32,
    pub total: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReconstructionProposal {
    pub id: ReconstructionProposalId,
    pub rank: usize,
    pub label: String,
    pub tempo: Option<TempoChoice>,
    pub pitch_model: PitchedRenderingModel,
    pub tracks: Vec<EditableTrackProposal>,
    pub score: ProposalScore,
    pub evidence: Vec<ReconstructionEvidenceId>,
    pub caveats: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconstructionSelection {
    Unresolved,
    EvidencePreferred(ReconstructionProposalId),
    UserSelected(ReconstructionProposalId),
    UserRejectedAll,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReconstructionSet {
    pub schema_version: u32,
    pub sample_rate: u32,
    pub source_frame_count: u64,
    pub evidence: Vec<ReconstructionEvidence>,
    pub proposals: Vec<ReconstructionProposal>,
    pub selection: ReconstructionSelection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconstructionValidationIssue {
    pub path: String,
    pub message: String,
}

impl ReconstructionValidationIssue {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl ReconstructionSet {
    /// Validate an imported or user-edited proposal set before bridging it into
    /// a project. All issues are returned in one pass.
    pub fn validate(&self) -> Vec<ReconstructionValidationIssue> {
        let mut issues = Vec::new();
        if self.schema_version == 0 || self.schema_version > RECONSTRUCTION_SCHEMA_VERSION {
            issues.push(ReconstructionValidationIssue::new(
                "schema_version",
                "is zero or newer than this implementation",
            ));
        }
        if self.sample_rate == 0 || self.source_frame_count == 0 {
            issues.push(ReconstructionValidationIssue::new(
                "source",
                "sample rate and frame count must be non-zero",
            ));
        }
        let evidence_ids: BTreeSet<_> = self.evidence.iter().map(|item| item.id).collect();
        if evidence_ids.len() != self.evidence.len()
            || evidence_ids.contains(&ReconstructionEvidenceId::from_raw(0))
        {
            issues.push(ReconstructionValidationIssue::new(
                "evidence",
                "ids must be unique and non-zero",
            ));
        }
        for (index, evidence) in self.evidence.iter().enumerate() {
            let path = format!("evidence[{index}]");
            if !unit(evidence.strength) {
                issues.push(ReconstructionValidationIssue::new(
                    &path,
                    "strength must be finite and in [0, 1]",
                ));
            }
            if let ReconstructionEvidenceKind::Derived { premises, .. } = &evidence.kind {
                validate_evidence_refs(&path, premises, &evidence_ids, &mut issues);
            }
        }

        let proposal_ids: BTreeSet<_> = self.proposals.iter().map(|item| item.id).collect();
        if proposal_ids.len() != self.proposals.len()
            || proposal_ids.contains(&ReconstructionProposalId::from_raw(0))
        {
            issues.push(ReconstructionValidationIssue::new(
                "proposals",
                "ids must be unique and non-zero",
            ));
        }
        for (index, proposal) in self.proposals.iter().enumerate() {
            let path = format!("proposals[{index}]");
            if proposal.rank != index || !unit(proposal.score.total) {
                issues.push(ReconstructionValidationIssue::new(
                    &path,
                    "rank or total score is invalid",
                ));
            }
            validate_evidence_refs(&path, &proposal.evidence, &evidence_ids, &mut issues);
            let track_ids: BTreeSet<_> = proposal.tracks.iter().map(|item| item.id).collect();
            if track_ids.len() != proposal.tracks.len()
                || track_ids.contains(&ReconstructionTrackId::from_raw(0))
            {
                issues.push(ReconstructionValidationIssue::new(
                    &path,
                    "track ids must be unique and non-zero",
                ));
            }
            let residuals = proposal
                .tracks
                .iter()
                .filter(|track| matches!(track.kind, ReconstructionTrackKind::Residual))
                .count();
            if residuals != 1 {
                issues.push(ReconstructionValidationIssue::new(
                    &path,
                    "each proposal must contain exactly one residual track",
                ));
            }
            for (track_index, track) in proposal.tracks.iter().enumerate() {
                validate_track(
                    track,
                    &format!("{path}.tracks[{track_index}]"),
                    self.source_frame_count,
                    &evidence_ids,
                    &mut issues,
                );
            }
        }
        let selected = match self.selection {
            ReconstructionSelection::EvidencePreferred(id)
            | ReconstructionSelection::UserSelected(id) => Some(id),
            ReconstructionSelection::Unresolved | ReconstructionSelection::UserRejectedAll => None,
        };
        if selected.is_some_and(|id| !proposal_ids.contains(&id)) {
            issues.push(ReconstructionValidationIssue::new(
                "selection",
                "references a missing proposal",
            ));
        }
        issues
    }
}

fn validate_track(
    track: &EditableTrackProposal,
    path: &str,
    source_frames: u64,
    evidence_ids: &BTreeSet<ReconstructionEvidenceId>,
    issues: &mut Vec<ReconstructionValidationIssue>,
) {
    if !unit(track.confidence) {
        issues.push(ReconstructionValidationIssue::new(
            path,
            "track confidence must be finite and in [0, 1]",
        ));
    }
    validate_evidence_refs(path, &track.evidence, evidence_ids, issues);
    let slice_ids: BTreeSet<_> = track.sample_slices.iter().map(|item| item.id).collect();
    if slice_ids.len() != track.sample_slices.len()
        || slice_ids.contains(&SampleSliceId::from_raw(0))
    {
        issues.push(ReconstructionValidationIssue::new(
            path,
            "slice ids must be unique and non-zero",
        ));
    }
    for slice in &track.sample_slices {
        if slice.source.start >= slice.source.end
            || slice.source.end > source_frames
            || slice.onset_offset_frames >= slice.source.len()
            || !unit(slice.confidence)
        {
            issues.push(ReconstructionValidationIssue::new(
                path,
                "sample slice range, onset, or confidence is invalid",
            ));
        }
        validate_evidence_refs(path, &slice.evidence, evidence_ids, issues);
    }
    let mut trigger_ids = BTreeSet::new();
    for trigger in &track.triggers {
        if trigger.id.get() == 0
            || !trigger_ids.insert(trigger.id)
            || !slice_ids.contains(&trigger.slice)
            || trigger.source_onset_frame >= source_frames
            || !unit(trigger.gain)
            || !unit(trigger.confidence)
        {
            issues.push(ReconstructionValidationIssue::new(
                path,
                "trigger identity, slice, time, gain, or confidence is invalid",
            ));
        }
        validate_evidence_refs(path, &trigger.evidence, evidence_ids, issues);
    }
    let mut event_ids = BTreeSet::new();
    for event in &track.pitched_events {
        if event.id.get() == 0
            || !event_ids.insert(event.id)
            || event.source.start >= event.source.end
            || event.source.end > source_frames
            || event.alternatives.is_empty()
            || !unit(event.velocity)
            || !unit(event.confidence)
        {
            issues.push(ReconstructionValidationIssue::new(
                path,
                "pitched event identity, range, alternatives, or confidence is invalid",
            ));
        }
        for choice in &event.alternatives {
            if !choice.hz.is_finite()
                || choice.hz <= 0.0
                || !choice.cents.is_finite()
                || !unit(choice.support)
            {
                issues.push(ReconstructionValidationIssue::new(
                    path,
                    "pitch alternatives must be finite, positive, and supported",
                ));
            }
            validate_evidence_refs(path, &choice.evidence, evidence_ids, issues);
        }
        let selected = match event.selection {
            PitchChoiceSelection::EvidencePreferred(index)
            | PitchChoiceSelection::UserSelected(index) => Some(index),
            PitchChoiceSelection::Unresolved => None,
        };
        if selected.is_some_and(|index| index >= event.alternatives.len()) {
            issues.push(ReconstructionValidationIssue::new(
                path,
                "pitch selection is outside the alternatives",
            ));
        }
        validate_evidence_refs(path, &event.evidence, evidence_ids, issues);
    }
    for automation in &track.automations {
        if automation.id.get() == 0
            || !unit(automation.confidence)
            || !automation
                .points
                .windows(2)
                .all(|pair| pair[0].source_frame <= pair[1].source_frame)
            || automation
                .points
                .iter()
                .any(|point| point.source_frame >= source_frames || !point.value.is_finite())
        {
            issues.push(ReconstructionValidationIssue::new(
                path,
                "automation identity, order, extent, value, or confidence is invalid",
            ));
        }
        validate_evidence_refs(path, &automation.evidence, evidence_ids, issues);
    }
    for modulation in &track.modulations {
        if modulation.id.get() == 0
            || modulation.implementations.is_empty()
            || !unit(modulation.confidence)
            || modulation
                .selected_implementation
                .is_some_and(|index| index >= modulation.implementations.len())
        {
            issues.push(ReconstructionValidationIssue::new(
                path,
                "modulation identity, alternatives, selection, or confidence is invalid",
            ));
        }
        validate_evidence_refs(path, &modulation.evidence, evidence_ids, issues);
    }
    for effect in &track.effects {
        if effect.id.get() == 0
            || effect.implementations.is_empty()
            || !unit(effect.confidence)
            || effect
                .selected_implementation
                .is_some_and(|index| index >= effect.implementations.len())
        {
            issues.push(ReconstructionValidationIssue::new(
                path,
                "effect identity, alternatives, selection, or confidence is invalid",
            ));
        }
        validate_evidence_refs(path, &effect.evidence, evidence_ids, issues);
    }
    if let Some(residual) = &track.residual {
        if !matches!(track.kind, ReconstructionTrackKind::Residual)
            || residual.source.start != 0
            || residual.source.end != source_frames
            || !unit(residual.estimated_fraction)
        {
            issues.push(ReconstructionValidationIssue::new(
                path,
                "residual must cover the complete source on a residual track",
            ));
        }
        validate_evidence_refs(path, &residual.evidence, evidence_ids, issues);
    } else if matches!(track.kind, ReconstructionTrackKind::Residual) {
        issues.push(ReconstructionValidationIssue::new(
            path,
            "residual track is missing its residual description",
        ));
    }
}

fn validate_evidence_refs(
    path: &str,
    references: &[ReconstructionEvidenceId],
    evidence_ids: &BTreeSet<ReconstructionEvidenceId>,
    issues: &mut Vec<ReconstructionValidationIssue>,
) {
    if references.iter().any(|id| !evidence_ids.contains(id)) {
        issues.push(ReconstructionValidationIssue::new(
            path,
            "references missing evidence",
        ));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconstructionError {
    InvalidSampleRate,
    EmptySource,
    InvalidConfiguration(&'static str),
    AnalysisSampleRateMismatch {
        analysis: &'static str,
        expected: u32,
        actual: u32,
    },
    InvalidComponentTiming,
    ObservationOutOfBounds {
        analysis: &'static str,
        index: usize,
    },
}

impl fmt::Display for ReconstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSampleRate => formatter.write_str("sample rate must be non-zero"),
            Self::EmptySource => formatter.write_str("source must contain at least one frame"),
            Self::InvalidConfiguration(message) => {
                write!(formatter, "invalid configuration: {message}")
            }
            Self::AnalysisSampleRateMismatch {
                analysis,
                expected,
                actual,
            } => write!(
                formatter,
                "{analysis} sample rate {actual} differs from source sample rate {expected}"
            ),
            Self::InvalidComponentTiming => {
                formatter.write_str("component timing hop must be non-zero and in source bounds")
            }
            Self::ObservationOutOfBounds { analysis, index } => {
                write!(
                    formatter,
                    "{analysis} observation {index} is outside the source"
                )
            }
        }
    }
}

impl Error for ReconstructionError {}

/// Builds and deterministically ranks competing editable explanations.
pub fn reconstruct(
    input: &ReconstructionInputs<'_>,
    config: ReconstructionConfig,
) -> Result<ReconstructionSet, ReconstructionError> {
    validate_inputs(input, config)?;
    let mut builder = Builder::new(input, config);
    builder.collect_evidence();

    let mut proposals = Vec::new();
    proposals.push(builder.build_proposal(None, PitchedRenderingModel::DiscreteNotes));
    if input.pitch.is_some_and(|pitch| !pitch.tracks.is_empty()) {
        proposals.push(builder.build_proposal(None, PitchedRenderingModel::ContinuousPitch));
    }

    for tempo in builder.tempo_choices() {
        proposals.push(builder.build_proposal(Some(tempo), PitchedRenderingModel::DiscreteNotes));
        if input.pitch.is_some_and(|pitch| !pitch.tracks.is_empty()) {
            proposals
                .push(builder.build_proposal(Some(tempo), PitchedRenderingModel::ContinuousPitch));
        }
    }

    proposals.sort_by(|left, right| {
        right
            .score
            .total
            .total_cmp(&left.score.total)
            .then_with(|| proposal_key(left).cmp(&proposal_key(right)))
    });
    proposals.truncate(config.maximum_proposals);
    for (rank, proposal) in proposals.iter_mut().enumerate() {
        proposal.rank = rank;
        proposal.id = ReconstructionProposalId::from_raw(rank as u64 + 1);
    }
    let selection = proposals
        .first()
        .map(|proposal| ReconstructionSelection::EvidencePreferred(proposal.id))
        .unwrap_or(ReconstructionSelection::Unresolved);

    Ok(ReconstructionSet {
        schema_version: RECONSTRUCTION_SCHEMA_VERSION,
        sample_rate: input.sample_rate,
        source_frame_count: input.source_frame_count,
        evidence: builder.evidence,
        proposals,
        selection,
    })
}

fn validate_inputs(
    input: &ReconstructionInputs<'_>,
    config: ReconstructionConfig,
) -> Result<(), ReconstructionError> {
    if input.sample_rate == 0 {
        return Err(ReconstructionError::InvalidSampleRate);
    }
    if input.source_frame_count == 0 {
        return Err(ReconstructionError::EmptySource);
    }
    if config.maximum_proposals == 0 {
        return Err(ReconstructionError::InvalidConfiguration(
            "maximum proposals must be non-zero",
        ));
    }
    if config.maximum_beat_phases == 0 {
        return Err(ReconstructionError::InvalidConfiguration(
            "maximum beat phases must be non-zero",
        ));
    }
    if config.steps_per_quarter == 0
        || i64::from(config.steps_per_quarter) > PPQ
        || PPQ % i64::from(config.steps_per_quarter) != 0
        || !config.note_split_semitones.is_finite()
        || config.note_split_semitones <= 0.0
        || !unit(config.minimum_pitch_alternative_support)
    {
        return Err(ReconstructionError::InvalidConfiguration(
            "pitch and grid parameters must be finite and positive",
        ));
    }
    if let Some(rhythm) = input.rhythm {
        if rhythm.sample_rate != input.sample_rate {
            return Err(ReconstructionError::AnalysisSampleRateMismatch {
                analysis: "rhythm",
                expected: input.sample_rate,
                actual: rhythm.sample_rate,
            });
        }
        for (index, hit) in rhythm.hits.iter().enumerate() {
            if hit.span.start >= hit.span.end
                || hit.span.end as u64 > input.source_frame_count
                || hit.onset_sample < hit.span.start
                || hit.onset_sample >= hit.span.end
            {
                return Err(ReconstructionError::ObservationOutOfBounds {
                    analysis: "rhythm hit",
                    index,
                });
            }
        }
    }
    if let Some(pitch) = input.pitch {
        if pitch.sample_rate != input.sample_rate {
            return Err(ReconstructionError::AnalysisSampleRateMismatch {
                analysis: "pitch",
                expected: input.sample_rate,
                actual: pitch.sample_rate,
            });
        }
        for (track_index, track) in pitch.tracks.iter().enumerate() {
            if track
                .points
                .iter()
                .any(|point| point.offset_frames >= input.source_frame_count)
            {
                return Err(ReconstructionError::ObservationOutOfBounds {
                    analysis: "pitch track",
                    index: track_index,
                });
            }
        }
        if let Some(index) = pitch
            .frames
            .iter()
            .position(|frame| frame.offset_frames >= input.source_frame_count)
        {
            return Err(ReconstructionError::ObservationOutOfBounds {
                analysis: "pitch frame",
                index,
            });
        }
    }
    if let Some(loom) = input.loom {
        if loom.sample_rate != input.sample_rate {
            return Err(ReconstructionError::AnalysisSampleRateMismatch {
                analysis: "loom",
                expected: input.sample_rate,
                actual: loom.sample_rate,
            });
        }
    }
    if let Some(components) = input.components {
        let last = (components.decomposition.frames.saturating_sub(1) as u64)
            .saturating_mul(components.hop_frames);
        if components.hop_frames == 0
            || components.first_frame.saturating_add(last) >= input.source_frame_count
        {
            return Err(ReconstructionError::InvalidComponentTiming);
        }
    }
    Ok(())
}

fn proposal_key(proposal: &ReconstructionProposal) -> (usize, usize, u8) {
    let (tempo, phase) = proposal
        .tempo
        .map(|tempo| {
            (
                tempo.upstream_tempo_rank + 1,
                tempo.upstream_beat_phase_index + 1,
            )
        })
        .unwrap_or((0, 0));
    let model = match proposal.pitch_model {
        PitchedRenderingModel::DiscreteNotes => 0,
        PitchedRenderingModel::ContinuousPitch => 1,
    };
    (tempo, phase, model)
}

struct Builder<'a> {
    input: &'a ReconstructionInputs<'a>,
    config: ReconstructionConfig,
    evidence: Vec<ReconstructionEvidence>,
    hit_evidence: Vec<ReconstructionEvidenceId>,
    family_evidence: BTreeMap<usize, ReconstructionEvidenceId>,
    tempo_evidence: BTreeMap<usize, ReconstructionEvidenceId>,
    phase_evidence: BTreeMap<usize, ReconstructionEvidenceId>,
    pitch_evidence: Vec<ReconstructionEvidenceId>,
    modulation_evidence: BTreeMap<(usize, usize), ReconstructionEvidenceId>,
    component_evidence: Vec<ReconstructionEvidenceId>,
    loom_evidence: BTreeMap<usize, ReconstructionEvidenceId>,
    residual_evidence: ReconstructionEvidenceId,
}

impl<'a> Builder<'a> {
    fn new(input: &'a ReconstructionInputs<'a>, config: ReconstructionConfig) -> Self {
        Self {
            input,
            config,
            evidence: Vec::new(),
            hit_evidence: Vec::new(),
            family_evidence: BTreeMap::new(),
            tempo_evidence: BTreeMap::new(),
            phase_evidence: BTreeMap::new(),
            pitch_evidence: Vec::new(),
            modulation_evidence: BTreeMap::new(),
            component_evidence: Vec::new(),
            loom_evidence: BTreeMap::new(),
            residual_evidence: ReconstructionEvidenceId::from_raw(0),
        }
    }

    fn push_evidence(
        &mut self,
        kind: ReconstructionEvidenceKind,
        strength: f32,
        analyzer: &str,
        version: &str,
        locator: impl Into<String>,
    ) -> ReconstructionEvidenceId {
        let id = ReconstructionEvidenceId::from_raw(self.evidence.len() as u64 + 1);
        self.evidence.push(ReconstructionEvidence {
            id,
            kind,
            strength: finite_unit(strength),
            provenance: AnalysisProvenance {
                analyzer: analyzer.to_owned(),
                version: version.to_owned(),
                source_revision: self.input.source_revision.map(str::to_owned),
                locator: locator.into(),
            },
        });
        id
    }

    fn collect_evidence(&mut self) {
        if let Some(rhythm) = self.input.rhythm {
            for (index, hit) in rhythm.hits.iter().enumerate() {
                let strength = (0.55 * finite_unit(hit.novelty_strength)
                    + 0.25 * finite_unit(hit.threshold_excess)
                    + 0.20 * finite_unit(hit.family_similarity))
                .clamp(0.0, 1.0);
                let evidence = self.push_evidence(
                    ReconstructionEvidenceKind::RhythmHit { event_index: index },
                    strength,
                    "audec.rhythm",
                    "1",
                    format!("hits[{index}]"),
                );
                self.hit_evidence.push(evidence);
            }
            for family in &rhythm.event_families {
                let evidence = self.push_evidence(
                    ReconstructionEvidenceKind::AnonymousEventFamily {
                        family_id: family.id,
                    },
                    family.evidence,
                    "audec.rhythm",
                    "1",
                    format!("event_families[id={}]", family.id),
                );
                self.family_evidence.insert(family.id, evidence);
            }
            for tempo in &rhythm.tempo_hypotheses {
                let evidence = self.push_evidence(
                    ReconstructionEvidenceKind::Tempo {
                        tempo_rank: tempo.rank,
                    },
                    tempo.evidence,
                    "audec.rhythm",
                    "1",
                    format!("tempo_hypotheses[rank={}]", tempo.rank),
                );
                self.tempo_evidence.insert(tempo.rank, evidence);
            }
            for (index, phase) in rhythm.beat_phase_hypotheses.iter().enumerate() {
                let evidence = self.push_evidence(
                    ReconstructionEvidenceKind::BeatPhase {
                        beat_phase_index: index,
                    },
                    phase.score,
                    "audec.rhythm",
                    "1",
                    format!("beat_phase_hypotheses[{index}]"),
                );
                self.phase_evidence.insert(index, evidence);
            }
        }
        if let Some(pitch) = self.input.pitch {
            for (track_index, track) in pitch.tracks.iter().enumerate() {
                let evidence = self.push_evidence(
                    ReconstructionEvidenceKind::PitchTrack { track_index },
                    track.confidence,
                    "audec.pitch",
                    "1",
                    format!("tracks[{track_index}]"),
                );
                self.pitch_evidence.push(evidence);
                for (index, modulation) in track.modulation.iter().enumerate() {
                    let strength = match modulation {
                        ModulationEvidence::Glide { confidence, .. }
                        | ModulationEvidence::Vibrato { confidence, .. } => *confidence,
                    };
                    let evidence = self.push_evidence(
                        ReconstructionEvidenceKind::PitchModulation { track_index, index },
                        strength,
                        "audec.pitch",
                        "1",
                        format!("tracks[{track_index}].modulation[{index}]"),
                    );
                    self.modulation_evidence
                        .insert((track_index, index), evidence);
                }
            }
        }
        if let Some(components) = self.input.components {
            for (index, component) in components.decomposition.components.iter().enumerate() {
                let evidence = self.push_evidence(
                    ReconstructionEvidenceKind::LatentComponent {
                        component_index: index,
                    },
                    component.confidence,
                    components.analyzer_name,
                    components.analyzer_version,
                    format!("components[{index}]"),
                );
                self.component_evidence.push(evidence);
            }
        }
        if let Some(loom) = self.input.loom {
            for cluster in &loom.clusters {
                let family = cluster.template.cluster_id;
                let evidence = self.push_evidence(
                    ReconstructionEvidenceKind::LoomTemplate {
                        anonymous_cluster_id: family,
                    },
                    cluster.template.exemplar_agreement,
                    "audec.loom",
                    "1",
                    format!("clusters[id={family}]"),
                );
                self.loom_evidence.insert(family, evidence);
            }
        }
        let full_range = SourceFrameRange {
            start: 0,
            end: self.input.source_frame_count,
        };
        self.residual_evidence = self.push_evidence(
            ReconstructionEvidenceKind::ResidualCoverage { range: full_range },
            1.0,
            "audec.reconstruction",
            "1",
            "full-source residual safety layer",
        );
    }

    fn tempo_choices(&self) -> Vec<TempoChoice> {
        let Some(rhythm) = self.input.rhythm else {
            return Vec::new();
        };
        let mut indexed: Vec<_> = rhythm.beat_phase_hypotheses.iter().enumerate().collect();
        indexed.sort_by(|(left_index, left), (right_index, right)| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.tempo_rank.cmp(&right.tempo_rank))
                .then_with(|| left_index.cmp(right_index))
        });
        indexed.truncate(self.config.maximum_beat_phases);
        indexed
            .into_iter()
            .map(|(index, phase)| TempoChoice {
                bpm: finite_positive_or(phase.bpm, 120.0),
                phase_source_frame: seconds_to_frame(phase.phase_seconds, self.input.sample_rate),
                upstream_tempo_rank: phase.tempo_rank,
                upstream_beat_phase_index: index,
                support: finite_unit(phase.score),
            })
            .collect()
    }

    fn build_proposal(
        &self,
        tempo: Option<TempoChoice>,
        pitch_model: PitchedRenderingModel,
    ) -> ReconstructionProposal {
        let mut next_track = 1_u64;
        let mut next_slice = 1_u64;
        let mut next_trigger = 1_u64;
        let mut next_note = 1_u64;
        let mut next_automation = 1_u64;
        let mut next_modulation = 1_u64;
        let mut next_effect = 1_u64;
        let mut tracks = self.build_hit_tracks(
            tempo,
            &mut next_track,
            &mut next_slice,
            &mut next_trigger,
            &mut next_effect,
        );
        tracks.extend(self.build_pitch_tracks(
            tempo,
            pitch_model,
            &mut next_track,
            &mut next_note,
            &mut next_automation,
            &mut next_modulation,
        ));
        tracks.extend(self.build_component_tracks(&mut next_track, &mut next_automation));
        tracks.push(self.build_residual_track(&mut next_track, &tracks));

        let mut evidence = BTreeSet::new();
        for track in &tracks {
            evidence.extend(track.evidence.iter().copied());
        }
        if let Some(tempo) = tempo {
            if let Some(id) = self.tempo_evidence.get(&tempo.upstream_tempo_rank) {
                evidence.insert(*id);
            }
            if let Some(id) = self.phase_evidence.get(&tempo.upstream_beat_phase_index) {
                evidence.insert(*id);
            }
        }
        let score = self.score(&tracks, tempo, pitch_model, &evidence);
        let pitch_label = match pitch_model {
            PitchedRenderingModel::DiscreteNotes => "segmented notes",
            PitchedRenderingModel::ContinuousPitch => "continuous pitch",
        };
        let label = tempo.map_or_else(
            || format!("source-time · {pitch_label}"),
            |tempo| format!("{:.2} BPM grid · {pitch_label}", tempo.bpm),
        );
        let mut caveats = vec![
            "Tracks are overlapping explanations, not isolated physical sources.".to_owned(),
            "Anonymous hit families intentionally retain neutral identities.".to_owned(),
            "Residual audio must remain audible until subtraction is rendered and inspected."
                .to_owned(),
        ];
        if tempo.is_some() {
            caveats.push(
                "Grid placement is hypothetical; original source timing is retained as microtiming."
                    .to_owned(),
            );
        }
        ReconstructionProposal {
            id: ReconstructionProposalId::from_raw(0),
            rank: 0,
            label,
            tempo,
            pitch_model,
            tracks,
            score,
            evidence: evidence.into_iter().collect(),
            caveats,
        }
    }

    fn build_hit_tracks(
        &self,
        tempo: Option<TempoChoice>,
        next_track: &mut u64,
        next_slice: &mut u64,
        next_trigger: &mut u64,
        next_effect: &mut u64,
    ) -> Vec<EditableTrackProposal> {
        let Some(rhythm) = self.input.rhythm else {
            return Vec::new();
        };
        let mut by_family = BTreeMap::<Option<usize>, Vec<usize>>::new();
        for (event_index, hit) in rhythm.hits.iter().enumerate() {
            by_family.entry(hit.family).or_default().push(event_index);
        }
        let maximum_novelty = rhythm
            .hits
            .iter()
            .map(|hit| finite_nonnegative(hit.novelty_strength))
            .fold(0.0_f32, f32::max)
            .max(1.0e-9);
        let mut result = Vec::new();
        for (family, event_indices) in by_family {
            let mut slices = Vec::new();
            let mut triggers = Vec::new();
            let mut evidence = BTreeSet::new();
            let shared_slice = family.and_then(|family_id| {
                let representative = rhythm
                    .event_families
                    .iter()
                    .find(|candidate| candidate.id == family_id)
                    .map(|candidate| candidate.medoid.event_index)
                    .filter(|index| *index < rhythm.hits.len())
                    .or_else(|| event_indices.first().copied())?;
                let hit = &rhythm.hits[representative];
                let id = take_id(next_slice, SampleSliceId::from_raw);
                let mut slice_evidence = vec![self.hit_evidence[representative]];
                if let Some(id) = self.family_evidence.get(&family_id) {
                    slice_evidence.push(*id);
                    evidence.insert(*id);
                }
                if let Some(id) = self.loom_evidence.get(&family_id) {
                    slice_evidence.push(*id);
                    evidence.insert(*id);
                }
                evidence.extend(slice_evidence.iter().copied());
                slices.push(SampleSliceProposal {
                    id,
                    source: source_range(hit.span),
                    onset_offset_frames: hit.onset_sample.saturating_sub(hit.span.start) as u64,
                    role: SliceRole::AnonymousFamilyRepresentative,
                    anonymous_family_id: Some(family_id),
                    representative_event_index: representative,
                    confidence: family_confidence(rhythm, family_id),
                    evidence: slice_evidence,
                });
                Some(id)
            });

            for event_index in event_indices {
                let hit = &rhythm.hits[event_index];
                let slice = shared_slice.unwrap_or_else(|| {
                    let id = take_id(next_slice, SampleSliceId::from_raw);
                    slices.push(SampleSliceProposal {
                        id,
                        source: source_range(hit.span),
                        onset_offset_frames: hit.onset_sample.saturating_sub(hit.span.start) as u64,
                        role: SliceRole::UnclusteredEvent,
                        anonymous_family_id: None,
                        representative_event_index: event_index,
                        confidence: evidence_strength(
                            &self.evidence,
                            self.hit_evidence[event_index],
                        ),
                        evidence: vec![self.hit_evidence[event_index]],
                    });
                    id
                });
                let musical = tempo.map(|tempo| {
                    frame_to_grid(
                        hit.onset_sample as u64,
                        tempo,
                        self.input.sample_rate,
                        self.config.steps_per_quarter,
                    )
                });
                let (musical_tick, micro_offset_frames) = musical
                    .map(|placement| (Some(placement.tick), placement.micro_offset_frames))
                    .unwrap_or((None, 0));
                let confidence = evidence_strength(&self.evidence, self.hit_evidence[event_index]);
                evidence.insert(self.hit_evidence[event_index]);
                triggers.push(SampleTriggerProposal {
                    id: take_id(next_trigger, TriggerId::from_raw),
                    slice,
                    source_onset_frame: hit.onset_sample as u64,
                    gain: (finite_nonnegative(hit.novelty_strength) / maximum_novelty)
                        .sqrt()
                        .clamp(0.0, 1.0),
                    musical_tick,
                    micro_offset_frames,
                    confidence,
                    evidence: vec![self.hit_evidence[event_index]],
                });
            }
            triggers.sort_by_key(|trigger| (trigger.source_onset_frame, trigger.id));
            let step_pattern = tempo.map(|tempo| StepPatternProposal {
                steps_per_quarter: self.config.steps_per_quarter,
                placements: triggers
                    .iter()
                    .map(|trigger| StepPlacement {
                        trigger: trigger.id,
                        step: trigger.musical_tick.unwrap_or(0)
                            / (PPQ / i64::from(self.config.steps_per_quarter)),
                        micro_offset_frames: trigger.micro_offset_frames,
                        velocity: trigger.gain,
                        confidence: trigger.confidence,
                    })
                    .collect(),
                tempo,
                evidence: tempo_evidence(self, tempo),
            });
            let effects = inferred_hit_effects(rhythm, &triggers, &self.hit_evidence, next_effect);
            let confidence = mean_strength(&self.evidence, evidence.iter().copied());
            result.push(EditableTrackProposal {
                id: take_id(next_track, ReconstructionTrackId::from_raw),
                label: family.map_or_else(
                    || "unclustered hit observations".to_owned(),
                    |family| format!("anonymous hit family {family}"),
                ),
                kind: family.map_or(
                    ReconstructionTrackKind::UnclusteredHits,
                    |upstream_family_id| ReconstructionTrackKind::AnonymousHitFamily {
                        upstream_family_id,
                    },
                ),
                sample_slices: slices,
                triggers,
                pitched_events: Vec::new(),
                step_pattern,
                automations: Vec::new(),
                modulations: Vec::new(),
                effects,
                latent_component: None,
                residual: None,
                confidence,
                evidence: evidence.into_iter().collect(),
            });
        }
        result
    }

    fn build_pitch_tracks(
        &self,
        tempo: Option<TempoChoice>,
        model: PitchedRenderingModel,
        next_track: &mut u64,
        next_note: &mut u64,
        next_automation: &mut u64,
        next_modulation: &mut u64,
    ) -> Vec<EditableTrackProposal> {
        let Some(pitch) = self.input.pitch else {
            return Vec::new();
        };
        pitch
            .tracks
            .iter()
            .enumerate()
            .map(|(track_index, track)| {
                let track_evidence = self.pitch_evidence[track_index];
                let segments = segment_pitch_track(
                    track,
                    pitch.hop_size as u64,
                    self.config.maximum_pitch_gap_frames,
                    match model {
                        PitchedRenderingModel::DiscreteNotes => self.config.note_split_semitones,
                        PitchedRenderingModel::ContinuousPitch => f32::INFINITY,
                    },
                    self.input.source_frame_count,
                );
                let mut pitched_events = Vec::new();
                for segment in segments {
                    let alternatives = pitch_alternatives(
                        pitch,
                        segment.range,
                        segment.preferred_hz,
                        self.config.minimum_pitch_alternative_support,
                        track_evidence,
                        segment.confidence,
                    );
                    let musical = tempo.map(|tempo| {
                        let start = frame_to_grid(
                            segment.range.start,
                            tempo,
                            self.input.sample_rate,
                            self.config.steps_per_quarter,
                        );
                        let end = frame_to_tick(segment.range.end, tempo, self.input.sample_rate);
                        (start, end)
                    });
                    let (musical_start_tick, musical_duration_ticks, micro_offset_frames) = musical
                        .map(|(start, end)| {
                            (
                                Some(start.tick),
                                Some(end.saturating_sub(start.tick).max(1) as u64),
                                start.micro_offset_frames,
                            )
                        })
                        .unwrap_or((None, None, 0));
                    pitched_events.push(PitchedEventProposal {
                        id: take_id(next_note, PitchedEventId::from_raw),
                        source: segment.range,
                        musical_start_tick,
                        musical_duration_ticks,
                        micro_offset_frames,
                        velocity: segment.mean_level,
                        selection: if alternatives.len() > 1 {
                            PitchChoiceSelection::Unresolved
                        } else {
                            PitchChoiceSelection::EvidencePreferred(0)
                        },
                        alternatives,
                        pitch_curve: segment.curve,
                        confidence: segment.confidence,
                        evidence: vec![track_evidence],
                    });
                }
                let automations = pitched_events
                    .iter()
                    .filter(|event| event.pitch_curve.len() > 1)
                    .map(|event| AutomationProposal {
                        id: take_id(next_automation, AutomationProposalId::from_raw),
                        target: AutomationTarget::PitchCents,
                        interpolation: AutomationInterpolation::Smooth,
                        points: event
                            .pitch_curve
                            .iter()
                            .map(|point| AutomationProposalPoint {
                                source_frame: event.source.start + point.offset_frames,
                                value: point.cents_from_preferred,
                            })
                            .collect(),
                        confidence: event.confidence,
                        evidence: event.evidence.clone(),
                    })
                    .collect();
                let modulations = track
                    .modulation
                    .iter()
                    .enumerate()
                    .map(|(index, modulation)| {
                        modulation_proposal(
                            modulation,
                            self.modulation_evidence[&(track_index, index)],
                            next_modulation,
                        )
                    })
                    .collect();
                EditableTrackProposal {
                    id: take_id(next_track, ReconstructionTrackId::from_raw),
                    label: format!("pitched stream {track_index}"),
                    kind: ReconstructionTrackKind::PitchedStream {
                        upstream_track_index: track_index,
                        rendering: model,
                    },
                    sample_slices: Vec::new(),
                    triggers: Vec::new(),
                    pitched_events,
                    step_pattern: None,
                    automations,
                    modulations,
                    effects: Vec::new(),
                    latent_component: None,
                    residual: None,
                    confidence: finite_unit(track.confidence),
                    evidence: vec![track_evidence],
                }
            })
            .collect()
    }

    fn build_component_tracks(
        &self,
        next_track: &mut u64,
        next_automation: &mut u64,
    ) -> Vec<EditableTrackProposal> {
        let Some(timed) = self.input.components else {
            return Vec::new();
        };
        timed
            .decomposition
            .components
            .iter()
            .enumerate()
            .map(|(index, component)| {
                let evidence = self.component_evidence[index];
                let maximum = component
                    .activation
                    .iter()
                    .copied()
                    .map(finite_nonnegative)
                    .fold(0.0_f32, f32::max)
                    .max(1.0e-9);
                let points = component
                    .activation
                    .iter()
                    .enumerate()
                    .map(|(frame, value)| AutomationProposalPoint {
                        source_frame: timed
                            .first_frame
                            .saturating_add(frame as u64 * timed.hop_frames),
                        value: (finite_nonnegative(*value) / maximum).clamp(0.0, 1.0),
                    })
                    .collect();
                EditableTrackProposal {
                    id: take_id(next_track, ReconstructionTrackId::from_raw),
                    label: format!("latent spectral component {index}"),
                    kind: ReconstructionTrackKind::LatentComponent {
                        upstream_component_index: index,
                    },
                    sample_slices: Vec::new(),
                    triggers: Vec::new(),
                    pitched_events: Vec::new(),
                    step_pattern: None,
                    automations: vec![AutomationProposal {
                        id: take_id(next_automation, AutomationProposalId::from_raw),
                        target: AutomationTarget::SpectralActivity,
                        interpolation: AutomationInterpolation::Smooth,
                        points,
                        confidence: finite_unit(component.confidence),
                        evidence: vec![evidence],
                    }],
                    modulations: Vec::new(),
                    effects: Vec::new(),
                    latent_component: Some(LatentComponentProposal {
                        upstream_component_index: index,
                        spectral_template: component.spectral_template.clone(),
                        first_frame: timed.first_frame,
                        hop_frames: timed.hop_frames,
                        confidence: finite_unit(component.confidence),
                        evidence: vec![evidence],
                    }),
                    residual: None,
                    confidence: finite_unit(component.confidence),
                    evidence: vec![evidence],
                }
            })
            .collect()
    }

    fn build_residual_track(
        &self,
        next_track: &mut u64,
        constructive_tracks: &[EditableTrackProposal],
    ) -> EditableTrackProposal {
        let explained =
            estimated_constructive_coverage(constructive_tracks, self.input.source_frame_count);
        EditableTrackProposal {
            id: take_id(next_track, ReconstructionTrackId::from_raw),
            label: "unexplained / overlapping source residual".to_owned(),
            kind: ReconstructionTrackKind::Residual,
            sample_slices: Vec::new(),
            triggers: Vec::new(),
            pitched_events: Vec::new(),
            step_pattern: None,
            automations: Vec::new(),
            modulations: Vec::new(),
            effects: Vec::new(),
            latent_component: None,
            residual: Some(ResidualAudioProposal {
                source: SourceFrameRange {
                    start: 0,
                    end: self.input.source_frame_count,
                },
                preferred_mode: ResidualRenderMode::SubtractConstructiveRender,
                fallback_mode: ResidualRenderMode::OriginalSafetyLayer,
                estimated_fraction: (1.0 - explained).clamp(0.0, 1.0),
                evidence: vec![self.residual_evidence],
            }),
            confidence: 1.0,
            evidence: vec![self.residual_evidence],
        }
    }

    fn score(
        &self,
        tracks: &[EditableTrackProposal],
        tempo: Option<TempoChoice>,
        pitch_model: PitchedRenderingModel,
        evidence: &BTreeSet<ReconstructionEvidenceId>,
    ) -> ProposalScore {
        let observation_support = mean_strength(
            &self.evidence,
            evidence
                .iter()
                .copied()
                .filter(|id| *id != self.residual_evidence),
        );
        let pattern_support = tempo.map_or(0.35, |tempo| tempo.support);
        let constructive = tracks
            .iter()
            .filter(|track| !matches!(track.kind, ReconstructionTrackKind::Residual))
            .count();
        let editability = (constructive as f32 / 6.0).clamp(0.0, 1.0);
        let estimated_coverage =
            estimated_constructive_coverage(tracks, self.input.source_frame_count);
        let assumption_penalty = match (tempo.is_some(), pitch_model) {
            (false, PitchedRenderingModel::DiscreteNotes) => 0.02,
            (false, PitchedRenderingModel::ContinuousPitch) => 0.035,
            (true, PitchedRenderingModel::DiscreteNotes) => 0.08,
            (true, PitchedRenderingModel::ContinuousPitch) => 0.10,
        };
        let total = (0.46 * observation_support
            + 0.18 * pattern_support
            + 0.18 * editability
            + 0.18 * estimated_coverage
            - assumption_penalty)
            .clamp(0.0, 1.0);
        ProposalScore {
            observation_support,
            pattern_support,
            editability,
            estimated_coverage,
            assumption_penalty,
            total,
        }
    }
}

#[derive(Clone, Debug)]
struct PitchSegment {
    range: SourceFrameRange,
    preferred_hz: f32,
    mean_level: f32,
    confidence: f32,
    curve: Vec<PitchCurvePoint>,
}

fn segment_pitch_track(
    track: &PitchTrack,
    hop_frames: u64,
    maximum_gap_frames: usize,
    split_semitones: f32,
    source_frames: u64,
) -> Vec<PitchSegment> {
    let mut groups: Vec<Vec<_>> = Vec::new();
    let mut current = Vec::new();
    let mut last_index = None;
    let mut last_hz = None;
    for (index, point) in track.points.iter().enumerate() {
        let Some(hz) = point.hz.filter(|hz| hz.is_finite() && *hz > 0.0) else {
            continue;
        };
        let gap = last_index.map_or(0, |last: usize| index.saturating_sub(last + 1));
        let jump = last_hz.map_or(0.0, |last| semitone_distance(last, hz));
        if !current.is_empty() && (gap > maximum_gap_frames || jump > split_semitones) {
            groups.push(std::mem::take(&mut current));
        }
        current.push((index, point, hz));
        last_index = Some(index);
        last_hz = Some(hz);
    }
    if !current.is_empty() {
        groups.push(current);
    }

    groups
        .into_iter()
        .filter_map(|group| {
            let first = group
                .first()?
                .1
                .offset_frames
                .min(source_frames.saturating_sub(1));
            let last = group.last()?.1.offset_frames;
            let end = last.saturating_add(hop_frames.max(1)).min(source_frames);
            let range = SourceFrameRange::new(first, end)?;
            let preferred_hz = weighted_geometric_pitch(
                group.iter().map(|(_, point, hz)| (*hz, point.confidence)),
            );
            let confidence = mean(
                group
                    .iter()
                    .map(|(_, point, _)| finite_unit(point.confidence)),
            );
            let mean_level = mean(
                group
                    .iter()
                    .map(|(_, point, _)| dbfs_to_unit(point.support.level_dbfs)),
            );
            let curve = group
                .iter()
                .map(|(_, point, hz)| PitchCurvePoint {
                    offset_frames: point.offset_frames.saturating_sub(range.start),
                    cents_from_preferred: 1_200.0 * (*hz / preferred_hz).log2(),
                    confidence: finite_unit(point.confidence),
                })
                .collect();
            Some(PitchSegment {
                range,
                preferred_hz,
                mean_level,
                confidence,
                curve,
            })
        })
        .collect()
}

fn pitch_alternatives(
    pitch: &PitchAnalysis,
    range: SourceFrameRange,
    tracked_hz: f32,
    minimum_support: f32,
    evidence: ReconstructionEvidenceId,
    tracked_support: f32,
) -> Vec<PitchChoice> {
    #[derive(Clone, Copy)]
    struct Aggregate {
        log_sum: f64,
        weight: f64,
        strongest: f32,
        interpretation: PitchInterpretation,
    }
    let mut aggregates = BTreeMap::<i32, Aggregate>::new();
    for frame in &pitch.frames {
        if !range.contains(frame.offset_frames) {
            continue;
        }
        for candidate in &frame.candidates {
            if candidate.confidence < minimum_support
                || !candidate.hz.is_finite()
                || candidate.hz <= 0.0
            {
                continue;
            }
            let cents_bin = (1_200.0 * candidate.hz.log2() / 35.0).round() as i32;
            let weight = f64::from(candidate.confidence.max(1.0e-6));
            let interpretation = match candidate.method {
                crate::pitch::CandidateMethod::Yin => PitchInterpretation::PeriodicityCandidate,
                crate::pitch::CandidateMethod::HarmonicSpectrum => {
                    PitchInterpretation::HarmonicSpectrumCandidate
                }
                crate::pitch::CandidateMethod::Combined => PitchInterpretation::CombinedMeasurement,
            };
            aggregates
                .entry(cents_bin)
                .and_modify(|aggregate| {
                    aggregate.log_sum += f64::from(candidate.hz).ln() * weight;
                    aggregate.weight += weight;
                    if candidate.confidence > aggregate.strongest {
                        aggregate.strongest = candidate.confidence;
                        aggregate.interpretation = interpretation;
                    }
                })
                .or_insert(Aggregate {
                    log_sum: f64::from(candidate.hz).ln() * weight,
                    weight,
                    strongest: candidate.confidence,
                    interpretation,
                });
        }
    }
    let mut choices: Vec<_> = aggregates
        .into_values()
        .map(|aggregate| {
            pitch_choice(
                (aggregate.log_sum / aggregate.weight).exp() as f32,
                aggregate.interpretation,
                aggregate.strongest,
                evidence,
            )
        })
        .collect();
    choices.push(pitch_choice(
        tracked_hz,
        PitchInterpretation::TrackedFundamentalCandidate,
        tracked_support,
        evidence,
    ));
    choices.sort_by(|left, right| {
        right
            .support
            .total_cmp(&left.support)
            .then_with(|| left.hz.total_cmp(&right.hz))
    });
    let mut consolidated = Vec::<PitchChoice>::new();
    for choice in choices {
        if consolidated
            .iter()
            .any(|existing| semitone_distance(existing.hz, choice.hz) < 0.35)
        {
            continue;
        }
        consolidated.push(choice);
        if consolidated.len() == 4 {
            break;
        }
    }
    consolidated
}

fn pitch_choice(
    hz: f32,
    interpretation: PitchInterpretation,
    support: f32,
    evidence: ReconstructionEvidenceId,
) -> PitchChoice {
    let midi_float = 69.0 + 12.0 * (hz / 440.0).log2();
    let midi_key = midi_float.round().clamp(0.0, 127.0) as u8;
    PitchChoice {
        hz,
        midi_key,
        cents: (midi_float - f32::from(midi_key)) * 100.0,
        interpretation,
        support: finite_unit(support),
        evidence: vec![evidence],
    }
}

fn modulation_proposal(
    evidence: &ModulationEvidence,
    evidence_id: ReconstructionEvidenceId,
    next_id: &mut u64,
) -> ModulationProposal {
    match evidence {
        ModulationEvidence::Glide {
            direction,
            extent_semitones,
            confidence,
            ..
        } => ModulationProposal {
            id: take_id(next_id, ModulationProposalId::from_raw),
            phenomenon: ModulationPhenomenon::PitchGlide,
            rate_hz: None,
            extent_semitones: match direction {
                GlideDirection::Rising => finite_nonnegative(extent_semitones.abs()),
                GlideDirection::Falling => -finite_nonnegative(extent_semitones.abs()),
            },
            implementations: vec![
                ModulationImplementationCandidate {
                    implementation: ModulationImplementation::DirectPitchAutomation,
                    support: finite_unit(*confidence),
                },
                ModulationImplementationCandidate {
                    implementation: ModulationImplementation::FrequencyModulation,
                    support: finite_unit(*confidence * 0.35),
                },
                ModulationImplementationCandidate {
                    implementation: ModulationImplementation::Unspecified,
                    support: finite_unit(*confidence * 0.5),
                },
            ],
            selected_implementation: None,
            confidence: finite_unit(*confidence),
            evidence: vec![evidence_id],
        },
        ModulationEvidence::Vibrato {
            rate_hz,
            extent_semitones,
            confidence,
            ..
        } => ModulationProposal {
            id: take_id(next_id, ModulationProposalId::from_raw),
            phenomenon: ModulationPhenomenon::PeriodicPitchMotion,
            rate_hz: (rate_hz.is_finite() && *rate_hz > 0.0).then_some(*rate_hz),
            extent_semitones: finite_nonnegative(*extent_semitones),
            implementations: vec![
                ModulationImplementationCandidate {
                    implementation: ModulationImplementation::PitchLfo,
                    support: finite_unit(*confidence * 0.90),
                },
                ModulationImplementationCandidate {
                    implementation: ModulationImplementation::FrequencyModulation,
                    support: finite_unit(*confidence * 0.65),
                },
                ModulationImplementationCandidate {
                    implementation: ModulationImplementation::ModulatedDelay,
                    support: finite_unit(*confidence * 0.45),
                },
                ModulationImplementationCandidate {
                    implementation: ModulationImplementation::Unspecified,
                    support: finite_unit(*confidence * 0.50),
                },
            ],
            selected_implementation: None,
            confidence: finite_unit(*confidence),
            evidence: vec![evidence_id],
        },
    }
}

fn inferred_hit_effects(
    rhythm: &RhythmDeprojection,
    triggers: &[SampleTriggerProposal],
    hit_evidence: &[ReconstructionEvidenceId],
    next_id: &mut u64,
) -> Vec<EffectProposal> {
    let event_indices: BTreeSet<_> = triggers
        .iter()
        .filter_map(|trigger| {
            trigger
                .evidence
                .first()
                .and_then(|id| hit_evidence.iter().position(|candidate| candidate == id))
        })
        .collect();
    if event_indices.is_empty() {
        return Vec::new();
    }
    let mean_decay = mean(
        event_indices
            .iter()
            .map(|index| rhythm.hits[*index].decay_seconds),
    );
    let mean_noisiness = mean(
        event_indices
            .iter()
            .map(|index| rhythm.hits[*index].noisiness),
    );
    let stereo: Vec<_> = event_indices
        .iter()
        .filter_map(|index| rhythm.hits[*index].stereo)
        .collect();
    let evidence: Vec<_> = event_indices
        .iter()
        .map(|index| hit_evidence[*index])
        .collect();
    let mut effects = Vec::new();
    if mean_decay > 0.06 {
        effects.push(EffectProposal {
            id: take_id(next_id, EffectProposalId::from_raw),
            phenomenon: if mean_noisiness > 0.55 {
                EffectPhenomenon::DiffuseOrNoisyTail
            } else {
                EffectPhenomenon::DecayingTail
            },
            implementations: vec![
                EffectImplementationCandidate {
                    implementation: EffectImplementation::SourceLayer,
                    support: 0.85,
                },
                EffectImplementationCandidate {
                    implementation: EffectImplementation::Envelope,
                    support: 0.62,
                },
                EffectImplementationCandidate {
                    implementation: EffectImplementation::Reverberation,
                    support: (mean_noisiness * 0.7).clamp(0.0, 1.0),
                },
                EffectImplementationCandidate {
                    implementation: EffectImplementation::Convolution,
                    support: (mean_noisiness * 0.5).clamp(0.0, 1.0),
                },
                EffectImplementationCandidate {
                    implementation: EffectImplementation::Unspecified,
                    support: 0.5,
                },
            ],
            selected_implementation: None,
            confidence: (mean_decay / 0.5).clamp(0.0, 1.0),
            evidence: evidence.clone(),
        });
    }
    let mean_width = mean(stereo.iter().map(|observation| observation.width));
    if !stereo.is_empty() && mean_width > 0.15 {
        effects.push(EffectProposal {
            id: take_id(next_id, EffectProposalId::from_raw),
            phenomenon: EffectPhenomenon::StereoSpread,
            implementations: vec![
                EffectImplementationCandidate {
                    implementation: EffectImplementation::SourceLayer,
                    support: 0.9,
                },
                EffectImplementationCandidate {
                    implementation: EffectImplementation::StereoProcessor,
                    support: (mean_width * 0.7).clamp(0.0, 1.0),
                },
                EffectImplementationCandidate {
                    implementation: EffectImplementation::Unspecified,
                    support: 0.5,
                },
            ],
            selected_implementation: None,
            confidence: mean_width.clamp(0.0, 1.0),
            evidence,
        });
    }
    effects
}

#[derive(Clone, Copy)]
struct GridPlacement {
    tick: i64,
    micro_offset_frames: i64,
}

fn frame_to_grid(
    frame: u64,
    tempo: TempoChoice,
    sample_rate: u32,
    steps_per_quarter: u16,
) -> GridPlacement {
    let raw_tick = frame_to_tick(frame, tempo, sample_rate);
    let grid = PPQ / i64::from(steps_per_quarter);
    let lower = raw_tick.div_euclid(grid);
    let remainder = raw_tick.rem_euclid(grid);
    let tick = lower
        .saturating_add(i64::from(remainder.saturating_mul(2) >= grid))
        .saturating_mul(grid);
    let quantized_frame = tick_to_frame(tick, tempo, sample_rate);
    GridPlacement {
        tick,
        micro_offset_frames: (frame as i128 - quantized_frame as i128)
            .clamp(i64::MIN as i128, i64::MAX as i128) as i64,
    }
}

fn frame_to_tick(frame: u64, tempo: TempoChoice, sample_rate: u32) -> i64 {
    let relative = frame as i128 - tempo.phase_source_frame as i128;
    // Use a high precision fixed BPM representation to avoid cumulative drift.
    let bpm_micros = (f64::from(tempo.bpm) * 1_000_000.0).round() as i128;
    let numerator = relative
        .saturating_mul(PPQ as i128)
        .saturating_mul(bpm_micros);
    let denominator = (sample_rate as i128).saturating_mul(60_000_000_i128);
    (numerator.div_euclid(denominator)).clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn tick_to_frame(tick: i64, tempo: TempoChoice, sample_rate: u32) -> i64 {
    let bpm_micros = (f64::from(tempo.bpm) * 1_000_000.0).round() as i128;
    let relative = (tick as i128)
        .saturating_mul(sample_rate as i128)
        .saturating_mul(60_000_000_i128)
        / (PPQ as i128).saturating_mul(bpm_micros.max(1));
    (relative + tempo.phase_source_frame as i128).clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn tempo_evidence(builder: &Builder<'_>, tempo: TempoChoice) -> Vec<ReconstructionEvidenceId> {
    let mut result = Vec::new();
    if let Some(id) = builder.tempo_evidence.get(&tempo.upstream_tempo_rank) {
        result.push(*id);
    }
    if let Some(id) = builder.phase_evidence.get(&tempo.upstream_beat_phase_index) {
        result.push(*id);
    }
    result
}

fn source_range(span: SampleSpan) -> SourceFrameRange {
    SourceFrameRange {
        start: span.start as u64,
        end: span.end as u64,
    }
}

fn family_confidence(rhythm: &RhythmDeprojection, family_id: usize) -> f32 {
    rhythm
        .event_families
        .iter()
        .find(|family| family.id == family_id)
        .map(|family| finite_unit(family.evidence))
        .unwrap_or(0.0)
}

fn estimated_constructive_coverage(tracks: &[EditableTrackProposal], source_frames: u64) -> f32 {
    if source_frames == 0 {
        return 0.0;
    }
    let mut ranges = Vec::new();
    for track in tracks {
        for trigger in &track.triggers {
            let Some(slice) = track
                .sample_slices
                .iter()
                .find(|slice| slice.id == trigger.slice)
            else {
                continue;
            };
            let start = trigger
                .source_onset_frame
                .saturating_sub(slice.onset_offset_frames);
            ranges.push(SourceFrameRange {
                start,
                end: start.saturating_add(slice.source.len()).min(source_frames),
            });
        }
        ranges.extend(track.pitched_events.iter().map(|event| event.source));
        if let Some(component) = &track.latent_component {
            if !component.spectral_template.is_empty() {
                ranges.push(SourceFrameRange {
                    start: component.first_frame,
                    end: source_frames,
                });
            }
        }
    }
    ranges.sort();
    let mut covered = 0_u64;
    let mut cursor: Option<SourceFrameRange> = None;
    for range in ranges {
        cursor = match cursor {
            None => Some(range),
            Some(mut current) if range.start <= current.end => {
                current.end = current.end.max(range.end);
                Some(current)
            }
            Some(current) => {
                covered = covered.saturating_add(current.len());
                Some(range)
            }
        };
    }
    if let Some(current) = cursor {
        covered = covered.saturating_add(current.len());
    }
    (covered as f64 / source_frames as f64).clamp(0.0, 1.0) as f32
}

fn weighted_geometric_pitch(values: impl Iterator<Item = (f32, f32)>) -> f32 {
    let mut log_sum = 0.0_f64;
    let mut weight_sum = 0.0_f64;
    for (hz, weight) in values {
        let weight = f64::from(finite_unit(weight).max(1.0e-6));
        log_sum += f64::from(hz).ln() * weight;
        weight_sum += weight;
    }
    (log_sum / weight_sum.max(1.0e-9)).exp() as f32
}

fn semitone_distance(left: f32, right: f32) -> f32 {
    (12.0 * (right / left).log2()).abs()
}

fn seconds_to_frame(seconds: f64, sample_rate: u32) -> i64 {
    if !seconds.is_finite() {
        return 0;
    }
    (seconds * f64::from(sample_rate))
        .round()
        .clamp(i64::MIN as f64, i64::MAX as f64) as i64
}

fn dbfs_to_unit(dbfs: f32) -> f32 {
    if !dbfs.is_finite() {
        return 0.0;
    }
    (10.0_f32.powf(dbfs / 20.0)).clamp(0.0, 1.0)
}

fn evidence_strength(evidence: &[ReconstructionEvidence], id: ReconstructionEvidenceId) -> f32 {
    evidence
        .get(id.get().saturating_sub(1) as usize)
        .map(|evidence| evidence.strength)
        .unwrap_or(0.0)
}

fn mean_strength(
    evidence: &[ReconstructionEvidence],
    ids: impl Iterator<Item = ReconstructionEvidenceId>,
) -> f32 {
    mean(ids.map(|id| evidence_strength(evidence, id)))
}

fn mean(values: impl Iterator<Item = f32>) -> f32 {
    let mut sum = 0.0_f64;
    let mut count = 0_u64;
    for value in values {
        sum += f64::from(if value.is_finite() { value } else { 0.0 });
        count += 1;
    }
    if count == 0 {
        0.0
    } else {
        (sum / count as f64) as f32
    }
}

fn take_id<T>(next: &mut u64, constructor: impl FnOnce(u64) -> T) -> T {
    let value = *next;
    *next = next.saturating_add(1);
    constructor(value)
}

fn finite_nonnegative(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn finite_positive_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        fallback
    }
}

fn finite_unit(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn unit(value: f32) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decomposition::{ComponentDecomposition, ComponentHypothesis};
    use crate::pitch::{
        CandidateMethod, PitchCandidate, PitchFrame, PitchSupport, PitchTrackPoint,
    };
    use crate::rhythm::{
        AnalysisStatus, BeatPhaseHypothesis, EventFamilyHypothesis, HitObservation,
        MedoidSampleReference, TempoHypothesis, TempoRelation,
    };

    fn hit(onset: usize, family: Option<usize>, strength: f32) -> HitObservation {
        HitObservation {
            span: SampleSpan {
                start: onset.saturating_sub(20),
                end: onset + 200,
            },
            onset_sample: onset,
            novelty_peak_sample: onset,
            peak_sample: onset,
            onset_seconds: onset as f64 / 48_000.0,
            duration_seconds: 220.0 / 48_000.0,
            novelty_strength: strength,
            threshold_excess: strength * 0.8,
            decay_seconds: 0.15,
            noisiness: 0.7,
            family,
            family_similarity: 0.9,
            ..HitObservation::default()
        }
    }

    fn rhythm() -> RhythmDeprojection {
        RhythmDeprojection {
            status: AnalysisStatus::Complete,
            sample_rate: 48_000,
            sample_frames: 96_000,
            hits: vec![hit(1_000, Some(7), 0.9), hit(25_100, Some(7), 0.8)],
            event_families: vec![EventFamilyHypothesis {
                id: 7,
                event_indices: vec![0, 1],
                medoid: MedoidSampleReference {
                    event_index: 0,
                    excerpt: SampleSpan {
                        start: 980,
                        end: 1_200,
                    },
                },
                mean_medoid_similarity: 0.9,
                minimum_medoid_similarity: 0.85,
                evidence: 0.88,
            }],
            tempo_hypotheses: vec![TempoHypothesis {
                rank: 0,
                bpm: 120.0,
                period_frames: 24_000.0,
                periodicity: 0.9,
                evidence: 0.82,
                relation: TempoRelation::Independent,
            }],
            beat_phase_hypotheses: vec![
                BeatPhaseHypothesis {
                    tempo_rank: 0,
                    bpm: 120.0,
                    phase_seconds: 0.0,
                    score: 0.86,
                    beat_samples: vec![0, 24_000, 48_000],
                },
                BeatPhaseHypothesis {
                    tempo_rank: 0,
                    bpm: 120.0,
                    phase_seconds: 0.25,
                    score: 0.62,
                    beat_samples: vec![12_000, 36_000],
                },
            ],
            ..RhythmDeprojection::default()
        }
    }

    fn pitch() -> PitchAnalysis {
        let support = PitchSupport {
            periodicity: 0.8,
            harmonicity: 0.7,
            spectral_prominence: 0.6,
            level_dbfs: -12.0,
        };
        let points = vec![
            PitchTrackPoint {
                offset_frames: 10_000,
                hz: Some(220.0),
                confidence: 0.9,
                support,
            },
            PitchTrackPoint {
                offset_frames: 10_256,
                hz: Some(222.0),
                confidence: 0.88,
                support,
            },
            PitchTrackPoint {
                offset_frames: 10_512,
                hz: Some(224.0),
                confidence: 0.86,
                support,
            },
        ];
        PitchAnalysis {
            sample_rate: 48_000,
            frame_size: 2_048,
            hop_size: 256,
            frames: points
                .iter()
                .map(|point| PitchFrame {
                    offset_frames: point.offset_frames,
                    rms: 0.2,
                    voiced: true,
                    voicing_confidence: point.confidence,
                    candidates: vec![
                        PitchCandidate {
                            hz: point.hz.unwrap(),
                            confidence: 0.9,
                            support,
                            method: CandidateMethod::Combined,
                        },
                        PitchCandidate {
                            hz: point.hz.unwrap() * 2.0,
                            confidence: 0.4,
                            support,
                            method: CandidateMethod::HarmonicSpectrum,
                        },
                    ],
                })
                .collect(),
            tracks: vec![PitchTrack {
                points,
                confidence: 0.87,
                voiced_points: 3,
                modulation: vec![ModulationEvidence::Vibrato {
                    start_offset_frames: 10_000,
                    end_offset_frames: 10_768,
                    rate_hz: 5.5,
                    extent_semitones: 0.25,
                    confidence: 0.74,
                }],
            }],
        }
    }

    fn inputs<'a>(
        rhythm: Option<&'a RhythmDeprojection>,
        pitch: Option<&'a PitchAnalysis>,
    ) -> ReconstructionInputs<'a> {
        ReconstructionInputs {
            sample_rate: 48_000,
            source_frame_count: 96_000,
            source_revision: Some("sha256:test"),
            rhythm,
            pitch,
            loom: None,
            components: None,
        }
    }

    #[test]
    fn anonymous_families_never_become_instrument_labels() {
        let rhythm = rhythm();
        let result = reconstruct(
            &inputs(Some(&rhythm), None),
            ReconstructionConfig::default(),
        )
        .unwrap();
        for proposal in &result.proposals {
            let family = proposal
                .tracks
                .iter()
                .find(|track| {
                    matches!(
                        track.kind,
                        ReconstructionTrackKind::AnonymousHitFamily {
                            upstream_family_id: 7
                        }
                    )
                })
                .unwrap();
            assert_eq!(family.label, "anonymous hit family 7");
            assert!(!family.label.contains("kick"));
            assert!(family
                .sample_slices
                .iter()
                .all(|slice| slice.anonymous_family_id == Some(7)));
        }
    }

    #[test]
    fn ranking_and_ids_are_deterministic() {
        let rhythm = rhythm();
        let pitch = pitch();
        let input = inputs(Some(&rhythm), Some(&pitch));
        let left = reconstruct(&input, ReconstructionConfig::default()).unwrap();
        let right = reconstruct(&input, ReconstructionConfig::default()).unwrap();
        assert_eq!(left, right);
        assert_eq!(left.validate(), Vec::<ReconstructionValidationIssue>::new());
        assert!(left
            .proposals
            .windows(2)
            .all(|pair| pair[0].score.total >= pair[1].score.total));
        assert!(left.proposals.iter().enumerate().all(
            |(index, proposal)| proposal.rank == index && proposal.id.get() == index as u64 + 1
        ));
    }

    #[test]
    fn grid_hypotheses_preserve_source_microtiming() {
        let rhythm = rhythm();
        let result = reconstruct(
            &inputs(Some(&rhythm), None),
            ReconstructionConfig::default(),
        )
        .unwrap();
        let grid = result
            .proposals
            .iter()
            .find(|proposal| proposal.tempo.is_some())
            .unwrap();
        let trigger = grid
            .tracks
            .iter()
            .flat_map(|track| &track.triggers)
            .next()
            .unwrap();
        assert!(trigger.musical_tick.is_some());
        let tempo = grid.tempo.unwrap();
        let reconstructed = tick_to_frame(trigger.musical_tick.unwrap(), tempo, 48_000)
            + trigger.micro_offset_frames;
        assert_eq!(reconstructed, trigger.source_onset_frame as i64);
    }

    #[test]
    fn pitch_and_modulation_alternatives_remain_unresolved() {
        let pitch = pitch();
        let result =
            reconstruct(&inputs(None, Some(&pitch)), ReconstructionConfig::default()).unwrap();
        let track = result.proposals[0]
            .tracks
            .iter()
            .find(|track| matches!(track.kind, ReconstructionTrackKind::PitchedStream { .. }))
            .unwrap();
        assert!(track.pitched_events[0].alternatives.len() >= 2);
        assert_eq!(
            track.pitched_events[0].selection,
            PitchChoiceSelection::Unresolved
        );
        assert_eq!(track.modulations[0].selected_implementation, None);
        assert_eq!(track.modulations[0].implementations.len(), 4);
    }

    #[test]
    fn every_proposal_has_an_explicit_residual_safety_layer() {
        let rhythm = rhythm();
        let result = reconstruct(
            &inputs(Some(&rhythm), None),
            ReconstructionConfig::default(),
        )
        .unwrap();
        for proposal in &result.proposals {
            let residual = proposal
                .tracks
                .iter()
                .find_map(|track| track.residual.as_ref())
                .unwrap();
            assert_eq!(residual.source.start, 0);
            assert_eq!(residual.source.end, 96_000);
            assert_eq!(
                residual.fallback_mode,
                ResidualRenderMode::OriginalSafetyLayer
            );
        }
    }

    #[test]
    fn latent_components_become_neutral_activity_tracks() {
        let decomposition = ComponentDecomposition {
            frequency_bins: 2,
            frames: 3,
            components: vec![ComponentHypothesis {
                spectral_template: vec![0.8, 0.2],
                activation: vec![0.0, 2.0, 1.0],
                energy_share: 0.5,
                spectral_distinctness: 0.7,
                confidence: 0.75,
            }],
            iterations_run: 10,
            reconstruction_rmse: 0.1,
            relative_error: 0.2,
            explained_energy: 0.8,
            confidence: 0.7,
            silent: false,
            gestures: None,
        };
        let mut input = inputs(None, None);
        input.components = Some(TimedComponents {
            decomposition: &decomposition,
            first_frame: 100,
            hop_frames: 256,
            analyzer_name: "test.nmf",
            analyzer_version: "1",
        });
        let result = reconstruct(&input, ReconstructionConfig::default()).unwrap();
        let component = result.proposals[0]
            .tracks
            .iter()
            .find(|track| matches!(track.kind, ReconstructionTrackKind::LatentComponent { .. }))
            .unwrap();
        assert_eq!(component.label, "latent spectral component 0");
        assert_eq!(component.automations[0].points[1].source_frame, 356);
        assert_eq!(component.automations[0].points[1].value, 1.0);
    }

    #[test]
    fn rejects_out_of_bounds_exact_slices() {
        let mut rhythm = rhythm();
        rhythm.hits[0].span.end = 200_000;
        assert!(matches!(
            reconstruct(
                &inputs(Some(&rhythm), None),
                ReconstructionConfig::default()
            ),
            Err(ReconstructionError::ObservationOutOfBounds { .. })
        ));
    }
}
