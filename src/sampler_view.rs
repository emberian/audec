//! Musician-facing GPUI pad bank and sample-zone inspector.
//!
//! The editor reads the shared sample-kit library on every render. It owns
//! only ephemeral selection and emits [`SampleAction`](crate::sample_actions::SampleAction)
//! values for every audible or authored consequence, keeping command history,
//! ID allocation, undo, and constructive planning in the project controller.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};

use gpui::{
    div, prelude::*, px, rgb, rgba, App, Context, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, KeyUpEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Render,
    SharedString, Subscription, Window,
};

use crate::assets::{AssetFrameRange, AssetRegistry, MediaAsset, SampleFrames};
use crate::mixer::BusId;
use crate::sample_actions::{
    sample_result_provenance_label, CreatePatternFromPadsIntent, SampleAction,
    SampleActionCallback, SampleActionError, SampleActionResult, SampleActionTracker,
    SampleAuditionIntent, SampleDispatchReceipt, SampleEnvelopeIntent, SampleFeedbackTone,
    SampleFocusCallback, SampleInspectTarget, SampleLoopMode, SamplePublishedResult,
    SampleRequestId, SampleResultFocus, SampleViewOutcome, SamplerDiagnostic,
    SamplerDiagnosticSeverity, SamplerGatePress, SamplerPaneModel, SamplerTarget,
    SamplerViewDisposition, SamplerWorkspaceIntent, ZoneEditIntent, ZoneEditTarget,
    SAMPLER_KEYBOARD_KEYS,
};
use crate::sample_kit::{KitId, PadId, SampleKit, SampleKitLibrary, SamplePad, SampleZone, ZoneId};
use crate::sample_material::SampleMaterialProvenance;
use crate::sample_material::SourceMaterialRef;
use crate::ui_drag::{AssetDrag, DragModifiers};

#[path = "sampler_gate_lifecycle.rs"]
mod sampler_gate_lifecycle;
use sampler_gate_lifecycle::{SamplerGateLifecycle, SamplerGateTransition};

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

const PAD_KEY_COUNT: usize = SAMPLER_KEYBOARD_KEYS.len();

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SamplerBusOption {
    pub id: BusId,
    pub name: String,
}

pub type SamplerBusProvider = Arc<dyn Fn() -> Vec<SamplerBusOption> + Send + Sync + 'static>;
pub type SamplerDiagnosticsProvider =
    Arc<dyn Fn(SamplerTarget) -> Vec<SamplerDiagnostic> + Send + Sync + 'static>;

/// Authoritative inputs for a pad editor. `buses` is a live projection
/// callback rather than a cached mixer copy.
#[derive(Clone)]
pub struct SamplerViewSource {
    pub kits: Arc<Mutex<SampleKitLibrary>>,
    pub assets: Arc<Mutex<AssetRegistry>>,
    pub kit: KitId,
    pub buses: SamplerBusProvider,
    pub diagnostics: SamplerDiagnosticsProvider,
}

impl SamplerViewSource {
    pub fn new(
        kits: Arc<Mutex<SampleKitLibrary>>,
        assets: Arc<Mutex<AssetRegistry>>,
        kit: KitId,
        buses: SamplerBusProvider,
    ) -> Self {
        Self {
            kits,
            assets,
            kit,
            buses,
            diagnostics: Arc::new(|_| Vec::new()),
        }
    }

