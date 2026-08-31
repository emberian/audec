//! DAW control surfaces for Audec's mixer and automation engines.
//!
//! Preferred integrations render controller-published snapshots and emit one
//! typed semantic action for each operation. Direct graph mutation remains an
//! explicitly named compatibility mode for the legacy six-pane host.

#[path = "control_actions.rs"]
pub mod control_actions;

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use gpui::{
    actions, canvas, div, point, prelude::*, px, quad, relative, rgb, rgba, App, Bounds, Context,
    FocusHandle, Focusable, IntoElement, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PathBuilder, Pixels, Render, SharedString, Window,
};

use crate::automation::{
    AutomationGraph, AutomationIntent, AutomationLane, AutomationLaneId, AutomationPoint,
    AutomationPointId, BeatFrameMap, BeatTime, BindingMode, FixedTempo, MixerTarget,
    ParameterAddress, ParameterDescriptor, ParameterUnit, ProjectFrame, SegmentShape,
    SmoothingPolicy, TimeDomain, TimePosition, ValueMapping, WriteMode, PPQ,
};
use crate::mixer::{
    BusId, BusKind, MixerCommand, MixerError, MixerGraph, PluginDescriptor, SendId, SendTap,
};
pub use control_actions::{
    AutomationAction, AutomationActionIntent, AutomationItemState, ControlAction,
    ControlActionCallback, ControlHistoryIntent, ControlIntegrationMode, ControlItemState,
    ControlItemTarget, ControlRenderStatus, ControlSurface, HistoryDirection, MeterValue,
    MixerAction, MixerActionIntent, MixerItemState, MixerMeterSnapshot,
};

actions!(
    audec_control_views,
    [ControlUndo, ControlRedo, DeleteAutomationPoint]
);

const BACKGROUND: u32 = 0x090b10;
const PANEL: u32 = 0x10141d;
const PANEL_ALT: u32 = 0x0d1118;
const BORDER: u32 = 0x252c38;
const TEXT: u32 = 0xe8edf5;
const MUTED: u32 = 0x8c98a9;
const DIM: u32 = 0x596579;
const CYAN: u32 = 0x50d8d7;
const MAGENTA: u32 = 0xf172b6;
const AMBER: u32 = 0xf6b760;
const LIME: u32 = 0xa7d877;

pub fn bind_control_view_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-z", ControlUndo, Some("AudecMixer")),
        KeyBinding::new("cmd-shift-z", ControlRedo, Some("AudecMixer")),
        KeyBinding::new("cmd-z", ControlUndo, Some("AudecAutomation")),
        KeyBinding::new("cmd-shift-z", ControlRedo, Some("AudecAutomation")),
        KeyBinding::new("backspace", DeleteAutomationPoint, Some("AudecAutomation")),
        KeyBinding::new("delete", DeleteAutomationPoint, Some("AudecAutomation")),
    ]);
}

/// Pluggable ownership seam for a project mixer and its history.
pub trait MixerBackend: 'static {
    fn snapshot(&self) -> MixerGraph;
    fn execute(&mut self, command: MixerCommand) -> Result<(), MixerError>;
    fn undo(&mut self) -> Result<bool, MixerError>;
    fn redo(&mut self) -> Result<bool, MixerError>;
}

/// In-memory command history used by standalone/pop-out mixer windows.
pub struct LocalMixerBackend {
    graph: MixerGraph,
    undo: VecDeque<MixerCommand>,
    redo: Vec<MixerCommand>,
    limit: usize,
}

impl LocalMixerBackend {
    pub fn new(graph: MixerGraph, history_limit: usize) -> Self {
        Self {
            graph,
            undo: VecDeque::new(),
            redo: Vec::new(),
            limit: history_limit,
        }
    }
}

impl MixerBackend for LocalMixerBackend {
    fn snapshot(&self) -> MixerGraph {
        self.graph.clone()
    }

    fn execute(&mut self, command: MixerCommand) -> Result<(), MixerError> {
        let inverse = command.inverse();
        command.apply(&mut self.graph)?;
        self.redo.clear();
        if self.limit > 0 {
            self.undo.push_back(inverse);
            while self.undo.len() > self.limit {
                self.undo.pop_front();
            }
        }
        Ok(())
    }

    fn undo(&mut self) -> Result<bool, MixerError> {
        let Some(command) = self.undo.pop_back() else {
            return Ok(false);
        };
        let redo = command.inverse();
        if let Err(error) = command.apply(&mut self.graph) {
            self.undo.push_back(command);
            return Err(error);
        }
        self.redo.push(redo);
        Ok(true)
    }

    fn redo(&mut self) -> Result<bool, MixerError> {
        let Some(command) = self.redo.pop() else {
            return Ok(false);
        };
        let undo = command.inverse();
        if let Err(error) = command.apply(&mut self.graph) {
            self.redo.push(command);
            return Err(error);
        }
        self.undo.push_back(undo);
        Ok(true)
    }
}

/// Mixer history over controller-owned truth. The view never keeps a graph
/// mirror: every snapshot and edit observes the shared graph under its lock.
pub struct SharedMixerBackend {
    shared_graph: Arc<Mutex<MixerGraph>>,
    undo: VecDeque<MixerCommand>,
    redo: Vec<MixerCommand>,
    limit: usize,
}

impl SharedMixerBackend {
    pub fn new(shared_graph: Arc<Mutex<MixerGraph>>, history_limit: usize) -> Self {
        Self {
            shared_graph,
            undo: VecDeque::new(),
            redo: Vec::new(),
            limit: history_limit,
        }
    }
}

impl MixerBackend for SharedMixerBackend {
    fn snapshot(&self) -> MixerGraph {
        self.shared_graph
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn execute(&mut self, command: MixerCommand) -> Result<(), MixerError> {
        let mut shared_graph = self
            .shared_graph
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let inverse = command.inverse();
        command.apply(&mut shared_graph)?;
        self.redo.clear();
        if self.limit > 0 {
            self.undo.push_back(inverse);
            while self.undo.len() > self.limit {
                self.undo.pop_front();
            }
        }
        Ok(())
    }

    fn undo(&mut self) -> Result<bool, MixerError> {
        let mut shared_graph = self
            .shared_graph
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(command) = self.undo.pop_back() else {
            return Ok(false);
        };
        let redo = command.inverse();
        if let Err(error) = command.apply(&mut shared_graph) {
            self.undo.push_back(command);
            return Err(error);
        }
        self.redo.push(redo);
        Ok(true)
    }

    fn redo(&mut self) -> Result<bool, MixerError> {
        let mut shared_graph = self
            .shared_graph
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(command) = self.redo.pop() else {
            return Ok(false);
        };
        let undo = command.inverse();
        if let Err(error) = command.apply(&mut shared_graph) {
            self.redo.push(command);
            return Err(error);
        }
        self.undo.push_back(undo);
        Ok(true)
    }
}

pub type MeterReading = MeterValue;

#[derive(Clone)]
struct StripSnapshot {
    id: BusId,
    name: String,
    kind: BusKind,
    output: Option<(BusId, String)>,
    gain_db: f32,
    pan: f32,
    muted: bool,
    soloed: bool,
    audible: bool,
    solo_suppressed: bool,
    inserts: Vec<(u64, String, bool, f32)>,
    sends: Vec<(u64, String, f32, bool, SendTap)>,
    meter: Option<MeterReading>,
}

#[derive(Clone, Copy, Debug)]
enum MixerControl {
    Gain,
    Pan,
}

#[derive(Clone, Copy, Debug)]
struct MixerGesture {
    bus: BusId,
    control: MixerControl,
    base_revision: u64,
    origin_x: f32,
    origin_y: f32,
    original: f32,
    preview: f32,
}

impl MixerGesture {
    fn into_intent(self) -> Option<MixerActionIntent> {
        if (self.preview - self.original).abs() <= f32::EPSILON {
            return None;
        }
        let action = match self.control {
            MixerControl::Gain => MixerAction::SetGainDb {
                bus: self.bus,
                gain_db: self.preview,
            },
            MixerControl::Pan => MixerAction::SetPan {
                bus: self.bus,
                pan: self.preview,
            },
        };
        Some(MixerActionIntent::new(self.base_revision, action))
    }
}

pub struct MixerView {
    backend: Box<dyn MixerBackend>,
    meter_readings: BTreeMap<BusId, MeterReading>,
    meter_sequence: u64,
    controller_snapshot: Option<MixerGraph>,
    callback: Option<ControlActionCallback>,
    integration_mode: ControlIntegrationMode,
    render_status: Option<ControlRenderStatus>,
    selected_bus: Option<BusId>,
    gesture: Option<MixerGesture>,
    status: String,
    focus_handle: FocusHandle,
}

impl MixerView {
    pub fn demo(cx: &mut Context<Self>) -> Self {
        Self::with_compatibility_backend(Box::new(LocalMixerBackend::new(demo_mixer(), 128)), cx)
    }

    pub fn from_graph(graph: MixerGraph, cx: &mut Context<Self>) -> Self {
        Self::with_compatibility_backend(Box::new(LocalMixerBackend::new(graph, 128)), cx)
    }

    /// Legacy direct-mutation bridge. New aggregate hosts should use
    /// [`Self::from_controller_snapshot`].
    pub fn from_shared_graph(shared_graph: Arc<Mutex<MixerGraph>>, cx: &mut Context<Self>) -> Self {
        Self::from_shared_graph_compatibility(shared_graph, cx)
    }

    pub fn from_shared_graph_compatibility(
        shared_graph: Arc<Mutex<MixerGraph>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_compatibility_backend(Box::new(SharedMixerBackend::new(shared_graph, 128)), cx)
    }

    /// Preferred aggregate mode: render a read snapshot and emit semantic
    /// actions without mutating it. The controller publishes later snapshots
    /// through [`Self::set_controller_snapshot`].
    pub fn from_controller_snapshot(
        graph: MixerGraph,
        target_bus: Option<BusId>,
        callback: ControlActionCallback,
        cx: &mut Context<Self>,
    ) -> Self {
        let target_bus = target_bus
            .filter(|bus| graph.bus(*bus).is_some())
            .or_else(|| graph.buses().next().map(|bus| bus.id()));
        let fallback = graph.clone();
        let mut view =
            Self::with_compatibility_backend(Box::new(LocalMixerBackend::new(fallback, 0)), cx);
        view.controller_snapshot = Some(graph);
        view.callback = Some(callback);
        view.integration_mode = ControlIntegrationMode::Controller;
        view.selected_bus = target_bus;
        view.status = "Controller snapshot ready · edits emit semantic intents".into();
        view
    }

    /// Explicit compatibility construction for standalone and legacy hosts.
    pub fn with_compatibility_backend(
        backend: Box<dyn MixerBackend>,
        cx: &mut Context<Self>,
    ) -> Self {
        let selected_bus = backend.snapshot().buses().next().map(|bus| bus.id());
        Self {
            backend,
            meter_readings: BTreeMap::new(),
            meter_sequence: 0,
            controller_snapshot: None,
            callback: None,
            integration_mode: ControlIntegrationMode::Compatibility,
            render_status: None,
            selected_bus,
            gesture: None,
            status: "Compatibility mode · local graph history".into(),
            focus_handle: cx.focus_handle(),
        }
    }

    /// Compatibility alias retained for callers compiled against Cycle 2.
    pub fn with_backend(backend: Box<dyn MixerBackend>, cx: &mut Context<Self>) -> Self {
        Self::with_compatibility_backend(backend, cx)
    }

    /// Supply genuine post-DSP meter values. No synthetic activity is shown
    /// while a realtime engine has not connected a meter tap.
    pub fn set_meter_reading(&mut self, bus: BusId, reading: Option<MeterReading>) {
        if let Some(reading) =
            reading.filter(|reading| reading.peak_db.is_finite() && reading.rms_db.is_finite())
        {
            let peak_db = reading.peak_db.clamp(-120.0, 24.0);
            self.meter_readings.insert(
                bus,
                MeterReading {
                    peak_db,
                    rms_db: reading.rms_db.clamp(-120.0, peak_db),
                },
            );
        } else {
            self.meter_readings.remove(&bus);
        }
    }

