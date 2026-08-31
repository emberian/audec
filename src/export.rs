//! Durable, deterministic offline WAV export for rendered project audio.
//!
//! This is deliberately a small boundary above [`crate::render`].  The DAW
//! engine (or an analysis/reconstruction worker) supplies immutable
//! [`ProjectAudio`], and this module makes an all-or-nothing file at a chosen
//! path.  It never opens an audio device, resamples, changes channel count, or
//! adds a master limiter.
//!
//! The policy is intentionally explicit:
//!
//! * `Pcm16` and `Pcm24` clip finite post-gain samples to `[-1, 1]` only while
//!   quantizing; the returned report says exactly how many samples clipped.
//! * `Float32` preserves finite values above full scale. This is useful for
//!   continuing work in a float-capable DAW, but downstream players may clip.
//! * Optional TPDF dither is seeded, so a given audio buffer and request make
//!   byte-identical integer exports. It is not applied to float WAV because
//!   there is no integer quantization step.
//! * Non-finite source samples become silence in the renderer and are counted
//!   in [`RenderStats`].
//!
//! A destination is only replaced after its sibling temporary file has been
//! fully written and synced. Cancellation or an error removes that temporary
//! file and leaves a pre-existing destination untouched.

use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crate::audio::{AudioFormat, PcmRenderer, ProjectAudio};
use crate::render::{
    self, RenderGain, RenderObserver, RenderPhase, RenderProgress, RenderRange, RenderRequest,
    RenderStats,
};

pub use crate::render::{Dither, WavSampleFormat};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// The stage currently in progress.  Values are always reported in
/// non-decreasing frame order inside a stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportPhase {
    Rendering,
    Encoding,
    Writing,
}

/// Progress expressed in source/project frames, even during byte-oriented
/// file writing. This keeps one progress UI valid for all sample formats.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExportProgress {
    pub phase: ExportPhase,
    pub completed_frames: u64,
    pub total_frames: u64,
}

impl ExportProgress {
    pub fn fraction(self) -> f64 {
        if self.total_frames == 0 {
            1.0
        } else {
            self.completed_frames as f64 / self.total_frames as f64
        }
    }
}

/// UI-agnostic cancellation and progress seam.  Implementations must return
/// quickly: it is polled between render blocks and file-write chunks.
pub trait ExportObserver {
    fn is_cancelled(&mut self) -> bool {
        false
    }

    fn report_progress(&mut self, _progress: ExportProgress) {}
}

#[derive(Default)]
pub struct NoopExportObserver;

impl ExportObserver for NoopExportObserver {}

/// Cloneable cancellation handle suitable for passing to a worker. A UI that
/// also needs progress can use a small `ExportObserver` wrapper around it.
#[derive(Clone, Debug, Default)]
pub struct ExportCancellation {
    cancelled: Arc<AtomicBool>,
}

impl ExportCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl ExportObserver for ExportCancellation {
    fn is_cancelled(&mut self) -> bool {
        ExportCancellation::is_cancelled(self)
    }
}

/// One native-format WAV export. No sample-rate or channel conversion is
/// hidden in this API; `ProjectAudio`'s exact format becomes the WAV format.
#[derive(Clone, Debug, PartialEq)]
pub struct WavExportRequest {
    pub destination: PathBuf,
    /// `Pcm24` is the practical default for a reconstruction handoff: it
    /// avoids 16-bit quantization loss without committing to float-only tools.
    pub sample_format: WavSampleFormat,
    pub dither: Dither,
    pub gain: RenderGain,
    /// `None` means the complete, end-exclusive project-audio range.
    pub range: Option<RenderRange>,
    /// Maximum frames requested at a time from the in-memory renderer.
    pub block_frames: usize,
}

impl WavExportRequest {
    pub fn new(destination: impl Into<PathBuf>) -> Self {
        Self {
            destination: destination.into(),
            sample_format: WavSampleFormat::Pcm24,
            dither: Dither::Tpdf {
                seed: 0xa0de_c001_u64,
            },
            gain: RenderGain::Unity,
            range: None,
            block_frames: 4_096,
        }
    }

    fn render_request(&self, audio: &ProjectAudio) -> Result<RenderRequest, ExportError> {
        let range = match self.range {
            Some(range) => range,
            None if audio.frame_count().0 != 0 => {
                RenderRange::from_frames(0, audio.frame_count().0).expect("non-empty range")
            }
            None => return Err(ExportError::EmptyAudio),
        };
        let mut request = RenderRequest::new(
            range,
            render::RenderFormat::new(audio.format(), self.sample_format),
        );
        request.gain = self.gain;
        request.dither = self.dither;
        request.block_frames = self.block_frames;
        Ok(request)
    }
}

