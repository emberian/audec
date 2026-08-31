//! Explicit, local-only registry for optional machine-learning artifacts.
//!
//! A registry entry is a *lock file in memory*, not an installer.  It names
//! one exact [`ModelManifest`], the relative files that make up that model,
//! and the runtimes that may ask a worker to load it.  This keeps three
//! important boundaries visible:
//!
//! * this module never makes a network request or chooses a `latest` model;
//! * a file is not considered installed until its bytes match its lock hash;
//! * verifying an artifact never executes it.  Worker launch remains a
//!   separate, opt-in controller responsibility.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use crate::model_worker::{
    Backend, ContentHash, LicenseProvenance, ModelManifest, WorkerCapabilities, PROTOCOL_VERSION,
};

/// Deliberately fixed state for every catalogued external model.
///
/// The application may offer a link or instructions elsewhere, but registry
/// code does not fetch, unpack, convert, or run anything in this state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DownloadState {
    #[default]
    UserDownloadRequired,
    /// Reserved for an explicit future installer with a separately reviewed
    /// trust policy.  It is never produced by this module today.
    DownloadNotImplemented,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArtifactRole {
    Weights,
    Configuration,
    Adapter,
    ConversionRecipe,
    NumericalValidation,
    Runtime,
    Auxiliary,
}

impl ArtifactRole {
    pub const fn is_manifest_bound(self) -> bool {
        !matches!(self, Self::Runtime | Self::Auxiliary)
    }
}

