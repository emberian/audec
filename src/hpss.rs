//! Reconstructible short-time Fourier analysis and classical median-filter HPSS.
//!
//! This module deliberately has no sample-rate assumptions. Frequencies and times
//! can be attached by a caller using its own sample-rate context; the transform is
//! entirely described by [`HpssSettings`]. Spectra are stored frame-major and keep
//! the positive-frequency complex bins required to reconstruct real-valued audio.

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;
use std::error::Error;
use std::fmt;

const SILENCE_FLOOR: f32 = 1.0e-12;

/// Analysis and masking parameters for harmonic/percussive source separation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HpssSettings {
    /// Transform length in samples.
    pub fft_size: usize,
    /// Distance between adjacent frames in samples.
    pub hop_size: usize,
    /// Exponent used by the complementary Wiener-like soft masks.
    pub soft_mask_power: f32,
    /// Odd median-filter width across frames, used for the harmonic estimate.
    pub time_median_width: usize,
    /// Odd median-filter width across bins, used for the percussive estimate.
    pub frequency_median_width: usize,
}

impl Default for HpssSettings {
    fn default() -> Self {
        Self {
            fft_size: 2_048,
            hop_size: 512,
            soft_mask_power: 2.0,
            time_median_width: 17,
            frequency_median_width: 17,
        }
    }
}

impl HpssSettings {
    fn validate(self) -> Result<Self, HpssError> {
        if self.fft_size < 2 {
            return Err(HpssError::InvalidSettings("fft_size must be at least 2"));
        }
        if self.hop_size == 0 || self.hop_size > self.fft_size {
            return Err(HpssError::InvalidSettings(
                "hop_size must be in 1..=fft_size",
            ));
        }
        if !self.soft_mask_power.is_finite() || self.soft_mask_power <= 0.0 {
            return Err(HpssError::InvalidSettings(
                "soft_mask_power must be finite and positive",
            ));
        }
        if self.time_median_width == 0 || self.time_median_width % 2 == 0 {
            return Err(HpssError::InvalidSettings(
                "time_median_width must be positive and odd",
            ));
        }
        if self.frequency_median_width == 0 || self.frequency_median_width % 2 == 0 {
            return Err(HpssError::InvalidSettings(
                "frequency_median_width must be positive and odd",
            ));
        }
        Ok(self)
    }
}

/// Errors produced by transform or HPSS operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HpssError {
    InvalidSettings(&'static str),
    InvalidSpectrumLength { expected: usize, actual: usize },
    InvalidMaskLength { expected: usize, actual: usize },
}

impl fmt::Display for HpssError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSettings(message) => write!(formatter, "invalid HPSS settings: {message}"),
            Self::InvalidSpectrumLength { expected, actual } => write!(
                formatter,
                "invalid spectrum length: expected {expected} complex bins, got {actual}"
            ),
            Self::InvalidMaskLength { expected, actual } => write!(
                formatter,
                "invalid mask length: expected {expected} weights, got {actual}"
            ),
        }
    }
}

impl Error for HpssError {}

/// A contiguous, one-sided complex STFT for a real-valued mono signal.
///
/// `bins` is laid out as `frame * bin_count + bin`. The transform uses a
/// half-sample-shifted square-root Hann window. Padding puts the original signal
/// safely inside the overlap region, and inverse synthesis divides by the exact
/// accumulated window-square envelope, so reconstruction does not depend on a
/// constant-overlap-add hop ratio.
#[derive(Clone, Debug)]
pub struct ComplexStft {
    pub fft_size: usize,
    pub hop_size: usize,
    pub frame_count: usize,
    pub bin_count: usize,
    pub original_len: usize,
    pub pad_left: usize,
    pub bins: Vec<Complex<f32>>,
    window: Vec<f32>,
    overlap_normalization: Vec<f32>,
}

