//! DawWorkspace product shell: explorer, inspector, reveal, and root render.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

impl DawWorkspace {
    pub(super) fn handle_object_reveals(&mut self, cx: &mut Context<Self>) {
        let pending = self
            .workbench
            .update(cx, |workbench, _| workbench.take_object_reveals());
        for pending in pending {
            self.apply_object_reveal(pending, cx);
        }
    }

    pub(super) fn refresh_product_shell(&mut self, cx: &mut Context<Self>) {
        let kept = self.workspace_document();
        let (project, selected_object, collections) = {
            let workbench = self.workbench.read(cx);
            let session = workbench.session.read(cx);
            let project = session
                .project_snapshot()
                .ok()
                .map(|snapshot| snapshot.project.clone());
            let selected_object = session
                .selection()
                .selection
                .objects
                .inspector_target()
                .cloned();
            let collections = workbench
                .reverse_surface_store
                .lock()
                .ok()
                .map(|store| {
                    ExplorerSemanticCollections::from_reverse_documents(store.documents())
                        .include_interpretations(session.deprojection_workspace_interpretations())
                })
                .unwrap_or_default()
                .include_kept_findings(&kept);
            (project, selected_object, collections)
        };
        let Some(project) = project else {
            self.explorer_model = None;
            self.explorer_semantic = None;
            self.inspector_report = None;
            self.explorer_breadcrumb.clear();
            return;
        };
        let rebuild = !self
            .explorer_model
            .as_ref()
            .is_some_and(|model| model.revision() == project.revisions().aggregate)
            || self.explorer_semantic.as_ref() != Some(&collections);
        if rebuild {
            let model = ExplorerModel::build(ExplorerInput::from_collections(
                project.as_ref(),
                &collections,
            ));
            self.explorer_semantic = Some(collections);
            let reconciled = model.reconcile_selection(self.explorer_selection.clone());
            self.explorer_selection = reconciled.selection;
            self.explorer_breadcrumb = reconciled.breadcrumb;
            self.explorer_diagnostic = reconciled.diagnostic.map(|value| value.message);
            self.explorer_model = Some(model);
        }
        let Some(model) = self.explorer_model.as_ref() else {
            return;
        };
        if let Some(object) = selected_object {
            if let Some(id) = model.object_node(&object).cloned() {
                let result = model.select(self.explorer_selection.clone(), id);
                self.explorer_selection = result.selection;
                self.explorer_breadcrumb = result.breadcrumb;
                self.explorer_diagnostic = result.diagnostic.map(|value| value.message);
            } else {
                self.explorer_breadcrumb = vec![reveal_breadcrumb(&object).replace(" › ", " / ")];
            }
            self.inspector_report = Some(InspectorModel::inspect(project.as_ref(), object));
        } else {
            self.inspector_report =
                self.explorer_selection
                    .selected
                    .as_ref()
                    .and_then(|id| match model.node(id) {
                        Some(ExplorerTarget::Object(object)) => {
                            Some(InspectorModel::inspect(project.as_ref(), object.clone()))
                        }
                        _ => None,
                    });
        }
    }

    pub(super) fn set_explorer_mode(&mut self, mode: ExplorerMode, cx: &mut Context<Self>) {
        self.explorer_selection.mode = mode;
        self.explorer_diagnostic = None;
        cx.notify();
    }

    pub(super) fn select_explorer_node(&mut self, id: ExplorerNodeId, cx: &mut Context<Self>) {
        let Some(model) = self.explorer_model.as_ref() else {
            return;
        };
        let result = model.select(self.explorer_selection.clone(), id.clone());
        let target = model.node(&id).cloned();
        let report = match &target {
            Some(ExplorerTarget::Object(object)) => self
                .workbench
                .read(cx)
                .session
                .read(cx)
                .project_snapshot()
                .ok()
                .map(|snapshot| InspectorModel::inspect(&snapshot.project, object.clone())),
            _ => None,
        };
        if let Some(ExplorerTarget::Mode(mode)) = target.as_ref() {
            self.explorer_selection.mode = *mode;
        }
        if let Some(ExplorerTarget::Object(object)) = target.as_ref() {
            self.workbench.update(cx, |workbench, cx| {
                if let Err(error) = workbench.session.update(cx, |session, _| {
                    session.replace_object_selection(
                        ObjectSelection {
                            primary: Some(object.clone()),
                            ..ObjectSelection::default()
                        },
                        SelectionProvenance {
                            source: SelectionSource::Inspector,
                            source_view: None,
                        },
                    )
                }) {
                    workbench.constructive_status =
                        Some(format!("Inspector selection unavailable · {error}"));
                }
            });
        }
        self.explorer_selection = result.selection;
        self.explorer_breadcrumb = result.breadcrumb;
        self.explorer_diagnostic = result.diagnostic.map(|value| value.message);
        self.inspector_report = report;
        cx.notify();
    }

