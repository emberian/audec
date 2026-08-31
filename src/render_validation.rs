//! Portable regression checks for offline and realtime renderers.
//!
//! This module intentionally depends only on `std`, so it can be compiled on
//! its own (`rustc --test src/render_validation.rs`) while a new render graph is
//! still being prototyped.  Its fingerprints are made from quantized *signal
//! features*, never platform-specific IEEE byte representations.  They are
//! therefore useful as reviewable golden data without making harmless last-bit
//! differences into test failures.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Describes interleaved PCM presented to the validation helpers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignalFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl SignalFormat {
    pub fn new(sample_rate: u32, channels: u16) -> Result<Self, ValidationError> {
        if sample_rate == 0 || channels == 0 {
            return Err(ValidationError::InvalidFormat {
                sample_rate,
                channels,
            });
        }
        Ok(Self {
            sample_rate,
            channels,
        })
    }
}

/// Borrowed, interleaved PCM with an explicit frame-domain origin.
#[derive(Clone, Copy, Debug)]
pub struct Signal<'a> {
    pub format: SignalFormat,
    pub start_frame: i64,
    pub interleaved: &'a [f32],
}

impl<'a> Signal<'a> {
    pub fn new(
        format: SignalFormat,
        start_frame: i64,
        interleaved: &'a [f32],
    ) -> Result<Self, ValidationError> {
        if interleaved.len() % usize::from(format.channels) != 0 {
            return Err(ValidationError::PartialFrame {
                samples: interleaved.len(),
                channels: format.channels,
            });
        }
        Ok(Self {
            format,
            start_frame,
            interleaved,
        })
    }

    pub fn frames(self) -> usize {
        self.interleaved.len() / usize::from(self.format.channels)
    }

    pub fn end_frame(self) -> i64 {
        self.start_frame.saturating_add(self.frames() as i64)
    }

    pub fn frame(self, index: usize) -> Option<&'a [f32]> {
        let channels = usize::from(self.format.channels);
        self.interleaved
            .get(index.checked_mul(channels)?..index.checked_add(1)?.checked_mul(channels)?)
    }
}

/// Per-channel measurements, expressed in normalised linear PCM units.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelMetrics {
    pub peak: f64,
    pub rms: f64,
    pub dc_offset: f64,
    pub non_finite_samples: usize,
}

/// Descriptive signal measurements used by UI diagnostics and tests.
#[derive(Clone, Debug, PartialEq)]
pub struct SignalMetrics {
    pub frames: usize,
    pub first_active_frame: Option<i64>,
    pub last_active_frame: Option<i64>,
    pub peak: f64,
    pub rms: f64,
    pub dc_offset: f64,
    pub crest_factor: f64,
    pub non_finite_samples: usize,
    pub channels: Vec<ChannelMetrics>,
}