    pub fn with_diagnostics(mut self, diagnostics: SamplerDiagnosticsProvider) -> Self {
        self.diagnostics = diagnostics;
        self
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SamplerViewState {
    pub selected_pad: Option<PadId>,
    pub selected_zone: Option<ZoneId>,
    pub bank: u16,
}

pub struct SamplerView {
    source: SamplerViewSource,
    callback: Option<SampleActionCallback>,
    focus_callback: Option<SampleFocusCallback>,
    sample_actions: SampleActionTracker,
    auditioned_pads: BTreeMap<PadId, bool>,
    last_publication: Option<SamplePublishedResult>,
    pane: SamplerPaneModel,
    gates: SamplerGateLifecycle,
    focus_handle: FocusHandle,
    focus_subscription: Option<Subscription>,
    status: String,
}

impl SamplerView {
    pub fn new(source: SamplerViewSource, cx: &mut Context<Self>) -> Self {
        Self::with_callback(source, None, cx)
    }

    pub fn with_callback(
        source: SamplerViewSource,
        callback: Option<SampleActionCallback>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
            pane: SamplerPaneModel::new(SamplerTarget::Kit(source.kit)),
            source,
            callback,
            focus_callback: None,
            sample_actions: SampleActionTracker::default(),
            auditioned_pads: BTreeMap::new(),
            last_publication: None,
            gates: SamplerGateLifecycle::default(),
            focus_handle: cx.focus_handle(),
            focus_subscription: None,
            status: "Ready · hold/drag across pads to audition · drop exact material to assign"
                .into(),
        };
        view.reconcile_selection();
        view
    }

    pub fn source(&self) -> &SamplerViewSource {
        &self.source
    }

    pub fn state(&self) -> SamplerViewState {
        let selection = self.pane.selection();
        SamplerViewState {
            selected_pad: selection.pad,
            selected_zone: selection.zone,
            bank: selection.bank,
        }
    }

    pub fn target(&self) -> SamplerTarget {
        self.pane.target()
    }

    pub fn set_callback(&mut self, callback: Option<SampleActionCallback>) {
        self.callback = callback;
    }

    pub fn set_focus_callback(&mut self, callback: Option<SampleFocusCallback>) {
        self.focus_callback = callback;
    }

    pub fn sample_feedback(&self) -> &crate::sample_actions::SampleActionFeedback {
        self.sample_actions.feedback()
    }

    pub fn pending_sample_action_count(&self) -> usize {
        self.sample_actions.pending_count()
    }

    pub fn auditioned_pads(&self) -> Vec<PadId> {
        self.auditioned_pads.keys().copied().collect()
    }

    pub fn last_sample_publication(&self) -> Option<&SamplePublishedResult> {
        self.last_publication.as_ref()
    }

    pub fn chop_result(&self) -> Option<&crate::sample_actions::ChopResultSelection> {
        self.pane.chop_result()
    }

    /// Re-emit the durable result's typed reveal without synthesizing pane
    /// navigation locally.
    pub fn reveal_last_result(&self) -> bool {
        let Some(receipt) = self.last_publication.as_ref() else {
            return false;
        };
        if receipt.focus == SampleResultFocus::Stay {
            return false;
        }
        let Some(callback) = self.focus_callback.as_ref() else {
            return false;
        };
        callback(receipt.focus);
        true
    }

    /// Deliver a result previously accepted by the session adapter. Stale IDs
    /// are ignored, which keeps old analysis from retargeting a reused editor.
    pub fn complete_request(
        &mut self,
        request_id: SampleRequestId,
        result: SampleActionResult,
        cx: &mut Context<Self>,
    ) -> bool {
        let Ok(action) = self.sample_actions.complete(request_id, &result) else {
            return false;
        };
        self.apply_sample_outcome(action, result, cx);
        true
    }

    pub fn set_kit(&mut self, kit: KitId, cx: &mut Context<Self>) {
        self.retarget(SamplerTarget::Kit(kit), cx);
    }

    pub fn retarget(&mut self, target: SamplerTarget, cx: &mut Context<Self>) {
        self.release_all_pads(cx);
        self.pane.retarget(target);
        if let Some(kit) = target.kit() {
            self.source.kit = kit;
        }
        self.reconcile_selection();
        cx.notify();
    }

    pub fn set_state(&mut self, state: SamplerViewState, cx: &mut Context<Self>) {
        if let Some(kit) = self.kit_snapshot() {
            let _ = self.pane.set_bank(&kit, state.bank);
            if let Some(pad) = state.selected_pad {
                let _ = self.pane.select_pad(&kit, pad);
            }
            if let Some(zone) = state.selected_zone {
                let _ = self.pane.select_zone(&kit, zone);
            }
            let _ = self.pane.project(&kit);
        }
        cx.notify();
    }

    /// Public integration seam for panes that mediate drag/drop themselves.
    pub fn accept_asset_drop(
        &mut self,
        source: AssetDrag,
        pad: PadId,
        modifiers: DragModifiers,
        cx: &mut Context<Self>,
    ) {
        let Some(kit) = self.kit_snapshot() else {
            self.status = "Create or choose a kit before dropping material".into();
            cx.notify();
            return;
        };
        match self.pane.map_browser_to_pad(&kit, pad, source, modifiers) {
            Ok(action) => {
                // Mapping may replace the material beneath an active preview.
                // Close every pointer/key owner of this pad first; a late
                // press completion then observes that the pad is no longer
                // held and cannot resurrect the old sound.
                let releases = self.gates.release_pad(pad);
                self.emit_gate_transitions(releases, cx);
                self.status = match source.source_range {
                    Some(range) => format!(
                        "Mapping frames {}–{} to pad {}",
                        range.start.0,
                        range.end.0,
                        pad.get()
                    ),
                    None => format!("Mapping asset {} to pad {}", source.asset.0, pad.get()),
                };
                self.emit(action, cx);
            }
            Err(error) => self.status = format!("Drop refused: {error}"),
        }
        cx.notify();
    }

    fn emit(&mut self, action: SampleAction, cx: &mut Context<Self>) {
        let request = self.sample_actions.prepare(action);
        let Some(callback) = self.callback.as_ref() else {
            self.sample_actions.disconnect(&request.action);
            cx.notify();
            return;
        };
        match callback(request.clone()) {
            SampleDispatchReceipt::Completed(result) => {
                self.sample_actions.complete_now(&request.action, &result);
                self.apply_sample_outcome(request.action, result, cx);
            }
            SampleDispatchReceipt::Accepted {
                request_id,
                kind,
                provenance,
            } => {
                let _ = self
                    .sample_actions
                    .accept(request, request_id, kind, provenance);
                cx.notify();
            }
        }
    }

    fn apply_sample_outcome(
        &mut self,
        action: SampleAction,
        result: SampleActionResult,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(SampleViewOutcome::Audition(SampleAuditionIntent::PadGate {
                pad, pressed, ..
            })) => {
                let still_held = self.gates.holds_pad(pad);
                if pressed && still_held {
                    self.auditioned_pads.insert(pad, true);
                } else {
                    // A late async press completion must not resurrect a gate
                    // whose pointer/key has already been released.
                    self.auditioned_pads.remove(&pad);
                }
            }
            Ok(SampleViewOutcome::Audition(_)) => {}
            Ok(SampleViewOutcome::Published(receipt)) => {
                let focus = receipt.focus;
                let published = Ok(SampleViewOutcome::Published(receipt.clone()));
                if let Some(target) = focus.sampler_retarget() {
                    self.retarget(target, cx);
                }
                if let Some(kit) = self
                    .source
                    .kits
                    .lock()
                    .ok()
                    .and_then(|library| library.kits.get(&receipt.kit).cloned())
                {
                    if let Err(error) = self.pane.apply_publication(&receipt, &kit) {
                        self.status =
                            format!("Published, but result selection is unavailable: {error}");
                    } else {
                        self.source.kit = receipt.kit;
                        self.status = publication_status(&receipt);
                    }
                }
                // Retargeting releases held pads through the same callback and
                // may update transient audition feedback. Restore the durable
                // publication receipt as the musician-visible final result.
                self.sample_actions.complete_now(&action, &published);
                self.last_publication = Some(receipt);
                if focus != SampleResultFocus::Stay {
                    if let Some(callback) = self.focus_callback.as_ref() {
                        callback(focus);
                    }
                }
            }
            Ok(SampleViewOutcome::ChopPreview(_)) | Ok(SampleViewOutcome::Acknowledged { .. }) => {}
            Err(_) => {
                if let SampleAction::Audition(SampleAuditionIntent::PadGate {
                    pad,
                    pressed: false,
                    ..
                }) = action
                {
                    self.auditioned_pads.remove(&pad);
                }
            }
        }
        cx.notify();
    }

    fn kit_snapshot(&self) -> Option<SampleKit> {
        let kit = self.pane.target().kit()?;
        self.source
            .kits
            .lock()
            .ok()
            .and_then(|library| library.kits.get(&kit).cloned())
    }

    fn reconcile_selection(&mut self) {
        let Some(kit) = self.kit_snapshot() else {
            return;
        };
        let _ = self.pane.project(&kit);
    }

    fn select_pad_ephemeral(
        &mut self,
        pad: PadId,
        cx: &mut Context<Self>,
    ) -> Option<(KitId, bool)> {
        let Some(kit) = self.kit_snapshot() else {
            return None;
        };
        let changed = self.pane.selection().pad != Some(pad);
        if let Err(error) = self.pane.select_pad(&kit, pad) {
            self.status = error.to_string();
            cx.notify();
            return None;
        }
        self.status = format!("Selected pad {}", pad.get());
        cx.notify();
        Some((kit.id, changed))
    }

    fn select_pad(&mut self, pad: PadId, cx: &mut Context<Self>) {
        let Some((kit, changed)) = self.select_pad_ephemeral(pad, cx) else {
            return;
        };
        if changed {
            self.emit(
                SampleAction::Workspace(SamplerWorkspaceIntent {
                    target: SamplerTarget::Pad { kit, pad },
                    disposition: SamplerViewDisposition::RetargetCurrent,
                }),
                cx,
            );
        }
    }

    fn select_chop_result_pad(&mut self, pad: PadId, cx: &mut Context<Self>) {
        let Some(kit) = self.kit_snapshot() else {
            return;
        };
        match self.pane.select_chop_result_pad(&kit, pad) {
            Ok(()) => self.status = format!("Created pad {} selected", pad.get()),
            Err(error) => self.status = error.to_string(),
        }
        cx.notify();
    }

    fn emit_gate(&mut self, gate: SamplerGatePress, pressed: bool, cx: &mut Context<Self>) {
        if !pressed {
            // Visual release follows physical ownership immediately. The
            // correlated controller outcome may arrive later, but it must not
            // leave a pad glowing after the last pointer/key owner is gone.
            self.auditioned_pads.remove(&gate.pad);
        }
        self.emit(
            if pressed {
                gate.press_action()
            } else {
                gate.release_action()
            },
            cx,
        );
        self.status = if pressed {
            format!("Auditioning pad {}", gate.pad.get())
        } else {
            "Ready".into()
        };
        cx.notify();
    }

    fn emit_gate_transitions(
        &mut self,
        transitions: Vec<SamplerGateTransition>,
        cx: &mut Context<Self>,
    ) {
        for transition in transitions {
            match transition {
                SamplerGateTransition::Press(gate) => self.emit_gate(gate, true, cx),
                SamplerGateTransition::Release(gate) => self.emit_gate(gate, false, cx),
            }
        }
    }

