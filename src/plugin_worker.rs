//! Process-independent scanner/runtime core and deterministic fake backend.
//!
//! A launcher owns child processes, deadlines, and OS sandbox handles. This
//! module owns protocol behavior and conversion into the durable `plugin.rs`
//! cache. It never loads a dynamic library.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::plugin::{
    ArtifactFingerprint, AudioPortDescriptor, AudioPortRole, ChannelLayout, CpuArchitecture,
    DeterminismClass, ExecutionCapabilities, ParameterMapping, PluginIndex, PluginKey,
    PluginMetadata, PluginParameterDescriptor, PluginParameterKey, PluginRole, PluginStateBlob,
    PortDirection, ScanFailure, ScanFailureKind, ScanRecord, ScannerProvenance,
    SCAN_SCHEMA_VERSION,
};
use crate::plugin_wire::{
    self, CapabilitiesDto, FormatDto, Message, RuntimeFailureDto, RuntimeFailureKindDto,
    ScanFailureDto, ScanRecordDto, StateArtifactDto, TailDto, TokenDto,
};

pub const FAKE_CLAP_ID: &str = "org.audec.fixture.gain";
pub const FAKE_VST3_ID: &str = "56535441554445434649585455524531";

#[derive(Debug)]
pub struct FakeWorker {
    session_root: PathBuf,
    instances: BTreeMap<TokenDto, FakeInstance>,
}

#[derive(Debug)]
struct FakeInstance {
    plugin: PluginKey,
    gain: f64,
}

impl FakeWorker {
    pub fn new(session_root: PathBuf) -> Self {
        Self {
            session_root,
            instances: BTreeMap::new(),
        }
    }

    pub fn capabilities() -> CapabilitiesDto {
        CapabilitiesDto {
            worker_name: "audec-fake-plugin".into(),
            worker_version: "1".into(),
            worker_build_sha256: plugin_wire::digest_bytes(b"audec-fake-plugin-v1").to_hex(),
            formats: BTreeSet::from([FormatDto::Clap, FormatDto::Vst3]),
            architectures: BTreeSet::from([current_architecture().into()]),
            scanning: true,
            realtime: true,
            offline: true,
            shared_memory: true,
            maximum_instances: 64,
        }
    }

    pub fn handle(&mut self, message: Message) -> Result<Option<Message>, WorkerError> {
        match message {
            Message::Hello => Ok(Some(Message::Capabilities {
                capabilities: Self::capabilities(),
            })),
            Message::Scan { request } => Ok(Some(match scan_fixture(&request.candidate_path) {
                Ok(record) => Message::ScanReady {
                    request_id: request.request_id,
                    record: ScanRecordDto::from_domain(&record)?,
                },
                Err(failure) => Message::ScanFailed {
                    request_id: request.request_id,
                    failure: ScanFailureDto::from_domain(&failure)?,
                },
            })),
            Message::Instantiate { request } => {
                let plugin = request.plugin.to_domain()?;
                let mut instance = FakeInstance { plugin, gain: 0.5 };
                if let Some(state) = request.state {
                    let path = self.resolve(&state.relative_path)?;
                    let bytes =
                        fs::read(&path).map_err(|source| WorkerError::Io { path, source })?;
                    instance.restore(&state.into_blob(bytes)?)?;
                }
                if self
                    .instances
                    .insert(request.instance.clone(), instance)
                    .is_some()
                {
                    return Err(WorkerError::State("duplicate instance"));
                }
                Ok(Some(Message::Instantiated {
                    request_id: request.request_id,
                    instance: request.instance,
                    latency_frames: 16,
                    tail: TailDto::FiniteFrames { frames: 64 },
                }))
            }
            Message::BindSharedMemory { binding } => Ok(Some(Message::Bound {
                instance: binding.instance,
            })),
            Message::Activate { instance } => {
                self.instance(&instance)?;
                Ok(Some(Message::Activated { instance }))
            }
            Message::SetParameters { request } => {
                let instance = self.instance_mut(&request.instance)?;
                for parameter in request.values {
                    let (key, value) = parameter.to_domain()?;
                    if key != fake_parameter_key(&instance.plugin) {
                        return Err(WorkerError::State("unknown fake parameter"));
                    }
                    instance.gain = value.get();
                }
                Ok(Some(Message::ParametersSet {
                    request_id: request.request_id,
                    instance: request.instance,
                }))
            }
            Message::Process {
                instance,
                process_sequence,
                ..
            } => {
                self.instance(&instance)?;
                Ok(Some(Message::Processed {
                    instance,
                    process_sequence,
                    output_event_count: 0,
                }))
            }
            Message::SaveState { request } => {
                let state = self.instance(&request.instance)?.save();
                if state.bytes.len() as u64 > request.maximum_bytes {
                    return Err(WorkerError::State("state exceeds request limit"));
                }
                let path = self.resolve(&request.output_relative_path)?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|source| WorkerError::Io {
                        path: parent.to_path_buf(),
                        source,
                    })?;
                }
                fs::write(&path, &state.bytes).map_err(|source| WorkerError::Io {
                    path: path.clone(),
                    source,
                })?;
                Ok(Some(Message::StateSaved {
                    request_id: request.request_id,
                    state: StateArtifactDto::from_blob(&state, request.output_relative_path)?,
                }))
            }
            Message::Deactivate { instance } => {
                self.instance(&instance)?;
                Ok(Some(Message::Deactivated { instance }))
            }
            Message::Destroy { instance } => {
                if self.instances.remove(&instance).is_none() {
                    return Err(WorkerError::State("unknown instance"));
                }
                Ok(Some(Message::Destroyed { instance }))
            }
            Message::Shutdown => Ok(None),
            _ => Err(WorkerError::State("worker received response message")),
        }
    }

    pub fn failure(error: &WorkerError) -> Message {
        Message::Error {
            failure: RuntimeFailureDto {
                request_id: None,
                instance: None,
                kind: match error {
                    WorkerError::Wire(_) => RuntimeFailureKindDto::InvalidRequest,
                    WorkerError::Domain(_) => RuntimeFailureKindDto::InvalidState,
                    WorkerError::Io { .. } => RuntimeFailureKindDto::Io,
                    WorkerError::State(_) => RuntimeFailureKindDto::Backend,
                },
                recoverable: false,
                detail: error.to_string(),
            },
        }
    }

    fn resolve(&self, relative: &str) -> Result<PathBuf, WorkerError> {
        plugin_wire::validate_relative_path(relative)?;
        Ok(self.session_root.join(relative))
    }

    fn instance(&self, token: &TokenDto) -> Result<&FakeInstance, WorkerError> {
        self.instances
            .get(token)
            .ok_or(WorkerError::State("unknown instance"))
    }

    fn instance_mut(&mut self, token: &TokenDto) -> Result<&mut FakeInstance, WorkerError> {
        self.instances
            .get_mut(token)
            .ok_or(WorkerError::State("unknown instance"))
    }
}