    pub fn set_meter_snapshot(&mut self, snapshot: MixerMeterSnapshot, cx: &mut Context<Self>) {
        if snapshot.sequence < self.meter_sequence {
            return;
        }
        let snapshot = snapshot.sanitized();
        self.meter_sequence = snapshot.sequence;
        self.meter_readings = snapshot.buses;
        cx.notify();
    }

    pub fn set_render_status(
        &mut self,
        status: Option<ControlRenderStatus>,
        cx: &mut Context<Self>,
    ) {
        self.render_status = status;
        cx.notify();
    }

    pub fn set_controller_snapshot(&mut self, graph: MixerGraph, cx: &mut Context<Self>) {
        let revision_changed = self
            .controller_snapshot
            .as_ref()
            .is_none_or(|current| current.revision() != graph.revision());
        self.controller_snapshot = Some(graph);
        if self.selected_bus.is_none_or(|bus| {
            self.controller_snapshot
                .as_ref()
                .is_none_or(|graph| graph.bus(bus).is_none())
        }) {
            self.selected_bus = self
                .controller_snapshot
                .as_ref()
                .and_then(|graph| graph.buses().next().map(|bus| bus.id()));
        }
        if revision_changed {
            self.gesture = None;
        }
        cx.notify();
    }

    pub fn set_action_callback(&mut self, callback: Option<ControlActionCallback>) {
        self.callback = callback;
        self.integration_mode = if self.callback.is_none() && self.controller_snapshot.is_none() {
            ControlIntegrationMode::Compatibility
        } else {
            ControlIntegrationMode::Controller
        };
    }

    pub const fn integration_mode(&self) -> ControlIntegrationMode {
        self.integration_mode
    }

    pub fn set_item_target(&mut self, target: ControlItemTarget, cx: &mut Context<Self>) -> bool {
        let ControlItemTarget::Mixer { bus } = target else {
            return false;
        };
        if bus.is_some_and(|bus| self.graph_snapshot().bus(bus).is_none()) {
            return false;
        }
        self.selected_bus =
            bus.or_else(|| self.graph_snapshot().buses().next().map(|bus| bus.id()));
        cx.notify();
        true
    }

    pub fn item_state(&self) -> ControlItemState {
        ControlItemState::Mixer(MixerItemState {
            target_bus: self.selected_bus,
        })
    }

    pub fn graph_snapshot(&self) -> MixerGraph {
        self.controller_snapshot
            .clone()
            .unwrap_or_else(|| self.backend.snapshot())
    }

    fn dispatch_mixer(&mut self, intent: MixerActionIntent, cx: &mut Context<Self>) {
        let label = intent.action.label();
        if let Some(callback) = self.callback.as_ref() {
            callback(ControlAction::Mixer(intent));
            self.status = format!("{label} · sent to project controller");
        } else if self.integration_mode == ControlIntegrationMode::Compatibility {
            let graph = self.graph_snapshot();
            self.status = match intent
                .command(&graph)
                .and_then(|command| self.backend.execute(command))
            {
                Ok(()) => format!("{label} · compatibility history"),
                Err(error) => format!("Could not {label}: {error}"),
            };
        } else {
            self.status = format!("{label} not sent · no project command adapter attached");
        }
        cx.notify();
    }

    fn adjust_gain(&mut self, bus: BusId, delta: f32, cx: &mut Context<Self>) {
        let graph = self.graph_snapshot();
        let current = graph
            .bus(bus)
            .map(|bus| bus.fader().gain_db())
            .unwrap_or(0.0);
        let next = (current + delta).clamp(-72.0, 12.0);
        self.dispatch_mixer(
            MixerActionIntent::new(
                graph.revision(),
                MixerAction::SetGainDb { bus, gain_db: next },
            ),
            cx,
        );
    }

    fn adjust_pan(&mut self, bus: BusId, delta: f32, cx: &mut Context<Self>) {
        let graph = self.graph_snapshot();
        let current = graph.bus(bus).map(|bus| bus.fader().pan()).unwrap_or(0.0);
        let next = (current + delta).clamp(-1.0, 1.0);
        self.dispatch_mixer(
            MixerActionIntent::new(graph.revision(), MixerAction::SetPan { bus, pan: next }),
            cx,
        );
    }

    fn toggle_mute(&mut self, bus: BusId, cx: &mut Context<Self>) {
        let graph = self.graph_snapshot();
        let Some(current) = graph.bus(bus) else {
            return;
        };
        let next = !current.fader().muted();
        self.dispatch_mixer(
            MixerActionIntent::new(graph.revision(), MixerAction::SetMuted { bus, muted: next }),
            cx,
        );
    }

    fn toggle_solo(&mut self, bus: BusId, cx: &mut Context<Self>) {
        let graph = self.graph_snapshot();
        let Some(current) = graph.bus(bus) else {
            return;
        };
        let next = !current.fader().soloed();
        self.dispatch_mixer(
            MixerActionIntent::new(
                graph.revision(),
                MixerAction::SetSoloed { bus, soloed: next },
            ),
            cx,
        );
    }

    fn adjust_send(&mut self, send_raw: u64, delta: f32, cx: &mut Context<Self>) {
        let send_id = SendId::from_raw(send_raw);
        let graph = self.graph_snapshot();
        let current = graph
            .buses()
            .flat_map(|bus| bus.sends())
            .find(|send| send.id() == send_id)
            .map(|send| send.level_db())
            .unwrap_or(-18.0);
        self.dispatch_mixer(
            MixerActionIntent::new(
                graph.revision(),
                MixerAction::SetSendLevel {
                    send: send_id,
                    level_db: (current + delta).clamp(-72.0, 12.0),
                },
            ),
            cx,
        );
    }

    fn toggle_send(&mut self, send_raw: u64, cx: &mut Context<Self>) {
        let send_id = SendId::from_raw(send_raw);
        let graph = self.graph_snapshot();
        let next = !graph
            .buses()
            .flat_map(|bus| bus.sends())
            .find(|send| send.id() == send_id)
            .map(|send| send.muted())
            .unwrap_or(false);
        self.dispatch_mixer(
            MixerActionIntent::new(
                graph.revision(),
                MixerAction::SetSendMuted {
                    send: send_id,
                    muted: next,
                },
            ),
            cx,
        );
    }

    fn toggle_send_tap(&mut self, send_raw: u64, cx: &mut Context<Self>) {
        let send_id = SendId::from_raw(send_raw);
        let graph = self.graph_snapshot();
        let Some(send) = graph
            .buses()
            .flat_map(|bus| bus.sends())
            .find(|send| send.id() == send_id)
        else {
            self.status = "Send no longer exists".into();
            cx.notify();
            return;
        };
        let next = match send.tap() {
            SendTap::PreFader => SendTap::PostFader,
            SendTap::PostFader => SendTap::PreFader,
        };
        self.dispatch_mixer(
            MixerActionIntent::new(
                graph.revision(),
                MixerAction::SetSendTap {
                    send: send_id,
                    tap: next,
                },
            ),
            cx,
        );
    }

    fn remove_send(&mut self, send_raw: u64, cx: &mut Context<Self>) {
        let send_id = SendId::from_raw(send_raw);
        let revision = self.graph_snapshot().revision();
        self.dispatch_mixer(
            MixerActionIntent::new(revision, MixerAction::RemoveSend { send: send_id }),
            cx,
        );
    }

    fn add_send(&mut self, bus: BusId, cx: &mut Context<Self>) {
        let graph = self.graph_snapshot();
        if bus == graph.master() {
            self.status = "Master is the terminal output and cannot create sends".into();
            cx.notify();
            return;
        }
        // Prefer a group destination, then master. Build each possibility on a
        // clone so graph cycle rules remain the source of truth.
        let mut candidates: Vec<_> = graph
            .buses()
            .filter(|candidate| candidate.id() != bus)
            .map(|candidate| {
                (
                    candidate.kind(),
                    candidate.id(),
                    candidate.name().to_owned(),
                )
            })
            .collect();
        candidates.sort_by_key(|(kind, id, _)| {
            let rank = match kind {
                BusKind::Group => 0,
                BusKind::Master => 1,
                BusKind::Component => 2,
                BusKind::Source => 3,
            };
            (rank, *id)
        });
        for (_, target, target_name) in candidates {
            let intent = MixerActionIntent::new(
                graph.revision(),
                MixerAction::AddSend {
                    bus,
                    target,
                    tap: SendTap::PostFader,
                    level_db: -18.0,
                },
            );
            if intent.command(&graph).is_err() {
                continue;
            }
            self.dispatch_mixer(intent, cx);
            self.status = format!("Post-fader send → {target_name} at −18 dB · intent sent");
            return;
        }
        self.status = "No cycle-safe send destination is available".into();
        cx.notify();
    }

    fn begin_mixer_gesture(
        &mut self,
        bus: BusId,
        control: MixerControl,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        let graph = self.graph_snapshot();
        let Some(strip) = graph.bus(bus) else {
            return;
        };
        let original = match control {
            MixerControl::Gain => strip.fader().gain_db(),
            MixerControl::Pan => strip.fader().pan(),
        };
        self.selected_bus = Some(bus);
        self.gesture = Some(MixerGesture {
            bus,
            control,
            base_revision: graph.revision(),
            origin_x: f32::from(event.position.x),
            origin_y: f32::from(event.position.y),
            original,
            preview: original,
        });
        self.status = match control {
            MixerControl::Gain => "Dragging fader · release to commit one undo step".into(),
            MixerControl::Pan => "Dragging pan · release to commit one undo step".into(),
        };
        cx.notify();
    }

    fn drag_mixer_control(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(mut gesture) = self.gesture else {
            return;
        };
        gesture.preview = match gesture.control {
            MixerControl::Gain => (gesture.original
                + (gesture.origin_y - f32::from(event.position.y)) * 0.25)
                .clamp(-72.0, 12.0),
            MixerControl::Pan => (gesture.original
                + (f32::from(event.position.x) - gesture.origin_x) * 0.01)
                .clamp(-1.0, 1.0),
        };
        self.gesture = Some(gesture);
        cx.notify();
    }

    fn end_mixer_gesture(&mut self, _event: &MouseUpEvent, cx: &mut Context<Self>) {
        let Some(gesture) = self.gesture.take() else {
            return;
        };
        let Some(intent) = gesture.into_intent() else {
            self.status = "Control unchanged".into();
            cx.notify();
            return;
        };
        self.dispatch_mixer(intent, cx);
    }

    fn toggle_insert(&mut self, processor_raw: u64, cx: &mut Context<Self>) {
        let id = crate::mixer::ProcessorId::from_raw(processor_raw);
        let graph = self.graph_snapshot();
        let next = graph
            .buses()
            .flat_map(|bus| bus.inserts())
            .find(|slot| slot.processor_id() == id)
            .map(|slot| !slot.bypassed())
            .unwrap_or(true);
        self.dispatch_mixer(
            MixerActionIntent::new(
                graph.revision(),
                MixerAction::SetInsertBypassed {
                    processor: id,
                    bypassed: next,
                },
            ),
            cx,
        );
    }

    fn adjust_insert_wet(&mut self, processor_raw: u64, delta: f32, cx: &mut Context<Self>) {
        let id = crate::mixer::ProcessorId::from_raw(processor_raw);
        let graph = self.graph_snapshot();
        let current = graph
            .buses()
            .flat_map(|bus| bus.inserts())
            .find(|slot| slot.processor_id() == id)
            .map(|slot| slot.wet())
            .unwrap_or(1.0);
        self.dispatch_mixer(
            MixerActionIntent::new(
                graph.revision(),
                MixerAction::SetInsertWet {
                    processor: id,
                    wet: (current + delta).clamp(0.0, 1.0),
                },
            ),
            cx,
        );
    }

