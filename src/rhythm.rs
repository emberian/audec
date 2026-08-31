//! Deterministic rhythm and event deprojection for mixed electronic music.
//!
//! This module reports observations and recurring event-family hypotheses.  It
//! deliberately does not call them instruments or stems: a family can just as
//! easily be a layered hit, a production gesture, or two sounds that recur
//! together.

use std::cmp::Ordering;

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

const EPSILON: f32 = 1.0e-12;

#[derive(Clone, Debug, PartialEq)]
pub struct RhythmConfig {
    pub fft_size: usize,
    pub hop_size: usize,
    pub log_band_count: usize,
    pub minimum_frequency_hz: f32,
    pub maximum_frequency_hz: f32,
    /// Radius of the frequency maximum applied to the preceding spectrum.
    pub spectral_max_radius: usize,
    pub threshold_window_seconds: f32,
    pub threshold_mad_multiplier: f32,
    /// Fraction of the file's maximum novelty used as the minimum threshold.
    pub threshold_floor: f32,
    pub minimum_hit_spacing_seconds: f32,
    pub maximum_span_seconds: f32,
    pub tempo_min_bpm: f32,
    pub tempo_max_bpm: f32,
    pub tempo_hypotheses: usize,
    pub phase_hypotheses_per_tempo: usize,
    pub family_similarity_threshold: f32,
    pub maximum_families: usize,
    pub maximum_patterns: usize,
}

