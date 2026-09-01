//! Process-independent scanner/runtime core and deterministic fake backend.
//!
//! A launcher owns child processes, deadlines, and OS sandbox handles. This
//! module owns protocol behavior and conversion into the durable `plugin.rs`
//! cache. It never loads a dynamic library.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::plugin::{
    ArtifactFingerprint, AudioPortDescriptor, AudioPortRole, ChannelLayout, CpuArchitecture,
    DeterminismClass, ExecutionCapabilities, ParameterMapping, PluginIndex, PluginKey,
    PluginMetadata, PluginParameterDescriptor, PluginParameterKey, PluginRole, PluginStateBlob,
    PortDirection, ScanFailure, ScanFailureKind, ScanRecord, ScannerProvenance,
    SCAN_SCHEMA_VERSION,
};
use crate::plugin_wire::{
    self, CapabilitiesDto, Envelope, FormatDto, Message, ParameterValueDto, RuntimeFailureDto,
    RuntimeFailureKindDto, ScanFailureDto, ScanRecordDto, SessionValidator, SharedMemoryBindingDto,
    StateArtifactDto, TailDto, TokenDto,
};

pub const FAKE_CLAP_ID: &str = "org.audec.fixture.gain";
pub const FAKE_VST3_ID: &str = "56535441554445434649585455524531";

const MAX_CONTROL_LINE_BYTES: usize = 8 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct FakeWorker {
    session_root: PathBuf,
    instances: BTreeMap<TokenDto, FakeInstance>,
}

#[derive(Debug)]
struct FakeInstance {
    plugin: PluginKey,
    gain: f64,
}

impl FakeWorker {
    pub fn new(session_root: PathBuf) -> Self {
        Self {
            session_root,
            instances: BTreeMap::new(),
        }
    }

    pub fn capabilities() -> CapabilitiesDto {
        CapabilitiesDto {
            worker_name: "audec-fake-plugin".into(),
            worker_version: "1".into(),
            worker_build_sha256: plugin_wire::digest_bytes(b"audec-fake-plugin-v1").to_hex(),
            formats: BTreeSet::from([FormatDto::Clap, FormatDto::Vst3]),
            architectures: BTreeSet::from([current_architecture().into()]),
            scanning: true,
            realtime: true,
            offline: true,
            shared_memory: true,
            maximum_instances: 64,
        }
    }

    pub fn handle(&mut self, message: Message) -> Result<Option<Message>, WorkerError> {
        match message {
            Message::Hello => Ok(Some(Message::Capabilities {
                capabilities: Self::capabilities(),
            })),
            Message::Scan { request } => Ok(Some(match scan_fixture(&request.candidate_path) {
                Ok(record) => Message::ScanReady {
                    request_id: request.request_id,
                    record: ScanRecordDto::from_domain(&record)?,
                },
                Err(failure) => Message::ScanFailed {
                    request_id: request.request_id,
                    failure: ScanFailureDto::from_domain(&failure)?,
                },
            })),
            Message::Instantiate { request } => {
                let plugin = request.plugin.to_domain()?;
                let mut instance = FakeInstance { plugin, gain: 0.5 };
                if let Some(state) = request.state {
                    let path = self.resolve(&state.relative_path)?;
                    let bytes =
                        fs::read(&path).map_err(|source| WorkerError::Io { path, source })?;
                    instance.restore(&state.into_blob(bytes)?)?;
                }
                if self
                    .instances
                    .insert(request.instance.clone(), instance)
                    .is_some()
                {
                    return Err(WorkerError::State("duplicate instance"));
                }
                Ok(Some(Message::Instantiated {
                    request_id: request.request_id,
                    instance: request.instance,
                    latency_frames: 16,
                    tail: TailDto::FiniteFrames { frames: 64 },
                }))
            }
            Message::BindSharedMemory { binding } => Ok(Some(Message::Bound {
                instance: binding.instance,
            })),
            Message::Activate { instance } => {
                self.instance(&instance)?;
                Ok(Some(Message::Activated { instance }))
            }
            Message::SetParameters { request } => {
                let instance = self.instance_mut(&request.instance)?;
                for parameter in request.values {
                    let (key, value) = parameter.to_domain()?;
                    if key != fake_parameter_key(&instance.plugin) {
                        return Err(WorkerError::State("unknown fake parameter"));
                    }
                    instance.gain = value.get();
                }
                Ok(Some(Message::ParametersSet {
                    request_id: request.request_id,
                    instance: request.instance,
                }))
            }
            Message::Process {
                instance,
                process_sequence,
                ..
            } => {
                self.instance(&instance)?;
                Ok(Some(Message::Processed {
                    instance,
                    process_sequence,
                    output_event_count: 0,
                }))
            }
            Message::SaveState { request } => {
                let state = self.instance(&request.instance)?.save();
                if state.bytes.len() as u64 > request.maximum_bytes {
                    return Err(WorkerError::State("state exceeds request limit"));
                }
                let path = self.resolve(&request.output_relative_path)?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|source| WorkerError::Io {
                        path: parent.to_path_buf(),
                        source,
                    })?;
                }
                fs::write(&path, &state.bytes).map_err(|source| WorkerError::Io {
                    path: path.clone(),
                    source,
                })?;
                Ok(Some(Message::StateSaved {
                    request_id: request.request_id,
                    state: StateArtifactDto::from_blob(&state, request.output_relative_path)?,
                }))
            }
            Message::Deactivate { instance } => {
                self.instance(&instance)?;
                Ok(Some(Message::Deactivated { instance }))
            }
            Message::Destroy { instance } => {
                if self.instances.remove(&instance).is_none() {
                    return Err(WorkerError::State("unknown instance"));
                }
                Ok(Some(Message::Destroyed { instance }))
            }
            Message::Shutdown => Ok(None),
            _ => Err(WorkerError::State("worker received response message")),
        }
    }

    pub fn failure(error: &WorkerError) -> Message {
        Message::Error {
            failure: RuntimeFailureDto {
                request_id: None,
                instance: None,
                kind: match error {
                    WorkerError::Wire(_) => RuntimeFailureKindDto::InvalidRequest,
                    WorkerError::Domain(_) => RuntimeFailureKindDto::InvalidState,
                    WorkerError::Io { .. } => RuntimeFailureKindDto::Io,
                    WorkerError::State(_) => RuntimeFailureKindDto::Backend,
                },
                recoverable: false,
                detail: error.to_string(),
            },
        }
    }

    fn resolve(&self, relative: &str) -> Result<PathBuf, WorkerError> {
        plugin_wire::validate_relative_path(relative)?;
        Ok(self.session_root.join(relative))
    }

    fn instance(&self, token: &TokenDto) -> Result<&FakeInstance, WorkerError> {
        self.instances
            .get(token)
            .ok_or(WorkerError::State("unknown instance"))
    }

    fn instance_mut(&mut self, token: &TokenDto) -> Result<&mut FakeInstance, WorkerError> {
        self.instances
            .get_mut(token)
            .ok_or(WorkerError::State("unknown instance"))
    }
}

