//! Executable controller for one isolated model-worker child process.
//!
//! This layer owns pipes and process lifetime, while `model_supervisor` owns
//! protocol/cache state and `model_store` owns publication. It has no GPUI
//! dependency, so the same runtime can later serve the CLI.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[path = "worker_broker.rs"]
pub mod broker;

pub use broker::{RuntimePolicy, WorkerDeadlines, WorkerOutputPolicy};

use crate::model_claim::{
    ClaimError, ClaimSource, ModelClaimBundle, ModelLabel, WorkerRuntimeProvenance,
};
use crate::model_registry::{InstallStatus, ModelRegistration, ModelRegistry, RegistryError};
use crate::model_store::{JobSandbox, ModelStore, StoreError, StoredResult};
use crate::model_supervisor::{
    crash_failure, BeginJob, ModelSupervisor, SupervisorError, SupervisorEvent, SupervisorFailure,
};
use crate::model_wire::{
    AnalyzeRequest, JobFiles, WireCapabilities, WireEnvelope, WireError, WireMessage,
};
use crate::model_worker::WorkerCapabilities;

#[derive(Clone, Debug)]
pub struct WorkerLaunch {
    pub program: PathBuf,
    pub arguments: Vec<String>,
    /// A launch path is selected by the application, but the handshake still
    /// verifies this exact expected name before any model load/request.
    pub expected_worker_name: String,
}

#[derive(Debug)]
pub struct WorkerRuntime {
    child: Child,
    input: ChildStdin,
    output: Option<mpsc::Receiver<PumpEvent>>,
    output_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    supervisor: ModelSupervisor,
    capabilities: WireCapabilities,
    next_controller_sequence: u64,
    claim_publications: BTreeMap<String, ClaimPublication>,
    policy: RuntimePolicy,
    cancellation_started: Option<Instant>,
    process_reaped: bool,
}

#[derive(Clone, Debug)]
pub enum RuntimeReservation {
    CacheHit(StoredResult),
    Busy,
    Reserved(JobSandbox),
}

#[derive(Debug)]
pub enum RuntimeEvent {
    Progress(crate::model_wire::ProgressReport),
    Published(StoredResult),
    ClaimPublished {
        stored: StoredResult,
        claim: ModelClaimBundle,
    },
    Cancelled {
        job_id: String,
    },
    Failed(SupervisorFailure),
    JobTerminated {
        job_id: String,
        failure: SupervisorFailure,
    },
}

#[derive(Clone, Debug)]
pub struct ClaimPublication {
    pub model_manifest_sha256: String,
    pub source: ClaimSource,
    pub runtime: WorkerRuntimeProvenance,
    pub additivity: crate::model_wire::AdditivityDeclaration,
    pub outputs: Vec<(String, Vec<ModelLabel>)>,
}

impl WorkerRuntime {
    /// Spawn a child rooted at the supervisor-owned cache parent, perform the
    /// mandatory hello/capabilities exchange, and reject a name mismatch.
    pub fn launch(store: &ModelStore, launch: WorkerLaunch) -> Result<Self, RuntimeError> {
        Self::launch_with_policy(store, launch, RuntimePolicy::default())
    }

