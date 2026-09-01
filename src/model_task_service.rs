//! Per-project task service for optional isolated model analysis.
//!
//! The service is deliberately GPUI-neutral. It owns task lifecycle and the
//! project's in-memory claim catalogue, while callers supply a UI action,
//! CLI command, or future command-envelope bridge. A task is not a mixer
//! edit: publication creates immutable evidence material only.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, Instant};

use crate::inference_recipe::InferenceRecipe;
use crate::model_claim::{ModelClaimBundle, ModelClaimId};
use crate::model_registry::{InstallStatus, ModelRegistry, RegistryError};
use crate::model_store::{ModelStore, StoreError, StoredResult};
use crate::model_wire::AnalyzeRequest;
use crate::model_wire::WireParameter;
use crate::worker_runtime::broker::{
    BrokerAction, BrokerCapacity, BrokerConfigError, BrokerTick, CancellationReason,
    CompletionAttempt, CompletionReceipt, ForegroundPressure, JobIdentity, JobPriority, JobTicket,
    ResourceDemand, RuntimePolicy, WorkerBroker,
};
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

/// A live worker completion whose immutable result, claim publication, and
/// broker receipt all survived their respective validators. Model-specific
/// adapters consume this view; the generic task service does not interpret
/// musical meaning or promote evidence into project state.
#[derive(Clone, Copy, Debug)]
pub struct VerifiedModelCompletion<'a> {
    pub task: &'a ModelTask,
    pub claim: &'a ModelClaimBundle,
    pub stored: &'a StoredResult,
    pub receipt: &'a CompletionReceipt,
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

/// Host-side policy for the per-project model queue. Every admitted job still
/// gets its own isolated worker process; this merely defines how many of those
/// processes may coexist and what foreground work they must yield to.
#[derive(Clone, Debug)]
pub struct ModelTaskServiceConfig {
    pub capacity: BrokerCapacity,
    pub runtime: RuntimePolicy,
    pub aging_window: Duration,
    pub maximum_queued_jobs: usize,
}

impl Default for ModelTaskServiceConfig {
    fn default() -> Self {
        let parallelism = std::thread::available_parallelism()
            .map(|value| u16::try_from(value.get()).unwrap_or(u16::MAX))
            .unwrap_or(4)
            .max(4);
        Self {
            capacity: BrokerCapacity {
                cpu_slots: parallelism,
                memory_bytes: 16 * 1024 * 1024 * 1024,
                scratch_bytes: 128 * 1024 * 1024 * 1024,
                worker_slots: parallelism.saturating_sub(2).max(1),
                accelerators: BTreeMap::new(),
                realtime_cpu_reserve: 1,
                realtime_memory_reserve: 512 * 1024 * 1024,
                render_cpu_reserve: 1,
                render_memory_reserve: 512 * 1024 * 1024,
            },
            runtime: RuntimePolicy::default(),
            aging_window: Duration::from_secs(30),
            maximum_queued_jobs: 256,
        }
    }
}

/// One per-project service. `WorkerBroker` is the sole launch authority:
/// submissions remain queued until its fair, resource-aware scheduler emits a
/// `Start`, and terminal worker output is not made visible as a claim until the
/// broker issues an immutable completion receipt.
#[derive(Debug)]
pub struct ModelTaskService {
    registry: ModelRegistry,
    store: ModelStore,
    launch: WorkerLaunch,
    next_id: u64,
    tasks: BTreeMap<ModelTaskId, ModelTask>,
    claims: BTreeMap<ModelClaimId, ModelClaimBundle>,
    inference_recipes: BTreeMap<ModelTaskId, InferenceRecipe>,
    broker: WorkerBroker,
    runtime_policy: RuntimePolicy,
    maximum_queued_jobs: usize,
    pressure: ForegroundPressure,
    clock_origin: Instant,
    identities: BTreeMap<ModelTaskId, JobIdentity>,
    task_by_identity: BTreeMap<JobIdentity, ModelTaskId>,
    active: BTreeMap<JobIdentity, ActiveTask>,
    receipts: BTreeMap<ModelTaskId, CompletionReceipt>,
    completed_results: BTreeMap<ModelTaskId, StoredResult>,
    poll_cursor: Option<JobIdentity>,
}

