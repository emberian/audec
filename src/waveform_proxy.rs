//! Content-addressed, visible-range waveform proxy requests for audio clips.
//!
//! This module plans numeric queries; it does not paint, decode files, retain
//! GPUI objects, or claim to describe post-DSP output. A source envelope is a
//! navigation proxy for an immutable asset. Pitch preservation, warping,
//! looping, effects, automation, and mixer processing may make the rendered
//! waveform differ, so non-contiguous mappings are surfaced for a controller
//! to resolve instead of being silently approximated as one source interval.

use std::error::Error;
use std::fmt;

use crate::arrangement::{
    AudioLoopMode, ChannelMapping, ClipId, FrameRange, PlaybackTransform, SourceRange,
    StretchAlgorithm, WarpMarker,
};
use crate::assets::{AssetId, ContentFingerprint, SampleFrames};
use crate::pyramid::{WaveformPyramid, WaveformQuery};

/// Immutable facts needed to address one decoded media-pool asset.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WaveformAssetKey {
    pub asset: AssetId,
    pub content: ContentFingerprint,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub frame_count: SampleFrames,
}

impl WaveformAssetKey {
    pub fn new(
        asset: AssetId,
        content: ContentFingerprint,
        sample_rate_hz: u32,
        channels: u16,
        frame_count: SampleFrames,
    ) -> Result<Self, WaveformProxyError> {
        if content.bytes_hashed == 0 {
            return Err(WaveformProxyError::EmptyContentFingerprint);
        }
        if sample_rate_hz == 0 || channels == 0 || frame_count.0 == 0 {
            return Err(WaveformProxyError::InvalidAssetMetadata);
        }
        Ok(Self {
            asset,
            content,
            sample_rate_hz,
            channels,
            frame_count,
        })
    }
}

/// Physical raster demand derived at the GPUI boundary.
///
/// Both input values are retained as IEEE bits for diagnostics. Cache identity
/// uses the resulting physical width, since two displays requesting the same
/// number of bins can reuse the same numeric proxy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelTarget {
    pub logical_width_bits: u64,
    pub scale_factor_bits: u64,
    pub physical_width: u32,
}

impl PixelTarget {
    pub fn new(logical_width: f64, scale_factor: f64) -> Result<Self, WaveformProxyError> {
        if !logical_width.is_finite()
            || !scale_factor.is_finite()
            || logical_width <= 0.0
            || scale_factor <= 0.0
        {
            return Err(WaveformProxyError::InvalidPixelTarget);
        }
        let physical = (logical_width * scale_factor).ceil();
        if !physical.is_finite() || physical > f64::from(u32::MAX) {
            return Err(WaveformProxyError::InvalidPixelTarget);
        }
        Ok(Self {
            logical_width_bits: logical_width.to_bits(),
            scale_factor_bits: scale_factor.to_bits(),
            physical_width: physical.max(1.0) as u32,
        })
    }
}

/// Canonical projection requested from the source channels.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ChannelProjection {
    All,
    Channels(Vec<u16>),
    MonoSum,
    Mid,
    Side,
}

impl ChannelProjection {
    pub fn from_mapping(
        mapping: &ChannelMapping,
        source_channels: u16,
    ) -> Result<Self, WaveformProxyError> {
        match mapping {
            ChannelMapping::All => Ok(Self::All),
            ChannelMapping::Channels(channels) => {
                if channels.is_empty() || channels.iter().any(|channel| *channel >= source_channels)
                {
                    return Err(WaveformProxyError::InvalidChannelProjection);
                }
                Ok(Self::Channels(channels.clone()))
            }
            ChannelMapping::MonoSum => Ok(Self::MonoSum),
            ChannelMapping::Mid if source_channels >= 2 => Ok(Self::Mid),
            ChannelMapping::Side if source_channels >= 2 => Ok(Self::Side),
            ChannelMapping::Mid | ChannelMapping::Side => {
                Err(WaveformProxyError::InvalidChannelProjection)
            }
        }
    }

