//! Immutable pattern-audition recipes for the shared DAW renderer.
//!
//! A recipe is derived from one authoritative aggregate snapshot. It retains
//! the real occurrence position and cycle, concretizes generated notation for
//! that cycle, and freezes the exact decoder/instrument inputs supplied by the
//! project audio boundary. It does not own a device, transport, or DSP path.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::arrangement::ClipContent;
use crate::assets::AssetId;
use crate::daw_engine::{AssetPcmMap, BuiltInInstrumentDefinition, DawEngineConfig};
use crate::daw_project::{BridgeError, DawProject, ProjectRevisions};
use crate::mixer::{BusId, PluginDescriptor, ProcessorId};
use crate::pattern_authoring;
use crate::pattern_use_graph::{
    validate_occurrence_target, PatternOccurrenceTarget, PatternUseError, PatternUseSnapshot,
};
use crate::sample_kit::SampleTargetRef;
use crate::sampler_runtime::build_authoritative_sampler_routes;
use crate::sequencer::{
    NoteId, PatternContent, PatternDefinition, PatternOrigin, SampleAssetId, ScheduledEvent,
    ScheduledKind, SequencerCommand, StepLaneId, TriggerTarget,
};

use super::{plan_loop_audition, PatternLoopAuditionIntent, PatternWorkflowError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternAuditionPad {
    pub lane: StepLaneId,
    /// Guards against a stale lane picker which now names different material.
    pub target: TriggerTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternAuditionSelection {
    Notes(BTreeSet<NoteId>),
    Steps(BTreeSet<(StepLaneId, u32)>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternAuditionScope {
    Pattern,
    Pad(PatternAuditionPad),
    Selection(PatternAuditionSelection),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternAuditionRequest {
    pub expected_project_revision: u64,
    pub occurrence: PatternOccurrenceTarget,
    pub cycle_index: u64,
    pub performance_seed: u64,
    pub scope: PatternAuditionScope,
}

/// Exact runtime facts supplied by the same boundary that renders the project.
/// `plugin_instruments` is an explicit identity bridge; raw sequencer and
/// mixer IDs are never assumed to share a namespace.
#[derive(Clone, Debug)]
pub struct PatternAuditionRenderInputs {
    pub pcm: Arc<AssetPcmMap>,
    pub engine: Arc<DawEngineConfig>,
    pub plugin_instruments: BTreeMap<u64, ProcessorId>,
}

impl PatternAuditionRenderInputs {
    pub fn new(pcm: Arc<AssetPcmMap>, engine: Arc<DawEngineConfig>) -> Self {
        Self {
            pcm,
            engine,
            plugin_instruments: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternAuditionMaterial {
    Sampler {
        alias: SampleAssetId,
        target: SampleTargetRef,
        asset: AssetId,
        bus: BusId,
    },
    BuiltInInstrument {
        identity: u64,
        bus: BusId,
        sampler: bool,
    },
    PluginInstrument {
        identity: u64,
        processor: ProcessorId,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct PatternAuditionPluginMaterial {
    pub bus: BusId,
    pub processor: ProcessorId,
    pub bypassed: bool,
    pub wet: f32,
    pub descriptor: PluginDescriptor,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PatternAuditionPin {
    pub revisions: ProjectRevisions,
    pub occurrence: PatternOccurrenceTarget,
    pub pattern_revision: u64,
    pub cycle_index: u64,
    pub performance_seed: u64,
    pub scope: PatternAuditionScope,
    pub loop_range: crate::sequencer::FrameRange,
    pub events: Arc<[ScheduledEvent]>,
    pub materials: Arc<[PatternAuditionMaterial]>,
    pub plugins: Arc<[PatternAuditionPluginMaterial]>,
}

/// Frozen inputs accepted by the normal `compile_daw_engine` path.
#[derive(Clone, Debug)]
pub struct PatternAuditionRecipe {
    pub pin: PatternAuditionPin,
    pub project: Arc<DawProject>,
    pub pcm: Arc<AssetPcmMap>,
    pub engine: Arc<DawEngineConfig>,
}

impl PatternAuditionRecipe {
    pub fn is_current(&self, aggregate_revision: u64) -> bool {
        self.pin.revisions.aggregate == aggregate_revision
    }
}

pub fn prepare_pattern_audition(
    project: &DawProject,
    request: &PatternAuditionRequest,
    inputs: PatternAuditionRenderInputs,
) -> Result<PatternAuditionRecipe, PatternAuditionError> {
    if request.expected_project_revision != project.revisions().aggregate {
        return Err(PatternAuditionError::StaleRevision {
            expected: request.expected_project_revision,
            actual: project.revisions().aggregate,
        });
    }
    let snapshot = PatternUseSnapshot::from_project(project);
    let occurrence = validate_occurrence_target(snapshot, request.occurrence)?;
    let plan = plan_loop_audition(
        snapshot,
        PatternLoopAuditionIntent {
            expected_project_revision: request.expected_project_revision,
            occurrence: request.occurrence,
            cycle_index: request.cycle_index,
            performance_seed: request.performance_seed,
        },
    )?;
    let source = project
        .state()
        .domains
        .sequencer
        .patterns()
        .get(request.occurrence.pattern)
        .cloned()
        .ok_or(PatternAuditionError::MissingPattern)?;
    let mut concrete = pattern_authoring::preview_expression_placement(
        &source,
        request.cycle_index,
        request.performance_seed,
    )?
    .definition;
    filter_definition(&mut concrete, &request.scope)?;
    // This clone is a render artifact, not an authored edit. Marking it
    // concrete prevents the sequencer from regenerating a different cycle.
    concrete.origin = PatternOrigin::Authored;

    let isolated = isolate_project(project, request.occurrence, concrete)?;
    let events = isolated
        .state()
        .domains
        .sequencer
        .schedule_project_window(plan.loop_range, request.performance_seed)
        .into_iter()
        .filter(|event| {
            super::scheduled_clip(&event.kind) == Some(request.occurrence.sequencer_clip)
        })
        .collect::<Vec<_>>();
    let materials = resolve_materials(project, &events, &inputs)?;
    let plugins = plugin_path_materials(project, occurrence.track)?;
    let mut engine = (*inputs.engine).clone();
    engine.performance_seed = request.performance_seed;

    Ok(PatternAuditionRecipe {
        pin: PatternAuditionPin {
            revisions: project.revisions(),
            occurrence: request.occurrence,
            pattern_revision: source.revision,
            cycle_index: request.cycle_index,
            performance_seed: request.performance_seed,
            scope: request.scope.clone(),
            loop_range: plan.loop_range,
            events: events.into(),
            materials: materials.into(),
            plugins: plugins.into(),
        },
        project: Arc::new(isolated),
        pcm: inputs.pcm,
        engine: Arc::new(engine),
    })
}

fn filter_definition(
    definition: &mut PatternDefinition,
    scope: &PatternAuditionScope,
) -> Result<(), PatternAuditionError> {
    match scope {
        PatternAuditionScope::Pattern => Ok(()),
        PatternAuditionScope::Pad(pad) => {
            let PatternContent::Steps(steps) = &mut definition.content else {
                return Err(PatternAuditionError::ScopeKindMismatch);
            };
            let lane = steps
                .lanes
                .get(&pad.lane)
                .ok_or(PatternAuditionError::MissingLane(pad.lane))?;
            if lane.target != pad.target {
                return Err(PatternAuditionError::StalePadTarget {
                    lane: pad.lane,
                    expected: pad.target.clone(),
                    actual: lane.target.clone(),
                });
            }
            steps.lanes.retain(|lane, _| *lane == pad.lane);
            Ok(())
        }
        PatternAuditionScope::Selection(PatternAuditionSelection::Notes(selection)) => {
            if selection.is_empty() {
                return Err(PatternAuditionError::EmptySelection);
            }
            let PatternContent::Notes(notes) = &mut definition.content else {
                return Err(PatternAuditionError::ScopeKindMismatch);
            };
            if selection.iter().any(|note| !notes.notes.contains_key(note)) {
                return Err(PatternAuditionError::StaleSelection);
            }
            notes.notes.retain(|note, _| selection.contains(note));
            Ok(())
        }
        PatternAuditionScope::Selection(PatternAuditionSelection::Steps(selection)) => {
            if selection.is_empty() {
                return Err(PatternAuditionError::EmptySelection);
            }
            let PatternContent::Steps(steps) = &mut definition.content else {
                return Err(PatternAuditionError::ScopeKindMismatch);
            };
            if selection.iter().any(|(lane, step)| {
                !steps
                    .lanes
                    .get(lane)
                    .is_some_and(|lane| lane.steps.contains_key(step))
            }) {
                return Err(PatternAuditionError::StaleSelection);
            }
            steps.lanes.retain(|lane_id, lane| {
                lane.steps
                    .retain(|step, _| selection.contains(&(*lane_id, *step)));
                !lane.steps.is_empty()
            });
            Ok(())
        }
    }
}

fn isolate_project(
    project: &DawProject,
    occurrence: PatternOccurrenceTarget,
    concrete: PatternDefinition,
) -> Result<DawProject, PatternAuditionError> {
    let mut state = project.state().clone();
    let mut commands = Vec::new();
    for clip in state.domains.sequencer.clips().cloned().collect::<Vec<_>>() {
        let should_mute = clip.id != occurrence.sequencer_clip;
        if clip.muted != should_mute {
            let mut muted = clip.clone();
            muted.muted = should_mute;
            commands.push(SequencerCommand::PutClip {
                before: Some(clip),
                after: Some(muted),
            });
        }
    }
    let before = state
        .domains
        .sequencer
        .patterns()
        .get(occurrence.pattern)
        .cloned()
        .ok_or(PatternAuditionError::MissingPattern)?;
    commands.push(SequencerCommand::PutPattern {
        before: Some(before),
        after: Some(concrete),
    });
    state
        .domains
        .sequencer
        .apply_without_history(&commands)
        .map_err(|error| PatternAuditionError::Isolation(error.to_string()))?;
    for clip in state.domains.arrangement.clips.values_mut() {
        if matches!(clip.content, ClipContent::Audio(_)) {
            clip.muted = true;
        }
    }
    DawProject::from_restored(
        project.name.clone(),
        project.schema_version,
        state,
        project.revisions(),
        0,
    )
    .map_err(PatternAuditionError::Bridge)
}

fn resolve_materials(
    project: &DawProject,
    events: &[ScheduledEvent],
    inputs: &PatternAuditionRenderInputs,
) -> Result<Vec<PatternAuditionMaterial>, PatternAuditionError> {
    let mut sample_aliases = BTreeSet::new();
    let mut instruments = BTreeSet::new();
    for event in events {
        match &event.kind {
            ScheduledKind::NoteOn { instrument, .. }
            | ScheduledKind::NoteOff { instrument, .. }
            | ScheduledKind::NoteExpression { instrument, .. } => {
                instruments.insert(instrument.ok_or(PatternAuditionError::IdentityFreeNote)?);
            }
            ScheduledKind::Trigger { target, .. } => match target {
                TriggerTarget::Sample(alias) => {
                    sample_aliases.insert(*alias);
                }
                TriggerTarget::InstrumentNote { instrument, .. } => {
                    instruments.insert(*instrument);
                }
                TriggerTarget::DrumPad { .. } | TriggerTarget::AnalysisTemplate(_) => {
                    return Err(PatternAuditionError::UnresolvedTrigger(target.clone()));
                }
            },
            ScheduledKind::LoopBoundary => {}
        }
    }

    let sampler_routes = build_authoritative_sampler_routes(project, &inputs.pcm)?;
    let routes = sampler_routes
        .routes
        .into_iter()
        .map(|route| (route.sample_alias, route))
        .collect::<BTreeMap<_, _>>();
    let mut materials = Vec::new();
    for alias in sample_aliases {
        let route = routes
            .get(&alias)
            .ok_or(PatternAuditionError::MissingSamplerMaterial(alias))?;
        let zone = project.state().domains.sample_kits.kits[&route.target.kit]
            .zone_for_target(route.target)
            .expect("validated sampler target");
        materials.push(PatternAuditionMaterial::Sampler {
            alias,
            target: route.target,
            asset: zone.material.asset_id(),
            bus: route.bus,
        });
    }
    for identity in instruments {
        if let Some(processor) = inputs.plugin_instruments.get(&identity).copied() {
            if project.state().domains.mixer.processor(processor).is_none() {
                return Err(PatternAuditionError::MissingPluginProcessor(processor));
            }
            materials.push(PatternAuditionMaterial::PluginInstrument {
                identity,
                processor,
            });
            continue;
        }
        let route = inputs
            .engine
            .instruments
            .get(&identity)
            .ok_or(PatternAuditionError::MissingInstrument(identity))?;
        materials.push(PatternAuditionMaterial::BuiltInInstrument {
            identity,
            bus: route.bus,
            sampler: matches!(
                &route.definition,
                BuiltInInstrumentDefinition::Sampler { .. }
            ),
        });
    }
    materials.sort_by_key(|material| match material {
        PatternAuditionMaterial::Sampler { alias, .. } => (0, alias.get()),
        PatternAuditionMaterial::BuiltInInstrument { identity, .. } => (1, *identity),
        PatternAuditionMaterial::PluginInstrument { identity, .. } => (2, *identity),
    });
    Ok(materials)
}

fn plugin_path_materials(
    project: &DawProject,
    track: crate::arrangement::TrackId,
) -> Result<Vec<PatternAuditionPluginMaterial>, PatternAuditionError> {
    let mixer = &project.state().domains.mixer;
    let first = project
        .state()
        .bindings
        .mixer
        .tracks
        .get(&track)
        .copied()
        .unwrap_or_else(|| mixer.master());
    let mut pending = vec![first];
    let mut visited = BTreeSet::new();
    let mut result = Vec::new();
    while let Some(bus_id) = pending.pop() {
        if !visited.insert(bus_id) {
            continue;
        }
        let bus = mixer
            .bus(bus_id)
            .ok_or(PatternAuditionError::MissingMixerBus(bus_id))?;
        for slot in bus.inserts() {
            let processor = mixer.processor(slot.processor_id()).ok_or(
                PatternAuditionError::MissingPluginProcessor(slot.processor_id()),
            )?;
            result.push(PatternAuditionPluginMaterial {
                bus: bus_id,
                processor: processor.id(),
                bypassed: slot.bypassed(),
                wet: slot.wet(),
                descriptor: processor.descriptor().clone(),
            });
        }
        if let Some(output) = bus.output() {
            pending.push(output);
        }
        pending.extend(bus.sends().iter().map(|send| send.target()));
    }
    result.sort_by_key(|plugin| (plugin.bus, plugin.processor));
    Ok(result)
}

#[derive(Debug)]
pub enum PatternAuditionError {
    Workflow(PatternWorkflowError),
    Use(PatternUseError),
    Authoring(pattern_authoring::PatternAuthoringError),
    Bridge(BridgeError),
    StaleRevision {
        expected: u64,
        actual: u64,
    },
    MissingPattern,
    ScopeKindMismatch,
    EmptySelection,
    StaleSelection,
    MissingLane(StepLaneId),
    StalePadTarget {
        lane: StepLaneId,
        expected: TriggerTarget,
        actual: TriggerTarget,
    },
    IdentityFreeNote,
    UnresolvedTrigger(TriggerTarget),
    MissingSamplerMaterial(SampleAssetId),
    MissingInstrument(u64),
    MissingPluginProcessor(ProcessorId),
    MissingMixerBus(BusId),
    Isolation(String),
    Render(String),
    Cancelled,
    Superseded,
}

impl fmt::Display for PatternAuditionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workflow(error) => write!(formatter, "pattern audition planning failed: {error}"),
            Self::Use(error) => write!(formatter, "pattern occurrence is stale: {error}"),
            Self::Authoring(error) => write!(formatter, "pattern cycle cannot be realized: {error}"),
            Self::Bridge(error) => write!(formatter, "pattern render snapshot is invalid: {error}"),
            Self::StaleRevision { expected, actual } => write!(
                formatter,
                "pattern audition expected project revision {expected}, current revision is {actual}"
            ),
            Self::MissingPattern => formatter.write_str("pattern definition is missing"),
            Self::ScopeKindMismatch => formatter.write_str("audition scope does not match the pattern kind"),
            Self::EmptySelection => formatter.write_str("pattern audition selection is empty"),
            Self::StaleSelection => formatter.write_str("pattern audition selection is stale"),
            Self::MissingLane(lane) => write!(formatter, "pattern lane {} is missing", lane.get()),
            Self::StalePadTarget { lane, .. } => write!(formatter, "pattern lane {} was retargeted", lane.get()),
            Self::IdentityFreeNote => formatter.write_str("note audition has no instrument identity"),
            Self::UnresolvedTrigger(target) => write!(formatter, "trigger {target:?} has no shared-render identity"),
            Self::MissingSamplerMaterial(alias) => write!(formatter, "sampler alias {} has no exact decoded material", alias.get()),
            Self::MissingInstrument(identity) => write!(formatter, "instrument {identity} is absent from the shared engine recipe"),
            Self::MissingPluginProcessor(processor) => write!(formatter, "plugin processor {} is missing", processor.get()),
            Self::MissingMixerBus(bus) => write!(formatter, "mixer bus {} is missing", bus.get()),
            Self::Isolation(message) => write!(formatter, "pattern render isolation failed: {message}"),
            Self::Render(message) => write!(formatter, "shared pattern render failed: {message}"),
            Self::Cancelled => formatter.write_str("pattern audition was cancelled"),
            Self::Superseded => formatter.write_str("pattern audition was superseded"),
        }
    }
}

impl Error for PatternAuditionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Workflow(error) => Some(error),
            Self::Use(error) => Some(error),
            Self::Authoring(error) => Some(error),
            Self::Bridge(error) => Some(error),
            _ => None,
        }
    }
}

impl From<PatternWorkflowError> for PatternAuditionError {
    fn from(error: PatternWorkflowError) -> Self {
        Self::Workflow(error)
    }
}

impl From<PatternUseError> for PatternAuditionError {
    fn from(error: PatternUseError) -> Self {
        Self::Use(error)
    }
}

impl From<pattern_authoring::PatternAuthoringError> for PatternAuditionError {
    fn from(error: pattern_authoring::PatternAuthoringError) -> Self {
        Self::Authoring(error)
    }
}

impl From<BridgeError> for PatternAuditionError {
    fn from(error: BridgeError) -> Self {
        Self::Bridge(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::arrangement::Frame;
    use crate::arrangement_interaction::{ArrangementEdit, ArrangementEditIntent, GestureCommit};
    use crate::arrangement_view::{
        ArrangementAction, ArrangementActionIntent, ArrangementViewEvent,
    };
    use crate::instruments::{SynthParams, Waveform};
    use crate::live_project::LiveProject;
    use crate::pattern_actions::{
        CreatePatternIntent, PatternAction, PatternActionIntent, PatternEdit, PatternEditIntent,
        PatternEditorMode,
    };
    use crate::pattern_authoring::{DivergedOverwrite, ExpressionRealizationContext};
    use crate::pattern_use_graph::PatternUseGraph;
    use crate::project_controller::{
        lower_arrangement_event, lower_gesture, ArrangementDispatch, PatternWorkflowIntent,
        PatternWorkflowOutcome, ProjectController,
    };
    use crate::sequencer::{BeatDuration, PPQ};
    use crate::ui_drag::DropIntent;

    fn expression_project(
        source: &str,
        bindings: BTreeMap<String, TriggerTarget>,
        cycles: u64,
    ) -> (DawProject, PatternOccurrenceTarget) {
        let project = DawProject::new("Pattern audition", 48_000, 120.0).unwrap();
        let live = LiveProject::from_project(project, BTreeMap::new()).unwrap();
        let mut controller = ProjectController::new(live).unwrap();
        let create = PatternWorkflowIntent::Action(PatternActionIntent {
            expected_project_revision: controller.revisions().aggregate,
            action: PatternAction::Create(CreatePatternIntent {
                mode: PatternEditorMode::Steps,
                name: "Audition expression".into(),
                length: BeatDuration((PPQ * 4) as u64),
                step_resolution: BeatDuration((PPQ / 4) as u64),
                initial_target: None,
            }),
        });
        let PatternWorkflowOutcome::Published { publication, .. } =
            controller.execute_pattern_workflow(create).unwrap()
        else {
            panic!("create must publish")
        };
        let pattern = publication.pattern;
        let revision = publication.definition.unwrap().revision;
        let apply = PatternWorkflowIntent::Action(PatternActionIntent {
            expected_project_revision: controller.revisions().aggregate,
            action: PatternAction::Edit(PatternEditIntent {
                pattern,
                expected_pattern_revision: revision,
                edit: PatternEdit::ApplyExpression {
                    source: source.into(),
                    bindings,
                    overwrite: DivergedOverwrite::Refuse,
                    realization: ExpressionRealizationContext::default(),
                },
            }),
        });
        controller.execute_pattern_workflow(apply).unwrap();

        let drop = ArrangementViewEvent::Action(ArrangementActionIntent {
            expected_revision: controller.revisions().aggregate,
            action: ArrangementAction::Drop(DropIntent::InsertPattern {
                pattern,
                track: None,
                at: Frame::ZERO,
                make_unique: false,
            }),
        });
        let ArrangementDispatch::Apply(drop) =
            lower_arrangement_event(controller.snapshot(), drop).unwrap()
        else {
            panic!("drop must apply")
        };
        controller.execute(drop.envelope).unwrap();
        let occurrence = PatternUseGraph::build(PatternUseSnapshot::from_project(
            &controller.snapshot().project,
        ))
        .unwrap()
        .pattern(pattern)
        .unwrap()
        .occurrences[0]
            .clone();
        let boundary = Frame(
            occurrence
                .placement
                .start
                .0
                .checked_add((occurrence.placement.len() as i64) * cycles as i64)
                .unwrap(),
        );
        let repeat = GestureCommit {
            selection: None,
            edit: Some(ArrangementEditIntent {
                expected_revision: controller.revisions().aggregate,
                edit: ArrangementEdit::SetRepeatBoundary {
                    clip_id: occurrence.target.arrangement_clip,
                    boundary,
                },
            }),
        };
        let ArrangementDispatch::Apply(repeat) =
            lower_gesture(controller.snapshot(), repeat).unwrap()
        else {
            panic!("repeat must apply")
        };
        controller.execute(repeat.envelope).unwrap();
        let target = PatternUseGraph::build(PatternUseSnapshot::from_project(
            &controller.snapshot().project,
        ))
        .unwrap()
        .occurrence_for_clip(occurrence.target.arrangement_clip)
        .unwrap()
        .target;
        (controller.snapshot().project.as_ref().clone(), target)
    }

    fn synth_inputs(project: &DawProject) -> PatternAuditionRenderInputs {
        let master = project.state().domains.mixer.master();
        let mut saw = SynthParams::default();
        saw.waveform = Waveform::Saw;
        let mut sine = SynthParams::default();
        sine.waveform = Waveform::Sine;
        let mut engine = DawEngineConfig::default();
        engine.instruments.insert(
            11,
            crate::daw_engine::BuiltInInstrumentRoute {
                definition: BuiltInInstrumentDefinition::Subtractive(saw),
                bus: master,
            },
        );
        engine.instruments.insert(
            22,
            crate::daw_engine::BuiltInInstrumentRoute {
                definition: BuiltInInstrumentDefinition::Subtractive(sine),
                bus: master,
            },
        );
        PatternAuditionRenderInputs::new(Arc::new(BTreeMap::new()), Arc::new(engine))
    }

    fn request(
        project: &DawProject,
        occurrence: PatternOccurrenceTarget,
        cycle_index: u64,
    ) -> PatternAuditionRequest {
        PatternAuditionRequest {
            expected_project_revision: project.revisions().aggregate,
            occurrence,
            cycle_index,
            performance_seed: 71,
            scope: PatternAuditionScope::Pattern,
        }
    }

    fn trigger_facts(recipe: &PatternAuditionRecipe) -> Vec<(u64, f32, i64)> {
        recipe
            .pin
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                ScheduledKind::Trigger {
                    target: TriggerTarget::InstrumentNote { instrument, .. },
                    velocity,
                    ..
                } => Some((*instrument, *velocity, event.project_frame.0)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn audition_recipes_follow_real_alternation_every_fast_and_slow_cycles() {
        let bindings = BTreeMap::from([
            (
                "a".into(),
                TriggerTarget::InstrumentNote {
                    instrument: 11,
                    key: 48,
                },
            ),
            (
                "b".into(),
                TriggerTarget::InstrumentNote {
                    instrument: 22,
                    key: 72,
                },
            ),
        ]);

        let (project, occurrence) = expression_project("<a b>", bindings.clone(), 2);
        let first = prepare_pattern_audition(
            &project,
            &request(&project, occurrence, 0),
            synth_inputs(&project),
        )
        .unwrap();
        let second = prepare_pattern_audition(
            &project,
            &request(&project, occurrence, 1),
            synth_inputs(&project),
        )
        .unwrap();
        assert_eq!(trigger_facts(&first)[0].0, 11);
        assert_eq!(trigger_facts(&second)[0].0, 22);
        assert_eq!(first.pin.loop_range.end, second.pin.loop_range.start);

        let (project, occurrence) =
            expression_project("every(2, gain(0.25), a)", bindings.clone(), 2);
        let on = prepare_pattern_audition(
            &project,
            &request(&project, occurrence, 0),
            synth_inputs(&project),
        )
        .unwrap();
        let off = prepare_pattern_audition(
            &project,
            &request(&project, occurrence, 1),
            synth_inputs(&project),
        )
        .unwrap();
        assert!((trigger_facts(&on)[0].1 - 0.25).abs() < 1.0e-6);
        assert!((trigger_facts(&off)[0].1 - 1.0).abs() < 1.0e-6);

        let (project, occurrence) = expression_project("fast(2, <a b>)", bindings.clone(), 1);
        let fast = prepare_pattern_audition(
            &project,
            &request(&project, occurrence, 0),
            synth_inputs(&project),
        )
        .unwrap();
        let fast = trigger_facts(&fast);
        assert_eq!(fast.iter().map(|fact| fact.0).collect::<Vec<_>>(), [11, 22]);
        assert!(fast[0].2 < fast[1].2);

        let (project, occurrence) = expression_project("slow(2, a b)", bindings, 2);
        let slow_a = prepare_pattern_audition(
            &project,
            &request(&project, occurrence, 0),
            synth_inputs(&project),
        )
        .unwrap();
        let slow_b = prepare_pattern_audition(
            &project,
            &request(&project, occurrence, 1),
            synth_inputs(&project),
        )
        .unwrap();
        assert_eq!(trigger_facts(&slow_a)[0].0, 11);
        assert_eq!(trigger_facts(&slow_b)[0].0, 22);
    }

    #[test]
    fn pad_and_step_selection_scopes_retain_only_exact_requested_material() {
        let bindings = BTreeMap::from([
            (
                "a".into(),
                TriggerTarget::InstrumentNote {
                    instrument: 11,
                    key: 48,
                },
            ),
            (
                "b".into(),
                TriggerTarget::InstrumentNote {
                    instrument: 22,
                    key: 72,
                },
            ),
        ]);
        let (project, occurrence) = expression_project("stack(a, b)", bindings, 1);
        let full = prepare_pattern_audition(
            &project,
            &request(&project, occurrence, 0),
            synth_inputs(&project),
        )
        .unwrap();
        let (lane, target) = full
            .pin
            .events
            .iter()
            .find_map(|event| match &event.kind {
                ScheduledKind::Trigger {
                    lane,
                    target: target @ TriggerTarget::InstrumentNote { instrument: 11, .. },
                    ..
                } => Some((*lane, target.clone())),
                _ => None,
            })
            .unwrap();

        let mut pad_request = request(&project, occurrence, 0);
        pad_request.scope = PatternAuditionScope::Pad(PatternAuditionPad {
            lane,
            target: target.clone(),
        });
        let pad = prepare_pattern_audition(&project, &pad_request, synth_inputs(&project)).unwrap();
        assert_eq!(
            trigger_facts(&pad)
                .iter()
                .map(|fact| fact.0)
                .collect::<Vec<_>>(),
            [11]
        );

        let mut selection_request = request(&project, occurrence, 0);
        selection_request.scope = PatternAuditionScope::Selection(PatternAuditionSelection::Steps(
            BTreeSet::from([(lane, 0)]),
        ));
        let selection =
            prepare_pattern_audition(&project, &selection_request, synth_inputs(&project)).unwrap();
        assert_eq!(trigger_facts(&selection).len(), 1);

        pad_request.scope = PatternAuditionScope::Pad(PatternAuditionPad {
            lane,
            target: TriggerTarget::AnalysisTemplate(999),
        });
        assert!(matches!(
            prepare_pattern_audition(&project, &pad_request, synth_inputs(&project)),
            Err(PatternAuditionError::StalePadTarget { .. })
        ));
    }

    #[test]
    fn shared_renderer_produces_non_silent_distinct_cycle_pcm() {
        let bindings = BTreeMap::from([
            (
                "a".into(),
                TriggerTarget::InstrumentNote {
                    instrument: 11,
                    key: 48,
                },
            ),
            (
                "b".into(),
                TriggerTarget::InstrumentNote {
                    instrument: 22,
                    key: 72,
                },
            ),
        ]);
        let (project, occurrence) = expression_project("<a b>", bindings, 2);
        let mut adapter = super::super::PatternAuditionAdapter::default();
        let first = adapter
            .prepare(
                &project,
                &request(&project, occurrence, 0),
                synth_inputs(&project),
            )
            .unwrap()
            .execute()
            .unwrap();
        let first = adapter
            .finish(first, project.revisions().aggregate)
            .unwrap();
        let second = adapter
            .prepare(
                &project,
                &request(&project, occurrence, 1),
                synth_inputs(&project),
            )
            .unwrap()
            .execute()
            .unwrap();
        let second = adapter
            .finish(second, project.revisions().aggregate)
            .unwrap();
        let first_pcm = first.render.audio.interleaved();
        let second_pcm = second.render.audio.interleaved();
        assert!(first_pcm.iter().any(|sample| sample.abs() > 1.0e-4));
        assert!(second_pcm.iter().any(|sample| sample.abs() > 1.0e-4));
        assert_ne!(first_pcm, second_pcm);
    }

    #[test]
    fn newer_request_cancels_old_job_and_revision_gate_refuses_stale_completion() {
        let bindings = BTreeMap::from([(
            "a".into(),
            TriggerTarget::InstrumentNote {
                instrument: 11,
                key: 48,
            },
        )]);
        let (project, occurrence) = expression_project("a", bindings, 2);
        let mut adapter = super::super::PatternAuditionAdapter::default();
        let old = adapter
            .prepare(
                &project,
                &request(&project, occurrence, 0),
                synth_inputs(&project),
            )
            .unwrap();
        let current = adapter
            .prepare(
                &project,
                &request(&project, occurrence, 1),
                synth_inputs(&project),
            )
            .unwrap();
        assert!(matches!(
            old.execute(),
            Err(PatternAuditionError::Cancelled)
        ));
        let completion = current.execute().unwrap();
        assert!(matches!(
            adapter.finish(completion, project.revisions().aggregate + 1),
            Err(PatternAuditionError::StaleRevision { .. })
        ));
    }
}
