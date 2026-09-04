//! The options step the Export command opens before it asks for a path.
//!
//! Bit depth, dither, gain, range, and scope were all reachable in
//! [`crate::export`] and [`crate::render_plan`] long before a musician could
//! choose any of them; this small window is where the choice happens. It owns
//! no project state: the host hands it the scope names and the ranges the
//! transport can currently resolve, and hands back an [`ExportOptions`] only
//! when the musician confirms. Closing the window exports nothing.

use gpui::{
    div, prelude::*, px, rgb, rgba, App, Bounds, Context, FocusHandle, Focusable, IntoElement,
    Render, SharedString, Window, WindowOptions,
};

use crate::export::{ExportOptions, ExportRange};
use crate::render_plan::RenderScope;

const BACKGROUND: u32 = 0x090b10;
const PANEL: u32 = 0x10141d;
const BORDER: u32 = 0x252c38;
const TEXT: u32 = 0xe8edf5;
const MUTED: u32 = 0x8c98a9;
const DIM: u32 = 0x596579;
const CYAN: u32 = 0x50d8d7;

/// One selectable render scope with the two names it needs: what the musician
/// reads, and the token the control socket accepts for the same scope.
#[derive(Clone, Debug, PartialEq)]
pub struct ExportScopeChoice {
    pub scope: RenderScope,
    /// `master`, `bus:7`, `track:3`.
    pub token: String,
    /// `master`, `bus Drums`, `track Kick`.
    pub label: String,
}

/// What the host does with a confirmed choice: prompt for a destination and
/// start the export. The view calls this exactly once and then closes.
pub type ExportOptionsConfirm = Box<dyn FnOnce(ExportOptions, &mut App) + 'static>;

/// The ranges the transport can resolve right now, in seconds, so a range the
/// export would refuse is shown as unavailable instead of failing afterwards.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ExportRangeAvailability {
    pub project: Option<(f64, f64)>,
    pub loop_range: Option<(f64, f64)>,
    pub selection: Option<(f64, f64)>,
}

impl ExportRangeAvailability {
    pub fn seconds(&self, range: ExportRange) -> Option<(f64, f64)> {
        match range {
            ExportRange::Project => self.project,
            ExportRange::Loop => self.loop_range,
            ExportRange::Selection => self.selection,
            ExportRange::Custom { .. } => None,
        }
    }
}

pub struct ExportOptionsView {
    options: ExportOptions,
    scopes: Vec<ExportScopeChoice>,
    ranges: ExportRangeAvailability,
    /// Taken by the first confirmation; a closed or twice-clicked options step
    /// cannot start two exports.
    confirm: Option<ExportOptionsConfirm>,
    focus_handle: FocusHandle,
}

