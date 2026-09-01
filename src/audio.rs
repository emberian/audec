//! Sample-domain project transport and the realtime playback adapter.
//!
//! The transport deliberately measures time in project audio frames. Seconds
//! are only a presentation/input format; using them as the transport clock
//! makes exact seeks and end-exclusive loops needlessly ambiguous.

use std::fmt;
use std::num::{NonZeroU16, NonZeroU32};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rodio::{ChannelCount, SampleRate, Source};

static NEXT_TRANSPORT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// Process-local identity of one renderer/transport pair. It prevents a
/// controller handoff from accepting commands against the retired host.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TransportSessionId(u64);

/// A zero-based frame in the project's native sample rate.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct ProjectFrame(pub u64);

/// An end-exclusive range of project frames: `[start, end)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameRange {
    pub start: ProjectFrame,
    pub end: ProjectFrame,
}

impl FrameRange {
    pub fn new(start: ProjectFrame, end: ProjectFrame) -> Result<Self, AudioError> {
        if start >= end {
            return Err(AudioError::EmptyRange { start, end });
        }
        Ok(Self { start, end })
    }

    pub fn len(self) -> u64 {
        self.end.0 - self.start.0
    }

    pub fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

/// Channel and frame-rate metadata for interleaved PCM.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioFormat {
    pub sample_rate: NonZeroU32,
    pub channels: NonZeroU16,
}

impl AudioFormat {
    pub fn new(sample_rate: u32, channels: u16) -> Result<Self, AudioError> {
        let sample_rate = NonZeroU32::new(sample_rate).ok_or(AudioError::ZeroSampleRate)?;
        let channels = NonZeroU16::new(channels).ok_or(AudioError::ZeroChannels)?;
        Ok(Self {
            sample_rate,
            channels,
        })
    }

    /// Convert seconds to the nearest native project frame.
    pub fn frame_at_seconds(self, seconds: f64) -> Result<ProjectFrame, AudioError> {
        if !seconds.is_finite() {
            return Err(AudioError::NonFiniteSeconds(seconds));
        }
        if seconds <= 0.0 {
            return Ok(ProjectFrame(0));
        }
        let frames = seconds * f64::from(self.sample_rate.get());
        Ok(ProjectFrame(if frames >= u64::MAX as f64 {
            u64::MAX
        } else {
            frames.round() as u64
        }))
    }

    pub fn seconds_at_frame(self, frame: ProjectFrame) -> f64 {
        frame.0 as f64 / f64::from(self.sample_rate.get())
    }
}

/// Immutable, shared, interleaved project audio.
#[derive(Clone, Debug)]
pub struct ProjectAudio {
    format: AudioFormat,
    samples: Arc<[f32]>,
    frame_count: ProjectFrame,
}

impl ProjectAudio {
    pub fn new(format: AudioFormat, samples: Arc<[f32]>) -> Result<Self, AudioError> {
        let channels = usize::from(format.channels.get());
        if samples.len() % channels != 0 {
            return Err(AudioError::PartialFrame {
                samples: samples.len(),
                channels,
            });
        }
        let frames = samples.len() / channels;
        let frame_count =
            ProjectFrame(u64::try_from(frames).map_err(|_| AudioError::AudioTooLong { frames })?);
        Ok(Self {
            format,
            samples,
            frame_count,
        })
    }

    pub fn from_interleaved(format: AudioFormat, samples: Vec<f32>) -> Result<Self, AudioError> {
        Self::new(format, samples.into())
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }

    pub fn frame_count(&self) -> ProjectFrame {
        self.frame_count
    }

    pub fn interleaved(&self) -> &[f32] {
        &self.samples
    }

    pub fn shared_interleaved(&self) -> Arc<[f32]> {
        Arc::clone(&self.samples)
    }

    pub fn frame_at_seconds_clamped(&self, seconds: f64) -> Result<ProjectFrame, AudioError> {
        Ok(ProjectFrame(
            self.format
                .frame_at_seconds(seconds)?
                .0
                .min(self.frame_count.0),
        ))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AudioError {
    ZeroSampleRate,
    ZeroChannels,
    NonFiniteSeconds(f64),
    EmptyRange {
        start: ProjectFrame,
        end: ProjectFrame,
    },
    LoopOutOfBounds {
        range: FrameRange,
        length: ProjectFrame,
    },
    PartialFrame {
        samples: usize,
        channels: usize,
    },
    AudioTooLong {
        frames: usize,
    },
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSampleRate => write!(f, "sample rate must not be zero"),
            Self::ZeroChannels => write!(f, "channel count must not be zero"),
            Self::NonFiniteSeconds(seconds) => {
                write!(f, "transport time must be finite, got {seconds}")
            }
            Self::EmptyRange { start, end } => write!(
                f,
                "frame range must be non-empty, got {}..{}",
                start.0, end.0
            ),
            Self::LoopOutOfBounds { range, length } => write!(
                f,
                "loop {}..{} exceeds project length {}",
                range.start.0, range.end.0, length.0
            ),
            Self::PartialFrame { samples, channels } => write!(
                f,
                "{samples} interleaved samples do not contain complete {channels}-channel frames"
            ),
            Self::AudioTooLong { frames } => {
                write!(
                    f,
                    "audio contains too many frames for the transport: {frames}"
                )
            }
        }
    }
}

impl std::error::Error for AudioError {}

