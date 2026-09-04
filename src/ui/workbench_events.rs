//! Workbench mailbox drains for asset, arrangement, sample, pattern, and control events.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;
use crate::control_views::control_actions::{ControlReceipt, CreatedControlIdentity};

impl Workbench {
    pub(super) fn handle_asset_events(&mut self, cx: &mut Context<Self>) {
        let events = self
            .asset_events
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default();
        for event in events {
            match event {
                AssetBrowserEvent::Activate(asset) => {
                    self.reveal_library_material(asset, cx);
                }
                // Connected Browser panes send exact material/range audition
                // through SamplePaneBridge. Ignore the legacy event rather
                // than silently playing the unrelated primary source.
                AssetBrowserEvent::Audition(_) => {}
                AssetBrowserEvent::ToggleFavorite(asset) => {
                    let starred = self
                        .session
                        .update(cx, |session, _| session.toggle_asset_favorite(asset));
                    match starred {
                        Ok(_) => {
                            if let Err(error) = self.session.update(cx, |session, _| {
                                session.replace_object_selection(
                                    ObjectSelection {
                                        primary: Some(ObjectRef::Material(asset)),
                                        ..ObjectSelection::default()
                                    },
                                    SelectionProvenance {
                                        source: SelectionSource::AssetBrowser,
                                        source_view: None,
                                    },
                                )
                            }) {
                                self.constructive_status = Some(format!(
                                    "Starred material selection unavailable · {error}"
                                ));
                            } else {
                                self.constructive_status =
                                    Some("Material star saved in the project".into());
                            }
                        }
                        Err(error) => {
                            self.constructive_status =
                                Some(format!("Material starring refused · {error}"));
                        }
                    }
                }
            }
        }
    }

    pub(super) fn reveal_library_material(&mut self, asset: AssetId, cx: &mut Context<Self>) {
        let exists = self
            .session
            .read(cx)
            .project_snapshot()
            .ok()
            .is_some_and(|snapshot| snapshot.project.state().domains.assets.get(asset).is_some());
        if !exists {
            self.constructive_status = Some("Material is no longer in the project".into());
            cx.notify();
            return;
        }
        let recommendation = recommend_asset(asset);
        let receipt = match self.session.read(cx).issue_reveal(recommendation.request) {
            Ok(receipt) => receipt,
            Err(error) => {
                self.constructive_status = Some(format!("Reveal unavailable · {error}"));
                cx.notify();
                return;
            }
        };
        if let Ok(mut reveals) = self.object_reveals.lock() {
            reveals.push(PendingObjectReveal {
                receipt,
                diagnostics: recommendation.diagnostics,
                headline: "Revealed material".into(),
            });
        }
        cx.notify();
    }

    pub(super) fn handle_arrangement_events(&mut self, cx: &mut Context<Self>) {
        let events = self
            .arrangement_events
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default();
        for pending in events {
            let selection_intent = match &pending.event {
                ArrangementViewEvent::Commit(commit) => commit.selection.clone(),
                _ => None,
            };
            let receipt = self.session.update(cx, |session, _| {
                execute_arrangement_event_revealed(session, pending.event)
            });
            match receipt {
                Ok(receipt) => {
                    match receipt.execution {
                        ArrangementExecution::Seek(frame) => {
                            self.seek_to_sample(u64::try_from(frame.get()).unwrap_or(0), cx);
                        }
                        ArrangementExecution::ProjectChanged { .. }
                        | ArrangementExecution::SelectionOnly
                        | ArrangementExecution::HistoryUnchanged(_) => {}
                    }
                    if let Some(consequence) = apply_arrangement_reveal_selection(&receipt) {
                        self.apply_object_reveal_selection(pending.source, &consequence, cx);
                    }
                    if let Some(mut recommendation) = receipt.reveal {
                        recommendation.request.current_view = pending.source;
                        self.enqueue_reveal_recommendation(
                            recommendation,
                            pending.source,
                            arrangement_reveal_headline,
                            cx,
                        );
                    }
                }
                Err(error) => {
                    let reason = error.to_string();
                    self.constructive_status = Some(format!("Arrangement edit failed · {reason}"));
                    self.deliver_arrangement_refusal(pending.source, &reason, cx);
                }
            }
            if let (Some(source), Some(intent)) = (pending.source, selection_intent) {
                self.publish_arrangement_selection(source, intent, cx);
            }
        }
        self.handle_session_events(cx);
    }

