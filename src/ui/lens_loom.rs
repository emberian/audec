//! Loom event-template reconstruction lens.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

use gpui::Point;

use crate::project_controller::{
    plan_loom_event_edit, recommend_constructive, LoomClusterEditIntent, LoomEventEditIntent,
    LOOM_MUTE_DB,
};

impl Visualizer {
    pub(super) fn open_loom_finding(&mut self, index: usize, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &self.loom_state else {
            self.say(
                "Loom · there is no sequence hypothesis yet, so there is no Finding to open",
                cx,
            );
            return;
        };
        let count = result.findings.len();
        let Some(summary) = result.findings.get(index) else {
            self.say(
                format!("Loom · Finding {} of {count} does not exist", index + 1),
                cx,
            );
            return;
        };
        let finding = summary.finding;
        let source_view = WorkspaceViewId(self.audition_owner.local);
        self.workbench.update(cx, |workbench, cx| {
            workbench.reveal_analysis_finding(source_view, finding, cx)
        });
    }

    pub(super) fn keep_loom_finding(&mut self, index: usize, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &self.loom_state else {
            self.say(
                "Loom · there is no sequence hypothesis yet, so there is no Finding to keep",
                cx,
            );
            return;
        };
        let count = result.findings.len();
        let Some(summary) = result.findings.get(index) else {
            self.say(
                format!("Loom · Finding {} of {count} does not exist", index + 1),
                cx,
            );
            return;
        };
        let finding = summary.finding;
        let source_view = WorkspaceViewId(self.audition_owner.local);
        self.workbench.update(cx, |workbench, cx| {
            workbench.keep_analysis_finding(source_view, finding, cx)
        });
    }

    pub(super) fn apply_loom_sequence(&mut self, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &self.loom_state else {
            self.say(
                "Make Pattern · refused · there is no sequence hypothesis yet · press Reinfer to build one",
                cx,
            );
            return;
        };
        let Some(summary) = result
            .findings
            .iter()
            .find(|summary| summary.kind == AnalysisEvidenceKind::LoomSequence)
        else {
            let kinds = result
                .findings
                .iter()
                .map(|summary| format!("{:?}", summary.kind))
                .collect::<Vec<_>>();
            self.say(
                format!(
                    "Make Pattern · refused · this inference published no LoomSequence Finding to construct from · it published {}",
                    if kinds.is_empty() {
                        "nothing".to_owned()
                    } else {
                        kinds.join(", ")
                    }
                ),
                cx,
            );
            return;
        };
        let artifact = summary.artifact;
        let finding = summary.finding;
        let source_view = WorkspaceViewId(self.audition_owner.local);
        let published = self.workbench.update(cx, |workbench, cx| {
            match workbench.execute_loom_result_construction(artifact, finding, cx) {
                Ok(publication) => {
                    // Reveal the pattern this construction made, by its own id.
                    // `open_sequencer_editor` opened whichever pattern came
                    // first, which was only ever this one by accident.
                    let recommendation = recommend_constructive(&publication);
                    workbench.enqueue_reveal_recommendation(
                        recommendation,
                        Some(source_view),
                        |_| "Loom pattern created",
                        cx,
                    );
                    Some(publication)
                }
                Err(error) => {
                    workbench.constructive_status =
                        Some(format!("Loom construction was not applied · {error}"));
                    cx.notify();
                    None
                }
            }
        });
        let Some(publication) = published else {
            return;
        };
        let LoomViewState::Ready(result) = &mut self.loom_state else {
            self.say(
                "Make Pattern · the construction was committed, but this pane no longer holds the sketch it was made from",
                cx,
            );
            return;
        };
        result.binding = publication.loom.clone();
        if result.binding.is_none() {
            self.workbench.update(cx, |workbench, _| {
                workbench.constructive_status = Some(
                    "Loom construction committed, but it published no pattern to bind to".into(),
                );
            });
        }
        cx.notify();
    }

