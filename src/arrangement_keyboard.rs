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

use crate::arrangement::{
    ArrangementState, ClipContent, ClipId, Fade, FadeCurve, Frame, FrameRange,
    StretchAlgorithm, TrackId, TrackKind,
};
use crate::arrangement_interaction::{
    ArrangementEdit, ArrangementEditIntent, ClipMove, FadeEdge, PhraseClipEdit, SelectionIntent,
    SelectionMode, SnapContext, SnapResult, TrimEdge,
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
    AnchorNotSelected(ClipId),
    InvalidBoundary(ClipId),
    UnsupportedFade(ClipId),
    UnsupportedRepeat(ClipId),
    UnsupportedStretch(ClipId),
    WarpedStretchRequiresCompiler(ClipId),
    IdentityExhausted,
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
            Self::AnchorNotSelected(id) => {
                write!(formatter, "phrase anchor clip {id} is not selected")
            }
            Self::InvalidBoundary(id) => {
                write!(formatter, "phrase boundary is invalid for clip {id}")
            }
            Self::UnsupportedFade(id) => write!(formatter, "clip {id} does not support fades"),
            Self::UnsupportedRepeat(id) => {
                write!(formatter, "clip {id} does not support placement repeat")
            }
            Self::UnsupportedStretch(id) => {
                write!(formatter, "clip {id} does not support time stretch")
            }
            Self::WarpedStretchRequiresCompiler(id) => {
                write!(
                    formatter,
                    "clip {id} requires the warp-marker stretch compiler"
                )
            }
            Self::IdentityExhausted => {
                formatter.write_str("phrase edit exhausted arrangement clip identities")
            }
            Self::TimeOverflow => {
                formatter.write_str("keyboard edit leaves the frame address space")
            }
        }
    }
}

impl Error for KeyboardPlanError {}

