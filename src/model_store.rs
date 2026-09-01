//! Supervisor-owned, content-addressed storage for model-worker jobs.
//!
//! Workers receive a job sandbox but never a cache destination. They may write
//! only declared files in `staging`; the supervisor hashes, fsyncs, and
//! renames that directory into the cache after a successful wire completion.
//! This module deliberately has no process or GPUI dependency.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use crate::content_identity::{Digest, SchemaTag, Sha256Digest};
use crate::content_store::{FsContentStore, ObjectRef, PublishAcquire};
use crate::model_wire::{ArtifactDescriptor, WireError, WorkerResult};

const CACHE_DIRECTORY: &str = "cache";
const LOCK_DIRECTORY: &str = "locks";
const STAGING_DIRECTORY: &str = "staging";
const RESULT_MANIFEST: &str = ".audec-result.json";

/// A store root is owned by the controller. Paths arriving from a worker are
/// always interpreted relative to a particular [`JobSandbox`].
#[derive(Clone, Debug)]
pub struct ModelStore {
    root: PathBuf,
}

impl ModelStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Creates the fixed directory layout but no cache entry or worker job.
    pub fn ensure_layout(&self) -> Result<(), StoreError> {
        fs::create_dir_all(self.root.join(CACHE_DIRECTORY))
            .map_err(|error| self.io("create cache root", error))?;
        fs::create_dir_all(self.root.join(LOCK_DIRECTORY))
            .map_err(|error| self.io("create lock root", error))?;
        fs::create_dir_all(self.root.join(STAGING_DIRECTORY))
            .map_err(|error| self.io("create staging root", error))?;
        Ok(())
    }

    pub fn cached(&self, cache_key: &str) -> Result<Option<StoredResult>, StoreError> {
        validate_cache_key(cache_key)?;
        let directory = self.cache_directory(cache_key);
        if !directory.exists() {
            return Ok(None);
        }
        self.read_result(&directory).map(Some)
    }

    /// Acquires a filesystem-visible single-flight lease. A second controller
    /// gets `Busy`, while a validated completed entry wins over any lock.
    pub fn acquire(&self, job_id: &str, cache_key: &str) -> Result<CacheAcquire, StoreError> {
        validate_label("job_id", job_id)?;
        validate_cache_key(cache_key)?;
        self.ensure_layout()?;
        if let Some(result) = self.cached(cache_key)? {
            return Ok(CacheAcquire::Hit(result));
        }

        let lock_path = self.lock_path(cache_key);
        match fs::create_dir(&lock_path) {
            Ok(()) => {
                let sandbox = self.create_sandbox(job_id, cache_key)?;
                Ok(CacheAcquire::Acquired(CacheLease {
                    cache_key: cache_key.into(),
                    lock_path,
                    sandbox,
                }))
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(CacheAcquire::Busy {
                cache_key: cache_key.into(),
                lock_path,
            }),
            Err(error) => Err(self.io("acquire cache lock", error)),
        }
    }

    /// Lists interrupted job directories without deleting them. A coordinator
    /// can inspect these candidates or offer a recovery choice; a crash never
    /// silently converts partial output into a cache hit.
    pub fn recovery_candidates(&self) -> Result<Vec<PathBuf>, StoreError> {
        let root = self.root.join(STAGING_DIRECTORY);
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(self.io("scan staging root", error)),
        };
        let mut candidates = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| self.io("read staging entry", error))?;
            let file_type = entry
                .file_type()
                .map_err(|error| self.io("inspect staging entry", error))?;
            if file_type.is_dir() {
                candidates.push(entry.path());
            }
        }
        candidates.sort();
        Ok(candidates)
    }

    fn create_sandbox(&self, job_id: &str, cache_key: &str) -> Result<JobSandbox, StoreError> {
        // Inputs and output staging live below the cache parent, making every
        // path in a wire request relative to one deterministic worker root
        // while keeping the output directory a sibling of its destination.
        let wire_root = self.root.join(CACHE_DIRECTORY);
        let directory = wire_root.join(format!(".{cache_key}.{job_id}.inputs"));
        if directory.exists() {
            return Err(StoreError::Conflict(format!(
                "job sandbox already exists: {}",
                directory.display()
            )));
        }
        fs::create_dir(&directory).map_err(|error| self.io("create job sandbox", error))?;
        let input = directory.join("input");
        let references = directory.join("references");
        let masks = directory.join("masks");
        // The result directory is a sibling of its cache destination so the
        // final promotion is a single atomic rename. Inputs stay private to
        // the job sandbox and are never published with the result.
        let staging = wire_root.join(format!(".{cache_key}.{job_id}.tmp"));
        for path in [&input, &references, &masks] {
            fs::create_dir(path).map_err(|error| self.io("create job sandbox directory", error))?;
        }
        fs::create_dir(&staging)
            .map_err(|error| self.io("create result staging directory", error))?;
        Ok(JobSandbox {
            root: wire_root,
            job_directory: directory,
            input,
            references,
            masks,
            staging,
        })
    }

    fn read_result(&self, directory: &Path) -> Result<StoredResult, StoreError> {
        let manifest = directory.join(RESULT_MANIFEST);
        let bytes =
            fs::read(&manifest).map_err(|error| self.io("read stored result manifest", error))?;
        let result: WorkerResult =
            serde_json::from_slice(&bytes).map_err(StoreError::ResultDecode)?;
        validate_result_shape(&result)?;
        verify_declared_artifacts(directory, &result.artifacts)?;
        Ok(StoredResult {
            directory: directory.to_path_buf(),
            result,
        })
    }

    fn cache_directory(&self, cache_key: &str) -> PathBuf {
        self.root.join(CACHE_DIRECTORY).join(cache_key)
    }

    fn lock_path(&self, cache_key: &str) -> PathBuf {
        self.root
            .join(LOCK_DIRECTORY)
            .join(format!("{cache_key}.lock"))
    }

    fn io(&self, action: &'static str, error: io::Error) -> StoreError {
        StoreError::Io {
            action,
            path: self.root.clone(),
            error,
        }
    }
}

