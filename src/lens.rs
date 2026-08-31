//! Typed, validated settings for a *lens*: a particular way of inspecting
//! audio evidence.
//!
//! This deliberately knows nothing about GPUI, a decoder, an audio device, or
//! an inferred instrument.  A lens says how material should be framed and
//! projected; an analysis result says what was measured.  Keeping that line
//! sharp makes it safe to save, share, and reopen views without accidentally
//! turning a display preference into a claim about the source.
//!
//! ## Legacy correspondence
//!
//! * `Scope`: a rolling capture, 800 x 200 preferred size, a 1,024-sample
//!   zero-crossing search centred at 0.5, and amplitude power 1.0.
//! * `Spec`: a 1,024-point Hann FFT, 800 x 600 preferred size, -5 dBFS top,
//!   30 dB range, and a lower 80% scrolling waterfall.
//! * `Vector`: 400 x 400 preferred size, raw left/right axes, fade 32 and
//!   brightness 32.
//!
//! The SDL implementation did not expose a persistent timebase, trigger,
//! channel matrix, frequency endpoints, waterfall decay, or vectorscope
//! rotation/scale.  Those are explicit here, with defaults that reproduce the
//! old behaviour where it was observable.

use core::fmt;

/// Which rendering family a lens prefers.  This is a presentation request,
/// not a statement about what produced the sound.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum LensKind {
    Waveform,
    Scope,
    Spectrum,
    Waterfall,
    Vectorscope,
    Composite,
}

/// A preferred initial pixel size. The host may honour or ignore it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct PreferredSize {
    pub width: u32,
    pub height: u32,
}

impl PreferredSize {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    fn validate(self, path: &'static str, issues: &mut Vec<LensValidationIssue>) {
        if self.width == 0 || self.height == 0 {
            issues.push(LensValidationIssue::new(
                path,
                "width and height must be non-zero",
            ));
        }
    }
}

/// Units used by [`TimeView`]. Beat time is intentionally only a coordinate
/// system; it does not assert that any beat is present in the material.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Timebase {
    Samples,
    Seconds,
    Beats { bpm: f64, beats_per_bar: u8 },
}

impl Timebase {
    fn validate(self, issues: &mut Vec<LensValidationIssue>) {
        if let Self::Beats { bpm, beats_per_bar } = self {
            if !is_positive_f64(bpm) {
                issues.push(LensValidationIssue::new(
                    "time.base.bpm",
                    "must be finite and above zero",
                ));
            }
            if beats_per_bar == 0 {
                issues.push(LensValidationIssue::new(
                    "time.base.beats_per_bar",
                    "must be non-zero",
                ));
            }
        }
    }
}

/// The reference point from which a visible range is interpreted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TimeAnchor {
    /// A fixed position in the source/project coordinate system.
    Absolute,
    /// Keep the playhead at the requested relative position.
    Playhead,
    /// Use the current selection's start when a selection exists.
    SelectionStart,
    /// Use the capture trigger when a trigger fires.
    Trigger,
}

/// A non-negative start and positive duration expressed in [`Timebase`] units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimeRange {
    pub start: f64,
    pub duration: f64,
}

impl TimeRange {
    pub const fn new(start: f64, duration: f64) -> Self {
        Self { start, duration }
    }

    fn validate(self, issues: &mut Vec<LensValidationIssue>) {
        if !is_non_negative_f64(self.start) {
            issues.push(LensValidationIssue::new(
                "time.range.start",
                "must be finite and non-negative",
            ));
        }
        if !is_positive_f64(self.duration) {
            issues.push(LensValidationIssue::new(
                "time.range.duration",
                "must be finite and above zero",
            ));
        }
    }
}

/// Time framing shared by wave, spectral, and correlation lenses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimeView {
    pub base: Timebase,
    pub anchor: TimeAnchor,
    pub range: TimeRange,
    /// Position of the anchor within the visible range. 0 is left/top, 1 is
    /// right/bottom. This allows a scope to hold pre-trigger context.
    pub anchor_position: f32,
}

impl Default for TimeView {
    fn default() -> Self {
        Self {
            base: Timebase::Samples,
            anchor: TimeAnchor::Playhead,
            range: TimeRange::new(0.0, 1_024.0),
            anchor_position: 0.5,
        }
    }
}