    fn cycle_output(&mut self, bus: BusId, cx: &mut Context<Self>) {
        let graph = self.graph_snapshot();
        if bus == graph.master() {
            self.status = "Master is the terminal output".into();
            cx.notify();
            return;
        }
        let candidates: Vec<_> = graph.buses().map(|bus| bus.id()).collect();
        let current = graph.bus(bus).and_then(|bus| bus.output());
        let start = current
            .and_then(|id| candidates.iter().position(|candidate| *candidate == id))
            .unwrap_or(0);
        for offset in 1..=candidates.len() {
            let target = candidates[(start + offset) % candidates.len()];
            if target == bus {
                continue;
            }
            let intent =
                MixerActionIntent::new(graph.revision(), MixerAction::SetOutput { bus, target });
            if intent.command(&graph).is_ok() {
                let name = graph.bus(target).unwrap().name().to_owned();
                self.dispatch_mixer(intent, cx);
                self.status = format!("Output → {name} · intent sent");
                return;
            }
        }
        self.status = "No cycle-safe output target is available".into();
        cx.notify();
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        if let Some(callback) = self.callback.as_ref() {
            callback(ControlAction::History(ControlHistoryIntent {
                surface: ControlSurface::Mixer,
                expected_revision: self.graph_snapshot().revision(),
                direction: HistoryDirection::Undo,
            }));
            self.status = "Mixer undo sent to project controller".into();
        } else if self.integration_mode == ControlIntegrationMode::Compatibility {
            self.status = match self.backend.undo() {
                Ok(true) => "Undid mixer edit · compatibility history".into(),
                Ok(false) => "Mixer history is already at its beginning".into(),
                Err(error) => format!("Undo failed: {error}"),
            };
        } else {
            self.status = "Mixer undo not sent · no project command adapter attached".into();
        }
        cx.notify();
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        if let Some(callback) = self.callback.as_ref() {
            callback(ControlAction::History(ControlHistoryIntent {
                surface: ControlSurface::Mixer,
                expected_revision: self.graph_snapshot().revision(),
                direction: HistoryDirection::Redo,
            }));
            self.status = "Mixer redo sent to project controller".into();
        } else if self.integration_mode == ControlIntegrationMode::Compatibility {
            self.status = match self.backend.redo() {
                Ok(true) => "Redid mixer edit · compatibility history".into(),
                Ok(false) => "Nothing to redo".into(),
                Err(error) => format!("Redo failed: {error}"),
            };
        } else {
            self.status = "Mixer redo not sent · no project command adapter attached".into();
        }
        cx.notify();
    }

    fn snapshots(&self) -> Vec<StripSnapshot> {
        let graph = self.graph_snapshot();
        let effective = graph.effective_states();
        let mut snapshots: Vec<_> = graph
            .buses()
            .map(|bus| {
                let effective = effective
                    .get(&bus.id())
                    .copied()
                    .expect("effective state exists for every mixer bus");
                let gesture = self.gesture.filter(|gesture| gesture.bus == bus.id());
                StripSnapshot {
                    id: bus.id(),
                    name: bus.name().to_owned(),
                    kind: bus.kind(),
                    output: bus
                        .output()
                        .and_then(|id| graph.bus(id).map(|target| (id, target.name().to_owned()))),
                    gain_db: gesture
                        .filter(|gesture| matches!(gesture.control, MixerControl::Gain))
                        .map(|gesture| gesture.preview)
                        .unwrap_or_else(|| bus.fader().gain_db()),
                    pan: gesture
                        .filter(|gesture| matches!(gesture.control, MixerControl::Pan))
                        .map(|gesture| gesture.preview)
                        .unwrap_or_else(|| bus.fader().pan()),
                    muted: bus.fader().muted(),
                    soloed: bus.fader().soloed(),
                    audible: effective.audible,
                    solo_suppressed: effective.solo_suppressed,
                    inserts: bus
                        .inserts()
                        .iter()
                        .filter_map(|slot| {
                            graph.processor(slot.processor_id()).map(|processor| {
                                (
                                    slot.processor_id().get(),
                                    processor.descriptor().display_name.clone(),
                                    slot.bypassed(),
                                    slot.wet(),
                                )
                            })
                        })
                        .collect(),
                    sends: bus
                        .sends()
                        .iter()
                        .map(|send| {
                            (
                                send.id().get(),
                                graph
                                    .bus(send.target())
                                    .map(|bus| bus.name().to_owned())
                                    .unwrap_or_else(|| "missing".into()),
                                send.level_db(),
                                send.muted(),
                                send.tap(),
                            )
                        })
                        .collect(),
                    meter: self.meter_readings.get(&bus.id()).copied(),
                }
            })
            .collect();
        snapshots.sort_by_key(|strip| {
            let order = match strip.kind {
                BusKind::Source => 0,
                BusKind::Component => 1,
                BusKind::Group => 2,
                BusKind::Master => 3,
            };
            (order, strip.id)
        });
        snapshots
    }

    fn render_strip(&self, strip: StripSnapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let bus = strip.id;
        let selected = self.selected_bus == Some(bus);
        let gain_fraction = gain_to_fader_fraction(strip.gain_db);
        let meter = strip.meter;
        let meter_fraction = meter
            .map(|value| db_to_meter_fraction(value.peak_db))
            .unwrap_or(0.0);
        let mut inserts = div().flex().flex_col().gap_1();
        if strip.inserts.is_empty() {
            inserts = inserts.child(empty_slot("+ insert", "Plugin host not connected"));
        } else {
            for (id, name, bypassed, wet) in strip.inserts.clone() {
                inserts = inserts.child(
                    div()
                        .id(SharedString::from(format!("insert-{id}")))
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(rgb(BORDER))
                        .bg(rgb(if bypassed { PANEL_ALT } else { 0x18212c }))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_insert(id, cx)))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(if bypassed { DIM } else { TEXT }))
                                .child(name),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_between()
                                .text_xs()
                                .text_color(rgb(DIM))
                                .child(if bypassed { "bypassed" } else { "active" })
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .child(
                                            div()
                                                .id(SharedString::from(format!(
                                                    "insert-wet-down-{id}"
                                                )))
                                                .px_1()
                                                .cursor_pointer()
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.adjust_insert_wet(id, -0.1, cx);
                                                    cx.stop_propagation();
                                                }))
                                                .child("−"),
                                        )
                                        .child(format!("{:>3.0}%", wet * 100.0))
                                        .child(
                                            div()
                                                .id(SharedString::from(format!(
                                                    "insert-wet-up-{id}"
                                                )))
                                                .px_1()
                                                .cursor_pointer()
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.adjust_insert_wet(id, 0.1, cx);
                                                    cx.stop_propagation();
                                                }))
                                                .child("+"),
                                        ),
                                ),
                        ),
                );
            }
        }

        let mut sends = div().flex().flex_col().gap_1();
        if strip.sends.is_empty() {
            sends = sends.child(empty_slot("no sends", "Parallel routing available"));
        } else {
            for (id, target, level, muted, tap) in strip.sends.clone() {
                let tap_label = match tap {
                    SendTap::PreFader => "PRE",
                    SendTap::PostFader => "POST",
                };
                sends = sends.child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(rgb(BORDER))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(if muted { DIM } else { CYAN }))
                                        .child(format!("→ {target}")),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .id(SharedString::from(format!("send-mute-{id}")))
                                                .cursor_pointer()
                                                .text_xs()
                                                .text_color(rgb(if muted {
                                                    MAGENTA
                                                } else {
                                                    MUTED
                                                }))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.toggle_send(id, cx);
                                                    cx.stop_propagation();
                                                }))
                                                .child(if muted { "OFF" } else { "ON" }),
                                        )
                                        .child(
                                            div()
                                                .id(SharedString::from(format!("send-remove-{id}")))
                                                .cursor_pointer()
                                                .text_xs()
                                                .text_color(rgb(DIM))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.remove_send(id, cx);
                                                    cx.stop_propagation();
                                                }))
                                                .child("×"),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .id(SharedString::from(format!("send-down-{id}")))
                                        .cursor_pointer()
                                        .text_xs()
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.adjust_send(id, -1.0, cx)
                                        }))
                                        .child("−"),
                                )
                                .child(
                                    div()
                                        .id(SharedString::from(format!("send-tap-{id}")))
                                        .px_1()
                                        .rounded_sm()
                                        .border_1()
                                        .border_color(rgb(BORDER))
                                        .cursor_pointer()
                                        .text_xs()
                                        .text_color(rgb(CYAN))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.toggle_send_tap(id, cx);
                                            cx.stop_propagation();
                                        }))
                                        .child(format!("{level:.1} dB · {tap_label}")),
                                )
                                .child(
                                    div()
                                        .id(SharedString::from(format!("send-up-{id}")))
                                        .cursor_pointer()
                                        .text_xs()
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.adjust_send(id, 1.0, cx)
                                        }))
                                        .child("+"),
                                ),
                        ),
                );
            }
        }
        if strip.kind != BusKind::Master {
            sends = sends.child(
                div()
                    .id(SharedString::from(format!("send-add-{}", bus.get())))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .cursor_pointer()
                    .text_xs()
                    .text_color(rgb(CYAN))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.add_send(bus, cx);
                        cx.stop_propagation();
                    }))
                    .child("+ send"),
            );
        }

        div()
            .id(SharedString::from(format!("mixer-strip-{}", bus.get())))
            .w(px(190.0))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_r_1()
            .border_color(rgb(if selected { CYAN } else { BORDER }))
            .bg(rgb(if selected {
                0x111b24
            } else if strip.audible {
                PANEL
            } else {
                0x0b0e14
            }))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected_bus = Some(bus);
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_sm().text_color(rgb(TEXT)).child(strip.name))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_end()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(DIM))
                                    .child(format!("{:?}", strip.kind).to_uppercase()),
                            )
                            .when(strip.solo_suppressed, |view| {
                                view.child(
                                    div().text_xs().text_color(rgb(AMBER)).child("SOLO MUTED"),
                                )
                            }),
                    ),
            )
            .child(div().h(px(1.0)).bg(rgb(BORDER)))
            .child(section_label("INSERTS"))
            .child(inserts)
            .child(section_label("SENDS"))
            .child(sends)
            .child(
                div()
                    .mt_2()
                    .flex_1()
                    .min_h(px(130.0))
                    .flex()
                    .justify_center()
                    .gap_3()
                    .child(vertical_meter(meter_fraction, meter))
                    .child(
                        div()
                            .id(SharedString::from(format!("fader-drag-{}", bus.get())))
                            .cursor_ns_resize()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                    this.begin_mixer_gesture(bus, MixerControl::Gain, event, cx);
                                    cx.stop_propagation();
                                }),
                            )
                            .child(vertical_fader(gain_fraction, strip.gain_db)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(step_button(
                        "gain-minus",
                        bus,
                        "−",
                        move |this, cx| this.adjust_gain(bus, -1.0, cx),
                        cx,
                    ))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT))
                            .child(format!("{:+.1} dB", strip.gain_db)),
                    )
                    .child(step_button(
                        "gain-plus",
                        bus,
                        "+",
                        move |this, cx| this.adjust_gain(bus, 1.0, cx),
                        cx,
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(step_button(
                        "pan-left",
                        bus,
                        "L",
                        move |this, cx| this.adjust_pan(bus, -0.1, cx),
                        cx,
                    ))
                    .child(
                        div()
                            .id(SharedString::from(format!("pan-drag-{}", bus.get())))
                            .px_2()
                            .cursor_ew_resize()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                    this.begin_mixer_gesture(bus, MixerControl::Pan, event, cx);
                                    cx.stop_propagation();
                                }),
                            )
                            .child(format_pan(strip.pan)),
                    )
                    .child(step_button(
                        "pan-right",
                        bus,
                        "R",
                        move |this, cx| this.adjust_pan(bus, 0.1, cx),
                        cx,
                    )),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(toggle_button(
                        "mute",
                        bus,
                        "M",
                        strip.muted,
                        MAGENTA,
                        move |this, cx| this.toggle_mute(bus, cx),
                        cx,
                    ))
                    .child(toggle_button(
                        "solo",
                        bus,
                        "S",
                        strip.soloed,
                        AMBER,
                        move |this, cx| this.toggle_solo(bus, cx),
                        cx,
                    )),
            )
            .child(
                div()
                    .id(SharedString::from(format!("route-{}", bus.get())))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL_ALT))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.cycle_output(bus, cx)))
                    .child(div().text_xs().text_color(rgb(DIM)).child("OUTPUT"))
                    .child(
                        div().text_xs().text_color(rgb(CYAN)).child(
                            strip
                                .output
                                .map(|(_, name)| name)
                                .unwrap_or_else(|| "Hardware out".into()),
                        ),
                    ),
            )
    }
}

