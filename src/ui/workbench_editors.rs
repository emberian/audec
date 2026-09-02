//! Opening editors, visualizers, and dynamic workspace panes.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

impl Workbench {
    pub(super) fn open_visualizer(&mut self, kind: VizKind, cx: &mut Context<Self>) {
        let workbench = cx.entity();
        let options = visualizer_window_options(kind, cx);
        // `open_window` renders its root synchronously. Defer until this action's
        // Workbench update lease has ended so the new view can safely observe it.
        cx.defer(move |cx| {
            if let Err(error) = cx.open_window(options, move |window, cx| {
                let visualizer = cx.new(|cx| Visualizer::new(kind, workbench, cx));
                window.focus(&visualizer.focus_handle(cx), cx);
                if kind == VizKind::Rhythm {
                    visualizer.update(cx, |visualizer, cx| visualizer.refresh_rhythm(cx));
                } else if kind == VizKind::Separation {
                    visualizer.update(cx, |visualizer, cx| visualizer.refresh_hpss(cx));
                } else if kind == VizKind::Loom {
                    visualizer.update(cx, |visualizer, cx| visualizer.refresh_loom(cx));
                }
                visualizer
            }) {
                eprintln!("opening {}: {error:#}", kind.title());
            }
        });
    }

    pub(super) fn create_arrangement_view(
        &mut self,
        source: Option<WorkspaceViewId>,
        cx: &mut Context<Self>,
    ) -> Entity<ArrangementView> {
        if let Ok(snapshot) = self.session.read(cx).project_snapshot().cloned() {
            let domains = &snapshot.project.state().domains;
            let aggregate_revision = snapshot.revisions().aggregate;
            let tempo_map = domains.sequencer.tempo_map().clone();
            let editor =
                ArrangementEditor::from_state(domains.arrangement.clone()).unwrap_or_else(|_| {
                    ArrangementEditor::new(domains.arrangement.sample_rate)
                        .expect("published arrangement sample rate is valid")
                });
            let selection = editor.selection.clone();
            let shared = Arc::new(Mutex::new(editor));
            let events = Arc::clone(&self.arrangement_events);
            let callback = Arc::new(move |event| {
                if let Ok(mut events) = events.lock() {
                    events.push(PendingArrangementEvent { source, event });
                }
            });
            let waveform_provider = self.arrangement_waveform_provider(&snapshot);
            let entity = cx.new(|cx| {
                ArrangementView::from_shared_sources(
                    shared,
                    aggregate_revision,
                    callback,
                    waveform_provider,
                    cx,
                )
            });
            let timeline_events = Arc::clone(&self.arrangement_timeline_events);
            let timeline_callback = Arc::new(move |event| {
                if let Ok(mut events) = timeline_events.lock() {
                    events.push(PendingArrangementTimelineEvent { source, event });
                }
            });
            let playhead =
                ArrangementFrame::new(i64::try_from(self.playhead_sample()).unwrap_or(i64::MAX));
            let playing = self.transport_is_playing();
            entity.update(cx, |editor, cx| {
                editor.set_timeline_callback(Some(timeline_callback));
                editor.set_tempo_map(tempo_map, cx);
                editor.set_project_revision(aggregate_revision, cx);
                editor.set_selection(selection, cx);
                editor.set_playhead(playhead, playing, cx);
            });
            self.apply_arrangement_timeline_state(&entity, cx);
            entity
        } else {
            let editor_state = self.analysis().and_then(|analysis| {
                let total_frames = analysis.waveform_pyramid.frame_count() as u64;
                let mut editor = ArrangementEditor::new(analysis.sample_rate).ok()?;
                let track = editor
                    .create_track("Source material", TrackKind::Audio)
                    .ok()?;
                let placement = ArrangementFrameRange::new(
                    ArrangementFrame::ZERO,
                    ArrangementFrame::new(i64::try_from(total_frames).ok()?),
                )
                .ok()?;
                let source = ArrangementSourceRange::new(0, total_frames).ok()?;
                let asset = ArrangementAssetId::from_raw(stable_source_id(
                    &analysis.path.to_string_lossy(),
                    total_frames,
                    analysis.sample_rate,
                ));
                editor
                    .create_audio_clip(track, analysis.title.clone(), placement, asset, source)
                    .ok()?;
                editor.mark_saved();
                Some(editor)
            });
            let entity = cx.new(|cx| match editor_state {
                Some(editor) => ArrangementView::new(editor, cx),
                None => ArrangementView::demo(cx),
            });
            let timeline_events = Arc::clone(&self.arrangement_timeline_events);
            let timeline_callback = Arc::new(move |event| {
                if let Ok(mut events) = timeline_events.lock() {
                    events.push(PendingArrangementTimelineEvent { source, event });
                }
            });
            entity.update(cx, |editor, _| {
                editor.set_timeline_callback(Some(timeline_callback))
            });
            self.apply_arrangement_timeline_state(&entity, cx);
            entity
        }
    }

