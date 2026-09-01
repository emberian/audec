//! DAW-surface policy shared by arrangement views and controllers.
//!
//! This module refuses to own GPUI entities, transport playback, project
//! mutation, or decoded PCM. It makes the otherwise easy-to-drift interaction
//! contracts explicit: object selection, time selection, and looping are
//! independent; musical grids come from the authoritative tempo map; stretch
//! is not trim; and a continuous gesture has one typed coalescing identity.

use std::collections::BTreeSet;

use crate::arrangement::{
    ArrangementState, ClipContent, ClipId, Frame, FrameRange, StretchAlgorithm, TrackId,
};
use crate::arrangement_interaction::{
    ArrangementEdit, ArrangementEditIntent, SelectionIntent, SelectionMode, SnapContext, SnapGuide,
    SnapGuideKind,
};
use crate::command_record::{CoalesceToken, CommandAddress};
use crate::sequencer::{BeatTime, ProjectFrame, TempoMap, PPQ};
use crate::waveform_proxy::{
    plan_clip_waveform, ClipWaveformSpec, PixelTarget, WaveformProxyError, WaveformProxyPlan,
};

/// The three timeline selections a DAW surface must not conflate.
///
/// `objects` answers which clips an edit targets. `time` is a range chosen on
/// the ruler/canvas and may exist with no clips selected. `loop_range` is a
/// transport setting copied from a time selection only by an explicit command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArrangementSelections {
    pub objects: BTreeSet<ClipId>,
    pub primary: Option<ClipId>,
    pub tracks: BTreeSet<TrackId>,
    pub time: Option<FrameRange>,
    pub loop_range: Option<FrameRange>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimelineSelectionEdit {
    SetTime(FrameRange),
    ClearTime,
    SetLoop(FrameRange),
    SetLoopFromTime,
    ClearLoop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionRefusal {
    MissingClip(ClipId),
    NoTimeSelection,
}

impl ArrangementSelections {
    pub fn apply_objects(
        &mut self,
        state: &ArrangementState,
        intent: SelectionIntent,
    ) -> Result<(), SelectionRefusal> {
        match intent {
            SelectionIntent::Clips { ids, primary, mode } => {
                if let Some(missing) = ids.iter().find(|id| state.clip(**id).is_none()) {
                    return Err(SelectionRefusal::MissingClip(*missing));
                }
                apply_set(&mut self.objects, ids, mode);
                self.primary = primary
                    .filter(|id| self.objects.contains(id))
                    .or_else(|| self.objects.iter().next().copied());
            }
            SelectionIntent::Marquee {
                range,
                tracks,
                mode,
            } => {
                let ids = state
                    .clips
                    .values()
                    .filter(|clip| {
                        (tracks.is_empty() || tracks.contains(&clip.track_id))
                            && clip.placement.intersects(range)
                    })
                    .map(|clip| clip.id)
                    .collect();
                apply_set(&mut self.objects, ids, mode);
                self.primary = self
                    .primary
                    .filter(|id| self.objects.contains(id))
                    .or_else(|| self.objects.iter().next().copied());
            }
            SelectionIntent::ClearObjects => {
                self.objects.clear();
                self.primary = None;
            }
        }
        self.tracks = self
            .objects
            .iter()
            .filter_map(|id| state.clip(*id).map(|clip| clip.track_id))
            .collect();
        Ok(())
    }

    pub fn apply_timeline(&mut self, edit: TimelineSelectionEdit) -> Result<(), SelectionRefusal> {
        match edit {
            TimelineSelectionEdit::SetTime(range) => self.time = Some(range),
            TimelineSelectionEdit::ClearTime => self.time = None,
            TimelineSelectionEdit::SetLoop(range) => self.loop_range = Some(range),
            TimelineSelectionEdit::SetLoopFromTime => {
                self.loop_range = Some(self.time.ok_or(SelectionRefusal::NoTimeSelection)?);
            }
            TimelineSelectionEdit::ClearLoop => self.loop_range = None,
        }
        Ok(())
    }

    /// Drop deleted identities while retaining independent time and loop
    /// ranges. Reconciliation must never silently derive either range from the
    /// surviving object selection.
    pub fn reconcile(&mut self, state: &ArrangementState) {
        self.objects.retain(|id| state.clip(*id).is_some());
        self.primary = self
            .primary
            .filter(|id| self.objects.contains(id))
            .or_else(|| self.objects.iter().next().copied());
        self.tracks = self
            .objects
            .iter()
            .filter_map(|id| state.clip(*id).map(|clip| clip.track_id))
            .collect();
    }
}

fn apply_set(target: &mut BTreeSet<ClipId>, ids: BTreeSet<ClipId>, mode: SelectionMode) {
    match mode {
        SelectionMode::Replace => *target = ids,
        SelectionMode::Add => target.extend(ids),
        SelectionMode::Toggle => {
            for id in ids {
                if !target.remove(&id) {
                    target.insert(id);
                }
            }
        }
    }
}

/// Musical ruler density. The grid is evaluated through [`TempoMap`] rather
/// than a fixed BPM approximation, so tempo and meter changes stay aligned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MusicalGridResolution {
    Bar,
    Beat,
    Quarter,
    Eighth,
    Sixteenth,
    ThirtySecond,
    EighthTriplet,
    SixteenthTriplet,
    Ticks(u32),
}

