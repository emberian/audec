//! Opt-in direct CPAL ownership behind [`AudioDeviceService`].
//!
//! CPAL 0.17.3 is pinned deliberately: it is the exact CPAL line already used
//! by Rodio 0.22.2. CPAL 0.18.2 is the current release as of 2026-09-01, but
//! adopting it during the transition would instantiate two native backend
//! stacks and incompatible device types. Revisit both dependencies together.
//!
//! This adapter is real output/input device I/O, but deliberately refuses a
//! duplex plan: CPAL exposes input and output as independently scheduled
//! callbacks and does not promise one shared clock. Audec's independent-clock
//! FIFO/ASRC is not landed yet. It likewise refuses project/device sample-rate
//! conversion until a preallocated converter implements this explicit seam.
//! The ordinary [`crate::audio_host::AudioHost`] remains the Rodio fallback.

use std::cell::UnsafeCell;
use std::collections::BTreeMap;
use std::fmt;
use std::num::{NonZeroU16, NonZeroU32};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Sample;

use crate::audio::{
    AudioFormat, PcmRenderer, ProjectAudio, ProjectRenderer, TransportHandle, TransportSnapshot,
    TransportSource,
};
use crate::device_service::{
    AudioDeviceBackend, AudioDeviceService, AudioDeviceStream, BackendDeviceDescriptor,
    BackendDeviceId, BackendFailure, BackendOpenPlan, BackendStreamEvent, BufferSizeRequest,
    BufferSizeSupport, CallbackObservation, DeviceCallbackBuffers, DeviceCallbackContext,
    DeviceClockAnchor, DeviceDirection, DeviceOpenSettings, DeviceSampleFormat, DeviceSelection,
    DeviceServiceError, DeviceServiceEvent, DeviceServiceFeatures, DeviceServiceSnapshot,
    DeviceStreamSessionId, DeviceStreamSpan, DuplexClockCapability, DuplexClockPolicy,
    EndpointSettings, InclusiveU32Range, RealtimeDeviceProcessor, RealtimeTelemetry,
    RecoveryPolicy, SampleRatePolicy, SupportedStreamConfig,
};

const DEFAULT_CALLBACK_FRAMES: u32 = 256;
// CPAL documents that native callbacks may differ from the requested size.
// Preallocate a bounded safety ceiling rather than resizing in the callback.
const CALLBACK_SAFETY_CEILING_FRAMES: u32 = 16_384;
const FAULT_NONE: u8 = 0;
const FAULT_DEVICE_LOST: u8 = 1;
const FAULT_INVALIDATED: u8 = 2;
const FAULT_BACKEND: u8 = 3;
const FAULT_CALLBACK_CONTRACT: u8 = 4;

struct ProcessorSlot {
    processor: UnsafeCell<Box<dyn RealtimeDeviceProcessor>>,
    callback_active: AtomicBool,
}

impl ProcessorSlot {
    fn new(processor: Box<dyn RealtimeDeviceProcessor>) -> Self {
        Self {
            processor: UnsafeCell::new(processor),
            callback_active: AtomicBool::new(false),
        }
    }

    fn process(&self, context: DeviceCallbackContext, buffers: DeviceCallbackBuffers<'_>) -> bool {
        if self
            .callback_active
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        struct ActiveGuard<'a>(&'a AtomicBool);
        impl Drop for ActiveGuard<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _guard = ActiveGuard(&self.callback_active);
        // SAFETY: `AudioDeviceService` permits one active stream, this backend
        // refuses duplex, and the atomic guard rejects accidental concurrent
        // callbacks. Stream destruction completes before a reopen reuses the
        // persistent processor slot.
        unsafe { (&mut *self.processor.get()).process(context, buffers) };
        true
    }
}

// SAFETY: mutable processor access is restricted to the single-callback guard
// above. The processor is Send, and control code never receives its pointer.
unsafe impl Send for ProcessorSlot {}
unsafe impl Sync for ProcessorSlot {}

struct StreamSignals {
    fault: AtomicU8,
}

impl StreamSignals {
    fn new() -> Self {
        Self {
            fault: AtomicU8::new(FAULT_NONE),
        }
    }

    fn publish_fault(&self, code: u8) {
        let _ = self
            .fault
            .compare_exchange(FAULT_NONE, code, Ordering::Release, Ordering::Relaxed);
    }

    fn take_fault(&self) -> u8 {
        self.fault.swap(FAULT_NONE, Ordering::AcqRel)
    }
}

/// Direct CPAL backend. One persistent processor survives stream loss/reopen;
/// the stream callback holds non-final `Arc`s and drops no project graph.
pub struct CpalDeviceBackend {
    host: cpal::Host,
    backend_name: String,
    devices: BTreeMap<BackendDeviceId, cpal::Device>,
    processor: Arc<ProcessorSlot>,
    next_session: u64,
}

impl CpalDeviceBackend {
    pub fn new(processor: Box<dyn RealtimeDeviceProcessor>) -> Self {
        let host = cpal::default_host();
        let backend_name = format!("cpal/{}", host.id());
        Self {
            host,
            backend_name,
            devices: BTreeMap::new(),
            processor: Arc::new(ProcessorSlot::new(processor)),
            next_session: 1,
        }
    }

    pub fn backend_name(&self) -> &str {
        &self.backend_name
    }

    fn next_session(&mut self) -> DeviceStreamSessionId {
        let session = DeviceStreamSessionId(self.next_session);
        self.next_session = self.next_session.wrapping_add(1).max(1);
        session
    }

    fn find_device(&self, id: &BackendDeviceId) -> Result<cpal::Device, BackendFailure> {
        self.devices.get(id).cloned().ok_or_else(|| {
            BackendFailure::new(
                "cpal-device-disappeared",
                "selected device is not in the current CPAL discovery generation",
            )
        })
    }
}

