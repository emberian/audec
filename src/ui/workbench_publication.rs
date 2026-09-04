//! Project publication into the host: audio requests, meters, and arrangement sync.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

impl Workbench {
    pub(super) fn apply_project_to_host(
        &mut self,
        _view_id: WorkspaceViewId,
        host: &WeakEntity<WorkspacePaneHost>,
        publication: ProjectPublication,
        cx: &mut Context<Self>,
    ) {
        let Some(host_entity) = host.upgrade() else {
            return;
        };
        let (descriptor, content, previous) = {
            let host = host_entity.read(cx);
            (
                host.descriptor.clone(),
                host.content.clone(),
                host.project_revisions,
            )
        };
        let revisions = publication.revisions;
        let domains = &publication.snapshot.project.state().domains;

        match &content {
            WorkspacePaneContent::Overview(_) => {}
            WorkspacePaneContent::Arrangement(view) => {
                let entities_changed = previous.is_none_or(|previous| {
                    previous.arrangement != revisions.arrangement
                        || previous.sequencer != revisions.sequencer
                });
                let refreshed = entities_changed
                    .then(|| ArrangementEditor::from_state(domains.arrangement.clone()).ok())
                    .flatten()
                    .map(|editor| {
                        (
                            editor,
                            self.arrangement_waveform_provider(&publication.snapshot),
                            domains.sequencer.tempo_map().clone(),
                        )
                    });
                let truth = (revisions.aggregate, publication.snapshot.is_dirty());
                let history = self.session.read(cx).history_status().ok();
                view.update(cx, |view, cx| {
                    match refreshed {
                        Some((editor, waveform, tempo_map)) => {
                            view.set_waveform_provider(waveform);
                            view.set_project_snapshot(editor, truth.0, cx);
                            view.set_tempo_map(tempo_map, cx);
                        }
                        // Every publication moves the aggregate revision, this
                        // domain or not. A view left holding an older token
                        // would have its next edit refused for a conflict it
                        // did not cause.
                        None => view.set_project_revision(truth.0, cx),
                    }
                    view.set_project_truth(truth.0, truth.1, cx);
                    if let Some(history) = history {
                        view.set_project_history(history, cx);
                    }
                });
            }
            WorkspacePaneContent::Pattern(view) => {
                if previous.is_none_or(|previous| previous.sequencer != revisions.sequencer) {
                    let preferred_occurrence = view
                        .read(cx)
                        .source()
                        .workflow
                        .as_ref()
                        .and_then(|workflow| workflow.occurrence);
                    let source = workspace_pattern_source(
                        &descriptor,
                        &publication.snapshot,
                        preferred_occurrence,
                    );
                    view.update(cx, |view, cx| {
                        view.set_source_snapshot(source, revisions.aggregate, cx)
                    });
                }
            }
            WorkspacePaneContent::Mixer(view) => {
                if previous.is_none_or(|previous| previous.mixer != revisions.mixer) {
                    view.update(cx, |view, cx| {
                        view.set_controller_snapshot(domains.mixer.clone(), cx)
                    });
                }
            }
            WorkspacePaneContent::Automation(view) => {
                if previous.is_none_or(|previous| {
                    previous.automation != revisions.automation || previous.mixer != revisions.mixer
                }) {
                    view.update(cx, |view, cx| {
                        view.set_controller_snapshot(domains.automation.clone(), cx);
                        view.set_mixer_snapshot(&domains.mixer, cx);
                    });
                }
            }
            WorkspacePaneContent::Analysis(view) => {
                view.update(cx, |view, cx| {
                    view.set_project_generation(publication.generation, cx)
                });
            }
            WorkspacePaneContent::Browser(view) => {
                if previous.is_none_or(|previous| {
                    previous.assets != revisions.assets
                        || previous.sample_kits != revisions.sample_kits
                }) {
                    let state = view.read(cx).state().clone();
                    let events = Arc::clone(&self.asset_events);
                    let callback = Arc::new(move |event| {
                        if let Ok(mut events) = events.lock() {
                            events.push(event);
                        }
                    });
                    let registry = Arc::new(Mutex::new(domains.assets.clone()));
                    let material_pool =
                        MaterialPoolSnapshot::from_project(&publication.snapshot.project);
                    let replacement = cx.new(|cx| {
                        let mut view = AssetBrowserView::with_callback(
                            Arc::clone(&registry),
                            Some(callback),
                            cx,
                        );
                        view.set_state(state, cx);
                        view.set_material_pool_snapshot(material_pool, cx);
                        view
                    });
                    self.install_browser_sample_callbacks(&replacement, Some(descriptor.id), cx);
                    host_entity.update(cx, |host, cx| {
                        host.content = WorkspacePaneContent::Browser(replacement);
                        cx.notify();
                    });
                }
            }
            WorkspacePaneContent::Sampler(view) => {
                let changed = previous.is_none_or(|previous| {
                    previous.sample_kits != revisions.sample_kits
                        || previous.assets != revisions.assets
                        || previous.mixer != revisions.mixer
                });
                if changed {
                    let state = view.read(cx).state();
                    let target = view.read(cx).target();
                    if let Some(replacement) = self.sampler_view_for_publication(
                        &descriptor,
                        &publication,
                        Some((state, target)),
                        cx,
                    ) {
                        host_entity.update(cx, |host, cx| {
                            host.content = WorkspacePaneContent::Sampler(replacement);
                            cx.notify();
                        });
                    }
                }
            }
            WorkspacePaneContent::ReadingQuery(view) => {
                // Keep historical rows/provenance in the document, while new
                // requests execute against a freshly captured project fact base.
                self.refresh_reading_query_inputs(view, cx);
            }
            WorkspacePaneContent::Notice(_) => {
                let replacement = match descriptor.kind {
                    WorkspaceKind::PatternEditor { .. } => Some(WorkspacePaneContent::Pattern(
                        self.pattern_view_for_publication(&descriptor, &publication, cx),
                    )),
                    WorkspaceKind::Arrangement => Some(WorkspacePaneContent::Arrangement(
                        self.create_arrangement_view(Some(descriptor.id), cx),
                    )),
                    WorkspaceKind::Browser => {
                        let events = Arc::clone(&self.asset_events);
                        let callback = Arc::new(move |event| {
                            if let Ok(mut events) = events.lock() {
                                events.push(event);
                            }
                        });
                        let registry = Arc::new(Mutex::new(domains.assets.clone()));
                        let material_pool =
                            MaterialPoolSnapshot::from_project(&publication.snapshot.project);
                        let view = cx.new(|cx| {
                            let mut view =
                                AssetBrowserView::with_callback(registry, Some(callback), cx);
                            view.set_material_pool_snapshot(material_pool, cx);
                            view
                        });
                        self.install_browser_sample_callbacks(&view, Some(descriptor.id), cx);
                        Some(WorkspacePaneContent::Browser(view))
                    }
                    WorkspaceKind::Mixer => {
                        let actions = Arc::clone(&self.control_actions);
                        let editor_session = descriptor.id.0;
                        let callback = Arc::new(move |action| {
                            if let Ok(mut actions) = actions.lock() {
                                actions.push(PendingControlAction {
                                    editor_session,
                                    action,
                                });
                            }
                        });
                        Some(WorkspacePaneContent::Mixer(cx.new(|cx| {
                            MixerView::from_controller_snapshot(
                                domains.mixer.clone(),
                                None,
                                callback,
                                cx,
                            )
                        })))
                    }
                    WorkspaceKind::AutomationEditor => {
                        let target = domains.automation.lanes().next().map(|lane| lane.id);
                        let actions = Arc::clone(&self.control_actions);
                        let editor_session = descriptor.id.0;
                        let callback = Arc::new(move |action| {
                            if let Ok(mut actions) = actions.lock() {
                                actions.push(PendingControlAction {
                                    editor_session,
                                    action,
                                });
                            }
                        });
                        Some(WorkspacePaneContent::Automation(cx.new(|cx| {
                            AutomationView::from_controller_snapshots_optional(
                                domains.automation.clone(),
                                &domains.mixer,
                                target,
                                callback,
                                cx,
                            )
                        })))
                    }
                    WorkspaceKind::Extension {
                        ref namespace,
                        ref name,
                    } if namespace == "audec" && name == "sampler" => self
                        .sampler_view_for_publication(&descriptor, &publication, None, cx)
                        .map(WorkspacePaneContent::Sampler),
                    _ => None,
                };
                if let Some(replacement) = replacement {
                    host_entity.update(cx, |host, cx| {
                        host.content = replacement;
                        cx.notify();
                    });
                }
            }
        }
        host_entity.update(cx, |host, _| {
            host.project_generation = Some(publication.generation);
            host.project_revisions = Some(revisions);
        });
    }

