//! Deterministic, multiresolution constant-Q analysis for pitch-oriented views.
//!
//! The transform uses one logarithmically spaced bin per requested pitch step.
//! Bins with similar analysis-window lengths share an FFT, while every constant-Q
//! kernel is sparsified in the frequency domain.  A [`ConstantQ`] value therefore
//! owns all expensive planning and kernel construction and can be reused for many
//! mono signals.
//!
//! Output is frame-major. Frame `t` is centred on input sample `t * hop_size`, and
//! bin `k` is centred on
//! `minimum_frequency_hz * 2^(k / bins_per_octave)`. Samples outside the input are
//! zero padded. This explicit convention makes overlays and phase measurements
//! independent of any caller-side padding.

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

const MAX_FFT_SIZE: usize = 1 << 20;
const MAX_BIN_COUNT: usize = 65_536;
const SPARSE_RELATIVE_AMPLITUDE: f32 = 1.0e-5;

/// Taper applied to each variable-length constant-Q kernel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CqtWindow {
    /// Symmetric Hann taper. This is the best general-purpose display window.
    #[default]
    Hann,
    /// Four-term Blackman-Harris taper for strong sidelobe rejection.
    BlackmanHarris,
    /// No taper. This has the narrowest main lobe but substantial leakage.
    Rectangular,
}

impl CqtWindow {
    fn coefficient(self, offset: isize, radius: usize) -> f32 {
        if radius == 0 {
            return 1.0;
        }
        let angle = std::f32::consts::PI * offset as f32 / radius as f32;
        match self {
            Self::Hann => 0.5 * (1.0 + angle.cos()),
            Self::BlackmanHarris => {
                0.358_75
                    + 0.488_29 * angle.cos()
                    + 0.141_28 * (2.0 * angle).cos()
                    + 0.011_68 * (3.0 * angle).cos()
            }
            Self::Rectangular => 1.0,
        }
    }
}

/// Sampling, pitch-grid, and framing parameters for constant-Q analysis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CqtSettings {
    /// Number of equal log-frequency steps in an octave.
    pub bins_per_octave: usize,
    /// Centre frequency of bin zero.
    pub minimum_frequency_hz: f32,
    /// Greatest permitted bin centre. The final centre never exceeds this value.
    pub maximum_frequency_hz: f32,
    /// PCM sampling rate.
    pub sample_rate: u32,
    /// Distance between adjacent frame centres, in samples.
    pub hop_size: usize,
    /// Kernel taper.
    pub window: CqtWindow,
}

impl Default for CqtSettings {
    fn default() -> Self {
        Self {
            bins_per_octave: 24,
            minimum_frequency_hz: 27.5,
            maximum_frequency_hz: 8_000.0,
            sample_rate: 48_000,
            hop_size: 256,
            window: CqtWindow::Hann,
        }
    }
}

impl CqtSettings {
    /// Validate settings without allocating transform kernels.
    pub fn validate(self) -> Result<Self, CqtError> {
        if self.bins_per_octave == 0 || self.bins_per_octave > 192 {
            return Err(CqtError::InvalidSettings(
                "bins_per_octave must be in 1..=192",
            ));
        }
        if self.sample_rate < 2 {
            return Err(CqtError::InvalidSettings(
                "sample_rate must be at least 2 Hz",
            ));
        }
        if self.hop_size == 0 {
            return Err(CqtError::InvalidSettings("hop_size must be positive"));
        }
        if !self.minimum_frequency_hz.is_finite() || self.minimum_frequency_hz <= 0.0 {
            return Err(CqtError::InvalidSettings(
                "minimum_frequency_hz must be finite and positive",
            ));
        }
        if !self.maximum_frequency_hz.is_finite()
            || self.maximum_frequency_hz < self.minimum_frequency_hz
        {
            return Err(CqtError::InvalidSettings(
                "maximum_frequency_hz must be finite and at least the minimum",
            ));
        }
        if self.maximum_frequency_hz >= self.sample_rate as f32 * 0.5 {
            return Err(CqtError::InvalidSettings(
                "maximum_frequency_hz must be below Nyquist",
            ));
        }
        Ok(self)
    }
}

