//! Bounded, chunk-addressed project-rate media contracts.
//!
//! This module owns no decoder, filesystem, audio device, or hash function.
//! Encoded-source and decoded-PCM digests are supplied by Audec's canonical
//! digest boundary. Decoders publish finite, immutable project-rate chunks;
//! consumers request and lease only the chunks they need. Consequently none
//! of these types claim that a whole file is resident, that a codec supports
//! exact random access, or that a recording FIFO is connected to hardware.
//! Missing chunks and failed relinks stay explicit instead of becoming
//! zero-filled audio or silently accepted paths.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::hash::Hash;
use std::sync::Arc;

/// Trait boundary for the canonical digest layer. This module compares and
/// carries digests, but deliberately cannot construct one from media bytes.
pub trait MediaDigest: Copy + fmt::Debug + Eq + Hash + Ord {}

impl<T> MediaDigest for T where T: Copy + fmt::Debug + Eq + Hash + Ord {}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EncodedSourceId<D: MediaDigest>(pub D);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DecodedPcmId<D: MediaDigest>(pub D);

/// Immutable chunk geometry for canonical PCM at one project rate.
///
/// A geometry belongs to decoded PCM identity and is never selected from a
/// viewport. Power-of-two chunks make range division exact and let render,
/// decode, and waveform caches share boundaries.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PcmChunkGeometry {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub frames_per_chunk: u32,
}

impl PcmChunkGeometry {
    pub const DEFAULT_FRAMES_PER_CHUNK: u32 = 1 << 16;

    pub fn new(
        sample_rate_hz: u32,
        channels: u16,
        frames_per_chunk: u32,
    ) -> Result<Self, StreamingMediaError> {
        if sample_rate_hz == 0 {
            return Err(StreamingMediaError::ZeroSampleRate);
        }
        if channels == 0 {
            return Err(StreamingMediaError::ZeroChannels);
        }
        if frames_per_chunk == 0 || !frames_per_chunk.is_power_of_two() {
            return Err(StreamingMediaError::InvalidChunkFrames(frames_per_chunk));
        }
        Ok(Self {
            sample_rate_hz,
            channels,
            frames_per_chunk,
        })
    }

    pub fn project_default(
        sample_rate_hz: u32,
        channels: u16,
    ) -> Result<Self, StreamingMediaError> {
        Self::new(sample_rate_hz, channels, Self::DEFAULT_FRAMES_PER_CHUNK)
    }

    pub const fn chunk_index(self, frame: u64) -> PcmChunkIndex {
        PcmChunkIndex(frame / self.frames_per_chunk as u64)
    }

    pub fn chunk_span(
        self,
        index: PcmChunkIndex,
        total_frames: u64,
    ) -> Result<FrameSpan, StreamingMediaError> {
        let start = index
            .0
            .checked_mul(u64::from(self.frames_per_chunk))
            .ok_or(StreamingMediaError::ArithmeticOverflow)?;
        if start >= total_frames {
            return Err(StreamingMediaError::ChunkOutsidePcm {
                index,
                total_frames,
            });
        }
        let end = start
            .checked_add(u64::from(self.frames_per_chunk))
            .ok_or(StreamingMediaError::ArithmeticOverflow)?
            .min(total_frames);
        FrameSpan::new(start, end)
    }