impl ExportOptionsView {
    pub fn new(
        options: ExportOptions,
        scopes: Vec<ExportScopeChoice>,
        ranges: ExportRangeAvailability,
        confirm: ExportOptionsConfirm,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            options,
            scopes,
            ranges,
            confirm: Some(confirm),
            focus_handle: cx.focus_handle(),
        }
    }

    fn scope_label(&self) -> String {
        self.scopes
            .iter()
            .find(|choice| choice.scope == self.options.scope)
            .map_or_else(
                || format!("{:?}", self.options.scope),
                |choice| choice.label.clone(),
            )
    }

    fn summary(&self) -> String {
        self.options
            .summary(&self.scope_label(), self.ranges.seconds(self.options.range))
    }

    fn depth_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = row("Bit depth");
        for (bits, label) in [(16_u16, "16-bit"), (24, "24-bit"), (32, "32-bit float")] {
            row = row.child(
                chip(
                    SharedString::from(format!("export-bits-{bits}")),
                    label,
                    self.options.bits() == bits,
                    true,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.options.set_bits(bits);
                    cx.notify();
                })),
            );
        }
        row
    }

    fn dither_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let float = self.options.bits() == 32;
        let applies = self.options.dither_applies();
        let label = if float {
            "not quantized — dither has nothing to do"
        } else if applies {
            "TPDF dither on"
        } else {
            "TPDF dither off"
        };
        row("Dither").child(
            chip("export-dither", label, applies, !float).on_click(cx.listener(
                move |this, _, _, cx| {
                    if this.options.bits() != 32 {
                        let on = this.options.dither_applies();
                        this.options.set_dither_enabled(!on);
                        cx.notify();
                    }
                },
            )),
        )
    }

    fn gain_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let readout = self.options.gain_db().map_or_else(
            || "non-decibel gain".to_owned(),
            |db| format!("{db:+.1} dB"),
        );
        row("Gain")
            .child(
                chip("export-gain-down", "−0.5", false, true).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.nudge_gain(-0.5);
                        cx.notify();
                    },
                )),
            )
            .child(
                div()
                    .w(px(84.0))
                    .text_xs()
                    .text_color(rgb(TEXT))
                    .child(readout),
            )
            .child(
                chip("export-gain-up", "+0.5", false, true).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.nudge_gain(0.5);
                        cx.notify();
                    },
                )),
            )
            .child(
                chip(
                    "export-gain-unity",
                    "Unity",
                    self.options.gain_db() == Some(0.0),
                    true,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.options.set_gain_db(0.0);
                    cx.notify();
                })),
            )
    }

    fn nudge_gain(&mut self, delta: f64) {
        let db = self.options.gain_db().unwrap_or(0.0) + delta;
        self.options.set_gain_db((db * 10.0).round() / 10.0);
    }

    fn range_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = row("Range");
        for range in [
            ExportRange::Project,
            ExportRange::Loop,
            ExportRange::Selection,
        ] {
            let seconds = self.ranges.seconds(range);
            let label = match seconds {
                Some((start, end)) => format!("{} {start:.1}–{end:.1} s", range.label()),
                None => format!("{} (none)", range.label()),
            };
            row = row.child(
                chip(
                    SharedString::from(format!("export-range-{}", range.label())),
                    label,
                    self.options.range == range,
                    seconds.is_some(),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.ranges.seconds(range).is_some() {
                        this.options.range = range;
                        cx.notify();
                    }
                })),
            );
        }
        row
    }

    fn scope_rows(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = row("Scope").flex_wrap();
        for choice in &self.scopes {
            let scope = choice.scope.clone();
            row = row.child(
                chip(
                    SharedString::from(format!("export-scope-{}", choice.token)),
                    choice.label.clone(),
                    self.options.scope == choice.scope,
                    true,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.options.scope = scope.clone();
                    cx.notify();
                })),
            );
        }
        row
    }
}

impl Focusable for ExportOptionsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ExportOptionsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .child(div().text_sm().child("Export audio"))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child("These settings are chosen before the destination."),
            )
            .child(self.depth_row(cx))
            .child(self.dither_row(cx))
            .child(self.gain_row(cx))
            .child(self.range_row(cx))
            .child(self.scope_rows(cx))
            .child(
                div()
                    .mt_2()
                    .p_2()
                    .rounded_sm()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .text_xs()
                    .text_color(rgb(CYAN))
                    .child(self.summary()),
            )
            .child(
                div()
                    .mt_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        chip("export-cancel", "Cancel", false, true).on_click(cx.listener(
                            |_, _, window, _| {
                                window.remove_window();
                            },
                        )),
                    )
                    .child(
                        chip("export-confirm", "Choose destination…", true, true).on_click(
                            cx.listener(|this, _, window, cx| {
                                let Some(confirm) = this.confirm.take() else {
                                    return;
                                };
                                let options = this.options.clone();
                                window.remove_window();
                                // The destination prompt belongs to the host,
                                // not to a window that is closing.
                                cx.defer(move |cx| confirm(options, cx));
                            }),
                        ),
                    ),
            )
    }
}

/// A small centered window; the options step is a short form, not an editor.
pub fn export_options_window_options(cx: &mut App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::centered(
            None,
            gpui::size(px(560.0), px(400.0)),
            cx,
        ))),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(SharedString::from("audec — Export audio")),
            appears_transparent: true,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn row(label: &'static str) -> gpui::Div {
    div().flex().items_center().gap_2().child(
        div()
            .w(px(96.0))
            .text_xs()
            .text_color(rgb(MUTED))
            .child(label),
    )
}

fn chip(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    active: bool,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    let text = if !enabled {
        DIM
    } else if active {
        TEXT
    } else {
        MUTED
    };
    div()
        .id(id)
        .h(px(26.0))
        .px_2()
        .flex()
        .items_center()
        .rounded_sm()
        .border_1()
        .border_color(rgb(if active && enabled { CYAN } else { BORDER }))
        .bg(if active && enabled {
            rgba(0x50d8d71a)
        } else {
            rgba(0x00000000)
        })
        .text_xs()
        .text_color(rgb(text))
        .when(enabled, |chip| {
            chip.cursor_pointer()
                .hover(|style| style.bg(rgba(0xffffff0c)))
        })
        .child(label.into())
}
