//! Executable controller for one isolated model-worker child process.
//!
//! This layer owns pipes and process lifetime, while `model_supervisor` owns
//! protocol/cache state and `model_store` owns publication. It has no GPUI
//! dependency, so the same runtime can later serve the CLI.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

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
    output: BufReader<ChildStdout>,
    supervisor: ModelSupervisor,
    capabilities: WireCapabilities,
    next_controller_sequence: u64,
    claim_publications: BTreeMap<String, ClaimPublication>,
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
        store.ensure_layout()?;
        let working_root = store.root().join("cache");
        let mut child = Command::new(&launch.program)
            .args(&launch.arguments)
            .current_dir(&working_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| RuntimeError::Launch {
                program: launch.program.clone(),
                error,
            })?;
        let input = child
            .stdin
            .take()
            .ok_or(RuntimeError::MissingPipe("stdin"))?;
        let output = child
            .stdout
            .take()
            .ok_or(RuntimeError::MissingPipe("stdout"))?;
        let mut runtime = Self {
            child,
            input,
            output: BufReader::new(output),
            supervisor: ModelSupervisor::new(),
            capabilities: WireCapabilities {
                worker_name: String::new(),
                backends: BTreeSet::new(),
                maximum_parallel_jobs: 1,
                shared_memory: false,
            },
            next_controller_sequence: 0,
            claim_publications: BTreeMap::new(),
        };
        runtime.send(WireMessage::Hello)?;
        let response = runtime.read_envelope()?;
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
        let response = self.read_envelope()?;
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
        let envelope = self.read_envelope()?;
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
        })
    }

    /// Kill/reap the child after a launcher cancellation deadline or pipe
    /// failure. Active staging remains untouched and every active job becomes
    /// a typed terminal event.
    pub fn terminate(&mut self, likely_oom: bool) -> Result<Vec<RuntimeEvent>, RuntimeError> {
        let _ = self.child.kill();
        let status = self.child.wait().map_err(|error| RuntimeError::Io {
            action: "wait for worker termination",
            path: PathBuf::new(),
            error,
        })?;
        let failure = crash_failure(format!("worker exited with {status}"), likely_oom);
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

    fn read_envelope(&mut self) -> Result<WireEnvelope, RuntimeError> {
        let mut line = String::new();
        let bytes = self
            .output
            .read_line(&mut line)
            .map_err(|error| RuntimeError::Io {
                action: "read worker response",
                path: PathBuf::new(),
                error,
            })?;
        if bytes == 0 {
            let status = self.child.try_wait().map_err(|error| RuntimeError::Io {
                action: "inspect worker exit",
                path: PathBuf::new(),
                error,
            })?;
            return Err(RuntimeError::WorkerExited(
                status.map(|status| status.to_string()),
            ));
        }
        WireEnvelope::from_jsonl(line.trim_end_matches(['\n', '\r'])).map_err(RuntimeError::Wire)
    }
}

impl Drop for WorkerRuntime {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
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