impl AudioDeviceBackend for CpalDeviceBackend {
    type Stream = CpalAudioStream;

    fn discover(&mut self) -> Result<Vec<BackendDeviceDescriptor>, BackendFailure> {
        let default_input = self
            .host
            .default_input_device()
            .and_then(|device| device.id().ok())
            .map(|id| id.to_string());
        let default_output = self
            .host
            .default_output_device()
            .and_then(|device| device.id().ok())
            .map(|id| id.to_string());
        let devices = self
            .host
            .devices()
            .map_err(|error| BackendFailure::new("cpal-discovery", error.to_string()))?;
        self.devices.clear();
        let mut descriptors = Vec::new();
        for device in devices {
            let id = device
                .id()
                .map_err(|error| BackendFailure::new("cpal-device-id", error.to_string()))?;
            let id_text = id.to_string();
            let description = device.description().map_err(|error| {
                BackendFailure::new("cpal-device-description", error.to_string())
            })?;
            let runtime_id = BackendDeviceId(id_text.clone());
            let input_configs = device
                .supported_input_configs()
                .map(|configs| configs.filter_map(map_supported_config).collect())
                .unwrap_or_default();
            let output_configs = device
                .supported_output_configs()
                .map(|configs| configs.filter_map(map_supported_config).collect())
                .unwrap_or_default();
            descriptors.push(BackendDeviceDescriptor {
                runtime_id: runtime_id.clone(),
                backend_name: self.backend_name.clone(),
                display_name: description.to_string(),
                // CPAL 0.17's DeviceId is explicitly intended to persist
                // across runs/reconnection where the host can provide it.
                stable_hardware_id: Some(id_text.clone()),
                default_input: default_input.as_deref() == Some(&id_text),
                default_output: default_output.as_deref() == Some(&id_text),
                input_configs,
                output_configs,
                // CPAL does not promise synchronized input/output callbacks.
                duplex_clock: DuplexClockCapability::UnknownOrIndependent,
            });
            self.devices.insert(runtime_id, device);
        }
        Ok(descriptors)
    }

    fn open(
        &mut self,
        plan: &BackendOpenPlan,
        telemetry: RealtimeTelemetry,
    ) -> Result<Self::Stream, BackendFailure> {
        match (&plan.public.input, &plan.public.output) {
            (Some(_), Some(_)) => {
                return Err(BackendFailure::new(
                    "cpal-duplex-asrc-pending",
                    "direct CPAL duplex requires the independent-clock FIFO/ASRC",
                ))
            }
            (None, None) => {
                return Err(BackendFailure::new(
                    "cpal-empty-plan",
                    "no input or output endpoint was negotiated",
                ))
            }
            _ => {}
        }

        let session = self.next_session();
        let signals = Arc::new(StreamSignals::new());
        let processor = Arc::clone(&self.processor);
        let stream = if let Some(endpoint) = &plan.public.output {
            if endpoint.requires_sample_rate_conversion {
                return Err(BackendFailure::new(
                    "cpal-output-resampler-pending",
                    "negotiated output needs a project-to-device converter",
                ));
            }
            let runtime_id = plan.output_runtime_id.as_ref().ok_or_else(|| {
                BackendFailure::new("cpal-output-id", "output runtime identity is absent")
            })?;
            let device = self.find_device(runtime_id)?;
            let config = cpal_stream_config(endpoint.config);
            let format = cpal_sample_format(endpoint.config.sample_format);
            let mut callback = OutputCallback::new(
                session,
                endpoint.config.channels,
                endpoint.config.sample_rate,
                endpoint.config.buffer_frames,
                endpoint.config.sample_format,
                processor,
                Arc::clone(&signals),
                telemetry.clone(),
            );
            let error_signals = Arc::clone(&signals);
            let error_telemetry = telemetry.clone();
            let stream = device
                .build_output_stream_raw(
                    &config,
                    format,
                    move |data, info| callback.process(data, info),
                    move |error| {
                        observe_cpal_error(
                            error,
                            DeviceDirection::Output,
                            &error_signals,
                            &error_telemetry,
                        )
                    },
                    None,
                )
                .map_err(|error| BackendFailure::new("cpal-build-output", error.to_string()))?;
            stream
                .play()
                .map_err(|error| BackendFailure::new("cpal-play-output", error.to_string()))?;
            stream
        } else {
            let endpoint = plan.public.input.as_ref().expect("input-only plan");
            if endpoint.requires_sample_rate_conversion {
                return Err(BackendFailure::new(
                    "cpal-input-resampler-pending",
                    "negotiated input needs a device-to-project converter",
                ));
            }
            let runtime_id = plan.input_runtime_id.as_ref().ok_or_else(|| {
                BackendFailure::new("cpal-input-id", "input runtime identity is absent")
            })?;
            let device = self.find_device(runtime_id)?;
            let config = cpal_stream_config(endpoint.config);
            let format = cpal_sample_format(endpoint.config.sample_format);
            let mut callback = InputCallback::new(
                session,
                endpoint.config.channels,
                endpoint.config.sample_rate,
                endpoint.config.buffer_frames,
                endpoint.config.sample_format,
                processor,
                Arc::clone(&signals),
                telemetry.clone(),
            );
            let error_signals = Arc::clone(&signals);
            let error_telemetry = telemetry.clone();
            let stream = device
                .build_input_stream_raw(
                    &config,
                    format,
                    move |data, info| callback.process(data, info),
                    move |error| {
                        observe_cpal_error(
                            error,
                            DeviceDirection::Input,
                            &error_signals,
                            &error_telemetry,
                        )
                    },
                    None,
                )
                .map_err(|error| BackendFailure::new("cpal-build-input", error.to_string()))?;
            stream
                .play()
                .map_err(|error| BackendFailure::new("cpal-play-input", error.to_string()))?;
            stream
        };
        Ok(CpalAudioStream {
            session,
            stream: Some(stream),
            signals,
        })
    }
}