    pub(super) fn refresh_loom(&mut self, cx: &mut Context<Self>) {
        self.cancel_loom_job();
        let source = {
            let workbench = self.workbench.read(cx);
            workbench.analysis_arc().and_then(|analysis_arc| {
                let analysis = analysis_arc.as_ref();
                let session = workbench.session.read(cx);
                let revisions = session.project_snapshot().ok()?.revisions();
                let frame_count = analysis.waveform_pyramid.frame_count();
                let observations = analysis
                    .rhythm
                    .onsets
                    .iter()
                    .map(|onset| EventObservation {
                        sample_index: (onset.time_seconds * f64::from(analysis.sample_rate)).round()
                            as usize,
                        cluster_id: onset.cluster,
                        salience: onset.strength,
                        template_similarity: onset.template_similarity,
                    })
                    .collect::<Arc<[_]>>();
                Some((
                    analysis.sample_rate,
                    frame_count,
                    Arc::clone(&analysis_arc),
                    observations,
                    session.document_generation(),
                    session.snapshot().generation,
                    revisions,
                    session.id().0,
                ))
            })
        };
        let Some((
            sample_rate,
            frame_count,
            analysis,
            observations,
            document_generation,
            publication_generation,
            project_revisions,
            project_session,
        )) = source
        else {
            self.loom_state = LoomViewState::Idle;
            return;
        };
        if frame_count == 0 || observations.is_empty() {
            self.loom_state = LoomViewState::Failed(
                "No recurring onset observations are available to sequence.".to_owned(),
            );
            cx.notify();
            return;
        }

        let start_sample = (self.time_start * frame_count as f64).floor() as usize;
        let end_sample = (self.time_end * frame_count as f64).ceil() as usize;
        let start_seconds = start_sample as f64 / f64::from(sample_rate);
        let end_seconds = end_sample as f64 / f64::from(sample_rate);
        let requested = self.loom_freshness.epoch();
        let settings = self.loom_settings.normalized();
        let config = settings.build_config(sample_rate);
        // Templates come from the selection and the material just before it,
        // not from the whole recording: what the lens reads stays a function
        // of the window rather than of the length of the recording. How long
        // that window is, is the musician's choice.
        let lookbehind = settings.lookbehind_seconds() * sample_rate as usize;
        let window = LoomWindow::around(
            start_sample,
            end_sample,
            lookbehind,
            config,
            frame_count,
            |start, end| analysis.mono_range(start, end),
        );
        let observations: Arc<[EventObservation]> = observations
            .iter()
            .copied()
            .filter(|observation| window.admits(observation, config))
            .collect();
        let (template_start_seconds, template_end_seconds) = window.extent_seconds(sample_rate);
        let event_count = observations.len();
        let owner = AnalysisProductOwner {
            project_session,
            namespace: self.audition_owner.namespace,
            local: self.audition_owner.local ^ 0x6c6f_6f6d,
            pane: Some(self.audition_owner.local),
            generation: requested.get(),
        };
        self.loom_state = LoomViewState::Inferring {
            start_seconds,
            end_seconds,
            event_count,
            template_start_seconds,
            template_end_seconds,
        };
        cx.notify();

        let preparation = cx.background_spawn(async move {
            let template_extent = i64::try_from(window.samples.len())
                .map_err(|_| "Loom source exceeds the signed project timeline".to_owned())?;
            let full_span =
                RenderSpan::new(0, template_extent).map_err(|error| error.to_string())?;
            let format = RenderFormat::new(sample_rate, 1).map_err(|error| error.to_string())?;
            let template_source_pin = PaneSourcePin::new(
                document_generation,
                publication_generation,
                project_revisions,
                None,
                full_span,
                format,
                &window.samples,
            )
            .map_err(|error| error.to_string())?;
            let start = i64::try_from(start_sample)
                .map_err(|_| "Loom span start exceeds the signed timeline".to_owned())?;
            let end = i64::try_from(end_sample)
                .map_err(|_| "Loom span end exceeds the signed timeline".to_owned())?;
            let span = RenderSpan::new(start, end).map_err(|error| error.to_string())?;
            let (retained_start, retained_end) = (
                window.start,
                window.start.saturating_add(window.samples.len()),
            );
            if start_sample < retained_start || end_sample > retained_end {
                return Err("Loom span lies outside the template window".to_owned());
            }
            let original =
                &window.samples[start_sample - retained_start..end_sample - retained_start];
            let source_pin = PaneSourcePin::new(
                document_generation,
                publication_generation,
                project_revisions,
                None,
                span,
                format,
                original,
            )
            .map_err(|error| error.to_string())?;
            let descriptor = loom_artifact_descriptor(&window.samples, &source_pin, config)?;
            let prepared = AnalysisProductRuntime::prepare_loom(
                window,
                sample_rate,
                observations,
                config,
                start_sample,
                end_sample,
            )
            .map_err(|error| error.to_string())?;
            Ok::<_, String>((prepared, source_pin, template_source_pin, descriptor))
        });
        cx.spawn(async move |this, cx| {
            let prepared = preparation.await;
            let (ticket, source_pin, template_source_pin, descriptor) =
                match this.update(cx, |this, cx| {
                    if !this.loom_freshness.still_current(requested) {
                        return None;
                    }
                    match prepared {
                        Ok((prepared, source_pin, template_source_pin, descriptor)) => {
                            match this
                                .workbench
                                .read(cx)
                                .analysis_runtime
                                .submit_prepared(owner, prepared)
                            {
                                Ok(ticket) => {
                                    this.loom_cancellation = Some(ticket.cancellation());
                                    Some((ticket, source_pin, template_source_pin, descriptor))
                                }
                                Err(error) => {
                                    this.loom_state = LoomViewState::Failed(error.to_string());
                                    cx.notify();
                                    None
                                }
                            }
                        }
                        Err(error) => {
                            this.loom_state = LoomViewState::Failed(format!(
                                "Loom inference could not retain its project receipt · {error}"
                            ));
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
                if !this.loom_freshness.still_current(requested) {
                    return;
                }
                this.loom_cancellation = None;
                this.loom_state = match completion {
                    Ok(completion) => match completion.product.as_ref() {
                        AnalysisProduct::Loom(product) => {
                            let product = Arc::clone(product);
                            let workbench = this.workbench.clone();
                            let publication = workbench.update(cx, |workbench, cx| {
                                let cancellation = RenderCancellation::new();
                                let findings = workbench
                                    .session
                                    .update(cx, |session, _| {
                                        session.publish_loom_evidence(
                                            descriptor.clone(),
                                            product.sketch.as_ref().clone(),
                                            product.start_sample as u64,
                                            &cancellation,
                                        )
                                    })
                                    .map_err(|error| error.to_string())?;
                                let registered = workbench.register_loom_analysis_results(
                                    &descriptor,
                                    &findings,
                                    &source_pin,
                                    Arc::clone(&product.original),
                                    &product.sketch,
                                    cx,
                                )?;
                                let document_count =
                                    workbench.refresh_reverse_surface_documents(cx)?;
                                workbench.constructive_status = Some(format!(
                                    "Published {registered} Loom Finding(s) across {document_count} reverse documents"
                                ));
                                Ok::<_, String>(Arc::<[AnalysisEvidenceDocumentSummary]>::from(
                                    findings,
                                ))
                            });
                            match publication {
                                Ok(findings) => LoomViewState::Ready(
                                    loom_view_result_from_product(
                                        &product,
                                        sample_rate,
                                        start_seconds,
                                        end_seconds,
                                        source_pin,
                                        template_source_pin,
                                        template_start_seconds,
                                        template_end_seconds,
                                        findings,
                                        settings,
                                    ),
                                ),
                                Err(error) => LoomViewState::Failed(format!(
                                    "Loom completed but its Findings could not publish · {error}"
                                )),
                            }
                        }
                        other => LoomViewState::Failed(format!(
                            "analysis runtime returned {} to the Loom pane",
                            other.kind_name()
                        )),
                    },
                    Err(error) => LoomViewState::Failed(error.to_string()),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Step the lookbehind the templates are gathered from. Like the rhythm
    /// knobs this changes the question, not the answer: Reinfer asks it.
    pub(super) fn step_loom_window(&mut self, direction: i32, cx: &mut Context<Self>) {
        let mut settings = self.loom_settings.normalized();
        if !settings.step_lookbehind(direction) {
            self.say(
                format!(
                    "Loom · the template window is already {} s · the lens offers {}",
                    settings.lookbehind_seconds(),
                    crate::loom::LOOM_LOOKBEHIND_SECONDS
                        .iter()
                        .map(|seconds| format!("{seconds} s"))
                        .collect::<Vec<_>>()
                        .join(" / ")
                ),
                cx,
            );
            return;
        }
        self.loom_settings = settings;
        self.remember_loom_choices();
        self.say(self.loom_knob_notice(), cx);
        cx.notify();
    }

    /// Step the template length. 240 ms is a transient; a second can hold a
    /// chord stab or a short phrase.
    pub(super) fn step_loom_template_length(&mut self, direction: i32, cx: &mut Context<Self>) {
        let mut settings = self.loom_settings.normalized();
        if !settings.step_template_length(direction) {
            self.say(
                format!(
                    "Loom · the template is already {} ms · the lens offers {}",
                    settings.template_milliseconds(),
                    crate::loom::LOOM_TEMPLATE_MILLISECONDS
                        .iter()
                        .map(|ms| format!("{ms} ms"))
                        .collect::<Vec<_>>()
                        .join(" / ")
                ),
                cx,
            );
            return;
        }
        self.loom_settings = settings;
        self.remember_loom_choices();
        self.say(self.loom_knob_notice(), cx);
        cx.notify();
    }

    fn loom_knob_notice(&self) -> String {
        let settings = self.loom_settings.normalized();
        let asked = format!(
            "Loom · templates {} ms long, gathered from {} s of lookbehind",
            settings.template_milliseconds(),
            settings.lookbehind_seconds()
        );
        match &self.loom_state {
            LoomViewState::Ready(result) if result.settings != settings => format!(
                "{asked} · the sketch on screen was inferred at {} ms / {} s · press Reinfer to ask again",
                result.settings.template_milliseconds(),
                result.settings.lookbehind_seconds()
            ),
            LoomViewState::Ready(_) => {
                format!("{asked} · this is what the sketch on screen was inferred at")
            }
            _ => format!("{asked} · press Reinfer to infer with it"),
        }
    }

    /// Persist the Loom knobs. Only an explicit press writes them.
    pub(super) fn remember_loom_choices(&self) {
        let settings = self.loom_settings;
        if let Err(error) = crate::preferences::update(|preferences| {
            preferences.loom = Some(settings);
        }) {
            eprintln!("preferences not saved: {error}");
        }
    }

    pub(super) fn loom_result_is_stale(&self) -> bool {
        match &self.loom_state {
            LoomViewState::Ready(result) => result.settings != self.loom_settings.normalized(),
            _ => false,
        }
    }

    pub(super) fn loom_finding_count(&self) -> usize {
        match &self.loom_state {
            LoomViewState::Ready(result) => result.findings.len(),
            _ => 0,
        }
    }

    /// A press inside the event plot. On a painted event it becomes the
    /// event the edits land on, and its cluster becomes the selected one;
    /// anywhere else it is a seek, which this plot never offered before.
    pub(super) fn press_loom_plot(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let painted = self.timeline_bounds.lock().ok().and_then(|bounds| *bounds);
        let (bounds, _) = press_geometry(painted, self.kind);
        let found = match &self.loom_state {
            LoomViewState::Ready(result) => loom_event_at(
                bounds,
                &result.sketch,
                result.start_seconds,
                result.end_seconds,
                position,
            )
            .map(|event| (event, result.sample_rate)),
            _ => None,
        };
        let Some((event, sample_rate)) = found else {
            self.seek_within(bounds, position, cx);
            let seconds = self.workbench.read(cx).playhead_seconds;
            self.say(
                format!(
                    "Loom · no painted event under that press · sought {} by position · the edits still target {}",
                    format_time(seconds),
                    match &self.loom_state {
                        LoomViewState::Ready(result) if result.selected_event.is_some() =>
                            "the event you pressed before",
                        _ => "the event nearest the playhead",
                    }
                ),
                cx,
            );
            return;
        };
        let row = {
            let LoomViewState::Ready(result) = &mut self.loom_state else {
                return;
            };
            let row = result
                .sketch
                .clusters
                .iter()
                .position(|cluster| cluster.template.cluster_id == event.cluster_id);
            if let Some(row) = row {
                result.selected_cluster = row;
            }
            result.selected_event = Some(event.event_id);
            row
        };
        let seconds = event.sample_index as f64 / f64::from(sample_rate.max(1));
        self.say(
            format!(
                "Loom · event {} of cluster {} at {} is the one the edits will land on",
                event.event_id,
                row.map_or(event.cluster_id + 1, |row| row + 1),
                format_time(seconds)
            ),
            cx,
        );
        cx.notify();
    }

    pub(super) fn cancel_loom_job(&mut self) {
        if let Some(cancellation) = self.loom_cancellation.take() {
            cancellation.cancel();
        }
        self.loom_freshness.bump();
    }

    pub(super) fn rerender_loom_span(&mut self, cx: &mut Context<Self>) {
        let source = {
            let workbench = self.workbench.read(cx);
            workbench
                .analysis()
                .map(|analysis| -> Result<_, String> {
                    let frame_count = analysis.waveform_pyramid.frame_count();
                    let start_sample = (self.time_start * frame_count as f64).floor() as usize;
                    let end_sample = (self.time_end * frame_count as f64).ceil() as usize;
                    let original = analysis.mono_range(start_sample, end_sample);
                    let start = i64::try_from(start_sample)
                        .map_err(|_| "Loom span start exceeds the signed timeline".to_owned())?;
                    let end = i64::try_from(end_sample)
                        .map_err(|_| "Loom span end exceeds the signed timeline".to_owned())?;
                    let span = RenderSpan::new(start, end).map_err(|error| error.to_string())?;
                    let source =
                        workbench.capture_pane_source(span, analysis.sample_rate, &original, cx)?;
                    let current = workbench.pane_audition_context(cx)?;
                    Ok((
                        start_sample,
                        end_sample,
                        analysis.sample_rate,
                        original,
                        source,
                        current,
                    ))
                })
                .transpose()
        };
        let Ok(Some((start_sample, end_sample, sample_rate, original, source, current))) = source
        else {
            return;
        };
        let pinned = {
            let LoomViewState::Ready(result) = &self.loom_state else {
                return;
            };
            result.template_source.validate_current(
                current.document_generation,
                current.publication_generation,
                current.revisions,
                current.audible_cohort.as_ref(),
            )
        };
        if let Err(error) = pinned {
            self.say(
                format!(
                    "Loom · the view moved, but this sketch's templates are pinned to material the project has since replaced · {error} · press Reinfer"
                ),
                cx,
            );
            return;
        }
        let LoomViewState::Ready(result) = &mut self.loom_state else {
            return;
        };
        result.source = source;
        update_loom_render(result, original, start_sample, end_sample, sample_rate);
        cx.notify();
    }

    pub(super) fn cycle_loom_cluster(&mut self, direction: i32, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &mut self.loom_state else {
            self.say(
                "Loom · there is no sequence hypothesis yet, so there are no clusters to cycle",
                cx,
            );
            return;
        };
        let count = result.sketch.clusters.len();
        if count == 0 {
            self.say(
                "Loom · this sketch has no clusters · nothing recurred often enough to template",
                cx,
            );
            return;
        }
        result.selected_cluster = if direction < 0 {
            (result.selected_cluster + count - 1) % count
        } else {
            (result.selected_cluster + 1) % count
        };
        // A cluster the musician moved to owns the choice of event again.
        result.selected_event = None;
        cx.notify();
    }

    pub(super) fn toggle_loom_cluster(&mut self, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &self.loom_state else {
            self.say(
                "Loom · there is no sequence hypothesis yet, so there is no cluster to mute",
                cx,
            );
            return;
        };
        let Some(cluster_id) = selected_loom_cluster_id(result) else {
            self.say("Loom · this sketch has no cluster selected to mute", cx);
            return;
        };
        let Some(cluster) = result.sketch.cluster(cluster_id) else {
            self.say(
                format!("Loom · cluster {} is not in this sketch", cluster_id + 1),
                cx,
            );
            return;
        };
        let (enabled, gain) = (!cluster.enabled, cluster.gain);
        self.commit_loom_cluster_edit(cluster_id, enabled, gain, cx);
    }

    pub(super) fn adjust_loom_cluster_gain(&mut self, delta: f32, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &self.loom_state else {
            self.say(
                "Loom · there is no sequence hypothesis yet, so there is no cluster gain to move",
                cx,
            );
            return;
        };
        let Some(cluster_id) = selected_loom_cluster_id(result) else {
            self.say("Loom · this sketch has no cluster selected", cx);
            return;
        };
        let Some(cluster) = result.sketch.cluster(cluster_id) else {
            self.say(
                format!("Loom · cluster {} is not in this sketch", cluster_id + 1),
                cx,
            );
            return;
        };
        let (enabled, gain) = (cluster.enabled, (cluster.gain + delta).clamp(0.0, 4.0));
        self.commit_loom_cluster_edit(cluster_id, enabled, gain, cx);
    }

    /// A binding outlives neither its pattern nor its kit. When undo walks
    /// past "Make pattern", or either object is deleted, the pane says so and
    /// returns to editing the sketch instead of addressing objects that are
    /// gone.
    fn revalidate_loom_binding(&mut self, cx: &mut Context<Self>) {
        let stale = {
            let LoomViewState::Ready(result) = &self.loom_state else {
                return;
            };
            let Some(binding) = result.binding.as_ref() else {
                return;
            };
            let workbench = self.workbench.read(cx);
            match workbench.session.read(cx).project_snapshot() {
                Ok(snapshot) => {
                    let state = snapshot.project.state();
                    state
                        .domains
                        .sequencer
                        .patterns()
                        .get(binding.pattern)
                        .is_none()
                        || !state.domains.sample_kits.kits.contains_key(&binding.kit)
                }
                Err(_) => true,
            }
        };
        if !stale {
            return;
        }
        if let LoomViewState::Ready(result) = &mut self.loom_state {
            result.binding = None;
        }
        self.workbench.update(cx, |workbench, cx| {
            workbench.constructive_status = Some(
                "Loom · the pattern this pane made is gone · these edits are sketch-only again"
                    .into(),
            );
            cx.notify();
        });
    }

    /// Before "Make pattern" this only moves the sketch. After it, the kit the
    /// construction made is the thing being edited, so the project is asked
    /// first and the sketch follows only a committed revision — otherwise the
    /// pane would show a change the project refused.
    fn commit_loom_cluster_edit(
        &mut self,
        cluster_id: usize,
        enabled: bool,
        gain: f32,
        cx: &mut Context<Self>,
    ) {
        self.revalidate_loom_binding(cx);
        let bound = {
            let LoomViewState::Ready(result) = &self.loom_state else {
                return;
            };
            result.binding.as_ref().map(|binding| {
                (
                    binding.kit,
                    binding.clusters.get(&cluster_id).cloned(),
                    binding.pattern,
                )
            })
        };
        if let Some((kit, cluster, pattern)) = bound {
            let label = format!("cluster {}", cluster_id + 1);
            let requested = if enabled {
                format!("set {label} gain to {gain:.2}×")
            } else {
                format!("mute {label} (pad driven to {LOOM_MUTE_DB:.0} dB)")
            };
            let Some(cluster) = cluster else {
                self.workbench.update(cx, |workbench, cx| {
                    workbench.constructive_status = Some(format!(
                        "Loom · {requested} · refused · {label} was never published into pattern {}",
                        pattern.get()
                    ));
                    cx.notify();
                });
                return;
            };
            let committed = self.workbench.update(cx, |workbench, cx| {
                workbench.constructive_status = Some(format!("Loom · {requested} · requested"));
                let outcome = workbench.session.update(cx, |session, _| {
                    session.execute_loom_cluster_edit(LoomClusterEditIntent {
                        kit,
                        cluster,
                        enabled,
                        gain,
                    })
                });
                let committed = match outcome {
                    Ok(outcome) => {
                        workbench.constructive_status = Some(format!(
                            "Loom · {requested} · committed at revision {}",
                            outcome.publication.revision
                        ));
                        true
                    }
                    Err(error) => {
                        workbench.constructive_status =
                            Some(format!("Loom · {requested} · refused · {error}"));
                        false
                    }
                };
                workbench.handle_session_events(cx);
                cx.notify();
                committed
            });
            if !committed {
                return;
            }
        }
        let LoomViewState::Ready(result) = &mut self.loom_state else {
            return;
        };
        result.sketch.set_cluster_enabled(cluster_id, enabled);
        result.sketch.set_cluster_gain(cluster_id, gain);
        result.diverged_from_evidence = true;
        rebuild_loom_audio(result);
        let retained = loom_construction_product_from_result(result);
        if let Some((artifact, product)) = retained {
            self.workbench.update(cx, |workbench, _| {
                workbench
                    .loom_construction_products
                    .insert(artifact, product);
            });
        }
        cx.notify();
    }

    pub(super) fn edit_nearest_loom_event(
        &mut self,
        timing_delta_seconds: f64,
        gain_delta: f32,
        toggle: bool,
        cx: &mut Context<Self>,
    ) {
        let playhead_sample = {
            let workbench = self.workbench.read(cx);
            workbench
                .analysis()
                .map(|analysis| {
                    (workbench.playhead_seconds * f64::from(analysis.sample_rate)).round() as i64
                })
                .unwrap_or(0)
        };
        let candidate = {
            let LoomViewState::Ready(result) = &self.loom_state else {
                self.say(
                    "Loom · there is no sequence hypothesis yet, so there is no event to edit",
                    cx,
                );
                return;
            };
            let Some(cluster_id) = selected_loom_cluster_id(result) else {
                self.say("Loom · this sketch has no cluster selected to edit", cx);
                return;
            };
            // A press on a painted event chooses it. Without a press the
            // edits still go to the event nearest the playhead, which is
            // what they always did.
            let chosen = result.selected_event.filter(|event_id| {
                result
                    .sketch
                    .event(*event_id)
                    .is_some_and(|event| event.cluster_id == cluster_id)
            });
            let Some(event_id) =
                chosen.or_else(|| nearest_loom_event(&result.sketch, cluster_id, playhead_sample))
            else {
                self.say(
                    format!("Loom · cluster {} has no events to edit", cluster_id + 1),
                    cx,
                );
                return;
            };
            let Some(event) = result.sketch.event(event_id) else {
                self.say(
                    format!("Loom · event {event_id} is no longer in this sketch"),
                    cx,
                );
                return;
            };
            let chosen = chosen.is_some();
            let sample_index = if timing_delta_seconds == 0.0 {
                event.sample_index
            } else {
                event.sample_index
                    + (timing_delta_seconds * f64::from(result.sample_rate)).round() as i64
            };
            let gain = if gain_delta == 0.0 {
                event.gain
            } else {
                (event.gain + gain_delta).clamp(0.0, 4.0)
            };
            let enabled = if toggle {
                !event.enabled
            } else {
                event.enabled
            };
            (cluster_id, event_id, sample_index, gain, enabled, chosen)
        };
        let (cluster_id, event_id, sample_index, gain, enabled, chosen) = candidate;
        // Which event an edit lands on was never said out loud; a musician
        // who pressed one needs to know the press is what is being obeyed.
        self.say(
            format!(
                "Loom · editing event {event_id} of cluster {} · {}",
                cluster_id + 1,
                if chosen {
                    "the event you pressed"
                } else {
                    "the event nearest the playhead; press one in the plot to choose it"
                }
            ),
            cx,
        );
        self.revalidate_loom_binding(cx);
        let bound = {
            let LoomViewState::Ready(result) = &self.loom_state else {
                return;
            };
            result.binding.as_ref().map(|binding| {
                (
                    binding.pattern,
                    binding.placement_start,
                    binding.resolution,
                    binding.events.get(&event_id).copied(),
                    binding
                        .clusters
                        .get(&cluster_id)
                        .map(|cluster| cluster.event_gain_max),
                )
            })
        };
        let mut moved_to = None;
        if let Some((pattern, placement_start, resolution, address, event_gain_max)) = bound {
            let requested = format!("edit event {event_id}");
            let (Some(address), Some(event_gain_max)) = (address, event_gain_max) else {
                self.workbench.update(cx, |workbench, cx| {
                    workbench.constructive_status = Some(format!(
                        "Loom · {requested} · refused · that event was never published into pattern {}",
                        pattern.get()
                    ));
                    cx.notify();
                });
                return;
            };
            let intent = LoomEventEditIntent {
                pattern,
                address,
                placement_start,
                resolution,
                sample_index,
                enabled,
                gain,
                event_gain_max,
            };
            moved_to = self.workbench.update(cx, |workbench, cx| {
                workbench.constructive_status = Some(format!("Loom · {requested} · requested"));
                let planned = {
                    let session = workbench.session.read(cx);
                    session
                        .project_snapshot()
                        .map_err(|error| error.to_string())
                        .and_then(|snapshot| {
                            plan_loom_event_edit(snapshot, intent)
                                .map_err(|error| error.to_string())
                        })
                };
                let plan = match planned {
                    Ok(plan) => plan,
                    Err(error) => {
                        workbench.constructive_status =
                            Some(format!("Loom · {requested} · refused · {error}"));
                        cx.notify();
                        return None;
                    }
                };
                let described = plan.requested.clone();
                let address = plan.address;
                workbench.constructive_status = Some(format!("Loom · {described} · requested"));
                let outcome = workbench
                    .session
                    .update(cx, |session, _| session.execute_pattern_workflow(plan.workflow));
                let committed = match outcome {
                    Ok(_) => {
                        let revision = workbench
                            .session
                            .read(cx)
                            .project_snapshot()
                            .map(|snapshot| snapshot.revisions().aggregate);
                        workbench.constructive_status = Some(match revision {
                            Ok(revision) => format!(
                                "Loom · {described} · committed at revision {revision}"
                            ),
                            Err(error) => {
                                format!("Loom · {described} · committed · revision unreadable · {error}")
                            }
                        });
                        Some(address)
                    }
                    Err(error) => {
                        workbench.constructive_status =
                            Some(format!("Loom · {described} · refused · {error}"));
                        None
                    }
                };
                workbench.handle_session_events(cx);
                cx.notify();
                committed
            });
            if moved_to.is_none() {
                return;
            }
        }
        let LoomViewState::Ready(result) = &mut self.loom_state else {
            return;
        };
        result.sketch.move_event(event_id, sample_index);
        result.sketch.set_event_gain(event_id, gain);
        result.sketch.set_event_enabled(event_id, enabled);
        if let (Some(binding), Some(address)) = (result.binding.as_mut(), moved_to) {
            binding.events.insert(event_id, address);
        }
        result.diverged_from_evidence = true;
        rebuild_loom_audio(result);
        let retained = loom_construction_product_from_result(result);
        if let Some((artifact, product)) = retained {
            self.workbench.update(cx, |workbench, _| {
                workbench
                    .loom_construction_products
                    .insert(artifact, product);
            });
        }
        cx.notify();
    }

    pub(super) fn audition_loom(&mut self, kind: LoomAudition, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &self.loom_state else {
            self.say(
                format!(
                    "Loom · there is no sequence hypothesis yet, so there is no {} to hear · press Reinfer",
                    match kind {
                        LoomAudition::Original => "mix",
                        LoomAudition::Reconstruction => "render",
                        LoomAudition::Residual => "residual",
                        LoomAudition::Template => "template",
                    }
                ),
                cx,
            );
            return;
        };
        let sample_rate = result.sample_rate;
        let owner = self.audition_owner;
        let aligned = match kind {
            LoomAudition::Original => Some((result.original.clone(), PaneAudioKind::LoomSource)),
            LoomAudition::Reconstruction => Some((
                result.reconstruction.clone(),
                PaneAudioKind::LoomConstruction,
            )),
            LoomAudition::Residual => Some((result.residual.clone(), PaneAudioKind::LoomResidual)),
            LoomAudition::Template => None,
        };
        let source = result.source.clone();
        let template_source = result.template_source.clone();
        // The template audition is the answer to "what will Make pattern make",
        // so it renders the cluster as edited: gain scaled in, and a muted
        // cluster refused rather than played as if it were still in.
        let selected = selected_loom_cluster_id(result)
            .and_then(|cluster_id| result.sketch.cluster(cluster_id).map(|c| (cluster_id, c)));
        let template = match selected {
            Some((cluster_id, cluster)) if !cluster.enabled => Err(format!(
                "Cluster {} is muted, so it contributes nothing; unmute it to hear its template",
                cluster_id + 1
            )),
            Some((_, cluster)) => Ok(Arc::<[f32]>::from(
                cluster
                    .template
                    .samples
                    .iter()
                    .map(|sample| sample * cluster.gain)
                    .collect::<Vec<_>>(),
            )),
            None => Err("The selected Loom template is empty".to_owned()),
        };
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| match (aligned, template) {
            (Some((samples, kind)), _) => {
                workbench.audition_pane_timeline(owner, kind, source, samples, cx)
            }
            (None, Ok(template)) => workbench.preview_pane_mono(
                owner,
                PaneAudioKind::LoomTemplate,
                &template_source,
                sample_rate,
                template,
                cx,
            ),
            (None, Err(message)) => {
                workbench.audio_error = Some(message.into());
                cx.notify();
            }
        });
    }

    pub(super) fn render_loom(
        &self,
        _analysis: Arc<Analysis>,
        playhead_seconds: f64,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match &self.loom_state {
            LoomViewState::Idle => empty_state(
                "No editable reconstruction yet",
                "Infer recurring excerpts and their event sequence for this material.",
            ),
            LoomViewState::Inferring {
                start_seconds,
                end_seconds,
                event_count,
                template_start_seconds,
                template_end_seconds,
            } => empty_state(
                "Inferring reusable event templates…",
                &format!(
                    "Aligning {event_count} mixed-signal occurrences from {}–{}, then rendering {}–{}.",
                    format_time(*template_start_seconds),
                    format_time(*template_end_seconds),
                    format_time(*start_seconds),
                    format_time(*end_seconds)
                ),
            ),
            LoomViewState::Failed(error) => empty_state("The sequence hypothesis failed", error),
            LoomViewState::Ready(result) => {
                let cluster_count = result.sketch.clusters.len();
                let selected = result
                    .sketch
                    .clusters
                    .get(result.selected_cluster.min(cluster_count.saturating_sub(1)));
                let selected_cluster_id = selected
                    .map(|cluster| cluster.template.cluster_id)
                    .unwrap_or(0);
                let selected_events = result
                    .sketch
                    .events
                    .iter()
                    .filter(|event| event.cluster_id == selected_cluster_id)
                    .count();
                let selected_gain = selected.map_or(0.0, |cluster| cluster.gain);
                let selected_enabled = selected.is_some_and(|cluster| cluster.enabled);
                let agreement = selected.map_or(0.0, |cluster| cluster.template.exemplar_agreement);
                let template = selected
                    .map(|cluster| mono_waveform_bins(&cluster.template.samples, 1_200))
                    .unwrap_or_default();
                let local_playhead = ((playhead_seconds - result.start_seconds)
                    / (result.end_seconds - result.start_seconds).max(f64::EPSILON))
                    as f32;
                let original = Arc::clone(&result.original_waveform);
                let reconstruction = Arc::clone(&result.reconstruction_waveform);
                let residual = Arc::clone(&result.residual_waveform);
                // Explained energy is `1 - residual/source` and goes negative
                // when the hypothesis leaves more residual than there was
                // source -- which one press of LEN + can do, because long
                // templates overlap. "-71.9% explained" is not a sentence;
                // say what actually happened.
                let explained = result.fit.explained_energy * 100.0;
                let fit_sentence = if explained < 0.0 {
                    format!(
                        "worse than silence · the residual carries {:.1}% more energy than the source",
                        -explained
                    )
                } else {
                    format!("{explained:.1}% source energy explained")
                };
                let phase = loom_phase_label(self.workbench.read(cx), result, cx);
                let bound = result.binding.is_some();
                let settings = self.loom_settings.normalized();
                let asked_settings = result.settings;
                let stale = asked_settings != settings;
                let finding_count = result.findings.len();
                let selected_finding = self.selected_finding.min(finding_count.saturating_sub(1));
                let selected_event = result.selected_event;
                let selected_event_label = selected_event
                    .and_then(|event_id| result.sketch.event(event_id))
                    .map(|event| {
                        format!(
                            "event {} at {}",
                            event.id,
                            format_time(
                                event.sample_index as f64 / f64::from(result.sample_rate.max(1))
                            )
                        )
                    });
                let plot_bounds = Arc::clone(&self.timeline_bounds);

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
                            .gap_3()
                            .bg(rgb(PANEL_ALT))
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if bound { rgb(CYAN) } else { rgb(MUTED) })
                                    .child(phase),
                            )
                            .child(
                                div()
                                    .text_color(if explained < 0.0 {
                                        rgb(AMBER)
                                    } else {
                                        rgb(CYAN)
                                    })
                                    .child(fit_sentence),
                            )
                            .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                                "correlation {:+.3}  ·  {} templates / {} events  ·  {} ms templates from {}–{} ({} s lookbehind)  ·  editable overlap-add render",
                                result.fit.correlation,
                                cluster_count,
                                result.sketch.events.len(),
                                asked_settings.template_milliseconds(),
                                format_time(result.template_start_seconds),
                                format_time(result.template_end_seconds),
                                asked_settings.lookbehind_seconds(),
                            )))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(viz_control("hear-loom-mix", "Mix").px_2().on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.audition_loom(LoomAudition::Original, cx)
                                        }),
                                    ))
                                    .child(
                                        viz_control("hear-loom-render", "Render")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.audition_loom(
                                                    LoomAudition::Reconstruction,
                                                    cx,
                                                )
                                            })),
                                    )
                                    .child(
                                        viz_control("hear-loom-residual", "Residual")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.audition_loom(LoomAudition::Residual, cx)
                                            })),
                                    )
                                    .child(
                                        viz_control("hear-loom-template", "Template")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.audition_loom(LoomAudition::Template, cx)
                                            })),
                                    )
                                    .child(
                                        viz_control("loom-finding-prev", "◂").on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.step_selected_finding(-1, finding_count, cx)
                                            }),
                                        ),
                                    )
                                    .child(
                                        viz_control("open-loom-finding", "Open Finding")
                                            .px_2()
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.open_loom_finding(selected_finding, cx)
                                            })),
                                    )
                                    .child(
                                        div()
                                            .min_w(px(46.0))
                                            .text_xs()
                                            .text_color(rgb(MUTED))
                                            .child(if finding_count == 0 {
                                                "0 of 0".to_owned()
                                            } else {
                                                format!(
                                                    "{} of {finding_count}",
                                                    selected_finding + 1
                                                )
                                            }),
                                    )
                                    .child(
                                        viz_control("loom-finding-next", "▸").on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.step_selected_finding(1, finding_count, cx)
                                            }),
                                        ),
                                    )
                                    .child(
                                        viz_control("keep-loom-finding", "Keep finding")
                                            .px_2()
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.keep_loom_finding(selected_finding, cx)
                                            })),
                                    )
                                    .child(
                                        viz_control("apply-loom-sequence", "Make Pattern")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.apply_loom_sequence(cx)
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .h(px(42.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px_4()
                            .gap_1()
                            .bg(rgb(PANEL))
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(viz_control("loom-cluster-prev", "Cluster ‹").px_2().on_click(
                                cx.listener(|this, _, _, cx| this.cycle_loom_cluster(-1, cx)),
                            ))
                            .child(
                                div()
                                    .min_w(px(178.0))
                                    .px_2()
                                    .text_xs()
                                    .text_color(cluster_color(selected_cluster_id))
                                    .child(format!(
                                        "Cluster {} · {} events · {:.0}% agreement",
                                        selected_cluster_id + 1,
                                        selected_events,
                                        agreement * 100.0
                                    )),
                            )
                            .child(viz_control("loom-cluster-next", "Cluster ›").px_2().on_click(
                                cx.listener(|this, _, _, cx| this.cycle_loom_cluster(1, cx)),
                            ))
                            .child(viz_control("loom-cluster-toggle", "Mute/on").px_2().on_click(
                                cx.listener(|this, _, _, cx| this.toggle_loom_cluster(cx)),
                            ))
                            .child(viz_control("loom-cluster-gain-down", "Gain −").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.adjust_loom_cluster_gain(-0.1, cx)
                                }),
                            ))
                            .child(
                                div().min_w(px(48.0)).text_xs().text_color(if selected_enabled {
                                    rgb(TEXT)
                                } else {
                                    rgb(DIM)
                                }).child(format!("{selected_gain:.2}×")),
                            )
                            .child(viz_control("loom-cluster-gain-up", "Gain +").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.adjust_loom_cluster_gain(0.1, cx)
                                }),
                            ))
                            .child(div().w(px(10.0)))
                            .child(viz_control("loom-event-left", "Event −10ms").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.edit_nearest_loom_event(-0.010, 0.0, false, cx)
                                }),
                            ))
                            .child(viz_control("loom-event-right", "Event +10ms").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.edit_nearest_loom_event(0.010, 0.0, false, cx)
                                }),
                            ))
                            .child(viz_control("loom-event-gain-down", "Ev −").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.edit_nearest_loom_event(0.0, -0.1, false, cx)
                                }),
                            ))
                            .child(viz_control("loom-event-gain-up", "Ev +").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.edit_nearest_loom_event(0.0, 0.1, false, cx)
                                }),
                            ))
                            .child(viz_control("loom-event-toggle", "Ev on/off").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.edit_nearest_loom_event(0.0, 0.0, true, cx)
                                }),
                            ))
                            .child(div().flex_1())
                            .child(
                                div().text_xs().text_color(rgb(DIM)).child(
                                    selected_event_label.clone().map_or_else(
                                        || "editing the event nearest the playhead".to_owned(),
                                        |label| format!("editing {label}"),
                                    ),
                                ),
                            ),
                    )
                    .child(
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
                            .child(viz_control("loom-window-down", "WINDOW −").px_2().on_click(
                                cx.listener(|this, _, _, cx| this.step_loom_window(-1, cx)),
                            ))
                            .child(
                                div()
                                    .min_w(px(46.0))
                                    .px_1()
                                    .text_xs()
                                    .text_color(rgb(TEXT))
                                    .child(format!("{} s", settings.lookbehind_seconds())),
                            )
                            .child(viz_control("loom-window-up", "WINDOW +").px_2().on_click(
                                cx.listener(|this, _, _, cx| this.step_loom_window(1, cx)),
                            ))
                            .child(div().w(px(8.0)))
                            .child(viz_control("loom-len-down", "LEN −").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.step_loom_template_length(-1, cx)
                                }),
                            ))
                            .child(
                                div()
                                    .min_w(px(58.0))
                                    .px_1()
                                    .text_xs()
                                    .text_color(rgb(TEXT))
                                    .child(format!("{} ms", settings.template_milliseconds())),
                            )
                            .child(viz_control("loom-len-up", "LEN +").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.step_loom_template_length(1, cx)
                                }),
                            ))
                            .child(div().w(px(8.0)))
                            .child(viz_control("loom-reinfer", "Reinfer").px_2().on_click(
                                cx.listener(|this, _, _, cx| this.refresh_loom(cx)),
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .px_2()
                                    .text_xs()
                                    .text_color(if stale { rgb(AMBER) } else { rgb(DIM) })
                                    .child(if stale {
                                        format!(
                                            "inferred at {} ms / {} s · Reinfer to ask again",
                                            asked_settings.template_milliseconds(),
                                            asked_settings.lookbehind_seconds()
                                        )
                                    } else {
                                        "these are the knobs this sketch was inferred at".to_owned()
                                    }),
                            ),
                    )
                    .child(time_ruler_range(result.start_seconds, result.end_seconds))
                    .child(lane(
                        "SELECTED REUSABLE MIXED-SIGNAL TEMPLATE",
                        px(78.0),
                        waveform_plot(
                            template,
                            -1.0,
                            Arc::clone(&self.waveform_geometry),
                            WaveformRenderKey::fractions(
                                20,
                                self.loom_freshness.epoch().get(),
                                result.start_seconds,
                                result.end_seconds,
                            ),
                        ),
                    ))
                    .child(
                        div()
                            .relative()
                            .h(px(LOOM_EVENT_LANE_HEIGHT))
                            .flex_none()
                            .cursor_crosshair()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                    this.press_loom_plot(event.position, cx)
                                }),
                            )
                            .child(lane(
                                "EDITABLE EVENT SEQUENCE · HEIGHT = GAIN · DIM = DISABLED · PRESS AN EVENT TO EDIT IT",
                                px(LOOM_EVENT_LANE_HEIGHT),
                                loom_event_plot(
                                    result.sketch.clone(),
                                    result.start_seconds,
                                    result.end_seconds,
                                    local_playhead,
                                    selected_cluster_id,
                                    selected_event,
                                    plot_bounds,
                                ),
                            )),
                    )
                    .child(lane(
                        "ORIGINAL MIX",
                        px(78.0),
                        waveform_plot(
                            original,
                            local_playhead,
                            Arc::clone(&self.waveform_geometry),
                            WaveformRenderKey::fractions(
                                21,
                                self.loom_freshness.epoch().get(),
                                result.start_seconds,
                                result.end_seconds,
                            ),
                        ),
                    ))
                    .child(lane(
                        "EVENT-TEMPLATE RECONSTRUCTION",
                        px(78.0),
                        waveform_plot(
                            reconstruction,
                            local_playhead,
                            Arc::clone(&self.waveform_geometry),
                            WaveformRenderKey::fractions(
                                22,
                                self.loom_freshness.epoch().get(),
                                result.start_seconds,
                                result.end_seconds,
                            ),
                        ),
                    ))
                    .child(lane(
                        "UNEXPLAINED RESIDUAL · ORIGINAL − RECONSTRUCTION",
                        px(78.0),
                        waveform_plot(
                            residual,
                            local_playhead,
                            Arc::clone(&self.waveform_geometry),
                            WaveformRenderKey::fractions(
                                23,
                                self.loom_freshness.epoch().get(),
                                result.start_seconds,
                                result.end_seconds,
                            ),
                        ),
                    ))
                    .child(
                        div()
                            .h(px(34.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px_4()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(selected_event_label.map_or_else(
                                || "Edits target the selected cluster and its event nearest the shared playhead; press an event in the sequence to target that one instead. Templates are real aligned excerpts from the mix, so overlapping voices and effects leak into them.".to_owned(),
                                |label| format!("Edits target {label}, the one you pressed; cycling clusters gives the playhead back. Templates are real aligned excerpts from the mix, so overlapping voices and effects leak into them."),
                            )),
                    )
                    .into_any_element()
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn loom_view_result_from_product(
    product: &Arc<LoomAnalysisProduct>,
    sample_rate: u32,
    start_seconds: f64,
    end_seconds: f64,
    source_pin: PaneSourcePin,
    template_source_pin: PaneSourcePin,
    template_start_seconds: f64,
    template_end_seconds: f64,
    findings: Arc<[AnalysisEvidenceDocumentSummary]>,
    settings: LoomLensSettings,
) -> LoomViewResult {
    LoomViewResult {
        source: source_pin.clone(),
        settings,
        selected_event: None,
        artifact_source: source_pin,
        template_source: template_source_pin,
        sketch: product.sketch.as_ref().clone(),
        selected_cluster: 0,
        start_sample: product.start_sample,
        end_sample: product.end_sample,
        start_seconds,
        end_seconds,
        sample_rate,
        original: Arc::clone(&product.original),
        reconstruction: Arc::clone(&product.reconstruction),
        residual: Arc::clone(&product.residual),
        original_waveform: Arc::clone(&product.original_waveform),
        reconstruction_waveform: Arc::clone(&product.reconstruction_waveform),
        residual_waveform: Arc::clone(&product.residual_waveform),
        fit: product.fit,
        template_start_seconds,
        template_end_seconds,
        findings,
        diverged_from_evidence: false,
        binding: None,
    }
}

/// The pane's phase, in the words the audit asked for. Phase 1 says the sketch
/// is not in the project; phase 2 names the two objects it is editing.
pub(super) fn loom_phase_label(workbench: &Workbench, result: &LoomViewResult, cx: &App) -> String {
    let Some(binding) = result.binding.as_ref() else {
        return "SKETCH · not in the project until Make pattern".to_owned();
    };
    let Ok(snapshot) = workbench.session.read(cx).project_snapshot() else {
        return "SKETCH · no project is open, so these edits are sketch-only".to_owned();
    };
    let state = snapshot.project.state();
    match (
        state.domains.sequencer.patterns().get(binding.pattern),
        state.domains.sample_kits.kits.get(&binding.kit),
    ) {
        (Some(pattern), Some(kit)) => {
            format!(
                "BOUND · pattern \"{}\" · kit \"{}\"",
                pattern.name, kit.name
            )
        }
        _ => "SKETCH · the pattern this pane made is gone, so these edits are sketch-only again"
            .to_owned(),
    }
}

pub(super) fn update_loom_render(
    result: &mut LoomViewResult,
    original: Vec<f32>,
    start_sample: usize,
    _end_sample: usize,
    sample_rate: u32,
) {
    let end_sample = start_sample.saturating_add(original.len());
    result.start_sample = start_sample;
    result.end_sample = end_sample;
    result.start_seconds = start_sample as f64 / f64::from(sample_rate);
    result.end_seconds = end_sample as f64 / f64::from(sample_rate);
    result.sample_rate = sample_rate;
    result.original = Arc::from(original);
    rebuild_loom_audio(result);
}

pub(super) fn rebuild_loom_audio(result: &mut LoomViewResult) {
    result.reconstruction = Arc::from(
        result
            .sketch
            .render_span(result.start_sample, result.original.len()),
    );
    result.residual = Arc::from(
        result
            .original
            .iter()
            .zip(result.reconstruction.iter())
            .map(|(source, rendered)| source - rendered)
            .collect::<Vec<_>>(),
    );
    result.original_waveform = Arc::from(mono_waveform_bins(&result.original, 2_400));
    result.reconstruction_waveform = Arc::from(mono_waveform_bins(&result.reconstruction, 2_400));
    result.residual_waveform = Arc::from(mono_waveform_bins(&result.residual, 2_400));
    result.fit = fit_rendered_span(
        &result.original,
        &result.reconstruction,
        result.start_sample,
    );
}

pub(super) fn fit_rendered_span(
    source: &[f32],
    rendered: &[f32],
    start_sample: usize,
) -> FitMetrics {
    let mut source_energy = 0.0_f64;
    let mut rendered_energy = 0.0_f64;
    let mut residual_energy = 0.0_f64;
    let mut dot = 0.0_f64;
    for (&source, &rendered) in source.iter().zip(rendered) {
        let source = f64::from(source);
        let rendered = f64::from(rendered);
        let residual = source - rendered;
        source_energy += source * source;
        rendered_energy += rendered * rendered;
        residual_energy += residual * residual;
        dot += source * rendered;
    }
    let normalized_error = if source_energy <= f64::EPSILON {
        if residual_energy <= f64::EPSILON {
            0.0
        } else {
            f64::INFINITY
        }
    } else {
        residual_energy / source_energy
    };
    let correlation_denominator = (source_energy * rendered_energy).sqrt();
    let correlation = if correlation_denominator <= f64::EPSILON {
        if source_energy <= f64::EPSILON && rendered_energy <= f64::EPSILON {
            1.0
        } else {
            0.0
        }
    } else {
        (dot / correlation_denominator).clamp(-1.0, 1.0) as f32
    };
    FitMetrics {
        start_sample,
        sample_count: source.len().min(rendered.len()),
        source_energy,
        rendered_energy,
        residual_energy,
        normalized_error,
        explained_energy: 1.0 - normalized_error,
        correlation,
    }
}

pub(super) fn selected_loom_cluster_id(result: &LoomViewResult) -> Option<usize> {
    result
        .sketch
        .clusters
        .get(result.selected_cluster)
        .map(|cluster| cluster.template.cluster_id)
}

pub(super) fn loom_construction_product_from_result(
    result: &LoomViewResult,
) -> Option<(ArtifactId, LoomConstructionProduct)> {
    let summary = result
        .findings
        .iter()
        .find(|summary| summary.kind == AnalysisEvidenceKind::LoomSequence)?;
    Some((
        summary.artifact,
        LoomConstructionProduct {
            source: result.artifact_source.clone(),
            sketch: result.sketch.clone(),
            label: summary.label.clone(),
            finding: summary.finding,
            diverged_from_evidence: result.diverged_from_evidence,
        },
    ))
}

pub(super) fn nearest_loom_event(
    sketch: &SequenceSketch,
    cluster_id: usize,
    playhead_sample: i64,
) -> Option<u64> {
    sketch
        .events
        .iter()
        .filter(|event| event.cluster_id == cluster_id)
        .min_by_key(|event| event.sample_index.abs_diff(playhead_sample))
        .map(|event| event.id)
}
