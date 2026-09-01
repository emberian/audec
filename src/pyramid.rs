//! A lossless (for min/max/RMS) multiresolution waveform envelope.
//!
//! The old audec UI reduced a whole track to one fixed vector.  That is fine
//! for an atlas, but it means a close inspection has no more information than
//! the overview.  `WaveformPyramid` keeps channel-aware, power-of-two summary
//! levels, so a renderer can request exactly one bin per screen pixel for any
//! time range.  Queries never include samples outside their requested range.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::streaming_media::{
    DecodedPcmDescriptor, MediaDigest, PcmChunkIndex, StreamingMediaError, WaveformChunkSideProduct,
};

/// Source frames in the first summary level.
///
/// PCM stays compact below this resolution.  With 256-frame blocks, summary
/// accumulators add roughly 4.7% over f32 PCM on 64-bit targets (their geometric
/// series is fewer than `2 * channels * frames / 256` accumulators).
pub const BASE_BLOCK_FRAMES: usize = 256;

/// Envelope statistics for one channel over a contiguous group of frames.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ChannelEnvelope {
    /// Smallest finite sample in the group.
    pub min: f32,
    /// Largest finite sample in the group.
    pub max: f32,
    /// Root-mean-square amplitude of the group.
    pub rms: f32,
    /// Number of source frames represented by this envelope.
    pub frames: usize,
}

/// A channel-aware range of source frames, suitable for one waveform pixel.
#[derive(Clone, Debug, PartialEq)]
pub struct WaveformBin {
    /// Inclusive source-frame start.
    pub start_frame: usize,
    /// Exclusive source-frame end.
    pub end_frame: usize,
    /// Envelope statistics in source channel order.
    pub channels: Vec<ChannelEnvelope>,
}

/// The exact result of a visible-range waveform request.
#[derive(Clone, Debug, PartialEq)]
pub struct WaveformQuery {
    /// Clamped inclusive source-frame start.
    pub start_frame: usize,
    /// Clamped exclusive source-frame end.
    pub end_frame: usize,
    /// The largest source-frame power-of-two aggregation appropriate for the
    /// request.
    ///
    /// Individual result bins may also use smaller levels at their edges to
    /// avoid leaking samples from outside the exact requested interval.
    pub preferred_level: usize,
    /// At most one bin per requested target bin; never empty-width bins.
    pub bins: Vec<WaveformBin>,
}

/// Compact accounting for the waveform data retained by a pyramid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaveformStorage {
    /// Interleaved f32 samples retained for sub-base exact zooms.
    pub pcm_samples: usize,
    /// Number of summary accumulators, never one per source sample.
    pub summary_accumulators: usize,
    /// Number of contiguous summary buffers.
    pub summary_levels: usize,
    /// Bytes in the retained PCM and summary buffers, excluding Vec headers.
    pub estimated_bytes: usize,
}

#[derive(Clone, Copy, Debug)]
struct Accumulator {
    min: f32,
    max: f32,
    sum_squares: f64,
    frames: usize,
}

impl Accumulator {
    fn sample(sample: f32) -> Self {
        // Corrupt/undefined audio should not poison every zoom level with NaN.
        // Decoders normally produce finite PCM; treating a non-finite sample as
        // silence makes the pyramid deterministic if one does not.
        let sample = if sample.is_finite() { sample } else { 0.0 };
        Self {
            min: sample,
            max: sample,
            sum_squares: f64::from(sample) * f64::from(sample),
            frames: 1,
        }
    }

    fn empty() -> Self {
        Self {
            min: f32::INFINITY,
            max: f32::NEG_INFINITY,
            sum_squares: 0.0,
            frames: 0,
        }
    }

    fn from_streaming_envelope(
        envelope: crate::streaming_media::ChannelEnvelope,
    ) -> Result<Self, StreamingWaveformError> {
        if envelope.frames == 0
            || !envelope.min.is_finite()
            || !envelope.max.is_finite()
            || !envelope.rms.is_finite()
            || envelope.min > envelope.max
            || envelope.rms < 0.0
        {
            return Err(StreamingWaveformError::InvalidSideProduct);
        }
        Ok(Self {
            min: envelope.min,
            max: envelope.max,
            sum_squares: f64::from(envelope.rms)
                * f64::from(envelope.rms)
                * f64::from(envelope.frames),
            frames: envelope.frames as usize,
        })
    }

