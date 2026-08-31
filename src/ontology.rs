//! Auditory Intermediate Representation (AIR).
//!
//! AIR describes *what can be heard and edited* without pretending to know
//! which instrument or physical source produced it.  Every perceptual object
//! may be backed by exact source-file sample spans, linked to evidence, placed
//! in competing hypothesis sets, and controlled through editable parameters.
//!
//! The module intentionally depends only on `std`.  IDs are explicit rather
//! than process-generated so an importer can preserve them across saves,
//! analysis passes, and collaborative edits.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

macro_rules! typed_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

typed_id!(SourceId);
typed_id!(SpanId);
typed_id!(ObjectId);
typed_id!(TransformId);
typed_id!(RelationId);
typed_id!(EvidenceId);
typed_id!(HypothesisId);
typed_id!(HypothesisSetId);
typed_id!(ParameterId);
typed_id!(AutomationId);
typed_id!(ModulationId);

/// Half-open range in frames of an original audio asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleRange {
    pub start: u64,
    pub end: u64,
}

impl SampleRange {
    pub fn new(start: u64, end: u64) -> Option<Self> {
        (start < end).then_some(Self { start, end })
    }

    pub const fn len(self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    pub const fn contains(self, frame: u64) -> bool {
        self.start <= frame && frame < self.end
    }
}

/// Signed arrangement time plus a non-negative duration, in project frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimelineRange {
    pub start: i64,
    pub duration: u64,
}

impl TimelineRange {
    pub const fn new(start: i64, duration: u64) -> Self {
        Self { start, duration }
    }

    pub fn end(self) -> Option<i64> {
        i64::try_from(self.duration)
            .ok()
            .and_then(|duration| self.start.checked_add(duration))
    }
}

/// A file, capture, or immutable decoded buffer from which claims originate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioSource {
    pub id: SourceId,
    pub uri: String,
    pub content_digest: Option<String>,
    pub sample_rate: u32,
    pub channels: u16,
    pub frame_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChannelSelection {
    All,
    Channel(u16),
    Mid,
    Side,
    /// Explicit channel indices; mixing weights live in an editable transform.
    Channels(Vec<u16>),
}

/// An exact, reusable citation into source audio.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceSpan {
    pub id: SpanId,
    pub source: SourceId,
    pub range: SampleRange,
    pub channels: ChannelSelection,
}

