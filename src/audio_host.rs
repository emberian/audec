//! Rodio device ownership and independent project/audition playback buses.
//!
//! [`AudioHost::open`] consumes already-decoded [`ProjectAudio`]. File decoding
//! belongs at the project-loading boundary, so playback never opens or decodes
//! the source file a second time.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rodio::mixer::Mixer;
use rodio::{DeviceSinkBuilder, DeviceSinkError, MixerDeviceSink, Player, Source};

use crate::audio::{
    AudioError, AudioFormat, PcmRenderer, ProjectAudio, ProjectFrame, ProjectRenderer,
    TransportHandle, TransportSource,
};

/// A finite mono or stereo PCM clip for the independent audition bus.
///
/// Cloning a clip only clones its `Arc`; audition does not duplicate the PCM.
#[derive(Clone, Debug)]
pub struct AuditionClip {
    audio: ProjectAudio,
}

impl AuditionClip {
    pub fn mono(sample_rate: u32, samples: Vec<f32>) -> Result<Self, AudioHostError> {
        Self::from_interleaved(AudioFormat::new(sample_rate, 1)?, samples)
    }

    pub fn stereo(sample_rate: u32, samples: Vec<f32>) -> Result<Self, AudioHostError> {
        Self::from_interleaved(AudioFormat::new(sample_rate, 2)?, samples)
    }

    pub fn from_interleaved(
        format: AudioFormat,
        samples: Vec<f32>,
    ) -> Result<Self, AudioHostError> {
        Self::from_project_audio(ProjectAudio::from_interleaved(format, samples)?)
    }

    pub fn from_shared(format: AudioFormat, samples: Arc<[f32]>) -> Result<Self, AudioHostError> {
        Self::from_project_audio(ProjectAudio::new(format, samples)?)
    }

    pub fn from_project_audio(audio: ProjectAudio) -> Result<Self, AudioHostError> {
        let channels = audio.format().channels.get();
        if !(1..=2).contains(&channels) {
            return Err(AudioHostError::UnsupportedAuditionChannels(channels));
        }
        Ok(Self { audio })
    }

    pub fn format(&self) -> AudioFormat {
        self.audio.format()
    }

    pub fn frame_count(&self) -> ProjectFrame {
        self.audio.frame_count()
    }

    pub fn interleaved(&self) -> &[f32] {
        self.audio.interleaved()
    }
}

#[derive(Debug)]
pub enum AudioHostError {
    Device(DeviceSinkError),
    Audio(AudioError),
    UnsupportedAuditionChannels(u16),
}

impl fmt::Display for AudioHostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Device(error) => write!(f, "could not open the audio output: {error}"),
            Self::Audio(error) => error.fmt(f),
            Self::UnsupportedAuditionChannels(channels) => write!(
                f,
                "audition clips must be mono or stereo, got {channels} channels"
            ),
        }
    }
}

impl std::error::Error for AudioHostError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Device(error) => Some(error),
            Self::Audio(error) => Some(error),
            Self::UnsupportedAuditionChannels(_) => None,
        }
    }
}

impl From<DeviceSinkError> for AudioHostError {
    fn from(error: DeviceSinkError) -> Self {
        Self::Device(error)
    }
}

impl From<AudioError> for AudioHostError {
    fn from(error: AudioError) -> Self {
        Self::Audio(error)
    }
}

/// Owns the output device for as long as project or preview audio may play.
///
/// Project playback is an always-connected [`TransportSource`]. Its transport
/// handle is the sole play/pause/seek authority. The separate preview `Player`
/// can be replaced or stopped without changing project transport state.
pub struct AudioHost {
    transport: TransportHandle,
    // Field order is intentional: players are dropped before their device.
    buses: PlaybackBuses,
    _device: MixerDeviceSink,
}

impl AudioHost {
    /// Open the default output device around already-decoded project PCM.
    pub fn open(project: ProjectAudio) -> Result<Self, AudioHostError> {
        Self::open_renderer(PcmRenderer::new(project))
    }

