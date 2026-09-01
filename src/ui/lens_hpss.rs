//! Harmonic / transient decomposition lens.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

impl Visualizer {
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
            mono,
            document_generation,
            publication_generation,
            project_revisions,
            project_session,
        ) = {
            let workbench = self.workbench.read(cx);
            let Some(analysis) = workbench.analysis() else {
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
                Arc::clone(&analysis.mono_pcm),
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

        // A reconstructible whole-song complex STFT can consume hundreds of
        // megabytes. HPSS is therefore an Aspect-local transform. Keep an
        // explicit upper bound until the field becomes a tiled disk cache.
        let maximum_span = (30.0 / duration).min(1.0);
        if self.time_span() > maximum_span {
            let anchor = if (self.time_start..=self.time_end).contains(&playhead) {
                playhead
            } else {
                (self.time_start + self.time_end) * 0.5
            };
            self.time_start = (anchor - maximum_span * 0.5).clamp(0.0, 1.0 - maximum_span);
            self.time_end = self.time_start + maximum_span;
        }

        let start_frame = (self.time_start * frame_count as f64).floor() as usize;
        let end_frame = (self.time_end * frame_count as f64).ceil() as usize;
        let start_seconds = start_frame as f64 / f64::from(sample_rate);
        let end_seconds = end_frame as f64 / f64::from(sample_rate);
        let generation = self.hpss_generation;
        let settings = HpssSettings::default();
        let owner = AnalysisProductOwner {
            project_session,
            namespace: self.audition_owner.namespace,
            local: self.audition_owner.local,
            pane: Some(self.audition_owner.local),
            generation,
        };
        self.hpss_state = HpssViewState::Analyzing {
            start_seconds,
            end_seconds,
        };
        cx.notify();

        let preparation = cx.background_spawn(async move {
            let original: Arc<[f32]> = mono
                .get(start_frame..end_frame)
                .map(|samples| Arc::from(samples.to_vec()))
                .ok_or_else(|| "HPSS span lies outside retained PCM".to_owned())?;
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
                if this.hpss_generation != generation {
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
                if this.hpss_generation != generation {
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
        self.hpss_generation = self.hpss_generation.wrapping_add(1);
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

    pub(super) fn render_separation(
        &self,
        analysis: Arc<Analysis>,
        playhead_seconds: f64,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match &self.hpss_state {
            HpssViewState::Idle => empty_state(
                "No selected-span decomposition yet",
                "Frame a span of at most 30 seconds, then choose Analyze view.",
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
                            .h(px(42.0))
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
                                "mixture null {:.1} dB  ·  FFT {} / hop {}  ·  {}",
                                null_db,
                                result.product.separation.settings.fft_size,
                                result.product.separation.settings.hop_size,
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
                    .child(lane(
                        "ORIGINAL MIX / SELECTED ASPECT",
                        px(120.0),
                        waveform_plot(
                            original,
                            result_playhead,
                            Arc::clone(&self.waveform_geometry),
                            WaveformRenderKey::fractions(
                                10,
                                self.hpss_generation,
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
                                self.hpss_generation,
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
                                self.hpss_generation,
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
                                self.hpss_generation,
                                result.start_seconds,
                                result.end_seconds,
                            ),
                        ),
                    ))
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
        }
    }
}