    /// Show a refused arrangement edit in the pane that asked for it. The shell
    /// notice row is not where a musician is looking when a drag is refused, so
    /// the refusal reaches the view's own status as well.
    fn deliver_arrangement_refusal(
        &mut self,
        source: Option<WorkspaceViewId>,
        reason: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = source else {
            if let Some(view) = self.arrangement_view.clone() {
                view.update(cx, |view, cx| view.note_request_refused(reason, cx));
            }
            return;
        };
        let Some(WorkspacePaneRuntime::Hosted(host)) = self.workspace_panes.get(&source) else {
            return;
        };
        let Some(host) = host.upgrade() else {
            return;
        };
        // Take the entity before updating it: the host read lease must end
        // before the pane's own update begins.
        let view = match &host.read(cx).content {
            WorkspacePaneContent::Arrangement(view) => Some(view.clone()),
            _ => None,
        };
        if let Some(view) = view {
            view.update(cx, |view, cx| view.note_request_refused(reason, cx));
        }
    }

    pub(super) fn handle_arrangement_timeline_events(&mut self, cx: &mut Context<Self>) {
        let events = self
            .arrangement_timeline_events
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default();
        let had_events = !events.is_empty();
        for pending in events {
            match pending.event {
                ArrangementTimelineEvent::TimeSelectionChanged(range) => {
                    let project_range = range.and_then(|range| {
                        FrameRange::new(
                            ProjectFrame(u64::try_from(range.start.get()).ok()?),
                            ProjectFrame(u64::try_from(range.end.get()).ok()?),
                        )
                        .ok()
                    });
                    self.apply_project_transport_command(
                        ProjectTransportCommand::ReplaceSelection(project_range),
                        cx,
                    );
                    let timeline_range = project_range.and_then(|range| {
                        TimelineRange::new(TimelinePoint(range.start.0), TimelinePoint(range.end.0))
                    });
                    let _ = self
                        .timeline_interaction
                        .apply(TimelineInteractionEvent::ReplaceSelection(timeline_range));
                }
                ArrangementTimelineEvent::LoopChanged(Some(range)) => {
                    let Ok(start) = u64::try_from(range.start.get()) else {
                        continue;
                    };
                    let Ok(end) = u64::try_from(range.end.get()) else {
                        continue;
                    };
                    let Ok(range) = FrameRange::new(ProjectFrame(start), ProjectFrame(end)) else {
                        continue;
                    };
                    self.apply_project_transport_command(
                        ProjectTransportCommand::ReplaceLoop {
                            range,
                            enabled: true,
                            locate_start: false,
                        },
                        cx,
                    );
                    if let Some(range) =
                        TimelineRange::new(TimelinePoint(start), TimelinePoint(end))
                    {
                        let _ =
                            self.timeline_interaction
                                .apply(TimelineInteractionEvent::ReplaceLoop(
                                    TimelineLoopState::active(range),
                                ));
                    }
                }
                ArrangementTimelineEvent::LoopChanged(None) => {
                    self.apply_project_transport_command(
                        ProjectTransportCommand::SetLoopEnabled(false),
                        cx,
                    );
                    let transport = self.audio_controller.transport_session().snapshot();
                    let loop_state = TimelineLoopState {
                        range: transport.transport.loop_region.and_then(|range| {
                            TimelineRange::new(
                                TimelinePoint(range.start.0),
                                TimelinePoint(range.end.0),
                            )
                        }),
                        enabled: false,
                    };
                    let _ = self
                        .timeline_interaction
                        .apply(TimelineInteractionEvent::ReplaceLoop(loop_state));
                }
            }
            if let Some(source) = pending.source {
                self.active_workspace_view = Some(source);
            }
            self.sync_timeline_presentation();
        }
        if had_events {
            self.sync_arrangement_timeline_views(cx);
            cx.notify();
        }
    }

