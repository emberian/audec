//! DawWorkspace action projection, palette, context menu, and dispatch.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

impl DawWorkspace {
    pub(super) fn persist_editor_viewports(&mut self, cx: &mut Context<Self>) {
        let states = self.workbench.read(cx).collect_editor_view_states(cx);
        let Ok(mut layout) = self.workspace_layout.lock() else {
            return;
        };
        for (view, state) in states {
            let _ = layout.update_view_state(PaneInstanceId(view), state);
        }
    }

    /// "+ insert" from outside the strip: one native filter on the master,
    /// with the defaults the picker would use. The strip's own picker is a
    /// pointer control, so this is what a menu, the palette and the control
    /// socket can reach; the receipt says which bus carries it.
    pub(super) fn insert_effect_on_master(
        &mut self,
        effect: crate::mixer::NativeEffectKind,
        cx: &mut Context<Self>,
    ) {
        let session = self.workbench.read(cx).session.clone();
        let target = session.read(cx).project_snapshot().ok().map(|snapshot| {
            let mixer = &snapshot.project.state().domains.mixer;
            let master = mixer.master();
            (
                mixer.revision(),
                master,
                mixer
                    .bus(master)
                    .map(|bus| bus.name().to_owned())
                    .unwrap_or_else(|| "Master".into()),
            )
        });
        let Some((revision, master, name)) = target else {
            self.action_failure("Insert needs an open project", cx);
            return;
        };
        let label = effect.display_name();
        self.workbench.update(cx, |workbench, cx| {
            let before = workbench.constructive_status.clone();
            workbench.on_control_action(
                None,
                ControlAction::Mixer(
                    crate::control_views::control_actions::MixerActionIntent::new(
                        revision,
                        crate::control_views::control_actions::MixerAction::AddInsert {
                            bus: master,
                            effect,
                        },
                    ),
                ),
                cx,
            );
            // A refusal writes its own reason; anything else means the
            // envelope was accepted, and the receipt names what now runs.
            if workbench.constructive_status == before {
                workbench.constructive_status =
                    Some(format!("Insert · {label} on '{name}' · active"));
            }
            cx.notify();
        });
    }

    /// The insert row's ↑ on the master's last insert, asked for by name.
    ///
    /// The destination is the identity the row would have named, so this and
    /// the arrow move the same insert to the same place through the same
    /// `MoveInsertBefore` command.
    pub(super) fn move_last_insert_up_on_master(&mut self, cx: &mut Context<Self>) {
        let session = self.workbench.read(cx).session.clone();
        let chain = session.read(cx).project_snapshot().ok().map(|snapshot| {
            let mixer = &snapshot.project.state().domains.mixer;
            let master = mixer.master();
            // Name each insert the way the row does, so the receipt says which
            // effect moved rather than which integer did.
            let inserts: Vec<(crate::mixer::ProcessorId, String)> = mixer
                .bus(master)
                .map(|bus| {
                    bus.inserts()
                        .iter()
                        .map(|slot| {
                            let id = slot.processor_id();
                            let name = mixer
                                .processor(id)
                                .map(|processor| processor.descriptor().display_name.clone())
                                .unwrap_or_else(|| format!("insert {}", id.get()));
                            (id, name)
                        })
                        .collect()
                })
                .unwrap_or_default();
            (mixer.revision(), inserts)
        });
        let Some((revision, inserts)) = chain else {
            self.action_failure("Reordering an insert needs an open project", cx);
            return;
        };
        if inserts.len() < 2 {
            self.action_failure(
                "The master has fewer than two inserts; there is no earlier slot to move to",
                cx,
            );
            return;
        }
        let (processor, moved) = inserts[inserts.len() - 1].clone();
        let (before, destination) = inserts[inserts.len() - 2].clone();
        self.workbench.update(cx, |workbench, cx| {
            let previous = workbench.constructive_status.clone();
            workbench.on_control_action(
                None,
                ControlAction::Mixer(
                    crate::control_views::control_actions::MixerActionIntent::new(
                        revision,
                        crate::control_views::control_actions::MixerAction::MoveInsertBefore {
                            processor,
                            before: Some(before),
                        },
                    ),
                ),
                cx,
            );
            if workbench.constructive_status == previous {
                workbench.constructive_status = Some(format!(
                    "Insert · {moved} moved before {destination} on the master"
                ));
            }
            cx.notify();
        });
    }

    /// Drive the mixer strip's routing drop from outside the window.
    ///
    /// The decision is `control_actions::next_output_route`, the same function
    /// the OUTPUT button and a strip-on-strip drop go through, so this action
    /// cannot route somewhere the gesture would refuse. The channel is the one
    /// the project selection names; with none selected it is the first channel
    /// that is not the master, and the receipt says which one moved.
    pub(super) fn route_selected_channel(&mut self, cx: &mut Context<Self>) {
        use crate::control_views::control_actions::{next_output_route, ControlAction};
        let session = self.workbench.read(cx).session.clone();
        let selected = match session.read(cx).selection().selection.objects.primary {
            Some(crate::project_controller::ObjectRef::Bus(bus)) => Some(bus),
            _ => None,
        };
        // The snapshot is borrowed from the session, so the graph is taken
        // out of it before anything asks for the context mutably.
        let graph = session
            .read(cx)
            .project_snapshot()
            .ok()
            .map(|snapshot| snapshot.project.state().domains.mixer.clone());
        let Some(graph) = graph else {
            self.action_failure("Routing needs an open project", cx);
            return;
        };
        let Some(source) = selected
            .filter(|bus| graph.bus(*bus).is_some())
            .or_else(|| {
                graph
                    .buses()
                    .map(|bus| bus.id())
                    .find(|bus| *bus != graph.master())
            })
        else {
            self.action_failure("This project has only a master channel", cx);
            return;
        };
        let intent = match next_output_route(&graph, source) {
            Ok(intent) => intent,
            Err(refusal) => {
                self.action_failure(format!("Route refused · {refusal}"), cx);
                return;
            }
        };
        let name = |bus| {
            graph
                .bus(bus)
                .map(|bus| bus.name().to_owned())
                .unwrap_or_else(|| format!("channel {bus}"))
        };
        let receipt = match intent.action {
            crate::control_views::control_actions::MixerAction::SetOutput { bus, target } => {
                format!("Route · {} → {}", name(bus), name(target))
            }
            _ => "Route".to_owned(),
        };
        self.workbench.update(cx, |workbench, cx| {
            let before = workbench.constructive_status.clone();
            workbench.on_control_action(None, ControlAction::Mixer(intent), cx);
            // A refusal writes its own reason; anything else means the
            // envelope was accepted and the receipt names what now plays
            // through what.
            if workbench.constructive_status == before {
                workbench.constructive_status = Some(receipt);
            }
            cx.notify();
        });
    }

