//! DAW control surfaces for Audec's mixer and automation engines.
//!
//! A view holds a read-only snapshot published by the project controller and a
//! callback. Every edit leaves as one typed [`ControlAction`]; the view mutates
//! nothing and keeps no history. Its status reads "requested" until the
//! controller's [`ControlReceipt`] names the commit or the refusal.

#[path = "control_actions.rs"]
pub mod control_actions;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use gpui::{
    actions, canvas, div, point, prelude::*, px, quad, relative, rgb, rgba, App, Bounds, Context,
    FocusHandle, Focusable, IntoElement, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels, Render, SharedString, Window,
};

use crate::automation::{
    discover_mixer_parameters, AutomationGraph, AutomationLane, AutomationLaneId, AutomationPoint,
    AutomationPointId, BeatFrameMap, BeatTime, BindingMode, FixedTempo, ParameterAddress,
    ParameterDescriptor, ParameterUnit, ProjectFrame, SegmentShape, TimeDomain, TimePosition,
    WriteMode, PPQ,
};
use crate::mixer::{BusId, BusKind, MixerError, MixerGraph, ProcessorId, SendId, SendTap};
#[allow(unused_imports)]
pub use control_actions::{
    AutomationAction, AutomationActionIntent, AutomationItemState, AutomationLaneControlDescriptor,
    AutomationSessionDescriptor, AutomationWriterCallback, AutomationWriterEffect,
    AutomationWriterIntent, AutomationWriterReceipt, AutomationWriterSession,
    AutomationWriterSnapshot, ControlAction, ControlActionCallback, ControlEdit,
    ControlHistoryIntent, ControlItemState, ControlItemTarget, ControlNumericError, ControlReceipt,
    ControlRenderStatus, ControlSessionAdapter, ControlSessionAdapterError,
    ControlSessionDescriptor, ControlSessionOperation, ControlSurface, CreatedControlIdentity,
    HistoryDirection, MeterValue, MixerAction, MixerActionIntent, MixerBusControlDescriptor,
    MixerBusRole, MixerInsertControlDescriptor, MixerItemState, MixerMeterSnapshot,
    MixerNumericTarget, MixerSendControlDescriptor, MixerSessionDescriptor,
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

/// Plugin inserts have no path to the audio: `MixerAction::RequestInsert` is
/// refused by the aggregate and `daw_render` bypasses every insert processor.
/// Insert affordances say so instead of claiming an effect.
const INSERT_CAPABILITY: &str = "plugin hosting not connected in this build";

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
    SendLevel(SendId),
    InsertWet(ProcessorId),
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
    series: u64,
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
            MixerControl::SendLevel(send) => MixerAction::SetSendLevel {
                send,
                level_db: self.preview,
            },
            MixerControl::InsertWet(processor) => MixerAction::SetInsertWet {
                processor,
                wet: self.preview,
            },
        };
        Some(
            MixerActionIntent::new(self.base_revision, action).with_edit(ControlEdit::Gesture {
                series: self.series,
            }),
        )
    }
}

/// Control edits a view has emitted and not yet seen answered, oldest first.
///
/// The controller answers one receipt per action, in the order the actions were
/// sent, so the oldest outstanding request is the one a receipt resolves.
#[derive(Debug, Default)]
struct PendingRequests {
    entries: BTreeMap<u64, String>,
    next_series: u64,
}

impl PendingRequests {
    /// A fresh identity for one edit. Gestures allocate at pointer-down and
    /// reuse the number as their [`ControlEdit::Gesture`] coalescing series.
    fn allocate_series(&mut self) -> u64 {
        self.next_series = self.next_series.wrapping_add(1).max(1);
        self.next_series
    }

    /// Record `label` as outstanding and return the status shown until the
    /// controller answers.
    fn record(&mut self, series: u64, label: &str) -> String {
        self.entries.insert(series, label.to_owned());
        format!("{label} · requested")
    }

    /// Replace the oldest outstanding request with the controller's answer. A
    /// receipt with nothing outstanding still surfaces a refusal: an edit this
    /// view did not send may still be the reason its snapshot did not move.
    fn resolve(&mut self, receipt: &ControlReceipt) -> Option<String> {
        let series = self.entries.keys().next().copied();
        let label = series.and_then(|series| self.entries.remove(&series));
        match receipt {
            ControlReceipt::Committed { revision, .. } => {
                let label = label?;
                Some(match revision {
                    Some(revision) => format!("{label} · committed at revision {revision}"),
                    None => format!("{label} · accepted with no project change"),
                })
            }
            ControlReceipt::Refused { reason, .. } => Some(match label {
                Some(label) => format!("{label} refused · {reason}"),
                None => format!("Control edit refused · {reason}"),
            }),
        }
    }
}

fn retain_selected_bus(selected: Option<BusId>, graph: &MixerGraph) -> Option<BusId> {
    selected
        .filter(|bus| graph.bus(*bus).is_some())
        .or_else(|| graph.buses().next().map(|bus| bus.id()))
}

fn retain_selected_lane(
    selected: Option<AutomationLaneId>,
    graph: &AutomationGraph,
) -> Option<AutomationLaneId> {
    selected
        .filter(|lane| graph.lane(*lane).is_some())
        .or_else(|| graph.lanes().next().map(|lane| lane.id))
}

pub struct MixerView {
    graph: MixerGraph,
    meter_readings: BTreeMap<BusId, MeterReading>,
    meter_sequence: u64,
    meter_source: Option<crate::render_products::PlaybackCohortId>,
    callback: ControlActionCallback,
    render_status: Option<ControlRenderStatus>,
    selected_bus: Option<BusId>,
    rename_draft: Option<(BusId, String)>,
    gesture: Option<MixerGesture>,
    pending: PendingRequests,
    status: String,
    focus_handle: FocusHandle,
}