fn map_supported_config(config: cpal::SupportedStreamConfigRange) -> Option<SupportedStreamConfig> {
    let channels = NonZeroU16::new(config.channels())?;
    let min_rate = config.min_sample_rate();
    let max_rate = config.max_sample_rate();
    let sample_rates = InclusiveU32Range::new(min_rate, max_rate).ok()?;
    let sample_format = device_sample_format(config.sample_format())?;
    let buffer_sizes = match config.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => {
            let min = NonZeroU32::new(*min)?;
            let max = NonZeroU32::new(*max)?;
            if min > max {
                return None;
            }
            BufferSizeSupport::RangeWithoutDefault { min, max }
        }
        cpal::SupportedBufferSize::Unknown => BufferSizeSupport::Unreported,
    };
    Some(SupportedStreamConfig {
        channels,
        sample_format,
        sample_rates,
        buffer_sizes,
    })
}

fn device_sample_format(format: cpal::SampleFormat) -> Option<DeviceSampleFormat> {
    match format {
        cpal::SampleFormat::F32 => Some(DeviceSampleFormat::F32),
        cpal::SampleFormat::F64 => Some(DeviceSampleFormat::F64),
        cpal::SampleFormat::I8 => Some(DeviceSampleFormat::I8),
        cpal::SampleFormat::I16 => Some(DeviceSampleFormat::I16),
        cpal::SampleFormat::I24 => Some(DeviceSampleFormat::I24),
        cpal::SampleFormat::I32 => Some(DeviceSampleFormat::I32),
        cpal::SampleFormat::I64 => Some(DeviceSampleFormat::I64),
        cpal::SampleFormat::U8 => Some(DeviceSampleFormat::U8),
        cpal::SampleFormat::U16 => Some(DeviceSampleFormat::U16),
        cpal::SampleFormat::U24 => Some(DeviceSampleFormat::U24),
        cpal::SampleFormat::U32 => Some(DeviceSampleFormat::U32),
        cpal::SampleFormat::U64 => Some(DeviceSampleFormat::U64),
        _ => None,
    }
}

fn cpal_sample_format(format: DeviceSampleFormat) -> cpal::SampleFormat {
    match format {
        DeviceSampleFormat::F32 => cpal::SampleFormat::F32,
        DeviceSampleFormat::F64 => cpal::SampleFormat::F64,
        DeviceSampleFormat::I8 => cpal::SampleFormat::I8,
        DeviceSampleFormat::I16 => cpal::SampleFormat::I16,
        DeviceSampleFormat::I24 => cpal::SampleFormat::I24,
        DeviceSampleFormat::I32 => cpal::SampleFormat::I32,
        DeviceSampleFormat::I64 => cpal::SampleFormat::I64,
        DeviceSampleFormat::U8 => cpal::SampleFormat::U8,
        DeviceSampleFormat::U16 => cpal::SampleFormat::U16,
        DeviceSampleFormat::U24 => cpal::SampleFormat::U24,
        DeviceSampleFormat::U32 => cpal::SampleFormat::U32,
        DeviceSampleFormat::U64 => cpal::SampleFormat::U64,
    }
}

fn cpal_stream_config(config: crate::device_service::NegotiatedStreamConfig) -> cpal::StreamConfig {
    cpal::StreamConfig {
        channels: config.channels.get(),
        sample_rate: config.sample_rate.get(),
        buffer_size: cpal::BufferSize::Fixed(config.buffer_frames.get()),
    }
}

fn observe_cpal_error(
    error: cpal::StreamError,
    direction: DeviceDirection,
    signals: &StreamSignals,
    telemetry: &RealtimeTelemetry,
) {
    match error {
        cpal::StreamError::BufferUnderrun => telemetry.observe_stream_xrun(direction),
        cpal::StreamError::DeviceNotAvailable => signals.publish_fault(FAULT_DEVICE_LOST),
        cpal::StreamError::StreamInvalidated => signals.publish_fault(FAULT_INVALIDATED),
        cpal::StreamError::BackendSpecific { .. } => signals.publish_fault(FAULT_BACKEND),
    }
}

pub struct CpalAudioStream {
    session: DeviceStreamSessionId,
    // Drop the native stream on the control side before the retained signals.
    stream: Option<cpal::Stream>,
    signals: Arc<StreamSignals>,
}

impl AudioDeviceStream for CpalAudioStream {
    fn session_id(&self) -> DeviceStreamSessionId {
        self.session
    }

    fn poll_event(&mut self) -> Result<Option<BackendStreamEvent>, BackendFailure> {
        let failure = match self.signals.take_fault() {
            FAULT_NONE => return Ok(None),
            FAULT_DEVICE_LOST => {
                BackendFailure::new("cpal-device-lost", "native device is unavailable")
            }
            FAULT_INVALIDATED => BackendFailure::new(
                "cpal-stream-invalidated",
                "native stream configuration must be rebuilt",
            ),
            FAULT_CALLBACK_CONTRACT => BackendFailure::new(
                "cpal-callback-contract",
                "native callback exceeded its negotiated shape",
            ),
            _ => BackendFailure::new("cpal-stream-backend", "native backend stream failure"),
        };
        Ok(Some(BackendStreamEvent::DeviceLost(failure)))
    }

    fn close(&mut self) {
        drop(self.stream.take());
    }
}

struct OutputCallback {
    session: DeviceStreamSessionId,
    channels: usize,
    sample_rate: u32,
    maximum_samples: usize,
    format: DeviceSampleFormat,
    scratch: Box<[f32]>,
    next_device_frame: u64,
    processor: Arc<ProcessorSlot>,
    signals: Arc<StreamSignals>,
    telemetry: RealtimeTelemetry,
}