#[derive(Clone, Debug)]
pub struct JobSandbox {
    /// The process working root for all relative wire paths.
    root: PathBuf,
    /// Input-only private directory; never becomes a published cache entry.
    job_directory: PathBuf,
    input: PathBuf,
    references: PathBuf,
    masks: PathBuf,
    staging: PathBuf,
}

impl JobSandbox {
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn job_directory(&self) -> &Path {
        &self.job_directory
    }
    pub fn input_directory(&self) -> &Path {
        &self.input
    }
    pub fn references_directory(&self) -> &Path {
        &self.references
    }
    pub fn masks_directory(&self) -> &Path {
        &self.masks
    }
    pub fn staging_directory(&self) -> &Path {
        &self.staging
    }

    /// Relative names form the only paths a worker needs in a wire request.
    pub fn worker_relative(&self, path: &Path) -> Result<String, StoreError> {
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| StoreError::UnsafePath(path.to_path_buf()))?;
        validate_relative_path(relative)?;
        Ok(relative.to_string_lossy().into_owned())
    }
}

#[derive(Debug)]
pub enum CacheAcquire {
    Hit(StoredResult),
    Acquired(CacheLease),
    Busy {
        cache_key: String,
        lock_path: PathBuf,
    },
}

/// A lease owns the exclusive cache-key lock. Call `publish` on successful
/// completion or `abandon` for cancellation/failure. Drop also releases only
/// the lock, deliberately leaving staging available for crash diagnosis.
#[derive(Debug)]
pub struct CacheLease {
    cache_key: String,
    lock_path: PathBuf,
    sandbox: JobSandbox,
}

impl CacheLease {
    pub fn cache_key(&self) -> &str {
        &self.cache_key
    }
    pub fn sandbox(&self) -> &JobSandbox {
        &self.sandbox
    }

    /// Verifies the worker's declared files and atomically promotes the whole
    /// staging directory. Publication either creates one cache directory or
    /// leaves the staging directory untouched for recovery.
    pub fn publish(
        mut self,
        store: &ModelStore,
        result: WorkerResult,
    ) -> Result<StoredResult, StoreError> {
        validate_result_shape(&result)?;
        if result.cache_key != self.cache_key {
            return Err(StoreError::Conflict(
                "worker result cache key does not match its lease".into(),
            ));
        }
        verify_declared_artifacts(self.sandbox.staging_directory(), &result.artifacts)?;
        reject_undeclared_files(self.sandbox.staging_directory(), &result.artifacts)?;

        let manifest = self.sandbox.staging_directory().join(RESULT_MANIFEST);
        let encoded = serde_json::to_vec(&result).map_err(StoreError::ResultEncode)?;
        write_sync(&manifest, &encoded)?;
        sync_tree(self.sandbox.staging_directory())?;

        let destination = store.cache_directory(&self.cache_key);
        if destination.exists() {
            return Err(StoreError::Conflict(format!(
                "cache destination already exists: {}",
                destination.display()
            )));
        }
        fs::rename(self.sandbox.staging_directory(), &destination).map_err(|error| {
            StoreError::Io {
                action: "rename staged model result",
                path: destination.clone(),
                error,
            }
        })?;
        sync_directory(destination.parent().unwrap_or_else(|| Path::new(".")))?;

        // The sandbox itself remains (with immutable inputs) as a recovery
        // record until an explicit future retention policy removes it.
        self.release_lock();
        Ok(StoredResult {
            directory: destination,
            result,
        })
    }

