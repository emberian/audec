//! Explicit source and derived-media resolution for an opened project.
//!
//! A project package stores route *intent*, not an assertion that its media is
//! present.  This module turns that intent into decoder requests and detailed
//! repair diagnostics.  It never mutates `AssetRegistry`: a UI/controller must
//! explicitly accept a [`RelinkProposal`] before calling `AssetRegistry::relink`.

use std::fmt;
use std::fs::File;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rubato::audioadapter_buffers::owned::InterleavedOwned;
use rubato::{Async, FixedAsync, Resampler, SincInterpolationParameters};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CodecType, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::assets::{
    AssetFrameRange, AssetId, AssetLocation, ContentFingerprint, DecodedAudioMetadata, SampleFrames,
};
use crate::audio::AudioFormat;
use crate::daw_render::PcmAsset;
use crate::project_io::AssetPathIntent;

/// Direct dependency versions are part of every decode/conversion recipe.
/// Symphonia is MPL-2.0; Rubato is MIT OR Apache-2.0.
pub const SYMPHONIA_DECODER_VERSION: &str = "0.5.5";
pub const RUBATO_CONVERTER_VERSION: &str = "5.0.0";

const DEFAULT_MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DEFAULT_MAX_DECODED_SAMPLES: u64 = 2_000_000_000;
const DEFAULT_RESAMPLE_CHUNK_FRAMES: usize = 1_024;

/// Whether a decoder request is for an original registered file or a material
/// derived from an exact source span.  Derived material stays first-class in
/// the resolver even when its PCM is cached/generated locally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MaterialKind {
    Source,
    Derived {
        source_asset: AssetId,
        source_range: AssetFrameRange,
        derivation: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterialRequest {
    pub asset: AssetId,
    pub name: String,
    pub path: AssetPathIntent,
    pub expected_metadata: DecodedAudioMetadata,
    pub expected_fingerprint: ContentFingerprint,
    pub kind: MaterialKind,
}

impl MaterialRequest {
    pub fn source(
        asset: AssetId,
        name: impl Into<String>,
        path: AssetPathIntent,
        expected_metadata: DecodedAudioMetadata,
        expected_fingerprint: ContentFingerprint,
    ) -> Self {
        Self {
            asset,
            name: name.into(),
            path,
            expected_metadata,
            expected_fingerprint,
            kind: MaterialKind::Source,
        }
    }

    pub fn derived(
        mut source: Self,
        source_asset: AssetId,
        source_range: AssetFrameRange,
        derivation: impl Into<String>,
    ) -> Self {
        source.kind = MaterialKind::Derived {
            source_asset,
            source_range,
            derivation: derivation.into(),
        };
        source
    }

    pub fn candidates(&self, project_manifest: &Path) -> Vec<PathBuf> {
        self.path.candidates(project_manifest)
    }
}

/// A successful decoder result.  The decoder is responsible for producing a
/// fingerprint over the same source bytes that supplied the PCM.
#[derive(Clone, Debug)]
pub struct DecodedMaterial {
    pub path: PathBuf,
    pub metadata: DecodedAudioMetadata,
    pub fingerprint: ContentFingerprint,
    pub pcm: PcmAsset,
}

/// Audec-owned account of how encoded source bytes became canonical PCM.
/// Library identifiers are descriptive recipe facts, never project identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaDecodeProvenance {
    pub backend: &'static str,
    pub backend_version: &'static str,
    pub source_bytes: u64,
    pub stream_count: u32,
    pub selected_track_id: u32,
    pub container: Option<String>,
    pub codec: String,
    pub declared_frames: Option<u64>,
    pub gapless: bool,
    pub verification: DecodeVerification,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeVerification {
    /// The codec supplied and passed an integrity check.
    Passed,
    /// The codec does not expose a whole-stream integrity check.
    Unavailable,
}

/// Extended result available from the production decoder. The small
/// `MediaDecoder` compatibility seam still returns `decoded`; callers that
/// publish derived PCM use this form so the decoder recipe stays attached.
#[derive(Clone, Debug)]
pub struct ProvenancedDecodedMaterial {
    pub decoded: DecodedMaterial,
    pub provenance: MediaDecodeProvenance,
}

/// A project-rate view retains the complete source identity that was decoded.
/// The converted PCM is derived material and never overwrites source metadata.
#[derive(Clone, Debug)]
pub struct ProjectRateMaterial {
    pub source_path: PathBuf,
    pub source_metadata: DecodedAudioMetadata,
    pub source_fingerprint: ContentFingerprint,
    pub decode_provenance: MediaDecodeProvenance,
    pub pcm: PcmAsset,
    pub conversion: Option<SampleRateConversionProvenance>,
}

impl ProvenancedDecodedMaterial {
    /// Materialize PCM at an explicitly selected project rate. Equal rates
    /// share the original allocation and do not invent a conversion recipe.
    pub fn pcm_for_project_rate(
        &self,
        project_sample_rate_hz: u32,
        converter: &impl SampleRateConverter,
    ) -> Result<ProjectRateMaterial, SampleRateConversionError> {
        if project_sample_rate_hz == 0 {
            return Err(SampleRateConversionError::ZeroOutputRate);
        }
        let (pcm, conversion) =
            if self.decoded.pcm.format.sample_rate.get() == project_sample_rate_hz {
                (self.decoded.pcm.clone(), None)
            } else {
                let converted = converter.convert(&self.decoded.pcm, project_sample_rate_hz)?;
                (converted.pcm, Some(converted.provenance))
            };
        Ok(ProjectRateMaterial {
            source_path: self.decoded.path.clone(),
            source_metadata: self.decoded.metadata.clone(),
            source_fingerprint: self.decoded.fingerprint,
            decode_provenance: self.provenance.clone(),
            pcm,
            conversion,
        })
    }
}

/// Deliberately small async-agnostic seam.  The application can invoke this
/// on a worker, and test decoders can remain deterministic without a window or
/// a particular file-format dependency.
pub trait MediaDecoder {
    fn decode(&self, path: &Path) -> Result<DecodedMaterial, MediaDecodeError>;
}

/// Canonical decoder for Audec's common import formats. The complete encoded
/// source is read once into a bounded immutable snapshot so its fingerprint
/// describes the exact bytes supplied to Symphonia.
#[derive(Clone, Debug)]
pub struct SymphoniaMediaDecoder {
    maximum_source_bytes: u64,
    maximum_decoded_samples: u64,
}

impl SymphoniaMediaDecoder {
    pub fn new(
        maximum_source_bytes: u64,
        maximum_decoded_samples: u64,
    ) -> Result<Self, MediaDecodeError> {
        if maximum_source_bytes == 0 || maximum_decoded_samples == 0 {
            return Err(MediaDecodeError::InvalidOutput(
                "decoder byte and sample limits must be non-zero".into(),
            ));
        }
        Ok(Self {
            maximum_source_bytes,
            maximum_decoded_samples,
        })
    }

    pub const fn maximum_source_bytes(&self) -> u64 {
        self.maximum_source_bytes
    }

    pub const fn maximum_decoded_samples(&self) -> u64 {
        self.maximum_decoded_samples
    }
}

impl Default for SymphoniaMediaDecoder {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_SOURCE_BYTES, DEFAULT_MAX_DECODED_SAMPLES)
            .expect("default media decode limits are non-zero")
    }
}