/// Measure a signal.  Non-finite samples are counted and treated as silence so
/// an invalid renderer cannot hide behind a NaN-derived comparison result.
pub fn measure(signal: Signal<'_>, activity_epsilon: f32) -> SignalMetrics {
    let epsilon = activity_epsilon.abs() as f64;
    let channels = usize::from(signal.format.channels);
    let mut sums = vec![0.0_f64; channels];
    let mut squares = vec![0.0_f64; channels];
    let mut peaks = vec![0.0_f64; channels];
    let mut invalid = vec![0_usize; channels];
    let mut first = None;
    let mut last = None;

    for (frame_index, frame) in signal.interleaved.chunks_exact(channels).enumerate() {
        let mut active = false;
        for (channel, &sample) in frame.iter().enumerate() {
            let value = f64::from(sample);
            if !value.is_finite() {
                invalid[channel] += 1;
                continue;
            }
            let magnitude = value.abs();
            active |= magnitude > epsilon;
            sums[channel] += value;
            squares[channel] += value * value;
            peaks[channel] = peaks[channel].max(magnitude);
        }
        if active {
            let frame = signal.start_frame.saturating_add(frame_index as i64);
            first.get_or_insert(frame);
            last = Some(frame);
        }
    }

    let frames = signal.frames().max(1) as f64;
    let channel_metrics: Vec<_> = (0..channels)
        .map(|channel| ChannelMetrics {
            peak: peaks[channel],
            rms: (squares[channel] / frames).sqrt(),
            dc_offset: sums[channel] / frames,
            non_finite_samples: invalid[channel],
        })
        .collect();
    let peak = channel_metrics.iter().map(|m| m.peak).fold(0.0, f64::max);
    let rms = if channels == 0 {
        0.0
    } else {
        (channel_metrics.iter().map(|m| m.rms * m.rms).sum::<f64>() / channels as f64).sqrt()
    };
    let dc_offset = if channels == 0 {
        0.0
    } else {
        channel_metrics.iter().map(|m| m.dc_offset).sum::<f64>() / channels as f64
    };
    SignalMetrics {
        frames: signal.frames(),
        first_active_frame: first,
        last_active_frame: last,
        peak,
        rms,
        dc_offset,
        crest_factor: if rms > 0.0 { peak / rms } else { 0.0 },
        non_finite_samples: invalid.iter().sum(),
        channels: channel_metrics,
    }
}

/// Comparison policy. Absolute error protects silence; relative error protects
/// proportional changes at useful signal levels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ComparisonTolerance {
    pub absolute: f64,
    pub relative: f64,
    pub minimum_snr_db: f64,
    pub allow_non_finite: bool,
}

impl Default for ComparisonTolerance {
    fn default() -> Self {
        Self {
            absolute: 1.0e-5,
            relative: 1.0e-4,
            minimum_snr_db: 80.0,
            allow_non_finite: false,
        }
    }
}

/// Error measurements between two same-format signals.
#[derive(Clone, Debug, PartialEq)]
pub struct SignalComparison {
    pub compared_samples: usize,
    pub max_absolute_error: f64,
    pub rms_error: f64,
    pub reference_rms: f64,
    pub snr_db: f64,
    pub first_failing_frame: Option<i64>,
    pub non_finite_samples: usize,
}

pub fn compare(
    reference: Signal<'_>,
    candidate: Signal<'_>,
    tolerance: ComparisonTolerance,
) -> Result<SignalComparison, ValidationError> {
    validate_compatible(reference, candidate)?;
    let mut max_error = 0.0_f64;
    let mut error_energy = 0.0_f64;
    let mut reference_energy = 0.0_f64;
    let mut first_failing = None;
    let mut invalid = 0;
    for (index, (&expected, &actual)) in reference
        .interleaved
        .iter()
        .zip(candidate.interleaved.iter())
        .enumerate()
    {
        let expected = f64::from(expected);
        let actual = f64::from(actual);
        if !expected.is_finite() || !actual.is_finite() {
            invalid += 1;
            if !tolerance.allow_non_finite {
                first_failing.get_or_insert(
                    reference.start_frame + (index / usize::from(reference.format.channels)) as i64,
                );
            }
            continue;
        }
        let error = (expected - actual).abs();
        max_error = max_error.max(error);
        error_energy += error * error;
        reference_energy += expected * expected;
        let permitted = tolerance.absolute.max(tolerance.relative * expected.abs());
        if error > permitted {
            first_failing.get_or_insert(
                reference.start_frame + (index / usize::from(reference.format.channels)) as i64,
            );
        }
    }
    let count = reference.interleaved.len().max(1) as f64;
    let rms_error = (error_energy / count).sqrt();
    let reference_rms = (reference_energy / count).sqrt();
    let snr_db = if rms_error == 0.0 {
        f64::INFINITY
    } else if reference_rms == 0.0 {
        f64::NEG_INFINITY
    } else {
        20.0 * (reference_rms / rms_error).log10()
    };
    Ok(SignalComparison {
        compared_samples: reference.interleaved.len(),
        max_absolute_error: max_error,
        rms_error,
        reference_rms,
        snr_db,
        first_failing_frame: first_failing,
        non_finite_samples: invalid,
    })
}

