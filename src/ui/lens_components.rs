//! Recurring component (NMF) lens.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

impl Visualizer {
    /// The Component findings this lens published, in the order
    /// `status.findings` lists them.
    ///
    /// This is the one published list — the same one the `finding` verb acts
    /// on and `status.findings` reports. It used to be read from the
    /// deprojection workspace's evidence documents filtered to
    /// `DeprojectionCandidateFreshness::Current`, which is empty for
    /// component evidence (components bind no deprojection candidate), so the
    /// header's Open and Keep never appeared at all while the socket listed
    /// six findings for the same lens.
    pub(super) fn component_findings(
        &self,
        cx: &App,
    ) -> Vec<crate::project_controller::FindingRef> {
        self.workbench
            .read(cx)
            .published_analysis_results()
            .into_iter()
            .filter(|published| {
                matches!(
                    published.result.kind,
                    crate::pane_audio::result_lifecycle::AnalysisResultKind::ComponentMagnitude
                )
            })
            .map(|published| published.result.finding)
            .collect()
    }

    pub(super) fn current_component_finding(
        &self,
        index: usize,
        cx: &App,
    ) -> Option<crate::project_controller::FindingRef> {
        self.component_findings(cx).into_iter().nth(index)
    }

    /// How many Component findings are published. The header's `of N`, the
    /// cursor's bound, and the refusals all read this one count.
    pub(super) fn component_finding_count(&self, cx: &App) -> usize {
        self.component_findings(cx).len()
    }

    /// Move the header's cursor over this lens's published findings. Refuses
    /// at the ends rather than wrapping: a musician stepping through evidence
    /// should be told they have reached the end of it.
    pub(super) fn step_component_finding(
        &mut self,
        delta: i64,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let count = self.component_finding_count(cx);
        if count == 0 {
            return Err(
                "No component finding is published yet, so there is none to step to".into(),
            );
        }
        let current = self.selected_finding.min(count - 1);
        let wanted = current as i64 + delta;
        if wanted < 0 || wanted as usize >= count {
            self.selected_finding = current;
            cx.notify();
            return Err(format!(
                "Finding {} of {count} is the {} component finding published",
                current + 1,
                if delta < 0 { "first" } else { "last" }
            ));
        }
        self.selected_finding = wanted as usize;
        cx.notify();
        Ok(())
    }

    pub(super) fn open_components_finding(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(finding) = self.current_component_finding(index, cx) else {
            let count = self.component_finding_count(cx);
            self.say(
                if count == 0 {
                    "No component finding is published yet · factor the song first, then Open"
                        .into()
                } else {
                    format!(
                        "Finding {} is past the {count} this lens has published",
                        index + 1
                    )
                },
                cx,
            );
            return;
        };
        let source_view = WorkspaceViewId(self.audition_owner.local);
        self.workbench.update(cx, |workbench, cx| {
            workbench.reveal_analysis_finding(source_view, finding, cx)
        });
    }

    pub(super) fn keep_components_finding(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(finding) = self.current_component_finding(index, cx) else {
            let count = self.component_finding_count(cx);
            self.say(
                if count == 0 {
                    "No component finding is published yet · there is nothing to keep".into()
                } else {
                    format!(
                        "Finding {} is past the {count} this lens has published",
                        index + 1
                    )
                },
                cx,
            );
            return;
        };
        let source_view = WorkspaceViewId(self.audition_owner.local);
        self.workbench.update(cx, |workbench, cx| {
            workbench.keep_analysis_finding(source_view, finding, cx)
        });
    }

