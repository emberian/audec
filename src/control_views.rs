//! DAW control surfaces for Audec's mixer and automation engines.
//!
//! These views deliberately edit the real backend-independent graphs instead
//! of maintaining cosmetic UI-only values.  They can be constructed around
//! local state today, or around an injected backend when the project/session
//! controller becomes the owner of history.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use gpui::{
    actions, canvas, div, point, prelude::*, px, quad, relative, rgb, rgba, App, Bounds, Context,
    FocusHandle, Focusable, IntoElement, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PathBuilder, Pixels, Render, SharedString, Window,
};

use crate::automation::{
    AutomationCommand, AutomationGraph, AutomationHistory, AutomationLane, AutomationLaneId,
    AutomationPoint, AutomationPointId, BeatFrameMap, BeatTime, BindingMode, FixedTempo,
    MixerTarget, ParameterAddress, ParameterDescriptor, ParameterUnit, ProjectFrame, SegmentShape,
    SmoothingPolicy, TimeDomain, TimePosition, ValueMapping, WriteMode, PPQ,
};
use crate::mixer::{
    BusId, BusKind, MixerCommand, MixerError, MixerGraph, PluginDescriptor, SendTap,
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
    fn graph(&self) -> &MixerGraph;
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
    fn graph(&self) -> &MixerGraph {
        &self.graph
    }

    fn execute(&mut self, command: MixerCommand) -> Result<(), MixerError> {
        command.apply(&mut self.graph)?;
        self.redo.clear();
        if self.limit > 0 {
            self.undo.push_back(command);
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
        command.revert(&mut self.graph)?;
        self.redo.push(command);
        Ok(true)
    }

    fn redo(&mut self) -> Result<bool, MixerError> {
        let Some(command) = self.redo.pop() else {
            return Ok(false);
        };
        command.apply(&mut self.graph)?;
        self.undo.push_back(command);
        Ok(true)
    }
}

/// Mixer history that mirrors each successful edit into controller-owned state.
///
/// The view keeps `local` so rendering can borrow a graph synchronously. The
/// shared graph is locked before the local history changes, then replaced with
/// the resulting clone before the edit reports success. Recovering a poisoned
/// lock preserves the latest controller state while still allowing the UI to
/// continue publishing edits.
pub struct SharedMixerBackend {
    local: LocalMixerBackend,
    shared_graph: Arc<Mutex<MixerGraph>>,
}

impl SharedMixerBackend {
    pub fn new(shared_graph: Arc<Mutex<MixerGraph>>, history_limit: usize) -> Self {
        let graph = shared_graph
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        Self {
            local: LocalMixerBackend::new(graph, history_limit),
            shared_graph,
        }
    }
}

impl MixerBackend for SharedMixerBackend {
    fn graph(&self) -> &MixerGraph {
        self.local.graph()
    }

    fn execute(&mut self, command: MixerCommand) -> Result<(), MixerError> {
        let mut shared_graph = self
            .shared_graph
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.local.execute(command)?;
        *shared_graph = self.local.graph().clone();
        Ok(())
    }

    fn undo(&mut self) -> Result<bool, MixerError> {
        let mut shared_graph = self
            .shared_graph
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = self.local.undo()?;
        *shared_graph = self.local.graph().clone();
        Ok(changed)
    }

    fn redo(&mut self) -> Result<bool, MixerError> {
        let mut shared_graph = self
            .shared_graph
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = self.local.redo()?;
        *shared_graph = self.local.graph().clone();
        Ok(changed)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeterReading {
    pub peak_db: f32,
    pub rms_db: f32,
}

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
    inserts: Vec<(u64, String, bool, f32)>,
    sends: Vec<(u64, String, f32, bool, SendTap)>,
    meter: Option<MeterReading>,
}

pub struct MixerView {
    backend: Box<dyn MixerBackend>,
    meter_readings: BTreeMap<BusId, MeterReading>,
    selected_bus: Option<BusId>,
    status: String,
    focus_handle: FocusHandle,
}

impl MixerView {
    pub fn demo(cx: &mut Context<Self>) -> Self {
        Self::with_backend(Box::new(LocalMixerBackend::new(demo_mixer(), 128)), cx)
    }

    pub fn from_graph(graph: MixerGraph, cx: &mut Context<Self>) -> Self {
        Self::with_backend(Box::new(LocalMixerBackend::new(graph, 128)), cx)
    }

    /// Construct a mixer view backed by graph state owned by a controller.
    pub fn from_shared_graph(shared_graph: Arc<Mutex<MixerGraph>>, cx: &mut Context<Self>) -> Self {
        Self::with_backend(Box::new(SharedMixerBackend::new(shared_graph, 128)), cx)
    }

    pub fn with_backend(backend: Box<dyn MixerBackend>, cx: &mut Context<Self>) -> Self {
        let selected_bus = backend.graph().buses().next().map(|bus| bus.id());
        Self {
            backend,
            meter_readings: BTreeMap::new(),
            selected_bus,
            status: "Ready · graph edits are undoable".into(),
            focus_handle: cx.focus_handle(),
        }
    }

    /// Supply genuine post-DSP meter values. No synthetic activity is shown
    /// while a realtime engine has not connected a meter tap.
    pub fn set_meter_reading(&mut self, bus: BusId, reading: Option<MeterReading>) {
        if let Some(reading) = reading {
            self.meter_readings.insert(bus, reading);
        } else {
            self.meter_readings.remove(&bus);
        }
    }

    pub fn graph(&self) -> &MixerGraph {
        self.backend.graph()
    }

    fn execute<F>(&mut self, label: &str, edit: F, cx: &mut Context<Self>)
    where
        F: FnOnce(&mut MixerGraph) -> Result<(), MixerError>,
    {
        let result = MixerCommand::build(label, self.backend.graph(), edit)
            .and_then(|command| self.backend.execute(command));
        self.status = match result {
            Ok(()) => format!("{label} · saved to mixer history"),
            Err(error) => format!("Could not {label}: {error}"),
        };
        cx.notify();
    }

    fn adjust_gain(&mut self, bus: BusId, delta: f32, cx: &mut Context<Self>) {
        let current = self
            .backend
            .graph()
            .bus(bus)
            .map(|bus| bus.fader().gain_db())
            .unwrap_or(0.0);
        let next = (current + delta).clamp(-72.0, 12.0);
        self.execute(
            "change channel gain",
            move |graph| graph.set_gain_db(bus, next),
            cx,
        );
    }

    fn adjust_pan(&mut self, bus: BusId, delta: f32, cx: &mut Context<Self>) {
        let current = self
            .backend
            .graph()
            .bus(bus)
            .map(|bus| bus.fader().pan())
            .unwrap_or(0.0);
        let next = (current + delta).clamp(-1.0, 1.0);
        self.execute(
            "change channel pan",
            move |graph| graph.set_pan(bus, next),
            cx,
        );
    }

    fn toggle_mute(&mut self, bus: BusId, cx: &mut Context<Self>) {
        let next = !self.backend.graph().bus(bus).unwrap().fader().muted();
        self.execute(
            "toggle channel mute",
            move |graph| graph.set_muted(bus, next),
            cx,
        );
    }

    fn toggle_solo(&mut self, bus: BusId, cx: &mut Context<Self>) {
        let next = !self.backend.graph().bus(bus).unwrap().fader().soloed();
        self.execute(
            "toggle channel solo",
            move |graph| graph.set_soloed(bus, next),
            cx,
        );
    }

    fn adjust_send(&mut self, send_raw: u64, delta: f32, cx: &mut Context<Self>) {
        let send_id = crate::mixer::SendId::from_raw(send_raw);
        let current = self
            .backend
            .graph()
            .buses()
            .flat_map(|bus| bus.sends())
            .find(|send| send.id() == send_id)
            .map(|send| send.level_db())
            .unwrap_or(-18.0);
        self.execute(
            "change send level",
            move |graph| graph.set_send_level(send_id, (current + delta).clamp(-72.0, 12.0)),
            cx,
        );
    }

    fn toggle_send(&mut self, send_raw: u64, cx: &mut Context<Self>) {
        let send_id = crate::mixer::SendId::from_raw(send_raw);
        let next = !self
            .backend
            .graph()
            .buses()
            .flat_map(|bus| bus.sends())
            .find(|send| send.id() == send_id)
            .map(|send| send.muted())
            .unwrap_or(false);
        self.execute(
            "toggle send mute",
            move |graph| graph.set_send_muted(send_id, next),
            cx,
        );
    }

    fn toggle_insert(&mut self, processor_raw: u64, cx: &mut Context<Self>) {
        let id = crate::mixer::ProcessorId::from_raw(processor_raw);
        let next = self
            .backend
            .graph()
            .buses()
            .flat_map(|bus| bus.inserts())
            .find(|slot| slot.processor_id() == id)
            .map(|slot| !slot.bypassed())
            .unwrap_or(true);
        self.execute(
            "toggle insert bypass",
            move |graph| graph.set_insert_bypassed(id, next),
            cx,
        );
    }

    fn adjust_insert_wet(&mut self, processor_raw: u64, delta: f32, cx: &mut Context<Self>) {
        let id = crate::mixer::ProcessorId::from_raw(processor_raw);
        let current = self
            .backend
            .graph()
            .buses()
            .flat_map(|bus| bus.inserts())
            .find(|slot| slot.processor_id() == id)
            .map(|slot| slot.wet())
            .unwrap_or(1.0);
        self.execute(
            "change insert mix",
            move |graph| graph.set_insert_wet(id, (current + delta).clamp(0.0, 1.0)),
            cx,
        );
    }

    fn cycle_output(&mut self, bus: BusId, cx: &mut Context<Self>) {
        if bus == self.backend.graph().master() {
            self.status = "Master is the terminal output".into();
            cx.notify();
            return;
        }
        let candidates: Vec<_> = self.backend.graph().buses().map(|bus| bus.id()).collect();
        let current = self.backend.graph().bus(bus).and_then(|bus| bus.output());
        let start = current
            .and_then(|id| candidates.iter().position(|candidate| *candidate == id))
            .unwrap_or(0);
        for offset in 1..=candidates.len() {
            let target = candidates[(start + offset) % candidates.len()];
            if target == bus {
                continue;
            }
            if let Ok(command) =
                MixerCommand::build("change channel output", self.backend.graph(), |graph| {
                    graph.set_output(bus, target)
                })
            {
                let name = self.backend.graph().bus(target).unwrap().name().to_owned();
                match self.backend.execute(command) {
                    Ok(()) => self.status = format!("Output → {name} · saved to mixer history"),
                    Err(error) => self.status = format!("Could not route output: {error}"),
                }
                cx.notify();
                return;
            }
        }
        self.status = "No cycle-safe output target is available".into();
        cx.notify();
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        self.status = match self.backend.undo() {
            Ok(true) => "Undid mixer edit".into(),
            Ok(false) => "Mixer history is already at its beginning".into(),
            Err(error) => format!("Undo failed: {error}"),
        };
        cx.notify();
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        self.status = match self.backend.redo() {
            Ok(true) => "Redid mixer edit".into(),
            Ok(false) => "Nothing to redo".into(),
            Err(error) => format!("Redo failed: {error}"),
        };
        cx.notify();
    }

    fn snapshots(&self) -> Vec<StripSnapshot> {
        self.backend
            .graph()
            .buses()
            .map(|bus| StripSnapshot {
                id: bus.id(),
                name: bus.name().to_owned(),
                kind: bus.kind(),
                output: bus.output().and_then(|id| {
                    self.backend
                        .graph()
                        .bus(id)
                        .map(|target| (id, target.name().to_owned()))
                }),
                gain_db: bus.fader().gain_db(),
                pan: bus.fader().pan(),
                muted: bus.fader().muted(),
                soloed: bus.fader().soloed(),
                inserts: bus
                    .inserts()
                    .iter()
                    .filter_map(|slot| {
                        self.backend
                            .graph()
                            .processor(slot.processor_id())
                            .map(|processor| {
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
                            self.backend
                                .graph()
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
            })
            .collect()
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
            sends = sends.child(empty_slot("no sends", "Route graph ready"));
        } else {
            for (id, target, level, muted, tap) in strip.sends.clone() {
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
                                        .id(SharedString::from(format!("send-mute-{id}")))
                                        .cursor_pointer()
                                        .text_xs()
                                        .text_color(rgb(if muted { MAGENTA } else { MUTED }))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.toggle_send(id, cx)
                                        }))
                                        .child(if muted { "OFF" } else { "ON" }),
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
                                        .text_xs()
                                        .text_color(rgb(MUTED))
                                        .child(format!("{level:.1} dB · {tap:?}")),
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
            .bg(rgb(if selected { 0x111b24 } else { PANEL }))
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
                            .text_xs()
                            .text_color(rgb(DIM))
                            .child(format!("{:?}", strip.kind).to_uppercase()),
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
                    .child(vertical_fader(gain_fraction, strip.gain_db)),
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
                            .text_xs()
                            .text_color(rgb(MUTED))
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
        let mut bank = div()
            .id("mixer-strip-bank")
            .h_full()
            .flex()
            .overflow_x_scroll();
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
                        div().child(div().text_sm().child("MIXER / ROUTING")).child(
                            div()
                                .text_xs()
                                .text_color(rgb(DIM))
                                .child("Non-destructive graph · latency-aware backend"),
                        ),
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
    fn graph(&self) -> &AutomationGraph;
    fn execute(&mut self, command: AutomationCommand) -> Result<(), String>;
    fn undo(&mut self) -> Result<bool, String>;
    fn redo(&mut self) -> Result<bool, String>;
}

pub struct LocalAutomationBackend {
    history: AutomationHistory,
}

impl LocalAutomationBackend {
    pub fn new(graph: AutomationGraph, history_limit: usize) -> Self {
        Self {
            history: AutomationHistory::new(graph, history_limit),
        }
    }
}

impl AutomationBackend for LocalAutomationBackend {
    fn graph(&self) -> &AutomationGraph {
        self.history.graph()
    }

    fn execute(&mut self, command: AutomationCommand) -> Result<(), String> {
        self.history
            .execute(command)
            .map_err(|error| error.to_string())
    }

    fn undo(&mut self) -> Result<bool, String> {
        self.history.undo().map_err(|error| error.to_string())
    }

    fn redo(&mut self) -> Result<bool, String> {
        self.history.redo().map_err(|error| error.to_string())
    }
}

/// Automation history that mirrors each successful edit into controller-owned
/// state while retaining a locally borrowable graph for rendering.
pub struct SharedAutomationBackend {
    local: LocalAutomationBackend,
    shared_graph: Arc<Mutex<AutomationGraph>>,
}

impl SharedAutomationBackend {
    pub fn new(shared_graph: Arc<Mutex<AutomationGraph>>, history_limit: usize) -> Self {
        let graph = shared_graph
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        Self {
            local: LocalAutomationBackend::new(graph, history_limit),
            shared_graph,
        }
    }
}

impl AutomationBackend for SharedAutomationBackend {
    fn graph(&self) -> &AutomationGraph {
        self.local.graph()
    }

    fn execute(&mut self, command: AutomationCommand) -> Result<(), String> {
        let mut shared_graph = self
            .shared_graph
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.local.execute(command)?;
        *shared_graph = self.local.graph().clone();
        Ok(())
    }

    fn undo(&mut self) -> Result<bool, String> {
        let mut shared_graph = self
            .shared_graph
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = self.local.undo()?;
        *shared_graph = self.local.graph().clone();
        Ok(changed)
    }

    fn redo(&mut self) -> Result<bool, String> {
        let mut shared_graph = self
            .shared_graph
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = self.local.redo()?;
        *shared_graph = self.local.graph().clone();
        Ok(changed)
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
    points: Vec<AutomationPoint>,
    descriptor: ParameterDescriptor,
}

pub struct AutomationView {
    backend: Box<dyn AutomationBackend>,
    selected_lane: Option<AutomationLaneId>,
    selected_point: Option<AutomationPointId>,
    curve_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    dragging: bool,
    cursor_coordinate: i64,
    view_start: i64,
    view_end: i64,
    write_mode: WriteMode,
    status: String,
    next_ui_point_id: u64,
    focus_handle: FocusHandle,
}

impl AutomationView {
    pub fn demo(cx: &mut Context<Self>) -> Self {
        Self::with_backend(
            Box::new(LocalAutomationBackend::new(demo_automation(), 256)),
            cx,
        )
    }

    pub fn from_graph(graph: AutomationGraph, cx: &mut Context<Self>) -> Self {
        Self::with_backend(Box::new(LocalAutomationBackend::new(graph, 256)), cx)
    }

    /// Construct an automation view backed by graph state owned by a controller.
    pub fn from_shared_graph(
        shared_graph: Arc<Mutex<AutomationGraph>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_backend(
            Box::new(SharedAutomationBackend::new(shared_graph, 256)),
            cx,
        )
    }

    pub fn with_backend(backend: Box<dyn AutomationBackend>, cx: &mut Context<Self>) -> Self {
        let selected_lane = backend.graph().lanes().next().map(|lane| lane.id);
        let next_ui_point_id = backend
            .graph()
            .lanes()
            .flat_map(|lane| lane.points())
            .map(|point| point.id.get())
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        Self {
            backend,
            selected_lane,
            selected_point: None,
            curve_bounds: Arc::new(Mutex::new(None)),
            dragging: false,
            cursor_coordinate: 4 * PPQ,
            view_start: 0,
            view_end: 16 * PPQ,
            write_mode: WriteMode::Read,
            status: "READ · curve edits are undoable".into(),
            next_ui_point_id,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn graph(&self) -> &AutomationGraph {
        self.backend.graph()
    }

    fn lane_snapshot(&self, id: AutomationLaneId) -> Option<LaneSnapshot> {
        let lane = self.backend.graph().lane(id)?;
        let descriptor = self
            .backend
            .graph()
            .descriptors()
            .find(|descriptor| descriptor.address == lane.target)?
            .clone();
        Some(LaneSnapshot {
            id,
            name: lane.name.clone(),
            target: describe_target(&lane.target),
            enabled: lane.enabled,
            binding: lane.binding,
            points: lane.points().to_vec(),
            descriptor,
        })
    }

    fn execute_lane_edit<F>(
        &mut self,
        label: &str,
        lane_id: AutomationLaneId,
        edit: F,
        cx: &mut Context<Self>,
    ) where
        F: FnOnce(&mut AutomationLane) -> Result<(), String>,
    {
        let Some(before) = self.backend.graph().lane(lane_id).cloned() else {
            self.status = "Selected automation lane no longer exists".into();
            cx.notify();
            return;
        };
        let mut after = before.clone();
        let result = edit(&mut after)
            .and_then(|()| {
                AutomationCommand::replace(label, before, after).map_err(|e| e.to_string())
            })
            .and_then(|command| self.backend.execute(command));
        self.status = match result {
            Ok(()) => format!("{label} · compiled preview refreshed"),
            Err(error) => format!("Could not {label}: {error}"),
        };
        cx.notify();
    }

    fn select_lane(&mut self, lane: AutomationLaneId, cx: &mut Context<Self>) {
        self.selected_lane = Some(lane);
        self.selected_point = None;
        self.status = "Lane selected · click curve to add, drag points to move".into();
        cx.notify();
    }

    fn toggle_lane(&mut self, lane: AutomationLaneId, cx: &mut Context<Self>) {
        self.execute_lane_edit(
            "toggle lane",
            lane,
            |lane| {
                lane.enabled = !lane.enabled;
                Ok(())
            },
            cx,
        );
    }

    fn cycle_binding(&mut self, cx: &mut Context<Self>) {
        let Some(lane) = self.selected_lane else {
            return;
        };
        self.execute_lane_edit(
            "change binding mode",
            lane,
            |lane| {
                lane.binding = match lane.binding {
                    BindingMode::Replace => BindingMode::Add,
                    BindingMode::Add => BindingMode::Multiply,
                    BindingMode::Multiply => BindingMode::Replace,
                };
                Ok(())
            },
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

    fn set_selected_shape(&mut self, shape: SegmentShape, cx: &mut Context<Self>) {
        let (Some(lane_id), Some(point_id)) = (self.selected_lane, self.selected_point) else {
            self.status = "Select a point before changing segment type".into();
            cx.notify();
            return;
        };
        self.execute_lane_edit(
            "change segment type",
            lane_id,
            move |lane| {
                let mut point = lane
                    .remove_point(point_id)
                    .ok_or_else(|| "point disappeared".to_string())?;
                point.outgoing = shape;
                lane.insert_point(point)
                    .map_err(|error| error.to_string())?;
                Ok(())
            },
            cx,
        );
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        let (Some(lane_id), Some(point_id)) = (self.selected_lane, self.selected_point) else {
            self.status = "No automation point selected".into();
            cx.notify();
            return;
        };
        self.execute_lane_edit(
            "delete point",
            lane_id,
            move |lane| {
                lane.remove_point(point_id)
                    .ok_or_else(|| "point disappeared".to_string())?;
                Ok(())
            },
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
        let (x, y) = (f32::from(event.position.x), f32::from(event.position.y));
        if let Some(point) = self.point_at(x, y, &snapshot) {
            self.selected_point = Some(point);
            self.dragging = true;
            self.status = "Dragging point · exact project-time/value edit".into();
            cx.notify();
            return;
        }
        let Some(viewport) = self.viewport() else {
            return;
        };
        let coordinate = viewport.x_to_position(x);
        let value = snapshot.descriptor.denormalize(viewport.y_to_normalized(y));
        let id = AutomationPointId::from_raw(self.next_ui_point_id);
        self.next_ui_point_id = self.next_ui_point_id.saturating_add(1);
        self.execute_lane_edit(
            "add point",
            lane_id,
            move |lane| {
                lane.insert_point(AutomationPoint {
                    id,
                    position: position_for_domain(lane.time_domain, coordinate),
                    value,
                    outgoing: SegmentShape::Linear,
                })
                .map_err(|error| error.to_string())?;
                Ok(())
            },
            cx,
        );
        self.selected_point = Some(id);
        self.dragging = true;
        self.cursor_coordinate = coordinate;
    }

    fn drag_curve_point(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.dragging {
            return;
        }
        let (Some(lane_id), Some(point_id), Some(viewport)) =
            (self.selected_lane, self.selected_point, self.viewport())
        else {
            return;
        };
        let Some(snapshot) = self.lane_snapshot(lane_id) else {
            return;
        };
        let requested = viewport.x_to_position(f32::from(event.position.x));
        let coordinate = clamp_point_coordinate(
            &snapshot.points,
            point_id,
            requested,
            self.view_start,
            self.view_end,
        );
        let value = snapshot
            .descriptor
            .denormalize(viewport.y_to_normalized(f32::from(event.position.y)));
        self.execute_lane_edit(
            "move point",
            lane_id,
            move |lane| {
                let mut point = lane
                    .remove_point(point_id)
                    .ok_or_else(|| "point disappeared".to_string())?;
                point.position = position_for_domain(lane.time_domain, coordinate);
                point.value = value;
                lane.insert_point(point)
                    .map_err(|error| error.to_string())?;
                Ok(())
            },
            cx,
        );
        self.cursor_coordinate = coordinate;
    }

    fn end_curve_edit(&mut self, _event: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.dragging {
            self.dragging = false;
            self.status = "Point edit committed · ⌘Z to undo".into();
            cx.notify();
        }
    }

    fn set_cursor_from_event(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if self.dragging {
            return;
        }
        if let Some(viewport) = self.viewport() {
            self.cursor_coordinate = viewport.x_to_position(f32::from(event.position.x));
            cx.notify();
        }
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        self.status = match self.backend.undo() {
            Ok(true) => "Undid automation edit · preview recompiled".into(),
            Ok(false) => "Automation history is already at its beginning".into(),
            Err(error) => format!("Undo failed: {error}"),
        };
        cx.notify();
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        self.status = match self.backend.redo() {
            Ok(true) => "Redid automation edit · preview recompiled".into(),
            Ok(false) => "Nothing to redo".into(),
            Err(error) => format!("Redo failed: {error}"),
        };
        cx.notify();
    }

    fn compiled_preview(&self, snapshot: &LaneSnapshot) -> Option<f64> {
        let tempo = FixedTempo::new(48_000, 120_000_000).ok()?;
        let compiled = self.backend.graph().compile(&tempo).ok()?;
        let frame = match self
            .backend
            .graph()
            .lane(snapshot.id)
            .map(|lane| lane.time_domain)?
        {
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
        let lanes: Vec<_> = self
            .backend
            .graph()
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
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(DIM))
                                    .child("Stable targets · sample-exact compiled evaluation"),
                            ),
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
                                            if this.dragging {
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
    fn fader_and_meter_geometry_is_bounded_and_monotonic() {
        let gains = [-100.0, -72.0, -36.0, 0.0, 12.0, 50.0];
        let fractions: Vec<_> = gains.into_iter().map(gain_to_fader_fraction).collect();
        assert!(fractions.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(fractions.iter().all(|value| (0.0..=1.0).contains(value)));
        assert_eq!(db_to_meter_fraction(-100.0), 0.0);
        assert_eq!(db_to_meter_fraction(12.0), 1.0);
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
        let command = MixerCommand::build("gain", backend.graph(), |graph| {
            graph.set_gain_db(bus, -9.0)
        })
        .unwrap();
        backend.execute(command).unwrap();
        assert_eq!(backend.graph().bus(bus).unwrap().fader().gain_db(), -9.0);
        assert!(backend.undo().unwrap());
        assert_eq!(backend.graph().bus(bus).unwrap().fader().gain_db(), 0.0);
        assert!(backend.redo().unwrap());
        assert_eq!(backend.graph().bus(bus).unwrap().fader().gain_db(), -9.0);
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
        let command = MixerCommand::build("gain", backend.graph(), |graph| {
            graph.set_gain_db(bus, -9.0)
        })
        .unwrap();

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
        backend.execute(command).unwrap();
        assert!(!backend.graph().lane(lane_id).unwrap().enabled);
        assert!(backend.undo().unwrap());
        assert!(backend.graph().lane(lane_id).unwrap().enabled);
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

        backend.execute(command).unwrap();
        assert!(!shared_graph.lock().unwrap().lane(lane_id).unwrap().enabled);

        assert!(backend.undo().unwrap());
        assert!(shared_graph.lock().unwrap().lane(lane_id).unwrap().enabled);

        assert!(backend.redo().unwrap());
        assert!(!shared_graph.lock().unwrap().lane(lane_id).unwrap().enabled);
    }
}