impl ComplexStft {
    /// Analyze mono PCM and retain the positive-frequency complex bins.
    pub fn analyze(input: &[f32], settings: HpssSettings) -> Result<Self, HpssError> {
        let settings = settings.validate()?;
        let fft_size = settings.fft_size;
        let hop_size = settings.hop_size;
        let bin_count = fft_size / 2 + 1;
        let window = sqrt_hann(fft_size);
        let overlap_normalization = periodic_overlap_normalization(&window, hop_size);

        if input.is_empty() {
            return Ok(Self {
                fft_size,
                hop_size,
                frame_count: 0,
                bin_count,
                original_len: 0,
                pad_left: fft_size,
                bins: Vec::new(),
                window,
                overlap_normalization,
            });
        }

        // A full transform of padding on each side keeps even very short signals
        // away from the window edge and gives every original sample coverage.
        let pad_left = fft_size;
        let required_len = pad_left
            .saturating_add(input.len())
            .saturating_add(fft_size);
        let frame_count = required_len
            .saturating_sub(fft_size)
            .div_ceil(hop_size)
            .saturating_add(1);
        let mut bins = vec![Complex::default(); frame_count.saturating_mul(bin_count)];

        let mut planner = FftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(fft_size);
        let mut frame = vec![Complex::default(); fft_size];
        let input_end = pad_left + input.len();

        for frame_index in 0..frame_count {
            let frame_start = frame_index * hop_size;
            for (offset, value) in frame.iter_mut().enumerate() {
                let padded_index = frame_start + offset;
                let sample = if (pad_left..input_end).contains(&padded_index) {
                    input[padded_index - pad_left]
                } else {
                    0.0
                };
                *value = Complex::new(sample * window[offset], 0.0);
            }

            forward.process(&mut frame);
            let destination = &mut bins[frame_index * bin_count..(frame_index + 1) * bin_count];
            destination.copy_from_slice(&frame[..bin_count]);
        }

        Ok(Self {
            fft_size,
            hop_size,
            frame_count,
            bin_count,
            original_len: input.len(),
            pad_left,
            bins,
            window,
            overlap_normalization,
        })
    }

    /// Return the complex bins for one frame.
    pub fn frame(&self, frame_index: usize) -> Option<&[Complex<f32>]> {
        if frame_index >= self.frame_count {
            return None;
        }
        let start = frame_index * self.bin_count;
        Some(&self.bins[start..start + self.bin_count])
    }

    /// Reconstruct PCM from a frame-major one-sided spectrum with this layout.
    pub fn synthesize(&self, bins: &[Complex<f32>]) -> Result<Vec<f32>, HpssError> {
        let expected = self.frame_count.saturating_mul(self.bin_count);
        if bins.len() != expected {
            return Err(HpssError::InvalidSpectrumLength {
                expected,
                actual: bins.len(),
            });
        }
        if self.original_len == 0 {
            return Ok(Vec::new());
        }

        Ok(self.synthesize_inner(bins, None))
    }

    /// Reconstruct PCM after multiplying every complex bin by a real mask.
    pub fn synthesize_masked(&self, mask: &[f32]) -> Result<Vec<f32>, HpssError> {
        if mask.len() != self.bins.len() {
            return Err(HpssError::InvalidMaskLength {
                expected: self.bins.len(),
                actual: mask.len(),
            });
        }
        if self.original_len == 0 {
            return Ok(Vec::new());
        }
        Ok(self.synthesize_inner(&self.bins, Some(mask)))
    }

    fn synthesize_inner(&self, bins: &[Complex<f32>], mask: Option<&[f32]>) -> Vec<f32> {
        let padded_len = (self.frame_count - 1) * self.hop_size + self.fft_size;
        let mut output = vec![0.0_f32; padded_len];
        let mut planner = FftPlanner::<f32>::new();
        let inverse = planner.plan_fft_inverse(self.fft_size);
        let mut frame = vec![Complex::default(); self.fft_size];
        let inverse_scale = 1.0 / self.fft_size as f32;

        for frame_index in 0..self.frame_count {
            let frame_range = frame_index * self.bin_count..(frame_index + 1) * self.bin_count;
            if let Some(mask) = mask {
                expand_real_spectrum_masked(
                    &bins[frame_range.clone()],
                    &mask[frame_range],
                    &mut frame,
                );
            } else {
                expand_real_spectrum(&bins[frame_range], &mut frame);
            }
            inverse.process(&mut frame);

            let frame_start = frame_index * self.hop_size;
            for offset in 0..self.fft_size {
                let destination = frame_start + offset;
                let window = self.window[offset];
                output[destination] += frame[offset].re * inverse_scale * window;
            }
        }

        let original_end = self.pad_left + self.original_len;
        let mut result = Vec::with_capacity(self.original_len);
        for index in self.pad_left..original_end {
            let denominator = self.overlap_normalization[index % self.hop_size];
            result.push(if denominator > SILENCE_FLOOR {
                output[index] / denominator
            } else {
                0.0
            });
        }
        result
    }
}

