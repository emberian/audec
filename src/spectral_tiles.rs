//! Resolution-aware, immutable spectrogram tiles.
//!
//! A spectrogram is evidence, not a decorative bitmap.  Cropping a fixed
//! whole-song image when a lens zooms in throws away source detail and then
//! magnifies the loss.  This module keeps the request in source-frame space,
//! resolves an FFT/hop recipe for the requested physical width, and computes
//! a fresh numeric tile from canonical mono PCM.  UI code may display a coarse
//! tile immediately, replace it with the final tile when ready, and colorize
//! the retained scalar raster without rerunning the FFT.

use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

use crate::settings::WindowFunction;

const MIN_FFT_SIZE: usize = 64;
const MAX_FFT_SIZE: usize = 131_072;
const MIN_DB: f32 = -160.0;

/// Exact half-open frame range in the canonical project sample rate.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct FrameRange {
    pub start: u64,
    pub end: u64,
}

impl FrameRange {
    pub fn new(start: u64, end: u64) -> Self {
        Self {
            start,
            end: end.max(start),
        }
    }

    pub fn len(self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// Identifies the PCM generation from which a tile was derived.
///
/// `content` should be a stable digest (or project-local content ID),
/// `revision` changes when the content is edited, and `generation` changes
/// when a renderer replaces the backing PCM without changing its logical
/// revision.  Keeping all three in the key makes stale analysis impossible to
/// confuse with a current result.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct SourceStamp {
    pub content: u64,
    pub revision: u64,
    pub generation: u64,
    pub sample_rate: u32,
    pub frame_count: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FrequencyScale {
    Linear,
    Logarithmic,
}

/// Displayed frequency extent. Equality and hashing preserve the exact IEEE
/// bits so cache behavior is deterministic even across very small UI edits.
#[derive(Clone, Copy, Debug)]
pub struct FrequencyRange {
    pub min_hz: f32,
    pub max_hz: f32,
    pub scale: FrequencyScale,
}

impl FrequencyRange {
    pub fn logarithmic(min_hz: f32, max_hz: f32) -> Self {
        Self {
            min_hz,
            max_hz,
            scale: FrequencyScale::Logarithmic,
        }
    }

    pub fn linear(min_hz: f32, max_hz: f32) -> Self {
        Self {
            min_hz,
            max_hz,
            scale: FrequencyScale::Linear,
        }
    }

    fn normalized(self, sample_rate: u32) -> Self {
        let nyquist = (sample_rate as f32 * 0.5).max(2.0);
        let minimum = match self.scale {
            FrequencyScale::Linear => self.min_hz.clamp(0.0, nyquist - 1.0),
            FrequencyScale::Logarithmic => self.min_hz.clamp(1.0, nyquist - 1.0),
        };
        let maximum = self.max_hz.clamp(minimum + 1.0, nyquist);
        Self {
            min_hz: if minimum.is_finite() { minimum } else { 1.0 },
            max_hz: if maximum.is_finite() {
                maximum
            } else {
                nyquist
            },
            scale: self.scale,
        }
    }
}

impl PartialEq for FrequencyRange {
    fn eq(&self, other: &Self) -> bool {
        self.min_hz.to_bits() == other.min_hz.to_bits()
            && self.max_hz.to_bits() == other.max_hz.to_bits()
            && self.scale == other.scale
    }
}

impl Eq for FrequencyRange {}

impl Hash for FrequencyRange {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.min_hz.to_bits().hash(state);
        self.max_hz.to_bits().hash(state);
        self.scale.hash(state);
    }
}

/// User/lens-selected analysis recipe. The planner may lower `fft_size` for a
/// close zoom, but never below `min_fft_size`; both the requested values and
/// the resolved FFT/hop are represented in the final key.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpectralRecipe {
    pub fft_size: usize,
    pub min_fft_size: usize,
    /// Maximum FFT periods per output column. A smaller value preserves fast
    /// transients on close zooms; four is a useful perceptual default.
    pub max_window_columns: usize,
    pub window: WindowFunction,
    pub frequency_bins: usize,
    pub db_ceiling: f32,
    pub db_range: f32,
}

