//! Open, save, export, autosave, and project replacement plumbing.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

use std::path::Path;

use crate::export::{ExportOptions, ExportRange};
use crate::render_plan::BusTap;

#[path = "export_options.rs"]
pub(super) mod export_options;

use export_options::{
    export_options_window_options, ExportOptionsConfirm, ExportOptionsView,
    ExportRangeAvailability, ExportScopeChoice,
};

thread_local! {
    /// Options for an export that is waiting for the current revision to
    /// finish compiling.
    ///
    /// `Workbench::pending_export_destination` carries the destination across
    /// that wait, and the render completion calls [`Workbench::start_export_to`]
    /// back with it; this keeps the chosen options beside that destination so a
    /// deferred export is still the export the musician asked for. The two
    /// halves belong in one pending-export field on the Workbench, which is
    /// declared in `ui.rs` and owned by another lane this cycle.
    static DEFERRED_EXPORT_OPTIONS: RefCell<BTreeMap<PathBuf, ExportOptions>> =
        RefCell::new(BTreeMap::new());
}

fn defer_export_options(destination: &Path, options: ExportOptions) {
    DEFERRED_EXPORT_OPTIONS.with(|deferred| {
        deferred
            .borrow_mut()
            .insert(destination.to_path_buf(), options);
    });
}