    pub fn abandon(mut self) {
        self.release_lock();
    }

    fn release_lock(&mut self) {
        if self.lock_path.as_os_str().is_empty() {
            return;
        }
        let _ = fs::remove_dir(&self.lock_path);
        self.lock_path.clear();
    }
}

impl Drop for CacheLease {
    fn drop(&mut self) {
        self.release_lock();
    }
}

#[derive(Clone, Debug)]
pub struct StoredResult {
    pub directory: PathBuf,
    pub result: WorkerResult,
}

impl StoredResult {
    /// Mirror verified worker outputs into the shared immutable CAS. The model
    /// store remains the owner of the worker result-set manifest; each returned
    /// object is independently reopenable and cryptographically verified.
    pub fn publish_content_objects(
        &self,
        store: &FsContentStore,
    ) -> Result<Vec<StoredModelArtifact>, StoreError> {
        let mut published = Vec::with_capacity(self.result.artifacts.len());
        for descriptor in &self.result.artifacts {
            let path = self.directory.join(&descriptor.relative_path);
            validate_relative_path(Path::new(&descriptor.relative_path))?;
            let (raw, raw_len) =
                Sha256Digest::hash_raw_reader(File::open(&path).map_err(|error| {
                    StoreError::Io {
                        action: "open model artifact for CAS publication",
                        path: path.clone(),
                        error,
                    }
                })?)
                .map_err(|error| StoreError::Io {
                    action: "hash model artifact for CAS publication",
                    path: path.clone(),
                    error,
                })?;
            if raw_len != descriptor.byte_len || raw.to_hex() != descriptor.sha256 {
                return Err(StoreError::Verification(format!(
                    "model artifact changed before CAS publication: {}",
                    descriptor.relative_path
                )));
            }
            let schema = SchemaTag::model_artifact(
                "audec.model-worker-artifact",
                descriptor.schema_revision,
            )
            .map_err(StoreError::ContentIdentity)?;
            let (digest, byte_len) = Digest::of_reader(
                schema,
                File::open(&path).map_err(|error| StoreError::Io {
                    action: "open model artifact for content identity",
                    path: path.clone(),
                    error,
                })?,
            )
            .map_err(StoreError::ContentIdentity)?;
            let object = ObjectRef { digest, byte_len };
            let stored = match store
                .acquire(object.clone())
                .map_err(StoreError::ContentStore)?
            {
                PublishAcquire::Hit(stored) => stored,
                PublishAcquire::Busy { .. } => {
                    return Err(StoreError::Conflict(format!(
                        "content publication is busy for {}",
                        descriptor.relative_path
                    )));
                }
                PublishAcquire::Acquired(lease) => {
                    lease
                        .publish_reader(
                            store,
                            File::open(&path).map_err(|error| StoreError::Io {
                                action: "open model artifact for CAS publication",
                                path: path.clone(),
                                error,
                            })?,
                        )
                        .map_err(StoreError::ContentStore)?
                        .stored
                }
            };
            published.push(StoredModelArtifact {
                descriptor: descriptor.clone(),
                object: stored.object,
            });
        }
        Ok(published)
    }
}

#[derive(Clone, Debug)]
pub struct StoredModelArtifact {
    pub descriptor: ArtifactDescriptor,
    pub object: ObjectRef,
}