impl Default for SpectralRecipe {
    fn default() -> Self {
        Self {
            fft_size: 4_096,
            min_fft_size: 256,
            max_window_columns: 4,
            window: WindowFunction::Hann,
            frequency_bins: 256,
            db_ceiling: -5.0,
            db_range: 84.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpectralTileRequest {
    pub source: SourceStamp,
    pub frames: FrameRange,
    /// Physical output pixels, after display scale is applied. The final tile
    /// normally provides one independently analyzed column per pixel.
    pub target_pixel_width: usize,
    pub frequencies: FrequencyRange,
    pub recipe: SpectralRecipe,
}

/// Fully resolved request and cache key.
#[derive(Clone, Copy, Debug)]
pub struct SpectralTileKey {
    pub source: SourceStamp,
    pub frames: FrameRange,
    pub target_pixel_width: usize,
    pub frequencies: FrequencyRange,
    pub frequency_bins: usize,
    pub requested_fft_size: usize,
    pub requested_min_fft_size: usize,
    pub max_window_columns: usize,
    pub fft_size: usize,
    pub hop_size: usize,
    pub column_count: usize,
    pub window: WindowFunction,
    pub db_ceiling: f32,
    pub db_range: f32,
}

impl PartialEq for SpectralTileKey {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
            && self.frames == other.frames
            && self.target_pixel_width == other.target_pixel_width
            && self.frequencies == other.frequencies
            && self.frequency_bins == other.frequency_bins
            && self.requested_fft_size == other.requested_fft_size
            && self.requested_min_fft_size == other.requested_min_fft_size
            && self.max_window_columns == other.max_window_columns
            && self.fft_size == other.fft_size
            && self.hop_size == other.hop_size
            && self.column_count == other.column_count
            && self.window == other.window
            && self.db_ceiling.to_bits() == other.db_ceiling.to_bits()
            && self.db_range.to_bits() == other.db_range.to_bits()
    }
}

impl Eq for SpectralTileKey {}

impl Hash for SpectralTileKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.source.hash(state);
        self.frames.hash(state);
        self.target_pixel_width.hash(state);
        self.frequencies.hash(state);
        self.frequency_bins.hash(state);
        self.requested_fft_size.hash(state);
        self.requested_min_fft_size.hash(state);
        self.max_window_columns.hash(state);
        self.fft_size.hash(state);
        self.hop_size.hash(state);
        self.column_count.hash(state);
        window_tag(self.window).hash(state);
        self.db_ceiling.to_bits().hash(state);
        self.db_range.to_bits().hash(state);
    }
}

impl SpectralTileKey {
    /// Stable FNV-1a fingerprint for disk paths, logs, and task de-duplication.
    /// This deliberately does not depend on Rust's `HashMap` hasher.
    pub fn stable_fingerprint(self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        let mut add = |value: u64| {
            hash ^= value;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        };
        add(self.source.content);
        add(self.source.revision);
        add(self.source.generation);
        add(u64::from(self.source.sample_rate));
        add(self.source.frame_count);
        add(self.frames.start);
        add(self.frames.end);
        add(self.target_pixel_width as u64);
        add(u64::from(self.frequencies.min_hz.to_bits()));
        add(u64::from(self.frequencies.max_hz.to_bits()));
        add(match self.frequencies.scale {
            FrequencyScale::Linear => 0,
            FrequencyScale::Logarithmic => 1,
        });
        add(self.frequency_bins as u64);
        add(self.requested_fft_size as u64);
        add(self.requested_min_fft_size as u64);
        add(self.max_window_columns as u64);
        add(self.fft_size as u64);
        add(self.hop_size as u64);
        add(self.column_count as u64);
        add(u64::from(window_tag(self.window)));
        add(u64::from(self.db_ceiling.to_bits()));
        add(u64::from(self.db_range.to_bits()));
        hash
    }

    /// Exact source interval represented by a column. Every source frame in
    /// the request maps to exactly one interval; the final one may be shorter.
    pub fn frame_range_for_column(self, column: usize) -> FrameRange {
        if self.frames.is_empty() || self.column_count == 0 {
            return FrameRange::new(self.frames.start, self.frames.start);
        }
        let column = column.min(self.column_count - 1);
        let start = self
            .frames
            .start
            .saturating_add((column as u64).saturating_mul(self.hop_size as u64))
            .min(self.frames.end);
        let end = start
            .saturating_add(self.hop_size as u64)
            .min(self.frames.end);
        FrameRange::new(start, end)
    }

    pub fn center_frame_for_column(self, column: usize) -> u64 {
        let range = self.frame_range_for_column(column);
        range.start + range.len().saturating_sub(1) / 2
    }

