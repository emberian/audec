//! Authoritative lowering from arrangement interaction terms to project commands.
//!
//! This module owns no GPUI state and mutates no live editor mirror. It reads
//! one immutable [`LiveProjectSnapshot`], builds put-style aggregate commands,
//! and dry-applies the envelope to a cloned `DawProject` so the returned
//! change set is the exact one the command kernel derives. Selection and
//! transport remain ephemeral; typed IDs cross domains only through explicit
//! binding commands.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use crate::arrangement::{
    self, AudioLoopMode, AudioRegion, ChannelMapping, Clip, ClipContent, ClipFades, ClipId, Fade,
    Frame, FrameRange, OverlapPolicy, PatternRegion, PlaybackTransform, SourceRange, StretchRatio,
    Track, TrackId, TrackKind,
};
use crate::arrangement_interaction::{ArrangementEdit, GestureCommit, TrimEdge};
use crate::arrangement_view::{ArrangementAction, ArrangementActionIntent, ArrangementViewEvent};
use crate::assets::{
    AssetFrameRange, AssetId as MediaAssetId, AssetRegistry, AssetUsage, AssetUsageOwner,
};
use crate::command::{
    claims_for_commands, AssetCommand, BindingCommand, ChangeSet, CommandEnvelope, DomainCommand,
    EnvelopeError,
};
use crate::command_record::CommandAddress;
use crate::daw_project::ProjectState;
use crate::live_project::LiveProjectSnapshot;
use crate::mixer::{BusKind, MixerCommand};
use crate::project_session::{ProjectSession, ProjectSessionError};
use crate::sequencer::{
    self, BeatDuration, BeatTime, PatternClip, PatternClipId, PatternId, ProjectFrame,
    SequencerCommand,
};
use crate::ui_drag::{AssetDrag, DropIntent};

/// A view event lowered without smuggling ephemeral UI state into the project.
#[derive(Clone, Debug, PartialEq)]
pub enum ArrangementDispatch {
    Apply(ValidatedArrangementEnvelope),
    History(ArrangementHistoryIntent),
    SelectionOnly,
    Seek(Frame),
}

/// Result of routing one arrangement event through the authoritative project
/// session.  Transport and selection remain explicit effects because neither
/// belongs in the durable command journal.
#[derive(Clone, Debug)]
pub enum ArrangementExecution {
    ProjectChanged(crate::daw_project::ProjectRevisions),
    HistoryUnchanged(ArrangementHistoryKind),
    SelectionOnly,
    Seek(Frame),
}

#[derive(Debug)]
pub enum ArrangementExecutionError {
    Lowering(ArrangementLoweringError),
    Session(ProjectSessionError),
}

impl fmt::Display for ArrangementExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lowering(error) => write!(formatter, "arrangement command refused: {error}"),
            Self::Session(error) => write!(formatter, "arrangement publication failed: {error}"),
        }
    }
}

impl Error for ArrangementExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Lowering(error) => Some(error),
            Self::Session(error) => Some(error),
        }
    }
}

