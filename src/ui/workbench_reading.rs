//! Reading/query workbench and explanation-workbench effects hosted by the Workbench.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

impl Workbench {
    pub(super) fn on_reading_query_effect(
        &mut self,
        source: WorkspaceViewId,
        effect: ReadingQueryViewEffect,
        cx: &mut Context<Self>,
    ) {
        match effect {
            ReadingQueryViewEffect::Command(envelope) => {
                let bridge = self.capture_reading_query_session(cx);
                let result = bridge.and_then(|bridge| {
                    let session = self.session.clone();
                    session
                        .update(cx, |session, _| bridge.apply_command(session, envelope))
                        .map_err(|error| error.to_string())
                });
                let committed = result.is_ok();
                self.constructive_status = Some(match &result {
                    Ok(receipt) => format!(
                        "Reading import committed · project revision {}",
                        receipt.publication.revisions.aggregate
                    ),
                    Err(error) => format!("Reading import refused · {error}"),
                });
                if committed {
                    if let Some(view) = self.reading_query_view(source, cx) {
                        self.refresh_reading_query_inputs(&view, cx);
                    }
                }
            }
            ReadingQueryViewEffect::Observation {
                request,
                cancellation,
            } => {
                let request_id = request.request_id.clone();
                match self.capture_reading_query_session(cx) {
                    Ok(bridge) => {
                        let execution =
                            cx.background_spawn(
                                async move { bridge.dispatch(request, &cancellation) },
                            );
                        cx.spawn(async move |this, cx| {
                            let result = execution.await;
                            let _ = this.update(cx, |this, cx| {
                                let Some(view) = this.reading_query_view(source, cx) else {
                                    return;
                                };
                                match result {
                                    Ok(dispatch) => view
                                        .update(cx, |view, cx| view.accept_dispatch(dispatch, cx)),
                                    Err(error) => {
                                        this.constructive_status =
                                            Some(format!("Reading query refused · {error}"));
                                        view.update(cx, |view, cx| {
                                            view.complete_external_failure(
                                                &request_id,
                                                error.to_string(),
                                                cx,
                                            );
                                        });
                                    }
                                }
                            });
                        })
                        .detach();
                    }
                    Err(error) => {
                        cancellation.cancel();
                        self.constructive_status =
                            Some(format!("Reading query unavailable · {error}"));
                        if let Some(view) = self.reading_query_view(source, cx) {
                            view.update(cx, |view, cx| {
                                view.complete_external_failure(&request_id, error, cx);
                            });
                        }
                    }
                }
            }
            ReadingQueryViewEffect::DocumentChanged(changed) => {
                self.reading_query_documents
                    .insert(source, changed.document);
                if let Some(view) = self.reading_query_view(source, cx) {
                    self.refresh_reading_query_inputs(&view, cx);
                }
                self.constructive_status = Some(match changed.reason {
                    crate::reading_query_view::QueryDocumentChangeReason::ResidualGuideInstalled => {
                        "Reading document updated · residual guide retained in workspace".into()
                    }
                    crate::reading_query_view::QueryDocumentChangeReason::QueryPageObserved => {
                        "Reading document updated · query result and provenance retained in workspace".into()
                    }
                });
            }
            ReadingQueryViewEffect::Render(target) => {
                self.request_reading_audition(source, target, cx);
            }
            ReadingQueryViewEffect::Reveal(target) => {
                self.apply_reading_reveal(source, target, cx);
            }
        }
        cx.notify();
    }

