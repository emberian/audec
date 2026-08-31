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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpectrumSettings {
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
}

impl Default for SpectrumSettings {
    fn default() -> Self {
        Self {
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
        self
    }

    pub fn display_floor(self) -> f32 {
        self.db_ceiling - self.db_range
    }
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

    #[test]
    fn window_endpoints_are_well_behaved() {
        assert_eq!(WindowFunction::Rectangular.coefficient(0, 8), 1.0);
        assert!(WindowFunction::Hann.coefficient(0, 8).abs() < 1.0e-6);
        assert!(WindowFunction::Blackman.coefficient(0, 8).abs() < 1.0e-5);
    }
}