    fn press_pointer_pad(&mut self, pad: PadId, cx: &mut Context<Self>) {
        if self.gates.pointer().is_some_and(|gate| gate.pad == pad) {
            return;
        }
        let Some(kit) = self.kit_snapshot() else {
            self.release_pointer_pad(cx);
            return;
        };
        match self.pane.press_pad(&kit, pad, 1.0) {
            Ok(gate) => {
                let transitions = self.gates.press_pointer(gate);
                self.emit_gate_transitions(transitions, cx);
            }
            Err(error) => {
                self.release_pointer_pad(cx);
                self.status = error.to_string();
                cx.notify();
            }
        }
    }

    fn drag_pointer_pad(&mut self, pad: PadId, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if cx.has_active_drag()
            || !event.dragging()
            || self.gates.pointer().is_some_and(|gate| gate.pad == pad)
        {
            return;
        }
        // Crossing pads during one held pointer gesture is ephemeral audition
        // navigation. Avoid emitting a workspace retarget command for every
        // pad crossed; the initial press/keyboard selection remains typed.
        if self.select_pad_ephemeral(pad, cx).is_none() {
            return;
        }
        self.press_pointer_pad(pad, cx);
    }

    fn release_pointer_pad(&mut self, cx: &mut Context<Self>) {
        let transitions = self.gates.release_pointer();
        self.emit_gate_transitions(transitions, cx);
    }

    fn release_all_pads(&mut self, cx: &mut Context<Self>) {
        let transitions = self.gates.drain();
        self.emit_gate_transitions(transitions, cx);
    }

    fn handle_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.platform
            || event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
        {
            return;
        }
        let key = event.keystroke.key.to_lowercase();
        match key.as_str() {
            "[" => {
                if let Some(kit) = self.kit_snapshot() {
                    let bank = self.pane.selection().bank.saturating_sub(1);
                    let _ = self.pane.set_bank(&kit, bank);
                    self.status = format!("Keyboard bank {}", bank + 1);
                }
            }
            "]" => {
                if let Some(kit) = self.kit_snapshot() {
                    let banks = kit.pad_order.len().max(1).div_ceil(PAD_KEY_COUNT);
                    let bank = (usize::from(self.pane.selection().bank) + 1)
                        .min(banks.saturating_sub(1)) as u16;
                    let _ = self.pane.set_bank(&kit, bank);
                    self.status = format!("Keyboard bank {}", bank + 1);
                }
            }
            _ => {
                if self.gates.key(&key).is_some() {
                    return;
                }
                let Some(kit) = self.kit_snapshot() else {
                    return;
                };
                let Some(pad) = pad_for_key(&kit, self.pane.selection().bank, &key) else {
                    return;
                };
                self.select_pad(pad, cx);
                if let Ok(gate) = self.pane.press_pad(&kit, pad, 1.0) {
                    let transitions = self.gates.press_key(key, gate);
                    self.emit_gate_transitions(transitions, cx);
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn handle_key_up(&mut self, event: &KeyUpEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.to_lowercase();
        if self.gates.key(&key).is_none() {
            return;
        }
        let transitions = self.gates.release_key(&key);
        self.emit_gate_transitions(transitions, cx);
        cx.stop_propagation();
    }

    fn cycle_output(&mut self, kit: &SampleKit, cx: &mut Context<Self>) {
        let buses = (self.source.buses)();
        if buses.is_empty() {
            self.status = "No routable mixer buses are available".into();
            cx.notify();
            return;
        }
        let current = buses
            .iter()
            .position(|candidate| candidate.id == kit.output.bus)
            .unwrap_or(buses.len() - 1);
        let bus = buses[(current + 1) % buses.len()].id;
        self.emit(
            SampleAction::SetKitOutput {
                kit: kit.id,
                bus,
                expected_revision: kit.revision,
            },
            cx,
        );
        self.status = format!("Routing kit to bus {bus}");
        cx.notify();
    }

    fn request_create_pattern_from_pads(&mut self, cx: &mut Context<Self>) {
        let Some(kit) = self.kit_snapshot() else {
            self.status = "This instrument is no longer available".into();
            cx.notify();
            return;
        };
        match self.pane.create_pattern_from_pads(&kit) {
            Ok(action) => {
                self.status = CreatePatternFromPadsIntent::LABEL.into();
                self.emit(action, cx);
            }
            Err(error) => {
                let action = SampleAction::CreatePatternFromPads(CreatePatternFromPadsIntent {
                    kit: kit.id,
                    expected_revision: kit.revision,
                    bars: CreatePatternFromPadsIntent::DEFAULT_BARS,
                    quantize_ticks: CreatePatternFromPadsIntent::default_quantize_ticks(),
                    result_focus: crate::sample_actions::MakeBeatResultFocus::PatternEditor,
                });
                let result = Err(SampleActionError::new(
                    "sample.empty-pads",
                    error.to_string(),
                ));
                self.sample_actions.complete_now(&action, &result);
                self.status = self.sample_actions.feedback().headline.clone();
                cx.notify();
            }
        }
    }

    fn request_target(
        &mut self,
        target: SamplerTarget,
        disposition: SamplerViewDisposition,
        cx: &mut Context<Self>,
    ) {
        self.emit(
            SampleAction::Workspace(SamplerWorkspaceIntent {
                target,
                disposition,
            }),
            cx,
        );
        self.status = format!("Workspace target · {target:?}");
        cx.notify();
    }

    fn cycle_kit_target(&mut self, delta: isize, cx: &mut Context<Self>) {
        let kits = self
            .source
            .kits
            .lock()
            .map(|library| library.kits.keys().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        if kits.is_empty() {
            self.request_target(
                SamplerTarget::NewKit,
                SamplerViewDisposition::RetargetCurrent,
                cx,
            );
            return;
        }
        let current = self
            .pane
            .target()
            .kit()
            .and_then(|id| kits.iter().position(|candidate| *candidate == id))
            .unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(kits.len() as isize) as usize;
        self.request_target(
            SamplerTarget::Kit(kits[next]),
            SamplerViewDisposition::RetargetCurrent,
            cx,
        );
    }

    fn selected_zone_context(&self) -> Option<(SampleKit, SampleZone, AssetFrameRange)> {
        let kit = self.kit_snapshot()?;
        let zone = kit.zones.get(&self.pane.selection().zone?).cloned()?;
        let asset = self.asset_for_material(zone.material);
        let range = material_range(zone.material, asset.as_ref());
        Some((kit, zone, range))
    }

    fn zone_edit_target(kit: &SampleKit, zone: &SampleZone) -> ZoneEditTarget {
        ZoneEditTarget {
            kit: kit.id,
            pad: zone.pad,
            zone: zone.id,
            expected_revision: kit.revision,
        }
    }

    fn trim_selected_zone(&mut self, cx: &mut Context<Self>) {
        let Some((kit, zone, range)) = self.selected_zone_context() else {
            return;
        };
        let inset = (range.len().0 / 20).max(1);
        if range.len().0 <= inset.saturating_mul(2) {
            self.status = "Zone is too short to trim by 5%".into();
            cx.notify();
            return;
        }
        let source_range = AssetFrameRange {
            start: SampleFrames(range.start.0 + inset),
            end: SampleFrames(range.end.0 - inset),
        };
        match self.pane.set_zone_range(&kit, zone.id, source_range) {
            Ok(action) => self.emit(action, cx),
            Err(error) => self.status = error.to_string(),
        }
        self.status = format!(
            "Trim request · frames {}–{}",
            source_range.start.0, source_range.end.0
        );
        cx.notify();
    }

    fn loop_selected_zone(&mut self, cx: &mut Context<Self>) {
        let Some((kit, zone, range)) = self.selected_zone_context() else {
            return;
        };
        self.emit(
            SampleAction::EditZone(ZoneEditIntent::SetLoop {
                target: Self::zone_edit_target(&kit, &zone),
                enabled: true,
                source_range: Some(range),
                mode: SampleLoopMode::Forward,
            }),
            cx,
        );
        self.status = "Forward loop request sent for the visible zone range".into();
        cx.notify();
    }

    fn ping_pong_selected_zone(&mut self, cx: &mut Context<Self>) {
        let Some((kit, zone, range)) = self.selected_zone_context() else {
            return;
        };
        self.emit(
            SampleAction::EditZone(ZoneEditIntent::SetLoop {
                target: Self::zone_edit_target(&kit, &zone),
                enabled: true,
                source_range: Some(range),
                mode: SampleLoopMode::PingPong,
            }),
            cx,
        );
        self.status = "Ping-pong loop request sent for the exact visible range".into();
        cx.notify();
    }

    fn adjust_selected_playback(
        &mut self,
        gain_delta: f32,
        pan_delta: f32,
        tuning_delta: f32,
        cx: &mut Context<Self>,
    ) {
        let Some((kit, zone, _)) = self.selected_zone_context() else {
            return;
        };
        let gain = (zone.gain_db + gain_delta).clamp(-144.0, 48.0);
        let pan = (zone.pan + pan_delta).clamp(-1.0, 1.0);
        let tuning = (zone.tuning_cents + tuning_delta).clamp(-9_600.0, 9_600.0);
        match self
            .pane
            .set_zone_playback(&kit, zone.id, gain, pan, tuning)
        {
            Ok(action) => {
                self.emit(action, cx);
                self.status =
                    format!("Playback edit · {gain:+.1} dB · pan {pan:+.2} · {tuning:+.0} cents");
            }
            Err(error) => self.status = error.to_string(),
        }
        cx.notify();
    }

    fn cycle_selected_choke(&mut self, cx: &mut Context<Self>) {
        let Some(kit) = self.kit_snapshot() else {
            return;
        };
        let Some(pad_id) = self.pane.selection().pad else {
            return;
        };
        let Some(pad) = kit.pads.get(&pad_id) else {
            return;
        };
        let next = match pad.choke_group {
            None => NonZeroU32::new(1),
            Some(1..=3) => NonZeroU32::new(pad.choke_group.unwrap() + 1),
            Some(_) => None,
        };
        match self.pane.set_pad_choke(&kit, pad_id, next) {
            Ok(action) => {
                self.emit(action, cx);
                self.status = next.map_or_else(
                    || "Pad choke disabled".into(),
                    |group| format!("Pad choke group {}", group.get()),
                );
            }
            Err(error) => self.status = error.to_string(),
        }
        cx.notify();
    }

    fn set_percussive_envelope(&mut self, cx: &mut Context<Self>) {
        let Some((kit, zone, _)) = self.selected_zone_context() else {
            return;
        };
        self.emit(
            SampleAction::EditZone(ZoneEditIntent::SetEnvelope {
                target: Self::zone_edit_target(&kit, &zone),
                envelope: SampleEnvelopeIntent::percussive(),
            }),
            cx,
        );
        self.status = "Percussive envelope request sent".into();
        cx.notify();
    }

    fn render_pad(
        &self,
        kit: &SampleKit,
        pad: &SamplePad,
        ordinal: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = pad.id;
        let selected = self.pane.selection().pad == Some(id);
        let held = self.gates.holds_pad(id);
        let auditioning = held || self.auditioned_pads.contains_key(&id);
        let zones = pad.zone_order.len();
        let primary = kit.ordered_zones(id).next();
        let material = primary
            .map(|zone| self.material_label(zone.material))
            .unwrap_or_else(|| "DROP SAMPLE".into());
        let accent = pad_color(ordinal);
        let key = SAMPLER_KEYBOARD_KEYS[ordinal % PAD_KEY_COUNT].to_uppercase();
        div()
            .id(("sample-pad", id.get() as usize))
            .w(px(132.0))
            .h(px(104.0))
            .flex_none()
            .flex()
            .flex_col()
            .justify_between()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(rgb(if selected || auditioning {
                accent
            } else {
                BORDER
            }))
            .bg(if auditioning {
                rgba(((accent as u64) << 8 | 0x32) as u32)
            } else if selected {
                rgba(((accent as u64) << 8 | 0x18) as u32)
            } else {
                rgb(PANEL)
            })
            .cursor_pointer()
            .hover(move |style| style.border_color(rgb(accent)).bg(rgba(0xffffff09)))
            .drag_over::<AssetDrag>(move |style, _, _, _| {
                style
                    .border_color(rgb(accent))
                    .bg(rgba(((accent as u64) << 8 | 0x2c) as u32))
            })
            .on_drop(cx.listener(move |this, source: &AssetDrag, window, cx| {
                this.accept_asset_drop(*source, id, sampler_drag_modifiers(window.modifiers()), cx)
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.select_pad(id, cx);
                    this.press_pointer_pad(id, cx);
                }),
            )
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                this.drag_pointer_pad(id, event, cx)
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(accent))
                            .child(format!("PAD {:02}", ordinal + 1)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(DIM))
                            .child(format!("{key} · {zones}Z")),
                    ),
            )
            .child(
                div()
                    .min_w_0()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(pad.name.clone()),
                    )
                    .child(
                        div()
                            .mt_1()
                            .text_xs()
                            .text_color(rgb(if primary.is_some() { MUTED } else { accent }))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(material),
                    ),
            )
    }

