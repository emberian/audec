//! Typed boundary between mixer/automation views and the project controller.
//!
//! These actions describe user intent, not GPUI events and not mutable view
//! state. A controller validates the observed domain revision, translates the
//! action into its aggregate command envelope, and publishes a fresh snapshot.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::automation::{
    AutomationCommand, AutomationError, AutomationGraph, AutomationIntent, AutomationLaneId,
    AutomationPoint, AutomationPointId, BindingMode, SegmentShape, TimePosition,
};
use crate::mixer::{BusId, MixerCommand, MixerError, MixerGraph, ProcessorId, SendId, SendTap};
use crate::render_runtime::CohortRendererStatus;
use crate::render_service::{RenderAvailability, RenderServiceStatus};
use crate::workspace_document::EditorTarget;

pub type ControlActionCallback = Arc<dyn Fn(ControlAction) + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlIntegrationMode {
    /// Read controller snapshots and emit actions; never mutate project truth.
    Controller,
    /// Execute through the local/shared Cycle 2 backend and its private history.
    Compatibility,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlSurface {
    Mixer,
    Automation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryDirection {
    Undo,
    Redo,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ControlAction {
    Mixer(MixerActionIntent),
    Automation(AutomationActionIntent),
    History(ControlHistoryIntent),
}

impl ControlAction {
    pub const fn expected_revision(&self) -> u64 {
        match self {
            Self::Mixer(intent) => intent.expected_revision,
            Self::Automation(intent) => intent.expected_revision,
            Self::History(intent) => intent.expected_revision,
        }
    }

    pub const fn surface(&self) -> ControlSurface {
        match self {
            Self::Mixer(_) => ControlSurface::Mixer,
            Self::Automation(_) => ControlSurface::Automation,
            Self::History(intent) => intent.surface,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlHistoryIntent {
    pub surface: ControlSurface,
    pub expected_revision: u64,
    pub direction: HistoryDirection,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MixerActionIntent {
    pub expected_revision: u64,
    pub action: MixerAction,
}

impl MixerActionIntent {
    pub const fn new(expected_revision: u64, action: MixerAction) -> Self {
        Self {
            expected_revision,
            action,
        }
    }

    /// Compatibility/controller adapter into the existing reversible command.
    pub fn command(&self, graph: &MixerGraph) -> Result<MixerCommand, MixerError> {
        let action = self.action.clone();
        MixerCommand::build_at_revision(
            action.label(),
            self.expected_revision,
            graph,
            move |graph| action.apply(graph),
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum MixerAction {
    SetGainDb {
        bus: BusId,
        gain_db: f32,
    },
    SetPan {
        bus: BusId,
        pan: f32,
    },
    SetMuted {
        bus: BusId,
        muted: bool,
    },
    SetSoloed {
        bus: BusId,
        soloed: bool,
    },
    SetOutput {
        bus: BusId,
        target: BusId,
    },
    AddSend {
        bus: BusId,
        target: BusId,
        tap: SendTap,
        level_db: f32,
    },
    RemoveSend {
        send: SendId,
    },
    SetSendLevel {
        send: SendId,
        level_db: f32,
    },
    SetSendMuted {
        send: SendId,
        muted: bool,
    },
    SetSendTap {
        send: SendId,
        tap: SendTap,
    },
    SetInsertBypassed {
        processor: ProcessorId,
        bypassed: bool,
    },
    SetInsertWet {
        processor: ProcessorId,
        wet: f32,
    },
}

impl MixerAction {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::SetGainDb { .. } => "change channel gain",
            Self::SetPan { .. } => "change channel pan",
            Self::SetMuted { .. } => "toggle channel mute",
            Self::SetSoloed { .. } => "toggle channel solo",
            Self::SetOutput { .. } => "change channel output",
            Self::AddSend { .. } => "add send",
            Self::RemoveSend { .. } => "remove send",
            Self::SetSendLevel { .. } => "change send level",
            Self::SetSendMuted { .. } => "toggle send mute",
            Self::SetSendTap { .. } => "change send tap",
            Self::SetInsertBypassed { .. } => "toggle insert bypass",
            Self::SetInsertWet { .. } => "change insert mix",
        }
    }

    pub fn apply(&self, graph: &mut MixerGraph) -> Result<(), MixerError> {
        match *self {
            Self::SetGainDb { bus, gain_db } => graph.set_gain_db(bus, gain_db),
            Self::SetPan { bus, pan } => graph.set_pan(bus, pan),
            Self::SetMuted { bus, muted } => graph.set_muted(bus, muted),
            Self::SetSoloed { bus, soloed } => graph.set_soloed(bus, soloed),
            Self::SetOutput { bus, target } => graph.set_output(bus, target),
            Self::AddSend {
                bus,
                target,
                tap,
                level_db,
            } => graph.add_send(bus, target, tap, level_db).map(|_| ()),
            Self::RemoveSend { send } => graph.remove_send(send).map(|_| ()),
            Self::SetSendLevel { send, level_db } => graph.set_send_level(send, level_db),
            Self::SetSendMuted { send, muted } => graph.set_send_muted(send, muted),
            Self::SetSendTap { send, tap } => graph.set_send_tap(send, tap),
            Self::SetInsertBypassed {
                processor,
                bypassed,
            } => graph.set_insert_bypassed(processor, bypassed),
            Self::SetInsertWet { processor, wet } => graph.set_insert_wet(processor, wet),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AutomationActionIntent {
    pub expected_revision: u64,
    pub action: AutomationAction,
}

impl AutomationActionIntent {
    pub const fn new(expected_revision: u64, action: AutomationAction) -> Self {
        Self {
            expected_revision,
            action,
        }
    }

    /// Compatibility/controller adapter into the existing lane command seam.
    pub fn legacy_intent(
        &self,
        graph: &AutomationGraph,
    ) -> Result<AutomationIntent, AutomationError> {
        if graph.revision() != self.expected_revision {
            return Err(AutomationError::RevisionConflict {
                expected: self.expected_revision,
                actual: graph.revision(),
            });
        }
        let lane_id = self.action.lane();
        let before = graph
            .lane(lane_id)
            .cloned()
            .ok_or(AutomationError::MissingLane(lane_id))?;
        let mut after = before.clone();
        let allocated_point = matches!(&self.action, AutomationAction::InsertPoint { .. })
            .then(|| graph.next_point_id_candidate())
            .transpose()?;
        self.action.apply(&mut after, allocated_point)?;
        let command = AutomationCommand::replace(self.action.label(), before, after)?;
        Ok(AutomationIntent::new(self.expected_revision, command))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AutomationAction {
    SetLaneEnabled {
        lane: AutomationLaneId,
        enabled: bool,
    },
    SetLaneBinding {
        lane: AutomationLaneId,
        binding: BindingMode,
    },
    SetPointShape {
        lane: AutomationLaneId,
        point: AutomationPointId,
        shape: SegmentShape,
    },
    DeletePoint {
        lane: AutomationLaneId,
        point: AutomationPointId,
    },
    /// The controller allocates the durable identity at commit time.
    InsertPoint {
        lane: AutomationLaneId,
        position: TimePosition,
        value: f64,
        outgoing: SegmentShape,
    },
    MovePoint {
        lane: AutomationLaneId,
        point: AutomationPoint,
    },
}

impl AutomationAction {
    pub const fn lane(&self) -> AutomationLaneId {
        match *self {
            Self::SetLaneEnabled { lane, .. }
            | Self::SetLaneBinding { lane, .. }
            | Self::SetPointShape { lane, .. }
            | Self::DeletePoint { lane, .. }
            | Self::InsertPoint { lane, .. }
            | Self::MovePoint { lane, .. } => lane,
        }
    }

    pub const fn label(&self) -> &'static str {
        match self {
            Self::SetLaneEnabled { .. } => "toggle lane",
            Self::SetLaneBinding { .. } => "change binding mode",
            Self::SetPointShape { .. } => "change segment type",
            Self::DeletePoint { .. } => "delete point",
            Self::InsertPoint { .. } => "add point",
            Self::MovePoint { .. } => "move point",
        }
    }

    fn apply(
        &self,
        lane: &mut crate::automation::AutomationLane,
        allocated_point: Option<AutomationPointId>,
    ) -> Result<(), AutomationError> {
        match self {
            Self::SetLaneEnabled { enabled, .. } => lane.enabled = *enabled,
            Self::SetLaneBinding { binding, .. } => lane.binding = *binding,
            Self::SetPointShape { point, shape, .. } => {
                let mut existing = lane
                    .remove_point(*point)
                    .ok_or(AutomationError::MissingPoint(*point))?;
                existing.outgoing = *shape;
                lane.insert_point(existing)?;
            }
            Self::DeletePoint { point, .. } => {
                lane.remove_point(*point)
                    .ok_or(AutomationError::MissingPoint(*point))?;
            }
            Self::InsertPoint {
                position,
                value,
                outgoing,
                ..
            } => {
                lane.insert_point(AutomationPoint {
                    id: allocated_point.ok_or(AutomationError::InvalidCommand)?,
                    position: *position,
                    value: *value,
                    outgoing: *outgoing,
                })?;
            }
            Self::MovePoint { point, .. } => {
                lane.remove_point(point.id)
                    .ok_or(AutomationError::MissingPoint(point.id))?;
                lane.insert_point(point.clone())?;
            }
        }
        Ok(())
    }
}

/// Persistable/runtime-neutral targets for dynamically created control panes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlItemTarget {
    Mixer { bus: Option<BusId> },
    Automation { lane: AutomationLaneId },
}

impl TryFrom<&EditorTarget> for ControlItemTarget {
    type Error = ControlAdapterError;

    fn try_from(target: &EditorTarget) -> Result<Self, Self::Error> {
        match target {
            EditorTarget::Mixer { bus_id } => Ok(Self::Mixer {
                bus: bus_id.map(BusId::from_raw),
            }),
            EditorTarget::AutomationLane { id } => Ok(Self::Automation {
                lane: AutomationLaneId::from_raw(*id),
            }),
            _ => Err(ControlAdapterError::UnsupportedTarget),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MixerItemState {
    pub target_bus: Option<BusId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutomationItemState {
    pub target_lane: AutomationLaneId,
    pub selected_point: Option<AutomationPointId>,
    pub cursor_coordinate: i64,
    pub view_start: i64,
    pub view_end: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlItemState {
    Mixer(MixerItemState),
    Automation(AutomationItemState),
}

impl ControlItemState {
    pub const fn target(&self) -> ControlItemTarget {
        match self {
            Self::Mixer(state) => ControlItemTarget::Mixer {
                bus: state.target_bus,
            },
            Self::Automation(state) => ControlItemTarget::Automation {
                lane: state.target_lane,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeterValue {
    pub peak_db: f32,
    pub rms_db: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MixerMeterSnapshot {
    /// Monotonic engine publication sequence; independent from project edits.
    pub sequence: u64,
    pub buses: BTreeMap<BusId, MeterValue>,
}

impl MixerMeterSnapshot {
    pub fn sanitized(mut self) -> Self {
        self.buses
            .retain(|_, value| value.peak_db.is_finite() && value.rms_db.is_finite());
        for value in self.buses.values_mut() {
            value.peak_db = value.peak_db.clamp(-120.0, 24.0);
            value.rms_db = value.rms_db.clamp(-120.0, value.peak_db);
        }
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderPhase {
    Empty,
    Priming,
    Ready,
    Stale,
    Updating,
    Failed,
}

/// Small, stable presentation adapter over render service/runtime snapshots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlRenderStatus {
    pub phase: RenderPhase,
    pub has_active_audio: bool,
    pub candidate_ready: bool,
    pub publication_in_flight: bool,
    pub failure: Option<String>,
    pub starvation_events: u64,
    pub starved_frames: u64,
}

impl ControlRenderStatus {
    pub fn from_snapshots(
        service: &RenderServiceStatus,
        renderer: Option<CohortRendererStatus>,
    ) -> Self {
        let (phase, has_active_audio, candidate_ready, publication_in_flight, failure) =
            match &service.availability {
                RenderAvailability::Empty => (RenderPhase::Empty, false, false, false, None),
                RenderAvailability::Priming { .. } => {
                    (RenderPhase::Priming, false, false, false, None)
                }
                RenderAvailability::Ready { .. } => (RenderPhase::Ready, true, false, false, None),
                RenderAvailability::Stale {
                    candidate_ready,
                    publication_in_flight,
                    ..
                } => (
                    RenderPhase::Stale,
                    true,
                    *candidate_ready,
                    *publication_in_flight,
                    None,
                ),
                RenderAvailability::Updating {
                    candidate_ready,
                    publication_in_flight,
                    ..
                } => (
                    RenderPhase::Updating,
                    true,
                    *candidate_ready,
                    *publication_in_flight,
                    None,
                ),
                RenderAvailability::Failed {
                    active, failure, ..
                } => (
                    RenderPhase::Failed,
                    active.is_some(),
                    false,
                    false,
                    Some(failure.message.clone()),
                ),
            };
        let renderer = renderer.unwrap_or_default();
        Self {
            phase,
            has_active_audio,
            candidate_ready,
            publication_in_flight: publication_in_flight || renderer.publication_queued,
            failure,
            starvation_events: renderer.starvation_events,
            starved_frames: renderer.starved_frames,
        }
    }

    pub fn label(&self) -> String {
        let phase = match self.phase {
            RenderPhase::Empty => "NO RENDER",
            RenderPhase::Priming => "RENDERING",
            RenderPhase::Ready => "RENDER READY",
            RenderPhase::Stale => "PLAYING PRIOR REVISION",
            RenderPhase::Updating => "UPDATING RENDER",
            RenderPhase::Failed if self.has_active_audio => "RENDER FAILED · PRIOR AUDIO ACTIVE",
            RenderPhase::Failed => "RENDER FAILED",
        };
        if self.starvation_events == 0 {
            phase.into()
        } else {
            format!("{phase} · {} XRUNS", self.starvation_events)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlAdapterError {
    UnsupportedTarget,
}

impl fmt::Display for ControlAdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedTarget => {
                write!(f, "workspace target is not a mixer or automation item")
            }
        }
    }
}

impl Error for ControlAdapterError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::{
        MixerTarget, ParameterAddress, ParameterDescriptor, ParameterUnit, SmoothingPolicy,
        TimeDomain, ValueMapping,
    };
    use crate::mixer::BusKind;

    #[test]
    fn stale_mixer_action_cannot_build_a_command() {
        let mut graph = MixerGraph::default();
        let bus = graph.add_bus(BusKind::Source, "Voice").unwrap();
        let intent = MixerActionIntent::new(
            graph.revision(),
            MixerAction::SetGainDb { bus, gain_db: -6.0 },
        );
        let intervening =
            MixerCommand::build("intervening", &graph, |graph| graph.set_pan(bus, 0.25)).unwrap();
        intervening.apply(&mut graph).unwrap();
        assert!(matches!(
            intent.command(&graph),
            Err(MixerError::RevisionConflict { .. })
        ));
    }

    #[test]
    fn automation_point_gesture_builds_one_guarded_intent() {
        let mut graph = AutomationGraph::new();
        let address = ParameterAddress::Mixer(MixerTarget::BusGain(2));
        graph
            .register_parameter(ParameterDescriptor {
                address: address.clone(),
                name: "Gain".into(),
                unit: ParameterUnit::Decibels,
                minimum: -72.0,
                maximum: 12.0,
                default: 0.0,
                mapping: ValueMapping::Linear,
                smoothing: SmoothingPolicy::None,
            })
            .unwrap();
        let lane = graph
            .create_lane("Gain", address, TimeDomain::Beats)
            .unwrap();
        let point = AutomationPoint {
            id: graph.next_point_id_candidate().unwrap(),
            position: crate::automation::TimePosition::Beats(crate::automation::BeatTime(960)),
            value: -6.0,
            outgoing: SegmentShape::Linear,
        };
        let action = AutomationActionIntent::new(
            graph.revision(),
            AutomationAction::InsertPoint {
                lane,
                position: point.position,
                value: point.value,
                outgoing: point.outgoing,
            },
        );

        let first = action.legacy_intent(&graph).unwrap();
        graph.apply_intent(&first).unwrap();
        assert_eq!(graph.lane(lane).unwrap().points().len(), 1);
        assert!(matches!(
            action.legacy_intent(&graph),
            Err(AutomationError::RevisionConflict { .. })
        ));
    }

    #[test]
    fn dynamic_workspace_targets_retain_exact_control_identity() {
        assert_eq!(
            ControlItemTarget::try_from(&EditorTarget::Mixer { bus_id: Some(17) }).unwrap(),
            ControlItemTarget::Mixer {
                bus: Some(BusId::from_raw(17))
            }
        );
        assert_eq!(
            ControlItemTarget::try_from(&EditorTarget::AutomationLane { id: 29 }).unwrap(),
            ControlItemTarget::Automation {
                lane: AutomationLaneId::from_raw(29)
            }
        );
    }

    #[test]
    fn meter_adapter_drops_invalid_values_and_clamps_engine_ranges() {
        let valid = BusId::from_raw(2);
        let invalid = BusId::from_raw(3);
        let snapshot = MixerMeterSnapshot {
            sequence: 8,
            buses: BTreeMap::from([
                (
                    valid,
                    MeterValue {
                        peak_db: 30.0,
                        rms_db: 40.0,
                    },
                ),
                (
                    invalid,
                    MeterValue {
                        peak_db: f32::NAN,
                        rms_db: -12.0,
                    },
                ),
            ]),
        }
        .sanitized();

        assert_eq!(snapshot.buses.len(), 1);
        assert_eq!(snapshot.buses[&valid].peak_db, 24.0);
        assert_eq!(snapshot.buses[&valid].rms_db, 24.0);
    }
}
