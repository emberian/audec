//! Authoritative pattern/placement queries and reveal adapters.
//!
//! Pattern occurrences span four identity spaces: an arrangement clip, its
//! arrangement-local definition alias, a placement binding, and a sequencer
//! clip. This module resolves the complete chain against one immutable project
//! publication. UI code receives typed, revalidatable targets rather than raw
//! integers or references into a lock guard.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::arrangement::{
    self, ClipContent, ClipId, FrameRange, PatternRegion, Selection, TrackId,
};
use crate::command::{claims_for_commands, BindingCommand, CommandEnvelope, DomainCommand};
use crate::daw_project::{DawProject, ProjectState};
use crate::pattern_actions::{PatternEditorMode, PatternEditorTarget};
use crate::project_controller::{ObjectRef, PatternOccurrenceRef, RevealIntent, RevealRequest};
use crate::sequencer::{
    PatternClipId, PatternContent, PatternDefinition, PatternId, SequencerCommand,
};

#[derive(Clone, Copy)]
pub struct PatternUseSnapshot<'a> {
    pub aggregate_revision: u64,
    pub state: &'a ProjectState,
}

impl<'a> PatternUseSnapshot<'a> {
    pub fn from_project(project: &'a DawProject) -> Self {
        Self {
            aggregate_revision: project.revisions().aggregate,
            state: project.state(),
        }
    }
}

/// Complete stable identity of one arrangement placement.
///
/// All three IDs are checked on every resolution. If undo, reopen, or another
/// edit deleted or retargeted any member, resolution refuses the stale target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PatternOccurrenceTarget {
    pub arrangement_clip: ClipId,
    pub sequencer_clip: PatternClipId,
    pub pattern: PatternId,
}