impl FakeInstance {
    fn save(&self) -> PluginStateBlob {
        let bytes = format!("audec-fake-state-v1\ngain={:.17}\n", self.gain).into_bytes();
        PluginStateBlob {
            plugin: self.plugin.clone(),
            state_format_version: 1,
            digest: plugin_wire::digest_bytes(&bytes),
            bytes,
        }
    }

    fn restore(&mut self, state: &PluginStateBlob) -> Result<(), WorkerError> {
        if state.plugin != self.plugin || state.state_format_version != 1 {
            return Err(WorkerError::State("state identity mismatch"));
        }
        let text = std::str::from_utf8(&state.bytes)
            .map_err(|_| WorkerError::State("state is not UTF-8"))?;
        self.gain = text
            .strip_prefix("audec-fake-state-v1\ngain=")
            .and_then(|value| value.strip_suffix('\n'))
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
            .ok_or(WorkerError::State("invalid fake state"))?;
        Ok(())
    }
}

/// The controller fingerprints independently before trusting a scan. A
/// descriptor with a mismatched path or digest is rejected rather than cached.
pub fn apply_scan_result(
    index: &mut PluginIndex,
    expected_path: &Path,
    expected_artifact: &ArtifactFingerprint,
    message: &Message,
    quarantine_after: u32,
) -> Result<(), WorkerError> {
    match message {
        Message::ScanReady { record, .. } => {
            let record = record.to_domain()?;
            if record.canonical_path != expected_path || &record.artifact != expected_artifact {
                return Err(WorkerError::State("scanner identity mismatch"));
            }
            index.apply_success(record)?;
            Ok(())
        }
        Message::ScanFailed { failure, .. } => {
            index.apply_failure(
                expected_path.to_path_buf(),
                expected_artifact.clone(),
                failure.to_domain()?,
                quarantine_after,
            )?;
            Ok(())
        }
        _ => Err(WorkerError::State("not a scan result")),
    }
}

/// Converts launcher-level timeout/crash information into the same persistent
/// cache diagnostics as an ordinary scanner failure.
pub fn record_scan_process_failure(
    index: &mut PluginIndex,
    path: PathBuf,
    artifact: ArtifactFingerprint,
    timed_out: bool,
    detail: String,
    quarantine_after: u32,
) -> Result<(), WorkerError> {
    index.apply_failure(
        path,
        artifact,
        ScanFailure {
            kind: if timed_out {
                ScanFailureKind::TimedOut
            } else {
                ScanFailureKind::Crashed
            },
            detail,
            scanner: scanner_provenance(),
        },
        quarantine_after,
    )?;
    Ok(())
}