impl OutputCallback {
    fn new(
        session: DeviceStreamSessionId,
        channels: NonZeroU16,
        sample_rate: NonZeroU32,
        maximum_frames: NonZeroU32,
        format: DeviceSampleFormat,
        processor: Arc<ProcessorSlot>,
        signals: Arc<StreamSignals>,
        telemetry: RealtimeTelemetry,
    ) -> Self {
        let maximum_frames = maximum_frames.get().max(CALLBACK_SAFETY_CEILING_FRAMES);
        let maximum_samples = maximum_frames as usize * channels.get() as usize;
        Self {
            session,
            channels: channels.get() as usize,
            sample_rate: sample_rate.get(),
            maximum_samples,
            format,
            scratch: vec![0.0; maximum_samples].into_boxed_slice(),
            next_device_frame: 0,
            processor,
            signals,
            telemetry,
        }
    }

    fn process(&mut self, data: &mut cpal::Data, info: &cpal::OutputCallbackInfo) {
        let started = Instant::now();
        let samples = data.len();
        if samples > self.maximum_samples || samples % self.channels != 0 {
            silence_data(data, self.format);
            self.telemetry.observe_stream_xrun(DeviceDirection::Output);
            self.signals.publish_fault(FAULT_CALLBACK_CONTRACT);
            return;
        }
        let frames = (samples / self.channels) as u32;
        let span = DeviceStreamSpan {
            start: self.next_device_frame,
            frames,
        };
        let context = DeviceCallbackContext {
            session: self.session,
            input: None,
            output: Some(span),
        };
        let processed = if self.format == DeviceSampleFormat::F32 {
            data.as_slice_mut::<f32>().is_some_and(|output| {
                self.processor.process(
                    context,
                    DeviceCallbackBuffers {
                        input_interleaved: None,
                        output_interleaved: Some(output),
                    },
                )
            })
        } else {
            let scratch = &mut self.scratch[..samples];
            scratch.fill(0.0);
            let processed = self.processor.process(
                context,
                DeviceCallbackBuffers {
                    input_interleaved: None,
                    output_interleaved: Some(scratch),
                },
            );
            if processed {
                convert_output(data, self.format, scratch)
            } else {
                false
            }
        };
        if !processed {
            silence_data(data, self.format);
            self.telemetry.observe_stream_xrun(DeviceDirection::Output);
            self.signals.publish_fault(FAULT_CALLBACK_CONTRACT);
            return;
        }
        self.next_device_frame = self.next_device_frame.saturating_add(u64::from(frames));
        let timestamp = info.timestamp();
        if let Some(delay) = timestamp.playback.duration_since(&timestamp.callback) {
            self.telemetry.publish_endpoint_latency(
                DeviceDirection::Output,
                duration_to_frames(delay, self.sample_rate),
            );
        }
        let elapsed = started.elapsed();
        self.telemetry.observe_callback(CallbackObservation {
            input: None,
            output: Some(span),
            callback_duration_nanos: duration_nanos(elapsed),
            callback_deadline_nanos: callback_deadline_nanos(frames, self.sample_rate),
            input_xrun: false,
            output_xrun: false,
        });
    }
}

struct InputCallback {
    session: DeviceStreamSessionId,
    channels: usize,
    sample_rate: u32,
    maximum_samples: usize,
    format: DeviceSampleFormat,
    scratch: Box<[f32]>,
    next_device_frame: u64,
    processor: Arc<ProcessorSlot>,
    signals: Arc<StreamSignals>,
    telemetry: RealtimeTelemetry,
}

impl InputCallback {
    fn new(
        session: DeviceStreamSessionId,
        channels: NonZeroU16,
        sample_rate: NonZeroU32,
        maximum_frames: NonZeroU32,
        format: DeviceSampleFormat,
        processor: Arc<ProcessorSlot>,
        signals: Arc<StreamSignals>,
        telemetry: RealtimeTelemetry,
    ) -> Self {
        let maximum_frames = maximum_frames.get().max(CALLBACK_SAFETY_CEILING_FRAMES);
        let maximum_samples = maximum_frames as usize * channels.get() as usize;
        Self {
            session,
            channels: channels.get() as usize,
            sample_rate: sample_rate.get(),
            maximum_samples,
            format,
            scratch: vec![0.0; maximum_samples].into_boxed_slice(),
            next_device_frame: 0,
            processor,
            signals,
            telemetry,
        }
    }

    fn process(&mut self, data: &cpal::Data, info: &cpal::InputCallbackInfo) {
        let started = Instant::now();
        let samples = data.len();
        if samples > self.maximum_samples || samples % self.channels != 0 {
            self.telemetry.observe_stream_xrun(DeviceDirection::Input);
            self.signals.publish_fault(FAULT_CALLBACK_CONTRACT);
            return;
        }
        let input = &mut self.scratch[..samples];
        if !convert_input(data, self.format, input) {
            self.telemetry.observe_stream_xrun(DeviceDirection::Input);
            self.signals.publish_fault(FAULT_CALLBACK_CONTRACT);
            return;
        }
        let frames = (samples / self.channels) as u32;
        let span = DeviceStreamSpan {
            start: self.next_device_frame,
            frames,
        };
        let processed = self.processor.process(
            DeviceCallbackContext {
                session: self.session,
                input: Some(span),
                output: None,
            },
            DeviceCallbackBuffers {
                input_interleaved: Some(input),
                output_interleaved: None,
            },
        );
        if !processed {
            self.telemetry.observe_stream_xrun(DeviceDirection::Input);
            self.signals.publish_fault(FAULT_CALLBACK_CONTRACT);
            return;
        }
        self.next_device_frame = self.next_device_frame.saturating_add(u64::from(frames));
        let timestamp = info.timestamp();
        if let Some(delay) = timestamp.callback.duration_since(&timestamp.capture) {
            self.telemetry.publish_endpoint_latency(
                DeviceDirection::Input,
                duration_to_frames(delay, self.sample_rate),
            );
        }
        self.telemetry.observe_callback(CallbackObservation {
            input: Some(span),
            output: None,
            callback_duration_nanos: duration_nanos(started.elapsed()),
            callback_deadline_nanos: callback_deadline_nanos(frames, self.sample_rate),
            input_xrun: false,
            output_xrun: false,
        });
    }
}