impl TimeView {
    fn validate(self, issues: &mut Vec<LensValidationIssue>) {
        self.base.validate(issues);
        self.range.validate(issues);
        if !is_unit_f32(self.anchor_position) {
            issues.push(LensValidationIssue::new(
                "time.anchor_position",
                "must be finite and in 0..=1",
            ));
        }
    }
}

/// How new source frames are retained for a live or offline capture lens.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum CaptureMode {
    /// Keep the newest window of frames. This is the legacy live-view mode.
    Rolling,
    /// Replace the captured window whenever the trigger fires.
    Retrigger,
    /// Fill once, then hold until explicitly armed again.
    OneShot,
    /// A host supplies a fixed selected range instead of collecting frames.
    Selection,
}

/// The direction of a level crossing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TriggerEdge {
    Rising,
    Falling,
    Either,
}

/// A source-channel index.  Indexing is zero-based to match decoded PCM.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ChannelId(pub u16);

/// Trigger policy. `Disabled` preserves the old continuously updating views.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Trigger {
    Disabled,
    LevelCrossing {
        channel: ChannelId,
        level: f32,
        edge: TriggerEdge,
        /// Fraction of the capture window retained before the trigger point.
        pretrigger: f32,
    },
    /// A host-provided marker, MIDI event, or analysis event. The string is an
    /// opaque routing key, not a semantic label for a source object.
    External,
}

/// Capture policy, independent of whether a renderer chooses to animate it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CaptureSettings {
    pub mode: CaptureMode,
    pub trigger: Trigger,
    pub frozen: bool,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            mode: CaptureMode::Rolling,
            trigger: Trigger::Disabled,
            frozen: false,
        }
    }
}

impl CaptureSettings {
    fn validate(self, context: LensValidationContext, issues: &mut Vec<LensValidationIssue>) {
        if let Trigger::LevelCrossing {
            channel,
            level,
            pretrigger,
            ..
        } = self.trigger
        {
            validate_channel(channel, context, "capture.trigger.channel", issues);
            if !level.is_finite() {
                issues.push(LensValidationIssue::new(
                    "capture.trigger.level",
                    "must be finite",
                ));
            }
            if !is_unit_f32(pretrigger) {
                issues.push(LensValidationIssue::new(
                    "capture.trigger.pretrigger",
                    "must be finite and in 0..=1",
                ));
            }
        }
    }
}

/// Which source channels a lens projects. `Mid`, `Side`, and `MidSide` use the
/// conventional `(left + right) / 2` and `(left - right) / 2` projections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChannelProjection {
    /// Render the listed source channels separately, in listed order.
    Explicit(Vec<ChannelId>),
    /// Average all available source channels.
    MonoMix,
    /// Preserve an ordered stereo pair.
    StereoPair {
        left: ChannelId,
        right: ChannelId,
    },
    Mid {
        left: ChannelId,
        right: ChannelId,
    },
    Side {
        left: ChannelId,
        right: ChannelId,
    },
    MidSide {
        left: ChannelId,
        right: ChannelId,
    },
}

impl Default for ChannelProjection {
    fn default() -> Self {
        Self::StereoPair {
            left: ChannelId(0),
            right: ChannelId(1),
        }
    }
}

impl ChannelProjection {
    fn validate(&self, context: LensValidationContext, issues: &mut Vec<LensValidationIssue>) {
        match self {
            Self::Explicit(channels) => {
                if channels.is_empty() {
                    issues.push(LensValidationIssue::new(
                        "channels",
                        "explicit projection must name at least one channel",
                    ));
                }
                for (index, channel) in channels.iter().copied().enumerate() {
                    validate_channel(channel, context, "channels.explicit", issues);
                    if channels[..index].contains(&channel) {
                        issues.push(LensValidationIssue::new(
                            "channels.explicit",
                            "must not contain a channel twice",
                        ));
                        break;
                    }
                }
            }
            Self::MonoMix => {}
            Self::StereoPair { left, right }
            | Self::Mid { left, right }
            | Self::Side { left, right }
            | Self::MidSide { left, right } => {
                validate_pair(*left, *right, context, "channels", issues)
            }
        }
    }
}

/// Vertical sample mapping for waveform-like views. A scale of 1 and offset
/// 0 reproduces the old unscaled centre-line mapping.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AmplitudeView {
    pub scale: f32,
    pub offset: f32,
    /// Sign-preserving power. 1 is linear; this maps legacy `--sco-pow`.
    pub power: f32,
}