    pub(super) fn request_reading_audition(
        &mut self,
        source: WorkspaceViewId,
        target: crate::air_query::workbench::AuditionTarget,
        cx: &mut Context<Self>,
    ) {
        let generation = self
            .reading_audition_generations
            .entry(source)
            .and_modify(|generation| *generation = generation.wrapping_add(1).max(1))
            .or_insert(1)
            .to_owned();
        let owner = reading_audition_owner(source);
        let _ = self.audio_controller.stop_scoped_audition(owner);
        let snapshot = match ReadingEffectSnapshot::capture(self.session.read(cx)) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.constructive_status = Some(format!("Reading audition refused · {error}"));
                return;
            }
        };
        match snapshot.plan_audition(&target, generation) {
            Ok(ReadingAuditionPlan::Source(plan)) => {
                self.request_reading_source_audition(source, snapshot, plan, cx)
            }
            Ok(ReadingAuditionPlan::Comparison(plan)) => {
                self.request_reading_comparison_audition(source, snapshot, plan, cx)
            }
            Err(error) => {
                self.constructive_status = Some(format!("Reading audition refused · {error}"));
            }
        }
    }

    pub(super) fn request_reading_source_audition(
        &mut self,
        source: WorkspaceViewId,
        snapshot: ReadingEffectSnapshot,
        plan: ReadingSourceAuditionPlan,
        cx: &mut Context<Self>,
    ) {
        let owner = reading_audition_owner(source);
        let Some(format) = self
            .audio_controller
            .renderer_control()
            .map(|control| control.format())
        else {
            self.constructive_status =
                Some("Reading audition refused · the shared project renderer is not ready".into());
            return;
        };
        let pin = plan.pin;
        let generation = plan.generation;
        let span = plan.citation.project_span;
        self.constructive_status = Some(format!(
            "Rendering reading source {}..{} on the shared transport",
            span.start, span.end
        ));
        let execution =
            cx.background_spawn(async move { snapshot.render_source(&plan, owner, format) });
        cx.spawn(async move |this, cx| {
            let result = execution.await;
            let _ = this.update(cx, |this, cx| {
                if this.reading_audition_generations.get(&source).copied() != Some(generation) {
                    return;
                }
                let current = ReadingEffectSnapshot::capture(this.session.read(cx));
                if current.as_ref().map(ReadingEffectSnapshot::pin) != Ok(pin) {
                    this.constructive_status = Some(
                        "Reading audition discarded · project publication changed while rendering"
                            .into(),
                    );
                    return;
                }
                let applied = result
                    .map_err(|error| error.to_string())
                    .and_then(|audition| {
                        let host = this.audio.as_ref().ok_or_else(|| {
                            "the shared project audio host is unavailable".to_owned()
                        })?;
                        this.audio_controller
                            .start_scoped_audition(
                                host,
                                audition,
                                AuditionAlignment::SeekToStart { play: true },
                            )
                            .map_err(|error| error.to_string())
                    });
                this.constructive_status = Some(match applied {
                    Ok(()) => format!(
                        "Reading source {}..{} is aligned to the project transport",
                        span.start, span.end
                    ),
                    Err(error) => format!("Reading audition refused · {error}"),
                });
                this.publish_audio_status(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn request_reading_comparison_audition(
        &mut self,
        source: WorkspaceViewId,
        snapshot: ReadingEffectSnapshot,
        plan: ReadingComparisonAuditionPlan,
        cx: &mut Context<Self>,
    ) {
        let controller = match self.reading_comparison_controllers.entry(source) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                match ComparisonController::new(source.0) {
                    Ok(controller) => entry.insert(controller),
                    Err(error) => {
                        self.constructive_status =
                            Some(format!("Reading audition refused · {error}"));
                        return;
                    }
                }
            }
        };
        let owner = controller.owner();
        self.comparison_executor.cancel_owner(owner);
        let _ = self.audio_controller.stop_scoped_audition(owner);
        let Some(definition) = snapshot.comparison_definition(plan.comparison) else {
            self.constructive_status = Some(format!(
                "Reading audition refused · comparison {} is not retained",
                plan.comparison.0
            ));
            return;
        };
        let Some(observation) = snapshot.comparison_observation(plan.comparison) else {
            self.constructive_status = Some(format!(
                "Reading audition refused · comparison {} has no recorded observation",
                plan.comparison.0
            ));
            return;
        };
        let request = match controller.select(
            definition,
            observation,
            snapshot.pin().revisions,
            plan.channel,
        ) {
            Ok(request) => request,
            Err(error) => {
                self.constructive_status = Some(format!("Reading audition refused · {error}"));
                return;
            }
        };
        let _ = controller;
        let semantics = match self.comparison_semantics_for(&request, cx) {
            Ok(semantics) => semantics,
            Err(error) => {
                if let Some(controller) = self.reading_comparison_controllers.get_mut(&source) {
                    let _ = controller.fail_request(&request, error.clone());
                }
                self.constructive_status = Some(format!("Reading audition refused · {error}"));
                return;
            }
        };
        let job = self.comparison_executor.capture(
            owner,
            request.clone(),
            self.session.read(cx),
            &self.audio_controller,
            semantics,
            ComparisonProductRecipe::default(),
        );
        match job {
            Ok(job) => {
                let focus = plan.focus;
                self.constructive_status = Some(format!(
                    "Rendering reading comparison {} {:?}",
                    plan.comparison.0, plan.channel
                ));
                let execution = cx.background_spawn(async move { job.execute() });
                cx.spawn(async move |this, cx| {
                    let result = execution.await;
                    let _ = this.update(cx, |this, cx| {
                        this.complete_reading_comparison_product(
                            source, owner, request, focus, result, cx,
                        )
                    });
                })
                .detach();
            }
            Err(error) => {
                if let Some(controller) = self.reading_comparison_controllers.get_mut(&source) {
                    let _ = controller.fail_request(&request, error.to_string());
                }
                self.constructive_status = Some(format!("Reading audition refused · {error}"));
            }
        }
    }

    pub(super) fn complete_reading_comparison_product(
        &mut self,
        source: WorkspaceViewId,
        owner: AuditionOwner,
        request: ComparisonSelectionRequest,
        focus: RenderSpan,
        result: Result<ComparisonProductCompletion, ComparisonProductExecutorError>,
        cx: &mut Context<Self>,
    ) {
        let Some(controller) = self.reading_comparison_controllers.get_mut(&source) else {
            self.comparison_executor.cancel_owner(owner);
            return;
        };
        match result {
            Ok(completion) => match self.comparison_executor.publish(
                self.session.read(cx),
                controller,
                completion,
            ) {
                Ok(published) => {
                    let applied = self
                        .audio
                        .as_ref()
                        .ok_or_else(|| "the shared project audio host is unavailable".to_owned())
                        .and_then(|host| {
                            controller
                                .apply_audio_effect(
                                    &mut self.audio_controller,
                                    host,
                                    published.effect,
                                    AuditionAlignment::PreserveTransport,
                                )
                                .map_err(|error| error.to_string())?;
                            let timeline_start = self
                                .audio_controller
                                .renderer_control()
                                .ok_or_else(|| {
                                    "the shared renderer control is unavailable".to_owned()
                                })?
                                .timeline()
                                .start;
                            let locate =
                                u64::try_from(focus.start - timeline_start).map_err(|_| {
                                    "reading focus precedes the active project timeline".to_owned()
                                })?;
                            self.audio_controller
                                .apply_transport_command(
                                    host,
                                    ProjectTransportCommand::Seek(ProjectFrame(locate)),
                                )
                                .and_then(|_| {
                                    self.audio_controller.apply_transport_command(
                                        host,
                                        ProjectTransportCommand::Play,
                                    )
                                })
                                .map_err(|error| error.to_string())?;
                            Ok(())
                        });
                    self.constructive_status = Some(match applied {
                        Ok(()) => format!(
                            "Reading comparison {} {:?} is aligned at {}..{}",
                            request.comparison.0, request.channel, focus.start, focus.end
                        ),
                        Err(error) => {
                            let _ = controller.fail_request(&request, error.clone());
                            format!("Reading audition refused · {error}")
                        }
                    });
                }
                Err(error) => {
                    self.constructive_status =
                        Some(format!("Reading audition discarded · {error}"));
                }
            },
            Err(error) => {
                let _ = controller.fail_request(&request, error.to_string());
                self.constructive_status = Some(format!("Reading audition failed · {error}"));
            }
        }
        self.publish_audio_status(cx);
        cx.notify();
    }

    pub(super) fn apply_reading_reveal(
        &mut self,
        source: WorkspaceViewId,
        target: crate::air_query::workbench::RevealTarget,
        cx: &mut Context<Self>,
    ) {
        let result = ReadingEffectSnapshot::capture(self.session.read(cx)).and_then(|snapshot| {
            let plan = snapshot.plan_reveal(&target)?;
            let guard = self.session.read(cx).current_selection_guard()?;
            let selection = snapshot.reveal_selection(&plan, guard, source)?;
            self.session
                .update(cx, |session, _| {
                    session.replace_guarded_selection(selection)
                })
                .map_err(crate::reading_effect_bridge::ReadingEffectBridgeError::from)?;
            Ok(plan.subject)
        });
        self.constructive_status = Some(match result {
            Ok(subject) => format!("Reading result revealed · {subject:?}"),
            Err(error) => format!("Reading reveal refused · {error}"),
        });
        self.handle_session_events(cx);
        cx.notify();
    }

    pub(super) fn on_explanation_workbench_event(
        &mut self,
        source: WorkspaceViewId,
        event: ExplanationWorkbenchEvent,
        cx: &mut Context<Self>,
    ) {
        // A released comparison controller abandons this event, not the
        // repaint the old batch drain always ran afterwards.
        'event: {
            match event {
                ExplanationWorkbenchEvent::Command(WorkbenchCommand::Plan { action, request }) => {
                    let cancellation = RenderCancellation::new();
                    self.explanation_cancellations
                        .insert((source, action), cancellation.clone());
                    let result = {
                        let session = self.session.read(cx);
                        plan_artifact_promotion_comparison(
                            &session,
                            session.deprojection_workspace_artifacts(),
                            request,
                            &cancellation,
                        )
                    };
                    self.explanation_cancellations.remove(&(source, action));
                    if let Some(view) = self.explanation_workbench_factory.entity(source) {
                        view.update(cx, |view, cx| {
                            match result {
                                Ok(plan) => {
                                    let _ = view.model_mut().accept_plan(action, Arc::new(plan));
                                }
                                Err(error) => {
                                    let _ = view.model_mut().reject(action, error);
                                }
                            }
                            view.notify_model_changed(cx);
                        });
                    }
                }
                ExplanationWorkbenchEvent::Command(WorkbenchCommand::Execute { action, plan }) => {
                    let cancellation = RenderCancellation::new();
                    self.explanation_cancellations
                        .insert((source, action), cancellation.clone());
                    let session = self.session.clone();
                    let result = session.update(cx, |session, _| {
                        (*plan).clone().execute(session, &cancellation)
                    });
                    self.explanation_cancellations.remove(&(source, action));
                    if let Some(view) = self.explanation_workbench_factory.entity(source) {
                        view.update(cx, |view, cx| {
                            match result {
                                Ok(result) => {
                                    let _ =
                                        view.model_mut().accept_promotion(action, Arc::new(result));
                                }
                                Err(error) => {
                                    let _ = view.model_mut().reject(action, error);
                                }
                            }
                            view.notify_model_changed(cx);
                        });
                    }
                }
                ExplanationWorkbenchEvent::Command(WorkbenchCommand::Render { action, result }) => {
                    let cancellation = RenderCancellation::new();
                    self.explanation_cancellations
                        .insert((source, action), cancellation);
                    self.explanation_render_waits
                        .insert(source, (action, Arc::clone(&result)));
                    self.request_project_audio(result.promotion.project.publication.clone(), cx);
                }
                ExplanationWorkbenchEvent::Command(WorkbenchCommand::Capture {
                    action,
                    result,
                    channel,
                }) => {
                    let cancellation = RenderCancellation::new();
                    self.explanation_cancellations
                        .insert((source, action), cancellation.clone());
                    let Some(shared_controller) =
                        self.explanation_workbench_factory.controller(source)
                    else {
                        self.reject_explanation_workbench(
                            source,
                            action,
                            ArtifactPromotionBridgeError::InvalidTarget(
                                "explanation comparison controller was released".into(),
                            ),
                            cx,
                        );
                        break 'event;
                    };
                    let capture = {
                        let session = self.session.read(cx);
                        let mut controller = shared_controller
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        result.capture_updated_comparison(
                            &session,
                            &self.audio_controller,
                            &mut controller,
                            &mut self.comparison_executor,
                            channel,
                            &cancellation,
                        )
                    };
                    match capture {
                        Ok(capture) => {
                            let owner = capture.owner;
                            let request = capture.request.clone();
                            let job = capture.job;
                            let work = cx.background_spawn(async move { job.execute() });
                            cx.spawn(async move |this, cx| {
                                let completion = work.await;
                                let _ = this.update(cx, |this, cx| {
                                    this.complete_explanation_comparison(
                                        source, action, owner, request, completion, cx,
                                    );
                                });
                            })
                            .detach();
                        }
                        Err(error) => {
                            self.explanation_cancellations.remove(&(source, action));
                            self.reject_explanation_workbench(source, action, error, cx);
                        }
                    }
                }
                ExplanationWorkbenchEvent::Command(WorkbenchCommand::Undo { action, result }) => {
                    let session = self.session.clone();
                    let undone = session.update(cx, |session, _| result.undo(session));
                    if let Some(view) = self.explanation_workbench_factory.entity(source) {
                        view.update(cx, |view, cx| {
                            match undone {
                                Ok(_) => {
                                    let _ = view.model_mut().accept_undo(action);
                                }
                                Err(error) => {
                                    let _ = view.model_mut().reject(action, error);
                                }
                            }
                            view.notify_model_changed(cx);
                        });
                    }
                }
                ExplanationWorkbenchEvent::Command(WorkbenchCommand::Cancel {
                    action,
                    operation,
                }) => {
                    if let Some(cancellation) =
                        self.explanation_cancellations.remove(&(source, action))
                    {
                        cancellation.cancel();
                    }
                    if matches!(operation, WorkbenchOperation::Render) {
                        self.explanation_render_waits.remove(&source);
                    }
                    if let Some(controller) = self.explanation_workbench_factory.controller(source)
                    {
                        let owner = controller
                            .lock()
                            .map(|controller| controller.owner())
                            .unwrap_or_else(|poisoned| poisoned.into_inner().owner());
                        self.comparison_executor.cancel_owner(owner);
                    }
                    if let Some(view) = self.explanation_workbench_factory.entity(source) {
                        view.update(cx, |view, cx| {
                            let _ = view.model_mut().accept_cancelled(action);
                            view.notify_model_changed(cx);
                        });
                    }
                }
                ExplanationWorkbenchEvent::Reveal(target) => {
                    self.reveal_from_explanation_workbench(source, target, cx);
                }
            }
        }
        cx.notify();
    }

    pub(super) fn reject_explanation_workbench(
        &mut self,
        source: WorkspaceViewId,
        action: WorkbenchActionId,
        error: ArtifactPromotionBridgeError,
        cx: &mut Context<Self>,
    ) {
        self.constructive_status = Some(error.to_string());
        if let Some(view) = self.explanation_workbench_factory.entity(source) {
            view.update(cx, |view, cx| {
                let _ = view.model_mut().reject(action, error);
                view.notify_model_changed(cx);
            });
        }
    }

    /// Lower one reverse identity and reveal it.
    ///
    /// The lowering, and the reason a lowering is impossible, belong to
    /// `reverse_navigation`; this used to be an inline copy that refused
    /// artifacts and evidence without saying why.
    pub(super) fn reveal_from_explanation_workbench(
        &mut self,
        source: WorkspaceViewId,
        target: ReverseTargetDescriptor,
        cx: &mut Context<Self>,
    ) {
        let result = match resolve_reverse_target(target, RevealIntent::ActivateExisting) {
            ReverseRevealResolution::Ready(reveal) => Ok(reveal.request),
            ReverseRevealResolution::Unsupported(unsupported) => {
                Err(unsupported.refusal().to_string())
            }
        };
        let receipt = result.and_then(|request| {
            let request = request
                .with_current_view(source)
                .from_origin(crate::project_controller::RevealOrigin::Pane(source));
            self.session
                .read(cx)
                .issue_reveal(request)
                .map_err(|error| error.to_string())
        });
        match receipt {
            Ok(receipt) => {
                self.object_reveals.push(PendingObjectReveal {
                    receipt,
                    diagnostics: Vec::new(),
                    headline: "Promoted object selected".into(),
                });
            }
            Err(error) => {
                self.constructive_status = Some(error.clone());
                if let Some(view) = self.explanation_workbench_factory.entity(source) {
                    view.update(cx, |view, cx| {
                        view.report_host_diagnostic(error, cx);
                    });
                }
            }
        }
    }

    pub(super) fn refresh_reverse_promotion_waits(&mut self, cx: &mut Context<Self>) {
        let ready_revision = match self.audio_controller.status().render {
            crate::project_session::RenderActivity::Ready { revision } => revision,
            crate::project_session::RenderActivity::Failed { .. } => {
                if !self.reverse_promotion_waits.is_empty() {
                    self.reverse_promotion_waits.clear();
                    self.constructive_status = Some(
                        "Construction committed, but its comparison render failed; the editable project objects remain available"
                            .into(),
                    );
                }
                return;
            }
            _ => return,
        };
        let stale = self
            .reverse_promotion_waits
            .iter()
            .filter_map(|(&view, result)| {
                (ready_revision > result.promoted_revisions().aggregate).then_some(view)
            })
            .collect::<Vec<_>>();
        for view in stale {
            self.reverse_promotion_waits.remove(&view);
            self.constructive_status = Some(
                "A later project edit superseded the pending reverse comparison; the promoted objects remain editable"
                    .into(),
            );
        }
        let ready = self
            .reverse_promotion_waits
            .iter()
            .filter_map(|(&view, result)| {
                (ready_revision == result.promoted_revisions().aggregate).then_some(view)
            })
            .collect::<Vec<_>>();
        for view in ready {
            let Some(result) = self.reverse_promotion_waits.remove(&view) else {
                continue;
            };
            let Some(shared_controller) = self.reverse_surface_factory.controller(view) else {
                self.constructive_status = Some(
                    "Construction committed; comparison was skipped because its pane closed".into(),
                );
                continue;
            };
            let cancellation = RenderCancellation::new();
            let capture = {
                let session = self.session.read(cx);
                let mut controller = shared_controller
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                result.capture_updated_comparison(
                    &session,
                    &self.audio_controller,
                    &mut controller,
                    &mut self.comparison_executor,
                    ComparisonChannel::Construction,
                    &cancellation,
                )
            };
            let capture = match capture {
                Ok(capture) => capture,
                Err(error) => {
                    self.constructive_status = Some(format!(
                        "Construction committed, but its aligned comparison could not start · {error}"
                    ));
                    continue;
                }
            };
            let published = {
                let session = self.session.clone();
                session.update(cx, |session, _| {
                    result.publish_updated_interpretation(session, &capture)
                })
            };
            if let Err(error) = published {
                self.constructive_status = Some(format!(
                    "Construction committed, but its comparison receipt could not be retained · {error}"
                ));
                continue;
            }
            if let Err(error) = self.refresh_reverse_surface_documents(cx) {
                self.constructive_status = Some(format!(
                    "Comparison was measured, but reverse surfaces could not refresh · {error}"
                ));
            }
            let owner = capture.owner;
            let request = capture.request.clone();
            let job = capture.job;
            let work = cx.background_spawn(async move { job.execute() });
            cx.spawn(async move |this, cx| {
                let completion = work.await;
                let _ = this.update(cx, |this, cx| {
                    this.complete_comparison_product(view, owner, request, completion, cx)
                });
            })
            .detach();
            self.constructive_status = Some(
                "Editable construction rendered · measuring and auditioning the aligned comparison"
                    .into(),
            );
        }
    }

    pub(super) fn refresh_explanation_render_waits(&mut self, cx: &mut Context<Self>) {
        let ready_revision = match self.audio_controller.status().render {
            crate::project_session::RenderActivity::Ready { revision } => Some(revision),
            _ => None,
        };
        let completed = self
            .explanation_render_waits
            .iter()
            .filter_map(|(&view, (action, result))| {
                (ready_revision == Some(result.promoted_revisions().aggregate)).then_some((
                    view,
                    *action,
                    result.promoted_revisions(),
                    result.promoted_publication_generation(),
                ))
            })
            .collect::<Vec<_>>();
        for (source, action, revisions, publication_generation) in completed {
            self.explanation_render_waits.remove(&source);
            self.explanation_cancellations.remove(&(source, action));
            if let Some(view) = self.explanation_workbench_factory.entity(source) {
                view.update(cx, |view, cx| {
                    let _ =
                        view.model_mut()
                            .accept_render(action, revisions, publication_generation);
                    view.notify_model_changed(cx);
                });
            }
        }
    }

    pub(super) fn complete_explanation_comparison(
        &mut self,
        source: WorkspaceViewId,
        action: WorkbenchActionId,
        owner: AuditionOwner,
        request: ComparisonSelectionRequest,
        completion: Result<ComparisonProductCompletion, ComparisonProductExecutorError>,
        cx: &mut Context<Self>,
    ) {
        self.explanation_cancellations.remove(&(source, action));
        let Some(shared_controller) = self.explanation_workbench_factory.controller(source) else {
            self.comparison_executor.cancel_owner(owner);
            return;
        };
        let mut controller = shared_controller
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let accepted = match completion {
            Ok(completion) => {
                let model_completion = Arc::new(completion.clone());
                match self.comparison_executor.publish(
                    self.session.read(cx),
                    &mut controller,
                    completion,
                ) {
                    Ok(published) => {
                        let applied = self.audio.as_ref().ok_or_else(|| {
                            "comparison product is ready, but the project audio host is unavailable"
                                .to_owned()
                        }).and_then(|host| {
                            controller
                                .apply_audio_effect(
                                    &mut self.audio_controller,
                                    host,
                                    published.effect,
                                    AuditionAlignment::SeekToStart { play: true },
                                )
                                .map_err(|error| error.to_string())
                        });
                        match applied {
                            Ok(()) => Ok(model_completion),
                            Err(error) => {
                                let _ = controller.fail_request(&request, error.clone());
                                Err(ArtifactPromotionBridgeError::InvalidTarget(error))
                            }
                        }
                    }
                    Err(error) => Err(error.into()),
                }
            }
            Err(error) => Err(error.into()),
        };
        drop(controller);
        match accepted {
            Ok(completion) => {
                if let Some(view) = self.explanation_workbench_factory.entity(source) {
                    view.update(cx, |view, cx| {
                        let _ = view.model_mut().accept_comparison(action, completion);
                        view.notify_model_changed(cx);
                    });
                }
            }
            Err(error) => self.reject_explanation_workbench(source, action, error, cx),
        }
        self.publish_audio_status(cx);
    }

    pub(super) fn take_reading_query_documents(
        &mut self,
    ) -> BTreeMap<WorkspaceViewId, QueryDocument> {
        std::mem::take(&mut self.reading_query_documents)
    }

    pub(super) fn restore_reading_query_documents(
        &mut self,
        documents: BTreeMap<WorkspaceViewId, QueryDocument>,
    ) {
        // A newer pane publication wins if one arrived while persistence was
        // attempted. Otherwise retain the failed update for the next drain.
        for (view, document) in documents {
            self.reading_query_documents.entry(view).or_insert(document);
        }
    }

    pub(super) fn capture_reading_query_session(
        &self,
        cx: &App,
    ) -> Result<ProjectReadingQuerySession, String> {
        let session = self.session.read(cx);
        ProjectReadingQuerySession::new(
            session,
            session.deprojection_workspace_artifacts(),
            session.deprojection_workspace_interpretations(),
            ProjectQueryResolverInputs::default(),
            Arc::new(|_| {}),
        )
        .map_err(|error| error.to_string())
    }

    pub(super) fn reading_query_view(
        &self,
        source: WorkspaceViewId,
        cx: &App,
    ) -> Option<Entity<ReadingQueryView>> {
        let WorkspacePaneRuntime::Hosted(host) = self.workspace_panes.get(&source)?.clone() else {
            return None;
        };
        let host = host.upgrade()?;
        let WorkspacePaneContent::ReadingQuery(view) = &host.read(cx).content else {
            return None;
        };
        Some(view.clone())
    }

    pub(super) fn refresh_reading_query_inputs(
        &self,
        view: &Entity<ReadingQueryView>,
        cx: &mut Context<Self>,
    ) {
        let Ok(bridge) = self.capture_reading_query_session(cx) else {
            return;
        };
        let inputs = ReadingQueryViewInputs {
            query_provenance: Some(bridge.snapshot().provenance()),
            existing_entities: bridge
                .snapshot()
                .existing_foreign_entities()
                .into_iter()
                .collect(),
            base_revision: self
                .session
                .read(cx)
                .project_snapshot()
                .ok()
                .map(|snapshot| snapshot.revisions().aggregate),
            ..ReadingQueryViewInputs::default()
        };
        view.update(cx, |view, cx| view.observe_inputs(inputs, cx));
    }
}
