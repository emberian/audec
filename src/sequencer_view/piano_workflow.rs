//! Pure piano-roll editing policy.
//!
//! GPUI owns pointer capture and painting; this module owns deterministic
//! musical edits so mouse, keyboard, and future MIDI/controller surfaces all
//! produce the same pattern replacement through the authoritative pattern
//! workflow.

use std::collections::{BTreeMap, BTreeSet};

use crate::sequencer::{
    BeatDuration, BeatTime, MusicalPosition, NoteEvent, NoteId, NotePattern, TempoMap,
    TimeSignature,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarMarker {
    pub tick: i64,
    pub bar: i64,
    pub signature: TimeSignature,
}

/// Meter-map aware visible bar boundaries. Unlike a fixed modulo grid this
/// remains correct after any valid time-signature change.
pub fn visible_bar_markers(
    map: &TempoMap,
    start_tick: i64,
    end_tick: i64,
    maximum: usize,
) -> Vec<BarMarker> {
    if end_tick <= start_tick || maximum == 0 {
        return Vec::new();
    }
    let position = map.musical_position(BeatTime(start_tick));
    let mut bar = position.bar;
    let current_start = map
        .beat_at_position(MusicalPosition {
            bar,
            beat: 0,
            tick: 0,
        })
        .unwrap_or(BeatTime(start_tick));
    if current_start.0 < start_tick {
        bar = bar.saturating_add(1);
    }
    let mut result = Vec::new();
    while result.len() < maximum {
        let Ok(at) = map.beat_at_position(MusicalPosition {
            bar,
            beat: 0,
            tick: 0,
        }) else {
            break;
        };
        if at.0 >= end_tick {
            break;
        }
        result.push(BarMarker {
            tick: at.0,
            bar,
            signature: map.meter_at(at),
        });
        bar = bar.saturating_add(1);
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScaleKind {
    Chromatic,
    Major,
    Minor,
    MajorPentatonic,
}

impl ScaleKind {
    pub const fn next(self) -> Self {
        match self {
            Self::Chromatic => Self::Major,
            Self::Major => Self::Minor,
            Self::Minor => Self::MajorPentatonic,
            Self::MajorPentatonic => Self::Chromatic,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Chromatic => "CHROMATIC",
            Self::Major => "MAJOR",
            Self::Minor => "MINOR",
            Self::MajorPentatonic => "PENTA",
        }
    }

    const fn intervals(self) -> &'static [u8] {
        match self {
            Self::Chromatic => &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
            Self::Major => &[0, 2, 4, 5, 7, 9, 11],
            Self::Minor => &[0, 2, 3, 5, 7, 8, 10],
            Self::MajorPentatonic => &[0, 2, 4, 7, 9],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PitchScale {
    pub root: u8,
    pub kind: ScaleKind,
}

impl Default for PitchScale {
    fn default() -> Self {
        Self {
            root: 0,
            kind: ScaleKind::Chromatic,
        }
    }
}

impl PitchScale {
    pub fn contains(self, key: u8) -> bool {
        let degree = (key + 12 - self.root % 12) % 12;
        self.kind.intervals().contains(&degree)
    }

    /// Nearest scale pitch, resolving an exact tie upward.
    pub fn constrain(self, key: i16) -> u8 {
        let key = key.clamp(0, 127);
        if self.contains(key as u8) {
            return key as u8;
        }
        for distance in 1..=12_i16 {
            let up = key + distance;
            if up <= 127 && self.contains(up as u8) {
                return up as u8;
            }
            let down = key - distance;
            if down >= 0 && self.contains(down as u8) {
                return down as u8;
            }
        }
        key as u8
    }

    pub fn step(self, key: u8, steps: i32) -> u8 {
        if steps == 0 {
            return self.constrain(i16::from(key));
        }
        let direction = steps.signum() as i16;
        let mut remaining = steps.unsigned_abs();
        let mut candidate = i16::from(key);
        while remaining > 0 {
            let next = candidate.saturating_add(direction).clamp(0, 127);
            if next == candidate {
                break;
            }
            candidate = next;
            if self.contains(candidate as u8) {
                remaining -= 1;
            }
        }
        self.constrain(candidate)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoteMarquee {
    pub start_tick: i64,
    pub end_tick: i64,
    pub low_key: u8,
    pub high_key: u8,
}

impl NoteMarquee {
    pub fn normalized(self) -> Self {
        Self {
            start_tick: self.start_tick.min(self.end_tick),
            end_tick: self.start_tick.max(self.end_tick),
            low_key: self.low_key.min(self.high_key),
            high_key: self.low_key.max(self.high_key),
        }
    }

    pub fn select(self, notes: &NotePattern) -> BTreeSet<NoteId> {
        let range = self.normalized();
        notes
            .notes
            .values()
            .filter(|note| {
                let end = note
                    .start
                    .0
                    .saturating_add(note.duration.0.min(i64::MAX as u64) as i64);
                note.start.0 < range.end_tick
                    && end > range.start_tick
                    && (range.low_key..=range.high_key).contains(&note.pitch.midi_key)
            })
            .map(|note| note.id)
            .collect()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NoteBatch {
    pub notes: BTreeMap<NoteId, NoteEvent>,
    pub selected: BTreeSet<NoteId>,
}

impl NoteBatch {
    pub fn capture(pattern: &NotePattern, selected: &BTreeSet<NoteId>) -> Self {
        Self {
            notes: selected
                .iter()
                .filter_map(|id| pattern.notes.get(id).cloned().map(|note| (*id, note)))
                .collect(),
            selected: selected.clone(),
        }
    }

    pub fn moved(
        &self,
        pattern_length: BeatDuration,
        tick_delta: i64,
        scale_steps: i32,
        scale: PitchScale,
    ) -> BTreeMap<NoteId, NoteEvent> {
        let earliest = self
            .notes
            .values()
            .map(|note| note.start.0)
            .min()
            .unwrap_or(0);
        let latest_end = self
            .notes
            .values()
            .map(|note| note.start.0.saturating_add(note.duration.0 as i64))
            .max()
            .unwrap_or(0);
        let clamped_delta = tick_delta.clamp(
            earliest.saturating_neg(),
            (pattern_length.0.min(i64::MAX as u64) as i64).saturating_sub(latest_end),
        );
        self.notes
            .iter()
            .map(|(id, note)| {
                let mut note = note.clone();
                note.start = BeatTime(note.start.0.saturating_add(clamped_delta));
                note.pitch.midi_key = scale.step(note.pitch.midi_key, scale_steps);
                (*id, note)
            })
            .collect()
    }

    pub fn resized(
        &self,
        pattern_length: BeatDuration,
        duration_delta: i64,
        minimum: BeatDuration,
    ) -> BTreeMap<NoteId, NoteEvent> {
        self.notes
            .iter()
            .map(|(id, note)| {
                let maximum = pattern_length.0.saturating_sub(note.start.0 as u64).max(1);
                let mut note = note.clone();
                note.duration.0 = (i128::from(note.duration.0) + i128::from(duration_delta))
                    .clamp(i128::from(minimum.0.max(1)), i128::from(maximum))
                    as u64;
                (*id, note)
            })
            .collect()
    }

    pub fn velocity_scaled(&self, delta: f32) -> BTreeMap<NoteId, NoteEvent> {
        self.notes
            .iter()
            .map(|(id, note)| {
                let mut note = note.clone();
                note.velocity = (note.velocity + delta).clamp(0.0, 1.0);
                (*id, note)
            })
            .collect()
    }

    pub fn probability_scaled(&self, delta: f32) -> BTreeMap<NoteId, NoteEvent> {
        self.notes
            .iter()
            .map(|(id, note)| {
                let mut note = note.clone();
                note.probability = (note.probability + delta).clamp(0.0, 1.0);
                (*id, note)
            })
            .collect()
    }

    pub fn microtiming_shifted(
        &self,
        delta: i32,
        maximum_offset: i32,
    ) -> BTreeMap<NoteId, NoteEvent> {
        let maximum_offset = maximum_offset.max(0);
        self.notes
            .iter()
            .map(|(id, note)| {
                let mut note = note.clone();
                note.micro_offset = note
                    .micro_offset
                    .saturating_add(delta)
                    .clamp(-maximum_offset, maximum_offset);
                (*id, note)
            })
            .collect()
    }
}

pub fn replace_notes(
    pattern: &NotePattern,
    replacement: BTreeMap<NoteId, NoteEvent>,
) -> NotePattern {
    let mut result = pattern.clone();
    result.notes.extend(replacement);
    result
}

pub fn gesture_tick_delta(raw_delta: i64, grid: u64, bypass_snap: bool) -> i64 {
    if bypass_snap || grid == 0 {
        return raw_delta;
    }
    let grid = grid.min(i64::MAX as u64) as i64;
    let lower = raw_delta.div_euclid(grid).saturating_mul(grid);
    let upper = lower.saturating_add(grid);
    if raw_delta.saturating_sub(lower) < upper.saturating_sub(raw_delta) {
        lower
    } else {
        upper
    }
}

/// One pointer gesture's immutable baseline and replaceable visual preview.
/// Motion may update `preview` any number of times; only [`finish`](Self::finish)
/// can yield a durable pattern, so hosts receive at most one command.
#[derive(Clone, Debug, PartialEq)]
pub struct PianoGestureTransaction {
    before: NotePattern,
    preview: NotePattern,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PianoGestureResolution {
    NoChange,
    Commit(NotePattern),
    Rollback(NotePattern),
}

impl PianoGestureTransaction {
    pub fn begin(before: &NotePattern) -> Self {
        Self {
            before: before.clone(),
            preview: before.clone(),
        }
    }

    pub fn before(&self) -> &NotePattern {
        &self.before
    }

    pub fn preview(&self) -> &NotePattern {
        &self.preview
    }

    /// Replace a preview from the immutable baseline, never from the prior
    /// pointer sample. This prevents accumulated rounding and snap drift.
    pub fn preview_replacement(&mut self, replacement: BTreeMap<NoteId, NoteEvent>) {
        self.preview = replace_notes(&self.before, replacement);
    }

    pub fn finish(self) -> PianoGestureResolution {
        if self.preview == self.before {
            PianoGestureResolution::NoChange
        } else {
            PianoGestureResolution::Commit(self.preview)
        }
    }

    pub fn rollback(self) -> PianoGestureResolution {
        PianoGestureResolution::Rollback(self.before)
    }
}

pub fn remove_notes(pattern: &NotePattern, selected: &BTreeSet<NoteId>) -> NotePattern {
    let mut result = pattern.clone();
    result.notes.retain(|id, _| !selected.contains(id));
    result
}

pub fn duplicate_notes(
    pattern: &NotePattern,
    selected: &BTreeSet<NoteId>,
    first_id: u64,
    tick_offset: i64,
    pattern_length: BeatDuration,
) -> (NotePattern, BTreeSet<NoteId>) {
    let mut result = pattern.clone();
    let mut next_id = first_id;
    let mut duplicated = BTreeSet::new();
    for source in selected.iter().filter_map(|id| pattern.notes.get(id)) {
        let maximum = pattern_length.0.saturating_sub(source.duration.0) as i64;
        let mut note = source.clone();
        note.id = NoteId::from_raw(next_id);
        next_id = next_id.saturating_add(1);
        note.start.0 = note.start.0.saturating_add(tick_offset).clamp(0, maximum);
        duplicated.insert(note.id);
        result.notes.insert(note.id, note);
    }
    (result, duplicated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sequencer::{Articulation, NotePitch, PerNoteExpression};

    fn note(id: u64, start: i64, duration: u64, key: u8) -> NoteEvent {
        NoteEvent {
            id: NoteId::from_raw(id),
            start: BeatTime(start),
            duration: BeatDuration(duration),
            pitch: NotePitch {
                midi_key: key,
                cents: 0.0,
            },
            velocity: 0.5,
            release_velocity: 0.5,
            pan: 0.0,
            probability: 1.0,
            micro_offset: 0,
            channel: 0,
            instrument: Some(7),
            articulation: Articulation::Normal,
            expression: PerNoteExpression::default(),
        }
    }

    #[test]
    fn marquee_selects_polyphony_by_time_overlap_and_pitch() {
        let pattern = NotePattern {
            notes: BTreeMap::from([
                (NoteId::from_raw(1), note(1, 0, 480, 60)),
                (NoteId::from_raw(2), note(2, 120, 480, 64)),
                (NoteId::from_raw(3), note(3, 960, 480, 60)),
            ]),
        };
        assert_eq!(
            NoteMarquee {
                start_tick: 100,
                end_tick: 500,
                low_key: 59,
                high_key: 62
            }
            .select(&pattern),
            BTreeSet::from([NoteId::from_raw(1)])
        );
    }

    #[test]
    fn group_move_clamps_as_one_shape_and_respects_scale() {
        let pattern = NotePattern {
            notes: BTreeMap::from([
                (NoteId::from_raw(1), note(1, 100, 100, 61)),
                (NoteId::from_raw(2), note(2, 300, 100, 64)),
            ]),
        };
        let batch = NoteBatch::capture(
            &pattern,
            &BTreeSet::from([NoteId::from_raw(1), NoteId::from_raw(2)]),
        );
        let moved = batch.moved(
            BeatDuration(1_000),
            -500,
            1,
            PitchScale {
                root: 0,
                kind: ScaleKind::Major,
            },
        );
        assert_eq!(moved[&NoteId::from_raw(1)].start, BeatTime(0));
        assert_eq!(moved[&NoteId::from_raw(2)].start, BeatTime(200));
        assert_eq!(moved[&NoteId::from_raw(1)].pitch.midi_key, 62);
        assert_eq!(moved[&NoteId::from_raw(2)].pitch.midi_key, 65);
    }

    #[test]
    fn duplicate_preserves_polyphony_and_allocates_stable_ids() {
        let pattern = NotePattern {
            notes: BTreeMap::from([
                (NoteId::from_raw(1), note(1, 0, 240, 60)),
                (NoteId::from_raw(2), note(2, 0, 240, 67)),
            ]),
        };
        let (result, selected) = duplicate_notes(
            &pattern,
            &BTreeSet::from([NoteId::from_raw(1), NoteId::from_raw(2)]),
            10,
            240,
            BeatDuration(960),
        );
        assert_eq!(
            selected,
            BTreeSet::from([NoteId::from_raw(10), NoteId::from_raw(11)])
        );
        assert_eq!(result.notes[&NoteId::from_raw(10)].start, BeatTime(240));
        assert_eq!(result.notes[&NoteId::from_raw(11)].pitch.midi_key, 67);
    }

    #[test]
    fn visible_bars_follow_meter_changes_without_fixed_modulo_drift() {
        let mut map = TempoMap::common_time(48_000, 120.0).unwrap();
        map.set_meter(BeatTime(3_840), TimeSignature::new(3, 4).unwrap())
            .unwrap();
        let ticks = visible_bar_markers(&map, 0, 10_000, 8)
            .into_iter()
            .map(|marker| marker.tick)
            .collect::<Vec<_>>();
        assert_eq!(ticks, vec![0, 3_840, 6_720, 9_600]);
    }

    #[test]
    fn gesture_motion_replaces_preview_but_finishes_as_one_commit() {
        let pattern = NotePattern {
            notes: BTreeMap::from([(NoteId::from_raw(1), note(1, 0, 240, 60))]),
        };
        let selected = BTreeSet::from([NoteId::from_raw(1)]);
        let batch = NoteBatch::capture(&pattern, &selected);
        let mut gesture = PianoGestureTransaction::begin(&pattern);
        gesture.preview_replacement(batch.moved(
            BeatDuration(1_920),
            240,
            0,
            PitchScale::default(),
        ));
        gesture.preview_replacement(batch.moved(
            BeatDuration(1_920),
            480,
            0,
            PitchScale::default(),
        ));

        let PianoGestureResolution::Commit(committed) = gesture.finish() else {
            panic!("changed gesture must commit");
        };
        assert_eq!(committed.notes[&NoteId::from_raw(1)].start, BeatTime(480));
    }

    #[test]
    fn focus_loss_rolls_back_to_the_exact_pre_gesture_pattern() {
        let pattern = NotePattern {
            notes: BTreeMap::from([(NoteId::from_raw(1), note(1, 120, 240, 64))]),
        };
        let selected = BTreeSet::from([NoteId::from_raw(1)]);
        let batch = NoteBatch::capture(&pattern, &selected);
        let mut gesture = PianoGestureTransaction::begin(&pattern);
        gesture.preview_replacement(batch.velocity_scaled(0.4));

        assert_eq!(
            gesture.rollback(),
            PianoGestureResolution::Rollback(pattern)
        );
    }

    #[test]
    fn note_properties_apply_relatively_and_stay_in_musical_bounds() {
        let mut pattern = NotePattern {
            notes: BTreeMap::from([(NoteId::from_raw(1), note(1, 120, 240, 64))]),
        };
        pattern
            .notes
            .get_mut(&NoteId::from_raw(1))
            .unwrap()
            .probability = 0.95;
        let selected = BTreeSet::from([NoteId::from_raw(1)]);
        let batch = NoteBatch::capture(&pattern, &selected);
        assert_eq!(
            batch.probability_scaled(0.2)[&NoteId::from_raw(1)].probability,
            1.0
        );
        assert_eq!(
            batch.microtiming_shifted(-999, 120)[&NoteId::from_raw(1)].micro_offset,
            -120
        );
    }

    #[test]
    fn command_modifier_bypasses_snap_without_changing_raw_delta() {
        assert_eq!(gesture_tick_delta(137, 240, false), 240);
        assert_eq!(gesture_tick_delta(137, 240, true), 137);
        assert_eq!(gesture_tick_delta(-137, 240, true), -137);
    }
}
