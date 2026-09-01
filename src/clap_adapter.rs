//! Real CLAP scanner adapter used only by the isolated `audec-clap-worker`.
//!
//! Loading a native library is inherently unsafe even through Clack's safe
//! wrappers. This module must never be linked into or called by the Audec UI or
//! audio process. The worker is disposable and the parent enforces its
//! deadline, output bounds, artifact identity, and quarantine policy.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CStr;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use clack_common::events::event_types::{
    NoteChokeEvent, NoteExpressionEvent, NoteExpressionType, NoteOffEvent, NoteOnEvent,
    ParamValueEvent,
};
use clack_common::events::Pckn;
use clack_common::utils::Cookie;
use clack_extensions::audio_ports::{AudioPortFlags, AudioPortInfoBuffer, PluginAudioPorts};
use clack_extensions::latency::PluginLatency;
use clack_extensions::note_ports::{
    NoteDialect as ClackNoteDialect, NoteDialects, NotePortInfoBuffer, PluginNotePorts,
};
use clack_extensions::params::{ParamInfoBuffer, ParamInfoFlags, PluginParams};
use clack_extensions::render::{PluginRender, RenderMode};
use clack_extensions::state::PluginState;
use clack_extensions::tail::PluginTail;
use clack_host::prelude::{
    AudioPortBuffer, AudioPortBufferType, AudioPorts, ClapId, EventBuffer, HostInfo,
    InputAudioBuffers, InputChannel, OutputAudioBuffers, PluginAudioConfiguration,
    PluginAudioProcessor, PluginEntry, PluginInstance,
};

use crate::plugin::{
    AudioPortDescriptor, AudioPortRole, ChannelLayout, CpuArchitecture, DeterminismClass,
    ExecutionCapabilities, NoteDialect, NotePortDescriptor, ParameterMapping, PluginFormat,
    PluginKey, PluginMetadata, PluginNoteEventKind, PluginNoteExpression,
    PluginParameterDescriptor, PluginParameterKey, PluginRole, PortDirection, ProcessingContract,
    ScanFailure, ScanFailureKind, ScanRecord, ScannerProvenance, TailReport, SCAN_SCHEMA_VERSION,
};
use crate::plugin_wire::{
    self, CapabilitiesDto, FormatDto, InstantiateDto, ParameterValueDto, ScanRequestDto,
    SharedMemoryBindingDto,
};
use crate::plugin_worker::fingerprint_artifact;
use crate::plugin_worker::transport::{InputEvent, SharedBlockTransport, DEFAULT_MAX_EVENTS};

const MAX_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;
const WORKER_BUILD_INPUT: &[u8] = b"audec-real-clap-worker-v1-clack-0.1.1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RealClapRuntimeSupport {
    /// First executable slice: one main f32 port in each direction (or no
    /// input for an instrument), CLAP note/parameter events, state and render
    /// mode. Multi-bus and f64 negotiation remain an explicit refusal.
    IsolatedF32SingleBus,
    /// The scanner can still run, but Audec has not qualified the mapping and
    /// subprocess lifecycle on this target.
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    UnsupportedPlatform,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub const RUNTIME_SUPPORT: RealClapRuntimeSupport = RealClapRuntimeSupport::IsolatedF32SingleBus;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub const RUNTIME_SUPPORT: RealClapRuntimeSupport = RealClapRuntimeSupport::UnsupportedPlatform;

pub fn capabilities() -> CapabilitiesDto {
    let runtime = matches!(
        RUNTIME_SUPPORT,
        RealClapRuntimeSupport::IsolatedF32SingleBus
    );
    CapabilitiesDto {
        worker_name: "audec-clap-worker".into(),
        worker_version: env!("CARGO_PKG_VERSION").into(),
        worker_build_sha256: plugin_wire::digest_bytes(WORKER_BUILD_INPUT).to_hex(),
        formats: BTreeSet::from([FormatDto::Clap]),
        architectures: BTreeSet::from([current_architecture().into()]),
        scanning: true,
        realtime: runtime,
        offline: runtime,
        shared_memory: runtime,
        // Protocol capabilities require a non-zero administrative limit even
        // for a scan-only worker; runtime booleans remain the authority.
        maximum_instances: if runtime { 64 } else { 1 },
    }
}

