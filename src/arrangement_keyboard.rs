//! Pure keyboard-edit planning for the arrangement surface.
//!
//! This module refuses to mutate an editor, allocate project identities, or
//! interpret platform key chords.  It turns a stable object selection plus an
//! exact frame/track displacement into the same semantic terms pointer
//! gestures emit.  Consequently keyboard and pointer edits share validation,
//! aggregate undo, provenance, and render invalidation rather than drifting
//! into parallel implementations.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::arrangement::{ArrangementState, ClipId, TrackId, TrackKind};
use crate::arrangement_interaction::{
    ArrangementEdit, ArrangementEditIntent, ClipMove, SelectionIntent, SelectionMode,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionNavigation {
    All,
    First,
    Previous,
    Next,
    Last,
    Clear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackDirection {
    Previous,
    Next,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyboardPlanError {
    EmptySelection,
    MissingClip(ClipId),
    LockedClip(ClipId),
    MissingTrack(TrackId),
    LockedTrack(TrackId),
    NoAdjacentTrack {
        track: TrackId,
        direction: TrackDirection,
    },
    IncompatibleTrack {
        clip: ClipId,
        track: TrackId,
    },
    TimeOverflow,
}

impl fmt::Display for KeyboardPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySelection => formatter.write_str("no arrangement clips are selected"),
            Self::MissingClip(id) => write!(formatter, "selected clip {id} no longer exists"),
            Self::LockedClip(id) => write!(formatter, "selected clip {id} is locked"),
            Self::MissingTrack(id) => write!(formatter, "track {id} no longer exists"),
            Self::LockedTrack(id) => write!(formatter, "track {id} is locked"),
            Self::NoAdjacentTrack { track, direction } => {
                write!(
                    formatter,
                    "track {track} has no {direction:?} arrangement track"
                )
            }
            Self::IncompatibleTrack { clip, track } => {
                write!(formatter, "clip {clip} cannot move to track {track}")
            }
            Self::TimeOverflow => {
                formatter.write_str("keyboard edit leaves the frame address space")
            }
        }
    }
}

impl Error for KeyboardPlanError {}

/// Stable visual order: track order first, then placement, then typed ID.
/// Corrupt/orphan identities are deliberately absent instead of acquiring an
/// invented track position.
pub fn ordered_clips(state: &ArrangementState) -> Vec<ClipId> {
    let track_order: BTreeMap<_, _> = state
        .track_order
        .iter()
        .copied()
        .enumerate()
        .map(|(index, id)| (id, index))
        .collect();
    let mut clips: Vec<_> = state
        .clips
        .values()
        .filter_map(|clip| {
            Some((
                *track_order.get(&clip.track_id)?,
                clip.placement.start,
                clip.placement.end,
                clip.id,
            ))
        })
        .collect();
    clips.sort();
    clips.into_iter().map(|(_, _, _, id)| id).collect()
}

/// Plan object focus without changing time selection or loop state.
pub fn plan_selection_navigation(
    state: &ArrangementState,
    selected: &BTreeSet<ClipId>,
    navigation: SelectionNavigation,
) -> SelectionIntent {
    if navigation == SelectionNavigation::Clear {
        return SelectionIntent::ClearObjects;
    }
    let order = ordered_clips(state);
    if navigation == SelectionNavigation::All {
        let ids = order.iter().copied().collect();
        return SelectionIntent::Clips {
            ids,
            primary: order.first().copied(),
            mode: SelectionMode::Replace,
        };
    }
    let current = order.iter().position(|id| selected.contains(id));
    let target = match navigation {
        SelectionNavigation::First => order.first().copied(),
        SelectionNavigation::Previous => current
            .map(|current| order[(current + order.len().saturating_sub(1)) % order.len()])
            .or_else(|| order.last().copied()),
        SelectionNavigation::Next => current
            .map(|current| order[(current + 1) % order.len()])
            .or_else(|| order.first().copied()),
        SelectionNavigation::Last => order.last().copied(),
        SelectionNavigation::All | SelectionNavigation::Clear => unreachable!(),
    };
    SelectionIntent::Clips {
        ids: target.into_iter().collect(),
        primary: target,
        mode: SelectionMode::Replace,
    }
}