/// Errors raised while constructing a transform plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CqtError {
    InvalidSettings(&'static str),
    /// The requested lowest bin needs a kernel beyond the implementation limit.
    WindowTooLong {
        required_fft_size: usize,
        maximum_fft_size: usize,
    },
    TooManyBins {
        count: usize,
        maximum: usize,
    },
}

impl fmt::Display for CqtError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSettings(message) => write!(formatter, "invalid CQT settings: {message}"),
            Self::WindowTooLong {
                required_fft_size,
                maximum_fft_size,
            } => write!(
                formatter,
                "CQT kernel requires FFT size {required_fft_size}, above limit {maximum_fft_size}"
            ),
            Self::TooManyBins { count, maximum } => {
                write!(
                    formatter,
                    "CQT requests {count} bins, above limit {maximum}"
                )
            }
        }
    }
}

impl Error for CqtError {}

#[derive(Clone, Debug)]
struct SparseCoefficient {
    index: usize,
    value: Complex<f32>,
}

#[derive(Clone, Debug)]
struct SparseKernel {
    output_bin: usize,
    coefficients: Vec<SparseCoefficient>,
}

#[derive(Clone)]
struct FftGroup {
    fft_size: usize,
    forward: Arc<dyn Fft<f32>>,
    kernels: Vec<SparseKernel>,
}

/// A reusable constant-Q transform plan.
///
/// Kernel lengths are `ceil(Q * sample_rate / frequency)`, rounded up to an odd
/// number, where `Q = 1 / (2^(1 / bins_per_octave) - 1)`. Each kernel is placed
/// in its smallest power-of-two FFT. This multiresolution grouping is the key
/// difference from interpolating a single fixed-resolution FFT.
#[derive(Clone)]
pub struct ConstantQ {
    settings: CqtSettings,
    quality_factor: f32,
    frequencies_hz: Vec<f32>,
    window_lengths: Vec<usize>,
    groups: Vec<FftGroup>,
}

impl fmt::Debug for ConstantQ {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConstantQ")
            .field("settings", &self.settings)
            .field("quality_factor", &self.quality_factor)
            .field("bin_count", &self.frequencies_hz.len())
            .field("fft_group_count", &self.groups.len())
            .finish()
    }
}

impl ConstantQ {
    /// Precompute all FFT plans and sparse kernels.
    pub fn new(settings: CqtSettings) -> Result<Self, CqtError> {
        let settings = settings.validate()?;
        let bins_per_octave = settings.bins_per_octave as f64;
        let quality_factor = 1.0 / (2.0_f64.powf(1.0 / bins_per_octave) - 1.0);
        let octave_span =
            (settings.maximum_frequency_hz as f64 / settings.minimum_frequency_hz as f64).log2();
        let bin_count = (octave_span * bins_per_octave + 1.0e-10).floor() as usize + 1;
        if bin_count > MAX_BIN_COUNT {
            return Err(CqtError::TooManyBins {
                count: bin_count,
                maximum: MAX_BIN_COUNT,
            });
        }

        let frequencies_hz: Vec<f32> = (0..bin_count)
            .map(|bin| {
                (settings.minimum_frequency_hz as f64 * 2.0_f64.powf(bin as f64 / bins_per_octave))
                    as f32
            })
            .collect();

        let mut specifications: BTreeMap<usize, Vec<(usize, usize, f32)>> = BTreeMap::new();
        let mut window_lengths = Vec::with_capacity(bin_count);
        for (bin, frequency) in frequencies_hz.iter().copied().enumerate() {
            let requested_length =
                (quality_factor * settings.sample_rate as f64 / frequency as f64).ceil() as usize;
            let window_length = requested_length.max(3) | 1;
            let fft_size =
                window_length
                    .checked_next_power_of_two()
                    .ok_or(CqtError::WindowTooLong {
                        required_fft_size: usize::MAX,
                        maximum_fft_size: MAX_FFT_SIZE,
                    })?;
            if fft_size > MAX_FFT_SIZE {
                return Err(CqtError::WindowTooLong {
                    required_fft_size: fft_size,
                    maximum_fft_size: MAX_FFT_SIZE,
                });
            }
            window_lengths.push(window_length);
            specifications
                .entry(fft_size)
                .or_default()
                .push((bin, window_length, frequency));
        }

        let mut planner = FftPlanner::<f32>::new();
        let mut groups = Vec::with_capacity(specifications.len());
        for (fft_size, specifications) in specifications {
            let forward = planner.plan_fft_forward(fft_size);
            let kernels = specifications
                .into_iter()
                .map(|(output_bin, window_length, frequency)| {
                    build_sparse_kernel(
                        output_bin,
                        window_length,
                        frequency,
                        settings.sample_rate,
                        settings.window,
                        fft_size,
                        &forward,
                    )
                })
                .collect();
            groups.push(FftGroup {
                fft_size,
                forward,
                kernels,
            });
        }

        Ok(Self {
            settings,
            quality_factor: quality_factor as f32,
            frequencies_hz,
            window_lengths,
            groups,
        })
    }