    pub fn workspace_document(&self) -> WorkspaceDocument {
        self.workspace_layout
            .lock()
            .map(|layout| {
                layout
                    .export_document()
                    .unwrap_or_else(|_| layout.document().clone())
            })
            .unwrap_or_else(|poisoned| {
                let layout = poisoned.into_inner();
                layout
                    .export_document()
                    .unwrap_or_else(|_| layout.document().clone())
            })
    }

    pub(super) fn action_context_material(
        &self,
        view_override: Option<WorkspaceViewId>,
        cx: &App,
    ) -> (ActionContextSignature, ActionContext) {
        let document = self.workspace_document();
        let workbench = self.workbench.read(cx);
        let session = workbench.session.read(cx);
        // The layout's focused pane is what "active" means; the Workbench
        // keeps a mirror of it for the panes it hosts. When the mirror is
        // empty (first frame, or right after a document install) the layout
        // still answers, so no verb is refused for a pane that is plainly
        // there.
        let active_view = view_override
            .or(workbench.active_workspace_view())
            .or_else(|| {
                self.workspace_layout.lock().ok().and_then(|layout| {
                    layout
                        .focused_pane(crate::workspace_session_layout::WorkspaceWindow::Main)
                        .map(|pane| pane.0)
                })
            });
        let descriptor = active_view.and_then(|view| document.views.get(&view));
        let active_kind = descriptor.and_then(|descriptor| action_workspace_kind(&descriptor.kind));
        // The document decides which panes stay open; the projection only
        // repeats it. `active_kind` already moves whenever this fact can move,
        // so `ActionContextSignature` covers it without a field of its own.
        let active_view_pinned = descriptor.is_some_and(|descriptor| descriptor.kind.is_pinned());
        let target = descriptor.map(|descriptor| descriptor.target.clone());
        let has_project = session.project_snapshot().is_ok();
        let has_selection =
            workbench.active_sample_span().is_some() || !session.selection().selection.is_empty();
        let history = session.history_status().ok();
        let transport_playing = workbench
            .audio_controller
            .transport_session()
            .snapshot()
            .transport
            .mode
            == TransportMode::Playing;
        let modal_active = self
            .close_guard
            .lock()
            .map(|guard| !matches!(guard.state(), CloseGuardState::Idle))
            .unwrap_or(true);
        let signature = ActionContextSignature {
            document_generation: session.document_generation(),
            project_generation: session.snapshot().generation,
            selection_revision: session.selection().revision,
            workspace_revision: self.workspace.read(cx).authority_revision(),
            has_project,
            has_selection,
            active_view,
            active_kind,
            target: target.clone(),
            modal_active,
            can_undo: history.as_ref().is_some_and(|history| history.can_undo),
            can_redo: history.as_ref().is_some_and(|history| history.can_redo),
            loop_enabled: workbench.loop_enabled,
            transport_playing,
        };
        let context = ActionContext {
            epoch: self.action_context_epoch,
            has_project,
            has_selection,
            active_view,
            active_kind,
            active_view_pinned,
            target,
            text_input_focused: false,
            modal_active,
            can_undo: signature.can_undo,
            can_redo: signature.can_redo,
            loop_enabled: signature.loop_enabled,
            transport_playing,
        };
        (signature, context)
    }

    pub(super) fn refresh_action_projection(&mut self, cx: &mut Context<Self>) {
        let (signature, mut context) = self.action_context_material(None, cx);
        if self.action_context_signature.as_ref() != Some(&signature) {
            self.action_context_epoch.0 = self.action_context_epoch.0.wrapping_add(1).max(1);
            self.action_context_signature = Some(signature);
        }
        context.epoch = self.action_context_epoch;
        self.action_projection = self.action_registry.project(&context, &self.action_keymap);
        if self.native_menu_epoch != Some(self.action_projection.epoch) {
            cx.set_menus(projected_app_menus(&self.action_projection));
            self.native_menu_epoch = Some(self.action_projection.epoch);
        }
    }

    pub(super) fn projection_for_view(
        &self,
        view: WorkspaceViewId,
        cx: &App,
    ) -> ActionProjectionSnapshot {
        let (_, mut context) = self.action_context_material(Some(view), cx);
        context.epoch = self.action_context_epoch;
        self.action_registry.project(&context, &self.action_keymap)
    }

    pub(super) fn open_command_palette(&mut self, cx: &mut Context<Self>) {
        self.refresh_action_projection(cx);
        self.command_palette = CommandPaletteState {
            open: true,
            query: String::new(),
            selected: 0,
            snapshot: self.action_projection.clone(),
        };
        self.pane_context_menu = None;
        cx.notify();
    }

