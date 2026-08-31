//! Deterministic, hardware-free project rendering and WAV export.
//!
//! This module does not open an audio device and does not apply any implicit
//! mastering. Samples are rendered in project-frame order, non-finite values
//! are replaced with silence, and the only level changes are those selected by
//! [`RenderGain`]. Integer WAV export is the sole point at which samples are
//! clipped to full scale or (optionally) dithered.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::audio::{AudioFormat, ProjectFrame, ProjectRenderer};

/// An end-exclusive range of project frames: `[start, end)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderRange {
    pub start: ProjectFrame,
    pub end: ProjectFrame,
}

impl RenderRange {
    pub fn new(start: ProjectFrame, end: ProjectFrame) -> Result<Self, RenderError> {
        if start >= end {
            return Err(RenderError::EmptyRange { start, end });
        }
        Ok(Self { start, end })
    }

    pub fn from_frames(start: u64, end: u64) -> Result<Self, RenderError> {
        Self::new(ProjectFrame(start), ProjectFrame(end))
    }

    pub fn len(self) -> u64 {
        self.end.0 - self.start.0
    }

    pub fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

/// Sample representation in the exported WAV data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WavSampleFormat {
    Pcm16,
    Pcm24,
    Float32,
}

impl WavSampleFormat {
    pub const fn bits_per_sample(self) -> u16 {
        match self {
            Self::Pcm16 => 16,
            Self::Pcm24 => 24,
            Self::Float32 => 32,
        }
    }

    fn bytes_per_sample(self) -> usize {
        usize::from(self.bits_per_sample() / 8)
    }
}

/// The native project format and the requested WAV representation.
///
/// Offline rendering currently performs neither resampling nor channel
/// conversion, so `audio` must equal the source format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderFormat {
    pub audio: AudioFormat,
    pub wav: WavSampleFormat,
}

impl RenderFormat {
    pub const fn new(audio: AudioFormat, wav: WavSampleFormat) -> Self {
        Self { audio, wav }
    }
}

/// The only gain operations performed by the renderer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RenderGain {
    /// Preserve the sanitized source level exactly.
    Unity,
    /// Multiply every sample by this finite value. Negative gain is allowed.
    Linear(f64),
    /// Scale the render peak to `target_peak`, which must be in `0.0..=1.0`.
    /// Silence remains silent with an effective gain of one.
    NormalizePeak { target_peak: f64 },
}

/// Dither applied immediately before integer quantization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dither {
    None,
    /// Deterministic triangular-PDF dither with a peak-to-peak width of two
    /// quantizer steps. The seed makes exports reproducible.
    Tpdf {
        seed: u64,
    },
}

/// Complete settings for one offline export.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderRequest {
    pub range: RenderRange,
    pub format: RenderFormat,
    pub gain: RenderGain,
    pub dither: Dither,
    /// Maximum number of frames requested from the source per call.
    pub block_frames: usize,
}

impl RenderRequest {
    pub fn new(range: RenderRange, format: RenderFormat) -> Self {
        Self {
            range,
            format,
            gain: RenderGain::Unity,
            dither: Dither::None,
            block_frames: 1_024,
        }
    }
}

/// Offline counterpart to [`ProjectRenderer`].
///
/// A call writes complete interleaved frames, returns the number of frames
/// written, and advances the source position by that amount. Returning zero
/// before `length()` is treated as a truncated source.
pub trait ProjectRenderSource {
    fn format(&self) -> AudioFormat;
    fn length(&self) -> ProjectFrame;
    fn seek(&mut self, frame: ProjectFrame);
    fn render_interleaved(&mut self, output: &mut [f32]) -> usize;
}

/// Every realtime project renderer can also be rendered offline.
impl<T: ProjectRenderer> ProjectRenderSource for T {
    fn format(&self) -> AudioFormat {
        ProjectRenderer::format(self)
    }

    fn length(&self) -> ProjectFrame {
        ProjectRenderer::length(self)
    }

    fn seek(&mut self, frame: ProjectFrame) {
        ProjectRenderer::seek(self, frame);
    }