    pub fn settings(&self) -> CqtSettings {
        self.settings
    }

    pub fn quality_factor(&self) -> f32 {
        self.quality_factor
    }

    pub fn bin_count(&self) -> usize {
        self.frequencies_hz.len()
    }

    pub fn bin_frequency_hz(&self, bin: usize) -> Option<f32> {
        self.frequencies_hz.get(bin).copied()
    }

    pub fn frequencies_hz(&self) -> &[f32] {
        &self.frequencies_hz
    }

    /// Effective time support of a bin's kernel, in samples.
    pub fn kernel_window_length(&self, bin: usize) -> Option<usize> {
        self.window_lengths.get(bin).copied()
    }

    /// Analyze mono PCM into frame-major complex coefficients and magnitudes.
    ///
    /// Empty input has no frames. Any nonempty input has
    /// `ceil(input.len() / hop_size)` frames, including short inputs. Non-finite
    /// PCM values are treated as silence so they cannot contaminate a display.
    pub fn analyze(&self, input: &[f32]) -> CqtSpectrogram {
        let frame_count = if input.is_empty() {
            0
        } else {
            (input.len() - 1) / self.settings.hop_size + 1
        };
        let value_count = frame_count.saturating_mul(self.bin_count());
        let mut coefficients = vec![Complex::new(0.0, 0.0); value_count];

        for group in &self.groups {
            let mut spectrum = vec![Complex::new(0.0, 0.0); group.fft_size];
            let fft_center = group.fft_size / 2;
            for frame in 0..frame_count {
                let input_center = frame.saturating_mul(self.settings.hop_size);
                for (index, point) in spectrum.iter_mut().enumerate() {
                    let relative = index as isize - fft_center as isize;
                    let source = input_center as isize + relative;
                    let sample = if source >= 0 && (source as usize) < input.len() {
                        let value = input[source as usize];
                        if value.is_finite() {
                            value
                        } else {
                            0.0
                        }
                    } else {
                        0.0
                    };
                    *point = Complex::new(sample, 0.0);
                }
                group.forward.process(&mut spectrum);

                for kernel in &group.kernels {
                    let mut projection = Complex::new(0.0, 0.0);
                    for coefficient in &kernel.coefficients {
                        projection += spectrum[coefficient.index] * coefficient.value.conj();
                    }
                    // Parseval supplies 1/N. The factor two converts the positive
                    // complex component of a real sinusoid to its peak amplitude.
                    coefficients[frame * self.bin_count() + kernel.output_bin] =
                        projection * (2.0 / group.fft_size as f32);
                }
            }
        }

        let magnitudes = coefficients.iter().map(|value| value.norm()).collect();
        CqtSpectrogram {
            settings: self.settings,
            frame_count,
            bin_count: self.bin_count(),
            frequencies_hz: self.frequencies_hz.clone(),
            coefficients,
            magnitudes,
        }
    }
}