    pub(super) fn handle_sample_actions(&mut self, cx: &mut Context<Self>) {
        let actions = self
            .sample_actions
            .lock()
            .map(|mut actions| std::mem::take(&mut *actions))
            .unwrap_or_default();
        for pending in actions {
            match pending.request.action.execution_class() {
                SampleActionExecutionClass::Immediate => {
                    let request_id = pending.request.id;
                    let action = pending.request.action.clone();
                    let bridge = match self.begin_sample_audition(pending.source, &action) {
                        Ok(bridge) => bridge,
                        Err(error) => {
                            self.complete_sample_request(
                                request_id,
                                Err(error),
                                pending.completion,
                                pending.source,
                                cx,
                            );
                            continue;
                        }
                    };
                    let result = match self.session.update(cx, |session, _| {
                        session.execute_sample_action(action.clone())
                    }) {
                        Ok(outcome) => self
                            .resolve_sample_pane_outcome(bridge.0, &action, outcome, bridge.1, cx),
                        Err(error) => {
                            self.cancel_sample_pane(bridge.0);
                            Err(SampleActionError::new("session", error.to_string())
                                .retryable(true))
                        }
                    };
                    self.complete_sample_request(
                        request_id,
                        result,
                        pending.completion,
                        pending.source,
                        cx,
                    );
                }
                SampleActionExecutionClass::BackgroundPlanning => {
                    self.dispatch_background_sample_request(pending, cx);
                }
            }
        }
        self.handle_session_events(cx);
    }

    pub(super) fn dispatch_background_sample_request(
        &mut self,
        pending: PendingSampleRequest,
        cx: &mut Context<Self>,
    ) {
        let request_id = pending.request.id;
        let action = pending.request.action.clone();
        let work = self
            .session
            .read(cx)
            .capture_sample_action_work(pending.request);
        let work = match work {
            Ok(work) => work,
            Err(error) => {
                self.complete_sample_request(
                    request_id,
                    Err(SampleActionError::new("session", error.to_string()).retryable(true)),
                    pending.completion,
                    pending.source,
                    cx,
                );
                return;
            }
        };
        let target = pending.completion;
        let source = pending.source;
        let bridge = SamplePaneBridge::new(source.unwrap_or(WorkspaceViewId::TRACK_OVERVIEW));
        let preparation = cx.background_spawn(async move { work.prepare() });
        cx.spawn(async move |this, cx| {
            let prepared = preparation.await;
            let _ = this.update(cx, |this, cx| {
                let result = match prepared {
                    Ok(prepared) => match this.session.update(cx, |session, _| {
                        session.commit_prepared_sample_action(prepared)
                    }) {
                        Ok(outcome) => match bridge {
                            Ok(bridge) => {
                                this.resolve_sample_pane_outcome(bridge, &action, outcome, None, cx)
                            }
                            Err(error) => Err(SampleActionError::new("preview", error.to_string())),
                        },
                        Err(error) => {
                            Err(SampleActionError::new("commit", error.to_string()).retryable(true))
                        }
                    },
                    Err(error) => {
                        Err(SampleActionError::new("planning", error.to_string()).retryable(true))
                    }
                };
                this.complete_sample_request(request_id, result, target, source, cx);
                this.handle_session_events(cx);
            });
        })
        .detach();
    }

    pub(super) fn begin_sample_audition(
        &mut self,
        source: Option<WorkspaceViewId>,
        action: &SampleAction,
    ) -> Result<(SamplePaneBridge, Option<SampleAuditionTicket>), SampleActionError> {
        let view = source.unwrap_or(WorkspaceViewId::TRACK_OVERVIEW);
        let bridge = SamplePaneBridge::new(view)
            .map_err(|error| SampleActionError::new("preview", error.to_string()))?;
        let SampleAction::Audition(intent) = action else {
            return Ok((bridge, None));
        };
        let ticket = match *intent {
            SampleAuditionIntent::MaterialOneShot { .. } => bridge
                .begin_audition(&mut self.preview_controller, *intent)
                .map_err(|error| SampleActionError::new("preview", error.to_string()))?,
            SampleAuditionIntent::PadGate {
                kit,
                pad,
                pressed: true,
                ..
            } => {
                let ticket = bridge
                    .begin_audition(&mut self.preview_controller, *intent)
                    .map_err(|error| SampleActionError::new("preview", error.to_string()))?;
                self.pad_preview_tickets.insert((view, kit, pad), ticket);
                ticket
            }
            SampleAuditionIntent::PadGate {
                kit,
                pad,
                pressed: false,
                ..
            } => self
                .pad_preview_tickets
                .remove(&(view, kit, pad))
                .ok_or_else(|| {
                    SampleActionError::new(
                        "preview.stale-release",
                        "This pad release no longer matches an active press",
                    )
                })?,
        };
        Ok((bridge, Some(ticket)))
    }