/// Complementary soft masks in the same frame-major layout as [`ComplexStft::bins`].
#[derive(Clone, Debug, Default)]
pub struct HpssMasks {
    pub harmonic: Vec<f32>,
    pub percussive: Vec<f32>,
}

/// Reconstruction and mask-separation quality measurements.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HpssDiagnostics {
    pub input_rms: f32,
    pub harmonic_rms: f32,
    pub percussive_rms: f32,
    /// RMS of `input - (harmonic + percussive)`.
    pub residual_rms: f32,
    pub relative_reconstruction_error: f32,
    pub max_abs_reconstruction_error: f32,
    /// Magnitude-weighted mean of `abs(harmonic_mask - percussive_mask)`.
    pub mask_confidence: f32,
    /// Complement of `mask_confidence`; one means maximally ambiguous.
    pub mask_ambiguity: f32,
}

/// Complete, inspectable output of [`separate_harmonic_percussive`].
#[derive(Clone, Debug)]
pub struct HpssResult {
    pub settings: HpssSettings,
    pub stft: ComplexStft,
    pub masks: HpssMasks,
    pub harmonic: Vec<f32>,
    pub percussive: Vec<f32>,
    /// The sample-domain null left by `input - (harmonic + percussive)`.
    pub residual: Vec<f32>,
    pub diagnostics: HpssDiagnostics,
}

/// Separate mono PCM with median-filter harmonic/percussive source separation.
///
/// The time-axis median favors bins that persist between frames; the frequency-
/// axis median favors broadband vertical events. Masks always sum to one, including
/// at silent bins (where each is 0.5), making the two outputs additive by design.
pub fn separate_harmonic_percussive(
    input: &[f32],
    settings: HpssSettings,
) -> Result<HpssResult, HpssError> {
    let settings = settings.validate()?;
    let stft = ComplexStft::analyze(input, settings)?;
    let cell_count = stft.bins.len();

    if cell_count == 0 {
        return Ok(HpssResult {
            settings,
            stft,
            masks: HpssMasks::default(),
            harmonic: Vec::new(),
            percussive: Vec::new(),
            residual: Vec::new(),
            diagnostics: HpssDiagnostics {
                mask_ambiguity: 1.0,
                ..HpssDiagnostics::default()
            },
        });
    }

    let magnitude: Vec<f32> = stft.bins.iter().map(|bin| bin.norm()).collect();
    let mut harmonic_estimate = vec![0.0_f32; cell_count];
    let mut percussive_estimate = vec![0.0_f32; cell_count];
    let scratch_capacity = settings
        .time_median_width
        .min(stft.frame_count)
        .max(settings.frequency_median_width.min(stft.bin_count));
    let mut scratch = Vec::with_capacity(scratch_capacity);

    median_filter_time(
        &magnitude,
        &mut harmonic_estimate,
        stft.frame_count,
        stft.bin_count,
        settings.time_median_width,
        &mut scratch,
    );
    median_filter_frequency(
        &magnitude,
        &mut percussive_estimate,
        stft.frame_count,
        stft.bin_count,
        settings.frequency_median_width,
        &mut scratch,
    );

    let mut harmonic_mask = Vec::with_capacity(cell_count);
    let mut percussive_mask = Vec::with_capacity(cell_count);
    let mut weighted_confidence = 0.0_f64;
    let mut total_weight = 0.0_f64;

    for ((harmonic, percussive), magnitude) in harmonic_estimate
        .iter()
        .zip(&percussive_estimate)
        .zip(&magnitude)
    {
        let scale = harmonic.max(*percussive);
        let harmonic_weight = if scale <= SILENCE_FLOOR {
            0.5
        } else {
            let harmonic_power = mask_power(harmonic / scale, settings.soft_mask_power);
            let percussive_power = mask_power(percussive / scale, settings.soft_mask_power);
            harmonic_power / (harmonic_power + percussive_power).max(SILENCE_FLOOR)
        };
        let percussive_weight = 1.0 - harmonic_weight;
        harmonic_mask.push(harmonic_weight);
        percussive_mask.push(percussive_weight);

        let weight = f64::from(*magnitude);
        weighted_confidence += weight * f64::from((harmonic_weight - percussive_weight).abs());
        total_weight += weight;
    }

    let harmonic = stft.synthesize_masked(&harmonic_mask)?;
    let percussive = stft.synthesize_masked(&percussive_mask)?;
    let residual: Vec<f32> = input
        .iter()
        .zip(harmonic.iter().zip(&percussive))
        .map(|(source, (harmonic, percussive))| source - harmonic - percussive)
        .collect();
    let input_rms = rms(input);
    let residual_rms = rms(&residual);
    let mask_confidence = if total_weight > f64::EPSILON {
        (weighted_confidence / total_weight) as f32
    } else {
        0.0
    }
    .clamp(0.0, 1.0);
    let max_abs_reconstruction_error = residual
        .iter()
        .fold(0.0_f32, |maximum, sample| maximum.max(sample.abs()));

    let diagnostics = HpssDiagnostics {
        input_rms,
        harmonic_rms: rms(&harmonic),
        percussive_rms: rms(&percussive),
        residual_rms,
        relative_reconstruction_error: residual_rms / input_rms.max(SILENCE_FLOOR),
        max_abs_reconstruction_error,
        mask_confidence,
        mask_ambiguity: 1.0 - mask_confidence,
    };

    Ok(HpssResult {
        settings,
        stft,
        masks: HpssMasks {
            harmonic: harmonic_mask,
            percussive: percussive_mask,
        },
        harmonic,
        percussive,
        residual,
        diagnostics,
    })
}

