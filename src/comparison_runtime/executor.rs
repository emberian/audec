//! Cancellable execution bridge for live reverse-surface comparisons.
//!
//! Capture is deliberately strict: a job is admitted only when the project
//! session, selection request, and the active shared-renderer executable all
//! name the same complete project revision and the same span. The job then
//! resolves the source from the frozen publication and renders constructive
//! DAW explanations through that executable's already-compiled
//! [`DawEngineSchedule`](crate::daw_engine::DawEngineSchedule). It never opens
//! an audio device, creates a transport, fabricates PCM, or compiles a second
//! engine schedule.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::artifact_catalog::{sha256_content, ArtifactCatalog};
use crate::comparison::{ComparisonDefinition, ComparisonObservation};
use crate::comparison_controller::{
    ComparisonAudioEffect, ComparisonAuditionProducts, ComparisonController,
    ComparisonControllerError, ComparisonDigestPins, ComparisonSelectionRequest,
};
use crate::comparison_runtime::{
    ComparisonExecution, ComparisonRuntime, ComparisonRuntimeError, ComparisonRuntimeProgress,
    PcmComparisonSourceResolver,
};
use crate::coverage::{
    CoverageComparisonIdentity, CoverageError, CoverageProductInputs, CoverageRecipe,
};
use crate::daw_project::ProjectRevisions;
use crate::daw_render::RenderCancellation;
use crate::explanation_adapters::{
    CatalogAnalysisResolver, FilteredScheduleIsolationBackend, ResolvingExplanationCompiler,
    ScheduleDawScopeResolver,
};
use crate::interpretation::InterpretationStore;
use crate::project_audio_controller::ProjectAudioController;
use crate::project_session::{ProjectSession, ProjectSessionError};
use crate::render_plan::ExactDigest;
use crate::render_runtime::{project_revision_stamp, AuditionOwner, ExecutableRenderPlan};
use crate::task_coordinator::{
    CanonicalRecipeKey, CompletionOutcome, CompletionReceipt, CompletionRejectionReason,
    CompletionReport, CoordinatorConfig, DiagnosticSeverity, OwnerScope, PaneId, PaneScope,
    ResourceClass, SessionGeneration, SessionId, TaskCoordinator, TaskDiagnostic, TaskDispatch,
    TaskId, TaskInstant, TaskOwner, TaskPriority, TaskProgress, TaskScope, TaskSpec,
};

const PRODUCT_BOUNDARY_DOMAIN: &[u8] = b"audec:comparison-product-boundary:v1";
const COMPARISON_TASK_RECIPE_DOMAIN: &str = "audec.comparison-products.v1";

#[derive(Debug)]
struct ComparisonTasks {
    coordinator: Mutex<TaskCoordinator>,
    clock: AtomicU64,
    flights: Mutex<BTreeMap<crate::task_coordinator::FlightId, Arc<ComparisonSharedFlight>>>,
}

impl ComparisonTasks {
    fn new() -> Self {
        Self {
            coordinator: Mutex::new(
                TaskCoordinator::new(CoordinatorConfig::default())
                    .expect("default task coordinator configuration is valid"),
            ),
            clock: AtomicU64::new(1),
            flights: Mutex::new(BTreeMap::new()),
        }
    }

    fn now(&self) -> TaskInstant {
        TaskInstant(self.clock.fetch_add(1, Ordering::Relaxed))
    }

    fn coordinator(&self) -> std::sync::MutexGuard<'_, TaskCoordinator> {
        self.coordinator
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Clone, Debug)]
enum ComparisonTaskGate {
    Accepted(CompletionReceipt),
    Rejected(CompletionRejectionReason),
}

#[derive(Clone, Debug)]
struct SharedComparisonResult {
    execution: ComparisonExecution,
    products: Arc<ComparisonAuditionProducts>,
    producing_revision: ProjectRevisions,
}

#[derive(Clone, Debug)]
enum SharedComparisonFailure {
    Cancelled,
    Failed(String),
}

#[derive(Clone, Debug)]
struct SharedComparisonOutcome {
    result: Result<SharedComparisonResult, SharedComparisonFailure>,
    gates: BTreeMap<TaskId, ComparisonTaskGate>,
}

#[derive(Debug)]
struct ComparisonSharedFlight {
    outcome: Mutex<Option<SharedComparisonOutcome>>,
    ready: Condvar,
    cancellation: RenderCancellation,
}

impl Default for ComparisonSharedFlight {
    fn default() -> Self {
        Self {
            outcome: Mutex::new(None),
            ready: Condvar::new(),
            cancellation: RenderCancellation::new(),
        }
    }
}

#[derive(Clone, Debug)]
struct ComparisonTaskLease {
    tasks: Arc<ComparisonTasks>,
    task: TaskId,
    dispatch: Option<TaskDispatch>,
    flight: crate::task_coordinator::FlightId,
    shared: Arc<ComparisonSharedFlight>,
}

/// Immutable semantic inputs which may be shared with a background job.
/// Callers replace these Arcs when interpretation or artifact truth changes;
/// an in-flight job continues to see the exact snapshot it captured.
#[derive(Clone)]
pub struct ComparisonSemanticSnapshot {
    pub interpretations: Arc<InterpretationStore>,
    pub artifacts: Arc<ArtifactCatalog>,
}

impl fmt::Debug for ComparisonSemanticSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ComparisonSemanticSnapshot")
            .field("interpretations", &self.interpretations)
            .field("artifacts", &self.artifacts)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ComparisonProductRecipe {
    pub coverage: CoverageRecipe,
    pub boundary_recipe: ExactDigest,
}

impl Default for ComparisonProductRecipe {
    fn default() -> Self {
        Self {
            coverage: CoverageRecipe::default(),
            boundary_recipe: ExactDigest::new(sha256_content(PRODUCT_BOUNDARY_DOMAIN, &[]).bytes),
        }
    }
}

