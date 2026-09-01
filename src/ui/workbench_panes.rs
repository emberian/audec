//! Workspace pane registry, binding, and per-pane project/audio/selection delivery.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

impl Workbench {
    pub(super) fn handle_session_events(&mut self, cx: &mut Context<Self>) {
        let batch = self.session.read(cx).poll_events(&mut self.session_events);
        let deliveries = {
            let session = self.session.clone();
            self.pane_session_binding
                .consume_batch(session.read(cx), batch.clone())
        };
        if batch.missed_events {
            if let Ok(snapshot) = self.session.read(cx).project_snapshot() {
                let publication = ProjectPublication {
                    generation: self.session.read(cx).snapshot().generation,
                    revisions: snapshot.revisions(),
                    snapshot: snapshot.clone(),
                    change_set: None,
                };
                self.accept_project_publication(publication, cx);
            }
        } else {
            for event in batch.events {
                if let ProjectSessionEvent::ProjectPublished(publication) = event {
                    self.accept_project_publication(publication, cx);
                }
            }
        }
        match deliveries {
            Ok(deliveries) => {
                for delivery in deliveries {
                    self.apply_pane_session_delivery(delivery, cx);
                }
            }
            Err(error) => {
                self.constructive_status =
                    Some(format!("Workspace session fanout failed · {error}"));
            }
        }
    }

    pub(super) fn register_workspace_runtime(
        &mut self,
        descriptor: &WorkspaceViewDescriptor,
        runtime: WorkspacePaneRuntime,
        cx: &mut Context<Self>,
    ) -> Result<(), SharedString> {
        self.install_unbound_workspace_runtime(descriptor.id, runtime, cx);
        self.attach_workspace_pane(descriptor, cx)
    }

    pub(super) fn install_unbound_workspace_runtime(
        &mut self,
        view: WorkspaceViewId,
        runtime: WorkspacePaneRuntime,
        cx: &mut Context<Self>,
    ) {
        self.unregister_workspace_pane(view, cx);
        self.workspace_panes.insert(view, runtime);
    }

    pub(super) fn set_workspace_completion(
        &mut self,
        view: WorkspaceViewId,
        completion: RevealCompletion,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(WorkspacePaneRuntime::Hosted(host)) = self.workspace_panes.get(&view).cloned()
        else {
            return false;
        };
        let Some(host) = host.upgrade() else {
            return false;
        };
        host.update(cx, |host, cx| host.set_completion(completion, cx));
        true
    }