#[derive(Debug)]
pub enum StoreError {
    Io {
        action: &'static str,
        path: PathBuf,
        error: io::Error,
    },
    ResultEncode(serde_json::Error),
    ResultDecode(serde_json::Error),
    Wire(WireError),
    UnsafePath(PathBuf),
    Conflict(String),
    Verification(String),
    ContentIdentity(crate::content_identity::IdentityError),
    ContentStore(crate::content_store::StoreError),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                action,
                path,
                error,
            } => write!(f, "could not {action} at {}: {error}", path.display()),
            Self::ResultEncode(error) => write!(f, "could not encode model result: {error}"),
            Self::ResultDecode(error) => write!(f, "could not decode model result: {error}"),
            Self::Wire(error) => error.fmt(f),
            Self::UnsafePath(path) => write!(f, "unsafe model-store path: {}", path.display()),
            Self::Conflict(detail) | Self::Verification(detail) => f.write_str(detail),
            Self::ContentIdentity(error) => error.fmt(f),
            Self::ContentStore(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for StoreError {}
impl From<WireError> for StoreError {
    fn from(error: WireError) -> Self {
        Self::Wire(error)
    }
}

fn validate_result_shape(result: &WorkerResult) -> Result<(), StoreError> {
    // Reuse the public wire decoder as the authority for result validation.
    let line = serde_json::to_string(&crate::model_wire::WireEnvelope::new(
        0,
        crate::model_wire::WireMessage::Complete {
            result: result.clone(),
        },
    ))
    .map_err(StoreError::ResultEncode)?;
    crate::model_wire::WireEnvelope::from_jsonl(&line)
        .map(|_| ())
        .map_err(StoreError::Wire)
}

fn verify_declared_artifacts(
    root: &Path,
    artifacts: &[ArtifactDescriptor],
) -> Result<(), StoreError> {
    for artifact in artifacts {
        let path = root.join(&artifact.relative_path);
        validate_relative_path(Path::new(&artifact.relative_path))?;
        let metadata = fs::symlink_metadata(&path).map_err(|error| StoreError::Io {
            action: "inspect staged artifact",
            path: path.clone(),
            error,
        })?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(StoreError::Verification(format!(
                "staged artifact is not a regular file: {}",
                artifact.relative_path
            )));
        }
        if metadata.len() != artifact.byte_len {
            return Err(StoreError::Verification(format!(
                "staged artifact length differs from declaration: {}",
                artifact.relative_path
            )));
        }
        let actual = sha256_file(&path).map_err(|error| StoreError::Io {
            action: "hash staged artifact",
            path: path.clone(),
            error,
        })?;
        if actual != artifact.sha256 {
            return Err(StoreError::Verification(format!(
                "staged artifact digest differs from declaration: {}",
                artifact.relative_path
            )));
        }
    }
    Ok(())
}

fn reject_undeclared_files(
    root: &Path,
    artifacts: &[ArtifactDescriptor],
) -> Result<(), StoreError> {
    let declared: BTreeSet<PathBuf> = artifacts
        .iter()
        .map(|artifact| PathBuf::from(&artifact.relative_path))
        .collect();
    let mut actual = BTreeSet::new();
    collect_regular_files(root, root, &mut actual)?;
    if actual != declared {
        return Err(StoreError::Verification(
            "staging contains undeclared or missing files".into(),
        ));
    }
    Ok(())
}