    /// Independent channel envelopes cannot exactly recover correlated sums,
    /// mid, or side. These projections need PCM or a projection-specific
    /// pyramid rather than arithmetic over min/max bins.
    pub fn requires_projected_pcm(&self) -> bool {
        matches!(self, Self::MonoSum | Self::Mid | Self::Side)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StretchAlgorithmKey {
    Resample,
    PreservePitch,
    PhaseVocoder,
    Granular,
    External(u32),
}

impl From<StretchAlgorithm> for StretchAlgorithmKey {
    fn from(value: StretchAlgorithm) -> Self {
        match value {
            StretchAlgorithm::Resample => Self::Resample,
            StretchAlgorithm::PreservePitch => Self::PreservePitch,
            StretchAlgorithm::PhaseVocoder => Self::PhaseVocoder,
            StretchAlgorithm::Granular => Self::Granular,
            StretchAlgorithm::External(id) => Self::External(id),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WarpMarkerKey {
    pub project_offset: u64,
    pub source_frame: u64,
}

impl From<WarpMarker> for WarpMarkerKey {
    fn from(value: WarpMarker) -> Self {
        Self {
            project_offset: value.project_offset,
            source_frame: value.source_frame,
        }
    }
}

/// Hashable playback recipe. Float equality is deliberately bit-exact.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PlaybackProxyKey {
    pub source_frames: u64,
    pub project_frames: u64,
    pub preserve_pitch: bool,
    pub pitch_semitones_bits: u64,
    pub reverse: bool,
    pub algorithm: StretchAlgorithmKey,
    pub warp_markers: Vec<WarpMarkerKey>,
}

impl PlaybackProxyKey {
    fn from_transform(transform: &PlaybackTransform) -> Result<Self, WaveformProxyError> {
        if !transform.pitch_semitones.is_finite()
            || transform.ratio.source_frames == 0
            || transform.ratio.project_frames == 0
        {
            return Err(WaveformProxyError::InvalidPlaybackTransform);
        }
        Ok(Self {
            source_frames: transform.ratio.source_frames,
            project_frames: transform.ratio.project_frames,
            preserve_pitch: transform.preserve_pitch,
            pitch_semitones_bits: transform.pitch_semitones.to_bits(),
            reverse: transform.reverse,
            algorithm: transform.algorithm.into(),
            warp_markers: transform
                .warp_markers
                .iter()
                .copied()
                .map(WarpMarkerKey::from)
                .collect(),
        })
    }
}

/// Controller-provided clip facts after resolving the arrangement asset alias
/// to a media-pool content identity.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipWaveformSpec {
    pub clip: ClipId,
    pub asset: WaveformAssetKey,
    pub placement: FrameRange,
    pub source: SourceRange,
    pub playback: PlaybackTransform,
    pub channels: ChannelMapping,
    pub loop_mode: AudioLoopMode,
}

/// Deterministic multiresolution choice for a visible interval.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WaveformLod {
    pub target_bins: u32,
    pub approximate_source_frames_per_bin: u64,
    pub preferred_power_of_two: u8,
}

/// Cache identity for one numeric source-envelope query.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct WaveformProxyKey {
    pub asset: WaveformAssetKey,
    pub source_start: u64,
    pub source_end: u64,
    pub projection: ChannelProjection,
    pub playback: PlaybackProxyKey,
    pub target_bins: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReadyWaveformRequest {
    pub clip: ClipId,
    pub visible_project: FrameRange,
    pub source: SourceRange,
    pub projection: ChannelProjection,
    pub reverse_display: bool,
    pub pixels: PixelTarget,
    pub lod: WaveformLod,
    pub key: WaveformProxyKey,
}

impl ReadyWaveformRequest {
    /// Execute this numeric request against the matching source pyramid.
    ///
    /// Correlated projections intentionally refuse the ordinary per-channel
    /// pyramid. Their controller path must build/query a projection-specific
    /// pyramid from PCM first.
    pub fn query_pyramid(
        &self,
        pyramid: &WaveformPyramid,
    ) -> Result<WaveformQuery, WaveformProxyError> {
        if self.projection.requires_projected_pcm() {
            return Err(WaveformProxyError::ProjectedPcmRequired);
        }
        if pyramid.channel_count() != usize::from(self.key.asset.channels)
            || u64::try_from(pyramid.frame_count()).ok() != Some(self.key.asset.frame_count.0)
        {
            return Err(WaveformProxyError::PyramidDoesNotMatchAsset);
        }
        let start = usize::try_from(self.source.start)
            .map_err(|_| WaveformProxyError::ArithmeticOverflow)?;
        let end =
            usize::try_from(self.source.end).map_err(|_| WaveformProxyError::ArithmeticOverflow)?;
        let target_bins = usize::try_from(self.lod.target_bins)
            .map_err(|_| WaveformProxyError::ArithmeticOverflow)?;
        Ok(pyramid.query(start, end, target_bins))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PiecewiseReason {
    WarpMarkers,
    ForwardLoop,
    PingPongLoop,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PiecewiseWaveformRequest {
    pub clip: ClipId,
    pub visible_project: FrameRange,
    pub asset: WaveformAssetKey,
    pub clip_source: SourceRange,
    pub projection: ChannelProjection,
    pub playback: PlaybackProxyKey,
    pub pixels: PixelTarget,
    pub reason: PiecewiseReason,
}

/// A plan never represents an empty query with a misleading source range.
#[derive(Clone, Debug, PartialEq)]
pub enum WaveformProxyPlan {
    NotVisible,
    Ready(ReadyWaveformRequest),
    RequiresPiecewiseMapping(PiecewiseWaveformRequest),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WaveformProxyError {
    EmptyContentFingerprint,
    InvalidAssetMetadata,
    InvalidPixelTarget,
    InvalidChannelProjection,
    InvalidPlaybackTransform,
    InconsistentClipMapping,
    SourceOutsideAsset,
    ProjectedPcmRequired,
    PyramidDoesNotMatchAsset,
    ArithmeticOverflow,
}

impl fmt::Display for WaveformProxyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EmptyContentFingerprint => "the asset fingerprint covers no bytes",
            Self::InvalidAssetMetadata => "waveform asset metadata is empty or invalid",
            Self::InvalidPixelTarget => "waveform raster dimensions are not finite and positive",
            Self::InvalidChannelProjection => "channel projection is invalid for the asset",
            Self::InvalidPlaybackTransform => "playback transform is invalid",
            Self::InconsistentClipMapping => {
                "clip placement, source range, and stretch ratio describe different durations"
            }
            Self::SourceOutsideAsset => "clip source range exceeds the decoded asset",
            Self::ProjectedPcmRequired => {
                "this channel projection requires PCM or a projection-specific pyramid"
            }
            Self::PyramidDoesNotMatchAsset => {
                "waveform pyramid shape does not match the addressed asset"
            }
            Self::ArithmeticOverflow => "waveform range arithmetic overflowed",
        };
        formatter.write_str(message)
    }
}

impl Error for WaveformProxyError {}

/// Plan the numeric waveform needed for one clip in one visible viewport.
pub fn plan_clip_waveform(
    spec: &ClipWaveformSpec,
    viewport: FrameRange,
    pixels: PixelTarget,
) -> Result<WaveformProxyPlan, WaveformProxyError> {
    if spec.source.start >= spec.source.end || spec.source.end > spec.asset.frame_count.0 {
        return Err(WaveformProxyError::SourceOutsideAsset);
    }
    let projection = ChannelProjection::from_mapping(&spec.channels, spec.asset.channels)?;
    let playback = PlaybackProxyKey::from_transform(&spec.playback)?;
    let Some(visible_project) = spec.placement.intersection(viewport) else {
        return Ok(WaveformProxyPlan::NotVisible);
    };

    let reason = if !spec.playback.warp_markers.is_empty() {
        Some(PiecewiseReason::WarpMarkers)
    } else {
        match spec.loop_mode {
            AudioLoopMode::Off => None,
            AudioLoopMode::Forward(_) => Some(PiecewiseReason::ForwardLoop),
            AudioLoopMode::PingPong(_) => Some(PiecewiseReason::PingPongLoop),
        }
    };
    if let Some(reason) = reason {
        return Ok(WaveformProxyPlan::RequiresPiecewiseMapping(
            PiecewiseWaveformRequest {
                clip: spec.clip,
                visible_project,
                asset: spec.asset,
                clip_source: spec.source,
                projection,
                playback,
                pixels,
                reason,
            },
        ));
    }

    // A contiguous clip has one exact affine mapping. Refuse inconsistent
    // metadata instead of clipping the mapped interval into a plausible
    // looking waveform that no longer corresponds to the audible source.
    if u128::from(spec.source.len()) * u128::from(spec.playback.ratio.project_frames)
        != u128::from(spec.placement.len()) * u128::from(spec.playback.ratio.source_frames)
    {
        return Err(WaveformProxyError::InconsistentClipMapping);
    }

    let visible_start = visible_project
        .start
        .0
        .checked_sub(spec.placement.start.0)
        .ok_or(WaveformProxyError::ArithmeticOverflow)? as u64;
    let visible_end = visible_project
        .end
        .0
        .checked_sub(spec.placement.start.0)
        .ok_or(WaveformProxyError::ArithmeticOverflow)? as u64;
    let mapped_start = ratio_floor(
        visible_start,
        spec.playback.ratio.source_frames,
        spec.playback.ratio.project_frames,
    )?;
    let mapped_end = ratio_ceil(
        visible_end,
        spec.playback.ratio.source_frames,
        spec.playback.ratio.project_frames,
    )?;
    let mapped_start = mapped_start.min(spec.source.len());
    let mapped_end = mapped_end.min(spec.source.len()).max(mapped_start);
    if mapped_start == mapped_end {
        return Ok(WaveformProxyPlan::NotVisible);
    }

    let (source_start, source_end) = if spec.playback.reverse {
        (
            spec.source
                .end
                .checked_sub(mapped_end)
                .ok_or(WaveformProxyError::ArithmeticOverflow)?,
            spec.source
                .end
                .checked_sub(mapped_start)
                .ok_or(WaveformProxyError::ArithmeticOverflow)?,
        )
    } else {
        (
            spec.source
                .start
                .checked_add(mapped_start)
                .ok_or(WaveformProxyError::ArithmeticOverflow)?,
            spec.source
                .start
                .checked_add(mapped_end)
                .ok_or(WaveformProxyError::ArithmeticOverflow)?,
        )
    };
    let source = SourceRange::new(source_start, source_end)
        .map_err(|_| WaveformProxyError::ArithmeticOverflow)?;
    let target_bins = pixels
        .physical_width
        .min(source.len().min(u64::from(u32::MAX)) as u32);
    let frames_per_bin = div_ceil(source.len(), u64::from(target_bins.max(1)));
    let lod = WaveformLod {
        target_bins,
        approximate_source_frames_per_bin: frames_per_bin,
        preferred_power_of_two: floor_log2(frames_per_bin),
    };
    let key = WaveformProxyKey {
        asset: spec.asset,
        source_start,
        source_end,
        projection: projection.clone(),
        playback,
        target_bins,
    };
    Ok(WaveformProxyPlan::Ready(ReadyWaveformRequest {
        clip: spec.clip,
        visible_project,
        source,
        projection,
        reverse_display: spec.playback.reverse,
        pixels,
        lod,
        key,
    }))
}

fn ratio_floor(value: u64, numerator: u64, denominator: u64) -> Result<u64, WaveformProxyError> {
    let product = u128::from(value) * u128::from(numerator);
    u64::try_from(product / u128::from(denominator))
        .map_err(|_| WaveformProxyError::ArithmeticOverflow)
}

fn ratio_ceil(value: u64, numerator: u64, denominator: u64) -> Result<u64, WaveformProxyError> {
    let product = u128::from(value) * u128::from(numerator);
    let denominator = u128::from(denominator);
    let quotient = product / denominator;
    let rounded = quotient + u128::from(product % denominator != 0);
    u64::try_from(rounded).map_err(|_| WaveformProxyError::ArithmeticOverflow)
}

fn div_ceil(value: u64, denominator: u64) -> u64 {
    value / denominator + u64::from(value % denominator != 0)
}

fn floor_log2(value: u64) -> u8 {
    (u64::BITS - 1 - value.max(1).leading_zeros()) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrangement::{Frame, StretchRatio, WarpMarker};
    use crate::assets::ContentFingerprint;
    use crate::pyramid::WaveformPyramid;

    fn asset(frame_count: u64, channels: u16) -> WaveformAssetKey {
        WaveformAssetKey::new(
            AssetId(3),
            ContentFingerprint::from_bytes(b"fixture-audio"),
            48_000,
            channels,
            SampleFrames(frame_count),
        )
        .unwrap()
    }

    fn spec() -> ClipWaveformSpec {
        ClipWaveformSpec {
            clip: ClipId::from_raw(7),
            asset: asset(20_000, 2),
            placement: FrameRange::new(Frame::new(1_000), Frame::new(5_000)).unwrap(),
            source: SourceRange::new(4_000, 12_000).unwrap(),
            playback: PlaybackTransform {
                ratio: StretchRatio::new(2, 1).unwrap(),
                preserve_pitch: true,
                pitch_semitones: 0.0,
                reverse: false,
                algorithm: StretchAlgorithm::PreservePitch,
                warp_markers: Vec::new(),
            },
            channels: ChannelMapping::All,
            loop_mode: AudioLoopMode::Off,
        }
    }

    #[test]
    fn visible_range_and_retina_width_determine_exact_query_and_lod() {
        let plan = plan_clip_waveform(
            &spec(),
            FrameRange::new(Frame::new(2_000), Frame::new(3_000)).unwrap(),
            PixelTarget::new(250.0, 2.0).unwrap(),
        )
        .unwrap();
        let WaveformProxyPlan::Ready(request) = plan else {
            panic!("expected a contiguous request");
        };
        assert_eq!(request.visible_project.start, Frame::new(2_000));
        assert_eq!(request.visible_project.end, Frame::new(3_000));
        assert_eq!(request.source, SourceRange::new(6_000, 8_000).unwrap());
        assert_eq!(request.lod.target_bins, 500);
        assert_eq!(request.lod.approximate_source_frames_per_bin, 4);
        assert_eq!(request.lod.preferred_power_of_two, 2);
    }

    #[test]
    fn non_integral_stretch_conservatively_covers_contributing_source_frames() {
        let mut spec = spec();
        spec.placement = FrameRange::new(Frame::ZERO, Frame::new(6_000)).unwrap();
        spec.source = SourceRange::new(100, 10_100).unwrap();
        spec.playback.ratio = StretchRatio::new(5, 3).unwrap();
        let plan = plan_clip_waveform(
            &spec,
            FrameRange::new(Frame::new(1), Frame::new(4)).unwrap(),
            PixelTarget::new(16.0, 1.0).unwrap(),
        )
        .unwrap();
        let WaveformProxyPlan::Ready(request) = plan else {
            panic!("expected a contiguous request");
        };
        // floor(1 * 5/3) .. ceil(4 * 5/3), offset by source start.
        assert_eq!(request.source, SourceRange::new(101, 107).unwrap());
    }

    #[test]
    fn inconsistent_contiguous_mapping_is_refused_instead_of_clamped() {
        let mut spec = spec();
        spec.source = SourceRange::new(4_000, 11_999).unwrap();
        assert_eq!(
            plan_clip_waveform(&spec, spec.placement, PixelTarget::new(100.0, 1.0).unwrap(),),
            Err(WaveformProxyError::InconsistentClipMapping)
        );
    }

    #[test]
    fn reverse_mapping_uses_the_mirrored_source_interval_and_retains_direction() {
        let mut spec = spec();
        spec.playback.reverse = true;
        let plan = plan_clip_waveform(
            &spec,
            FrameRange::new(Frame::new(1_000), Frame::new(2_000)).unwrap(),
            PixelTarget::new(100.0, 1.0).unwrap(),
        )
        .unwrap();
        let WaveformProxyPlan::Ready(request) = plan else {
            panic!("expected a contiguous request");
        };
        assert_eq!(request.source, SourceRange::new(10_000, 12_000).unwrap());
        assert!(request.reverse_display);
    }

    #[test]
    fn non_contiguous_mappings_are_never_flattened_into_a_plausible_range() {
        let mut warped = spec();
        warped.playback.warp_markers.push(WarpMarker {
            project_offset: 100,
            source_frame: 200,
        });
        let plan = plan_clip_waveform(
            &warped,
            warped.placement,
            PixelTarget::new(300.0, 1.0).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            plan,
            WaveformProxyPlan::RequiresPiecewiseMapping(PiecewiseWaveformRequest {
                reason: PiecewiseReason::WarpMarkers,
                ..
            })
        ));

        let mut looped = spec();
        looped.loop_mode = AudioLoopMode::Forward(SourceRange::new(4_000, 5_000).unwrap());
        let plan = plan_clip_waveform(
            &looped,
            looped.placement,
            PixelTarget::new(300.0, 1.0).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            plan,
            WaveformProxyPlan::RequiresPiecewiseMapping(PiecewiseWaveformRequest {
                reason: PiecewiseReason::ForwardLoop,
                ..
            })
        ));
    }

    #[test]
    fn projected_pcm_is_required_when_channel_correlation_matters() {
        assert!(!ChannelProjection::All.requires_projected_pcm());
        assert!(!ChannelProjection::Channels(vec![1]).requires_projected_pcm());
        assert!(ChannelProjection::MonoSum.requires_projected_pcm());
        assert!(ChannelProjection::Mid.requires_projected_pcm());
        assert_eq!(
            ChannelProjection::from_mapping(&ChannelMapping::Side, 1),
            Err(WaveformProxyError::InvalidChannelProjection)
        );
    }

    #[test]
    fn offscreen_clips_do_not_create_empty_cache_entries() {
        let plan = plan_clip_waveform(
            &spec(),
            FrameRange::new(Frame::new(20_000), Frame::new(21_000)).unwrap(),
            PixelTarget::new(100.0, 1.0).unwrap(),
        )
        .unwrap();
        assert_eq!(plan, WaveformProxyPlan::NotVisible);
    }

    #[test]
    fn ready_request_queries_only_its_exact_source_interval() {
        let samples = (0..20_000)
            .flat_map(|frame| [frame as f32, -(frame as f32)])
            .collect::<Vec<_>>();
        let pyramid = WaveformPyramid::from_interleaved(&samples, 2);
        let plan = plan_clip_waveform(
            &spec(),
            FrameRange::new(Frame::new(2_000), Frame::new(3_000)).unwrap(),
            PixelTarget::new(250.0, 2.0).unwrap(),
        )
        .unwrap();
        let WaveformProxyPlan::Ready(request) = plan else {
            panic!("expected a contiguous request");
        };
        let query = request.query_pyramid(&pyramid).unwrap();
        assert_eq!((query.start_frame, query.end_frame), (6_000, 8_000));
        assert_eq!(query.bins.len(), 500);
        assert_eq!(query.bins.first().unwrap().start_frame, 6_000);
        assert_eq!(query.bins.last().unwrap().end_frame, 8_000);
    }
}