    pub(super) fn resolve_sample_pane_outcome(
        &mut self,
        bridge: SamplePaneBridge,
        action: &SampleAction,
        outcome: SampleActionOutcome,
        ticket: Option<SampleAuditionTicket>,
        cx: &mut Context<Self>,
    ) -> SampleActionResult {
        let snapshot = self
            .session
            .read(cx)
            .project_snapshot()
            .cloned()
            .map_err(|error| SampleActionError::new("preview.snapshot", error.to_string()))?;
        let outcome = bridge
            .resolve_outcome(&snapshot, action, outcome, ticket)
            .map_err(|error| SampleActionError::new("preview.resolve", error.to_string()))?;
        if let Some(effect) = outcome.preview {
            let Some(audio) = self.audio.as_ref() else {
                return Err(SampleActionError::new(
                    "preview.host",
                    "The project preview bus is not ready",
                ));
            };
            effect.apply(&mut self.preview_controller, audio);
        }
        outcome.result
    }

    pub(super) fn cancel_sample_pane(&mut self, bridge: SamplePaneBridge) {
        if let Some(audio) = self.audio.as_ref() {
            bridge
                .dispose_effect()
                .apply(&mut self.preview_controller, audio);
        }
        let view = WorkspaceViewId(bridge.owner().local);
        self.pad_preview_tickets
            .retain(|(owner, _, _), _| *owner != view);
    }

    pub(super) fn complete_sample_request(
        &mut self,
        request_id: crate::sample_actions::SampleRequestId,
        result: SampleActionResult,
        target: Option<SampleCompletionTarget>,
        source: Option<WorkspaceViewId>,
        cx: &mut Context<Self>,
    ) {
        let publication = result.as_ref().ok().and_then(|outcome| match outcome {
            SampleViewOutcome::Published(publication) => Some(publication.clone()),
            _ => None,
        });
        match target {
            Some(SampleCompletionTarget::Browser(browser)) => {
                browser.update(cx, |browser, cx| {
                    browser.complete_request(request_id, result, cx);
                });
            }
            Some(SampleCompletionTarget::Sampler(sampler)) => {
                sampler.update(cx, |sampler, cx| {
                    sampler.complete_request(request_id, result, cx);
                });
            }
            None => {
                if let Err(error) = result {
                    self.audio_error = Some(error.message);
                }
            }
        }
        if let Some(publication) = publication {
            self.enqueue_sample_reveal(publication, source, cx);
        }
    }

    pub(super) fn enqueue_sample_reveal(
        &mut self,
        publication: SamplePublishedResult,
        source: Option<WorkspaceViewId>,
        cx: &mut Context<Self>,
    ) {
        // SampleFocusCallback is the view-owned signal that this correlated
        // result wants navigation. Pair it here with the full publication so
        // kit/pad/pattern identities and provenance are not lost.
        if publication.focus != SampleResultFocus::Stay {
            match self.sample_focuses.lock() {
                Ok(mut focuses) => {
                    if let Some(index) = focuses.iter().position(|pending| {
                        pending.source == source && pending.focus == publication.focus
                    }) {
                        focuses.remove(index);
                    }
                }
                Err(poisoned) => {
                    let mut focuses = poisoned.into_inner();
                    if let Some(index) = focuses.iter().position(|pending| {
                        pending.source == source && pending.focus == publication.focus
                    }) {
                        focuses.remove(index);
                    }
                }
            }
        }
        let mut recommendation = recommend_sample_result(&publication);
        recommendation.request.current_view = source;
        let headline = match &recommendation.request.object {
            ObjectRef::Pattern(_) | ObjectRef::PatternOccurrence(_) => "Beat created",
            ObjectRef::AutomationOccurrence(_) => "Automation edit created",
            ObjectRef::Instrument(_) | ObjectRef::Pad(_) => "Instrument created",
            _ => "Sample action completed",
        };
        let receipt = match self.session.read(cx).issue_reveal(recommendation.request) {
            Ok(receipt) => receipt,
            Err(error) => {
                self.constructive_status = Some(format!("Reveal unavailable · {error}"));
                cx.notify();
                return;
            }
        };
        self.enqueue_issued_reveal(receipt, recommendation.diagnostics, headline, cx);
    }

