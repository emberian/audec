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
    AutomationCommand, AutomationError, AutomationGraph, AutomationIntent, AutomationLane,
    AutomationLaneId, AutomationPoint, AutomationPointId, BindingMode, LaneChange,
    ParameterAddress, SegmentShape, TimeDomain, TimePosition,
};
use crate::command::{claims_for_commands, CommandEnvelope, DomainCommand};
use crate::command_record::{CoalesceToken, CommandAddress};
use crate::daw_project::ProjectDomain;
use crate::mixer::{
    BusId, BusKind, MixerCommand, MixerError, MixerGraph, ProcessorId, SendId, SendTap,
};
use crate::render_plan::{BusTap, RenderScope};
use crate::render_products::{PlaybackCohort, PlaybackCohortId, RenderProductId};
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

/// How a control value was entered. This is command metadata, never a second
/// copy of the value: aggregate truth still comes exclusively from the graph
/// snapshot used by [`ControlSessionAdapter`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ControlEdit {
    #[default]
    Discrete,
    /// Repeated +/- buttons, keyboard nudges, and committed numeric fields.
    Numeric,
    /// A stable series allocated by a view at pointer-down. A view may publish
    /// multiple values for the series, though the built-in views publish once
    /// on pointer-up.
    Gesture { series: u64 },
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
    pub edit: ControlEdit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MixerNumericTarget {
    Gain(BusId),
    Pan(BusId),
    SendLevel(SendId),
    InsertWet(ProcessorId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlNumericError {
    NonFinite,
    OutOfRange { control: &'static str },
    MissingTarget,
    InvalidEdit(String),
}

impl fmt::Display for ControlNumericError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFinite => write!(formatter, "numeric value must be finite"),
            Self::OutOfRange { control } => write!(formatter, "{control} value is out of range"),
            Self::MissingTarget => write!(formatter, "numeric control target no longer exists"),
            Self::InvalidEdit(error) => write!(formatter, "numeric edit is invalid: {error}"),
        }
    }
}

impl Error for ControlNumericError {}

impl MixerActionIntent {
    pub const fn new(expected_revision: u64, action: MixerAction) -> Self {
        Self {
            expected_revision,
            action,
            edit: ControlEdit::Discrete,
        }
    }

    pub const fn with_edit(mut self, edit: ControlEdit) -> Self {
        self.edit = edit;
        self
    }