    /// Launch with explicit process deadlines and protocol-visible resource
    /// limits. These are controller-side bounds; an OS sandbox/cgroup adapter
    /// must still enforce hard RSS/CPU limits described by broker admission.
    pub fn launch_with_policy(
        store: &ModelStore,
        launch: WorkerLaunch,
        policy: RuntimePolicy,
    ) -> Result<Self, RuntimeError> {
        policy
            .validate()
            .map_err(|error| RuntimeError::Protocol(error.to_string()))?;
        store.ensure_layout()?;
        let working_root = store.root().join("cache");
        let mut child = Command::new(&launch.program)
            .args(&launch.arguments)
            .current_dir(&working_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| RuntimeError::Launch {
                program: launch.program.clone(),
                error,
            })?;
        let Some(input) = child.stdin.take() else {
            terminate_partially_launched(&mut child, policy.deadlines.kill_grace);
            return Err(RuntimeError::MissingPipe("stdin"));
        };
        let Some(output) = child.stdout.take() else {
            terminate_partially_launched(&mut child, policy.deadlines.kill_grace);
            return Err(RuntimeError::MissingPipe("stdout"));
        };
        let Some(stderr) = child.stderr.take() else {
            terminate_partially_launched(&mut child, policy.deadlines.kill_grace);
            return Err(RuntimeError::MissingPipe("stderr"));
        };
        let (output_sender, output_receiver) =
            mpsc::sync_channel(policy.output.maximum_buffered_control_records);
        let maximum_control_line_bytes = policy.output.maximum_control_line_bytes;
        let output_thread = thread::Builder::new()
            .name("audec-model-control-reader".into())
            .spawn(move || control_reader(output, output_sender, maximum_control_line_bytes))
            .map_err(|error| RuntimeError::Io {
                action: "spawn model worker output pump",
                path: PathBuf::new(),
                error,
            })?;
        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        let stderr_for_thread = Arc::clone(&stderr_tail);
        let maximum_log_tail_bytes = policy.output.maximum_log_tail_bytes;
        let stderr_thread = thread::Builder::new()
            .name("audec-model-stderr-reader".into())
            .spawn(move || stderr_reader(stderr, stderr_for_thread, maximum_log_tail_bytes))
            .map_err(|error| RuntimeError::Io {
                action: "spawn model worker log pump",
                path: PathBuf::new(),
                error,
            })?;
        let mut runtime = Self {
            child,
            input,
            output: Some(output_receiver),
            output_thread: Some(output_thread),
            stderr_thread: Some(stderr_thread),
            stderr_tail,
            supervisor: ModelSupervisor::new(),
            capabilities: WireCapabilities {
                worker_name: String::new(),
                backends: BTreeSet::new(),
                maximum_parallel_jobs: 1,
                shared_memory: false,
            },
            next_controller_sequence: 0,
            claim_publications: BTreeMap::new(),
            policy,
            cancellation_started: None,
            process_reaped: false,
        };
        runtime.send(WireMessage::Hello)?;
        let response = runtime
            .read_envelope_with_timeout(runtime.policy.deadlines.startup, "startup handshake")?;
        runtime.supervisor.observe_worker_protocol(&response)?;
        let WireMessage::Capabilities { capabilities } = response.message else {
            return Err(RuntimeError::Protocol(
                "worker did not answer hello with capabilities".into(),
            ));
        };
        if capabilities.worker_name != launch.expected_worker_name {
            return Err(RuntimeError::Protocol(format!(
                "worker name {}, expected {}",
                capabilities.worker_name, launch.expected_worker_name
            )));
        }
        runtime.capabilities = capabilities;
        Ok(runtime)
    }

    /// Bounded tail of worker stderr. Logs are diagnostic only and never
    /// participate in cache identity or completion validity.
    pub fn stderr_tail(&self) -> String {
        let bytes = self
            .stderr_tail
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        String::from_utf8_lossy(&bytes).into_owned()
    }

    pub const fn policy(&self) -> RuntimePolicy {
        self.policy
    }

    pub fn capabilities(&self) -> &WireCapabilities {
        &self.capabilities
    }

    /// Registry verification happens before load. The worker sees only the
    /// canonical manifest hash; a later launcher can add an OS sandbox grant
    /// for the verified install directory without changing this protocol.
    pub fn load_registered(
        &mut self,
        registry: &ModelRegistry,
        registration: &ModelRegistration,
    ) -> Result<(), RuntimeError> {
        let InstallStatus::Installed { manifest_sha256 } = registry.verify(registration)? else {
            return Err(RuntimeError::ModelUnavailable(
                registration.model_id().into(),
            ));
        };
        let capabilities = WorkerCapabilities {
            worker_name: self.capabilities.worker_name.clone(),
            backends: self.capabilities.backends.iter().cloned().collect(),
            maximum_parallel_jobs: self.capabilities.maximum_parallel_jobs,
            shared_memory: self.capabilities.shared_memory,
        };
        if registration.compatible_workers(&capabilities).is_empty() {
            return Err(RuntimeError::Protocol(format!(
                "worker {} cannot run registered model {}",
                capabilities.worker_name,
                registration.model_id()
            )));
        }
        let hash = manifest_sha256.to_string();
        self.send(WireMessage::LoadModel {
            manifest_sha256: hash.clone(),
        })?;
        let response =
            self.read_envelope_with_timeout(self.policy.deadlines.request, "model load")?;
        self.supervisor.observe_worker_protocol(&response)?;
        match response.message {
            WireMessage::ModelLoaded { manifest_sha256 } if manifest_sha256 == hash => Ok(()),
            _ => Err(RuntimeError::Protocol(
                "worker did not acknowledge the requested manifest".into(),
            )),
        }
    }

