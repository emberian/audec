//! User preferences that survive a relaunch.
//!
//! Preferences are presentation and analysis choices that belong to the
//! person, not to a project: which spectral transform a lens uses, its FFT
//! size, window, and display range. They are stored as one small JSON file
//! under the platform config directory and read on demand. A missing or
//! unreadable file yields defaults and a diagnostic; it never blocks the app.
//!
//! The on-disk form is a DTO at this codec boundary; domain settings types
//! stay serde-free.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{self, AtomicU64};

use serde::{Deserialize, Serialize};

use crate::hpss::{HpssSettings, SPAN_LADDER_SECONDS};
use crate::loom::LoomLensSettings;
use crate::rhythm::RhythmLensSettings;
use crate::settings::{ComponentChoices, SpectralTransform, SpectrumSettings, WindowFunction};

/// Preferences in domain terms.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Preferences {
    /// Lens spectrum choices to apply when a lens is created. The dB ceiling
    /// is deliberately not remembered: it follows each material's peak.
    pub spectrum: Option<SpectrumSettings>,
    /// The recurring-component question last asked by hand. `None` means
    /// nobody has asked one, not "the default was chosen".
    pub components: Option<ComponentChoices>,
    /// The rhythm lens's onset-detector knobs, written only by a press on
    /// one of them. `None` means this person has never tuned the detector,
    /// which is not the same fact as "they chose the defaults".
    pub rhythm: Option<RhythmLensSettings>,
    /// The Loom lens's lookbehind and template length, on the same terms.
    pub loom: Option<LoomLensSettings>,
    /// Separation kernel widths and span, remembered only when a musician
    /// chose them. The transform's FFT size and hop are deliberately not
    /// remembered: they are not knobs, and a stored copy of a default is a
    /// default that can never move.
    pub separation: Option<SeparationChoices>,
}

/// What the Separation lens remembers between sessions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeparationChoices {
    pub time_median_width: usize,
    pub frequency_median_width: usize,
    pub span_seconds: f64,
}

impl Preferences {
    /// Apply the remembered spectrum choices onto a lens's fresh settings,
    /// keeping the material-derived values (frequency range, dB ceiling).
    pub fn apply_spectrum(&self, settings: &mut SpectrumSettings) {
        let Some(remembered) = self.spectrum else {
            return;
        };
        settings.transform = remembered.transform;
        settings.fft_size = remembered.fft_size.clamp(256, 65_536);
        settings.hop_size = remembered.hop_size.clamp(1, settings.fft_size);
        settings.window = remembered.window;
        settings.db_range = remembered.db_range.clamp(6.0, 180.0);
        settings.waterfall_fraction = remembered.waterfall_fraction.clamp(0.0, 1.0);
        settings.cqt_bins_per_octave = remembered.cqt_bins_per_octave.clamp(1, 192);
    }

    /// Apply the remembered component question onto the workbench's fresh
    /// params, leaving every kernel-owned field as this build defines it.
    /// The caller clamps; a file from another build is a request, not a fact.
    pub fn apply_components(&self, params: &mut crate::decomposition::ConvolutionalParams) {
        let Some(remembered) = self.components else {
            return;
        };
        params.rank = remembered.rank;
        params.template_length = remembered.template_length;
    }

    /// Apply the remembered rhythm knobs, clamped to what this build offers.
    pub fn apply_rhythm(&self, settings: &mut RhythmLensSettings) {
        if let Some(remembered) = self.rhythm {
            *settings = remembered.normalized();
        }
    }

    /// Apply the remembered Loom knobs, clamped to what this build offers.
    pub fn apply_loom(&self, settings: &mut LoomLensSettings) {
        if let Some(remembered) = self.loom {
            *settings = remembered.normalized();
        }
    }

