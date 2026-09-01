//! Rhythm deprojection lens.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

impl Visualizer {
    pub(super) fn refresh_rhythm(&mut self, cx: &mut Context<Self>) {
        self.cancel_rhythm_job();
        let source = {
            let workbench = self.workbench.read(cx);
            workbench.analysis().and_then(|analysis| {
                let session = workbench.session.read(cx);
                let revisions = session.project_snapshot().ok()?.revisions();
                Some((
                    analysis.mono_pcm.clone(),
                    analysis.sample_rate,
                    analysis.path.clone(),
                    session.document_generation(),
                    session.snapshot().generation,
                    revisions,
                    session.id().0,
                ))
            })
        };
        let Some((
            mono,
            sample_rate,
            path,
            document_generation,
            publication_generation,
            project_revisions,
            project_session,
        )) = source
        else {
            self.rhythm_state = RhythmViewState::Idle;
            return;
        };

        let generation = self.rhythm_generation;
        let owner = AnalysisProductOwner {
            project_session,
            namespace: self.audition_owner.namespace,
            local: self.audition_owner.local ^ 0x7268_7974_686d,
            pane: Some(self.audition_owner.local),
            generation,
        };
        self.rhythm_state = RhythmViewState::Analyzing;
        cx.notify();

        let preparing_mono = Arc::clone(&mono);
        let preparation = cx.background_spawn(async move {
            let span = i64::try_from(preparing_mono.len())
                .map_err(|_| "rhythm source is too large".to_owned())
                .and_then(|end| RenderSpan::new(0, end).map_err(|error| error.to_string()))?;
            let format = RenderFormat::new(sample_rate, 1).map_err(|error| error.to_string())?;
            let source = PaneSourcePin::new(
                document_generation,
                publication_generation,
                project_revisions,
                None,
                span,
                format,
                preparing_mono.as_ref(),
            )
            .map_err(|error| error.to_string())?;
            let descriptor = rhythm_artifact_descriptor(&preparing_mono, sample_rate)?;
            let rendered = RenderedExplanation {
                origin_frame: descriptor.extent.start,
                audio: ProjectAudio::from_interleaved(
                    AudioFormat::new(sample_rate, 1).map_err(|error| error.to_string())?,
                    preparing_mono.as_ref().to_vec(),
                )
                .map_err(|error| error.to_string())?,
            };
            let prepared = AnalysisProductRuntime::prepare_rhythm(
                Arc::clone(&preparing_mono),
                sample_rate,
                RhythmDeprojectionConfig::default(),
            )
            .map_err(|error| error.to_string())?;
            Ok::<_, String>((prepared, source, descriptor, rendered))
        });
        cx.spawn(async move |this, cx| {
            let prepared = preparation.await;
            let (ticket, source, descriptor, rendered) =
                match this.update(cx, |this, cx| {
                    if this.rhythm_generation != generation
                        || this.spectrogram_source.as_ref() != Some(&path)
                    {
                        return None;
                    }
                    match prepared {
                        Ok((prepared, source, descriptor, rendered)) => {
                            match this
                                .workbench
                                .read(cx)
                                .analysis_runtime
                                .submit_prepared(owner, prepared)
                            {
                                Ok(ticket) => {
                                    this.rhythm_cancellation = Some(ticket.cancellation());
                                    Some((ticket, source, descriptor, rendered))
                                }
                                Err(error) => {
                                    this.rhythm_state =
                                        RhythmViewState::Failed(error.to_string());
                                    cx.notify();
                                    None
                                }
                            }
                        }
                        Err(error) => {
                            this.rhythm_state = RhythmViewState::Failed(error);
                            cx.notify();
                            None
                        }
                    }
                }) {
                    Ok(Some(prepared)) => prepared,
                    _ => return,
                };
            let completion = ticket.receive().await;
            let _ = this.update(cx, |this, cx| {
                if this.rhythm_generation != generation
                    || this.spectrogram_source.as_ref() != Some(&path)
                {
                    return;
                }
                this.rhythm_cancellation = None;
                let result = match completion {
                    Ok(completion) => match completion.product.as_ref() {
                        AnalysisProduct::Rhythm(result) => Arc::clone(result),
                        other => {
                            this.rhythm_state = RhythmViewState::Failed(format!(
                                "analysis runtime returned {} to the rhythm pane",
                                other.kind_name()
                            ));
                            cx.notify();
                            return;
                        }
                    },
                    Err(error) => {
                        this.rhythm_state = RhythmViewState::Failed(error.to_string());
                        cx.notify();
                        return;
                    }
                };
                this.rhythm_state = match result.status {
                    RhythmAnalysisStatus::Complete => {
                        let workbench = this.workbench.clone();
                        let publication = workbench.update(cx, |workbench, cx| {
                            {
                                let current = workbench.session.read(cx);
                                let current_revisions = current
                                    .project_snapshot()
                                    .ok()
                                    .map(|snapshot| snapshot.revisions());
                                if current.snapshot().generation != publication_generation
                                    || current_revisions != Some(project_revisions)
                                {
                                    return Err(
                                        "rhythm analysis completed after its project publication was superseded"
                                            .to_owned(),
                                    );
                                }
                            }
                            let cancellation = RenderCancellation::new();
                            let session = workbench.session.clone();
                            let candidates = session
                                .update(cx, |session, _| {
                                    session.publish_live_deprojection_analysis(
                                        LiveDeprojectionAnalysis::from_rhythm(
                                            descriptor.clone(),
                                            result.as_ref().clone(),
                                            ExplainBudget::default(),
                                            rendered,
                                        ),
                                        &cancellation,
                                    )
                                })
                                .map_err(|error| error.to_string())?;
                            let registered = workbench.register_rhythm_analysis_results(
                                &descriptor,
                                &candidates,
                                &source,
                                cx,
                            )?;
                            let document_count = workbench.refresh_reverse_surface_documents(cx)?;
                            workbench.constructive_status = Some(format!(
                                "Published {} live rhythm candidate(s) as {registered} actionable Finding(s) across {document_count} reverse documents",
                                candidates.len()
                            ));
                            Ok((source, Arc::<[DeprojectionCandidateDocumentSummary]>::from(candidates)))
                        });
                        match publication {
                            Ok((source, candidates)) => {
                                RhythmViewState::Ready(Arc::new(RhythmViewResult {
                                    source,
                                    source_pcm: Arc::clone(&mono),
                                    deprojection: result,
                                    candidates,
                                }))
                            }
                            Err(error) => {
                                this.workbench.update(cx, |workbench, cx| {
                                    workbench.constructive_status = Some(format!(
                                        "Rhythm deprojection was analyzed but not published · {error}"
                                    ));
                                    cx.notify();
                                });
                                RhythmViewState::Failed(format!(
                                    "Rhythm result could not publish its actionable Finding · {error}"
                                ))
                            }
                        }
                    }
                    RhythmAnalysisStatus::Silent => {
                        RhythmViewState::Failed("The selected audio is effectively silent.".into())
                    }
                    RhythmAnalysisStatus::InsufficientInput => RhythmViewState::Failed(
                        "There is not enough audio to infer recurring events.".into(),
                    ),
                    RhythmAnalysisStatus::InvalidConfiguration => RhythmViewState::Failed(
                        "The rhythm analysis configuration is invalid.".into(),
                    ),
                };
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn cancel_rhythm_job(&mut self) {
        if let Some(cancellation) = self.rhythm_cancellation.take() {
            cancellation.cancel();
        }
        self.rhythm_generation = self.rhythm_generation.wrapping_add(1);
    }

    pub(super) fn audition_rhythm_family(&mut self, family_id: usize, cx: &mut Context<Self>) {
        let RhythmViewState::Ready(result) = &self.rhythm_state else {
            return;
        };
        let Some(span) = result
            .event_families
            .iter()
            .find(|family| family.id == family_id)
            .map(|family| family.medoid.excerpt)
        else {
            return;
        };
        let Some(samples) = result.source_pcm.get(span.start..span.end) else {
            return;
        };
        let owner = self.audition_owner;
        let source = result.source.clone();
        let sample_rate = result.sample_rate;
        let samples: Arc<[f32]> = Arc::from(samples);
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| {
            workbench.preview_pane_mono(
                owner,
                PaneAudioKind::RhythmFamilyMedoid,
                &source,
                sample_rate,
                samples,
                cx,
            )
        });
    }

    pub(super) fn open_rhythm_finding(&mut self, index: usize, cx: &mut Context<Self>) {
        let RhythmViewState::Ready(result) = &self.rhythm_state else {
            return;
        };
        let Some(summary) = result.candidates.get(index) else {
            return;
        };
        let finding = summary.finding;
        let source_view = WorkspaceViewId(self.audition_owner.local);
        self.workbench.update(cx, |workbench, cx| {
            workbench.reveal_analysis_finding(source_view, finding, cx)
        });
    }

    pub(super) fn keep_rhythm_finding(&mut self, index: usize, cx: &mut Context<Self>) {
        let RhythmViewState::Ready(result) = &self.rhythm_state else {
            return;
        };
        let Some(summary) = result.candidates.get(index) else {
            return;
        };
        let finding = summary.finding;
        let source_view = WorkspaceViewId(self.audition_owner.local);
        self.workbench.update(cx, |workbench, cx| {
            workbench.keep_analysis_finding(source_view, finding, cx)
        });
    }

    pub(super) fn adopt_rhythm_tempo(&mut self, rank: usize, cx: &mut Context<Self>) {
        let RhythmViewState::Ready(result) = &self.rhythm_state else {
            return;
        };
        let Some(hypothesis) = result
            .tempo_hypotheses
            .iter()
            .find(|hypothesis| hypothesis.rank == rank)
            .cloned()
        else {
            return;
        };
        let source = result.source.clone();
        self.workbench.update(cx, |workbench, cx| {
            let adoption = (|| {
                let current = workbench.pane_audition_context(cx)?;
                source
                    .validate_current(
                        current.document_generation,
                        current.publication_generation,
                        current.revisions,
                        current.audible_cohort.as_ref(),
                    )
                    .map_err(|error| error.to_string())?;
                let intent = AdoptTempoIntent {
                    expected_project_revision: current.revisions.aggregate,
                    bpm: f64::from(hypothesis.bpm),
                    source: Some(RhythmTempoEvidence {
                        source_content: source.source_content,
                        source_span: source.span,
                        candidate_rank: hypothesis.rank,
                        periodicity: hypothesis.periodicity,
                        evidence: hypothesis.evidence,
                    }),
                };
                workbench
                    .session
                    .update(cx, |session, _| session.adopt_project_tempo(intent))
                    .map_err(|error| error.to_string())
            })();
            workbench.constructive_status = Some(match adoption {
                Ok(TempoAdoptionOutcome::Published { publication, .. }) => format!(
                    "Adopted rhythm candidate #{} as {:.3} BPM · previous project tempo {:.3} BPM · undoable",
                    rank + 1,
                    publication.adopted_bpm,
                    publication.previous_bpm
                ),
                Ok(TempoAdoptionOutcome::Unchanged(publication)) => format!(
                    "Rhythm candidate #{} already matches the project tempo at {:.3} BPM",
                    rank + 1,
                    publication.adopted_bpm
                ),
                Err(error) => format!("Tempo was not adopted · {error}"),
            });
            cx.notify();
        });
    }

    pub(super) fn render_rhythm(
        &self,
        analysis: Arc<Analysis>,
        playhead: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let timeline_bounds = self.timeline_bounds.clone();
        let start_seconds = analysis.duration_seconds * self.time_start;
        let end_seconds = analysis.duration_seconds * self.time_end;
        let waveform = analysis.waveform_range(self.time_start, self.time_end, 4_096);
        let features = slice_visible(&analysis.features, self.time_start, self.time_end);
        let evidence = match &self.rhythm_state {
            RhythmViewState::Idle => empty_state(
                "Rhythm deprojection has not run",
                "Reopen this Aspect to analyze the retained PCM.",
            )
            .into_any_element(),
            RhythmViewState::Analyzing => empty_state(
                "Deprojecting rhythm…",
                "Finding multiband attacks, competing pulses, exact hit spans, and recurring mixed-audio families off the render thread.",
            )
            .into_any_element(),
            RhythmViewState::Failed(error) => {
                empty_state("Rhythm deprojection unavailable", error).into_any_element()
            }
            RhythmViewState::Ready(result) => {
                let visible_start = (self.time_start * result.sample_frames as f64).floor() as usize;
                let visible_end = (self.time_end * result.sample_frames as f64).ceil() as usize;
                let family_ids = visible_rhythm_family_ids(
                    result,
                    visible_start,
                    visible_end,
                    RHYTHM_MAX_VISIBLE_FAMILIES,
                );
                let tempo = tempo_hypotheses_summary(result);
                let phase_summary = format!(
                    "{} beat phases · {} downbeat/meter hypotheses · {} pattern candidates",
                    result.beat_phase_hypotheses.len(),
                    result.downbeat_hypotheses.len(),
                    result.patterns.len()
                );
                let finding_count = result.candidates.len();
                let result_for_plot = Arc::clone(&result.deprojection);
                let plot_family_ids = family_ids.clone();
                let sample_rate = result.sample_rate;
                let project_bpm = self
                    .workbench
                    .read(cx)
                    .session
                    .read(cx)
                    .project_snapshot()
                    .ok()
                    .map(|snapshot| {
                        snapshot
                            .project
                            .state()
                            .domains
                            .sequencer
                            .tempo_map()
                            .tempo_at(crate::sequencer::BeatTime::ZERO)
                            .bpm()
                    });
                let tempo_choices = result
                    .tempo_hypotheses
                    .iter()
                    .take(4)
                    .map(|hypothesis| {
                        let rank = hypothesis.rank;
                        let bpm = hypothesis.bpm;
                        let active = project_bpm
                            .is_some_and(|current| (current - f64::from(bpm)).abs() < 0.001);
                        div()
                            .id(("rhythm-adopt-tempo", rank))
                            .h(px(25.0))
                            .px_2()
                            .flex_none()
                            .rounded_sm()
                            .border_1()
                            .border_color(if active { rgb(CYAN) } else { rgb(BORDER) })
                            .bg(if active { rgb(BORDER) } else { rgb(PANEL) })
                            .flex()
                            .items_center()
                            .text_xs()
                            .text_color(if active { rgb(CYAN) } else { rgb(MUTED) })
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)).text_color(rgb(TEXT)))
                            .child(format!(
                                "{} #{} · {:.1} BPM · {:.0}%",
                                if active { "PROJECT" } else { "ADOPT" },
                                rank + 1,
                                bpm,
                                hypothesis.evidence * 100.0
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.adopt_rhythm_tempo(rank, cx)
                            }))
                    })
                    .collect::<Vec<_>>();

                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .min_h(px(82.0))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .bg(rgb(PANEL_ALT))
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(
                                div()
                                    .h(px(50.0))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .px_4()
                                    .gap_3()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .gap_1()
                                            .child(
                                                div().text_sm().text_color(rgb(CYAN)).child(tempo),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(MUTED))
                                                    .child(phase_summary),
                                            ),
                                    )
                                    .when(finding_count > 0, |header| {
                                        header
                                            .child(
                                                div()
                                                    .id("rhythm-open-finding")
                                                    .h(px(28.0))
                                                    .px_3()
                                                    .flex_none()
                                                    .rounded_sm()
                                                    .border_1()
                                                    .border_color(rgb(CYAN))
                                                    .flex()
                                                    .items_center()
                                                    .text_xs()
                                                    .text_color(rgb(CYAN))
                                                    .cursor_pointer()
                                                    .hover(|style| {
                                                        style.bg(rgb(BORDER)).text_color(rgb(TEXT))
                                                    })
                                                    .child(format!(
                                                        "Open Finding{} · {finding_count}",
                                                        if finding_count == 1 { "" } else { "s" }
                                                    ))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.open_rhythm_finding(0, cx)
                                                    })),
                                            )
                                            .child(
                                                viz_control("rhythm-keep-finding", "Keep finding")
                                                    .px_2()
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.keep_rhythm_finding(0, cx)
                                                    })),
                                            )
                                    }),
                            )
                            .child(
                                div()
                                    .min_h(px(32.0))
                                    .flex_none()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .px_4()
                                    .pb_2()
                                    .gap_1()
                                    .children(tempo_choices),
                            ),
                    )
                    .child(time_ruler_range(start_seconds, end_seconds))
                    .child(
                        div()
                            .h(px(RHYTHM_ROW_HEIGHT * RHYTHM_MAX_VISIBLE_FAMILIES as f32))
                            .flex_none()
                            .flex()
                            .child(
                                div()
                                    .w(px(RHYTHM_GUTTER))
                                    .flex_none()
                                    .flex()
                                    .flex_col()
                                    .bg(rgb(PANEL_ALT))
                                    .border_r_1()
                                    .border_color(rgb(BORDER))
                                    .children(family_ids.iter().copied().filter_map(|family_id| {
                                        let family = result
                                            .event_families
                                            .iter()
                                            .find(|family| family.id == family_id)?;
                                        let visible = family
                                            .event_indices
                                            .iter()
                                            .filter(|index| {
                                                result.hits.get(**index).is_some_and(|hit| {
                                                    spans_overlap(
                                                        hit.span,
                                                        visible_start,
                                                        visible_end,
                                                    )
                                                })
                                            })
                                            .count();
                                        let medoid_seconds = family.medoid.excerpt.start as f64
                                            / f64::from(sample_rate);
                                        let medoid_span = family.medoid.excerpt;
                                        Some(
                                            div()
                                                .id(("rhythm-family", family_id))
                                                .h(px(RHYTHM_ROW_HEIGHT))
                                                .flex_none()
                                                .px_3()
                                                .flex()
                                                .flex_col()
                                                .justify_center()
                                                .border_b_1()
                                                .border_color(rgb(BORDER))
                                                .cursor_pointer()
                                                .hover(|style| style.bg(rgb(PANEL)))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.audition_rhythm_family(family_id, cx)
                                                }))
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(cluster_color(family_id))
                                                        .child(format!(
                                                            "▶ Anonymous family {:02} · {visible}/{} visible",
                                                            family_id + 1,
                                                            family.event_indices.len()
                                                        )),
                                                )
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(rgb(MUTED))
                                                        .child(format!(
                                                            "mixed medoid {} · exact [{}..{})",
                                                            format_time(medoid_seconds),
                                                            medoid_span.start,
                                                            medoid_span.end
                                                        )),
                                                )
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(rgb(DIM))
                                                        .child(format!(
                                                            "{:.0}% cohesion evidence · click to audition",
                                                            family.evidence * 100.0
                                                        )),
                                                ),
                                        )
                                    })),
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
                                    .child(rhythm_deprojection_plot(
                                        result_for_plot,
                                        plot_family_ids,
                                        visible_start,
                                        visible_end,
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
                            .child(format!(
                                "{} exact hits in view · family rows are recurring mixed excerpts, not isolated instrument identities; magenta marks pattern-start evidence.",
                                visible_hit_count(result, visible_start, visible_end)
                            )),
                    )
                    .child(lane(
                        "STEREO AMPLITUDE",
                        px(100.0),
                        waveform_plot(
                            waveform,
                            playhead,
                            Arc::clone(&self.waveform_geometry),
                            WaveformRenderKey::fractions(
                                1,
                                self.rhythm_generation,
                                self.time_start,
                                self.time_end,
                            ),
                        ),
                    ))
                    .child(lane(
                        "TRANSIENT FLUX",
                        px(72.0),
                        feature_plot(
                            features,
                            playhead,
                            |feature| feature.flux,
                            rgba(0xf6b760cc),
                        ),
                    ))
                    .into_any_element()
            }
        };
        div().flex_1().min_h_0().child(evidence)
    }
}