    pub(super) fn open_pane_context_menu(
        &mut self,
        view: WorkspaceViewId,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.refresh_action_projection(cx);
        let snapshot = self.projection_for_view(view, cx);
        self.pane_context_menu = Some(PaneContextMenuState {
            view,
            position,
            snapshot,
        });
        self.command_palette.open = false;
        cx.notify();
    }

    pub(super) fn handle_pending_pane_context_menus(&mut self, cx: &mut Context<Self>) {
        let pending = self
            .pending_pane_context_menus
            .borrow_mut()
            .drain(..)
            .collect::<Vec<_>>();
        if let Some((view, position)) = pending.into_iter().last() {
            self.open_pane_context_menu(view, position, cx);
        }
    }

    pub(super) fn action_failure(&self, message: impl Into<String>, cx: &mut Context<Self>) {
        let message = message.into();
        self.workbench.update(cx, |workbench, cx| {
            workbench.constructive_status = Some(message);
            cx.notify();
        });
    }

    pub(super) fn invoke_action_id(
        &mut self,
        action: ActionId,
        origin: InvocationOrigin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.invoke_action_with_parameters(action, origin, ActionParameters::default(), window, cx);
    }

    /// The same dispatch with the parameters an id declares. A surface that
    /// has nothing to say passes `ActionParameters::default()`; the socket's
    /// `action` verb passes what the caller named, and an undeclared name is
    /// refused here rather than dropped on the floor downstream.
    pub(super) fn invoke_action_with_parameters(
        &mut self,
        action: ActionId,
        origin: InvocationOrigin,
        parameters: ActionParameters,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.refresh_action_projection(cx);
        match self.action_projection.request(
            action,
            origin,
            InvocationModifiers::default(),
            parameters,
        ) {
            Ok(request) => self.dispatch_action_request(request, window, cx),
            Err(error) => self.action_failure(error.to_string(), cx),
        }
    }

    pub(super) fn dispatch_action_request(
        &mut self,
        request: ActionRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.refresh_action_projection(cx);
        let view = request.invocation.view;
        let (_, mut current) = self.action_context_material(view, cx);
        current.epoch = self.action_context_epoch;
        let invocation = match self.action_registry.validate_request(&request, &current) {
            Ok(invocation) => invocation,
            Err(error) => {
                self.action_failure(format!("Action refused · {error}"), cx);
                return;
            }
        };
        let action = invocation.action;
        let accepted = crate::ui_actions::action_parameter_names(action);
        if let Some((name, _)) = request
            .parameters
            .iter()
            .find(|(name, _)| !accepted.contains(name))
        {
            self.action_failure(
                if accepted.is_empty() {
                    format!(
                        "{} takes no parameters · `{name}` was named",
                        action.as_str()
                    )
                } else {
                    format!(
                        "{} takes {} · `{name}` was named",
                        action.as_str(),
                        accepted.join(", ")
                    )
                },
                cx,
            );
            return;
        }
        if let Some(intent) = ProductActionIntent::from_action(action) {
            self.dispatch_product_action(intent, view, &request.parameters, window, cx);
            self.pane_context_menu = None;
            return;
        }
        match action {
            surface_ids::VIEW_ZOOM_IN => self.workbench.update(cx, |workbench, cx| {
                workbench.zoom_timeline(workbench.playhead_sample(), 0.5, cx)
            }),
            surface_ids::VIEW_ZOOM_OUT => self.workbench.update(cx, |workbench, cx| {
                workbench.zoom_timeline(workbench.playhead_sample(), 2.0, cx)
            }),
            surface_ids::VIEW_PAN_LEFT => self
                .workbench
                .update(cx, |workbench, cx| workbench.pan_timeline(-0.2, cx)),
            surface_ids::VIEW_PAN_RIGHT => self
                .workbench
                .update(cx, |workbench, cx| workbench.pan_timeline(0.2, cx)),
            surface_ids::VIEW_FIT => self
                .workbench
                .update(cx, |workbench, cx| workbench.fit_timeline(cx)),
            surface_ids::VIEW_FOLLOW => self
                .workbench
                .update(cx, |workbench, cx| workbench.follow_timeline(cx)),
            // Workspace verbs are catalog ids now, so they were already lowered
            // through the typed product intent above.
            _ => self.action_failure(
                format!("Action {} has no application adapter", action.as_str()),
                cx,
            ),
        }
        self.pane_context_menu = None;
    }

