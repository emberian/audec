//! UI-neutral audio-device discovery, negotiation, clocks, and recovery.
//!
//! This module is the control-plane contract for the device service Audec
//! still needs. It deliberately does **not** open hardware. The current
//! [`crate::audio_host::AudioHost`] continues to own Rodio's default sink until
//! a concrete CPAL adapter implements [`AudioDeviceBackend`] and replaces that
//! ownership path. Adding that adapter requires coordinated Cargo/dependency
//! ownership and platform hardware gates; the types below do not claim it has
//! landed.
//!
//! Device callbacks receive an already-open, exactly negotiated stream and an
//! isolated [`RealtimeDeviceProcessor`]. They never call this state machine.
//! The callback contract forbids allocation, locks, I/O, logging, discovery,
//! graph construction, project mutation, and UI work. Hardware-format
//! conversion and any resampling use backend-owned preallocated storage.

use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::num::{NonZeroU16, NonZeroU32};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

const NO_FRAME: u64 = u64::MAX;

/// Direction of one independently clocked device stream.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DeviceDirection {
    Input,
    Output,
}

/// Hardware sample representation at the backend edge.
///
/// Audec's processor contract is interleaved `f32`; the backend performs the
/// explicit, allocation-free conversion selected by negotiation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DeviceSampleFormat {
    F32,
    F64,
    I8,
    I16,
    I24,
    I32,
    I64,
    U8,
    U16,
    U24,
    U32,
    U64,
}

impl DeviceSampleFormat {
    fn default_rank(self) -> usize {
        match self {
            Self::F32 => 0,
            Self::F64 => 1,
            Self::I32 => 2,
            Self::I24 => 3,
            Self::I16 => 4,
            Self::I64 => 5,
            Self::U32 => 6,
            Self::U24 => 7,
            Self::U16 => 8,
            Self::I8 => 9,
            Self::U8 => 10,
            Self::U64 => 11,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InclusiveU32Range {
    pub min: NonZeroU32,
    pub max: NonZeroU32,
}

impl InclusiveU32Range {
    pub fn new(min: u32, max: u32) -> Result<Self, DeviceServiceError> {
        let min = NonZeroU32::new(min).ok_or(DeviceServiceError::ZeroSampleRate)?;
        let max = NonZeroU32::new(max).ok_or(DeviceServiceError::ZeroSampleRate)?;
        if min > max {
            return Err(DeviceServiceError::InvalidRange {
                field: "sample rate",
                min: min.get(),
                max: max.get(),
            });
        }
        Ok(Self { min, max })
    }

    pub fn contains(self, value: NonZeroU32) -> bool {
        self.min <= value && value <= self.max
    }

    fn nearest(self, value: NonZeroU32) -> NonZeroU32 {
        value.max(self.min).min(self.max)
    }
}

/// Exact hardware callback sizes supported by one stream profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BufferSizeSupport {
    Fixed(NonZeroU32),
    Range {
        min: NonZeroU32,
        max: NonZeroU32,
        default: NonZeroU32,
    },
}

impl BufferSizeSupport {
    pub fn range(min: u32, max: u32, default: u32) -> Result<Self, DeviceServiceError> {
        let min = NonZeroU32::new(min).ok_or(DeviceServiceError::ZeroBufferSize)?;
        let max = NonZeroU32::new(max).ok_or(DeviceServiceError::ZeroBufferSize)?;
        let default = NonZeroU32::new(default).ok_or(DeviceServiceError::ZeroBufferSize)?;
        if min > max {
            return Err(DeviceServiceError::InvalidRange {
                field: "buffer size",
                min: min.get(),
                max: max.get(),
            });
        }
        if !(min..=max).contains(&default) {
            return Err(DeviceServiceError::DefaultOutsideRange {
                default: default.get(),
                min: min.get(),
                max: max.get(),
            });
        }
        Ok(Self::Range { min, max, default })
    }

    fn choose(self, request: BufferSizeRequest) -> Option<NonZeroU32> {
        match (self, request) {
            (Self::Fixed(actual), BufferSizeRequest::BackendDefault) => Some(actual),
            (Self::Fixed(actual), BufferSizeRequest::Exact(requested)) => {
                (actual == requested).then_some(actual)
            }
            (Self::Fixed(actual), BufferSizeRequest::Prefer(_)) => Some(actual),
            (Self::Range { default, .. }, BufferSizeRequest::BackendDefault) => Some(default),
            (Self::Range { min, max, .. }, BufferSizeRequest::Exact(requested)) => {
                ((min..=max).contains(&requested)).then_some(requested)
            }
            (Self::Range { min, max, .. }, BufferSizeRequest::Prefer(requested)) => {
                Some(requested.max(min).min(max))
            }
        }
    }
}

/// One backend-reported stream configuration family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupportedStreamConfig {
    pub channels: NonZeroU16,
    pub sample_format: DeviceSampleFormat,
    pub sample_rates: InclusiveU32Range,
    pub buffer_sizes: BufferSizeSupport,
}

/// A backend assertion about full-duplex clocking for one physical device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DuplexClockCapability {
    /// The backend cannot prove that input and output counters share a clock.
    UnknownOrIndependent,
    /// Backend/platform evidence says both endpoint counters share one clock.
    GuaranteedShared,
}

/// Backend-private identity. It is valid only for the current discovery/open
/// lifecycle and must never enter project persistence.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BackendDeviceId(pub String);

/// Raw discovery record returned by a concrete backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendDeviceDescriptor {
    pub runtime_id: BackendDeviceId,
    pub backend_name: String,
    pub display_name: String,
    /// Optional durable hardware identity only when the backend can actually
    /// provide one. CPAL generally cannot, so name-based ambiguous relinking
    /// remains the honest fallback.
    pub stable_hardware_id: Option<String>,
    pub default_input: bool,
    pub default_output: bool,
    pub input_configs: Vec<SupportedStreamConfig>,
    pub output_configs: Vec<SupportedStreamConfig>,
    pub duplex_clock: DuplexClockCapability,
}

/// Exact selection token from one discovery generation. Never persist it.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeviceToken {
    pub generation: u64,
    pub ordinal: u32,
}

/// Conservative persistence-safe relink preference.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeviceMatch {
    pub backend_name: String,
    pub display_name: String,
    pub stable_hardware_id: Option<String>,
}

/// Public discovery record. Runtime identity and durable matching are visibly
/// distinct so an index or display name cannot silently become project truth.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioDeviceDescriptor {
    pub token: DeviceToken,
    pub display_name: String,
    pub relink: DeviceMatch,
    pub default_input: bool,
    pub default_output: bool,
    pub input_configs: Vec<SupportedStreamConfig>,
    pub output_configs: Vec<SupportedStreamConfig>,
    pub duplex_clock: DuplexClockCapability,
}

