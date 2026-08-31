//! Controller-facing intents emitted by the pattern editor.
//!
//! The GPUI surface may render a shared sequencer snapshot, but aggregate
//! project mutation crosses this typed seam. Stable domain IDs and optimistic
//! revisions make the intents safe to route through `ProjectController`;
//! runtime entities, picker indexes, and mutex guards never escape the view.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::pattern_authoring::DivergedOverwrite;
use crate::sequencer::{BeatDuration, PatternContent, PatternDefinition, PatternId, TriggerTarget};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatternEditorMode {
    PianoRoll,
    Steps,
}

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
    ApplyExpression {
        source: String,
        bindings: BTreeMap<String, TriggerTarget>,
        overwrite: DivergedOverwrite,
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
}