    pub(super) fn pattern_view_for_publication(
        &self,
        descriptor: &WorkspaceViewDescriptor,
        publication: &ProjectPublication,
        cx: &mut Context<Self>,
    ) -> Entity<SequencerEditor> {
        let source = workspace_pattern_source(descriptor, &publication.snapshot, None);
        let mode = match descriptor.kind {
            WorkspaceKind::PatternEditor {
                mode: WorkspacePatternMode::PianoRoll,
            } => crate::sequencer_view::EditorMode::PianoRoll,
            _ => crate::sequencer_view::EditorMode::Steps,
        };
        let view = cx.new(|cx| {
            let mut view = SequencerEditor::new(source, cx);
            view.set_mode(mode, cx);
            view
        });
        self.install_pattern_workflow_callback(
            &view,
            publication.revisions.aggregate,
            Some(descriptor.id),
            cx,
        );
        view
    }

    pub(super) fn sampler_view_for_publication(
        &self,
        descriptor: &WorkspaceViewDescriptor,
        publication: &ProjectPublication,
        previous: Option<(crate::sampler_view::SamplerViewState, SamplerTarget)>,
        cx: &mut Context<Self>,
    ) -> Option<Entity<SamplerView>> {
        let domains = &publication.snapshot.project.state().domains;
        let fallback = domains.sample_kits.kits.keys().next().copied()?;
        let target = sampler_target_from_descriptor(descriptor)
            .or_else(|| previous.map(|(_, target)| target))
            .filter(|target| {
                target
                    .kit()
                    .is_some_and(|kit| domains.sample_kits.kits.contains_key(&kit))
            })
            .unwrap_or(SamplerTarget::Kit(fallback));
        let kit = target.kit().unwrap_or(fallback);
        let mixer = domains.mixer.clone();
        let buses = Arc::new(move || {
            mixer
                .buses()
                .map(|bus| SamplerBusOption {
                    id: bus.id(),
                    name: bus.name().to_owned(),
                })
                .collect()
        });
        let source = SamplerViewSource::new(
            Arc::new(Mutex::new(domains.sample_kits.clone())),
            Arc::new(Mutex::new(domains.assets.clone())),
            kit,
            buses,
        );
        let state = previous.map(|(state, _)| state);
        let view = cx.new(|cx| {
            let mut view = SamplerView::new(source, cx);
            view.retarget(target, cx);
            if let Some(state) = state {
                view.set_state(state, cx);
            }
            view
        });
        self.install_sampler_sample_callbacks(&view, Some(descriptor.id), cx);
        Some(view)
    }