pub fn assert_matches(
    reference: Signal<'_>,
    candidate: Signal<'_>,
    tolerance: ComparisonTolerance,
) -> Result<SignalComparison, ValidationError> {
    let comparison = compare(reference, candidate, tolerance)?;
    if comparison.non_finite_samples > 0 && !tolerance.allow_non_finite {
        return Err(ValidationError::NonFinite {
            count: comparison.non_finite_samples,
        });
    }
    if comparison.first_failing_frame.is_some() || comparison.snr_db < tolerance.minimum_snr_db {
        return Err(ValidationError::SignalMismatch { comparison });
    }
    Ok(comparison)
}

/// A small, reviewable fingerprint. Values are quantized after measurement;
/// `feature_hash` hashes the quantized per-block energies, not float bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoldenFingerprint {
    pub version: u16,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    pub first_active_offset: Option<i64>,
    pub last_active_offset: Option<i64>,
    pub peak_millionths: i64,
    pub rms_millionths: i64,
    pub dc_millionths: i64,
    pub block_energy_hash: u64,
}

impl GoldenFingerprint {
    pub const VERSION: u16 = 1;

    pub fn from_signal(signal: Signal<'_>, activity_epsilon: f32) -> Self {
        let metrics = measure(signal, activity_epsilon);
        let mut hash = 0xcbf29ce484222325_u64;
        let channels = usize::from(signal.format.channels);
        // 1024 frames is long enough to ignore harmless dither-like variation,
        // while retaining phrase/envelope identity.
        for block in signal.interleaved.chunks(1024 * channels) {
            let energy = block
                .iter()
                .filter_map(|sample| {
                    let sample = f64::from(*sample);
                    sample.is_finite().then_some(sample * sample)
                })
                .sum::<f64>();
            fnv1a(&mut hash, quantize(energy.sqrt(), 10_000));
        }
        Self {
            version: Self::VERSION,
            sample_rate: signal.format.sample_rate,
            channels: signal.format.channels,
            frames: signal.frames() as u64,
            first_active_offset: metrics
                .first_active_frame
                .map(|frame| frame - signal.start_frame),
            last_active_offset: metrics
                .last_active_frame
                .map(|frame| frame - signal.start_frame),
            peak_millionths: quantize(metrics.peak, 1_000_000),
            rms_millionths: quantize(metrics.rms, 1_000_000),
            dc_millionths: quantize(metrics.dc_offset, 1_000_000),
            block_energy_hash: hash,
        }
    }

