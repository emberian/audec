//! JSONL DTOs for isolated plugin scanners and runtime workers.
//!
//! Only Audec-owned values cross this boundary. CLAP/VST3 ABI values must be
//! translated to stable keys and descriptors inside the worker. PCM, events,
//! and opaque state bytes never travel in JSON; the control plane carries
//! bounded shared-memory grants and hash-checked session-relative state files.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::plugin::{
    ArtifactFingerprint, AudioPortDescriptor, AudioPortRole, ChannelLayout, CpuArchitecture,
    DeterminismClass, Digest32, ExecutionCapabilities, NegotiatedAudioPort, NormalizedValue,
    NoteDialect, NotePortDescriptor, ParameterMapping, PluginFormat, PluginKey, PluginMetadata,
    PluginParameterDescriptor, PluginParameterKey, PluginRole, PluginStateBlob, PortDirection,
    ProcessingContract, ScanFailure, ScanFailureKind, ScanRecord, ScannerProvenance,
    SharedMemoryAccess, SharedMemoryRegion, TailReport, WORKER_PROTOCOL_VERSION,
};

pub const MAX_STATE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub protocol_version: u32,
    pub sequence: u64,
    #[serde(flatten)]
    pub message: Message,
}

impl Envelope {
    pub fn new(sequence: u64, message: Message) -> Self {
        Self {
            protocol_version: WORKER_PROTOCOL_VERSION,
            sequence,
            message,
        }
    }

    pub fn to_jsonl(&self) -> Result<String, WireError> {
        self.validate()?;
        let mut line = serde_json::to_string(self).map_err(WireError::Encode)?;
        line.push('\n');
        Ok(line)
    }

    pub fn from_jsonl(line: &str) -> Result<Self, WireError> {
        if line.contains('\n') || line.contains('\r') {
            return Err(WireError::Malformed(
                "expected exactly one JSONL record".into(),
            ));
        }
        let envelope: Self = serde_json::from_str(line).map_err(WireError::Decode)?;
        envelope.validate()?;
        Ok(envelope)
    }