    fn extend(&mut self, other: Self) {
        if other.frames == 0 {
            return;
        }
        if self.frames == 0 {
            *self = other;
            return;
        }
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.sum_squares += other.sum_squares;
        self.frames += other.frames;
    }

    fn envelope(self) -> ChannelEnvelope {
        let rms = if self.frames == 0 {
            0.0
        } else {
            (self.sum_squares / self.frames as f64).sqrt() as f32
        };
        ChannelEnvelope {
            min: if self.frames == 0 { 0.0 } else { self.min },
            max: if self.frames == 0 { 0.0 } else { self.max },
            rms,
            frames: self.frames,
        }
    }
}

/// One power-of-two resolution, stored as a flat row-major buffer.
///
/// `values[bin * channel_count + channel]` is the accumulator for one source
/// interval and channel.  There is deliberately no per-frame `Vec` here:
/// building a pyramid makes one allocation for this buffer per level.
#[derive(Clone, Debug)]
struct Level {
    block_frames: usize,
    values: Vec<Accumulator>,
}

/// A set of power-of-two waveform summary levels.
///
/// Compact interleaved f32 PCM is retained once.  Level zero starts at
/// [`BASE_BLOCK_FRAMES`] source frames and each following level doubles that
/// span.  This keeps a 6-minute 44.1 kHz stereo track near its canonical
/// 121 MiB PCM footprint rather than materializing a 24-byte accumulator for
/// every channel/sample; summaries add about 6 MiB at the default base size.
#[derive(Clone, Debug)]
pub struct WaveformPyramid {
    channel_count: usize,
    frame_count: usize,
    pcm: Arc<[f32]>,
    levels: Vec<Level>,
    sanitized_non_finite_samples: usize,
}

impl WaveformPyramid {
    /// Builds from interleaved PCM.  A trailing incomplete frame is ignored.
    pub fn from_interleaved(samples: &[f32], channel_count: usize) -> Self {
        if channel_count == 0 {
            return Self::empty(0);
        }
        let frame_count = samples.len() / channel_count;
        let retained = &samples[..frame_count * channel_count];
        let sanitized_non_finite_samples =
            retained.iter().filter(|sample| !sample.is_finite()).count();
        let pcm = retained
            .iter()
            .map(|sample| if sample.is_finite() { *sample } else { 0.0 })
            .collect();
        Self::from_pcm(
            channel_count,
            frame_count,
            pcm,
            sanitized_non_finite_samples,
        )
    }

    /// Strict constructor used by streamed media publication. Unlike the
    /// compatibility constructor above, corrupt PCM is quarantined rather
    /// than rendered as plausible silence.
    pub fn try_from_finite_interleaved(
        samples: &[f32],
        channel_count: usize,
    ) -> Result<Self, PyramidBuildError> {
        if channel_count == 0 {
            return Err(PyramidBuildError::ZeroChannels);
        }
        if samples.len() % channel_count != 0 {
            return Err(PyramidBuildError::PartialFrame {
                samples: samples.len(),
                channels: channel_count,
            });
        }
        if let Some((sample_index, _)) = samples
            .iter()
            .enumerate()
            .find(|(_, sample)| !sample.is_finite())
        {
            return Err(PyramidBuildError::NonFinitePcm { sample_index });
        }
        Ok(Self::from_pcm(
            channel_count,
            samples.len() / channel_count,
            samples.to_vec(),
            0,
        ))
    }

    /// Builds from planar PCM.  When channel lengths differ, the common
    /// prefix is used: an envelope always represents complete audio frames.
    pub fn from_channels(channels: &[&[f32]]) -> Self {
        let channel_count = channels.len();
        let frame_count = channels
            .iter()
            .map(|channel| channel.len())
            .min()
            .unwrap_or(0);
        let mut pcm = Vec::with_capacity(frame_count * channel_count);
        for frame in 0..frame_count {
            pcm.extend(channels.iter().map(|channel| {
                let sample = channel[frame];
                if sample.is_finite() {
                    sample
                } else {
                    0.0
                }
            }));
        }
        let sanitized_non_finite_samples = channels
            .iter()
            .flat_map(|channel| channel.iter().take(frame_count))
            .filter(|sample| !sample.is_finite())
            .count();
        Self::from_pcm(
            channel_count,
            frame_count,
            pcm,
            sanitized_non_finite_samples,
        )
    }

    /// Number of channels kept in every envelope bin.
    pub fn channel_count(&self) -> usize {
        self.channel_count
    }