impl SymphoniaMediaDecoder {
    pub fn decode_provenanced(
        &self,
        path: &Path,
    ) -> Result<ProvenancedDecodedMaterial, MediaDecodeError> {
        let source = read_source_snapshot(path, self.maximum_source_bytes)?;
        let fingerprint = ContentFingerprint::from_bytes(&source);
        let source_bytes = source.len() as u64;
        let container = identify_container(&source).map(str::to_owned);

        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
            hint.with_extension(extension);
        }
        let stream = MediaSourceStream::new(Box::new(Cursor::new(source)), Default::default());
        let format_options = FormatOptions {
            enable_gapless: true,
            ..FormatOptions::default()
        };
        let probe = symphonia::default::get_probe()
            .format(&hint, stream, &format_options, &MetadataOptions::default())
            .map_err(|error| map_symphonia_error("container probe", error))?;
        let mut format = probe.format;
        let stream_count = u32::try_from(format.tracks().len()).unwrap_or(u32::MAX);
        let track = format.default_track().ok_or_else(|| {
            MediaDecodeError::UnsupportedFormat(
                "container probe found no default audio stream".into(),
            )
        })?;
        let track_id = track.id;
        let codec_type = track.codec_params.codec;
        let codec = codec_name(codec_type);
        let declared_frames = track.codec_params.n_frames;
        let declared_sample_rate = track.codec_params.sample_rate;
        let declared_channels = track.codec_params.channels.map(|channels| channels.count());
        let bit_depth = track
            .codec_params
            .bits_per_sample
            .or(track.codec_params.bits_per_coded_sample)
            .and_then(|value| u16::try_from(value).ok());
        let mut decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions { verify: true })
            .map_err(|error| {
                map_symphonia_error(&format!("codec initialization ({codec})"), error)
            })?;

        let mut samples = Vec::new();
        let mut decoded_format: Option<(u32, u16)> = None;
        loop {
            let packet = match format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(error))
                    if error.kind() == io::ErrorKind::UnexpectedEof =>
                {
                    break;
                }
                Err(error) => return Err(map_symphonia_error("packet stream", error)),
            };
            if packet.track_id() != track_id {
                continue;
            }
            let packet_timestamp = packet.ts;
            let decoded = decoder.decode(&packet).map_err(|error| {
                map_symphonia_error(
                    &format!(
                        "codec decode ({codec}, track {track_id}, timestamp {packet_timestamp})"
                    ),
                    error,
                )
            })?;
            let sample_rate_hz = decoded.spec().rate;
            let channel_count = decoded.spec().channels.count();
            let channels = u16::try_from(channel_count).map_err(|_| {
                MediaDecodeError::InvalidOutput(format!(
                    "codec {codec} produced {channel_count} channels"
                ))
            })?;
            if sample_rate_hz == 0 || channels == 0 {
                return Err(MediaDecodeError::InvalidOutput(format!(
                    "codec {codec} produced a zero sample rate or channel count"
                )));
            }
            if let Some(expected) = decoded_format {
                if expected != (sample_rate_hz, channels) {
                    return Err(MediaDecodeError::Corrupt(format!(
                        "codec stream changed format from {} Hz/{} channels to {sample_rate_hz} Hz/{channels} channels",
                        expected.0, expected.1
                    )));
                }
            } else {
                decoded_format = Some((sample_rate_hz, channels));
            }

            let packet_samples = decoded.frames().checked_mul(channel_count).ok_or_else(|| {
                MediaDecodeError::LimitExceeded("decoded packet sample count overflowed".into())
            })?;
            let new_sample_count = samples.len().checked_add(packet_samples).ok_or_else(|| {
                MediaDecodeError::LimitExceeded("decoded sample count overflowed".into())
            })?;
            let new_sample_count_u64 = u64::try_from(new_sample_count).map_err(|_| {
                MediaDecodeError::LimitExceeded(
                    "decoded sample count cannot be represented by the configured limit".into(),
                )
            })?;
            if new_sample_count_u64 > self.maximum_decoded_samples {
                return Err(MediaDecodeError::LimitExceeded(format!(
                    "decoded stream exceeds the configured {}-sample limit",
                    self.maximum_decoded_samples
                )));
            }
            let mut converted =
                SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
            converted.copy_interleaved_ref(decoded);
            if converted.samples().len() != packet_samples {
                return Err(MediaDecodeError::InvalidOutput(format!(
                    "codec {codec} reported {packet_samples} samples but converted {}",
                    converted.samples().len()
                )));
            }
            if let Some(index) = converted
                .samples()
                .iter()
                .position(|sample| !sample.is_finite())
            {
                return Err(MediaDecodeError::InvalidOutput(format!(
                    "codec {codec} produced a non-finite sample at packet offset {index}"
                )));
            }
            samples.extend_from_slice(converted.samples());
        }

        let (sample_rate_hz, channels) = decoded_format.ok_or_else(|| {
            MediaDecodeError::Corrupt(format!(
                "codec {codec} produced no audio frames for track {track_id}"
            ))
        })?;
        if samples.is_empty() {
            return Err(MediaDecodeError::Corrupt(format!(
                "codec {codec} produced an empty audio stream"
            )));
        }
        if let Some(rate) = declared_sample_rate {
            if rate != sample_rate_hz {
                return Err(MediaDecodeError::Corrupt(format!(
                    "codec metadata declares {rate} Hz but decoded PCM is {sample_rate_hz} Hz"
                )));
            }
        }
        if let Some(count) = declared_channels {
            if count != usize::from(channels) {
                return Err(MediaDecodeError::Corrupt(format!(
                    "codec metadata declares {count} channels but decoded PCM has {channels}"
                )));
            }
        }

        let verification = match decoder.finalize().verify_ok {
            Some(true) => DecodeVerification::Passed,
            Some(false) => {
                return Err(MediaDecodeError::Corrupt(format!(
                    "codec integrity verification failed for {codec} track {track_id}"
                )))
            }
            None => DecodeVerification::Unavailable,
        };
        let frame_count = samples.len() / usize::from(channels);
        let frame_count = u64::try_from(frame_count).map_err(|_| {
            MediaDecodeError::LimitExceeded("decoded frame count does not fit u64".into())
        })?;
        let audio_format = AudioFormat::new(sample_rate_hz, channels)
            .map_err(|error| MediaDecodeError::InvalidOutput(error.to_string()))?;
        let pcm = PcmAsset::new(audio_format, Arc::from(samples))
            .map_err(|error| MediaDecodeError::InvalidOutput(error.to_string()))?;
        let metadata = DecodedAudioMetadata {
            sample_rate_hz,
            channels,
            frame_count: SampleFrames(frame_count),
            container: container.clone(),
            codec: Some(codec.clone()),
            bit_depth,
        };
        metadata
            .validate()
            .map_err(|error| MediaDecodeError::InvalidOutput(error.to_string()))?;

        Ok(ProvenancedDecodedMaterial {
            decoded: DecodedMaterial {
                path: path.to_path_buf(),
                metadata,
                fingerprint,
                pcm,
            },
            provenance: MediaDecodeProvenance {
                backend: "symphonia",
                backend_version: SYMPHONIA_DECODER_VERSION,
                source_bytes,
                stream_count,
                selected_track_id: track_id,
                container,
                codec,
                declared_frames,
                gapless: true,
                verification,
            },
        })
    }
}

