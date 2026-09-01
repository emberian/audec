//! Recurring component (NMF) lens.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

impl Visualizer {
    pub(super) fn current_component_finding(
        &self,
        index: usize,
        cx: &App,
    ) -> Option<crate::project_controller::FindingRef> {
        self.workbench
            .read(cx)
            .session
            .read(cx)
            .list_analysis_evidence_findings()
            .ok()?
            .into_iter()
            .filter(|summary| {
                summary.finding.kind == FindingKind::Components
                    && summary.freshness == DeprojectionCandidateFreshness::Current
            })
            .nth(index)
            .map(|summary| summary.finding)
    }

    pub(super) fn open_components_finding(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(finding) = self.current_component_finding(index, cx) else {
            return;
        };
        let source_view = WorkspaceViewId(self.audition_owner.local);
        self.workbench.update(cx, |workbench, cx| {
            workbench.reveal_analysis_finding(source_view, finding, cx)
        });
    }

    pub(super) fn keep_components_finding(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(finding) = self.current_component_finding(index, cx) else {
            return;
        };
        let source_view = WorkspaceViewId(self.audition_owner.local);
        self.workbench.update(cx, |workbench, cx| {
            workbench.keep_analysis_finding(source_view, finding, cx)
        });
    }

    pub(super) fn render_components(
        &self,
        analysis: Arc<Analysis>,
        playhead: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let timeline_bounds = self.timeline_bounds.clone();
        let start_seconds = analysis.duration_seconds * self.time_start;
        let end_seconds = analysis.duration_seconds * self.time_end;
        let Some(decomposition) = analysis.components.clone() else {
            let pending = self.workbench.read(cx).component_analysis_pending;
            return empty_state(
                if pending {
                    "Factoring recurring mixed-signal components…"
                } else {
                    "No component product is available"
                },
                "The waveform, transport, spectrum, rhythm, sampling, and editors are already usable. This iterative evidence product publishes here when ready.",
            );
        };
        let components = decomposition.components.clone();
        let component_count = components.len().max(1);
        let finding_count = self
            .workbench
            .read(cx)
            .session
            .read(cx)
            .list_analysis_evidence_findings()
            .ok()
            .map(|summaries| {
                summaries
                    .iter()
                    .filter(|summary| {
                        summary.finding.kind == FindingKind::Components
                            && summary.freshness == DeprojectionCandidateFreshness::Current
                    })
                    .count()
            })
            .unwrap_or(0);

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(38.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_4()
                    .gap_4()
                    .bg(rgb(PANEL_ALT))
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_color(rgb(CYAN))
                            .child(format!("{} components", components.len())),
                    )
                    .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                        "{:.0}% explained energy  ·  {:.1}% relative magnitude error  ·  {} iterations",
                        decomposition.explained_energy * 100.0,
                        decomposition.relative_error * 100.0,
                        decomposition.iterations_run
                    )))
                    .child(div().flex_1())
                    .when(finding_count > 0, |header| {
                        header
                            .child(
                                viz_control("open-components-finding", "Open Findings")
                                    .px_2()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.open_components_finding(0, cx)
                                    })),
                            )
                            .child(
                                viz_control("keep-components-finding", "Keep finding")
                                    .px_2()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.keep_components_finding(0, cx)
                                    })),
                            )
                    }),
            )
            .child(time_ruler_range(start_seconds, end_seconds))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(360.0))
                    .flex()
                    .child(
                        div()
                            .w(px(210.0))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .bg(rgb(PANEL_ALT))
                            .border_r_1()
                            .border_color(rgb(BORDER))
                            .children(components.into_iter().enumerate().map(
                                move |(index, component)| {
                                    div()
                                        .h(relative(1.0 / component_count as f32))
                                        .px_2()
                                        .flex()
                                        .flex_col()
                                        .justify_center()
                                        .border_b_1()
                                        .border_color(rgb(BORDER))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(cluster_color(index))
                                                .child(format!("Component C{}", index + 1)),
                                        )
                                        .child(div().text_xs().text_color(rgb(DIM)).child(format!(
                                            "{:.1}% energy · {:.0}% distinct",
                                            component.energy_share * 100.0,
                                            component.spectral_distinctness * 100.0,
                                        )))
                                        .child(cluster_spectrum_plot(
                                            component.spectral_template,
                                            cluster_rgba(index),
                                        ))
                                },
                            )),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .h_full()
                            .cursor_crosshair()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                    this.seek_from_pointer(event, cx)
                                }),
                            )
                            .child(component_activation_plot(
                                decomposition,
                                self.time_start,
                                self.time_end,
                                playhead,
                            ))
                            .child(timeline_overlay(timeline_bounds, playhead)),
                    ),
            )
            .child(
                div()
                    .h(px(34.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_4()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child("NMF factors recurring mixed-audio magnitude shapes. These are evidence-only: phase was not retained, so audec will not pretend they are auditionable isolated sources or instrument labels."),
            )
            .into_any_element()
    }
}