/// Lower, execute, and publish one view event through [`ProjectSession`].
///
/// Keeping this outside GPUI is important: the session remains the only owner
/// which mirrors controller publications into its event stream, so a view can
/// never accidentally execute against `ProjectController` while leaving the
/// session snapshot stale.
pub fn execute_arrangement_event(
    session: &mut ProjectSession,
    event: ArrangementViewEvent,
) -> Result<ArrangementExecution, ArrangementExecutionError> {
    let dispatch = lower_arrangement_event(
        session
            .project_snapshot()
            .map_err(ArrangementExecutionError::Session)?,
        event,
    )
    .map_err(ArrangementExecutionError::Lowering)?;
    match dispatch {
        ArrangementDispatch::Apply(validated) => session
            .execute(validated.envelope)
            .map(ArrangementExecution::ProjectChanged)
            .map_err(ArrangementExecutionError::Session),
        ArrangementDispatch::History(history) => {
            let revision = match history.kind {
                ArrangementHistoryKind::Undo => session.undo(),
                ArrangementHistoryKind::Redo => session.redo(),
            }
            .map_err(ArrangementExecutionError::Session)?;
            Ok(revision.map_or(
                ArrangementExecution::HistoryUnchanged(history.kind),
                ArrangementExecution::ProjectChanged,
            ))
        }
        ArrangementDispatch::SelectionOnly => Ok(ArrangementExecution::SelectionOnly),
        ArrangementDispatch::Seek(frame) => Ok(ArrangementExecution::Seek(frame)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrangementHistoryKind {
    Undo,
    Redo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArrangementHistoryIntent {
    pub expected_revision: u64,
    pub kind: ArrangementHistoryKind,
}

/// A command already proven against the immutable source publication.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedArrangementEnvelope {
    pub envelope: CommandEnvelope,
    pub change_set: ChangeSet,
    pub addresses: BTreeSet<CommandAddress>,
}

#[derive(Debug)]
pub enum ArrangementLoweringError {
    RevisionConflict { expected: u64, actual: u64 },
    MissingTrack(TrackId),
    MissingClip(ClipId),
    MissingMedia(MediaAssetId),
    MissingPattern(PatternId),
    MissingBinding(&'static str),
    LockedTrack(TrackId),
    LockedClip(ClipId),
    IncompatibleTrack { track: TrackId, kind: TrackKind },
    StaleGesture(String),
    InvalidEdit(String),
    UnsupportedDrop(&'static str),
    NonRepresentablePatternFrame(Frame),
    IdentityExhausted(&'static str),
    Domain(String),
    Envelope(EnvelopeError),
}

impl fmt::Display for ArrangementLoweringError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RevisionConflict { expected, actual } => {
                write!(
                    formatter,
                    "arrangement revision conflict: expected {expected}, actual {actual}"
                )
            }
            Self::MissingTrack(id) => write!(formatter, "arrangement track {id} does not exist"),
            Self::MissingClip(id) => write!(formatter, "arrangement clip {id} does not exist"),
            Self::MissingMedia(id) => write!(formatter, "media asset {id:?} does not exist"),
            Self::MissingPattern(id) => {
                write!(formatter, "sequencer pattern {id:?} does not exist")
            }
            Self::MissingBinding(kind) => write!(formatter, "missing required {kind} binding"),
            Self::LockedTrack(id) => write!(formatter, "arrangement track {id} is locked"),
            Self::LockedClip(id) => write!(formatter, "arrangement clip {id} is locked"),
            Self::IncompatibleTrack { track, kind } => {
                write!(formatter, "track {track} cannot contain {kind:?} content")
            }
            Self::StaleGesture(detail) => write!(formatter, "stale arrangement gesture: {detail}"),
            Self::InvalidEdit(detail) => write!(formatter, "invalid arrangement edit: {detail}"),
            Self::UnsupportedDrop(kind) => write!(formatter, "{kind} is not an arrangement drop"),
            Self::NonRepresentablePatternFrame(frame) => write!(
                formatter,
                "project frame {} is not exactly representable on the sequencer timeline",
                frame.0
            ),
            Self::IdentityExhausted(kind) => write!(formatter, "{kind} identity space exhausted"),
            Self::Domain(detail) => formatter.write_str(detail),
            Self::Envelope(error) => error.fmt(formatter),
        }
    }
}

impl Error for ArrangementLoweringError {}

/// Lower one arrangement event against one coherent publication.
pub fn lower_arrangement_event(
    snapshot: &LiveProjectSnapshot,
    event: ArrangementViewEvent,
) -> Result<ArrangementDispatch, ArrangementLoweringError> {
    match event {
        ArrangementViewEvent::SeekRequested(frame) => Ok(ArrangementDispatch::Seek(frame)),
        ArrangementViewEvent::Commit(commit) => lower_gesture(snapshot, commit),
        ArrangementViewEvent::Action(intent) => lower_action(snapshot, intent),
    }
}

pub fn lower_gesture(
    snapshot: &LiveProjectSnapshot,
    commit: GestureCommit,
) -> Result<ArrangementDispatch, ArrangementLoweringError> {
    let Some(intent) = commit.edit else {
        return Ok(ArrangementDispatch::SelectionOnly);
    };
    require_revision(snapshot, intent.expected_revision)?;
    let mut builder = ArrangementBuilder::new(snapshot);
    let label = builder.lower_edit(intent.edit)?;
    builder.finish(label).map(ArrangementDispatch::Apply)
}

pub fn lower_action(
    snapshot: &LiveProjectSnapshot,
    intent: ArrangementActionIntent,
) -> Result<ArrangementDispatch, ArrangementLoweringError> {
    require_revision(snapshot, intent.expected_revision)?;
    match intent.action {
        ArrangementAction::Undo => Ok(ArrangementDispatch::History(ArrangementHistoryIntent {
            expected_revision: intent.expected_revision,
            kind: ArrangementHistoryKind::Undo,
        })),
        ArrangementAction::Redo => Ok(ArrangementDispatch::History(ArrangementHistoryIntent {
            expected_revision: intent.expected_revision,
            kind: ArrangementHistoryKind::Redo,
        })),
        action => {
            let mut builder = ArrangementBuilder::new(snapshot);
            let label = builder.lower_action(action)?;
            builder.finish(label).map(ArrangementDispatch::Apply)
        }
    }
}

fn require_revision(
    snapshot: &LiveProjectSnapshot,
    expected: u64,
) -> Result<(), ArrangementLoweringError> {
    let actual = snapshot.revisions().aggregate;
    if expected == actual {
        Ok(())
    } else {
        Err(ArrangementLoweringError::RevisionConflict { expected, actual })
    }
}

struct ArrangementBuilder<'a> {
    snapshot: &'a LiveProjectSnapshot,
    commands: Vec<DomainCommand>,
    next_track: u64,
    next_clip: u64,
    next_media_alias: u64,
    next_pattern_alias: u64,
    sequencer_allocator: sequencer::Sequencer,
    asset_scratch: AssetRegistry,
}

impl<'a> ArrangementBuilder<'a> {
    fn new(snapshot: &'a LiveProjectSnapshot) -> Self {
        let state = snapshot.project.state();
        let cursors = state.bindings.allocator_state();
        Self {
            snapshot,
            commands: Vec::new(),
            next_track: state.domains.arrangement.next_track_id,
            next_clip: state.domains.arrangement.next_clip_id,
            next_media_alias: cursors.next_arrangement_asset,
            next_pattern_alias: cursors.next_arrangement_pattern,
            sequencer_allocator: state.domains.sequencer.clone(),
            asset_scratch: state.domains.assets.clone(),
        }
    }

    fn state(&self) -> &ProjectState {
        self.snapshot.project.state()
    }

    fn finish(
        self,
        label: impl Into<String>,
    ) -> Result<ValidatedArrangementEnvelope, ArrangementLoweringError> {
        let label = label.into();
        if self.commands.is_empty() {
            return Err(ArrangementLoweringError::InvalidEdit(
                "the edit produced no project commands".into(),
            ));
        }
        let addresses = self
            .commands
            .iter()
            .flat_map(DomainCommand::addresses)
            .collect::<BTreeSet<_>>();
        let envelope = CommandEnvelope {
            label,
            base_revision: self.snapshot.revisions().aggregate,
            // The view emits once at pointer-up. Refusing cross-event
            // coalescing preserves exactly one aggregate undo entry per
            // completed gesture.
            coalesce: None,
            id_claims: claims_for_commands(&self.commands),
            commands: self.commands,
        };
        let mut candidate = (*self.snapshot.project).clone();
        let applied = envelope
            .clone()
            .apply(&mut candidate)
            .map_err(ArrangementLoweringError::Envelope)?;
        Ok(ValidatedArrangementEnvelope {
            envelope,
            change_set: applied.change_set,
            addresses,
        })
    }

    fn lower_action(
        &mut self,
        action: ArrangementAction,
    ) -> Result<String, ArrangementLoweringError> {
        match action {
            ArrangementAction::Undo | ArrangementAction::Redo => unreachable!("handled above"),
            ArrangementAction::DeleteClips(clips) => {
                if clips.is_empty() {
                    return Err(ArrangementLoweringError::InvalidEdit(
                        "delete selection is empty".into(),
                    ));
                }
                for clip in clips.iter().copied() {
                    self.delete_clip(clip)?;
                }
                Ok(format!(
                    "Delete {} clip{}",
                    clips.len(),
                    plural(clips.len())
                ))
            }
            ArrangementAction::SplitClip { clip, at } => {
                self.split_clip(clip, at)?;
                Ok("Split clip".into())
            }
            ArrangementAction::CreateTrack { kind } => {
                self.create_track(kind, format!("{} track", track_kind_name(kind)))?;
                Ok(format!("Create {} track", track_kind_name(kind)))
            }
            ArrangementAction::Drop(drop) => self.lower_drop(drop),
        }
    }

    fn lower_edit(&mut self, edit: ArrangementEdit) -> Result<String, ArrangementLoweringError> {
        match edit {
            ArrangementEdit::MoveClips { moves, duplicate } => {
                if moves.is_empty() {
                    return Err(ArrangementLoweringError::InvalidEdit(
                        "clip move is empty".into(),
                    ));
                }
                for movement in &moves {
                    let before = self.editable_clip(movement.clip_id)?.clone();
                    if before.track_id != movement.from_track || before.placement != movement.from {
                        return Err(ArrangementLoweringError::StaleGesture(format!(
                            "clip {} no longer matches its drag baseline",
                            movement.clip_id
                        )));
                    }
                    self.require_compatible_track(movement.to_track, before.content.kind())?;
                    let mut after = before.clone();
                    after.track_id = movement.to_track;
                    after.placement = movement.to;
                    if duplicate {
                        after.id = self.allocate_clip()?;
                        self.commands.push(DomainCommand::Arrangement(
                            arrangement::ArrangementOperation::PutClip {
                                before: None,
                                after: Some(after.clone()),
                            },
                        ));
                        self.duplicate_links(&before, &after)?;
                    } else {
                        self.commands.push(DomainCommand::Arrangement(
                            arrangement::ArrangementOperation::PutClip {
                                before: Some(before.clone()),
                                after: Some(after.clone()),
                            },
                        ));
                        self.sync_existing_pattern_clip(&before, &after)?;
                    }
                }
                Ok(format!(
                    "{} {} clip{}",
                    if duplicate { "Duplicate" } else { "Move" },
                    moves.len(),
                    plural(moves.len())
                ))
            }
            ArrangementEdit::TrimClip {
                clip_id,
                edge,
                boundary,
            } => {
                self.trim_clip(clip_id, edge, boundary)?;
                Ok(match edge {
                    TrimEdge::Left => "Trim clip left",
                    TrimEdge::Right => "Trim clip right",
                }
                .into())
            }
            ArrangementEdit::SlipClip {
                clip_id,
                project_delta,
            } => {
                let before = self.editable_clip(clip_id)?.clone();
                let mut after = before.clone();
                slip_content(&mut after.content, project_delta)?;
                self.put_clip(before.clone(), after.clone());
                self.sync_existing_pattern_clip(&before, &after)?;
                self.update_audio_usage(&after)?;
                Ok("Slip clip".into())
            }
            ArrangementEdit::StretchClip {
                clip_id,
                boundary,
                algorithm,
                preserve_pitch,
            } => {
                let before = self.editable_clip(clip_id)?.clone();
                if boundary <= before.placement.start {
                    return Err(ArrangementLoweringError::InvalidEdit(
                        "stretch boundary must follow the clip start".into(),
                    ));
                }
                let mut after = before.clone();
                after.placement.end = boundary;
                let ClipContent::Audio(audio) = &mut after.content else {
                    return Err(ArrangementLoweringError::InvalidEdit(
                        "only audio clips have stretch semantics".into(),
                    ));
                };
                if !audio.playback.warp_markers.is_empty() {
                    return Err(ArrangementLoweringError::InvalidEdit(
                        "warp-marker stretch requires the piecewise mapping compiler".into(),
                    ));
                }
                audio.playback.ratio =
                    StretchRatio::new(audio.source.len(), after.placement.len()).map_err(domain)?;
                audio.playback.algorithm = algorithm;
                audio.playback.preserve_pitch = preserve_pitch;
                after.fades = clamp_fades_to_length(after.fades, after.placement.len());
                self.put_clip(before, after.clone());
                self.update_audio_usage(&after)?;
                Ok("Stretch clip".into())
            }
            ArrangementEdit::SetClipFades { clip_id, fades } => {
                let before = self.editable_clip(clip_id)?.clone();
                let mut after = before.clone();
                after.fades = fades;
                self.put_clip(before, after);
                Ok("Set clip fades".into())
            }
            ArrangementEdit::SetRepeatBoundary { clip_id, boundary } => {
                self.set_repeat_boundary(clip_id, boundary)?;
                Ok("Set repeat boundary".into())
            }
        }
    }

    fn lower_drop(&mut self, drop: DropIntent) -> Result<String, ArrangementLoweringError> {
        match drop {
            DropIntent::InsertAudio { source, track, at } => {
                self.insert_audio(source, track, at)?;
                Ok("Insert audio".into())
            }
            DropIntent::InsertPattern {
                pattern,
                track,
                at,
                make_unique,
            } => {
                self.insert_pattern(pattern, track, at, make_unique)?;
                Ok(if make_unique {
                    "Insert unique pattern"
                } else {
                    "Insert pattern"
                }
                .into())
            }
            DropIntent::MoveArrangementClips {
                clips,
                original_anchor,
                target_track,
                target_anchor,
                duplicate,
                ..
            } => {
                if clips.is_empty() {
                    return Err(ArrangementLoweringError::InvalidEdit(
                        "clip drop is empty".into(),
                    ));
                }
                let delta = target_anchor
                    .0
                    .checked_sub(original_anchor.0)
                    .ok_or_else(|| ArrangementLoweringError::InvalidEdit("move overflow".into()))?;
                let mut moves = Vec::with_capacity(clips.len());
                for id in clips {
                    let clip = self.editable_clip(id)?;
                    let to_track = target_track.unwrap_or(clip.track_id);
                    moves.push(crate::arrangement_interaction::ClipMove {
                        clip_id: id,
                        from_track: clip.track_id,
                        to_track,
                        from: clip.placement,
                        to: clip.placement.translated(delta).map_err(domain)?,
                    });
                }
                self.lower_edit(ArrangementEdit::MoveClips { moves, duplicate })
            }
            DropIntent::MapAssetToStepPattern { .. } => Err(
                ArrangementLoweringError::UnsupportedDrop("step-pattern mapping"),
            ),
            DropIntent::MapAssetToPad { .. } => Err(ArrangementLoweringError::UnsupportedDrop(
                "sampler-pad mapping",
            )),
            DropIntent::AddPatternToLibrary { .. } => Err(
                ArrangementLoweringError::UnsupportedDrop("pattern-library insertion"),
            ),
            DropIntent::PreviewAspectDeprojection { .. } => Err(
                ArrangementLoweringError::UnsupportedDrop("aspect deprojection preview"),
            ),
            DropIntent::PreviewReconstruction { .. } => Err(
                ArrangementLoweringError::UnsupportedDrop("reconstruction preview"),
            ),
            DropIntent::RouteBus { .. } => {
                Err(ArrangementLoweringError::UnsupportedDrop("mixer routing"))
            }
        }
    }

    fn insert_audio(
        &mut self,
        source: AssetDrag,
        track: Option<TrackId>,
        at: Frame,
    ) -> Result<ClipId, ArrangementLoweringError> {
        let media = self
            .state()
            .domains
            .assets
            .get(source.asset)
            .cloned()
            .ok_or(ArrangementLoweringError::MissingMedia(source.asset))?;
        let source_range = source.source_range.unwrap_or(AssetFrameRange {
            start: crate::assets::SampleFrames(0),
            end: media.metadata().frame_count,
        });
        if !source_range.is_within(media.metadata().frame_count) {
            return Err(ArrangementLoweringError::InvalidEdit(
                "audio drop range exceeds the decoded asset".into(),
            ));
        }
        let project_len = project_frame_count(
            source_range.len().0,
            media.metadata().sample_rate_hz,
            self.state().domains.arrangement.sample_rate,
        )?;
        let track = match track {
            Some(track) => {
                self.require_compatible_track(track, TrackKind::Audio)?;
                track
            }
            None => self.create_track(TrackKind::Audio, "Audio")?,
        };
        let alias = self.media_alias(source.asset)?;
        let id = self.allocate_clip()?;
        let placement = FrameRange::from_start_and_len(at, project_len).map_err(domain)?;
        let source = SourceRange::new(source_range.start.0, source_range.end.0).map_err(domain)?;
        let clip = Clip {
            id,
            track_id: track,
            name: media.name().to_owned(),
            placement,
            content: ClipContent::Audio(AudioRegion {
                asset: alias,
                source,
                playback: PlaybackTransform {
                    ratio: StretchRatio::new(source.len(), placement.len()).map_err(domain)?,
                    ..PlaybackTransform::default()
                },
                channels: ChannelMapping::All,
                loop_mode: AudioLoopMode::Off,
            }),
            fades: ClipFades::default(),
            gain_db: 0.0,
            muted: false,
            locked: false,
        };
        self.commands.push(DomainCommand::Arrangement(
            arrangement::ArrangementOperation::PutClip {
                before: None,
                after: Some(clip.clone()),
            },
        ));
        self.add_audio_usage(&clip)?;
        Ok(id)
    }

    fn insert_pattern(
        &mut self,
        source_pattern: PatternId,
        track: Option<TrackId>,
        at: Frame,
        make_unique: bool,
    ) -> Result<ClipId, ArrangementLoweringError> {
        let source = self
            .state()
            .domains
            .sequencer
            .patterns()
            .get(source_pattern)
            .cloned()
            .ok_or(ArrangementLoweringError::MissingPattern(source_pattern))?;
        let pattern = if make_unique {
            let mut copy = source.clone();
            copy.id = self.sequencer_allocator.allocate_pattern_id();
            copy.name = format!("{} copy", copy.name);
            copy.revision = 0;
            self.commands
                .push(DomainCommand::Sequencer(SequencerCommand::PutPattern {
                    before: None,
                    after: Some(copy.clone()),
                }));
            copy
        } else {
            source
        };
        let start = exact_beat(self.state(), at)?;
        let end_beat = BeatTime(
            start
                .0
                .checked_add(pattern.length.0 as i64)
                .ok_or(ArrangementLoweringError::IdentityExhausted("pattern time"))?,
        );
        let end = Frame(
            self.state()
                .domains
                .sequencer
                .tempo_map()
                .beat_to_frame(end_beat)
                .0,
        );
        let placement = FrameRange::new(at, end).map_err(domain)?;
        let track = match track {
            Some(track) => {
                self.require_compatible_track(track, TrackKind::Pattern)?;
                track
            }
            None => self.create_track(TrackKind::Pattern, "Pattern")?,
        };
        let alias = self.pattern_alias(pattern.id)?;
        let arrangement_clip = self.allocate_clip()?;
        let clip = Clip {
            id: arrangement_clip,
            track_id: track,
            name: pattern.name.clone(),
            placement,
            content: ClipContent::Pattern(PatternRegion {
                pattern: alias,
                content_offset_frames: 0,
                looped: false,
            }),
            fades: ClipFades::default(),
            gain_db: 0.0,
            muted: false,
            locked: false,
        };
        let sequence_clip = PatternClip {
            id: self.sequencer_allocator.allocate_clip_id(),
            pattern: pattern.id,
            start,
            length: pattern.length,
            pattern_offset: BeatTime(0),
            looped: false,
            transpose_semitones: 0.0,
            gain: 1.0,
            muted: false,
        };
        self.commands.push(DomainCommand::Arrangement(
            arrangement::ArrangementOperation::PutClip {
                before: None,
                after: Some(clip),
            },
        ));
        self.commands
            .push(DomainCommand::Sequencer(SequencerCommand::PutClip {
                before: None,
                after: Some(sequence_clip.clone()),
            }));
        self.commands.push(DomainCommand::Bindings(
            BindingCommand::PutPatternPlacement {
                clip: arrangement_clip,
                before: None,
                after: Some(sequence_clip.id),
            },
        ));
        Ok(arrangement_clip)
    }

    fn create_track(
        &mut self,
        kind: TrackKind,
        name: impl Into<String>,
    ) -> Result<TrackId, ArrangementLoweringError> {
        if matches!(kind, TrackKind::Group) {
            return Err(ArrangementLoweringError::InvalidEdit(
                "arrangement group tracks require an explicit mixer group policy".into(),
            ));
        }
        let id = self.allocate_track()?;
        let name = name.into();
        let track = Track {
            id,
            name: name.clone(),
            kind,
            overlap: OverlapPolicy::Mix,
            clip_ids: Vec::new(),
            muted: false,
            solo: false,
            locked: false,
            gain_db: 0.0,
            pan: 0.0,
        };
        let before_mixer = self.state().domains.mixer.clone();
        let mut after_mixer = before_mixer.clone();
        let bus = after_mixer.add_bus(BusKind::Source, name).map_err(domain)?;
        let mixer_command = MixerCommand::build("Create track bus", &before_mixer, move |graph| {
            *graph = after_mixer;
            Ok(())
        })
        .map_err(domain)?;
        self.commands.push(DomainCommand::Mixer(mixer_command));
        self.commands.push(DomainCommand::Arrangement(
            arrangement::ArrangementOperation::PutTrack {
                before: None,
                after: Some(track),
            },
        ));
        self.commands
            .push(DomainCommand::Bindings(BindingCommand::PutTrackBus {
                track: id,
                before: None,
                after: Some(bus),
            }));
        Ok(id)
    }

    fn trim_clip(
        &mut self,
        clip_id: ClipId,
        edge: TrimEdge,
        boundary: Frame,
    ) -> Result<(), ArrangementLoweringError> {
        let before = self.editable_clip(clip_id)?.clone();
        let mut after = before.clone();
        match edge {
            TrimEdge::Left => {
                if boundary < before.placement.start || boundary >= before.placement.end {
                    return Err(ArrangementLoweringError::InvalidEdit(
                        "left trim boundary is outside the clip".into(),
                    ));
                }
                let removed = boundary.0.saturating_sub(before.placement.start.0) as u64;
                after.placement.start = boundary;
                advance_content(&mut after.content, removed)?;
                after.fades = trim_fades_left(before.fades, before.placement.len(), removed);
            }
            TrimEdge::Right => {
                if boundary <= before.placement.start || boundary > before.placement.end {
                    return Err(ArrangementLoweringError::InvalidEdit(
                        "right trim boundary is outside the clip".into(),
                    ));
                }
                let removed = before.placement.end.0.saturating_sub(boundary.0) as u64;
                after.placement.end = boundary;
                retreat_content_end(&mut after.content, removed)?;
                after.fades = trim_fades_right(before.fades, before.placement.len(), removed);
            }
        }
        self.put_clip(before.clone(), after.clone());
        self.sync_existing_pattern_clip(&before, &after)?;
        self.update_audio_usage(&after)?;
        Ok(())
    }

    fn split_clip(&mut self, id: ClipId, at: Frame) -> Result<ClipId, ArrangementLoweringError> {
        let before = self.editable_clip(id)?.clone();
        if !before.placement.contains(at) || at == before.placement.start {
            return Err(ArrangementLoweringError::InvalidEdit(
                "split boundary is outside the clip interior".into(),
            ));
        }
        let left_len = at.0.saturating_sub(before.placement.start.0) as u64;
        let (left_content, right_content) = split_content(&before.content, left_len)?;
        let (left_fades, right_fades) = split_fades(before.fades, before.placement.len(), left_len);
        let mut left = before.clone();
        left.placement.end = at;
        left.content = left_content;
        left.fades = left_fades;
        let mut right = before.clone();
        right.id = self.allocate_clip()?;
        right.placement.start = at;
        right.content = right_content;
        right.fades = right_fades;
        self.put_clip(before.clone(), left.clone());
        self.commands.push(DomainCommand::Arrangement(
            arrangement::ArrangementOperation::PutClip {
                before: None,
                after: Some(right.clone()),
            },
        ));
        self.sync_existing_pattern_clip(&before, &left)?;
        self.duplicate_pattern_placement(&right)?;
        self.update_audio_usage(&left)?;
        self.add_audio_usage(&right)?;
        Ok(right.id)
    }

    fn set_repeat_boundary(
        &mut self,
        id: ClipId,
        boundary: Frame,
    ) -> Result<(), ArrangementLoweringError> {
        let before = self.editable_clip(id)?.clone();
        if boundary <= before.placement.start {
            return Err(ArrangementLoweringError::InvalidEdit(
                "repeat boundary must follow the clip start".into(),
            ));
        }
        let mut after = before.clone();
        after.placement.end = boundary;
        match &mut after.content {
            ClipContent::Pattern(region) => region.looped = true,
            ClipContent::Automation(region) => region.looped = true,
            ClipContent::Audio(_) => {
                return Err(ArrangementLoweringError::InvalidEdit(
                    "audio repeat requires a loop-aware placement representation".into(),
                ));
            }
        }
        self.put_clip(before.clone(), after.clone());
        self.sync_existing_pattern_clip(&before, &after)?;
        Ok(())
    }

    fn delete_clip(&mut self, id: ClipId) -> Result<(), ArrangementLoweringError> {
        let clip = self.editable_clip(id)?.clone();
        if let Some(sequence) = self.state().bindings.patterns.placements.get(&id).copied() {
            let before = self
                .state()
                .domains
                .sequencer
                .clip(sequence)
                .cloned()
                .ok_or(ArrangementLoweringError::MissingBinding(
                    "pattern placement",
                ))?;
            self.commands
                .push(DomainCommand::Sequencer(SequencerCommand::PutClip {
                    before: Some(before),
                    after: None,
                }));
            self.commands.push(DomainCommand::Bindings(
                BindingCommand::PutPatternPlacement {
                    clip: id,
                    before: Some(sequence),
                    after: None,
                },
            ));
        }
        if let Some(bus) = self.state().bindings.mixer.clip_overrides.get(&id).copied() {
            self.commands.push(DomainCommand::Bindings(
                BindingCommand::PutClipBusOverride {
                    clip: id,
                    before: Some(bus),
                    after: None,
                },
            ));
        }
        if let Some(object) = self.state().bindings.air.clips.get(&id).copied() {
            self.commands
                .push(DomainCommand::Bindings(BindingCommand::PutClipObjectLink {
                    clip: id,
                    before: Some(object),
                    after: None,
                }));
        }
        self.remove_audio_usage(&clip)?;
        self.commands.push(DomainCommand::Arrangement(
            arrangement::ArrangementOperation::PutClip {
                before: Some(clip),
                after: None,
            },
        ));
        Ok(())
    }

    fn duplicate_links(
        &mut self,
        source: &Clip,
        duplicate: &Clip,
    ) -> Result<(), ArrangementLoweringError> {
        if let Some(bus) = self
            .state()
            .bindings
            .mixer
            .clip_overrides
            .get(&source.id)
            .copied()
        {
            self.commands.push(DomainCommand::Bindings(
                BindingCommand::PutClipBusOverride {
                    clip: duplicate.id,
                    before: None,
                    after: Some(bus),
                },
            ));
        }
        // Analytic object links deliberately do not duplicate: an authored
        // copy is not new evidence for the same source identity.
        self.duplicate_pattern_placement(duplicate)?;
        self.add_audio_usage(duplicate)
    }

    fn sync_existing_pattern_clip(
        &mut self,
        before: &Clip,
        after: &Clip,
    ) -> Result<(), ArrangementLoweringError> {
        let ClipContent::Pattern(_) = &before.content else {
            return Ok(());
        };
        let sequence_id = self
            .state()
            .bindings
            .patterns
            .placements
            .get(&before.id)
            .copied()
            .ok_or(ArrangementLoweringError::MissingBinding(
                "pattern placement",
            ))?;
        let sequence_before = self
            .state()
            .domains
            .sequencer
            .clip(sequence_id)
            .cloned()
            .ok_or(ArrangementLoweringError::MissingBinding("sequencer clip"))?;
        let sequence_after = pattern_clip_for_arrangement(
            self.state(),
            after,
            sequence_id,
            sequence_before.transpose_semitones,
            sequence_before.gain,
            sequence_before.muted,
        )?;
        self.commands
            .push(DomainCommand::Sequencer(SequencerCommand::PutClip {
                before: Some(sequence_before),
                after: Some(sequence_after),
            }));
        Ok(())
    }

    fn duplicate_pattern_placement(&mut self, clip: &Clip) -> Result<(), ArrangementLoweringError> {
        if !matches!(clip.content, ClipContent::Pattern(_)) {
            return Ok(());
        }
        let id = self.sequencer_allocator.allocate_clip_id();
        let sequence = pattern_clip_for_arrangement(self.state(), clip, id, 0.0, 1.0, false)?;
        self.commands
            .push(DomainCommand::Sequencer(SequencerCommand::PutClip {
                before: None,
                after: Some(sequence),
            }));
        self.commands.push(DomainCommand::Bindings(
            BindingCommand::PutPatternPlacement {
                clip: clip.id,
                before: None,
                after: Some(id),
            },
        ));
        Ok(())
    }

    fn media_alias(
        &mut self,
        media: MediaAssetId,
    ) -> Result<arrangement::AssetId, ArrangementLoweringError> {
        if let Some((alias, _)) = self
            .state()
            .bindings
            .assets
            .arrangement_assets
            .iter()
            .find(|(_, candidate)| **candidate == media)
        {
            return Ok(*alias);
        }
        let alias = arrangement::AssetId::from_raw(take_id(
            &mut self.next_media_alias,
            "arrangement media alias",
        )?);
        self.commands.push(DomainCommand::Bindings(
            BindingCommand::PutMediaAssetAlias {
                alias,
                before: None,
                after: Some(media),
            },
        ));
        Ok(alias)
    }

    fn pattern_alias(
        &mut self,
        pattern: PatternId,
    ) -> Result<arrangement::PatternId, ArrangementLoweringError> {
        if let Some((alias, _)) = self
            .state()
            .bindings
            .patterns
            .definitions
            .iter()
            .find(|(_, candidate)| **candidate == pattern)
        {
            return Ok(*alias);
        }
        let alias = arrangement::PatternId::from_raw(take_id(
            &mut self.next_pattern_alias,
            "arrangement pattern alias",
        )?);
        self.commands.push(DomainCommand::Bindings(
            BindingCommand::PutPatternDefinitionAlias {
                alias,
                before: None,
                after: Some(pattern),
            },
        ));
        Ok(alias)
    }

    fn add_audio_usage(&mut self, clip: &Clip) -> Result<(), ArrangementLoweringError> {
        let ClipContent::Audio(audio) = &clip.content else {
            return Ok(());
        };
        let media = self
            .state()
            .bindings
            .assets
            .arrangement_assets
            .get(&audio.asset)
            .copied()
            .or_else(|| {
                self.commands.iter().find_map(|command| match command {
                    DomainCommand::Bindings(BindingCommand::PutMediaAssetAlias {
                        alias,
                        after: Some(media),
                        ..
                    }) if *alias == audio.asset => Some(*media),
                    _ => None,
                })
            })
            .ok_or(ArrangementLoweringError::MissingBinding("media alias"))?;
        let range = AssetFrameRange::new(
            crate::assets::SampleFrames(audio.source.start),
            crate::assets::SampleFrames(audio.source.end),
        )
        .map_err(domain)?;
        let usage = self
            .asset_scratch
            .add_usage(
                media,
                AssetUsageOwner::AudioClip {
                    persistent_id: clip.id.get(),
                },
                Some(range),
                clip.name.clone(),
            )
            .map_err(domain)?;
        let after = self.asset_scratch.get(media).unwrap().usages()[&usage].clone();
        self.commands
            .push(DomainCommand::Assets(AssetCommand::PutUsage {
                asset: media,
                usage,
                before: None,
                after: Some(after),
            }));
        Ok(())
    }

    fn update_audio_usage(&mut self, clip: &Clip) -> Result<(), ArrangementLoweringError> {
        let ClipContent::Audio(audio) = &clip.content else {
            return Ok(());
        };
        let media = self.media_for_audio(audio)?;
        let Some((usage, before)) = find_audio_usage(&self.asset_scratch, media, clip.id) else {
            return Ok(());
        };
        let mut after = before.clone();
        after.source_range = Some(
            AssetFrameRange::new(
                crate::assets::SampleFrames(audio.source.start),
                crate::assets::SampleFrames(audio.source.end),
            )
            .map_err(domain)?,
        );
        self.asset_scratch
            .put_usage(media, usage, Some(&before), Some(after.clone()))
            .map_err(domain)?;
        self.commands
            .push(DomainCommand::Assets(AssetCommand::PutUsage {
                asset: media,
                usage,
                before: Some(before),
                after: Some(after),
            }));
        Ok(())
    }

    fn remove_audio_usage(&mut self, clip: &Clip) -> Result<(), ArrangementLoweringError> {
        let ClipContent::Audio(audio) = &clip.content else {
            return Ok(());
        };
        let media = self.media_for_audio(audio)?;
        let Some((usage, before)) = find_audio_usage(&self.asset_scratch, media, clip.id) else {
            return Ok(());
        };
        self.asset_scratch
            .put_usage(media, usage, Some(&before), None)
            .map_err(domain)?;
        self.commands
            .push(DomainCommand::Assets(AssetCommand::PutUsage {
                asset: media,
                usage,
                before: Some(before),
                after: None,
            }));
        Ok(())
    }

    fn media_for_audio(
        &self,
        audio: &AudioRegion,
    ) -> Result<MediaAssetId, ArrangementLoweringError> {
        self.state()
            .bindings
            .assets
            .arrangement_assets
            .get(&audio.asset)
            .copied()
            .ok_or(ArrangementLoweringError::MissingBinding("media alias"))
    }

    fn require_compatible_track(
        &self,
        id: TrackId,
        content: TrackKind,
    ) -> Result<&Track, ArrangementLoweringError> {
        let track = self
            .state()
            .domains
            .arrangement
            .track(id)
            .ok_or(ArrangementLoweringError::MissingTrack(id))?;
        if track.locked {
            return Err(ArrangementLoweringError::LockedTrack(id));
        }
        if track.kind != TrackKind::Hybrid && track.kind != content {
            return Err(ArrangementLoweringError::IncompatibleTrack {
                track: id,
                kind: content,
            });
        }
        Ok(track)
    }

    fn editable_clip(&self, id: ClipId) -> Result<&Clip, ArrangementLoweringError> {
        let clip = self
            .state()
            .domains
            .arrangement
            .clip(id)
            .ok_or(ArrangementLoweringError::MissingClip(id))?;
        if clip.locked {
            return Err(ArrangementLoweringError::LockedClip(id));
        }
        self.require_compatible_track(clip.track_id, clip.content.kind())?;
        Ok(clip)
    }

    fn put_clip(&mut self, before: Clip, after: Clip) {
        self.commands.push(DomainCommand::Arrangement(
            arrangement::ArrangementOperation::PutClip {
                before: Some(before),
                after: Some(after),
            },
        ));
    }

    fn allocate_track(&mut self) -> Result<TrackId, ArrangementLoweringError> {
        Ok(TrackId::from_raw(take_id(
            &mut self.next_track,
            "arrangement track",
        )?))
    }

    fn allocate_clip(&mut self) -> Result<ClipId, ArrangementLoweringError> {
        Ok(ClipId::from_raw(take_id(
            &mut self.next_clip,
            "arrangement clip",
        )?))
    }
}

fn clamp_fades_to_length(mut fades: ClipFades, length: u64) -> ClipFades {
    if let Some(fade) = &mut fades.fade_in {
        fade.duration = fade.duration.min(length);
    }
    if let Some(fade) = &mut fades.fade_out {
        fade.duration = fade.duration.min(length);
    }
    fades
}

fn take_id(next: &mut u64, kind: &'static str) -> Result<u64, ArrangementLoweringError> {
    let id = *next;
    if id == 0 {
        return Err(ArrangementLoweringError::IdentityExhausted(kind));
    }
    *next = next
        .checked_add(1)
        .ok_or(ArrangementLoweringError::IdentityExhausted(kind))?;
    Ok(id)
}

fn project_frame_count(
    source_frames: u64,
    source_rate: u32,
    project_rate: u32,
) -> Result<u64, ArrangementLoweringError> {
    if source_frames == 0 || source_rate == 0 || project_rate == 0 {
        return Err(ArrangementLoweringError::InvalidEdit(
            "audio duration metadata is empty".into(),
        ));
    }
    let numerator = u128::from(source_frames) * u128::from(project_rate);
    let denominator = u128::from(source_rate);
    u64::try_from(numerator / denominator + u128::from(numerator % denominator != 0))
        .ok()
        .filter(|frames| *frames > 0)
        .ok_or(ArrangementLoweringError::IdentityExhausted("project frame"))
}

fn exact_beat(state: &ProjectState, frame: Frame) -> Result<BeatTime, ArrangementLoweringError> {
    let map = state.domains.sequencer.tempo_map();
    let beat = map.frame_to_beat_floor(ProjectFrame(frame.0));
    if map.beat_to_frame(beat).0 == frame.0 {
        Ok(beat)
    } else {
        Err(ArrangementLoweringError::NonRepresentablePatternFrame(
            frame,
        ))
    }
}

fn pattern_clip_for_arrangement(
    state: &ProjectState,
    clip: &Clip,
    id: PatternClipId,
    transpose_semitones: f32,
    gain: f32,
    muted: bool,
) -> Result<PatternClip, ArrangementLoweringError> {
    let ClipContent::Pattern(region) = &clip.content else {
        return Err(ArrangementLoweringError::InvalidEdit(
            "pattern placement requested for non-pattern clip".into(),
        ));
    };
    let pattern = state
        .bindings
        .patterns
        .definitions
        .get(&region.pattern)
        .copied()
        .ok_or(ArrangementLoweringError::MissingBinding(
            "pattern definition",
        ))?;
    let start = exact_beat(state, clip.placement.start)?;
    let end = exact_beat(state, clip.placement.end)?;
    let length = end
        .0
        .checked_sub(start.0)
        .and_then(|ticks| u64::try_from(ticks).ok())
        .filter(|ticks| *ticks > 0)
        .ok_or_else(|| ArrangementLoweringError::InvalidEdit("empty pattern placement".into()))?;
    let offset_frame = Frame(
        i64::try_from(region.content_offset_frames)
            .map_err(|_| ArrangementLoweringError::IdentityExhausted("pattern offset"))?,
    );
    let pattern_offset = exact_beat(state, offset_frame)?;
    Ok(PatternClip {
        id,
        pattern,
        start,
        length: BeatDuration(length),
        pattern_offset,
        looped: region.looped,
        transpose_semitones,
        gain,
        muted,
    })
}

fn find_audio_usage(
    registry: &AssetRegistry,
    asset: MediaAssetId,
    clip: ClipId,
) -> Option<(crate::assets::AssetUsageId, AssetUsage)> {
    registry
        .get(asset)?
        .usages()
        .iter()
        .find_map(|(id, usage)| {
            matches!(
                usage.owner,
                AssetUsageOwner::AudioClip { persistent_id } if persistent_id == clip.get()
            )
            .then(|| (*id, usage.clone()))
        })
}

fn advance_content(
    content: &mut ClipContent,
    project_delta: u64,
) -> Result<(), ArrangementLoweringError> {
    match content {
        ClipContent::Audio(audio) => {
            reject_warped(audio)?;
            let source_delta = audio
                .playback
                .ratio
                .source_offset(project_delta)
                .map_err(domain)?;
            if source_delta >= audio.source.len() {
                return Err(ArrangementLoweringError::InvalidEdit(
                    "trim removes the complete source".into(),
                ));
            }
            if audio.playback.reverse {
                audio.source.end = audio.source.end.checked_sub(source_delta).ok_or_else(|| {
                    ArrangementLoweringError::InvalidEdit("source underflow".into())
                })?;
            } else {
                audio.source.start =
                    audio
                        .source
                        .start
                        .checked_add(source_delta)
                        .ok_or_else(|| {
                            ArrangementLoweringError::InvalidEdit("source overflow".into())
                        })?;
            }
        }
        ClipContent::Pattern(region) => {
            region.content_offset_frames = region
                .content_offset_frames
                .checked_add(project_delta)
                .ok_or(ArrangementLoweringError::IdentityExhausted(
                    "pattern offset",
                ))?;
        }
        ClipContent::Automation(region) => {
            region.content_offset_frames = region
                .content_offset_frames
                .checked_add(project_delta)
                .ok_or(ArrangementLoweringError::IdentityExhausted(
                    "automation offset",
                ))?;
        }
    }
    Ok(())
}

fn retreat_content_end(
    content: &mut ClipContent,
    project_delta: u64,
) -> Result<(), ArrangementLoweringError> {
    if let ClipContent::Audio(audio) = content {
        reject_warped(audio)?;
        let source_delta = audio
            .playback
            .ratio
            .source_offset(project_delta)
            .map_err(domain)?;
        if source_delta >= audio.source.len() {
            return Err(ArrangementLoweringError::InvalidEdit(
                "trim removes the complete source".into(),
            ));
        }
        if audio.playback.reverse {
            audio.source.start = audio
                .source
                .start
                .checked_add(source_delta)
                .ok_or_else(|| ArrangementLoweringError::InvalidEdit("source overflow".into()))?;
        } else {
            audio.source.end =
                audio.source.end.checked_sub(source_delta).ok_or_else(|| {
                    ArrangementLoweringError::InvalidEdit("source underflow".into())
                })?;
        }
    }
    Ok(())
}

fn slip_content(
    content: &mut ClipContent,
    project_delta: i64,
) -> Result<(), ArrangementLoweringError> {
    match content {
        ClipContent::Audio(audio) => {
            reject_warped(audio)?;
            let magnitude = audio
                .playback
                .ratio
                .source_offset(project_delta.unsigned_abs())
                .map_err(domain)?;
            let signed = i128::from(magnitude) * if project_delta < 0 { -1 } else { 1 };
            let start = i128::from(audio.source.start) + signed;
            let end = i128::from(audio.source.end) + signed;
            if start < 0 || end > i128::from(u64::MAX) {
                return Err(ArrangementLoweringError::InvalidEdit(
                    "slip leaves the source address space".into(),
                ));
            }
            audio.source.start = start as u64;
            audio.source.end = end as u64;
        }
        ClipContent::Pattern(region) => {
            region.content_offset_frames = add_signed(region.content_offset_frames, project_delta)?;
        }
        ClipContent::Automation(region) => {
            region.content_offset_frames = add_signed(region.content_offset_frames, project_delta)?;
        }
    }
    Ok(())
}

fn split_content(
    content: &ClipContent,
    project_offset: u64,
) -> Result<(ClipContent, ClipContent), ArrangementLoweringError> {
    let mut left = content.clone();
    let mut right = content.clone();
    match (content, &mut left, &mut right) {
        (ClipContent::Audio(original), ClipContent::Audio(left), ClipContent::Audio(right)) => {
            reject_warped(original)?;
            let source_offset = original
                .playback
                .ratio
                .source_offset(project_offset)
                .map_err(domain)?;
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
        _ => unreachable!("cloned content variants remain identical"),
    }
    Ok((left, right))
}

fn reject_warped(audio: &AudioRegion) -> Result<(), ArrangementLoweringError> {
    if audio.playback.warp_markers.is_empty() {
        Ok(())
    } else {
        Err(ArrangementLoweringError::InvalidEdit(
            "warped audio requires the compiled edit path".into(),
        ))
    }
}

fn add_signed(value: u64, delta: i64) -> Result<u64, ArrangementLoweringError> {
    let value = i128::from(value) + i128::from(delta);
    u64::try_from(value).map_err(|_| {
        ArrangementLoweringError::InvalidEdit("content offset leaves its address space".into())
    })
}

fn split_fades(fades: ClipFades, total: u64, left_len: u64) -> (ClipFades, ClipFades) {
    let right_len = total - left_len;
    let mut left = ClipFades::default();
    let mut right = ClipFades::default();
    if let Some(fade) = fades.fade_in {
        if left_len >= fade.duration {
            left.fade_in = Some(fade);
        } else {
            let phase = lerp(
                fade.phase_start,
                fade.phase_end,
                left_len as f64 / fade.duration as f64,
            );
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
            let phase = lerp(
                fade.phase_start,
                fade.phase_end,
                elapsed as f64 / fade.duration as f64,
            );
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

fn lerp(left: f64, right: f64, amount: f64) -> f64 {
    left + (right - left) * amount
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

fn track_kind_name(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Audio => "Audio",
        TrackKind::Pattern => "Pattern",
        TrackKind::Automation => "Automation",
        TrackKind::Hybrid => "Hybrid",
        TrackKind::Group => "Group",
    }
}

fn domain(error: impl fmt::Display) -> ArrangementLoweringError {
    ArrangementLoweringError::Domain(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use crate::arrangement_interaction::ArrangementEditIntent;
    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        ContentFingerprint, DecodedAudioMetadata, SampleFrames,
    };
    use crate::audio::AudioFormat;
    use crate::command::IdClaims;
    use crate::daw_render::PcmAsset;
    use crate::live_project::{LiveProject, ProjectController, SourceMaterialMetadata};
    use crate::project_session::ProjectSessionId;

    const RATE: u32 = 48_000;
    const FRAMES: u64 = 16;

    fn live_source() -> LiveProject {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/fixture/arrangement-source.wav").unwrap()),
            None,
        )
        .unwrap();
        let mut registry = AssetRegistry::new();
        let asset = registry
            .register(AssetRegistration {
                name: "arrangement source".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: RATE,
                    channels: 1,
                    frame_count: SampleFrames(FRAMES),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32le".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"arrangement-authority-fixture"),
                provenance: AssetProvenance::new(
                    1,
                    AssetOrigin::ImportedFile {
                        importer: "arrangement-authority-test".into(),
                    },
                    location,
                ),
                tags: BTreeSet::new(),
                favorite: false,
            })
            .unwrap();
        let pcm = PcmAsset::new(
            AudioFormat::new(RATE, 1).unwrap(),
            Arc::from(
                (0..FRAMES)
                    .map(|frame| frame as f32 / FRAMES as f32)
                    .collect::<Vec<_>>(),
            ),
        )
        .unwrap();
        LiveProject::from_source_material(
            SourceMaterialMetadata::new("Arrangement authority", "Source"),
            registry,
            asset,
            pcm,
        )
        .unwrap()
    }

    fn expect_apply(dispatch: ArrangementDispatch) -> ValidatedArrangementEnvelope {
        match dispatch {
            ArrangementDispatch::Apply(validated) => validated,
            other => panic!("expected aggregate apply dispatch, got {other:?}"),
        }
    }

    fn snapshot_with_reverse(snapshot: &LiveProjectSnapshot, clip: ClipId) -> LiveProjectSnapshot {
        let mut project = (*snapshot.project).clone();
        let before = project
            .state()
            .domains
            .arrangement
            .clip(clip)
            .unwrap()
            .clone();
        let mut after = before.clone();
        let ClipContent::Audio(audio) = &mut after.content else {
            panic!("fixture source is audio")
        };
        audio.playback.reverse = true;
        CommandEnvelope {
            label: "Reverse fixture".into(),
            base_revision: project.revisions().aggregate,
            coalesce: None,
            commands: vec![DomainCommand::Arrangement(
                arrangement::ArrangementOperation::PutClip {
                    before: Some(before),
                    after: Some(after),
                },
            )],
            id_claims: IdClaims::new(),
        }
        .apply(&mut project)
        .unwrap();
        LiveProjectSnapshot {
            project: Arc::new(project),
            pcm: snapshot.pcm.clone(),
            sample_pcm: snapshot.sample_pcm.clone(),
        }
    }

    #[test]
    fn stale_revision_is_rejected_before_identity_allocation() {
        let live = live_source();
        let snapshot = live.snapshot().unwrap();
        let actual = snapshot.revisions().aggregate;
        let error = lower_action(
            &snapshot,
            ArrangementActionIntent {
                expected_revision: actual + 1,
                action: ArrangementAction::CreateTrack {
                    kind: TrackKind::Audio,
                },
            },
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ArrangementLoweringError::RevisionConflict {
                expected,
                actual: found
            } if expected == actual + 1 && found == actual
        ));
        assert_eq!(snapshot.project.state().domains.arrangement.tracks.len(), 1);
    }

    #[test]
    fn canvas_audio_drop_creates_one_validated_cross_domain_envelope() {
        let live = live_source();
        let source = live.source_ids();
        let snapshot = live.snapshot().unwrap();
        let before_tracks = snapshot.project.state().domains.arrangement.tracks.len();
        let before_clips = snapshot.project.state().domains.arrangement.clips.len();
        let before_buses = snapshot.project.state().domains.mixer.buses().count();

        let validated = expect_apply(
            lower_action(
                &snapshot,
                ArrangementActionIntent {
                    expected_revision: snapshot.revisions().aggregate,
                    action: ArrangementAction::Drop(DropIntent::InsertAudio {
                        source: AssetDrag {
                            asset: source.registry_asset,
                            source_range: Some(
                                AssetFrameRange::new(SampleFrames(2), SampleFrames(10)).unwrap(),
                            ),
                        },
                        track: None,
                        at: Frame(24),
                    }),
                },
            )
            .unwrap(),
        );

        assert_eq!(validated.envelope.coalesce, None);
        assert_eq!(
            validated.envelope.id_claims,
            claims_for_commands(&validated.envelope.commands)
        );
        assert_eq!(
            validated.addresses,
            validated
                .envelope
                .commands
                .iter()
                .flat_map(DomainCommand::addresses)
                .collect::<BTreeSet<_>>()
        );

        let mut project = (*snapshot.project).clone();
        let applied = validated.envelope.apply(&mut project).unwrap();
        assert_eq!(applied.change_set, validated.change_set);
        let state = project.state();
        assert_eq!(state.domains.arrangement.tracks.len(), before_tracks + 1);
        assert_eq!(state.domains.arrangement.clips.len(), before_clips + 1);
        assert_eq!(state.domains.mixer.buses().count(), before_buses + 1);
        let inserted = state
            .domains
            .arrangement
            .clips
            .values()
            .find(|clip| clip.id != source.clip)
            .unwrap();
        assert_eq!(
            inserted.placement,
            FrameRange::new(Frame(24), Frame(32)).unwrap()
        );
        let ClipContent::Audio(audio) = &inserted.content else {
            panic!("audio drop must remain audio")
        };
        assert_eq!(audio.source, SourceRange::new(2, 10).unwrap());
        assert_eq!(
            state.bindings.assets.arrangement_assets.get(&audio.asset),
            Some(&source.registry_asset)
        );
        assert!(state.bindings.mixer.tracks.contains_key(&inserted.track_id));
    }

    #[test]
    fn reverse_left_trim_updates_opposite_source_edge_and_usage() {
        let live = live_source();
        let source = live.source_ids();
        let initial = live.snapshot().unwrap();
        let snapshot = snapshot_with_reverse(&initial, source.clip);

        let validated = expect_apply(
            lower_gesture(
                &snapshot,
                GestureCommit {
                    selection: None,
                    edit: Some(ArrangementEditIntent {
                        expected_revision: snapshot.revisions().aggregate,
                        edit: ArrangementEdit::TrimClip {
                            clip_id: source.clip,
                            edge: TrimEdge::Left,
                            boundary: Frame(3),
                        },
                    }),
                },
            )
            .unwrap(),
        );
        let mut project = (*snapshot.project).clone();
        validated.envelope.apply(&mut project).unwrap();
        let clip = project
            .state()
            .domains
            .arrangement
            .clip(source.clip)
            .unwrap();
        assert_eq!(
            clip.placement,
            FrameRange::new(Frame(3), Frame(16)).unwrap()
        );
        let ClipContent::Audio(audio) = &clip.content else {
            panic!("fixture source is audio")
        };
        assert_eq!(audio.source, SourceRange::new(0, 13).unwrap());
        let usage = project
            .state()
            .domains
            .assets
            .get(source.registry_asset)
            .unwrap()
            .usages()
            .values()
            .find(|usage| {
                matches!(
                    usage.owner,
                    AssetUsageOwner::AudioClip { persistent_id }
                        if persistent_id == source.clip.get()
                )
            })
            .unwrap();
        assert_eq!(
            usage.source_range,
            Some(AssetFrameRange::new(SampleFrames(0), SampleFrames(13)).unwrap())
        );
    }

    #[test]
    fn one_drop_is_one_controller_undo_and_inverse_replay_restores_entities() {
        let live = live_source();
        let source = live.source_ids();
        let mut controller = ProjectController::new(live).unwrap();
        let original = controller.snapshot().clone();
        let validated = expect_apply(
            lower_action(
                controller.snapshot(),
                ArrangementActionIntent {
                    expected_revision: controller.revisions().aggregate,
                    action: ArrangementAction::Drop(DropIntent::InsertAudio {
                        source: AssetDrag {
                            asset: source.registry_asset,
                            source_range: None,
                        },
                        track: None,
                        at: Frame(32),
                    }),
                },
            )
            .unwrap(),
        );

        controller.execute(validated.envelope).unwrap();
        assert!(controller.can_undo());
        assert_eq!(controller.undo_label(), Some("Insert audio"));
        assert_eq!(
            controller
                .snapshot()
                .project
                .state()
                .domains
                .arrangement
                .clips
                .len(),
            2
        );
        controller
            .undo()
            .unwrap()
            .expect("the one gesture is undoable");
        assert!(!controller.can_undo());
        assert!(controller.can_redo());

        let restored = controller.snapshot().project.state();
        let before = original.project.state();
        assert_eq!(
            restored.domains.arrangement.tracks,
            before.domains.arrangement.tracks
        );
        assert_eq!(
            restored.domains.arrangement.clips,
            before.domains.arrangement.clips
        );
        assert_eq!(
            restored.domains.assets.assets(),
            before.domains.assets.assets()
        );
        assert_ne!(
            restored.domains.assets, before.domains.assets,
            "asset usage allocator high-water marks must not rewind on undo"
        );
        assert_eq!(restored.bindings, before.bindings);
        assert_eq!(
            restored.domains.mixer.buses().cloned().collect::<Vec<_>>(),
            before.domains.mixer.buses().cloned().collect::<Vec<_>>()
        );
        // Allocator high-water marks and domain revisions intentionally do
        // not rewind; redo must retain the same claimed identities.
        controller
            .redo()
            .unwrap()
            .expect("inverse replay remains redoable");
        assert_eq!(
            controller
                .snapshot()
                .project
                .state()
                .domains
                .arrangement
                .clips
                .len(),
            2
        );
    }

    #[test]
    fn execution_helper_publishes_back_through_project_session() {
        let mut session = ProjectSession::new(ProjectSessionId(17)).unwrap();
        session.install(live_source(), None).unwrap();
        let before_generation = session.snapshot().generation;
        let before_revision = session.project_snapshot().unwrap().revisions().aggregate;

        let execution = execute_arrangement_event(
            &mut session,
            ArrangementViewEvent::Action(ArrangementActionIntent {
                expected_revision: before_revision,
                action: ArrangementAction::CreateTrack {
                    kind: TrackKind::Pattern,
                },
            }),
        )
        .unwrap();

        let ArrangementExecution::ProjectChanged(revisions) = execution else {
            panic!("track creation must publish a project revision")
        };
        assert_eq!(revisions.aggregate, before_revision + 1);
        assert!(session.snapshot().generation > before_generation);
        assert_eq!(
            session
                .project_snapshot()
                .unwrap()
                .project
                .state()
                .domains
                .arrangement
                .tracks
                .len(),
            2
        );
        assert_eq!(
            session.history_status().unwrap().undo_label.as_deref(),
            Some("Create Pattern track")
        );
    }
}