fn comparison_task_session(
    session: crate::project_session::ProjectSessionId,
    owner: AuditionOwner,
) -> SessionId {
    let digest = sha256_content(
        b"audec:comparison-task-session:v1",
        &[
            &session.0.to_le_bytes(),
            &owner.namespace.to_le_bytes(),
            &owner.local.to_le_bytes(),
        ],
    );
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.bytes[..16]);
    SessionId(u128::from_le_bytes(bytes))
}

fn comparison_task_owner(session: SessionId, owner: AuditionOwner) -> OwnerScope {
    OwnerScope {
        owner: TaskOwner((owner.namespace as u64) ^ ((owner.namespace >> 64) as u64) ^ owner.local),
        scope: TaskScope {
            session,
            pane: PaneScope::Pane(PaneId(owner.local)),
        },
    }
}

fn comparison_task_recipe(
    request: &ComparisonSelectionRequest,
    recipe: ComparisonProductRecipe,
) -> Result<CanonicalRecipeKey, ComparisonProductExecutorError> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&request.comparison.0.to_le_bytes());
    bytes.extend_from_slice(&request.explanation.0.to_le_bytes());
    // This generation is semantic selection state, not merely a subscriber
    // counter: it prevents a cancelled old selection from blocking immediate
    // admission of an otherwise identical new selection for the same pane.
    bytes.extend_from_slice(&request.generation.to_le_bytes());
    bytes.extend_from_slice(&request.span.start.to_le_bytes());
    bytes.extend_from_slice(&request.span.end.to_le_bytes());
    for revision in [
        request.requested_at.aggregate,
        request.requested_at.arrangement,
        request.requested_at.sequencer,
        request.requested_at.automation,
        request.requested_at.assets,
        request.requested_at.mixer,
        request.requested_at.sample_kits,
        request.requested_at.air,
        request.requested_at.bindings,
    ] {
        bytes.extend_from_slice(&revision.to_le_bytes());
    }
    for digest in [
        request.digests.source.0,
        request.digests.construction.0,
        request.digests.residual.0,
    ] {
        bytes.push(match digest.algorithm {
            crate::artifact_catalog::DigestAlgorithm::Sha256 => 0,
            crate::artifact_catalog::DigestAlgorithm::Blake3 => 1,
            crate::artifact_catalog::DigestAlgorithm::StableNonCryptographic => 2,
        });
        bytes.extend_from_slice(&digest.bytes);
    }
    bytes.extend_from_slice(&(recipe.coverage.fft_size as u64).to_le_bytes());
    bytes.extend_from_slice(&(recipe.coverage.hop_size as u64).to_le_bytes());
    bytes.extend_from_slice(&recipe.coverage.power_floor.to_bits().to_le_bytes());
    bytes.extend_from_slice(&recipe.boundary_recipe.bytes());
    let digest = sha256_content(b"audec:comparison-task-recipe:v1", &[&bytes]).bytes;
    CanonicalRecipeKey::new(COMPARISON_TASK_RECIPE_DOMAIN, 1, digest)
        .map_err(|error| ComparisonProductExecutorError::Coordination(error.to_string()))
}

fn comparison_task_progress(progress: ComparisonRuntimeProgress) -> TaskProgress {
    let (phase, phase_index, complete) = match progress {
        ComparisonRuntimeProgress::ResolveIntent => ("resolve comparison intent", 0, false),
        ComparisonRuntimeProgress::CompileExplanation => ("compile explanation", 1, false),
        ComparisonRuntimeProgress::ResolveSource => ("resolve cited source", 2, false),
        ComparisonRuntimeProgress::RenderConstruction => ("render construction", 3, false),
        ComparisonRuntimeProgress::Subtract => ("subtract residual", 4, false),
        ComparisonRuntimeProgress::MeasureCoverage => ("measure coverage", 5, false),
        ComparisonRuntimeProgress::Complete => ("comparison complete", 6, true),
    };
    TaskProgress {
        phase: phase.into(),
        phase_index,
        phase_count: 7,
        completed_units: u64::from(complete),
        total_units: 1,
    }
}

#[derive(Clone, Debug)]
struct ActiveRequest {
    generation: u64,
    document_generation: u64,
    revisions: ProjectRevisions,
    cancellation: RenderCancellation,
    task: TaskId,
}

/// Control-thread owner of per-pane cancellation and supersession state.
#[derive(Debug)]
pub struct ComparisonProductExecutor {
    active: BTreeMap<AuditionOwner, ActiveRequest>,
    tasks: Arc<ComparisonTasks>,
}

impl Default for ComparisonProductExecutor {
    fn default() -> Self {
        Self {
            active: BTreeMap::new(),
            tasks: Arc::new(ComparisonTasks::new()),
        }
    }
}