#[inline]
fn mask_power(value: f32, power: f32) -> f32 {
    // Power two is the standard Wiener-like setting and avoiding libm `powf`
    // twice per time-frequency cell is a substantial whole-track speedup.
    if power == 2.0 {
        value * value
    } else if power == 1.0 {
        value
    } else {
        value.powf(power)
    }
}

fn sqrt_hann(size: usize) -> Vec<f32> {
    (0..size)
        .map(|index| {
            (std::f32::consts::PI * (index as f32 + 0.5) / size as f32)
                .sin()
                .max(0.0)
        })
        .collect()
}

fn periodic_overlap_normalization(window: &[f32], hop_size: usize) -> Vec<f32> {
    let mut normalization = vec![0.0_f32; hop_size];
    for (offset, window) in window.iter().enumerate() {
        normalization[offset % hop_size] += window * window;
    }
    normalization
}

fn expand_real_spectrum(one_sided: &[Complex<f32>], full: &mut [Complex<f32>]) {
    full.fill(Complex::default());
    full[..one_sided.len()].copy_from_slice(one_sided);
    let fft_size = full.len();
    let last_mirrored_bin = (fft_size - 1) / 2;
    for bin in 1..=last_mirrored_bin {
        full[fft_size - bin] = one_sided[bin].conj();
    }
}

fn expand_real_spectrum_masked(
    one_sided: &[Complex<f32>],
    mask: &[f32],
    full: &mut [Complex<f32>],
) {
    full.fill(Complex::default());
    for (destination, (bin, weight)) in full
        .iter_mut()
        .zip(one_sided.iter().zip(mask))
        .take(one_sided.len())
    {
        *destination = *bin * *weight;
    }
    let fft_size = full.len();
    let last_mirrored_bin = (fft_size - 1) / 2;
    for bin in 1..=last_mirrored_bin {
        full[fft_size - bin] = full[bin].conj();
    }
}

fn median_filter_time(
    source: &[f32],
    destination: &mut [f32],
    frame_count: usize,
    bin_count: usize,
    width: usize,
    scratch: &mut Vec<f32>,
) {
    if frame_count == 0 || bin_count == 0 {
        return;
    }
    let radius = width / 2;
    for bin in 0..bin_count {
        for frame in 0..frame_count {
            if frame == 0 {
                scratch.clear();
                let last = radius.min(frame_count - 1);
                for neighbor in 0..=last {
                    sorted_insert(scratch, source[neighbor * bin_count + bin]);
                }
            } else {
                let previous_first = (frame - 1).saturating_sub(radius);
                let first = frame.saturating_sub(radius);
                if first > previous_first {
                    sorted_remove(scratch, source[previous_first * bin_count + bin]);
                }

                let previous_last = (frame - 1).saturating_add(radius).min(frame_count - 1);
                let last = frame.saturating_add(radius).min(frame_count - 1);
                if last > previous_last {
                    sorted_insert(scratch, source[last * bin_count + bin]);
                }
            }
            destination[frame * bin_count + bin] = scratch[scratch.len() / 2];
        }
    }
}