impl Focusable for MixerView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for MixerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strips = self.snapshots();
        let render_label = self
            .render_status
            .as_ref()
            .map(ControlRenderStatus::label)
            .unwrap_or_else(|| "RENDER STATUS UNAVAILABLE".into());
        let mut bank =
            div()
                .id("mixer-strip-bank")
                .h_full()
                .flex()
                .overflow_x_scroll()
                .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                    this.drag_mixer_control(event, cx)
                }))
                .capture_any_mouse_up(cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    this.end_mixer_gesture(event, cx)
                }));
        for strip in strips {
            bank = bank.child(self.render_strip(strip, cx));
        }
        div()
            .key_context("AudecMixer")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &ControlUndo, _, cx| this.undo(cx)))
            .on_action(cx.listener(|this, _: &ControlRedo, _, cx| this.redo(cx)))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .child(
                div()
                    .h(px(52.0))
                    .flex_none()
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL_ALT))
                    .child(
                        div()
                            .child(div().text_sm().child("MIXER / ROUTING"))
                            .child(div().text_xs().text_color(rgb(DIM)).child(render_label)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                header_button("mixer-undo", "Undo ⌘Z")
                                    .on_click(cx.listener(|this, _, _, cx| this.undo(cx))),
                            )
                            .child(
                                header_button("mixer-redo", "Redo ⇧⌘Z")
                                    .on_click(cx.listener(|this, _, _, cx| this.redo(cx))),
                            ),
                    ),
            )
            .child(div().flex_1().min_h_0().child(bank))
            .child(
                div()
                    .h(px(28.0))
                    .flex_none()
                    .px_4()
                    .flex()
                    .items_center()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL_ALT))
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(self.status.clone()),
            )
    }
}

/// Pluggable ownership seam for automation data and its command history.
pub trait AutomationBackend: 'static {
    fn snapshot(&self) -> AutomationGraph;
    fn execute(&mut self, intent: AutomationIntent) -> Result<(), String>;
    fn undo(&mut self) -> Result<bool, String>;
    fn redo(&mut self) -> Result<bool, String>;
}

pub struct LocalAutomationBackend {
    graph: AutomationGraph,
    undo: VecDeque<AutomationIntent>,
    redo: Vec<AutomationIntent>,
    limit: usize,
}

impl LocalAutomationBackend {
    pub fn new(graph: AutomationGraph, history_limit: usize) -> Self {
        Self {
            graph,
            undo: VecDeque::new(),
            redo: Vec::new(),
            limit: history_limit,
        }
    }
}

impl AutomationBackend for LocalAutomationBackend {
    fn snapshot(&self) -> AutomationGraph {
        self.graph.clone()
    }

    fn execute(&mut self, intent: AutomationIntent) -> Result<(), String> {
        let inverse = self
            .graph
            .apply_intent(&intent)
            .map_err(|error| error.to_string())?;
        self.redo.clear();
        if self.limit > 0 {
            self.undo
                .push_back(AutomationIntent::new(self.graph.revision(), inverse));
            while self.undo.len() > self.limit {
                self.undo.pop_front();
            }
        }
        Ok(())
    }

    fn undo(&mut self) -> Result<bool, String> {
        let Some(intent) = self.undo.pop_back() else {
            return Ok(false);
        };
        match self.graph.apply_intent(&intent) {
            Ok(redo) => {
                self.redo
                    .push(AutomationIntent::new(self.graph.revision(), redo));
                Ok(true)
            }
            Err(error) => {
                self.undo.push_back(intent);
                Err(error.to_string())
            }
        }
    }

    fn redo(&mut self) -> Result<bool, String> {
        let Some(intent) = self.redo.pop() else {
            return Ok(false);
        };
        match self.graph.apply_intent(&intent) {
            Ok(undo) => {
                self.undo
                    .push_back(AutomationIntent::new(self.graph.revision(), undo));
                Ok(true)
            }
            Err(error) => {
                self.redo.push(intent);
                Err(error.to_string())
            }
        }
    }
}

/// Automation history over controller-owned truth, with no local graph mirror.
pub struct SharedAutomationBackend {
    shared_graph: Arc<Mutex<AutomationGraph>>,
    undo: VecDeque<AutomationIntent>,
    redo: Vec<AutomationIntent>,
    limit: usize,
}

impl SharedAutomationBackend {
    pub fn new(shared_graph: Arc<Mutex<AutomationGraph>>, history_limit: usize) -> Self {
        Self {
            shared_graph,
            undo: VecDeque::new(),
            redo: Vec::new(),
            limit: history_limit,
        }
    }
}

