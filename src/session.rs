//! UI-agnostic project/session state for a sample-accurate audio editor.
//!
//! [`Session`] deliberately separates ephemeral interaction state
//! ([`Transport`] and [`Selection`]) from revisioned [`Arrangement`] data.
//! Moving the playhead therefore never makes a project dirty, while every
//! arrangement command has an exact inverse and can be undone or redone.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::hash::Hash;

macro_rules! typed_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            pub const fn from_raw(raw: u64) -> Self {
                Self(raw)
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self(value)
            }
        }

        impl From<$name> for u64 {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

typed_id!(TrackId);
typed_id!(LaneId);
typed_id!(ClipId);
typed_id!(EventId);
typed_id!(ClusterId);

/// A signed PCM-frame coordinate on the project timeline.
///
/// Negative positions are valid, which is useful for preroll and dragging an
/// inferred event before the source origin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sample(pub i64);

impl Sample {
    pub const ZERO: Self = Self(0);

    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> i64 {
        self.0
    }

    pub fn saturating_add(self, samples: i64) -> Self {
        Self(self.0.saturating_add(samples))
    }

    pub fn saturating_sub(self, samples: i64) -> Self {
        Self(self.0.saturating_sub(samples))
    }
}

impl From<i64> for Sample {
    fn from(value: i64) -> Self {
        Self(value)
    }
}

impl From<Sample> for i64 {
    fn from(value: Sample) -> Self {
        value.0
    }
}

/// A normalized half-open timeline range: `start <= end` and `[start, end)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SampleRange {
    pub start: Sample,
    pub end: Sample,
}

impl SampleRange {
    /// Builds a normalized range, swapping the endpoints when necessary.
    pub fn new(a: impl Into<Sample>, b: impl Into<Sample>) -> Self {
        let a = a.into();
        let b = b.into();
        if a <= b {
            Self { start: a, end: b }
        } else {
            Self { start: b, end: a }
        }
    }

    pub const fn empty(at: Sample) -> Self {
        Self { start: at, end: at }
    }

    pub fn from_start_and_len(start: Sample, len: u64) -> Self {
        let delta = len.min(i64::MAX as u64) as i64;
        Self {
            start,
            end: start.saturating_add(delta),
        }
    }

    pub fn len(self) -> u64 {
        self.end.0.saturating_sub(self.start.0) as u64
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }

    pub fn contains(self, sample: Sample) -> bool {
        self.start <= sample && sample < self.end
    }

    pub fn contains_inclusive_end(self, sample: Sample) -> bool {
        self.start <= sample && sample <= self.end
    }

    pub fn intersects(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }

    pub fn intersection(self, other: Self) -> Option<Self> {
        let range = Self {
            start: self.start.max(other.start),
            end: self.end.min(other.end),
        };
        (!range.is_empty() && range.start < range.end).then_some(range)
    }

    pub fn translated(self, delta: i64) -> Self {
        Self {
            start: self.start.saturating_add(delta),
            end: self.end.saturating_add(delta),
        }
    }

    pub fn clamp(self, sample: Sample) -> Sample {
        sample.max(self.start).min(self.end)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TransportMode {
    #[default]
    Stopped,
    Playing,
    Recording,
}

/// Runtime playback state. Mutating it does not affect project revisions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transport {
    playhead: Sample,
    mode: TransportMode,
    loop_range: Option<SampleRange>,
    loop_enabled: bool,
}

impl Default for Transport {
    fn default() -> Self {
        Self {
            playhead: Sample::ZERO,
            mode: TransportMode::Stopped,
            loop_range: None,
            loop_enabled: false,
        }
    }
}

impl Transport {
    pub fn playhead(&self) -> Sample {
        self.playhead
    }

    pub fn seek(&mut self, sample: impl Into<Sample>) {
        self.playhead = sample.into();
    }

    pub fn mode(&self) -> TransportMode {
        self.mode
    }

    pub fn is_rolling(&self) -> bool {
        self.mode != TransportMode::Stopped
    }

    pub fn play(&mut self) {
        self.mode = TransportMode::Playing;
    }

    pub fn record(&mut self) {
        self.mode = TransportMode::Recording;
    }

    pub fn stop(&mut self) {
        self.mode = TransportMode::Stopped;
    }

    pub fn loop_range(&self) -> Option<SampleRange> {
        self.loop_range
    }

    pub fn set_loop_range(&mut self, range: Option<SampleRange>) {
        self.loop_range = range.filter(|range| !range.is_empty());
    }

    pub fn loop_enabled(&self) -> bool {
        self.loop_enabled
    }

    pub fn set_loop_enabled(&mut self, enabled: bool) {
        self.loop_enabled = enabled;
    }

    pub fn active_loop(&self) -> Option<SampleRange> {
        self.loop_enabled.then_some(self.loop_range).flatten()
    }

    /// Advances by a non-negative number of samples, wrapping within an active
    /// loop. Seeking and advancing are intentionally independent of `mode`, so
    /// an audio callback can decide when transport time should move.
    pub fn advance(&mut self, samples: u64) -> Sample {
        let delta = samples.min(i64::MAX as u64) as i64;
        let target = self.playhead.saturating_add(delta);
        self.playhead = match self.active_loop() {
            Some(loop_range) if target >= loop_range.end => {
                let len = loop_range.len() as i128;
                let offset = (target.0 as i128 - loop_range.start.0 as i128).rem_euclid(len);
                Sample(loop_range.start.0.saturating_add(offset as i64))
            }
            _ => target,
        };
        self.playhead
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QuantizeDirection {
    Previous,
    #[default]
    Nearest,
    Next,
}

/// A fixed, sample-coordinate snapping grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapGrid {
    enabled: bool,
    spacing: u64,
    origin: Sample,
}

impl Default for SnapGrid {
    fn default() -> Self {
        Self {
            enabled: false,
            spacing: 1,
            origin: Sample::ZERO,
        }
    }
}

impl SnapGrid {
    pub fn new(spacing: u64) -> Result<Self, SessionError> {
        if spacing == 0 {
            return Err(SessionError::InvalidGridSpacing);
        }
        Ok(Self {
            enabled: true,
            spacing,
            origin: Sample::ZERO,
        })
    }