impl MixerView {
    /// Render a controller-published snapshot and emit semantic actions. The
    /// view never mutates the graph: later truth arrives through
    /// [`Self::set_controller_snapshot`], answers through
    /// [`Self::apply_control_receipt`].
    pub fn from_controller_snapshot(
        graph: MixerGraph,
        target_bus: Option<BusId>,
        callback: ControlActionCallback,
        cx: &mut Context<Self>,
    ) -> Self {
        let selected_bus = retain_selected_bus(target_bus, &graph);
        Self {
            graph,
            meter_readings: BTreeMap::new(),
            meter_sequence: 0,
            meter_source: None,
            callback,
            render_status: None,
            selected_bus,
            rename_draft: None,
            gesture: None,
            pending: PendingRequests::default(),
            status: "Controller snapshot ready".into(),
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn set_meter_snapshot(&mut self, snapshot: MixerMeterSnapshot, cx: &mut Context<Self>) {
        // Meters show post-DSP truth or nothing: an unknown render status is
        // not evidence that these readings belong to the audible cohort.
        let audible_matches = self
            .render_status
            .as_ref()
            .is_some_and(|status| snapshot.is_audible_in(status));
        if !audible_matches
            || (self.meter_source.as_ref() == Some(&snapshot.audible)
                && snapshot.sequence < self.meter_sequence)
        {
            return;
        }
        let snapshot = snapshot.sanitized();
        self.meter_sequence = snapshot.sequence;
        self.meter_source = Some(snapshot.audible);
        self.meter_readings = snapshot.buses;
        cx.notify();
    }

    pub fn set_render_status(
        &mut self,
        status: Option<ControlRenderStatus>,
        cx: &mut Context<Self>,
    ) {
        if self.meter_source.as_ref() != status.as_ref().and_then(|status| status.active.as_ref()) {
            self.meter_source = None;
            self.meter_readings.clear();
        }
        self.render_status = status;
        cx.notify();
    }

    pub fn set_controller_snapshot(&mut self, graph: MixerGraph, cx: &mut Context<Self>) {
        let revision_changed = self.graph.revision() != graph.revision();
        self.graph = graph;
        self.reconcile_mixer_selection();
        if revision_changed {
            self.gesture = None;
        }
        cx.notify();
    }

    /// Replace a requested status with the controller's answer, and adopt the
    /// bus an accepted create allocated. Nothing is adopted before this point.
    pub fn apply_control_receipt(&mut self, receipt: &ControlReceipt, cx: &mut Context<Self>) {
        if receipt.surface() != ControlSurface::Mixer {
            return;
        }
        if let Some(CreatedControlIdentity::MixerBus(bus)) = receipt.created() {
            self.selected_bus = Some(bus);
            if self.rename_draft.as_ref().is_some_and(|(id, _)| *id != bus) {
                self.rename_draft = None;
            }
        }
        if let Some(status) = self.pending.resolve(receipt) {
            self.status = status;
        }
        cx.notify();
    }

    pub fn set_item_target(&mut self, target: ControlItemTarget, cx: &mut Context<Self>) -> bool {
        let ControlItemTarget::Mixer { bus } = target else {
            return false;
        };
        if bus.is_some_and(|bus| self.graph_snapshot().bus(bus).is_none()) {
            return false;
        }
        self.selected_bus = retain_selected_bus(bus, &self.graph_snapshot());
        cx.notify();
        true
    }

    /// Live selected bus after retain_selected_bus. Does not re-run retain on read.
    pub fn selected_bus(&self) -> Option<BusId> {
        self.selected_bus
    }

    pub fn item_state(&self) -> ControlItemState {
        ControlItemState::Mixer(MixerItemState {
            target_bus: self.selected_bus,
        })
    }

    pub fn graph_snapshot(&self) -> MixerGraph {
        self.graph.clone()
    }

    /// Commit a validated number-input value as one authoritative mixer edit.
    pub fn commit_exact_value(
        &mut self,
        target: MixerNumericTarget,
        value: f64,
        cx: &mut Context<Self>,
    ) -> Result<(), ControlNumericError> {
        let intent = MixerActionIntent::exact_value(&self.graph_snapshot(), target, value)?;
        self.dispatch_mixer(intent, cx);
        Ok(())
    }

    fn reconcile_mixer_selection(&mut self) {
        let graph = self.graph_snapshot();
        self.selected_bus = retain_selected_bus(self.selected_bus, &graph);
        if self
            .rename_draft
            .as_ref()
            .is_some_and(|(bus, _)| graph.bus(*bus).is_none())
        {
            self.rename_draft = None;
        }
    }

    /// Send one intent and show what was asked for. The status stays in the
    /// requested tense until [`Self::apply_control_receipt`] replaces it.
    fn dispatch_mixer(&mut self, intent: MixerActionIntent, cx: &mut Context<Self>) {
        let label = intent.action.label().to_owned();
        self.dispatch_mixer_labelled(intent, label, cx);
    }

    fn dispatch_mixer_labelled(
        &mut self,
        intent: MixerActionIntent,
        label: String,
        cx: &mut Context<Self>,
    ) {
        let series = match intent.edit {
            ControlEdit::Gesture { series } => series,
            ControlEdit::Discrete | ControlEdit::Numeric => self.pending.allocate_series(),
        };
        self.status = self.pending.record(series, &label);
        (self.callback)(ControlAction::Mixer(intent));
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
            )
            .with_edit(ControlEdit::Numeric),
            cx,
        );
    }