/// The renderer boundary shared by realtime playback and future offline export.
///
/// `render_interleaved` must write complete frames beginning at `position()`.
/// It returns the number of frames written and advances its position by that
/// amount. Realtime implementations must not allocate or block here.
pub trait ProjectRenderer: Send + 'static {
    fn format(&self) -> AudioFormat;
    fn length(&self) -> ProjectFrame;
    fn position(&self) -> ProjectFrame;
    fn seek(&mut self, frame: ProjectFrame);
    /// Observe a transport control boundary even when no project PCM will be
    /// requested. Persistent renderers use this to consume lock-free cohort
    /// and audition mailboxes while paused/stopped; simple PCM renderers need
    /// no special work.
    fn control_boundary(&mut self, _mode: TransportMode) {}
    fn render_interleaved(&mut self, output: &mut [f32]) -> usize;
}

/// The first project renderer: a cursor over immutable in-memory PCM.
#[derive(Clone, Debug)]
pub struct PcmRenderer {
    audio: ProjectAudio,
    position: ProjectFrame,
}

impl PcmRenderer {
    pub fn new(audio: ProjectAudio) -> Self {
        Self {
            audio,
            position: ProjectFrame(0),
        }
    }
}

impl ProjectRenderer for PcmRenderer {
    fn format(&self) -> AudioFormat {
        self.audio.format
    }

    fn length(&self) -> ProjectFrame {
        self.audio.frame_count
    }

    fn position(&self) -> ProjectFrame {
        self.position
    }

    fn seek(&mut self, frame: ProjectFrame) {
        self.position = ProjectFrame(frame.0.min(self.audio.frame_count.0));
    }