    /// Build an exact typed numeric edit from the current authoritative
    /// snapshot. Number-input components can call this without keeping a
    /// second copy of the mixer value.
    pub fn exact_value(
        graph: &MixerGraph,
        target: MixerNumericTarget,
        value: f64,
    ) -> Result<Self, ControlNumericError> {
        if !value.is_finite() {
            return Err(ControlNumericError::NonFinite);
        }
        let action = match target {
            MixerNumericTarget::Gain(bus) => {
                if graph.bus(bus).is_none() {
                    return Err(ControlNumericError::MissingTarget);
                }
                if !(-72.0..=12.0).contains(&value) {
                    return Err(ControlNumericError::OutOfRange { control: "gain" });
                }
                MixerAction::SetGainDb {
                    bus,
                    gain_db: value as f32,
                }
            }
            MixerNumericTarget::Pan(bus) => {
                if graph.bus(bus).is_none() {
                    return Err(ControlNumericError::MissingTarget);
                }
                if !(-1.0..=1.0).contains(&value) {
                    return Err(ControlNumericError::OutOfRange { control: "pan" });
                }
                MixerAction::SetPan {
                    bus,
                    pan: value as f32,
                }
            }
            MixerNumericTarget::SendLevel(send) => {
                if !graph
                    .buses()
                    .flat_map(|bus| bus.sends())
                    .any(|candidate| candidate.id() == send)
                {
                    return Err(ControlNumericError::MissingTarget);
                }
                if !(-72.0..=12.0).contains(&value) {
                    return Err(ControlNumericError::OutOfRange {
                        control: "send level",
                    });
                }
                MixerAction::SetSendLevel {
                    send,
                    level_db: value as f32,
                }
            }
            MixerNumericTarget::InsertWet(processor) => {
                if graph.processor(processor).is_none() {
                    return Err(ControlNumericError::MissingTarget);
                }
                if !(0.0..=1.0).contains(&value) {
                    return Err(ControlNumericError::OutOfRange {
                        control: "insert mix",
                    });
                }
                MixerAction::SetInsertWet {
                    processor,
                    wet: value as f32,
                }
            }
        };
        let intent = Self::new(graph.revision(), action).with_edit(ControlEdit::Numeric);
        intent
            .command(graph)
            .map_err(|error| ControlNumericError::InvalidEdit(error.to_string()))?;
        Ok(intent)
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
    AddBus {
        kind: BusKind,
        name: String,
    },
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
            Self::AddBus { .. } => "add mixer bus",
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
        match self {
            Self::AddBus { kind, name } => graph.add_bus(*kind, name.clone()).map(|_| ()),
            Self::SetGainDb { bus, gain_db } => graph.set_gain_db(*bus, *gain_db),
            Self::SetPan { bus, pan } => graph.set_pan(*bus, *pan),
            Self::SetMuted { bus, muted } => graph.set_muted(*bus, *muted),
            Self::SetSoloed { bus, soloed } => graph.set_soloed(*bus, *soloed),
            Self::SetOutput { bus, target } => graph.set_output(*bus, *target),
            Self::AddSend {
                bus,
                target,
                tap,
                level_db,
            } => graph.add_send(*bus, *target, *tap, *level_db).map(|_| ()),
            Self::RemoveSend { send } => graph.remove_send(*send).map(|_| ()),
            Self::SetSendLevel { send, level_db } => graph.set_send_level(*send, *level_db),
            Self::SetSendMuted { send, muted } => graph.set_send_muted(*send, *muted),
            Self::SetSendTap { send, tap } => graph.set_send_tap(*send, *tap),
            Self::SetInsertBypassed {
                processor,
                bypassed,
            } => graph.set_insert_bypassed(*processor, *bypassed),
            Self::SetInsertWet { processor, wet } => graph.set_insert_wet(*processor, *wet),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AutomationActionIntent {
    pub expected_revision: u64,
    pub action: AutomationAction,
    pub edit: ControlEdit,
}

impl AutomationActionIntent {
    pub const fn new(expected_revision: u64, action: AutomationAction) -> Self {
        Self {
            expected_revision,
            action,
            edit: ControlEdit::Discrete,
        }
    }

    pub const fn with_edit(mut self, edit: ControlEdit) -> Self {
        self.edit = edit;
        self
    }

    /// Build one exact position/value replacement for an existing point.
    /// Range, step mapping, time domain, neighbor collision, and revision are
    /// validated against the supplied controller snapshot before dispatch.
    pub fn exact_point(
        graph: &AutomationGraph,
        lane: AutomationLaneId,
        point: AutomationPointId,
        position: TimePosition,
        value: f64,
    ) -> Result<Self, ControlNumericError> {
        if !value.is_finite() {
            return Err(ControlNumericError::NonFinite);
        }
        let lane_snapshot = graph.lane(lane).ok_or(ControlNumericError::MissingTarget)?;
        let descriptor = graph
            .descriptors()
            .find(|descriptor| descriptor.address == lane_snapshot.target)
            .ok_or(ControlNumericError::MissingTarget)?;
        if position.domain() != lane_snapshot.time_domain {
            return Err(ControlNumericError::InvalidEdit(
                "point time domain does not match its lane".into(),
            ));
        }
        if descriptor.constrain(value) != value {
            return Err(ControlNumericError::OutOfRange {
                control: "automation parameter",
            });
        }
        if lane_snapshot.points().iter().any(|candidate| {
            candidate.id != point && candidate.position.coordinate() == position.coordinate()
        }) {
            return Err(ControlNumericError::InvalidEdit(
                "point time collides with another point".into(),
            ));
        }
        let mut replacement = lane_snapshot
            .points()
            .iter()
            .find(|candidate| candidate.id == point)
            .cloned()
            .ok_or(ControlNumericError::MissingTarget)?;
        replacement.position = position;
        replacement.value = value;
        let intent = Self::new(
            graph.revision(),
            AutomationAction::MovePoint {
                lane,
                point: replacement,
            },
        )
        .with_edit(ControlEdit::Numeric);
        intent
            .legacy_intent(graph)
            .map_err(|error| ControlNumericError::InvalidEdit(error.to_string()))?;
        Ok(intent)
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
        if let AutomationAction::CreateLane {
            name,
            target,
            domain,
            binding,
        } = &self.action
        {
            if !graph
                .descriptors()
                .any(|descriptor| descriptor.address == *target)
            {
                return Err(AutomationError::MissingParameter(target.clone()));
            }
            let id = graph.next_lane_id_candidate()?;
            let mut lane = AutomationLane::new(id, name.clone(), target.clone(), *domain);
            lane.binding = *binding;
            return Ok(AutomationIntent::new(
                self.expected_revision,
                AutomationCommand {
                    label: self.action.label().into(),
                    changes: vec![LaneChange {
                        before: None,
                        after: Some(lane),
                    }],
                },
            ));
        }
        let lane_id = self
            .action
            .lane()
            .expect("non-creation automation actions always target a lane");
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
    CreateLane {
        name: String,
        target: ParameterAddress,
        domain: TimeDomain,
        binding: BindingMode,
    },
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
    pub const fn lane(&self) -> Option<AutomationLaneId> {
        match self {
            Self::CreateLane { .. } => None,
            Self::SetLaneEnabled { lane, .. }
            | Self::SetLaneBinding { lane, .. }
            | Self::SetPointShape { lane, .. }
            | Self::DeletePoint { lane, .. }
            | Self::InsertPoint { lane, .. }
            | Self::MovePoint { lane, .. } => Some(*lane),
        }
    }

    pub const fn label(&self) -> &'static str {
        match self {
            Self::CreateLane { .. } => "create automation lane",
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
            Self::CreateLane { .. } => return Err(AutomationError::InvalidCommand),
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

/// Aggregate operation produced at the controller boundary. History carries
/// the aggregate generation observed by this adapter; surface views are not
/// asked to guess it from a mixer or automation revision.
#[derive(Clone, Debug, PartialEq)]
pub enum ControlSessionOperation {
    Execute(CommandEnvelope),
    History {
        expected_aggregate_revision: u64,
        direction: HistoryDirection,
    },
}

/// Pure, borrowing adapter from semantic control actions to the project's
/// single command language. It allocates no domain mirrors and mutates
/// nothing. Recreate it from each freshly published project snapshot.
pub struct ControlSessionAdapter<'a> {
    aggregate_revision: u64,
    editor_session: u64,
    mixer: &'a MixerGraph,
    automation: &'a AutomationGraph,
}

impl<'a> ControlSessionAdapter<'a> {
    pub const fn new(
        aggregate_revision: u64,
        editor_session: u64,
        mixer: &'a MixerGraph,
        automation: &'a AutomationGraph,
    ) -> Self {
        Self {
            aggregate_revision,
            editor_session,
            mixer,
            automation,
        }
    }

    pub fn adapt(
        &self,
        action: &ControlAction,
    ) -> Result<ControlSessionOperation, ControlSessionAdapterError> {
        match action {
            ControlAction::Mixer(intent) => {
                let command = DomainCommand::Mixer(intent.command(self.mixer)?);
                Ok(ControlSessionOperation::Execute(self.envelope(
                    intent.action.label(),
                    intent.edit,
                    &intent.action,
                    command,
                )))
            }
            ControlAction::Automation(intent) => {
                let AutomationIntent { command, .. } = intent.legacy_intent(self.automation)?;
                let command = DomainCommand::Automation(command);
                Ok(ControlSessionOperation::Execute(self.envelope(
                    intent.action.label(),
                    intent.edit,
                    &intent.action,
                    command,
                )))
            }
            ControlAction::History(intent) => {
                let actual = match intent.surface {
                    ControlSurface::Mixer => self.mixer.revision(),
                    ControlSurface::Automation => self.automation.revision(),
                };
                if intent.expected_revision != actual {
                    return Err(ControlSessionAdapterError::SurfaceRevisionConflict {
                        surface: intent.surface,
                        expected: intent.expected_revision,
                        actual,
                    });
                }
                Ok(ControlSessionOperation::History {
                    expected_aggregate_revision: self.aggregate_revision,
                    direction: intent.direction,
                })
            }
        }
    }

    fn envelope(
        &self,
        label: &str,
        edit: ControlEdit,
        semantic: &impl ControlCoalescing,
        command: DomainCommand,
    ) -> CommandEnvelope {
        let commands = vec![command];
        CommandEnvelope {
            label: label.into(),
            base_revision: self.aggregate_revision,
            coalesce: semantic.coalesce_token(edit, self.editor_session),
            id_claims: claims_for_commands(&commands),
            commands,
        }
    }
}

trait ControlCoalescing {
    fn coalesce_token(&self, edit: ControlEdit, editor_session: u64) -> Option<CoalesceToken>;
}

impl ControlCoalescing for MixerAction {
    fn coalesce_token(&self, edit: ControlEdit, editor_session: u64) -> Option<CoalesceToken> {
        let (kind, raw) = match self {
            Self::AddBus { .. } => return None,
            Self::SetGainDb { bus, .. } => (1, bus.get()),
            Self::SetPan { bus, .. } => (2, bus.get()),
            Self::SetSendLevel { send, .. } => (3, send.get()),
            Self::SetInsertWet { processor, .. } => (4, processor.get()),
            _ => return None,
        };
        let gesture_kind = exact_control_series(kind, raw, edit)?;
        Some(CoalesceToken {
            editor_session,
            gesture_kind,
            // MixerCommand is currently aggregate-granular, and its reported
            // affected address is correspondingly the mixer domain. `kind`
            // and `raw` above still prevent cross-control merging exactly.
            primary: CommandAddress::WholeDomain(ProjectDomain::Mixer),
        })
    }
}

impl ControlCoalescing for AutomationAction {
    fn coalesce_token(&self, edit: ControlEdit, editor_session: u64) -> Option<CoalesceToken> {
        let (kind, primary) = match self {
            Self::CreateLane { .. } => return None,
            Self::MovePoint { point, .. } => (1, CommandAddress::AutomationPoint(point.id)),
            Self::SetPointShape { point, .. } => (2, CommandAddress::AutomationPoint(*point)),
            Self::SetLaneEnabled { lane, .. } | Self::SetLaneBinding { lane, .. } => {
                (3, CommandAddress::AutomationLane(*lane))
            }
            Self::InsertPoint { lane, .. } => (4, CommandAddress::AutomationLane(*lane)),
            Self::DeletePoint { .. } => return None,
        };
        let gesture_kind = exact_control_series(kind, 0, edit)?;
        Some(CoalesceToken {
            editor_session,
            gesture_kind,
            primary,
        })
    }
}

fn exact_control_series(kind: u64, raw: u64, edit: ControlEdit) -> Option<u64> {
    let series = match edit {
        ControlEdit::Discrete => return None,
        ControlEdit::Numeric => 0,
        ControlEdit::Gesture { series } => series.checked_add(1)?,
    };
    if series >= (1 << 16) {
        return None;
    }
    // Checked mixed-radix packing is collision-free. Extremely large durable
    // IDs/series simply opt out of coalescing rather than using a lossy hash.
    raw.checked_mul(16)?
        .checked_add(kind)?
        .checked_mul(1 << 16)?
        .checked_add(series)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlSessionAdapterError {
    Mixer(String),
    Automation(String),
    SurfaceRevisionConflict {
        surface: ControlSurface,
        expected: u64,
        actual: u64,
    },
}

impl From<MixerError> for ControlSessionAdapterError {
    fn from(error: MixerError) -> Self {
        Self::Mixer(error.to_string())
    }
}

impl From<AutomationError> for ControlSessionAdapterError {
    fn from(error: AutomationError) -> Self {
        Self::Automation(error.to_string())
    }
}

impl fmt::Display for ControlSessionAdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mixer(error) => write!(f, "mixer control action failed: {error}"),
            Self::Automation(error) => write!(f, "automation control action failed: {error}"),
            Self::SurfaceRevisionConflict {
                surface,
                expected,
                actual,
            } => write!(
                f,
                "{surface:?} control revision conflict: expected {expected}, actual {actual}"
            ),
        }
    }
}

impl Error for ControlSessionAdapterError {}

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

#[derive(Clone, Debug, PartialEq)]
pub struct MixerMeterSnapshot {
    /// Exact immutable cohort which supplied every value below.
    pub audible: PlaybackCohortId,
    /// Monotonic engine publication sequence, repeated for simple UI ordering.
    pub sequence: u64,
    /// Product identities actually inspected for each strip.
    pub products: BTreeMap<BusId, Vec<RenderProductId>>,
    pub buses: BTreeMap<BusId, MeterValue>,
}

impl MixerMeterSnapshot {
    pub fn is_audible_in(&self, status: &ControlRenderStatus) -> bool {
        status.active.as_ref() == Some(&self.audible)
    }

    pub const fn aggregate_revision(&self) -> u64 {
        self.audible.plan.revisions.aggregate
    }

    /// Summarize immutable PCM from one acknowledged playback cohort. This is
    /// intentionally a rendered-product meter, not a fake realtime animation.
    /// Output taps win over post-fader taps, which win over pre-fader taps.
    pub fn from_audible_cohort(cohort: &PlaybackCohort, master: BusId) -> Self {
        #[derive(Default)]
        struct Accumulator {
            priority: u8,
            peak: f64,
            square_sum: f64,
            samples: u64,
            products: Vec<RenderProductId>,
        }

        let mut accumulators: BTreeMap<BusId, Accumulator> = BTreeMap::new();
        for entry in cohort.products() {
            let (bus, priority) = match entry.slot.scope {
                RenderScope::Master => (master, 4),
                RenderScope::Bus {
                    bus,
                    tap: BusTap::Output,
                } => (BusId::from_raw(bus), 3),
                RenderScope::Bus {
                    bus,
                    tap: BusTap::PostFader,
                } => (BusId::from_raw(bus), 2),
                RenderScope::Bus {
                    bus,
                    tap: BusTap::PreFader,
                } => (BusId::from_raw(bus), 1),
                RenderScope::Track(_) | RenderScope::Explanation(_) => continue,
            };
            let accumulator = accumulators.entry(bus).or_default();
            if priority < accumulator.priority {
                continue;
            }
            if priority > accumulator.priority {
                *accumulator = Accumulator {
                    priority,
                    ..Accumulator::default()
                };
            }
            for sample in entry.product.interleaved() {
                let amplitude = f64::from(sample.abs());
                accumulator.peak = accumulator.peak.max(amplitude);
                accumulator.square_sum += amplitude * amplitude;
                accumulator.samples = accumulator.samples.saturating_add(1);
            }
            if accumulator.products.last() != Some(&entry.product.id) {
                accumulator.products.push(entry.product.id);
            }
        }

        let mut buses = BTreeMap::new();
        let mut products = BTreeMap::new();
        for (bus, accumulator) in accumulators {
            let rms = if accumulator.samples == 0 {
                0.0
            } else {
                (accumulator.square_sum / accumulator.samples as f64).sqrt()
            };
            buses.insert(
                bus,
                MeterValue {
                    peak_db: amplitude_db(accumulator.peak),
                    rms_db: amplitude_db(rms),
                },
            );
            products.insert(bus, accumulator.products);
        }
        Self {
            audible: cohort.id.clone(),
            sequence: cohort.id.sequence,
            products,
            buses,
        }
        .sanitized()
    }

    pub fn sanitized(mut self) -> Self {
        self.buses
            .retain(|_, value| value.peak_db.is_finite() && value.rms_db.is_finite());
        for value in self.buses.values_mut() {
            value.peak_db = value.peak_db.clamp(-120.0, 24.0);
            value.rms_db = value.rms_db.clamp(-120.0, value.peak_db);
        }
        self.products
            .retain(|bus, products| self.buses.contains_key(bus) && !products.is_empty());
        self
    }
}

fn amplitude_db(amplitude: f64) -> f32 {
    if amplitude <= 1.0e-6 {
        -120.0
    } else {
        (20.0 * amplitude.log10()).clamp(-120.0, 24.0) as f32
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
    /// The cohort acknowledged by the audio side. Presentation adapters use
    /// this exact identity to reject meters from staged or retired PCM.
    pub active: Option<PlaybackCohortId>,
    pub has_active_audio: bool,
    pub candidate_ready: bool,
    pub publication_in_flight: bool,
    pub failure: Option<String>,
    pub starvation_events: u64,
    pub starved_frames: u64,
}

impl ControlRenderStatus {
    pub fn audible_project_revision(&self) -> Option<crate::render_plan::ProjectRevisionStamp> {
        self.active.as_ref().map(|active| active.plan.revisions)
    }

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
            active: service.active.clone(),
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
    use crate::arrangement::{
        ArrangementEditor, AssetId, Frame, FrameRange, SourceRange, TrackId, TrackKind,
    };
    use crate::audio::AudioFormat;
    use crate::automation::{
        MixerTarget, ParameterAddress, ParameterDescriptor, ParameterUnit, SmoothingPolicy,
        TimeDomain, ValueMapping,
    };
    use crate::daw_render::{
        compile_render_schedule, render_pcm_reference, PcmAsset, ProcessorRuntimeInfo,
        RenderCancellation, RenderCompileRequest, RenderWindow,
    };
    use crate::mixer::BusKind;
    use crate::render_plan::{
        EngineRecipeStamp, ExactDigest, ProjectRevisionStamp, RenderFormat, RenderPlanId,
        RenderSpan,
    };
    use crate::render_products::{
        CohortProduct, CohortProductProvenance, ProductPartition, RenderProduct, RenderProductKey,
        RenderSlot,
    };
    use crate::sequencer::{Sequencer, TempoMap};
    use std::collections::BTreeSet;

    fn digest(byte: u8) -> ExactDigest {
        ExactDigest::new([byte; 32])
    }

    fn rendered_cohort(sequence: u64, samples: &[f32]) -> PlaybackCohort {
        let format = RenderFormat::new(48_000, 2).unwrap();
        let engine = EngineRecipeStamp::new(1, format, 512, 0, digest(2)).unwrap();
        let span = RenderSpan::new(0, (samples.len() / 2) as i64).unwrap();
        let plan = RenderPlanId::new(
            4,
            digest(3),
            ProjectRevisionStamp {
                aggregate: 19,
                mixer: 7,
                ..ProjectRevisionStamp::default()
            },
            span,
            engine,
            Vec::new(),
        )
        .unwrap();
        let slot = RenderSlot {
            scope: RenderScope::Master,
            span,
        };
        let key = RenderProductKey::new(
            plan.clone(),
            RenderScope::Master,
            span,
            ProductPartition::WholeBounce,
            digest(5),
        )
        .unwrap();
        let product =
            Arc::new(RenderProduct::new(digest(6), key, Arc::from(samples.to_vec())).unwrap());
        PlaybackCohort::new(
            PlaybackCohortId { plan, sequence },
            None,
            vec![slot.clone()],
            vec![CohortProduct {
                slot,
                product,
                provenance: CohortProductProvenance::RenderedForTarget,
            }],
        )
        .unwrap()
    }

    fn reference_bounce(
        arrangement: &crate::arrangement::ArrangementState,
        asset: AssetId,
        track: TrackId,
        source_bus: BusId,
        mixer: &MixerGraph,
        automation: &AutomationGraph,
    ) -> Vec<f32> {
        let sequencer = Sequencer::new(TempoMap::common_time(48_000, 120.0).unwrap());
        let track_buses = BTreeMap::from([(track, source_bus)]);
        let processors: BTreeMap<ProcessorId, ProcessorRuntimeInfo> = BTreeMap::new();
        let schedule = compile_render_schedule(
            RenderCompileRequest {
                arrangement,
                sequencer: &sequencer,
                automation,
                mixer,
                track_buses: &track_buses,
                processors: &processors,
                window: RenderWindow::new(0, 6).unwrap(),
                output_channels: 2,
                block_frames: 4,
                performance_seed: 11,
            },
            &RenderCancellation::new(),
        )
        .unwrap();
        let assets = BTreeMap::from([(
            asset,
            PcmAsset::new(
                AudioFormat::new(48_000, 1).unwrap(),
                Arc::from([1.0, 1.0, 1.0, 1.0]),
            )
            .unwrap(),
        )]);
        render_pcm_reference(
            &schedule,
            &assets,
            schedule.window(),
            &RenderCancellation::new(),
        )
        .unwrap()
        .interleaved
    }

    fn apply_mixer_action(
        mixer: &mut MixerGraph,
        automation: &AutomationGraph,
        action: MixerAction,
    ) {
        let intent = ControlAction::Mixer(MixerActionIntent::new(mixer.revision(), action));
        let operation = ControlSessionAdapter::new(1, 2, mixer, automation)
            .adapt(&intent)
            .unwrap();
        let ControlSessionOperation::Execute(envelope) = operation else {
            unreachable!()
        };
        let DomainCommand::Mixer(command) = &envelope.commands[0] else {
            unreachable!()
        };
        command.apply(mixer).unwrap();
    }

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
    fn session_adapter_builds_one_coalescible_aggregate_command_without_mutation() {
        let mut mixer = MixerGraph::default();
        let bus = mixer.add_bus(BusKind::Source, "Voice").unwrap();
        let automation = AutomationGraph::new();
        let before = mixer.clone();
        let action = ControlAction::Mixer(
            MixerActionIntent::new(
                mixer.revision(),
                MixerAction::SetGainDb { bus, gain_db: -9.0 },
            )
            .with_edit(ControlEdit::Numeric),
        );
        let operation = ControlSessionAdapter::new(31, 8, &mixer, &automation)
            .adapt(&action)
            .unwrap();
        let ControlSessionOperation::Execute(envelope) = operation else {
            panic!("mixer action must execute")
        };
        assert_eq!(envelope.base_revision, 31);
        assert_eq!(envelope.commands.len(), 1);
        assert!(envelope.coalesce.is_some());
        assert_eq!(envelope.id_claims, BTreeSet::new());
        assert_eq!(mixer, before, "adapter must not mirror-mutate the graph");

        let DomainCommand::Mixer(command) = &envelope.commands[0] else {
            panic!("mixer action must lower to mixer command")
        };
        command.apply(&mut mixer).unwrap();
        assert_eq!(mixer.bus(bus).unwrap().fader().gain_db(), -9.0);
    }

    #[test]
    fn session_adapter_rejects_stale_surface_history_before_aggregate_dispatch() {
        let mut mixer = MixerGraph::default();
        mixer.add_bus(BusKind::Source, "Voice").unwrap();
        let automation = AutomationGraph::new();
        let action = ControlAction::History(ControlHistoryIntent {
            surface: ControlSurface::Mixer,
            expected_revision: mixer.revision().checked_add(1).unwrap(),
            direction: HistoryDirection::Undo,
        });
        assert!(matches!(
            ControlSessionAdapter::new(44, 3, &mixer, &automation).adapt(&action),
            Err(ControlSessionAdapterError::SurfaceRevisionConflict { .. })
        ));
    }

    #[test]
    fn numeric_and_pointer_coalescing_series_are_exact_and_target_specific() {
        let numeric = MixerAction::SetGainDb {
            bus: BusId::from_raw(7),
            gain_db: -3.0,
        }
        .coalesce_token(ControlEdit::Numeric, 91)
        .unwrap();
        let same_target = MixerAction::SetGainDb {
            bus: BusId::from_raw(7),
            gain_db: -6.0,
        }
        .coalesce_token(ControlEdit::Numeric, 91)
        .unwrap();
        let other_target = MixerAction::SetGainDb {
            bus: BusId::from_raw(8),
            gain_db: -6.0,
        }
        .coalesce_token(ControlEdit::Numeric, 91)
        .unwrap();
        let pointer_series = MixerAction::SetGainDb {
            bus: BusId::from_raw(7),
            gain_db: -6.0,
        }
        .coalesce_token(ControlEdit::Gesture { series: 4 }, 91)
        .unwrap();
        assert_eq!(numeric, same_target);
        assert_ne!(numeric, other_target);
        assert_ne!(numeric, pointer_series);
    }

    #[test]
    fn exact_numeric_entries_validate_target_range_and_lane_order() {
        let mut mixer = MixerGraph::default();
        let bus = mixer.add_bus(BusKind::Source, "Voice").unwrap();
        let gain =
            MixerActionIntent::exact_value(&mixer, MixerNumericTarget::Gain(bus), -7.25).unwrap();
        assert_eq!(gain.edit, ControlEdit::Numeric);
        assert!(matches!(
            gain.action,
            MixerAction::SetGainDb { gain_db, .. } if gain_db == -7.25
        ));
        assert!(matches!(
            MixerActionIntent::exact_value(&mixer, MixerNumericTarget::Pan(bus), 1.01),
            Err(ControlNumericError::OutOfRange { control: "pan" })
        ));

        let mut automation = AutomationGraph::new();
        let address = ParameterAddress::Mixer(MixerTarget::BusGain(bus.get()));
        automation
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
        let lane = automation
            .create_lane("Gain", address, TimeDomain::Frames)
            .unwrap();
        let first = automation
            .insert_point(
                lane,
                TimePosition::Frames(crate::automation::ProjectFrame(10)),
                0.0,
                SegmentShape::Linear,
            )
            .unwrap();
        automation
            .insert_point(
                lane,
                TimePosition::Frames(crate::automation::ProjectFrame(20)),
                -6.0,
                SegmentShape::Linear,
            )
            .unwrap();
        let exact = AutomationActionIntent::exact_point(
            &automation,
            lane,
            first,
            TimePosition::Frames(crate::automation::ProjectFrame(15)),
            -3.125,
        )
        .unwrap();
        assert_eq!(exact.edit, ControlEdit::Numeric);
        assert!(AutomationActionIntent::exact_point(
            &automation,
            lane,
            first,
            TimePosition::Frames(crate::automation::ProjectFrame(20)),
            -3.0,
        )
        .is_err());
    }

    #[test]
    fn lane_creation_is_revision_guarded_and_allocated_only_on_apply() {
        let mut graph = AutomationGraph::new();
        let address = ParameterAddress::Mixer(MixerTarget::BusPan(4));
        graph
            .register_parameter(ParameterDescriptor {
                address: address.clone(),
                name: "Pan".into(),
                unit: ParameterUnit::Normalized,
                minimum: -1.0,
                maximum: 1.0,
                default: 0.0,
                mapping: ValueMapping::Linear,
                smoothing: SmoothingPolicy::None,
            })
            .unwrap();
        let intent = AutomationActionIntent::new(
            graph.revision(),
            AutomationAction::CreateLane {
                name: "Pan write".into(),
                target: address,
                domain: TimeDomain::Beats,
                binding: BindingMode::Replace,
            },
        );
        let command = intent.legacy_intent(&graph).unwrap();
        assert_eq!(graph.lanes().count(), 0, "lowering must be read-only");
        graph.apply_intent(&command).unwrap();
        assert_eq!(graph.lanes().count(), 1);
        assert!(matches!(
            intent.legacy_intent(&graph),
            Err(AutomationError::RevisionConflict { .. })
        ));
    }

    #[test]
    fn rendered_meter_snapshot_has_exact_audible_product_provenance() {
        let master = BusId::from_raw(1);
        let cohort = rendered_cohort(12, &[1.0, -1.0, 0.5, -0.5]);
        let snapshot = MixerMeterSnapshot::from_audible_cohort(&cohort, master);
        assert_eq!(snapshot.audible, cohort.id);
        assert_eq!(snapshot.sequence, 12);
        assert_eq!(snapshot.aggregate_revision(), 19);
        assert_eq!(
            snapshot.products[&master],
            vec![cohort.products().next().unwrap().product.id]
        );
        assert_eq!(snapshot.buses[&master].peak_db, 0.0);
        assert!((snapshot.buses[&master].rms_db - -2.041_2).abs() < 1.0e-3);

        let retired = rendered_cohort(13, &[0.0, 0.0]);
        let status = ControlRenderStatus {
            phase: RenderPhase::Updating,
            active: Some(retired.id),
            has_active_audio: true,
            candidate_ready: true,
            publication_in_flight: false,
            failure: None,
            starvation_events: 0,
            starved_frames: 0,
        };
        assert!(!snapshot.is_audible_in(&status));
    }

    #[test]
    fn semantic_controls_change_the_same_deterministic_reference_bounce() {
        let mut editor = ArrangementEditor::new(48_000).unwrap();
        let track = editor.create_track("Audio", TrackKind::Audio).unwrap();
        let asset = AssetId::from_raw(90);
        editor
            .create_audio_clip(
                track,
                "Tone",
                FrameRange::new(Frame(1), Frame(5)).unwrap(),
                asset,
                SourceRange::new(0, 4).unwrap(),
            )
            .unwrap();
        let arrangement = editor.state().clone();
        let mut base = MixerGraph::new("Master");
        let source = base.add_bus(BusKind::Source, "Audio").unwrap();
        let other = base.add_bus(BusKind::Source, "Other").unwrap();
        let group = base.add_bus(BusKind::Group, "Group").unwrap();
        base.set_gain_db(group, -6.0).unwrap();
        let no_automation = AutomationGraph::new();
        let baseline = reference_bounce(&arrangement, asset, track, source, &base, &no_automation);
        let baseline_frame = &baseline[2..4];

        let mut gained = base.clone();
        apply_mixer_action(
            &mut gained,
            &no_automation,
            MixerAction::SetGainDb {
                bus: source,
                gain_db: -6.0,
            },
        );
        let gained_audio =
            reference_bounce(&arrangement, asset, track, source, &gained, &no_automation);
        assert!((gained_audio[2] / baseline_frame[0] - 10.0_f32.powf(-6.0 / 20.0)).abs() < 1.0e-5);

        let mut panned = base.clone();
        apply_mixer_action(
            &mut panned,
            &no_automation,
            MixerAction::SetPan {
                bus: source,
                pan: -1.0,
            },
        );
        let panned_audio =
            reference_bounce(&arrangement, asset, track, source, &panned, &no_automation);
        assert!(panned_audio[2] > baseline_frame[0]);
        assert!(panned_audio[3].abs() < 1.0e-6);

        for action in [
            MixerAction::SetMuted {
                bus: source,
                muted: true,
            },
            MixerAction::SetSoloed {
                bus: other,
                soloed: true,
            },
        ] {
            let mut silenced = base.clone();
            apply_mixer_action(&mut silenced, &no_automation, action);
            let audio = reference_bounce(
                &arrangement,
                asset,
                track,
                source,
                &silenced,
                &no_automation,
            );
            assert!(audio.iter().all(|sample| sample.abs() < 1.0e-6));
        }

        let mut routed = base.clone();
        apply_mixer_action(
            &mut routed,
            &no_automation,
            MixerAction::SetOutput {
                bus: source,
                target: group,
            },
        );
        let routed_audio =
            reference_bounce(&arrangement, asset, track, source, &routed, &no_automation);
        assert!(routed_audio[2] < baseline_frame[0]);

        let mut sent = base.clone();
        apply_mixer_action(
            &mut sent,
            &no_automation,
            MixerAction::AddSend {
                bus: source,
                target: group,
                tap: SendTap::PostFader,
                level_db: 0.0,
            },
        );
        let sent_audio =
            reference_bounce(&arrangement, asset, track, source, &sent, &no_automation);
        assert!(sent_audio[2] > baseline_frame[0]);
        let send = sent.bus(source).unwrap().sends()[0].id();
        apply_mixer_action(
            &mut sent,
            &no_automation,
            MixerAction::SetSendLevel {
                send,
                level_db: -12.0,
            },
        );
        let quieter_send =
            reference_bounce(&arrangement, asset, track, source, &sent, &no_automation);
        assert!(quieter_send[2] > baseline_frame[0]);
        assert!(quieter_send[2] < sent_audio[2]);
        apply_mixer_action(
            &mut sent,
            &no_automation,
            MixerAction::SetSendMuted { send, muted: true },
        );
        let muted_send =
            reference_bounce(&arrangement, asset, track, source, &sent, &no_automation);
        assert_eq!(muted_send, baseline);

        let mut automated = AutomationGraph::new();
        let address = ParameterAddress::Mixer(MixerTarget::BusGain(source.get()));
        automated
            .register_parameter(ParameterDescriptor {
                address: address.clone(),
                name: "Automated gain".into(),
                unit: ParameterUnit::Decibels,
                minimum: -72.0,
                maximum: 12.0,
                default: 0.0,
                mapping: ValueMapping::Linear,
                smoothing: SmoothingPolicy::None,
            })
            .unwrap();
        let lane = automated
            .create_lane("Automated gain", address, TimeDomain::Frames)
            .unwrap();
        let action = ControlAction::Automation(AutomationActionIntent::new(
            automated.revision(),
            AutomationAction::InsertPoint {
                lane,
                position: TimePosition::Frames(crate::automation::ProjectFrame(0)),
                value: -12.0,
                outgoing: SegmentShape::Hold,
            },
        ));
        let operation = ControlSessionAdapter::new(2, 2, &base, &automated)
            .adapt(&action)
            .unwrap();
        let ControlSessionOperation::Execute(envelope) = operation else {
            unreachable!()
        };
        let DomainCommand::Automation(command) = &envelope.commands[0] else {
            unreachable!()
        };
        automated.apply(command).unwrap();
        let automated_audio =
            reference_bounce(&arrangement, asset, track, source, &base, &automated);
        assert!(
            (automated_audio[2] / baseline_frame[0] - 10.0_f32.powf(-12.0 / 20.0)).abs() < 1.0e-5
        );
    }

    #[test]
    fn meter_adapter_drops_invalid_values_and_clamps_engine_ranges() {
        let valid = BusId::from_raw(2);
        let invalid = BusId::from_raw(3);
        let cohort = rendered_cohort(8, &[0.0, 0.0]);
        let product = cohort.products().next().unwrap().product.id;
        let snapshot = MixerMeterSnapshot {
            audible: cohort.id,
            sequence: 8,
            products: BTreeMap::from([(valid, vec![product]), (invalid, vec![product])]),
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
        assert_eq!(snapshot.products.len(), 1);
        assert_eq!(snapshot.buses[&valid].peak_db, 24.0);
        assert_eq!(snapshot.buses[&valid].rms_db, 24.0);
    }
}