    fn adjust_pan(&mut self, bus: BusId, delta: f32, cx: &mut Context<Self>) {
        let graph = self.graph_snapshot();
        let current = graph.bus(bus).map(|bus| bus.fader().pan()).unwrap_or(0.0);
        let next = (current + delta).clamp(-1.0, 1.0);
        self.dispatch_mixer(
            MixerActionIntent::new(graph.revision(), MixerAction::SetPan { bus, pan: next })
                .with_edit(ControlEdit::Numeric),
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
            )
            .with_edit(ControlEdit::Numeric),
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
        // Prefer a typed return, then a group/master compatibility target.
        // Build each possibility on a
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
                BusKind::Return => 0,
                BusKind::Group => 1,
                BusKind::Master => 2,
                BusKind::Component => 3,
                BusKind::Source => 4,
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
            self.dispatch_mixer_labelled(
                intent,
                format!("Post-fader send → {target_name} at −18 dB"),
                cx,
            );
            return;
        }
        self.status = "No cycle-safe send destination is available".into();
        cx.notify();
    }

    fn add_routing_bus(&mut self, return_bus: bool, cx: &mut Context<Self>) {
        let graph = self.graph_snapshot();
        let ordinal = graph
            .buses()
            .filter(|bus| {
                bus.kind()
                    == if return_bus {
                        BusKind::Return
                    } else {
                        BusKind::Group
                    }
            })
            .count()
            .saturating_add(1);
        let role = if return_bus { "Return" } else { "Group" };
        let action = if return_bus {
            MixerAction::AddReturn {
                name: format!("{role} {ordinal}"),
            }
        } else {
            MixerAction::AddBus {
                kind: BusKind::Group,
                name: format!("{role} {ordinal}"),
            }
        };
        self.dispatch_mixer(MixerActionIntent::new(graph.revision(), action), cx);
    }

    fn add_channel(&mut self, cx: &mut Context<Self>) {
        let graph = self.graph_snapshot();
        let ordinal = graph
            .buses()
            .filter(|bus| matches!(bus.kind(), BusKind::Source | BusKind::Component))
            .count()
            .saturating_add(1);
        self.dispatch_mixer(
            MixerActionIntent::new(
                graph.revision(),
                MixerAction::AddBus {
                    kind: BusKind::Source,
                    name: format!("Channel {ordinal}"),
                },
            ),
            cx,
        );
    }

    /// Typed rename entry point for an inspector, context menu, or future
    /// inline text editor. Validation and history stay in the mixer command;
    /// the view never mutates a local name shadow.
    pub fn rename_bus(
        &mut self,
        bus: BusId,
        name: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> Result<(), MixerError> {
        let graph = self.graph_snapshot();
        let intent = MixerActionIntent::new(
            graph.revision(),
            MixerAction::RenameBus {
                bus,
                name: name.into(),
            },
        );
        intent.command(&graph)?;
        self.dispatch_mixer(intent, cx);
        Ok(())
    }

    fn begin_bus_rename(&mut self, bus: BusId, window: &mut Window, cx: &mut Context<Self>) {
        let graph = self.graph_snapshot();
        let Some(name) = graph.bus(bus).map(|bus| bus.name().to_owned()) else {
            return;
        };
        self.selected_bus = Some(bus);
        self.rename_draft = Some((bus, name));
        self.status = "Editing channel name · Enter commits · Escape cancels".into();
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn handle_mixer_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((bus, draft)) = self.rename_draft.as_mut() else {
            return;
        };
        if event.keystroke.modifiers.platform || event.keystroke.modifiers.control {
            return;
        }
        match event.keystroke.key.as_str() {
            "escape" => {
                self.rename_draft = None;
                self.status = "Channel rename cancelled".into();
            }
            "enter" => {
                let bus = *bus;
                let submitted = draft.trim().to_owned();
                match self.rename_bus(bus, submitted, cx) {
                    Ok(()) => self.rename_draft = None,
                    Err(error) => self.status = format!("Channel rename rejected · {error}"),
                }
            }
            "backspace" => {
                draft.pop();
            }
            _ => {
                let Some(text) = event.keystroke.key_char.as_deref() else {
                    return;
                };
                if text.chars().all(|character| !character.is_control())
                    && draft.chars().count() + text.chars().count() <= 64
                {
                    draft.push_str(text);
                } else {
                    return;
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn remove_bus(&mut self, bus: BusId, cx: &mut Context<Self>) {
        let graph = self.graph_snapshot();
        if bus == graph.master() {
            self.status = "Master is the terminal channel and cannot be removed".into();
            cx.notify();
            return;
        }
        self.dispatch_mixer(
            MixerActionIntent::new(graph.revision(), MixerAction::RemoveBus { bus }),
            cx,
        );
    }

    fn move_bus_left(&mut self, bus: BusId, cx: &mut Context<Self>) {
        let graph = self.graph_snapshot();
        let Some(position) = graph
            .bus_order()
            .iter()
            .position(|candidate| *candidate == bus)
        else {
            return;
        };
        let Some(before) = position
            .checked_sub(1)
            .map(|index| graph.bus_order()[index])
        else {
            self.status = "Channel is already first".into();
            cx.notify();
            return;
        };
        self.dispatch_mixer(
            MixerActionIntent::new(
                graph.revision(),
                MixerAction::MoveBusBefore {
                    bus,
                    before: Some(before),
                },
            ),
            cx,
        );
    }

    fn move_bus_right(&mut self, bus: BusId, cx: &mut Context<Self>) {
        let graph = self.graph_snapshot();
        let Some(position) = graph
            .bus_order()
            .iter()
            .position(|candidate| *candidate == bus)
        else {
            return;
        };
        let Some(next) = graph.bus_order().get(position + 1).copied() else {
            return;
        };
        if next == graph.master() {
            self.status = "Channel is already next to the pinned master".into();
            cx.notify();
            return;
        }
        // Moving the following identity before this one is the same exact
        // adjacent swap, expressed without a stale integer destination.
        self.dispatch_mixer(
            MixerActionIntent::new(
                graph.revision(),
                MixerAction::MoveBusBefore {
                    bus: next,
                    before: Some(bus),
                },
            ),
            cx,
        );
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
            MixerControl::SendLevel(send) => strip
                .sends()
                .iter()
                .find(|candidate| candidate.id() == send)
                .map(|send| send.level_db())
                .unwrap_or(-18.0),
            MixerControl::InsertWet(processor) => strip
                .inserts()
                .iter()
                .find(|slot| slot.processor_id() == processor)
                .map(|slot| slot.wet())
                .unwrap_or(1.0),
        };
        self.selected_bus = Some(bus);
        let series = self.pending.allocate_series();
        self.gesture = Some(MixerGesture {
            bus,
            control,
            base_revision: graph.revision(),
            origin_x: f32::from(event.position.x),
            origin_y: f32::from(event.position.y),
            original,
            preview: original,
            series,
        });
        self.status = match control {
            MixerControl::Gain => "Dragging fader · release to commit one undo step".into(),
            MixerControl::Pan => "Dragging pan · release to commit one undo step".into(),
            MixerControl::SendLevel(_) => "Dragging send · release to commit one undo step".into(),
            MixerControl::InsertWet(_) => {
                "Dragging insert mix · release to commit one undo step".into()
            }
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
            MixerControl::SendLevel(_) => (gesture.original
                + (f32::from(event.position.x) - gesture.origin_x) * 0.2)
                .clamp(-72.0, 12.0),
            MixerControl::InsertWet(_) => (gesture.original
                + (f32::from(event.position.x) - gesture.origin_x) / 120.0)
                .clamp(0.0, 1.0),
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

    /// The reference renderer bypasses every insert processor and no plugin
    /// worker is mapped into a strip, so an insert request has exactly one
    /// answer. The button reports it instead of sending a doomed intent.
    fn report_insert_capability(&mut self, cx: &mut Context<Self>) {
        self.status = format!("Insert not added · {INSERT_CAPABILITY}");
        cx.notify();
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
            )
            .with_edit(ControlEdit::Numeric),
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
                self.dispatch_mixer_labelled(intent, format!("Output → {name}"), cx);
                return;
            }
        }
        self.status = "No cycle-safe output target is available".into();
        cx.notify();
    }

    /// Both buttons run the project journal, which is the only history there
    /// is: the view keeps none of its own.
    fn history(&mut self, direction: HistoryDirection, cx: &mut Context<Self>) {
        let label = match direction {
            HistoryDirection::Undo => "Undo (project)",
            HistoryDirection::Redo => "Redo (project)",
        };
        let series = self.pending.allocate_series();
        self.status = self.pending.record(series, label);
        (self.callback)(ControlAction::History(ControlHistoryIntent {
            surface: ControlSurface::Mixer,
            expected_revision: self.graph.revision(),
            direction,
        }));
        cx.notify();
    }

    fn snapshots(&self) -> Vec<StripSnapshot> {
        let graph = self.graph_snapshot();
        let effective = graph.effective_states();
        graph
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
                                    gesture
                                        .filter(|gesture| {
                                            matches!(
                                                gesture.control,
                                                MixerControl::InsertWet(id)
                                                    if id == slot.processor_id()
                                            )
                                        })
                                        .map(|gesture| gesture.preview)
                                        .unwrap_or_else(|| slot.wet()),
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
                                gesture
                                    .filter(|gesture| {
                                        matches!(
                                            gesture.control,
                                            MixerControl::SendLevel(id) if id == send.id()
                                        )
                                    })
                                    .map(|gesture| gesture.preview)
                                    .unwrap_or_else(|| send.level_db()),
                                send.muted(),
                                send.tap(),
                            )
                        })
                        .collect(),
                    meter: self.meter_readings.get(&bus.id()).copied(),
                }
            })
            .collect()
    }

    fn render_strip(&self, strip: StripSnapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let bus = strip.id;
        let selected = self.selected_bus == Some(bus);
        let rename_draft = self
            .rename_draft
            .as_ref()
            .filter(|(candidate, _)| *candidate == bus)
            .map(|(_, draft)| draft.clone());
        let renaming = rename_draft.is_some();
        let name = rename_draft
            .map(|draft| format!("✎ {draft}_"))
            .unwrap_or(strip.name);
        let gain_fraction = gain_to_fader_fraction(strip.gain_db);
        let meter = strip.meter;
        let meter_fraction = meter
            .map(|value| db_to_meter_fraction(value.peak_db))
            .unwrap_or(0.0);
        let mut inserts = div().flex().flex_col().gap_1();
        if strip.inserts.is_empty() {
            inserts = inserts.child(insert_request_slot(bus, cx));
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
                                .child(if bypassed {
                                    "bypassed · not rendered"
                                } else {
                                    "active · not rendered"
                                })
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
                                        .child(
                                            div()
                                                .id(SharedString::from(format!(
                                                    "insert-wet-drag-{id}"
                                                )))
                                                .px_1()
                                                .cursor_ew_resize()
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                                        this.begin_mixer_gesture(
                                                            bus,
                                                            MixerControl::InsertWet(
                                                                ProcessorId::from_raw(id),
                                                            ),
                                                            event,
                                                            cx,
                                                        );
                                                        cx.stop_propagation();
                                                    }),
                                                )
                                                .child(format!("{:>3.0}%", wet * 100.0)),
                                        )
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
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .child(
                                            div()
                                                .id(SharedString::from(format!(
                                                    "send-level-drag-{id}"
                                                )))
                                                .px_1()
                                                .cursor_ew_resize()
                                                .text_xs()
                                                .text_color(rgb(CYAN))
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                                        this.begin_mixer_gesture(
                                                            bus,
                                                            MixerControl::SendLevel(
                                                                SendId::from_raw(id),
                                                            ),
                                                            event,
                                                            cx,
                                                        );
                                                        cx.stop_propagation();
                                                    }),
                                                )
                                                .child(format!("{level:.1} dB")),
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
                                                .child(tap_label),
                                        ),
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
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .id(SharedString::from(format!("mixer-rename-{}", bus.get())))
                                    .px_1()
                                    .rounded_sm()
                                    .cursor_text()
                                    .text_sm()
                                    .text_color(rgb(if renaming { CYAN } else { TEXT }))
                                    .when(renaming, |name| {
                                        name.border_1().border_color(rgb(CYAN)).bg(rgb(PANEL_ALT))
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.begin_bus_rename(bus, window, cx);
                                        cx.stop_propagation();
                                    }))
                                    .child(name),
                            )
                            .when(strip.kind != BusKind::Master, |header| {
                                header
                                    .child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "mixer-move-left-{}",
                                                bus.get()
                                            )))
                                            .px_1()
                                            .cursor_pointer()
                                            .text_xs()
                                            .text_color(rgb(DIM))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.move_bus_left(bus, cx);
                                                cx.stop_propagation();
                                            }))
                                            .child("←"),
                                    )
                                    .child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "mixer-move-right-{}",
                                                bus.get()
                                            )))
                                            .px_1()
                                            .cursor_pointer()
                                            .text_xs()
                                            .text_color(rgb(DIM))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.move_bus_right(bus, cx);
                                                cx.stop_propagation();
                                            }))
                                            .child("→"),
                                    )
                                    .child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "mixer-remove-{}",
                                                bus.get()
                                            )))
                                            .px_1()
                                            .cursor_pointer()
                                            .text_xs()
                                            .text_color(rgb(MAGENTA))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.remove_bus(bus, cx);
                                                cx.stop_propagation();
                                            }))
                                            .child("×"),
                                    )
                            }),
                    )
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
            .on_key_down(cx.listener(Self::handle_mixer_key_down))
            .on_action(
                cx.listener(|this, _: &ControlUndo, _, cx| {
                    this.history(HistoryDirection::Undo, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ControlRedo, _, cx| {
                    this.history(HistoryDirection::Redo, cx)
                }),
            )
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
                                header_button("mixer-add-channel", "+ Channel")
                                    .on_click(cx.listener(|this, _, _, cx| this.add_channel(cx))),
                            )
                            .child(header_button("mixer-add-group", "+ Group").on_click(
                                cx.listener(|this, _, _, cx| this.add_routing_bus(false, cx)),
                            ))
                            .child(header_button("mixer-add-return", "+ Return").on_click(
                                cx.listener(|this, _, _, cx| this.add_routing_bus(true, cx)),
                            ))
                            .child(header_button("mixer-undo", "Undo (project)").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.history(HistoryDirection::Undo, cx)
                                }),
                            ))
                            .child(header_button("mixer-redo", "Redo (project)").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.history(HistoryDirection::Redo, cx)
                                }),
                            )),
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
    series: u64,
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
        AutomationActionIntent::new(self.base_revision, action).with_edit(ControlEdit::Gesture {
            series: self.series,
        })
    }
}