    /// Reserve cache ownership before copying material. A caller may use
    /// `write_input_bytes` for small fixtures or stream/copy large PCM into
    /// the returned `input_directory` itself before calling `analyze`.
    pub fn reserve(
        &mut self,
        store: &ModelStore,
        job_id: &str,
        cache_key: &str,
    ) -> Result<RuntimeReservation, RuntimeError> {
        if self.supervisor.active_job_count()
            >= usize::from(self.capabilities.maximum_parallel_jobs)
        {
            return Ok(RuntimeReservation::Busy);
        }
        match self.supervisor.reserve_job(store, job_id, cache_key)? {
            BeginJob::CacheHit(result) => Ok(RuntimeReservation::CacheHit(result)),
            BeginJob::Busy => Ok(RuntimeReservation::Busy),
            BeginJob::Started { sandbox } => Ok(RuntimeReservation::Reserved(sandbox)),
        }
    }

    pub fn write_input_bytes(
        sandbox: &JobSandbox,
        filename: &str,
        bytes: &[u8],
    ) -> Result<String, RuntimeError> {
        validate_file_name(filename)?;
        let path = sandbox.input_directory().join(filename);
        fs::write(&path, bytes).map_err(|error| RuntimeError::Io {
            action: "write job input",
            path: path.clone(),
            error,
        })?;
        sandbox.worker_relative(&path).map_err(RuntimeError::Store)
    }

    /// Builds path capabilities for an already-reserved job. The caller still
    /// supplies its content hashes, span, channels, and effective parameters.
    pub fn job_files(
        sandbox: &JobSandbox,
        material_relative: String,
    ) -> Result<JobFiles, RuntimeError> {
        Ok(JobFiles {
            material: material_relative,
            references: Default::default(),
            masks: Default::default(),
            staging_directory: sandbox
                .worker_relative(sandbox.staging_directory())
                .map_err(RuntimeError::Store)?,
        })
    }

    pub fn analyze(&mut self, request: AnalyzeRequest) -> Result<(), RuntimeError> {
        self.send(WireMessage::Analyze { request })
    }

    /// The claim is created only after its result has crossed the store's
    /// independent verification and atomic-publish boundary.
    pub fn analyze_claim(
        &mut self,
        request: AnalyzeRequest,
        publication: ClaimPublication,
    ) -> Result<(), RuntimeError> {
        if publication.model_manifest_sha256 != request.model_manifest_sha256 {
            return Err(RuntimeError::Protocol(
                "claim publication manifest does not match analysis request".into(),
            ));
        }
        if self
            .claim_publications
            .insert(request.job_id.clone(), publication)
            .is_some()
        {
            return Err(RuntimeError::Protocol(
                "claim publication already registered for job".into(),
            ));
        }
        if let Err(error) = self.analyze(request.clone()) {
            self.claim_publications.remove(&request.job_id);
            return Err(error);
        }
        Ok(())
    }

    /// Pump exactly one response. `Complete` is only returned after the
    /// supervisor has verified and atomically published all declared files.
    pub fn poll(&mut self, store: &ModelStore) -> Result<Option<RuntimeEvent>, RuntimeError> {
        self.receive(store)
    }