/// One regular file, relative to a model directory below the registry root.
/// No absolute path, parent traversal, or symlink escape is accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactLock {
    pub role: ArtifactRole,
    pub relative_path: PathBuf,
    pub sha256: ContentHash,
    /// `None` allows an exact digest with an unknown published byte count.
    pub byte_len: Option<u64>,
    pub required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactManifest {
    /// Stable local directory name, deliberately separate from `model_id`.
    pub install_directory: String,
    pub artifacts: Vec<ArtifactLock>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModelCapability {
    BeatAndDownbeat,
    PolyphonicPitch,
    JointStemSeparation,
    VocalSeparation,
    DrumDecomposition,
    SampleRetrieval,
    SectionAnalysis,
    SynthPatchProposal,
    OpenVocabularySeparation,
    Custom(String),
}

impl ModelCapability {
    fn stable_name(&self) -> &str {
        match self {
            Self::BeatAndDownbeat => "beat-and-downbeat",
            Self::PolyphonicPitch => "polyphonic-pitch",
            Self::JointStemSeparation => "joint-stem-separation",
            Self::VocalSeparation => "vocal-separation",
            Self::DrumDecomposition => "drum-decomposition",
            Self::SampleRetrieval => "sample-retrieval",
            Self::SectionAnalysis => "section-analysis",
            Self::SynthPatchProposal => "synth-patch-proposal",
            Self::OpenVocabularySeparation => "open-vocabulary-separation",
            Self::Custom(name) => name,
        }
    }
}

/// A runtime contract that can be compared with a worker's advertised
/// capabilities before a model-load request is sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeDescriptor {
    pub runtime_id: String,
    pub protocol_version: u32,
    /// Names must use the same stable strings a worker exposes in
    /// `WorkerCapabilities.backends` (for example `coreml`, `mlx`, `cpu`).
    pub supported_backends: BTreeSet<String>,
    pub required_accelerators: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerDescriptor {
    pub runtime: RuntimeDescriptor,
    /// This is descriptive only.  The controller must still authenticate its
    /// chosen executable and negotiate `Hello` before calling `LoadModel`.
    pub worker_name: String,
}

impl WorkerDescriptor {
    pub fn is_compatible_with(&self, worker: &WorkerCapabilities) -> bool {
        if self.runtime.protocol_version != PROTOCOL_VERSION
            || self.worker_name != worker.worker_name
            || worker.maximum_parallel_jobs == 0
        {
            return false;
        }
        self.runtime
            .supported_backends
            .iter()
            .all(|required| worker.backends.iter().any(|actual| actual == required))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRegistration {
    pub manifest: ModelManifest,
    pub artifacts: ArtifactManifest,
    pub license: LicenseProvenance,
    pub capabilities: BTreeSet<ModelCapability>,
    /// Lower values are preferred only within the same capability/runtime.
    pub selection_priority: u16,
    pub workers: Vec<WorkerDescriptor>,
    pub download_state: DownloadState,
}

impl ModelRegistration {
    pub fn model_id(&self) -> &str {
        &self.manifest.model_id
    }

    pub fn version(&self) -> &str {
        &self.manifest.architecture.version
    }

    pub fn supports(&self, capability: &ModelCapability) -> bool {
        self.capabilities.contains(capability)
    }

    pub fn compatible_workers(&self, capabilities: &WorkerCapabilities) -> Vec<&WorkerDescriptor> {
        self.workers
            .iter()
            .filter(|descriptor| descriptor.is_compatible_with(capabilities))
            .collect()
    }

    pub fn validate(&self) -> Result<(), RegistryError> {
        self.manifest
            .validate()
            .map_err(|error| RegistryError::InvalidRegistration {
                model_id: self.model_id().to_owned(),
                detail: error.to_string(),
            })?;
        if self.license != self.manifest.license {
            return Err(RegistryError::InvalidRegistration {
                model_id: self.model_id().to_owned(),
                detail: "registry license metadata must exactly match worker manifest".into(),
            });
        }
        validate_label("install_directory", &self.artifacts.install_directory)?;
        if self.artifacts.artifacts.is_empty() {
            return self.invalid("at least one artifact lock is required");
        }
        if self.capabilities.is_empty() {
            return self.invalid("at least one capability is required");
        }
        if self.workers.is_empty() {
            return self.invalid("at least one compatible-worker descriptor is required");
        }

        // A checkpoint may intentionally carry its configuration internally.
        // In that case two manifest roles name the same immutable file (Beat
        // This small0 is the first example).  Keep that relationship explicit
        // instead of inventing a second copied configuration artifact: the
        // path may repeat only when its digest agrees and its roles differ.
        let mut paths = BTreeMap::new();
        let mut roles = BTreeSet::new();
        for lock in &self.artifacts.artifacts {
            validate_safe_relative_path(&lock.relative_path).map_err(|detail| {
                RegistryError::InvalidRegistration {
                    model_id: self.model_id().to_owned(),
                    detail,
                }
            })?;
            if let Some(previous_hash) = paths.insert(lock.relative_path.clone(), lock.sha256) {
                if previous_hash != lock.sha256 {
                    return self
                        .invalid("repeated artifact paths must name identical immutable bytes");
                }
            }
            if lock.role.is_manifest_bound() && !roles.insert(lock.role) {
                return self.invalid("a manifest-bound artifact role may appear only once");
            }
            if lock.role.is_manifest_bound() && !lock.required {
                return self.invalid("a manifest-bound artifact may not be optional");
            }
        }
        self.validate_bound_hashes()?;

        let mut runtime_ids = BTreeSet::new();
        let manifest_backend = backend_name(&self.manifest.execution.backend);
        for worker in &self.workers {
            validate_label("worker_name", &worker.worker_name)?;
            validate_label("runtime_id", &worker.runtime.runtime_id)?;
            if worker.runtime.protocol_version != PROTOCOL_VERSION {
                return self.invalid("worker descriptor uses an unsupported protocol version");
            }
            if worker.runtime.supported_backends.is_empty() {
                return self.invalid("worker descriptor needs at least one backend");
            }
            if !worker.runtime.supported_backends.contains(manifest_backend) {
                return self.invalid("worker descriptor does not support the manifest backend");
            }
            if !self
                .manifest
                .execution
                .required_accelerators
                .iter()
                .all(|accelerator| worker.runtime.required_accelerators.contains(accelerator))
            {
                return self.invalid("worker descriptor omits a required accelerator");
            }
            if !runtime_ids.insert(worker.runtime.runtime_id.clone()) {
                return self.invalid("runtime IDs must be unique per model registration");
            }
        }
        Ok(())
    }

    fn validate_bound_hashes(&self) -> Result<(), RegistryError> {
        let expected = [
            (
                ArtifactRole::Weights,
                Some(self.manifest.artifacts.weights_sha256),
            ),
            (
                ArtifactRole::Configuration,
                Some(self.manifest.artifacts.config_sha256),
            ),
            (
                ArtifactRole::Adapter,
                self.manifest.artifacts.adapter_sha256,
            ),
            (
                ArtifactRole::ConversionRecipe,
                self.manifest.artifacts.conversion_recipe_sha256,
            ),
            (
                ArtifactRole::NumericalValidation,
                self.manifest.artifacts.numerical_validation_sha256,
            ),
        ];
        for (role, expected_hash) in expected {
            let lock = self
                .artifacts
                .artifacts
                .iter()
                .find(|lock| lock.role == role);
            match (expected_hash, lock) {
                (Some(expected_hash), Some(lock)) if lock.sha256 == expected_hash => {}
                (Some(_), Some(_)) => {
                    return self.invalid("artifact hash disagrees with worker manifest")
                }
                (Some(_), None) => return self.invalid("manifest-bound artifact lock is missing"),
                (None, None) => {}
                (None, Some(_)) => {
                    return self.invalid("artifact exists but worker manifest has no matching hash")
                }
            }
        }
        Ok(())
    }

    fn invalid<T>(&self, detail: impl Into<String>) -> Result<T, RegistryError> {
        Err(RegistryError::InvalidRegistration {
            model_id: self.model_id().to_owned(),
            detail: detail.into(),
        })
    }
}

fn backend_name(backend: &Backend) -> &'static str {
    match backend {
        Backend::Cpu { .. } => "cpu",
        Backend::Cuda { .. } => "cuda",
        Backend::CoreMl { .. } => "coreml",
        Backend::Mps { .. } => "mps",
        Backend::Mlx { .. } => "mlx",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistrySelection<'a> {
    pub registration: &'a ModelRegistration,
    pub worker: &'a WorkerDescriptor,
}

#[derive(Clone, Debug)]
pub struct ModelRegistry {
    root: PathBuf,
    registrations: BTreeMap<String, ModelRegistration>,
}

impl ModelRegistry {
    /// `root` is a local folder owned by the caller, not a URL or a cache key.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            registrations: BTreeMap::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn register(&mut self, registration: ModelRegistration) -> Result<(), RegistryError> {
        registration.validate()?;
        let model_id = registration.model_id().to_owned();
        if self.registrations.contains_key(&model_id) {
            return Err(RegistryError::DuplicateModelId(model_id));
        }
        self.registrations.insert(model_id, registration);
        Ok(())
    }

    pub fn get(&self, model_id: &str) -> Option<&ModelRegistration> {
        self.registrations.get(model_id)
    }

    pub fn registrations(&self) -> impl Iterator<Item = &ModelRegistration> {
        self.registrations.values()
    }

    /// Stable selection: priority, then model id, then runtime id.  It never
    /// selects a missing/tampered model; the caller may display its status and
    /// let the person install it deliberately.
    pub fn select_installed<'a>(
        &'a self,
        capability: &ModelCapability,
        worker: &WorkerCapabilities,
    ) -> Result<Option<RegistrySelection<'a>>, RegistryError> {
        let mut choices = Vec::new();
        for registration in self.registrations.values() {
            if !registration.supports(capability) {
                continue;
            }
            if !matches!(self.verify(registration)?, InstallStatus::Installed { .. }) {
                continue;
            }
            for descriptor in registration.compatible_workers(worker) {
                choices.push((
                    registration.selection_priority,
                    registration.model_id(),
                    descriptor.runtime.runtime_id.as_str(),
                    registration,
                    descriptor,
                ));
            }
        }
        choices.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then(left.1.cmp(right.1))
                .then(left.2.cmp(right.2))
        });
        Ok(choices.first().map(|choice| RegistrySelection {
            registration: choice.3,
            worker: choice.4,
        }))
    }

    pub fn verify(&self, registration: &ModelRegistration) -> Result<InstallStatus, RegistryError> {
        registration.validate()?;
        let root = match fs::canonicalize(&self.root) {
            Ok(root) if root.is_dir() => root,
            Ok(_) => return Ok(InstallStatus::RegistryRootUnavailable),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(InstallStatus::RegistryRootUnavailable)
            }
            Err(error) => {
                return Err(RegistryError::Io {
                    path: self.root.clone(),
                    error,
                })
            }
        };
        let model_directory = root.join(&registration.artifacts.install_directory);
        let model_directory = match fs::canonicalize(&model_directory) {
            Ok(path) if path.is_dir() && path.starts_with(&root) => path,
            Ok(_) => return Ok(InstallStatus::UnsafeInstallDirectory),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(InstallStatus::Missing {
                    paths: registration
                        .artifacts
                        .artifacts
                        .iter()
                        .filter(|lock| lock.required)
                        .map(|lock| lock.relative_path.clone())
                        .collect(),
                })
            }
            Err(error) => {
                return Err(RegistryError::Io {
                    path: model_directory,
                    error,
                })
            }
        };

        let mut missing = Vec::new();
        let mut invalid = Vec::new();
        for lock in &registration.artifacts.artifacts {
            match verify_one(&model_directory, &root, lock)? {
                ArtifactStatus::Valid => {}
                ArtifactStatus::Missing if lock.required => {
                    missing.push(lock.relative_path.clone())
                }
                ArtifactStatus::Missing => {}
                status => invalid.push((lock.relative_path.clone(), status)),
            }
        }
        if !missing.is_empty() {
            return Ok(InstallStatus::Missing { paths: missing });
        }
        if !invalid.is_empty() {
            return Ok(InstallStatus::Invalid { artifacts: invalid });
        }
        Ok(InstallStatus::Installed {
            manifest_sha256: registration.manifest.canonical_hash().map_err(|error| {
                RegistryError::InvalidRegistration {
                    model_id: registration.model_id().to_owned(),
                    detail: error.to_string(),
                }
            })?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallStatus {
    Installed {
        manifest_sha256: ContentHash,
    },
    RegistryRootUnavailable,
    UnsafeInstallDirectory,
    Missing {
        paths: Vec<PathBuf>,
    },
    Invalid {
        artifacts: Vec<(PathBuf, ArtifactStatus)>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtifactStatus {
    Valid,
    Missing,
    NotARegularFile,
    UnsafePath,
    SizeMismatch {
        expected: u64,
        actual: u64,
    },
    HashMismatch {
        expected: ContentHash,
        actual: ContentHash,
    },
}

#[derive(Debug)]
pub enum RegistryError {
    DuplicateModelId(String),
    InvalidRegistration { model_id: String, detail: String },
    Io { path: PathBuf, error: io::Error },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateModelId(id) => write!(formatter, "duplicate model registration: {id}"),
            Self::InvalidRegistration { model_id, detail } => {
                write!(formatter, "invalid model registration {model_id}: {detail}")
            }
            Self::Io { path, error } => write!(formatter, "{}: {error}", path.display()),
        }
    }
}

impl std::error::Error for RegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { error, .. } => Some(error),
            _ => None,
        }
    }
}

