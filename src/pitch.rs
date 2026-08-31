//! Deterministic pitch evidence for mono PCM.
//!
//! This module measures periodicity and harmonic spectral structure.  It does
//! not infer instruments, performers, or physical sound sources.  In
//! particular, a spectral candidate can be a fundamental, a subharmonic
//! explanation, or a prominent partial; callers should preserve the returned
//! support fields when presenting it as evidence.
//!
//! Tracks use a complete frame grid.  [`PitchTrackPoint::hz`] is `None` when a
//! track has no supported observation, so downstream AIR pitch trajectories do
//! not need to interpolate across analysis gaps.

use std::error::Error;
use std::f32::consts::TAU;
use std::fmt;

const EPSILON: f32 = 1.0e-12;

/// Configuration for the complete pitch-evidence pass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PitchConfig {
    pub frame_size: usize,
    pub hop_size: usize,
    pub yin: YinConfig,
    pub spectral: SpectralConfig,
    pub tracking: TrackingConfig,
}

impl Default for PitchConfig {
    fn default() -> Self {
        Self {
            frame_size: 2_048,
            hop_size: 256,
            yin: YinConfig::default(),
            spectral: SpectralConfig::default(),
            tracking: TrackingConfig::default(),
        }
    }
}

/// Controls the monophonic YIN/CMNDF observation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct YinConfig {
    pub min_hz: f32,
    pub max_hz: f32,
    /// First CMNDF valley below this value is preferred.
    pub threshold: f32,
    /// Frames below this RMS level are explicitly unvoiced.
    pub silence_rms: f32,
    /// Minimum `1 - CMNDF` required for a returned observation.
    pub min_confidence: f32,
}

impl Default for YinConfig {
    fn default() -> Self {
        Self {
            min_hz: 50.0,
            max_hz: 2_000.0,
            threshold: 0.15,
            silence_rms: 1.0e-4,
            min_confidence: 0.55,
        }
    }
}

/// Controls harmonic candidate extraction from a Hann-windowed spectrum.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpectralConfig {
    /// Zero-padded FFT length. Must be a power of two and at least frame size.
    pub fft_size: usize,
    pub max_harmonics: usize,
    pub max_candidates: usize,
    /// Local peaks weaker than this fraction of the strongest bin are ignored.
    pub min_peak_relative: f32,
    /// Minimum heuristic harmonic score for a candidate.
    pub min_candidate_confidence: f32,
    /// Candidate explanations closer than this are consolidated.
    pub merge_cents: f32,
}

impl Default for SpectralConfig {
    fn default() -> Self {
        Self {
            fft_size: 2_048,
            max_harmonics: 8,
            max_candidates: 4,
            min_peak_relative: 0.025,
            min_candidate_confidence: 0.12,
            merge_cents: 38.0,
        }
    }
}

/// Controls deterministic nearest-continuation tracking and descriptors.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackingConfig {
    pub min_candidate_confidence: f32,
    pub max_jump_semitones: f32,
    pub max_gap_frames: usize,
    pub min_track_points: usize,
    pub glide_min_semitones: f32,
    pub glide_min_fit: f32,
    pub vibrato_min_rate_hz: f32,
    pub vibrato_max_rate_hz: f32,
    pub vibrato_min_extent_semitones: f32,
}

