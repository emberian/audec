//! Callback-safe bridge from immutable compiled graphs to device playback.
//!
//! Graph construction, validation, executor allocation, and swap-envelope
//! allocation happen on the control thread. The callback only consumes bounded
//! preallocated blocks, observes the existing exact-frame [`TransportSource`],
//! and exchanges raw pointers through a single-producer mailbox. Retired graph
//! executors return to the control thread before their final `Arc` can drop.
//!
//! This module does not compile editable project state, negotiate hardware,
//! resample, or introduce a second rendering truth. [`RealtimeGraphExecutor`]
//! is the same kernel used by offline graph rendering. Hardware ownership is an
//! opt-in CPAL wrapper; Rodio remains the ordinary preview fallback.

use std::array;
use std::error::Error;
use std::fmt;
use std::ptr;
use std::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering,
};
use std::sync::Arc;

use crate::audio::{
    AudioFormat, ProjectFrame, ProjectRenderer, TransportHandle, TransportMode, TransportSource,
};
use crate::compiled_audio_graph::{
    CompiledGraph, GraphExecutionError, MeterReading, MeterSnapshot, MeterTapId,
    RealtimeGraphExecutor, MAX_METER_CHANNELS,
};
use crate::device_service::{
    DeviceCallbackBuffers, DeviceCallbackContext, RealtimeDeviceProcessor,
};
use crate::render_plan::{RenderFormat, RenderSpan};

const FAILURE_NONE: u8 = 0;
const FAILURE_PROCESS: u8 = 1;
const FAILURE_SEEK: u8 = 2;

/// A control-thread graph replacement token.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GraphSwapGeneration(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphSwapError {
    FormatMismatch {
        active: RenderFormat,
        replacement: RenderFormat,
    },
    ExtentMismatch {
        active: RenderSpan,
        replacement: RenderSpan,
    },
    Prepare(GraphExecutionError),
    MailboxBusy,
}

impl fmt::Display for GraphSwapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FormatMismatch {
                active,
                replacement,
            } => write!(
                formatter,
                "graph swap changes device format from {} Hz/{} channels to {} Hz/{} channels",
                active.sample_rate, active.channels, replacement.sample_rate, replacement.channels
            ),
            Self::ExtentMismatch {
                active,
                replacement,
            } => write!(
                formatter,
                "graph swap changes transport extent from [{}..{}) to [{}..{})",
                active.start, active.end, replacement.start, replacement.end
            ),
            Self::Prepare(error) => {
                write!(formatter, "replacement graph is not executable: {error}")
            }
            Self::MailboxBusy => write!(
                formatter,
                "a graph replacement is already awaiting a callback boundary"
            ),
        }
    }
}