    fn render_zone_list(
        &self,
        kit: &SampleKit,
        pad: Option<&SamplePad>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut list = div().flex().flex_col();
        let Some(pad) = pad else {
            return list.child(empty_message("Select a pad to inspect its zones"));
        };
        if pad.zone_order.is_empty() {
            return list.child(empty_message(
                "Drop a sample or selected range onto this pad",
            ));
        }
        for (index, id) in pad.zone_order.iter().enumerate() {
            let Some(zone) = kit.zones.get(id) else {
                continue;
            };
            let selected = self.pane.selection().zone == Some(*id);
            let zone_id = *id;
            list = list.child(
                div()
                    .id(("sample-zone", zone_id.get() as usize))
                    .h(px(48.0))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_3()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .bg(if selected { rgba(0x50d8d713) } else { rgba(0) })
                    .cursor_pointer()
                    .hover(|style| style.bg(rgba(0xffffff08)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(kit) = this.kit_snapshot() {
                            if this.pane.select_zone(&kit, zone_id).is_ok() {
                                this.status = format!("Selected zone {}", zone_id.get());
                            }
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .w(px(28.0))
                            .text_xs()
                            .text_color(rgb(if selected { CYAN } else { DIM }))
                            .child(format!("Z{}", index + 1)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(TEXT))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .child(self.material_label(zone.material)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(DIM))
                                    .child(zone_range_label(zone.material)),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!("{:+.1} dB", zone.gain_db)),
                    ),
            );
        }
        list
    }

    fn render_zone_inspector(
        &self,
        kit: &SampleKit,
        zone: Option<&SampleZone>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(zone) = zone else {
            return div()
                .w(px(304.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .border_l_1()
                .border_color(rgb(BORDER))
                .bg(rgb(PANEL_ALT))
                .text_sm()
                .text_color(rgb(DIM))
                .child("No zone selected");
        };
        let asset = self.asset_for_material(zone.material);
        let range = material_range(zone.material, asset.as_ref());
        let total = asset.as_ref().map(|asset| asset.metadata().frame_count);
        let asset_uses = asset
            .as_ref()
            .map(|asset| asset.usages().len())
            .unwrap_or_default();
        let mut evidence_rows = div().flex().flex_col();
        if zone.evidence.is_empty() {
            evidence_rows = evidence_rows.child(
                div()
                    .mt_2()
                    .text_xs()
                    .text_color(rgb(DIM))
                    .child("No evidence references · manually authored"),
            );
        } else {
            for evidence in zone.evidence.iter().copied() {
                let label = format!(
                    "Evidence {} · scope {:032x}  →",
                    evidence.local, evidence.scope.0
                );
                evidence_rows = evidence_rows.child(
                    action_row(
                        SharedString::from(format!(
                            "zone-evidence-{}-{}",
                            evidence.scope.0, evidence.local
                        )),
                        label,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.emit(
                            SampleAction::Inspect(SampleInspectTarget::Evidence(evidence)),
                            cx,
                        );
                    })),
                );
            }
        }
        let provenance = provenance_label(&zone.provenance);
        let provenance_target = zone.provenance.clone();
        let material = zone.material;
        let zone_id = zone.id;
        let pad_id = zone.pad;
        let kit_id = kit.id;
        let revision = kit.revision;
        div()
            .w(px(304.0))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL_ALT))
            .child(
                div()
                    .p_4()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(section_label("ZONE / RANGE"))
                    .child(
                        div()
                            .mt_1()
                            .text_base()
                            .text_color(rgb(TEXT))
                            .child(self.material_label(material)),
                    )
                    .child(div().mt_3().child(range_bar(range, total)))
                    .child(
                        div()
                            .mt_2()
                            .flex()
                            .justify_between()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(zone_range_label(material))
                            .child(range_duration_label(range, asset.as_ref())),
                    ),
            )
            .child(
                div()
                    .id("sampler-zone-inspector-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_4()
                    .child(inspector_value("GAIN", format!("{:+.1} dB", zone.gain_db)))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(action_button("zone-gain-down", "−1 dB", MUTED).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.adjust_selected_playback(-1.0, 0.0, 0.0, cx)
                                }),
                            ))
                            .child(action_button("zone-gain-up", "+1 dB", CYAN).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.adjust_selected_playback(1.0, 0.0, 0.0, cx)
                                }),
                            )),
                    )
                    .child(inspector_value("PAN", format!("{:+.2}", zone.pan)))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(action_button("zone-pan-left", "← 0.1", MUTED).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.adjust_selected_playback(0.0, -0.1, 0.0, cx)
                                }),
                            ))
                            .child(action_button("zone-pan-right", "0.1 →", CYAN).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.adjust_selected_playback(0.0, 0.1, 0.0, cx)
                                }),
                            )),
                    )
                    .child(inspector_value(
                        "TUNING",
                        format!("{:+.0} cents", zone.tuning_cents),
                    ))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(action_button("zone-tune-down", "−100¢", MUTED).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.adjust_selected_playback(0.0, 0.0, -100.0, cx)
                                }),
                            ))
                            .child(action_button("zone-tune-up", "+100¢", CYAN).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.adjust_selected_playback(0.0, 0.0, 100.0, cx)
                                }),
                            )),
                    )
                    .child(
                        action_row(
                            "pad-choke",
                            format!(
                                "Pad choke · {}  →",
                                kit.pads
                                    .get(&zone.pad)
                                    .and_then(|pad| pad.choke_group)
                                    .map_or_else(|| "off".into(), |group| group.to_string())
                            ),
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.cycle_selected_choke(cx))),
                    )
                    .child(
                        div()
                            .mt_4()
                            .child(section_label("WAVEFORM / PLAYBACK"))
                            .child(
                                div()
                                    .mt_2()
                                    .flex()
                                    .gap_2()
                                    .child(action_button("zone-trim", "TRIM 5%", CYAN).on_click(
                                        cx.listener(|this, _, _, cx| this.trim_selected_zone(cx)),
                                    ))
                                    .child(action_button("zone-loop", "LOOP RANGE", LIME).on_click(
                                        cx.listener(|this, _, _, cx| this.loop_selected_zone(cx)),
                                    ))
                                    .child(
                                        action_button("zone-ping-pong", "PING PONG", AMBER)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.ping_pong_selected_zone(cx)
                                            })),
                                    ),
                            )
                            .child(div().mt_2().text_xs().text_color(rgb(DIM)).child(
                                "Reverse playback is not persisted by the sample-zone model",
                            ))
                            .child(
                                action_row(
                                    "zone-envelope",
                                    "Envelope · percussive A64 / D4800 / S0 / R1200  →".into(),
                                )
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.set_percussive_envelope(cx)),
                                ),
                            ),
                    )
                    .child(div().mt_5().child(section_label("PROVENANCE")).child(
                        action_row("zone-provenance", provenance).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.emit(
                                    SampleAction::Inspect(SampleInspectTarget::Provenance(
                                        provenance_target.clone(),
                                    )),
                                    cx,
                                );
                            },
                        )),
                    ))
                    .child(div().mt_4().child(section_label("PROJECT USES")).child(
                        div().mt_2().text_xs().text_color(rgb(MUTED)).child(format!(
                            "Source asset {} · {asset_uses} registered uses",
                            material.asset_id().0
                        )),
                    ))
                    .child(
                        div()
                            .mt_4()
                            .child(section_label("EVIDENCE"))
                            .child(evidence_rows),
                    ),
            )
            .child(div().p_3().border_t_1().border_color(rgb(BORDER)).child(
                action_button("zone-remove", "REMOVE ZONE", MAGENTA).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.emit(
                            SampleAction::RemoveZone {
                                kit: kit_id,
                                pad: pad_id,
                                zone: zone_id,
                                expected_revision: revision,
                            },
                            cx,
                        );
                        this.status = format!("Removing zone {}", zone_id.get());
                        cx.notify();
                    },
                )),
            ))
    }

    fn material_label(&self, material: SourceMaterialRef) -> String {
        self.asset_for_material(material)
            .map(|asset| asset.name().to_owned())
            .unwrap_or_else(|| format!("Asset {} unavailable", material.asset_id().0))
    }

    fn asset_for_material(&self, material: SourceMaterialRef) -> Option<MediaAsset> {
        self.source
            .assets
            .lock()
            .ok()
            .and_then(|registry| registry.get(material.asset_id()).cloned())
    }
}