pub fn scan(request: &ScanRequestDto) -> Result<ScanRecord, ScanFailure> {
    let requested = PathBuf::from(&request.candidate_path);
    let canonical_path = fs::canonicalize(&requested).map_err(|error| {
        failure(
            ScanFailureKind::Io,
            format!("could not canonicalize CLAP artifact: {error}"),
        )
    })?;
    if canonical_path != requested {
        return Err(failure(
            ScanFailureKind::InvalidDescriptor,
            "scanner requires a canonical absolute candidate path".into(),
        ));
    }
    if !canonical_path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("clap"))
    {
        return Err(failure(
            ScanFailureKind::InvalidAbi,
            "real CLAP worker accepts only .clap libraries or bundles".into(),
        ));
    }
    let artifact = fingerprint_artifact(&canonical_path, MAX_ARTIFACT_BYTES).map_err(|error| {
        failure(
            ScanFailureKind::Io,
            format!("could not fingerprint CLAP artifact: {error}"),
        )
    })?;

    // SAFETY: native loading is isolated to this disposable worker. The
    // controller applies a deadline and kills/quarantines this exact artifact
    // on a crash or hang. No safe wrapper can make a hostile dylib safe.
    let entry = unsafe { PluginEntry::load(&canonical_path) }.map_err(|error| {
        failure(
            ScanFailureKind::InvalidAbi,
            format!("could not load CLAP entry: {error}"),
        )
    })?;
    let factory = entry.get_plugin_factory().ok_or_else(|| {
        failure(
            ScanFailureKind::InvalidAbi,
            "CLAP entry has no plugin factory".into(),
        )
    })?;
    let count = factory.plugin_count();
    if count == 0 || count > request.maximum_descriptors {
        return Err(failure(
            ScanFailureKind::InvalidDescriptor,
            format!(
                "CLAP factory descriptor count {count} is outside 1..={}",
                request.maximum_descriptors
            ),
        ));
    }

    let host_info = HostInfo::new(
        "Audec CLAP Scanner",
        "Audec",
        "https://github.com/",
        env!("CARGO_PKG_VERSION"),
    )
    .map_err(|error| failure(ScanFailureKind::InvalidDescriptor, error.to_string()))?;
    let mut plugins = Vec::with_capacity(count as usize);
    for index in 0..count {
        let descriptor = factory.plugin_descriptor(index).ok_or_else(|| {
            failure(
                ScanFailureKind::InvalidDescriptor,
                format!("CLAP factory omitted descriptor {index}"),
            )
        })?;
        let id = required_utf8("plugin ID", descriptor.id())?;
        let name = required_utf8("plugin name", descriptor.name())?;
        let features = descriptor
            .features()
            .map(|feature| utf8("plugin feature", feature))
            .collect::<Result<BTreeSet<_>, _>>()?;
        let roles = roles(&features);

        let mut instance =
            PluginInstance::<()>::new(|_| (), |_| (), &entry, descriptor.id().unwrap(), &host_info)
                .map_err(|error| {
                    failure(
                        ScanFailureKind::InvalidDescriptor,
                        format!("could not instantiate {id} for metadata scan: {error}"),
                    )
                })?;
        let mut handle = instance.plugin_handle();
        let audio_ports = scan_audio_ports(&mut handle)?;
        let note_ports = scan_note_ports(&mut handle)?;
        let parameters = scan_parameters(&mut handle, request.maximum_parameters_per_plugin)?;
        let state = handle.get_extension::<PluginState>().is_some();
        let latency = handle.get_extension::<PluginLatency>();
        let latency_reporting = latency.is_some();
        let _initial_latency = latency.map(|extension| extension.get(&mut handle));
        let tail_reporting = handle.get_extension::<PluginTail>().is_some();
        let offline_processing = handle.get_extension::<PluginRender>().is_some();
        drop(handle);
        drop(instance);

        plugins.push(PluginMetadata {
            key: PluginKey {
                format: PluginFormat::Clap,
                identifier: id,
            },
            name,
            vendor: optional_utf8("vendor", descriptor.vendor())?,
            version: optional_utf8("version", descriptor.version())?,
            description: optional_utf8("description", descriptor.description())?,
            homepage: optional_utf8("homepage", descriptor.url())?,
            features,
            roles,
            audio_ports,
            note_ports,
            parameters,
            capabilities: ExecutionCapabilities {
                realtime_safe_claimed: false,
                hard_realtime_required: false,
                offline_processing,
                editor: false,
                state,
                latency_reporting,
                tail_reporting,
                determinism: DeterminismClass::Unknown,
            },
        });
    }

    let record = ScanRecord {
        schema_version: SCAN_SCHEMA_VERSION,
        canonical_path,
        artifact,
        scanner: provenance(),
        plugins,
    };
    record.validate().map_err(|error| {
        failure(
            ScanFailureKind::InvalidDescriptor,
            format!("CLAP metadata failed Audec validation: {error}"),
        )
    })?;
    Ok(record)
}