    pub(super) fn select_workspace_target(
        &mut self,
        view: WorkspaceViewId,
        cx: &mut Context<Self>,
    ) {
        let Some(WorkspacePaneRuntime::Hosted(host)) = self.workspace_panes.get(&view).cloned()
        else {
            return;
        };
        let Some(host) = host.upgrade() else {
            return;
        };
        let target = match &host.read(cx).content {
            WorkspacePaneContent::Sampler(sampler) => {
                let sampler = sampler.read(cx);
                let source = sampler.source().clone();
                let state = sampler.state();
                source.kits.lock().ok().and_then(|library| {
                    let kit = library.kits.get(&source.kit)?;
                    let primary = state.selected_pad.map_or_else(
                        || ObjectRef::Instrument(InstrumentRef::SampleKit(kit.id)),
                        |pad| {
                            ObjectRef::Pad(PadRef {
                                kit: kit.id,
                                pad,
                                zone: state.selected_zone,
                            })
                        },
                    );
                    let mut related = vec![ObjectRef::Instrument(InstrumentRef::SampleKit(kit.id))];
                    if let Some(material) = state
                        .selected_zone
                        .and_then(|zone| kit.zones.get(&zone))
                        .map(|zone| zone.material)
                    {
                        related.push(ObjectRef::Sample(material));
                    }
                    Some((primary, related, SelectionSource::Sampler))
                })
            }
            WorkspacePaneContent::Pattern(editor) => editor.read(cx).target().map(|target| {
                (
                    ObjectRef::Pattern(target.pattern),
                    Vec::new(),
                    SelectionSource::PatternEditor,
                )
            }),
            WorkspacePaneContent::Arrangement(arrangement) => {
                let arrangement = arrangement.read(cx);
                let editor = arrangement.editor();
                editor
                    .selection
                    .clips
                    .iter()
                    .next()
                    .copied()
                    .map(|primary| {
                        (
                            ObjectRef::AudioClip(primary),
                            editor
                                .selection
                                .clips
                                .iter()
                                .copied()
                                .filter(|clip| *clip != primary)
                                .map(ObjectRef::AudioClip)
                                .collect(),
                            SelectionSource::Arrangement,
                        )
                    })
            }
            WorkspacePaneContent::Browser(browser) => {
                browser.read(cx).state().selected.map(|asset| {
                    (
                        ObjectRef::Material(asset),
                        Vec::new(),
                        SelectionSource::AssetBrowser,
                    )
                })
            }
            WorkspacePaneContent::Mixer(mixer) => mixer
                .read(cx)
                .selected_bus()
                .map(|bus| (ObjectRef::Bus(bus), Vec::new(), SelectionSource::Mixer)),
            WorkspacePaneContent::Automation(automation) => {
                automation.read(cx).selected_lane().map(|lane| {
                    (
                        ObjectRef::Automation(lane),
                        Vec::new(),
                        SelectionSource::Automation,
                    )
                })
            }
            _ => None,
        };
        let Some((primary, related, source)) = target else {
            return;
        };
        let guard = match self.session.read(cx).current_selection_guard() {
            Ok(guard) => guard,
            Err(error) => {
                self.constructive_status = Some(format!("Editor selection unavailable · {error}"));
                return;
            }
        };
        let previous = self.session.read(cx).selection().selection.clone();
        let mut selection = ProjectSelection::from_reveal(primary, related, guard, Some(view));
        selection.objects.provenance = SelectionProvenance {
            source,
            source_view: Some(view),
        };
        selection.time = previous.time;
        selection.aspect = previous.aspect;
        selection.signal = previous.signal;
        if let Err(error) = self.session.update(cx, |session, _| {
            session.replace_guarded_selection(selection)
        }) {
            self.constructive_status = Some(format!("Editor selection was stale · {error}"));
        }
    }

    pub(super) fn activate_workspace_target(
        &mut self,
        view: WorkspaceViewId,
        cx: &mut Context<Self>,
    ) {
        self.active_workspace_view = Some(view);
        self.select_workspace_target(view, cx);
        if let Some(WorkspacePaneRuntime::Hosted(host)) = self.workspace_panes.get(&view).cloned() {
            if let Some(host) = host.upgrade() {
                if let WorkspacePaneContent::Sampler(sampler) = &host.read(cx).content {
                    self.sampler_selection_cache
                        .insert(view, sampler.read(cx).state());
                }
            }
        }
    }

    pub(super) fn active_workspace_view(&self) -> Option<WorkspaceViewId> {
        self.active_workspace_view
    }

    pub(super) fn collect_editor_view_states(
        &self,
        cx: &App,
    ) -> Vec<(WorkspaceViewId, EditorViewState)> {
        let mut states = Vec::new();
        for (id, runtime) in &self.workspace_panes {
            let WorkspacePaneRuntime::Hosted(host) = runtime else {
                continue;
            };
            let Some(host) = host.upgrade() else {
                continue;
            };
            let state = match &host.read(cx).content {
                WorkspacePaneContent::Arrangement(view) => Some(view.read(cx).editor_view_state()),
                WorkspacePaneContent::Pattern(view) => Some(view.read(cx).editor_view_state()),
                _ => None,
            };
            if let Some(state) = state {
                states.push((*id, state));
            }
        }
        states
    }