    pub(super) fn accept_project_publication(
        &mut self,
        publication: ProjectPublication,
        cx: &mut Context<Self>,
    ) {
        let project = publication.snapshot.project.clone();
        if let Err(error) = self.session.update(cx, |session, _| {
            session.reconcile_guarded_selection(|object| {
                project_contains_object(project.as_ref(), object)
            })
        }) {
            self.constructive_status =
                Some(format!("Project selection reconciliation failed · {error}"));
        }
        let domains = &publication.snapshot.project.state().domains;
        self.asset_registry = Arc::new(Mutex::new(domains.assets.clone()));
        self.asset_view = None;

        if let Some(view) = self.arrangement_view.clone() {
            let editor = ArrangementEditor::from_state(domains.arrangement.clone()).ok();
            let tempo_map = domains.sequencer.tempo_map().clone();
            let revision = publication.revisions.aggregate;
            let dirty = publication.snapshot.is_dirty();
            let history = self.session.read(cx).history_status().ok();
            view.update(cx, |view, cx| {
                match editor {
                    Some(editor) => {
                        view.set_project_snapshot(editor, revision, cx);
                        view.set_tempo_map(tempo_map, cx);
                    }
                    None => view.set_project_revision(revision, cx),
                }
                view.set_project_truth(revision, dirty, cx);
                if let Some(history) = history {
                    view.set_project_history(history, cx);
                }
            });
        }
        if let Some(view) = self.sequencer_view.as_ref() {
            let current_target = view.read(cx).target();
            let current_occurrence = view
                .read(cx)
                .source()
                .workflow
                .as_ref()
                .and_then(|workflow| workflow.occurrence);
            let mut note = None;
            let mut steps = None;
            for pattern in domains.sequencer.patterns().patterns() {
                match &pattern.content {
                    PatternContent::Notes(_) if note.is_none() => note = Some(pattern.id),
                    PatternContent::Steps(_) if steps.is_none() => steps = Some(pattern.id),
                    _ => {}
                }
            }
            let source = current_target
                .filter(|target| domains.sequencer.patterns().get(target.pattern).is_some())
                .map(|target| {
                    hydrated_pattern_source(
                        &publication.snapshot,
                        domains.sequencer.clone(),
                        target,
                        current_occurrence,
                        "Project patterns".into(),
                    )
                })
                .unwrap_or_else(|| {
                    SequencerEditorSource::new(
                        Arc::new(Mutex::new(domains.sequencer.clone())),
                        note,
                        steps,
                        "Project patterns",
                    )
                });
            view.update(cx, |view, cx| {
                view.set_source_snapshot(source, publication.revisions.aggregate, cx)
            });
        }
        if let Some(view) = self.mixer_view.as_ref() {
            view.update(cx, |view, cx| {
                view.set_controller_snapshot(domains.mixer.clone(), cx)
            });
        }
        if let Some(view) = self.automation_view.as_ref() {
            view.update(cx, |view, cx| {
                view.set_controller_snapshot(domains.automation.clone(), cx);
                view.set_mixer_snapshot(&domains.mixer, cx);
            });
        }

        self.request_project_audio(publication, cx);
        cx.notify();
    }