    fn render_interleaved(&mut self, output: &mut [f32]) -> usize {
        let channels = usize::from(self.audio.format.channels.get());
        let requested_frames = output.len() / channels;
        let available_frames = self.audio.frame_count.0.saturating_sub(self.position.0);
        let rendered_frames = requested_frames.min(available_frames as usize);
        let source_start = self.position.0 as usize * channels;
        let sample_count = rendered_frames * channels;
        output[..sample_count]
            .copy_from_slice(&self.audio.samples[source_start..source_start + sample_count]);
        output[sample_count..].fill(0.0);
        self.position.0 += rendered_frames as u64;
        rendered_frames
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TransportMode {
    Stopped = 0,
    Paused = 1,
    Playing = 2,
    Ended = 3,
}

impl TransportMode {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Stopped,
            1 => Self::Paused,
            2 => Self::Playing,
            3 => Self::Ended,
            _ => Self::Stopped,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransportSnapshot {
    pub mode: TransportMode,
    /// The next project frame to be rendered.
    pub frame: ProjectFrame,
    pub loop_region: Option<FrameRange>,
    pub loop_enabled: bool,
    /// Changes whenever the audio thread publishes a new position or mode.
    pub revision: u64,
}

struct TransportShared {
    identity: TransportSessionId,
    // Serializes control writers only. The audio thread never touches it.
    writer: Mutex<()>,
    // An even/odd seqlock around the following desired-control atomics.
    control_revision: AtomicU64,
    desired_mode: AtomicU8,
    desired_frame: AtomicU64,
    seek_generation: AtomicU64,
    applied_seek_generation: AtomicU64,
    loop_present: AtomicBool,
    loop_enabled: AtomicBool,
    loop_start: AtomicU64,
    loop_end: AtomicU64,
    // A second seqlock protects the audio-thread publication.
    publish_revision: AtomicU64,
    published_mode: AtomicU8,
    published_frame: AtomicU64,
}

impl TransportShared {
    fn new() -> Self {
        Self {
            identity: TransportSessionId(NEXT_TRANSPORT_SESSION_ID.fetch_add(1, Ordering::Relaxed)),
            writer: Mutex::new(()),
            control_revision: AtomicU64::new(0),
            desired_mode: AtomicU8::new(TransportMode::Stopped as u8),
            desired_frame: AtomicU64::new(0),
            seek_generation: AtomicU64::new(0),
            applied_seek_generation: AtomicU64::new(0),
            loop_present: AtomicBool::new(false),
            loop_enabled: AtomicBool::new(false),
            loop_start: AtomicU64::new(0),
            loop_end: AtomicU64::new(0),
            publish_revision: AtomicU64::new(0),
            published_mode: AtomicU8::new(TransportMode::Stopped as u8),
            published_frame: AtomicU64::new(0),
        }
    }

    fn write_control(&self, update: impl FnOnce(&Self)) {
        let _writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.control_revision.fetch_add(1, Ordering::AcqRel);
        update(self);
        self.control_revision.fetch_add(1, Ordering::Release);
    }

    /// One non-spinning read for the realtime thread. A concurrent write is
    /// simply observed on the next audio frame.
    fn try_read_control(&self) -> Option<ControlSnapshot> {
        let before = self.control_revision.load(Ordering::Acquire);
        if before & 1 != 0 {
            return None;
        }
        let snapshot = ControlSnapshot {
            revision: before,
            mode: TransportMode::from_u8(self.desired_mode.load(Ordering::Relaxed)),
            desired_frame: ProjectFrame(self.desired_frame.load(Ordering::Relaxed)),
            seek_generation: self.seek_generation.load(Ordering::Relaxed),
            loop_region: if self.loop_present.load(Ordering::Relaxed) {
                Some(FrameRange {
                    start: ProjectFrame(self.loop_start.load(Ordering::Relaxed)),
                    end: ProjectFrame(self.loop_end.load(Ordering::Relaxed)),
                })
            } else {
                None
            },
            loop_enabled: self.loop_enabled.load(Ordering::Relaxed),
        };
        let after = self.control_revision.load(Ordering::Acquire);
        (before == after).then_some(snapshot)
    }

    fn publish(&self, frame: ProjectFrame, mode: TransportMode) {
        self.publish_revision.fetch_add(1, Ordering::Relaxed);
        self.published_frame.store(frame.0, Ordering::Relaxed);
        self.published_mode.store(mode as u8, Ordering::Relaxed);
        self.publish_revision.fetch_add(1, Ordering::Release);
    }

    fn read_publication(&self) -> (ProjectFrame, TransportMode, u64) {
        loop {
            let before = self.publish_revision.load(Ordering::Acquire);
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let frame = ProjectFrame(self.published_frame.load(Ordering::Relaxed));
            let mode = TransportMode::from_u8(self.published_mode.load(Ordering::Relaxed));
            let after = self.publish_revision.load(Ordering::Acquire);
            if before == after {
                return (frame, mode, after);
            }
            std::hint::spin_loop();
        }
    }
}

#[derive(Clone, Copy)]
struct ControlSnapshot {
    revision: u64,
    mode: TransportMode,
    desired_frame: ProjectFrame,
    seek_generation: u64,
    loop_region: Option<FrameRange>,
    loop_enabled: bool,
}

/// Cloneable, thread-safe control and observation handle for one project.
#[derive(Clone)]
pub struct TransportHandle {
    shared: Arc<TransportShared>,
    format: AudioFormat,
    length: ProjectFrame,
}

impl TransportHandle {
    fn new(shared: Arc<TransportShared>, format: AudioFormat, length: ProjectFrame) -> Self {
        Self {
            shared,
            format,
            length,
        }
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }

    pub fn length(&self) -> ProjectFrame {
        self.length
    }

    pub fn session_id(&self) -> TransportSessionId {
        self.shared.identity
    }

    pub fn play(&self) {
        self.shared.write_control(|shared| {
            let (published_frame, published_mode, _) = shared.read_publication();
            // An explicit seek can be followed immediately by play, before the
            // realtime thread has had a frame boundary at which to publish it.
            // Do not mistake that still-visible EOF snapshot for a request to
            // restart from zero.
            let pending_seek = shared.seek_generation.load(Ordering::Relaxed)
                != shared.applied_seek_generation.load(Ordering::Acquire);
            if !pending_seek
                && (published_mode == TransportMode::Ended || published_frame >= self.length)
            {
                shared.desired_frame.store(0, Ordering::Relaxed);
                shared.seek_generation.fetch_add(1, Ordering::Relaxed);
            }
            shared
                .desired_mode
                .store(TransportMode::Playing as u8, Ordering::Relaxed);
        });
    }

    pub fn pause(&self) {
        self.shared.write_control(|shared| {
            // Consult requested state rather than the audio publication so a
            // rapid play/pause pair remains last-writer-wins even if the audio
            // callback has not run between the two commands.
            let mode = if TransportMode::from_u8(shared.desired_mode.load(Ordering::Relaxed))
                == TransportMode::Stopped
            {
                TransportMode::Stopped
            } else {
                TransportMode::Paused
            };
            shared.desired_mode.store(mode as u8, Ordering::Relaxed);
        });
    }

    pub fn toggle(&self) {
        self.shared.write_control(|shared| {
            let (published_frame, published_mode, _) = shared.read_publication();
            let pending_seek = shared.seek_generation.load(Ordering::Relaxed)
                != shared.applied_seek_generation.load(Ordering::Acquire);
            let desired_mode = TransportMode::from_u8(shared.desired_mode.load(Ordering::Relaxed));
            let effectively_playing = desired_mode == TransportMode::Playing
                && (published_mode != TransportMode::Ended || pending_seek);
            if effectively_playing {
                shared
                    .desired_mode
                    .store(TransportMode::Paused as u8, Ordering::Relaxed);
            } else {
                if !pending_seek
                    && (published_mode == TransportMode::Ended || published_frame >= self.length)
                {
                    shared.desired_frame.store(0, Ordering::Relaxed);
                    shared.seek_generation.fetch_add(1, Ordering::Relaxed);
                }
                shared
                    .desired_mode
                    .store(TransportMode::Playing as u8, Ordering::Relaxed);
            }
        });
    }

    pub fn stop(&self) {
        self.shared.write_control(|shared| {
            shared
                .desired_mode
                .store(TransportMode::Stopped as u8, Ordering::Relaxed);
            shared.desired_frame.store(0, Ordering::Relaxed);
            shared.seek_generation.fetch_add(1, Ordering::Relaxed);
        });
    }

    pub fn seek(&self, frame: ProjectFrame) {
        let frame = ProjectFrame(frame.0.min(self.length.0));
        self.shared.write_control(|shared| {
            // Keep an already-requested play alive even when this seek lands
            // before the audio thread has published the Playing state.
            let (_, published_mode, _) = shared.read_publication();
            let pending_seek = shared.seek_generation.load(Ordering::Relaxed)
                != shared.applied_seek_generation.load(Ordering::Acquire);
            let desired_mode = TransportMode::from_u8(shared.desired_mode.load(Ordering::Relaxed));
            let mode = match desired_mode {
                TransportMode::Playing
                    if published_mode != TransportMode::Ended || pending_seek =>
                {
                    TransportMode::Playing
                }
                TransportMode::Stopped if frame.0 == 0 => TransportMode::Stopped,
                _ => TransportMode::Paused,
            };
            shared.desired_mode.store(mode as u8, Ordering::Relaxed);
            shared.desired_frame.store(frame.0, Ordering::Relaxed);
            shared.seek_generation.fetch_add(1, Ordering::Relaxed);
        });
    }

    pub fn seek_seconds(&self, seconds: f64) -> Result<(), AudioError> {
        let frame = self.format.frame_at_seconds(seconds)?;
        self.seek(ProjectFrame(frame.0.min(self.length.0)));
        Ok(())
    }

    /// Install and enable a loop, or clear and disable it.
    ///
    /// Editing loop bounds is not a locate command: it preserves the requested
    /// mode and frame even when that frame lies beyond the new loop end. A
    /// later explicit Play from a paused out-of-range frame enters at the loop
    /// start; already-running playback continues from its current position.
    pub fn set_loop_region(&self, range: Option<FrameRange>) -> Result<(), AudioError> {
        self.set_loop_state(range, range.is_some(), None)
    }

    /// Atomically replace loop bounds/enabled state and optionally locate.
    ///
    /// This is the backend transaction used when a drag authors a new loop or
    /// a click locates outside an older loop. The realtime thread can observe
    /// either the complete old control tuple or the complete new tuple; it can
    /// never combine a stale loop start with the new seek.
    pub fn set_loop_state(
        &self,
        range: Option<FrameRange>,
        enabled: bool,
        locate: Option<ProjectFrame>,
    ) -> Result<(), AudioError> {
        if let Some(range) = range {
            if range.is_empty() {
                return Err(AudioError::EmptyRange {
                    start: range.start,
                    end: range.end,
                });
            }
            if range.end > self.length {
                return Err(AudioError::LoopOutOfBounds {
                    range,
                    length: self.length,
                });
            }
        }
        let locate = locate.map(|frame| ProjectFrame(frame.0.min(self.length.0)));
        self.shared.write_control(|shared| {
            let (_, published_mode, _) = shared.read_publication();
            let pending_seek = shared.seek_generation.load(Ordering::Relaxed)
                != shared.applied_seek_generation.load(Ordering::Acquire);
            let desired_mode = TransportMode::from_u8(shared.desired_mode.load(Ordering::Relaxed));
            let requested_mode = if locate.is_some() {
                match desired_mode {
                    TransportMode::Playing
                        if published_mode != TransportMode::Ended || pending_seek =>
                    {
                        TransportMode::Playing
                    }
                    TransportMode::Stopped if locate == Some(ProjectFrame(0)) => {
                        TransportMode::Stopped
                    }
                    _ => TransportMode::Paused,
                }
            } else if published_mode == TransportMode::Ended && !pending_seek {
                TransportMode::Paused
            } else {
                normalized_control_mode(desired_mode)
            };
            if let Some(range) = range {
                shared.loop_start.store(range.start.0, Ordering::Relaxed);
                shared.loop_end.store(range.end.0, Ordering::Relaxed);
                shared.loop_present.store(true, Ordering::Relaxed);
            } else {
                shared.loop_present.store(false, Ordering::Relaxed);
            }
            shared
                .loop_enabled
                .store(enabled && range.is_some(), Ordering::Relaxed);
            if let Some(frame) = locate {
                shared.desired_frame.store(frame.0, Ordering::Relaxed);
                shared.seek_generation.fetch_add(1, Ordering::Relaxed);
            }
            shared
                .desired_mode
                .store(requested_mode as u8, Ordering::Relaxed);
        });
        Ok(())
    }

    pub fn set_loop_enabled(&self, enabled: bool) {
        self.shared.write_control(|shared| {
            let (_, published_mode, _) = shared.read_publication();
            let pending_seek = shared.seek_generation.load(Ordering::Relaxed)
                != shared.applied_seek_generation.load(Ordering::Acquire);
            let desired_mode = TransportMode::from_u8(shared.desired_mode.load(Ordering::Relaxed));
            let requested_mode = if published_mode == TransportMode::Ended && !pending_seek {
                TransportMode::Paused
            } else {
                normalized_control_mode(desired_mode)
            };
            let present = shared.loop_present.load(Ordering::Relaxed);
            shared
                .loop_enabled
                .store(enabled && present, Ordering::Relaxed);
            shared
                .desired_mode
                .store(requested_mode as u8, Ordering::Relaxed);
        });
    }

    pub fn snapshot(&self) -> TransportSnapshot {
        loop {
            let Some(before) = self.shared.try_read_control() else {
                std::hint::spin_loop();
                continue;
            };
            let (frame, mode, publish_revision) = self.shared.read_publication();
            let Some(after) = self.shared.try_read_control() else {
                std::hint::spin_loop();
                continue;
            };
            if before.revision == after.revision {
                return TransportSnapshot {
                    mode,
                    frame,
                    loop_region: after.loop_region,
                    loop_enabled: after.loop_enabled,
                    // Both seqlocks advance monotonically by two. Their sum is
                    // a cheap combined observation revision that also changes
                    // for loop-only edits.
                    revision: publish_revision.wrapping_add(after.revision),
                };
            }
            std::hint::spin_loop();
        }
    }
}

fn normalized_control_mode(mode: TransportMode) -> TransportMode {
    match mode {
        TransportMode::Ended => TransportMode::Paused,
        other => other,
    }
}

/// Infinite Rodio source which renders project audio while retaining exact
/// project-frame transport semantics.
///
/// The source owns one preallocated frame. `next` performs no allocation,
/// locking, logging, or blocking. Control writes are observed at frame
/// boundaries; a command arriving between stereo channels therefore cannot
/// scramble channel order.
pub struct TransportSource<R: ProjectRenderer> {
    renderer: R,
    shared: Arc<TransportShared>,
    format: AudioFormat,
    length: ProjectFrame,
    frame_samples: Box<[f32]>,
    channel_cursor: usize,
    cursor: ProjectFrame,
    rendered_frame: bool,
    mode: TransportMode,
    loop_region: Option<FrameRange>,
    loop_enabled: bool,
    /// True while the current cursor is approaching the active loop end.
    /// Explicit seeks and loop edits beyond that end remain exact locations
    /// instead of being rewritten to the loop start.
    loop_armed: bool,
    seen_control_revision: u64,
    seen_seek_generation: u64,
}

impl<R: ProjectRenderer> TransportSource<R> {
    pub fn new(mut renderer: R) -> (TransportHandle, Self) {
        let format = renderer.format();
        let length = renderer.length();
        renderer.seek(ProjectFrame(0));
        let shared = Arc::new(TransportShared::new());
        let handle = TransportHandle::new(Arc::clone(&shared), format, length);
        let source = Self {
            renderer,
            shared,
            format,
            length,
            frame_samples: vec![0.0; usize::from(format.channels.get())].into_boxed_slice(),
            channel_cursor: 0,
            cursor: ProjectFrame(0),
            rendered_frame: false,
            mode: TransportMode::Stopped,
            loop_region: None,
            loop_enabled: false,
            loop_armed: false,
            seen_control_revision: u64::MAX,
            seen_seek_generation: 0,
        };
        (handle, source)
    }

    fn apply_control(&mut self) {
        let Some(control) = self.shared.try_read_control() else {
            return;
        };
        if control.revision == self.seen_control_revision {
            return;
        }
        self.seen_control_revision = control.revision;
        let previous_loop_region = self.loop_region;
        let previous_loop_enabled = self.loop_enabled;
        self.mode = control.mode;
        self.loop_region = control.loop_region;
        self.loop_enabled = control.loop_enabled && control.loop_region.is_some();
        let seek_changed = control.seek_generation != self.seen_seek_generation;
        if seek_changed {
            self.seen_seek_generation = control.seek_generation;
            self.cursor = ProjectFrame(control.desired_frame.0.min(self.length.0));
            self.renderer.seek(self.cursor);
            self.shared
                .applied_seek_generation
                .store(control.seek_generation, Ordering::Release);
        }
        let loop_changed =
            previous_loop_region != self.loop_region || previous_loop_enabled != self.loop_enabled;
        if !self.loop_enabled {
            self.loop_armed = false;
        } else if seek_changed || loop_changed {
            self.loop_armed = self.loop_region.is_some_and(|range| {
                self.cursor < range.end || self.mode != TransportMode::Playing
            });
        }
        if self.mode == TransportMode::Playing && self.cursor >= self.length {
            self.mode = TransportMode::Ended;
        }
        self.shared.publish(self.cursor, self.mode);
    }

    fn prepare_frame(&mut self) {
        self.apply_control();
        self.renderer.control_boundary(self.mode);
        self.rendered_frame = false;
        self.frame_samples.fill(0.0);
        if self.mode != TransportMode::Playing {
            return;
        }

        if self.loop_enabled && self.loop_armed {
            if let Some(range) = self.loop_region {
                if self.cursor >= range.end {
                    self.cursor = range.start;
                    self.renderer.seek(self.cursor);
                    self.shared.publish(self.cursor, self.mode);
                }
            }
        }
        if self.cursor >= self.length {
            self.mode = TransportMode::Ended;
            self.shared.publish(self.cursor, self.mode);
            return;
        }

        let rendered = self.renderer.render_interleaved(&mut self.frame_samples);
        if rendered == 1 {
            self.rendered_frame = true;
        } else {
            self.frame_samples.fill(0.0);
            self.mode = TransportMode::Ended;
            self.cursor = self.length;
            self.shared.publish(self.cursor, self.mode);
        }
    }

    fn finish_frame(&mut self) {
        if !self.rendered_frame {
            return;
        }
        self.cursor.0 += 1;
        if self.loop_enabled && self.loop_armed {
            if let Some(range) = self.loop_region {
                if self.cursor >= range.end {
                    self.cursor = range.start;
                    self.renderer.seek(self.cursor);
                }
            }
        }
        if self.cursor >= self.length {
            self.cursor = self.length;
            self.mode = TransportMode::Ended;
        }
        self.shared.publish(self.cursor, self.mode);
    }
}

impl<R: ProjectRenderer> Iterator for TransportSource<R> {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.channel_cursor == 0 {
            self.prepare_frame();
        }
        let sample = self.frame_samples[self.channel_cursor];
        self.channel_cursor += 1;
        if self.channel_cursor == self.frame_samples.len() {
            self.channel_cursor = 0;
            self.finish_frame();
        }
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (usize::MAX, None)
    }
}

impl<R: ProjectRenderer> Source for TransportSource<R> {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        self.format.channels
    }

    fn sample_rate(&self) -> SampleRate {
        self.format.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        // The adapter intentionally remains alive while paused and after EOF.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    struct TrackingAllocator;

    thread_local! {
        static TRACK_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
        static ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
    }

    unsafe impl GlobalAlloc for TrackingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if TRACK_ALLOCATIONS.try_with(Cell::get).unwrap_or(false) {
                let _ = ALLOCATION_COUNT.try_with(|count| count.set(count.get() + 1));
            }
            // SAFETY: this allocator delegates the request unchanged to the
            // process system allocator.
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: `ptr` and `layout` came from the delegated allocation.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            if TRACK_ALLOCATIONS.try_with(Cell::get).unwrap_or(false) {
                let _ = ALLOCATION_COUNT.try_with(|count| count.set(count.get() + 1));
            }
            // SAFETY: this allocator delegates the request unchanged to the
            // process system allocator.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            if TRACK_ALLOCATIONS.try_with(Cell::get).unwrap_or(false) {
                let _ = ALLOCATION_COUNT.try_with(|count| count.set(count.get() + 1));
            }
            // SAFETY: `ptr` and `layout` came from the delegated allocation;
            // `new_size` is forwarded unchanged.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static TEST_ALLOCATOR: TrackingAllocator = TrackingAllocator;

    fn mono(values: &[f32]) -> (TransportHandle, TransportSource<PcmRenderer>) {
        let format = AudioFormat::new(48_000, 1).unwrap();
        let audio = ProjectAudio::from_interleaved(format, values.to_vec()).unwrap();
        TransportSource::new(PcmRenderer::new(audio))
    }

    fn stereo_frames(frames: &[[f32; 2]]) -> (TransportHandle, TransportSource<PcmRenderer>) {
        let format = AudioFormat::new(48_000, 2).unwrap();
        let samples = frames.iter().flatten().copied().collect();
        let audio = ProjectAudio::from_interleaved(format, samples).unwrap();
        TransportSource::new(PcmRenderer::new(audio))
    }

    fn pull_frame(source: &mut TransportSource<PcmRenderer>) -> Vec<f32> {
        (0..usize::from(source.channels().get()))
            .map(|_| source.next().unwrap())
            .collect()
    }

    #[test]
    fn format_and_pcm_validate_their_invariants() {
        assert_eq!(AudioFormat::new(0, 2), Err(AudioError::ZeroSampleRate));
        assert_eq!(AudioFormat::new(48_000, 0), Err(AudioError::ZeroChannels));
        let format = AudioFormat::new(48_000, 2).unwrap();
        assert!(matches!(
            ProjectAudio::from_interleaved(format, vec![0.0; 3]),
            Err(AudioError::PartialFrame { .. })
        ));
        assert!(FrameRange::new(ProjectFrame(4), ProjectFrame(4)).is_err());
    }

    #[test]
    fn time_conversion_rounds_to_the_nearest_frame() {
        let format = AudioFormat::new(44_100, 2).unwrap();
        assert_eq!(format.frame_at_seconds(0.5).unwrap(), ProjectFrame(22_050));
        assert_eq!(format.seconds_at_frame(ProjectFrame(22_050)), 0.5);
        assert_eq!(format.frame_at_seconds(-1.0).unwrap(), ProjectFrame(0));
        assert!(format.frame_at_seconds(f64::NAN).is_err());
    }

    #[test]
    fn pcm_renderer_seeks_and_preserves_interleaved_channels() {
        let format = AudioFormat::new(48_000, 2).unwrap();
        let audio = ProjectAudio::from_interleaved(format, vec![1.0, 2.0, 3.0, 4.0]).unwrap();
        let mut renderer = PcmRenderer::new(audio);
        renderer.seek(ProjectFrame(1));
        let mut output = [0.0; 4];
        assert_eq!(renderer.render_interleaved(&mut output), 1);
        assert_eq!(output, [3.0, 4.0, 0.0, 0.0]);
        assert_eq!(renderer.position(), ProjectFrame(2));
    }

    #[test]
    fn paused_and_stopped_sources_emit_silence_without_advancing() {
        let (handle, mut source) = mono(&[1.0, 2.0]);
        assert_eq!(pull_frame(&mut source), [0.0]);
        assert_eq!(handle.snapshot().frame, ProjectFrame(0));
        handle.play();
        assert_eq!(pull_frame(&mut source), [1.0]);
        handle.pause();
        assert_eq!(pull_frame(&mut source), [0.0]);
        assert_eq!(handle.snapshot().frame, ProjectFrame(1));
        handle.stop();
        assert_eq!(pull_frame(&mut source), [0.0]);
        assert_eq!(handle.snapshot().mode, TransportMode::Stopped);
        assert_eq!(handle.snapshot().frame, ProjectFrame(0));
    }

    #[test]
    fn seek_is_applied_only_between_complete_channel_frames() {
        let (handle, mut source) = stereo_frames(&[[1.0, 2.0], [3.0, 4.0], [5.0, 6.0]]);
        handle.play();
        assert_eq!(source.next(), Some(1.0));
        handle.seek(ProjectFrame(2));
        assert_eq!(source.next(), Some(2.0));
        assert_eq!(pull_frame(&mut source), [5.0, 6.0]);
        assert_eq!(handle.snapshot().frame, ProjectFrame(3));
    }

    #[test]
    fn stereo_frames_always_retain_left_right_order() {
        let (handle, mut source) = stereo_frames(&[[1.0, 10.0], [2.0, 20.0], [3.0, 30.0]]);
        handle.play();
        assert_eq!(pull_frame(&mut source), [1.0, 10.0]);
        assert_eq!(pull_frame(&mut source), [2.0, 20.0]);
        assert_eq!(pull_frame(&mut source), [3.0, 30.0]);
    }

    #[test]
    fn seek_while_paused_begins_at_the_exact_target() {
        let (handle, mut source) = mono(&[1.0, 2.0, 3.0]);
        handle.seek(ProjectFrame(1));
        assert_eq!(pull_frame(&mut source), [0.0]);
        assert_eq!(handle.snapshot().frame, ProjectFrame(1));
        handle.play();
        assert_eq!(pull_frame(&mut source), [2.0]);
    }

    #[test]
    fn paused_seek_outside_an_active_loop_stays_exact_and_does_not_resume() {
        let (handle, mut source) = mono(&[0.0, 1.0, 2.0, 3.0, 4.0]);
        handle
            .set_loop_region(Some(
                FrameRange::new(ProjectFrame(1), ProjectFrame(3)).unwrap(),
            ))
            .unwrap();
        pull_frame(&mut source);

        handle.seek(ProjectFrame(4));
        assert_eq!(pull_frame(&mut source), [0.0]);
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.frame, ProjectFrame(4));
        assert_eq!(snapshot.mode, TransportMode::Paused);
        assert!(snapshot.loop_enabled);

        handle.play();
        assert_eq!(pull_frame(&mut source), [1.0]);
    }

