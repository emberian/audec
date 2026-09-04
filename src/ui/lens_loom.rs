//! Loom event-template reconstruction lens.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

use crate::project_controller::{
    plan_loom_event_edit, recommend_constructive, LoomClusterEditIntent, LoomEventEditIntent,
    LOOM_MUTE_DB,
};

impl Visualizer {
    pub(super) fn open_loom_finding(&mut self, index: usize, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &self.loom_state else {
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

    pub(super) fn keep_loom_finding(&mut self, index: usize, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &self.loom_state else {
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

    pub(super) fn apply_loom_sequence(&mut self, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &self.loom_state else {
            return;
        };
        let Some(summary) = result
            .findings
            .iter()
            .find(|summary| summary.kind == AnalysisEvidenceKind::LoomSequence)
        else {
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
            workbench.analysis().and_then(|analysis| {
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
                    Arc::clone(&analysis.mono_pcm),
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
            mono,
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
        let event_count = observations.len();
        let generation = self.loom_generation;
        let config = TemplateBuildConfig::for_sample_rate(sample_rate);
        let owner = AnalysisProductOwner {
            project_session,
            namespace: self.audition_owner.namespace,
            local: self.audition_owner.local ^ 0x6c6f_6f6d,
            pane: Some(self.audition_owner.local),
            generation,
        };
        self.loom_state = LoomViewState::Inferring {
            start_seconds,
            end_seconds,
            event_count,
        };
        cx.notify();

        let preparation = cx.background_spawn(async move {
            let full_end = i64::try_from(frame_count)
                .map_err(|_| "Loom source exceeds the signed project timeline".to_owned())?;
            let full_span = RenderSpan::new(0, full_end).map_err(|error| error.to_string())?;
            let format = RenderFormat::new(sample_rate, 1).map_err(|error| error.to_string())?;
            let template_source_pin = PaneSourcePin::new(
                document_generation,
                publication_generation,
                project_revisions,
                None,
                full_span,
                format,
                mono.as_ref(),
            )
            .map_err(|error| error.to_string())?;
            let start = i64::try_from(start_sample)
                .map_err(|_| "Loom span start exceeds the signed timeline".to_owned())?;
            let end = i64::try_from(end_sample)
                .map_err(|_| "Loom span end exceeds the signed timeline".to_owned())?;
            let span = RenderSpan::new(start, end).map_err(|error| error.to_string())?;
            let original = mono
                .get(start_sample..end_sample)
                .ok_or_else(|| "Loom span lies outside retained PCM".to_owned())?;
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
            let descriptor = loom_artifact_descriptor(mono.as_ref(), &source_pin, config)?;
            let prepared = AnalysisProductRuntime::prepare_loom(
                mono,
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
                    if this.loom_generation != generation {
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
                if this.loom_generation != generation {
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
                                        findings,
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

    pub(super) fn cancel_loom_job(&mut self) {
        if let Some(cancellation) = self.loom_cancellation.take() {
            cancellation.cancel();
        }
        self.loom_generation = self.loom_generation.wrapping_add(1);
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
        let LoomViewState::Ready(result) = &mut self.loom_state else {
            return;
        };
        if result
            .template_source
            .validate_current(
                current.document_generation,
                current.publication_generation,
                current.revisions,
                current.audible_cohort.as_ref(),
            )
            .is_err()
        {
            return;
        }
        result.source = source;
        update_loom_render(result, original, start_sample, end_sample, sample_rate);
        cx.notify();
    }

    pub(super) fn cycle_loom_cluster(&mut self, direction: i32, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &mut self.loom_state else {
            return;
        };
        let count = result.sketch.clusters.len();
        if count == 0 {
            return;
        }
        result.selected_cluster = if direction < 0 {
            (result.selected_cluster + count - 1) % count
        } else {
            (result.selected_cluster + 1) % count
        };
        cx.notify();
    }

    pub(super) fn toggle_loom_cluster(&mut self, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &self.loom_state else {
            return;
        };
        let Some(cluster_id) = selected_loom_cluster_id(result) else {
            return;
        };
        let Some(cluster) = result.sketch.cluster(cluster_id) else {
            return;
        };
        let (enabled, gain) = (!cluster.enabled, cluster.gain);
        self.commit_loom_cluster_edit(cluster_id, enabled, gain, cx);
    }

    pub(super) fn adjust_loom_cluster_gain(&mut self, delta: f32, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &self.loom_state else {
            return;
        };
        let Some(cluster_id) = selected_loom_cluster_id(result) else {
            return;
        };
        let Some(cluster) = result.sketch.cluster(cluster_id) else {
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
                return;
            };
            let Some(cluster_id) = selected_loom_cluster_id(result) else {
                return;
            };
            let Some(event_id) = nearest_loom_event(&result.sketch, cluster_id, playhead_sample)
            else {
                return;
            };
            let Some(event) = result.sketch.event(event_id) else {
                return;
            };
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
            (cluster_id, event_id, sample_index, gain, enabled)
        };
        let (cluster_id, event_id, sample_index, gain, enabled) = candidate;
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
            } => empty_state(
                "Inferring reusable event templates…",
                &format!(
                    "Aligning {event_count} mixed-signal occurrences, then rendering {}–{}.",
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
                let explained = result.fit.explained_energy * 100.0;
                let phase = loom_phase_label(self.workbench.read(cx), result, cx);
                let bound = result.binding.is_some();

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
                            .child(div().text_color(rgb(CYAN)).child(format!(
                                "{explained:.1}% source energy explained"
                            )))
                            .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                                "correlation {:+.3}  ·  {} templates / {} events  ·  editable overlap-add render",
                                result.fit.correlation,
                                cluster_count,
                                result.sketch.events.len(),
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
                                        viz_control("open-loom-finding", "Open Findings")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.open_loom_finding(0, cx)
                                            })),
                                    )
                                    .child(
                                        viz_control("keep-loom-finding", "Keep finding")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.keep_loom_finding(0, cx)
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
                            )),
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
                                self.loom_generation,
                                result.start_seconds,
                                result.end_seconds,
                            ),
                        ),
                    ))
                    .child(lane(
                        "EDITABLE EVENT SEQUENCE · HEIGHT = GAIN · DIM = DISABLED",
                        px(150.0),
                        loom_event_plot(
                            result.sketch.clone(),
                            result.start_seconds,
                            result.end_seconds,
                            local_playhead,
                            selected_cluster_id,
                        ),
                    ))
                    .child(lane(
                        "ORIGINAL MIX",
                        px(78.0),
                        waveform_plot(
                            original,
                            local_playhead,
                            Arc::clone(&self.waveform_geometry),
                            WaveformRenderKey::fractions(
                                21,
                                self.loom_generation,
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
                                self.loom_generation,
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
                                self.loom_generation,
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
                            .child("Edits target the selected cluster and its event nearest the shared playhead. Templates are real aligned excerpts from the mix, so overlapping voices and effects leak into them."),
                    )
                    .into_any_element()
            }
        }
    }
}

pub(super) fn loom_view_result_from_product(
    product: &Arc<LoomAnalysisProduct>,
    sample_rate: u32,
    start_seconds: f64,
    end_seconds: f64,
    source_pin: PaneSourcePin,
    template_source_pin: PaneSourcePin,
    findings: Arc<[AnalysisEvidenceDocumentSummary]>,
) -> LoomViewResult {
    LoomViewResult {
        source: source_pin.clone(),
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
