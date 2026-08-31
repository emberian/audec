use std::io::BufWriter;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use claxon::FlacReader;
use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

use crate::decomposition::{decompose_nonnegative, ComponentDecomposition, DecompositionParams};
use crate::pyramid::WaveformPyramid;
use crate::settings::SpectrumSettings;

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

#[derive(Debug)]
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
    pub components: ComponentDecomposition,
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
        let channels = self.waveform_pyramid.channel_count();
        let frame_count = self.waveform_pyramid.frame_count();
        let start = start_frame.min(frame_count);
        let end = end_frame.min(frame_count).max(start);
        if channels == 0 {
            return Vec::new();
        }
        self.waveform_pyramid.interleaved_pcm()[start * channels..end * channels]
            .chunks_exact(channels)
            .map(|frame| {
                if channels == 1 {
                    frame[0]
                } else {
                    (frame[0] + frame[1]) * 0.5
                }
            })
            .collect()
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

pub fn analyze_file(path: &Path) -> Result<Analysis> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("flac") {
        bail!("offline analysis currently accepts FLAC files; playback support is broader")
    }

    let mut reader =
        FlacReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let stream = reader.streaminfo();
    let sample_rate = stream.sample_rate;
    let channels = stream.channels;
    let bits_per_sample = stream.bits_per_sample;
    let total_frames = stream
        .samples
        .context("the FLAC stream does not declare its sample count")?
        as usize;

    if channels == 0 || total_frames == 0 {
        bail!("the selected FLAC has no audio frames")
    }

    let channel_count = channels as usize;
    let scale = 1.0 / (2_i64.pow(bits_per_sample.saturating_sub(1)) as f32);
    let mut mono = Vec::with_capacity(total_frames);
    let mut interleaved_stereo = Vec::with_capacity(total_frames.saturating_mul(2));
    let mut accumulators = vec![BinAccumulator::default(); WAVEFORM_BINS];
    let mut samples = reader.samples();

    for frame_index in 0..total_frames {
        let mut left = 0.0;
        let mut right = 0.0;
        for channel in 0..channel_count {
            let sample = samples
                .next()
                .context("FLAC ended before its declared sample count")??
                as f32
                * scale;
            if channel == 0 {
                left = sample;
            }
            if channel == 1 {
                right = sample;
            }
        }
        if channel_count == 1 {
            right = left;
        }

        mono.push((left + right) * 0.5);
        interleaved_stereo.extend([left, right]);
        let bin = (frame_index * WAVEFORM_BINS / total_frames).min(WAVEFORM_BINS - 1);
        accumulators[bin].push(left, right);
    }

    let waveform = accumulators
        .iter()
        .copied()
        .map(BinAccumulator::waveform)
        .collect();
    let waveform_pyramid = WaveformPyramid::from_interleaved(&interleaved_stereo, 2);
    let (mut features, spectral_db) = analyze_spectrum(&mono, sample_rate, &accumulators);
    normalize_flux(&mut features);
    let rhythm = analyze_rhythm(&mono, sample_rate);
    let spectral_peak_db = spectral_db.iter().copied().fold(-120.0_f32, f32::max);
    let component_matrix = component_input(&spectral_db, spectral_peak_db);
    let components = decompose_nonnegative(
        &component_matrix,
        SPECTROGRAM_HEIGHT,
        SPECTROGRAM_WIDTH,
        DecompositionParams {
            rank: 6,
            iterations: 60,
            activation_sparsity: 0.004,
            ..DecompositionParams::default()
        },
    )
    .context("factoring recurring spectral components")?;
    let spectrogram_png = encode_spectrogram(&spectral_db, spectral_peak_db, 84.0)?;

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

    Ok(Analysis {
        path: path.to_owned(),
        title,
        album,
        duration_seconds: total_frames as f64 / f64::from(sample_rate),
        sample_rate,
        channels,
        bits_per_sample,
        waveform,
        waveform_pyramid,
        features,
        rhythm,
        components,
        spectral_db,
        spectral_peak_db,
        spectrogram_png,
    })
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