    /// Apply the remembered separation choices onto a lens's fresh settings.
    /// The widths are put back on the odd grid the transform accepts and the
    /// span onto a rung of the ladder; whether that rung fits this material's
    /// memory is the lens's question, not this file's.
    pub fn apply_separation(&self, settings: &mut HpssSettings, span_seconds: &mut f64) {
        let Some(remembered) = self.separation else {
            return;
        };
        settings.time_median_width = HpssSettings::clamp_median_width(remembered.time_median_width);
        settings.frequency_median_width =
            HpssSettings::clamp_median_width(remembered.frequency_median_width);
        if let Some(rung) = SPAN_LADDER_SECONDS
            .iter()
            .copied()
            .find(|seconds| (*seconds - remembered.span_seconds).abs() < 0.5)
        {
            *span_seconds = rung;
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PreferencesError {
    NoConfigDirectory,
    Io(String),
    Malformed(String),
}

impl fmt::Display for PreferencesError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoConfigDirectory => {
                write!(formatter, "no user configuration directory is available")
            }
            Self::Io(message) => write!(formatter, "preferences file: {message}"),
            Self::Malformed(message) => write!(formatter, "preferences are malformed: {message}"),
        }
    }
}

impl std::error::Error for PreferencesError {}

/// `<config dir>/software.ember.audec/preferences.json`.
pub fn preferences_path() -> Option<PathBuf> {
    dirs::config_dir().map(|root| root.join("software.ember.audec").join("preferences.json"))
}

/// Load the user's preferences; defaults when there is no file yet.
pub fn load() -> Result<Preferences, PreferencesError> {
    let path = preferences_path().ok_or(PreferencesError::NoConfigDirectory)?;
    load_from(&path)
}

/// Read, change, and write back the user's preferences atomically enough
/// for one desktop process: a temp file is written and renamed into place.
pub fn update(change: impl FnOnce(&mut Preferences)) -> Result<(), PreferencesError> {
    let path = preferences_path().ok_or(PreferencesError::NoConfigDirectory)?;
    let mut preferences = match load_from(&path) {
        Ok(preferences) => preferences,
        // A malformed file is replaced rather than preserved: it is a cache of
        // choices, and the diagnostic has already been surfaced by `load`.
        Err(PreferencesError::Malformed(_)) => Preferences::default(),
        Err(error) => return Err(error),
    };
    change(&mut preferences);
    save_to(&path, &preferences)
}

pub fn load_from(path: &Path) -> Result<Preferences, PreferencesError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Preferences::default());
        }
        Err(error) => return Err(PreferencesError::Io(error.to_string())),
    };
    let file: PreferencesFile = serde_json::from_slice(&bytes)
        .map_err(|error| PreferencesError::Malformed(error.to_string()))?;
    Ok(file.into_preferences())
}

pub fn save_to(path: &Path, preferences: &Preferences) -> Result<(), PreferencesError> {
    let parent = path
        .parent()
        .ok_or_else(|| PreferencesError::Io("preferences path has no parent".into()))?;
    fs::create_dir_all(parent).map_err(|error| PreferencesError::Io(error.to_string()))?;
    let file = PreferencesFile::from_preferences(preferences);
    let mut bytes = serde_json::to_vec_pretty(&file)
        .map_err(|error| PreferencesError::Io(error.to_string()))?;
    bytes.push(b'\n');
    // The temp name has to be unique per write, not per process: two writers
    // in one process (two tests, or two panes remembering at once) otherwise
    // share one temp path and one of the renames finds nothing there.
    static WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let temporary = parent.join(format!(
        ".preferences-{}-{}.json.tmp",
        std::process::id(),
        WRITE_SEQUENCE.fetch_add(1, atomic::Ordering::Relaxed)
    ));
    fs::write(&temporary, bytes).map_err(|error| PreferencesError::Io(error.to_string()))?;
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        PreferencesError::Io(error.to_string())
    })
}

/// On-disk form. Unknown fields are ignored so a newer build's file still
/// loads here; fields this build does not know are not preserved.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PreferencesFile {
    #[serde(default)]
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    spectrum: Option<SpectrumFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    components: Option<ComponentsFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rhythm: Option<RhythmFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    loom: Option<LoomFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    separation: Option<SeparationFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
struct RhythmFile {
    threshold_mad_multiplier: f32,
    tempo_window: usize,
}