    #[test]
    fn eof_is_explicit_and_play_restarts() {
        let (handle, mut source) = mono(&[1.0, 2.0]);
        handle.play();
        assert_eq!(pull_frame(&mut source), [1.0]);
        assert_eq!(pull_frame(&mut source), [2.0]);
        assert_eq!(handle.snapshot().mode, TransportMode::Ended);
        assert_eq!(handle.snapshot().frame, ProjectFrame(2));
        assert_eq!(pull_frame(&mut source), [0.0]);
        handle.play();
        assert_eq!(pull_frame(&mut source), [1.0]);
    }

    #[test]
    fn toggle_restarts_after_eof_instead_of_pausing_stale_control_state() {
        let (handle, mut source) = mono(&[1.0]);
        handle.toggle();
        assert_eq!(pull_frame(&mut source), [1.0]);
        assert_eq!(handle.snapshot().mode, TransportMode::Ended);
        handle.toggle();
        assert_eq!(pull_frame(&mut source), [1.0]);
        assert_eq!(handle.snapshot().mode, TransportMode::Ended);
    }

    #[test]
    fn loop_is_end_exclusive_and_sample_exact() {
        let (handle, mut source) = mono(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        handle
            .set_loop_region(Some(
                FrameRange::new(ProjectFrame(2), ProjectFrame(5)).unwrap(),
            ))
            .unwrap();
        handle.play();
        let values: Vec<_> = (0..9).map(|_| source.next().unwrap()).collect();
        assert_eq!(values, [0.0, 1.0, 2.0, 3.0, 4.0, 2.0, 3.0, 4.0, 2.0]);
        assert_eq!(handle.snapshot().frame, ProjectFrame(3));
    }

    #[test]
    fn one_frame_loop_makes_progress_and_disable_exits_it() {
        let (handle, mut source) = mono(&[0.0, 1.0, 2.0, 3.0]);
        handle
            .set_loop_region(Some(
                FrameRange::new(ProjectFrame(2), ProjectFrame(3)).unwrap(),
            ))
            .unwrap();
        handle.seek(ProjectFrame(2));
        handle.play();
        assert_eq!(pull_frame(&mut source), [2.0]);
        assert_eq!(pull_frame(&mut source), [2.0]);
        handle.set_loop_enabled(false);
        assert_eq!(pull_frame(&mut source), [2.0]);
        assert_eq!(pull_frame(&mut source), [3.0]);
        assert_eq!(handle.snapshot().mode, TransportMode::Ended);
    }

    #[test]
    fn editing_a_loop_beyond_its_end_preserves_the_paused_playhead() {
        let (handle, mut source) = mono(&[0.0, 1.0, 2.0, 3.0, 4.0]);
        handle.seek(ProjectFrame(4));
        pull_frame(&mut source);
        handle
            .set_loop_region(Some(
                FrameRange::new(ProjectFrame(1), ProjectFrame(3)).unwrap(),
            ))
            .unwrap();
        assert_eq!(pull_frame(&mut source), [0.0]);
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.frame, ProjectFrame(4));
        assert_eq!(snapshot.mode, TransportMode::Paused);
        handle.play();
        assert_eq!(pull_frame(&mut source), [1.0]);
    }