/// Rerun the log-frequency display projection from retained PCM using a
/// lens-selected FFT recipe. Unlike cropping the encoded PNG, this changes the
/// evidence resolution and window function and therefore belongs on a
/// background executor as a fresh transform.
pub fn spectral_projection(mono: &[f32], sample_rate: u32, settings: SpectrumSettings) -> Vec<f32> {
    if mono.is_empty() || sample_rate == 0 {
        return vec![-120.0; SPECTROGRAM_WIDTH * SPECTROGRAM_HEIGHT];
    }
    let settings = settings.normalized(sample_rate);
    let fft_size = settings.fft_size;
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(fft_size);
    let mut input = vec![Complex::default(); fft_size];
    let window: Vec<f32> = (0..fft_size)
        .map(|index| settings.window.coefficient(index, fft_size))
        .collect();
    let frequencies: Vec<f32> = (0..SPECTROGRAM_HEIGHT)
        .map(|index| {
            let fraction = index as f32 / (SPECTROGRAM_HEIGHT - 1) as f32;
            settings.min_frequency_hz
                * (settings.max_frequency_hz / settings.min_frequency_hz).powf(fraction)
        })
        .collect();
    let half_step = (settings.max_frequency_hz / settings.min_frequency_hz)
        .powf(0.5 / (SPECTROGRAM_HEIGHT - 1) as f32);
    let band_ranges: Vec<(usize, usize)> = frequencies
        .iter()
        .map(|frequency| {
            let low =
                ((frequency / half_step) * fft_size as f32 / sample_rate as f32).floor() as usize;
            let high =
                ((frequency * half_step) * fft_size as f32 / sample_rate as f32).ceil() as usize;
            let low = low.clamp(1, fft_size / 2 - 1);
            let high = high.clamp(low + 1, fft_size / 2);
            (low, high)
        })
        .collect();
    let mut result = vec![-120.0; SPECTROGRAM_WIDTH * SPECTROGRAM_HEIGHT];
    let mut magnitudes = vec![0.0_f32; fft_size / 2];

    for column in 0..SPECTROGRAM_WIDTH {
        let center = column * mono.len().saturating_sub(1) / SPECTROGRAM_WIDTH.saturating_sub(1);
        let start = center as isize - fft_size as isize / 2;
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
            *magnitude = point.norm() / fft_size as f32;
        }
        for (band, (low, high)) in band_ranges.iter().copied().enumerate() {
            let magnitude = magnitudes[low..high]
                .iter()
                .copied()
                .fold(0.0_f32, f32::max);
            result[column * SPECTROGRAM_HEIGHT + band] = 20.0 * magnitude.max(1.0e-8).log10();
        }
    }
    result
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
    if values.len() != SPECTROGRAM_WIDTH * SPECTROGRAM_HEIGHT {
        bail!(
            "spectrogram field has {} values; expected {}×{}",
            values.len(),
            SPECTROGRAM_WIDTH,
            SPECTROGRAM_HEIGHT
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
    let mut pixels = vec![0_u8; SPECTROGRAM_WIDTH * SPECTROGRAM_HEIGHT * 3];
    for row in 0..SPECTROGRAM_HEIGHT {
        let band = SPECTROGRAM_HEIGHT - row - 1;
        for column in 0..SPECTROGRAM_WIDTH {
            let db = values[column * SPECTROGRAM_HEIGHT + band];
            let intensity = ((db - floor) / db_range).clamp(0.0, 1.0);
            let color = spectral_color(intensity);
            let offset = (row * SPECTROGRAM_WIDTH + column) * 3;
            pixels[offset..offset + 3].copy_from_slice(&color);
        }
    }

    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(
            BufWriter::new(&mut bytes),
            SPECTROGRAM_WIDTH as u32,
            SPECTROGRAM_HEIGHT as u32,
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
}