impl Default for TrackingConfig {
    fn default() -> Self {
        Self {
            min_candidate_confidence: 0.20,
            max_jump_semitones: 5.0,
            max_gap_frames: 2,
            min_track_points: 2,
            glide_min_semitones: 0.65,
            glide_min_fit: 0.72,
            vibrato_min_rate_hz: 3.0,
            vibrato_max_rate_hz: 12.0,
            vibrato_min_extent_semitones: 0.08,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateMethod {
    Yin,
    HarmonicSpectrum,
    Combined,
}

/// Independent measurements supporting a pitch candidate.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PitchSupport {
    /// `1 - CMNDF`, or zero when no YIN observation supported the candidate.
    pub periodicity: f32,
    /// Weighted harmonic-bin agreement, normalized to `[0, 1]`.
    pub harmonicity: f32,
    /// Candidate fundamental-bin magnitude relative to the strongest bin.
    pub spectral_prominence: f32,
    pub level_dbfs: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PitchCandidate {
    pub hz: f32,
    /// A deterministic ranking confidence, not a calibrated probability.
    pub confidence: f32,
    pub support: PitchSupport,
    pub method: CandidateMethod,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PitchFrame {
    /// Center of the analysis window in source/project frames.
    pub offset_frames: u64,
    pub rms: f32,
    pub voiced: bool,
    /// Confidence of the strongest observation, or zero for an unvoiced frame.
    pub voicing_confidence: f32,
    pub candidates: Vec<PitchCandidate>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PitchTrackPoint {
    pub offset_frames: u64,
    pub hz: Option<f32>,
    pub confidence: f32,
    pub support: PitchSupport,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlideDirection {
    Rising,
    Falling,
}

/// Descriptive contour evidence.  Neither variant asserts a modulation source.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ModulationEvidence {
    Glide {
        start_offset_frames: u64,
        end_offset_frames: u64,
        direction: GlideDirection,
        semitones_per_second: f32,
        extent_semitones: f32,
        confidence: f32,
    },
    Vibrato {
        start_offset_frames: u64,
        end_offset_frames: u64,
        rate_hz: f32,
        extent_semitones: f32,
        confidence: f32,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct PitchTrack {
    pub points: Vec<PitchTrackPoint>,
    pub confidence: f32,
    pub voiced_points: usize,
    pub modulation: Vec<ModulationEvidence>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PitchAnalysis {
    pub sample_rate: u32,
    pub frame_size: usize,
    pub hop_size: usize,
    pub frames: Vec<PitchFrame>,
    pub tracks: Vec<PitchTrack>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PitchError {
    ZeroSampleRate,
    EmptyFrame,
    ZeroHop,
    InvalidFftSize { fft_size: usize, frame_size: usize },
    InvalidFrequencyRange { minimum: f32, maximum: f32 },
    InvalidConfiguration(&'static str),
    NonFiniteSample { index: usize, value: f32 },
}

impl fmt::Display for PitchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSampleRate => write!(formatter, "sample rate must be positive"),
            Self::EmptyFrame => write!(formatter, "pitch frame size must be positive"),
            Self::ZeroHop => write!(formatter, "pitch hop size must be positive"),
            Self::InvalidFftSize {
                fft_size,
                frame_size,
            } => write!(
                formatter,
                "FFT size {fft_size} must be a power of two and at least frame size {frame_size}"
            ),
            Self::InvalidFrequencyRange { minimum, maximum } => write!(
                formatter,
                "pitch range must be finite, positive, and increasing; got {minimum}..{maximum} Hz"
            ),
            Self::InvalidConfiguration(message) => write!(formatter, "{message}"),
            Self::NonFiniteSample { index, value } => {
                write!(formatter, "sample {index} is not finite: {value}")
            }
        }
    }
}

impl Error for PitchError {}

/// Analyze mono PCM and return frame observations plus gap-preserving tracks.
pub fn analyze_pitch(
    samples: &[f32],
    sample_rate: u32,
    config: PitchConfig,
) -> Result<PitchAnalysis, PitchError> {
    validate_config(sample_rate, config)?;
    if let Some((index, value)) = samples
        .iter()
        .copied()
        .enumerate()
        .find(|(_, value)| !value.is_finite())
    {
        return Err(PitchError::NonFiniteSample { index, value });
    }
    if samples.is_empty() {
        return Ok(PitchAnalysis {
            sample_rate,
            frame_size: config.frame_size,
            hop_size: config.hop_size,
            frames: Vec::new(),
            tracks: Vec::new(),
        });
    }

    let frame_count = (samples.len() - 1) / config.hop_size + 1;
    let mut frames = Vec::with_capacity(frame_count);
    let mut buffer = vec![0.0; config.frame_size];
    for frame_index in 0..frame_count {
        let start = frame_index * config.hop_size;
        buffer.fill(0.0);
        let available = (samples.len() - start).min(config.frame_size);
        buffer[..available].copy_from_slice(&samples[start..start + available]);
        // Partial tail windows use the center of their actual source support,
        // keeping offsets increasing without pretending zero padding is audio.
        let center = start.saturating_add(available / 2).min(samples.len() - 1);
        frames.push(analyze_frame(&buffer, center as u64, sample_rate, config));
    }
    let tracks = track_pitch_frames(&frames, sample_rate, config.hop_size, config.tracking);
    Ok(PitchAnalysis {
        sample_rate,
        frame_size: config.frame_size,
        hop_size: config.hop_size,
        frames,
        tracks,
    })
}

/// Return the YIN observation for one frame, if sufficiently periodic/voiced.
pub fn estimate_yin(frame: &[f32], sample_rate: u32, config: YinConfig) -> Option<PitchCandidate> {
    if frame.len() < 4 || sample_rate == 0 {
        return None;
    }
    let rms = root_mean_square(frame);
    if rms < config.silence_rms || !rms.is_finite() {
        return None;
    }
    let minimum_tau = ((sample_rate as f32 / config.max_hz).floor() as usize).max(2);
    let maximum_tau = ((sample_rate as f32 / config.min_hz).ceil() as usize)
        .min(frame.len().saturating_sub(2) / 2);
    if minimum_tau >= maximum_tau {
        return None;
    }

    let mean = frame.iter().sum::<f32>() / frame.len() as f32;
    let comparison_length = frame.len() - maximum_tau;
    let mut difference = vec![0.0_f32; maximum_tau + 1];
    for tau in 1..=maximum_tau {
        let mut sum = 0.0_f64;
        for index in 0..comparison_length {
            let delta = (frame[index] - mean) - (frame[index + tau] - mean);
            sum += f64::from(delta * delta);
        }
        difference[tau] = sum as f32;
    }

    let mut cmndf = vec![1.0_f32; maximum_tau + 1];
    let mut running_sum = 0.0_f64;
    for tau in 1..=maximum_tau {
        running_sum += f64::from(difference[tau]);
        if running_sum > f64::EPSILON {
            cmndf[tau] = (f64::from(difference[tau]) * tau as f64 / running_sum) as f32;
        }
    }

    let mut tau = minimum_tau;
    while tau <= maximum_tau {
        if cmndf[tau] < config.threshold {
            while tau < maximum_tau && cmndf[tau + 1] < cmndf[tau] {
                tau += 1;
            }
            break;
        }
        tau += 1;
    }
    if tau > maximum_tau {
        tau = (minimum_tau..=maximum_tau).min_by(|left, right| {
            cmndf[*left]
                .total_cmp(&cmndf[*right])
                .then_with(|| left.cmp(right))
        })?;
    }

    let refined_tau = parabolic_minimum(&cmndf, tau).max(1.0);
    let hz = sample_rate as f32 / refined_tau;
    let confidence = (1.0 - cmndf[tau]).clamp(0.0, 1.0);
    if confidence < config.min_confidence || hz < config.min_hz || hz > config.max_hz {
        return None;
    }
    Some(PitchCandidate {
        hz,
        confidence,
        support: PitchSupport {
            periodicity: confidence,
            level_dbfs: amplitude_dbfs(rms),
            ..PitchSupport::default()
        },
        method: CandidateMethod::Yin,
    })
}

/// Return harmonic explanations for one frame, strongest first.
pub fn harmonic_spectral_candidates(
    frame: &[f32],
    sample_rate: u32,
    minimum_hz: f32,
    maximum_hz: f32,
    config: SpectralConfig,
) -> Vec<PitchCandidate> {
    if frame.is_empty()
        || sample_rate == 0
        || config.fft_size < frame.len()
        || !config.fft_size.is_power_of_two()
        || config.max_harmonics == 0
        || config.max_candidates == 0
    {
        return Vec::new();
    }
    let rms = root_mean_square(frame);
    if rms <= EPSILON {
        return Vec::new();
    }

    let mut spectrum = vec![Complex32::default(); config.fft_size];
    let denominator = frame.len().saturating_sub(1).max(1) as f32;
    let mean = frame.iter().sum::<f32>() / frame.len() as f32;
    for (index, sample) in frame.iter().copied().enumerate() {
        let window = 0.5 - 0.5 * (TAU * index as f32 / denominator).cos();
        spectrum[index].re = (sample - mean) * window;
    }
    fft_in_place(&mut spectrum);
    let nyquist_bin = config.fft_size / 2;
    let magnitudes: Vec<f32> = spectrum[..=nyquist_bin]
        .iter()
        .map(|value| value.norm())
        .collect();
    let strongest = magnitudes.iter().copied().skip(1).fold(0.0_f32, f32::max);
    if strongest <= EPSILON {
        return Vec::new();
    }
    let bin_hz = sample_rate as f32 / config.fft_size as f32;
    let mut peaks = Vec::new();
    for bin in 2..nyquist_bin {
        if magnitudes[bin] >= strongest * config.min_peak_relative
            && magnitudes[bin] > magnitudes[bin - 1]
            && magnitudes[bin] >= magnitudes[bin + 1]
        {
            peaks.push((refined_peak_bin(&magnitudes, bin), magnitudes[bin]));
        }
    }
    peaks.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.total_cmp(&right.0))
    });
    // A bounded peak set keeps proposal generation cheap and deterministic.
    peaks.truncate(
        config
            .max_candidates
            .saturating_mul(config.max_harmonics)
            .max(16),
    );

    let mut proposals = Vec::new();
    for &(peak_bin, _) in &peaks {
        let peak_hz = peak_bin * bin_hz;
        for harmonic in 1..=config.max_harmonics {
            let fundamental = peak_hz / harmonic as f32;
            if fundamental >= minimum_hz && fundamental <= maximum_hz {
                proposals.push(fundamental);
            }
        }
    }
    proposals.sort_by(f32::total_cmp);
    proposals.dedup_by(|left, right| cents_distance(*left, *right) < 12.0);

    let weight_sum: f32 = (1..=config.max_harmonics)
        .map(|harmonic| 1.0 / harmonic as f32)
        .sum();
    let mut candidates = Vec::new();
    for proposal in proposals {
        let mut weighted = 0.0;
        let mut matched_weight = 0.0;
        let mut frequency_numerator = 0.0;
        let mut frequency_denominator = 0.0;
        let mut fundamental_prominence = 0.0;
        for harmonic in 1..=config.max_harmonics {
            let target = proposal * harmonic as f32 / bin_hz;
            if target >= nyquist_bin as f32 {
                break;
            }
            let bin = target.round() as usize;
            let (peak_index, magnitude) = neighborhood_maximum(&magnitudes, bin);
            let relative = magnitude / strongest;
            let weight = 1.0 / harmonic as f32;
            if relative >= config.min_peak_relative {
                weighted += relative * weight;
                matched_weight += weight;
                let implied = refined_peak_bin(&magnitudes, peak_index) * bin_hz / harmonic as f32;
                frequency_numerator += implied * relative * weight;
                frequency_denominator += relative * weight;
            }
            if harmonic == 1 {
                fundamental_prominence = relative;
            }
        }
        let harmonicity = (weighted / weight_sum).clamp(0.0, 1.0);
        let coverage = (matched_weight / weight_sum).clamp(0.0, 1.0);
        let confidence =
            (0.65 * harmonicity + 0.25 * fundamental_prominence.clamp(0.0, 1.0) + 0.10 * coverage)
                .clamp(0.0, 1.0);
        if confidence >= config.min_candidate_confidence {
            candidates.push(PitchCandidate {
                hz: if frequency_denominator > EPSILON {
                    frequency_numerator / frequency_denominator
                } else {
                    proposal
                },
                confidence,
                support: PitchSupport {
                    periodicity: 0.0,
                    harmonicity,
                    spectral_prominence: fundamental_prominence.clamp(0.0, 1.0),
                    level_dbfs: amplitude_dbfs(rms),
                },
                method: CandidateMethod::HarmonicSpectrum,
            });
        }
    }
    candidates.sort_by(candidate_order);
    consolidate_candidates(&mut candidates, config.merge_cents);
    candidates.truncate(config.max_candidates);
    candidates
}

fn analyze_frame(
    frame: &[f32],
    offset_frames: u64,
    sample_rate: u32,
    config: PitchConfig,
) -> PitchFrame {
    let rms = root_mean_square(frame);
    if rms < config.yin.silence_rms {
        return PitchFrame {
            offset_frames,
            rms,
            voiced: false,
            voicing_confidence: 0.0,
            candidates: Vec::new(),
        };
    }
    let yin = estimate_yin(frame, sample_rate, config.yin);
    let mut candidates = harmonic_spectral_candidates(
        frame,
        sample_rate,
        config.yin.min_hz,
        config.yin.max_hz,
        config.spectral,
    );
    if let Some(yin) = yin {
        candidates.push(yin);
    }
    candidates.sort_by(candidate_order);
    consolidate_candidates(&mut candidates, config.spectral.merge_cents);
    candidates.truncate(config.spectral.max_candidates);
    let voicing_confidence = candidates
        .first()
        .map(|candidate| candidate.confidence)
        .unwrap_or(0.0);
    PitchFrame {
        offset_frames,
        rms,
        voiced: voicing_confidence >= config.tracking.min_candidate_confidence,
        voicing_confidence,
        candidates,
    }
}

/// Link frame candidates into deterministic, full-grid trajectories.
pub fn track_pitch_frames(
    frames: &[PitchFrame],
    sample_rate: u32,
    hop_size: usize,
    config: TrackingConfig,
) -> Vec<PitchTrack> {
    #[derive(Clone, Debug)]
    struct WorkingTrack {
        points: Vec<PitchTrackPoint>,
        last_hz: f32,
        gap: usize,
    }

    let mut tracks: Vec<WorkingTrack> = Vec::new();
    for (frame_index, frame) in frames.iter().enumerate() {
        let eligible: Vec<(usize, &PitchCandidate)> = frame
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, candidate)| candidate.confidence >= config.min_candidate_confidence)
            .collect();
        let mut links = Vec::new();
        for (track_index, track) in tracks.iter().enumerate() {
            if track.gap > config.max_gap_frames {
                continue;
            }
            for &(candidate_index, candidate) in &eligible {
                let distance = semitone_distance(track.last_hz, candidate.hz);
                let allowed = config.max_jump_semitones * (track.gap + 1) as f32;
                if distance <= allowed {
                    let cost =
                        distance / allowed.max(EPSILON) + (1.0 - candidate.confidence) * 0.075;
                    links.push((cost, track_index, candidate_index));
                }
            }
        }
        links.sort_by(|left, right| {
            left.0
                .total_cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.2.cmp(&right.2))
        });
        let mut used_tracks = vec![false; tracks.len()];
        let mut used_candidates = vec![false; frame.candidates.len()];
        for (_, track_index, candidate_index) in links {
            if !used_tracks[track_index] && !used_candidates[candidate_index] {
                let candidate = frame.candidates[candidate_index];
                tracks[track_index]
                    .points
                    .push(track_point(frame, Some(candidate)));
                tracks[track_index].last_hz = candidate.hz;
                tracks[track_index].gap = 0;
                used_tracks[track_index] = true;
                used_candidates[candidate_index] = true;
            }
        }
        for (track_index, track) in tracks.iter_mut().enumerate() {
            if !used_tracks[track_index] {
                track.points.push(track_point(frame, None));
                track.gap = track.gap.saturating_add(1);
            }
        }
        for (candidate_index, candidate) in frame.candidates.iter().copied().enumerate() {
            if candidate.confidence < config.min_candidate_confidence
                || used_candidates[candidate_index]
            {
                continue;
            }
            let mut points = Vec::with_capacity(frames.len());
            for earlier in &frames[..frame_index] {
                points.push(track_point(earlier, None));
            }
            points.push(track_point(frame, Some(candidate)));
            tracks.push(WorkingTrack {
                points,
                last_hz: candidate.hz,
                gap: 0,
            });
        }
    }

    let mut result = Vec::new();
    for mut track in tracks {
        while track.points.len() < frames.len() {
            let frame = &frames[track.points.len()];
            track.points.push(track_point(frame, None));
        }
        let voiced_points = track
            .points
            .iter()
            .filter(|point| point.hz.is_some())
            .count();
        if voiced_points < config.min_track_points {
            continue;
        }
        let confidence = track
            .points
            .iter()
            .filter(|point| point.hz.is_some())
            .map(|point| point.confidence)
            .sum::<f32>()
            / voiced_points as f32;
        let modulation = describe_modulation(&track.points, sample_rate, hop_size, config);
        result.push(PitchTrack {
            points: track.points,
            confidence,
            voiced_points,
            modulation,
        });
    }
    result.sort_by(|left, right| {
        let left_hz = first_hz(&left.points).unwrap_or(f32::INFINITY);
        let right_hz = first_hz(&right.points).unwrap_or(f32::INFINITY);
        left_hz
            .total_cmp(&right_hz)
            .then_with(|| right.voiced_points.cmp(&left.voiced_points))
    });
    result
}

fn describe_modulation(
    points: &[PitchTrackPoint],
    sample_rate: u32,
    hop_size: usize,
    config: TrackingConfig,
) -> Vec<ModulationEvidence> {
    if sample_rate == 0 || hop_size == 0 {
        return Vec::new();
    }
    let mut result = Vec::new();
    let mut start = 0;
    while start < points.len() {
        while start < points.len() && points[start].hz.is_none() {
            start += 1;
        }
        if start == points.len() {
            break;
        }
        let mut end = start + 1;
        while end < points.len() && points[end].hz.is_some() {
            end += 1;
        }
        if end - start >= 5 {
            describe_run(
                &points[start..end],
                sample_rate,
                hop_size,
                config,
                &mut result,
            );
        }
        start = end;
    }
    result
}

fn describe_run(
    points: &[PitchTrackPoint],
    sample_rate: u32,
    hop_size: usize,
    config: TrackingConfig,
    output: &mut Vec<ModulationEvidence>,
) {
    let values: Vec<f32> = points
        .iter()
        .map(|point| 12.0 * point.hz.unwrap().max(EPSILON).log2())
        .collect();
    let count = values.len();
    let mean_x = (count - 1) as f32 * 0.5;
    let mean_y = values.iter().sum::<f32>() / count as f32;
    let mut xx = 0.0;
    let mut xy = 0.0;
    for (index, value) in values.iter().copied().enumerate() {
        let x = index as f32 - mean_x;
        xx += x * x;
        xy += x * (value - mean_y);
    }
    let slope_per_frame = if xx > EPSILON { xy / xx } else { 0.0 };
    let intercept = mean_y - slope_per_frame * mean_x;
    let mut residual = Vec::with_capacity(count);
    let mut squared_residual = 0.0;
    let mut squared_total = 0.0;
    for (index, value) in values.iter().copied().enumerate() {
        let error = value - (intercept + slope_per_frame * index as f32);
        residual.push(error);
        squared_residual += error * error;
        let centered = value - mean_y;
        squared_total += centered * centered;
    }
    let fit = if squared_total <= EPSILON {
        0.0
    } else {
        (1.0 - squared_residual / squared_total).clamp(0.0, 1.0)
    };
    let average_confidence =
        points.iter().map(|point| point.confidence).sum::<f32>() / count as f32;
    let duration_seconds = (count - 1) as f32 * hop_size as f32 / sample_rate as f32;
    let extent = slope_per_frame.abs() * (count - 1) as f32;
    if extent >= config.glide_min_semitones && fit >= config.glide_min_fit {
        output.push(ModulationEvidence::Glide {
            start_offset_frames: points[0].offset_frames,
            end_offset_frames: points[count - 1].offset_frames,
            direction: if slope_per_frame >= 0.0 {
                GlideDirection::Rising
            } else {
                GlideDirection::Falling
            },
            semitones_per_second: if duration_seconds > 0.0 {
                slope_per_frame * (count - 1) as f32 / duration_seconds
            } else {
                0.0
            },
            extent_semitones: extent,
            confidence: (fit * average_confidence).clamp(0.0, 1.0),
        });
    }

    let residual_mean = residual.iter().sum::<f32>() / count as f32;
    for value in &mut residual {
        *value -= residual_mean;
    }
    let variance = residual.iter().map(|value| value * value).sum::<f32>() / count as f32;
    let vibrato_extent = 2.0 * (2.0 * variance).sqrt();
    if vibrato_extent < config.vibrato_min_extent_semitones {
        return;
    }
    let frame_rate = sample_rate as f32 / hop_size as f32;
    let minimum_lag = (frame_rate / config.vibrato_max_rate_hz).floor().max(1.0) as usize;
    let maximum_lag = (frame_rate / config.vibrato_min_rate_hz).ceil() as usize;
    let maximum_lag = maximum_lag.min(count.saturating_sub(2));
    if minimum_lag > maximum_lag {
        return;
    }
    let mut best: Option<(f32, usize)> = None;
    for lag in minimum_lag..=maximum_lag {
        let mut cross = 0.0;
        let mut left_energy = 0.0;
        let mut right_energy = 0.0;
        for index in 0..count - lag {
            cross += residual[index] * residual[index + lag];
            left_energy += residual[index] * residual[index];
            right_energy += residual[index + lag] * residual[index + lag];
        }
        let correlation = cross / (left_energy * right_energy).sqrt().max(EPSILON);
        let replace = best
            .map(|(best_correlation, best_lag)| {
                correlation > best_correlation
                    || (correlation == best_correlation && lag < best_lag)
            })
            .unwrap_or(true);
        if replace {
            best = Some((correlation, lag));
        }
    }
    if let Some((correlation, lag)) = best {
        if correlation >= 0.30 {
            output.push(ModulationEvidence::Vibrato {
                start_offset_frames: points[0].offset_frames,
                end_offset_frames: points[count - 1].offset_frames,
                rate_hz: frame_rate / lag as f32,
                extent_semitones: vibrato_extent,
                confidence: (correlation * average_confidence).clamp(0.0, 1.0),
            });
        }
    }
}

fn validate_config(sample_rate: u32, config: PitchConfig) -> Result<(), PitchError> {
    if sample_rate == 0 {
        return Err(PitchError::ZeroSampleRate);
    }
    if config.frame_size == 0 {
        return Err(PitchError::EmptyFrame);
    }
    if config.hop_size == 0 {
        return Err(PitchError::ZeroHop);
    }
    if config.spectral.fft_size < config.frame_size || !config.spectral.fft_size.is_power_of_two() {
        return Err(PitchError::InvalidFftSize {
            fft_size: config.spectral.fft_size,
            frame_size: config.frame_size,
        });
    }
    if !config.yin.min_hz.is_finite()
        || !config.yin.max_hz.is_finite()
        || config.yin.min_hz <= 0.0
        || config.yin.min_hz >= config.yin.max_hz
        || config.yin.max_hz >= sample_rate as f32 * 0.5
    {
        return Err(PitchError::InvalidFrequencyRange {
            minimum: config.yin.min_hz,
            maximum: config.yin.max_hz,
        });
    }
    let unit_values = [
        config.yin.threshold,
        config.yin.min_confidence,
        config.spectral.min_peak_relative,
        config.spectral.min_candidate_confidence,
        config.tracking.min_candidate_confidence,
        config.tracking.glide_min_fit,
    ];
    if unit_values
        .iter()
        .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
    {
        return Err(PitchError::InvalidConfiguration(
            "thresholds and confidence cutoffs must be finite values in [0, 1]",
        ));
    }
    if !config.yin.silence_rms.is_finite()
        || config.yin.silence_rms < 0.0
        || config.spectral.max_harmonics == 0
        || config.spectral.max_candidates == 0
        || !config.spectral.merge_cents.is_finite()
        || config.spectral.merge_cents < 0.0
        || !config.tracking.max_jump_semitones.is_finite()
        || config.tracking.max_jump_semitones <= 0.0
        || config.tracking.min_track_points == 0
        || !config.tracking.glide_min_semitones.is_finite()
        || config.tracking.glide_min_semitones < 0.0
        || !config.tracking.vibrato_min_rate_hz.is_finite()
        || !config.tracking.vibrato_max_rate_hz.is_finite()
        || config.tracking.vibrato_min_rate_hz <= 0.0
        || config.tracking.vibrato_min_rate_hz >= config.tracking.vibrato_max_rate_hz
        || !config.tracking.vibrato_min_extent_semitones.is_finite()
        || config.tracking.vibrato_min_extent_semitones < 0.0
    {
        return Err(PitchError::InvalidConfiguration(
            "pitch configuration contains an invalid count, range, or magnitude",
        ));
    }
    Ok(())
}

fn consolidate_candidates(candidates: &mut Vec<PitchCandidate>, merge_cents: f32) {
    candidates.sort_by(candidate_order);
    let mut merged: Vec<PitchCandidate> = Vec::new();
    for candidate in candidates.drain(..) {
        if let Some(existing) = merged
            .iter_mut()
            .find(|existing| cents_distance(existing.hz, candidate.hz) <= merge_cents)
        {
            let left_weight = existing.confidence.max(EPSILON);
            let right_weight = candidate.confidence.max(EPSILON);
            existing.hz = (existing.hz * left_weight + candidate.hz * right_weight)
                / (left_weight + right_weight);
            existing.confidence =
                (1.0 - (1.0 - existing.confidence) * (1.0 - candidate.confidence)).clamp(0.0, 1.0);
            existing.support.periodicity = existing
                .support
                .periodicity
                .max(candidate.support.periodicity);
            existing.support.harmonicity = existing
                .support
                .harmonicity
                .max(candidate.support.harmonicity);
            existing.support.spectral_prominence = existing
                .support
                .spectral_prominence
                .max(candidate.support.spectral_prominence);
            existing.support.level_dbfs = existing
                .support
                .level_dbfs
                .max(candidate.support.level_dbfs);
            existing.method = if existing.method == candidate.method {
                existing.method
            } else {
                CandidateMethod::Combined
            };
        } else {
            merged.push(candidate);
        }
    }
    merged.sort_by(candidate_order);
    *candidates = merged;
}

fn candidate_order(left: &PitchCandidate, right: &PitchCandidate) -> std::cmp::Ordering {
    right
        .confidence
        .total_cmp(&left.confidence)
        .then_with(|| {
            right
                .support
                .periodicity
                .total_cmp(&left.support.periodicity)
        })
        .then_with(|| left.hz.total_cmp(&right.hz))
}

fn track_point(frame: &PitchFrame, candidate: Option<PitchCandidate>) -> PitchTrackPoint {
    match candidate {
        Some(candidate) => PitchTrackPoint {
            offset_frames: frame.offset_frames,
            hz: Some(candidate.hz),
            confidence: candidate.confidence,
            support: candidate.support,
        },
        None => PitchTrackPoint {
            offset_frames: frame.offset_frames,
            hz: None,
            confidence: 0.0,
            support: PitchSupport {
                level_dbfs: amplitude_dbfs(frame.rms),
                ..PitchSupport::default()
            },
        },
    }
}

fn first_hz(points: &[PitchTrackPoint]) -> Option<f32> {
    points.iter().find_map(|point| point.hz)
}

fn root_mean_square(values: &[f32]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    (values
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        / values.len() as f64)
        .sqrt() as f32
}

fn amplitude_dbfs(amplitude: f32) -> f32 {
    20.0 * amplitude.max(1.0e-12).log10()
}

fn cents_distance(left: f32, right: f32) -> f32 {
    1_200.0 * (left.max(EPSILON) / right.max(EPSILON)).log2().abs()
}

fn semitone_distance(left: f32, right: f32) -> f32 {
    cents_distance(left, right) / 100.0
}

fn parabolic_minimum(values: &[f32], index: usize) -> f32 {
    if index == 0 || index + 1 >= values.len() {
        return index as f32;
    }
    let left = values[index - 1];
    let center = values[index];
    let right = values[index + 1];
    let denominator = left - 2.0 * center + right;
    if denominator.abs() <= EPSILON {
        index as f32
    } else {
        index as f32 + (0.5 * (left - right) / denominator).clamp(-1.0, 1.0)
    }
}

fn refined_peak_bin(magnitudes: &[f32], index: usize) -> f32 {
    if index == 0 || index + 1 >= magnitudes.len() {
        return index as f32;
    }
    let left = magnitudes[index - 1];
    let center = magnitudes[index];
    let right = magnitudes[index + 1];
    let denominator = left - 2.0 * center + right;
    if denominator.abs() <= EPSILON {
        index as f32
    } else {
        index as f32 + (0.5 * (left - right) / denominator).clamp(-0.5, 0.5)
    }
}

fn neighborhood_maximum(magnitudes: &[f32], center: usize) -> (usize, f32) {
    let start = center.saturating_sub(1);
    let end = center.saturating_add(1).min(magnitudes.len() - 1);
    (start..=end)
        .map(|index| (index, magnitudes[index]))
        .max_by(|left, right| {
            left.1
                .total_cmp(&right.1)
                .then_with(|| right.0.cmp(&left.0))
        })
        .unwrap_or((center, magnitudes[center]))
}

#[derive(Clone, Copy, Debug, Default)]
struct Complex32 {
    re: f32,
    im: f32,
}

impl Complex32 {
    fn norm(self) -> f32 {
        (self.re * self.re + self.im * self.im).sqrt()
    }