    /// Which seconds a component owns: select its strongest activation span
    /// and put the playhead at the start of it.
    ///
    /// The span is the longest run of atlas frames where this component is at
    /// least [`crate::analysis::ACTIVATION_SPAN_FLOOR`] of its own peak —
    /// stated here because it is a claim, not a rendering detail, and the
    /// status line repeats it so a musician can judge the answer.
    pub(super) fn select_component_span(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some((activation, template_length, shown, duration, sample_rate)) = ({
            let workbench = self.workbench.read(cx);
            workbench.analysis().and_then(|analysis| {
                let decomposition = analysis.components.as_ref()?;
                let component = decomposition.components.get(index)?;
                Some((
                    component.activation.clone(),
                    decomposition
                        .gestures
                        .as_ref()
                        .map_or(1, |gestures| gestures.template_length),
                    decomposition.components.len(),
                    analysis.duration_seconds,
                    analysis.sample_rate,
                ))
            })
        }) else {
            let shown = self
                .workbench
                .read(cx)
                .analysis()
                .and_then(|analysis| analysis.components.as_ref())
                .map(|decomposition| decomposition.components.len());
            return Err(match shown {
                Some(shown) => format!(
                    "Component C{} is not in the current product; it has {shown}",
                    index + 1
                ),
                None => "No component product is published yet".into(),
            });
        };
        let Some(frames) = crate::analysis::strongest_activation_span(&activation, template_length)
        else {
            return Err(format!(
                "Component C{} never rises above silence, so it owns no seconds",
                index + 1
            ));
        };
        let columns = activation.len().max(1) as f64;
        let start_seconds = duration * frames.start as f64 / columns;
        let end_seconds = duration * frames.end as f64 / columns;
        let to_sample = |seconds: f64| (seconds * f64::from(sample_rate)).round().max(0.0) as u64;
        let Some(range) = TimelineRange::new(
            TimelinePoint(to_sample(start_seconds)),
            TimelinePoint(to_sample(end_seconds)),
        ) else {
            return Err(format!(
                "Component C{}'s strongest stretch is shorter than one sample",
                index + 1
            ));
        };
        let message = format!(
            "Component C{} of {shown} owns {} – {} · the longest stretch its occurrences cover without a gap, counting every onset at or above {:.0}% of its own peak as {template_length} atlas frames of gesture ({} of {} frames)",
            index + 1,
            format_time(start_seconds),
            format_time(end_seconds),
            crate::analysis::ACTIVATION_SPAN_FLOOR * 100.0,
            frames.end - frames.start,
            activation.len()
        );
        self.workbench.update(cx, |workbench, cx| {
            workbench.dispatch_timeline_event(
                TimelineInteractionEvent::ReplaceSelection(Some(range)),
                cx,
            );
            workbench.seek_to(start_seconds, cx);
            workbench.constructive_status = Some(message);
            cx.notify();
        });
        Ok(())
    }

    /// The K and LAG knobs. The question lives on the workbench because the
    /// product does; a refusal comes back so the caller can decide whether it
    /// is a status line (a click) or an error reply (the socket).
    pub(super) fn rank_control(
        &mut self,
        delta: i64,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.change_component_question(delta, 0, cx)
    }

    pub(super) fn template_length_control(
        &mut self,
        delta: i64,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.change_component_question(0, delta, cx)
    }

    fn change_component_question(
        &mut self,
        rank: i64,
        template: i64,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let outcome = self.workbench.update(cx, |workbench, cx| {
            workbench.adjust_component_params(rank, template, cx)
        });
        cx.notify();
        outcome
    }

    /// A header press: the same knob, with the refusal shown where a person
    /// is looking rather than returned to nobody.
    fn press_component_knob(&mut self, rank: i64, template: i64, cx: &mut Context<Self>) {
        if let Err(refusal) = self.change_component_question(rank, template, cx) {
            self.say(refusal, cx);
        }
    }

    fn press_finding_cursor(&mut self, delta: i64, cx: &mut Context<Self>) {
        if let Err(refusal) = self.step_component_finding(delta, cx) {
            self.say(refusal, cx);
        }
    }