    /// Blocking variant used by simple adapters/tests until an async pipe
    /// pump is introduced. It returns progress events as they arrive.
    pub fn receive(&mut self, store: &ModelStore) -> Result<Option<RuntimeEvent>, RuntimeError> {
        let timeout = self
            .cancellation_started
            .and_then(|started| {
                self.policy
                    .deadlines
                    .cancel_grace
                    .checked_sub(started.elapsed())
            })
            .unwrap_or_else(|| {
                if self.cancellation_started.is_some() {
                    Duration::ZERO
                } else {
                    self.policy.deadlines.progress
                }
            });
        if timeout.is_zero() {
            return Err(RuntimeError::Protocol(format!(
                "worker cancellation deadline exceeded; bounded log tail: {}",
                self.stderr_tail()
            )));
        }
        let envelope = self.read_envelope_with_timeout(
            timeout,
            if self.cancellation_started.is_some() {
                "cancellation acknowledgement"
            } else {
                "progress/liveness"
            },
        )?;
        if let WireMessage::Complete { result } = &envelope.message {
            self.policy
                .output
                .validate_result(result)
                .map_err(|error| RuntimeError::Protocol(error.to_string()))?;
        }
        let progress = match &envelope.message {
            WireMessage::Progress { progress } => Some(progress.clone()),
            _ => None,
        };
        let event = self.supervisor.observe_worker(store, &envelope)?;
        Ok(match event {
            Some(SupervisorEvent::Published(result)) => {
                match self.claim_publications.remove(&result.result.job_id) {
                    Some(publication) => {
                        let claim = ModelClaimBundle::from_worker_result(
                            publication.model_manifest_sha256,
                            publication.source,
                            publication.runtime,
                            publication.additivity,
                            result.result.clone(),
                            publication.outputs,
                        )?;
                        Some(RuntimeEvent::ClaimPublished {
                            stored: result,
                            claim,
                        })
                    }
                    None => Some(RuntimeEvent::Published(result)),
                }
            }
            Some(SupervisorEvent::Cancelled { job_id }) => {
                self.claim_publications.remove(&job_id);
                Some(RuntimeEvent::Cancelled { job_id })
            }
            Some(SupervisorEvent::Failed(failure)) => Some(RuntimeEvent::Failed(failure)),
            Some(SupervisorEvent::JobTerminated { job_id, failure }) => {
                Some(RuntimeEvent::JobTerminated { job_id, failure })
            }
            None => progress.map(RuntimeEvent::Progress),
        })
    }

    pub fn cancel(&mut self, job_id: impl Into<String>) -> Result<(), RuntimeError> {
        self.send(WireMessage::Cancel {
            job_id: job_id.into(),
        })?;
        self.cancellation_started.get_or_insert_with(Instant::now);
        Ok(())
    }

    /// Kill/reap the child after a launcher cancellation deadline or pipe
    /// failure. Active staging remains untouched and every active job becomes
    /// a typed terminal event.
    pub fn terminate(&mut self, likely_oom: bool) -> Result<Vec<RuntimeEvent>, RuntimeError> {
        let _ = self.child.kill();
        let status = wait_for_exit(&mut self.child, self.policy.deadlines.kill_grace)?;
        self.process_reaped = status.is_some();
        let detail = match status {
            Some(status) => format!("worker exited with {status}"),
            None => format!(
                "worker did not exit within kill deadline; bounded log tail: {}",
                self.stderr_tail()
            ),
        };
        let failure = crash_failure(detail, likely_oom);
        self.claim_publications.clear();
        Ok(self
            .supervisor
            .worker_terminated(failure)
            .into_iter()
            .filter_map(|event| match event {
                SupervisorEvent::JobTerminated { job_id, failure } => {
                    Some(RuntimeEvent::JobTerminated { job_id, failure })
                }
                _ => None,
            })
            .collect())
    }

    fn send(&mut self, message: WireMessage) -> Result<(), RuntimeError> {
        let envelope = WireEnvelope::new(self.next_controller_sequence, message);
        self.supervisor.observe_controller(&envelope)?;
        let line = envelope.to_jsonl()?;
        self.input
            .write_all(line.as_bytes())
            .map_err(|error| RuntimeError::Io {
                action: "write worker request",
                path: PathBuf::new(),
                error,
            })?;
        self.input.flush().map_err(|error| RuntimeError::Io {
            action: "flush worker request",
            path: PathBuf::new(),
            error,
        })?;
        self.next_controller_sequence += 1;
        Ok(())
    }

    fn read_envelope_with_timeout(
        &mut self,
        timeout: Duration,
        context: &'static str,
    ) -> Result<WireEnvelope, RuntimeError> {
        let event = self
            .output
            .as_ref()
            .ok_or(RuntimeError::WorkerExited(None))?
            .recv_timeout(timeout)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => RuntimeError::Protocol(format!(
                    "worker {context} deadline exceeded after {} ms; bounded log tail: {}",
                    timeout.as_millis(),
                    self.stderr_tail()
                )),
                mpsc::RecvTimeoutError::Disconnected => RuntimeError::WorkerExited(None),
            })?;
        let line = match event {
            PumpEvent::Line(line) => line,
            PumpEvent::Eof => {
                let status = self.child.try_wait().map_err(|error| RuntimeError::Io {
                    action: "inspect worker exit",
                    path: PathBuf::new(),
                    error,
                })?;
                return Err(RuntimeError::WorkerExited(
                    status.map(|status| status.to_string()),
                ));
            }
            PumpEvent::Io(detail) => {
                return Err(RuntimeError::Protocol(format!(
                    "worker control pump failed: {detail}"
                )));
            }
            PumpEvent::Oversized { maximum } => {
                return Err(RuntimeError::Protocol(format!(
                    "worker control record exceeded {maximum} bytes"
                )));
            }
        };
        WireEnvelope::from_jsonl(&line).map_err(RuntimeError::Wire)
    }
}