impl Focusable for SamplerView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SamplerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.focus_subscription.is_none() {
            let focus = self.focus_handle.clone();
            self.focus_subscription = Some(cx.on_focus_out(&focus, window, |this, _, _, cx| {
                // A pane/window focus change is allowed to swallow key-up or
                // pointer-up. Close every held semantic gate here so pad
                // audition cannot remain stuck after the sampler is hidden.
                this.release_all_pads(cx);
                this.status = "Pad audition released when sampler lost focus".into();
                cx.notify();
            }));
        }
        self.reconcile_selection();
        if self.pane.target() == SamplerTarget::NewKit {
            return div()
                .key_context("AudecSampler")
                .track_focus(&self.focus_handle)
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .bg(rgb(BACKGROUND))
                .text_color(rgb(TEXT))
                .child(div().text_xl().text_color(rgb(CYAN)).child("NEW SAMPLER KIT"))
                .child(
                    div()
                        .max_w(px(420.0))
                        .text_sm()
                        .text_center()
                        .text_color(rgb(MUTED))
                        .child(
                            "Choose an exact range in the sample browser, then use Sample selection & make beat.",
                        ),
                )
                .child(
                    action_button("sampler-choose-kit", "CHOOSE EXISTING KIT", LIME).on_click(
                        cx.listener(|this, _, _, cx| this.cycle_kit_target(1, cx)),
                    ),
                );
        }
        let Some(kit) = self.kit_snapshot() else {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .bg(rgb(BACKGROUND))
                .text_color(rgb(DIM))
                .child("Sampler kit is no longer available")
                .child(
                    div()
                        .mt_1()
                        .text_xs()
                        .child(format!("Target {:?}", self.pane.target())),
                );
        };
        let buses = (self.source.buses)();
        let diagnostics = (self.source.diagnostics)(self.pane.target());
        let feedback = self.sample_actions.feedback().clone();
        let pending_count = self.sample_actions.pending_count();
        let output_name = buses
            .iter()
            .find(|candidate| candidate.id == kit.output.bus)
            .map(|candidate| candidate.name.clone())
            .unwrap_or_else(|| format!("Bus {}", kit.output.bus));
        let selection = self.pane.selection();
        let selected_pad = selection.pad.and_then(|id| kit.pads.get(&id));
        let selected_zone = selection.zone.and_then(|id| kit.zones.get(&id));

        let mut pads = div().flex().flex_wrap().gap_3();
        if kit.pad_order.is_empty() {
            pads = pads.child(empty_message(
                "This kit has no authored pads yet · add pads from a controller action",
            ));
        } else {
            let bank_start = usize::from(selection.bank) * PAD_KEY_COUNT;
            for (index, pad) in kit
                .ordered_pads()
                .enumerate()
                .skip(bank_start)
                .take(PAD_KEY_COUNT)
            {
                pads = pads.child(self.render_pad(&kit, pad, index, cx));
            }
        }

        let output_kit = kit.clone();
        let mut diagnostic_panel = div();
        if !diagnostics.is_empty() {
            diagnostic_panel = diagnostic_panel
                .px_4()
                .py_2()
                .flex()
                .flex_col()
                .gap_1()
                .border_b_1()
                .border_color(rgb(BORDER))
                .bg(rgba(0xf6b7600c));
            for diagnostic in diagnostics.iter().take(3) {
                diagnostic_panel = diagnostic_panel.child(
                    div()
                        .flex()
                        .gap_2()
                        .text_xs()
                        .text_color(rgb(diagnostic_color(diagnostic.severity)))
                        .child(diagnostic.code.clone())
                        .child("·")
                        .child(diagnostic.message.clone()),
                );
            }
        }
        let feedback_panel = div().when(feedback.tone != SampleFeedbackTone::Idle, |this| {
            let provenance = feedback
                .provenance
                .as_ref()
                .map(sample_result_provenance_label);
            this.px_4()
                .py_2()
                .flex()
                .items_center()
                .justify_between()
                .border_b_1()
                .border_color(rgb(sampler_feedback_color(feedback.tone)))
                .bg(rgba(sampler_feedback_background(feedback.tone)))
                .child(
                    div()
                        .min_w_0()
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(sampler_feedback_color(feedback.tone)))
                                .child(feedback.headline.clone()),
                        )
                        .when_some(feedback.detail.clone(), |this, detail| {
                            this.child(div().text_xs().text_color(rgb(MUTED)).child(detail))
                        })
                        .when_some(provenance, |this, provenance| {
                            this.child(div().text_xs().text_color(rgb(DIM)).child(provenance))
                        }),
                )
                .when(pending_count > 0, |this| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(rgb(AMBER))
                            .child(format!("{pending_count} IN FLIGHT")),
                    )
                })
        });
        let mut result_panel = div();
        if let Some(result) = self.pane.chop_result().cloned() {
            let mut created = div().flex().items_center().gap_1();
            for pad in result.pads.iter().copied() {
                let selected = result.selected == Some(pad);
                created = created.child(
                    filter_button(
                        SharedString::from(format!("created-pad-{}", pad.get())),
                        format!("P{}", pad.get()),
                        selected,
                    )
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.select_chop_result_pad(pad, cx)),
                    ),
                );
            }
            result_panel = div()
                .px_4()
                .py_2()
                .flex()
                .items_center()
                .justify_between()
                .border_b_1()
                .border_color(rgb(CYAN))
                .bg(rgba(0x50d8d70d))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(section_label(format!(
                            "CREATED · {} PADS / {} ZONES",
                            result.pads.len(),
                            result.zones.len()
                        )))
                        .child(created),
                )
                .child(
                    action_button("sampler-reveal-result", "REVEAL RESULT ↗", CYAN).on_click(
                        cx.listener(|this, _, _, _| {
                            let _ = this.reveal_last_result();
                        }),
                    ),
                );
        }
        div()
            .key_context("AudecSampler")
            .track_focus(&self.focus_handle)
            .on_key_down(
                cx.listener(|this, event: &KeyDownEvent, _, cx| this.handle_key_down(event, cx)),
            )
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _, cx| this.handle_key_up(event, cx)))
            .capture_any_mouse_up(
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.release_pointer_pad(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.release_pointer_pad(cx)),
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
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(CYAN))
                                    .child(format!("SAMPLER KIT {}", kit.id.get())),
                            )
                            .child(
                                div()
                                    .text_base()
                                    .text_color(rgb(TEXT))
                                    .child(kit.name.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(action_button("sampler-prev-kit", "‹ KIT", MUTED).on_click(
                                cx.listener(|this, _, _, cx| this.cycle_kit_target(-1, cx)),
                            ))
                            .child(action_button("sampler-next-kit", "KIT ›", MUTED).on_click(
                                cx.listener(|this, _, _, cx| this.cycle_kit_target(1, cx)),
                            ))
                            .child(action_button("sampler-new-kit", "+ KIT", MAGENTA).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.request_target(
                                        SamplerTarget::NewKit,
                                        SamplerViewDisposition::OpenNew,
                                        cx,
                                    )
                                }),
                            ))
                            .child(action_button("sampler-new-pad", "+ PAD", AMBER).on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.request_target(
                                        SamplerTarget::NewPad { kit: output_kit.id },
                                        SamplerViewDisposition::RetargetCurrent,
                                        cx,
                                    )
                                }),
                            ))
                            .child(
                                action_button(
                                    "sampler-create-pattern-from-pads",
                                    CreatePatternFromPadsIntent::LABEL,
                                    CYAN,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| this.request_create_pattern_from_pads(cx),
                                )),
                            )
                            .child(div().text_xs().text_color(rgb(DIM)).child("TARGET BUS"))
                            .child(action_button("sampler-output", output_name, LIME).on_click(
                                cx.listener({
                                    let output_kit = kit.clone();
                                    move |this, _, _, cx| this.cycle_output(&output_kit, cx)
                                }),
                            )),
                    ),
            )
            .child(feedback_panel)
            .child(result_panel)
            .child(diagnostic_panel)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .px_4()
                                    .py_3()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .border_b_1()
                                    .border_color(rgb(BORDER))
                                    .child(section_label(format!(
                                        "PAD BANK {} · 1234 / QWER / ASDF / ZXCV · [ ] BANK",
                                        selection.bank + 1
                                    )))
                                    .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                                        "{} pads · {} zones · rev {}",
                                        kit.pads.len(),
                                        kit.zones.len(),
                                        kit.revision
                                    ))),
                            )
                            .child(
                                div()
                                    .id("sampler-pad-bank-scroll")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .p_4()
                                    .child(pads),
                            )
                            .child(
                                div()
                                    .h(px(160.0))
                                    .flex_none()
                                    .flex()
                                    .flex_col()
                                    .border_t_1()
                                    .border_color(rgb(BORDER))
                                    .bg(rgb(PANEL))
                                    .child(
                                        div()
                                            .h(px(34.0))
                                            .flex_none()
                                            .px_3()
                                            .flex()
                                            .items_center()
                                            .justify_between()
                                            .border_b_1()
                                            .border_color(rgb(BORDER))
                                            .child(section_label("PAD ZONES"))
                                            .child(div().text_xs().text_color(rgb(MUTED)).child(
                                                selected_pad.map_or_else(
                                                    || "—".into(),
                                                    |pad| pad.name.clone(),
                                                ),
                                            )),
                                    )
                                    .child(
                                        div()
                                            .id("sampler-zone-list-scroll")
                                            .flex_1()
                                            .min_h_0()
                                            .overflow_y_scroll()
                                            .child(self.render_zone_list(&kit, selected_pad, cx)),
                                    ),
                            ),
                    )
                    .child(self.render_zone_inspector(&kit, selected_zone, cx)),
            )
            .child(
                div()
                    .h(px(27.0))
                    .flex_none()
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL_ALT))
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .text_color(rgb(if feedback.tone == SampleFeedbackTone::Idle {
                        MUTED
                    } else {
                        sampler_feedback_color(feedback.tone)
                    }))
                    .child(if feedback.tone == SampleFeedbackTone::Idle {
                        self.status.clone()
                    } else {
                        feedback.headline
                    })
                    .child("Exact ranges · revision-guarded actions"),
            )
    }
}

