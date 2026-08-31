//! Lossless, versioned JSON codecs for audec's constructive project domains.
//!
//! `project_io` owns the portable envelope.  This module owns the payloads
//! named by that envelope and deliberately rebuilds models through their
//! checked public APIs.  Unsupported versions, allocator-incompatible IDs,
//! and unavailable AIR references are errors: loading never renumbers or
//! silently discards authored data.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::arrangement::ArrangementState;
use crate::assets::{
    AbsolutePath, AssetAvailability, AssetFrameRange, AssetId, AssetLocation, AssetOrigin,
    AssetProvenance, AssetRegistration, AssetRegistry, AssetUsageOwner, ContentFingerprint,
    ContentHashAlgorithm, ContentId, DecodedAudioMetadata, ProjectRelativePath, RelinkBasis,
    SampleFrames,
};
use crate::automation::{
    AutomationCommand, AutomationGraph, AutomationLane, AutomationLaneId, AutomationPoint,
    AutomationPointId, BindingMode, ClipParameter, DecompositionTarget, Extrapolation, LaneChange,
    LensParameter, MixerTarget, ParameterAddress, ParameterDescriptor, ParameterUnit, SegmentShape,
    SmoothingPolicy, TimeDomain, TimePosition, ValueMapping,
};
use crate::daw_project::{
    AirBindings, LegacyIdentityArchive, MixerBindings, ProjectBindings, ProjectState,
};
use crate::mixer::{BusId, BusKind, MixerGraph, PluginDescriptor, SendTap};
use crate::ontology::{self, AuditoryIr};
use crate::project_io::{DiagnosticLevel, ProjectFile, ProjectIoDiagnostic};
use crate::sequencer::{
    Articulation, BeatDuration, BeatTime, ExpressionPoint, NoteEvent, NoteId, NotePattern,
    NotePitch, PatternClip, PatternContent, PatternDefinition, PatternId, PerNoteExpression,
    SampleAssetId, Sequencer, SequencerCommand, StepEvent, StepLane, StepLaneId, StepPattern,
    Tempo, TempoMap, TimeSignature, TriggerTarget,
};
use crate::session;

pub const CONSTRUCTIVE_CODEC_VERSION: u32 = 1;
pub const JSON_ENCODING: &str = "json";

/// Payload bytes keyed exactly as `ProjectFile.sections[*].payload_key`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DomainPayloads(pub BTreeMap<PathBuf, Vec<u8>>);

impl DomainPayloads {
    pub fn get(&self, key: &Path) -> Option<&[u8]> {
        self.0.get(key).map(Vec::as_slice)
    }
}

/// Constructive state is immediately editable. AIR is supplied by the AIR
/// codec (or an empty document) rather than guessed by this module.
#[derive(Clone, Debug)]
pub struct DecodedConstructiveProject {
    pub name: String,
    pub aggregate_revision: u64,
    pub state: ProjectState,
    pub diagnostics: Vec<ProjectIoDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    MissingSection(String),
    DuplicateSection(String),
    UnsupportedSection {
        domain: String,
        version: u32,
        encoding: String,
    },
    MissingPayload(PathBuf),
    Json {
        domain: String,
        message: String,
    },
    Invalid {
        domain: String,
        message: String,
    },
    Identity {
        domain: String,
        expected: u64,
        allocated: u64,
    },
    UnresolvedAirBindings(usize),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSection(domain) => write!(f, "project has no {domain} section"),
            Self::DuplicateSection(domain) => write!(f, "project has duplicate {domain} sections"),
            Self::UnsupportedSection { domain, version, encoding } => write!(f, "unsupported {domain} payload schema {version} encoded as {encoding}"),
            Self::MissingPayload(path) => write!(f, "required project payload is missing: {}", path.display()),
            Self::Json { domain, message } => write!(f, "{domain} payload JSON is malformed: {message}"),
            Self::Invalid { domain, message } => write!(f, "{domain} payload is invalid: {message}"),
            Self::Identity { domain, expected, allocated } => write!(f, "{domain} identity {expected} cannot be restored losslessly (public allocator produced {allocated})"),
            Self::UnresolvedAirBindings(count) => write!(f, "{count} AIR binding(s) do not resolve in the supplied AIR document"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Encode every constructive section. The AIR payload remains owned by its
/// dedicated codec, but all AIR and legacy associations are preserved in the
/// bindings payload.
pub fn encode_constructive(
    project: &crate::daw_project::DawProject,
) -> Result<DomainPayloads, CodecError> {
    let state = project.state();
    let mut payloads = BTreeMap::new();
    insert_json(
        &mut payloads,
        "arrangement.json",
        &state.domains.arrangement,
        "arrangement",
    )?;
    insert_json(
        &mut payloads,
        "sequencer.json",
        &SequencerDto::from_model(&state.domains.sequencer),
        "sequencer",
    )?;
    insert_json(
        &mut payloads,
        "automation.json",
        &AutomationDto::from_model(&state.domains.automation),
        "automation",
    )?;
    insert_json(
        &mut payloads,
        "assets.json",
        &AssetsDto::from_model(&state.domains.assets),
        "assets",
    )?;
    insert_json(
        &mut payloads,
        "mixer.json",
        &MixerDto::from_model(&state.domains.mixer),
        "mixer",
    )?;
    insert_json(
        &mut payloads,
        "bindings.json",
        &BindingsDto::from_model(&state.bindings),
        "bindings",
    )?;
    Ok(DomainPayloads(payloads))
}

/// Decode constructive domains with an explicit AIR document. Passing an
/// empty AIR is valid only when no persisted binding targets AIR identities.
pub fn decode_constructive(
    file: &ProjectFile,
    payloads: &DomainPayloads,
    air: AuditoryIr,
) -> Result<DecodedConstructiveProject, CodecError> {
    let arrangement: ArrangementState = decode_section(file, payloads, "arrangement")?;
    arrangement
        .validate()
        .map_err(|e| invalid("arrangement", e))?;
    let sequencer = decode_section::<SequencerDto>(file, payloads, "sequencer")?.into_model()?;
    let automation = decode_section::<AutomationDto>(file, payloads, "automation")?.into_model()?;
    let assets = decode_section::<AssetsDto>(file, payloads, "assets")?.into_model()?;
    let mixer = decode_section::<MixerDto>(file, payloads, "mixer")?.into_model()?;
    let bindings = decode_section::<BindingsDto>(file, payloads, "bindings")?.into_model()?;

    if arrangement.sample_rate != sequencer.tempo_map().sample_rate()
        || arrangement.sample_rate != air.sample_rate
    {
        return Err(CodecError::Invalid {
            domain: "project".into(),
            message: "arrangement, sequencer, and AIR sample rates differ".into(),
        });
    }
    let state = ProjectState {
        domains: crate::daw_project::ProjectDomains {
            arrangement,
            sequencer,
            automation,
            assets,
            mixer,
            air,
        },
        bindings,
    };
    let issues = crate::daw_project::validate_project_state(
        crate::daw_project::DAW_PROJECT_SCHEMA_VERSION,
        &state,
    );
    if !issues.is_empty() {
        let air_count = issues
            .iter()
            .filter(|issue| issue.path.contains("air") || issue.message.contains("AIR"))
            .count();
        if air_count > 0 {
            return Err(CodecError::UnresolvedAirBindings(air_count));
        }
        return Err(CodecError::Invalid {
            domain: "project".into(),
            message: issues
                .iter()
                .map(|issue| format!("{}: {}", issue.path, issue.message))
                .collect::<Vec<_>>()
                .join("; "),
        });
    }
    let diagnostics = section_revision_diagnostics(file);
    Ok(DecodedConstructiveProject {
        name: file.project_name.clone(),
        aggregate_revision: file.aggregate_revision,
        state,
        diagnostics,
    })
}

fn insert_json<T: Serialize>(
    out: &mut BTreeMap<PathBuf, Vec<u8>>,
    key: &str,
    value: &T,
    domain: &str,
) -> Result<(), CodecError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|e| CodecError::Json {
        domain: domain.into(),
        message: e.to_string(),
    })?;
    bytes.push(b'\n');
    out.insert(PathBuf::from(key), bytes);
    Ok(())
}

fn decode_section<T: for<'de> Deserialize<'de>>(
    file: &ProjectFile,
    payloads: &DomainPayloads,
    domain: &str,
) -> Result<T, CodecError> {
    let matches = file
        .sections
        .iter()
        .filter(|section| section.domain == domain)
        .collect::<Vec<_>>();
    let section = match matches.as_slice() {
        [] => return Err(CodecError::MissingSection(domain.into())),
        [section] => *section,
        _ => return Err(CodecError::DuplicateSection(domain.into())),
    };
    if section.schema_version != CONSTRUCTIVE_CODEC_VERSION || section.encoding != JSON_ENCODING {
        return Err(CodecError::UnsupportedSection {
            domain: domain.into(),
            version: section.schema_version,
            encoding: section.encoding.clone(),
        });
    }
    let bytes = payloads
        .get(&section.payload_key)
        .ok_or_else(|| CodecError::MissingPayload(section.payload_key.clone()))?;
    serde_json::from_slice(bytes).map_err(|e| CodecError::Json {
        domain: domain.into(),
        message: e.to_string(),
    })
}

fn invalid(domain: &str, error: impl fmt::Display) -> CodecError {
    CodecError::Invalid {
        domain: domain.into(),
        message: error.to_string(),
    }
}

