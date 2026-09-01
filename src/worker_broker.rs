//! Deterministic admission, fairness, liveness, and completion policy for
//! isolated analysis workers.
//!
//! This module refuses to claim that an operating-system limit exists merely
//! because a job declared a budget.  It decides which jobs may be launched,
//! bounds protocol-visible output/logs, and describes deadline escalation.
//! Platform launchers remain responsible for applying hard RSS/CPU/process
//! limits and for carrying out the returned cancel/kill actions.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use crate::model_wire::WorkerResult;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BrokerTick(u64);

impl BrokerTick {
    pub const ZERO: Self = Self(0);

    pub const fn from_millis(millis: u64) -> Self {
        Self(millis)
    }

    pub const fn as_millis(self) -> u64 {
        self.0
    }

    pub const fn saturating_add(self, millis: u64) -> Self {
        Self(self.0.saturating_add(millis))
    }

    pub const fn elapsed_since(self, earlier: Self) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerDeadlines {
    pub startup: Duration,
    pub request: Duration,
    /// Schema-v1 has no heartbeat record. Progress records refresh this
    /// deadline as well as the progress deadline.
    pub heartbeat: Duration,
    pub progress: Duration,
    pub cancel_grace: Duration,
    pub kill_grace: Duration,
}

impl Default for WorkerDeadlines {
    fn default() -> Self {
        Self {
            startup: Duration::from_secs(10),
            request: Duration::from_secs(120),
            heartbeat: Duration::from_secs(120),
            progress: Duration::from_secs(300),
            cancel_grace: Duration::from_secs(10),
            kill_grace: Duration::from_secs(5),
        }
    }
}

impl WorkerDeadlines {
    pub fn validate(self) -> Result<(), BrokerConfigError> {
        if [
            self.startup,
            self.request,
            self.heartbeat,
            self.progress,
            self.cancel_grace,
            self.kill_grace,
        ]
        .into_iter()
        .any(|duration| duration.is_zero())
        {
            return Err(BrokerConfigError::ZeroDeadline);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerOutputPolicy {
    pub maximum_control_line_bytes: usize,
    pub maximum_buffered_control_records: usize,
    pub maximum_log_tail_bytes: usize,
    pub maximum_artifacts: usize,
    pub maximum_artifact_bytes: u64,
    pub maximum_total_output_bytes: u64,
    pub maximum_measurements: usize,
}

impl Default for WorkerOutputPolicy {
    fn default() -> Self {
        Self {
            maximum_control_line_bytes: 8 * 1024 * 1024,
            maximum_buffered_control_records: 64,
            maximum_log_tail_bytes: 64 * 1024,
            maximum_artifacts: 4_096,
            maximum_artifact_bytes: 16 * 1024 * 1024 * 1024,
            maximum_total_output_bytes: 64 * 1024 * 1024 * 1024,
            maximum_measurements: 65_536,
        }
    }
}

impl WorkerOutputPolicy {
    pub fn validate(self) -> Result<(), BrokerConfigError> {
        if self.maximum_control_line_bytes == 0
            || self.maximum_buffered_control_records == 0
            || self.maximum_log_tail_bytes == 0
            || self.maximum_artifacts == 0
            || self.maximum_artifact_bytes == 0
            || self.maximum_total_output_bytes == 0
            || self.maximum_measurements == 0
        {
            return Err(BrokerConfigError::ZeroOutputLimit);
        }
        if self.maximum_artifact_bytes > self.maximum_total_output_bytes {
            return Err(BrokerConfigError::ArtifactExceedsTotalOutput);
        }
        Ok(())
    }

    pub fn validate_result(self, result: &WorkerResult) -> Result<OutputSummary, OutputRefusal> {
        if result.artifacts.len() > self.maximum_artifacts {
            return Err(OutputRefusal::TooManyArtifacts {
                declared: result.artifacts.len(),
                maximum: self.maximum_artifacts,
            });
        }
        if result.measurements.len() > self.maximum_measurements {
            return Err(OutputRefusal::TooManyMeasurements {
                declared: result.measurements.len(),
                maximum: self.maximum_measurements,
            });
        }
        let mut total_bytes = 0_u64;
        for artifact in &result.artifacts {
            if artifact.byte_len > self.maximum_artifact_bytes {
                return Err(OutputRefusal::ArtifactTooLarge {
                    path: artifact.relative_path.clone(),
                    declared: artifact.byte_len,
                    maximum: self.maximum_artifact_bytes,
                });
            }
            total_bytes = total_bytes
                .checked_add(artifact.byte_len)
                .ok_or(OutputRefusal::TotalSizeOverflow)?;
            if total_bytes > self.maximum_total_output_bytes {
                return Err(OutputRefusal::TotalTooLarge {
                    declared: total_bytes,
                    maximum: self.maximum_total_output_bytes,
                });
            }
        }
        Ok(OutputSummary {
            artifact_count: result.artifacts.len(),
            measurement_count: result.measurements.len(),
            total_bytes,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimePolicy {
    pub deadlines: WorkerDeadlines,
    pub output: WorkerOutputPolicy,
}

impl Default for RuntimePolicy {
    fn default() -> Self {
        Self {
            deadlines: WorkerDeadlines::default(),
            output: WorkerOutputPolicy::default(),
        }
    }
}

impl RuntimePolicy {
    pub fn validate(self) -> Result<(), BrokerConfigError> {
        self.deadlines.validate()?;
        self.output.validate()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrokerConfigError {
    ZeroDeadline,
    ZeroOutputLimit,
    ArtifactExceedsTotalOutput,
    InvalidCapacity,
    InvalidAgingWindow,
}

impl fmt::Display for BrokerConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ZeroDeadline => "worker deadlines must be non-zero",
            Self::ZeroOutputLimit => "worker output limits must be non-zero",
            Self::ArtifactExceedsTotalOutput => {
                "per-artifact output limit cannot exceed total output limit"
            }
            Self::InvalidCapacity => "worker broker capacity or reservation is invalid",
            Self::InvalidAgingWindow => "worker broker aging window must be non-zero",
        })
    }
}

impl std::error::Error for BrokerConfigError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputRefusal {
    TooManyArtifacts {
        declared: usize,
        maximum: usize,
    },
    TooManyMeasurements {
        declared: usize,
        maximum: usize,
    },
    ArtifactTooLarge {
        path: String,
        declared: u64,
        maximum: u64,
    },
    TotalTooLarge {
        declared: u64,
        maximum: u64,
    },
    TotalSizeOverflow,
}

impl fmt::Display for OutputRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyArtifacts { declared, maximum } => write!(
                formatter,
                "worker declared {declared} artifacts, maximum is {maximum}"
            ),
            Self::TooManyMeasurements { declared, maximum } => write!(
                formatter,
                "worker declared {declared} measurements, maximum is {maximum}"
            ),
            Self::ArtifactTooLarge {
                path,
                declared,
                maximum,
            } => write!(
                formatter,
                "worker artifact {path} declares {declared} bytes, maximum is {maximum}"
            ),
            Self::TotalTooLarge { declared, maximum } => write!(
                formatter,
                "worker result declares {declared} bytes, maximum is {maximum}"
            ),
            Self::TotalSizeOverflow => formatter.write_str("worker result size overflowed u64"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputSummary {
    pub artifact_count: usize,
    pub measurement_count: usize,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobIdentity {
    job_id: String,
    generation: u64,
    cache_key: String,
    manifest_sha256: String,
}

impl JobIdentity {
    pub fn new(
        job_id: impl Into<String>,
        generation: u64,
        cache_key: impl Into<String>,
        manifest_sha256: impl Into<String>,
    ) -> Result<Self, SubmissionRefusal> {
        let identity = Self {
            job_id: job_id.into(),
            generation,
            cache_key: cache_key.into(),
            manifest_sha256: manifest_sha256.into(),
        };
        if !valid_label(&identity.job_id)
            || !valid_hash(&identity.cache_key)
            || !valid_hash(&identity.manifest_sha256)
        {
            return Err(SubmissionRefusal::InvalidIdentity);
        }
        Ok(identity)
    }

    pub fn job_id(&self) -> &str {
        &self.job_id
    }
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    pub fn cache_key(&self) -> &str {
        &self.cache_key
    }
    pub fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum JobPriority {
    Background,
    UserInitiated,
    Interactive,
}

impl JobPriority {
    const fn rank(self) -> u64 {
        match self {
            Self::Background => 0,
            Self::UserInitiated => 1,
            Self::Interactive => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceDemand {
    pub cpu_slots: u16,
    pub memory_bytes: u64,
    pub scratch_bytes: u64,
    pub expected_output_bytes: u64,
    pub accelerator: Option<String>,
}

impl ResourceDemand {
    fn checked_add_to(&self, usage: &mut ResourceUsage) -> bool {
        let Some(cpu_slots) = usage.cpu_slots.checked_add(self.cpu_slots) else {
            return false;
        };
        let Some(memory_bytes) = usage.memory_bytes.checked_add(self.memory_bytes) else {
            return false;
        };
        let Some(scratch_bytes) = usage.scratch_bytes.checked_add(self.scratch_bytes) else {
            return false;
        };
        usage.cpu_slots = cpu_slots;
        usage.memory_bytes = memory_bytes;
        usage.scratch_bytes = scratch_bytes;
        if let Some(accelerator) = &self.accelerator {
            *usage.accelerators.entry(accelerator.clone()).or_default() += 1;
        }
        true
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrokerCapacity {
    pub cpu_slots: u16,
    pub memory_bytes: u64,
    pub scratch_bytes: u64,
    pub worker_slots: u16,
    pub accelerators: BTreeMap<String, u16>,
    pub realtime_cpu_reserve: u16,
    pub realtime_memory_reserve: u64,
    pub render_cpu_reserve: u16,
    pub render_memory_reserve: u64,
}

impl BrokerCapacity {
    pub fn validate(&self) -> Result<(), BrokerConfigError> {
        if self.cpu_slots == 0
            || self.memory_bytes == 0
            || self.scratch_bytes == 0
            || self.worker_slots == 0
            || self
                .realtime_cpu_reserve
                .saturating_add(self.render_cpu_reserve)
                > self.cpu_slots
            || self
                .realtime_memory_reserve
                .saturating_add(self.render_memory_reserve)
                > self.memory_bytes
            || self.accelerators.values().any(|slots| *slots == 0)
        {
            return Err(BrokerConfigError::InvalidCapacity);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ForegroundPressure {
    pub realtime_audio_active: bool,
    pub render_work_pending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobTicket {
    pub identity: JobIdentity,
    pub priority: JobPriority,
    pub demand: ResourceDemand,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmissionRefusal {
    InvalidIdentity,
    DuplicateOrOldGeneration,
    GenerationAlreadyActive,
    ImpossibleResourceDemand,
    OutputBudgetExceeded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancellationReason {
    User,
    Superseded,
    ForegroundPressure,
    StartupDeadline,
    HeartbeatDeadline,
    ProgressDeadline,
    InvalidOutput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscalationState {
    Starting,
    Running,
    CancelRequested { reason: CancellationReason },
    KillRequested { reason: CancellationReason },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrokerAction {
    Start(JobTicket),
    SendCancel {
        identity: JobIdentity,
        reason: CancellationReason,
    },
    Kill {
        identity: JobIdentity,
        reason: CancellationReason,
    },
    DeclareUnresponsive {
        identity: JobIdentity,
        reason: CancellationReason,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgressPoint {
    pub phase: u16,
    pub completed: u64,
    pub total: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservationRefusal {
    UnknownOrStaleJob,
    InvalidState,
    RegressingProgress,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompletionRefusal {
    UnknownJob,
    StaleGeneration { latest: u64, received: u64 },
    IdentityMismatch,
    CancelledOrKilling,
    Output(OutputRefusal),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionAttempt {
    pub identity: JobIdentity,
    pub result_sha256: String,
    pub output: OutputSummary,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionReceipt {
    identity: JobIdentity,
    result_sha256: String,
    output: OutputSummary,
    completed_at: BrokerTick,
    receipt_sequence: u64,
}

impl CompletionReceipt {
    pub fn identity(&self) -> &JobIdentity {
        &self.identity
    }
    pub fn result_sha256(&self) -> &str {
        &self.result_sha256
    }
    pub const fn output(&self) -> OutputSummary {
        self.output
    }
    pub const fn completed_at(&self) -> BrokerTick {
        self.completed_at
    }
    pub const fn receipt_sequence(&self) -> u64 {
        self.receipt_sequence
    }
}

#[derive(Clone, Debug)]
struct QueuedJob {
    ticket: JobTicket,
    enqueued_at: BrokerTick,
    sequence: u64,
}

#[derive(Clone, Debug)]
struct ActiveJob {
    ticket: JobTicket,
    admitted_at: BrokerTick,
    last_heartbeat: BrokerTick,
    last_progress: BrokerTick,
    progress: Option<ProgressPoint>,
    escalation_at: Option<BrokerTick>,
    state: EscalationState,
}

#[derive(Clone, Debug, Default)]
struct ResourceUsage {
    cpu_slots: u16,
    memory_bytes: u64,
    scratch_bytes: u64,
    accelerators: BTreeMap<String, u16>,
}

/// Pure broker state. Time is supplied by the caller, which makes deadline and
/// fake-worker tests deterministic and keeps wall-clock access out of policy.
#[derive(Debug)]
pub struct WorkerBroker {
    capacity: BrokerCapacity,
    deadlines_millis: DeadlineMillis,
    maximum_output_bytes: u64,
    aging_window_millis: u64,
    queued: Vec<QueuedJob>,
    active: BTreeMap<JobIdentity, ActiveJob>,
    latest_generation: BTreeMap<String, u64>,
    completed: BTreeMap<JobIdentity, CompletionReceipt>,
    terminal_without_receipt: BTreeSet<JobIdentity>,
    next_sequence: u64,
    next_receipt_sequence: u64,
}

#[derive(Clone, Copy, Debug)]
struct DeadlineMillis {
    startup: u64,
    heartbeat: u64,
    progress: u64,
    cancel: u64,
    kill: u64,
}

impl WorkerBroker {
    pub fn new(
        capacity: BrokerCapacity,
        policy: RuntimePolicy,
        aging_window: Duration,
    ) -> Result<Self, BrokerConfigError> {
        capacity.validate()?;
        policy.validate()?;
        let aging_window_millis = duration_millis(aging_window);
        if aging_window_millis == 0 {
            return Err(BrokerConfigError::InvalidAgingWindow);
        }
        Ok(Self {
            capacity,
            deadlines_millis: DeadlineMillis {
                startup: duration_millis(policy.deadlines.startup),
                heartbeat: duration_millis(policy.deadlines.heartbeat),
                progress: duration_millis(policy.deadlines.progress),
                cancel: duration_millis(policy.deadlines.cancel_grace),
                kill: duration_millis(policy.deadlines.kill_grace),
            },
            maximum_output_bytes: policy.output.maximum_total_output_bytes,
            aging_window_millis,
            queued: Vec::new(),
            active: BTreeMap::new(),
            latest_generation: BTreeMap::new(),
            completed: BTreeMap::new(),
            terminal_without_receipt: BTreeSet::new(),
            next_sequence: 0,
            next_receipt_sequence: 0,
        })
    }

    pub fn submit(&mut self, ticket: JobTicket, now: BrokerTick) -> Result<(), SubmissionRefusal> {
        if !valid_label(ticket.identity.job_id())
            || !valid_hash(ticket.identity.cache_key())
            || !valid_hash(ticket.identity.manifest_sha256())
        {
            return Err(SubmissionRefusal::InvalidIdentity);
        }
        if ticket.demand.cpu_slots == 0
            || ticket.demand.memory_bytes == 0
            || ticket.demand.scratch_bytes == 0
            || ticket.demand.cpu_slots > self.capacity.cpu_slots
            || ticket.demand.memory_bytes > self.capacity.memory_bytes
            || ticket.demand.scratch_bytes > self.capacity.scratch_bytes
            || ticket
                .demand
                .accelerator
                .as_ref()
                .is_some_and(|accelerator| !self.capacity.accelerators.contains_key(accelerator))
        {
            return Err(SubmissionRefusal::ImpossibleResourceDemand);
        }
        if ticket.demand.expected_output_bytes > self.maximum_output_bytes {
            return Err(SubmissionRefusal::OutputBudgetExceeded);
        }
        if let Some(latest) = self.latest_generation.get(ticket.identity.job_id()) {
            if ticket.identity.generation() <= *latest {
                return Err(SubmissionRefusal::DuplicateOrOldGeneration);
            }
            if self
                .active
                .keys()
                .any(|identity| identity.job_id() == ticket.identity.job_id())
                || self
                    .queued
                    .iter()
                    .any(|job| job.ticket.identity.job_id() == ticket.identity.job_id())
            {
                return Err(SubmissionRefusal::GenerationAlreadyActive);
            }
        }
        self.latest_generation.insert(
            ticket.identity.job_id().to_owned(),
            ticket.identity.generation(),
        );
        self.queued.push(QueuedJob {
            ticket,
            enqueued_at: now,
            sequence: self.next_sequence,
        });
        self.next_sequence = self.next_sequence.saturating_add(1);
        Ok(())
    }

    /// Admits every currently fitting job. Priority is aged once per aging
    /// window; enqueue sequence is the deterministic final tie-breaker.
    pub fn schedule(&mut self, now: BrokerTick, pressure: ForegroundPressure) -> Vec<BrokerAction> {
        let mut actions = Vec::new();
        loop {
            let usage = self.resource_usage();
            let mut candidates: Vec<usize> = self
                .queued
                .iter()
                .enumerate()
                .filter_map(|(index, queued)| {
                    self.fits(&queued.ticket.demand, &usage, pressure)
                        .then_some(index)
                })
                .collect();
            candidates.sort_by(|left, right| {
                let left = &self.queued[*left];
                let right = &self.queued[*right];
                let left_score =
                    left.ticket.priority.rank().saturating_add(
                        now.elapsed_since(left.enqueued_at) / self.aging_window_millis,
                    );
                let right_score = right.ticket.priority.rank().saturating_add(
                    now.elapsed_since(right.enqueued_at) / self.aging_window_millis,
                );
                right_score
                    .cmp(&left_score)
                    .then_with(|| left.sequence.cmp(&right.sequence))
            });
            let Some(index) = candidates.first().copied() else {
                break;
            };
            let queued = self.queued.remove(index);
            let ticket = queued.ticket;
            self.active.insert(
                ticket.identity.clone(),
                ActiveJob {
                    ticket: ticket.clone(),
                    admitted_at: now,
                    last_heartbeat: now,
                    last_progress: now,
                    progress: None,
                    escalation_at: None,
                    state: EscalationState::Starting,
                },
            );
            actions.push(BrokerAction::Start(ticket));
        }
        actions
    }

    /// Reclaims advisory analysis capacity when realtime audio starts or an
    /// audition/render becomes pending. Lowest-priority, newest analysis work
    /// yields first; resources remain charged until the worker acknowledges a
    /// terminal record or is reaped.
    pub fn protect_foreground(
        &mut self,
        now: BrokerTick,
        pressure: ForegroundPressure,
    ) -> Vec<BrokerAction> {
        let maximum_cpu = self.capacity.cpu_slots.saturating_sub(
            (if pressure.realtime_audio_active {
                self.capacity.realtime_cpu_reserve
            } else {
                0
            })
            .saturating_add(if pressure.render_work_pending {
                self.capacity.render_cpu_reserve
            } else {
                0
            }),
        );
        let maximum_memory = self.capacity.memory_bytes.saturating_sub(
            (if pressure.realtime_audio_active {
                self.capacity.realtime_memory_reserve
            } else {
                0
            })
            .saturating_add(if pressure.render_work_pending {
                self.capacity.render_memory_reserve
            } else {
                0
            }),
        );
        let mut usage = self.resource_usage();
        if usage.cpu_slots <= maximum_cpu && usage.memory_bytes <= maximum_memory {
            return Vec::new();
        }
        let mut candidates: Vec<JobIdentity> = self
            .active
            .iter()
            .filter_map(|(identity, active)| {
                matches!(
                    active.state,
                    EscalationState::Starting | EscalationState::Running
                )
                .then_some(identity.clone())
            })
            .collect();
        candidates.sort_by(|left, right| {
            let left_job = &self.active[left];
            let right_job = &self.active[right];
            left_job
                .ticket
                .priority
                .cmp(&right_job.ticket.priority)
                // Newer work yields before an older job of equal priority.
                .then_with(|| right_job.admitted_at.cmp(&left_job.admitted_at))
                .then_with(|| left.cmp(right))
        });

        let mut actions = Vec::new();
        for identity in candidates {
            if usage.cpu_slots <= maximum_cpu && usage.memory_bytes <= maximum_memory {
                break;
            }
            let active = self.active.get_mut(&identity).expect("candidate is active");
            usage.cpu_slots = usage
                .cpu_slots
                .saturating_sub(active.ticket.demand.cpu_slots);
            usage.memory_bytes = usage
                .memory_bytes
                .saturating_sub(active.ticket.demand.memory_bytes);
            active.escalation_at = Some(now);
            let action = match active.state {
                EscalationState::Starting => {
                    active.state = EscalationState::KillRequested {
                        reason: CancellationReason::ForegroundPressure,
                    };
                    BrokerAction::Kill {
                        identity,
                        reason: CancellationReason::ForegroundPressure,
                    }
                }
                EscalationState::Running => {
                    active.state = EscalationState::CancelRequested {
                        reason: CancellationReason::ForegroundPressure,
                    };
                    BrokerAction::SendCancel {
                        identity,
                        reason: CancellationReason::ForegroundPressure,
                    }
                }
                EscalationState::CancelRequested { .. } | EscalationState::KillRequested { .. } => {
                    continue
                }
            };
            actions.push(action);
        }
        actions
    }

    pub fn observe_started(
        &mut self,
        identity: &JobIdentity,
        now: BrokerTick,
    ) -> Result<(), ObservationRefusal> {
        let active = self
            .active
            .get_mut(identity)
            .ok_or(ObservationRefusal::UnknownOrStaleJob)?;
        if active.state != EscalationState::Starting {
            return Err(ObservationRefusal::InvalidState);
        }
        active.state = EscalationState::Running;
        active.last_heartbeat = now;
        active.last_progress = now;
        Ok(())
    }

    pub fn observe_heartbeat(
        &mut self,
        identity: &JobIdentity,
        now: BrokerTick,
    ) -> Result<(), ObservationRefusal> {
        let active = self.running_mut(identity)?;
        active.last_heartbeat = now;
        Ok(())
    }

    /// Schema-v1 adapters call this for each Progress record. It refreshes
    /// both liveness clocks because schema-v1 has no distinct heartbeat.
    pub fn observe_progress(
        &mut self,
        identity: &JobIdentity,
        now: BrokerTick,
        progress: ProgressPoint,
    ) -> Result<(), ObservationRefusal> {
        if progress.total == 0 || progress.completed > progress.total {
            return Err(ObservationRefusal::RegressingProgress);
        }
        let active = self.running_mut(identity)?;
        if let Some(previous) = active.progress {
            if progress.phase < previous.phase
                || (progress.phase == previous.phase
                    && (progress.total != previous.total
                        || progress.completed < previous.completed))
            {
                return Err(ObservationRefusal::RegressingProgress);
            }
        }
        active.progress = Some(progress);
        active.last_progress = now;
        active.last_heartbeat = now;
        Ok(())
    }

    pub fn request_cancel(
        &mut self,
        identity: &JobIdentity,
        now: BrokerTick,
        reason: CancellationReason,
    ) -> Result<Option<BrokerAction>, ObservationRefusal> {
        if let Some(index) = self
            .queued
            .iter()
            .position(|job| &job.ticket.identity == identity)
        {
            let queued = self.queued.remove(index);
            self.terminal_without_receipt.insert(queued.ticket.identity);
            return Ok(None);
        }
        let active = self
            .active
            .get_mut(identity)
            .ok_or(ObservationRefusal::UnknownOrStaleJob)?;
        match active.state {
            EscalationState::Starting => {
                active.state = EscalationState::KillRequested { reason };
                active.escalation_at = Some(now);
                Ok(Some(BrokerAction::Kill {
                    identity: identity.clone(),
                    reason,
                }))
            }
            EscalationState::Running => {
                active.state = EscalationState::CancelRequested { reason };
                active.escalation_at = Some(now);
                Ok(Some(BrokerAction::SendCancel {
                    identity: identity.clone(),
                    reason,
                }))
            }
            EscalationState::CancelRequested { .. } | EscalationState::KillRequested { .. } => {
                Ok(None)
            }
        }
    }

    pub fn acknowledge_terminal(
        &mut self,
        identity: &JobIdentity,
    ) -> Result<(), ObservationRefusal> {
        let active = self
            .active
            .remove(identity)
            .ok_or(ObservationRefusal::UnknownOrStaleJob)?;
        self.terminal_without_receipt.insert(active.ticket.identity);
        Ok(())
    }

    pub fn tick(&mut self, now: BrokerTick) -> Vec<BrokerAction> {
        let mut actions = Vec::new();
        for (identity, active) in &mut self.active {
            let action = match active.state {
                EscalationState::Starting
                    if now.elapsed_since(active.admitted_at) >= self.deadlines_millis.startup =>
                {
                    active.state = EscalationState::KillRequested {
                        reason: CancellationReason::StartupDeadline,
                    };
                    active.escalation_at = Some(now);
                    Some(BrokerAction::Kill {
                        identity: identity.clone(),
                        reason: CancellationReason::StartupDeadline,
                    })
                }
                EscalationState::Running
                    if now.elapsed_since(active.last_heartbeat)
                        >= self.deadlines_millis.heartbeat =>
                {
                    active.state = EscalationState::CancelRequested {
                        reason: CancellationReason::HeartbeatDeadline,
                    };
                    active.escalation_at = Some(now);
                    Some(BrokerAction::SendCancel {
                        identity: identity.clone(),
                        reason: CancellationReason::HeartbeatDeadline,
                    })
                }
                EscalationState::Running
                    if now.elapsed_since(active.last_progress)
                        >= self.deadlines_millis.progress =>
                {
                    active.state = EscalationState::CancelRequested {
                        reason: CancellationReason::ProgressDeadline,
                    };
                    active.escalation_at = Some(now);
                    Some(BrokerAction::SendCancel {
                        identity: identity.clone(),
                        reason: CancellationReason::ProgressDeadline,
                    })
                }
                EscalationState::CancelRequested { reason }
                    if now.elapsed_since(active.escalation_at.unwrap_or(now))
                        >= self.deadlines_millis.cancel =>
                {
                    active.state = EscalationState::KillRequested { reason };
                    active.escalation_at = Some(now);
                    Some(BrokerAction::Kill {
                        identity: identity.clone(),
                        reason,
                    })
                }
                EscalationState::KillRequested { reason }
                    if now.elapsed_since(active.escalation_at.unwrap_or(now))
                        >= self.deadlines_millis.kill =>
                {
                    active.escalation_at = Some(now);
                    Some(BrokerAction::DeclareUnresponsive {
                        identity: identity.clone(),
                        reason,
                    })
                }
                _ => None,
            };
            if let Some(action) = action {
                actions.push(action)
            }
        }
        actions
    }

    pub fn accept_completion(
        &mut self,
        attempt: CompletionAttempt,
        now: BrokerTick,
    ) -> Result<CompletionReceipt, CompletionRefusal> {
        let latest = self
            .latest_generation
            .get(attempt.identity.job_id())
            .copied();
        if let Some(latest) = latest {
            if attempt.identity.generation() != latest {
                return Err(CompletionRefusal::StaleGeneration {
                    latest,
                    received: attempt.identity.generation(),
                });
            }
        } else {
            return Err(CompletionRefusal::UnknownJob);
        }
        let Some(active) = self.active.get(&attempt.identity) else {
            if self
                .active
                .keys()
                .any(|identity| identity.job_id() == attempt.identity.job_id())
            {
                return Err(CompletionRefusal::IdentityMismatch);
            }
            return Err(CompletionRefusal::UnknownJob);
        };
        if !matches!(active.state, EscalationState::Running) {
            return Err(CompletionRefusal::CancelledOrKilling);
        }
        if attempt.output.total_bytes > active.ticket.demand.expected_output_bytes
            || attempt.output.total_bytes > self.maximum_output_bytes
        {
            return Err(CompletionRefusal::Output(OutputRefusal::TotalTooLarge {
                declared: attempt.output.total_bytes,
                maximum: active
                    .ticket
                    .demand
                    .expected_output_bytes
                    .min(self.maximum_output_bytes),
            }));
        }
        if !valid_hash(&attempt.result_sha256) {
            return Err(CompletionRefusal::IdentityMismatch);
        }
        self.active.remove(&attempt.identity);
        let receipt = CompletionReceipt {
            identity: attempt.identity.clone(),
            result_sha256: attempt.result_sha256,
            output: attempt.output,
            completed_at: now,
            receipt_sequence: self.next_receipt_sequence,
        };
        self.next_receipt_sequence = self.next_receipt_sequence.saturating_add(1);
        self.completed.insert(attempt.identity, receipt.clone());
        Ok(receipt)
    }

    pub fn state(&self, identity: &JobIdentity) -> Option<EscalationState> {
        self.active.get(identity).map(|job| job.state)
    }

    pub fn completion(&self, identity: &JobIdentity) -> Option<&CompletionReceipt> {
        self.completed.get(identity)
    }

    pub fn queued_count(&self) -> usize {
        self.queued.len()
    }
    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    fn running_mut(
        &mut self,
        identity: &JobIdentity,
    ) -> Result<&mut ActiveJob, ObservationRefusal> {
        let active = self
            .active
            .get_mut(identity)
            .ok_or(ObservationRefusal::UnknownOrStaleJob)?;
        if active.state != EscalationState::Running {
            return Err(ObservationRefusal::InvalidState);
        }
        Ok(active)
    }

    fn resource_usage(&self) -> ResourceUsage {
        let mut usage = ResourceUsage::default();
        for active in self.active.values() {
            let _ = active.ticket.demand.checked_add_to(&mut usage);
        }
        usage
    }

    fn fits(
        &self,
        demand: &ResourceDemand,
        usage: &ResourceUsage,
        pressure: ForegroundPressure,
    ) -> bool {
        if self.active.len() >= usize::from(self.capacity.worker_slots) {
            return false;
        }
        let cpu_reserve = if pressure.realtime_audio_active {
            self.capacity.realtime_cpu_reserve
        } else {
            0
        }
        .saturating_add(if pressure.render_work_pending {
            self.capacity.render_cpu_reserve
        } else {
            0
        });
        let memory_reserve = if pressure.realtime_audio_active {
            self.capacity.realtime_memory_reserve
        } else {
            0
        }
        .saturating_add(if pressure.render_work_pending {
            self.capacity.render_memory_reserve
        } else {
            0
        });
        usage.cpu_slots.saturating_add(demand.cpu_slots)
            <= self.capacity.cpu_slots.saturating_sub(cpu_reserve)
            && usage.memory_bytes.saturating_add(demand.memory_bytes)
                <= self.capacity.memory_bytes.saturating_sub(memory_reserve)
            && usage.scratch_bytes.saturating_add(demand.scratch_bytes)
                <= self.capacity.scratch_bytes
            && demand.accelerator.as_ref().is_none_or(|accelerator| {
                usage.accelerators.get(accelerator).copied().unwrap_or(0)
                    < self
                        .capacity
                        .accelerators
                        .get(accelerator)
                        .copied()
                        .unwrap_or(0)
            })
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 160
        && !value.contains(char::is_whitespace)
        && !value.contains(['/', '\\', '\0'])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: char) -> String {
        std::iter::repeat_n(byte, 64).collect()
    }

    fn policy() -> RuntimePolicy {
        RuntimePolicy {
            deadlines: WorkerDeadlines {
                startup: Duration::from_millis(10),
                request: Duration::from_millis(10),
                heartbeat: Duration::from_millis(20),
                progress: Duration::from_millis(30),
                cancel_grace: Duration::from_millis(5),
                kill_grace: Duration::from_millis(4),
            },
            output: WorkerOutputPolicy {
                maximum_total_output_bytes: 1_000,
                maximum_artifact_bytes: 1_000,
                ..WorkerOutputPolicy::default()
            },
        }
    }

    fn capacity() -> BrokerCapacity {
        BrokerCapacity {
            cpu_slots: 8,
            memory_bytes: 8_000,
            scratch_bytes: 20_000,
            worker_slots: 4,
            accelerators: BTreeMap::from([("mps".into(), 1)]),
            realtime_cpu_reserve: 2,
            realtime_memory_reserve: 1_000,
            render_cpu_reserve: 2,
            render_memory_reserve: 1_000,
        }
    }

    fn ticket(id: &str, generation: u64, priority: JobPriority, cpu: u16) -> JobTicket {
        JobTicket {
            identity: JobIdentity::new(id, generation, hash('a'), hash('b')).unwrap(),
            priority,
            demand: ResourceDemand {
                cpu_slots: cpu,
                memory_bytes: 1_000,
                scratch_bytes: 1_000,
                expected_output_bytes: 900,
                accelerator: None,
            },
        }
    }

    #[test]
    fn admission_preserves_realtime_and_render_reservations() {
        let mut broker =
            WorkerBroker::new(capacity(), policy(), Duration::from_millis(100)).unwrap();
        broker
            .submit(
                ticket("a", 1, JobPriority::Interactive, 3),
                BrokerTick::ZERO,
            )
            .unwrap();
        broker
            .submit(
                ticket("b", 1, JobPriority::Interactive, 3),
                BrokerTick::ZERO,
            )
            .unwrap();
        let actions = broker.schedule(
            BrokerTick::ZERO,
            ForegroundPressure {
                realtime_audio_active: true,
                render_work_pending: true,
            },
        );
        assert_eq!(actions.len(), 1);
        assert_eq!(broker.active_count(), 1);
        assert_eq!(broker.queued_count(), 1);
    }

    #[test]
    fn new_foreground_pressure_preempts_background_before_interactive_work() {
        let mut broker =
            WorkerBroker::new(capacity(), policy(), Duration::from_millis(100)).unwrap();
        let background = ticket("background", 1, JobPriority::Background, 3);
        let interactive = ticket("interactive", 1, JobPriority::Interactive, 3);
        let background_id = background.identity.clone();
        let interactive_id = interactive.identity.clone();
        broker
            .submit(background, BrokerTick::from_millis(1))
            .unwrap();
        broker
            .submit(interactive, BrokerTick::from_millis(2))
            .unwrap();
        broker.schedule(BrokerTick::from_millis(2), ForegroundPressure::default());
        broker
            .observe_started(&background_id, BrokerTick::from_millis(2))
            .unwrap();
        broker
            .observe_started(&interactive_id, BrokerTick::from_millis(2))
            .unwrap();
        let actions = broker.protect_foreground(
            BrokerTick::from_millis(3),
            ForegroundPressure {
                realtime_audio_active: true,
                render_work_pending: true,
            },
        );
        assert!(matches!(
            actions.as_slice(),
            [BrokerAction::SendCancel { identity, .. }]
                if identity == &background_id
        ));
        assert_eq!(
            broker.state(&interactive_id),
            Some(EscalationState::Running)
        );
    }

    #[test]
    fn aging_eventually_places_old_background_work_first() {
        let mut broker =
            WorkerBroker::new(capacity(), policy(), Duration::from_millis(10)).unwrap();
        broker
            .submit(
                ticket("old", 1, JobPriority::Background, 8),
                BrokerTick::ZERO,
            )
            .unwrap();
        broker
            .submit(
                ticket("new", 1, JobPriority::Interactive, 8),
                BrokerTick::from_millis(30),
            )
            .unwrap();
        let actions = broker.schedule(BrokerTick::from_millis(30), ForegroundPressure::default());
        let BrokerAction::Start(job) = &actions[0] else {
            panic!("expected start")
        };
        assert_eq!(job.identity.job_id(), "old");
    }

    #[test]
    fn deadline_escalation_is_cancel_then_kill_then_unresponsive() {
        let mut broker =
            WorkerBroker::new(capacity(), policy(), Duration::from_millis(100)).unwrap();
        let ticket = ticket("job", 1, JobPriority::Interactive, 1);
        let identity = ticket.identity.clone();
        broker.submit(ticket, BrokerTick::ZERO).unwrap();
        broker.schedule(BrokerTick::ZERO, ForegroundPressure::default());
        broker.observe_started(&identity, BrokerTick::ZERO).unwrap();
        assert!(broker.tick(BrokerTick::from_millis(19)).is_empty());
        assert!(matches!(
            broker.tick(BrokerTick::from_millis(20)).as_slice(),
            [BrokerAction::SendCancel {
                reason: CancellationReason::HeartbeatDeadline,
                ..
            }]
        ));
        assert!(matches!(
            broker.tick(BrokerTick::from_millis(25)).as_slice(),
            [BrokerAction::Kill {
                reason: CancellationReason::HeartbeatDeadline,
                ..
            }]
        ));
        assert!(matches!(
            broker.tick(BrokerTick::from_millis(29)).as_slice(),
            [BrokerAction::DeclareUnresponsive { .. }]
        ));
    }

    #[test]
    fn progress_is_schema_v1_heartbeat_and_cannot_regress() {
        let mut broker =
            WorkerBroker::new(capacity(), policy(), Duration::from_millis(100)).unwrap();
        let ticket = ticket("job", 1, JobPriority::Interactive, 1);
        let identity = ticket.identity.clone();
        broker.submit(ticket, BrokerTick::ZERO).unwrap();
        broker.schedule(BrokerTick::ZERO, ForegroundPressure::default());
        broker.observe_started(&identity, BrokerTick::ZERO).unwrap();
        broker
            .observe_progress(
                &identity,
                BrokerTick::from_millis(15),
                ProgressPoint {
                    phase: 1,
                    completed: 1,
                    total: 4,
                },
            )
            .unwrap();
        assert!(broker.tick(BrokerTick::from_millis(34)).is_empty());
        assert_eq!(
            broker.observe_progress(
                &identity,
                BrokerTick::from_millis(35),
                ProgressPoint {
                    phase: 1,
                    completed: 0,
                    total: 4
                },
            ),
            Err(ObservationRefusal::RegressingProgress)
        );
    }

    #[test]
    fn immutable_receipt_refuses_stale_generation() {
        let mut broker =
            WorkerBroker::new(capacity(), policy(), Duration::from_millis(100)).unwrap();
        let first = ticket("job", 1, JobPriority::Interactive, 1);
        let first_identity = first.identity.clone();
        broker.submit(first, BrokerTick::ZERO).unwrap();
        broker.schedule(BrokerTick::ZERO, ForegroundPressure::default());
        broker
            .observe_started(&first_identity, BrokerTick::ZERO)
            .unwrap();
        let receipt = broker
            .accept_completion(
                CompletionAttempt {
                    identity: first_identity.clone(),
                    result_sha256: hash('c'),
                    output: OutputSummary {
                        artifact_count: 1,
                        measurement_count: 0,
                        total_bytes: 10,
                    },
                },
                BrokerTick::from_millis(1),
            )
            .unwrap();
        assert_eq!(receipt.receipt_sequence(), 0);
        assert_eq!(broker.completion(&first_identity), Some(&receipt));

        let second = ticket("job", 2, JobPriority::Interactive, 1);
        let second_identity = second.identity.clone();
        broker.submit(second, BrokerTick::from_millis(2)).unwrap();
        broker.schedule(BrokerTick::from_millis(2), ForegroundPressure::default());
        broker
            .observe_started(&second_identity, BrokerTick::from_millis(2))
            .unwrap();
        let refusal = broker
            .accept_completion(
                CompletionAttempt {
                    identity: first_identity,
                    result_sha256: hash('d'),
                    output: OutputSummary {
                        artifact_count: 1,
                        measurement_count: 0,
                        total_bytes: 10,
                    },
                },
                BrokerTick::from_millis(3),
            )
            .unwrap_err();
        assert_eq!(
            refusal,
            CompletionRefusal::StaleGeneration {
                latest: 2,
                received: 1
            }
        );
    }
}