impl MediaDecoder for SymphoniaMediaDecoder {
    fn decode(&self, path: &Path) -> Result<DecodedMaterial, MediaDecodeError> {
        self.decode_provenanced(path).map(|decoded| decoded.decoded)
    }
}

fn read_source_snapshot(path: &Path, maximum_bytes: u64) -> Result<Vec<u8>, MediaDecodeError> {
    let file = File::open(path)
        .map_err(|error| MediaDecodeError::Io(format!("opening {}: {error}", path.display())))?;
    let declared_bytes = file.metadata().ok().map(|metadata| metadata.len());
    if declared_bytes.is_some_and(|bytes| bytes > maximum_bytes) {
        return Err(MediaDecodeError::LimitExceeded(format!(
            "{} exceeds the configured {maximum_bytes}-byte source limit",
            path.display()
        )));
    }
    let capacity = declared_bytes
        .unwrap_or(0)
        .min(maximum_bytes)
        .min(usize::MAX as u64) as usize;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(maximum_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| MediaDecodeError::Io(format!("reading {}: {error}", path.display())))?;
    if bytes.len() as u64 > maximum_bytes {
        return Err(MediaDecodeError::LimitExceeded(format!(
            "{} exceeds the configured {maximum_bytes}-byte source limit",
            path.display()
        )));
    }
    if bytes.is_empty() {
        return Err(MediaDecodeError::Corrupt("source file is empty".into()));
    }
    Ok(bytes)
}