    /// The sampler currently owns pad focus internally. Until its view event
    /// surface carries that focus, observe only the active durable workspace
    /// pane and publish changes into the canonical project selection.
    pub(super) fn sync_active_sampler_selection(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.active_workspace_view else {
            return;
        };
        let Some(WorkspacePaneRuntime::Hosted(host)) = self.workspace_panes.get(&view).cloned()
        else {
            return;
        };
        let Some(host) = host.upgrade() else {
            return;
        };
        let WorkspacePaneContent::Sampler(sampler) = &host.read(cx).content else {
            return;
        };
        let state = sampler.read(cx).state();
        if self.sampler_selection_cache.get(&view).copied() == Some(state) {
            return;
        }
        self.sampler_selection_cache.insert(view, state);
        self.select_workspace_target(view, cx);
    }

    pub(super) fn attach_workspace_pane(
        &mut self,
        descriptor: &WorkspaceViewDescriptor,
        cx: &mut Context<Self>,
    ) -> Result<(), SharedString> {
        let registration = PaneSessionRegistration {
            view: descriptor.id,
            links: descriptor.links,
            topics: PaneSessionTopics::ALL,
        };
        let session = self.session.clone();
        let delivery = session
            .update(cx, |session, _| {
                self.pane_session_binding
                    .register_pane(session, registration)
            })
            .map_err(|error| SharedString::from(error.to_string()))?;
        self.apply_pane_session_delivery(delivery, cx);
        Ok(())
    }

    pub(super) fn apply_workspace_binding_effect(
        &mut self,
        effect: PaneBindingEffect,
        cx: &mut Context<Self>,
    ) -> Result<(), SharedString> {
        let detached = match effect {
            PaneBindingEffect::Detach(pane) => Some(pane.0),
            PaneBindingEffect::Attach(_) => None,
        };
        let session = self.session.clone();
        let delivery = session
            .update(cx, |session, _| {
                effect.apply(&mut self.pane_session_binding, session)
            })
            .map_err(|error| SharedString::from(error.to_string()))?;
        if let Some(delivery) = delivery {
            self.apply_pane_session_delivery(delivery, cx);
        }
        if let Some(view) = detached {
            self.detach_workspace_pane(view, cx);
        }
        Ok(())
    }

    pub(super) fn detach_workspace_pane(&mut self, view: WorkspaceViewId, cx: &mut Context<Self>) {
        if let Some(WorkspacePaneRuntime::Analysis(analysis)) =
            self.workspace_panes.get(&view).cloned()
        {
            if let Some(analysis) = analysis.upgrade() {
                let owner = analysis.read(cx).audition_owner;
                let _ = analysis.update(cx, |analysis, cx| analysis.cancel_background_work(cx));
                let _ = self.audio_controller.stop_scoped_audition(owner);
                if let Some(audio) = self.audio.as_ref() {
                    AnalysisPaneBridge::from_owner(owner)
                        .dispose_preview_effect()
                        .apply(&mut self.preview_controller, audio);
                }
            }
        }
        if workspace_audition_owner(view).ok() == self.pattern_audition_owner {
            if let Some(owner) = self.pattern_audition_owner.take() {
                let session = self.session.clone();
                let _ = session.update(cx, |session, _| {
                    self.pattern_audition
                        .stop(session, &mut self.audio_controller, owner)
                });
            }
        }
        if let Some(controller) = self.reverse_surface_factory.controller(view) {
            let owner = controller
                .lock()
                .map(|controller| controller.owner())
                .unwrap_or_else(|poisoned| poisoned.into_inner().owner());
            self.comparison_executor.cancel_owner(owner);
            let _ = self.audio_controller.stop_scoped_audition(owner);
        }
        self.reverse_promotion_waits.remove(&view);
        if let Some(controller) = self.explanation_workbench_factory.controller(view) {
            let owner = controller
                .lock()
                .map(|controller| controller.owner())
                .unwrap_or_else(|poisoned| poisoned.into_inner().owner());
            self.comparison_executor.cancel_owner(owner);
            let _ = self.audio_controller.stop_scoped_audition(owner);
        }
        if let Some(controller) = self.reading_comparison_controllers.remove(&view) {
            self.comparison_executor.cancel_owner(controller.owner());
            let _ = self
                .audio_controller
                .stop_scoped_audition(controller.owner());
        }
        let _ = self
            .audio_controller
            .stop_scoped_audition(reading_audition_owner(view));
        self.reading_audition_generations.remove(&view);
        self.explanation_cancellations
            .retain(|(owner, _), cancellation| {
                if *owner == view {
                    cancellation.cancel();
                    false
                } else {
                    true
                }
            });
        if let Ok(bridge) = SamplePaneBridge::new(view) {
            if let Some(audio) = self.audio.as_ref() {
                bridge
                    .dispose_effect()
                    .apply(&mut self.preview_controller, audio);
            }
            let _ = self.audio_controller.stop_scoped_audition(bridge.owner());
        }
        self.pad_preview_tickets
            .retain(|(owner, _, _), _| *owner != view);
        let session = self.session.clone();
        session.update(cx, |session, _| {
            self.pane_session_binding.unregister_pane(session, view);
        });
    }