/// Frame-major constant-Q result. `index = frame * bin_count + bin`.
#[derive(Clone, Debug, PartialEq)]
pub struct CqtSpectrogram {
    pub settings: CqtSettings,
    pub frame_count: usize,
    pub bin_count: usize,
    pub frequencies_hz: Vec<f32>,
    /// Complex analytic amplitudes; their arguments are phases at frame centres.
    pub coefficients: Vec<Complex<f32>>,
    /// Peak-amplitude magnitudes matching `coefficients` exactly.
    pub magnitudes: Vec<f32>,
}

impl CqtSpectrogram {
    pub fn frame(&self, frame: usize) -> Option<&[f32]> {
        if frame >= self.frame_count {
            return None;
        }
        let start = frame * self.bin_count;
        Some(&self.magnitudes[start..start + self.bin_count])
    }

    pub fn complex_frame(&self, frame: usize) -> Option<&[Complex<f32>]> {
        if frame >= self.frame_count {
            return None;
        }
        let start = frame * self.bin_count;
        Some(&self.coefficients[start..start + self.bin_count])
    }

    pub fn magnitude(&self, frame: usize, bin: usize) -> Option<f32> {
        if frame >= self.frame_count || bin >= self.bin_count {
            return None;
        }
        Some(self.magnitudes[frame * self.bin_count + bin])
    }

    pub fn phase_radians(&self, frame: usize, bin: usize) -> Option<f32> {
        if frame >= self.frame_count || bin >= self.bin_count {
            return None;
        }
        Some(self.coefficients[frame * self.bin_count + bin].arg())
    }

    /// Exact centre sample used for this frame.
    pub fn frame_center_sample(&self, frame: usize) -> Option<usize> {
        (frame < self.frame_count).then(|| frame * self.settings.hop_size)
    }

    /// Exact centre time used for this frame.
    pub fn frame_time_seconds(&self, frame: usize) -> Option<f64> {
        self.frame_center_sample(frame)
            .map(|sample| sample as f64 / self.settings.sample_rate as f64)
    }

    pub fn bin_frequency_hz(&self, bin: usize) -> Option<f32> {
        self.frequencies_hz.get(bin).copied()
    }
}