    /// Number of complete source frames represented by the pyramid.
    pub fn frame_count(&self) -> usize {
        self.frame_count
    }

    /// Number of stored power-of-two levels (zero for empty audio).
    pub fn level_count(&self) -> usize {
        self.levels.len()
    }

    /// Number of samples replaced with silence by the legacy compatibility
    /// constructor. Streaming and newly imported media should use the strict
    /// constructor and will therefore always report zero here.
    pub const fn sanitized_non_finite_samples(&self) -> usize {
        self.sanitized_non_finite_samples
    }

    /// Canonical sanitized PCM retained by the pyramid, interleaved in source
    /// channel order. Analysis transforms may borrow this to inspect or
    /// reconstruct an exact selected span without decoding the material again.
    pub fn interleaved_pcm(&self) -> &[f32] {
        &self.pcm
    }

    /// Share canonical PCM with playback, rendering, and background analysis
    /// without copying an album-sized buffer for each consumer.
    pub fn shared_interleaved_pcm(&self) -> Arc<[f32]> {
        Arc::clone(&self.pcm)
    }

    /// Returns storage accounting without exposing internal buffers.
    pub fn storage(&self) -> WaveformStorage {
        let summary_accumulators = self.levels.iter().map(|level| level.values.len()).sum();
        WaveformStorage {
            pcm_samples: self.pcm.len(),
            summary_accumulators,
            summary_levels: self.levels.len(),
            estimated_bytes: self.pcm.len() * std::mem::size_of::<f32>()
                + summary_accumulators * std::mem::size_of::<Accumulator>(),
        }
    }

    /// Returns exact, channel-aware min/max/RMS envelopes for a visible range.
    ///
    /// `start_frame` and `end_frame` are clamped to the source.  `target_bins`
    /// is a maximum: a ten-frame range queried with 100 target bins returns
    /// ten one-frame bins rather than ninety empty ones.
    pub fn query(&self, start_frame: usize, end_frame: usize, target_bins: usize) -> WaveformQuery {
        let start_frame = start_frame.min(self.frame_count);
        let end_frame = end_frame.min(self.frame_count).max(start_frame);
        let frame_count = end_frame - start_frame;
        if frame_count == 0 || target_bins == 0 {
            return WaveformQuery {
                start_frame,
                end_frame,
                preferred_level: 0,
                bins: Vec::new(),
            };
        }

        let bin_count = target_bins.min(frame_count);
        let frames_per_bin = (frame_count + bin_count - 1) / bin_count;
        let preferred_level = floor_log2(frames_per_bin);
        let max_summary_level = if frames_per_bin < BASE_BLOCK_FRAMES {
            0
        } else {
            floor_log2(frames_per_bin / BASE_BLOCK_FRAMES).min(self.levels.len().saturating_sub(1))
        };
        let mut bins = Vec::with_capacity(bin_count);

        for bin in 0..bin_count {
            let bin_start = start_frame + frame_count * bin / bin_count;
            let bin_end = start_frame + frame_count * (bin + 1) / bin_count;
            bins.push(self.aggregate_exact(bin_start, bin_end, max_summary_level));
        }

        WaveformQuery {
            start_frame,
            end_frame,
            preferred_level,
            bins,
        }
    }

    fn empty(channel_count: usize) -> Self {
        Self {
            channel_count,
            frame_count: 0,
            pcm: Vec::new().into(),
            levels: Vec::new(),
            sanitized_non_finite_samples: 0,
        }
    }

    fn from_pcm(
        channel_count: usize,
        frame_count: usize,
        pcm: Vec<f32>,
        sanitized_non_finite_samples: usize,
    ) -> Self {
        if frame_count == 0 {
            return Self::empty(channel_count);
        }
        debug_assert_eq!(pcm.len(), frame_count * channel_count);
        let base_bins = frame_count.div_ceil(BASE_BLOCK_FRAMES);
        let mut base_values = Vec::with_capacity(base_bins * channel_count);
        for bin in 0..base_bins {
            let start = bin * BASE_BLOCK_FRAMES;
            let end = (start + BASE_BLOCK_FRAMES).min(frame_count);
            base_values.extend(Self::aggregate_pcm_range(&pcm, channel_count, start, end));
        }
        let mut levels = vec![Level {
            block_frames: BASE_BLOCK_FRAMES,
            values: base_values,
        }];
        let mut previous_bins = base_bins;
        while previous_bins > 1 {
            let previous = levels.last().expect("a level was just inserted");
            let next_bins = previous_bins.div_ceil(2);
            let mut values = Vec::with_capacity(next_bins * channel_count);
            for bin in 0..next_bins {
                for channel in 0..channel_count {
                    let mut combined = previous.values[(bin * 2) * channel_count + channel];
                    if bin * 2 + 1 < previous_bins {
                        combined.extend(previous.values[(bin * 2 + 1) * channel_count + channel]);
                    }
                    values.push(combined);
                }
            }
            levels.push(Level {
                block_frames: previous.block_frames * 2,
                values,
            });
            previous_bins = next_bins;
        }
        Self {
            channel_count,
            frame_count,
            pcm: pcm.into(),
            levels,
            sanitized_non_finite_samples,
        }
    }