impl ComparisonProductExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Capture a worker-safe job from the one session snapshot and the one
    /// active render executable. Selecting another generation for `owner`
    /// cancels its older job before any potentially failing resolution work.
    pub fn capture(
        &mut self,
        owner: AuditionOwner,
        request: ComparisonSelectionRequest,
        session: &ProjectSession,
        audio: &ProjectAudioController,
        semantics: ComparisonSemanticSnapshot,
        recipe: ComparisonProductRecipe,
    ) -> Result<ComparisonProductJob, ComparisonProductExecutorError> {
        if let Some(active) = self.active.get(&owner) {
            if request.generation <= active.generation {
                return Err(ComparisonProductExecutorError::Superseded {
                    completed: request.generation,
                    desired: Some(active.generation),
                });
            }
        }
        self.cancel_owner(owner);

        let snapshot = session.project_snapshot()?.clone();
        let current = snapshot.revisions();
        require_revision(request.requested_at, current)?;

        let definition = semantics
            .interpretations
            .comparison(request.comparison)
            .cloned()
            .ok_or(ComparisonProductExecutorError::MissingComparison(
                request.comparison,
            ))?;
        validate_request_definition(&request, &definition)?;
        let recorded_observation = semantics
            .interpretations
            .observation(request.comparison)
            .cloned()
            .ok_or(ComparisonProductExecutorError::MissingObservation(
                request.comparison,
            ))?;
        if ComparisonDigestPins::from(&recorded_observation) != request.digests {
            return Err(ComparisonProductExecutorError::ObservationChanged);
        }

        let active_cohort = audio
            .runtime()
            .service()
            .active_cohort()
            .ok_or(ComparisonProductExecutorError::NoActiveRender)?;
        let executable = audio
            .runtime()
            .executable_plan(&active_cohort.id.plan)
            .map_err(|error| ComparisonProductExecutorError::Render(error.to_string()))?;
        let active_revision = executable.schedule.project_revision();
        require_revision(request.requested_at, active_revision).map_err(|_| {
            ComparisonProductExecutorError::ActiveRenderRevisionMismatch {
                requested: request.requested_at,
                active: active_revision,
            }
        })?;
        if executable.id().revisions != project_revision_stamp(request.requested_at) {
            return Err(ComparisonProductExecutorError::ActiveRenderStampMismatch);
        }
        if !executable.descriptor.extent().contains_span(request.span) {
            return Err(ComparisonProductExecutorError::SpanOutsideActiveRender {
                span: request.span,
                active: executable.descriptor.extent(),
            });
        }

        let cancellation = RenderCancellation::new();
        let task_session = comparison_task_session(session.id(), owner);
        let generation = SessionGeneration(request.generation);
        let now = self.tasks.now();
        let mut coordinator = self.tasks.coordinator();
        coordinator
            .observe_session(task_session, generation)
            .map_err(|error| ComparisonProductExecutorError::Coordination(error.to_string()))?;
        let submission = coordinator
            .submit(
                TaskSpec {
                    owner: comparison_task_owner(task_session, owner),
                    generation,
                    recipe: comparison_task_recipe(&request, recipe)?,
                    resource: ResourceClass::Cpu,
                    priority: TaskPriority::Interactive,
                    deadline: None,
                },
                now,
            )
            .map_err(|error| ComparisonProductExecutorError::Admission(error.to_string()))?;
        let (dispatch, shared) = if submission.joined_existing_flight {
            let shared = self
                .tasks
                .flights
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&submission.flight)
                .cloned()
                .ok_or_else(|| {
                    ComparisonProductExecutorError::Coordination(
                        "single-flight subscriber has no shared result cell".into(),
                    )
                })?;
            (None, shared)
        } else {
            let dispatch = coordinator.dispatch_next(now).ok_or_else(|| {
                ComparisonProductExecutorError::Admission(
                    "comparison was admitted but bounded CPU capacity has no dispatch slot".into(),
                )
            })?;
            if dispatch.flight() != submission.flight {
                return Err(ComparisonProductExecutorError::Coordination(
                    "comparison scheduler dispatched a different queued flight".into(),
                ));
            }
            let shared = Arc::new(ComparisonSharedFlight::default());
            self.tasks
                .flights
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(submission.flight, Arc::clone(&shared));
            (Some(dispatch), shared)
        };
        drop(coordinator);
        self.active.insert(
            owner,
            ActiveRequest {
                generation: request.generation,
                document_generation: session.document_generation(),
                revisions: request.requested_at,
                cancellation: cancellation.clone(),
                task: submission.task,
            },
        );
        Ok(ComparisonProductJob {
            owner,
            document_generation: session.document_generation(),
            request,
            definition,
            recorded_observation,
            project: snapshot,
            executable,
            semantics,
            recipe,
            cancellation,
            task: ComparisonTaskLease {
                tasks: Arc::clone(&self.tasks),
                task: submission.task,
                dispatch,
                flight: submission.flight,
                shared,
            },
        })
    }

    /// Cancel a pane's outstanding comparison without touching the shared
    /// project render, transport, or another pane's owner.
    pub fn cancel_owner(&mut self, owner: AuditionOwner) -> bool {
        let Some(active) = self.active.remove(&owner) else {
            return false;
        };
        let _ = self.tasks.coordinator().cancel_task(
            active.task,
            crate::task_coordinator::CancellationReason::Requested,
        );
        active.cancellation.cancel();
        true
    }

    pub fn is_current(&self, owner: AuditionOwner, generation: u64) -> bool {
        self.active
            .get(&owner)
            .is_some_and(|active| active.generation == generation)
    }

    pub fn task_snapshot(
        &self,
        owner: AuditionOwner,
    ) -> Option<crate::task_coordinator::TaskSnapshot> {
        let task = self.active.get(&owner)?.task;
        self.tasks.coordinator().snapshot(task)
    }

    /// Short main-thread publication boundary. It rechecks session revision
    /// and owner/generation immediately before delegating the already
    /// validated products to `ComparisonController`.
    pub fn publish(
        &mut self,
        session: &ProjectSession,
        controller: &mut ComparisonController,
        completion: ComparisonProductCompletion,
    ) -> Result<PublishedComparisonProducts, ComparisonProductExecutorError> {
        if controller.owner() != completion.owner {
            return Err(ComparisonProductExecutorError::OwnerMismatch {
                expected: controller.owner(),
                actual: completion.owner,
            });
        }
        let Some(active) = self.active.get(&completion.owner) else {
            return Err(ComparisonProductExecutorError::Superseded {
                completed: completion.generation,
                desired: None,
            });
        };
        if active.generation != completion.generation {
            return Err(ComparisonProductExecutorError::Superseded {
                completed: completion.generation,
                desired: Some(active.generation),
            });
        }
        if active.cancellation.is_cancelled() || completion.cancellation.is_cancelled() {
            return Err(ComparisonProductExecutorError::Cancelled);
        }
        match &completion.task_gate {
            ComparisonTaskGate::Rejected(reason) => {
                return Err(ComparisonProductExecutorError::Publication(format!(
                    "comparison completion was rejected: {reason:?}"
                )));
            }
            ComparisonTaskGate::Accepted(receipt) => self
                .tasks
                .coordinator()
                .validate_for_publication(receipt, self.tasks.now())
                .map_err(|reason| {
                    ComparisonProductExecutorError::Publication(format!(
                        "comparison receipt lost publication authority: {reason:?}"
                    ))
                })?,
        }
        let current_document = session.document_generation();
        if active.document_generation != current_document
            || completion.document_generation != current_document
        {
            return Err(ComparisonProductExecutorError::DocumentSuperseded {
                captured: completion.document_generation,
                current: current_document,
            });
        }
        let current = session.project_snapshot()?.revisions();
        require_revision(active.revisions, current)?;
        if completion.producing_revision != current {
            return Err(ComparisonProductExecutorError::StaleRevision {
                requested: completion.producing_revision,
                current,
            });
        }
        let effect = controller
            .accept_products(&completion.request, Arc::clone(&completion.products))
            .map_err(ComparisonProductExecutorError::Controller)?;
        self.active.remove(&completion.owner);
        Ok(PublishedComparisonProducts {
            effect,
            execution: completion.execution,
            products: completion.products,
        })
    }
}