fn section_revision_diagnostics(file: &ProjectFile) -> Vec<ProjectIoDiagnostic> {
    file.sections
        .iter()
        .filter(|s| s.revision > file.aggregate_revision)
        .map(|s| ProjectIoDiagnostic {
            level: DiagnosticLevel::Warning,
            code: "domain-revision-ahead",
            message: format!(
                "{} section revision {} is ahead of aggregate revision {}",
                s.domain, s.revision, file.aggregate_revision
            ),
        })
        .collect()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SequencerDto {
    schema_version: u32,
    sample_rate: u32,
    tempos: Vec<TempoPointDto>,
    meters: Vec<MeterPointDto>,
    patterns: Vec<PatternDto>,
    clips: Vec<PatternClipDto>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct TempoPointDto {
    at: i64,
    micros_per_quarter: u32,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct MeterPointDto {
    at: i64,
    numerator: u16,
    denominator: u16,
}

impl SequencerDto {
    fn from_model(value: &Sequencer) -> Self {
        Self {
            schema_version: 1,
            sample_rate: value.tempo_map().sample_rate(),
            tempos: value
                .tempo_map()
                .tempo_points()
                .iter()
                .map(|p| TempoPointDto {
                    at: p.at.0,
                    micros_per_quarter: p.tempo.micros_per_quarter,
                })
                .collect(),
            meters: value
                .tempo_map()
                .meter_points()
                .iter()
                .map(|p| MeterPointDto {
                    at: p.at.0,
                    numerator: p.signature.numerator,
                    denominator: p.signature.denominator,
                })
                .collect(),
            patterns: value
                .patterns()
                .patterns()
                .map(PatternDto::from_model)
                .collect(),
            clips: value.clips().map(PatternClipDto::from_model).collect(),
        }
    }

    fn into_model(self) -> Result<Sequencer, CodecError> {
        if self.schema_version != 1 {
            return Err(CodecError::UnsupportedSection {
                domain: "sequencer".into(),
                version: self.schema_version,
                encoding: JSON_ENCODING.into(),
            });
        }
        let first_tempo = self
            .tempos
            .first()
            .ok_or_else(|| invalid("sequencer", "tempo map is empty"))?;
        let first_meter = self
            .meters
            .first()
            .ok_or_else(|| invalid("sequencer", "meter map is empty"))?;
        if first_tempo.at != 0 || first_meter.at != 0 {
            return Err(invalid(
                "sequencer",
                "tempo and meter maps must begin at tick zero",
            ));
        }
        let mut map = TempoMap::new(
            self.sample_rate,
            Tempo::from_micros_per_quarter(first_tempo.micros_per_quarter),
            TimeSignature::new(first_meter.numerator, first_meter.denominator)
                .map_err(|e| invalid("sequencer", e))?,
        )
        .map_err(|e| invalid("sequencer", e))?;
        for point in self.tempos.into_iter().skip(1) {
            map.set_tempo(
                BeatTime(point.at),
                Tempo::from_micros_per_quarter(point.micros_per_quarter),
            )
            .map_err(|e| invalid("sequencer", e))?;
        }
        for point in self.meters.into_iter().skip(1) {
            map.set_meter(
                BeatTime(point.at),
                TimeSignature::new(point.numerator, point.denominator)
                    .map_err(|e| invalid("sequencer", e))?,
            )
            .map_err(|e| invalid("sequencer", e))?;
        }
        let mut model = Sequencer::new(map);
        let mut commands = Vec::new();
        for pattern in self.patterns {
            commands.push(SequencerCommand::PutPattern {
                before: None,
                after: Some(pattern.into_model()?),
            });
        }
        for clip in self.clips {
            commands.push(SequencerCommand::PutClip {
                before: None,
                after: Some(clip.into_model()),
            });
        }
        model
            .execute("restore sequencer", commands)
            .map_err(|e| invalid("sequencer", e))?;
        Ok(model)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PatternDto {
    id: u64,
    name: String,
    length: u64,
    revision: u64,
    content: PatternContentDto,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum PatternContentDto {
    Notes(Vec<NoteDto>),
    Steps(StepPatternDto),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct NoteDto {
    id: u64,
    start: i64,
    duration: u64,
    midi_key: u8,
    cents: f32,
    velocity: f32,
    release_velocity: f32,
    pan: f32,
    probability: f32,
    micro_offset: i32,
    channel: u8,
    articulation: ArticulationDto,
    expression: ExpressionDto,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
enum ArticulationDto {
    Normal,
    Staccato,
    Tenuto,
    Legato,
    Accent,
    Named(String),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ExpressionDto {
    pitch_cents: Vec<(f32, f32)>,
    pressure: Vec<(f32, f32)>,
    timbre: Vec<(f32, f32)>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StepPatternDto {
    resolution: u64,
    swing: f32,
    lanes: Vec<StepLaneDto>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StepLaneDto {
    id: u64,
    name: String,
    target: TriggerTargetDto,
    choke_group: Option<u32>,
    steps: BTreeMap<u32, StepEventDto>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TriggerTargetDto {
    InstrumentNote { instrument: u64, key: u8 },
    DrumPad { rack: u64, pad: u16 },
    Sample { asset: u64 },
    AnalysisTemplate { template: u64 },
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct StepEventDto {
    velocity: f32,
    probability: f32,
    micro_offset: i32,
    gate: u64,
    ratchets: u8,
    pitch_semitones: f32,
    pan: f32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PatternClipDto {
    id: u64,
    pattern: u64,
    start: i64,
    length: u64,
    pattern_offset: i64,
    looped: bool,
    transpose_semitones: f32,
    gain: f32,
    muted: bool,
}

impl PatternDto {
    fn from_model(p: &PatternDefinition) -> Self {
        let content = match &p.content {
            PatternContent::Notes(notes) => {
                PatternContentDto::Notes(notes.notes.values().map(NoteDto::from_model).collect())
            }
            PatternContent::Steps(steps) => PatternContentDto::Steps(StepPatternDto {
                resolution: steps.resolution.0,
                swing: steps.swing,
                lanes: steps.lanes.values().map(StepLaneDto::from_model).collect(),
            }),
        };
        Self {
            id: p.id.get(),
            name: p.name.clone(),
            length: p.length.0,
            revision: p.revision,
            content,
        }
    }
    fn into_model(self) -> Result<PatternDefinition, CodecError> {
        let content = match self.content {
            PatternContentDto::Notes(notes) => PatternContent::Notes(NotePattern {
                notes: notes
                    .into_iter()
                    .map(NoteDto::into_model)
                    .map(|n| (n.id, n))
                    .collect(),
            }),
            PatternContentDto::Steps(steps) => PatternContent::Steps(StepPattern {
                resolution: BeatDuration(steps.resolution),
                swing: steps.swing,
                lanes: steps
                    .lanes
                    .into_iter()
                    .map(StepLaneDto::into_model)
                    .map(|l| (l.id, l))
                    .collect(),
            }),
        };
        let value = PatternDefinition {
            id: PatternId::from_raw(self.id),
            name: self.name,
            length: BeatDuration(self.length),
            content,
            revision: self.revision,
        };
        value.validate().map_err(|e| invalid("sequencer", e))?;
        Ok(value)
    }
}

impl NoteDto {
    fn from_model(n: &NoteEvent) -> Self {
        Self {
            id: n.id.get(),
            start: n.start.0,
            duration: n.duration.0,
            midi_key: n.pitch.midi_key,
            cents: n.pitch.cents,
            velocity: n.velocity,
            release_velocity: n.release_velocity,
            pan: n.pan,
            probability: n.probability,
            micro_offset: n.micro_offset,
            channel: n.channel,
            articulation: ArticulationDto::from_model(&n.articulation),
            expression: ExpressionDto::from_model(&n.expression),
        }
    }
    fn into_model(self) -> NoteEvent {
        NoteEvent {
            id: NoteId::from_raw(self.id),
            start: BeatTime(self.start),
            duration: BeatDuration(self.duration),
            pitch: NotePitch {
                midi_key: self.midi_key,
                cents: self.cents,
            },
            velocity: self.velocity,
            release_velocity: self.release_velocity,
            pan: self.pan,
            probability: self.probability,
            micro_offset: self.micro_offset,
            channel: self.channel,
            articulation: self.articulation.into_model(),
            expression: self.expression.into_model(),
        }
    }
}
impl ArticulationDto {
    fn from_model(v: &Articulation) -> Self {
        match v {
            Articulation::Normal => Self::Normal,
            Articulation::Staccato => Self::Staccato,
            Articulation::Tenuto => Self::Tenuto,
            Articulation::Legato => Self::Legato,
            Articulation::Accent => Self::Accent,
            Articulation::Named(s) => Self::Named(s.clone()),
        }
    }
    fn into_model(self) -> Articulation {
        match self {
            Self::Normal => Articulation::Normal,
            Self::Staccato => Articulation::Staccato,
            Self::Tenuto => Articulation::Tenuto,
            Self::Legato => Articulation::Legato,
            Self::Accent => Articulation::Accent,
            Self::Named(s) => Articulation::Named(s),
        }
    }
}
impl ExpressionDto {
    fn from_model(v: &PerNoteExpression) -> Self {
        let c = |xs: &[ExpressionPoint]| xs.iter().map(|p| (p.position, p.value)).collect();
        Self {
            pitch_cents: c(&v.pitch_cents),
            pressure: c(&v.pressure),
            timbre: c(&v.timbre),
        }
    }
    fn into_model(self) -> PerNoteExpression {
        let c = |xs: Vec<(f32, f32)>| {
            xs.into_iter()
                .map(|(position, value)| ExpressionPoint { position, value })
                .collect()
        };
        PerNoteExpression {
            pitch_cents: c(self.pitch_cents),
            pressure: c(self.pressure),
            timbre: c(self.timbre),
        }
    }
}
impl StepLaneDto {
    fn from_model(v: &StepLane) -> Self {
        Self {
            id: v.id.get(),
            name: v.name.clone(),
            target: TriggerTargetDto::from_model(&v.target),
            choke_group: v.choke_group,
            steps: v
                .steps
                .iter()
                .map(|(k, e)| (*k, StepEventDto::from_model(e)))
                .collect(),
        }
    }
    fn into_model(self) -> StepLane {
        StepLane {
            id: StepLaneId::from_raw(self.id),
            name: self.name,
            target: self.target.into_model(),
            choke_group: self.choke_group,
            steps: self
                .steps
                .into_iter()
                .map(|(k, e)| (k, e.into_model()))
                .collect(),
        }
    }
}
impl TriggerTargetDto {
    fn from_model(v: &TriggerTarget) -> Self {
        match v {
            TriggerTarget::InstrumentNote { instrument, key } => Self::InstrumentNote {
                instrument: *instrument,
                key: *key,
            },
            TriggerTarget::DrumPad { rack, pad } => Self::DrumPad {
                rack: *rack,
                pad: *pad,
            },
            TriggerTarget::Sample(id) => Self::Sample { asset: id.get() },
            TriggerTarget::AnalysisTemplate(id) => Self::AnalysisTemplate { template: *id },
        }
    }
    fn into_model(self) -> TriggerTarget {
        match self {
            Self::InstrumentNote { instrument, key } => {
                TriggerTarget::InstrumentNote { instrument, key }
            }
            Self::DrumPad { rack, pad } => TriggerTarget::DrumPad { rack, pad },
            Self::Sample { asset } => TriggerTarget::Sample(SampleAssetId::from_raw(asset)),
            Self::AnalysisTemplate { template } => TriggerTarget::AnalysisTemplate(template),
        }
    }
}
impl StepEventDto {
    fn from_model(v: &StepEvent) -> Self {
        Self {
            velocity: v.velocity,
            probability: v.probability,
            micro_offset: v.micro_offset,
            gate: v.gate.0,
            ratchets: v.ratchets,
            pitch_semitones: v.pitch_semitones,
            pan: v.pan,
        }
    }
    fn into_model(self) -> StepEvent {
        StepEvent {
            velocity: self.velocity,
            probability: self.probability,
            micro_offset: self.micro_offset,
            gate: BeatDuration(self.gate),
            ratchets: self.ratchets,
            pitch_semitones: self.pitch_semitones,
            pan: self.pan,
        }
    }
}
impl PatternClipDto {
    fn from_model(v: &PatternClip) -> Self {
        Self {
            id: v.id.get(),
            pattern: v.pattern.get(),
            start: v.start.0,
            length: v.length.0,
            pattern_offset: v.pattern_offset.0,
            looped: v.looped,
            transpose_semitones: v.transpose_semitones,
            gain: v.gain,
            muted: v.muted,
        }
    }
    fn into_model(self) -> PatternClip {
        PatternClip {
            id: crate::sequencer::PatternClipId::from_raw(self.id),
            pattern: PatternId::from_raw(self.pattern),
            start: BeatTime(self.start),
            length: BeatDuration(self.length),
            pattern_offset: BeatTime(self.pattern_offset),
            looped: self.looped,
            transpose_semitones: self.transpose_semitones,
            gain: self.gain,
            muted: self.muted,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AutomationDto {
    schema_version: u32,
    descriptors: Vec<DescriptorDto>,
    lanes: Vec<LaneDto>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct DescriptorDto {
    address: AddressDto,
    name: String,
    unit: UnitDto,
    minimum: f64,
    maximum: f64,
    default: f64,
    mapping: MappingDto,
    smoothing: SmoothingDto,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct LaneDto {
    id: u64,
    name: String,
    target: AddressDto,
    time_domain: TimeDomainDto,
    binding: BindingModeDto,
    extrapolation: ExtrapolationDto,
    enabled: bool,
    points: Vec<PointDto>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PointDto {
    id: u64,
    position: PositionDto,
    value: f64,
    outgoing: ShapeDto,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AddressDto {
    Mixer {
        target: MixerTargetDto,
    },
    Plugin {
        processor_id: u64,
        key: String,
    },
    Clip {
        clip_id: u64,
        parameter: ClipParameterDto,
    },
    Decomposition {
        target: DecompositionDto,
    },
    PerceptualLens {
        lens_id: String,
        parameter: LensParameterDto,
    },
    AirParameter {
        id: u64,
    },
    Custom {
        namespace: String,
        entity: String,
        parameter: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
enum MixerTargetDto {
    BusGain(u64),
    BusPan(u64),
    BusMute(u64),
    SendLevel(u64),
    SendMute(u64),
    InsertWet(u64),
    InsertBypass(u64),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
enum ClipParameterDto {
    Gain,
    Pan,
    PitchSemitones,
    PlaybackRate,
    FadeIn,
    FadeOut,
    Reverse,
    Custom(String),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DecompositionDto {
    ComponentGain {
        component_id: u64,
    },
    ComponentPan {
        component_id: u64,
    },
    ObjectTransformParameter {
        object_id: u64,
        transform_id: u64,
        parameter_id: u64,
    },
    HypothesisBlend {
        hypothesis_id: u64,
    },
    ResidualMix {
        hypothesis_set_id: u64,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
enum LensParameterDto {
    MinimumFrequency,
    MaximumFrequency,
    DynamicRange,
    DbCeiling,
    TimeResolution,
    FrequencyResolution,
    HarmonicEmphasis,
    TransientEmphasis,
    ChromaticAberration,
    DepthDefocus,
    Custom(String),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "values", rename_all = "snake_case")]
enum UnitDto {
    Linear,
    Normalized,
    Decibels,
    Hertz,
    Semitones,
    Ratio,
    Percent,
    Frames,
    Seconds,
    Degrees,
    Radians,
    Boolean,
    Enumerated(Vec<String>),
    Custom(String),
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum MappingDto {
    Linear,
    Logarithmic,
    Stepped { values: u32 },
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum SmoothingDto {
    None,
    LinearFrames(u32),
    OnePoleMilliseconds(f64),
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TimeDomainDto {
    Frames,
    Beats,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BindingModeDto {
    Replace,
    Add,
    Multiply,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExtrapolationDto {
    HoldEndpoints,
    ParameterDefault,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(tag = "domain", content = "coordinate", rename_all = "snake_case")]
enum PositionDto {
    Frames(i64),
    Beats(i64),
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ShapeDto {
    Hold,
    Linear,
    Smooth,
    Exponential,
    CubicBezier {
        outgoing_tangent: f64,
        incoming_tangent: f64,
    },
}

impl AutomationDto {
    fn from_model(v: &AutomationGraph) -> Self {
        Self {
            schema_version: 1,
            descriptors: v.descriptors().map(DescriptorDto::from_model).collect(),
            lanes: v.lanes().map(LaneDto::from_model).collect(),
        }
    }
    fn into_model(self) -> Result<AutomationGraph, CodecError> {
        if self.schema_version != 1 {
            return Err(CodecError::UnsupportedSection {
                domain: "automation".into(),
                version: self.schema_version,
                encoding: JSON_ENCODING.into(),
            });
        }
        let mut graph = AutomationGraph::new();
        for d in self.descriptors {
            graph
                .register_parameter(d.into_model())
                .map_err(|e| invalid("automation", e))?;
        }
        for lane in self.lanes {
            let lane = lane.into_model()?;
            graph
                .apply(&AutomationCommand {
                    label: "restore lane".into(),
                    changes: vec![LaneChange {
                        before: None,
                        after: Some(lane),
                    }],
                })
                .map_err(|e| invalid("automation", e))?;
        }
        graph.validate().map_err(|e| invalid("automation", e))?;
        Ok(graph)
    }
}
impl DescriptorDto {
    fn from_model(v: &ParameterDescriptor) -> Self {
        Self {
            address: AddressDto::from_model(&v.address),
            name: v.name.clone(),
            unit: UnitDto::from_model(&v.unit),
            minimum: v.minimum,
            maximum: v.maximum,
            default: v.default,
            mapping: MappingDto::from_model(v.mapping),
            smoothing: SmoothingDto::from_model(v.smoothing),
        }
    }
    fn into_model(self) -> ParameterDescriptor {
        ParameterDescriptor {
            address: self.address.into_model(),
            name: self.name,
            unit: self.unit.into_model(),
            minimum: self.minimum,
            maximum: self.maximum,
            default: self.default,
            mapping: self.mapping.into_model(),
            smoothing: self.smoothing.into_model(),
        }
    }
}
impl LaneDto {
    fn from_model(v: &AutomationLane) -> Self {
        Self {
            id: v.id.get(),
            name: v.name.clone(),
            target: AddressDto::from_model(&v.target),
            time_domain: TimeDomainDto::from_model(v.time_domain),
            binding: BindingModeDto::from_model(v.binding),
            extrapolation: ExtrapolationDto::from_model(v.extrapolation),
            enabled: v.enabled,
            points: v.points().iter().map(PointDto::from_model).collect(),
        }
    }
    fn into_model(self) -> Result<AutomationLane, CodecError> {
        let target = self.target.into_model();
        let domain = self.time_domain.into_model();
        let mut lane = AutomationLane::new(
            AutomationLaneId::from_raw(self.id),
            self.name,
            target,
            domain,
        );
        lane.binding = self.binding.into_model();
        lane.extrapolation = self.extrapolation.into_model();
        lane.enabled = self.enabled;
        for p in self.points {
            lane.insert_point(p.into_model())
                .map_err(|e| invalid("automation", e))?;
        }
        Ok(lane)
    }
}
impl PointDto {
    fn from_model(v: &AutomationPoint) -> Self {
        Self {
            id: v.id.get(),
            position: PositionDto::from_model(v.position),
            value: v.value,
            outgoing: ShapeDto::from_model(v.outgoing),
        }
    }
    fn into_model(self) -> AutomationPoint {
        AutomationPoint {
            id: AutomationPointId::from_raw(self.id),
            position: self.position.into_model(),
            value: self.value,
            outgoing: self.outgoing.into_model(),
        }
    }
}
impl AddressDto {
    fn from_model(v: &ParameterAddress) -> Self {
        match v {
            ParameterAddress::Mixer(t) => Self::Mixer {
                target: MixerTargetDto::from_model(t),
            },
            ParameterAddress::Plugin { processor_id, key } => Self::Plugin {
                processor_id: *processor_id,
                key: key.clone(),
            },
            ParameterAddress::Clip { clip_id, parameter } => Self::Clip {
                clip_id: *clip_id,
                parameter: ClipParameterDto::from_model(parameter),
            },
            ParameterAddress::Decomposition(t) => Self::Decomposition {
                target: DecompositionDto::from_model(t),
            },
            ParameterAddress::PerceptualLens { lens_id, parameter } => Self::PerceptualLens {
                lens_id: lens_id.clone(),
                parameter: LensParameterDto::from_model(parameter),
            },
            ParameterAddress::AirParameter(id) => Self::AirParameter { id: *id },
            ParameterAddress::Custom {
                namespace,
                entity,
                parameter,
            } => Self::Custom {
                namespace: namespace.clone(),
                entity: entity.clone(),
                parameter: parameter.clone(),
            },
        }
    }
    fn into_model(self) -> ParameterAddress {
        match self {
            Self::Mixer { target } => ParameterAddress::Mixer(target.into_model()),
            Self::Plugin { processor_id, key } => ParameterAddress::Plugin { processor_id, key },
            Self::Clip { clip_id, parameter } => ParameterAddress::Clip {
                clip_id,
                parameter: parameter.into_model(),
            },
            Self::Decomposition { target } => ParameterAddress::Decomposition(target.into_model()),
            Self::PerceptualLens { lens_id, parameter } => ParameterAddress::PerceptualLens {
                lens_id,
                parameter: parameter.into_model(),
            },
            Self::AirParameter { id } => ParameterAddress::AirParameter(id),
            Self::Custom {
                namespace,
                entity,
                parameter,
            } => ParameterAddress::Custom {
                namespace,
                entity,
                parameter,
            },
        }
    }
}
impl UnitDto {
    fn from_model(v: &ParameterUnit) -> Self {
        match v {
            ParameterUnit::Linear => Self::Linear,
            ParameterUnit::Normalized => Self::Normalized,
            ParameterUnit::Decibels => Self::Decibels,
            ParameterUnit::Hertz => Self::Hertz,
            ParameterUnit::Semitones => Self::Semitones,
            ParameterUnit::Ratio => Self::Ratio,
            ParameterUnit::Percent => Self::Percent,
            ParameterUnit::Frames => Self::Frames,
            ParameterUnit::Seconds => Self::Seconds,
            ParameterUnit::Degrees => Self::Degrees,
            ParameterUnit::Radians => Self::Radians,
            ParameterUnit::Boolean => Self::Boolean,
            ParameterUnit::Enumerated(x) => Self::Enumerated(x.clone()),
            ParameterUnit::Custom(x) => Self::Custom(x.clone()),
        }
    }
    fn into_model(self) -> ParameterUnit {
        match self {
            Self::Linear => ParameterUnit::Linear,
            Self::Normalized => ParameterUnit::Normalized,
            Self::Decibels => ParameterUnit::Decibels,
            Self::Hertz => ParameterUnit::Hertz,
            Self::Semitones => ParameterUnit::Semitones,
            Self::Ratio => ParameterUnit::Ratio,
            Self::Percent => ParameterUnit::Percent,
            Self::Frames => ParameterUnit::Frames,
            Self::Seconds => ParameterUnit::Seconds,
            Self::Degrees => ParameterUnit::Degrees,
            Self::Radians => ParameterUnit::Radians,
            Self::Boolean => ParameterUnit::Boolean,
            Self::Enumerated(x) => ParameterUnit::Enumerated(x),
            Self::Custom(x) => ParameterUnit::Custom(x),
        }
    }
}
impl TimeDomainDto {
    fn from_model(v: TimeDomain) -> Self {
        match v {
            TimeDomain::Frames => Self::Frames,
            TimeDomain::Beats => Self::Beats,
        }
    }
    fn into_model(self) -> TimeDomain {
        match self {
            Self::Frames => TimeDomain::Frames,
            Self::Beats => TimeDomain::Beats,
        }
    }
}
impl BindingModeDto {
    fn from_model(v: BindingMode) -> Self {
        match v {
            BindingMode::Replace => Self::Replace,
            BindingMode::Add => Self::Add,
            BindingMode::Multiply => Self::Multiply,
        }
    }
    fn into_model(self) -> BindingMode {
        match self {
            Self::Replace => BindingMode::Replace,
            Self::Add => BindingMode::Add,
            Self::Multiply => BindingMode::Multiply,
        }
    }
}
impl ExtrapolationDto {
    fn from_model(v: Extrapolation) -> Self {
        match v {
            Extrapolation::HoldEndpoints => Self::HoldEndpoints,
            Extrapolation::ParameterDefault => Self::ParameterDefault,
        }
    }
    fn into_model(self) -> Extrapolation {
        match self {
            Self::HoldEndpoints => Extrapolation::HoldEndpoints,
            Self::ParameterDefault => Extrapolation::ParameterDefault,
        }
    }
}
impl MappingDto {
    fn from_model(v: ValueMapping) -> Self {
        match v {
            ValueMapping::Linear => Self::Linear,
            ValueMapping::Logarithmic => Self::Logarithmic,
            ValueMapping::Stepped { values } => Self::Stepped { values },
        }
    }
    fn into_model(self) -> ValueMapping {
        match self {
            Self::Linear => ValueMapping::Linear,
            Self::Logarithmic => ValueMapping::Logarithmic,
            Self::Stepped { values } => ValueMapping::Stepped { values },
        }
    }
}
impl SmoothingDto {
    fn from_model(v: SmoothingPolicy) -> Self {
        match v {
            SmoothingPolicy::None => Self::None,
            SmoothingPolicy::LinearFrames(x) => Self::LinearFrames(x),
            SmoothingPolicy::OnePoleMilliseconds(x) => Self::OnePoleMilliseconds(x),
        }
    }
    fn into_model(self) -> SmoothingPolicy {
        match self {
            Self::None => SmoothingPolicy::None,
            Self::LinearFrames(x) => SmoothingPolicy::LinearFrames(x),
            Self::OnePoleMilliseconds(x) => SmoothingPolicy::OnePoleMilliseconds(x),
        }
    }
}
impl PositionDto {
    fn from_model(v: TimePosition) -> Self {
        match v {
            TimePosition::Frames(x) => Self::Frames(x.0),
            TimePosition::Beats(x) => Self::Beats(x.0),
        }
    }
    fn into_model(self) -> TimePosition {
        match self {
            Self::Frames(x) => TimePosition::Frames(crate::automation::ProjectFrame(x)),
            Self::Beats(x) => TimePosition::Beats(crate::automation::BeatTime(x)),
        }
    }
}
impl ShapeDto {
    fn from_model(v: SegmentShape) -> Self {
        match v {
            SegmentShape::Hold => Self::Hold,
            SegmentShape::Linear => Self::Linear,
            SegmentShape::Smooth => Self::Smooth,
            SegmentShape::Exponential => Self::Exponential,
            SegmentShape::CubicBezier {
                outgoing_tangent,
                incoming_tangent,
            } => Self::CubicBezier {
                outgoing_tangent,
                incoming_tangent,
            },
        }
    }
    fn into_model(self) -> SegmentShape {
        match self {
            Self::Hold => SegmentShape::Hold,
            Self::Linear => SegmentShape::Linear,
            Self::Smooth => SegmentShape::Smooth,
            Self::Exponential => SegmentShape::Exponential,
            Self::CubicBezier {
                outgoing_tangent,
                incoming_tangent,
            } => SegmentShape::CubicBezier {
                outgoing_tangent,
                incoming_tangent,
            },
        }
    }
}
impl MixerTargetDto {
    fn from_model(v: &MixerTarget) -> Self {
        match v {
            MixerTarget::BusGain(x) => Self::BusGain(*x),
            MixerTarget::BusPan(x) => Self::BusPan(*x),
            MixerTarget::BusMute(x) => Self::BusMute(*x),
            MixerTarget::SendLevel(x) => Self::SendLevel(*x),
            MixerTarget::SendMute(x) => Self::SendMute(*x),
            MixerTarget::InsertWet(x) => Self::InsertWet(*x),
            MixerTarget::InsertBypass(x) => Self::InsertBypass(*x),
        }
    }
    fn into_model(self) -> MixerTarget {
        match self {
            Self::BusGain(x) => MixerTarget::BusGain(x),
            Self::BusPan(x) => MixerTarget::BusPan(x),
            Self::BusMute(x) => MixerTarget::BusMute(x),
            Self::SendLevel(x) => MixerTarget::SendLevel(x),
            Self::SendMute(x) => MixerTarget::SendMute(x),
            Self::InsertWet(x) => MixerTarget::InsertWet(x),
            Self::InsertBypass(x) => MixerTarget::InsertBypass(x),
        }
    }
}
impl ClipParameterDto {
    fn from_model(v: &ClipParameter) -> Self {
        match v {
            ClipParameter::Gain => Self::Gain,
            ClipParameter::Pan => Self::Pan,
            ClipParameter::PitchSemitones => Self::PitchSemitones,
            ClipParameter::PlaybackRate => Self::PlaybackRate,
            ClipParameter::FadeIn => Self::FadeIn,
            ClipParameter::FadeOut => Self::FadeOut,
            ClipParameter::Reverse => Self::Reverse,
            ClipParameter::Custom(x) => Self::Custom(x.clone()),
        }
    }
    fn into_model(self) -> ClipParameter {
        match self {
            Self::Gain => ClipParameter::Gain,
            Self::Pan => ClipParameter::Pan,
            Self::PitchSemitones => ClipParameter::PitchSemitones,
            Self::PlaybackRate => ClipParameter::PlaybackRate,
            Self::FadeIn => ClipParameter::FadeIn,
            Self::FadeOut => ClipParameter::FadeOut,
            Self::Reverse => ClipParameter::Reverse,
            Self::Custom(x) => ClipParameter::Custom(x),
        }
    }
}
impl DecompositionDto {
    fn from_model(v: &DecompositionTarget) -> Self {
        match v {
            DecompositionTarget::ComponentGain { component_id } => Self::ComponentGain {
                component_id: *component_id,
            },
            DecompositionTarget::ComponentPan { component_id } => Self::ComponentPan {
                component_id: *component_id,
            },
            DecompositionTarget::ObjectTransformParameter {
                object_id,
                transform_id,
                parameter_id,
            } => Self::ObjectTransformParameter {
                object_id: *object_id,
                transform_id: *transform_id,
                parameter_id: *parameter_id,
            },
            DecompositionTarget::HypothesisBlend { hypothesis_id } => Self::HypothesisBlend {
                hypothesis_id: *hypothesis_id,
            },
            DecompositionTarget::ResidualMix { hypothesis_set_id } => Self::ResidualMix {
                hypothesis_set_id: *hypothesis_set_id,
            },
        }
    }
    fn into_model(self) -> DecompositionTarget {
        match self {
            Self::ComponentGain { component_id } => {
                DecompositionTarget::ComponentGain { component_id }
            }
            Self::ComponentPan { component_id } => {
                DecompositionTarget::ComponentPan { component_id }
            }
            Self::ObjectTransformParameter {
                object_id,
                transform_id,
                parameter_id,
            } => DecompositionTarget::ObjectTransformParameter {
                object_id,
                transform_id,
                parameter_id,
            },
            Self::HypothesisBlend { hypothesis_id } => {
                DecompositionTarget::HypothesisBlend { hypothesis_id }
            }
            Self::ResidualMix { hypothesis_set_id } => {
                DecompositionTarget::ResidualMix { hypothesis_set_id }
            }
        }
    }
}
impl LensParameterDto {
    fn from_model(v: &LensParameter) -> Self {
        match v {
            LensParameter::MinimumFrequency => Self::MinimumFrequency,
            LensParameter::MaximumFrequency => Self::MaximumFrequency,
            LensParameter::DynamicRange => Self::DynamicRange,
            LensParameter::DbCeiling => Self::DbCeiling,
            LensParameter::TimeResolution => Self::TimeResolution,
            LensParameter::FrequencyResolution => Self::FrequencyResolution,
            LensParameter::HarmonicEmphasis => Self::HarmonicEmphasis,
            LensParameter::TransientEmphasis => Self::TransientEmphasis,
            LensParameter::ChromaticAberration => Self::ChromaticAberration,
            LensParameter::DepthDefocus => Self::DepthDefocus,
            LensParameter::Custom(x) => Self::Custom(x.clone()),
        }
    }
    fn into_model(self) -> LensParameter {
        match self {
            Self::MinimumFrequency => LensParameter::MinimumFrequency,
            Self::MaximumFrequency => LensParameter::MaximumFrequency,
            Self::DynamicRange => LensParameter::DynamicRange,
            Self::DbCeiling => LensParameter::DbCeiling,
            Self::TimeResolution => LensParameter::TimeResolution,
            Self::FrequencyResolution => LensParameter::FrequencyResolution,
            Self::HarmonicEmphasis => LensParameter::HarmonicEmphasis,
            Self::TransientEmphasis => LensParameter::TransientEmphasis,
            Self::ChromaticAberration => LensParameter::ChromaticAberration,
            Self::DepthDefocus => LensParameter::DepthDefocus,
            Self::Custom(x) => LensParameter::Custom(x),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AssetsDto {
    schema_version: u32,
    assets: Vec<AssetDto>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct AssetDto {
    id: u64,
    name: String,
    location: LocationDto,
    availability: AvailabilityDto,
    metadata: MetadataDto,
    content_id: String,
    bytes_hashed: u64,
    imported_at_unix_ms: u64,
    origin: OriginDto,
    original_location: LocationDto,
    tags: BTreeSet<String>,
    favorite: bool,
    usages: Vec<UsageDto>,
    relinks: Vec<RelinkDto>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct LocationDto {
    absolute: Option<String>,
    project_relative: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct MetadataDto {
    sample_rate_hz: u32,
    channels: u16,
    frame_count: u64,
    container: Option<String>,
    codec: Option<String>,
    bit_depth: Option<u16>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AvailabilityDto {
    Present,
    Missing {
        checked_at_unix_ms: u64,
    },
    Relinked {
        previous_location: LocationDto,
        relinked_at_unix_ms: u64,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum OriginDto {
    ImportedFile {
        importer: String,
    },
    RecordedInput {
        device: String,
    },
    Rendered {
        renderer: String,
        source_revision: u64,
    },
    Generated {
        generator: String,
    },
    Migrated {
        source_format: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct UsageDto {
    id: u64,
    owner: UsageOwnerDto,
    source_range: Option<(u64, u64)>,
    label: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum UsageOwnerDto {
    AudioClip {
        persistent_id: u64,
    },
    SamplerZone {
        persistent_id: u64,
    },
    Step {
        persistent_id: u64,
    },
    AnalysisObject {
        persistent_id: u64,
    },
    Render {
        persistent_id: u64,
    },
    External {
        namespace: String,
        persistent_id: u64,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct RelinkDto {
    previous_location: LocationDto,
    new_location: LocationDto,
    relinked_at_unix_ms: u64,
    basis: RelinkBasisDto,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RelinkBasisDto {
    ExactContentFingerprint,
    MetadataAndNameCandidate,
    UserConfirmed,
}

impl AssetsDto {
    fn from_model(v: &AssetRegistry) -> Self {
        Self {
            schema_version: 1,
            assets: v.assets().values().map(AssetDto::from_model).collect(),
        }
    }
    fn into_model(self) -> Result<AssetRegistry, CodecError> {
        if self.schema_version != 1 {
            return Err(CodecError::UnsupportedSection {
                domain: "assets".into(),
                version: self.schema_version,
                encoding: JSON_ENCODING.into(),
            });
        }
        let mut registry = AssetRegistry::new();
        for asset in self.assets {
            asset.restore(&mut registry)?;
        }
        Ok(registry)
    }
}
impl AssetDto {
    fn from_model(v: &crate::assets::MediaAsset) -> Self {
        Self {
            id: v.id().0,
            name: v.name().into(),
            location: LocationDto::from_model(v.location()),
            availability: AvailabilityDto::from_model(v.availability()),
            metadata: MetadataDto::from_model(v.metadata()),
            content_id: v.content().id.to_hex(),
            bytes_hashed: v.content().bytes_hashed,
            imported_at_unix_ms: v.provenance().imported_at_unix_ms(),
            origin: OriginDto::from_model(v.provenance().origin()),
            original_location: LocationDto::from_model(v.provenance().original_location()),
            tags: v.tags().clone(),
            favorite: v.is_favorite(),
            usages: v.usages().values().map(UsageDto::from_model).collect(),
            relinks: v
                .relink_history()
                .iter()
                .map(RelinkDto::from_model)
                .collect(),
        }
    }
    fn restore(self, registry: &mut AssetRegistry) -> Result<(), CodecError> {
        let original = self.original_location.into_model()?;
        let current = self.location.into_model()?;
        let raw = u128::from_str_radix(&self.content_id, 16).map_err(|e| invalid("assets", e))?;
        let id = registry
            .register(AssetRegistration {
                name: self.name,
                location: original.clone(),
                metadata: self.metadata.into_model(),
                content: ContentFingerprint {
                    algorithm: ContentHashAlgorithm::Fnv1a128NonCryptographic,
                    id: ContentId(raw),
                    bytes_hashed: self.bytes_hashed,
                },
                provenance: AssetProvenance::new(
                    self.imported_at_unix_ms,
                    self.origin.into_model(),
                    original,
                ),
                tags: self.tags,
                favorite: self.favorite,
            })
            .map_err(|e| invalid("assets", e))?;
        if id.0 != self.id {
            return Err(CodecError::Identity {
                domain: "assets".into(),
                expected: self.id,
                allocated: id.0,
            });
        }
        for relink in self.relinks {
            registry
                .relink(
                    id,
                    relink.new_location.into_model()?,
                    relink.relinked_at_unix_ms,
                    relink.basis.into_model(),
                )
                .map_err(|e| invalid("assets", e))?;
        }
        match self.availability {
            AvailabilityDto::Present => {
                if registry.get(id).unwrap().location() != &current {
                    registry
                        .relink(id, current, 0, RelinkBasis::UserConfirmed)
                        .map_err(|e| invalid("assets", e))?;
                }
                registry
                    .mark_present(id)
                    .map_err(|e| invalid("assets", e))?
            }
            AvailabilityDto::Missing { checked_at_unix_ms } => {
                if registry.get(id).unwrap().location() != &current {
                    registry
                        .relink(id, current, 0, RelinkBasis::UserConfirmed)
                        .map_err(|e| invalid("assets", e))?;
                }
                registry
                    .mark_missing(id, checked_at_unix_ms)
                    .map_err(|e| invalid("assets", e))?
            }
            AvailabilityDto::Relinked { .. } => {
                if registry.get(id).unwrap().location() != &current {
                    return Err(invalid(
                        "assets",
                        "relink history does not produce persisted current location",
                    ));
                }
            }
        }
        for usage in self.usages {
            let expected = usage.id;
            let range = usage
                .source_range
                .map(|(a, b)| {
                    AssetFrameRange::new(SampleFrames(a), SampleFrames(b))
                        .map_err(|e| invalid("assets", e))
                })
                .transpose()?;
            let allocated = registry
                .add_usage(id, usage.owner.into_model(), range, usage.label)
                .map_err(|e| invalid("assets", e))?;
            if allocated.0 != expected {
                return Err(CodecError::Identity {
                    domain: "asset usage".into(),
                    expected,
                    allocated: allocated.0,
                });
            }
        }
        Ok(())
    }
}
impl LocationDto {
    fn from_model(v: &AssetLocation) -> Self {
        Self {
            absolute: v.absolute.as_ref().map(|x| x.as_str().into()),
            project_relative: v.project_relative.as_ref().map(|x| x.as_str().into()),
        }
    }
    fn into_model(self) -> Result<AssetLocation, CodecError> {
        AssetLocation::new(
            self.absolute
                .map(AbsolutePath::parse)
                .transpose()
                .map_err(|e| invalid("assets", e))?,
            self.project_relative
                .map(ProjectRelativePath::parse)
                .transpose()
                .map_err(|e| invalid("assets", e))?,
        )
        .map_err(|e| invalid("assets", e))
    }
}
impl MetadataDto {
    fn from_model(v: &DecodedAudioMetadata) -> Self {
        Self {
            sample_rate_hz: v.sample_rate_hz,
            channels: v.channels,
            frame_count: v.frame_count.0,
            container: v.container.clone(),
            codec: v.codec.clone(),
            bit_depth: v.bit_depth,
        }
    }
    fn into_model(self) -> DecodedAudioMetadata {
        DecodedAudioMetadata {
            sample_rate_hz: self.sample_rate_hz,
            channels: self.channels,
            frame_count: SampleFrames(self.frame_count),
            container: self.container,
            codec: self.codec,
            bit_depth: self.bit_depth,
        }
    }
}
impl AvailabilityDto {
    fn from_model(v: &AssetAvailability) -> Self {
        match v {
            AssetAvailability::Present => Self::Present,
            AssetAvailability::Missing { checked_at_unix_ms } => Self::Missing {
                checked_at_unix_ms: *checked_at_unix_ms,
            },
            AssetAvailability::Relinked {
                previous_location,
                relinked_at_unix_ms,
            } => Self::Relinked {
                previous_location: LocationDto::from_model(previous_location),
                relinked_at_unix_ms: *relinked_at_unix_ms,
            },
        }
    }
}
impl OriginDto {
    fn from_model(v: &AssetOrigin) -> Self {
        match v {
            AssetOrigin::ImportedFile { importer } => Self::ImportedFile {
                importer: importer.clone(),
            },
            AssetOrigin::RecordedInput { device } => Self::RecordedInput {
                device: device.clone(),
            },
            AssetOrigin::Rendered {
                renderer,
                source_revision,
            } => Self::Rendered {
                renderer: renderer.clone(),
                source_revision: *source_revision,
            },
            AssetOrigin::Generated { generator } => Self::Generated {
                generator: generator.clone(),
            },
            AssetOrigin::Migrated { source_format } => Self::Migrated {
                source_format: source_format.clone(),
            },
        }
    }
    fn into_model(self) -> AssetOrigin {
        match self {
            Self::ImportedFile { importer } => AssetOrigin::ImportedFile { importer },
            Self::RecordedInput { device } => AssetOrigin::RecordedInput { device },
            Self::Rendered {
                renderer,
                source_revision,
            } => AssetOrigin::Rendered {
                renderer,
                source_revision,
            },
            Self::Generated { generator } => AssetOrigin::Generated { generator },
            Self::Migrated { source_format } => AssetOrigin::Migrated { source_format },
        }
    }
}
impl UsageDto {
    fn from_model(v: &crate::assets::AssetUsage) -> Self {
        Self {
            id: v.id.0,
            owner: UsageOwnerDto::from_model(&v.owner),
            source_range: v.source_range.map(|r| (r.start.0, r.end.0)),
            label: v.label.clone(),
        }
    }
}
impl UsageOwnerDto {
    fn from_model(v: &AssetUsageOwner) -> Self {
        match v {
            AssetUsageOwner::AudioClip { persistent_id } => Self::AudioClip {
                persistent_id: *persistent_id,
            },
            AssetUsageOwner::SamplerZone { persistent_id } => Self::SamplerZone {
                persistent_id: *persistent_id,
            },
            AssetUsageOwner::Step { persistent_id } => Self::Step {
                persistent_id: *persistent_id,
            },
            AssetUsageOwner::AnalysisObject { persistent_id } => Self::AnalysisObject {
                persistent_id: *persistent_id,
            },
            AssetUsageOwner::Render { persistent_id } => Self::Render {
                persistent_id: *persistent_id,
            },
            AssetUsageOwner::External {
                kind,
                persistent_id,
            } => Self::External {
                namespace: kind.clone(),
                persistent_id: *persistent_id,
            },
        }
    }
    fn into_model(self) -> AssetUsageOwner {
        match self {
            Self::AudioClip { persistent_id } => AssetUsageOwner::AudioClip { persistent_id },
            Self::SamplerZone { persistent_id } => AssetUsageOwner::SamplerZone { persistent_id },
            Self::Step { persistent_id } => AssetUsageOwner::Step { persistent_id },
            Self::AnalysisObject { persistent_id } => {
                AssetUsageOwner::AnalysisObject { persistent_id }
            }
            Self::Render { persistent_id } => AssetUsageOwner::Render { persistent_id },
            Self::External {
                namespace,
                persistent_id,
            } => AssetUsageOwner::External {
                kind: namespace,
                persistent_id,
            },
        }
    }
}
impl RelinkDto {
    fn from_model(v: &crate::assets::RelinkEvent) -> Self {
        Self {
            previous_location: LocationDto::from_model(&v.previous_location),
            new_location: LocationDto::from_model(&v.new_location),
            relinked_at_unix_ms: v.relinked_at_unix_ms,
            basis: RelinkBasisDto::from_model(v.basis),
        }
    }
}
impl RelinkBasisDto {
    fn from_model(v: RelinkBasis) -> Self {
        match v {
            RelinkBasis::ExactContentFingerprint => Self::ExactContentFingerprint,
            RelinkBasis::MetadataAndNameCandidate => Self::MetadataAndNameCandidate,
            RelinkBasis::UserConfirmed => Self::UserConfirmed,
        }
    }
    fn into_model(self) -> RelinkBasis {
        match self {
            Self::ExactContentFingerprint => RelinkBasis::ExactContentFingerprint,
            Self::MetadataAndNameCandidate => RelinkBasis::MetadataAndNameCandidate,
            Self::UserConfirmed => RelinkBasis::UserConfirmed,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct MixerDto {
    schema_version: u32,
    master: u64,
    buses: Vec<BusDto>,
    processors: Vec<ProcessorDto>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct BusDto {
    id: u64,
    node_id: u64,
    name: String,
    kind: BusKindDto,
    output: Option<u64>,
    gain_db: f32,
    pan: f32,
    muted: bool,
    soloed: bool,
    inserts: Vec<InsertDto>,
    sends: Vec<SendDto>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BusKindDto {
    Source,
    Component,
    Group,
    Master,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct InsertDto {
    processor_id: u64,
    bypassed: bool,
    wet: f32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SendDto {
    id: u64,
    target: u64,
    tap: SendTapDto,
    level_db: f32,
    muted: bool,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SendTapDto {
    PreFader,
    PostFader,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProcessorDto {
    id: u64,
    node_id: u64,
    owner_bus: u64,
    descriptor: PluginDto,
    latency_samples: u32,
    parameters: Vec<ProcessorParameterDto>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PluginDto {
    format: String,
    identifier: String,
    display_name: String,
    vendor: Option<String>,
    opaque_state: Vec<u8>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProcessorParameterDto {
    id: u64,
    key: String,
    name: String,
    normalized_value: f32,
}

impl MixerDto {
    fn from_model(v: &MixerGraph) -> Self {
        Self {
            schema_version: 1,
            master: v.master().get(),
            buses: v.buses().map(BusDto::from_model).collect(),
            processors: v
                .processors()
                .map(|p| {
                    let owner = v
                        .buses()
                        .find(|b| b.inserts().iter().any(|i| i.processor_id() == p.id()))
                        .expect("validated mixer processor owner")
                        .id();
                    ProcessorDto::from_model(p, owner)
                })
                .collect(),
        }
    }
    fn into_model(self) -> Result<MixerGraph, CodecError> {
        if self.schema_version != 1 {
            return Err(CodecError::UnsupportedSection {
                domain: "mixer".into(),
                version: self.schema_version,
                encoding: JSON_ENCODING.into(),
            });
        }
        let master = self
            .buses
            .iter()
            .find(|b| b.id == self.master)
            .ok_or_else(|| invalid("mixer", "missing master bus"))?;
        if master.id != 1 || master.node_id != 1 || !matches!(master.kind, BusKindDto::Master) {
            return Err(invalid(
                "mixer",
                "public mixer API requires master bus/node identity 1",
            ));
        }
        let mut graph = MixerGraph::new(master.name.clone());
        enum Node<'a> {
            Bus(&'a BusDto),
            Processor(&'a ProcessorDto),
        }
        let mut nodes: Vec<(u64, Node<'_>)> = self
            .buses
            .iter()
            .filter(|b| b.id != self.master)
            .map(|b| (b.node_id, Node::Bus(b)))
            .chain(
                self.processors
                    .iter()
                    .map(|p| (p.node_id, Node::Processor(p))),
            )
            .collect();
        nodes.sort_by_key(|x| x.0);
        let mut expected_node = 2;
        for (node_id, node) in nodes {
            if node_id != expected_node {
                return Err(CodecError::Identity {
                    domain: "mixer node".into(),
                    expected: node_id,
                    allocated: expected_node,
                });
            }
            match node {
                Node::Bus(b) => {
                    let id = graph
                        .add_bus(b.kind.into_model(), b.name.clone())
                        .map_err(|e| invalid("mixer", e))?;
                    if id.get() != b.id {
                        return Err(CodecError::Identity {
                            domain: "mixer bus".into(),
                            expected: b.id,
                            allocated: id.get(),
                        });
                    }
                }
                Node::Processor(p) => {
                    let id = graph
                        .insert_processor(
                            BusId::from_raw(p.owner_bus),
                            None,
                            p.descriptor.clone().into_model(),
                            p.latency_samples,
                        )
                        .map_err(|e| invalid("mixer", e))?;
                    if id.get() != p.id {
                        return Err(CodecError::Identity {
                            domain: "mixer processor".into(),
                            expected: p.id,
                            allocated: id.get(),
                        });
                    }
                }
            }
            expected_node += 1;
        }
        let mut parameters = self
            .processors
            .iter()
            .flat_map(|p| p.parameters.iter().map(move |x| (p.id, x)))
            .collect::<Vec<_>>();
        parameters.sort_by_key(|(_, p)| p.id);
        for (processor, p) in parameters {
            let id = graph
                .add_parameter(
                    crate::mixer::ProcessorId::from_raw(processor),
                    p.key.clone(),
                    p.name.clone(),
                    p.normalized_value,
                )
                .map_err(|e| invalid("mixer", e))?;
            if id.get() != p.id {
                return Err(CodecError::Identity {
                    domain: "mixer parameter".into(),
                    expected: p.id,
                    allocated: id.get(),
                });
            }
        }
        for bus in &self.buses {
            if let Some(output) = bus.output {
                graph
                    .set_output(BusId::from_raw(bus.id), BusId::from_raw(output))
                    .map_err(|e| invalid("mixer", e))?;
            }
            graph
                .set_gain_db(BusId::from_raw(bus.id), bus.gain_db)
                .and_then(|_| graph.set_pan(BusId::from_raw(bus.id), bus.pan))
                .and_then(|_| graph.set_muted(BusId::from_raw(bus.id), bus.muted))
                .and_then(|_| graph.set_soloed(BusId::from_raw(bus.id), bus.soloed))
                .map_err(|e| invalid("mixer", e))?;
        }
        let mut sends = self
            .buses
            .iter()
            .flat_map(|b| b.sends.iter().map(move |s| (b.id, s)))
            .collect::<Vec<_>>();
        sends.sort_by_key(|(_, s)| s.id);
        for (from, s) in sends {
            let id = graph
                .add_send(
                    BusId::from_raw(from),
                    BusId::from_raw(s.target),
                    s.tap.into_model(),
                    s.level_db,
                )
                .map_err(|e| invalid("mixer", e))?;
            if id.get() != s.id {
                return Err(CodecError::Identity {
                    domain: "mixer send".into(),
                    expected: s.id,
                    allocated: id.get(),
                });
            }
            graph
                .set_send_muted(id, s.muted)
                .map_err(|e| invalid("mixer", e))?;
        }
        for bus in &self.buses {
            for (index, slot) in bus.inserts.iter().enumerate() {
                let id = crate::mixer::ProcessorId::from_raw(slot.processor_id);
                graph
                    .move_processor(BusId::from_raw(bus.id), id, index)
                    .map_err(|e| invalid("mixer", e))?;
                graph
                    .set_insert_bypassed(id, slot.bypassed)
                    .and_then(|_| graph.set_insert_wet(id, slot.wet))
                    .map_err(|e| invalid("mixer", e))?;
            }
        }
        graph.validate().map_err(|e| invalid("mixer", e))?;
        Ok(graph)
    }
}
impl BusDto {
    fn from_model(v: &crate::mixer::Bus) -> Self {
        let f = v.fader();
        Self {
            id: v.id().get(),
            node_id: v.node_id().get(),
            name: v.name().into(),
            kind: BusKindDto::from_model(v.kind()),
            output: v.output().map(|x| x.get()),
            gain_db: f.gain_db(),
            pan: f.pan(),
            muted: f.muted(),
            soloed: f.soloed(),
            inserts: v
                .inserts()
                .iter()
                .map(|x| InsertDto {
                    processor_id: x.processor_id().get(),
                    bypassed: x.bypassed(),
                    wet: x.wet(),
                })
                .collect(),
            sends: v.sends().iter().map(SendDto::from_model).collect(),
        }
    }
}
impl ProcessorDto {
    fn from_model(v: &crate::mixer::Processor, owner: BusId) -> Self {
        Self {
            id: v.id().get(),
            node_id: v.node_id().get(),
            owner_bus: owner.get(),
            descriptor: PluginDto::from_model(v.descriptor()),
            latency_samples: v.latency_samples(),
            parameters: v
                .parameters()
                .map(|p| ProcessorParameterDto {
                    id: p.id().get(),
                    key: p.key().into(),
                    name: p.name().into(),
                    normalized_value: p.normalized_value(),
                })
                .collect(),
        }
    }
}
impl SendDto {
    fn from_model(v: &crate::mixer::Send) -> Self {
        Self {
            id: v.id().get(),
            target: v.target().get(),
            tap: SendTapDto::from_model(v.tap()),
            level_db: v.level_db(),
            muted: v.muted(),
        }
    }
}
impl PluginDto {
    fn from_model(v: &PluginDescriptor) -> Self {
        Self {
            format: v.format.clone(),
            identifier: v.identifier.clone(),
            display_name: v.display_name.clone(),
            vendor: v.vendor.clone(),
            opaque_state: v.opaque_state.clone(),
        }
    }
    fn into_model(self) -> PluginDescriptor {
        PluginDescriptor {
            format: self.format,
            identifier: self.identifier,
            display_name: self.display_name,
            vendor: self.vendor,
            opaque_state: self.opaque_state,
        }
    }
}
impl BusKindDto {
    fn from_model(v: BusKind) -> Self {
        match v {
            BusKind::Source => Self::Source,
            BusKind::Component => Self::Component,
            BusKind::Group => Self::Group,
            BusKind::Master => Self::Master,
        }
    }
    fn into_model(self) -> BusKind {
        match self {
            Self::Source => BusKind::Source,
            Self::Component => BusKind::Component,
            Self::Group => BusKind::Group,
            Self::Master => BusKind::Master,
        }
    }
}
impl SendTapDto {
    fn from_model(v: SendTap) -> Self {
        match v {
            SendTap::PreFader => Self::PreFader,
            SendTap::PostFader => Self::PostFader,
        }
    }
    fn into_model(self) -> SendTap {
        match self {
            Self::PreFader => SendTap::PreFader,
            Self::PostFader => SendTap::PostFader,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BindingsDto {
    schema_version: u32,
    arrangement_assets: Vec<(u64, u64)>,
    sequencer_samples: Vec<(u64, u64)>,
    pattern_definitions: Vec<(u64, u64)>,
    pattern_placements: Vec<(u64, u64)>,
    automation_lanes: Vec<(u64, u64)>,
    mixer_tracks: Vec<(u64, u64)>,
    mixer_clip_overrides: Vec<(u64, u64)>,
    air_clips: Vec<(u64, u64)>,
    air_assets: Vec<(u64, u64)>,
    air_automation_lanes: Vec<(u64, u64)>,
    air_patterns: Vec<(u64, u64)>,
    legacy_events: Vec<(u64, u64)>,
    legacy_clusters: Vec<(u64, u64)>,
}
impl BindingsDto {
    fn from_model(v: &ProjectBindings) -> Self {
        Self {
            schema_version: 1,
            arrangement_assets: pairs(&v.assets.arrangement_assets, |x| x.get(), |x| x.0),
            sequencer_samples: pairs(&v.assets.sequencer_samples, |x| x.get(), |x| x.0),
            pattern_definitions: pairs(&v.patterns.definitions, |x| x.get(), |x| x.get()),
            pattern_placements: pairs(&v.patterns.placements, |x| x.get(), |x| x.get()),
            automation_lanes: pairs(&v.automation.lanes, |x| x.get(), |x| x.get()),
            mixer_tracks: pairs(&v.mixer.tracks, |x| x.get(), |x| x.get()),
            mixer_clip_overrides: pairs(&v.mixer.clip_overrides, |x| x.get(), |x| x.get()),
            air_clips: pairs(&v.air.clips, |x| x.get(), |x| x.get()),
            air_assets: pairs(&v.air.assets, |x| x.0, |x| x.get()),
            air_automation_lanes: pairs(&v.air.automation_lanes, |x| x.get(), |x| x.get()),
            air_patterns: pairs(&v.air.patterns, |x| x.get(), |x| x.get()),
            legacy_events: pairs(&v.legacy_air.events, |x| x.get(), |x| x.get()),
            legacy_clusters: pairs(&v.legacy_air.clusters, |x| x.get(), |x| x.get()),
        }
    }
    fn into_model(self) -> Result<ProjectBindings, CodecError> {
        if self.schema_version != 1 {
            return Err(CodecError::UnsupportedSection {
                domain: "bindings".into(),
                version: self.schema_version,
                encoding: JSON_ENCODING.into(),
            });
        }
        let mut out = ProjectBindings::default();
        for (left, right) in self.arrangement_assets {
            let got = out
                .bind_media_asset(AssetId(right))
                .map_err(|e| invalid("bindings", e))?;
            if got.get() != left {
                return Err(CodecError::Identity {
                    domain: "arrangement asset alias".into(),
                    expected: left,
                    allocated: got.get(),
                });
            }
        }
        for (left, right) in self.sequencer_samples {
            let got = out
                .bind_sequencer_sample(AssetId(right))
                .map_err(|e| invalid("bindings", e))?;
            if got.get() != left {
                return Err(CodecError::Identity {
                    domain: "sequencer sample alias".into(),
                    expected: left,
                    allocated: got.get(),
                });
            }
        }
        for (left, right) in self.pattern_definitions {
            let got = out
                .bind_pattern_definition(PatternId::from_raw(right))
                .map_err(|e| invalid("bindings", e))?;
            if got.get() != left {
                return Err(CodecError::Identity {
                    domain: "arrangement pattern alias".into(),
                    expected: left,
                    allocated: got.get(),
                });
            }
        }
        for (left, right) in self.automation_lanes {
            let got = out
                .bind_automation_lane(AutomationLaneId::from_raw(right))
                .map_err(|e| invalid("bindings", e))?;
            if got.get() != left {
                return Err(CodecError::Identity {
                    domain: "arrangement automation alias".into(),
                    expected: left,
                    allocated: got.get(),
                });
            }
        }
        out.patterns.placements = self
            .pattern_placements
            .into_iter()
            .map(|(a, b)| {
                (
                    crate::arrangement::ClipId::from_raw(a),
                    crate::sequencer::PatternClipId::from_raw(b),
                )
            })
            .collect();
        out.mixer = MixerBindings {
            tracks: self
                .mixer_tracks
                .into_iter()
                .map(|(a, b)| (crate::arrangement::TrackId::from_raw(a), BusId::from_raw(b)))
                .collect(),
            clip_overrides: self
                .mixer_clip_overrides
                .into_iter()
                .map(|(a, b)| (crate::arrangement::ClipId::from_raw(a), BusId::from_raw(b)))
                .collect(),
        };
        out.air = AirBindings {
            clips: self
                .air_clips
                .into_iter()
                .map(|(a, b)| {
                    (
                        crate::arrangement::ClipId::from_raw(a),
                        ontology::ObjectId::new(b),
                    )
                })
                .collect(),
            assets: self
                .air_assets
                .into_iter()
                .map(|(a, b)| (AssetId(a), ontology::SourceId::new(b)))
                .collect(),
            automation_lanes: self
                .air_automation_lanes
                .into_iter()
                .map(|(a, b)| (AutomationLaneId::from_raw(a), ontology::ParameterId::new(b)))
                .collect(),
            patterns: self
                .air_patterns
                .into_iter()
                .map(|(a, b)| (PatternId::from_raw(a), ontology::ObjectId::new(b)))
                .collect(),
        };
        out.legacy_air = LegacyIdentityArchive {
            events: self
                .legacy_events
                .into_iter()
                .map(|(a, b)| (session::EventId::from_raw(a), ontology::ObjectId::new(b)))
                .collect(),
            clusters: self
                .legacy_clusters
                .into_iter()
                .map(|(a, b)| {
                    (
                        session::ClusterId::from_raw(a),
                        ontology::HypothesisId::new(b),
                    )
                })
                .collect(),
        };
        Ok(out)
    }
}
fn pairs<K: Ord, V>(
    map: &BTreeMap<K, V>,
    left: impl Fn(&K) -> u64,
    right: impl Fn(&V) -> u64,
) -> Vec<(u64, u64)> {
    map.iter().map(|(k, v)| (left(k), right(v))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrangement::{Frame, FrameRange, SourceRange, TrackKind};
    use crate::daw_project::{DawProject, ProjectDomain};
    use std::collections::BTreeSet;

    fn project() -> DawProject {
        let mut project = DawProject::new("codec test", 48_000, 123.0).unwrap();
        let revision = project.revisions().aggregate;
        let touched = BTreeSet::from([
            ProjectDomain::Arrangement,
            ProjectDomain::Assets,
            ProjectDomain::Bindings,
            ProjectDomain::Mixer,
        ]);
        project
            .transact("seed", revision, touched, |state| -> Result<(), String> {
                let location =
                    AssetLocation::new(Some(AbsolutePath::parse("/tmp/source.wav").unwrap()), None)
                        .unwrap();
                let asset = state
                    .domains
                    .assets
                    .register(AssetRegistration {
                        name: "source".into(),
                        location: location.clone(),
                        metadata: DecodedAudioMetadata {
                            sample_rate_hz: 48_000,
                            channels: 2,
                            frame_count: SampleFrames(480),
                            container: Some("wav".into()),
                            codec: Some("pcm".into()),
                            bit_depth: Some(24),
                        },
                        content: ContentFingerprint::from_bytes(b"source"),
                        provenance: AssetProvenance::new(
                            7,
                            AssetOrigin::ImportedFile {
                                importer: "test".into(),
                            },
                            location,
                        ),
                        tags: BTreeSet::from(["source".into()]),
                        favorite: true,
                    })
                    .map_err(|e| e.to_string())?;
                let alias = state
                    .bindings
                    .bind_media_asset(asset)
                    .map_err(|e| e.to_string())?;
                let mut editor = crate::arrangement::ArrangementEditor::from_state(
                    state.domains.arrangement.clone(),
                )
                .map_err(|e| e.to_string())?;
                let track = editor
                    .create_track("Audio", TrackKind::Audio)
                    .map_err(|e| e.to_string())?;
                let clip = editor
                    .create_audio_clip(
                        track,
                        "source",
                        FrameRange::new(Frame(0), Frame(480)).unwrap(),
                        alias,
                        SourceRange::new(0, 480).unwrap(),
                    )
                    .map_err(|e| e.to_string())?;
                state
                    .domains
                    .assets
                    .add_usage(
                        asset,
                        AssetUsageOwner::AudioClip {
                            persistent_id: clip.get(),
                        },
                        Some(AssetFrameRange::new(SampleFrames(0), SampleFrames(480)).unwrap()),
                        "arrangement",
                    )
                    .map_err(|e| e.to_string())?;
                state.domains.arrangement = editor.state().clone();
                let bus = state
                    .domains
                    .mixer
                    .add_bus(BusKind::Source, "Audio")
                    .map_err(|e| e.to_string())?;
                state.bindings.mixer.tracks.insert(track, bus);
                Ok(())
            })
            .unwrap();
        project
    }
    #[test]
    fn constructive_round_trip_preserves_valid_state() {
        let p = project();
        let file = ProjectFile::from_project(&p, None);
        let payloads = encode_constructive(&p).unwrap();
        let decoded = decode_constructive(&file, &payloads, AuditoryIr::new(48_000)).unwrap();
        assert_eq!(
            decoded.state.domains.arrangement,
            p.state().domains.arrangement
        );
        assert_eq!(decoded.state.domains.assets, p.state().domains.assets);
        assert_eq!(decoded.state.domains.mixer, p.state().domains.mixer);
        assert_eq!(decoded.state.bindings, p.state().bindings);
        assert!(crate::daw_project::validate_project_state(1, &decoded.state).is_empty());
    }
    #[test]
    fn refuses_missing_payload() {
        let p = project();
        let file = ProjectFile::from_project(&p, None);
        let mut payloads = encode_constructive(&p).unwrap();
        payloads.0.remove(Path::new("mixer.json"));
        assert!(matches!(
            decode_constructive(&file, &payloads, AuditoryIr::new(48_000)),
            Err(CodecError::MissingPayload(_))
        ));
    }
    #[test]
    fn refuses_future_domain_schema() {
        let p = project();
        let mut file = ProjectFile::from_project(&p, None);
        file.sections
            .iter_mut()
            .find(|s| s.domain == "sequencer")
            .unwrap()
            .schema_version = 99;
        let payloads = encode_constructive(&p).unwrap();
        assert!(matches!(
            decode_constructive(&file, &payloads, AuditoryIr::new(48_000)),
            Err(CodecError::UnsupportedSection { .. })
        ));
    }
    #[test]
    fn binding_payload_preserves_legacy_archive() {
        let mut b = ProjectBindings::default();
        b.legacy_air
            .events
            .insert(session::EventId::from_raw(9), ontology::ObjectId::new(12));
        let bytes = serde_json::to_vec(&BindingsDto::from_model(&b)).unwrap();
        let dto: BindingsDto = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(dto.into_model().unwrap().legacy_air, b.legacy_air);
    }
}