fn median_filter_frequency(
    source: &[f32],
    destination: &mut [f32],
    frame_count: usize,
    bin_count: usize,
    width: usize,
    scratch: &mut Vec<f32>,
) {
    if frame_count == 0 || bin_count == 0 {
        return;
    }
    let radius = width / 2;
    for frame in 0..frame_count {
        let frame_offset = frame * bin_count;
        for bin in 0..bin_count {
            if bin == 0 {
                scratch.clear();
                let last = radius.min(bin_count - 1);
                for neighbor in 0..=last {
                    sorted_insert(scratch, source[frame_offset + neighbor]);
                }
            } else {
                let previous_first = (bin - 1).saturating_sub(radius);
                let first = bin.saturating_sub(radius);
                if first > previous_first {
                    sorted_remove(scratch, source[frame_offset + previous_first]);
                }

                let previous_last = (bin - 1).saturating_add(radius).min(bin_count - 1);
                let last = bin.saturating_add(radius).min(bin_count - 1);
                if last > previous_last {
                    sorted_insert(scratch, source[frame_offset + last]);
                }
            }
            destination[frame_offset + bin] = scratch[scratch.len() / 2];
        }
    }
}

/// Insert into a tiny sorted median window. `Vec::insert` moves at most the filter
/// width (normally 17) and never allocates because the shared scratch vector is
/// reserved once. This is substantially cheaper than rebuilding and selecting a
/// fresh neighborhood at every time-frequency cell.
fn sorted_insert(values: &mut Vec<f32>, value: f32) {
    let position = values.partition_point(|candidate| candidate.total_cmp(&value).is_le());
    values.insert(position, value);
}