fn build_sparse_kernel(
    output_bin: usize,
    window_length: usize,
    frequency: f32,
    sample_rate: u32,
    window: CqtWindow,
    fft_size: usize,
    forward: &Arc<dyn Fft<f32>>,
) -> SparseKernel {
    let fft_center = fft_size / 2;
    let radius = window_length / 2;
    let angular_frequency = std::f32::consts::TAU * frequency / sample_rate as f32;
    let mut kernel_spectrum = vec![Complex::new(0.0, 0.0); fft_size];
    let mut normalization = 0.0_f32;

    for offset in -(radius as isize)..=radius as isize {
        let taper = window.coefficient(offset, radius);
        normalization += taper;
        let phase = angular_frequency * offset as f32;
        kernel_spectrum[(fft_center as isize + offset) as usize] =
            Complex::from_polar(taper, phase);
    }
    let inverse_normalization = normalization.recip();
    for value in &mut kernel_spectrum {
        *value *= inverse_normalization;
    }
    forward.process(&mut kernel_spectrum);

    let peak = kernel_spectrum
        .iter()
        .map(|value| value.norm())
        .fold(0.0_f32, f32::max);
    let cutoff = peak * SPARSE_RELATIVE_AMPLITUDE;
    let mut coefficients: Vec<SparseCoefficient> = kernel_spectrum
        .into_iter()
        .enumerate()
        .filter(|(_, value)| value.norm() >= cutoff)
        .map(|(index, value)| SparseCoefficient { index, value })
        .collect();

    // Correct the very small complex gain error introduced by sparsification.
    // This makes an exact-bin complex sinusoid have unit response and preserves
    // the phase-at-frame-centre convention.
    let mut calibration = vec![Complex::new(0.0, 0.0); fft_size];
    for (index, value) in calibration.iter_mut().enumerate() {
        let offset = index as isize - fft_center as isize;
        *value = Complex::from_polar(1.0, angular_frequency * offset as f32);
    }
    forward.process(&mut calibration);
    let mut response = Complex::new(0.0, 0.0);
    for coefficient in &coefficients {
        response += calibration[coefficient.index] * coefficient.value.conj();
    }
    response /= fft_size as f32;
    if response.norm_sqr() > f32::EPSILON {
        let correction = Complex::new(1.0, 0.0) / response.conj();
        for coefficient in &mut coefficients {
            coefficient.value *= correction;
        }
    }

    SparseKernel {
        output_bin,
        coefficients,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> CqtSettings {
        CqtSettings {
            bins_per_octave: 12,
            minimum_frequency_hz: 64.0,
            maximum_frequency_hz: 2_048.0,
            sample_rate: 8_192,
            hop_size: 128,
            window: CqtWindow::Hann,
        }
    }

    fn sine(frequency: f32, length: usize, sample_rate: u32) -> Vec<f32> {
        (0..length)
            .map(|sample| {
                (std::f32::consts::TAU * frequency * sample as f32 / sample_rate as f32).sin()
            })
            .collect()
    }

    fn strongest_bin(frame: &[f32]) -> usize {
        frame
            .iter()
            .enumerate()
            .max_by(|left, right| left.1.total_cmp(right.1))
            .map(|(index, _)| index)
            .unwrap()
    }

    #[test]
    fn sine_localizes_to_its_bin_across_octaves() {
        let transform = ConstantQ::new(settings()).unwrap();
        for expected_bin in [6, 18, 30, 42, 54] {
            let frequency = transform.bin_frequency_hz(expected_bin).unwrap();
            let output = transform.analyze(&sine(frequency, 8_192, settings().sample_rate));
            let frame = output.frame(output.frame_count / 2).unwrap();
            let actual_bin = strongest_bin(frame);
            assert!(
                actual_bin.abs_diff(expected_bin) <= 1,
                "{frequency} Hz selected bin {actual_bin}, expected {expected_bin}"
            );
            assert!((frame[actual_bin] - 1.0).abs() < 0.04);
        }
    }

    #[test]
    fn logarithmic_chirp_moves_monotonically_upward() {
        let mut configured = settings();
        configured.hop_size = 256;
        let transform = ConstantQ::new(configured).unwrap();
        let length = configured.sample_rate as usize * 3;
        let start_hz = 128.0_f64;
        let end_hz = 1_024.0_f64;
        let duration = length as f64 / configured.sample_rate as f64;
        let rate = (end_hz / start_hz).ln() / duration;
        let chirp: Vec<f32> = (0..length)
            .map(|sample| {
                let time = sample as f64 / configured.sample_rate as f64;
                let phase = std::f64::consts::TAU * start_hz * (rate * time).exp_m1() / rate;
                phase.sin() as f32
            })
            .collect();
        let output = transform.analyze(&chirp);
        let track: Vec<usize> = (4..output.frame_count - 4)
            .map(|frame| strongest_bin(output.frame(frame).unwrap()))
            .collect();
        let backwards = track
            .windows(2)
            .filter(|pair| pair[1].saturating_add(1) < pair[0])
            .count();
        assert!(backwards <= 1, "unexpected descending track: {track:?}");
        assert!(track.last().unwrap() > &(track[0] + 24));
    }

    #[test]
    fn silence_empty_and_short_inputs_are_well_defined() {
        let transform = ConstantQ::new(settings()).unwrap();
        let empty = transform.analyze(&[]);
        assert_eq!(empty.frame_count, 0);
        assert!(empty.magnitudes.is_empty());

        let silent = transform.analyze(&[0.0; 17]);
        assert_eq!(silent.frame_count, 1);
        assert!(silent.magnitudes.iter().all(|value| *value == 0.0));
        assert_eq!(silent.frame_center_sample(0), Some(0));
        assert_eq!(silent.frame_time_seconds(0), Some(0.0));

        let impulse = transform.analyze(&[1.0]);
        assert_eq!(impulse.frame_count, 1);
        assert!(impulse.magnitudes.iter().all(|value| value.is_finite()));
    }

    #[test]
    fn repeated_analysis_is_bit_deterministic() {
        let transform = ConstantQ::new(settings()).unwrap();
        let input: Vec<f32> = (0..4_096)
            .map(|index| {
                let x = index as f32;
                (x * 0.071).sin() * 0.7 + (x * 0.193).cos() * 0.2
            })
            .collect();
        assert_eq!(transform.analyze(&input), transform.analyze(&input));
    }

    #[test]
    fn rejects_invalid_parameters_and_excessive_windows() {
        let mut invalid = settings();
        invalid.bins_per_octave = 0;
        assert!(matches!(
            ConstantQ::new(invalid),
            Err(CqtError::InvalidSettings(_))
        ));
        invalid = settings();
        invalid.hop_size = 0;
        assert!(ConstantQ::new(invalid).is_err());
        invalid = settings();
        invalid.minimum_frequency_hz = f32::NAN;
        assert!(ConstantQ::new(invalid).is_err());
        invalid = settings();
        invalid.maximum_frequency_hz = invalid.sample_rate as f32 * 0.5;
        assert!(ConstantQ::new(invalid).is_err());
        invalid = settings();
        invalid.bins_per_octave = 192;
        invalid.minimum_frequency_hz = 0.01;
        assert!(matches!(
            ConstantQ::new(invalid),
            Err(CqtError::WindowTooLong { .. })
        ));
    }

    #[test]
    fn logarithmic_bins_are_more_pitch_consistent_than_naive_fixed_fft_bins() {
        let transform = ConstantQ::new(settings()).unwrap();
        let frequencies = [90.0_f32, 180.0, 360.0, 720.0];
        let mut cqt_error = 0.0_f32;
        let mut fixed_error = 0.0_f32;
        let naive_fft_size = 256.0_f32;
        for frequency in frequencies {
            let output = transform.analyze(&sine(frequency, 8_192, settings().sample_rate));
            let selected = strongest_bin(output.frame(output.frame_count / 2).unwrap());
            let cqt_hz = output.bin_frequency_hz(selected).unwrap();
            cqt_error += 1_200.0 * (cqt_hz / frequency).log2().abs();

            let fixed_bin = (frequency * naive_fft_size / settings().sample_rate as f32).round();
            let fixed_hz = fixed_bin * settings().sample_rate as f32 / naive_fft_size;
            fixed_error += 1_200.0 * (fixed_hz / frequency).log2().abs();
        }
        assert!(
            cqt_error < fixed_error * 0.5,
            "CQT error {cqt_error} cents versus fixed-bin error {fixed_error} cents"
        );
    }

    #[test]
    fn frame_and_frequency_mappings_are_exact_and_bounded() {
        let transform = ConstantQ::new(settings()).unwrap();
        assert_eq!(transform.bin_count(), 61);
        assert_eq!(transform.bin_frequency_hz(0), Some(64.0));
        assert!((transform.bin_frequency_hz(12).unwrap() - 128.0).abs() < 1.0e-5);
        assert!(transform.frequencies_hz().last().unwrap() <= &settings().maximum_frequency_hz);
        assert!(
            transform.kernel_window_length(0).unwrap()
                > transform.kernel_window_length(24).unwrap()
        );

        let output = transform.analyze(&[0.0; 385]);
        assert_eq!(output.frame_count, 4);
        assert_eq!(output.frame_center_sample(3), Some(384));
        assert_eq!(
            output.frame_time_seconds(3),
            Some(384.0 / settings().sample_rate as f64)
        );
    }
}