fn scan_audio_ports(
    handle: &mut clack_host::prelude::PluginMainThreadHandle<'_>,
) -> Result<Vec<AudioPortDescriptor>, ScanFailure> {
    let Some(extension) = handle.get_extension::<PluginAudioPorts>() else {
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
    for is_input in [true, false] {
        let count = extension.count(handle, is_input);
        if count > 4096 {
            return Err(failure(
                ScanFailureKind::InvalidDescriptor,
                "CLAP audio port count exceeds 4096".into(),
            ));
        }
        for index in 0..count {
            let mut buffer = AudioPortInfoBuffer::new();
            let info = extension
                .get(handle, index, is_input, &mut buffer)
                .ok_or_else(|| {
                    failure(
                        ScanFailureKind::InvalidDescriptor,
                        format!("CLAP omitted audio port {index}"),
                    )
                })?;
            let channels = u16::try_from(info.channel_count).map_err(|_| {
                failure(
                    ScanFailureKind::InvalidDescriptor,
                    "CLAP audio port channel count exceeds u16".into(),
                )
            })?;
            let layout = match channels {
                1 => ChannelLayout::Mono,
                2 => ChannelLayout::Stereo,
                other => ChannelLayout::Discrete(other),
            };
            result.push(AudioPortDescriptor {
                native_id: info.id.get(),
                name: utf8_bytes("audio port name", info.name)?,
                direction: if is_input {
                    PortDirection::Input
                } else {
                    PortDirection::Output
                },
                role: if info.flags.contains(AudioPortFlags::IS_MAIN) {
                    AudioPortRole::Main
                } else {
                    AudioPortRole::Auxiliary
                },
                layouts: vec![layout],
                required: info.flags.contains(AudioPortFlags::IS_MAIN),
            });
        }
    }
    Ok(result)
}

fn scan_note_ports(
    handle: &mut clack_host::prelude::PluginMainThreadHandle<'_>,
) -> Result<Vec<NotePortDescriptor>, ScanFailure> {
    let Some(extension) = handle.get_extension::<PluginNotePorts>() else {
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
    for is_input in [true, false] {
        let count = extension.count(handle, is_input);
        if count > 4096 {
            return Err(failure(
                ScanFailureKind::InvalidDescriptor,
                "CLAP note port count exceeds 4096".into(),
            ));
        }
        for index in 0..count {
            let mut buffer = NotePortInfoBuffer::new();
            let info = extension
                .get(handle, index, is_input, &mut buffer)
                .ok_or_else(|| {
                    failure(
                        ScanFailureKind::InvalidDescriptor,
                        format!("CLAP omitted note port {index}"),
                    )
                })?;
            let dialects = note_dialects(info.supported_dialects);
            let preferred = info.preferred_dialect.map(note_dialect);
            result.push(NotePortDescriptor {
                native_id: info.id.get(),
                name: utf8_bytes("note port name", info.name)?,
                direction: if is_input {
                    PortDirection::Input
                } else {
                    PortDirection::Output
                },
                dialects,
                preferred,
            });
        }
    }
    Ok(result)
}

fn scan_parameters(
    handle: &mut clack_host::prelude::PluginMainThreadHandle<'_>,
    maximum: u32,
) -> Result<Vec<PluginParameterDescriptor>, ScanFailure> {
    let Some(extension) = handle.get_extension::<PluginParams>() else {
        return Ok(Vec::new());
    };
    let count = extension.count(handle);
    if count > maximum {
        return Err(failure(
            ScanFailureKind::InvalidDescriptor,
            format!("CLAP parameter count {count} exceeds limit {maximum}"),
        ));
    }
    let mut result = Vec::with_capacity(count as usize);
    for index in 0..count {
        let mut buffer = ParamInfoBuffer::new();
        let info = extension
            .get_info(handle, index, &mut buffer)
            .ok_or_else(|| {
                failure(
                    ScanFailureKind::InvalidDescriptor,
                    format!("CLAP omitted parameter {index}"),
                )
            })?;
        let mapping = if info.flags.contains(ParamInfoFlags::IS_STEPPED) {
            let span = info.max_value - info.min_value;
            let steps = if span.is_finite() && span >= 1.0 && span <= (u32::MAX - 1) as f64 {
                span.round() as u32 + 1
            } else {
                return Err(failure(
                    ScanFailureKind::InvalidDescriptor,
                    format!("invalid stepped range for CLAP parameter {}", info.id.get()),
                ));
            };
            ParameterMapping::Stepped { steps }
        } else {
            ParameterMapping::Linear
        };
        result.push(PluginParameterDescriptor {
            key: PluginParameterKey::Clap(info.id.get()),
            name: utf8_bytes("parameter name", info.name)?,
            module: optional_utf8_bytes("parameter module", info.module)?,
            unit: None,
            plain_min: info.min_value,
            plain_max: info.max_value,
            plain_default: info.default_value,
            mapping,
            automatable: info.flags.contains(ParamInfoFlags::IS_AUTOMATABLE),
            modulatable: info.flags.contains(ParamInfoFlags::IS_MODULATABLE),
            read_only: info.flags.contains(ParamInfoFlags::IS_READONLY),
            hidden: info.flags.contains(ParamInfoFlags::IS_HIDDEN),
        });
    }
    Ok(result)
}

fn roles(features: &BTreeSet<String>) -> BTreeSet<PluginRole> {
    let mut roles = BTreeSet::new();
    if features.iter().any(|feature| {
        matches!(
            feature.as_str(),
            "instrument" | "synthesizer" | "sampler" | "drum" | "drum-machine"
        )
    }) {
        roles.insert(PluginRole::Instrument);
    }
    if features.contains("audio-effect") {
        roles.insert(PluginRole::AudioEffect);
    }
    if features.contains("note-effect") {
        roles.insert(PluginRole::NoteEffect);
    }
    if features.contains("analyzer") {
        roles.insert(PluginRole::Analyzer);
    }
    if roles.is_empty() {
        roles.insert(PluginRole::Utility);
    }
    roles
}

fn note_dialects(value: NoteDialects) -> BTreeSet<NoteDialect> {
    let mut result = BTreeSet::new();
    if value.contains(NoteDialects::CLAP) {
        result.insert(NoteDialect::Clap);
    }
    if value.intersects(NoteDialects::MIDI | NoteDialects::MIDI_MPE) {
        result.insert(NoteDialect::Midi1);
    }
    if value.contains(NoteDialects::MIDI2) {
        result.insert(NoteDialect::Midi2);
    }
    result
}

fn note_dialect(value: ClackNoteDialect) -> NoteDialect {
    match value {
        ClackNoteDialect::Clap => NoteDialect::Clap,
        ClackNoteDialect::Midi | ClackNoteDialect::MidiMpe => NoteDialect::Midi1,
        ClackNoteDialect::Midi2 => NoteDialect::Midi2,
    }
}

fn required_utf8(field: &'static str, value: Option<&CStr>) -> Result<String, ScanFailure> {
    let value = value.ok_or_else(|| {
        failure(
            ScanFailureKind::InvalidDescriptor,
            format!("CLAP descriptor is missing {field}"),
        )
    })?;
    utf8(field, value)
}

fn optional_utf8(field: &'static str, value: Option<&CStr>) -> Result<Option<String>, ScanFailure> {
    value.map(|value| utf8(field, value)).transpose()
}

fn utf8(field: &'static str, value: &CStr) -> Result<String, ScanFailure> {
    utf8_bytes(field, value.to_bytes())
}

fn optional_utf8_bytes(field: &'static str, value: &[u8]) -> Result<Option<String>, ScanFailure> {
    if value.is_empty() {
        Ok(None)
    } else {
        utf8_bytes(field, value).map(Some)
    }
}

fn utf8_bytes(field: &'static str, value: &[u8]) -> Result<String, ScanFailure> {
    std::str::from_utf8(value).map(str::to_owned).map_err(|_| {
        failure(
            ScanFailureKind::InvalidDescriptor,
            format!("CLAP {field} is not valid UTF-8"),
        )
    })
}

fn failure(kind: ScanFailureKind, detail: String) -> ScanFailure {
    ScanFailure {
        kind,
        detail,
        scanner: provenance(),
    }
}

fn provenance() -> ScannerProvenance {
    ScannerProvenance {
        scanner_name: "audec-clap-worker".into(),
        scanner_version: env!("CARGO_PKG_VERSION").into(),
        scanner_build: plugin_wire::digest_bytes(WORKER_BUILD_INPUT),
        host_os: std::env::consts::OS.into(),
        host_architecture: current_architecture(),
        hash_algorithm: "sha256".into(),
    }
}

fn current_architecture() -> CpuArchitecture {
    match std::env::consts::ARCH {
        "aarch64" => CpuArchitecture::Aarch64,
        "x86_64" => CpuArchitecture::X86_64,
        _ => CpuArchitecture::Other,
    }
}

#[derive(Clone, Copy, Debug)]
struct RuntimeParameter {
    minimum: f64,
    maximum: f64,
    stepped: bool,
}

/// A native CLAP instance that only exists inside `audec-clap-worker`.
/// The DAW engine remains the block/graph authority; this adapter processes
/// exactly one block after the worker receives its matching control message.
pub struct NativeClapInstance {
    _entry: PluginEntry,
    instance: PluginInstance<()>,
    processor: Option<PluginAudioProcessor<()>>,
    contract: ProcessingContract,
    transport: Option<SharedBlockTransport>,
    parameters: BTreeMap<u32, RuntimeParameter>,
    pending_parameters: BTreeMap<u32, f64>,
    note_port_indexes: BTreeMap<u32, u16>,
    input_channels: usize,
    output_channels: usize,
    steady_time: u64,
}

impl NativeClapInstance {
    pub fn instantiate(
        request: &InstantiateDto,
        session_root: &Path,
    ) -> Result<(Self, u32, TailReport), String> {
        let grant = request.artifact.as_ref().ok_or_else(|| {
            "native CLAP instantiate requires a verified artifact grant".to_owned()
        })?;
        let requested_path = PathBuf::from(&grant.canonical_path);
        let path = fs::canonicalize(&requested_path)
            .map_err(|error| format!("could not canonicalize granted CLAP artifact: {error}"))?;
        if path != requested_path {
            return Err("native CLAP artifact grant is not canonical".into());
        }
        let expected = grant
            .fingerprint
            .to_domain()
            .map_err(|error| error.to_string())?;
        let actual = fingerprint_artifact(&path, MAX_ARTIFACT_BYTES)
            .map_err(|error| format!("could not fingerprint granted CLAP artifact: {error}"))?;
        if actual.content != expected.content || actual.byte_len != expected.byte_len {
            return Err("native CLAP artifact changed after scan".into());
        }
        let plugin = request
            .plugin
            .to_domain()
            .map_err(|error| error.to_string())?;
        if plugin.format != PluginFormat::Clap {
            return Err("native CLAP worker refuses non-CLAP format".into());
        }
        let contract = request
            .contract
            .to_domain()
            .map_err(|error| error.to_string())?;
        let input_ports = contract
            .audio_ports
            .iter()
            .filter(|port| port.direction == PortDirection::Input)
            .collect::<Vec<_>>();
        let output_ports = contract
            .audio_ports
            .iter()
            .filter(|port| port.direction == PortDirection::Output)
            .collect::<Vec<_>>();
        if input_ports.len() > 1
            || output_ports.len() > 1
            || input_ports
                .first()
                .is_some_and(|port| port.channel_offset != 0)
            || output_ports
                .first()
                .is_some_and(|port| port.channel_offset != 0)
        {
            return Err(
                "CLAP runtime slice supports one contiguous f32 port per direction; multi-bus/f64 is unsupported"
                    .into(),
            );
        }
        let input_channels = input_ports
            .first()
            .map_or(0, |port| port.layout.channels() as usize);
        let output_channels = output_ports
            .first()
            .map_or(0, |port| port.layout.channels() as usize);

        // SAFETY: this is the dedicated crash-contained worker. The supervisor
        // verifies bytes before launch and kills the entire child on ABI fault,
        // timeout, malformed output, or protocol failure.
        let entry = unsafe { PluginEntry::load(&path) }
            .map_err(|error| format!("could not load granted CLAP entry: {error}"))?;
        let factory = entry
            .get_plugin_factory()
            .ok_or_else(|| "CLAP entry has no plugin factory".to_owned())?;
        let descriptor = factory
            .plugin_descriptors()
            .find(|descriptor| {
                descriptor
                    .id()
                    .is_some_and(|id| id.to_bytes() == plugin.identifier.as_bytes())
            })
            .ok_or_else(|| "granted artifact does not expose requested CLAP ID".to_owned())?;
        let host_info = HostInfo::new(
            "Audec CLAP Runtime",
            "Audec",
            "https://github.com/",
            env!("CARGO_PKG_VERSION"),
        )
        .map_err(|error| error.to_string())?;
        let mut instance =
            PluginInstance::<()>::new(|_| (), |_| (), &entry, descriptor.id().unwrap(), &host_info)
                .map_err(|error| format!("could not instantiate CLAP plugin: {error}"))?;

        let mut parameters = BTreeMap::new();
        let mut note_port_indexes = BTreeMap::new();
        let latency_frames;
        {
            let mut handle = instance.plugin_handle();
            if contract.offline {
                let render = handle.get_extension::<PluginRender>().ok_or_else(|| {
                    "plugin does not implement CLAP render extension required for offline mode"
                        .to_owned()
                })?;
                render
                    .set(&mut handle, RenderMode::Offline)
                    .map_err(|error| format!("plugin refused offline render mode: {error}"))?;
            }
            if let Some(extension) = handle.get_extension::<PluginParams>() {
                for index in 0..extension.count(&mut handle) {
                    let mut buffer = ParamInfoBuffer::new();
                    let Some(info) = extension.get_info(&mut handle, index, &mut buffer) else {
                        continue;
                    };
                    parameters.insert(
                        info.id.get(),
                        RuntimeParameter {
                            minimum: info.min_value,
                            maximum: info.max_value,
                            stepped: info.flags.contains(ParamInfoFlags::IS_STEPPED),
                        },
                    );
                }
            }
            if let Some(extension) = handle.get_extension::<PluginNotePorts>() {
                for index in 0..extension.count(&mut handle, true) {
                    let mut buffer = NotePortInfoBuffer::new();
                    if let Some(info) = extension.get(&mut handle, index, true, &mut buffer) {
                        note_port_indexes.insert(info.id.get(), index as u16);
                    }
                }
            }
            if let (Some(state), Some(extension)) = (
                request.state.as_ref(),
                handle.get_extension::<PluginState>(),
            ) {
                let bytes = fs::read(session_root.join(&state.relative_path))
                    .map_err(|error| format!("could not read initial CLAP state: {error}"))?;
                let blob = state
                    .clone()
                    .into_blob(bytes)
                    .map_err(|error| error.to_string())?;
                extension
                    .load(&mut handle, &mut Cursor::new(blob.bytes))
                    .map_err(|error| error.to_string())?;
            }
            latency_frames = handle
                .get_extension::<PluginLatency>()
                .map_or(0, |extension| extension.get(&mut handle));
        }

        let initial_tail = contract.initial_tail;
        Ok((
            Self {
                _entry: entry,
                instance,
                processor: None,
                contract,
                transport: None,
                parameters,
                pending_parameters: BTreeMap::new(),
                note_port_indexes,
                input_channels,
                output_channels,
                steady_time: 0,
            },
            latency_frames,
            initial_tail,
        ))
    }

    pub fn bind(&mut self, binding: &SharedMemoryBindingDto) -> Result<(), String> {
        if self.transport.is_some() {
            return Err("CLAP instance already has shared memory".into());
        }
        self.transport = Some(
            SharedBlockTransport::open(&self.contract, binding, DEFAULT_MAX_EVENTS)
                .map_err(|error| error.to_string())?,
        );
        Ok(())
    }

    pub fn set_parameters(&mut self, values: &[ParameterValueDto]) -> Result<(), String> {
        for value in values {
            let (key, normalized) = value.to_domain().map_err(|error| error.to_string())?;
            let PluginParameterKey::Clap(id) = key else {
                return Err("CLAP worker received a non-CLAP parameter key".into());
            };
            let descriptor = self
                .parameters
                .get(&id)
                .ok_or_else(|| format!("unknown CLAP parameter {id}"))?;
            let mut plain =
                descriptor.minimum + normalized.get() * (descriptor.maximum - descriptor.minimum);
            if descriptor.stepped {
                plain = plain.round();
            }
            self.pending_parameters.insert(id, plain);
        }
        Ok(())
    }

    pub fn activate(&mut self) -> Result<(), String> {
        if self.transport.is_none() {
            return Err("shared memory must be bound before activation".into());
        }
        if self.processor.is_some() {
            return Err("CLAP instance is already active".into());
        }
        let stopped = self
            .instance
            .activate(
                |_, _| (),
                PluginAudioConfiguration {
                    sample_rate: self.contract.sample_rate as f64,
                    min_frames_count: self.contract.minimum_frames,
                    max_frames_count: self.contract.maximum_frames,
                },
            )
            .map_err(|error| format!("CLAP activation failed: {error}"))?;
        let started = stopped
            .start_processing()
            .map_err(|_| "CLAP start_processing failed".to_owned())?;
        self.processor = Some(started.into());
        Ok(())
    }

    pub fn process(&mut self, frames: u32, declared_event_count: u32) -> Result<u32, String> {
        if frames < self.contract.minimum_frames || frames > self.contract.maximum_frames {
            return Err("process frame count is outside negotiated range".into());
        }
        if self.processor.is_none() {
            return Err("CLAP instance is not active".into());
        }
        let mut input = vec![Vec::new(); self.input_channels];
        let mut transport_events = self
            .transport
            .as_ref()
            .ok_or_else(|| "CLAP instance has no shared memory".to_owned())?
            .worker_read_inputs(frames, &mut input)
            .map_err(|error| error.to_string())?;
        if transport_events.len() != declared_event_count as usize {
            return Err("shared event count does not match Process control message".into());
        }
        for (id, plain) in std::mem::take(&mut self.pending_parameters) {
            transport_events.insert(
                0,
                InputEvent::Parameter(crate::plugin::ParameterEvent {
                    frame_offset: 0,
                    key: PluginParameterKey::Clap(id),
                    value: crate::plugin::NormalizedValue::new(
                        (plain - self.parameters[&id].minimum)
                            / (self.parameters[&id].maximum - self.parameters[&id].minimum),
                    )
                    .map_err(|error| error.to_string())?,
                }),
            );
        }
        transport_events.sort_by_key(|event| match event {
            InputEvent::Parameter(event) => event.frame_offset,
            InputEvent::Note(event) => event.frame_offset,
        });
        let mut input_events = EventBuffer::with_capacity(transport_events.len());
        for event in &transport_events {
            self.push_clap_event(&mut input_events, event)?;
        }
        let mut output_events = EventBuffer::with_capacity(DEFAULT_MAX_EVENTS as usize);
        let mut output = vec![vec![0.0; frames as usize]; self.output_channels];
        let mut input_ports =
            AudioPorts::with_capacity(self.input_channels, usize::from(self.input_channels != 0));
        let mut output_ports =
            AudioPorts::with_capacity(self.output_channels, usize::from(self.output_channels != 0));
        let input_audio = if self.input_channels == 0 {
            InputAudioBuffers::empty()
        } else {
            input_ports.with_input_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_input_only(
                    input
                        .iter_mut()
                        .map(|channel| InputChannel::variable(channel)),
                ),
            }])
        };
        let mut output_audio = if self.output_channels == 0 {
            OutputAudioBuffers::empty()
        } else {
            output_ports.with_output_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_output_only(
                    output.iter_mut().map(Vec::as_mut_slice),
                ),
            }])
        };
        let processor = self
            .processor
            .as_mut()
            .expect("processor checked above")
            .ensure_processing_started()
            .map_err(|error| format!("CLAP processing could not start: {error}"))?;
        processor
            .process(
                &input_audio,
                &mut output_audio,
                &input_events.as_input(),
                &mut output_events.as_output(),
                Some(self.steady_time),
                None,
            )
            .map_err(|error| format!("CLAP process failed: {error}"))?;
        self.steady_time = self.steady_time.wrapping_add(frames as u64);
        if !output_events.is_empty() {
            return Err(
                "CLAP plugin emitted output events; output-event transport is unsupported".into(),
            );
        }
        let outputs = output.iter().map(Vec::as_slice).collect::<Vec<_>>();
        self.transport
            .as_mut()
            .expect("transport checked above")
            .worker_write_outputs(frames, &outputs)
            .map_err(|error| error.to_string())?;
        Ok(0)
    }

    fn push_clap_event(&self, buffer: &mut EventBuffer, event: &InputEvent) -> Result<(), String> {
        match event {
            InputEvent::Parameter(event) => {
                let PluginParameterKey::Clap(id) = event.key else {
                    return Err("non-CLAP parameter event".into());
                };
                let descriptor = self
                    .parameters
                    .get(&id)
                    .ok_or_else(|| format!("unknown CLAP parameter {id}"))?;
                let mut value = descriptor.minimum
                    + event.value.get() * (descriptor.maximum - descriptor.minimum);
                if descriptor.stepped {
                    value = value.round();
                }
                buffer.push(&ParamValueEvent::new(
                    event.frame_offset,
                    ClapId::new(id),
                    Pckn::match_all(),
                    value,
                    Cookie::empty(),
                ));
            }
            InputEvent::Note(event) => {
                let port = *self
                    .note_port_indexes
                    .get(&event.address.port)
                    .ok_or_else(|| format!("unknown CLAP note port {}", event.address.port))?;
                let pckn = Pckn::new(
                    port,
                    event.address.channel,
                    event.address.key,
                    event.address.note_id,
                );
                match event.kind {
                    PluginNoteEventKind::On { velocity } => {
                        buffer.push(&NoteOnEvent::new(event.frame_offset, pckn, velocity.get()))
                    }
                    PluginNoteEventKind::Off { velocity } => {
                        buffer.push(&NoteOffEvent::new(event.frame_offset, pckn, velocity.get()))
                    }
                    PluginNoteEventKind::Choke => {
                        buffer.push(&NoteChokeEvent::new(event.frame_offset, pckn))
                    }
                    PluginNoteEventKind::Expression { dimension, value } => {
                        let (expression, clap_value) = match dimension {
                            PluginNoteExpression::Pressure => {
                                (NoteExpressionType::Pressure, value.clamp(0.0, 1.0))
                            }
                            PluginNoteExpression::Tuning => {
                                (NoteExpressionType::Tuning, value * 120.0)
                            }
                            PluginNoteExpression::Brightness | PluginNoteExpression::Timbre => {
                                (NoteExpressionType::Brightness, value.clamp(0.0, 1.0))
                            }
                            PluginNoteExpression::Pan => {
                                (NoteExpressionType::Pan, (value + 1.0) * 0.5)
                            }
                            PluginNoteExpression::Volume => {
                                (NoteExpressionType::Volume, 4.0_f64.powf(value))
                            }
                            PluginNoteExpression::Other(_) => {
                                return Err("unsupported CLAP note-expression dimension".into());
                            }
                        };
                        buffer.push(&NoteExpressionEvent::new(
                            event.frame_offset,
                            pckn,
                            expression,
                            clap_value,
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn save_state(&mut self, maximum_bytes: u64) -> Result<Vec<u8>, String> {
        let mut handle = self.instance.plugin_handle();
        let extension = handle
            .get_extension::<PluginState>()
            .ok_or_else(|| "plugin does not implement CLAP state".to_owned())?;
        let mut bytes = Vec::new();
        extension
            .save(&mut handle, &mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() as u64 > maximum_bytes {
            return Err("plugin state exceeds requested bound".into());
        }
        Ok(bytes)
    }

    pub fn deactivate(&mut self) -> Result<(), String> {
        let processor = self
            .processor
            .take()
            .ok_or_else(|| "CLAP instance is not active".to_owned())?;
        self.instance.deactivate(processor.into_stopped());
        Ok(())
    }
}

impl Drop for NativeClapInstance {
    fn drop(&mut self) {
        if let Some(processor) = self.processor.take() {
            self.instance.deactivate(processor.into_stopped());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn self_built_fixture_executes_real_clap_dsp_over_shared_memory() {
        use std::process::Command;
        use std::time::{SystemTime, UNIX_EPOCH};

        use crate::plugin::{ChannelLayout, NegotiatedAudioPort, NormalizedValue, ParameterEvent};
        use crate::plugin_wire::{
            ArtifactDto, ArtifactGrantDto, PluginKeyDto, ProcessingContractDto, TokenDto,
        };
        use crate::plugin_worker::transport::{binding_for, InputEvent};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("audec-clap-direct-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        let target = root.join("target");
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "--quiet",
                "--locked",
                "--manifest-path",
                "tests/fixtures/clap_gain/Cargo.toml",
                "--target-dir",
            ])
            .arg(&target)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .unwrap();
        assert!(status.success());
        #[cfg(target_os = "macos")]
        let built = target.join("debug/libaudec_clap_gain_fixture.dylib");
        #[cfg(target_os = "linux")]
        let built = target.join("debug/libaudec_clap_gain_fixture.so");
        let artifact = root.join("fixture.clap");
        fs::copy(built, &artifact).unwrap();
        let artifact = fs::canonicalize(artifact).unwrap();
        let fingerprint = fingerprint_artifact(&artifact, MAX_ARTIFACT_BYTES).unwrap();
        let contract = ProcessingContract {
            sample_rate: 48_000,
            minimum_frames: 1,
            maximum_frames: 64,
            audio_ports: vec![
                NegotiatedAudioPort {
                    native_id: 0,
                    direction: PortDirection::Input,
                    layout: ChannelLayout::Stereo,
                    channel_offset: 0,
                },
                NegotiatedAudioPort {
                    native_id: 0,
                    direction: PortDirection::Output,
                    layout: ChannelLayout::Stereo,
                    channel_offset: 0,
                },
            ],
            note_inputs: BTreeMap::new(),
            note_outputs: BTreeMap::new(),
            initial_latency_frames: 0,
            initial_tail: TailReport::Unknown,
            offline: false,
        };
        let binding = binding_for(
            TokenDto::new(1),
            &contract,
            DEFAULT_MAX_EVENTS,
            nonce & ((1_u128 << 120) - 1),
        )
        .unwrap();
        let mut controller =
            SharedBlockTransport::create(&contract, binding.clone(), DEFAULT_MAX_EVENTS).unwrap();
        let request = InstantiateDto {
            request_id: 1,
            instance: TokenDto::new(1),
            artifact_lease: TokenDto::new(2),
            plugin: PluginKeyDto::from_domain(&PluginKey {
                format: PluginFormat::Clap,
                identifier: "dev.audec.fixture.gain".into(),
            })
            .unwrap(),
            contract: ProcessingContractDto::from_domain(&contract).unwrap(),
            artifact: Some(ArtifactGrantDto {
                canonical_path: artifact.to_string_lossy().into_owned(),
                fingerprint: ArtifactDto::from_domain(&fingerprint).unwrap(),
            }),
            state: None,
        };
        let (mut native, latency, _) = NativeClapInstance::instantiate(&request, &root).unwrap();
        assert_eq!(latency, 0);
        native.bind(&binding).unwrap();
        native.activate().unwrap();
        let left = [1.0_f32; 4];
        let right = [0.5_f32; 4];
        controller
            .controller_write_inputs(
                4,
                &[&left, &right],
                &[InputEvent::Parameter(ParameterEvent {
                    frame_offset: 2,
                    key: PluginParameterKey::Clap(1),
                    value: NormalizedValue::new(0.25).unwrap(),
                })],
            )
            .unwrap();
        native.process(4, 1).unwrap();
        let mut output = vec![Vec::new(), Vec::new()];
        controller.controller_read_outputs(4, &mut output).unwrap();
        assert_eq!(output[0], vec![1.0, 1.0, 0.25, 0.25]);
        assert_eq!(output[1], vec![0.5, 0.5, 0.125, 0.125]);
        assert_eq!(native.save_state(64).unwrap(), 0.25_f32.to_le_bytes());
        native.deactivate().unwrap();
        drop(controller);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn real_worker_advertises_only_the_executable_clap_slice() {
        let capabilities = capabilities();
        assert!(capabilities.scanning);
        assert!(capabilities.realtime);
        assert!(capabilities.offline);
        assert!(capabilities.shared_memory);
        assert_eq!(
            RUNTIME_SUPPORT,
            RealClapRuntimeSupport::IsolatedF32SingleBus
        );
    }

    #[test]
    fn feature_roles_preserve_instrument_and_effect_identity() {
        assert_eq!(
            roles(&BTreeSet::from([
                "instrument".into(),
                "audio-effect".into()
            ])),
            BTreeSet::from([PluginRole::AudioEffect, PluginRole::Instrument])
        );
        assert_eq!(
            roles(&BTreeSet::new()),
            BTreeSet::from([PluginRole::Utility])
        );
    }
}
