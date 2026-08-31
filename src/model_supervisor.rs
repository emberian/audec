//! GPUI-independent coordination for model-worker sessions.
//!
//! Process spawning and pipe pumping are intentionally adapters around this
//! state core. The core owns the session validator, cache leases, and the
//! rule that a completed wire result is not visible until ModelStore publishes
//! it atomically.

use std::collections::BTreeMap;
use std::fmt;

use crate::model_store::{
    CacheAcquire, CacheLease, JobSandbox, ModelStore, StoreError, StoredResult,
};
use crate::model_wire::{
    AnalyzeRequest, SessionValidator, WireEnvelope, WireError, WireMessage, WorkerFailure,
    WorkerFailureKind,
};

#[derive(Debug)]
pub struct ModelSupervisor {
    session: SessionValidator,
    active: BTreeMap<String, ActiveJob>,
}

#[derive(Debug)]
struct ActiveJob {
    lease: CacheLease,
    cancellation_requested: bool,
}

#[derive(Debug)]
pub enum BeginJob {
    CacheHit(StoredResult),
    Started { sandbox: JobSandbox },
    Busy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SupervisorFailure {
    Worker(WorkerFailure),
    Crash { detail: String },
    OutOfMemory { detail: String },
    Protocol { detail: String },
}

impl ModelSupervisor {
    pub fn new() -> Self {
        Self {
            session: SessionValidator::default(),
            active: BTreeMap::new(),
        }
    }

    /// Acquires a cache single-flight lease before an `Analyze` record is
    /// sent. The caller writes input files into the returned sandbox, then
    /// fills `AnalyzeRequest.files` with sandbox-relative paths.
    pub fn begin_job(
        &mut self,
        store: &ModelStore,
        request: &AnalyzeRequest,
        cache_key: &str,
    ) -> Result<BeginJob, SupervisorError> {
        self.reserve_job(store, &request.job_id, cache_key)
    }

    /// Reserves cache ownership before bulk input is copied into the sandbox.
    /// Sending the later `Analyze` record still establishes the protocol job.
    pub fn reserve_job(
        &mut self,
        store: &ModelStore,
        job_id: &str,
        cache_key: &str,
    ) -> Result<BeginJob, SupervisorError> {
        if self.active.contains_key(job_id) {
            return Err(SupervisorError::State(
                "job ID already active in supervisor".into(),
            ));
        }
        match store.acquire(job_id, cache_key)? {
            CacheAcquire::Hit(result) => Ok(BeginJob::CacheHit(result)),
            CacheAcquire::Busy { .. } => Ok(BeginJob::Busy),
            CacheAcquire::Acquired(lease) => {
                let sandbox = lease.sandbox().clone();
                self.active.insert(
                    job_id.to_owned(),
                    ActiveJob {
                        lease,
                        cancellation_requested: false,
                    },
                );
                Ok(BeginJob::Started { sandbox })
            }
        }
    }

    /// Validates and records a controller→worker record before a launcher
    /// writes it to stdin. This is intentionally separate from pipe I/O.
    pub fn observe_controller(&mut self, envelope: &WireEnvelope) -> Result<(), SupervisorError> {
        match &envelope.message {
            WireMessage::Analyze { request } | WireMessage::Separate { request } => {
                let job = self.active.get(&request.job_id).ok_or_else(|| {
                    SupervisorError::State("cannot analyze without a reserved cache lease".into())
                })?;
                if request.cache_key != job.lease.cache_key() {
                    return Err(SupervisorError::State(
                        "analysis request cache key does not match its reserved lease".into(),
                    ));
                }
                let expected_staging = job
                    .lease
                    .sandbox()
                    .worker_relative(job.lease.sandbox().staging_directory())
                    .map_err(SupervisorError::Store)?;
                if request.files.staging_directory != expected_staging {
                    return Err(SupervisorError::State(
                        "analysis request may write only its reserved staging directory".into(),
                    ));
                }
            }
            WireMessage::Cancel { job_id } => {
                let job = self.active.get_mut(job_id).ok_or_else(|| {
                    SupervisorError::State("cannot cancel a non-active job".into())
                })?;
                job.cancellation_requested = true;
            }
            _ => {}
        }
        self.session.observe_controller(envelope)?;
        Ok(())
    }