    pub(super) fn unregister_workspace_pane(
        &mut self,
        view: WorkspaceViewId,
        cx: &mut Context<Self>,
    ) {
        self.detach_workspace_pane(view, cx);
        self.workspace_panes.remove(&view);
        self.sampler_selection_cache.remove(&view);
        if self.active_workspace_view == Some(view) {
            self.active_workspace_view = None;
        }
        let _ = self.reverse_surface_factory.release(view);
        self.reverse_surface_factory.remove_released();
        self.explanation_workbench_factory.release(view);
        self.explanation_workbench_factory.remove_released();
    }

    pub(super) fn reconcile_workspace_pane_visibility(
        &mut self,
        document: &WorkspaceDocument,
        cx: &mut Context<Self>,
    ) {
        self.retain_workspace_panes(document, cx);
        let panes = self
            .workspace_panes
            .iter()
            .map(|(&view, runtime)| (view, runtime.clone()))
            .collect::<Vec<_>>();
        for (view, _) in panes {
            let visible = document.location(view).is_ok_and(|location| {
                !matches!(location, crate::workspace_document::ViewLocation::Hidden)
            });
            let attached = self.pane_session_binding.contains(view);
            if visible && !attached {
                if let Some(descriptor) = document.views.get(&view) {
                    if let Err(error) = self.attach_workspace_pane(descriptor, cx) {
                        self.constructive_status =
                            Some(format!("Workspace pane attach failed · {error}"));
                    }
                }
            } else if !visible && attached {
                self.detach_workspace_pane(view, cx);
            }
        }
    }

    pub(super) fn retain_workspace_panes(
        &mut self,
        document: &WorkspaceDocument,
        cx: &mut Context<Self>,
    ) {
        let stale = self
            .workspace_panes
            .keys()
            .copied()
            .filter(|view| !document.views.contains_key(view))
            .collect::<Vec<_>>();
        for view in stale {
            self.unregister_workspace_pane(view, cx);
        }
    }