    pub(super) fn open_arrangement_editor(&mut self, cx: &mut Context<Self>) {
        let editor = self.arrangement_view.clone().unwrap_or_else(|| {
            let editor = self.create_arrangement_view(None, cx);
            self.arrangement_view = Some(editor.clone());
            editor
        });
        let options = editor_window_options("Arrangement editor", cx);
        cx.defer(move |cx| {
            if let Err(error) = cx.open_window(options, move |window, cx| {
                window.focus(&editor.focus_handle(cx), cx);
                editor.clone()
            }) {
                eprintln!("opening Arrangement editor: {error:#}");
            }
        });
    }

    pub(super) fn open_sequencer_editor(&mut self, cx: &mut Context<Self>) {
        let editor = if let Some(editor) = &self.sequencer_view {
            editor.clone()
        } else if let Ok(snapshot) = self.session.read(cx).project_snapshot().cloned() {
            let revision = snapshot.revisions().aggregate;
            let sequencer = snapshot.project.state().domains.sequencer.clone();
            let (note_pattern, step_pattern) = {
                let mut note_pattern = None;
                let mut step_pattern = None;
                for pattern in sequencer.patterns().patterns() {
                    match pattern.content {
                        PatternContent::Notes(_) if note_pattern.is_none() => {
                            note_pattern = Some(pattern.id)
                        }
                        PatternContent::Steps(_) if step_pattern.is_none() => {
                            step_pattern = Some(pattern.id)
                        }
                        _ => {}
                    }
                }
                (note_pattern, step_pattern)
            };
            let source = step_pattern
                .or(note_pattern)
                .map(|pattern| {
                    let mode = if step_pattern == Some(pattern) {
                        PatternEditorMode::Steps
                    } else {
                        PatternEditorMode::PianoRoll
                    };
                    hydrated_pattern_source(
                        &snapshot,
                        sequencer.clone(),
                        PatternEditorTarget::new(pattern, mode),
                        None,
                        "Project patterns".into(),
                    )
                })
                .unwrap_or_else(|| {
                    SequencerEditorSource::new(
                        Arc::new(Mutex::new(sequencer)),
                        note_pattern,
                        step_pattern,
                        "Project patterns",
                    )
                });
            let entity = cx.new(|cx| SequencerEditor::new(source, cx));
            self.install_pattern_workflow_callback(&entity, revision, None, cx);
            self.sequencer_view = Some(entity.clone());
            entity
        } else {
            let editor = cx.new(SequencerEditor::demo);
            editor.update(cx, |editor, cx| {
                editor.set_audition_availability(
                    SequencerAuditionAvailability::unavailable(
                        "Open a project before auditioning a pattern",
                    ),
                    cx,
                )
            });
            self.sequencer_view = Some(editor.clone());
            editor
        };
        open_editor_entity(editor, "Piano roll + drum sequencer", cx);
    }