    fn aggregate_exact(
        &self,
        start_frame: usize,
        end_frame: usize,
        max_level: usize,
    ) -> WaveformBin {
        debug_assert!(start_frame < end_frame);
        let mut cursor = start_frame;
        let mut accumulator = vec![Accumulator::empty(); self.channel_count];

        while cursor < end_frame {
            if let Some((level, index, candidate_end)) =
                self.summary_at(cursor, end_frame, max_level)
            {
                let row_start = index * self.channel_count;
                for (output, input) in accumulator
                    .iter_mut()
                    .zip(&self.levels[level].values[row_start..row_start + self.channel_count])
                {
                    output.extend(*input);
                }
                cursor = candidate_end;
            } else {
                // There are fewer than BASE_BLOCK_FRAMES before an aligned
                // summary block (or the end): inspect only this bounded PCM
                // edge fragment and preserve exact sub-base zooms.
                let next_aligned = cursor
                    .checked_add(BASE_BLOCK_FRAMES - cursor % BASE_BLOCK_FRAMES)
                    .unwrap_or(end_frame);
                let raw_end = next_aligned.min(end_frame);
                for (output, input) in accumulator.iter_mut().zip(Self::aggregate_pcm_range(
                    &self.pcm,
                    self.channel_count,
                    cursor,
                    raw_end,
                )) {
                    output.extend(input);
                }
                cursor = raw_end;
            }
        }

        WaveformBin {
            start_frame,
            end_frame,
            channels: accumulator.into_iter().map(Accumulator::envelope).collect(),
        }
    }

    fn summary_at(
        &self,
        cursor: usize,
        end_frame: usize,
        max_level: usize,
    ) -> Option<(usize, usize, usize)> {
        for level in (0..=max_level.min(self.levels.len().saturating_sub(1))).rev() {
            let block_frames = self.levels[level].block_frames;
            if cursor % block_frames != 0 {
                continue;
            }
            let candidate_end = (cursor + block_frames).min(self.frame_count);
            if candidate_end <= end_frame {
                return Some((level, cursor / block_frames, candidate_end));
            }
        }
        None
    }

    fn aggregate_pcm_range(
        pcm: &[f32],
        channel_count: usize,
        start_frame: usize,
        end_frame: usize,
    ) -> Vec<Accumulator> {
        let mut result = vec![Accumulator::empty(); channel_count];
        for frame in start_frame..end_frame {
            let offset = frame * channel_count;
            for (output, sample) in result.iter_mut().zip(&pcm[offset..offset + channel_count]) {
                output.extend(Accumulator::sample(*sample));
            }
        }
        result
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PyramidBuildError {
    ZeroChannels,
    PartialFrame { samples: usize, channels: usize },
    NonFinitePcm { sample_index: usize },
}

impl fmt::Display for PyramidBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroChannels => formatter.write_str("waveform PCM has no channels"),
            Self::PartialFrame { samples, channels } => write!(
                formatter,
                "waveform PCM has {samples} samples, not complete {channels}-channel frames"
            ),
            Self::NonFinitePcm { sample_index } => {
                write!(
                    formatter,
                    "waveform PCM sample {sample_index} is not finite"
                )
            }
        }
    }
}

impl Error for PyramidBuildError {}

/// Sparse index over the waveform summaries emitted beside streamed PCM
/// chunks. Full summary bins are merged without reading PCM; only partial
/// bins and missing summary extents are fetched from the bounded media store.
/// Consequently exact viewport edges do not force whole-file residency.
#[derive(Clone, Debug)]
pub struct StreamingWaveformIndex<D: MediaDigest> {
    source: DecodedPcmDescriptor<D>,
    products: BTreeMap<PcmChunkIndex, WaveformChunkSideProduct<D>>,
}