    /// Open the default output device around any project renderer.
    ///
    /// Incremental/whole-bounce publication lives inside the renderer supplied
    /// here. The device, project player, transport handle, and independent
    /// audition bus therefore remain alive while project revisions change.
    pub fn open_renderer<R: ProjectRenderer>(renderer: R) -> Result<Self, AudioHostError> {
        let mut device = DeviceSinkBuilder::open_default_sink()?;
        device.log_on_drop(false);
        let (transport, buses) = PlaybackBuses::connect_renderer(device.mixer(), renderer);
        Ok(Self {
            transport,
            buses,
            _device: device,
        })
    }

    pub fn transport(&self) -> TransportHandle {
        self.transport.clone()
    }

    /// Replace the audition bus with `clip` without touching project transport.
    pub fn audition(&self, clip: AuditionClip) {
        self.buses.audition(clip);
    }

    pub fn audition_mono(&self, sample_rate: u32, samples: Vec<f32>) -> Result<(), AudioHostError> {
        self.audition(AuditionClip::mono(sample_rate, samples)?);
        Ok(())
    }

    pub fn audition_stereo(
        &self,
        sample_rate: u32,
        samples: Vec<f32>,
    ) -> Result<(), AudioHostError> {
        self.audition(AuditionClip::stereo(sample_rate, samples)?);
        Ok(())
    }

    pub fn stop_preview(&self) {
        self.buses.stop_preview();
    }

    pub fn preview_active(&self) -> bool {
        self.buses.preview_active()
    }
}

struct PlaybackBuses {
    // Keep both players alive while their sources are attached to the mixer.
    _project: Player,
    preview: Player,
    preview_requested: AtomicBool,
}

impl PlaybackBuses {
    fn connect(mixer: &Mixer, project: ProjectAudio) -> (TransportHandle, Self) {
        Self::connect_renderer(mixer, PcmRenderer::new(project))
    }

    fn connect_renderer<R: ProjectRenderer>(mixer: &Mixer, renderer: R) -> (TransportHandle, Self) {
        let (transport, source) = TransportSource::new(renderer);
        let project_player = Player::connect_new(mixer);
        project_player.append(source);

        let preview = Player::connect_new(mixer);
        (
            transport,
            Self {
                _project: project_player,
                preview,
                preview_requested: AtomicBool::new(false),
            },
        )
    }

    fn audition(&self, clip: AuditionClip) {
        self.preview_requested.store(false, Ordering::Release);
        self.preview.stop();
        self.preview.append(AuditionSource::new(clip.audio));
        self.preview.play();
        self.preview_requested.store(true, Ordering::Release);
    }

    fn stop_preview(&self) {
        self.preview_requested.store(false, Ordering::Release);
        self.preview.stop();
    }

    fn preview_active(&self) -> bool {
        self.preview_requested.load(Ordering::Acquire) && !self.preview.empty()
    }
}

/// A zero-copy, finite Rodio view over immutable audition PCM.
struct AuditionSource {
    audio: ProjectAudio,
    sample_index: usize,
}

impl AuditionSource {
    fn new(audio: ProjectAudio) -> Self {
        Self {
            audio,
            sample_index: 0,
        }
    }
}

impl Iterator for AuditionSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        let sample = self.audio.interleaved().get(self.sample_index).copied();
        if sample.is_some() {
            self.sample_index += 1;
        }
        sample
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self
            .audio
            .interleaved()
            .len()
            .saturating_sub(self.sample_index);
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for AuditionSource {}

impl Source for AuditionSource {
    fn current_span_len(&self) -> Option<usize> {
        Some(self.len())
    }