impl Drop for WorkerRuntime {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => self.process_reaped = true,
            Ok(None) => {
                let _ = self.child.kill();
                self.process_reaped =
                    wait_for_exit(&mut self.child, self.policy.deadlines.kill_grace)
                        .ok()
                        .flatten()
                        .is_some();
            }
            Err(_) => {}
        }
        // Disconnect before joining so a producer backpressured by the bounded
        // channel can exit instead of deadlocking Drop.
        self.output.take();
        if self.process_reaped {
            if let Some(thread) = self.output_thread.take() {
                let _ = thread.join();
            }
            if let Some(thread) = self.stderr_thread.take() {
                let _ = thread.join();
            }
        }
    }
}

#[derive(Debug)]
pub enum RuntimeError {
    Launch {
        program: PathBuf,
        error: io::Error,
    },
    MissingPipe(&'static str),
    Io {
        action: &'static str,
        path: PathBuf,
        error: io::Error,
    },
    Store(StoreError),
    Registry(RegistryError),
    Claim(ClaimError),
    Supervisor(SupervisorError),
    Wire(WireError),
    ModelUnavailable(String),
    WorkerExited(Option<String>),
    Protocol(String),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Launch { program, error } => {
                write!(f, "could not launch {}: {error}", program.display())
            }
            Self::MissingPipe(name) => write!(f, "worker process did not expose {name}"),
            Self::Io {
                action,
                path,
                error,
            } if path.as_os_str().is_empty() => write!(f, "could not {action}: {error}"),
            Self::Io {
                action,
                path,
                error,
            } => write!(f, "could not {action} at {}: {error}", path.display()),
            Self::Store(error) => error.fmt(f),
            Self::Registry(error) => error.fmt(f),
            Self::Claim(error) => error.fmt(f),
            Self::Supervisor(error) => error.fmt(f),
            Self::Wire(error) => error.fmt(f),
            Self::ModelUnavailable(id) => write!(f, "model is unavailable or unverified: {id}"),
            Self::WorkerExited(status) => write!(
                f,
                "worker pipe closed{}",
                status
                    .as_deref()
                    .map(|value| format!(" ({value})"))
                    .unwrap_or_default()
            ),
            Self::Protocol(detail) => f.write_str(detail),
        }
    }
}
impl std::error::Error for RuntimeError {}
impl From<StoreError> for RuntimeError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}
impl From<RegistryError> for RuntimeError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}
impl From<ClaimError> for RuntimeError {
    fn from(error: ClaimError) -> Self {
        Self::Claim(error)
    }
}
impl From<SupervisorError> for RuntimeError {
    fn from(error: SupervisorError) -> Self {
        Self::Supervisor(error)
    }
}
impl From<WireError> for RuntimeError {
    fn from(error: WireError) -> Self {
        Self::Wire(error)
    }
}

fn validate_file_name(value: &str) -> Result<(), RuntimeError> {
    let path = Path::new(value);
    if value.is_empty() || path.components().count() != 1 || path.file_name().is_none() {
        return Err(RuntimeError::Protocol(
            "job input filename must be one normal path component".into(),
        ));
    }
    Ok(())
}

#[derive(Debug)]
enum PumpEvent {
    Line(String),
    Eof,
    Io(String),
    Oversized { maximum: usize },
}

fn control_reader(
    stdout: ChildStdout,
    sender: mpsc::SyncSender<PumpEvent>,
    maximum_line_bytes: usize,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let event = match read_control_line(&mut reader, maximum_line_bytes) {
            Ok(Some(line)) => PumpEvent::Line(line),
            Ok(None) => PumpEvent::Eof,
            Err(ControlReadError::Io(error)) => PumpEvent::Io(error.to_string()),
            Err(ControlReadError::Oversized) => PumpEvent::Oversized {
                maximum: maximum_line_bytes,
            },
        };
        let terminal = !matches!(event, PumpEvent::Line(_));
        if sender.send(event).is_err() || terminal {
            return;
        }
    }
}