    pub(super) fn enqueue_reveal_recommendation(
        &mut self,
        recommendation: RevealRecommendation,
        source: Option<WorkspaceViewId>,
        headline: impl FnOnce(&ObjectRef) -> &'static str,
        cx: &mut Context<Self>,
    ) {
        let mut recommendation = recommendation;
        if recommendation.request.current_view.is_none() {
            recommendation.request.current_view = source;
        }
        let headline = headline(&recommendation.request.object);
        let receipt = match self.session.read(cx).issue_reveal(recommendation.request) {
            Ok(receipt) => receipt,
            Err(error) => {
                self.constructive_status = Some(format!("Reveal unavailable · {error}"));
                cx.notify();
                return;
            }
        };
        self.enqueue_issued_reveal(receipt, recommendation.diagnostics, headline, cx);
    }

    pub(super) fn enqueue_issued_reveal(
        &mut self,
        receipt: RevealReceipt,
        diagnostics: Vec<crate::project_controller::RevealDiagnostic>,
        headline: impl Into<String>,
        _cx: &mut Context<Self>,
    ) {
        if let Ok(mut reveals) = self.object_reveals.lock() {
            reveals.push(PendingObjectReveal {
                receipt,
                diagnostics,
                headline: headline.into(),
            });
        }
    }

    pub(super) fn apply_object_reveal_selection(
        &mut self,
        view: Option<WorkspaceViewId>,
        consequence: &SelectionConsequence,
        cx: &mut Context<Self>,
    ) {
        let project = self
            .session
            .read(cx)
            .project_snapshot()
            .ok()
            .map(|snapshot| snapshot.project.clone());
        if let Some(project) = project.as_ref() {
            let guard = self.session.read(cx).current_selection_guard();
            match guard {
                Ok(guard) => {
                    let mut selection = ProjectSelection::from_reveal(
                        consequence.primary.clone(),
                        consequence.related.iter().cloned(),
                        guard,
                        view,
                    );
                    for object in std::iter::once(&consequence.primary).chain(&consequence.related)
                    {
                        add_product_object_to_selection(&mut selection, object, project);
                    }
                    if let Err(error) = self.session.update(cx, |session, _| {
                        session.replace_guarded_selection(selection)
                    }) {
                        self.constructive_status =
                            Some(format!("Created object selection was stale · {error}"));
                    }
                }
                Err(error) => {
                    self.constructive_status =
                        Some(format!("Created object selection unavailable · {error}"));
                }
            }
        }

        let Some(view) = view else {
            self.handle_session_events(cx);
            return;
        };
        let Some(WorkspacePaneRuntime::Hosted(host)) = self.workspace_panes.get(&view).cloned()
        else {
            self.handle_session_events(cx);
            return;
        };
        let Some(host) = host.upgrade() else {
            self.handle_session_events(cx);
            return;
        };
        let primary = consequence.primary.clone();
        let project = project.clone();
        host.update(cx, |host, cx| match &host.content {
            WorkspacePaneContent::Browser(browser) => {
                if let Some(asset) = object_asset(&primary) {
                    browser.update(cx, |browser, cx| {
                        let mut state = browser.state().clone();
                        state.selected = Some(asset);
                        browser.set_state(state, cx);
                    });
                }
            }
            WorkspacePaneContent::Sampler(sampler) => match primary {
                ObjectRef::Instrument(InstrumentRef::SampleKit(kit)) => {
                    sampler.update(cx, |sampler, cx| {
                        sampler.retarget(SamplerTarget::Kit(kit), cx)
                    });
                }
                ObjectRef::Pad(pad) => {
                    sampler.update(cx, |sampler, cx| {
                        sampler.retarget(
                            SamplerTarget::Pad {
                                kit: pad.kit,
                                pad: pad.pad,
                            },
                            cx,
                        )
                    });
                }
                _ => {}
            },
            WorkspacePaneContent::Arrangement(arrangement) => {
                if let Some(project) = project.as_ref() {
                    let state = &project.state().domains.arrangement;
                    let mut selected = ArrangementSelection::default();
                    match primary {
                        ObjectRef::PatternOccurrence(occurrence) => {
                            selected.clips.insert(occurrence.arrangement_clip);
                        }
                        ObjectRef::AudioClip(clip) => {
                            selected.clips.insert(clip);
                        }
                        ObjectRef::AutomationOccurrence(occurrence) => {
                            selected.clips.insert(occurrence.arrangement_clip);
                        }
                        ObjectRef::Track(track) => {
                            selected.tracks.insert(track);
                        }
                        _ => {}
                    }
                    for clip in &selected.clips {
                        if let Some(clip) = state.clip(*clip) {
                            selected.tracks.insert(clip.track_id);
                            selected.time = Some(clip.placement);
                        }
                    }
                    arrangement.update(cx, |arrangement, cx| {
                        arrangement.set_selection(selected.clone(), cx);
                        if let Some(range) = selected.time {
                            let mut viewport = arrangement.viewport();
                            if viewport.ensure_visible(range.start, 0.18) {
                                arrangement.set_viewport(viewport, cx);
                            }
                        }
                    });
                }
            }
            _ => {}
        });
        self.handle_session_events(cx);
    }

