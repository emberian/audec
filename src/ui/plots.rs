//! Retained-plot drawing helpers for the overview and lenses.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

use gpui::Point;

pub(super) fn section_label(label: &'static str) -> impl IntoElement {
    div().mt_3().text_xs().text_color(rgb(DIM)).child(label)
}

pub(super) fn viz_control(id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .min_w(px(25.0))
        .h(px(25.0))
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(rgb(BORDER))
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .text_color(rgb(MUTED))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(BORDER)).text_color(rgb(TEXT)))
        .child(label)
}

pub(super) fn layer_row(label: &'static str, color: u32, active: bool) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .py_1()
        .text_xs()
        .text_color(if active { rgb(TEXT) } else { rgb(DIM) })
        .child(div().size(px(7.0)).rounded_full().bg(rgb(color)))
        .child(label)
}

pub(super) fn metric(label: &'static str, value: String, color: u32) -> impl IntoElement {
    div()
        .py_2()
        .border_b_1()
        .border_color(rgb(BORDER))
        .flex()
        .items_baseline()
        .justify_between()
        .child(div().text_xs().text_color(rgb(DIM)).child(label))
        .child(div().text_sm().text_color(rgb(color)).child(value))
}

pub(super) fn empty_state(title: &str, detail: &str) -> gpui::AnyElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .px_8()
        .child(div().text_lg().child(title.to_owned()))
        .child(
            div()
                .max_w(px(520.0))
                .text_color(rgb(MUTED))
                .text_center()
                .child(detail.to_owned()),
        )
        .into_any_element()
}

pub(super) fn arrangement_ruler(
    start: f64,
    end: f64,
    viewport: TimelineViewport,
    follow: bool,
    loop_enabled: bool,
    loop_label: String,
    material_scope_label: Option<String>,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let zoom = if viewport.span() == 0 {
        1.0
    } else {
        viewport.total_samples.max(1) as f64 / viewport.span() as f64
    };
    let has_material_scope = material_scope_label.is_some();
    div()
        .h(px(62.0))
        .flex_none()
        .flex()
        .border_b_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL_ALT))
        .child(
            div()
                .w(px(ARRANGEMENT_GUTTER))
                .h_full()
                .flex_none()
                .px_3()
                .border_r_1()
                .border_color(rgb(BORDER))
                .flex()
                .items_center()
                .justify_between()
                .child(div().text_xs().text_color(rgb(MUTED)).child("ARRANGEMENT"))
                .child(
                    div()
                        .text_xs()
                        .text_color(if follow { rgb(CYAN) } else { rgb(DIM) })
                        .child(if follow { "FOLLOW" } else { "FREE" }),
                ),
        )
        .child(
            div()
                .h_full()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(time_ruler_range(start, end))
                .child(
                    div()
                        .id("timeline-material-toolbar")
                        .h(px(34.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .overflow_x_scroll()
                        .gap_1()
                        .px_2()
                        .bg(rgb(PANEL_ALT))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .flex()
                                .flex_none()
                                .items_center()
                                .gap_1()
                                .when_some(material_scope_label, |row, label| {
                                    row.child(
                                        div()
                                            .max_w(px(250.0))
                                            .truncate()
                                            .px_2()
                                            .text_xs()
                                            .text_color(rgb(CYAN))
                                            .child(label),
                                    )
                                })
                                .when(has_material_scope, |row| {
                                    row.child(
                                        viz_control("timeline-make-sample", "Sample").on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.make_sample_from_active_span(cx)
                                            }),
                                        ),
                                    )
                                    .child(viz_control("timeline-slice-kit", "Slice").on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.slice_active_span_to_kit(cx)
                                        }),
                                    ))
                                    .child(
                                        viz_control("timeline-make-beat", "Beat").on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.make_beat_from_active_span(cx)
                                            }),
                                        ),
                                    )
                                }),
                        )
                        .child(div().flex_1().min_w(px(12.0)))
                        .child(
                            div()
                                .flex()
                                .flex_none()
                                .items_center()
                                .gap_1()
                                .child(
                                    div()
                                        .px_2()
                                        .text_xs()
                                        .text_color(if loop_enabled {
                                            rgb(AMBER)
                                        } else {
                                            rgb(DIM)
                                        })
                                        .child(format!(
                                            "{zoom:.1}× · {} · {loop_label}",
                                            if loop_enabled { "LOOP ON" } else { "LOOP OFF" }
                                        )),
                                )
                                .child(viz_control("arrangement-set-loop", "Set loop").on_click(
                                    cx.listener(|this, _, _, cx| this.set_loop_from_selection(cx)),
                                ))
                                .child(viz_control("arrangement-clear-loop", "Clear").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.dispatch_timeline_event(
                                            TimelineInteractionEvent::ClearLoop,
                                            cx,
                                        )
                                    }),
                                ))
                                .child(viz_control("arrangement-zoom-out", "−").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.zoom_timeline(this.playhead_sample(), 2.0, cx)
                                    }),
                                ))
                                .child(viz_control("arrangement-zoom-in", "+").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.zoom_timeline(this.playhead_sample(), 0.5, cx)
                                    }),
                                ))
                                .child(
                                    viz_control("arrangement-fit", "Fit").on_click(
                                        cx.listener(|this, _, _, cx| this.fit_timeline(cx)),
                                    ),
                                )
                                .child(viz_control("arrangement-follow", "Follow").on_click(
                                    cx.listener(|this, _, _, cx| this.follow_timeline(cx)),
                                )),
                        ),
                ),
        )
}

pub(super) fn arrangement_lane(
    label: &'static str,
    detail: &'static str,
    height: Pixels,
    plot: impl IntoElement,
) -> impl IntoElement {
    div()
        .h(height)
        .flex_none()
        .flex()
        .border_b_1()
        .border_color(rgb(BORDER))
        .child(
            div()
                .w(px(ARRANGEMENT_GUTTER))
                .h_full()
                .flex_none()
                .px_3()
                .border_r_1()
                .border_color(rgb(BORDER))
                .bg(rgb(PANEL_ALT))
                .flex()
                .flex_col()
                .justify_center()
                .gap_1()
                .child(div().text_xs().text_color(rgb(TEXT)).child(label))
                .child(div().text_xs().text_color(rgb(DIM)).child(detail)),
        )
        .child(div().relative().h_full().flex_1().min_w_0().child(plot))
}