    pub fn column_for_frame(self, frame: u64) -> Option<usize> {
        if !self.frames.is_empty() && frame >= self.frames.start && frame < self.frames.end {
            Some(
                (((frame - self.frames.start) / self.hop_size.max(1) as u64) as usize)
                    .min(self.column_count.saturating_sub(1)),
            )
        } else {
            None
        }
    }

    pub fn frequency_for_row(self, row: usize) -> f32 {
        let denominator = self.frequency_bins.saturating_sub(1).max(1) as f32;
        let fraction = row.min(self.frequency_bins.saturating_sub(1)) as f32 / denominator;
        match self.frequencies.scale {
            FrequencyScale::Linear => {
                self.frequencies.min_hz
                    + fraction * (self.frequencies.max_hz - self.frequencies.min_hz)
            }
            FrequencyScale::Logarithmic => {
                self.frequencies.min_hz
                    * (self.frequencies.max_hz / self.frequencies.min_hz).powf(fraction)
            }
        }
    }
}

fn window_tag(window: WindowFunction) -> u8 {
    match window {
        WindowFunction::Rectangular => 0,
        WindowFunction::Hann => 1,
        WindowFunction::Blackman => 2,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpectralRequestPlan {
    /// Optional quick preview. Check the cache first; otherwise schedule it at
    /// lower priority than an already-cached result and before `final_key`.
    pub coarse_key: Option<SpectralTileKey>,
    pub final_key: SpectralTileKey,
}

#[derive(Clone, Copy, Debug)]
pub struct SpectralTilePlanner {
    pub coarse_pixel_width: usize,
    pub max_pixel_width: usize,
}

impl Default for SpectralTilePlanner {
    fn default() -> Self {
        Self {
            coarse_pixel_width: 256,
            max_pixel_width: 8_192,
        }
    }
}

impl SpectralTilePlanner {
    pub fn plan(&self, request: SpectralTileRequest) -> SpectralRequestPlan {
        let final_key = self.resolve(request, request.target_pixel_width);
        let coarse_width = request
            .target_pixel_width
            .min(self.coarse_pixel_width.max(1));
        let coarse_key = (coarse_width < final_key.target_pixel_width)
            .then(|| self.resolve(request, coarse_width));
        SpectralRequestPlan {
            coarse_key,
            final_key,
        }
    }

    pub fn resolve(&self, request: SpectralTileRequest, pixel_width: usize) -> SpectralTileKey {
        let sample_rate = request.source.sample_rate.max(1);
        let frames = FrameRange::new(
            request.frames.start.min(request.source.frame_count),
            request.frames.end.min(request.source.frame_count),
        );
        let target_pixel_width = pixel_width.clamp(1, self.max_pixel_width.max(1));
        let span = frames.len().max(1) as usize;
        let hop_size = span.div_ceil(target_pixel_width).max(1);
        let column_count = span.div_ceil(hop_size).max(1);

        let requested_fft_size = normalize_fft(request.recipe.fft_size);
        let requested_min_fft_size = normalize_fft(request.recipe.min_fft_size)
            .min(requested_fft_size)
            .max(MIN_FFT_SIZE);
        let temporal_limit = hop_size
            .saturating_mul(request.recipe.max_window_columns.max(1))
            .max(MIN_FFT_SIZE)
            .next_power_of_two()
            .min(MAX_FFT_SIZE);
        let fft_size = requested_fft_size
            .min(temporal_limit)
            .max(requested_min_fft_size);

        SpectralTileKey {
            source: request.source,
            frames,
            target_pixel_width,
            frequencies: request.frequencies.normalized(sample_rate),
            frequency_bins: request.recipe.frequency_bins.clamp(8, 4_096),
            requested_fft_size,
            requested_min_fft_size,
            max_window_columns: request.recipe.max_window_columns.max(1),
            fft_size,
            hop_size,
            column_count,
            window: request.recipe.window,
            db_ceiling: finite_or(request.recipe.db_ceiling, 0.0).clamp(-120.0, 24.0),
            db_range: finite_or(request.recipe.db_range, 84.0).clamp(6.0, 180.0),
        }
    }
}

fn normalize_fft(size: usize) -> usize {
    size.clamp(MIN_FFT_SIZE, MAX_FFT_SIZE).next_power_of_two()
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

/// Row-major, top-to-bottom 8-bit intensity image. This can be uploaded as an
/// R8 texture directly, or colorized on the CPU/GPU. Numeric dB values remain
/// available in `SpectralTile::db`, column-major and low-frequency first.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScalarRaster {
    pub width: usize,
    pub height: usize,
    pub pixels: Arc<[u8]>,
}

#[derive(Clone, Debug)]
pub struct SpectralTile {
    pub key: SpectralTileKey,
    /// `db[column * frequency_bins + low_to_high_row]`.
    pub db: Arc<[f32]>,
    pub scalar: ScalarRaster,
}

impl SpectralTile {
    pub fn db_at(&self, column: usize, low_to_high_row: usize) -> Option<f32> {
        (column < self.key.column_count && low_to_high_row < self.key.frequency_bins)
            .then(|| self.db[column * self.key.frequency_bins + low_to_high_row])
    }

    pub fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.db.len() * std::mem::size_of::<f32>())
            .saturating_add(self.scalar.pixels.len())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpectralTileError {
    Cancelled,
    InvalidSource {
        expected_frames: u64,
        actual_frames: usize,
    },
}

impl fmt::Display for SpectralTileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("spectral tile computation was cancelled"),
            Self::InvalidSource {
                expected_frames,
                actual_frames,
            } => write!(
                formatter,
                "spectral source declares {expected_frames} frames but PCM has {actual_frames}"
            ),
        }
    }
}