impl AutomationBackend for SharedAutomationBackend {
    fn snapshot(&self) -> AutomationGraph {
        self.shared_graph
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn execute(&mut self, intent: AutomationIntent) -> Result<(), String> {
        let mut shared_graph = self
            .shared_graph
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let inverse = shared_graph
            .apply_intent(&intent)
            .map_err(|error| error.to_string())?;
        self.redo.clear();
        if self.limit > 0 {
            self.undo
                .push_back(AutomationIntent::new(shared_graph.revision(), inverse));
            while self.undo.len() > self.limit {
                self.undo.pop_front();
            }
        }
        Ok(())
    }

    fn undo(&mut self) -> Result<bool, String> {
        let mut shared_graph = self
            .shared_graph
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(intent) = self.undo.pop_back() else {
            return Ok(false);
        };
        match shared_graph.apply_intent(&intent) {
            Ok(redo) => {
                self.redo
                    .push(AutomationIntent::new(shared_graph.revision(), redo));
                Ok(true)
            }
            Err(error) => {
                self.undo.push_back(intent);
                Err(error.to_string())
            }
        }
    }

    fn redo(&mut self) -> Result<bool, String> {
        let mut shared_graph = self
            .shared_graph
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(intent) = self.redo.pop() else {
            return Ok(false);
        };
        match shared_graph.apply_intent(&intent) {
            Ok(undo) => {
                self.undo
                    .push_back(AutomationIntent::new(shared_graph.revision(), undo));
                Ok(true)
            }
            Err(error) => {
                self.redo.push(intent);
                Err(error.to_string())
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CurveViewport {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
    pub start: i64,
    pub end: i64,
}

impl CurveViewport {
    pub fn position_to_x(self, position: i64) -> f32 {
        let span = (self.end - self.start).max(1) as f64;
        self.left + self.width * ((position - self.start) as f64 / span).clamp(0.0, 1.0) as f32
    }

    pub fn normalized_to_y(self, normalized: f64) -> f32 {
        self.top + self.height * (1.0 - normalized.clamp(0.0, 1.0) as f32)
    }

    pub fn x_to_position(self, x: f32) -> i64 {
        let t = ((x - self.left) / self.width.max(1.0)).clamp(0.0, 1.0) as f64;
        self.start + ((self.end - self.start) as f64 * t).round() as i64
    }

    pub fn y_to_normalized(self, y: f32) -> f64 {
        (1.0 - ((y - self.top) / self.height.max(1.0)).clamp(0.0, 1.0)) as f64
    }
}

#[derive(Clone)]
struct LaneSnapshot {
    id: AutomationLaneId,
    name: String,
    target: String,
    enabled: bool,
    binding: BindingMode,
    time_domain: TimeDomain,
    points: Vec<AutomationPoint>,
    descriptor: ParameterDescriptor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutomationSnap {
    Off,
    Fine,
    Beat,
    Bar,
}

impl AutomationSnap {
    fn next(self) -> Self {
        match self {
            Self::Off => Self::Fine,
            Self::Fine => Self::Beat,
            Self::Beat => Self::Bar,
            Self::Bar => Self::Off,
        }
    }

    fn interval(self, domain: TimeDomain) -> Option<i64> {
        match (self, domain) {
            (Self::Off, _) => None,
            (Self::Fine, TimeDomain::Beats) => Some(PPQ / 4),
            (Self::Beat, TimeDomain::Beats) => Some(PPQ),
            (Self::Bar, TimeDomain::Beats) => Some(PPQ * 4),
            (Self::Fine, TimeDomain::Frames) => Some(64),
            (Self::Beat, TimeDomain::Frames) => Some(256),
            (Self::Bar, TimeDomain::Frames) => Some(1_024),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Off => "SNAP OFF",
            Self::Fine => "SNAP FINE",
            Self::Beat => "SNAP BEAT",
            Self::Bar => "SNAP BAR",
        }
    }
}

#[derive(Clone, Debug)]
struct AutomationGesture {
    base_revision: u64,
    lane: AutomationLaneId,
    point: AutomationPoint,
    is_new: bool,
}

impl AutomationGesture {
    fn into_intent(self) -> AutomationActionIntent {
        let action = if self.is_new {
            AutomationAction::InsertPoint {
                lane: self.lane,
                position: self.point.position,
                value: self.point.value,
                outgoing: self.point.outgoing,
            }
        } else {
            AutomationAction::MovePoint {
                lane: self.lane,
                point: self.point,
            }
        };
        AutomationActionIntent::new(self.base_revision, action)
    }
}

pub struct AutomationView {
    backend: Box<dyn AutomationBackend>,
    controller_snapshot: Option<AutomationGraph>,
    callback: Option<ControlActionCallback>,
    integration_mode: ControlIntegrationMode,
    render_status: Option<ControlRenderStatus>,
    selected_lane: Option<AutomationLaneId>,
    selected_point: Option<AutomationPointId>,
    curve_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    gesture: Option<AutomationGesture>,
    cursor_coordinate: i64,
    view_start: i64,
    view_end: i64,
    write_mode: WriteMode,
    snap: AutomationSnap,
    status: String,
    focus_handle: FocusHandle,
}

impl AutomationView {
    pub fn demo(cx: &mut Context<Self>) -> Self {
        Self::with_compatibility_backend(
            Box::new(LocalAutomationBackend::new(demo_automation(), 256)),
            cx,
        )
    }

    pub fn from_graph(graph: AutomationGraph, cx: &mut Context<Self>) -> Self {
        Self::with_compatibility_backend(Box::new(LocalAutomationBackend::new(graph, 256)), cx)
    }

    /// Legacy direct-mutation bridge. New aggregate hosts should use
    /// [`Self::from_controller_snapshot`].
    pub fn from_shared_graph(
        shared_graph: Arc<Mutex<AutomationGraph>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::from_shared_graph_compatibility(shared_graph, cx)
    }

    pub fn from_shared_graph_compatibility(
        shared_graph: Arc<Mutex<AutomationGraph>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_compatibility_backend(
            Box::new(SharedAutomationBackend::new(shared_graph, 256)),
            cx,
        )
    }

    pub fn from_controller_snapshot(
        graph: AutomationGraph,
        target_lane: AutomationLaneId,
        callback: ControlActionCallback,
        cx: &mut Context<Self>,
    ) -> Self {
        let target_lane = graph
            .lane(target_lane)
            .map(|lane| lane.id)
            .or_else(|| graph.lanes().next().map(|lane| lane.id));
        let fallback = graph.clone();
        let mut view = Self::with_compatibility_backend(
            Box::new(LocalAutomationBackend::new(fallback, 0)),
            cx,
        );
        view.controller_snapshot = Some(graph);
        view.callback = Some(callback);
        view.integration_mode = ControlIntegrationMode::Controller;
        view.selected_lane = target_lane;
        view.status = "Controller snapshot ready · edits emit semantic intents".into();
        view
    }

    pub fn with_compatibility_backend(
        backend: Box<dyn AutomationBackend>,
        cx: &mut Context<Self>,
    ) -> Self {
        let graph = backend.snapshot();
        let selected_lane = graph.lanes().next().map(|lane| lane.id);
        Self {
            backend,
            controller_snapshot: None,
            callback: None,
            integration_mode: ControlIntegrationMode::Compatibility,
            render_status: None,
            selected_lane,
            selected_point: None,
            curve_bounds: Arc::new(Mutex::new(None)),
            gesture: None,
            cursor_coordinate: 4 * PPQ,
            view_start: 0,
            view_end: 16 * PPQ,
            write_mode: WriteMode::Read,
            snap: AutomationSnap::Fine,
            status: "READ · compatibility graph history".into(),
            focus_handle: cx.focus_handle(),
        }
    }

    /// Compatibility alias retained for callers compiled against Cycle 2.
    pub fn with_backend(backend: Box<dyn AutomationBackend>, cx: &mut Context<Self>) -> Self {
        Self::with_compatibility_backend(backend, cx)
    }

    pub fn set_controller_snapshot(&mut self, graph: AutomationGraph, cx: &mut Context<Self>) {
        let revision_changed = self
            .controller_snapshot
            .as_ref()
            .is_none_or(|current| current.revision() != graph.revision());
        self.controller_snapshot = Some(graph);
        if self.selected_lane.is_none_or(|lane| {
            self.controller_snapshot
                .as_ref()
                .is_none_or(|graph| graph.lane(lane).is_none())
        }) {
            self.selected_lane = self
                .controller_snapshot
                .as_ref()
                .and_then(|graph| graph.lanes().next().map(|lane| lane.id));
            self.selected_point = None;
        }
        if revision_changed {
            self.gesture = None;
        }
        cx.notify();
    }

    pub fn set_action_callback(&mut self, callback: Option<ControlActionCallback>) {
        self.callback = callback;
        self.integration_mode = if self.callback.is_none() && self.controller_snapshot.is_none() {
            ControlIntegrationMode::Compatibility
        } else {
            ControlIntegrationMode::Controller
        };
    }

    pub const fn integration_mode(&self) -> ControlIntegrationMode {
        self.integration_mode
    }

    pub fn set_render_status(
        &mut self,
        status: Option<ControlRenderStatus>,
        cx: &mut Context<Self>,
    ) {
        self.render_status = status;
        cx.notify();
    }

    pub fn set_item_target(&mut self, target: ControlItemTarget, cx: &mut Context<Self>) -> bool {
        let ControlItemTarget::Automation { lane } = target else {
            return false;
        };
        if self.graph_snapshot().lane(lane).is_none() {
            return false;
        }
        self.selected_lane = Some(lane);
        self.selected_point = None;
        self.gesture = None;
        cx.notify();
        true
    }

    pub fn item_state(&self) -> Option<ControlItemState> {
        Some(ControlItemState::Automation(AutomationItemState {
            target_lane: self.selected_lane?,
            selected_point: self.selected_point,
            cursor_coordinate: self.cursor_coordinate,
            view_start: self.view_start,
            view_end: self.view_end,
        }))
    }

    pub fn graph_snapshot(&self) -> AutomationGraph {
        self.controller_snapshot
            .clone()
            .unwrap_or_else(|| self.backend.snapshot())
    }

    fn lane_snapshot(&self, id: AutomationLaneId) -> Option<LaneSnapshot> {
        let graph = self.graph_snapshot();
        let lane = graph.lane(id)?;
        let descriptor = graph
            .descriptors()
            .find(|descriptor| descriptor.address == lane.target)?
            .clone();
        let mut points = lane.points().to_vec();
        if let Some(gesture) = self.gesture.as_ref().filter(|gesture| gesture.lane == id) {
            if gesture.is_new {
                points.push(gesture.point.clone());
            } else if let Some(point) = points.iter_mut().find(|point| point.id == gesture.point.id)
            {
                *point = gesture.point.clone();
            }
            points.sort_by_key(|point| position_coordinate(point.position));
        }
        Some(LaneSnapshot {
            id,
            name: lane.name.clone(),
            target: describe_target(&lane.target),
            enabled: lane.enabled,
            binding: lane.binding,
            time_domain: lane.time_domain,
            points,
            descriptor,
        })
    }

    fn dispatch_automation(&mut self, intent: AutomationActionIntent, cx: &mut Context<Self>) {
        let label = intent.action.label();
        if let Some(callback) = self.callback.as_ref() {
            callback(ControlAction::Automation(intent));
            self.status = format!("{label} · sent to project controller");
        } else if self.integration_mode == ControlIntegrationMode::Compatibility {
            let graph = self.graph_snapshot();
            self.status = match intent
                .legacy_intent(&graph)
                .map_err(|error| error.to_string())
                .and_then(|legacy| self.backend.execute(legacy))
            {
                Ok(()) => format!("{label} · compatibility history"),
                Err(error) => format!("Could not {label}: {error}"),
            };
        } else {
            self.status = format!("{label} not sent · no project command adapter attached");
        }
        cx.notify();
    }

    fn select_lane(&mut self, lane: AutomationLaneId, cx: &mut Context<Self>) {
        self.selected_lane = Some(lane);
        self.selected_point = None;
        self.status = "Lane selected · click curve to add, drag points to move".into();
        cx.notify();
    }

    fn toggle_lane(&mut self, lane: AutomationLaneId, cx: &mut Context<Self>) {
        let graph = self.graph_snapshot();
        let Some(enabled) = graph.lane(lane).map(|lane| !lane.enabled) else {
            return;
        };
        self.dispatch_automation(
            AutomationActionIntent::new(
                graph.revision(),
                AutomationAction::SetLaneEnabled { lane, enabled },
            ),
            cx,
        );
    }

    fn cycle_binding(&mut self, cx: &mut Context<Self>) {
        let Some(lane) = self.selected_lane else {
            return;
        };
        let graph = self.graph_snapshot();
        let Some(current) = graph.lane(lane) else {
            return;
        };
        let binding = match current.binding {
            BindingMode::Replace => BindingMode::Add,
            BindingMode::Add => BindingMode::Multiply,
            BindingMode::Multiply => BindingMode::Replace,
        };
        self.dispatch_automation(
            AutomationActionIntent::new(
                graph.revision(),
                AutomationAction::SetLaneBinding { lane, binding },
            ),
            cx,
        );
    }

    fn cycle_write_mode(&mut self, cx: &mut Context<Self>) {
        self.write_mode = match self.write_mode {
            WriteMode::Read => WriteMode::Touch,
            WriteMode::Touch => WriteMode::Latch,
            WriteMode::Latch => WriteMode::Write,
            WriteMode::Write => WriteMode::Read,
        };
        self.status = format!(
            "{:?} mode · transport writer hookup pending",
            self.write_mode
        )
        .to_uppercase();
        cx.notify();
    }

    fn cycle_snap(&mut self, cx: &mut Context<Self>) {
        self.snap = self.snap.next();
        self.status = format!("{} · applies to point creation and drag", self.snap.label());
        cx.notify();
    }

    fn set_selected_shape(&mut self, shape: SegmentShape, cx: &mut Context<Self>) {
        let (Some(lane_id), Some(point_id)) = (self.selected_lane, self.selected_point) else {
            self.status = "Select a point before changing segment type".into();
            cx.notify();
            return;
        };
        let revision = self.graph_snapshot().revision();
        self.dispatch_automation(
            AutomationActionIntent::new(
                revision,
                AutomationAction::SetPointShape {
                    lane: lane_id,
                    point: point_id,
                    shape,
                },
            ),
            cx,
        );
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        let (Some(lane_id), Some(point_id)) = (self.selected_lane, self.selected_point) else {
            self.status = "No automation point selected".into();
            cx.notify();
            return;
        };
        let revision = self.graph_snapshot().revision();
        self.dispatch_automation(
            AutomationActionIntent::new(
                revision,
                AutomationAction::DeletePoint {
                    lane: lane_id,
                    point: point_id,
                },
            ),
            cx,
        );
        self.selected_point = None;
    }

    fn viewport(&self) -> Option<CurveViewport> {
        let bounds = *self.curve_bounds.lock().unwrap();
        bounds.map(|bounds| CurveViewport {
            left: f32::from(bounds.origin.x),
            top: f32::from(bounds.origin.y),
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
            start: self.view_start,
            end: self.view_end,
        })
    }

    fn point_at(&self, x: f32, y: f32, snapshot: &LaneSnapshot) -> Option<AutomationPointId> {
        let viewport = self.viewport()?;
        snapshot
            .points
            .iter()
            .filter_map(|point| {
                let px = viewport.position_to_x(position_coordinate(point.position));
                let py = viewport.normalized_to_y(snapshot.descriptor.normalize(point.value));
                let distance = (px - x).hypot(py - y);
                (distance <= 11.0).then_some((distance, point.id))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, id)| id)
    }

    fn begin_curve_edit(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some(lane_id) = self.selected_lane else {
            return;
        };
        let Some(snapshot) = self.lane_snapshot(lane_id) else {
            return;
        };
        let graph = self.graph_snapshot();
        let base_revision = graph.revision();
        let (x, y) = (f32::from(event.position.x), f32::from(event.position.y));
        if let Some(point_id) = self.point_at(x, y, &snapshot) {
            let Some(point) = snapshot
                .points
                .iter()
                .find(|point| point.id == point_id)
                .cloned()
            else {
                return;
            };
            self.selected_point = Some(point_id);
            self.gesture = Some(AutomationGesture {
                base_revision,
                lane: lane_id,
                point,
                is_new: false,
            });
            self.status = "Dragging point · exact project-time/value edit".into();
            cx.notify();
            return;
        }
        let Some(viewport) = self.viewport() else {
            return;
        };
        let requested = viewport.x_to_position(x);
        let coordinate = snap_coordinate(requested, self.snap.interval(snapshot.time_domain));
        let value = snapshot.descriptor.denormalize(viewport.y_to_normalized(y));
        let Ok(id) = graph.next_point_id_candidate() else {
            self.status = "Automation point identity space is exhausted".into();
            cx.notify();
            return;
        };
        self.gesture = Some(AutomationGesture {
            base_revision,
            lane: lane_id,
            point: AutomationPoint {
                id,
                position: position_for_domain(snapshot.time_domain, coordinate),
                value,
                outgoing: SegmentShape::Linear,
            },
            is_new: true,
        });
        self.selected_point = Some(id);
        self.cursor_coordinate = coordinate;
        self.status = format!(
            "New point preview · {} · release to commit",
            self.snap.label()
        );
        cx.notify();
    }

    fn drag_curve_point(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let (Some(mut gesture), Some(viewport)) = (self.gesture.clone(), self.viewport()) else {
            return;
        };
        let Some(snapshot) = self.lane_snapshot(gesture.lane) else {
            return;
        };
        let requested = viewport.x_to_position(f32::from(event.position.x));
        let snapped = snap_coordinate(requested, self.snap.interval(snapshot.time_domain));
        let graph = self.graph_snapshot();
        let Some(lane) = graph.lane(gesture.lane) else {
            return;
        };
        let coordinate = if gesture.is_new {
            snapped.clamp(self.view_start, self.view_end)
        } else {
            clamp_point_coordinate(
                lane.points(),
                gesture.point.id,
                snapped,
                self.view_start,
                self.view_end,
            )
        };
        let value = snapshot
            .descriptor
            .denormalize(viewport.y_to_normalized(f32::from(event.position.y)));
        gesture.point.position = position_for_domain(snapshot.time_domain, coordinate);
        gesture.point.value = value;
        self.gesture = Some(gesture);
        self.cursor_coordinate = coordinate;
        self.status = format!(
            "Gesture preview · {} · release to commit",
            self.snap.label()
        );
        cx.notify();
    }

    fn end_curve_edit(&mut self, _event: &MouseUpEvent, cx: &mut Context<Self>) {
        let Some(gesture) = self.gesture.take() else {
            return;
        };
        let graph = self.graph_snapshot();
        if graph.revision() != gesture.base_revision {
            self.status = format!(
                "Point edit cancelled: lane changed at revision {}",
                graph.revision()
            );
            cx.notify();
            return;
        }
        let Some(lane) = graph.lane(gesture.lane) else {
            self.status = "Point edit cancelled: lane was removed".into();
            cx.notify();
            return;
        };
        if !gesture.is_new
            && !lane
                .points()
                .iter()
                .any(|point| point.id == gesture.point.id)
        {
            self.status = "Point edit cancelled: point was removed".into();
            cx.notify();
            return;
        }
        let controller_allocates_identity =
            gesture.is_new && self.integration_mode == ControlIntegrationMode::Controller;
        self.dispatch_automation(gesture.into_intent(), cx);
        if controller_allocates_identity {
            self.selected_point = None;
        }
    }

    fn set_cursor_from_event(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if self.gesture.is_some() {
            return;
        }
        if let Some(viewport) = self.viewport() {
            self.cursor_coordinate = viewport.x_to_position(f32::from(event.position.x));
            cx.notify();
        }
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        if let Some(callback) = self.callback.as_ref() {
            callback(ControlAction::History(ControlHistoryIntent {
                surface: ControlSurface::Automation,
                expected_revision: self.graph_snapshot().revision(),
                direction: HistoryDirection::Undo,
            }));
            self.status = "Automation undo sent to project controller".into();
        } else if self.integration_mode == ControlIntegrationMode::Compatibility {
            self.status = match self.backend.undo() {
                Ok(true) => "Undid automation edit · compatibility history".into(),
                Ok(false) => "Automation history is already at its beginning".into(),
                Err(error) => format!("Undo failed: {error}"),
            };
        } else {
            self.status = "Automation undo not sent · no project command adapter attached".into();
        }
        cx.notify();
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        if let Some(callback) = self.callback.as_ref() {
            callback(ControlAction::History(ControlHistoryIntent {
                surface: ControlSurface::Automation,
                expected_revision: self.graph_snapshot().revision(),
                direction: HistoryDirection::Redo,
            }));
            self.status = "Automation redo sent to project controller".into();
        } else if self.integration_mode == ControlIntegrationMode::Compatibility {
            self.status = match self.backend.redo() {
                Ok(true) => "Redid automation edit · compatibility history".into(),
                Ok(false) => "Nothing to redo".into(),
                Err(error) => format!("Redo failed: {error}"),
            };
        } else {
            self.status = "Automation redo not sent · no project command adapter attached".into();
        }
        cx.notify();
    }

    fn compiled_preview(&self, snapshot: &LaneSnapshot) -> Option<f64> {
        let tempo = FixedTempo::new(48_000, 120_000_000).ok()?;
        let graph = self.graph_snapshot();
        let compiled = graph.compile(&tempo).ok()?;
        let frame = match graph.lane(snapshot.id).map(|lane| lane.time_domain)? {
            TimeDomain::Beats => tempo.beat_to_frame(BeatTime(self.cursor_coordinate)),
            TimeDomain::Frames => ProjectFrame(self.cursor_coordinate),
        };
        compiled.value_at(
            &snapshot.descriptor.address,
            frame,
            snapshot.descriptor.default,
        )
    }

    fn render_lane_row(&self, lane: LaneSnapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let id = lane.id;
        let selected = self.selected_lane == Some(id);
        div()
            .id(SharedString::from(format!("automation-lane-{}", id.get())))
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(if selected { 0x15202a } else { PANEL_ALT }))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.select_lane(id, cx)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_sm().text_color(rgb(TEXT)).child(lane.name))
                    .child(
                        div()
                            .id(SharedString::from(format!("lane-enabled-{}", id.get())))
                            .px_2()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(if lane.enabled { LIME } else { BORDER }))
                            .text_xs()
                            .text_color(rgb(if lane.enabled { LIME } else { DIM }))
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle_lane(id, cx)))
                            .child(if lane.enabled { "ON" } else { "OFF" }),
                    ),
            )
            .child(div().text_xs().text_color(rgb(MUTED)).child(lane.target))
            .child(div().mt_1().text_xs().text_color(rgb(DIM)).child(format!(
                "{:?} · {} points",
                lane.binding,
                lane.points.len()
            )))
    }