    pub(super) fn refresh_components(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let outcome = self
            .workbench
            .update(cx, |workbench, cx| workbench.refactor_components(cx));
        cx.notify();
        outcome
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
        let shown_count = components.len();
        let component_count = components.len().max(1);
        let gestures = decomposition.gestures.clone();
        let frequency_bins = decomposition.frequency_bins;
        let finding_count = self.component_finding_count(cx);
        let selected_finding = self.selected_finding.min(finding_count.saturating_sub(1));
        let (asked_rank, asked_template, template_seconds, refactoring) = {
            let workbench = self.workbench.read(cx);
            let params = workbench.component_params;
            (
                params.rank,
                params.template_length,
                workbench.component_template_seconds(),
                workbench.component_analysis_pending,
            )
        };

        // Built before the tree: each row carries its own click listener, and
        // `cx` cannot be moved into a closure that the element tree is still
        // going to borrow.
        let component_rows: Vec<gpui::AnyElement> = components
            .into_iter()
            .enumerate()
            .map(|(index, component)| {
                div()
                    .id(("component-row", index))
                    .h(relative(1.0 / component_count as f32))
                    .px_2()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(BORDER)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Err(refusal) = this.select_component_span(index, cx) {
                            this.say(refusal, cx);
                        }
                    }))
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
                    .children(gestures.as_ref().map(|gestures| {
                        template_gesture_plot(
                            gestures.template(index, frequency_bins).to_vec(),
                            frequency_bins,
                            gestures.template_length,
                            cluster_rgba(index),
                        )
                    }))
                    .child(cluster_spectrum_plot(
                        component.spectral_template,
                        cluster_rgba(index),
                    ))
                    .into_any_element()
            })
            .collect();

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
                            .child(format!("{shown_count} components")),
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
                            .child(viz_control("previous-components-finding", "◂").on_click(
                                cx.listener(|this, _, _, cx| this.press_finding_cursor(-1, cx)),
                            ))
                            .child(
                                viz_control("open-components-finding", "Open Finding")
                                    .px_2()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.open_components_finding(selected_finding, cx)
                                    })),
                            )
                            .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                                "{} of {finding_count}",
                                selected_finding + 1
                            )))
                            .child(viz_control("next-components-finding", "▸").on_click(
                                cx.listener(|this, _, _, cx| this.press_finding_cursor(1, cx)),
                            ))
                            .child(
                                viz_control("keep-components-finding", "Keep finding")
                                    .px_2()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.keep_components_finding(selected_finding, cx)
                                    })),
                            )
                    }),
            )
            .child(
                // The question, not the answer: what a Refactor would ask of
                // the atlas. It is deliberately a separate act from turning
                // the knob, because it re-factors the whole song.
                div()
                    .h(px(34.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_4()
                    .gap_1()
                    .bg(rgb(PANEL_ALT))
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(div().text_xs().text_color(rgb(DIM)).child("ASK"))
                    .child(
                        viz_control("components-rank-down", "K−").on_click(
                            cx.listener(|this, _, _, cx| this.press_component_knob(-1, 0, cx)),
                        ),
                    )
                    .child(
                        div()
                            .min_w(px(86.0))
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!("{asked_rank} components")),
                    )
                    .child(
                        viz_control("components-rank-up", "K+").on_click(
                            cx.listener(|this, _, _, cx| this.press_component_knob(1, 0, cx)),
                        ),
                    )
                    .child(div().w(px(12.0)))
                    .child(viz_control("components-lag-down", "LAG−").on_click(cx.listener(
                        |this, _, _, cx| this.press_component_knob(0, -1, cx),
                    )))
                    .child(
                        div()
                            .min_w(px(126.0))
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(match template_seconds {
                                Some(seconds) => {
                                    format!("{asked_template} frames · {seconds:.2} s")
                                }
                                None => format!("{asked_template} frames"),
                            }),
                    )
                    .child(viz_control("components-lag-up", "LAG+").on_click(cx.listener(
                        |this, _, _, cx| this.press_component_knob(0, 1, cx),
                    )))
                    .child(div().w(px(12.0)))
                    .child(
                        viz_control("refactor-components", "Refactor")
                            .px_2()
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Err(refusal) = this.refresh_components(cx) {
                                    this.say(refusal, cx);
                                }
                            })),
                    )
                    .child(div().flex_1())
                    .child(div().text_xs().text_color(rgb(DIM)).child(if refactoring {
                        "Refactoring the whole song…"
                    } else {
                        "K / LAG change the question · Refactor re-factors the whole song"
                    })),
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
                            .children(component_rows),
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
                    .child("NMF factors recurring mixed-audio magnitude shapes. These are evidence-only: phase was not retained, so audec will not pretend they are auditionable isolated sources or instrument labels. Click a component to select the seconds it owns."),
            )
            .into_any_element()
    }
}