impl Default for AmplitudeView {
    fn default() -> Self {
        Self {
            scale: 1.0,
            offset: 0.0,
            power: 1.0,
        }
    }
}

impl AmplitudeView {
    fn validate(self, path: &'static str, issues: &mut Vec<LensValidationIssue>) {
        if !is_positive_f32(self.scale) {
            issues.push(LensValidationIssue::new(
                path,
                "scale must be finite and above zero",
            ));
        }
        if !self.offset.is_finite() {
            issues.push(LensValidationIssue::new(path, "offset must be finite"));
        }
        if !is_positive_f32(self.power) {
            issues.push(LensValidationIssue::new(
                path,
                "power must be finite and above zero",
            ));
        }
    }
}

/// Scope-only visual alignment. This is deliberately separate from [`Trigger`]:
/// a zero crossing may make a display steadier without controlling capture.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ZeroCrossingAlignment {
    Disabled,
    Search {
        search_frames: u32,
        horizontal_position: f32,
    },
}

/// Waveform and oscilloscope projection controls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaveformSettings {
    pub amplitude: AmplitudeView,
    pub zero_crossing: ZeroCrossingAlignment,
}

impl Default for WaveformSettings {
    fn default() -> Self {
        Self {
            amplitude: AmplitudeView::default(),
            zero_crossing: ZeroCrossingAlignment::Search {
                search_frames: 1_024,
                horizontal_position: 0.5,
            },
        }
    }
}

impl WaveformSettings {
    fn validate(self, issues: &mut Vec<LensValidationIssue>) {
        self.amplitude.validate("waveform.amplitude", issues);
        if let ZeroCrossingAlignment::Search {
            search_frames,
            horizontal_position,
        } = self.zero_crossing
        {
            if search_frames == 0 {
                issues.push(LensValidationIssue::new(
                    "waveform.zero_crossing.search_frames",
                    "must be non-zero when alignment is enabled",
                ));
            }
            if !is_unit_f32(horizontal_position) {
                issues.push(LensValidationIssue::new(
                    "waveform.zero_crossing.horizontal_position",
                    "must be finite and in 0..=1",
                ));
            }
        }
    }
}

/// Window shape used for an FFT analysis recipe.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SpectrumWindow {
    Rectangular,
    Hann,
    Blackman,
}

/// Frequency-axis mapping. The old `Spec` view used a logarithmic mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum FrequencyScale {
    Linear,
    Logarithmic,
}

/// FFT and display-transfer parameters for spectral evidence.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpectrumSettings {
    pub fft_size: u32,
    pub hop_size: u32,
    pub window: SpectrumWindow,
    pub frequency_scale: FrequencyScale,
    pub minimum_hz: f32,
    pub maximum_hz: f32,
    /// Top of the display transfer function, in dBFS.
    pub db_ceiling: f32,
    /// Dynamic range below `db_ceiling`, in dB.
    pub db_range: f32,
}

impl Default for SpectrumSettings {
    fn default() -> Self {
        Self {
            fft_size: 1_024,
            hop_size: 256,
            window: SpectrumWindow::Hann,
            frequency_scale: FrequencyScale::Logarithmic,
            minimum_hz: 32.703,
            maximum_hz: 16_000.0,
            db_ceiling: -5.0,
            db_range: 30.0,
        }
    }
}

impl SpectrumSettings {
    pub fn display_floor(self) -> f32 {
        self.db_ceiling - self.db_range
    }