fn collect_regular_files(
    root: &Path,
    directory: &Path,
    output: &mut BTreeSet<PathBuf>,
) -> Result<(), StoreError> {
    for entry in fs::read_dir(directory).map_err(|error| StoreError::Io {
        action: "read staging directory",
        path: directory.to_path_buf(),
        error,
    })? {
        let entry = entry.map_err(|error| StoreError::Io {
            action: "read staging entry",
            path: directory.to_path_buf(),
            error,
        })?;
        let type_ = entry.file_type().map_err(|error| StoreError::Io {
            action: "inspect staging entry",
            path: entry.path(),
            error,
        })?;
        if type_.is_symlink() {
            return Err(StoreError::Verification(format!(
                "staging may not contain symlinks: {}",
                entry.path().display()
            )));
        }
        if type_.is_dir() {
            collect_regular_files(root, &entry.path(), output)?;
        } else if type_.is_file() {
            let relative = entry
                .path()
                .strip_prefix(root)
                .map_err(|_| StoreError::UnsafePath(entry.path()))?
                .to_path_buf();
            output.insert(relative);
        } else {
            return Err(StoreError::Verification(format!(
                "staging contains a non-regular entry: {}",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

fn validate_cache_key(value: &str) -> Result<(), StoreError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(StoreError::Verification(
            "cache key must be a lowercase SHA-256".into(),
        ));
    }
    Ok(())
}

fn validate_label(field: &str, value: &str) -> Result<(), StoreError> {
    if value.is_empty()
        || value.len() > 160
        || value.contains(char::is_whitespace)
        || value.contains(['/', '\\', '\0'])
    {
        return Err(StoreError::Verification(format!(
            "{field} must be a compact non-path label"
        )));
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<(), StoreError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(StoreError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

fn write_sync(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| StoreError::Io {
            action: "create result manifest",
            path: path.to_path_buf(),
            error,
        })?;
    file.write_all(bytes).map_err(|error| StoreError::Io {
        action: "write result manifest",
        path: path.to_path_buf(),
        error,
    })?;
    file.sync_all().map_err(|error| StoreError::Io {
        action: "sync result manifest",
        path: path.to_path_buf(),
        error,
    })
}

fn sync_tree(directory: &Path) -> Result<(), StoreError> {
    for entry in fs::read_dir(directory).map_err(|error| StoreError::Io {
        action: "read staging tree",
        path: directory.to_path_buf(),
        error,
    })? {
        let entry = entry.map_err(|error| StoreError::Io {
            action: "read staging tree entry",
            path: directory.to_path_buf(),
            error,
        })?;
        let type_ = entry.file_type().map_err(|error| StoreError::Io {
            action: "inspect staging tree entry",
            path: entry.path(),
            error,
        })?;
        if type_.is_dir() {
            sync_tree(&entry.path())?;
        }
        if type_.is_file() {
            File::open(entry.path())
                .and_then(|file| file.sync_all())
                .map_err(|error| StoreError::Io {
                    action: "sync staged artifact",
                    path: entry.path(),
                    error,
                })?;
        }
    }
    sync_directory(directory)
}

fn sync_directory(path: &Path) -> Result<(), StoreError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| StoreError::Io {
            action: "sync staging directory",
            path: path.to_path_buf(),
            error,
        })
}

fn sha256_file(path: &Path) -> io::Result<String> {
    Sha256Digest::hash_raw_reader(File::open(path)?).map(|(digest, _)| digest.to_hex())
}

// Streaming SHA-256 avoids loading large audio/model sidecars merely to
// verify a worker declaration. It is kept local because registry hashing is
// intentionally private implementation detail today.
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
            let n = (64 - self.buffer_len).min(input.len());
            self.buffer[self.buffer_len..self.buffer_len + n].copy_from_slice(&input[..n]);
            self.buffer_len += n;
            input = &input[n..];
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
    fn finish_hex(mut self) -> String {
        let bits = self.total_len.wrapping_mul(8);
        self.buffer[self.buffer_len] = 0x80;
        self.buffer_len += 1;
        if self.buffer_len > 56 {
            self.buffer[self.buffer_len..].fill(0);
            compress(&mut self.state, &self.buffer);
            self.buffer_len = 0;
        }
        self.buffer[self.buffer_len..56].fill(0);
        self.buffer[56..].copy_from_slice(&bits.to_be_bytes());
        compress(&mut self.state, &self.buffer);
        let mut text = String::with_capacity(64);
        for word in self.state {
            use std::fmt::Write as _;
            write!(&mut text, "{word:08x}").unwrap();
        }
        text
    }
}
fn compress(state: &mut [u32; 8], block: &[u8]) {
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
    let mut w = [0u32; 64];
    for (i, chunk) in block.chunks_exact(4).enumerate() {
        w[i] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_empty_digest() {
        assert_eq!(
            Sha256Digest::hash_raw(b"").to_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn verified_set_publishes_as_one_cache_entry() {
        let root = std::env::temp_dir().join(format!(
            "audec-model-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = ModelStore::new(&root);
        let key = "ab".repeat(32);
        let lease = match store.acquire("job-1", &key).unwrap() {
            CacheAcquire::Acquired(lease) => lease,
            _ => panic!("fresh cache key must acquire a lease"),
        };
        fs::write(lease.sandbox().staging_directory().join("empty.json"), []).unwrap();
        let result = WorkerResult {
            job_id: "job-1".into(),
            cache_key: key.clone(),
            artifacts: vec![ArtifactDescriptor {
                relative_path: "empty.json".into(),
                sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
                byte_len: 0,
                kind: crate::model_wire::ArtifactKind::Measurement,
                media_type: "application/json".into(),
                schema_revision: 1,
                time_base_hz: None,
                additivity: crate::model_wire::AdditivityDeclaration::NonAudio,
                source_backlinks: vec![],
            }],
            measurements: vec![],
        };
        let published = lease.publish(&store, result).unwrap();
        assert!(published.directory.ends_with(&key));
        assert!(store.cached(&key).unwrap().is_some());

        let content_store = FsContentStore::new(root.join("content"));
        let objects = published.publish_content_objects(&content_store).unwrap();
        assert_eq!(objects.len(), 1);
        assert_eq!(
            objects[0].object.digest.schema().class(),
            crate::content_identity::ContentClass::ModelArtifact
        );
        assert_eq!(
            content_store.read_verified(&objects[0].object, 1).unwrap(),
            b""
        );
        fs::remove_dir_all(root).unwrap();
    }
}