    fn render_curve(&self, snapshot: &LaneSnapshot) -> impl IntoElement {
        let bounds_store = self.curve_bounds.clone();
        let points = snapshot.points.clone();
        let descriptor = snapshot.descriptor.clone();
        let selected = self.selected_point;
        let view_start = self.view_start;
        let view_end = self.view_end;
        canvas(
            move |bounds, _, _| {
                *bounds_store.lock().unwrap() = Some(bounds);
                bounds
            },
            move |bounds, _, window, _| {
                let viewport = CurveViewport {
                    left: f32::from(bounds.origin.x),
                    top: f32::from(bounds.origin.y),
                    width: f32::from(bounds.size.width),
                    height: f32::from(bounds.size.height),
                    start: view_start,
                    end: view_end,
                };
                for fraction in [0.25_f32, 0.5, 0.75] {
                    let x = bounds.origin.x + bounds.size.width * fraction;
                    window.paint_quad(quad(
                        Bounds::new(
                            point(x, bounds.origin.y),
                            gpui::size(px(1.0), bounds.size.height),
                        ),
                        px(0.0),
                        rgba(0xffffff12),
                        px(0.0),
                        rgba(0x00000000),
                        Default::default(),
                    ));
                    let y = bounds.origin.y + bounds.size.height * fraction;
                    window.paint_quad(quad(
                        Bounds::new(
                            point(bounds.origin.x, y),
                            gpui::size(bounds.size.width, px(1.0)),
                        ),
                        px(0.0),
                        rgba(0xffffff0d),
                        px(0.0),
                        rgba(0x00000000),
                        Default::default(),
                    ));
                }
                if points.len() >= 2 {
                    let mut builder = PathBuilder::stroke(px(2.0));
                    for (index, sample) in
                        sampled_curve(&points, &descriptor, view_start, view_end, 384)
                            .into_iter()
                            .enumerate()
                    {
                        let location = point(
                            px(viewport.position_to_x(sample.0)),
                            px(viewport.normalized_to_y(sample.1)),
                        );
                        if index == 0 {
                            builder.move_to(location);
                        } else {
                            builder.line_to(location);
                        }
                    }
                    if let Ok(path) = builder.build() {
                        window.paint_path(path, rgba(0x50d8d7ee));
                    }
                }
                for automation_point in &points {
                    let x =
                        px(viewport.position_to_x(position_coordinate(automation_point.position)));
                    let y =
                        px(viewport.normalized_to_y(descriptor.normalize(automation_point.value)));
                    let is_selected = selected == Some(automation_point.id);
                    window.paint_quad(quad(
                        Bounds::new(
                            point(x - px(5.0), y - px(5.0)),
                            gpui::size(px(10.0), px(10.0)),
                        ),
                        px(if is_selected { 5.0 } else { 2.0 }),
                        rgba(if is_selected { 0xf6b760ff } else { 0x50d8d7ff }),
                        px(1.0),
                        rgba(0x090b10ff),
                        Default::default(),
                    ));
                }
            },
        )
        .size_full()
    }
}

impl Focusable for AutomationView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AutomationView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let graph = self.graph_snapshot();
        let render_label = self
            .render_status
            .as_ref()
            .map(ControlRenderStatus::label)
            .unwrap_or_else(|| "RENDER STATUS UNAVAILABLE".into());
        let lanes: Vec<_> = graph
            .lanes()
            .filter_map(|lane| self.lane_snapshot(lane.id))
            .collect();
        let selected = self.selected_lane.and_then(|id| self.lane_snapshot(id));
        let mut lane_list = div().flex().flex_col();
        for lane in lanes {
            lane_list = lane_list.child(self.render_lane_row(lane, cx));
        }
        let preview = selected
            .as_ref()
            .and_then(|lane| self.compiled_preview(lane));
        let selected_shape = selected.as_ref().and_then(|lane| {
            let point = self.selected_point?;
            lane.points
                .iter()
                .find(|candidate| candidate.id == point)
                .map(|point| point.outgoing)
        });
        let curve = selected
            .as_ref()
            .map(|snapshot| self.render_curve(snapshot).into_any_element());

        div()
            .key_context("AudecAutomation")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &ControlUndo, _, cx| this.undo(cx)))
            .on_action(cx.listener(|this, _: &ControlRedo, _, cx| this.redo(cx)))
            .on_action(
                cx.listener(|this, _: &DeleteAutomationPoint, _, cx| this.delete_selected(cx)),
            )
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .child(
                div()
                    .h(px(54.0))
                    .flex_none()
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL_ALT))
                    .child(
                        div()
                            .child(div().text_sm().child("AUTOMATION EDITOR"))
                            .child(div().text_xs().text_color(rgb(DIM)).child(render_label)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                header_button("automation-mode", &format!("{:?}", self.write_mode))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.cycle_write_mode(cx)),
                                    ),
                            )
                            .child(
                                header_button("automation-snap", self.snap.label())
                                    .on_click(cx.listener(|this, _, _, cx| this.cycle_snap(cx))),
                            )
                            .child(
                                header_button("automation-undo", "Undo")
                                    .on_click(cx.listener(|this, _, _, cx| this.undo(cx))),
                            )
                            .child(
                                header_button("automation-redo", "Redo")
                                    .on_click(cx.listener(|this, _, _, cx| this.redo(cx))),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .id("automation-lane-list")
                            .w(px(260.0))
                            .flex_none()
                            .overflow_y_scroll()
                            .border_r_1()
                            .border_color(rgb(BORDER))
                            .bg(rgb(PANEL_ALT))
                            .child(section_label("TARGET LANES").px_3().py_2())
                            .child(lane_list),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .h(px(42.0))
                                    .flex_none()
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .border_b_1()
                                    .border_color(rgb(BORDER))
                                    .child(
                                        div()
                                            .flex()
                                            .gap_2()
                                            .child(shape_button(
                                                "Hold",
                                                SegmentShape::Hold,
                                                selected_shape,
                                                cx,
                                            ))
                                            .child(shape_button(
                                                "Linear",
                                                SegmentShape::Linear,
                                                selected_shape,
                                                cx,
                                            ))
                                            .child(shape_button(
                                                "Smooth",
                                                SegmentShape::Smooth,
                                                selected_shape,
                                                cx,
                                            ))
                                            .child(shape_button(
                                                "Expo",
                                                SegmentShape::Exponential,
                                                selected_shape,
                                                cx,
                                            ))
                                            .child(
                                                header_button(
                                                    "automation-binding",
                                                    &format!(
                                                        "Binding {:?}",
                                                        selected
                                                            .as_ref()
                                                            .map(|lane| lane.binding)
                                                            .unwrap_or(BindingMode::Replace)
                                                    ),
                                                )
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.cycle_binding(cx)
                                                })),
                                            )
                                            .child(
                                                header_button("automation-delete", "Delete point")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.delete_selected(cx)
                                                    })),
                                            ),
                                    )
                                    .child(div().text_xs().text_color(rgb(CYAN)).child(
                                        match (selected.as_ref(), preview) {
                                            (Some(lane), Some(value)) => format!(
                                                "{} @ {:.2} beats = {}",
                                                lane.descriptor.name,
                                                self.cursor_coordinate as f64 / PPQ as f64,
                                                format_parameter_value(
                                                    value,
                                                    &lane.descriptor.unit
                                                ),
                                            ),
                                            _ => "No compiled value".into(),
                                        },
                                    )),
                            )
                            .child(
                                div()
                                    .id("automation-curve-hit-area")
                                    .relative()
                                    .flex_1()
                                    .min_h_0()
                                    .cursor_crosshair()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                            this.begin_curve_edit(event, cx);
                                            cx.stop_propagation();
                                        }),
                                    )
                                    .on_mouse_move(cx.listener(
                                        |this, event: &MouseMoveEvent, _, cx| {
                                            if this.gesture.is_some() {
                                                this.drag_curve_point(event, cx);
                                            } else {
                                                this.set_cursor_from_event(event, cx);
                                            }
                                        },
                                    ))
                                    .capture_any_mouse_up(cx.listener(
                                        |this, event: &MouseUpEvent, _, cx| {
                                            this.end_curve_edit(event, cx)
                                        },
                                    ))
                                    .child(curve.unwrap_or_else(|| {
                                        div()
                                            .size_full()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_color(rgb(DIM))
                                            .child("Select an automation lane")
                                            .into_any_element()
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .h(px(28.0))
                    .flex_none()
                    .px_4()
                    .flex()
                    .items_center()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL_ALT))
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(self.status.clone()),
            )
    }
}