impl std::error::Error for SpectralTileError {}

/// Executor-independent cancellation seam. A background task should own a
/// token and cancel the previous generation when its lens viewport changes.
pub trait CancellationToken: Send + Sync {
    fn is_cancelled(&self) -> bool;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NeverCancel;

impl CancellationToken for NeverCancel {
    fn is_cancelled(&self) -> bool {
        false
    }
}

#[derive(Clone, Debug, Default)]
pub struct SpectralCancellation(Arc<AtomicBool>);

impl SpectralCancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

impl CancellationToken for SpectralCancellation {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Compute one immutable tile from the retained, whole-source mono PCM.
/// Window reads are zero-padded at source boundaries. Cancellation is checked
/// once per output column, which keeps interaction latency proportional to a
/// single FFT rather than a whole tile.
pub fn compute_spectral_tile(
    mono: &[f32],
    key: SpectralTileKey,
    cancellation: &dyn CancellationToken,
) -> Result<SpectralTile, SpectralTileError> {
    if key.source.frame_count as usize != mono.len() {
        return Err(SpectralTileError::InvalidSource {
            expected_frames: key.source.frame_count,
            actual_frames: mono.len(),
        });
    }
    if cancellation.is_cancelled() {
        return Err(SpectralTileError::Cancelled);
    }

    let fft_size = key.fft_size;
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(fft_size);
    let window: Vec<f32> = (0..fft_size)
        .map(|index| key.window.coefficient(index, fft_size))
        .collect();
    let amplitude_scale = 2.0 / window.iter().copied().sum::<f32>().max(1.0e-12);
    let band_ranges = fft_band_ranges(key);
    let mut input = vec![Complex::default(); fft_size];
    let mut magnitudes = vec![0.0_f32; fft_size / 2 + 1];
    let mut db = vec![MIN_DB; key.column_count * key.frequency_bins];

    for column in 0..key.column_count {
        if cancellation.is_cancelled() {
            return Err(SpectralTileError::Cancelled);
        }
        let center = key.center_frame_for_column(column) as i128;
        let window_start = center - fft_size as i128 / 2;
        for (offset, point) in input.iter_mut().enumerate() {
            let source_frame = window_start + offset as i128;
            point.re = if source_frame >= 0 && source_frame < mono.len() as i128 {
                mono[source_frame as usize] * window[offset]
            } else {
                0.0
            };
            point.im = 0.0;
        }
        fft.process(&mut input);
        for (magnitude, point) in magnitudes.iter_mut().zip(input.iter()) {
            *magnitude = point.norm() * amplitude_scale;
        }
        for (row, &(low, high)) in band_ranges.iter().enumerate() {
            let magnitude = magnitudes[low..high]
                .iter()
                .copied()
                .fold(0.0_f32, f32::max);
            db[column * key.frequency_bins + row] =
                (20.0 * magnitude.max(1.0e-8).log10()).max(MIN_DB);
        }
    }

    let scalar = scalar_raster(&db, key);
    Ok(SpectralTile {
        key,
        db: db.into(),
        scalar,
    })
}

fn fft_band_ranges(key: SpectralTileKey) -> Vec<(usize, usize)> {
    let max_bin_exclusive = key.fft_size / 2 + 1;
    let bin_for_hz = |hz: f32| {
        (hz * key.fft_size as f32 / key.source.sample_rate.max(1) as f32)
            .floor()
            .clamp(0.0, (max_bin_exclusive - 1) as f32) as usize
    };
    (0..key.frequency_bins)
        .map(|row| {
            let center = key.frequency_for_row(row);
            let (low_hz, high_hz) = if key.frequency_bins <= 1 {
                (key.frequencies.min_hz, key.frequencies.max_hz)
            } else {
                match key.frequencies.scale {
                    FrequencyScale::Linear => {
                        let half = (key.frequencies.max_hz - key.frequencies.min_hz)
                            / (key.frequency_bins - 1) as f32
                            * 0.5;
                        (center - half, center + half)
                    }
                    FrequencyScale::Logarithmic => {
                        let half_step = (key.frequencies.max_hz / key.frequencies.min_hz)
                            .powf(0.5 / (key.frequency_bins - 1) as f32);
                        (center / half_step, center * half_step)
                    }
                }
            };
            let low = bin_for_hz(low_hz.max(0.0)).min(max_bin_exclusive - 1);
            let high = (bin_for_hz(high_hz).saturating_add(1))
                .max(low + 1)
                .min(max_bin_exclusive);
            (low, high)
        })
        .collect()
}

fn scalar_raster(db: &[f32], key: SpectralTileKey) -> ScalarRaster {
    let mut pixels = vec![0_u8; key.column_count * key.frequency_bins];
    let floor = key.db_ceiling - key.db_range;
    for display_row in 0..key.frequency_bins {
        let source_row = key.frequency_bins - display_row - 1;
        for column in 0..key.column_count {
            let value = db[column * key.frequency_bins + source_row];
            let normalized = ((value - floor) / key.db_range).clamp(0.0, 1.0);
            pixels[display_row * key.column_count + column] = (normalized * 255.0).round() as u8;
        }
    }
    ScalarRaster {
        width: key.column_count,
        height: key.frequency_bins,
        pixels: pixels.into(),
    }
}

struct CacheEntry {
    tile: Arc<SpectralTile>,
    bytes: usize,
    last_used: u64,
}

/// Small dependency-free LRU-ish memory cache. Eviction is exact LRU at tile
/// granularity; "ish" acknowledges that a tile held by the UI may outlive its
/// cache entry through `Arc`.
pub struct SpectralTileCache {
    entries: HashMap<SpectralTileKey, CacheEntry>,
    max_entries: usize,
    max_bytes: usize,
    resident_bytes: usize,
    clock: u64,
}

impl SpectralTileCache {
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries,
            max_bytes,
            resident_bytes: 0,
            clock: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }

    pub fn get(&mut self, key: &SpectralTileKey) -> Option<Arc<SpectralTile>> {
        self.clock = self.clock.wrapping_add(1);
        self.entries.get_mut(key).map(|entry| {
            entry.last_used = self.clock;
            Arc::clone(&entry.tile)
        })
    }

    /// Returns false when this cache is disabled or one tile exceeds its
    /// complete byte budget. Existing entries are never evicted for a tile
    /// that cannot itself fit.
    pub fn insert(&mut self, tile: Arc<SpectralTile>) -> bool {
        let bytes = tile.estimated_bytes();
        if self.max_entries == 0 || bytes > self.max_bytes {
            return false;
        }
        self.clock = self.clock.wrapping_add(1);
        if let Some(previous) = self.entries.remove(&tile.key) {
            self.resident_bytes = self.resident_bytes.saturating_sub(previous.bytes);
        }
        self.resident_bytes = self.resident_bytes.saturating_add(bytes);
        self.entries.insert(
            tile.key,
            CacheEntry {
                tile,
                bytes,
                last_used: self.clock,
            },
        );
        self.evict_to_budget();
        true
    }

    pub fn invalidate_content(&mut self, content: u64) {
        self.remove_where(|key| key.source.content == content);
    }

    /// Drop all old revisions/generations of a logical source after installing
    /// a new source stamp, while preserving other open projects' tiles.
    pub fn retain_generation(&mut self, source: SourceStamp) {
        self.remove_where(|key| key.source.content == source.content && key.source != source);
    }