    fn validate(self, context: LensValidationContext, issues: &mut Vec<LensValidationIssue>) {
        if self.fft_size < 2 || !self.fft_size.is_power_of_two() {
            issues.push(LensValidationIssue::new(
                "spectrum.fft_size",
                "must be a power of two of at least 2",
            ));
        }
        if self.hop_size == 0 || self.hop_size > self.fft_size {
            issues.push(LensValidationIssue::new(
                "spectrum.hop_size",
                "must be in 1..=fft_size",
            ));
        }
        if !is_positive_f32(self.minimum_hz) {
            issues.push(LensValidationIssue::new(
                "spectrum.minimum_hz",
                "must be finite and above zero",
            ));
        }
        if !is_positive_f32(self.maximum_hz) || self.maximum_hz <= self.minimum_hz {
            issues.push(LensValidationIssue::new(
                "spectrum.maximum_hz",
                "must be finite and greater than minimum_hz",
            ));
        }
        if let Some(sample_rate_hz) = context.sample_rate_hz {
            let nyquist = sample_rate_hz as f32 * 0.5;
            if self.maximum_hz > nyquist {
                issues.push(LensValidationIssue::new(
                    "spectrum.maximum_hz",
                    "must not exceed the source Nyquist frequency",
                ));
            }
        }
        if !self.db_ceiling.is_finite() {
            issues.push(LensValidationIssue::new(
                "spectrum.db_ceiling",
                "must be finite",
            ));
        }
        if !is_positive_f32(self.db_range) {
            issues.push(LensValidationIssue::new(
                "spectrum.db_range",
                "must be finite and above zero",
            ));
        }
    }
}

/// How a waterfall retains older spectral rows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WaterfallPersistence {
    /// Keep full-strength rows until display history evicts them. This is the
    /// legacy texture-scroll behaviour.
    UntilEvicted,
    /// Decay row intensity exponentially by the supplied half-life in frames.
    Exponential { half_life_frames: f32 },
}

/// Direction in which historical rows recede.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum WaterfallDirection {
    NewestAtBottom,
    NewestAtTop,
}

/// Spectral-history settings. Rows are a host/UI capacity, so this model gives
/// a policy rather than pretending a particular GPU texture height is audio.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaterfallSettings {
    /// Fraction of a composite spectral lens allocated to the waterfall.
    pub display_fraction: f32,
    pub persistence: WaterfallPersistence,
    pub direction: WaterfallDirection,
}

impl Default for WaterfallSettings {
    fn default() -> Self {
        Self {
            display_fraction: 0.8,
            persistence: WaterfallPersistence::UntilEvicted,
            direction: WaterfallDirection::NewestAtBottom,
        }
    }
}

impl WaterfallSettings {
    fn validate(self, issues: &mut Vec<LensValidationIssue>) {
        if !is_unit_f32(self.display_fraction) {
            issues.push(LensValidationIssue::new(
                "waterfall.display_fraction",
                "must be finite and in 0..=1",
            ));
        }
        if let WaterfallPersistence::Exponential { half_life_frames } = self.persistence {
            if !is_positive_f32(half_life_frames) {
                issues.push(LensValidationIssue::new(
                    "waterfall.persistence.half_life_frames",
                    "must be finite and above zero",
                ));
            }
        }
    }
}

/// A two-dimensional visual transform used by a vectorscope.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaneTransform {
    pub rotation_radians: f32,
    pub scale_x: f32,
    pub scale_y: f32,
    pub offset_x: f32,
    pub offset_y: f32,
}

impl Default for PlaneTransform {
    fn default() -> Self {
        Self {
            rotation_radians: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
        }
    }
}

impl PlaneTransform {
    fn validate(self, path: &'static str, issues: &mut Vec<LensValidationIssue>) {
        if !self.rotation_radians.is_finite() {
            issues.push(LensValidationIssue::new(path, "rotation must be finite"));
        }
        if !is_positive_f32(self.scale_x) || !is_positive_f32(self.scale_y) {
            issues.push(LensValidationIssue::new(
                path,
                "both scale axes must be finite and above zero",
            ));
        }
        if !self.offset_x.is_finite() || !self.offset_y.is_finite() {
            issues.push(LensValidationIssue::new(
                path,
                "both offsets must be finite",
            ));
        }
    }
}

/// Persistence and projection controls for stereo/vector displays.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VectorscopeSettings {
    /// Alpha used to clear the phosphor surface each render. 0 means no fade;
    /// 255 clears it immediately, exactly as the legacy `--vec-fade` option.
    pub fade_per_frame: u8,
    /// Additive trace intensity, exactly as legacy `--vec-brightness`.
    pub brightness: u8,
    pub transform: PlaneTransform,
}

impl Default for VectorscopeSettings {
    fn default() -> Self {
        Self {
            fade_per_frame: 32,
            brightness: 32,
            transform: PlaneTransform::default(),
        }
    }
}

impl VectorscopeSettings {
    fn validate(self, issues: &mut Vec<LensValidationIssue>) {
        self.transform.validate("vectorscope.transform", issues);
    }
}

