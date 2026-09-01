//! Musical-time sequencing primitives for audec's constructive DAW side.
//!
//! This module is deliberately UI and audio-backend independent. Musical data
//! stays in signed PPQ ticks and is compiled into exact, half-open project-frame
//! windows before it reaches a realtime graph. Probability and humanization
//! are stateless hashes of stable identities, so changing the callback block
//! size cannot change a performance.

use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;

use crate::reconstruction::ReconstructionProposalId;

pub const PPQ: i64 = 960;
const MICROS_PER_SECOND: i128 = 1_000_000;

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
    };
}

typed_id!(PatternId);
typed_id!(PatternClipId);
typed_id!(NoteId);
typed_id!(StepLaneId);
typed_id!(SampleAssetId);

/// Stable content fingerprint used by expression-backed pattern provenance.
///
/// It is deliberately an opaque value here. The pattern language owns the
/// hashing algorithm; persistence adapters only round-trip the two words.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PatternTermHash(pub u128);

/// Durable account of how a pattern definition was produced.
///
/// Expression bindings are stored as values, not merely represented by a
/// hash: evaluating alternations on later placement cycles may require a
/// binding that did not produce an event in cycle zero. `bindings_hash`
/// remains an inexpensive integrity/identity check for persistence adapters.
#[derive(Clone, Debug, PartialEq)]
pub enum PatternOrigin {
    Authored,
    Expression {
        source: String,
        term_hash: PatternTermHash,
        bindings_hash: PatternTermHash,
        bindings: BTreeMap<String, TriggerTarget>,
        /// Once true, ordinary content edits never silently clear it.
        diverged: bool,
    },
    Deprojected {
        proposal: ReconstructionProposalId,
        diverged: bool,
    },
}

impl Default for PatternOrigin {
    /// The explicit old-file contract: absence of provenance means the user
    /// authored the stored events. A codec should call this default when its
    /// optional origin member is absent.
    fn default() -> Self {
        Self::Authored
    }
}

impl PatternOrigin {
    pub fn diverged(&self) -> bool {
        match self {
            Self::Authored => false,
            Self::Expression { diverged, .. } | Self::Deprojected { diverged, .. } => *diverged,
        }
    }

    pub fn mark_diverged(&mut self) {
        match self {
            Self::Authored => {}
            Self::Expression { diverged, .. } | Self::Deprojected { diverged, .. } => {
                *diverged = true;
            }
        }
    }
}

/// Signed musical time at [`PPQ`] ticks per quarter note.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BeatTime(pub i64);

impl BeatTime {
    pub const ZERO: Self = Self(0);

    pub const fn ticks(self) -> i64 {
        self.0
    }

    pub fn saturating_add(self, delta: i64) -> Self {
        Self(self.0.saturating_add(delta))
    }
}

/// Non-negative musical duration in PPQ ticks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BeatDuration(pub u64);

impl BeatDuration {
    pub const fn ticks(self) -> u64 {
        self.0
    }
}

/// Signed, exact position in PCM frames. Negative values support preroll.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectFrame(pub i64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameRange {
    pub start: ProjectFrame,
    pub end: ProjectFrame,
}

impl FrameRange {
    pub fn new(start: ProjectFrame, end: ProjectFrame) -> Result<Self, SequencerError> {
        if start.0 >= end.0 {
            return Err(SequencerError::InvalidRange);
        }
        Ok(Self { start, end })
    }

    pub fn len(self) -> u64 {
        self.end.0.saturating_sub(self.start.0) as u64
    }

    pub fn contains(self, frame: ProjectFrame) -> bool {
        self.start <= frame && frame < self.end
    }
}

/// Integer tempo representation avoids unstable floating-point accumulation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tempo {
    pub micros_per_quarter: u32,
}

impl Tempo {
    pub const fn from_micros_per_quarter(micros: u32) -> Self {
        Self {
            micros_per_quarter: micros,
        }
    }

    pub fn from_bpm(bpm: f64) -> Result<Self, SequencerError> {
        if !bpm.is_finite() || bpm <= 0.0 {
            return Err(SequencerError::InvalidTempo);
        }
        let micros = (60_000_000.0 / bpm).round();
        if !(1.0..=u32::MAX as f64).contains(&micros) {
            return Err(SequencerError::InvalidTempo);
        }
        Ok(Self::from_micros_per_quarter(micros as u32))
    }

    pub fn bpm(self) -> f64 {
        60_000_000.0 / self.micros_per_quarter as f64
    }