/// Frozen background work. The cancellation token is intentionally exposed so
/// a task owner can cancel it without inventing another cancellation domain.
#[derive(Clone)]
pub struct ComparisonProductJob {
    owner: AuditionOwner,
    document_generation: u64,
    request: ComparisonSelectionRequest,
    definition: ComparisonDefinition,
    recorded_observation: ComparisonObservation,
    project: crate::live_project::LiveProjectSnapshot,
    executable: Arc<ExecutableRenderPlan>,
    semantics: ComparisonSemanticSnapshot,
    recipe: ComparisonProductRecipe,
    cancellation: RenderCancellation,
    task: ComparisonTaskLease,
}

impl fmt::Debug for ComparisonProductJob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ComparisonProductJob")
            .field("owner", &self.owner)
            .field("request", &self.request)
            .field("producing_plan", self.executable.id())
            .finish_non_exhaustive()
    }
}

impl ComparisonProductJob {
    pub const fn owner(&self) -> AuditionOwner {
        self.owner
    }

    pub const fn generation(&self) -> u64 {
        self.request.generation
    }

    pub fn request(&self) -> &ComparisonSelectionRequest {
        &self.request
    }

    pub fn cancellation(&self) -> RenderCancellation {
        self.cancellation.clone()
    }

    /// Resolve and render from frozen inputs. DAW-backed explanations use the
    /// active executable schedule; artifact-backed scopes use immutable
    /// content-addressed payloads from the semantic snapshot.
    pub fn execute(&self) -> Result<ComparisonProductCompletion, ComparisonProductExecutorError> {
        if self.task.dispatch.is_none() {
            return self.await_shared_result();
        }
        let result = self.execute_physical();
        let report = match &result {
            Ok(_) => CompletionReport {
                outcome: CompletionOutcome::Succeeded { output: None },
                diagnostics: Vec::new(),
            },
            Err(ComparisonProductExecutorError::Cancelled) => CompletionReport {
                outcome: CompletionOutcome::Cancelled,
                diagnostics: Vec::new(),
            },
            Err(error) => CompletionReport {
                outcome: CompletionOutcome::Failed {
                    code: "comparison-products-failed".into(),
                    detail: error.to_string(),
                },
                diagnostics: vec![TaskDiagnostic {
                    severity: DiagnosticSeverity::Error,
                    code: "comparison-products-failed".into(),
                    detail: error.to_string(),
                }],
            },
        };
        let batch = self
            .task
            .tasks
            .coordinator()
            .complete(self.task.flight, report, self.task.tasks.now())
            .map_err(|error| ComparisonProductExecutorError::Coordination(error.to_string()))?;
        let mut gates = BTreeMap::new();
        for receipt in batch.accepted {
            gates.insert(receipt.task(), ComparisonTaskGate::Accepted(receipt));
        }
        for rejected in batch.rejected {
            gates.insert(
                rejected.receipt.task(),
                ComparisonTaskGate::Rejected(rejected.reason),
            );
        }
        let shared_result = result.as_ref().map(Clone::clone).map_err(|error| {
            if matches!(error, ComparisonProductExecutorError::Cancelled) {
                SharedComparisonFailure::Cancelled
            } else {
                SharedComparisonFailure::Failed(error.to_string())
            }
        });
        {
            let mut outcome = self
                .task
                .shared
                .outcome
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *outcome = Some(SharedComparisonOutcome {
                result: shared_result,
                gates,
            });
            self.task.shared.ready.notify_all();
        }
        self.task
            .tasks
            .flights
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.task.flight);
        match result {
            Ok(shared) => self.completion_from_shared(shared),
            Err(error) => Err(error),
        }
    }

    fn execute_physical(&self) -> Result<SharedComparisonResult, ComparisonProductExecutorError> {
        self.sync_physical_cancellation();
        if self.task.shared.cancellation.is_cancelled() {
            return Err(ComparisonProductExecutorError::Cancelled);
        }
        let daw = ScheduleDawScopeResolver::new(
            &self.project.project,
            Arc::clone(&self.executable.schedule),
            Arc::new(FilteredScheduleIsolationBackend),
        )
        .map_err(|error| ComparisonProductExecutorError::Explanation(error.to_string()))?;
        let analysis = CatalogAnalysisResolver {
            catalog: &self.semantics.artifacts,
        };
        let compiler = ResolvingExplanationCompiler {
            revisions: self.project.revisions(),
            definitions: self.semantics.interpretations.as_ref(),
            daw: &daw,
            analysis: &analysis,
        };
        let source = PcmComparisonSourceResolver {
            assets: &self.project.project.state().domains.assets,
            pcm: &self.project.pcm,
        };
        let runtime = ComparisonRuntime {
            interpretations: &self.semantics.interpretations,
            explanations: &compiler,
            sources: &source,
        };
        let execution = match runtime.execute_with_progress(
            &self.definition,
            self.recipe.coverage,
            &self.task.shared.cancellation,
            |progress| {
                self.sync_physical_cancellation();
                self.report_progress(progress);
            },
        ) {
            Ok(execution) => execution,
            Err(_) if self.task.shared.cancellation.is_cancelled() => {
                return Err(ComparisonProductExecutorError::Cancelled);
            }
            Err(error) => return Err(map_runtime_error(error)),
        };
        if execution.observation != self.recorded_observation {
            return Err(ComparisonProductExecutorError::ObservationChanged);
        }
        self.sync_physical_cancellation();
        if self.task.shared.cancellation.is_cancelled() {
            return Err(ComparisonProductExecutorError::Cancelled);
        }
        let products = execution
            .render_products(self.executable.id(), self.recipe.boundary_recipe)
            .map_err(ComparisonProductExecutorError::Runtime)?;
        let products = Arc::new(
            ComparisonAuditionProducts::validate(
                &self.definition,
                execution.observation.clone(),
                products,
            )
            .map_err(ComparisonProductExecutorError::Controller)?,
        );
        self.sync_physical_cancellation();
        if self.task.shared.cancellation.is_cancelled() {
            return Err(ComparisonProductExecutorError::Cancelled);
        }
        Ok(SharedComparisonResult {
            producing_revision: self.project.revisions(),
            execution,
            products,
        })
    }

    fn await_shared_result(
        &self,
    ) -> Result<ComparisonProductCompletion, ComparisonProductExecutorError> {
        let mut outcome = self
            .task
            .shared
            .outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while outcome.is_none() {
            if self.cancellation.is_cancelled() {
                return Err(ComparisonProductExecutorError::Cancelled);
            }
            let waited = self
                .task
                .shared
                .ready
                .wait_timeout(outcome, Duration::from_millis(20))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            outcome = waited.0;
        }
        let outcome = outcome.as_ref().expect("shared result became ready");
        match &outcome.result {
            Ok(shared) => self.completion_from_shared_with_gates(shared.clone(), &outcome.gates),
            Err(SharedComparisonFailure::Cancelled) => {
                Err(ComparisonProductExecutorError::Cancelled)
            }
            Err(SharedComparisonFailure::Failed(message)) => Err(
                ComparisonProductExecutorError::SharedFlightFailed(message.clone()),
            ),
        }
    }

    fn completion_from_shared(
        &self,
        shared: SharedComparisonResult,
    ) -> Result<ComparisonProductCompletion, ComparisonProductExecutorError> {
        let outcome = self
            .task
            .shared
            .outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.completion_from_shared_with_gates(
            shared,
            &outcome
                .as_ref()
                .expect("runner published shared result")
                .gates,
        )
    }

    fn completion_from_shared_with_gates(
        &self,
        shared: SharedComparisonResult,
        gates: &BTreeMap<TaskId, ComparisonTaskGate>,
    ) -> Result<ComparisonProductCompletion, ComparisonProductExecutorError> {
        let task_gate = gates.get(&self.task.task).cloned().ok_or_else(|| {
            ComparisonProductExecutorError::Coordination(
                "comparison flight produced no logical completion receipt".into(),
            )
        })?;
        Ok(ComparisonProductCompletion {
            owner: self.owner,
            generation: self.request.generation,
            document_generation: self.document_generation,
            producing_revision: shared.producing_revision,
            request: self.request.clone(),
            execution: shared.execution,
            products: shared.products,
            cancellation: self.cancellation.clone(),
            task_gate,
        })
    }

    fn report_progress(&self, progress: ComparisonRuntimeProgress) {
        let _ = self
            .task
            .tasks
            .coordinator()
            .report_progress(self.task.flight, comparison_task_progress(progress));
    }

    fn sync_physical_cancellation(&self) {
        if self
            .task
            .dispatch
            .as_ref()
            .is_some_and(|dispatch| dispatch.cancellation().is_cancelled())
        {
            self.task.shared.cancellation.cancel();
        }
    }
}