    /// Lower the stable action vocabulary through one exhaustive typed seam.
    /// Menu, palette, shortcut, context-menu, and accessibility requests all
    /// arrive here after the same projection/epoch validation, so a capability
    /// cannot exist on one surface while silently falling through on another.
    pub(super) fn dispatch_product_action(
        &mut self,
        intent: ProductActionIntent,
        view: Option<WorkspaceViewId>,
        parameters: &ActionParameters,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match intent {
            ProductActionIntent::File(intent) => match intent {
                FileActionIntent::NewProject => self.request_project_replacement(
                    ProjectReplacementIntent::NewProject,
                    window,
                    cx,
                ),
                FileActionIntent::OpenProject => self.request_project_replacement(
                    ProjectReplacementIntent::ChooseProject,
                    window,
                    cx,
                ),
                FileActionIntent::OpenAudio => self.request_project_replacement(
                    ProjectReplacementIntent::ChooseAudio,
                    window,
                    cx,
                ),
                FileActionIntent::Save => self.save(false, None, cx),
                FileActionIntent::SaveAs => self.save(true, None, cx),
                FileActionIntent::OpenRecovery => self.request_project_replacement(
                    ProjectReplacementIntent::ChooseRecovery,
                    window,
                    cx,
                ),
                FileActionIntent::ExportAudio => self
                    .workbench
                    .update(cx, |workbench, cx| workbench.export_wav(cx)),
                FileActionIntent::Quit => self.request_application_close(window, cx),
            },
            ProductActionIntent::Edit(intent) => match intent {
                EditActionIntent::Undo | EditActionIntent::Redo => {
                    let session = self.workbench.read(cx).session.clone();
                    let result = session.update(cx, |session, _| match intent {
                        EditActionIntent::Undo => session.undo(),
                        EditActionIntent::Redo => session.redo(),
                        _ => unreachable!("matched undo/redo above"),
                    });
                    if let Err(error) = result {
                        self.action_failure(
                            format!(
                                "{} unavailable · {error}",
                                if matches!(intent, EditActionIntent::Undo) {
                                    "Undo"
                                } else {
                                    "Redo"
                                }
                            ),
                            cx,
                        );
                    }
                }
                EditActionIntent::Delete
                | EditActionIntent::Duplicate
                | EditActionIntent::SplitClip
                | EditActionIntent::FocusedEditor(_) => {
                    let action = match intent {
                        EditActionIntent::Delete => action_ids::EDIT_DELETE,
                        EditActionIntent::Duplicate => action_ids::EDIT_DUPLICATE,
                        EditActionIntent::SplitClip => action_ids::CLIP_SPLIT,
                        EditActionIntent::FocusedEditor(action) => action,
                        _ => unreachable!("matched focused edit above"),
                    };
                    if !self.dispatch_focused_editor_action(action, view, window, cx) {
                        self.action_failure("The focused editor cannot perform that edit", cx);
                    }
                }
            },
            ProductActionIntent::Transport(intent) => match intent {
                TransportActionIntent::TogglePlayback => self
                    .workbench
                    .update(cx, |workbench, cx| workbench.toggle_playback(cx)),
                TransportActionIntent::Stop => self.workbench.update(cx, |workbench, cx| {
                    workbench.dispatch_timeline_event(TimelineInteractionEvent::StopRequested, cx)
                }),
                TransportActionIntent::DecreaseTempo => self
                    .workbench
                    .update(cx, |workbench, cx| workbench.adjust_project_tempo(-1.0, cx)),
                TransportActionIntent::IncreaseTempo => self
                    .workbench
                    .update(cx, |workbench, cx| workbench.adjust_project_tempo(1.0, cx)),
                TransportActionIntent::MarkTempoAtPlayhead => self
                    .workbench
                    .update(cx, |workbench, cx| workbench.mark_tempo_at_playhead(cx)),
                TransportActionIntent::CycleMeterAtPlayhead => self
                    .workbench
                    .update(cx, |workbench, cx| workbench.cycle_meter_at_playhead(cx)),
                TransportActionIntent::AuditionDiff => self
                    .workbench
                    .update(cx, |workbench, cx| workbench.audition_diff(cx)),
                TransportActionIntent::ToggleLoop => self
                    .workbench
                    .update(cx, |workbench, cx| workbench.toggle_loop(cx)),
                TransportActionIntent::LoopFromSelection => self
                    .workbench
                    .update(cx, |workbench, cx| workbench.set_loop_from_selection(cx)),
                TransportActionIntent::ClearLoop => self.workbench.update(cx, |workbench, cx| {
                    workbench.dispatch_timeline_event(TimelineInteractionEvent::ClearLoop, cx)
                }),
            },
            ProductActionIntent::Pattern(intent) => match intent {
                crate::ui_actions::PatternPaneIntent::AuditionCycle => {
                    self.audition_pattern_cycle(view, cx)
                }
            },
            ProductActionIntent::Mixer(intent) => match intent {
                crate::ui_actions::MixerPaneIntent::InsertFilterOnMaster => {
                    self.insert_effect_on_master(crate::mixer::NativeEffectKind::Filter, cx)
                }
                crate::ui_actions::MixerPaneIntent::InsertCompressorOnMaster => {
                    self.insert_effect_on_master(crate::mixer::NativeEffectKind::Compressor, cx)
                }
                crate::ui_actions::MixerPaneIntent::MoveLastInsertUpOnMaster => {
                    self.move_last_insert_up_on_master(cx)
                }
                crate::ui_actions::MixerPaneIntent::RouteSelectedChannel => {
                    self.route_selected_channel(cx)
                }
            },
            ProductActionIntent::Sample(intent) => {
                self.workbench.update(cx, |workbench, cx| match intent {
                    SampleActionIntent::MakeSample => workbench.make_sample_from_active_span(cx),
                    SampleActionIntent::SliceToKit => workbench.slice_active_span_to_kit(cx),
                    SampleActionIntent::MakeBeat => workbench.make_beat_from_active_span(cx),
                    SampleActionIntent::ReverseSelectedZone => workbench.reverse_selected_zone(cx),
                })
            }
            ProductActionIntent::OpenPane(intent) => match intent {
                PaneOpenIntent::Arrangement => self.activate_or_create_dynamic(
                    default_view(WorkspaceKind::Arrangement, WorkspaceTarget::Arrangement),
                    cx,
                ),
                PaneOpenIntent::PianoRoll | PaneOpenIntent::Drums => {
                    let mode = if matches!(intent, PaneOpenIntent::PianoRoll) {
                        WorkspacePatternMode::PianoRoll
                    } else {
                        WorkspacePatternMode::Steps
                    };
                    // A pattern editor addresses one pattern. Starting a song
                    // by writing notes is the reason to open one, so an empty
                    // project gets the pattern the editor's own "+ NEW" would
                    // make instead of a refusal telling the musician to go
                    // find some other way in.
                    let mut pattern = self.workbench.read(cx).first_pattern_id(cx);
                    if pattern == 0 {
                        match self.create_default_pattern(mode, cx) {
                            Ok(created) => pattern = created,
                            Err(error) => {
                                self.action_failure(
                                    format!("No pattern to edit yet · {error}"),
                                    cx,
                                );
                                return;
                            }
                        }
                    }
                    self.show_pattern_editor(mode, pattern, cx);
                }
                PaneOpenIntent::Automation => {
                    // 0 when the project has no lane yet: the editor opens
                    // empty and creates the first lane itself.
                    let lane = self.workbench.read(cx).first_automation_lane_id(cx);
                    self.activate_or_create_dynamic(
                        default_view(
                            WorkspaceKind::AutomationEditor,
                            WorkspaceTarget::AutomationLane { id: lane },
                        ),
                        cx,
                    );
                }
                PaneOpenIntent::Mixer => self.activate_or_create_dynamic(
                    default_view(
                        WorkspaceKind::Mixer,
                        WorkspaceTarget::Mixer { bus_id: None },
                    ),
                    cx,
                ),
                PaneOpenIntent::Assets => self.activate_or_create_dynamic(
                    default_view(WorkspaceKind::Browser, WorkspaceTarget::Assets),
                    cx,
                ),
                PaneOpenIntent::Sampler => self.activate_or_create_dynamic(
                    default_view(
                        WorkspaceKind::Extension {
                            namespace: "audec".into(),
                            name: "sampler".into(),
                        },
                        WorkspaceTarget::Extension {
                            namespace: "audec".into(),
                            key: "active-kit".into(),
                        },
                    ),
                    cx,
                ),
                PaneOpenIntent::ReadingQuery => self.create_reading_query(cx),
            },
            ProductActionIntent::OpenLens(lens) => self.show_analysis_lens(lens, cx),
            ProductActionIntent::Workspace(intent) => {
                // `view` is the pane the invocation was projected against.
                // Activate is the one verb a caller can aim somewhere else by
                // name, so it reads the parameter the catalog declares for it.
                let view = match parameters.get("view") {
                    None => view,
                    Some(ActionParameterValue::Unsigned(named)) => Some(WorkspaceViewId(*named)),
                    Some(other) => {
                        self.action_failure(
                            format!("`view` must be a pane number; got {other:?}"),
                            cx,
                        );
                        return;
                    }
                };
                let (node, action) = match intent {
                    WorkspaceActionIntent::NextPane => (
                        WorkspaceSemanticNodeId::Workspace,
                        WorkspaceSemanticAction::NextPane,
                    ),
                    WorkspaceActionIntent::PreviousPane => (
                        WorkspaceSemanticNodeId::Workspace,
                        WorkspaceSemanticAction::PreviousPane,
                    ),
                    WorkspaceActionIntent::Focus
                    | WorkspaceActionIntent::Activate
                    | WorkspaceActionIntent::Reopen
                    | WorkspaceActionIntent::Close
                    | WorkspaceActionIntent::FloatOrDock
                    | WorkspaceActionIntent::NextTab
                    | WorkspaceActionIntent::PreviousTab => {
                        let Some(view) =
                            view.or_else(|| self.workbench.read(cx).active_workspace_view())
                        else {
                            self.action_failure(
                                "Workspace action unavailable · no target pane",
                                cx,
                            );
                            return;
                        };
                        let node = if matches!(intent, WorkspaceActionIntent::Reopen) {
                            WorkspaceSemanticNodeId::HiddenTab(view)
                        } else {
                            WorkspaceSemanticNodeId::Tab(view)
                        };
                        let action = match intent {
                            WorkspaceActionIntent::Focus => WorkspaceSemanticAction::Focus,
                            WorkspaceActionIntent::Activate => WorkspaceSemanticAction::Activate,
                            WorkspaceActionIntent::Reopen => WorkspaceSemanticAction::Reopen,
                            WorkspaceActionIntent::Close => WorkspaceSemanticAction::Close,
                            WorkspaceActionIntent::FloatOrDock => {
                                WorkspaceSemanticAction::FloatOrDock
                            }
                            WorkspaceActionIntent::NextTab => WorkspaceSemanticAction::NextTab,
                            WorkspaceActionIntent::PreviousTab => {
                                WorkspaceSemanticAction::PreviousTab
                            }
                            _ => unreachable!("matched target-pane workspace action above"),
                        };
                        (node, action)
                    }
                };
                self.execute_workspace_semantic(node, action, cx);
            }
            ProductActionIntent::OpenPalette => self.open_command_palette(cx),
        }
    }