fn duration_to_frames(duration: Duration, sample_rate: u32) -> u32 {
    let numerator = duration.as_nanos().saturating_mul(u128::from(sample_rate));
    let frames = (numerator + 500_000_000) / 1_000_000_000;
    frames.min(u128::from(u32::MAX)) as u32
}

fn duration_nanos(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

fn callback_deadline_nanos(frames: u32, sample_rate: u32) -> u64 {
    (u128::from(frames) * 1_000_000_000 / u128::from(sample_rate)) as u64
}

fn sanitize(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

fn convert_output(data: &mut cpal::Data, format: DeviceSampleFormat, input: &[f32]) -> bool {
    macro_rules! convert {
        ($ty:ty) => {
            data.as_slice_mut::<$ty>().is_some_and(|output| {
                for (target, source) in output.iter_mut().zip(input.iter().copied()) {
                    *target = <$ty>::from_sample(sanitize(source));
                }
                true
            })
        };
    }
    match format {
        DeviceSampleFormat::F32 => convert!(f32),
        DeviceSampleFormat::F64 => convert!(f64),
        DeviceSampleFormat::I8 => convert!(i8),
        DeviceSampleFormat::I16 => convert!(i16),
        DeviceSampleFormat::I24 => convert!(cpal::I24),
        DeviceSampleFormat::I32 => convert!(i32),
        DeviceSampleFormat::I64 => convert!(i64),
        DeviceSampleFormat::U8 => convert!(u8),
        DeviceSampleFormat::U16 => convert!(u16),
        DeviceSampleFormat::U24 => convert!(cpal::U24),
        DeviceSampleFormat::U32 => convert!(u32),
        DeviceSampleFormat::U64 => convert!(u64),
    }
}

fn convert_input(data: &cpal::Data, format: DeviceSampleFormat, output: &mut [f32]) -> bool {
    macro_rules! convert {
        ($ty:ty) => {
            data.as_slice::<$ty>().is_some_and(|input| {
                for (target, source) in output.iter_mut().zip(input.iter().copied()) {
                    *target = f32::from_sample(source);
                }
                true
            })
        };
    }
    match format {
        DeviceSampleFormat::F32 => convert!(f32),
        DeviceSampleFormat::F64 => convert!(f64),
        DeviceSampleFormat::I8 => convert!(i8),
        DeviceSampleFormat::I16 => convert!(i16),
        DeviceSampleFormat::I24 => convert!(cpal::I24),
        DeviceSampleFormat::I32 => convert!(i32),
        DeviceSampleFormat::I64 => convert!(i64),
        DeviceSampleFormat::U8 => convert!(u8),
        DeviceSampleFormat::U16 => convert!(u16),
        DeviceSampleFormat::U24 => convert!(cpal::U24),
        DeviceSampleFormat::U32 => convert!(u32),
        DeviceSampleFormat::U64 => convert!(u64),
    }
}

fn silence_data(data: &mut cpal::Data, format: DeviceSampleFormat) {
    macro_rules! silence {
        ($ty:ty) => {
            if let Some(output) = data.as_slice_mut::<$ty>() {
                output.fill(<$ty as Sample>::EQUILIBRIUM);
            }
        };
    }
    match format {
        DeviceSampleFormat::F32 => silence!(f32),
        DeviceSampleFormat::F64 => silence!(f64),
        DeviceSampleFormat::I8 => silence!(i8),
        DeviceSampleFormat::I16 => silence!(i16),
        DeviceSampleFormat::I24 => silence!(cpal::I24),
        DeviceSampleFormat::I32 => silence!(i32),
        DeviceSampleFormat::I64 => silence!(i64),
        DeviceSampleFormat::U8 => silence!(u8),
        DeviceSampleFormat::U16 => silence!(u16),
        DeviceSampleFormat::U24 => silence!(cpal::U24),
        DeviceSampleFormat::U32 => silence!(u32),
        DeviceSampleFormat::U64 => silence!(u64),
    }
}

struct PreviewEnvelope {
    generation: u64,
    audio: Option<ProjectAudio>,
    source_phase: u64,
    receipt_next: usize,
}

impl PreviewEnvelope {
    fn play(generation: u64, audio: ProjectAudio) -> Self {
        Self {
            generation,
            audio: Some(audio),
            source_phase: 0,
            receipt_next: 0,
        }
    }

    fn stop(generation: u64) -> Self {
        Self {
            generation,
            audio: None,
            source_phase: 0,
            receipt_next: 0,
        }
    }
}

struct PreviewMailbox {
    incoming: AtomicPtr<PreviewEnvelope>,
    receipt: AtomicPtr<PreviewEnvelope>,
    next_generation: AtomicU64,
    active_generation: AtomicU64,
}

impl PreviewMailbox {
    fn new() -> Self {
        Self {
            incoming: AtomicPtr::new(ptr::null_mut()),
            receipt: AtomicPtr::new(ptr::null_mut()),
            next_generation: AtomicU64::new(1),
            active_generation: AtomicU64::new(0),
        }
    }
}

impl Drop for PreviewMailbox {
    fn drop(&mut self) {
        let incoming = self.incoming.swap(ptr::null_mut(), Ordering::AcqRel);
        if !incoming.is_null() {
            // SAFETY: final mailbox ownership means neither control nor
            // callback can still own the incoming pointer.
            drop(unsafe { Box::from_raw(incoming) });
        }
        drain_preview_receipts(&self.receipt);
    }
}

#[derive(Clone)]
struct DirectPreviewControl {
    mailbox: Arc<PreviewMailbox>,
}

impl DirectPreviewControl {
    fn play(&self, audio: ProjectAudio) {
        let generation = self.next_generation();
        self.mailbox
            .active_generation
            .store(generation, Ordering::Release);
        self.publish(PreviewEnvelope::play(generation, audio));
    }

    fn stop(&self) {
        let generation = self.next_generation();
        self.mailbox.active_generation.store(0, Ordering::Release);
        self.publish(PreviewEnvelope::stop(generation));
    }

    fn active(&self) -> bool {
        self.mailbox.active_generation.load(Ordering::Acquire) != 0
    }

    fn next_generation(&self) -> u64 {
        self.mailbox
            .next_generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                Some(generation.wrapping_add(1).max(1))
            })
            .expect("preview generation update cannot be refused")
    }

    fn publish(&self, envelope: PreviewEnvelope) {
        self.drain_receipt();
        let raw = Box::into_raw(Box::new(envelope));
        let superseded = self.mailbox.incoming.swap(raw, Ordering::AcqRel);
        if !superseded.is_null() {
            // SAFETY: the atomic swap retained the newest command and
            // transferred unique ownership of the superseded command here.
            drop(unsafe { Box::from_raw(superseded) });
        }
    }

    fn drain_receipt(&self) {
        drain_preview_receipts(&self.mailbox.receipt);
    }
}

