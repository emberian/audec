//! Project-wide automation model and deterministic curve compiler.
//!
//! This module intentionally contains no UI, plugin-host, or audio-device
//! code. Persistent lanes address stable project entities; a control-thread
//! compiler resolves beat positions to exact project frames; the resulting
//! [`CompiledLane`] can be evaluated without allocation or mutable state.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt;

pub const PPQ: i64 = 960;

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
            fn from(raw: u64) -> Self {
                Self(raw)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

typed_id!(AutomationLaneId);
typed_id!(AutomationPointId);

/// Signed project-frame time. Negative positions support count-in and preroll.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectFrame(pub i64);

/// Signed musical time in 960 ticks per quarter note.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BeatTime(pub i64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TimePosition {
    Frames(ProjectFrame),
    Beats(BeatTime),
}

impl TimePosition {
    fn coordinate(self) -> i64 {
        match self {
            Self::Frames(ProjectFrame(value)) | Self::Beats(BeatTime(value)) => value,
        }
    }

    fn domain(self) -> TimeDomain {
        match self {
            Self::Frames(_) => TimeDomain::Frames,
            Self::Beats(_) => TimeDomain::Beats,
        }
    }

    fn with_coordinate(self, coordinate: i64) -> Self {
        match self {
            Self::Frames(_) => Self::Frames(ProjectFrame(coordinate)),
            Self::Beats(_) => Self::Beats(BeatTime(coordinate)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TimeDomain {
    Frames,
    Beats,
}

/// Converts authored musical positions during control-thread compilation.
/// Implementations must be monotonic. The audio thread never calls this trait.
pub trait BeatFrameMap {
    fn beat_to_frame(&self, beat: BeatTime) -> ProjectFrame;
}

/// Exact fixed-tempo mapping. Tempo is stored in micro-BPM, avoiding a
/// platform-dependent floating-point conversion at scheduling boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixedTempo {
    pub sample_rate: u32,
    pub micro_bpm: u64,
}

impl FixedTempo {
    pub fn new(sample_rate: u32, micro_bpm: u64) -> Result<Self, AutomationError> {
        if sample_rate == 0 || micro_bpm == 0 {
            return Err(AutomationError::InvalidTempo);
        }
        Ok(Self {
            sample_rate,
            micro_bpm,
        })
    }
}

impl BeatFrameMap for FixedTempo {
    fn beat_to_frame(&self, BeatTime(ticks): BeatTime) -> ProjectFrame {
        let numerator = i128::from(ticks)
            .saturating_mul(i128::from(self.sample_rate))
            .saturating_mul(60_000_000);
        let denominator = i128::from(PPQ).saturating_mul(i128::from(self.micro_bpm));
        ProjectFrame(clamp_i128(round_ratio_ties_away(numerator, denominator)))
    }
}

/// A stable, serializable-looking address. Raw IDs bridge existing typed ID
/// spaces without making plugin pointers or collection indexes persistent.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ParameterAddress {
    Mixer(MixerTarget),
    Plugin {
        processor_id: u64,
        /// Format-native stable key (for example, a CLAP parameter ID).
        key: String,
    },
    Clip {
        clip_id: u64,
        parameter: ClipParameter,
    },
    Decomposition(DecompositionTarget),
    PerceptualLens {
        lens_id: String,
        parameter: LensParameter,
    },
    /// Existing AIR `ontology::ParameterId` represented without pointer identity.
    AirParameter(u64),
    Custom {
        namespace: String,
        entity: String,
        parameter: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MixerTarget {
    BusGain(u64),
    BusPan(u64),
    BusMute(u64),
    SendLevel(u64),
    SendMute(u64),
    InsertWet(u64),
    InsertBypass(u64),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClipParameter {
    Gain,
    Pan,
    PitchSemitones,
    PlaybackRate,
    FadeIn,
    FadeOut,
    Reverse,
    Custom(String),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DecompositionTarget {
    ComponentGain {
        component_id: u64,
    },
    ComponentPan {
        component_id: u64,
    },
    ObjectTransformParameter {
        object_id: u64,
        transform_id: u64,
        parameter_id: u64,
    },
    HypothesisBlend {
        hypothesis_id: u64,
    },
    ResidualMix {
        hypothesis_set_id: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LensParameter {
    MinimumFrequency,
    MaximumFrequency,
    DynamicRange,
    DbCeiling,
    TimeResolution,
    FrequencyResolution,
    HarmonicEmphasis,
    TransientEmphasis,
    ChromaticAberration,
    DepthDefocus,
    Custom(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParameterUnit {
    Linear,
    Normalized,
    Decibels,
    Hertz,
    Semitones,
    Ratio,
    Percent,
    Frames,
    Seconds,
    Degrees,
    Radians,
    Boolean,
    Enumerated(Vec<String>),
    Custom(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ValueMapping {
    Linear,
    /// Both range endpoints must be positive.
    Logarithmic,
    /// Round to one of this many evenly spaced values, including endpoints.
    Stepped {
        values: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SmoothingPolicy {
    None,
    LinearFrames(u32),
    OnePoleMilliseconds(f64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParameterDescriptor {
    pub address: ParameterAddress,
    pub name: String,
    pub unit: ParameterUnit,
    pub minimum: f64,
    pub maximum: f64,
    pub default: f64,
    pub mapping: ValueMapping,
    pub smoothing: SmoothingPolicy,
}

impl ParameterDescriptor {
    pub fn validate(&self) -> Result<(), AutomationError> {
        if self.name.is_empty() {
            return Err(AutomationError::EmptyName);
        }
        if !self.minimum.is_finite()
            || !self.maximum.is_finite()
            || !self.default.is_finite()
            || self.minimum >= self.maximum
            || !(self.minimum..=self.maximum).contains(&self.default)
        {
            return Err(AutomationError::InvalidRange);
        }
        match self.mapping {
            ValueMapping::Logarithmic if self.minimum <= 0.0 => {
                return Err(AutomationError::InvalidRange)
            }
            ValueMapping::Stepped { values } if values < 2 => {
                return Err(AutomationError::InvalidRange)
            }
            _ => {}
        }
        match self.smoothing {
            SmoothingPolicy::OnePoleMilliseconds(value) if !value.is_finite() || value <= 0.0 => {
                Err(AutomationError::InvalidSmoothing)
            }
            _ => Ok(()),
        }
    }

    pub fn constrain(&self, value: f64) -> f64 {
        let value = if value.is_finite() {
            value.clamp(self.minimum, self.maximum)
        } else {
            self.default
        };
        match self.mapping {
            ValueMapping::Stepped { values } => {
                let intervals = f64::from(values - 1);
                let normalized = (value - self.minimum) / (self.maximum - self.minimum);
                self.minimum
                    + normalized.mul_add(intervals, 0.0).round() / intervals
                        * (self.maximum - self.minimum)
            }
            _ => value,
        }
    }

    pub fn normalize(&self, plain: f64) -> f64 {
        let value = self.constrain(plain);
        match self.mapping {
            ValueMapping::Logarithmic => {
                (value / self.minimum).ln() / (self.maximum / self.minimum).ln()
            }
            _ => (value - self.minimum) / (self.maximum - self.minimum),
        }
    }

    pub fn denormalize(&self, normalized: f64) -> f64 {
        let normalized = if normalized.is_finite() {
            normalized.clamp(0.0, 1.0)
        } else {
            self.normalize(self.default)
        };
        let value = match self.mapping {
            ValueMapping::Logarithmic => {
                self.minimum * (self.maximum / self.minimum).powf(normalized)
            }
            _ => self.minimum + (self.maximum - self.minimum) * normalized,
        };
        self.constrain(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SegmentShape {
    Hold,
    Linear,
    /// Cubic smoothstep with zero endpoint slopes.
    Smooth,
    /// Exponential in value when endpoints have the same nonzero sign,
    /// otherwise linear. Useful for frequency and ratio controls.
    Exponential,
    /// Cubic Hermite, equivalent to a Bezier whose time handles are at 1/3
    /// and 2/3. Tangents are normalized value-change per normalized time.
    CubicBezier {
        outgoing_tangent: f64,
        incoming_tangent: f64,
    },
}

impl SegmentShape {
    fn validate(self) -> Result<(), AutomationError> {
        match self {
            Self::CubicBezier {
                outgoing_tangent,
                incoming_tangent,
            } if !outgoing_tangent.is_finite() || !incoming_tangent.is_finite() => {
                Err(AutomationError::NonFiniteCurve)
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AutomationPoint {
    pub id: AutomationPointId,
    pub position: TimePosition,
    pub value: f64,
    /// Shape from this point to the next point.
    pub outgoing: SegmentShape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingMode {
    Replace,
    Add,
    Multiply,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Extrapolation {
    HoldEndpoints,
    ParameterDefault,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AutomationLane {
    pub id: AutomationLaneId,
    pub name: String,
    pub target: ParameterAddress,
    pub time_domain: TimeDomain,
    pub binding: BindingMode,
    pub extrapolation: Extrapolation,
    pub enabled: bool,
    points: Vec<AutomationPoint>,
}

impl AutomationLane {
    pub fn new(
        id: AutomationLaneId,
        name: impl Into<String>,
        target: ParameterAddress,
        time_domain: TimeDomain,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            target,
            time_domain,
            binding: BindingMode::Replace,
            extrapolation: Extrapolation::HoldEndpoints,
            enabled: true,
            points: Vec::new(),
        }
    }

    pub fn points(&self) -> &[AutomationPoint] {
        &self.points
    }

    pub fn validate(&self, descriptor: &ParameterDescriptor) -> Result<(), AutomationError> {
        if self.name.is_empty() {
            return Err(AutomationError::EmptyName);
        }
        if self.target != descriptor.address {
            return Err(AutomationError::TargetMismatch);
        }
        let mut prior = None;
        let mut ids = BTreeSet::new();
        for point in &self.points {
            if point.position.domain() != self.time_domain {
                return Err(AutomationError::TimeDomainMismatch);
            }
            if !point.value.is_finite() {
                return Err(AutomationError::NonFiniteValue);
            }
            point.outgoing.validate()?;
            if prior.is_some_and(|prior| point.position.coordinate() <= prior) {
                return Err(AutomationError::PointsNotStrictlyOrdered);
            }
            if !ids.insert(point.id) {
                return Err(AutomationError::DuplicatePointId(point.id));
            }
            prior = Some(point.position.coordinate());
        }
        Ok(())
    }

    pub fn insert_point(
        &mut self,
        point: AutomationPoint,
    ) -> Result<Option<AutomationPoint>, AutomationError> {
        if point.position.domain() != self.time_domain {
            return Err(AutomationError::TimeDomainMismatch);
        }
        if !point.value.is_finite() {
            return Err(AutomationError::NonFiniteValue);
        }
        point.outgoing.validate()?;
        if self.points.iter().any(|existing| existing.id == point.id) {
            return Err(AutomationError::DuplicatePointId(point.id));
        }
        let coordinate = point.position.coordinate();
        match self
            .points
            .binary_search_by_key(&coordinate, |point| point.position.coordinate())
        {
            Ok(index) => Ok(Some(std::mem::replace(&mut self.points[index], point))),
            Err(index) => {
                self.points.insert(index, point);
                Ok(None)
            }
        }
    }

    pub fn remove_point(&mut self, id: AutomationPointId) -> Option<AutomationPoint> {
        let index = self.points.iter().position(|point| point.id == id)?;
        Some(self.points.remove(index))
    }

    pub fn value_at(
        &self,
        position: TimePosition,
        descriptor: &ParameterDescriptor,
    ) -> Option<f64> {
        if position.domain() != self.time_domain || !self.enabled {
            return None;
        }
        Some(descriptor.constrain(evaluate_points(
            &self.points,
            position.coordinate(),
            descriptor.default,
            self.extrapolation,
        )))
    }

    pub fn copy_range(
        &self,
        start: TimePosition,
        end: TimePosition,
        descriptor: &ParameterDescriptor,
    ) -> Result<AutomationClipboard, AutomationError> {
        if start.domain() != self.time_domain || end.domain() != self.time_domain {
            return Err(AutomationError::TimeDomainMismatch);
        }
        let (start, end) = ordered_pair(start.coordinate(), end.coordinate());
        if start == end {
            return Err(AutomationError::EmptyTimeRange);
        }
        let mut points = Vec::new();
        let start_value = self
            .value_at(start_position(self.time_domain, start), descriptor)
            .unwrap_or(descriptor.default);
        points.push(ClipboardPoint {
            offset: 0,
            value: start_value,
            outgoing: segment_at(&self.points, start),
        });
        for point in &self.points {
            let coordinate = point.position.coordinate();
            if coordinate > start && coordinate < end {
                points.push(ClipboardPoint {
                    offset: coordinate - start,
                    value: descriptor.constrain(point.value),
                    outgoing: point.outgoing,
                });
            }
        }
        let end_value = self
            .value_at(start_position(self.time_domain, end), descriptor)
            .unwrap_or(descriptor.default);
        points.push(ClipboardPoint {
            offset: end - start,
            value: end_value,
            outgoing: SegmentShape::Linear,
        });
        Ok(AutomationClipboard {
            domain: self.time_domain,
            duration: end - start,
            points,
        })
    }

    /// Paste a clipboard, replacing points in the destination's closed range.
    /// IDs come from the graph allocator and are never copied or reused.
    pub fn paste(
        &mut self,
        at: TimePosition,
        clipboard: &AutomationClipboard,
        value_scale: ValueScale,
        next_point_id: &mut u64,
    ) -> Result<(), AutomationError> {
        if at.domain() != self.time_domain || clipboard.domain != self.time_domain {
            return Err(AutomationError::TimeDomainMismatch);
        }
        if clipboard.duration < 0
            || clipboard.points.iter().any(|point| {
                point.offset < 0
                    || point.offset > clipboard.duration
                    || !point.value.is_finite()
                    || point.outgoing.validate().is_err()
            })
        {
            return Err(AutomationError::InvalidClipboard);
        }
        value_scale.validate()?;
        let start = at.coordinate();
        let end = start
            .checked_add(clipboard.duration)
            .ok_or(AutomationError::TimeOverflow)?;
        let mut replacement = self.clone();
        let mut allocated_through = *next_point_id;
        replacement.points.retain(|point| {
            let time = point.position.coordinate();
            time < start || time > end
        });
        for source in &clipboard.points {
            let coordinate = start
                .checked_add(source.offset)
                .ok_or(AutomationError::TimeOverflow)?;
            let id = allocate_id(&mut allocated_through)?;
            replacement.insert_point(AutomationPoint {
                id: AutomationPointId::from_raw(id),
                position: at.with_coordinate(coordinate),
                value: source.value.mul_add(value_scale.factor, value_scale.offset),
                outgoing: source.outgoing,
            })?;
        }
        self.points = replacement.points;
        *next_point_id = allocated_through;
        Ok(())
    }

    /// Scale point positions in `[start, end]` around `origin` using exact
    /// integer rational arithmetic. Colliding rounded positions are rejected.
    pub fn scale_time(
        &mut self,
        start: TimePosition,
        end: TimePosition,
        origin: TimePosition,
        scale: RationalScale,
    ) -> Result<(), AutomationError> {
        if start.domain() != self.time_domain
            || end.domain() != self.time_domain
            || origin.domain() != self.time_domain
        {
            return Err(AutomationError::TimeDomainMismatch);
        }
        scale.validate()?;
        let (start, end) = ordered_pair(start.coordinate(), end.coordinate());
        let origin = origin.coordinate();
        let mut replacement = self.points.clone();
        for point in &mut replacement {
            let time = point.position.coordinate();
            if (start..=end).contains(&time) {
                let delta = i128::from(time) - i128::from(origin);
                let scaled = round_ratio_ties_away(
                    delta.saturating_mul(i128::from(scale.numerator)),
                    i128::from(scale.denominator),
                );
                let coordinate = i128::from(origin)
                    .checked_add(scaled)
                    .ok_or(AutomationError::TimeOverflow)?;
                point.position = point.position.with_coordinate(checked_i128(coordinate)?);
            }
        }
        replacement.sort_by_key(|point| point.position.coordinate());
        if replacement
            .windows(2)
            .any(|pair| pair[0].position.coordinate() >= pair[1].position.coordinate())
        {
            return Err(AutomationError::PointCollision);
        }
        self.points = replacement;
        Ok(())
    }

    pub fn scale_values(
        &mut self,
        start: TimePosition,
        end: TimePosition,
        scale: ValueScale,
        descriptor: &ParameterDescriptor,
    ) -> Result<(), AutomationError> {
        if start.domain() != self.time_domain || end.domain() != self.time_domain {
            return Err(AutomationError::TimeDomainMismatch);
        }
        scale.validate()?;
        let (start, end) = ordered_pair(start.coordinate(), end.coordinate());
        for point in &mut self.points {
            if (start..=end).contains(&point.position.coordinate()) {
                point.value = descriptor.constrain(point.value.mul_add(scale.factor, scale.offset));
            }
        }
        Ok(())
    }

    /// Douglas-Peucker simplification in plain-value units. Curved and held
    /// segment boundaries are protected, so simplification cannot erase a
    /// deliberate discontinuity or change a non-linear segment's semantics.
    pub fn simplify(&mut self, tolerance: f64) -> Result<usize, AutomationError> {
        if !tolerance.is_finite() || tolerance < 0.0 {
            return Err(AutomationError::InvalidTolerance);
        }
        let old_len = self.points.len();
        if old_len <= 2 {
            return Ok(0);
        }
        let mut keep = vec![false; old_len];
        keep[0] = true;
        keep[old_len - 1] = true;
        for index in 0..old_len - 1 {
            if self.points[index].outgoing != SegmentShape::Linear {
                keep[index] = true;
                keep[index + 1] = true;
            }
        }
        let protected: Vec<usize> = keep
            .iter()
            .enumerate()
            .filter_map(|(index, keep)| keep.then_some(index))
            .collect();
        for pair in protected.windows(2) {
            simplify_span(&self.points, pair[0], pair[1], tolerance, &mut keep);
        }
        self.points = self
            .points
            .drain(..)
            .enumerate()
            .filter_map(|(index, point)| keep[index].then_some(point))
            .collect();
        Ok(old_len - self.points.len())
    }

    pub fn compile(
        &self,
        descriptor: &ParameterDescriptor,
        beat_map: &impl BeatFrameMap,
    ) -> Result<CompiledLane, AutomationError> {
        self.validate(descriptor)?;
        let mut points = Vec::with_capacity(self.points.len());
        for point in &self.points {
            let frame = match point.position {
                TimePosition::Frames(frame) => frame,
                TimePosition::Beats(beat) => beat_map.beat_to_frame(beat),
            };
            if points
                .last()
                .is_some_and(|prior: &CompiledPoint| prior.frame >= frame)
            {
                return Err(AutomationError::NonMonotonicBeatMap);
            }
            points.push(CompiledPoint {
                frame,
                value: descriptor.constrain(point.value),
                outgoing: point.outgoing,
            });
        }
        Ok(CompiledLane {
            id: self.id,
            target: self.target.clone(),
            binding: self.binding,
            extrapolation: self.extrapolation,
            enabled: self.enabled,
            default: descriptor.default,
            minimum: descriptor.minimum,
            maximum: descriptor.maximum,
            mapping: descriptor.mapping,
            points,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClipboardPoint {
    pub offset: i64,
    pub value: f64,
    pub outgoing: SegmentShape,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AutomationClipboard {
    pub domain: TimeDomain,
    pub duration: i64,
    pub points: Vec<ClipboardPoint>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ValueScale {
    pub factor: f64,
    pub offset: f64,
}

impl ValueScale {
    pub const IDENTITY: Self = Self {
        factor: 1.0,
        offset: 0.0,
    };

    fn validate(self) -> Result<(), AutomationError> {
        if self.factor.is_finite() && self.offset.is_finite() {
            Ok(())
        } else {
            Err(AutomationError::NonFiniteValue)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RationalScale {
    pub numerator: i64,
    pub denominator: i64,
}

impl RationalScale {
    fn validate(self) -> Result<(), AutomationError> {
        if self.numerator <= 0 || self.denominator <= 0 {
            Err(AutomationError::InvalidTimeScale)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct CompiledPoint {
    frame: ProjectFrame,
    value: f64,
    outgoing: SegmentShape,
}

/// Immutable, frame-domain automation prepared for an audio graph snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct CompiledLane {
    pub id: AutomationLaneId,
    pub target: ParameterAddress,
    pub binding: BindingMode,
    pub extrapolation: Extrapolation,
    pub enabled: bool,
    default: f64,
    minimum: f64,
    maximum: f64,
    mapping: ValueMapping,
    points: Vec<CompiledPoint>,
}

impl CompiledLane {
    pub fn value_at(&self, frame: ProjectFrame) -> Option<f64> {
        if !self.enabled {
            return None;
        }
        Some(self.constrain(evaluate_compiled_points(
            &self.points,
            frame.0,
            self.default,
            self.extrapolation,
        )))
    }

    /// Fill consecutive sample values. This method does not allocate.
    pub fn fill_block(&self, start: ProjectFrame, output: &mut [f64]) {
        for (offset, value) in output.iter_mut().enumerate() {
            let frame = start.0.saturating_add(saturating_usize_to_i64(offset));
            *value = self.value_at(ProjectFrame(frame)).unwrap_or(self.default);
        }
    }

    /// Evaluate a control-rate block at an exact integral frame stride.
    pub fn fill_strided(&self, start: ProjectFrame, frame_stride: u32, output: &mut [f64]) {
        for (offset, value) in output.iter_mut().enumerate() {
            let delta = i128::try_from(offset)
                .unwrap_or(i128::MAX)
                .saturating_mul(i128::from(frame_stride));
            let frame = i128::from(start.0).saturating_add(delta);
            *value = self
                .value_at(ProjectFrame(clamp_i128(frame)))
                .unwrap_or(self.default);
        }
    }

    fn constrain(&self, value: f64) -> f64 {
        let value = if value.is_finite() {
            value.clamp(self.minimum, self.maximum)
        } else {
            self.default
        };
        match self.mapping {
            ValueMapping::Stepped { values } => {
                let intervals = f64::from(values - 1);
                let t = (value - self.minimum) / (self.maximum - self.minimum);
                self.minimum + (t * intervals).round() / intervals * (self.maximum - self.minimum)
            }
            _ => value,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AutomationGraph {
    descriptors: BTreeMap<ParameterAddress, ParameterDescriptor>,
    lanes: BTreeMap<AutomationLaneId, AutomationLane>,
    /// Ephemeral optimistic-concurrency token for UI/controller intents.
    revision: u64,
    next_lane_id: u64,
    next_point_id: u64,
}

impl AutomationGraph {
    pub fn new() -> Self {
        Self {
            next_lane_id: 1,
            next_point_id: 1,
            ..Self::default()
        }
    }

    pub fn descriptors(&self) -> impl Iterator<Item = &ParameterDescriptor> {
        self.descriptors.values()
    }

    pub fn lanes(&self) -> impl Iterator<Item = &AutomationLane> {
        self.lanes.values()
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub fn lane(&self, id: AutomationLaneId) -> Option<&AutomationLane> {
        self.lanes.get(&id)
    }

    pub fn register_parameter(
        &mut self,
        descriptor: ParameterDescriptor,
    ) -> Result<Option<ParameterDescriptor>, AutomationError> {
        descriptor.validate()?;
        Ok(self
            .descriptors
            .insert(descriptor.address.clone(), descriptor))
    }

    pub fn create_lane(
        &mut self,
        name: impl Into<String>,
        target: ParameterAddress,
        domain: TimeDomain,
    ) -> Result<AutomationLaneId, AutomationError> {
        if !self.descriptors.contains_key(&target) {
            return Err(AutomationError::MissingParameter(target));
        }
        let id = AutomationLaneId::from_raw(allocate_id(&mut self.next_lane_id)?);
        self.lanes
            .insert(id, AutomationLane::new(id, name, target, domain));
        Ok(id)
    }

    /// Paste through the graph-owned monotonic point allocator. The lane and
    /// allocator are committed together only after the complete paste validates.
    pub fn paste_into_lane(
        &mut self,
        lane_id: AutomationLaneId,
        at: TimePosition,
        clipboard: &AutomationClipboard,
        value_scale: ValueScale,
    ) -> Result<(), AutomationError> {
        let mut replacement = self
            .lanes
            .get(&lane_id)
            .cloned()
            .ok_or(AutomationError::MissingLane(lane_id))?;
        let mut next_point_id = self.next_point_id;
        replacement.paste(at, clipboard, value_scale, &mut next_point_id)?;
        let descriptor = self
            .descriptors
            .get(&replacement.target)
            .ok_or_else(|| AutomationError::MissingParameter(replacement.target.clone()))?;
        replacement.validate(descriptor)?;
        self.lanes.insert(lane_id, replacement);
        self.next_point_id = next_point_id;
        Ok(())
    }

    pub fn allocate_point_id(&mut self) -> Result<AutomationPointId, AutomationError> {
        Ok(AutomationPointId::from_raw(allocate_id(
            &mut self.next_point_id,
        )?))
    }

    /// Read-only candidate used by a gesture draft. Applying the resulting
    /// guarded lane replacement advances the allocator atomically.
    pub fn next_point_id_candidate(&self) -> Result<AutomationPointId, AutomationError> {
        if self.next_point_id == 0 {
            return Err(AutomationError::IdExhausted);
        }
        Ok(AutomationPointId::from_raw(self.next_point_id))
    }

    pub fn insert_point(
        &mut self,
        lane: AutomationLaneId,
        position: TimePosition,
        value: f64,
        outgoing: SegmentShape,
    ) -> Result<AutomationPointId, AutomationError> {
        let id = self.allocate_point_id()?;
        self.lanes
            .get_mut(&lane)
            .ok_or(AutomationError::MissingLane(lane))?
            .insert_point(AutomationPoint {
                id,
                position,
                value,
                outgoing,
            })?;
        Ok(id)
    }

    pub fn validate(&self) -> Result<(), AutomationError> {
        for descriptor in self.descriptors.values() {
            descriptor.validate()?;
        }
        for lane in self.lanes.values() {
            let descriptor = self
                .descriptors
                .get(&lane.target)
                .ok_or_else(|| AutomationError::MissingParameter(lane.target.clone()))?;
            lane.validate(descriptor)?;
        }
        Ok(())
    }

    pub fn compile(
        &self,
        beat_map: &impl BeatFrameMap,
    ) -> Result<CompiledAutomation, AutomationError> {
        self.validate()?;
        let mut targets: BTreeMap<ParameterAddress, Vec<CompiledLane>> = BTreeMap::new();
        for lane in self.lanes.values() {
            let descriptor = &self.descriptors[&lane.target];
            targets
                .entry(lane.target.clone())
                .or_default()
                .push(lane.compile(descriptor, beat_map)?);
        }
        Ok(CompiledAutomation {
            descriptors: self.descriptors.clone(),
            targets,
        })
    }

    /// Atomically apply exact lane replacements and return the inverse command.
    pub fn apply(
        &mut self,
        command: &AutomationCommand,
    ) -> Result<AutomationCommand, AutomationError> {
        self.apply_with_expected(None, command)
    }

    pub fn apply_intent(
        &mut self,
        intent: &AutomationIntent,
    ) -> Result<AutomationCommand, AutomationError> {
        self.apply_with_expected(Some(intent.expected_revision), &intent.command)
    }

    fn apply_with_expected(
        &mut self,
        expected_revision: Option<u64>,
        command: &AutomationCommand,
    ) -> Result<AutomationCommand, AutomationError> {
        if let Some(expected) = expected_revision {
            if self.revision != expected {
                return Err(AutomationError::RevisionConflict {
                    expected,
                    actual: self.revision,
                });
            }
        }
        let next_revision = self
            .revision
            .checked_add(1)
            .ok_or(AutomationError::RevisionExhausted)?;
        // Validate all optimistic preconditions before touching the graph.
        let mut changed_ids = BTreeSet::new();
        for change in &command.changes {
            let id = change.id()?;
            if !changed_ids.insert(id) {
                return Err(AutomationError::DuplicateLaneChange(id));
            }
            if self.lanes.get(&id) != change.before.as_ref() {
                return Err(AutomationError::CommandConflict(id));
            }
        }

        let before = self.lanes.clone();
        for change in &command.changes {
            let id = change.id()?;
            match &change.after {
                Some(lane) => {
                    self.lanes.insert(id, lane.clone());
                }
                None => {
                    self.lanes.remove(&id);
                }
            }
        }
        if let Err(error) = self.validate() {
            self.lanes = before;
            return Err(error);
        }
        // Imported/redo-created identities advance but never rewind allocators.
        for change in &command.changes {
            if let Some(lane) = &change.after {
                advance_allocator_past(&mut self.next_lane_id, lane.id.get());
                for point in &lane.points {
                    advance_allocator_past(&mut self.next_point_id, point.id.get());
                }
            }
        }
        self.revision = next_revision;
        Ok(command.inverse())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledAutomation {
    descriptors: BTreeMap<ParameterAddress, ParameterDescriptor>,
    targets: BTreeMap<ParameterAddress, Vec<CompiledLane>>,
}

impl CompiledAutomation {
    pub fn value_at(
        &self,
        address: &ParameterAddress,
        frame: ProjectFrame,
        base_value: f64,
    ) -> Option<f64> {
        let descriptor = self.descriptors.get(address)?;
        let mut value = descriptor.constrain(base_value);
        for lane in self.targets.get(address).into_iter().flatten() {
            let Some(automated) = lane.value_at(frame) else {
                continue;
            };
            value = match lane.binding {
                BindingMode::Replace => automated,
                BindingMode::Add => value + automated,
                BindingMode::Multiply => value * automated,
            };
            value = descriptor.constrain(value);
        }
        Some(value)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LaneChange {
    pub before: Option<AutomationLane>,
    pub after: Option<AutomationLane>,
}

impl LaneChange {
    fn id(&self) -> Result<AutomationLaneId, AutomationError> {
        match (&self.before, &self.after) {
            (Some(before), Some(after)) if before.id == after.id => Ok(before.id),
            (Some(before), None) => Ok(before.id),
            (None, Some(after)) => Ok(after.id),
            _ => Err(AutomationError::InvalidCommand),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AutomationCommand {
    pub label: String,
    pub changes: Vec<LaneChange>,
}

impl AutomationCommand {
    pub fn replace(
        label: impl Into<String>,
        before: AutomationLane,
        after: AutomationLane,
    ) -> Result<Self, AutomationError> {
        if before.id != after.id {
            return Err(AutomationError::InvalidCommand);
        }
        Ok(Self {
            label: label.into(),
            changes: vec![LaneChange {
                before: Some(before),
                after: Some(after),
            }],
        })
    }

    pub fn inverse(&self) -> Self {
        Self {
            label: self.label.clone(),
            changes: self
                .changes
                .iter()
                .rev()
                .map(|change| LaneChange {
                    before: change.after.clone(),
                    after: change.before.clone(),
                })
                .collect(),
        }
    }
}

/// A semantic automation edit paired with the graph revision observed by the
/// initiating gesture. The command remains reusable by persistence and the
/// aggregate envelope; only live interaction needs this guard wrapper.
#[derive(Clone, Debug, PartialEq)]
pub struct AutomationIntent {
    pub expected_revision: u64,
    pub command: AutomationCommand,
}

impl AutomationIntent {
    pub fn new(expected_revision: u64, command: AutomationCommand) -> Self {
        Self {
            expected_revision,
            command,
        }
    }

    pub fn inverse_for_revision(&self, expected_revision: u64) -> Self {
        Self::new(expected_revision, self.command.inverse())
    }
}

#[derive(Clone, Debug)]
pub struct AutomationHistory {
    graph: AutomationGraph,
    undo: VecDeque<AutomationCommand>,
    redo: Vec<AutomationCommand>,
    limit: usize,
}

impl AutomationHistory {
    pub fn new(graph: AutomationGraph, limit: usize) -> Self {
        Self {
            graph,
            undo: VecDeque::new(),
            redo: Vec::new(),
            limit,
        }
    }

    pub fn graph(&self) -> &AutomationGraph {
        &self.graph
    }

    pub fn execute(&mut self, command: AutomationCommand) -> Result<(), AutomationError> {
        let inverse = self.graph.apply(&command)?;
        self.redo.clear();
        if self.limit > 0 {
            self.undo.push_back(inverse);
            while self.undo.len() > self.limit {
                self.undo.pop_front();
            }
        }
        Ok(())
    }

    pub fn undo(&mut self) -> Result<bool, AutomationError> {
        let Some(command) = self.undo.pop_back() else {
            return Ok(false);
        };
        let redo = self.graph.apply(&command)?;
        self.redo.push(redo);
        Ok(true)
    }

    pub fn redo(&mut self) -> Result<bool, AutomationError> {
        let Some(command) = self.redo.pop() else {
            return Ok(false);
        };
        let undo = self.graph.apply(&command)?;
        self.undo.push_back(undo);
        Ok(true)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMode {
    Read,
    Touch,
    Latch,
    Write,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WriterEvent {
    TransportStarted,
    TransportStopped,
    TouchStarted { value: f64 },
    ControlChanged { value: f64 },
    TouchEnded,
    Tick { position: TimePosition },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WriterAction {
    Write { position: TimePosition, value: f64 },
    ResumeRead,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriterState {
    Stopped,
    Reading,
    Touching,
    Latched,
    Writing,
}

/// Explicit write/touch/latch state machine. One `Tick` corresponds to one
/// host-selected automation write quantum; the state machine never invents
/// wall-clock timing or points between ticks.
#[derive(Clone, Debug, PartialEq)]
pub struct AutomationWriter {
    mode: WriteMode,
    state: WriterState,
    rolling: bool,
    current_value: f64,
}

impl AutomationWriter {
    pub fn new(mode: WriteMode, initial_value: f64) -> Result<Self, AutomationError> {
        if !initial_value.is_finite() {
            return Err(AutomationError::NonFiniteValue);
        }
        Ok(Self {
            mode,
            state: WriterState::Stopped,
            rolling: false,
            current_value: initial_value,
        })
    }

    pub fn mode(&self) -> WriteMode {
        self.mode
    }

    pub fn state(&self) -> WriterState {
        self.state
    }

    pub fn set_mode(&mut self, mode: WriteMode) {
        self.mode = mode;
        self.state = if self.rolling {
            if mode == WriteMode::Write {
                WriterState::Writing
            } else {
                WriterState::Reading
            }
        } else {
            WriterState::Stopped
        };
    }

    pub fn process(&mut self, event: WriterEvent) -> Result<Option<WriterAction>, AutomationError> {
        match event {
            WriterEvent::TransportStarted => {
                self.rolling = true;
                self.state = if self.mode == WriteMode::Write {
                    WriterState::Writing
                } else {
                    WriterState::Reading
                };
                Ok(None)
            }
            WriterEvent::TransportStopped => {
                let was_writing = matches!(
                    self.state,
                    WriterState::Touching | WriterState::Latched | WriterState::Writing
                );
                self.rolling = false;
                self.state = WriterState::Stopped;
                Ok(was_writing.then_some(WriterAction::ResumeRead))
            }
            WriterEvent::TouchStarted { value } | WriterEvent::ControlChanged { value } => {
                if !value.is_finite() {
                    return Err(AutomationError::NonFiniteValue);
                }
                self.current_value = value;
                if self.rolling {
                    self.state = match self.mode {
                        WriteMode::Read => WriterState::Reading,
                        WriteMode::Touch | WriteMode::Latch => WriterState::Touching,
                        WriteMode::Write => WriterState::Writing,
                    };
                }
                Ok(None)
            }
            WriterEvent::TouchEnded => {
                let action = match (self.mode, self.state) {
                    (WriteMode::Touch, WriterState::Touching) => {
                        self.state = WriterState::Reading;
                        Some(WriterAction::ResumeRead)
                    }
                    (WriteMode::Latch, WriterState::Touching) => {
                        self.state = WriterState::Latched;
                        None
                    }
                    _ => None,
                };
                Ok(action)
            }
            WriterEvent::Tick { position } => {
                let writing = self.rolling
                    && matches!(
                        self.state,
                        WriterState::Touching | WriterState::Latched | WriterState::Writing
                    );
                Ok(writing.then_some(WriterAction::Write {
                    position,
                    value: self.current_value,
                }))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AutomationError {
    EmptyName,
    InvalidRange,
    InvalidSmoothing,
    InvalidTempo,
    InvalidTimeScale,
    InvalidTolerance,
    EmptyTimeRange,
    InvalidClipboard,
    NonFiniteValue,
    NonFiniteCurve,
    TimeDomainMismatch,
    TargetMismatch,
    PointsNotStrictlyOrdered,
    DuplicatePointId(AutomationPointId),
    MissingParameter(ParameterAddress),
    MissingLane(AutomationLaneId),
    NonMonotonicBeatMap,
    PointCollision,
    TimeOverflow,
    IdExhausted,
    InvalidCommand,
    RevisionConflict { expected: u64, actual: u64 },
    RevisionExhausted,
    DuplicateLaneChange(AutomationLaneId),
    CommandConflict(AutomationLaneId),
}

impl fmt::Display for AutomationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName => write!(f, "name must not be empty"),
            Self::InvalidRange => write!(f, "invalid parameter range or mapping"),
            Self::InvalidSmoothing => write!(f, "invalid smoothing policy"),
            Self::InvalidTempo => write!(f, "sample rate and tempo must be nonzero"),
            Self::InvalidTimeScale => write!(f, "time scale must be positive"),
            Self::InvalidTolerance => write!(
                f,
                "simplification tolerance must be finite and non-negative"
            ),
            Self::EmptyTimeRange => write!(f, "time range must not be empty"),
            Self::InvalidClipboard => write!(f, "automation clipboard is malformed"),
            Self::NonFiniteValue => write!(f, "automation value must be finite"),
            Self::NonFiniteCurve => write!(f, "curve tangents must be finite"),
            Self::TimeDomainMismatch => write!(f, "position is in the wrong time domain"),
            Self::TargetMismatch => write!(f, "lane and descriptor targets differ"),
            Self::PointsNotStrictlyOrdered => {
                write!(f, "points must have unique increasing positions")
            }
            Self::DuplicatePointId(id) => write!(f, "duplicate automation point {id}"),
            Self::MissingParameter(address) => write!(f, "missing parameter {address:?}"),
            Self::MissingLane(id) => write!(f, "missing automation lane {id}"),
            Self::NonMonotonicBeatMap => {
                write!(f, "beat map collapsed or reversed automation points")
            }
            Self::PointCollision => write!(f, "edit would place multiple points at one time"),
            Self::TimeOverflow => write!(f, "automation time overflow"),
            Self::IdExhausted => write!(f, "automation ID space exhausted"),
            Self::InvalidCommand => write!(f, "invalid automation command"),
            Self::RevisionConflict { expected, actual } => write!(
                f,
                "automation revision conflict: expected {expected}, found {actual}"
            ),
            Self::RevisionExhausted => write!(f, "automation revision exhausted"),
            Self::DuplicateLaneChange(id) => {
                write!(f, "automation lane {id} appears twice in one command")
            }
            Self::CommandConflict(id) => {
                write!(f, "automation lane {id} changed since command creation")
            }
        }
    }
}

impl Error for AutomationError {}

fn evaluate_points(
    points: &[AutomationPoint],
    position: i64,
    default: f64,
    extrapolation: Extrapolation,
) -> f64 {
    evaluate_generic(
        points,
        position,
        default,
        extrapolation,
        |point| point.position.coordinate(),
        |point| point.value,
        |point| point.outgoing,
    )
}

fn evaluate_compiled_points(
    points: &[CompiledPoint],
    position: i64,
    default: f64,
    extrapolation: Extrapolation,
) -> f64 {
    evaluate_generic(
        points,
        position,
        default,
        extrapolation,
        |point| point.frame.0,
        |point| point.value,
        |point| point.outgoing,
    )
}

fn evaluate_generic<T>(
    points: &[T],
    position: i64,
    default: f64,
    extrapolation: Extrapolation,
    time: impl Fn(&T) -> i64,
    value: impl Fn(&T) -> f64,
    shape: impl Fn(&T) -> SegmentShape,
) -> f64 {
    let Some(first) = points.first() else {
        return default;
    };
    if position < time(first) {
        return match extrapolation {
            Extrapolation::HoldEndpoints => value(first),
            Extrapolation::ParameterDefault => default,
        };
    }
    let last = points.last().expect("non-empty");
    if position >= time(last) {
        return if position == time(last) || extrapolation == Extrapolation::HoldEndpoints {
            value(last)
        } else {
            default
        };
    }
    let right = points.partition_point(|point| time(point) <= position);
    let a = &points[right - 1];
    let b = &points[right];
    let width = time(b) - time(a);
    let t = (position - time(a)) as f64 / width as f64;
    interpolate(value(a), value(b), t, shape(a))
}

fn interpolate(a: f64, b: f64, t: f64, shape: SegmentShape) -> f64 {
    match shape {
        SegmentShape::Hold => a,
        SegmentShape::Linear => a + (b - a) * t,
        SegmentShape::Smooth => {
            let smooth = t * t * (3.0 - 2.0 * t);
            a + (b - a) * smooth
        }
        SegmentShape::Exponential if a != 0.0 && b != 0.0 && a.signum() == b.signum() => {
            a * (b / a).powf(t)
        }
        SegmentShape::Exponential => a + (b - a) * t,
        SegmentShape::CubicBezier {
            outgoing_tangent,
            incoming_tangent,
        } => {
            let delta = b - a;
            let t2 = t * t;
            let t3 = t2 * t;
            let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
            let h10 = t3 - 2.0 * t2 + t;
            let h01 = -2.0 * t3 + 3.0 * t2;
            let h11 = t3 - t2;
            h00 * a + h10 * outgoing_tangent * delta + h01 * b + h11 * incoming_tangent * delta
        }
    }
}

fn segment_at(points: &[AutomationPoint], position: i64) -> SegmentShape {
    let right = points.partition_point(|point| point.position.coordinate() <= position);
    right
        .checked_sub(1)
        .and_then(|index| points.get(index))
        .map_or(SegmentShape::Linear, |point| point.outgoing)
}

fn simplify_span(
    points: &[AutomationPoint],
    start: usize,
    end: usize,
    tolerance: f64,
    keep: &mut [bool],
) {
    if end <= start + 1 {
        return;
    }
    let a = &points[start];
    let b = &points[end];
    let t0 = a.position.coordinate() as f64;
    let width = (b.position.coordinate() - a.position.coordinate()) as f64;
    let mut furthest = None;
    let mut maximum_error = tolerance;
    for (index, point) in points.iter().enumerate().take(end).skip(start + 1) {
        let t = (point.position.coordinate() as f64 - t0) / width;
        let expected = a.value + (b.value - a.value) * t;
        let error = (point.value - expected).abs();
        if error > maximum_error {
            maximum_error = error;
            furthest = Some(index);
        }
    }
    if let Some(index) = furthest {
        keep[index] = true;
        simplify_span(points, start, index, tolerance, keep);
        simplify_span(points, index, end, tolerance, keep);
    }
}

fn start_position(domain: TimeDomain, coordinate: i64) -> TimePosition {
    match domain {
        TimeDomain::Frames => TimePosition::Frames(ProjectFrame(coordinate)),
        TimeDomain::Beats => TimePosition::Beats(BeatTime(coordinate)),
    }
}

fn ordered_pair(a: i64, b: i64) -> (i64, i64) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

fn allocate_id(next: &mut u64) -> Result<u64, AutomationError> {
    if *next == 0 || *next == u64::MAX {
        return Err(AutomationError::IdExhausted);
    }
    let id = *next;
    *next += 1;
    Ok(id)
}

fn advance_allocator_past(next: &mut u64, used: u64) {
    *next = (*next).max(used.saturating_add(1));
}

fn round_ratio_ties_away(numerator: i128, denominator: i128) -> i128 {
    debug_assert!(denominator > 0);
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    let doubled = remainder.abs().saturating_mul(2);
    if doubled >= denominator {
        quotient + numerator.signum()
    } else {
        quotient
    }
}

fn checked_i128(value: i128) -> Result<i64, AutomationError> {
    i64::try_from(value).map_err(|_| AutomationError::TimeOverflow)
}

fn clamp_i128(value: i128) -> i64 {
    value.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn saturating_usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address() -> ParameterAddress {
        ParameterAddress::Mixer(MixerTarget::BusGain(7))
    }

    fn descriptor() -> ParameterDescriptor {
        ParameterDescriptor {
            address: address(),
            name: "Gain".into(),
            unit: ParameterUnit::Decibels,
            minimum: -60.0,
            maximum: 12.0,
            default: 0.0,
            mapping: ValueMapping::Linear,
            smoothing: SmoothingPolicy::LinearFrames(32),
        }
    }

    fn point(id: u64, frame: i64, value: f64, outgoing: SegmentShape) -> AutomationPoint {
        AutomationPoint {
            id: AutomationPointId::from_raw(id),
            position: TimePosition::Frames(ProjectFrame(frame)),
            value,
            outgoing,
        }
    }

    fn lane(points: Vec<AutomationPoint>) -> AutomationLane {
        let mut lane = AutomationLane::new(
            AutomationLaneId::from_raw(1),
            "gain",
            address(),
            TimeDomain::Frames,
        );
        for point in points {
            lane.insert_point(point).unwrap();
        }
        lane
    }

    #[test]
    fn fixed_tempo_is_exact_at_boundaries_and_signed() {
        let tempo = FixedTempo::new(48_000, 120_000_000).unwrap();
        assert_eq!(tempo.beat_to_frame(BeatTime(0)), ProjectFrame(0));
        assert_eq!(tempo.beat_to_frame(BeatTime(PPQ)), ProjectFrame(24_000));
        assert_eq!(tempo.beat_to_frame(BeatTime(-PPQ)), ProjectFrame(-24_000));
        assert_eq!(tempo.beat_to_frame(BeatTime(PPQ / 2)), ProjectFrame(12_000));
    }

    #[test]
    fn parameter_mapping_round_trips_and_steps() {
        let mut descriptor = descriptor();
        for value in [-60.0, -31.25, 0.0, 12.0] {
            assert!((descriptor.denormalize(descriptor.normalize(value)) - value).abs() < 1e-10);
        }
        descriptor.minimum = 20.0;
        descriptor.maximum = 20_000.0;
        descriptor.default = 440.0;
        descriptor.mapping = ValueMapping::Logarithmic;
        for value in [20.0, 440.0, 20_000.0] {
            assert!((descriptor.denormalize(descriptor.normalize(value)) - value).abs() < 1e-8);
        }
        descriptor.minimum = 0.0;
        descriptor.maximum = 1.0;
        descriptor.default = 0.0;
        descriptor.mapping = ValueMapping::Stepped { values: 5 };
        assert_eq!(descriptor.constrain(0.36), 0.25);
        assert_eq!(descriptor.constrain(0.91), 1.0);
    }

    #[test]
    fn all_segment_shapes_have_defined_edges() {
        for shape in [
            SegmentShape::Hold,
            SegmentShape::Linear,
            SegmentShape::Smooth,
            SegmentShape::Exponential,
            SegmentShape::CubicBezier {
                outgoing_tangent: 0.0,
                incoming_tangent: 0.0,
            },
        ] {
            let lane = lane(vec![point(1, 10, 1.0, shape), point(2, 20, 4.0, shape)]);
            assert_eq!(
                lane.value_at(TimePosition::Frames(ProjectFrame(10)), &descriptor()),
                Some(1.0)
            );
            assert_eq!(
                lane.value_at(TimePosition::Frames(ProjectFrame(20)), &descriptor()),
                Some(4.0)
            );
            assert!(lane
                .value_at(TimePosition::Frames(ProjectFrame(15)), &descriptor())
                .unwrap()
                .is_finite());
        }
        let hold = lane(vec![
            point(1, 0, 1.0, SegmentShape::Hold),
            point(2, 10, 4.0, SegmentShape::Linear),
        ]);
        assert_eq!(
            hold.value_at(TimePosition::Frames(ProjectFrame(9)), &descriptor()),
            Some(1.0)
        );
        let exponential = lane(vec![
            point(1, 0, 1.0, SegmentShape::Exponential),
            point(2, 10, 4.0, SegmentShape::Linear),
        ]);
        assert!(
            (exponential
                .value_at(TimePosition::Frames(ProjectFrame(5)), &descriptor())
                .unwrap()
                - 2.0)
                .abs()
                < 1e-12
        );
    }

    #[test]
    fn extrapolation_is_explicit() {
        let mut lane = lane(vec![
            point(1, 10, -12.0, SegmentShape::Linear),
            point(2, 20, -6.0, SegmentShape::Linear),
        ]);
        assert_eq!(
            lane.value_at(TimePosition::Frames(ProjectFrame(-99)), &descriptor()),
            Some(-12.0)
        );
        lane.extrapolation = Extrapolation::ParameterDefault;
        assert_eq!(
            lane.value_at(TimePosition::Frames(ProjectFrame(9)), &descriptor()),
            Some(0.0)
        );
        assert_eq!(
            lane.value_at(TimePosition::Frames(ProjectFrame(21)), &descriptor()),
            Some(0.0)
        );
        assert_eq!(
            lane.value_at(TimePosition::Frames(ProjectFrame(20)), &descriptor()),
            Some(-6.0)
        );
    }

    #[test]
    fn insertion_sorts_replaces_time_and_rejects_duplicate_identity() {
        let mut lane = lane(vec![]);
        assert_eq!(
            lane.insert_point(point(1, 20, 2.0, SegmentShape::Linear))
                .unwrap(),
            None
        );
        assert_eq!(
            lane.insert_point(point(2, 10, 1.0, SegmentShape::Linear))
                .unwrap(),
            None
        );
        let replaced = lane
            .insert_point(point(3, 10, 3.0, SegmentShape::Hold))
            .unwrap()
            .unwrap();
        assert_eq!(replaced.id, AutomationPointId::from_raw(2));
        assert_eq!(lane.points()[0].id, AutomationPointId::from_raw(3));
        assert_eq!(
            lane.insert_point(point(3, 30, 0.0, SegmentShape::Linear)),
            Err(AutomationError::DuplicatePointId(
                AutomationPointId::from_raw(3)
            ))
        );
    }

    #[test]
    fn compiled_blocks_are_partition_invariant() {
        let lane = lane(vec![
            point(1, -10, -20.0, SegmentShape::Linear),
            point(2, 17, 7.0, SegmentShape::Smooth),
            point(3, 81, -3.0, SegmentShape::Linear),
        ]);
        let compiled = lane
            .compile(
                &descriptor(),
                &FixedTempo::new(48_000, 120_000_000).unwrap(),
            )
            .unwrap();
        let mut whole = vec![0.0; 128];
        compiled.fill_block(ProjectFrame(-32), &mut whole);
        let mut partitioned = Vec::new();
        for (start, len) in [(-32, 1), (-31, 7), (-24, 31), (7, 64), (71, 25)] {
            let mut block = vec![0.0; len];
            compiled.fill_block(ProjectFrame(start), &mut block);
            partitioned.extend(block);
        }
        assert_eq!(whole, partitioned);
        for (index, value) in whole.iter().enumerate() {
            assert_eq!(
                *value,
                compiled.value_at(ProjectFrame(-32 + index as i64)).unwrap()
            );
        }
    }

    #[test]
    fn beat_lane_compiles_to_frame_lane() {
        let mut lane = AutomationLane::new(
            AutomationLaneId::from_raw(2),
            "beat gain",
            address(),
            TimeDomain::Beats,
        );
        lane.insert_point(AutomationPoint {
            id: 1.into(),
            position: TimePosition::Beats(BeatTime(0)),
            value: -12.0,
            outgoing: SegmentShape::Linear,
        })
        .unwrap();
        lane.insert_point(AutomationPoint {
            id: 2.into(),
            position: TimePosition::Beats(BeatTime(PPQ)),
            value: 0.0,
            outgoing: SegmentShape::Linear,
        })
        .unwrap();
        let compiled = lane
            .compile(
                &descriptor(),
                &FixedTempo::new(48_000, 120_000_000).unwrap(),
            )
            .unwrap();
        assert_eq!(compiled.value_at(ProjectFrame(0)), Some(-12.0));
        assert_eq!(compiled.value_at(ProjectFrame(12_000)), Some(-6.0));
        assert_eq!(compiled.value_at(ProjectFrame(24_000)), Some(0.0));
    }

    #[test]
    fn graph_binds_multiple_lanes_in_stable_id_order() {
        let mut graph = AutomationGraph::new();
        graph.register_parameter(descriptor()).unwrap();
        let replace = graph
            .create_lane("replace", address(), TimeDomain::Frames)
            .unwrap();
        graph
            .insert_point(
                replace,
                TimePosition::Frames(ProjectFrame(0)),
                -12.0,
                SegmentShape::Linear,
            )
            .unwrap();
        let add = graph
            .create_lane("add", address(), TimeDomain::Frames)
            .unwrap();
        graph.lanes.get_mut(&add).unwrap().binding = BindingMode::Add;
        graph
            .insert_point(
                add,
                TimePosition::Frames(ProjectFrame(0)),
                3.0,
                SegmentShape::Linear,
            )
            .unwrap();
        let multiply = graph
            .create_lane("multiply", address(), TimeDomain::Frames)
            .unwrap();
        graph.lanes.get_mut(&multiply).unwrap().binding = BindingMode::Multiply;
        graph
            .insert_point(
                multiply,
                TimePosition::Frames(ProjectFrame(0)),
                2.0,
                SegmentShape::Linear,
            )
            .unwrap();
        let compiled = graph
            .compile(&FixedTempo::new(48_000, 120_000_000).unwrap())
            .unwrap();
        assert_eq!(
            compiled.value_at(&address(), ProjectFrame(0), 6.0),
            Some(-18.0)
        );
    }

    #[test]
    fn clipboard_injects_exact_boundary_samples_and_paste_allocates_ids() {
        let source = lane(vec![
            point(1, 0, 0.0, SegmentShape::Linear),
            point(2, 100, 10.0, SegmentShape::Linear),
        ]);
        let clip = source
            .copy_range(
                TimePosition::Frames(ProjectFrame(25)),
                TimePosition::Frames(ProjectFrame(75)),
                &descriptor(),
            )
            .unwrap();
        assert_eq!(clip.duration, 50);
        assert_eq!(clip.points[0].value, 2.5);
        assert_eq!(clip.points[1].value, 7.5);
        let mut destination = lane(vec![point(8, 210, 11.0, SegmentShape::Linear)]);
        let mut next = 100;
        destination
            .paste(
                TimePosition::Frames(ProjectFrame(200)),
                &clip,
                ValueScale {
                    factor: 2.0,
                    offset: -1.0,
                },
                &mut next,
            )
            .unwrap();
        assert_eq!(
            destination
                .points()
                .iter()
                .map(|p| (p.id.get(), p.position.coordinate(), p.value))
                .collect::<Vec<_>>(),
            vec![(100, 200, 4.0), (101, 250, 14.0)]
        );
    }

    #[test]
    fn rational_time_scaling_is_exact_and_collision_is_atomic() {
        let mut lane = lane(vec![
            point(1, -10, 0.0, SegmentShape::Linear),
            point(2, 0, 1.0, SegmentShape::Linear),
            point(3, 10, 2.0, SegmentShape::Linear),
        ]);
        lane.scale_time(
            TimePosition::Frames(ProjectFrame(-10)),
            TimePosition::Frames(ProjectFrame(10)),
            TimePosition::Frames(ProjectFrame(0)),
            RationalScale {
                numerator: 3,
                denominator: 2,
            },
        )
        .unwrap();
        assert_eq!(
            lane.points()
                .iter()
                .map(|p| p.position.coordinate())
                .collect::<Vec<_>>(),
            vec![-15, 0, 15]
        );
        let before = lane.clone();
        assert_eq!(
            lane.scale_time(
                TimePosition::Frames(ProjectFrame(-15)),
                TimePosition::Frames(ProjectFrame(15)),
                TimePosition::Frames(ProjectFrame(0)),
                RationalScale {
                    numerator: 1,
                    denominator: 100
                }
            ),
            Err(AutomationError::PointCollision)
        );
        assert_eq!(lane, before);
    }

    #[test]
    fn simplification_removes_linear_redundancy_but_protects_curve_boundaries() {
        let mut linear = lane(
            (0..=10)
                .map(|n| point(n as u64 + 1, n, n as f64, SegmentShape::Linear))
                .collect(),
        );
        assert_eq!(linear.simplify(0.0).unwrap(), 9);
        assert_eq!(linear.points().len(), 2);
        let mut shaped = lane(vec![
            point(1, 0, 0.0, SegmentShape::Linear),
            point(2, 10, 1.0, SegmentShape::Hold),
            point(3, 20, 2.0, SegmentShape::Linear),
            point(4, 30, 3.0, SegmentShape::Linear),
        ]);
        shaped.simplify(100.0).unwrap();
        assert!(shaped.points().iter().any(|point| point.id.get() == 2));
        assert!(shaped.points().iter().any(|point| point.id.get() == 3));
    }

    #[test]
    fn write_modes_follow_daw_state_semantics() {
        let tick = WriterEvent::Tick {
            position: TimePosition::Frames(ProjectFrame(10)),
        };
        let mut read = AutomationWriter::new(WriteMode::Read, 0.5).unwrap();
        read.process(WriterEvent::TransportStarted).unwrap();
        read.process(WriterEvent::TouchStarted { value: 0.8 })
            .unwrap();
        assert_eq!(read.process(tick).unwrap(), None);

        let mut touch = AutomationWriter::new(WriteMode::Touch, 0.5).unwrap();
        touch.process(WriterEvent::TransportStarted).unwrap();
        assert_eq!(touch.process(tick).unwrap(), None);
        touch
            .process(WriterEvent::TouchStarted { value: 0.8 })
            .unwrap();
        assert!(matches!(
            touch.process(tick).unwrap(),
            Some(WriterAction::Write { value: 0.8, .. })
        ));
        assert_eq!(
            touch.process(WriterEvent::TouchEnded).unwrap(),
            Some(WriterAction::ResumeRead)
        );
        assert_eq!(touch.process(tick).unwrap(), None);

        let mut latch = AutomationWriter::new(WriteMode::Latch, 0.5).unwrap();
        latch.process(WriterEvent::TransportStarted).unwrap();
        latch
            .process(WriterEvent::TouchStarted { value: 0.9 })
            .unwrap();
        latch.process(WriterEvent::TouchEnded).unwrap();
        assert_eq!(latch.state(), WriterState::Latched);
        assert!(latch.process(tick).unwrap().is_some());
        assert_eq!(
            latch.process(WriterEvent::TransportStopped).unwrap(),
            Some(WriterAction::ResumeRead)
        );

        let mut write = AutomationWriter::new(WriteMode::Write, 0.4).unwrap();
        write.process(WriterEvent::TransportStarted).unwrap();
        assert!(write.process(tick).unwrap().is_some());
    }

    #[test]
    fn commands_are_atomic_invertible_and_conflict_checked() {
        let mut graph = AutomationGraph::new();
        graph.register_parameter(descriptor()).unwrap();
        let id = graph
            .create_lane("gain", address(), TimeDomain::Frames)
            .unwrap();
        let before = graph.lane(id).unwrap().clone();
        let mut after = before.clone();
        after.name = "volume".into();
        let command = AutomationCommand {
            label: "Rename automation".into(),
            changes: vec![LaneChange {
                before: Some(before.clone()),
                after: Some(after.clone()),
            }],
        };
        let inverse = graph.apply(&command).unwrap();
        assert_eq!(graph.lane(id), Some(&after));
        graph.apply(&inverse).unwrap();
        assert_eq!(graph.lane(id), Some(&before));
        graph.lanes.get_mut(&id).unwrap().name = "concurrent edit".into();
        assert_eq!(
            graph.apply(&command),
            Err(AutomationError::CommandConflict(id))
        );
    }

    #[test]
    fn multi_lane_command_conflict_changes_nothing() {
        let mut graph = AutomationGraph::new();
        graph.register_parameter(descriptor()).unwrap();
        let first = graph
            .create_lane("first", address(), TimeDomain::Frames)
            .unwrap();
        let second = graph
            .create_lane("second", address(), TimeDomain::Frames)
            .unwrap();
        let original = graph.clone();
        let mut first_after = graph.lane(first).unwrap().clone();
        first_after.name = "first edited".into();
        let mut stale_second = graph.lane(second).unwrap().clone();
        stale_second.name = "not current".into();
        let mut second_after = stale_second.clone();
        second_after.name = "second edited".into();
        let command = AutomationCommand {
            label: "conflicting transaction".into(),
            changes: vec![
                LaneChange {
                    before: graph.lane(first).cloned(),
                    after: Some(first_after),
                },
                LaneChange {
                    before: Some(stale_second),
                    after: Some(second_after),
                },
            ],
        };
        assert_eq!(
            graph.apply(&command),
            Err(AutomationError::CommandConflict(second))
        );
        assert_eq!(graph, original);
    }

    #[test]
    fn malformed_paste_is_atomic_and_does_not_burn_ids() {
        let mut graph = AutomationGraph::new();
        graph.register_parameter(descriptor()).unwrap();
        let id = graph
            .create_lane("gain", address(), TimeDomain::Frames)
            .unwrap();
        graph
            .insert_point(
                id,
                TimePosition::Frames(ProjectFrame(5)),
                1.0,
                SegmentShape::Linear,
            )
            .unwrap();
        let before = graph.clone();
        let malformed = AutomationClipboard {
            domain: TimeDomain::Frames,
            duration: 10,
            points: vec![ClipboardPoint {
                offset: 11,
                value: 2.0,
                outgoing: SegmentShape::Linear,
            }],
        };
        assert_eq!(
            graph.paste_into_lane(
                id,
                TimePosition::Frames(ProjectFrame(0)),
                &malformed,
                ValueScale::IDENTITY,
            ),
            Err(AutomationError::InvalidClipboard)
        );
        assert_eq!(graph, before);
        let next = graph
            .insert_point(
                id,
                TimePosition::Frames(ProjectFrame(6)),
                2.0,
                SegmentShape::Linear,
            )
            .unwrap();
        assert_eq!(next.get(), 2);
    }

    #[test]
    fn history_undo_redo_restores_exact_lane() {
        let mut graph = AutomationGraph::new();
        graph.register_parameter(descriptor()).unwrap();
        let id = graph
            .create_lane("gain", address(), TimeDomain::Frames)
            .unwrap();
        let before = graph.lane(id).unwrap().clone();
        let mut after = before.clone();
        after.enabled = false;
        let mut history = AutomationHistory::new(graph, 32);
        history
            .execute(AutomationCommand {
                label: "Disable lane".into(),
                changes: vec![LaneChange {
                    before: Some(before.clone()),
                    after: Some(after.clone()),
                }],
            })
            .unwrap();
        assert_eq!(history.graph().lane(id), Some(&after));
        assert!(history.undo().unwrap());
        assert_eq!(history.graph().lane(id), Some(&before));
        assert!(history.redo().unwrap());
        assert_eq!(history.graph().lane(id), Some(&after));
    }

    #[test]
    fn randomized_linear_interpolation_is_bounded_and_block_exact() {
        let descriptor = descriptor();
        let tempo = FixedTempo::new(44_100, 123_456_000).unwrap();
        let mut seed = 0x1234_5678_9abc_def0_u64;
        for _case in 0..128 {
            let mut points = Vec::new();
            let mut time = -10_000_i64;
            for id in 1..=32 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                time += 1 + ((seed >> 32) % 997) as i64;
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let value = -60.0 + (seed as f64 / u64::MAX as f64) * 72.0;
                points.push(point(id, time, value, SegmentShape::Linear));
            }
            let lane = lane(points);
            let compiled = lane.compile(&descriptor, &tempo).unwrap();
            let start = ProjectFrame(-12_000);
            let mut block = vec![0.0; 25_000];
            compiled.fill_block(start, &mut block);
            for (index, value) in block.into_iter().enumerate() {
                assert!(descriptor.minimum <= value && value <= descriptor.maximum);
                assert_eq!(
                    Some(value),
                    compiled.value_at(ProjectFrame(start.0 + index as i64))
                );
            }
        }
    }

    #[test]
    fn malformed_data_is_rejected() {
        let mut invalid = descriptor();
        invalid.maximum = invalid.minimum;
        assert_eq!(invalid.validate(), Err(AutomationError::InvalidRange));
        let mut lane = lane(vec![]);
        assert_eq!(
            lane.insert_point(AutomationPoint {
                id: 1.into(),
                position: TimePosition::Beats(BeatTime(0)),
                value: 0.0,
                outgoing: SegmentShape::Linear
            }),
            Err(AutomationError::TimeDomainMismatch)
        );
        assert_eq!(
            lane.insert_point(point(1, 0, f64::NAN, SegmentShape::Linear)),
            Err(AutomationError::NonFiniteValue)
        );
        assert_eq!(
            AutomationWriter::new(WriteMode::Read, f64::INFINITY),
            Err(AutomationError::NonFiniteValue)
        );
    }
}