pub(super) fn range_fractions(
    range: SampleRange,
    viewport: TimelineViewport,
) -> Option<(f32, f32)> {
    let start = range.start.get().max(0) as u64;
    let end = range.end.get().max(0) as u64;
    if end < viewport.start_sample || start > viewport.end_sample {
        return None;
    }
    Some((viewport.fraction_of(start), viewport.fraction_of(end)))
}

pub(super) fn arrangement_overlay(
    timeline_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    playhead: f32,
    selection: Option<(f32, f32)>,
    loop_range: Option<(f32, f32)>,
    loop_enabled: bool,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| {
            *timeline_bounds.lock().unwrap() = Some(bounds);
            bounds
        },
        move |bounds, _, window, _| {
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
            }

            if let Some((start, end)) = selection {
                let left = bounds.origin.x + bounds.size.width * start.min(end);
                let right = bounds.origin.x + bounds.size.width * start.max(end);
                window.paint_quad(quad(
                    Bounds::new(
                        point(left, bounds.origin.y),
                        gpui::size((right - left).max(px(1.0)), bounds.size.height),
                    ),
                    px(0.0),
                    rgba(0x50d8d71f),
                    px(1.0),
                    rgba(0x50d8d7aa),
                    Default::default(),
                ));
            }

            if let Some((start, end)) = loop_range {
                let left = bounds.origin.x + bounds.size.width * start.min(end);
                let right = bounds.origin.x + bounds.size.width * start.max(end);
                let color = if loop_enabled {
                    rgba(0xf6b760ee)
                } else {
                    rgba(0x59657999)
                };
                window.paint_quad(quad(
                    Bounds::new(
                        point(left, bounds.origin.y),
                        gpui::size((right - left).max(px(1.0)), px(4.0)),
                    ),
                    px(1.0),
                    color,
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }

            paint_playhead(bounds, playhead, window);
        },
    )
    .absolute()
    .left(px(ARRANGEMENT_GUTTER))
    .right_0()
    .top_0()
    .bottom_0()
}

pub(super) fn time_ruler_range(start: f64, end: f64) -> impl IntoElement {
    let ticks = 8;
    div()
        .h(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_between()
        .px_2()
        .border_b_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL_ALT))
        .children((0..=ticks).map(|tick| {
            div().text_xs().text_color(rgb(DIM)).child(format_time(
                start + (end - start) * tick as f64 / ticks as f64,
            ))
        }))
}

pub(super) fn cropped_spectrogram(
    image: Arc<Image>,
    time_start: f64,
    time_end: f64,
    frequency_start: f32,
    frequency_end: f32,
) -> impl IntoElement {
    let time_span = (time_end - time_start).max(1.0e-6) as f32;
    let frequency_span = (frequency_end - frequency_start).max(1.0e-6);
    let source_top = 1.0 - frequency_end;
    img(image)
        .absolute()
        .left(relative(-(time_start as f32) / time_span))
        .top(relative(-source_top / frequency_span))
        .w(relative(1.0 / time_span))
        .h(relative(1.0 / frequency_span))
        .object_fit(ObjectFit::Fill)
}

pub(super) fn slice_visible<T: Clone>(values: &[T], start: f64, end: f64) -> Vec<T> {
    if values.is_empty() {
        return Vec::new();
    }
    let first = (start.clamp(0.0, 1.0) * values.len() as f64).floor() as usize;
    let last = (end.clamp(0.0, 1.0) * values.len() as f64).ceil() as usize;
    values[first.min(values.len() - 1)..last.clamp(first + 1, values.len())].to_vec()
}

pub(super) fn lane(
    label: &'static str,
    height: Pixels,
    plot: impl IntoElement,
) -> impl IntoElement {
    div()
        .relative()
        .h(height)
        .flex_none()
        .border_b_1()
        .border_color(rgb(BORDER))
        .child(plot)
        .child(
            div()
                .absolute()
                .top_2()
                .left_2()
                .px_2()
                .py_1()
                .rounded_sm()
                .bg(rgba(0x090b10cc))
                .text_xs()
                .text_color(rgb(MUTED))
                .child(label),
        )
}

pub(super) fn waveform_plot(
    waveform: impl Into<Arc<[WaveformBin]>>,
    playhead: f32,
    geometry_cache: Arc<Mutex<WaveformGeometryCache>>,
    key: WaveformRenderKey,
) -> impl IntoElement {
    let waveform = waveform.into();
    canvas(
        move |bounds, _, _| {
            geometry_cache
                .lock()
                .map(|mut cache| cache.paths(key, &waveform, bounds))
                .unwrap_or_else(|_| {
                    (
                        waveform_envelope(&waveform, bounds, true),
                        waveform_envelope(&waveform, bounds, false),
                    )
                })
        },
        move |_bounds, (left, right), window, _| {
            if let Some(path) = left {
                window.paint_path(path, rgba(0x50d8d7a8));
            }
            if let Some(path) = right {
                window.paint_path(path, rgba(0xf172b69a));
            }
            paint_playhead(_bounds, playhead, window);
        },
    )
    .size_full()
}

pub(super) fn mono_waveform_bins(samples: &[f32], target_bins: usize) -> Vec<WaveformBin> {
    if samples.is_empty() || target_bins == 0 {
        return Vec::new();
    }
    let bin_count = target_bins.min(samples.len());
    (0..bin_count)
        .map(|bin| {
            let start = samples.len() * bin / bin_count;
            let end = samples.len() * (bin + 1) / bin_count;
            let mut minimum = f32::INFINITY;
            let mut maximum = f32::NEG_INFINITY;
            for sample in samples[start..end].iter().copied() {
                minimum = minimum.min(sample);
                maximum = maximum.max(sample);
            }
            WaveformBin {
                left_min: minimum,
                left_max: maximum,
                right_min: minimum,
                right_max: maximum,
            }
        })
        .collect()
}