fn drain_preview_receipts(receipts: &AtomicPtr<PreviewEnvelope>) {
    let mut raw = receipts.swap(ptr::null_mut(), Ordering::AcqRel);
    while !raw.is_null() {
        // SAFETY: the atomic swap transfers this entire intrusive receipt
        // stack to the control thread (or final mailbox owner).
        let mut envelope = unsafe { Box::from_raw(raw) };
        raw = envelope.receipt_next as *mut PreviewEnvelope;
        envelope.receipt_next = 0;
        drop(envelope);
    }
}

struct RealtimePreviewMixer {
    format: AudioFormat,
    mailbox: Arc<PreviewMailbox>,
    active: Option<Box<PreviewEnvelope>>,
}

impl RealtimePreviewMixer {
    fn new(format: AudioFormat) -> (DirectPreviewControl, Self) {
        let mailbox = Arc::new(PreviewMailbox::new());
        (
            DirectPreviewControl {
                mailbox: Arc::clone(&mailbox),
            },
            Self {
                format,
                mailbox,
                active: None,
            },
        )
    }

    fn return_receipt(&self, envelope: Box<PreviewEnvelope>) {
        let raw = Box::into_raw(envelope);
        let mut head = self.mailbox.receipt.load(Ordering::Acquire);
        loop {
            // SAFETY: `raw` remains uniquely callback-owned until a successful
            // publication. Failed comparisons publish neither pointer nor
            // mutation, so its intrusive next link may be retried in place.
            unsafe { (*raw).receipt_next = head as usize };
            match self.mailbox.receipt.compare_exchange_weak(
                head,
                raw,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => head = observed,
            }
        }
    }

    fn apply_command(&mut self) {
        let raw = self
            .mailbox
            .incoming
            .swap(ptr::null_mut(), Ordering::AcqRel);
        if raw.is_null() {
            return;
        }
        // SAFETY: the swap transfers unique callback ownership.
        let incoming = unsafe { Box::from_raw(raw) };
        let previous = self.active.take();
        if incoming.audio.is_some() {
            self.active = Some(incoming);
        } else {
            self.return_receipt(incoming);
        }
        if let Some(previous) = previous {
            self.return_receipt(previous);
        }
    }

    fn mix_interleaved(&mut self, output: &mut [f32]) {
        self.apply_command();
        let output_channels = usize::from(self.format.channels.get());
        if output_channels == 0 || output.len() % output_channels != 0 {
            return;
        }
        let mut completed = false;
        if let Some(active) = self.active.as_mut() {
            let audio = active.audio.as_ref().expect("active preview owns PCM");
            let source_rate = u64::from(audio.format().sample_rate.get());
            let output_rate = u64::from(self.format.sample_rate.get());
            let source_channels = usize::from(audio.format().channels.get());
            let source_frames = audio.frame_count().0;
            for target in output.chunks_exact_mut(output_channels) {
                let source_frame = active.source_phase / output_rate;
                if source_frame >= source_frames {
                    completed = true;
                    break;
                }
                let next_frame = source_frame.saturating_add(1).min(source_frames - 1);
                let fraction = (active.source_phase % output_rate) as f32 / output_rate as f32;
                for (channel, sample) in target.iter_mut().enumerate() {
                    let preview = preview_sample(
                        audio.interleaved(),
                        source_channels,
                        source_frame as usize,
                        next_frame as usize,
                        channel,
                        output_channels,
                        fraction,
                    );
                    let mixed = *sample + preview;
                    *sample = if mixed.is_finite() { mixed } else { 0.0 };
                }
                active.source_phase = active.source_phase.saturating_add(source_rate);
            }
        }
        if completed {
            if let Some(completed) = self.active.take() {
                let _ = self.mailbox.active_generation.compare_exchange(
                    completed.generation,
                    0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                self.return_receipt(completed);
            }
        }
    }
}

fn preview_sample(
    source: &[f32],
    source_channels: usize,
    frame: usize,
    next_frame: usize,
    output_channel: usize,
    output_channels: usize,
    fraction: f32,
) -> f32 {
    let read = |frame: usize, channel: usize| {
        source
            .get(
                frame
                    .saturating_mul(source_channels)
                    .saturating_add(channel),
            )
            .copied()
            .filter(|sample| sample.is_finite())
            .unwrap_or(0.0)
    };
    let channel_sample = |frame: usize| match (source_channels, output_channels) {
        (1, _) => read(frame, 0),
        (_, 1) => (read(frame, 0) + read(frame, 1)) * 0.5,
        (_, _) if output_channel < 2 => read(frame, output_channel),
        _ => 0.0,
    };
    let first = channel_sample(frame);
    let second = channel_sample(next_frame);
    first + (second - first) * fraction
}

struct TransportProcessor<R: ProjectRenderer> {
    source: TransportSource<R>,
    preview: RealtimePreviewMixer,
}

impl<R: ProjectRenderer> RealtimeDeviceProcessor for TransportProcessor<R> {
    fn process(&mut self, _context: DeviceCallbackContext, mut buffers: DeviceCallbackBuffers<'_>) {
        let Some(output) = buffers.output_interleaved.as_deref_mut() else {
            return;
        };
        for sample in output.iter_mut() {
            *sample = self.source.next().unwrap_or(0.0);
        }
        self.preview.mix_interleaved(output);
    }
}

#[derive(Debug)]
pub enum DirectCpalAudioHostError {
    Device(DeviceServiceError),
    RendererFormatMismatch {
        renderer: AudioFormat,
        settings_rate: NonZeroU32,
        settings_channels: NonZeroU16,
    },
}

impl fmt::Display for DirectCpalAudioHostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Device(error) => error.fmt(formatter),
            Self::RendererFormatMismatch {
                renderer,
                settings_rate,
                settings_channels,
            } => write!(
                formatter,
                "renderer is {} Hz/{} channels but CPAL settings request {} Hz/{} channels",
                renderer.sample_rate, renderer.channels, settings_rate, settings_channels
            ),
        }
    }
}

