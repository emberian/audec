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

use serde::{Deserialize, Serialize};

use crate::settings::{SpectralTransform, SpectrumSettings, WindowFunction};

/// Preferences in domain terms.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Preferences {
    /// Lens spectrum choices to apply when a lens is created. The dB ceiling
    /// is deliberately not remembered: it follows each material's peak.
    pub spectrum: Option<SpectrumSettings>,
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
    let temporary = parent.join(format!(".preferences-{}.json.tmp", std::process::id()));
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SpectrumFile {
    transform: String,
    fft_size: usize,
    hop_size: usize,
    window: String,
    db_range: f32,
    waterfall_fraction: f32,
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
                ..SpectrumSettings::default()
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("audec-preferences-{}", std::process::id()));
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
}