/// Facts about a successful, durable export.
#[derive(Clone, Debug, PartialEq)]
pub struct WavExportReport {
    pub destination: PathBuf,
    pub audio_format: AudioFormat,
    pub sample_format: WavSampleFormat,
    pub range: RenderRange,
    pub bytes_written: u64,
    pub stats: RenderStats,
    pub clipped_samples: u64,
    /// `false` for float WAV even when the request selects TPDF, because float
    /// encoding performs no quantization.
    pub dither_applied: bool,
}

/// Immutable render material paired with the aggregate project revision that
/// produced it.  This is the export-side counterpart of an audition snapshot:
/// callers may keep editing while an export runs, but the resulting file can
/// always state exactly which construction it rendered.
#[derive(Clone, Debug)]
pub struct RevisionPinnedAudio {
    pub aggregate_revision: u64,
    pub audio: ProjectAudio,
}

impl RevisionPinnedAudio {
    pub fn new(aggregate_revision: u64, audio: ProjectAudio) -> Self {
        Self {
            aggregate_revision,
            audio,
        }
    }
}

/// A successful WAV report plus the revision that was rendered.  The caller
/// must compare this token with its current project revision before presenting
/// the result as an export of the *current* project; a later edit does not
/// invalidate the already-correct historical export.
#[derive(Clone, Debug, PartialEq)]
pub struct RevisionPinnedWavExportReport {
    pub aggregate_revision: u64,
    pub wav: WavExportReport,
}

/// Render `audio` and atomically replace `request.destination` with a classic
/// RIFF/WAV file.  The source is cloned cheaply and never mutated.
pub fn export_project_audio_to_wav<O: ExportObserver>(
    audio: ProjectAudio,
    request: &WavExportRequest,
    observer: &mut O,
) -> Result<WavExportReport, ExportError> {
    check_cancelled(observer)?;
    let render_request = request.render_request(&audio)?;
    let mut source = PcmRenderer::new(audio.clone());
    let mut bridge = RenderObserverBridge { observer };
    let rendered = render::render_project(&mut source, &render_request, &mut bridge)
        .map_err(map_render_error)?;

    check_cancelled(bridge.observer)?;
    bridge.observer.report_progress(ExportProgress {
        phase: ExportPhase::Encoding,
        completed_frames: 0,
        total_frames: render_request.range.len(),
    });
    let encoded = render::encode_wav(&rendered, request.sample_format, request.dither)
        .map_err(map_render_error)?;
    bridge.observer.report_progress(ExportProgress {
        phase: ExportPhase::Encoding,
        completed_frames: render_request.range.len(),
        total_frames: render_request.range.len(),
    });
    check_cancelled(bridge.observer)?;

    write_wav_atomically(
        &request.destination,
        &encoded.bytes,
        audio.format(),
        request.sample_format,
        render_request.range.len(),
        bridge.observer,
    )?;

    Ok(WavExportReport {
        destination: request.destination.clone(),
        audio_format: audio.format(),
        sample_format: request.sample_format,
        range: render_request.range,
        bytes_written: encoded.bytes.len() as u64,
        stats: rendered.stats,
        clipped_samples: encoded.clipped_samples,
        dither_applied: encoded.dithered,
    })
}

/// Export immutable audio while retaining its source revision in the result.
/// There is deliberately no read of mutable project state here: the snapshot
/// is the authority, which makes offline export and bounce-on-play agree on
/// what revision they represent.
pub fn export_revision_pinned_audio_to_wav<O: ExportObserver>(
    pinned: RevisionPinnedAudio,
    request: &WavExportRequest,
    observer: &mut O,
) -> Result<RevisionPinnedWavExportReport, ExportError> {
    let aggregate_revision = pinned.aggregate_revision;
    let wav = export_project_audio_to_wav(pinned.audio, request, observer)?;
    Ok(RevisionPinnedWavExportReport {
        aggregate_revision,
        wav,
    })
}

struct RenderObserverBridge<'a, O> {
    observer: &'a mut O,
}

impl<O: ExportObserver> RenderObserver for RenderObserverBridge<'_, O> {
    fn is_cancelled(&mut self) -> bool {
        self.observer.is_cancelled()
    }

    fn report_progress(&mut self, progress: RenderProgress) {
        self.observer.report_progress(ExportProgress {
            phase: match progress.phase {
                RenderPhase::Rendering => ExportPhase::Rendering,
                RenderPhase::Encoding => ExportPhase::Encoding,
            },
            completed_frames: progress.completed_frames,
            total_frames: progress.total_frames,
        });
    }
}

