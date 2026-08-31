//! JSONL wire types for isolated model workers.
//!
//! This is intentionally a transport module, not an adapter API.  It gives
//! the schema-v1 types in `model_worker` an executable, deterministic JSONL
//! representation while keeping bulk data in supervisor-created job files.
//! No PCM, tensor, mask, or model bytes may appear in a wire record.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use crate::model_worker::{ContentHash, PROTOCOL_VERSION};

/// A single newline-delimited protocol record. Sequence numbers are scoped to
/// one direction of one worker process and must advance by exactly one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireEnvelope {
    pub protocol_version: u32,
    pub sequence: u64,
    #[serde(flatten)]
    pub message: WireMessage,
}

impl WireEnvelope {
    pub fn new(sequence: u64, message: WireMessage) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            sequence,
            message,
        }
    }

    pub fn to_jsonl(&self) -> Result<String, WireError> {
        self.validate_shape()?;
        let mut result = serde_json::to_string(self).map_err(WireError::Encode)?;
        result.push('\n');
        Ok(result)
    }

    pub fn from_jsonl(line: &str) -> Result<Self, WireError> {
        if line.contains('\n') || line.contains('\r') {
            return Err(WireError::MalformedRecord(
                "a JSONL decoder accepts exactly one line".into(),
            ));
        }
        let envelope: Self = serde_json::from_str(line).map_err(WireError::Decode)?;
        envelope.validate_shape()?;
        Ok(envelope)
    }

    pub fn validate_shape(&self) -> Result<(), WireError> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(WireError::ProtocolVersion {
                actual: self.protocol_version,
                expected: PROTOCOL_VERSION,
            });
        }
        self.message.validate_shape()
    }
}

/// The `kind` spelling is intentionally the vocabulary documented in
/// ML_MODELS.md. `analyze` is the neutral spelling; a schema-v1 controller
/// may send the same payload under `separate` during migration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireMessage {
    Hello,
    Capabilities {
        capabilities: WireCapabilities,
    },
    LoadModel {
        manifest_sha256: String,
    },
    ModelLoaded {
        manifest_sha256: String,
    },
    Analyze {
        request: AnalyzeRequest,
    },
    /// Compatibility spelling for existing `WorkerRequest::Separate`.
    Separate {
        request: AnalyzeRequest,
    },
    Progress {
        progress: ProgressReport,
    },
    Complete {
        result: WorkerResult,
    },
    Error {
        error: WorkerFailure,
    },
    Cancel {
        job_id: String,
    },
    Cancelled {
        job_id: String,
    },
    Shutdown,
}

impl WireMessage {
    pub const fn direction(&self) -> WireDirection {
        match self {
            Self::Hello
            | Self::LoadModel { .. }
            | Self::Analyze { .. }
            | Self::Separate { .. }
            | Self::Cancel { .. }
            | Self::Shutdown => WireDirection::ControllerToWorker,
            Self::Capabilities { .. }
            | Self::ModelLoaded { .. }
            | Self::Progress { .. }
            | Self::Complete { .. }
            | Self::Error { .. }
            | Self::Cancelled { .. } => WireDirection::WorkerToController,
        }
    }