    fn validate(&self) -> Result<(), WireError> {
        if self.protocol_version != WORKER_PROTOCOL_VERSION {
            return Err(WireError::ProtocolVersion {
                actual: self.protocol_version,
                expected: WORKER_PROTOCOL_VERSION,
            });
        }
        self.message.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Message {
    Hello,
    Capabilities {
        capabilities: CapabilitiesDto,
    },
    Scan {
        request: ScanRequestDto,
    },
    ScanReady {
        request_id: u64,
        record: ScanRecordDto,
    },
    ScanFailed {
        request_id: u64,
        failure: ScanFailureDto,
    },
    Instantiate {
        request: InstantiateDto,
    },
    Instantiated {
        request_id: u64,
        instance: TokenDto,
        latency_frames: u32,
        tail: TailDto,
    },
    BindSharedMemory {
        binding: SharedMemoryBindingDto,
    },
    Bound {
        instance: TokenDto,
    },
    Activate {
        instance: TokenDto,
    },
    Activated {
        instance: TokenDto,
    },
    SetParameters {
        request: SetParametersDto,
    },
    ParametersSet {
        request_id: u64,
        instance: TokenDto,
    },
    Process {
        instance: TokenDto,
        process_sequence: u64,
        frames: u32,
        input_event_count: u32,
    },
    Processed {
        instance: TokenDto,
        process_sequence: u64,
        output_event_count: u32,
    },
    SaveState {
        request: SaveStateDto,
    },
    StateSaved {
        request_id: u64,
        state: StateArtifactDto,
    },
    LatencyChanged {
        instance: TokenDto,
        frames: u32,
    },
    TailChanged {
        instance: TokenDto,
        tail: TailDto,
    },
    Deactivate {
        instance: TokenDto,
    },
    Deactivated {
        instance: TokenDto,
    },
    Destroy {
        instance: TokenDto,
    },
    Destroyed {
        instance: TokenDto,
    },
    Error {
        failure: RuntimeFailureDto,
    },
    Shutdown,
}

impl Message {
    pub const fn direction(&self) -> Direction {
        match self {
            Self::Hello
            | Self::Scan { .. }
            | Self::Instantiate { .. }
            | Self::BindSharedMemory { .. }
            | Self::Activate { .. }
            | Self::SetParameters { .. }
            | Self::Process { .. }
            | Self::SaveState { .. }
            | Self::Deactivate { .. }
            | Self::Destroy { .. }
            | Self::Shutdown => Direction::ControllerToWorker,
            _ => Direction::WorkerToController,
        }
    }

    fn validate(&self) -> Result<(), WireError> {
        match self {
            Self::Hello | Self::Shutdown => Ok(()),
            Self::Capabilities { capabilities } => capabilities.validate(),
            Self::Scan { request } => request.to_domain().map(|_| ()),
            Self::ScanReady { record, .. } => record.to_domain().map(|_| ()),
            Self::ScanFailed { failure, .. } => failure.to_domain().map(|_| ()),
            Self::Instantiate { request } => request.validate(),
            Self::Instantiated { instance, tail, .. } => {
                instance.value()?;
                tail.to_domain();
                Ok(())
            }
            Self::BindSharedMemory { binding } => binding.to_domain().map(|_| ()),
            Self::SetParameters { request } => request.validate(),
            Self::Process {
                instance,
                frames,
                input_event_count,
                ..
            } => {
                instance.value()?;
                if *frames == 0 || *input_event_count > 1_000_000 {
                    return Err(WireError::Malformed("invalid process bounds".into()));
                }
                Ok(())
            }
            Self::SaveState { request } => request.validate(),
            Self::StateSaved { state, .. } => state.validate(),
            Self::TailChanged { instance, tail } => {
                instance.value()?;
                tail.to_domain();
                Ok(())
            }
            Self::Error { failure } => failure.validate(),
            Self::Bound { instance }
            | Self::Activate { instance }
            | Self::Activated { instance }
            | Self::ParametersSet { instance, .. }
            | Self::Processed { instance, .. }
            | Self::LatencyChanged { instance, .. }
            | Self::Deactivate { instance }
            | Self::Deactivated { instance }
            | Self::Destroy { instance }
            | Self::Destroyed { instance } => instance.value().map(|_| ()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    ControllerToWorker,
    WorkerToController,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenDto(String);

impl TokenDto {
    pub fn new(value: u128) -> Self {
        Self(format!("{value:032x}"))
    }

    pub fn value(&self) -> Result<u128, WireError> {
        if self.0.len() != 32
            || self
                .0
                .bytes()
                .any(|byte| !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte))
        {
            return Err(WireError::Malformed(
                "tokens must be 32 lowercase hexadecimal characters".into(),
            ));
        }
        u128::from_str_radix(&self.0, 16).map_err(|_| WireError::Malformed("invalid token".into()))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormatDto {
    Clap,
    Vst3,
}

impl TryFrom<&PluginFormat> for FormatDto {
    type Error = WireError;

    fn try_from(value: &PluginFormat) -> Result<Self, Self::Error> {
        match value {
            PluginFormat::Clap => Ok(Self::Clap),
            PluginFormat::Vst3 => Ok(Self::Vst3),
            other => Err(WireError::UnsupportedFormat(other.stable_name().into())),
        }
    }
}

impl From<FormatDto> for PluginFormat {
    fn from(value: FormatDto) -> Self {
        match value {
            FormatDto::Clap => Self::Clap,
            FormatDto::Vst3 => Self::Vst3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchitectureDto {
    Aarch64,
    X86_64,
    Universal,
    Other,
}

impl From<CpuArchitecture> for ArchitectureDto {
    fn from(value: CpuArchitecture) -> Self {
        match value {
            CpuArchitecture::Aarch64 => Self::Aarch64,
            CpuArchitecture::X86_64 => Self::X86_64,
            CpuArchitecture::Universal => Self::Universal,
            CpuArchitecture::Other => Self::Other,
        }
    }
}

impl From<ArchitectureDto> for CpuArchitecture {
    fn from(value: ArchitectureDto) -> Self {
        match value {
            ArchitectureDto::Aarch64 => Self::Aarch64,
            ArchitectureDto::X86_64 => Self::X86_64,
            ArchitectureDto::Universal => Self::Universal,
            ArchitectureDto::Other => Self::Other,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct PluginKeyDto {
    pub format: FormatDto,
    pub identifier: String,
}

impl PluginKeyDto {
    pub fn from_domain(value: &PluginKey) -> Result<Self, WireError> {
        value.validate().map_err(WireError::Domain)?;
        Ok(Self {
            format: FormatDto::try_from(&value.format)?,
            identifier: value.identifier.clone(),
        })
    }

    pub fn to_domain(&self) -> Result<PluginKey, WireError> {
        let value = PluginKey {
            format: self.format.into(),
            identifier: self.identifier.clone(),
        };
        value.validate().map_err(WireError::Domain)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "snake_case")]
pub enum ParameterKeyDto {
    Clap { id: u32 },
    Vst3 { id_hex: String },
}

impl ParameterKeyDto {
    pub fn from_domain(value: &PluginParameterKey) -> Result<Self, WireError> {
        match value {
            PluginParameterKey::Clap(id) => Ok(Self::Clap { id: *id }),
            PluginParameterKey::Vst3(id) => Ok(Self::Vst3 {
                id_hex: encode_hex(id),
            }),
            _ => Err(WireError::UnsupportedParameterKey),
        }
    }

    pub fn to_domain(&self) -> Result<PluginParameterKey, WireError> {
        match self {
            Self::Clap { id } => Ok(PluginParameterKey::Clap(*id)),
            Self::Vst3 { id_hex } => Ok(PluginParameterKey::Vst3(decode_hex_16(id_hex)?)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParameterValueDto {
    pub key: ParameterKeyDto,
    pub normalized: f64,
}

impl ParameterValueDto {
    pub fn to_domain(&self) -> Result<(PluginParameterKey, NormalizedValue), WireError> {
        Ok((
            self.key.to_domain()?,
            NormalizedValue::new(self.normalized).map_err(WireError::Domain)?,
        ))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TailDto {
    None,
    FiniteFrames { frames: u64 },
    Infinite,
    Unknown,
}

impl TailDto {
    pub fn from_domain(value: TailReport) -> Self {
        match value {
            TailReport::None => Self::None,
            TailReport::FiniteFrames(frames) => Self::FiniteFrames { frames },
            TailReport::Infinite => Self::Infinite,
            TailReport::Unknown => Self::Unknown,
        }
    }

    pub fn to_domain(&self) -> TailReport {
        match self {
            Self::None => TailReport::None,
            Self::FiniteFrames { frames } => TailReport::FiniteFrames(*frames),
            Self::Infinite => TailReport::Infinite,
            Self::Unknown => TailReport::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortDirectionDto {
    Input,
    Output,
}

impl From<PortDirection> for PortDirectionDto {
    fn from(value: PortDirection) -> Self {
        match value {
            PortDirection::Input => Self::Input,
            PortDirection::Output => Self::Output,
        }
    }
}

impl From<PortDirectionDto> for PortDirection {
    fn from(value: PortDirectionDto) -> Self {
        match value {
            PortDirectionDto::Input => Self::Input,
            PortDirectionDto::Output => Self::Output,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChannelLayoutDto {
    Mono,
    Stereo,
    Discrete { channels: u16 },
}

impl From<ChannelLayout> for ChannelLayoutDto {
    fn from(value: ChannelLayout) -> Self {
        match value {
            ChannelLayout::Mono => Self::Mono,
            ChannelLayout::Stereo => Self::Stereo,
            ChannelLayout::Discrete(channels) => Self::Discrete { channels },
        }
    }
}

impl From<ChannelLayoutDto> for ChannelLayout {
    fn from(value: ChannelLayoutDto) -> Self {
        match value {
            ChannelLayoutDto::Mono => Self::Mono,
            ChannelLayoutDto::Stereo => Self::Stereo,
            ChannelLayoutDto::Discrete { channels } => Self::Discrete(channels),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteDialectDto {
    Clap,
    Midi1,
    Midi2,
}

impl From<NoteDialect> for NoteDialectDto {
    fn from(value: NoteDialect) -> Self {
        match value {
            NoteDialect::Clap => Self::Clap,
            NoteDialect::Midi1 => Self::Midi1,
            NoteDialect::Midi2 => Self::Midi2,
        }
    }
}

impl From<NoteDialectDto> for NoteDialect {
    fn from(value: NoteDialectDto) -> Self {
        match value {
            NoteDialectDto::Clap => Self::Clap,
            NoteDialectDto::Midi1 => Self::Midi1,
            NoteDialectDto::Midi2 => Self::Midi2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NegotiatedPortDto {
    pub native_id: u32,
    pub direction: PortDirectionDto,
    pub layout: ChannelLayoutDto,
    pub channel_offset: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessingContractDto {
    pub sample_rate: u32,
    pub minimum_frames: u32,
    pub maximum_frames: u32,
    pub audio_ports: Vec<NegotiatedPortDto>,
    pub note_inputs: BTreeMap<u32, NoteDialectDto>,
    pub note_outputs: BTreeMap<u32, NoteDialectDto>,
    pub initial_latency_frames: u32,
    pub initial_tail: TailDto,
    pub offline: bool,
}

impl ProcessingContractDto {
    pub fn from_domain(value: &ProcessingContract) -> Result<Self, WireError> {
        value.validate().map_err(WireError::Domain)?;
        Ok(Self {
            sample_rate: value.sample_rate,
            minimum_frames: value.minimum_frames,
            maximum_frames: value.maximum_frames,
            audio_ports: value
                .audio_ports
                .iter()
                .map(|port| NegotiatedPortDto {
                    native_id: port.native_id,
                    direction: port.direction.into(),
                    layout: port.layout.into(),
                    channel_offset: port.channel_offset,
                })
                .collect(),
            note_inputs: value
                .note_inputs
                .iter()
                .map(|(id, dialect)| (*id, (*dialect).into()))
                .collect(),
            note_outputs: value
                .note_outputs
                .iter()
                .map(|(id, dialect)| (*id, (*dialect).into()))
                .collect(),
            initial_latency_frames: value.initial_latency_frames,
            initial_tail: TailDto::from_domain(value.initial_tail),
            offline: value.offline,
        })
    }

    pub fn to_domain(&self) -> Result<ProcessingContract, WireError> {
        let contract = ProcessingContract {
            sample_rate: self.sample_rate,
            minimum_frames: self.minimum_frames,
            maximum_frames: self.maximum_frames,
            audio_ports: self
                .audio_ports
                .iter()
                .map(|port| NegotiatedAudioPort {
                    native_id: port.native_id,
                    direction: port.direction.into(),
                    layout: port.layout.into(),
                    channel_offset: port.channel_offset,
                })
                .collect(),
            note_inputs: self
                .note_inputs
                .iter()
                .map(|(id, dialect)| (*id, (*dialect).into()))
                .collect(),
            note_outputs: self
                .note_outputs
                .iter()
                .map(|(id, dialect)| (*id, (*dialect).into()))
                .collect(),
            initial_latency_frames: self.initial_latency_frames,
            initial_tail: self.initial_tail.to_domain(),
            offline: self.offline,
        };
        contract.validate().map_err(WireError::Domain)?;
        Ok(contract)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateArtifactDto {
    pub plugin: PluginKeyDto,
    pub state_format_version: u32,
    pub sha256: String,
    pub byte_len: u64,
    pub relative_path: String,
}

impl StateArtifactDto {
    pub fn from_blob(blob: &PluginStateBlob, relative_path: String) -> Result<Self, WireError> {
        blob.validate(MAX_STATE_BYTES as usize)
            .map_err(WireError::Domain)?;
        if digest_bytes(&blob.bytes) != blob.digest {
            return Err(WireError::ArtifactMismatch("state SHA-256"));
        }
        validate_relative_path(&relative_path)?;
        Ok(Self {
            plugin: PluginKeyDto::from_domain(&blob.plugin)?,
            state_format_version: blob.state_format_version,
            sha256: blob.digest.to_hex(),
            byte_len: blob.bytes.len() as u64,
            relative_path,
        })
    }

    pub fn validate(&self) -> Result<(), WireError> {
        self.plugin.to_domain()?;
        canonical_digest(&self.sha256)?;
        validate_relative_path(&self.relative_path)?;
        if self.byte_len > MAX_STATE_BYTES {
            return Err(WireError::Malformed("state artifact exceeds limit".into()));
        }
        Ok(())
    }

    pub fn into_blob(&self, bytes: Vec<u8>) -> Result<PluginStateBlob, WireError> {
        self.validate()?;
        if bytes.len() as u64 != self.byte_len {
            return Err(WireError::ArtifactMismatch("state length"));
        }
        let expected = canonical_digest(&self.sha256)?;
        if digest_bytes(&bytes) != expected {
            return Err(WireError::ArtifactMismatch("state SHA-256"));
        }
        let blob = PluginStateBlob {
            plugin: self.plugin.to_domain()?,
            state_format_version: self.state_format_version,
            bytes,
            digest: expected,
        };
        blob.validate(MAX_STATE_BYTES as usize)
            .map_err(WireError::Domain)?;
        Ok(blob)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstantiateDto {
    pub request_id: u64,
    pub instance: TokenDto,
    pub artifact_lease: TokenDto,
    pub plugin: PluginKeyDto,
    pub contract: ProcessingContractDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<StateArtifactDto>,
}

impl InstantiateDto {
    fn validate(&self) -> Result<(), WireError> {
        if self.instance.value()? == 0 || self.artifact_lease.value()? == 0 {
            return Err(WireError::Malformed("zero instance or lease token".into()));
        }
        self.plugin.to_domain()?;
        self.contract.to_domain()?;
        if let Some(state) = &self.state {
            state.validate()?;
            if state.plugin != self.plugin {
                return Err(WireError::Malformed("state plugin mismatch".into()));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetParametersDto {
    pub request_id: u64,
    pub instance: TokenDto,
    pub values: Vec<ParameterValueDto>,
}

impl SetParametersDto {
    fn validate(&self) -> Result<(), WireError> {
        self.instance.value()?;
        if self.values.len() > 1_000_000 {
            return Err(WireError::Malformed("too many parameters".into()));
        }
        let mut keys = BTreeSet::new();
        for value in &self.values {
            let (key, _) = value.to_domain()?;
            if !keys.insert(key) {
                return Err(WireError::Malformed("duplicate parameter".into()));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedMemoryAccessDto {
    HostWrites,
    WorkerWrites,
}

impl From<SharedMemoryAccessDto> for SharedMemoryAccess {
    fn from(value: SharedMemoryAccessDto) -> Self {
        match value {
            SharedMemoryAccessDto::HostWrites => Self::HostWrites,
            SharedMemoryAccessDto::WorkerWrites => Self::WorkerWrites,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedMemoryRegionDto {
    pub token: TokenDto,
    pub byte_len: u64,
    pub access: SharedMemoryAccessDto,
}

impl SharedMemoryRegionDto {
    fn to_domain(&self) -> Result<SharedMemoryRegion, WireError> {
        Ok(SharedMemoryRegion {
            token: self.token.value()?,
            byte_len: self.byte_len,
            access: self.access.into(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedMemoryBindingDto {
    pub instance: TokenDto,
    pub audio_inputs: SharedMemoryRegionDto,
    pub audio_outputs: SharedMemoryRegionDto,
    pub events_to_worker: SharedMemoryRegionDto,
    pub events_from_worker: SharedMemoryRegionDto,
}

impl SharedMemoryBindingDto {
    fn to_domain(&self) -> Result<crate::plugin::WorkerRequest, WireError> {
        let request = crate::plugin::WorkerRequest::BindSharedMemory {
            instance: crate::plugin::InstanceToken(self.instance.value()?),
            audio_inputs: self.audio_inputs.to_domain()?,
            audio_outputs: self.audio_outputs.to_domain()?,
            events_to_worker: self.events_to_worker.to_domain()?,
            events_from_worker: self.events_from_worker.to_domain()?,
        };
        crate::plugin::validate_worker_request(&request).map_err(WireError::Domain)?;
        Ok(request)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveStateDto {
    pub request_id: u64,
    pub instance: TokenDto,
    pub maximum_bytes: u64,
    pub output_relative_path: String,
}

impl SaveStateDto {
    fn validate(&self) -> Result<(), WireError> {
        self.instance.value()?;
        validate_relative_path(&self.output_relative_path)?;
        if self.maximum_bytes == 0 || self.maximum_bytes > MAX_STATE_BYTES {
            return Err(WireError::Malformed("invalid state limit".into()));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilitiesDto {
    pub worker_name: String,
    pub worker_version: String,
    pub worker_build_sha256: String,
    pub formats: BTreeSet<FormatDto>,
    pub architectures: BTreeSet<ArchitectureDto>,
    pub scanning: bool,
    pub realtime: bool,
    pub offline: bool,
    pub shared_memory: bool,
    pub maximum_instances: u16,
}

impl CapabilitiesDto {
    fn validate(&self) -> Result<(), WireError> {
        validate_text("worker name", &self.worker_name)?;
        validate_text("worker version", &self.worker_version)?;
        canonical_digest(&self.worker_build_sha256)?;
        if self.formats.is_empty()
            || self.architectures.is_empty()
            || self.maximum_instances == 0
            || (!self.scanning && !self.realtime && !self.offline)
        {
            return Err(WireError::Malformed("invalid capabilities".into()));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanRequestDto {
    pub request_id: u64,
    pub candidate_path: String,
    pub timeout_millis: u64,
    pub maximum_descriptors: u32,
    pub maximum_parameters_per_plugin: u32,
}

impl ScanRequestDto {
    pub fn to_domain(&self) -> Result<crate::plugin::ScanRequest, WireError> {
        let request = crate::plugin::ScanRequest {
            protocol_version: WORKER_PROTOCOL_VERSION,
            request_id: self.request_id,
            candidate_path: PathBuf::from(&self.candidate_path),
            timeout_millis: self.timeout_millis,
            maximum_descriptors: self.maximum_descriptors,
            maximum_parameters_per_plugin: self.maximum_parameters_per_plugin,
        };
        request.validate().map_err(WireError::Domain)?;
        Ok(request)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDto {
    pub sha256: String,
    pub byte_len: u64,
    pub architectures: BTreeSet<ArchitectureDto>,
}

impl ArtifactDto {
    pub fn from_domain(value: &ArtifactFingerprint) -> Result<Self, WireError> {
        value.validate().map_err(WireError::Domain)?;
        Ok(Self {
            sha256: value.content.to_hex(),
            byte_len: value.byte_len,
            architectures: value
                .architectures
                .iter()
                .copied()
                .map(Into::into)
                .collect(),
        })
    }

    pub fn to_domain(&self) -> Result<ArtifactFingerprint, WireError> {
        let artifact = ArtifactFingerprint {
            content: canonical_digest(&self.sha256)?,
            byte_len: self.byte_len,
            architectures: self.architectures.iter().copied().map(Into::into).collect(),
        };
        artifact.validate().map_err(WireError::Domain)?;
        Ok(artifact)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScannerDto {
    pub name: String,
    pub version: String,
    pub build_sha256: String,
    pub host_os: String,
    pub host_architecture: ArchitectureDto,
    pub hash_algorithm: String,
}

impl ScannerDto {
    fn from_domain(value: &ScannerProvenance) -> Self {
        Self {
            name: value.scanner_name.clone(),
            version: value.scanner_version.clone(),
            build_sha256: value.scanner_build.to_hex(),
            host_os: value.host_os.clone(),
            host_architecture: value.host_architecture.into(),
            hash_algorithm: value.hash_algorithm.clone(),
        }
    }

    fn to_domain(&self) -> Result<ScannerProvenance, WireError> {
        Ok(ScannerProvenance {
            scanner_name: self.name.clone(),
            scanner_version: self.version.clone(),
            scanner_build: canonical_digest(&self.build_sha256)?,
            host_os: self.host_os.clone(),
            host_architecture: self.host_architecture.into(),
            hash_algorithm: self.hash_algorithm.clone(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleDto {
    AudioEffect,
    Instrument,
    NoteEffect,
    Analyzer,
    Utility,
}

impl From<PluginRole> for RoleDto {
    fn from(value: PluginRole) -> Self {
        match value {
            PluginRole::AudioEffect => Self::AudioEffect,
            PluginRole::Instrument => Self::Instrument,
            PluginRole::NoteEffect => Self::NoteEffect,
            PluginRole::Analyzer => Self::Analyzer,
            PluginRole::Utility => Self::Utility,
        }
    }
}

impl From<RoleDto> for PluginRole {
    fn from(value: RoleDto) -> Self {
        match value {
            RoleDto::AudioEffect => Self::AudioEffect,
            RoleDto::Instrument => Self::Instrument,
            RoleDto::NoteEffect => Self::NoteEffect,
            RoleDto::Analyzer => Self::Analyzer,
            RoleDto::Utility => Self::Utility,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioPortRoleDto {
    Main,
    Auxiliary,
    Sidechain,
}

impl From<AudioPortRole> for AudioPortRoleDto {
    fn from(value: AudioPortRole) -> Self {
        match value {
            AudioPortRole::Main => Self::Main,
            AudioPortRole::Auxiliary => Self::Auxiliary,
            AudioPortRole::Sidechain => Self::Sidechain,
        }
    }
}

impl From<AudioPortRoleDto> for AudioPortRole {
    fn from(value: AudioPortRoleDto) -> Self {
        match value {
            AudioPortRoleDto::Main => Self::Main,
            AudioPortRoleDto::Auxiliary => Self::Auxiliary,
            AudioPortRoleDto::Sidechain => Self::Sidechain,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioPortDto {
    pub native_id: u32,
    pub name: String,
    pub direction: PortDirectionDto,
    pub role: AudioPortRoleDto,
    pub layouts: Vec<ChannelLayoutDto>,
    pub required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotePortDto {
    pub native_id: u32,
    pub name: String,
    pub direction: PortDirectionDto,
    pub dialects: BTreeSet<NoteDialectDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred: Option<NoteDialectDto>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParameterMappingDto {
    Linear,
    Logarithmic,
    Stepped { steps: u32 },
    Enumerated { labels: Vec<String> },
}

impl ParameterMappingDto {
    fn from_domain(value: &ParameterMapping) -> Self {
        match value {
            ParameterMapping::Linear => Self::Linear,
            ParameterMapping::Logarithmic => Self::Logarithmic,
            ParameterMapping::Stepped { steps } => Self::Stepped { steps: *steps },
            ParameterMapping::Enumerated(labels) => Self::Enumerated {
                labels: labels.clone(),
            },
        }
    }

    fn to_domain(&self) -> ParameterMapping {
        match self {
            Self::Linear => ParameterMapping::Linear,
            Self::Logarithmic => ParameterMapping::Logarithmic,
            Self::Stepped { steps } => ParameterMapping::Stepped { steps: *steps },
            Self::Enumerated { labels } => ParameterMapping::Enumerated(labels.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParameterDescriptorDto {
    pub key: ParameterKeyDto,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    pub plain_min: f64,
    pub plain_max: f64,
    pub plain_default: f64,
    pub mapping: ParameterMappingDto,
    pub automatable: bool,
    pub modulatable: bool,
    pub read_only: bool,
    pub hidden: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeterminismDto {
    Deterministic,
    Seeded,
    NotGuaranteed,
    Unknown,
}

impl From<DeterminismClass> for DeterminismDto {
    fn from(value: DeterminismClass) -> Self {
        match value {
            DeterminismClass::Deterministic => Self::Deterministic,
            DeterminismClass::Seeded => Self::Seeded,
            DeterminismClass::NotGuaranteed => Self::NotGuaranteed,
            DeterminismClass::Unknown => Self::Unknown,
        }
    }
}

impl From<DeterminismDto> for DeterminismClass {
    fn from(value: DeterminismDto) -> Self {
        match value {
            DeterminismDto::Deterministic => Self::Deterministic,
            DeterminismDto::Seeded => Self::Seeded,
            DeterminismDto::NotGuaranteed => Self::NotGuaranteed,
            DeterminismDto::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionDto {
    pub realtime_safe_claimed: bool,
    pub hard_realtime_required: bool,
    pub offline_processing: bool,
    pub editor: bool,
    pub state: bool,
    pub latency_reporting: bool,
    pub tail_reporting: bool,
    pub determinism: DeterminismDto,
}

impl From<ExecutionCapabilities> for ExecutionDto {
    fn from(value: ExecutionCapabilities) -> Self {
        Self {
            realtime_safe_claimed: value.realtime_safe_claimed,
            hard_realtime_required: value.hard_realtime_required,
            offline_processing: value.offline_processing,
            editor: value.editor,
            state: value.state,
            latency_reporting: value.latency_reporting,
            tail_reporting: value.tail_reporting,
            determinism: value.determinism.into(),
        }
    }
}

impl From<ExecutionDto> for ExecutionCapabilities {
    fn from(value: ExecutionDto) -> Self {
        Self {
            realtime_safe_claimed: value.realtime_safe_claimed,
            hard_realtime_required: value.hard_realtime_required,
            offline_processing: value.offline_processing,
            editor: value.editor,
            state: value.state,
            latency_reporting: value.latency_reporting,
            tail_reporting: value.tail_reporting,
            determinism: value.determinism.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginDescriptorDto {
    pub key: PluginKeyDto,
    pub name: String,
    pub vendor: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub features: BTreeSet<String>,
    pub roles: BTreeSet<RoleDto>,
    pub audio_ports: Vec<AudioPortDto>,
    pub note_ports: Vec<NotePortDto>,
    pub parameters: Vec<ParameterDescriptorDto>,
    pub execution: ExecutionDto,
}

impl PluginDescriptorDto {
    pub fn from_domain(value: &PluginMetadata) -> Result<Self, WireError> {
        value.validate().map_err(WireError::Domain)?;
        Ok(Self {
            key: PluginKeyDto::from_domain(&value.key)?,
            name: value.name.clone(),
            vendor: value.vendor.clone(),
            version: value.version.clone(),
            description: value.description.clone(),
            homepage: value.homepage.clone(),
            features: value.features.clone(),
            roles: value.roles.iter().copied().map(Into::into).collect(),
            audio_ports: value
                .audio_ports
                .iter()
                .map(|port| AudioPortDto {
                    native_id: port.native_id,
                    name: port.name.clone(),
                    direction: port.direction.into(),
                    role: port.role.into(),
                    layouts: port.layouts.iter().copied().map(Into::into).collect(),
                    required: port.required,
                })
                .collect(),
            note_ports: value
                .note_ports
                .iter()
                .map(|port| NotePortDto {
                    native_id: port.native_id,
                    name: port.name.clone(),
                    direction: port.direction.into(),
                    dialects: port.dialects.iter().copied().map(Into::into).collect(),
                    preferred: port.preferred.map(Into::into),
                })
                .collect(),
            parameters: value
                .parameters
                .iter()
                .map(|parameter| {
                    Ok(ParameterDescriptorDto {
                        key: ParameterKeyDto::from_domain(&parameter.key)?,
                        name: parameter.name.clone(),
                        module: parameter.module.clone(),
                        unit: parameter.unit.clone(),
                        plain_min: parameter.plain_min,
                        plain_max: parameter.plain_max,
                        plain_default: parameter.plain_default,
                        mapping: ParameterMappingDto::from_domain(&parameter.mapping),
                        automatable: parameter.automatable,
                        modulatable: parameter.modulatable,
                        read_only: parameter.read_only,
                        hidden: parameter.hidden,
                    })
                })
                .collect::<Result<_, WireError>>()?,
            execution: value.capabilities.into(),
        })
    }

    pub fn to_domain(&self) -> Result<PluginMetadata, WireError> {
        let metadata = PluginMetadata {
            key: self.key.to_domain()?,
            name: self.name.clone(),
            vendor: self.vendor.clone(),
            version: self.version.clone(),
            description: self.description.clone(),
            homepage: self.homepage.clone(),
            features: self.features.clone(),
            roles: self.roles.iter().copied().map(Into::into).collect(),
            audio_ports: self
                .audio_ports
                .iter()
                .map(|port| AudioPortDescriptor {
                    native_id: port.native_id,
                    name: port.name.clone(),
                    direction: port.direction.into(),
                    role: port.role.into(),
                    layouts: port.layouts.iter().copied().map(Into::into).collect(),
                    required: port.required,
                })
                .collect(),
            note_ports: self
                .note_ports
                .iter()
                .map(|port| NotePortDescriptor {
                    native_id: port.native_id,
                    name: port.name.clone(),
                    direction: port.direction.into(),
                    dialects: port.dialects.iter().copied().map(Into::into).collect(),
                    preferred: port.preferred.map(Into::into),
                })
                .collect(),
            parameters: self
                .parameters
                .iter()
                .map(|parameter| {
                    Ok(PluginParameterDescriptor {
                        key: parameter.key.to_domain()?,
                        name: parameter.name.clone(),
                        module: parameter.module.clone(),
                        unit: parameter.unit.clone(),
                        plain_min: parameter.plain_min,
                        plain_max: parameter.plain_max,
                        plain_default: parameter.plain_default,
                        mapping: parameter.mapping.to_domain(),
                        automatable: parameter.automatable,
                        modulatable: parameter.modulatable,
                        read_only: parameter.read_only,
                        hidden: parameter.hidden,
                    })
                })
                .collect::<Result<_, WireError>>()?,
            capabilities: self.execution.into(),
        };
        metadata.validate().map_err(WireError::Domain)?;
        Ok(metadata)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScanRecordDto {
    pub schema_version: u32,
    pub canonical_path: String,
    pub artifact: ArtifactDto,
    pub scanner: ScannerDto,
    pub plugins: Vec<PluginDescriptorDto>,
}

impl ScanRecordDto {
    pub fn from_domain(value: &ScanRecord) -> Result<Self, WireError> {
        value.validate().map_err(WireError::Domain)?;
        Ok(Self {
            schema_version: value.schema_version,
            canonical_path: path_to_string(&value.canonical_path)?,
            artifact: ArtifactDto::from_domain(&value.artifact)?,
            scanner: ScannerDto::from_domain(&value.scanner),
            plugins: value
                .plugins
                .iter()
                .map(PluginDescriptorDto::from_domain)
                .collect::<Result<_, _>>()?,
        })
    }

    pub fn to_domain(&self) -> Result<ScanRecord, WireError> {
        let mut record = ScanRecord {
            schema_version: self.schema_version,
            canonical_path: PathBuf::from(&self.canonical_path),
            artifact: self.artifact.to_domain()?,
            scanner: self.scanner.to_domain()?,
            plugins: self
                .plugins
                .iter()
                .map(PluginDescriptorDto::to_domain)
                .collect::<Result<_, _>>()?,
        };
        record.validate().map_err(WireError::Domain)?;
        record.canonicalize_order();
        Ok(record)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanFailureKindDto {
    TimedOut,
    Crashed,
    InvalidAbi,
    InvalidDescriptor,
    PermissionDenied,
    UnsupportedArchitecture,
    Io,
}

impl From<ScanFailureKind> for ScanFailureKindDto {
    fn from(value: ScanFailureKind) -> Self {
        match value {
            ScanFailureKind::TimedOut => Self::TimedOut,
            ScanFailureKind::Crashed => Self::Crashed,
            ScanFailureKind::InvalidAbi => Self::InvalidAbi,
            ScanFailureKind::InvalidDescriptor => Self::InvalidDescriptor,
            ScanFailureKind::PermissionDenied => Self::PermissionDenied,
            ScanFailureKind::UnsupportedArchitecture => Self::UnsupportedArchitecture,
            ScanFailureKind::Io => Self::Io,
        }
    }
}

impl From<ScanFailureKindDto> for ScanFailureKind {
    fn from(value: ScanFailureKindDto) -> Self {
        match value {
            ScanFailureKindDto::TimedOut => Self::TimedOut,
            ScanFailureKindDto::Crashed => Self::Crashed,
            ScanFailureKindDto::InvalidAbi => Self::InvalidAbi,
            ScanFailureKindDto::InvalidDescriptor => Self::InvalidDescriptor,
            ScanFailureKindDto::PermissionDenied => Self::PermissionDenied,
            ScanFailureKindDto::UnsupportedArchitecture => Self::UnsupportedArchitecture,
            ScanFailureKindDto::Io => Self::Io,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanFailureDto {
    pub kind: ScanFailureKindDto,
    pub detail: String,
    pub scanner: ScannerDto,
}

impl ScanFailureDto {
    pub fn from_domain(value: &ScanFailure) -> Result<Self, WireError> {
        validate_text("scan failure", &value.detail)?;
        Ok(Self {
            kind: value.kind.clone().into(),
            detail: value.detail.clone(),
            scanner: ScannerDto::from_domain(&value.scanner),
        })
    }

    pub fn to_domain(&self) -> Result<ScanFailure, WireError> {
        validate_text("scan failure", &self.detail)?;
        Ok(ScanFailure {
            kind: self.kind.into(),
            detail: self.detail.clone(),
            scanner: self.scanner.to_domain()?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeFailureKindDto {
    InvalidRequest,
    MissingLease,
    InvalidState,
    InvalidLifecycle,
    Backend,
    Io,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeFailureDto {
    pub request_id: Option<u64>,
    pub instance: Option<TokenDto>,
    pub kind: RuntimeFailureKindDto,
    pub recoverable: bool,
    pub detail: String,
}

impl RuntimeFailureDto {
    fn validate(&self) -> Result<(), WireError> {
        if let Some(instance) = &self.instance {
            instance.value()?;
        }
        validate_text("runtime failure", &self.detail)
    }
}

#[derive(Clone, Debug, Default)]
pub struct SessionValidator {
    controller_sequence: u64,
    worker_sequence: u64,
    handshake: Handshake,
    pending: BTreeMap<u64, Pending>,
    instances: BTreeMap<TokenDto, InstanceState>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Handshake {
    #[default]
    Fresh,
    Waiting,
    Ready,
    Closed,
}

#[derive(Clone, Debug)]
enum Pending {
    Scan(String),
    Instantiate(TokenDto),
    Parameters(TokenDto),
    State(TokenDto),
}

#[derive(Clone, Debug)]
struct InstanceState {
    plugin: PluginKeyDto,
    phase: Phase,
    next_process: u64,
    pending_process: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Instantiating,
    Instantiated,
    Binding,
    Bound,
    Activating,
    Active,
    Deactivating,
    Inactive,
    Destroying,
}

impl SessionValidator {
    pub fn observe_controller(&mut self, envelope: &Envelope) -> Result<(), WireError> {
        envelope.validate()?;
        if envelope.message.direction() != Direction::ControllerToWorker {
            return Err(WireError::WrongDirection);
        }
        check_sequence("controller", envelope.sequence, self.controller_sequence)?;
        self.controller_transition(&envelope.message)?;
        self.controller_sequence += 1;
        Ok(())
    }

    pub fn observe_worker(&mut self, envelope: &Envelope) -> Result<(), WireError> {
        envelope.validate()?;
        if envelope.message.direction() != Direction::WorkerToController {
            return Err(WireError::WrongDirection);
        }
        check_sequence("worker", envelope.sequence, self.worker_sequence)?;
        self.worker_transition(&envelope.message)?;
        self.worker_sequence += 1;
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.handshake == Handshake::Closed
    }

    fn controller_transition(&mut self, message: &Message) -> Result<(), WireError> {
        match message {
            Message::Hello if self.handshake == Handshake::Fresh => {
                self.handshake = Handshake::Waiting;
                return Ok(());
            }
            Message::Shutdown
                if self.handshake == Handshake::Ready
                    && self.pending.is_empty()
                    && self.instances.is_empty() =>
            {
                self.handshake = Handshake::Closed;
                return Ok(());
            }
            _ => {}
        }
        if self.handshake != Handshake::Ready {
            return Err(WireError::InvalidTransition("handshake is incomplete"));
        }
        match message {
            Message::Scan { request } => self.reserve(
                request.request_id,
                Pending::Scan(request.candidate_path.clone()),
            ),
            Message::Instantiate { request } => {
                if self.instances.contains_key(&request.instance) {
                    return Err(WireError::InvalidTransition("duplicate instance token"));
                }
                self.reserve(
                    request.request_id,
                    Pending::Instantiate(request.instance.clone()),
                )?;
                self.instances.insert(
                    request.instance.clone(),
                    InstanceState {
                        plugin: request.plugin.clone(),
                        phase: Phase::Instantiating,
                        next_process: 0,
                        pending_process: None,
                    },
                );
                Ok(())
            }
            Message::BindSharedMemory { binding } => {
                let state = self.instance_mut(&binding.instance)?;
                require_phase(state.phase, Phase::Instantiated)?;
                state.phase = Phase::Binding;
                Ok(())
            }
            Message::Activate { instance } => {
                let state = self.instance_mut(instance)?;
                if !matches!(state.phase, Phase::Bound | Phase::Inactive) {
                    return Err(WireError::InvalidTransition("instance is not activatable"));
                }
                state.phase = Phase::Activating;
                Ok(())
            }
            Message::SetParameters { request } => {
                let phase = self.instance(&request.instance)?.phase;
                if !matches!(phase, Phase::Bound | Phase::Active | Phase::Inactive) {
                    return Err(WireError::InvalidTransition(
                        "instance is not parameter-ready",
                    ));
                }
                self.reserve(
                    request.request_id,
                    Pending::Parameters(request.instance.clone()),
                )
            }
            Message::Process {
                instance,
                process_sequence,
                ..
            } => {
                let state = self.instance_mut(instance)?;
                require_phase(state.phase, Phase::Active)?;
                if state.pending_process.is_some() || *process_sequence != state.next_process {
                    return Err(WireError::InvalidTransition("invalid process sequence"));
                }
                state.pending_process = Some(*process_sequence);
                Ok(())
            }
            Message::SaveState { request } => {
                let phase = self.instance(&request.instance)?.phase;
                if !matches!(phase, Phase::Bound | Phase::Active | Phase::Inactive) {
                    return Err(WireError::InvalidTransition("instance cannot save state"));
                }
                self.reserve(request.request_id, Pending::State(request.instance.clone()))
            }
            Message::Deactivate { instance } => {
                let state = self.instance_mut(instance)?;
                require_phase(state.phase, Phase::Active)?;
                if state.pending_process.is_some() {
                    return Err(WireError::InvalidTransition("process is still pending"));
                }
                state.phase = Phase::Deactivating;
                Ok(())
            }
            Message::Destroy { instance } => {
                let state = self.instance_mut(instance)?;
                if !matches!(
                    state.phase,
                    Phase::Instantiated | Phase::Bound | Phase::Inactive
                ) {
                    return Err(WireError::InvalidTransition("instance is not destroyable"));
                }
                state.phase = Phase::Destroying;
                Ok(())
            }
            _ => Err(WireError::InvalidTransition(
                "invalid controller transition",
            )),
        }
    }

    fn worker_transition(&mut self, message: &Message) -> Result<(), WireError> {
        if let Message::Capabilities { .. } = message {
            if self.handshake == Handshake::Waiting {
                self.handshake = Handshake::Ready;
                return Ok(());
            }
        }
        if self.handshake != Handshake::Ready {
            return Err(WireError::InvalidTransition(
                "unexpected handshake response",
            ));
        }
        match message {
            Message::ScanReady { request_id, record } => {
                let Pending::Scan(path) = self.take(*request_id)? else {
                    return Err(WireError::InvalidTransition("scan response mismatch"));
                };
                if path != record.canonical_path {
                    return Err(WireError::InvalidTransition("scan path mismatch"));
                }
                Ok(())
            }
            Message::ScanFailed { request_id, .. } => {
                if !matches!(self.take(*request_id)?, Pending::Scan(_)) {
                    return Err(WireError::InvalidTransition("scan response mismatch"));
                }
                Ok(())
            }
            Message::Instantiated {
                request_id,
                instance,
                ..
            } => {
                let Pending::Instantiate(expected) = self.take(*request_id)? else {
                    return Err(WireError::InvalidTransition(
                        "instantiate response mismatch",
                    ));
                };
                if expected != *instance {
                    return Err(WireError::InvalidTransition("instance token mismatch"));
                }
                let state = self.instance_mut(instance)?;
                require_phase(state.phase, Phase::Instantiating)?;
                state.phase = Phase::Instantiated;
                Ok(())
            }
            Message::Bound { instance } => {
                let state = self.instance_mut(instance)?;
                require_phase(state.phase, Phase::Binding)?;
                state.phase = Phase::Bound;
                Ok(())
            }
            Message::Activated { instance } => {
                let state = self.instance_mut(instance)?;
                require_phase(state.phase, Phase::Activating)?;
                state.phase = Phase::Active;
                Ok(())
            }
            Message::ParametersSet {
                request_id,
                instance,
            } => {
                let Pending::Parameters(expected) = self.take(*request_id)? else {
                    return Err(WireError::InvalidTransition("parameter response mismatch"));
                };
                if expected != *instance {
                    return Err(WireError::InvalidTransition("instance token mismatch"));
                }
                Ok(())
            }
            Message::Processed {
                instance,
                process_sequence,
                ..
            } => {
                let state = self.instance_mut(instance)?;
                require_phase(state.phase, Phase::Active)?;
                if state.pending_process != Some(*process_sequence) {
                    return Err(WireError::InvalidTransition("process completion mismatch"));
                }
                state.pending_process = None;
                state.next_process += 1;
                Ok(())
            }
            Message::StateSaved { request_id, state } => {
                let Pending::State(instance) = self.take(*request_id)? else {
                    return Err(WireError::InvalidTransition("state response mismatch"));
                };
                if self.instance(&instance)?.plugin != state.plugin {
                    return Err(WireError::InvalidTransition("state plugin mismatch"));
                }
                Ok(())
            }
            Message::LatencyChanged { instance, .. } | Message::TailChanged { instance, .. } => {
                self.instance(instance)?;
                Ok(())
            }
            Message::Deactivated { instance } => {
                let state = self.instance_mut(instance)?;
                require_phase(state.phase, Phase::Deactivating)?;
                state.phase = Phase::Inactive;
                Ok(())
            }
            Message::Destroyed { instance } => {
                require_phase(self.instance(instance)?.phase, Phase::Destroying)?;
                self.instances.remove(instance);
                Ok(())
            }
            Message::Error { failure } => {
                if let Some(request_id) = failure.request_id {
                    let pending = self.take(request_id)?;
                    if let Pending::Instantiate(instance) = pending {
                        self.instances.remove(&instance);
                    }
                }
                if !failure.recoverable {
                    if let Some(instance) = &failure.instance {
                        self.instances.remove(instance);
                    }
                }
                Ok(())
            }
            _ => Err(WireError::InvalidTransition("invalid worker transition")),
        }
    }

    fn reserve(&mut self, id: u64, pending: Pending) -> Result<(), WireError> {
        if self.pending.insert(id, pending).is_some() {
            Err(WireError::InvalidTransition("duplicate pending request ID"))
        } else {
            Ok(())
        }
    }

    fn take(&mut self, id: u64) -> Result<Pending, WireError> {
        self.pending
            .remove(&id)
            .ok_or(WireError::InvalidTransition("unknown request ID"))
    }

    fn instance(&self, token: &TokenDto) -> Result<&InstanceState, WireError> {
        self.instances
            .get(token)
            .ok_or(WireError::InvalidTransition("unknown instance"))
    }

    fn instance_mut(&mut self, token: &TokenDto) -> Result<&mut InstanceState, WireError> {
        self.instances
            .get_mut(token)
            .ok_or(WireError::InvalidTransition("unknown instance"))
    }
}

fn require_phase(actual: Phase, expected: Phase) -> Result<(), WireError> {
    if actual == expected {
        Ok(())
    } else {
        Err(WireError::InvalidTransition("instance lifecycle mismatch"))
    }
}

fn check_sequence(direction: &'static str, actual: u64, expected: u64) -> Result<(), WireError> {
    if actual == expected {
        Ok(())
    } else {
        Err(WireError::Sequence {
            direction,
            actual,
            expected,
        })
    }
}

pub fn validate_relative_path(value: &str) -> Result<(), WireError> {
    if value.is_empty()
        || value.len() > MAX_TEXT_BYTES
        || value.contains('\0')
        || value.contains('\n')
        || value.contains('\r')
    {
        return Err(WireError::Malformed("invalid session-relative path".into()));
    }
    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(WireError::Malformed(
            "state path escapes the worker session".into(),
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub enum WireError {
    Encode(serde_json::Error),
    Decode(serde_json::Error),
    Domain(crate::plugin::PluginValidationError),
    ProtocolVersion {
        actual: u32,
        expected: u32,
    },
    Sequence {
        direction: &'static str,
        actual: u64,
        expected: u64,
    },
    WrongDirection,
    InvalidTransition(&'static str),
    UnsupportedFormat(String),
    UnsupportedParameterKey,
    ArtifactMismatch(&'static str),
    Malformed(String),
}

impl fmt::Display for WireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode(error) => write!(formatter, "plugin wire encode failed: {error}"),
            Self::Decode(error) => write!(formatter, "plugin wire decode failed: {error}"),
            Self::Domain(error) => error.fmt(formatter),
            Self::ProtocolVersion { actual, expected } => {
                write!(formatter, "protocol version {actual}, expected {expected}")
            }
            Self::Sequence {
                direction,
                actual,
                expected,
            } => write!(
                formatter,
                "{direction} sequence {actual}, expected {expected}"
            ),
            Self::WrongDirection => formatter.write_str("wire message has wrong direction"),
            Self::InvalidTransition(detail) | Self::ArtifactMismatch(detail) => {
                formatter.write_str(detail)
            }
            Self::UnsupportedFormat(format) => write!(formatter, "unsupported format {format}"),
            Self::UnsupportedParameterKey => {
                formatter.write_str("parameter key is not CLAP or VST3")
            }
            Self::Malformed(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for WireError {}

fn validate_text(field: &str, value: &str) -> Result<(), WireError> {
    if value.is_empty() || value.len() > MAX_TEXT_BYTES || value.chars().any(char::is_control) {
        Err(WireError::Malformed(format!("invalid {field}")))
    } else {
        Ok(())
    }
}

fn canonical_digest(value: &str) -> Result<Digest32, WireError> {
    let digest = Digest32::from_hex(value).map_err(WireError::Domain)?;
    if value != digest.to_hex() {
        return Err(WireError::Malformed("non-canonical SHA-256".into()));
    }
    Ok(digest)
}

fn path_to_string(path: &Path) -> Result<String, WireError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| WireError::Malformed("plugin path is not UTF-8".into()))
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

fn decode_hex_16(value: &str) -> Result<[u8; 16], WireError> {
    if value.len() != 32 {
        return Err(WireError::Malformed("invalid VST3 parameter ID".into()));
    }
    let mut bytes = [0; 16];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    if value != encode_hex(&bytes) {
        return Err(WireError::Malformed(
            "non-canonical VST3 parameter ID".into(),
        ));
    }
    Ok(bytes)
}

fn nibble(value: u8) -> Result<u8, WireError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(WireError::Malformed("invalid lowercase hexadecimal".into())),
    }
}

/// Dependency-free SHA-256 used for artifact/state identity on both sides of
/// this worker boundary.
pub fn digest_bytes(input: &[u8]) -> Digest32 {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut padded = input.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    let mut state = INITIAL;
    for block in padded.chunks_exact(64) {
        let mut words = [0u32; 64];
        for (index, word) in block.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut digest = [0; 32];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    Digest32(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin() -> PluginKeyDto {
        PluginKeyDto {
            format: FormatDto::Clap,
            identifier: "org.audec.fixture.gain".into(),
        }
    }

    fn contract() -> ProcessingContractDto {
        ProcessingContractDto {
            sample_rate: 48_000,
            minimum_frames: 1,
            maximum_frames: 512,
            audio_ports: vec![],
            note_inputs: BTreeMap::new(),
            note_outputs: BTreeMap::new(),
            initial_latency_frames: 16,
            initial_tail: TailDto::FiniteFrames { frames: 64 },
            offline: false,
        }
    }

    fn capabilities() -> CapabilitiesDto {
        CapabilitiesDto {
            worker_name: "audec-fake-plugin".into(),
            worker_version: "1".into(),
            worker_build_sha256: digest_bytes(b"worker").to_hex(),
            formats: BTreeSet::from([FormatDto::Clap, FormatDto::Vst3]),
            architectures: BTreeSet::from([ArchitectureDto::Aarch64]),
            scanning: true,
            realtime: true,
            offline: true,
            shared_memory: true,
            maximum_instances: 8,
        }
    }

    fn region(token: u128, worker_writes: bool) -> SharedMemoryRegionDto {
        SharedMemoryRegionDto {
            token: TokenDto::new(token),
            byte_len: 4096,
            access: if worker_writes {
                SharedMemoryAccessDto::WorkerWrites
            } else {
                SharedMemoryAccessDto::HostWrites
            },
        }
    }

    #[test]
    fn digest_and_native_parameter_keys_are_canonical() {
        assert_eq!(
            digest_bytes(b"").to_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let key = PluginParameterKey::Vst3([0xab; 16]);
        assert_eq!(
            ParameterKeyDto::from_domain(&key)
                .unwrap()
                .to_domain()
                .unwrap(),
            key
        );
    }

    #[test]
    fn state_artifact_rejects_tampering() {
        let bytes = b"opaque-state".to_vec();
        let blob = PluginStateBlob {
            plugin: plugin().to_domain().unwrap(),
            state_format_version: 1,
            digest: digest_bytes(&bytes),
            bytes: bytes.clone(),
        };
        let artifact = StateArtifactDto::from_blob(&blob, "state.bin".into()).unwrap();
        assert_eq!(artifact.into_blob(bytes).unwrap(), blob);
        assert!(matches!(
            artifact.into_blob(b"tampered-stat".to_vec()),
            Err(WireError::ArtifactMismatch(_))
        ));
    }

    #[test]
    fn lifecycle_gates_process_sequence_and_state_roundtrip() {
        let instance = TokenDto::new(1);
        let mut session = SessionValidator::default();
        let mut controller = 0;
        let mut worker = 0;
        let send_controller = |session: &mut SessionValidator, sequence: &mut u64, message| {
            session
                .observe_controller(&Envelope::new(*sequence, message))
                .unwrap();
            *sequence += 1;
        };
        let send_worker = |session: &mut SessionValidator, sequence: &mut u64, message| {
            session
                .observe_worker(&Envelope::new(*sequence, message))
                .unwrap();
            *sequence += 1;
        };

        send_controller(&mut session, &mut controller, Message::Hello);
        send_worker(
            &mut session,
            &mut worker,
            Message::Capabilities {
                capabilities: capabilities(),
            },
        );
        send_controller(
            &mut session,
            &mut controller,
            Message::Instantiate {
                request: InstantiateDto {
                    request_id: 10,
                    instance: instance.clone(),
                    artifact_lease: TokenDto::new(2),
                    plugin: plugin(),
                    contract: contract(),
                    state: None,
                },
            },
        );
        send_worker(
            &mut session,
            &mut worker,
            Message::Instantiated {
                request_id: 10,
                instance: instance.clone(),
                latency_frames: 16,
                tail: TailDto::FiniteFrames { frames: 64 },
            },
        );
        send_controller(
            &mut session,
            &mut controller,
            Message::BindSharedMemory {
                binding: SharedMemoryBindingDto {
                    instance: instance.clone(),
                    audio_inputs: region(3, false),
                    audio_outputs: region(4, true),
                    events_to_worker: region(5, false),
                    events_from_worker: region(6, true),
                },
            },
        );
        send_worker(
            &mut session,
            &mut worker,
            Message::Bound {
                instance: instance.clone(),
            },
        );
        send_controller(
            &mut session,
            &mut controller,
            Message::Activate {
                instance: instance.clone(),
            },
        );
        send_worker(
            &mut session,
            &mut worker,
            Message::Activated {
                instance: instance.clone(),
            },
        );
        send_controller(
            &mut session,
            &mut controller,
            Message::Process {
                instance: instance.clone(),
                process_sequence: 0,
                frames: 128,
                input_event_count: 0,
            },
        );
        assert!(session
            .observe_controller(&Envelope::new(
                controller,
                Message::Process {
                    instance: instance.clone(),
                    process_sequence: 1,
                    frames: 128,
                    input_event_count: 0,
                },
            ))
            .is_err());
        send_worker(
            &mut session,
            &mut worker,
            Message::Processed {
                instance: instance.clone(),
                process_sequence: 0,
                output_event_count: 0,
            },
        );
        send_controller(
            &mut session,
            &mut controller,
            Message::Deactivate {
                instance: instance.clone(),
            },
        );
        send_worker(
            &mut session,
            &mut worker,
            Message::Deactivated {
                instance: instance.clone(),
            },
        );
        send_controller(
            &mut session,
            &mut controller,
            Message::Destroy {
                instance: instance.clone(),
            },
        );
        send_worker(&mut session, &mut worker, Message::Destroyed { instance });
        send_controller(&mut session, &mut controller, Message::Shutdown);
        assert!(session.is_closed());
    }
}
