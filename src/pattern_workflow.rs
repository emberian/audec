//! End-to-end controller API for authored pattern work.
//!
//! The workflow joins semantic editor actions, constructive Make Beat
//! publications, occurrence navigation, and exact loop-cycle audition without
//! introducing a GPUI or shared-lock mutation path.

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::command_record::{CoalesceToken, CommandAddress};
use crate::live_project::{ProjectController, ProjectControllerError, ProjectControllerUpdate};
use crate::pattern_actions::{PatternAction, PatternActionIntent, PatternEditorTarget};
use crate::pattern_authoring;
use crate::pattern_controller::{
    lower_pattern_action, LoweredPatternAction, PatternActionSnapshot, PatternLoweringError,
};
use crate::pattern_lang::PatternEvalDiagnostic;
use crate::pattern_use_graph::{
    lower_make_occurrence_unique, resolve_pattern_occurrence, validate_occurrence_target,
    MakeOccurrenceUniqueIntent, PatternOccurrenceTarget, PatternRevealData, PatternUseError,
    PatternUseGraph, PatternUseSnapshot, PatternUseSummary,
};
use crate::sample_kit::KitId;
use crate::sequencer::{
    FrameRange as SequencerFrameRange, PatternDefinition, PatternId, PatternOrigin, ProjectFrame,
    ScheduledEvent, ScheduledKind,
};

use super::ConstructivePublication;
use super::ProjectGesture;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PatternWorkflowRequestId(u64);