fn identify_container(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"fLaC") {
        Some("flac")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        Some("wav")
    } else if bytes.starts_with(b"OggS") {
        Some("ogg")
    } else if bytes.starts_with(b"ID3")
        || bytes
            .windows(2)
            .take(4_096)
            .any(|window| window[0] == 0xff && window[1] & 0xe0 == 0xe0)
    {
        Some("mp3")
    } else {
        None
    }
}

fn codec_name(codec: CodecType) -> String {
    symphonia::default::get_codecs()
        .get_codec(codec)
        .map(|descriptor| descriptor.short_name.to_owned())
        .unwrap_or_else(|| format!("unknown-{codec}"))
}

fn map_symphonia_error(stage: &str, error: SymphoniaError) -> MediaDecodeError {
    match error {
        SymphoniaError::Unsupported(message) => {
            MediaDecodeError::UnsupportedFormat(format!("{stage}: {message}"))
        }
        SymphoniaError::LimitError(message) => {
            MediaDecodeError::LimitExceeded(format!("{stage}: {message}"))
        }
        SymphoniaError::IoError(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
            MediaDecodeError::Corrupt(format!("{stage}: unexpected end of stream"))
        }
        SymphoniaError::IoError(error) => MediaDecodeError::Io(format!("{stage}: {error}")),
        SymphoniaError::DecodeError(message) => {
            MediaDecodeError::Corrupt(format!("{stage}: {message}"))
        }
        SymphoniaError::SeekError(error) => {
            MediaDecodeError::Corrupt(format!("{stage}: {error:?}"))
        }
        SymphoniaError::ResetRequired => {
            MediaDecodeError::Corrupt(format!("{stage}: stream requires an unsupported reset"))
        }
    }
}

/// Explicit, async-agnostic conversion seam. Implementations convert sample
/// rate only; channel layout and ordering must remain unchanged.
pub trait SampleRateConverter {
    fn convert(
        &self,
        source: &PcmAsset,
        output_sample_rate_hz: u32,
    ) -> Result<ConvertedPcm, SampleRateConversionError>;
}