    /// Show the lens an `audec.lens.*` id names. The workspace already holds
    /// one pane per lens from the bootstrap layout, so the verb activates the
    /// pane that is the lens rather than stacking a second view on the same
    /// analysis; only a document that has lost it creates one.
    pub(super) fn show_analysis_lens(&mut self, lens: LensOpenIntent, cx: &mut Context<Self>) {
        let kind = match lens {
            LensOpenIntent::Waterfall => AnalysisLensKind::Waterfall,
            LensOpenIntent::Rhythm => AnalysisLensKind::Rhythm,
            LensOpenIntent::Components => AnalysisLensKind::Components,
            LensOpenIntent::Separation => AnalysisLensKind::Separation,
            LensOpenIntent::Loom => AnalysisLensKind::Loom,
        };
        let existing = self
            .workspace_document()
            .views
            .values()
            .filter(|descriptor| descriptor.kind == WorkspaceKind::AnalysisLens { lens: kind })
            .map(|descriptor| descriptor.id)
            .min();
        let Some(view) = existing else {
            // Creating the pane runs its own first analysis; nothing to kick.
            self.create_dynamic(analysis_view(kind), cx);
            return;
        };
        if let Err(error) = self
            .workspace
            .update(cx, |workspace, cx| workspace.activate_or_show(view, cx))
        {
            self.action_failure(format!("{kind:?} lens could not be shown · {error}"), cx);
            return;
        }
        // Naming a lens is asking to see its analysis. Focusing a pane the
        // workspace already holds does not by itself make an idle lens read
        // the song — the pane-creation path is what used to start the work —
        // so the same first refresh runs here, and only when the lens has
        // nothing to show. The waterfall and components fields are the
        // Workbench's, not the pane's, so there is nothing to start for them.
        let Some(lens) = self.analysis_lens(view, cx) else {
            return;
        };
        lens.update(cx, |lens, cx| match lens.kind {
            VizKind::Rhythm if matches!(lens.rhythm_state, RhythmViewState::Idle) => {
                lens.refresh_rhythm(cx)
            }
            VizKind::Separation if matches!(lens.hpss_state, HpssViewState::Idle) => {
                lens.refresh_hpss(cx)
            }
            VizKind::Loom if matches!(lens.loom_state, LoomViewState::Idle) => {
                lens.refresh_loom(cx)
            }
            VizKind::Waterfall
            | VizKind::Components
            | VizKind::Rhythm
            | VizKind::Separation
            | VizKind::Loom => {}
        });
    }