    fn validate_shape(&self) -> Result<(), WireError> {
        match self {
            Self::Hello | Self::Shutdown => Ok(()),
            Self::Capabilities { capabilities } => capabilities.validate(),
            Self::LoadModel { manifest_sha256 } | Self::ModelLoaded { manifest_sha256 } => {
                parse_hash("manifest_sha256", manifest_sha256).map(|_| ())
            }
            Self::Analyze { request } | Self::Separate { request } => request.validate(),
            Self::Progress { progress } => progress.validate(),
            Self::Complete { result } => result.validate(),
            Self::Error { error } => error.validate(),
            Self::Cancel { job_id } | Self::Cancelled { job_id } => {
                validate_label("job_id", job_id)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireDirection {
    ControllerToWorker,
    WorkerToController,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireCapabilities {
    pub worker_name: String,
    pub backends: BTreeSet<String>,
    pub maximum_parallel_jobs: u16,
    pub shared_memory: bool,
}

impl WireCapabilities {
    fn validate(&self) -> Result<(), WireError> {
        validate_label("capabilities.worker_name", &self.worker_name)?;
        if self.maximum_parallel_jobs == 0 {
            return Err(WireError::MalformedRecord(
                "capabilities.maximum_parallel_jobs must be non-zero".into(),
            ));
        }
        if self.backends.is_empty() {
            return Err(WireError::MalformedRecord(
                "capabilities.backends must not be empty".into(),
            ));
        }
        for backend in &self.backends {
            validate_label("capabilities.backends", backend)?;
        }
        Ok(())
    }
}

/// File names are relative to a supervisor-created job sandbox. Hashes in the
/// request identify their contents; workers never resolve arbitrary paths.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobFiles {
    pub material: String,
    #[serde(default)]
    pub references: BTreeMap<String, String>,
    #[serde(default)]
    pub masks: BTreeMap<String, String>,
    pub staging_directory: String,
}

impl JobFiles {
    fn validate(&self) -> Result<(), WireError> {
        validate_relative_path("files.material", &self.material)?;
        validate_relative_path("files.staging_directory", &self.staging_directory)?;
        for (hash, path) in self.references.iter().chain(self.masks.iter()) {
            parse_hash("files.reference_or_mask_hash", hash)?;
            validate_relative_path("files.reference_or_mask_path", path)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum WireParameter {
    String(String),
    Signed(i64),
    Unsigned(u64),
    Bool(bool),
    Float(f64),
}

impl WireParameter {
    fn validate(&self) -> Result<(), WireError> {
        if matches!(self, Self::Float(value) if !value.is_finite()) {
            return Err(WireError::MalformedRecord(
                "floating-point parameters must be finite".into(),
            ));
        }
        Ok(())
    }
}

/// Generic analysis request. Exact model behavior is identified by the
/// manifest and effective parameters, never by an implicit worker default.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnalyzeRequest {
    pub job_id: String,
    pub model_manifest_sha256: String,
    /// Supervisor-computed content identity for this exact request. The worker
    /// echoes it on completion; it never chooses a cache destination.
    pub cache_key: String,
    pub material_sha256: String,
    pub start_frame: u64,
    pub frame_count: u64,
    #[serde(default)]
    pub channel_selection: Vec<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default)]
    pub reference_sha256: Vec<String>,
    #[serde(default)]
    pub mask_sha256: Vec<String>,
    #[serde(default)]
    pub parameters: BTreeMap<String, WireParameter>,
    pub files: JobFiles,
}

impl AnalyzeRequest {
    pub fn manifest_hash(&self) -> Result<ContentHash, WireError> {
        parse_hash("model_manifest_sha256", &self.model_manifest_sha256)
    }

    fn validate(&self) -> Result<(), WireError> {
        validate_label("job_id", &self.job_id)?;
        self.manifest_hash()?;
        parse_hash("cache_key", &self.cache_key)?;
        parse_hash("material_sha256", &self.material_sha256)?;
        if self.frame_count == 0 || self.start_frame.checked_add(self.frame_count).is_none() {
            return Err(WireError::MalformedRecord(
                "frame range must be non-empty and fit in u64".into(),
            ));
        }
        let mut channels = self.channel_selection.clone();
        channels.sort_unstable();
        if channels.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(WireError::MalformedRecord(
                "channel_selection values must be unique".into(),
            ));
        }
        for hash in self.reference_sha256.iter().chain(&self.mask_sha256) {
            parse_hash("reference_or_mask_sha256", hash)?;
        }
        for (name, parameter) in &self.parameters {
            validate_label("parameters.name", name)?;
            parameter.validate()?;
        }
        self.files.validate()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultPhase {
    Preparing,
    Decoding,
    Analyzing,
    Encoding,
    Verifying,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressReport {
    pub job_id: String,
    pub phase: ResultPhase,
    pub completed_chunks: u64,
    pub total_chunks: u64,
}

impl ProgressReport {
    fn validate(&self) -> Result<(), WireError> {
        validate_label("progress.job_id", &self.job_id)?;
        if self.total_chunks == 0 || self.completed_chunks > self.total_chunks {
            return Err(WireError::MalformedRecord(
                "progress requires 0 <= completed <= total and non-zero total".into(),
            ));
        }
        Ok(())
    }
}

/// Typed artifact metadata is additive to schema-v1's
/// `StagedArtifact { path, hash, length }`. It is deliberately per artifact
/// so a mixed IDM/Syntheon result cannot masquerade as a single waveform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Audio,
    Mask,
    EventMap,
    Midi,
    Preset,
    ControlCurve,
    Embedding,
    Measurement,
    AdapterMetadata,
    Custom(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdditivityDeclaration {
    LinearSum,
    LinearSumWithResidual { residual_artifact: String },
    OverlappingEstimates,
    Generative,
    NonAudio,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDescriptor {
    pub relative_path: String,
    pub sha256: String,
    pub byte_len: u64,
    pub kind: ArtifactKind,
    pub media_type: String,
    pub schema_revision: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_base_hz: Option<u32>,
    pub additivity: AdditivityDeclaration,
    #[serde(default)]
    pub source_backlinks: Vec<SourceBacklink>,
}

impl ArtifactDescriptor {
    fn validate(&self) -> Result<(), WireError> {
        validate_relative_path("artifact.relative_path", &self.relative_path)?;
        parse_hash("artifact.sha256", &self.sha256)?;
        validate_media_type(&self.media_type)?;
        if self.schema_revision == 0 {
            return Err(WireError::MalformedRecord(
                "artifact.schema_revision must be non-zero".into(),
            ));
        }
        if matches!(self.kind, ArtifactKind::Audio) && self.time_base_hz.unwrap_or(0) == 0 {
            return Err(WireError::MalformedRecord(
                "audio artifact needs a non-zero time_base_hz".into(),
            ));
        }
        for backlink in &self.source_backlinks {
            backlink.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceBacklink {
    pub material_sha256: String,
    pub start_frame: u64,
    pub frame_count: u64,
}

impl SourceBacklink {
    fn validate(&self) -> Result<(), WireError> {
        parse_hash("source_backlink.material_sha256", &self.material_sha256)?;
        if self.frame_count == 0 || self.start_frame.checked_add(self.frame_count).is_none() {
            return Err(WireError::MalformedRecord(
                "source backlink must name a non-empty non-overflowing frame span".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeasurementValue {
    pub name: String,
    pub value: WireParameter,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkerResult {
    pub job_id: String,
    pub cache_key: String,
    pub artifacts: Vec<ArtifactDescriptor>,
    #[serde(default)]
    pub measurements: Vec<MeasurementValue>,
}

impl WorkerResult {
    fn validate(&self) -> Result<(), WireError> {
        validate_label("result.job_id", &self.job_id)?;
        parse_hash("result.cache_key", &self.cache_key)?;
        if self.artifacts.is_empty() {
            return Err(WireError::MalformedRecord(
                "result must declare at least one artifact".into(),
            ));
        }
        let mut paths = BTreeSet::new();
        for artifact in &self.artifacts {
            artifact.validate()?;
            if !paths.insert(&artifact.relative_path) {
                return Err(WireError::MalformedRecord(
                    "result artifact paths must be unique".into(),
                ));
            }
        }
        for measurement in &self.measurements {
            validate_label("measurement.name", &measurement.name)?;
            measurement.value.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFailureKind {
    Adapter,
    Cancelled,
    Crash,
    OutOfMemory,
    Protocol,
    InvalidInput,
    InvalidOutput,
    Unavailable,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerFailure {
    pub job_id: String,
    pub kind: WorkerFailureKind,
    pub detail: String,
}

impl WorkerFailure {
    fn validate(&self) -> Result<(), WireError> {
        validate_label("error.job_id", &self.job_id)?;
        if self.detail.trim().is_empty() {
            return Err(WireError::MalformedRecord(
                "error.detail must not be empty".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum WireError {
    Encode(serde_json::Error),
    Decode(serde_json::Error),
    ProtocolVersion {
        actual: u32,
        expected: u32,
    },
    WrongDirection {
        expected: WireDirection,
        actual: WireDirection,
    },
    Sequence {
        expected: u64,
        actual: u64,
    },
    State(String),
    MalformedRecord(String),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode(error) => write!(f, "could not encode worker JSONL: {error}"),
            Self::Decode(error) => write!(f, "could not decode worker JSONL: {error}"),
            Self::ProtocolVersion { actual, expected } => {
                write!(f, "worker protocol version {actual}, expected {expected}")
            }
            Self::WrongDirection { expected, actual } => {
                write!(
                    f,
                    "worker record direction {actual:?}, expected {expected:?}"
                )
            }
            Self::Sequence { expected, actual } => {
                write!(f, "worker record sequence {actual}, expected {expected}")
            }
            Self::State(detail) | Self::MalformedRecord(detail) => f.write_str(detail),
        }
    }
}

impl std::error::Error for WireError {}

/// Small session validator shared by a supervisor and fake worker. It does
/// not perform I/O or own a process; it rejects impossible conversations
/// before they can mutate the artifact store.
#[derive(Clone, Debug, Default)]
pub struct SessionValidator {
    next_controller_sequence: u64,
    next_worker_sequence: u64,
    stage: SessionStage,
    pending_loads: BTreeSet<String>,
    loaded_manifests: BTreeSet<String>,
    jobs: BTreeMap<String, SessionJob>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum SessionStage {
    #[default]
    New,
    HelloSent,
    Ready,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionJob {
    phase: ResultPhase,
    completed_chunks: u64,
    total_chunks: u64,
    cancelling: bool,
    terminal: bool,
}

impl SessionValidator {
    pub fn observe_controller(&mut self, envelope: &WireEnvelope) -> Result<(), WireError> {
        self.observe(envelope, WireDirection::ControllerToWorker)
    }

    pub fn observe_worker(&mut self, envelope: &WireEnvelope) -> Result<(), WireError> {
        self.observe(envelope, WireDirection::WorkerToController)
    }

    fn observe(
        &mut self,
        envelope: &WireEnvelope,
        expected: WireDirection,
    ) -> Result<(), WireError> {
        envelope.validate_shape()?;
        let actual = envelope.message.direction();
        if actual != expected {
            return Err(WireError::WrongDirection { expected, actual });
        }
        let next = match expected {
            WireDirection::ControllerToWorker => &mut self.next_controller_sequence,
            WireDirection::WorkerToController => &mut self.next_worker_sequence,
        };
        if envelope.sequence != *next {
            return Err(WireError::Sequence {
                expected: *next,
                actual: envelope.sequence,
            });
        }
        *next = next.saturating_add(1);
        self.apply(&envelope.message)
    }

    fn apply(&mut self, message: &WireMessage) -> Result<(), WireError> {
        match message {
            WireMessage::Hello if self.stage == SessionStage::New => {
                self.stage = SessionStage::HelloSent
            }
            WireMessage::Capabilities { .. } if self.stage == SessionStage::HelloSent => {
                self.stage = SessionStage::Ready
            }
            WireMessage::LoadModel { manifest_sha256 } if self.stage == SessionStage::Ready => {
                self.pending_loads.insert(manifest_sha256.clone());
            }
            WireMessage::ModelLoaded { manifest_sha256 } if self.stage == SessionStage::Ready => {
                if !self.pending_loads.remove(manifest_sha256) {
                    return Err(WireError::State(
                        "worker acknowledged a model that was not requested".into(),
                    ));
                }
                self.loaded_manifests.insert(manifest_sha256.clone());
            }
            WireMessage::Analyze { request } | WireMessage::Separate { request }
                if self.stage == SessionStage::Ready =>
            {
                if !self
                    .loaded_manifests
                    .contains(&request.model_manifest_sha256)
                {
                    return Err(WireError::State(
                        "analysis requested before its model is loaded".into(),
                    ));
                }
                if self
                    .jobs
                    .insert(
                        request.job_id.clone(),
                        SessionJob {
                            phase: ResultPhase::Preparing,
                            completed_chunks: 0,
                            total_chunks: 0,
                            cancelling: false,
                            terminal: false,
                        },
                    )
                    .is_some()
                {
                    return Err(WireError::State(
                        "job IDs cannot be reused within one worker session".into(),
                    ));
                }
            }
            WireMessage::Progress { progress } => {
                let job = self.active_job_mut(&progress.job_id)?;
                if progress.phase < job.phase
                    || (job.total_chunks != 0
                        && (progress.total_chunks != job.total_chunks
                            || progress.completed_chunks < job.completed_chunks))
                {
                    return Err(WireError::State("worker progress regressed".into()));
                }
                job.phase = progress.phase;
                job.completed_chunks = progress.completed_chunks;
                job.total_chunks = progress.total_chunks;
            }
            WireMessage::Cancel { job_id } => self.active_job_mut(job_id)?.cancelling = true,
            WireMessage::Cancelled { job_id } => self.finish_job(job_id)?,
            WireMessage::Error { error } => self.finish_job(&error.job_id)?,
            WireMessage::Complete { result } => {
                let job = self.active_job_mut(&result.job_id)?;
                if job.cancelling {
                    return Err(WireError::State(
                        "cancelled job cannot publish a result".into(),
                    ));
                }
                if job.phase != ResultPhase::Verifying
                    || job.total_chunks == 0
                    || job.completed_chunks != job.total_chunks
                {
                    return Err(WireError::State(
                        "result arrived before verified complete progress".into(),
                    ));
                }
                job.terminal = true;
            }
            WireMessage::Shutdown
                if self.stage == SessionStage::Ready
                    && self.jobs.values().all(|job| job.terminal) =>
            {
                self.stage = SessionStage::Closed;
            }
            _ => return Err(WireError::State("invalid worker-session transition".into())),
        }
        Ok(())
    }

    fn active_job_mut(&mut self, id: &str) -> Result<&mut SessionJob, WireError> {
        let job = self
            .jobs
            .get_mut(id)
            .ok_or_else(|| WireError::State("unknown job ID".into()))?;
        if job.terminal {
            return Err(WireError::State("job is already terminal".into()));
        }
        Ok(job)
    }

    fn finish_job(&mut self, id: &str) -> Result<(), WireError> {
        self.active_job_mut(id)?.terminal = true;
        Ok(())
    }
}

fn parse_hash(field: &str, value: &str) -> Result<ContentHash, WireError> {
    value.parse::<ContentHash>().map_err(|error| {
        WireError::MalformedRecord(format!("{field} must be a lowercase SHA-256: {error}"))
    })
}

fn validate_label(field: &str, value: &str) -> Result<(), WireError> {
    if value.is_empty()
        || value.len() > 160
        || value.contains(char::is_whitespace)
        || value.contains(['/', '\\', '\0'])
    {
        return Err(WireError::MalformedRecord(format!(
            "{field} must be a compact non-path label"
        )));
    }
    Ok(())
}

fn validate_relative_path(field: &str, value: &str) -> Result<(), WireError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(WireError::MalformedRecord(format!(
            "{field} must be a normalized relative path"
        )));
    }
    Ok(())
}

fn validate_media_type(value: &str) -> Result<(), WireError> {
    if value.is_empty()
        || value.len() > 160
        || !value.contains('/')
        || value.contains(char::is_whitespace)
    {
        return Err(WireError::MalformedRecord(
            "artifact.media_type must be a compact MIME/schema type".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonl_round_trip_preserves_typed_mixed_artifacts() {
        let record = WireEnvelope::new(
            3,
            WireMessage::Complete {
                result: WorkerResult {
                    job_id: "job-1".into(),
                    cache_key: "00".repeat(32),
                    artifacts: vec![ArtifactDescriptor {
                        relative_path: "staging/onsets.json".into(),
                        sha256: "11".repeat(32),
                        byte_len: 9,
                        kind: ArtifactKind::EventMap,
                        media_type: "application/vnd.audec.idm-events+json".into(),
                        schema_revision: 1,
                        time_base_hz: Some(44_100),
                        additivity: AdditivityDeclaration::NonAudio,
                        source_backlinks: vec![SourceBacklink {
                            material_sha256: "22".repeat(32),
                            start_frame: 0,
                            frame_count: 44_100,
                        }],
                    }],
                    measurements: vec![],
                },
            },
        );
        let line = record.to_jsonl().unwrap();
        assert_eq!(WireEnvelope::from_jsonl(line.trim_end()).unwrap(), record);
    }

    #[test]
    fn session_rejects_result_before_verification() {
        let mut session = SessionValidator::default();
        let manifest = "aa".repeat(32);
        session
            .observe_controller(&WireEnvelope::new(0, WireMessage::Hello))
            .unwrap();
        session
            .observe_worker(&WireEnvelope::new(
                0,
                WireMessage::Capabilities {
                    capabilities: WireCapabilities {
                        worker_name: "fake-worker".into(),
                        backends: BTreeSet::from(["cpu".into()]),
                        maximum_parallel_jobs: 1,
                        shared_memory: false,
                    },
                },
            ))
            .unwrap();
        session
            .observe_controller(&WireEnvelope::new(
                1,
                WireMessage::LoadModel {
                    manifest_sha256: manifest.clone(),
                },
            ))
            .unwrap();
        session
            .observe_worker(&WireEnvelope::new(
                1,
                WireMessage::ModelLoaded {
                    manifest_sha256: manifest.clone(),
                },
            ))
            .unwrap();
        session
            .observe_controller(&WireEnvelope::new(
                2,
                WireMessage::Analyze {
                    request: AnalyzeRequest {
                        job_id: "job-1".into(),
                        model_manifest_sha256: manifest,
                        cache_key: "cc".repeat(32),
                        material_sha256: "bb".repeat(32),
                        start_frame: 0,
                        frame_count: 1,
                        channel_selection: vec![],
                        prompt: None,
                        reference_sha256: vec![],
                        mask_sha256: vec![],
                        parameters: BTreeMap::new(),
                        files: JobFiles {
                            material: "input.pcm".into(),
                            references: BTreeMap::new(),
                            masks: BTreeMap::new(),
                            staging_directory: "staging".into(),
                        },
                    },
                },
            ))
            .unwrap();
        let error = session
            .observe_worker(&WireEnvelope::new(
                2,
                WireMessage::Complete {
                    result: WorkerResult {
                        job_id: "job-1".into(),
                        cache_key: "cc".repeat(32),
                        artifacts: vec![ArtifactDescriptor {
                            relative_path: "staging/result.json".into(),
                            sha256: "dd".repeat(32),
                            byte_len: 1,
                            kind: ArtifactKind::Measurement,
                            media_type: "application/json".into(),
                            schema_revision: 1,
                            time_base_hz: None,
                            additivity: AdditivityDeclaration::NonAudio,
                            source_backlinks: vec![],
                        }],
                        measurements: vec![],
                    },
                },
            ))
            .unwrap_err();
        assert!(matches!(error, WireError::State(_)));
    }
}