pub fn fingerprint_file(path: &Path) -> Result<ArtifactFingerprint, WorkerError> {
    let bytes = fs::read(path).map_err(|source| WorkerError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(ArtifactFingerprint {
        content: plugin_wire::digest_bytes(&bytes),
        byte_len: bytes.len() as u64,
        architectures: BTreeSet::from([current_architecture()]),
    })
}

fn scan_fixture(path: &str) -> Result<ScanRecord, ScanFailure> {
    let canonical_path = fs::canonicalize(path).map_err(|error| scan_io(error.to_string()))?;
    let bytes = fs::read(&canonical_path).map_err(|error| scan_io(error.to_string()))?;
    if bytes.is_empty() {
        return Err(ScanFailure {
            kind: ScanFailureKind::InvalidDescriptor,
            detail: "empty fake plugin artifact".into(),
            scanner: scanner_provenance(),
        });
    }
    let format = match canonical_path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("clap") => crate::plugin::PluginFormat::Clap,
        Some("vst3") => crate::plugin::PluginFormat::Vst3,
        _ => {
            return Err(ScanFailure {
                kind: ScanFailureKind::InvalidAbi,
                detail: "fixture accepts only .clap and .vst3 files".into(),
                scanner: scanner_provenance(),
            })
        }
    };
    Ok(ScanRecord {
        schema_version: SCAN_SCHEMA_VERSION,
        canonical_path,
        artifact: ArtifactFingerprint {
            content: plugin_wire::digest_bytes(&bytes),
            byte_len: bytes.len() as u64,
            architectures: BTreeSet::from([current_architecture()]),
        },
        scanner: scanner_provenance(),
        plugins: vec![fake_metadata(format)],
    })
}

fn fake_metadata(format: crate::plugin::PluginFormat) -> PluginMetadata {
    let plugin = PluginKey {
        identifier: match format {
            crate::plugin::PluginFormat::Clap => FAKE_CLAP_ID.into(),
            crate::plugin::PluginFormat::Vst3 => FAKE_VST3_ID.into(),
            _ => unreachable!(),
        },
        format,
    };
    PluginMetadata {
        key: plugin.clone(),
        name: "Audec Fixture Gain".into(),
        vendor: Some("Audec".into()),
        version: Some("1".into()),
        description: Some("deterministic isolated worker fixture".into()),
        homepage: None,
        features: BTreeSet::from(["audio-effect".into(), "stereo".into()]),
        roles: BTreeSet::from([PluginRole::AudioEffect]),
        audio_ports: vec![
            AudioPortDescriptor {
                native_id: 0,
                name: "Main Input".into(),
                direction: PortDirection::Input,
                role: AudioPortRole::Main,
                layouts: vec![ChannelLayout::Stereo],
                required: true,
            },
            AudioPortDescriptor {
                native_id: 0,
                name: "Main Output".into(),
                direction: PortDirection::Output,
                role: AudioPortRole::Main,
                layouts: vec![ChannelLayout::Stereo],
                required: true,
            },
        ],
        note_ports: vec![],
        parameters: vec![PluginParameterDescriptor {
            key: fake_parameter_key(&plugin),
            name: "Gain".into(),
            module: None,
            unit: Some("normalized".into()),
            plain_min: 0.0,
            plain_max: 1.0,
            plain_default: 0.5,
            mapping: ParameterMapping::Linear,
            automatable: true,
            modulatable: true,
            read_only: false,
            hidden: false,
        }],
        capabilities: ExecutionCapabilities {
            realtime_safe_claimed: true,
            hard_realtime_required: false,
            offline_processing: true,
            editor: false,
            state: true,
            latency_reporting: true,
            tail_reporting: true,
            determinism: DeterminismClass::Deterministic,
        },
    }
}

fn fake_parameter_key(plugin: &PluginKey) -> PluginParameterKey {
    match plugin.format {
        crate::plugin::PluginFormat::Clap => PluginParameterKey::Clap(1),
        crate::plugin::PluginFormat::Vst3 => PluginParameterKey::Vst3([1; 16]),
        _ => unreachable!(),
    }
}

fn scanner_provenance() -> ScannerProvenance {
    ScannerProvenance {
        scanner_name: "audec-fake-plugin".into(),
        scanner_version: "1".into(),
        scanner_build: plugin_wire::digest_bytes(b"audec-fake-plugin-v1"),
        host_os: std::env::consts::OS.into(),
        host_architecture: current_architecture(),
        hash_algorithm: "sha256".into(),
    }
}

fn scan_io(detail: String) -> ScanFailure {
    ScanFailure {
        kind: ScanFailureKind::Io,
        detail,
        scanner: scanner_provenance(),
    }
}

fn current_architecture() -> CpuArchitecture {
    match std::env::consts::ARCH {
        "aarch64" => CpuArchitecture::Aarch64,
        "x86_64" => CpuArchitecture::X86_64,
        _ => CpuArchitecture::Other,
    }
}