impl Default for RhythmFile {
    fn default() -> Self {
        let settings = RhythmLensSettings::default();
        Self {
            threshold_mad_multiplier: settings.threshold_mad_multiplier,
            tempo_window: settings.tempo_window,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
struct LoomFile {
    lookbehind: usize,
    template_length: usize,
}

impl Default for LoomFile {
    fn default() -> Self {
        let settings = LoomLensSettings::default();
        Self {
            lookbehind: settings.lookbehind,
            template_length: settings.template_length,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
struct SeparationFile {
    time_median_width: usize,
    frequency_median_width: usize,
    span_seconds: f64,
}

impl Default for SeparationFile {
    fn default() -> Self {
        let settings = HpssSettings::default();
        Self {
            time_median_width: settings.time_median_width,
            frequency_median_width: settings.frequency_median_width,
            span_seconds: 30.0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
struct SpectrumFile {
    transform: String,
    fft_size: usize,
    hop_size: usize,
    window: String,
    db_range: f32,
    waterfall_fraction: f32,
    cqt_bins_per_octave: u8,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
struct ComponentsFile {
    rank: usize,
    template_length: usize,
}

impl Default for ComponentsFile {
    fn default() -> Self {
        let params = crate::decomposition::ConvolutionalParams::default();
        Self {
            rank: params.rank,
            template_length: params.template_length,
        }
    }
}

impl Default for SpectrumFile {
    fn default() -> Self {
        let settings = SpectrumSettings::default();
        Self {
            transform: transform_name(settings.transform).into(),
            fft_size: settings.fft_size,
            hop_size: settings.hop_size,
            window: window_name(settings.window).into(),
            db_range: settings.db_range,
            waterfall_fraction: settings.waterfall_fraction,
            cqt_bins_per_octave: settings.cqt_bins_per_octave,
        }
    }
}

const FILE_VERSION: u32 = 1;

impl PreferencesFile {
    fn from_preferences(preferences: &Preferences) -> Self {
        Self {
            version: FILE_VERSION,
            spectrum: preferences.spectrum.map(|settings| SpectrumFile {
                transform: transform_name(settings.transform).into(),
                fft_size: settings.fft_size,
                hop_size: settings.hop_size,
                window: window_name(settings.window).into(),
                db_range: settings.db_range,
                waterfall_fraction: settings.waterfall_fraction,
                cqt_bins_per_octave: settings.cqt_bins_per_octave,
            }),
            components: preferences.components.map(|choices| ComponentsFile {
                rank: choices.rank,
                template_length: choices.template_length,
            }),
            rhythm: preferences.rhythm.map(|settings| RhythmFile {
                threshold_mad_multiplier: settings.threshold_mad_multiplier,
                tempo_window: settings.tempo_window,
            }),
            loom: preferences.loom.map(|settings| LoomFile {
                lookbehind: settings.lookbehind,
                template_length: settings.template_length,
            }),
            separation: preferences.separation.map(|choices| SeparationFile {
                time_median_width: choices.time_median_width,
                frequency_median_width: choices.frequency_median_width,
                span_seconds: choices.span_seconds,
            }),
        }
    }

    fn into_preferences(self) -> Preferences {
        Preferences {
            spectrum: self.spectrum.map(|file| SpectrumSettings {
                transform: parse_transform(&file.transform).unwrap_or_default(),
                fft_size: file.fft_size,
                hop_size: file.hop_size,
                window: parse_window(&file.window).unwrap_or(WindowFunction::Hann),
                db_range: file.db_range,
                waterfall_fraction: file.waterfall_fraction,
                cqt_bins_per_octave: file.cqt_bins_per_octave,
                ..SpectrumSettings::default()
            }),
            components: self.components.map(|file| ComponentChoices {
                rank: file.rank,
                template_length: file.template_length,
            }),
            rhythm: self.rhythm.map(|file| {
                RhythmLensSettings {
                    threshold_mad_multiplier: file.threshold_mad_multiplier,
                    tempo_window: file.tempo_window,
                }
                .normalized()
            }),
            loom: self.loom.map(|file| {
                LoomLensSettings {
                    lookbehind: file.lookbehind,
                    template_length: file.template_length,
                }
                .normalized()
            }),
            separation: self.separation.map(|file| SeparationChoices {
                time_median_width: file.time_median_width,
                frequency_median_width: file.frequency_median_width,
                span_seconds: file.span_seconds,
            }),
        }
    }
}

fn transform_name(transform: SpectralTransform) -> &'static str {
    match transform {
        SpectralTransform::Fft => "fft",
        SpectralTransform::ConstantQ => "constant_q",
    }
}

fn parse_transform(name: &str) -> Option<SpectralTransform> {
    match name {
        "fft" => Some(SpectralTransform::Fft),
        "constant_q" => Some(SpectralTransform::ConstantQ),
        _ => None,
    }
}

fn window_name(window: WindowFunction) -> &'static str {
    match window {
        WindowFunction::Rectangular => "rectangular",
        WindowFunction::Hann => "hann",
        WindowFunction::Blackman => "blackman",
    }
}

fn parse_window(name: &str) -> Option<WindowFunction> {
    match name {
        "rectangular" => Some(WindowFunction::Rectangular),
        "hann" => Some(WindowFunction::Hann),
        "blackman" => Some(WindowFunction::Blackman),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One directory per file, so two tests saving at the same time cannot
    /// see each other's writes.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "audec-preferences-{}-{}",
            std::process::id(),
            name.trim_end_matches(".json")
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn spectrum_choices_round_trip_and_keep_material_values() {
        let path = scratch("round-trip.json");
        let _ = fs::remove_file(&path);
        assert_eq!(load_from(&path).unwrap(), Preferences::default());
        let chosen = SpectrumSettings {
            transform: SpectralTransform::ConstantQ,
            fft_size: 16_384,
            hop_size: 4_096,
            window: WindowFunction::Blackman,
            db_range: 72.0,
            waterfall_fraction: 0.4,
            ..SpectrumSettings::default()
        };
        save_to(
            &path,
            &Preferences {
                spectrum: Some(chosen),
                ..Preferences::default()
            },
        )
        .unwrap();
        let loaded = load_from(&path).unwrap();
        let remembered = loaded.spectrum.unwrap();
        assert_eq!(remembered.transform, SpectralTransform::ConstantQ);
        assert_eq!(remembered.fft_size, 16_384);
        assert_eq!(remembered.window, WindowFunction::Blackman);
        // Applying onto a lens's fresh settings keeps the material-derived
        // ceiling and frequency range.
        let mut fresh = SpectrumSettings {
            db_ceiling: -13.0,
            min_frequency_hz: 30.0,
            ..SpectrumSettings::default()
        };
        loaded.apply_spectrum(&mut fresh);
        assert_eq!(fresh.db_ceiling, -13.0);
        assert_eq!(fresh.min_frequency_hz, 30.0);
        assert_eq!(fresh.transform, SpectralTransform::ConstantQ);
        assert_eq!(fresh.db_range, 72.0);
    }

    /// The two knobs a musician turns survive a relaunch; the kernel's own
    /// numbers are this build's, not the file's.
    #[test]
    fn the_component_question_round_trips_without_pinning_the_kernel() {
        let path = scratch("components.json");
        let _ = fs::remove_file(&path);
        assert_eq!(load_from(&path).unwrap().components, None);
        save_to(
            &path,
            &Preferences {
                spectrum: None,
                components: Some(ComponentChoices {
                    rank: 9,
                    template_length: 20,
                }),
                ..Preferences::default()
            },
        )
        .unwrap();
        let loaded = load_from(&path).unwrap();
        assert_eq!(
            loaded.components,
            Some(ComponentChoices {
                rank: 9,
                template_length: 20
            })
        );
        let mut params = crate::analysis::default_component_params();
        let kernel_iterations = params.iterations;
        let kernel_seed = params.seed;
        loaded.apply_components(&mut params);
        assert_eq!(params.rank, 9);
        assert_eq!(params.template_length, 20);
        assert_eq!(params.iterations, kernel_iterations);
        assert_eq!(params.seed, kernel_seed);

        // No remembered question leaves this build's own alone.
        let mut untouched = crate::analysis::default_component_params();
        Preferences::default().apply_components(&mut untouched);
        assert_eq!(untouched, crate::analysis::default_component_params());
    }

    /// Row 20's setting is a choice like the others, so it is remembered.
    #[test]
    fn constant_q_resolution_is_remembered_and_clamped_on_the_way_back() {
        let path = scratch("cqt-bins.json");
        let _ = fs::remove_file(&path);
        save_to(
            &path,
            &Preferences {
                spectrum: Some(SpectrumSettings {
                    transform: SpectralTransform::ConstantQ,
                    cqt_bins_per_octave: 36,
                    ..SpectrumSettings::default()
                }),
                components: None,
                ..Preferences::default()
            },
        )
        .unwrap();
        let mut fresh = SpectrumSettings::default();
        load_from(&path).unwrap().apply_spectrum(&mut fresh);
        assert_eq!(fresh.cqt_bins_per_octave, 36);
        assert_eq!(fresh.transform, SpectralTransform::ConstantQ);

        // A file from another build that asks for an impossible grid is held
        // rather than refused: it is a cache of choices, not a contract.
        fs::write(
            &path,
            br#"{"version": 1, "spectrum": {"transform": "constant_q", "cqt_bins_per_octave": 255}}"#,
        )
        .unwrap();
        let mut held = SpectrumSettings::default();
        load_from(&path).unwrap().apply_spectrum(&mut held);
        assert_eq!(held.cqt_bins_per_octave, 192);

        // A spectrum block written before this field existed still loads, at
        // audec's own grid.
        fs::write(
            &path,
            br#"{"version": 1, "spectrum": {"transform": "constant_q", "fft_size": 4096}}"#,
        )
        .unwrap();
        let older = load_from(&path).unwrap().spectrum.unwrap();
        assert_eq!(
            older.cqt_bins_per_octave,
            crate::settings::DEFAULT_CQT_BINS_PER_OCTAVE
        );
    }

    #[test]
    fn lens_knob_choices_round_trip_and_are_clamped_to_what_this_build_offers() {
        let path = scratch("knobs.json");
        let _ = fs::remove_file(&path);
        // No file: no choice has been made, so nothing is applied and the
        // lens keeps this build's own defaults.
        let mut rhythm = RhythmLensSettings::default();
        let mut loom = LoomLensSettings::default();
        load_from(&path).unwrap().apply_rhythm(&mut rhythm);
        load_from(&path).unwrap().apply_loom(&mut loom);
        assert_eq!(rhythm, RhythmLensSettings::default());
        assert_eq!(loom, LoomLensSettings::default());

        let mut chosen_rhythm = RhythmLensSettings::default();
        chosen_rhythm.step_sensitivity(-2);
        chosen_rhythm.cycle_tempo_window();
        let mut chosen_loom = LoomLensSettings::default();
        chosen_loom.step_lookbehind(1);
        chosen_loom.step_template_length(2); // two notches: 240 ms -> 1000 ms
        save_to(
            &path,
            &Preferences {
                spectrum: None,
                rhythm: Some(chosen_rhythm),
                loom: Some(chosen_loom),
                ..Preferences::default()
            },
        )
        .unwrap();
        let loaded = load_from(&path).unwrap();
        loaded.apply_rhythm(&mut rhythm);
        loaded.apply_loom(&mut loom);
        assert_eq!(rhythm.tempo_range(), (60.0, 120.0));
        assert!((rhythm.threshold_mad_multiplier - 2.0).abs() < 1.0e-5);
        assert_eq!(loom.lookbehind_seconds(), 120);
        assert_eq!(loom.template_milliseconds(), 1_000);

        // A file from a build that offered more positions than this one does
        // falls back rather than indexing off the end of the table.
        fs::write(
            &path,
            br#"{"version": 1, "rhythm": {"threshold_mad_multiplier": 99.0, "tempo_window": 40}, "loom": {"lookbehind": 7, "template_length": 7}}"#,
        )
        .unwrap();
        let wild = load_from(&path).unwrap();
        assert_eq!(wild.rhythm.unwrap().tempo_window, 0);
        assert_eq!(wild.loom.unwrap(), LoomLensSettings::default());
    }

    #[test]
    fn unknown_fields_are_tolerated_and_garbage_is_a_named_error() {
        let path = scratch("forward.json");
        fs::write(
            &path,
            br#"{"version": 9, "future": {"x": 1}, "spectrum": {"transform": "warp", "fft_size": 512, "hop_size": 128, "window": "kaiser", "db_range": 60.0, "waterfall_fraction": 0.5}}"#,
        )
        .unwrap();
        let loaded = load_from(&path).unwrap().spectrum.unwrap();
        assert_eq!(
            loaded.transform,
            SpectralTransform::Fft,
            "unknown transform falls back"
        );
        // A spectrum block from another build that lacks a field still loads.
        fs::write(
            &path,
            br#"{"version": 2, "spectrum": {"transform": "constant_q"}}"#,
        )
        .unwrap();
        let partial = load_from(&path).unwrap().spectrum.unwrap();
        assert_eq!(partial.transform, SpectralTransform::ConstantQ);
        assert_eq!(partial.fft_size, SpectrumSettings::default().fft_size);
        assert_eq!(
            loaded.window,
            WindowFunction::Hann,
            "unknown window falls back"
        );
        assert_eq!(loaded.fft_size, 512);
        fs::write(&path, b"{not json").unwrap();
        assert!(matches!(
            load_from(&path),
            Err(PreferencesError::Malformed(_))
        ));
    }

    #[test]
    fn separation_choices_round_trip_and_land_on_widths_the_transform_accepts() {
        let path = scratch("separation.json");
        let _ = fs::remove_file(&path);
        assert_eq!(load_from(&path).unwrap().separation, None);
        save_to(
            &path,
            &Preferences {
                spectrum: None,
                separation: Some(SeparationChoices {
                    time_median_width: 25,
                    frequency_median_width: 9,
                    span_seconds: 60.0,
                }),
            },
        )
        .unwrap();
        let loaded = load_from(&path).unwrap();
        let mut settings = HpssSettings::default();
        let mut span = 30.0;
        loaded.apply_separation(&mut settings, &mut span);
        assert_eq!(settings.time_median_width, 25);
        assert_eq!(settings.frequency_median_width, 9);
        assert_eq!(span, 60.0);
        settings.validate().unwrap();

        // A file written by hand, or by a build with other bounds, still has
        // to yield a width this transform will run and a span on the ladder.
        fs::write(
            &path,
            br#"{"version": 1, "separation": {"time_median_width": 400, "frequency_median_width": 2, "span_seconds": 47.0}}"#,
        )
        .unwrap();
        let mut settings = HpssSettings::default();
        let mut span = 30.0;
        load_from(&path)
            .unwrap()
            .apply_separation(&mut settings, &mut span);
        settings.validate().unwrap();
        assert_eq!(
            settings.time_median_width,
            crate::hpss::MEDIAN_WIDTH_MAXIMUM
        );
        assert_eq!(
            settings.frequency_median_width,
            crate::hpss::MEDIAN_WIDTH_MINIMUM
        );
        assert_eq!(span, 30.0, "a span off the ladder is not adopted");

        // Remembering nothing must stay remembering nothing: a lens that has
        // never been touched is not given a stored copy of today's default.
        let path = scratch("separation-absent.json");
        let _ = fs::remove_file(&path);
        save_to(
            &path,
            &Preferences {
                spectrum: None,
                separation: None,
            },
        )
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("separation"), "{text}");
    }
}