impl Default for RhythmConfig {
    fn default() -> Self {
        Self {
            fft_size: 2_048,
            hop_size: 256,
            log_band_count: 48,
            minimum_frequency_hz: 30.0,
            maximum_frequency_hz: 18_000.0,
            spectral_max_radius: 2,
            threshold_window_seconds: 0.45,
            threshold_mad_multiplier: 2.8,
            threshold_floor: 0.015,
            minimum_hit_spacing_seconds: 0.018,
            maximum_span_seconds: 0.45,
            tempo_min_bpm: 55.0,
            tempo_max_bpm: 210.0,
            tempo_hypotheses: 6,
            phase_hypotheses_per_tempo: 3,
            family_similarity_threshold: 0.79,
            maximum_families: 24,
            maximum_patterns: 16,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SampleSpan {
    /// Inclusive sample-frame index.
    pub start: usize,
    /// Exclusive sample-frame index.
    pub end: usize,
}

impl SampleSpan {
    pub fn len(self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StereoObservation {
    /// Side energy divided by mid-plus-side energy.
    pub width: f32,
    pub correlation: f32,
    /// `(left_rms - right_rms) / (left_rms + right_rms)`.
    pub channel_balance: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HitObservation {
    pub span: SampleSpan,
    /// The initiation may precede the retained audio.
    pub begins_at_input_boundary: bool,
    /// The release may continue beyond the retained audio.
    pub ends_at_input_boundary: bool,
    pub onset_sample: usize,
    /// Time-frequency novelty peak before sample-domain peak refinement.
    pub novelty_peak_sample: usize,
    /// Largest absolute sample near the initiation.
    pub peak_sample: usize,
    pub onset_seconds: f64,
    pub duration_seconds: f32,
    pub novelty_strength: f32,
    pub threshold_excess: f32,
    /// Low, middle, and high-band energy envelopes, peak-normalized per hit.
    pub band_envelope: [Vec<f32>; 3],
    /// L1-normalized log-frequency spectral shape.
    pub spectral_shape: Vec<f32>,
    pub spectral_centroid_hz: f32,
    pub spectral_spread_hz: f32,
    pub spectral_rolloff_hz: f32,
    pub spectral_flatness: f32,
    pub dominant_pitch_hz: Option<f32>,
    /// Spectral harmonic support. This is evidence of tonality, not pitch confidence.
    pub pitch_salience: f32,
    pub tonality: f32,
    pub noisiness: f32,
    /// Seconds for the post-peak RMS envelope to fall by about 60%.
    pub decay_seconds: f32,
    pub stereo: Option<StereoObservation>,
    pub family: Option<usize>,
    /// Similarity to the assigned family's medoid.
    pub family_similarity: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TempoRelation {
    Independent,
    HalfTimeOf(usize),
    DoubleTimeOf(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct TempoHypothesis {
    pub rank: usize,
    pub bpm: f32,
    pub period_frames: f32,
    /// Normalized autocorrelation plus beat-alignment support.
    pub periodicity: f32,
    /// Contrast against the median tested lag, not calibrated confidence.
    pub evidence: f32,
    pub relation: TempoRelation,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LocalPeriodicity {
    pub bpm: f32,
    pub strength: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TempogramFrame {
    pub center_sample: usize,
    pub periodicities: Vec<LocalPeriodicity>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BeatPhaseHypothesis {
    pub tempo_rank: usize,
    pub bpm: f32,
    pub phase_seconds: f64,
    pub score: f32,
    pub beat_samples: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DownbeatHypothesis {
    pub beat_phase_index: usize,
    pub meter_beats: usize,
    pub downbeat_offset: usize,
    pub score: f32,
    pub downbeat_samples: Vec<usize>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MedoidSampleReference {
    pub event_index: usize,
    pub excerpt: SampleSpan,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct EventFamilyHypothesis {
    pub id: usize,
    pub event_indices: Vec<usize>,
    pub medoid: MedoidSampleReference,
    pub mean_medoid_similarity: f32,
    pub minimum_medoid_similarity: f32,
    /// Cohesion and recurrence support, not probability of a physical source.
    pub evidence: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PatternOccurrence {
    pub event_index: usize,
    pub start_sample: usize,
    pub beat_position: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PatternHypothesis {
    /// Anonymous family ids in temporal order.
    pub family_sequence: Vec<usize>,
    /// Quantized sixteenth-note offsets from the first token.
    pub step_offsets: Vec<i32>,
    pub occurrences: Vec<PatternOccurrence>,
    pub evidence: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RhythmDeprojection {
    pub status: AnalysisStatus,
    pub sample_rate: u32,
    pub sample_frames: usize,
    pub analysis_hop: usize,
    pub novelty: Vec<f32>,
    pub band_novelty: Vec<[f32; 3]>,
    pub adaptive_threshold: Vec<f32>,
    pub hits: Vec<HitObservation>,
    /// Sparse local autocorrelation tempogram, retaining competing pulses.
    pub tempogram: Vec<TempogramFrame>,
    pub tempo_hypotheses: Vec<TempoHypothesis>,
    pub beat_phase_hypotheses: Vec<BeatPhaseHypothesis>,
    pub downbeat_hypotheses: Vec<DownbeatHypothesis>,
    pub event_families: Vec<EventFamilyHypothesis>,
    pub patterns: Vec<PatternHypothesis>,
    pub silent: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AnalysisStatus {
    #[default]
    Complete,
    Silent,
    InsufficientInput,
    InvalidConfiguration,
}

/// Analyze mono PCM. Non-finite samples are treated as silence.
pub fn analyze_mono(
    samples: &[f32],
    sample_rate: u32,
    config: &RhythmConfig,
) -> RhythmDeprojection {
    analyze_channels(samples, None, sample_rate, config)
}

/// Analyze interleaved PCM, preserving stereo observations when two or more
/// channels are supplied. All channels contribute equally to onset analysis.
pub fn analyze_interleaved(
    samples: &[f32],
    sample_rate: u32,
    channels: usize,
    config: &RhythmConfig,
) -> RhythmDeprojection {
    if channels == 0 {
        return empty_result(
            sample_rate,
            0,
            config.hop_size,
            AnalysisStatus::InvalidConfiguration,
        );
    }
    let frames = samples.len() / channels;
    let mut mono = Vec::with_capacity(frames);
    let mut stereo = if channels >= 2 {
        Some(Vec::with_capacity(frames * 2))
    } else {
        None
    };
    for frame in samples[..frames * channels].chunks_exact(channels) {
        let sum: f32 = frame.iter().map(|x| finite(*x)).sum();
        mono.push(sum / channels as f32);
        if let Some(stereo) = &mut stereo {
            stereo.extend([finite(frame[0]), finite(frame[1])]);
        }
    }
    analyze_channels(&mono, stereo.as_deref(), sample_rate, config)
}

fn analyze_channels(
    mono: &[f32],
    stereo: Option<&[f32]>,
    sample_rate: u32,
    config: &RhythmConfig,
) -> RhythmDeprojection {
    let hop = config.hop_size.max(1);
    if !valid_config(config, sample_rate) {
        return empty_result(
            sample_rate,
            mono.len(),
            hop,
            AnalysisStatus::InvalidConfiguration,
        );
    }
    if mono.is_empty() {
        return empty_result(
            sample_rate,
            mono.len(),
            hop,
            AnalysisStatus::InsufficientInput,
        );
    }
    let mean_square = mono
        .iter()
        .map(|sample| {
            let x = finite(*sample) as f64;
            x * x
        })
        .sum::<f64>()
        / mono.len() as f64;
    if mean_square < 1.0e-14 {
        return empty_result(sample_rate, mono.len(), hop, AnalysisStatus::Silent);
    }

    let transform = spectral_novelty(mono, sample_rate, config);
    let threshold = adaptive_threshold(&transform.novelty, sample_rate, hop, config);
    let candidates = pick_candidates(
        &transform.novelty,
        &threshold,
        mono.len(),
        sample_rate,
        config,
    );
    let mut hits = describe_hits(
        mono,
        stereo,
        sample_rate,
        config,
        &transform,
        &threshold,
        &candidates,
    );
    let tempogram = compute_tempogram(&transform.novelty, sample_rate, hop, config);
    let tempo_hypotheses = estimate_tempi(
        &transform.novelty,
        &tempogram,
        &hits,
        sample_rate,
        hop,
        config,
    );
    let beat_phase_hypotheses =
        infer_beat_phases(&hits, &tempo_hypotheses, mono.len(), sample_rate, config);
    let downbeat_hypotheses = infer_downbeats(&hits, &beat_phase_hypotheses, sample_rate);
    let event_families = cluster_families(&mut hits, config);
    let patterns = infer_patterns(
        &hits,
        &beat_phase_hypotheses,
        sample_rate,
        config.maximum_patterns,
    );

    RhythmDeprojection {
        status: AnalysisStatus::Complete,
        sample_rate,
        sample_frames: mono.len(),
        analysis_hop: hop,
        band_novelty: transform.band_novelty.clone(),
        novelty: transform.novelty,
        adaptive_threshold: threshold,
        hits,
        tempogram,
        tempo_hypotheses,
        beat_phase_hypotheses,
        downbeat_hypotheses,
        event_families,
        patterns,
        silent: false,
    }
}

fn valid_config(config: &RhythmConfig, sample_rate: u32) -> bool {
    let nyquist = sample_rate as f32 * 0.5;
    let resolution = if (16..=65_536).contains(&config.fft_size) {
        sample_rate as f32 / config.fft_size.next_power_of_two() as f32
    } else {
        f32::INFINITY
    };
    let usable_frequency_range =
        config.minimum_frequency_hz.max(resolution) < config.maximum_frequency_hz.min(nyquist);
    sample_rate > 0
        && (16..=65_536).contains(&config.fft_size)
        && config.hop_size > 0
        && (3..=256).contains(&config.log_band_count)
        && config.spectral_max_radius <= 256
        && config.minimum_frequency_hz.is_finite()
        && config.maximum_frequency_hz.is_finite()
        && config.minimum_frequency_hz > 0.0
        && config.minimum_frequency_hz < config.maximum_frequency_hz
        && config.minimum_frequency_hz < nyquist
        && usable_frequency_range
        && config.threshold_window_seconds.is_finite()
        && config.threshold_window_seconds > 0.0
        && config.threshold_mad_multiplier.is_finite()
        && config.threshold_mad_multiplier >= 0.0
        && config.threshold_floor.is_finite()
        && config.threshold_floor >= 0.0
        && config.minimum_hit_spacing_seconds.is_finite()
        && config.minimum_hit_spacing_seconds >= 0.0
        && config.maximum_span_seconds.is_finite()
        && config.maximum_span_seconds > 0.0
        && config.tempo_min_bpm.is_finite()
        && config.tempo_max_bpm.is_finite()
        && config.tempo_min_bpm > 0.0
        && config.tempo_min_bpm < config.tempo_max_bpm
        && config.tempo_hypotheses != 1
        && config.family_similarity_threshold.is_finite()
        && (0.0..=1.0).contains(&config.family_similarity_threshold)
}

fn empty_result(
    sample_rate: u32,
    frames: usize,
    hop: usize,
    status: AnalysisStatus,
) -> RhythmDeprojection {
    RhythmDeprojection {
        status,
        sample_rate,
        sample_frames: frames,
        analysis_hop: hop.max(1),
        silent: status == AnalysisStatus::Silent,
        ..RhythmDeprojection::default()
    }
}

struct SpectralTransform {
    novelty: Vec<f32>,
    band_novelty: Vec<[f32; 3]>,
    spectra: Vec<Vec<f32>>,
    band_centers: Vec<f32>,
}

fn spectral_novelty(mono: &[f32], sample_rate: u32, config: &RhythmConfig) -> SpectralTransform {
    let fft_size = config.fft_size.clamp(16, 65_536).next_power_of_two();
    let hop = config.hop_size.max(1);
    let frame_count = mono.len().saturating_sub(1) / hop + 1;
    let bands = config.log_band_count.clamp(3, 256);
    let nyquist = sample_rate as f32 * 0.5;
    let min_hz = config
        .minimum_frequency_hz
        .max(sample_rate as f32 / fft_size as f32);
    let max_hz = config.maximum_frequency_hz.min(nyquist).max(min_hz * 1.01);
    let ratio = (max_hz / min_hz).powf(1.0 / bands as f32);
    let edges: Vec<f32> = (0..=bands).map(|i| min_hz * ratio.powi(i as i32)).collect();
    let band_centers: Vec<f32> = edges.windows(2).map(|e| (e[0] * e[1]).sqrt()).collect();
    let mut bin_band = vec![None; fft_size / 2 + 1];
    for (bin, slot) in bin_band.iter_mut().enumerate() {
        let hz = bin as f32 * sample_rate as f32 / fft_size as f32;
        if hz >= min_hz && hz <= max_hz {
            let position = ((hz / min_hz).ln() / ratio.ln()).floor() as usize;
            *slot = Some(position.min(bands - 1));
        }
    }

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(fft_size);
    let window: Vec<f32> = (0..fft_size)
        .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / fft_size as f32).cos())
        .collect();
    let mut buffer = vec![Complex::new(0.0, 0.0); fft_size];
    let mut spectra = Vec::with_capacity(frame_count);
    let mut novelty = vec![0.0; frame_count];
    let mut band_novelty = vec![[0.0; 3]; frame_count];
    let mut previous = vec![0.0; bands];
    let mut region_counts = [0usize; 3];
    for band in 0..bands {
        region_counts[(band * 3 / bands).min(2)] += 1;
    }

    for frame in 0..frame_count {
        let center = frame * hop;
        for (i, sample) in buffer.iter_mut().enumerate() {
            let source = center as isize + i as isize - fft_size as isize / 2;
            let value = if source >= 0 && (source as usize) < mono.len() {
                finite(mono[source as usize])
            } else {
                0.0
            };
            *sample = Complex::new(value * window[i], 0.0);
        }
        fft.process(&mut buffer);
        let mut spectrum = vec![0.0; bands];
        let mut counts = vec![0usize; bands];
        for (bin, band) in bin_band.iter().enumerate() {
            if let Some(band) = *band {
                let magnitude = buffer[bin].norm() / fft_size as f32;
                spectrum[band] += magnitude;
                counts[band] += 1;
            }
        }
        for (value, count) in spectrum.iter_mut().zip(counts) {
            *value = (1.0 + 80.0 * *value / count.max(1) as f32).ln();
        }

        let radius = config.spectral_max_radius;
        for band in 0..bands {
            let lo = band.saturating_sub(radius);
            let hi = band.saturating_add(radius).saturating_add(1).min(bands);
            let reference = previous[lo..hi].iter().copied().fold(0.0_f32, f32::max);
            let delta = (spectrum[band] - reference).max(0.0);
            novelty[frame] += delta;
            let region = if band * 3 < bands {
                0
            } else if band * 3 < bands * 2 {
                1
            } else {
                2
            };
            band_novelty[frame][region] += delta;
        }
        novelty[frame] /= bands as f32;
        for (value, count) in band_novelty[frame].iter_mut().zip(region_counts) {
            *value /= count.max(1) as f32;
        }
        previous.clone_from(&spectrum);
        spectra.push(spectrum);
    }

    // A three-frame triangular smoother removes FFT scalloping without erasing ratchets.
    if novelty.len() > 2 {
        let raw = novelty.clone();
        let raw_bands = band_novelty.clone();
        for i in 1..novelty.len() - 1 {
            novelty[i] = raw[i - 1] * 0.2 + raw[i] * 0.6 + raw[i + 1] * 0.2;
            for band in 0..3 {
                band_novelty[i][band] = raw_bands[i - 1][band] * 0.2
                    + raw_bands[i][band] * 0.6
                    + raw_bands[i + 1][band] * 0.2;
            }
        }
    }
    SpectralTransform {
        novelty,
        band_novelty,
        spectra,
        band_centers,
    }
}

fn adaptive_threshold(
    novelty: &[f32],
    sample_rate: u32,
    hop: usize,
    config: &RhythmConfig,
) -> Vec<f32> {
    let radius =
        ((config.threshold_window_seconds * 0.5 * sample_rate as f32 / hop as f32) as usize).max(3);
    let scale_floor =
        config.threshold_floor.max(0.0) * novelty.iter().copied().fold(0.0_f32, f32::max);
    let mut threshold = vec![0.0; novelty.len()];
    for i in 0..novelty.len() {
        let lo = i.saturating_sub(radius);
        let hi = i
            .saturating_add(radius)
            .saturating_add(1)
            .min(novelty.len());
        let mut local = novelty[lo..hi].to_vec();
        let median = median_in_place(&mut local);
        for value in &mut local {
            *value = (*value - median).abs();
        }
        let mad = median_in_place(&mut local);
        threshold[i] =
            median + config.threshold_mad_multiplier.max(0.0) * 1.4826 * mad + scale_floor;
    }
    threshold
}

fn pick_candidates(
    novelty: &[f32],
    threshold: &[f32],
    sample_frames: usize,
    sample_rate: u32,
    config: &RhythmConfig,
) -> Vec<(usize, usize, usize)> {
    if novelty.len() < 3 {
        return Vec::new();
    }
    let hop = config.hop_size.max(1);
    let minimum_frames = (config.minimum_hit_spacing_seconds * sample_rate as f32 / hop as f32)
        .round()
        .max(1.0) as usize;
    let maximum_frames = (config.maximum_span_seconds * sample_rate as f32 / hop as f32)
        .ceil()
        .max(2.0) as usize;
    let mut peaks = Vec::<usize>::new();
    if novelty[0] > threshold[0] && novelty[0] > novelty[1] {
        peaks.push(0);
    }
    for i in 1..novelty.len() - 1 {
        if novelty[i] > threshold[i] && novelty[i] >= novelty[i - 1] && novelty[i] > novelty[i + 1]
        {
            if let Some(last) = peaks.last_mut() {
                if i - *last < minimum_frames {
                    if novelty[i] > novelty[*last] {
                        *last = i;
                    }
                    continue;
                }
            }
            peaks.push(i);
        }
    }
    let last = novelty.len() - 1;
    if novelty[last] > threshold[last] && novelty[last] >= novelty[last - 1] {
        if peaks
            .last()
            .is_none_or(|previous| last - *previous >= minimum_frames)
        {
            peaks.push(last);
        }
    }
    peaks
        .into_iter()
        .map(|peak| {
            let release = (threshold[peak] * 0.55).max(novelty[peak] * 0.12);
            let mut start = peak;
            while start > 0
                && peak - start < maximum_frames / 3
                && novelty[start - 1] > threshold[start - 1] * 0.65
            {
                start -= 1;
            }
            let mut end = peak + 1;
            while end < novelty.len() && end - peak < maximum_frames && novelty[end] > release {
                end += 1;
            }
            (start * hop, peak * hop, (end * hop).min(sample_frames))
        })
        .collect()
}

fn describe_hits(
    mono: &[f32],
    stereo: Option<&[f32]>,
    sample_rate: u32,
    config: &RhythmConfig,
    transform: &SpectralTransform,
    thresholds: &[f32],
    candidates: &[(usize, usize, usize)],
) -> Vec<HitObservation> {
    let hop = config.hop_size.max(1);
    candidates
        .iter()
        .enumerate()
        .map(|(candidate_index, &(start, novelty_peak, novelty_end))| {
            let maximum_end = start
                .saturating_add(
                    (config.maximum_span_seconds * sample_rate as f32)
                        .round()
                        .max(1.0) as usize,
                )
                .min(mono.len());
            let next_onset = candidates
                .get(candidate_index + 1)
                .map(|candidate| candidate.0)
                .unwrap_or(mono.len());
            let peak_search_end = novelty_end
                .max(novelty_peak.saturating_add(hop))
                .min(maximum_end)
                .min(next_onset)
                .max((start + 1).min(mono.len()));
            let peak = mono[start..peak_search_end]
                .iter()
                .enumerate()
                .max_by(|a, b| total_cmp(finite(*a.1).abs(), finite(*b.1).abs()))
                .map(|(offset, _)| start + offset)
                .unwrap_or(novelty_peak);
            let frame = (novelty_peak / hop).min(transform.spectra.len() - 1);
            let spectrum = &transform.spectra[frame];
            let sum = spectrum.iter().sum::<f32>().max(EPSILON);
            let spectral_shape: Vec<f32> = spectrum.iter().map(|value| value / sum).collect();
            let centroid = weighted_mean(&transform.band_centers, &spectral_shape);
            let spread = transform
                .band_centers
                .iter()
                .zip(&spectral_shape)
                .map(|(hz, weight)| weight * (hz - centroid).powi(2))
                .sum::<f32>()
                .sqrt();
            let rolloff = rolloff(&transform.band_centers, &spectral_shape, 0.85);
            let flatness = spectral_flatness(spectrum);
            let (pitch, salience) = pitch_evidence(spectrum, &transform.band_centers);
            let tonality = (salience * (1.0 - flatness).sqrt()).clamp(0.0, 1.0);
            let noisiness =
                (0.65 * flatness + 0.35 * (spread / centroid.max(1.0)).min(1.0)).clamp(0.0, 1.0);
            let (decay_seconds, amplitude_end) = decay_measure(mono, peak, sample_rate, hop);
            let end = novelty_end
                .max(amplitude_end)
                .min(mono.len())
                .min(maximum_end)
                .min(next_onset)
                .max(peak.saturating_add(1).min(maximum_end));
            let envelope =
                hit_band_envelope(transform, start / hop, end.saturating_add(hop - 1) / hop);
            let stereo =
                stereo.map(|channels| stereo_observation(channels, SampleSpan { start, end }));
            HitObservation {
                span: SampleSpan { start, end },
                begins_at_input_boundary: start == 0,
                ends_at_input_boundary: end == mono.len(),
                onset_sample: start,
                novelty_peak_sample: novelty_peak,
                peak_sample: peak,
                onset_seconds: start as f64 / sample_rate as f64,
                duration_seconds: (end - start) as f32 / sample_rate as f32,
                novelty_strength: transform.novelty[frame],
                threshold_excess: (transform.novelty[frame] - thresholds[frame]).max(0.0),
                band_envelope: envelope,
                spectral_shape,
                spectral_centroid_hz: centroid,
                spectral_spread_hz: spread,
                spectral_rolloff_hz: rolloff,
                spectral_flatness: flatness,
                dominant_pitch_hz: pitch,
                pitch_salience: salience,
                tonality,
                noisiness,
                decay_seconds: decay_seconds.min((end - peak) as f32 / sample_rate as f32),
                stereo,
                family: None,
                family_similarity: 0.0,
            }
        })
        .collect()
}

fn hit_band_envelope(
    transform: &SpectralTransform,
    start_frame: usize,
    end_frame: usize,
) -> [Vec<f32>; 3] {
    let lo = start_frame.min(transform.spectra.len());
    let hi = end_frame.min(transform.spectra.len()).max(lo);
    let mut output = [Vec::new(), Vec::new(), Vec::new()];
    for band in 0..3 {
        let band_start = band * transform.band_centers.len() / 3;
        let band_end = (band + 1) * transform.band_centers.len() / 3;
        output[band].extend(transform.spectra[lo..hi].iter().map(|spectrum| {
            spectrum[band_start..band_end].iter().sum::<f32>()
                / band_end.saturating_sub(band_start).max(1) as f32
        }));
    }
    let maximum = output
        .iter()
        .flat_map(|band| band.iter())
        .copied()
        .fold(0.0_f32, f32::max);
    if maximum > EPSILON {
        for value in output.iter_mut().flat_map(|band| band.iter_mut()) {
            *value /= maximum;
        }
    }
    output
}

fn decay_measure(mono: &[f32], peak: usize, sample_rate: u32, hop: usize) -> (f32, usize) {
    let maximum_end = peak
        .saturating_add(sample_rate as usize * 2 / 5)
        .min(mono.len());
    let window = (hop / 2).max(8);
    let initial = rms(&mono[peak.saturating_sub(window)..(peak + window).min(mono.len())]);
    if initial < EPSILON {
        return (0.0, peak.min(mono.len()));
    }
    let target = initial * 0.4;
    let release = initial * 0.12;
    let mut decay_at = maximum_end;
    let mut end = maximum_end;
    let mut cursor = peak.saturating_add(hop);
    while cursor < maximum_end {
        let level = rms(&mono[cursor.saturating_sub(window)..(cursor + window).min(mono.len())]);
        if decay_at == maximum_end && level <= target {
            decay_at = cursor;
        }
        if level <= release {
            end = cursor;
            break;
        }
        cursor = cursor.saturating_add(hop);
    }
    (
        (decay_at.saturating_sub(peak)) as f32 / sample_rate as f32,
        end,
    )
}

fn stereo_observation(stereo: &[f32], span: SampleSpan) -> StereoObservation {
    let frames = stereo.len() / 2;
    let start = span.start.min(frames);
    let end = span.end.min(frames);
    let mut mid_sq = 0.0_f64;
    let mut side_sq = 0.0_f64;
    let mut left_sq = 0.0_f64;
    let mut right_sq = 0.0_f64;
    let mut cross = 0.0_f64;
    for pair in stereo[start * 2..end * 2].chunks_exact(2) {
        let left = finite(pair[0]) as f64;
        let right = finite(pair[1]) as f64;
        let mid = (left + right) * 0.5;
        let side = (left - right) * 0.5;
        mid_sq += mid * mid;
        side_sq += side * side;
        left_sq += left * left;
        right_sq += right * right;
        cross += left * right;
    }
    let total = mid_sq + side_sq;
    let correlation_denominator = (left_sq * right_sq).sqrt();
    StereoObservation {
        width: if total > 0.0 {
            (side_sq / total) as f32
        } else {
            0.0
        },
        correlation: if correlation_denominator > 0.0 {
            (cross / correlation_denominator).clamp(-1.0, 1.0) as f32
        } else {
            0.0
        },
        channel_balance: if left_sq + right_sq > 0.0 {
            ((left_sq.sqrt() - right_sq.sqrt()) / (left_sq.sqrt() + right_sq.sqrt())) as f32
        } else {
            0.0
        },
    }
}

fn compute_tempogram(
    novelty: &[f32],
    sample_rate: u32,
    hop: usize,
    config: &RhythmConfig,
) -> Vec<TempogramFrame> {
    if novelty.len() < 8 || sample_rate == 0 {
        return Vec::new();
    }
    let frames_per_minute = 60.0 * sample_rate as f32 / hop as f32;
    let min_lag = (frames_per_minute / config.tempo_max_bpm.max(1.0))
        .floor()
        .max(2.0) as usize;
    let max_lag = (frames_per_minute / config.tempo_min_bpm.max(1.0)).ceil() as usize;
    if min_lag >= novelty.len() || min_lag > max_lag {
        return Vec::new();
    }
    let desired_window = (8.0 * sample_rate as f32 / hop as f32).round() as usize;
    let window = desired_window
        .max(max_lag.saturating_mul(3))
        .min(novelty.len());
    let step = (2.0 * sample_rate as f32 / hop as f32).round().max(1.0) as usize;
    let mut starts = Vec::new();
    if novelty.len() <= window {
        starts.push(0);
    } else {
        starts.extend((0..=novelty.len() - window).step_by(step));
        let last = novelty.len() - window;
        if starts.last().copied() != Some(last) {
            starts.push(last);
        }
    }
    starts
        .into_iter()
        .filter_map(|start| {
            let segment = &novelty[start..start + window];
            let mean = segment.iter().sum::<f32>() / segment.len() as f32;
            let centered: Vec<f32> = segment.iter().map(|value| value - mean).collect();
            let energy = centered.iter().map(|value| value * value).sum::<f32>();
            if energy <= EPSILON {
                return None;
            }
            let usable_max = max_lag.min(segment.len().saturating_sub(2));
            let mut scores = Vec::new();
            for lag in min_lag..=usable_max {
                let score = normalized_autocorrelation(&centered, lag);
                scores.push((lag, score.max(0.0)));
            }
            let mut peaks: Vec<(usize, f32)> = scores
                .iter()
                .enumerate()
                .filter(|(index, (_, score))| {
                    (*index == 0 || *score >= scores[*index - 1].1)
                        && (*index + 1 == scores.len() || *score > scores[*index + 1].1)
                })
                .map(|(_, entry)| *entry)
                .collect();
            peaks.sort_by(|a, b| total_cmp(b.1, a.1).then_with(|| a.0.cmp(&b.0)));
            let mut separated = Vec::new();
            for peak in peaks {
                if separated
                    .iter()
                    .all(|other: &(usize, f32)| other.0.abs_diff(peak.0) > 2)
                {
                    separated.push(peak);
                }
                if separated.len() == 5 {
                    break;
                }
            }
            Some(TempogramFrame {
                center_sample: (start + window / 2) * hop,
                periodicities: separated
                    .into_iter()
                    .map(|(lag, score)| LocalPeriodicity {
                        bpm: frames_per_minute / lag as f32,
                        strength: score.clamp(0.0, 1.0),
                    })
                    .collect(),
            })
        })
        .collect()
}

fn estimate_tempi(
    novelty: &[f32],
    tempogram: &[TempogramFrame],
    hits: &[HitObservation],
    sample_rate: u32,
    hop: usize,
    config: &RhythmConfig,
) -> Vec<TempoHypothesis> {
    if novelty.len() < 8 || hits.len() < 2 || config.tempo_hypotheses == 0 {
        return Vec::new();
    }
    let frames_per_minute = 60.0 * sample_rate as f32 / hop as f32;
    let min_lag = (frames_per_minute / config.tempo_max_bpm.max(1.0))
        .floor()
        .max(2.0) as usize;
    let max_lag = (frames_per_minute / config.tempo_min_bpm.max(1.0)).ceil() as usize;
    let max_lag = max_lag.min(novelty.len().saturating_sub(2));
    if min_lag > max_lag {
        return Vec::new();
    }
    let mean = novelty.iter().sum::<f32>() / novelty.len() as f32;
    let centered: Vec<f32> = novelty.iter().map(|value| value - mean).collect();
    let energy = centered
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .max(0.0);
    if energy <= EPSILON {
        return Vec::new();
    }
    let mut scores = Vec::new();
    for lag in min_lag..=max_lag {
        let correlation = normalized_autocorrelation(&centered, lag).max(0.0);
        let double = if lag * 2 <= max_lag {
            normalized_autocorrelation(&centered, lag * 2)
        } else {
            0.0
        };
        let half = if lag / 2 >= min_lag {
            normalized_autocorrelation(&centered, lag / 2)
        } else {
            0.0
        };
        let bpm = frames_per_minute / lag as f32;
        let local_matches: Vec<f32> = tempogram
            .iter()
            .flat_map(|frame| &frame.periodicities)
            .filter(|periodicity| (periodicity.bpm / bpm - 1.0).abs() < 0.035)
            .map(|periodicity| periodicity.strength)
            .collect();
        let local_support = if local_matches.is_empty() {
            0.0
        } else {
            local_matches.iter().sum::<f32>() / local_matches.len() as f32
        };
        let period_samples = lag as f32 * hop as f32;
        let mut interval_support = 0.0;
        let mut interval_count = 0;
        for first in 0..hits.len() {
            for second in first + 1..(first + 9).min(hits.len()) {
                let interval = hits[second]
                    .peak_sample
                    .saturating_sub(hits[first].peak_sample) as f32;
                let multiple = (interval / period_samples).round().max(1.0);
                if multiple <= 8.0 {
                    let error = (interval / period_samples - multiple).abs();
                    interval_support += (-0.5 * (error / 0.08).powi(2)).exp() / multiple.sqrt();
                    interval_count += 1;
                }
            }
        }
        let interval_support = if interval_count == 0 {
            0.0
        } else {
            interval_support / interval_count as f32
        };
        scores.push((
            lag,
            (0.7 * correlation
                + 0.14 * double.max(0.0)
                + 0.06 * half.max(0.0)
                + 0.07 * local_support
                + 0.03 * interval_support)
                .max(0.0),
        ));
    }
    let mut distribution: Vec<f32> = scores.iter().map(|entry| entry.1).collect();
    let baseline = median_in_place(&mut distribution);
    let mut peaks: Vec<(usize, f32)> = scores
        .iter()
        .enumerate()
        .filter(|(i, (_, value))| {
            (*i == 0 || *value >= scores[*i - 1].1)
                && (*i + 1 == scores.len() || *value > scores[*i + 1].1)
        })
        .map(|(_, value)| *value)
        .collect();
    peaks.sort_by(|a, b| total_cmp(b.1, a.1).then_with(|| a.0.cmp(&b.0)));

    let requested = config.tempo_hypotheses;
    let Some(primary) = peaks.first().copied() else {
        return Vec::new();
    };
    let primary_lag = primary.0;
    let mut chosen = Vec::<(usize, f32)>::new();
    chosen.push(primary);
    for factor in [2.0, 0.5] {
        let lag = (primary.0 as f32 * factor).round() as usize;
        if lag >= min_lag && lag <= max_lag {
            let score = scores[lag - min_lag].1;
            chosen.push((lag, score));
        }
    }
    for (lag, score) in peaks {
        if chosen.len() >= requested {
            break;
        }
        let bpm = frames_per_minute / lag as f32;
        if chosen.iter().all(|(other, _)| {
            let other_bpm = frames_per_minute / *other as f32;
            (bpm / other_bpm - 1.0).abs() > 0.025
        }) {
            chosen.push((lag, score));
        }
    }
    if chosen.len() < requested.min(scores.len()) {
        let mut remaining = scores.clone();
        remaining.sort_by(|a, b| total_cmp(b.1, a.1).then_with(|| a.0.cmp(&b.0)));
        for (lag, score) in remaining {
            if chosen.len() >= requested {
                break;
            }
            if chosen.iter().all(|(other, _)| other.abs_diff(lag) > 1) {
                chosen.push((lag, score));
            }
        }
    }
    chosen.sort_by(|a, b| total_cmp(b.1, a.1).then_with(|| a.0.cmp(&b.0)));
    chosen.truncate(requested);
    let primary_rank = chosen
        .iter()
        .position(|(lag, _)| *lag == primary_lag)
        .unwrap_or(0);
    chosen
        .into_iter()
        .enumerate()
        .map(|(rank, (lag, score))| {
            let ratio = lag as f32 / primary_lag as f32;
            let relation = if rank == primary_rank {
                TempoRelation::Independent
            } else if (ratio - 2.0).abs() < 0.04 {
                TempoRelation::HalfTimeOf(primary_rank)
            } else if (ratio - 0.5).abs() < 0.04 {
                TempoRelation::DoubleTimeOf(primary_rank)
            } else {
                TempoRelation::Independent
            };
            TempoHypothesis {
                rank,
                bpm: frames_per_minute / lag as f32,
                period_frames: lag as f32,
                periodicity: score.clamp(0.0, 1.0),
                evidence: ((score - baseline) / (score + baseline + EPSILON)).clamp(0.0, 1.0),
                relation,
            }
        })
        .collect()
}

fn infer_beat_phases(
    hits: &[HitObservation],
    tempi: &[TempoHypothesis],
    sample_frames: usize,
    sample_rate: u32,
    config: &RhythmConfig,
) -> Vec<BeatPhaseHypothesis> {
    if config.phase_hypotheses_per_tempo == 0 {
        return Vec::new();
    }
    let mut output = Vec::new();
    for tempo in tempi {
        if tempo.evidence <= 0.01 && tempo.periodicity <= 0.01 {
            continue;
        }
        let period = sample_rate as f64 * 60.0 / tempo.bpm as f64;
        let phase_bins = 32usize;
        let mut scores = vec![0.0_f32; phase_bins];
        for hit in hits {
            let phase = (hit.peak_sample as f64 % period) / period;
            for (bin, score) in scores.iter_mut().enumerate() {
                let center = bin as f64 / phase_bins as f64;
                let distance = circular_distance(phase, center);
                let kernel = (-0.5 * (distance / 0.075).powi(2)).exp() as f32;
                *score += kernel * (hit.novelty_strength + hit.threshold_excess).sqrt();
            }
        }
        let mut distribution = scores.clone();
        let baseline = median_in_place(&mut distribution);
        let mut bins: Vec<usize> = (0..phase_bins)
            .filter(|bin| {
                scores[*bin] >= scores[(*bin + phase_bins - 1) % phase_bins]
                    && scores[*bin] > scores[(*bin + 1) % phase_bins]
                    && scores[*bin] > baseline
            })
            .collect();
        bins.sort_by(|a, b| total_cmp(scores[*b], scores[*a]).then_with(|| a.cmp(b)));
        let mut selected = Vec::new();
        for bin in bins {
            if selected.iter().all(|other: &usize| {
                let difference = bin.abs_diff(*other).min(phase_bins - bin.abs_diff(*other));
                difference >= 3
            }) {
                selected.push(bin);
            }
            if selected.len() >= config.phase_hypotheses_per_tempo {
                break;
            }
        }
        for bin in selected {
            let phase_samples = period * bin as f64 / phase_bins as f64;
            let mut beat_samples = Vec::new();
            let mut sample = phase_samples;
            while sample < sample_frames as f64 {
                let rounded = sample.round() as usize;
                if rounded < sample_frames && beat_samples.last().copied() != Some(rounded) {
                    beat_samples.push(rounded);
                }
                sample += period;
            }
            let contrast =
                ((scores[bin] - baseline) / (scores[bin] + baseline + EPSILON)).clamp(0.0, 1.0);
            output.push(BeatPhaseHypothesis {
                tempo_rank: tempo.rank,
                bpm: tempo.bpm,
                phase_seconds: phase_samples / sample_rate as f64,
                score: (contrast * tempo.evidence).clamp(0.0, 1.0),
                beat_samples,
            });
        }
    }
    output.sort_by(|a, b| {
        a.tempo_rank
            .cmp(&b.tempo_rank)
            .then_with(|| total_cmp(b.score, a.score))
            .then_with(|| total_cmp(a.phase_seconds as f32, b.phase_seconds as f32))
    });
    output
}

fn infer_downbeats(
    hits: &[HitObservation],
    phases: &[BeatPhaseHypothesis],
    sample_rate: u32,
) -> Vec<DownbeatHypothesis> {
    let mut output = Vec::new();
    for (phase_index, phase) in phases.iter().enumerate() {
        if phase.tempo_rank != 0 || phase.beat_samples.len() < 4 || phase.score <= 0.01 {
            continue;
        }
        let period = sample_rate as f64 * 60.0 / phase.bpm as f64;
        for meter in [4usize, 3, 6] {
            if phase.beat_samples.len() < meter * 2 {
                continue;
            }
            let mut offsets = vec![0.0_f32; meter];
            let mut active_cycles = vec![Vec::<usize>::new(); meter];
            for hit in hits {
                if hit.begins_at_input_boundary || hit.ends_at_input_boundary {
                    continue;
                }
                let beat_position =
                    (hit.peak_sample as f64 - phase.phase_seconds * sample_rate as f64) / period;
                let nearest = beat_position.round();
                if (beat_position - nearest).abs() > 0.18 {
                    continue;
                }
                let beat = nearest as i64;
                if beat >= 0 {
                    let slot = beat as usize % meter;
                    let low_bias = hit.spectral_shape[..hit
                        .spectral_shape
                        .len()
                        .min(hit.spectral_shape.len() / 3 + 1)]
                        .iter()
                        .sum::<f32>();
                    offsets[slot] += hit.novelty_strength * (0.6 + 0.8 * low_bias);
                    let cycle = beat as usize / meter;
                    if active_cycles[slot].last().copied() != Some(cycle) {
                        active_cycles[slot].push(cycle);
                    }
                }
            }
            let mut null_distribution = offsets.clone();
            let null_level = median_in_place(&mut null_distribution);
            let maximum = offsets.iter().copied().fold(0.0_f32, f32::max);
            if maximum <= EPSILON || maximum <= null_level * 1.05 {
                continue;
            }
            let cycles = phase.beat_samples.len().div_ceil(meter).max(1);
            let mut slots: Vec<usize> = (0..meter).collect();
            slots.sort_by(|a, b| total_cmp(offsets[*b], offsets[*a]).then_with(|| a.cmp(b)));
            for &slot in slots.iter().take(2) {
                let accent_contrast = ((offsets[slot] - null_level)
                    / (offsets[slot] + null_level + EPSILON))
                    .clamp(0.0, 1.0);
                let recurrence = active_cycles[slot].len() as f32 / cycles as f32;
                let score = (accent_contrast * recurrence * phase.score).clamp(0.0, 1.0);
                if score <= 0.01 {
                    continue;
                }
                let downbeat_samples = phase
                    .beat_samples
                    .iter()
                    .enumerate()
                    .filter_map(|(index, sample)| (index % meter == slot).then_some(*sample))
                    .collect();
                output.push(DownbeatHypothesis {
                    beat_phase_index: phase_index,
                    meter_beats: meter,
                    downbeat_offset: slot,
                    score,
                    downbeat_samples,
                });
            }
        }
    }
    output.sort_by(|a, b| {
        total_cmp(b.score, a.score).then_with(|| a.meter_beats.cmp(&b.meter_beats))
    });
    output.truncate(12);
    output
}

fn cluster_families(
    hits: &mut [HitObservation],
    config: &RhythmConfig,
) -> Vec<EventFamilyHypothesis> {
    if hits.is_empty() || config.maximum_families == 0 {
        return Vec::new();
    }
    let vectors: Vec<Vec<f32>> = hits.iter().map(event_vector).collect();
    let threshold = config.family_similarity_threshold.clamp(0.0, 1.0);
    let mut clusters: Vec<Vec<usize>> = Vec::new();
    let mut medoids: Vec<usize> = Vec::new();
    for index in 0..hits.len() {
        let best = medoids
            .iter()
            .enumerate()
            .map(|(family, medoid)| (family, cosine(&vectors[index], &vectors[*medoid])))
            .max_by(|a, b| total_cmp(a.1, b.1).then_with(|| b.0.cmp(&a.0)));
        if let Some((family, _similarity)) = best.filter(|(_, similarity)| *similarity >= threshold)
        {
            clusters[family].push(index);
            medoids[family] = select_medoid(&clusters[family], &vectors);
        } else if clusters.len() < config.maximum_families {
            clusters.push(vec![index]);
            medoids.push(index);
        }
    }

    // One deterministic reassignment pass removes order artifacts from online seeding.
    let stable_medoids = medoids.clone();
    clusters.iter_mut().for_each(Vec::clear);
    for index in 0..hits.len() {
        let best = stable_medoids
            .iter()
            .enumerate()
            .map(|(family, medoid)| (family, cosine(&vectors[index], &vectors[*medoid])))
            .max_by(|a, b| total_cmp(a.1, b.1).then_with(|| b.0.cmp(&a.0)));
        if let Some((family, _)) = best.filter(|(_, similarity)| *similarity >= threshold) {
            clusters[family].push(index);
        }
    }
    medoids = clusters
        .iter()
        .map(|cluster| select_medoid(cluster, &vectors))
        .collect();

    let mut families = Vec::new();
    for (id, events) in clusters.into_iter().enumerate() {
        if events.is_empty() {
            continue;
        }
        let medoid = medoids[id];
        let similarities: Vec<f32> = events
            .iter()
            .map(|event| cosine(&vectors[*event], &vectors[medoid]))
            .collect();
        let mean = similarities.iter().sum::<f32>() / similarities.len() as f32;
        let minimum = similarities.iter().copied().fold(1.0_f32, f32::min);
        let recurrence = if events.len() <= 1 {
            0.12
        } else {
            (events.len() as f32 / 4.0).min(1.0)
        };
        let evidence = (mean * recurrence * (0.6 + 0.4 * minimum)).clamp(0.0, 1.0);
        let excerpt = hits[medoid].span;
        for (&event, &similarity) in events.iter().zip(&similarities) {
            hits[event].family = Some(id);
            hits[event].family_similarity = similarity;
        }
        families.push(EventFamilyHypothesis {
            id,
            event_indices: events,
            medoid: MedoidSampleReference {
                event_index: medoid,
                excerpt,
            },
            mean_medoid_similarity: mean,
            minimum_medoid_similarity: minimum,
            evidence,
        });
    }
    families
}

fn event_vector(hit: &HitObservation) -> Vec<f32> {
    let mut vector = hit.spectral_shape.clone();
    vector.extend([
        (hit.duration_seconds / 0.5).min(1.0),
        (hit.decay_seconds / 0.4).min(1.0),
        hit.spectral_flatness,
        hit.tonality,
        hit.noisiness,
        (hit.spectral_centroid_hz / 12_000.0).min(1.0),
        hit.pitch_salience,
    ]);
    for envelope in &hit.band_envelope {
        vector.push(envelope.iter().copied().fold(0.0_f32, f32::max));
        vector.push(if envelope.is_empty() {
            0.0
        } else {
            envelope.iter().sum::<f32>() / envelope.len() as f32
        });
    }
    if let Some(stereo) = hit.stereo {
        vector.extend([stereo.width, (stereo.correlation + 1.0) * 0.5]);
    }
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > EPSILON {
        vector.iter_mut().for_each(|value| *value /= norm);
    }
    vector
}

fn select_medoid(cluster: &[usize], vectors: &[Vec<f32>]) -> usize {
    cluster
        .iter()
        .copied()
        .max_by(|a, b| {
            let score_a: f32 = cluster
                .iter()
                .map(|other| cosine(&vectors[*a], &vectors[*other]))
                .sum();
            let score_b: f32 = cluster
                .iter()
                .map(|other| cosine(&vectors[*b], &vectors[*other]))
                .sum();
            total_cmp(score_a, score_b).then_with(|| b.cmp(a))
        })
        .unwrap_or(0)
}

fn infer_patterns(
    hits: &[HitObservation],
    phases: &[BeatPhaseHypothesis],
    sample_rate: u32,
    maximum: usize,
) -> Vec<PatternHypothesis> {
    let Some(phase) = phases.iter().find(|phase| phase.tempo_rank == 0) else {
        return Vec::new();
    };
    if hits.len() < 4 || maximum == 0 {
        return Vec::new();
    }
    let period = sample_rate as f64 * 60.0 / phase.bpm as f64;
    let origin = phase.phase_seconds * sample_rate as f64;
    let tokens: Vec<(usize, i32, usize)> = hits
        .iter()
        .enumerate()
        .filter_map(|(event_index, hit)| {
            hit.family.map(|family| {
                let step = (((hit.peak_sample as f64 - origin) / period) * 4.0).round() as i32;
                (family, step, event_index)
            })
        })
        .collect();
    let mut patterns = Vec::new();
    for length in 2..=8.min(tokens.len() / 2) {
        for start in 0..=tokens.len() - length {
            let base_step = tokens[start].1;
            let sequence: Vec<usize> = tokens[start..start + length]
                .iter()
                .map(|token| token.0)
                .collect();
            let offsets: Vec<i32> = tokens[start..start + length]
                .iter()
                .map(|token| token.1 - base_step)
                .collect();
            let mut occurrence_indices = vec![start];
            for other in start + length..=tokens.len() - length {
                let other_base = tokens[other].1;
                let same = (0..length).all(|offset| {
                    tokens[other + offset].0 == sequence[offset]
                        && tokens[other + offset].1 - other_base == offsets[offset]
                });
                if same {
                    occurrence_indices.push(other);
                }
            }
            if occurrence_indices.len() < 2 {
                continue;
            }
            let duplicate = patterns.iter().any(|pattern: &PatternHypothesis| {
                pattern.family_sequence == sequence && pattern.step_offsets == offsets
            });
            if duplicate {
                continue;
            }
            let occurrences = occurrence_indices
                .iter()
                .map(|index| PatternOccurrence {
                    event_index: tokens[*index].2,
                    start_sample: hits[tokens[*index].2].onset_sample,
                    beat_position: tokens[*index].1 as f32 / 4.0,
                })
                .collect::<Vec<_>>();
            let evidence = (occurrences.len() as f32 / 4.0).min(1.0)
                * (length as f32 / 6.0).min(1.0)
                * phase.score.sqrt();
            patterns.push(PatternHypothesis {
                family_sequence: sequence,
                step_offsets: offsets,
                occurrences,
                evidence,
            });
        }
    }
    patterns.sort_by(|a, b| {
        total_cmp(b.evidence, a.evidence)
            .then_with(|| b.family_sequence.len().cmp(&a.family_sequence.len()))
            .then_with(|| a.step_offsets.cmp(&b.step_offsets))
    });
    patterns.truncate(maximum);
    patterns
}

fn pitch_evidence(spectrum: &[f32], centers: &[f32]) -> (Option<f32>, f32) {
    if spectrum.len() < 6 {
        return (None, 0.0);
    }
    let mut best = (0usize, 0.0_f32);
    let total = spectrum.iter().sum::<f32>().max(EPSILON);
    for fundamental in 0..spectrum.len().saturating_sub(4) {
        let hz = centers[fundamental];
        if hz < 45.0 || hz > 4_500.0 {
            continue;
        }
        let mut support = spectrum[fundamental];
        for harmonic in 2..=5 {
            let target = hz * harmonic as f32;
            if let Some(index) = nearest_index(centers, target) {
                support += spectrum[index] / harmonic as f32;
            }
        }
        if support > best.1 {
            best = (fundamental, support);
        }
    }
    let salience = (best.1 / total * 2.2).clamp(0.0, 1.0);
    if salience >= 0.2 {
        (Some(centers[best.0]), salience)
    } else {
        (None, salience)
    }
}

fn nearest_index(values: &[f32], target: f32) -> Option<usize> {
    values
        .binary_search_by(|value| total_cmp(*value, target))
        .map(Some)
        .unwrap_or_else(|index| {
            if index == 0 {
                Some(0)
            } else if index >= values.len() {
                None
            } else if (values[index] - target).abs() < (values[index - 1] - target).abs() {
                Some(index)
            } else {
                Some(index - 1)
            }
        })
}

fn rolloff(centers: &[f32], weights: &[f32], fraction: f32) -> f32 {
    let mut cumulative = 0.0;
    for (hz, weight) in centers.iter().zip(weights) {
        cumulative += weight;
        if cumulative >= fraction {
            return *hz;
        }
    }
    centers.last().copied().unwrap_or(0.0)
}

fn spectral_flatness(spectrum: &[f32]) -> f32 {
    if spectrum.is_empty() {
        return 0.0;
    }
    let arithmetic = spectrum.iter().sum::<f32>() / spectrum.len() as f32;
    if arithmetic <= EPSILON {
        return 0.0;
    }
    let geometric = (spectrum
        .iter()
        .map(|value| value.max(EPSILON).ln())
        .sum::<f32>()
        / spectrum.len() as f32)
        .exp();
    (geometric / arithmetic).clamp(0.0, 1.0)
}

fn weighted_mean(values: &[f32], weights: &[f32]) -> f32 {
    values
        .iter()
        .zip(weights)
        .map(|(value, weight)| value * weight)
        .sum()
}

fn median_in_place(values: &mut [f32]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| total_cmp(*a, *b));
    let middle = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) * 0.5
    } else {
        values[middle]
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let length = a.len().min(b.len());
    let mut dot = 0.0;
    let mut norm_a = 0.0;
    let mut norm_b = 0.0;
    for index in 0..length {
        dot += a[index] * b[index];
        norm_a += a[index] * a[index];
        norm_b += b[index] * b[index];
    }
    if norm_a <= EPSILON || norm_b <= EPSILON {
        0.0
    } else {
        (dot / (norm_a * norm_b).sqrt()).clamp(0.0, 1.0)
    }
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples
        .iter()
        .map(|sample| finite(*sample).powi(2))
        .sum::<f32>()
        / samples.len() as f32)
        .sqrt()
}

fn normalized_autocorrelation(values: &[f32], lag: usize) -> f32 {
    if lag == 0 || lag >= values.len() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut first_energy = 0.0;
    let mut second_energy = 0.0;
    for index in lag..values.len() {
        let first = values[index];
        let second = values[index - lag];
        dot += first * second;
        first_energy += first * first;
        second_energy += second * second;
    }
    let denominator = (first_energy * second_energy).sqrt();
    if denominator <= EPSILON {
        0.0
    } else {
        (dot / denominator).clamp(-1.0, 1.0)
    }
}

fn circular_distance(a: f64, b: f64) -> f64 {
    let difference = (a - b).abs();
    difference.min(1.0 - difference)
}

fn finite(value: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

fn total_cmp(a: f32, b: f32) -> Ordering {
    a.total_cmp(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 16_000;

    fn impulse_train(bpm: f32, seconds: f32) -> Vec<f32> {
        let mut signal = vec![0.0; (seconds * RATE as f32) as usize];
        let period = (RATE as f32 * 60.0 / bpm) as usize;
        for start in (0..signal.len()).step_by(period) {
            for offset in 0..(RATE as usize / 20).min(signal.len() - start) {
                let envelope = (-(offset as f32) / 130.0).exp();
                signal[start + offset] +=
                    envelope * (std::f32::consts::TAU * 70.0 * offset as f32 / RATE as f32).sin();
            }
        }
        signal
    }

    fn add_noise_hit(signal: &mut [f32], start: usize, duration: usize, gain: f32) {
        let mut state = 0x1234_5678_u32 ^ start as u32 ^ gain.to_bits();
        for offset in 0..duration.min(signal.len().saturating_sub(start)) {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let noise = state as f32 / u32::MAX as f32 * 2.0 - 1.0;
            signal[start + offset] += gain * noise * (-(offset as f32) / 90.0).exp();
        }
    }

    #[test]
    fn silence_and_short_input_are_safe() {
        let config = RhythmConfig::default();
        let empty = analyze_mono(&[], RATE, &config);
        let quiet = analyze_mono(&vec![0.0; 31], RATE, &config);
        assert_eq!(empty.status, AnalysisStatus::InsufficientInput);
        assert_eq!(quiet.status, AnalysisStatus::Silent);
        assert!(empty.hits.is_empty() && quiet.tempo_hypotheses.is_empty());
    }

    #[test]
    fn tempo_keeps_ranked_half_double_ambiguity() {
        let config = RhythmConfig {
            fft_size: 512,
            hop_size: 64,
            ..RhythmConfig::default()
        };
        let analysis = analyze_mono(&impulse_train(120.0, 10.0), RATE, &config);
        assert!(analysis.tempo_hypotheses.len() >= 2);
        assert!(analysis
            .tempo_hypotheses
            .iter()
            .any(|tempo| (tempo.bpm - 120.0).abs() < 8.0));
        assert!(analysis.tempo_hypotheses.iter().any(|tempo| {
            matches!(
                tempo.relation,
                TempoRelation::HalfTimeOf(_) | TempoRelation::DoubleTimeOf(_)
            ) && tempo.periodicity > 0.05
        }));
        assert!(!analysis.beat_phase_hypotheses.is_empty());
        assert!(analysis
            .beat_phase_hypotheses
            .iter()
            .flat_map(|phase| &phase.beat_samples)
            .all(|sample| *sample < analysis.sample_frames));
    }

    #[test]
    fn separates_recurring_kick_snare_and_hat_shapes() {
        let mut signal = impulse_train(120.0, 8.0);
        for beat in 0..16 {
            let start = beat * RATE as usize / 2;
            if beat % 2 == 1 {
                add_noise_hit(&mut signal, start, 1_200, 0.7);
            }
            add_noise_hit(&mut signal, start + RATE as usize / 4, 280, 0.22);
        }
        let config = RhythmConfig {
            fft_size: 512,
            hop_size: 64,
            family_similarity_threshold: 0.86,
            ..RhythmConfig::default()
        };
        let analysis = analyze_mono(&signal, RATE, &config);
        assert!(analysis.hits.len() >= 12);
        assert!(analysis.event_families.len() >= 2);
        assert!(analysis
            .event_families
            .iter()
            .all(|family| family.minimum_medoid_similarity >= config.family_similarity_threshold));
        assert!(!analysis.patterns.is_empty());
        assert!(analysis
            .event_families
            .iter()
            .all(|family| !family.medoid.excerpt.is_empty()));
    }

    #[test]
    fn dense_ratchets_remain_distinct() {
        let mut signal = vec![0.0; RATE as usize * 2];
        for start in (RATE as usize / 2..RATE as usize).step_by(RATE as usize / 40) {
            add_noise_hit(&mut signal, start, 160, 0.8);
        }
        let config = RhythmConfig {
            fft_size: 256,
            hop_size: 32,
            minimum_hit_spacing_seconds: 0.012,
            ..RhythmConfig::default()
        };
        let analysis = analyze_mono(&signal, RATE, &config);
        assert!(
            analysis.hits.len() >= 10,
            "detected {}",
            analysis.hits.len()
        );
    }

    #[test]
    fn vibrato_does_not_become_a_hit_train_but_slide_has_an_initiation() {
        let seconds = 4.0;
        let mut signal = vec![0.0; (seconds * RATE as f32) as usize];
        let mut phase = 0.0_f32;
        for (index, sample) in signal.iter_mut().enumerate() {
            let t = index as f32 / RATE as f32;
            let frequency = if t < 1.5 {
                440.0 + 9.0 * (std::f32::consts::TAU * 6.0 * t).sin()
            } else {
                440.0 + 180.0 * (t - 2.0).max(0.0)
            };
            phase += std::f32::consts::TAU * frequency / RATE as f32;
            let gain = if (1.5..2.0).contains(&t) { 0.0 } else { 0.35 };
            *sample = gain * phase.sin();
        }
        let config = RhythmConfig {
            fft_size: 1024,
            hop_size: 64,
            spectral_max_radius: 3,
            ..RhythmConfig::default()
        };
        let analysis = analyze_mono(&signal, RATE, &config);
        assert!(
            analysis.hits.len() <= 8,
            "vibrato produced {} hits",
            analysis.hits.len()
        );
        assert!(analysis
            .hits
            .iter()
            .any(|hit| (1.6..2.2).contains(&hit.onset_seconds)));
    }

    #[test]
    fn output_is_bit_deterministic_and_stereo_is_retained() {
        let mono = impulse_train(130.0, 5.0);
        let mut stereo = Vec::with_capacity(mono.len() * 2);
        for sample in &mono {
            stereo.extend([*sample, *sample * 0.4]);
        }
        let config = RhythmConfig {
            fft_size: 512,
            hop_size: 64,
            ..RhythmConfig::default()
        };
        let a = analyze_interleaved(&stereo, RATE, 2, &config);
        let b = analyze_interleaved(&stereo, RATE, 2, &config);
        assert_eq!(a, b);
        assert!(a.hits.iter().all(|hit| hit.stereo.is_some()));
    }

    #[test]
    fn malformed_config_is_reported_without_panicking() {
        let signal = impulse_train(120.0, 2.0);
        let malformed = RhythmConfig {
            spectral_max_radius: usize::MAX,
            threshold_window_seconds: f32::INFINITY,
            ..RhythmConfig::default()
        };
        let analysis = analyze_mono(&signal, RATE, &malformed);
        assert_eq!(analysis.status, AnalysisStatus::InvalidConfiguration);
        assert!(analysis.hits.is_empty());
    }

    #[test]
    fn configured_span_limit_is_exact_and_threshold_is_gain_robust() {
        let signal = impulse_train(126.0, 5.0);
        let quiet: Vec<f32> = signal.iter().map(|sample| sample * 0.02).collect();
        let config = RhythmConfig {
            fft_size: 512,
            hop_size: 64,
            maximum_span_seconds: 0.035,
            ..RhythmConfig::default()
        };
        let loud_analysis = analyze_mono(&signal, RATE, &config);
        let quiet_analysis = analyze_mono(&quiet, RATE, &config);
        let maximum_samples = (config.maximum_span_seconds * RATE as f32).round() as usize;
        assert!(loud_analysis
            .hits
            .iter()
            .all(|hit| hit.span.len() <= maximum_samples));
        assert!(loud_analysis.hits.len().abs_diff(quiet_analysis.hits.len()) <= 1);
    }

    #[test]
    fn uniform_pulse_does_not_fabricate_meter_or_downbeat() {
        let config = RhythmConfig {
            fft_size: 512,
            hop_size: 64,
            ..RhythmConfig::default()
        };
        let analysis = analyze_mono(&impulse_train(120.0, 12.0), RATE, &config);
        assert!(!analysis.beat_phase_hypotheses.is_empty());
        assert!(
            analysis.downbeat_hypotheses.is_empty(),
            "uniform downbeats: {:?}",
            analysis.downbeat_hypotheses
        );
    }

    #[test]
    fn recurring_accent_supports_a_four_beat_downbeat_hypothesis() {
        let mut signal = vec![0.0; RATE as usize * 12];
        let period = RATE as usize / 2;
        for (beat, start) in (0..signal.len()).step_by(period).enumerate() {
            let gain = if beat % 4 == 0 { 1.0 } else { 0.38 };
            for offset in 0..800.min(signal.len() - start) {
                signal[start + offset] += gain
                    * (-(offset as f32) / 130.0).exp()
                    * (std::f32::consts::TAU * 70.0 * offset as f32 / RATE as f32).sin();
            }
        }
        let config = RhythmConfig {
            fft_size: 512,
            hop_size: 64,
            ..RhythmConfig::default()
        };
        let analysis = analyze_mono(&signal, RATE, &config);
        assert!(analysis
            .downbeat_hypotheses
            .iter()
            .any(|hypothesis| hypothesis.meter_beats == 4 && hypothesis.score > 0.1));
    }
}
