//! Pure drum-grid editing policy.
//!
//! The view owns focus and pointer capture; this module owns deterministic
//! selection and batch edits so keyboard, pointer, and future controller
//! surfaces all produce the same `StepPattern` replacement.

use std::collections::{BTreeMap, BTreeSet};

use crate::sequencer::{BeatDuration, StepEvent, StepLaneId, StepPattern};

pub type StepKey = (StepLaneId, u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepMarquee {
    pub start_step: u32,
    pub end_step: u32,
    pub start_lane: usize,
    pub end_lane: usize,
}

impl StepMarquee {
    pub fn select(self, pattern: &StepPattern) -> BTreeSet<StepKey> {
        let first_step = self.start_step.min(self.end_step);
        let last_step = self.start_step.max(self.end_step);
        let first_lane = self.start_lane.min(self.end_lane);
        let last_lane = self.start_lane.max(self.end_lane);
        pattern
            .lanes
            .values()
            .enumerate()
            .filter(|(row, _)| (first_lane..=last_lane).contains(row))
            .flat_map(|(_, lane)| {
                lane.steps
                    .range(first_step..=last_step)
                    .map(move |(step, _)| (lane.id, *step))
            })
            .collect()
    }
}

pub fn all_steps(pattern: &StepPattern) -> BTreeSet<StepKey> {
    pattern
        .lanes
        .values()
        .flat_map(|lane| lane.steps.keys().map(move |step| (lane.id, *step)))
        .collect()
}

pub fn remove_steps(pattern: &StepPattern, selected: &BTreeSet<StepKey>) -> StepPattern {
    let mut result = pattern.clone();
    for (lane, step) in selected {
        if let Some(lane) = result.lanes.get_mut(lane) {
            lane.steps.remove(step);
        }
    }
    result
}

/// Width of the selected time span, suitable for a non-overlapping Cmd-D.
pub fn duplication_offset(selected: &BTreeSet<StepKey>) -> i64 {
    let Some(first) = selected.iter().map(|(_, step)| i64::from(*step)).min() else {
        return 0;
    };
    let last = selected
        .iter()
        .map(|(_, step)| i64::from(*step))
        .max()
        .unwrap_or(first);
    last.saturating_sub(first).saturating_add(1)
}

/// Duplicate a batch without overwriting existing cells. Near the pattern end
/// the whole shape is shifted left just enough to fit; an occupied destination
/// makes the operation inert instead of silently replacing another hit.
pub fn duplicate_steps(
    pattern: &StepPattern,
    selected: &BTreeSet<StepKey>,
    requested_offset: i64,
    pattern_length: BeatDuration,
) -> (StepPattern, BTreeSet<StepKey>) {
    let source = captured_steps(pattern, selected);
    if source.is_empty() || requested_offset == 0 {
        return (pattern.clone(), BTreeSet::new());
    }
    let max_step = maximum_step(pattern, pattern_length);
    let earliest = source
        .keys()
        .map(|(_, step)| i64::from(*step))
        .min()
        .unwrap_or(0);
    let latest = source
        .keys()
        .map(|(_, step)| i64::from(*step))
        .max()
        .unwrap_or(0);
    let offset = requested_offset.clamp(earliest.saturating_neg(), max_step.saturating_sub(latest));
    if offset == 0 {
        return (pattern.clone(), BTreeSet::new());
    }
    let destinations = source
        .iter()
        .map(|((lane, step), event)| {
            let destination = (*lane, (i64::from(*step) + offset) as u32);
            (destination, event.clone())
        })
        .collect::<BTreeMap<_, _>>();
    if destinations.keys().any(|(lane, step)| {
        pattern
            .lanes
            .get(lane)
            .is_some_and(|lane| lane.steps.contains_key(step))
    }) {
        return (pattern.clone(), BTreeSet::new());
    }
    let mut result = pattern.clone();
    for ((lane, step), event) in &destinations {
        result
            .lanes
            .get_mut(lane)
            .expect("captured step lane remains present")
            .steps
            .insert(*step, event.clone());
    }
    (result, destinations.into_keys().collect())
}

/// One copied cell, carrying the lane it came from by identity and by name so
/// a paste into another pattern can resolve it or refuse it by name rather
/// than dropping it.
#[derive(Clone, Debug, PartialEq)]
pub struct CopiedStep {
    pub lane: StepLaneId,
    pub lane_name: String,
    pub step: u32,
    pub event: StepEvent,
}

/// Why a paste did not happen, in the words the editor shows.
#[derive(Clone, Debug, PartialEq)]
pub enum StepPasteRefusal {
    /// The destination pattern holds no lane with this identity or name.
    MissingLane(String),
    /// A destination cell already holds a hit. Pasting over it would delete
    /// something the musician never selected.
    Occupied,
    /// The batch does not fit: every landing place is past the pattern end.
    OutOfRange,
}

pub fn copy_steps(pattern: &StepPattern, selected: &BTreeSet<StepKey>) -> Vec<CopiedStep> {
    captured_steps(pattern, selected)
        .into_iter()
        .map(|((lane, step), event)| CopiedStep {
            lane,
            lane_name: pattern
                .lanes
                .get(&lane)
                .map(|lane| lane.name.clone())
                .unwrap_or_default(),
            step,
            event,
        })
        .collect()
}

/// Width of a copied batch in steps, so a second paste lands after the first.
pub fn copied_span(copied: &[CopiedStep]) -> i64 {
    let Some(first) = copied.iter().map(|copied| i64::from(copied.step)).min() else {
        return 0;
    };
    let last = copied
        .iter()
        .map(|copied| i64::from(copied.step))
        .max()
        .unwrap_or(first);
    last.saturating_sub(first).saturating_add(1)
}

/// Put a copied batch into a pattern at a step offset. Lanes resolve by
/// identity first and by exact name second, so a batch copied from one
/// pattern lands on the same drum in another; anything else is refused.
pub fn paste_steps(
    pattern: &StepPattern,
    copied: &[CopiedStep],
    offset: i64,
    pattern_length: BeatDuration,
) -> Result<(StepPattern, BTreeSet<StepKey>), StepPasteRefusal> {
    if copied.is_empty() {
        return Err(StepPasteRefusal::OutOfRange);
    }
    let max_step = maximum_step(pattern, pattern_length);
    let mut destinations = BTreeMap::new();
    for copied in copied {
        let lane = if pattern.lanes.contains_key(&copied.lane) {
            copied.lane
        } else {
            *pattern
                .lanes
                .iter()
                .find(|(_, lane)| lane.name == copied.lane_name)
                .map(|(id, _)| id)
                .ok_or_else(|| StepPasteRefusal::MissingLane(copied.lane_name.clone()))?
        };
        let step = i64::from(copied.step).saturating_add(offset);
        if step < 0 || step > max_step {
            continue;
        }
        destinations.insert((lane, step as u32), copied.event.clone());
    }
    if destinations.is_empty() {
        return Err(StepPasteRefusal::OutOfRange);
    }
    if destinations.keys().any(|(lane, step)| {
        pattern
            .lanes
            .get(lane)
            .is_some_and(|lane| lane.steps.contains_key(step))
    }) {
        return Err(StepPasteRefusal::Occupied);
    }
    let mut result = pattern.clone();
    for ((lane, step), event) in &destinations {
        result
            .lanes
            .get_mut(lane)
            .expect("resolved paste lane remains present")
            .steps
            .insert(*step, event.clone());
    }
    Ok((result, destinations.into_keys().collect()))
}

/// Translate a batch as one shape. The delta is collectively clamped at the
/// pattern/lane boundaries and the edit is refused if it would overwrite any
/// unselected cell.
pub fn move_steps(
    pattern: &StepPattern,
    selected: &BTreeSet<StepKey>,
    requested_time_delta: i64,
    requested_lane_delta: i32,
    pattern_length: BeatDuration,
) -> Option<(StepPattern, BTreeSet<StepKey>)> {
    let source = captured_steps(pattern, selected);
    if source.is_empty() {
        return None;
    }
    let lane_ids = pattern.lanes.keys().copied().collect::<Vec<_>>();
    let lane_rows = lane_ids
        .iter()
        .copied()
        .enumerate()
        .map(|(row, lane)| (lane, row as i32))
        .collect::<BTreeMap<_, _>>();
    let earliest = source.keys().map(|(_, step)| i64::from(*step)).min()?;
    let latest = source.keys().map(|(_, step)| i64::from(*step)).max()?;
    let first_row = source
        .keys()
        .filter_map(|(lane, _)| lane_rows.get(lane).copied())
        .min()?;
    let last_row = source
        .keys()
        .filter_map(|(lane, _)| lane_rows.get(lane).copied())
        .max()?;
    let time_delta = requested_time_delta.clamp(
        earliest.saturating_neg(),
        maximum_step(pattern, pattern_length).saturating_sub(latest),
    );
    let lane_delta = requested_lane_delta.clamp(
        first_row.saturating_neg(),
        lane_ids.len().saturating_sub(1) as i32 - last_row,
    );
    if time_delta == 0 && lane_delta == 0 {
        return None;
    }
    let destinations = source
        .iter()
        .map(|((lane, step), event)| {
            let row = lane_rows[lane] + lane_delta;
            let destination = (
                lane_ids[row as usize],
                (i64::from(*step) + time_delta) as u32,
            );
            (destination, event.clone())
        })
        .collect::<BTreeMap<_, _>>();
    if destinations.keys().any(|key @ (lane, step)| {
        !selected.contains(key)
            && pattern
                .lanes
                .get(lane)
                .is_some_and(|lane| lane.steps.contains_key(step))
    }) {
        return None;
    }
    let mut result = remove_steps(pattern, selected);
    for ((lane, step), event) in &destinations {
        result
            .lanes
            .get_mut(lane)
            .expect("translated lane is in range")
            .steps
            .insert(*step, event.clone());
    }
    Some((result, destinations.into_keys().collect()))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StepPropertyDelta {
    Velocity(f32),
    Probability(f32),
    MicroOffset(i32),
    Gate(i64),
    Ratchets(i16),
    Pitch(f32),
    Pan(f32),
}

pub fn adjust_steps(
    pattern: &StepPattern,
    selected: &BTreeSet<StepKey>,
    delta: StepPropertyDelta,
) -> StepPattern {
    let mut result = pattern.clone();
    let half_step = (pattern.resolution.0 / 2).min(i32::MAX as u64) as i32;
    for (lane, step) in selected {
        let Some(event) = result
            .lanes
            .get_mut(lane)
            .and_then(|lane| lane.steps.get_mut(step))
        else {
            continue;
        };
        match delta {
            StepPropertyDelta::Velocity(value) => {
                event.velocity = (event.velocity + value).clamp(0.0, 1.0)
            }
            StepPropertyDelta::Probability(value) => {
                event.probability = (event.probability + value).clamp(0.0, 1.0)
            }
            StepPropertyDelta::MicroOffset(value) => {
                event.micro_offset = event
                    .micro_offset
                    .saturating_add(value)
                    .clamp(-half_step, half_step)
            }
            StepPropertyDelta::Gate(value) => {
                event.gate.0 = (i128::from(event.gate.0) + i128::from(value))
                    .clamp(1, i128::from(u64::MAX)) as u64
            }
            StepPropertyDelta::Ratchets(value) => {
                event.ratchets = (i16::from(event.ratchets) + value).clamp(1, 32) as u8
            }
            StepPropertyDelta::Pitch(value) => {
                event.pitch_semitones = (event.pitch_semitones + value).clamp(-48.0, 48.0)
            }
            StepPropertyDelta::Pan(value) => event.pan = (event.pan + value).clamp(-1.0, 1.0),
        }
    }
    result
}

fn captured_steps(
    pattern: &StepPattern,
    selected: &BTreeSet<StepKey>,
) -> BTreeMap<StepKey, StepEvent> {
    selected
        .iter()
        .filter_map(|(lane, step)| {
            pattern
                .lanes
                .get(lane)?
                .steps
                .get(step)
                .cloned()
                .map(|event| ((*lane, *step), event))
        })
        .collect()
}

fn maximum_step(pattern: &StepPattern, pattern_length: BeatDuration) -> i64 {
    if pattern.resolution.0 == 0 || pattern_length.0 == 0 {
        return 0;
    }
    (pattern_length.0.saturating_sub(1) / pattern.resolution.0).min(i64::MAX as u64) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sequencer::{StepLane, TriggerTarget};

    fn event(velocity: f32) -> StepEvent {
        StepEvent {
            velocity,
            probability: 1.0,
            micro_offset: 0,
            gate: BeatDuration(240),
            ratchets: 1,
            pitch_semitones: 0.0,
            pan: 0.0,
        }
    }

    fn pattern() -> StepPattern {
        let first = StepLaneId::from_raw(1);
        let second = StepLaneId::from_raw(2);
        StepPattern {
            resolution: BeatDuration(240),
            swing: 0.0,
            lanes: BTreeMap::from([
                (
                    first,
                    StepLane {
                        id: first,
                        name: "Kick".into(),
                        target: TriggerTarget::DrumPad { rack: 1, pad: 0 },
                        choke_group: None,
                        steps: BTreeMap::from([(0, event(0.8)), (4, event(0.6))]),
                    },
                ),
                (
                    second,
                    StepLane {
                        id: second,
                        name: "Hat".into(),
                        target: TriggerTarget::DrumPad { rack: 1, pad: 1 },
                        choke_group: None,
                        steps: BTreeMap::from([(2, event(0.5))]),
                    },
                ),
            ]),
        }
    }

    #[test]
    fn marquee_and_select_all_keep_stable_lane_step_keys() {
        let pattern = pattern();
        assert_eq!(
            StepMarquee {
                start_step: 0,
                end_step: 2,
                start_lane: 0,
                end_lane: 1,
            }
            .select(&pattern),
            BTreeSet::from([(StepLaneId::from_raw(1), 0), (StepLaneId::from_raw(2), 2),])
        );
        assert_eq!(all_steps(&pattern).len(), 3);
    }

    #[test]
    fn duplicate_uses_selection_width_and_never_overwrites() {
        let pattern = pattern();
        let selection =
            BTreeSet::from([(StepLaneId::from_raw(1), 0), (StepLaneId::from_raw(2), 2)]);
        assert_eq!(duplication_offset(&selection), 3);
        let (duplicated, selected) = duplicate_steps(
            &pattern,
            &selection,
            duplication_offset(&selection),
            BeatDuration(1_920),
        );
        assert_eq!(
            selected,
            BTreeSet::from([(StepLaneId::from_raw(1), 3), (StepLaneId::from_raw(2), 5),])
        );
        assert_eq!(
            duplicated.lanes[&StepLaneId::from_raw(1)].steps[&3].velocity,
            0.8
        );
        assert_eq!(
            duplicated.lanes[&StepLaneId::from_raw(2)].steps[&5].velocity,
            0.5
        );

        let occupied = BTreeSet::from([(StepLaneId::from_raw(1), 0)]);
        let (unchanged, selected) = duplicate_steps(&pattern, &occupied, 4, BeatDuration(1_920));
        assert_eq!(unchanged, pattern);
        assert!(selected.is_empty());
    }

    #[test]
    fn group_move_clamps_as_one_shape_and_refuses_collisions() {
        let pattern = pattern();
        let selected = BTreeSet::from([(StepLaneId::from_raw(1), 4), (StepLaneId::from_raw(2), 2)]);
        let (moved, moved_selection) =
            move_steps(&pattern, &selected, -4, 0, BeatDuration(1_920)).unwrap();
        assert_eq!(
            moved_selection,
            BTreeSet::from([(StepLaneId::from_raw(1), 2), (StepLaneId::from_raw(2), 0),])
        );
        assert_eq!(
            moved.lanes[&StepLaneId::from_raw(1)].steps[&2].velocity,
            0.6
        );
        assert_eq!(
            moved.lanes[&StepLaneId::from_raw(2)].steps[&0].velocity,
            0.5
        );

        assert!(move_steps(
            &pattern,
            &BTreeSet::from([(StepLaneId::from_raw(1), 0)]),
            4,
            0,
            BeatDuration(1_920),
        )
        .is_none());
    }

    #[test]
    fn properties_apply_relatively_with_musical_bounds() {
        let pattern = pattern();
        let selected = BTreeSet::from([(StepLaneId::from_raw(1), 0), (StepLaneId::from_raw(2), 2)]);
        let velocity = adjust_steps(&pattern, &selected, StepPropertyDelta::Velocity(0.3));
        assert_eq!(
            velocity.lanes[&StepLaneId::from_raw(1)].steps[&0].velocity,
            1.0
        );
        assert_eq!(
            velocity.lanes[&StepLaneId::from_raw(2)].steps[&2].velocity,
            0.8
        );

        let ratchets = adjust_steps(&pattern, &selected, StepPropertyDelta::Ratchets(2));
        assert_eq!(
            ratchets.lanes[&StepLaneId::from_raw(1)].steps[&0].ratchets,
            3
        );

        let early = adjust_steps(&pattern, &selected, StepPropertyDelta::MicroOffset(-999));
        assert_eq!(
            early.lanes[&StepLaneId::from_raw(1)].steps[&0].micro_offset,
            -120
        );
    }

    #[test]
    fn a_copied_batch_pastes_into_the_lane_it_came_from() {
        let pattern = pattern();
        let selected = BTreeSet::from([(StepLaneId::from_raw(1), 0u32)]);
        let copied = copy_steps(&pattern, &selected);
        assert_eq!(copied.len(), 1);
        assert_eq!(copied[0].lane_name, "Kick");
        assert_eq!(copied_span(&copied), 1);

        let (pasted, landed) = paste_steps(&pattern, &copied, 8, BeatDuration(3_840))
            .expect("an empty destination accepts the paste");
        assert_eq!(landed, BTreeSet::from([(StepLaneId::from_raw(1), 8u32)]));
        assert_eq!(
            pasted.lanes[&StepLaneId::from_raw(1)].steps[&8].velocity,
            0.8
        );
        // The source cells are untouched.
        assert!(pasted.lanes[&StepLaneId::from_raw(1)]
            .steps
            .contains_key(&0));
    }

    #[test]
    fn a_paste_over_an_occupied_cell_or_past_the_end_refuses_by_name() {
        let pattern = pattern();
        let copied = copy_steps(&pattern, &BTreeSet::from([(StepLaneId::from_raw(1), 0u32)]));
        assert_eq!(
            paste_steps(&pattern, &copied, 4, BeatDuration(3_840)),
            Err(StepPasteRefusal::Occupied),
            "step 4 of Kick already holds a hit"
        );
        assert_eq!(
            paste_steps(&pattern, &copied, 4_000, BeatDuration(3_840)),
            Err(StepPasteRefusal::OutOfRange)
        );
    }

    #[test]
    fn a_paste_into_a_pattern_without_that_lane_names_the_lane() {
        let mut destination = pattern();
        destination.lanes.remove(&StepLaneId::from_raw(1));
        let copied = copy_steps(
            &pattern(),
            &BTreeSet::from([(StepLaneId::from_raw(1), 0u32)]),
        );
        assert_eq!(
            paste_steps(&destination, &copied, 8, BeatDuration(3_840)),
            Err(StepPasteRefusal::MissingLane("Kick".into()))
        );
    }

    #[test]
    fn a_lane_that_moved_identity_still_resolves_by_its_name() {
        let source = pattern();
        let copied = copy_steps(&source, &BTreeSet::from([(StepLaneId::from_raw(1), 0u32)]));
        let mut destination = pattern();
        let moved = StepLaneId::from_raw(9);
        let mut lane = destination
            .lanes
            .remove(&StepLaneId::from_raw(1))
            .expect("the Kick lane exists");
        lane.id = moved;
        lane.steps.clear();
        destination.lanes.insert(moved, lane);
        let (pasted, landed) = paste_steps(&destination, &copied, 0, BeatDuration(3_840))
            .expect("the same drum by name accepts the paste");
        assert_eq!(landed, BTreeSet::from([(moved, 0u32)]));
        assert_eq!(pasted.lanes[&moved].steps[&0].velocity, 0.8);
    }
}
