use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context as _, Result};
use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

use crate::assets::ContentFingerprint;
use crate::decomposition::{
    decompose_convolutional_cancellable, decompose_nonnegative_cancellable, ComponentDecomposition,
    ConvolutionalParams, DecompositionCancellation, DecompositionParams,
};
use crate::media_resolver::open_material_image;
use crate::pyramid::WaveformPyramid;
use crate::settings::{SpectralTransform, SpectrumSettings, WindowFunction};

pub const WAVEFORM_BINS: usize = 2_048;
pub const SPECTROGRAM_WIDTH: usize = 1_200;
pub const SPECTROGRAM_HEIGHT: usize = 216;
pub const MIN_FREQUENCY: f32 = 32.703;
pub const MAX_FREQUENCY: f32 = 16_000.0;

#[derive(Clone, Copy, Debug, Default)]
pub struct WaveformBin {
    pub left_min: f32,
    pub left_max: f32,
    pub right_min: f32,
    pub right_max: f32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FeatureFrame {
    pub loudness: f32,
    pub brightness: f32,
    pub flux: f32,
    pub stereo_width: f32,
    pub correlation: f32,
    pub dominant_hz: f32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OnsetEvent {
    pub time_seconds: f64,
    pub strength: f32,
    pub low: f32,
    pub mid: f32,
    pub high: f32,
    pub cluster: usize,
    /// Cosine similarity to the cluster's spectral template. This is not a
    /// probability that two events came from the same instrument.
    pub template_similarity: f32,
}

#[derive(Clone, Debug, Default)]
pub struct EventCluster {
    pub label: String,
    pub event_count: usize,
    pub centroid_hz: f32,
    pub consistency: f32,
    pub spectrum: Vec<f32>,
}

#[derive(Clone, Debug, Default)]
pub struct RhythmAnalysis {
    pub tempo_bpm: f32,
    /// Contrast of the strongest periodicity candidate against other tested
    /// lags. This is relative support, not calibrated confidence.
    pub pulse_contrast: f32,
    pub beat_times: Vec<f64>,
    pub onsets: Vec<OnsetEvent>,
    pub event_clusters: Vec<EventCluster>,
}

#[derive(Clone, Debug)]
pub struct Analysis {
    pub path: PathBuf,
    pub title: String,
    pub album: String,
    pub duration_seconds: f64,
    pub sample_rate: u32,
    pub channels: u32,
    pub bits_per_sample: u32,
    pub waveform: Vec<WaveformBin>,
    pub waveform_pyramid: WaveformPyramid,
    pub features: Vec<FeatureFrame>,
    pub rhythm: RhythmAnalysis,
    /// Low-rank recurring spectral/activation hypotheses over the display
    /// magnitude field. These are mixed-audio components, not named sources.
    /// Recurring magnitude factors arrive as a deferred immutable product.
    /// `None` means no factorization has published yet, not zero components.
    pub components: Option<ComponentDecomposition>,
    /// Column-major log-frequency magnitude field in dBFS.  Keeping the
    /// numeric field (rather than only its PNG) lets each lens apply its own
    /// honest level transfer and later feeds component analysis.
    pub spectral_db: Vec<f32>,
    pub spectral_peak_db: f32,
    pub spectrogram_png: Vec<u8>,
}

impl Analysis {
    /// Resolve a lens-local time range against the retained PCM pyramid.  A
    /// close zoom therefore reveals new source detail instead of stretching
    /// the fixed whole-song atlas bins.
    pub fn waveform_range(&self, start: f64, end: f64, target_bins: usize) -> Vec<WaveformBin> {
        let frame_count = self.waveform_pyramid.frame_count();
        let start_frame = (start.clamp(0.0, 1.0) * frame_count as f64).floor() as usize;
        let end_frame = (end.clamp(0.0, 1.0) * frame_count as f64).ceil() as usize;
        self.waveform_pyramid
            .query(start_frame, end_frame, target_bins)
            .bins
            .into_iter()
            .map(|bin| {
                let left = bin.channels.first().copied().unwrap_or_default();
                let right = bin.channels.get(1).copied().unwrap_or(left);
                WaveformBin {
                    left_min: left.min,
                    left_max: left.max,
                    right_min: right.min,
                    right_max: right.max,
                }
            })
            .collect()
    }

    /// Materialize an exact mono selection from canonical retained stereo PCM.
    /// This is intentionally range-based so heavy transforms can stay local to
    /// an Aspect rather than analyzing every complex bin of a whole album.
    pub fn mono_range(&self, start_frame: usize, end_frame: usize) -> Vec<f32> {
        let mut out = Vec::new();
        self.mono_range_into(start_frame, end_frame, &mut out);
        out
    }

    /// The same window, appended to a buffer the caller keeps.
    ///
    /// A lens that sweeps a field asks for thousands of windows; reusing one
    /// buffer keeps that a read of the mapped image rather than thousands of
    /// allocations. Appending (rather than clearing) is what `MonoReader`
    /// promises its callers, so this is the same contract they already hold.
    pub fn mono_range_into(&self, start_frame: usize, end_frame: usize, out: &mut Vec<f32>) {
        let channels = self.waveform_pyramid.channel_count();
        let frame_count = self.waveform_pyramid.frame_count();
        let start = start_frame.min(frame_count);
        let end = end_frame.min(frame_count).max(start);
        if channels == 0 {
            return;
        }
        out.reserve(end - start);
        out.extend(
            self.waveform_pyramid.interleaved_pcm()[start * channels..end * channels]
                .chunks_exact(channels)
                .map(|frame| {
                    if channels == 1 {
                        frame[0]
                    } else {
                        (frame[0] + frame[1]) * 0.5
                    }
                }),
        );
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct BinAccumulator {
    left_min: f32,
    left_max: f32,
    right_min: f32,
    right_max: f32,
    left_sq: f64,
    right_sq: f64,
    mid_sq: f64,
    side_sq: f64,
    cross: f64,
    count: usize,
}

impl BinAccumulator {
    fn push(&mut self, left: f32, right: f32) {
        if self.count == 0 {
            self.left_min = left;
            self.left_max = left;
            self.right_min = right;
            self.right_max = right;
        } else {
            self.left_min = self.left_min.min(left);
            self.left_max = self.left_max.max(left);
            self.right_min = self.right_min.min(right);
            self.right_max = self.right_max.max(right);
        }

        let left = f64::from(left);
        let right = f64::from(right);
        let mid = (left + right) * 0.5;
        let side = (left - right) * 0.5;
        self.left_sq += left * left;
        self.right_sq += right * right;
        self.mid_sq += mid * mid;
        self.side_sq += side * side;
        self.cross += left * right;
        self.count += 1;
    }

    fn waveform(self) -> WaveformBin {
        WaveformBin {
            left_min: self.left_min,
            left_max: self.left_max,
            right_min: self.right_min,
            right_max: self.right_max,
        }
    }

    fn loudness(self) -> f32 {
        if self.count == 0 {
            return 0.0;
        }
        let mean_square = (self.left_sq + self.right_sq) / (2.0 * self.count as f64);
        let rms = mean_square.sqrt() as f32;
        ((20.0 * rms.max(1.0e-7).log10() + 60.0) / 60.0).clamp(0.0, 1.0)
    }

    fn stereo_width(self) -> f32 {
        let energy = self.mid_sq + self.side_sq;
        if energy <= f64::EPSILON {
            0.0
        } else {
            (self.side_sq / energy).sqrt() as f32
        }
    }

    fn correlation(self) -> f32 {
        let denominator = (self.left_sq * self.right_sq).sqrt();
        if denominator <= f64::EPSILON {
            1.0
        } else {
            (self.cross / denominator).clamp(-1.0, 1.0) as f32
        }
    }
}

/// One opened material: the analysis a lens reads, plus what the open cost
/// and which source bytes it was.
#[derive(Clone, Debug)]
pub struct AnalyzedMaterial {
    pub analysis: Analysis,
    /// The fingerprint of the encoded source bytes, taken while the decoded
    /// image was keyed. Nothing reads the file a second time to learn it.
    pub source_fingerprint: ContentFingerprint,
    pub image_path: PathBuf,
    pub image_bytes: u64,
    /// What the decoder said the source was. Carried in the image header, so
    /// a cache hit names the container and codec as exactly as a decode does.
    pub container: Option<String>,
    pub codec: Option<String>,
    /// True when the open decoded nothing because the image already existed.
    pub cache_hit: bool,
}

/// Decode source audio and publish the immediately useful waveform, spectrum,
/// pulse evidence, and playback PCM without waiting for iterative NMF.
///
/// Every container takes this path. The decode streams into one image under
/// the decoded-material cache and everything here reads that image mapped:
/// the pyramid, the project audio and the sampler share the mapping rather
/// than each holding a copy, and a second open of the same material decodes
/// nothing at all.
pub fn analyze_file_base(path: &Path) -> Result<Analysis> {
    analyze_material(path).map(|material| material.analysis)
}

/// The same open, with the facts an installer needs about it.
pub fn analyze_material(path: &Path) -> Result<AnalyzedMaterial> {
    let opened_at = Instant::now();
    let image = open_material_image(path).with_context(|| format!("opening {}", path.display()))?;
    let sample_rate = image.shape.sample_rate_hz;
    let channels = image.shape.channels;
    let channel_count = usize::from(channels);
    let total_frames = usize::try_from(image.shape.frame_count)
        .context("the decoded material has more frames than this machine can address")?;
    if channel_count == 0 || total_frames == 0 {
        bail!("{} has no audio frames", path.display())
    }

    let waveform_started = Instant::now();
    let samples = image.samples.as_slice();
    let mut accumulators = vec![BinAccumulator::default(); WAVEFORM_BINS];
    let mut mono = Vec::new();
    mono.try_reserve_exact(total_frames)
        .context("the decoded material is too long to project to mono")?;
    for (frame_index, frame) in samples.chunks_exact(channel_count).enumerate() {
        let left = frame[0];
        let right = if channel_count == 1 { left } else { frame[1] };
        mono.push((left + right) * 0.5);
        let bin = (frame_index * WAVEFORM_BINS / total_frames).min(WAVEFORM_BINS - 1);
        accumulators[bin].push(left, right);
    }
    let waveform = accumulators
        .iter()
        .copied()
        .map(BinAccumulator::waveform)
        .collect();
    let waveform_seconds = waveform_started.elapsed().as_secs_f64();

    let pyramid_started = Instant::now();
    let waveform_pyramid = WaveformPyramid::from_samples(image.samples.clone(), channel_count)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let pyramid_seconds = pyramid_started.elapsed().as_secs_f64();

    let spectrum_started = Instant::now();
    let (mut features, spectral_db) = analyze_spectrum(&mono, sample_rate, &accumulators);
    normalize_flux(&mut features);
    let spectrum_seconds = spectrum_started.elapsed().as_secs_f64();

    let rhythm_started = Instant::now();
    let rhythm = analyze_rhythm(&mono, sample_rate);
    let rhythm_seconds = rhythm_started.elapsed().as_secs_f64();
    drop(mono);

    let spectrogram_started = Instant::now();
    let spectral_peak_db = spectral_db.iter().copied().fold(-120.0_f32, f32::max);
    let spectrogram_png = encode_spectrogram(&spectral_db, spectral_peak_db, 84.0)?;
    let spectrogram_seconds = spectrogram_started.elapsed().as_secs_f64();

    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("Untitled");
    let title = stem.rsplit(" - ").next().unwrap_or(stem).to_owned();
    let album = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("Unsorted audio")
        .to_owned();

    // One line per open, so where the wait went is in the app log instead of
    // in a musician's guess.
    eprintln!(
        "audec open phase · {} · fingerprint {:.2}s · decode {:.2}s ({}) · map {:.2}s · \
         waveform {:.2}s · pyramid {:.2}s · spectrum {:.2}s · rhythm {:.2}s · \
         spectrogram {:.2}s · total {:.2}s · image {:.1} MB · {} Hz × {} ch × {} frames",
        path.display(),
        image.fingerprint_seconds,
        image.decode_seconds,
        if image.cache_hit {
            "cache hit"
        } else {
            "decoded"
        },
        image.map_seconds,
        waveform_seconds,
        pyramid_seconds,
        spectrum_seconds,
        rhythm_seconds,
        spectrogram_seconds,
        opened_at.elapsed().as_secs_f64(),
        image.image_bytes() as f64 / (1024.0 * 1024.0),
        sample_rate,
        channels,
        total_frames,
    );

    let analysis = Analysis {
        path: path.to_owned(),
        title,
        album,
        duration_seconds: total_frames as f64 / f64::from(sample_rate),
        sample_rate,
        channels: u32::from(channels),
        bits_per_sample: u32::from(image.facts.bit_depth.unwrap_or(32)),
        waveform,
        waveform_pyramid,
        features,
        rhythm,
        components: None,
        spectral_db,
        spectral_peak_db,
        spectrogram_png,
    };
    Ok(AnalyzedMaterial {
        source_fingerprint: image.source_fingerprint,
        image_path: image.path.clone(),
        container: image.facts.container.clone(),
        codec: image.facts.codec.clone(),
        image_bytes: image.image_bytes(),
        cache_hit: image.cache_hit,
        analysis,
    })
}

/// How many recurring gestures the component question asks for, and how long
/// one gesture may be, when nobody has said otherwise.
///
/// The template length is in atlas frames, and the atlas is
/// [`SPECTROGRAM_WIDTH`] columns over the whole material — about 310 ms a
/// column on a six-minute song, so eight frames is a bar-scale gesture there
/// and a drum stroke on a short selection.
pub fn default_component_params() -> ConvolutionalParams {
    ConvolutionalParams {
        rank: 6,
        template_length: 8,
        iterations: 60,
        activation_sparsity: 0.004,
        ..ConvolutionalParams::default()
    }
}

/// What the K and LAG knobs may ask for. Below two components there is
/// nothing to compare and the answer is the mixture; past sixteen the atlas
/// has more hypotheses than it has distinguishable shapes. A template shorter
/// than two frames is a frozen spectrum, not a gesture, and one longer than
/// thirty-two frames is ten seconds of song on a six-minute atlas — and every
/// added lag is another `SPECTROGRAM_HEIGHT`-tall plane the kernel updates on
/// every iteration.
pub const COMPONENT_RANK_RANGE: std::ops::RangeInclusive<usize> = 2..=16;
pub const COMPONENT_TEMPLATE_LENGTH_RANGE: std::ops::RangeInclusive<usize> = 2..=32;

/// Hold a chosen component question inside what the kernel can answer. The
/// clamp is named once so the header, the socket, and the remembered
/// preference cannot disagree about the bounds.
pub fn clamp_component_params(mut params: ConvolutionalParams) -> ConvolutionalParams {
    params.rank = params
        .rank
        .clamp(*COMPONENT_RANK_RANGE.start(), *COMPONENT_RANK_RANGE.end());
    params.template_length = params.template_length.clamp(
        *COMPONENT_TEMPLATE_LENGTH_RANGE.start(),
        *COMPONENT_TEMPLATE_LENGTH_RANGE.end(),
    );
    params
}

/// Seconds of material one atlas frame stands for: the whole material spread
/// over [`SPECTROGRAM_WIDTH`] columns. A template length is only meaningful
/// to a musician through this.
pub fn atlas_frame_seconds(duration_seconds: f64) -> f64 {
    duration_seconds / SPECTROGRAM_WIDTH as f64
}

/// The fraction of its own peak a component must still be at for a frame to
/// count as part of the stretch it owns.
pub const ACTIVATION_SPAN_FLOOR: f32 = 0.5;

/// The stretch of the atlas one component most owns.
///
/// A convolutional activation is onset-aligned: `activation[f]` is how
/// strongly this gesture *starts* at frame `f`, and the gesture then sounds
/// for `template_length` frames. So an occurrence is counted where the
/// activation is at least [`ACTIVATION_SPAN_FLOOR`] of that component's own
/// peak, it covers `f .. f + template_length`, and the answer is the longest
/// stretch those occurrences cover without a gap. Taking the bare run of
/// above-floor frames instead would report the onset alone — a single atlas
/// column, about 310 ms of a six-minute song — rather than the seconds the
/// gesture is audible in.
///
/// The range is end-exclusive in atlas frames and never past the end of the
/// activation. Ties go to the earliest stretch, so the answer does not move
/// between two readings of the same product; a component with no positive
/// activation owns nothing and gets `None`.
///
/// The floor is relative to the component, not to the mixture: a quiet
/// recurring shape names its own seconds rather than being outranked by a
/// loud one.
pub fn strongest_activation_span(
    activation: &[f32],
    template_length: usize,
) -> Option<std::ops::Range<usize>> {
    let peak = activation
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .fold(0.0_f32, f32::max);
    if peak <= 0.0 {
        return None;
    }
    let floor = peak * ACTIVATION_SPAN_FLOOR;
    let footprint = template_length.max(1);
    let mut best: Option<std::ops::Range<usize>> = None;
    let mut current: Option<std::ops::Range<usize>> = None;
    let close = |span: std::ops::Range<usize>, best: &mut Option<std::ops::Range<usize>>| {
        if best
            .as_ref()
            .is_none_or(|held| span.end - span.start > held.end - held.start)
        {
            *best = Some(span);
        }
    };
    for (index, value) in activation.iter().enumerate() {
        if !value.is_finite() || *value < floor {
            continue;
        }
        let reach = (index + footprint).min(activation.len());
        current = Some(match current.take() {
            // Adjacent counts as touching: an occurrence starting exactly
            // where the previous one stops leaves no silence between them.
            Some(span) if index <= span.end => span.start..reach.max(span.end),
            Some(span) => {
                close(span, &mut best);
                index..reach
            }
            None => index..reach,
        });
    }
    if let Some(span) = current {
        close(span, &mut best);
    }
    best
}

/// Compute the deferred recurring-component product from the exact atlas
/// carried by a base analysis. This never decodes or reprojects source media.
pub fn factor_analysis_components(analysis: &Analysis) -> Result<ComponentDecomposition> {
    factor_analysis_components_cancellable(
        analysis,
        default_component_params(),
        &DecompositionCancellation::default(),
    )
}

pub fn factor_analysis_components_cancellable(
    analysis: &Analysis,
    params: ConvolutionalParams,
    cancellation: &DecompositionCancellation,
) -> Result<ComponentDecomposition> {
    let component_matrix = component_input(&analysis.spectral_db, analysis.spectral_peak_db);
    // Convolutional templates: each component is a recurring gesture over
    // `template_length` spectrogram frames (a bar-scale pattern over a whole
    // song, a drum stroke over a short selection), not one frozen spectrum.
    decompose_convolutional_cancellable(
        &component_matrix,
        SPECTROGRAM_HEIGHT,
        SPECTROGRAM_WIDTH,
        clamp_component_params(params),
        cancellation,
    )
    .context("factoring recurring spectral gestures")
}

/// Compatibility full analysis for headless callers and deterministic tests.
pub fn analyze_file(path: &Path) -> Result<Analysis> {
    let mut analysis = analyze_file_base(path)?;
    analysis.components = Some(factor_analysis_components(&analysis)?);
    Ok(analysis)
}

/// Convert the column-major dB display field to a row-major normalized linear
/// magnitude matrix for NMF. Keeping this conversion explicit avoids feeding
/// colors or logarithmic pixel intensities into a component model.
fn component_input(spectral_db: &[f32], peak_db: f32) -> Vec<f32> {
    let mut matrix = vec![0.0; SPECTROGRAM_HEIGHT * SPECTROGRAM_WIDTH];
    for frame in 0..SPECTROGRAM_WIDTH {
        for frequency in 0..SPECTROGRAM_HEIGHT {
            let db = spectral_db[frame * SPECTROGRAM_HEIGHT + frequency];
            let relative_db = (db - peak_db).clamp(-120.0, 0.0);
            matrix[frequency * SPECTROGRAM_WIDTH + frame] = 10.0_f32.powf(relative_db / 20.0);
        }
    }
    matrix
}

fn analyze_rhythm(mono: &[f32], sample_rate: u32) -> RhythmAnalysis {
    const TARGET_FRAME_RATE: usize = 100;
    const MIN_BPM: f32 = 65.0;
    const MAX_BPM: f32 = 190.0;

    if mono.is_empty() || sample_rate == 0 {
        return RhythmAnalysis::default();
    }

    let hop = (sample_rate as usize / TARGET_FRAME_RATE).max(1);
    let frame_rate = sample_rate as f32 / hop as f32;
    let frame_count = mono.len().div_ceil(hop);
    let low_alpha = 1.0 - (-2.0 * std::f32::consts::PI * 180.0 / sample_rate as f32).exp();
    let high_alpha = 1.0 - (-2.0 * std::f32::consts::PI * 2_200.0 / sample_rate as f32).exp();
    let mut low_state = 0.0_f32;
    let mut high_state = 0.0_f32;
    let mut energies = vec![[0.0_f32; 3]; frame_count];

    for (frame, samples) in mono.chunks(hop).enumerate() {
        let mut squares = [0.0_f64; 3];
        for sample in samples.iter().copied() {
            low_state += low_alpha * (sample - low_state);
            high_state += high_alpha * (sample - high_state);
            let bands = [low_state, high_state - low_state, sample - high_state];
            for (sum, band) in squares.iter_mut().zip(bands) {
                *sum += f64::from(band * band);
            }
        }
        for (energy, sum) in energies[frame].iter_mut().zip(squares) {
            let rms = (sum / samples.len().max(1) as f64).sqrt() as f32;
            *energy = (1.0 + 32.0 * rms).ln();
        }
    }

    let mut band_flux = vec![[0.0_f32; 3]; frame_count];
    let mut onset_envelope = vec![0.0_f32; frame_count];
    for frame in 1..frame_count {
        for band in 0..3 {
            band_flux[frame][band] = (energies[frame][band] - energies[frame - 1][band]).max(0.0);
        }
        onset_envelope[frame] =
            band_flux[frame][0] * 0.9 + band_flux[frame][1] + band_flux[frame][2] * 0.85;
    }

    // A moving mean follows dense sustained modulation too eagerly and caused
    // the old detector to emit almost exactly one event per refractory window
    // on compressed electronic material. A local median/MAD gate treats that
    // modulation as background while retaining attacks that are exceptional
    // in their immediate production context.
    let threshold_radius = (frame_rate * 0.30).round().max(2.0) as usize;
    let mut nonzero_novelty: Vec<f32> = onset_envelope
        .iter()
        .copied()
        .filter(|value| *value > 0.0 && value.is_finite())
        .collect();
    nonzero_novelty.sort_by(f32::total_cmp);
    let global_floor = percentile(&nonzero_novelty, 0.70) * 0.12;
    let mut salience = vec![0.0_f32; frame_count];
    for frame in 0..frame_count {
        let start = frame.saturating_sub(threshold_radius);
        let end = (frame + threshold_radius + 1).min(frame_count);
        let mut neighborhood = onset_envelope[start..end].to_vec();
        neighborhood.sort_by(f32::total_cmp);
        let median = percentile(&neighborhood, 0.50);
        for value in &mut neighborhood {
            *value = (*value - median).abs();
        }
        neighborhood.sort_by(f32::total_cmp);
        let mad = percentile(&neighborhood, 0.50);
        let threshold = median + 3.0 * mad + global_floor;
        let margin = (onset_envelope[frame] - threshold).max(0.0);
        salience[frame] = margin / (threshold + mad + 1.0e-7);
    }
    normalize_values(&mut salience);

    let peak_radius = (frame_rate * 0.020).round().max(1.0) as usize;
    let refractory = (frame_rate * 0.025).round().max(1.0) as usize;
    let mut onsets = Vec::new();
    let mut last_peak = None;
    for frame in peak_radius..frame_count.saturating_sub(peak_radius) {
        let strength = salience[frame];
        if strength < 0.12
            || salience[frame - peak_radius..=frame + peak_radius]
                .iter()
                .any(|candidate| *candidate > strength)
            || last_peak.is_some_and(|last| frame - last < refractory)
        {
            continue;
        }
        let bands = band_flux[frame];
        let total = bands.iter().sum::<f32>().max(1.0e-8);
        onsets.push(OnsetEvent {
            time_seconds: frame as f64 / f64::from(frame_rate),
            strength,
            low: bands[0] / total,
            mid: bands[1] / total,
            high: bands[2] / total,
            cluster: 0,
            template_similarity: 0.0,
        });
        last_peak = Some(frame);
    }

    let min_lag = (frame_rate * 60.0 / MAX_BPM).round().max(1.0) as usize;
    let max_lag = (frame_rate * 60.0 / MIN_BPM).round().max(min_lag as f32) as usize;
    let correlations: Vec<f32> = (min_lag..=max_lag)
        .map(|lag| normalized_autocorrelation(&salience, lag))
        .collect();
    let (best_offset, best_score) = correlations
        .iter()
        .copied()
        .enumerate()
        .max_by(|(_, left), (_, right)| left.total_cmp(right))
        .unwrap_or((0, 0.0));
    let best_lag = min_lag + best_offset;
    let tempo_bpm = 60.0 * frame_rate / best_lag as f32;
    let mean_score = correlations.iter().sum::<f32>() / correlations.len().max(1) as f32;
    let pulse_contrast =
        ((best_score - mean_score) / (1.0 - mean_score).max(1.0e-6)).clamp(0.0, 1.0);

    let mut best_phase = 0;
    let mut best_phase_score = -1.0_f32;
    for phase in 0..best_lag {
        let mut score = 0.0;
        let mut count = 0;
        for center in (phase..frame_count).step_by(best_lag) {
            let start = center.saturating_sub(2);
            let end = (center + 2).min(frame_count - 1);
            score += salience[start..=end]
                .iter()
                .copied()
                .fold(0.0_f32, f32::max);
            count += 1;
        }
        score /= count.max(1) as f32;
        if score > best_phase_score {
            best_phase = phase;
            best_phase_score = score;
        }
    }
    let beat_times = (best_phase..frame_count)
        .step_by(best_lag)
        .map(|frame| frame as f64 / f64::from(frame_rate))
        .collect();

    let event_clusters = cluster_events(&mut onsets, mono, sample_rate);

    RhythmAnalysis {
        tempo_bpm,
        pulse_contrast,
        beat_times,
        onsets,
        event_clusters,
    }
}

fn cluster_events(onsets: &mut [OnsetEvent], mono: &[f32], sample_rate: u32) -> Vec<EventCluster> {
    const FFT_SIZE: usize = 4_096;
    const FINGERPRINT_BINS: usize = 40;
    const MAX_CLUSTERS: usize = 8;

    if onsets.is_empty() {
        return Vec::new();
    }

    let frequencies: Vec<f32> = (0..FINGERPRINT_BINS)
        .map(|index| {
            let fraction = index as f32 / (FINGERPRINT_BINS - 1) as f32;
            45.0 * (16_000.0_f32 / 45.0).powf(fraction)
        })
        .collect();
    let half_step = (16_000.0_f32 / 45.0).powf(0.5 / (FINGERPRINT_BINS - 1) as f32);
    let ranges: Vec<(usize, usize)> = frequencies
        .iter()
        .map(|frequency| {
            let low =
                ((frequency / half_step) * FFT_SIZE as f32 / sample_rate as f32).floor() as usize;
            let high =
                ((frequency * half_step) * FFT_SIZE as f32 / sample_rate as f32).ceil() as usize;
            let low = low.clamp(1, FFT_SIZE / 2 - 1);
            let high = high.clamp(low + 1, FFT_SIZE / 2);
            (low, high)
        })
        .collect();
    let window: Vec<f32> = (0..FFT_SIZE)
        .map(|index| {
            let phase = std::f32::consts::PI * index as f32 / FFT_SIZE as f32;
            phase.sin().powi(2)
        })
        .collect();
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let mut input = vec![Complex::default(); FFT_SIZE];
    let mut magnitudes = vec![0.0_f32; FFT_SIZE / 2];
    let mut fingerprints = Vec::with_capacity(onsets.len());

    for onset in onsets.iter() {
        let center = (onset.time_seconds * f64::from(sample_rate)).round() as isize;
        let start = center - 256;
        for (index, point) in input.iter_mut().enumerate() {
            let source = start + index as isize;
            point.re = if source >= 0 && (source as usize) < mono.len() {
                mono[source as usize] * window[index]
            } else {
                0.0
            };
            point.im = 0.0;
        }
        fft.process(&mut input);
        for (magnitude, point) in magnitudes.iter_mut().zip(&input) {
            *magnitude = point.norm() / FFT_SIZE as f32;
        }
        let mut fingerprint = vec![0.0_f32; FINGERPRINT_BINS];
        for (value, (low, high)) in fingerprint.iter_mut().zip(ranges.iter().copied()) {
            let magnitude = magnitudes[low..high]
                .iter()
                .copied()
                .fold(0.0_f32, f32::max);
            *value = (1.0 + 256.0 * magnitude).ln();
        }
        normalize_vector(&mut fingerprint);
        fingerprints.push(fingerprint);
    }

    let cluster_count = ((onsets.len() as f32 / 28.0).sqrt().round() as usize)
        .clamp(1, MAX_CLUSTERS)
        .min(onsets.len());
    let mut centroids = Vec::with_capacity(cluster_count);
    let first = onsets
        .iter()
        .enumerate()
        .max_by(|(_, left), (_, right)| left.strength.total_cmp(&right.strength))
        .map(|(index, _)| index)
        .unwrap_or(0);
    centroids.push(fingerprints[first].clone());
    while centroids.len() < cluster_count {
        let next = fingerprints
            .iter()
            .enumerate()
            .max_by(|(_, left), (_, right)| {
                let left_distance = centroids
                    .iter()
                    .map(|center| 1.0 - dot(left, center))
                    .fold(f32::INFINITY, f32::min);
                let right_distance = centroids
                    .iter()
                    .map(|center| 1.0 - dot(right, center))
                    .fold(f32::INFINITY, f32::min);
                left_distance.total_cmp(&right_distance)
            })
            .map(|(index, _)| index)
            .unwrap_or(0);
        centroids.push(fingerprints[next].clone());
    }

    let mut assignments = vec![0_usize; onsets.len()];
    for _ in 0..14 {
        for (assignment, fingerprint) in assignments.iter_mut().zip(&fingerprints) {
            *assignment = centroids
                .iter()
                .enumerate()
                .max_by(|(_, left), (_, right)| {
                    dot(fingerprint, left).total_cmp(&dot(fingerprint, right))
                })
                .map(|(index, _)| index)
                .unwrap_or(0);
        }
        let mut next = vec![vec![0.0_f32; FINGERPRINT_BINS]; cluster_count];
        let mut weights = vec![0.0_f32; cluster_count];
        for ((fingerprint, assignment), onset) in
            fingerprints.iter().zip(&assignments).zip(onsets.iter())
        {
            let weight = onset.strength.max(0.1);
            weights[*assignment] += weight;
            for (sum, value) in next[*assignment].iter_mut().zip(fingerprint) {
                *sum += value * weight;
            }
        }
        for (index, centroid) in next.iter_mut().enumerate() {
            if weights[index] > 0.0 {
                normalize_vector(centroid);
                centroids[index].clone_from(centroid);
            }
        }
    }

    let mut order: Vec<usize> = (0..cluster_count).collect();
    order.sort_by(|left, right| {
        spectral_centroid(&centroids[*left], &frequencies)
            .total_cmp(&spectral_centroid(&centroids[*right], &frequencies))
    });
    let mut remap = vec![0_usize; cluster_count];
    for (new, old) in order.iter().copied().enumerate() {
        remap[old] = new;
    }
    for ((onset, fingerprint), assignment) in onsets
        .iter_mut()
        .zip(&fingerprints)
        .zip(assignments.iter().copied())
    {
        onset.cluster = remap[assignment];
        onset.template_similarity = dot(fingerprint, &centroids[assignment]).clamp(0.0, 1.0);
    }

    order
        .into_iter()
        .enumerate()
        .map(|(index, old)| {
            let members: Vec<&OnsetEvent> = onsets
                .iter()
                .filter(|onset| onset.cluster == index)
                .collect();
            let centroid_hz = spectral_centroid(&centroids[old], &frequencies);
            let consistency = members
                .iter()
                .map(|onset| onset.template_similarity)
                .sum::<f32>()
                / members.len().max(1) as f32;
            EventCluster {
                label: event_cluster_label(index),
                event_count: members.len(),
                centroid_hz,
                consistency,
                spectrum: centroids[old].clone(),
            }
        })
        .collect()
}

fn normalize_vector(values: &mut [f32]) {
    let norm = values
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .sqrt()
        .max(1.0e-8);
    for value in values {
        *value /= norm;
    }
}

fn dot(left: &[f32], right: &[f32]) -> f32 {
    left.iter()
        .zip(right)
        .map(|(left, right)| left * right)
        .sum()
}

fn spectral_centroid(spectrum: &[f32], frequencies: &[f32]) -> f32 {
    let total = spectrum.iter().sum::<f32>().max(1.0e-8);
    spectrum
        .iter()
        .zip(frequencies)
        .map(|(magnitude, frequency)| magnitude * frequency)
        .sum::<f32>()
        / total
}

fn event_cluster_label(index: usize) -> String {
    format!("Cluster {}", (b'A' + index as u8) as char)
}

fn normalized_autocorrelation(values: &[f32], lag: usize) -> f32 {
    if lag == 0 || lag >= values.len() {
        return 0.0;
    }
    let mut product = 0.0_f64;
    let mut left_energy = 0.0_f64;
    let mut right_energy = 0.0_f64;
    for index in lag..values.len() {
        let left = f64::from(values[index]);
        let right = f64::from(values[index - lag]);
        product += left * right;
        left_energy += left * left;
        right_energy += right * right;
    }
    if left_energy == 0.0 || right_energy == 0.0 {
        0.0
    } else {
        (product / (left_energy * right_energy).sqrt()) as f32
    }
}

fn normalize_values(values: &mut [f32]) {
    let mut sorted = values.to_vec();
    sorted.sort_by(f32::total_cmp);
    let ceiling = sorted
        .get((sorted.len() as f32 * 0.98) as usize)
        .copied()
        .unwrap_or(1.0)
        .max(1.0e-8);
    for value in values {
        *value = (*value / ceiling).clamp(0.0, 1.0);
    }
}

fn percentile(sorted_values: &[f32], quantile: f32) -> f32 {
    if sorted_values.is_empty() {
        return 0.0;
    }
    let index = (quantile.clamp(0.0, 1.0) * (sorted_values.len() - 1) as f32).round() as usize;
    sorted_values[index]
}

fn analyze_spectrum(
    mono: &[f32],
    sample_rate: u32,
    accumulators: &[BinAccumulator],
) -> (Vec<FeatureFrame>, Vec<f32>) {
    // Large enough to keep bass events legible while remaining interactive for a
    // whole-song overview. A future zoomed analysis can use longer windows.
    const FFT_SIZE: usize = 8_192;

    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let mut input = vec![Complex::default(); FFT_SIZE];
    let window: Vec<f32> = (0..FFT_SIZE)
        .map(|index| {
            let phase = std::f32::consts::PI * index as f32 / FFT_SIZE as f32;
            phase.sin().powi(2)
        })
        .collect();
    let frequencies: Vec<f32> = (0..SPECTROGRAM_HEIGHT)
        .map(|index| {
            let fraction = index as f32 / (SPECTROGRAM_HEIGHT - 1) as f32;
            MIN_FREQUENCY * (MAX_FREQUENCY / MIN_FREQUENCY).powf(fraction)
        })
        .collect();
    let half_step = (MAX_FREQUENCY / MIN_FREQUENCY).powf(0.5 / (SPECTROGRAM_HEIGHT - 1) as f32);
    let band_ranges: Vec<(usize, usize)> = frequencies
        .iter()
        .map(|frequency| {
            let low =
                ((frequency / half_step) * FFT_SIZE as f32 / sample_rate as f32).floor() as usize;
            let high =
                ((frequency * half_step) * FFT_SIZE as f32 / sample_rate as f32).ceil() as usize;
            let low = low.clamp(1, FFT_SIZE / 2 - 1);
            let high = high.clamp(low + 1, FFT_SIZE / 2);
            (low, high)
        })
        .collect();
    let mut result = vec![-120.0; SPECTROGRAM_WIDTH * SPECTROGRAM_HEIGHT];
    let mut features = vec![FeatureFrame::default(); SPECTROGRAM_WIDTH];
    let mut previous_bands = vec![0.0_f32; SPECTROGRAM_HEIGHT];
    let mut magnitudes = vec![0.0_f32; FFT_SIZE / 2];

    for column in 0..SPECTROGRAM_WIDTH {
        let center = column * mono.len().saturating_sub(1) / SPECTROGRAM_WIDTH.saturating_sub(1);
        let start = center as isize - FFT_SIZE as isize / 2;
        for (index, point) in input.iter_mut().enumerate() {
            let source_index = start + index as isize;
            point.re = if source_index >= 0 && (source_index as usize) < mono.len() {
                mono[source_index as usize] * window[index]
            } else {
                0.0
            };
            point.im = 0.0;
        }
        fft.process(&mut input);

        for (magnitude, point) in magnitudes.iter_mut().zip(&input) {
            *magnitude = point.norm() / FFT_SIZE as f32;
        }
        let mut weighted_frequency = 0.0;
        let mut magnitude_sum = 0.0;
        for (index, magnitude) in magnitudes.iter().copied().enumerate().skip(1) {
            let frequency = index as f32 * sample_rate as f32 / FFT_SIZE as f32;
            weighted_frequency += frequency * magnitude;
            magnitude_sum += magnitude;
        }
        let centroid = if magnitude_sum > 0.0 {
            weighted_frequency / magnitude_sum
        } else {
            MIN_FREQUENCY
        };

        let mut strongest = (0.0_f32, MIN_FREQUENCY);
        let mut flux = 0.0;
        for (band, (frequency, (low_bin, high_bin))) in frequencies
            .iter()
            .copied()
            .zip(band_ranges.iter().copied())
            .enumerate()
        {
            let magnitude = magnitudes[low_bin..high_bin]
                .iter()
                .copied()
                .fold(0.0_f32, f32::max);
            if magnitude > strongest.0 {
                strongest = (magnitude, frequency);
            }
            flux += (magnitude - previous_bands[band]).max(0.0);
            previous_bands[band] = magnitude;
            result[column * SPECTROGRAM_HEIGHT + band] = 20.0 * magnitude.max(1.0e-8).log10();
        }

        let source_bin = column * accumulators.len() / SPECTROGRAM_WIDTH;
        let accumulator = accumulators[source_bin.min(accumulators.len() - 1)];
        features[column] = FeatureFrame {
            loudness: accumulator.loudness(),
            brightness: ((centroid / MIN_FREQUENCY).ln() / (MAX_FREQUENCY / MIN_FREQUENCY).ln())
                .clamp(0.0, 1.0),
            flux,
            stereo_width: accumulator.stereo_width().clamp(0.0, 1.0),
            correlation: accumulator.correlation(),
            dominant_hz: strongest.1,
        };
    }

    (features, result)
}

/// The spectral field for the lens's chosen transform, in the same
/// column-major `SPECTROGRAM_WIDTH x SPECTROGRAM_HEIGHT` dB layout.
///
/// One algorithm: the windowed forms in `spectral_tiles` are the projection,
/// and PCM already in memory is one more window source. The whole-slice
/// bodies that used to live here are the test oracle those forms are proved
/// against (`spectral_tiles::tests`).
pub fn spectral_field(
    mono: &[f32],
    sample_rate: u32,
    settings: SpectrumSettings,
) -> Result<Vec<f32>, crate::cqt::CqtError> {
    crate::spectral_tiles::display_field_streamed(
        SPECTROGRAM_WIDTH,
        SPECTROGRAM_HEIGHT,
        mono.len(),
        sample_rate,
        settings,
        &mut whole_slice_reader(mono),
    )
}

/// Constant-Q field on the display's log-frequency bands: one analysis
/// window per quarter-tone, frames centred on each display column.
pub fn constant_q_projection(
    mono: &[f32],
    sample_rate: u32,
    settings: SpectrumSettings,
) -> Result<Vec<f32>, crate::cqt::CqtError> {
    crate::spectral_tiles::constant_q_display_field_streamed(
        SPECTROGRAM_WIDTH,
        SPECTROGRAM_HEIGHT,
        mono.len(),
        sample_rate,
        settings,
        &mut whole_slice_reader(mono),
    )
}

/// FFT field on the display's log-frequency bands: one centred FFT per
/// display column, each band the peak of the bins it spans.
pub fn spectral_projection(mono: &[f32], sample_rate: u32, settings: SpectrumSettings) -> Vec<f32> {
    crate::spectral_tiles::fft_display_field_streamed(
        SPECTROGRAM_WIDTH,
        SPECTROGRAM_HEIGHT,
        mono.len(),
        sample_rate,
        settings,
        &mut whole_slice_reader(mono),
    )
}

/// A window reader over PCM already in memory, under the append contract
/// `Analysis::mono_range_into` keeps: a read past the end appends what exists
/// and the field zero-pads the rest.
fn whole_slice_reader(
    mono: &[f32],
) -> impl FnMut(crate::spectral_tiles::FrameRange, &mut Vec<f32>) + '_ {
    move |range, out| {
        let start = (range.start as usize).min(mono.len());
        let end = (range.end as usize).clamp(start, mono.len());
        out.extend_from_slice(&mono[start..end]);
    }
}

fn normalize_flux(features: &mut [FeatureFrame]) {
    let mut values: Vec<f32> = features.iter().map(|feature| feature.flux).collect();
    values.sort_by(f32::total_cmp);
    let ceiling = values
        .get((values.len() as f32 * 0.98) as usize)
        .copied()
        .unwrap_or(1.0)
        .max(1.0e-8);
    for feature in features {
        feature.flux = (feature.flux / ceiling).clamp(0.0, 1.0);
    }
}

pub fn encode_spectrogram(values: &[f32], db_ceiling: f32, db_range: f32) -> Result<Vec<u8>> {
    encode_spectrogram_field(
        values,
        SPECTROGRAM_WIDTH,
        SPECTROGRAM_HEIGHT,
        db_ceiling,
        db_range,
    )
}

/// Colorize and encode a column-major, low-frequency-first dB field with
/// dynamic dimensions. Visible-range spectral tiles use this instead of
/// enlarging the fixed whole-material atlas.
pub fn encode_spectrogram_field(
    values: &[f32],
    width: usize,
    height: usize,
    db_ceiling: f32,
    db_range: f32,
) -> Result<Vec<u8>> {
    if width == 0 || height == 0 || values.len() != width.saturating_mul(height) {
        bail!(
            "spectrogram field has {} values; expected {}×{}",
            values.len(),
            width,
            height
        );
    }
    let db_ceiling = if db_ceiling.is_finite() {
        db_ceiling
    } else {
        0.0
    };
    let db_range = if db_range.is_finite() {
        db_range.clamp(1.0, 240.0)
    } else {
        84.0
    };
    let floor = db_ceiling - db_range;
    let mut pixels = vec![0_u8; width * height * 3];
    for row in 0..height {
        let band = height - row - 1;
        for column in 0..width {
            let db = values[column * height + band];
            let intensity = ((db - floor) / db_range).clamp(0.0, 1.0);
            let color = spectral_color(intensity);
            let offset = (row * width + column) * 3;
            pixels[offset..offset + 3].copy_from_slice(&color);
        }
    }

    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(
            BufWriter::new(&mut bytes),
            u32::try_from(width).context("spectrogram width exceeds PNG limits")?,
            u32::try_from(height).context("spectrogram height exceeds PNG limits")?,
        );
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().context("starting spectrogram PNG")?;
        writer
            .write_image_data(&pixels)
            .context("encoding spectrogram PNG")?;
    }
    Ok(bytes)
}

fn spectral_color(value: f32) -> [u8; 3] {
    const STOPS: &[(f32, [u8; 3])] = &[
        (0.00, [5, 8, 18]),
        (0.16, [14, 24, 58]),
        (0.34, [41, 35, 92]),
        (0.52, [102, 42, 116]),
        (0.70, [194, 63, 105]),
        (0.86, [244, 140, 76]),
        (1.00, [255, 235, 173]),
    ];
    for pair in STOPS.windows(2) {
        let (start_at, start) = pair[0];
        let (end_at, end) = pair[1];
        if value <= end_at {
            let amount = ((value - start_at) / (end_at - start_at)).clamp(0.0, 1.0);
            return [
                lerp_u8(start[0], end[0], amount),
                lerp_u8(start[1], end[1], amount),
                lerp_u8(start[2], end[2], amount),
            ];
        }
    }
    STOPS.last().unwrap().1
}

fn lerp_u8(start: u8, end: u8, amount: f32) -> u8 {
    (start as f32 + (end as f32 - start as f32) * amount).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spectral_palette_has_distinct_endpoints() {
        assert_eq!(spectral_color(0.0), [5, 8, 18]);
        assert_eq!(spectral_color(1.0), [255, 235, 173]);
        assert_ne!(spectral_color(0.5), spectral_color(0.75));
    }

    #[test]
    fn silent_accumulator_is_well_behaved() {
        let accumulator = BinAccumulator::default();
        assert_eq!(accumulator.loudness(), 0.0);
        assert_eq!(accumulator.stereo_width(), 0.0);
        assert_eq!(accumulator.correlation(), 1.0);
    }

    #[test]
    fn recovers_a_simple_pulse_train() {
        let sample_rate = 8_000;
        let mut signal = vec![0.0_f32; sample_rate as usize * 8];
        for pulse in (0..signal.len()).step_by(sample_rate as usize / 2) {
            signal[pulse] = 1.0;
        }
        let rhythm = analyze_rhythm(&signal, sample_rate);
        assert!((rhythm.tempo_bpm - 120.0).abs() < 3.0, "{rhythm:?}");
        assert!(rhythm.beat_times.len() >= 14, "{rhythm:?}");
        assert!((14..=18).contains(&rhythm.onsets.len()), "{rhythm:?}");
    }

    #[test]
    fn compressed_modulation_does_not_fill_every_refractory_slot() {
        let sample_rate = 8_000;
        let seconds = 8;
        let signal = (0..sample_rate as usize * seconds)
            .map(|sample| {
                let time = sample as f32 / sample_rate as f32;
                let carrier = (std::f32::consts::TAU * 220.0 * time).sin();
                let modulation = 0.55 + 0.40 * (std::f32::consts::TAU * 3.0 * time).sin();
                (carrier * modulation).tanh()
            })
            .collect::<Vec<_>>();
        let rhythm = analyze_rhythm(&signal, sample_rate);
        assert!(
            rhythm.onsets.len() < seconds * 8,
            "modulation produced implausibly dense onsets: {rhythm:?}"
        );
    }

    #[test]
    fn constant_q_field_resolves_a_low_tone_where_the_fft_field_smears_it() {
        let sample_rate = 44_100_u32;
        let tone_hz = 55.0_f32;
        let mono: Vec<f32> = (0..sample_rate as usize * 2)
            .map(|index| {
                (2.0 * std::f32::consts::PI * tone_hz * index as f32 / sample_rate as f32).sin()
                    * 0.5
            })
            .collect();
        let settings = SpectrumSettings {
            fft_size: 4_096,
            hop_size: 1_024,
            ..SpectrumSettings::default()
        };
        let cqt = constant_q_projection(&mono, sample_rate, settings).unwrap();
        assert_eq!(cqt.len(), SPECTROGRAM_WIDTH * SPECTROGRAM_HEIGHT);
        let fft = spectral_projection(&mono, sample_rate, settings);
        let column = SPECTROGRAM_WIDTH / 2;
        let band_frequency = |band: usize| {
            let fraction = band as f32 / (SPECTROGRAM_HEIGHT - 1) as f32;
            MIN_FREQUENCY * (MAX_FREQUENCY / MIN_FREQUENCY).powf(fraction)
        };
        let peak_band = |field: &[f32]| {
            (0..SPECTROGRAM_HEIGHT)
                .max_by(|&a, &b| {
                    field[column * SPECTROGRAM_HEIGHT + a]
                        .total_cmp(&field[column * SPECTROGRAM_HEIGHT + b])
                })
                .unwrap()
        };
        let cqt_peak = peak_band(&cqt);
        assert!(
            (band_frequency(cqt_peak) / tone_hz).log2().abs() < 0.05,
            "CQT peak at {} Hz",
            band_frequency(cqt_peak)
        );
        let width = |field: &[f32], peak: usize| {
            let level = field[column * SPECTROGRAM_HEIGHT + peak];
            (0..SPECTROGRAM_HEIGHT)
                .filter(|&b| field[column * SPECTROGRAM_HEIGHT + b] >= level - 6.0)
                .count()
        };
        let fft_peak = peak_band(&fft);
        assert!(
            width(&cqt, cqt_peak) <= width(&fft, fft_peak),
            "cqt width {} vs fft width {}",
            width(&cqt, cqt_peak),
            width(&fft, fft_peak)
        );
    }

    /// The rule the components lens states in its status line, on activations
    /// whose answer can be read off by eye.
    #[test]
    fn the_strongest_activation_span_is_the_longest_run_above_half_the_peak() {
        // A one-frame template is the bare run of above-floor frames. Peak
        // 1.0, so the floor is 0.5: frames 1..3 (length 2) and frames 5..9
        // (length 4). The longer one wins even though the shorter one holds
        // the single loudest frame.
        let activation = [0.1, 0.9, 1.0, 0.2, 0.0, 0.6, 0.7, 0.6, 0.55, 0.1];
        assert_eq!(strongest_activation_span(&activation, 1), Some(5..9));

        // A stretch that reaches the end of the atlas is not truncated.
        assert_eq!(
            strongest_activation_span(&[0.0, 0.1, 1.0, 0.9], 1),
            Some(2..4)
        );

        // Ties go to the earliest stretch, so two readings of the same product
        // name the same seconds.
        assert_eq!(
            strongest_activation_span(&[1.0, 0.0, 0.8, 0.0], 1),
            Some(0..1)
        );

        // The floor is relative to this component, not to the mixture: a
        // component that never rises above 0.002 still owns its own peak.
        assert_eq!(
            strongest_activation_span(&[0.0, 0.002, 0.0018, 0.0], 1),
            Some(1..3)
        );

        // Silence owns nothing, and neither does an empty product.
        assert_eq!(strongest_activation_span(&[0.0, 0.0, 0.0], 1), None);
        assert_eq!(strongest_activation_span(&[], 1), None);
        assert_eq!(strongest_activation_span(&[f32::NAN, 0.0], 1), None);

        // One onset of an eight-frame gesture owns eight frames, not one.
        let single = [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        assert_eq!(strongest_activation_span(&single, 8), Some(1..9));
        // And it never runs past the material.
        assert_eq!(strongest_activation_span(&[0.0, 0.0, 1.0], 8), Some(2..3));

        // Occurrences whose footprints touch are one stretch; a gap wider
        // than the gesture is two, and the longer one wins.
        //         0    1    2    3    4    5    6    7    8    9   10   11
        let dense = [1.0, 0.0, 0.0, 0.0, 0.0, 0.9, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        // With a three-frame gesture: 0..3, then 5..8 merged with 7..10.
        assert_eq!(strongest_activation_span(&dense, 3), Some(5..10));
        // With a six-frame gesture every occurrence touches the next: one
        // stretch from the first onset to six frames past the last.
        assert_eq!(strongest_activation_span(&dense, 6), Some(0..12));
    }

    #[test]
    fn the_component_question_is_held_inside_what_the_kernel_answers() {
        let asked = ConvolutionalParams {
            rank: 99,
            template_length: 1,
            ..default_component_params()
        };
        let held = clamp_component_params(asked);
        assert_eq!(held.rank, *COMPONENT_RANK_RANGE.end());
        assert_eq!(
            held.template_length,
            *COMPONENT_TEMPLATE_LENGTH_RANGE.start()
        );
        // Nothing else about the question is rewritten by the clamp.
        assert_eq!(held.iterations, asked.iterations);
        assert_eq!(held.activation_sparsity, asked.activation_sparsity);
        assert_eq!(held.seed, asked.seed);

        // The default is inside its own bounds, so the app never opens on a
        // question it would refuse.
        let default = default_component_params();
        assert_eq!(clamp_component_params(default), default);
        assert_eq!(default.rank, 6);
        assert_eq!(default.template_length, 8);
    }

    /// A template length only means something to a musician in seconds, and
    /// that conversion is the atlas, not the FFT.
    #[test]
    fn a_template_length_is_read_in_seconds_of_this_material() {
        let six_minutes = 373.0_f64;
        let column = atlas_frame_seconds(six_minutes);
        assert!(
            (column - six_minutes / SPECTROGRAM_WIDTH as f64).abs() < 1.0e-12,
            "{column}"
        );
        assert!((0.30..0.32).contains(&column), "a column is {column} s");
        assert!(
            (2.4..2.6).contains(&(column * 8.0)),
            "eight frames is {} s",
            column * 8.0
        );
    }
}