    #[test]
    fn playing_seek_and_loop_edit_beyond_the_end_do_not_jump_to_loop_start() {
        let (handle, mut source) = mono(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        handle
            .set_loop_region(Some(
                FrameRange::new(ProjectFrame(1), ProjectFrame(3)).unwrap(),
            ))
            .unwrap();
        handle.play();
        assert_eq!(pull_frame(&mut source), [0.0]);

        // An explicit locate while already playing is exact even though the
        // old loop remains enabled.
        handle.seek(ProjectFrame(4));
        assert_eq!(pull_frame(&mut source), [4.0]);
        assert_eq!(handle.snapshot().mode, TransportMode::Playing);

        // Replacing the active loop behind the current playhead is likewise a
        // bounds edit, not an implicit seek or playback command.
        handle
            .set_loop_region(Some(
                FrameRange::new(ProjectFrame(1), ProjectFrame(2)).unwrap(),
            ))
            .unwrap();
        assert_eq!(pull_frame(&mut source), [5.0]);
        assert_eq!(handle.snapshot().mode, TransportMode::Ended);
    }

    #[test]
    fn loop_replacement_and_locate_are_one_last_writer_wins_transaction() {
        let (handle, mut source) = mono(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        handle
            .set_loop_state(
                Some(FrameRange::new(ProjectFrame(1), ProjectFrame(3)).unwrap()),
                true,
                Some(ProjectFrame(1)),
            )
            .unwrap();
        handle.play();
        assert_eq!(pull_frame(&mut source), [1.0]);

        let replacement = FrameRange::new(ProjectFrame(4), ProjectFrame(6)).unwrap();
        handle
            .set_loop_state(Some(replacement), true, Some(replacement.start))
            .unwrap();
        assert_eq!(pull_frame(&mut source), [4.0]);
        assert_eq!(pull_frame(&mut source), [5.0]);
        assert_eq!(pull_frame(&mut source), [4.0]);
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.loop_region, Some(replacement));
        assert!(snapshot.loop_enabled);
    }

    #[test]
    fn exact_locate_can_disable_stale_loop_in_the_same_transaction() {
        let (handle, mut source) = mono(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        let stale = FrameRange::new(ProjectFrame(1), ProjectFrame(3)).unwrap();
        handle.set_loop_state(Some(stale), true, None).unwrap();
        handle.play();
        assert_eq!(pull_frame(&mut source), [0.0]);

        handle
            .set_loop_state(Some(stale), false, Some(ProjectFrame(4)))
            .unwrap();
        assert_eq!(pull_frame(&mut source), [4.0]);
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.loop_region, Some(stale));
        assert!(!snapshot.loop_enabled);
    }

    #[test]
    fn invalid_loop_is_rejected_without_changing_the_previous_loop() {
        let (handle, _source) = mono(&[0.0, 1.0, 2.0]);
        let good = FrameRange::new(ProjectFrame(0), ProjectFrame(2)).unwrap();
        handle.set_loop_region(Some(good)).unwrap();
        let bad = FrameRange {
            start: ProjectFrame(2),
            end: ProjectFrame(4),
        };
        assert!(matches!(
            handle.set_loop_region(Some(bad)),
            Err(AudioError::LoopOutOfBounds { .. })
        ));
        assert_eq!(handle.snapshot().loop_region, Some(good));
    }

    #[test]
    fn coherent_snapshot_revision_includes_loop_only_control_edits() {
        let (handle, _source) = mono(&[0.0, 1.0, 2.0]);
        let before = handle.snapshot();
        let range = FrameRange::new(ProjectFrame(1), ProjectFrame(3)).unwrap();
        handle.set_loop_state(Some(range), false, None).unwrap();
        let after = handle.snapshot();
        assert_eq!(after.frame, before.frame);
        assert_eq!(after.mode, before.mode);
        assert_eq!(after.loop_region, Some(range));
        assert!(!after.loop_enabled);
        assert!(after.revision > before.revision);
    }

    #[test]
    fn rapid_seeks_are_last_writer_wins() {
        let (handle, mut source) = mono(&[0.0, 1.0, 2.0, 3.0]);
        handle.seek(ProjectFrame(1));
        handle.seek(ProjectFrame(3));
        handle.play();
        assert_eq!(pull_frame(&mut source), [3.0]);
        assert_eq!(handle.snapshot().mode, TransportMode::Ended);
    }

    #[test]
    fn rapid_commands_use_requested_state_until_the_audio_thread_catches_up() {
        let (handle, mut source) = mono(&[0.0, 1.0, 2.0, 3.0]);

        handle.play();
        handle.seek(ProjectFrame(2));
        assert_eq!(pull_frame(&mut source), [2.0]);
        assert_eq!(handle.snapshot().mode, TransportMode::Playing);

        handle.play();
        handle.pause();
        assert_eq!(pull_frame(&mut source), [0.0]);
        assert_eq!(handle.snapshot().mode, TransportMode::Paused);
        assert_eq!(handle.snapshot().frame, ProjectFrame(3));
    }

    #[test]
    fn loop_edits_do_not_cancel_a_pending_play_command() {
        let (handle, mut source) = mono(&[0.0, 1.0, 2.0, 3.0]);
        handle.play();
        handle
            .set_loop_region(Some(
                FrameRange::new(ProjectFrame(1), ProjectFrame(3)).unwrap(),
            ))
            .unwrap();
        assert_eq!(pull_frame(&mut source), [0.0]);
        assert_eq!(pull_frame(&mut source), [1.0]);
        assert_eq!(handle.snapshot().mode, TransportMode::Playing);
    }

    #[test]
    fn every_applied_command_is_published_with_a_new_revision() {
        let (handle, mut source) = mono(&[0.0, 1.0, 2.0, 3.0]);
        let initial = handle.snapshot();

        handle.play();
        pull_frame(&mut source);
        let playing = handle.snapshot();
        assert_eq!(playing.mode, TransportMode::Playing);
        assert_eq!(playing.frame, ProjectFrame(1));
        assert!(playing.revision > initial.revision);

        handle.seek(ProjectFrame(2));
        pull_frame(&mut source);
        let sought = handle.snapshot();
        assert_eq!(sought.frame, ProjectFrame(3));
        assert!(sought.revision > playing.revision);

        handle.stop();
        pull_frame(&mut source);
        let stopped = handle.snapshot();
        assert_eq!(stopped.mode, TransportMode::Stopped);
        assert_eq!(stopped.frame, ProjectFrame(0));
        assert!(stopped.revision > sought.revision);
    }

    #[test]
    fn looping_has_no_accumulated_phase_drift() {
        let (handle, mut source) = mono(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        let range = FrameRange::new(ProjectFrame(2), ProjectFrame(7)).unwrap();
        handle.set_loop_region(Some(range)).unwrap();
        handle.seek(range.start);
        handle.play();

        for index in 0..20_003_u64 {
            assert_eq!(
                source.next(),
                Some((range.start.0 + index % range.len()) as f32)
            );
        }
        assert_eq!(
            handle.snapshot().frame,
            ProjectFrame(range.start.0 + 20_003 % range.len())
        );
    }

    #[test]
    fn realtime_next_path_does_not_allocate_after_construction() {
        let (handle, mut source) = stereo_frames(&[[1.0, 2.0], [3.0, 4.0]]);
        handle
            .set_loop_region(Some(
                FrameRange::new(ProjectFrame(0), ProjectFrame(2)).unwrap(),
            ))
            .unwrap();
        handle.play();

        // Initialize this test thread's TLS before allocation tracking begins.
        TRACK_ALLOCATIONS.with(|tracking| tracking.set(false));
        ALLOCATION_COUNT.with(|count| count.set(0));
        TRACK_ALLOCATIONS.with(|tracking| tracking.set(true));
        for _ in 0..16_384 {
            let _ = source.next();
        }
        TRACK_ALLOCATIONS.with(|tracking| tracking.set(false));
        let allocations = ALLOCATION_COUNT.with(Cell::get);
        assert_eq!(allocations, 0);
    }

    #[test]
    fn seek_then_play_at_eof_keeps_the_explicit_target() {
        let (handle, mut source) = mono(&[0.0, 1.0, 2.0, 3.0]);
        handle.play();
        for _ in 0..4 {
            pull_frame(&mut source);
        }
        assert_eq!(handle.snapshot().mode, TransportMode::Ended);
        handle.seek(ProjectFrame(2));
        handle.play();
        assert_eq!(pull_frame(&mut source), [2.0]);
    }

    #[test]
    fn rodio_source_metadata_is_project_native_and_source_is_infinite() {
        let (_handle, source) = stereo_frames(&[[1.0, 2.0]]);
        assert_eq!(source.channels().get(), 2);
        assert_eq!(source.sample_rate().get(), 48_000);
        assert_eq!(source.current_span_len(), None);
        assert_eq!(source.total_duration(), None);
    }
}