    pub fn enabled(self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn spacing(self) -> u64 {
        self.spacing
    }

    pub fn set_spacing(&mut self, spacing: u64) -> Result<(), SessionError> {
        if spacing == 0 {
            return Err(SessionError::InvalidGridSpacing);
        }
        self.spacing = spacing;
        Ok(())
    }

    pub fn origin(self) -> Sample {
        self.origin
    }

    pub fn set_origin(&mut self, origin: Sample) {
        self.origin = origin;
    }

    /// Quantizes a sample. Disabled grids return the input unchanged.
    /// Nearest ties resolve toward the later grid point.
    pub fn quantize(self, sample: Sample, direction: QuantizeDirection) -> Sample {
        if !self.enabled {
            return sample;
        }
        let spacing = self.spacing as i128;
        let relative = sample.0 as i128 - self.origin.0 as i128;
        let previous_step = relative.div_euclid(spacing);
        let remainder = relative.rem_euclid(spacing);
        let step = match direction {
            QuantizeDirection::Previous => previous_step,
            QuantizeDirection::Next => previous_step + i128::from(remainder != 0),
            QuantizeDirection::Nearest => previous_step + i128::from(remainder * 2 >= spacing),
        };
        let quantized = self.origin.0 as i128 + step * spacing;
        Sample(quantized.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
    }

    /// Snaps a drag target and returns the resulting delta from its anchor.
    pub fn quantize_delta(
        self,
        anchor: Sample,
        proposed_delta: i64,
        direction: QuantizeDirection,
    ) -> i64 {
        let target = anchor.saturating_add(proposed_delta);
        self.quantize(target, direction).0.saturating_sub(anchor.0)
    }

    pub fn quantize_range(
        self,
        range: SampleRange,
        start_direction: QuantizeDirection,
        end_direction: QuantizeDirection,
    ) -> SampleRange {
        SampleRange::new(
            self.quantize(range.start, start_direction),
            self.quantize(range.end, end_direction),
        )
    }
}

/// Ephemeral editor selection, including both time and object selection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub time: Option<SampleRange>,
    pub tracks: BTreeSet<TrackId>,
    pub clips: BTreeSet<ClipId>,
    pub events: BTreeSet<EventId>,
    pub clusters: BTreeSet<ClusterId>,
}

impl Selection {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn is_empty(&self) -> bool {
        self.time.is_none()
            && self.tracks.is_empty()
            && self.clips.is_empty()
            && self.events.is_empty()
            && self.clusters.is_empty()
    }

    pub fn set_time(&mut self, range: Option<SampleRange>) {
        self.time = range.filter(|range| !range.is_empty());
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrackKind {
    #[default]
    Audio,
    Events,
    Group,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    pub id: TrackId,
    pub name: String,
    pub kind: TrackKind,
    pub lane_ids: Vec<LaneId>,
    pub gain: f32,
    pub muted: bool,
    pub solo: bool,
}

impl Track {
    pub fn new(id: TrackId, name: impl Into<String>, kind: TrackKind) -> Self {
        Self {
            id,
            name: name.into(),
            kind,
            lane_ids: Vec::new(),
            gain: 1.0,
            muted: false,
            solo: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Lane {
    pub id: LaneId,
    pub track_id: TrackId,
    pub name: String,
    pub clip_ids: Vec<ClipId>,
}

impl Lane {
    pub fn new(id: LaneId, track_id: TrackId, name: impl Into<String>) -> Self {
        Self {
            id,
            track_id,
            name: name.into(),
            clip_ids: Vec::new(),
        }
    }
}

/// A non-destructive reference to a span of source audio.
#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
    pub id: ClipId,
    pub lane_id: LaneId,
    pub name: String,
    pub timeline: SampleRange,
    pub source_start: u64,
    pub gain: f32,
    pub muted: bool,
    pub locked: bool,
}

impl Clip {
    pub fn new(
        id: ClipId,
        lane_id: LaneId,
        name: impl Into<String>,
        timeline: SampleRange,
    ) -> Self {
        Self {
            id,
            lane_id,
            name: name.into(),
            timeline,
            source_start: 0,
            gain: 1.0,
            muted: false,
            locked: false,
        }
    }
}

/// Editable controls shared by recurring events.
#[derive(Clone, Debug, PartialEq)]
pub struct Cluster {
    pub id: ClusterId,
    pub name: String,
    pub gain: f32,
    pub muted: bool,
}

impl Cluster {
    pub fn new(id: ClusterId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            gain: 1.0,
            muted: false,
        }
    }
}

/// An editable occurrence referring to a reusable [`Cluster`].
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub id: EventId,
    pub lane_id: LaneId,
    pub cluster_id: ClusterId,
    pub sample: Sample,
    pub duration: u64,
    pub gain: f32,
    pub muted: bool,
}

impl Event {
    pub fn new(id: EventId, lane_id: LaneId, cluster_id: ClusterId, sample: Sample) -> Self {
        Self {
            id,
            lane_id,
            cluster_id,
            sample,
            duration: 0,
            gain: 1.0,
            muted: false,
        }
    }

    pub fn range(&self) -> SampleRange {
        SampleRange::from_start_and_len(self.sample, self.duration)
    }
}

/// The persistent, revisioned part of a project.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Arrangement {
    tracks: BTreeMap<TrackId, Track>,
    lanes: BTreeMap<LaneId, Lane>,
    clips: BTreeMap<ClipId, Clip>,
    clusters: BTreeMap<ClusterId, Cluster>,
    events: BTreeMap<EventId, Event>,
}

impl Arrangement {
    pub fn tracks(&self) -> impl ExactSizeIterator<Item = &Track> {
        self.tracks.values()
    }

