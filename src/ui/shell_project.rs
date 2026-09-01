//! DawWorkspace project replacement, close guard, save, and workspace import.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

impl DawWorkspace {
    pub(super) fn create_dynamic(&mut self, descriptor: NewWorkspaceView, cx: &mut Context<Self>) {
        if let Err(error) = self.workspace.update(cx, |workspace, cx| {
            workspace.create_view(descriptor, None, cx)
        }) {
            eprintln!("creating workspace item: {error:#}");
        }
    }

    pub(super) fn activate_or_create_dynamic(
        &mut self,
        descriptor: NewWorkspaceView,
        cx: &mut Context<Self>,
    ) {
        let document = self.workspace_document();
        let reusable = document.reusable_view_for(&descriptor);
        let replacement = reusable.and_then(|view| {
            let existing = document.views.get(&view)?;
            if existing.kind == descriptor.kind {
                return None;
            }
            let mut replacement = existing.clone();
            replacement.kind = descriptor.kind.clone();
            Some(replacement)
        });
        let result = self.workspace.update(cx, |workspace, cx| {
            if let Some(view) = reusable {
                if let Some(replacement) = replacement {
                    workspace.replace_view_descriptor(replacement, cx)?;
                }
                workspace.activate_or_show(view, cx)?;
                Ok(view)
            } else {
                workspace.create_view(descriptor, None, cx)
            }
        });
        if let Err(error) = result {
            self.action_failure(format!("Opening workspace tool failed · {error}"), cx);
        }
    }

    pub(super) fn request_project_replacement(
        &mut self,
        intent: ProjectReplacementIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workbench.read(cx).replacement_disposition(cx)
            != ProjectReplacementDisposition::Dirty
        {
            self.perform_project_replacement(intent, window, cx);
            return;
        }

        let Some(handle) = window.window_handle().downcast::<DawWorkspace>() else {
            self.workbench.update(cx, |workbench, cx| {
                workbench.project_io_status =
                    ProjectIoStatus::Failed("project window identity is unavailable".into());
                cx.notify();
            });
            return;
        };
        let prompt = window.prompt(
            PromptLevel::Warning,
            "Save changes before replacing this project?",
            Some(
                "New Project, Open Project, Open Audio, and Recovery replace the current session.",
            ),
            &[
                PromptButton::ok("Save"),
                PromptButton::new("Discard"),
                PromptButton::cancel("Cancel"),
            ],
            cx,
        );
        cx.spawn(async move |_this, cx| {
            let choice = prompt.await.unwrap_or(2);
            match choice {
                0 => {
                    let _ = handle.update(cx, |workspace, _window, cx| {
                        workspace.save(
                            false,
                            Some(PostSaveAction::Replace {
                                intent,
                                window: handle,
                            }),
                            cx,
                        );
                    });
                }
                1 => {
                    let _ = handle.update(cx, |workspace, window, cx| {
                        workspace.perform_project_replacement(intent, window, cx)
                    });
                }
                _ => {}
            }
        })
        .detach();
    }

    pub(super) fn perform_project_replacement(
        &mut self,
        intent: ProjectReplacementIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match intent {
            ProjectReplacementIntent::NewProject => self
                .workbench
                .update(cx, |workbench, cx| workbench.new_project(cx)),
            ProjectReplacementIntent::ChooseAudio => self
                .workbench
                .update(cx, |workbench, cx| workbench.choose_audio(cx)),
            ProjectReplacementIntent::ChooseProject => self
                .workbench
                .update(cx, |workbench, cx| workbench.choose_project(cx)),
            ProjectReplacementIntent::ChooseRecovery => self.choose_recovery(window, cx),
            ProjectReplacementIntent::OpenRecovery {
                package_root,
                checkpoint,
            } => self.workbench.update(cx, |workbench, cx| {
                workbench.open_project_package(package_root, Some(checkpoint), cx)
            }),
        }
    }

    pub(super) fn create_reading_query(&mut self, cx: &mut Context<Self>) {
        let id = NEXT_QUERY_DOCUMENT.fetch_add(1, Ordering::Relaxed).max(1);
        let document = QueryDocument::new(
            QueryDocumentId(id),
            format!("Reading query {id}"),
            QueryTermDto::Kind {
                kind: FactKindDto::Object,
            },
        );
        match WorkbenchPaneFactory::workspace_view(&document) {
            Ok(descriptor) => self.create_dynamic(descriptor, cx),
            Err(error) => self.workbench.update(cx, |workbench, cx| {
                workbench.constructive_status =
                    Some(format!("Reading query unavailable · {error}"));
                cx.notify();
            }),
        }
    }

    pub(super) fn execute_workspace_semantic(
        &mut self,
        node: WorkspaceSemanticNodeId,
        action: WorkspaceSemanticAction,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = self.workspace.update(cx, |workspace, cx| {
            workspace.execute_semantic_action(node, action, cx)
        }) {
            self.workbench.update(cx, |workbench, cx| {
                workbench.constructive_status =
                    Some(format!("Workspace action unavailable · {error}"));
                cx.notify();
            });
        }
    }