#[derive(Clone, Debug)]
pub struct ComparisonProductCompletion {
    pub owner: AuditionOwner,
    pub generation: u64,
    pub document_generation: u64,
    pub producing_revision: ProjectRevisions,
    pub request: ComparisonSelectionRequest,
    pub execution: ComparisonExecution,
    pub products: Arc<ComparisonAuditionProducts>,
    cancellation: RenderCancellation,
    task_gate: ComparisonTaskGate,
}

impl ComparisonProductCompletion {
    pub fn coverage_inputs(&self) -> Result<CoverageProductInputs, CoverageError> {
        coverage_inputs(&self.execution, &self.products)
    }
}

#[derive(Clone, Debug)]
pub struct PublishedComparisonProducts {
    pub effect: ComparisonAudioEffect,
    pub execution: ComparisonExecution,
    pub products: Arc<ComparisonAuditionProducts>,
}

impl PublishedComparisonProducts {
    pub fn coverage_inputs(&self) -> Result<CoverageProductInputs, CoverageError> {
        coverage_inputs(&self.execution, &self.products)
    }
}

fn coverage_inputs(
    execution: &ComparisonExecution,
    products: &ComparisonAuditionProducts,
) -> Result<CoverageProductInputs, CoverageError> {
    let identity = CoverageComparisonIdentity::new(execution.comparison, execution.explanation)?;
    let product = |channel| {
        products
            .product(channel)
            .cloned()
            .ok_or(CoverageError::UnalignedRenderProducts)
    };
    CoverageProductInputs::new(
        identity,
        product(crate::comparison_controller::ComparisonChannel::Source)?,
        product(crate::comparison_controller::ComparisonChannel::Construction)?,
        product(crate::comparison_controller::ComparisonChannel::Residual)?,
    )
}

fn validate_request_definition(
    request: &ComparisonSelectionRequest,
    definition: &ComparisonDefinition,
) -> Result<(), ComparisonProductExecutorError> {
    if definition.id != request.comparison
        || definition.explanation != request.explanation
        || definition.source.project_span.start != request.span.start
        || definition.source.project_span.end != request.span.end
    {
        return Err(ComparisonProductExecutorError::DefinitionChanged);
    }
    definition
        .validate()
        .map_err(|error| ComparisonProductExecutorError::Definition(error.to_string()))
}

fn require_revision(
    requested: ProjectRevisions,
    current: ProjectRevisions,
) -> Result<(), ComparisonProductExecutorError> {
    if requested == current {
        Ok(())
    } else {
        Err(ComparisonProductExecutorError::StaleRevision { requested, current })
    }
}