impl PatternWorkflowRequestId {
    pub const fn from_raw(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PatternWorkflowRequest {
    pub id: PatternWorkflowRequestId,
    pub intent: PatternWorkflowIntent,
}

pub enum PatternWorkflowDispatchReceipt {
    Accepted(PatternWorkflowRequestId),
    Completed {
        request: PatternWorkflowRequestId,
        result: Result<PatternWorkflowOutcome, PatternWorkflowError>,
    },
}

impl PatternWorkflowDispatchReceipt {
    pub const fn accepted(request: PatternWorkflowRequestId) -> Self {
        Self::Accepted(request)
    }
}

pub type PatternWorkflowCallback =
    Arc<dyn Fn(PatternWorkflowRequest) -> PatternWorkflowDispatchReceipt + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatternGestureKind {
    MoveNote,
    ResizeNote,
    MoveStep,
    AdjustEvent,
}

impl PatternGestureKind {
    const fn token_kind(self) -> u64 {
        match self {
            Self::MoveNote => 1,
            Self::ResizeNote => 2,
            Self::MoveStep => 3,
            Self::AdjustEvent => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BeginPatternGestureIntent {
    pub expected_project_revision: u64,
    pub editor_session: u64,
    pub pattern: PatternId,
    pub kind: PatternGestureKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternGestureReceipt {
    pub editor_session: u64,
    pub pattern: PatternId,
    pub kind: PatternGestureKind,
    controller: ProjectGesture,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternMutationKind {
    Created,
    Duplicated {
        source: PatternId,
    },
    Edited,
    Deleted,
    MadeUnique {
        source: PatternId,
        occurrence: PatternOccurrenceTarget,
    },
    ConstructiveBeat {
        kit: KitId,
    },
}

/// Exact after-publication data for editor refresh, Inspector, and navigation.
#[derive(Clone, Debug, PartialEq)]
pub struct PatternEditPublication {
    pub revision: u64,
    pub mutation: PatternMutationKind,
    pub pattern: PatternId,
    pub definition: Option<PatternDefinition>,
    pub origin: Option<PatternOrigin>,
    pub uses: Option<PatternUseSummary>,
    pub reveal: Option<PatternRevealData>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PatternCyclePublication {
    pub revision: u64,
    pub target: PatternEditorTarget,
    pub cycle_index: u64,
    pub performance_seed: u64,
    pub definition: PatternDefinition,
    pub diagnostics: Vec<PatternEvalDiagnostic>,
    pub reveal: PatternRevealData,
}

/// Immutable data needed to open or retarget a live pattern editor without
/// making the GPUI layer reconstruct use-graph relationships.
#[derive(Clone, Debug, PartialEq)]
pub struct PatternEditorHydration {
    pub revision: u64,
    pub target: PatternEditorTarget,
    pub definition: PatternDefinition,
    pub occurrence: Option<PatternOccurrenceTarget>,
    pub uses: PatternUseSummary,
    pub reveal: PatternRevealData,
}

pub fn hydrate_pattern_editor(
    snapshot: PatternUseSnapshot<'_>,
    target: PatternEditorTarget,
    occurrence: Option<PatternOccurrenceTarget>,
) -> Result<PatternEditorHydration, PatternWorkflowError> {
    let definition = snapshot
        .state
        .domains
        .sequencer
        .patterns()
        .get(target.pattern)
        .cloned()
        .ok_or(PatternWorkflowError::MissingPublicationPattern)?;
    let actual_mode = match &definition.content {
        crate::sequencer::PatternContent::Notes(_) => {
            crate::pattern_actions::PatternEditorMode::PianoRoll
        }
        crate::sequencer::PatternContent::Steps(_) => {
            crate::pattern_actions::PatternEditorMode::Steps
        }
    };
    if actual_mode != target.mode {
        return Err(PatternWorkflowError::EditorModeMismatch {
            expected: target.mode,
            actual: actual_mode,
        });
    }
    if let Some(occurrence) = occurrence {
        let resolved = validate_occurrence_target(snapshot, occurrence)?;
        if resolved.target.pattern != target.pattern {
            return Err(PatternWorkflowError::OccurrencePatternMismatch {
                editor: target.pattern,
                occurrence: resolved.target.pattern,
            });
        }
    }
    let graph = PatternUseGraph::build(snapshot)?;
    let uses = graph.pattern(target.pattern)?.clone();
    let reveal = match occurrence {
        Some(occurrence) => graph.reveal_occurrence(occurrence)?,
        None => graph.reveal_pattern(target.pattern)?,
    };
    Ok(PatternEditorHydration {
        revision: snapshot.aggregate_revision,
        target,
        definition,
        occurrence,
        uses,
        reveal,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PatternLoopAuditionIntent {
    pub expected_project_revision: u64,
    pub occurrence: PatternOccurrenceTarget,
    pub cycle_index: u64,
    pub performance_seed: u64,
}

/// A host can loop `loop_range` and submit only `events`; unrelated patterns
/// sharing the same project time are deliberately excluded.
#[derive(Clone, Debug, PartialEq)]
pub struct PatternLoopAuditionPlan {
    pub revision: u64,
    pub occurrence: PatternOccurrenceTarget,
    pub cycle_index: u64,
    pub performance_seed: u64,
    pub loop_range: SequencerFrameRange,
    pub events: Vec<ScheduledEvent>,
    pub reveal: PatternRevealData,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PatternWorkflowIntent {
    Action(PatternActionIntent),
    BeginGesture(BeginPatternGestureIntent),
    GestureEdit {
        receipt: PatternGestureReceipt,
        action: PatternActionIntent,
    },
    EndGesture(PatternGestureReceipt),
    MakeOccurrenceUnique(MakeOccurrenceUniqueIntent),
    Audition(PatternLoopAuditionIntent),
}

#[derive(Clone, Debug)]
pub enum PatternWorkflowOutcome {
    Published {
        update: ProjectControllerUpdate,
        publication: PatternEditPublication,
    },
    History(Option<ProjectControllerUpdate>),
    Navigate(PatternRevealData),
    Targeted(PatternEditorHydration),
    Preview(PatternCyclePublication),
    Audition(PatternLoopAuditionPlan),
    GestureBegan(PatternGestureReceipt),
    GestureEnded,
}

impl ProjectController {
    pub fn execute_pattern_workflow(
        &mut self,
        intent: PatternWorkflowIntent,
    ) -> Result<PatternWorkflowOutcome, PatternWorkflowError> {
        match intent {
            PatternWorkflowIntent::Action(intent) => self.execute_pattern_editor_action(intent),
            PatternWorkflowIntent::BeginGesture(intent) => {
                if intent.expected_project_revision != self.revisions().aggregate {
                    return Err(PatternWorkflowError::ProjectRevisionConflict {
                        expected: intent.expected_project_revision,
                        actual: self.revisions().aggregate,
                    });
                }
                if self
                    .snapshot()
                    .project
                    .state()
                    .domains
                    .sequencer
                    .patterns()
                    .get(intent.pattern)
                    .is_none()
                {
                    return Err(PatternWorkflowError::MissingPublicationPattern);
                }
                let controller = self.begin_gesture(CoalesceToken {
                    editor_session: intent.editor_session,
                    gesture_kind: intent.kind.token_kind(),
                    primary: CommandAddress::SequencerPattern(intent.pattern),
                });
                Ok(PatternWorkflowOutcome::GestureBegan(
                    PatternGestureReceipt {
                        editor_session: intent.editor_session,
                        pattern: intent.pattern,
                        kind: intent.kind,
                        controller,
                    },
                ))
            }
            PatternWorkflowIntent::GestureEdit { receipt, action } => {
                if !matches!(&action.action, PatternAction::Edit(_)) {
                    return Err(PatternWorkflowError::UnsupportedGestureAction);
                }
                if action_pattern(&action.action) != Some(receipt.pattern) {
                    return Err(PatternWorkflowError::GestureTargetMismatch {
                        expected: receipt.pattern,
                        actual: action_pattern(&action.action),
                    });
                }
                self.execute_pattern_editor_action_in_gesture(action, &receipt.controller)
            }
            PatternWorkflowIntent::EndGesture(receipt) => {
                self.end_gesture(&receipt.controller)?;
                Ok(PatternWorkflowOutcome::GestureEnded)
            }
            PatternWorkflowIntent::MakeOccurrenceUnique(intent) => {
                let envelope = lower_make_occurrence_unique(
                    PatternUseSnapshot::from_project(&self.snapshot().project),
                    &intent,
                )?;
                let update = self.execute(envelope)?;
                let occurrence = resolve_pattern_occurrence(
                    PatternUseSnapshot::from_project(&update.snapshot.project),
                    intent.occurrence.arrangement_clip,
                )?;
                let publication = publish_pattern(
                    PatternUseSnapshot::from_project(&update.snapshot.project),
                    occurrence.target.pattern,
                    PatternMutationKind::MadeUnique {
                        source: intent.occurrence.pattern,
                        occurrence: occurrence.target,
                    },
                    Some(occurrence.target),
                )?;
                Ok(PatternWorkflowOutcome::Published {
                    update,
                    publication,
                })
            }
            PatternWorkflowIntent::Audition(intent) => plan_loop_audition(
                PatternUseSnapshot::from_project(&self.snapshot().project),
                intent,
            )
            .map(PatternWorkflowOutcome::Audition),
        }
    }

    fn execute_pattern_editor_action(
        &mut self,
        intent: PatternActionIntent,
    ) -> Result<PatternWorkflowOutcome, PatternWorkflowError> {
        let lowered = lower_pattern_action(
            PatternActionSnapshot::from_project(&self.snapshot().project),
            &intent,
        )?;
        match lowered {
            LoweredPatternAction::Execute(envelope) => {
                let mutation = mutation_for_action(&intent.action);
                let target_before = action_pattern(&intent.action);
                let update = self.execute(envelope)?;
                let pattern = target_before
                    .or_else(|| created_pattern(&update))
                    .ok_or(PatternWorkflowError::MissingPublicationPattern)?;
                let publication = publish_pattern(
                    PatternUseSnapshot::from_project(&update.snapshot.project),
                    pattern,
                    mutation,
                    None,
                )?;
                Ok(PatternWorkflowOutcome::Published {
                    update,
                    publication,
                })
            }
            LoweredPatternAction::Undo => self
                .undo()
                .map(PatternWorkflowOutcome::History)
                .map_err(PatternWorkflowError::Controller),
            LoweredPatternAction::Redo => self
                .redo()
                .map(PatternWorkflowOutcome::History)
                .map_err(PatternWorkflowError::Controller),
            LoweredPatternAction::Retarget(target) => hydrate_pattern_editor(
                PatternUseSnapshot::from_project(&self.snapshot().project),
                target,
                None,
            )
            .map(PatternWorkflowOutcome::Targeted),
            LoweredPatternAction::PreviewCycle {
                target,
                cycle_index,
                performance_seed,
            } => {
                let snapshot = PatternUseSnapshot::from_project(&self.snapshot().project);
                let definition = snapshot
                    .state
                    .domains
                    .sequencer
                    .patterns()
                    .get(target.pattern)
                    .ok_or(PatternWorkflowError::MissingPublicationPattern)?;
                let preview = pattern_authoring::preview_expression_placement(
                    definition,
                    cycle_index,
                    performance_seed,
                )?;
                let graph = PatternUseGraph::build(snapshot)?;
                Ok(PatternWorkflowOutcome::Preview(PatternCyclePublication {
                    revision: snapshot.aggregate_revision,
                    target,
                    cycle_index,
                    performance_seed,
                    definition: preview.definition,
                    diagnostics: preview.diagnostics,
                    reveal: graph.reveal_pattern(target.pattern)?,
                }))
            }
        }
    }

    fn execute_pattern_editor_action_in_gesture(
        &mut self,
        intent: PatternActionIntent,
        gesture: &ProjectGesture,
    ) -> Result<PatternWorkflowOutcome, PatternWorkflowError> {
        let lowered = lower_pattern_action(
            PatternActionSnapshot::from_project(&self.snapshot().project),
            &intent,
        )?;
        let LoweredPatternAction::Execute(envelope) = lowered else {
            return Err(PatternWorkflowError::UnsupportedGestureAction);
        };
        let mutation = mutation_for_action(&intent.action);
        let pattern =
            action_pattern(&intent.action).ok_or(PatternWorkflowError::UnsupportedGestureAction)?;
        let update = self.execute_in_gesture(gesture, envelope)?;
        let publication = publish_pattern(
            PatternUseSnapshot::from_project(&update.snapshot.project),
            pattern,
            mutation,
            None,
        )?;
        Ok(PatternWorkflowOutcome::Published {
            update,
            publication,
        })
    }
}

/// Adapt an already-committed select/chop → Make Beat result into the exact
/// same publication consumed by later editor actions.
pub fn publication_from_constructive(
    snapshot: PatternUseSnapshot<'_>,
    publication: &ConstructivePublication,
) -> Result<Option<PatternEditPublication>, PatternWorkflowError> {
    let Some(pattern) = publication.pattern else {
        return Ok(None);
    };
    if publication.revision != snapshot.aggregate_revision {
        return Err(PatternWorkflowError::ProjectRevisionConflict {
            expected: publication.revision,
            actual: snapshot.aggregate_revision,
        });
    }
    let occurrence = publication
        .arrangement_clip
        .map(|clip| resolve_pattern_occurrence(snapshot, clip))
        .transpose()?
        .map(|occurrence| occurrence.target);
    publish_pattern(
        snapshot,
        pattern,
        PatternMutationKind::ConstructiveBeat {
            kit: publication.kit,
        },
        occurrence,
    )
    .map(Some)
}

fn publish_pattern(
    snapshot: PatternUseSnapshot<'_>,
    pattern: PatternId,
    mutation: PatternMutationKind,
    preferred_occurrence: Option<PatternOccurrenceTarget>,
) -> Result<PatternEditPublication, PatternWorkflowError> {
    let definition = snapshot
        .state
        .domains
        .sequencer
        .patterns()
        .get(pattern)
        .cloned();
    if definition.is_none() && !matches!(mutation, PatternMutationKind::Deleted) {
        return Err(PatternWorkflowError::MissingPublicationPattern);
    }
    let graph = PatternUseGraph::build(snapshot)?;
    let uses = definition
        .as_ref()
        .map(|_| graph.pattern(pattern).cloned())
        .transpose()?;
    let reveal = match (definition.as_ref(), preferred_occurrence) {
        (Some(_), Some(occurrence)) => Some(graph.reveal_occurrence(occurrence)?),
        (Some(_), None) => Some(graph.reveal_pattern(pattern)?),
        (None, _) => None,
    };
    Ok(PatternEditPublication {
        revision: snapshot.aggregate_revision,
        mutation,
        pattern,
        origin: definition
            .as_ref()
            .map(|definition| definition.origin.clone()),
        definition,
        uses,
        reveal,
    })
}

fn mutation_for_action(action: &PatternAction) -> PatternMutationKind {
    match action {
        PatternAction::Create(_) => PatternMutationKind::Created,
        PatternAction::Duplicate { source, .. } => {
            PatternMutationKind::Duplicated { source: *source }
        }
        PatternAction::Delete { .. } => PatternMutationKind::Deleted,
        PatternAction::Edit(_) => PatternMutationKind::Edited,
        PatternAction::Undo
        | PatternAction::Redo
        | PatternAction::Retarget(_)
        | PatternAction::PreviewCycle { .. } => PatternMutationKind::Edited,
    }
}

fn action_pattern(action: &PatternAction) -> Option<PatternId> {
    match action {
        PatternAction::Delete { pattern, .. } => Some(*pattern),
        PatternAction::Edit(intent) => Some(intent.pattern),
        PatternAction::Create(_)
        | PatternAction::Duplicate { .. }
        | PatternAction::Undo
        | PatternAction::Redo
        | PatternAction::Retarget(_)
        | PatternAction::PreviewCycle { .. } => None,
    }
}

fn created_pattern(update: &ProjectControllerUpdate) -> Option<PatternId> {
    update.applied.envelope.commands.iter().find_map(|command| {
        let crate::command::DomainCommand::Sequencer(
            crate::sequencer::SequencerCommand::PutPattern {
                before: None,
                after: Some(pattern),
            },
        ) = command
        else {
            return None;
        };
        Some(pattern.id)
    })
}

fn plan_loop_audition(
    snapshot: PatternUseSnapshot<'_>,
    intent: PatternLoopAuditionIntent,
) -> Result<PatternLoopAuditionPlan, PatternWorkflowError> {
    if intent.expected_project_revision != snapshot.aggregate_revision {
        return Err(PatternWorkflowError::ProjectRevisionConflict {
            expected: intent.expected_project_revision,
            actual: snapshot.aggregate_revision,
        });
    }
    let occurrence = validate_occurrence_target(snapshot, intent.occurrence)?;
    let sequence = snapshot
        .state
        .domains
        .sequencer
        .clip(intent.occurrence.sequencer_clip)
        .ok_or(PatternWorkflowError::MissingPublicationPattern)?;
    let pattern = snapshot
        .state
        .domains
        .sequencer
        .patterns()
        .get(intent.occurrence.pattern)
        .ok_or(PatternWorkflowError::MissingPublicationPattern)?;
    let cycle = i128::from(intent.cycle_index);
    let pattern_ticks = i128::from(pattern.length.0);
    let anchor = i128::from(sequence.start.0) - i128::from(sequence.pattern_offset.0);
    let raw_start = anchor
        .checked_add(
            cycle
                .checked_mul(pattern_ticks)
                .ok_or(PatternWorkflowError::CycleOutOfRange)?,
        )
        .ok_or(PatternWorkflowError::CycleOutOfRange)?;
    let raw_end = raw_start
        .checked_add(pattern_ticks)
        .ok_or(PatternWorkflowError::CycleOutOfRange)?;
    let tick_start = raw_start.max(i128::from(sequence.start.0));
    let tick_end = raw_end.min(i128::from(sequence.end().0));
    if tick_start >= tick_end
        || tick_start < i128::from(i64::MIN)
        || tick_end > i128::from(i64::MAX)
    {
        return Err(PatternWorkflowError::CycleOutOfRange);
    }
    let frame_start = snapshot
        .state
        .domains
        .sequencer
        .tempo_map()
        .beat_to_frame(crate::sequencer::BeatTime(tick_start as i64));
    let frame_end = snapshot
        .state
        .domains
        .sequencer
        .tempo_map()
        .beat_to_frame(crate::sequencer::BeatTime(tick_end as i64));
    if frame_start >= frame_end || frame_end.0.saturating_sub(frame_start.0) > i64::from(u32::MAX) {
        return Err(PatternWorkflowError::CycleTooLong);
    }
    let loop_range = SequencerFrameRange {
        start: frame_start,
        end: frame_end,
    };
    let events = snapshot
        .state
        .domains
        .sequencer
        .schedule_project_window(loop_range, intent.performance_seed)
        .into_iter()
        .filter(|event| scheduled_clip(&event.kind) == Some(intent.occurrence.sequencer_clip))
        .collect();
    let graph = PatternUseGraph::build(snapshot)?;
    Ok(PatternLoopAuditionPlan {
        revision: snapshot.aggregate_revision,
        occurrence: occurrence.target,
        cycle_index: intent.cycle_index,
        performance_seed: intent.performance_seed,
        loop_range,
        events,
        reveal: graph.reveal_occurrence(occurrence.target)?,
    })
}

fn scheduled_clip(kind: &ScheduledKind) -> Option<crate::sequencer::PatternClipId> {
    match kind {
        ScheduledKind::LoopBoundary => None,
        ScheduledKind::NoteOff { clip, .. }
        | ScheduledKind::NoteOn { clip, .. }
        | ScheduledKind::NoteExpression { clip, .. }
        | ScheduledKind::Trigger { clip, .. } => Some(*clip),
    }
}

#[derive(Debug)]
pub enum PatternWorkflowError {
    Lowering(PatternLoweringError),
    Controller(ProjectControllerError),
    Use(PatternUseError),
    Authoring(pattern_authoring::PatternAuthoringError),
    ProjectRevisionConflict {
        expected: u64,
        actual: u64,
    },
    MissingPublicationPattern,
    GestureTargetMismatch {
        expected: PatternId,
        actual: Option<PatternId>,
    },
    UnsupportedGestureAction,
    EditorModeMismatch {
        expected: crate::pattern_actions::PatternEditorMode,
        actual: crate::pattern_actions::PatternEditorMode,
    },
    OccurrencePatternMismatch {
        editor: PatternId,
        occurrence: PatternId,
    },
    CycleOutOfRange,
    CycleTooLong,
}

impl fmt::Display for PatternWorkflowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lowering(error) => error.fmt(formatter),
            Self::Controller(error) => error.fmt(formatter),
            Self::Use(error) => error.fmt(formatter),
            Self::Authoring(error) => error.fmt(formatter),
            Self::ProjectRevisionConflict { expected, actual } => write!(
                formatter,
                "pattern workflow expected project revision {expected}, current revision is {actual}"
            ),
            Self::MissingPublicationPattern => {
                formatter.write_str("pattern workflow publication target is missing")
            }
            Self::GestureTargetMismatch { expected, actual } => write!(
                formatter,
                "pattern gesture targets #{}, action targets {}",
                expected.get(),
                actual
                    .map(|pattern| format!("#{}", pattern.get()))
                    .unwrap_or_else(|| "no pattern".into())
            ),
            Self::UnsupportedGestureAction => {
                formatter.write_str("only durable pattern edits may run inside a gesture")
            }
            Self::EditorModeMismatch { expected, actual } => write!(
                formatter,
                "pattern editor mode {expected:?} does not match definition mode {actual:?}"
            ),
            Self::OccurrencePatternMismatch { editor, occurrence } => write!(
                formatter,
                "pattern editor targets #{}, occurrence targets #{}",
                editor.get(),
                occurrence.get()
            ),
            Self::CycleOutOfRange => formatter.write_str("placement cycle is outside the occurrence"),
            Self::CycleTooLong => formatter.write_str("placement cycle exceeds audition block limits"),
        }
    }
}

impl Error for PatternWorkflowError {}

impl From<PatternLoweringError> for PatternWorkflowError {
    fn from(value: PatternLoweringError) -> Self {
        Self::Lowering(value)
    }
}

impl From<ProjectControllerError> for PatternWorkflowError {
    fn from(value: ProjectControllerError) -> Self {
        Self::Controller(value)
    }
}

impl From<PatternUseError> for PatternWorkflowError {
    fn from(value: PatternUseError) -> Self {
        Self::Use(value)
    }
}

impl From<pattern_authoring::PatternAuthoringError> for PatternWorkflowError {
    fn from(value: pattern_authoring::PatternAuthoringError) -> Self {
        Self::Authoring(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    use crate::arrangement::Frame;
    use crate::arrangement_interaction::{ArrangementEdit, ArrangementEditIntent, GestureCommit};
    use crate::arrangement_view::{
        ArrangementAction, ArrangementActionIntent, ArrangementViewEvent,
    };
    use crate::daw_project::DawProject;
    use crate::live_project::LiveProject;
    use crate::pattern_actions::{
        CreatePatternIntent, PatternEdit, PatternEditIntent, PatternEditorMode,
    };
    use crate::pattern_authoring::{DivergedOverwrite, ExpressionRealizationContext};
    use crate::project_controller::{lower_arrangement_event, lower_gesture, ArrangementDispatch};
    use crate::sequencer::{BeatDuration, PatternContent, StepEvent, TriggerTarget, PPQ};
    use crate::ui_drag::DropIntent;

    fn controller() -> ProjectController {
        let project = DawProject::new("Pattern workflow", 48_000, 120.0).unwrap();
        let live = LiveProject::from_project(project, BTreeMap::new()).unwrap();
        ProjectController::new(live).unwrap()
    }

    fn workflow_intent(
        controller: &ProjectController,
        action: PatternAction,
    ) -> PatternWorkflowIntent {
        PatternWorkflowIntent::Action(PatternActionIntent {
            expected_project_revision: controller.revisions().aggregate,
            action,
        })
    }

    fn published(outcome: PatternWorkflowOutcome) -> PatternEditPublication {
        let PatternWorkflowOutcome::Published { publication, .. } = outcome else {
            panic!("expected a pattern publication")
        };
        publication
    }

    fn create_steps(
        controller: &mut ProjectController,
        initial_target: Option<TriggerTarget>,
    ) -> PatternEditPublication {
        let intent = workflow_intent(
            controller,
            PatternAction::Create(CreatePatternIntent {
                mode: PatternEditorMode::Steps,
                name: "Beat".into(),
                length: BeatDuration((PPQ * 4) as u64),
                step_resolution: BeatDuration((PPQ / 4) as u64),
                initial_target,
            }),
        );
        published(controller.execute_pattern_workflow(intent).unwrap())
    }

    fn place_pattern(
        controller: &mut ProjectController,
        pattern: PatternId,
        at: Frame,
    ) -> PatternOccurrenceTarget {
        let event = ArrangementViewEvent::Action(ArrangementActionIntent {
            expected_revision: controller.revisions().aggregate,
            action: ArrangementAction::Drop(DropIntent::InsertPattern {
                pattern,
                track: None,
                at,
                make_unique: false,
            }),
        });
        let ArrangementDispatch::Apply(validated) =
            lower_arrangement_event(controller.snapshot(), event).unwrap()
        else {
            panic!("pattern placement must produce an aggregate envelope")
        };
        controller.execute(validated.envelope).unwrap();
        PatternUseGraph::build(PatternUseSnapshot::from_project(
            &controller.snapshot().project,
        ))
        .unwrap()
        .pattern(pattern)
        .unwrap()
        .occurrences[0]
            .target
    }

    fn repeat_for_two_cycles(
        controller: &mut ProjectController,
        occurrence: PatternOccurrenceTarget,
    ) {
        let placement = controller
            .snapshot()
            .project
            .state()
            .domains
            .arrangement
            .clip(occurrence.arrangement_clip)
            .unwrap()
            .placement;
        let boundary = Frame(placement.end.0.checked_add(placement.len() as i64).unwrap());
        let commit = GestureCommit {
            selection: None,
            edit: Some(ArrangementEditIntent {
                expected_revision: controller.revisions().aggregate,
                edit: ArrangementEdit::SetRepeatBoundary {
                    clip_id: occurrence.arrangement_clip,
                    boundary,
                },
            }),
        };
        let ArrangementDispatch::Apply(validated) =
            lower_gesture(controller.snapshot(), commit).unwrap()
        else {
            panic!("repeat edit must produce an aggregate envelope")
        };
        controller.execute(validated.envelope).unwrap();
    }

    fn trigger(events: &[ScheduledEvent]) -> (&TriggerTarget, f32) {
        events
            .iter()
            .find_map(|event| match &event.kind {
                ScheduledKind::Trigger {
                    target, velocity, ..
                } => Some((target, *velocity)),
                _ => None,
            })
            .expect("audition cycle must contain a trigger")
    }

    fn definition_trigger(definition: &PatternDefinition) -> (&TriggerTarget, f32) {
        let PatternContent::Steps(steps) = &definition.content else {
            panic!("expression preview must stay a step pattern")
        };
        steps
            .lanes
            .values()
            .find_map(|lane| {
                lane.steps
                    .values()
                    .next()
                    .map(|event| (&lane.target, event.velocity))
            })
            .expect("expression preview must contain a trigger")
    }

    #[test]
    fn lifecycle_lane_edits_expression_divergence_and_publications_share_one_api() {
        let mut controller = controller();
        let created = create_steps(&mut controller, None);
        assert_eq!(created.mutation, PatternMutationKind::Created);
        assert_eq!(created.uses.as_ref().unwrap().occurrences.len(), 0);
        let pattern = created.pattern;

        let before = created.definition.unwrap();
        let add_lane = workflow_intent(
            &controller,
            PatternAction::Edit(PatternEditIntent {
                pattern,
                expected_pattern_revision: before.revision,
                edit: PatternEdit::AddLane {
                    name: "Kick".into(),
                    target: TriggerTarget::AnalysisTemplate(7),
                    choke_group: None,
                },
            }),
        );
        let lane_publication = published(controller.execute_pattern_workflow(add_lane).unwrap());
        let lane_definition = lane_publication.definition.unwrap();
        let PatternContent::Steps(lanes) = &lane_definition.content else {
            panic!("created step pattern changed kind")
        };
        let lane = *lanes.lanes.keys().next().unwrap();

        let put_step = workflow_intent(
            &controller,
            PatternAction::Edit(PatternEditIntent {
                pattern,
                expected_pattern_revision: lane_definition.revision,
                edit: PatternEdit::PutStep {
                    lane,
                    step: 0,
                    event: StepEvent {
                        velocity: 0.42,
                        probability: 1.0,
                        micro_offset: 0,
                        gate: BeatDuration((PPQ / 4) as u64),
                        ratchets: 1,
                        pitch_semitones: 0.0,
                        pan: 0.0,
                    },
                },
            }),
        );
        let stepped = published(controller.execute_pattern_workflow(put_step).unwrap());
        let stepped_definition = stepped.definition.unwrap();
        let PatternContent::Steps(steps) = &stepped_definition.content else {
            unreachable!()
        };
        assert_eq!(steps.lanes[&lane].steps[&0].velocity, 0.42);

        let expression = workflow_intent(
            &controller,
            PatternAction::Edit(PatternEditIntent {
                pattern,
                expected_pattern_revision: stepped_definition.revision,
                edit: PatternEdit::ApplyExpression {
                    source: "a^0.25".into(),
                    bindings: BTreeMap::from([("a".into(), TriggerTarget::AnalysisTemplate(11))]),
                    overwrite: DivergedOverwrite::Refuse,
                    realization: ExpressionRealizationContext::default(),
                },
            }),
        );
        let generated = published(controller.execute_pattern_workflow(expression).unwrap());
        let generated_definition = generated.definition.unwrap();
        assert!(!generated_definition.origin.diverged());
        let PatternContent::Steps(generated_steps) = &generated_definition.content else {
            unreachable!()
        };
        let generated_lane = *generated_steps.lanes.keys().next().unwrap();
        assert_eq!(
            generated_steps.lanes[&generated_lane].steps[&0].velocity,
            0.25
        );

        let hand_edit = workflow_intent(
            &controller,
            PatternAction::Edit(PatternEditIntent {
                pattern,
                expected_pattern_revision: generated_definition.revision,
                edit: PatternEdit::RemoveStep {
                    lane: generated_lane,
                    step: 0,
                },
            }),
        );
        let diverged = published(controller.execute_pattern_workflow(hand_edit).unwrap());
        let diverged_definition = diverged.definition.unwrap();
        assert!(diverged_definition.origin.diverged());

        let duplicate = workflow_intent(
            &controller,
            PatternAction::Duplicate {
                source: pattern,
                expected_pattern_revision: diverged_definition.revision,
                name: "Beat copy".into(),
            },
        );
        let duplicate = published(controller.execute_pattern_workflow(duplicate).unwrap());
        assert_eq!(
            duplicate.mutation,
            PatternMutationKind::Duplicated { source: pattern }
        );
        assert_ne!(duplicate.pattern, pattern);
        assert!(duplicate.origin.as_ref().unwrap().diverged());

        let delete = workflow_intent(
            &controller,
            PatternAction::Delete {
                pattern: duplicate.pattern,
                expected_pattern_revision: duplicate.definition.as_ref().unwrap().revision,
            },
        );
        let deleted = published(controller.execute_pattern_workflow(delete).unwrap());
        assert_eq!(deleted.mutation, PatternMutationKind::Deleted);
        assert!(deleted.definition.is_none());
        assert!(deleted.reveal.is_none());
    }

    #[test]
    fn placement_preview_audition_and_make_unique_use_real_occurrence_cycles() {
        let mut controller = controller();
        let created = create_steps(&mut controller, None);
        let pattern = created.pattern;
        let expression = workflow_intent(
            &controller,
            PatternAction::Edit(PatternEditIntent {
                pattern,
                expected_pattern_revision: created.definition.unwrap().revision,
                edit: PatternEdit::ApplyExpression {
                    source: "<a^0.25 b^0.75>".into(),
                    bindings: BTreeMap::from([
                        ("a".into(), TriggerTarget::AnalysisTemplate(7)),
                        ("b".into(), TriggerTarget::AnalysisTemplate(9)),
                    ]),
                    overwrite: DivergedOverwrite::Refuse,
                    realization: ExpressionRealizationContext::default(),
                },
            }),
        );
        published(controller.execute_pattern_workflow(expression).unwrap());
        let occurrence = place_pattern(&mut controller, pattern, Frame::ZERO);
        repeat_for_two_cycles(&mut controller, occurrence);

        let target = PatternUseGraph::build(PatternUseSnapshot::from_project(
            &controller.snapshot().project,
        ))
        .unwrap()
        .occurrence_for_clip(occurrence.arrangement_clip)
        .unwrap()
        .target;

        let preview = |controller: &mut ProjectController, cycle_index| {
            let intent = workflow_intent(
                controller,
                PatternAction::PreviewCycle {
                    target: PatternEditorTarget::new(pattern, PatternEditorMode::Steps),
                    cycle_index,
                    performance_seed: 17,
                },
            );
            let PatternWorkflowOutcome::Preview(publication) =
                controller.execute_pattern_workflow(intent).unwrap()
            else {
                panic!("expected a cycle preview")
            };
            publication
        };
        let first_preview = preview(&mut controller, 0);
        let second_preview = preview(&mut controller, 1);
        assert_eq!(
            definition_trigger(&first_preview.definition),
            (&TriggerTarget::AnalysisTemplate(7), 0.25)
        );
        assert_eq!(
            definition_trigger(&second_preview.definition),
            (&TriggerTarget::AnalysisTemplate(9), 0.75)
        );

        let audition = |controller: &mut ProjectController, cycle_index| {
            let intent = PatternWorkflowIntent::Audition(PatternLoopAuditionIntent {
                expected_project_revision: controller.revisions().aggregate,
                occurrence: target,
                cycle_index,
                performance_seed: 17,
            });
            let PatternWorkflowOutcome::Audition(plan) =
                controller.execute_pattern_workflow(intent).unwrap()
            else {
                panic!("expected an audition plan")
            };
            plan
        };
        let first = audition(&mut controller, 0);
        let second = audition(&mut controller, 1);
        assert_eq!(
            trigger(&first.events),
            (&TriggerTarget::AnalysisTemplate(7), 0.25)
        );
        assert_eq!(
            trigger(&second.events),
            (&TriggerTarget::AnalysisTemplate(9), 0.75)
        );
        assert_eq!(first.loop_range.end, second.loop_range.start);
        assert_eq!(first.loop_range.end.0 - first.loop_range.start.0, 96_000);

        // A second use makes the first placement eligible for Make Unique.
        place_pattern(&mut controller, pattern, Frame(192_000));

        let unique = PatternWorkflowIntent::MakeOccurrenceUnique(MakeOccurrenceUniqueIntent {
            expected_project_revision: controller.revisions().aggregate,
            occurrence: target,
            name: Some("Unique loop".into()),
        });
        let unique = published(controller.execute_pattern_workflow(unique).unwrap());
        assert!(matches!(
            unique.mutation,
            PatternMutationKind::MadeUnique {
                source,
                occurrence
            } if source == pattern && occurrence.pattern == unique.pattern
        ));
        assert_ne!(unique.pattern, pattern);
        assert_eq!(unique.uses.as_ref().unwrap().occurrences.len(), 1);
        assert_eq!(
            unique.reveal.as_ref().unwrap().arrangement_selection.clips,
            BTreeSet::from([target.arrangement_clip])
        );
    }

    #[test]
    fn continuous_step_edits_use_one_controller_gesture_and_one_undo_unit() {
        let mut controller = controller();
        let created = create_steps(&mut controller, Some(TriggerTarget::AnalysisTemplate(7)));
        let pattern = created.pattern;
        let definition = created.definition.unwrap();
        let PatternContent::Steps(steps) = &definition.content else {
            unreachable!()
        };
        let lane = *steps.lanes.keys().next().unwrap();
        let began = controller
            .execute_pattern_workflow(PatternWorkflowIntent::BeginGesture(
                BeginPatternGestureIntent {
                    expected_project_revision: controller.revisions().aggregate,
                    editor_session: 44,
                    pattern,
                    kind: PatternGestureKind::AdjustEvent,
                },
            ))
            .unwrap();
        let PatternWorkflowOutcome::GestureBegan(receipt) = began else {
            panic!("expected gesture receipt")
        };

        let event = |velocity| StepEvent {
            velocity,
            probability: 1.0,
            micro_offset: 0,
            gate: BeatDuration((PPQ / 4) as u64),
            ratchets: 1,
            pitch_semitones: 0.0,
            pan: 0.0,
        };
        let edit = |controller: &ProjectController, revision, velocity| PatternActionIntent {
            expected_project_revision: controller.revisions().aggregate,
            action: PatternAction::Edit(PatternEditIntent {
                pattern,
                expected_pattern_revision: revision,
                edit: PatternEdit::PutStep {
                    lane,
                    step: 0,
                    event: event(velocity),
                },
            }),
        };
        let first_action = edit(&controller, definition.revision, 0.25);
        let first = controller
            .execute_pattern_workflow(PatternWorkflowIntent::GestureEdit {
                receipt: receipt.clone(),
                action: first_action,
            })
            .unwrap();
        let first_revision = published(first).definition.unwrap().revision;
        let second_action = edit(&controller, first_revision, 0.75);
        let second = controller
            .execute_pattern_workflow(PatternWorkflowIntent::GestureEdit {
                receipt: receipt.clone(),
                action: second_action,
            })
            .unwrap();
        let second = published(second);
        let PatternContent::Steps(steps) = &second.definition.unwrap().content else {
            unreachable!()
        };
        assert_eq!(steps.lanes[&lane].steps[&0].velocity, 0.75);
        assert!(matches!(
            controller
                .execute_pattern_workflow(PatternWorkflowIntent::EndGesture(receipt))
                .unwrap(),
            PatternWorkflowOutcome::GestureEnded
        ));

        controller.undo().unwrap();
        let restored = controller
            .snapshot()
            .project
            .state()
            .domains
            .sequencer
            .patterns()
            .get(pattern)
            .unwrap();
        let PatternContent::Steps(steps) = &restored.content else {
            unreachable!()
        };
        assert!(steps.lanes[&lane].steps.is_empty());
    }

    #[test]
    fn editor_hydration_preserves_exact_occurrence_use_and_reveal_targets() {
        let mut controller = controller();
        let created = create_steps(&mut controller, None);
        let pattern = created.pattern;
        let occurrence = place_pattern(&mut controller, pattern, Frame::ZERO);
        let target = PatternEditorTarget::new(pattern, PatternEditorMode::Steps);
        let hydration = hydrate_pattern_editor(
            PatternUseSnapshot::from_project(&controller.snapshot().project),
            target,
            Some(occurrence),
        )
        .unwrap();
        assert_eq!(hydration.target, target);
        assert_eq!(hydration.occurrence, Some(occurrence));
        assert_eq!(hydration.uses.occurrences.len(), 1);
        assert_eq!(
            hydration.reveal.arrangement_selection.clips,
            BTreeSet::from([occurrence.arrangement_clip])
        );
        assert!(matches!(
            hydrate_pattern_editor(
                PatternUseSnapshot::from_project(&controller.snapshot().project),
                PatternEditorTarget::new(pattern, PatternEditorMode::PianoRoll),
                Some(occurrence),
            ),
            Err(PatternWorkflowError::EditorModeMismatch { .. })
        ));
    }

    #[test]
    fn piano_roll_note_edits_publish_exact_events() {
        let mut controller = controller();
        let create = workflow_intent(
            &controller,
            PatternAction::Create(CreatePatternIntent {
                mode: PatternEditorMode::PianoRoll,
                name: "Phrase".into(),
                length: BeatDuration((PPQ * 4) as u64),
                step_resolution: BeatDuration((PPQ / 4) as u64),
                initial_target: None,
            }),
        );
        let created = published(controller.execute_pattern_workflow(create).unwrap());
        let definition = created.definition.unwrap();
        let note = crate::sequencer::NoteEvent {
            id: crate::sequencer::NoteId::from_raw(1),
            start: crate::sequencer::BeatTime(0),
            duration: BeatDuration(PPQ as u64),
            pitch: crate::sequencer::NotePitch {
                midi_key: 60,
                cents: 0.0,
            },
            velocity: 0.7,
            release_velocity: 0.5,
            pan: 0.0,
            probability: 1.0,
            micro_offset: 0,
            channel: 0,
            instrument: Some(7),
            articulation: crate::sequencer::Articulation::Normal,
            expression: crate::sequencer::PerNoteExpression::default(),
        };
        let put = workflow_intent(
            &controller,
            PatternAction::Edit(PatternEditIntent {
                pattern: definition.id,
                expected_pattern_revision: definition.revision,
                edit: PatternEdit::PutNote { note: note.clone() },
            }),
        );
        let put = published(controller.execute_pattern_workflow(put).unwrap());
        let put_definition = put.definition.unwrap();
        let PatternContent::Notes(notes) = &put_definition.content else {
            unreachable!()
        };
        assert_eq!(notes.notes[&note.id], note);

        let remove = workflow_intent(
            &controller,
            PatternAction::Edit(PatternEditIntent {
                pattern: put_definition.id,
                expected_pattern_revision: put_definition.revision,
                edit: PatternEdit::RemoveNote { note: note.id },
            }),
        );
        let removed = published(controller.execute_pattern_workflow(remove).unwrap());
        let PatternContent::Notes(notes) = &removed.definition.unwrap().content else {
            unreachable!()
        };
        assert!(notes.notes.is_empty());
    }
}