fn demo_mixer() -> MixerGraph {
    let mut graph = MixerGraph::new("Master");
    let drums = graph.add_bus(BusKind::Component, "Drums").unwrap();
    let bass = graph.add_bus(BusKind::Component, "Bass / slides").unwrap();
    let voice = graph.add_bus(BusKind::Component, "Voice").unwrap();
    let air = graph.add_bus(BusKind::Group, "Cold room").unwrap();
    graph
        .insert_processor(
            drums,
            None,
            PluginDescriptor::new("builtin", "transient-shaper", "Transient contour"),
            0,
        )
        .unwrap();
    graph
        .insert_processor(
            voice,
            None,
            PluginDescriptor::new("builtin", "spectral-gate", "Spectral gate"),
            256,
        )
        .unwrap();
    graph
        .add_send(drums, air, SendTap::PostFader, -15.0)
        .unwrap();
    graph.add_send(voice, air, SendTap::PreFader, -9.0).unwrap();
    graph.set_pan(bass, -0.12).unwrap();
    graph.set_pan(voice, 0.08).unwrap();
    graph
}

fn demo_automation() -> AutomationGraph {
    let mut graph = AutomationGraph::new();
    let gain_address = ParameterAddress::Mixer(MixerTarget::BusGain(2));
    graph
        .register_parameter(ParameterDescriptor {
            address: gain_address.clone(),
            name: "Drums gain".into(),
            unit: ParameterUnit::Decibels,
            minimum: -72.0,
            maximum: 12.0,
            default: 0.0,
            mapping: ValueMapping::Linear,
            smoothing: SmoothingPolicy::LinearFrames(64),
        })
        .unwrap();
    let cutoff_address = ParameterAddress::Plugin {
        processor_id: 1,
        key: "cutoff".into(),
    };
    graph
        .register_parameter(ParameterDescriptor {
            address: cutoff_address.clone(),
            name: "Filter cutoff".into(),
            unit: ParameterUnit::Hertz,
            minimum: 30.0,
            maximum: 18_000.0,
            default: 1_200.0,
            mapping: ValueMapping::Logarithmic,
            smoothing: SmoothingPolicy::OnePoleMilliseconds(8.0),
        })
        .unwrap();
    let gain = graph
        .create_lane("Drum emphasis", gain_address, TimeDomain::Beats)
        .unwrap();
    let cutoff = graph
        .create_lane("Cold-room aperture", cutoff_address, TimeDomain::Beats)
        .unwrap();
    for (beat, value, shape) in [
        (0, -12.0, SegmentShape::Smooth),
        (4, 0.0, SegmentShape::Hold),
        (8, -5.0, SegmentShape::Linear),
        (12, 3.0, SegmentShape::Smooth),
        (16, -2.0, SegmentShape::Linear),
    ] {
        graph
            .insert_point(
                gain,
                TimePosition::Beats(BeatTime(beat * PPQ)),
                value,
                shape,
            )
            .unwrap();
    }
    for (beat, value, shape) in [
        (0, 280.0, SegmentShape::Exponential),
        (4, 2_400.0, SegmentShape::Smooth),
        (8, 640.0, SegmentShape::Exponential),
        (12, 8_200.0, SegmentShape::Smooth),
        (16, 1_200.0, SegmentShape::Linear),
    ] {
        graph
            .insert_point(
                cutoff,
                TimePosition::Beats(BeatTime(beat * PPQ)),
                value,
                shape,
            )
            .unwrap();
    }
    graph
}

fn header_button(id: &'static str, label: &str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL))
        .cursor_pointer()
        .text_xs()
        .text_color(rgb(TEXT))
        .child(label.to_owned())
}

fn section_label(label: &'static str) -> gpui::Div {
    div().text_xs().text_color(rgb(DIM)).child(label)
}

fn empty_slot(title: &'static str, detail: &'static str) -> impl IntoElement {
    div()
        .px_2()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL_ALT))
        .child(div().text_xs().text_color(rgb(MUTED)).child(title))
        .child(div().text_xs().text_color(rgb(DIM)).child(detail))
}

fn step_button<F>(
    prefix: &'static str,
    bus: BusId,
    label: &'static str,
    action: F,
    cx: &mut Context<MixerView>,
) -> impl IntoElement
where
    F: Fn(&mut MixerView, &mut Context<MixerView>) + 'static,
{
    div()
        .id(SharedString::from(format!("{prefix}-{}", bus.get())))
        .w(px(28.0))
        .h(px(24.0))
        .rounded_sm()
        .border_1()
        .border_color(rgb(BORDER))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_xs()
        .on_click(cx.listener(move |this, _, _, cx| action(this, cx)))
        .child(label)
}

fn toggle_button<F>(
    prefix: &'static str,
    bus: BusId,
    label: &'static str,
    active: bool,
    color: u32,
    action: F,
    cx: &mut Context<MixerView>,
) -> impl IntoElement
where
    F: Fn(&mut MixerView, &mut Context<MixerView>) + 'static,
{
    div()
        .id(SharedString::from(format!("{prefix}-{}", bus.get())))
        .h(px(28.0))
        .flex_1()
        .rounded_sm()
        .border_1()
        .border_color(rgb(if active { color } else { BORDER }))
        .bg(rgb(if active { color } else { PANEL_ALT }))
        .text_color(rgb(if active { BACKGROUND } else { MUTED }))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| action(this, cx)))
        .child(label)
}

fn vertical_meter(fraction: f32, reading: Option<MeterReading>) -> impl IntoElement {
    div()
        .w(px(30.0))
        .h_full()
        .flex()
        .flex_col()
        .items_center()
        .gap_1()
        .child(
            div()
                .relative()
                .w(px(10.0))
                .flex_1()
                .rounded_sm()
                .bg(rgb(0x080a0e))
                .border_1()
                .border_color(rgb(BORDER))
                .child(
                    div()
                        .absolute()
                        .bottom_0()
                        .w_full()
                        .h(relative(fraction))
                        .bg(rgb(if fraction > 0.9 { MAGENTA } else { LIME })),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(if reading.is_some() { MUTED } else { DIM }))
                .child(
                    reading
                        .map(|m| format!("{:.0}", m.peak_db))
                        .unwrap_or_else(|| "NO TAP".into()),
                ),
        )
}

fn vertical_fader(fraction: f32, gain_db: f32) -> impl IntoElement {
    div()
        .w(px(48.0))
        .h_full()
        .flex()
        .flex_col()
        .items_center()
        .gap_1()
        .child(
            div()
                .relative()
                .w(px(6.0))
                .flex_1()
                .rounded_sm()
                .bg(rgb(BORDER))
                .child(
                    div()
                        .absolute()
                        .bottom(relative(fraction))
                        .left(px(-8.0))
                        .w(px(22.0))
                        .h(px(8.0))
                        .rounded_sm()
                        .bg(rgb(CYAN)),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(DIM))
                .child(format!("{gain_db:+.0}")),
        )
}

fn shape_button(
    label: &'static str,
    shape: SegmentShape,
    selected: Option<SegmentShape>,
    cx: &mut Context<AutomationView>,
) -> impl IntoElement {
    let active = selected == Some(shape);
    div()
        .id(SharedString::from(format!("shape-{label}")))
        .px_2()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(rgb(if active { CYAN } else { BORDER }))
        .bg(rgb(if active { 0x173238 } else { PANEL_ALT }))
        .text_xs()
        .text_color(rgb(if active { CYAN } else { MUTED }))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| this.set_selected_shape(shape, cx)))
        .child(label)
}

pub fn gain_to_fader_fraction(gain_db: f32) -> f32 {
    ((gain_db.clamp(-72.0, 12.0) + 72.0) / 84.0).powf(0.55)
}

pub fn db_to_meter_fraction(db: f32) -> f32 {
    ((db.clamp(-72.0, 6.0) + 72.0) / 78.0).clamp(0.0, 1.0)
}

fn format_pan(pan: f32) -> String {
    if pan.abs() < 0.005 {
        "C".into()
    } else if pan < 0.0 {
        format!("L{:02}", (-pan * 100.0).round())
    } else {
        format!("R{:02}", (pan * 100.0).round())
    }
}

fn describe_target(target: &ParameterAddress) -> String {
    match target {
        ParameterAddress::Mixer(target) => format!("Mixer / {target:?}"),
        ParameterAddress::Plugin { processor_id, key } => format!("Insert {processor_id} / {key}"),
        ParameterAddress::Clip { clip_id, parameter } => format!("Clip {clip_id} / {parameter:?}"),
        ParameterAddress::Decomposition(target) => format!("Decomposition / {target:?}"),
        ParameterAddress::PerceptualLens { lens_id, parameter } => {
            format!("Lens {lens_id} / {parameter:?}")
        }
        ParameterAddress::AirParameter(id) => format!("AIR parameter {id}"),
        ParameterAddress::Custom {
            namespace,
            entity,
            parameter,
        } => format!("{namespace} / {entity} / {parameter}"),
    }
}

fn format_parameter_value(value: f64, unit: &ParameterUnit) -> String {
    match unit {
        ParameterUnit::Decibels => format!("{value:+.2} dB"),
        ParameterUnit::Hertz if value >= 1000.0 => format!("{:.2} kHz", value / 1000.0),
        ParameterUnit::Hertz => format!("{value:.1} Hz"),
        ParameterUnit::Percent => format!("{value:.1}%"),
        ParameterUnit::Boolean => {
            if value >= 0.5 {
                "on".into()
            } else {
                "off".into()
            }
        }
        _ => format!("{value:.3}"),
    }
}

fn position_coordinate(position: TimePosition) -> i64 {
    match position {
        TimePosition::Frames(ProjectFrame(frame)) => frame,
        TimePosition::Beats(BeatTime(ticks)) => ticks,
    }
}

fn position_for_domain(domain: TimeDomain, coordinate: i64) -> TimePosition {
    match domain {
        TimeDomain::Frames => TimePosition::Frames(ProjectFrame(coordinate)),
        TimeDomain::Beats => TimePosition::Beats(BeatTime(coordinate)),
    }
}

pub fn snap_coordinate(coordinate: i64, interval: Option<i64>) -> i64 {
    let Some(interval) = interval.filter(|interval| *interval > 1) else {
        return coordinate;
    };
    let lower = coordinate.div_euclid(interval);
    let remainder = coordinate.rem_euclid(interval);
    lower
        .saturating_add(i64::from(remainder.saturating_mul(2) >= interval))
        .saturating_mul(interval)
}

pub fn clamp_point_coordinate(
    points: &[AutomationPoint],
    id: AutomationPointId,
    requested: i64,
    view_start: i64,
    view_end: i64,
) -> i64 {
    let Some(index) = points.iter().position(|point| point.id == id) else {
        return requested.clamp(view_start, view_end);
    };
    let minimum = index
        .checked_sub(1)
        .map(|prior| position_coordinate(points[prior].position).saturating_add(1))
        .unwrap_or(view_start);
    let maximum = points
        .get(index + 1)
        .map(|next| position_coordinate(next.position).saturating_sub(1))
        .unwrap_or(view_end);
    requested.clamp(minimum.min(maximum), maximum.max(minimum))
}