/// The complete, serializable configuration of one lens.  All parameter groups
/// are retained even when `kind` does not currently draw them, making a
/// composite or a changed lens kind reversible without losing its recipe.
#[derive(Clone, Debug, PartialEq)]
pub struct LensParameters {
    pub kind: LensKind,
    pub preferred_size: Option<PreferredSize>,
    pub time: TimeView,
    pub capture: CaptureSettings,
    pub channels: ChannelProjection,
    pub waveform: WaveformSettings,
    pub spectrum: SpectrumSettings,
    pub waterfall: WaterfallSettings,
    pub vectorscope: VectorscopeSettings,
}

impl LensParameters {
    pub fn waveform() -> Self {
        Self {
            kind: LensKind::Waveform,
            preferred_size: Some(PreferredSize::new(800, 200)),
            time: TimeView {
                range: TimeRange::new(0.0, 800.0),
                ..TimeView::default()
            },
            ..Self::default()
        }
    }

    pub fn scope() -> Self {
        Self {
            kind: LensKind::Scope,
            preferred_size: Some(PreferredSize::new(800, 200)),
            time: TimeView {
                range: TimeRange::new(0.0, 800.0),
                ..TimeView::default()
            },
            ..Self::default()
        }
    }

    pub fn spectrum() -> Self {
        Self {
            kind: LensKind::Spectrum,
            preferred_size: Some(PreferredSize::new(800, 600)),
            ..Self::default()
        }
    }

    pub fn waterfall() -> Self {
        Self {
            kind: LensKind::Waterfall,
            preferred_size: Some(PreferredSize::new(800, 600)),
            ..Self::default()
        }
    }

    pub fn vectorscope() -> Self {
        Self {
            kind: LensKind::Vectorscope,
            preferred_size: Some(PreferredSize::new(400, 400)),
            ..Self::default()
        }
    }

    /// Return every invalid field rather than silently clamping a saved lens.
    /// `context` is optional metadata about the material; it tightens channel
    /// and Nyquist checks when known.
    pub fn validate(&self, context: LensValidationContext) -> Result<(), LensValidationErrors> {
        let mut issues = Vec::new();
        if let Some(size) = self.preferred_size {
            size.validate("preferred_size", &mut issues);
        }
        self.time.validate(&mut issues);
        self.capture.validate(context, &mut issues);
        self.channels.validate(context, &mut issues);
        self.waveform.validate(&mut issues);
        self.spectrum.validate(context, &mut issues);
        self.waterfall.validate(&mut issues);
        self.vectorscope.validate(&mut issues);
        if issues.is_empty() {
            Ok(())
        } else {
            Err(LensValidationErrors { issues })
        }
    }

    /// Whether changing this lens requires a spectral transform rather than a
    /// presentation-only redraw. Capture/transport state is intentionally not
    /// treated as analysis state.
    pub fn spectral_recipe_changed(&self, other: &Self) -> bool {
        self.spectrum != other.spectrum || self.channels != other.channels
    }
}

impl Default for LensParameters {
    fn default() -> Self {
        Self {
            kind: LensKind::Composite,
            preferred_size: None,
            time: TimeView::default(),
            capture: CaptureSettings::default(),
            channels: ChannelProjection::default(),
            waveform: WaveformSettings::default(),
            spectrum: SpectrumSettings::default(),
            waterfall: WaterfallSettings::default(),
            vectorscope: VectorscopeSettings::default(),
        }
    }
}

/// Optional facts about the material being viewed.  Absence means that the
/// lens is still valid as a saved recipe, while present facts add constraints.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LensValidationContext {
    pub sample_rate_hz: Option<u32>,
    pub channel_count: Option<u16>,
}

impl LensValidationContext {
    pub const UNKNOWN: Self = Self {
        sample_rate_hz: None,
        channel_count: None,
    };
}

/// One field-level validation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LensValidationIssue {
    pub path: &'static str,
    pub message: &'static str,
}

impl LensValidationIssue {
    const fn new(path: &'static str, message: &'static str) -> Self {
        Self { path, message }
    }
}

/// Validation failures collected without discarding the rest of a lens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LensValidationErrors {
    pub issues: Vec<LensValidationIssue>,
}