    pub(super) fn execute_active_workspace_semantic(
        &mut self,
        action: WorkspaceSemanticAction,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.workbench.read(cx).active_workspace_view() else {
            self.workbench.update(cx, |workbench, cx| {
                workbench.constructive_status =
                    Some("Workspace action unavailable · no active pane".into());
                cx.notify();
            });
            return;
        };
        self.execute_workspace_semantic(WorkspaceSemanticNodeId::Tab(view), action, cx);
    }

    pub(super) fn choose_recovery(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(package_root) = self.workbench.read(cx).package_root() else {
            self.workbench.update(cx, |workbench, cx| {
                workbench.project_io_status = ProjectIoStatus::Failed(
                    "open a project package before choosing recovery".into(),
                );
                cx.notify();
            });
            return;
        };
        let discovery = match ProjectPackage::new(package_root.clone()) {
            Ok(package) => ProjectStore::new(package).discover_recovery(),
            Err(error) => {
                self.workbench.update(cx, |workbench, cx| {
                    workbench.project_io_status = ProjectIoStatus::Failed(error.to_string());
                    cx.notify();
                });
                return;
            }
        };
        if discovery.checkpoints.is_empty() {
            let detail = discovery.diagnostics.first().map_or_else(
                || "no labeled autosave checkpoints were found".to_owned(),
                |diagnostic| format!("no usable checkpoints · {}", diagnostic.message),
            );
            self.workbench.update(cx, |workbench, cx| {
                workbench.project_io_status = ProjectIoStatus::Failed(detail);
                cx.notify();
            });
            return;
        }

        let checkpoints = discovery.checkpoints;
        let mut buttons = checkpoints
            .iter()
            .enumerate()
            .map(|(index, checkpoint)| {
                let file = checkpoint
                    .manifest_path
                    .file_name()
                    .and_then(|file| file.to_str())
                    .unwrap_or("autosave.json");
                let label = format!(
                    "Revision {} · saved {} · {}",
                    checkpoint.base_project_revision, checkpoint.saved_unix_ms, file
                );
                if index == 0 {
                    PromptButton::ok(label)
                } else {
                    PromptButton::new(label)
                }
            })
            .collect::<Vec<_>>();
        buttons.push(PromptButton::cancel("Cancel"));
        let prompt = window.prompt(
            PromptLevel::Warning,
            "Choose a recovery checkpoint",
            Some(
                "Recovery never replaces the current project until you choose a labeled revision.",
            ),
            &buttons,
            cx,
        );
        cx.spawn(async move |this, cx| {
            let Ok(choice) = prompt.await else {
                return;
            };
            let Some(checkpoint) = checkpoints.get(choice).cloned() else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.workbench.update(cx, |workbench, cx| {
                    workbench.open_project_package(package_root, Some(checkpoint), cx)
                });
            });
        })
        .detach();
    }

    pub(super) fn enter_close_modal(&self, request: CloseRequestId) {
        let document = self.workspace_document();
        if let Ok(mut input) = self.product_input.lock() {
            let _ = input.replace_snapshot(workspace_input_snapshot(&document, Some(request)));
            let _ = input.enter_modal(
                request,
                FocusTarget::ClosePrompt {
                    request,
                    choice: CloseChoice::Cancel,
                },
            );
        }
    }

    pub(super) fn leave_close_modal(&self) {
        let document = self.workspace_document();
        if let Ok(mut input) = self.product_input.lock() {
            let _ = input.leave_modal();
            let _ = input.replace_snapshot(workspace_input_snapshot(&document, None));
        }
    }

    pub(super) fn request_application_close(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dirty = self.workbench.read(cx).is_project_dirty(cx);
        let effect = self
            .close_guard
            .lock()
            .map(|mut guard| guard.request(CloseScope::Application, dirty))
            .unwrap_or(CloseGuardEffect::KeepOpen);
        match effect {
            CloseGuardEffect::CloseNow(CloseScope::Application) => cx.quit(),
            CloseGuardEffect::OpenPrompt { request, .. } => {
                self.enter_close_modal(request);
                let prompt = window.prompt(
                    PromptLevel::Warning,
                    "Save changes before quitting?",
                    Some("The project has edits newer than its last durable checkpoint."),
                    &[
                        PromptButton::ok("Save"),
                        PromptButton::new("Discard"),
                        PromptButton::cancel("Cancel"),
                    ],
                    cx,
                );
                cx.spawn(async move |this, cx| {
                    let choice = match prompt.await.unwrap_or(2) {
                        0 => CloseChoice::Save,
                        1 => CloseChoice::Discard,
                        _ => CloseChoice::Cancel,
                    };
                    let _ = this.update(cx, |this, cx| {
                        this.leave_close_modal();
                        let effect = this
                            .close_guard
                            .lock()
                            .map(|mut guard| guard.choose(request, choice))
                            .unwrap_or(CloseGuardEffect::KeepOpen);
                        match effect {
                            CloseGuardEffect::SaveProject { request } => {
                                this.save(false, Some(PostSaveAction::Quit), cx);
                                // Workbench owns the asynchronous save and
                                // quits only on success. Release the modal
                                // guard now so a cancelled Save As or failed
                                // write cannot permanently swallow Cmd-Q.
                                if let Ok(mut guard) = this.close_guard.lock() {
                                    let _ = guard.save_finished(request, false);
                                }
                            }
                            CloseGuardEffect::CloseNow(CloseScope::Application) => cx.quit(),
                            CloseGuardEffect::KeepOpen
                            | CloseGuardEffect::OpenPrompt { .. }
                            | CloseGuardEffect::CloseNow(_) => {}
                        }
                    });
                })
                .detach();
            }
            CloseGuardEffect::KeepOpen
            | CloseGuardEffect::SaveProject { .. }
            | CloseGuardEffect::CloseNow(_) => {}
        }
    }

    pub(super) fn recover_failed_close_guard(&mut self, cx: &App) {
        if !matches!(
            self.workbench.read(cx).project_io_status,
            ProjectIoStatus::Failed(_)
        ) {
            return;
        }
        let request = self
            .close_guard
            .lock()
            .ok()
            .and_then(|guard| match guard.state() {
                CloseGuardState::Saving { request, .. } => Some(request),
                _ => None,
            });
        if let Some(request) = request {
            if let Ok(mut guard) = self.close_guard.lock() {
                let _ = guard.save_finished(request, false);
            }
        }
    }

    pub(super) fn save(
        &mut self,
        save_as: bool,
        post_save: Option<PostSaveAction>,
        cx: &mut Context<Self>,
    ) {
        let document = self.workspace_document();
        self.workbench.update(cx, |workbench, _| {
            workbench.observe_workspace(document.clone())
        });
        let path = self.workbench.read(cx).package_root();
        self.workbench.update(cx, |workbench, cx| {
            if save_as || path.is_none() {
                workbench.save_as(document, post_save, cx);
            } else if let Some(path) = path {
                workbench.save_project(path, document, post_save, cx);
            }
        });
    }

    pub(super) fn import_pending_workspace(&mut self, cx: &mut Context<Self>) {
        let document = self
            .workbench
            .update(cx, |workbench, _cx| workbench.take_workspace_import());
        let Some(document) = document else {
            return;
        };
        self.workbench.update(cx, |workbench, cx| {
            workbench.retain_workspace_panes(&document, cx)
        });
        let authoritative = document.clone();
        match self
            .workspace
            .update(cx, |workspace, cx| workspace.import_document(document, cx))
        {
            Ok(()) => {
                replace_workspace_layout_document(&self.workspace_layout, authoritative, false);
            }
            Err(error) => eprintln!("restoring workspace document: {error:#}"),
        }
    }

    pub(super) fn persist_reading_query_documents(&mut self, cx: &mut Context<Self>) {
        let updates = self.workbench.update(cx, |workbench, _cx| {
            workbench.take_reading_query_documents()
        });
        if updates.is_empty() {
            return;
        }
        let mut retry = BTreeMap::new();
        let mut document = self.workspace.read(cx).export_document();
        for (view, query) in &updates {
            let Some(mut descriptor) = document.views.get(&view).cloned() else {
                retry.insert(*view, query.clone());
                continue;
            };
            let Ok(data) = serde_json::to_value(query) else {
                retry.insert(*view, query.clone());
                continue;
            };
            descriptor.state = WorkspaceViewState::Extension { data };
            if let Err(error) = document.replace_view(descriptor) {
                retry.insert(*view, query.clone());
                self.workbench.update(cx, |workbench, cx| {
                    workbench.constructive_status =
                        Some(format!("Reading document could not be retained · {error}"));
                    cx.notify();
                });
                continue;
            }
        }
        let authoritative = document.clone();
        match self
            .workspace
            .update(cx, |workspace, cx| workspace.import_document(document, cx))
        {
            Ok(()) => {
                replace_workspace_layout_document(&self.workspace_layout, authoritative, false);
                if !retry.is_empty() {
                    self.workbench.update(cx, |workbench, _| {
                        workbench.restore_reading_query_documents(retry)
                    });
                }
            }
            Err(error) => self.workbench.update(cx, |workbench, cx| {
                // Import is atomic at the workspace boundary. If it failed,
                // none of this drain is durable, so retry every latest pane
                // document rather than only the locally rejected entries.
                workbench.restore_reading_query_documents(updates);
                workbench.constructive_status =
                    Some(format!("Reading document could not be retained · {error}"));
                cx.notify();
            }),
        }
    }
}
