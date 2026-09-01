//! UI-neutral admission and publication control for background work.
//!
//! This module owns no threads, async runtime, GPUI entities, worker
//! processes, or project mutations. It admits bounded logical tasks, folds
//! identical canonical recipes into one physical flight, and hands an
//! immutable [`TaskDispatch`] to an executor selected by the application.
//! Execution remains cooperative: workers must observe [`TaskCancellation`]
//! and report progress/completion back to this control-thread-owned state.
//!
//! A successful computation is not publication authority. Every logical
//! subscriber retains its owner, pane scope, and captured session generation;
//! [`TaskCoordinator::complete`] rejects receipts whose generation is no
//! longer current. This is intentionally stricter than returning a payload to
//! whichever pane happens to exist when a worker finishes.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId(u64);

impl TaskId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FlightId(u64);

impl FlightId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(pub u128);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionGeneration(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskOwner(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PaneScope {
    /// Work belongs to the project session rather than one presentation.
    Session,
    Pane(PaneId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskScope {
    pub session: SessionId,
    pub pane: PaneScope,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OwnerScope {
    pub owner: TaskOwner,
    pub scope: TaskScope,
}

/// Caller-supplied monotonic time in one application-defined tick unit.
///
/// Wall-clock timestamps are deliberately excluded so admission tests and
/// journal replays remain deterministic. The app normally converts one
/// process-local monotonic clock into these ticks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskInstant(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskDeadline(pub TaskInstant);

/// Exact identity of a pure computation.
///
/// The coordinator does not hash ad-hoc structures. An adapter must first
/// canonicalize every effective input (source identity/span, model/runtime,
/// render generations, parameters, random seed, post-processing, and output
/// schema as applicable), then supply its digest under a versioned domain.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalRecipeKey {
    domain: String,
    schema_revision: u32,
    digest: [u8; 32],
}

impl CanonicalRecipeKey {
    pub fn new(
        domain: impl Into<String>,
        schema_revision: u32,
        digest: [u8; 32],
    ) -> Result<Self, RecipeKeyError> {
        let domain = domain.into();
        if domain.trim().is_empty() {
            return Err(RecipeKeyError::EmptyDomain);
        }
        if schema_revision == 0 {
            return Err(RecipeKeyError::ZeroSchemaRevision);
        }
        Ok(Self {
            domain,
            schema_revision,
            digest,
        })
    }

    pub fn domain(&self) -> &str {
        &self.domain
    }

    pub const fn schema_revision(&self) -> u32 {
        self.schema_revision
    }

    pub const fn digest(&self) -> [u8; 32] {
        self.digest
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecipeKeyError {
    EmptyDomain,
    ZeroSchemaRevision,
}

impl fmt::Display for RecipeKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyDomain => write!(f, "canonical recipe domain is empty"),
            Self::ZeroSchemaRevision => {
                write!(f, "canonical recipe schema revision must be non-zero")
            }
        }
    }
}

impl std::error::Error for RecipeKeyError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceClass {
    Control,
    Io,
    Cpu,
    Render,
    ModelWorker,
    PluginWorker,
    Gpu,
    Custom(u16),
}

/// Scheduling urgency. This is not a realtime-thread priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TaskPriority {
    Maintenance,
    Background,
    Foreground,
    Interactive,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSpec {
    pub owner: OwnerScope,
    pub generation: SessionGeneration,
    pub recipe: CanonicalRecipeKey,
    pub resource: ResourceClass,
    pub priority: TaskPriority,
    pub deadline: Option<TaskDeadline>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoordinatorConfig {
    pub maximum_active_tasks: usize,
    pub maximum_queued_flights: usize,
    pub maximum_subscribers_per_flight: usize,
    pub maximum_diagnostics_per_task: usize,
    pub maximum_terminal_snapshots: usize,
    pub resource_limits: BTreeMap<ResourceClass, usize>,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        let mut resource_limits = BTreeMap::new();
        resource_limits.insert(ResourceClass::Control, 1);
        resource_limits.insert(ResourceClass::Io, 2);
        resource_limits.insert(ResourceClass::Cpu, 2);
        resource_limits.insert(ResourceClass::Render, 2);
        resource_limits.insert(ResourceClass::ModelWorker, 1);
        resource_limits.insert(ResourceClass::PluginWorker, 1);
        resource_limits.insert(ResourceClass::Gpu, 1);
        Self {
            maximum_active_tasks: 256,
            maximum_queued_flights: 128,
            maximum_subscribers_per_flight: 32,
            maximum_diagnostics_per_task: 64,
            maximum_terminal_snapshots: 256,
            resource_limits,
        }
    }
}

impl CoordinatorConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.maximum_active_tasks == 0 {
            return Err(ConfigError::ZeroLimit("maximum_active_tasks"));
        }
        if self.maximum_queued_flights == 0 {
            return Err(ConfigError::ZeroLimit("maximum_queued_flights"));
        }
        if self.maximum_subscribers_per_flight == 0 {
            return Err(ConfigError::ZeroLimit("maximum_subscribers_per_flight"));
        }
        if self.maximum_diagnostics_per_task == 0 {
            return Err(ConfigError::ZeroLimit("maximum_diagnostics_per_task"));
        }
        if self.maximum_terminal_snapshots == 0 {
            return Err(ConfigError::ZeroLimit("maximum_terminal_snapshots"));
        }
        if self.resource_limits.values().any(|limit| *limit == 0) {
            return Err(ConfigError::ZeroResourceLimit);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    ZeroLimit(&'static str),
    ZeroResourceLimit,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroLimit(name) => write!(f, "coordinator limit {name} must be non-zero"),
            Self::ZeroResourceLimit => write!(f, "resource limits must be non-zero"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Clone, Debug)]
pub struct TaskCancellation {
    cancelled: Arc<AtomicBool>,
}

impl TaskCancellation {
    fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn check(&self) -> Result<(), TaskCancelled> {
        if self.is_cancelled() {
            Err(TaskCancelled)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskCancelled;

impl fmt::Display for TaskCancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "task was cancelled")
    }
}

impl std::error::Error for TaskCancelled {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskProgress {
    pub phase: String,
    pub phase_index: u16,
    pub phase_count: u16,
    pub completed_units: u64,
    pub total_units: u64,
}

impl TaskProgress {
    pub fn validate(&self) -> Result<(), ProgressError> {
        if self.phase.trim().is_empty() {
            return Err(ProgressError::EmptyPhase);
        }
        if self.phase_count == 0 || self.phase_index >= self.phase_count {
            return Err(ProgressError::InvalidPhaseIndex);
        }
        if self.total_units == 0 || self.completed_units > self.total_units {
            return Err(ProgressError::InvalidUnits);
        }
        Ok(())
    }

    pub fn parts_per_million(&self) -> u32 {
        let phase_fraction =
            (u128::from(self.completed_units) * 1_000_000) / u128::from(self.total_units);
        ((u128::from(self.phase_index) * 1_000_000 + phase_fraction) / u128::from(self.phase_count))
            as u32
    }

    fn follows(&self, previous: &Self) -> bool {
        if self.phase_index > previous.phase_index {
            return true;
        }
        self.phase_index == previous.phase_index
            && self.phase == previous.phase
            && self.phase_count == previous.phase_count
            && self.total_units == previous.total_units
            && self.completed_units >= previous.completed_units
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressError {
    EmptyPhase,
    InvalidPhaseIndex,
    InvalidUnits,
    Regression,
}

impl fmt::Display for ProgressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid task progress: {self:?}")
    }
}

impl std::error::Error for ProgressError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskDiagnostic {
    pub severity: DiagnosticSeverity,
    pub code: String,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskState {
    Queued,
    Running,
    Cancelling(CancellationReason),
    Succeeded,
    Failed,
    Cancelled,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancellationReason {
    Requested,
    Superseded,
    SessionAdvanced,
    DeadlineExceeded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSnapshot {
    pub id: TaskId,
    pub flight: FlightId,
    pub spec: TaskSpec,
    pub state: TaskState,
    pub progress: Option<TaskProgress>,
    pub diagnostics: Vec<TaskDiagnostic>,
    pub dropped_diagnostics: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Submission {
    pub task: TaskId,
    pub flight: FlightId,
    pub joined_existing_flight: bool,
}

/// Frozen handoff to an application-selected executor.
///
/// Receiving this value does not imply that a thread was created. The
/// executor must call `complete` exactly once (including failure/cancellation)
/// and should report progress while the flight remains current.
#[derive(Clone, Debug)]
pub struct TaskDispatch {
    flight: FlightId,
    representative_task: TaskId,
    recipe: CanonicalRecipeKey,
    resource: ResourceClass,
    cancellation: TaskCancellation,
}

impl TaskDispatch {
    pub const fn flight(&self) -> FlightId {
        self.flight
    }

    pub const fn representative_task(&self) -> TaskId {
        self.representative_task
    }

    pub fn recipe(&self) -> &CanonicalRecipeKey {
        &self.recipe
    }

    pub const fn resource(&self) -> ResourceClass {
        self.resource
    }

    pub fn cancellation(&self) -> TaskCancellation {
        self.cancellation.clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompletionOutcome {
    Succeeded {
        /// Optional content identity of the immutable result. Publication
        /// adapters may require this even though the generic coordinator does
        /// not: some bounded control tasks legitimately have no artifact.
        output: Option<CanonicalRecipeKey>,
    },
    Failed {
        code: String,
        detail: String,
    },
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionReport {
    pub outcome: CompletionOutcome,
    pub diagnostics: Vec<TaskDiagnostic>,
}

/// Immutable evidence that one executor finished one canonical flight for a
/// particular logical subscriber.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionReceipt {
    task: TaskId,
    flight: FlightId,
    owner: OwnerScope,
    captured_generation: SessionGeneration,
    recipe: CanonicalRecipeKey,
    resource: ResourceClass,
    deadline: Option<TaskDeadline>,
    completed_at: TaskInstant,
    outcome: CompletionOutcome,
}

impl CompletionReceipt {
    pub const fn task(&self) -> TaskId {
        self.task
    }

    pub const fn flight(&self) -> FlightId {
        self.flight
    }

    pub const fn owner(&self) -> OwnerScope {
        self.owner
    }

    pub const fn captured_generation(&self) -> SessionGeneration {
        self.captured_generation
    }

    pub fn recipe(&self) -> &CanonicalRecipeKey {
        &self.recipe
    }

    pub const fn resource(&self) -> ResourceClass {
        self.resource
    }

    pub const fn deadline(&self) -> Option<TaskDeadline> {
        self.deadline
    }

    pub const fn completed_at(&self) -> TaskInstant {
        self.completed_at
    }

    pub fn outcome(&self) -> &CompletionOutcome {
        &self.outcome
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionRejectionReason {
    Cancelled(CancellationReason),
    StaleGeneration {
        captured: SessionGeneration,
        current: SessionGeneration,
    },
    SessionClosed,
    DeadlineExceeded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectedCompletion {
    pub receipt: CompletionReceipt,
    pub reason: CompletionRejectionReason,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompletionBatch {
    /// Receipts which passed the owner/session-generation/deadline gate.
    /// The application may now interpret their outcome or publish an artifact.
    pub accepted: Vec<CompletionReceipt>,
    /// Late work remains observable but has no publication authority.
    pub rejected: Vec<RejectedCompletion>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionError {
    UnknownSession(SessionId),
    StaleSubmission {
        submitted: SessionGeneration,
        current: SessionGeneration,
    },
    ActiveTaskLimit,
    QueuedFlightLimit,
    SubscriberLimit(FlightId),
    ResourceUnavailable(ResourceClass),
    FlightCancelling(FlightId),
    DeadlineAlreadyElapsed,
}

impl fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "task admission refused: {self:?}")
    }
}

impl std::error::Error for AdmissionError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoordinatorError {
    UnknownTask(TaskId),
    UnknownFlight(FlightId),
    FlightNotRunning(FlightId),
    Progress(ProgressError),
    SessionGenerationRegression {
        session: SessionId,
        previous: SessionGeneration,
        proposed: SessionGeneration,
    },
}

impl fmt::Display for CoordinatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "task coordinator error: {self:?}")
    }
}

impl std::error::Error for CoordinatorError {}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SingleFlightKey {
    resource: ResourceClass,
    recipe: CanonicalRecipeKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FlightState {
    Queued,
    Running,
}

#[derive(Debug)]
struct FlightRecord {
    id: FlightId,
    key: SingleFlightKey,
    state: FlightState,
    submitted_at: TaskInstant,
    subscribers: Vec<TaskId>,
    cancellation: TaskCancellation,
}

#[derive(Debug)]
struct TaskRecord {
    snapshot: TaskSnapshot,
}

impl TaskRecord {
    fn is_eligible(&self) -> bool {
        !matches!(self.snapshot.state, TaskState::Cancelling(_))
    }
}

/// Control-thread-owned bounded task state. It is not an executor and contains
/// no internal scheduler thread or synchronization policy. Worker threads
/// receive only immutable dispatch data and a cloneable cancellation flag.
#[derive(Debug)]
pub struct TaskCoordinator {
    config: CoordinatorConfig,
    next_task: u64,
    next_flight: u64,
    sessions: BTreeMap<SessionId, SessionGeneration>,
    tasks: BTreeMap<TaskId, TaskRecord>,
    flights: BTreeMap<FlightId, FlightRecord>,
    single_flights: BTreeMap<SingleFlightKey, FlightId>,
    running_by_resource: BTreeMap<ResourceClass, usize>,
    terminal: VecDeque<TaskSnapshot>,
}

impl TaskCoordinator {
    pub fn new(config: CoordinatorConfig) -> Result<Self, ConfigError> {
        config.validate()?;
        Ok(Self {
            config,
            next_task: 1,
            next_flight: 1,
            sessions: BTreeMap::new(),
            tasks: BTreeMap::new(),
            flights: BTreeMap::new(),
            single_flights: BTreeMap::new(),
            running_by_resource: BTreeMap::new(),
            terminal: VecDeque::new(),
        })
    }

    pub fn observe_session(
        &mut self,
        session: SessionId,
        generation: SessionGeneration,
    ) -> Result<(), CoordinatorError> {
        if let Some(previous) = self.sessions.get(&session).copied() {
            if generation < previous {
                return Err(CoordinatorError::SessionGenerationRegression {
                    session,
                    previous,
                    proposed: generation,
                });
            }
            if generation == previous {
                return Ok(());
            }
        }
        self.sessions.insert(session, generation);
        let affected: Vec<_> = self
            .tasks
            .iter()
            .filter_map(|(id, task)| {
                (task.snapshot.spec.owner.scope.session == session
                    && task.snapshot.spec.generation != generation)
                    .then_some(*id)
            })
            .collect();
        for id in affected {
            self.mark_cancelling(id, CancellationReason::SessionAdvanced);
        }
        self.cancel_unobserved_flights();
        Ok(())
    }

    /// Close a session and revoke every completion receipt captured from it.
    pub fn close_session(&mut self, session: SessionId) {
        self.sessions.remove(&session);
        let affected: Vec<_> = self
            .tasks
            .iter()
            .filter_map(|(id, task)| {
                (task.snapshot.spec.owner.scope.session == session).then_some(*id)
            })
            .collect();
        for id in affected {
            self.mark_cancelling(id, CancellationReason::SessionAdvanced);
        }
        self.cancel_unobserved_flights();
    }

    pub fn submit(
        &mut self,
        spec: TaskSpec,
        now: TaskInstant,
    ) -> Result<Submission, AdmissionError> {
        self.expire(now);
        let Some(current) = self.sessions.get(&spec.owner.scope.session).copied() else {
            return Err(AdmissionError::UnknownSession(spec.owner.scope.session));
        };
        if spec.generation != current {
            return Err(AdmissionError::StaleSubmission {
                submitted: spec.generation,
                current,
            });
        }
        if spec.deadline.is_some_and(|deadline| deadline.0 <= now) {
            return Err(AdmissionError::DeadlineAlreadyElapsed);
        }
        if !self.config.resource_limits.contains_key(&spec.resource) {
            return Err(AdmissionError::ResourceUnavailable(spec.resource));
        }

        let key = SingleFlightKey {
            resource: spec.resource,
            recipe: spec.recipe.clone(),
        };
        if let Some(flight_id) = self.single_flights.get(&key).copied() {
            let flight_state = {
                let flight = self.flights.get(&flight_id).expect("indexed flight exists");
                if flight.cancellation.is_cancelled()
                    || !flight
                        .subscribers
                        .iter()
                        .any(|id| self.tasks.get(id).is_some_and(TaskRecord::is_eligible))
                {
                    return Err(AdmissionError::FlightCancelling(flight_id));
                }
                if let Some(existing) = flight.subscribers.iter().find_map(|id| {
                    let task = self.tasks.get(id)?;
                    (task.snapshot.spec.owner == spec.owner
                        && task.snapshot.spec.generation == spec.generation)
                        .then_some(*id)
                }) {
                    return Ok(Submission {
                        task: existing,
                        flight: flight_id,
                        joined_existing_flight: true,
                    });
                }
                if flight.subscribers.len() >= self.config.maximum_subscribers_per_flight {
                    return Err(AdmissionError::SubscriberLimit(flight_id));
                }
                flight.state
            };
            self.check_active_capacity_for(&spec)?;
            let task = self.allocate_task();
            self.tasks.insert(
                task,
                TaskRecord {
                    snapshot: TaskSnapshot {
                        id: task,
                        flight: flight_id,
                        spec: spec.clone(),
                        state: match flight_state {
                            FlightState::Queued => TaskState::Queued,
                            FlightState::Running => TaskState::Running,
                        },
                        progress: None,
                        diagnostics: Vec::new(),
                        dropped_diagnostics: 0,
                    },
                },
            );
            self.flights
                .get_mut(&flight_id)
                .expect("indexed flight exists")
                .subscribers
                .push(task);
            self.supersede_older(&spec, task);
            return Ok(Submission {
                task,
                flight: flight_id,
                joined_existing_flight: true,
            });
        }

        self.check_active_capacity_for(&spec)?;
        if self.queued_flight_count() >= self.config.maximum_queued_flights {
            return Err(AdmissionError::QueuedFlightLimit);
        }
        let task = self.allocate_task();
        let flight = self.allocate_flight();
        self.tasks.insert(
            task,
            TaskRecord {
                snapshot: TaskSnapshot {
                    id: task,
                    flight,
                    spec: spec.clone(),
                    state: TaskState::Queued,
                    progress: None,
                    diagnostics: Vec::new(),
                    dropped_diagnostics: 0,
                },
            },
        );
        self.flights.insert(
            flight,
            FlightRecord {
                id: flight,
                key: key.clone(),
                state: FlightState::Queued,
                submitted_at: now,
                subscribers: vec![task],
                cancellation: TaskCancellation::new(),
            },
        );
        self.single_flights.insert(key, flight);
        self.supersede_older(&spec, task);
        Ok(Submission {
            task,
            flight,
            joined_existing_flight: false,
        })
    }

    /// Select one admitted flight according to priority, then nearest
    /// deadline, then submission time and ID. No thread is started here.
    pub fn dispatch_next(&mut self, now: TaskInstant) -> Option<TaskDispatch> {
        self.expire(now);
        let selected = self
            .flights
            .values()
            .filter(|flight| flight.state == FlightState::Queued)
            .filter(|flight| self.resource_has_capacity(flight.key.resource))
            .filter_map(|flight| self.flight_rank(flight).map(|rank| (flight.id, rank)))
            .max_by(|left, right| compare_rank(&left.1, &right.1))
            .map(|(id, _)| id)?;

        let (resource, recipe, cancellation, representative) = {
            let flight = self
                .flights
                .get_mut(&selected)
                .expect("selected flight exists");
            flight.state = FlightState::Running;
            let representative = flight
                .subscribers
                .iter()
                .copied()
                .find(|id| self.tasks.get(id).is_some_and(TaskRecord::is_eligible))
                .expect("ranked flight has an eligible subscriber");
            (
                flight.key.resource,
                flight.key.recipe.clone(),
                flight.cancellation.clone(),
                representative,
            )
        };
        *self.running_by_resource.entry(resource).or_insert(0) += 1;
        let subscribers = self.flights[&selected].subscribers.clone();
        for task_id in subscribers {
            if let Some(task) = self.tasks.get_mut(&task_id) {
                if task.is_eligible() {
                    task.snapshot.state = TaskState::Running;
                }
            }
        }
        Some(TaskDispatch {
            flight: selected,
            representative_task: representative,
            recipe,
            resource,
            cancellation,
        })
    }

    pub fn report_progress(
        &mut self,
        flight: FlightId,
        progress: TaskProgress,
    ) -> Result<(), CoordinatorError> {
        progress.validate().map_err(CoordinatorError::Progress)?;
        let record = self
            .flights
            .get(&flight)
            .ok_or(CoordinatorError::UnknownFlight(flight))?;
        if record.state != FlightState::Running {
            return Err(CoordinatorError::FlightNotRunning(flight));
        }
        let subscribers = record.subscribers.clone();
        for task_id in subscribers {
            let Some(task) = self.tasks.get_mut(&task_id) else {
                continue;
            };
            if let Some(previous) = &task.snapshot.progress {
                if !progress.follows(previous) {
                    return Err(CoordinatorError::Progress(ProgressError::Regression));
                }
            }
            task.snapshot.progress = Some(progress.clone());
        }
        Ok(())
    }

    pub fn report_diagnostic(
        &mut self,
        flight: FlightId,
        diagnostic: TaskDiagnostic,
    ) -> Result<(), CoordinatorError> {
        let subscribers = self
            .flights
            .get(&flight)
            .ok_or(CoordinatorError::UnknownFlight(flight))?
            .subscribers
            .clone();
        for id in subscribers {
            self.push_diagnostic(id, diagnostic.clone());
        }
        Ok(())
    }

    pub fn cancel_task(
        &mut self,
        task: TaskId,
        reason: CancellationReason,
    ) -> Result<bool, CoordinatorError> {
        if !self.tasks.contains_key(&task) {
            return Err(CoordinatorError::UnknownTask(task));
        }
        let changed = self.mark_cancelling(task, reason);
        self.cancel_unobserved_flights();
        Ok(changed)
    }

    pub fn cancel_owner(&mut self, owner: OwnerScope) -> usize {
        let tasks: Vec<_> = self
            .tasks
            .iter()
            .filter_map(|(id, task)| (task.snapshot.spec.owner == owner).then_some(*id))
            .collect();
        let mut changed = 0;
        for id in tasks {
            changed += usize::from(self.mark_cancelling(id, CancellationReason::Requested));
        }
        self.cancel_unobserved_flights();
        changed
    }

    /// Finish a running physical flight and gate every logical receipt against
    /// the current session generation. A stale/cancelled receipt is returned
    /// for observability but cannot appear in `accepted`.
    pub fn complete(
        &mut self,
        flight: FlightId,
        report: CompletionReport,
        now: TaskInstant,
    ) -> Result<CompletionBatch, CoordinatorError> {
        self.expire(now);
        let record = self
            .flights
            .get(&flight)
            .ok_or(CoordinatorError::UnknownFlight(flight))?;
        if record.state != FlightState::Running {
            return Err(CoordinatorError::FlightNotRunning(flight));
        }
        let subscribers = record.subscribers.clone();
        for diagnostic in &report.diagnostics {
            for task in &subscribers {
                self.push_diagnostic(*task, diagnostic.clone());
            }
        }

        let mut batch = CompletionBatch::default();
        for task_id in subscribers {
            let Some(task) = self.tasks.get(&task_id) else {
                continue;
            };
            let snapshot = &task.snapshot;
            let receipt = CompletionReceipt {
                task: task_id,
                flight,
                owner: snapshot.spec.owner,
                captured_generation: snapshot.spec.generation,
                recipe: snapshot.spec.recipe.clone(),
                resource: snapshot.spec.resource,
                deadline: snapshot.spec.deadline,
                completed_at: now,
                outcome: report.outcome.clone(),
            };
            let rejection = match self
                .sessions
                .get(&snapshot.spec.owner.scope.session)
                .copied()
            {
                None => Some(CompletionRejectionReason::SessionClosed),
                Some(current) if current != snapshot.spec.generation => {
                    Some(CompletionRejectionReason::StaleGeneration {
                        captured: snapshot.spec.generation,
                        current,
                    })
                }
                Some(_)
                    if snapshot
                        .spec
                        .deadline
                        .is_some_and(|deadline| deadline.0 <= now) =>
                {
                    Some(CompletionRejectionReason::DeadlineExceeded)
                }
                Some(_) => match snapshot.state {
                    TaskState::Cancelling(reason) => {
                        Some(CompletionRejectionReason::Cancelled(reason))
                    }
                    _ => None,
                },
            };
            if let Some(reason) = rejection {
                batch.rejected.push(RejectedCompletion { receipt, reason });
            } else {
                batch.accepted.push(receipt);
            }
        }

        let accepted: Vec<_> = batch.accepted.iter().map(|receipt| receipt.task).collect();
        let rejected: Vec<_> = batch
            .rejected
            .iter()
            .map(|rejected| rejected.receipt.task)
            .collect();
        for id in accepted {
            let state = match report.outcome {
                CompletionOutcome::Succeeded { .. } => TaskState::Succeeded,
                CompletionOutcome::Failed { .. } => TaskState::Failed,
                CompletionOutcome::Cancelled => TaskState::Cancelled,
            };
            self.retire_task(id, state);
        }
        for id in rejected {
            self.retire_task(id, TaskState::Rejected);
        }
        self.remove_flight(flight);
        Ok(batch)
    }

    /// Re-check an already accepted immutable receipt at the exact boundary
    /// where an application adapter is about to publish or reveal it. This
    /// closes the gap when command handling is deferred after `complete`.
    pub fn validate_for_publication(
        &self,
        receipt: &CompletionReceipt,
        now: TaskInstant,
    ) -> Result<(), CompletionRejectionReason> {
        let Some(current) = self.sessions.get(&receipt.owner.scope.session).copied() else {
            return Err(CompletionRejectionReason::SessionClosed);
        };
        if current != receipt.captured_generation {
            return Err(CompletionRejectionReason::StaleGeneration {
                captured: receipt.captured_generation,
                current,
            });
        }
        if receipt.deadline.is_some_and(|deadline| deadline.0 <= now) {
            return Err(CompletionRejectionReason::DeadlineExceeded);
        }
        Ok(())
    }

    pub fn snapshot(&self, task: TaskId) -> Option<TaskSnapshot> {
        self.tasks
            .get(&task)
            .map(|record| record.snapshot.clone())
            .or_else(|| self.terminal.iter().find(|entry| entry.id == task).cloned())
    }

    pub fn active_task_count(&self) -> usize {
        self.tasks.len()
    }

    pub fn queued_flight_count(&self) -> usize {
        self.flights
            .values()
            .filter(|flight| flight.state == FlightState::Queued)
            .count()
    }

    pub fn running_flight_count(&self, resource: ResourceClass) -> usize {
        self.running_by_resource
            .get(&resource)
            .copied()
            .unwrap_or(0)
    }

    fn check_active_capacity_for(&self, spec: &TaskSpec) -> Result<(), AdmissionError> {
        let supersedable = self
            .tasks
            .values()
            .filter(|task| {
                task.snapshot.spec.owner == spec.owner
                    && task.snapshot.spec.generation < spec.generation
            })
            .count();
        if self.tasks.len().saturating_sub(supersedable) >= self.config.maximum_active_tasks {
            return Err(AdmissionError::ActiveTaskLimit);
        }
        Ok(())
    }

    fn supersede_older(&mut self, spec: &TaskSpec, except: TaskId) {
        let older: Vec<_> = self
            .tasks
            .iter()
            .filter_map(|(id, task)| {
                (*id != except
                    && task.snapshot.spec.owner == spec.owner
                    && task.snapshot.spec.generation < spec.generation)
                    .then_some(*id)
            })
            .collect();
        for id in older {
            self.mark_cancelling(id, CancellationReason::Superseded);
        }
        self.cancel_unobserved_flights();
    }

    fn expire(&mut self, now: TaskInstant) {
        let expired: Vec<_> = self
            .tasks
            .iter()
            .filter_map(|(id, task)| {
                (task.is_eligible()
                    && task
                        .snapshot
                        .spec
                        .deadline
                        .is_some_and(|deadline| deadline.0 <= now))
                .then_some(*id)
            })
            .collect();
        for id in expired {
            self.mark_cancelling(id, CancellationReason::DeadlineExceeded);
        }
        self.cancel_unobserved_flights();
        self.retire_abandoned_queued_flights();
    }

    fn mark_cancelling(&mut self, task: TaskId, reason: CancellationReason) -> bool {
        let Some(record) = self.tasks.get_mut(&task) else {
            return false;
        };
        if matches!(record.snapshot.state, TaskState::Cancelling(_)) {
            return false;
        }
        record.snapshot.state = TaskState::Cancelling(reason);
        true
    }

    fn cancel_unobserved_flights(&mut self) {
        for flight in self.flights.values() {
            let any_eligible = flight
                .subscribers
                .iter()
                .any(|id| self.tasks.get(id).is_some_and(TaskRecord::is_eligible));
            if !any_eligible {
                flight.cancellation.cancel();
            }
        }
    }

    fn retire_abandoned_queued_flights(&mut self) {
        let abandoned: Vec<_> = self
            .flights
            .values()
            .filter(|flight| flight.state == FlightState::Queued)
            .filter(|flight| {
                !flight
                    .subscribers
                    .iter()
                    .any(|id| self.tasks.get(id).is_some_and(TaskRecord::is_eligible))
            })
            .map(|flight| flight.id)
            .collect();
        for flight in abandoned {
            let subscribers = self.flights[&flight].subscribers.clone();
            for id in subscribers {
                let state = match self.tasks.get(&id).map(|task| task.snapshot.state) {
                    Some(TaskState::Cancelling(CancellationReason::DeadlineExceeded)) => {
                        TaskState::Rejected
                    }
                    _ => TaskState::Cancelled,
                };
                self.retire_task(id, state);
            }
            self.remove_flight(flight);
        }
    }

    fn push_diagnostic(&mut self, task: TaskId, diagnostic: TaskDiagnostic) {
        let Some(task) = self.tasks.get_mut(&task) else {
            return;
        };
        if task.snapshot.diagnostics.len() == self.config.maximum_diagnostics_per_task {
            task.snapshot.diagnostics.remove(0);
            task.snapshot.dropped_diagnostics = task.snapshot.dropped_diagnostics.saturating_add(1);
        }
        task.snapshot.diagnostics.push(diagnostic);
    }

    fn retire_task(&mut self, task: TaskId, state: TaskState) {
        let Some(mut record) = self.tasks.remove(&task) else {
            return;
        };
        record.snapshot.state = state;
        self.terminal.push_back(record.snapshot);
        while self.terminal.len() > self.config.maximum_terminal_snapshots {
            self.terminal.pop_front();
        }
    }

    fn remove_flight(&mut self, flight: FlightId) {
        let Some(record) = self.flights.remove(&flight) else {
            return;
        };
        if self.single_flights.get(&record.key) == Some(&flight) {
            self.single_flights.remove(&record.key);
        }
        if record.state == FlightState::Running {
            if let Some(count) = self.running_by_resource.get_mut(&record.key.resource) {
                *count = count.saturating_sub(1);
            }
        }
    }

    fn resource_has_capacity(&self, resource: ResourceClass) -> bool {
        let Some(limit) = self.config.resource_limits.get(&resource) else {
            return false;
        };
        self.running_flight_count(resource) < *limit
    }

    fn flight_rank(&self, flight: &FlightRecord) -> Option<FlightRank> {
        let mut priority = TaskPriority::Maintenance;
        let mut deadline = None;
        let mut has_eligible = false;
        for id in &flight.subscribers {
            let Some(task) = self.tasks.get(id) else {
                continue;
            };
            if !task.is_eligible() {
                continue;
            }
            has_eligible = true;
            priority = priority.max(task.snapshot.spec.priority);
            deadline = match (deadline, task.snapshot.spec.deadline) {
                (None, next) => next,
                (some, None) => some,
                (Some(left), Some(right)) => Some(left.min(right)),
            };
        }
        has_eligible.then_some(FlightRank {
            priority,
            deadline,
            submitted_at: flight.submitted_at,
            id: flight.id,
        })
    }

    fn allocate_task(&mut self) -> TaskId {
        let id = TaskId(self.next_task);
        self.next_task = self.next_task.saturating_add(1);
        id
    }

    fn allocate_flight(&mut self) -> FlightId {
        let id = FlightId(self.next_flight);
        self.next_flight = self.next_flight.saturating_add(1);
        id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FlightRank {
    priority: TaskPriority,
    deadline: Option<TaskDeadline>,
    submitted_at: TaskInstant,
    id: FlightId,
}

fn compare_rank(left: &FlightRank, right: &FlightRank) -> CmpOrdering {
    left.priority
        .cmp(&right.priority)
        .then_with(|| match (left.deadline, right.deadline) {
            (Some(left), Some(right)) => right.cmp(&left),
            (Some(_), None) => CmpOrdering::Greater,
            (None, Some(_)) => CmpOrdering::Less,
            (None, None) => CmpOrdering::Equal,
        })
        .then_with(|| right.submitted_at.cmp(&left.submitted_at))
        .then_with(|| right.id.cmp(&left.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe(byte: u8) -> CanonicalRecipeKey {
        CanonicalRecipeKey::new("audec.test", 1, [byte; 32]).unwrap()
    }

    fn owner(session: u128, pane: u64, owner: u64) -> OwnerScope {
        OwnerScope {
            owner: TaskOwner(owner),
            scope: TaskScope {
                session: SessionId(session),
                pane: PaneScope::Pane(PaneId(pane)),
            },
        }
    }

    fn spec(
        owner: OwnerScope,
        generation: u64,
        recipe: CanonicalRecipeKey,
        priority: TaskPriority,
    ) -> TaskSpec {
        TaskSpec {
            owner,
            generation: SessionGeneration(generation),
            recipe,
            resource: ResourceClass::Cpu,
            priority,
            deadline: None,
        }
    }

    fn coordinator() -> TaskCoordinator {
        TaskCoordinator::new(CoordinatorConfig::default()).unwrap()
    }

    fn success() -> CompletionReport {
        CompletionReport {
            outcome: CompletionOutcome::Succeeded { output: None },
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn identical_recipes_share_a_flight_but_not_logical_authority() {
        let mut coordinator = coordinator();
        coordinator
            .observe_session(SessionId(1), SessionGeneration(4))
            .unwrap();
        let first = coordinator
            .submit(
                spec(owner(1, 10, 7), 4, recipe(1), TaskPriority::Foreground),
                TaskInstant(0),
            )
            .unwrap();
        let second = coordinator
            .submit(
                spec(owner(1, 11, 8), 4, recipe(1), TaskPriority::Interactive),
                TaskInstant(1),
            )
            .unwrap();
        assert_ne!(first.task, second.task);
        assert_eq!(first.flight, second.flight);
        assert!(second.joined_existing_flight);

        let dispatch = coordinator.dispatch_next(TaskInstant(2)).unwrap();
        assert_eq!(dispatch.flight(), first.flight);
        let batch = coordinator
            .complete(dispatch.flight(), success(), TaskInstant(3))
            .unwrap();
        assert_eq!(batch.accepted.len(), 2);
        assert!(batch.rejected.is_empty());
    }

    #[test]
    fn advancing_a_session_cancels_work_and_rejects_late_completion() {
        let mut coordinator = coordinator();
        coordinator
            .observe_session(SessionId(1), SessionGeneration(9))
            .unwrap();
        let submitted = coordinator
            .submit(
                spec(owner(1, 3, 4), 9, recipe(2), TaskPriority::Interactive),
                TaskInstant(0),
            )
            .unwrap();
        let dispatch = coordinator.dispatch_next(TaskInstant(1)).unwrap();
        coordinator
            .observe_session(SessionId(1), SessionGeneration(10))
            .unwrap();
        assert!(dispatch.cancellation().is_cancelled());

        let batch = coordinator
            .complete(submitted.flight, success(), TaskInstant(2))
            .unwrap();
        assert!(batch.accepted.is_empty());
        assert_eq!(batch.rejected.len(), 1);
        assert_eq!(
            batch.rejected[0].reason,
            CompletionRejectionReason::StaleGeneration {
                captured: SessionGeneration(9),
                current: SessionGeneration(10),
            }
        );
        assert_eq!(
            coordinator.snapshot(submitted.task).unwrap().state,
            TaskState::Rejected
        );
    }

    #[test]
    fn one_cancelled_subscriber_does_not_cancel_shared_work() {
        let mut coordinator = coordinator();
        coordinator
            .observe_session(SessionId(1), SessionGeneration(1))
            .unwrap();
        let first = coordinator
            .submit(
                spec(owner(1, 1, 1), 1, recipe(3), TaskPriority::Foreground),
                TaskInstant(0),
            )
            .unwrap();
        let second = coordinator
            .submit(
                spec(owner(1, 2, 2), 1, recipe(3), TaskPriority::Foreground),
                TaskInstant(0),
            )
            .unwrap();
        let dispatch = coordinator.dispatch_next(TaskInstant(1)).unwrap();
        coordinator
            .cancel_task(first.task, CancellationReason::Requested)
            .unwrap();
        assert!(!dispatch.cancellation().is_cancelled());
        let batch = coordinator
            .complete(dispatch.flight(), success(), TaskInstant(2))
            .unwrap();
        assert_eq!(batch.accepted[0].task(), second.task);
        assert_eq!(batch.rejected[0].receipt.task(), first.task);
    }

    #[test]
    fn admission_and_resource_bounds_are_enforced() {
        let mut config = CoordinatorConfig::default();
        config.maximum_active_tasks = 2;
        config.maximum_queued_flights = 2;
        config.resource_limits.insert(ResourceClass::Cpu, 1);
        let mut coordinator = TaskCoordinator::new(config).unwrap();
        coordinator
            .observe_session(SessionId(1), SessionGeneration(1))
            .unwrap();
        coordinator
            .submit(
                spec(owner(1, 1, 1), 1, recipe(1), TaskPriority::Background),
                TaskInstant(0),
            )
            .unwrap();
        coordinator
            .submit(
                spec(owner(1, 2, 2), 1, recipe(2), TaskPriority::Foreground),
                TaskInstant(0),
            )
            .unwrap();
        assert_eq!(
            coordinator.submit(
                spec(owner(1, 3, 3), 1, recipe(3), TaskPriority::Interactive),
                TaskInstant(0),
            ),
            Err(AdmissionError::ActiveTaskLimit)
        );
        assert!(coordinator.dispatch_next(TaskInstant(1)).is_some());
        assert!(coordinator.dispatch_next(TaskInstant(1)).is_none());
    }

    #[test]
    fn priority_deadline_and_fifo_ties_are_deterministic() {
        let mut coordinator = coordinator();
        coordinator
            .observe_session(SessionId(1), SessionGeneration(1))
            .unwrap();
        let background = coordinator
            .submit(
                spec(owner(1, 1, 1), 1, recipe(1), TaskPriority::Background),
                TaskInstant(0),
            )
            .unwrap();
        let mut soon = spec(owner(1, 2, 2), 1, recipe(2), TaskPriority::Interactive);
        soon.deadline = Some(TaskDeadline(TaskInstant(20)));
        let soon = coordinator.submit(soon, TaskInstant(2)).unwrap();
        let mut later = spec(owner(1, 3, 3), 1, recipe(3), TaskPriority::Interactive);
        later.deadline = Some(TaskDeadline(TaskInstant(30)));
        coordinator.submit(later, TaskInstant(1)).unwrap();

        let dispatch = coordinator.dispatch_next(TaskInstant(3)).unwrap();
        assert_eq!(dispatch.flight(), soon.flight);
        assert_ne!(dispatch.flight(), background.flight);
    }

    #[test]
    fn progress_is_validated_and_diagnostics_are_bounded() {
        let mut config = CoordinatorConfig::default();
        config.maximum_diagnostics_per_task = 2;
        let mut coordinator = TaskCoordinator::new(config).unwrap();
        coordinator
            .observe_session(SessionId(1), SessionGeneration(1))
            .unwrap();
        let submitted = coordinator
            .submit(
                spec(owner(1, 1, 1), 1, recipe(1), TaskPriority::Foreground),
                TaskInstant(0),
            )
            .unwrap();
        coordinator.dispatch_next(TaskInstant(1)).unwrap();
        let progress = TaskProgress {
            phase: "analyzing".into(),
            phase_index: 0,
            phase_count: 2,
            completed_units: 4,
            total_units: 10,
        };
        coordinator
            .report_progress(submitted.flight, progress.clone())
            .unwrap();
        let mut regressed = progress;
        regressed.completed_units = 3;
        assert_eq!(
            coordinator.report_progress(submitted.flight, regressed),
            Err(CoordinatorError::Progress(ProgressError::Regression))
        );
        for index in 0..3 {
            coordinator
                .report_diagnostic(
                    submitted.flight,
                    TaskDiagnostic {
                        severity: DiagnosticSeverity::Info,
                        code: format!("d{index}"),
                        detail: String::new(),
                    },
                )
                .unwrap();
        }
        let snapshot = coordinator.snapshot(submitted.task).unwrap();
        assert_eq!(snapshot.diagnostics.len(), 2);
        assert_eq!(snapshot.dropped_diagnostics, 1);
        assert_eq!(snapshot.diagnostics[0].code, "d1");
    }

    #[test]
    fn expired_queued_work_never_dispatches() {
        let mut coordinator = coordinator();
        coordinator
            .observe_session(SessionId(1), SessionGeneration(1))
            .unwrap();
        let mut deadline = spec(owner(1, 1, 1), 1, recipe(1), TaskPriority::Interactive);
        deadline.deadline = Some(TaskDeadline(TaskInstant(5)));
        let submitted = coordinator.submit(deadline, TaskInstant(0)).unwrap();
        assert!(coordinator.dispatch_next(TaskInstant(5)).is_none());
        assert_eq!(
            coordinator.snapshot(submitted.task).unwrap().state,
            TaskState::Rejected
        );
    }

    #[test]
    fn duplicate_owner_submission_is_idempotent() {
        let mut coordinator = coordinator();
        coordinator
            .observe_session(SessionId(1), SessionGeneration(1))
            .unwrap();
        let value = spec(owner(1, 1, 1), 1, recipe(9), TaskPriority::Foreground);
        let first = coordinator.submit(value.clone(), TaskInstant(0)).unwrap();
        let second = coordinator.submit(value, TaskInstant(1)).unwrap();
        assert_eq!(first.task, second.task);
        assert_eq!(coordinator.active_task_count(), 1);
    }

    #[test]
    fn accepted_receipt_can_be_rechecked_at_deferred_publication_boundary() {
        let mut coordinator = coordinator();
        coordinator
            .observe_session(SessionId(1), SessionGeneration(1))
            .unwrap();
        let submitted = coordinator
            .submit(
                spec(owner(1, 1, 1), 1, recipe(1), TaskPriority::Foreground),
                TaskInstant(0),
            )
            .unwrap();
        coordinator.dispatch_next(TaskInstant(1)).unwrap();
        let receipt = coordinator
            .complete(submitted.flight, success(), TaskInstant(2))
            .unwrap()
            .accepted
            .pop()
            .unwrap();
        assert!(coordinator
            .validate_for_publication(&receipt, TaskInstant(2))
            .is_ok());
        coordinator
            .observe_session(SessionId(1), SessionGeneration(2))
            .unwrap();
        assert_eq!(
            coordinator.validate_for_publication(&receipt, TaskInstant(3)),
            Err(CompletionRejectionReason::StaleGeneration {
                captured: SessionGeneration(1),
                current: SessionGeneration(2),
            })
        );
    }
}