    pub(super) fn request_project_audio(
        &mut self,
        publication: ProjectPublication,
        cx: &mut Context<Self>,
    ) {
        let recipe = match project_audio_recipe(&publication, self.session.read(cx).id()) {
            Ok(recipe) => recipe,
            Err(error) => {
                self.audio_error = Some(error);
                return;
            }
        };
        if self.audio_snapshot_digest == Some(recipe.stamp.snapshot) {
            self.publish_audio_status(cx);
            return;
        }
        self.audio_snapshot_digest = Some(recipe.stamp.snapshot);
        if let Some(cancellation) = self.audio_render_cancellation.take() {
            cancellation.cancel();
        }
        let job = self.audio_controller.request_render(publication, recipe);
        // The controller owns generation cancellation. Retaining the job's
        // token here keeps the GPUI lifecycle and the tile/whole render
        // scheduler on the same cancellation authority.
        let cancellation = job.cancellation();
        self.audio_render_cancellation = Some(cancellation.clone());
        self.audio_rendering = true;
        let generation = job.generation();
        let render = cx.background_spawn(async move { job.execute(&cancellation) });
        cx.spawn(async move |this, cx| {
            let result = render.await;
            let _ = this.update(cx, |this, cx| {
                this.audio_rendering = false;
                let previous_transport = this
                    .audio_controller
                    .renderer_control()
                    .zip(this.audio.as_ref())
                    .map(|(control, host)| {
                        (
                            TransportEndpoint {
                                timeline: control.timeline(),
                                format: control.format(),
                            },
                            host.snapshot().transport,
                        )
                    });
                match result {
                    Ok(completion) => match this.audio_controller.complete_render(completion) {
                        Ok(ProjectAudioControllerEffect::OpenHost(renderer)) => {
                            match ProjectAudioOutputHost::open_renderer(
                                renderer,
                                ProjectAudioBackendPreference::default(),
                            ) {
                                Ok(host) => {
                                    if let Err(error) = this.audio_controller.bind_audio_host(&host)
                                    {
                                        this.audio_controller = this.fresh_audio_controller();
                                        this.audio_snapshot_digest = None;
                                        this.audio_error = Some(error.to_string());
                                        return;
                                    }
                                    if let Some(old) = this.audio.as_ref() {
                                        this.preview_controller.cancel_all(old);
                                    }
                                    this.pad_preview_tickets.clear();
                                    if let Some(old) = this.audio.take() {
                                        old.transport().stop();
                                    }
                                    this.audio = Some(host);
                                    this.audio_device_status = this.audio.as_ref().map(|host| {
                                        format!("{:?} output active", host.backend_kind())
                                    });
                                    // The kernel is the transport authority before
                                    // a host exists: restore its loop, playhead, and
                                    // playback mode so requests made during the
                                    // opening bounce are honoured rather than lost.
                                    let snapshot = this.timeline_interaction.snapshot();
                                    this.apply_timeline_transport_effect(
                                        TimelineTransportEffect::SetLoop(snapshot.loop_state),
                                        cx,
                                    );
                                    if let Some(range) = snapshot.selection.range {
                                        if let Ok(range) = FrameRange::new(
                                            ProjectFrame(range.start.get()),
                                            ProjectFrame(range.end.get()),
                                        ) {
                                            this.apply_project_transport_command(
                                                ProjectTransportCommand::ReplaceSelection(Some(
                                                    range,
                                                )),
                                                cx,
                                            );
                                        }
                                    }
                                    // A loop enabled before the host existed was
                                    // never located into; seeking to a playhead
                                    // outside it would disable it again. Land
                                    // where Play would: the loop start.
                                    let seek_to = match snapshot.loop_state.range {
                                        Some(range)
                                            if snapshot.loop_state.enabled
                                                && !range.contains(snapshot.playhead) =>
                                        {
                                            range.start
                                        }
                                        _ => snapshot.playhead,
                                    };
                                    this.apply_timeline_transport_effect(
                                        TimelineTransportEffect::Seek {
                                            to: seek_to,
                                            preserve_playback: false,
                                        },
                                        cx,
                                    );
                                    if snapshot.playback == TimelinePlaybackMode::Playing {
                                        this.apply_timeline_transport_effect(
                                            TimelineTransportEffect::Play,
                                            cx,
                                        );
                                    }
                                }
                                Err(error) => {
                                    this.audio_controller = this.fresh_audio_controller();
                                    this.audio_snapshot_digest = None;
                                    this.audio_error = Some(error.to_string());
                                }
                            }
                        }
                        Ok(ProjectAudioControllerEffect::ReplaceHost(renderer)) => {
                            let next = this.audio_controller.renderer_control().map(|control| {
                                TransportEndpoint {
                                    timeline: control.timeline(),
                                    format: control.format(),
                                }
                            });
                            match ProjectAudioOutputHost::open_renderer(
                                renderer,
                                ProjectAudioBackendPreference::default(),
                            ) {
                                Ok(host) => {
                                    let handoff = previous_transport
                                        .zip(next)
                                        .map(|((previous, snapshot), next)| {
                                            ProjectTransportHandoff::plan(previous, snapshot, next)
                                        })
                                        .transpose();
                                    match handoff.and_then(|handoff| {
                                        handoff
                                            .map(|handoff| handoff.apply(&host.transport()))
                                            .transpose()
                                    }) {
                                        Ok(_) => {
                                            if let Err(error) =
                                                this.audio_controller.bind_audio_host(&host)
                                            {
                                                this.audio_controller =
                                                    this.fresh_audio_controller();
                                                this.audio_snapshot_digest = None;
                                                this.audio_error = Some(error.to_string());
                                                return;
                                            }
                                            if let Some(old) = this.audio.as_ref() {
                                                this.preview_controller.cancel_all(old);
                                            }
                                            this.pad_preview_tickets.clear();
                                            if let Some(old) = this.audio.take() {
                                                old.transport().stop();
                                            }
                                            this.audio = Some(host);
                                            this.audio_device_status =
                                                this.audio.as_ref().map(|host| {
                                                    format!(
                                                        "{:?} output active",
                                                        host.backend_kind()
                                                    )
                                                });
                                        }
                                        Err(error) => {
                                            this.audio_controller = this.fresh_audio_controller();
                                            this.audio_snapshot_digest = None;
                                            this.audio_error = Some(error.to_string());
                                        }
                                    }
                                }
                                Err(error) => {
                                    this.audio_controller = this.fresh_audio_controller();
                                    this.audio_snapshot_digest = None;
                                    this.audio_error = Some(error.to_string());
                                }
                            }
                        }
                        Ok(
                            ProjectAudioControllerEffect::None
                            | ProjectAudioControllerEffect::Superseded { .. },
                        ) => {}
                        Err(error) => {
                            this.audio_snapshot_digest = None;
                            this.audio_error = Some(error.to_string());
                        }
                    },
                    Err(error) => {
                        this.audio_controller
                            .fail_render(generation, error.to_string());
                        this.audio_snapshot_digest = None;
                        this.audio_error = Some(error.to_string());
                    }
                }
                this.refresh_audible_export_audio();
                this.publish_audio_status(cx);
                if let Some((destination, options)) = this.pending_export.take() {
                    this.start_export_with(destination, options, cx);
                }
                cx.notify();
            });
        })
        .detach();
        self.publish_audio_status(cx);
    }