#[derive(Clone, Debug)]
pub struct ConvertedPcm {
    pub pcm: PcmAsset,
    pub provenance: SampleRateConversionProvenance,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleRateConversionProvenance {
    pub backend: &'static str,
    pub backend_version: &'static str,
    pub algorithm: &'static str,
    pub input_sample_rate_hz: u32,
    pub output_sample_rate_hz: u32,
    pub channels: u16,
    pub input_frames: u64,
    pub output_frames: u64,
    pub chunk_frames: usize,
    pub sinc_length: usize,
    pub oversampling_factor: usize,
}

#[derive(Clone, Debug)]
pub struct RubatoSampleRateConverter {
    chunk_frames: usize,
    parameters: SincInterpolationParameters,
}

impl RubatoSampleRateConverter {
    pub fn new(chunk_frames: usize) -> Result<Self, SampleRateConversionError> {
        if chunk_frames == 0 {
            return Err(SampleRateConversionError::ZeroChunkFrames);
        }
        Ok(Self {
            chunk_frames,
            parameters: SincInterpolationParameters::default(),
        })
    }
}

impl Default for RubatoSampleRateConverter {
    fn default() -> Self {
        Self::new(DEFAULT_RESAMPLE_CHUNK_FRAMES).expect("the default Rubato chunk size is non-zero")
    }
}

/// Number of destination frames needed to cover the source's exact rational
/// duration. Keep this calculation independent of Rubato's floating-point
/// ratio: an exactly integral conversion (for example 960 frames at 48 kHz to
/// 24 kHz) must not gain an endpoint frame because the backend rounded its
/// internal ratio slightly upward.
fn exact_resampled_frame_count(
    input_frames: u64,
    input_sample_rate_hz: u32,
    output_sample_rate_hz: u32,
) -> Result<usize, SampleRateConversionError> {
    let numerator = u128::from(input_frames) * u128::from(output_sample_rate_hz);
    let denominator = u128::from(input_sample_rate_hz);
    let frames = (numerator + denominator - 1) / denominator;
    usize::try_from(frames).map_err(|_| SampleRateConversionError::OutputTooLarge)
}

impl SampleRateConverter for RubatoSampleRateConverter {
    fn convert(
        &self,
        source: &PcmAsset,
        output_sample_rate_hz: u32,
    ) -> Result<ConvertedPcm, SampleRateConversionError> {
        if output_sample_rate_hz == 0 {
            return Err(SampleRateConversionError::ZeroOutputRate);
        }
        let input_sample_rate_hz = source.format.sample_rate.get();
        if output_sample_rate_hz == input_sample_rate_hz {
            return Err(SampleRateConversionError::RatesEqual(input_sample_rate_hz));
        }
        if source.frame_count() == 0 {
            return Err(SampleRateConversionError::EmptyInput);
        }
        if let Some(index) = source.samples.iter().position(|sample| !sample.is_finite()) {
            return Err(SampleRateConversionError::NonFiniteInput { index });
        }

        let channels = usize::from(source.format.channels.get());
        let input_frames = usize::try_from(source.frame_count())
            .map_err(|_| SampleRateConversionError::OutputTooLarge)?;
        let input = InterleavedOwned::new_from(source.samples.to_vec(), channels, input_frames)
            .map_err(|error| SampleRateConversionError::Adapter(error.to_string()))?;
        let ratio = f64::from(output_sample_rate_hz) / f64::from(input_sample_rate_hz);
        let mut resampler = Async::<f32>::new_sinc(
            ratio,
            1.0,
            &self.parameters,
            self.chunk_frames,
            channels,
            FixedAsync::Input,
        )
        .map_err(|error| SampleRateConversionError::Construction(error.to_string()))?;
        let output = resampler
            .process_all(&input, input_frames, None)
            .map_err(|error| SampleRateConversionError::Processing(error.to_string()))?;
        let mut samples = output.take_data();
        if samples.len() % channels != 0 {
            return Err(SampleRateConversionError::InvalidOutput(format!(
                "Rubato returned {} samples that do not form complete {channels}-channel frames",
                samples.len()
            )));
        }
        let produced_frames = samples.len() / channels;
        let output_frames = exact_resampled_frame_count(
            source.frame_count(),
            input_sample_rate_hz,
            output_sample_rate_hz,
        )?;
        if produced_frames < output_frames {
            return Err(SampleRateConversionError::InvalidOutput(format!(
                "Rubato returned {produced_frames} frames, fewer than the exact-duration requirement of {output_frames}"
            )));
        }
        let output_samples = output_frames
            .checked_mul(channels)
            .ok_or(SampleRateConversionError::OutputTooLarge)?;
        // `process_all` already removes the sinc startup delay and filter
        // padding. Its remaining length is calculated from a floating-point
        // ratio, though, so it can include one surplus endpoint frame when an
        // exact rational duration is integral. The project-rate contract owns
        // the duration and deterministically removes that numerical surplus.
        samples.truncate(output_samples);
        if let Some(index) = samples.iter().position(|sample| !sample.is_finite()) {
            return Err(SampleRateConversionError::NonFiniteOutput { index });
        }
        let format = AudioFormat::new(output_sample_rate_hz, source.format.channels.get())
            .map_err(|error| SampleRateConversionError::InvalidOutput(error.to_string()))?;
        let pcm = PcmAsset::new(format, Arc::from(samples))
            .map_err(|error| SampleRateConversionError::InvalidOutput(error.to_string()))?;
        if pcm.frame_count() != output_frames as u64 {
            return Err(SampleRateConversionError::InvalidOutput(
                "Rubato frame count disagrees with its output buffer".into(),
            ));
        }

        Ok(ConvertedPcm {
            provenance: SampleRateConversionProvenance {
                backend: "rubato",
                backend_version: RUBATO_CONVERTER_VERSION,
                algorithm: "asynchronous-windowed-sinc",
                input_sample_rate_hz,
                output_sample_rate_hz,
                channels: source.format.channels.get(),
                input_frames: source.frame_count(),
                output_frames: pcm.frame_count(),
                chunk_frames: self.chunk_frames,
                sinc_length: self.parameters.sinc_len,
                oversampling_factor: self.parameters.oversampling_factor,
            },
            pcm,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SampleRateConversionError {
    ZeroOutputRate,
    ZeroChunkFrames,
    RatesEqual(u32),
    EmptyInput,
    NonFiniteInput { index: usize },
    NonFiniteOutput { index: usize },
    OutputTooLarge,
    Adapter(String),
    Construction(String),
    Processing(String),
    InvalidOutput(String),
}

impl fmt::Display for SampleRateConversionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroOutputRate => write!(formatter, "output sample rate must not be zero"),
            Self::ZeroChunkFrames => write!(formatter, "resampler chunk size must not be zero"),
            Self::RatesEqual(rate) => write!(
                formatter,
                "explicit sample-rate conversion requires different rates; both are {rate} Hz"
            ),
            Self::EmptyInput => write!(formatter, "cannot resample empty PCM"),
            Self::NonFiniteInput { index } => {
                write!(formatter, "input PCM sample {index} is not finite")
            }
            Self::NonFiniteOutput { index } => {
                write!(formatter, "resampled PCM sample {index} is not finite")
            }
            Self::OutputTooLarge => {
                write!(formatter, "resampled PCM is too large for this process")
            }
            Self::Adapter(message) => write!(formatter, "invalid resampler buffer: {message}"),
            Self::Construction(message) => {
                write!(formatter, "resampler construction failed: {message}")
            }
            Self::Processing(message) => write!(formatter, "resampling failed: {message}"),
            Self::InvalidOutput(message) => {
                write!(formatter, "resampler produced invalid PCM: {message}")
            }
        }
    }
}

impl std::error::Error for SampleRateConversionError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelinkProposal {
    pub asset: AssetId,
    pub new_path: PathBuf,
    pub exact_fingerprint: bool,
    pub exact_metadata: bool,
    /// This is a presentation/confirmation requirement, not permission for a
    /// resolver to mutate the registry.
    pub requires_user_confirmation: bool,
}

#[derive(Clone, Debug)]
pub struct ResolvedMaterial {
    pub request: MaterialRequest,
    pub decoded: DecodedMaterial,
    pub relink: Option<RelinkProposal>,
    pub diagnostics: Vec<ResolutionDiagnostic>,
}

#[derive(Clone, Debug)]
pub struct UnresolvedMaterial {
    pub request: MaterialRequest,
    pub diagnostics: Vec<ResolutionDiagnostic>,
    /// Candidates that decoded but did not establish the registered identity.
    /// They remain visible repair leads, never an implicit relink.
    pub repair_candidates: Vec<RelinkProposal>,
}

#[derive(Clone, Debug)]
pub enum MaterialResolution {
    Resolved(ResolvedMaterial),
    Unresolved(UnresolvedMaterial),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolutionDiagnostic {
    pub path: Option<PathBuf>,
    pub code: &'static str,
    pub message: String,
}

/// Resolve one asset in deterministic candidate order: project-relative route
/// first, original absolute route second.  The only successful result has PCM
/// whose immutable metadata and fingerprint agree with the saved asset facts.
pub fn resolve_material(
    decoder: &impl MediaDecoder,
    project_manifest: &Path,
    request: MaterialRequest,
) -> MaterialResolution {
    let mut diagnostics = Vec::new();
    let mut repair_candidates = Vec::new();
    for path in request.candidates(project_manifest) {
        if !path.is_file() {
            diagnostics.push(ResolutionDiagnostic {
                path: Some(path),
                code: "route-missing",
                message: "the saved media route is not a file".into(),
            });
            continue;
        }
        let decoded = match decoder.decode(&path) {
            Ok(decoded) => decoded,
            Err(error) => {
                diagnostics.push(ResolutionDiagnostic {
                    path: Some(path),
                    code: "decode-failed",
                    message: error.to_string(),
                });
                continue;
            }
        };
        let metadata_matches = decoded.metadata == request.expected_metadata;
        let fingerprint_matches = decoded.fingerprint == request.expected_fingerprint;
        let pcm_matches = pcm_matches_metadata(&decoded.pcm, &decoded.metadata);
        if !pcm_matches {
            diagnostics.push(ResolutionDiagnostic {
                path: Some(decoded.path.clone()),
                code: "decoder-metadata-mismatch",
                message: "decoder PCM disagrees with its decoded metadata".into(),
            });
            continue;
        }
        let proposal = RelinkProposal {
            asset: request.asset,
            new_path: decoded.path.clone(),
            exact_fingerprint: fingerprint_matches,
            exact_metadata: metadata_matches,
            requires_user_confirmation: true,
        };
        if metadata_matches && fingerprint_matches {
            let relink = (!request
                .candidates(project_manifest)
                .iter()
                .any(|candidate| candidate == &decoded.path))
            .then_some(proposal);
            return MaterialResolution::Resolved(ResolvedMaterial {
                request,
                decoded,
                relink,
                diagnostics,
            });
        }
        let reason = match (fingerprint_matches, metadata_matches) {
            (false, false) => "content fingerprint and decoded metadata differ",
            (false, true) => "content fingerprint differs despite matching decoded metadata",
            (true, false) => "decoded metadata differs despite matching content fingerprint",
            (true, true) => unreachable!("the matching case returned above"),
        };
        diagnostics.push(ResolutionDiagnostic {
            path: Some(decoded.path.clone()),
            code: "candidate-identity-mismatch",
            message: reason.into(),
        });
        repair_candidates.push(proposal);
    }
    MaterialResolution::Unresolved(UnresolvedMaterial {
        request,
        diagnostics,
        repair_candidates,
    })
}

fn pcm_matches_metadata(pcm: &PcmAsset, metadata: &DecodedAudioMetadata) -> bool {
    pcm.format.sample_rate.get() == metadata.sample_rate_hz
        && pcm.format.channels.get() == metadata.channels
        && pcm.frame_count() == metadata.frame_count.0
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaDecodeError {
    UnsupportedFormat(String),
    Corrupt(String),
    Io(String),
    LimitExceeded(String),
    InvalidOutput(String),
}

impl fmt::Display for MediaDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFormat(message) => write!(formatter, "unsupported media: {message}"),
            Self::Corrupt(message) => write!(formatter, "corrupt media: {message}"),
            Self::Io(message) => write!(formatter, "media I/O failed: {message}"),
            Self::LimitExceeded(message) => write!(formatter, "media limit exceeded: {message}"),
            Self::InvalidOutput(message) => {
                write!(formatter, "decoder produced invalid PCM: {message}")
            }
        }
    }
}

impl std::error::Error for MediaDecodeError {}

/// Convert an explicit accepted repair into an `AssetLocation` input suitable
/// for `AssetRegistry::relink`.  Relative-route policy stays in the document
/// controller because it knows the package location; this helper refuses a
/// non-absolute proposed path rather than inventing one.
pub fn accepted_relink_location(
    proposal: &RelinkProposal,
) -> Result<AssetLocation, MediaDecodeError> {
    let absolute =
        crate::assets::AbsolutePath::parse(proposal.new_path.to_string_lossy().into_owned())
            .map_err(|error| MediaDecodeError::InvalidOutput(error.to_string()))?;
    AssetLocation::new(Some(absolute), None)
        .map_err(|error| MediaDecodeError::InvalidOutput(error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    fn temp_path(extension: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "audec-media-resolver-{}-{}.{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed),
            extension
        ))
    }

    fn pcm16_wav(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        assert_eq!(samples.len() % usize::from(channels), 0);
        let data_bytes = u32::try_from(samples.len() * 2).unwrap();
        let block_align = channels * 2;
        let byte_rate = sample_rate * u32::from(block_align);
        let mut bytes = Vec::with_capacity(44 + data_bytes as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&byte_rate.to_le_bytes());
        bytes.extend_from_slice(&block_align.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_bytes.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn symphonia_wav_decode_retains_source_identity_and_recipe() {
        let path = temp_path("wav");
        let encoded = pcm16_wav(48_000, 2, &[i16::MIN, 0, i16::MAX, -16_384]);
        fs::write(&path, &encoded).unwrap();

        let decoded = SymphoniaMediaDecoder::default()
            .decode_provenanced(&path)
            .unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(decoded.decoded.path, path);
        assert_eq!(
            decoded.decoded.fingerprint,
            ContentFingerprint::from_bytes(&encoded)
        );
        assert_eq!(decoded.decoded.metadata.sample_rate_hz, 48_000);
        assert_eq!(decoded.decoded.metadata.channels, 2);
        assert_eq!(decoded.decoded.metadata.frame_count, SampleFrames(2));
        assert_eq!(decoded.decoded.metadata.container.as_deref(), Some("wav"));
        assert_eq!(decoded.decoded.metadata.codec.as_deref(), Some("pcm_s16le"));
        assert_eq!(decoded.decoded.metadata.bit_depth, Some(16));
        assert_eq!(decoded.decoded.pcm.frame_count(), 2);
        assert_eq!(decoded.decoded.pcm.samples[0], -1.0);
        assert_eq!(decoded.decoded.pcm.samples[1], 0.0);
        assert!((decoded.decoded.pcm.samples[2] - 32_767.0 / 32_768.0).abs() < 1.0e-6);
        assert_eq!(decoded.decoded.pcm.samples[3], -0.5);
        assert_eq!(decoded.provenance.backend, "symphonia");
        assert_eq!(
            decoded.provenance.backend_version,
            SYMPHONIA_DECODER_VERSION
        );
        assert_eq!(decoded.provenance.source_bytes, encoded.len() as u64);
        assert_eq!(decoded.provenance.stream_count, 1);
        assert_eq!(decoded.provenance.container.as_deref(), Some("wav"));
        assert_eq!(decoded.provenance.codec, "pcm_s16le");
        assert!(decoded.provenance.gapless);
    }

    #[test]
    fn symphonia_decoder_refuses_source_limit_and_truncated_stream() {
        let path = temp_path("wav");
        let encoded = pcm16_wav(8_000, 1, &[0, 1, 2, 3]);
        fs::write(&path, &encoded).unwrap();
        let limited = SymphoniaMediaDecoder::new(16, 1_000).unwrap();
        assert!(matches!(
            limited.decode(&path),
            Err(MediaDecodeError::LimitExceeded(_))
        ));

        fs::write(&path, &encoded[..42]).unwrap();
        let error = SymphoniaMediaDecoder::default().decode(&path).unwrap_err();
        let _ = fs::remove_file(&path);
        assert!(matches!(
            &error,
            MediaDecodeError::Corrupt(_) | MediaDecodeError::UnsupportedFormat(_)
        ));
        assert!(error.to_string().contains("media"));
    }

    fn stereo_pcm(sample_rate: u32, frames: usize) -> PcmAsset {
        let mut samples = Vec::with_capacity(frames * 2);
        for frame in 0..frames {
            let sample = (TAU * 440.0 * frame as f32 / sample_rate as f32).sin() * 0.5;
            samples.extend([sample, -sample]);
        }
        PcmAsset::new(
            AudioFormat::new(sample_rate, 2).unwrap(),
            Arc::from(samples),
        )
        .unwrap()
    }

    #[test]
    fn rubato_conversion_is_explicit_deterministic_and_preserves_channels() {
        let source = stereo_pcm(44_100, 441);
        let converter = RubatoSampleRateConverter::default();
        let first = converter.convert(&source, 48_000).unwrap();
        let second = converter.convert(&source, 48_000).unwrap();

        assert_eq!(first.pcm.format.sample_rate.get(), 48_000);
        assert_eq!(first.pcm.format.channels.get(), 2);
        assert_eq!(first.pcm.frame_count(), 480);
        assert_eq!(first.pcm.samples.as_ref(), second.pcm.samples.as_ref());
        assert!(first.pcm.samples.iter().all(|sample| sample.is_finite()));
        assert!(first
            .pcm
            .samples
            .chunks_exact(2)
            .all(|frame| (frame[0] + frame[1]).abs() < 1.0e-6));
        assert_eq!(first.provenance.backend, "rubato");
        assert_eq!(first.provenance.backend_version, RUBATO_CONVERTER_VERSION);
        assert_eq!(first.provenance.algorithm, "asynchronous-windowed-sinc");
        assert_eq!(first.provenance.input_frames, 441);
        assert_eq!(first.provenance.output_frames, 480);
        assert_eq!(first.provenance.channels, 2);
    }

    #[test]
    fn rubato_conversion_uses_exact_rational_duration_for_downsampling() {
        let converter = RubatoSampleRateConverter::default();

        let integral = converter.convert(&stereo_pcm(48_000, 960), 24_000).unwrap();
        assert_eq!(integral.pcm.frame_count(), 480);
        assert_eq!(integral.pcm.samples.len(), 480 * 2);
        assert_eq!(integral.provenance.output_frames, 480);

        // A partial destination-frame duration is retained, but the frame
        // count is still derived using integer rational arithmetic.
        let fractional = converter.convert(&stereo_pcm(48_000, 2), 32_000).unwrap();
        assert_eq!(fractional.pcm.frame_count(), 2);
        assert_eq!(fractional.pcm.samples.len(), 2 * 2);
        assert_eq!(fractional.provenance.output_frames, 2);
    }

    #[test]
    fn project_rate_material_keeps_original_provenance_and_skips_equal_rate() {
        let source = stereo_pcm(44_100, 441);
        let source_samples = Arc::clone(&source.samples);
        let encoded = b"original encoded source";
        let decoded = ProvenancedDecodedMaterial {
            decoded: DecodedMaterial {
                path: PathBuf::from("/original/source.wav"),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: 44_100,
                    channels: 2,
                    frame_count: SampleFrames(441),
                    container: Some("wav".into()),
                    codec: Some("pcm".into()),
                    bit_depth: Some(16),
                },
                fingerprint: ContentFingerprint::from_bytes(encoded),
                pcm: source,
            },
            provenance: MediaDecodeProvenance {
                backend: "symphonia",
                backend_version: SYMPHONIA_DECODER_VERSION,
                source_bytes: encoded.len() as u64,
                stream_count: 1,
                selected_track_id: 0,
                container: Some("wav".into()),
                codec: "pcm".into(),
                declared_frames: Some(441),
                gapless: true,
                verification: DecodeVerification::Unavailable,
            },
        };
        let converter = RubatoSampleRateConverter::default();

        let native = decoded.pcm_for_project_rate(44_100, &converter).unwrap();
        assert!(native.conversion.is_none());
        assert!(Arc::ptr_eq(&native.pcm.samples, &source_samples));
        assert_eq!(native.source_metadata.sample_rate_hz, 44_100);
        assert_eq!(
            native.source_fingerprint,
            ContentFingerprint::from_bytes(encoded)
        );

        let converted = decoded.pcm_for_project_rate(48_000, &converter).unwrap();
        assert_eq!(converted.pcm.format.sample_rate.get(), 48_000);
        assert_eq!(converted.source_metadata.sample_rate_hz, 44_100);
        assert_eq!(converted.decode_provenance.backend, "symphonia");
        assert_eq!(converted.conversion.unwrap().input_sample_rate_hz, 44_100);
    }

    #[test]
    fn resampler_refuses_nonfinite_input_and_equal_rate_calls() {
        let converter = RubatoSampleRateConverter::default();
        let invalid =
            PcmAsset::new(AudioFormat::new(44_100, 1).unwrap(), Arc::from([f32::NAN])).unwrap();
        assert_eq!(
            converter.convert(&invalid, 48_000).unwrap_err(),
            SampleRateConversionError::NonFiniteInput { index: 0 }
        );

        let valid = stereo_pcm(44_100, 16);
        assert_eq!(
            converter.convert(&valid, 44_100).unwrap_err(),
            SampleRateConversionError::RatesEqual(44_100)
        );
    }
}
