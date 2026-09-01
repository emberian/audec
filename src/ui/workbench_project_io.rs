//! Open, save, export, autosave, and project replacement plumbing.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

impl Workbench {
    pub(super) fn choose_audio(&mut self, cx: &mut Context<Self>) {
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(SharedString::from("Analyze")),
            initial_directory: None,
            extensions: ["flac", "wav", "ogg", "mp3"]
                .into_iter()
                .map(SharedString::from)
                .collect(),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = selection.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this, cx| this.load_path(path, cx));
        })
        .detach();
    }

    pub(super) fn choose_project(&mut self, cx: &mut Context<Self>) {
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: Some(SharedString::from("Open audec project")),
            initial_directory: None,
            extensions: vec![SharedString::from("json")],
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = selection.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let package = if path.file_name().and_then(|name| name.to_str()) == Some("project.json")
            {
                path.parent()
                    .map_or(path.clone(), std::path::Path::to_path_buf)
            } else {
                path
            };
            let _ = this.update(cx, |this, cx| this.open_project_package(package, None, cx));
        })
        .detach();
    }

    pub(super) fn new_project(&mut self, cx: &mut Context<Self>) {
        let project = match crate::daw_project::DawProject::new("Untitled", 48_000, 120.0) {
            Ok(project) => project,
            Err(error) => {
                self.project_io_status = ProjectIoStatus::Failed(error.to_string());
                cx.notify();
                return;
            }
        };
        let live = match LiveProject::from_project(project, crate::daw_engine::AssetPcmMap::new()) {
            Ok(live) => live,
            Err(error) => {
                self.project_io_status = ProjectIoStatus::Failed(error.to_string());
                cx.notify();
                return;
            }
        };
        self.open_generation = self.open_generation.wrapping_add(1).max(1);
        self.save_generation = self.save_generation.wrapping_add(1).max(1);
        self.prepare_for_document_install(cx);
        self.project_lifecycle = ProjectDocumentLifecycle::new();
        match self
            .session
            .update(cx, |session, _| session.install(live, None))
        {
            Ok(_) => {
                self.project_io_status = ProjectIoStatus::Idle;
                self.autosave_last_attempt = Instant::now();
                self.handle_session_events(cx);
            }
            Err(error) => {
                self.project_io_status = ProjectIoStatus::Failed(error.to_string());
            }
        }
        cx.notify();
    }

    pub(super) fn package_root(&self) -> Option<PathBuf> {
        self.project_lifecycle
            .manifest_path()
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf)
    }

    pub(super) fn observe_workspace(&mut self, document: WorkspaceDocument) {
        self.project_lifecycle.replace_workspace(Some(document));
    }

    pub(super) fn open_project_package(
        &mut self,
        package_root: PathBuf,
        recovery: Option<crate::project_store::RecoveryCheckpoint>,
        cx: &mut Context<Self>,
    ) {
        self.open_generation = self.open_generation.wrapping_add(1).max(1);
        let open_generation = self.open_generation;
        self.project_io_status = ProjectIoStatus::Opening(package_root.clone());
        let package = match ProjectPackage::new(package_root.clone()) {
            Ok(package) => package,
            Err(error) => {
                self.project_io_status = ProjectIoStatus::Failed(error.to_string());
                cx.notify();
                return;
            }
        };
        let actions = ProjectFileActions::new(ProjectRepository::new(
            ProjectStore::new(package),
            JsonAirPayloadCodec,
        ));
        let request = match recovery {
            Some(checkpoint) => self
                .project_lifecycle
                .begin_open_recovery_discarding_changes(actions, checkpoint),
            None => self
                .project_lifecycle
                .begin_open_primary_discarding_changes(actions),
        };
        let load = cx.background_spawn(async move {
            request.load_with_journal_decoder_factory(
                &DeterministicRuntimeCommandCodec,
                |project| {
                    ProjectRateHydrationDecoder::new(
                        project.state().domains.arrangement.sample_rate,
                    )
                },
            )
        });
        cx.spawn(async move |this, cx| {
            let completion = load.await;
            let _ = this.update(cx, |this, cx| {
                if this.open_generation != open_generation {
                    return;
                }
                let finish = {
                    let lifecycle = &mut this.project_lifecycle;
                    this.session.update(cx, |session, _| {
                        lifecycle.finish_open(session, completion, None)
                    })
                };
                match finish {
                    Ok(outcome) => {
                        this.save_generation = this.save_generation.wrapping_add(1).max(1);
                        this.prepare_for_document_install(cx);
                        this.pending_workspace_import = this.project_lifecycle.workspace().cloned();
                        let diagnostics = this
                            .project_lifecycle
                            .diagnostics()
                            .project_io
                            .iter()
                            .map(|diagnostic| diagnostic.message.clone())
                            .chain(
                                this.project_lifecycle
                                    .diagnostics()
                                    .media
                                    .iter()
                                    .map(|diagnostic| diagnostic.message.clone()),
                            )
                            .collect::<Vec<_>>();
                        this.audio_error =
                            (!diagnostics.is_empty()).then(|| diagnostics.join(" · "));
                        this.project_io_status = if outcome.recovery_available == 0 {
                            ProjectIoStatus::Saved(package_root)
                        } else {
                            ProjectIoStatus::RecoveryAvailable {
                                count: outcome.recovery_available,
                            }
                        };
                        this.autosave_last_attempt = Instant::now();
                        this.handle_session_events(cx);
                    }
                    Err(error) => {
                        this.project_io_status = ProjectIoStatus::Failed(error.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn save_project(
        &mut self,
        package_root: PathBuf,
        workspace: WorkspaceDocument,
        post_save: Option<PostSaveAction>,
        cx: &mut Context<Self>,
    ) {
        self.save_generation = self.save_generation.wrapping_add(1).max(1);
        let save_generation = self.save_generation;
        let open_generation = self.open_generation;
        self.project_lifecycle.replace_workspace(Some(workspace));
        let package = match ProjectPackage::new(package_root.clone()) {
            Ok(package) => package,
            Err(error) => {
                self.project_io_status = ProjectIoStatus::Failed(error.to_string());
                cx.notify();
                return;
            }
        };
        let actions = ProjectFileActions::new(ProjectRepository::new(
            ProjectStore::new(package),
            JsonAirPayloadCodec,
        ));
        let request = {
            let session = self.session.read(cx);
            if self.package_root().as_ref() == Some(&package_root) {
                self.project_lifecycle.begin_save(session)
            } else {
                self.project_lifecycle.begin_save_as(session, actions)
            }
        };
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                self.project_io_status = ProjectIoStatus::Failed(error.to_string());
                cx.notify();
                return;
            }
        };
        self.project_io_status = ProjectIoStatus::Saving(package_root.clone());
        let save = cx.background_spawn(async move {
            request.persist_with_journal(&DeterministicRuntimeCommandCodec)
        });
        cx.spawn(async move |this, cx| {
            let completion = save.await;
            let _ = this.update(cx, |this, cx| {
                if this.save_generation != save_generation
                    || this.open_generation != open_generation
                {
                    return;
                }
                let result = {
                    let lifecycle = &mut this.project_lifecycle;
                    this.session
                        .update(cx, |session, _| lifecycle.finish_save(session, completion))
                };
                match result {
                    Ok(outcome) => {
                        this.project_io_status = if outcome.document_clean {
                            ProjectIoStatus::Saved(package_root.clone())
                        } else {
                            ProjectIoStatus::Failed(format!(
                                "saved revision {}, but newer edits remain",
                                outcome.result.revision_guard.revision
                            ))
                        };
                        this.autosave_last_attempt = Instant::now();
                        if outcome.document_clean {
                            if let Some(action) = post_save {
                                match action {
                                    PostSaveAction::Quit => cx.quit(),
                                    PostSaveAction::Replace { intent, window } => {
                                        let _ = window.update(cx, |workspace, window, cx| {
                                            workspace
                                                .perform_project_replacement(intent, window, cx)
                                        });
                                    }
                                }
                            }
                        }
                        cx.notify();
                    }
                    Err(error) => {
                        this.project_io_status = ProjectIoStatus::Failed(error.to_string());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn save_as(
        &mut self,
        workspace: WorkspaceDocument,
        post_save: Option<PostSaveAction>,
        cx: &mut Context<Self>,
    ) {
        let open_generation = self.open_generation;
        let package_root = self.package_root();
        let directory = package_root
            .as_deref()
            .and_then(std::path::Path::parent)
            .unwrap_or_else(|| std::path::Path::new("."));
        let suggested = self
            .session
            .read(cx)
            .project_snapshot()
            .ok()
            .map(|snapshot| format!("{}.audec", snapshot.project.name))
            .unwrap_or_else(|| "Untitled.audec".into());
        let selection = cx.prompt_for_new_path(directory, Some(&suggested));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(mut path))) = selection.await else {
                return;
            };
            if path.extension().and_then(|extension| extension.to_str()) != Some("audec") {
                path.set_extension("audec");
            }
            let _ = this.update(cx, |this, cx| {
                if this.open_generation != open_generation {
                    return;
                }
                this.save_project(path, workspace, post_save, cx)
            });
        })
        .detach();
    }

    pub(super) fn export_wav(&mut self, cx: &mut Context<Self>) {
        let package_root = self.package_root();
        let directory = package_root
            .as_deref()
            .and_then(std::path::Path::parent)
            .unwrap_or_else(|| std::path::Path::new("."));
        let selection = cx.prompt_for_new_path(directory, Some("audec-export.wav"));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(mut destination))) = selection.await else {
                return;
            };
            if destination
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("wav")
            {
                destination.set_extension("wav");
            }
            let _ = this.update(cx, |this, cx| {
                this.start_export_to(destination, cx);
            });
        })
        .detach();
    }

    pub(super) fn start_export_to(&mut self, destination: PathBuf, cx: &mut Context<Self>) {
        let span = self
            .session
            .read(cx)
            .project_snapshot()
            .map_err(|error| error.to_string())
            .and_then(|snapshot| {
                let range = snapshot
                    .project
                    .state()
                    .domains
                    .arrangement
                    .project_range()
                    .ok_or_else(|| "The arrangement is empty".to_owned())?;
                RenderSpan::new(range.start.get().min(0), range.end.get())
                    .map_err(|error| error.to_string())
            });
        let span = match span {
            Ok(span) => span,
            Err(error) => {
                self.project_io_status = ProjectIoStatus::Failed(error);
                cx.notify();
                return;
            }
        };
        let job = match self.audio_controller.request_current_export(
            RenderScope::Master,
            span,
            OutputTailPolicy::Crop,
        ) {
            Ok(job) => job,
            Err(ProjectAudioControllerError::CurrentExportTargetNotCompiled { .. })
                if self.audio_snapshot_digest.is_some() =>
            {
                self.pending_export_destination = Some(destination.clone());
                self.project_io_status = ProjectIoStatus::Exporting(destination);
                cx.notify();
                return;
            }
            Err(error) => {
                self.project_io_status = ProjectIoStatus::Failed(error.to_string());
                cx.notify();
                return;
            }
        };
        self.pending_export_destination = None;
        self.project_io_status = ProjectIoStatus::Exporting(destination.clone());
        let revision = job.revision();
        let cancellation = RenderCancellation::new();
        let render = cx.background_spawn(async move { job.execute(&cancellation) });
        cx.spawn(async move |this, cx| {
            let completion = match render.await {
                Ok(completion) => completion,
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.project_io_status = ProjectIoStatus::Failed(error.to_string());
                        cx.notify();
                    });
                    return;
                }
            };
            let request = this.update(cx, |this, cx| {
                let rendered = this
                    .audio_controller
                    .complete_current_export(completion)
                    .map_err(|error| error.to_string())?;
                this.project_lifecycle
                    .begin_export(
                        this.session.read(cx),
                        revision,
                        rendered.audio,
                        WavExportRequest::new(destination.clone()),
                    )
                    .map_err(|error| error.to_string())
            });
            let Ok(request) = request else {
                return;
            };
            let request = match request {
                Ok(request) => request,
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.project_io_status = ProjectIoStatus::Failed(error);
                        cx.notify();
                    });
                    return;
                }
            };
            let shown = destination.clone();
            let export = cx.background_spawn(async move {
                request
                    .export(&mut NoopExportObserver)
                    .map_err(|error| error.to_string())
            });
            let result = export.await;
            let _ = this.update(cx, |this, cx| {
                this.project_io_status = match result {
                    Ok(_) => ProjectIoStatus::Exported(shown),
                    Err(error) => ProjectIoStatus::Failed(error),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn is_project_dirty(&self, cx: &App) -> bool {
        self.project_lifecycle
            .is_dirty(self.session.read(cx))
            .unwrap_or(false)
    }

    pub(super) fn replacement_disposition(&self, cx: &App) -> ProjectReplacementDisposition {
        self.project_lifecycle
            .replacement_disposition(self.session.read(cx))
            .unwrap_or(ProjectReplacementDisposition::Dirty)
    }

    pub(super) fn maybe_autosave(&mut self, cx: &mut Context<Self>) {
        if self.autosave_in_flight
            || self.autosave_last_attempt.elapsed() < AUTOSAVE_INTERVAL
            || self.project_lifecycle.manifest_path().is_none()
            || !self.is_project_dirty(cx)
        {
            return;
        }
        self.autosave_last_attempt = Instant::now();
        let request = match self
            .project_lifecycle
            .begin_autosave(self.session.read(cx), unix_time_ms())
        {
            Ok(request) => request,
            Err(error) => {
                self.project_io_status = ProjectIoStatus::Failed(error.to_string());
                cx.notify();
                return;
            }
        };
        self.autosave_in_flight = true;
        let save = cx.background_spawn(async move {
            request.persist_with_journal(&DeterministicRuntimeCommandCodec)
        });
        cx.spawn(async move |this, cx| {
            let completion = save.await;
            let _ = this.update(cx, |this, cx| {
                this.autosave_in_flight = false;
                let result = {
                    let lifecycle = &mut this.project_lifecycle;
                    this.session
                        .update(cx, |session, _| lifecycle.finish_save(session, completion))
                };
                match result {
                    Ok(_) => {
                        let count = this.project_lifecycle.recovery_options().checkpoints.len();
                        if count > 0 {
                            this.project_io_status = ProjectIoStatus::RecoveryAvailable { count };
                        }
                    }
                    Err(ProjectLifecycleError::DocumentChangedDuringOperation) => {}
                    Err(error) => {
                        this.project_io_status = ProjectIoStatus::Failed(error.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn first_pattern_id(&self, cx: &App) -> u64 {
        self.session
            .read(cx)
            .project_snapshot()
            .ok()
            .and_then(|snapshot| {
                snapshot
                    .project
                    .state()
                    .domains
                    .sequencer
                    .patterns()
                    .patterns()
                    .next()
                    .map(|pattern| pattern.id.get())
            })
            .unwrap_or(0)
    }

    pub(super) fn first_automation_lane_id(&self, cx: &App) -> u64 {
        self.session
            .read(cx)
            .project_snapshot()
            .ok()
            .and_then(|snapshot| {
                snapshot
                    .project
                    .state()
                    .domains
                    .automation
                    .lanes()
                    .next()
                    .map(|lane| lane.id.get())
            })
            .unwrap_or(0)
    }

    pub(super) fn take_workspace_import(&mut self) -> Option<WorkspaceDocument> {
        self.pending_workspace_import.take()
    }

    pub(super) fn set_product_shell_hosted(&mut self, hosted: bool, cx: &mut Context<Self>) {
        self.product_shell_hosted = hosted;
        cx.notify();
    }
}