fn verify_one(
    root: &Path,
    registry_root: &Path,
    lock: &ArtifactLock,
) -> Result<ArtifactStatus, RegistryError> {
    let candidate = root.join(&lock.relative_path);
    let canonical = match fs::canonicalize(&candidate) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ArtifactStatus::Missing)
        }
        Err(error) => {
            return Err(RegistryError::Io {
                path: candidate,
                error,
            })
        }
    };
    // `canonicalize` resolves symlinks before the containment check, preventing
    // a lock such as `weights.bin` from escaping through a malicious symlink.
    if !canonical.starts_with(root) || !canonical.starts_with(registry_root) {
        return Ok(ArtifactStatus::UnsafePath);
    }
    let metadata = fs::metadata(&canonical).map_err(|error| RegistryError::Io {
        path: canonical.clone(),
        error,
    })?;
    if !metadata.is_file() {
        return Ok(ArtifactStatus::NotARegularFile);
    }
    if let Some(expected) = lock.byte_len {
        if metadata.len() != expected {
            return Ok(ArtifactStatus::SizeMismatch {
                expected,
                actual: metadata.len(),
            });
        }
    }
    let actual = sha256_file(&canonical).map_err(|error| RegistryError::Io {
        path: canonical,
        error,
    })?;
    if actual != lock.sha256 {
        return Ok(ArtifactStatus::HashMismatch {
            expected: lock.sha256,
            actual,
        });
    }
    Ok(ArtifactStatus::Valid)
}