    pub(super) fn reveal_explorer_selection(&mut self, cx: &mut Context<Self>) {
        let request = self
            .explorer_model
            .as_ref()
            .zip(self.explorer_selection.selected.as_ref())
            .map(|(model, selected)| {
                model.reveal_request(selected, RevealIntent::ActivateExisting)
            });
        let Some(request) = request else {
            return;
        };
        match request {
            Ok(request) => self.queue_direct_reveal(request, "Opened from Explorer", cx),
            Err(error) => {
                self.explorer_diagnostic = Some(error.message);
                cx.notify();
            }
        }
    }

    pub(super) fn reveal_inspector_object(&mut self, object: ObjectRef, cx: &mut Context<Self>) {
        self.queue_direct_reveal(
            crate::project_controller::RevealRequest::new(object, RevealIntent::ActivateExisting),
            "Opened from Inspector",
            cx,
        );
    }

    pub(super) fn queue_direct_reveal(
        &mut self,
        request: crate::project_controller::RevealRequest,
        headline: &'static str,
        cx: &mut Context<Self>,
    ) {
        let receipt = self
            .workbench
            .read(cx)
            .session
            .read(cx)
            .issue_reveal(request);
        match receipt {
            Ok(receipt) => {
                let pending = PendingObjectReveal {
                    receipt,
                    diagnostics: Vec::new(),
                    headline: headline.into(),
                };
                self.apply_object_reveal(pending, cx);
            }
            Err(error) => {
                self.explorer_diagnostic = Some(format!("Reveal unavailable · {error}"));
                cx.notify();
            }
        }
    }

    /// Write a kept finding into the durable workspace so the Explorer still
    /// lists it after the project is reopened. Keeping used to change nothing
    /// but a status line.
    pub(super) fn record_kept_finding(&mut self, object: &ObjectRef, revision: u64) {
        if !matches!(object, ObjectRef::Finding(_)) {
            return;
        }
        let mut document = self.workspace_document();
        let changed = document.record_kept_finding(crate::workspace_document::KeptFindingRecord {
            address: object.address(),
            title: self
                .explorer_semantic
                .as_ref()
                .and_then(|collections| collections.finding_titles.get(&object.address()).cloned()),
            revision,
        });
        if changed {
            replace_workspace_layout_document(&self.workspace_layout, document, true);
            self.explorer_semantic = None;
        }
    }

