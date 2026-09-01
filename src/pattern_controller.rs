//! Pure lowering from pattern-editor intent to the aggregate command language.
//!
//! This module neither owns a controller nor mutates its snapshot. A session
//! captures one immutable [`DawProject`], lowers one UI gesture, and either
//! executes the returned envelope once or routes the explicit history/view
//! directive. Concrete IDs, provenance, divergence and optimistic before
//! values are therefore journal-ready before mutation begins.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::arrangement;
use crate::command::{claims_for_commands, BindingCommand, CommandEnvelope, DomainCommand};
use crate::daw_project::{DawProject, ProjectState};
use crate::pattern_actions::{
    CreatePatternIntent, PatternAction, PatternActionIntent, PatternEdit, PatternEditorMode,
    PatternEditorTarget,
};
use crate::pattern_authoring::{self, PatternAuthoringError};
use crate::sequencer::{
    NotePattern, PatternContent, PatternDefinition, PatternId, PatternOrigin, SampleAssetId,
    Sequencer, SequencerCommand, StepLane, StepLaneId, StepPattern, TriggerTarget,
};

/// Immutable facts needed to lower a pattern action. This deliberately borrows
/// aggregate state instead of accepting a shared editor lock.
#[derive(Clone, Copy)]
pub struct PatternActionSnapshot<'a> {
    pub aggregate_revision: u64,
    pub state: &'a ProjectState,
}

impl<'a> PatternActionSnapshot<'a> {
    pub fn from_project(project: &'a DawProject) -> Self {
        Self {
            aggregate_revision: project.revisions().aggregate,
            state: project.state(),
        }
    }
}

/// Exact routing decision for `ProjectSession` or another controller host.
#[derive(Clone, Debug, PartialEq)]
pub enum LoweredPatternAction {
    Execute(CommandEnvelope),
    Undo,
    Redo,
    Retarget(PatternEditorTarget),
    PreviewCycle {
        target: PatternEditorTarget,
        cycle_index: u64,
        performance_seed: u64,
    },
}