pub(super) fn waveform_envelope(
    waveform: &[WaveformBin],
    bounds: Bounds<Pixels>,
    left_channel: bool,
) -> Option<gpui::Path<Pixels>> {
    if waveform.len() < 2 {
        return None;
    }
    let center = bounds.origin.y + bounds.size.height * if left_channel { 0.28 } else { 0.72 };
    let amplitude = bounds.size.height * 0.20;
    let mut builder = PathBuilder::fill();
    for (index, bin) in waveform.iter().enumerate() {
        let fraction = index as f32 / (waveform.len() - 1) as f32;
        let value = if left_channel {
            bin.left_max
        } else {
            bin.right_max
        };
        let location = point(
            bounds.origin.x + bounds.size.width * fraction,
            center - amplitude * value.clamp(-1.0, 1.0),
        );
        if index == 0 {
            builder.move_to(location);
        } else {
            builder.line_to(location);
        }
    }
    for (index, bin) in waveform.iter().enumerate().rev() {
        let fraction = index as f32 / (waveform.len() - 1) as f32;
        let value = if left_channel {
            bin.left_min
        } else {
            bin.right_min
        };
        builder.line_to(point(
            bounds.origin.x + bounds.size.width * fraction,
            center - amplitude * value.clamp(-1.0, 1.0),
        ));
    }
    builder.close();
    builder.build().ok()
}