impl From<PatternOccurrenceTarget> for PatternOccurrenceRef {
    fn from(value: PatternOccurrenceTarget) -> Self {
        Self {
            arrangement_clip: value.arrangement_clip,
            sequencer_clip: Some(value.sequencer_clip),
            pattern: Some(value.pattern),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternOccurrence {
    pub target: PatternOccurrenceTarget,
    pub arrangement_pattern: arrangement::PatternId,
    pub track: TrackId,
    pub placement: FrameRange,
    pub content_offset_frames: u64,
    pub looped: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternUseSummary {
    pub pattern: PatternId,
    pub editor_target: PatternEditorTarget,
    pub occurrences: Vec<PatternOccurrence>,
    pub tracks: Vec<TrackId>,
}

/// Inspector target data contains only durable IDs and exact placement facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternInspectorTarget {
    Definition {
        pattern: PatternId,
        occurrences: Vec<PatternOccurrenceTarget>,
        tracks: Vec<TrackId>,
    },
    Occurrence(PatternOccurrence),
}

/// One neutral payload can drive ObjectNavigator, Inspector, the pattern
/// editor, and arrangement selection without any GPUI entity dependency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternRevealData {
    pub primary: ObjectRef,
    pub related: Vec<ObjectRef>,
    pub inspector: PatternInspectorTarget,
    pub pattern_editor: PatternEditorTarget,
    pub arrangement_selection: Selection,
}

impl PatternRevealData {
    pub fn reveal_request(&self, intent: RevealIntent) -> RevealRequest {
        RevealRequest::new(self.primary.clone(), intent).with_related(self.related.clone())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PatternUseGraph {
    aggregate_revision: u64,
    by_pattern: BTreeMap<PatternId, PatternUseSummary>,
    by_occurrence: BTreeMap<ClipId, PatternOccurrence>,
}

impl PatternUseGraph {
    pub fn build(snapshot: PatternUseSnapshot<'_>) -> Result<Self, PatternUseError> {
        let mut by_pattern = snapshot
            .state
            .domains
            .sequencer
            .patterns()
            .patterns()
            .map(|pattern| {
                (
                    pattern.id,
                    PatternUseSummary {
                        pattern: pattern.id,
                        editor_target: editor_target(pattern),
                        occurrences: Vec::new(),
                        tracks: Vec::new(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut by_occurrence = BTreeMap::new();

        // Arrangement ordering is presentation ordering and remains stable
        // across serialization. Never infer it from raw ID ordering.
        for track_id in &snapshot.state.domains.arrangement.track_order {
            let track = snapshot
                .state
                .domains
                .arrangement
                .track(*track_id)
                .ok_or(PatternUseError::MissingTrack(*track_id))?;
            for clip_id in &track.clip_ids {
                let clip = snapshot
                    .state
                    .domains
                    .arrangement
                    .clip(*clip_id)
                    .ok_or(PatternUseError::MissingOccurrence(*clip_id))?;
                if !matches!(&clip.content, ClipContent::Pattern(_)) {
                    continue;
                }
                let occurrence = resolve_arrangement_occurrence(snapshot.state, *clip_id)?;
                let summary = by_pattern
                    .get_mut(&occurrence.target.pattern)
                    .ok_or(PatternUseError::MissingPattern(occurrence.target.pattern))?;
                if !summary.tracks.contains(&occurrence.track) {
                    summary.tracks.push(occurrence.track);
                }
                summary.occurrences.push(occurrence.clone());
                by_occurrence.insert(*clip_id, occurrence);
            }
        }

        Ok(Self {
            aggregate_revision: snapshot.aggregate_revision,
            by_pattern,
            by_occurrence,
        })
    }

    pub const fn aggregate_revision(&self) -> u64 {
        self.aggregate_revision
    }

    pub fn pattern(&self, pattern: PatternId) -> Result<&PatternUseSummary, PatternUseError> {
        self.by_pattern
            .get(&pattern)
            .ok_or(PatternUseError::MissingPattern(pattern))
    }

    pub fn occurrence(
        &self,
        target: PatternOccurrenceTarget,
    ) -> Result<&PatternOccurrence, PatternUseError> {
        let occurrence = self
            .by_occurrence
            .get(&target.arrangement_clip)
            .ok_or(PatternUseError::MissingOccurrence(target.arrangement_clip))?;
        if occurrence.target != target {
            return Err(PatternUseError::StaleOccurrence {
                expected: target,
                actual: occurrence.target,
            });
        }
        Ok(occurrence)
    }

    pub fn occurrence_for_clip(
        &self,
        arrangement_clip: ClipId,
    ) -> Result<&PatternOccurrence, PatternUseError> {
        self.by_occurrence
            .get(&arrangement_clip)
            .ok_or(PatternUseError::MissingOccurrence(arrangement_clip))
    }

    pub fn reveal_pattern(&self, pattern: PatternId) -> Result<PatternRevealData, PatternUseError> {
        let summary = self.pattern(pattern)?;
        let occurrences = summary
            .occurrences
            .iter()
            .map(|occurrence| occurrence.target)
            .collect::<Vec<_>>();
        let related = summary
            .occurrences
            .iter()
            .map(|occurrence| ObjectRef::PatternOccurrence(occurrence.target.into()))
            .chain(summary.tracks.iter().copied().map(ObjectRef::Track))
            .collect();
        Ok(PatternRevealData {
            primary: ObjectRef::Pattern(pattern),
            related,
            inspector: PatternInspectorTarget::Definition {
                pattern,
                occurrences,
                tracks: summary.tracks.clone(),
            },
            pattern_editor: summary.editor_target,
            arrangement_selection: selection_for_occurrences(&summary.occurrences),
        })
    }

    pub fn reveal_occurrence(
        &self,
        target: PatternOccurrenceTarget,
    ) -> Result<PatternRevealData, PatternUseError> {
        let occurrence = self.occurrence(target)?.clone();
        let definition = self.pattern(target.pattern)?;
        Ok(PatternRevealData {
            primary: ObjectRef::PatternOccurrence(target.into()),
            related: vec![
                ObjectRef::Pattern(target.pattern),
                ObjectRef::Track(occurrence.track),
            ],
            inspector: PatternInspectorTarget::Occurrence(occurrence.clone()),
            pattern_editor: definition.editor_target,
            arrangement_selection: selection_for_occurrences(&[occurrence]),
        })
    }
}

/// Resolve a current arrangement clip into its complete occurrence identity.
pub fn resolve_pattern_occurrence(
    snapshot: PatternUseSnapshot<'_>,
    arrangement_clip: ClipId,
) -> Result<PatternOccurrence, PatternUseError> {
    resolve_arrangement_occurrence(snapshot.state, arrangement_clip)
}

/// Re-resolve a stored occurrence identity against a later publication.
pub fn validate_occurrence_target(
    snapshot: PatternUseSnapshot<'_>,
    target: PatternOccurrenceTarget,
) -> Result<PatternOccurrence, PatternUseError> {
    let occurrence = resolve_arrangement_occurrence(snapshot.state, target.arrangement_clip)?;
    if occurrence.target != target {
        return Err(PatternUseError::StaleOccurrence {
            expected: target,
            actual: occurrence.target,
        });
    }
    Ok(occurrence)
}

fn resolve_arrangement_occurrence(
    state: &ProjectState,
    arrangement_clip: ClipId,
) -> Result<PatternOccurrence, PatternUseError> {
    let clip = state
        .domains
        .arrangement
        .clip(arrangement_clip)
        .ok_or(PatternUseError::MissingOccurrence(arrangement_clip))?;
    let ClipContent::Pattern(PatternRegion {
        pattern: arrangement_pattern,
        content_offset_frames,
        looped,
    }) = &clip.content
    else {
        return Err(PatternUseError::NotPatternOccurrence(arrangement_clip));
    };
    let pattern = state
        .bindings
        .patterns
        .definitions
        .get(arrangement_pattern)
        .copied()
        .ok_or(PatternUseError::MissingDefinitionBinding(
            *arrangement_pattern,
        ))?;
    let sequencer_clip = state
        .bindings
        .patterns
        .placements
        .get(&arrangement_clip)
        .copied()
        .ok_or(PatternUseError::MissingPlacementBinding(arrangement_clip))?;
    let sequence = state
        .domains
        .sequencer
        .clip(sequencer_clip)
        .ok_or(PatternUseError::MissingSequencerClip(sequencer_clip))?;
    if sequence.pattern != pattern {
        return Err(PatternUseError::PlacementPatternMismatch {
            occurrence: arrangement_clip,
            alias_pattern: pattern,
            sequencer_pattern: sequence.pattern,
        });
    }
    if state.domains.sequencer.patterns().get(pattern).is_none() {
        return Err(PatternUseError::MissingPattern(pattern));
    }
    if state.domains.arrangement.track(clip.track_id).is_none() {
        return Err(PatternUseError::MissingTrack(clip.track_id));
    }
    Ok(PatternOccurrence {
        target: PatternOccurrenceTarget {
            arrangement_clip,
            sequencer_clip,
            pattern,
        },
        arrangement_pattern: *arrangement_pattern,
        track: clip.track_id,
        placement: clip.placement,
        content_offset_frames: *content_offset_frames,
        looped: *looped,
    })
}

fn editor_target(definition: &PatternDefinition) -> PatternEditorTarget {
    PatternEditorTarget::new(
        definition.id,
        match &definition.content {
            PatternContent::Notes(_) => PatternEditorMode::PianoRoll,
            PatternContent::Steps(_) => PatternEditorMode::Steps,
        },
    )
}

fn selection_for_occurrences(occurrences: &[PatternOccurrence]) -> Selection {
    let clips = occurrences
        .iter()
        .map(|occurrence| occurrence.target.arrangement_clip)
        .collect::<BTreeSet<_>>();
    let tracks = occurrences
        .iter()
        .map(|occurrence| occurrence.track)
        .collect::<BTreeSet<_>>();
    let time = occurrences
        .iter()
        .map(|occurrence| occurrence.placement)
        .reduce(|left, right| {
            FrameRange::new(left.start.min(right.start), left.end.max(right.end))
                .expect("two valid ranges have a valid union")
        });
    Selection {
        clips,
        tracks,
        time,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MakeOccurrenceUniqueIntent {
    pub expected_project_revision: u64,
    pub occurrence: PatternOccurrenceTarget,
    pub name: Option<String>,
}

/// Copy one shared definition and retarget only the selected occurrence.
/// The returned multi-domain envelope is one gesture and one undo unit.
pub fn lower_make_occurrence_unique(
    snapshot: PatternUseSnapshot<'_>,
    intent: &MakeOccurrenceUniqueIntent,
) -> Result<CommandEnvelope, PatternUseError> {
    if intent.expected_project_revision != snapshot.aggregate_revision {
        return Err(PatternUseError::ProjectRevisionConflict {
            expected: intent.expected_project_revision,
            actual: snapshot.aggregate_revision,
        });
    }
    let occurrence = validate_occurrence_target(snapshot, intent.occurrence)?;
    let arrangement_clip = snapshot
        .state
        .domains
        .arrangement
        .clip(occurrence.target.arrangement_clip)
        .ok_or(PatternUseError::MissingOccurrence(
            occurrence.target.arrangement_clip,
        ))?;
    if arrangement_clip.locked {
        return Err(PatternUseError::LockedOccurrence(arrangement_clip.id));
    }
    let track = snapshot
        .state
        .domains
        .arrangement
        .track(occurrence.track)
        .ok_or(PatternUseError::MissingTrack(occurrence.track))?;
    if track.locked {
        return Err(PatternUseError::LockedTrack(track.id));
    }
    let graph = PatternUseGraph::build(snapshot)?;
    if graph.pattern(occurrence.target.pattern)?.occurrences.len() <= 1 {
        return Err(PatternUseError::AlreadyUnique(occurrence.target));
    }

    let before_pattern = snapshot
        .state
        .domains
        .sequencer
        .patterns()
        .get(occurrence.target.pattern)
        .ok_or(PatternUseError::MissingPattern(occurrence.target.pattern))?;
    let mut allocator = snapshot.state.domains.sequencer.clone();
    let mut unique_pattern = before_pattern.clone();
    unique_pattern.id = allocator.allocate_pattern_id();
    if unique_pattern.id.get() == u64::MAX {
        return Err(PatternUseError::IdentityExhausted("pattern"));
    }
    unique_pattern.name = match intent.name.as_deref().map(str::trim) {
        Some("") => return Err(PatternUseError::EmptyName),
        Some(name) => name.to_owned(),
        None => format!("{} copy", before_pattern.name),
    };
    unique_pattern.revision = 0;

    let binding_cursor = snapshot.state.bindings.allocator_state();
    let alias = arrangement::PatternId::from_raw(binding_cursor.next_arrangement_pattern);
    if alias.get() == 0 || alias.get() == u64::MAX {
        return Err(PatternUseError::IdentityExhausted("pattern alias"));
    }
    let mut unique_arrangement_clip = arrangement_clip.clone();
    let ClipContent::Pattern(region) = &mut unique_arrangement_clip.content else {
        return Err(PatternUseError::NotPatternOccurrence(arrangement_clip.id));
    };
    region.pattern = alias;

    let before_sequence = snapshot
        .state
        .domains
        .sequencer
        .clip(occurrence.target.sequencer_clip)
        .ok_or(PatternUseError::MissingSequencerClip(
            occurrence.target.sequencer_clip,
        ))?;
    let mut unique_sequence = before_sequence.clone();
    unique_sequence.pattern = unique_pattern.id;

    let commands = vec![
        DomainCommand::Sequencer(SequencerCommand::PutPattern {
            before: None,
            after: Some(unique_pattern.clone()),
        }),
        DomainCommand::Bindings(BindingCommand::PutPatternDefinitionAlias {
            alias,
            before: None,
            after: Some(unique_pattern.id),
        }),
        DomainCommand::Arrangement(arrangement::ArrangementOperation::PutClip {
            before: Some(arrangement_clip.clone()),
            after: Some(unique_arrangement_clip),
        }),
        DomainCommand::Sequencer(SequencerCommand::PutClip {
            before: Some(before_sequence.clone()),
            after: Some(unique_sequence),
        }),
    ];
    Ok(CommandEnvelope {
        label: "Make pattern occurrence unique".into(),
        base_revision: snapshot.aggregate_revision,
        coalesce: None,
        id_claims: claims_for_commands(&commands),
        commands,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternUseError {
    ProjectRevisionConflict {
        expected: u64,
        actual: u64,
    },
    MissingPattern(PatternId),
    MissingOccurrence(ClipId),
    NotPatternOccurrence(ClipId),
    MissingTrack(TrackId),
    MissingDefinitionBinding(arrangement::PatternId),
    MissingPlacementBinding(ClipId),
    MissingSequencerClip(PatternClipId),
    PlacementPatternMismatch {
        occurrence: ClipId,
        alias_pattern: PatternId,
        sequencer_pattern: PatternId,
    },
    StaleOccurrence {
        expected: PatternOccurrenceTarget,
        actual: PatternOccurrenceTarget,
    },
    LockedOccurrence(ClipId),
    LockedTrack(TrackId),
    AlreadyUnique(PatternOccurrenceTarget),
    EmptyName,
    IdentityExhausted(&'static str),
}

impl fmt::Display for PatternUseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectRevisionConflict { expected, actual } => write!(
                formatter,
                "pattern use expected project revision {expected}, current revision is {actual}"
            ),
            Self::MissingPattern(pattern) => write!(formatter, "pattern {} is missing", pattern.get()),
            Self::MissingOccurrence(clip) => {
                write!(formatter, "pattern occurrence clip {} is missing", clip.get())
            }
            Self::NotPatternOccurrence(clip) => {
                write!(formatter, "arrangement clip {} is not a pattern occurrence", clip.get())
            }
            Self::MissingTrack(track) => write!(formatter, "track {} is missing", track.get()),
            Self::MissingDefinitionBinding(alias) => write!(
                formatter,
                "arrangement pattern alias {} has no definition binding",
                alias.get()
            ),
            Self::MissingPlacementBinding(clip) => write!(
                formatter,
                "pattern occurrence {} has no sequencer placement binding",
                clip.get()
            ),
            Self::MissingSequencerClip(clip) => {
                write!(formatter, "sequencer placement {} is missing", clip.get())
            }
            Self::PlacementPatternMismatch {
                occurrence,
                alias_pattern,
                sequencer_pattern,
            } => write!(
                formatter,
                "occurrence {} resolves to pattern {} through its alias but sequencer placement targets {}",
                occurrence.get(),
                alias_pattern.get(),
                sequencer_pattern.get()
            ),
            Self::StaleOccurrence { expected, actual } => write!(
                formatter,
                "occurrence {} changed from sequencer clip {}/pattern {} to {}/{}",
                expected.arrangement_clip.get(),
                expected.sequencer_clip.get(),
                expected.pattern.get(),
                actual.sequencer_clip.get(),
                actual.pattern.get()
            ),
            Self::LockedOccurrence(clip) => {
                write!(formatter, "occurrence {} is locked", clip.get())
            }
            Self::LockedTrack(track) => write!(formatter, "track {} is locked", track.get()),
            Self::AlreadyUnique(target) => write!(
                formatter,
                "occurrence {} is already the pattern's only placement",
                target.arrangement_clip.get()
            ),
            Self::EmptyName => formatter.write_str("unique pattern name must not be empty"),
            Self::IdentityExhausted(kind) => write!(formatter, "{kind} identity is exhausted"),
        }
    }
}

impl Error for PatternUseError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::arrangement::Frame;
    use crate::arrangement_view::{
        ArrangementAction, ArrangementActionIntent, ArrangementViewEvent,
    };
    use crate::command::AppliedEnvelope;
    use crate::live_project::LiveProjectSnapshot;
    use crate::ontology::AuditoryIr;
    use crate::pattern_actions::{
        CreatePatternIntent, PatternAction, PatternActionIntent, PatternEditorMode,
    };
    use crate::pattern_controller::{
        lower_pattern_action, LoweredPatternAction, PatternActionSnapshot,
    };
    use crate::project_codecs::{decode_constructive, encode_constructive};
    use crate::project_controller::{lower_arrangement_event, ArrangementDispatch};
    use crate::project_io::ProjectFile;
    use crate::sequencer::{BeatDuration, PatternOrigin, PPQ};
    use crate::ui_drag::DropIntent;

    fn project_with_pattern() -> (DawProject, PatternId) {
        let mut project = DawProject::new("Pattern uses", 48_000, 120.0).unwrap();
        let intent = PatternActionIntent {
            expected_project_revision: project.revisions().aggregate,
            action: PatternAction::Create(CreatePatternIntent {
                mode: PatternEditorMode::Steps,
                name: "Shared beat".into(),
                length: BeatDuration((PPQ * 4) as u64),
                step_resolution: BeatDuration((PPQ / 4) as u64),
                initial_target: None,
            }),
        };
        let LoweredPatternAction::Execute(envelope) =
            lower_pattern_action(PatternActionSnapshot::from_project(&project), &intent).unwrap()
        else {
            panic!("create must lower to an envelope")
        };
        envelope.apply(&mut project).unwrap();
        let pattern = project
            .state()
            .domains
            .sequencer
            .patterns()
            .patterns()
            .next()
            .unwrap()
            .id;
        (project, pattern)
    }

    fn live_snapshot(project: &DawProject) -> LiveProjectSnapshot {
        LiveProjectSnapshot {
            project: Arc::new(project.clone()),
            pcm: Arc::new(BTreeMap::new()),
            sample_pcm: Arc::new(BTreeMap::new()),
        }
    }

    fn place(project: &mut DawProject, pattern: PatternId, at: Frame) -> AppliedEnvelope {
        let snapshot = live_snapshot(project);
        let event = ArrangementViewEvent::Action(ArrangementActionIntent {
            expected_revision: project.revisions().aggregate,
            action: ArrangementAction::Drop(DropIntent::InsertPattern {
                pattern,
                track: None,
                at,
                make_unique: false,
            }),
        });
        let ArrangementDispatch::Apply(validated) =
            lower_arrangement_event(&snapshot, event).unwrap()
        else {
            panic!("pattern drop must lower to an envelope")
        };
        validated.envelope.apply(project).unwrap()
    }

    fn delete_occurrence(project: &mut DawProject, clip: ClipId) -> AppliedEnvelope {
        let snapshot = live_snapshot(project);
        let event = ArrangementViewEvent::Action(ArrangementActionIntent {
            expected_revision: project.revisions().aggregate,
            action: ArrangementAction::DeleteClips(BTreeSet::from([clip])),
        });
        let ArrangementDispatch::Apply(validated) =
            lower_arrangement_event(&snapshot, event).unwrap()
        else {
            panic!("delete must lower to an envelope")
        };
        validated.envelope.apply(project).unwrap()
    }

    #[test]
    fn graph_resolves_all_placements_tracks_and_exact_reveal_data() {
        let (mut project, pattern) = project_with_pattern();
        place(&mut project, pattern, Frame::ZERO);
        place(&mut project, pattern, Frame(192_000));

        let graph = PatternUseGraph::build(PatternUseSnapshot::from_project(&project)).unwrap();
        let summary = graph.pattern(pattern).unwrap();
        assert_eq!(summary.occurrences.len(), 2);
        assert_eq!(summary.tracks.len(), 2);
        assert!(summary
            .occurrences
            .iter()
            .all(|occurrence| occurrence.target.pattern == pattern));
        assert_eq!(
            summary
                .occurrences
                .iter()
                .map(|occurrence| occurrence.track)
                .collect::<Vec<_>>(),
            summary.tracks
        );

        let reveal = graph.reveal_pattern(pattern).unwrap();
        assert_eq!(reveal.primary, ObjectRef::Pattern(pattern));
        assert_eq!(reveal.arrangement_selection.clips.len(), 2);
        assert_eq!(reveal.arrangement_selection.tracks.len(), 2);
        assert_eq!(
            reveal.arrangement_selection.time,
            FrameRange::new(Frame::ZERO, Frame(288_000)).ok()
        );
        assert_eq!(
            reveal.pattern_editor,
            PatternEditorTarget::new(pattern, PatternEditorMode::Steps)
        );
        let request = reveal.reveal_request(RevealIntent::ShowInspector);
        assert_eq!(request.object, ObjectRef::Pattern(pattern));

        let occurrence = summary.occurrences[1].target;
        let reveal = graph.reveal_occurrence(occurrence).unwrap();
        assert_eq!(
            reveal.primary,
            ObjectRef::PatternOccurrence(occurrence.into())
        );
        assert_eq!(
            reveal.arrangement_selection.clips,
            BTreeSet::from([occurrence.arrangement_clip])
        );
        assert_eq!(
            graph.occurrence(occurrence).unwrap().target.pattern,
            pattern
        );
        assert_eq!(
            graph
                .occurrence_for_clip(occurrence.arrangement_clip)
                .unwrap()
                .target,
            occurrence
        );
        assert_eq!(
            resolve_pattern_occurrence(
                PatternUseSnapshot::from_project(&project),
                occurrence.arrangement_clip,
            )
            .unwrap()
            .target,
            occurrence
        );
    }

    #[test]
    fn occurrence_targets_reject_deletion_and_survive_undo_and_reopen() {
        let (mut project, pattern) = project_with_pattern();
        place(&mut project, pattern, Frame::ZERO);
        place(&mut project, pattern, Frame(192_000));
        let graph = PatternUseGraph::build(PatternUseSnapshot::from_project(&project)).unwrap();
        let target = graph.pattern(pattern).unwrap().occurrences[0].target;

        let deletion = delete_occurrence(&mut project, target.arrangement_clip);
        assert!(matches!(
            validate_occurrence_target(PatternUseSnapshot::from_project(&project), target),
            Err(PatternUseError::MissingOccurrence(id)) if id == target.arrangement_clip
        ));
        deletion.inverse.apply(&mut project).unwrap();
        assert_eq!(
            validate_occurrence_target(PatternUseSnapshot::from_project(&project), target)
                .unwrap()
                .target,
            target
        );

        let file = ProjectFile::from_project(&project, None);
        let payloads = encode_constructive(&project).unwrap();
        let reopened = decode_constructive(&file, &payloads, AuditoryIr::new(48_000)).unwrap();
        let reopened_snapshot = PatternUseSnapshot {
            aggregate_revision: reopened.aggregate_revision,
            state: &reopened.state,
        };
        assert_eq!(
            validate_occurrence_target(reopened_snapshot, target)
                .unwrap()
                .target,
            target
        );
        assert!(matches!(
            PatternUseGraph::build(reopened_snapshot)
                .unwrap()
                .pattern(PatternId::from_raw(9_999)),
            Err(PatternUseError::MissingPattern(_))
        ));
        assert!(matches!(
            validate_occurrence_target(
                reopened_snapshot,
                PatternOccurrenceTarget {
                    arrangement_clip: ClipId::from_raw(9_999),
                    ..target
                }
            ),
            Err(PatternUseError::MissingOccurrence(_))
        ));
    }

    #[test]
    fn make_unique_retargets_one_occurrence_and_is_one_undo_unit() {
        let (mut project, pattern) = project_with_pattern();
        place(&mut project, pattern, Frame::ZERO);
        place(&mut project, pattern, Frame(192_000));
        let graph = PatternUseGraph::build(PatternUseSnapshot::from_project(&project)).unwrap();
        let target = graph.pattern(pattern).unwrap().occurrences[0].target;
        let source_origin = project
            .state()
            .domains
            .sequencer
            .patterns()
            .get(pattern)
            .unwrap()
            .origin
            .clone();
        assert_eq!(source_origin, PatternOrigin::Authored);

        let intent = MakeOccurrenceUniqueIntent {
            expected_project_revision: project.revisions().aggregate,
            occurrence: target,
            name: Some("Unique beat".into()),
        };
        let mut stale = intent.clone();
        stale.expected_project_revision = stale.expected_project_revision.saturating_add(1);
        assert!(matches!(
            lower_make_occurrence_unique(PatternUseSnapshot::from_project(&project), &stale),
            Err(PatternUseError::ProjectRevisionConflict { .. })
        ));
        let envelope =
            lower_make_occurrence_unique(PatternUseSnapshot::from_project(&project), &intent)
                .unwrap();
        assert_eq!(envelope.commands.len(), 4);
        assert!(envelope.coalesce.is_none());
        let applied = envelope.apply(&mut project).unwrap();

        let graph = PatternUseGraph::build(PatternUseSnapshot::from_project(&project)).unwrap();
        assert_eq!(graph.pattern(pattern).unwrap().occurrences.len(), 1);
        let actual =
            match validate_occurrence_target(PatternUseSnapshot::from_project(&project), target) {
                Err(PatternUseError::StaleOccurrence { actual, .. }) => actual,
                other => panic!("expected a retargeted occurrence, got {other:?}"),
            };
        assert_ne!(actual.pattern, pattern);
        assert_eq!(
            project
                .state()
                .domains
                .sequencer
                .patterns()
                .get(actual.pattern)
                .unwrap()
                .origin,
            source_origin
        );
        assert!(matches!(
            lower_make_occurrence_unique(
                PatternUseSnapshot::from_project(&project),
                &MakeOccurrenceUniqueIntent {
                    expected_project_revision: project.revisions().aggregate,
                    occurrence: actual,
                    name: None,
                }
            ),
            Err(PatternUseError::AlreadyUnique(found)) if found == actual
        ));

        applied.inverse.apply(&mut project).unwrap();
        assert_eq!(
            validate_occurrence_target(PatternUseSnapshot::from_project(&project), target)
                .unwrap()
                .target,
            target
        );
        assert_eq!(
            PatternUseGraph::build(PatternUseSnapshot::from_project(&project))
                .unwrap()
                .pattern(pattern)
                .unwrap()
                .occurrences
                .len(),
            2
        );
    }
}
