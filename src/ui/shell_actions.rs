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
        let active_view = view_override.or(workbench.active_workspace_view());
        let descriptor = active_view.and_then(|view| document.views.get(&view));
        let active_kind = descriptor.and_then(|descriptor| action_workspace_kind(&descriptor.kind));
        let target = descriptor.map(action_editor_target);
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
        self.refresh_action_projection(cx);
        match self.action_projection.request(
            action,
            origin,
            InvocationModifiers::default(),
            ActionParameters::default(),
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
        if let Some(intent) = ProductActionIntent::from_action(action) {
            self.dispatch_product_action(intent, view, window, cx);
            self.pane_context_menu = None;
            return;
        }
        match action {
            surface_ids::ANALYSIS_WATERFALL => {
                self.create_dynamic(analysis_view(AnalysisLensKind::Waterfall), cx)
            }
            surface_ids::ANALYSIS_RHYTHM => {
                self.create_dynamic(analysis_view(AnalysisLensKind::Rhythm), cx)
            }
            surface_ids::ANALYSIS_COMPONENTS => {
                self.create_dynamic(analysis_view(AnalysisLensKind::Components), cx)
            }
            surface_ids::ANALYSIS_SEPARATION => {
                self.create_dynamic(analysis_view(AnalysisLensKind::Separation), cx)
            }
            surface_ids::ANALYSIS_LOOM => {
                self.create_dynamic(analysis_view(AnalysisLensKind::Loom), cx)
            }
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
            // The surface registers its own workspace ids (menus, keymap);
            // they are the same verbs as the product intents.
            surface_ids::WORKSPACE_FLOAT_DOCK => self.dispatch_product_action(
                ProductActionIntent::Workspace(WorkspaceActionIntent::FloatOrDock),
                view,
                window,
                cx,
            ),
            surface_ids::WORKSPACE_NEXT => self.dispatch_product_action(
                ProductActionIntent::Workspace(WorkspaceActionIntent::NextTab),
                view,
                window,
                cx,
            ),
            surface_ids::WORKSPACE_PREVIOUS => self.dispatch_product_action(
                ProductActionIntent::Workspace(WorkspaceActionIntent::PreviousTab),
                view,
                window,
                cx,
            ),
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
                | EditActionIntent::SplitClip => {
                    let action = match intent {
                        EditActionIntent::Delete => action_ids::EDIT_DELETE,
                        EditActionIntent::Duplicate => action_ids::EDIT_DUPLICATE,
                        EditActionIntent::SplitClip => action_ids::CLIP_SPLIT,
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
            ProductActionIntent::Sample(intent) => {
                self.workbench.update(cx, |workbench, cx| match intent {
                    SampleActionIntent::MakeSample => workbench.make_sample_from_active_span(cx),
                    SampleActionIntent::SliceToKit => workbench.slice_active_span_to_kit(cx),
                    SampleActionIntent::MakeBeat => workbench.make_beat_from_active_span(cx),
                })
            }
            ProductActionIntent::OpenPane(intent) => match intent {
                PaneOpenIntent::Arrangement => self.activate_or_create_dynamic(
                    default_view(WorkspaceKind::Arrangement, WorkspaceTarget::Arrangement),
                    cx,
                ),
                PaneOpenIntent::PianoRoll | PaneOpenIntent::Drums => {
                    let pattern = self.workbench.read(cx).first_pattern_id(cx);
                    if pattern == 0 {
                        // A pattern editor addresses one pattern; with none in
                        // the project the tool would fail identity validation
                        // and report a workspace error. Say what creates one.
                        self.action_failure(
                            "No pattern to edit yet · Make beat from a selection, or Place a pattern",
                            cx,
                        );
                        return;
                    }
                    let mode = if matches!(intent, PaneOpenIntent::PianoRoll) {
                        WorkspacePatternMode::PianoRoll
                    } else {
                        WorkspacePatternMode::Steps
                    };
                    self.activate_or_create_dynamic(
                        default_view(
                            WorkspaceKind::PatternEditor { mode },
                            WorkspaceTarget::PatternDefinition { id: pattern },
                        ),
                        cx,
                    );
                }
                PaneOpenIntent::Automation => {
                    let lane = self.workbench.read(cx).first_automation_lane_id(cx);
                    if lane == 0 {
                        self.action_failure(
                            "No automation lane yet · automate a mixer parameter to create one",
                            cx,
                        );
                        return;
                    }
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
            ProductActionIntent::Workspace(intent) => {
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
                let focus = editor.focus_handle(cx);
                match action {
                    action_ids::EDIT_DELETE => {
                        focus.dispatch_action(&crate::arrangement_view::DeleteClip, window, cx)
                    }
                    action_ids::EDIT_DUPLICATE => {
                        focus.dispatch_action(&crate::arrangement_view::DuplicateClip, window, cx)
                    }
                    action_ids::CLIP_SPLIT => {
                        focus.dispatch_action(&crate::arrangement_view::SplitClip, window, cx)
                    }
                    _ => return false,
                }
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
                .text_color(rgb(if item.enabled { TEXT } else { DIM }))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.choose_palette_action(action, window, cx)
                }))
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
        let items = menu.snapshot.context_menu(&[
            surface_ids::WORKSPACE_FLOAT_DOCK,
            surface_ids::WORKSPACE_CLOSE,
        ]);
        let rows = items.into_iter().map(|item| {
            let action = item.action;
            let shortcut = item.shortcuts.first().cloned();
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
                .text_color(rgb(if item.enabled { TEXT } else { DIM }))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(BORDER)))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.choose_context_action(action, window, cx)
                }))
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