    pub(super) fn sample_focus_callback(
        &self,
        source: Option<WorkspaceViewId>,
    ) -> SampleFocusCallback {
        let focuses = Arc::clone(&self.sample_focuses);
        Arc::new(move |focus| {
            if let Ok(mut focuses) = focuses.lock() {
                focuses.push(PendingSampleFocus { source, focus });
            }
        })
    }

    pub(super) fn install_browser_sample_callbacks(
        &self,
        browser: &Entity<AssetBrowserView>,
        source: Option<WorkspaceViewId>,
        cx: &mut Context<Self>,
    ) {
        let actions = Arc::clone(&self.sample_actions);
        let completion = browser.clone();
        let callback = Arc::new(move |request: SampleActionRequest| {
            let receipt = SampleDispatchReceipt::accepted(&request);
            if let Ok(mut actions) = actions.lock() {
                actions.push(PendingSampleRequest {
                    request,
                    completion: Some(SampleCompletionTarget::Browser(completion.clone())),
                    source,
                });
            }
            receipt
        });
        let focus = self.sample_focus_callback(source);
        browser.update(cx, |browser, _| {
            browser.set_sample_callback(Some(callback));
            browser.set_sample_focus_callback(Some(focus));
        });
    }

    pub(super) fn install_sampler_sample_callbacks(
        &self,
        sampler: &Entity<SamplerView>,
        source: Option<WorkspaceViewId>,
        cx: &mut Context<Self>,
    ) {
        let actions = Arc::clone(&self.sample_actions);
        let completion = sampler.clone();
        let callback = Arc::new(move |request: SampleActionRequest| {
            let receipt = SampleDispatchReceipt::accepted(&request);
            if let Ok(mut actions) = actions.lock() {
                actions.push(PendingSampleRequest {
                    request,
                    completion: Some(SampleCompletionTarget::Sampler(completion.clone())),
                    source,
                });
            }
            receipt
        });
        let focus = self.sample_focus_callback(source);
        sampler.update(cx, |sampler, _| {
            sampler.set_callback(Some(callback));
            sampler.set_focus_callback(Some(focus));
        });
    }