    pub(super) fn apply_object_reveal(
        &mut self,
        pending: PendingObjectReveal,
        cx: &mut Context<Self>,
    ) {
        let resolution = {
            let workbench = self.workbench.read(cx);
            workbench.session.read(cx).resolve_reveal(&pending.receipt)
        };
        let Some(request) = resolution.request else {
            if matches!(resolution.disposition, RevealDisposition::Fallback { .. }) {
                self.workspace
                    .update(cx, |workspace, cx| {
                        workspace.activate_or_show(WorkspaceViewId::TRACK_OVERVIEW, cx)
                    })
                    .ok();
            }
            self.explorer_diagnostic = Some(format!(
                "{} · result is no longer current · {:?}",
                pending.headline, resolution.disposition
            ));
            cx.notify();
            return;
        };
        let mut request = request;
        let object = request.object.clone();
        let Some(guard) = resolution.guard else {
            self.explorer_diagnostic = Some(format!(
                "{} · reveal unavailable · the session did not issue a current reveal guard",
                pending.headline
            ));
            cx.notify();
            return;
        };

        if request.origin
            == crate::project_controller::RevealOrigin::Completion(
                crate::project_controller::RevealCompletionKind::KeptFinding,
            )
        {
            self.record_kept_finding(&object, guard.project_revision);
        }
        let document = self.workspace_document();
        // The session resolver revalidated this request against `guard`; pin
        // the planner to that exact current revision rather than its original
        // publication when it selected a surviving object or predecessor.
        request.expected_project_revision = Some(guard.project_revision);
        let intent = request.intent;
        let plan = ObjectNavigator::plan_at_revision(&document, guard.project_revision, request);
        let mut diagnostic = pending
            .diagnostics
            .first()
            .map(|diagnostic| diagnostic.message.clone())
            .or_else(|| {
                plan.diagnostics
                    .first()
                    .map(|diagnostic| diagnostic.message.clone())
            });
        if matches!(
            resolution.disposition,
            RevealDisposition::Predecessor { .. }
        ) {
            diagnostic.get_or_insert_with(|| {
                "The created object was removed; revealing its nearest current predecessor.".into()
            });
        }
        let (guard_is_current, current_project_revision) = {
            let workbench = self.workbench.read(cx);
            let session = workbench.session.read(cx);
            (
                session.reveal_guard_is_current(guard),
                session
                    .project_snapshot()
                    .map(|snapshot| snapshot.revisions().aggregate)
                    .unwrap_or(guard.project_revision),
            )
        };
        if !guard_is_current {
            let refusal = RevealRefusal::Stale {
                requested: guard.project_revision,
                current: current_project_revision,
            };
            self.explorer_diagnostic = Some(format!("{} · {refusal}", pending.headline));
            cx.notify();
            return;
        }
        let view = match plan.workspace {
            WorkspaceReveal::Activate { view, .. } => self
                .workspace
                .update(cx, |workspace, cx| workspace.activate_or_show(view, cx))
                .map(|()| Some(view)),
            WorkspaceReveal::Create(descriptor) => self
                .workspace
                .update(cx, |workspace, cx| {
                    workspace.create_view(descriptor, None, cx)
                })
                .map(Some),
            WorkspaceReveal::Retarget { descriptor, .. } => {
                let view = descriptor.id;
                self.workspace
                    .update(cx, |workspace, cx| {
                        workspace.replace_view_descriptor(descriptor, cx)?;
                        workspace.activate_or_show(view, cx)
                    })
                    .map(|()| Some(view))
            }
            // The plan opened nothing. Ask the surfaces where the object
            // already is, and say either which one has it or why none can,
            // instead of answering with silence.
            WorkspaceReveal::None | WorkspaceReveal::Unsupported => {
                let surfaces: [&dyn RevealSurface; 1] = [&document];
                let answer = answer_reveal(
                    crate::project_controller::RevealRequest::new(object.clone(), intent),
                    guard.project_revision,
                    surfaces,
                )
                .checked_at(current_project_revision);
                match answer.outcome {
                    RevealOutcome::Shown(location) => diagnostic.get_or_insert_with(|| {
                        format!("Already shown in view {}", location.view.0)
                    }),
                    RevealOutcome::Refused(refusal) => {
                        diagnostic.get_or_insert_with(|| refusal.to_string())
                    }
                    RevealOutcome::Created { view } | RevealOutcome::Retargeted { view } => {
                        diagnostic.get_or_insert_with(|| format!("Shown in view {}", view.0))
                    }
                };
                Ok(None)
            }
        };
        let view = match view {
            Ok(view) => view,
            Err(error) => {
                diagnostic = Some(format!("Reveal failed · {error}"));
                None
            }
        };
        self.workbench.update(cx, |workbench, cx| {
            workbench.apply_object_reveal_selection(view, &plan.selection, cx)
        });
        let completion = RevealCompletion {
            headline: pending.headline,
            breadcrumb: reveal_breadcrumb(&object).into(),
            diagnostic,
        };
        let shown_contextually = view.is_some_and(|view| {
            self.workbench.update(cx, |workbench, cx| {
                workbench.set_workspace_completion(view, completion.clone(), cx)
            })
        });
        if !shown_contextually {
            self.explorer_diagnostic = Some(match completion.diagnostic {
                Some(diagnostic) => format!("{} · {diagnostic}", completion.headline),
                None => format!("{} · {}", completion.headline, completion.breadcrumb),
            });
        }
        cx.notify();
    }