fn sorted_remove(values: &mut Vec<f32>, value: f32) {
    let position = values
        .binary_search_by(|candidate| candidate.total_cmp(&value))
        .expect("sliding median removes only values present in its active window");
    values.remove(position);
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_settings() -> HpssSettings {
        HpssSettings {
            fft_size: 512,
            hop_size: 128,
            soft_mask_power: 2.0,
            time_median_width: 17,
            frequency_median_width: 17,
        }
    }

    fn sine(length: usize, frequency: f32, sample_rate: f32) -> Vec<f32> {
        (0..length)
            .map(|sample| {
                (2.0 * std::f32::consts::PI * frequency * sample as f32 / sample_rate).sin()
            })
            .collect()
    }

    fn error_rms(left: &[f32], right: &[f32]) -> f32 {
        let difference: Vec<f32> = left
            .iter()
            .zip(right)
            .map(|(left, right)| left - right)
            .collect();
        rms(&difference)
    }

    #[test]
    fn stft_roundtrip_reconstructs_even_and_odd_transforms() {
        for fft_size in [511, 512] {
            let settings = HpssSettings {
                fft_size,
                hop_size: 127,
                ..test_settings()
            };
            let input: Vec<f32> = (0..4_321)
                .map(|index| {
                    0.6 * (index as f32 * 0.031).sin()
                        + 0.2 * (index as f32 * 0.117).cos()
                        + 0.05 * (((index * 73) % 101) as f32 / 50.0 - 1.0)
                })
                .collect();
            let stft = ComplexStft::analyze(&input, settings).unwrap();
            let output = stft.synthesize(&stft.bins).unwrap();
            assert_eq!(input.len(), output.len());
            assert!(
                error_rms(&input, &output) < 2.0e-6,
                "fft_size={fft_size} error={}",
                error_rms(&input, &output)
            );
        }
    }

    #[test]
    fn stable_sine_and_impulse_train_prefer_expected_components() {
        let length = 16_384;
        let harmonic_input = sine(length, 440.0, 8_192.0);
        let harmonic = separate_harmonic_percussive(&harmonic_input, test_settings()).unwrap();
        assert!(
            harmonic.diagnostics.harmonic_rms > harmonic.diagnostics.percussive_rms * 3.0,
            "sine harmonic={} percussive={}",
            harmonic.diagnostics.harmonic_rms,
            harmonic.diagnostics.percussive_rms
        );

        let mut impulse_input = vec![0.0_f32; length];
        for index in (256..length).step_by(1_024) {
            impulse_input[index] = 1.0;
            impulse_input[index + 1] = -0.5;
        }
        let impulses = separate_harmonic_percussive(&impulse_input, test_settings()).unwrap();
        assert!(
            impulses.diagnostics.percussive_rms > impulses.diagnostics.harmonic_rms * 3.0,
            "impulses harmonic={} percussive={}",
            impulses.diagnostics.harmonic_rms,
            impulses.diagnostics.percussive_rms
        );
    }

    #[test]
    fn mixture_is_additive_and_masks_are_complementary() {
        let mut input = sine(12_345, 233.0, 8_192.0);
        for index in (400..input.len()).step_by(733) {
            input[index] += 0.8;
        }
        let result = separate_harmonic_percussive(&input, test_settings()).unwrap();
        assert!(result
            .masks
            .harmonic
            .iter()
            .zip(&result.masks.percussive)
            .all(|(harmonic, percussive)| (harmonic + percussive - 1.0).abs() < 1.0e-7));
        assert!(result.diagnostics.residual_rms < 2.0e-6);
        assert!(result.diagnostics.relative_reconstruction_error < 5.0e-6);
        assert!(result.diagnostics.mask_confidence >= 0.0);
        assert!(result.diagnostics.mask_confidence <= 1.0);
        assert!(
            (result.diagnostics.mask_confidence + result.diagnostics.mask_ambiguity - 1.0).abs()
                < 1.0e-6
        );
    }

    #[test]
    fn silence_empty_and_short_inputs_are_well_defined() {
        let empty = separate_harmonic_percussive(&[], test_settings()).unwrap();
        assert!(empty.harmonic.is_empty());
        assert_eq!(empty.diagnostics.mask_confidence, 0.0);
        assert_eq!(empty.diagnostics.mask_ambiguity, 1.0);

        for input in [vec![0.0; 13], vec![0.0; 1_000]] {
            let result = separate_harmonic_percussive(&input, test_settings()).unwrap();
            assert_eq!(result.harmonic.len(), input.len());
            assert_eq!(result.percussive.len(), input.len());
            assert!(result.harmonic.iter().all(|sample| *sample == 0.0));
            assert!(result.percussive.iter().all(|sample| *sample == 0.0));
            assert_eq!(result.diagnostics.mask_confidence, 0.0);
            assert_eq!(result.diagnostics.mask_ambiguity, 1.0);
        }

        let short = vec![0.25, -0.5, 0.75];
        let stft = ComplexStft::analyze(&short, test_settings()).unwrap();
        let reconstructed = stft.synthesize(&stft.bins).unwrap();
        assert!(error_rms(&short, &reconstructed) < 2.0e-6);
    }

    /// Manual performance smoke test:
    /// `cargo test hpss::tests::benchmark_eighteen_seconds -- --ignored --nocapture`
    #[test]
    #[ignore = "manual timing guard"]
    fn benchmark_eighteen_seconds() {
        let sample_rate = 44_100.0;
        let input: Vec<f32> = (0..(sample_rate as usize * 18))
            .map(|sample| {
                let time = sample as f32 / sample_rate;
                let tone = 0.35 * (std::f32::consts::TAU * 220.0 * time).sin()
                    + 0.2 * (std::f32::consts::TAU * 329.63 * time).sin();
                let impulse = if sample % 11_025 < 32 {
                    0.7 * (1.0 - (sample % 11_025) as f32 / 32.0)
                } else {
                    0.0
                };
                tone + impulse
            })
            .collect();
        let started = std::time::Instant::now();
        let result = separate_harmonic_percussive(&input, HpssSettings::default()).unwrap();
        let elapsed = started.elapsed();
        assert!(result.diagnostics.relative_reconstruction_error < 1.0e-5);
        eprintln!(
            "18 s / 44.1 kHz / 2048 FFT / 512 hop: {:.3} s",
            elapsed.as_secs_f64()
        );
    }
}