    pub(super) fn install_pattern_workflow_callback(
        &self,
        editor: &Entity<SequencerEditor>,
        revision: u64,
        source: Option<WorkspaceViewId>,
        cx: &mut Context<Self>,
    ) {
        let workflows = Arc::clone(&self.pattern_workflows);
        let completion = editor.clone();
        let callback = Arc::new(move |request: PatternWorkflowRequest| {
            let id = request.id;
            workflows
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(PendingPatternWorkflow {
                    request,
                    completion: completion.clone(),
                });
            PatternWorkflowDispatchReceipt::accepted(id)
        });
        let shared_audition = source.and_then(|view| {
            let owner = workspace_audition_owner(view).ok()?;
            let auditions = Arc::clone(&self.pattern_auditions);
            Some(Arc::new(move |request: PatternAuditionRequest| {
                auditions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(PendingPatternAudition { request, owner });
            })
                as crate::project_controller::SharedPatternAuditionCallback)
        });
        let placement_frame = ArrangementFrame::new(
            i64::try_from(
                self.audio_controller
                    .transport_session()
                    .snapshot()
                    .transport
                    .frame
                    .0,
            )
            .unwrap_or(i64::MAX),
        );
        editor.update(cx, |editor, cx| {
            editor.set_project_revision(revision, cx);
            editor.set_placement_frame(placement_frame, cx);
            editor.set_workflow_callback(Some(callback));
            editor.set_shared_pattern_audition_callback(shared_audition.clone());
            editor.set_audition_availability(
                if shared_audition.is_some() {
                    SequencerAuditionAvailability::Available
                } else {
                    SequencerAuditionAvailability::unavailable(
                        "Pattern audition requires a project workspace pane",
                    )
                },
                cx,
            );
        });
    }