#[derive(Debug)]
struct ActiveTask {
    id: ModelTaskId,
    runtime: WorkerRuntime,
}

impl ModelTaskService {
    pub fn new(registry: ModelRegistry, store: ModelStore, launch: WorkerLaunch) -> Self {
        Self::with_config(registry, store, launch, ModelTaskServiceConfig::default())
            .expect("default model-task broker configuration is valid")
    }

    pub fn with_config(
        registry: ModelRegistry,
        store: ModelStore,
        launch: WorkerLaunch,
        config: ModelTaskServiceConfig,
    ) -> Result<Self, TaskServiceError> {
        if config.maximum_queued_jobs == 0 {
            return Err(TaskServiceError::BrokerConfig(
                "maximum queued model jobs must be non-zero".into(),
            ));
        }
        let broker = WorkerBroker::new(config.capacity, config.runtime, config.aging_window)?;
        Ok(Self {
            registry,
            store,
            launch,
            next_id: 1,
            tasks: BTreeMap::new(),
            claims: BTreeMap::new(),
            inference_recipes: BTreeMap::new(),
            broker,
            runtime_policy: config.runtime,
            maximum_queued_jobs: config.maximum_queued_jobs,
            pressure: ForegroundPressure::default(),
            clock_origin: Instant::now(),
            identities: BTreeMap::new(),
            task_by_identity: BTreeMap::new(),
            active: BTreeMap::new(),
            receipts: BTreeMap::new(),
            completed_results: BTreeMap::new(),
            poll_cursor: None,
        })
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
        self.run_inner(recipe, None)
    }

    /// Submit an inference whose cache identity, material digest, output
    /// schema, publication vocabulary, and provenance all come from one
    /// validated recipe.
    pub fn run_inference(
        &mut self,
        model_id: impl Into<String>,
        recipe: InferenceRecipe,
    ) -> Result<ModelTaskId, TaskServiceError> {
        let task = recipe.model_task_recipe(model_id);
        self.run_inner(task, Some(recipe))
    }

    fn run_inner(
        &mut self,
        recipe: ModelTaskRecipe,
        inference_recipe: Option<InferenceRecipe>,
    ) -> Result<ModelTaskId, TaskServiceError> {
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
        if let Some(inference_recipe) = inference_recipe {
            self.inference_recipes.insert(id, inference_recipe);
        }

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
        if self.broker.queued_count() >= self.maximum_queued_jobs {
            self.fail(id, TaskDiagnosticKind::Launch, "model task queue is full");
            return Ok(id);
        }
        let manifest = match registration.manifest.canonical_hash() {
            Ok(manifest) => manifest.to_string(),
            Err(error) => {
                self.fail(id, TaskDiagnosticKind::Install, error.to_string());
                return Ok(id);
            }
        };
        let identity = match JobIdentity::new(
            format!("model-task-{}", id.get()),
            1,
            recipe.cache_key.clone(),
            manifest,
        ) {
            Ok(identity) => identity,
            Err(error) => {
                self.fail(id, TaskDiagnosticKind::Protocol, format!("{error:?}"));
                return Ok(id);
            }
        };
        let ticket = JobTicket {
            identity: identity.clone(),
            priority: JobPriority::UserInitiated,
            demand: resource_demand(&registration.manifest, &recipe, self.runtime_policy),
        };
        if let Err(error) = self.broker.submit(ticket, self.now()) {
            self.fail(id, TaskDiagnosticKind::Launch, format!("{error:?}"));
            return Ok(id);
        }
        self.identities.insert(id, identity.clone());
        self.task_by_identity.insert(identity, id);
        self.schedule(self.now());
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
        let inference_recipe = self.inference_recipes.get(&prior).cloned();
        self.run_inner(recipe, inference_recipe)
    }