    fn render_interleaved(&mut self, output: &mut [f32]) -> usize {
        ProjectRenderer::render_interleaved(self, output)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderPhase {
    Rendering,
    Encoding,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderProgress {
    pub phase: RenderPhase,
    pub completed_frames: u64,
    pub total_frames: u64,
}

impl RenderProgress {
    pub fn fraction(self) -> f64 {
        if self.total_frames == 0 {
            1.0
        } else {
            self.completed_frames as f64 / self.total_frames as f64
        }
    }
}

/// Cancellation and progress seam for UI, CLI, or task-system adapters.
pub trait RenderObserver {
    fn is_cancelled(&mut self) -> bool {
        false
    }

    fn report_progress(&mut self, _progress: RenderProgress) {}
}

#[derive(Default)]
pub struct NoopRenderObserver;

impl RenderObserver for NoopRenderObserver {}

/// Cloneable cancellation handle usable directly as a render observer.
#[derive(Clone, Debug, Default)]
pub struct RenderCancellation {
    cancelled: Arc<AtomicBool>,
}

impl RenderCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl RenderObserver for RenderCancellation {
    fn is_cancelled(&mut self) -> bool {
        RenderCancellation::is_cancelled(self)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RenderStats {
    pub frames: u64,
    pub samples: u64,
    /// Peak after replacing non-finite source samples with silence.
    pub source_peak: f32,
    /// Peak after the selected gain. This may exceed 1.0 for float exports.
    pub output_peak: f32,
    pub effective_gain: f64,
    pub non_finite_source_samples: u64,
    pub non_finite_output_samples: u64,
    /// Finite post-gain samples outside `[-1.0, 1.0]`.
    pub samples_over_full_scale: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenderedProject {
    pub format: AudioFormat,
    pub range: RenderRange,
    pub interleaved: Vec<f32>,
    pub stats: RenderStats,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EncodedWav {
    pub bytes: Vec<u8>,
    /// Samples outside full scale before integer quantization.
    pub clipped_samples: u64,
    pub dithered: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WavExport {
    pub bytes: Vec<u8>,
    pub stats: RenderStats,
    pub clipped_samples: u64,
}

/// Render a project range into sanitized, post-gain interleaved `f32` samples.
pub fn render_project<S: ProjectRenderSource, O: RenderObserver>(
    source: &mut S,
    request: &RenderRequest,
    observer: &mut O,
) -> Result<RenderedProject, RenderError> {
    validate_request(source, request)?;
    check_cancelled(observer)?;

    let channels = usize::from(request.format.audio.channels.get());
    let frames = request.range.len();
    let frame_capacity = usize::try_from(frames).map_err(|_| RenderError::RenderTooLarge)?;
    let sample_capacity = frame_capacity
        .checked_mul(channels)
        .ok_or(RenderError::RenderTooLarge)?;
    let scratch_samples = request
        .block_frames
        .checked_mul(channels)
        .ok_or(RenderError::RenderTooLarge)?;

    let mut samples = Vec::with_capacity(sample_capacity);
    let mut scratch = vec![0.0_f32; scratch_samples];
    let mut completed = 0_u64;
    let mut non_finite_source_samples = 0_u64;
    let mut source_peak = 0.0_f32;

    observer.report_progress(RenderProgress {
        phase: RenderPhase::Rendering,
        completed_frames: 0,
        total_frames: frames,
    });
    check_cancelled(observer)?;
    source.seek(request.range.start);

    while completed < frames {
        check_cancelled(observer)?;
        let requested_frames =
            usize::try_from((frames - completed).min(request.block_frames as u64))
                .map_err(|_| RenderError::RenderTooLarge)?;
        let requested_samples = requested_frames * channels;
        let mut block_completed = 0_usize;

        while block_completed < requested_frames {
            check_cancelled(observer)?;
            let offset = block_completed * channels;
            let written = source.render_interleaved(&mut scratch[offset..requested_samples]);
            let remaining = requested_frames - block_completed;
            if written > remaining {
                return Err(RenderError::SourceOverrun {
                    requested_frames: remaining,
                    returned_frames: written,
                });
            }
            if written == 0 {
                return Err(RenderError::UnexpectedSourceEnd {
                    frame: ProjectFrame(request.range.start.0 + completed + block_completed as u64),
                });
            }
            block_completed += written;
        }

        for sample in &scratch[..requested_samples] {
            let sample = if sample.is_finite() {
                *sample
            } else {
                non_finite_source_samples += 1;
                0.0
            };
            source_peak = source_peak.max(sample.abs());
            samples.push(sample);
        }

        completed += requested_frames as u64;
        observer.report_progress(RenderProgress {
            phase: RenderPhase::Rendering,
            completed_frames: completed,
            total_frames: frames,
        });
        check_cancelled(observer)?;
    }

    let effective_gain = effective_gain(request.gain, source_peak)?;
    let mut output_peak = 0.0_f32;
    let mut non_finite_output_samples = 0_u64;
    let mut samples_over_full_scale = 0_u64;
    for sample in &mut samples {
        let scaled = f64::from(*sample) * effective_gain;
        *sample = if scaled.is_nan() {
            non_finite_output_samples += 1;
            0.0
        } else if scaled > f64::from(f32::MAX) {
            non_finite_output_samples += 1;
            f32::MAX
        } else if scaled < -f64::from(f32::MAX) {
            non_finite_output_samples += 1;
            -f32::MAX
        } else {
            scaled as f32
        };
        output_peak = output_peak.max(sample.abs());
        if *sample < -1.0 || *sample > 1.0 {
            samples_over_full_scale += 1;
        }
    }

    Ok(RenderedProject {
        format: request.format.audio,
        range: request.range,
        interleaved: samples,
        stats: RenderStats {
            frames,
            samples: frames
                .checked_mul(channels as u64)
                .ok_or(RenderError::RenderTooLarge)?,
            source_peak,
            output_peak,
            effective_gain,
            non_finite_source_samples,
            non_finite_output_samples,
            samples_over_full_scale,
        },
    })
}

/// Render and encode a complete in-memory WAV file.
pub fn render_to_wav<S: ProjectRenderSource, O: RenderObserver>(
    source: &mut S,
    request: &RenderRequest,
    observer: &mut O,
) -> Result<WavExport, RenderError> {
    let rendered = render_project(source, request, observer)?;
    check_cancelled(observer)?;
    observer.report_progress(RenderProgress {
        phase: RenderPhase::Encoding,
        completed_frames: 0,
        total_frames: request.range.len(),
    });
    check_cancelled(observer)?;
    let encoded = encode_wav(&rendered, request.format.wav, request.dither)?;
    observer.report_progress(RenderProgress {
        phase: RenderPhase::Encoding,
        completed_frames: request.range.len(),
        total_frames: request.range.len(),
    });
    check_cancelled(observer)?;
    Ok(WavExport {
        bytes: encoded.bytes,
        stats: rendered.stats,
        clipped_samples: encoded.clipped_samples,
    })
}

/// Encode rendered interleaved samples as a canonical little-endian WAV file.
pub fn encode_wav(
    rendered: &RenderedProject,
    sample_format: WavSampleFormat,
    dither: Dither,
) -> Result<EncodedWav, RenderError> {
    let channels = usize::from(rendered.format.channels.get());
    if rendered.interleaved.len() % channels != 0 {
        return Err(RenderError::PartialFrame {
            samples: rendered.interleaved.len(),
            channels,
        });
    }

    let bytes_per_sample = sample_format.bytes_per_sample();
    let data_len = rendered
        .interleaved
        .len()
        .checked_mul(bytes_per_sample)
        .ok_or(RenderError::WavTooLarge)?;
    let data_len_u32 = u32::try_from(data_len).map_err(|_| RenderError::WavTooLarge)?;
    let riff_size = 36_u32
        .checked_add(data_len_u32)
        .ok_or(RenderError::WavTooLarge)?;
    let block_align_usize = channels
        .checked_mul(bytes_per_sample)
        .ok_or(RenderError::WavTooLarge)?;
    let block_align = u16::try_from(block_align_usize).map_err(|_| RenderError::WavTooLarge)?;
    let byte_rate = rendered
        .format
        .sample_rate
        .get()
        .checked_mul(u32::from(block_align))
        .ok_or(RenderError::WavTooLarge)?;

    let mut bytes = Vec::with_capacity(
        44_usize
            .checked_add(data_len)
            .ok_or(RenderError::WavTooLarge)?,
    );
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&riff_size.to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    let wav_tag = if sample_format == WavSampleFormat::Float32 {
        3_u16
    } else {
        1_u16
    };
    bytes.extend_from_slice(&wav_tag.to_le_bytes());
    bytes.extend_from_slice(&rendered.format.channels.get().to_le_bytes());
    bytes.extend_from_slice(&rendered.format.sample_rate.get().to_le_bytes());
    bytes.extend_from_slice(&byte_rate.to_le_bytes());
    bytes.extend_from_slice(&block_align.to_le_bytes());
    bytes.extend_from_slice(&sample_format.bits_per_sample().to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len_u32.to_le_bytes());

    let mut random = match dither {
        Dither::None => None,
        Dither::Tpdf { seed } if sample_format != WavSampleFormat::Float32 => {
            Some(DeterministicRandom::new(seed))
        }
        Dither::Tpdf { .. } => None,
    };
    let mut clipped_samples = 0_u64;
    for &raw_sample in &rendered.interleaved {
        let sample = if raw_sample.is_finite() {
            raw_sample
        } else {
            0.0
        };
        if sample < -1.0 || sample > 1.0 {
            clipped_samples += 1;
        }
        match sample_format {
            WavSampleFormat::Pcm16 => {
                let quantized = quantize_signed(sample, 16, random.as_mut());
                bytes.extend_from_slice(&(quantized as i16).to_le_bytes());
            }
            WavSampleFormat::Pcm24 => {
                let quantized = quantize_signed(sample, 24, random.as_mut());
                let encoded = quantized.to_le_bytes();
                bytes.extend_from_slice(&encoded[..3]);
            }
            WavSampleFormat::Float32 => bytes.extend_from_slice(&sample.to_le_bytes()),
        }
    }

    Ok(EncodedWav {
        bytes,
        clipped_samples,
        dithered: random.is_some(),
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ResidualMetrics {
    pub samples: u64,
    pub peak_absolute: f64,
    pub mean_absolute: f64,
    pub rms: f64,
    pub dc_offset: f64,
    pub reference_peak: f64,
    pub reference_rms: f64,
    /// Non-finite values in either the reference or any additive component.
    /// They are treated as silence for the comparison.
    pub non_finite_inputs: u64,
}

/// Measure `reference - (component[0] + component[1] + ...)` sample by sample.
///
/// Components must have exactly the same interleaved sample count as the
/// reference. Accumulation uses `f64` so the metric itself does not introduce
/// ordinary `f32` summation error.
pub fn additive_residual(
    reference: &[f32],
    components: &[&[f32]],
) -> Result<ResidualMetrics, RenderError> {
    for (index, component) in components.iter().enumerate() {
        if component.len() != reference.len() {
            return Err(RenderError::ResidualLengthMismatch {
                component: index,
                expected_samples: reference.len(),
                actual_samples: component.len(),
            });
        }
    }
    if reference.is_empty() {
        return Ok(ResidualMetrics::default());
    }

    let mut metrics = ResidualMetrics {
        samples: reference.len() as u64,
        ..ResidualMetrics::default()
    };
    let mut absolute_sum = 0.0_f64;
    let mut residual_square_sum = 0.0_f64;
    let mut residual_sum = 0.0_f64;
    let mut reference_square_sum = 0.0_f64;

    for sample_index in 0..reference.len() {
        let reference_sample =
            finite_or_silence(reference[sample_index], &mut metrics.non_finite_inputs);
        let mut component_sum = 0.0_f64;
        for component in components {
            component_sum +=
                finite_or_silence(component[sample_index], &mut metrics.non_finite_inputs);
        }
        let residual = reference_sample - component_sum;
        let absolute = residual.abs();
        metrics.peak_absolute = metrics.peak_absolute.max(absolute);
        metrics.reference_peak = metrics.reference_peak.max(reference_sample.abs());
        absolute_sum += absolute;
        residual_square_sum += residual * residual;
        residual_sum += residual;
        reference_square_sum += reference_sample * reference_sample;
    }

    let count = reference.len() as f64;
    metrics.mean_absolute = absolute_sum / count;
    metrics.rms = (residual_square_sum / count).sqrt();
    metrics.dc_offset = residual_sum / count;
    metrics.reference_rms = (reference_square_sum / count).sqrt();
    Ok(metrics)
}

#[derive(Clone, Debug, PartialEq)]
pub enum RenderError {
    EmptyRange {
        start: ProjectFrame,
        end: ProjectFrame,
    },
    RangeOutOfBounds {
        range: RenderRange,
        length: ProjectFrame,
    },
    FormatMismatch {
        source: AudioFormat,
        requested: AudioFormat,
    },
    ZeroBlockFrames,
    NonFiniteGain(f64),
    InvalidNormalizationTarget(f64),
    RenderTooLarge,
    UnexpectedSourceEnd {
        frame: ProjectFrame,
    },
    SourceOverrun {
        requested_frames: usize,
        returned_frames: usize,
    },
    PartialFrame {
        samples: usize,
        channels: usize,
    },
    WavTooLarge,
    ResidualLengthMismatch {
        component: usize,
        expected_samples: usize,
        actual_samples: usize,
    },
    Cancelled,
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRange { start, end } => write!(
                f,
                "render range must be non-empty, got {}..{}",
                start.0, end.0
            ),
            Self::RangeOutOfBounds { range, length } => write!(
                f,
                "render range {}..{} exceeds project length {}",
                range.start.0, range.end.0, length.0
            ),
            Self::FormatMismatch { source, requested } => write!(
                f,
                "offline resampling/channel conversion is unavailable: source is {} Hz/{} channels, request is {} Hz/{} channels",
                source.sample_rate,
                source.channels,
                requested.sample_rate,
                requested.channels
            ),
            Self::ZeroBlockFrames => write!(f, "render block size must not be zero"),
            Self::NonFiniteGain(gain) => write!(f, "render gain must be finite, got {gain}"),
            Self::InvalidNormalizationTarget(target) => write!(
                f,
                "normalization target must be finite and in 0.0..=1.0, got {target}"
            ),
            Self::RenderTooLarge => write!(f, "render is too large for this process"),
            Self::UnexpectedSourceEnd { frame } => {
                write!(f, "project source ended unexpectedly at frame {}", frame.0)
            }
            Self::SourceOverrun {
                requested_frames,
                returned_frames,
            } => write!(
                f,
                "project source returned {returned_frames} frames for a {requested_frames}-frame buffer"
            ),
            Self::PartialFrame { samples, channels } => write!(
                f,
                "{samples} samples do not contain complete {channels}-channel frames"
            ),
            Self::WavTooLarge => write!(f, "render exceeds the classic RIFF/WAV size limit"),
            Self::ResidualLengthMismatch {
                component,
                expected_samples,
                actual_samples,
            } => write!(
                f,
                "residual component {component} has {actual_samples} samples; expected {expected_samples}"
            ),
            Self::Cancelled => write!(f, "render cancelled"),
        }
    }
}

impl std::error::Error for RenderError {}

fn validate_request<S: ProjectRenderSource>(
    source: &S,
    request: &RenderRequest,
) -> Result<(), RenderError> {
    if request.range.is_empty() {
        return Err(RenderError::EmptyRange {
            start: request.range.start,
            end: request.range.end,
        });
    }
    if request.range.end > source.length() {
        return Err(RenderError::RangeOutOfBounds {
            range: request.range,
            length: source.length(),
        });
    }
    if request.format.audio != source.format() {
        return Err(RenderError::FormatMismatch {
            source: source.format(),
            requested: request.format.audio,
        });
    }
    if request.block_frames == 0 {
        return Err(RenderError::ZeroBlockFrames);
    }
    effective_gain(request.gain, 1.0)?;
    Ok(())
}

fn effective_gain(gain: RenderGain, peak: f32) -> Result<f64, RenderError> {
    match gain {
        RenderGain::Unity => Ok(1.0),
        RenderGain::Linear(value) if value.is_finite() => Ok(value),
        RenderGain::Linear(value) => Err(RenderError::NonFiniteGain(value)),
        RenderGain::NormalizePeak { target_peak }
            if target_peak.is_finite() && (0.0..=1.0).contains(&target_peak) =>
        {
            if peak == 0.0 {
                Ok(1.0)
            } else {
                Ok(target_peak / f64::from(peak))
            }
        }
        RenderGain::NormalizePeak { target_peak } => {
            Err(RenderError::InvalidNormalizationTarget(target_peak))
        }
    }
}

fn check_cancelled(observer: &mut impl RenderObserver) -> Result<(), RenderError> {
    if observer.is_cancelled() {
        Err(RenderError::Cancelled)
    } else {
        Ok(())
    }
}

fn finite_or_silence(value: f32, non_finite_count: &mut u64) -> f64 {
    if value.is_finite() {
        f64::from(value)
    } else {
        *non_finite_count += 1;
        0.0
    }
}

fn quantize_signed(sample: f32, bits: u32, random: Option<&mut DeterministicRandom>) -> i32 {
    let scale = (1_u32 << (bits - 1)) as f64;
    let dither = random.map_or(0.0, DeterministicRandom::tpdf);
    let quantized = (f64::from(sample).clamp(-1.0, 1.0) * scale + dither).round();
    let minimum = -scale;
    let maximum = scale - 1.0;
    quantized.clamp(minimum, maximum) as i32
}

struct DeterministicRandom {
    state: u64,
}

impl DeterministicRandom {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn uniform(&mut self) -> f64 {
        // SplitMix64 is fully specified with wrapping integer operations.
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^= value >> 31;
        (value >> 11) as f64 * (1.0 / ((1_u64 << 53) as f64))
    }

    fn tpdf(&mut self) -> f64 {
        self.uniform() - self.uniform()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct TestSource {
        format: AudioFormat,
        samples: Vec<f32>,
        position: u64,
        max_frames_per_call: usize,
    }

    impl TestSource {
        fn new(channels: u16, samples: Vec<f32>) -> Self {
            Self {
                format: AudioFormat::new(48_000, channels).unwrap(),
                samples,
                position: 0,
                max_frames_per_call: usize::MAX,
            }
        }
    }

    impl ProjectRenderSource for TestSource {
        fn format(&self) -> AudioFormat {
            self.format
        }

        fn length(&self) -> ProjectFrame {
            ProjectFrame((self.samples.len() / usize::from(self.format.channels.get())) as u64)
        }

        fn seek(&mut self, frame: ProjectFrame) {
            self.position = frame.0.min(self.length().0);
        }

        fn render_interleaved(&mut self, output: &mut [f32]) -> usize {
            let channels = usize::from(self.format.channels.get());
            let available = (self.length().0 - self.position) as usize;
            let frames = (output.len() / channels)
                .min(available)
                .min(self.max_frames_per_call);
            let start = self.position as usize * channels;
            let sample_count = frames * channels;
            output[..sample_count].copy_from_slice(&self.samples[start..start + sample_count]);
            self.position += frames as u64;
            frames
        }
    }

    fn request(source: &TestSource, range: RenderRange, wav: WavSampleFormat) -> RenderRequest {
        RenderRequest::new(range, RenderFormat::new(source.format, wav))
    }

    fn pcm16_data(bytes: &[u8]) -> Vec<i16> {
        bytes[44..]
            .chunks_exact(2)
            .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
            .collect()
    }

    #[test]
    fn renders_exact_range_channel_order_and_short_source_blocks() {
        let mut source = TestSource::new(2, (0..16).map(|sample| sample as f32).collect());
        source.max_frames_per_call = 1;
        let range = RenderRange::from_frames(2, 7).unwrap();
        let mut request = request(&source, range, WavSampleFormat::Float32);
        request.block_frames = 3;

        let rendered = render_project(&mut source, &request, &mut NoopRenderObserver).unwrap();
        assert_eq!(rendered.stats.frames, 5);
        assert_eq!(rendered.stats.samples, 10);
        assert_eq!(
            rendered.interleaved,
            (4..14).map(|sample| sample as f32).collect::<Vec<_>>()
        );
        assert_eq!(source.position, 7);
    }

    #[test]
    fn wav_headers_and_payload_sizes_match_channels_and_frames() {
        let mut source = TestSource::new(2, vec![0.0; 14]);
        let range = RenderRange::from_frames(1, 6).unwrap();
        let request = request(&source, range, WavSampleFormat::Pcm24);
        let export = render_to_wav(&mut source, &request, &mut NoopRenderObserver).unwrap();
        assert_eq!(&export.bytes[..4], b"RIFF");
        assert_eq!(u16::from_le_bytes([export.bytes[22], export.bytes[23]]), 2);
        assert_eq!(u16::from_le_bytes([export.bytes[34], export.bytes[35]]), 24);
        assert_eq!(
            u32::from_le_bytes(export.bytes[40..44].try_into().unwrap()),
            30
        );
        assert_eq!(export.bytes.len(), 44 + 5 * 2 * 3);
    }

    #[test]
    fn sanitizes_non_finite_samples_and_peak_normalizes_only_when_requested() {
        let mut source = TestSource::new(1, vec![f32::NAN, f32::INFINITY, -0.25, 0.5]);
        let mut request = request(
            &source,
            RenderRange::from_frames(0, 4).unwrap(),
            WavSampleFormat::Float32,
        );
        request.gain = RenderGain::NormalizePeak { target_peak: 0.8 };
        let rendered = render_project(&mut source, &request, &mut NoopRenderObserver).unwrap();

        assert_eq!(rendered.stats.non_finite_source_samples, 2);
        assert_eq!(rendered.interleaved, vec![0.0, 0.0, -0.4, 0.8]);
        assert_eq!(rendered.stats.source_peak, 0.5);
        assert_eq!(rendered.stats.output_peak, 0.8);
        assert_eq!(rendered.stats.effective_gain, 1.6);
    }

    struct CancelAfterFirstBlock {
        cancelled: bool,
    }

    impl RenderObserver for CancelAfterFirstBlock {
        fn is_cancelled(&mut self) -> bool {
            self.cancelled
        }

        fn report_progress(&mut self, progress: RenderProgress) {
            if progress.phase == RenderPhase::Rendering && progress.completed_frames > 0 {
                self.cancelled = true;
            }
        }
    }

    #[test]
    fn cancellation_stops_between_blocks() {
        let mut source = TestSource::new(1, vec![0.0; 20]);
        let mut request = request(
            &source,
            RenderRange::from_frames(0, 20).unwrap(),
            WavSampleFormat::Pcm16,
        );
        request.block_frames = 4;
        let error = render_project(
            &mut source,
            &request,
            &mut CancelAfterFirstBlock { cancelled: false },
        )
        .unwrap_err();
        assert_eq!(error, RenderError::Cancelled);
        assert_eq!(source.position, 4);
    }

    #[test]
    fn integer_export_clips_but_float_render_does_not() {
        let mut source = TestSource::new(1, vec![-2.0, -1.0, 0.0, 1.0, 2.0]);
        let request = request(
            &source,
            RenderRange::from_frames(0, 5).unwrap(),
            WavSampleFormat::Pcm16,
        );
        let rendered = render_project(&mut source, &request, &mut NoopRenderObserver).unwrap();
        assert_eq!(rendered.interleaved, vec![-2.0, -1.0, 0.0, 1.0, 2.0]);
        assert_eq!(rendered.stats.samples_over_full_scale, 2);

        let encoded = encode_wav(&rendered, WavSampleFormat::Pcm16, Dither::None).unwrap();
        assert_eq!(encoded.clipped_samples, 2);
        assert_eq!(
            pcm16_data(&encoded.bytes),
            vec![i16::MIN, i16::MIN, 0, i16::MAX, i16::MAX]
        );
    }

    #[test]
    fn tpdf_dither_is_seeded_and_deterministic() {
        let source = TestSource::new(1, vec![0.0; 2_048]);
        let mut source_a = source.clone();
        let request = request(
            &source,
            RenderRange::from_frames(0, 2_048).unwrap(),
            WavSampleFormat::Pcm16,
        );
        let rendered = render_project(&mut source_a, &request, &mut NoopRenderObserver).unwrap();
        let first =
            encode_wav(&rendered, WavSampleFormat::Pcm16, Dither::Tpdf { seed: 7 }).unwrap();
        let second =
            encode_wav(&rendered, WavSampleFormat::Pcm16, Dither::Tpdf { seed: 7 }).unwrap();
        let other =
            encode_wav(&rendered, WavSampleFormat::Pcm16, Dither::Tpdf { seed: 8 }).unwrap();

        assert_eq!(first.bytes, second.bytes);
        assert_ne!(first.bytes, other.bytes);
        assert!(pcm16_data(&first.bytes).iter().any(|&sample| sample != 0));
    }

    #[test]
    fn additive_residual_reports_exact_reconstruction_and_error() {
        let reference = [0.5, -0.25, 1.0, 0.0];
        let first = [0.125, -0.5, 0.75, 0.0];
        let second = [0.375, 0.25, 0.0, 0.0];
        let exact = additive_residual(&reference, &[&first, &second]).unwrap();
        assert_eq!(exact.peak_absolute, 0.25);
        assert_eq!(exact.mean_absolute, 0.0625);
        assert_eq!(exact.rms, 0.125);

        let complete_second = [0.375, 0.25, 0.25, 0.0];
        let complete = additive_residual(&reference, &[&first, &complete_second]).unwrap();
        assert_eq!(complete.peak_absolute, 0.0);
        assert_eq!(complete.rms, 0.0);
    }

    #[test]
    fn residual_rejects_mismatched_component_lengths() {
        let error = additive_residual(&[0.0, 1.0], &[&[0.0]]).unwrap_err();
        assert_eq!(
            error,
            RenderError::ResidualLengthMismatch {
                component: 0,
                expected_samples: 2,
                actual_samples: 1,
            }
        );
    }
}