/// Typed, viewport-independent completion data for one phrase operation.
/// `selection` can be applied immediately at the ephemeral view boundary; a
/// revision conflict prevents the corresponding command from publishing.
#[derive(Clone, Debug, PartialEq)]
pub struct PhraseEditPlan {
    pub intent: ArrangementEditIntent,
    pub selection: SelectionIntent,
    pub reveal: PhraseReveal,
    pub snap: Option<SnapResult>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhraseReveal {
    pub clips: BTreeSet<ClipId>,
    pub primary: Option<ClipId>,
    pub range: FrameRange,
}

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

/// Split every selected occurrence at one absolute project-frame boundary.
/// The predicted right-hand IDs are safe to expose as typed selection targets:
/// command lowering uses the same revision-guarded allocator cursor.
pub fn plan_phrase_split(
    state: &ArrangementState,
    selected: &BTreeSet<ClipId>,
    expected_revision: u64,
    proposed: Frame,
    snap: Option<&SnapContext>,
) -> Result<PhraseEditPlan, KeyboardPlanError> {
    let clips = selected_clips(state, selected)?;
    let (boundary, snap) = snapped_boundary(proposed, selected, snap);
    let mut edits = Vec::with_capacity(clips.len());
    let mut right_ids = BTreeSet::new();
    let mut right_by_source = BTreeMap::new();
    let mut next_id = state.next_clip_id;
    let mut reveal_end = boundary;
    for clip in clips {
        editable(state, clip.id)?;
        if !clip.placement.contains(boundary) || boundary == clip.placement.start {
            return Err(KeyboardPlanError::InvalidBoundary(clip.id));
        }
        let created = ClipId::from_raw(next_id);
        next_id = next_id
            .checked_add(1)
            .ok_or(KeyboardPlanError::IdentityExhausted)?;
        edits.push(PhraseClipEdit::Split {
            clip_id: clip.id,
            boundary,
        });
        right_ids.insert(created);
        right_by_source.insert(clip.id, created);
        reveal_end = reveal_end.max(clip.placement.end);
    }
    let primary = selected
        .iter()
        .next()
        .and_then(|source| right_by_source.get(source))
        .copied();
    phrase_plan(
        expected_revision,
        edits,
        right_ids,
        primary,
        FrameRange::new(boundary, reveal_end).map_err(|_| KeyboardPlanError::TimeOverflow)?,
        snap,
    )
}

/// Move the same logical edge of every selected clip by the anchor's snapped
/// project-time delta. Different clip starts and ends remain cross-track
/// aligned because snapping is resolved once rather than per occurrence.
pub fn plan_phrase_trim(
    state: &ArrangementState,
    selected: &BTreeSet<ClipId>,
    expected_revision: u64,
    anchor: ClipId,
    edge: TrimEdge,
    proposed_anchor_boundary: Frame,
    snap: Option<&SnapContext>,
) -> Result<PhraseEditPlan, KeyboardPlanError> {
    let clips = selected_clips_with_anchor(state, selected, anchor)?;
    let anchor_clip = state
        .clip(anchor)
        .ok_or(KeyboardPlanError::MissingClip(anchor))?;
    let anchor_edge = match edge {
        TrimEdge::Left => anchor_clip.placement.start,
        TrimEdge::Right => anchor_clip.placement.end,
    };
    let (boundary, snap) = snapped_boundary(proposed_anchor_boundary, selected, snap);
    let delta = boundary
        .0
        .checked_sub(anchor_edge.0)
        .ok_or(KeyboardPlanError::TimeOverflow)?;
    let mut edits = Vec::with_capacity(clips.len());
    let mut after_ranges = Vec::with_capacity(clips.len());
    for clip in clips {
        editable(state, clip.id)?;
        let moved_edge = match edge {
            TrimEdge::Left => clip.placement.start.0.checked_add(delta),
            TrimEdge::Right => clip.placement.end.0.checked_add(delta),
        }
        .map(Frame)
        .ok_or(KeyboardPlanError::TimeOverflow)?;
        let after = match edge {
            TrimEdge::Left => FrameRange::new(moved_edge, clip.placement.end),
            TrimEdge::Right => FrameRange::new(clip.placement.start, moved_edge),
        }
        .map_err(|_| KeyboardPlanError::InvalidBoundary(clip.id))?;
        edits.push(PhraseClipEdit::Trim {
            clip_id: clip.id,
            edge,
            boundary: moved_edge,
        });
        after_ranges.push(after);
    }
    phrase_plan_for_existing(
        expected_revision,
        edits,
        selected,
        anchor,
        &after_ranges,
        snap,
    )
}

/// Apply one snapped fade duration to every selected audio clip. Curves remain
/// clip-local, so a selection can retain deliberate linear/equal-power choices
/// while its handles move as one phrase.
pub fn plan_phrase_fade(
    state: &ArrangementState,
    selected: &BTreeSet<ClipId>,
    expected_revision: u64,
    anchor: ClipId,
    edge: FadeEdge,
    proposed_anchor_boundary: Frame,
    snap: Option<&SnapContext>,
) -> Result<PhraseEditPlan, KeyboardPlanError> {
    let clips = selected_clips_with_anchor(state, selected, anchor)?;
    let anchor_clip = state
        .clip(anchor)
        .ok_or(KeyboardPlanError::MissingClip(anchor))?;
    let anchor_edge = match edge {
        FadeEdge::In => anchor_clip.placement.start,
        FadeEdge::Out => anchor_clip.placement.end,
    };
    let (boundary, snap) = snapped_boundary(proposed_anchor_boundary, selected, snap);
    let signed_duration = match edge {
        FadeEdge::In => boundary.0.checked_sub(anchor_edge.0),
        FadeEdge::Out => anchor_edge.0.checked_sub(boundary.0),
    }
    .ok_or(KeyboardPlanError::TimeOverflow)?;
    let duration =
        u64::try_from(signed_duration).map_err(|_| KeyboardPlanError::InvalidBoundary(anchor))?;
    let mut edits = Vec::with_capacity(clips.len());
    let mut ranges = Vec::with_capacity(clips.len());
    for clip in clips {
        editable(state, clip.id)?;
        if !matches!(clip.content, ClipContent::Audio(_)) {
            return Err(KeyboardPlanError::UnsupportedFade(clip.id));
        }
        if duration > clip.placement.len() {
            return Err(KeyboardPlanError::InvalidBoundary(clip.id));
        }
        let mut fades = clip.fades;
        let slot = match edge {
            FadeEdge::In => &mut fades.fade_in,
            FadeEdge::Out => &mut fades.fade_out,
        };
        let curve = slot.map_or(FadeCurve::EqualPower, |fade| fade.curve);
        *slot = (duration > 0).then(|| Fade::full(duration, curve));
        edits.push(PhraseClipEdit::SetFades {
            clip_id: clip.id,
            fades,
        });
        ranges.push(clip.placement);
    }
    phrase_plan_for_existing(expected_revision, edits, selected, anchor, &ranges, snap)
}

/// Extend or contract every selected pattern/automation repeat by the same
/// snapped delta from the anchor's right edge.
pub fn plan_phrase_repeat(
    state: &ArrangementState,
    selected: &BTreeSet<ClipId>,
    expected_revision: u64,
    anchor: ClipId,
    proposed_anchor_boundary: Frame,
    snap: Option<&SnapContext>,
) -> Result<PhraseEditPlan, KeyboardPlanError> {
    let clips = selected_clips_with_anchor(state, selected, anchor)?;
    let anchor_clip = state
        .clip(anchor)
        .ok_or(KeyboardPlanError::MissingClip(anchor))?;
    let (boundary, snap) = snapped_boundary(proposed_anchor_boundary, selected, snap);
    let delta = boundary
        .0
        .checked_sub(anchor_clip.placement.end.0)
        .ok_or(KeyboardPlanError::TimeOverflow)?;
    let mut edits = Vec::with_capacity(clips.len());
    let mut ranges = Vec::with_capacity(clips.len());
    for clip in clips {
        editable(state, clip.id)?;
        if matches!(clip.content, ClipContent::Audio(_)) {
            return Err(KeyboardPlanError::UnsupportedRepeat(clip.id));
        }
        let end = clip
            .placement
            .end
            .0
            .checked_add(delta)
            .map(Frame)
            .ok_or(KeyboardPlanError::TimeOverflow)?;
        let after = FrameRange::new(clip.placement.start, end)
            .map_err(|_| KeyboardPlanError::InvalidBoundary(clip.id))?;
        edits.push(PhraseClipEdit::SetRepeatBoundary {
            clip_id: clip.id,
            boundary: end,
        });
        ranges.push(after);
    }
    phrase_plan_for_existing(expected_revision, edits, selected, anchor, &ranges, snap)
}

/// Stretch each selected audio occurrence by one shared right-edge delta.
/// Source ranges remain untouched; the aggregate lowerer derives every exact
/// ratio inside the same command envelope.
pub fn plan_phrase_stretch(
    state: &ArrangementState,
    selected: &BTreeSet<ClipId>,
    expected_revision: u64,
    anchor: ClipId,
    proposed_anchor_boundary: Frame,
    algorithm: StretchAlgorithm,
    preserve_pitch: bool,
    snap: Option<&SnapContext>,
) -> Result<PhraseEditPlan, KeyboardPlanError> {
    let clips = selected_clips_with_anchor(state, selected, anchor)?;
    let anchor_clip = state
        .clip(anchor)
        .ok_or(KeyboardPlanError::MissingClip(anchor))?;
    let (boundary, snap) = snapped_boundary(proposed_anchor_boundary, selected, snap);
    let delta = boundary
        .0
        .checked_sub(anchor_clip.placement.end.0)
        .ok_or(KeyboardPlanError::TimeOverflow)?;
    let mut edits = Vec::with_capacity(clips.len());
    let mut ranges = Vec::with_capacity(clips.len());
    for clip in clips {
        editable(state, clip.id)?;
        let ClipContent::Audio(audio) = &clip.content else {
            return Err(KeyboardPlanError::UnsupportedStretch(clip.id));
        };
        if !audio.playback.warp_markers.is_empty() {
            return Err(KeyboardPlanError::WarpedStretchRequiresCompiler(clip.id));
        }
        let end = clip
            .placement
            .end
            .0
            .checked_add(delta)
            .map(Frame)
            .ok_or(KeyboardPlanError::TimeOverflow)?;
        let after = FrameRange::new(clip.placement.start, end)
            .map_err(|_| KeyboardPlanError::InvalidBoundary(clip.id))?;
        edits.push(PhraseClipEdit::Stretch {
            clip_id: clip.id,
            boundary: end,
            algorithm,
            preserve_pitch,
        });
        ranges.push(after);
    }
    phrase_plan_for_existing(expected_revision, edits, selected, anchor, &ranges, snap)
}

fn selected_clips_with_anchor<'a>(
    state: &'a ArrangementState,
    selected: &BTreeSet<ClipId>,
    anchor: ClipId,
) -> Result<Vec<&'a crate::arrangement::Clip>, KeyboardPlanError> {
    if !selected.contains(&anchor) {
        return Err(KeyboardPlanError::AnchorNotSelected(anchor));
    }
    selected_clips(state, selected)
}