    pub(super) fn render_project_commands(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let workbench = self.workbench.read(cx);
        let project = workbench
            .package_root()
            .as_deref()
            .and_then(std::path::Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or("Untitled project")
            .to_owned();
        let dirty = workbench.is_project_dirty(cx);
        // A "SAVED · path" chip is stale the moment the project is edited;
        // only in-flight, recovery, and failure states survive a dirty tree.
        let status = workbench.project_io_status.label().filter(|label| {
            !dirty || !(label.starts_with("SAVED ·") || label.starts_with("EXPORTED ·"))
        });
        // Refusals and receipts from every action land here; the sidebar that
        // used to render them is hidden in the product shell.
        let notice = workbench.constructive_status.clone();
        let audio_error = workbench.audio_error.clone();

        div()
            .id("project-commands")
            .flex_none()
            .flex()
            .items_center()
            .overflow_x_scroll()
            .gap_2()
            .pl(px(82.0))
            .pr_3()
            .py_2()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL_ALT))
            .child(section_label("PROJECT"))
            .child(
                div()
                    .max_w(px(190.0))
                    .truncate()
                    .text_xs()
                    .text_color(if dirty { rgb(AMBER) } else { rgb(MUTED) })
                    .child(if dirty {
                        format!("{project} · EDITED")
                    } else {
                        project
                    }),
            )
            .when_some(audio_error, |row, error| {
                row.child(
                    div()
                        .id("shell-audio-error")
                        .max_w(px(360.0))
                        .truncate()
                        .text_xs()
                        .text_color(rgb(AMBER))
                        .child(format!("AUDIO · {error}")),
                )
            })
            .when_some(notice, |row, notice| {
                row.child(
                    div()
                        .id("shell-notice")
                        .max_w(px(420.0))
                        .truncate()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child(notice),
                )
            })
            .child(viz_control("project-new", "New").on_click(cx.listener(
                |this, _, window, cx| {
                    this.request_project_replacement(
                        ProjectReplacementIntent::NewProject,
                        window,
                        cx,
                    )
                },
            )))
            .child(
                viz_control("project-open", "Open project").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.request_project_replacement(
                            ProjectReplacementIntent::ChooseProject,
                            window,
                            cx,
                        )
                    },
                )),
            )
            .child(
                viz_control("project-open-audio", "Open audio").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.request_project_replacement(
                            ProjectReplacementIntent::ChooseAudio,
                            window,
                            cx,
                        )
                    },
                )),
            )
            .child(
                viz_control("project-save", "Save")
                    .on_click(cx.listener(|this, _, _, cx| this.save(false, None, cx))),
            )
            .child(
                viz_control("project-save-as", "Save as")
                    .on_click(cx.listener(|this, _, _, cx| this.save(true, None, cx))),
            )
            .child(
                viz_control("project-recovery", "Recovery").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.request_project_replacement(
                            ProjectReplacementIntent::ChooseRecovery,
                            window,
                            cx,
                        )
                    },
                )),
            )
            .child(viz_control("project-export", "Export WAV").on_click({
                let workbench = self.workbench.clone();
                move |_, _, cx| workbench.update(cx, |workbench, cx| workbench.export_wav(cx))
            }))
            .child(
                viz_control("project-reading-query", "Reading query").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.invoke_action_id(
                            surface_ids::EDITOR_READING_QUERY,
                            InvocationOrigin::Toolbar,
                            window,
                            cx,
                        )
                    },
                )),
            )
            .child(
                viz_control("project-arrangement", "Arrangement").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.invoke_action_id(
                            action_ids::EDITOR_ARRANGEMENT,
                            InvocationOrigin::Toolbar,
                            window,
                            cx,
                        )
                    },
                )),
            )
            .child(
                viz_control("project-pattern", "Pattern").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.invoke_action_id(
                            action_ids::EDITOR_DRUMS,
                            InvocationOrigin::Toolbar,
                            window,
                            cx,
                        );
                    },
                )),
            )
            .when_some(status, |row, status| {
                row.child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_xs()
                        .text_color(if status.starts_with("FILE ERROR") {
                            rgb(AMBER)
                        } else {
                            rgb(DIM)
                        })
                        .child(status),
                )
            })
            .into_any_element()
    }

    pub(super) fn render_product_explorer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selected = self.explorer_selection.selected.clone();
        let root = self.explorer_model.as_ref().map(|model| {
            model.filtered(
                self.explorer_selection.mode,
                &self.explorer_selection.filter,
            )
        });
        let (sample_workflow_heading, active_span_label, destination_summary) = {
            let workbench = self.workbench.read(cx);
            let active_sample = workbench.active_sample_span();
            let heading =
                if active_sample.is_some_and(|scope| scope.origin() == SampleSpanOrigin::Loop) {
                    "MAKE FROM LOOP"
                } else {
                    "MAKE FROM SELECTION"
                };
            let label = active_sample.map_or_else(
                || "Enable a loop or select a source range to make material".to_owned(),
                |scope| workbench.active_sample_span_label(scope),
            );
            let source_name = workbench
                .analysis()
                .map(|analysis| sample_workflow_name_stem(&analysis.title))
                .unwrap_or_else(|| "Source".into());
            let sample_instrument =
                sample_workflow_instrument_name(SampleWorkflowCommand::MakeSample, &source_name);
            let kit_instrument =
                sample_workflow_instrument_name(SampleWorkflowCommand::SliceToPads, &source_name);
            (
                heading,
                label,
                format!(
                    "Destinations · Instrument “{sample_instrument}” · Instrument “{kit_instrument}” · beat opens Pattern “{source_name} beat”"
                ),
            )
        };
        div()
            .id("product-explorer")
            .w(px(244.0))
            .h_full()
            .flex_none()
            .min_h_0()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL_ALT))
            .child(
                div()
                    .flex_none()
                    .p_3()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(section_label("EXPLORER"))
                    .child(div().mt_2().flex().flex_wrap().gap_1().children(
                        ExplorerMode::ALL.into_iter().map(|mode| {
                            let active = self.explorer_selection.mode == mode;
                            div()
                                .id(SharedString::from(format!(
                                    "explorer-mode:{}",
                                    mode.label()
                                )))
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .border_1()
                                .border_color(rgb(if active { CYAN } else { BORDER }))
                                .text_xs()
                                .text_color(rgb(if active { CYAN } else { MUTED }))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_explorer_mode(mode, cx)
                                }))
                                .child(mode.label())
                        }),
                    )),
            )
            .child(
                div()
                    .id("product-explorer-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.explorer_scroll)
                    .py_2()
                    .when_some(root, |tree, root| {
                        tree.child(render_explorer_node(root, 0, selected, cx))
                    })
                    .when_some(self.explorer_diagnostic.clone(), |tree, diagnostic| {
                        tree.child(
                            div()
                                .px_3()
                                .py_2()
                                .text_xs()
                                .text_color(rgb(AMBER))
                                .child(diagnostic),
                        )
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .p_3()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .when(
                        matches!(
                            self.explorer_selection.mode,
                            ExplorerMode::Project | ExplorerMode::Library
                        ),
                        |footer| {
                            footer
                                .child(section_label(sample_workflow_heading))
                                .child(
                                    div()
                                        .mt_1()
                                        .text_xs()
                                        .text_color(rgb(MUTED))
                                        .child(active_span_label),
                                )
                                .child(
                                    div()
                                        .mt_2()
                                        .flex()
                                        .gap_1()
                                        .child(
                                            viz_control("explorer-make-sample", "Make sample")
                                                .on_click({
                                                    let workbench = self.workbench.clone();
                                                    move |_, _, cx| {
                                                        workbench.update(cx, |workbench, cx| {
                                                            workbench
                                                                .make_sample_from_active_span(cx)
                                                        })
                                                    }
                                                }),
                                        )
                                        .child(
                                            viz_control("explorer-slice-kit", "Slice to kit")
                                                .on_click({
                                                    let workbench = self.workbench.clone();
                                                    move |_, _, cx| {
                                                        workbench.update(cx, |workbench, cx| {
                                                            workbench.slice_active_span_to_kit(cx)
                                                        })
                                                    }
                                                }),
                                        ),
                                )
                                .child(
                                    viz_control("explorer-make-beat", "Make beat")
                                        .mt_1()
                                        .w_full()
                                        .on_click({
                                            let workbench = self.workbench.clone();
                                            move |_, _, cx| {
                                                workbench.update(cx, |workbench, cx| {
                                                    workbench.make_beat_from_active_span(cx)
                                                })
                                            }
                                        }),
                                )
                                .child(
                                    div()
                                        .mt_1()
                                        .text_xs()
                                        .text_color(rgb(DIM))
                                        .child("Shortcuts · S sample · ⇧S slice · B beat"),
                                )
                                .child(
                                    div()
                                        .mt_1()
                                        .text_xs()
                                        .text_color(rgb(DIM))
                                        .child(destination_summary),
                                )
                        },
                    )
                    .when(
                        self.explorer_selection.mode == ExplorerMode::Readings,
                        |footer| {
                            footer
                                .child(section_label("READINGS"))
                                .child(div().mt_1().text_xs().text_color(rgb(MUTED)).child(
                                    "Query imported evidence without changing project truth.",
                                ))
                                .child(
                                    viz_control("explorer-new-reading-query", "New reading query")
                                        .mt_2()
                                        .w_full()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.create_reading_query(cx)
                                        })),
                                )
                        },
                    )
                    .when(
                        self.explorer_selection.mode == ExplorerMode::Investigate,
                        |footer| {
                            let candidates = self
                                .workbench
                                .read(cx)
                                .session
                                .read(cx)
                                .list_deprojection_workspace_candidates()
                                .unwrap_or_default();
                            let current_count = candidates
                                .iter()
                                .filter(|candidate| {
                                    candidate.freshness == DeprojectionCandidateFreshness::Current
                                })
                                .count();
                            footer
                                .child(section_label("CANDIDATES"))
                                .child(
                                    div()
                                        .mt_1()
                                        .text_xs()
                                        .text_color(rgb(if candidates.is_empty() {
                                            DIM
                                        } else {
                                            CYAN
                                        }))
                                        .child(if candidates.is_empty() {
                                            "No deprojection candidate is published".to_owned()
                                        } else {
                                            format!(
                                                "{current_count} current · {} retained reading(s)",
                                                candidates.len()
                                            )
                                        }),
                                )
                                .children(candidates.into_iter().map(|candidate| {
                                    let finding = candidate.finding;
                                    let current = candidate.freshness
                                        == DeprojectionCandidateFreshness::Current;
                                    div()
                                        .id(SharedString::from(format!(
                                            "deprojection-candidate:{}",
                                            candidate.id.0
                                        )))
                                        .mt_2()
                                        .px_2()
                                        .py_1()
                                        .rounded_sm()
                                        .border_1()
                                        .border_color(rgb(BORDER))
                                        .text_xs()
                                        .text_color(rgb(if current { TEXT } else { MUTED }))
                                        .cursor_pointer()
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.queue_direct_reveal(
                                                crate::project_controller::RevealRequest::new(
                                                    ObjectRef::Finding(finding),
                                                    RevealIntent::OpenNew,
                                                ),
                                                if current {
                                                    "Opened current deprojection candidate"
                                                } else {
                                                    "Opened retained deprojection evidence"
                                                },
                                                cx,
                                            )
                                        }))
                                        .child(format!(
                                            "{} · {}",
                                            candidate.label,
                                            if current {
                                                "ready to apply"
                                            } else {
                                                "evidence only"
                                            }
                                        ))
                                }))
                        },
                    ),
            )
            .into_any_element()
    }

    pub(super) fn render_product_inspector(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let report = self.inspector_report.clone();
        let breadcrumb = if self.explorer_breadcrumb.is_empty() {
            "No object selected".to_owned()
        } else {
            self.explorer_breadcrumb.join(" › ")
        };
        div()
            .id("product-inspector")
            .w(px(268.0))
            .h_full()
            .flex_none()
            .min_h_0()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL_ALT))
            .child(
                div()
                    .flex_none()
                    .p_3()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(section_label("INSPECTOR"))
                    .child(
                        div()
                            .mt_1()
                            .text_xs()
                            .text_color(rgb(CYAN))
                            .child(breadcrumb),
                    )
                    .when(report.is_some(), |header| {
                        header.child(
                            viz_control("inspector-open-object", "Open")
                                .mt_2()
                                .on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.reveal_explorer_selection(cx)
                                    }),
                                ),
                        )
                    }),
            )
            .child(
                div()
                    .id("product-inspector-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.product_inspector_scroll)
                    .p_3()
                    .when_some(report, |body, report| {
                        body.child(div().text_sm().text_color(rgb(TEXT)).child(report.title))
                            .children(report.sections.into_iter().map(|section| {
                                let fields = section.fields.into_iter().map(|field| {
                                    let reveal = field.reveal.clone();
                                    div()
                                        .py_1()
                                        .border_b_1()
                                        .border_color(rgb(BORDER))
                                        .child(
                                            div().text_xs().text_color(rgb(DIM)).child(field.label),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .justify_between()
                                                .gap_2()
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(rgb(MUTED))
                                                        .child(field.value),
                                                )
                                                .when_some(reveal, |row, object| {
                                                    row.child(
                                                        div()
                                                            .id(SharedString::from(format!(
                                                                "inspector-reveal:{}",
                                                                object.address()
                                                            )))
                                                            .text_xs()
                                                            .text_color(rgb(CYAN))
                                                            .cursor_pointer()
                                                            .on_click(cx.listener(
                                                                move |this, _, _, cx| {
                                                                    this.reveal_inspector_object(
                                                                        object.clone(),
                                                                        cx,
                                                                    )
                                                                },
                                                            ))
                                                            .child("Reveal"),
                                                    )
                                                }),
                                        )
                                });
                                div()
                                    .mt_3()
                                    .child(section_label(section.kind.label()))
                                    .children(fields)
                            }))
                            .when(!report.diagnostics.is_empty(), |body| {
                                body.children(report.diagnostics.into_iter().map(|diagnostic| {
                                    div()
                                        .mt_2()
                                        .text_xs()
                                        .text_color(rgb(AMBER))
                                        .child(diagnostic.message)
                                }))
                            })
                    }),
            )
            .into_any_element()
    }
}