pub(super) fn dual_feature_plot(
    features: Vec<FeatureFrame>,
    playhead: f32,
    first: fn(FeatureFrame) -> f32,
    second: fn(FeatureFrame) -> f32,
    first_color: gpui::Rgba,
    second_color: gpui::Rgba,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            if let Some(path) = feature_area(&features, bounds, first) {
                window.paint_path(path, first_color);
            }
            if let Some(path) = feature_line(&features, bounds, second) {
                window.paint_path(path, second_color);
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .size_full()
}

pub(super) fn feature_plot(
    features: Vec<FeatureFrame>,
    playhead: f32,
    value: fn(FeatureFrame) -> f32,
    color: gpui::Rgba,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            if let Some(path) = feature_area(&features, bounds, value) {
                window.paint_path(path, color);
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .size_full()
}

pub(super) fn rhythm_plot(
    rhythm: RhythmAnalysis,
    start_seconds: f64,
    end_seconds: f64,
    playhead: f32,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            let duration = (end_seconds - start_seconds).max(f64::EPSILON);
            for time in rhythm.beat_times.iter().copied() {
                if time < start_seconds || time > end_seconds {
                    continue;
                }
                let fraction = ((time - start_seconds) / duration) as f32;
                let x = bounds.origin.x + bounds.size.width * fraction;
                window.paint_quad(quad(
                    Bounds::new(
                        point(x, bounds.origin.y),
                        gpui::size(px(1.0), bounds.size.height),
                    ),
                    px(0.0),
                    rgba(0xffffff18),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }

            for onset in rhythm.onsets.iter().copied() {
                if onset.time_seconds < start_seconds || onset.time_seconds > end_seconds {
                    continue;
                }
                let fraction = ((onset.time_seconds - start_seconds) / duration) as f32;
                let x = bounds.origin.x + bounds.size.width * fraction;
                let (row, color) = onset_style(onset);
                let lane_height = bounds.size.height / 3.0;
                let max_height = lane_height * 0.84;
                let height = (max_height * onset.strength.max(0.12)).max(px(2.0));
                let bottom = bounds.origin.y + lane_height * (row as f32 + 0.92);
                window.paint_quad(quad(
                    Bounds::new(
                        point(x - px(1.0), bottom - height),
                        gpui::size(px(2.0), height),
                    ),
                    px(1.0),
                    color,
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .size_full()
}

pub(super) fn rhythm_deprojection_plot(
    rhythm: Arc<RhythmDeprojection>,
    family_ids: Vec<usize>,
    visible_start: usize,
    visible_end: usize,
    playhead: f32,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            let row_height = px(RHYTHM_ROW_HEIGHT);
            for row in 1..=family_ids.len() {
                let y = bounds.origin.y + row_height * row as f32;
                window.paint_quad(quad(
                    Bounds::new(
                        point(bounds.origin.x, y),
                        gpui::size(bounds.size.width, px(1.0)),
                    ),
                    px(0.0),
                    rgba(0xffffff14),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }

            if let Some(phase) = rhythm.beat_phase_hypotheses.first() {
                for sample in phase.beat_samples.iter().copied() {
                    paint_sample_marker(
                        sample,
                        visible_start,
                        visible_end,
                        bounds,
                        px(1.0),
                        rgba(0xffffff18),
                        window,
                    );
                }
            }
            if let Some(downbeats) = rhythm.downbeat_hypotheses.first() {
                for sample in downbeats.downbeat_samples.iter().copied() {
                    paint_sample_marker(
                        sample,
                        visible_start,
                        visible_end,
                        bounds,
                        px(2.0),
                        rgba(0xf6b76055),
                        window,
                    );
                }
            }
            for occurrence in rhythm
                .patterns
                .iter()
                .take(4)
                .flat_map(|pattern| &pattern.occurrences)
            {
                paint_sample_marker(
                    occurrence.start_sample,
                    visible_start,
                    visible_end,
                    bounds,
                    px(1.0),
                    rgba(0xf172b650),
                    window,
                );
            }

            let peak_strength = rhythm
                .hits
                .iter()
                .filter(|hit| spans_overlap(hit.span, visible_start, visible_end))
                .map(|hit| hit.novelty_strength)
                .fold(1.0e-6_f32, f32::max);
            for hit in &rhythm.hits {
                let Some(family_id) = hit.family else {
                    continue;
                };
                let Some(row) = family_ids
                    .iter()
                    .position(|candidate| *candidate == family_id)
                else {
                    continue;
                };
                let Some((start, end)) = clip_sample_span(hit.span, visible_start, visible_end)
                else {
                    continue;
                };
                let x = bounds.origin.x + bounds.size.width * start;
                let right = bounds.origin.x + bounds.size.width * end;
                let width = (right - x).max(px(2.0));
                let strength = (hit.novelty_strength / peak_strength)
                    .sqrt()
                    .clamp(0.18, 1.0);
                let inset = px(5.0 + (1.0 - strength) * 15.0);
                let top = bounds.origin.y + row_height * row as f32 + inset;
                let height = (row_height - inset * 2.0).max(px(3.0));
                window.paint_quad(quad(
                    Bounds::new(point(x, top), gpui::size(width, height)),
                    px(2.0),
                    cluster_rgba(family_id),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .size_full()
}

/// The height the Loom lens gives its event sequence. Named because the
/// pointer half of that plot has to know the geometry the layout declares,
/// not only the geometry a frame happened to paint.
pub(super) const LOOM_EVENT_LANE_HEIGHT: f32 = 150.0;

/// The geometry a press is resolved against, and where it came from.
///
/// A plot records where it was painted; a session that has not drawn a frame
/// (a scripted one draws very few) has no such record. The fall-back is not a
/// guess: these plots are laid out at a declared size -- the rhythm rows are
/// `RHYTHM_ROW_HEIGHT` each up to `RHYTHM_MAX_VISIBLE_FAMILIES`, the Loom
/// sequence is one `LOOM_EVENT_LANE_HEIGHT` lane -- and a press given in
/// fractions maps onto the same row and the same instant either way. Only the
/// origin and the width differ, and neither reaches a fraction.
pub(super) fn press_geometry(
    painted: Option<Bounds<Pixels>>,
    kind: VizKind,
) -> (Bounds<Pixels>, &'static str) {
    if let Some(bounds) = painted {
        return (bounds, "painted");
    }
    let height = match kind {
        VizKind::Loom => LOOM_EVENT_LANE_HEIGHT,
        _ => RHYTHM_ROW_HEIGHT * RHYTHM_MAX_VISIBLE_FAMILIES as f32,
    };
    (
        Bounds::new(point(px(0.0), px(0.0)), gpui::size(px(1_200.0), px(height))),
        "declared",
    )
}

/// One painted rhythm hit, named by the row it was painted in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RhythmPlotHit {
    pub family_id: usize,
    /// Index into `RhythmDeprojection::hits`.
    pub hit_index: usize,
    pub onset_sample: usize,
    pub span: SampleSpan,
}

/// Which painted hit is under `position`, or nothing.
///
/// This is the pointer half of [`rhythm_deprojection_plot`] and reads the
/// same geometry: `family_ids` is one fixed-height row each, in the order the
/// plot draws them, and a hit occupies its clipped span across the full width
/// of its family's row. The painted bar is inset vertically by up to 20 px
/// (weak hits are drawn thin), but the row band belongs to one family alone,
/// so the whole band is the target: aiming at a 3 px bar is not a gesture a
/// musician can make. The topmost match wins, which is the last one painted.
pub(super) fn rhythm_hit_at(
    bounds: Bounds<Pixels>,
    rhythm: &RhythmDeprojection,
    family_ids: &[usize],
    visible_start: usize,
    visible_end: usize,
    position: Point<Pixels>,
) -> Option<RhythmPlotHit> {
    if bounds.size.width <= px(0.0) || family_ids.is_empty() {
        return None;
    }
    if position.x < bounds.origin.x || position.x > bounds.origin.x + bounds.size.width {
        return None;
    }
    let row_height = px(RHYTHM_ROW_HEIGHT);
    let offset = position.y - bounds.origin.y;
    if offset < px(0.0) {
        return None;
    }
    let row = (f32::from(offset) / f32::from(row_height)).floor() as usize;
    let family_id = *family_ids.get(row)?;
    rhythm
        .hits
        .iter()
        .enumerate()
        .rev()
        .find(|(_, hit)| {
            if hit.family != Some(family_id) {
                return false;
            }
            let Some((start, end)) = clip_sample_span(hit.span, visible_start, visible_end) else {
                return false;
            };
            let left = bounds.origin.x + bounds.size.width * start;
            let right = (bounds.origin.x + bounds.size.width * end).max(left + px(2.0));
            (left..=right).contains(&position.x)
        })
        .map(|(hit_index, hit)| RhythmPlotHit {
            family_id,
            hit_index,
            onset_sample: hit.onset_sample,
            span: hit.span,
        })
}

pub(super) fn paint_sample_marker(
    sample: usize,
    visible_start: usize,
    visible_end: usize,
    bounds: Bounds<Pixels>,
    width: Pixels,
    color: gpui::Rgba,
    window: &mut Window,
) {
    if sample < visible_start || sample >= visible_end || visible_end <= visible_start {
        return;
    }
    let fraction = (sample - visible_start) as f32 / (visible_end - visible_start) as f32;
    let x = bounds.origin.x + bounds.size.width * fraction;
    window.paint_quad(quad(
        Bounds::new(
            point(x - width * 0.5, bounds.origin.y),
            gpui::size(width, bounds.size.height),
        ),
        px(0.0),
        color,
        px(0.0),
        rgba(0x00000000),
        Default::default(),
    ));
}

pub(super) fn spans_overlap(span: SampleSpan, visible_start: usize, visible_end: usize) -> bool {
    span.start < visible_end && span.end > visible_start && visible_start < visible_end
}

pub(super) fn clip_sample_span(
    span: SampleSpan,
    visible_start: usize,
    visible_end: usize,
) -> Option<(f32, f32)> {
    if !spans_overlap(span, visible_start, visible_end) {
        return None;
    }
    let length = visible_end.saturating_sub(visible_start).max(1) as f32;
    let start = span.start.max(visible_start).saturating_sub(visible_start) as f32 / length;
    let end = span.end.min(visible_end).saturating_sub(visible_start) as f32 / length;
    Some((start.clamp(0.0, 1.0), end.clamp(0.0, 1.0)))
}

pub(super) fn visible_hit_count(rhythm: &RhythmDeprojection, start: usize, end: usize) -> usize {
    rhythm
        .hits
        .iter()
        .filter(|hit| spans_overlap(hit.span, start, end))
        .count()
}

pub(super) fn visible_rhythm_family_ids(
    rhythm: &RhythmDeprojection,
    start: usize,
    end: usize,
    maximum: usize,
) -> Vec<usize> {
    let mut families = rhythm
        .event_families
        .iter()
        .filter_map(|family| {
            let visible = family
                .event_indices
                .iter()
                .filter(|index| {
                    rhythm
                        .hits
                        .get(**index)
                        .is_some_and(|hit| spans_overlap(hit.span, start, end))
                })
                .count();
            (visible > 0).then_some((family.id, visible, family.evidence))
        })
        .collect::<Vec<_>>();
    families.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.total_cmp(&a.2))
            .then_with(|| a.0.cmp(&b.0))
    });
    families.truncate(maximum);
    families.into_iter().map(|family| family.0).collect()
}

pub(super) fn tempo_hypotheses_summary(rhythm: &RhythmDeprojection) -> String {
    if rhythm.tempo_hypotheses.is_empty() {
        return "No stable tempo hypothesis · the pulse remains ambiguous".to_owned();
    }
    let candidates = rhythm
        .tempo_hypotheses
        .iter()
        .take(4)
        .map(|tempo| {
            let relation = match tempo.relation {
                TempoRelation::Independent => "",
                TempoRelation::HalfTimeOf(_) => " ½-time",
                TempoRelation::DoubleTimeOf(_) => " 2×-time",
            };
            format!(
                "#{rank} {bpm:.1} BPM {evidence:.0}%{relation}",
                rank = tempo.rank + 1,
                bpm = tempo.bpm,
                evidence = tempo.evidence * 100.0
            )
        })
        .collect::<Vec<_>>()
        .join("   ·   ");
    if rhythm.tempo_hypotheses.len() > 4 {
        return format!(
            "Tempo alternatives (4 of {}): {candidates}",
            rhythm.tempo_hypotheses.len()
        );
    }
    format!("Tempo alternatives: {candidates}")
}

/// One painted Loom event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LoomPlotEvent {
    pub event_id: u64,
    pub cluster_id: usize,
    pub sample_index: i64,
}

/// How far from an event's painted stem a press still names it. The stem is
/// 3 px wide; this is the width of a finger, not of the mark.
const LOOM_EVENT_HIT_RADIUS: f32 = 6.0;

/// Which painted event is under `position`, or nothing.
///
/// The pointer half of [`loom_event_plot`], reading the same geometry: one
/// row per cluster over the plot's height, time mapped linearly across its
/// width. Events inside a row can be closer together than a pointer can
/// separate, so the nearest stem within [`LOOM_EVENT_HIT_RADIUS`] wins rather
/// than the first one whose rectangle contains the press.
pub(super) fn loom_event_at(
    bounds: Bounds<Pixels>,
    sketch: &SequenceSketch,
    start_seconds: f64,
    end_seconds: f64,
    position: Point<Pixels>,
) -> Option<LoomPlotEvent> {
    if bounds.size.width <= px(0.0) || bounds.size.height <= px(0.0) {
        return None;
    }
    if position.x < bounds.origin.x || position.x > bounds.origin.x + bounds.size.width {
        return None;
    }
    let duration = (end_seconds - start_seconds).max(f64::EPSILON);
    let rows = sketch.clusters.len().max(1);
    let row_height = bounds.size.height / rows as f32;
    if row_height <= px(0.0) {
        return None;
    }
    let offset = position.y - bounds.origin.y;
    if offset < px(0.0) || offset >= bounds.size.height {
        return None;
    }
    let row = (f32::from(offset) / f32::from(row_height)).floor() as usize;
    let cluster = sketch.clusters.get(row)?;
    let cluster_id = cluster.template.cluster_id;
    sketch
        .events
        .iter()
        .filter(|event| event.cluster_id == cluster_id)
        .filter_map(|event| {
            let seconds = event.sample_index as f64 / f64::from(sketch.sample_rate.max(1));
            if seconds < start_seconds || seconds > end_seconds {
                return None;
            }
            let fraction = ((seconds - start_seconds) / duration) as f32;
            let x = bounds.origin.x + bounds.size.width * fraction;
            let distance = f32::from(position.x - x).abs();
            (distance <= LOOM_EVENT_HIT_RADIUS).then_some((distance, event))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, event)| LoomPlotEvent {
            event_id: event.id,
            cluster_id: event.cluster_id,
            sample_index: event.sample_index,
        })
}

pub(super) fn loom_event_plot(
    sketch: SequenceSketch,
    start_seconds: f64,
    end_seconds: f64,
    playhead: f32,
    selected_cluster_id: usize,
    selected_event: Option<u64>,
    plot_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| {
            // The pointer half of this plot needs to know where it was
            // painted; without this the clicked event could only be guessed.
            if let Ok(mut cell) = plot_bounds.lock() {
                *cell = Some(bounds);
            }
            bounds
        },
        move |bounds, _, window, _| {
            let duration = (end_seconds - start_seconds).max(f64::EPSILON);
            let rows = sketch.clusters.len().max(1);
            let row_height = bounds.size.height / rows as f32;
            if let Some(selected_row) = sketch
                .clusters
                .iter()
                .position(|cluster| cluster.template.cluster_id == selected_cluster_id)
            {
                window.paint_quad(quad(
                    Bounds::new(
                        point(
                            bounds.origin.x,
                            bounds.origin.y + row_height * selected_row as f32,
                        ),
                        gpui::size(bounds.size.width, row_height),
                    ),
                    px(0.0),
                    rgba(0x50d8d70d),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            for row in 1..rows {
                let y = bounds.origin.y + row_height * row as f32;
                window.paint_quad(quad(
                    Bounds::new(
                        point(bounds.origin.x, y),
                        gpui::size(bounds.size.width, px(1.0)),
                    ),
                    px(0.0),
                    rgba(0xffffff12),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            for event in &sketch.events {
                let seconds = event.sample_index as f64 / f64::from(sketch.sample_rate);
                if seconds < start_seconds || seconds > end_seconds {
                    continue;
                }
                let Some(row) = sketch
                    .clusters
                    .iter()
                    .position(|cluster| cluster.template.cluster_id == event.cluster_id)
                else {
                    continue;
                };
                let cluster_enabled = sketch.clusters[row].enabled;
                let fraction = ((seconds - start_seconds) / duration) as f32;
                let x = bounds.origin.x + bounds.size.width * fraction;
                let height = row_height * (0.18 + 0.68 * event.gain.abs().clamp(0.0, 1.6) / 1.6);
                let bottom = bounds.origin.y + row_height * (row as f32 + 0.90);
                let color = if event.enabled && cluster_enabled {
                    cluster_rgba(event.cluster_id)
                } else {
                    rgba(0x59657966)
                };
                let chosen = selected_event == Some(event.id);
                window.paint_quad(quad(
                    Bounds::new(
                        point(x - px(1.5), bottom - height),
                        gpui::size(px(3.0), height),
                    ),
                    px(1.0),
                    color,
                    // The event the edits will land on wears a ring, so
                    // "which one am I editing" is answered by the plot.
                    if chosen { px(1.0) } else { px(0.0) },
                    if chosen {
                        rgba(0xe8edf5dd)
                    } else {
                        rgba(0x00000000)
                    },
                    Default::default(),
                ));
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .size_full()
}

pub(super) fn component_activation_plot(
    decomposition: ComponentDecomposition,
    start: f64,
    end: f64,
    playhead: f32,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            let rows = decomposition.components.len().max(1);
            let row_height = bounds.size.height / rows as f32;
            let first = (start.clamp(0.0, 1.0) * decomposition.frames as f64).floor() as usize;
            let last = (end.clamp(0.0, 1.0) * decomposition.frames as f64).ceil() as usize;
            let first = first.min(decomposition.frames.saturating_sub(1));
            let last = last.clamp(first + 1, decomposition.frames);

            for row in 1..rows {
                let y = bounds.origin.y + row_height * row as f32;
                window.paint_quad(quad(
                    Bounds::new(
                        point(bounds.origin.x, y),
                        gpui::size(bounds.size.width, px(1.0)),
                    ),
                    px(0.0),
                    rgba(0xffffff14),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }

            for (row, component) in decomposition.components.iter().enumerate() {
                let values = &component.activation[first..last];
                if values.len() < 2 {
                    continue;
                }
                let peak = values.iter().copied().fold(0.0_f32, f32::max).max(1.0e-9);
                let top = bounds.origin.y + row_height * row as f32;
                let bottom = top + row_height;
                let mut builder = PathBuilder::fill();
                builder.move_to(point(bounds.origin.x, bottom));
                for (index, value) in values.iter().copied().enumerate() {
                    let fraction = index as f32 / (values.len() - 1) as f32;
                    builder.line_to(point(
                        bounds.origin.x + bounds.size.width * fraction,
                        bottom - row_height * 0.88 * (value / peak).sqrt().clamp(0.0, 1.0),
                    ));
                }
                builder.line_to(point(bounds.origin.x + bounds.size.width, bottom));
                builder.close();
                if let Ok(path) = builder.build() {
                    window.paint_path(path, cluster_rgba(row));
                }
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .size_full()
}

pub(super) fn cluster_color(index: usize) -> gpui::Rgba {
    rgb([
        CYAN, MAGENTA, AMBER, LIME, 0x8e9cff, 0xe99172, 0x78d5a3, 0xd8a7ff,
    ][index % 8])
}

pub(super) fn cluster_rgba(index: usize) -> gpui::Rgba {
    rgba(
        [
            0x50d8d7dd, 0xf172b6dd, 0xf6b760dd, 0xa7d877dd, 0x8e9cffdd, 0xe99172dd, 0x78d5a3dd,
            0xd8a7ffdd,
        ][index % 8],
    )
}

pub(super) fn cluster_spectrum_plot(spectrum: Vec<f32>, color: gpui::Rgba) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            if spectrum.len() < 2 {
                return;
            }
            let peak = spectrum.iter().copied().fold(0.0_f32, f32::max).max(1.0e-8);
            let mut builder = PathBuilder::stroke(px(1.0));
            for (index, value) in spectrum.iter().copied().enumerate() {
                let x = bounds.origin.x
                    + bounds.size.width * index as f32 / (spectrum.len() - 1) as f32;
                let y = bounds.origin.y + bounds.size.height * (1.0 - value / peak);
                if index == 0 {
                    builder.move_to(point(x, y));
                } else {
                    builder.line_to(point(x, y));
                }
            }
            if let Ok(path) = builder.build() {
                window.paint_path(path, color);
            }
        },
    )
    .h(px(8.0))
    .w_full()
}

/// A component's frequency-by-lag gesture as a small heat tile: lags left to
/// right, low frequencies at the bottom, opacity by template magnitude.
pub(super) fn template_gesture_plot(
    template: Vec<f32>,
    frequency_bins: usize,
    template_length: usize,
    color: gpui::Rgba,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            if frequency_bins == 0 || template_length == 0 {
                return;
            }
            let peak = template.iter().copied().fold(0.0_f32, f32::max).max(1.0e-8);
            // Never more rows than pixels: bins that share a pixel row take
            // their maximum, so nothing is lost to sub-pixel snapping.
            let rows =
                (f32::from(bounds.size.height).floor().max(1.0) as usize).min(frequency_bins);
            let cell_w = bounds.size.width / template_length as f32;
            let cell_h = bounds.size.height / rows as f32;
            for row in 0..rows {
                let first = row * frequency_bins / rows;
                let last = ((row + 1) * frequency_bins / rows).max(first + 1);
                for lag in 0..template_length {
                    let value = (first..last)
                        .map(|frequency| template[frequency * template_length + lag])
                        .fold(0.0_f32, f32::max)
                        / peak;
                    if value <= 0.02 {
                        continue;
                    }
                    let x = bounds.origin.x + cell_w * lag as f32;
                    let y = bounds.origin.y + bounds.size.height - cell_h * (row + 1) as f32;
                    let fill = gpui::Rgba {
                        a: color.a * value.clamp(0.0, 1.0),
                        ..color
                    };
                    window.paint_quad(quad(
                        Bounds::new(point(x, y), gpui::size(cell_w, cell_h)),
                        px(0.0),
                        fill,
                        px(0.0),
                        rgba(0x00000000),
                        Default::default(),
                    ));
                }
            }
        },
    )
    .h(px(28.0))
    .w_full()
}

pub(super) fn onset_style(onset: OnsetEvent) -> (usize, gpui::Rgba) {
    if onset.high >= onset.mid && onset.high >= onset.low {
        (0, rgba(0xf6b760dd))
    } else if onset.mid >= onset.low {
        (1, rgba(0xf172b6dd))
    } else {
        (2, rgba(0x50d8d7dd))
    }
}

pub(super) fn feature_area(
    features: &[FeatureFrame],
    bounds: Bounds<Pixels>,
    value: fn(FeatureFrame) -> f32,
) -> Option<gpui::Path<Pixels>> {
    if features.len() < 2 {
        return None;
    }
    let bottom = bounds.origin.y + bounds.size.height;
    let mut builder = PathBuilder::fill();
    builder.move_to(point(bounds.origin.x, bottom));
    for (index, feature) in features.iter().copied().enumerate() {
        let fraction = index as f32 / (features.len() - 1) as f32;
        builder.line_to(point(
            bounds.origin.x + bounds.size.width * fraction,
            bottom - bounds.size.height * value(feature).clamp(0.0, 1.0),
        ));
    }
    builder.line_to(point(bounds.origin.x + bounds.size.width, bottom));
    builder.close();
    builder.build().ok()
}

pub(super) fn feature_line(
    features: &[FeatureFrame],
    bounds: Bounds<Pixels>,
    value: fn(FeatureFrame) -> f32,
) -> Option<gpui::Path<Pixels>> {
    if features.len() < 2 {
        return None;
    }
    let mut builder = PathBuilder::stroke(px(1.5));
    for (index, feature) in features.iter().copied().enumerate() {
        let fraction = index as f32 / (features.len() - 1) as f32;
        let location = point(
            bounds.origin.x + bounds.size.width * fraction,
            bounds.origin.y + bounds.size.height * (1.0 - value(feature).clamp(0.0, 1.0)),
        );
        if index == 0 {
            builder.move_to(location);
        } else {
            builder.line_to(location);
        }
    }
    builder.build().ok()
}

pub(super) fn timeline_overlay(
    timeline_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    playhead: f32,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| {
            *timeline_bounds.lock().unwrap() = Some(bounds);
            bounds
        },
        move |bounds, _, window, _| {
            for fraction in [0.25_f32, 0.5, 0.75] {
                let x = bounds.origin.x + bounds.size.width * fraction;
                window.paint_quad(quad(
                    Bounds::new(
                        point(x, bounds.origin.y),
                        gpui::size(px(1.0), bounds.size.height),
                    ),
                    px(0.0),
                    rgba(0xffffff14),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .absolute()
    .inset_0()
}

pub(super) fn paint_playhead(bounds: Bounds<Pixels>, fraction: f32, window: &mut Window) {
    if !(0.0..=1.0).contains(&fraction) {
        return;
    }
    let x = bounds.origin.x + bounds.size.width * fraction;
    window.paint_quad(quad(
        Bounds::new(
            point(x, bounds.origin.y),
            gpui::size(px(1.0), bounds.size.height),
        ),
        px(0.0),
        rgba(0xe8edf5dd),
        px(0.0),
        rgba(0x00000000),
        Default::default(),
    ));
}

pub(super) fn format_time(seconds: f64) -> String {
    if !seconds.is_finite() {
        return "0:00.0".to_owned();
    }
    let minutes = (seconds / 60.0).floor() as u64;
    let remainder = seconds - minutes as f64 * 60.0;
    format!("{minutes}:{remainder:04.1}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loom::{ClusterTemplate, SequenceCluster, SequenceEvent};
    use crate::rhythm::{EventFamilyHypothesis, HitObservation};

    fn plot_bounds() -> Bounds<Pixels> {
        Bounds::new(point(px(100.0), px(50.0)), gpui::size(px(800.0), px(290.0)))
    }

    fn rhythm_with_two_families() -> RhythmDeprojection {
        let mut rhythm = RhythmDeprojection {
            sample_frames: 1_000,
            ..RhythmDeprojection::default()
        };
        rhythm.hits = vec![
            // family 0, occupying [100, 140) of a 0..1000 view
            HitObservation {
                span: SampleSpan {
                    start: 100,
                    end: 140,
                },
                onset_sample: 102,
                family: Some(0),
                ..HitObservation::default()
            },
            // family 1, occupying [600, 620)
            HitObservation {
                span: SampleSpan {
                    start: 600,
                    end: 620,
                },
                onset_sample: 601,
                family: Some(1),
                ..HitObservation::default()
            },
        ];
        rhythm.event_families = vec![
            EventFamilyHypothesis {
                id: 0,
                event_indices: vec![0],
                ..EventFamilyHypothesis::default()
            },
            EventFamilyHypothesis {
                id: 1,
                event_indices: vec![1],
                ..EventFamilyHypothesis::default()
            },
        ];
        rhythm
    }

    #[test]
    fn a_press_on_a_painted_rhythm_hit_names_that_hit_and_its_row() {
        let rhythm = rhythm_with_two_families();
        let bounds = plot_bounds();
        let rows = [0_usize, 1];
        // The middle of family 0's bar: x = 100 + 800 * 0.12, y inside row 0.
        let hit = rhythm_hit_at(bounds, &rhythm, &rows, 0, 1_000, point(px(196.0), px(70.0)))
            .expect("a press inside the painted bar names it");
        assert_eq!(hit.family_id, 0);
        assert_eq!(hit.hit_index, 0);
        assert_eq!(hit.onset_sample, 102);
        // The same x one row lower belongs to family 1, which has nothing there.
        assert_eq!(
            rhythm_hit_at(
                bounds,
                &rhythm,
                &rows,
                0,
                1_000,
                point(px(196.0), px(50.0 + RHYTHM_ROW_HEIGHT + 4.0)),
            ),
            None,
            "a row owns its family alone"
        );
        // Family 1's bar, in family 1's row.
        let second = rhythm_hit_at(
            bounds,
            &rhythm,
            &rows,
            0,
            1_000,
            point(
                px(100.0 + 800.0 * 0.605),
                px(50.0 + RHYTHM_ROW_HEIGHT + 4.0),
            ),
        )
        .expect("family 1's bar is where the plot paints it");
        assert_eq!(second.hit_index, 1);
        assert_eq!(second.onset_sample, 601);
    }

    #[test]
    fn a_press_on_empty_plot_is_not_a_hit() {
        let rhythm = rhythm_with_two_families();
        let bounds = plot_bounds();
        let rows = [0_usize, 1];
        for position in [
            point(px(500.0), px(70.0)),  // between the two bars, row 0
            point(px(196.0), px(40.0)),  // above the plot
            point(px(50.0), px(70.0)),   // left of the plot
            point(px(980.0), px(70.0)),  // right of the plot
            point(px(196.0), px(400.0)), // below every drawn row
        ] {
            assert_eq!(
                rhythm_hit_at(bounds, &rhythm, &rows, 0, 1_000, position),
                None,
                "{position:?} is not on a painted mark"
            );
        }
        assert_eq!(
            rhythm_hit_at(bounds, &rhythm, &[], 0, 1_000, point(px(196.0), px(70.0))),
            None,
            "a plot with no rows paints nothing to press"
        );
    }

    #[test]
    fn the_rhythm_hit_test_reads_the_visible_window_the_plot_was_given() {
        let rhythm = rhythm_with_two_families();
        let bounds = plot_bounds();
        let rows = [0_usize, 1];
        // Zoomed to [80, 160): family 0's hit now fills a quarter of the plot
        // starting at x = 100 + 800 * 0.25.
        let hit = rhythm_hit_at(bounds, &rhythm, &rows, 80, 160, point(px(350.0), px(70.0)))
            .expect("the same hit, at the zoomed position");
        assert_eq!(hit.hit_index, 0);
        assert_eq!(
            rhythm_hit_at(bounds, &rhythm, &rows, 80, 160, point(px(150.0), px(70.0))),
            None,
            "before the span starts there is nothing painted"
        );
    }

    fn sketch_with_two_clusters() -> SequenceSketch {
        let cluster = |cluster_id: usize| SequenceCluster {
            template: ClusterTemplate {
                cluster_id,
                samples: vec![0.0; 8],
                onset_offset: 0,
                medoid_event_id: 0,
                exemplar_count: 1,
                exemplar_agreement: 1.0,
            },
            enabled: true,
            gain: 1.0,
        };
        let event = |id: u64, cluster_id: usize, sample_index: i64| SequenceEvent {
            id,
            cluster_id,
            sample_index,
            gain: 1.0,
            enabled: true,
            salience: 1.0,
            upstream_similarity: 1.0,
            timing_adjustment: 0,
            template_correlation: 1.0,
        };
        SequenceSketch {
            sample_rate: 1_000,
            clusters: vec![cluster(0), cluster(1)],
            events: vec![
                event(10, 0, 1_000), // 1.0 s
                event(11, 0, 3_000), // 3.0 s
                event(12, 1, 2_000), // 2.0 s
            ],
        }
    }

    #[test]
    fn a_press_on_a_painted_loom_event_names_that_event() {
        let sketch = sketch_with_two_clusters();
        let bounds = plot_bounds();
        // 0..4 s over 800 px: 1.0 s is at x = 100 + 200 = 300; rows are 145 px.
        let event = loom_event_at(bounds, &sketch, 0.0, 4.0, point(px(300.0), px(70.0)))
            .expect("the stem at 1.0 s in cluster 0's row");
        assert_eq!(event.event_id, 10);
        assert_eq!(event.cluster_id, 0);
        assert_eq!(event.sample_index, 1_000);
        // The same x in cluster 1's row has no event; 2.0 s does.
        assert_eq!(
            loom_event_at(bounds, &sketch, 0.0, 4.0, point(px(300.0), px(250.0))),
            None
        );
        let second = loom_event_at(bounds, &sketch, 0.0, 4.0, point(px(500.0), px(250.0)))
            .expect("cluster 1's own event");
        assert_eq!(second.event_id, 12);
        assert_eq!(second.cluster_id, 1);
    }

    #[test]
    fn the_nearest_stem_within_a_fingers_width_wins_and_nothing_else_does() {
        let sketch = sketch_with_two_clusters();
        let bounds = plot_bounds();
        // Four pixels right of the 1.0 s stem: still that stem.
        let near = loom_event_at(bounds, &sketch, 0.0, 4.0, point(px(304.0), px(70.0)))
            .expect("within a finger's width");
        assert_eq!(near.event_id, 10);
        // Twenty pixels away is not a press on anything.
        assert_eq!(
            loom_event_at(bounds, &sketch, 0.0, 4.0, point(px(324.0), px(70.0))),
            None
        );
        // Outside the drawn window there is nothing, even in the right row.
        assert_eq!(
            loom_event_at(bounds, &sketch, 2.5, 4.0, point(px(300.0), px(70.0))),
            None,
            "1.0 s is not inside 2.5-4.0 s"
        );
        // A press past the last row is off the plot.
        assert_eq!(
            loom_event_at(bounds, &sketch, 0.0, 4.0, point(px(300.0), px(345.0))),
            None
        );
    }
}