fn snapped_boundary(
    proposed: Frame,
    selected: &BTreeSet<ClipId>,
    snap: Option<&SnapContext>,
) -> (Frame, Option<SnapResult>) {
    let resolved = snap.and_then(|context| context.resolve(proposed, selected));
    (resolved.map_or(proposed, |result| result.snapped), resolved)
}

fn phrase_plan_for_existing(
    expected_revision: u64,
    edits: Vec<PhraseClipEdit>,
    selected: &BTreeSet<ClipId>,
    primary: ClipId,
    ranges: &[FrameRange],
    snap: Option<SnapResult>,
) -> Result<PhraseEditPlan, KeyboardPlanError> {
    phrase_plan(
        expected_revision,
        edits,
        selected.clone(),
        Some(primary),
        covering_range(ranges)?,
        snap,
    )
}

fn phrase_plan(
    expected_revision: u64,
    edits: Vec<PhraseClipEdit>,
    clips: BTreeSet<ClipId>,
    primary: Option<ClipId>,
    range: FrameRange,
    snap: Option<SnapResult>,
) -> Result<PhraseEditPlan, KeyboardPlanError> {
    Ok(PhraseEditPlan {
        intent: ArrangementEditIntent {
            expected_revision,
            edit: ArrangementEdit::EditPhrase { edits },
        },
        selection: SelectionIntent::Clips {
            ids: clips.clone(),
            primary,
            mode: SelectionMode::Replace,
        },
        reveal: PhraseReveal {
            clips,
            primary,
            range,
        },
        snap,
    })
}

