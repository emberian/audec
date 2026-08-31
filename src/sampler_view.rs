//! Musician-facing GPUI pad bank and sample-zone inspector.
//!
//! The editor reads the shared sample-kit library on every render. It owns
//! only ephemeral selection and emits [`SampleAction`](crate::sample_actions::SampleAction)
//! values for every audible or authored consequence, keeping command history,
//! ID allocation, undo, and constructive planning in the project controller.

use std::sync::{Arc, Mutex};

use gpui::{
    div, prelude::*, px, rgb, rgba, App, Context, FocusHandle, Focusable, IntoElement, MouseButton,
    MouseDownEvent, MouseUpEvent, Render, SharedString, Window,
};

use crate::assets::{AssetFrameRange, AssetRegistry, MediaAsset, SampleFrames};
use crate::mixer::BusId;
use crate::sample_actions::{
    SampleAction, SampleActionCallback, SampleAuditionIntent, SampleInspectTarget,
};
use crate::sample_kit::{KitId, PadId, SampleKit, SampleKitLibrary, SamplePad, SampleZone, ZoneId};
use crate::sample_material::SampleMaterialProvenance;
use crate::sample_material::SourceMaterialRef;
use crate::ui_drag::{interpret_drop, AssetDrag, DragModifiers, DragPayload, DropTarget};

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SamplerBusOption {
    pub id: BusId,
    pub name: String,
}

pub type SamplerBusProvider = Arc<dyn Fn() -> Vec<SamplerBusOption> + Send + Sync + 'static>;