    pub fn cancel(&mut self, id: ModelTaskId) -> Result<(), TaskServiceError> {
        let identity = self
            .identities
            .get(&id)
            .cloned()
            .ok_or(TaskServiceError::NotRunning(id))?;
        let now = self.now();
        let action = self
            .broker
            .request_cancel(&identity, now, CancellationReason::User)
            .map_err(|error| TaskServiceError::Broker(format!("{error:?}")))?;
        if let Some(action) = action {
            self.apply_broker_action(action);
        } else {
            self.set_status(id, ModelTaskStatus::Cancelled);
            self.schedule(now);
        }
        Ok(())
    }

    /// Updates the foreground reservations and immediately asks analysis jobs
    /// to yield if they would consume capacity reserved for playback/render.
    pub fn set_foreground_pressure(&mut self, pressure: ForegroundPressure) {
        self.pressure = pressure;
        let now = self.now();
        let actions = self.broker.protect_foreground(now, pressure);
        self.apply_broker_actions(actions);
        self.schedule(now);
    }

    pub fn completion_receipt(&self, id: ModelTaskId) -> Option<&CompletionReceipt> {
        self.receipts.get(&id)
    }

    /// Return the exact four-way join required by a model-specific evidence
    /// adapter. Cache restorations intentionally return `None`: they retain a
    /// validated claim but do not invent a broker receipt for an older run.
    pub fn verified_completion(&self, id: ModelTaskId) -> Option<VerifiedModelCompletion<'_>> {
        let task = self.tasks.get(&id)?;
        let ModelTaskStatus::Published {
            claim_id,
            cache_hit: false,
        } = &task.status
        else {
            return None;
        };
        Some(VerifiedModelCompletion {
            task,
            claim: self.claims.get(claim_id)?,
            stored: self.completed_results.get(&id)?,
            receipt: self.receipts.get(&id)?,
        })
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    pub fn queued_count(&self) -> usize {
        self.broker.queued_count()
    }

    /// Block for one event from one admitted worker. Calls rotate across
    /// active jobs; each wait is bounded by the worker runtime policy. The UI
    /// should run this pump off its render thread.
    pub fn poll(&mut self) -> Result<bool, TaskServiceError> {
        let now = self.now();
        let actions = self.broker.tick(now);
        self.apply_broker_actions(actions);
        self.schedule(now);
        let Some(identity) = self.next_poll_identity() else {
            return Ok(self.broker.queued_count() != 0);
        };
        let mut active = self
            .active
            .remove(&identity)
            .expect("poll identity names an active task");
        let id = active.id;
        let event = active.runtime.receive(&self.store);
        match event {
            Ok(Some(event)) => {
                let keep_running = matches!(event, RuntimeEvent::Progress(_));
                self.apply_runtime_event(&identity, id, event);
                if keep_running
                    && matches!(
                        self.tasks.get(&id).map(|task| &task.status),
                        Some(ModelTaskStatus::Running { .. })
                    )
                {
                    self.active.insert(identity, active);
                }
            }
            Ok(None) => {
                self.active.insert(identity, active);
            }
            Err(error) => {
                self.record_diagnostic(id, classify_runtime_error(&error), error.to_string());
                self.active.insert(identity.clone(), active);
                let now = self.now();
                let actions = self.broker.tick(now);
                if actions.is_empty() {
                    if let Ok(Some(action)) = self.broker.request_cancel(
                        &identity,
                        now,
                        CancellationReason::ProgressDeadline,
                    ) {
                        self.apply_broker_action(action);
                    }
                } else {
                    self.apply_broker_actions(actions);
                }
            }
        }
        let now = self.now();
        self.schedule(now);
        Ok(true)
    }