    /// Show the piano roll or the step sequencer for one pattern. They are two
    /// editors of the same music, not one editor in two modes, so asking for
    /// the drums activates a step grid if one is open on this pattern and
    /// otherwise opens one — it never converts the musician's open piano roll
    /// into a step grid behind their back. (`activate_or_create_dynamic`
    /// would: the document's reuse rule matches any `PatternEditor` on the
    /// same target, so it rewrites the descriptor's mode. Beyond being a lie
    /// about what the musician asked for, rewriting a pane's kind while
    /// another pane holds the focus wedges the dynamic workspace in a
    /// `handle_group_event` / `execute_layout_command` oscillation — see the
    /// lane report.)
    pub(super) fn show_pattern_editor(
        &mut self,
        mode: WorkspacePatternMode,
        pattern: u64,
        cx: &mut Context<Self>,
    ) {
        let descriptor = default_view(
            WorkspaceKind::PatternEditor { mode },
            WorkspaceTarget::PatternDefinition { id: pattern },
        );
        let existing = self
            .workspace_document()
            .views
            .values()
            .filter(|view| view.kind == descriptor.kind && view.target == descriptor.target)
            .map(|view| view.id)
            .min();
        let Some(view) = existing else {
            self.create_dynamic(descriptor, cx);
            return;
        };
        if let Err(error) = self
            .workspace
            .update(cx, |workspace, cx| workspace.activate_or_show(view, cx))
        {
            self.action_failure(format!("Pattern editor could not be shown · {error}"), cx);
        }
    }

    /// The pattern the sequencer's "+ NEW" builds, made without the sequencer
    /// being open: four bars of the meter in force at the start of the song,
    /// sixteenth-note steps. Returns the pattern id the editor should address.
    pub(super) fn create_default_pattern(
        &mut self,
        mode: WorkspacePatternMode,
        cx: &mut Context<Self>,
    ) -> Result<u64, String> {
        let mode = match mode {
            WorkspacePatternMode::PianoRoll => PatternEditorMode::PianoRoll,
            WorkspacePatternMode::Steps => PatternEditorMode::Steps,
        };
        let session = self.workbench.read(cx).session.clone();
        let (revision, bar_ticks) = {
            let snapshot = session
                .read(cx)
                .project_snapshot()
                .map_err(|error| error.to_string())?;
            let ticks = snapshot
                .project
                .state()
                .domains
                .sequencer
                .tempo_map()
                .meter_at(crate::sequencer::BeatTime::ZERO)
                .ticks_per_bar();
            (snapshot.revisions().aggregate, ticks)
        };
        let intent = PatternWorkflowIntent::Action(PatternActionIntent {
            expected_project_revision: revision,
            action: PatternAction::Create(CreatePatternIntent {
                mode,
                name: match mode {
                    PatternEditorMode::PianoRoll => "New note pattern".into(),
                    PatternEditorMode::Steps => "New step pattern".into(),
                },
                length: BeatDuration((bar_ticks * 4).max(1) as u64),
                step_resolution: BeatDuration((crate::sequencer::PPQ / 4) as u64),
                initial_target: None,
            }),
        });
        let outcome = session
            .update(cx, |session, _| session.execute_pattern_workflow(intent))
            .map_err(|error| error.to_string())?;
        match outcome {
            PatternWorkflowOutcome::Published { publication, .. } => {
                let id = publication.pattern.get();
                self.workbench.update(cx, |workbench, cx| {
                    workbench.constructive_status = Some(format!(
                        "New 4-bar pattern created at revision {}",
                        publication.revision
                    ));
                    cx.notify();
                });
                Ok(id)
            }
            other => Err(format!(
                "pattern creation answered {other:?} instead of a publication"
            )),
        }
    }