fn sampler_drag_modifiers(modifiers: gpui::Modifiers) -> DragModifiers {
    DragModifiers {
        duplicate: modifiers.alt,
        make_unique: modifiers.alt,
        suppress_snap: modifiers.shift,
    }
}

fn material_range(material: SourceMaterialRef, asset: Option<&MediaAsset>) -> AssetFrameRange {
    match material {
        SourceMaterialRef::VirtualSlice(slice) => slice.source_range,
        SourceMaterialRef::Asset(_) => AssetFrameRange {
            start: SampleFrames(0),
            end: asset
                .map(|asset| asset.metadata().frame_count)
                .unwrap_or(SampleFrames(1)),
        },
    }
}

pub fn pad_for_key(kit: &SampleKit, bank: u16, key: &str) -> Option<PadId> {
    let key_index = SAMPLER_KEYBOARD_KEYS
        .iter()
        .position(|candidate| *candidate == key)?;
    let index = usize::from(bank)
        .checked_mul(PAD_KEY_COUNT)?
        .checked_add(key_index)?;
    kit.pad_order.get(index).copied()
}

pub fn reconcile_sampler_state(
    target: SamplerTarget,
    kit: &SampleKit,
    state: &mut SamplerViewState,
) {
    let maximum_bank = kit.pad_order.len().saturating_sub(1) / PAD_KEY_COUNT;
    state.bank = usize::from(state.bank).min(maximum_bank) as u16;
    match target {
        SamplerTarget::NewKit | SamplerTarget::NewPad { .. } => {
            state.selected_pad = None;
            state.selected_zone = None;
            return;
        }
        SamplerTarget::Pad { pad, .. } if kit.pads.contains_key(&pad) => {
            state.selected_pad = Some(pad);
        }
        SamplerTarget::Pad { .. } | SamplerTarget::Kit(_) => {
            if state
                .selected_pad
                .is_none_or(|pad| !kit.pads.contains_key(&pad))
            {
                state.selected_pad = kit.pad_order.first().copied();
            }
        }
    }
    let selected_pad = state.selected_pad;
    if state.selected_zone.is_none_or(|zone| {
        kit.zones
            .get(&zone)
            .is_none_or(|zone| Some(zone.pad) != selected_pad)
    }) {
        state.selected_zone = selected_pad
            .and_then(|pad| kit.pads.get(&pad))
            .and_then(|pad| pad.zone_order.first().copied());
    }
}