impl<D: MediaDigest> StreamingWaveformIndex<D> {
    pub fn new(source: DecodedPcmDescriptor<D>) -> Self {
        Self {
            source,
            products: BTreeMap::new(),
        }
    }

    pub const fn source(&self) -> DecodedPcmDescriptor<D> {
        self.source
    }

    pub fn publish(
        &mut self,
        product: WaveformChunkSideProduct<D>,
    ) -> Result<(), StreamingWaveformError> {
        if product.source.pcm != self.source.id || product.source.geometry != self.source.geometry {
            return Err(StreamingWaveformError::SourceMismatch);
        }
        let expected_span = self
            .source
            .geometry
            .chunk_span(product.source.index, self.source.frame_count)
            .map_err(StreamingWaveformError::Streaming)?;
        let mut cursor = expected_span.start;
        for bin in product.bins.iter() {
            if bin.source.start != cursor
                || bin.source.end > expected_span.end
                || bin.channels.len() != usize::from(self.source.geometry.channels)
            {
                return Err(StreamingWaveformError::InvalidSideProduct);
            }
            let represented = bin.source.len();
            for envelope in bin.channels.iter().copied() {
                if u64::from(envelope.frames) != represented {
                    return Err(StreamingWaveformError::InvalidSideProduct);
                }
                Accumulator::from_streaming_envelope(envelope)?;
            }
            cursor = bin.source.end;
        }
        if cursor != expected_span.end {
            return Err(StreamingWaveformError::InvalidSideProduct);
        }
        self.products.insert(product.source.index, product);
        Ok(())
    }

    pub fn product_count(&self) -> usize {
        self.products.len()
    }

    /// Query exact min/max/RMS bins. `read_pcm` is invoked only for partial
    /// summary bins or summary gaps and must return complete finite frames for
    /// the requested half-open source interval.
    pub fn query_exact(
        &self,
        start_frame: u64,
        end_frame: u64,
        target_bins: usize,
        mut read_pcm: impl FnMut(u64, u64) -> Result<Vec<f32>, StreamingWaveformError>,
    ) -> Result<WaveformQuery, StreamingWaveformError> {
        if start_frame >= end_frame || end_frame > self.source.frame_count {
            return Err(StreamingWaveformError::RangeOutsideSource);
        }
        if target_bins == 0 {
            return Err(StreamingWaveformError::ZeroTargetBins);
        }
        let frame_count = end_frame - start_frame;
        let bin_count = target_bins.min(
            usize::try_from(frame_count).map_err(|_| StreamingWaveformError::ArithmeticOverflow)?,
        );
        let frames_per_bin = frame_count.div_ceil(bin_count as u64);
        let preferred_level = floor_log2(
            usize::try_from(frames_per_bin)
                .map_err(|_| StreamingWaveformError::ArithmeticOverflow)?,
        );
        let channels = usize::from(self.source.geometry.channels);
        let mut bins = Vec::with_capacity(bin_count);
        for bin_index in 0..bin_count {
            let bin_start = start_frame + frame_count * bin_index as u64 / bin_count as u64;
            let bin_end = start_frame + frame_count * (bin_index as u64 + 1) / bin_count as u64;
            let mut accumulators = vec![Accumulator::empty(); channels];
            let mut cursor = bin_start;
            let first_chunk = self.source.geometry.chunk_index(bin_start).0;
            let last_chunk = self
                .source
                .geometry
                .chunk_index(bin_end.saturating_sub(1))
                .0;
            for chunk_index in first_chunk..=last_chunk {
                let Some(product) = self.products.get(&PcmChunkIndex(chunk_index)) else {
                    continue;
                };
                for summary in product.bins.iter() {
                    if summary.source.end <= cursor || summary.source.start >= bin_end {
                        continue;
                    }
                    let overlap_start = summary.source.start.max(cursor);
                    if cursor < overlap_start {
                        extend_from_streamed_pcm(
                            &mut accumulators,
                            cursor,
                            overlap_start,
                            channels,
                            &mut read_pcm,
                        )?;
                    }
                    let overlap_end = summary.source.end.min(bin_end);
                    if overlap_start == summary.source.start && overlap_end == summary.source.end {
                        for (output, envelope) in accumulators
                            .iter_mut()
                            .zip(summary.channels.iter().copied())
                        {
                            output.extend(Accumulator::from_streaming_envelope(envelope)?);
                        }
                    } else {
                        extend_from_streamed_pcm(
                            &mut accumulators,
                            overlap_start,
                            overlap_end,
                            channels,
                            &mut read_pcm,
                        )?;
                    }
                    cursor = overlap_end;
                    if cursor == bin_end {
                        break;
                    }
                }
            }
            if cursor < bin_end {
                extend_from_streamed_pcm(
                    &mut accumulators,
                    cursor,
                    bin_end,
                    channels,
                    &mut read_pcm,
                )?;
            }
            bins.push(WaveformBin {
                start_frame: usize::try_from(bin_start)
                    .map_err(|_| StreamingWaveformError::ArithmeticOverflow)?,
                end_frame: usize::try_from(bin_end)
                    .map_err(|_| StreamingWaveformError::ArithmeticOverflow)?,
                channels: accumulators
                    .into_iter()
                    .map(Accumulator::envelope)
                    .collect(),
            });
        }
        Ok(WaveformQuery {
            start_frame: usize::try_from(start_frame)
                .map_err(|_| StreamingWaveformError::ArithmeticOverflow)?,
            end_frame: usize::try_from(end_frame)
                .map_err(|_| StreamingWaveformError::ArithmeticOverflow)?,
            preferred_level,
            bins,
        })
    }
}

