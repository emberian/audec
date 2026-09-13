//! Typed parameters shared by analysis transforms and GPUI lenses.
//!
//! The original audec exposed these mostly as process-wide command-line
//! switches.  Keeping them in explicit value types lets a lens say whether a
//! change is merely presentational, requires a cheap projection, invalidates
//! an analysis transform, or needs the audio engine to be rebuilt.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingEffect {
    Presentation,
    Projection,
    Analysis,
    AudioEngine,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowFunction {
    Rectangular,
    Hann,
    Blackman,
}

impl WindowFunction {
    pub const ALL: [Self; 3] = [Self::Rectangular, Self::Hann, Self::Blackman];

    pub fn label(self) -> &'static str {
        match self {
            Self::Rectangular => "Rect",
            Self::Hann => "Hann",
            Self::Blackman => "Blackman",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Rectangular => Self::Hann,
            Self::Hann => Self::Blackman,
            Self::Blackman => Self::Rectangular,
        }
    }

    pub fn coefficient(self, index: usize, size: usize) -> f32 {
        if size <= 1 {
            return 1.0;
        }
        let phase = std::f32::consts::TAU * index as f32 / (size - 1) as f32;
        match self {
            Self::Rectangular => 1.0,
            Self::Hann => 0.5 - 0.5 * phase.cos(),
            Self::Blackman => 0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos(),
        }
    }
}

/// The transform behind a spectral field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SpectralTransform {
    /// One fixed-size FFT per column; log-frequency bands take the peak bin.
    #[default]
    Fft,
    /// Multiresolution constant-Q: one analysis window per pitch step, so low
    /// notes resolve instead of smearing. How fine a pitch step is is
    /// [`SpectrumSettings::cqt_bins_per_octave`].
    ConstantQ,
}

/// Quarter-tones: the constant-Q resolution audec asks for when nobody has
/// said otherwise. It lives here once so the transform, the tile field, and
/// the lens header all read the same number instead of three literals.
pub const DEFAULT_CQT_BINS_PER_OCTAVE: u8 = 24;

/// The constant-Q resolutions the waterfall's FFT± steps through: semitones,
/// quarter-tones, sixth-tones. Coarser than a semitone stops being a pitch
/// grid; finer than a sixth-tone costs one more kernel per step for detail
/// the display bands cannot show.
pub const CQT_BINS_PER_OCTAVE_STEPS: [u8; 3] = [12, DEFAULT_CQT_BINS_PER_OCTAVE, 36];

/// The next coarser (`direction < 0`) or finer constant-Q resolution, held at
/// the ends rather than wrapping: a musician pressing FFT+ wants more detail,
/// not the coarsest grid again.
pub fn step_cqt_bins_per_octave(current: u8, direction: i32) -> u8 {
    let index = CQT_BINS_PER_OCTAVE_STEPS
        .iter()
        .position(|&value| value >= current)
        .unwrap_or(CQT_BINS_PER_OCTAVE_STEPS.len() - 1);
    let stepped = if direction < 0 {
        index.saturating_sub(1)
    } else {
        (index + 1).min(CQT_BINS_PER_OCTAVE_STEPS.len() - 1)
    };
    CQT_BINS_PER_OCTAVE_STEPS[stepped]
}