fn zone_range_label(material: SourceMaterialRef) -> String {
    match material {
        SourceMaterialRef::Asset(asset) => format!("Asset {} · full source", asset.0),
        SourceMaterialRef::VirtualSlice(slice) => format!(
            "Frames {}–{} · {} frames",
            slice.source_range.start.0,
            slice.source_range.end.0,
            slice.source_range.len().0
        ),
    }
}

fn range_duration_label(range: AssetFrameRange, asset: Option<&MediaAsset>) -> String {
    asset.map_or_else(
        || format!("{} fr", range.len().0),
        |asset| {
            let seconds = range.len().0 as f64 / f64::from(asset.metadata().sample_rate_hz);
            format!("{seconds:.3} s")
        },
    )
}

fn provenance_label(provenance: &SampleMaterialProvenance) -> String {
    match provenance {
        SampleMaterialProvenance::ExistingAsset => "Existing media-pool asset".into(),
        SampleMaterialProvenance::ManualSelection => "Manual exact-range selection".into(),
        SampleMaterialProvenance::OnsetChop { analyzer, evidence } => {
            format!("Onset chop · {analyzer} · {} evidence", evidence.len())
        }
        SampleMaterialProvenance::Deprojection { proposal, evidence } => format!(
            "Deprojection P{} · {:08x} · {} evidence",
            proposal.local,
            proposal.scope.0 as u32,
            evidence.len()
        ),
        SampleMaterialProvenance::AnalysisTemplate { analyzer, evidence } => {
            format!("{analyzer} template · {} evidence", evidence.len())
        }
        SampleMaterialProvenance::Consolidated(record) => format!(
            "Consolidated from asset {} frames {}–{}",
            record.derived_from.source_asset.0,
            record.derived_from.source_range.start.0,
            record.derived_from.source_range.end.0
        ),
    }
}

fn range_bar(range: AssetFrameRange, total: Option<SampleFrames>) -> impl IntoElement {
    let total = total.unwrap_or(range.end).0.max(1);
    let width = 264.0_f32;
    let left = (range.start.0.min(total) as f64 / total as f64) as f32 * width;
    let selected_width =
        ((range.end.0.min(total) - range.start.0.min(total)) as f64 / total as f64) as f32 * width;
    div()
        .relative()
        .w(px(width))
        .h(px(58.0))
        .overflow_hidden()
        .rounded_sm()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(BACKGROUND))
        .children((0..24).map(|index| {
            let height = 10.0 + (((index * 17 + 9) % 31) as f32);
            div()
                .absolute()
                .left(px(5.0 + index as f32 * 10.7))
                .top(px(29.0 - height / 2.0))
                .w(px(3.0))
                .h(px(height))
                .rounded_full()
                .bg(rgba(0x8c98a94a))
        }))
        .child(
            div()
                .absolute()
                .left(px(left))
                .top_0()
                .w(px(selected_width.max(2.0)))
                .h_full()
                .border_l_1()
                .border_r_1()
                .border_color(rgb(CYAN))
                .bg(rgba(0x50d8d725)),
        )
}