    pub(super) fn tick_project_audio(&mut self, cx: &mut Context<Self>) {
        if let Some(audio) = self.audio.as_mut() {
            match audio.poll_runtime() {
                Ok(Some(event)) => self.audio_device_status = Some(event.message),
                Ok(None) => {}
                Err(error) => self.audio_error = Some(error.to_string()),
            }
        }
        let Some(audio) = self.audio.as_ref() else {
            self.publish_audio_status(cx);
            return;
        };
        let host_snapshot = audio.snapshot();
        let status = ProjectAudioStatus {
            transport: host_snapshot.transport,
            ..self.audio_controller.status()
        };
        self.observe_timeline_audio(&status, cx);
        let observation = host_snapshot.into();
        match self.audio_controller.tick(observation) {
            Ok(Some(_)) => self.refresh_audible_export_audio(),
            Ok(None) => {}
            Err(error) => self.audio_error = Some(error.to_string()),
        }
        self.publish_audio_status(cx);
    }

    pub(super) fn publish_audio_status(&mut self, cx: &mut Context<Self>) {
        let status = self.audio_controller.status();
        self.session.update(cx, |session, _| {
            session.set_audio_status(status);
        });
        self.publish_mixer_meters(cx);
    }

    /// Post-DSP meters come from the acknowledged playback cohort, never from
    /// a decorative animation. Missing cohort leaves strips at silence.
    pub(super) fn publish_mixer_meters(&mut self, cx: &mut Context<Self>) {
        let Ok(snapshot) = self.session.read(cx).project_snapshot().cloned() else {
            return;
        };
        let master = snapshot.project.state().domains.mixer.master();
        let service = self.audio_controller.runtime().service();
        let render_status = ControlRenderStatus::from_snapshots(
            &service.status(),
            self.audio_controller.renderer_status(),
        );
        let meters = service
            .active_cohort()
            .map(|cohort| MixerMeterSnapshot::from_audible_cohort(&cohort, master));
        let mut mixers = Vec::new();
        if let Some(view) = self.mixer_view.clone() {
            mixers.push(view);
        }
        for runtime in self.workspace_panes.values() {
            let WorkspacePaneRuntime::Hosted(host) = runtime else {
                continue;
            };
            let Some(host) = host.upgrade() else {
                continue;
            };
            if let WorkspacePaneContent::Mixer(view) = &host.read(cx).content {
                mixers.push(view.clone());
            }
        }
        for view in mixers {
            view.update(cx, |view, cx| {
                view.set_render_status(Some(render_status.clone()), cx);
                if let Some(meters) = meters.as_ref() {
                    view.set_meter_snapshot(meters.clone(), cx);
                }
            });
        }
    }