fn extend_from_streamed_pcm(
    accumulators: &mut [Accumulator],
    start: u64,
    end: u64,
    channels: usize,
    read_pcm: &mut impl FnMut(u64, u64) -> Result<Vec<f32>, StreamingWaveformError>,
) -> Result<(), StreamingWaveformError> {
    if start >= end {
        return Ok(());
    }
    let pcm = read_pcm(start, end)?;
    let frames =
        usize::try_from(end - start).map_err(|_| StreamingWaveformError::ArithmeticOverflow)?;
    let expected = frames
        .checked_mul(channels)
        .ok_or(StreamingWaveformError::ArithmeticOverflow)?;
    if pcm.len() != expected {
        return Err(StreamingWaveformError::PcmSampleCount {
            expected,
            actual: pcm.len(),
        });
    }
    if let Some((sample_index, _)) = pcm
        .iter()
        .enumerate()
        .find(|(_, sample)| !sample.is_finite())
    {
        return Err(StreamingWaveformError::NonFinitePcm { sample_index });
    }
    for frame in pcm.chunks_exact(channels) {
        for (output, sample) in accumulators.iter_mut().zip(frame.iter().copied()) {
            output.extend(Accumulator::sample(sample));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamingWaveformError {
    SourceMismatch,
    ProjectedPcmRequired,
    InvalidSideProduct,
    RangeOutsideSource,
    ZeroTargetBins,
    PcmSampleCount { expected: usize, actual: usize },
    NonFinitePcm { sample_index: usize },
    Streaming(StreamingMediaError),
    ArithmeticOverflow,
}

impl fmt::Display for StreamingWaveformError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceMismatch => {
                formatter.write_str("waveform side product addresses different PCM")
            }
            Self::ProjectedPcmRequired => formatter
                .write_str("this channel projection requires projection-specific streamed PCM"),
            Self::InvalidSideProduct => {
                formatter.write_str("waveform side product is incomplete or invalid")
            }
            Self::RangeOutsideSource => {
                formatter.write_str("waveform query lies outside streamed PCM")
            }
            Self::ZeroTargetBins => formatter.write_str("waveform query has zero target bins"),
            Self::PcmSampleCount { expected, actual } => write!(
                formatter,
                "streamed waveform edge returned {actual} samples; expected {expected}"
            ),
            Self::NonFinitePcm { sample_index } => write!(
                formatter,
                "streamed waveform edge contains non-finite sample {sample_index}"
            ),
            Self::Streaming(error) => write!(formatter, "streaming media error: {error}"),
            Self::ArithmeticOverflow => formatter.write_str("streamed waveform range overflowed"),
        }
    }
}

impl Error for StreamingWaveformError {}