    /// Explicit process escalation for a cancelled/stuck task. No staged
    /// output is published on this path; the cache store retains it for later
    /// inspection/recovery.
    pub fn terminate_active(&mut self, likely_oom: bool) -> Result<(), TaskServiceError> {
        let identities: Vec<_> = self.active.keys().cloned().collect();
        for identity in identities {
            self.kill_identity(&identity, CancellationReason::User, likely_oom);
        }
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
        let claim = if let Some(recipe) = self.inference_recipes.get(&id) {
            recipe.validate_stored(stored)?.claim
        } else {
            ModelClaimBundle::from_worker_result(
                task.recipe.publication.model_manifest_sha256.clone(),
                task.recipe.publication.source.clone(),
                task.recipe.publication.runtime.clone(),
                task.recipe.publication.additivity.clone(),
                stored.result,
                task.recipe.publication.outputs.clone(),
            )?
        };
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

    fn apply_runtime_event(
        &mut self,
        identity: &JobIdentity,
        id: ModelTaskId,
        event: RuntimeEvent,
    ) {
        match event {
            RuntimeEvent::Progress(progress) => {
                let point = crate::worker_runtime::broker::ProgressPoint {
                    phase: result_phase_number(progress.phase),
                    completed: progress.completed_chunks,
                    total: progress.total_chunks,
                };
                match self.broker.observe_progress(identity, self.now(), point) {
                    Ok(()) => {
                        self.set_status(
                            id,
                            ModelTaskStatus::Running {
                                completed_chunks: progress.completed_chunks,
                                total_chunks: progress.total_chunks,
                                phase: progress.phase,
                            },
                        );
                    }
                    Err(error) => {
                        self.fail(id, TaskDiagnosticKind::Protocol, format!("{error:?}"));
                        let _ = self.broker.acknowledge_terminal(identity);
                    }
                }
            }
            RuntimeEvent::ClaimPublished { stored, claim } => {
                self.accept_published(identity, id, stored, Some(claim));
            }
            RuntimeEvent::Published(stored) => {
                self.accept_published(identity, id, stored, None);
            }
            RuntimeEvent::Cancelled { .. } => {
                let _ = self.broker.acknowledge_terminal(identity);
                self.set_status(id, ModelTaskStatus::Cancelled);
            }
            RuntimeEvent::Failed(failure) => {
                let _ = self.broker.acknowledge_terminal(identity);
                self.fail(id, TaskDiagnosticKind::Worker, format!("{failure:?}"));
            }
            RuntimeEvent::JobTerminated { failure, .. } => {
                let _ = self.broker.acknowledge_terminal(identity);
                self.fail(id, TaskDiagnosticKind::Crash, format!("{failure:?}"));
            }
        }
    }

    fn schedule(&mut self, now: BrokerTick) {
        let actions = self.broker.schedule(now, self.pressure);
        self.apply_broker_actions(actions);
    }

    fn apply_broker_actions(&mut self, actions: Vec<BrokerAction>) {
        for action in actions {
            self.apply_broker_action(action);
        }
    }

    fn apply_broker_action(&mut self, action: BrokerAction) {
        match action {
            BrokerAction::Start(ticket) => self.start_ticket(ticket),
            BrokerAction::SendCancel { identity, reason } => {
                let Some(active) = self.active.get_mut(&identity) else {
                    self.reject_broker_state(&identity, "broker requested cancel before launch");
                    return;
                };
                let id = active.id;
                match active.runtime.cancel(identity.job_id().to_owned()) {
                    Ok(()) => self.set_status(id, ModelTaskStatus::Cancelling),
                    Err(error) => {
                        self.record_diagnostic(
                            id,
                            TaskDiagnosticKind::Cancel,
                            format!("could not send broker cancellation ({reason:?}): {error}"),
                        );
                        self.kill_identity(&identity, reason, false);
                    }
                }
            }
            BrokerAction::Kill { identity, reason }
            | BrokerAction::DeclareUnresponsive { identity, reason } => {
                self.kill_identity(&identity, reason, false);
            }
        }
    }

    fn start_ticket(&mut self, ticket: JobTicket) {
        let identity = ticket.identity;
        let Some(id) = self.task_by_identity.get(&identity).copied() else {
            let _ = self.broker.acknowledge_terminal(&identity);
            return;
        };
        let Some(task) = self.tasks.get(&id) else {
            let _ = self.broker.acknowledge_terminal(&identity);
            return;
        };
        let recipe = task.recipe.clone();
        let Some(registration) = self.registry.get(&recipe.model_id).cloned() else {
            self.reject_start(
                &identity,
                id,
                TaskDiagnosticKind::Install,
                "model registration disappeared while queued".into(),
            );
            return;
        };
        let mut runtime = match WorkerRuntime::launch_with_policy(
            &self.store,
            self.launch.clone(),
            self.runtime_policy,
        ) {
            Ok(runtime) => runtime,
            Err(error) => {
                self.reject_start(&identity, id, TaskDiagnosticKind::Launch, error.to_string());
                return;
            }
        };
        if let Err(error) = runtime.load_registered(&self.registry, &registration) {
            self.reject_start(
                &identity,
                id,
                TaskDiagnosticKind::Install,
                error.to_string(),
            );
            return;
        }
        let sandbox = match runtime.reserve(&self.store, identity.job_id(), identity.cache_key()) {
            Ok(RuntimeReservation::CacheHit(stored)) => {
                let _ = self.broker.acknowledge_terminal(&identity);
                if let Err(error) = self.restore_cached(id, stored) {
                    self.fail(id, TaskDiagnosticKind::Claim, error.to_string());
                }
                return;
            }
            Ok(RuntimeReservation::Busy) => {
                self.reject_start(
                    &identity,
                    id,
                    TaskDiagnosticKind::Cache,
                    "cache key is already being computed".into(),
                );
                return;
            }
            Ok(RuntimeReservation::Reserved(sandbox)) => sandbox,
            Err(error) => {
                self.reject_start(&identity, id, TaskDiagnosticKind::Cache, error.to_string());
                return;
            }
        };
        let material = match WorkerRuntime::write_input_bytes(
            &sandbox,
            "material.pcm",
            &recipe.material.bytes,
        ) {
            Ok(material) => material,
            Err(error) => {
                self.reject_start(&identity, id, TaskDiagnosticKind::Cache, error.to_string());
                return;
            }
        };
        let files = match WorkerRuntime::job_files(&sandbox, material) {
            Ok(files) => files,
            Err(error) => {
                self.reject_start(&identity, id, TaskDiagnosticKind::Cache, error.to_string());
                return;
            }
        };
        let request = AnalyzeRequest {
            job_id: identity.job_id().to_owned(),
            model_manifest_sha256: identity.manifest_sha256().to_owned(),
            cache_key: identity.cache_key().to_owned(),
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
        if let Err(error) = runtime.analyze_claim(request, recipe.publication) {
            self.reject_start(
                &identity,
                id,
                TaskDiagnosticKind::Protocol,
                error.to_string(),
            );
            return;
        }
        if let Err(error) = self.broker.observe_started(&identity, self.now()) {
            self.reject_start(
                &identity,
                id,
                TaskDiagnosticKind::Protocol,
                format!("broker refused worker start: {error:?}"),
            );
            return;
        }
        self.set_status(
            id,
            ModelTaskStatus::Running {
                completed_chunks: 0,
                total_chunks: 0,
                phase: crate::model_wire::ResultPhase::Preparing,
            },
        );
        self.active.insert(identity, ActiveTask { id, runtime });
    }

    fn accept_published(
        &mut self,
        identity: &JobIdentity,
        id: ModelTaskId,
        stored: StoredResult,
        claim: Option<ModelClaimBundle>,
    ) {
        if stored.result.job_id != identity.job_id()
            || stored.result.cache_key != identity.cache_key()
        {
            let _ = self.broker.acknowledge_terminal(identity);
            self.fail(
                id,
                TaskDiagnosticKind::Protocol,
                "worker completion identity differs from broker admission",
            );
            return;
        }
        let output = match self.runtime_policy.output.validate_result(&stored.result) {
            Ok(output) => output,
            Err(error) => {
                let _ = self.broker.acknowledge_terminal(identity);
                self.fail(id, TaskDiagnosticKind::Protocol, error.to_string());
                return;
            }
        };
        let encoded = match serde_json::to_vec(&stored.result) {
            Ok(encoded) => encoded,
            Err(error) => {
                let _ = self.broker.acknowledge_terminal(identity);
                self.fail(id, TaskDiagnosticKind::Protocol, error.to_string());
                return;
            }
        };
        let attempt = CompletionAttempt {
            identity: identity.clone(),
            result_sha256: crate::model_worker::sha256_bytes(&encoded).to_string(),
            output,
        };
        let receipt = match self.broker.accept_completion(attempt, self.now()) {
            Ok(receipt) => receipt,
            Err(error) => {
                self.fail(
                    id,
                    TaskDiagnosticKind::Protocol,
                    format!("broker refused stale or invalid completion: {error:?}"),
                );
                return;
            }
        };
        let stored_for_evidence = stored.clone();
        let claim = if let Some(recipe) = self.inference_recipes.get(&id) {
            match recipe.validate_stored(stored) {
                Ok(bundle) => bundle.claim,
                Err(error) => {
                    self.fail(id, TaskDiagnosticKind::Claim, error.to_string());
                    return;
                }
            }
        } else if let Some(claim) = claim {
            claim
        } else {
            self.fail(
                id,
                TaskDiagnosticKind::Claim,
                "worker result had no claim publication recipe",
            );
            return;
        };
        let claim_id = claim.id.clone();
        self.completed_results.insert(id, stored_for_evidence);
        self.receipts.insert(id, receipt);
        self.claims.insert(claim_id.clone(), claim);
        self.set_status(
            id,
            ModelTaskStatus::Published {
                claim_id,
                cache_hit: false,
            },
        );
    }

    fn reject_start(
        &mut self,
        identity: &JobIdentity,
        id: ModelTaskId,
        kind: TaskDiagnosticKind,
        detail: String,
    ) {
        let _ = self.broker.acknowledge_terminal(identity);
        self.fail(id, kind, detail);
    }

    fn reject_broker_state(&mut self, identity: &JobIdentity, detail: &str) {
        if let Some(id) = self.task_by_identity.get(identity).copied() {
            let _ = self.broker.acknowledge_terminal(identity);
            self.fail(id, TaskDiagnosticKind::Protocol, detail);
        }
    }

    fn kill_identity(
        &mut self,
        identity: &JobIdentity,
        reason: CancellationReason,
        likely_oom: bool,
    ) {
        let Some(mut active) = self.active.remove(identity) else {
            self.reject_broker_state(identity, "broker requested kill before launch");
            return;
        };
        let id = active.id;
        let termination = active.runtime.terminate(likely_oom);
        let _ = self.broker.acknowledge_terminal(identity);
        match reason {
            CancellationReason::User | CancellationReason::Superseded => {
                self.set_status(id, ModelTaskStatus::Cancelled);
            }
            _ => {
                let detail = termination
                    .err()
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| format!("worker terminated after {reason:?}"));
                self.fail(id, TaskDiagnosticKind::Crash, detail);
            }
        }
    }

    fn next_poll_identity(&mut self) -> Option<JobIdentity> {
        let next = self
            .poll_cursor
            .as_ref()
            .and_then(|cursor| {
                self.active
                    .keys()
                    .find(|identity| *identity > cursor)
                    .cloned()
            })
            .or_else(|| self.active.keys().next().cloned());
        self.poll_cursor = next.clone();
        next
    }

    fn now(&self) -> BrokerTick {
        BrokerTick::from_millis(
            u64::try_from(self.clock_origin.elapsed().as_millis()).unwrap_or(u64::MAX),
        )
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

    fn record_diagnostic(
        &mut self,
        id: ModelTaskId,
        kind: TaskDiagnosticKind,
        detail: impl Into<String>,
    ) {
        if let Some(task) = self.tasks.get_mut(&id) {
            task.diagnostics.push(TaskDiagnostic {
                kind,
                detail: detail.into(),
            });
        }
    }
}

fn resource_demand(
    manifest: &crate::model_worker::ModelManifest,
    recipe: &ModelTaskRecipe,
    policy: RuntimePolicy,
) -> ResourceDemand {
    let input_bytes = u64::try_from(recipe.material.bytes.len()).unwrap_or(u64::MAX);
    let output_streams = u64::try_from(manifest.output.names.len())
        .unwrap_or(u64::MAX)
        .max(1);
    // Audio-like workers usually produce one input-sized stream per declared
    // output. The floor leaves room for small metadata/event results without
    // pretending to be an OS-enforced disk quota.
    let expected_output_bytes = input_bytes
        .saturating_mul(output_streams)
        .max(1024 * 1024)
        .min(policy.output.maximum_total_output_bytes);
    ResourceDemand {
        cpu_slots: 1,
        memory_bytes: manifest.execution.estimated_peak_memory_bytes.max(1),
        scratch_bytes: input_bytes.saturating_add(expected_output_bytes).max(1),
        expected_output_bytes,
        accelerator: manifest.execution.required_accelerators.first().cloned(),
    }
}

const fn result_phase_number(phase: crate::model_wire::ResultPhase) -> u16 {
    match phase {
        crate::model_wire::ResultPhase::Preparing => 0,
        crate::model_wire::ResultPhase::Decoding => 1,
        crate::model_wire::ResultPhase::Analyzing => 2,
        crate::model_wire::ResultPhase::Encoding => 3,
        crate::model_wire::ResultPhase::Verifying => 4,
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
    Recipe(crate::inference_recipe::RecipeError),
    Manifest(crate::model_worker::ValidationError),
    BrokerConfig(String),
    Broker(String),
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
            Self::Recipe(error) => error.fmt(f),
            Self::Manifest(error) => error.fmt(f),
            Self::BrokerConfig(detail) | Self::Broker(detail) => f.write_str(detail),
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
impl From<crate::inference_recipe::RecipeError> for TaskServiceError {
    fn from(value: crate::inference_recipe::RecipeError) -> Self {
        Self::Recipe(value)
    }
}
impl From<crate::model_worker::ValidationError> for TaskServiceError {
    fn from(value: crate::model_worker::ValidationError) -> Self {
        Self::Manifest(value)
    }
}
impl From<BrokerConfigError> for TaskServiceError {
    fn from(value: BrokerConfigError) -> Self {
        Self::BrokerConfig(value.to_string())
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
        Normalization, NumericPrecision, OutputAdditivity, OutputContract, PROTOCOL_VERSION,
        Redistribution, SampleEncoding, TrainingProvenance,
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

    fn hung_worker(root: &Path) -> std::path::PathBuf {
        let script = root.join("hung-worker.sh");
        fs::write(
            &script,
            r#"#!/bin/sh
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
      # Deliberately never emits progress or a terminal record.
      ;;
    *'"kind":"cancel"'*)
      # Deliberately ignores cooperative cancellation.
      ;;
  esac
done
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        script
    }

    fn bounded_config() -> ModelTaskServiceConfig {
        ModelTaskServiceConfig {
            capacity: BrokerCapacity {
                cpu_slots: 4,
                memory_bytes: 8 * 1024 * 1024,
                scratch_bytes: 16 * 1024 * 1024,
                worker_slots: 4,
                accelerators: BTreeMap::new(),
                realtime_cpu_reserve: 1,
                realtime_memory_reserve: 1024 * 1024,
                render_cpu_reserve: 1,
                render_memory_reserve: 1024 * 1024,
            },
            runtime: RuntimePolicy {
                deadlines: crate::worker_runtime::broker::WorkerDeadlines {
                    startup: Duration::from_secs(1),
                    request: Duration::from_secs(1),
                    heartbeat: Duration::from_millis(30),
                    progress: Duration::from_millis(30),
                    cancel_grace: Duration::from_millis(20),
                    kill_grace: Duration::from_millis(20),
                },
                output: crate::worker_runtime::broker::WorkerOutputPolicy::default(),
            },
            aging_window: Duration::from_millis(100),
            maximum_queued_jobs: 8,
        }
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
        assert!(service.completion_receipt(first).is_some());
        let verified = service
            .verified_completion(first)
            .expect("live publication retains its receipt/result/claim join");
        assert_eq!(verified.claim.id.as_str(), claim_id.as_str());
        assert_eq!(
            verified.stored.result.job_id,
            verified.receipt.identity().job_id()
        );
        let retry = service.retry(first).unwrap();
        let ModelTaskStatus::Published { cache_hit, .. } = &service.task(retry).unwrap().status
        else {
            panic!("retry should restore cache");
        };
        assert!(*cache_hit);
        assert!(service.verified_completion(retry).is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn broker_keeps_ml_workers_out_of_realtime_and_render_reservations() {
        let root = temp_root();
        let registry = registry(&root);
        let manifest = registry
            .get("fake-model")
            .unwrap()
            .manifest
            .canonical_hash()
            .unwrap()
            .to_string();
        let script = hung_worker(&root);
        let mut service = ModelTaskService::with_config(
            registry,
            ModelStore::new(root.join("store")),
            WorkerLaunch {
                program: script,
                arguments: vec![],
                expected_worker_name: "fake-service-worker".into(),
            },
            bounded_config(),
        )
        .unwrap();
        service.set_foreground_pressure(ForegroundPressure {
            realtime_audio_active: true,
            render_work_pending: true,
        });
        let mut ids = Vec::new();
        for byte in ['a', 'b', 'c'] {
            let mut recipe = recipe();
            recipe.cache_key = std::iter::repeat_n(byte, 64).collect();
            recipe.publication.model_manifest_sha256 = manifest.clone();
            ids.push(service.run(recipe).unwrap());
        }
        assert_eq!(service.active_count(), 2);
        assert_eq!(service.queued_count(), 1);
        assert_eq!(
            ids.iter()
                .filter(|id| matches!(
                    service.task(**id).unwrap().status,
                    ModelTaskStatus::Running { .. }
                ))
                .count(),
            2
        );
        assert!(matches!(
            service.task(ids[2]).unwrap().status,
            ModelTaskStatus::Queued
        ));
        service.terminate_active(false).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hung_worker_is_cancelled_then_killed_within_bounded_time() {
        let root = temp_root();
        let registry = registry(&root);
        let manifest = registry
            .get("fake-model")
            .unwrap()
            .manifest
            .canonical_hash()
            .unwrap()
            .to_string();
        let script = hung_worker(&root);
        let mut service = ModelTaskService::with_config(
            registry,
            ModelStore::new(root.join("store")),
            WorkerLaunch {
                program: script,
                arguments: vec![],
                expected_worker_name: "fake-service-worker".into(),
            },
            bounded_config(),
        )
        .unwrap();
        let mut recipe = recipe();
        recipe.publication.model_manifest_sha256 = manifest;
        let id = service.run(recipe).unwrap();
        let started = Instant::now();
        assert!(service.poll().unwrap());
        assert!(matches!(
            service.task(id).unwrap().status,
            ModelTaskStatus::Cancelling
        ));
        assert!(service.poll().unwrap());
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(matches!(
            service.task(id).unwrap().status,
            ModelTaskStatus::Failed
        ));
        assert_eq!(service.active_count(), 0);
        fs::remove_dir_all(root).unwrap();
    }
}