    /// Compares dimensions exactly and scalar features within `scalar_slop`.
    /// The feature hash is deliberately advisory: it catches an unexpected
    /// structural change without replacing audible sample-level comparison.
    pub fn compare(&self, actual: &Self, scalar_slop: i64) -> FingerprintComparison {
        let scalar_slop = scalar_slop.abs();
        FingerprintComparison {
            metadata_matches: self.version == actual.version
                && self.sample_rate == actual.sample_rate
                && self.channels == actual.channels
                && self.frames == actual.frames,
            active_window_matches: self.first_active_offset == actual.first_active_offset
                && self.last_active_offset == actual.last_active_offset,
            peak_delta: actual.peak_millionths - self.peak_millionths,
            rms_delta: actual.rms_millionths - self.rms_millionths,
            dc_delta: actual.dc_millionths - self.dc_millionths,
            block_energy_hash_matches: self.block_energy_hash == actual.block_energy_hash,
            scalar_slop,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FingerprintComparison {
    pub metadata_matches: bool,
    pub active_window_matches: bool,
    pub peak_delta: i64,
    pub rms_delta: i64,
    pub dc_delta: i64,
    pub block_energy_hash_matches: bool,
    pub scalar_slop: i64,
}

impl FingerprintComparison {
    pub fn scalar_matches(&self) -> bool {
        self.peak_delta.abs() <= self.scalar_slop
            && self.rms_delta.abs() <= self.scalar_slop
            && self.dc_delta.abs() <= self.scalar_slop
    }

    pub fn is_compatible(&self) -> bool {
        self.metadata_matches && self.active_window_matches && self.scalar_matches()
    }
}

pub fn assert_fingerprint(
    golden: &GoldenFingerprint,
    actual: &GoldenFingerprint,
    scalar_slop: i64,
) -> Result<FingerprintComparison, ValidationError> {
    let comparison = golden.compare(actual, scalar_slop);
    if comparison.is_compatible() {
        Ok(comparison)
    } else {
        Err(ValidationError::FingerprintMismatch { comparison })
    }
}

/// Timing report derived from an impulse or other isolated transient.
#[derive(Clone, Debug, PartialEq)]
pub struct ImpulseReport {
    pub expected_frame: i64,
    pub observed_frame: Option<i64>,
    pub latency_frames: Option<i64>,
    pub peak: f64,
    pub competing_peaks: usize,
}

/// Finds the first strongest threshold-crossing frame. `threshold` is applied
/// to the max magnitude over output channels, so it works for mono and stereo.
pub fn inspect_impulse(signal: Signal<'_>, expected_frame: i64, threshold: f32) -> ImpulseReport {
    let threshold = threshold.abs() as f64;
    let channels = usize::from(signal.format.channels);
    let mut observed = None;
    let mut peak = 0.0_f64;
    let mut peaks: usize = 0;
    for (index, frame) in signal.interleaved.chunks_exact(channels).enumerate() {
        let magnitude = frame
            .iter()
            .filter_map(|sample| {
                let value = f64::from(*sample).abs();
                value.is_finite().then_some(value)
            })
            .fold(0.0, f64::max);
        peak = peak.max(magnitude);
        if magnitude >= threshold {
            peaks += 1;
            observed.get_or_insert(signal.start_frame + index as i64);
        }
    }
    ImpulseReport {
        expected_frame,
        observed_frame: observed,
        latency_frames: observed.map(|observed| observed - expected_frame),
        peak,
        competing_peaks: peaks.saturating_sub(usize::from(observed.is_some())),
    }
}

pub fn assert_impulse_latency(
    signal: Signal<'_>,
    expected_frame: i64,
    threshold: f32,
    allowed_latency_frames: i64,
) -> Result<ImpulseReport, ValidationError> {
    let report = inspect_impulse(signal, expected_frame, threshold);
    let latency = report.observed_frame.map(|frame| frame - expected_frame);
    if latency.map(|value| value.abs() <= allowed_latency_frames.abs()) == Some(true) {
        Ok(report)
    } else {
        Err(ValidationError::ImpulseTiming { report })
    }
}

/// Checks the half-open silence invariant that catches off-by-one scheduling:
/// `[signal.start_frame, until_frame)` must remain below `epsilon`.
pub fn assert_silent_until(
    signal: Signal<'_>,
    until_frame: i64,
    epsilon: f32,
) -> Result<(), ValidationError> {
    let channels = usize::from(signal.format.channels);
    let end = until_frame.clamp(signal.start_frame, signal.end_frame());
    for (index, frame) in signal.interleaved.chunks_exact(channels).enumerate() {
        let frame_number = signal.start_frame + index as i64;
        if frame_number >= end {
            break;
        }
        if frame
            .iter()
            .any(|sample| !sample.is_finite() || sample.abs() > epsilon.abs())
        {
            return Err(ValidationError::ExpectedSilence {
                frame: frame_number,
                epsilon: epsilon.abs(),
            });
        }
    }
    Ok(())
}

/// A cancellation flag useful for testing render loops without coupling this
/// module to any particular executor's cancellation type.
#[derive(Clone, Debug, Default)]
pub struct ValidationCancellation(Arc<AtomicBool>);

impl ValidationCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub fn check(&self) -> Result<(), ValidationError> {
        if self.is_cancelled() {
            Err(ValidationError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Verifies that a renderer refuses to begin when cancellation is already set.
/// The closure receives the test flag; a correct renderer checks it before it
/// allocates buffers or touches source assets.
pub fn assert_pre_cancelled<T>(
    render: impl FnOnce(&ValidationCancellation) -> Result<T, ValidationError>,
) -> Result<(), ValidationError> {
    let cancellation = ValidationCancellation::new();
    cancellation.cancel();
    match render(&cancellation) {
        Err(ValidationError::Cancelled) => Ok(()),
        Err(other) => Err(ValidationError::WrongCancellationError {
            actual: other.to_string(),
        }),
        Ok(_) => Err(ValidationError::CancellationIgnored),
    }
}

/// Runs two independently produced renders through identical validation. This
/// is especially valuable for asserting callback-block-size independence.
pub fn assert_deterministic(
    first: Signal<'_>,
    second: Signal<'_>,
    tolerance: ComparisonTolerance,
) -> Result<SignalComparison, ValidationError> {
    assert_matches(first, second, tolerance)
}

fn validate_compatible(
    reference: Signal<'_>,
    candidate: Signal<'_>,
) -> Result<(), ValidationError> {
    if reference.format != candidate.format
        || reference.start_frame != candidate.start_frame
        || reference.frames() != candidate.frames()
    {
        return Err(ValidationError::IncompatibleSignals {
            expected_rate: reference.format.sample_rate,
            expected_channels: reference.format.channels,
            expected_start: reference.start_frame,
            expected_frames: reference.frames(),
            actual_rate: candidate.format.sample_rate,
            actual_channels: candidate.format.channels,
            actual_start: candidate.start_frame,
            actual_frames: candidate.frames(),
        });
    }
    Ok(())
}

fn quantize(value: f64, scale: i64) -> i64 {
    if !value.is_finite() {
        return i64::MAX;
    }
    (value * scale as f64)
        .round()
        .clamp(i64::MIN as f64, i64::MAX as f64) as i64
}

fn fnv1a(hash: &mut u64, value: i64) {
    for byte in value.to_le_bytes() {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(0x100000001b3);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ValidationError {
    InvalidFormat {
        sample_rate: u32,
        channels: u16,
    },
    PartialFrame {
        samples: usize,
        channels: u16,
    },
    IncompatibleSignals {
        expected_rate: u32,
        expected_channels: u16,
        expected_start: i64,
        expected_frames: usize,
        actual_rate: u32,
        actual_channels: u16,
        actual_start: i64,
        actual_frames: usize,
    },
    NonFinite {
        count: usize,
    },
    SignalMismatch {
        comparison: SignalComparison,
    },
    FingerprintMismatch {
        comparison: FingerprintComparison,
    },
    ImpulseTiming {
        report: ImpulseReport,
    },
    ExpectedSilence {
        frame: i64,
        epsilon: f32,
    },
    Cancelled,
    CancellationIgnored,
    WrongCancellationError {
        actual: String,
    },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFormat {
                sample_rate,
                channels,
            } => {
                write!(
                    f,
                    "invalid signal format: {sample_rate} Hz / {channels} channels"
                )
            }
            Self::PartialFrame { samples, channels } => {
                write!(
                    f,
                    "{samples} samples is not a whole {channels}-channel frame count"
                )
            }
            Self::IncompatibleSignals { .. } => {
                write!(f, "signals have incompatible timeline or format")
            }
            Self::NonFinite { count } => write!(f, "render contained {count} non-finite samples"),
            Self::SignalMismatch { comparison } => write!(
                f,
                "signal mismatch: peak error {:.6}, RMS error {:.6}, SNR {:.2} dB",
                comparison.max_absolute_error, comparison.rms_error, comparison.snr_db
            ),
            Self::FingerprintMismatch { comparison } => write!(
                f,
                "golden fingerprint mismatch (metadata={}, active={}, scalar={}, structural={})",
                comparison.metadata_matches,
                comparison.active_window_matches,
                comparison.scalar_matches(),
                comparison.block_energy_hash_matches
            ),
            Self::ImpulseTiming { report } => write!(
                f,
                "impulse timing mismatch: expected {}, observed {:?}",
                report.expected_frame, report.observed_frame
            ),
            Self::ExpectedSilence { frame, epsilon } => {
                write!(
                    f,
                    "expected silence through frame {frame} (epsilon {epsilon})"
                )
            }
            Self::Cancelled => write!(f, "render cancelled"),
            Self::CancellationIgnored => write!(f, "renderer ignored pre-cancellation"),
            Self::WrongCancellationError { actual } => {
                write!(f, "renderer returned wrong cancellation error: {actual}")
            }
        }
    }
}

impl std::error::Error for ValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono(samples: &[f32]) -> Signal<'_> {
        Signal::new(SignalFormat::new(48_000, 1).unwrap(), 1_000, samples).unwrap()
    }

    #[test]
    fn fingerprint_is_stable_for_tiny_float_noise_but_spots_phrase_change() {
        let original = mono(&[0.0, 0.25, -0.5, 0.25, 0.0, 0.0]);
        let nearly_identical = mono(&[0.0, 0.25000001, -0.49999999, 0.25, 0.0, 0.0]);
        let changed = mono(&[0.0, 0.25, -0.5, 0.0, 0.0, 0.0]);
        let golden = GoldenFingerprint::from_signal(original, 1.0e-4);
        assert!(assert_fingerprint(
            &golden,
            &GoldenFingerprint::from_signal(nearly_identical, 1.0e-4),
            1
        )
        .is_ok());
        assert!(
            !golden
                .compare(&GoldenFingerprint::from_signal(changed, 1.0e-4), 1)
                .block_energy_hash_matches
        );
    }

    #[test]
    fn comparison_reports_a_precise_first_bad_frame() {
        let expected = mono(&[0.0, 0.0, 0.5, 0.0]);
        let actual = mono(&[0.0, 0.0, 0.49, 0.0]);
        let error = assert_matches(expected, actual, ComparisonTolerance::default()).unwrap_err();
        match error {
            ValidationError::SignalMismatch { comparison } => {
                assert_eq!(comparison.first_failing_frame, Some(1_002));
            }
            _ => panic!("unexpected validation error"),
        }
    }

    #[test]
    fn impulse_and_half_open_silence_catch_latency_off_by_one() {
        let signal = mono(&[0.0, 0.0, 0.0, 0.8, 0.0]);
        assert_silent_until(signal, 1_003, 1.0e-6).unwrap();
        let impulse = assert_impulse_latency(signal, 1_003, 0.5, 0).unwrap();
        assert_eq!(impulse.latency_frames, Some(0));
        assert!(assert_impulse_latency(signal, 1_002, 0.5, 0).is_err());
    }

    #[test]
    fn cancellation_harness_requires_an_early_check() {
        assert!(assert_pre_cancelled(|cancel| cancel.check()).is_ok());
        assert!(matches!(
            assert_pre_cancelled(|_| Ok::<_, ValidationError>(())),
            Err(ValidationError::CancellationIgnored)
        ));
    }

    #[test]
    fn non_finite_output_never_compares_as_valid_audio() {
        let good = mono(&[0.0, 0.25]);
        let bad = mono(&[0.0, f32::NAN]);
        assert!(matches!(
            assert_matches(good, bad, ComparisonTolerance::default()),
            Err(ValidationError::NonFinite { count: 1 })
        ));
    }
}