impl Error for GraphSwapError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphSwapOutcome {
    Applied,
    Rejected(GraphExecutionError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphSwapReceipt {
    pub generation: GraphSwapGeneration,
    pub outcome: GraphSwapOutcome,
}

struct PreparedGraph {
    executor: RealtimeGraphExecutor,
    maximum_block_frames: usize,
}

impl PreparedGraph {
    fn new(graph: Arc<CompiledGraph>) -> Result<Self, GraphExecutionError> {
        let executor = RealtimeGraphExecutor::new(graph)?;
        let maximum_block_frames = executor.contract().maximum_block_frames as usize;
        Ok(Self {
            executor,
            maximum_block_frames,
        })
    }
}

struct GraphSwapEnvelope {
    generation: GraphSwapGeneration,
    replacement: Option<PreparedGraph>,
    reclaimed: Option<PreparedGraph>,
    outcome: Option<GraphSwapOutcome>,
}

struct GraphSwapMailbox {
    incoming: AtomicPtr<GraphSwapEnvelope>,
    receipt: AtomicPtr<GraphSwapEnvelope>,
    next_generation: AtomicU64,
}

impl GraphSwapMailbox {
    fn new() -> Self {
        Self {
            incoming: AtomicPtr::new(ptr::null_mut()),
            receipt: AtomicPtr::new(ptr::null_mut()),
            next_generation: AtomicU64::new(1),
        }
    }
}

impl Drop for GraphSwapMailbox {
    fn drop(&mut self) {
        for slot in [&self.incoming, &self.receipt] {
            let raw = slot.swap(ptr::null_mut(), Ordering::AcqRel);
            if !raw.is_null() {
                // SAFETY: the final mailbox owner is gone, so neither the
                // control nor callback side can concurrently own this pointer.
                drop(unsafe { Box::from_raw(raw) });
            }
        }
    }
}

struct MeterPublication {
    revision: AtomicU64,
    present: AtomicBool,
    id: AtomicU64,
    channels: AtomicU16,
    frames_observed: AtomicU64,
    latest_peak: [AtomicU32; MAX_METER_CHANNELS],
    integrated_peak: [AtomicU32; MAX_METER_CHANNELS],
    integrated_rms: [AtomicU32; MAX_METER_CHANNELS],
}

impl MeterPublication {
    fn new() -> Self {
        Self {
            revision: AtomicU64::new(0),
            present: AtomicBool::new(false),
            id: AtomicU64::new(0),
            channels: AtomicU16::new(0),
            frames_observed: AtomicU64::new(0),
            latest_peak: array::from_fn(|_| AtomicU32::new(0)),
            integrated_peak: array::from_fn(|_| AtomicU32::new(0)),
            integrated_rms: array::from_fn(|_| AtomicU32::new(0)),
        }
    }

    fn publish(&self, reading: Option<MeterReading>) {
        self.revision.fetch_add(1, Ordering::AcqRel);
        if let Some(reading) = reading {
            self.id.store(reading.id.0, Ordering::Relaxed);
            self.channels
                .store(reading.snapshot.channels, Ordering::Relaxed);
            self.frames_observed
                .store(reading.snapshot.frames_observed, Ordering::Relaxed);
            for index in 0..MAX_METER_CHANNELS {
                self.latest_peak[index].store(
                    reading.snapshot.latest_peak[index].to_bits(),
                    Ordering::Relaxed,
                );
                self.integrated_peak[index].store(
                    reading.snapshot.integrated_peak[index].to_bits(),
                    Ordering::Relaxed,
                );
                self.integrated_rms[index].store(
                    reading.snapshot.integrated_rms[index].to_bits(),
                    Ordering::Relaxed,
                );
            }
            self.present.store(true, Ordering::Relaxed);
        } else {
            self.present.store(false, Ordering::Relaxed);
        }
        self.revision.fetch_add(1, Ordering::Release);
    }

    fn snapshot(&self) -> Option<MeterReading> {
        loop {
            let before = self.revision.load(Ordering::Acquire);
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let present = self.present.load(Ordering::Relaxed);
            let id = self.id.load(Ordering::Relaxed);
            let channels = self.channels.load(Ordering::Relaxed);
            let frames_observed = self.frames_observed.load(Ordering::Relaxed);
            let latest_peak =
                array::from_fn(|i| f32::from_bits(self.latest_peak[i].load(Ordering::Relaxed)));
            let integrated_peak =
                array::from_fn(|i| f32::from_bits(self.integrated_peak[i].load(Ordering::Relaxed)));
            let integrated_rms =
                array::from_fn(|i| f32::from_bits(self.integrated_rms[i].load(Ordering::Relaxed)));
            let after = self.revision.load(Ordering::Acquire);
            if before == after {
                return present.then_some(MeterReading {
                    id: MeterTapId(id),
                    snapshot: MeterSnapshot {
                        channels,
                        frames_observed,
                        latest_peak,
                        integrated_peak,
                        integrated_rms,
                    },
                });
            }
        }
    }
}

struct GraphRuntimeTelemetry {
    active_generation: AtomicU64,
    processed_frames: AtomicU64,
    process_failures: AtomicU64,
    seek_failures: AtomicU64,
    swaps_applied: AtomicU64,
    swaps_rejected: AtomicU64,
    last_failure: AtomicU8,
    meter: MeterPublication,
}

impl GraphRuntimeTelemetry {
    fn new() -> Self {
        Self {
            active_generation: AtomicU64::new(0),
            processed_frames: AtomicU64::new(0),
            process_failures: AtomicU64::new(0),
            seek_failures: AtomicU64::new(0),
            swaps_applied: AtomicU64::new(0),
            swaps_rejected: AtomicU64::new(0),
            last_failure: AtomicU8::new(FAILURE_NONE),
            meter: MeterPublication::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphRuntimeFailure {
    Process,
    Seek,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GraphRuntimeSnapshot {
    pub active_generation: GraphSwapGeneration,
    /// Frames computed by the graph. A bounded renderer cache can place this
    /// ahead of the audible transport cursor by at most one canonical block.
    pub processed_frames: u64,
    pub process_failures: u64,
    pub seek_failures: u64,
    pub swaps_applied: u64,
    pub swaps_rejected: u64,
    pub last_failure: Option<GraphRuntimeFailure>,
    /// First semantic meter tap, when the graph declares one.
    pub meter: Option<MeterReading>,
}

/// Cloneable control-side ownership of a running graph renderer.
#[derive(Clone)]
pub struct GraphDeviceControl {
    format: RenderFormat,
    extent: RenderSpan,
    mailbox: Arc<GraphSwapMailbox>,
    telemetry: Arc<GraphRuntimeTelemetry>,
}

impl GraphDeviceControl {
    /// Prepare an executor and publish it for the next safe render boundary.
    /// This method may allocate and must never be called from the callback.
    pub fn replace_graph(
        &self,
        graph: Arc<CompiledGraph>,
    ) -> Result<GraphSwapGeneration, GraphSwapError> {
        let replacement_format = graph.plan().format();
        let replacement_extent = graph.plan().extent();
        if replacement_format != self.format {
            return Err(GraphSwapError::FormatMismatch {
                active: self.format,
                replacement: replacement_format,
            });
        }
        if replacement_extent != self.extent {
            return Err(GraphSwapError::ExtentMismatch {
                active: self.extent,
                replacement: replacement_extent,
            });
        }
        let replacement = PreparedGraph::new(graph).map_err(GraphSwapError::Prepare)?;
        let generation =
            GraphSwapGeneration(self.mailbox.next_generation.fetch_add(1, Ordering::Relaxed));
        let envelope = Box::new(GraphSwapEnvelope {
            generation,
            replacement: Some(replacement),
            reclaimed: None,
            outcome: None,
        });
        let raw = Box::into_raw(envelope);
        if self
            .mailbox
            .incoming
            .compare_exchange(ptr::null_mut(), raw, Ordering::Release, Ordering::Relaxed)
            .is_err()
        {
            // SAFETY: publication failed, so `raw` is still solely ours.
            drop(unsafe { Box::from_raw(raw) });
            return Err(GraphSwapError::MailboxBusy);
        }
        Ok(generation)
    }

    /// Reclaim a retired executor and its graph on the control thread.
    pub fn poll_swap_receipt(&self) -> Option<GraphSwapReceipt> {
        let raw = self.mailbox.receipt.swap(ptr::null_mut(), Ordering::AcqRel);
        if raw.is_null() {
            return None;
        }
        // SAFETY: the atomic swap transferred unique ownership to this thread.
        let envelope = unsafe { Box::from_raw(raw) };
        Some(GraphSwapReceipt {
            generation: envelope.generation,
            outcome: envelope
                .outcome
                .clone()
                .expect("callback receipts always contain an outcome"),
        })
    }

    pub fn snapshot(&self) -> GraphRuntimeSnapshot {
        let last_failure = match self.telemetry.last_failure.load(Ordering::Acquire) {
            FAILURE_PROCESS => Some(GraphRuntimeFailure::Process),
            FAILURE_SEEK => Some(GraphRuntimeFailure::Seek),
            _ => None,
        };
        GraphRuntimeSnapshot {
            active_generation: GraphSwapGeneration(
                self.telemetry.active_generation.load(Ordering::Acquire),
            ),
            processed_frames: self.telemetry.processed_frames.load(Ordering::Relaxed),
            process_failures: self.telemetry.process_failures.load(Ordering::Relaxed),
            seek_failures: self.telemetry.seek_failures.load(Ordering::Relaxed),
            swaps_applied: self.telemetry.swaps_applied.load(Ordering::Relaxed),
            swaps_rejected: self.telemetry.swaps_rejected.load(Ordering::Relaxed),
            last_failure,
            meter: self.telemetry.meter.snapshot(),
        }
    }
}

/// Project renderer backed directly by the immutable native graph executor.
///
/// The prefetch block amortizes the existing sample-wise transport adapter
/// without changing transport semantics. A seek or loop wrap invalidates the
/// unread portion and reconstructs bounded graph history at the exact frame.
pub struct RealtimeGraphRenderer {
    format: AudioFormat,
    extent: RenderSpan,
    active: PreparedGraph,
    logical_position: ProjectFrame,
    cache: Box<[f32]>,
    cache_frame: usize,
    cached_frames: usize,
    mailbox: Arc<GraphSwapMailbox>,
    telemetry: Arc<GraphRuntimeTelemetry>,
    pending_receipt: Option<Box<GraphSwapEnvelope>>,
}

impl RealtimeGraphRenderer {
    pub fn new(
        graph: Arc<CompiledGraph>,
    ) -> Result<(GraphDeviceControl, Self), GraphExecutionError> {
        let render_format = graph.plan().format();
        let extent = graph.plan().extent();
        let format = AudioFormat {
            sample_rate: render_format.sample_rate,
            channels: render_format.channels,
        };
        let mut active = PreparedGraph::new(graph)?;
        active.executor.seek(extent.start)?;
        let cache_samples = active
            .maximum_block_frames
            .checked_mul(usize::from(format.channels.get()))
            .ok_or(GraphExecutionError::RenderTooLarge)?;
        let mailbox = Arc::new(GraphSwapMailbox::new());
        let telemetry = Arc::new(GraphRuntimeTelemetry::new());
        let control = GraphDeviceControl {
            format: render_format,
            extent,
            mailbox: Arc::clone(&mailbox),
            telemetry: Arc::clone(&telemetry),
        };
        Ok((
            control,
            Self {
                format,
                extent,
                active,
                logical_position: ProjectFrame(0),
                cache: vec![0.0; cache_samples].into_boxed_slice(),
                cache_frame: 0,
                cached_frames: 0,
                mailbox,
                telemetry,
                pending_receipt: None,
            },
        ))
    }

    fn invalidate_cache(&mut self) {
        self.cache_frame = 0;
        self.cached_frames = 0;
    }

    fn absolute_position(&self) -> i64 {
        self.extent
            .start
            .saturating_add(i64::try_from(self.logical_position.0).unwrap_or(i64::MAX))
    }

    fn flush_pending_receipt(&mut self) -> bool {
        let Some(envelope) = self.pending_receipt.take() else {
            return true;
        };
        let raw = Box::into_raw(envelope);
        match self.mailbox.receipt.compare_exchange(
            ptr::null_mut(),
            raw,
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) => true,
            Err(_) => {
                // SAFETY: failed publication leaves unique ownership here.
                self.pending_receipt = Some(unsafe { Box::from_raw(raw) });
                false
            }
        }
    }

    fn apply_pending_graph(&mut self) {
        if !self.flush_pending_receipt() {
            return;
        }
        let raw = self
            .mailbox
            .incoming
            .swap(ptr::null_mut(), Ordering::AcqRel);
        if raw.is_null() {
            return;
        }
        // SAFETY: the atomic swap transferred unique ownership to callback.
        let mut envelope = unsafe { Box::from_raw(raw) };
        let mut replacement = envelope
            .replacement
            .take()
            .expect("published graph envelope owns its executor");
        match replacement.executor.seek(self.absolute_position()) {
            Ok(()) => {
                envelope.reclaimed = Some(std::mem::replace(&mut self.active, replacement));
                envelope.outcome = Some(GraphSwapOutcome::Applied);
                self.telemetry
                    .active_generation
                    .store(envelope.generation.0, Ordering::Release);
                self.telemetry.swaps_applied.fetch_add(1, Ordering::Relaxed);
                self.telemetry
                    .last_failure
                    .store(FAILURE_NONE, Ordering::Release);
                self.invalidate_cache();
            }
            Err(error) => {
                envelope.reclaimed = Some(replacement);
                envelope.outcome = Some(GraphSwapOutcome::Rejected(error));
                self.telemetry
                    .swaps_rejected
                    .fetch_add(1, Ordering::Relaxed);
                self.telemetry.seek_failures.fetch_add(1, Ordering::Relaxed);
                self.telemetry
                    .last_failure
                    .store(FAILURE_SEEK, Ordering::Release);
            }
        }
        self.pending_receipt = Some(envelope);
        let _ = self.flush_pending_receipt();
    }

    fn refill(&mut self) -> usize {
        self.apply_pending_graph();
        let channels = usize::from(self.format.channels.get());
        let remaining = self.length().0.saturating_sub(self.logical_position.0) as usize;
        if remaining == 0 {
            return 0;
        }
        let capacity_frames = self.cache.len() / channels;
        let frames = remaining
            .min(capacity_frames)
            .min(self.active.maximum_block_frames);
        let samples = frames * channels;
        match self
            .active
            .executor
            .process_interleaved(&mut self.cache[..samples])
        {
            Ok(rendered) => {
                self.cache_frame = 0;
                self.cached_frames = rendered;
                self.telemetry
                    .processed_frames
                    .fetch_add(rendered as u64, Ordering::Relaxed);
                self.telemetry
                    .meter
                    .publish(self.active.executor.meter_readings().first().copied());
                rendered
            }
            Err(_) => {
                self.cache[..samples].fill(0.0);
                self.invalidate_cache();
                self.telemetry
                    .process_failures
                    .fetch_add(1, Ordering::Relaxed);
                self.telemetry
                    .last_failure
                    .store(FAILURE_PROCESS, Ordering::Release);
                0
            }
        }
    }
}

impl ProjectRenderer for RealtimeGraphRenderer {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn length(&self) -> ProjectFrame {
        ProjectFrame(self.extent.len())
    }

    fn position(&self) -> ProjectFrame {
        self.logical_position
    }

    fn seek(&mut self, frame: ProjectFrame) {
        let frame = ProjectFrame(frame.0.min(self.length().0));
        self.logical_position = frame;
        self.invalidate_cache();
        match self.active.executor.seek(self.absolute_position()) {
            Ok(()) => self
                .telemetry
                .last_failure
                .store(FAILURE_NONE, Ordering::Release),
            Err(_) => {
                self.telemetry.seek_failures.fetch_add(1, Ordering::Relaxed);
                self.telemetry
                    .last_failure
                    .store(FAILURE_SEEK, Ordering::Release);
            }
        }
    }

    fn control_boundary(&mut self, mode: TransportMode) {
        // Playing swaps wait for the prefetched block to drain. While stopped
        // or paused no samples are pending at the device, so discard prefetch
        // and apply immediately.
        if mode != TransportMode::Playing {
            self.invalidate_cache();
        }
        if self.cached_frames == self.cache_frame {
            self.apply_pending_graph();
        }
    }

    fn render_interleaved(&mut self, output: &mut [f32]) -> usize {
        let channels = usize::from(self.format.channels.get());
        if output.len() % channels != 0 {
            output.fill(0.0);
            return 0;
        }
        let requested = output.len() / channels;
        let mut written = 0;
        while written < requested {
            if self.cache_frame == self.cached_frames && self.refill() == 0 {
                break;
            }
            let available = self.cached_frames - self.cache_frame;
            let copying = available.min(requested - written);
            let source_start = self.cache_frame * channels;
            let source_end = source_start + copying * channels;
            let target_start = written * channels;
            let target_end = target_start + copying * channels;
            output[target_start..target_end].copy_from_slice(&self.cache[source_start..source_end]);
            self.cache_frame += copying;
            self.logical_position.0 += copying as u64;
            written += copying;
        }
        output[written * channels..].fill(0.0);
        written
    }
}

/// Device processor used by deterministic backends and direct hardware.
pub struct RealtimeGraphProcessor {
    source: TransportSource<RealtimeGraphRenderer>,
}

impl RealtimeGraphProcessor {
    pub fn new(
        graph: Arc<CompiledGraph>,
    ) -> Result<(TransportHandle, GraphDeviceControl, Self), GraphExecutionError> {
        let (control, renderer) = RealtimeGraphRenderer::new(graph)?;
        let (transport, source) = TransportSource::new(renderer);
        Ok((transport, control, Self { source }))
    }
}

impl RealtimeDeviceProcessor for RealtimeGraphProcessor {
    fn process(&mut self, _context: DeviceCallbackContext, mut buffers: DeviceCallbackBuffers<'_>) {
        let Some(output) = buffers.output_interleaved.as_deref_mut() else {
            return;
        };
        for sample in output {
            *sample = self.source.next().unwrap_or(0.0);
        }
    }
}

#[cfg(feature = "cpal-device")]
#[derive(Debug)]
pub enum GraphDeviceHostError {
    Graph(GraphExecutionError),
    Device(crate::cpal_device_backend::DirectCpalAudioHostError),
}

#[cfg(feature = "cpal-device")]
impl fmt::Display for GraphDeviceHostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Graph(error) => error.fmt(formatter),
            Self::Device(error) => error.fmt(formatter),
        }
    }
}

#[cfg(feature = "cpal-device")]
impl Error for GraphDeviceHostError {}

/// Opt-in hardware owner for the native graph path. Rodio `AudioHost` remains
/// untouched and available as the preview/recovery path.
#[cfg(feature = "cpal-device")]
pub struct GraphDeviceHost {
    inner: crate::cpal_device_backend::DirectCpalAudioHost,
    graph: GraphDeviceControl,
}

#[cfg(feature = "cpal-device")]
impl GraphDeviceHost {
    pub fn open(graph: Arc<CompiledGraph>) -> Result<Self, GraphDeviceHostError> {
        let (control, renderer) =
            RealtimeGraphRenderer::new(graph).map_err(GraphDeviceHostError::Graph)?;
        let inner = crate::cpal_device_backend::DirectCpalAudioHost::open_renderer(renderer)
            .map_err(GraphDeviceHostError::Device)?;
        Ok(Self {
            inner,
            graph: control,
        })
    }

    pub fn transport(&self) -> TransportHandle {
        self.inner.transport()
    }

    pub fn graph_control(&self) -> GraphDeviceControl {
        self.graph.clone()
    }

    pub fn device_host(&self) -> &crate::cpal_device_backend::DirectCpalAudioHost {
        &self.inner
    }

    pub fn device_host_mut(&mut self) -> &mut crate::cpal_device_backend::DirectCpalAudioHost {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::compiled_audio_graph::{CompiledGraphBuilder, FrozenPcmProduct};
    use crate::daw_engine::{compile_daw_engine, DawEngineConfig};
    use crate::daw_project::DawProject;
    use crate::daw_render::{RenderCancellation, RenderWindow};
    use crate::render_plan::{ExactDigest, RenderScope};

    fn graph(scale: f32) -> Arc<CompiledGraph> {
        let project = DawProject::new("graph device fixture", 48_000, 120.0).unwrap();
        let config = DawEngineConfig {
            output_channels: 2,
            block_frames: 4,
            performance_seed: 7,
            ..DawEngineConfig::default()
        };
        let schedule = compile_daw_engine(
            &project,
            &BTreeMap::new(),
            RenderWindow::new(-2, 14).unwrap(),
            &config,
            &RenderCancellation::new(),
        )
        .unwrap();
        let plan = schedule.native_render_plan().unwrap();
        let extent = plan.extent();
        let mut samples = Vec::new();
        for frame in 0..extent.len() {
            samples.push((frame + 1) as f32 * scale);
            samples.push(-((frame + 1) as f32) * scale);
        }
        let mut builder = CompiledGraphBuilder::new(plan, Arc::new(schedule)).unwrap();
        let source = builder
            .add_frozen_pcm(FrozenPcmProduct {
                scope: RenderScope::Master,
                span: extent,
                content: ExactDigest::new([(scale * 10.0) as u8; 32]),
                interleaved: samples.into(),
            })
            .unwrap();
        builder.add_meter_tap(MeterTapId(9), source).unwrap();
        builder.set_output(source).unwrap();
        Arc::new(builder.finish().unwrap())
    }

    #[test]
    fn renderer_keeps_signed_graph_origin_out_of_project_transport() {
        let (_control, mut renderer) = RealtimeGraphRenderer::new(graph(1.0)).unwrap();
        let mut first = [0.0; 6];
        assert_eq!(renderer.render_interleaved(&mut first), 3);
        assert_eq!(first, [1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);
        assert_eq!(renderer.position(), ProjectFrame(3));
        renderer.seek(ProjectFrame(7));
        let mut sought = [0.0; 4];
        assert_eq!(renderer.render_interleaved(&mut sought), 2);
        assert_eq!(sought, [8.0, -8.0, 9.0, -9.0]);
        assert_eq!(renderer.position(), ProjectFrame(9));
    }

    #[test]
    fn transport_loop_remains_exact_across_graph_prefetch_blocks() {
        let (_control, renderer) = RealtimeGraphRenderer::new(graph(1.0)).unwrap();
        let (transport, mut source) = TransportSource::new(renderer);
        transport
            .set_loop_state(
                Some(crate::audio::FrameRange::new(ProjectFrame(2), ProjectFrame(5)).unwrap()),
                true,
                Some(ProjectFrame(2)),
            )
            .unwrap();
        transport.play();
        let rendered = (0..16).map(|_| source.next().unwrap()).collect::<Vec<_>>();
        assert_eq!(
            rendered,
            vec![
                3.0, -3.0, 4.0, -4.0, 5.0, -5.0, 3.0, -3.0, 4.0, -4.0, 5.0, -5.0, 3.0, -3.0, 4.0,
                -4.0
            ]
        );
        assert_eq!(transport.snapshot().frame, ProjectFrame(4));
    }

    #[test]
    fn graph_swap_is_block_atomic_and_retirement_is_control_owned() {
        let (control, mut renderer) = RealtimeGraphRenderer::new(graph(1.0)).unwrap();
        let mut prefix = [0.0; 2];
        assert_eq!(renderer.render_interleaved(&mut prefix), 1);
        assert_eq!(prefix, [1.0, -1.0]);

        let generation = control.replace_graph(graph(10.0)).unwrap();
        // Three prefetched frames from the old four-frame block remain.
        let mut old_tail = [0.0; 6];
        assert_eq!(renderer.render_interleaved(&mut old_tail), 3);
        assert_eq!(old_tail, [2.0, -2.0, 3.0, -3.0, 4.0, -4.0]);
        let mut replacement = [0.0; 2];
        assert_eq!(renderer.render_interleaved(&mut replacement), 1);
        assert_eq!(replacement, [50.0, -50.0]);

        let receipt = control.poll_swap_receipt().unwrap();
        assert_eq!(receipt.generation, generation);
        assert_eq!(receipt.outcome, GraphSwapOutcome::Applied);
        assert_eq!(control.snapshot().swaps_applied, 1);
    }

    #[test]
    fn simulated_device_processor_observes_seek_and_publishes_meters() {
        let (transport, control, mut processor) = RealtimeGraphProcessor::new(graph(1.0)).unwrap();
        transport.seek(ProjectFrame(6));
        transport.play();
        let mut output = [0.0; 8];
        processor.process(
            DeviceCallbackContext {
                session: crate::device_service::DeviceStreamSessionId(1),
                input: None,
                output: Some(crate::device_service::DeviceStreamSpan {
                    start: 0,
                    frames: 4,
                }),
            },
            DeviceCallbackBuffers {
                input_interleaved: None,
                output_interleaved: Some(&mut output),
            },
        );
        assert_eq!(output, [7.0, -7.0, 8.0, -8.0, 9.0, -9.0, 10.0, -10.0]);
        assert_eq!(transport.snapshot().frame, ProjectFrame(10));
        let snapshot = control.snapshot();
        assert_eq!(snapshot.process_failures, 0);
        assert!(snapshot.processed_frames >= 4);
        assert_eq!(snapshot.meter.unwrap().id, MeterTapId(9));
    }
}