    fn channels(&self) -> rodio::ChannelCount {
        self.audio.format().channels
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        self.audio.format().sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        let frames = self.audio.frame_count().0;
        let rate = u64::from(self.audio.format().sample_rate.get());
        let whole_seconds = frames / rate;
        let fractional_frames = frames % rate;
        let nanos = fractional_frames * 1_000_000_000 / rate;
        Some(Duration::new(whole_seconds, nanos as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::TransportMode;
    use std::num::NonZero;

    fn stereo_project(frames: &[[f32; 2]]) -> ProjectAudio {
        let samples = frames.iter().flatten().copied().collect();
        ProjectAudio::from_interleaved(AudioFormat::new(48_000, 2).unwrap(), samples).unwrap()
    }

    #[test]
    fn audition_clip_validates_channel_shape_without_copying_shared_pcm() {
        let shared: Arc<[f32]> = vec![1.0, 2.0, 3.0, 4.0].into();
        let clip =
            AuditionClip::from_shared(AudioFormat::new(48_000, 2).unwrap(), Arc::clone(&shared))
                .unwrap();
        assert_eq!(clip.frame_count(), ProjectFrame(2));
        assert_eq!(clip.interleaved().as_ptr(), shared.as_ptr());

        assert!(AuditionClip::stereo(48_000, vec![1.0, 2.0, 3.0]).is_err());
        let surround =
            ProjectAudio::from_interleaved(AudioFormat::new(48_000, 4).unwrap(), vec![0.0; 4])
                .unwrap();
        assert!(matches!(
            AuditionClip::from_project_audio(surround),
            Err(AudioHostError::UnsupportedAuditionChannels(4))
        ));
    }

    #[test]
    fn audition_source_is_finite_exact_and_preserves_stereo_order() {
        let clip = AuditionClip::stereo(4, vec![1.0, 10.0, 2.0, 20.0, 3.0, 30.0]).unwrap();
        let mut source = AuditionSource::new(clip.audio);
        assert_eq!(source.channels().get(), 2);
        assert_eq!(source.sample_rate().get(), 4);
        assert_eq!(source.total_duration(), Some(Duration::from_millis(750)));
        assert_eq!(source.size_hint(), (6, Some(6)));
        assert_eq!(
            source.by_ref().collect::<Vec<_>>(),
            [1.0, 10.0, 2.0, 20.0, 3.0, 30.0]
        );
        assert_eq!(source.current_span_len(), Some(0));
        assert_eq!(source.next(), None);
    }

    #[test]
    fn hardware_free_mixer_keeps_preview_commands_independent_of_transport() {
        let channels = NonZero::new(2).unwrap();
        let sample_rate = NonZero::new(48_000).unwrap();
        let (mixer, mut output) = rodio::mixer::mixer(channels, sample_rate);
        let (transport, buses) = PlaybackBuses::connect(
            &mixer,
            stereo_project(&[[1.0, 10.0], [2.0, 20.0], [3.0, 30.0]]),
        );

        transport.play();
        for _ in 0..4 {
            let _ = output.next();
        }
        let before = transport.snapshot();
        assert_eq!(before.mode, TransportMode::Playing);
        assert!(before.frame > ProjectFrame(0));

        buses.audition(AuditionClip::stereo(48_000, vec![100.0, 1_000.0]).unwrap());
        let after_audition_command = transport.snapshot();
        assert_eq!(after_audition_command.mode, before.mode);
        assert_eq!(after_audition_command.frame, before.frame);

        // Rodio's uniform adapter has a short interpolation startup, so avoid
        // asserting a particular output sample. The preview impulse must still
        // appear on the mixer while project transport continues to advance.
        let mixed: Vec<_> = (0..1_024).filter_map(|_| output.next()).collect();
        assert!(
            mixed.iter().any(|sample| sample.abs() > 50.0),
            "preview impulse missing from mixer output: {mixed:?}"
        );
        assert!(transport.snapshot().frame >= before.frame);

        let before_stop = transport.snapshot();
        buses.stop_preview();
        let after_stop = transport.snapshot();
        assert_eq!(after_stop.mode, before_stop.mode);
        assert_eq!(after_stop.frame, before_stop.frame);
    }
}