    pub(super) fn refresh_audible_export_audio(&mut self) {
        let Some(control) = self.audio_controller.renderer_control() else {
            return;
        };
        let span = control.timeline();
        let pin = self.audio_controller.pin_audible_export(
            RenderScope::Master,
            span,
            OutputTailPolicy::Crop,
        );
        if let Ok(pin) = pin {
            if let Ok(rendered) = self
                .audio_controller
                .render_export(&pin, &RenderCancellation::new())
            {
                self.audition_audio = Some(rendered.audio);
            }
        }
    }

    pub(super) fn sync_arrangement_playhead(&self, playing: bool, cx: &mut Context<Self>) {
        let Some(view) = self.arrangement_view.as_ref() else {
            return;
        };
        let playhead =
            ArrangementFrame::new(i64::try_from(self.playhead_sample()).unwrap_or(i64::MAX));
        view.update(cx, |view, cx| view.set_playhead(playhead, playing, cx));
    }

    pub(super) fn sync_pattern_placement_frame(&self, cx: &mut Context<Self>) {
        let frame = ArrangementFrame::new(
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
        if let Some(view) = self.sequencer_view.as_ref() {
            view.update(cx, |view, cx| view.set_placement_frame(frame, cx));
        }
        for runtime in self.workspace_panes.values() {
            let WorkspacePaneRuntime::Hosted(host) = runtime else {
                continue;
            };
            let Some(host) = host.upgrade() else {
                continue;
            };
            let pattern = match &host.read(cx).content {
                WorkspacePaneContent::Pattern(view) => Some(view.clone()),
                _ => None,
            };
            if let Some(view) = pattern {
                view.update(cx, |view, cx| view.set_placement_frame(frame, cx));
            }
        }
    }

    pub(super) fn current_arrangement_timeline_state(
        &self,
    ) -> (
        Option<ArrangementFrameRange>,
        Option<ArrangementFrameRange>,
        bool,
    ) {
        let snapshot = self.audio_controller.transport_session().snapshot();
        let convert = |range: FrameRange| {
            ArrangementFrameRange::new(
                ArrangementFrame::new(i64::try_from(range.start.0).ok()?),
                ArrangementFrame::new(i64::try_from(range.end.0).ok()?),
            )
            .ok()
        };
        (
            snapshot.selection.and_then(convert),
            snapshot.transport.loop_region.and_then(convert),
            snapshot.transport.loop_enabled,
        )
    }

    pub(super) fn apply_arrangement_timeline_state(
        &self,
        view: &Entity<ArrangementView>,
        cx: &mut Context<Self>,
    ) {
        let (selection, loop_range, loop_enabled) = self.current_arrangement_timeline_state();
        view.update(cx, |view, cx| {
            view.set_time_selection(selection, cx);
            view.set_loop_range(loop_range, cx);
            if !loop_enabled {
                view.set_loop_range(None, cx);
            }
        });
    }

    pub(super) fn sync_arrangement_timeline_views(&self, cx: &mut Context<Self>) {
        if let Some(view) = self.arrangement_view.as_ref() {
            self.apply_arrangement_timeline_state(view, cx);
        }
        for runtime in self.workspace_panes.values() {
            let WorkspacePaneRuntime::Hosted(host) = runtime else {
                continue;
            };
            let Some(host) = host.upgrade() else {
                continue;
            };
            let arrangement = match &host.read(cx).content {
                WorkspacePaneContent::Arrangement(view) => Some(view.clone()),
                _ => None,
            };
            if let Some(view) = arrangement {
                self.apply_arrangement_timeline_state(&view, cx);
            }
        }
    }

    pub(super) fn arrangement_waveform_provider(
        &self,
        snapshot: &LiveProjectSnapshot,
    ) -> Option<ArrangementWaveformProvider> {
        let analysis = self.analysis()?;
        let state = snapshot.project.state();
        let (&arrangement_asset, &registry_asset) = state
            .bindings
            .assets
            .arrangement_assets
            .iter()
            .find(|(_, registry_asset)| {
                state
                    .domains
                    .assets
                    .get(**registry_asset)
                    .is_some_and(|media| {
                        media.metadata().frame_count.0
                            == analysis.waveform_pyramid.frame_count() as u64
                            && media.metadata().sample_rate_hz == analysis.sample_rate
                    })
            })?;
        let media = state.domains.assets.get(registry_asset)?;
        let metadata = media.metadata();
        let key = WaveformAssetKey::new(
            registry_asset,
            media.content(),
            metadata.sample_rate_hz,
            metadata.channels,
            metadata.frame_count,
        )
        .ok()?;
        let source = ArrangementWaveformSource {
            key,
            pyramid: Arc::new(analysis.waveform_pyramid.clone()),
        };
        Some(Arc::new(move |asset| {
            (asset == arrangement_asset).then(|| source.clone())
        }))
    }
}