/// Samples the authored lane with the backend's own interpolation semantics.
pub fn sampled_curve(
    points: &[AutomationPoint],
    descriptor: &ParameterDescriptor,
    start: i64,
    end: i64,
    samples: usize,
) -> Vec<(i64, f64)> {
    if samples == 0 || end <= start {
        return Vec::new();
    }
    let mut lane = AutomationLane::new(
        AutomationLaneId::from_raw(u64::MAX),
        "curve preview",
        descriptor.address.clone(),
        points
            .first()
            .map(|point| match point.position {
                TimePosition::Frames(_) => TimeDomain::Frames,
                TimePosition::Beats(_) => TimeDomain::Beats,
            })
            .unwrap_or(TimeDomain::Beats),
    );
    for point in points.iter().cloned() {
        let _ = lane.insert_point(point);
    }
    (0..samples)
        .map(|index| {
            let fraction = if samples == 1 {
                0.0
            } else {
                index as f64 / (samples - 1) as f64
            };
            let coordinate = start + ((end - start) as f64 * fraction).round() as i64;
            let value = lane
                .value_at(
                    position_for_domain(lane.time_domain, coordinate),
                    descriptor,
                )
                .unwrap_or(descriptor.default);
            (coordinate, descriptor.normalize(value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::AutomationCommand;

    fn descriptor() -> ParameterDescriptor {
        ParameterDescriptor {
            address: ParameterAddress::Mixer(MixerTarget::BusGain(2)),
            name: "gain".into(),
            unit: ParameterUnit::Decibels,
            minimum: -72.0,
            maximum: 12.0,
            default: 0.0,
            mapping: ValueMapping::Linear,
            smoothing: SmoothingPolicy::None,
        }
    }

    #[test]
    fn curve_viewport_round_trips_coordinates() {
        let viewport = CurveViewport {
            left: 20.0,
            top: 10.0,
            width: 800.0,
            height: 300.0,
            start: -960,
            end: 15_360,
        };
        for coordinate in [-960, 0, 3840, 15_360] {
            let round_trip = viewport.x_to_position(viewport.position_to_x(coordinate));
            assert!((round_trip - coordinate).abs() <= 1);
        }
        for value in [0.0, 0.25, 0.5, 1.0] {
            let round_trip = viewport.y_to_normalized(viewport.normalized_to_y(value));
            assert!((round_trip - value).abs() < 1.0e-6);
        }
    }

    #[test]
    fn point_drag_cannot_cross_neighbors() {
        let points = [0, 10, 20]
            .into_iter()
            .enumerate()
            .map(|(index, frame)| AutomationPoint {
                id: AutomationPointId::from_raw(index as u64 + 1),
                position: TimePosition::Frames(ProjectFrame(frame)),
                value: 0.0,
                outgoing: SegmentShape::Linear,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            clamp_point_coordinate(&points, points[1].id, -99, -100, 100),
            1
        );
        assert_eq!(
            clamp_point_coordinate(&points, points[1].id, 99, -100, 100),
            19
        );
    }

    #[test]
    fn automation_snap_is_symmetric_across_zero_and_handles_ties() {
        assert_eq!(snap_coordinate(479, Some(960)), 0);
        assert_eq!(snap_coordinate(480, Some(960)), 960);
        assert_eq!(snap_coordinate(-479, Some(960)), 0);
        assert_eq!(snap_coordinate(-480, Some(960)), 0);
        assert_eq!(snap_coordinate(-481, Some(960)), -960);
        assert_eq!(snap_coordinate(123, None), 123);
    }

    #[test]
    fn fader_and_meter_geometry_is_bounded_and_monotonic() {
        let gains = [-100.0, -72.0, -36.0, 0.0, 12.0, 50.0];
        let fractions: Vec<_> = gains.into_iter().map(gain_to_fader_fraction).collect();
        assert!(fractions.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(fractions.iter().all(|value| (0.0..=1.0).contains(value)));
        assert_eq!(db_to_meter_fraction(-100.0), 0.0);
        assert_eq!(db_to_meter_fraction(12.0), 1.0);
    }

    #[test]
    fn completed_mixer_gesture_yields_exactly_one_semantic_intent() {
        let bus = BusId::from_raw(7);
        let mut gesture = Some(MixerGesture {
            bus,
            control: MixerControl::Gain,
            base_revision: 41,
            origin_x: 0.0,
            origin_y: 0.0,
            original: 0.0,
            preview: -6.0,
        });

        let first = gesture.take().and_then(MixerGesture::into_intent);
        let second = gesture.take().and_then(MixerGesture::into_intent);
        assert_eq!(
            first,
            Some(MixerActionIntent::new(
                41,
                MixerAction::SetGainDb { bus, gain_db: -6.0 }
            ))
        );
        assert_eq!(second, None);
    }

    #[test]
    fn completed_automation_gesture_yields_exactly_one_semantic_intent() {
        let lane = AutomationLaneId::from_raw(3);
        let point = AutomationPoint {
            id: AutomationPointId::from_raw(9),
            position: TimePosition::Beats(BeatTime(1_920)),
            value: -12.0,
            outgoing: SegmentShape::Linear,
        };
        let mut gesture = Some(AutomationGesture {
            base_revision: 12,
            lane,
            point: point.clone(),
            is_new: false,
        });

        let first = gesture.take().map(AutomationGesture::into_intent);
        let second = gesture.take().map(AutomationGesture::into_intent);
        assert_eq!(
            first,
            Some(AutomationActionIntent::new(
                12,
                AutomationAction::MovePoint { lane, point }
            ))
        );
        assert_eq!(second, None);
    }

    #[test]
    fn local_mixer_backend_executes_undo_and_redo() {
        let graph = demo_mixer();
        let bus = graph
            .buses()
            .find(|bus| bus.kind() != BusKind::Master)
            .unwrap()
            .id();
        let mut backend = LocalMixerBackend::new(graph, 8);
        let snapshot = backend.snapshot();
        let command =
            MixerCommand::build("gain", &snapshot, |graph| graph.set_gain_db(bus, -9.0)).unwrap();
        backend.execute(command).unwrap();
        assert_eq!(backend.snapshot().bus(bus).unwrap().fader().gain_db(), -9.0);
        assert!(backend.undo().unwrap());
        assert_eq!(backend.snapshot().bus(bus).unwrap().fader().gain_db(), 0.0);
        assert!(backend.redo().unwrap());
        assert_eq!(backend.snapshot().bus(bus).unwrap().fader().gain_db(), -9.0);
    }

    #[test]
    fn shared_mixer_backend_publishes_edit_undo_and_redo() {
        let graph = demo_mixer();
        let bus = graph
            .buses()
            .find(|bus| bus.kind() != BusKind::Master)
            .unwrap()
            .id();
        let shared_graph = Arc::new(Mutex::new(graph));
        let mut backend = SharedMixerBackend::new(Arc::clone(&shared_graph), 8);
        let snapshot = backend.snapshot();
        let command =
            MixerCommand::build("gain", &snapshot, |graph| graph.set_gain_db(bus, -9.0)).unwrap();

        backend.execute(command).unwrap();
        assert_eq!(
            shared_graph
                .lock()
                .unwrap()
                .bus(bus)
                .unwrap()
                .fader()
                .gain_db(),
            -9.0
        );

        assert!(backend.undo().unwrap());
        assert_eq!(
            shared_graph
                .lock()
                .unwrap()
                .bus(bus)
                .unwrap()
                .fader()
                .gain_db(),
            0.0
        );

        assert!(backend.redo().unwrap());
        assert_eq!(
            shared_graph
                .lock()
                .unwrap()
                .bus(bus)
                .unwrap()
                .fader()
                .gain_db(),
            -9.0
        );
    }

    #[test]
    fn shared_mixer_backend_rejects_stale_gesture_without_shadow_overwrite() {
        let graph = demo_mixer();
        let bus = graph
            .buses()
            .find(|bus| bus.kind() != BusKind::Master)
            .unwrap()
            .id();
        let shared_graph = Arc::new(Mutex::new(graph));
        let mut backend = SharedMixerBackend::new(Arc::clone(&shared_graph), 8);
        let stale_snapshot = backend.snapshot();
        let stale = MixerCommand::build("stale fader", &stale_snapshot, |graph| {
            graph.set_gain_db(bus, -24.0)
        })
        .unwrap();

        shared_graph.lock().unwrap().set_pan(bus, 0.75).unwrap();
        assert_eq!(backend.execute(stale), Err(MixerError::CommandConflict));
        let truth = shared_graph.lock().unwrap();
        assert_eq!(truth.bus(bus).unwrap().fader().pan(), 0.75);
        assert_eq!(truth.bus(bus).unwrap().fader().gain_db(), 0.0);
    }

    #[test]
    fn sampled_curve_uses_real_segment_semantics() {
        let descriptor = descriptor();
        let points = vec![
            AutomationPoint {
                id: AutomationPointId::from_raw(1),
                position: TimePosition::Beats(BeatTime(0)),
                value: -72.0,
                outgoing: SegmentShape::Hold,
            },
            AutomationPoint {
                id: AutomationPointId::from_raw(2),
                position: TimePosition::Beats(BeatTime(100)),
                value: 12.0,
                outgoing: SegmentShape::Linear,
            },
        ];
        let curve = sampled_curve(&points, &descriptor, 0, 100, 3);
        assert_eq!(curve.len(), 3);
        assert_eq!(curve[1].1, 0.0, "hold remains at the first value");
        assert_eq!(curve[2].1, 1.0);
    }

    #[test]
    fn automation_edits_are_real_history_commands() {
        let graph = demo_automation();
        let lane_id = graph.lanes().next().unwrap().id;
        let before = graph.lane(lane_id).unwrap().clone();
        let mut after = before.clone();
        after.enabled = false;
        let command = AutomationCommand::replace("disable", before, after).unwrap();
        let mut backend = LocalAutomationBackend::new(graph, 8);
        let revision = backend.snapshot().revision();
        backend
            .execute(AutomationIntent::new(revision, command))
            .unwrap();
        assert!(!backend.snapshot().lane(lane_id).unwrap().enabled);
        assert!(backend.undo().unwrap());
        assert!(backend.snapshot().lane(lane_id).unwrap().enabled);
    }

    #[test]
    fn shared_automation_backend_publishes_edit_undo_and_redo() {
        let graph = demo_automation();
        let lane_id = graph.lanes().next().unwrap().id;
        let before = graph.lane(lane_id).unwrap().clone();
        let mut after = before.clone();
        after.enabled = false;
        let command = AutomationCommand::replace("disable", before, after).unwrap();
        let shared_graph = Arc::new(Mutex::new(graph));
        let mut backend = SharedAutomationBackend::new(Arc::clone(&shared_graph), 8);

        let revision = backend.snapshot().revision();
        backend
            .execute(AutomationIntent::new(revision, command))
            .unwrap();
        assert!(!shared_graph.lock().unwrap().lane(lane_id).unwrap().enabled);

        assert!(backend.undo().unwrap());
        assert!(shared_graph.lock().unwrap().lane(lane_id).unwrap().enabled);

        assert!(backend.redo().unwrap());
        assert!(!shared_graph.lock().unwrap().lane(lane_id).unwrap().enabled);
    }

    #[test]
    fn shared_automation_backend_rejects_stale_revision_without_shadow_overwrite() {
        let graph = demo_automation();
        let lane_id = graph.lanes().next().unwrap().id;
        let shared_graph = Arc::new(Mutex::new(graph));
        let mut backend = SharedAutomationBackend::new(Arc::clone(&shared_graph), 8);
        let snapshot = backend.snapshot();
        let mut stale_after = snapshot.lane(lane_id).unwrap().clone();
        stale_after.name = "stale name".into();
        let stale = AutomationIntent::new(
            snapshot.revision(),
            AutomationCommand::replace(
                "stale rename",
                snapshot.lane(lane_id).unwrap().clone(),
                stale_after,
            )
            .unwrap(),
        );

        let mut current = snapshot.lane(lane_id).unwrap().clone();
        current.enabled = false;
        shared_graph
            .lock()
            .unwrap()
            .apply_intent(&AutomationIntent::new(
                snapshot.revision(),
                AutomationCommand::replace(
                    "controller edit",
                    snapshot.lane(lane_id).unwrap().clone(),
                    current,
                )
                .unwrap(),
            ))
            .unwrap();

        let error = backend.execute(stale).unwrap_err();
        assert!(error.contains("revision conflict"));
        let truth = shared_graph.lock().unwrap();
        assert!(!truth.lane(lane_id).unwrap().enabled);
        assert_ne!(truth.lane(lane_id).unwrap().name, "stale name");
    }
}
