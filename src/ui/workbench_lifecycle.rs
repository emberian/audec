//! Workbench construction, material loading, and analysis installation.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

impl Workbench {
    pub fn new(initial_path: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let session = cx.new(|_| {
            ProjectSession::new(ProjectSessionId(1))
                .expect("the application project session ID is non-zero")
        });
        let session_events = session.read(cx).subscribe(ProjectEventFilter::ALL);
        let reverse_surface_events = Arc::new(Mutex::new(Vec::new()));
        let reverse_surface_callback_events = Arc::clone(&reverse_surface_events);
        let reverse_surface_store = Arc::new(Mutex::new(ReverseSurfaceStore::new()));
        let reverse_surface_factory = ReverseSurfaceViewFactory::new(
            Arc::clone(&reverse_surface_store),
            Arc::new(move |event| {
                if let Ok(mut events) = reverse_surface_callback_events.lock() {
                    events.push(event);
                }
            }),
        );
        let reverse_analysis_result_events = Arc::new(Mutex::new(Vec::new()));
        let reverse_analysis_callback_events = Arc::clone(&reverse_analysis_result_events);
        reverse_surface_factory.set_analysis_result_callback(
            Arc::new(move |event| {
                if let Ok(mut events) = reverse_analysis_callback_events.lock() {
                    events.push(event);
                }
            }),
            cx,
        );
        let explanation_workbench_events = Arc::new(Mutex::new(Vec::new()));
        let explanation_callback_events = Arc::clone(&explanation_workbench_events);
        let explanation_workbench_factory =
            ExplanationWorkbenchViewFactory::new(Arc::new(move |source, event| {
                if let Ok(mut events) = explanation_callback_events.lock() {
                    events.push(PendingExplanationWorkbenchEvent { source, event });
                }
            }));
        let (render_tile_cache, render_cache_error) = match open_application_tile_cache() {
            Ok(cache) => (Some(Arc::new(Mutex::new(cache))), None),
            Err(error) => (
                None,
                Some(format!(
                    "Persistent render cache is unavailable; audition will render normally · {error}"
                )),
            ),
        };
        let mut audio_controller = ProjectAudioController::new();
        audio_controller.set_tile_product_cache(render_tile_cache.clone());
        let ticker = cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(33))
                .await;
            if this
                .update(cx, |this, cx| {
                    this.handle_asset_events(cx);
                    this.handle_arrangement_events(cx);
                    this.handle_arrangement_timeline_events(cx);
                    this.handle_sample_actions(cx);
                    this.handle_control_actions(cx);
                    this.handle_pattern_auditions(cx);
                    this.handle_reading_query_effects(cx);
                    this.handle_explanation_workbench_events(cx);
                    this.handle_reverse_surface_events(cx);
                    this.handle_reverse_analysis_result_events(cx);
                    this.handle_session_events(cx);
                    this.sync_active_sampler_selection(cx);
                    this.tick_project_audio(cx);
                    this.refresh_reverse_promotion_waits(cx);
                    this.refresh_explanation_render_waits(cx);
                    this.maybe_autosave(cx);
                    if this
                        .audio
                        .as_ref()
                        .is_some_and(|audio| !audio.preview_active())
                    {
                        this.preview_controller.observe_bus_idle();
                    }
                    let Some((next, frame, playback, playing)) = this.audio.as_ref().map(|audio| {
                        let transport = audio.transport();
                        let snapshot = this
                            .audio_controller
                            .transport_session()
                            .snapshot()
                            .transport;
                        (
                            transport.format().seconds_at_frame(snapshot.frame),
                            snapshot.frame.0,
                            timeline_playback_mode(snapshot.mode),
                            snapshot.mode == TransportMode::Playing,
                        )
                    }) else {
                        return;
                    };
                    this.dispatch_timeline_event(
                        TimelineInteractionEvent::TransportObserved {
                            playhead: TimelinePoint(frame),
                            mode: playback,
                        },
                        cx,
                    );
                    if playing || (next - this.playhead_seconds).abs() > 0.001 {
                        this.playhead_seconds = next;
                        this.sync_arrangement_playhead(playing, cx);
                        this.sync_pattern_placement_frame(cx);
                        cx.notify();
                    }
                })
                .is_err()
            {
                break;
            }
        });

        let mut workbench = Self {
            state: ProjectState::Empty,
            spectrogram: None,
            spectrogram_detail: None,
            spectrogram_detail_key: None,
            spectrogram_cancellation: None,
            spectrogram_generation: 0,
            spectrogram_refining: false,
            arrangement_view: None,
            arrangement_events: Arc::new(Mutex::new(Vec::new())),
            arrangement_timeline_events: Arc::new(Mutex::new(Vec::new())),
            sample_actions: Arc::new(Mutex::new(Vec::new())),
            sample_focuses: Arc::new(Mutex::new(Vec::new())),
            object_reveals: Arc::new(Mutex::new(Vec::new())),
            reverse_surface_events,
            reverse_analysis_result_events,
            analysis_pcm_products: BTreeMap::new(),
            analysis_derived_pcm_products: BTreeMap::new(),
            loom_construction_products: BTreeMap::new(),
            reverse_surface_store,
            reverse_surface_factory,
            reverse_promotion_waits: BTreeMap::new(),
            explanation_workbench_events,
            explanation_workbench_factory,
            explanation_cancellations: BTreeMap::new(),
            explanation_render_waits: BTreeMap::new(),
            comparison_executor: ComparisonProductExecutor::new(),
            control_actions: Arc::new(Mutex::new(Vec::new())),
            pattern_workflows: Arc::new(Mutex::new(Vec::new())),
            pattern_auditions: Arc::new(Mutex::new(Vec::new())),
            pattern_audition: PatternAuditionSessionAdapter::default(),
            pattern_audition_owner: None,
            reading_query_effects: Rc::new(RefCell::new(Vec::new())),
            reading_query_documents: BTreeMap::new(),
            reading_audition_generations: BTreeMap::new(),
            reading_comparison_controllers: BTreeMap::new(),
            sequencer_view: None,
            mixer_view: None,
            automation_view: None,
            asset_registry: Arc::new(Mutex::new(AssetRegistry::new())),
            asset_view: None,
            asset_events: Arc::new(Mutex::new(Vec::new())),
            session,
            session_events,
            pane_session_binding: PaneSessionBinding::new(),
            workspace_panes: BTreeMap::new(),
            active_workspace_view: None,
            sampler_selection_cache: BTreeMap::new(),
            project_lifecycle: ProjectDocumentLifecycle::new(),
            project_io_status: ProjectIoStatus::Idle,
            open_generation: 0,
            analysis_runtime: AnalysisProductRuntime::default(),
            component_analysis_generation: 0,
            component_analysis_cancellation: None,
            component_analysis_pending: false,
            save_generation: 0,
            autosave_last_attempt: Instant::now(),
            autosave_in_flight: false,
            pending_export_destination: None,
            pending_workspace_import: None,
            audition_audio: None,
            audio: None,
            audio_controller,
            render_tile_cache,
            preview_controller: PreviewController::default(),
            pad_preview_tickets: BTreeMap::new(),
            audio_render_cancellation: None,
            audio_snapshot_digest: None,
            audio_rendering: false,
            audio_error: render_cache_error,
            audio_device_status: None,
            constructive_status: None,
            primary_source_timeline_aligned: false,
            playhead_seconds: 0.0,
            timeline_bounds: Arc::new(Mutex::new(None)),
            timeline_waveform_geometry: Arc::new(Mutex::new(WaveformGeometryCache::default())),
            timeline_interaction: TimelineInteraction::new(
                TimelineControllerId(WorkspaceViewId::TRACK_OVERVIEW.0),
                0,
                TimelinePoint::ZERO,
                1,
                1,
            ),
            timeline_viewport: TimelineViewport::fit(0),
            timeline_follow: true,
            timeline_selection: None,
            timeline_signal: SignalLayer::Source,
            loop_range: None,
            loop_enabled: false,
            material_rail_scroll: ScrollHandle::new(),
            inspector_rail_scroll: ScrollHandle::new(),
            product_shell_hosted: false,
            focus_handle: cx.focus_handle().tab_stop(true),
            _ticker: ticker,
        };
        if let Some(path) = initial_path {
            workbench.load_path(path, cx);
        }
        workbench
    }

    pub(super) fn fresh_audio_controller(&self) -> ProjectAudioController {
        let mut controller = ProjectAudioController::new();
        controller.set_tile_product_cache(self.render_tile_cache.clone());
        controller
    }

    pub(super) fn load_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.cancel_component_analysis();
        self.open_generation = self.open_generation.wrapping_add(1).max(1);
        let open_generation = self.open_generation;
        // Analysis is a candidate document until it completes. Keep the
        // current project, transport, repository, and workspace alive so a
        // corrupt or unsupported file cannot destroy the session it was
        // meant to replace.
        self.project_io_status = ProjectIoStatus::Opening(path.clone());
        cx.notify();

        let analysis_path = path.clone();
        let analysis = cx.background_spawn(async move {
            let fingerprint =
                std::fs::read(&analysis_path).map(|bytes| ContentFingerprint::from_bytes(&bytes));
            (analyze_file_base(&analysis_path), fingerprint)
        });
        cx.spawn(async move |this, cx| {
            let (result, fingerprint) = analysis.await;
            let _ = this.update(cx, |this, cx| {
                if this.open_generation != open_generation {
                    return;
                }
                match result {
                    Ok(analysis) => {
                        this.save_generation = this.save_generation.wrapping_add(1).max(1);
                        this.prepare_for_document_install(cx);
                        this.project_lifecycle = ProjectDocumentLifecycle::new();
                        this.install_analysis(analysis, fingerprint.ok(), cx);
                        this.project_io_status = ProjectIoStatus::Idle;
                    }
                    Err(error) => {
                        this.project_io_status = ProjectIoStatus::Failed(format!("{error:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn prepare_for_document_install(&mut self, cx: &mut Context<Self>) {
        self.reset_project_runtime_bridges(cx);
        if let Some(audio) = self.audio.as_ref() {
            self.preview_controller.cancel_all(audio);
        }
        self.pad_preview_tickets.clear();
        if let Some(audio) = self.audio.take() {
            audio.transport().stop();
        }
        self.audio_device_status = None;
        self.spectrogram = None;
        self.spectrogram_detail = None;
        self.spectrogram_detail_key = None;
        if let Some(cancellation) = self.spectrogram_cancellation.take() {
            cancellation.cancel();
        }
        self.spectrogram_generation = self.spectrogram_generation.wrapping_add(1);
        self.spectrogram_refining = false;
        self.arrangement_view = None;
        self.arrangement_events = Arc::new(Mutex::new(Vec::new()));
        self.arrangement_timeline_events = Arc::new(Mutex::new(Vec::new()));
        self.sample_actions = Arc::new(Mutex::new(Vec::new()));
        match self.sample_focuses.lock() {
            Ok(mut focuses) => focuses.clear(),
            Err(poisoned) => poisoned.into_inner().clear(),
        }
        match self.object_reveals.lock() {
            Ok(mut reveals) => reveals.clear(),
            Err(poisoned) => poisoned.into_inner().clear(),
        }
        self.active_workspace_view = None;
        self.sampler_selection_cache.clear();
        self.sequencer_view = None;
        self.mixer_view = None;
        self.automation_view = None;
        self.asset_registry = Arc::new(Mutex::new(AssetRegistry::new()));
        self.asset_view = None;
        self.pending_export_destination = None;
        self.pending_workspace_import = None;
        self.audition_audio = None;
        if let Some(cancellation) = self.audio_render_cancellation.take() {
            cancellation.cancel();
        }
        self.audio_controller = self.fresh_audio_controller();
        self.audio_snapshot_digest = None;
        self.audio_rendering = false;
        self.audio_error = None;
        self.constructive_status = None;
        self.primary_source_timeline_aligned = false;
        self.playhead_seconds = 0.0;
        self.timeline_interaction = TimelineInteraction::new(
            TimelineControllerId(WorkspaceViewId::TRACK_OVERVIEW.0),
            0,
            TimelinePoint::ZERO,
            1,
            1,
        );
        self.sync_timeline_presentation();
        self.timeline_signal = SignalLayer::Source;
        self.state = ProjectState::Empty;
    }

    pub(super) fn reset_project_runtime_bridges(&mut self, cx: &mut Context<Self>) {
        match self.explanation_workbench_events.lock() {
            Ok(mut events) => events.clear(),
            Err(poisoned) => poisoned.into_inner().clear(),
        }
        for cancellation in std::mem::take(&mut self.explanation_cancellations).into_values() {
            cancellation.cancel();
        }
        self.explanation_render_waits.clear();
        match self.reverse_surface_events.lock() {
            Ok(mut events) => events.clear(),
            Err(poisoned) => poisoned.into_inner().clear(),
        }
        self.reverse_promotion_waits.clear();
        for view in self.workspace_panes.keys().copied().collect::<Vec<_>>() {
            if let Some(controller) = self.reverse_surface_factory.controller(view) {
                let owner = controller
                    .lock()
                    .map(|controller| controller.owner())
                    .unwrap_or_else(|poisoned| poisoned.into_inner().owner());
                self.comparison_executor.cancel_owner(owner);
                let _ = self.audio_controller.stop_scoped_audition(owner);
            }
        }
        self.reverse_surface_factory.clear_documents(cx);
        self.analysis_pcm_products.clear();
        self.analysis_derived_pcm_products.clear();
        self.loom_construction_products.clear();
        self.control_actions = Arc::new(Mutex::new(Vec::new()));
        self.pattern_workflows = Arc::new(Mutex::new(Vec::new()));
        self.pattern_auditions = Arc::new(Mutex::new(Vec::new()));
        if let Some(owner) = self.pattern_audition_owner.take() {
            let session = self.session.clone();
            let _ = session.update(cx, |session, _| {
                self.pattern_audition
                    .stop(session, &mut self.audio_controller, owner)
            });
        }
        self.pattern_audition = PatternAuditionSessionAdapter::default();
        self.reading_query_effects.borrow_mut().clear();
        self.reading_query_documents.clear();
        for (&view, controller) in &self.reading_comparison_controllers {
            self.comparison_executor.cancel_owner(controller.owner());
            let _ = self
                .audio_controller
                .stop_scoped_audition(controller.owner());
            let _ = self
                .audio_controller
                .stop_scoped_audition(reading_audition_owner(view));
        }
        self.reading_comparison_controllers.clear();
        self.reading_audition_generations.clear();
    }

    pub(super) fn install_analysis(
        &mut self,
        analysis: Analysis,
        source_fingerprint: Option<ContentFingerprint>,
        cx: &mut Context<Self>,
    ) {
        let total_samples = analysis.waveform_pyramid.frame_count() as u64;
        let initial_span = u64::from(analysis.sample_rate)
            .saturating_mul(30)
            .min(total_samples);
        self.timeline_interaction = TimelineInteraction::new(
            TimelineControllerId(WorkspaceViewId::TRACK_OVERVIEW.0),
            total_samples,
            TimelinePoint::ZERO,
            initial_span,
            (u64::from(analysis.sample_rate) / 100).max(1),
        );
        self.sync_timeline_presentation();
        let image = Image::from_bytes(ImageFormat::Png, analysis.spectrogram_png.clone());
        self.spectrogram = Some(Arc::new(image));
        let analysis = Arc::new(analysis);
        let audio = u16::try_from(analysis.waveform_pyramid.channel_count())
            .map_err(|_| "source has too many channels for playback".to_owned())
            .and_then(|channels| {
                let format = AudioFormat::new(analysis.sample_rate, channels)
                    .map_err(|error| error.to_string())?;
                let project =
                    ProjectAudio::new(format, analysis.waveform_pyramid.shared_interleaved_pcm())
                        .map_err(|error| error.to_string())?;
                let pcm = PcmAsset::new(format, project.shared_interleaved())
                    .map_err(|error| error.to_string())?;
                Ok((project, pcm))
            });
        match audio {
            Ok((_project_audio, pcm)) => {
                match self.install_source_asset(&analysis, source_fingerprint) {
                    Some(asset) => {
                        let registry = self
                            .asset_registry
                            .lock()
                            .map(|registry| registry.clone())
                            .map_err(|_| "media pool lock poisoned".to_owned());
                        match registry.and_then(|registry| {
                            let mut metadata = SourceMaterialMetadata::new(
                                analysis.title.clone(),
                                "Source material",
                            );
                            metadata.initial_bpm = f64::from(analysis.rhythm.tempo_bpm);
                            LiveProject::from_analyzed_source_material(
                                metadata, registry, asset, pcm, &analysis,
                            )
                            .map_err(|error| error.to_string())
                        }) {
                            Ok(live_project) => {
                                if let Err(error) = self.session.update(cx, |session, _| {
                                    session.install(live_project, Some(Arc::clone(&analysis)))
                                }) {
                                    self.audio_error = Some(format!(
                                        "Project session initialization failed: {error}"
                                    ));
                                } else {
                                    self.primary_source_timeline_aligned = true;
                                }
                            }
                            Err(error) => {
                                self.audio_error =
                                    Some(format!("Live project initialization failed: {error}"));
                            }
                        }
                    }
                    None => {}
                }
            }
            Err(error) => self.audio_error = Some(error),
        }
        self.state = ProjectState::Ready(analysis);
        self.handle_session_events(cx);
        self.refresh_spectrogram_detail(cx);
        let base = match &self.state {
            ProjectState::Ready(analysis) => Some(Arc::clone(analysis)),
            _ => None,
        };
        if let Some(base) = base {
            self.start_component_analysis(base, cx);
        }
    }

    pub(super) fn cancel_component_analysis(&mut self) {
        if let Some(cancellation) = self.component_analysis_cancellation.take() {
            cancellation.cancel();
        }
        self.component_analysis_generation = self.component_analysis_generation.wrapping_add(1);
        self.component_analysis_pending = false;
    }

    pub(super) fn start_component_analysis(&mut self, base: Arc<Analysis>, cx: &mut Context<Self>) {
        self.cancel_component_analysis();
        let generation = self.component_analysis_generation;
        let open_generation = self.open_generation;
        let project_session = self.session.read(cx).id().0;
        self.component_analysis_pending = true;
        cx.notify();

        let preparing_base = Arc::clone(&base);
        let preparation = cx.background_spawn(async move {
            AnalysisProductRuntime::prepare_components(preparing_base)
        });
        cx.spawn(async move |this, cx| {
            let prepared = preparation.await;
            let ticket = match this.update(cx, |this, cx| {
                if this.component_analysis_generation != generation
                    || this.open_generation != open_generation
                {
                    return None;
                }
                let ticket = match prepared {
                    Ok(prepared) => this.analysis_runtime.submit_prepared(
                        AnalysisProductOwner::components(project_session, generation),
                        prepared,
                    ),
                    Err(error) => Err(error),
                };
                match ticket {
                    Ok(ticket) => {
                        this.component_analysis_cancellation = Some(ticket.cancellation());
                        Some(ticket)
                    }
                    Err(error) => {
                        this.component_analysis_pending = false;
                        this.constructive_status = Some(format!(
                            "Source is ready; recurring-component analysis could not start · {error}"
                        ));
                        cx.notify();
                        None
                    }
                }
            }) {
                Ok(Some(ticket)) => ticket,
                _ => return,
            };
            let result = ticket.receive().await;
            let _ = this.update(cx, |this, cx| {
                if this.component_analysis_generation != generation
                    || this.open_generation != open_generation
                {
                    return;
                }
                this.component_analysis_cancellation = None;
                this.component_analysis_pending = false;
                match result {
                    Ok(completion) => {
                        let AnalysisProduct::Components(components) = completion.product.as_ref()
                        else {
                            this.constructive_status = Some(
                                "Source is ready; analysis runtime returned the wrong product kind"
                                    .into(),
                            );
                            cx.notify();
                            return;
                        };
                        let Some(current) = this.analysis() else {
                            return;
                        };
                        if current.path != base.path {
                            return;
                        }
                        let mut enriched = current.clone();
                        enriched.components = Some(components.as_ref().clone());
                        let enriched = Arc::new(enriched);
                        this.state = ProjectState::Ready(Arc::clone(&enriched));
                        let session = this.session.clone();
                        session.update(cx, |session, _| {
                            session.replace_analysis_snapshot(Arc::clone(&enriched))
                        });
                        let publication = (|| {
                            let end = i64::try_from(enriched.mono_pcm.len()).map_err(|_| {
                                "component source exceeds the signed project timeline".to_owned()
                            })?;
                            let span = RenderSpan::new(0, end).map_err(|error| error.to_string())?;
                            let source = this.capture_pane_source(
                                span,
                                enriched.sample_rate,
                                &enriched.mono_pcm,
                                cx,
                            )?;
                            let descriptor = components_artifact_descriptor(
                                &enriched.mono_pcm,
                                &source,
                                components.as_ref(),
                            )?;
                            let cancellation = RenderCancellation::new();
                            let findings = this
                                .session
                                .update(cx, |session, _| {
                                    session.publish_components_evidence(
                                        descriptor.clone(),
                                        components.as_ref().clone(),
                                        &cancellation,
                                    )
                                })
                                .map_err(|error| error.to_string())?;
                            let registered = this.register_components_analysis_results(
                                &descriptor,
                                &findings,
                                &source,
                                cx,
                            )?;
                            let document_count = this.refresh_reverse_surface_documents(cx)?;
                            this.constructive_status = Some(format!(
                                "Published {registered} component magnitude Finding(s) across {document_count} reverse documents"
                            ));
                            Ok::<_, String>(())
                        })();
                        if let Err(error) = publication {
                            this.constructive_status = Some(format!(
                                "Source is ready; recurring-component evidence could not publish · {error}"
                            ));
                        }
                    }
                    Err(error)
                        if !matches!(
                            error,
                            crate::analysis_product_runtime::AnalysisProductError::Cancelled
                                | crate::analysis_product_runtime::AnalysisProductError::Rejected(
                                    _
                                )
                        ) =>
                    {
                        this.constructive_status = Some(format!(
                            "Source is ready; recurring-component analysis failed · {error}"
                        ));
                    }
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn install_source_asset(
        &mut self,
        analysis: &Analysis,
        source_fingerprint: Option<ContentFingerprint>,
    ) -> Option<crate::assets::AssetId> {
        let Some(content) = source_fingerprint else {
            self.audio_error =
                Some("Source loaded, but its asset fingerprint could not be read".into());
            return None;
        };
        let Ok(absolute) = AbsolutePath::parse(analysis.path.to_string_lossy().into_owned()) else {
            self.audio_error =
                Some("Source path is not absolute; media pool entry was omitted".into());
            return None;
        };
        let Ok(location) = AssetLocation::new(Some(absolute), None) else {
            self.audio_error = Some("Source has no usable asset route".into());
            return None;
        };
        let imported_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let metadata = DecodedAudioMetadata {
            sample_rate_hz: analysis.sample_rate,
            channels: analysis.channels.min(u32::from(u16::MAX)) as u16,
            frame_count: SampleFrames(analysis.waveform_pyramid.frame_count() as u64),
            container: analysis
                .path
                .extension()
                .and_then(|extension| extension.to_str())
                .map(str::to_ascii_lowercase),
            codec: Some("FLAC".into()),
            bit_depth: u16::try_from(analysis.bits_per_sample).ok(),
        };
        let provenance = AssetProvenance::new(
            imported_at_unix_ms,
            AssetOrigin::ImportedFile {
                importer: format!("audec {}", env!("CARGO_PKG_VERSION")),
            },
            location.clone(),
        );
        let mut registry = AssetRegistry::new();
        let registration = AssetRegistration {
            name: analysis.title.clone(),
            location,
            metadata,
            content,
            provenance,
            tags: BTreeSet::from(["imported".into(), "source-material".into()]),
            favorite: false,
        };
        match registry.register(registration) {
            Ok(asset) => {
                self.asset_registry = Arc::new(Mutex::new(registry));
                self.asset_view = None;
                Some(asset)
            }
            Err(error) => {
                self.audio_error = Some(format!("Source asset registration failed: {error}"));
                None
            }
        }
    }
}