    /// The pattern editor's AUDITION, asked for by name. The editor owns the
    /// request (it knows the placed occurrence and the preview cycle), so this
    /// sends it the action its own button sends rather than rebuilding the
    /// audition here.
    pub(super) fn audition_pattern_cycle(
        &mut self,
        view: Option<WorkspaceViewId>,
        cx: &mut Context<Self>,
    ) {
        let view = view.or_else(|| self.workbench.read(cx).active_workspace_view());
        let editor = view
            .and_then(|view| self.workbench.read(cx).workspace_panes.get(&view).cloned())
            .and_then(|runtime| match runtime {
                WorkspacePaneRuntime::Hosted(host) => host.upgrade(),
                _ => None,
            })
            .and_then(|host| match &host.read(cx).content {
                WorkspacePaneContent::Pattern(editor) => Some(editor.clone()),
                _ => None,
            });
        let Some(editor) = editor else {
            self.action_failure(
                "Pattern audition unavailable · no pattern editor is the active pane",
                cx,
            );
            return;
        };
        // Not `focus_handle.dispatch_action`: that resolves against the last
        // *rendered* frame's dispatch tree, so a pane opened a moment ago
        // swallows the verb in silence. The editor entity is in hand; ask it.
        editor.update(cx, |editor, cx| editor.audition_cycle(cx));
        // The editor answers in its own status, which lives in its pane. An
        // action invoked by name was invoked from somewhere else, so its answer
        // is repeated in the notice channel the caller can read.
        let answer = editor.read(cx).status().map(str::to_owned);
        self.workbench.update(cx, |workbench, cx| {
            workbench.constructive_status = answer.or_else(|| {
                Some("Pattern audition was not acknowledged by the editor".into())
            });
            cx.notify();
        });
    }

    pub(super) fn dispatch_focused_editor_action(
        &self,
        action: ActionId,
        view: Option<WorkspaceViewId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(view) = view else {
            return false;
        };
        let runtime = self.workbench.read(cx).workspace_panes.get(&view).cloned();
        let Some(WorkspacePaneRuntime::Hosted(host)) = runtime else {
            return false;
        };
        let Some(host) = host.upgrade() else {
            return false;
        };
        match &host.read(cx).content {
            WorkspacePaneContent::Arrangement(editor) => {
                // Not `FocusHandle::dispatch_action`: that resolves the node in
                // the most recently *rendered* frame, so a pane opened by the
                // same script that then asks for an edit silently does nothing
                // (measured on the desktop: `audec.clip.split` answered
                // `dispatched` and split no clip). The pane's entity is the
                // authority whether or not a frame has been painted, and the
                // keyboard actions call the same verb.
                let verb = match action {
                    action_ids::EDIT_DELETE => ArrangementVerb::Delete,
                    action_ids::EDIT_DUPLICATE => ArrangementVerb::Duplicate,
                    action_ids::CLIP_SPLIT => ArrangementVerb::Split,
                    action_ids::CLIP_SELECT_ALL => ArrangementVerb::SelectAll,
                    action_ids::CLIP_GAIN_DOWN => ArrangementVerb::GainDown,
                    action_ids::CLIP_GAIN_UP => ArrangementVerb::GainUp,
                    action_ids::CLIP_TOGGLE_MUTE => ArrangementVerb::ToggleMute,
                    action_ids::CLIP_RENAME => ArrangementVerb::Rename,
                    action_ids::CLIP_FADE_IN => ArrangementVerb::FadeIn,
                    action_ids::CLIP_FADE_OUT => ArrangementVerb::FadeOut,
                    action_ids::CLIP_CLEAR_FADES => ArrangementVerb::ClearFades,
                    action_ids::CLIP_CROSSFADE => ArrangementVerb::Crossfade,
                    action_ids::CLIP_REPEAT => ArrangementVerb::Repeat,
                    action_ids::CLIP_STRETCH => ArrangementVerb::Stretch,
                    action_ids::CLIP_PLACE_SELECTED_ASSET => {
                        ArrangementVerb::PlaceSelectedAssetAtPlayhead
                    }
                    action_ids::MARKER_PUT_AT_PLAYHEAD => ArrangementVerb::PutMarkerAtPlayhead,
                    _ => return false,
                };
                let editor = editor.clone();
                cx.defer(move |cx| {
                    editor.update(cx, |editor, cx| editor.perform(verb, cx));
                });
                true
            }
            WorkspacePaneContent::Pattern(editor) => {
                let focus = editor.focus_handle(cx);
                match action {
                    action_ids::EDIT_DELETE => {
                        focus.dispatch_action(&crate::sequencer_view::EditorDelete, window, cx)
                    }
                    action_ids::EDIT_DUPLICATE => {
                        focus.dispatch_action(&crate::sequencer_view::EditorDuplicate, window, cx)
                    }
                    _ => return false,
                }
                true
            }
            _ => false,
        }
    }

