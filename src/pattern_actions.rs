//! Controller-facing intents emitted by the pattern editor.
//!
//! The GPUI surface may render a shared sequencer snapshot, but aggregate
//! project mutation crosses this typed seam. Stable domain IDs and optimistic
//! revisions make the intents safe to route through `ProjectController`;
//! runtime entities, picker indexes, and mutex guards never escape the view.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::pattern_authoring::{DivergedOverwrite, ExpressionRealizationContext};
use crate::sample_kit::SampleTargetRef;
use crate::sequencer::{
    BeatDuration, NoteEvent, NoteId, PatternContent, PatternDefinition, PatternId, StepEvent,
    StepLaneId, TriggerTarget,
};
pub use crate::workspace_document::PatternEditorMode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PatternEditorTarget {
    pub pattern: PatternId,
    pub mode: PatternEditorMode,
}

impl PatternEditorTarget {
    pub const fn new(pattern: PatternId, mode: PatternEditorMode) -> Self {
        Self { pattern, mode }
    }

    pub const fn from_raw(pattern: u64, mode: PatternEditorMode) -> Self {
        Self::new(PatternId::from_raw(pattern), mode)
    }

    pub fn from_definition(definition: &PatternDefinition) -> Self {
        Self {
            pattern: definition.id,
            mode: match &definition.content {
                PatternContent::Notes(_) => PatternEditorMode::PianoRoll,
                PatternContent::Steps(_) => PatternEditorMode::Steps,
            },
        }
    }
}

/// Create and duplicate always land on the new definition, never the previous
/// editor target.
pub const fn editor_target_after_create(
    pattern: PatternId,
    mode: PatternEditorMode,
) -> PatternEditorTarget {
    PatternEditorTarget { pattern, mode }
}

pub const fn editor_target_after_duplicate(
    pattern: PatternId,
    mode: PatternEditorMode,
) -> PatternEditorTarget {
    editor_target_after_create(pattern, mode)
}

/// A refused delete must keep the current target, including when the refused
/// identity is unrelated to what the editor is showing.
pub fn editor_target_after_delete(
    current: Option<PatternEditorTarget>,
    deleted: PatternId,
    refused: bool,
) -> Option<PatternEditorTarget> {
    if refused {
        return current;
    }
    current.filter(|target| target.pattern != deleted)
}

/// A stable, controller-supplied option for expression bindings and initial
/// step lanes. The label is presentation metadata; selection uses `target`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TriggerTargetOption {
    pub target: TriggerTarget,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreatePatternIntent {
    pub mode: PatternEditorMode,
    pub name: String,
    pub length: BeatDuration,
    pub step_resolution: BeatDuration,
    pub initial_target: Option<TriggerTarget>,
}