/// File-system failures and rendering failures remain distinct for a UI that
/// wants to offer retry/relink versus changing an export setting.
#[derive(Debug)]
pub enum ExportError {
    EmptyAudio,
    DestinationParentMissing { path: PathBuf },
    DestinationHasNoFileName { path: PathBuf },
    Render(render::RenderError),
    Io { path: PathBuf, source: io::Error },
    Cancelled,
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyAudio => write!(f, "cannot export an empty ProjectAudio buffer"),
            Self::DestinationParentMissing { path } => {
                write!(f, "export directory does not exist: {}", path.display())
            }
            Self::DestinationHasNoFileName { path } => {
                write!(f, "export destination has no filename: {}", path.display())
            }
            Self::Render(error) => write!(f, "offline render failed: {error}"),
            Self::Io { path, source } => write!(f, "I/O at {}: {source}", path.display()),
            Self::Cancelled => write!(f, "offline export cancelled"),
        }
    }
}

impl Error for ExportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Render(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn check_cancelled(observer: &mut impl ExportObserver) -> Result<(), ExportError> {
    if observer.is_cancelled() {
        Err(ExportError::Cancelled)
    } else {
        Ok(())
    }
}

fn map_render_error(error: render::RenderError) -> ExportError {
    if error == render::RenderError::Cancelled {
        ExportError::Cancelled
    } else {
        ExportError::Render(error)
    }
}

fn write_wav_atomically<O: ExportObserver>(
    destination: &Path,
    bytes: &[u8],
    audio_format: AudioFormat,
    sample_format: WavSampleFormat,
    total_frames: u64,
    observer: &mut O,
) -> Result<(), ExportError> {
    const WAV_HEADER_BYTES: usize = 44;
    const WRITE_CHUNK_BYTES: usize = 256 * 1024;

    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(ExportError::DestinationParentMissing {
            path: parent.to_path_buf(),
        });
    }
    let file_name =
        destination
            .file_name()
            .ok_or_else(|| ExportError::DestinationHasNoFileName {
                path: destination.to_path_buf(),
            })?;
    let mut temporary = None;
    let result = (|| -> Result<(), ExportError> {
        let (temp, mut file) = create_sibling_temp(parent, file_name)?;
        temporary = Some(temp);
        let temporary = temporary
            .as_ref()
            .expect("temporary path was just assigned");
        check_cancelled(observer)?;
        if bytes.len() < WAV_HEADER_BYTES {
            return Err(ExportError::Io {
                path: temporary.clone(),
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    "WAV encoder returned no header",
                ),
            });
        }

        let block_align = usize::from(audio_format.channels.get())
            .checked_mul(usize::from(sample_format.bits_per_sample() / 8))
            .ok_or_else(|| ExportError::Io {
                path: temporary.clone(),
                source: io::Error::new(io::ErrorKind::Other, "WAV block alignment overflow"),
            })?;
        let data = &bytes[WAV_HEADER_BYTES..];
        if data.len() % block_align != 0 {
            return Err(ExportError::Io {
                path: temporary.clone(),
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    "WAV data ends on a partial frame",
                ),
            });
        }

        observer.report_progress(ExportProgress {
            phase: ExportPhase::Writing,
            completed_frames: 0,
            total_frames,
        });
        check_cancelled(observer)?;
        file.write_all(&bytes[..WAV_HEADER_BYTES])
            .map_err(|source| ExportError::Io {
                path: temporary.clone(),
                source,
            })?;

        let chunk_bytes = (WRITE_CHUNK_BYTES / block_align).max(1) * block_align;
        let mut offset = 0_usize;
        while offset < data.len() {
            check_cancelled(observer)?;
            let end = (offset + chunk_bytes).min(data.len());
            file.write_all(&data[offset..end])
                .map_err(|source| ExportError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            offset = end;
            observer.report_progress(ExportProgress {
                phase: ExportPhase::Writing,
                completed_frames: (offset / block_align) as u64,
                total_frames,
            });
        }
        check_cancelled(observer)?;
        file.sync_all().map_err(|source| ExportError::Io {
            path: temporary.clone(),
            source,
        })?;
        drop(file);
        check_cancelled(observer)?;
        fs::rename(&temporary, destination).map_err(|source| ExportError::Io {
            path: destination.to_path_buf(),
            source,
        })?;
        // Best effort: this completes the rename-durability story on Unix;
        // Windows filesystems do not universally permit directory syncing.
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        if let Some(temporary) = temporary {
            let _ = fs::remove_file(temporary);
        }
    }
    result
}