    fn validate(self) -> Result<(), SequencerError> {
        if self.micros_per_quarter == 0 {
            Err(SequencerError::InvalidTempo)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeSignature {
    pub numerator: u16,
    pub denominator: u16,
}

impl TimeSignature {
    pub fn new(numerator: u16, denominator: u16) -> Result<Self, SequencerError> {
        let value = Self {
            numerator,
            denominator,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn ticks_per_beat(self) -> i64 {
        PPQ * 4 / self.denominator as i64
    }

    pub fn ticks_per_bar(self) -> i64 {
        self.ticks_per_beat() * self.numerator as i64
    }

    fn validate(self) -> Result<(), SequencerError> {
        if self.numerator == 0
            || !self.denominator.is_power_of_two()
            || self.denominator as i64 > PPQ * 4
            || (PPQ * 4) % self.denominator as i64 != 0
        {
            return Err(SequencerError::InvalidTimeSignature);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TempoPoint {
    pub at: BeatTime,
    pub tempo: Tempo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MeterPoint {
    pub at: BeatTime,
    pub signature: TimeSignature,
}

/// Zero-based bar/beat/tick display coordinate. Bars may be negative in preroll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MusicalPosition {
    pub bar: i64,
    pub beat: u16,
    pub tick: u16,
}

/// Step-tempo and meter map. The first point in each map is always at tick zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TempoMap {
    sample_rate: u32,
    tempos: Vec<TempoPoint>,
    meters: Vec<MeterPoint>,
}

impl TempoMap {
    pub fn new(
        sample_rate: u32,
        initial_tempo: Tempo,
        initial_meter: TimeSignature,
    ) -> Result<Self, SequencerError> {
        if sample_rate == 0 {
            return Err(SequencerError::InvalidSampleRate);
        }
        initial_tempo.validate()?;
        initial_meter.validate()?;
        Ok(Self {
            sample_rate,
            tempos: vec![TempoPoint {
                at: BeatTime::ZERO,
                tempo: initial_tempo,
            }],
            meters: vec![MeterPoint {
                at: BeatTime::ZERO,
                signature: initial_meter,
            }],
        })
    }

    pub fn common_time(sample_rate: u32, bpm: f64) -> Result<Self, SequencerError> {
        Self::new(
            sample_rate,
            Tempo::from_bpm(bpm)?,
            TimeSignature::new(4, 4)?,
        )
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn tempo_points(&self) -> &[TempoPoint] {
        &self.tempos
    }

    pub fn meter_points(&self) -> &[MeterPoint] {
        &self.meters
    }

    pub fn set_tempo(&mut self, at: BeatTime, tempo: Tempo) -> Result<(), SequencerError> {
        if at.0 < 0 {
            return Err(SequencerError::MapPointBeforeZero);
        }
        tempo.validate()?;
        match self.tempos.binary_search_by_key(&at, |point| point.at) {
            Ok(index) => self.tempos[index].tempo = tempo,
            Err(index) => self.tempos.insert(index, TempoPoint { at, tempo }),
        }
        Ok(())
    }

    /// Inserts a meter change only at a bar boundary of the preceding meter.
    pub fn set_meter(
        &mut self,
        at: BeatTime,
        signature: TimeSignature,
    ) -> Result<(), SequencerError> {
        if at.0 < 0 {
            return Err(SequencerError::MapPointBeforeZero);
        }
        signature.validate()?;
        let mut candidate = self.meters.clone();
        match candidate.binary_search_by_key(&at, |point| point.at) {
            Ok(index) => candidate[index].signature = signature,
            Err(index) => candidate.insert(index, MeterPoint { at, signature }),
        }
        validate_meter_boundaries(&candidate)?;
        self.meters = candidate;
        Ok(())
    }

    pub fn tempo_at(&self, at: BeatTime) -> Tempo {
        let index = upper_bound_by_tick(&self.tempos, at, |point| point.at).saturating_sub(1);
        self.tempos[index].tempo
    }

    pub fn meter_at(&self, at: BeatTime) -> TimeSignature {
        let index = upper_bound_by_tick(&self.meters, at, |point| point.at).saturating_sub(1);
        self.meters[index].signature
    }

    /// Converts a musical tick to the PCM frame at or immediately before it.
    ///
    /// Integration stays as an exact rational until the final Euclidean floor,
    /// avoiding drift across arbitrarily many tempo changes.
    pub fn beat_to_frame(&self, beat: BeatTime) -> ProjectFrame {
        let numerator = if beat.0 < 0 {
            beat.0 as i128
                * self.tempos[0].tempo.micros_per_quarter as i128
                * self.sample_rate as i128
        } else {
            let mut total = 0_i128;
            let mut cursor = 0_i64;
            let mut tempo = self.tempos[0].tempo;
            for point in self.tempos.iter().skip(1) {
                if point.at.0 >= beat.0 {
                    break;
                }
                total += (point.at.0 - cursor) as i128
                    * tempo.micros_per_quarter as i128
                    * self.sample_rate as i128;
                cursor = point.at.0;
                tempo = point.tempo;
            }
            total
                + (beat.0 - cursor) as i128
                    * tempo.micros_per_quarter as i128
                    * self.sample_rate as i128
        };
        let denominator = PPQ as i128 * MICROS_PER_SECOND;
        ProjectFrame(saturating_i128_to_i64(numerator.div_euclid(denominator)))
    }

    /// Returns the greatest musical tick whose compiled frame is `<= frame`.
    /// This definition is stable even when several ticks collapse onto one PCM
    /// frame at unusual sample rates or extreme tempos.
    pub fn frame_to_beat_floor(&self, frame: ProjectFrame) -> BeatTime {
        let mut low = i64::MIN as i128;
        let mut high = i64::MAX as i128;
        while low < high {
            let mid = low + (high - low + 1) / 2;
            if self.beat_to_frame(BeatTime(mid as i64)) <= frame {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        BeatTime(low as i64)
    }

    /// Returns the least musical tick whose compiled frame is `>= frame`.
    fn first_beat_at_or_after_frame(&self, frame: ProjectFrame) -> BeatTime {
        if frame.0 == i64::MIN {
            return BeatTime(i64::MIN);
        }
        let prior = self.frame_to_beat_floor(ProjectFrame(frame.0 - 1));
        BeatTime(prior.0.saturating_add(1))
    }

    pub fn musical_position(&self, at: BeatTime) -> MusicalPosition {
        if at.0 < 0 {
            return position_within_segment(at.0, 0, 0, self.meters[0].signature);
        }
        let index = upper_bound_by_tick(&self.meters, at, |point| point.at).saturating_sub(1);
        let start_bar = self.meter_start_bar(index);
        position_within_segment(
            at.0,
            self.meters[index].at.0,
            start_bar,
            self.meters[index].signature,
        )
    }

    pub fn beat_at_position(&self, position: MusicalPosition) -> Result<BeatTime, SequencerError> {
        let (index, start_bar) = if position.bar < 0 {
            (0, 0)
        } else {
            let mut selected = (0, 0);
            for index in 1..self.meters.len() {
                let bar = self.meter_start_bar(index);
                if bar > position.bar {
                    break;
                }
                selected = (index, bar);
            }
            selected
        };
        let meter = self.meters[index];
        if position.beat >= meter.signature.numerator
            || position.tick as i64 >= meter.signature.ticks_per_beat()
        {
            return Err(SequencerError::InvalidMusicalPosition);
        }
        let tick = meter.at.0 as i128
            + (position.bar - start_bar) as i128 * meter.signature.ticks_per_bar() as i128
            + position.beat as i128 * meter.signature.ticks_per_beat() as i128
            + position.tick as i128;
        Ok(BeatTime(saturating_i128_to_i64(tick)))
    }

    fn meter_start_bar(&self, index: usize) -> i64 {
        let mut bars = 0_i64;
        for pair in self.meters[..=index].windows(2) {
            bars = bars
                .saturating_add((pair[1].at.0 - pair[0].at.0) / pair[0].signature.ticks_per_bar());
        }
        bars
    }
}

fn position_within_segment(
    tick: i64,
    segment_tick: i64,
    segment_bar: i64,
    signature: TimeSignature,
) -> MusicalPosition {
    let relative = tick as i128 - segment_tick as i128;
    let ticks_per_bar = signature.ticks_per_bar() as i128;
    let ticks_per_beat = signature.ticks_per_beat() as i128;
    let bar_offset = relative.div_euclid(ticks_per_bar);
    let within_bar = relative.rem_euclid(ticks_per_bar);
    MusicalPosition {
        bar: saturating_i128_to_i64(segment_bar as i128 + bar_offset),
        beat: (within_bar / ticks_per_beat) as u16,
        tick: (within_bar % ticks_per_beat) as u16,
    }
}

fn validate_meter_boundaries(points: &[MeterPoint]) -> Result<(), SequencerError> {
    if points.first().map(|point| point.at) != Some(BeatTime::ZERO) {
        return Err(SequencerError::MissingMapOrigin);
    }
    for pair in points.windows(2) {
        if pair[1].at.0 <= pair[0].at.0
            || (pair[1].at.0 - pair[0].at.0) % pair[0].signature.ticks_per_bar() != 0
        {
            return Err(SequencerError::MeterChangeNotAtBar);
        }
    }
    Ok(())
}

fn upper_bound_by_tick<T>(values: &[T], tick: BeatTime, key: impl Fn(&T) -> BeatTime) -> usize {
    values.partition_point(|value| key(value) <= tick)
}

fn saturating_i128_to_i64(value: i128) -> i64 {
    value.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NotePitch {
    pub midi_key: u8,
    pub cents: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Articulation {
    Normal,
    Staccato,
    Tenuto,
    Legato,
    Accent,
    Named(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExpressionPoint {
    /// Normalized position through the note, from zero to one.
    pub position: f32,
    pub value: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PerNoteExpression {
    pub pitch_cents: Vec<ExpressionPoint>,
    pub pressure: Vec<ExpressionPoint>,
    pub timbre: Vec<ExpressionPoint>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NoteEvent {
    pub id: NoteId,
    pub start: BeatTime,
    pub duration: BeatDuration,
    pub pitch: NotePitch,
    pub velocity: f32,
    pub release_velocity: f32,
    pub pan: f32,
    pub probability: f32,
    pub micro_offset: i32,
    pub channel: u8,
    /// Stable built-in instrument identity. `None` is retained only for
    /// backward-compatible decoding of older projects and is deliberately
    /// silent in the built-in engine rather than broadcast to every synth.
    pub instrument: Option<u64>,
    pub articulation: Articulation,
    pub expression: PerNoteExpression,
}

impl NoteEvent {
    pub fn validate(&self) -> Result<(), SequencerError> {
        if self.start.0 < 0
            || self.duration.0 == 0
            || !unit(self.velocity)
            || !unit(self.release_velocity)
            || !unit(self.probability)
            || !bipolar(self.pan)
            || !self.pitch.cents.is_finite()
            || self.channel > 15
            || !valid_expression(&self.expression)
        {
            return Err(SequencerError::InvalidNote(self.id));
        }
        Ok(())
    }

    fn sounding_duration(&self) -> u64 {
        match self.articulation {
            Articulation::Staccato => (self.duration.0 / 2).max(1),
            Articulation::Tenuto => self.duration.0.saturating_add(self.duration.0 / 20),
            _ => self.duration.0,
        }
    }
}

fn valid_expression(expression: &PerNoteExpression) -> bool {
    [
        &expression.pitch_cents,
        &expression.pressure,
        &expression.timbre,
    ]
    .into_iter()
    .all(|curve| {
        curve
            .iter()
            .all(|point| unit(point.position) && point.value.is_finite())
            && curve
                .windows(2)
                .all(|pair| pair[0].position <= pair[1].position)
    })
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NotePattern {
    pub notes: BTreeMap<NoteId, NoteEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TriggerTarget {
    InstrumentNote { instrument: u64, key: u8 },
    DrumPad { rack: u64, pad: u16 },
    Sample(SampleAssetId),
    AnalysisTemplate(u64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct StepEvent {
    pub velocity: f32,
    pub probability: f32,
    pub micro_offset: i32,
    pub gate: BeatDuration,
    /// Total hits at this step, including the first one.
    pub ratchets: u8,
    pub pitch_semitones: f32,
    pub pan: f32,
}

impl StepEvent {
    fn validate(&self) -> bool {
        unit(self.velocity)
            && unit(self.probability)
            && bipolar(self.pan)
            && self.pitch_semitones.is_finite()
            && self.ratchets > 0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StepLane {
    pub id: StepLaneId,
    pub name: String,
    pub target: TriggerTarget,
    pub choke_group: Option<u32>,
    pub steps: BTreeMap<u32, StepEvent>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StepPattern {
    pub resolution: BeatDuration,
    /// Delays odd grid positions by up to half a step, in `[0, 1]`.
    pub swing: f32,
    pub lanes: BTreeMap<StepLaneId, StepLane>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PatternContent {
    Notes(NotePattern),
    Steps(StepPattern),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PatternDefinition {
    pub id: PatternId,
    pub name: String,
    pub length: BeatDuration,
    pub content: PatternContent,
    pub origin: PatternOrigin,
    pub revision: u64,
}

impl PatternDefinition {
    pub fn validate(&self) -> Result<(), SequencerError> {
        if self.length.0 == 0 || self.length.0 > i64::MAX as u64 {
            return Err(SequencerError::InvalidPattern(self.id));
        }
        if let PatternOrigin::Expression {
            source,
            term_hash,
            bindings_hash,
            bindings,
            ..
        } = &self.origin
        {
            let term = crate::pattern_lang::parse(source)
                .map_err(|_| SequencerError::InvalidPattern(self.id))?;
            if crate::pattern_lang::term_hash(&term) != *term_hash
                || crate::pattern_lang::bindings_hash(bindings) != *bindings_hash
                || !crate::pattern_lang::referenced_bindings(&term)
                    .iter()
                    .all(|name| bindings.contains_key(name))
                || !matches!(self.content, PatternContent::Steps(_))
            {
                return Err(SequencerError::InvalidPattern(self.id));
            }
            crate::pattern_lang::eval_steps(
                &term,
                &crate::pattern_lang::EvalContext {
                    bindings,
                    cycle: self.length,
                    seed: 0,
                    cycle_index: 0,
                },
            )
            .map_err(|_| SequencerError::InvalidPattern(self.id))?;
        }
        match &self.content {
            PatternContent::Notes(pattern) => {
                for (id, note) in &pattern.notes {
                    if *id != note.id || note.start.0 >= self.length.0 as i64 {
                        return Err(SequencerError::InvalidPattern(self.id));
                    }
                    note.validate()?;
                }
            }
            PatternContent::Steps(pattern) => {
                if pattern.resolution.0 == 0 || !unit(pattern.swing) {
                    return Err(SequencerError::InvalidPattern(self.id));
                }
                for (lane_id, lane) in &pattern.lanes {
                    if *lane_id != lane.id {
                        return Err(SequencerError::InvalidPattern(self.id));
                    }
                    for (step, event) in &lane.steps {
                        if *step as u128 * pattern.resolution.0 as u128 >= self.length.0 as u128
                            || !event.validate()
                        {
                            return Err(SequencerError::InvalidPattern(self.id));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PatternLibrary {
    patterns: BTreeMap<PatternId, PatternDefinition>,
}

impl PatternLibrary {
    pub fn patterns(&self) -> impl ExactSizeIterator<Item = &PatternDefinition> {
        self.patterns.values()
    }

    pub fn get(&self, id: PatternId) -> Option<&PatternDefinition> {
        self.patterns.get(&id)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PatternClip {
    pub id: PatternClipId,
    pub pattern: PatternId,
    pub start: BeatTime,
    pub length: BeatDuration,
    /// Point in the definition placed at the clip's start.
    pub pattern_offset: BeatTime,
    pub looped: bool,
    pub transpose_semitones: f32,
    pub gain: f32,
    pub muted: bool,
}

impl PatternClip {
    pub fn end(&self) -> BeatTime {
        self.start
            .saturating_add(self.length.0.min(i64::MAX as u64) as i64)
    }

    fn validate(&self) -> Result<(), SequencerError> {
        if self.length.0 == 0
            || self.length.0 > i64::MAX as u64
            || self.end() <= self.start
            || self.pattern_offset.0 < 0
            || !self.transpose_semitones.is_finite()
            || !self.gain.is_finite()
            || self.gain < 0.0
        {
            return Err(SequencerError::InvalidClip(self.id));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QuantizeSpec {
    pub grid: BeatDuration,
    /// Zero leaves timing intact; one moves fully onto the grid.
    pub strength: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HumanizeSpec {
    pub maximum_ticks: i32,
    pub maximum_velocity_delta: f32,
    pub seed: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SwingSpec {
    /// Grid whose odd divisions are delayed.
    pub grid: BeatDuration,
    /// Zero is straight; one delays odd divisions by half a grid interval.
    pub amount: f32,
}

pub fn quantize_notes(
    pattern: &NotePattern,
    spec: QuantizeSpec,
) -> Result<NotePattern, SequencerError> {
    if spec.grid.0 == 0 || !unit(spec.strength) {
        return Err(SequencerError::InvalidTransform);
    }
    let grid = spec.grid.0.min(i64::MAX as u64) as i64;
    let mut result = pattern.clone();
    for note in result.notes.values_mut() {
        let nearest = nearest_grid(note.start.0, grid);
        let delta = nearest.saturating_sub(note.start.0) as f64 * spec.strength as f64;
        note.start.0 = note.start.0.saturating_add(delta.round() as i64).max(0);
    }
    Ok(result)
}

/// Deterministically varies note timing and velocity without storing RNG state.
pub fn humanize_notes(
    pattern: &NotePattern,
    spec: HumanizeSpec,
) -> Result<NotePattern, SequencerError> {
    if spec.maximum_ticks < 0
        || !spec.maximum_velocity_delta.is_finite()
        || !(0.0..=1.0).contains(&spec.maximum_velocity_delta)
    {
        return Err(SequencerError::InvalidTransform);
    }
    let mut result = pattern.clone();
    for note in result.notes.values_mut() {
        let timing = signed_unit(hash64(spec.seed ^ note.id.get() ^ 0xa8f9_01e4));
        let velocity = signed_unit(hash64(spec.seed ^ note.id.get() ^ 0x7d39_b614));
        let tick_delta = (timing * spec.maximum_ticks as f64).round() as i32;
        note.micro_offset = note.micro_offset.saturating_add(tick_delta);
        note.velocity = (note.velocity + (velocity * spec.maximum_velocity_delta as f64) as f32)
            .clamp(0.0, 1.0);
    }
    Ok(result)
}

pub fn swing_notes(pattern: &NotePattern, spec: SwingSpec) -> Result<NotePattern, SequencerError> {
    if spec.grid.0 == 0 || spec.grid.0 > i64::MAX as u64 || !unit(spec.amount) {
        return Err(SequencerError::InvalidTransform);
    }
    let grid = spec.grid.0 as i64;
    let delay = (grid as f64 * 0.5 * spec.amount as f64).round() as i32;
    let mut result = pattern.clone();
    for note in result.notes.values_mut() {
        if note.start.0.div_euclid(grid).rem_euclid(2) == 1 {
            note.micro_offset = note.micro_offset.saturating_add(delay);
        }
    }
    Ok(result)
}

/// Humanizes a step pattern without changing lane/step identities or its grid.
pub fn humanize_steps(
    pattern: &StepPattern,
    spec: HumanizeSpec,
) -> Result<StepPattern, SequencerError> {
    if spec.maximum_ticks < 0
        || !spec.maximum_velocity_delta.is_finite()
        || !(0.0..=1.0).contains(&spec.maximum_velocity_delta)
    {
        return Err(SequencerError::InvalidTransform);
    }
    let mut result = pattern.clone();
    for lane in result.lanes.values_mut() {
        for (step_index, step) in &mut lane.steps {
            let identity = lane.id.get().rotate_left(17) ^ *step_index as u64;
            let timing = signed_unit(hash64(spec.seed ^ identity ^ 0xa8f9_01e4));
            let velocity = signed_unit(hash64(spec.seed ^ identity ^ 0x7d39_b614));
            let tick_delta = (timing * spec.maximum_ticks as f64).round() as i32;
            step.micro_offset = step.micro_offset.saturating_add(tick_delta);
            step.velocity = (step.velocity
                + (velocity * spec.maximum_velocity_delta as f64) as f32)
                .clamp(0.0, 1.0);
        }
    }
    Ok(result)
}

fn nearest_grid(value: i64, grid: i64) -> i64 {
    let lower = value.div_euclid(grid);
    let remainder = value.rem_euclid(grid);
    let index = lower.saturating_add(i64::from(remainder.saturating_mul(2) >= grid));
    index.saturating_mul(grid)
}

#[derive(Clone, Debug, PartialEq)]
pub enum SequencerCommand {
    PutPattern {
        before: Option<PatternDefinition>,
        after: Option<PatternDefinition>,
    },
    PutClip {
        before: Option<PatternClip>,
        after: Option<PatternClip>,
    },
    SetTempoMap {
        before: TempoMap,
        after: TempoMap,
    },
}

impl SequencerCommand {
    /// Exact inverse used by both the standalone sequencer and the aggregate
    /// project command history.
    pub fn inverse(&self) -> Self {
        match self {
            Self::PutPattern { before, after } => Self::PutPattern {
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutClip { before, after } => Self::PutClip {
                before: after.clone(),
                after: before.clone(),
            },
            Self::SetTempoMap { before, after } => Self::SetTempoMap {
                before: after.clone(),
                after: before.clone(),
            },
        }
    }
}

#[derive(Clone, Debug)]
struct HistoryEntry {
    label: String,
    forward: Vec<SequencerCommand>,
    backward: Vec<SequencerCommand>,
}

/// Persistent musical sequencer model plus bounded, atomic undo history.
#[derive(Clone, Debug)]
pub struct Sequencer {
    tempo_map: TempoMap,
    patterns: PatternLibrary,
    clips: BTreeMap<PatternClipId, PatternClip>,
    next_pattern_id: u64,
    next_clip_id: u64,
    next_note_id: u64,
    next_lane_id: u64,
    revision: u64,
    history_limit: usize,
    undo: VecDeque<HistoryEntry>,
    redo: Vec<HistoryEntry>,
}

/// Durable high-water marks for every sequencer-owned identity space.
///
/// Commands carry the concrete IDs they allocate, while this state prevents a
/// save/reopen after deletion from reusing an identity that existed earlier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SequencerAllocatorState {
    pub next_pattern_id: u64,
    pub next_clip_id: u64,
    pub next_note_id: u64,
    pub next_lane_id: u64,
}

impl Sequencer {
    pub fn new(tempo_map: TempoMap) -> Self {
        Self {
            tempo_map,
            patterns: PatternLibrary::default(),
            clips: BTreeMap::new(),
            next_pattern_id: 1,
            next_clip_id: 1,
            next_note_id: 1,
            next_lane_id: 1,
            revision: 0,
            history_limit: 256,
            undo: VecDeque::new(),
            redo: Vec::new(),
        }
    }

    pub fn tempo_map(&self) -> &TempoMap {
        &self.tempo_map
    }

    pub fn patterns(&self) -> &PatternLibrary {
        &self.patterns
    }

    pub fn clips(&self) -> impl ExactSizeIterator<Item = &PatternClip> {
        self.clips.values()
    }

    pub fn clip(&self, id: PatternClipId) -> Option<&PatternClip> {
        self.clips.get(&id)
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub const fn allocator_state(&self) -> SequencerAllocatorState {
        SequencerAllocatorState {
            next_pattern_id: self.next_pattern_id,
            next_clip_id: self.next_clip_id,
            next_note_id: self.next_note_id,
            next_lane_id: self.next_lane_id,
        }
    }

    /// Restore persisted high-water marks without renumbering any entity.
    pub fn restore_allocator_state(
        &mut self,
        state: SequencerAllocatorState,
    ) -> Result<(), SequencerError> {
        let required = SequencerAllocatorState {
            next_pattern_id: self
                .patterns
                .patterns
                .keys()
                .map(|id| id.get())
                .max()
                .unwrap_or(0)
                .saturating_add(1),
            next_clip_id: self
                .clips
                .keys()
                .map(|id| id.get())
                .max()
                .unwrap_or(0)
                .saturating_add(1),
            next_note_id: self
                .patterns
                .patterns
                .values()
                .filter_map(|pattern| match &pattern.content {
                    PatternContent::Notes(notes) => notes.notes.keys().map(|id| id.get()).max(),
                    PatternContent::Steps(_) => None,
                })
                .max()
                .unwrap_or(0)
                .saturating_add(1),
            next_lane_id: self
                .patterns
                .patterns
                .values()
                .filter_map(|pattern| match &pattern.content {
                    PatternContent::Steps(steps) => steps.lanes.keys().map(|id| id.get()).max(),
                    PatternContent::Notes(_) => None,
                })
                .max()
                .unwrap_or(0)
                .saturating_add(1),
        };
        if state.next_pattern_id < required.next_pattern_id
            || state.next_clip_id < required.next_clip_id
            || state.next_note_id < required.next_note_id
            || state.next_lane_id < required.next_lane_id
            || state.next_pattern_id == 0
            || state.next_clip_id == 0
            || state.next_note_id == 0
            || state.next_lane_id == 0
        {
            return Err(SequencerError::InvalidAllocatorState);
        }
        self.next_pattern_id = state.next_pattern_id;
        self.next_clip_id = state.next_clip_id;
        self.next_note_id = state.next_note_id;
        self.next_lane_id = state.next_lane_id;
        Ok(())
    }

    pub fn allocate_pattern_id(&mut self) -> PatternId {
        let id = PatternId(self.next_pattern_id);
        self.next_pattern_id = self.next_pattern_id.saturating_add(1);
        id
    }

    pub fn allocate_clip_id(&mut self) -> PatternClipId {
        let id = PatternClipId(self.next_clip_id);
        self.next_clip_id = self.next_clip_id.saturating_add(1);
        id
    }

    pub fn allocate_note_id(&mut self) -> NoteId {
        let id = NoteId(self.next_note_id);
        self.next_note_id = self.next_note_id.saturating_add(1);
        id
    }

    pub fn allocate_step_lane_id(&mut self) -> StepLaneId {
        let id = StepLaneId(self.next_lane_id);
        self.next_lane_id = self.next_lane_id.saturating_add(1);
        id
    }

    pub fn set_history_limit(&mut self, limit: usize) {
        self.history_limit = limit;
        while self.undo.len() > limit {
            self.undo.pop_front();
        }
        if self.redo.len() > limit {
            self.redo.drain(..self.redo.len() - limit);
        }
    }

    pub fn undo_label(&self) -> Option<&str> {
        self.undo.back().map(|entry| entry.label.as_str())
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|entry| entry.label.as_str())
    }

    /// Commits one validated transaction. `before` values act as optimistic
    /// concurrency guards and make each command exactly invertible.
    pub fn execute(
        &mut self,
        label: impl Into<String>,
        mut commands: Vec<SequencerCommand>,
    ) -> Result<u64, SequencerError> {
        if commands.is_empty() {
            return Ok(self.revision);
        }
        prepare_pattern_divergence(&mut commands);
        let mut candidate = self.clone_without_history();
        apply_commands(&mut candidate, &commands)?;
        candidate.validate()?;
        let backward = commands
            .iter()
            .rev()
            .map(SequencerCommand::inverse)
            .collect();
        self.tempo_map = candidate.tempo_map;
        self.patterns = candidate.patterns;
        self.clips = candidate.clips;
        self.next_pattern_id = self.next_pattern_id.max(candidate.next_pattern_id);
        self.next_clip_id = self.next_clip_id.max(candidate.next_clip_id);
        self.next_note_id = self.next_note_id.max(candidate.next_note_id);
        self.next_lane_id = self.next_lane_id.max(candidate.next_lane_id);
        self.revision = self.revision.saturating_add(1);
        self.redo.clear();
        if self.history_limit > 0 {
            self.undo.push_back(HistoryEntry {
                label: label.into(),
                forward: commands,
                backward,
            });
            while self.undo.len() > self.history_limit {
                self.undo.pop_front();
            }
        }
        Ok(self.revision)
    }

    /// Applies validated commands atomically without adding an entry to the
    /// sequencer-local undo history.
    ///
    /// This is the command kernel for an aggregate project controller. Any
    /// existing local history is cleared on publication because its entries
    /// describe a state lineage that an external command has replaced.
    pub fn apply_without_history(
        &mut self,
        commands: &[SequencerCommand],
    ) -> Result<u64, SequencerError> {
        if commands.is_empty() {
            return Ok(self.revision);
        }
        let mut commands = commands.to_vec();
        prepare_pattern_divergence(&mut commands);
        let mut candidate = self.clone_without_history();
        apply_commands(&mut candidate, &commands)?;
        candidate.validate()?;
        candidate.revision = candidate.revision.saturating_add(1);
        *self = candidate;
        Ok(self.revision)
    }

    pub fn undo(&mut self) -> Result<Option<u64>, SequencerError> {
        let Some(entry) = self.undo.pop_back() else {
            return Ok(None);
        };
        let mut candidate = self.clone_without_history();
        if let Err(error) =
            apply_commands(&mut candidate, &entry.backward).and_then(|_| candidate.validate())
        {
            self.undo.push_back(entry);
            return Err(error);
        }
        self.tempo_map = candidate.tempo_map;
        self.patterns = candidate.patterns;
        self.clips = candidate.clips;
        self.next_pattern_id = self.next_pattern_id.max(candidate.next_pattern_id);
        self.next_clip_id = self.next_clip_id.max(candidate.next_clip_id);
        self.next_note_id = self.next_note_id.max(candidate.next_note_id);
        self.next_lane_id = self.next_lane_id.max(candidate.next_lane_id);
        self.revision = self.revision.saturating_add(1);
        self.redo.push(entry);
        Ok(Some(self.revision))
    }

    pub fn redo(&mut self) -> Result<Option<u64>, SequencerError> {
        let Some(entry) = self.redo.pop() else {
            return Ok(None);
        };
        let mut candidate = self.clone_without_history();
        if let Err(error) =
            apply_commands(&mut candidate, &entry.forward).and_then(|_| candidate.validate())
        {
            self.redo.push(entry);
            return Err(error);
        }
        self.tempo_map = candidate.tempo_map;
        self.patterns = candidate.patterns;
        self.clips = candidate.clips;
        self.next_pattern_id = self.next_pattern_id.max(candidate.next_pattern_id);
        self.next_clip_id = self.next_clip_id.max(candidate.next_clip_id);
        self.next_note_id = self.next_note_id.max(candidate.next_note_id);
        self.next_lane_id = self.next_lane_id.max(candidate.next_lane_id);
        self.revision = self.revision.saturating_add(1);
        self.undo.push_back(entry);
        Ok(Some(self.revision))
    }

    pub fn validate(&self) -> Result<(), SequencerError> {
        for (id, pattern) in &self.patterns.patterns {
            if *id != pattern.id {
                return Err(SequencerError::InvalidPattern(*id));
            }
            pattern.validate()?;
        }
        for (id, clip) in &self.clips {
            if *id != clip.id {
                return Err(SequencerError::InvalidClip(*id));
            }
            clip.validate()?;
            let pattern = self
                .patterns
                .get(clip.pattern)
                .ok_or(SequencerError::MissingPattern(clip.pattern))?;
            if clip.pattern_offset.0 >= pattern.length.0 as i64 {
                return Err(SequencerError::InvalidClip(*id));
            }
            if !clip.looped
                && clip.pattern_offset.0 as u128 + clip.length.0 as u128 > pattern.length.0 as u128
            {
                return Err(SequencerError::InvalidClip(*id));
            }
        }
        Ok(())
    }

    fn clone_without_history(&self) -> Self {
        let mut cloned = self.clone();
        cloned.undo.clear();
        cloned.redo.clear();
        cloned
    }

    /// Schedules events in the exact half-open project range `[start, end)`.
    pub fn schedule_project_window(
        &self,
        range: FrameRange,
        performance_seed: u64,
    ) -> Vec<ScheduledEvent> {
        let mut result = Vec::new();
        self.schedule_span(range, 0, performance_seed, &mut result);
        sort_scheduled(&mut result);
        result
    }

    /// Schedules a realtime block, splitting it at every loop wrap. Loop end is
    /// excluded, loop start is replayed, and a boundary event precedes new note
    /// ons at the same callback offset so voices cannot remain stuck.
    pub fn schedule_transport_window(
        &self,
        playhead: ProjectFrame,
        frame_count: u32,
        loop_range: Option<FrameRange>,
        performance_seed: u64,
    ) -> Result<Vec<ScheduledEvent>, SequencerError> {
        if frame_count == 0 {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        let mut remaining = frame_count as u64;
        let mut block_offset = 0_u64;
        let mut position = playhead;
        while remaining > 0 {
            if let Some(loop_range) = loop_range {
                let loop_len = loop_range.len();
                if position >= loop_range.end {
                    position = ProjectFrame(
                        loop_range.start.0
                            + (position.0 - loop_range.start.0).rem_euclid(loop_len as i64),
                    );
                    result.push(ScheduledEvent {
                        block_offset: block_offset as u32,
                        project_frame: loop_range.end,
                        kind: ScheduledKind::LoopBoundary,
                    });
                }
                let until_wrap = loop_range.end.0.saturating_sub(position.0).max(1) as u64;
                let count = remaining.min(until_wrap);
                let end = ProjectFrame(position.0.saturating_add(count as i64));
                self.schedule_span(
                    FrameRange {
                        start: position,
                        end,
                    },
                    block_offset,
                    performance_seed,
                    &mut result,
                );
                remaining -= count;
                block_offset += count;
                position = end;
                if position == loop_range.end && remaining > 0 {
                    result.push(ScheduledEvent {
                        block_offset: block_offset as u32,
                        project_frame: loop_range.end,
                        kind: ScheduledKind::LoopBoundary,
                    });
                    position = loop_range.start;
                }
            } else {
                let end = ProjectFrame(position.0.saturating_add(remaining as i64));
                self.schedule_span(
                    FrameRange {
                        start: position,
                        end,
                    },
                    block_offset,
                    performance_seed,
                    &mut result,
                );
                remaining = 0;
            }
        }
        sort_scheduled(&mut result);
        Ok(result)
    }

    fn schedule_span(
        &self,
        range: FrameRange,
        block_base: u64,
        seed: u64,
        output: &mut Vec<ScheduledEvent>,
    ) {
        let tick_start = self.tempo_map.first_beat_at_or_after_frame(range.start).0;
        let tick_end = self.tempo_map.first_beat_at_or_after_frame(range.end).0;
        for clip in self.clips.values().filter(|clip| !clip.muted) {
            let pattern = &self.patterns.patterns[&clip.pattern];
            let pattern_len = pattern.length.0 as i64;
            let clip_len = clip.length.0 as i64;
            match &pattern.content {
                PatternContent::Notes(notes) => {
                    for note in notes.notes.values() {
                        let on_delta = note.micro_offset as i64;
                        let off_delta = on_delta
                            .saturating_add(note.sounding_duration().min(i64::MAX as u64) as i64);
                        for (base, cycle) in occurrence_bases(
                            clip,
                            note.start.0,
                            pattern_len,
                            clip_len,
                            tick_start.saturating_sub(on_delta.max(off_delta)),
                            tick_end.saturating_sub(on_delta.min(off_delta)),
                        ) {
                            let on_tick = base.saturating_add(note.micro_offset as i64);
                            if on_tick < clip.start.0 || on_tick >= clip.end().0 {
                                continue;
                            }
                            let identity =
                                performance_identity(seed, clip.id.get(), note.id.get(), cycle, 0);
                            if !passes_probability(note.probability, identity) {
                                continue;
                            }
                            let off_tick =
                                on_tick
                                    .saturating_add(
                                        note.sounding_duration().min(i64::MAX as u64) as i64
                                    )
                                    .min(clip.end().0);
                            let on_frame = self.tempo_map.beat_to_frame(BeatTime(on_tick));
                            let off_frame = self.tempo_map.beat_to_frame(BeatTime(off_tick));
                            push_if_in_range(
                                output,
                                range,
                                block_base,
                                on_frame,
                                ScheduledKind::NoteOn {
                                    clip: clip.id,
                                    note: note.id,
                                    instrument: note.instrument,
                                    pitch: NotePitch {
                                        midi_key: note.pitch.midi_key,
                                        cents: note.pitch.cents + clip.transpose_semitones * 100.0,
                                    },
                                    velocity: (note.velocity * clip.gain).clamp(0.0, 1.0),
                                    pan: note.pan,
                                    channel: note.channel,
                                    articulation: note.articulation.clone(),
                                },
                            );
                            push_if_in_range(
                                output,
                                range,
                                block_base,
                                off_frame,
                                ScheduledKind::NoteOff {
                                    clip: clip.id,
                                    note: note.id,
                                    instrument: note.instrument,
                                    release_velocity: note.release_velocity,
                                    channel: note.channel,
                                },
                            );
                            for (dimension, curve) in [
                                (
                                    ExpressionDimension::PitchCents,
                                    &note.expression.pitch_cents,
                                ),
                                (ExpressionDimension::Pressure, &note.expression.pressure),
                                (ExpressionDimension::Timbre, &note.expression.timbre),
                            ] {
                                for point in curve {
                                    let duration = off_tick.saturating_sub(on_tick);
                                    let offset =
                                        (duration as f64 * point.position as f64).round() as i64;
                                    let expression_tick = on_tick
                                        .saturating_add(offset)
                                        .min(off_tick.saturating_sub(1));
                                    push_if_in_range(
                                        output,
                                        range,
                                        block_base,
                                        self.tempo_map.beat_to_frame(BeatTime(expression_tick)),
                                        ScheduledKind::NoteExpression {
                                            clip: clip.id,
                                            note: note.id,
                                            instrument: note.instrument,
                                            dimension,
                                            value: point.value,
                                            channel: note.channel,
                                        },
                                    );
                                }
                            }
                        }
                    }
                }
                PatternContent::Steps(steps) => {
                    // Expression patterns are terms over a cycle, not a
                    // cycle-zero event cache. Realize every placement cycle
                    // that can intersect this scheduling window so `<...>`,
                    // `every`, and `slow` evolve while a clip loops.
                    if let PatternOrigin::Expression {
                        source,
                        bindings,
                        diverged: false,
                        ..
                    } = &pattern.origin
                    {
                        if let Ok(term) = crate::pattern_lang::parse(source) {
                            let cycles = placement_cycles(clip, pattern_len, tick_start, tick_end);
                            let realized: Result<Vec<_>, _> = cycles
                                .into_iter()
                                .map(|cycle| {
                                    crate::pattern_lang::eval_steps(
                                        &term,
                                        &crate::pattern_lang::EvalContext {
                                            bindings,
                                            cycle: pattern.length,
                                            seed,
                                            cycle_index: cycle as u64,
                                        },
                                    )
                                    .map(|output| (cycle, output.pattern))
                                })
                                .collect();
                            if let Ok(realized) = realized {
                                for (cycle, realized_steps) in realized {
                                    self.schedule_step_cycle(
                                        range,
                                        block_base,
                                        seed,
                                        clip,
                                        pattern_len,
                                        cycle,
                                        &realized_steps,
                                        output,
                                    );
                                }
                                continue;
                            }
                        }
                        // A damaged origin must not silence a project. Its
                        // validated stored realization is the safe fallback.
                    }
                    for lane in steps.lanes.values() {
                        for (step_index, step) in &lane.steps {
                            let local = *step_index as i64 * steps.resolution.0 as i64;
                            let swing = if step_index % 2 == 1 {
                                (steps.resolution.0 as f64 * 0.5 * steps.swing as f64).round()
                                    as i64
                            } else {
                                0
                            };
                            let ratchets = step.ratchets.max(1);
                            let spacing = if ratchets > 1 {
                                let extent = if step.gate.0 > 0 {
                                    step.gate.0
                                } else {
                                    steps.resolution.0
                                };
                                (extent / ratchets as u64).max(1) as i64
                            } else {
                                0
                            };
                            let first_delta = swing.saturating_add(step.micro_offset as i64);
                            let last_delta = first_delta
                                .saturating_add(spacing.saturating_mul(ratchets as i64 - 1));
                            for (base, cycle) in occurrence_bases(
                                clip,
                                local,
                                pattern_len,
                                clip_len,
                                tick_start.saturating_sub(first_delta.max(last_delta)),
                                tick_end.saturating_sub(first_delta.min(last_delta)),
                            ) {
                                let first = base
                                    .saturating_add(swing)
                                    .saturating_add(step.micro_offset as i64);
                                for ratchet in 0..ratchets {
                                    let tick = first.saturating_add(spacing * ratchet as i64);
                                    if tick < clip.start.0 || tick >= clip.end().0 {
                                        continue;
                                    }
                                    let identity = performance_identity(
                                        seed,
                                        clip.id.get(),
                                        lane.id.get() ^ *step_index as u64,
                                        cycle,
                                        ratchet,
                                    );
                                    if !passes_probability(step.probability, identity) {
                                        continue;
                                    }
                                    push_if_in_range(
                                        output,
                                        range,
                                        block_base,
                                        self.tempo_map.beat_to_frame(BeatTime(tick)),
                                        ScheduledKind::Trigger {
                                            clip: clip.id,
                                            lane: lane.id,
                                            target: lane.target.clone(),
                                            choke_group: lane.choke_group,
                                            velocity: (step.velocity * clip.gain).clamp(0.0, 1.0),
                                            pan: step.pan,
                                            pitch_semitones: step.pitch_semitones
                                                + clip.transpose_semitones,
                                            gate_frames: self
                                                .tempo_map
                                                .beat_to_frame(BeatTime(tick.saturating_add(
                                                    if ratchets > 1 {
                                                        spacing
                                                    } else {
                                                        step.gate.0.min(i64::MAX as u64) as i64
                                                    },
                                                )))
                                                .0
                                                .saturating_sub(
                                                    self.tempo_map.beat_to_frame(BeatTime(tick)).0,
                                                )
                                                .max(0)
                                                as u64,
                                            ratchet,
                                        },
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn schedule_step_cycle(
        &self,
        range: FrameRange,
        block_base: u64,
        seed: u64,
        clip: &PatternClip,
        pattern_len: i64,
        cycle: i64,
        steps: &StepPattern,
        output: &mut Vec<ScheduledEvent>,
    ) {
        for lane in steps.lanes.values() {
            for (step_index, step) in &lane.steps {
                let local = *step_index as i64 * steps.resolution.0 as i64;
                let swing = if step_index % 2 == 1 {
                    (steps.resolution.0 as f64 * 0.5 * steps.swing as f64).round() as i64
                } else {
                    0
                };
                let ratchets = step.ratchets.max(1);
                let spacing = if ratchets > 1 {
                    (step.gate.0.max(1) / ratchets as u64).max(1) as i64
                } else {
                    0
                };
                let base = clip
                    .start
                    .0
                    .saturating_add(local.saturating_sub(clip.pattern_offset.0))
                    .saturating_add(cycle.saturating_mul(pattern_len));
                let first = base
                    .saturating_add(swing)
                    .saturating_add(step.micro_offset as i64);
                for ratchet in 0..ratchets {
                    let tick = first.saturating_add(spacing.saturating_mul(ratchet as i64));
                    if tick < clip.start.0 || tick >= clip.end().0 {
                        continue;
                    }
                    let identity = performance_identity(
                        seed,
                        clip.id.get(),
                        lane.id.get() ^ *step_index as u64,
                        cycle,
                        ratchet,
                    );
                    if !passes_probability(step.probability, identity) {
                        continue;
                    }
                    let frame = self.tempo_map.beat_to_frame(BeatTime(tick));
                    push_if_in_range(
                        output,
                        range,
                        block_base,
                        frame,
                        ScheduledKind::Trigger {
                            clip: clip.id,
                            lane: lane.id,
                            target: lane.target.clone(),
                            choke_group: lane.choke_group,
                            velocity: (step.velocity * clip.gain).clamp(0.0, 1.0),
                            pan: step.pan,
                            pitch_semitones: step.pitch_semitones + clip.transpose_semitones,
                            gate_frames: self
                                .tempo_map
                                .beat_to_frame(BeatTime(tick.saturating_add(if ratchets > 1 {
                                    spacing
                                } else {
                                    step.gate.0.min(i64::MAX as u64) as i64
                                })))
                                .0
                                .saturating_sub(frame.0)
                                .max(0) as u64,
                            ratchet,
                        },
                    );
                }
            }
        }
    }
}

/// Candidate zero-based placement cycles whose normalized pattern window can
/// intersect `[tick_start, tick_end)`. One cycle of padding covers generated
/// swing/micro-offset residues around the boundary.
fn placement_cycles(
    clip: &PatternClip,
    pattern_len: i64,
    tick_start: i64,
    tick_end: i64,
) -> Vec<i64> {
    if !clip.looped {
        return vec![0];
    }
    let anchor = clip.start.0.saturating_sub(clip.pattern_offset.0);
    let first = tick_start
        .saturating_sub(anchor)
        .div_euclid(pattern_len)
        .saturating_sub(1)
        .max(0);
    let last = tick_end
        .saturating_sub(anchor)
        .div_euclid(pattern_len)
        .saturating_add(2);
    let clip_last = clip
        .end()
        .0
        .saturating_sub(anchor)
        .div_euclid(pattern_len)
        .saturating_add(1);
    (first..last.min(clip_last)).collect()
}

fn apply_commands(
    sequencer: &mut Sequencer,
    commands: &[SequencerCommand],
) -> Result<(), SequencerError> {
    for command in commands {
        match command {
            SequencerCommand::PutPattern { before, after } => {
                let id = before
                    .as_ref()
                    .map(|value| value.id)
                    .or_else(|| after.as_ref().map(|value| value.id))
                    .ok_or(SequencerError::EmptyCommand)?;
                if before.as_ref().map(|value| value.id) != before.as_ref().map(|_| id)
                    || after.as_ref().map(|value| value.id) != after.as_ref().map(|_| id)
                    || sequencer.patterns.patterns.get(&id) != before.as_ref()
                {
                    return Err(SequencerError::StaleCommand);
                }
                match after {
                    Some(value) => {
                        sequencer.patterns.patterns.insert(id, value.clone());
                        sequencer.next_pattern_id =
                            sequencer.next_pattern_id.max(id.get().saturating_add(1));
                        match &value.content {
                            PatternContent::Notes(notes) => {
                                if let Some(maximum) = notes.notes.keys().map(|id| id.get()).max() {
                                    sequencer.next_note_id =
                                        sequencer.next_note_id.max(maximum.saturating_add(1));
                                }
                            }
                            PatternContent::Steps(steps) => {
                                if let Some(maximum) = steps.lanes.keys().map(|id| id.get()).max() {
                                    sequencer.next_lane_id =
                                        sequencer.next_lane_id.max(maximum.saturating_add(1));
                                }
                            }
                        }
                    }
                    None => {
                        sequencer.patterns.patterns.remove(&id);
                    }
                }
            }
            SequencerCommand::PutClip { before, after } => {
                let id = before
                    .as_ref()
                    .map(|value| value.id)
                    .or_else(|| after.as_ref().map(|value| value.id))
                    .ok_or(SequencerError::EmptyCommand)?;
                if before.as_ref().map(|value| value.id) != before.as_ref().map(|_| id)
                    || after.as_ref().map(|value| value.id) != after.as_ref().map(|_| id)
                    || sequencer.clips.get(&id) != before.as_ref()
                {
                    return Err(SequencerError::StaleCommand);
                }
                match after {
                    Some(value) => {
                        sequencer.clips.insert(id, value.clone());
                        sequencer.next_clip_id =
                            sequencer.next_clip_id.max(id.get().saturating_add(1));
                    }
                    None => {
                        sequencer.clips.remove(&id);
                    }
                }
            }
            SequencerCommand::SetTempoMap { before, after } => {
                if &sequencer.tempo_map != before {
                    return Err(SequencerError::StaleCommand);
                }
                sequencer.tempo_map = after.clone();
            }
        }
    }
    Ok(())
}

fn prepare_pattern_divergence(commands: &mut [SequencerCommand]) {
    for command in commands {
        if let SequencerCommand::PutPattern {
            before,
            after: Some(after),
        } = command
        {
            mark_diverged_after_manual_edit(before.as_ref(), after);
        }
    }
}

/// Commands that preserve a generated origin while changing its realization
/// are ordinary grid edits and therefore diverge. Authoring regeneration
/// changes the expression identity (or explicitly clears an already-diverged
/// origin), so it remains distinguishable without adding an edit-only command
/// variant to the sequencer protocol.
fn mark_diverged_after_manual_edit(
    before: Option<&PatternDefinition>,
    after: &mut PatternDefinition,
) {
    let Some(before) = before else {
        return;
    };
    let realization_changed = before.length != after.length || before.content != after.content;
    if !realization_changed {
        return;
    }
    match (&before.origin, &mut after.origin) {
        (
            PatternOrigin::Expression {
                source: before_source,
                term_hash: before_term,
                bindings_hash: before_bindings,
                diverged: false,
                ..
            },
            PatternOrigin::Expression {
                source,
                term_hash,
                bindings_hash,
                diverged,
                ..
            },
        ) if source == before_source
            && term_hash == before_term
            && bindings_hash == before_bindings =>
        {
            *diverged = true;
        }
        (
            PatternOrigin::Deprojected {
                proposal: before_proposal,
                diverged: false,
            },
            PatternOrigin::Deprojected { proposal, diverged },
        ) if proposal == before_proposal => *diverged = true,
        _ => {}
    }
}

/// Returns `(absolute event tick before microtiming, repetition index)`.
fn occurrence_bases(
    clip: &PatternClip,
    local_tick: i64,
    pattern_len: i64,
    clip_len: i64,
    absolute_min: i64,
    absolute_max: i64,
) -> Vec<(i64, i64)> {
    let initial = local_tick.saturating_sub(clip.pattern_offset.0);
    let minimum_elapsed = absolute_min.saturating_sub(clip.start.0).max(0);
    let maximum_elapsed = absolute_max.saturating_sub(clip.start.0).min(clip_len);
    if minimum_elapsed >= maximum_elapsed {
        return Vec::new();
    }
    if !clip.looped {
        return (initial >= minimum_elapsed && initial < maximum_elapsed)
            .then_some((clip.start.0.saturating_add(initial), 0))
            .into_iter()
            .collect();
    }
    let cycle_numerator = minimum_elapsed.saturating_sub(initial);
    let first_cycle = if cycle_numerator > 0 {
        cycle_numerator.saturating_add(pattern_len - 1) / pattern_len
    } else {
        0
    };
    let mut result = Vec::new();
    let mut cycle = first_cycle;
    loop {
        let elapsed = initial.saturating_add(cycle.saturating_mul(pattern_len));
        if elapsed >= maximum_elapsed {
            break;
        }
        if elapsed >= minimum_elapsed {
            result.push((clip.start.0.saturating_add(elapsed), cycle));
        }
        cycle = cycle.saturating_add(1);
        if cycle == i64::MAX {
            break;
        }
    }
    result
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScheduledEvent {
    /// Frame offset in the requested callback window.
    pub block_offset: u32,
    /// Source project frame (loop boundary retains the exclusive loop end).
    pub project_frame: ProjectFrame,
    pub kind: ScheduledKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ScheduledKind {
    LoopBoundary,
    NoteOff {
        clip: PatternClipId,
        note: NoteId,
        instrument: Option<u64>,
        release_velocity: f32,
        channel: u8,
    },
    NoteOn {
        clip: PatternClipId,
        note: NoteId,
        instrument: Option<u64>,
        pitch: NotePitch,
        velocity: f32,
        pan: f32,
        channel: u8,
        articulation: Articulation,
    },
    NoteExpression {
        clip: PatternClipId,
        note: NoteId,
        instrument: Option<u64>,
        dimension: ExpressionDimension,
        value: f32,
        channel: u8,
    },
    Trigger {
        clip: PatternClipId,
        lane: StepLaneId,
        target: TriggerTarget,
        choke_group: Option<u32>,
        velocity: f32,
        pan: f32,
        pitch_semitones: f32,
        gate_frames: u64,
        ratchet: u8,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpressionDimension {
    PitchCents,
    Pressure,
    Timbre,
}

fn push_if_in_range(
    output: &mut Vec<ScheduledEvent>,
    range: FrameRange,
    block_base: u64,
    frame: ProjectFrame,
    kind: ScheduledKind,
) {
    if range.contains(frame) {
        output.push(ScheduledEvent {
            block_offset: block_base
                .saturating_add(frame.0.saturating_sub(range.start.0) as u64)
                .min(u32::MAX as u64) as u32,
            project_frame: frame,
            kind,
        });
    }
}

fn sort_scheduled(events: &mut [ScheduledEvent]) {
    events.sort_by(|left, right| {
        left.block_offset
            .cmp(&right.block_offset)
            .then_with(|| event_priority(&left.kind).cmp(&event_priority(&right.kind)))
            .then_with(|| stable_event_key(&left.kind).cmp(&stable_event_key(&right.kind)))
    });
}

fn event_priority(event: &ScheduledKind) -> u8 {
    match event {
        ScheduledKind::LoopBoundary => 0,
        ScheduledKind::NoteOff { .. } => 1,
        ScheduledKind::NoteOn { .. } => 2,
        ScheduledKind::NoteExpression { .. } => 3,
        ScheduledKind::Trigger { .. } => 4,
    }
}

fn stable_event_key(event: &ScheduledKind) -> (u64, u64, u64) {
    match event {
        ScheduledKind::LoopBoundary => (0, 0, 0),
        ScheduledKind::NoteOff { clip, note, .. }
        | ScheduledKind::NoteOn { clip, note, .. }
        | ScheduledKind::NoteExpression { clip, note, .. } => (clip.get(), note.get(), 0),
        ScheduledKind::Trigger {
            clip,
            lane,
            ratchet,
            ..
        } => (clip.get(), lane.get(), *ratchet as u64),
    }
}

fn performance_identity(seed: u64, clip: u64, event: u64, cycle: i64, sub: u8) -> u64 {
    hash64(
        seed ^ clip.rotate_left(11)
            ^ event.rotate_left(29)
            ^ (cycle as u64).rotate_left(43)
            ^ sub as u64,
    )
}

fn passes_probability(probability: f32, identity: u64) -> bool {
    probability >= 1.0 || (probability > 0.0 && unit_from_hash(identity) < probability as f64)
}

fn hash64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn unit_from_hash(value: u64) -> f64 {
    (hash64(value) >> 11) as f64 * (1.0 / ((1_u64 << 53) as f64))
}

fn signed_unit(value: u64) -> f64 {
    unit_from_hash(value) * 2.0 - 1.0
}

fn unit(value: f32) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn bipolar(value: f32) -> bool {
    value.is_finite() && (-1.0..=1.0).contains(&value)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SequencerError {
    InvalidSampleRate,
    InvalidTempo,
    InvalidTimeSignature,
    MapPointBeforeZero,
    MissingMapOrigin,
    MeterChangeNotAtBar,
    InvalidMusicalPosition,
    InvalidRange,
    InvalidPattern(PatternId),
    MissingPattern(PatternId),
    InvalidClip(PatternClipId),
    InvalidNote(NoteId),
    InvalidTransform,
    InvalidAllocatorState,
    EmptyCommand,
    StaleCommand,
}

impl fmt::Display for SequencerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSampleRate => write!(formatter, "sample rate must be non-zero"),
            Self::InvalidTempo => write!(formatter, "tempo must be finite and positive"),
            Self::InvalidTimeSignature => write!(formatter, "invalid time signature"),
            Self::MapPointBeforeZero => {
                write!(formatter, "map points before tick zero are unsupported")
            }
            Self::MissingMapOrigin => write!(formatter, "map requires an origin point"),
            Self::MeterChangeNotAtBar => {
                write!(formatter, "meter change must be on a bar boundary")
            }
            Self::InvalidMusicalPosition => {
                write!(formatter, "beat or tick exceeds the active meter")
            }
            Self::InvalidRange => write!(formatter, "frame range must be non-empty and ordered"),
            Self::InvalidPattern(id) => write!(formatter, "pattern {id:?} is invalid"),
            Self::MissingPattern(id) => write!(formatter, "pattern {id:?} does not exist"),
            Self::InvalidClip(id) => write!(formatter, "pattern clip {id:?} is invalid"),
            Self::InvalidNote(id) => write!(formatter, "note {id:?} is invalid"),
            Self::InvalidTransform => {
                write!(formatter, "sequencer transform parameters are invalid")
            }
            Self::InvalidAllocatorState => {
                write!(
                    formatter,
                    "sequencer allocator state is below an identity high-water mark"
                )
            }
            Self::EmptyCommand => write!(formatter, "command does not identify an entity"),
            Self::StaleCommand => {
                write!(formatter, "command before-state does not match the project")
            }
        }
    }
}

impl Error for SequencerError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> TempoMap {
        TempoMap::common_time(48_000, 120.0).unwrap()
    }

    fn note(id: u64, start: i64, duration: u64) -> NoteEvent {
        NoteEvent {
            id: NoteId::from_raw(id),
            start: BeatTime(start),
            duration: BeatDuration(duration),
            pitch: NotePitch {
                midi_key: 60,
                cents: 0.0,
            },
            velocity: 0.8,
            release_velocity: 0.4,
            pan: 0.0,
            probability: 1.0,
            micro_offset: 0,
            channel: 0,
            instrument: Some(1),
            articulation: Articulation::Normal,
            expression: PerNoteExpression::default(),
        }
    }

    fn note_sequencer(note_start: i64) -> Sequencer {
        let mut sequencer = Sequencer::new(map());
        let pattern_id = sequencer.allocate_pattern_id();
        let clip_id = sequencer.allocate_clip_id();
        let mut notes = NotePattern::default();
        notes
            .notes
            .insert(NoteId::from_raw(1), note(1, note_start, 480));
        let pattern = PatternDefinition {
            id: pattern_id,
            name: "notes".into(),
            length: BeatDuration(4 * PPQ as u64),
            content: PatternContent::Notes(notes),
            origin: PatternOrigin::Authored,
            revision: 0,
        };
        let clip = PatternClip {
            id: clip_id,
            pattern: pattern_id,
            start: BeatTime::ZERO,
            length: BeatDuration(8 * PPQ as u64),
            pattern_offset: BeatTime::ZERO,
            looped: true,
            transpose_semitones: 0.0,
            gain: 1.0,
            muted: false,
        };
        sequencer
            .execute(
                "seed",
                vec![
                    SequencerCommand::PutPattern {
                        before: None,
                        after: Some(pattern),
                    },
                    SequencerCommand::PutClip {
                        before: None,
                        after: Some(clip),
                    },
                ],
            )
            .unwrap();
        sequencer
    }

    #[test]
    fn constant_tempo_converts_exact_quarters() {
        let map = map();
        assert_eq!(map.beat_to_frame(BeatTime(PPQ)), ProjectFrame(24_000));
        assert_eq!(map.beat_to_frame(BeatTime(-PPQ)), ProjectFrame(-24_000));
        assert_eq!(map.frame_to_beat_floor(ProjectFrame(24_000)), BeatTime(PPQ));
    }

    #[test]
    fn tempo_change_integrates_without_resetting_time() {
        let mut map = map();
        map.set_tempo(BeatTime(4 * PPQ), Tempo::from_bpm(60.0).unwrap())
            .unwrap();
        assert_eq!(map.beat_to_frame(BeatTime(4 * PPQ)), ProjectFrame(96_000));
        assert_eq!(map.beat_to_frame(BeatTime(5 * PPQ)), ProjectFrame(144_000));
        assert_eq!(
            map.frame_to_beat_floor(ProjectFrame(143_999)),
            BeatTime(5 * PPQ - 1)
        );
    }

    #[test]
    fn many_tempo_segments_have_no_segment_rounding_drift() {
        let mut map = TempoMap::common_time(44_100, 123.0).unwrap();
        for quarter in 1..100 {
            map.set_tempo(
                BeatTime(quarter * PPQ),
                Tempo::from_micros_per_quarter(400_001 + quarter as u32),
            )
            .unwrap();
        }
        let exact_numerator: i128 = (0..100)
            .map(|quarter| {
                PPQ as i128
                    * (if quarter == 0 {
                        487_805
                    } else {
                        400_001 + quarter
                    } as i128)
                    * 44_100
            })
            .sum();
        let expected = exact_numerator.div_euclid(PPQ as i128 * 1_000_000);
        assert_eq!(map.beat_to_frame(BeatTime(100 * PPQ)).0 as i128, expected);
    }

    #[test]
    fn inverse_is_floor_at_sub_tick_frames() {
        let map = TempoMap::common_time(1_000, 240.0).unwrap();
        for frame in -2_000..2_000 {
            let beat = map.frame_to_beat_floor(ProjectFrame(frame));
            assert!(map.beat_to_frame(beat).0 <= frame);
            if beat.0 < i64::MAX {
                assert!(map.beat_to_frame(BeatTime(beat.0 + 1)).0 > frame);
            }
        }
    }

    #[test]
    fn meter_change_round_trips_bar_positions() {
        let mut map = map();
        map.set_meter(BeatTime(8 * PPQ), TimeSignature::new(3, 4).unwrap())
            .unwrap();
        let at_change = map.musical_position(BeatTime(8 * PPQ));
        assert_eq!(
            at_change,
            MusicalPosition {
                bar: 2,
                beat: 0,
                tick: 0
            }
        );
        let position = MusicalPosition {
            bar: 4,
            beat: 2,
            tick: 17,
        };
        let tick = map.beat_at_position(position).unwrap();
        assert_eq!(map.musical_position(tick), position);
    }

    #[test]
    fn meter_change_must_be_on_old_bar_boundary() {
        let mut map = map();
        assert_eq!(
            map.set_meter(BeatTime(PPQ), TimeSignature::new(7, 8).unwrap()),
            Err(SequencerError::MeterChangeNotAtBar)
        );
    }

    #[test]
    fn negative_preroll_has_euclidean_bar_coordinates() {
        let map = map();
        assert_eq!(
            map.musical_position(BeatTime(-1)),
            MusicalPosition {
                bar: -1,
                beat: 3,
                tick: 959
            }
        );
        assert_eq!(
            map.beat_at_position(MusicalPosition {
                bar: -1,
                beat: 3,
                tick: 959
            })
            .unwrap(),
            BeatTime(-1)
        );
    }

    #[test]
    fn quantize_strength_and_negative_ties_are_deterministic() {
        let mut pattern = NotePattern::default();
        pattern.notes.insert(NoteId(1), note(1, 350, 100));
        let quantized = quantize_notes(
            &pattern,
            QuantizeSpec {
                grid: BeatDuration(240),
                strength: 0.5,
            },
        )
        .unwrap();
        assert_eq!(quantized.notes[&NoteId(1)].start, BeatTime(295));
    }

    #[test]
    fn humanize_is_seeded_and_identity_stable() {
        let mut pattern = NotePattern::default();
        pattern.notes.insert(NoteId(1), note(1, 0, 100));
        pattern.notes.insert(NoteId(2), note(2, 100, 100));
        let spec = HumanizeSpec {
            maximum_ticks: 12,
            maximum_velocity_delta: 0.1,
            seed: 42,
        };
        let first = humanize_notes(&pattern, spec).unwrap();
        let second = humanize_notes(&pattern, spec).unwrap();
        assert_eq!(first, second);
        assert_ne!(
            first.notes[&NoteId(1)].micro_offset,
            first.notes[&NoteId(2)].micro_offset
        );
    }

    #[test]
    fn swing_moves_only_odd_note_divisions() {
        let mut pattern = NotePattern::default();
        pattern.notes.insert(NoteId(1), note(1, 0, 100));
        pattern.notes.insert(NoteId(2), note(2, 240, 100));
        pattern.notes.insert(NoteId(3), note(3, 480, 100));
        let swung = swing_notes(
            &pattern,
            SwingSpec {
                grid: BeatDuration(240),
                amount: 1.0,
            },
        )
        .unwrap();
        assert_eq!(swung.notes[&NoteId(1)].micro_offset, 0);
        assert_eq!(swung.notes[&NoteId(2)].micro_offset, 120);
        assert_eq!(swung.notes[&NoteId(3)].micro_offset, 0);
    }

    #[test]
    fn per_note_expression_compiles_between_on_and_off() {
        let mut sequencer = note_sequencer(0);
        let before = sequencer.patterns.get(PatternId(1)).unwrap().clone();
        let mut after = before.clone();
        if let PatternContent::Notes(notes) = &mut after.content {
            notes.notes.get_mut(&NoteId(1)).unwrap().expression.pressure = vec![
                ExpressionPoint {
                    position: 0.0,
                    value: 0.2,
                },
                ExpressionPoint {
                    position: 0.5,
                    value: 0.9,
                },
            ];
        }
        sequencer
            .execute(
                "expression",
                vec![SequencerCommand::PutPattern {
                    before: Some(before),
                    after: Some(after),
                }],
            )
            .unwrap();
        let events = sequencer.schedule_project_window(
            FrameRange::new(ProjectFrame(0), ProjectFrame(24_000)).unwrap(),
            0,
        );
        assert!(matches!(events[0].kind, ScheduledKind::NoteOn { .. }));
        assert!(matches!(
            events[1].kind,
            ScheduledKind::NoteExpression {
                dimension: ExpressionDimension::Pressure,
                value: 0.2,
                ..
            }
        ));
        assert_eq!(events[2].project_frame, ProjectFrame(6_000));
    }

    #[test]
    fn project_window_is_half_open() {
        let sequencer = note_sequencer(PPQ);
        let at = sequencer.tempo_map.beat_to_frame(BeatTime(PPQ));
        assert!(sequencer
            .schedule_project_window(FrameRange::new(ProjectFrame(0), at).unwrap(), 1)
            .is_empty());
        let events = sequencer
            .schedule_project_window(FrameRange::new(at, ProjectFrame(at.0 + 1)).unwrap(), 1);
        assert!(matches!(events[0].kind, ScheduledKind::NoteOn { .. }));
    }

    #[test]
    fn scheduling_across_tempo_change_uses_exact_event_frames() {
        let mut sequencer = note_sequencer(PPQ);
        let before_clip = sequencer.clip(PatternClipId(1)).unwrap().clone();
        let mut moved_clip = before_clip.clone();
        moved_clip.start = BeatTime(4 * PPQ);
        sequencer
            .execute(
                "move clip",
                vec![SequencerCommand::PutClip {
                    before: Some(before_clip),
                    after: Some(moved_clip),
                }],
            )
            .unwrap();
        let mut changed = sequencer.tempo_map.clone();
        changed
            .set_tempo(BeatTime(4 * PPQ), Tempo::from_bpm(60.0).unwrap())
            .unwrap();
        sequencer
            .execute(
                "tempo",
                vec![SequencerCommand::SetTempoMap {
                    before: sequencer.tempo_map.clone(),
                    after: changed,
                }],
            )
            .unwrap();
        let expected = ProjectFrame(144_000);
        let events = sequencer.schedule_project_window(
            FrameRange::new(ProjectFrame(143_999), ProjectFrame(144_001)).unwrap(),
            0,
        );
        assert_eq!(events[0].project_frame, expected);
    }

    #[test]
    fn loop_wrap_excludes_end_replays_start_and_orders_boundary_first() {
        let sequencer = note_sequencer(0);
        let loop_range = FrameRange::new(ProjectFrame(0), ProjectFrame(96_000)).unwrap();
        let events = sequencer
            .schedule_transport_window(ProjectFrame(95_999), 3, Some(loop_range), 0)
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].block_offset, 1);
        assert!(matches!(events[0].kind, ScheduledKind::LoopBoundary));
        assert_eq!(events[1].block_offset, 1);
        assert!(matches!(events[1].kind, ScheduledKind::NoteOn { .. }));
    }

    #[test]
    fn callback_starting_at_loop_end_emits_boundary_before_loop_start() {
        let sequencer = note_sequencer(0);
        let loop_range = FrameRange::new(ProjectFrame(0), ProjectFrame(96_000)).unwrap();
        let events = sequencer
            .schedule_transport_window(ProjectFrame(96_000), 1, Some(loop_range), 0)
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].block_offset, 0);
        assert!(matches!(events[0].kind, ScheduledKind::LoopBoundary));
        assert!(matches!(events[1].kind, ScheduledKind::NoteOn { .. }));
    }

    #[test]
    fn repeated_loop_iterations_retain_distinct_callback_offsets() {
        let sequencer = note_sequencer(0);
        let loop_range = FrameRange::new(ProjectFrame(0), ProjectFrame(24_000)).unwrap();
        let events = sequencer
            .schedule_transport_window(ProjectFrame(0), 48_001, Some(loop_range), 0)
            .unwrap();
        let ons: Vec<_> = events
            .iter()
            .filter(|event| matches!(event.kind, ScheduledKind::NoteOn { .. }))
            .map(|event| event.block_offset)
            .collect();
        assert_eq!(ons, vec![0, 24_000, 48_000]);
    }

    #[test]
    fn note_off_at_same_frame_precedes_retrigger() {
        let mut sequencer = note_sequencer(0);
        let id = PatternId(1);
        let before = sequencer.patterns.get(id).unwrap().clone();
        let mut after = before.clone();
        if let PatternContent::Notes(notes) = &mut after.content {
            notes.notes.get_mut(&NoteId(1)).unwrap().duration = BeatDuration(4 * PPQ as u64);
        }
        sequencer
            .execute(
                "legato boundary",
                vec![SequencerCommand::PutPattern {
                    before: Some(before),
                    after: Some(after),
                }],
            )
            .unwrap();
        let at = ProjectFrame(96_000);
        let events = sequencer
            .schedule_project_window(FrameRange::new(at, ProjectFrame(at.0 + 1)).unwrap(), 0);
        assert!(matches!(events[0].kind, ScheduledKind::NoteOff { .. }));
        assert!(matches!(events[1].kind, ScheduledKind::NoteOn { .. }));
    }

    #[test]
    fn step_swing_ratchets_and_sample_target_compile() {
        let mut sequencer = Sequencer::new(map());
        let pattern_id = sequencer.allocate_pattern_id();
        let clip_id = sequencer.allocate_clip_id();
        let lane_id = sequencer.allocate_step_lane_id();
        let mut steps = BTreeMap::new();
        steps.insert(
            1,
            StepEvent {
                velocity: 1.0,
                probability: 1.0,
                micro_offset: 0,
                gate: BeatDuration(240),
                ratchets: 2,
                pitch_semitones: -1.0,
                pan: 0.25,
            },
        );
        let lane = StepLane {
            id: lane_id,
            name: "kick".into(),
            target: TriggerTarget::Sample(SampleAssetId(9)),
            choke_group: Some(2),
            steps,
        };
        let pattern = PatternDefinition {
            id: pattern_id,
            name: "drums".into(),
            length: BeatDuration(PPQ as u64),
            content: PatternContent::Steps(StepPattern {
                resolution: BeatDuration(240),
                swing: 1.0,
                lanes: BTreeMap::from([(lane_id, lane)]),
            }),
            origin: PatternOrigin::Authored,
            revision: 0,
        };
        let clip = PatternClip {
            id: clip_id,
            pattern: pattern_id,
            start: BeatTime::ZERO,
            length: BeatDuration(PPQ as u64),
            pattern_offset: BeatTime::ZERO,
            looped: false,
            transpose_semitones: 2.0,
            gain: 0.5,
            muted: false,
        };
        sequencer
            .execute(
                "drums",
                vec![
                    SequencerCommand::PutPattern {
                        before: None,
                        after: Some(pattern),
                    },
                    SequencerCommand::PutClip {
                        before: None,
                        after: Some(clip),
                    },
                ],
            )
            .unwrap();
        let events = sequencer.schedule_project_window(
            FrameRange::new(ProjectFrame(0), ProjectFrame(24_000)).unwrap(),
            0,
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].project_frame, map().beat_to_frame(BeatTime(360)));
        match &events[0].kind {
            ScheduledKind::Trigger {
                target,
                velocity,
                pitch_semitones,
                gate_frames,
                ..
            } => {
                assert_eq!(*target, TriggerTarget::Sample(SampleAssetId(9)));
                assert_eq!(*velocity, 0.5);
                assert_eq!(*pitch_semitones, 1.0);
                assert_eq!(*gate_frames, 3_000);
            }
            _ => panic!("expected trigger"),
        }
    }

    #[test]
    fn expression_pattern_uses_the_real_placement_cycle() {
        let mut sequencer = Sequencer::new(map());
        let pattern_id = sequencer.allocate_pattern_id();
        let clip_id = sequencer.allocate_clip_id();
        let bindings = BTreeMap::from([
            ("a".to_owned(), TriggerTarget::AnalysisTemplate(10)),
            ("b".to_owned(), TriggerTarget::AnalysisTemplate(20)),
        ]);
        let source = "<a b>";
        let term = crate::pattern_lang::parse(source).unwrap();
        let preview = crate::pattern_lang::eval_steps(
            &term,
            &crate::pattern_lang::EvalContext {
                bindings: &bindings,
                cycle: BeatDuration(PPQ as u64),
                seed: 0,
                cycle_index: 0,
            },
        )
        .unwrap()
        .pattern;
        let pattern = PatternDefinition {
            id: pattern_id,
            name: "alternating".into(),
            length: BeatDuration(PPQ as u64),
            content: PatternContent::Steps(preview),
            origin: PatternOrigin::Expression {
                source: source.into(),
                term_hash: crate::pattern_lang::term_hash(&term),
                bindings_hash: crate::pattern_lang::bindings_hash(&bindings),
                bindings,
                diverged: false,
            },
            revision: 0,
        };
        let clip = PatternClip {
            id: clip_id,
            pattern: pattern_id,
            start: BeatTime(0),
            length: BeatDuration((2 * PPQ) as u64),
            pattern_offset: BeatTime(0),
            looped: true,
            transpose_semitones: 0.0,
            gain: 1.0,
            muted: false,
        };
        sequencer
            .execute(
                "place expression",
                vec![
                    SequencerCommand::PutPattern {
                        before: None,
                        after: Some(pattern),
                    },
                    SequencerCommand::PutClip {
                        before: None,
                        after: Some(clip),
                    },
                ],
            )
            .unwrap();

        let end = sequencer.tempo_map.beat_to_frame(BeatTime(2 * PPQ));
        let targets = sequencer
            .schedule_project_window(FrameRange::new(ProjectFrame(0), end).unwrap(), 0)
            .into_iter()
            .filter_map(|event| match event.kind {
                ScheduledKind::Trigger { target, .. } => Some(target),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            targets,
            vec![
                TriggerTarget::AnalysisTemplate(10),
                TriggerTarget::AnalysisTemplate(20)
            ]
        );
    }

    #[test]
    fn content_edit_marks_expression_origin_diverged() {
        let mut sequencer = Sequencer::new(map());
        let id = sequencer.allocate_pattern_id();
        let bindings = BTreeMap::from([("a".to_owned(), TriggerTarget::AnalysisTemplate(1))]);
        let term = crate::pattern_lang::parse("a").unwrap();
        let content = crate::pattern_lang::eval_steps(
            &term,
            &crate::pattern_lang::EvalContext {
                bindings: &bindings,
                cycle: BeatDuration(PPQ as u64),
                seed: 0,
                cycle_index: 0,
            },
        )
        .unwrap()
        .pattern;
        let pattern = PatternDefinition {
            id,
            name: "generated".into(),
            length: BeatDuration(PPQ as u64),
            content: PatternContent::Steps(content),
            origin: PatternOrigin::Expression {
                source: "a".into(),
                term_hash: crate::pattern_lang::term_hash(&term),
                bindings_hash: crate::pattern_lang::bindings_hash(&bindings),
                bindings,
                diverged: false,
            },
            revision: 0,
        };
        sequencer
            .execute(
                "create",
                vec![SequencerCommand::PutPattern {
                    before: None,
                    after: Some(pattern),
                }],
            )
            .unwrap();
        let before = sequencer.patterns().get(id).unwrap().clone();
        let mut after = before.clone();
        let PatternContent::Steps(steps) = &mut after.content else {
            unreachable!()
        };
        steps
            .lanes
            .values_mut()
            .next()
            .unwrap()
            .steps
            .get_mut(&0)
            .unwrap()
            .velocity = 0.5;
        sequencer
            .execute(
                "manual velocity",
                vec![SequencerCommand::PutPattern {
                    before: Some(before),
                    after: Some(after),
                }],
            )
            .unwrap();
        assert!(sequencer.patterns().get(id).unwrap().origin.diverged());
        sequencer.undo().unwrap();
        assert!(!sequencer.patterns().get(id).unwrap().origin.diverged());
    }

    #[test]
    fn scheduling_a_tiny_window_in_a_huge_loop_is_bounded() {
        let mut sequencer = note_sequencer(0);
        let before = sequencer.clip(PatternClipId(1)).unwrap().clone();
        let mut after = before.clone();
        after.length = BeatDuration(i64::MAX as u64);
        sequencer
            .execute(
                "extend",
                vec![SequencerCommand::PutClip {
                    before: Some(before),
                    after: Some(after),
                }],
            )
            .unwrap();
        let target_tick = 1_000_000_000_i64 / (4 * PPQ) * (4 * PPQ);
        let frame = sequencer.tempo_map.beat_to_frame(BeatTime(target_tick));
        let events = sequencer.schedule_project_window(
            FrameRange::new(frame, ProjectFrame(frame.0 + 1)).unwrap(),
            0,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].kind, ScheduledKind::NoteOn { .. }));
    }

    #[test]
    fn probability_is_block_partition_independent() {
        let mut sequencer = note_sequencer(0);
        let before = sequencer.patterns.get(PatternId(1)).unwrap().clone();
        let mut after = before.clone();
        if let PatternContent::Notes(notes) = &mut after.content {
            notes.notes.get_mut(&NoteId(1)).unwrap().probability = 0.5;
        }
        sequencer
            .execute(
                "probability",
                vec![SequencerCommand::PutPattern {
                    before: Some(before),
                    after: Some(after),
                }],
            )
            .unwrap();
        let whole = sequencer.schedule_project_window(
            FrameRange::new(ProjectFrame(0), ProjectFrame(192_000)).unwrap(),
            991,
        );
        let mut split = Vec::new();
        for index in 0..4 {
            split.extend(
                sequencer.schedule_project_window(
                    FrameRange::new(
                        ProjectFrame(index * 48_000),
                        ProjectFrame((index + 1) * 48_000),
                    )
                    .unwrap(),
                    991,
                ),
            );
        }
        let whole_frames: Vec<_> = whole
            .iter()
            .filter(|event| matches!(event.kind, ScheduledKind::NoteOn { .. }))
            .map(|event| event.project_frame)
            .collect();
        let split_frames: Vec<_> = split
            .iter()
            .filter(|event| matches!(event.kind, ScheduledKind::NoteOn { .. }))
            .map(|event| event.project_frame)
            .collect();
        assert_eq!(whole_frames, split_frames);
    }

    #[test]
    fn transaction_is_atomic_and_undoable() {
        let mut sequencer = note_sequencer(0);
        let before = sequencer.clip(PatternClipId(1)).unwrap().clone();
        let mut after = before.clone();
        after.transpose_semitones = 7.0;
        sequencer
            .execute(
                "transpose",
                vec![SequencerCommand::PutClip {
                    before: Some(before.clone()),
                    after: Some(after),
                }],
            )
            .unwrap();
        assert_eq!(sequencer.clip(before.id).unwrap().transpose_semitones, 7.0);
        assert_eq!(sequencer.undo_label(), Some("transpose"));
        sequencer.undo().unwrap();
        assert_eq!(sequencer.clip(before.id).unwrap().transpose_semitones, 0.0);
        sequencer.redo().unwrap();
        assert_eq!(sequencer.clip(before.id).unwrap().transpose_semitones, 7.0);
    }

    #[test]
    fn invalid_transaction_does_not_partially_apply() {
        let mut sequencer = note_sequencer(0);
        let before = sequencer.clip(PatternClipId(1)).unwrap().clone();
        let mut invalid = before.clone();
        invalid.pattern = PatternId(999);
        let revision = sequencer.revision();
        assert!(sequencer
            .execute(
                "invalid",
                vec![SequencerCommand::PutClip {
                    before: Some(before.clone()),
                    after: Some(invalid)
                }]
            )
            .is_err());
        assert_eq!(sequencer.revision(), revision);
        assert_eq!(sequencer.clip(before.id), Some(&before));
    }

    #[test]
    fn stale_commands_are_rejected() {
        let mut sequencer = note_sequencer(0);
        let mut fictional = sequencer.clip(PatternClipId(1)).unwrap().clone();
        fictional.gain = 0.5;
        assert_eq!(
            sequencer.execute(
                "stale",
                vec![SequencerCommand::PutClip {
                    before: Some(fictional.clone()),
                    after: Some(fictional)
                }]
            ),
            Err(SequencerError::StaleCommand)
        );
    }

    #[test]
    fn ids_are_not_reused_after_undo() {
        let mut sequencer = Sequencer::new(map());
        let first = sequencer.allocate_pattern_id();
        let pattern = PatternDefinition {
            id: first,
            name: "one".into(),
            length: BeatDuration(PPQ as u64),
            content: PatternContent::Notes(NotePattern::default()),
            origin: PatternOrigin::Authored,
            revision: 0,
        };
        sequencer
            .execute(
                "insert",
                vec![SequencerCommand::PutPattern {
                    before: None,
                    after: Some(pattern),
                }],
            )
            .unwrap();
        sequencer.undo().unwrap();
        assert!(sequencer.allocate_pattern_id().get() > first.get());
    }

    #[test]
    fn imported_ids_advance_allocation_high_water_mark() {
        let mut sequencer = Sequencer::new(map());
        let pattern = PatternDefinition {
            id: PatternId(99),
            name: "imported".into(),
            length: BeatDuration(PPQ as u64),
            content: PatternContent::Notes(NotePattern {
                notes: BTreeMap::from([(NoteId(77), note(77, 0, 1))]),
            }),
            origin: PatternOrigin::Authored,
            revision: 0,
        };
        sequencer
            .execute(
                "import",
                vec![SequencerCommand::PutPattern {
                    before: None,
                    after: Some(pattern),
                }],
            )
            .unwrap();
        assert_eq!(sequencer.allocate_pattern_id(), PatternId(100));
        assert_eq!(sequencer.allocate_note_id(), NoteId(78));
    }
}