    pub fn lanes(&self) -> impl ExactSizeIterator<Item = &Lane> {
        self.lanes.values()
    }

    pub fn clips(&self) -> impl ExactSizeIterator<Item = &Clip> {
        self.clips.values()
    }

    pub fn clusters(&self) -> impl ExactSizeIterator<Item = &Cluster> {
        self.clusters.values()
    }

    pub fn events(&self) -> impl ExactSizeIterator<Item = &Event> {
        self.events.values()
    }

    pub fn track(&self, id: TrackId) -> Option<&Track> {
        self.tracks.get(&id)
    }

    pub fn lane(&self, id: LaneId) -> Option<&Lane> {
        self.lanes.get(&id)
    }

    pub fn clip(&self, id: ClipId) -> Option<&Clip> {
        self.clips.get(&id)
    }

    pub fn cluster(&self, id: ClusterId) -> Option<&Cluster> {
        self.clusters.get(&id)
    }

    pub fn event(&self, id: EventId) -> Option<&Event> {
        self.events.get(&id)
    }

    pub fn clips_in_lane(&self, lane: LaneId) -> impl Iterator<Item = &Clip> {
        self.lane(lane)
            .into_iter()
            .flat_map(|lane| lane.clip_ids.iter())
            .filter_map(|id| self.clip(*id))
    }

    pub fn events_in_lane(&self, lane: LaneId) -> impl Iterator<Item = &Event> {
        self.events
            .values()
            .filter(move |event| event.lane_id == lane)
    }

    pub fn events_in_range(&self, range: SampleRange) -> impl Iterator<Item = &Event> {
        self.events.values().filter(move |event| {
            if event.duration == 0 {
                range.contains(event.sample)
            } else {
                event.range().intersects(range)
            }
        })
    }

    pub fn project_end(&self) -> Sample {
        let clip_end = self.clips.values().map(|clip| clip.timeline.end).max();
        let event_end = self
            .events
            .values()
            .map(|event| event.range().end.max(event.sample))
            .max();
        clip_end
            .into_iter()
            .chain(event_end)
            .max()
            .unwrap_or(Sample::ZERO)
    }