fn covering_range(ranges: &[FrameRange]) -> Result<FrameRange, KeyboardPlanError> {
    let start = ranges
        .iter()
        .map(|range| range.start)
        .min()
        .ok_or(KeyboardPlanError::EmptySelection)?;
    let end = ranges
        .iter()
        .map(|range| range.end)
        .max()
        .ok_or(KeyboardPlanError::EmptySelection)?;
    FrameRange::new(start, end).map_err(|_| KeyboardPlanError::TimeOverflow)
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

    fn overlapping_phrase() -> (ArrangementEditor, Vec<TrackId>, Vec<ClipId>) {
        let mut editor = ArrangementEditor::new(48_000).unwrap();
        let tracks = vec![
            editor.create_track("Audio 1", TrackKind::Audio).unwrap(),
            editor.create_track("Audio 2", TrackKind::Audio).unwrap(),
        ];
        let clips = vec![
            editor
                .create_audio_clip(
                    tracks[0],
                    "A",
                    FrameRange::new(Frame(100), Frame(300)).unwrap(),
                    AssetId::from_raw(1),
                    SourceRange::new(0, 200).unwrap(),
                )
                .unwrap(),
            editor
                .create_audio_clip(
                    tracks[1],
                    "B",
                    FrameRange::new(Frame(150), Frame(350)).unwrap(),
                    AssetId::from_raw(1),
                    SourceRange::new(200, 400).unwrap(),
                )
                .unwrap(),
        ];
        (editor, tracks, clips)
    }

    #[test]
    fn phrase_split_snaps_once_and_predicts_typed_right_selection() {
        use crate::arrangement_interaction::{SnapGuide, SnapGuideKind};

        let (editor, _, clips) = overlapping_phrase();
        let selected = BTreeSet::from_iter(clips.iter().copied());
        let next = editor.state().next_clip_id;
        let snap = SnapContext {
            grid_quantum: None,
            tolerance_frames: 3,
            guides: vec![SnapGuide {
                frame: Frame(200),
                kind: SnapGuideKind::Bar,
                key: 0,
            }],
        };
        let plan =
            plan_phrase_split(editor.state(), &selected, 22, Frame(198), Some(&snap)).unwrap();
        let ArrangementEdit::EditPhrase { edits } = &plan.intent.edit else {
            panic!("expected phrase edit")
        };
        assert_eq!(edits.len(), 2);
        assert!(edits.iter().all(|edit| matches!(
            edit,
            PhraseClipEdit::Split {
                boundary: Frame(200),
                ..
            }
        )));
        assert_eq!(
            plan.reveal.clips,
            BTreeSet::from([ClipId::from_raw(next), ClipId::from_raw(next + 1)])
        );
        assert_eq!(
            plan.reveal.range,
            FrameRange::new(Frame(200), Frame(350)).unwrap()
        );
        assert_eq!(plan.snap.unwrap().snapped, Frame(200));
    }

    #[test]
    fn phrase_trim_and_stretch_share_one_delta_across_tracks() {
        let (editor, _, clips) = overlapping_phrase();
        let selected = BTreeSet::from_iter(clips.iter().copied());
        let trim = plan_phrase_trim(
            editor.state(),
            &selected,
            23,
            clips[0],
            TrimEdge::Right,
            Frame(280),
            None,
        )
        .unwrap();
        let ArrangementEdit::EditPhrase { edits } = trim.intent.edit else {
            panic!("expected phrase edit")
        };
        assert_eq!(
            edits,
            vec![
                PhraseClipEdit::Trim {
                    clip_id: clips[0],
                    edge: TrimEdge::Right,
                    boundary: Frame(280),
                },
                PhraseClipEdit::Trim {
                    clip_id: clips[1],
                    edge: TrimEdge::Right,
                    boundary: Frame(330),
                },
            ]
        );

        let stretch = plan_phrase_stretch(
            editor.state(),
            &selected,
            24,
            clips[0],
            Frame(320),
            StretchAlgorithm::PhaseVocoder,
            true,
            None,
        )
        .unwrap();
        let ArrangementEdit::EditPhrase { edits } = stretch.intent.edit else {
            panic!("expected phrase edit")
        };
        assert!(matches!(
            edits[1],
            PhraseClipEdit::Stretch {
                boundary: Frame(370),
                ..
            }
        ));
    }

    #[test]
    fn locked_track_refuses_phrase_before_it_can_emit_members() {
        let (mut editor, tracks, clips) = overlapping_phrase();
        let before = editor.state().track(tracks[1]).unwrap().clone();
        let mut after = before.clone();
        after.locked = true;
        editor
            .apply(ArrangementTransaction::new(
                "Lock track",
                vec![ArrangementOperation::PutTrack {
                    before: Some(before),
                    after: Some(after),
                }],
            ))
            .unwrap();
        assert_eq!(
            plan_phrase_fade(
                editor.state(),
                &BTreeSet::from_iter(clips.iter().copied()),
                25,
                clips[0],
                FadeEdge::In,
                Frame(120),
                None,
            ),
            Err(KeyboardPlanError::LockedTrack(tracks[1]))
        );
    }
}