impl SpectralTransform {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fft => "FFT",
            Self::ConstantQ => "CQT",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Fft => Self::ConstantQ,
            Self::ConstantQ => Self::Fft,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpectrumSettings {
    /// Which time-frequency transform produces the field. Changing it
    /// re-runs analysis; it never only restyles the image.
    pub transform: SpectralTransform,
    pub fft_size: usize,
    pub hop_size: usize,
    pub window: WindowFunction,
    pub min_frequency_hz: f32,
    pub max_frequency_hz: f32,
    /// Top of the display transfer function. The legacy `spec-bias=-5`
    /// corresponds to a -5 dBFS ceiling.
    pub db_ceiling: f32,
    pub db_range: f32,
    pub waterfall_fraction: f32,
    /// Pitch steps per octave when `transform` is `ConstantQ`. Ignored by the
    /// FFT field, so one settings value serves both transforms.
    pub cqt_bins_per_octave: u8,
}

impl Default for SpectrumSettings {
    fn default() -> Self {
        Self {
            transform: SpectralTransform::Fft,
            // Preserve the old defaults in the settings model. Individual
            // overview transforms may deliberately request a larger FFT.
            fft_size: 1_024,
            hop_size: 256,
            window: WindowFunction::Hann,
            min_frequency_hz: 32.703,
            max_frequency_hz: 16_000.0,
            db_ceiling: -5.0,
            db_range: 30.0,
            waterfall_fraction: 0.8,
            cqt_bins_per_octave: DEFAULT_CQT_BINS_PER_OCTAVE,
        }
    }
}

impl SpectrumSettings {
    pub fn normalized(mut self, sample_rate: u32) -> Self {
        self.fft_size = self.fft_size.clamp(64, 131_072).next_power_of_two();
        self.hop_size = self.hop_size.clamp(1, self.fft_size);
        let nyquist = (sample_rate as f32 * 0.5).max(2.0);
        self.min_frequency_hz = self.min_frequency_hz.clamp(1.0, nyquist - 1.0);
        self.max_frequency_hz = self
            .max_frequency_hz
            .clamp(self.min_frequency_hz + 1.0, nyquist);
        self.db_ceiling = self.db_ceiling.clamp(-120.0, 24.0);
        self.db_range = self.db_range.clamp(6.0, 180.0);
        self.waterfall_fraction = self.waterfall_fraction.clamp(0.0, 1.0);
        // `cqt::CqtSettings` refuses 0 and anything past 192; a field that
        // asked for one of those would be a refusal, not a coarser picture.
        self.cqt_bins_per_octave = self.cqt_bins_per_octave.clamp(1, 192);
        self
    }