    pub fn validate(&self) -> Result<(), SessionError> {
        for (id, track) in &self.tracks {
            if *id != track.id || !valid_gain(track.gain) {
                return Err(SessionError::InvalidEntity("track"));
            }
            for lane_id in &track.lane_ids {
                let lane = self
                    .lanes
                    .get(lane_id)
                    .ok_or(SessionError::MissingLane(*lane_id))?;
                if lane.track_id != *id {
                    return Err(SessionError::InvalidEntity("lane ownership"));
                }
            }
        }
        for (id, lane) in &self.lanes {
            if *id != lane.id || !self.tracks.contains_key(&lane.track_id) {
                return Err(SessionError::InvalidEntity("lane"));
            }
            let owner = &self.tracks[&lane.track_id];
            if owner
                .lane_ids
                .iter()
                .filter(|candidate| *candidate == id)
                .count()
                != 1
            {
                return Err(SessionError::InvalidEntity("track lane index"));
            }
            for clip_id in &lane.clip_ids {
                let clip = self
                    .clips
                    .get(clip_id)
                    .ok_or(SessionError::MissingClip(*clip_id))?;
                if clip.lane_id != *id {
                    return Err(SessionError::InvalidEntity("clip ownership"));
                }
            }
        }
        for (id, clip) in &self.clips {
            if *id != clip.id || clip.timeline.is_empty() || !valid_gain(clip.gain) {
                return Err(SessionError::InvalidEntity("clip"));
            }
            let lane = self
                .lanes
                .get(&clip.lane_id)
                .ok_or(SessionError::MissingLane(clip.lane_id))?;
            if lane
                .clip_ids
                .iter()
                .filter(|candidate| *candidate == id)
                .count()
                != 1
            {
                return Err(SessionError::InvalidEntity("lane clip index"));
            }
        }
        for (id, cluster) in &self.clusters {
            if *id != cluster.id || !valid_gain(cluster.gain) {
                return Err(SessionError::InvalidEntity("cluster"));
            }
        }
        for (id, event) in &self.events {
            if *id != event.id || !valid_gain(event.gain) {
                return Err(SessionError::InvalidEntity("event"));
            }
            if !self.lanes.contains_key(&event.lane_id) {
                return Err(SessionError::MissingLane(event.lane_id));
            }
            if !self.clusters.contains_key(&event.cluster_id) {
                return Err(SessionError::MissingCluster(event.cluster_id));
            }
        }
        Ok(())
    }
}

fn valid_gain(gain: f32) -> bool {
    gain.is_finite() && gain >= 0.0
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectRevision(u64);

impl ProjectRevision {
    pub const INITIAL: Self = Self(0);

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// An exact entity replacement. `None -> Some` inserts, `Some -> None`
/// removes, and `Some -> Some` edits non-destructively.
#[derive(Clone, Debug, PartialEq)]
pub enum ProjectChange {
    Track {
        before: Option<Track>,
        after: Option<Track>,
    },
    Lane {
        before: Option<Lane>,
        after: Option<Lane>,
    },
    Clip {
        before: Option<Clip>,
        after: Option<Clip>,
    },
    Cluster {
        before: Option<Cluster>,
        after: Option<Cluster>,
    },
    Event {
        before: Option<Event>,
        after: Option<Event>,
    },
}

impl ProjectChange {
    pub fn event(before: Event, after: Event) -> Self {
        Self::Event {
            before: Some(before),
            after: Some(after),
        }
    }

    pub fn cluster(before: Cluster, after: Cluster) -> Self {
        Self::Cluster {
            before: Some(before),
            after: Some(after),
        }
    }

    fn is_noop(&self) -> bool {
        match self {
            Self::Track { before, after } => before == after,
            Self::Lane { before, after } => before == after,
            Self::Clip { before, after } => before == after,
            Self::Cluster { before, after } => before == after,
            Self::Event { before, after } => before == after,
        }
    }

    fn apply(&self, arrangement: &mut Arrangement, forward: bool) -> Result<(), SessionError> {
        macro_rules! replace {
            ($map:expr, $before:expr, $after:expr, $id:expr, $kind:literal) => {{
                let (expected, replacement) = if forward {
                    ($before, $after)
                } else {
                    ($after, $before)
                };
                if $map.get(&$id) != expected.as_ref() {
                    return Err(SessionError::CommandConflict($kind));
                }
                match replacement {
                    Some(value) => {
                        $map.insert($id, value.clone());
                    }
                    None => {
                        $map.remove(&$id);
                    }
                }
            }};
        }
        match self {
            Self::Track { before, after } => {
                let id = option_id(
                    before.as_ref().map(|v| v.id),
                    after.as_ref().map(|v| v.id),
                    "track",
                )?;
                replace!(arrangement.tracks, before, after, id, "track");
            }
            Self::Lane { before, after } => {
                let id = option_id(
                    before.as_ref().map(|v| v.id),
                    after.as_ref().map(|v| v.id),
                    "lane",
                )?;
                replace!(arrangement.lanes, before, after, id, "lane");
            }
            Self::Clip { before, after } => {
                let id = option_id(
                    before.as_ref().map(|v| v.id),
                    after.as_ref().map(|v| v.id),
                    "clip",
                )?;
                replace!(arrangement.clips, before, after, id, "clip");
            }
            Self::Cluster { before, after } => {
                let id = option_id(
                    before.as_ref().map(|v| v.id),
                    after.as_ref().map(|v| v.id),
                    "cluster",
                )?;
                replace!(arrangement.clusters, before, after, id, "cluster");
            }
            Self::Event { before, after } => {
                let id = option_id(
                    before.as_ref().map(|v| v.id),
                    after.as_ref().map(|v| v.id),
                    "event",
                )?;
                replace!(arrangement.events, before, after, id, "event");
            }
        }
        Ok(())
    }
}

fn option_id<T: Copy + Eq>(
    before: Option<T>,
    after: Option<T>,
    kind: &'static str,
) -> Result<T, SessionError> {
    match (before, after) {
        (Some(a), Some(b)) if a == b => Ok(a),
        (Some(a), None) | (None, Some(a)) => Ok(a),
        _ => Err(SessionError::InvalidCommand(kind)),
    }
}

/// A labelled atomic command suitable for an Undo menu or toast.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionCommand {
    pub label: String,
    pub changes: Vec<ProjectChange>,
}

impl SessionCommand {
    pub fn new(label: impl Into<String>, changes: Vec<ProjectChange>) -> Self {
        Self {
            label: label.into(),
            changes,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.changes.iter().all(ProjectChange::is_noop)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandOutcome {
    pub changed: bool,
    pub revision: ProjectRevision,
}

#[derive(Clone, Debug)]
struct HistoryEntry {
    command: SessionCommand,
    before_revision: ProjectRevision,
    after_revision: ProjectRevision,
}

#[derive(Clone, Debug)]
struct IdAllocator {
    next_track: u64,
    next_lane: u64,
    next_clip: u64,
    next_event: u64,
    next_cluster: u64,
}

impl Default for IdAllocator {
    fn default() -> Self {
        Self {
            next_track: 1,
            next_lane: 1,
            next_clip: 1,
            next_event: 1,
            next_cluster: 1,
        }
    }
}

/// Complete editor session state. This type has no UI or audio dependencies.
#[derive(Clone, Debug)]
pub struct Session {
    sample_rate: u32,
    pub transport: Transport,
    pub selection: Selection,
    pub snap: SnapGrid,
    arrangement: Arrangement,
    ids: IdAllocator,
    revision: ProjectRevision,
    saved_revision: ProjectRevision,
    next_revision: u64,
    undo: VecDeque<HistoryEntry>,
    redo: Vec<HistoryEntry>,
    history_limit: usize,
}

impl Session {
    pub fn new(sample_rate: u32) -> Result<Self, SessionError> {
        if sample_rate == 0 {
            return Err(SessionError::InvalidSampleRate);
        }
        Ok(Self {
            sample_rate,
            transport: Transport::default(),
            selection: Selection::default(),
            snap: SnapGrid::default(),
            arrangement: Arrangement::default(),
            ids: IdAllocator::default(),
            revision: ProjectRevision::INITIAL,
            saved_revision: ProjectRevision::INITIAL,
            next_revision: 1,
            undo: VecDeque::new(),
            redo: Vec::new(),
            history_limit: 256,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn arrangement(&self) -> &Arrangement {
        &self.arrangement
    }

    pub fn revision(&self) -> ProjectRevision {
        self.revision
    }

    pub fn saved_revision(&self) -> ProjectRevision {
        self.saved_revision
    }

    pub fn is_dirty(&self) -> bool {
        self.revision != self.saved_revision
    }

    pub fn mark_saved(&mut self) {
        self.saved_revision = self.revision;
    }

    pub fn history_limit(&self) -> usize {
        self.history_limit
    }

    pub fn set_history_limit(&mut self, limit: usize) {
        self.history_limit = limit;
        while self.undo.len() > limit {
            self.undo.pop_front();
        }
        if self.redo.len() > limit {
            // The end of this vector is the next command to redo.
            self.redo.drain(..self.redo.len() - limit);
        }
        if limit == 0 {
            self.undo.clear();
            self.redo.clear();
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo_label(&self) -> Option<&str> {
        self.undo.back().map(|entry| entry.command.label.as_str())
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|entry| entry.command.label.as_str())
    }

    pub fn execute(&mut self, command: SessionCommand) -> Result<CommandOutcome, SessionError> {
        if command.is_empty() {
            return Ok(CommandOutcome {
                changed: false,
                revision: self.revision,
            });
        }
        let before = self.arrangement.clone();
        if let Err(error) = apply_changes(&command.changes, &mut self.arrangement, true) {
            self.arrangement = before;
            return Err(error);
        }
        if let Err(error) = self.arrangement.validate() {
            self.arrangement = before;
            return Err(error);
        }
        let before_revision = self.revision;
        let after_revision = self.allocate_revision();
        self.revision = after_revision;
        self.redo.clear();
        if self.history_limit > 0 {
            self.undo.push_back(HistoryEntry {
                command,
                before_revision,
                after_revision,
            });
            while self.undo.len() > self.history_limit {
                self.undo.pop_front();
            }
        }
        Ok(CommandOutcome {
            changed: true,
            revision: self.revision,
        })
    }

    pub fn undo(&mut self) -> Result<Option<ProjectRevision>, SessionError> {
        let Some(entry) = self.undo.pop_back() else {
            return Ok(None);
        };
        let before = self.arrangement.clone();
        if let Err(error) = apply_changes(&entry.command.changes, &mut self.arrangement, false)
            .and_then(|_| self.arrangement.validate())
        {
            self.arrangement = before;
            self.undo.push_back(entry);
            return Err(error);
        }
        self.revision = entry.before_revision;
        self.redo.push(entry);
        Ok(Some(self.revision))
    }

    pub fn redo(&mut self) -> Result<Option<ProjectRevision>, SessionError> {
        let Some(entry) = self.redo.pop() else {
            return Ok(None);
        };
        let before = self.arrangement.clone();
        if let Err(error) = apply_changes(&entry.command.changes, &mut self.arrangement, true)
            .and_then(|_| self.arrangement.validate())
        {
            self.arrangement = before;
            self.redo.push(entry);
            return Err(error);
        }
        self.revision = entry.after_revision;
        self.undo.push_back(entry);
        while self.undo.len() > self.history_limit {
            self.undo.pop_front();
        }
        Ok(Some(self.revision))
    }

    pub fn create_track(
        &mut self,
        name: impl Into<String>,
        kind: TrackKind,
    ) -> Result<TrackId, SessionError> {
        let id = TrackId(self.take_id(IdKind::Track));
        let track = Track::new(id, name, kind);
        self.execute(SessionCommand::new(
            "Create track",
            vec![ProjectChange::Track {
                before: None,
                after: Some(track),
            }],
        ))?;
        Ok(id)
    }

    pub fn create_lane(
        &mut self,
        track_id: TrackId,
        name: impl Into<String>,
    ) -> Result<LaneId, SessionError> {
        let before_track = self
            .arrangement
            .track(track_id)
            .cloned()
            .ok_or(SessionError::MissingTrack(track_id))?;
        let id = LaneId(self.take_id(IdKind::Lane));
        let mut after_track = before_track.clone();
        after_track.lane_ids.push(id);
        let lane = Lane::new(id, track_id, name);
        self.execute(SessionCommand::new(
            "Create lane",
            vec![
                ProjectChange::Track {
                    before: Some(before_track),
                    after: Some(after_track),
                },
                ProjectChange::Lane {
                    before: None,
                    after: Some(lane),
                },
            ],
        ))?;
        Ok(id)
    }

    pub fn create_clip(
        &mut self,
        lane_id: LaneId,
        name: impl Into<String>,
        timeline: SampleRange,
    ) -> Result<ClipId, SessionError> {
        if timeline.is_empty() {
            return Err(SessionError::InvalidEntity("clip"));
        }
        let before_lane = self
            .arrangement
            .lane(lane_id)
            .cloned()
            .ok_or(SessionError::MissingLane(lane_id))?;
        let id = ClipId(self.take_id(IdKind::Clip));
        let mut after_lane = before_lane.clone();
        after_lane.clip_ids.push(id);
        let clip = Clip::new(id, lane_id, name, timeline);
        self.execute(SessionCommand::new(
            "Create clip",
            vec![
                ProjectChange::Lane {
                    before: Some(before_lane),
                    after: Some(after_lane),
                },
                ProjectChange::Clip {
                    before: None,
                    after: Some(clip),
                },
            ],
        ))?;
        Ok(id)
    }

    pub fn create_cluster(&mut self, name: impl Into<String>) -> Result<ClusterId, SessionError> {
        let id = ClusterId(self.take_id(IdKind::Cluster));
        let cluster = Cluster::new(id, name);
        self.execute(SessionCommand::new(
            "Create cluster",
            vec![ProjectChange::Cluster {
                before: None,
                after: Some(cluster),
            }],
        ))?;
        Ok(id)
    }

    pub fn create_event(
        &mut self,
        lane_id: LaneId,
        cluster_id: ClusterId,
        sample: Sample,
    ) -> Result<EventId, SessionError> {
        if self.arrangement.lane(lane_id).is_none() {
            return Err(SessionError::MissingLane(lane_id));
        }
        if self.arrangement.cluster(cluster_id).is_none() {
            return Err(SessionError::MissingCluster(cluster_id));
        }
        let id = EventId(self.take_id(IdKind::Event));
        let event = Event::new(id, lane_id, cluster_id, sample);
        self.execute(SessionCommand::new(
            "Create event",
            vec![ProjectChange::Event {
                before: None,
                after: Some(event),
            }],
        ))?;
        Ok(id)
    }

    /// Builds, validates, and commits one event edit as an undoable command.
    pub fn edit_event<F>(
        &mut self,
        id: EventId,
        label: impl Into<String>,
        edit: F,
    ) -> Result<CommandOutcome, SessionError>
    where
        F: FnOnce(&mut Event),
    {
        let before = self
            .arrangement
            .event(id)
            .cloned()
            .ok_or(SessionError::MissingEvent(id))?;
        let mut after = before.clone();
        edit(&mut after);
        if after.id != id {
            return Err(SessionError::InvalidCommand("event id changed"));
        }
        self.execute(SessionCommand::new(
            label,
            vec![ProjectChange::event(before, after)],
        ))
    }

    /// Commits one atomic multi-event edit (for example a multi-selection drag).
    pub fn edit_events<I, F>(
        &mut self,
        ids: I,
        label: impl Into<String>,
        mut edit: F,
    ) -> Result<CommandOutcome, SessionError>
    where
        I: IntoIterator<Item = EventId>,
        F: FnMut(&mut Event),
    {
        let mut unique = BTreeSet::new();
        let mut changes = Vec::new();
        for id in ids {
            if !unique.insert(id) {
                continue;
            }
            let before = self
                .arrangement
                .event(id)
                .cloned()
                .ok_or(SessionError::MissingEvent(id))?;
            let mut after = before.clone();
            edit(&mut after);
            if after.id != id {
                return Err(SessionError::InvalidCommand("event id changed"));
            }
            changes.push(ProjectChange::event(before, after));
        }
        self.execute(SessionCommand::new(label, changes))
    }

    pub fn edit_cluster<F>(
        &mut self,
        id: ClusterId,
        label: impl Into<String>,
        edit: F,
    ) -> Result<CommandOutcome, SessionError>
    where
        F: FnOnce(&mut Cluster),
    {
        let before = self
            .arrangement
            .cluster(id)
            .cloned()
            .ok_or(SessionError::MissingCluster(id))?;
        let mut after = before.clone();
        edit(&mut after);
        if after.id != id {
            return Err(SessionError::InvalidCommand("cluster id changed"));
        }
        self.execute(SessionCommand::new(
            label,
            vec![ProjectChange::cluster(before, after)],
        ))
    }

    pub fn command_for_event<F>(
        &self,
        id: EventId,
        label: impl Into<String>,
        edit: F,
    ) -> Result<SessionCommand, SessionError>
    where
        F: FnOnce(&mut Event),
    {
        let before = self
            .arrangement
            .event(id)
            .cloned()
            .ok_or(SessionError::MissingEvent(id))?;
        let mut after = before.clone();
        edit(&mut after);
        if after.id != id {
            return Err(SessionError::InvalidCommand("event id changed"));
        }
        Ok(SessionCommand::new(
            label,
            vec![ProjectChange::event(before, after)],
        ))
    }

    fn allocate_revision(&mut self) -> ProjectRevision {
        let revision = ProjectRevision(self.next_revision);
        self.next_revision = self.next_revision.saturating_add(1);
        revision
    }

    fn take_id(&mut self, kind: IdKind) -> u64 {
        let next = match kind {
            IdKind::Track => &mut self.ids.next_track,
            IdKind::Lane => &mut self.ids.next_lane,
            IdKind::Clip => &mut self.ids.next_clip,
            IdKind::Event => &mut self.ids.next_event,
            IdKind::Cluster => &mut self.ids.next_cluster,
        };
        let value = *next;
        *next = next.saturating_add(1);
        value
    }
}

#[derive(Clone, Copy)]
enum IdKind {
    Track,
    Lane,
    Clip,
    Event,
    Cluster,
}

fn apply_changes(
    changes: &[ProjectChange],
    arrangement: &mut Arrangement,
    forward: bool,
) -> Result<(), SessionError> {
    if forward {
        for change in changes {
            change.apply(arrangement, true)?;
        }
    } else {
        for change in changes.iter().rev() {
            change.apply(arrangement, false)?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionError {
    InvalidSampleRate,
    InvalidGridSpacing,
    InvalidEntity(&'static str),
    InvalidCommand(&'static str),
    CommandConflict(&'static str),
    MissingTrack(TrackId),
    MissingLane(LaneId),
    MissingClip(ClipId),
    MissingCluster(ClusterId),
    MissingEvent(EventId),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSampleRate => f.write_str("sample rate must be non-zero"),
            Self::InvalidGridSpacing => f.write_str("snap grid spacing must be non-zero"),
            Self::InvalidEntity(kind) => write!(f, "invalid {kind}"),
            Self::InvalidCommand(reason) => write!(f, "invalid command: {reason}"),
            Self::CommandConflict(kind) => write!(f, "command no longer matches current {kind}"),
            Self::MissingTrack(id) => write!(f, "track {id} does not exist"),
            Self::MissingLane(id) => write!(f, "lane {id} does not exist"),
            Self::MissingClip(id) => write!(f, "clip {id} does not exist"),
            Self::MissingCluster(id) => write!(f, "cluster {id} does not exist"),
            Self::MissingEvent(id) => write!(f, "event {id} does not exist"),
        }
    }
}

impl Error for SessionError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn populated_session() -> (Session, TrackId, LaneId, ClusterId, EventId) {
        let mut session = Session::new(48_000).unwrap();
        let track = session.create_track("Inferred", TrackKind::Events).unwrap();
        let lane = session.create_lane(track, "Main").unwrap();
        let cluster = session.create_cluster("Kick-ish").unwrap();
        let event = session.create_event(lane, cluster, Sample(12_000)).unwrap();
        (session, track, lane, cluster, event)
    }

    #[test]
    fn sample_ranges_are_normalized_half_open_and_saturating() {
        let range = SampleRange::new(20, -10);
        assert_eq!(
            range,
            SampleRange {
                start: Sample(-10),
                end: Sample(20)
            }
        );
        assert!(range.contains(Sample(-10)));
        assert!(!range.contains(Sample(20)));
        assert_eq!(range.len(), 30);
        assert_eq!(
            SampleRange::from_start_and_len(Sample(i64::MAX - 2), 20).end,
            Sample(i64::MAX)
        );
        assert_eq!(
            range.intersection(SampleRange::new(0, 30)),
            Some(SampleRange::new(0, 20))
        );
    }

    #[test]
    fn transport_wraps_active_loop_and_playhead_is_independent_of_mode() {
        let mut transport = Transport::default();
        transport.set_loop_range(Some(SampleRange::new(100, 200)));
        transport.set_loop_enabled(true);
        transport.seek(190);
        assert_eq!(transport.advance(25), Sample(115));
        assert_eq!(transport.mode(), TransportMode::Stopped);
        transport.play();
        assert!(transport.is_rolling());
        transport.stop();
        assert!(!transport.is_rolling());
    }

    #[test]
    fn disabled_or_empty_loop_does_not_wrap() {
        let mut transport = Transport::default();
        transport.set_loop_range(Some(SampleRange::empty(Sample(2))));
        transport.set_loop_enabled(true);
        transport.seek(10);
        assert_eq!(transport.advance(5), Sample(15));
        assert_eq!(transport.active_loop(), None);
    }

    #[test]
    fn snap_grid_handles_negative_coordinates_and_ties() {
        let mut grid = SnapGrid::new(10).unwrap();
        grid.set_origin(Sample(3));
        assert_eq!(
            grid.quantize(Sample(-4), QuantizeDirection::Previous),
            Sample(-7)
        );
        assert_eq!(
            grid.quantize(Sample(-4), QuantizeDirection::Next),
            Sample(3)
        );
        assert_eq!(
            grid.quantize(Sample(-2), QuantizeDirection::Nearest),
            Sample(3)
        );
        assert_eq!(
            grid.quantize_delta(Sample(8), 8, QuantizeDirection::Nearest),
            5
        );
        grid.set_enabled(false);
        assert_eq!(
            grid.quantize(Sample(-4), QuantizeDirection::Nearest),
            Sample(-4)
        );
    }

    #[test]
    fn selection_and_transport_do_not_dirty_project() {
        let mut session = Session::new(44_100).unwrap();
        session.transport.seek(1234);
        session.selection.set_time(Some(SampleRange::new(10, 20)));
        session.snap = SnapGrid::new(128).unwrap();
        assert_eq!(session.revision(), ProjectRevision::INITIAL);
        assert!(!session.is_dirty());
    }

    #[test]
    fn creates_a_valid_basic_arrangement() {
        let (mut session, track, lane, cluster, event) = populated_session();
        let clip = session
            .create_clip(lane, "Source", SampleRange::new(0, 48_000))
            .unwrap();
        assert_eq!(
            session.arrangement().track(track).unwrap().lane_ids,
            vec![lane]
        );
        assert_eq!(
            session.arrangement().lane(lane).unwrap().clip_ids,
            vec![clip]
        );
        assert_eq!(
            session.arrangement().event(event).unwrap().cluster_id,
            cluster
        );
        assert_eq!(session.arrangement().project_end(), Sample(48_000));
        session.arrangement().validate().unwrap();
    }

    #[test]
    fn event_edit_is_atomic_reversible_and_redoable() {
        let (mut session, _, _, _, event) = populated_session();
        let before_revision = session.revision();
        let outcome = session
            .edit_event(event, "Move and fade event", |event| {
                event.sample = Sample(24_000);
                event.gain = 0.5;
                event.muted = true;
            })
            .unwrap();
        assert!(outcome.changed);
        let edited_revision = outcome.revision;
        assert_eq!(session.undo_label(), Some("Move and fade event"));
        assert_eq!(
            session.arrangement().event(event).unwrap().sample,
            Sample(24_000)
        );

        assert_eq!(session.undo().unwrap(), Some(before_revision));
        let restored = session.arrangement().event(event).unwrap();
        assert_eq!(restored.sample, Sample(12_000));
        assert_eq!(restored.gain, 1.0);
        assert!(!restored.muted);
        assert_eq!(session.redo_label(), Some("Move and fade event"));

        assert_eq!(session.redo().unwrap(), Some(edited_revision));
        assert_eq!(session.arrangement().event(event).unwrap().gain, 0.5);
    }

    #[test]
    fn multi_event_edit_is_one_history_step_and_deduplicates_ids() {
        let (mut session, _, lane, cluster, first) = populated_session();
        let second = session.create_event(lane, cluster, Sample(20_000)).unwrap();
        session
            .edit_events([first, second, first], "Nudge events", |event| {
                event.sample = event.sample.saturating_add(100);
            })
            .unwrap();
        assert_eq!(
            session.arrangement().event(first).unwrap().sample,
            Sample(12_100)
        );
        assert_eq!(
            session.arrangement().event(second).unwrap().sample,
            Sample(20_100)
        );
        session.undo().unwrap();
        assert_eq!(
            session.arrangement().event(first).unwrap().sample,
            Sample(12_000)
        );
        assert_eq!(
            session.arrangement().event(second).unwrap().sample,
            Sample(20_000)
        );
    }

    #[test]
    fn cluster_edits_are_non_destructive_and_reversible() {
        let (mut session, _, _, cluster, event) = populated_session();
        session
            .edit_cluster(cluster, "Mute cluster", |cluster| {
                cluster.muted = true;
                cluster.gain = 0.25;
            })
            .unwrap();
        assert!(session.arrangement().cluster(cluster).unwrap().muted);
        assert_eq!(
            session.arrangement().event(event).unwrap().cluster_id,
            cluster
        );
        session.undo().unwrap();
        assert!(!session.arrangement().cluster(cluster).unwrap().muted);
        assert!(session.arrangement().event(event).is_some());
    }

    #[test]
    fn saved_revision_tracks_undo_and_redo_state_identity() {
        let (mut session, _, _, _, event) = populated_session();
        session.mark_saved();
        let saved = session.saved_revision();
        session
            .edit_event(event, "Move", |event| event.sample = Sample(1))
            .unwrap();
        assert!(session.is_dirty());
        session.undo().unwrap();
        assert_eq!(session.revision(), saved);
        assert!(!session.is_dirty());
        session.redo().unwrap();
        assert!(session.is_dirty());
    }

    #[test]
    fn editing_after_undo_branches_history_and_clears_redo() {
        let (mut session, _, _, _, event) = populated_session();
        session
            .edit_event(event, "First move", |event| event.sample = Sample(1))
            .unwrap();
        let abandoned_revision = session.revision();
        session.undo().unwrap();
        session
            .edit_event(event, "Branched move", |event| event.sample = Sample(2))
            .unwrap();
        assert!(!session.can_redo());
        assert_ne!(session.revision(), abandoned_revision);
        assert_eq!(
            session.arrangement().event(event).unwrap().sample,
            Sample(2)
        );
    }

    #[test]
    fn no_op_edits_do_not_create_revisions() {
        let (mut session, _, _, _, event) = populated_session();
        let revision = session.revision();
        let label = session.undo_label().map(str::to_owned);
        let outcome = session.edit_event(event, "No-op", |_| {}).unwrap();
        assert!(!outcome.changed);
        assert_eq!(session.revision(), revision);
        assert_eq!(session.undo_label(), label.as_deref());
    }

    #[test]
    fn invalid_edits_roll_back_without_touching_history_or_revision() {
        let (mut session, _, _, _, event) = populated_session();
        let revision = session.revision();
        let previous_label = session.undo_label().map(str::to_owned);
        let error = session
            .edit_event(event, "Bad gain", |event| event.gain = f32::NAN)
            .unwrap_err();
        assert_eq!(error, SessionError::InvalidEntity("event"));
        assert_eq!(session.arrangement().event(event).unwrap().gain, 1.0);
        assert_eq!(session.revision(), revision);
        assert_eq!(session.undo_label(), previous_label.as_deref());
    }

    #[test]
    fn deferred_commands_detect_stale_state() {
        let (mut session, _, _, _, event) = populated_session();
        let deferred = session
            .command_for_event(event, "Deferred", |event| event.sample = Sample(50))
            .unwrap();
        session
            .edit_event(event, "Immediate", |event| event.sample = Sample(40))
            .unwrap();
        assert_eq!(
            session.execute(deferred).unwrap_err(),
            SessionError::CommandConflict("event")
        );
        assert_eq!(
            session.arrangement().event(event).unwrap().sample,
            Sample(40)
        );
    }

    #[test]
    fn structural_creation_is_undoable_as_an_atomic_command() {
        let mut session = Session::new(48_000).unwrap();
        let track = session.create_track("Track", TrackKind::Audio).unwrap();
        let lane = session.create_lane(track, "Take 1").unwrap();
        assert!(session.arrangement().lane(lane).is_some());
        session.undo().unwrap();
        assert!(session.arrangement().lane(lane).is_none());
        assert!(session
            .arrangement()
            .track(track)
            .unwrap()
            .lane_ids
            .is_empty());
        session.redo().unwrap();
        assert!(session.arrangement().lane(lane).is_some());
    }

    #[test]
    fn range_queries_include_points_and_overlapping_events() {
        let (mut session, _, lane, cluster, point) = populated_session();
        let span = session.create_event(lane, cluster, Sample(100)).unwrap();
        session
            .edit_event(span, "Set duration", |event| event.duration = 100)
            .unwrap();
        let ids: Vec<_> = session
            .arrangement()
            .events_in_range(SampleRange::new(150, 160))
            .map(|event| event.id)
            .collect();
        assert_eq!(ids, vec![span]);
        let ids: Vec<_> = session
            .arrangement()
            .events_in_range(SampleRange::new(12_000, 12_001))
            .map(|event| event.id)
            .collect();
        assert_eq!(ids, vec![point]);
    }

    #[test]
    fn bad_foreign_keys_and_zero_sample_rate_are_rejected() {
        assert_eq!(
            Session::new(0).unwrap_err(),
            SessionError::InvalidSampleRate
        );
        let mut session = Session::new(48_000).unwrap();
        assert_eq!(
            session.create_lane(TrackId(99), "Nope").unwrap_err(),
            SessionError::MissingTrack(TrackId(99))
        );
        assert_eq!(
            SnapGrid::new(0).unwrap_err(),
            SessionError::InvalidGridSpacing
        );
    }
}