fn map_runtime_error(error: ComparisonRuntimeError) -> ComparisonProductExecutorError {
    if matches!(
        error,
        ComparisonRuntimeError::Cancelled
            | ComparisonRuntimeError::Explanation(crate::explanation::ExplanationError::Cancelled)
            | ComparisonRuntimeError::Coverage(crate::coverage::CoverageError::Cancelled)
    ) {
        ComparisonProductExecutorError::Cancelled
    } else {
        ComparisonProductExecutorError::Runtime(error)
    }
}

#[derive(Debug)]
pub enum ComparisonProductExecutorError {
    Cancelled,
    Admission(String),
    Coordination(String),
    Publication(String),
    SharedFlightFailed(String),
    NoActiveRender,
    MissingComparison(crate::comparison::ComparisonId),
    MissingObservation(crate::comparison::ComparisonId),
    DefinitionChanged,
    ObservationChanged,
    StaleRevision {
        requested: ProjectRevisions,
        current: ProjectRevisions,
    },
    ActiveRenderRevisionMismatch {
        requested: ProjectRevisions,
        active: ProjectRevisions,
    },
    ActiveRenderStampMismatch,
    SpanOutsideActiveRender {
        span: crate::render_plan::RenderSpan,
        active: crate::render_plan::RenderSpan,
    },
    OwnerMismatch {
        expected: AuditionOwner,
        actual: AuditionOwner,
    },
    Superseded {
        completed: u64,
        desired: Option<u64>,
    },
    DocumentSuperseded {
        captured: u64,
        current: u64,
    },
    Definition(String),
    Explanation(String),
    Render(String),
    Runtime(ComparisonRuntimeError),
    Controller(ComparisonControllerError),
    Session(ProjectSessionError),
}

impl fmt::Display for ComparisonProductExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "comparison product executor: {self:?}")
    }
}