    fn remove_where(&mut self, predicate: impl Fn(&SpectralTileKey) -> bool) {
        let keys: Vec<_> = self
            .entries
            .keys()
            .filter(|key| predicate(key))
            .copied()
            .collect();
        for key in keys {
            if let Some(entry) = self.entries.remove(&key) {
                self.resident_bytes = self.resident_bytes.saturating_sub(entry.bytes);
            }
        }
    }

    fn evict_to_budget(&mut self) {
        while self.entries.len() > self.max_entries || self.resident_bytes > self.max_bytes {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.resident_bytes = self.resident_bytes.saturating_sub(entry.bytes);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(frame_count: u64) -> SourceStamp {
        SourceStamp {
            content: 0xace,
            revision: 7,
            generation: 3,
            sample_rate: 48_000,
            frame_count,
        }
    }

    fn request(frames: FrameRange, width: usize) -> SpectralTileRequest {
        SpectralTileRequest {
            source: source(480_000),
            frames,
            target_pixel_width: width,
            frequencies: FrequencyRange::logarithmic(30.0, 20_000.0),
            recipe: SpectralRecipe::default(),
        }
    }

    #[test]
    fn zoom_reanalyzes_materially_more_source_detail_than_atlas_crop() {
        let planner = SpectralTilePlanner::default();
        let atlas = planner.plan(request(FrameRange::new(0, 480_000), 1_200));
        let zoom = planner.plan(request(FrameRange::new(120_000, 168_000), 1_200));
        let atlas_columns_visible_in_zoom = atlas.final_key.column_count * 48_000 / 480_000;

        assert!(zoom.final_key.column_count >= 1_000);
        assert!(zoom.final_key.column_count > atlas_columns_visible_in_zoom * 8);
        assert!(zoom.final_key.hop_size < atlas.final_key.hop_size / 8);
        assert!(zoom.final_key.fft_size <= atlas.final_key.fft_size);
    }

    #[test]
    fn column_mapping_exactly_partitions_requested_time() {
        let key = SpectralTilePlanner::default()
            .plan(request(FrameRange::new(101, 10_104), 317))
            .final_key;
        assert_eq!(key.frame_range_for_column(0).start, 101);
        assert_eq!(key.frame_range_for_column(key.column_count - 1).end, 10_104);
        for column in 0..key.column_count {
            let range = key.frame_range_for_column(column);
            assert!(!range.is_empty());
            assert_eq!(key.column_for_frame(range.start), Some(column));
            assert_eq!(key.column_for_frame(range.end - 1), Some(column));
            if column > 0 {
                assert_eq!(key.frame_range_for_column(column - 1).end, range.start);
            }
        }
        assert_eq!(key.column_for_frame(100), None);
        assert_eq!(key.column_for_frame(10_104), None);
    }

    #[test]
    fn key_and_stable_fingerprint_are_deterministic_and_recipe_sensitive() {
        let planner = SpectralTilePlanner::default();
        let original = request(FrameRange::new(0, 48_000), 800);
        let first = planner.plan(original).final_key;
        let second = planner.plan(original).final_key;
        assert_eq!(first, second);
        assert_eq!(first.stable_fingerprint(), second.stable_fingerprint());

        let mut changed = original;
        changed.recipe.window = WindowFunction::Blackman;
        let changed_window = planner.plan(changed).final_key;
        assert_ne!(first, changed_window);
        assert_ne!(
            first.stable_fingerprint(),
            changed_window.stable_fingerprint()
        );

        changed = original;
        changed.frames.start += 1;
        assert_ne!(first, planner.plan(changed).final_key);
        changed = original;
        changed.frequencies.min_hz = 31.0;
        assert_ne!(first, planner.plan(changed).final_key);
        changed = original;
        changed.source.generation += 1;
        assert_ne!(first, planner.plan(changed).final_key);
    }

    #[test]
    fn silence_and_short_pcm_are_finite_and_image_ready() {
        let planner = SpectralTilePlanner {
            coarse_pixel_width: 8,
            max_pixel_width: 64,
        };
        let mut requested = request(FrameRange::new(0, 17), 32);
        requested.source = source(17);
        requested.recipe.fft_size = 512;
        requested.recipe.min_fft_size = 256;
        requested.recipe.frequency_bins = 24;
        let key = planner.plan(requested).final_key;
        let tile = compute_spectral_tile(&[0.0; 17], key, &NeverCancel).unwrap();
        assert_eq!(tile.db.len(), key.column_count * key.frequency_bins);
        assert!(tile.db.iter().all(|value| value.is_finite()));
        assert!(tile.db.iter().all(|value| *value == MIN_DB));
        assert_eq!(tile.scalar.width, key.column_count);
        assert_eq!(tile.scalar.height, key.frequency_bins);
        assert!(tile.scalar.pixels.iter().all(|pixel| *pixel == 0));
    }

    #[test]
    fn cancellation_and_source_validation_fail_without_partial_tiles() {
        let mut requested = request(FrameRange::new(0, 8), 8);
        requested.source = source(8);
        let key = SpectralTilePlanner::default().plan(requested).final_key;
        let cancelled = SpectralCancellation::default();
        cancelled.cancel();
        assert!(matches!(
            compute_spectral_tile(&[0.0; 8], key, &cancelled),
            Err(SpectralTileError::Cancelled)
        ));
        assert!(matches!(
            compute_spectral_tile(&[0.0; 7], key, &NeverCancel),
            Err(SpectralTileError::InvalidSource { .. })
        ));
    }

    fn small_tile(key: SpectralTileKey, byte: u8) -> Arc<SpectralTile> {
        let length = key.column_count * key.frequency_bins;
        Arc::new(SpectralTile {
            key,
            db: vec![f32::from(byte); length].into(),
            scalar: ScalarRaster {
                width: key.column_count,
                height: key.frequency_bins,
                pixels: vec![byte; length].into(),
            },
        })
    }

    #[test]
    fn cache_is_lru_bounded_and_generation_aware() {
        let planner = SpectralTilePlanner {
            coarse_pixel_width: 4,
            max_pixel_width: 4,
        };
        let mut requested = request(FrameRange::new(0, 4), 4);
        requested.recipe.frequency_bins = 8;
        let key_a = planner.plan(requested).final_key;
        requested.frames = FrameRange::new(4, 8);
        let key_b = planner.plan(requested).final_key;
        requested.frames = FrameRange::new(8, 12);
        let key_c = planner.plan(requested).final_key;
        let tile_bytes = small_tile(key_a, 1).estimated_bytes();
        let mut cache = SpectralTileCache::new(2, tile_bytes * 2);
        assert!(cache.insert(small_tile(key_a, 1)));
        assert!(cache.insert(small_tile(key_b, 2)));
        assert!(cache.get(&key_a).is_some()); // A is newest; B must be evicted.
        assert!(cache.insert(small_tile(key_c, 3)));
        assert!(cache.get(&key_b).is_none());
        assert!(cache.get(&key_a).is_some());
        assert!(cache.get(&key_c).is_some());
        assert!(cache.len() <= 2);
        assert!(cache.resident_bytes() <= tile_bytes * 2);

        let mut new_generation = key_a.source;
        new_generation.generation += 1;
        cache.retain_generation(new_generation);
        assert!(cache.is_empty());
    }

    #[test]
    fn oversized_tile_does_not_flush_useful_cache_entries() {
        let planner = SpectralTilePlanner {
            coarse_pixel_width: 4,
            max_pixel_width: 4,
        };
        let mut requested = request(FrameRange::new(0, 4), 4);
        requested.recipe.frequency_bins = 8;
        let key = planner.plan(requested).final_key;
        let tile = small_tile(key, 1);
        let mut cache = SpectralTileCache::new(4, tile.estimated_bytes());
        assert!(cache.insert(Arc::clone(&tile)));

        let mut large_key = key;
        large_key.frequency_bins = 4_096;
        let large = small_tile(large_key, 2);
        assert!(!cache.insert(large));
        assert!(cache.get(&key).is_some());
    }

    #[test]
    fn coarse_to_fine_plan_uses_distinct_resolution_keys() {
        let plan = SpectralTilePlanner::default().plan(request(FrameRange::new(0, 96_000), 1_600));
        let coarse = plan.coarse_key.expect("wide lens should request preview");
        assert_eq!(coarse.target_pixel_width, 256);
        assert_eq!(plan.final_key.target_pixel_width, 1_600);
        assert!(coarse.column_count < plan.final_key.column_count);
        assert!(coarse.hop_size > plan.final_key.hop_size);
    }
}