    pub(super) fn apply_pane_session_delivery(
        &mut self,
        delivery: PaneSessionDelivery,
        cx: &mut Context<Self>,
    ) {
        let Some(runtime) = self.workspace_panes.get(&delivery.recipient).cloned() else {
            return;
        };
        if matches!(runtime, WorkspacePaneRuntime::Reverse) {
            let _ = self
                .reverse_surface_factory
                .deliver(delivery.recipient, delivery.payload, cx);
            return;
        }
        match delivery.payload {
            PaneSessionPayload::FullState(snapshot) => {
                if let Some(publication) = snapshot.project {
                    self.apply_project_to_workspace_pane(
                        delivery.recipient,
                        &runtime,
                        publication,
                        cx,
                    );
                }
                self.apply_audio_to_workspace_pane(&runtime, snapshot.audio, cx);
                self.apply_selection_to_workspace_pane(
                    &runtime,
                    PaneSemanticSelection {
                        selection: snapshot.selection,
                        signal: snapshot.signal,
                        group: WorkspaceLinkGroupId::UNLINKED,
                        link_revision: snapshot.selection_revision,
                    },
                    cx,
                );
            }
            PaneSessionPayload::ProjectPublished(publication) => {
                self.apply_project_to_workspace_pane(delivery.recipient, &runtime, publication, cx);
            }
            PaneSessionPayload::SemanticSelection(selection) => {
                self.apply_selection_to_workspace_pane(&runtime, selection, cx);
            }
            PaneSessionPayload::AuthoritativeSelection(selection) => {
                self.apply_selection_to_workspace_pane(
                    &runtime,
                    PaneSemanticSelection {
                        selection: selection.selection,
                        signal: selection.signal,
                        group: WorkspaceLinkGroupId::UNLINKED,
                        link_revision: selection.selection_revision,
                    },
                    cx,
                );
            }
            PaneSessionPayload::AudioChanged(audio) => {
                self.apply_audio_to_workspace_pane(&runtime, audio, cx);
            }
        }
    }

    pub(super) fn apply_audio_to_workspace_pane(
        &mut self,
        runtime: &WorkspacePaneRuntime,
        audio: ProjectAudioStatus,
        cx: &mut Context<Self>,
    ) {
        match runtime {
            WorkspacePaneRuntime::Overview => {
                self.observe_timeline_audio(&audio, cx);
            }
            WorkspacePaneRuntime::Analysis(view) => {
                let _ = view.update(cx, |view, cx| view.set_session_audio(audio, cx));
            }
            WorkspacePaneRuntime::Reverse | WorkspacePaneRuntime::ExplanationWorkbench => {}
            WorkspacePaneRuntime::Hosted(host) => {
                let _ = host.update(cx, |host, cx| host.set_audio(audio, cx));
            }
        }
    }

    pub(super) fn apply_selection_to_workspace_pane(
        &mut self,
        runtime: &WorkspacePaneRuntime,
        selection: PaneSemanticSelection,
        cx: &mut Context<Self>,
    ) {
        match runtime {
            WorkspacePaneRuntime::Overview => {
                self.timeline_signal = selection.signal;
                let range = selection.selection.time.and_then(|range| {
                    TimelineRange::between(
                        TimelinePoint(range.start.max(0) as u64),
                        TimelinePoint(range.end.max(0) as u64),
                    )
                });
                let _ = self
                    .timeline_interaction
                    .apply(TimelineInteractionEvent::ReplaceSelection(range));
                self.sync_timeline_presentation();
                cx.notify();
            }
            WorkspacePaneRuntime::Analysis(view) => {
                let _ = view.update(cx, |view, cx| view.set_semantic_selection(selection, cx));
            }
            WorkspacePaneRuntime::Reverse | WorkspacePaneRuntime::ExplanationWorkbench => {}
            WorkspacePaneRuntime::Hosted(host) => {
                let _ = host.update(cx, |host, cx| host.set_semantic_selection(selection, cx));
            }
        }
    }

    pub(super) fn apply_project_to_workspace_pane(
        &mut self,
        view_id: WorkspaceViewId,
        runtime: &WorkspacePaneRuntime,
        publication: ProjectPublication,
        cx: &mut Context<Self>,
    ) {
        match runtime {
            WorkspacePaneRuntime::Overview => {}
            WorkspacePaneRuntime::Analysis(view) => {
                let generation = publication.generation;
                let _ = view.update(cx, |view, cx| view.set_project_generation(generation, cx));
            }
            WorkspacePaneRuntime::Reverse | WorkspacePaneRuntime::ExplanationWorkbench => {}
            WorkspacePaneRuntime::Hosted(host) => {
                self.apply_project_to_host(view_id, host, publication, cx);
            }
        }
    }
}