    pub(super) fn handle_pattern_auditions(&mut self, cx: &mut Context<Self>) {
        let requests = std::mem::take(
            &mut *self
                .pattern_auditions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for pending in requests {
            if self.audio.is_none() {
                self.constructive_status =
                    Some("Pattern audition unavailable · project audio is not ready".into());
                continue;
            }
            if let Some(previous) = self.pattern_audition_owner.take() {
                let session = self.session.clone();
                let _ = session.update(cx, |session, _| {
                    self.pattern_audition
                        .stop(session, &mut self.audio_controller, previous)
                });
            }
            let alignment =
                PatternAuditionSessionInputs::adoption_for_scope(&pending.request.scope);
            let start = PatternAuditionStartRequest {
                audition: pending.request,
                adoption: PatternAuditionAdoption {
                    owner: pending.owner,
                    subject: AuditionSubject::Construction,
                    mix: AuditionMix::Replace,
                    alignment,
                },
            };
            let session = self.session.clone();
            let prepared = session.update(cx, |session, _| {
                let inputs = PatternAuditionSessionInputs::from_session(session)?;
                self.pattern_audition.prepare(session, start, inputs)
            });
            match prepared {
                Ok(job) => {
                    self.pattern_audition_owner = Some(pending.owner);
                    self.constructive_status = Some("Rendering exact pattern audition".into());
                    let execution = cx.background_spawn(async move { job.execute() });
                    cx.spawn(async move |this, cx| {
                        let work = execution.await;
                        let _ = this.update(cx, |this, cx| {
                            let session = this.session.clone();
                            let Some(host) = this.audio.as_ref() else {
                                return;
                            };
                            match session.update(cx, |session, _| {
                                this.pattern_audition.complete(
                                    session,
                                    &mut this.audio_controller,
                                    host,
                                    work,
                                )
                            }) {
                                Ok(_) => {
                                    this.constructive_status =
                                        Some("Playing exact pattern audition".into());
                                    this.publish_audio_status(cx);
                                }
                                Err(error) => {
                                    this.constructive_status =
                                        Some(format!("Pattern audition refused · {error}"));
                                }
                            }
                            cx.notify();
                        });
                    })
                    .detach();
                }
                Err(error) => {
                    self.constructive_status = Some(format!("Pattern audition refused · {error}"));
                }
            }
        }
    }

    pub(super) fn handle_pattern_workflows(&mut self, cx: &mut Context<Self>) {
        let workflows = std::mem::take(
            &mut *self
                .pattern_workflows
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for pending in workflows {
            let request = pending.request.id;
            let result = self.session.update(cx, |session, _| {
                session.execute_pattern_workflow(pending.request.intent)
            });
            match result {
                Ok(outcome) => {
                    if let Some(reveal) = pattern_workflow_reveal_request(&outcome) {
                        self.enqueue_reveal_recommendation(
                            RevealRecommendation {
                                request: reveal,
                                diagnostics: Vec::new(),
                            },
                            None,
                            pattern_workflow_reveal_headline,
                            cx,
                        );
                    }
                    pending.completion.update(cx, |editor, cx| {
                        editor.complete_workflow(request, Ok(outcome), cx);
                    });
                }
                Err(error) => {
                    self.constructive_status = Some(format!("Pattern workflow failed · {error}"));
                    pending.completion.update(cx, |editor, cx| {
                        editor.complete_workflow_failure(request, error.to_string(), cx);
                    });
                }
            }
        }
    }

    pub(super) fn handle_control_actions(&mut self, cx: &mut Context<Self>) {
        let actions = self
            .control_actions
            .lock()
            .map(|mut actions| std::mem::take(&mut *actions))
            .unwrap_or_default();
        for pending in actions {
            let editor_session = pending.editor_session;
            let surface = pending.action.surface();
            let receipt = match self.session.update(cx, |session, _| {
                execute_control_action_revealed(session, editor_session, pending.action)
            }) {
                Ok(receipt) => {
                    if let Some(reveal) = receipt.reveal {
                        self.enqueue_reveal_recommendation(
                            reveal,
                            self.active_workspace_view,
                            |object| match object {
                                ObjectRef::Bus(_) => "Mixer bus created",
                                ObjectRef::Automation(_) => "Automation lane created",
                                _ => "Control object created",
                            },
                            cx,
                        );
                    }
                    ControlReceipt::Committed {
                        surface,
                        revision: receipt.revisions.map(|revisions| revisions.aggregate),
                        created: match receipt.primary {
                            Some(ObjectRef::Bus(id)) => Some(CreatedControlIdentity::MixerBus(id)),
                            Some(ObjectRef::Automation(id)) => {
                                Some(CreatedControlIdentity::AutomationLane(id))
                            }
                            _ => None,
                        },
                    }
                }
                Err(error) => {
                    let reason = error.to_string();
                    self.constructive_status = Some(reason.clone());
                    ControlReceipt::Refused { surface, reason }
                }
            };
            self.deliver_control_receipt(editor_session, &receipt, cx);
        }
        self.handle_pattern_workflows(cx);
        self.handle_session_events(cx);
    }

    /// Answer the editor that asked. A control view shows what it requested
    /// until its own receipt arrives, so a receipt must reach exactly the
    /// editor session that emitted the action and no other.
    fn deliver_control_receipt(
        &mut self,
        editor_session: u64,
        receipt: &ControlReceipt,
        cx: &mut Context<Self>,
    ) {
        if editor_session == 0 {
            if let Some(view) = self.mixer_view.clone() {
                view.update(cx, |view, cx| view.apply_control_receipt(receipt, cx));
            }
            if let Some(view) = self.automation_view.clone() {
                view.update(cx, |view, cx| view.apply_control_receipt(receipt, cx));
            }
            return;
        }
        let Some(WorkspacePaneRuntime::Hosted(host)) = self
            .workspace_panes
            .get(&crate::workspace_document::WorkspaceViewId(editor_session))
        else {
            return;
        };
        let Some(host) = host.upgrade() else {
            return;
        };
        // Take the entity before updating it: the host read lease must end
        // before the pane's own update begins.
        let (mixer, automation) = match &host.read(cx).content {
            WorkspacePaneContent::Mixer(view) => (Some(view.clone()), None),
            WorkspacePaneContent::Automation(view) => (None, Some(view.clone())),
            _ => (None, None),
        };
        if let Some(view) = mixer {
            view.update(cx, |view, cx| view.apply_control_receipt(receipt, cx));
        }
        if let Some(view) = automation {
            view.update(cx, |view, cx| view.apply_control_receipt(receipt, cx));
        }
    }
}