    pub(super) fn open_mixer(&mut self, cx: &mut Context<Self>) {
        let mixer = if let Some(mixer) = &self.mixer_view {
            mixer.clone()
        } else if let Ok(snapshot) = self.session.read(cx).project_snapshot().cloned() {
            let graph = snapshot.project.state().domains.mixer.clone();
            let actions = Arc::clone(&self.control_actions);
            let callback = Arc::new(move |action| {
                if let Ok(mut actions) = actions.lock() {
                    actions.push(PendingControlAction {
                        editor_session: 0,
                        action,
                    });
                }
            });
            let entity =
                cx.new(|cx| MixerView::from_controller_snapshot(graph, None, callback, cx));
            self.mixer_view = Some(entity.clone());
            entity
        } else {
            let actions = Arc::clone(&self.control_actions);
            let callback = Arc::new(move |action| {
                if let Ok(mut actions) = actions.lock() {
                    actions.push(PendingControlAction {
                        editor_session: 0,
                        action,
                    });
                }
            });
            self.constructive_status =
                Some("Mixer opened without a project; channel edits are not kept".into());
            let mixer = cx.new(|cx| {
                MixerView::from_controller_snapshot(
                    crate::mixer::MixerGraph::default(),
                    None,
                    callback,
                    cx,
                )
            });
            self.mixer_view = Some(mixer.clone());
            mixer
        };
        open_editor_entity(mixer, "Mixer", cx);
    }

    pub(super) fn open_automation(&mut self, cx: &mut Context<Self>) {
        let automation = if let Some(automation) = &self.automation_view {
            automation.clone()
        } else if let Ok(snapshot) = self.session.read(cx).project_snapshot().cloned() {
            let domains = &snapshot.project.state().domains;
            let graph = domains.automation.clone();
            let mixer = domains.mixer.clone();
            let target = graph.lanes().next().map(|lane| lane.id);
            let actions = Arc::clone(&self.control_actions);
            let callback = Arc::new(move |action| {
                if let Ok(mut actions) = actions.lock() {
                    actions.push(PendingControlAction {
                        editor_session: 0,
                        action,
                    });
                }
            });
            let entity = cx.new(|cx| {
                AutomationView::from_controller_snapshots_optional(
                    graph, &mixer, target, callback, cx,
                )
            });
            self.automation_view = Some(entity.clone());
            entity
        } else {
            let automation = cx.new(|cx| {
                AutomationView::from_graph(crate::automation::AutomationGraph::new(), cx)
            });
            self.automation_view = Some(automation.clone());
            automation
        };
        open_editor_entity(automation, "Automation", cx);
    }

    pub(super) fn open_assets(&mut self, cx: &mut Context<Self>) {
        let browser = if let Some(browser) = &self.asset_view {
            browser.clone()
        } else {
            let registry = Arc::clone(&self.asset_registry);
            let events = Arc::clone(&self.asset_events);
            let callback = Arc::new(move |event| {
                if let Ok(mut queue) = events.lock() {
                    queue.push(event);
                }
            });
            let browser =
                cx.new(|cx| AssetBrowserView::with_callback(registry, Some(callback), cx));
            self.asset_view = Some(browser.clone());
            browser
        };
        if let Ok(snapshot) = self.session.read(cx).project_snapshot() {
            let material_pool = MaterialPoolSnapshot::from_project(&snapshot.project);
            browser.update(cx, |browser, cx| {
                browser.set_material_pool_snapshot(material_pool, cx)
            });
        }
        self.install_browser_sample_callbacks(&browser, None, cx);
        open_editor_entity(browser, "Media pool", cx);
    }