impl Render for DawWorkspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.import_pending_workspace(cx);
        self.persist_reading_query_documents(cx);
        self.handle_object_reveals(cx);
        self.refresh_product_shell(cx);
        self.persist_editor_viewports(cx);
        self.recover_failed_close_guard(cx);
        self.handle_pending_pane_context_menus(cx);
        self.refresh_action_projection(cx);
        div()
            .key_context("Audec")
            .track_focus(&self.focus_handle)
            .tab_group()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .on_key_down(cx.listener(Self::on_action_surface_key))
            .on_action(
                cx.listener(|this, action: &InvokeProjectedAction, window, cx| {
                    this.dispatch_action_request(action.request.clone(), window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &OpenCommandPalette, window, cx| {
                this.invoke_action_id(
                    action_ids::PALETTE_OPEN,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                )
            }))
            .on_action(cx.listener(|this, _: &QuitAudec, window, cx| {
                this.request_application_close(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewProject, window, cx| {
                this.invoke_action_id(
                    surface_ids::FILE_NEW,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenAudio, window, cx| {
                this.invoke_action_id(
                    surface_ids::FILE_OPEN_AUDIO,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenProject, window, cx| {
                this.invoke_action_id(
                    action_ids::FILE_OPEN,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &SaveProject, window, cx| {
                this.invoke_action_id(
                    action_ids::FILE_SAVE,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &SaveProjectAs, window, cx| {
                this.invoke_action_id(
                    surface_ids::FILE_SAVE_AS,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenRecovery, window, cx| {
                this.invoke_action_id(
                    surface_ids::FILE_RECOVERY,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &ExportWav, window, cx| {
                this.invoke_action_id(
                    action_ids::FILE_EXPORT,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &TogglePlayback, window, cx| {
                this.invoke_action_id(
                    action_ids::TRANSPORT_TOGGLE,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &SeekBackward, _, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.seek_relative(-5.0, cx));
            }))
            .on_action(cx.listener(|this, _: &SeekForward, _, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.seek_relative(5.0, cx));
            }))
            .on_action(cx.listener(|this, _: &OpenWaterfall, window, cx| {
                this.invoke_action_id(
                    surface_ids::ANALYSIS_WATERFALL,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenRhythm, window, cx| {
                this.invoke_action_id(
                    surface_ids::ANALYSIS_RHYTHM,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenComponents, window, cx| {
                this.invoke_action_id(
                    surface_ids::ANALYSIS_COMPONENTS,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenSeparation, window, cx| {
                this.invoke_action_id(
                    surface_ids::ANALYSIS_SEPARATION,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenLoom, window, cx| {
                this.invoke_action_id(
                    surface_ids::ANALYSIS_LOOM,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenArrangementEditor, window, cx| {
                this.invoke_action_id(
                    action_ids::EDITOR_ARRANGEMENT,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenSequencerEditor, window, cx| {
                this.invoke_action_id(
                    action_ids::EDITOR_DRUMS,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenMixer, window, cx| {
                this.invoke_action_id(
                    action_ids::EDITOR_MIXER,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenAutomation, window, cx| {
                this.invoke_action_id(
                    action_ids::EDITOR_AUTOMATION,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenAssets, window, cx| {
                this.invoke_action_id(
                    surface_ids::EDITOR_ASSETS,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenSampler, window, cx| {
                this.invoke_action_id(
                    surface_ids::EDITOR_SAMPLER,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenReadingQuery, window, cx| {
                this.invoke_action_id(
                    surface_ids::EDITOR_READING_QUERY,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &ViewZoomIn, window, cx| {
                this.invoke_action_id(
                    surface_ids::VIEW_ZOOM_IN,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &ViewZoomOut, window, cx| {
                this.invoke_action_id(
                    surface_ids::VIEW_ZOOM_OUT,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &ViewPanLeft, window, cx| {
                this.invoke_action_id(
                    surface_ids::VIEW_PAN_LEFT,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &ViewPanRight, window, cx| {
                this.invoke_action_id(
                    surface_ids::VIEW_PAN_RIGHT,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &ViewFit, window, cx| {
                this.invoke_action_id(
                    surface_ids::VIEW_FIT,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &ViewFollow, window, cx| {
                this.invoke_action_id(
                    surface_ids::VIEW_FOLLOW,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &SetLoopFromSelection, window, cx| {
                this.invoke_action_id(
                    surface_ids::LOOP_FROM_SELECTION,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &ToggleLoop, window, cx| {
                this.invoke_action_id(
                    action_ids::LOOP_TOGGLE,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(
                cx.listener(|this, _: &MakeSampleFromActiveSpan, window, cx| {
                    this.invoke_action_id(
                        surface_ids::SAMPLE_MAKE,
                        InvocationOrigin::Shortcut,
                        window,
                        cx,
                    );
                }),
            )
            .on_action(cx.listener(|this, _: &SliceActiveSpanToKit, window, cx| {
                this.invoke_action_id(
                    surface_ids::SAMPLE_SLICE_KIT,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &MakeBeatFromActiveSpan, window, cx| {
                this.invoke_action_id(
                    surface_ids::SAMPLE_MAKE_BEAT,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &NextWorkspacePane, window, cx| {
                this.invoke_action_id(
                    surface_ids::WORKSPACE_NEXT,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &PreviousWorkspacePane, window, cx| {
                this.invoke_action_id(
                    surface_ids::WORKSPACE_PREVIOUS,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &CloseWorkspacePane, window, cx| {
                this.invoke_action_id(
                    surface_ids::WORKSPACE_CLOSE,
                    InvocationOrigin::Shortcut,
                    window,
                    cx,
                );
            }))
            .on_action(
                cx.listener(|this, _: &FloatOrDockWorkspacePane, window, cx| {
                    this.invoke_action_id(
                        surface_ids::WORKSPACE_FLOAT_DOCK,
                        InvocationOrigin::Shortcut,
                        window,
                        cx,
                    );
                }),
            )
            .child(self.render_project_commands(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.render_product_explorer(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .child(self.workspace.clone()),
                    )
                    .child(self.render_product_inspector(cx)),
            )
            .child(self.render_command_palette(cx))
            .child(self.render_pane_context_menu(cx))
    }
}

pub(super) fn render_explorer_node(
    node: ExplorerNode,
    depth: usize,
    selected: Option<ExplorerNodeId>,
    cx: &mut Context<DawWorkspace>,
) -> gpui::AnyElement {
    let id = node.id.clone();
    let is_selected = selected.as_ref() == Some(&id);
    let marker = match node.target {
        ExplorerTarget::Mode(_) => "▾",
        ExplorerTarget::Category(_) => "›",
        ExplorerTarget::Object(_) => "•",
    };
    let children = node
        .children
        .into_iter()
        .map(|child| render_explorer_node(child, depth.saturating_add(1), selected.clone(), cx))
        .collect::<Vec<_>>();
    div()
        .child(
            div()
                .id(SharedString::from(format!("explorer-node:{}", id.as_str())))
                .pl(px(10.0 + depth as f32 * 12.0))
                .pr_2()
                .py_1()
                .flex()
                .items_center()
                .gap_2()
                .bg(rgb(if is_selected { BORDER } else { PANEL_ALT }))
                .text_color(rgb(if is_selected { TEXT } else { MUTED }))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(BORDER)).text_color(rgb(TEXT)))
                .on_click(
                    cx.listener(move |this, _, _, cx| this.select_explorer_node(id.clone(), cx)),
                )
                .child(
                    div()
                        .w(px(10.0))
                        .text_xs()
                        .text_color(rgb(DIM))
                        .child(marker),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .text_xs()
                        .truncate()
                        .child(node.label),
                )
                .when_some(node.detail, |row, detail| {
                    row.child(div().text_xs().text_color(rgb(DIM)).child(detail))
                }),
        )
        .when_some(node.diagnostic, |tree, diagnostic| {
            tree.child(
                div()
                    .pl(px(22.0 + depth as f32 * 12.0))
                    .pr_2()
                    .pb_1()
                    .text_xs()
                    .text_color(rgb(AMBER))
                    .child(diagnostic.message),
            )
        })
        .children(children)
        .into_any_element()
}