impl MusicalGridResolution {
    pub fn tick_quantum(self) -> Option<i64> {
        let ticks = match self {
            Self::Bar | Self::Beat | Self::Quarter => PPQ,
            Self::Eighth => PPQ / 2,
            Self::Sixteenth => PPQ / 4,
            Self::ThirtySecond => PPQ / 8,
            Self::EighthTriplet => PPQ / 3,
            Self::SixteenthTriplet => PPQ / 6,
            Self::Ticks(ticks) => i64::from(ticks),
        };
        (ticks > 0).then_some(ticks)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MusicalGridLine {
    pub frame: Frame,
    pub tick: BeatTime,
    pub bar: i64,
    pub beat: u16,
    pub tick_in_beat: u16,
    pub kind: SnapGuideKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MusicalGridPlan {
    pub lines: Vec<MusicalGridLine>,
    pub snap: SnapContext,
    pub truncated: bool,
}

pub const DEFAULT_GRID_LINE_LIMIT: usize = 16_384;

pub fn plan_musical_grid(
    tempo: &TempoMap,
    visible: FrameRange,
    resolution: MusicalGridResolution,
    tolerance_frames: u64,
    line_limit: usize,
) -> MusicalGridPlan {
    let quantum = resolution.tick_quantum().unwrap_or(PPQ).max(1);
    let first_tick = tempo
        .frame_to_beat_floor(ProjectFrame(visible.start.0))
        .0
        .div_euclid(quantum)
        .saturating_mul(quantum);
    let last_tick = tempo
        .frame_to_beat_floor(ProjectFrame(visible.end.0))
        .0
        .saturating_add(quantum);
    let limit = line_limit.max(1);
    let mut lines = Vec::new();
    let mut tick = first_tick;
    let mut truncated = false;
    while tick <= last_tick {
        if lines.len() == limit {
            truncated = true;
            break;
        }
        let beat_time = BeatTime(tick);
        let frame = Frame(tempo.beat_to_frame(beat_time).0);
        if frame >= visible.start && frame <= visible.end {
            let position = tempo.musical_position(beat_time);
            let kind = if position.beat == 0 && position.tick == 0 {
                SnapGuideKind::Bar
            } else if position.tick == 0 {
                SnapGuideKind::Beat
            } else {
                SnapGuideKind::Grid
            };
            lines.push(MusicalGridLine {
                frame,
                tick: beat_time,
                bar: position.bar,
                beat: position.beat,
                tick_in_beat: position.tick,
                kind,
            });
        }
        let next = tick.saturating_add(quantum);
        if next == tick {
            truncated = true;
            break;
        }
        tick = next;
    }
    let guides = lines
        .iter()
        .map(|line| SnapGuide {
            frame: line.frame,
            kind: line.kind,
            key: signed_key(line.tick.0),
        })
        .collect();
    MusicalGridPlan {
        lines,
        snap: SnapContext {
            grid_quantum: None,
            tolerance_frames,
            guides,
        },
        truncated,
    }
}

fn signed_key(value: i64) -> u64 {
    (value as u64) ^ (1_u64 << 63)
}

/// Typed identity shared by all updates in one continuous editor gesture.
/// A new pointer-down or inspector scrub allocates a fresh `series`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArrangementGestureIdentity {
    pub editor_session: u64,
    pub series: u64,
    pub primary_clip: ClipId,
}

impl ArrangementGestureIdentity {
    pub fn coalesce_token(self) -> CoalesceToken {
        CoalesceToken {
            editor_session: self.editor_session,
            gesture_kind: self.series,
            primary: CommandAddress::ArrangementClip(self.primary_clip),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrangementPublicationPhase {
    Begin,
    Update,
    Finish,
    Cancel,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArrangementEditPublication {
    pub gesture: ArrangementGestureIdentity,
    pub phase: ArrangementPublicationPhase,
    pub intent: Option<ArrangementEditIntent>,
}

impl ArrangementEditPublication {
    pub fn update(
        gesture: ArrangementGestureIdentity,
        expected_revision: u64,
        edit: ArrangementEdit,
    ) -> Self {
        Self {
            gesture,
            phase: ArrangementPublicationPhase::Update,
            intent: Some(ArrangementEditIntent {
                expected_revision,
                edit,
            }),
        }
    }

    pub fn boundary(
        gesture: ArrangementGestureIdentity,
        phase: ArrangementPublicationPhase,
    ) -> Self {
        debug_assert!(matches!(
            phase,
            ArrangementPublicationPhase::Begin
                | ArrangementPublicationPhase::Finish
                | ArrangementPublicationPhase::Cancel
        ));
        Self {
            gesture,
            phase,
            intent: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StretchRefusal {
    MissingClip(ClipId),
    MissingTrack(TrackId),
    LockedClip(ClipId),
    LockedTrack(TrackId),
    NotAudio(ClipId),
    InvalidBoundary,
    WarpMarkersRequireCompiler(ClipId),
}

/// Plan a stretch edit without confusing it with a trim. The source range is
/// deliberately absent from the result because the lowering layer reads and
/// validates it against the same revision used to apply the command.
pub fn plan_stretch(
    state: &ArrangementState,
    expected_revision: u64,
    clip_id: ClipId,
    boundary: Frame,
    algorithm: StretchAlgorithm,
    preserve_pitch: bool,
) -> Result<ArrangementEditIntent, StretchRefusal> {
    let clip = state
        .clip(clip_id)
        .ok_or(StretchRefusal::MissingClip(clip_id))?;
    if clip.locked {
        return Err(StretchRefusal::LockedClip(clip_id));
    }
    let track = state
        .track(clip.track_id)
        .ok_or(StretchRefusal::MissingTrack(clip.track_id))?;
    if track.locked {
        return Err(StretchRefusal::LockedTrack(track.id));
    }
    if boundary <= clip.placement.start {
        return Err(StretchRefusal::InvalidBoundary);
    }
    let ClipContent::Audio(audio) = &clip.content else {
        return Err(StretchRefusal::NotAudio(clip_id));
    };
    if !audio.playback.warp_markers.is_empty() {
        return Err(StretchRefusal::WarpMarkersRequireCompiler(clip_id));
    }
    Ok(ArrangementEditIntent {
        expected_revision,
        edit: ArrangementEdit::StretchClip {
            clip_id,
            boundary,
            algorithm,
            preserve_pitch,
        },
    })
}

/// A viewport-native waveform pass. Replanning on every viewport or display
/// scale change is part of the contract; callers cache numeric results by the
/// returned content/playback/pixel key, never by clip ID alone.
pub fn plan_visible_waveforms<'a>(
    specs: impl IntoIterator<Item = &'a ClipWaveformSpec>,
    viewport: FrameRange,
    logical_width: f64,
    scale_factor: f64,
) -> Vec<(ClipId, Result<WaveformProxyPlan, WaveformProxyError>)> {
    let target = PixelTarget::new(logical_width, scale_factor);
    specs
        .into_iter()
        .map(|spec| {
            let plan = target
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|pixels| plan_clip_waveform(spec, viewport, *pixels));
            (spec.clip, plan)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrangement::{
        ArrangementEditor, ArrangementOperation, ArrangementTransaction, SourceRange, TrackKind,
    };
    use crate::sequencer::Tempo;

    fn arrangement() -> (ArrangementEditor, TrackId, ClipId, ClipId) {
        let mut editor = ArrangementEditor::new(48_000).unwrap();
        let track = editor.create_track("Audio", TrackKind::Audio).unwrap();
        let first = editor
            .create_audio_clip(
                track,
                "A",
                FrameRange::new(Frame(0), Frame(48_000)).unwrap(),
                crate::arrangement::AssetId::from_raw(1),
                SourceRange::new(0, 48_000).unwrap(),
            )
            .unwrap();
        let second = editor
            .create_audio_clip(
                track,
                "B",
                FrameRange::new(Frame(72_000), Frame(96_000)).unwrap(),
                crate::arrangement::AssetId::from_raw(1),
                SourceRange::new(48_000, 72_000).unwrap(),
            )
            .unwrap();
        (editor, track, first, second)
    }

    #[test]
    fn object_time_and_loop_selection_are_independent() {
        let (editor, _, first, second) = arrangement();
        let mut selections = ArrangementSelections::default();
        let time = FrameRange::new(Frame(10), Frame(20)).unwrap();
        selections
            .apply_timeline(TimelineSelectionEdit::SetTime(time))
            .unwrap();
        selections
            .apply_objects(
                editor.state(),
                SelectionIntent::Clips {
                    ids: BTreeSet::from([first, second]),
                    primary: Some(second),
                    mode: SelectionMode::Replace,
                },
            )
            .unwrap();
        assert_eq!(selections.time, Some(time));
        assert_eq!(selections.loop_range, None);
        selections
            .apply_timeline(TimelineSelectionEdit::SetLoopFromTime)
            .unwrap();
        selections
            .apply_objects(editor.state(), SelectionIntent::ClearObjects)
            .unwrap();
        assert!(selections.objects.is_empty());
        assert_eq!(selections.time, Some(time));
        assert_eq!(selections.loop_range, Some(time));
    }

    #[test]
    fn grid_follows_tempo_changes_instead_of_assuming_one_bpm() {
        let mut tempo = TempoMap::common_time(48_000, 120.0).unwrap();
        tempo
            .set_tempo(BeatTime(4 * PPQ), Tempo::from_bpm(60.0).unwrap())
            .unwrap();
        let plan = plan_musical_grid(
            &tempo,
            FrameRange::new(Frame(0), Frame(192_001)).unwrap(),
            MusicalGridResolution::Quarter,
            100,
            DEFAULT_GRID_LINE_LIMIT,
        );
        let frames: Vec<_> = plan.lines.iter().map(|line| line.frame.0).collect();
        assert!(frames.starts_with(&[0, 24_000, 48_000, 72_000, 96_000]));
        assert!(frames.contains(&144_000));
        assert!(!plan.truncated);
        assert_eq!(plan.lines[0].kind, SnapGuideKind::Bar);
    }

    #[test]
    fn stretch_is_typed_and_gesture_identity_is_stable() {
        let (editor, _, first, _) = arrangement();
        let intent = plan_stretch(
            editor.state(),
            41,
            first,
            Frame(96_000),
            StretchAlgorithm::PhaseVocoder,
            true,
        )
        .unwrap();
        assert!(matches!(
            intent.edit,
            ArrangementEdit::StretchClip {
                boundary: Frame(96_000),
                algorithm: StretchAlgorithm::PhaseVocoder,
                preserve_pitch: true,
                ..
            }
        ));
        let gesture = ArrangementGestureIdentity {
            editor_session: 7,
            series: 99,
            primary_clip: first,
        };
        assert_eq!(gesture.coalesce_token(), gesture.coalesce_token());
        assert_ne!(
            gesture.coalesce_token(),
            ArrangementGestureIdentity {
                series: 100,
                ..gesture
            }
            .coalesce_token()
        );
    }

    #[test]
    fn surface_stretch_refuses_a_locked_track_before_emitting_an_intent() {
        let (mut editor, track, first, _) = arrangement();
        let before = editor.state().track(track).unwrap().clone();
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
            plan_stretch(
                editor.state(),
                42,
                first,
                Frame(96_000),
                StretchAlgorithm::PhaseVocoder,
                true,
            ),
            Err(StretchRefusal::LockedTrack(track))
        );
    }
}