fn inspector_value(label: &'static str, value: String) -> impl IntoElement {
    div()
        .h(px(31.0))
        .flex()
        .items_center()
        .justify_between()
        .border_b_1()
        .border_color(rgb(BORDER))
        .child(div().text_xs().text_color(rgb(DIM)).child(label))
        .child(div().text_xs().text_color(rgb(TEXT)).child(value))
}

fn section_label(label: impl Into<SharedString>) -> impl IntoElement {
    div().text_xs().text_color(rgb(CYAN)).child(label.into())
}

fn action_row(id: impl Into<gpui::ElementId>, label: String) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .mt_2()
        .p_2()
        .rounded_sm()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL))
        .text_xs()
        .text_color(rgb(MUTED))
        .cursor_pointer()
        .hover(|style| style.border_color(rgb(CYAN)).text_color(rgb(TEXT)))
        .child(label)
}

fn action_button(
    id: &'static str,
    label: impl Into<SharedString>,
    accent: u32,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(28.0))
        .px_3()
        .flex()
        .items_center()
        .rounded_sm()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL))
        .text_xs()
        .text_color(rgb(accent))
        .cursor_pointer()
        .hover(move |style| style.border_color(rgb(accent)).bg(rgba(0xffffff0c)))
        .child(label.into())
}

fn filter_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(24.0))
        .px_2()
        .flex()
        .items_center()
        .rounded_sm()
        .border_1()
        .border_color(rgb(if selected { CYAN } else { BORDER }))
        .bg(if selected {
            rgba(0x50d8d71c)
        } else {
            rgb(PANEL)
        })
        .text_xs()
        .text_color(rgb(if selected { TEXT } else { MUTED }))
        .cursor_pointer()
        .hover(|style| style.border_color(rgb(CYAN)))
        .child(label.into())
}

fn publication_status(receipt: &SamplePublishedResult) -> String {
    format!(
        "Published revision {} · {} pads · {} zones",
        receipt.revision,
        receipt.created_pads.len(),
        receipt.created_zones.len()
    )
}

fn empty_message(message: &'static str) -> impl IntoElement {
    div().p_4().text_sm().text_color(rgb(DIM)).child(message)
}

fn pad_color(index: usize) -> u32 {
    [CYAN, MAGENTA, AMBER, LIME][index % 4]
}

fn diagnostic_color(severity: SamplerDiagnosticSeverity) -> u32 {
    match severity {
        SamplerDiagnosticSeverity::Info => CYAN,
        SamplerDiagnosticSeverity::Warning => AMBER,
        SamplerDiagnosticSeverity::Error => MAGENTA,
    }
}

fn sampler_feedback_color(tone: SampleFeedbackTone) -> u32 {
    match tone {
        SampleFeedbackTone::Idle => MUTED,
        SampleFeedbackTone::Pending => AMBER,
        SampleFeedbackTone::Success => LIME,
        SampleFeedbackTone::Error => MAGENTA,
    }
}

fn sampler_feedback_background(tone: SampleFeedbackTone) -> u32 {
    match tone {
        SampleFeedbackTone::Idle => 0x00000000,
        SampleFeedbackTone::Pending => 0xf6b76012,
        SampleFeedbackTone::Success => 0xa7d87712,
        SampleFeedbackTone::Error => 0xf172b618,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::AssetId;
    use crate::sample_kit::{SamplePad, SampleRouteIntent, SampleZone};
    use crate::sample_material::VirtualSliceRef;

    fn kit_with_pads(count: u64) -> SampleKit {
        let mut kit = SampleKit::new(
            KitId::from_raw(1),
            "Test kit",
            SampleRouteIntent::new(BusId::from_raw(1)).unwrap(),
        );
        for raw in 1..=count {
            let id = PadId::from_raw(raw);
            kit.pads
                .insert(id, SamplePad::new(id, format!("Pad {raw}")));
            kit.pad_order.push(id);
        }
        kit
    }

    #[test]
    fn range_label_distinguishes_virtual_material_from_a_whole_asset() {
        assert!(zone_range_label(SourceMaterialRef::Asset(AssetId(3))).contains("full source"));
        let slice = VirtualSliceRef {
            source_asset: AssetId(3),
            source_range: AssetFrameRange::new(SampleFrames(20), SampleFrames(80)).unwrap(),
        };
        let label = zone_range_label(SourceMaterialRef::VirtualSlice(slice));
        assert!(label.contains("20–80"));
        assert!(label.contains("60 frames"));
    }

    #[test]
    fn keyboard_banks_map_stably_to_authored_pad_order() {
        let kit = kit_with_pads(20);
        assert_eq!(pad_for_key(&kit, 0, "1"), Some(PadId::from_raw(1)));
        assert_eq!(pad_for_key(&kit, 0, "v"), Some(PadId::from_raw(16)));
        assert_eq!(pad_for_key(&kit, 1, "1"), Some(PadId::from_raw(17)));
        assert_eq!(pad_for_key(&kit, 1, "q"), None);
        assert_eq!(pad_for_key(&kit, 0, "escape"), None);
    }

    #[test]
    fn retargeting_new_pad_clears_selection_and_clamps_bank() {
        let kit = kit_with_pads(4);
        let mut state = SamplerViewState {
            selected_pad: Some(PadId::from_raw(2)),
            selected_zone: Some(ZoneId::from_raw(99)),
            bank: 9,
        };
        reconcile_sampler_state(SamplerTarget::NewPad { kit: kit.id }, &kit, &mut state);
        assert_eq!(state.selected_pad, None);
        assert_eq!(state.selected_zone, None);
        assert_eq!(state.bank, 0);
    }

    #[test]
    fn pad_target_overrides_stale_ephemeral_pad_selection() {
        let kit = kit_with_pads(3);
        let mut state = SamplerViewState {
            selected_pad: Some(PadId::from_raw(1)),
            selected_zone: None,
            bank: 0,
        };
        reconcile_sampler_state(
            SamplerTarget::Pad {
                kit: kit.id,
                pad: PadId::from_raw(3),
            },
            &kit,
            &mut state,
        );
        assert_eq!(state.selected_pad, Some(PadId::from_raw(3)));
    }

    #[test]
    fn create_pattern_from_pads_is_the_instrument_local_next_step() {
        assert_eq!(
            CreatePatternFromPadsIntent::LABEL,
            "Create pattern from pads"
        );
        let empty = kit_with_pads(0);
        assert_eq!(
            CreatePatternFromPadsIntent::from_kit(&empty)
                .unwrap_err()
                .code,
            "sample.empty-pads"
        );

        let mut kit = kit_with_pads(3);
        for raw in 1..=3 {
            let pad = PadId::from_raw(raw);
            let zone = ZoneId::from_raw(raw + 50);
            kit.pads.get_mut(&pad).unwrap().zone_order.push(zone);
            kit.zones.insert(
                zone,
                SampleZone::new(zone, pad, SourceMaterialRef::Asset(AssetId(raw))),
            );
        }
        let intent = CreatePatternFromPadsIntent::from_kit(&kit).unwrap();
        assert_eq!(intent.kit, kit.id);
        assert_eq!(
            intent.result_focus,
            crate::sample_actions::MakeBeatResultFocus::PatternEditor
        );
        assert_ne!(
            intent.result_focus,
            crate::sample_actions::MakeBeatResultFocus::Stay
        );
        let action = SampleAction::CreatePatternFromPads(intent);
        assert_eq!(
            action.kind(),
            crate::sample_actions::SampleActionKind::CreatePatternFromPads
        );
        assert_eq!(
            action.execution_class(),
            crate::sample_actions::SampleActionExecutionClass::Immediate
        );
    }
}