impl FakeInstance {
    fn save(&self) -> PluginStateBlob {
        let bytes = format!("audec-fake-state-v1\ngain={:.17}\n", self.gain).into_bytes();
        PluginStateBlob {
            plugin: self.plugin.clone(),
            state_format_version: 1,
            digest: plugin_wire::digest_bytes(&bytes),
            bytes,
        }
    }

    fn restore(&mut self, state: &PluginStateBlob) -> Result<(), WorkerError> {
        if state.plugin != self.plugin || state.state_format_version != 1 {
            return Err(WorkerError::State("state identity mismatch"));
        }
        let text = std::str::from_utf8(&state.bytes)
            .map_err(|_| WorkerError::State("state is not UTF-8"))?;
        self.gain = text
            .strip_prefix("audec-fake-state-v1\ngain=")
            .and_then(|value| value.strip_suffix('\n'))
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
            .ok_or(WorkerError::State("invalid fake state"))?;
        Ok(())
    }
}

/// An executable boundary for a scanner/runtime worker.
///
/// The executable is always launched directly (never through a shell), with
/// only stdio inherited as explicit protocol pipes. A platform launcher may
/// add a sandbox profile and inherited shared-memory handles before calling
/// [`WorkerProcess::spawn`].
#[derive(Clone, Debug)]
pub struct WorkerLaunch {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub working_directory: PathBuf,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
}

impl WorkerLaunch {
    pub fn new(executable: PathBuf, working_directory: PathBuf) -> Self {
        Self {
            executable,
            arguments: Vec::new(),
            working_directory,
            startup_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(2),
        }
    }