impl AudioDeviceDescriptor {
    pub fn supports(&self, direction: DeviceDirection) -> bool {
        !match direction {
            DeviceDirection::Input => &self.input_configs,
            DeviceDirection::Output => &self.output_configs,
        }
        .is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceSelection {
    /// Exact current selection, optionally carrying a deliberate relink choice
    /// for recovery after the discovery generation changes.
    Runtime {
        token: DeviceToken,
        relink_fallback: Option<DeviceMatch>,
    },
    Relink(DeviceMatch),
    /// Follow the backend's explicitly reported default for this direction.
    Default,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleRatePolicy {
    /// Device rate must equal the project rate. This is the current safe
    /// default while no explicit device resampler has been integrated.
    RequireProjectRate,
    /// A backend may choose the nearest rate only when the service feature set
    /// says a preallocated sample-rate converter is actually installed.
    AllowExplicitConversion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BufferSizeRequest {
    BackendDefault,
    Exact(NonZeroU32),
    Prefer(NonZeroU32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DuplexClockPolicy {
    RequireShared,
    /// Independent input/output clocks require a drift-tracking asynchronous
    /// resampler, not merely equal nominal sample rates.
    AllowIndependentWithCompensation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointSettings {
    pub selection: DeviceSelection,
    pub channels: NonZeroU16,
    /// Calibration supplied by the user/device layer. The original value is
    /// retained and never baked destructively into recorded media.
    pub latency_compensation_frames: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveryPolicy {
    pub reopen_after_loss: bool,
    pub max_attempts: NonZeroU16,
    /// Deterministic control-poll delay. A platform adapter may schedule these
    /// polls with its own timer; this state machine never reads wall clock.
    pub retry_delay_polls: u32,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            reopen_after_loss: true,
            max_attempts: NonZeroU16::new(3).expect("three is non-zero"),
            retry_delay_polls: 1,
        }
    }
}

/// User-owned device settings. These are plain stable data; a later codec may
/// serialize them without persisting backend handles or runtime tokens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceOpenSettings {
    pub project_sample_rate: NonZeroU32,
    pub output: Option<EndpointSettings>,
    pub input: Option<EndpointSettings>,
    pub sample_rate_policy: SampleRatePolicy,
    pub buffer_size: BufferSizeRequest,
    /// Ordered preference. Empty means Audec's deterministic default order.
    pub sample_formats: Vec<DeviceSampleFormat>,
    pub duplex_clock_policy: DuplexClockPolicy,
    pub recovery: RecoveryPolicy,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeviceServiceFeatures {
    pub sample_rate_conversion: bool,
    pub independent_duplex_clock_compensation: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NegotiatedStreamConfig {
    pub channels: NonZeroU16,
    pub sample_rate: NonZeroU32,
    pub sample_format: DeviceSampleFormat,
    pub buffer_frames: NonZeroU32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NegotiatedEndpoint {
    pub token: DeviceToken,
    pub display_name: String,
    pub relink: DeviceMatch,
    pub config: NegotiatedStreamConfig,
    pub requires_sample_rate_conversion: bool,
    pub latency_compensation_frames: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DuplexClocking {
    InputOnly,
    OutputOnly,
    Shared,
    IndependentWithCompensation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NegotiatedDevicePlan {
    pub project_sample_rate: NonZeroU32,
    pub output: Option<NegotiatedEndpoint>,
    pub input: Option<NegotiatedEndpoint>,
    pub clocking: DuplexClocking,
}

/// Exact plan handed to a backend. Its IDs are process-local even though the
/// negotiated public plan is safe to present in UI and diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendOpenPlan {
    pub public: NegotiatedDevicePlan,
    pub output_runtime_id: Option<BackendDeviceId>,
    pub input_runtime_id: Option<BackendDeviceId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendFailure {
    pub code: String,
    pub detail: String,
}

impl BackendFailure {
    pub fn new(code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            detail: detail.into(),
        }
    }
}

impl fmt::Display for BackendFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceServiceError {
    ZeroSampleRate,
    ZeroBufferSize,
    InvalidRange {
        field: &'static str,
        min: u32,
        max: u32,
    },
    DefaultOutsideRange {
        default: u32,
        min: u32,
        max: u32,
    },
    NoEndpointsRequested,
    TooManyDevices(usize),
    DuplicateBackendId(BackendDeviceId),
    StaleSelection {
        requested: u64,
        current: u64,
    },
    MissingRuntimeDevice(DeviceToken),
    MissingRelinkDevice(DeviceMatch),
    AmbiguousRelinkDevice {
        preference: DeviceMatch,
        matches: usize,
    },
    MissingDefault(DeviceDirection),
    AmbiguousDefault {
        direction: DeviceDirection,
        matches: usize,
    },
    DirectionUnavailable {
        device: String,
        direction: DeviceDirection,
    },
    NoCompatibleConfiguration {
        device: String,
        direction: DeviceDirection,
    },
    SampleRateConversionUnavailable {
        requested: u32,
        nearest: u32,
    },
    IndependentDuplexClock,
    IndependentDuplexCompensationUnavailable,
    AlreadyOpen,
    NotRunning,
    EndpointNotOpen(DeviceDirection),
    Backend(BackendFailure),
}

impl fmt::Display for DeviceServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSampleRate => formatter.write_str("sample rate must be positive"),
            Self::ZeroBufferSize => formatter.write_str("buffer size must be positive"),
            Self::InvalidRange { field, min, max } =>
                write!(formatter, "{field} range is inverted: {min}..={max}"),
            Self::DefaultOutsideRange { default, min, max } =>
                write!(formatter, "default buffer {default} is outside {min}..={max}"),
            Self::NoEndpointsRequested => formatter.write_str("at least one input or output endpoint is required"),
            Self::TooManyDevices(count) => write!(formatter, "backend returned {count} devices; runtime tokens support at most {}", u32::MAX),
            Self::DuplicateBackendId(id) => write!(formatter, "backend returned duplicate runtime identity {:?}", id.0),
            Self::StaleSelection { requested, current } => write!(formatter, "device selection is from generation {requested}, current generation is {current}"),
            Self::MissingRuntimeDevice(token) => write!(formatter, "device {} from discovery generation {} is unavailable", token.ordinal, token.generation),
            Self::MissingRelinkDevice(preference) => write!(formatter, "no device matches {preference:?}"),
            Self::AmbiguousRelinkDevice { preference, matches } => write!(formatter, "device preference {preference:?} matches {matches} current devices"),
            Self::MissingDefault(direction) => write!(formatter, "backend reports no default {direction:?} device"),
            Self::AmbiguousDefault { direction, matches } => write!(formatter, "backend reports {matches} default {direction:?} devices"),
            Self::DirectionUnavailable { device, direction } => write!(formatter, "device {device:?} has no {direction:?} endpoint"),
            Self::NoCompatibleConfiguration { device, direction } => write!(formatter, "device {device:?} has no compatible {direction:?} configuration"),
            Self::SampleRateConversionUnavailable { requested, nearest } => write!(formatter, "project rate {requested} Hz needs explicit conversion to {nearest} Hz, but no converter is installed"),
            Self::IndependentDuplexClock => formatter.write_str("input and output do not have a backend-guaranteed shared clock"),
            Self::IndependentDuplexCompensationUnavailable => formatter.write_str("independent duplex clocks need drift compensation, but no asynchronous converter is installed"),
            Self::AlreadyOpen => formatter.write_str("close the current audio device before opening another"),
            Self::NotRunning => formatter.write_str("audio device is not running"),
            Self::EndpointNotOpen(direction) => write!(formatter, "{direction:?} endpoint is not open"),
            Self::Backend(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for DeviceServiceError {}

/// Monotonic process-local identity for one open stream generation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeviceStreamSessionId(pub u64);

/// One stream's bounded event observed from the control side.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendStreamEvent {
    DeviceLost(BackendFailure),
    ClockDiscontinuity,
    TelemetryAdvanced,
}

pub trait AudioDeviceStream {
    fn session_id(&self) -> DeviceStreamSessionId;
    fn poll_event(&mut self) -> Result<Option<BackendStreamEvent>, BackendFailure>;
    fn close(&mut self);
}

/// Device backend seam. Discovery and open run only on a control worker.
/// `open` installs callback ownership and may allocate; the resulting callback
/// may only touch its processor, preallocated backend buffers, and telemetry.
pub trait AudioDeviceBackend {
    type Stream: AudioDeviceStream;

    fn discover(&mut self) -> Result<Vec<BackendDeviceDescriptor>, BackendFailure>;
    fn open(
        &mut self,
        plan: &BackendOpenPlan,
        telemetry: RealtimeTelemetry,
    ) -> Result<Self::Stream, BackendFailure>;
}

/// Device-frame interval attached to one callback invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceStreamSpan {
    pub start: u64,
    pub frames: u32,
}

/// Fixed-size callback observation. Recording it touches atomics only.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CallbackObservation {
    pub input: Option<DeviceStreamSpan>,
    pub output: Option<DeviceStreamSpan>,
    pub callback_duration_nanos: u64,
    pub callback_deadline_nanos: u64,
    pub input_xrun: bool,
    pub output_xrun: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LatencyObservation {
    pub input_frames: Option<u32>,
    pub output_frames: Option<u32>,
    pub round_trip_frames: Option<u32>,
}

#[derive(Default)]
struct RealtimeTelemetryInner {
    callbacks: AtomicU64,
    input_callbacks: AtomicU64,
    output_callbacks: AtomicU64,
    input_frames: AtomicU64,
    output_frames: AtomicU64,
    input_xruns: AtomicU64,
    output_xruns: AtomicU64,
    callback_overruns: AtomicU64,
    peak_callback_nanos: AtomicU64,
    last_input_frame: AtomicU64,
    last_output_frame: AtomicU64,
    input_latency_present: AtomicBool,
    input_latency_frames: AtomicU64,
    output_latency_present: AtomicBool,
    output_latency_frames: AtomicU64,
    round_trip_latency_present: AtomicBool,
    round_trip_latency_frames: AtomicU64,
    device_losses: AtomicU64,
    reopen_attempts: AtomicU64,
    reopen_successes: AtomicU64,
}

/// Cloneable callback-safe telemetry writer. Snapshots are intentionally
/// approximate rather than seqlocked; counters remain monotonic and no metric
/// is project truth.
#[derive(Clone)]
pub struct RealtimeTelemetry {
    inner: Arc<RealtimeTelemetryInner>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeviceTelemetrySnapshot {
    pub callbacks: u64,
    pub input_callbacks: u64,
    pub output_callbacks: u64,
    pub input_frames: u64,
    pub output_frames: u64,
    pub input_xruns: u64,
    pub output_xruns: u64,
    pub callback_overruns: u64,
    pub peak_callback_nanos: u64,
    pub last_input_frame: Option<u64>,
    pub last_output_frame: Option<u64>,
    pub latency: LatencyObservation,
    pub device_losses: u64,
    pub reopen_attempts: u64,
    pub reopen_successes: u64,
}

impl RealtimeTelemetry {
    pub fn new() -> Self {
        let inner = RealtimeTelemetryInner::default();
        inner.last_input_frame.store(NO_FRAME, Ordering::Relaxed);
        inner.last_output_frame.store(NO_FRAME, Ordering::Relaxed);
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Callback-safe: fixed atomic operations only.
    pub fn observe_callback(&self, observation: CallbackObservation) {
        self.inner.callbacks.fetch_add(1, Ordering::Relaxed);
        self.inner
            .peak_callback_nanos
            .fetch_max(observation.callback_duration_nanos, Ordering::Relaxed);
        if observation.callback_deadline_nanos > 0
            && observation.callback_duration_nanos > observation.callback_deadline_nanos
        {
            self.inner.callback_overruns.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(span) = observation.input {
            self.inner.input_callbacks.fetch_add(1, Ordering::Relaxed);
            self.inner
                .input_frames
                .fetch_add(u64::from(span.frames), Ordering::Relaxed);
            self.inner.last_input_frame.store(
                span.start.saturating_add(u64::from(span.frames)),
                Ordering::Relaxed,
            );
        }
        if let Some(span) = observation.output {
            self.inner.output_callbacks.fetch_add(1, Ordering::Relaxed);
            self.inner
                .output_frames
                .fetch_add(u64::from(span.frames), Ordering::Relaxed);
            self.inner.last_output_frame.store(
                span.start.saturating_add(u64::from(span.frames)),
                Ordering::Relaxed,
            );
        }
        if observation.input_xrun {
            self.inner.input_xruns.fetch_add(1, Ordering::Relaxed);
        }
        if observation.output_xrun {
            self.inner.output_xruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Backend/control-side latency publication; no inference from nominal
    /// buffer size is performed.
    pub fn publish_latency(&self, latency: LatencyObservation) {
        publish_optional_u32(
            &self.inner.input_latency_present,
            &self.inner.input_latency_frames,
            latency.input_frames,
        );
        publish_optional_u32(
            &self.inner.output_latency_present,
            &self.inner.output_latency_frames,
            latency.output_frames,
        );
        publish_optional_u32(
            &self.inner.round_trip_latency_present,
            &self.inner.round_trip_latency_frames,
            latency.round_trip_frames,
        );
    }

    pub fn snapshot(&self) -> DeviceTelemetrySnapshot {
        let last_input = self.inner.last_input_frame.load(Ordering::Relaxed);
        let last_output = self.inner.last_output_frame.load(Ordering::Relaxed);
        DeviceTelemetrySnapshot {
            callbacks: self.inner.callbacks.load(Ordering::Relaxed),
            input_callbacks: self.inner.input_callbacks.load(Ordering::Relaxed),
            output_callbacks: self.inner.output_callbacks.load(Ordering::Relaxed),
            input_frames: self.inner.input_frames.load(Ordering::Relaxed),
            output_frames: self.inner.output_frames.load(Ordering::Relaxed),
            input_xruns: self.inner.input_xruns.load(Ordering::Relaxed),
            output_xruns: self.inner.output_xruns.load(Ordering::Relaxed),
            callback_overruns: self.inner.callback_overruns.load(Ordering::Relaxed),
            peak_callback_nanos: self.inner.peak_callback_nanos.load(Ordering::Relaxed),
            last_input_frame: (last_input != NO_FRAME).then_some(last_input),
            last_output_frame: (last_output != NO_FRAME).then_some(last_output),
            latency: LatencyObservation {
                input_frames: load_optional_u32(
                    &self.inner.input_latency_present,
                    &self.inner.input_latency_frames,
                ),
                output_frames: load_optional_u32(
                    &self.inner.output_latency_present,
                    &self.inner.output_latency_frames,
                ),
                round_trip_frames: load_optional_u32(
                    &self.inner.round_trip_latency_present,
                    &self.inner.round_trip_latency_frames,
                ),
            },
            device_losses: self.inner.device_losses.load(Ordering::Relaxed),
            reopen_attempts: self.inner.reopen_attempts.load(Ordering::Relaxed),
            reopen_successes: self.inner.reopen_successes.load(Ordering::Relaxed),
        }
    }

    fn record_device_loss(&self) {
        self.inner.device_losses.fetch_add(1, Ordering::Relaxed);
    }

    fn record_reopen_attempt(&self) {
        self.inner.reopen_attempts.fetch_add(1, Ordering::Relaxed);
    }

    fn record_reopen_success(&self) {
        self.inner.reopen_successes.fetch_add(1, Ordering::Relaxed);
    }
}

impl Default for RealtimeTelemetry {
    fn default() -> Self {
        Self::new()
    }
}

fn publish_optional_u32(present: &AtomicBool, value: &AtomicU64, next: Option<u32>) {
    if let Some(next) = next {
        value.store(u64::from(next), Ordering::Relaxed);
        present.store(true, Ordering::Release);
    } else {
        present.store(false, Ordering::Release);
    }
}

fn load_optional_u32(present: &AtomicBool, value: &AtomicU64) -> Option<u32> {
    present
        .load(Ordering::Acquire)
        .then(|| value.load(Ordering::Relaxed) as u32)
}

/// Callback context for the processor's canonical interleaved-f32 boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceCallbackContext {
    pub session: DeviceStreamSessionId,
    pub input: Option<DeviceStreamSpan>,
    pub output: Option<DeviceStreamSpan>,
}

pub struct DeviceCallbackBuffers<'a> {
    pub input_interleaved: Option<&'a [f32]>,
    pub output_interleaved: Option<&'a mut [f32]>,
}

/// Sole DSP entry point owned by a hardware callback.
///
/// Implementations must not allocate, lock, block, perform I/O, log, panic,
/// call GPUI, mutate the project, or rebuild a graph. A backend must validate
/// buffer/channel shapes before activation and silence output if activation
/// fails. It may not improvise a fallback configuration in the callback.
pub trait RealtimeDeviceProcessor: Send + 'static {
    fn process(&mut self, context: DeviceCallbackContext, buffers: DeviceCallbackBuffers<'_>);
}

/// Signed project-frame anchor paired with a stream-local frame counter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceClockAnchor {
    pub device_frame: u64,
    pub project_frame: i64,
}

/// Exact rational mapping for one endpoint clock. This mirrors MIDI's paired
/// timestamp calibration: no wall clock is consulted, and seek/reopen/rate
/// changes invalidate the anchor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectDeviceClockMap {
    direction: DeviceDirection,
    anchor: DeviceClockAnchor,
    project_sample_rate: NonZeroU32,
    device_sample_rate: NonZeroU32,
    latency_compensation_frames: u32,
}

impl ProjectDeviceClockMap {
    pub fn new(
        direction: DeviceDirection,
        anchor: DeviceClockAnchor,
        project_sample_rate: NonZeroU32,
        device_sample_rate: NonZeroU32,
        latency_compensation_frames: u32,
    ) -> Self {
        Self {
            direction,
            anchor,
            project_sample_rate,
            device_sample_rate,
            latency_compensation_frames,
        }
    }

    pub fn direction(self) -> DeviceDirection {
        self.direction
    }

    pub fn anchor(self) -> DeviceClockAnchor {
        self.anchor
    }

    /// Map device frames to the uncorrected project render clock.
    pub fn device_to_project(self, device_frame: u64) -> ClockMapOutcome<i64> {
        let delta = i128::from(device_frame) - i128::from(self.anchor.device_frame);
        let project_delta = round_ratio(
            delta * i128::from(self.project_sample_rate.get()),
            i128::from(self.device_sample_rate.get()),
        );
        let mapped = i128::from(self.anchor.project_frame) + project_delta;
        let (value, saturated) = saturating_i128_to_i64(mapped);
        ClockMapOutcome { value, saturated }
    }

    /// Map a project render frame back to the endpoint counter.
    pub fn project_to_device(self, project_frame: i64) -> ClockMapOutcome<u64> {
        let delta = i128::from(project_frame) - i128::from(self.anchor.project_frame);
        let device_delta = round_ratio(
            delta * i128::from(self.device_sample_rate.get()),
            i128::from(self.project_sample_rate.get()),
        );
        let mapped = i128::from(self.anchor.device_frame) + device_delta;
        let (value, saturated) = saturating_i128_to_u64(mapped);
        ClockMapOutcome { value, saturated }
    }

    /// Latency-compensated signal origin, matching MIDI input's convention of
    /// subtracting configured latency from the paired project frame.
    pub fn device_to_compensated_project(self, device_frame: u64) -> ClockMapOutcome<i64> {
        let raw = self.device_to_project(device_frame);
        let device_latency = i128::from(self.latency_compensation_frames);
        let project_latency = round_ratio(
            device_latency * i128::from(self.project_sample_rate.get()),
            i128::from(self.device_sample_rate.get()),
        );
        let (value, saturated_subtract) =
            saturating_i128_to_i64(i128::from(raw.value) - project_latency);
        ClockMapOutcome {
            value,
            saturated: raw.saturated || saturated_subtract,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockMapOutcome<T> {
    pub value: T,
    pub saturated: bool,
}

fn round_ratio(numerator: i128, denominator: i128) -> i128 {
    debug_assert!(denominator > 0);
    if numerator >= 0 {
        (numerator + denominator / 2) / denominator
    } else {
        (numerator - denominator / 2) / denominator
    }
}

fn saturating_i128_to_i64(value: i128) -> (i64, bool) {
    if value > i128::from(i64::MAX) {
        (i64::MAX, true)
    } else if value < i128::from(i64::MIN) {
        (i64::MIN, true)
    } else {
        (value as i64, false)
    }
}

fn saturating_i128_to_u64(value: i128) -> (u64, bool) {
    if value > i128::from(u64::MAX) {
        (u64::MAX, true)
    } else if value < 0 {
        (0, true)
    } else {
        (value as u64, false)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ClockSet {
    input: Option<ProjectDeviceClockMap>,
    output: Option<ProjectDeviceClockMap>,
}

impl ClockSet {
    fn get(self, direction: DeviceDirection) -> Option<ProjectDeviceClockMap> {
        match direction {
            DeviceDirection::Input => self.input,
            DeviceDirection::Output => self.output,
        }
    }

    fn set(&mut self, direction: DeviceDirection, clock: ProjectDeviceClockMap) {
        match direction {
            DeviceDirection::Input => self.input = Some(clock),
            DeviceDirection::Output => self.output = Some(clock),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceServicePhase {
    Ready,
    Opening,
    Running,
    Recovering,
    Faulted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceServiceSnapshot {
    pub phase: DeviceServicePhase,
    pub catalog_generation: u64,
    pub session: Option<DeviceStreamSessionId>,
    pub plan: Option<NegotiatedDevicePlan>,
    pub input_clock_anchored: bool,
    pub output_clock_anchored: bool,
    pub recovery_attempts: u16,
    pub recovery_polls_remaining: u32,
    pub last_failure: Option<BackendFailure>,
    pub telemetry: DeviceTelemetrySnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceServiceEvent {
    CatalogRefreshed {
        generation: u64,
        devices: usize,
    },
    Opened {
        session: DeviceStreamSessionId,
        plan: NegotiatedDevicePlan,
    },
    Closed,
    DeviceLost {
        failure: BackendFailure,
        recovery_scheduled: bool,
    },
    RecoveryWaiting {
        attempts: u16,
        polls_remaining: u32,
    },
    ReopenFailed {
        attempt: u16,
        failure: BackendFailure,
    },
    Reopened {
        attempt: u16,
        session: DeviceStreamSessionId,
        plan: NegotiatedDevicePlan,
    },
    Faulted {
        failure: BackendFailure,
    },
    ClockAnchored {
        direction: DeviceDirection,
        anchor: DeviceClockAnchor,
    },
    ClockInvalidated,
    TelemetryAdvanced,
}

#[derive(Clone)]
struct CatalogEntry {
    public: AudioDeviceDescriptor,
    backend_id: BackendDeviceId,
}

struct ActiveStream<S> {
    stream: S,
    settings: DeviceOpenSettings,
    plan: BackendOpenPlan,
    clocks: ClockSet,
}

struct RecoveryState {
    settings: DeviceOpenSettings,
    attempts: u16,
    polls_remaining: u32,
    failure: BackendFailure,
}

enum ServiceState<S> {
    Ready,
    Opening,
    Running(ActiveStream<S>),
    Recovering(RecoveryState),
    Faulted { failure: BackendFailure },
}

/// Headless control-side device service. The generic backend makes discovery,
/// loss, and retry logic deterministic under simulation without making a
/// hardware-support claim.
pub struct AudioDeviceService<B: AudioDeviceBackend> {
    backend: B,
    features: DeviceServiceFeatures,
    catalog_generation: u64,
    catalog: Vec<CatalogEntry>,
    state: ServiceState<B::Stream>,
    telemetry: RealtimeTelemetry,
}

impl<B: AudioDeviceBackend> AudioDeviceService<B> {
    pub fn new(backend: B, features: DeviceServiceFeatures) -> Self {
        Self {
            backend,
            features,
            catalog_generation: 0,
            catalog: Vec::new(),
            state: ServiceState::Ready,
            telemetry: RealtimeTelemetry::new(),
        }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    pub fn catalog(&self) -> Vec<AudioDeviceDescriptor> {
        self.catalog
            .iter()
            .map(|entry| entry.public.clone())
            .collect()
    }

    pub fn telemetry(&self) -> &RealtimeTelemetry {
        &self.telemetry
    }

    pub fn refresh_catalog(&mut self) -> Result<DeviceServiceEvent, DeviceServiceError> {
        let devices = self.refresh_catalog_inner()?;
        Ok(DeviceServiceEvent::CatalogRefreshed {
            generation: self.catalog_generation,
            devices,
        })
    }

    fn refresh_catalog_inner(&mut self) -> Result<usize, DeviceServiceError> {
        // As with MIDI discovery, a failed refresh must invalidate the former
        // runtime tokens. They describe a catalog we just failed to prove is
        // still current and therefore may not remain silently selectable.
        self.catalog_generation = self.catalog_generation.wrapping_add(1).max(1);
        self.catalog.clear();
        let mut discovered = self
            .backend
            .discover()
            .map_err(DeviceServiceError::Backend)?;
        if discovered.len() > u32::MAX as usize {
            self.catalog.clear();
            return Err(DeviceServiceError::TooManyDevices(discovered.len()));
        }
        discovered.sort_by(|left, right| {
            (
                &left.backend_name,
                &left.display_name,
                &left.stable_hardware_id,
                &left.runtime_id,
            )
                .cmp(&(
                    &right.backend_name,
                    &right.display_name,
                    &right.stable_hardware_id,
                    &right.runtime_id,
                ))
        });
        let mut backend_ids = BTreeSet::new();
        for descriptor in &discovered {
            if !backend_ids.insert(descriptor.runtime_id.clone()) {
                self.catalog.clear();
                return Err(DeviceServiceError::DuplicateBackendId(
                    descriptor.runtime_id.clone(),
                ));
            }
        }
        let generation = self.catalog_generation;
        self.catalog = discovered
            .into_iter()
            .enumerate()
            .map(|(ordinal, descriptor)| {
                let relink = DeviceMatch {
                    backend_name: descriptor.backend_name,
                    display_name: descriptor.display_name.clone(),
                    stable_hardware_id: descriptor.stable_hardware_id,
                };
                CatalogEntry {
                    backend_id: descriptor.runtime_id,
                    public: AudioDeviceDescriptor {
                        token: DeviceToken {
                            generation,
                            ordinal: ordinal as u32,
                        },
                        display_name: descriptor.display_name,
                        relink,
                        default_input: descriptor.default_input,
                        default_output: descriptor.default_output,
                        input_configs: descriptor.input_configs,
                        output_configs: descriptor.output_configs,
                        duplex_clock: descriptor.duplex_clock,
                    },
                }
            })
            .collect();
        Ok(self.catalog.len())
    }

    pub fn open(
        &mut self,
        settings: DeviceOpenSettings,
    ) -> Result<DeviceServiceEvent, DeviceServiceError> {
        if matches!(self.state, ServiceState::Running(_) | ServiceState::Opening) {
            return Err(DeviceServiceError::AlreadyOpen);
        }
        if settings.input.is_none() && settings.output.is_none() {
            return Err(DeviceServiceError::NoEndpointsRequested);
        }
        if self.catalog.is_empty() {
            self.refresh_catalog_inner()?;
        }
        let plan = self.negotiate(&settings)?;
        self.state = ServiceState::Opening;
        match self.backend.open(&plan, self.telemetry.clone()) {
            Ok(stream) => {
                let session = stream.session_id();
                let public = plan.public.clone();
                self.state = ServiceState::Running(ActiveStream {
                    stream,
                    settings,
                    plan,
                    clocks: ClockSet::default(),
                });
                Ok(DeviceServiceEvent::Opened {
                    session,
                    plan: public,
                })
            }
            Err(failure) => {
                self.state = ServiceState::Faulted {
                    failure: failure.clone(),
                };
                Err(DeviceServiceError::Backend(failure))
            }
        }
    }

    pub fn close(&mut self) -> DeviceServiceEvent {
        let previous = std::mem::replace(&mut self.state, ServiceState::Ready);
        if let ServiceState::Running(mut active) = previous {
            active.stream.close();
        }
        DeviceServiceEvent::Closed
    }

    pub fn snapshot(&self) -> DeviceServiceSnapshot {
        let mut snapshot = DeviceServiceSnapshot {
            phase: DeviceServicePhase::Ready,
            catalog_generation: self.catalog_generation,
            session: None,
            plan: None,
            input_clock_anchored: false,
            output_clock_anchored: false,
            recovery_attempts: 0,
            recovery_polls_remaining: 0,
            last_failure: None,
            telemetry: self.telemetry.snapshot(),
        };
        match &self.state {
            ServiceState::Ready => {}
            ServiceState::Opening => snapshot.phase = DeviceServicePhase::Opening,
            ServiceState::Running(active) => {
                snapshot.phase = DeviceServicePhase::Running;
                snapshot.session = Some(active.stream.session_id());
                snapshot.plan = Some(active.plan.public.clone());
                snapshot.input_clock_anchored = active.clocks.input.is_some();
                snapshot.output_clock_anchored = active.clocks.output.is_some();
            }
            ServiceState::Recovering(recovery) => {
                snapshot.phase = DeviceServicePhase::Recovering;
                snapshot.recovery_attempts = recovery.attempts;
                snapshot.recovery_polls_remaining = recovery.polls_remaining;
                snapshot.last_failure = Some(recovery.failure.clone());
            }
            ServiceState::Faulted { failure } => {
                snapshot.phase = DeviceServicePhase::Faulted;
                snapshot.last_failure = Some(failure.clone());
            }
        }
        snapshot
    }

    pub fn clock_map(&self, direction: DeviceDirection) -> Option<ProjectDeviceClockMap> {
        match &self.state {
            ServiceState::Running(active) => active.clocks.get(direction),
            _ => None,
        }
    }

    /// Pair a backend stream counter with the authoritative project render
    /// frame. Re-anchor after seek, transport discontinuity, rate change, or
    /// backend `ClockDiscontinuity`; reopen always starts unanchored.
    pub fn anchor_clock(
        &mut self,
        direction: DeviceDirection,
        anchor: DeviceClockAnchor,
    ) -> Result<DeviceServiceEvent, DeviceServiceError> {
        let ServiceState::Running(active) = &mut self.state else {
            return Err(DeviceServiceError::NotRunning);
        };
        let endpoint = match direction {
            DeviceDirection::Input => active.plan.public.input.as_ref(),
            DeviceDirection::Output => active.plan.public.output.as_ref(),
        }
        .ok_or(DeviceServiceError::EndpointNotOpen(direction))?;
        let map = ProjectDeviceClockMap::new(
            direction,
            anchor,
            active.plan.public.project_sample_rate,
            endpoint.config.sample_rate,
            endpoint.latency_compensation_frames,
        );
        active.clocks.set(direction, map);
        Ok(DeviceServiceEvent::ClockAnchored { direction, anchor })
    }

    pub fn poll(&mut self) -> Result<Option<DeviceServiceEvent>, DeviceServiceError> {
        let previous = std::mem::replace(&mut self.state, ServiceState::Ready);
        match previous {
            ServiceState::Running(mut active) => match active.stream.poll_event() {
                Ok(None) => {
                    self.state = ServiceState::Running(active);
                    Ok(None)
                }
                Ok(Some(BackendStreamEvent::TelemetryAdvanced)) => {
                    self.state = ServiceState::Running(active);
                    Ok(Some(DeviceServiceEvent::TelemetryAdvanced))
                }
                Ok(Some(BackendStreamEvent::ClockDiscontinuity)) => {
                    active.clocks = ClockSet::default();
                    self.state = ServiceState::Running(active);
                    Ok(Some(DeviceServiceEvent::ClockInvalidated))
                }
                Ok(Some(BackendStreamEvent::DeviceLost(failure))) | Err(failure) => {
                    active.stream.close();
                    self.telemetry.record_device_loss();
                    let recovery_scheduled = active.settings.recovery.reopen_after_loss;
                    if recovery_scheduled {
                        let delay = active.settings.recovery.retry_delay_polls;
                        self.state = ServiceState::Recovering(RecoveryState {
                            settings: active.settings,
                            attempts: 0,
                            polls_remaining: delay,
                            failure: failure.clone(),
                        });
                    } else {
                        self.state = ServiceState::Faulted {
                            failure: failure.clone(),
                        };
                    }
                    Ok(Some(DeviceServiceEvent::DeviceLost {
                        failure,
                        recovery_scheduled,
                    }))
                }
            },
            ServiceState::Recovering(mut recovery) => {
                if recovery.polls_remaining > 0 {
                    recovery.polls_remaining -= 1;
                    let event = DeviceServiceEvent::RecoveryWaiting {
                        attempts: recovery.attempts,
                        polls_remaining: recovery.polls_remaining,
                    };
                    self.state = ServiceState::Recovering(recovery);
                    return Ok(Some(event));
                }
                recovery.attempts = recovery.attempts.saturating_add(1);
                self.telemetry.record_reopen_attempt();
                let attempt = recovery.attempts;
                let result = self
                    .refresh_catalog_inner()
                    .and_then(|_| self.negotiate(&recovery.settings))
                    .and_then(|plan| {
                        self.backend
                            .open(&plan, self.telemetry.clone())
                            .map(|stream| (plan, stream))
                            .map_err(DeviceServiceError::Backend)
                    });
                match result {
                    Ok((plan, stream)) => {
                        let session = stream.session_id();
                        let public = plan.public.clone();
                        self.telemetry.record_reopen_success();
                        self.state = ServiceState::Running(ActiveStream {
                            stream,
                            settings: recovery.settings,
                            plan,
                            clocks: ClockSet::default(),
                        });
                        Ok(Some(DeviceServiceEvent::Reopened {
                            attempt,
                            session,
                            plan: public,
                        }))
                    }
                    Err(error) => {
                        let failure = service_error_as_backend_failure(&error);
                        if attempt >= recovery.settings.recovery.max_attempts.get() {
                            self.state = ServiceState::Faulted {
                                failure: failure.clone(),
                            };
                            Ok(Some(DeviceServiceEvent::Faulted { failure }))
                        } else {
                            recovery.failure = failure.clone();
                            recovery.polls_remaining = recovery.settings.recovery.retry_delay_polls;
                            self.state = ServiceState::Recovering(recovery);
                            Ok(Some(DeviceServiceEvent::ReopenFailed { attempt, failure }))
                        }
                    }
                }
            }
            other => {
                self.state = other;
                Ok(None)
            }
        }
    }

    fn negotiate(
        &self,
        settings: &DeviceOpenSettings,
    ) -> Result<BackendOpenPlan, DeviceServiceError> {
        let output = settings
            .output
            .as_ref()
            .map(|endpoint| self.negotiate_endpoint(endpoint, DeviceDirection::Output, settings))
            .transpose()?;
        let input = settings
            .input
            .as_ref()
            .map(|endpoint| self.negotiate_endpoint(endpoint, DeviceDirection::Input, settings))
            .transpose()?;
        let clocking = match (&input, &output) {
            (Some((input_entry, input)), Some((output_entry, output))) => {
                let shared = input_entry.backend_id == output_entry.backend_id
                    && input.config.sample_rate == output.config.sample_rate
                    && input_entry.public.duplex_clock == DuplexClockCapability::GuaranteedShared;
                if shared {
                    DuplexClocking::Shared
                } else {
                    match settings.duplex_clock_policy {
                        DuplexClockPolicy::RequireShared => {
                            return Err(DeviceServiceError::IndependentDuplexClock)
                        }
                        DuplexClockPolicy::AllowIndependentWithCompensation
                            if !self.features.independent_duplex_clock_compensation =>
                        {
                            return Err(
                                DeviceServiceError::IndependentDuplexCompensationUnavailable,
                            )
                        }
                        DuplexClockPolicy::AllowIndependentWithCompensation => {
                            DuplexClocking::IndependentWithCompensation
                        }
                    }
                }
            }
            (Some(_), None) => DuplexClocking::InputOnly,
            (None, Some(_)) => DuplexClocking::OutputOnly,
            (None, None) => return Err(DeviceServiceError::NoEndpointsRequested),
        };
        Ok(BackendOpenPlan {
            public: NegotiatedDevicePlan {
                project_sample_rate: settings.project_sample_rate,
                output: output.as_ref().map(|(_, endpoint)| endpoint.clone()),
                input: input.as_ref().map(|(_, endpoint)| endpoint.clone()),
                clocking,
            },
            output_runtime_id: output.map(|(entry, _)| entry.backend_id.clone()),
            input_runtime_id: input.map(|(entry, _)| entry.backend_id.clone()),
        })
    }

    fn negotiate_endpoint<'a>(
        &'a self,
        endpoint: &EndpointSettings,
        direction: DeviceDirection,
        settings: &DeviceOpenSettings,
    ) -> Result<(&'a CatalogEntry, NegotiatedEndpoint), DeviceServiceError> {
        let entry = self.resolve_selection(&endpoint.selection, direction)?;
        let capabilities = match direction {
            DeviceDirection::Input => &entry.public.input_configs,
            DeviceDirection::Output => &entry.public.output_configs,
        };
        if capabilities.is_empty() {
            return Err(DeviceServiceError::DirectionUnavailable {
                device: entry.public.display_name.clone(),
                direction,
            });
        }
        let requested_rate = settings.project_sample_rate;
        let mut best: Option<(
            (bool, u64, usize, u64, u32, DeviceSampleFormat),
            NegotiatedStreamConfig,
        )> = None;
        for capability in capabilities
            .iter()
            .copied()
            .filter(|candidate| candidate.channels == endpoint.channels)
        {
            let chosen_rate = if capability.sample_rates.contains(requested_rate) {
                requested_rate
            } else {
                capability.sample_rates.nearest(requested_rate)
            };
            let needs_conversion = chosen_rate != requested_rate;
            if needs_conversion
                && settings.sample_rate_policy == SampleRatePolicy::RequireProjectRate
            {
                continue;
            }
            let Some(buffer_frames) = capability.buffer_sizes.choose(settings.buffer_size) else {
                continue;
            };
            let format_rank = settings
                .sample_formats
                .iter()
                .position(|format| *format == capability.sample_format)
                .unwrap_or_else(|| {
                    settings.sample_formats.len() + capability.sample_format.default_rank()
                });
            let buffer_target = match settings.buffer_size {
                BufferSizeRequest::BackendDefault => buffer_frames,
                BufferSizeRequest::Exact(target) | BufferSizeRequest::Prefer(target) => target,
            };
            let score = (
                needs_conversion,
                u64::from(chosen_rate.get().abs_diff(requested_rate.get())),
                format_rank,
                u64::from(buffer_frames.get().abs_diff(buffer_target.get())),
                buffer_frames.get(),
                capability.sample_format,
            );
            let config = NegotiatedStreamConfig {
                channels: capability.channels,
                sample_rate: chosen_rate,
                sample_format: capability.sample_format,
                buffer_frames,
            };
            if best
                .as_ref()
                .is_none_or(|(best_score, _)| score < *best_score)
            {
                best = Some((score, config));
            }
        }
        let Some((_, config)) = best else {
            return Err(DeviceServiceError::NoCompatibleConfiguration {
                device: entry.public.display_name.clone(),
                direction,
            });
        };
        if config.sample_rate != requested_rate && !self.features.sample_rate_conversion {
            return Err(DeviceServiceError::SampleRateConversionUnavailable {
                requested: requested_rate.get(),
                nearest: config.sample_rate.get(),
            });
        }
        Ok((
            entry,
            NegotiatedEndpoint {
                token: entry.public.token,
                display_name: entry.public.display_name.clone(),
                relink: entry.public.relink.clone(),
                config,
                requires_sample_rate_conversion: config.sample_rate != requested_rate,
                latency_compensation_frames: endpoint.latency_compensation_frames,
            },
        ))
    }

    fn resolve_selection(
        &self,
        selection: &DeviceSelection,
        direction: DeviceDirection,
    ) -> Result<&CatalogEntry, DeviceServiceError> {
        match selection {
            DeviceSelection::Runtime {
                token,
                relink_fallback,
            } => {
                if token.generation == self.catalog_generation {
                    if let Some(entry) = self
                        .catalog
                        .get(token.ordinal as usize)
                        .filter(|entry| entry.public.token == *token)
                    {
                        return Ok(entry);
                    }
                    if relink_fallback.is_none() {
                        return Err(DeviceServiceError::MissingRuntimeDevice(*token));
                    }
                } else if relink_fallback.is_none() {
                    return Err(DeviceServiceError::StaleSelection {
                        requested: token.generation,
                        current: self.catalog_generation,
                    });
                }
                self.resolve_relink(relink_fallback.as_ref().expect("checked above"), direction)
            }
            DeviceSelection::Relink(preference) => self.resolve_relink(preference, direction),
            DeviceSelection::Default => {
                let matches: Vec<_> = self
                    .catalog
                    .iter()
                    .filter(|entry| match direction {
                        DeviceDirection::Input => entry.public.default_input,
                        DeviceDirection::Output => entry.public.default_output,
                    })
                    .filter(|entry| entry.public.supports(direction))
                    .collect();
                match matches.as_slice() {
                    [one] => Ok(*one),
                    [] => Err(DeviceServiceError::MissingDefault(direction)),
                    many => Err(DeviceServiceError::AmbiguousDefault {
                        direction,
                        matches: many.len(),
                    }),
                }
            }
        }
    }

    fn resolve_relink(
        &self,
        preference: &DeviceMatch,
        direction: DeviceDirection,
    ) -> Result<&CatalogEntry, DeviceServiceError> {
        let matches: Vec<_> = self
            .catalog
            .iter()
            .filter(|entry| {
                let actual = &entry.public.relink;
                actual.backend_name == preference.backend_name
                    && match &preference.stable_hardware_id {
                        Some(stable) => actual.stable_hardware_id.as_ref() == Some(stable),
                        None => actual.display_name == preference.display_name,
                    }
                    && entry.public.supports(direction)
            })
            .collect();
        match matches.as_slice() {
            [one] => Ok(*one),
            [] => Err(DeviceServiceError::MissingRelinkDevice(preference.clone())),
            many => Err(DeviceServiceError::AmbiguousRelinkDevice {
                preference: preference.clone(),
                matches: many.len(),
            }),
        }
    }
}

fn service_error_as_backend_failure(error: &DeviceServiceError) -> BackendFailure {
    match error {
        DeviceServiceError::Backend(failure) => failure.clone(),
        other => BackendFailure::new("reopen-refused", other.to_string()),
    }
}

/// Deterministic backend script used by state-machine and future controller
/// tests. It owns no thread, timer, callback, or hardware handle.
#[derive(Clone, Debug)]
pub enum SimulatedOpenOutcome {
    Open {
        events: VecDeque<SimulatedStreamEvent>,
    },
    Fail(BackendFailure),
}

#[derive(Clone, Debug)]
pub enum SimulatedStreamEvent {
    Callback(CallbackObservation),
    DeviceLost(BackendFailure),
    ClockDiscontinuity,
}

pub struct SimulatedAudioBackend {
    devices: Vec<BackendDeviceDescriptor>,
    discovery_failure: Option<BackendFailure>,
    open_outcomes: VecDeque<SimulatedOpenOutcome>,
    opened_plans: Vec<BackendOpenPlan>,
    next_session: u64,
}

impl SimulatedAudioBackend {
    pub fn new(devices: Vec<BackendDeviceDescriptor>) -> Self {
        Self {
            devices,
            discovery_failure: None,
            open_outcomes: VecDeque::new(),
            opened_plans: Vec::new(),
            next_session: 1,
        }
    }

    pub fn script_open(&mut self, outcome: SimulatedOpenOutcome) {
        self.open_outcomes.push_back(outcome);
    }

    pub fn set_devices(&mut self, devices: Vec<BackendDeviceDescriptor>) {
        self.devices = devices;
    }

    pub fn fail_discovery(&mut self, failure: Option<BackendFailure>) {
        self.discovery_failure = failure;
    }

    pub fn opened_plans(&self) -> &[BackendOpenPlan] {
        &self.opened_plans
    }
}

pub struct SimulatedAudioStream {
    session: DeviceStreamSessionId,
    events: VecDeque<SimulatedStreamEvent>,
    telemetry: RealtimeTelemetry,
    closed: bool,
}

impl AudioDeviceStream for SimulatedAudioStream {
    fn session_id(&self) -> DeviceStreamSessionId {
        self.session
    }

    fn poll_event(&mut self) -> Result<Option<BackendStreamEvent>, BackendFailure> {
        if self.closed {
            return Ok(None);
        }
        match self.events.pop_front() {
            Some(SimulatedStreamEvent::Callback(observation)) => {
                self.telemetry.observe_callback(observation);
                Ok(Some(BackendStreamEvent::TelemetryAdvanced))
            }
            Some(SimulatedStreamEvent::DeviceLost(failure)) => {
                Ok(Some(BackendStreamEvent::DeviceLost(failure)))
            }
            Some(SimulatedStreamEvent::ClockDiscontinuity) => {
                Ok(Some(BackendStreamEvent::ClockDiscontinuity))
            }
            None => Ok(None),
        }
    }

    fn close(&mut self) {
        self.closed = true;
    }
}

impl AudioDeviceBackend for SimulatedAudioBackend {
    type Stream = SimulatedAudioStream;

    fn discover(&mut self) -> Result<Vec<BackendDeviceDescriptor>, BackendFailure> {
        if let Some(failure) = &self.discovery_failure {
            return Err(failure.clone());
        }
        Ok(self.devices.clone())
    }

    fn open(
        &mut self,
        plan: &BackendOpenPlan,
        telemetry: RealtimeTelemetry,
    ) -> Result<Self::Stream, BackendFailure> {
        self.opened_plans.push(plan.clone());
        let outcome =
            self.open_outcomes
                .pop_front()
                .unwrap_or_else(|| SimulatedOpenOutcome::Open {
                    events: VecDeque::new(),
                });
        match outcome {
            SimulatedOpenOutcome::Open { events } => {
                let session = DeviceStreamSessionId(self.next_session);
                self.next_session = self.next_session.wrapping_add(1).max(1);
                Ok(SimulatedAudioStream {
                    session,
                    events,
                    telemetry,
                    closed: false,
                })
            }
            SimulatedOpenOutcome::Fail(failure) => Err(failure),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nz32(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap()
    }

    fn nz16(value: u16) -> NonZeroU16 {
        NonZeroU16::new(value).unwrap()
    }

    fn config(
        channels: u16,
        format: DeviceSampleFormat,
        min_rate: u32,
        max_rate: u32,
        min_buffer: u32,
        max_buffer: u32,
        default_buffer: u32,
    ) -> SupportedStreamConfig {
        SupportedStreamConfig {
            channels: nz16(channels),
            sample_format: format,
            sample_rates: InclusiveU32Range::new(min_rate, max_rate).unwrap(),
            buffer_sizes: BufferSizeSupport::range(min_buffer, max_buffer, default_buffer).unwrap(),
        }
    }

    fn duplex_device(id: &str, name: &str, stable: Option<&str>) -> BackendDeviceDescriptor {
        let configs = vec![
            config(2, DeviceSampleFormat::I16, 44_100, 96_000, 64, 1_024, 512),
            config(2, DeviceSampleFormat::F32, 44_100, 96_000, 32, 512, 128),
        ];
        BackendDeviceDescriptor {
            runtime_id: BackendDeviceId(id.into()),
            backend_name: "sim".into(),
            display_name: name.into(),
            stable_hardware_id: stable.map(str::to_owned),
            default_input: true,
            default_output: true,
            input_configs: configs.clone(),
            output_configs: configs,
            duplex_clock: DuplexClockCapability::GuaranteedShared,
        }
    }

    fn output_settings(selection: DeviceSelection) -> DeviceOpenSettings {
        DeviceOpenSettings {
            project_sample_rate: nz32(48_000),
            output: Some(EndpointSettings {
                selection,
                channels: nz16(2),
                latency_compensation_frames: 96,
            }),
            input: None,
            sample_rate_policy: SampleRatePolicy::RequireProjectRate,
            buffer_size: BufferSizeRequest::Prefer(nz32(256)),
            sample_formats: vec![DeviceSampleFormat::F32, DeviceSampleFormat::I16],
            duplex_clock_policy: DuplexClockPolicy::RequireShared,
            recovery: RecoveryPolicy::default(),
        }
    }

    #[test]
    fn discovery_and_negotiation_are_stable_and_explicit() {
        let backend = SimulatedAudioBackend::new(vec![
            duplex_device("z", "Zulu", Some("z-serial")),
            duplex_device("a", "Alpha", Some("a-serial")),
        ]);
        let mut service = AudioDeviceService::new(backend, DeviceServiceFeatures::default());
        let event = service.refresh_catalog().unwrap();
        assert_eq!(
            event,
            DeviceServiceEvent::CatalogRefreshed {
                generation: 1,
                devices: 2
            }
        );
        let catalog = service.catalog();
        assert_eq!(catalog[0].display_name, "Alpha");
        assert_eq!(
            catalog[0].token,
            DeviceToken {
                generation: 1,
                ordinal: 0
            }
        );

        let opened = service
            .open(output_settings(DeviceSelection::Runtime {
                token: catalog[0].token,
                relink_fallback: Some(catalog[0].relink.clone()),
            }))
            .unwrap();
        let DeviceServiceEvent::Opened { plan, .. } = opened else {
            panic!("unexpected event")
        };
        let endpoint = plan.output.unwrap();
        assert_eq!(endpoint.config.sample_rate, nz32(48_000));
        assert_eq!(endpoint.config.sample_format, DeviceSampleFormat::F32);
        assert_eq!(endpoint.config.buffer_frames, nz32(256));
        assert!(!endpoint.requires_sample_rate_conversion);
        assert_eq!(service.backend().opened_plans().len(), 1);
    }

    #[test]
    fn sample_rate_conversion_is_never_implicit() {
        let mut device = duplex_device("one", "Interface", Some("serial"));
        device.output_configs = vec![config(
            2,
            DeviceSampleFormat::F32,
            44_100,
            44_100,
            128,
            128,
            128,
        )];
        let backend = SimulatedAudioBackend::new(vec![device.clone()]);
        let mut service = AudioDeviceService::new(backend, DeviceServiceFeatures::default());
        service.refresh_catalog().unwrap();
        let mut settings = output_settings(DeviceSelection::Default);
        settings.sample_rate_policy = SampleRatePolicy::AllowExplicitConversion;
        assert_eq!(
            service.open(settings.clone()).unwrap_err(),
            DeviceServiceError::SampleRateConversionUnavailable {
                requested: 48_000,
                nearest: 44_100
            }
        );

        let backend = SimulatedAudioBackend::new(vec![device]);
        let mut service = AudioDeviceService::new(
            backend,
            DeviceServiceFeatures {
                sample_rate_conversion: true,
                independent_duplex_clock_compensation: false,
            },
        );
        service.refresh_catalog().unwrap();
        let DeviceServiceEvent::Opened { plan, .. } = service.open(settings).unwrap() else {
            panic!("expected open")
        };
        assert!(plan.output.unwrap().requires_sample_rate_conversion);
    }

    #[test]
    fn duplex_needs_shared_clock_or_an_installed_compensator() {
        let mut input = duplex_device("in", "Input", Some("in"));
        input.default_output = false;
        input.output_configs.clear();
        input.duplex_clock = DuplexClockCapability::UnknownOrIndependent;
        let mut output = duplex_device("out", "Output", Some("out"));
        output.default_input = false;
        output.input_configs.clear();
        output.duplex_clock = DuplexClockCapability::UnknownOrIndependent;
        let backend = SimulatedAudioBackend::new(vec![input.clone(), output.clone()]);
        let mut service = AudioDeviceService::new(backend, DeviceServiceFeatures::default());
        service.refresh_catalog().unwrap();
        let catalog = service.catalog();
        let input_descriptor = catalog
            .iter()
            .find(|device| device.display_name == "Input")
            .unwrap();
        let output_descriptor = catalog
            .iter()
            .find(|device| device.display_name == "Output")
            .unwrap();
        let mut settings =
            output_settings(DeviceSelection::Relink(output_descriptor.relink.clone()));
        settings.input = Some(EndpointSettings {
            selection: DeviceSelection::Relink(input_descriptor.relink.clone()),
            channels: nz16(2),
            latency_compensation_frames: 48,
        });
        assert_eq!(
            service.open(settings.clone()).unwrap_err(),
            DeviceServiceError::IndependentDuplexClock
        );

        settings.duplex_clock_policy = DuplexClockPolicy::AllowIndependentWithCompensation;
        assert_eq!(
            service.open(settings.clone()).unwrap_err(),
            DeviceServiceError::IndependentDuplexCompensationUnavailable
        );

        let backend = SimulatedAudioBackend::new(vec![input, output]);
        let mut service = AudioDeviceService::new(
            backend,
            DeviceServiceFeatures {
                sample_rate_conversion: false,
                independent_duplex_clock_compensation: true,
            },
        );
        service.refresh_catalog().unwrap();
        let DeviceServiceEvent::Opened { plan, .. } = service.open(settings).unwrap() else {
            panic!("expected open")
        };
        assert_eq!(plan.clocking, DuplexClocking::IndependentWithCompensation);
    }

    #[test]
    fn clock_mapping_is_signed_rational_and_latency_compensated() {
        let map = ProjectDeviceClockMap::new(
            DeviceDirection::Input,
            DeviceClockAnchor {
                device_frame: 44_100,
                project_frame: 48_000,
            },
            nz32(48_000),
            nz32(44_100),
            441,
        );
        assert_eq!(map.device_to_project(44_100).value, 48_000);
        assert_eq!(map.device_to_project(88_200).value, 96_000);
        assert_eq!(map.device_to_project(0).value, 0);
        assert_eq!(map.device_to_compensated_project(44_100).value, 47_520);
        assert_eq!(map.project_to_device(96_000).value, 88_200);
        assert_eq!(
            map.project_to_device(i64::MIN),
            ClockMapOutcome {
                value: 0,
                saturated: true
            }
        );
    }

    #[test]
    fn device_loss_relinks_reopens_and_invalidates_clock() {
        let device = duplex_device("one", "Interface", Some("serial"));
        let relink = DeviceMatch {
            backend_name: "sim".into(),
            display_name: "Interface".into(),
            stable_hardware_id: Some("serial".into()),
        };
        let mut backend = SimulatedAudioBackend::new(vec![device]);
        backend.script_open(SimulatedOpenOutcome::Open {
            events: VecDeque::from([SimulatedStreamEvent::DeviceLost(BackendFailure::new(
                "unplugged",
                "interface disappeared",
            ))]),
        });
        backend.script_open(SimulatedOpenOutcome::Open {
            events: VecDeque::new(),
        });
        let mut service = AudioDeviceService::new(backend, DeviceServiceFeatures::default());
        service.refresh_catalog().unwrap();
        let mut settings = output_settings(DeviceSelection::Relink(relink));
        settings.recovery.retry_delay_polls = 1;
        service.open(settings).unwrap();
        service
            .anchor_clock(
                DeviceDirection::Output,
                DeviceClockAnchor {
                    device_frame: 0,
                    project_frame: 100,
                },
            )
            .unwrap();
        assert!(service.clock_map(DeviceDirection::Output).is_some());

        let lost = service.poll().unwrap().unwrap();
        assert!(matches!(
            lost,
            DeviceServiceEvent::DeviceLost {
                recovery_scheduled: true,
                ..
            }
        ));
        assert_eq!(service.snapshot().phase, DeviceServicePhase::Recovering);
        assert_eq!(service.telemetry().snapshot().device_losses, 1);
        assert!(matches!(
            service.poll().unwrap(),
            Some(DeviceServiceEvent::RecoveryWaiting {
                polls_remaining: 0,
                ..
            })
        ));
        let reopened = service.poll().unwrap().unwrap();
        assert!(matches!(
            reopened,
            DeviceServiceEvent::Reopened { attempt: 1, .. }
        ));
        let snapshot = service.snapshot();
        assert_eq!(snapshot.phase, DeviceServicePhase::Running);
        assert!(!snapshot.output_clock_anchored);
        assert_eq!(snapshot.catalog_generation, 2);
        assert_eq!(snapshot.telemetry.reopen_attempts, 1);
        assert_eq!(snapshot.telemetry.reopen_successes, 1);
    }

    #[test]
    fn recovery_exhaustion_becomes_a_visible_fault() {
        let mut backend = SimulatedAudioBackend::new(vec![duplex_device("one", "Interface", None)]);
        backend.script_open(SimulatedOpenOutcome::Open {
            events: VecDeque::from([SimulatedStreamEvent::DeviceLost(BackendFailure::new(
                "lost", "gone",
            ))]),
        });
        backend.script_open(SimulatedOpenOutcome::Fail(BackendFailure::new(
            "busy",
            "attempt one",
        )));
        backend.script_open(SimulatedOpenOutcome::Fail(BackendFailure::new(
            "busy",
            "attempt two",
        )));
        let mut service = AudioDeviceService::new(backend, DeviceServiceFeatures::default());
        service.refresh_catalog().unwrap();
        let mut settings = output_settings(DeviceSelection::Default);
        settings.recovery.max_attempts = nz16(2);
        settings.recovery.retry_delay_polls = 0;
        service.open(settings).unwrap();
        service.poll().unwrap();
        assert!(matches!(
            service.poll().unwrap(),
            Some(DeviceServiceEvent::ReopenFailed { attempt: 1, .. })
        ));
        assert!(matches!(
            service.poll().unwrap(),
            Some(DeviceServiceEvent::Faulted { .. })
        ));
        let snapshot = service.snapshot();
        assert_eq!(snapshot.phase, DeviceServicePhase::Faulted);
        assert_eq!(snapshot.telemetry.reopen_attempts, 2);
        assert_eq!(snapshot.telemetry.reopen_successes, 0);
    }

    #[test]
    fn simulated_callbacks_publish_latency_xruns_and_overruns() {
        let observation = CallbackObservation {
            input: Some(DeviceStreamSpan {
                start: 10,
                frames: 64,
            }),
            output: Some(DeviceStreamSpan {
                start: 20,
                frames: 64,
            }),
            callback_duration_nanos: 2_000,
            callback_deadline_nanos: 1_000,
            input_xrun: true,
            output_xrun: false,
        };
        let mut backend = SimulatedAudioBackend::new(vec![duplex_device("one", "Interface", None)]);
        backend.script_open(SimulatedOpenOutcome::Open {
            events: VecDeque::from([SimulatedStreamEvent::Callback(observation)]),
        });
        let mut service = AudioDeviceService::new(backend, DeviceServiceFeatures::default());
        service.refresh_catalog().unwrap();
        service
            .open(output_settings(DeviceSelection::Default))
            .unwrap();
        service.telemetry().publish_latency(LatencyObservation {
            input_frames: Some(32),
            output_frames: Some(96),
            round_trip_frames: Some(128),
        });
        assert_eq!(
            service.poll().unwrap(),
            Some(DeviceServiceEvent::TelemetryAdvanced)
        );
        let telemetry = service.telemetry().snapshot();
        assert_eq!(telemetry.callbacks, 1);
        assert_eq!(telemetry.input_frames, 64);
        assert_eq!(telemetry.output_frames, 64);
        assert_eq!(telemetry.input_xruns, 1);
        assert_eq!(telemetry.callback_overruns, 1);
        assert_eq!(telemetry.peak_callback_nanos, 2_000);
        assert_eq!(telemetry.last_input_frame, Some(74));
        assert_eq!(telemetry.last_output_frame, Some(84));
        assert_eq!(telemetry.latency.round_trip_frames, Some(128));
    }

    #[test]
    fn ambiguous_name_relink_is_refused_without_guessing() {
        let mut first = duplex_device("one", "USB Audio", None);
        first.default_input = false;
        first.default_output = false;
        let mut second = duplex_device("two", "USB Audio", None);
        second.default_input = false;
        second.default_output = false;
        let backend = SimulatedAudioBackend::new(vec![first, second]);
        let mut service = AudioDeviceService::new(backend, DeviceServiceFeatures::default());
        service.refresh_catalog().unwrap();
        let preference = DeviceMatch {
            backend_name: "sim".into(),
            display_name: "USB Audio".into(),
            stable_hardware_id: None,
        };
        assert_eq!(
            service
                .open(output_settings(DeviceSelection::Relink(preference.clone())))
                .unwrap_err(),
            DeviceServiceError::AmbiguousRelinkDevice {
                preference,
                matches: 2
            }
        );
    }

    #[test]
    fn failed_refresh_invalidates_the_previous_runtime_catalog() {
        let backend =
            SimulatedAudioBackend::new(vec![duplex_device("one", "Interface", Some("serial"))]);
        let mut service = AudioDeviceService::new(backend, DeviceServiceFeatures::default());
        service.refresh_catalog().unwrap();
        let token = service.catalog()[0].token;
        service
            .backend_mut()
            .fail_discovery(Some(BackendFailure::new("enumeration", "backend offline")));
        assert!(matches!(
            service.refresh_catalog(),
            Err(DeviceServiceError::Backend(_))
        ));
        assert!(service.catalog().is_empty());
        assert_eq!(service.snapshot().catalog_generation, 2);
        assert_eq!(
            service
                .open(output_settings(DeviceSelection::Runtime {
                    token,
                    relink_fallback: None,
                }))
                .unwrap_err(),
            DeviceServiceError::Backend(BackendFailure::new("enumeration", "backend offline"))
        );
    }

    #[test]
    fn default_telemetry_does_not_invent_a_frame_zero_callback() {
        let telemetry = RealtimeTelemetry::default().snapshot();
        assert_eq!(telemetry.last_input_frame, None);
        assert_eq!(telemetry.last_output_frame, None);
        assert_eq!(telemetry.callbacks, 0);
    }

    #[test]
    fn clock_discontinuity_requires_explicit_reanchor() {
        let mut backend = SimulatedAudioBackend::new(vec![duplex_device("one", "Interface", None)]);
        backend.script_open(SimulatedOpenOutcome::Open {
            events: VecDeque::from([SimulatedStreamEvent::ClockDiscontinuity]),
        });
        let mut service = AudioDeviceService::new(backend, DeviceServiceFeatures::default());
        service.refresh_catalog().unwrap();
        service
            .open(output_settings(DeviceSelection::Default))
            .unwrap();
        service
            .anchor_clock(
                DeviceDirection::Output,
                DeviceClockAnchor {
                    device_frame: 1_000,
                    project_frame: 2_000,
                },
            )
            .unwrap();
        assert!(service.clock_map(DeviceDirection::Output).is_some());
        assert_eq!(
            service.poll().unwrap(),
            Some(DeviceServiceEvent::ClockInvalidated)
        );
        assert!(service.clock_map(DeviceDirection::Output).is_none());
    }
}
