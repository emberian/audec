//! Deterministic, sample-accurate arrangement editing.
//!
//! This module is deliberately backend-only. It describes edits and playback
//! metadata, but it does not decode assets, stretch audio, evaluate patterns,
//! or render fades. A renderer may compile this state after validation.
//!
//! Persistent state uses typed, monotonic IDs, integer frame coordinates,
//! ordered maps, and data-only transactions. That makes it suitable for a
//! versioned serialization boundary without coupling project truth to a UI.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! typed_id {
    ($name:ident) => {
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        pub struct $name(u64);

        impl $name {
            pub const fn from_raw(raw: u64) -> Self {
                Self(raw)
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

typed_id!(TrackId);
typed_id!(ClipId);
typed_id!(AssetId);
typed_id!(PatternId);
typed_id!(ParameterId);

/// A signed project-frame coordinate. Negative positions support preroll.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Frame(pub i64);

impl Frame {
    pub const ZERO: Self = Self(0);

    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> i64 {
        self.0
    }

    pub fn checked_add(self, delta: i64) -> Result<Self, ArrangementError> {
        self.0
            .checked_add(delta)
            .map(Self)
            .ok_or(ArrangementError::TimeOverflow)
    }
}

/// A non-empty, half-open range in project frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FrameRange {
    pub start: Frame,
    pub end: Frame,
}

impl FrameRange {
    pub fn new(start: Frame, end: Frame) -> Result<Self, ArrangementError> {
        if start >= end {
            return Err(ArrangementError::EmptyRange);
        }
        Ok(Self { start, end })
    }

    pub fn from_start_and_len(start: Frame, len: u64) -> Result<Self, ArrangementError> {
        if len == 0 || len > i64::MAX as u64 {
            return Err(ArrangementError::EmptyRange);
        }
        Self::new(start, start.checked_add(len as i64)?)
    }

    pub fn len(self) -> u64 {
        self.end.0.saturating_sub(self.start.0) as u64
    }

    pub fn contains(self, frame: Frame) -> bool {
        self.start <= frame && frame < self.end
    }

    pub fn intersects(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }

    pub fn intersection(self, other: Self) -> Option<Self> {
        Self::new(self.start.max(other.start), self.end.min(other.end)).ok()
    }

    pub fn translated(self, delta: i64) -> Result<Self, ArrangementError> {
        Self::new(self.start.checked_add(delta)?, self.end.checked_add(delta)?)
    }
}

/// A non-empty, half-open range in an immutable audio asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceRange {
    pub start: u64,
    pub end: u64,
}

impl SourceRange {
    pub fn new(start: u64, end: u64) -> Result<Self, ArrangementError> {
        if start >= end {
            return Err(ArrangementError::EmptySourceRange);
        }
        Ok(Self { start, end })
    }

    pub const fn len(self) -> u64 {
        self.end - self.start
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackKind {
    Audio,
    Pattern,
    Automation,
    Hybrid,
    Group,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverlapPolicy {
    /// All overlapping clips render; the mixer or comp layer decides how.
    #[default]
    Mix,
    /// Any positive-duration overlap on this track is invalid.
    Reject,
    /// Overlaps render and are intended to receive explicit crossfades.
    Crossfade,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: TrackId,
    pub name: String,
    pub kind: TrackKind,
    pub overlap: OverlapPolicy,
    pub clip_ids: Vec<ClipId>,
    pub muted: bool,
    pub solo: bool,
    pub locked: bool,
    pub gain_db: f32,
    pub pan: f32,
}

impl Track {
    fn new(id: TrackId, name: String, kind: TrackKind) -> Self {
        Self {
            id,
            name,
            kind,
            overlap: OverlapPolicy::Mix,
            clip_ids: Vec::new(),
            muted: false,
            solo: false,
            locked: false,
            gain_db: 0.0,
            pan: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StretchAlgorithm {
    Resample,
    PreservePitch,
    PhaseVocoder,
    Granular,
    External(u32),
}

/// Exact source-frames/project-frames ratio. DSP algorithm choice is separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StretchRatio {
    pub source_frames: u64,
    pub project_frames: u64,
}

impl StretchRatio {
    pub fn new(source_frames: u64, project_frames: u64) -> Result<Self, ArrangementError> {
        if source_frames == 0 || project_frames == 0 {
            return Err(ArrangementError::InvalidStretch);
        }
        let divisor = gcd(source_frames, project_frames);
        Ok(Self {
            source_frames: source_frames / divisor,
            project_frames: project_frames / divisor,
        })
    }

    pub const fn unity() -> Self {
        Self {
            source_frames: 1,
            project_frames: 1,
        }
    }

    /// Maps a project offset exactly. Boundaries between source frames fail
    /// rather than being rounded differently by edit and render paths.
    pub fn source_offset(self, project_offset: u64) -> Result<u64, ArrangementError> {
        let numerator = u128::from(project_offset) * u128::from(self.source_frames);
        let denominator = u128::from(self.project_frames);
        if numerator % denominator != 0 {
            return Err(ArrangementError::NonIntegralSourceBoundary);
        }
        u64::try_from(numerator / denominator).map_err(|_| ArrangementError::TimeOverflow)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WarpMarker {
    pub project_offset: u64,
    pub source_frame: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlaybackTransform {
    pub ratio: StretchRatio,
    pub preserve_pitch: bool,
    pub pitch_semitones: f64,
    pub reverse: bool,
    pub algorithm: StretchAlgorithm,
    /// Metadata for a future piecewise renderer. Destructive arrangement
    /// operations reject populated marker lists until that mapping is compiled.
    pub warp_markers: Vec<WarpMarker>,
}

impl Default for PlaybackTransform {
    fn default() -> Self {
        Self {
            ratio: StretchRatio::unity(),
            preserve_pitch: true,
            pitch_semitones: 0.0,
            reverse: false,
            algorithm: StretchAlgorithm::PreservePitch,
            warp_markers: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FadeCurve {
    Linear,
    EqualPower,
    SmoothStep,
}

/// A clip-edge fade. Phase bounds let a split preserve the original envelope
/// without inventing a new fade at the cut.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fade {
    pub duration: u64,
    pub curve: FadeCurve,
    pub phase_start: f64,
    pub phase_end: f64,
}

impl Fade {
    pub fn full(duration: u64, curve: FadeCurve) -> Self {
        Self {
            duration,
            curve,
            phase_start: 0.0,
            phase_end: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ClipFades {
    pub fade_in: Option<Fade>,
    pub fade_out: Option<Fade>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioRegion {
    pub asset: AssetId,
    pub source: SourceRange,
    pub playback: PlaybackTransform,
    pub channels: ChannelMapping,
    pub loop_mode: AudioLoopMode,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelMapping {
    #[default]
    All,
    Channels(Vec<u16>),
    MonoSum,
    Mid,
    Side,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioLoopMode {
    #[default]
    Off,
    Forward(SourceRange),
    PingPong(SourceRange),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatternRegion {
    pub pattern: PatternId,
    /// Offset into the reusable definition in compiled project frames.
    pub content_offset_frames: u64,
    pub looped: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationRegion {
    pub parameter: ParameterId,
    /// Offset into the reusable automation curve in compiled project frames.
    pub content_offset_frames: u64,
    pub looped: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ClipContent {
    Audio(AudioRegion),
    Pattern(PatternRegion),
    Automation(AutomationRegion),
}

impl ClipContent {
    pub fn kind(&self) -> TrackKind {
        match self {
            Self::Audio(_) => TrackKind::Audio,
            Self::Pattern(_) => TrackKind::Pattern,
            Self::Automation(_) => TrackKind::Automation,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub id: ClipId,
    pub track_id: TrackId,
    pub name: String,
    pub placement: FrameRange,
    pub content: ClipContent,
    pub fades: ClipFades,
    pub gain_db: f32,
    pub muted: bool,
    pub locked: bool,
}

impl Clip {
    /// Evaluates only the clip-edge envelope metadata. Track/clip gain and DSP
    /// are intentionally outside this normalized `0..=1` factor.
    pub fn fade_gain_at(&self, project_offset: u64) -> Option<f64> {
        let len = self.placement.len();
        if project_offset > len {
            return None;
        }
        let mut gain = 1.0;
        if let Some(fade) = self.fades.fade_in {
            if project_offset <= fade.duration {
                let amount = project_offset as f64 / fade.duration as f64;
                let phase = lerp(fade.phase_start, fade.phase_end, amount);
                gain *= fade_curve_gain(fade.curve, phase);
            }
        }
        if let Some(fade) = self.fades.fade_out {
            let fade_start = len - fade.duration;
            if project_offset >= fade_start {
                let amount = (project_offset - fade_start) as f64 / fade.duration as f64;
                let phase = lerp(fade.phase_start, fade.phase_end, amount);
                gain *= 1.0 - fade_curve_gain(fade.curve, phase);
            }
        }
        Some(gain.clamp(0.0, 1.0))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub clips: BTreeSet<ClipId>,
    pub tracks: BTreeSet<TrackId>,
    pub time: Option<FrameRange>,
}

impl Selection {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn retain_valid(&mut self, state: &ArrangementState) {
        self.clips.retain(|id| state.clips.contains_key(id));
        self.tracks.retain(|id| state.tracks.contains_key(id));
    }
}

/// Persistent project arrangement. Ephemeral selection/history live in
/// [`ArrangementEditor`]. Counters are serialized and IDs are never reused.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArrangementState {
    pub schema_version: u32,
    pub sample_rate: u32,
    pub tracks: BTreeMap<TrackId, Track>,
    pub clips: BTreeMap<ClipId, Clip>,
    pub track_order: Vec<TrackId>,
    pub next_track_id: u64,
    pub next_clip_id: u64,
}

impl ArrangementState {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn new(sample_rate: u32) -> Result<Self, ArrangementError> {
        if sample_rate == 0 {
            return Err(ArrangementError::InvalidSampleRate);
        }
        Ok(Self {
            schema_version: Self::SCHEMA_VERSION,
            sample_rate,
            tracks: BTreeMap::new(),
            clips: BTreeMap::new(),
            track_order: Vec::new(),
            next_track_id: 1,
            next_clip_id: 1,
        })
    }

    pub fn track(&self, id: TrackId) -> Option<&Track> {
        self.tracks.get(&id)
    }

    pub fn clip(&self, id: ClipId) -> Option<&Clip> {
        self.clips.get(&id)
    }

    pub fn clips_on_track(&self, id: TrackId) -> impl Iterator<Item = &Clip> {
        self.tracks
            .get(&id)
            .into_iter()
            .flat_map(|track| track.clip_ids.iter())
            .filter_map(|clip| self.clips.get(clip))
    }

    pub fn clips_intersecting(&self, range: FrameRange) -> impl Iterator<Item = &Clip> {
        self.clips
            .values()
            .filter(move |clip| clip.placement.intersects(range))
    }

    pub fn project_range(&self) -> Option<FrameRange> {
        let start = self.clips.values().map(|clip| clip.placement.start).min()?;
        let end = self.clips.values().map(|clip| clip.placement.end).max()?;
        FrameRange::new(start, end).ok()
    }

    pub fn validate(&self) -> Result<(), ArrangementError> {
        if self.schema_version != Self::SCHEMA_VERSION {
            return Err(ArrangementError::UnsupportedSchema(self.schema_version));
        }
        if self.sample_rate == 0 {
            return Err(ArrangementError::InvalidSampleRate);
        }
        validate_order(
            &self.track_order,
            self.tracks.keys().copied(),
            "track order",
        )?;
        if self.next_track_id == 0
            || self.next_clip_id == 0
            || self
                .tracks
                .keys()
                .any(|id| id.get() == 0 || id.get() >= self.next_track_id)
            || self
                .clips
                .keys()
                .any(|id| id.get() == 0 || id.get() >= self.next_clip_id)
        {
            return Err(ArrangementError::InvalidIdCounter);
        }

        for (id, track) in &self.tracks {
            if *id != track.id || !valid_db(track.gain_db) || !(-1.0..=1.0).contains(&track.pan) {
                return Err(ArrangementError::InvalidTrack(*id));
            }
            validate_order(
                &track.clip_ids,
                self.clips
                    .values()
                    .filter(|clip| clip.track_id == *id)
                    .map(|clip| clip.id),
                "track clip index",
            )?;
            let mut previous: Option<&Clip> = None;
            for clip_id in &track.clip_ids {
                let clip = self
                    .clips
                    .get(clip_id)
                    .ok_or(ArrangementError::MissingClip(*clip_id))?;
                if let Some(prior) = previous {
                    let expected = (prior.placement.start, prior.id);
                    let actual = (clip.placement.start, clip.id);
                    if expected > actual {
                        return Err(ArrangementError::InvalidOrder("track clip index"));
                    }
                    if track.overlap == OverlapPolicy::Reject
                        && prior.placement.intersects(clip.placement)
                    {
                        return Err(ArrangementError::Overlap {
                            track: *id,
                            first: prior.id,
                            second: clip.id,
                        });
                    }
                }
                previous = Some(clip);
            }
        }

        for (id, clip) in &self.clips {
            if *id != clip.id || !self.tracks.contains_key(&clip.track_id) {
                return Err(ArrangementError::InvalidClip(*id));
            }
            if !valid_db(clip.gain_db) {
                return Err(ArrangementError::InvalidClip(*id));
            }
            let track = &self.tracks[&clip.track_id];
            if track.kind != TrackKind::Hybrid && track.kind != clip.content.kind() {
                return Err(ArrangementError::IncompatibleTrack {
                    track: clip.track_id,
                    clip: *id,
                });
            }
            validate_fades(clip.fades, clip.placement.len())?;
            if let ClipContent::Audio(audio) = &clip.content {
                validate_audio(audio, clip.placement.len())?;
            }
        }
        Ok(())
    }

    /// Applies put-style operations atomically without creating an editor-
    /// local undo entry.
    ///
    /// Aggregate project controllers use this kernel after cloning the whole
    /// project state. Preconditions are still checked against this state,
    /// indexes are normalized once for the batch, and a failed operation or
    /// validation leaves `self` untouched.
    pub fn apply_operations(
        &mut self,
        operations: &[ArrangementOperation],
    ) -> Result<(), ArrangementError> {
        if operations.is_empty() {
            return Err(ArrangementError::EmptyTransaction);
        }
        let mut candidate = self.clone();
        for operation in operations {
            operation.apply(&mut candidate)?;
        }
        candidate.normalize_indexes();
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    fn normalize_indexes(&mut self) {
        let mut seen = BTreeSet::new();
        self.track_order
            .retain(|id| self.tracks.contains_key(id) && seen.insert(*id));
        for id in self.tracks.keys() {
            if seen.insert(*id) {
                self.track_order.push(*id);
            }
        }
        for track in self.tracks.values_mut() {
            track.clip_ids = self
                .clips
                .values()
                .filter(|clip| clip.track_id == track.id)
                .map(|clip| clip.id)
                .collect();
            track.clip_ids.sort_by_key(|id| {
                let clip = &self.clips[id];
                (clip.placement.start, clip.id)
            });
        }
    }
}

fn validate_order<T: Copy + Ord>(
    order: &[T],
    expected: impl Iterator<Item = T>,
    label: &'static str,
) -> Result<(), ArrangementError> {
    let actual: BTreeSet<_> = order.iter().copied().collect();
    let expected: BTreeSet<_> = expected.collect();
    if actual != expected || actual.len() != order.len() {
        return Err(ArrangementError::InvalidOrder(label));
    }
    Ok(())
}

fn validate_audio(audio: &AudioRegion, placement_len: u64) -> Result<(), ArrangementError> {
    if !audio.playback.pitch_semitones.is_finite()
        || audio.playback.ratio.source_frames == 0
        || audio.playback.ratio.project_frames == 0
    {
        return Err(ArrangementError::InvalidStretch);
    }
    let mapped = audio.playback.ratio.source_offset(placement_len)?;
    if mapped != audio.source.len() {
        return Err(ArrangementError::SourceDurationMismatch);
    }
    let mut previous = None;
    for marker in &audio.playback.warp_markers {
        if marker.project_offset > placement_len
            || marker.source_frame < audio.source.start
            || marker.source_frame > audio.source.end
            || previous.is_some_and(|prior| prior >= marker.project_offset)
        {
            return Err(ArrangementError::InvalidWarpMarkers);
        }
        previous = Some(marker.project_offset);
    }
    if let AudioLoopMode::Forward(loop_range) | AudioLoopMode::PingPong(loop_range) =
        audio.loop_mode
    {
        if loop_range.start < audio.source.start || loop_range.end > audio.source.end {
            return Err(ArrangementError::InvalidAudioLoop);
        }
    }
    if let ChannelMapping::Channels(channels) = &audio.channels {
        let unique: BTreeSet<_> = channels.iter().copied().collect();
        if channels.is_empty() || unique.len() != channels.len() {
            return Err(ArrangementError::InvalidChannelMapping);
        }
    }
    Ok(())
}

fn validate_fades(fades: ClipFades, clip_len: u64) -> Result<(), ArrangementError> {
    for fade in [fades.fade_in, fades.fade_out].into_iter().flatten() {
        if fade.duration == 0
            || fade.duration > clip_len
            || !fade.phase_start.is_finite()
            || !fade.phase_end.is_finite()
            || !(0.0..=1.0).contains(&fade.phase_start)
            || !(0.0..=1.0).contains(&fade.phase_end)
            || fade.phase_start >= fade.phase_end
        {
            return Err(ArrangementError::InvalidFade);
        }
    }
    Ok(())
}

fn valid_db(value: f32) -> bool {
    value.is_finite() && (-144.0..=48.0).contains(&value)
}

fn gcd(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ArrangementOperation {
    PutTrack {
        before: Option<Track>,
        after: Option<Track>,
    },
    PutClip {
        before: Option<Clip>,
        after: Option<Clip>,
    },
    SetTrackOrder {
        before: Vec<TrackId>,
        after: Vec<TrackId>,
    },
}

impl ArrangementOperation {
    /// Exact inverse used by both the standalone editor and the aggregate
    /// project command history.
    pub fn inverse(&self) -> Self {
        match self {
            Self::PutTrack { before, after } => Self::PutTrack {
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutClip { before, after } => Self::PutClip {
                before: after.clone(),
                after: before.clone(),
            },
            Self::SetTrackOrder { before, after } => Self::SetTrackOrder {
                before: after.clone(),
                after: before.clone(),
            },
        }
    }

    fn apply(&self, state: &mut ArrangementState) -> Result<(), ArrangementError> {
        match self {
            Self::PutTrack { before, after } => {
                check_existing("track", before.as_ref(), after.as_ref(), |track| {
                    state.tracks.get(&track.id)
                })?;
                let id = before
                    .as_ref()
                    .or(after.as_ref())
                    .expect("validated replacement has an entity")
                    .id;
                match after {
                    Some(track) => {
                        state.next_track_id = state.next_track_id.max(
                            track
                                .id
                                .get()
                                .checked_add(1)
                                .ok_or(ArrangementError::IdOverflow)?,
                        );
                        state.tracks.insert(id, track.clone());
                    }
                    None => {
                        state.tracks.remove(&id);
                    }
                }
            }
            Self::PutClip { before, after } => {
                check_existing("clip", before.as_ref(), after.as_ref(), |clip| {
                    state.clips.get(&clip.id)
                })?;
                let id = before
                    .as_ref()
                    .or(after.as_ref())
                    .expect("validated replacement has an entity")
                    .id;
                match after {
                    Some(clip) => {
                        state.next_clip_id = state.next_clip_id.max(
                            clip.id
                                .get()
                                .checked_add(1)
                                .ok_or(ArrangementError::IdOverflow)?,
                        );
                        state.clips.insert(id, clip.clone());
                    }
                    None => {
                        state.clips.remove(&id);
                    }
                }
            }
            Self::SetTrackOrder { before, after } => {
                if &state.track_order != before {
                    return Err(ArrangementError::StaleOperation("track order"));
                }
                state.track_order = after.clone();
            }
        }
        Ok(())
    }
}

fn operation_is_noop(operation: &ArrangementOperation) -> bool {
    match operation {
        ArrangementOperation::PutTrack { before, after } => before == after,
        ArrangementOperation::PutClip { before, after } => before == after,
        ArrangementOperation::SetTrackOrder { before, after } => before == after,
    }
}

fn check_existing<'a, T: PartialEq + 'a>(
    label: &'static str,
    before: Option<&T>,
    after: Option<&T>,
    lookup: impl FnOnce(&T) -> Option<&'a T>,
) -> Result<(), ArrangementError> {
    if before.is_none() && after.is_none() {
        return Err(ArrangementError::EmptyOperation);
    }
    let probe = before.or(after).expect("checked above");
    if lookup(probe) != before {
        return Err(ArrangementError::StaleOperation(label));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArrangementTransaction {
    pub label: String,
    pub operations: Vec<ArrangementOperation>,
}

impl ArrangementTransaction {
    pub fn new(label: impl Into<String>, operations: Vec<ArrangementOperation>) -> Self {
        Self {
            label: label.into(),
            operations,
        }
    }

    pub fn inverse(&self) -> Self {
        Self {
            label: self.label.clone(),
            operations: self
                .operations
                .iter()
                .rev()
                .map(|op| op.inverse())
                .collect(),
        }
    }
}

#[derive(Clone, Debug)]
struct HistoryEntry {
    forward: ArrangementTransaction,
    inverse: ArrangementTransaction,
    before_revision: u64,
    after_revision: u64,
}

/// Stateful edit façade. Transactions validate atomically on a clone before
/// commit; failures never partially mutate project state.
#[derive(Clone, Debug)]
pub struct ArrangementEditor {
    state: ArrangementState,
    pub selection: Selection,
    undo: VecDeque<HistoryEntry>,
    redo: Vec<HistoryEntry>,
    revision: u64,
    saved_revision: u64,
    history_limit: usize,
}

impl ArrangementEditor {
    pub fn new(sample_rate: u32) -> Result<Self, ArrangementError> {
        Self::from_state(ArrangementState::new(sample_rate)?)
    }

    pub fn from_state(state: ArrangementState) -> Result<Self, ArrangementError> {
        state.validate()?;
        Ok(Self {
            state,
            selection: Selection::default(),
            undo: VecDeque::new(),
            redo: Vec::new(),
            revision: 0,
            saved_revision: 0,
            history_limit: 256,
        })
    }

    pub fn state(&self) -> &ArrangementState {
        &self.state
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_dirty(&self) -> bool {
        self.revision != self.saved_revision
    }

    pub fn mark_saved(&mut self) {
        self.saved_revision = self.revision;
    }

    pub fn set_history_limit(&mut self, limit: usize) {
        self.history_limit = limit;
        while self.undo.len() > limit {
            self.undo.pop_front();
        }
        if limit == 0 {
            self.undo.clear();
            self.redo.clear();
        } else if self.redo.len() > limit {
            self.redo.drain(..self.redo.len() - limit);
        }
    }

    pub fn undo_label(&self) -> Option<&str> {
        self.undo.back().map(|entry| entry.forward.label.as_str())
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|entry| entry.forward.label.as_str())
    }

    pub fn apply(&mut self, transaction: ArrangementTransaction) -> Result<u64, ArrangementError> {
        if transaction.operations.is_empty() {
            return Err(ArrangementError::EmptyTransaction);
        }
        if transaction.operations.iter().all(operation_is_noop) {
            return Ok(self.revision);
        }
        let mut candidate = self.state.clone();
        for operation in &transaction.operations {
            operation.apply(&mut candidate)?;
        }
        candidate.normalize_indexes();
        candidate.validate()?;
        let inverse = transaction.inverse();
        let before_revision = self.revision;
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(ArrangementError::RevisionOverflow)?;
        self.state = candidate;
        self.redo.clear();
        if self.history_limit > 0 {
            self.undo.push_back(HistoryEntry {
                forward: transaction,
                inverse,
                before_revision,
                after_revision: self.revision,
            });
            while self.undo.len() > self.history_limit {
                self.undo.pop_front();
            }
        }
        self.selection.retain_valid(&self.state);
        Ok(self.revision)
    }

    pub fn undo(&mut self) -> Result<Option<u64>, ArrangementError> {
        let Some(entry) = self.undo.pop_back() else {
            return Ok(None);
        };
        let mut candidate = self.state.clone();
        for operation in &entry.inverse.operations {
            operation.apply(&mut candidate)?;
        }
        candidate.normalize_indexes();
        candidate.validate()?;
        self.state = candidate;
        self.revision = entry.before_revision;
        self.selection.retain_valid(&self.state);
        self.redo.push(entry);
        Ok(Some(self.revision))
    }

    pub fn redo(&mut self) -> Result<Option<u64>, ArrangementError> {
        let Some(entry) = self.redo.pop() else {
            return Ok(None);
        };
        let mut candidate = self.state.clone();
        for operation in &entry.forward.operations {
            operation.apply(&mut candidate)?;
        }
        candidate.normalize_indexes();
        candidate.validate()?;
        self.state = candidate;
        self.revision = entry.after_revision;
        self.selection.retain_valid(&self.state);
        self.undo.push_back(entry);
        Ok(Some(self.revision))
    }

    pub fn create_track(
        &mut self,
        name: impl Into<String>,
        kind: TrackKind,
    ) -> Result<TrackId, ArrangementError> {
        let id = self.allocate_track_id()?;
        let track = Track::new(id, name.into(), kind);
        let mut order = self.state.track_order.clone();
        order.push(id);
        self.apply(ArrangementTransaction::new(
            "Create track",
            vec![
                ArrangementOperation::PutTrack {
                    before: None,
                    after: Some(track),
                },
                ArrangementOperation::SetTrackOrder {
                    before: self.state.track_order.clone(),
                    after: order,
                },
            ],
        ))?;
        Ok(id)
    }

    pub fn delete_track(&mut self, id: TrackId) -> Result<(), ArrangementError> {
        let track = self.require_track(id)?.clone();
        let mut operations = Vec::with_capacity(track.clip_ids.len() + 2);
        for clip_id in &track.clip_ids {
            let clip = self.require_clip(*clip_id)?.clone();
            operations.push(ArrangementOperation::PutClip {
                before: Some(clip),
                after: None,
            });
        }
        operations.push(ArrangementOperation::PutTrack {
            before: Some(track),
            after: None,
        });
        let mut order = self.state.track_order.clone();
        order.retain(|candidate| *candidate != id);
        operations.push(ArrangementOperation::SetTrackOrder {
            before: self.state.track_order.clone(),
            after: order,
        });
        self.apply(ArrangementTransaction::new("Delete track", operations))?;
        Ok(())
    }

    pub fn reorder_track(&mut self, id: TrackId, new_index: usize) -> Result<(), ArrangementError> {
        self.require_track(id)?;
        if new_index >= self.state.track_order.len() {
            return Err(ArrangementError::InvalidTrackIndex(new_index));
        }
        let before = self.state.track_order.clone();
        let mut after = before.clone();
        let old_index = after
            .iter()
            .position(|candidate| *candidate == id)
            .ok_or(ArrangementError::MissingTrack(id))?;
        after.remove(old_index);
        after.insert(new_index, id);
        self.apply(ArrangementTransaction::new(
            "Reorder track",
            vec![ArrangementOperation::SetTrackOrder { before, after }],
        ))?;
        Ok(())
    }

    /// Replaces clip selection from an exact marquee range, optionally scoped
    /// to selected tracks. Touching at a half-open edge is not an intersection.
    pub fn select_intersecting(&mut self, range: FrameRange, tracks: Option<&BTreeSet<TrackId>>) {
        self.selection.time = Some(range);
        self.selection.clips = self
            .state
            .clips_intersecting(range)
            .filter(|clip| tracks.map_or(true, |set| set.contains(&clip.track_id)))
            .map(|clip| clip.id)
            .collect();
    }

    pub fn set_overlap_policy(
        &mut self,
        id: TrackId,
        policy: OverlapPolicy,
    ) -> Result<(), ArrangementError> {
        let before = self.require_track(id)?.clone();
        let mut after = before.clone();
        after.overlap = policy;
        self.replace_track("Set overlap policy", before, after)
    }

    pub fn create_audio_clip(
        &mut self,
        track: TrackId,
        name: impl Into<String>,
        placement: FrameRange,
        asset: AssetId,
        source: SourceRange,
    ) -> Result<ClipId, ArrangementError> {
        let ratio = StretchRatio::new(source.len(), placement.len())?;
        self.create_clip(
            track,
            name,
            placement,
            ClipContent::Audio(AudioRegion {
                asset,
                source,
                playback: PlaybackTransform {
                    ratio,
                    ..PlaybackTransform::default()
                },
                channels: ChannelMapping::All,
                loop_mode: AudioLoopMode::Off,
            }),
        )
    }

    pub fn create_pattern_clip(
        &mut self,
        track: TrackId,
        name: impl Into<String>,
        placement: FrameRange,
        pattern: PatternId,
    ) -> Result<ClipId, ArrangementError> {
        self.create_clip(
            track,
            name,
            placement,
            ClipContent::Pattern(PatternRegion {
                pattern,
                content_offset_frames: 0,
                looped: false,
            }),
        )
    }

    pub fn create_automation_clip(
        &mut self,
        track: TrackId,
        name: impl Into<String>,
        placement: FrameRange,
        parameter: ParameterId,
    ) -> Result<ClipId, ArrangementError> {
        self.create_clip(
            track,
            name,
            placement,
            ClipContent::Automation(AutomationRegion {
                parameter,
                content_offset_frames: 0,
                looped: false,
            }),
        )
    }

    fn create_clip(
        &mut self,
        track: TrackId,
        name: impl Into<String>,
        placement: FrameRange,
        content: ClipContent,
    ) -> Result<ClipId, ArrangementError> {
        self.require_track(track)?;
        let id = self.allocate_clip_id()?;
        let clip = Clip {
            id,
            track_id: track,
            name: name.into(),
            placement,
            content,
            fades: ClipFades::default(),
            gain_db: 0.0,
            muted: false,
            locked: false,
        };
        self.apply(ArrangementTransaction::new(
            "Create clip",
            vec![ArrangementOperation::PutClip {
                before: None,
                after: Some(clip),
            }],
        ))?;
        Ok(id)
    }

    pub fn delete_clip(&mut self, id: ClipId) -> Result<(), ArrangementError> {
        let clip = self.editable_clip(id)?.clone();
        self.apply(ArrangementTransaction::new(
            "Delete clip",
            vec![ArrangementOperation::PutClip {
                before: Some(clip),
                after: None,
            }],
        ))?;
        Ok(())
    }

    pub fn move_clip(
        &mut self,
        id: ClipId,
        track: TrackId,
        start: Frame,
    ) -> Result<(), ArrangementError> {
        self.require_track(track)?;
        let before = self.editable_clip(id)?.clone();
        let mut after = before.clone();
        after.track_id = track;
        after.placement = FrameRange::from_start_and_len(start, before.placement.len())?;
        self.replace_clip("Move clip", before, after)
    }

    /// Trims the left edge while preserving source-to-timeline alignment.
    pub fn trim_left(&mut self, id: ClipId, new_start: Frame) -> Result<(), ArrangementError> {
        let before = self.editable_clip(id)?.clone();
        if new_start < before.placement.start || new_start >= before.placement.end {
            return Err(ArrangementError::InvalidTrim);
        }
        let delta = new_start.0.saturating_sub(before.placement.start.0) as u64;
        let mut after = before.clone();
        after.placement.start = new_start;
        advance_content(&mut after.content, delta)?;
        after.fades = trim_fades_left(before.fades, before.placement.len(), delta);
        self.replace_clip("Trim clip left", before, after)
    }

    /// Trims the right edge while preserving source-to-timeline alignment.
    pub fn trim_right(&mut self, id: ClipId, new_end: Frame) -> Result<(), ArrangementError> {
        let before = self.editable_clip(id)?.clone();
        if new_end <= before.placement.start || new_end > before.placement.end {
            return Err(ArrangementError::InvalidTrim);
        }
        let removed = before.placement.end.0.saturating_sub(new_end.0) as u64;
        let mut after = before.clone();
        after.placement.end = new_end;
        retreat_content_end(&mut after.content, removed)?;
        after.fades = trim_fades_right(before.fades, before.placement.len(), removed);
        self.replace_clip("Trim clip right", before, after)
    }

    /// Moves content beneath a fixed clip. Audio slip is exact in asset frames.
    pub fn slip_clip(&mut self, id: ClipId, project_delta: i64) -> Result<(), ArrangementError> {
        let before = self.editable_clip(id)?.clone();
        let mut after = before.clone();
        slip_content(&mut after.content, project_delta)?;
        self.replace_clip("Slip clip", before, after)
    }

    /// Splits at a project-frame boundary. The returned right clip receives a
    /// fresh ID; undo never makes that ID available for reuse.
    pub fn split_clip(&mut self, id: ClipId, at: Frame) -> Result<ClipId, ArrangementError> {
        let before = self.editable_clip(id)?.clone();
        if !before.placement.contains(at) || at == before.placement.start {
            return Err(ArrangementError::InvalidSplit);
        }
        let left_len = at.0.saturating_sub(before.placement.start.0) as u64;
        let right_id = self.allocate_clip_id()?;
        let (left_content, right_content) = split_content(&before.content, left_len)?;
        let (left_fades, right_fades) = split_fades(before.fades, before.placement.len(), left_len);

        let mut left = before.clone();
        left.placement.end = at;
        left.content = left_content;
        left.fades = left_fades;
        let mut right = before.clone();
        right.id = right_id;
        right.placement.start = at;
        right.content = right_content;
        right.fades = right_fades;
        self.apply(ArrangementTransaction::new(
            "Split clip",
            vec![
                ArrangementOperation::PutClip {
                    before: Some(before),
                    after: Some(left),
                },
                ArrangementOperation::PutClip {
                    before: None,
                    after: Some(right),
                },
            ],
        ))?;
        Ok(right_id)
    }

    pub fn duplicate_clip(
        &mut self,
        id: ClipId,
        new_start: Frame,
    ) -> Result<ClipId, ArrangementError> {
        let source = self.require_clip(id)?.clone();
        let duplicate_id = self.allocate_clip_id()?;
        let mut duplicate = source.clone();
        duplicate.id = duplicate_id;
        duplicate.placement = FrameRange::from_start_and_len(new_start, source.placement.len())?;
        self.apply(ArrangementTransaction::new(
            "Duplicate clip",
            vec![ArrangementOperation::PutClip {
                before: None,
                after: Some(duplicate),
            }],
        ))?;
        Ok(duplicate_id)
    }

    /// Resizes a clip. For audio this updates stretch metadata while retaining
    /// the exact source range; it does not perform time-stretch DSP.
    pub fn stretch_resize(
        &mut self,
        id: ClipId,
        new_end: Frame,
        algorithm: StretchAlgorithm,
        preserve_pitch: bool,
    ) -> Result<(), ArrangementError> {
        let before = self.editable_clip(id)?.clone();
        let placement = FrameRange::new(before.placement.start, new_end)?;
        let mut after = before.clone();
        after.placement = placement;
        match &mut after.content {
            ClipContent::Audio(audio) => {
                if !audio.playback.warp_markers.is_empty() {
                    return Err(ArrangementError::WarpedEditRequiresCompiler);
                }
                audio.playback.ratio = StretchRatio::new(audio.source.len(), placement.len())?;
                audio.playback.algorithm = algorithm;
                audio.playback.preserve_pitch = preserve_pitch;
            }
            ClipContent::Pattern(_) | ClipContent::Automation(_) => {}
        }
        after.fades = clamp_fades(after.fades, placement.len());
        self.replace_clip("Stretch clip", before, after)
    }

    pub fn set_fades(&mut self, id: ClipId, fades: ClipFades) -> Result<(), ArrangementError> {
        let before = self.editable_clip(id)?.clone();
        validate_fades(fades, before.placement.len())?;
        let mut after = before.clone();
        after.fades = fades;
        self.replace_clip("Set clip fades", before, after)
    }

    fn replace_track(
        &mut self,
        label: &'static str,
        before: Track,
        after: Track,
    ) -> Result<(), ArrangementError> {
        self.apply(ArrangementTransaction::new(
            label,
            vec![ArrangementOperation::PutTrack {
                before: Some(before),
                after: Some(after),
            }],
        ))?;
        Ok(())
    }

    fn replace_clip(
        &mut self,
        label: &'static str,
        before: Clip,
        after: Clip,
    ) -> Result<(), ArrangementError> {
        self.apply(ArrangementTransaction::new(
            label,
            vec![ArrangementOperation::PutClip {
                before: Some(before),
                after: Some(after),
            }],
        ))?;
        Ok(())
    }

    fn require_track(&self, id: TrackId) -> Result<&Track, ArrangementError> {
        self.state
            .tracks
            .get(&id)
            .ok_or(ArrangementError::MissingTrack(id))
    }

    fn require_clip(&self, id: ClipId) -> Result<&Clip, ArrangementError> {
        self.state
            .clips
            .get(&id)
            .ok_or(ArrangementError::MissingClip(id))
    }

    fn editable_clip(&self, id: ClipId) -> Result<&Clip, ArrangementError> {
        let clip = self.require_clip(id)?;
        if clip.locked || self.require_track(clip.track_id)?.locked {
            return Err(ArrangementError::LockedClip(id));
        }
        Ok(clip)
    }

    fn allocate_track_id(&self) -> Result<TrackId, ArrangementError> {
        let raw = self.state.next_track_id;
        raw.checked_add(1).ok_or(ArrangementError::IdOverflow)?;
        Ok(TrackId::from_raw(raw))
    }

    fn allocate_clip_id(&self) -> Result<ClipId, ArrangementError> {
        let raw = self.state.next_clip_id;
        raw.checked_add(1).ok_or(ArrangementError::IdOverflow)?;
        Ok(ClipId::from_raw(raw))
    }
}

fn advance_content(content: &mut ClipContent, project_delta: u64) -> Result<(), ArrangementError> {
    match content {
        ClipContent::Audio(audio) => {
            reject_warped(audio)?;
            let source_delta = audio.playback.ratio.source_offset(project_delta)?;
            if source_delta >= audio.source.len() {
                return Err(ArrangementError::InvalidTrim);
            }
            if audio.playback.reverse {
                audio.source.end = audio
                    .source
                    .end
                    .checked_sub(source_delta)
                    .ok_or(ArrangementError::TimeOverflow)?;
            } else {
                audio.source.start = audio
                    .source
                    .start
                    .checked_add(source_delta)
                    .ok_or(ArrangementError::TimeOverflow)?;
            }
        }
        ClipContent::Pattern(region) => {
            region.content_offset_frames = region
                .content_offset_frames
                .checked_add(project_delta)
                .ok_or(ArrangementError::TimeOverflow)?;
        }
        ClipContent::Automation(region) => {
            region.content_offset_frames = region
                .content_offset_frames
                .checked_add(project_delta)
                .ok_or(ArrangementError::TimeOverflow)?;
        }
    }
    Ok(())
}

fn retreat_content_end(
    content: &mut ClipContent,
    project_delta: u64,
) -> Result<(), ArrangementError> {
    if let ClipContent::Audio(audio) = content {
        reject_warped(audio)?;
        let source_delta = audio.playback.ratio.source_offset(project_delta)?;
        if source_delta >= audio.source.len() {
            return Err(ArrangementError::InvalidTrim);
        }
        if audio.playback.reverse {
            audio.source.start = audio
                .source
                .start
                .checked_add(source_delta)
                .ok_or(ArrangementError::TimeOverflow)?;
        } else {
            audio.source.end = audio
                .source
                .end
                .checked_sub(source_delta)
                .ok_or(ArrangementError::TimeOverflow)?;
        }
    }
    Ok(())
}

fn slip_content(content: &mut ClipContent, project_delta: i64) -> Result<(), ArrangementError> {
    match content {
        ClipContent::Audio(audio) => {
            reject_warped(audio)?;
            let magnitude = audio
                .playback
                .ratio
                .source_offset(project_delta.unsigned_abs())?;
            let signed = i128::from(magnitude) * if project_delta < 0 { -1 } else { 1 };
            let start = i128::from(audio.source.start) + signed;
            let end = i128::from(audio.source.end) + signed;
            if start < 0 || end > i128::from(u64::MAX) {
                return Err(ArrangementError::SourceOutOfBounds);
            }
            audio.source.start = start as u64;
            audio.source.end = end as u64;
        }
        ClipContent::Pattern(region) => {
            region.content_offset_frames =
                add_signed_u64(region.content_offset_frames, project_delta)?;
        }
        ClipContent::Automation(region) => {
            region.content_offset_frames =
                add_signed_u64(region.content_offset_frames, project_delta)?;
        }
    }
    Ok(())
}

fn split_content(
    content: &ClipContent,
    project_offset: u64,
) -> Result<(ClipContent, ClipContent), ArrangementError> {
    let mut left = content.clone();
    let mut right = content.clone();
    match (content, &mut left, &mut right) {
        (ClipContent::Audio(original), ClipContent::Audio(left), ClipContent::Audio(right)) => {
            reject_warped(original)?;
            let source_offset = original.playback.ratio.source_offset(project_offset)?;
            if original.playback.reverse {
                let boundary = original.source.end - source_offset;
                left.source.start = boundary;
                right.source.end = boundary;
            } else {
                let boundary = original.source.start + source_offset;
                left.source.end = boundary;
                right.source.start = boundary;
            }
        }
        (ClipContent::Pattern(_), _, _) | (ClipContent::Automation(_), _, _) => {
            advance_content(&mut right, project_offset)?;
        }
        _ => unreachable!("clones retain content variants"),
    }
    Ok((left, right))
}

fn reject_warped(audio: &AudioRegion) -> Result<(), ArrangementError> {
    if audio.playback.warp_markers.is_empty() {
        Ok(())
    } else {
        Err(ArrangementError::WarpedEditRequiresCompiler)
    }
}

fn add_signed_u64(value: u64, delta: i64) -> Result<u64, ArrangementError> {
    let result = i128::from(value) + i128::from(delta);
    if !(0..=i128::from(u64::MAX)).contains(&result) {
        return Err(ArrangementError::SourceOutOfBounds);
    }
    Ok(result as u64)
}

fn split_fades(fades: ClipFades, total: u64, left_len: u64) -> (ClipFades, ClipFades) {
    let right_len = total - left_len;
    let mut left = ClipFades::default();
    let mut right = ClipFades::default();
    if let Some(fade) = fades.fade_in {
        if left_len >= fade.duration {
            left.fade_in = Some(fade);
        } else {
            let fraction = left_len as f64 / fade.duration as f64;
            let phase = lerp(fade.phase_start, fade.phase_end, fraction);
            left.fade_in = Some(Fade {
                duration: left_len,
                phase_end: phase,
                ..fade
            });
            right.fade_in = Some(Fade {
                duration: fade.duration - left_len,
                phase_start: phase,
                ..fade
            });
        }
    }
    if let Some(fade) = fades.fade_out {
        let fade_start = total - fade.duration;
        if left_len <= fade_start {
            right.fade_out = Some(fade);
        } else {
            let elapsed = left_len - fade_start;
            let fraction = elapsed as f64 / fade.duration as f64;
            let phase = lerp(fade.phase_start, fade.phase_end, fraction);
            left.fade_out = Some(Fade {
                duration: fade.duration - (total - left_len),
                phase_end: phase,
                ..fade
            });
            right.fade_out = Some(Fade {
                duration: right_len,
                phase_start: phase,
                ..fade
            });
        }
    }
    (left, right)
}

fn trim_fades_left(fades: ClipFades, total: u64, removed: u64) -> ClipFades {
    split_fades(fades, total, removed).1
}

fn trim_fades_right(fades: ClipFades, total: u64, removed: u64) -> ClipFades {
    split_fades(fades, total, total - removed).0
}

fn clamp_fades(mut fades: ClipFades, len: u64) -> ClipFades {
    if let Some(fade) = &mut fades.fade_in {
        fade.duration = fade.duration.min(len);
    }
    if let Some(fade) = &mut fades.fade_out {
        fade.duration = fade.duration.min(len);
    }
    fades
}

fn lerp(left: f64, right: f64, amount: f64) -> f64 {
    left + (right - left) * amount
}

fn fade_curve_gain(curve: FadeCurve, phase: f64) -> f64 {
    let phase = phase.clamp(0.0, 1.0);
    match curve {
        FadeCurve::Linear => phase,
        FadeCurve::EqualPower => (phase * std::f64::consts::FRAC_PI_2).sin(),
        FadeCurve::SmoothStep => phase * phase * (3.0 - 2.0 * phase),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ArrangementError {
    InvalidSampleRate,
    EmptyRange,
    EmptySourceRange,
    TimeOverflow,
    IdOverflow,
    RevisionOverflow,
    InvalidIdCounter,
    UnsupportedSchema(u32),
    MissingTrack(TrackId),
    MissingClip(ClipId),
    InvalidTrack(TrackId),
    InvalidTrackIndex(usize),
    InvalidClip(ClipId),
    LockedClip(ClipId),
    IncompatibleTrack {
        track: TrackId,
        clip: ClipId,
    },
    InvalidOrder(&'static str),
    Overlap {
        track: TrackId,
        first: ClipId,
        second: ClipId,
    },
    InvalidStretch,
    NonIntegralSourceBoundary,
    SourceDurationMismatch,
    SourceOutOfBounds,
    InvalidWarpMarkers,
    InvalidAudioLoop,
    InvalidChannelMapping,
    WarpedEditRequiresCompiler,
    InvalidFade,
    InvalidTrim,
    InvalidSplit,
    EmptyOperation,
    EmptyTransaction,
    StaleOperation(&'static str),
}

impl fmt::Display for ArrangementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSampleRate => write!(formatter, "project sample rate must be non-zero"),
            Self::EmptyRange => write!(formatter, "frame range must be non-empty"),
            Self::EmptySourceRange => write!(formatter, "source range must be non-empty"),
            Self::TimeOverflow => write!(formatter, "frame arithmetic overflowed"),
            Self::IdOverflow => write!(formatter, "entity ID space exhausted"),
            Self::RevisionOverflow => write!(formatter, "revision space exhausted"),
            Self::InvalidIdCounter => write!(formatter, "serialized ID counter is invalid"),
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported arrangement schema {version}")
            }
            Self::MissingTrack(id) => write!(formatter, "track {id} does not exist"),
            Self::MissingClip(id) => write!(formatter, "clip {id} does not exist"),
            Self::InvalidTrack(id) => write!(formatter, "track {id} is invalid"),
            Self::InvalidTrackIndex(index) => write!(formatter, "track index {index} is invalid"),
            Self::InvalidClip(id) => write!(formatter, "clip {id} is invalid"),
            Self::LockedClip(id) => write!(formatter, "clip {id} or its track is locked"),
            Self::IncompatibleTrack { track, clip } => {
                write!(formatter, "clip {clip} is incompatible with track {track}")
            }
            Self::InvalidOrder(label) => write!(formatter, "invalid {label}"),
            Self::Overlap {
                track,
                first,
                second,
            } => write!(
                formatter,
                "clips {first} and {second} overlap on rejecting track {track}"
            ),
            Self::InvalidStretch => write!(formatter, "stretch metadata is invalid"),
            Self::NonIntegralSourceBoundary => {
                write!(formatter, "edit boundary falls between exact source frames")
            }
            Self::SourceDurationMismatch => {
                write!(
                    formatter,
                    "source range does not match playback ratio and placement"
                )
            }
            Self::SourceOutOfBounds => write!(formatter, "content offset is out of bounds"),
            Self::InvalidWarpMarkers => write!(formatter, "warp markers are invalid"),
            Self::InvalidAudioLoop => {
                write!(formatter, "audio loop lies outside its source range")
            }
            Self::InvalidChannelMapping => write!(formatter, "audio channel mapping is invalid"),
            Self::WarpedEditRequiresCompiler => {
                write!(
                    formatter,
                    "editing warped audio requires a piecewise mapping compiler"
                )
            }
            Self::InvalidFade => write!(formatter, "fade metadata is invalid"),
            Self::InvalidTrim => write!(formatter, "trim boundary is outside the clip"),
            Self::InvalidSplit => write!(formatter, "split boundary is outside the clip interior"),
            Self::EmptyOperation => write!(formatter, "operation has no before or after state"),
            Self::EmptyTransaction => write!(formatter, "transaction has no operations"),
            Self::StaleOperation(label) => write!(formatter, "stale {label} operation"),
        }
    }
}

impl Error for ArrangementError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: i64, len: u64) -> FrameRange {
        FrameRange::from_start_and_len(Frame(start), len).unwrap()
    }

    fn source(start: u64, len: u64) -> SourceRange {
        SourceRange::new(start, start + len).unwrap()
    }

    fn audio_editor() -> (ArrangementEditor, TrackId, ClipId) {
        let mut editor = ArrangementEditor::new(48_000).unwrap();
        let track = editor.create_track("Audio", TrackKind::Audio).unwrap();
        let clip = editor
            .create_audio_clip(
                track,
                "take",
                range(100, 1_000),
                AssetId::from_raw(4),
                source(500, 1_000),
            )
            .unwrap();
        (editor, track, clip)
    }

    #[test]
    fn typed_clips_enforce_track_capabilities_atomically() {
        let mut editor = ArrangementEditor::new(48_000).unwrap();
        let audio = editor.create_track("Audio", TrackKind::Audio).unwrap();
        let before = editor.state().clone();
        let error = editor
            .create_pattern_clip(audio, "wrong", range(0, 10), PatternId::from_raw(1))
            .unwrap_err();
        assert!(matches!(error, ArrangementError::IncompatibleTrack { .. }));
        assert_eq!(editor.state(), &before);
    }

    #[test]
    fn clips_are_indexed_in_stable_timeline_order() {
        let mut editor = ArrangementEditor::new(44_100).unwrap();
        let track = editor.create_track("Patterns", TrackKind::Pattern).unwrap();
        let late = editor
            .create_pattern_clip(track, "late", range(50, 10), PatternId::from_raw(1))
            .unwrap();
        let early = editor
            .create_pattern_clip(track, "early", range(-20, 10), PatternId::from_raw(1))
            .unwrap();
        assert_eq!(
            editor.state().track(track).unwrap().clip_ids,
            vec![early, late]
        );
    }

    #[test]
    fn move_changes_only_placement_and_track() {
        let (mut editor, _track, clip) = audio_editor();
        let destination = editor.create_track("Other", TrackKind::Audio).unwrap();
        let content = editor.state().clip(clip).unwrap().content.clone();
        editor.move_clip(clip, destination, Frame(-200)).unwrap();
        let moved = editor.state().clip(clip).unwrap();
        assert_eq!(moved.placement, range(-200, 1_000));
        assert_eq!(moved.content, content);
        assert_eq!(moved.track_id, destination);
    }

    #[test]
    fn audio_trim_and_slip_preserve_exact_mapping() {
        let (mut editor, _track, clip) = audio_editor();
        editor.trim_left(clip, Frame(300)).unwrap();
        editor.trim_right(clip, Frame(900)).unwrap();
        editor.slip_clip(clip, 50).unwrap();
        let edited = editor.state().clip(clip).unwrap();
        assert_eq!(edited.placement, range(300, 600));
        let ClipContent::Audio(audio) = &edited.content else {
            panic!()
        };
        assert_eq!(audio.source, source(750, 600));
    }

    #[test]
    fn reverse_audio_trims_from_opposite_source_edges() {
        let (mut editor, _track, clip) = audio_editor();
        let before = editor.state().clip(clip).unwrap().clone();
        let mut after = before.clone();
        let ClipContent::Audio(audio) = &mut after.content else {
            panic!()
        };
        audio.playback.reverse = true;
        editor.replace_clip("Reverse", before, after).unwrap();
        editor.trim_left(clip, Frame(300)).unwrap();
        editor.trim_right(clip, Frame(900)).unwrap();
        let ClipContent::Audio(audio) = &editor.state().clip(clip).unwrap().content else {
            panic!()
        };
        assert_eq!(audio.source, source(700, 600));
    }

    #[test]
    fn stretched_edits_refuse_fractional_source_boundaries() {
        let mut editor = ArrangementEditor::new(48_000).unwrap();
        let track = editor.create_track("Audio", TrackKind::Audio).unwrap();
        let clip = editor
            .create_audio_clip(
                track,
                "2x",
                range(0, 2_000),
                AssetId::from_raw(1),
                source(0, 1_000),
            )
            .unwrap();
        let before = editor.state().clone();
        assert_eq!(
            editor.trim_left(clip, Frame(1)),
            Err(ArrangementError::NonIntegralSourceBoundary)
        );
        assert_eq!(editor.state().clips, before.clips);
    }

    #[test]
    fn split_audio_concatenates_to_original_source() {
        let (mut editor, _track, clip) = audio_editor();
        let right = editor.split_clip(clip, Frame(450)).unwrap();
        let left = editor.state().clip(clip).unwrap();
        let right = editor.state().clip(right).unwrap();
        assert_eq!(left.placement, range(100, 350));
        assert_eq!(right.placement, range(450, 650));
        let (ClipContent::Audio(left), ClipContent::Audio(right)) = (&left.content, &right.content)
        else {
            panic!()
        };
        assert_eq!(left.source, source(500, 350));
        assert_eq!(right.source, source(850, 650));
    }

    #[test]
    fn split_preserves_fade_phases_through_cut() {
        let (mut editor, _track, clip) = audio_editor();
        editor
            .set_fades(
                clip,
                ClipFades {
                    fade_in: Some(Fade::full(600, FadeCurve::EqualPower)),
                    fade_out: None,
                },
            )
            .unwrap();
        let right = editor.split_clip(clip, Frame(400)).unwrap();
        let left_fade = editor.state().clip(clip).unwrap().fades.fade_in.unwrap();
        let right_fade = editor.state().clip(right).unwrap().fades.fade_in.unwrap();
        assert_eq!(left_fade.duration, 300);
        assert_eq!(right_fade.duration, 300);
        assert!((left_fade.phase_end - 0.5).abs() < 1e-12);
        assert_eq!(left_fade.phase_end, right_fade.phase_start);
    }

    #[test]
    fn duplicate_has_new_identity_but_shared_content_identity() {
        let (mut editor, _track, clip) = audio_editor();
        let duplicate = editor.duplicate_clip(clip, Frame(2_000)).unwrap();
        assert_ne!(clip, duplicate);
        assert_eq!(
            editor.state().clip(clip).unwrap().content,
            editor.state().clip(duplicate).unwrap().content
        );
    }

    #[test]
    fn stretch_resize_is_metadata_only_and_keeps_source() {
        let (mut editor, _track, clip) = audio_editor();
        editor
            .stretch_resize(clip, Frame(2_100), StretchAlgorithm::Granular, true)
            .unwrap();
        let ClipContent::Audio(audio) = &editor.state().clip(clip).unwrap().content else {
            panic!()
        };
        assert_eq!(audio.source, source(500, 1_000));
        assert_eq!(
            audio.playback.ratio,
            StretchRatio::new(1_000, 2_000).unwrap()
        );
        assert_eq!(audio.playback.algorithm, StretchAlgorithm::Granular);
    }

    #[test]
    fn rejecting_overlap_rolls_back_move() {
        let (mut editor, track, clip) = audio_editor();
        let other = editor
            .create_audio_clip(
                track,
                "other",
                range(2_000, 100),
                AssetId::from_raw(2),
                source(0, 100),
            )
            .unwrap();
        editor
            .set_overlap_policy(track, OverlapPolicy::Reject)
            .unwrap();
        let before = editor.state().clone();
        let error = editor.move_clip(other, track, Frame(150)).unwrap_err();
        assert!(
            matches!(error, ArrangementError::Overlap { first, second, .. } if first == clip && second == other)
        );
        assert_eq!(editor.state().clips, before.clips);
    }

    #[test]
    fn a_multi_entity_transaction_undoes_and_redoes_atomically() {
        let (mut editor, track, clip) = audio_editor();
        let right = editor.split_clip(clip, Frame(600)).unwrap();
        assert_eq!(editor.undo_label(), Some("Split clip"));
        editor.undo().unwrap();
        assert!(editor.state().clip(right).is_none());
        assert_eq!(
            editor.state().clip(clip).unwrap().placement,
            range(100, 1_000)
        );
        editor.redo().unwrap();
        assert!(editor.state().clip(right).is_some());
        assert_eq!(editor.state().track(track).unwrap().clip_ids.len(), 2);
    }

    #[test]
    fn selection_is_ephemeral_and_pruned_on_delete() {
        let (mut editor, track, clip) = audio_editor();
        editor.selection.clips.insert(clip);
        editor.selection.tracks.insert(track);
        editor.selection.time = Some(range(0, 20));
        let revision = editor.revision();
        editor.delete_clip(clip).unwrap();
        assert!(!editor.selection.clips.contains(&clip));
        assert!(editor.selection.tracks.contains(&track));
        assert_eq!(editor.selection.time, Some(range(0, 20)));
        assert!(editor.revision() > revision);
    }

    #[test]
    fn undo_does_not_reuse_ids() {
        let mut editor = ArrangementEditor::new(48_000).unwrap();
        let first = editor.create_track("one", TrackKind::Audio).unwrap();
        editor.undo().unwrap();
        let second = editor.create_track("two", TrackKind::Audio).unwrap();
        assert!(second.get() > first.get());
        assert!(editor.redo_label().is_none());
    }

    #[test]
    fn stale_external_transaction_cannot_clobber_newer_state() {
        let (mut editor, _track, clip) = audio_editor();
        let before = editor.state().clip(clip).unwrap().clone();
        let mut after = before.clone();
        after.name = "stale rename".into();
        let transaction = ArrangementTransaction::new(
            "Rename",
            vec![ArrangementOperation::PutClip {
                before: Some(before),
                after: Some(after),
            }],
        );
        editor
            .move_clip(
                clip,
                editor.state().clip(clip).unwrap().track_id,
                Frame(999),
            )
            .unwrap();
        assert_eq!(
            editor.apply(transaction),
            Err(ArrangementError::StaleOperation("clip"))
        );
    }

    #[test]
    fn delete_track_restores_children_and_order_on_undo() {
        let (mut editor, track, clip) = audio_editor();
        let second = editor.create_track("second", TrackKind::Group).unwrap();
        editor.delete_track(track).unwrap();
        assert!(editor.state().track(track).is_none());
        assert!(editor.state().clip(clip).is_none());
        editor.undo().unwrap();
        assert_eq!(editor.state().track_order, vec![track, second]);
        assert!(editor.state().clip(clip).is_some());
    }

    #[test]
    fn project_range_includes_negative_preroll() {
        let mut editor = ArrangementEditor::new(48_000).unwrap();
        let track = editor.create_track("hybrid", TrackKind::Hybrid).unwrap();
        editor
            .create_pattern_clip(track, "pre", range(-100, 50), PatternId::from_raw(3))
            .unwrap();
        editor
            .create_automation_clip(track, "post", range(400, 10), ParameterId::from_raw(7))
            .unwrap();
        assert_eq!(editor.state().project_range(), Some(range(-100, 510)));
    }

    #[test]
    fn track_reorder_and_marquee_selection_are_deterministic() {
        let mut editor = ArrangementEditor::new(48_000).unwrap();
        let first = editor.create_track("first", TrackKind::Pattern).unwrap();
        let second = editor.create_track("second", TrackKind::Pattern).unwrap();
        let a = editor
            .create_pattern_clip(first, "a", range(0, 100), PatternId::from_raw(1))
            .unwrap();
        let b = editor
            .create_pattern_clip(second, "b", range(100, 100), PatternId::from_raw(1))
            .unwrap();
        editor.reorder_track(second, 0).unwrap();
        assert_eq!(editor.state().track_order, vec![second, first]);

        editor.select_intersecting(range(50, 51), None);
        assert_eq!(editor.selection.clips, BTreeSet::from([a, b]));
        let only_first = BTreeSet::from([first]);
        editor.select_intersecting(range(50, 51), Some(&only_first));
        assert_eq!(editor.selection.clips, BTreeSet::from([a]));
    }

    #[test]
    fn split_fade_envelope_is_continuous() {
        let (mut editor, _track, clip) = audio_editor();
        editor
            .set_fades(
                clip,
                ClipFades {
                    fade_in: Some(Fade::full(600, FadeCurve::EqualPower)),
                    fade_out: Some(Fade::full(200, FadeCurve::SmoothStep)),
                },
            )
            .unwrap();
        let cut = 300;
        let before_gain = editor
            .state()
            .clip(clip)
            .unwrap()
            .fade_gain_at(cut)
            .unwrap();
        let right = editor.split_clip(clip, Frame(100 + cut as i64)).unwrap();
        let left_gain = editor
            .state()
            .clip(clip)
            .unwrap()
            .fade_gain_at(cut)
            .unwrap();
        let right_gain = editor.state().clip(right).unwrap().fade_gain_at(0).unwrap();
        assert!((before_gain - left_gain).abs() < 1e-12);
        assert!((before_gain - right_gain).abs() < 1e-12);
    }

    #[test]
    fn persistent_state_json_round_trips_deterministically() {
        let (mut editor, track, clip) = audio_editor();
        editor.move_clip(clip, track, Frame(-25)).unwrap();
        let encoded = serde_json::to_string(editor.state()).unwrap();
        let decoded: ArrangementState = serde_json::from_str(&encoded).unwrap();
        assert_eq!(&decoded, editor.state());
        assert_eq!(serde_json::to_string(&decoded).unwrap(), encoded);
        decoded.validate().unwrap();
    }

    #[test]
    fn audio_loop_and_channel_metadata_are_validated_atomically() {
        let (mut editor, _track, clip) = audio_editor();
        let before = editor.state().clip(clip).unwrap().clone();
        let mut after = before.clone();
        let ClipContent::Audio(audio) = &mut after.content else {
            panic!()
        };
        audio.channels = ChannelMapping::Channels(vec![0, 0]);
        audio.loop_mode = AudioLoopMode::Forward(source(0, 20));
        let transaction = ArrangementTransaction::new(
            "Invalid routing",
            vec![ArrangementOperation::PutClip {
                before: Some(before.clone()),
                after: Some(after),
            }],
        );
        assert!(matches!(
            editor.apply(transaction),
            Err(ArrangementError::InvalidAudioLoop) | Err(ArrangementError::InvalidChannelMapping)
        ));
        assert_eq!(editor.state().clip(clip), Some(&before));
    }

    #[test]
    fn no_op_does_not_create_history_or_dirty_project() {
        let (mut editor, _track, clip) = audio_editor();
        editor.mark_saved();
        let revision = editor.revision();
        let value = editor.state().clip(clip).unwrap().clone();
        let result = editor
            .apply(ArrangementTransaction::new(
                "No-op",
                vec![ArrangementOperation::PutClip {
                    before: Some(value.clone()),
                    after: Some(value),
                }],
            ))
            .unwrap();
        assert_eq!(result, revision);
        assert!(!editor.is_dirty());
        assert_ne!(editor.undo_label(), Some("No-op"));
    }
}