    fn add(self, other: Self) -> Self {
        Self {
            re: self.re + other.re,
            im: self.im + other.im,
        }
    }

    fn sub(self, other: Self) -> Self {
        Self {
            re: self.re - other.re,
            im: self.im - other.im,
        }
    }

    fn mul(self, other: Self) -> Self {
        Self {
            re: self.re * other.re - self.im * other.im,
            im: self.re * other.im + self.im * other.re,
        }
    }
}

/// In-place radix-2 FFT kept private so the evidence extractor has no mandatory
/// dependency beyond `std`.  The application may later swap in its FFT planner.
fn fft_in_place(values: &mut [Complex32]) {
    let length = values.len();
    debug_assert!(length.is_power_of_two());
    let mut target = 0;
    for source in 1..length {
        let mut bit = length >> 1;
        while target & bit != 0 {
            target ^= bit;
            bit >>= 1;
        }
        target ^= bit;
        if source < target {
            values.swap(source, target);
        }
    }
    let mut span = 2;
    while span <= length {
        let angle = -TAU / span as f32;
        let step = Complex32 {
            re: angle.cos(),
            im: angle.sin(),
        };
        for start in (0..length).step_by(span) {
            let mut twiddle = Complex32 { re: 1.0, im: 0.0 };
            for offset in 0..span / 2 {
                let even = values[start + offset];
                let odd = values[start + offset + span / 2].mul(twiddle);
                values[start + offset] = even.add(odd);
                values[start + offset + span / 2] = even.sub(odd);
                twiddle = twiddle.mul(step);
            }
        }
        span <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE: u32 = 16_000;

    fn test_config() -> PitchConfig {
        PitchConfig {
            frame_size: 1_024,
            hop_size: 128,
            yin: YinConfig {
                min_hz: 70.0,
                max_hz: 1_500.0,
                ..YinConfig::default()
            },
            spectral: SpectralConfig {
                fft_size: 2_048,
                max_candidates: 4,
                ..SpectralConfig::default()
            },
            ..PitchConfig::default()
        }
    }

    fn tone(frequency: f32, seconds: f32) -> Vec<f32> {
        let length = (SAMPLE_RATE as f32 * seconds) as usize;
        (0..length)
            .map(|index| (TAU * frequency * index as f32 / SAMPLE_RATE as f32).sin() * 0.6)
            .collect()
    }

    fn median(mut values: Vec<f32>) -> f32 {
        values.sort_by(f32::total_cmp);
        values[values.len() / 2]
    }

    #[test]
    fn silence_is_explicitly_unvoiced() {
        let result = analyze_pitch(
            &vec![0.0; SAMPLE_RATE as usize / 3],
            SAMPLE_RATE,
            test_config(),
        )
        .unwrap();
        assert!(!result.frames.is_empty());
        assert!(result.frames.iter().all(|frame| {
            !frame.voiced && frame.voicing_confidence == 0.0 && frame.candidates.is_empty()
        }));
        assert!(result.tracks.is_empty());
    }

    #[test]
    fn sine_is_measured_near_frequency() {
        let result = analyze_pitch(&tone(440.0, 0.35), SAMPLE_RATE, test_config()).unwrap();
        let estimates: Vec<f32> = result.frames[2..result.frames.len() - 2]
            .iter()
            .filter_map(|frame| frame.candidates.first().map(|candidate| candidate.hz))
            .collect();
        assert!(!estimates.is_empty());
        assert!((median(estimates) - 440.0).abs() < 3.0);
        assert!(result.frames.iter().any(|frame| {
            frame
                .candidates
                .iter()
                .any(|candidate| candidate.support.periodicity > 0.9)
        }));
    }

    #[test]
    fn rising_glissando_produces_contour_and_glide_evidence() {
        let seconds = 0.7;
        let length = (SAMPLE_RATE as f32 * seconds) as usize;
        let mut phase = 0.0;
        let samples: Vec<f32> = (0..length)
            .map(|index| {
                let t = index as f32 / (length - 1) as f32;
                let frequency = 220.0 * (440.0_f32 / 220.0).powf(t);
                phase += TAU * frequency / SAMPLE_RATE as f32;
                phase.sin() * 0.65
            })
            .collect();
        let result = analyze_pitch(&samples, SAMPLE_RATE, test_config()).unwrap();
        let track = result
            .tracks
            .iter()
            .max_by_key(|track| track.voiced_points)
            .unwrap();
        let voiced: Vec<f32> = track.points.iter().filter_map(|point| point.hz).collect();
        assert!(voiced.last().unwrap() > &(voiced.first().unwrap() * 1.7));
        assert!(track.modulation.iter().any(|descriptor| matches!(
            descriptor,
            ModulationEvidence::Glide {
                direction: GlideDirection::Rising,
                extent_semitones,
                ..
            } if *extent_semitones > 8.0
        )));
    }

    #[test]
    fn two_tone_mixture_retains_both_spectral_candidates() {
        let mut samples = tone(440.0, 0.3);
        for (index, sample) in samples.iter_mut().enumerate() {
            *sample += 0.55 * (TAU * 660.0 * index as f32 / SAMPLE_RATE as f32).sin();
        }
        let result = analyze_pitch(&samples, SAMPLE_RATE, test_config()).unwrap();
        let middle = &result.frames[result.frames.len() / 2];
        assert!(middle.candidates.iter().any(|candidate| {
            (candidate.hz - 440.0).abs() < 10.0 && candidate.support.spectral_prominence > 0.25
        }));
        assert!(middle.candidates.iter().any(|candidate| {
            (candidate.hz - 660.0).abs() < 12.0 && candidate.support.spectral_prominence > 0.20
        }));
    }

    #[test]
    fn octave_ambiguity_keeps_support_visible_without_promoting_subharmonic() {
        let frame = tone(440.0, 1_024.0 / SAMPLE_RATE as f32);
        let candidates = harmonic_spectral_candidates(
            &frame,
            SAMPLE_RATE,
            70.0,
            1_500.0,
            test_config().spectral,
        );
        let actual = candidates
            .iter()
            .position(|candidate| (candidate.hz - 440.0).abs() < 10.0)
            .unwrap();
        let subharmonic = candidates
            .iter()
            .position(|candidate| (candidate.hz - 220.0).abs() < 8.0);
        assert_eq!(actual, 0);
        if let Some(subharmonic) = subharmonic {
            assert!(candidates[subharmonic].confidence < candidates[actual].confidence);
            assert!(candidates[subharmonic].support.spectral_prominence < 0.1);
        }
    }

    #[test]
    fn tracking_writes_gaps_instead_of_interpolation() {
        let mut samples = tone(330.0, 0.55);
        for sample in &mut samples[SAMPLE_RATE as usize / 5..SAMPLE_RATE as usize / 4] {
            *sample = 0.0;
        }
        let result = analyze_pitch(&samples, SAMPLE_RATE, test_config()).unwrap();
        let track = result
            .tracks
            .iter()
            .max_by_key(|track| track.voiced_points)
            .unwrap();
        assert_eq!(track.points.len(), result.frames.len());
        assert!(track.points.iter().any(|point| point.hz.is_none()));
        assert!(track.points.iter().any(|point| point.hz.is_some()));
    }

    #[test]
    fn vibrato_descriptor_is_measurement_not_identity() {
        let seconds = 1.0;
        let length = (SAMPLE_RATE as f32 * seconds) as usize;
        let mut phase = 0.0;
        let samples: Vec<f32> = (0..length)
            .map(|index| {
                let time = index as f32 / SAMPLE_RATE as f32;
                let semitones = 0.28 * (TAU * 6.0 * time).sin();
                let frequency = 440.0 * 2.0_f32.powf(semitones / 12.0);
                phase += TAU * frequency / SAMPLE_RATE as f32;
                phase.sin() * 0.6
            })
            .collect();
        let result = analyze_pitch(&samples, SAMPLE_RATE, test_config()).unwrap();
        let track = result
            .tracks
            .iter()
            .max_by_key(|track| track.voiced_points)
            .unwrap();
        assert!(track.modulation.iter().any(|descriptor| matches!(
            descriptor,
            ModulationEvidence::Vibrato { rate_hz, .. } if (*rate_hz - 6.0).abs() < 1.0
        )));
    }

    #[test]
    fn analysis_is_bitwise_deterministic() {
        let samples = tone(523.25, 0.25);
        let first = analyze_pitch(&samples, SAMPLE_RATE, test_config()).unwrap();
        let second = analyze_pitch(&samples, SAMPLE_RATE, test_config()).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn rejects_non_finite_pcm() {
        let error = analyze_pitch(&[0.0, f32::NAN], SAMPLE_RATE, test_config()).unwrap_err();
        assert!(matches!(
            error,
            PitchError::NonFiniteSample { index: 1, .. }
        ));
    }
}