impl fmt::Display for LensValidationErrors {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid lens parameters")?;
        for issue in &self.issues {
            write!(formatter, "; {}: {}", issue.path, issue.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for LensValidationErrors {}

fn validate_channel(
    channel: ChannelId,
    context: LensValidationContext,
    path: &'static str,
    issues: &mut Vec<LensValidationIssue>,
) {
    if let Some(channel_count) = context.channel_count {
        if channel.0 >= channel_count {
            issues.push(LensValidationIssue::new(
                path,
                "references a channel outside the source",
            ));
        }
    }
}

fn validate_pair(
    left: ChannelId,
    right: ChannelId,
    context: LensValidationContext,
    path: &'static str,
    issues: &mut Vec<LensValidationIssue>,
) {
    validate_channel(left, context, path, issues);
    validate_channel(right, context, path, issues);
    if left == right {
        issues.push(LensValidationIssue::new(
            path,
            "a stereo or mid-side pair needs two distinct channels",
        ));
    }
}

fn is_positive_f32(value: f32) -> bool {
    value.is_finite() && value > 0.0
}

fn is_unit_f32(value: f32) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn is_positive_f64(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

fn is_non_negative_f64(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEREO_48K: LensValidationContext = LensValidationContext {
        sample_rate_hz: Some(48_000),
        channel_count: Some(2),
    };

    #[test]
    fn legacy_scope_recipe_is_preserved() {
        let lens = LensParameters::scope();
        assert_eq!(lens.preferred_size, Some(PreferredSize::new(800, 200)));
        assert_eq!(lens.capture, CaptureSettings::default());
        assert_eq!(lens.time.range, TimeRange::new(0.0, 800.0));
        assert_eq!(lens.waveform.amplitude, AmplitudeView::default());
        assert_eq!(
            lens.waveform.zero_crossing,
            ZeroCrossingAlignment::Search {
                search_frames: 1_024,
                horizontal_position: 0.5,
            }
        );
        assert!(lens.validate(STEREO_48K).is_ok());
    }

    #[test]
    fn legacy_spectral_and_waterfall_recipe_is_preserved() {
        let lens = LensParameters::waterfall();
        assert_eq!(lens.preferred_size, Some(PreferredSize::new(800, 600)));
        assert_eq!(lens.spectrum.fft_size, 1_024);
        assert_eq!(lens.spectrum.hop_size, 256);
        assert_eq!(lens.spectrum.window, SpectrumWindow::Hann);
        assert_eq!(lens.spectrum.frequency_scale, FrequencyScale::Logarithmic);
        assert_eq!(lens.spectrum.db_ceiling, -5.0);
        assert_eq!(lens.spectrum.db_range, 30.0);
        assert_eq!(lens.waterfall.display_fraction, 0.8);
        assert_eq!(
            lens.waterfall.persistence,
            WaterfallPersistence::UntilEvicted
        );
        assert_eq!(lens.waterfall.direction, WaterfallDirection::NewestAtBottom);
        assert_eq!(lens.spectrum.display_floor(), -35.0);
        assert!(lens.validate(STEREO_48K).is_ok());
    }

    #[test]
    fn legacy_vectorscope_recipe_is_preserved() {
        let lens = LensParameters::vectorscope();
        assert_eq!(lens.preferred_size, Some(PreferredSize::new(400, 400)));
        assert_eq!(lens.channels, ChannelProjection::default());
        assert_eq!(lens.vectorscope.fade_per_frame, 32);
        assert_eq!(lens.vectorscope.brightness, 32);
        assert_eq!(lens.vectorscope.transform, PlaneTransform::default());
        assert!(lens.validate(STEREO_48K).is_ok());
    }

    #[test]
    fn fft_recipe_is_strictly_validated() {
        let mut lens = LensParameters::spectrum();
        lens.spectrum.fft_size = 1_000;
        lens.spectrum.hop_size = 0;
        lens.spectrum.minimum_hz = 25_000.0;
        lens.spectrum.maximum_hz = 24_001.0;
        lens.spectrum.db_range = 0.0;
        let errors = lens.validate(STEREO_48K).unwrap_err();
        assert!(errors
            .issues
            .iter()
            .any(|issue| issue.path == "spectrum.fft_size"));
        assert!(errors
            .issues
            .iter()
            .any(|issue| issue.path == "spectrum.hop_size"));
        assert!(errors
            .issues
            .iter()
            .any(|issue| issue.path == "spectrum.maximum_hz"));
        assert!(errors
            .issues
            .iter()
            .any(|issue| issue.path == "spectrum.db_range"));
    }

    #[test]
    fn validation_uses_material_context_without_requiring_it() {
        let mut lens = LensParameters::vectorscope();
        lens.channels = ChannelProjection::MidSide {
            left: ChannelId(0),
            right: ChannelId(2),
        };
        assert!(lens.validate(LensValidationContext::UNKNOWN).is_ok());
        let errors = lens.validate(STEREO_48K).unwrap_err();
        assert!(errors.issues.iter().any(|issue| issue.path == "channels"));
    }

    #[test]
    fn pair_projection_rejects_a_channel_used_twice() {
        let mut lens = LensParameters::default();
        lens.channels = ChannelProjection::Mid {
            left: ChannelId(0),
            right: ChannelId(0),
        };
        let errors = lens.validate(STEREO_48K).unwrap_err();
        assert!(errors
            .issues
            .iter()
            .any(|issue| issue.message.contains("two distinct channels")));
    }

    #[test]
    fn explicit_projection_must_be_nonempty_and_unique() {
        let mut lens = LensParameters::default();
        lens.channels = ChannelProjection::Explicit(vec![]);
        assert!(lens.validate(STEREO_48K).is_err());
        lens.channels = ChannelProjection::Explicit(vec![ChannelId(0), ChannelId(0)]);
        let errors = lens.validate(STEREO_48K).unwrap_err();
        assert!(errors
            .issues
            .iter()
            .any(|issue| issue.message.contains("must not contain")));
    }

    #[test]
    fn trigger_and_freeze_are_independent_and_checked() {
        let mut lens = LensParameters::scope();
        lens.capture.mode = CaptureMode::OneShot;
        lens.capture.frozen = true;
        lens.capture.trigger = Trigger::LevelCrossing {
            channel: ChannelId(0),
            level: f32::NAN,
            edge: TriggerEdge::Rising,
            pretrigger: 1.2,
        };
        let errors = lens.validate(STEREO_48K).unwrap_err();
        assert!(errors
            .issues
            .iter()
            .any(|issue| issue.path == "capture.trigger.level"));
        assert!(errors
            .issues
            .iter()
            .any(|issue| issue.path == "capture.trigger.pretrigger"));
    }

    #[test]
    fn timebase_and_scope_mapping_reject_non_finite_values() {
        let mut lens = LensParameters::scope();
        lens.time.base = Timebase::Beats {
            bpm: 0.0,
            beats_per_bar: 0,
        };
        lens.time.range.duration = f64::NAN;
        lens.time.anchor_position = -0.1;
        lens.waveform.amplitude.scale = f32::INFINITY;
        lens.waveform.amplitude.offset = f32::NAN;
        lens.waveform.amplitude.power = 0.0;
        lens.waveform.zero_crossing = ZeroCrossingAlignment::Search {
            search_frames: 0,
            horizontal_position: f32::NAN,
        };
        let errors = lens.validate(STEREO_48K).unwrap_err();
        assert!(errors.issues.len() >= 8, "{errors}");
    }

    #[test]
    fn persistence_and_plane_transform_are_checked() {
        let mut lens = LensParameters::vectorscope();
        lens.waterfall.persistence = WaterfallPersistence::Exponential {
            half_life_frames: 0.0,
        };
        lens.vectorscope.transform.rotation_radians = f32::NAN;
        lens.vectorscope.transform.scale_x = -1.0;
        lens.vectorscope.transform.offset_y = f32::INFINITY;
        let errors = lens.validate(STEREO_48K).unwrap_err();
        assert!(errors
            .issues
            .iter()
            .any(|issue| issue.path == "waterfall.persistence.half_life_frames"));
        assert!(errors
            .issues
            .iter()
            .any(|issue| issue.path == "vectorscope.transform"));
    }

    #[test]
    fn analysis_invalidation_is_limited_to_analysis_recipe() {
        let reference = LensParameters::waterfall();
        let mut presentation_change = reference.clone();
        presentation_change.vectorscope.brightness = 200;
        presentation_change.time.anchor = TimeAnchor::SelectionStart;
        assert!(!reference.spectral_recipe_changed(&presentation_change));

        let mut recipe_change = reference.clone();
        recipe_change.spectrum.window = SpectrumWindow::Blackman;
        assert!(reference.spectral_recipe_changed(&recipe_change));
    }
}