    pub fn display_floor(self) -> f32 {
        self.db_ceiling - self.db_range
    }
}

/// The recurring-component question a person asked, as a person asked it.
///
/// Only the two knobs a musician turns live here. The iteration budget, the
/// seed, the sparsity and the convergence tolerance are the kernel's business
/// and are not a preference: remembering them would let a stale file pin the
/// numerical behaviour of a future build.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComponentChoices {
    pub rank: usize,
    pub template_length: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScopeSettings {
    pub zero_crossing_search: usize,
    pub zero_crossing_position: f32,
    pub amplitude_power: f32,
}

impl Default for ScopeSettings {
    fn default() -> Self {
        Self {
            zero_crossing_search: 1_024,
            zero_crossing_position: 0.5,
            amplitude_power: 1.0,
        }
    }
}

impl ScopeSettings {
    pub fn normalized(mut self) -> Self {
        self.zero_crossing_search = self.zero_crossing_search.min(1 << 20);
        self.zero_crossing_position = self.zero_crossing_position.clamp(0.0, 1.0);
        self.amplitude_power = self.amplitude_power.clamp(0.05, 8.0);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VectorSettings {
    pub fade_rate: u8,
    pub brightness: u8,
}

impl Default for VectorSettings {
    fn default() -> Self {
        Self {
            fade_rate: 32,
            brightness: 32,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioSettings {
    pub device_name: Option<String>,
    pub requested_sample_rate: Option<u32>,
    pub period_frames: u32,
    pub gain: f32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            device_name: None,
            requested_sample_rate: None,
            period_frames: 256,
            gain: 1.0,
        }
    }
}

impl AudioSettings {
    pub fn normalized(mut self) -> Self {
        self.requested_sample_rate = self
            .requested_sample_rate
            .map(|rate| rate.clamp(8_000, 768_000));
        self.period_frames = self.period_frames.clamp(16, 65_536);
        self.gain = self.gain.clamp(0.0, 64.0);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LensViewport {
    pub time_start: f64,
    pub time_end: f64,
    /// Normalized coordinates in the active log-frequency projection.
    pub frequency_start: f32,
    pub frequency_end: f32,
}

impl Default for LensViewport {
    fn default() -> Self {
        Self {
            time_start: 0.0,
            time_end: 1.0,
            frequency_start: 0.0,
            frequency_end: 1.0,
        }
    }
}

impl LensViewport {
    pub fn normalized(mut self) -> Self {
        self.time_start = self.time_start.clamp(0.0, 1.0);
        self.time_end = self.time_end.clamp(self.time_start + 1.0e-6, 1.0);
        self.frequency_start = self.frequency_start.clamp(0.0, 1.0);
        self.frequency_end = self.frequency_end.clamp(self.frequency_start + 1.0e-6, 1.0);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_defaults_are_preserved() {
        let spectrum = SpectrumSettings::default();
        assert_eq!(spectrum.fft_size, 1_024);
        assert_eq!(spectrum.window, WindowFunction::Hann);
        assert_eq!(spectrum.db_ceiling, -5.0);
        assert_eq!(spectrum.db_range, 30.0);
        assert_eq!(spectrum.waterfall_fraction, 0.8);

        let scope = ScopeSettings::default();
        assert_eq!(scope.zero_crossing_search, 1_024);
        assert_eq!(scope.zero_crossing_position, 0.5);
        assert_eq!(scope.amplitude_power, 1.0);

        let vector = VectorSettings::default();
        assert_eq!(vector.fade_rate, 32);
        assert_eq!(vector.brightness, 32);
    }

    #[test]
    fn spectrum_normalization_respects_nyquist_and_power_of_two() {
        let settings = SpectrumSettings {
            fft_size: 1_001,
            hop_size: 4_096,
            min_frequency_hz: -2.0,
            max_frequency_hz: 100_000.0,
            db_range: 1.0,
            waterfall_fraction: 2.0,
            ..SpectrumSettings::default()
        }
        .normalized(48_000);
        assert_eq!(settings.fft_size, 1_024);
        assert_eq!(settings.hop_size, 1_024);
        assert_eq!(settings.min_frequency_hz, 1.0);
        assert_eq!(settings.max_frequency_hz, 24_000.0);
        assert_eq!(settings.db_range, 6.0);
        assert_eq!(settings.waterfall_fraction, 1.0);
    }

    /// FFT+ / FFT− under constant-Q step the pitch grid, and hold at the
    /// ends rather than wrapping round to the coarsest.
    #[test]
    fn constant_q_resolution_steps_between_the_three_pitch_grids() {
        assert_eq!(CQT_BINS_PER_OCTAVE_STEPS, [12, 24, 36]);
        assert_eq!(DEFAULT_CQT_BINS_PER_OCTAVE, 24);
        assert_eq!(SpectrumSettings::default().cqt_bins_per_octave, 24);

        assert_eq!(step_cqt_bins_per_octave(24, 1), 36);
        assert_eq!(step_cqt_bins_per_octave(24, -1), 12);
        assert_eq!(step_cqt_bins_per_octave(12, -1), 12, "coarsest holds");
        assert_eq!(step_cqt_bins_per_octave(36, 1), 36, "finest holds");
        assert_eq!(step_cqt_bins_per_octave(12, 1), 24);
        assert_eq!(step_cqt_bins_per_octave(36, -1), 24);

        // A value from another build lands on the nearest grid at or above it
        // and steps from there, rather than being refused or silently kept.
        assert_eq!(step_cqt_bins_per_octave(18, 1), 36);
        assert_eq!(step_cqt_bins_per_octave(18, -1), 12);
        assert_eq!(step_cqt_bins_per_octave(200, -1), 24);
    }

    /// `cqt::CqtSettings` refuses 0 and anything past 192, so a field never
    /// gets to ask for one.
    #[test]
    fn constant_q_resolution_is_normalized_into_what_the_transform_accepts() {
        let low = SpectrumSettings {
            cqt_bins_per_octave: 0,
            ..SpectrumSettings::default()
        }
        .normalized(48_000);
        assert_eq!(low.cqt_bins_per_octave, 1);
        let high = SpectrumSettings {
            cqt_bins_per_octave: 255,
            ..SpectrumSettings::default()
        }
        .normalized(48_000);
        assert_eq!(high.cqt_bins_per_octave, 192);
    }

    #[test]
    fn window_endpoints_are_well_behaved() {
        assert_eq!(WindowFunction::Rectangular.coefficient(0, 8), 1.0);
        assert!(WindowFunction::Hann.coefficient(0, 8).abs() < 1.0e-6);
        assert!(WindowFunction::Blackman.coefficient(0, 8).abs() < 1.0e-5);
    }
}
