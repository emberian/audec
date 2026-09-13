//! Harmonic / transient decomposition lens.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;
use crate::hpss::{span_frames, SPAN_LADDER_SECONDS};
use crate::streaming_media::CacheBudgets;

/// The span a Separation lens reads when nothing has been remembered. It is
/// the bound the lens used to hardcode; it is now the starting rung of a
/// ladder whose reachable rungs are decided by memory.
pub(super) const DEFAULT_HPSS_SPAN_SECONDS: f64 = 30.0;

/// The ceiling a separation's peak allocation is held under: the same budget
/// the render-product catalog is held under, so the analysis half and the
/// audio half are bounded by one number rather than two opinions.
pub(super) fn hpss_memory_budget_bytes() -> u64 {
    CacheBudgets::for_render_products().memory_bytes
}

fn format_megabytes(bytes: u64) -> String {
    format!("{:.0} MB", bytes as f64 / (1024.0 * 1024.0))
}

impl Visualizer {
    /// The spans this lens can offer for this material: every rung of the
    /// ladder whose peak allocation fits the budget, at the kernels currently
    /// chosen. A lower sample rate buys more seconds; a wider kernel does not
    /// change the cost, and says so by leaving the ladder alone.
    pub(super) fn hpss_span_choices(&self, sample_rate: u32) -> Vec<f64> {
        let budget = hpss_memory_budget_bytes();
        SPAN_LADDER_SECONDS
            .iter()
            .copied()
            .filter(|seconds| self.hpss_settings.span_fits(sample_rate, *seconds, budget))
            .collect()
    }

    /// The span this lens will actually read, which is the chosen limit held
    /// down to what fits. A remembered 120 s from another machine's material
    /// must not silently become an allocation this one cannot make.
    pub(super) fn hpss_span_seconds(&self, sample_rate: u32) -> f64 {
        let choices = self.hpss_span_choices(sample_rate);
        match choices
            .iter()
            .copied()
            .find(|seconds| (*seconds - self.hpss_span_limit_seconds).abs() < 0.5)
        {
            Some(seconds) => seconds,
            // Nothing on the ladder matches the remembered limit: take the
            // longest that fits, and the shortest rung if none does (the
            // refusal is then the clamp notice, which names the bytes).
            None => choices
                .iter()
                .copied()
                .filter(|seconds| *seconds <= self.hpss_span_limit_seconds)
                .next_back()
                .or_else(|| choices.first().copied())
                .unwrap_or(SPAN_LADDER_SECONDS[0]),
        }
    }

    pub(super) fn step_hpss_median(
        &mut self,
        time_steps: i32,
        frequency_steps: i32,
        cx: &mut Context<Self>,
    ) {
        let before = self.hpss_settings;
        let after = before
            .step_time_median_width(time_steps)
            .step_frequency_median_width(frequency_steps);
        if after == before {
            let axis = if time_steps != 0 { "H" } else { "P" };
            let bound = if time_steps.max(frequency_steps) > 0 {
                crate::hpss::MEDIAN_WIDTH_MAXIMUM
            } else {
                crate::hpss::MEDIAN_WIDTH_MINIMUM
            };
            self.say(
                format!(
                    "Separation kernel {axis} is already at {bound} · widths run {}–{} in steps of {}, and stay odd",
                    crate::hpss::MEDIAN_WIDTH_MINIMUM,
                    crate::hpss::MEDIAN_WIDTH_MAXIMUM,
                    crate::hpss::MEDIAN_WIDTH_STEP
                ),
                cx,
            );
            return;
        }
        if let Err(error) = after.validate() {
            self.say(format!("Separation refused those kernels · {error}"), cx);
            return;
        }
        self.hpss_settings = after;
        self.remember_separation_choices();
        self.say(
            format!(
                "Separation kernels H {} · P {} · press Analyze view to read the span again with them",
                after.time_median_width, after.frequency_median_width
            ),
            cx,
        );
        cx.notify();
    }