pub struct AutomationView {
    graph: AutomationGraph,
    mixer_snapshot: Option<MixerGraph>,
    discovered_parameters: Vec<ParameterDescriptor>,
    callback: ControlActionCallback,
    writer_callback: Option<AutomationWriterCallback>,
    writer_snapshot: Option<AutomationWriterSnapshot>,
    render_status: Option<ControlRenderStatus>,
    selected_lane: Option<AutomationLaneId>,
    selected_point: Option<AutomationPointId>,
    curve_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    gesture: Option<AutomationGesture>,
    pending: PendingRequests,
    cursor_coordinate: i64,
    view_start: i64,
    view_end: i64,
    snap: AutomationSnap,
    status: String,
    focus_handle: FocusHandle,
}

impl AutomationView {
    /// Render a controller-published lane graph and emit semantic actions.
    /// A project with no lanes yet opens the same editor around an empty
    /// graph; mixer discovery gives `+ Lane` a real target.
    pub fn from_controller_snapshots_optional(
        graph: AutomationGraph,
        mixer: &MixerGraph,
        target_lane: Option<AutomationLaneId>,
        callback: ControlActionCallback,
        cx: &mut Context<Self>,
    ) -> Self {
        let selected_lane = retain_selected_lane(target_lane, &graph);
        Self {
            graph,
            mixer_snapshot: Some(mixer.clone()),
            discovered_parameters: discover_mixer_parameters(mixer),
            callback,
            writer_callback: None,
            writer_snapshot: None,
            render_status: None,
            selected_lane,
            selected_point: None,
            curve_bounds: Arc::new(Mutex::new(None)),
            gesture: None,
            pending: PendingRequests::default(),
            cursor_coordinate: 4 * PPQ,
            view_start: 0,
            view_end: 16 * PPQ,
            snap: AutomationSnap::Fine,
            status: "Controller snapshot ready".into(),
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn set_controller_snapshot(&mut self, graph: AutomationGraph, cx: &mut Context<Self>) {
        let revision_changed = self.graph.revision() != graph.revision();
        self.graph = graph;
        self.reconcile_automation_selection();
        if revision_changed {
            self.gesture = None;
        }
        cx.notify();
    }

    /// Replace a requested status with the controller's answer, and adopt the
    /// lane an accepted create allocated. Nothing is adopted before this point.
    pub fn apply_control_receipt(&mut self, receipt: &ControlReceipt, cx: &mut Context<Self>) {
        if receipt.surface() != ControlSurface::Automation {
            return;
        }
        if let Some(CreatedControlIdentity::AutomationLane(lane)) = receipt.created() {
            self.selected_lane = Some(lane);
            self.selected_point = None;
        }
        if let Some(status) = self.pending.resolve(receipt) {
            self.status = status;
        }
        cx.notify();
    }

    /// Refresh automatable mixer targets from the same project snapshot used
    /// by the aggregate action adapter. Discovery is presentation-only until
    /// lane creation commits the descriptor and lane together.
    pub fn set_mixer_snapshot(&mut self, mixer: &MixerGraph, cx: &mut Context<Self>) {
        self.mixer_snapshot = Some(mixer.clone());
        self.discovered_parameters = discover_mixer_parameters(mixer);
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

    /// Install the host's writer adapter. Without one the write-mode button is
    /// not rendered at all, rather than shown stuck on Read.
    pub fn set_writer_callback(&mut self, callback: Option<AutomationWriterCallback>) {
        self.writer_callback = callback;
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
        self.forget_writer_for_other_lane(lane);
        cx.notify();
        true
    }

    /// Live selected lane after retain_selected_lane. Does not re-run retain on read.
    pub fn selected_lane(&self) -> Option<AutomationLaneId> {
        self.selected_lane
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
        self.graph.clone()
    }

    /// Commit exact time/value fields for an existing selected point. The
    /// caller supplies typed project time, so beat ticks and frames are never
    /// conflated by text parsing in the view.
    pub fn commit_exact_point(
        &mut self,
        lane: AutomationLaneId,
        point: AutomationPointId,
        position: TimePosition,
        value: f64,
        cx: &mut Context<Self>,
    ) -> Result<(), ControlNumericError> {
        let intent = AutomationActionIntent::exact_point(
            &self.graph_snapshot(),
            lane,
            point,
            position,
            value,
        )?;
        self.dispatch_automation(intent, cx);
        Ok(())
    }

    fn reconcile_automation_selection(&mut self) {
        let graph = self.graph_snapshot();
        let next = retain_selected_lane(self.selected_lane, &graph);
        if next != self.selected_lane {
            self.selected_point = None;
        }
        self.selected_lane = next;
        if self
            .writer_snapshot
            .is_some_and(|writer| graph.lane(writer.lane).is_none())
        {
            self.writer_snapshot = None;
        }
    }

    /// The writer is bound to one lane. Leaving that lane drops the state the
    /// button reads, so it falls back to Read instead of reporting a mode that
    /// belongs to a lane nobody is looking at.
    fn forget_writer_for_other_lane(&mut self, lane: AutomationLaneId) {
        if self
            .writer_snapshot
            .is_some_and(|writer| writer.lane != lane)
        {
            self.writer_snapshot = None;
        }
    }

    fn lane_snapshot(&self, id: AutomationLaneId) -> Option<LaneSnapshot> {
        let graph = self.graph_snapshot();
        let lane = graph.lane(id)?;
        let descriptor = graph
            .descriptors()
            .find(|descriptor| descriptor.address == lane.target)
            .or_else(|| {
                self.discovered_parameters
                    .iter()
                    .find(|descriptor| descriptor.address == lane.target)
            })?
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

    /// Send one intent and show what was asked for. The status stays in the
    /// requested tense until [`Self::apply_control_receipt`] replaces it.
    fn dispatch_automation(&mut self, intent: AutomationActionIntent, cx: &mut Context<Self>) {
        let series = match intent.edit {
            ControlEdit::Gesture { series } => series,
            ControlEdit::Discrete | ControlEdit::Numeric => self.pending.allocate_series(),
        };
        let label = intent.action.label().to_owned();
        self.status = self.pending.record(series, &label);
        (self.callback)(ControlAction::Automation(intent));
        cx.notify();
    }

    fn select_lane(&mut self, lane: AutomationLaneId, cx: &mut Context<Self>) {
        self.selected_lane = Some(lane);
        self.selected_point = None;
        self.forget_writer_for_other_lane(lane);
        self.status = "Lane selected · click curve to add, drag points to move".into();
        cx.notify();
    }

    fn create_lane(&mut self, cx: &mut Context<Self>) {
        let graph = self.graph_snapshot();
        let selected_target = self
            .selected_lane
            .and_then(|lane| graph.lane(lane).map(|lane| lane.target.clone()));
        let mut available = graph.descriptors().cloned().collect::<Vec<_>>();
        for descriptor in &self.discovered_parameters {
            if !available
                .iter()
                .any(|registered| registered.address == descriptor.address)
            {
                available.push(descriptor.clone());
            }
        }
        let target = available
            .iter()
            .find(|descriptor| !graph.lanes().any(|lane| lane.target == descriptor.address))
            .map(|descriptor| descriptor.address.clone())
            .or(selected_target);
        let Some(target) = target else {
            self.status = "No automatable parameter is available in this project".into();
            cx.notify();
            return;
        };
        let descriptor = available
            .iter()
            .find(|descriptor| descriptor.address == target)
            .expect("selected target came from the descriptor registry");
        let ordinal = graph
            .lanes()
            .filter(|lane| lane.target == target)
            .count()
            .saturating_add(1);
        let intent = AutomationActionIntent::new(
            graph.revision(),
            AutomationAction::CreateLane {
                name: format!("{} {ordinal}", descriptor.name),
                target,
                domain: TimeDomain::Beats,
                binding: BindingMode::Replace,
            },
        );
        // The lane identity belongs to the controller. The view adopts it when
        // the receipt names it, never before.
        self.dispatch_automation(intent, cx);
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
        let mode = match self.write_mode() {
            WriteMode::Read => WriteMode::Touch,
            WriteMode::Touch => WriteMode::Latch,
            WriteMode::Latch => WriteMode::Write,
            WriteMode::Write => WriteMode::Read,
        };
        self.set_write_mode(mode, cx);
    }

    /// Live write policy, as the host's writer adapter last answered. The view
    /// holds no policy of its own, so the button cannot show a mode the
    /// adapter never granted.
    pub fn write_mode(&self) -> WriteMode {
        self.writer_snapshot
            .map(|writer| writer.mode)
            .unwrap_or(WriteMode::Read)
    }

    /// Hand one writer intent to the host adapter against the snapshot this
    /// view is rendering, and adopt the state it answers with.
    fn submit_writer_intent(
        &mut self,
        intent: AutomationWriterIntent,
    ) -> Result<AutomationWriterReceipt, String> {
        let callback = self
            .writer_callback
            .clone()
            .ok_or_else(|| "no session automation-writer adapter is attached".to_owned())?;
        let receipt = callback(&self.graph, intent)?;
        self.writer_snapshot = Some(receipt.snapshot);
        if receipt.submitted_edit {
            let series = self.pending.allocate_series();
            self.pending.record(series, "Writer point");
        }
        Ok(receipt)
    }

    /// Value the adapter binds a fresh writer at: the lane parameter's own
    /// default, which is in range by construction.
    fn writer_seed_value(&self, lane: AutomationLaneId) -> f64 {
        self.graph
            .lane(lane)
            .and_then(|lane| {
                self.graph
                    .descriptors()
                    .find(|descriptor| descriptor.address == lane.target)
            })
            .map(|descriptor| descriptor.default)
            .unwrap_or(0.0)
    }

    pub fn bind_writer(
        &mut self,
        mode: WriteMode,
        initial_value: f64,
        cx: &mut Context<Self>,
    ) -> Result<(), ControlNumericError> {
        let lane = self
            .selected_lane
            .ok_or(ControlNumericError::MissingTarget)?;
        let receipt = self
            .submit_writer_intent(AutomationWriterIntent::Bind {
                lane,
                mode,
                initial_value,
            })
            .map_err(ControlNumericError::InvalidEdit)?;
        self.status = writer_status(receipt.snapshot);
        cx.notify();
        Ok(())
    }

    /// Ask the adapter for a recording policy. The button never advances on
    /// its own: the mode it shows is the one the adapter granted.
    pub fn set_write_mode(&mut self, mode: WriteMode, cx: &mut Context<Self>) {
        let Some(lane) = self.selected_lane else {
            self.status = "Select a lane before changing writer mode".into();
            cx.notify();
            return;
        };
        let bound = self
            .writer_snapshot
            .is_some_and(|writer| writer.lane == lane);
        let intent = if bound {
            AutomationWriterIntent::SetMode { lane, mode }
        } else {
            AutomationWriterIntent::Bind {
                lane,
                mode,
                initial_value: self.writer_seed_value(lane),
            }
        };
        self.status = match self.submit_writer_intent(intent) {
            Ok(receipt) => writer_status(receipt.snapshot),
            Err(reason) => format!("{mode:?} mode refused · {reason}"),
        };
        cx.notify();
    }

    /// Offer one transport-timed sample to the writer. Any point it decides to
    /// write leaves through the same control-action path as a hand-drawn point.
    pub fn record_automation_value(
        &mut self,
        position: TimePosition,
        value: f64,
        cx: &mut Context<Self>,
    ) -> Result<(), ControlNumericError> {
        let lane = self
            .selected_lane
            .ok_or(ControlNumericError::MissingTarget)?;
        self.submit_writer_intent(AutomationWriterIntent::Event {
            lane,
            event: crate::automation::WriterEvent::ControlChanged { value },
        })
        .map_err(ControlNumericError::InvalidEdit)?;
        let receipt = self
            .submit_writer_intent(AutomationWriterIntent::Event {
                lane,
                event: crate::automation::WriterEvent::Tick { position },
            })
            .map_err(ControlNumericError::InvalidEdit)?;
        if !receipt.submitted_edit {
            self.status = writer_status(receipt.snapshot);
        }
        cx.notify();
        Ok(())
    }

    /// Forward a transport or touch boundary to the writer adapter.
    pub fn process_writer_event(
        &mut self,
        event: crate::automation::WriterEvent,
        cx: &mut Context<Self>,
    ) -> Result<(), ControlNumericError> {
        let lane = self
            .selected_lane
            .ok_or(ControlNumericError::MissingTarget)?;
        let receipt = self
            .submit_writer_intent(AutomationWriterIntent::Event { lane, event })
            .map_err(ControlNumericError::InvalidEdit)?;
        if !receipt.submitted_edit {
            self.status = writer_status(receipt.snapshot);
        }
        cx.notify();
        Ok(())
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
        let series = self.pending.allocate_series();
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
                series,
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
            series,
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
        // The controller allocates a new point's identity, so the preview id
        // this gesture drew with is not the durable one.
        let is_new = gesture.is_new;
        self.dispatch_automation(gesture.into_intent(), cx);
        if is_new {
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

    /// Both buttons run the project journal, which is the only history there
    /// is: the view keeps none of its own.
    fn history(&mut self, direction: HistoryDirection, cx: &mut Context<Self>) {
        let label = match direction {
            HistoryDirection::Undo => "Undo (project)",
            HistoryDirection::Redo => "Redo (project)",
        };
        let series = self.pending.allocate_series();
        self.status = self.pending.record(series, label);
        (self.callback)(ControlAction::History(ControlHistoryIntent {
            surface: ControlSurface::Automation,
            expected_revision: self.graph.revision(),
            direction,
        }));
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
            .on_action(
                cx.listener(|this, _: &ControlUndo, _, cx| {
                    this.history(HistoryDirection::Undo, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ControlRedo, _, cx| {
                    this.history(HistoryDirection::Redo, cx)
                }),
            )
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
                                header_button("automation-add-lane", "+ Lane")
                                    .on_click(cx.listener(|this, _, _, cx| this.create_lane(cx))),
                            )
                            .when(self.writer_callback.is_some(), |header| {
                                header.child(
                                    header_button(
                                        "automation-mode",
                                        &format!("{:?}", self.write_mode()),
                                    )
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.cycle_write_mode(cx)),
                                    ),
                                )
                            })
                            .child(
                                header_button("automation-snap", self.snap.label())
                                    .on_click(cx.listener(|this, _, _, cx| this.cycle_snap(cx))),
                            )
                            .child(header_button("automation-undo", "Undo (project)").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.history(HistoryDirection::Undo, cx)
                                }),
                            ))
                            .child(header_button("automation-redo", "Redo (project)").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.history(HistoryDirection::Redo, cx)
                                }),
                            )),
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

/// What the writer adapter granted, in the words the mode button uses.
fn writer_status(snapshot: AutomationWriterSnapshot) -> String {
    match snapshot.mode {
        WriteMode::Read => "READ · automation playback only".into(),
        WriteMode::Touch => "TOUCH · writes while the bound control is touched".into(),
        WriteMode::Latch => "LATCH · continues writing the last touched value".into(),
        WriteMode::Write => "WRITE · overwrites while transport advances".into(),
    }
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

fn insert_request_slot(bus: BusId, cx: &mut Context<MixerView>) -> impl IntoElement {
    div()
        .id(SharedString::from(format!("insert-request-{}", bus.get())))
        .px_2()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL_ALT))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| this.report_insert_capability(cx)))
        .child(div().text_xs().text_color(rgb(MUTED)).child("+ insert"))
        .child(
            div()
                .text_xs()
                .text_color(rgb(DIM))
                .child(INSERT_CAPABILITY),
        )
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
    use crate::automation::{MixerTarget, SmoothingPolicy, ValueMapping};

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
            series: 5,
        });

        let first = gesture.take().and_then(MixerGesture::into_intent);
        let second = gesture.take().and_then(MixerGesture::into_intent);
        assert_eq!(
            first,
            Some(
                MixerActionIntent::new(41, MixerAction::SetGainDb { bus, gain_db: -6.0 },)
                    .with_edit(ControlEdit::Gesture { series: 5 }),
            )
        );
        assert_eq!(second, None);
    }

    #[test]
    fn send_and_insert_gestures_keep_exact_target_identity() {
        let bus = BusId::from_raw(7);
        let send = SendId::from_raw(19);
        let processor = ProcessorId::from_raw(23);
        let send_intent = MixerGesture {
            bus,
            control: MixerControl::SendLevel(send),
            base_revision: 42,
            origin_x: 0.0,
            origin_y: 0.0,
            original: -18.0,
            preview: -7.5,
            series: 6,
        }
        .into_intent()
        .unwrap();
        assert_eq!(
            send_intent,
            MixerActionIntent::new(
                42,
                MixerAction::SetSendLevel {
                    send,
                    level_db: -7.5,
                },
            )
            .with_edit(ControlEdit::Gesture { series: 6 })
        );

        let wet_intent = MixerGesture {
            bus,
            control: MixerControl::InsertWet(processor),
            base_revision: 43,
            origin_x: 0.0,
            origin_y: 0.0,
            original: 1.0,
            preview: 0.625,
            series: 7,
        }
        .into_intent()
        .unwrap();
        assert!(matches!(
            wet_intent.action,
            MixerAction::SetInsertWet { processor: target, wet }
                if target == processor && wet == 0.625
        ));
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
            series: 8,
        });

        let first = gesture.take().map(AutomationGesture::into_intent);
        let second = gesture.take().map(AutomationGesture::into_intent);
        assert_eq!(
            first,
            Some(
                AutomationActionIntent::new(12, AutomationAction::MovePoint { lane, point },)
                    .with_edit(ControlEdit::Gesture { series: 8 }),
            )
        );
        assert_eq!(second, None);
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
    fn mixer_add_return_and_group_select_the_created_bus_across_publication() {
        let mut graph = MixerGraph::default();
        let source = graph.add_bus(BusKind::Source, "Voice").unwrap();
        let mut selected = Some(source);
        assert_eq!(selected, Some(source));

        let add_return = MixerActionIntent::new(
            graph.revision(),
            MixerAction::AddReturn {
                name: "Room".into(),
            },
        );
        let created_return = add_return.created_bus(&graph).unwrap().unwrap();
        assert_ne!(created_return, source);
        selected = Some(created_return);
        let command = add_return.command(&graph).unwrap();
        command.apply(&mut graph).unwrap();
        selected = retain_selected_bus(selected, &graph);
        assert_eq!(selected, Some(created_return));
        assert_eq!(graph.bus(created_return).unwrap().kind(), BusKind::Return);

        let add_group = MixerActionIntent::new(
            graph.revision(),
            MixerAction::AddBus {
                kind: BusKind::Group,
                name: "Music".into(),
            },
        );
        let created_group = add_group.created_bus(&graph).unwrap().unwrap();
        assert_ne!(created_group, created_return);
        selected = Some(created_group);
        let group_command = add_group.command(&graph).unwrap();
        group_command.apply(&mut graph).unwrap();
        selected = retain_selected_bus(selected, &graph);
        assert_eq!(selected, Some(created_group));
        assert_eq!(graph.bus(created_group).unwrap().kind(), BusKind::Group);

        group_command.revert(&mut graph).unwrap();
        selected = retain_selected_bus(selected, &graph);
        assert_ne!(selected, Some(created_group));
        assert!(graph.bus(created_group).is_none());
        assert_eq!(selected, Some(source));
    }

    #[test]
    fn refused_mixer_create_does_not_change_bus_selection() {
        let graph = MixerGraph::default();
        let selected = Some(graph.master());
        let empty = MixerActionIntent::new(
            graph.revision(),
            MixerAction::AddReturn { name: " ".into() },
        );
        assert!(empty.created_bus(&graph).is_err());
        assert_eq!(retain_selected_bus(selected, &graph), selected);

        let stale = MixerActionIntent::new(
            99,
            MixerAction::AddBus {
                kind: BusKind::Group,
                name: "Late".into(),
            },
        );
        assert!(stale.created_bus(&graph).is_err());
        assert_eq!(retain_selected_bus(selected, &graph), selected);
    }

    fn register_lane_parameter(graph: &mut AutomationGraph, address: ParameterAddress, name: &str) {
        graph
            .register_parameter(ParameterDescriptor {
                address,
                name: name.into(),
                unit: ParameterUnit::Decibels,
                minimum: -72.0,
                maximum: 12.0,
                default: 0.0,
                mapping: ValueMapping::Linear,
                smoothing: SmoothingPolicy::None,
            })
            .unwrap();
    }

    /// Headless stand-in for [`MixerView`] selection. GPUI `Context` is
    /// required to construct a real view; this makes the same assignments
    /// `from_controller_snapshot` and `apply_control_receipt` perform.
    struct MixerViewSelection {
        selected_bus: Option<BusId>,
        pending: PendingRequests,
    }

    impl MixerViewSelection {
        fn from_controller_snapshot(graph: &MixerGraph, target_bus: Option<BusId>) -> Self {
            Self {
                selected_bus: retain_selected_bus(target_bus, graph),
                pending: PendingRequests::default(),
            }
        }

        fn selected_bus(&self) -> Option<BusId> {
            self.selected_bus
        }

        fn request(&mut self, label: &str) -> String {
            let series = self.pending.allocate_series();
            self.pending.record(series, label)
        }

        fn apply_control_receipt(&mut self, receipt: &ControlReceipt) -> Option<String> {
            if receipt.surface() != ControlSurface::Mixer {
                return None;
            }
            if let Some(CreatedControlIdentity::MixerBus(bus)) = receipt.created() {
                self.selected_bus = Some(bus);
            }
            self.pending.resolve(receipt)
        }

        fn reconcile(&mut self, graph: &MixerGraph) {
            self.selected_bus = retain_selected_bus(self.selected_bus, graph);
        }
    }

    /// Headless stand-in for [`AutomationView`] selection, with the same
    /// receipt-driven adoption.
    struct AutomationViewSelection {
        selected_lane: Option<AutomationLaneId>,
        pending: PendingRequests,
    }

    impl AutomationViewSelection {
        fn from_controller_snapshot(
            graph: &AutomationGraph,
            target_lane: AutomationLaneId,
        ) -> Self {
            Self {
                selected_lane: retain_selected_lane(Some(target_lane), graph),
                pending: PendingRequests::default(),
            }
        }

        fn selected_lane(&self) -> Option<AutomationLaneId> {
            self.selected_lane
        }

        fn request(&mut self, label: &str) -> String {
            let series = self.pending.allocate_series();
            self.pending.record(series, label)
        }

        fn apply_control_receipt(&mut self, receipt: &ControlReceipt) -> Option<String> {
            if receipt.surface() != ControlSurface::Automation {
                return None;
            }
            if let Some(CreatedControlIdentity::AutomationLane(lane)) = receipt.created() {
                self.selected_lane = Some(lane);
            }
            self.pending.resolve(receipt)
        }

        fn reconcile(&mut self, graph: &AutomationGraph) {
            self.selected_lane = retain_selected_lane(self.selected_lane, graph);
        }
    }

    fn committed(surface: ControlSurface, revision: u64) -> ControlReceipt {
        ControlReceipt::Committed {
            surface,
            revision: Some(revision),
            created: None,
        }
    }

    #[test]
    fn a_request_reads_as_requested_until_its_receipt_arrives() {
        let mut pending = PendingRequests::default();
        let series = pending.allocate_series();
        assert_eq!(
            pending.record(series, "add mixer bus"),
            "add mixer bus · requested"
        );
        assert_eq!(
            pending.resolve(&committed(ControlSurface::Mixer, 12)),
            Some("add mixer bus · committed at revision 12".into())
        );
        assert!(pending.entries.is_empty());
    }

    #[test]
    fn a_refusal_replaces_the_request_verbatim() {
        let mut pending = PendingRequests::default();
        let series = pending.allocate_series();
        pending.record(series, "add send");
        let refused = ControlReceipt::Refused {
            surface: ControlSurface::Mixer,
            reason: "bus 4 no longer exists".into(),
        };
        assert_eq!(
            pending.resolve(&refused),
            Some("add send refused · bus 4 no longer exists".into())
        );
    }

    #[test]
    fn receipts_answer_outstanding_requests_oldest_first() {
        let mut pending = PendingRequests::default();
        for label in ["add mixer bus", "add send", "Undo (project)"] {
            let series = pending.allocate_series();
            pending.record(series, label);
        }
        assert_eq!(
            pending.resolve(&committed(ControlSurface::Mixer, 3)),
            Some("add mixer bus · committed at revision 3".into())
        );
        assert_eq!(
            pending.resolve(&committed(ControlSurface::Mixer, 4)),
            Some("add send · committed at revision 4".into())
        );
        assert_eq!(
            pending.resolve(&committed(ControlSurface::Mixer, 5)),
            Some("Undo (project) · committed at revision 5".into())
        );
        assert!(pending.entries.is_empty());
    }

    #[test]
    fn a_refusal_with_nothing_outstanding_is_still_shown() {
        let mut pending = PendingRequests::default();
        assert_eq!(pending.resolve(&committed(ControlSurface::Mixer, 9)), None);
        let refused = ControlReceipt::Refused {
            surface: ControlSurface::Automation,
            reason: "project is not open".into(),
        };
        assert_eq!(
            pending.resolve(&refused),
            Some("Control edit refused · project is not open".into())
        );
    }

    #[test]
    fn a_receipt_that_changed_nothing_does_not_claim_a_revision() {
        let mut pending = PendingRequests::default();
        let series = pending.allocate_series();
        pending.record(series, "toggle channel mute");
        assert_eq!(
            pending.resolve(&ControlReceipt::Committed {
                surface: ControlSurface::Mixer,
                revision: None,
                created: None,
            }),
            Some("toggle channel mute · accepted with no project change".into())
        );
    }

    #[test]
    fn insert_affordances_name_the_missing_plugin_host() {
        assert_eq!(
            INSERT_CAPABILITY,
            "plugin hosting not connected in this build"
        );
    }

    #[test]
    fn mixer_from_controller_snapshot_selected_bus_is_the_target() {
        let mut graph = MixerGraph::default();
        let voice = graph.add_bus(BusKind::Source, "Voice").unwrap();
        let group = graph.add_bus(BusKind::Group, "Music").unwrap();
        let view = MixerViewSelection::from_controller_snapshot(&graph, Some(group));
        assert_eq!(view.selected_bus(), Some(group));
        assert_ne!(view.selected_bus(), Some(voice));
        assert_eq!(
            MixerViewSelection::from_controller_snapshot(&graph, Some(voice)).selected_bus(),
            Some(voice)
        );
    }

    #[test]
    fn request_insert_intent_from_controller_snapshot_graph_is_refused() {
        let mut graph = MixerGraph::default();
        let source = graph.add_bus(BusKind::Source, "Voice").unwrap();
        let _view = MixerViewSelection::from_controller_snapshot(&graph, Some(source));
        let intent =
            MixerActionIntent::new(graph.revision(), MixerAction::RequestInsert { bus: source });
        assert!(matches!(
            intent.command(&graph),
            Err(MixerError::PluginHostNotConnected)
        ));
        assert_eq!(graph.buses().flat_map(|bus| bus.inserts()).count(), 0);
    }

    #[test]
    fn mixer_create_adopts_the_bus_only_when_the_receipt_names_it() {
        let mut graph = MixerGraph::default();
        let voice = graph.add_bus(BusKind::Source, "Voice").unwrap();
        let mut view = MixerViewSelection::from_controller_snapshot(&graph, Some(voice));
        assert_eq!(view.selected_bus(), Some(voice));

        let add_return = MixerActionIntent::new(
            graph.revision(),
            MixerAction::AddReturn {
                name: "Room".into(),
            },
        );
        assert_eq!(view.request("add return bus"), "add return bus · requested");
        assert_eq!(
            view.selected_bus(),
            Some(voice),
            "a requested create must not move the selection"
        );

        let command = add_return.command(&graph).unwrap();
        command.apply(&mut graph).unwrap();
        let created = graph
            .buses()
            .find(|bus| bus.kind() == BusKind::Return)
            .unwrap()
            .id();
        let status = view.apply_control_receipt(&ControlReceipt::Committed {
            surface: ControlSurface::Mixer,
            revision: Some(graph.revision()),
            created: Some(CreatedControlIdentity::MixerBus(created)),
        });
        assert_eq!(view.selected_bus(), Some(created));
        assert_eq!(
            status,
            Some(format!(
                "add return bus · committed at revision {}",
                graph.revision()
            ))
        );
        view.reconcile(&graph);
        assert_eq!(view.selected_bus(), Some(created));
    }

    #[test]
    fn a_refused_mixer_create_leaves_the_selection_where_it_was() {
        let mut graph = MixerGraph::default();
        let voice = graph.add_bus(BusKind::Source, "Voice").unwrap();
        let mut view = MixerViewSelection::from_controller_snapshot(&graph, Some(voice));
        view.request("add return bus");
        let status = view.apply_control_receipt(&ControlReceipt::Refused {
            surface: ControlSurface::Mixer,
            reason: "bus name is empty".into(),
        });
        assert_eq!(view.selected_bus(), Some(voice));
        assert_eq!(
            status,
            Some("add return bus refused · bus name is empty".into())
        );
    }

    #[test]
    fn automation_from_controller_snapshot_selected_lane_is_the_target() {
        let mut graph = AutomationGraph::new();
        let gain = ParameterAddress::Mixer(MixerTarget::BusGain(2));
        let pan = ParameterAddress::Mixer(MixerTarget::BusPan(2));
        register_lane_parameter(&mut graph, gain.clone(), "Gain");
        register_lane_parameter(&mut graph, pan.clone(), "Pan");
        let existing = graph.create_lane("Gain", gain, TimeDomain::Beats).unwrap();
        let extra = graph.create_lane("Pan", pan, TimeDomain::Beats).unwrap();
        let view = AutomationViewSelection::from_controller_snapshot(&graph, extra);
        assert_eq!(view.selected_lane(), Some(extra));
        assert_ne!(view.selected_lane(), Some(existing));
        assert_eq!(
            AutomationViewSelection::from_controller_snapshot(&graph, existing).selected_lane(),
            Some(existing)
        );
    }

    #[test]
    fn automation_create_adopts_the_lane_only_when_the_receipt_names_it() {
        let mut graph = AutomationGraph::new();
        let gain = ParameterAddress::Mixer(MixerTarget::BusGain(2));
        let pan = ParameterAddress::Mixer(MixerTarget::BusPan(2));
        register_lane_parameter(&mut graph, gain.clone(), "Gain");
        register_lane_parameter(&mut graph, pan.clone(), "Pan");
        let existing = graph.create_lane("Gain", gain, TimeDomain::Beats).unwrap();
        let mut view = AutomationViewSelection::from_controller_snapshot(&graph, existing);

        assert_eq!(
            view.request("create automation lane"),
            "create automation lane · requested"
        );
        assert_eq!(
            view.selected_lane(),
            Some(existing),
            "a requested create must not move the selection"
        );

        let created = graph.create_lane("Pan 1", pan, TimeDomain::Beats).unwrap();
        view.apply_control_receipt(&ControlReceipt::Committed {
            surface: ControlSurface::Automation,
            revision: Some(graph.revision()),
            created: Some(CreatedControlIdentity::AutomationLane(created)),
        });
        assert_eq!(view.selected_lane(), Some(created));
        view.reconcile(&graph);
        assert_eq!(view.selected_lane(), Some(created));
    }

    #[test]
    fn a_mixer_receipt_never_answers_the_automation_editor() {
        let mut graph = AutomationGraph::new();
        let gain = ParameterAddress::Mixer(MixerTarget::BusGain(2));
        register_lane_parameter(&mut graph, gain.clone(), "Gain");
        let existing = graph.create_lane("Gain", gain, TimeDomain::Beats).unwrap();
        let mut view = AutomationViewSelection::from_controller_snapshot(&graph, existing);
        view.request("add point");
        assert_eq!(
            view.apply_control_receipt(&committed(ControlSurface::Mixer, 7)),
            None
        );
        assert_eq!(
            view.apply_control_receipt(&committed(ControlSurface::Automation, 8)),
            Some("add point · committed at revision 8".into())
        );
    }
}