fn create_sibling_temp(
    parent: &Path,
    name: &std::ffi::OsStr,
) -> Result<(PathBuf, File), ExportError> {
    for _ in 0..32 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{}.audec-export-{}-{sequence}.tmp",
            name.to_string_lossy(),
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(ExportError::Io {
                    path: temporary,
                    source,
                })
            }
        }
    }
    Err(ExportError::Io {
        path: parent.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a unique audec export temporary path",
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    struct TempDirectory {
        path: PathBuf,
    }

    impl TempDirectory {
        fn new() -> Self {
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "audec-export-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn audio(samples: Vec<f32>) -> ProjectAudio {
        ProjectAudio::from_interleaved(AudioFormat::new(48_000, 2).unwrap(), samples).unwrap()
    }

    #[test]
    fn writes_an_atomic_native_format_pcm24_wav() {
        let directory = TempDirectory::new();
        let destination = directory.path.join("take.wav");
        let mut request = WavExportRequest::new(&destination);
        request.dither = Dither::None;
        let report = export_project_audio_to_wav(
            audio(vec![-1.0, 1.0, 0.0, 0.5, -0.5, 0.25]),
            &request,
            &mut NoopExportObserver,
        )
        .unwrap();

        let bytes = fs::read(&destination).unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes([bytes[22], bytes[23]]), 2);
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            48_000
        );
        assert_eq!(u16::from_le_bytes([bytes[34], bytes[35]]), 24);
        assert_eq!(bytes.len(), 44 + 3 * 2 * 3);
        assert_eq!(report.bytes_written, bytes.len() as u64);
        assert_eq!(report.stats.frames, 3);
        assert_eq!(report.clipped_samples, 0);
        assert!(!report.dither_applied);
    }

    #[test]
    fn seeded_integer_dither_is_byte_deterministic_and_float_is_not_dithered() {
        let directory = TempDirectory::new();
        let audio = audio(vec![0.0; 2_048]);
        let mut first = WavExportRequest::new(directory.path.join("first.wav"));
        first.sample_format = WavSampleFormat::Pcm16;
        first.dither = Dither::Tpdf { seed: 42 };
        let mut second = first.clone();
        second.destination = directory.path.join("second.wav");
        let mut float = first.clone();
        float.destination = directory.path.join("float.wav");
        float.sample_format = WavSampleFormat::Float32;

        let first_report =
            export_project_audio_to_wav(audio.clone(), &first, &mut NoopExportObserver).unwrap();
        let second_report =
            export_project_audio_to_wav(audio.clone(), &second, &mut NoopExportObserver).unwrap();
        let float_report =
            export_project_audio_to_wav(audio, &float, &mut NoopExportObserver).unwrap();
        assert_eq!(
            fs::read(&first.destination).unwrap(),
            fs::read(&second.destination).unwrap()
        );
        assert!(first_report.dither_applied);
        assert!(second_report.dither_applied);
        assert!(!float_report.dither_applied);
    }

    #[test]
    fn revision_pinned_export_reports_the_snapshot_revision() {
        let directory = TempDirectory::new();
        let request = WavExportRequest::new(directory.path.join("pinned.wav"));
        let report = export_revision_pinned_audio_to_wav(
            RevisionPinnedAudio::new(73, audio(vec![0.0, 0.0, 0.25, -0.25])),
            &request,
            &mut NoopExportObserver,
        )
        .unwrap();

        assert_eq!(report.aggregate_revision, 73);
        assert_eq!(report.wav.destination, request.destination);
        assert!(report.wav.destination.is_file());
    }

    struct CancelAtWriteStart {
        cancelled: bool,
    }

    impl ExportObserver for CancelAtWriteStart {
        fn is_cancelled(&mut self) -> bool {
            self.cancelled
        }

        fn report_progress(&mut self, progress: ExportProgress) {
            if progress.phase == ExportPhase::Writing && progress.completed_frames == 0 {
                self.cancelled = true;
            }
        }
    }

    #[test]
    fn cancellation_removes_temp_and_never_replaces_existing_destination() {
        let directory = TempDirectory::new();
        let destination = directory.path.join("protected.wav");
        fs::write(&destination, b"last known good export").unwrap();
        let request = WavExportRequest::new(&destination);
        let error = export_project_audio_to_wav(
            audio(vec![0.0, 0.0, 0.25, -0.25]),
            &request,
            &mut CancelAtWriteStart { cancelled: false },
        )
        .unwrap_err();

        assert!(matches!(error, ExportError::Cancelled));
        assert_eq!(fs::read(&destination).unwrap(), b"last known good export");
        let children = fs::read_dir(&directory.path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(children, vec!["protected.wav"]);
    }
}