    /// Validates a worker record. A `Complete` is synchronously verified and
    /// atomically published; cancellation/error terminal records release only
    /// the lock and retain staging for diagnosis/recovery.
    pub fn observe_worker(
        &mut self,
        store: &ModelStore,
        envelope: &WireEnvelope,
    ) -> Result<Option<SupervisorEvent>, SupervisorError> {
        self.observe_worker_protocol(envelope)?;
        match &envelope.message {
            WireMessage::Complete { result } => {
                let active = self.active.remove(&result.job_id).ok_or_else(|| {
                    SupervisorError::State("worker completed a job without a cache lease".into())
                })?;
                let published = active.lease.publish(store, result.clone())?;
                Ok(Some(SupervisorEvent::Published(published)))
            }
            WireMessage::Cancelled { job_id } => {
                let active = self.active.remove(job_id).ok_or_else(|| {
                    SupervisorError::State("worker cancelled a job without a cache lease".into())
                })?;
                active.lease.abandon();
                Ok(Some(SupervisorEvent::Cancelled {
                    job_id: job_id.clone(),
                }))
            }
            WireMessage::Error { error } => {
                let active = self.active.remove(&error.job_id).ok_or_else(|| {
                    SupervisorError::State("worker failed a job without a cache lease".into())
                })?;
                active.lease.abandon();
                Ok(Some(SupervisorEvent::Failed(SupervisorFailure::Worker(
                    error.clone(),
                ))))
            }
            _ => Ok(None),
        }
    }

    /// Handshake/load responses do not touch a cache lease, but they must use
    /// the same sequencing and state validator as job responses.
    pub fn observe_worker_protocol(
        &mut self,
        envelope: &WireEnvelope,
    ) -> Result<(), SupervisorError> {
        self.session.observe_worker(envelope)?;
        Ok(())
    }

    /// Called by a launcher when stdout closes, the child exits, or a process
    /// watchdog has escalated cancellation. This never publishes partial
    /// output; every active cache lease is abandoned with its staging intact.
    pub fn worker_terminated(&mut self, failure: SupervisorFailure) -> Vec<SupervisorEvent> {
        let mut events = Vec::new();
        for (job_id, active) in std::mem::take(&mut self.active) {
            active.lease.abandon();
            events.push(SupervisorEvent::JobTerminated {
                job_id,
                failure: failure.clone(),
            });
        }
        events
    }

    pub fn active_job_count(&self) -> usize {
        self.active.len()
    }
    pub fn cancellation_requested(&self, job_id: &str) -> bool {
        self.active
            .get(job_id)
            .is_some_and(|job| job.cancellation_requested)
    }
}

impl Default for ModelSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum SupervisorEvent {
    Published(StoredResult),
    Cancelled {
        job_id: String,
    },
    Failed(SupervisorFailure),
    JobTerminated {
        job_id: String,
        failure: SupervisorFailure,
    },
}

#[derive(Debug)]
pub enum SupervisorError {
    Store(StoreError),
    Wire(WireError),
    State(String),
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => error.fmt(f),
            Self::Wire(error) => error.fmt(f),
            Self::State(detail) => f.write_str(detail),
        }
    }
}
impl std::error::Error for SupervisorError {}
impl From<StoreError> for SupervisorError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}
impl From<WireError> for SupervisorError {
    fn from(error: WireError) -> Self {
        Self::Wire(error)
    }
}

// Kept as a named constructor so a future process launcher does not collapse
// an OOM exit into an untyped adapter failure.
pub fn crash_failure(detail: impl Into<String>, likely_oom: bool) -> SupervisorFailure {
    let detail = detail.into();
    if likely_oom {
        SupervisorFailure::OutOfMemory { detail }
    } else {
        SupervisorFailure::Crash { detail }
    }
}

pub fn protocol_failure(error: impl Into<String>) -> SupervisorFailure {
    SupervisorFailure::Protocol {
        detail: error.into(),
    }
}

pub fn invalid_output_failure(
    job_id: impl Into<String>,
    detail: impl Into<String>,
) -> SupervisorFailure {
    SupervisorFailure::Worker(WorkerFailure {
        job_id: job_id.into(),
        kind: WorkerFailureKind::InvalidOutput,
        detail: detail.into(),
    })
}