    pub(super) fn step_hpss_span(&mut self, steps: i32, cx: &mut Context<Self>) {
        let sample_rate = self
            .workbench
            .read(cx)
            .analysis()
            .map_or(0, |analysis| analysis.sample_rate);
        if sample_rate == 0 {
            self.say(
                "Separation has no material, so there is no span to lengthen".to_owned(),
                cx,
            );
            return;
        }
        let choices = self.hpss_span_choices(sample_rate);
        let current = self.hpss_span_seconds(sample_rate);
        let index = choices
            .iter()
            .position(|seconds| (*seconds - current).abs() < 0.5)
            .unwrap_or(0);
        let wanted = index as i64 + i64::from(steps);
        if wanted < 0 {
            self.say(
                format!(
                    "Separation is already reading its shortest span, {:.0} s",
                    choices.first().copied().unwrap_or(current)
                ),
                cx,
            );
            return;
        }
        let Some(next) = choices.get(wanted as usize).copied() else {
            // The ladder ran out because the next rung does not fit, not
            // because someone chose a constant. Say the number.
            let refused = SPAN_LADDER_SECONDS
                .iter()
                .copied()
                .find(|seconds| *seconds > current)
                .unwrap_or(current);
            let cost = self
                .hpss_settings
                .peak_bytes(span_frames(sample_rate, refused));
            self.say(
                format!(
                    "Separation will not read {refused:.0} s · it would hold {} at its widest moment and the budget is {}",
                    format_megabytes(cost),
                    format_megabytes(hpss_memory_budget_bytes())
                ),
                cx,
            );
            return;
        };
        self.hpss_span_limit_seconds = next;
        self.remember_separation_choices();
        let cost = self
            .hpss_settings
            .peak_bytes(span_frames(sample_rate, next));
        self.say(
            format!(
                "Separation will read {next:.0} s around the playhead · {} at its widest moment, against a budget of {} · press Analyze view",
                format_megabytes(cost),
                format_megabytes(hpss_memory_budget_bytes())
            ),
            cx,
        );
        cx.notify();
    }

    fn remember_separation_choices(&self) {
        let settings = self.hpss_settings;
        let span_seconds = self.hpss_span_limit_seconds;
        if let Err(error) = crate::preferences::update(|preferences| {
            preferences.separation = Some(crate::preferences::SeparationChoices {
                time_median_width: settings.time_median_width,
                frequency_median_width: settings.frequency_median_width,
                span_seconds,
            });
        }) {
            eprintln!("separation choices not remembered: {error}");
        }
    }

    pub(super) fn open_hpss_finding(&mut self, index: usize, cx: &mut Context<Self>) {
        let HpssViewState::Ready(result) = &self.hpss_state else {
            return;
        };
        let Some(summary) = result.findings.get(index) else {
            return;
        };
        let finding = summary.finding;
        let source_view = WorkspaceViewId(self.audition_owner.local);
        self.workbench.update(cx, |workbench, cx| {
            workbench.reveal_analysis_finding(source_view, finding, cx)
        });
    }

    pub(super) fn keep_hpss_finding(&mut self, index: usize, cx: &mut Context<Self>) {
        let HpssViewState::Ready(result) = &self.hpss_state else {
            return;
        };
        let Some(summary) = result.findings.get(index) else {
            return;
        };
        let finding = summary.finding;
        let source_view = WorkspaceViewId(self.audition_owner.local);
        self.workbench.update(cx, |workbench, cx| {
            workbench.keep_analysis_finding(source_view, finding, cx)
        });
    }