    pub fn interleaved_samples(self, frames: u64) -> Result<usize, StreamingMediaError> {
        usize::try_from(
            frames
                .checked_mul(u64::from(self.channels))
                .ok_or(StreamingMediaError::ArithmeticOverflow)?,
        )
        .map_err(|_| StreamingMediaError::ArithmeticOverflow)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PcmChunkIndex(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FrameSpan {
    pub start: u64,
    pub end: u64,
}

impl FrameSpan {
    pub fn new(start: u64, end: u64) -> Result<Self, StreamingMediaError> {
        if start >= end {
            return Err(StreamingMediaError::InvalidFrameSpan { start, end });
        }
        Ok(Self { start, end })
    }

    pub const fn len(self) -> u64 {
        self.end - self.start
    }

    pub const fn contains(self, frame: u64) -> bool {
        self.start <= frame && frame < self.end
    }
}

/// Exact immutable facts for one canonical project-rate PCM stream.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DecodedPcmDescriptor<D: MediaDigest> {
    pub id: DecodedPcmId<D>,
    pub geometry: PcmChunkGeometry,
    pub frame_count: u64,
}

impl<D: MediaDigest> DecodedPcmDescriptor<D> {
    pub fn new(
        id: DecodedPcmId<D>,
        geometry: PcmChunkGeometry,
        frame_count: u64,
    ) -> Result<Self, StreamingMediaError> {
        if frame_count == 0 {
            return Err(StreamingMediaError::EmptyPcm);
        }
        Ok(Self {
            id,
            geometry,
            frame_count,
        })
    }

    pub fn chunk_key(self, index: PcmChunkIndex) -> Result<PcmChunkKey<D>, StreamingMediaError> {
        self.geometry.chunk_span(index, self.frame_count)?;
        Ok(PcmChunkKey {
            pcm: self.id,
            geometry: self.geometry,
            index,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PcmChunkKey<D: MediaDigest> {
    pub pcm: DecodedPcmId<D>,
    pub geometry: PcmChunkGeometry,
    pub index: PcmChunkIndex,
}

/// A source reference that creates no copied asset. Structural equality is
/// the slice identity: exact decoded PCM identity plus an exact half-open
/// source range.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct VirtualSliceRef<D: MediaDigest> {
    pub source: DecodedPcmDescriptor<D>,
    pub range: FrameSpan,
}

impl<D: MediaDigest> VirtualSliceRef<D> {
    pub fn new(
        source: DecodedPcmDescriptor<D>,
        range: FrameSpan,
    ) -> Result<Self, StreamingMediaError> {
        if range.end > source.frame_count {
            return Err(StreamingMediaError::SliceOutsidePcm {
                end: range.end,
                frame_count: source.frame_count,
            });
        }
        Ok(Self { source, range })
    }

    pub const fn frame_count(self) -> u64 {
        self.range.len()
    }

    pub fn covering_chunks(self) -> Vec<PcmChunkKey<D>> {
        let first = self.source.geometry.chunk_index(self.range.start).0;
        let last = self
            .source
            .geometry
            .chunk_index(self.range.end.saturating_sub(1))
            .0;
        (first..=last)
            .map(|index| PcmChunkKey {
                pcm: self.source.id,
                geometry: self.source.geometry,
                index: PcmChunkIndex(index),
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ChannelEnvelope {
    pub min: f32,
    pub max: f32,
    pub rms: f32,
    pub frames: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WaveformSummaryBin {
    pub source: FrameSpan,
    pub channels: Arc<[ChannelEnvelope]>,
}

/// Incremental waveform-pyramid leaves emitted beside each decoded chunk.
/// Higher levels can merge adjacent bins without reopening encoded media.
#[derive(Clone, Debug, PartialEq)]
pub struct WaveformChunkSideProduct<D: MediaDigest> {
    pub source: PcmChunkKey<D>,
    pub base_bin_frames: u32,
    pub bins: Arc<[WaveformSummaryBin]>,
}

impl<D: MediaDigest> WaveformChunkSideProduct<D> {
    pub fn from_finite_interleaved(
        source: PcmChunkKey<D>,
        span: FrameSpan,
        samples: &[f32],
        base_bin_frames: u32,
    ) -> Result<Self, StreamingMediaError> {
        if base_bin_frames == 0 || !base_bin_frames.is_power_of_two() {
            return Err(StreamingMediaError::InvalidWaveformBinFrames(
                base_bin_frames,
            ));
        }
        let channels = usize::from(source.geometry.channels);
        let expected = source.geometry.interleaved_samples(span.len())?;
        if samples.len() != expected {
            return Err(StreamingMediaError::ChunkSampleCount {
                expected,
                actual: samples.len(),
            });
        }
        if let Some((sample_index, _)) = samples
            .iter()
            .enumerate()
            .find(|(_, sample)| !sample.is_finite())
        {
            return Err(StreamingMediaError::NonFinitePcm { sample_index });
        }

        let mut bins = Vec::new();
        let frames =
            usize::try_from(span.len()).map_err(|_| StreamingMediaError::ArithmeticOverflow)?;
        let width = usize::try_from(base_bin_frames)
            .map_err(|_| StreamingMediaError::ArithmeticOverflow)?;
        for local_start in (0..frames).step_by(width) {
            let local_end = (local_start + width).min(frames);
            let mut envelopes = Vec::with_capacity(channels);
            for channel in 0..channels {
                let mut min = f32::INFINITY;
                let mut max = f32::NEG_INFINITY;
                let mut sum_squares = 0.0_f64;
                for frame in local_start..local_end {
                    let sample = samples[frame * channels + channel];
                    min = min.min(sample);
                    max = max.max(sample);
                    sum_squares += f64::from(sample) * f64::from(sample);
                }
                let represented = local_end - local_start;
                envelopes.push(ChannelEnvelope {
                    min,
                    max,
                    rms: (sum_squares / represented as f64).sqrt() as f32,
                    frames: u32::try_from(represented)
                        .map_err(|_| StreamingMediaError::ArithmeticOverflow)?,
                });
            }
            bins.push(WaveformSummaryBin {
                source: FrameSpan {
                    start: span.start + local_start as u64,
                    end: span.start + local_end as u64,
                },
                channels: envelopes.into(),
            });
        }
        Ok(Self {
            source,
            base_bin_frames,
            bins: bins.into(),
        })
    }

    pub fn estimated_bytes(&self) -> u64 {
        self.bins
            .iter()
            .map(|bin| {
                std::mem::size_of::<WaveformSummaryBin>() as u64
                    + (bin.channels.len() * std::mem::size_of::<ChannelEnvelope>()) as u64
            })
            .sum()
    }
}

/// One validated project-rate PCM chunk. A final chunk may be short; every
/// other chunk must have exactly the canonical geometry's width.
#[derive(Clone, Debug)]
pub struct PcmChunk<D: MediaDigest> {
    pub key: PcmChunkKey<D>,
    pub span: FrameSpan,
    pub interleaved: Arc<[f32]>,
    pub waveform: WaveformChunkSideProduct<D>,
}

impl<D: MediaDigest> PcmChunk<D> {
    pub fn new(
        source: DecodedPcmDescriptor<D>,
        index: PcmChunkIndex,
        interleaved: Arc<[f32]>,
        waveform_bin_frames: u32,
    ) -> Result<Self, StreamingMediaError> {
        let key = source.chunk_key(index)?;
        let span = source.geometry.chunk_span(index, source.frame_count)?;
        let expected = source.geometry.interleaved_samples(span.len())?;
        if interleaved.len() != expected {
            return Err(StreamingMediaError::ChunkSampleCount {
                expected,
                actual: interleaved.len(),
            });
        }
        if let Some((sample_index, _)) = interleaved
            .iter()
            .enumerate()
            .find(|(_, sample)| !sample.is_finite())
        {
            return Err(StreamingMediaError::NonFinitePcm { sample_index });
        }
        let waveform = WaveformChunkSideProduct::from_finite_interleaved(
            key,
            span,
            &interleaved,
            waveform_bin_frames,
        )?;
        Ok(Self {
            key,
            span,
            interleaved,
            waveform,
        })
    }

    pub fn resident_bytes(&self) -> u64 {
        (self.interleaved.len() * std::mem::size_of::<f32>()) as u64
            + self.waveform.estimated_bytes()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestPriority {
    Playback,
    ActiveLoop,
    Visible,
    Lookahead,
    Background,
}

impl RequestPriority {
    const fn rank(self) -> u8 {
        match self {
            Self::Playback => 0,
            Self::ActiveLoop => 1,
            Self::Visible => 2,
            Self::Lookahead => 3,
            Self::Background => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackDirection {
    Forward,
    Reverse,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrefetchPolicy {
    pub lookahead_chunks: u16,
    pub lookbehind_chunks: u16,
    pub max_queued: usize,
    pub max_in_flight: usize,
}

impl PrefetchPolicy {
    pub fn validate(self) -> Result<Self, StreamingMediaError> {
        if self.max_queued == 0 || self.max_in_flight == 0 || self.max_in_flight > self.max_queued {
            return Err(StreamingMediaError::InvalidPrefetchPolicy);
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeRequest<D: MediaDigest> {
    pub key: PcmChunkKey<D>,
    pub priority: RequestPriority,
    pub distance_chunks: u32,
    /// Revision of the transport/project demand. Superseded epochs can be
    /// cancelled before publication without making chunks mutable.
    pub demand_epoch: u64,
}

impl<D: MediaDigest> DecodeRequest<D> {
    fn ordering_key(self) -> (u8, u32, PcmChunkKey<D>) {
        (self.priority.rank(), self.distance_chunks, self.key)
    }
}

/// Deterministic lookahead around the playhead. The current chunk is first;
/// motion-direction lookahead precedes bounded opposite-direction context.
pub fn plan_prefetch<D: MediaDigest>(
    source: DecodedPcmDescriptor<D>,
    playhead_frame: u64,
    direction: PlaybackDirection,
    policy: PrefetchPolicy,
    demand_epoch: u64,
) -> Result<Vec<DecodeRequest<D>>, StreamingMediaError> {
    let policy = policy.validate()?;
    if playhead_frame >= source.frame_count {
        return Err(StreamingMediaError::PlayheadOutsidePcm {
            frame: playhead_frame,
            frame_count: source.frame_count,
        });
    }
    let current = source.geometry.chunk_index(playhead_frame).0;
    let last = source.geometry.chunk_index(source.frame_count - 1).0;
    let mut requests = Vec::new();
    let mut push = |index: u64, priority: RequestPriority, distance: u32| {
        if requests.len() < policy.max_queued && index <= last {
            requests.push(DecodeRequest {
                key: PcmChunkKey {
                    pcm: source.id,
                    geometry: source.geometry,
                    index: PcmChunkIndex(index),
                },
                priority,
                distance_chunks: distance,
                demand_epoch,
            });
        }
    };
    push(current, RequestPriority::Playback, 0);
    for distance in 1..=u64::from(policy.lookahead_chunks) {
        let candidate = match direction {
            PlaybackDirection::Forward => {
                current.checked_add(distance).filter(|value| *value <= last)
            }
            PlaybackDirection::Reverse => current.checked_sub(distance),
        };
        if let Some(index) = candidate {
            push(index, RequestPriority::Lookahead, distance as u32);
        }
    }
    for distance in 1..=u64::from(policy.lookbehind_chunks) {
        let candidate = match direction {
            PlaybackDirection::Forward => current.checked_sub(distance),
            PlaybackDirection::Reverse => {
                current.checked_add(distance).filter(|value| *value <= last)
            }
        };
        if let Some(index) = candidate {
            push(index, RequestPriority::Background, distance as u32);
        }
    }
    Ok(requests)
}

/// Explicit bound for a scrollable viewport demand. Visible chunks are never
/// silently dropped; an excessively broad request is refused so callers can
/// switch to summary products or process the range in bounded batches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ViewportChunkPolicy {
    pub context_before_chunks: u16,
    pub context_after_chunks: u16,
    pub maximum_visible_chunks: usize,
    pub maximum_total_chunks: usize,
}

impl ViewportChunkPolicy {
    pub fn validate(self) -> Result<Self, StreamingMediaError> {
        if self.maximum_visible_chunks == 0
            || self.maximum_total_chunks < self.maximum_visible_chunks
        {
            return Err(StreamingMediaError::InvalidViewportChunkPolicy);
        }
        Ok(self)
    }
}

/// Plan exact visible chunks plus bounded scroll context. Context after the
/// viewport is lookahead; context before it is lower-priority background.
pub fn plan_viewport_chunks<D: MediaDigest>(
    source: DecodedPcmDescriptor<D>,
    visible: FrameSpan,
    policy: ViewportChunkPolicy,
    demand_epoch: u64,
) -> Result<Vec<DecodeRequest<D>>, StreamingMediaError> {
    let policy = policy.validate()?;
    if visible.end > source.frame_count {
        return Err(StreamingMediaError::ViewportOutsidePcm {
            end: visible.end,
            frame_count: source.frame_count,
        });
    }
    let first = source.geometry.chunk_index(visible.start).0;
    let last = source.geometry.chunk_index(visible.end - 1).0;
    let visible_count =
        usize::try_from(last - first + 1).map_err(|_| StreamingMediaError::ArithmeticOverflow)?;
    if visible_count > policy.maximum_visible_chunks || visible_count > policy.maximum_total_chunks
    {
        return Err(StreamingMediaError::ViewportDemandExceedsLimit {
            required_chunks: visible_count,
            maximum_chunks: policy
                .maximum_visible_chunks
                .min(policy.maximum_total_chunks),
        });
    }
    let mut requests = Vec::with_capacity(
        policy.maximum_total_chunks.min(
            visible_count
                .saturating_add(usize::from(policy.context_before_chunks))
                .saturating_add(usize::from(policy.context_after_chunks)),
        ),
    );
    for index in first..=last {
        requests.push(DecodeRequest {
            key: source.chunk_key(PcmChunkIndex(index))?,
            priority: RequestPriority::Visible,
            distance_chunks: 0,
            demand_epoch,
        });
    }
    for distance in 1..=u64::from(policy.context_after_chunks) {
        if requests.len() == policy.maximum_total_chunks {
            break;
        }
        let Some(index) = last.checked_add(distance) else {
            break;
        };
        if source
            .geometry
            .chunk_span(PcmChunkIndex(index), source.frame_count)
            .is_err()
        {
            break;
        }
        requests.push(DecodeRequest {
            key: source.chunk_key(PcmChunkIndex(index))?,
            priority: RequestPriority::Lookahead,
            distance_chunks: distance as u32,
            demand_epoch,
        });
    }
    for distance in 1..=u64::from(policy.context_before_chunks) {
        if requests.len() == policy.maximum_total_chunks {
            break;
        }
        let Some(index) = first.checked_sub(distance) else {
            break;
        };
        requests.push(DecodeRequest {
            key: source.chunk_key(PcmChunkIndex(index))?,
            priority: RequestPriority::Background,
            distance_chunks: distance as u32,
            demand_epoch,
        });
    }
    requests.sort_by_key(|request| request.ordering_key());
    Ok(requests)
}

/// Bounded control-thread queue. It deduplicates chunk requests, upgrades an
/// existing request when urgency rises, and deterministically discards the
/// least valuable pending work when capacity is exhausted.
#[derive(Clone, Debug)]
pub struct DecodeRequestQueue<D: MediaDigest> {
    policy: PrefetchPolicy,
    queued: BTreeMap<PcmChunkKey<D>, DecodeRequest<D>>,
    in_flight: BTreeMap<PcmChunkKey<D>, DecodeRequest<D>>,
}

impl<D: MediaDigest> DecodeRequestQueue<D> {
    pub fn new(policy: PrefetchPolicy) -> Result<Self, StreamingMediaError> {
        Ok(Self {
            policy: policy.validate()?,
            queued: BTreeMap::new(),
            in_flight: BTreeMap::new(),
        })
    }

    pub fn submit(&mut self, request: DecodeRequest<D>) -> bool {
        if self.in_flight.contains_key(&request.key) {
            return false;
        }
        if let Some(existing) = self.queued.get_mut(&request.key) {
            if request.ordering_key() < existing.ordering_key() {
                *existing = request;
                return true;
            }
            return false;
        }
        if self.queued.len() < self.policy.max_queued {
            self.queued.insert(request.key, request);
            return true;
        }
        let worst = self
            .queued
            .values()
            .copied()
            .max_by_key(|candidate| candidate.ordering_key());
        if let Some(worst) = worst {
            if request.ordering_key() < worst.ordering_key() {
                self.queued.remove(&worst.key);
                self.queued.insert(request.key, request);
                return true;
            }
        }
        false
    }

    pub fn submit_all(&mut self, requests: impl IntoIterator<Item = DecodeRequest<D>>) {
        for request in requests {
            self.submit(request);
        }
    }

    pub fn start_next(&mut self) -> Option<DecodeRequest<D>> {
        if self.in_flight.len() >= self.policy.max_in_flight {
            return None;
        }
        let next = self
            .queued
            .values()
            .copied()
            .min_by_key(|request| request.ordering_key())?;
        self.queued.remove(&next.key);
        self.in_flight.insert(next.key, next);
        Some(next)
    }

    pub fn complete(&mut self, key: PcmChunkKey<D>) -> bool {
        self.in_flight.remove(&key).is_some()
    }

    pub fn cancel_before_epoch(&mut self, minimum_epoch: u64) -> usize {
        let old_len = self.queued.len();
        self.queued
            .retain(|_, request| request.demand_epoch >= minimum_epoch);
        old_len - self.queued.len()
    }

    pub fn queued_len(&self) -> usize {
        self.queued.len()
    }

    pub fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheBudgets {
    pub memory_bytes: u64,
    pub disk_bytes: u64,
}

impl CacheBudgets {
    pub fn validate(self) -> Result<Self, StreamingMediaError> {
        if self.memory_bytes == 0 || self.disk_bytes == 0 {
            return Err(StreamingMediaError::ZeroCacheBudget);
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheAccounting {
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub memory_entries: usize,
    pub disk_entries: usize,
    pub active_leases: usize,
    pub memory_evictions: u64,
    pub disk_evictions: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiskChunkRecord<D: MediaDigest> {
    pub key: PcmChunkKey<D>,
    pub encoded_bytes: u64,
    /// Opaque location within an Audec-managed cache. It is not source truth.
    pub cache_locator: String,
}

#[derive(Clone, Debug)]
struct ResidentEntry<D: MediaDigest> {
    chunk: Arc<PcmChunk<D>>,
    bytes: u64,
    last_touch: u64,
}

#[derive(Clone, Debug)]
struct DiskEntry<D: MediaDigest> {
    record: DiskChunkRecord<D>,
    last_touch: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ChunkLeaseId(pub u64);

#[derive(Clone, Debug)]
pub struct ChunkLease<D: MediaDigest> {
    pub id: ChunkLeaseId,
    pub key: PcmChunkKey<D>,
    pub chunk: Arc<PcmChunk<D>>,
}

/// Deterministic two-tier accounting and an in-memory reference
/// implementation. The disk tier records durable cache extents supplied by a
/// filesystem adapter; it does not open or delete paths itself.
#[derive(Clone, Debug)]
pub struct BoundedMediaStore<D: MediaDigest> {
    budgets: CacheBudgets,
    residents: BTreeMap<PcmChunkKey<D>, ResidentEntry<D>>,
    disk: BTreeMap<PcmChunkKey<D>, DiskEntry<D>>,
    leases: BTreeMap<ChunkLeaseId, PcmChunkKey<D>>,
    pin_counts: BTreeMap<PcmChunkKey<D>, u32>,
    clock: u64,
    next_lease: u64,
    memory_bytes: u64,
    disk_bytes: u64,
    memory_evictions: u64,
    disk_evictions: u64,
}

impl<D: MediaDigest> BoundedMediaStore<D> {
    pub fn new(budgets: CacheBudgets) -> Result<Self, StreamingMediaError> {
        Ok(Self {
            budgets: budgets.validate()?,
            residents: BTreeMap::new(),
            disk: BTreeMap::new(),
            leases: BTreeMap::new(),
            pin_counts: BTreeMap::new(),
            clock: 0,
            next_lease: 1,
            memory_bytes: 0,
            disk_bytes: 0,
            memory_evictions: 0,
            disk_evictions: 0,
        })
    }

    fn tick(&mut self) -> u64 {
        self.clock = self.clock.saturating_add(1);
        self.clock
    }

    fn is_pinned(&self, key: &PcmChunkKey<D>) -> bool {
        self.pin_counts.get(key).copied().unwrap_or(0) > 0
    }

    pub fn publish_resident(&mut self, chunk: PcmChunk<D>) -> Result<(), StreamingMediaError> {
        let key = chunk.key;
        let bytes = chunk.resident_bytes();
        if bytes > self.budgets.memory_bytes {
            return Err(StreamingMediaError::EntryExceedsBudget {
                tier: CacheTier::Memory,
                bytes,
                budget: self.budgets.memory_bytes,
            });
        }
        let replaced = self
            .residents
            .get(&key)
            .map(|entry| entry.bytes)
            .unwrap_or(0);
        let required = self
            .memory_bytes
            .saturating_sub(replaced)
            .saturating_add(bytes);
        self.prepare_memory_capacity(required, Some(key))?;
        if let Some(previous) = self.residents.remove(&key) {
            self.memory_bytes -= previous.bytes;
        }
        let last_touch = self.tick();
        self.residents.insert(
            key,
            ResidentEntry {
                chunk: Arc::new(chunk),
                bytes,
                last_touch,
            },
        );
        self.memory_bytes += bytes;
        Ok(())
    }

    pub fn publish_disk_record(
        &mut self,
        record: DiskChunkRecord<D>,
    ) -> Result<(), StreamingMediaError> {
        if record.encoded_bytes == 0 {
            return Err(StreamingMediaError::EmptyDiskRecord);
        }
        if record.encoded_bytes > self.budgets.disk_bytes {
            return Err(StreamingMediaError::EntryExceedsBudget {
                tier: CacheTier::Disk,
                bytes: record.encoded_bytes,
                budget: self.budgets.disk_bytes,
            });
        }
        let replaced = self
            .disk
            .get(&record.key)
            .map(|entry| entry.record.encoded_bytes)
            .unwrap_or(0);
        let required = self
            .disk_bytes
            .saturating_sub(replaced)
            .saturating_add(record.encoded_bytes);
        self.prepare_disk_capacity(required, Some(record.key))?;
        if let Some(previous) = self.disk.remove(&record.key) {
            self.disk_bytes -= previous.record.encoded_bytes;
        }
        let last_touch = self.tick();
        self.disk_bytes += record.encoded_bytes;
        self.disk
            .insert(record.key, DiskEntry { record, last_touch });
        Ok(())
    }

    fn prepare_memory_capacity(
        &mut self,
        required: u64,
        replacing: Option<PcmChunkKey<D>>,
    ) -> Result<(), StreamingMediaError> {
        let need_free = required.saturating_sub(self.budgets.memory_bytes);
        if need_free == 0 {
            return Ok(());
        }
        let mut candidates: Vec<_> = self
            .residents
            .iter()
            .filter(|(key, _)| Some(**key) != replacing && !self.is_pinned(key))
            .map(|(key, entry)| (entry.last_touch, *key, entry.bytes))
            .collect();
        candidates.sort_by_key(|(touch, key, _)| (*touch, *key));
        let mut selected = Vec::new();
        let mut selected_bytes = 0_u64;
        for (_, key, bytes) in candidates {
            selected.push((key, bytes));
            selected_bytes = selected_bytes.saturating_add(bytes);
            if selected_bytes >= need_free {
                break;
            }
        }
        if selected_bytes < need_free {
            return Err(StreamingMediaError::AllEvictionCandidatesPinned(
                CacheTier::Memory,
            ));
        }
        for (key, bytes) in selected {
            self.residents.remove(&key);
            self.memory_bytes -= bytes;
            self.memory_evictions = self.memory_evictions.saturating_add(1);
        }
        Ok(())
    }

    fn prepare_disk_capacity(
        &mut self,
        required: u64,
        replacing: Option<PcmChunkKey<D>>,
    ) -> Result<(), StreamingMediaError> {
        let need_free = required.saturating_sub(self.budgets.disk_bytes);
        if need_free == 0 {
            return Ok(());
        }
        let mut candidates: Vec<_> = self
            .disk
            .iter()
            .filter(|(key, _)| Some(**key) != replacing && !self.is_pinned(key))
            .map(|(key, entry)| (entry.last_touch, *key, entry.record.encoded_bytes))
            .collect();
        candidates.sort_by_key(|(touch, key, _)| (*touch, *key));
        let mut selected = Vec::new();
        let mut selected_bytes = 0_u64;
        for (_, key, bytes) in candidates {
            selected.push((key, bytes));
            selected_bytes = selected_bytes.saturating_add(bytes);
            if selected_bytes >= need_free {
                break;
            }
        }
        if selected_bytes < need_free {
            return Err(StreamingMediaError::AllEvictionCandidatesPinned(
                CacheTier::Disk,
            ));
        }
        for (key, bytes) in selected {
            self.disk.remove(&key);
            self.disk_bytes -= bytes;
            self.disk_evictions = self.disk_evictions.saturating_add(1);
        }
        Ok(())
    }

    pub fn acquire(&mut self, key: PcmChunkKey<D>) -> Result<ChunkLease<D>, StreamingMediaError> {
        let last_touch = self.tick();
        let Some(entry) = self.residents.get_mut(&key) else {
            return Err(StreamingMediaError::ChunkUnavailable);
        };
        entry.last_touch = last_touch;
        let id = ChunkLeaseId(self.next_lease);
        self.next_lease = self
            .next_lease
            .checked_add(1)
            .ok_or(StreamingMediaError::ArithmeticOverflow)?;
        self.leases.insert(id, key);
        *self.pin_counts.entry(key).or_default() += 1;
        Ok(ChunkLease {
            id,
            key,
            chunk: Arc::clone(&entry.chunk),
        })
    }

    pub fn release(&mut self, lease: ChunkLeaseId) -> Result<(), StreamingMediaError> {
        let key = self
            .leases
            .remove(&lease)
            .ok_or(StreamingMediaError::UnknownLease(lease))?;
        let count = self
            .pin_counts
            .get_mut(&key)
            .expect("every lease contributes one pin");
        *count -= 1;
        if *count == 0 {
            self.pin_counts.remove(&key);
        }
        Ok(())
    }

    pub fn contains_resident(&self, key: PcmChunkKey<D>) -> bool {
        self.residents.contains_key(&key)
    }

    pub fn contains_disk(&self, key: PcmChunkKey<D>) -> bool {
        self.disk.contains_key(&key)
    }

    pub fn touch_disk(&mut self, key: PcmChunkKey<D>) -> bool {
        let touch = self.tick();
        if let Some(entry) = self.disk.get_mut(&key) {
            entry.last_touch = touch;
            true
        } else {
            false
        }
    }

    /// Exact cross-chunk virtual-slice read. Missing chunks are an error; a
    /// realtime adapter may convert that error to observable starvation, but
    /// the store itself never pretends silence came from the source.
    pub fn read_slice(
        &mut self,
        slice: VirtualSliceRef<D>,
        relative_start: u64,
        frame_count: u64,
    ) -> Result<Vec<f32>, StreamingMediaError> {
        let relative_end = relative_start
            .checked_add(frame_count)
            .ok_or(StreamingMediaError::ArithmeticOverflow)?;
        if relative_end > slice.frame_count() {
            return Err(StreamingMediaError::SliceReadOutsideRange {
                end: relative_end,
                frame_count: slice.frame_count(),
            });
        }
        let channels = usize::from(slice.source.geometry.channels);
        let capacity = slice.source.geometry.interleaved_samples(frame_count)?;
        let mut output = Vec::with_capacity(capacity);
        let mut cursor = slice.range.start + relative_start;
        let end = cursor + frame_count;
        while cursor < end {
            let index = slice.source.geometry.chunk_index(cursor);
            let key = slice.source.chunk_key(index)?;
            let touch = self.tick();
            let entry = self
                .residents
                .get_mut(&key)
                .ok_or(StreamingMediaError::MissingChunk(key.index))?;
            entry.last_touch = touch;
            let copy_end = end.min(entry.chunk.span.end);
            let local_start = usize::try_from(cursor - entry.chunk.span.start)
                .map_err(|_| StreamingMediaError::ArithmeticOverflow)?;
            let local_end = usize::try_from(copy_end - entry.chunk.span.start)
                .map_err(|_| StreamingMediaError::ArithmeticOverflow)?;
            output.extend_from_slice(
                &entry.chunk.interleaved[local_start * channels..local_end * channels],
            );
            cursor = copy_end;
        }
        Ok(output)
    }

    pub fn accounting(&self) -> CacheAccounting {
        CacheAccounting {
            memory_bytes: self.memory_bytes,
            disk_bytes: self.disk_bytes,
            memory_entries: self.residents.len(),
            disk_entries: self.disk.len(),
            active_leases: self.leases.len(),
            memory_evictions: self.memory_evictions,
            disk_evictions: self.disk_evictions,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheTier {
    Memory,
    Disk,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaRoute {
    /// Displayable route selected by the resolver. Identity remains digest-
    /// based; two paths are never considered the same source by name alone.
    pub locator: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MediaAvailability<D: MediaDigest> {
    Unresolved,
    Available {
        route: MediaRoute,
    },
    Missing {
        attempted: Vec<MediaRoute>,
    },
    Offline {
        reason: String,
    },
    Corrupt {
        route: MediaRoute,
        detail: String,
    },
    RelinkCandidate {
        route: MediaRoute,
        observed: EncodedSourceId<D>,
    },
    IdentityMismatch {
        route: MediaRoute,
        expected: EncodedSourceId<D>,
        observed: EncodedSourceId<D>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaAvailabilityRecord<D: MediaDigest> {
    pub expected_source: EncodedSourceId<D>,
    pub decoded_pcm: Option<DecodedPcmDescriptor<D>>,
    pub state: MediaAvailability<D>,
}

impl<D: MediaDigest> MediaAvailabilityRecord<D> {
    pub fn new(expected_source: EncodedSourceId<D>) -> Self {
        Self {
            expected_source,
            decoded_pcm: None,
            state: MediaAvailability::Unresolved,
        }
    }

    pub fn observe_relink(&mut self, route: MediaRoute, observed: EncodedSourceId<D>) {
        self.state = if observed == self.expected_source {
            MediaAvailability::RelinkCandidate { route, observed }
        } else {
            MediaAvailability::IdentityMismatch {
                route,
                expected: self.expected_source,
                observed,
            }
        };
    }

    /// Accept only the currently verified candidate. Path discovery alone is
    /// never enough to mutate source resolution.
    pub fn accept_verified_relink(&mut self) -> Result<(), StreamingMediaError> {
        let MediaAvailability::RelinkCandidate { route, observed } = &self.state else {
            return Err(StreamingMediaError::RelinkNotVerified);
        };
        if *observed != self.expected_source {
            return Err(StreamingMediaError::RelinkNotVerified);
        }
        self.state = MediaAvailability::Available {
            route: route.clone(),
        };
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RecordingTakeId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordingFifoContract {
    pub capacity_blocks: u16,
    pub frames_per_block: u32,
    pub channels: u16,
}

impl RecordingFifoContract {
    pub fn validate(self) -> Result<Self, StreamingMediaError> {
        if self.capacity_blocks == 0 || self.frames_per_block == 0 || self.channels == 0 {
            return Err(StreamingMediaError::InvalidRecordingFifo);
        }
        Ok(self)
    }
}

/// Preallocated capture block transferred from a FIFO to the writer worker.
/// The protocol validates its contents but does not allocate it in a callback.
#[derive(Clone, Debug)]
pub struct CaptureBlock {
    pub sequence: u64,
    pub first_project_frame: i64,
    pub frames: u32,
    pub interleaved: Arc<[f32]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingJournalHeader<D: MediaDigest> {
    pub take: RecordingTakeId,
    pub project: D,
    pub track_key: u64,
    pub start_project_frame: i64,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub fifo: RecordingFifoContract,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingJournalEntry<D: MediaDigest> {
    pub sequence: u64,
    pub first_project_frame: i64,
    pub frames: u32,
    pub durable_byte_end: u64,
    /// Digest of canonical bytes actually acknowledged durable by the writer.
    pub payload_digest: D,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingWriterState<D: MediaDigest> {
    Capturing,
    Finalizing,
    Finalized {
        encoded_source: EncodedSourceId<D>,
        decoded_pcm: DecodedPcmId<D>,
    },
    Aborted,
}

/// Control-side journal state machine for FIFO -> writer communication.
/// Data acknowledgements become recoverable journal entries strictly in
/// sequence. A filesystem adapter must make data durable before calling
/// `acknowledge_durable`; final identities are supplied after flush/fsync.
#[derive(Clone, Debug)]
pub struct RecordingJournalWriter<D: MediaDigest> {
    pub header: RecordingJournalHeader<D>,
    state: RecordingWriterState<D>,
    next_sequence: u64,
    frames_acknowledged: u64,
    pending: BTreeMap<u64, CaptureBlock>,
    journal: Vec<RecordingJournalEntry<D>>,
}

impl<D: MediaDigest> RecordingJournalWriter<D> {
    pub fn new(header: RecordingJournalHeader<D>) -> Result<Self, StreamingMediaError> {
        header.fifo.validate()?;
        if header.sample_rate_hz == 0
            || header.channels == 0
            || header.channels != header.fifo.channels
        {
            return Err(StreamingMediaError::InvalidRecordingHeader);
        }
        Ok(Self {
            header,
            state: RecordingWriterState::Capturing,
            next_sequence: 0,
            frames_acknowledged: 0,
            pending: BTreeMap::new(),
            journal: Vec::new(),
        })
    }

    pub fn enqueue_from_fifo(&mut self, block: CaptureBlock) -> Result<(), StreamingMediaError> {
        if self.state != RecordingWriterState::Capturing {
            return Err(StreamingMediaError::RecordingNotCapturing);
        }
        if self.pending.len() >= usize::from(self.header.fifo.capacity_blocks) {
            return Err(StreamingMediaError::RecordingFifoFull);
        }
        if block.sequence != self.next_sequence + self.pending.len() as u64 {
            return Err(StreamingMediaError::UnexpectedRecordingSequence {
                expected: self.next_sequence + self.pending.len() as u64,
                actual: block.sequence,
            });
        }
        if block.frames == 0 || block.frames > self.header.fifo.frames_per_block {
            return Err(StreamingMediaError::InvalidCaptureBlockFrames(block.frames));
        }
        let expected_samples = usize::try_from(block.frames)
            .ok()
            .and_then(|frames| frames.checked_mul(usize::from(self.header.channels)))
            .ok_or(StreamingMediaError::ArithmeticOverflow)?;
        if block.interleaved.len() != expected_samples {
            return Err(StreamingMediaError::ChunkSampleCount {
                expected: expected_samples,
                actual: block.interleaved.len(),
            });
        }
        if let Some((sample_index, _)) = block
            .interleaved
            .iter()
            .enumerate()
            .find(|(_, sample)| !sample.is_finite())
        {
            return Err(StreamingMediaError::NonFinitePcm { sample_index });
        }
        let expected_frame = self
            .header
            .start_project_frame
            .checked_add(
                i64::try_from(self.frames_acknowledged)
                    .map_err(|_| StreamingMediaError::ArithmeticOverflow)?,
            )
            .and_then(|base| {
                self.pending.values().try_fold(base, |frame, pending| {
                    frame.checked_add(i64::from(pending.frames))
                })
            })
            .ok_or(StreamingMediaError::ArithmeticOverflow)?;
        if block.first_project_frame != expected_frame {
            return Err(StreamingMediaError::DiscontinuousCapture {
                expected: expected_frame,
                actual: block.first_project_frame,
            });
        }
        self.pending.insert(block.sequence, block);
        Ok(())
    }

    pub fn next_write(&self) -> Option<&CaptureBlock> {
        self.pending.get(&self.next_sequence)
    }

    pub fn acknowledge_durable(
        &mut self,
        sequence: u64,
        durable_byte_end: u64,
        payload_digest: D,
    ) -> Result<(), StreamingMediaError> {
        if sequence != self.next_sequence {
            return Err(StreamingMediaError::UnexpectedRecordingSequence {
                expected: self.next_sequence,
                actual: sequence,
            });
        }
        if !self.pending.contains_key(&sequence) {
            return Err(StreamingMediaError::RecordingBlockNotPending(sequence));
        }
        if let Some(previous) = self.journal.last() {
            if durable_byte_end <= previous.durable_byte_end {
                return Err(StreamingMediaError::NonMonotonicDurableOffset);
            }
        } else if durable_byte_end == 0 {
            return Err(StreamingMediaError::NonMonotonicDurableOffset);
        }
        let block = self
            .pending
            .remove(&sequence)
            .expect("the pending block was validated above");
        self.frames_acknowledged = self
            .frames_acknowledged
            .checked_add(u64::from(block.frames))
            .ok_or(StreamingMediaError::ArithmeticOverflow)?;
        self.journal.push(RecordingJournalEntry {
            sequence,
            first_project_frame: block.first_project_frame,
            frames: block.frames,
            durable_byte_end,
            payload_digest,
        });
        self.next_sequence += 1;
        Ok(())
    }

    pub fn request_finalize(&mut self) -> Result<(), StreamingMediaError> {
        if self.state != RecordingWriterState::Capturing {
            return Err(StreamingMediaError::RecordingNotCapturing);
        }
        if !self.pending.is_empty() {
            return Err(StreamingMediaError::RecordingWritesPending);
        }
        self.state = RecordingWriterState::Finalizing;
        Ok(())
    }

    pub fn acknowledge_finalized(
        &mut self,
        encoded_source: EncodedSourceId<D>,
        decoded_pcm: DecodedPcmId<D>,
    ) -> Result<(), StreamingMediaError> {
        if self.state != RecordingWriterState::Finalizing {
            return Err(StreamingMediaError::RecordingNotFinalizing);
        }
        self.state = RecordingWriterState::Finalized {
            encoded_source,
            decoded_pcm,
        };
        Ok(())
    }

    pub fn abort(&mut self) {
        self.state = RecordingWriterState::Aborted;
        self.pending.clear();
    }

    pub const fn state(&self) -> RecordingWriterState<D> {
        self.state
    }

    pub const fn recoverable_frames(&self) -> u64 {
        self.frames_acknowledged
    }

    pub fn journal(&self) -> &[RecordingJournalEntry<D>] {
        &self.journal
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamingMediaError {
    ZeroSampleRate,
    ZeroChannels,
    InvalidChunkFrames(u32),
    InvalidWaveformBinFrames(u32),
    EmptyPcm,
    InvalidFrameSpan {
        start: u64,
        end: u64,
    },
    ChunkOutsidePcm {
        index: PcmChunkIndex,
        total_frames: u64,
    },
    SliceOutsidePcm {
        end: u64,
        frame_count: u64,
    },
    SliceReadOutsideRange {
        end: u64,
        frame_count: u64,
    },
    PlayheadOutsidePcm {
        frame: u64,
        frame_count: u64,
    },
    ChunkSampleCount {
        expected: usize,
        actual: usize,
    },
    NonFinitePcm {
        sample_index: usize,
    },
    MissingChunk(PcmChunkIndex),
    ChunkUnavailable,
    InvalidPrefetchPolicy,
    InvalidViewportChunkPolicy,
    InvalidSnapshotChunkLimit,
    ViewportOutsidePcm {
        end: u64,
        frame_count: u64,
    },
    ViewportDemandExceedsLimit {
        required_chunks: usize,
        maximum_chunks: usize,
    },
    ZeroCacheBudget,
    EntryExceedsBudget {
        tier: CacheTier,
        bytes: u64,
        budget: u64,
    },
    AllEvictionCandidatesPinned(CacheTier),
    EmptyDiskRecord,
    UnknownLease(ChunkLeaseId),
    RelinkNotVerified,
    InvalidRecordingFifo,
    InvalidRecordingHeader,
    RecordingNotCapturing,
    RecordingNotFinalizing,
    RecordingFifoFull,
    InvalidCaptureBlockFrames(u32),
    UnexpectedRecordingSequence {
        expected: u64,
        actual: u64,
    },
    DiscontinuousCapture {
        expected: i64,
        actual: i64,
    },
    RecordingBlockNotPending(u64),
    NonMonotonicDurableOffset,
    RecordingWritesPending,
    ArithmeticOverflow,
}

impl fmt::Display for StreamingMediaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSampleRate => formatter.write_str("PCM sample rate must not be zero"),
            Self::ZeroChannels => formatter.write_str("PCM channel count must not be zero"),
            Self::InvalidChunkFrames(value) => write!(
                formatter,
                "PCM chunk width {value} is not a non-zero power of two"
            ),
            Self::InvalidWaveformBinFrames(value) => write!(
                formatter,
                "waveform bin width {value} is not a non-zero power of two"
            ),
            Self::EmptyPcm => formatter.write_str("decoded PCM must contain at least one frame"),
            Self::InvalidFrameSpan { start, end } => write!(
                formatter,
                "frame span [{start}, {end}) is empty or reversed"
            ),
            Self::ChunkOutsidePcm {
                index,
                total_frames,
            } => write!(
                formatter,
                "chunk {} starts outside {total_frames}-frame PCM",
                index.0
            ),
            Self::SliceOutsidePcm { end, frame_count } => {
                write!(formatter, "slice end {end} exceeds {frame_count}-frame PCM")
            }
            Self::SliceReadOutsideRange { end, frame_count } => write!(
                formatter,
                "slice-relative read end {end} exceeds {frame_count} frames"
            ),
            Self::PlayheadOutsidePcm { frame, frame_count } => write!(
                formatter,
                "playhead frame {frame} exceeds {frame_count}-frame PCM"
            ),
            Self::ChunkSampleCount { expected, actual } => write!(
                formatter,
                "chunk has {actual} interleaved samples; expected {expected}"
            ),
            Self::NonFinitePcm { sample_index } => {
                write!(formatter, "PCM sample {sample_index} is not finite")
            }
            Self::MissingChunk(index) => write!(formatter, "PCM chunk {} is not resident", index.0),
            Self::ChunkUnavailable => formatter.write_str("PCM chunk is not resident"),
            Self::InvalidPrefetchPolicy => {
                formatter.write_str("prefetch bounds are zero or internally inconsistent")
            }
            Self::InvalidViewportChunkPolicy => formatter.write_str(
                "viewport chunk bounds are zero or internally inconsistent",
            ),
            Self::InvalidSnapshotChunkLimit => {
                formatter.write_str("prepared media snapshots need a non-zero chunk limit")
            }
            Self::ViewportOutsidePcm {
                end,
                frame_count,
            } => write!(
                formatter,
                "viewport ends at frame {end}, beyond {frame_count}-frame PCM"
            ),
            Self::ViewportDemandExceedsLimit {
                required_chunks,
                maximum_chunks,
            } => write!(
                formatter,
                "viewport needs {required_chunks} PCM chunks, above the {maximum_chunks}-chunk bound"
            ),
            Self::ZeroCacheBudget => formatter.write_str("media cache budgets must be non-zero"),
            Self::EntryExceedsBudget {
                tier,
                bytes,
                budget,
            } => write!(
                formatter,
                "{tier:?} entry of {bytes} bytes exceeds {budget}-byte budget"
            ),
            Self::AllEvictionCandidatesPinned(tier) => {
                write!(formatter, "all {tier:?} eviction candidates are leased")
            }
            Self::EmptyDiskRecord => formatter.write_str("disk cache extent must contain bytes"),
            Self::UnknownLease(id) => write!(formatter, "chunk lease {} is not active", id.0),
            Self::RelinkNotVerified => formatter
                .write_str("relink candidate has not passed encoded-source identity verification"),
            Self::InvalidRecordingFifo => formatter.write_str("recording FIFO geometry is empty"),
            Self::InvalidRecordingHeader => {
                formatter.write_str("recording journal header does not match FIFO format")
            }
            Self::RecordingNotCapturing => {
                formatter.write_str("recording writer is not accepting capture blocks")
            }
            Self::RecordingNotFinalizing => {
                formatter.write_str("recording writer has not entered finalization")
            }
            Self::RecordingFifoFull => formatter.write_str("recording FIFO handoff is full"),
            Self::InvalidCaptureBlockFrames(frames) => write!(
                formatter,
                "capture block contains invalid frame count {frames}"
            ),
            Self::UnexpectedRecordingSequence { expected, actual } => write!(
                formatter,
                "recording sequence {actual} arrived; expected {expected}"
            ),
            Self::DiscontinuousCapture { expected, actual } => write!(
                formatter,
                "capture starts at project frame {actual}; expected {expected}"
            ),
            Self::RecordingBlockNotPending(sequence) => {
                write!(formatter, "recording block {sequence} is not pending")
            }
            Self::NonMonotonicDurableOffset => {
                formatter.write_str("recording durable byte offsets must increase")
            }
            Self::RecordingWritesPending => formatter
                .write_str("recording cannot finalize while blocks await durable acknowledgement"),
            Self::ArithmeticOverflow => formatter.write_str("media range arithmetic overflowed"),
        }
    }
}

impl Error for StreamingMediaError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
    struct TestDigest([u8; 32]);

    fn digest(value: u8) -> TestDigest {
        TestDigest([value; 32])
    }

    fn descriptor(id: u8, frames: u64) -> DecodedPcmDescriptor<TestDigest> {
        DecodedPcmDescriptor::new(
            DecodedPcmId(digest(id)),
            PcmChunkGeometry::new(48_000, 2, 4).unwrap(),
            frames,
        )
        .unwrap()
    }

    fn chunk(source: DecodedPcmDescriptor<TestDigest>, index: u64) -> PcmChunk<TestDigest> {
        let span = source
            .geometry
            .chunk_span(PcmChunkIndex(index), source.frame_count)
            .unwrap();
        let mut samples = Vec::new();
        for frame in span.start..span.end {
            samples.push(frame as f32);
            samples.push(-(frame as f32));
        }
        PcmChunk::new(source, PcmChunkIndex(index), samples.into(), 2).unwrap()
    }

    fn generous_store() -> BoundedMediaStore<TestDigest> {
        BoundedMediaStore::new(CacheBudgets {
            memory_bytes: 1_000_000,
            disk_bytes: 1_000_000,
        })
        .unwrap()
    }

    #[test]
    fn boundary_read_crosses_chunks_without_duplication_or_gap() {
        let source = descriptor(1, 10);
        let mut store = generous_store();
        for index in 0..3 {
            store.publish_resident(chunk(source, index)).unwrap();
        }
        let slice = VirtualSliceRef::new(source, FrameSpan::new(2, 9).unwrap()).unwrap();
        let samples = store.read_slice(slice, 1, 6).unwrap();
        assert_eq!(
            samples,
            vec![3.0, -3.0, 4.0, -4.0, 5.0, -5.0, 6.0, -6.0, 7.0, -7.0, 8.0, -8.0]
        );
    }

    #[test]
    fn prefetch_is_directional_bounded_and_prioritizes_current_chunk() {
        let source = descriptor(2, 32);
        let policy = PrefetchPolicy {
            lookahead_chunks: 3,
            lookbehind_chunks: 2,
            max_queued: 5,
            max_in_flight: 2,
        };
        let plan = plan_prefetch(source, 17, PlaybackDirection::Forward, policy, 9).unwrap();
        let indices: Vec<_> = plan.iter().map(|request| request.key.index.0).collect();
        assert_eq!(indices, vec![4, 5, 6, 7, 3]);
        assert_eq!(plan[0].priority, RequestPriority::Playback);

        let mut queue = DecodeRequestQueue::new(policy).unwrap();
        queue.submit_all(plan);
        assert_eq!(queue.start_next().unwrap().key.index, PcmChunkIndex(4));
        assert_eq!(queue.start_next().unwrap().key.index, PcmChunkIndex(5));
        assert!(queue.start_next().is_none());
    }

    #[test]
    fn viewport_plan_keeps_all_visible_chunks_and_bounds_scroll_context() {
        let source = descriptor(7, 40);
        let policy = ViewportChunkPolicy {
            context_before_chunks: 3,
            context_after_chunks: 3,
            maximum_visible_chunks: 5,
            maximum_total_chunks: 7,
        };
        let plan =
            plan_viewport_chunks(source, FrameSpan::new(9, 25).unwrap(), policy, 12).unwrap();
        assert_eq!(
            plan.iter()
                .map(|request| (request.key.index.0, request.priority))
                .collect::<Vec<_>>(),
            vec![
                (2, RequestPriority::Visible),
                (3, RequestPriority::Visible),
                (4, RequestPriority::Visible),
                (5, RequestPriority::Visible),
                (6, RequestPriority::Visible),
                (7, RequestPriority::Lookahead),
                (8, RequestPriority::Lookahead),
            ]
        );
        assert!(plan.iter().all(|request| request.demand_epoch == 12));

        let too_narrow = ViewportChunkPolicy {
            maximum_visible_chunks: 4,
            ..policy
        };
        assert_eq!(
            plan_viewport_chunks(source, FrameSpan::new(9, 25).unwrap(), too_narrow, 13,),
            Err(StreamingMediaError::ViewportDemandExceedsLimit {
                required_chunks: 5,
                maximum_chunks: 4,
            })
        );
    }

    #[test]
    fn memory_lru_evicts_oldest_unpinned_and_a_lease_blocks_eviction() {
        let source = descriptor(3, 12);
        let first = chunk(source, 0);
        let bytes = first.resident_bytes();
        let mut store = BoundedMediaStore::new(CacheBudgets {
            memory_bytes: bytes * 2,
            disk_bytes: 1_000,
        })
        .unwrap();
        store.publish_resident(first).unwrap();
        store.publish_resident(chunk(source, 1)).unwrap();
        let key0 = source.chunk_key(PcmChunkIndex(0)).unwrap();
        let key1 = source.chunk_key(PcmChunkIndex(1)).unwrap();
        let key2 = source.chunk_key(PcmChunkIndex(2)).unwrap();
        let lease = store.acquire(key0).unwrap();
        store.publish_resident(chunk(source, 2)).unwrap();
        assert!(store.contains_resident(key0));
        assert!(!store.contains_resident(key1));
        assert!(store.contains_resident(key2));
        assert_eq!(store.accounting().memory_evictions, 1);
        store.release(lease.id).unwrap();
    }

    #[test]
    fn disk_lru_is_separate_and_honors_the_same_lease_pin() {
        let source = descriptor(4, 12);
        let mut store = BoundedMediaStore::new(CacheBudgets {
            memory_bytes: 100_000,
            disk_bytes: 20,
        })
        .unwrap();
        for index in 0..2 {
            store.publish_resident(chunk(source, index)).unwrap();
            store
                .publish_disk_record(DiskChunkRecord {
                    key: source.chunk_key(PcmChunkIndex(index)).unwrap(),
                    encoded_bytes: 10,
                    cache_locator: format!("chunk-{index}"),
                })
                .unwrap();
        }
        let key0 = source.chunk_key(PcmChunkIndex(0)).unwrap();
        let key1 = source.chunk_key(PcmChunkIndex(1)).unwrap();
        let key2 = source.chunk_key(PcmChunkIndex(2)).unwrap();
        let lease = store.acquire(key0).unwrap();
        store
            .publish_disk_record(DiskChunkRecord {
                key: key2,
                encoded_bytes: 10,
                cache_locator: "chunk-2".into(),
            })
            .unwrap();
        assert!(store.contains_disk(key0));
        assert!(!store.contains_disk(key1));
        assert!(store.contains_disk(key2));
        assert_eq!(store.accounting().disk_evictions, 1);
        store.release(lease.id).unwrap();
    }

    #[test]
    fn non_finite_pcm_is_quarantined_before_publication_and_waveform_work() {
        let source = descriptor(5, 4);
        let result = PcmChunk::new(
            source,
            PcmChunkIndex(0),
            Arc::from([0.0, 0.0, f32::NAN, 1.0, 0.0, 0.0, 0.0, 0.0]),
            2,
        );
        assert!(matches!(
            result,
            Err(StreamingMediaError::NonFinitePcm { sample_index: 2 })
        ));
    }

    #[test]
    fn slice_identity_is_structural_and_never_path_based() {
        let source = descriptor(6, 20);
        let same_a = VirtualSliceRef::new(source, FrameSpan::new(3, 11).unwrap()).unwrap();
        let same_b = VirtualSliceRef::new(source, FrameSpan::new(3, 11).unwrap()).unwrap();
        let shifted = VirtualSliceRef::new(source, FrameSpan::new(4, 12).unwrap()).unwrap();
        let other_source =
            VirtualSliceRef::new(descriptor(7, 20), FrameSpan::new(3, 11).unwrap()).unwrap();
        assert_eq!(same_a, same_b);
        assert_ne!(same_a, shifted);
        assert_ne!(same_a, other_source);
        assert_eq!(same_a.covering_chunks().len(), 3);
    }

    #[test]
    fn relink_requires_exact_encoded_source_identity() {
        let mut record = MediaAvailabilityRecord::new(EncodedSourceId(digest(8)));
        record.observe_relink(
            MediaRoute {
                locator: "/replacement.wav".into(),
            },
            EncodedSourceId(digest(9)),
        );
        assert!(record.accept_verified_relink().is_err());
        record.observe_relink(
            MediaRoute {
                locator: "/verified.wav".into(),
            },
            EncodedSourceId(digest(8)),
        );
        record.accept_verified_relink().unwrap();
        assert!(matches!(record.state, MediaAvailability::Available { .. }));
    }

    #[test]
    fn recording_journal_exposes_only_durably_acknowledged_prefix() {
        let header = RecordingJournalHeader {
            take: RecordingTakeId(1),
            project: digest(10),
            track_key: 7,
            start_project_frame: 1_000,
            sample_rate_hz: 48_000,
            channels: 2,
            fifo: RecordingFifoContract {
                capacity_blocks: 2,
                frames_per_block: 4,
                channels: 2,
            },
        };
        let mut writer = RecordingJournalWriter::new(header).unwrap();
        for sequence in 0..2 {
            writer
                .enqueue_from_fifo(CaptureBlock {
                    sequence,
                    first_project_frame: 1_000 + sequence as i64 * 4,
                    frames: 4,
                    interleaved: Arc::from([0.0_f32; 8]),
                })
                .unwrap();
        }
        assert_eq!(writer.recoverable_frames(), 0);
        assert!(writer.request_finalize().is_err());
        writer.acknowledge_durable(0, 32, digest(11)).unwrap();
        assert_eq!(writer.recoverable_frames(), 4);
        writer.acknowledge_durable(1, 64, digest(12)).unwrap();
        writer.request_finalize().unwrap();
        writer
            .acknowledge_finalized(EncodedSourceId(digest(13)), DecodedPcmId(digest(14)))
            .unwrap();
        assert_eq!(writer.journal().len(), 2);
        assert!(matches!(
            writer.state(),
            RecordingWriterState::Finalized { .. }
        ));
    }
}