fn take_deferred_export_options(destination: &Path) -> Option<ExportOptions> {
    DEFERRED_EXPORT_OPTIONS.with(|deferred| deferred.borrow_mut().remove(destination))
}

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

    /// Where file prompts start: next to the package, else next to the loaded
    /// material, else the user's documents. Never the process working
    /// directory: a scripted or shell-launched run must not litter wherever
    /// it started.
    pub(super) fn prompt_directory(&self) -> PathBuf {
        self.package_root()
            .as_deref()
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf)
            .or_else(|| {
                self.analysis()
                    .and_then(|analysis| analysis.path.parent().map(std::path::Path::to_path_buf))
            })
            .or_else(dirs::document_dir)
            .unwrap_or_else(|| PathBuf::from("."))
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
        let directory = self.prompt_directory();
        let directory = directory.as_path();
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

    /// The Export command: choose what the file will be, then where it goes.
    /// Closing the options step without confirming exports nothing.
    pub(super) fn export_wav(&mut self, cx: &mut Context<Self>) {
        let options = ExportOptions::default();
        let scopes = self.export_scope_choices(cx);
        let ranges = self.export_range_availability(cx);
        let workbench = cx.entity().downgrade();
        let window_options = export_options_window_options(cx);
        // `open_window` renders its root synchronously; defer until this
        // action's Workbench update lease has ended.
        cx.defer(move |cx| {
            let confirm: ExportOptionsConfirm = Box::new(move |options, cx| {
                let _ = workbench.update(cx, |workbench, cx| {
                    workbench.prompt_export_destination(options, cx);
                });
            });
            if let Err(error) = cx.open_window(window_options, move |window, cx| {
                let view =
                    cx.new(|cx| ExportOptionsView::new(options, scopes, ranges, confirm, cx));
                window.focus(&view.focus_handle(cx), cx);
                view
            }) {
                eprintln!("opening Export audio options: {error:#}");
            }
        });
    }

    /// Ask for a destination for an already-chosen set of options.
    pub(super) fn prompt_export_destination(
        &mut self,
        options: ExportOptions,
        cx: &mut Context<Self>,
    ) {
        let directory = self.prompt_directory();
        let directory = directory.as_path();
        let suggested = format!(
            "{}.wav",
            export_file_stem(&self.export_scope_label(&options.scope, cx), options.range)
        );
        let selection = cx.prompt_for_new_path(directory, Some(&suggested));
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
                this.start_export_with(destination, options, cx);
            });
        })
        .detach();
    }

    /// Start an export whose options were chosen earlier: either by the
    /// options step (held beside `pending_export_destination` while the render
    /// compiles) or, with no such record, today's defaults.
    pub(super) fn start_export_to(&mut self, destination: PathBuf, cx: &mut Context<Self>) {
        let options = take_deferred_export_options(&destination).unwrap_or_default();
        self.start_export_with(destination, options, cx);
    }

    pub(super) fn start_export_with(
        &mut self,
        destination: PathBuf,
        options: ExportOptions,
        cx: &mut Context<Self>,
    ) {
        let summary = self.export_summary(&options, cx);
        let span = match self.export_span_for_range(options.range, cx) {
            Ok(span) => span,
            Err(error) => {
                self.project_io_status = ProjectIoStatus::Failed(format!("{summary} · {error}"));
                cx.notify();
                return;
            }
        };
        let job = match self.audio_controller.request_current_export(
            options.scope.clone(),
            span,
            OutputTailPolicy::Crop,
        ) {
            Ok(job) => job,
            Err(ProjectAudioControllerError::CurrentExportTargetNotCompiled { .. }) => {
                // The current revision has not finished compiling. Queue the
                // export behind that render instead of reporting a file error;
                // the render completion drains `pending_export_destination`
                // and the options travel with it.
                // If nothing is compiling (a failed render left no digest),
                // republish so the host requests the render again.
                defer_export_options(&destination, options);
                self.pending_export_destination = Some(destination.clone());
                self.project_io_status = ProjectIoStatus::Exporting(destination.clone());
                if !self.audio_rendering && self.audio_snapshot_digest.is_none() {
                    let republished = self
                        .session
                        .update(cx, |session, _| session.refresh_published(None));
                    if let Err(error) = republished {
                        take_deferred_export_options(&destination);
                        self.pending_export_destination = None;
                        self.project_io_status = ProjectIoStatus::Failed(format!(
                            "{summary} · export needs a compiled render and none could be requested: {error}"
                        ));
                    }
                }
                cx.notify();
                return;
            }
            Err(error) => {
                self.project_io_status = ProjectIoStatus::Failed(format!("{summary} · {error}"));
                cx.notify();
                return;
            }
        };
        self.pending_export_destination = None;
        self.project_io_status = ProjectIoStatus::Exporting(destination.clone());
        let revision = job.revision();
        let cancellation = RenderCancellation::new();
        let render = cx.background_spawn(async move { job.execute(&cancellation) });
        let failure_prefix = summary;
        cx.spawn(async move |this, cx| {
            let completion = match render.await {
                Ok(completion) => completion,
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.project_io_status =
                            ProjectIoStatus::Failed(format!("{failure_prefix} · {error}"));
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
                        options.wav_request(destination.clone()),
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
                        this.project_io_status =
                            ProjectIoStatus::Failed(format!("{failure_prefix} · {error}"));
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
                    Err(error) => ProjectIoStatus::Failed(format!("{failure_prefix} · {error}")),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Every scope this project can export, in authored order: the master
    /// output, each mixer bus, each arrangement track. The token is what the
    /// control socket accepts; the label is what a musician reads.
    pub(super) fn export_scope_choices(&self, cx: &App) -> Vec<ExportScopeChoice> {
        let mut choices = vec![ExportScopeChoice {
            scope: RenderScope::Master,
            token: "master".to_owned(),
            label: "master".to_owned(),
        }];
        let Ok(snapshot) = self.session.read(cx).project_snapshot() else {
            return choices;
        };
        let domains = &snapshot.project.state().domains;
        for bus in domains.mixer.buses() {
            let id = bus.id().get();
            choices.push(ExportScopeChoice {
                scope: RenderScope::Bus {
                    bus: id,
                    tap: BusTap::Output,
                },
                token: format!("bus:{id}"),
                label: format!("bus {}", bus.name()),
            });
        }
        let arrangement = &domains.arrangement;
        for track in arrangement
            .track_order
            .iter()
            .filter_map(|id| arrangement.tracks.get(id))
        {
            let id = track.id.get();
            choices.push(ExportScopeChoice {
                scope: RenderScope::Track(id),
                token: format!("track:{id}"),
                label: format!("track {}", track.name),
            });
        }
        choices
    }

    pub(super) fn export_scope_label(&self, scope: &RenderScope, cx: &App) -> String {
        self.export_scope_choices(cx)
            .into_iter()
            .find(|choice| &choice.scope == scope)
            .map_or_else(|| format!("{scope:?}"), |choice| choice.label)
    }

    /// The ranges the export can resolve right now, in seconds.
    pub(super) fn export_range_availability(&self, cx: &App) -> ExportRangeAvailability {
        let seconds = |range| {
            self.export_span_for_range(range, cx)
                .ok()
                .map(|span| self.export_span_seconds(span, cx))
        };
        ExportRangeAvailability {
            project: seconds(ExportRange::Project),
            loop_range: seconds(ExportRange::Loop),
            selection: seconds(ExportRange::Selection),
        }
    }

    /// Turn socket-supplied overrides into a complete set of options, refusing
    /// a scope this project does not have by naming the ones it does.
    pub(super) fn resolve_export_options(
        &self,
        overrides: &crate::control_socket::ExportOverrides,
        cx: &App,
    ) -> Result<ExportOptions, String> {
        let mut options = ExportOptions::default();
        if let Some(bits) = overrides.bits {
            if !options.set_bits(bits) {
                return Err(format!("bits must be 16, 24, or 32; got {bits}"));
            }
        }
        if let Some(dither) = overrides.dither {
            options.set_dither_enabled(dither);
        }
        if let Some(gain_db) = overrides.gain_db {
            if !gain_db.is_finite() {
                return Err("gain_db must be finite".to_owned());
            }
            options.set_gain_db(gain_db);
        }
        if let Some(range) = overrides.range {
            options.range = range;
        }
        if let Some(scope) = overrides.scope.clone() {
            let choices = self.export_scope_choices(cx);
            if !choices.iter().any(|choice| choice.scope == scope) {
                let known = choices
                    .iter()
                    .map(|choice| choice.token.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!(
                    "no such export scope in this project; it has {known}"
                ));
            }
            options.scope = scope;
        }
        Ok(options)
    }

    /// What the export will be, in one line: `bus Drums · loop 60.0–68.0 s ·
    /// 16-bit`.
    pub(super) fn export_summary(&self, options: &ExportOptions, cx: &App) -> String {
        let scope = self.export_scope_label(&options.scope, cx);
        let seconds = self
            .export_span_for_range(options.range, cx)
            .ok()
            .map(|span| self.export_span_seconds(span, cx));
        options.summary(&scope, seconds)
    }

    fn export_span_for_range(&self, range: ExportRange, cx: &App) -> Result<RenderSpan, String> {
        match range {
            ExportRange::Project => {
                let snapshot = self
                    .session
                    .read(cx)
                    .project_snapshot()
                    .map_err(|error| error.to_string())?;
                let range = snapshot
                    .project
                    .state()
                    .domains
                    .arrangement
                    .project_range()
                    .ok_or_else(|| "The arrangement is empty".to_owned())?;
                RenderSpan::new(range.start.get().min(0), range.end.get())
                    .map_err(|error| error.to_string())
            }
            ExportRange::Loop => {
                let loop_range = self
                    .loop_range
                    .ok_or_else(|| "there is no loop range to export".to_owned())?;
                export_sample_span(loop_range.start.0, loop_range.end.0)
            }
            ExportRange::Selection => {
                let selection = self
                    .timeline_selection
                    .ok_or_else(|| "there is no time selection to export".to_owned())?;
                export_sample_span(selection.start.0, selection.end.0)
            }
            ExportRange::Custom { start, end } => {
                let start =
                    i64::try_from(start).map_err(|_| "range start is too large".to_owned())?;
                let end = i64::try_from(end).map_err(|_| "range end is too large".to_owned())?;
                export_sample_span(start, end)
            }
        }
    }

    fn export_span_seconds(&self, span: RenderSpan, cx: &App) -> (f64, f64) {
        let rate = self.export_sample_rate(cx);
        (span.start as f64 / rate, span.end as f64 / rate)
    }

    /// The project's own rate when there is a project, else the loaded
    /// material's. Never zero: this only scales a displayed number.
    fn export_sample_rate(&self, cx: &App) -> f64 {
        self.session
            .read(cx)
            .project_snapshot()
            .ok()
            .map(|snapshot| snapshot.project.state().domains.arrangement.sample_rate)
            .filter(|rate| *rate != 0)
            .or_else(|| self.analysis().map(|analysis| analysis.sample_rate))
            .map_or(1.0, f64::from)
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

fn export_sample_span(start: i64, end: i64) -> Result<RenderSpan, String> {
    RenderSpan::new(start, end).map_err(|error| error.to_string())
}

/// A suggested filename that says what the file is: `audec-bus-drums-loop`.
fn export_file_stem(scope_label: &str, range: ExportRange) -> String {
    let mut stem = String::from("audec");
    for word in [scope_label, range.label()] {
        let mut previous_dash = true;
        for character in word.chars() {
            if character.is_ascii_alphanumeric() {
                if previous_dash {
                    stem.push('-');
                    previous_dash = false;
                }
                stem.push(character.to_ascii_lowercase());
            } else {
                previous_dash = true;
            }
        }
    }
    stem
}