impl LoweredPatternAction {
    /// The Pattern a successful create or duplicate should make the editor
    /// target. Edits, deletes, and view directives do not introduce a new id.
    pub fn created_editor_target(&self) -> Option<PatternEditorTarget> {
        let Self::Execute(envelope) = self else {
            return None;
        };
        envelope.commands.iter().find_map(|command| {
            let DomainCommand::Sequencer(SequencerCommand::PutPattern {
                before: None,
                after: Some(pattern),
            }) = command
            else {
                return None;
            };
            Some(PatternEditorTarget::from_definition(pattern))
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternReferenceSummary {
    pub sequencer_clips: Vec<crate::sequencer::PatternClipId>,
    pub arrangement_aliases: Vec<arrangement::PatternId>,
    pub has_air_link: bool,
}

impl PatternReferenceSummary {
    fn is_empty(&self) -> bool {
        self.sequencer_clips.is_empty() && self.arrangement_aliases.is_empty() && !self.has_air_link
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PatternLoweringError {
    ProjectRevisionConflict {
        expected: u64,
        actual: u64,
    },
    MissingPattern(PatternId),
    PatternRevisionConflict {
        pattern: PatternId,
        expected: u64,
        actual: u64,
    },
    ModeMismatch {
        pattern: PatternId,
        requested: PatternEditorMode,
    },
    PatternInUse {
        pattern: PatternId,
        references: PatternReferenceSummary,
    },
    EmptyName,
    InvalidEdit(&'static str),
    MissingLane(StepLaneId),
    MissingStep {
        lane: StepLaneId,
        step: u32,
    },
    MissingNote(crate::sequencer::NoteId),
    EmptyLaneName,
    MissingSampleTarget(crate::sample_kit::SampleTargetRef),
    IdentityExhausted(&'static str),
    RevisionExhausted(PatternId),
    NoChange(PatternId),
    InvalidPattern(String),
    Authoring(PatternAuthoringError),
}

impl fmt::Display for PatternLoweringError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectRevisionConflict { expected, actual } => write!(
                formatter,
                "pattern action expected project revision {expected}, current revision is {actual}"
            ),
            Self::MissingPattern(pattern) => {
                write!(formatter, "pattern #{} no longer exists", pattern.get())
            }
            Self::PatternRevisionConflict {
                pattern,
                expected,
                actual,
            } => write!(
                formatter,
                "pattern #{} expected revision {expected}, current revision is {actual}",
                pattern.get()
            ),
            Self::ModeMismatch { pattern, requested } => write!(
                formatter,
                "pattern #{} is incompatible with {requested:?}",
                pattern.get()
            ),
            Self::PatternInUse {
                pattern,
                references,
            } => write!(
                formatter,
                "pattern #{} is still referenced by {} placement(s), {} arrangement alias(es), AIR link: {}",
                pattern.get(),
                references.sequencer_clips.len(),
                references.arrangement_aliases.len(),
                references.has_air_link
            ),
            Self::EmptyName => formatter.write_str("pattern name must not be empty"),
            Self::InvalidEdit(message) => formatter.write_str(message),
            Self::MissingLane(lane) => write!(formatter, "step lane {} is missing", lane.get()),
            Self::MissingStep { lane, step } => {
                write!(formatter, "step {step} is missing from lane {}", lane.get())
            }
            Self::MissingNote(note) => write!(formatter, "note #{} is missing", note.get()),
            Self::EmptyLaneName => formatter.write_str("step lane name must not be empty"),
            Self::MissingSampleTarget(target) => write!(
                formatter,
                "sampler target kit {}/pad {}/zone {} is missing",
                target.kit.get(),
                target.pad.get(),
                target.zone.get()
            ),
            Self::IdentityExhausted(kind) => write!(formatter, "{kind} identity is exhausted"),
            Self::RevisionExhausted(pattern) => {
                write!(formatter, "pattern #{} revision is exhausted", pattern.get())
            }
            Self::NoChange(pattern) => {
                write!(formatter, "pattern #{} edit makes no change", pattern.get())
            }
            Self::InvalidPattern(message) => write!(formatter, "invalid lowered pattern: {message}"),
            Self::Authoring(error) => error.fmt(formatter),
        }
    }
}

impl Error for PatternLoweringError {}

impl From<PatternAuthoringError> for PatternLoweringError {
    fn from(value: PatternAuthoringError) -> Self {
        Self::Authoring(value)
    }
}

/// Lower one user gesture against one frozen project publication.
///
/// `Execute` always contains exactly one atomic envelope with coalescing
/// disabled, so a controller records one undo unit for the gesture.
pub fn lower_pattern_action(
    snapshot: PatternActionSnapshot<'_>,
    intent: &PatternActionIntent,
) -> Result<LoweredPatternAction, PatternLoweringError> {
    if intent.expected_project_revision != snapshot.aggregate_revision {
        return Err(PatternLoweringError::ProjectRevisionConflict {
            expected: intent.expected_project_revision,
            actual: snapshot.aggregate_revision,
        });
    }
    match &intent.action {
        PatternAction::Create(create) => lower_create(snapshot, create),
        PatternAction::Delete {
            pattern,
            expected_pattern_revision,
        } => lower_delete(snapshot, *pattern, *expected_pattern_revision),
        PatternAction::Duplicate {
            source,
            expected_pattern_revision,
            name,
        } => lower_duplicate(snapshot, *source, *expected_pattern_revision, name),
        PatternAction::Edit(edit) => lower_edit(snapshot, edit),
        PatternAction::Undo => Ok(LoweredPatternAction::Undo),
        PatternAction::Redo => Ok(LoweredPatternAction::Redo),
        PatternAction::Retarget(target) => {
            require_target(snapshot.state, *target)?;
            Ok(LoweredPatternAction::Retarget(*target))
        }
        PatternAction::PreviewCycle {
            target,
            cycle_index,
            performance_seed,
        } => {
            require_target(snapshot.state, *target)?;
            Ok(LoweredPatternAction::PreviewCycle {
                target: *target,
                cycle_index: *cycle_index,
                performance_seed: *performance_seed,
            })
        }
    }
}

fn lower_create(
    snapshot: PatternActionSnapshot<'_>,
    create: &CreatePatternIntent,
) -> Result<LoweredPatternAction, PatternLoweringError> {
    require_name(&create.name)?;
    let mut allocator = snapshot.state.domains.sequencer.clone();
    let id = allocator.allocate_pattern_id();
    let content = match create.mode {
        PatternEditorMode::PianoRoll => PatternContent::Notes(NotePattern::default()),
        PatternEditorMode::Steps => {
            let mut lanes = BTreeMap::new();
            if let Some(target) = create.initial_target.clone() {
                let lane = allocator.allocate_step_lane_id();
                lanes.insert(
                    lane,
                    StepLane {
                        id: lane,
                        name: "Lane 1".into(),
                        target,
                        choke_group: None,
                        steps: BTreeMap::new(),
                    },
                );
            }
            PatternContent::Steps(StepPattern {
                resolution: create.step_resolution,
                swing: 0.0,
                lanes,
            })
        }
    };
    let after = PatternDefinition {
        id,
        name: create.name.trim().to_owned(),
        length: create.length,
        content,
        origin: PatternOrigin::Authored,
        revision: 0,
    };
    validate(&after)?;
    Ok(execute(
        snapshot.aggregate_revision,
        "Create pattern",
        SequencerCommand::PutPattern {
            before: None,
            after: Some(after),
        },
    ))
}

fn lower_delete(
    snapshot: PatternActionSnapshot<'_>,
    pattern: PatternId,
    expected_revision: u64,
) -> Result<LoweredPatternAction, PatternLoweringError> {
    let before = require_pattern(snapshot.state, pattern, expected_revision)?.clone();
    let references = references_to(snapshot.state, pattern);
    if !references.is_empty() {
        return Err(PatternLoweringError::PatternInUse {
            pattern,
            references,
        });
    }
    Ok(execute(
        snapshot.aggregate_revision,
        "Delete pattern",
        SequencerCommand::PutPattern {
            before: Some(before),
            after: None,
        },
    ))
}

fn lower_duplicate(
    snapshot: PatternActionSnapshot<'_>,
    source: PatternId,
    expected_revision: u64,
    name: &str,
) -> Result<LoweredPatternAction, PatternLoweringError> {
    require_name(name)?;
    let before = require_pattern(snapshot.state, source, expected_revision)?;
    let mut allocator = snapshot.state.domains.sequencer.clone();
    let mut after = before.clone();
    after.id = allocator.allocate_pattern_id();
    after.name = name.trim().to_owned();
    after.revision = 0;
    // Live expression lanes are binding-table ordinals by language contract;
    // preserving them keeps the cached grid identical to later scheduling.
    // Authored and diverged grids instead receive fresh sequencer identities.
    if !matches!(
        &after.origin,
        PatternOrigin::Expression {
            diverged: false,
            ..
        }
    ) {
        after.content = remap_child_ids(after.content, &mut allocator);
    }
    validate(&after)?;
    Ok(execute(
        snapshot.aggregate_revision,
        "Duplicate pattern",
        SequencerCommand::PutPattern {
            before: None,
            after: Some(after),
        },
    ))
}

fn lower_edit(
    snapshot: PatternActionSnapshot<'_>,
    intent: &crate::pattern_actions::PatternEditIntent,
) -> Result<LoweredPatternAction, PatternLoweringError> {
    let before = require_pattern(
        snapshot.state,
        intent.pattern,
        intent.expected_pattern_revision,
    )?
    .clone();
    let revision = before
        .revision
        .checked_add(1)
        .ok_or(PatternLoweringError::RevisionExhausted(before.id))?;
    let mut prefix_commands = Vec::new();
    let mut after = match &intent.edit {
        PatternEdit::ReplaceContent(content) => {
            if content_mode(content) != content_mode(&before.content) {
                return Err(PatternLoweringError::InvalidEdit(
                    "a grid edit cannot change the pattern editor mode",
                ));
            }
            let mut after = before.clone();
            let mut allocator = snapshot.state.domains.sequencer.clone();
            after.content = canonicalize_new_child_ids(&before.content, content, &mut allocator);
            if after.content != before.content {
                after.origin.mark_diverged();
            }
            after
        }
        PatternEdit::SetSwing(swing) => {
            let mut after = before.clone();
            let PatternContent::Steps(steps) = &mut after.content else {
                return Err(PatternLoweringError::InvalidEdit(
                    "swing applies only to step patterns",
                ));
            };
            steps.swing = *swing;
            if after.content != before.content {
                after.origin.mark_diverged();
            }
            after
        }
        PatternEdit::AddLane {
            name,
            target,
            choke_group,
        } => {
            require_lane_name(name)?;
            let mut after = before.clone();
            let mut allocator = snapshot.state.domains.sequencer.clone();
            let lane = allocator.allocate_step_lane_id();
            if lane.get() == u64::MAX {
                return Err(PatternLoweringError::IdentityExhausted("step lane"));
            }
            let steps = require_step_pattern_mut(&mut after)?;
            steps.lanes.insert(
                lane,
                StepLane {
                    id: lane,
                    name: name.trim().to_owned(),
                    target: target.clone(),
                    choke_group: *choke_group,
                    steps: BTreeMap::new(),
                },
            );
            after.origin.mark_diverged();
            after
        }
        PatternEdit::RemoveLane { lane } => {
            let mut after = before.clone();
            let steps = require_step_pattern_mut(&mut after)?;
            if steps.lanes.remove(lane).is_none() {
                return Err(PatternLoweringError::MissingLane(*lane));
            }
            after.origin.mark_diverged();
            after
        }
        PatternEdit::RenameLane { lane, name } => {
            require_lane_name(name)?;
            let mut after = before.clone();
            require_lane_mut(&mut after, *lane)?.name = name.trim().to_owned();
            if after.content != before.content {
                after.origin.mark_diverged();
            }
            after
        }
        PatternEdit::SetLaneTarget { lane, target } => {
            let mut after = before.clone();
            require_lane_mut(&mut after, *lane)?.target = target.clone();
            if after.content != before.content {
                after.origin.mark_diverged();
            }
            after
        }
        PatternEdit::MapLaneToPad { lane, target } => {
            let (alias, binding) = sample_target_alias(snapshot.state, *target)?;
            if let Some(binding) = binding {
                prefix_commands.push(DomainCommand::Bindings(binding));
            }
            let mut after = before.clone();
            require_lane_mut(&mut after, *lane)?.target = TriggerTarget::Sample(alias);
            if after.content != before.content {
                after.origin.mark_diverged();
            }
            after
        }
        PatternEdit::SetLaneChokeGroup { lane, choke_group } => {
            let mut after = before.clone();
            require_lane_mut(&mut after, *lane)?.choke_group = *choke_group;
            if after.content != before.content {
                after.origin.mark_diverged();
            }
            after
        }
        PatternEdit::PutNote { note } => {
            let mut after = before.clone();
            let notes = require_note_pattern_mut(&mut after)?;
            notes.notes.insert(note.id, note.clone());
            if after.content != before.content {
                after.origin.mark_diverged();
            }
            after
        }
        PatternEdit::RemoveNote { note } => {
            let mut after = before.clone();
            if require_note_pattern_mut(&mut after)?
                .notes
                .remove(note)
                .is_none()
            {
                return Err(PatternLoweringError::MissingNote(*note));
            }
            after.origin.mark_diverged();
            after
        }
        PatternEdit::PutStep { lane, step, event } => {
            let mut after = before.clone();
            require_lane_mut(&mut after, *lane)?
                .steps
                .insert(*step, event.clone());
            if after.content != before.content {
                after.origin.mark_diverged();
            }
            after
        }
        PatternEdit::RemoveStep { lane, step } => {
            let mut after = before.clone();
            if require_lane_mut(&mut after, *lane)?
                .steps
                .remove(step)
                .is_none()
            {
                return Err(PatternLoweringError::MissingStep {
                    lane: *lane,
                    step: *step,
                });
            }
            after.origin.mark_diverged();
            after
        }
        PatternEdit::MoveStep {
            from_lane,
            from_step,
            to_lane,
            to_step,
        } => {
            let mut after = before.clone();
            let steps = require_step_pattern_mut(&mut after)?;
            let event = steps
                .lanes
                .get_mut(from_lane)
                .ok_or(PatternLoweringError::MissingLane(*from_lane))?
                .steps
                .remove(from_step)
                .ok_or(PatternLoweringError::MissingStep {
                    lane: *from_lane,
                    step: *from_step,
                })?;
            let destination = steps
                .lanes
                .get_mut(to_lane)
                .ok_or(PatternLoweringError::MissingLane(*to_lane))?;
            if destination.steps.contains_key(to_step) {
                return Err(PatternLoweringError::InvalidEdit(
                    "step move destination is occupied",
                ));
            }
            destination.steps.insert(*to_step, event);
            after.origin.mark_diverged();
            after
        }
        PatternEdit::ApplyExpression {
            source,
            bindings,
            overwrite,
            realization,
        } => {
            pattern_authoring::apply_expression_with_context(
                &before,
                source,
                bindings.clone(),
                *overwrite,
                *realization,
            )?
            .definition
        }
    };
    // Authoring helpers may precompute the next revision. Compare musical and
    // provenance state at the current revision so a redundant Apply remains a
    // refused no-op rather than an empty undo record.
    after.revision = before.revision;
    if after == before {
        return Err(PatternLoweringError::NoChange(before.id));
    }
    after.revision = revision;
    validate(&after)?;
    let command = SequencerCommand::PutPattern {
        before: Some(before.clone()),
        after: Some(after.clone()),
    };
    if same_expression_regeneration(&before, &after) {
        // `PutPattern` is deliberately conservative: equal provenance plus a
        // changed grid is treated as a hand edit. Split this authoritative
        // regeneration through a valid provenance-neutral state so neither
        // command can be mistaken for a manual mutation. The two commands
        // remain one envelope and therefore one undo/journal unit.
        let mut neutral = before.clone();
        neutral.origin = PatternOrigin::Authored;
        return Ok(execute_commands(
            snapshot.aggregate_revision,
            edit_label(&intent.edit),
            vec![
                SequencerCommand::PutPattern {
                    before: Some(before),
                    after: Some(neutral.clone()),
                },
                SequencerCommand::PutPattern {
                    before: Some(neutral),
                    after: Some(after),
                },
            ],
        ));
    }
    prefix_commands.push(DomainCommand::Sequencer(command));
    Ok(execute_domain_commands(
        snapshot.aggregate_revision,
        edit_label(&intent.edit),
        prefix_commands,
    ))
}

fn execute(
    base_revision: u64,
    label: &'static str,
    command: SequencerCommand,
) -> LoweredPatternAction {
    execute_commands(base_revision, label, vec![command])
}

fn execute_commands(
    base_revision: u64,
    label: &'static str,
    commands: Vec<SequencerCommand>,
) -> LoweredPatternAction {
    let commands = commands
        .into_iter()
        .map(DomainCommand::Sequencer)
        .collect::<Vec<_>>();
    execute_domain_commands(base_revision, label, commands)
}

fn execute_domain_commands(
    base_revision: u64,
    label: &'static str,
    commands: Vec<DomainCommand>,
) -> LoweredPatternAction {
    LoweredPatternAction::Execute(CommandEnvelope {
        label: label.into(),
        base_revision,
        coalesce: None,
        id_claims: claims_for_commands(&commands),
        commands,
    })
}

fn same_expression_regeneration(before: &PatternDefinition, after: &PatternDefinition) -> bool {
    if before.content == after.content {
        return false;
    }
    matches!(
        (&before.origin, &after.origin),
        (
            PatternOrigin::Expression {
                source: before_source,
                term_hash: before_term,
                bindings_hash: before_bindings,
                diverged: false,
                ..
            },
            PatternOrigin::Expression {
                source: after_source,
                term_hash: after_term,
                bindings_hash: after_bindings,
                diverged: false,
                ..
            }
        ) if before_source == after_source
            && before_term == after_term
            && before_bindings == after_bindings
    )
}

fn require_pattern(
    state: &ProjectState,
    pattern: PatternId,
    expected_revision: u64,
) -> Result<&PatternDefinition, PatternLoweringError> {
    let definition = state
        .domains
        .sequencer
        .patterns()
        .get(pattern)
        .ok_or(PatternLoweringError::MissingPattern(pattern))?;
    if definition.revision != expected_revision {
        return Err(PatternLoweringError::PatternRevisionConflict {
            pattern,
            expected: expected_revision,
            actual: definition.revision,
        });
    }
    Ok(definition)
}

fn require_target(
    state: &ProjectState,
    target: PatternEditorTarget,
) -> Result<(), PatternLoweringError> {
    let definition = state
        .domains
        .sequencer
        .patterns()
        .get(target.pattern)
        .ok_or(PatternLoweringError::MissingPattern(target.pattern))?;
    if content_mode(&definition.content) != target.mode {
        return Err(PatternLoweringError::ModeMismatch {
            pattern: target.pattern,
            requested: target.mode,
        });
    }
    Ok(())
}

fn references_to(state: &ProjectState, pattern: PatternId) -> PatternReferenceSummary {
    PatternReferenceSummary {
        sequencer_clips: state
            .domains
            .sequencer
            .clips()
            .filter(|clip| clip.pattern == pattern)
            .map(|clip| clip.id)
            .collect(),
        arrangement_aliases: state
            .bindings
            .patterns
            .definitions
            .iter()
            .filter_map(|(alias, target)| (*target == pattern).then_some(*alias))
            .collect(),
        has_air_link: state.bindings.air.patterns.contains_key(&pattern),
    }
}

fn remap_child_ids(content: PatternContent, allocator: &mut Sequencer) -> PatternContent {
    match content {
        PatternContent::Notes(notes) => {
            let notes = notes
                .notes
                .into_values()
                .map(|mut note| {
                    note.id = allocator.allocate_note_id();
                    (note.id, note)
                })
                .collect();
            PatternContent::Notes(NotePattern { notes })
        }
        PatternContent::Steps(steps) => {
            let lanes = steps
                .lanes
                .into_values()
                .map(|mut lane| {
                    lane.id = allocator.allocate_step_lane_id();
                    (lane.id, lane)
                })
                .collect();
            PatternContent::Steps(StepPattern {
                resolution: steps.resolution,
                swing: steps.swing,
                lanes,
            })
        }
    }
}

/// Editor-local additions may use provisional IDs. Existing identities are
/// stable, while every genuinely new child is allocated from the frozen
/// authoritative high-water mark in deterministic map order.
fn canonicalize_new_child_ids(
    before: &PatternContent,
    proposed: &PatternContent,
    allocator: &mut Sequencer,
) -> PatternContent {
    match (before, proposed) {
        (PatternContent::Notes(before), PatternContent::Notes(proposed)) => {
            let notes = proposed
                .notes
                .values()
                .cloned()
                .map(|mut note| {
                    if !before.notes.contains_key(&note.id) {
                        note.id = allocator.allocate_note_id();
                    }
                    (note.id, note)
                })
                .collect();
            PatternContent::Notes(NotePattern { notes })
        }
        (PatternContent::Steps(before), PatternContent::Steps(proposed)) => {
            let lanes = proposed
                .lanes
                .values()
                .cloned()
                .map(|mut lane| {
                    if !before.lanes.contains_key(&lane.id) {
                        lane.id = allocator.allocate_step_lane_id();
                    }
                    (lane.id, lane)
                })
                .collect();
            PatternContent::Steps(StepPattern {
                resolution: proposed.resolution,
                swing: proposed.swing,
                lanes,
            })
        }
        _ => proposed.clone(),
    }
}

fn content_mode(content: &PatternContent) -> PatternEditorMode {
    match content {
        PatternContent::Notes(_) => PatternEditorMode::PianoRoll,
        PatternContent::Steps(_) => PatternEditorMode::Steps,
    }
}

fn require_step_pattern_mut(
    definition: &mut PatternDefinition,
) -> Result<&mut StepPattern, PatternLoweringError> {
    match &mut definition.content {
        PatternContent::Steps(steps) => Ok(steps),
        PatternContent::Notes(_) => Err(PatternLoweringError::InvalidEdit(
            "step and lane edits require a step pattern",
        )),
    }
}

fn require_note_pattern_mut(
    definition: &mut PatternDefinition,
) -> Result<&mut NotePattern, PatternLoweringError> {
    match &mut definition.content {
        PatternContent::Notes(notes) => Ok(notes),
        PatternContent::Steps(_) => Err(PatternLoweringError::InvalidEdit(
            "note edits require a piano-roll pattern",
        )),
    }
}

fn require_lane_mut(
    definition: &mut PatternDefinition,
    lane: StepLaneId,
) -> Result<&mut StepLane, PatternLoweringError> {
    require_step_pattern_mut(definition)?
        .lanes
        .get_mut(&lane)
        .ok_or(PatternLoweringError::MissingLane(lane))
}

fn require_lane_name(name: &str) -> Result<(), PatternLoweringError> {
    if name.trim().is_empty() {
        Err(PatternLoweringError::EmptyLaneName)
    } else {
        Ok(())
    }
}

fn sample_target_alias(
    state: &ProjectState,
    target: crate::sample_kit::SampleTargetRef,
) -> Result<(SampleAssetId, Option<BindingCommand>), PatternLoweringError> {
    let kit = state
        .domains
        .sample_kits
        .kits
        .get(&target.kit)
        .ok_or(PatternLoweringError::MissingSampleTarget(target))?;
    if kit.zone_for_target(target).is_none() {
        return Err(PatternLoweringError::MissingSampleTarget(target));
    }
    if let Some((alias, _)) = state
        .bindings
        .sample_targets
        .targets
        .iter()
        .find(|(_, candidate)| **candidate == target)
    {
        return Ok((*alias, None));
    }
    let next = state.bindings.allocator_state().next_sequencer_sample;
    if next == 0 || next == u64::MAX {
        return Err(PatternLoweringError::IdentityExhausted(
            "sequencer sample alias",
        ));
    }
    let alias = SampleAssetId::from_raw(next);
    Ok((
        alias,
        Some(BindingCommand::PutSampleTargetAlias {
            alias,
            before: None,
            after: Some(target),
        }),
    ))
}

fn validate(definition: &PatternDefinition) -> Result<(), PatternLoweringError> {
    definition
        .validate()
        .map_err(|error| PatternLoweringError::InvalidPattern(error.to_string()))
}

fn require_name(name: &str) -> Result<(), PatternLoweringError> {
    if name.trim().is_empty() {
        Err(PatternLoweringError::EmptyName)
    } else {
        Ok(())
    }
}

fn edit_label(edit: &PatternEdit) -> &'static str {
    match edit {
        PatternEdit::ReplaceContent(_) => "Edit pattern grid",
        PatternEdit::SetSwing(_) => "Set pattern swing",
        PatternEdit::AddLane { .. } => "Add pattern lane",
        PatternEdit::RemoveLane { .. } => "Remove pattern lane",
        PatternEdit::RenameLane { .. } => "Rename pattern lane",
        PatternEdit::SetLaneTarget { .. } => "Retarget pattern lane",
        PatternEdit::MapLaneToPad { .. } => "Map pattern lane to pad",
        PatternEdit::SetLaneChokeGroup { .. } => "Set lane choke group",
        PatternEdit::PutNote { .. } => "Edit pattern note",
        PatternEdit::RemoveNote { .. } => "Remove pattern note",
        PatternEdit::PutStep { .. } => "Edit pattern step",
        PatternEdit::RemoveStep { .. } => "Remove pattern step",
        PatternEdit::MoveStep { .. } => "Move pattern step",
        PatternEdit::ApplyExpression { .. } => "Apply pattern expression",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ontology::AuditoryIr;
    use crate::pattern_actions::{PatternEditIntent, PatternEditorMode};
    use crate::pattern_authoring::{DivergedOverwrite, ExpressionRealizationContext};
    use crate::project_codecs::{decode_constructive, encode_constructive};
    use crate::project_io::ProjectFile;
    use crate::sequencer::{BeatDuration, PatternOrigin, TriggerTarget, PPQ};

    fn project() -> DawProject {
        DawProject::new("Patterns", 48_000, 120.0).unwrap()
    }

    fn action(project: &DawProject, action: PatternAction) -> PatternActionIntent {
        PatternActionIntent {
            expected_project_revision: project.revisions().aggregate,
            action,
        }
    }

    fn create_action(project: &DawProject, name: &str) -> PatternActionIntent {
        action(
            project,
            PatternAction::Create(CreatePatternIntent {
                mode: PatternEditorMode::Steps,
                name: name.into(),
                length: BeatDuration((PPQ * 4) as u64),
                step_resolution: BeatDuration((PPQ / 4) as u64),
                initial_target: Some(TriggerTarget::AnalysisTemplate(7)),
            }),
        )
    }

    fn envelope(value: LoweredPatternAction) -> CommandEnvelope {
        match value {
            LoweredPatternAction::Execute(envelope) => envelope,
            other => panic!("expected envelope, got {other:?}"),
        }
    }

    fn only_pattern(project: &DawProject) -> PatternDefinition {
        project
            .state()
            .domains
            .sequencer
            .patterns()
            .patterns()
            .next()
            .unwrap()
            .clone()
    }

    #[test]
    fn create_is_deterministic_replayable_and_one_undo_unit() {
        let mut first = project();
        let mut replay = project();
        let intent = create_action(&first, "Beat");
        let lowered_a = envelope(
            lower_pattern_action(PatternActionSnapshot::from_project(&first), &intent).unwrap(),
        );
        let lowered_b = envelope(
            lower_pattern_action(PatternActionSnapshot::from_project(&first), &intent).unwrap(),
        );
        assert_eq!(lowered_a, lowered_b);
        assert_eq!(lowered_a.commands.len(), 1);
        assert!(lowered_a.coalesce.is_none());

        let applied = lowered_a.clone().apply(&mut first).unwrap();
        CommandEnvelope::from_batch(0, lowered_a.as_batch())
            .apply(&mut replay)
            .unwrap();
        assert_eq!(only_pattern(&first), only_pattern(&replay));
        assert_eq!(
            first.state().domains.sequencer.allocator_state(),
            replay.state().domains.sequencer.allocator_state()
        );

        applied.inverse.apply(&mut first).unwrap();
        assert_eq!(
            first.state().domains.sequencer.patterns().patterns().len(),
            0
        );
        assert_eq!(
            first
                .state()
                .domains
                .sequencer
                .allocator_state()
                .next_pattern_id,
            2
        );
    }

    #[test]
    fn create_exposes_the_new_editor_target() {
        let mut project = project();
        let lowered = lower_pattern_action(
            PatternActionSnapshot::from_project(&project),
            &create_action(&project, "Beat"),
        )
        .unwrap();
        let target = lowered.created_editor_target().expect("create target");
        envelope(lowered).apply(&mut project).unwrap();
        assert_eq!(target.pattern, only_pattern(&project).id);
        assert_eq!(target.mode, PatternEditorMode::Steps);
    }

    #[test]
    fn stale_project_and_pattern_revisions_are_refused_before_lowering() {
        let mut project = project();
        let mut stale = create_action(&project, "Beat");
        stale.expected_project_revision = 99;
        assert!(matches!(
            lower_pattern_action(PatternActionSnapshot::from_project(&project), &stale),
            Err(PatternLoweringError::ProjectRevisionConflict { .. })
        ));

        envelope(
            lower_pattern_action(
                PatternActionSnapshot::from_project(&project),
                &create_action(&project, "Beat"),
            )
            .unwrap(),
        )
        .apply(&mut project)
        .unwrap();
        let pattern = only_pattern(&project);
        let edit = action(
            &project,
            PatternAction::Edit(PatternEditIntent {
                pattern: pattern.id,
                expected_pattern_revision: pattern.revision + 1,
                edit: PatternEdit::SetSwing(0.5),
            }),
        );
        assert!(matches!(
            lower_pattern_action(PatternActionSnapshot::from_project(&project), &edit),
            Err(PatternLoweringError::PatternRevisionConflict { .. })
        ));
    }

    #[test]
    fn duplicate_claims_fresh_pattern_and_child_identities() {
        let mut project = project();
        envelope(
            lower_pattern_action(
                PatternActionSnapshot::from_project(&project),
                &create_action(&project, "Original"),
            )
            .unwrap(),
        )
        .apply(&mut project)
        .unwrap();
        let source = only_pattern(&project);
        let duplicate = action(
            &project,
            PatternAction::Duplicate {
                source: source.id,
                expected_pattern_revision: source.revision,
                name: "Copy".into(),
            },
        );
        let lowered_action =
            lower_pattern_action(PatternActionSnapshot::from_project(&project), &duplicate)
                .unwrap();
        let copy_target = lowered_action.created_editor_target().unwrap();
        let lowered = envelope(lowered_action);
        let DomainCommand::Sequencer(SequencerCommand::PutPattern {
            before: None,
            after: Some(copy),
        }) = &lowered.commands[0]
        else {
            panic!("expected pattern creation")
        };
        let source_lanes = match &source.content {
            PatternContent::Steps(steps) => steps.lanes.keys().copied().collect::<Vec<_>>(),
            PatternContent::Notes(_) => unreachable!(),
        };
        let copy_lanes = match &copy.content {
            PatternContent::Steps(steps) => steps.lanes.keys().copied().collect::<Vec<_>>(),
            PatternContent::Notes(_) => unreachable!(),
        };
        assert_ne!(copy.id, source.id);
        assert!(source_lanes.iter().all(|id| !copy_lanes.contains(id)));
        assert_eq!(lowered.id_claims, claims_for_commands(&lowered.commands));
        assert_eq!(copy_target.pattern, copy.id);
        assert_ne!(copy_target.pattern, source.id);
        assert_eq!(copy_target.mode, PatternEditorMode::Steps);
    }

    #[test]
    fn delete_of_referenced_pattern_is_refused() {
        use crate::pattern_actions::editor_target_after_delete;

        let mut project = project();
        envelope(
            lower_pattern_action(
                PatternActionSnapshot::from_project(&project),
                &create_action(&project, "Placed"),
            )
            .unwrap(),
        )
        .apply(&mut project)
        .unwrap();
        let pattern = only_pattern(&project);
        let commands = vec![DomainCommand::Bindings(
            BindingCommand::PutPatternDefinitionAlias {
                alias: arrangement::PatternId::from_raw(1),
                before: None,
                after: Some(pattern.id),
            },
        )];
        CommandEnvelope {
            label: "Alias pattern".into(),
            base_revision: project.revisions().aggregate,
            coalesce: None,
            id_claims: claims_for_commands(&commands),
            commands,
        }
        .apply(&mut project)
        .unwrap();

        let current = PatternEditorTarget::from_raw(99, PatternEditorMode::PianoRoll);
        let delete = action(
            &project,
            PatternAction::Delete {
                pattern: pattern.id,
                expected_pattern_revision: pattern.revision,
            },
        );
        assert!(matches!(
            lower_pattern_action(PatternActionSnapshot::from_project(&project), &delete),
            Err(PatternLoweringError::PatternInUse {
                pattern: refused,
                ..
            }) if refused == pattern.id
        ));
        assert_eq!(
            editor_target_after_delete(Some(current), pattern.id, true),
            Some(current)
        );
    }

    #[test]
    fn expression_policy_and_manual_divergence_are_authoritative_command_data() {
        let mut project = project();
        envelope(
            lower_pattern_action(
                PatternActionSnapshot::from_project(&project),
                &create_action(&project, "Alternating"),
            )
            .unwrap(),
        )
        .apply(&mut project)
        .unwrap();
        let pattern = only_pattern(&project);
        let bindings = BTreeMap::from([
            ("a".into(), TriggerTarget::AnalysisTemplate(7)),
            ("b".into(), TriggerTarget::AnalysisTemplate(9)),
        ]);
        let notation = action(
            &project,
            PatternAction::Edit(PatternEditIntent {
                pattern: pattern.id,
                expected_pattern_revision: pattern.revision,
                edit: PatternEdit::ApplyExpression {
                    source: "<a b>".into(),
                    bindings: bindings.clone(),
                    overwrite: DivergedOverwrite::Refuse,
                    realization: ExpressionRealizationContext {
                        cycle_index: 1,
                        performance_seed: 41,
                    },
                },
            }),
        );
        envelope(
            lower_pattern_action(PatternActionSnapshot::from_project(&project), &notation).unwrap(),
        )
        .apply(&mut project)
        .unwrap();

        let generated = only_pattern(&project);
        let sounding_targets = match &generated.content {
            PatternContent::Steps(steps) => steps
                .lanes
                .values()
                .filter(|lane| !lane.steps.is_empty())
                .map(|lane| lane.target.clone())
                .collect::<Vec<_>>(),
            PatternContent::Notes(_) => unreachable!(),
        };
        assert_eq!(sounding_targets, vec![TriggerTarget::AnalysisTemplate(9)]);

        let regenerate = action(
            &project,
            PatternAction::Edit(PatternEditIntent {
                pattern: generated.id,
                expected_pattern_revision: generated.revision,
                edit: PatternEdit::ApplyExpression {
                    source: "<a b>".into(),
                    bindings,
                    overwrite: DivergedOverwrite::Refuse,
                    realization: ExpressionRealizationContext {
                        cycle_index: 0,
                        performance_seed: 41,
                    },
                },
            }),
        );
        let regenerate = envelope(
            lower_pattern_action(PatternActionSnapshot::from_project(&project), &regenerate)
                .unwrap(),
        );
        assert_eq!(regenerate.commands.len(), 2);
        let applied = regenerate.clone().apply(&mut project).unwrap();
        let generated = only_pattern(&project);
        assert!(!generated.origin.diverged());
        let sounding_targets = match &generated.content {
            PatternContent::Steps(steps) => steps
                .lanes
                .values()
                .filter(|lane| !lane.steps.is_empty())
                .map(|lane| lane.target.clone())
                .collect::<Vec<_>>(),
            PatternContent::Notes(_) => unreachable!(),
        };
        assert_eq!(sounding_targets, vec![TriggerTarget::AnalysisTemplate(7)]);

        applied.inverse.apply(&mut project).unwrap();
        let restored = only_pattern(&project);
        assert!(!restored.origin.diverged());
        let restored_targets = match &restored.content {
            PatternContent::Steps(steps) => steps
                .lanes
                .values()
                .filter(|lane| !lane.steps.is_empty())
                .map(|lane| lane.target.clone())
                .collect::<Vec<_>>(),
            PatternContent::Notes(_) => unreachable!(),
        };
        assert_eq!(restored_targets, vec![TriggerTarget::AnalysisTemplate(9)]);
        CommandEnvelope::from_batch(project.revisions().aggregate, regenerate.as_batch())
            .apply(&mut project)
            .unwrap();
        let generated = only_pattern(&project);
        assert!(!generated.origin.diverged());

        let mut content = generated.content.clone();
        let PatternContent::Steps(steps) = &mut content else {
            unreachable!()
        };
        let active = steps
            .lanes
            .values_mut()
            .find(|lane| !lane.steps.is_empty())
            .unwrap();
        active.steps.clear();
        let hand_edit = action(
            &project,
            PatternAction::Edit(PatternEditIntent::replace_content(&generated, content)),
        );
        let lowered = envelope(
            lower_pattern_action(PatternActionSnapshot::from_project(&project), &hand_edit)
                .unwrap(),
        );
        let DomainCommand::Sequencer(SequencerCommand::PutPattern {
            after: Some(after), ..
        }) = &lowered.commands[0]
        else {
            panic!("expected pattern put")
        };
        assert!(after.origin.diverged());
        lowered.apply(&mut project).unwrap();
        assert!(only_pattern(&project).origin.diverged());
    }

    #[test]
    fn save_round_trip_preserves_origin_and_deleted_id_high_water_mark() {
        let mut project = project();
        envelope(
            lower_pattern_action(
                PatternActionSnapshot::from_project(&project),
                &create_action(&project, "Saved"),
            )
            .unwrap(),
        )
        .apply(&mut project)
        .unwrap();
        let pattern = only_pattern(&project);
        let expression = action(
            &project,
            PatternAction::Edit(PatternEditIntent {
                pattern: pattern.id,
                expected_pattern_revision: pattern.revision,
                edit: PatternEdit::ApplyExpression {
                    source: "a".into(),
                    bindings: BTreeMap::from([("a".into(), TriggerTarget::AnalysisTemplate(7))]),
                    overwrite: DivergedOverwrite::Refuse,
                    realization: ExpressionRealizationContext::default(),
                },
            }),
        );
        envelope(
            lower_pattern_action(PatternActionSnapshot::from_project(&project), &expression)
                .unwrap(),
        )
        .apply(&mut project)
        .unwrap();
        let expected_origin = only_pattern(&project).origin;
        assert!(matches!(&expected_origin, PatternOrigin::Expression { .. }));

        let file = ProjectFile::from_project(&project, None);
        let payloads = encode_constructive(&project).unwrap();
        let decoded = decode_constructive(&file, &payloads, AuditoryIr::new(48_000)).unwrap();
        let decoded_pattern = decoded
            .state
            .domains
            .sequencer
            .patterns()
            .patterns()
            .next()
            .unwrap();
        assert_eq!(decoded_pattern.origin, expected_origin);

        let delete = action(
            &project,
            PatternAction::Delete {
                pattern: decoded_pattern.id,
                expected_pattern_revision: decoded_pattern.revision,
            },
        );
        envelope(
            lower_pattern_action(PatternActionSnapshot::from_project(&project), &delete).unwrap(),
        )
        .apply(&mut project)
        .unwrap();
        let file = ProjectFile::from_project(&project, None);
        let payloads = encode_constructive(&project).unwrap();
        let decoded = decode_constructive(&file, &payloads, AuditoryIr::new(48_000)).unwrap();
        assert_eq!(
            decoded
                .state
                .domains
                .sequencer
                .allocator_state()
                .next_pattern_id,
            2
        );
        let next = create_action_for_snapshot(decoded.aggregate_revision, "After reopen");
        let lowered = envelope(
            lower_pattern_action(
                PatternActionSnapshot {
                    aggregate_revision: decoded.aggregate_revision,
                    state: &decoded.state,
                },
                &next,
            )
            .unwrap(),
        );
        let DomainCommand::Sequencer(SequencerCommand::PutPattern {
            after: Some(after), ..
        }) = &lowered.commands[0]
        else {
            panic!("expected pattern create")
        };
        assert_eq!(after.id, PatternId::from_raw(2));
    }

    fn create_action_for_snapshot(revision: u64, name: &str) -> PatternActionIntent {
        PatternActionIntent {
            expected_project_revision: revision,
            action: PatternAction::Create(CreatePatternIntent {
                mode: PatternEditorMode::Steps,
                name: name.into(),
                length: BeatDuration((PPQ * 4) as u64),
                step_resolution: BeatDuration((PPQ / 4) as u64),
                initial_target: Some(TriggerTarget::AnalysisTemplate(7)),
            }),
        }
    }
}