/// Positions source evidence relative to an object's start.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceAnchor {
    pub span: SpanId,
    pub object_offset_frames: i64,
    /// Relative evidential contribution, not an audio gain.
    pub weight: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Articulation {
    Impulsive,
    Sustained,
    Swell,
    Decay,
    Continuous,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GestureShape {
    Rising,
    Falling,
    Arch,
    Oscillating,
    Repeating,
    Irregular,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupingBasis {
    TemporalContinuity,
    PitchContinuity,
    SpectralContinuity,
    SpatialContinuity,
    Recurrence,
    Joint,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentDomain {
    Waveform,
    MagnitudeSpectrum,
    PowerSpectrum,
    Cepstrum,
    LearnedEmbedding,
    Other,
}

/// Perceptual/analytic kind.  None of these variants is an instrument label.
#[derive(Clone, Debug, PartialEq)]
pub enum ObjectKind {
    Event {
        articulation: Articulation,
        onset_strength: Option<f32>,
    },
    Gesture {
        shape: GestureShape,
        members: Vec<ObjectId>,
    },
    Stream {
        basis: GroupingBasis,
        members: Vec<ObjectId>,
    },
    /// A latent decomposition basis; `index` is local to its analysis run.
    Component {
        domain: ComponentDomain,
        index: u32,
        members: Vec<ObjectId>,
    },
}

impl ObjectKind {
    pub fn members(&self) -> &[ObjectId] {
        match self {
            Self::Event { .. } => &[],
            Self::Gesture { members, .. }
            | Self::Stream { members, .. }
            | Self::Component { members, .. } => members,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PitchReference {
    Absolute,
    /// Values are ratios relative to the first voiced point.
    RelativeRatio,
}

/// One sample of a pitch contour. `hz = None` records an explicitly unvoiced
/// interval rather than fabricating a fundamental.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PitchPoint {
    pub offset_frames: u64,
    pub hz: Option<f32>,
    pub confidence: f32,
}

/// A single pitch voice. Objects may contain multiple trajectories, allowing
/// chords, crossings, uncertain partial assignments, and pitch modulation.
#[derive(Clone, Debug, PartialEq)]
pub struct PitchTrajectory {
    pub label: Option<String>,
    pub reference: PitchReference,
    pub points: Vec<PitchPoint>,
    /// Optional evidence specific to this trajectory.
    pub evidence: Vec<EvidenceId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AuditoryObject {
    pub id: ObjectId,
    /// A user-facing neutral label such as "bright recurrence A".
    pub label: String,
    pub kind: ObjectKind,
    pub timeline: TimelineRange,
    pub source_anchors: Vec<SourceAnchor>,
    pub pitches: Vec<PitchTrajectory>,
    pub evidence: Vec<EvidenceId>,
    pub transform_chain: Vec<TransformId>,
    pub tags: BTreeSet<String>,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Linear,
    Decibels,
    Hertz,
    Semitones,
    Ratio,
    Percent,
    Frames,
    Seconds,
    Degrees,
    Radians,
    Normalized,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub minimum: f64,
    pub maximum: f64,
}

/// Semantic keys are deliberately production-oriented, not source-identity
/// oriented. `Custom` supports future analyzers without a schema migration.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ParameterKey {
    Gain,
    Wet,
    Bypass,
    TimeScale,
    PitchShift,
    Cutoff,
    Resonance,
    FilterGain,
    Pan,
    Width,
    Azimuth,
    Elevation,
    Distance,
    ModulationRate,
    ModulationDepth,
    Attack,
    Release,
    Custom(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ParameterOwner {
    Object(ObjectId),
    Transform(TransformId),
}

/// An editable scalar. Automation and modulation refer to this stable ID.
#[derive(Clone, Debug, PartialEq)]
pub struct Parameter {
    pub id: ParameterId,
    pub owner: ParameterOwner,
    pub key: ParameterKey,
    pub unit: Unit,
    pub bounds: Bounds,
    pub default: f64,
    pub value: f64,
    pub editable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interpolation {
    Hold,
    Linear,
    Smooth,
    Exponential,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutomationPoint {
    pub offset_frames: u64,
    pub value: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingMode {
    Replace,
    Add,
    Multiply,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Automation {
    pub id: AutomationId,
    pub parameter: ParameterId,
    pub interpolation: Interpolation,
    pub mode: BindingMode,
    pub points: Vec<AutomationPoint>,
    pub enabled: bool,
}

impl Automation {
    /// Evaluate the curve. Smooth currently uses smoothstep between points;
    /// exponential interpolation falls back to linear across zero/sign flips.
    pub fn value_at(&self, offset_frames: u64) -> Option<f64> {
        let first = *self.points.first()?;
        if offset_frames <= first.offset_frames {
            return Some(first.value);
        }
        let last = *self.points.last()?;
        if offset_frames >= last.offset_frames {
            return Some(last.value);
        }
        let right = self
            .points
            .partition_point(|point| point.offset_frames < offset_frames);
        let a = self.points[right - 1];
        let b = self.points[right];
        if self.interpolation == Interpolation::Hold {
            return Some(a.value);
        }
        let width = (b.offset_frames - a.offset_frames) as f64;
        let mut t = (offset_frames - a.offset_frames) as f64 / width;
        if self.interpolation == Interpolation::Smooth {
            t = t * t * (3.0 - 2.0 * t);
        }
        if self.interpolation == Interpolation::Exponential
            && a.value != 0.0
            && b.value != 0.0
            && a.value.signum() == b.value.signum()
        {
            Some(a.value * (b.value / a.value).powf(t))
        } else {
            Some(a.value + (b.value - a.value) * t)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterShape {
    LowPass,
    HighPass,
    BandPass,
    Notch,
    Peak,
    LowShelf,
    HighShelf,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SpectralBand {
    pub low_hz: f32,
    pub high_hz: f32,
    pub gain_db: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StretchAlgorithm {
    Resample,
    PhaseVocoder,
    Granular,
    TransientPreserving,
    Unspecified,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SpectralOperation {
    Tilt,
    Gate,
    Freeze,
    Blur,
    HarmonicShift,
    EnvelopeTransfer,
    Bands(Vec<SpectralBand>),
    Custom(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectKind {
    Delay,
    Reverberation,
    Convolution,
    Diffusion,
    Resonator,
    Distortion,
    Dynamics,
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TailExtent {
    None,
    FiniteFrames(u64),
    /// Tail ends once level remains below this threshold for the hold time.
    UntilBelow {
        threshold_db: f32,
        hold_frames: u64,
        maximum_frames: u64,
    },
    Unbounded,
}

/// A chain stage. Numeric controls are stable parameter references, so every
/// stage can be edited, automated, and modulated through the same mechanism.
#[derive(Clone, Debug, PartialEq)]
pub enum TransformKind {
    TimeScale {
        rate: ParameterId,
        preserve_pitch: bool,
        algorithm: StretchAlgorithm,
    },
    Gain {
        gain: ParameterId,
    },
    Filter {
        shape: FilterShape,
        cutoff: ParameterId,
        resonance: Option<ParameterId>,
        gain: Option<ParameterId>,
    },
    Spectral {
        operation: SpectralOperation,
        amount: ParameterId,
    },
    Spatial {
        pan: Option<ParameterId>,
        width: Option<ParameterId>,
        azimuth: Option<ParameterId>,
        elevation: Option<ParameterId>,
        distance: Option<ParameterId>,
    },
    Effect {
        effect: EffectKind,
        wet: ParameterId,
        tail: TailExtent,
    },
}

impl TransformKind {
    pub fn parameters(&self) -> Vec<ParameterId> {
        let mut result = Vec::new();
        match self {
            Self::TimeScale { rate, .. } => result.push(*rate),
            Self::Gain { gain } => result.push(*gain),
            Self::Filter {
                cutoff,
                resonance,
                gain,
                ..
            } => {
                result.push(*cutoff);
                result.extend(*resonance);
                result.extend(*gain);
            }
            Self::Spectral { amount, .. } => result.push(*amount),
            Self::Spatial {
                pan,
                width,
                azimuth,
                elevation,
                distance,
            } => {
                result.extend(*pan);
                result.extend(*width);
                result.extend(*azimuth);
                result.extend(*elevation);
                result.extend(*distance);
            }
            Self::Effect { wet, .. } => result.push(*wet),
        }
        result
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Transform {
    pub id: TransformId,
    pub owner: ObjectId,
    pub kind: TransformKind,
    pub bypass: Option<ParameterId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OscillatorShape {
    Sine,
    Triangle,
    SawUp,
    SawDown,
    Square,
    SampleAndHold,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectFeature {
    Amplitude,
    Fundamental,
    Brightness,
    OnsetEnvelope,
    SpectralFlux,
    Custom,
}

/// Modulators include ordinary LFOs, envelopes, parameter links, and
/// sidechain-like features extracted from another neutral auditory object.
#[derive(Clone, Debug, PartialEq)]
pub enum ModulationSource {
    Oscillator {
        shape: OscillatorShape,
        rate_hz: f32,
        phase_radians: f32,
    },
    Automation(AutomationId),
    Parameter(ParameterId),
    ObjectFeature {
        object: ObjectId,
        feature: ObjectFeature,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModulationMode {
    Add,
    Multiply,
    Replace,
    FrequencyModulation,
    AmplitudeModulation,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Modulation {
    pub id: ModulationId,
    pub target: ParameterId,
    pub source: ModulationSource,
    pub depth: f64,
    pub offset: f64,
    pub mode: ModulationMode,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum VariationDimension {
    Timing,
    Duration,
    Gain,
    Pitch,
    Timbre,
    Articulation,
    Modulation,
    SpatialPosition,
    Effects,
    Custom(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum RelationKind {
    Recurs {
        similarity: f32,
        alignment_offset_frames: i64,
    },
    VariantOf {
        similarity: f32,
        dimensions: Vec<VariationDimension>,
    },
    Precedes,
    Overlaps,
    Supports,
    Contradicts,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ObjectRelation {
    pub id: RelationId,
    pub from: ObjectId,
    pub to: ObjectId,
    pub kind: RelationKind,
    pub evidence: Vec<EvidenceId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Producer {
    Human {
        name: Option<String>,
    },
    Analyzer {
        name: String,
        version: String,
        configuration_digest: Option<String>,
    },
    Importer {
        format: String,
        version: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    pub producer: Producer,
    /// Unix milliseconds when known. Omit rather than inventing a timestamp.
    pub created_unix_ms: Option<u64>,
    pub source_revision: Option<String>,
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MeasurementValue {
    Scalar { value: f64, unit: Unit },
    Vector(Vec<f32>),
    Text(String),
    Boolean(bool),
}

#[derive(Clone, Debug, PartialEq)]
pub enum EvidenceKind {
    SourceMeasurement {
        spans: Vec<SpanId>,
        feature: String,
        value: MeasurementValue,
    },
    HumanAnnotation {
        text: String,
    },
    Derived {
        premises: Vec<EvidenceId>,
        method: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Evidence {
    pub id: EvidenceId,
    pub kind: EvidenceKind,
    /// Strength of this observation in `[0, 1]`, not posterior probability.
    pub strength: f32,
    pub provenance: Provenance,
}

/// Claim vocabulary is perceptual and structural. There is intentionally no
/// `InstrumentIdentity` or physical-source enum.
#[derive(Clone, Debug, PartialEq)]
pub enum HypothesisClaim {
    GroupsObjects(Vec<ObjectId>),
    SeparatesObjects(Vec<ObjectId>),
    Relation(RelationId),
    PitchTrack {
        object: ObjectId,
        trajectory_index: usize,
    },
    TransformApplies(TransformId),
    FreeformPerceptualDescription {
        objects: Vec<ObjectId>,
        description: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hypothesis {
    pub id: HypothesisId,
    pub label: String,
    pub claims: Vec<HypothesisClaim>,
    /// Relative support within a set; no probability calibration is implied.
    pub support: f32,
    pub evidence: Vec<EvidenceId>,
    pub provenance: Provenance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HypothesisSelection {
    Unresolved,
    Preferred(HypothesisId),
    UserAccepted(HypothesisId),
    UserRejectedAll,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HypothesisSet {
    pub id: HypothesisSetId,
    pub question: String,
    pub alternatives: Vec<HypothesisId>,
    pub selection: HypothesisSelection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InsertError {
    DuplicateId { entity: &'static str, id: u64 },
}

impl fmt::Display for InsertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateId { entity, id } => write!(formatter, "duplicate {entity} id {id}"),
        }
    }
}

impl std::error::Error for InsertError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationIssue {
    pub path: String,
    pub message: String,
}

impl ValidationIssue {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

/// Complete editable AIR document. `BTreeMap` makes traversal and eventual
/// serialization deterministic.
#[derive(Clone, Debug, PartialEq)]
pub struct AuditoryIr {
    pub schema_version: u32,
    pub sample_rate: u32,
    pub sources: BTreeMap<SourceId, AudioSource>,
    pub spans: BTreeMap<SpanId, SourceSpan>,
    pub objects: BTreeMap<ObjectId, AuditoryObject>,
    pub transforms: BTreeMap<TransformId, Transform>,
    pub parameters: BTreeMap<ParameterId, Parameter>,
    pub automations: BTreeMap<AutomationId, Automation>,
    pub modulations: BTreeMap<ModulationId, Modulation>,
    pub relations: BTreeMap<RelationId, ObjectRelation>,
    pub evidence: BTreeMap<EvidenceId, Evidence>,
    pub hypotheses: BTreeMap<HypothesisId, Hypothesis>,
    pub hypothesis_sets: BTreeMap<HypothesisSetId, HypothesisSet>,
}

impl AuditoryIr {
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;

    pub fn new(sample_rate: u32) -> Self {
        Self {
            schema_version: Self::CURRENT_SCHEMA_VERSION,
            sample_rate,
            sources: BTreeMap::new(),
            spans: BTreeMap::new(),
            objects: BTreeMap::new(),
            transforms: BTreeMap::new(),
            parameters: BTreeMap::new(),
            automations: BTreeMap::new(),
            modulations: BTreeMap::new(),
            relations: BTreeMap::new(),
            evidence: BTreeMap::new(),
            hypotheses: BTreeMap::new(),
            hypothesis_sets: BTreeMap::new(),
        }
    }

    pub fn insert_source(&mut self, value: AudioSource) -> Result<(), InsertError> {
        insert_unique(&mut self.sources, value.id, value, "source")
    }
    pub fn insert_span(&mut self, value: SourceSpan) -> Result<(), InsertError> {
        insert_unique(&mut self.spans, value.id, value, "span")
    }
    pub fn insert_object(&mut self, value: AuditoryObject) -> Result<(), InsertError> {
        insert_unique(&mut self.objects, value.id, value, "object")
    }
    pub fn insert_transform(&mut self, value: Transform) -> Result<(), InsertError> {
        insert_unique(&mut self.transforms, value.id, value, "transform")
    }
    pub fn insert_parameter(&mut self, value: Parameter) -> Result<(), InsertError> {
        insert_unique(&mut self.parameters, value.id, value, "parameter")
    }
    pub fn insert_automation(&mut self, value: Automation) -> Result<(), InsertError> {
        insert_unique(&mut self.automations, value.id, value, "automation")
    }
    pub fn insert_modulation(&mut self, value: Modulation) -> Result<(), InsertError> {
        insert_unique(&mut self.modulations, value.id, value, "modulation")
    }
    pub fn insert_relation(&mut self, value: ObjectRelation) -> Result<(), InsertError> {
        insert_unique(&mut self.relations, value.id, value, "relation")
    }
    pub fn insert_evidence(&mut self, value: Evidence) -> Result<(), InsertError> {
        insert_unique(&mut self.evidence, value.id, value, "evidence")
    }
    pub fn insert_hypothesis(&mut self, value: Hypothesis) -> Result<(), InsertError> {
        insert_unique(&mut self.hypotheses, value.id, value, "hypothesis")
    }
    pub fn insert_hypothesis_set(&mut self, value: HypothesisSet) -> Result<(), InsertError> {
        insert_unique(&mut self.hypothesis_sets, value.id, value, "hypothesis set")
    }

    /// Resolve the effective unmodulated value of a parameter at an object-
    /// local frame. Multiple enabled automation lanes apply in ID order.
    pub fn parameter_value_at(&self, id: ParameterId, frame: u64) -> Option<f64> {
        let parameter = self.parameters.get(&id)?;
        let mut value = parameter.value;
        for automation in self
            .automations
            .values()
            .filter(|automation| automation.enabled && automation.parameter == id)
        {
            let automated = automation.value_at(frame)?;
            value = match automation.mode {
                BindingMode::Replace => automated,
                BindingMode::Add => value + automated,
                BindingMode::Multiply => value * automated,
            };
        }
        Some(value.clamp(parameter.bounds.minimum, parameter.bounds.maximum))
    }

    /// Returns every validation issue so editors can repair a whole imported
    /// document in one pass. Empty means the graph is internally consistent.
    pub fn validate(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        if self.sample_rate == 0 {
            issues.push(ValidationIssue::new("sample_rate", "must be non-zero"));
        }
        if self.schema_version == 0 || self.schema_version > Self::CURRENT_SCHEMA_VERSION {
            issues.push(ValidationIssue::new(
                "schema_version",
                "is zero or newer than this implementation",
            ));
        }

        for (id, source) in &self.sources {
            let path = format!("sources[{id}]");
            if source.id != *id {
                issues.push(ValidationIssue::new(
                    &path,
                    "map key and embedded id differ",
                ));
            }
            if source.sample_rate == 0 || source.channels == 0 {
                issues.push(ValidationIssue::new(
                    &path,
                    "sample rate and channel count must be non-zero",
                ));
            }
        }
        for (id, span) in &self.spans {
            let path = format!("spans[{id}]");
            if span.id != *id {
                issues.push(ValidationIssue::new(
                    &path,
                    "map key and embedded id differ",
                ));
            }
            let Some(source) = self.sources.get(&span.source) else {
                issues.push(ValidationIssue::new(&path, "references a missing source"));
                continue;
            };
            if span.range.start >= span.range.end || span.range.end > source.frame_count {
                issues.push(ValidationIssue::new(
                    &path,
                    "sample range is empty or out of bounds",
                ));
            }
            let channels: Vec<u16> = match &span.channels {
                ChannelSelection::Channel(channel) => vec![*channel],
                ChannelSelection::Channels(channels) => channels.clone(),
                ChannelSelection::Mid | ChannelSelection::Side if source.channels < 2 => {
                    issues.push(ValidationIssue::new(
                        &path,
                        "mid/side requires at least two channels",
                    ));
                    Vec::new()
                }
                _ => Vec::new(),
            };
            if channels.is_empty() && matches!(&span.channels, ChannelSelection::Channels(_)) {
                issues.push(ValidationIssue::new(
                    &path,
                    "explicit channel list is empty",
                ));
            }
            if channels.iter().any(|channel| *channel >= source.channels) {
                issues.push(ValidationIssue::new(
                    &path,
                    "channel index is out of bounds",
                ));
            }
        }

        for (id, object) in &self.objects {
            let path = format!("objects[{id}]");
            if object.id != *id {
                issues.push(ValidationIssue::new(
                    &path,
                    "map key and embedded id differ",
                ));
            }
            if object.timeline.end().is_none() {
                issues.push(ValidationIssue::new(&path, "timeline overflows i64"));
            }
            for anchor in &object.source_anchors {
                if !self.spans.contains_key(&anchor.span) {
                    issues.push(ValidationIssue::new(
                        &path,
                        "source anchor references a missing span",
                    ));
                }
                if !finite_unit(anchor.weight) {
                    issues.push(ValidationIssue::new(
                        &path,
                        "source anchor weight must be in [0, 1]",
                    ));
                }
            }
            for member in object.kind.members() {
                if member == id {
                    issues.push(ValidationIssue::new(&path, "object cannot contain itself"));
                } else if !self.objects.contains_key(member) {
                    issues.push(ValidationIssue::new(
                        &path,
                        "member references a missing object",
                    ));
                }
            }
            if let ObjectKind::Event {
                onset_strength: Some(value),
                ..
            } = object.kind
            {
                if !finite_unit(value) {
                    issues.push(ValidationIssue::new(
                        &path,
                        "onset strength must be in [0, 1]",
                    ));
                }
            }
            for pitch in &object.pitches {
                validate_pitch(
                    pitch,
                    object.timeline.duration,
                    &path,
                    &self.evidence,
                    &mut issues,
                );
            }
            validate_refs(
                &object.evidence,
                &self.evidence,
                &path,
                "evidence",
                &mut issues,
            );
            let mut seen = BTreeSet::new();
            for transform in &object.transform_chain {
                if !seen.insert(*transform) {
                    issues.push(ValidationIssue::new(
                        &path,
                        "transform chain contains a duplicate",
                    ));
                }
                match self.transforms.get(transform) {
                    Some(value) if value.owner != *id => issues.push(ValidationIssue::new(
                        &path,
                        "transform chain contains a transform owned by another object",
                    )),
                    None => issues.push(ValidationIssue::new(
                        &path,
                        "transform chain references a missing transform",
                    )),
                    _ => {}
                }
            }
        }
        validate_membership_cycles(&self.objects, &mut issues);

        for (id, parameter) in &self.parameters {
            let path = format!("parameters[{id}]");
            if parameter.id != *id {
                issues.push(ValidationIssue::new(
                    &path,
                    "map key and embedded id differ",
                ));
            }
            let owner_exists = match parameter.owner {
                ParameterOwner::Object(id) => self.objects.contains_key(&id),
                ParameterOwner::Transform(id) => self.transforms.contains_key(&id),
            };
            if !owner_exists {
                issues.push(ValidationIssue::new(&path, "references a missing owner"));
            }
            let bounds = parameter.bounds;
            if !bounds.minimum.is_finite()
                || !bounds.maximum.is_finite()
                || bounds.minimum > bounds.maximum
                || !parameter.default.is_finite()
                || !parameter.value.is_finite()
                || !(bounds.minimum..=bounds.maximum).contains(&parameter.default)
                || !(bounds.minimum..=bounds.maximum).contains(&parameter.value)
            {
                issues.push(ValidationIssue::new(
                    &path,
                    "bounds/default/value are invalid",
                ));
            }
        }
        for (id, transform) in &self.transforms {
            let path = format!("transforms[{id}]");
            if transform.id != *id || !self.objects.contains_key(&transform.owner) {
                issues.push(ValidationIssue::new(&path, "id mismatch or missing owner"));
            }
            for parameter in transform
                .kind
                .parameters()
                .into_iter()
                .chain(transform.bypass)
            {
                match self.parameters.get(&parameter) {
                    Some(value) if value.owner != ParameterOwner::Transform(*id) => issues.push(
                        ValidationIssue::new(&path, "control parameter is owned by another entity"),
                    ),
                    None => issues.push(ValidationIssue::new(
                        &path,
                        "references a missing parameter",
                    )),
                    _ => {}
                }
            }
            validate_transform_values(&transform.kind, &path, &mut issues);
        }
        for (id, automation) in &self.automations {
            let path = format!("automations[{id}]");
            if automation.id != *id || !self.parameters.contains_key(&automation.parameter) {
                issues.push(ValidationIssue::new(
                    &path,
                    "id mismatch or missing parameter",
                ));
            }
            if automation.points.is_empty()
                || automation
                    .points
                    .iter()
                    .any(|point| !point.value.is_finite())
                || automation
                    .points
                    .windows(2)
                    .any(|pair| pair[0].offset_frames >= pair[1].offset_frames)
            {
                issues.push(ValidationIssue::new(
                    &path,
                    "curve must have finite values and strictly increasing points",
                ));
            }
        }
        for (id, modulation) in &self.modulations {
            let path = format!("modulations[{id}]");
            if modulation.id != *id || !self.parameters.contains_key(&modulation.target) {
                issues.push(ValidationIssue::new(
                    &path,
                    "id mismatch or missing target parameter",
                ));
            }
            if !modulation.depth.is_finite() || !modulation.offset.is_finite() {
                issues.push(ValidationIssue::new(
                    &path,
                    "depth and offset must be finite",
                ));
            }
            match modulation.source {
                ModulationSource::Oscillator {
                    rate_hz,
                    phase_radians,
                    ..
                } => {
                    if !rate_hz.is_finite() || rate_hz < 0.0 || !phase_radians.is_finite() {
                        issues.push(ValidationIssue::new(&path, "oscillator values are invalid"));
                    }
                }
                ModulationSource::Automation(id) if !self.automations.contains_key(&id) => {
                    issues.push(ValidationIssue::new(
                        &path,
                        "references a missing automation",
                    ));
                }
                ModulationSource::Parameter(id) if !self.parameters.contains_key(&id) => {
                    issues.push(ValidationIssue::new(
                        &path,
                        "references a missing parameter",
                    ));
                }
                ModulationSource::ObjectFeature { object, .. }
                    if !self.objects.contains_key(&object) =>
                {
                    issues.push(ValidationIssue::new(
                        &path,
                        "references a missing sidechain object",
                    ));
                }
                _ => {}
            }
        }

        for (id, relation) in &self.relations {
            let path = format!("relations[{id}]");
            if relation.id != *id
                || !self.objects.contains_key(&relation.from)
                || !self.objects.contains_key(&relation.to)
            {
                issues.push(ValidationIssue::new(
                    &path,
                    "id mismatch or missing endpoint",
                ));
            }
            match &relation.kind {
                RelationKind::Recurs { similarity, .. }
                | RelationKind::VariantOf { similarity, .. }
                    if !finite_unit(*similarity) =>
                {
                    issues.push(ValidationIssue::new(&path, "similarity must be in [0, 1]"));
                }
                _ => {}
            }
            validate_refs(
                &relation.evidence,
                &self.evidence,
                &path,
                "evidence",
                &mut issues,
            );
        }

        for (id, evidence) in &self.evidence {
            let path = format!("evidence[{id}]");
            if evidence.id != *id || !finite_unit(evidence.strength) {
                issues.push(ValidationIssue::new(
                    &path,
                    "id mismatch or strength outside [0, 1]",
                ));
            }
            match &evidence.kind {
                EvidenceKind::SourceMeasurement { spans, value, .. } => {
                    validate_refs(spans, &self.spans, &path, "span", &mut issues);
                    if !measurement_is_finite(value) {
                        issues.push(ValidationIssue::new(
                            &path,
                            "measurement contains a non-finite value",
                        ));
                    }
                }
                EvidenceKind::Derived { premises, .. } => {
                    validate_refs(premises, &self.evidence, &path, "premise", &mut issues);
                    if premises.contains(id) {
                        issues.push(ValidationIssue::new(
                            &path,
                            "evidence cannot derive from itself",
                        ));
                    }
                }
                EvidenceKind::HumanAnnotation { .. } => {}
            }
        }

        for (id, hypothesis) in &self.hypotheses {
            let path = format!("hypotheses[{id}]");
            if hypothesis.id != *id || !finite_unit(hypothesis.support) {
                issues.push(ValidationIssue::new(
                    &path,
                    "id mismatch or support outside [0, 1]",
                ));
            }
            validate_refs(
                &hypothesis.evidence,
                &self.evidence,
                &path,
                "evidence",
                &mut issues,
            );
            for claim in &hypothesis.claims {
                validate_claim(claim, self, &path, &mut issues);
            }
        }
        for (id, set) in &self.hypothesis_sets {
            let path = format!("hypothesis_sets[{id}]");
            if set.id != *id || set.alternatives.len() < 2 {
                issues.push(ValidationIssue::new(
                    &path,
                    "id mismatch or fewer than two alternatives",
                ));
            }
            validate_refs(
                &set.alternatives,
                &self.hypotheses,
                &path,
                "hypothesis",
                &mut issues,
            );
            let selected = match set.selection {
                HypothesisSelection::Preferred(id) | HypothesisSelection::UserAccepted(id) => {
                    Some(id)
                }
                _ => None,
            };
            if selected.is_some_and(|id| !set.alternatives.contains(&id)) {
                issues.push(ValidationIssue::new(
                    &path,
                    "selection is not one of the alternatives",
                ));
            }
        }
        issues
    }
}

fn insert_unique<K: Ord + Copy + IntoId, V>(
    map: &mut BTreeMap<K, V>,
    key: K,
    value: V,
    entity: &'static str,
) -> Result<(), InsertError> {
    if map.contains_key(&key) {
        return Err(InsertError::DuplicateId {
            entity,
            id: key.into_id(),
        });
    }
    map.insert(key, value);
    Ok(())
}

trait IntoId {
    fn into_id(self) -> u64;
}

macro_rules! impl_into_id {
    ($($name:ident),+ $(,)?) => {$(
        impl IntoId for $name {
            fn into_id(self) -> u64 { self.get() }
        }
    )+};
}

impl_into_id!(
    SourceId,
    SpanId,
    ObjectId,
    TransformId,
    RelationId,
    EvidenceId,
    HypothesisId,
    HypothesisSetId,
    ParameterId,
    AutomationId,
    ModulationId,
);

fn finite_unit(value: f32) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn measurement_is_finite(value: &MeasurementValue) -> bool {
    match value {
        MeasurementValue::Scalar { value, .. } => value.is_finite(),
        MeasurementValue::Vector(values) => values.iter().all(|value| value.is_finite()),
        MeasurementValue::Text(_) | MeasurementValue::Boolean(_) => true,
    }
}

fn validate_refs<K: Ord + fmt::Display, V>(
    references: &[K],
    values: &BTreeMap<K, V>,
    path: &str,
    kind: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    for reference in references {
        if !values.contains_key(reference) {
            issues.push(ValidationIssue::new(
                path,
                format!("references missing {kind} {reference}"),
            ));
        }
    }
}

fn validate_pitch(
    pitch: &PitchTrajectory,
    duration: u64,
    path: &str,
    evidence: &BTreeMap<EvidenceId, Evidence>,
    issues: &mut Vec<ValidationIssue>,
) {
    if pitch.points.is_empty()
        || pitch
            .points
            .windows(2)
            .any(|pair| pair[0].offset_frames >= pair[1].offset_frames)
    {
        issues.push(ValidationIssue::new(
            path,
            "pitch points must be nonempty and strictly ordered",
        ));
    }
    for point in &pitch.points {
        if point.offset_frames > duration
            || !finite_unit(point.confidence)
            || point.hz.is_some_and(|hz| !hz.is_finite() || hz <= 0.0)
        {
            issues.push(ValidationIssue::new(
                path,
                "pitch point is outside time/frequency/confidence bounds",
            ));
        }
    }
    validate_refs(&pitch.evidence, evidence, path, "pitch evidence", issues);
}

fn validate_transform_values(kind: &TransformKind, path: &str, issues: &mut Vec<ValidationIssue>) {
    match kind {
        TransformKind::Spectral {
            operation: SpectralOperation::Bands(bands),
            ..
        } => {
            for band in bands {
                if !band.low_hz.is_finite()
                    || !band.high_hz.is_finite()
                    || !band.gain_db.is_finite()
                    || band.low_hz < 0.0
                    || band.low_hz >= band.high_hz
                {
                    issues.push(ValidationIssue::new(path, "spectral band is invalid"));
                }
            }
        }
        TransformKind::Effect {
            tail:
                TailExtent::UntilBelow {
                    threshold_db,
                    hold_frames,
                    maximum_frames,
                },
            ..
        } if !threshold_db.is_finite() || *hold_frames > *maximum_frames => {
            issues.push(ValidationIssue::new(
                path,
                "effect tail threshold/limits are invalid",
            ));
        }
        _ => {}
    }
}

fn validate_claim(
    claim: &HypothesisClaim,
    ir: &AuditoryIr,
    path: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    match claim {
        HypothesisClaim::GroupsObjects(objects)
        | HypothesisClaim::SeparatesObjects(objects)
        | HypothesisClaim::FreeformPerceptualDescription { objects, .. } => {
            validate_refs(objects, &ir.objects, path, "object", issues)
        }
        HypothesisClaim::Relation(id) if !ir.relations.contains_key(id) => {
            issues.push(ValidationIssue::new(
                path,
                "claim references a missing relation",
            ));
        }
        HypothesisClaim::PitchTrack {
            object,
            trajectory_index,
        } => match ir.objects.get(object) {
            Some(value) if *trajectory_index >= value.pitches.len() => {
                issues.push(ValidationIssue::new(
                    path,
                    "claim references a missing pitch trajectory",
                ));
            }
            None => issues.push(ValidationIssue::new(
                path,
                "claim references a missing object",
            )),
            _ => {}
        },
        HypothesisClaim::TransformApplies(id) if !ir.transforms.contains_key(id) => {
            issues.push(ValidationIssue::new(
                path,
                "claim references a missing transform",
            ));
        }
        _ => {}
    }
}

fn validate_membership_cycles(
    objects: &BTreeMap<ObjectId, AuditoryObject>,
    issues: &mut Vec<ValidationIssue>,
) {
    fn visit(
        id: ObjectId,
        objects: &BTreeMap<ObjectId, AuditoryObject>,
        visiting: &mut BTreeSet<ObjectId>,
        visited: &mut BTreeSet<ObjectId>,
    ) -> bool {
        if visited.contains(&id) {
            return false;
        }
        if !visiting.insert(id) {
            return true;
        }
        let cyclic = objects.get(&id).is_some_and(|object| {
            object.kind.members().iter().any(|member| {
                objects.contains_key(member) && visit(*member, objects, visiting, visited)
            })
        });
        visiting.remove(&id);
        visited.insert(id);
        cyclic
    }
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for id in objects.keys().copied() {
        if visit(id, objects, &mut visiting, &mut visited) {
            issues.push(ValidationIssue::new(
                format!("objects[{id}]"),
                "object membership contains a cycle",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provenance() -> Provenance {
        Provenance {
            producer: Producer::Analyzer {
                name: "test-analyzer".into(),
                version: "1.0".into(),
                configuration_digest: Some("cfg-1".into()),
            },
            created_unix_ms: Some(123),
            source_revision: Some("audio-sha".into()),
            note: None,
        }
    }

    fn event(id: u64, span: u64, start: i64, pitch_hz: f32) -> AuditoryObject {
        AuditoryObject {
            id: ObjectId::new(id),
            label: format!("event {id}"),
            kind: ObjectKind::Event {
                articulation: Articulation::Impulsive,
                onset_strength: Some(0.8),
            },
            timeline: TimelineRange::new(start, 1_000),
            source_anchors: vec![SourceAnchor {
                span: SpanId::new(span),
                object_offset_frames: 0,
                weight: 1.0,
            }],
            pitches: vec![PitchTrajectory {
                label: None,
                reference: PitchReference::Absolute,
                points: vec![
                    PitchPoint {
                        offset_frames: 0,
                        hz: Some(pitch_hz),
                        confidence: 0.9,
                    },
                    PitchPoint {
                        offset_frames: 1_000,
                        hz: Some(pitch_hz * 1.01),
                        confidence: 0.8,
                    },
                ],
                evidence: vec![EvidenceId::new(1)],
            }],
            evidence: vec![EvidenceId::new(1)],
            transform_chain: Vec::new(),
            tags: BTreeSet::new(),
            enabled: true,
        }
    }

    fn valid_ir() -> AuditoryIr {
        let mut ir = AuditoryIr::new(48_000);
        ir.insert_source(AudioSource {
            id: SourceId::new(1),
            uri: "file:///song.flac".into(),
            content_digest: Some("abc".into()),
            sample_rate: 48_000,
            channels: 2,
            frame_count: 200_000,
        })
        .unwrap();
        for (id, start) in [(1, 10_000), (2, 30_000)] {
            ir.insert_span(SourceSpan {
                id: SpanId::new(id),
                source: SourceId::new(1),
                range: SampleRange::new(start, start + 1_000).unwrap(),
                channels: ChannelSelection::All,
            })
            .unwrap();
        }
        ir.insert_evidence(Evidence {
            id: EvidenceId::new(1),
            kind: EvidenceKind::SourceMeasurement {
                spans: vec![SpanId::new(1), SpanId::new(2)],
                feature: "fundamental candidates".into(),
                value: MeasurementValue::Vector(vec![220.0, 330.0]),
            },
            strength: 0.85,
            provenance: provenance(),
        })
        .unwrap();
        ir.insert_object(event(1, 1, 10_000, 220.0)).unwrap();
        ir.insert_object(event(2, 2, 30_000, 330.0)).unwrap();

        let transform = TransformId::new(1);
        let gain = ParameterId::new(1);
        let rate = ParameterId::new(2);
        ir.insert_transform(Transform {
            id: transform,
            owner: ObjectId::new(1),
            kind: TransformKind::TimeScale {
                rate,
                preserve_pitch: true,
                algorithm: StretchAlgorithm::TransientPreserving,
            },
            bypass: None,
        })
        .unwrap();
        ir.insert_parameter(Parameter {
            id: rate,
            owner: ParameterOwner::Transform(transform),
            key: ParameterKey::TimeScale,
            unit: Unit::Ratio,
            bounds: Bounds {
                minimum: 0.25,
                maximum: 4.0,
            },
            default: 1.0,
            value: 1.0,
            editable: true,
        })
        .unwrap();
        ir.insert_parameter(Parameter {
            id: gain,
            owner: ParameterOwner::Object(ObjectId::new(1)),
            key: ParameterKey::Gain,
            unit: Unit::Linear,
            bounds: Bounds {
                minimum: 0.0,
                maximum: 2.0,
            },
            default: 1.0,
            value: 1.0,
            editable: true,
        })
        .unwrap();
        ir.objects
            .get_mut(&ObjectId::new(1))
            .unwrap()
            .transform_chain
            .push(transform);
        ir.insert_automation(Automation {
            id: AutomationId::new(1),
            parameter: gain,
            interpolation: Interpolation::Linear,
            mode: BindingMode::Replace,
            points: vec![
                AutomationPoint {
                    offset_frames: 0,
                    value: 0.25,
                },
                AutomationPoint {
                    offset_frames: 1_000,
                    value: 1.25,
                },
            ],
            enabled: true,
        })
        .unwrap();
        ir.insert_modulation(Modulation {
            id: ModulationId::new(1),
            target: gain,
            source: ModulationSource::ObjectFeature {
                object: ObjectId::new(2),
                feature: ObjectFeature::Amplitude,
            },
            depth: 0.5,
            offset: 0.0,
            mode: ModulationMode::AmplitudeModulation,
            enabled: true,
        })
        .unwrap();
        ir.insert_relation(ObjectRelation {
            id: RelationId::new(1),
            from: ObjectId::new(2),
            to: ObjectId::new(1),
            kind: RelationKind::VariantOf {
                similarity: 0.82,
                dimensions: vec![VariationDimension::Pitch, VariationDimension::Gain],
            },
            evidence: vec![EvidenceId::new(1)],
        })
        .unwrap();
        for (id, claim, support) in [
            (1, HypothesisClaim::Relation(RelationId::new(1)), 0.75),
            (
                2,
                HypothesisClaim::SeparatesObjects(vec![ObjectId::new(1), ObjectId::new(2)]),
                0.25,
            ),
        ] {
            ir.insert_hypothesis(Hypothesis {
                id: HypothesisId::new(id),
                label: format!("alternative {id}"),
                claims: vec![claim],
                support,
                evidence: vec![EvidenceId::new(1)],
                provenance: provenance(),
            })
            .unwrap();
        }
        ir.insert_hypothesis_set(HypothesisSet {
            id: HypothesisSetId::new(1),
            question: "Are these perceptual variants?".into(),
            alternatives: vec![HypothesisId::new(1), HypothesisId::new(2)],
            selection: HypothesisSelection::Preferred(HypothesisId::new(1)),
        })
        .unwrap();
        ir
    }

    #[test]
    fn complete_multpitch_modulated_graph_validates() {
        let mut ir = valid_ir();
        ir.objects
            .get_mut(&ObjectId::new(1))
            .unwrap()
            .pitches
            .push(PitchTrajectory {
                label: Some("upper simultaneous candidate".into()),
                reference: PitchReference::Absolute,
                points: vec![PitchPoint {
                    offset_frames: 0,
                    hz: Some(440.0),
                    confidence: 0.55,
                }],
                evidence: vec![EvidenceId::new(1)],
            });
        assert_eq!(ir.validate(), Vec::<ValidationIssue>::new());
    }

    #[test]
    fn typed_ids_do_not_alias_and_preserve_values() {
        let source = SourceId::new(42);
        let object = ObjectId::new(42);
        assert_eq!(source.get(), object.get());
        assert_eq!(source.to_string(), "42");
        // The following intentionally cannot compile: source == object.
    }

    #[test]
    fn duplicate_ids_are_rejected_without_overwrite() {
        let mut ir = valid_ir();
        let old = ir.objects[&ObjectId::new(1)].clone();
        let error = ir.insert_object(event(1, 2, 20, 999.0)).unwrap_err();
        assert_eq!(
            error,
            InsertError::DuplicateId {
                entity: "object",
                id: 1
            }
        );
        assert_eq!(ir.objects[&ObjectId::new(1)], old);
    }

    #[test]
    fn automation_interpolates_and_binding_clamps() {
        let ir = valid_ir();
        assert_eq!(
            ir.automations[&AutomationId::new(1)].value_at(500),
            Some(0.75)
        );
        assert_eq!(ir.parameter_value_at(ParameterId::new(1), 500), Some(0.75));
        assert_eq!(
            ir.parameter_value_at(ParameterId::new(1), 5_000),
            Some(1.25)
        );
    }

    #[test]
    fn exponential_and_hold_automation_have_defined_edges() {
        let points = vec![
            AutomationPoint {
                offset_frames: 10,
                value: 1.0,
            },
            AutomationPoint {
                offset_frames: 20,
                value: 4.0,
            },
        ];
        let mut automation = Automation {
            id: AutomationId::new(9),
            parameter: ParameterId::new(9),
            interpolation: Interpolation::Exponential,
            mode: BindingMode::Replace,
            points,
            enabled: true,
        };
        assert_eq!(automation.value_at(0), Some(1.0));
        assert!((automation.value_at(15).unwrap() - 2.0).abs() < 1e-9);
        assert_eq!(automation.value_at(99), Some(4.0));
        automation.interpolation = Interpolation::Hold;
        assert_eq!(automation.value_at(15), Some(1.0));
    }

    #[test]
    fn rejects_broken_source_pitch_and_sidechain_references() {
        let mut ir = valid_ir();
        ir.spans.get_mut(&SpanId::new(1)).unwrap().range.end = 999_999;
        let object = ir.objects.get_mut(&ObjectId::new(1)).unwrap();
        object.source_anchors[0].span = SpanId::new(999);
        object.pitches[0].points[0].hz = Some(-1.0);
        if let ModulationSource::ObjectFeature { object, .. } = &mut ir
            .modulations
            .get_mut(&ModulationId::new(1))
            .unwrap()
            .source
        {
            *object = ObjectId::new(999);
        }
        let issues = ir.validate();
        assert!(issues
            .iter()
            .any(|issue| issue.message.contains("out of bounds")));
        assert!(issues
            .iter()
            .any(|issue| issue.message.contains("missing span")));
        assert!(issues
            .iter()
            .any(|issue| issue.message.contains("pitch point")));
        assert!(issues
            .iter()
            .any(|issue| issue.message.contains("sidechain")));
    }

    #[test]
    fn detects_membership_cycles() {
        let mut ir = valid_ir();
        ir.objects.get_mut(&ObjectId::new(1)).unwrap().kind = ObjectKind::Gesture {
            shape: GestureShape::Repeating,
            members: vec![ObjectId::new(2)],
        };
        ir.objects.get_mut(&ObjectId::new(2)).unwrap().kind = ObjectKind::Stream {
            basis: GroupingBasis::Recurrence,
            members: vec![ObjectId::new(1)],
        };
        assert!(ir
            .validate()
            .iter()
            .any(|issue| issue.message.contains("cycle")));
    }

    #[test]
    fn transform_controls_must_be_owned_by_the_transform() {
        let mut ir = valid_ir();
        ir.parameters.get_mut(&ParameterId::new(2)).unwrap().owner =
            ParameterOwner::Object(ObjectId::new(1));
        assert!(ir
            .validate()
            .iter()
            .any(|issue| issue.message.contains("owned by another")));
    }

    #[test]
    fn hypothesis_selection_must_name_an_alternative() {
        let mut ir = valid_ir();
        ir.hypothesis_sets
            .get_mut(&HypothesisSetId::new(1))
            .unwrap()
            .selection = HypothesisSelection::UserAccepted(HypothesisId::new(99));
        assert!(ir
            .validate()
            .iter()
            .any(|issue| issue.message.contains("not one of")));
    }

    #[test]
    fn invalid_automation_and_parameter_values_are_reported() {
        let mut ir = valid_ir();
        ir.parameters.get_mut(&ParameterId::new(1)).unwrap().value = 3.0;
        ir.automations
            .get_mut(&AutomationId::new(1))
            .unwrap()
            .points[1]
            .offset_frames = 0;
        let issues = ir.validate();
        assert!(issues
            .iter()
            .any(|issue| issue.message.contains("bounds/default/value")));
        assert!(issues
            .iter()
            .any(|issue| issue.message.contains("strictly increasing")));
    }

    #[test]
    fn sample_and_timeline_ranges_have_precise_edges() {
        assert!(SampleRange::new(4, 4).is_none());
        let range = SampleRange::new(4, 7).unwrap();
        assert_eq!(range.len(), 3);
        assert!(range.contains(4));
        assert!(!range.contains(7));
        assert_eq!(TimelineRange::new(-10, 20).end(), Some(10));
        assert_eq!(TimelineRange::new(i64::MAX, 1).end(), None);
    }
}