fn validate_label(field: &'static str, value: &str) -> Result<(), RegistryError> {
    if value.is_empty()
        || value.len() > 160
        || value.contains(char::is_whitespace)
        || value.contains(['/', '\\', '\0'])
    {
        return Err(RegistryError::InvalidRegistration {
            model_id: "<unknown>".into(),
            detail: format!("{field} must be a compact non-path label"),
        });
    }
    Ok(())
}

fn validate_safe_relative_path(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err("artifact path must be non-empty and relative".into());
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(
                "artifact path may not contain root, prefix, '.', or '..' components".into(),
            );
        }
    }
    Ok(())
}

fn sha256_file(path: &Path) -> io::Result<ContentHash> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(ContentHash::from_bytes(hash.finish()));
        }
        hash.update(&buffer[..read]);
    }
}

// A streaming, dependency-free SHA-256 implementation. Unlike the
// cache-only helper in model_worker, this avoids loading multi-GB checkpoints
// into memory during local verification.
struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffer_len: usize,
    total_len: u64,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: [0; 64],
            buffer_len: 0,
            total_len: 0,
        }
    }

    fn update(&mut self, mut input: &[u8]) {
        self.total_len = self.total_len.wrapping_add(input.len() as u64);
        if self.buffer_len != 0 {
            let copied = (64 - self.buffer_len).min(input.len());
            self.buffer[self.buffer_len..self.buffer_len + copied]
                .copy_from_slice(&input[..copied]);
            self.buffer_len += copied;
            input = &input[copied..];
            if self.buffer_len == 64 {
                compress(&mut self.state, &self.buffer);
                self.buffer_len = 0;
            }
        }
        while input.len() >= 64 {
            let (block, rest) = input.split_at(64);
            compress(&mut self.state, block);
            input = rest;
        }
        if !input.is_empty() {
            self.buffer[..input.len()].copy_from_slice(input);
            self.buffer_len = input.len();
        }
    }

    fn finish(mut self) -> [u8; 32] {
        let bit_len = self.total_len.wrapping_mul(8);
        self.buffer[self.buffer_len] = 0x80;
        self.buffer_len += 1;
        if self.buffer_len > 56 {
            self.buffer[self.buffer_len..].fill(0);
            compress(&mut self.state, &self.buffer);
            self.buffer_len = 0;
        }
        self.buffer[self.buffer_len..56].fill(0);
        self.buffer[56..].copy_from_slice(&bit_len.to_be_bytes());
        compress(&mut self.state, &self.buffer);
        let mut output = [0; 32];
        for (index, word) in self.state.into_iter().enumerate() {
            output[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        output
    }
}

fn compress(state: &mut [u32; 8], block: &[u8]) {
    let k = sha256_constants();
    let mut w = [0_u32; 64];
    for (index, word) in block.chunks_exact(4).enumerate() {
        w[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
    }
    for index in 16..64 {
        let s0 =
            w[index - 15].rotate_right(7) ^ w[index - 15].rotate_right(18) ^ (w[index - 15] >> 3);
        let s1 =
            w[index - 2].rotate_right(17) ^ w[index - 2].rotate_right(19) ^ (w[index - 2] >> 10);
        w[index] = w[index - 16]
            .wrapping_add(s0)
            .wrapping_add(w[index - 7])
            .wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for index in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choice = (e & f) ^ ((!e) & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(choice)
            .wrapping_add(k[index])
            .wrapping_add(w[index]);
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
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

fn sha256_constants() -> &'static [u32; 64] {
    &[
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
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_worker::{
        Architecture, AudioContract, ChannelContract, ExactRevision, ExecutionContract,
        LicenseReference, ModelArtifacts, Normalization, NumericPrecision, OutputAdditivity,
        OutputContract, Redistribution, SampleEncoding, TrainingProvenance,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "audec-model-registry-{}-{nonce}-{sequence}",
            std::process::id(),
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write(root: &Path, name: &str, contents: &[u8]) -> ContentHash {
        fs::write(root.join(name), contents).unwrap();
        sha256_file(&root.join(name)).unwrap()
    }

    fn registration(root: &Path, id: &str, priority: u16) -> ModelRegistration {
        let model = root.join(id);
        fs::create_dir_all(&model).unwrap();
        let weights = write(&model, "weights.bin", b"weights");
        let config = write(&model, "config.json", b"config");
        let recipe = write(&model, "conversion.txt", b"recipe");
        let validation = write(&model, "golden.json", b"validation");
        let license = LicenseProvenance {
            code: LicenseReference::Spdx("MIT".into()),
            checkpoint: LicenseReference::Spdx("MIT".into()),
            redistribution: Redistribution::RequiresReview,
            source_url: Some("https://example.invalid/pinned".into()),
            review_notes: "Explicit local artifact review required".into(),
        };
        ModelRegistration {
            manifest: ModelManifest {
                schema_version: 1,
                model_id: id.into(),
                architecture: Architecture {
                    family: "test-model".into(),
                    version: "v1".into(),
                },
                revision: ExactRevision::Commit(weights),
                artifacts: ModelArtifacts {
                    weights_sha256: weights,
                    config_sha256: config,
                    adapter_sha256: None,
                    conversion_recipe_sha256: Some(recipe),
                    numerical_validation_sha256: Some(validation),
                },
                license: license.clone(),
                training: TrainingProvenance {
                    summary: "locally generated test fixture".into(),
                    sources: Vec::new(),
                    documentation_sha256: validation,
                },
                input: AudioContract {
                    sample_rate_hz: 44_100,
                    channels: ChannelContract::Stereo,
                    encoding: SampleEncoding::Float32Le,
                },
                execution: ExecutionContract {
                    chunk_frames: 44_100,
                    overlap_frames: 0,
                    normalization: Normalization::None,
                    backend: Backend::Cpu {
                        runtime: "test-runtime".into(),
                        precision: NumericPrecision::Float32,
                    },
                    estimated_peak_memory_bytes: 1,
                    required_accelerators: Vec::new(),
                },
                output: OutputContract {
                    names: vec!["target".into()],
                    sample_rate_hz: 44_100,
                    channels: ChannelContract::Stereo,
                    additivity: OutputAdditivity::OverlappingEstimates {
                        explanation: "test".into(),
                    },
                },
                golden_validations: Vec::new(),
            },
            artifacts: ArtifactManifest {
                install_directory: id.into(),
                artifacts: vec![
                    ArtifactLock {
                        role: ArtifactRole::Weights,
                        relative_path: "weights.bin".into(),
                        sha256: weights,
                        byte_len: Some(7),
                        required: true,
                    },
                    ArtifactLock {
                        role: ArtifactRole::Configuration,
                        relative_path: "config.json".into(),
                        sha256: config,
                        byte_len: None,
                        required: true,
                    },
                    ArtifactLock {
                        role: ArtifactRole::ConversionRecipe,
                        relative_path: "conversion.txt".into(),
                        sha256: recipe,
                        byte_len: None,
                        required: true,
                    },
                    ArtifactLock {
                        role: ArtifactRole::NumericalValidation,
                        relative_path: "golden.json".into(),
                        sha256: validation,
                        byte_len: None,
                        required: true,
                    },
                ],
            },
            license,
            capabilities: BTreeSet::from([ModelCapability::VocalSeparation]),
            selection_priority: priority,
            workers: vec![WorkerDescriptor {
                worker_name: "test-worker".into(),
                runtime: RuntimeDescriptor {
                    runtime_id: "cpu-test".into(),
                    protocol_version: PROTOCOL_VERSION,
                    supported_backends: BTreeSet::from(["cpu".into()]),
                    required_accelerators: BTreeSet::new(),
                },
            }],
            download_state: DownloadState::UserDownloadRequired,
        }
    }

    fn worker() -> WorkerCapabilities {
        WorkerCapabilities {
            worker_name: "test-worker".into(),
            backends: vec!["cpu".into()],
            maximum_parallel_jobs: 1,
            shared_memory: false,
        }
    }

    #[test]
    fn streaming_hash_matches_sha256_test_vector() {
        let directory = temp_dir();
        let path = directory.join("abc");
        fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap().to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn streaming_hash_is_independent_of_chunk_boundaries() {
        let mut hash = Sha256::new();
        hash.update(b"a");
        hash.update(b"b");
        hash.update(b"c");
        assert_eq!(
            ContentHash::from_bytes(hash.finish()).to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verifies_installed_artifacts_without_executing_them() {
        let directory = temp_dir();
        let mut registry = ModelRegistry::new(&directory);
        registry
            .register(registration(&directory, "test-model", 10))
            .unwrap();
        assert!(matches!(
            registry
                .verify(registry.get("test-model").unwrap())
                .unwrap(),
            InstallStatus::Installed { .. }
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn tampering_and_missing_files_are_distinct() {
        let directory = temp_dir();
        let mut registry = ModelRegistry::new(&directory);
        registry
            .register(registration(&directory, "test-model", 10))
            .unwrap();
        fs::write(directory.join("test-model/weights.bin"), b"changed").unwrap();
        assert!(matches!(
            registry
                .verify(registry.get("test-model").unwrap())
                .unwrap(),
            InstallStatus::Invalid { .. }
        ));
        fs::remove_file(directory.join("test-model/config.json")).unwrap();
        assert!(matches!(
            registry
                .verify(registry.get("test-model").unwrap())
                .unwrap(),
            InstallStatus::Missing { .. }
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn selection_is_priority_then_model_id_and_requires_verified_files() {
        let directory = temp_dir();
        let mut registry = ModelRegistry::new(&directory);
        registry
            .register(registration(&directory, "zebra-model", 5))
            .unwrap();
        registry
            .register(registration(&directory, "alpha-model", 5))
            .unwrap();
        let selected = registry
            .select_installed(&ModelCapability::VocalSeparation, &worker())
            .unwrap()
            .unwrap();
        assert_eq!(selected.registration.model_id(), "alpha-model");
        fs::remove_file(directory.join("alpha-model/weights.bin")).unwrap();
        let selected = registry
            .select_installed(&ModelCapability::VocalSeparation, &worker())
            .unwrap()
            .unwrap();
        assert_eq!(selected.registration.model_id(), "zebra-model");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejects_parent_paths_and_manifest_hash_drift() {
        let directory = temp_dir();
        let mut bad_path = registration(&directory, "bad-path", 1);
        bad_path.artifacts.artifacts[0].relative_path = "../weights.bin".into();
        assert!(matches!(
            bad_path.validate(),
            Err(RegistryError::InvalidRegistration { .. })
        ));
        let mut bad_hash = registration(&directory, "bad-hash", 1);
        bad_hash.artifacts.artifacts[0].sha256 = ContentHash::from_bytes([9; 32]);
        assert!(matches!(
            bad_hash.validate(),
            Err(RegistryError::InvalidRegistration { .. })
        ));
        fs::remove_dir_all(directory).unwrap();
    }
}
