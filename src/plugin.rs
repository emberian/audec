//! Persistent plugin metadata and process-boundary contracts.
//!
//! This module intentionally does **not** load or execute a plugin. It is the
//! audec-owned seam between project/mixer state, an out-of-process scanner,
//! and a future CLAP (then VST3) runtime adapter. Consequently project parsing
//! can validate and preserve a plugin without ever mapping unknown code into
//! the GPUI process.
//!
//! The first executable host should implement these contracts with Clack, but
//! must not leak Clack or raw ABI types through this module.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

pub const SCAN_SCHEMA_VERSION: u32 = 1;
pub const WORKER_PROTOCOL_VERSION: u32 = 1;

/// A content digest supplied by the asset hasher or scanner.
///
/// The core contract does not pick a hashing implementation. Callers should
/// currently use SHA-256 and identify that algorithm in provenance.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Digest32(pub [u8; 32]);

impl Digest32 {
    pub const ZERO: Self = Self([0; 32]);

    pub fn from_hex(value: &str) -> Result<Self, PluginValidationError> {
        if value.len() != 64 {
            return Err(PluginValidationError::InvalidDigest);
        }
        let mut bytes = [0; 32];
        for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = (hex_digit(chunk[0])? << 4) | hex_digit(chunk[1])?;
        }
        Ok(Self(bytes))
    }

    pub fn to_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut value = String::with_capacity(64);
        for byte in self.0 {
            value.push(HEX[(byte >> 4) as usize] as char);
            value.push(HEX[(byte & 0x0f) as usize] as char);
        }
        value
    }
}

fn hex_digit(value: u8) -> Result<u8, PluginValidationError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(PluginValidationError::InvalidDigest),
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PluginFormat {
    Clap,
    Vst3,
    AudioUnit,
    /// Allows state-preserving import before audec gains an adapter.
    Other(String),
}

impl PluginFormat {
    pub fn stable_name(&self) -> &str {
        match self {
            Self::Clap => "clap",
            Self::Vst3 => "vst3",
            Self::AudioUnit => "audio-unit",
            Self::Other(name) => name,
        }
    }

    pub fn is_executable_target(&self) -> bool {
        matches!(self, Self::Clap)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum IsolationMode {
    /// Allowed only after explicit trust; never used by descriptor scanning.
    TrustedInProcess,
    DedicatedWorker,
    SharedWorker,
}

/// One installed format adapter/runtime, separate from plugin metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginBackendDescriptor {
    pub stable_id: String,
    pub name: String,
    pub version: String,
    pub build: Digest32,
    pub formats: BTreeSet<PluginFormat>,
    pub architectures: BTreeSet<CpuArchitecture>,
    pub isolation_modes: BTreeSet<IsolationMode>,
    pub realtime: bool,
    pub offline: bool,
    pub editor_bridge: bool,
}

impl PluginBackendDescriptor {
    pub fn validate(&self) -> Result<(), PluginValidationError> {
        validate_text("backend ID", &self.stable_id, 1, 256)?;
        validate_text("backend name", &self.name, 1, 256)?;
        validate_text("backend version", &self.version, 1, 256)?;
        if self.build == Digest32::ZERO {
            return Err(PluginValidationError::ZeroBackendDigest);
        }
        if self.formats.is_empty()
            || self.architectures.is_empty()
            || self.isolation_modes.is_empty()
            || (!self.realtime && !self.offline)
        {
            return Err(PluginValidationError::InvalidBackendCapabilities);
        }
        for format in &self.formats {
            if let PluginFormat::Other(name) = format {
                validate_text("plugin format", name, 1, 64)?;
            }
        }
        Ok(())
    }