    fn validate(&self) -> Result<(), HostError> {
        if !self.executable.is_absolute() || !self.working_directory.is_absolute() {
            return Err(HostError::InvalidLaunch(
                "worker executable and working directory must be absolute",
            ));
        }
        if self.startup_timeout.is_zero() || self.request_timeout.is_zero() {
            return Err(HostError::InvalidLaunch(
                "worker deadlines must be non-zero",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostHealth {
    Ready,
    Failed,
    Shutdown,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HostDiagnostics {
    pub launches: u64,
    pub recoveries: u64,
    pub crashes: u64,
    pub timeouts: u64,
    pub protocol_failures: u64,
    pub worker_failures: u64,
    pub completed_process_blocks: u64,
    pub last_failure: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InstanceStatus {
    pub plugin: PluginKey,
    pub active: bool,
    pub latency_frames: u32,
    pub tail: crate::plugin::TailReport,
    pub next_process_sequence: u64,
    pub recovery_count: u64,
}

/// Everything needed to reconstruct one isolated instance after a worker
/// crash. State artifacts are session-relative and hash checked by both ends.
#[derive(Clone, Debug, PartialEq)]
pub struct InstanceRecipe {
    pub instance: u128,
    pub artifact_lease: u128,
    pub plugin: PluginKey,
    pub contract: crate::plugin::ProcessingContract,
    pub state: Option<StateArtifactDto>,
    pub shared_memory: SharedMemoryBindingDto,
    pub parameters: Vec<ParameterValueDto>,
    pub activate: bool,
}

#[derive(Clone, Debug)]
struct ManagedInstance {
    recipe: InstanceRecipe,
    status: InstanceStatus,
}

enum ReaderEvent {
    Line(String),
    Eof,
    Io(String),
    Oversized,
}

struct Exchange {
    response: Message,
    notifications: Vec<Message>,
}

/// Synchronous control-plane connection to a child process.
///
/// This type is intentionally not usable from a realtime callback: every
/// method may perform IPC or wait up to a deadline. Audio and event payloads
/// belong in the negotiated shared-memory regions, not in this object.
pub struct WorkerProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    reader: mpsc::Receiver<ReaderEvent>,
    reader_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    session: SessionValidator,
    controller_sequence: u64,
    capabilities: CapabilitiesDto,
    request_timeout: Duration,
    closed: bool,
}

impl WorkerProcess {
    pub fn spawn(launch: &WorkerLaunch) -> Result<Self, HostError> {
        launch.validate()?;
        let mut command = Command::new(&launch.executable);
        command
            .args(&launch.arguments)
            .current_dir(&launch.working_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|source| HostError::Io {
            operation: "spawn worker",
            source,
        })?;
        let stdin = child.stdin.take().ok_or(HostError::MissingPipe("stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(HostError::MissingPipe("stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or(HostError::MissingPipe("stderr"))?;

        let (sender, reader) = mpsc::channel();
        let reader_thread = thread::Builder::new()
            .name("audec-plugin-control-reader".into())
            .spawn(move || control_reader(stdout, sender))
            .map_err(|source| HostError::Io {
                operation: "spawn worker reader",
                source,
            })?;
        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        let stderr_for_thread = Arc::clone(&stderr_tail);
        let stderr_thread = thread::Builder::new()
            .name("audec-plugin-stderr-reader".into())
            .spawn(move || stderr_reader(stderr, stderr_for_thread))
            .map_err(|source| HostError::Io {
                operation: "spawn worker stderr reader",
                source,
            })?;

        let mut process = Self {
            child,
            stdin: Some(stdin),
            reader,
            reader_thread: Some(reader_thread),
            stderr_thread: Some(stderr_thread),
            stderr_tail,
            session: SessionValidator::default(),
            controller_sequence: 0,
            capabilities: FakeWorker::capabilities(),
            request_timeout: launch.request_timeout,
            closed: false,
        };
        let exchange = process.exchange_with_timeout(Message::Hello, launch.startup_timeout)?;
        let Message::Capabilities { capabilities } = exchange.response else {
            return Err(process.fail(HostErrorKind::Protocol, "worker did not answer hello"));
        };
        process.capabilities = capabilities;
        Ok(process)
    }

    pub fn capabilities(&self) -> &CapabilitiesDto {
        &self.capabilities
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    fn exchange(&mut self, message: Message) -> Result<Exchange, HostError> {
        self.exchange_with_timeout(message, self.request_timeout)
    }

    fn exchange_with_timeout(
        &mut self,
        message: Message,
        timeout: Duration,
    ) -> Result<Exchange, HostError> {
        if self.closed {
            return Err(HostError::Closed);
        }
        let envelope = Envelope::new(self.controller_sequence, message);
        self.session
            .observe_controller(&envelope)
            .map_err(HostError::Wire)?;
        let encoded = envelope.to_jsonl().map_err(HostError::Wire)?;
        let stdin = self.stdin.as_mut().ok_or(HostError::Closed)?;
        stdin
            .write_all(encoded.as_bytes())
            .and_then(|_| stdin.flush())
            .map_err(|source| HostError::Io {
                operation: "write worker request",
                source,
            })?;
        self.controller_sequence = self.controller_sequence.saturating_add(1);

        let started = std::time::Instant::now();
        let mut notifications = Vec::new();
        loop {
            let remaining = timeout
                .checked_sub(started.elapsed())
                .ok_or_else(|| self.fail(HostErrorKind::Timeout, "worker request timed out"))?;
            let event = self
                .reader
                .recv_timeout(remaining)
                .map_err(|error| match error {
                    mpsc::RecvTimeoutError::Timeout => {
                        self.fail(HostErrorKind::Timeout, "worker request timed out")
                    }
                    mpsc::RecvTimeoutError::Disconnected => {
                        self.fail(HostErrorKind::Crashed, "worker control pipe disconnected")
                    }
                })?;
            let line = match event {
                ReaderEvent::Line(line) => line,
                ReaderEvent::Eof => {
                    return Err(self.fail(HostErrorKind::Crashed, "worker exited before response"))
                }
                ReaderEvent::Io(detail) => return Err(self.fail(HostErrorKind::Io, &detail)),
                ReaderEvent::Oversized => {
                    return Err(self.fail(HostErrorKind::Protocol, "worker response exceeded limit"))
                }
            };
            let incoming = Envelope::from_jsonl(&line).map_err(|error| {
                self.fail(
                    HostErrorKind::Protocol,
                    &format!("invalid worker response: {error}"),
                )
            })?;
            self.session.observe_worker(&incoming).map_err(|error| {
                self.fail(
                    HostErrorKind::Protocol,
                    &format!("invalid worker transition: {error}"),
                )
            })?;
            match incoming.message {
                message @ (Message::LatencyChanged { .. } | Message::TailChanged { .. }) => {
                    notifications.push(message)
                }
                Message::Error { failure } => {
                    return Err(HostError::WorkerFailure(failure));
                }
                response => {
                    return Ok(Exchange {
                        response,
                        notifications,
                    })
                }
            }
        }
    }

    fn fail(&mut self, kind: HostErrorKind, detail: &str) -> HostError {
        let _ = self.child.kill();
        let status = self.child.wait().ok();
        self.closed = true;
        HostError::ProcessFailure {
            kind,
            detail: detail.into(),
            status,
            stderr: self.stderr(),
        }
    }

    pub fn stderr(&self) -> String {
        let bytes = self
            .stderr_tail
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn shutdown(mut self) -> Result<(), HostError> {
        if self.closed {
            return Err(HostError::Closed);
        }
        let envelope = Envelope::new(self.controller_sequence, Message::Shutdown);
        self.session
            .observe_controller(&envelope)
            .map_err(HostError::Wire)?;
        let encoded = envelope.to_jsonl().map_err(HostError::Wire)?;
        let stdin = self.stdin.as_mut().ok_or(HostError::Closed)?;
        stdin
            .write_all(encoded.as_bytes())
            .and_then(|_| stdin.flush())
            .map_err(|source| HostError::Io {
                operation: "write worker shutdown",
                source,
            })?;
        self.stdin.take();
        let started = std::time::Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().map_err(|source| HostError::Io {
                operation: "poll worker shutdown",
                source,
            })? {
                break status;
            }
            if started.elapsed() >= self.request_timeout {
                return Err(self.fail(HostErrorKind::Timeout, "worker shutdown timed out"));
            }
            thread::sleep(Duration::from_millis(2));
        };
        self.closed = true;
        if !status.success() {
            return Err(HostError::ProcessFailure {
                kind: HostErrorKind::Crashed,
                detail: "worker exited unsuccessfully during shutdown".into(),
                status: Some(status),
                stderr: self.stderr(),
            });
        }
        Ok(())
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.stdin.take();
        if let Some(thread) = self.reader_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }
}

/// Control-thread facade suitable for a project controller or future GPUI
/// surface. It owns no PCM and cannot execute plugin code in-process.
pub struct OutOfProcessPluginHost {
    launch: WorkerLaunch,
    process: Option<WorkerProcess>,
    capabilities: CapabilitiesDto,
    instances: BTreeMap<u128, ManagedInstance>,
    next_request_id: u64,
    health: HostHealth,
    diagnostics: HostDiagnostics,
}

impl OutOfProcessPluginHost {
    pub fn launch(launch: WorkerLaunch) -> Result<Self, HostError> {
        let process = WorkerProcess::spawn(&launch)?;
        let capabilities = process.capabilities().clone();
        Ok(Self {
            launch,
            process: Some(process),
            capabilities,
            instances: BTreeMap::new(),
            next_request_id: 1,
            health: HostHealth::Ready,
            diagnostics: HostDiagnostics {
                launches: 1,
                ..HostDiagnostics::default()
            },
        })
    }

    pub const fn health(&self) -> HostHealth {
        self.health
    }

    pub fn capabilities(&self) -> &CapabilitiesDto {
        &self.capabilities
    }

    pub fn diagnostics(&self) -> &HostDiagnostics {
        &self.diagnostics
    }

    pub fn instance(&self, instance: u128) -> Option<&InstanceStatus> {
        self.instances.get(&instance).map(|value| &value.status)
    }

    pub fn instances(&self) -> impl Iterator<Item = (u128, &InstanceStatus)> {
        self.instances
            .iter()
            .map(|(id, value)| (*id, &value.status))
    }

    pub fn scan_candidate(
        &mut self,
        request: crate::plugin_wire::ScanRequestDto,
    ) -> Result<Message, HostError> {
        if !self.capabilities.scanning {
            return Err(HostError::UnsupportedCapability("scanning"));
        }
        let exchange = self.exchange(Message::Scan { request })?;
        self.apply_notifications(&exchange.notifications);
        if matches!(
            exchange.response,
            Message::ScanReady { .. } | Message::ScanFailed { .. }
        ) {
            Ok(exchange.response)
        } else {
            Err(HostError::UnexpectedResponse("scan"))
        }
    }

    pub fn create_instance(&mut self, recipe: InstanceRecipe) -> Result<InstanceStatus, HostError> {
        if recipe.instance == 0 || recipe.artifact_lease == 0 {
            return Err(HostError::InvalidRecipe(
                "instance and lease tokens must be non-zero",
            ));
        }
        if self.instances.contains_key(&recipe.instance) {
            return Err(HostError::InvalidRecipe("duplicate instance token"));
        }
        recipe.plugin.validate().map_err(HostError::Domain)?;
        recipe.contract.validate().map_err(HostError::Domain)?;
        if recipe.shared_memory.instance != TokenDto::new(recipe.instance) {
            return Err(HostError::InvalidRecipe("shared-memory instance mismatch"));
        }
        let status = self.replay_recipe(&recipe, 0)?;
        self.instances.insert(
            recipe.instance,
            ManagedInstance {
                recipe,
                status: status.clone(),
            },
        );
        Ok(status)
    }

    pub fn set_parameters(
        &mut self,
        instance: u128,
        values: Vec<ParameterValueDto>,
    ) -> Result<(), HostError> {
        let request_id = self.request_id()?;
        let token = TokenDto::new(instance);
        let exchange = self.exchange(Message::SetParameters {
            request: crate::plugin_wire::SetParametersDto {
                request_id,
                instance: token.clone(),
                values: values.clone(),
            },
        })?;
        self.apply_notifications(&exchange.notifications);
        match exchange.response {
            Message::ParametersSet {
                instance: actual, ..
            } if actual == token => {
                self.managed_mut(instance)?.recipe.parameters = values;
                Ok(())
            }
            _ => Err(HostError::UnexpectedResponse("set parameters")),
        }
    }

    /// Notify the worker that one already-published shared-memory block is
    /// ready. On timeout or crash the child is terminated and the caller must
    /// apply the instance's persisted silence/bypass fallback for this block.
    pub fn process_block(
        &mut self,
        instance: u128,
        frames: u32,
        input_event_count: u32,
    ) -> Result<u32, HostError> {
        let sequence = self.managed(instance)?.status.next_process_sequence;
        let token = TokenDto::new(instance);
        let exchange = self.exchange(Message::Process {
            instance: token.clone(),
            process_sequence: sequence,
            frames,
            input_event_count,
        })?;
        self.apply_notifications(&exchange.notifications);
        match exchange.response {
            Message::Processed {
                instance: actual,
                process_sequence,
                output_event_count,
            } if actual == token && process_sequence == sequence => {
                self.managed_mut(instance)?.status.next_process_sequence += 1;
                self.diagnostics.completed_process_blocks += 1;
                Ok(output_event_count)
            }
            _ => Err(HostError::UnexpectedResponse("process block")),
        }
    }

    pub fn save_state(
        &mut self,
        instance: u128,
        output_relative_path: String,
        maximum_bytes: u64,
    ) -> Result<PluginStateBlob, HostError> {
        let request_id = self.request_id()?;
        let exchange = self.exchange(Message::SaveState {
            request: crate::plugin_wire::SaveStateDto {
                request_id,
                instance: TokenDto::new(instance),
                maximum_bytes,
                output_relative_path,
            },
        })?;
        self.apply_notifications(&exchange.notifications);
        let Message::StateSaved { state, .. } = exchange.response else {
            return Err(HostError::UnexpectedResponse("save state"));
        };
        let path = self.launch.working_directory.join(&state.relative_path);
        let bytes = fs::read(&path).map_err(|source| HostError::Io {
            operation: "read saved plugin state",
            source,
        })?;
        let blob = state.clone().into_blob(bytes).map_err(HostError::Wire)?;
        self.managed_mut(instance)?.recipe.state = Some(state);
        Ok(blob)
    }

    pub fn destroy_instance(&mut self, instance: u128) -> Result<(), HostError> {
        let active = self.managed(instance)?.status.active;
        if active {
            let exchange = self.exchange(Message::Deactivate {
                instance: TokenDto::new(instance),
            })?;
            self.apply_notifications(&exchange.notifications);
            if !matches!(exchange.response, Message::Deactivated { .. }) {
                return Err(HostError::UnexpectedResponse("deactivate"));
            }
        }
        let exchange = self.exchange(Message::Destroy {
            instance: TokenDto::new(instance),
        })?;
        self.apply_notifications(&exchange.notifications);
        if !matches!(exchange.response, Message::Destroyed { .. }) {
            return Err(HostError::UnexpectedResponse("destroy"));
        }
        self.instances.remove(&instance);
        Ok(())
    }

    /// Start a fresh worker and deterministically recreate every retained
    /// instance from its last verified state plus current parameter values.
    pub fn recover(&mut self) -> Result<(), HostError> {
        if self.health != HostHealth::Failed {
            return Err(HostError::InvalidRecoveryState);
        }
        let process = WorkerProcess::spawn(&self.launch)?;
        if process.capabilities().worker_build_sha256 != self.capabilities.worker_build_sha256 {
            return Err(HostError::WorkerIdentityChanged);
        }
        self.process = Some(process);
        self.health = HostHealth::Ready;
        self.diagnostics.launches += 1;
        self.diagnostics.recoveries += 1;
        let recipes = self
            .instances
            .values()
            .map(|value| (value.recipe.clone(), value.status.recovery_count + 1))
            .collect::<Vec<_>>();
        for (recipe, recovery_count) in recipes {
            let status = self.replay_recipe(&recipe, recovery_count)?;
            self.instances.get_mut(&recipe.instance).unwrap().status = status;
        }
        Ok(())
    }

    pub fn shutdown(mut self) -> Result<(), HostError> {
        let ids = self.instances.keys().copied().collect::<Vec<_>>();
        for id in ids.into_iter().rev() {
            self.destroy_instance(id)?;
        }
        let process = self.process.take().ok_or(HostError::Closed)?;
        let result = process.shutdown();
        self.health = HostHealth::Shutdown;
        result
    }

    fn replay_recipe(
        &mut self,
        recipe: &InstanceRecipe,
        recovery_count: u64,
    ) -> Result<InstanceStatus, HostError> {
        let request_id = self.request_id()?;
        let token = TokenDto::new(recipe.instance);
        let exchange = self.exchange(Message::Instantiate {
            request: crate::plugin_wire::InstantiateDto {
                request_id,
                instance: token.clone(),
                artifact_lease: TokenDto::new(recipe.artifact_lease),
                plugin: crate::plugin_wire::PluginKeyDto::from_domain(&recipe.plugin)
                    .map_err(HostError::Wire)?,
                contract: crate::plugin_wire::ProcessingContractDto::from_domain(&recipe.contract)
                    .map_err(HostError::Wire)?,
                state: recipe.state.clone(),
            },
        })?;
        let Message::Instantiated {
            instance,
            latency_frames,
            tail,
            ..
        } = exchange.response
        else {
            return Err(HostError::UnexpectedResponse("instantiate"));
        };
        if instance != token {
            return Err(HostError::UnexpectedResponse("instantiate token"));
        }
        let exchange = self.exchange(Message::BindSharedMemory {
            binding: recipe.shared_memory.clone(),
        })?;
        if !matches!(exchange.response, Message::Bound { .. }) {
            return Err(HostError::UnexpectedResponse("bind shared memory"));
        }
        if !recipe.parameters.is_empty() {
            let request_id = self.request_id()?;
            let exchange = self.exchange(Message::SetParameters {
                request: crate::plugin_wire::SetParametersDto {
                    request_id,
                    instance: token.clone(),
                    values: recipe.parameters.clone(),
                },
            })?;
            if !matches!(exchange.response, Message::ParametersSet { .. }) {
                return Err(HostError::UnexpectedResponse("restore parameters"));
            }
        }
        if recipe.activate {
            let exchange = self.exchange(Message::Activate {
                instance: token.clone(),
            })?;
            if !matches!(exchange.response, Message::Activated { .. }) {
                return Err(HostError::UnexpectedResponse("activate"));
            }
        }
        Ok(InstanceStatus {
            plugin: recipe.plugin.clone(),
            active: recipe.activate,
            latency_frames,
            tail: tail.to_domain(),
            next_process_sequence: 0,
            recovery_count,
        })
    }

    fn exchange(&mut self, message: Message) -> Result<Exchange, HostError> {
        if self.health != HostHealth::Ready {
            return Err(HostError::Closed);
        }
        let result = self
            .process
            .as_mut()
            .ok_or(HostError::Closed)?
            .exchange(message);
        if let Err(error) = &result {
            self.record_failure(error);
        }
        result
    }

    fn record_failure(&mut self, error: &HostError) {
        self.diagnostics.last_failure = Some(error.to_string());
        match error {
            HostError::ProcessFailure { kind, .. } => match kind {
                HostErrorKind::Timeout => self.diagnostics.timeouts += 1,
                HostErrorKind::Protocol => self.diagnostics.protocol_failures += 1,
                _ => self.diagnostics.crashes += 1,
            },
            HostError::Wire(_) | HostError::UnexpectedResponse(_) => {
                self.diagnostics.protocol_failures += 1
            }
            HostError::WorkerFailure(_) => self.diagnostics.worker_failures += 1,
            _ => {}
        }
        self.health = HostHealth::Failed;
    }

    fn apply_notifications(&mut self, notifications: &[Message]) {
        for notification in notifications {
            match notification {
                Message::LatencyChanged { instance, frames } => {
                    if let Ok(id) = instance.value() {
                        if let Some(managed) = self.instances.get_mut(&id) {
                            managed.status.latency_frames = *frames;
                        }
                    }
                }
                Message::TailChanged { instance, tail } => {
                    if let Ok(id) = instance.value() {
                        if let Some(managed) = self.instances.get_mut(&id) {
                            managed.status.tail = tail.to_domain();
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn request_id(&mut self) -> Result<u64, HostError> {
        let id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or(HostError::RequestIdExhausted)?;
        Ok(id)
    }

    fn managed(&self, instance: u128) -> Result<&ManagedInstance, HostError> {
        self.instances
            .get(&instance)
            .ok_or(HostError::UnknownInstance(instance))
    }

    fn managed_mut(&mut self, instance: u128) -> Result<&mut ManagedInstance, HostError> {
        self.instances
            .get_mut(&instance)
            .ok_or(HostError::UnknownInstance(instance))
    }
}

fn control_reader(stdout: impl Read, sender: mpsc::Sender<ReaderEvent>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_control_line(&mut reader) {
            Ok(Some(line)) => {
                if sender.send(ReaderEvent::Line(line)).is_err() {
                    return;
                }
            }
            Ok(None) => {
                let _ = sender.send(ReaderEvent::Eof);
                return;
            }
            Err(ControlReadError::Io(error)) => {
                let _ = sender.send(ReaderEvent::Io(error.to_string()));
                return;
            }
            Err(ControlReadError::Oversized) => {
                let _ = sender.send(ReaderEvent::Oversized);
                return;
            }
        }
    }
}

enum ControlReadError {
    Io(io::Error),
    Oversized,
}

fn read_control_line(reader: &mut impl BufRead) -> Result<Option<String>, ControlReadError> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(ControlReadError::Io)?;
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            return String::from_utf8(bytes).map(Some).map_err(|_| {
                ControlReadError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "worker output is not UTF-8",
                ))
            });
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if bytes.len().saturating_add(take) > MAX_CONTROL_LINE_BYTES {
            return Err(ControlReadError::Oversized);
        }
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            return String::from_utf8(bytes).map(Some).map_err(|_| {
                ControlReadError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "worker output is not UTF-8",
                ))
            });
        }
    }
}

fn stderr_reader(mut stderr: impl Read, tail: Arc<Mutex<Vec<u8>>>) {
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
        if bytes.len() > MAX_STDERR_BYTES {
            let excess = bytes.len() - MAX_STDERR_BYTES;
            bytes.drain(..excess);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostErrorKind {
    Timeout,
    Crashed,
    Protocol,
    Io,
}

#[derive(Debug)]
pub enum HostError {
    InvalidLaunch(&'static str),
    InvalidRecipe(&'static str),
    MissingPipe(&'static str),
    Io {
        operation: &'static str,
        source: io::Error,
    },
    Wire(plugin_wire::WireError),
    Domain(crate::plugin::PluginValidationError),
    WorkerFailure(RuntimeFailureDto),
    ProcessFailure {
        kind: HostErrorKind,
        detail: String,
        status: Option<ExitStatus>,
        stderr: String,
    },
    UnexpectedResponse(&'static str),
    UnknownInstance(u128),
    RequestIdExhausted,
    InvalidRecoveryState,
    WorkerIdentityChanged,
    UnsupportedCapability(&'static str),
    Closed,
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLaunch(detail) | Self::InvalidRecipe(detail) => {
                formatter.write_str(detail)
            }
            Self::MissingPipe(pipe) => write!(formatter, "worker did not provide {pipe} pipe"),
            Self::Io { operation, source } => write!(formatter, "{operation}: {source}"),
            Self::Wire(error) => error.fmt(formatter),
            Self::Domain(error) => error.fmt(formatter),
            Self::WorkerFailure(failure) => {
                write!(formatter, "plugin worker failure: {}", failure.detail)
            }
            Self::ProcessFailure {
                kind,
                detail,
                status,
                stderr,
            } => {
                write!(formatter, "plugin worker {kind:?}: {detail}")?;
                if let Some(status) = status {
                    write!(formatter, " ({status})")?;
                }
                if !stderr.is_empty() {
                    write!(formatter, ": {stderr}")?;
                }
                Ok(())
            }
            Self::UnexpectedResponse(operation) => {
                write!(formatter, "unexpected worker response to {operation}")
            }
            Self::UnknownInstance(instance) => {
                write!(formatter, "unknown plugin instance {instance:032x}")
            }
            Self::RequestIdExhausted => formatter.write_str("plugin request ID exhausted"),
            Self::InvalidRecoveryState => {
                formatter.write_str("plugin host is not awaiting recovery")
            }
            Self::WorkerIdentityChanged => {
                formatter.write_str("plugin worker identity changed during recovery")
            }
            Self::UnsupportedCapability(capability) => {
                write!(formatter, "plugin worker does not support {capability}")
            }
            Self::Closed => formatter.write_str("plugin worker is unavailable"),
        }
    }
}

impl std::error::Error for HostError {}

/// The controller fingerprints independently before trusting a scan. A
/// descriptor with a mismatched path or digest is rejected rather than cached.
pub fn apply_scan_result(
    index: &mut PluginIndex,
    expected_path: &Path,
    expected_artifact: &ArtifactFingerprint,
    message: &Message,
    quarantine_after: u32,
) -> Result<(), WorkerError> {
    match message {
        Message::ScanReady { record, .. } => {
            let record = record.to_domain()?;
            if record.canonical_path != expected_path || &record.artifact != expected_artifact {
                return Err(WorkerError::State("scanner identity mismatch"));
            }
            index.apply_success(record)?;
            Ok(())
        }
        Message::ScanFailed { failure, .. } => {
            index.apply_failure(
                expected_path.to_path_buf(),
                expected_artifact.clone(),
                failure.to_domain()?,
                quarantine_after,
            )?;
            Ok(())
        }
        _ => Err(WorkerError::State("not a scan result")),
    }
}

/// Converts launcher-level timeout/crash information into the same persistent
/// cache diagnostics as an ordinary scanner failure.
pub fn record_scan_process_failure(
    index: &mut PluginIndex,
    path: PathBuf,
    artifact: ArtifactFingerprint,
    timed_out: bool,
    detail: String,
    quarantine_after: u32,
) -> Result<(), WorkerError> {
    index.apply_failure(
        path,
        artifact,
        ScanFailure {
            kind: if timed_out {
                ScanFailureKind::TimedOut
            } else {
                ScanFailureKind::Crashed
            },
            detail,
            scanner: scanner_provenance(),
        },
        quarantine_after,
    )?;
    Ok(())
}

pub fn fingerprint_file(path: &Path) -> Result<ArtifactFingerprint, WorkerError> {
    let bytes = fs::read(path).map_err(|source| WorkerError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(ArtifactFingerprint {
        content: plugin_wire::digest_bytes(&bytes),
        byte_len: bytes.len() as u64,
        architectures: BTreeSet::from([current_architecture()]),
    })
}

fn scan_fixture(path: &str) -> Result<ScanRecord, ScanFailure> {
    let canonical_path = fs::canonicalize(path).map_err(|error| scan_io(error.to_string()))?;
    let bytes = fs::read(&canonical_path).map_err(|error| scan_io(error.to_string()))?;
    if bytes.is_empty() {
        return Err(ScanFailure {
            kind: ScanFailureKind::InvalidDescriptor,
            detail: "empty fake plugin artifact".into(),
            scanner: scanner_provenance(),
        });
    }
    let format = match canonical_path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("clap") => crate::plugin::PluginFormat::Clap,
        Some("vst3") => crate::plugin::PluginFormat::Vst3,
        _ => {
            return Err(ScanFailure {
                kind: ScanFailureKind::InvalidAbi,
                detail: "fixture accepts only .clap and .vst3 files".into(),
                scanner: scanner_provenance(),
            })
        }
    };
    Ok(ScanRecord {
        schema_version: SCAN_SCHEMA_VERSION,
        canonical_path,
        artifact: ArtifactFingerprint {
            content: plugin_wire::digest_bytes(&bytes),
            byte_len: bytes.len() as u64,
            architectures: BTreeSet::from([current_architecture()]),
        },
        scanner: scanner_provenance(),
        plugins: vec![fake_metadata(format)],
    })
}

fn fake_metadata(format: crate::plugin::PluginFormat) -> PluginMetadata {
    let plugin = PluginKey {
        identifier: match format {
            crate::plugin::PluginFormat::Clap => FAKE_CLAP_ID.into(),
            crate::plugin::PluginFormat::Vst3 => FAKE_VST3_ID.into(),
            _ => unreachable!(),
        },
        format,
    };
    PluginMetadata {
        key: plugin.clone(),
        name: "Audec Fixture Gain".into(),
        vendor: Some("Audec".into()),
        version: Some("1".into()),
        description: Some("deterministic isolated worker fixture".into()),
        homepage: None,
        features: BTreeSet::from(["audio-effect".into(), "stereo".into()]),
        roles: BTreeSet::from([PluginRole::AudioEffect]),
        audio_ports: vec![
            AudioPortDescriptor {
                native_id: 0,
                name: "Main Input".into(),
                direction: PortDirection::Input,
                role: AudioPortRole::Main,
                layouts: vec![ChannelLayout::Stereo],
                required: true,
            },
            AudioPortDescriptor {
                native_id: 0,
                name: "Main Output".into(),
                direction: PortDirection::Output,
                role: AudioPortRole::Main,
                layouts: vec![ChannelLayout::Stereo],
                required: true,
            },
        ],
        note_ports: vec![],
        parameters: vec![PluginParameterDescriptor {
            key: fake_parameter_key(&plugin),
            name: "Gain".into(),
            module: None,
            unit: Some("normalized".into()),
            plain_min: 0.0,
            plain_max: 1.0,
            plain_default: 0.5,
            mapping: ParameterMapping::Linear,
            automatable: true,
            modulatable: true,
            read_only: false,
            hidden: false,
        }],
        capabilities: ExecutionCapabilities {
            realtime_safe_claimed: true,
            hard_realtime_required: false,
            offline_processing: true,
            editor: false,
            state: true,
            latency_reporting: true,
            tail_reporting: true,
            determinism: DeterminismClass::Deterministic,
        },
    }
}

fn fake_parameter_key(plugin: &PluginKey) -> PluginParameterKey {
    match plugin.format {
        crate::plugin::PluginFormat::Clap => PluginParameterKey::Clap(1),
        crate::plugin::PluginFormat::Vst3 => PluginParameterKey::Vst3([1; 16]),
        _ => unreachable!(),
    }
}

fn scanner_provenance() -> ScannerProvenance {
    ScannerProvenance {
        scanner_name: "audec-fake-plugin".into(),
        scanner_version: "1".into(),
        scanner_build: plugin_wire::digest_bytes(b"audec-fake-plugin-v1"),
        host_os: std::env::consts::OS.into(),
        host_architecture: current_architecture(),
        hash_algorithm: "sha256".into(),
    }
}

fn scan_io(detail: String) -> ScanFailure {
    ScanFailure {
        kind: ScanFailureKind::Io,
        detail,
        scanner: scanner_provenance(),
    }
}

fn current_architecture() -> CpuArchitecture {
    match std::env::consts::ARCH {
        "aarch64" => CpuArchitecture::Aarch64,
        "x86_64" => CpuArchitecture::X86_64,
        _ => CpuArchitecture::Other,
    }
}

#[derive(Debug)]
pub enum WorkerError {
    Wire(plugin_wire::WireError),
    Domain(crate::plugin::PluginValidationError),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    State(&'static str),
}

impl fmt::Display for WorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wire(error) => error.fmt(formatter),
            Self::Domain(error) => error.fmt(formatter),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::State(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for WorkerError {}

impl From<plugin_wire::WireError> for WorkerError {
    fn from(value: plugin_wire::WireError) -> Self {
        Self::Wire(value)
    }
}

impl From<crate::plugin::PluginValidationError> for WorkerError {
    fn from(value: crate::plugin::PluginValidationError) -> Self {
        Self::Domain(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::plugin::{PluginFormat, ScanCacheEntry};
    use crate::plugin_wire::{
        InstantiateDto, ParameterKeyDto, ParameterValueDto, PluginKeyDto, ProcessingContractDto,
        SaveStateDto, SetParametersDto,
    };

    fn temporary_root() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("audec-fake-plugin-{unique}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn contract() -> ProcessingContractDto {
        ProcessingContractDto {
            sample_rate: 48_000,
            minimum_frames: 1,
            maximum_frames: 512,
            audio_ports: vec![],
            note_inputs: BTreeMap::new(),
            note_outputs: BTreeMap::new(),
            initial_latency_frames: 16,
            initial_tail: TailDto::FiniteFrames { frames: 64 },
            offline: true,
        }
    }

    fn instantiate(instance: u128, state: Option<StateArtifactDto>) -> Message {
        Message::Instantiate {
            request: InstantiateDto {
                request_id: instance as u64,
                instance: TokenDto::new(instance),
                artifact_lease: TokenDto::new(99),
                plugin: PluginKeyDto::from_domain(&PluginKey {
                    format: PluginFormat::Clap,
                    identifier: FAKE_CLAP_ID.into(),
                })
                .unwrap(),
                contract: contract(),
                state,
            },
        }
    }

    #[test]
    fn fake_state_roundtrip_is_byte_identical() {
        let root = temporary_root();
        let mut first = FakeWorker::new(root.clone());
        first.handle(instantiate(1, None)).unwrap();
        first
            .handle(Message::SetParameters {
                request: SetParametersDto {
                    request_id: 2,
                    instance: TokenDto::new(1),
                    values: vec![ParameterValueDto {
                        key: ParameterKeyDto::Clap { id: 1 },
                        normalized: 0.25,
                    }],
                },
            })
            .unwrap();
        let saved = first
            .handle(Message::SaveState {
                request: SaveStateDto {
                    request_id: 3,
                    instance: TokenDto::new(1),
                    maximum_bytes: 1024,
                    output_relative_path: "first.state".into(),
                },
            })
            .unwrap()
            .unwrap();
        let Message::StateSaved { state, .. } = saved else {
            panic!("expected state")
        };
        let mut second = FakeWorker::new(root.clone());
        second.handle(instantiate(4, Some(state))).unwrap();
        second
            .handle(Message::SaveState {
                request: SaveStateDto {
                    request_id: 5,
                    instance: TokenDto::new(4),
                    maximum_bytes: 1024,
                    output_relative_path: "second.state".into(),
                },
            })
            .unwrap();
        assert_eq!(
            fs::read(root.join("first.state")).unwrap(),
            fs::read(root.join("second.state")).unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_identity_is_verified_and_repeated_crashes_quarantine() {
        let root = temporary_root();
        let path = root.join("fixture.clap");
        fs::write(&path, b"deterministic fake artifact").unwrap();
        let path = fs::canonicalize(path).unwrap();
        let expected = fingerprint_file(&path).unwrap();
        let record = scan_fixture(path.to_str().unwrap()).unwrap();
        let message = Message::ScanReady {
            request_id: 1,
            record: ScanRecordDto::from_domain(&record).unwrap(),
        };
        let mut index = PluginIndex::default();
        apply_scan_result(&mut index, &path, &expected, &message, 2).unwrap();
        assert_eq!(
            index
                .descriptor(&PluginKey {
                    format: PluginFormat::Clap,
                    identifier: FAKE_CLAP_ID.into(),
                })
                .unwrap()
                .name,
            "Audec Fixture Gain"
        );

        let crash_path = root.join("crash.clap");
        fs::write(&crash_path, b"crashing bytes").unwrap();
        let crash_path = fs::canonicalize(crash_path).unwrap();
        let crash_fingerprint = fingerprint_file(&crash_path).unwrap();
        for _ in 0..2 {
            record_scan_process_failure(
                &mut index,
                crash_path.clone(),
                crash_fingerprint.clone(),
                false,
                "worker exited with status 70".into(),
                2,
            )
            .unwrap();
        }
        assert!(matches!(
            index.entries()[&crash_path],
            ScanCacheEntry::Quarantined {
                consecutive_failures: 2,
                ..
            }
        ));
        assert!(!index.needs_scan(&crash_path, &crash_fingerprint));
        fs::remove_dir_all(root).unwrap();
    }
}