    pub(super) fn refresh_hpss(&mut self, cx: &mut Context<Self>) {
        self.cancel_hpss_job();
        let (
            duration,
            sample_rate,
            frame_count,
            playhead,
            analysis,
            document_generation,
            publication_generation,
            project_revisions,
            project_session,
        ) = {
            let workbench = self.workbench.read(cx);
            let Some(analysis) = workbench.analysis_arc() else {
                self.hpss_state = HpssViewState::Idle;
                return;
            };
            let session = workbench.session.read(cx);
            let Ok(snapshot) = session.project_snapshot() else {
                self.hpss_state = HpssViewState::Idle;
                return;
            };
            (
                analysis.duration_seconds,
                analysis.sample_rate,
                analysis.waveform_pyramid.frame_count(),
                workbench.playhead_fraction() as f64,
                analysis,
                session.document_generation(),
                session.snapshot().generation,
                snapshot.revisions(),
                session.id().0,
            )
        };
        if frame_count == 0 || duration <= 0.0 {
            self.hpss_state = HpssViewState::Idle;
            return;
        }
        let mut clamp_notice: Option<String> = None;

        // A reconstructible whole-song complex STFT can consume hundreds of
        // megabytes, and this lens holds the whole thing so that every
        // audition is an exact resynthesis. The bound is therefore a memory
        // budget, not a constant: the longest span on the ladder whose peak
        // allocation fits what the render products are held to.
        let span_seconds = self.hpss_span_seconds(sample_rate);
        let maximum_span = (span_seconds / duration).min(1.0);
        let asked_seconds = self.time_span() * duration;
        if self.time_span() > maximum_span {
            let anchor = if (self.time_start..=self.time_end).contains(&playhead) {
                playhead
            } else {
                (self.time_start + self.time_end) * 0.5
            };
            self.time_start = (anchor - maximum_span * 0.5).clamp(0.0, 1.0 - maximum_span);
            self.time_end = self.time_start + maximum_span;
            let held = self
                .hpss_settings
                .peak_bytes(span_frames(sample_rate, span_seconds));
            let choices = self.hpss_span_choices(sample_rate);
            let longer = match choices.last().copied() {
                Some(longest) if longest > span_seconds => format!(
                    " · SPAN+ reaches {longest:.0} s ({})",
                    format_megabytes(
                        self.hpss_settings
                            .peak_bytes(span_frames(sample_rate, longest))
                    )
                ),
                _ => String::new(),
            };
            let refused = match SPAN_LADDER_SECONDS.iter().copied().find(|seconds| {
                !self
                    .hpss_settings
                    .span_fits(sample_rate, *seconds, hpss_memory_budget_bytes())
            }) {
                Some(seconds) => format!(
                    " · {seconds:.0} s would hold {}, over the {} budget",
                    format_megabytes(
                        self.hpss_settings
                            .peak_bytes(span_frames(sample_rate, seconds))
                    ),
                    format_megabytes(hpss_memory_budget_bytes())
                ),
                None => String::new(),
            };
            clamp_notice = Some(format!(
                "Separation reads {span_seconds:.0} s around the playhead · the view asked for {asked_seconds:.0} s · this span holds {} of complex STFT at its widest moment{longer}{refused}",
                format_megabytes(held)
            ));
        }

        let start_frame = (self.time_start * frame_count as f64).floor() as usize;
        let end_frame = (self.time_end * frame_count as f64).ceil() as usize;
        let start_seconds = start_frame as f64 / f64::from(sample_rate);
        let end_seconds = end_frame as f64 / f64::from(sample_rate);
        let requested = self.hpss_freshness.epoch();
        let settings = self.hpss_settings;
        let owner = AnalysisProductOwner {
            project_session,
            namespace: self.audition_owner.namespace,
            local: self.audition_owner.local,
            pane: Some(self.audition_owner.local),
            generation: requested.get(),
        };
        self.hpss_state = HpssViewState::Analyzing {
            start_seconds,
            end_seconds,
        };
        if let Some(notice) = clamp_notice {
            self.say(notice, cx);
        }
        cx.notify();

        let preparation = cx.background_spawn(async move {
            // The lens is already bounded to its span; read exactly that
            // window out of the canonical PCM rather than holding the whole
            // mono to slice it.
            let original: Arc<[f32]> = Arc::from(analysis.mono_range(start_frame, end_frame));
            if original.len() != end_frame.saturating_sub(start_frame) {
                return Err("HPSS span lies outside retained PCM".to_owned());
            }
            let start = i64::try_from(start_frame)
                .map_err(|_| "HPSS start frame exceeds the signed project timeline".to_owned())?;
            let end = i64::try_from(end_frame)
                .map_err(|_| "HPSS end frame exceeds the signed project timeline".to_owned())?;
            let span = RenderSpan::new(start, end).map_err(|error| error.to_string())?;
            let format = RenderFormat::new(sample_rate, 1).map_err(|error| error.to_string())?;
            let source = PaneSourcePin::new(
                document_generation,
                publication_generation,
                project_revisions,
                None,
                span,
                format,
                original.as_ref(),
            )
            .map_err(|error| error.to_string())?;
            let descriptor = hpss_artifact_descriptor(&original, &source, settings)?;
            let prepared = AnalysisProductRuntime::prepare_hpss(Arc::clone(&original), settings)
                .map_err(|error| error.to_string())?;
            Ok::<_, String>((prepared, source, descriptor))
        });
        cx.spawn(async move |this, cx| {
            let prepared = preparation.await;
            let (ticket, source, descriptor) = match this.update(cx, |this, cx| {
                if !this.hpss_freshness.still_current(requested) {
                    return None;
                }
                match prepared {
                    Ok((prepared, source, descriptor)) => match this
                        .workbench
                        .read(cx)
                        .analysis_runtime
                        .submit_prepared(owner, prepared)
                    {
                        Ok(ticket) => {
                            this.hpss_cancellation = Some(ticket.cancellation());
                            Some((ticket, source, descriptor))
                        }
                        Err(error) => {
                            this.hpss_state = HpssViewState::Failed(error.to_string());
                            cx.notify();
                            None
                        }
                    },
                    Err(error) => {
                        this.hpss_state = HpssViewState::Failed(format!(
                            "Selected-span transform could not retain its project receipt · {error}"
                        ));
                        cx.notify();
                        None
                    }
                }
            }) {
                Ok(Some(prepared)) => prepared,
                _ => return,
            };
            let result = ticket.receive().await;
            let _ = this.update(cx, |this, cx| {
                if !this.hpss_freshness.still_current(requested) {
                    return;
                }
                this.hpss_cancellation = None;
                this.hpss_state = match result {
                    Ok(completion) => match completion.product.as_ref() {
                        AnalysisProduct::Hpss(product) => {
                            let product = Arc::clone(product);
                            let workbench = this.workbench.clone();
                            let publication = workbench.update(cx, |workbench, cx| {
                                let cancellation = RenderCancellation::new();
                                let findings = workbench
                                    .session
                                    .update(cx, |session, _| {
                                        session.publish_hpss_evidence(
                                            descriptor.clone(),
                                            product.separation.as_ref().clone(),
                                            &cancellation,
                                        )
                                    })
                                    .map_err(|error| error.to_string())?;
                                let registered = workbench.register_hpss_analysis_results(
                                    &descriptor,
                                    &findings,
                                    &source,
                                    Arc::clone(&product.original),
                                    &product.separation,
                                    cx,
                                )?;
                                let document_count =
                                    workbench.refresh_reverse_surface_documents(cx)?;
                                workbench.constructive_status = Some(format!(
                                    "Published {registered} HPSS evidence Finding(s) across {document_count} reverse documents"
                                ));
                                Ok::<_, String>(Arc::<[AnalysisEvidenceDocumentSummary]>::from(
                                    findings,
                                ))
                            });
                            match publication {
                                Ok(findings) => HpssViewState::Ready(Arc::new(HpssViewResult {
                                    source,
                                    start_frame: start_frame as u64,
                                    end_frame: end_frame as u64,
                                    start_seconds,
                                    end_seconds,
                                    sample_rate,
                                    product,
                                    findings,
                                })),
                                Err(error) => HpssViewState::Failed(format!(
                                    "HPSS completed but its evidence could not publish · {error}"
                                )),
                            }
                        }
                        other => HpssViewState::Failed(format!(
                            "analysis runtime returned {} to the HPSS pane",
                            other.kind_name()
                        )),
                    },
                    Err(error) => HpssViewState::Failed(error.to_string()),
                };
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn cancel_hpss_job(&mut self) {
        if let Some(cancellation) = self.hpss_cancellation.take() {
            cancellation.cancel();
        }
        self.hpss_freshness.bump();
    }

    pub(super) fn audition_hpss(&mut self, kind: HpssAudition, cx: &mut Context<Self>) {
        let HpssViewState::Ready(result) = &self.hpss_state else {
            return;
        };
        let (samples, audio_kind) = match kind {
            HpssAudition::Original => (
                Arc::clone(&result.product.original),
                PaneAudioKind::HpssSource,
            ),
            HpssAudition::Harmonic => (
                Arc::from(result.product.separation.harmonic.clone()),
                PaneAudioKind::HpssHarmonic,
            ),
            HpssAudition::Percussive => (
                Arc::from(result.product.separation.percussive.clone()),
                PaneAudioKind::HpssTransient,
            ),
            HpssAudition::Residual => (
                Arc::from(result.product.separation.residual.clone()),
                PaneAudioKind::HpssResidual,
            ),
        };
        let owner = self.audition_owner;
        let source = result.source.clone();
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| {
            workbench.audition_pane_timeline(owner, audio_kind, source, samples, cx)
        });
    }

    /// The kernels and the span, reachable in every state. They are how a
    /// musician gets out of Idle with a different question, so they cannot
    /// live only in the branch that already has an answer.
    fn render_separation_controls(
        &self,
        analysis: &Analysis,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let span_seconds = self.hpss_span_seconds(analysis.sample_rate);
        let peak = self
            .hpss_settings
            .peak_bytes(span_frames(analysis.sample_rate, span_seconds));
        let choices = self.hpss_span_choices(analysis.sample_rate);
        let longest = choices.last().copied().unwrap_or(span_seconds);
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
            .child(
                viz_control("hpss-time-median-down", "H−")
                    .on_click(cx.listener(|this, _, _, cx| this.step_hpss_median(-1, 0, cx))),
            )
            .child(
                viz_control("hpss-time-median-up", "H+")
                    .on_click(cx.listener(|this, _, _, cx| this.step_hpss_median(1, 0, cx))),
            )
            .child(
                div()
                    .min_w(px(86.0))
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(format!(
                        "H {} · P {}",
                        self.hpss_settings.time_median_width,
                        self.hpss_settings.frequency_median_width
                    )),
            )
            .child(
                viz_control("hpss-frequency-median-down", "P−")
                    .on_click(cx.listener(|this, _, _, cx| this.step_hpss_median(0, -1, cx))),
            )
            .child(
                viz_control("hpss-frequency-median-up", "P+")
                    .on_click(cx.listener(|this, _, _, cx| this.step_hpss_median(0, 1, cx))),
            )
            .child(div().w(px(12.0)))
            .child(
                viz_control("hpss-span-down", "SPAN−")
                    .px_2()
                    .on_click(cx.listener(|this, _, _, cx| this.step_hpss_span(-1, cx))),
            )
            .child(
                div()
                    .min_w(px(132.0))
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(format!(
                        "{span_seconds:.0} s · {} peak",
                        format_megabytes(peak)
                    )),
            )
            .child(
                viz_control("hpss-span-up", "SPAN+")
                    .px_2()
                    .on_click(cx.listener(|this, _, _, cx| this.step_hpss_span(1, cx))),
            )
            .child(div().flex_1())
            .child(div().text_xs().text_color(rgb(DIM)).child(format!(
                "H/P are the median widths and rebuild evidence · the span stops at {longest:.0} s because a longer one would not fit {}",
                format_megabytes(hpss_memory_budget_bytes())
            )))
    }

    pub(super) fn render_separation(
        &self,
        analysis: Arc<Analysis>,
        playhead_seconds: f64,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let controls = self.render_separation_controls(&analysis, cx);
        let span_seconds = self.hpss_span_seconds(analysis.sample_rate);
        let body = match &self.hpss_state {
            HpssViewState::Idle => empty_state(
                "No selected-span decomposition yet",
                &format!(
                    "Frame a span of at most {span_seconds:.0} seconds, then choose Analyze view."
                ),
            ),
            HpssViewState::Analyzing {
                start_seconds,
                end_seconds,
            } => empty_state(
                "Separating sustained and transient evidence…",
                &format!(
                    "Analyzing {}–{} with a reconstructible complex STFT and complementary soft masks.",
                    format_time(*start_seconds),
                    format_time(*end_seconds)
                ),
            ),
            HpssViewState::Failed(error) => {
                empty_state("The selected-span transform failed", error)
            }
            HpssViewState::Ready(result) => {
                let diagnostics = result.product.separation.diagnostics;
                let null_db = if diagnostics.relative_reconstruction_error <= 1.0e-9 {
                    -180.0
                } else {
                    20.0 * diagnostics.relative_reconstruction_error.log10()
                };
                let result_playhead = ((playhead_seconds - result.start_seconds)
                    / (result.end_seconds - result.start_seconds).max(f64::EPSILON))
                    as f32;
                let original = Arc::clone(&result.product.original_waveform);
                let harmonic = Arc::clone(&result.product.harmonic_waveform);
                let percussive = Arc::clone(&result.product.percussive_waveform);
                let residual = Arc::clone(&result.product.residual_waveform);
                let result_span = (result.end_seconds - result.start_seconds).max(f64::EPSILON);
                let requested_start = analysis.duration_seconds * self.time_start;
                let requested_end = analysis.duration_seconds * self.time_end;
                let stale = (requested_start - result.start_seconds).abs() > result_span * 0.002
                    || (requested_end - result.end_seconds).abs() > result_span * 0.002;

                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(40.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px_4()
                            .gap_4()
                            .bg(rgb(PANEL_ALT))
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(
                                div().text_color(rgb(CYAN)).child(format!(
                                    "mask separation {:.0}%",
                                    diagnostics.mask_confidence * 100.0
                                )),
                            )
                            .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                                "mixture null {:.1} dB  ·  FFT {} / hop {}  ·  H {} / P {}  ·  {}",
                                null_db,
                                result.product.separation.settings.fft_size,
                                result.product.separation.settings.hop_size,
                                result.product.separation.settings.time_median_width,
                                result.product.separation.settings.frequency_median_width,
                                if stale { "view changed — reanalyze to update" } else { "selected span is current" }
                            )))
                            .child(div().flex_1())
                            .when(!result.findings.is_empty(), |header| {
                                header
                                    .child(
                                        viz_control("open-hpss-finding", "Open Findings")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.open_hpss_finding(0, cx)
                                            })),
                                    )
                                    .child(
                                        viz_control("keep-hpss-finding", "Keep finding")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.keep_hpss_finding(0, cx)
                                            })),
                                    )
                            })
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(
                                        viz_control("hear-hpss-original", "Hear mix").px_2().on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.audition_hpss(HpssAudition::Original, cx)
                                            }),
                                        ),
                                    )
                                    .child(
                                        viz_control("hear-hpss-harmonic", "Hear sustained")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.audition_hpss(HpssAudition::Harmonic, cx)
                                            })),
                                    )
                                    .child(
                                        viz_control("hear-hpss-percussive", "Hear transient")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.audition_hpss(HpssAudition::Percussive, cx)
                                            })),
                                    )
                                    .child(
                                        viz_control("hear-hpss-residual", "Hear null")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.audition_hpss(HpssAudition::Residual, cx)
                                            })),
                                    ),
                            ),
                    )
                    .child(time_ruler_range(result.start_seconds, result.end_seconds))
                    .child(
                        // One pointer region over the whole stack: the four
                        // lanes are one window of the song drawn four ways, so
                        // a gesture belongs to the stack, not to a lane. The
                        // overlay is also what captures the bounds the
                        // pointer kernel maps x through.
                        div()
                            .relative()
                            .flex_none()
                            .flex()
                            .flex_col()
                            .cursor_crosshair()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                    this.lens_pointer_down(event, cx);
                                }),
                            )
                            .on_mouse_move(cx.listener(
                                |this, event: &MouseMoveEvent, _, cx| {
                                    this.lens_pointer_move(event, cx);
                                },
                            ))
                            .capture_any_mouse_up(cx.listener(
                                |this, event: &MouseUpEvent, _, cx| {
                                    this.lens_pointer_up(event, cx);
                                },
                            ))
                    .child(lane(
                        "ORIGINAL MIX / SELECTED ASPECT",
                        px(120.0),
                        waveform_plot(
                            original,
                            result_playhead,
                            Arc::clone(&self.waveform_geometry),
                            WaveformRenderKey::fractions(
                                10,
                                self.hpss_freshness.epoch().get(),
                                result.start_seconds,
                                result.end_seconds,
                            ),
                        ),
                    ))
                    .child(lane(
                        "TONALLY SUSTAINED ESTIMATE",
                        px(120.0),
                        waveform_plot(
                            harmonic,
                            result_playhead,
                            Arc::clone(&self.waveform_geometry),
                            WaveformRenderKey::fractions(
                                11,
                                self.hpss_freshness.epoch().get(),
                                result.start_seconds,
                                result.end_seconds,
                            ),
                        ),
                    ))
                    .child(lane(
                        "TRANSIENT ESTIMATE",
                        px(120.0),
                        waveform_plot(
                            percussive,
                            result_playhead,
                            Arc::clone(&self.waveform_geometry),
                            WaveformRenderKey::fractions(
                                12,
                                self.hpss_freshness.epoch().get(),
                                result.start_seconds,
                                result.end_seconds,
                            ),
                        ),
                    ))
                    .child(lane(
                        "MIXTURE NULL (ORIGINAL − ESTIMATES)",
                        px(92.0),
                        waveform_plot(
                            residual,
                            result_playhead,
                            Arc::clone(&self.waveform_geometry),
                            WaveformRenderKey::fractions(
                                13,
                                self.hpss_freshness.epoch().get(),
                                result.start_seconds,
                                result.end_seconds,
                            ),
                        ),
                    ))
                            .child(timeline_overlay(
                                self.timeline_bounds.clone(),
                                result_playhead,
                            )),
                    )
                    .child(
                        div()
                            .h(px(38.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px_4()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("HPSS separates time-persistent from frequency-broad evidence. It is auditionable and additive, but it is not an instrument or vocal classifier."),
                    )
                    .into_any_element()
            }
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(controls)
            .child(body)
            .into_any_element()
    }
}
