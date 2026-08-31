//! Per-project task service for optional isolated model analysis.
//!
//! The service is deliberately GPUI-neutral. It owns task lifecycle and the
//! project's in-memory claim catalogue, while callers supply a UI action,
//! CLI command, or future command-envelope bridge. A task is not a mixer
//! edit: publication creates immutable evidence material only.

use std::collections::BTreeMap;
use std::fmt;

use crate::model_claim::{ModelClaimBundle, ModelClaimId};
use crate::model_registry::{InstallStatus, ModelRegistry, RegistryError};
use crate::model_store::{ModelStore, StoreError, StoredResult};
use crate::model_wire::AnalyzeRequest;
use crate::model_wire::WireParameter;
use crate::worker_runtime::{
    ClaimPublication, RuntimeEvent, RuntimeReservation, WorkerLaunch, WorkerRuntime,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModelTaskId(u64);

impl ModelTaskId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The caller-provided, already-decoded material contract. `bytes` are copied
/// into a job sandbox; no project/UI mutable state crosses into the worker.
#[derive(Clone, Debug)]
pub struct TaskMaterial {
    pub sha256: String,
    pub bytes: Vec<u8>,
    pub start_frame: u64,
    pub frame_count: u64,
    pub channel_selection: Vec<u16>,
}

#[derive(Clone, Debug)]
pub struct ModelTaskRecipe {
    pub model_id: String,
    /// Must be the cache identity formed by the adapter from its full,
    /// canonical model-worker request. The service never invents a cache key.
    pub cache_key: String,
    pub material: TaskMaterial,
    pub prompt: Option<String>,
    pub reference_sha256: Vec<String>,
    pub mask_sha256: Vec<String>,
    pub parameters: BTreeMap<String, WireParameter>,
    /// Immutable interpretation that is attached after verified publication.
    pub publication: ClaimPublication,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelTaskStatus {
    Queued,
    Running {
        completed_chunks: u64,
        total_chunks: u64,
        phase: crate::model_wire::ResultPhase,
    },
    Cancelling,
    Published {
        claim_id: ModelClaimId,
        cache_hit: bool,
    },
    Cancelled,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskDiagnosticKind {
    Install,
    Launch,
    Protocol,
    Cache,
    Worker,
    Crash,
    Cancel,
    Claim,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskDiagnostic {
    pub kind: TaskDiagnosticKind,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub struct ModelTask {
    pub id: ModelTaskId,
    pub recipe: ModelTaskRecipe,
    pub status: ModelTaskStatus,
    pub diagnostics: Vec<TaskDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelAvailability {
    UnknownModel,
    Installed,
    Missing,
    Invalid,
    RegistryUnavailable,
    UnsafeInstall,
}

/// One per-project service. It intentionally executes at most one job at a
/// time today because a runtime represents one child worker process. Cache
/// hits and claim restoration do not need a worker slot.
#[derive(Debug)]
pub struct ModelTaskService {
    registry: ModelRegistry,
    store: ModelStore,
    launch: WorkerLaunch,
    next_id: u64,
    tasks: BTreeMap<ModelTaskId, ModelTask>,
    claims: BTreeMap<ModelClaimId, ModelClaimBundle>,
    active: Option<ActiveTask>,
}

#[derive(Debug)]
struct ActiveTask {
    id: ModelTaskId,
    runtime: WorkerRuntime,
}

impl ModelTaskService {
    pub fn new(registry: ModelRegistry, store: ModelStore, launch: WorkerLaunch) -> Self {
        Self {
            registry,
            store,
            launch,
            next_id: 1,
            tasks: BTreeMap::new(),
            claims: BTreeMap::new(),
            active: None,
        }
    }

    pub fn availability(&self, model_id: &str) -> Result<ModelAvailability, TaskServiceError> {
        let Some(registration) = self.registry.get(model_id) else {
            return Ok(ModelAvailability::UnknownModel);
        };
        Ok(match self.registry.verify(registration)? {
            InstallStatus::Installed { .. } => ModelAvailability::Installed,
            InstallStatus::Missing { .. } => ModelAvailability::Missing,
            InstallStatus::Invalid { .. } => ModelAvailability::Invalid,
            InstallStatus::RegistryRootUnavailable => ModelAvailability::RegistryUnavailable,
            InstallStatus::UnsafeInstallDirectory => ModelAvailability::UnsafeInstall,
        })
    }

    pub fn task(&self, id: ModelTaskId) -> Option<&ModelTask> {
        self.tasks.get(&id)
    }

    pub fn tasks(&self) -> impl Iterator<Item = &ModelTask> {
        self.tasks.values()
    }

    pub fn claim(&self, id: &ModelClaimId) -> Option<&ModelClaimBundle> {
        self.claims.get(id)
    }

    pub fn claims(&self) -> impl Iterator<Item = &ModelClaimBundle> {
        self.claims.values()
    }

    /// Start a task or restore a validated cache entry. Missing/invalid
    /// models create a retained Failed task with an install diagnostic rather
    /// than a silent no-op.
    pub fn run(&mut self, recipe: ModelTaskRecipe) -> Result<ModelTaskId, TaskServiceError> {
        let id = self.allocate_id();
        self.tasks.insert(
            id,
            ModelTask {
                id,
                recipe: recipe.clone(),
                status: ModelTaskStatus::Queued,
                diagnostics: Vec::new(),
            },
        );

        let Some(registration) = self.registry.get(&recipe.model_id).cloned() else {
            self.fail(
                id,
                TaskDiagnosticKind::Install,
                "unknown model registration",
            );
            return Ok(id);
        };
        // Published cache entries are immutable evidence material. They are
        // intentionally restorable even after an optional runtime has been
        // removed, as long as the pinned registration/recipe remains known.
        match self.store.cached(&recipe.cache_key) {
            Ok(Some(stored)) => {
                if let Err(error) = self.restore_cached(id, stored) {
                    self.fail(id, TaskDiagnosticKind::Claim, error.to_string());
                }
                return Ok(id);
            }
            Ok(None) => {}
            Err(error) => {
                self.fail(id, TaskDiagnosticKind::Cache, error.to_string());
                return Ok(id);
            }
        }
        if self.active.is_some() {
            self.fail(
                id,
                TaskDiagnosticKind::Launch,
                "another model task is already running",
            );
            return Ok(id);
        }
        let availability = match self.availability(&recipe.model_id) {
            Ok(availability) => availability,
            Err(error) => {
                self.fail(id, TaskDiagnosticKind::Install, error.to_string());
                return Ok(id);
            }
        };
        if availability != ModelAvailability::Installed {
            self.fail(
                id,
                TaskDiagnosticKind::Install,
                format!("model is not installed and verified: {availability:?}"),
            );
            return Ok(id);
        }

        let mut runtime = match WorkerRuntime::launch(&self.store, self.launch.clone()) {
            Ok(runtime) => runtime,
            Err(error) => {
                self.fail(id, TaskDiagnosticKind::Launch, error.to_string());
                return Ok(id);
            }
        };
        if let Err(error) = runtime.load_registered(&self.registry, &registration) {
            self.fail(id, TaskDiagnosticKind::Install, error.to_string());
            return Ok(id);
        }

        let job_id = format!("model-task-{}", id.get());
        match runtime.reserve(&self.store, &job_id, &recipe.cache_key) {
            Ok(RuntimeReservation::CacheHit(stored)) => {
                self.restore_cached(id, stored)?;
                return Ok(id);
            }
            Ok(RuntimeReservation::Busy) => {
                self.fail(
                    id,
                    TaskDiagnosticKind::Cache,
                    "cache key is already being computed",
                );
                return Ok(id);
            }
            Ok(RuntimeReservation::Reserved(sandbox)) => {
                let material = match WorkerRuntime::write_input_bytes(
                    &sandbox,
                    "material.pcm",
                    &recipe.material.bytes,
                ) {
                    Ok(path) => path,
                    Err(error) => {
                        self.fail(id, TaskDiagnosticKind::Cache, error.to_string());
                        return Ok(id);
                    }
                };
                let files = match WorkerRuntime::job_files(&sandbox, material) {
                    Ok(files) => files,
                    Err(error) => {
                        self.fail(id, TaskDiagnosticKind::Cache, error.to_string());
                        return Ok(id);
                    }
                };
                let manifest = match registration.manifest.canonical_hash() {
                    Ok(manifest) => manifest,
                    Err(error) => {
                        self.fail(id, TaskDiagnosticKind::Install, error.to_string());
                        return Ok(id);
                    }
                };
                let request = AnalyzeRequest {
                    job_id,
                    model_manifest_sha256: manifest.to_string(),
                    cache_key: recipe.cache_key.clone(),
                    material_sha256: recipe.material.sha256.clone(),
                    start_frame: recipe.material.start_frame,
                    frame_count: recipe.material.frame_count,
                    channel_selection: recipe.material.channel_selection.clone(),
                    prompt: recipe.prompt.clone(),
                    reference_sha256: recipe.reference_sha256.clone(),
                    mask_sha256: recipe.mask_sha256.clone(),
                    parameters: recipe.parameters.clone(),
                    files,
                };
                if let Err(error) = runtime.analyze_claim(request, recipe.publication.clone()) {
                    self.fail(id, TaskDiagnosticKind::Protocol, error.to_string());
                    return Ok(id);
                }
                self.set_status(
                    id,
                    ModelTaskStatus::Running {
                        completed_chunks: 0,
                        total_chunks: 0,
                        phase: crate::model_wire::ResultPhase::Preparing,
                    },
                );
                self.active = Some(ActiveTask { id, runtime });
            }
            Err(error) => self.fail(id, TaskDiagnosticKind::Cache, error.to_string()),
        }
        Ok(id)
    }

    /// Retry creates a distinct task and job identity. It may restore a cache
    /// instead of executing a second worker, which is correct for identical
    /// deterministic recipes.
    pub fn retry(&mut self, prior: ModelTaskId) -> Result<ModelTaskId, TaskServiceError> {
        let recipe = self
            .tasks
            .get(&prior)
            .ok_or(TaskServiceError::UnknownTask(prior))?
            .recipe
            .clone();
        self.run(recipe)
    }

    pub fn cancel(&mut self, id: ModelTaskId) -> Result<(), TaskServiceError> {
        let Some(active) = self.active.as_mut() else {
            return Err(TaskServiceError::NotRunning(id));
        };
        if active.id != id {
            return Err(TaskServiceError::NotRunning(id));
        }
        active.runtime.cancel(format!("model-task-{}", id.get()))?;
        self.set_status(id, ModelTaskStatus::Cancelling);
        Ok(())
    }

    /// Block for one worker event. A UI can call this from a task/pump thread
    /// and render the immutable `ModelTask` snapshot on its own schedule.
    pub fn poll(&mut self) -> Result<bool, TaskServiceError> {
        let Some(active) = self.active.as_mut() else {
            return Ok(false);
        };
        let id = active.id;
        let event = active.runtime.receive(&self.store);
        match event {
            Ok(Some(event)) => self.apply_runtime_event(id, event),
            Ok(None) => {}
            Err(error) => {
                self.fail(id, classify_runtime_error(&error), error.to_string());
                self.active = None;
            }
        }
        Ok(true)
    }

    /// Explicit process escalation for a cancelled/stuck task. No staged
    /// output is published on this path; the cache store retains it for later
    /// inspection/recovery.
    pub fn terminate_active(&mut self, likely_oom: bool) -> Result<(), TaskServiceError> {
        let Some(active) = self.active.as_mut() else {
            return Ok(());
        };
        let id = active.id;
        let events = active.runtime.terminate(likely_oom)?;
        for event in events {
            self.apply_runtime_event(id, event);
        }
        self.active = None;
        Ok(())
    }

    fn restore_cached(
        &mut self,
        id: ModelTaskId,
        stored: StoredResult,
    ) -> Result<(), TaskServiceError> {
        let task = self
            .tasks
            .get(&id)
            .ok_or(TaskServiceError::UnknownTask(id))?;
        let claim = ModelClaimBundle::from_worker_result(
            task.recipe.publication.model_manifest_sha256.clone(),
            task.recipe.publication.source.clone(),
            task.recipe.publication.runtime.clone(),
            task.recipe.publication.additivity.clone(),
            stored.result,
            task.recipe.publication.outputs.clone(),
        )?;
        let claim_id = claim.id.clone();
        self.claims.insert(claim_id.clone(), claim);
        self.set_status(
            id,
            ModelTaskStatus::Published {
                claim_id,
                cache_hit: true,
            },
        );
        Ok(())
    }

    fn apply_runtime_event(&mut self, id: ModelTaskId, event: RuntimeEvent) {
        match event {
            RuntimeEvent::Progress(progress) => self.set_status(
                id,
                ModelTaskStatus::Running {
                    completed_chunks: progress.completed_chunks,
                    total_chunks: progress.total_chunks,
                    phase: progress.phase,
                },
            ),
            RuntimeEvent::ClaimPublished { claim, .. } => {
                let claim_id = claim.id.clone();
                self.claims.insert(claim_id.clone(), claim);
                self.set_status(
                    id,
                    ModelTaskStatus::Published {
                        claim_id,
                        cache_hit: false,
                    },
                );
                self.active = None;
            }
            RuntimeEvent::Published(_) => {
                self.fail(
                    id,
                    TaskDiagnosticKind::Claim,
                    "worker result had no claim publication recipe",
                );
                self.active = None;
            }
            RuntimeEvent::Cancelled { .. } => {
                self.set_status(id, ModelTaskStatus::Cancelled);
                self.active = None;
            }
            RuntimeEvent::Failed(failure) => {
                self.fail(id, TaskDiagnosticKind::Worker, format!("{failure:?}"));
                self.active = None;
            }
            RuntimeEvent::JobTerminated { failure, .. } => {
                self.fail(id, TaskDiagnosticKind::Crash, format!("{failure:?}"));
                self.active = None;
            }
        }
    }

    fn allocate_id(&mut self) -> ModelTaskId {
        let id = ModelTaskId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    fn set_status(&mut self, id: ModelTaskId, status: ModelTaskStatus) {
        if let Some(task) = self.tasks.get_mut(&id) {
            task.status = status;
        }
    }

    fn fail(&mut self, id: ModelTaskId, kind: TaskDiagnosticKind, detail: impl Into<String>) {
        if let Some(task) = self.tasks.get_mut(&id) {
            task.diagnostics.push(TaskDiagnostic {
                kind,
                detail: detail.into(),
            });
            task.status = ModelTaskStatus::Failed;
        }
    }
}

fn classify_runtime_error(error: &crate::worker_runtime::RuntimeError) -> TaskDiagnosticKind {
    use crate::worker_runtime::RuntimeError;
    match error {
        RuntimeError::Launch { .. } | RuntimeError::MissingPipe(_) => TaskDiagnosticKind::Launch,
        RuntimeError::Store(_) => TaskDiagnosticKind::Cache,
        RuntimeError::Registry(_) | RuntimeError::ModelUnavailable(_) => {
            TaskDiagnosticKind::Install
        }
        RuntimeError::Claim(_) => TaskDiagnosticKind::Claim,
        RuntimeError::WorkerExited(_) => TaskDiagnosticKind::Crash,
        RuntimeError::Supervisor(_) | RuntimeError::Wire(_) | RuntimeError::Protocol(_) => {
            TaskDiagnosticKind::Protocol
        }
        RuntimeError::Io { .. } => TaskDiagnosticKind::Worker,
    }
}

#[derive(Debug)]
pub enum TaskServiceError {
    Registry(RegistryError),
    Store(StoreError),
    Runtime(crate::worker_runtime::RuntimeError),
    Claim(crate::model_claim::ClaimError),
    Manifest(crate::model_worker::ValidationError),
    UnknownTask(ModelTaskId),
    NotRunning(ModelTaskId),
}

impl fmt::Display for TaskServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registry(error) => error.fmt(f),
            Self::Store(error) => error.fmt(f),
            Self::Runtime(error) => error.fmt(f),
            Self::Claim(error) => error.fmt(f),
            Self::Manifest(error) => error.fmt(f),
            Self::UnknownTask(id) => write!(f, "unknown model task {}", id.get()),
            Self::NotRunning(id) => write!(f, "model task {} is not running", id.get()),
        }
    }
}
impl std::error::Error for TaskServiceError {}
impl From<RegistryError> for TaskServiceError {
    fn from(value: RegistryError) -> Self {
        Self::Registry(value)
    }
}
impl From<StoreError> for TaskServiceError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}
impl From<crate::worker_runtime::RuntimeError> for TaskServiceError {
    fn from(value: crate::worker_runtime::RuntimeError) -> Self {
        Self::Runtime(value)
    }
}
impl From<crate::model_claim::ClaimError> for TaskServiceError {
    fn from(value: crate::model_claim::ClaimError) -> Self {
        Self::Claim(value)
    }
}
impl From<crate::model_worker::ValidationError> for TaskServiceError {
    fn from(value: crate::model_worker::ValidationError) -> Self {
        Self::Manifest(value)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::model_claim::{ClaimSource, WorkerRuntimeProvenance};
    use crate::model_registry::{
        ArtifactLock, ArtifactManifest, ArtifactRole, DownloadState, ModelCapability,
        ModelRegistration, RuntimeDescriptor, WorkerDescriptor,
    };
    use crate::model_wire::AdditivityDeclaration;
    use crate::model_worker::{
        Architecture, AudioContract, Backend, ChannelContract, ContentHash, ExactRevision,
        ExecutionContract, LicenseProvenance, LicenseReference, ModelArtifacts, ModelManifest,
        Normalization, NumericPrecision, OutputAdditivity, OutputContract, Redistribution,
        SampleEncoding, TrainingProvenance, PROTOCOL_VERSION,
    };

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn temp_root() -> std::path::PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "audec-model-task-service-{}-{stamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn hash() -> ContentHash {
        ContentHash::from_str(EMPTY).unwrap()
    }

    fn registry(root: &Path) -> ModelRegistry {
        let models = root.join("models");
        let installed = models.join("fake-model");
        fs::create_dir_all(&installed).unwrap();
        fs::write(installed.join("weights.bin"), []).unwrap();
        fs::write(installed.join("config.json"), []).unwrap();
        let hash = hash();
        let license = LicenseProvenance {
            code: LicenseReference::Spdx("MIT".into()),
            checkpoint: LicenseReference::Spdx("MIT".into()),
            redistribution: Redistribution::RequiresReview,
            source_url: None,
            review_notes: "test-only reviewed fixture".into(),
        };
        let registration = ModelRegistration {
            manifest: ModelManifest {
                schema_version: 1,
                model_id: "fake-model".into(),
                architecture: Architecture {
                    family: "fake".into(),
                    version: "v1".into(),
                },
                revision: ExactRevision::Commit(hash),
                artifacts: ModelArtifacts {
                    weights_sha256: hash,
                    config_sha256: hash,
                    adapter_sha256: None,
                    conversion_recipe_sha256: None,
                    numerical_validation_sha256: None,
                },
                license: license.clone(),
                training: TrainingProvenance {
                    summary: "local fake fixture".into(),
                    sources: vec![],
                    documentation_sha256: hash,
                },
                input: AudioContract {
                    sample_rate_hz: 44_100,
                    channels: ChannelContract::Mono,
                    encoding: SampleEncoding::Float32Le,
                },
                execution: ExecutionContract {
                    chunk_frames: 44_100,
                    overlap_frames: 0,
                    normalization: Normalization::None,
                    backend: Backend::Cpu {
                        runtime: "fake-runtime".into(),
                        precision: NumericPrecision::Float32,
                    },
                    estimated_peak_memory_bytes: 1,
                    required_accelerators: vec![],
                },
                output: OutputContract {
                    names: vec!["fake-output".into()],
                    sample_rate_hz: 44_100,
                    channels: ChannelContract::Mono,
                    additivity: OutputAdditivity::NonAudio {
                        units: "fake values".into(),
                    },
                },
                golden_validations: vec![],
            },
            artifacts: ArtifactManifest {
                install_directory: "fake-model".into(),
                artifacts: vec![
                    ArtifactLock {
                        role: ArtifactRole::Weights,
                        relative_path: "weights.bin".into(),
                        sha256: hash,
                        byte_len: Some(0),
                        required: true,
                    },
                    ArtifactLock {
                        role: ArtifactRole::Configuration,
                        relative_path: "config.json".into(),
                        sha256: hash,
                        byte_len: Some(0),
                        required: true,
                    },
                ],
            },
            license,
            capabilities: BTreeSet::from([ModelCapability::BeatAndDownbeat]),
            selection_priority: 0,
            workers: vec![WorkerDescriptor {
                worker_name: "fake-service-worker".into(),
                runtime: RuntimeDescriptor {
                    runtime_id: "fake-cpu".into(),
                    protocol_version: PROTOCOL_VERSION,
                    supported_backends: BTreeSet::from(["cpu".into()]),
                    required_accelerators: BTreeSet::new(),
                },
            }],
            download_state: DownloadState::UserDownloadRequired,
        };
        let mut registry = ModelRegistry::new(models);
        registry.register(registration).unwrap();
        registry
    }

    fn fake_worker(root: &Path) -> std::path::PathBuf {
        let script = root.join("fake-worker.sh");
        fs::write(&script, r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"kind":"hello"'*)
      printf '%s\n' '{"protocol_version":1,"sequence":0,"kind":"capabilities","capabilities":{"worker_name":"fake-service-worker","backends":["cpu"],"maximum_parallel_jobs":1,"shared_memory":false}}'
      ;;
    *'"kind":"load_model"'*)
      manifest=$(printf '%s' "$line" | sed -n 's/.*"manifest_sha256":"\([^"]*\)".*/\1/p')
      printf '{"protocol_version":1,"sequence":1,"kind":"model_loaded","manifest_sha256":"%s"}\n' "$manifest"
      ;;
    *'"kind":"analyze"'*)
      job=$(printf '%s' "$line" | sed -n 's/.*"job_id":"\([^"]*\)".*/\1/p')
      key=$(printf '%s' "$line" | sed -n 's/.*"cache_key":"\([^"]*\)".*/\1/p')
      staging=$(printf '%s' "$line" | sed -n 's/.*"staging_directory":"\([^"]*\)".*/\1/p')
      mkdir -p "$staging"
      : > "$staging/empty.json"
      printf '{"protocol_version":1,"sequence":2,"kind":"progress","progress":{"job_id":"%s","phase":"analyzing","completed_chunks":1,"total_chunks":2}}\n' "$job"
      printf '{"protocol_version":1,"sequence":3,"kind":"progress","progress":{"job_id":"%s","phase":"verifying","completed_chunks":2,"total_chunks":2}}\n' "$job"
      printf '{"protocol_version":1,"sequence":4,"kind":"complete","result":{"job_id":"%s","cache_key":"%s","artifacts":[{"relative_path":"empty.json","sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","byte_len":0,"kind":"measurement","media_type":"application/json","schema_revision":1,"additivity":"non_audio","source_backlinks":[]}],"measurements":[]}}\n' "$job" "$key"
      ;;
  esac
done
"#).unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        script
    }

    fn recipe() -> ModelTaskRecipe {
        ModelTaskRecipe {
            model_id: "fake-model".into(),
            cache_key: "aa".repeat(32),
            material: TaskMaterial {
                sha256: "bb".repeat(32),
                bytes: vec![0.0f32.to_le_bytes()[0]],
                start_frame: 0,
                frame_count: 1,
                channel_selection: vec![],
            },
            prompt: None,
            reference_sha256: vec![],
            mask_sha256: vec![],
            parameters: BTreeMap::new(),
            publication: ClaimPublication {
                model_manifest_sha256: String::new(),
                source: ClaimSource {
                    material_sha256: "bb".repeat(32),
                    start_frame: 0,
                    frame_count: 1,
                    sample_rate_hz: 44_100,
                    channels: 1,
                },
                runtime: WorkerRuntimeProvenance {
                    worker_name: "fake-service-worker".into(),
                    runtime: "fake-runtime".into(),
                    adapter_sha256: None,
                },
                additivity: AdditivityDeclaration::NonAudio,
                outputs: vec![("fake-output".into(), vec![])],
            },
        }
    }

    #[test]
    fn fake_worker_task_publishes_then_restores_a_claim_from_cache() {
        let root = temp_root();
        let registry = registry(&root);
        let manifest = registry
            .get("fake-model")
            .unwrap()
            .manifest
            .canonical_hash()
            .unwrap()
            .to_string();
        let mut recipe = recipe();
        recipe.publication.model_manifest_sha256 = manifest;
        let script = fake_worker(&root);
        let mut service = ModelTaskService::new(
            registry,
            ModelStore::new(root.join("store")),
            WorkerLaunch {
                program: script,
                arguments: vec![],
                expected_worker_name: "fake-service-worker".into(),
            },
        );
        assert_eq!(
            service.availability("fake-model").unwrap(),
            ModelAvailability::Installed
        );
        let first = service.run(recipe).unwrap();
        for _ in 0..3 {
            service.poll().unwrap();
        }
        let ModelTaskStatus::Published {
            claim_id,
            cache_hit,
        } = &service.task(first).unwrap().status
        else {
            panic!("fake task should publish");
        };
        assert!(!cache_hit);
        assert!(service.claim(claim_id).is_some());
        let retry = service.retry(first).unwrap();
        let ModelTaskStatus::Published { cache_hit, .. } = &service.task(retry).unwrap().status
        else {
            panic!("retry should restore cache");
        };
        assert!(*cache_hit);
        fs::remove_dir_all(root).unwrap();
    }
}