fn floor_log2(value: usize) -> usize {
    debug_assert!(value > 0);
    (usize::BITS - 1 - value.leading_zeros()) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::ContentId;
    use crate::streaming_media::{DecodedPcmId, PcmChunk, PcmChunkGeometry, PcmChunkIndex};

    fn mono(values: &[f32]) -> WaveformPyramid {
        WaveformPyramid::from_interleaved(values, 1)
    }

    #[test]
    fn range_query_is_exact_and_uses_even_boundaries() {
        let pyramid = mono(&[-1.0, 0.25, 0.5, -0.75, 1.0, 0.0, -0.5, 0.75]);
        let query = pyramid.query(1, 7, 3);

        assert_eq!(query.preferred_level, 1);
        assert_eq!(
            query
                .bins
                .iter()
                .map(|bin| (bin.start_frame, bin.end_frame))
                .collect::<Vec<_>>(),
            vec![(1, 3), (3, 5), (5, 7)]
        );
        assert_eq!(
            query.bins[0].channels[0],
            ChannelEnvelope {
                min: 0.25,
                max: 0.5,
                rms: (0.15625_f32).sqrt(),
                frames: 2
            }
        );
        assert_eq!(query.bins[1].channels[0].min, -0.75);
        assert_eq!(query.bins[1].channels[0].max, 1.0);
        assert_eq!(query.bins[2].channels[0].min, -0.5);
        assert_eq!(query.bins[2].channels[0].max, 0.0);
    }

    #[test]
    fn impulses_do_not_leak_across_visible_range_edges() {
        let mut values = vec![0.0; 65];
        values[0] = -1.0;
        values[32] = 1.0;
        values[64] = -0.75;
        let pyramid = mono(&values);

        let middle = pyramid.query(1, 64, 1);
        assert_eq!(middle.bins[0].channels[0].min, 0.0);
        assert_eq!(middle.bins[0].channels[0].max, 1.0);

        let before = pyramid.query(1, 32, 1);
        assert_eq!(before.bins[0].channels[0].min, 0.0);
        assert_eq!(before.bins[0].channels[0].max, 0.0);

        let after = pyramid.query(33, 64, 1);
        assert_eq!(after.bins[0].channels[0].min, 0.0);
        assert_eq!(after.bins[0].channels[0].max, 0.0);
    }

    #[test]
    fn summary_interiors_remain_exact_at_base_block_boundaries() {
        let mut values = vec![0.0; BASE_BLOCK_FRAMES * 4];
        values[BASE_BLOCK_FRAMES - 1] = -1.0;
        values[BASE_BLOCK_FRAMES] = 0.5;
        values[BASE_BLOCK_FRAMES * 3 - 1] = 0.75;
        values[BASE_BLOCK_FRAMES * 3] = -0.5;
        let pyramid = mono(&values);

        let interior = pyramid.query(BASE_BLOCK_FRAMES, BASE_BLOCK_FRAMES * 3, 1);
        assert_eq!(interior.bins[0].channels[0].min, 0.0);
        assert_eq!(interior.bins[0].channels[0].max, 0.75);
    }

    #[test]
    fn planar_and_interleaved_stereo_preserve_channel_identity() {
        let left = [-1.0, 0.0, 1.0, 0.0];
        let right = [0.5, -0.5, 0.25, -0.25];
        let planar = WaveformPyramid::from_channels(&[&left, &right]);
        let interleaved =
            WaveformPyramid::from_interleaved(&[-1.0, 0.5, 0.0, -0.5, 1.0, 0.25, 0.0, -0.25], 2);

        assert_eq!(planar.channel_count(), 2);
        assert_eq!(planar.query(0, 4, 1), interleaved.query(0, 4, 1));
        let bin = &planar.query(0, 4, 1).bins[0];
        assert_eq!(bin.channels[0].min, -1.0);
        assert_eq!(bin.channels[0].max, 1.0);
        assert_eq!(bin.channels[1].min, -0.5);
        assert_eq!(bin.channels[1].max, 0.5);
    }

    #[test]
    fn empty_and_clamped_queries_have_no_empty_output_bins() {
        let empty = WaveformPyramid::from_interleaved(&[], 2);
        assert_eq!(empty.frame_count(), 0);
        assert!(empty.query(0, 20, 32).bins.is_empty());

        let pyramid = mono(&[1.0, -1.0, 0.5]);
        assert!(pyramid.query(2, 1, 2).bins.is_empty());
        assert!(pyramid.query(0, 3, 0).bins.is_empty());
        let clamped = pyramid.query(1, 99, 99);
        assert_eq!(clamped.start_frame, 1);
        assert_eq!(clamped.end_frame, 3);
        assert_eq!(clamped.bins.len(), 2);
        assert!(clamped
            .bins
            .iter()
            .all(|bin| bin.start_frame < bin.end_frame));
    }

    #[test]
    fn construction_and_queries_are_deterministic() {
        let values = [f32::NAN, -0.4, 0.2, f32::INFINITY, -0.9, 0.9, 0.1];
        let first = mono(&values);
        let second = mono(&values);
        assert_eq!(
            first.query(0, values.len(), 3),
            second.query(0, values.len(), 3)
        );
        let envelope = &first.query(0, values.len(), 1).bins[0].channels[0];
        assert_eq!(envelope.min, -0.9);
        assert_eq!(envelope.max, 0.9);
    }

    #[test]
    fn canonical_pcm_can_be_shared_without_copying() {
        let pyramid = WaveformPyramid::from_interleaved(&[0.1, -0.1, 0.2, -0.2], 2);
        let shared = pyramid.shared_interleaved_pcm();
        assert_eq!(shared.as_ptr(), pyramid.interleaved_pcm().as_ptr());
        assert_eq!(&*shared, pyramid.interleaved_pcm());
    }

    #[test]
    fn storage_is_pcm_plus_flat_summary_levels_not_per_sample_accumulators() {
        let frame_count = BASE_BLOCK_FRAMES * 1025 + 7;
        let samples = (0..frame_count * 3)
            .map(|value| value as f32 / frame_count as f32)
            .collect::<Vec<_>>();
        let pyramid = WaveformPyramid::from_interleaved(&samples, 3);

        // This is the allocation-shape invariant: a level is one contiguous
        // row-major Vec rather than a Vec of per-frame/per-bin Vecs.
        for level in &pyramid.levels {
            let bin_count = pyramid.frame_count.div_ceil(level.block_frames);
            assert_eq!(level.values.len(), bin_count * pyramid.channel_count);
        }
        let storage = pyramid.storage();
        assert_eq!(storage.pcm_samples, frame_count * 3);
        assert!(storage.summary_accumulators < storage.pcm_samples / 100);
        assert_eq!(pyramid.level_count(), 12); // 1026 base blocks, then 513 .. 1
    }

    #[test]
    fn strict_streaming_build_quarantines_non_finite_pcm_while_legacy_is_auditable() {
        assert!(matches!(
            WaveformPyramid::try_from_finite_interleaved(&[0.25, f32::NAN], 1),
            Err(PyramidBuildError::NonFinitePcm { sample_index: 1 })
        ));
        let legacy = WaveformPyramid::from_interleaved(&[0.25, f32::NAN], 1);
        assert_eq!(legacy.sanitized_non_finite_samples(), 1);
        assert_eq!(legacy.interleaved_pcm(), &[0.25, 0.0]);
    }

    #[test]
    fn streamed_side_products_read_only_distinctive_boundary_pcm() {
        let descriptor = DecodedPcmDescriptor::new(
            DecodedPcmId(ContentId(71)),
            PcmChunkGeometry::new(48_000, 1, 4).unwrap(),
            8,
        )
        .unwrap();
        let mut index = StreamingWaveformIndex::new(descriptor);
        for chunk_index in 0..2 {
            let span = descriptor
                .geometry
                .chunk_span(PcmChunkIndex(chunk_index), descriptor.frame_count)
                .unwrap();
            let samples = (span.start..span.end)
                .map(|frame| frame as f32 * 10.0 + 0.5)
                .collect::<Vec<_>>();
            let chunk =
                PcmChunk::new(descriptor, PcmChunkIndex(chunk_index), samples.into(), 2).unwrap();
            index.publish(chunk.waveform).unwrap();
        }
        let mut edge_reads = Vec::new();
        let query = index
            .query_exact(1, 7, 2, |start, end| {
                edge_reads.push((start, end));
                Ok((start..end)
                    .map(|frame| frame as f32 * 10.0 + 0.5)
                    .collect())
            })
            .unwrap();
        assert_eq!(edge_reads, vec![(1, 2), (6, 7)]);
        assert_eq!(
            query
                .bins
                .iter()
                .map(|bin| (
                    bin.start_frame,
                    bin.end_frame,
                    bin.channels[0].min,
                    bin.channels[0].max
                ))
                .collect::<Vec<_>>(),
            vec![(1, 4, 10.5, 30.5), (4, 7, 40.5, 60.5)]
        );
    }
}