    pub fn supports(
        &self,
        format: &PluginFormat,
        architecture: CpuArchitecture,
        isolation: IsolationMode,
        offline: bool,
    ) -> bool {
        self.formats.contains(format)
            && (self.architectures.contains(&architecture)
                || self.architectures.contains(&CpuArchitecture::Universal))
            && self.isolation_modes.contains(&isolation)
            && if offline { self.offline } else { self.realtime }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PluginBackendRegistry {
    entries: BTreeMap<String, PluginBackendDescriptor>,
}

impl PluginBackendRegistry {
    pub fn insert(
        &mut self,
        descriptor: PluginBackendDescriptor,
    ) -> Result<(), PluginValidationError> {
        descriptor.validate()?;
        if self.entries.contains_key(&descriptor.stable_id) {
            return Err(PluginValidationError::DuplicateBackend(
                descriptor.stable_id,
            ));
        }
        self.entries
            .insert(descriptor.stable_id.clone(), descriptor);
        Ok(())
    }

    pub fn get(&self, stable_id: &str) -> Option<&PluginBackendDescriptor> {
        self.entries.get(stable_id)
    }

    /// Results are stable-ID ordered so selection is reproducible. Policy, not
    /// registration order, chooses among multiple viable backends.
    pub fn compatible(
        &self,
        format: &PluginFormat,
        architecture: CpuArchitecture,
        isolation: IsolationMode,
        offline: bool,
    ) -> Vec<&PluginBackendDescriptor> {
        self.entries
            .values()
            .filter(|backend| backend.supports(format, architecture, isolation, offline))
            .collect()
    }
}

/// Format-native identity; never a scan-order index or filesystem path.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PluginKey {
    pub format: PluginFormat,
    pub identifier: String,
}

impl PluginKey {
    pub fn validate(&self) -> Result<(), PluginValidationError> {
        validate_text("plugin identifier", &self.identifier, 1, 1024)?;
        if let PluginFormat::Other(name) = &self.format {
            validate_text("plugin format", name, 1, 64)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CpuArchitecture {
    Aarch64,
    X86_64,
    Universal,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PluginRole {
    AudioEffect,
    Instrument,
    NoteEffect,
    Analyzer,
    Utility,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PortDirection {
    Input,
    Output,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AudioPortRole {
    Main,
    Auxiliary,
    Sidechain,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ChannelLayout {
    Mono,
    Stereo,
    Discrete(u16),
}

impl ChannelLayout {
    pub fn channels(self) -> u16 {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::Discrete(channels) => channels,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioPortDescriptor {
    pub native_id: u32,
    pub name: String,
    pub direction: PortDirection,
    pub role: AudioPortRole,
    pub layouts: Vec<ChannelLayout>,
    pub required: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum NoteDialect {
    Clap,
    Midi1,
    Midi2,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotePortDescriptor {
    pub native_id: u32,
    pub name: String,
    pub direction: PortDirection,
    pub dialects: BTreeSet<NoteDialect>,
    pub preferred: Option<NoteDialect>,
}

/// A stable automation identity in the plugin's native namespace.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PluginParameterKey {
    Clap(u32),
    Vst3([u8; 16]),
    AudioUnit { scope: u32, element: u32, id: u32 },
    Opaque(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParameterMapping {
    Linear,
    Logarithmic,
    Stepped { steps: u32 },
    Enumerated(Vec<String>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PluginParameterDescriptor {
    pub key: PluginParameterKey,
    pub name: String,
    pub module: Option<String>,
    pub unit: Option<String>,
    pub plain_min: f64,
    pub plain_max: f64,
    pub plain_default: f64,
    pub mapping: ParameterMapping,
    pub automatable: bool,
    pub modulatable: bool,
    pub read_only: bool,
    pub hidden: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeterminismClass {
    /// Repeated offline runs are expected to agree for identical state/events.
    Deterministic,
    /// Determinism requires the host to provide a recorded seed.
    Seeded,
    /// Network, wall clock, external hardware, or undocumented entropy exists.
    NotGuaranteed,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionCapabilities {
    pub realtime_safe_claimed: bool,
    pub hard_realtime_required: bool,
    pub offline_processing: bool,
    pub editor: bool,
    pub state: bool,
    pub latency_reporting: bool,
    pub tail_reporting: bool,
    pub determinism: DeterminismClass,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PluginMetadata {
    pub key: PluginKey,
    pub name: String,
    pub vendor: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub features: BTreeSet<String>,
    pub roles: BTreeSet<PluginRole>,
    pub audio_ports: Vec<AudioPortDescriptor>,
    pub note_ports: Vec<NotePortDescriptor>,
    pub parameters: Vec<PluginParameterDescriptor>,
    pub capabilities: ExecutionCapabilities,
}

impl PluginMetadata {
    pub fn validate(&self) -> Result<(), PluginValidationError> {
        self.key.validate()?;
        validate_text("plugin name", &self.name, 1, 1024)?;
        validate_optional_text("vendor", self.vendor.as_deref(), 1024)?;
        validate_optional_text("version", self.version.as_deref(), 256)?;

        let mut audio_ids = BTreeSet::new();
        for port in &self.audio_ports {
            validate_text("audio port name", &port.name, 1, 1024)?;
            if !audio_ids.insert((port.direction, port.native_id)) {
                return Err(PluginValidationError::DuplicateAudioPort {
                    direction: port.direction,
                    native_id: port.native_id,
                });
            }
            if port.layouts.is_empty() || port.layouts.iter().any(|layout| layout.channels() == 0) {
                return Err(PluginValidationError::InvalidChannelLayout(port.native_id));
            }
        }

        let mut note_ids = BTreeSet::new();
        for port in &self.note_ports {
            validate_text("note port name", &port.name, 1, 1024)?;
            if !note_ids.insert((port.direction, port.native_id)) {
                return Err(PluginValidationError::DuplicateNotePort {
                    direction: port.direction,
                    native_id: port.native_id,
                });
            }
            if port.dialects.is_empty()
                || port
                    .preferred
                    .is_some_and(|value| !port.dialects.contains(&value))
            {
                return Err(PluginValidationError::InvalidNoteDialects(port.native_id));
            }
        }

        let mut keys = BTreeSet::new();
        for parameter in &self.parameters {
            validate_parameter(parameter)?;
            if !keys.insert(parameter.key.clone()) {
                return Err(PluginValidationError::DuplicateParameter(
                    parameter.key.clone(),
                ));
            }
        }
        Ok(())
    }

    /// Conservative behavior when executable code cannot be restored.
    pub fn missing_behavior(&self) -> MissingPluginBehavior {
        if self.roles.contains(&PluginRole::Instrument) {
            MissingPluginBehavior::Silence
        } else if self.roles.contains(&PluginRole::AudioEffect) {
            MissingPluginBehavior::BypassAudio
        } else {
            MissingPluginBehavior::Silence
        }
    }
}

fn validate_parameter(value: &PluginParameterDescriptor) -> Result<(), PluginValidationError> {
    validate_text("parameter name", &value.name, 1, 1024)?;
    if let PluginParameterKey::Opaque(key) = &value.key {
        validate_text("opaque parameter key", key, 1, 1024)?;
    }
    if !value.plain_min.is_finite()
        || !value.plain_max.is_finite()
        || !value.plain_default.is_finite()
        || value.plain_min > value.plain_max
        || !(value.plain_min..=value.plain_max).contains(&value.plain_default)
    {
        return Err(PluginValidationError::InvalidParameterRange(
            value.key.clone(),
        ));
    }
    match &value.mapping {
        ParameterMapping::Logarithmic if value.plain_min <= 0.0 => {
            return Err(PluginValidationError::InvalidParameterMapping(
                value.key.clone(),
            ));
        }
        ParameterMapping::Stepped { steps } if *steps < 2 => {
            return Err(PluginValidationError::InvalidParameterMapping(
                value.key.clone(),
            ));
        }
        ParameterMapping::Enumerated(labels) if labels.is_empty() => {
            return Err(PluginValidationError::InvalidParameterMapping(
                value.key.clone(),
            ));
        }
        _ => {}
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactFingerprint {
    pub content: Digest32,
    pub byte_len: u64,
    pub architectures: BTreeSet<CpuArchitecture>,
}

impl ArtifactFingerprint {
    pub fn validate(&self) -> Result<(), PluginValidationError> {
        if self.content == Digest32::ZERO {
            return Err(PluginValidationError::ZeroArtifactDigest);
        }
        if self.byte_len == 0 {
            return Err(PluginValidationError::EmptyArtifact);
        }
        if self.architectures.is_empty() {
            return Err(PluginValidationError::MissingArchitecture);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScannerProvenance {
    pub scanner_name: String,
    pub scanner_version: String,
    pub scanner_build: Digest32,
    pub host_os: String,
    pub host_architecture: CpuArchitecture,
    pub hash_algorithm: String,
}

impl ScannerProvenance {
    fn validate(&self) -> Result<(), PluginValidationError> {
        validate_text("scanner name", &self.scanner_name, 1, 256)?;
        validate_text("scanner version", &self.scanner_version, 1, 256)?;
        validate_text("host OS", &self.host_os, 1, 256)?;
        validate_text("hash algorithm", &self.hash_algorithm, 1, 64)?;
        if self.scanner_build == Digest32::ZERO {
            return Err(PluginValidationError::ZeroScannerDigest);
        }
        Ok(())
    }
}

/// A successful out-of-process scan of one artifact.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanRecord {
    pub schema_version: u32,
    pub canonical_path: PathBuf,
    pub artifact: ArtifactFingerprint,
    pub scanner: ScannerProvenance,
    pub plugins: Vec<PluginMetadata>,
}

impl ScanRecord {
    pub fn validate(&self) -> Result<(), PluginValidationError> {
        if self.schema_version != SCAN_SCHEMA_VERSION {
            return Err(PluginValidationError::UnsupportedScanSchema(
                self.schema_version,
            ));
        }
        if !self.canonical_path.is_absolute() {
            return Err(PluginValidationError::NonAbsoluteArtifactPath);
        }
        self.artifact.validate()?;
        self.scanner.validate()?;
        if self.plugins.is_empty() {
            return Err(PluginValidationError::NoPluginDescriptors);
        }
        let mut keys = BTreeSet::new();
        for plugin in &self.plugins {
            plugin.validate()?;
            if !keys.insert(plugin.key.clone()) {
                return Err(PluginValidationError::DuplicatePlugin(plugin.key.clone()));
            }
        }
        Ok(())
    }

    pub fn canonicalize_order(&mut self) {
        self.plugins.sort_by(|a, b| a.key.cmp(&b.key));
        for plugin in &mut self.plugins {
            plugin
                .audio_ports
                .sort_by_key(|port| (port.direction, port.native_id));
            plugin
                .note_ports
                .sort_by_key(|port| (port.direction, port.native_id));
            plugin.parameters.sort_by(|a, b| a.key.cmp(&b.key));
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScanFailureKind {
    TimedOut,
    Crashed,
    InvalidAbi,
    InvalidDescriptor,
    PermissionDenied,
    UnsupportedArchitecture,
    Io,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanFailure {
    pub kind: ScanFailureKind,
    pub detail: String,
    pub scanner: ScannerProvenance,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ScanCacheEntry {
    Ready(ScanRecord),
    Failed {
        artifact: ArtifactFingerprint,
        failure: ScanFailure,
        consecutive_failures: u32,
    },
    Quarantined {
        artifact: ArtifactFingerprint,
        last_failure: ScanFailure,
        consecutive_failures: u32,
    },
}

impl ScanCacheEntry {
    pub fn fingerprint(&self) -> &ArtifactFingerprint {
        match self {
            Self::Ready(record) => &record.artifact,
            Self::Failed { artifact, .. } | Self::Quarantined { artifact, .. } => artifact,
        }
    }
}

/// Deterministic, path-keyed scanner cache.
///
/// A content change clears prior failure counts. Repeated crashes of identical
/// bytes become quarantine; they are never retried implicitly.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PluginIndex {
    entries: BTreeMap<PathBuf, ScanCacheEntry>,
}

impl PluginIndex {
    pub fn entries(&self) -> &BTreeMap<PathBuf, ScanCacheEntry> {
        &self.entries
    }

    pub fn apply_success(&mut self, mut record: ScanRecord) -> Result<(), PluginValidationError> {
        record.validate()?;
        record.canonicalize_order();
        self.entries
            .insert(record.canonical_path.clone(), ScanCacheEntry::Ready(record));
        Ok(())
    }

    pub fn apply_failure(
        &mut self,
        canonical_path: PathBuf,
        artifact: ArtifactFingerprint,
        failure: ScanFailure,
        quarantine_after: u32,
    ) -> Result<(), PluginValidationError> {
        if !canonical_path.is_absolute() {
            return Err(PluginValidationError::NonAbsoluteArtifactPath);
        }
        artifact.validate()?;
        failure.scanner.validate()?;
        validate_text("scan failure detail", &failure.detail, 1, 4096)?;
        if quarantine_after == 0 {
            return Err(PluginValidationError::InvalidQuarantineThreshold);
        }

        let previous = self.entries.get(&canonical_path);
        let count = if previous.is_some_and(|entry| entry.fingerprint() == &artifact) {
            match previous {
                Some(ScanCacheEntry::Failed {
                    consecutive_failures,
                    ..
                })
                | Some(ScanCacheEntry::Quarantined {
                    consecutive_failures,
                    ..
                }) => consecutive_failures.saturating_add(1),
                _ => 1,
            }
        } else {
            1
        };
        let entry = if count >= quarantine_after {
            ScanCacheEntry::Quarantined {
                artifact,
                last_failure: failure,
                consecutive_failures: count,
            }
        } else {
            ScanCacheEntry::Failed {
                artifact,
                failure,
                consecutive_failures: count,
            }
        };
        self.entries.insert(canonical_path, entry);
        Ok(())
    }

    pub fn needs_scan(&self, path: &Path, fingerprint: &ArtifactFingerprint) -> bool {
        match self.entries.get(path) {
            None => true,
            Some(ScanCacheEntry::Ready(record)) => &record.artifact != fingerprint,
            Some(ScanCacheEntry::Failed { .. }) => true,
            Some(ScanCacheEntry::Quarantined { artifact, .. }) if artifact == fingerprint => false,
            Some(ScanCacheEntry::Quarantined { .. }) => true,
        }
    }

    /// Explicit user action for retrying unchanged quarantined bytes.
    pub fn clear_quarantine(&mut self, path: &Path) -> bool {
        if matches!(
            self.entries.get(path),
            Some(ScanCacheEntry::Quarantined { .. })
        ) {
            self.entries.remove(path);
            true
        } else {
            false
        }
    }

    pub fn descriptor(&self, key: &PluginKey) -> Option<&PluginMetadata> {
        self.entries.values().find_map(|entry| match entry {
            ScanCacheEntry::Ready(record) => {
                record.plugins.iter().find(|plugin| &plugin.key == key)
            }
            _ => None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginStateBlob {
    pub plugin: PluginKey,
    pub state_format_version: u32,
    pub bytes: Vec<u8>,
    pub digest: Digest32,
}

impl PluginStateBlob {
    pub fn validate(&self, maximum_bytes: usize) -> Result<(), PluginValidationError> {
        self.plugin.validate()?;
        if self.bytes.len() > maximum_bytes {
            return Err(PluginValidationError::StateTooLarge {
                actual: self.bytes.len(),
                maximum: maximum_bytes,
            });
        }
        if !self.bytes.is_empty() && self.digest == Digest32::ZERO {
            return Err(PluginValidationError::ZeroStateDigest);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MissingPluginBehavior {
    /// Effect inputs reach outputs unchanged, subject to explicit channel-map
    /// validation by the graph compiler.
    BypassAudio,
    /// Emit silence and no note events. This is the safe instrument/unknown
    /// fallback: it never substitutes an unrelated sound.
    Silence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginUnavailableReason {
    MissingArtifact,
    ChangedArtifact,
    Quarantined,
    Crashed,
    UnsupportedFormat,
    UnsupportedArchitecture,
    IncompatiblePorts,
    ProtocolMismatch,
}

/// Persistent placeholder: nothing here requires executable plugin code.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginInstanceState {
    pub plugin: PluginKey,
    pub scanned_artifact: Option<Digest32>,
    pub state: Option<PluginStateBlob>,
    pub parameter_values: BTreeMap<PluginParameterKey, NormalizedValue>,
    pub unavailable: Option<PluginUnavailableReason>,
    pub missing_behavior: MissingPluginBehavior,
}

impl PluginInstanceState {
    pub fn validate(&self, maximum_state_bytes: usize) -> Result<(), PluginValidationError> {
        self.plugin.validate()?;
        if let Some(state) = &self.state {
            state.validate(maximum_state_bytes)?;
            if state.plugin != self.plugin {
                return Err(PluginValidationError::StatePluginMismatch);
            }
        }
        for value in self.parameter_values.values() {
            value.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NormalizedValue(f64);

impl NormalizedValue {
    pub fn new(value: f64) -> Result<Self, PluginValidationError> {
        if value.is_finite() && (0.0..=1.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(PluginValidationError::InvalidNormalizedValue)
        }
    }

    pub fn get(self) -> f64 {
        self.0
    }

    fn validate(self) -> Result<(), PluginValidationError> {
        Self::new(self.0).map(|_| ())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TailReport {
    None,
    FiniteFrames(u64),
    Infinite,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NegotiatedAudioPort {
    pub native_id: u32,
    pub direction: PortDirection,
    pub layout: ChannelLayout,
    /// Offset of this port's first channel in the shared planar buffer table.
    pub channel_offset: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessingContract {
    pub sample_rate: u32,
    pub minimum_frames: u32,
    pub maximum_frames: u32,
    pub audio_ports: Vec<NegotiatedAudioPort>,
    pub note_inputs: BTreeMap<u32, NoteDialect>,
    pub note_outputs: BTreeMap<u32, NoteDialect>,
    pub initial_latency_frames: u32,
    pub initial_tail: TailReport,
    pub offline: bool,
}

impl ProcessingContract {
    pub fn validate(&self) -> Result<(), PluginValidationError> {
        if self.sample_rate == 0
            || self.minimum_frames == 0
            || self.minimum_frames > self.maximum_frames
            || self.maximum_frames > 1_048_576
        {
            return Err(PluginValidationError::InvalidProcessingRange);
        }
        let mut ids = BTreeSet::new();
        let mut occupied = BTreeSet::new();
        for port in &self.audio_ports {
            if port.layout.channels() == 0 || !ids.insert((port.direction, port.native_id)) {
                return Err(PluginValidationError::InvalidNegotiatedPort(port.native_id));
            }
            for channel in 0..port.layout.channels() {
                let slot = port
                    .channel_offset
                    .checked_add(channel)
                    .ok_or(PluginValidationError::InvalidNegotiatedPort(port.native_id))?;
                if !occupied.insert((port.direction, slot)) {
                    return Err(PluginValidationError::OverlappingAudioBuffers);
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParameterEvent {
    pub frame_offset: u32,
    pub key: PluginParameterKey,
    pub value: NormalizedValue,
}

/// Validate a bounded RT event batch before publishing it to the audio thread.
pub fn validate_parameter_events(
    events: &[ParameterEvent],
    frames: u32,
    known: &BTreeSet<PluginParameterKey>,
    maximum_events: usize,
) -> Result<(), PluginValidationError> {
    if frames == 0 {
        return Err(PluginValidationError::InvalidProcessingRange);
    }
    if events.len() > maximum_events {
        return Err(PluginValidationError::TooManyEvents {
            actual: events.len(),
            maximum: maximum_events,
        });
    }
    let mut previous_offset = 0;
    let mut seen_at_offset = BTreeSet::new();
    for (index, event) in events.iter().enumerate() {
        event.value.validate()?;
        if event.frame_offset >= frames {
            return Err(PluginValidationError::EventOutsideBlock {
                offset: event.frame_offset,
                frames,
            });
        }
        if index > 0 && event.frame_offset < previous_offset {
            return Err(PluginValidationError::UnsortedEvents);
        }
        if event.frame_offset != previous_offset {
            seen_at_offset.clear();
        }
        if !known.contains(&event.key) {
            return Err(PluginValidationError::UnknownParameter(event.key.clone()));
        }
        if !seen_at_offset.insert(event.key.clone()) {
            return Err(PluginValidationError::DuplicateParameterEvent {
                offset: event.frame_offset,
                key: event.key.clone(),
            });
        }
        previous_offset = event.frame_offset;
    }
    Ok(())
}

/// Scanner subprocess request. `candidate_path` is the sole filesystem grant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanRequest {
    pub protocol_version: u32,
    pub request_id: u64,
    pub candidate_path: PathBuf,
    pub timeout_millis: u64,
    pub maximum_descriptors: u32,
    pub maximum_parameters_per_plugin: u32,
}

impl ScanRequest {
    pub fn validate(&self) -> Result<(), PluginValidationError> {
        if self.protocol_version != WORKER_PROTOCOL_VERSION {
            return Err(PluginValidationError::ProtocolMismatch);
        }
        if !self.candidate_path.is_absolute() {
            return Err(PluginValidationError::NonAbsoluteArtifactPath);
        }
        if self.timeout_millis == 0
            || self.timeout_millis > 300_000
            || self.maximum_descriptors == 0
            || self.maximum_descriptors > 65_536
            || self.maximum_parameters_per_plugin == 0
            || self.maximum_parameters_per_plugin > 1_000_000
        {
            return Err(PluginValidationError::InvalidScannerLimit);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ScanResponse {
    Ready {
        request_id: u64,
        record: ScanRecord,
    },
    Failed {
        request_id: u64,
        failure: ScanFailure,
    },
}

/// Opaque launch capability minted by the supervisor after matching a scan
/// record to current artifact bytes. It is not a path and conveys no project
/// filesystem authority to a sandboxed runtime worker.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArtifactLease(pub u128);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InstanceToken(pub u128);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SharedMemoryRegion {
    pub token: u128,
    pub byte_len: u64,
    pub access: SharedMemoryAccess,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SharedMemoryAccess {
    HostWrites,
    WorkerWrites,
}

/// Control-plane messages only. PCM and events live in bounded shared-memory
/// slots; no unbounded bytes are copied through this channel during DSP.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkerRequest {
    Hello {
        protocol_version: u32,
    },
    Instantiate {
        request_id: u64,
        instance: InstanceToken,
        artifact: ArtifactLease,
        plugin: PluginKey,
        contract: ProcessingContract,
        state: Option<PluginStateBlob>,
    },
    BindSharedMemory {
        instance: InstanceToken,
        audio_inputs: SharedMemoryRegion,
        audio_outputs: SharedMemoryRegion,
        events_to_worker: SharedMemoryRegion,
        events_from_worker: SharedMemoryRegion,
    },
    Activate {
        instance: InstanceToken,
    },
    Process {
        instance: InstanceToken,
        sequence: u64,
        frames: u32,
        input_event_count: u32,
    },
    SaveState {
        request_id: u64,
        instance: InstanceToken,
        maximum_bytes: u64,
    },
    Deactivate {
        instance: InstanceToken,
    },
    Destroy {
        instance: InstanceToken,
    },
    Shutdown,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WorkerResponse {
    Hello {
        protocol_version: u32,
        runtime_name: String,
        runtime_version: String,
    },
    Instantiated {
        request_id: u64,
        instance: InstanceToken,
        latency_frames: u32,
        tail: TailReport,
    },
    Processed {
        instance: InstanceToken,
        sequence: u64,
        output_event_count: u32,
    },
    StateSaved {
        request_id: u64,
        state: PluginStateBlob,
    },
    LatencyChanged {
        instance: InstanceToken,
        frames: u32,
    },
    TailChanged {
        instance: InstanceToken,
        tail: TailReport,
    },
    Failed {
        request_id: Option<u64>,
        instance: Option<InstanceToken>,
        recoverable: bool,
        detail: String,
    },
}

/// Validate control messages before they cross the process boundary.
pub fn validate_worker_request(request: &WorkerRequest) -> Result<(), PluginValidationError> {
    match request {
        WorkerRequest::Hello { protocol_version } => {
            if *protocol_version != WORKER_PROTOCOL_VERSION {
                return Err(PluginValidationError::ProtocolMismatch);
            }
        }
        WorkerRequest::Instantiate {
            plugin,
            contract,
            state,
            ..
        } => {
            plugin.validate()?;
            contract.validate()?;
            if let Some(state) = state {
                state.validate(256 * 1024 * 1024)?;
                if &state.plugin != plugin {
                    return Err(PluginValidationError::StatePluginMismatch);
                }
            }
        }
        WorkerRequest::BindSharedMemory {
            audio_inputs,
            audio_outputs,
            events_to_worker,
            events_from_worker,
            ..
        } => {
            let regions = [
                audio_inputs,
                audio_outputs,
                events_to_worker,
                events_from_worker,
            ];
            let tokens = regions
                .iter()
                .map(|region| region.token)
                .collect::<BTreeSet<_>>();
            if regions.iter().any(|region| region.byte_len == 0)
                || audio_inputs.byte_len > 1024 * 1024 * 1024
                || audio_outputs.byte_len > 1024 * 1024 * 1024
                || events_to_worker.byte_len > 64 * 1024 * 1024
                || events_from_worker.byte_len > 64 * 1024 * 1024
                || audio_inputs.access != SharedMemoryAccess::HostWrites
                || events_to_worker.access != SharedMemoryAccess::HostWrites
                || audio_outputs.access != SharedMemoryAccess::WorkerWrites
                || events_from_worker.access != SharedMemoryAccess::WorkerWrites
                || tokens.len() != regions.len()
            {
                return Err(PluginValidationError::InvalidSharedMemory);
            }
        }
        WorkerRequest::Process {
            frames,
            input_event_count,
            ..
        } if *frames == 0 || *input_event_count > 1_000_000 => {
            return Err(PluginValidationError::InvalidProcessMessage);
        }
        WorkerRequest::SaveState { maximum_bytes, .. }
            if *maximum_bytes == 0 || *maximum_bytes > 256 * 1024 * 1024 =>
        {
            return Err(PluginValidationError::InvalidStateLimit);
        }
        _ => {}
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClapCandidateKind {
    Bundle,
    Library,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClapCandidate {
    pub path: PathBuf,
    pub kind: ClapCandidateKind,
}

/// Dependency-free CLAP *artifact discovery*.
///
/// This only finds `.clap` files/bundles and never loads them. Extracting CLAP
/// descriptors necessarily executes the entry/factory ABI and therefore
/// belongs in the scanner subprocess. Directory symlinks are not followed.
pub fn discover_clap_candidates(
    roots: impl IntoIterator<Item = PathBuf>,
    maximum_depth: usize,
) -> Result<Vec<ClapCandidate>, std::io::Error> {
    let mut found = BTreeMap::<PathBuf, ClapCandidateKind>::new();
    for root in roots {
        discover_clap_at(&root, 0, maximum_depth, &mut found)?;
    }
    Ok(found
        .into_iter()
        .map(|(path, kind)| ClapCandidate { path, kind })
        .collect())
}

fn discover_clap_at(
    path: &Path,
    depth: usize,
    maximum_depth: usize,
    found: &mut BTreeMap<PathBuf, ClapCandidateKind>,
) -> Result<(), std::io::Error> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    let has_clap_extension = path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("clap"));
    if has_clap_extension {
        let kind = if metadata.is_dir() {
            ClapCandidateKind::Bundle
        } else {
            ClapCandidateKind::Library
        };
        found.insert(path.to_path_buf(), kind);
        return Ok(());
    }
    if !metadata.is_dir() || depth >= maximum_depth {
        return Ok(());
    }
    let mut children = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        discover_clap_at(&child.path(), depth + 1, maximum_depth, found)?;
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub enum PluginValidationError {
    EmptyText(&'static str),
    TextTooLong {
        field: &'static str,
        maximum: usize,
    },
    InvalidDigest,
    ZeroArtifactDigest,
    ZeroBackendDigest,
    ZeroScannerDigest,
    ZeroStateDigest,
    EmptyArtifact,
    MissingArchitecture,
    InvalidBackendCapabilities,
    DuplicateBackend(String),
    NonAbsoluteArtifactPath,
    UnsupportedScanSchema(u32),
    NoPluginDescriptors,
    DuplicatePlugin(PluginKey),
    DuplicateAudioPort {
        direction: PortDirection,
        native_id: u32,
    },
    DuplicateNotePort {
        direction: PortDirection,
        native_id: u32,
    },
    InvalidChannelLayout(u32),
    InvalidNoteDialects(u32),
    DuplicateParameter(PluginParameterKey),
    InvalidParameterRange(PluginParameterKey),
    InvalidParameterMapping(PluginParameterKey),
    InvalidNormalizedValue,
    StateTooLarge {
        actual: usize,
        maximum: usize,
    },
    StatePluginMismatch,
    InvalidQuarantineThreshold,
    InvalidProcessingRange,
    InvalidNegotiatedPort(u32),
    OverlappingAudioBuffers,
    TooManyEvents {
        actual: usize,
        maximum: usize,
    },
    EventOutsideBlock {
        offset: u32,
        frames: u32,
    },
    UnsortedEvents,
    UnknownParameter(PluginParameterKey),
    DuplicateParameterEvent {
        offset: u32,
        key: PluginParameterKey,
    },
    ProtocolMismatch,
    InvalidScannerLimit,
    InvalidSharedMemory,
    InvalidProcessMessage,
    InvalidStateLimit,
}

impl fmt::Display for PluginValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid plugin contract: {self:?}")
    }
}

impl std::error::Error for PluginValidationError {}

fn validate_text(
    field: &'static str,
    value: &str,
    minimum: usize,
    maximum: usize,
) -> Result<(), PluginValidationError> {
    let length = value.chars().count();
    if length < minimum || value.chars().any(char::is_control) {
        Err(PluginValidationError::EmptyText(field))
    } else if length > maximum {
        Err(PluginValidationError::TextTooLong { field, maximum })
    } else {
        Ok(())
    }
}

fn validate_optional_text(
    field: &'static str,
    value: Option<&str>,
    maximum: usize,
) -> Result<(), PluginValidationError> {
    if let Some(value) = value {
        validate_text(field, value, 1, maximum)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn digest(byte: u8) -> Digest32 {
        Digest32([byte; 32])
    }

    fn scanner() -> ScannerProvenance {
        ScannerProvenance {
            scanner_name: "audec-plugin-scan".into(),
            scanner_version: "1".into(),
            scanner_build: digest(9),
            host_os: "macos".into(),
            host_architecture: CpuArchitecture::Aarch64,
            hash_algorithm: "sha256".into(),
        }
    }

    fn fingerprint(byte: u8) -> ArtifactFingerprint {
        ArtifactFingerprint {
            content: digest(byte),
            byte_len: 4096,
            architectures: BTreeSet::from([CpuArchitecture::Aarch64]),
        }
    }

    fn key(id: &str) -> PluginKey {
        PluginKey {
            format: PluginFormat::Clap,
            identifier: id.into(),
        }
    }

    fn capabilities() -> ExecutionCapabilities {
        ExecutionCapabilities {
            realtime_safe_claimed: true,
            hard_realtime_required: false,
            offline_processing: true,
            editor: false,
            state: true,
            latency_reporting: true,
            tail_reporting: true,
            determinism: DeterminismClass::Unknown,
        }
    }

    fn backend(id: &str, isolation: IsolationMode) -> PluginBackendDescriptor {
        PluginBackendDescriptor {
            stable_id: id.into(),
            name: id.into(),
            version: "1".into(),
            build: digest(8),
            formats: BTreeSet::from([PluginFormat::Clap]),
            architectures: BTreeSet::from([CpuArchitecture::Aarch64]),
            isolation_modes: BTreeSet::from([isolation]),
            realtime: true,
            offline: true,
            editor_bridge: false,
        }
    }

    fn plugin(id: &str, role: PluginRole) -> PluginMetadata {
        PluginMetadata {
            key: key(id),
            name: id.into(),
            vendor: Some("audec tests".into()),
            version: Some("1.0".into()),
            description: None,
            homepage: None,
            features: BTreeSet::new(),
            roles: BTreeSet::from([role]),
            audio_ports: vec![AudioPortDescriptor {
                native_id: 0,
                name: "main".into(),
                direction: PortDirection::Input,
                role: AudioPortRole::Main,
                layouts: vec![ChannelLayout::Stereo],
                required: true,
            }],
            note_ports: Vec::new(),
            parameters: vec![PluginParameterDescriptor {
                key: PluginParameterKey::Clap(7),
                name: "Cutoff".into(),
                module: None,
                unit: Some("Hz".into()),
                plain_min: 20.0,
                plain_max: 20_000.0,
                plain_default: 1_000.0,
                mapping: ParameterMapping::Logarithmic,
                automatable: true,
                modulatable: true,
                read_only: false,
                hidden: false,
            }],
            capabilities: capabilities(),
        }
    }

    fn record(path: PathBuf, byte: u8) -> ScanRecord {
        ScanRecord {
            schema_version: SCAN_SCHEMA_VERSION,
            canonical_path: path,
            artifact: fingerprint(byte),
            scanner: scanner(),
            plugins: vec![
                plugin("z.plugin", PluginRole::AudioEffect),
                plugin("a.plugin", PluginRole::Instrument),
            ],
        }
    }

    #[test]
    fn digest_hex_round_trip_is_canonical() {
        let value = digest(0xab);
        assert_eq!(Digest32::from_hex(&value.to_hex()).unwrap(), value);
        assert_eq!(
            Digest32::from_hex(&value.to_hex().to_uppercase()).unwrap(),
            value
        );
        assert!(Digest32::from_hex("beef").is_err());
    }

    #[test]
    fn backend_selection_is_policy_filtered_and_stably_ordered() {
        let mut registry = PluginBackendRegistry::default();
        registry
            .insert(backend("z-worker", IsolationMode::DedicatedWorker))
            .unwrap();
        registry
            .insert(backend("a-worker", IsolationMode::DedicatedWorker))
            .unwrap();
        registry
            .insert(backend("trusted", IsolationMode::TrustedInProcess))
            .unwrap();
        let compatible = registry.compatible(
            &PluginFormat::Clap,
            CpuArchitecture::Aarch64,
            IsolationMode::DedicatedWorker,
            false,
        );
        assert_eq!(
            compatible
                .iter()
                .map(|backend| backend.stable_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a-worker", "z-worker"]
        );
        assert!(matches!(
            registry.insert(backend("a-worker", IsolationMode::DedicatedWorker)),
            Err(PluginValidationError::DuplicateBackend(_))
        ));
    }

    #[test]
    fn metadata_rejects_duplicate_native_parameter_keys() {
        let mut value = plugin("duplicate", PluginRole::AudioEffect);
        value.parameters.push(value.parameters[0].clone());
        assert!(matches!(
            value.validate(),
            Err(PluginValidationError::DuplicateParameter(
                PluginParameterKey::Clap(7)
            ))
        ));
    }

    #[test]
    fn missing_effect_bypasses_but_missing_instrument_is_silent() {
        assert_eq!(
            plugin("effect", PluginRole::AudioEffect).missing_behavior(),
            MissingPluginBehavior::BypassAudio
        );
        assert_eq!(
            plugin("instrument", PluginRole::Instrument).missing_behavior(),
            MissingPluginBehavior::Silence
        );
    }

    #[test]
    fn successful_indexing_canonicalizes_descriptor_order() {
        let path = PathBuf::from("/Library/Audio/Plug-Ins/CLAP/test.clap");
        let mut index = PluginIndex::default();
        index.apply_success(record(path.clone(), 1)).unwrap();
        let ScanCacheEntry::Ready(value) = &index.entries()[&path] else {
            panic!("expected ready entry")
        };
        assert_eq!(value.plugins[0].key.identifier, "a.plugin");
        assert_eq!(index.descriptor(&key("z.plugin")).unwrap().name, "z.plugin");
        assert!(!index.needs_scan(&path, &fingerprint(1)));
        assert!(index.needs_scan(&path, &fingerprint(2)));
    }

    #[test]
    fn repeated_identical_failures_quarantine_but_new_bytes_reset_counter() {
        let path = PathBuf::from("/tmp/hostile.clap");
        let failure = ScanFailure {
            kind: ScanFailureKind::Crashed,
            detail: "signal 11".into(),
            scanner: scanner(),
        };
        let mut index = PluginIndex::default();
        index
            .apply_failure(path.clone(), fingerprint(1), failure.clone(), 2)
            .unwrap();
        index
            .apply_failure(path.clone(), fingerprint(1), failure.clone(), 2)
            .unwrap();
        assert!(matches!(
            index.entries()[&path],
            ScanCacheEntry::Quarantined {
                consecutive_failures: 2,
                ..
            }
        ));
        assert!(!index.needs_scan(&path, &fingerprint(1)));
        assert!(index.needs_scan(&path, &fingerprint(2)));
        index
            .apply_failure(path.clone(), fingerprint(2), failure, 2)
            .unwrap();
        assert!(matches!(
            index.entries()[&path],
            ScanCacheEntry::Failed {
                consecutive_failures: 1,
                ..
            }
        ));
    }

    #[test]
    fn state_blob_is_preserved_and_bounded_without_executing_plugin() {
        let state = PluginStateBlob {
            plugin: key("missing"),
            state_format_version: 1,
            bytes: vec![1, 2, 3],
            digest: digest(3),
        };
        let instance = PluginInstanceState {
            plugin: key("missing"),
            scanned_artifact: Some(digest(1)),
            state: Some(state.clone()),
            parameter_values: BTreeMap::from([(
                PluginParameterKey::Clap(4),
                NormalizedValue::new(0.25).unwrap(),
            )]),
            unavailable: Some(PluginUnavailableReason::MissingArtifact),
            missing_behavior: MissingPluginBehavior::BypassAudio,
        };
        instance.validate(3).unwrap();
        assert_eq!(instance.state.unwrap().bytes, vec![1, 2, 3]);
        assert!(matches!(
            state.validate(2),
            Err(PluginValidationError::StateTooLarge { .. })
        ));
    }

    #[test]
    fn processing_contract_rejects_overlapping_channel_slots() {
        let contract = ProcessingContract {
            sample_rate: 48_000,
            minimum_frames: 1,
            maximum_frames: 1024,
            audio_ports: vec![
                NegotiatedAudioPort {
                    native_id: 1,
                    direction: PortDirection::Input,
                    layout: ChannelLayout::Stereo,
                    channel_offset: 0,
                },
                NegotiatedAudioPort {
                    native_id: 2,
                    direction: PortDirection::Input,
                    layout: ChannelLayout::Mono,
                    channel_offset: 1,
                },
            ],
            note_inputs: BTreeMap::new(),
            note_outputs: BTreeMap::new(),
            initial_latency_frames: 0,
            initial_tail: TailReport::None,
            offline: false,
        };
        assert_eq!(
            contract.validate(),
            Err(PluginValidationError::OverlappingAudioBuffers)
        );
    }

    #[test]
    fn parameter_events_are_bounded_known_sorted_and_unique_per_frame() {
        let known = BTreeSet::from([PluginParameterKey::Clap(1)]);
        let event = |offset| ParameterEvent {
            frame_offset: offset,
            key: PluginParameterKey::Clap(1),
            value: NormalizedValue::new(0.5).unwrap(),
        };
        assert!(validate_parameter_events(&[event(0), event(31)], 32, &known, 8).is_ok());
        assert!(matches!(
            validate_parameter_events(&[event(2), event(1)], 32, &known, 8),
            Err(PluginValidationError::UnsortedEvents)
        ));
        assert!(matches!(
            validate_parameter_events(&[event(2), event(2)], 32, &known, 8),
            Err(PluginValidationError::DuplicateParameterEvent { .. })
        ));
        assert!(matches!(
            validate_parameter_events(&[event(32)], 32, &known, 8),
            Err(PluginValidationError::EventOutsideBlock { .. })
        ));
    }

    #[test]
    fn worker_messages_reject_bad_protocol_state_and_shared_memory() {
        assert_eq!(
            validate_worker_request(&WorkerRequest::Hello {
                protocol_version: 99
            }),
            Err(PluginValidationError::ProtocolMismatch)
        );
        let host = SharedMemoryRegion {
            token: 1,
            byte_len: 4096,
            access: SharedMemoryAccess::HostWrites,
        };
        let worker = SharedMemoryRegion {
            token: 1,
            byte_len: 4096,
            access: SharedMemoryAccess::WorkerWrites,
        };
        assert_eq!(
            validate_worker_request(&WorkerRequest::BindSharedMemory {
                instance: InstanceToken(1),
                audio_inputs: host.clone(),
                audio_outputs: worker.clone(),
                events_to_worker: host,
                events_from_worker: worker,
            }),
            Err(PluginValidationError::InvalidSharedMemory)
        );
    }

    #[test]
    fn dependency_free_discovery_is_sorted_bounded_and_does_not_enter_bundles() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("audec-plugin-test-{unique}"));
        fs::create_dir_all(root.join("b.clap/Contents/MacOS")).unwrap();
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("a.clap"), b"not actually executable").unwrap();
        fs::write(root.join("b.clap/Contents/MacOS/hidden.clap"), b"ignored").unwrap();
        fs::write(root.join("nested/c.clap"), b"candidate").unwrap();

        let found = discover_clap_candidates([root.clone()], 3).unwrap();
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].path, root.join("a.clap"));
        assert_eq!(found[0].kind, ClapCandidateKind::Library);
        assert_eq!(found[1].path, root.join("b.clap"));
        assert_eq!(found[1].kind, ClapCandidateKind::Bundle);
        assert_eq!(found[2].path, root.join("nested/c.clap"));
        fs::remove_dir_all(root).unwrap();
    }
}