#[derive(Debug)]
pub enum WorkerError {
    Wire(plugin_wire::WireError),
    Domain(crate::plugin::PluginValidationError),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    State(&'static str),
}

impl fmt::Display for WorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wire(error) => error.fmt(formatter),
            Self::Domain(error) => error.fmt(formatter),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::State(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for WorkerError {}

impl From<plugin_wire::WireError> for WorkerError {
    fn from(value: plugin_wire::WireError) -> Self {
        Self::Wire(value)
    }
}

impl From<crate::plugin::PluginValidationError> for WorkerError {
    fn from(value: crate::plugin::PluginValidationError) -> Self {
        Self::Domain(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::plugin::{PluginFormat, ScanCacheEntry};
    use crate::plugin_wire::{
        InstantiateDto, ParameterKeyDto, ParameterValueDto, PluginKeyDto, ProcessingContractDto,
        SaveStateDto, SetParametersDto,
    };

    fn temporary_root() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("audec-fake-plugin-{unique}"));
        fs::create_dir_all(&path).unwrap();
        path
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
            offline: true,
        }
    }

    fn instantiate(instance: u128, state: Option<StateArtifactDto>) -> Message {
        Message::Instantiate {
            request: InstantiateDto {
                request_id: instance as u64,
                instance: TokenDto::new(instance),
                artifact_lease: TokenDto::new(99),
                plugin: PluginKeyDto::from_domain(&PluginKey {
                    format: PluginFormat::Clap,
                    identifier: FAKE_CLAP_ID.into(),
                })
                .unwrap(),
                contract: contract(),
                state,
            },
        }
    }

    #[test]
    fn fake_state_roundtrip_is_byte_identical() {
        let root = temporary_root();
        let mut first = FakeWorker::new(root.clone());
        first.handle(instantiate(1, None)).unwrap();
        first
            .handle(Message::SetParameters {
                request: SetParametersDto {
                    request_id: 2,
                    instance: TokenDto::new(1),
                    values: vec![ParameterValueDto {
                        key: ParameterKeyDto::Clap { id: 1 },
                        normalized: 0.25,
                    }],
                },
            })
            .unwrap();
        let saved = first
            .handle(Message::SaveState {
                request: SaveStateDto {
                    request_id: 3,
                    instance: TokenDto::new(1),
                    maximum_bytes: 1024,
                    output_relative_path: "first.state".into(),
                },
            })
            .unwrap()
            .unwrap();
        let Message::StateSaved { state, .. } = saved else {
            panic!("expected state")
        };
        let mut second = FakeWorker::new(root.clone());
        second.handle(instantiate(4, Some(state))).unwrap();
        second
            .handle(Message::SaveState {
                request: SaveStateDto {
                    request_id: 5,
                    instance: TokenDto::new(4),
                    maximum_bytes: 1024,
                    output_relative_path: "second.state".into(),
                },
            })
            .unwrap();
        assert_eq!(
            fs::read(root.join("first.state")).unwrap(),
            fs::read(root.join("second.state")).unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_identity_is_verified_and_repeated_crashes_quarantine() {
        let root = temporary_root();
        let path = root.join("fixture.clap");
        fs::write(&path, b"deterministic fake artifact").unwrap();
        let path = fs::canonicalize(path).unwrap();
        let expected = fingerprint_file(&path).unwrap();
        let record = scan_fixture(path.to_str().unwrap()).unwrap();
        let message = Message::ScanReady {
            request_id: 1,
            record: ScanRecordDto::from_domain(&record).unwrap(),
        };
        let mut index = PluginIndex::default();
        apply_scan_result(&mut index, &path, &expected, &message, 2).unwrap();
        assert_eq!(
            index
                .descriptor(&PluginKey {
                    format: PluginFormat::Clap,
                    identifier: FAKE_CLAP_ID.into(),
                })
                .unwrap()
                .name,
            "Audec Fixture Gain"
        );

        let crash_path = root.join("crash.clap");
        fs::write(&crash_path, b"crashing bytes").unwrap();
        let crash_path = fs::canonicalize(crash_path).unwrap();
        let crash_fingerprint = fingerprint_file(&crash_path).unwrap();
        for _ in 0..2 {
            record_scan_process_failure(
                &mut index,
                crash_path.clone(),
                crash_fingerprint.clone(),
                false,
                "worker exited with status 70".into(),
                2,
            )
            .unwrap();
        }
        assert!(matches!(
            index.entries()[&crash_path],
            ScanCacheEntry::Quarantined {
                consecutive_failures: 2,
                ..
            }
        ));
        assert!(!index.needs_scan(&crash_path, &crash_fingerprint));
        fs::remove_dir_all(root).unwrap();
    }
}