#[derive(Debug)]
enum ControlReadError {
    Io(io::Error),
    Oversized,
}

/// Reads without ever allocating beyond the configured control-record cap.
/// The bounded sync channel in `launch_with_policy` additionally backpressures
/// a worker that emits many individually valid records faster than the host.
fn read_control_line(
    reader: &mut impl BufRead,
    maximum_line_bytes: usize,
) -> Result<Option<String>, ControlReadError> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(ControlReadError::Io)?;
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            return decode_control_line(bytes).map(Some);
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if bytes.len().saturating_add(take) > maximum_line_bytes {
            return Err(ControlReadError::Oversized);
        }
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            return decode_control_line(bytes).map(Some);
        }
    }
}

fn decode_control_line(bytes: Vec<u8>) -> Result<String, ControlReadError> {
    String::from_utf8(bytes).map_err(|_| {
        ControlReadError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "worker output is not UTF-8",
        ))
    })
}

fn stderr_reader(mut stderr: impl Read, tail: Arc<Mutex<Vec<u8>>>, maximum_tail_bytes: usize) {
    let mut buffer = [0_u8; 4096];
    loop {
        let Ok(count) = stderr.read(&mut buffer) else {
            return;
        };
        if count == 0 {
            return;
        }
        let mut bytes = tail.lock().unwrap_or_else(|error| error.into_inner());
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() > maximum_tail_bytes {
            let excess = bytes.len() - maximum_tail_bytes;
            bytes.drain(..excess);
        }
    }
}

fn wait_for_exit(
    child: &mut Child,
    deadline: Duration,
) -> Result<Option<std::process::ExitStatus>, RuntimeError> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(|error| RuntimeError::Io {
            action: "inspect worker termination",
            path: PathBuf::new(),
            error,
        })? {
            return Ok(Some(status));
        }
        if started.elapsed() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn terminate_partially_launched(child: &mut Child, deadline: Duration) {
    let _ = child.kill();
    let _ = wait_for_exit(child, deadline);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn control_reader_refuses_a_line_before_allocating_past_its_bound() {
        let mut input = BufReader::new(Cursor::new(vec![b'x'; 65]));
        assert!(matches!(
            read_control_line(&mut input, 64),
            Err(ControlReadError::Oversized)
        ));
    }

    #[test]
    fn bounded_log_tail_keeps_only_the_newest_worker_diagnostics() {
        let tail = Arc::new(Mutex::new(Vec::new()));
        stderr_reader(Cursor::new(b"0123456789abcdef"), Arc::clone(&tail), 8);
        assert_eq!(&*tail.lock().unwrap(), b"89abcdef");
    }

    #[cfg(unix)]
    #[test]
    fn fake_process_uses_the_bounded_async_pumps() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "audec-worker-pump-{}-{stamp}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let script = root.join("worker.sh");
        fs::write(
            &script,
            r#"#!/bin/sh
IFS= read -r line
printf '%s' 'discarded-prefix-retained-tail' >&2
printf '%s\n' '{"protocol_version":1,"sequence":0,"kind":"capabilities","capabilities":{"worker_name":"bounded-fake","backends":["cpu"],"maximum_parallel_jobs":1,"shared_memory":false}}'
while IFS= read -r line; do :; done
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        let store = ModelStore::new(root.join("store"));
        let policy = RuntimePolicy {
            output: WorkerOutputPolicy {
                maximum_log_tail_bytes: 13,
                ..WorkerOutputPolicy::default()
            },
            ..RuntimePolicy::default()
        };
        let runtime = WorkerRuntime::launch_with_policy(
            &store,
            WorkerLaunch {
                program: script,
                arguments: vec![],
                expected_worker_name: "bounded-fake".into(),
            },
            policy,
        )
        .unwrap();
        for _ in 0..100 {
            if runtime.stderr_tail().ends_with("retained-tail") {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(runtime.stderr_tail(), "retained-tail");
        drop(runtime);
        fs::remove_dir_all(root).unwrap();
    }
}