    pub(super) fn on_action_surface_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pane_context_menu.is_some() {
            if event.keystroke.key == "escape" {
                self.pane_context_menu = None;
                cx.notify();
                cx.stop_propagation();
            }
            return;
        }
        if !self.command_palette.open {
            return;
        }
        let keystroke = &event.keystroke;
        let items = self
            .command_palette
            .snapshot
            .palette(&self.command_palette.query);
        match keystroke.key.as_str() {
            "escape" => self.command_palette.open = false,
            "up" => self.command_palette.selected = self.command_palette.selected.saturating_sub(1),
            "down" => {
                if !items.is_empty() {
                    self.command_palette.selected =
                        (self.command_palette.selected + 1).min(items.len() - 1);
                }
            }
            "enter" => {
                if let Some(item) = items.get(self.command_palette.selected) {
                    let action = item.action;
                    let request = self.command_palette.snapshot.request(
                        action,
                        InvocationOrigin::Palette,
                        InvocationModifiers::default(),
                        ActionParameters::default(),
                    );
                    self.command_palette.open = false;
                    match request {
                        Ok(request) => self.dispatch_action_request(request, window, cx),
                        Err(error) => self.action_failure(error.to_string(), cx),
                    }
                }
            }
            "backspace" if !keystroke.modifiers.platform && !keystroke.modifiers.control => {
                self.command_palette.query.pop();
                self.command_palette.selected = 0;
            }
            _ if !keystroke.modifiers.platform && !keystroke.modifiers.control => {
                if let Some(text) = keystroke
                    .key_char
                    .as_deref()
                    .filter(|text| !text.is_empty() && !keystroke.modifiers.alt)
                {
                    self.command_palette.query.push_str(text);
                    self.command_palette.selected = 0;
                }
            }
            _ => return,
        }
        cx.notify();
        cx.stop_propagation();
    }

    pub(super) fn choose_palette_action(
        &mut self,
        action: ActionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = self.command_palette.snapshot.request(
            action,
            InvocationOrigin::Palette,
            InvocationModifiers::default(),
            ActionParameters::default(),
        );
        self.command_palette.open = false;
        match request {
            Ok(request) => self.dispatch_action_request(request, window, cx),
            Err(error) => self.action_failure(error.to_string(), cx),
        }
    }

    pub(super) fn render_command_palette(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if !self.command_palette.open {
            return div().into_any_element();
        }
        let items = self
            .command_palette
            .snapshot
            .palette(&self.command_palette.query);
        let query = if self.command_palette.query.is_empty() {
            "Type a command…".to_owned()
        } else {
            format!("{}▏", self.command_palette.query)
        };
        let rows = items.into_iter().enumerate().map(|(index, item)| {
            let selected = index == self.command_palette.selected;
            let action = item.action;
            let shortcut = item.shortcuts.first().cloned();
            let reason = item.disabled_reason;
            let enabled = item.enabled;
            div()
                .id(SharedString::from(format!(
                    "action-palette:{}",
                    action.as_str()
                )))
                .px_3()
                .py_2()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .rounded_sm()
                .bg(rgb(if selected { BORDER } else { PANEL }))
                .text_color(rgb(if enabled { TEXT } else { DIM }))
                // A row that names its own refusal must not accept the click
                // that would only restate it.
                .when(enabled, |row| {
                    row.cursor_pointer()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.choose_palette_action(action, window, cx)
                        }))
                })
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .child(format!(
                            "{}{}",
                            if item.checked { "✓ " } else { "" },
                            item.label
                        ))
                        .when_some(reason, |column, reason| {
                            column.child(div().text_xs().text_color(rgb(DIM)).child(reason))
                        }),
                )
                .when_some(shortcut, |row, shortcut| {
                    row.child(div().text_xs().text_color(rgb(MUTED)).child(shortcut))
                })
        });
        div()
            .id("action-palette-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .justify_center()
            .bg(rgba(0x00000099))
            .on_click(cx.listener(|this, _, _, cx| {
                this.command_palette.open = false;
                cx.notify();
            }))
            .child(
                div()
                    .id("action-palette-panel")
                    .mt(px(72.0))
                    .w(px(580.0))
                    .max_h(px(620.0))
                    .flex()
                    .flex_col()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL))
                    .shadow_lg()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex_none()
                            .px_4()
                            .py_3()
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .text_color(rgb(CYAN))
                            .child(query),
                    )
                    .child(
                        div()
                            .id("action-palette-results")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.command_palette_scroll)
                            .p_2()
                            .children(rows),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn choose_context_action(
        &mut self,
        action: ActionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.pane_context_menu.clone() else {
            return;
        };
        let mut parameters = ActionParameters::default();
        parameters.insert("view_id", ActionParameterValue::Unsigned(menu.view.0));
        let request = menu.snapshot.request(
            action,
            InvocationOrigin::ContextMenu,
            InvocationModifiers::default(),
            parameters,
        );
        self.pane_context_menu = None;
        match request {
            Ok(request) => self.dispatch_action_request(request, window, cx),
            Err(error) => self.action_failure(error.to_string(), cx),
        }
    }

    pub(super) fn render_pane_context_menu(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(menu) = self.pane_context_menu.as_ref() else {
            return div().into_any_element();
        };
        let items = menu
            .snapshot
            .context_menu(crate::ui_actions::PANE_CONTEXT_ACTIONS);
        let rows = items.into_iter().map(|item| {
            let action = item.action;
            let shortcut = item.shortcuts.first().cloned();
            let enabled = item.enabled;
            div()
                .id(SharedString::from(format!(
                    "pane-context:{}",
                    action.as_str()
                )))
                .px_3()
                .py_2()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .text_color(rgb(if enabled { TEXT } else { DIM }))
                // Disabled rows keep their reason and lose their affordance:
                // no pointer, no hover, no click that would only error.
                .when(enabled, |row| {
                    row.cursor_pointer()
                        .hover(|style| style.bg(rgb(BORDER)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.choose_context_action(action, window, cx)
                        }))
                })
                .child(div().flex().flex_col().child(item.label).when_some(
                    item.disabled_reason,
                    |column, reason| {
                        column.child(div().text_xs().text_color(rgb(DIM)).child(reason))
                    },
                ))
                .when_some(shortcut, |row, shortcut| {
                    row.child(div().text_xs().text_color(rgb(MUTED)).child(shortcut))
                })
        });
        let position = menu.position;
        div()
            .id("pane-context-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .on_click(cx.listener(|this, _, _, cx| {
                this.pane_context_menu = None;
                cx.notify();
            }))
            .child(
                div()
                    .id("pane-context-panel")
                    .absolute()
                    .left(position.x)
                    .top(position.y)
                    .w(px(260.0))
                    .p_1()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL))
                    .shadow_lg()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .children(rows),
            )
            .into_any_element()
    }
}