impl Error for ComparisonProductExecutorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Runtime(error) => Some(error),
            Self::Controller(error) => Some(error),
            Self::Session(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ProjectSessionError> for ComparisonProductExecutorError {
    fn from(error: ProjectSessionError) -> Self {
        Self::Session(error)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::aspect::{Aspect, ChannelMask, FrameSpan};
    use crate::assets::{
        AbsolutePath, AssetFrameRange, AssetLocation, AssetOrigin, AssetProvenance,
        AssetRegistration, ContentFingerprint, DecodedAudioMetadata, SampleFrames,
    };
    use crate::audio::AudioFormat;
    use crate::comparison::{ComparisonId, SourceCitation};
    use crate::comparison_controller::ComparisonChannel;
    use crate::daw_engine::DawEngineConfig;
    use crate::daw_render::PcmAsset;
    use crate::explanation::{ExplanationDefinition, ExplanationId, ExplanationScope};
    use crate::interpretation::InterpretationCommand;
    use crate::live_project::{LiveProject, SourceMaterialMetadata};
    use crate::ontology::{Producer, Provenance};
    use crate::project_audio_controller::{
        ProjectAudioControllerEffect, ProjectAudioPlanStamp, ProjectAudioRenderRecipe,
    };
    use crate::project_session::{ProjectPublication, ProjectSessionId};
    use crate::render_plan::{DeterminismGrade, RenderSpan, Tileability};

    struct Fixture {
        session: ProjectSession,
        audio: ProjectAudioController,
        semantics: ComparisonSemanticSnapshot,
        definition: ComparisonDefinition,
        observation: ComparisonObservation,
    }

    fn provenance() -> Provenance {
        Provenance {
            producer: Producer::Human { name: None },
            created_unix_ms: None,
            source_revision: None,
            note: None,
        }
    }

    fn recipe() -> ComparisonProductRecipe {
        ComparisonProductRecipe {
            coverage: CoverageRecipe {
                fft_size: 2,
                hop_size: 1,
                power_floor: 1.0e-12,
            },
            ..ComparisonProductRecipe::default()
        }
    }

    fn fixture() -> Fixture {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/audio/comparison-source.wav").unwrap()),
            None,
        )
        .unwrap();
        let mut registry = crate::assets::AssetRegistry::new();
        let asset = registry
            .register(AssetRegistration {
                name: "comparison source".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: 8_000,
                    channels: 2,
                    frame_count: SampleFrames(4),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32le".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"comparison source"),
                provenance: AssetProvenance::new(
                    1,
                    AssetOrigin::ImportedFile {
                        importer: "test".into(),
                    },
                    location,
                ),
                tags: BTreeSet::new(),
                favorite: false,
            })
            .unwrap();
        let pcm = PcmAsset::new(
            AudioFormat::new(8_000, 2).unwrap(),
            Arc::from([0.25, -0.25, 0.5, -0.5, 0.75, -0.75, 1.0, -1.0]),
        )
        .unwrap();
        let live = LiveProject::from_source_material(
            SourceMaterialMetadata::new("comparison", "source"),
            registry,
            asset,
            pcm,
        )
        .unwrap();
        let source_ids = live.primary_source_ids().unwrap();
        let mut session = ProjectSession::new(ProjectSessionId(91)).unwrap();
        let revisions = session.install(live, None).unwrap();
        let snapshot = session.project_snapshot().unwrap().clone();
        let publication = ProjectPublication {
            generation: session.snapshot().generation,
            revisions,
            snapshot: snapshot.clone(),
            change_set: None,
        };
        let mut audio = ProjectAudioController::new();
        audio.set_tile_policy(None);
        let render_job = audio.request_render(
            publication,
            ProjectAudioRenderRecipe {
                extent: RenderSpan::new(0, 4).unwrap(),
                engine: Arc::new(DawEngineConfig {
                    output_channels: 2,
                    block_frames: 2,
                    ..DawEngineConfig::default()
                }),
                stamp: ProjectAudioPlanStamp {
                    project_namespace: 91,
                    snapshot: ExactDigest::new([1; 32]),
                    engine_abi: 1,
                    engine_configuration: ExactDigest::new([2; 32]),
                    dependencies: Vec::new(),
                    determinism: DeterminismGrade::BitExact,
                    tileability: Tileability::Stateless,
                },
            },
        );
        let completion = render_job.execute(&RenderCancellation::new()).unwrap();
        assert!(matches!(
            audio.complete_render(completion).unwrap(),
            ProjectAudioControllerEffect::OpenHost(_)
        ));

        let explanation = ExplanationDefinition {
            id: ExplanationId(7),
            label: "only source clip".into(),
            scope: ExplanationScope::ArrangementClip(source_ids.clip),
            extent: Aspect::Time(FrameSpan::new(0, 4).unwrap()),
            evidence: Vec::new(),
            provenance: provenance(),
        };
        let definition = ComparisonDefinition {
            id: ComparisonId(8),
            label: "source against construction".into(),
            source: SourceCitation {
                asset,
                source_range: AssetFrameRange::new(SampleFrames(0), SampleFrames(4)).unwrap(),
                project_span: FrameSpan::new(0, 4).unwrap(),
                channels: ChannelMask(0b11),
            },
            explanation: explanation.id,
            provenance: provenance(),
        };
        let mut interpretations = InterpretationStore::new();
        interpretations
            .apply(&[
                InterpretationCommand::PutExplanation {
                    before: None,
                    after: Some(explanation),
                },
                InterpretationCommand::PutComparison {
                    before: None,
                    after: Some(definition.clone()),
                },
            ])
            .unwrap();

        let executable = {
            let cohort = audio.runtime().service().active_cohort().unwrap();
            audio.runtime().executable_plan(&cohort.id.plan).unwrap()
        };
        let artifacts = Arc::new(ArtifactCatalog::new());
        let daw = ScheduleDawScopeResolver::new(
            &snapshot.project,
            Arc::clone(&executable.schedule),
            Arc::new(FilteredScheduleIsolationBackend),
        )
        .unwrap();
        let analysis = CatalogAnalysisResolver {
            catalog: &artifacts,
        };
        let compiler = ResolvingExplanationCompiler {
            revisions,
            definitions: &interpretations,
            daw: &daw,
            analysis: &analysis,
        };
        let source = PcmComparisonSourceResolver {
            assets: &snapshot.project.state().domains.assets,
            pcm: &snapshot.pcm,
        };
        let observation = ComparisonRuntime {
            interpretations: &interpretations,
            explanations: &compiler,
            sources: &source,
        }
        .execute(&definition, recipe().coverage, &RenderCancellation::new())
        .unwrap()
        .observation;
        interpretations
            .apply(&[InterpretationCommand::PutObservation {
                comparison: definition.id,
                before: None,
                after: Some(observation.clone()),
            }])
            .unwrap();
        Fixture {
            session,
            audio,
            semantics: ComparisonSemanticSnapshot {
                interpretations: Arc::new(interpretations),
                artifacts,
            },
            definition,
            observation,
        }
    }

    fn select(
        fixture: &Fixture,
        controller: &mut ComparisonController,
        channel: ComparisonChannel,
    ) -> ComparisonSelectionRequest {
        controller
            .select(
                &fixture.definition,
                &fixture.observation,
                fixture.session.project_snapshot().unwrap().revisions(),
                channel,
            )
            .unwrap()
    }

    #[test]
    fn exact_revision_job_preserves_alignment_and_exact_unfitted_residual() {
        let fixture = fixture();
        let mut controller = ComparisonController::new(12).unwrap();
        let request = select(&fixture, &mut controller, ComparisonChannel::Source);
        let mut executor = ComparisonProductExecutor::new();
        let completion = executor
            .capture(
                controller.owner(),
                request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics.clone(),
                recipe(),
            )
            .unwrap()
            .execute()
            .unwrap();
        let rendered = &completion.execution.rendered;
        assert_eq!(rendered.origin_frame, 0);
        assert_eq!(
            rendered.source.frame_count(),
            rendered.construction.frame_count()
        );
        assert_eq!(rendered.source.format(), rendered.construction.format());
        for ((source, construction), residual) in rendered
            .source
            .interleaved()
            .iter()
            .zip(rendered.construction.interleaved())
            .zip(rendered.residual.interleaved())
        {
            assert_eq!(residual.to_bits(), (source - construction).to_bits());
        }
        assert_eq!(
            completion.execution.coverage.origin_frame,
            rendered.origin_frame
        );
        assert_eq!(
            completion.execution.coverage.frame_count,
            rendered.source.frame_count().0
        );
        assert_eq!(
            completion
                .products
                .product(ComparisonChannel::Source)
                .unwrap()
                .interleaved(),
            rendered.source.interleaved()
        );
        let coverage_inputs = completion.coverage_inputs().unwrap();
        assert_eq!(coverage_inputs.identity.comparison, fixture.definition.id);
        assert_eq!(
            coverage_inputs.pins().source,
            completion
                .products
                .product(ComparisonChannel::Source)
                .unwrap()
                .id
        );

        let published = executor
            .publish(&fixture.session, &mut controller, completion)
            .unwrap();
        let ComparisonAudioEffect::Publish { ref audition, .. } = published.effect else {
            panic!("source is a time-domain publication")
        };
        assert_eq!(
            audition.interleaved(),
            published.execution.rendered.source.interleaved()
        );
        assert_eq!(
            published.coverage_inputs().unwrap().identity.comparison,
            fixture.definition.id
        );
    }

    #[test]
    fn identical_pane_requests_share_one_physical_comparison_flight() {
        let fixture = fixture();
        let mut source_controller = ComparisonController::new(120).unwrap();
        let mut residual_controller = ComparisonController::new(121).unwrap();
        let source_request = select(&fixture, &mut source_controller, ComparisonChannel::Source);
        let residual_request = select(
            &fixture,
            &mut residual_controller,
            ComparisonChannel::Residual,
        );
        assert_eq!(source_request.generation, residual_request.generation);

        let mut executor = ComparisonProductExecutor::new();
        let runner = executor
            .capture(
                source_controller.owner(),
                source_request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics.clone(),
                recipe(),
            )
            .unwrap();
        let follower = executor
            .capture(
                residual_controller.owner(),
                residual_request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics.clone(),
                recipe(),
            )
            .unwrap();
        assert!(runner.task.dispatch.is_some());
        assert!(follower.task.dispatch.is_none());
        assert_eq!(runner.task.flight, follower.task.flight);

        let source = runner.execute().unwrap();
        let residual = follower.execute().unwrap();
        assert!(Arc::ptr_eq(&source.products, &residual.products));
        assert_eq!(source.execution.observation, residual.execution.observation);
        assert!(matches!(
            executor
                .task_snapshot(source_controller.owner())
                .unwrap()
                .state,
            crate::task_coordinator::TaskState::Succeeded
        ));
        assert!(matches!(
            executor
                .task_snapshot(residual_controller.owner())
                .unwrap()
                .state,
            crate::task_coordinator::TaskState::Succeeded
        ));
    }

    #[test]
    fn cancelling_representative_keeps_shared_flight_alive_for_follower() {
        let fixture = fixture();
        let mut first_controller = ComparisonController::new(122).unwrap();
        let mut second_controller = ComparisonController::new(123).unwrap();
        let first_request = select(&fixture, &mut first_controller, ComparisonChannel::Source);
        let second_request = select(
            &fixture,
            &mut second_controller,
            ComparisonChannel::Residual,
        );
        let mut executor = ComparisonProductExecutor::new();
        let runner = executor
            .capture(
                first_controller.owner(),
                first_request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics.clone(),
                recipe(),
            )
            .unwrap();
        let follower = executor
            .capture(
                second_controller.owner(),
                second_request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics.clone(),
                recipe(),
            )
            .unwrap();

        assert!(executor.cancel_owner(first_controller.owner()));
        let cancelled_subscription = runner.execute().unwrap();
        let live_subscription = follower.execute().unwrap();
        assert!(matches!(
            &cancelled_subscription.task_gate,
            ComparisonTaskGate::Rejected(CompletionRejectionReason::Cancelled(_))
        ));
        assert!(matches!(
            &live_subscription.task_gate,
            ComparisonTaskGate::Accepted(_)
        ));
        assert!(Arc::ptr_eq(
            &cancelled_subscription.products,
            &live_subscription.products
        ));
    }

    #[test]
    fn owner_supersession_cancels_work_and_rejects_a_late_completion() {
        let fixture = fixture();
        let mut controller = ComparisonController::new(13).unwrap();
        let mut executor = ComparisonProductExecutor::new();
        let first_request = select(&fixture, &mut controller, ComparisonChannel::Source);
        let first = executor
            .capture(
                controller.owner(),
                first_request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics.clone(),
                recipe(),
            )
            .unwrap();
        let first_completion = first.execute().unwrap();
        let second_request = select(&fixture, &mut controller, ComparisonChannel::Residual);
        let second = executor
            .capture(
                controller.owner(),
                second_request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics.clone(),
                recipe(),
            )
            .unwrap();
        assert!(first.cancellation().is_cancelled());
        assert!(matches!(
            executor.publish(&fixture.session, &mut controller, first_completion),
            Err(ComparisonProductExecutorError::Superseded { .. })
        ));
        assert!(second.execute().is_ok());

        let third_request = select(&fixture, &mut controller, ComparisonChannel::Construction);
        let cancelled = executor
            .capture(
                controller.owner(),
                third_request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics.clone(),
                recipe(),
            )
            .unwrap();
        let fourth_request = select(&fixture, &mut controller, ComparisonChannel::Source);
        let _fourth = executor
            .capture(
                controller.owner(),
                fourth_request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics.clone(),
                recipe(),
            )
            .unwrap();
        assert!(matches!(
            cancelled.execute(),
            Err(ComparisonProductExecutorError::Cancelled)
        ));
    }

    #[test]
    fn stale_capture_is_refused_before_rendering() {
        let fixture = fixture();
        let mut controller = ComparisonController::new(14).unwrap();
        let mut stale = fixture.session.project_snapshot().unwrap().revisions();
        stale.aggregate = stale.aggregate.saturating_sub(1);
        let request = controller
            .select(
                &fixture.definition,
                &fixture.observation,
                stale,
                ComparisonChannel::Source,
            )
            .unwrap();
        let error = ComparisonProductExecutor::new()
            .capture(
                controller.owner(),
                request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics,
                recipe(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ComparisonProductExecutorError::StaleRevision { .. }
        ));
    }

    #[test]
    fn publication_rechecks_the_completion_revision() {
        let fixture = fixture();
        let mut controller = ComparisonController::new(16).unwrap();
        let request = select(&fixture, &mut controller, ComparisonChannel::Residual);
        let mut executor = ComparisonProductExecutor::new();
        let mut completion = executor
            .capture(
                controller.owner(),
                request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics,
                recipe(),
            )
            .unwrap()
            .execute()
            .unwrap();
        completion.producing_revision.aggregate =
            completion.producing_revision.aggregate.saturating_sub(1);
        assert!(matches!(
            executor.publish(&fixture.session, &mut controller, completion),
            Err(ComparisonProductExecutorError::StaleRevision { .. })
        ));
    }

    #[test]
    fn excess_remains_spectral_and_clears_pcm_for_its_owner() {
        let fixture = fixture();
        let mut controller = ComparisonController::new(15).unwrap();
        let request = select(&fixture, &mut controller, ComparisonChannel::Excess);
        let mut executor = ComparisonProductExecutor::new();
        let completion = executor
            .capture(
                controller.owner(),
                request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics.clone(),
                recipe(),
            )
            .unwrap()
            .execute()
            .unwrap();
        assert!(completion
            .products
            .product(ComparisonChannel::Excess)
            .is_none());
        assert_eq!(
            completion.execution.coverage.excess.len(),
            completion.execution.coverage.explained.len()
        );
        let published = executor
            .publish(&fixture.session, &mut controller, completion)
            .unwrap();
        assert!(matches!(
            published.effect,
            ComparisonAudioEffect::Clear { .. }
        ));
    }
}