    pub(super) fn create_workspace_pane(
        &mut self,
        descriptor: &WorkspaceViewDescriptor,
        cx: &mut Context<Self>,
    ) -> Result<PaneRegistration, SharedString> {
        match resolve_specialized_presenter(descriptor)
            .map_err(|error| SharedString::from(error.to_string()))?
        {
            Some(SpecializedWorkspacePresenter::ExplanationWorkbench(route)) => {
                let resolved = self
                    .session
                    .read(cx)
                    .resolve_deprojection_workspace_request(route.deprojection_target())
                    .map_err(|error| SharedString::from(error.to_string()))?;
                let pane = self
                    .explanation_workbench_factory
                    .create_pane(&route, resolved, cx)?;
                self.install_unbound_workspace_runtime(
                    descriptor.id,
                    WorkspacePaneRuntime::ExplanationWorkbench,
                    cx,
                );
                return Ok(pane);
            }
            Some(SpecializedWorkspacePresenter::ReadingQuery) | None => {}
        }
        let reverse_target = crate::project_controller::object_from_descriptor(descriptor)
            .map_err(|error| SharedString::from(error.to_string()))?
            .is_some_and(|object| {
                matches!(
                    object,
                    ObjectRef::Finding(_)
                        | ObjectRef::Explanation(_)
                        | ObjectRef::Comparison(_)
                        | ObjectRef::Reading(_)
                )
            });
        if reverse_target {
            let pane = self.reverse_surface_factory.create_pane(descriptor, cx)?;
            self.install_unbound_workspace_runtime(
                descriptor.id,
                WorkspacePaneRuntime::Reverse,
                cx,
            );
            return Ok(pane);
        }
        let title = workspace_view_title(descriptor);
        let content = match &descriptor.kind {
            WorkspaceKind::Overview => WorkspacePaneContent::Overview(cx.entity()),
            WorkspaceKind::Arrangement => {
                let view = self.create_arrangement_view(Some(descriptor.id), cx);
                if let WorkspaceViewState::Arrangement {
                    viewport, follow, ..
                } = &descriptor.state
                {
                    view.update(cx, |view, cx| {
                        view.set_viewport(
                            ArrangementViewport::new(
                                ArrangementFrame::new(viewport.start),
                                ArrangementFrame::new(viewport.end),
                                1,
                            ),
                            cx,
                        );
                        view.set_follow_playhead(*follow, cx);
                    });
                }
                WorkspacePaneContent::Arrangement(view)
            }
            WorkspaceKind::Browser => {
                let events = Arc::clone(&self.asset_events);
                let callback = Arc::new(move |event| {
                    if let Ok(mut events) = events.lock() {
                        events.push(event);
                    }
                });
                let view = cx.new(|cx| {
                    AssetBrowserView::with_callback(
                        Arc::clone(&self.asset_registry),
                        Some(callback),
                        cx,
                    )
                });
                if let Ok(snapshot) = self.session.read(cx).project_snapshot() {
                    let material_pool = MaterialPoolSnapshot::from_project(&snapshot.project);
                    view.update(cx, |view, cx| {
                        view.set_material_pool_snapshot(material_pool, cx)
                    });
                }
                if let Some(state) = browser_state_from_descriptor(descriptor) {
                    view.update(cx, |view, cx| view.set_state(state, cx));
                }
                self.install_browser_sample_callbacks(&view, Some(descriptor.id), cx);
                WorkspacePaneContent::Browser(view)
            }
            WorkspaceKind::Extension { namespace, name }
                if namespace == crate::air_query::workbench::WORKBENCH_NAMESPACE
                    && name == crate::air_query::workbench::WORKBENCH_VIEW_NAME =>
            {
                let WorkspaceViewState::Extension { data } = &descriptor.state else {
                    let notice = cx.new(|_| {
                        WorkspaceNotice::new("Reading query has no portable document state")
                    });
                    return self.finish_workspace_pane(
                        descriptor,
                        title,
                        WorkspacePaneContent::Notice(notice),
                        cx,
                    );
                };
                let document = serde_json::from_value::<QueryDocument>(data.clone())
                    .map_err(|error| SharedString::from(error.to_string()))?;
                let model = WorkbenchPaneFactory::model(document)
                    .map_err(|error| SharedString::from(error.to_string()))?;
                let effects = Rc::clone(&self.reading_query_effects);
                let source = descriptor.id;
                let callback = Rc::new(move |effect| {
                    effects
                        .borrow_mut()
                        .push(PendingReadingQueryEffect { source, effect });
                });
                let view = cx.new(|cx| ReadingQueryView::from_model(model, callback, cx));
                if let Ok(bridge) = self.capture_reading_query_session(cx) {
                    let inputs = ReadingQueryViewInputs {
                        query_provenance: Some(bridge.snapshot().provenance()),
                        existing_entities: bridge
                            .snapshot()
                            .existing_foreign_entities()
                            .into_iter()
                            .collect(),
                        base_revision: Some(
                            self.session
                                .read(cx)
                                .project_snapshot()
                                .map_err(|error| SharedString::from(error.to_string()))?
                                .revisions()
                                .aggregate,
                        ),
                        ..ReadingQueryViewInputs::default()
                    };
                    view.update(cx, |view, cx| view.observe_inputs(inputs, cx));
                }
                WorkspacePaneContent::ReadingQuery(view)
            }
            WorkspaceKind::PatternEditor { mode } => {
                let Ok(snapshot) = self.session.read(cx).project_snapshot().cloned() else {
                    let notice =
                        cx.new(|_| WorkspaceNotice::new("Open a project to edit patterns"));
                    return self.finish_workspace_pane(
                        descriptor,
                        title,
                        WorkspacePaneContent::Notice(notice),
                        cx,
                    );
                };
                let revision = snapshot.revisions().aggregate;
                let source = workspace_pattern_source(descriptor, &snapshot, None);
                let view = cx.new(|cx| {
                    let mut view = SequencerEditor::new(source, cx);
                    view.set_mode(
                        match mode {
                            WorkspacePatternMode::PianoRoll => {
                                crate::sequencer_view::EditorMode::PianoRoll
                            }
                            WorkspacePatternMode::Steps => crate::sequencer_view::EditorMode::Steps,
                        },
                        cx,
                    );
                    view
                });
                self.install_pattern_workflow_callback(&view, revision, Some(descriptor.id), cx);
                WorkspacePaneContent::Pattern(view)
            }
            WorkspaceKind::Mixer => {
                let Ok(snapshot) = self.session.read(cx).project_snapshot().cloned() else {
                    let notice = cx.new(|_| WorkspaceNotice::new("Open a project to mix"));
                    return self.finish_workspace_pane(
                        descriptor,
                        title,
                        WorkspacePaneContent::Notice(notice),
                        cx,
                    );
                };
                let graph = snapshot.project.state().domains.mixer.clone();
                let target = match descriptor.target {
                    WorkspaceTarget::Mixer { bus_id: Some(id) }
                        if graph.bus(crate::mixer::BusId::from_raw(id)).is_some() =>
                    {
                        Some(crate::mixer::BusId::from_raw(id))
                    }
                    _ => None,
                };
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
                WorkspacePaneContent::Mixer(
                    cx.new(|cx| MixerView::from_controller_snapshot(graph, target, callback, cx)),
                )
            }
            WorkspaceKind::AutomationEditor => {
                let Ok(snapshot) = self.session.read(cx).project_snapshot().cloned() else {
                    let notice =
                        cx.new(|_| WorkspaceNotice::new("Open a project to edit automation"));
                    return self.finish_workspace_pane(
                        descriptor,
                        title,
                        WorkspacePaneContent::Notice(notice),
                        cx,
                    );
                };
                let domains = &snapshot.project.state().domains;
                let graph = domains.automation.clone();
                let mixer = domains.mixer.clone();
                let requested = match descriptor.target {
                    WorkspaceTarget::AutomationLane { id } if id != 0 => {
                        Some(crate::automation::AutomationLaneId::from_raw(id))
                    }
                    _ => None,
                };
                let target = requested
                    .filter(|target| graph.lane(*target).is_some())
                    .or_else(|| graph.lanes().next().map(|lane| lane.id));
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
                WorkspacePaneContent::Automation(cx.new(|cx| {
                    AutomationView::from_controller_snapshots_optional(
                        graph, &mixer, target, callback, cx,
                    )
                }))
            }
            WorkspaceKind::AnalysisLens { lens } => {
                let kind = match lens {
                    AnalysisLensKind::Waterfall
                    | AnalysisLensKind::Waveform
                    | AnalysisLensKind::Spectrum => VizKind::Waterfall,
                    AnalysisLensKind::Rhythm => VizKind::Rhythm,
                    AnalysisLensKind::Components
                    | AnalysisLensKind::Coverage
                    | AnalysisLensKind::Comparison
                    | AnalysisLensKind::AirQuery => VizKind::Components,
                    AnalysisLensKind::Separation => VizKind::Separation,
                    AnalysisLensKind::Loom => VizKind::Loom,
                };
                // This runs inside the Workbench's own update lease: the lens
                // must be seeded from `self`, not by reading the entity, and
                // its first refresh (which reads the Workbench) must wait
                // until the lease has ended.
                let workbench = cx.entity();
                let analysis = self.analysis_arc();
                let playhead = self.playhead_fraction() as f64;
                let view =
                    cx.new(|cx| Visualizer::with_seed(kind, workbench, analysis, playhead, cx));
                view.update(cx, |view, _| view.set_workspace_view_id(descriptor.id));
                let refresh = view.clone();
                cx.defer(move |cx| {
                    refresh.update(cx, |view, cx| match kind {
                        VizKind::Rhythm => view.refresh_rhythm(cx),
                        VizKind::Separation => view.refresh_hpss(cx),
                        VizKind::Loom => view.refresh_loom(cx),
                        VizKind::Waterfall | VizKind::Components => {}
                    });
                });
                WorkspacePaneContent::Analysis(view)
            }
            WorkspaceKind::Extension { namespace, name }
                if namespace == "audec" && name == "sampler" =>
            {
                let Ok(snapshot) = self.session.read(cx).project_snapshot().cloned() else {
                    let notice =
                        cx.new(|_| WorkspaceNotice::new("Open a project to edit sampler pads"));
                    return self.finish_workspace_pane(
                        descriptor,
                        title,
                        WorkspacePaneContent::Notice(notice),
                        cx,
                    );
                };
                let kits = snapshot.project.state().domains.sample_kits.clone();
                let Some(fallback) = kits.kits.keys().next().copied() else {
                    let notice =
                        cx.new(|_| WorkspaceNotice::new("Create a sample kit to open pad editing"));
                    return self.finish_workspace_pane(
                        descriptor,
                        title,
                        WorkspacePaneContent::Notice(notice),
                        cx,
                    );
                };
                let target = sampler_target_from_descriptor(descriptor)
                    .filter(|target| target.kit().is_some_and(|kit| kits.kits.contains_key(&kit)))
                    .unwrap_or(SamplerTarget::Kit(fallback));
                let kit = target.kit().unwrap_or(fallback);
                let mixer = snapshot.project.state().domains.mixer.clone();
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
                    Arc::new(Mutex::new(kits)),
                    Arc::new(Mutex::new(snapshot.project.state().domains.assets.clone())),
                    kit,
                    buses,
                );
                let view = cx.new(|cx| {
                    let mut view = SamplerView::new(source, cx);
                    view.retarget(target, cx);
                    view
                });
                self.install_sampler_sample_callbacks(&view, Some(descriptor.id), cx);
                WorkspacePaneContent::Sampler(view)
            }
            _ => WorkspacePaneContent::Notice(cx.new(|_| {
                WorkspaceNotice::new("This workspace item is not available in this build")
            })),
        };
        self.finish_workspace_pane(descriptor, title, content, cx)
    }

    pub(super) fn finish_workspace_pane(
        &mut self,
        descriptor: &WorkspaceViewDescriptor,
        title: SharedString,
        content: WorkspacePaneContent,
        cx: &mut Context<Self>,
    ) -> Result<PaneRegistration, SharedString> {
        let workbench = cx.entity().downgrade();
        let host = cx.new(move |cx| {
            WorkspacePaneHost::new(
                descriptor.clone(),
                content,
                workbench,
                cx.focus_handle().tab_stop(true),
            )
        });
        self.install_unbound_workspace_runtime(
            descriptor.id,
            WorkspacePaneRuntime::Hosted(host.downgrade()),
            cx,
        );
        Ok(PaneRegistration::entity(title, host))
    }
}