/// Move every selected clip by one exact project-time delta. Relative timing
/// and track identity are preserved, including selections spanning tracks.
pub fn plan_nudge(
    state: &ArrangementState,
    selected: &BTreeSet<ClipId>,
    expected_revision: u64,
    delta_frames: i64,
) -> Result<ArrangementEditIntent, KeyboardPlanError> {
    let clips = selected_clips(state, selected)?;
    let moves = clips
        .into_iter()
        .map(|clip| {
            editable(state, clip.id)?;
            Ok(ClipMove {
                clip_id: clip.id,
                from_track: clip.track_id,
                to_track: clip.track_id,
                from: clip.placement,
                to: clip
                    .placement
                    .translated(delta_frames)
                    .map_err(|_| KeyboardPlanError::TimeOverflow)?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(intent(expected_revision, moves, false))
}

/// Duplicate a selection as one phrase. The bounding-span displacement keeps
/// inter-clip rests and cross-track alignment intact. `quantum_frames` rounds
/// the first duplicate start upward so repeated keyboard duplication remains
/// musically stable without moving any source clip.
pub fn plan_duplicate_after(
    state: &ArrangementState,
    selected: &BTreeSet<ClipId>,
    expected_revision: u64,
    quantum_frames: u64,
) -> Result<ArrangementEditIntent, KeyboardPlanError> {
    let clips = selected_clips(state, selected)?;
    for clip in &clips {
        editable(state, clip.id)?;
    }
    let first = clips
        .iter()
        .map(|clip| clip.placement.start.0)
        .min()
        .ok_or(KeyboardPlanError::EmptySelection)?;
    let end = clips
        .iter()
        .map(|clip| clip.placement.end.0)
        .max()
        .ok_or(KeyboardPlanError::EmptySelection)?;
    let destination = ceil_to_quantum(end, quantum_frames.max(1))?;
    let delta = destination
        .checked_sub(first)
        .ok_or(KeyboardPlanError::TimeOverflow)?;
    let moves = clips
        .into_iter()
        .map(|clip| {
            Ok(ClipMove {
                clip_id: clip.id,
                from_track: clip.track_id,
                to_track: clip.track_id,
                from: clip.placement,
                to: clip
                    .placement
                    .translated(delta)
                    .map_err(|_| KeyboardPlanError::TimeOverflow)?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(intent(expected_revision, moves, true))
}

/// Move a possibly multi-track selection one track up/down while preserving
/// the selection's vertical shape. Every destination is checked before a
/// semantic term is returned, so the operation is atomic at its planning
/// boundary as well as at command application.
pub fn plan_move_to_adjacent_tracks(
    state: &ArrangementState,
    selected: &BTreeSet<ClipId>,
    expected_revision: u64,
    direction: TrackDirection,
) -> Result<ArrangementEditIntent, KeyboardPlanError> {
    let clips = selected_clips(state, selected)?;
    let positions: BTreeMap<_, _> = state
        .track_order
        .iter()
        .copied()
        .enumerate()
        .map(|(index, id)| (id, index))
        .collect();
    let mut moves = Vec::with_capacity(clips.len());
    for clip in clips {
        editable(state, clip.id)?;
        let index = *positions
            .get(&clip.track_id)
            .ok_or(KeyboardPlanError::MissingTrack(clip.track_id))?;
        let destination_index = match direction {
            TrackDirection::Previous => index.checked_sub(1),
            TrackDirection::Next => index.checked_add(1),
        }
        .filter(|index| *index < state.track_order.len())
        .ok_or(KeyboardPlanError::NoAdjacentTrack {
            track: clip.track_id,
            direction,
        })?;
        let destination = state.track_order[destination_index];
        let track = state
            .track(destination)
            .ok_or(KeyboardPlanError::MissingTrack(destination))?;
        if track.locked {
            return Err(KeyboardPlanError::LockedTrack(destination));
        }
        let kind = clip.content.kind();
        if track.kind != TrackKind::Hybrid && track.kind != kind {
            return Err(KeyboardPlanError::IncompatibleTrack {
                clip: clip.id,
                track: destination,
            });
        }
        moves.push(ClipMove {
            clip_id: clip.id,
            from_track: clip.track_id,
            to_track: destination,
            from: clip.placement,
            to: clip.placement,
        });
    }
    Ok(intent(expected_revision, moves, false))
}

fn selected_clips<'a>(
    state: &'a ArrangementState,
    selected: &BTreeSet<ClipId>,
) -> Result<Vec<&'a crate::arrangement::Clip>, KeyboardPlanError> {
    if selected.is_empty() {
        return Err(KeyboardPlanError::EmptySelection);
    }
    selected
        .iter()
        .map(|id| state.clip(*id).ok_or(KeyboardPlanError::MissingClip(*id)))
        .collect()
}

fn editable(state: &ArrangementState, clip_id: ClipId) -> Result<(), KeyboardPlanError> {
    let clip = state
        .clip(clip_id)
        .ok_or(KeyboardPlanError::MissingClip(clip_id))?;
    if clip.locked {
        return Err(KeyboardPlanError::LockedClip(clip_id));
    }
    let track = state
        .track(clip.track_id)
        .ok_or(KeyboardPlanError::MissingTrack(clip.track_id))?;
    if track.locked {
        return Err(KeyboardPlanError::LockedTrack(track.id));
    }
    Ok(())
}

fn intent(expected_revision: u64, moves: Vec<ClipMove>, duplicate: bool) -> ArrangementEditIntent {
    ArrangementEditIntent {
        expected_revision,
        edit: ArrangementEdit::MoveClips { moves, duplicate },
    }
}

fn ceil_to_quantum(frame: i64, quantum: u64) -> Result<i64, KeyboardPlanError> {
    let quantum = i64::try_from(quantum).map_err(|_| KeyboardPlanError::TimeOverflow)?;
    let remainder = frame.rem_euclid(quantum);
    if remainder == 0 {
        Ok(frame)
    } else {
        frame
            .checked_add(quantum - remainder)
            .ok_or(KeyboardPlanError::TimeOverflow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrangement::{
        ArrangementEditor, ArrangementOperation, ArrangementTransaction, AssetId, Frame,
        FrameRange, SourceRange, TrackKind,
    };

    fn fixture() -> (ArrangementEditor, Vec<TrackId>, Vec<ClipId>) {
        let mut editor = ArrangementEditor::new(48_000).unwrap();
        let tracks = vec![
            editor.create_track("Audio 1", TrackKind::Audio).unwrap(),
            editor.create_track("Audio 2", TrackKind::Audio).unwrap(),
            editor.create_track("Audio 3", TrackKind::Audio).unwrap(),
        ];
        let clips = vec![
            editor
                .create_audio_clip(
                    tracks[0],
                    "A",
                    FrameRange::new(Frame(100), Frame(200)).unwrap(),
                    AssetId::from_raw(1),
                    SourceRange::new(0, 100).unwrap(),
                )
                .unwrap(),
            editor
                .create_audio_clip(
                    tracks[1],
                    "B",
                    FrameRange::new(Frame(250), Frame(350)).unwrap(),
                    AssetId::from_raw(1),
                    SourceRange::new(100, 200).unwrap(),
                )
                .unwrap(),
            editor
                .create_audio_clip(
                    tracks[0],
                    "C",
                    FrameRange::new(Frame(500), Frame(600)).unwrap(),
                    AssetId::from_raw(1),
                    SourceRange::new(200, 300).unwrap(),
                )
                .unwrap(),
        ];
        (editor, tracks, clips)
    }

    #[test]
    fn navigation_uses_visual_order_and_wraps() {
        let (editor, _, clips) = fixture();
        assert_eq!(
            ordered_clips(editor.state()),
            vec![clips[0], clips[2], clips[1]]
        );
        let selected = BTreeSet::from([clips[1]]);
        assert_eq!(
            plan_selection_navigation(editor.state(), &selected, SelectionNavigation::Next),
            SelectionIntent::Clips {
                ids: BTreeSet::from([clips[0]]),
                primary: Some(clips[0]),
                mode: SelectionMode::Replace,
            }
        );
        assert_eq!(
            plan_selection_navigation(editor.state(), &BTreeSet::new(), SelectionNavigation::Next,),
            SelectionIntent::Clips {
                ids: BTreeSet::from([clips[0]]),
                primary: Some(clips[0]),
                mode: SelectionMode::Replace,
            }
        );
    }

    #[test]
    fn group_nudge_preserves_relative_time_and_tracks() {
        let (editor, tracks, clips) = fixture();
        let intent = plan_nudge(
            editor.state(),
            &BTreeSet::from([clips[0], clips[1]]),
            17,
            -25,
        )
        .unwrap();
        let ArrangementEdit::MoveClips { moves, duplicate } = intent.edit else {
            panic!("expected move");
        };
        assert!(!duplicate);
        assert_eq!(moves[0].to.start, Frame(75));
        assert_eq!(moves[1].to.start, Frame(225));
        assert_eq!(moves[0].to_track, tracks[0]);
        assert_eq!(moves[1].to_track, tracks[1]);
    }

    #[test]
    fn group_duplicate_preserves_phrase_gaps_and_quantizes_once() {
        let (editor, _, clips) = fixture();
        let intent = plan_duplicate_after(
            editor.state(),
            &BTreeSet::from([clips[0], clips[1]]),
            18,
            128,
        )
        .unwrap();
        let ArrangementEdit::MoveClips { moves, duplicate } = intent.edit else {
            panic!("expected move");
        };
        assert!(duplicate);
        // Selection bounds are 100..350; the next 128-frame boundary is 384.
        assert_eq!(
            moves[0].to,
            FrameRange::new(Frame(384), Frame(484)).unwrap()
        );
        assert_eq!(
            moves[1].to,
            FrameRange::new(Frame(534), Frame(634)).unwrap()
        );
    }

    #[test]
    fn vertical_move_is_atomic_and_preserves_multitrack_shape() {
        let (editor, tracks, clips) = fixture();
        let intent = plan_move_to_adjacent_tracks(
            editor.state(),
            &BTreeSet::from([clips[0], clips[1]]),
            19,
            TrackDirection::Next,
        )
        .unwrap();
        let ArrangementEdit::MoveClips { moves, .. } = intent.edit else {
            panic!("expected move");
        };
        assert_eq!(moves[0].to_track, tracks[1]);
        assert_eq!(moves[1].to_track, tracks[2]);
        assert_eq!(moves[0].to, moves[0].from);
        assert_eq!(moves[1].to, moves[1].from);

        assert_eq!(
            plan_move_to_adjacent_tracks(
                editor.state(),
                &BTreeSet::from([clips[1]]),
                20,
                TrackDirection::Next,
            )
            .unwrap()
            .edit,
            ArrangementEdit::MoveClips {
                moves: vec![ClipMove {
                    clip_id: clips[1],
                    from_track: tracks[1],
                    to_track: tracks[2],
                    from: FrameRange::new(Frame(250), Frame(350)).unwrap(),
                    to: FrameRange::new(Frame(250), Frame(350)).unwrap(),
                }],
                duplicate: false,
            }
        );
    }

    #[test]
    fn locked_member_refuses_the_entire_batch() {
        let (mut editor, _, clips) = fixture();
        let before = editor.state().clip(clips[1]).unwrap().clone();
        let mut after = before.clone();
        after.locked = true;
        editor
            .apply(ArrangementTransaction::new(
                "Lock clip",
                vec![ArrangementOperation::PutClip {
                    before: Some(before),
                    after: Some(after),
                }],
            ))
            .unwrap();
        assert_eq!(
            plan_nudge(editor.state(), &BTreeSet::from([clips[0], clips[1]]), 21, 1,),
            Err(KeyboardPlanError::LockedClip(clips[1]))
        );
    }
}