impl std::error::Error for DirectCpalAudioHostError {}

impl From<DeviceServiceError> for DirectCpalAudioHostError {
    fn from(error: DeviceServiceError) -> Self {
        Self::Device(error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectCpalAudioHostSnapshot {
    pub transport: TransportSnapshot,
    pub device: DeviceServiceSnapshot,
    pub preview_active: bool,
}

/// Exact-rate application output through one real CPAL stream.
///
/// Project playback and finite preview share this callback, while the project
/// renderer remains the sole timeline/transport authority. Rodio remains an
/// alternative application backend; it is never opened alongside this host.
pub struct DirectCpalAudioHost {
    transport: TransportHandle,
    preview: DirectPreviewControl,
    devices: AudioDeviceService<CpalDeviceBackend>,
}

impl DirectCpalAudioHost {
    pub fn open(project: ProjectAudio) -> Result<Self, DirectCpalAudioHostError> {
        Self::open_renderer(PcmRenderer::new(project))
    }

    pub fn open_renderer<R: ProjectRenderer>(
        renderer: R,
    ) -> Result<Self, DirectCpalAudioHostError> {
        let format = renderer.format();
        let settings = default_output_settings(format);
        Self::open_renderer_with_settings(renderer, settings)
    }

    pub fn open_renderer_with_settings<R: ProjectRenderer>(
        renderer: R,
        settings: DeviceOpenSettings,
    ) -> Result<Self, DirectCpalAudioHostError> {
        let renderer_format = renderer.format();
        let output = settings
            .output
            .as_ref()
            .ok_or(DeviceServiceError::NoEndpointsRequested)?;
        if settings.input.is_some()
            || settings.project_sample_rate != renderer_format.sample_rate
            || output.channels != renderer_format.channels
        {
            return Err(DirectCpalAudioHostError::RendererFormatMismatch {
                renderer: renderer_format,
                settings_rate: settings.project_sample_rate,
                settings_channels: output.channels,
            });
        }
        let (preview, preview_mixer) = RealtimePreviewMixer::new(renderer_format);
        let (transport, source) = TransportSource::new(renderer);
        let processor = Box::new(TransportProcessor {
            source,
            preview: preview_mixer,
        });
        let backend = CpalDeviceBackend::new(processor);
        let mut devices = AudioDeviceService::new(backend, DeviceServiceFeatures::default());
        devices.refresh_catalog()?;
        devices.open(settings)?;
        Ok(Self {
            transport,
            preview,
            devices,
        })
    }

    pub fn transport(&self) -> TransportHandle {
        self.transport.clone()
    }

    pub fn devices(&self) -> &AudioDeviceService<CpalDeviceBackend> {
        &self.devices
    }

    pub fn devices_mut(&mut self) -> &mut AudioDeviceService<CpalDeviceBackend> {
        &mut self.devices
    }

    pub fn poll_device(&mut self) -> Result<Option<DeviceServiceEvent>, DeviceServiceError> {
        self.devices.poll()
    }

    pub fn audition(&self, audio: ProjectAudio) {
        self.preview.play(audio);
    }

    pub fn stop_preview(&self) {
        self.preview.stop();
    }

    pub fn preview_active(&self) -> bool {
        self.preview.active()
    }

    pub fn anchor_output_clock(
        &mut self,
        anchor: DeviceClockAnchor,
    ) -> Result<DeviceServiceEvent, DeviceServiceError> {
        self.devices.anchor_clock(DeviceDirection::Output, anchor)
    }

    pub fn snapshot(&self) -> DirectCpalAudioHostSnapshot {
        DirectCpalAudioHostSnapshot {
            transport: self.transport.snapshot(),
            device: self.devices.snapshot(),
            preview_active: self.preview_active(),
        }
    }

    pub fn close(&mut self) {
        self.stop_preview();
        self.devices.close();
    }
}

impl Drop for DirectCpalAudioHost {
    fn drop(&mut self) {
        self.transport.stop();
        self.close();
    }
}

fn default_output_settings(format: AudioFormat) -> DeviceOpenSettings {
    DeviceOpenSettings {
        project_sample_rate: format.sample_rate,
        output: Some(EndpointSettings {
            selection: DeviceSelection::Default,
            channels: format.channels,
            latency_compensation_frames: 0,
        }),
        input: None,
        sample_rate_policy: SampleRatePolicy::RequireProjectRate,
        buffer_size: BufferSizeRequest::Prefer(
            NonZeroU32::new(DEFAULT_CALLBACK_FRAMES).expect("default buffer is positive"),
        ),
        sample_formats: vec![
            DeviceSampleFormat::F32,
            DeviceSampleFormat::I24,
            DeviceSampleFormat::I32,
            DeviceSampleFormat::I16,
            DeviceSampleFormat::F64,
        ],
        duplex_clock_policy: DuplexClockPolicy::RequireShared,
        recovery: RecoveryPolicy::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{ProjectFrame, TransportMode};

    #[test]
    fn duration_and_deadline_mapping_are_integer_and_saturating() {
        assert_eq!(duration_to_frames(Duration::from_millis(10), 48_000), 480);
        assert_eq!(callback_deadline_nanos(480, 48_000), 10_000_000);
        assert_eq!(duration_nanos(Duration::from_nanos(5)), 5);
    }

    #[test]
    fn default_settings_require_exact_renderer_format() {
        let format = AudioFormat::new(48_000, 2).unwrap();
        let settings = default_output_settings(format);
        assert_eq!(settings.project_sample_rate, format.sample_rate);
        assert_eq!(settings.output.unwrap().channels, format.channels);
        assert_eq!(
            settings.sample_rate_policy,
            SampleRatePolicy::RequireProjectRate
        );
        assert_eq!(
            settings.buffer_size,
            BufferSizeRequest::Prefer(NonZeroU32::new(256).unwrap())
        );
    }

    #[test]
    fn all_pcm_formats_map_both_directions() {
        let formats = [
            cpal::SampleFormat::F32,
            cpal::SampleFormat::F64,
            cpal::SampleFormat::I8,
            cpal::SampleFormat::I16,
            cpal::SampleFormat::I24,
            cpal::SampleFormat::I32,
            cpal::SampleFormat::I64,
            cpal::SampleFormat::U8,
            cpal::SampleFormat::U16,
            cpal::SampleFormat::U24,
            cpal::SampleFormat::U32,
            cpal::SampleFormat::U64,
        ];
        for format in formats {
            let device = device_sample_format(format).unwrap();
            assert_eq!(cpal_sample_format(device), format);
        }
    }

    #[test]
    fn processor_slot_rejects_reentrant_mutable_access() {
        struct Silence;
        impl RealtimeDeviceProcessor for Silence {
            fn process(
                &mut self,
                _context: DeviceCallbackContext,
                mut buffers: DeviceCallbackBuffers<'_>,
            ) {
                if let Some(output) = buffers.output_interleaved.as_deref_mut() {
                    output.fill(0.0);
                }
            }
        }
        let slot = ProcessorSlot::new(Box::new(Silence));
        slot.callback_active.store(true, Ordering::Relaxed);
        let mut output = [1.0; 2];
        assert!(!slot.process(
            DeviceCallbackContext {
                session: DeviceStreamSessionId(1),
                input: None,
                output: Some(DeviceStreamSpan {
                    start: 0,
                    frames: 1
                }),
            },
            DeviceCallbackBuffers {
                input_interleaved: None,
                output_interleaved: Some(&mut output),
            },
        ));
        assert_eq!(output, [1.0, 1.0]);
    }

    #[test]
    fn direct_preview_mixes_while_project_transport_is_stopped() {
        let format = AudioFormat::new(4, 2).unwrap();
        let project = ProjectAudio::from_interleaved(format, vec![0.0; 12]).unwrap();
        let renderer = PcmRenderer::new(project);
        let (preview, preview_mixer) = RealtimePreviewMixer::new(format);
        let (transport, source) = TransportSource::new(renderer);
        let mut processor = TransportProcessor {
            source,
            preview: preview_mixer,
        };
        preview.play(
            ProjectAudio::from_interleaved(AudioFormat::new(2, 1).unwrap(), vec![0.0, 1.0])
                .unwrap(),
        );

        let mut output = [0.0; 10];
        processor.process(
            DeviceCallbackContext {
                session: DeviceStreamSessionId(1),
                input: None,
                output: Some(DeviceStreamSpan {
                    start: 0,
                    frames: 5,
                }),
            },
            DeviceCallbackBuffers {
                input_interleaved: None,
                output_interleaved: Some(&mut output),
            },
        );

        assert_eq!(transport.snapshot().mode, TransportMode::Stopped);
        assert_eq!(transport.snapshot().frame, ProjectFrame(0));
        assert_eq!(output, [0.0, 0.0, 0.5, 0.5, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0]);
        assert!(!preview.active());
        preview.drain_receipt();
    }

    #[test]
    fn direct_preview_stop_and_replacement_are_latest_command_wins() {
        let format = AudioFormat::new(48_000, 2).unwrap();
        let (preview, mut mixer) = RealtimePreviewMixer::new(format);
        let mono = |sample| {
            ProjectAudio::from_interleaved(AudioFormat::new(48_000, 1).unwrap(), vec![sample; 8])
                .unwrap()
        };

        preview.play(mono(0.25));
        preview.play(mono(0.75));
        let mut output = [0.0; 2];
        mixer.mix_interleaved(&mut output);
        assert_eq!(output, [0.75, 0.75]);

        preview.stop();
        output.fill(0.0);
        mixer.mix_interleaved(&mut output);
        assert_eq!(output, [0.0, 0.0]);
        assert!(!preview.active());
        preview.drain_receipt();
    }
}