/// Authoritative inputs for a pad editor. `buses` is a live projection
/// callback rather than a cached mixer copy.
#[derive(Clone)]
pub struct SamplerViewSource {
    pub kits: Arc<Mutex<SampleKitLibrary>>,
    pub assets: Arc<Mutex<AssetRegistry>>,
    pub kit: KitId,
    pub buses: SamplerBusProvider,
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
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SamplerViewState {
    pub selected_pad: Option<PadId>,
    pub selected_zone: Option<ZoneId>,
}

pub struct SamplerView {
    source: SamplerViewSource,
    callback: Option<SampleActionCallback>,
    state: SamplerViewState,
    pressed_pad: Option<PadId>,
    focus_handle: FocusHandle,
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
            source,
            callback,
            state: SamplerViewState::default(),
            pressed_pad: None,
            focus_handle: cx.focus_handle(),
            status: "Ready · drag a sample or exact selection onto a pad".into(),
        };
        view.reconcile_selection();
        view
    }

    pub fn source(&self) -> &SamplerViewSource {
        &self.source
    }

    pub fn state(&self) -> SamplerViewState {
        self.state
    }

    pub fn set_callback(&mut self, callback: Option<SampleActionCallback>) {
        self.callback = callback;
    }

    pub fn set_kit(&mut self, kit: KitId, cx: &mut Context<Self>) {
        self.release_pressed_pad(cx);
        self.source.kit = kit;
        self.state = SamplerViewState::default();
        self.reconcile_selection();
        cx.notify();
    }

    pub fn set_state(&mut self, state: SamplerViewState, cx: &mut Context<Self>) {
        self.state = state;
        self.reconcile_selection();
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
        let target = DropTarget::SamplerPad {
            kit: self.source.kit,
            pad,
        };
        match interpret_drop(DragPayload::Asset(source), target, modifiers) {
            Ok(intent) => {
                self.state.selected_pad = Some(pad);
                self.state.selected_zone = None;
                self.status = match source.source_range {
                    Some(range) => format!(
                        "Mapping frames {}–{} to pad {}",
                        range.start.0,
                        range.end.0,
                        pad.get()
                    ),
                    None => format!("Mapping asset {} to pad {}", source.asset.0, pad.get()),
                };
                self.emit(SampleAction::ApplyDrop(intent));
            }
            Err(error) => self.status = format!("Drop refused: {error}"),
        }
        cx.notify();
    }

    fn emit(&self, action: SampleAction) {
        if let Some(callback) = self.callback.as_ref() {
            callback(action);
        }
    }

    fn kit_snapshot(&self) -> Option<SampleKit> {
        self.source
            .kits
            .lock()
            .ok()
            .and_then(|library| library.kits.get(&self.source.kit).cloned())
    }

    fn reconcile_selection(&mut self) {
        let Some(kit) = self.kit_snapshot() else {
            self.state = SamplerViewState::default();
            return;
        };
        if self
            .state
            .selected_pad
            .is_none_or(|pad| !kit.pads.contains_key(&pad))
        {
            self.state.selected_pad = kit.pad_order.first().copied();
        }
        let selected_pad = self.state.selected_pad;
        if self.state.selected_zone.is_none_or(|zone| {
            kit.zones
                .get(&zone)
                .is_none_or(|zone| Some(zone.pad) != selected_pad)
        }) {
            self.state.selected_zone = selected_pad
                .and_then(|pad| kit.pads.get(&pad))
                .and_then(|pad| pad.zone_order.first().copied());
        }
    }

    fn select_pad(&mut self, pad: PadId, cx: &mut Context<Self>) {
        self.state.selected_pad = Some(pad);
        self.state.selected_zone = self
            .kit_snapshot()
            .and_then(|kit| kit.pads.get(&pad).cloned())
            .and_then(|pad| pad.zone_order.first().copied());
        self.status = format!("Selected pad {}", pad.get());
        cx.notify();
    }

    fn audition_pad(&mut self, pad: PadId, pressed: bool, cx: &mut Context<Self>) {
        if pressed {
            self.pressed_pad = Some(pad);
        } else if self.pressed_pad != Some(pad) {
            return;
        } else {
            self.pressed_pad = None;
        }
        self.emit(SampleAction::Audition(SampleAuditionIntent::PadGate {
            kit: self.source.kit,
            pad,
            velocity: 1.0,
            pressed,
        }));
        self.status = if pressed {
            format!("Auditioning pad {}", pad.get())
        } else {
            "Ready".into()
        };
        cx.notify();
    }

    fn release_pressed_pad(&mut self, cx: &mut Context<Self>) {
        if let Some(pad) = self.pressed_pad {
            self.audition_pad(pad, false, cx);
        }
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
        self.emit(SampleAction::SetKitOutput {
            kit: kit.id,
            bus,
            expected_revision: kit.revision,
        });
        self.status = format!("Routing request sent to bus {bus}");
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
        let selected = self.state.selected_pad == Some(id);
        let zones = pad.zone_order.len();
        let primary = kit.ordered_zones(id).next();
        let material = primary
            .map(|zone| self.material_label(zone.material))
            .unwrap_or_else(|| "DROP SAMPLE".into());
        let accent = pad_color(ordinal);
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
            .border_color(rgb(if selected { accent } else { BORDER }))
            .bg(if selected {
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
            .on_drop(cx.listener(move |this, source: &AssetDrag, _, cx| {
                this.accept_asset_drop(*source, id, DragModifiers::default(), cx)
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.select_pad(id, cx);
                    this.audition_pad(id, true, cx);
                }),
            )
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
                            .child(format!("{zones}Z")),
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
            let selected = self.state.selected_zone == Some(*id);
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
                        this.state.selected_zone = Some(zone_id);
                        this.status = format!("Selected zone {}", zone_id.get());
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
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.emit(SampleAction::Inspect(SampleInspectTarget::Evidence(
                            evidence,
                        )));
                    })),
                );
            }
        }
        let provenance = provenance_label(&zone.provenance);
        let provenance_target = zone.provenance.clone();
        let material = zone.material;
        let zone_id = zone.id;
        let pad_id = zone.pad;
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
                    .child(inspector_value("PAN", format!("{:+.2}", zone.pan)))
                    .child(inspector_value(
                        "TUNING",
                        format!("{:+.0} cents", zone.tuning_cents),
                    ))
                    .child(div().mt_5().child(section_label("PROVENANCE")).child(
                        action_row("zone-provenance", provenance).on_click(cx.listener(
                            move |this, _, _, _| {
                                this.emit(SampleAction::Inspect(SampleInspectTarget::Provenance(
                                    provenance_target.clone(),
                                )));
                            },
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
                        this.emit(SampleAction::RemoveZone {
                            kit: this.source.kit,
                            pad: pad_id,
                            zone: zone_id,
                            expected_revision: revision,
                        });
                        this.status = format!("Remove request sent for zone {}", zone_id.get());
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.reconcile_selection();
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
                        .child(format!("Kit {}", self.source.kit.get())),
                );
        };
        let buses = (self.source.buses)();
        let output_name = buses
            .iter()
            .find(|candidate| candidate.id == kit.output.bus)
            .map(|candidate| candidate.name.clone())
            .unwrap_or_else(|| format!("Bus {}", kit.output.bus));
        let selected_pad = self.state.selected_pad.and_then(|id| kit.pads.get(&id));
        let selected_zone = self.state.selected_zone.and_then(|id| kit.zones.get(&id));

        let mut pads = div().flex().flex_wrap().gap_3();
        if kit.pad_order.is_empty() {
            pads = pads.child(empty_message(
                "This kit has no authored pads yet · add pads from a controller action",
            ));
        } else {
            for (index, pad) in kit.ordered_pads().enumerate() {
                pads = pads.child(self.render_pad(&kit, pad, index, cx));
            }
        }

        let output_kit = kit.clone();
        div()
            .key_context("AudecSampler")
            .track_focus(&self.focus_handle)
            .capture_any_mouse_up(
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.release_pressed_pad(cx)),
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
                            .child(div().text_xs().text_color(rgb(DIM)).child("TARGET BUS"))
                            .child(action_button("sampler-output", output_name, LIME).on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.cycle_output(&output_kit, cx)
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
                                    .child(section_label("PAD BANK · HOLD TO AUDITION"))
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
                    .child(self.status.clone())
                    .child("Exact ranges · revision-guarded actions"),
            )
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

fn empty_message(message: &'static str) -> impl IntoElement {
    div().p_4().text_sm().text_color(rgb(DIM)).child(message)
}

fn pad_color(index: usize) -> u32 {
    [CYAN, MAGENTA, AMBER, LIME][index % 4]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::AssetId;
    use crate::sample_material::VirtualSliceRef;

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
}