/// One revision-guarded edit to an existing definition.
#[derive(Clone, Debug, PartialEq)]
pub struct PatternEditIntent {
    pub pattern: PatternId,
    pub expected_pattern_revision: u64,
    pub edit: PatternEdit,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PatternEdit {
    /// Manual piano-roll/step-grid or quantize result. Sequencer command
    /// preparation is responsible for marking generated origins divergent.
    ReplaceContent(PatternContent),
    SetSwing(f32),
    AddLane {
        name: String,
        target: TriggerTarget,
        choke_group: Option<u32>,
    },
    RemoveLane {
        lane: StepLaneId,
    },
    RenameLane {
        lane: StepLaneId,
        name: String,
    },
    SetLaneTarget {
        lane: StepLaneId,
        target: TriggerTarget,
    },
    /// Resolve or allocate the durable sequencer alias for one exact sampler
    /// zone, then retarget this lane in the same aggregate undo unit.
    MapLaneToPad {
        lane: StepLaneId,
        target: SampleTargetRef,
    },
    SetLaneChokeGroup {
        lane: StepLaneId,
        choke_group: Option<u32>,
    },
    PutNote {
        note: NoteEvent,
    },
    RemoveNote {
        note: NoteId,
    },
    PutStep {
        lane: StepLaneId,
        step: u32,
        event: StepEvent,
    },
    RemoveStep {
        lane: StepLaneId,
        step: u32,
    },
    MoveStep {
        from_lane: StepLaneId,
        from_step: u32,
        to_lane: StepLaneId,
        to_step: u32,
    },
    ApplyExpression {
        source: String,
        bindings: BTreeMap<String, TriggerTarget>,
        overwrite: DivergedOverwrite,
        /// Exact placement context visible when Apply was pressed. This only
        /// chooses the cached grid; runtime scheduling still regenerates each
        /// placement from source and bindings.
        realization: ExpressionRealizationContext,
    },
}

/// Semantic output from a targetable pattern workspace item.
#[derive(Clone, Debug, PartialEq)]
pub enum PatternAction {
    Create(CreatePatternIntent),
    Delete {
        pattern: PatternId,
        expected_pattern_revision: u64,
    },
    Duplicate {
        source: PatternId,
        expected_pattern_revision: u64,
        name: String,
    },
    Edit(PatternEditIntent),
    Undo,
    Redo,
    /// The workspace host should update this item's durable descriptor target.
    Retarget(PatternEditorTarget),
    /// View-local, but emitted so linked/floating workspace hosts can preserve
    /// or synchronize the exact placement-cycle being inspected.
    PreviewCycle {
        target: PatternEditorTarget,
        cycle_index: u64,
        performance_seed: u64,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct PatternActionIntent {
    pub expected_project_revision: u64,
    pub action: PatternAction,
}

pub type PatternActionCallback = Arc<dyn Fn(PatternActionIntent) + Send + Sync + 'static>;

impl PatternEditIntent {
    pub fn replace_content(before: &PatternDefinition, content: PatternContent) -> Self {
        Self {
            pattern: before.id,
            expected_pattern_revision: before.revision,
            edit: PatternEdit::ReplaceContent(content),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sequencer::{NotePattern, PatternOrigin};

    #[test]
    fn replacement_intent_keeps_stable_identity_and_revision() {
        let before = PatternDefinition {
            id: PatternId::from_raw(17),
            name: "Bass".into(),
            length: BeatDuration(3_840),
            content: PatternContent::Notes(NotePattern::default()),
            origin: PatternOrigin::Authored,
            revision: 9,
        };
        let intent = PatternEditIntent::replace_content(
            &before,
            PatternContent::Notes(NotePattern::default()),
        );
        assert_eq!(intent.pattern, PatternId::from_raw(17));
        assert_eq!(intent.expected_pattern_revision, 9);
    }

    #[test]
    fn create_and_duplicate_change_target_pattern_to_the_new_id() {
        let previous = PatternEditorTarget::from_raw(1, PatternEditorMode::Steps);
        let created = PatternId::from_raw(8);
        let target = editor_target_after_create(created, PatternEditorMode::Steps);
        assert_eq!(target.pattern, created);
        assert_ne!(target.pattern, previous.pattern);

        let duplicated = PatternId::from_raw(9);
        let target = editor_target_after_duplicate(duplicated, previous.mode);
        assert_eq!(target.pattern, duplicated);
        assert_ne!(target.pattern, previous.pattern);

        let definition = PatternDefinition {
            id: PatternId::from_raw(22),
            name: "Copy".into(),
            length: BeatDuration(3_840),
            content: PatternContent::Notes(NotePattern::default()),
            origin: PatternOrigin::Authored,
            revision: 0,
        };
        let from_definition = PatternEditorTarget::from_definition(&definition);
        assert_eq!(from_definition.pattern, definition.id);
        assert_eq!(from_definition.mode, PatternEditorMode::PianoRoll);
    }

    #[test]
    fn refused_delete_does_not_clear_an_unrelated_target() {
        let current = PatternEditorTarget::from_raw(4, PatternEditorMode::PianoRoll);
        let unrelated = PatternId::from_raw(12);
        assert_eq!(
            editor_target_after_delete(Some(current), unrelated, true),
            Some(current)
        );
        assert_eq!(
            editor_target_after_delete(Some(current), current.pattern, true),
            Some(current)
        );
        assert_eq!(
            editor_target_after_delete(Some(current), unrelated, false),
            Some(current)
        );
        assert_eq!(
            editor_target_after_delete(Some(current), current.pattern, false),
            None
        );
    }
}
