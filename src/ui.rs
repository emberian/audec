use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    actions, canvas, div, img, point, prelude::*, px, quad, relative, rgb, rgba, App, Bounds,
    Context, Entity, FocusHandle, Focusable, Image, ImageFormat, IntoElement, KeyBinding,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, PathBuilder,
    PathPromptOptions, Pixels, Render, ScrollWheelEvent, SharedString, Task, Window, WindowOptions,
};

use crate::analysis::{
    analyze_file, encode_spectrogram, encode_spectrogram_field, spectral_projection, Analysis,
    FeatureFrame, OnsetEvent, RhythmAnalysis, WaveformBin, MAX_FREQUENCY, MIN_FREQUENCY,
};
use crate::arrangement::{
    ArrangementEditor, AssetId as ArrangementAssetId, Frame as ArrangementFrame,
    FrameRange as ArrangementFrameRange, Selection as ArrangementSelection,
    SourceRange as ArrangementSourceRange, TrackKind,
};
use crate::arrangement_view::{
    ArrangementView, ArrangementViewEvent, ArrangementWaveformProvider, ArrangementWaveformSource,
};
use crate::asset_view::{AssetBrowserEvent, AssetBrowserView};
use crate::assets::{
    AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration, AssetRegistry,
    ContentFingerprint, DecodedAudioMetadata, SampleFrames,
};
use crate::audio::{AudioFormat, FrameRange, ProjectAudio, ProjectFrame, TransportMode};
use crate::audio_host::{AudioHost, AuditionClip};
use crate::control_views::{AutomationView, MixerView};
use crate::daw_engine::DawEngineConfig;
use crate::daw_render::{PcmAsset, RenderCancellation};
use crate::decomposition::ComponentDecomposition;
use crate::hpss::{separate_harmonic_percussive, HpssResult, HpssSettings};
use crate::live_project::{LiveProject, SourceMaterialMetadata};
use crate::loom::{EventObservation, FitMetrics, SequenceSketch, TemplateBuildConfig};
use crate::rhythm::{
    analyze_mono as deproject_rhythm, AnalysisStatus as RhythmAnalysisStatus,
    RhythmConfig as RhythmDeprojectionConfig, RhythmDeprojection, SampleSpan, TempoRelation,
};
use crate::sequencer::PatternContent;
use crate::sequencer_view::{SequencerEditor, SequencerEditorSource};
use crate::session::{Sample, SampleRange};
use crate::settings::SpectrumSettings;
use crate::spectral_tiles::{
    compute_spectral_tile, FrameRange as SpectralFrameRange, FrequencyRange, SourceStamp,
    SpectralCancellation, SpectralRecipe, SpectralTileKey, SpectralTilePlanner,
    SpectralTileRequest,
};
use crate::timeline::TimelineViewport;
use crate::waveform_proxy::WaveformAssetKey;
use crate::workspace::{BuiltinView, WorkspaceLayout, WorkspaceModel};
use crate::workspace_document::WorkspaceDocument;
use crate::workspace_ui::{
    DynamicWorkspaceBootstrap, DynamicWorkspaceHooks, DynamicWorkspaceRoot,
    DynamicWorkspaceUiEvent, PaneRegistry,
};

actions!(
    audec,
    [
        OpenAudio,
        QuitAudec,
        TogglePlayback,
        SeekBackward,
        SeekForward,
        OpenWaterfall,
        OpenRhythm,
        OpenComponents,
        OpenSeparation,
        OpenLoom,
        OpenArrangementEditor,
        OpenSequencerEditor,
        OpenMixer,
        OpenAutomation,
        OpenAssets,
        ViewZoomIn,
        ViewZoomOut,
        ViewPanLeft,
        ViewPanRight,
        ViewFit,
        ViewFollow,
        SetLoopFromSelection,
        ToggleLoop,
    ]
);

const BACKGROUND: u32 = 0x090b10;
const PANEL: u32 = 0x10141d;
const PANEL_ALT: u32 = 0x0d1118;
const BORDER: u32 = 0x252c38;
const TEXT: u32 = 0xe8edf5;
const MUTED: u32 = 0x8c98a9;
const DIM: u32 = 0x596579;
const CYAN: u32 = 0x50d8d7;
const MAGENTA: u32 = 0xf172b6;
const AMBER: u32 = 0xf6b760;
const LIME: u32 = 0xa7d877;
const ARRANGEMENT_GUTTER: f32 = 170.0;
const RHYTHM_GUTTER: f32 = 260.0;
const RHYTHM_ROW_HEIGHT: f32 = 58.0;
const RHYTHM_MAX_VISIBLE_FAMILIES: usize = 5;

pub fn init_theme(cx: &mut App) {
    use guise::prelude::Theme;

    Theme::dark()
        .with_body(rgb(BACKGROUND))
        .with_surface(rgb(PANEL))
        .with_surface_hover(rgb(BORDER))
        .with_text(rgb(TEXT))
        .with_dimmed(rgb(MUTED))
        .with_border(rgb(BORDER))
        .with_primary(rgb(CYAN))
        .init(cx);
}

pub fn bind_keys(cx: &mut App) {
    cx.on_action(|_: &QuitAudec, cx| cx.quit());
    cx.bind_keys([
        KeyBinding::new("cmd-q", QuitAudec, None),
        KeyBinding::new("cmd-o", OpenAudio, Some("Audec")),
        KeyBinding::new("space", TogglePlayback, Some("Audec")),
        KeyBinding::new("left", SeekBackward, Some("Audec")),
        KeyBinding::new("right", SeekForward, Some("Audec")),
        KeyBinding::new("cmd-1", OpenWaterfall, Some("Audec")),
        KeyBinding::new("cmd-2", OpenRhythm, Some("Audec")),
        KeyBinding::new("cmd-3", OpenComponents, Some("Audec")),
        KeyBinding::new("cmd-4", OpenSeparation, Some("Audec")),
        KeyBinding::new("cmd-5", OpenLoom, Some("Audec")),
        KeyBinding::new("cmd-6", OpenArrangementEditor, Some("Audec")),
        KeyBinding::new("cmd-7", OpenSequencerEditor, Some("Audec")),
        KeyBinding::new("cmd-8", OpenMixer, Some("Audec")),
        KeyBinding::new("cmd-9", OpenAutomation, Some("Audec")),
        KeyBinding::new("cmd-b", OpenAssets, Some("Audec")),
        KeyBinding::new("=", ViewZoomIn, Some("Audec")),
        KeyBinding::new("-", ViewZoomOut, Some("Audec")),
        KeyBinding::new("shift-left", ViewPanLeft, Some("Audec")),
        KeyBinding::new("shift-right", ViewPanRight, Some("Audec")),
        KeyBinding::new("0", ViewFit, Some("Audec")),
        KeyBinding::new("f", ViewFollow, Some("Audec")),
        KeyBinding::new("cmd-l", SetLoopFromSelection, Some("Audec")),
        KeyBinding::new("l", ToggleLoop, Some("Audec")),
        KeyBinding::new("space", TogglePlayback, Some("AudecLens")),
        KeyBinding::new("left", SeekBackward, Some("AudecLens")),
        KeyBinding::new("right", SeekForward, Some("AudecLens")),
        KeyBinding::new("=", ViewZoomIn, Some("AudecLens")),
        KeyBinding::new("-", ViewZoomOut, Some("AudecLens")),
        KeyBinding::new("shift-left", ViewPanLeft, Some("AudecLens")),
        KeyBinding::new("shift-right", ViewPanRight, Some("AudecLens")),
        KeyBinding::new("0", ViewFit, Some("AudecLens")),
        KeyBinding::new("f", ViewFollow, Some("AudecLens")),
    ]);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VizKind {
    Waterfall,
    Rhythm,
    Components,
    Separation,
    Loom,
}

impl VizKind {
    fn title(self) -> &'static str {
        match self {
            Self::Waterfall => "Spectral waterfall",
            Self::Rhythm => "Event recurrence / pulse hypotheses",
            Self::Components => "Recurring component hypotheses",
            Self::Separation => "Harmonic / transient decomposition",
            Self::Loom => "Loom — editable event reconstruction",
        }
    }
}

enum ProjectState {
    Empty,
    Loading(PathBuf),
    Ready(Arc<Analysis>),
    Failed(String),
}

pub struct Workbench {
    state: ProjectState,
    spectrogram: Option<Arc<Image>>,
    spectrogram_detail: Option<Arc<Image>>,
    spectrogram_detail_key: Option<SpectralTileKey>,
    spectrogram_cancellation: Option<SpectralCancellation>,
    spectrogram_generation: u64,
    spectrogram_refining: bool,
    arrangement_view: Option<Entity<ArrangementView>>,
    arrangement_events: Arc<Mutex<Vec<ArrangementViewEvent>>>,
    /// Controller-bound arrangement intents retained until the command adapter
    /// can translate them without bypassing aggregate project ownership.
    pending_arrangement_events: Vec<ArrangementViewEvent>,
    sequencer_view: Option<Entity<SequencerEditor>>,
    mixer_view: Option<Entity<MixerView>>,
    automation_view: Option<Entity<AutomationView>>,
    asset_registry: Arc<Mutex<AssetRegistry>>,
    asset_view: Option<Entity<AssetBrowserView>>,
    asset_events: Arc<Mutex<Vec<AssetBrowserEvent>>>,
    source_audition: Option<AuditionClip>,
    live_project: Option<LiveProject>,
    audio: Option<AudioHost>,
    audio_project_revision: Option<u64>,
    audio_render_generation: u64,
    audio_rendering: bool,
    audio_error: Option<String>,
    playhead_seconds: f64,
    timeline_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    timeline_viewport: TimelineViewport,
    timeline_follow: bool,
    timeline_selection: Option<SampleRange>,
    loop_range: Option<SampleRange>,
    loop_enabled: bool,
    selection_anchor: Option<u64>,
    focus_handle: FocusHandle,
    _ticker: Task<()>,
}

impl Workbench {
    pub fn new(initial_path: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let ticker = cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(33))
                .await;
            if this
                .update(cx, |this, cx| {
                    this.handle_asset_events(cx);
                    this.handle_arrangement_events(cx);
                    let Some((next, playing)) = this.audio.as_ref().map(|audio| {
                        let transport = audio.transport();
                        let snapshot = transport.snapshot();
                        (
                            transport.format().seconds_at_frame(snapshot.frame),
                            snapshot.mode == TransportMode::Playing,
                        )
                    }) else {
                        return;
                    };
                    if playing || (next - this.playhead_seconds).abs() > 0.001 {
                        this.playhead_seconds = next;
                        if this.timeline_follow {
                            let playhead_sample = this.playhead_sample();
                            if this.timeline_viewport.ensure_visible(playhead_sample, 0.16) {
                                this.refresh_spectrogram_detail(cx);
                            }
                        }
                        this.sync_arrangement_playhead(playing, cx);
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
            pending_arrangement_events: Vec::new(),
            sequencer_view: None,
            mixer_view: None,
            automation_view: None,
            asset_registry: Arc::new(Mutex::new(AssetRegistry::new())),
            asset_view: None,
            asset_events: Arc::new(Mutex::new(Vec::new())),
            source_audition: None,
            live_project: None,
            audio: None,
            audio_project_revision: None,
            audio_render_generation: 0,
            audio_rendering: false,
            audio_error: None,
            playhead_seconds: 0.0,
            timeline_bounds: Arc::new(Mutex::new(None)),
            timeline_viewport: TimelineViewport::fit(0),
            timeline_follow: true,
            timeline_selection: None,
            loop_range: None,
            loop_enabled: false,
            selection_anchor: None,
            focus_handle: cx.focus_handle(),
            _ticker: ticker,
        };
        if let Some(path) = initial_path {
            workbench.load_path(path, cx);
        }
        workbench
    }

    fn load_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(audio) = self.audio.take() {
            audio.transport().stop();
        }
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
        self.pending_arrangement_events.clear();
        self.sequencer_view = None;
        self.mixer_view = None;
        self.automation_view = None;
        self.asset_registry = Arc::new(Mutex::new(AssetRegistry::new()));
        self.asset_view = None;
        self.source_audition = None;
        self.live_project = None;
        self.audio_project_revision = None;
        self.audio_render_generation = self.audio_render_generation.wrapping_add(1);
        self.audio_rendering = false;
        self.audio_error = None;
        self.playhead_seconds = 0.0;
        self.timeline_viewport = TimelineViewport::fit(0);
        self.timeline_follow = true;
        self.timeline_selection = None;
        self.loop_range = None;
        self.loop_enabled = false;
        self.selection_anchor = None;
        self.state = ProjectState::Loading(path.clone());
        cx.notify();

        let analysis = cx.background_spawn(async move {
            let fingerprint =
                std::fs::read(&path).map(|bytes| ContentFingerprint::from_bytes(&bytes));
            (analyze_file(&path), fingerprint)
        });
        cx.spawn(async move |this, cx| {
            let (result, fingerprint) = analysis.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(analysis) => this.install_analysis(analysis, fingerprint.ok(), cx),
                    Err(error) => {
                        this.state = ProjectState::Failed(format!("{error:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn install_analysis(
        &mut self,
        analysis: Analysis,
        source_fingerprint: Option<ContentFingerprint>,
        cx: &mut Context<Self>,
    ) {
        let total_samples = analysis.waveform_pyramid.frame_count() as u64;
        let initial_span = u64::from(analysis.sample_rate)
            .saturating_mul(30)
            .min(total_samples);
        self.timeline_viewport = TimelineViewport::around(total_samples, 0, initial_span);
        self.timeline_viewport.minimum_span = (u64::from(analysis.sample_rate) / 100).max(1);
        let image = Image::from_bytes(ImageFormat::Png, analysis.spectrogram_png.clone());
        self.spectrogram = Some(Arc::new(image));
        let audio = u16::try_from(analysis.waveform_pyramid.channel_count())
            .map_err(|_| "source has too many channels for playback".to_owned())
            .and_then(|channels| {
                let format = AudioFormat::new(analysis.sample_rate, channels)
                    .map_err(|error| error.to_string())?;
                let project =
                    ProjectAudio::new(format, analysis.waveform_pyramid.shared_interleaved_pcm())
                        .map_err(|error| error.to_string())?;
                let audition = AuditionClip::from_project_audio(project.clone())
                    .map_err(|error| error.to_string())?;
                let pcm = PcmAsset::new(format, project.shared_interleaved())
                    .map_err(|error| error.to_string())?;
                Ok((project, pcm, audition))
            });
        match audio {
            Ok((project_audio, pcm, audition)) => {
                self.source_audition = Some(audition);
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
                            LiveProject::from_source_material(metadata, registry, asset, pcm)
                                .map_err(|error| error.to_string())
                        }) {
                            Ok(live_project) => {
                                self.audio_project_revision = live_project
                                    .revisions()
                                    .ok()
                                    .map(|revision| revision.aggregate);
                                self.asset_registry = live_project.domains().assets;
                                self.live_project = Some(live_project);
                            }
                            Err(error) => {
                                self.audio_error =
                                    Some(format!("Live project initialization failed: {error}"));
                            }
                        }
                    }
                    None => {}
                }
                match AudioHost::open(project_audio) {
                    Ok(host) => self.audio = Some(host),
                    Err(error) => self.audio_error = Some(error.to_string()),
                }
            }
            Err(error) => self.audio_error = Some(error),
        }
        self.state = ProjectState::Ready(Arc::new(analysis));
        self.refresh_spectrogram_detail(cx);
    }

    fn install_source_asset(
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

    fn handle_asset_events(&mut self, cx: &mut Context<Self>) {
        let events = self
            .asset_events
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default();
        for event in events {
            match event {
                AssetBrowserEvent::Audition(asset)
                    if self
                        .asset_registry
                        .lock()
                        .is_ok_and(|registry| registry.get(asset).is_some()) =>
                {
                    if let (Some(audio), Some(clip)) = (&self.audio, &self.source_audition) {
                        audio.audition(clip.clone());
                    }
                }
                AssetBrowserEvent::Activate(asset)
                    if self
                        .asset_registry
                        .lock()
                        .is_ok_and(|registry| registry.get(asset).is_some()) =>
                {
                    self.open_arrangement_editor(cx);
                }
                _ => {}
            }
        }
    }

    fn handle_arrangement_events(&mut self, cx: &mut Context<Self>) {
        let events = self
            .arrangement_events
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default();
        for event in events {
            match event {
                ArrangementViewEvent::SeekRequested(frame) => {
                    self.seek_to_sample(u64::try_from(frame.get()).unwrap_or(0), cx);
                }
                // These are deliberately retained as semantic intents. The
                // legacy Workbench still has editors which mutate injected
                // domain handles, so claiming whole-project command ownership
                // here would strand those edits. The convergence adapter can
                // drain this queue into ProjectController atomically.
                pending @ (ArrangementViewEvent::Commit(_) | ArrangementViewEvent::Action(_)) => {
                    self.pending_arrangement_events.push(pending);
                }
            }
        }
    }

    fn sync_arrangement_playhead(&self, playing: bool, cx: &mut Context<Self>) {
        let Some(view) = self.arrangement_view.as_ref() else {
            return;
        };
        let playhead =
            ArrangementFrame::new(i64::try_from(self.playhead_sample()).unwrap_or(i64::MAX));
        view.update(cx, |view, cx| view.set_playhead(playhead, playing, cx));
    }

    fn arrangement_waveform_provider(
        &self,
        live_project: &LiveProject,
    ) -> Option<ArrangementWaveformProvider> {
        let analysis = self.analysis()?;
        let ids = live_project.primary_source_ids()?;
        let domains = live_project.domains();
        let registry = domains.assets.lock().ok()?;
        let media = registry.get(ids.registry_asset)?;
        let metadata = media.metadata();
        let key = WaveformAssetKey::new(
            ids.registry_asset,
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
            (asset == ids.arrangement_asset).then(|| source.clone())
        }))
    }

    fn choose_audio(&mut self, cx: &mut Context<Self>) {
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(SharedString::from("Analyze")),
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

    fn toggle_playback(&mut self, cx: &mut Context<Self>) {
        if self.audio_rendering {
            return;
        }
        if self.transport_is_playing() {
            if let Some(audio) = &self.audio {
                audio.transport().pause();
            }
            cx.notify();
            return;
        }
        let Some(live_project) = self.live_project.clone() else {
            if let Some(audio) = &self.audio {
                audio.stop_preview();
                audio.transport().toggle();
                cx.notify();
            }
            return;
        };
        let revision = match live_project.revisions() {
            Ok(revision) => revision.aggregate,
            Err(error) => {
                self.audio_error = Some(format!("Project validation failed: {error}"));
                cx.notify();
                return;
            }
        };
        if self.audio_project_revision == Some(revision) {
            if let Some(audio) = &self.audio {
                audio.stop_preview();
                audio.transport().play();
                cx.notify();
            }
            return;
        }

        if let Some(audio) = &self.audio {
            audio.stop_preview();
            audio.transport().pause();
        }
        self.audio_rendering = true;
        self.audio_error = None;
        self.audio_render_generation = self.audio_render_generation.wrapping_add(1);
        let generation = self.audio_render_generation;
        let playhead_seconds = self.playhead_seconds;
        cx.notify();

        let render = cx.background_spawn(async move {
            let cancellation = RenderCancellation::new();
            let schedule = live_project
                .compile_audition(&DawEngineConfig::default(), &cancellation)
                .map_err(|error| error.to_string())?;
            let revision = schedule.project_revision().aggregate;
            let rendered = schedule
                .render_for_audition(&cancellation)
                .map_err(|error| error.to_string())?;
            Ok::<_, String>((rendered.audio, revision))
        });
        cx.spawn(async move |this, cx| {
            let result = render.await;
            let _ = this.update(cx, |this, cx| {
                if this.audio_render_generation != generation {
                    return;
                }
                this.audio_rendering = false;
                match result {
                    Ok((project_audio, revision)) => match AudioHost::open(project_audio) {
                        Ok(host) => {
                            if let Err(error) = host.transport().seek_seconds(playhead_seconds) {
                                this.audio_error = Some(error.to_string());
                            }
                            this.audio = Some(host);
                            this.audio_project_revision = Some(revision);
                            this.sync_audio_loop();
                            if let Some(audio) = &this.audio {
                                audio.transport().play();
                            }
                        }
                        Err(error) => this.audio_error = Some(error.to_string()),
                    },
                    Err(error) => {
                        this.audio_error = Some(format!("Project render failed: {error}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn seek_to(&mut self, seconds: f64, cx: &mut Context<Self>) {
        let duration = self
            .analysis()
            .map_or(0.0, |analysis| analysis.duration_seconds);
        let seconds = seconds.clamp(0.0, duration);
        self.playhead_seconds = seconds;
        if let Some(audio) = &self.audio {
            audio.stop_preview();
            if let Err(error) = audio.transport().seek_seconds(seconds) {
                self.audio_error = Some(format!("{error:#}"));
            }
        }
        let playing = self
            .audio
            .as_ref()
            .is_some_and(|audio| audio.transport().snapshot().mode == TransportMode::Playing);
        self.sync_arrangement_playhead(playing, cx);
        cx.notify();
    }

    fn seek_relative(&mut self, delta: f64, cx: &mut Context<Self>) {
        self.seek_to(self.playhead_seconds + delta, cx);
    }

    fn total_samples(&self) -> u64 {
        self.analysis()
            .map_or(0, |analysis| analysis.waveform_pyramid.frame_count() as u64)
    }

    fn playhead_sample(&self) -> u64 {
        let Some(analysis) = self.analysis() else {
            return 0;
        };
        (self.playhead_seconds.max(0.0) * f64::from(analysis.sample_rate))
            .round()
            .clamp(0.0, self.total_samples() as f64) as u64
    }

    fn seconds_for_sample(&self, sample: u64) -> f64 {
        self.analysis().map_or(0.0, |analysis| {
            sample.min(self.total_samples()) as f64 / f64::from(analysis.sample_rate)
        })
    }

    fn visible_seconds(&self) -> (f64, f64) {
        (
            self.seconds_for_sample(self.timeline_viewport.start_sample),
            self.seconds_for_sample(self.timeline_viewport.end_sample),
        )
    }

    fn refresh_spectrogram_detail(&mut self, cx: &mut Context<Self>) {
        let target_width = self
            .timeline_bounds
            .lock()
            .unwrap()
            .as_ref()
            .map(|bounds| f32::from(bounds.size.width).round() as usize)
            .unwrap_or(1_200)
            .clamp(256, 4_096);
        let Some((mono, source, db_ceiling)) = self.analysis().map(|analysis| {
            let frame_count = analysis.waveform_pyramid.frame_count() as u64;
            (
                Arc::clone(&analysis.mono_pcm),
                SourceStamp {
                    content: stable_source_id(
                        &analysis.path.to_string_lossy(),
                        frame_count,
                        analysis.sample_rate,
                    ),
                    revision: 0,
                    generation: 0,
                    sample_rate: analysis.sample_rate,
                    frame_count,
                },
                analysis.spectral_peak_db,
            )
        }) else {
            return;
        };
        let request = SpectralTileRequest {
            source,
            frames: SpectralFrameRange::new(
                self.timeline_viewport.start_sample,
                self.timeline_viewport.end_sample,
            ),
            target_pixel_width: target_width,
            frequencies: FrequencyRange::logarithmic(MIN_FREQUENCY, MAX_FREQUENCY),
            recipe: SpectralRecipe {
                fft_size: 8_192,
                min_fft_size: 256,
                max_window_columns: 4,
                window: crate::settings::WindowFunction::Hann,
                frequency_bins: 256,
                db_ceiling,
                db_range: 84.0,
            },
        };
        let key = SpectralTilePlanner::default().plan(request).final_key;
        if self.spectrogram_detail_key == Some(key) && self.spectrogram_detail.is_some() {
            return;
        }

        if let Some(cancellation) = self.spectrogram_cancellation.take() {
            cancellation.cancel();
        }
        let cancellation = SpectralCancellation::default();
        self.spectrogram_cancellation = Some(cancellation.clone());
        self.spectrogram_generation = self.spectrogram_generation.wrapping_add(1);
        let generation = self.spectrogram_generation;
        self.spectrogram_detail = None;
        self.spectrogram_detail_key = None;
        self.spectrogram_refining = true;

        let task = cx.background_spawn(async move {
            let tile = compute_spectral_tile(&mono, key, &cancellation)
                .map_err(|error| error.to_string())?;
            let png = encode_spectrogram_field(
                &tile.db,
                tile.scalar.width,
                tile.scalar.height,
                key.db_ceiling,
                key.db_range,
            )
            .map_err(|error| format!("encoding spectral tile: {error:#}"))?;
            Ok::<_, String>((key, png))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.spectrogram_generation != generation {
                    return;
                }
                this.spectrogram_refining = false;
                match result {
                    Ok((key, png)) => {
                        this.spectrogram_detail =
                            Some(Arc::new(Image::from_bytes(ImageFormat::Png, png)));
                        this.spectrogram_detail_key = Some(key);
                    }
                    Err(error) if error != "spectral tile computation was cancelled" => {
                        eprintln!("refining workbench spectrum: {error}");
                    }
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn sample_from_x(&self, x: Pixels, clamp: bool) -> Option<u64> {
        let bounds = (*self.timeline_bounds.lock().unwrap())?;
        if bounds.size.width <= px(0.0) {
            return None;
        }
        let raw_fraction = f64::from((x - bounds.origin.x) / bounds.size.width);
        if !clamp && !(0.0..=1.0).contains(&raw_fraction) {
            return None;
        }
        Some(
            self.timeline_viewport
                .sample_at_fraction(raw_fraction.clamp(0.0, 1.0)),
        )
    }

    fn seek_to_sample(&mut self, sample: u64, cx: &mut Context<Self>) {
        self.seek_to(self.seconds_for_sample(sample), cx);
    }

    fn begin_timeline_selection(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some(sample) = self.sample_from_x(event.position.x, false) else {
            return;
        };
        self.selection_anchor = Some(sample);
        self.timeline_selection = Some(SampleRange::empty(Sample::new(sample as i64)));
        self.seek_from_pointer(event, cx);
    }

    fn extend_timeline_selection(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !event.dragging() {
            return;
        }
        let Some(anchor) = self.selection_anchor else {
            return;
        };
        let Some(sample) = self.sample_from_x(event.position.x, true) else {
            return;
        };
        self.timeline_selection = Some(SampleRange::new(
            Sample::new(anchor as i64),
            Sample::new(sample as i64),
        ));
        self.playhead_seconds = self.seconds_for_sample(sample);
        cx.notify();
    }

    fn end_timeline_selection(&mut self, _: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.selection_anchor.take().is_some() {
            cx.notify();
        }
    }

    fn navigate_timeline(&mut self, operation: impl FnOnce(&mut TimelineViewport)) {
        operation(&mut self.timeline_viewport);
        self.timeline_follow = false;
    }

    fn zoom_timeline(&mut self, anchor: u64, scale: f64, cx: &mut Context<Self>) {
        self.navigate_timeline(|viewport| viewport.zoom_around(anchor, scale));
        self.refresh_spectrogram_detail(cx);
        cx.notify();
    }

    fn pan_timeline(&mut self, fraction: f64, cx: &mut Context<Self>) {
        self.navigate_timeline(|viewport| viewport.pan_fraction(fraction));
        self.refresh_spectrogram_detail(cx);
        cx.notify();
    }

    fn fit_timeline(&mut self, cx: &mut Context<Self>) {
        let minimum_span = self.timeline_viewport.minimum_span;
        self.timeline_viewport = TimelineViewport::fit(self.total_samples());
        self.timeline_viewport.minimum_span = minimum_span;
        self.timeline_follow = false;
        self.refresh_spectrogram_detail(cx);
        cx.notify();
    }

    fn follow_timeline(&mut self, cx: &mut Context<Self>) {
        self.timeline_follow = true;
        let playhead = self.playhead_sample();
        self.timeline_viewport.ensure_visible(playhead, 0.16);
        self.refresh_spectrogram_detail(cx);
        cx.notify();
    }

    fn set_loop_from_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(selection) = self.timeline_selection.filter(|range| !range.is_empty()) {
            self.loop_range = Some(selection);
            self.loop_enabled = true;
            self.sync_audio_loop();
            cx.notify();
        }
    }

    fn toggle_loop(&mut self, cx: &mut Context<Self>) {
        if self.loop_range.is_none() {
            self.loop_range = self
                .timeline_selection
                .filter(|range| !range.is_empty())
                .or_else(|| {
                    let range = SampleRange::new(
                        Sample::new(self.timeline_viewport.start_sample as i64),
                        Sample::new(self.timeline_viewport.end_sample as i64),
                    );
                    (!range.is_empty()).then_some(range)
                });
        }
        if self.loop_range.is_some() {
            self.loop_enabled = !self.loop_enabled;
            self.sync_audio_loop();
            cx.notify();
        }
    }

    fn sync_audio_loop(&mut self) {
        let Some(audio) = &self.audio else {
            return;
        };
        let transport = audio.transport();
        let result = if let Some(range) = self.loop_range {
            let start = range.start.get().max(0) as u64;
            let end = range.end.get().max(0) as u64;
            match FrameRange::new(ProjectFrame(start), ProjectFrame(end)) {
                Ok(range) => transport.set_loop_region(Some(range)).map(|_| {
                    transport.set_loop_enabled(self.loop_enabled);
                }),
                Err(error) => Err(error),
            }
        } else {
            transport.set_loop_region(None)
        };
        if let Err(error) = result {
            self.audio_error = Some(error.to_string());
        }
    }

    fn audition_pcm(&mut self, samples: Vec<f32>, sample_rate: u32, cx: &mut Context<Self>) {
        if let Some(audio) = &self.audio {
            if let Err(error) = audio.audition_mono(sample_rate, samples) {
                self.audio_error = Some(error.to_string());
            }
            cx.notify();
        }
    }

    fn seek_from_pointer(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if let Some(sample) = self.sample_from_x(event.position.x, false) {
            self.seek_to_sample(sample, cx);
        }
    }

    fn analysis(&self) -> Option<&Analysis> {
        match &self.state {
            ProjectState::Ready(analysis) => Some(analysis),
            _ => None,
        }
    }

    fn transport_is_playing(&self) -> bool {
        self.audio
            .as_ref()
            .is_some_and(|audio| audio.transport().snapshot().mode == TransportMode::Playing)
    }

    fn playhead_fraction(&self) -> f32 {
        self.analysis()
            .map(|analysis| {
                (self.playhead_seconds / analysis.duration_seconds.max(f64::EPSILON)) as f32
            })
            .unwrap_or(0.0)
            .clamp(0.0, 1.0)
    }

    fn visible_playhead_fraction(&self) -> f32 {
        let sample = self.playhead_sample();
        if sample < self.timeline_viewport.start_sample
            || sample > self.timeline_viewport.end_sample
        {
            return -1.0;
        }
        self.timeline_viewport.fraction_of(sample)
    }

    fn current_feature(&self) -> Option<FeatureFrame> {
        let analysis = self.analysis()?;
        let index = (self.playhead_fraction() * analysis.features.len() as f32) as usize;
        analysis
            .features
            .get(index.min(analysis.features.len().saturating_sub(1)))
            .copied()
    }

    fn open_visualizer(&mut self, kind: VizKind, cx: &mut Context<Self>) {
        let workbench = cx.entity();
        let options = visualizer_window_options(kind, cx);
        // `open_window` renders its root synchronously. Defer until this action's
        // Workbench update lease has ended so the new view can safely observe it.
        cx.defer(move |cx| {
            if let Err(error) = cx.open_window(options, move |window, cx| {
                let visualizer = cx.new(|cx| Visualizer::new(kind, workbench, cx));
                window.focus(&visualizer.focus_handle(cx));
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

    fn open_arrangement_editor(&mut self, cx: &mut Context<Self>) {
        let editor = if let Some(editor) = &self.arrangement_view {
            editor.clone()
        } else if let Some(live_project) = &self.live_project {
            let domains = live_project.domains();
            let aggregate_revision = live_project
                .revisions()
                .ok()
                .map_or(0, |revision| revision.aggregate);
            let (bpm, beats_per_bar) = domains
                .sequencer
                .lock()
                .ok()
                .map(|sequencer| {
                    let map = sequencer.tempo_map();
                    let bpm = map
                        .tempo_points()
                        .first()
                        .map_or(120.0, |point| point.tempo.bpm());
                    let beats_per_bar = map.meter_points().first().map_or(4, |point| {
                        point.signature.numerator.min(u16::from(u8::MAX)) as u8
                    });
                    (bpm, beats_per_bar)
                })
                .unwrap_or((120.0, 4));
            let shared = domains.arrangement;
            let selection = shared
                .lock()
                .ok()
                .map(|editor| editor.selection.clone())
                .unwrap_or_else(ArrangementSelection::default);
            let events = Arc::clone(&self.arrangement_events);
            let callback = Arc::new(move |event| {
                if let Ok(mut events) = events.lock() {
                    events.push(event);
                }
            });
            let waveform_provider = self.arrangement_waveform_provider(live_project);
            let entity = cx.new(|cx| {
                ArrangementView::from_shared_sources(
                    shared,
                    aggregate_revision,
                    callback,
                    waveform_provider,
                    cx,
                )
            });
            let playhead =
                ArrangementFrame::new(i64::try_from(self.playhead_sample()).unwrap_or(i64::MAX));
            let playing = self
                .audio
                .as_ref()
                .is_some_and(|audio| audio.transport().snapshot().mode == TransportMode::Playing);
            entity.update(cx, |editor, cx| {
                editor.set_tempo(bpm, beats_per_bar, cx);
                editor.set_project_revision(aggregate_revision, cx);
                editor.set_selection(selection, cx);
                editor.set_playhead(playhead, playing, cx);
            });
            self.arrangement_view = Some(entity.clone());
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
            self.arrangement_view = Some(entity.clone());
            entity
        };
        let options = editor_window_options("Arrangement editor", cx);
        cx.defer(move |cx| {
            if let Err(error) = cx.open_window(options, move |window, cx| {
                window.focus(&editor.focus_handle(cx));
                editor.clone()
            }) {
                eprintln!("opening Arrangement editor: {error:#}");
            }
        });
    }

    fn open_sequencer_editor(&mut self, cx: &mut Context<Self>) {
        let editor = if let Some(editor) = &self.sequencer_view {
            editor.clone()
        } else if let Some(live_project) = &self.live_project {
            let domains = live_project.domains();
            let (note_pattern, step_pattern) = domains
                .sequencer
                .lock()
                .map(|sequencer| {
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
                })
                .unwrap_or((None, None));
            let source = SequencerEditorSource::new(
                domains.sequencer,
                note_pattern,
                step_pattern,
                "Project patterns",
            );
            let entity = cx.new(|cx| SequencerEditor::new(source, cx));
            self.sequencer_view = Some(entity.clone());
            entity
        } else {
            let editor = cx.new(SequencerEditor::demo);
            self.sequencer_view = Some(editor.clone());
            editor
        };
        open_editor_entity(editor, "Piano roll + drum sequencer", cx);
    }

    fn open_mixer(&mut self, cx: &mut Context<Self>) {
        let mixer = if let Some(mixer) = &self.mixer_view {
            mixer.clone()
        } else if let Some(live_project) = &self.live_project {
            let shared = live_project.domains().mixer;
            let entity = cx.new(|cx| MixerView::from_shared_graph(shared, cx));
            self.mixer_view = Some(entity.clone());
            entity
        } else {
            let mixer = cx.new(MixerView::demo);
            self.mixer_view = Some(mixer.clone());
            mixer
        };
        open_editor_entity(mixer, "Mixer", cx);
    }

    fn open_automation(&mut self, cx: &mut Context<Self>) {
        let automation = if let Some(automation) = &self.automation_view {
            automation.clone()
        } else if let Some(live_project) = &self.live_project {
            let shared = live_project.domains().automation;
            let entity = cx.new(|cx| AutomationView::from_shared_graph(shared, cx));
            self.automation_view = Some(entity.clone());
            entity
        } else {
            let automation = cx.new(AutomationView::demo);
            self.automation_view = Some(automation.clone());
            automation
        };
        open_editor_entity(automation, "Automation", cx);
    }

    fn open_assets(&mut self, cx: &mut Context<Self>) {
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
        open_editor_entity(browser, "Media pool", cx);
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_playing = self.transport_is_playing();
        let transport_enabled = self.audio.is_some() || self.live_project.is_some();
        let title = self
            .analysis()
            .map(|analysis| analysis.title.clone())
            .unwrap_or_else(|| "No material loaded".to_owned());
        let duration = self.audio.as_ref().map_or_else(
            || {
                self.analysis()
                    .map_or(0.0, |analysis| analysis.duration_seconds)
            },
            |audio| {
                let transport = audio.transport();
                transport.format().seconds_at_frame(transport.length())
            },
        );

        div()
            .h(px(54.0))
            .flex_none()
            .flex()
            .items_center()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(
                div()
                    .w(px(220.0))
                    .pl(px(82.0))
                    .pr_4()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .child(div().font_weight(gpui::FontWeight::BOLD).child("audec"))
                    .child(div().text_xs().text_color(rgb(MUTED)).child("reverse DAW")),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        div()
                            .id("seek-back")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .text_color(if transport_enabled {
                                rgb(TEXT)
                            } else {
                                rgb(DIM)
                            })
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| this.seek_relative(-5.0, cx)))
                            .child("−5s"),
                    )
                    .child(
                        div()
                            .id("play-pause")
                            .size(px(34.0))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(if transport_enabled {
                                rgb(TEXT)
                            } else {
                                rgb(BORDER)
                            })
                            .text_color(rgb(BACKGROUND))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_playback(cx)))
                            .child(if self.audio_rendering {
                                "…"
                            } else if is_playing {
                                "❚❚"
                            } else {
                                "▶"
                            }),
                    )
                    .child(
                        div()
                            .id("seek-forward")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .text_color(if transport_enabled {
                                rgb(TEXT)
                            } else {
                                rgb(DIM)
                            })
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| this.seek_relative(5.0, cx)))
                            .child("+5s"),
                    )
                    .child(
                        div()
                            .ml_3()
                            .min_w(px(92.0))
                            .text_sm()
                            .text_color(rgb(CYAN))
                            .child(format!(
                                "{} / {}",
                                format_time(self.playhead_seconds),
                                format_time(duration)
                            )),
                    )
                    .child(div().ml_2().text_sm().text_color(rgb(MUTED)).child(
                        if self.audio_rendering {
                            format!("{title} · rendering edits…")
                        } else {
                            title
                        },
                    )),
            )
            .child(
                div().w(px(220.0)).px_4().flex().justify_end().child(
                    div()
                        .id("open-audio")
                        .px_3()
                        .py_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(rgb(BORDER))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(BORDER)))
                        .on_click(cx.listener(|this, _, _, cx| this.choose_audio(cx)))
                        .child("Open audio…"),
                ),
            )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (album, title, path) = self.analysis().map_or_else(
            || {
                (
                    "SESSION".to_owned(),
                    "Empty".to_owned(),
                    "Choose a FLAC to begin".to_owned(),
                )
            },
            |analysis| {
                (
                    analysis.album.clone(),
                    analysis.title.clone(),
                    analysis.path.display().to_string(),
                )
            },
        );

        div()
            .w(px(220.0))
            .flex_none()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL_ALT))
            .p_3()
            .gap_2()
            .child(section_label("MATERIAL"))
            .child(
                div()
                    .mt_1()
                    .p_3()
                    .rounded_md()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_xs().text_color(rgb(MAGENTA)).child(album))
                    .child(div().text_sm().child(title))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(DIM))
                            .line_clamp(2)
                            .child(path),
                    ),
            )
            .child(section_label("LAYERS"))
            .child(layer_row("Stereo waveform", CYAN, true))
            .child(layer_row("Log-frequency energy", MAGENTA, true))
            .child(layer_row("Transient flux", AMBER, true))
            .child(layer_row("Pulse / onset evidence", CYAN, true))
            .child(layer_row("Stereo field", LIME, true))
            .child(section_label("EDIT / RECONSTRUCT"))
            .child(
                div()
                    .id("open-arrangement-editor")
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(CYAN))
                    .text_color(rgb(CYAN))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(BORDER)).text_color(rgb(TEXT)))
                    .on_click(cx.listener(|this, _, _, cx| this.open_arrangement_editor(cx)))
                    .child("Arrangement editor"),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .id("open-sequencer-editor")
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_sequencer_editor(cx)
                            }))
                            .child("Piano / drums"),
                    )
                    .child(
                        div()
                            .id("open-mixer")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| this.open_mixer(cx)))
                            .child("Mixer"),
                    ),
            )
            .child(
                div()
                    .id("open-automation")
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(BORDER)))
                    .on_click(cx.listener(|this, _, _, cx| this.open_automation(cx)))
                    .child("Automation editor"),
            )
            .child(
                div()
                    .id("open-assets")
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(BORDER)))
                    .on_click(cx.listener(|this, _, _, cx| this.open_assets(cx)))
                    .child("Media pool"),
            )
            .child(section_label("OPEN VIEWS"))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .id("open-waterfall")
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_visualizer(VizKind::Waterfall, cx)
                            }))
                            .child("Waterfall"),
                    )
                    .child(
                        div()
                            .id("open-rhythm")
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_visualizer(VizKind::Rhythm, cx)
                            }))
                            .child("Rhythm"),
                    ),
            )
            .child(
                div()
                    .id("open-components")
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(BORDER)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_visualizer(VizKind::Components, cx)
                    }))
                    .child("Components"),
            )
            .child(
                div()
                    .id("open-separation")
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(BORDER)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_visualizer(VizKind::Separation, cx)
                    }))
                    .child("Decompose selected span"),
            )
            .child(
                div()
                    .id("open-loom")
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(BORDER)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_visualizer(VizKind::Loom, cx)
                    }))
                    .child("Loom · reconstruct events"),
            )
            .child(
                div()
                    .mt_auto()
                    .pt_3()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .text_xs()
                    .text_color(rgb(DIM))
                    .child(
                        "Space  play/pause\n← →  seek 5 seconds\n= / −  zoom · ⇧← ⇧→  pan\n0  fit · F  follow\nDrag  select · ⌘L  set loop · L  toggle\n⌘1…⌘5  aspects · ⌘6…⌘9  editors · ⌘B pool",
                    ),
            )
    }

    fn render_inspector(&self) -> impl IntoElement {
        let feature = self.current_feature().unwrap_or_default();
        let (tempo, pulse_support, beat) = self.analysis().map_or_else(
            || ("—".to_owned(), "—".to_owned(), "—".to_owned()),
            |analysis| {
                let beat = analysis
                    .rhythm
                    .beat_times
                    .partition_point(|time| *time <= self.playhead_seconds);
                (
                    format!("{:.1} BPM", analysis.rhythm.tempo_bpm),
                    format!("{:.0}%", analysis.rhythm.pulse_contrast * 100.0),
                    format!("{}", beat + 1),
                )
            },
        );
        let metadata = self.analysis().map(|analysis| {
            format!(
                "{} Hz  ·  {}-bit  ·  {} ch",
                analysis.sample_rate, analysis.bits_per_sample, analysis.channels
            )
        });
        let selection = self
            .timeline_selection
            .filter(|range| !range.is_empty())
            .map_or_else(
                || "—".to_owned(),
                |range| {
                    format!(
                        "{} — {}",
                        format_time(self.seconds_for_sample(range.start.get().max(0) as u64)),
                        format_time(self.seconds_for_sample(range.end.get().max(0) as u64))
                    )
                },
            );
        let loop_status = self.loop_range.map_or_else(
            || "—".to_owned(),
            |range| {
                format!(
                    "{} {} — {}",
                    if self.loop_enabled { "ON" } else { "OFF" },
                    format_time(self.seconds_for_sample(range.start.get().max(0) as u64)),
                    format_time(self.seconds_for_sample(range.end.get().max(0) as u64))
                )
            },
        );
        div()
            .w(px(220.0))
            .flex_none()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL_ALT))
            .p_3()
            .gap_2()
            .child(section_label("AT PLAYHEAD"))
            .child(metric("PULSE", tempo, CYAN))
            .child(metric("PULSE CONTRAST", pulse_support, CYAN))
            .child(metric("BEAT", beat, CYAN))
            .child(metric(
                "DOMINANT",
                format_frequency(feature.dominant_hz),
                MAGENTA,
            ))
            .child(metric(
                "LOUDNESS",
                format!("{:.0}%", feature.loudness * 100.0),
                CYAN,
            ))
            .child(metric(
                "BRIGHTNESS",
                format!("{:.0}%", feature.brightness * 100.0),
                AMBER,
            ))
            .child(metric(
                "TRANSIENT",
                format!("{:.0}%", feature.flux * 100.0),
                AMBER,
            ))
            .child(metric(
                "STEREO WIDTH",
                format!("{:.0}%", feature.stereo_width * 100.0),
                LIME,
            ))
            .child(metric(
                "CORRELATION",
                format!("{:+.2}", feature.correlation),
                LIME,
            ))
            .child(section_label("EDIT RANGE"))
            .child(metric("SELECTION", selection, CYAN))
            .child(metric("LOOP", loop_status, AMBER))
            .when_some(metadata, |this, metadata| {
                this.child(
                    div()
                        .mt_auto()
                        .pt_3()
                        .border_t_1()
                        .border_color(rgb(BORDER))
                        .text_xs()
                        .text_color(rgb(DIM))
                        .child(metadata),
                )
            })
            .when_some(self.audio_error.clone(), |this, error| {
                this.child(div().text_xs().text_color(rgb(MAGENTA)).child(error))
            })
    }

    fn render_timeline(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        match &self.state {
            ProjectState::Empty => empty_state(
                "Sound becomes material here.",
                "Open a FLAC to build a persistent waveform, log-frequency map, and perceptual feature lanes.",
            ),
            ProjectState::Loading(path) => empty_state(
                "Decompiling audio…",
                &format!("Reading {}, then projecting it into inspectable layers.", path.display()),
            ),
            ProjectState::Failed(error) => empty_state("The material would not open.", error),
            ProjectState::Ready(analysis) => {
                let fraction = self.visible_playhead_fraction();
                let spectrogram = self.spectrogram.clone().unwrap();
                let spectrogram_detail = self.spectrogram_detail.clone();
                let spectrogram_refining = self.spectrogram_refining;
                let total_samples = self.timeline_viewport.total_samples.max(1);
                let (time_start, time_end) = self.visible_seconds();
                let normalized_start =
                    self.timeline_viewport.start_sample as f64 / total_samples as f64;
                let normalized_end =
                    self.timeline_viewport.end_sample as f64 / total_samples as f64;
                let waveform = analysis.waveform_range(normalized_start, normalized_end, 2_048);
                let features = slice_visible(
                    &analysis.features,
                    normalized_start,
                    normalized_end,
                );
                let rhythm = analysis.rhythm.clone();
                let timeline_bounds = self.timeline_bounds.clone();
                let selection = self
                    .timeline_selection
                    .and_then(|range| range_fractions(range, self.timeline_viewport));
                let loop_range = self
                    .loop_range
                    .and_then(|range| range_fractions(range, self.timeline_viewport));
                let loop_enabled = self.loop_enabled;
                let viewport = self.timeline_viewport;
                let follow = self.timeline_follow;

                div()
                    .id("arrangement-timeline")
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .relative()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .bg(rgb(BACKGROUND))
                    .cursor_crosshair()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.begin_timeline_selection(event, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_move(cx.listener(
                        |this, event: &MouseMoveEvent, _, cx| {
                            this.extend_timeline_selection(event, cx);
                        },
                    ))
                    .capture_any_mouse_up(cx.listener(
                        |this, event: &MouseUpEvent, _, cx| {
                            this.end_timeline_selection(event, cx);
                        },
                    ))
                    .on_scroll_wheel(cx.listener(
                        |this, event: &ScrollWheelEvent, window, cx| {
                            let Some(bounds) = *this.timeline_bounds.lock().unwrap() else {
                                return;
                            };
                            if !bounds.contains(&event.position) {
                                return;
                            }
                            let delta = event.delta.pixel_delta(window.line_height());
                            let command_zoom =
                                event.modifiers.secondary() || event.modifiers.control;
                            if command_zoom {
                                let wheel = if delta.y.abs() >= delta.x.abs() {
                                    delta.y
                                } else {
                                    delta.x
                                };
                                let amount = f64::from(wheel / px(180.0));
                                if amount.abs() > 0.0001 {
                                    if let Some(anchor) =
                                        this.sample_from_x(event.position.x, true)
                                    {
                                        this.zoom_timeline(anchor, amount.exp(), cx);
                                        cx.stop_propagation();
                                    }
                                }
                            } else if delta.x.abs() > px(0.01) || event.modifiers.shift {
                                let wheel = if delta.x.abs() > px(0.01) {
                                    delta.x
                                } else {
                                    delta.y
                                };
                                let amount = f64::from(wheel / px(480.0));
                                if amount.abs() > 0.0001 {
                                    this.pan_timeline(-amount, cx);
                                    cx.stop_propagation();
                                }
                            }
                        },
                    ))
                    .child(arrangement_ruler(
                        time_start,
                        time_end,
                        viewport,
                        follow,
                        loop_enabled,
                        cx,
                    ))
                    .child(arrangement_lane(
                        "STEREO AMPLITUDE",
                        "retained PCM · L / R",
                        px(100.0),
                        waveform_plot(waveform, fraction),
                    ))
                    .child(arrangement_lane(
                        "LOG-FREQUENCY ENERGY",
                        if spectrogram_refining {
                            "32.7 Hz — 16 kHz · refining visible resolution"
                        } else {
                            "32.7 Hz — 16 kHz · viewport-native detail"
                        },
                        px(250.0),
                        div()
                            .relative()
                            .size_full()
                            .overflow_hidden()
                            .child(if let Some(detail) = spectrogram_detail {
                                img(detail)
                                    .size_full()
                                    .object_fit(ObjectFit::Fill)
                                    .into_any_element()
                            } else {
                                cropped_spectrogram(
                                    spectrogram,
                                    normalized_start,
                                    normalized_end,
                                    0.0,
                                    1.0,
                                )
                                .into_any_element()
                            }),
                    ))
                    .child(arrangement_lane(
                        "PULSE / ONSETS",
                        "low · mid · high evidence",
                        px(92.0),
                        rhythm_plot(rhythm, time_start, time_end, fraction),
                    ))
                    .child(arrangement_lane(
                        "LOUDNESS / BRIGHTNESS",
                        "cyan energy · amber centroid",
                        px(72.0),
                        dual_feature_plot(
                            features.clone(),
                            fraction,
                            |feature| feature.loudness,
                            |feature| feature.brightness,
                            rgba(0x50d8d7cc),
                            rgba(0xf6b76099),
                        )
                    ))
                    .child(arrangement_lane(
                        "TRANSIENT FLUX",
                        "positive spectral change",
                        px(64.0),
                        feature_plot(
                            features.clone(),
                            fraction,
                            |feature| feature.flux,
                            rgba(0xf6b760cc),
                        ),
                    ))
                    .child(arrangement_lane(
                        "STEREO WIDTH",
                        "mid / side energy ratio",
                        px(64.0),
                        feature_plot(
                            features,
                            fraction,
                            |feature| feature.stereo_width,
                            rgba(0xa7d877cc),
                        ),
                    ))
                    .child(arrangement_overlay(
                        timeline_bounds,
                        fraction,
                        selection,
                        loop_range,
                        loop_enabled,
                    ))
                    .into_any_element()
            }
        }
    }
}

impl Focusable for Workbench {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workbench {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("Audec")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .text_sm()
            .child(self.render_header(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.render_sidebar(cx))
                    .child(self.render_timeline(cx))
                    .child(self.render_inspector()),
            )
    }
}

enum HpssViewState {
    Idle,
    Analyzing {
        start_seconds: f64,
        end_seconds: f64,
    },
    Ready(Arc<HpssViewResult>),
    Failed(String),
}

struct HpssViewResult {
    start_seconds: f64,
    end_seconds: f64,
    sample_rate: u32,
    original: Vec<f32>,
    separation: HpssResult,
}

#[derive(Clone, Copy)]
enum HpssAudition {
    Original,
    Harmonic,
    Percussive,
    Residual,
}

enum LoomViewState {
    Idle,
    Inferring {
        start_seconds: f64,
        end_seconds: f64,
        event_count: usize,
    },
    Ready(LoomViewResult),
    Failed(String),
}

enum RhythmViewState {
    Idle,
    Analyzing,
    Ready(Arc<RhythmDeprojection>),
    Failed(String),
}

struct LoomViewResult {
    sketch: SequenceSketch,
    selected_cluster: usize,
    start_sample: usize,
    start_seconds: f64,
    end_seconds: f64,
    sample_rate: u32,
    original: Vec<f32>,
    reconstruction: Vec<f32>,
    residual: Vec<f32>,
    fit: FitMetrics,
}

#[derive(Clone, Copy)]
enum LoomAudition {
    Original,
    Reconstruction,
    Residual,
    Template,
}

struct Visualizer {
    kind: VizKind,
    workbench: Entity<Workbench>,
    timeline_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    focus_handle: FocusHandle,
    time_start: f64,
    time_end: f64,
    follow_playhead: bool,
    frequency_start: f32,
    frequency_end: f32,
    spectrum_settings: SpectrumSettings,
    local_spectrogram: Option<Arc<Image>>,
    local_spectral_db: Option<Arc<Vec<f32>>>,
    spectrogram_source: Option<PathBuf>,
    spectrum_generation: u64,
    spectrum_transforming: bool,
    hpss_state: HpssViewState,
    hpss_generation: u64,
    rhythm_state: RhythmViewState,
    rhythm_generation: u64,
    loom_state: LoomViewState,
    loom_generation: u64,
}

impl Visualizer {
    fn new(kind: VizKind, workbench: Entity<Workbench>, cx: &mut Context<Self>) -> Self {
        cx.observe(&workbench, |_, _, cx| cx.notify()).detach();
        let (spectrum_settings, spectrogram_source, playhead, duration) = {
            let workbench = workbench.read(cx);
            if let Some(analysis) = workbench.analysis() {
                (
                    SpectrumSettings {
                        fft_size: 8_192,
                        hop_size: 2_048,
                        min_frequency_hz: MIN_FREQUENCY,
                        max_frequency_hz: MAX_FREQUENCY,
                        db_ceiling: analysis.spectral_peak_db,
                        db_range: 84.0,
                        ..SpectrumSettings::default()
                    },
                    Some(analysis.path.clone()),
                    workbench.playhead_fraction() as f64,
                    analysis.duration_seconds,
                )
            } else {
                (SpectrumSettings::default(), None, 0.0, 0.0)
            }
        };
        let (time_start, time_end) =
            if matches!(kind, VizKind::Separation | VizKind::Loom) && duration > 0.0 {
                let span = (18.0 / duration).clamp(0.0025, 1.0);
                let start = (playhead - span * 0.5).clamp(0.0, 1.0 - span);
                (start, start + span)
            } else {
                (0.0, 1.0)
            };
        Self {
            kind,
            workbench,
            timeline_bounds: Arc::new(Mutex::new(None)),
            focus_handle: cx.focus_handle(),
            time_start,
            time_end,
            follow_playhead: true,
            frequency_start: 0.0,
            frequency_end: 1.0,
            spectrum_settings,
            local_spectrogram: None,
            local_spectral_db: None,
            spectrogram_source,
            spectrum_generation: 0,
            spectrum_transforming: false,
            hpss_state: HpssViewState::Idle,
            hpss_generation: 0,
            rhythm_state: RhythmViewState::Idle,
            rhythm_generation: 0,
            loom_state: LoomViewState::Idle,
            loom_generation: 0,
        }
    }

    fn rebuild_spectrogram(&mut self, cx: &mut Context<Self>) {
        let analysis = self.workbench.read(cx).analysis().map(|value| {
            (
                value.path.clone(),
                self.local_spectral_db
                    .as_ref()
                    .map(|values| values.as_ref().clone())
                    .unwrap_or_else(|| value.spectral_db.clone()),
                value.spectral_peak_db,
            )
        });
        let Some((path, spectral_db, _)) = analysis else {
            return;
        };
        match encode_spectrogram(
            &spectral_db,
            self.spectrum_settings.db_ceiling,
            self.spectrum_settings.db_range,
        ) {
            Ok(bytes) => {
                self.local_spectrogram = Some(Arc::new(Image::from_bytes(ImageFormat::Png, bytes)));
                self.spectrogram_source = Some(path);
            }
            Err(error) => eprintln!("rendering lens spectrogram: {error:#}"),
        }
        cx.notify();
    }

    fn adjust_db_ceiling(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.spectrum_settings.db_ceiling =
            (self.spectrum_settings.db_ceiling + delta).clamp(-120.0, 24.0);
        self.rebuild_spectrogram(cx);
    }

    fn adjust_db_range(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.spectrum_settings.db_range =
            (self.spectrum_settings.db_range + delta).clamp(6.0, 180.0);
        self.rebuild_spectrogram(cx);
    }

    fn rerun_spectrum(&mut self, cx: &mut Context<Self>) {
        let settings = self.spectrum_settings;
        let source = self.workbench.read(cx).analysis().map(|analysis| {
            let frames = analysis.waveform_pyramid.frame_count();
            (
                analysis.path.clone(),
                analysis.sample_rate,
                analysis.mono_range(0, frames),
            )
        });
        let Some((path, sample_rate, mono)) = source else {
            return;
        };

        self.spectrum_generation = self.spectrum_generation.wrapping_add(1);
        let generation = self.spectrum_generation;
        self.spectrum_transforming = true;
        cx.notify();
        let task = cx.background_spawn(async move {
            let values = spectral_projection(&mono, sample_rate, settings);
            let image = encode_spectrogram(&values, settings.db_ceiling, settings.db_range)
                .map(|bytes| Arc::new(Image::from_bytes(ImageFormat::Png, bytes)))
                .map_err(|error| format!("{error:#}"));
            (values, image)
        });
        cx.spawn(async move |this, cx| {
            let (values, image) = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.spectrum_generation != generation {
                    return;
                }
                this.spectrum_transforming = false;
                match image {
                    Ok(image) => {
                        this.local_spectral_db = Some(Arc::new(values));
                        this.local_spectrogram = Some(image);
                        this.spectrogram_source = Some(path);
                    }
                    Err(error) => eprintln!("rerunning spectrum transform: {error}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn change_fft_size(&mut self, direction: i32, cx: &mut Context<Self>) {
        self.spectrum_settings.fft_size = if direction < 0 {
            (self.spectrum_settings.fft_size / 2).max(256)
        } else {
            (self.spectrum_settings.fft_size * 2).min(65_536)
        };
        self.spectrum_settings.hop_size = (self.spectrum_settings.fft_size / 4).max(1);
        self.rerun_spectrum(cx);
    }

    fn cycle_window_function(&mut self, cx: &mut Context<Self>) {
        self.spectrum_settings.window = self.spectrum_settings.window.next();
        self.rerun_spectrum(cx);
    }

    fn refresh_rhythm(&mut self, cx: &mut Context<Self>) {
        let source = self.workbench.read(cx).analysis().map(|analysis| {
            (
                analysis.mono_pcm.clone(),
                analysis.sample_rate,
                analysis.path.clone(),
            )
        });
        let Some((mono, sample_rate, path)) = source else {
            self.rhythm_state = RhythmViewState::Idle;
            return;
        };

        self.rhythm_generation = self.rhythm_generation.wrapping_add(1);
        let generation = self.rhythm_generation;
        self.rhythm_state = RhythmViewState::Analyzing;
        cx.notify();

        // Deprojection is intentionally off the render path. The retained Arc
        // avoids another file decode and makes opening the window immediate.
        let task = cx.background_spawn(async move {
            let result = deproject_rhythm(&mono, sample_rate, &RhythmDeprojectionConfig::default());
            (path, Arc::new(result))
        });
        cx.spawn(async move |this, cx| {
            let (path, result) = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.rhythm_generation != generation
                    || this.spectrogram_source.as_ref() != Some(&path)
                {
                    return;
                }
                this.rhythm_state = match result.status {
                    RhythmAnalysisStatus::Complete => RhythmViewState::Ready(result),
                    RhythmAnalysisStatus::Silent => {
                        RhythmViewState::Failed("The selected audio is effectively silent.".into())
                    }
                    RhythmAnalysisStatus::InsufficientInput => RhythmViewState::Failed(
                        "There is not enough audio to infer recurring events.".into(),
                    ),
                    RhythmAnalysisStatus::InvalidConfiguration => RhythmViewState::Failed(
                        "The rhythm analysis configuration is invalid.".into(),
                    ),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn audition_rhythm_family(&mut self, family_id: usize, cx: &mut Context<Self>) {
        let RhythmViewState::Ready(result) = &self.rhythm_state else {
            return;
        };
        let Some(span) = result
            .event_families
            .iter()
            .find(|family| family.id == family_id)
            .map(|family| family.medoid.excerpt)
        else {
            return;
        };
        let source = self.workbench.read(cx).analysis().map(|analysis| {
            (
                analysis.mono_range(span.start, span.end),
                analysis.sample_rate,
            )
        });
        let Some((samples, sample_rate)) = source else {
            return;
        };
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| {
            workbench.audition_pcm(samples, sample_rate, cx)
        });
    }

    fn refresh_hpss(&mut self, cx: &mut Context<Self>) {
        let (duration, sample_rate, frame_count, playhead) = {
            let workbench = self.workbench.read(cx);
            let Some(analysis) = workbench.analysis() else {
                self.hpss_state = HpssViewState::Idle;
                return;
            };
            (
                analysis.duration_seconds,
                analysis.sample_rate,
                analysis.waveform_pyramid.frame_count(),
                workbench.playhead_fraction() as f64,
            )
        };
        if frame_count == 0 || duration <= 0.0 {
            self.hpss_state = HpssViewState::Idle;
            return;
        }

        // A reconstructible whole-song complex STFT can consume hundreds of
        // megabytes. HPSS is therefore an Aspect-local transform. Keep an
        // explicit upper bound until the field becomes a tiled disk cache.
        let maximum_span = (30.0 / duration).min(1.0);
        if self.time_span() > maximum_span {
            let anchor = if (self.time_start..=self.time_end).contains(&playhead) {
                playhead
            } else {
                (self.time_start + self.time_end) * 0.5
            };
            self.time_start = (anchor - maximum_span * 0.5).clamp(0.0, 1.0 - maximum_span);
            self.time_end = self.time_start + maximum_span;
        }

        let start_frame = (self.time_start * frame_count as f64).floor() as usize;
        let end_frame = (self.time_end * frame_count as f64).ceil() as usize;
        let original = self
            .workbench
            .read(cx)
            .analysis()
            .map(|analysis| analysis.mono_range(start_frame, end_frame))
            .unwrap_or_default();
        let start_seconds = start_frame as f64 / f64::from(sample_rate);
        let end_seconds = end_frame as f64 / f64::from(sample_rate);

        self.hpss_generation = self.hpss_generation.wrapping_add(1);
        let generation = self.hpss_generation;
        self.hpss_state = HpssViewState::Analyzing {
            start_seconds,
            end_seconds,
        };
        cx.notify();

        let task = cx.background_spawn(async move {
            separate_harmonic_percussive(&original, HpssSettings::default())
                .map(|separation| {
                    Arc::new(HpssViewResult {
                        start_seconds,
                        end_seconds,
                        sample_rate,
                        original,
                        separation,
                    })
                })
                .map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.hpss_generation != generation {
                    return;
                }
                this.hpss_state = match result {
                    Ok(result) => HpssViewState::Ready(result),
                    Err(error) => HpssViewState::Failed(error),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn audition_hpss(&mut self, kind: HpssAudition, cx: &mut Context<Self>) {
        let HpssViewState::Ready(result) = &self.hpss_state else {
            return;
        };
        let samples = match kind {
            HpssAudition::Original => result.original.clone(),
            HpssAudition::Harmonic => result.separation.harmonic.clone(),
            HpssAudition::Percussive => result.separation.percussive.clone(),
            HpssAudition::Residual => result.separation.residual.clone(),
        };
        let sample_rate = result.sample_rate;
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| {
            workbench.audition_pcm(samples, sample_rate, cx)
        });
    }

    fn refresh_loom(&mut self, cx: &mut Context<Self>) {
        let source = {
            let workbench = self.workbench.read(cx);
            workbench.analysis().map(|analysis| {
                let frame_count = analysis.waveform_pyramid.frame_count();
                let observations = analysis
                    .rhythm
                    .onsets
                    .iter()
                    .map(|onset| EventObservation {
                        sample_index: (onset.time_seconds * f64::from(analysis.sample_rate)).round()
                            as usize,
                        cluster_id: onset.cluster,
                        salience: onset.strength,
                        template_similarity: onset.template_similarity,
                    })
                    .collect::<Vec<_>>();
                (
                    analysis.sample_rate,
                    frame_count,
                    analysis.mono_range(0, frame_count),
                    observations,
                )
            })
        };
        let Some((sample_rate, frame_count, mono, observations)) = source else {
            self.loom_state = LoomViewState::Idle;
            return;
        };
        if frame_count == 0 || observations.is_empty() {
            self.loom_state = LoomViewState::Failed(
                "No recurring onset observations are available to sequence.".to_owned(),
            );
            cx.notify();
            return;
        }

        let start_sample = (self.time_start * frame_count as f64).floor() as usize;
        let end_sample = (self.time_end * frame_count as f64).ceil() as usize;
        let start_seconds = start_sample as f64 / f64::from(sample_rate);
        let end_seconds = end_sample as f64 / f64::from(sample_rate);
        let event_count = observations.len();
        self.loom_generation = self.loom_generation.wrapping_add(1);
        let generation = self.loom_generation;
        self.loom_state = LoomViewState::Inferring {
            start_seconds,
            end_seconds,
            event_count,
        };
        cx.notify();

        let task = cx.background_spawn(async move {
            let started = Instant::now();
            let sketch = SequenceSketch::infer(
                &mono,
                sample_rate,
                &observations,
                TemplateBuildConfig::for_sample_rate(sample_rate),
            )
            .map_err(|error| error.to_string())?;
            let result = build_loom_result(sketch, &mono, start_sample, end_sample, sample_rate, 0);
            eprintln!(
                "inferred and rendered {} Loom events across {} templates in {:.3}s",
                result.sketch.events.len(),
                result.sketch.clusters.len(),
                started.elapsed().as_secs_f64(),
            );
            Ok::<_, String>(result)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.loom_generation != generation {
                    return;
                }
                this.loom_state = match result {
                    Ok(result) => LoomViewState::Ready(result),
                    Err(error) => LoomViewState::Failed(error),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn rerender_loom_span(&mut self, cx: &mut Context<Self>) {
        let source = {
            let workbench = self.workbench.read(cx);
            workbench.analysis().map(|analysis| {
                let frame_count = analysis.waveform_pyramid.frame_count();
                let start_sample = (self.time_start * frame_count as f64).floor() as usize;
                let end_sample = (self.time_end * frame_count as f64).ceil() as usize;
                (
                    start_sample,
                    end_sample,
                    analysis.sample_rate,
                    analysis.mono_range(start_sample, end_sample),
                )
            })
        };
        let Some((start_sample, end_sample, sample_rate, original)) = source else {
            return;
        };
        let LoomViewState::Ready(result) = &mut self.loom_state else {
            return;
        };
        update_loom_render(result, original, start_sample, end_sample, sample_rate);
        cx.notify();
    }

    fn cycle_loom_cluster(&mut self, direction: i32, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &mut self.loom_state else {
            return;
        };
        let count = result.sketch.clusters.len();
        if count == 0 {
            return;
        }
        result.selected_cluster = if direction < 0 {
            (result.selected_cluster + count - 1) % count
        } else {
            (result.selected_cluster + 1) % count
        };
        cx.notify();
    }

    fn toggle_loom_cluster(&mut self, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &mut self.loom_state else {
            return;
        };
        let Some(cluster_id) = selected_loom_cluster_id(result) else {
            return;
        };
        let enabled = result
            .sketch
            .cluster(cluster_id)
            .is_some_and(|cluster| !cluster.enabled);
        result.sketch.set_cluster_enabled(cluster_id, enabled);
        rebuild_loom_audio(result);
        cx.notify();
    }

    fn adjust_loom_cluster_gain(&mut self, delta: f32, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &mut self.loom_state else {
            return;
        };
        let Some(cluster_id) = selected_loom_cluster_id(result) else {
            return;
        };
        let gain = result
            .sketch
            .cluster(cluster_id)
            .map_or(1.0, |cluster| cluster.gain);
        result
            .sketch
            .set_cluster_gain(cluster_id, (gain + delta).clamp(0.0, 4.0));
        rebuild_loom_audio(result);
        cx.notify();
    }

    fn edit_nearest_loom_event(
        &mut self,
        timing_delta_seconds: f64,
        gain_delta: f32,
        toggle: bool,
        cx: &mut Context<Self>,
    ) {
        let playhead_sample = {
            let workbench = self.workbench.read(cx);
            workbench
                .analysis()
                .map(|analysis| {
                    (workbench.playhead_seconds * f64::from(analysis.sample_rate)).round() as i64
                })
                .unwrap_or(0)
        };
        let LoomViewState::Ready(result) = &mut self.loom_state else {
            return;
        };
        let Some(cluster_id) = selected_loom_cluster_id(result) else {
            return;
        };
        let Some(event_id) = nearest_loom_event(&result.sketch, cluster_id, playhead_sample) else {
            return;
        };
        if let Some(event) = result.sketch.event(event_id).cloned() {
            if timing_delta_seconds != 0.0 {
                let delta = (timing_delta_seconds * f64::from(result.sample_rate)).round() as i64;
                result
                    .sketch
                    .move_event(event_id, event.sample_index + delta);
            }
            if gain_delta != 0.0 {
                result
                    .sketch
                    .set_event_gain(event_id, (event.gain + gain_delta).clamp(0.0, 4.0));
            }
            if toggle {
                result.sketch.set_event_enabled(event_id, !event.enabled);
            }
        }
        rebuild_loom_audio(result);
        cx.notify();
    }

    fn audition_loom(&mut self, kind: LoomAudition, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &self.loom_state else {
            return;
        };
        let samples = match kind {
            LoomAudition::Original => result.original.clone(),
            LoomAudition::Reconstruction => result.reconstruction.clone(),
            LoomAudition::Residual => result.residual.clone(),
            LoomAudition::Template => selected_loom_cluster_id(result)
                .and_then(|cluster_id| result.sketch.cluster(cluster_id))
                .map(|cluster| cluster.template.samples.clone())
                .unwrap_or_default(),
        };
        let sample_rate = result.sample_rate;
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| {
            workbench.audition_pcm(samples, sample_rate, cx)
        });
    }

    fn seek_from_pointer(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some(bounds) = *self.timeline_bounds.lock().unwrap() else {
            return;
        };
        let duration = self
            .workbench
            .read(cx)
            .analysis()
            .map_or(0.0, |analysis| analysis.duration_seconds);
        let fraction = ((event.position.x - bounds.origin.x) / bounds.size.width).clamp(0.0, 1.0);
        let global_fraction = self.time_start + f64::from(fraction) * self.time_span();
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| {
            workbench.seek_to(duration * global_fraction, cx)
        });
    }

    fn time_span(&self) -> f64 {
        (self.time_end - self.time_start).max(1.0e-6)
    }

    fn follow_playhead_if_needed(&mut self, analysis: &Analysis, playhead_seconds: f64) {
        if !self.follow_playhead || self.time_span() >= 0.999_999 {
            return;
        }
        let current =
            (playhead_seconds / analysis.duration_seconds.max(f64::EPSILON)).clamp(0.0, 1.0);
        if (self.time_start..=self.time_end).contains(&current) {
            return;
        }
        let span = self.time_span();
        self.time_start = (current - span * 0.5).clamp(0.0, 1.0 - span);
        self.time_end = self.time_start + span;

        if self.kind == VizKind::Loom {
            let frame_count = analysis.waveform_pyramid.frame_count();
            let start_sample = (self.time_start * frame_count as f64).floor() as usize;
            let end_sample = (self.time_end * frame_count as f64).ceil() as usize;
            let original = analysis.mono_range(start_sample, end_sample);
            if let LoomViewState::Ready(result) = &mut self.loom_state {
                update_loom_render(
                    result,
                    original,
                    start_sample,
                    end_sample,
                    analysis.sample_rate,
                );
            }
        }
    }

    fn center_time_on_playhead(&mut self, cx: &mut Context<Self>) {
        self.follow_playhead = true;
        let center = self.workbench.read(cx).playhead_fraction() as f64;
        let span = self.time_span();
        self.time_start = (center - span * 0.5).clamp(0.0, 1.0 - span);
        self.time_end = self.time_start + span;
        if self.kind == VizKind::Loom {
            self.rerender_loom_span(cx);
        } else {
            cx.notify();
        }
    }

    fn zoom_time(&mut self, scale: f64, cx: &mut Context<Self>) {
        self.follow_playhead = false;
        let current = self.workbench.read(cx).playhead_fraction() as f64;
        let current_is_visible = (self.time_start..=self.time_end).contains(&current);
        let anchor = if current_is_visible {
            current
        } else {
            current.clamp(0.0, 1.0)
        };
        let new_span = (self.time_span() * scale).clamp(0.0025, 1.0);
        let anchor_position = if current_is_visible {
            (anchor - self.time_start) / self.time_span()
        } else {
            0.5
        };
        let mut start = anchor - anchor_position * new_span;
        start = start.clamp(0.0, 1.0 - new_span);
        self.time_start = start;
        self.time_end = start + new_span;
        if self.kind == VizKind::Loom {
            self.rerender_loom_span(cx);
        } else {
            cx.notify();
        }
    }

    fn pan_time(&mut self, amount: f64, cx: &mut Context<Self>) {
        self.follow_playhead = false;
        let span = self.time_span();
        let start = (self.time_start + amount * span).clamp(0.0, 1.0 - span);
        self.time_start = start;
        self.time_end = start + span;
        if self.kind == VizKind::Loom {
            self.rerender_loom_span(cx);
        } else {
            cx.notify();
        }
    }

    fn zoom_frequency(&mut self, scale: f32, cx: &mut Context<Self>) {
        let center = (self.frequency_start + self.frequency_end) * 0.5;
        let span = ((self.frequency_end - self.frequency_start) * scale).clamp(0.05, 1.0);
        let start = (center - span * 0.5).clamp(0.0, 1.0 - span);
        self.frequency_start = start;
        self.frequency_end = start + span;
        cx.notify();
    }

    fn reset_view(&mut self, cx: &mut Context<Self>) {
        self.follow_playhead = false;
        self.time_start = 0.0;
        self.time_end = 1.0;
        self.frequency_start = 0.0;
        self.frequency_end = 1.0;
        if self.kind == VizKind::Loom {
            self.rerender_loom_span(cx);
        } else {
            cx.notify();
        }
    }

    fn on_toggle(&mut self, _: &TogglePlayback, _: &mut Window, cx: &mut Context<Self>) {
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| workbench.toggle_playback(cx));
    }

    fn on_seek_backward(&mut self, _: &SeekBackward, _: &mut Window, cx: &mut Context<Self>) {
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| workbench.seek_relative(-5.0, cx));
    }

    fn on_seek_forward(&mut self, _: &SeekForward, _: &mut Window, cx: &mut Context<Self>) {
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| workbench.seek_relative(5.0, cx));
    }

    fn on_view_zoom_in(&mut self, _: &ViewZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_time(0.5, cx);
    }

    fn on_view_zoom_out(&mut self, _: &ViewZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_time(2.0, cx);
    }

    fn on_view_pan_left(&mut self, _: &ViewPanLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.pan_time(-0.7, cx);
    }

    fn on_view_pan_right(&mut self, _: &ViewPanRight, _: &mut Window, cx: &mut Context<Self>) {
        self.pan_time(0.7, cx);
    }

    fn on_view_fit(&mut self, _: &ViewFit, _: &mut Window, cx: &mut Context<Self>) {
        self.reset_view(cx);
    }

    fn render_header(
        &self,
        analysis: &Analysis,
        playhead_seconds: f64,
        is_playing: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let frequency_ratio = MAX_FREQUENCY / MIN_FREQUENCY;
        let frequency_low = MIN_FREQUENCY * frequency_ratio.powf(self.frequency_start);
        let frequency_high = MIN_FREQUENCY * frequency_ratio.powf(self.frequency_end);
        let is_waterfall = self.kind == VizKind::Waterfall;
        let top_row =
            div()
                .h(px(50.0))
                .flex_none()
                .flex()
                .items_center()
                .pl(px(82.0))
                .pr_4()
                .gap_3()
                .border_b_1()
                .border_color(rgb(BORDER))
                .child(
                    div()
                        .max_w(px(310.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_sm()
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(self.kind.title()),
                )
                .child(
                    div()
                        .max_w(px(180.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_xs()
                        .text_color(rgb(MAGENTA))
                        .child(analysis.title.clone()),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            viz_control("view-pan-left", "←")
                                .on_click(cx.listener(|this, _, _, cx| this.pan_time(-0.7, cx))),
                        )
                        .child(
                            viz_control("view-zoom-out", "−")
                                .on_click(cx.listener(|this, _, _, cx| this.zoom_time(2.0, cx))),
                        )
                        .child(
                            viz_control("view-fit", "Fit")
                                .on_click(cx.listener(|this, _, _, cx| this.reset_view(cx))),
                        )
                        .child(viz_control("view-current", "Follow").px_2().on_click(
                            cx.listener(|this, _, _, cx| this.center_time_on_playhead(cx)),
                        ))
                        .child(
                            viz_control("view-zoom-in", "+")
                                .on_click(cx.listener(|this, _, _, cx| this.zoom_time(0.5, cx))),
                        )
                        .child(
                            viz_control("view-pan-right", "→")
                                .on_click(cx.listener(|this, _, _, cx| this.pan_time(0.7, cx))),
                        ),
                )
                .when(self.kind == VizKind::Separation, |header| {
                    header.child(
                        viz_control("reanalyze-hpss", "Analyze view")
                            .px_2()
                            .on_click(cx.listener(|this, _, _, cx| this.refresh_hpss(cx))),
                    )
                })
                .when(self.kind == VizKind::Loom, |header| {
                    header.child(
                        viz_control("reinfer-loom", "Reinfer")
                            .px_2()
                            .on_click(cx.listener(|this, _, _, cx| this.refresh_loom(cx))),
                    )
                })
                .child(div().flex_1())
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(CYAN))
                        .child(format_time(playhead_seconds)),
                )
                .child(
                    div()
                        .id("viz-play-pause")
                        .size(px(30.0))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(rgb(TEXT))
                        .text_color(rgb(BACKGROUND))
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            let workbench = this.workbench.clone();
                            workbench.update(cx, |workbench, cx| workbench.toggle_playback(cx));
                        }))
                        .child(if is_playing { "❚❚" } else { "▶" }),
                );

        div()
            .h(px(if is_waterfall { 86.0 } else { 50.0 }))
            .flex_none()
            .flex()
            .flex_col()
            .bg(rgb(PANEL))
            .child(top_row)
            .when(is_waterfall, |header| {
                header.child(
                    div()
                        .h(px(36.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .pl(px(82.0))
                        .pr_4()
                        .gap_1()
                        .bg(rgb(PANEL_ALT))
                        .border_b_1()
                        .border_color(rgb(BORDER))
                        .child(
                            viz_control("fft-size-down", "FFT−").on_click(
                                cx.listener(|this, _, _, cx| this.change_fft_size(-1, cx)),
                            ),
                        )
                        .child(
                            div()
                                .min_w(px(82.0))
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .child(format!(
                                    "{} {}{}",
                                    self.spectrum_settings.fft_size,
                                    self.spectrum_settings.window.label(),
                                    if self.spectrum_transforming {
                                        " …"
                                    } else {
                                        ""
                                    }
                                )),
                        )
                        .child(
                            viz_control("fft-size-up", "FFT+").on_click(
                                cx.listener(|this, _, _, cx| this.change_fft_size(1, cx)),
                            ),
                        )
                        .child(
                            viz_control("fft-window", "Win").on_click(
                                cx.listener(|this, _, _, cx| this.cycle_window_function(cx)),
                            ),
                        )
                        .child(div().w(px(12.0)))
                        .child(
                            viz_control("frequency-out", "F−").on_click(
                                cx.listener(|this, _, _, cx| this.zoom_frequency(2.0, cx)),
                            ),
                        )
                        .child(
                            div()
                                .min_w(px(105.0))
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .child(format!(
                                    "{}–{}",
                                    format_frequency(frequency_low),
                                    format_frequency(frequency_high)
                                )),
                        )
                        .child(
                            viz_control("frequency-in", "F+").on_click(
                                cx.listener(|this, _, _, cx| this.zoom_frequency(0.5, cx)),
                            ),
                        )
                        .child(div().w(px(12.0)))
                        .child(viz_control("db-ceiling-down", "D−").on_click(
                            cx.listener(|this, _, _, cx| this.adjust_db_ceiling(-3.0, cx)),
                        ))
                        .child(
                            div()
                                .min_w(px(88.0))
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .child(format!(
                                    "{:.0}/{:.0} dB",
                                    self.spectrum_settings.db_ceiling,
                                    self.spectrum_settings.db_range
                                )),
                        )
                        .child(viz_control("db-ceiling-up", "D+").on_click(
                            cx.listener(|this, _, _, cx| this.adjust_db_ceiling(3.0, cx)),
                        ))
                        .child(
                            viz_control("db-range-down", "R−").on_click(
                                cx.listener(|this, _, _, cx| this.adjust_db_range(-6.0, cx)),
                            ),
                        )
                        .child(
                            viz_control("db-range-up", "R+").on_click(
                                cx.listener(|this, _, _, cx| this.adjust_db_range(6.0, cx)),
                            ),
                        )
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(DIM))
                                .child("FFT/window rebuild evidence · F/D/R are view transfer"),
                        ),
                )
            })
    }

    fn render_waterfall(
        &self,
        analysis: Arc<Analysis>,
        spectrogram: Arc<Image>,
        playhead: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let timeline_bounds = self.timeline_bounds.clone();
        let start_seconds = analysis.duration_seconds * self.time_start;
        let end_seconds = analysis.duration_seconds * self.time_end;
        let features = slice_visible(&analysis.features, self.time_start, self.time_end);
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(time_ruler_range(start_seconds, end_seconds))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(300.0))
                    .overflow_hidden()
                    .cursor_crosshair()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.seek_from_pointer(event, cx)
                        }),
                    )
                    .child(cropped_spectrogram(
                        spectrogram,
                        self.time_start,
                        self.time_end,
                        self.frequency_start,
                        self.frequency_end,
                    ))
                    .child(timeline_overlay(timeline_bounds, playhead)),
            )
            .child(lane(
                "LOUDNESS / BRIGHTNESS",
                px(92.0),
                dual_feature_plot(
                    features.clone(),
                    playhead,
                    |feature| feature.loudness,
                    |feature| feature.brightness,
                    rgba(0x50d8d7cc),
                    rgba(0xf6b76099),
                ),
            ))
            .child(lane(
                "TRANSIENT FLUX",
                px(82.0),
                feature_plot(features, playhead, |feature| feature.flux, rgba(0xf6b760cc)),
            ))
    }

    fn render_rhythm(
        &self,
        analysis: Arc<Analysis>,
        playhead: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let timeline_bounds = self.timeline_bounds.clone();
        let start_seconds = analysis.duration_seconds * self.time_start;
        let end_seconds = analysis.duration_seconds * self.time_end;
        let waveform = analysis.waveform_range(self.time_start, self.time_end, 4_096);
        let features = slice_visible(&analysis.features, self.time_start, self.time_end);
        let evidence = match &self.rhythm_state {
            RhythmViewState::Idle => empty_state(
                "Rhythm deprojection has not run",
                "Reopen this Aspect to analyze the retained PCM.",
            )
            .into_any_element(),
            RhythmViewState::Analyzing => empty_state(
                "Deprojecting rhythm…",
                "Finding multiband attacks, competing pulses, exact hit spans, and recurring mixed-audio families off the render thread.",
            )
            .into_any_element(),
            RhythmViewState::Failed(error) => {
                empty_state("Rhythm deprojection unavailable", error).into_any_element()
            }
            RhythmViewState::Ready(result) => {
                let visible_start = (self.time_start * result.sample_frames as f64).floor() as usize;
                let visible_end = (self.time_end * result.sample_frames as f64).ceil() as usize;
                let family_ids = visible_rhythm_family_ids(
                    result,
                    visible_start,
                    visible_end,
                    RHYTHM_MAX_VISIBLE_FAMILIES,
                );
                let tempo = tempo_hypotheses_summary(result);
                let phase_summary = format!(
                    "{} beat phases · {} downbeat/meter hypotheses · {} pattern candidates",
                    result.beat_phase_hypotheses.len(),
                    result.downbeat_hypotheses.len(),
                    result.patterns.len()
                );
                let result_for_plot = result.clone();
                let plot_family_ids = family_ids.clone();
                let sample_rate = result.sample_rate;

                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(54.0))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .justify_center()
                            .px_4()
                            .gap_1()
                            .bg(rgb(PANEL_ALT))
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(div().text_sm().text_color(rgb(CYAN)).child(tempo))
                            .child(div().text_xs().text_color(rgb(MUTED)).child(phase_summary)),
                    )
                    .child(time_ruler_range(start_seconds, end_seconds))
                    .child(
                        div()
                            .h(px(RHYTHM_ROW_HEIGHT * RHYTHM_MAX_VISIBLE_FAMILIES as f32))
                            .flex_none()
                            .flex()
                            .child(
                                div()
                                    .w(px(RHYTHM_GUTTER))
                                    .flex_none()
                                    .flex()
                                    .flex_col()
                                    .bg(rgb(PANEL_ALT))
                                    .border_r_1()
                                    .border_color(rgb(BORDER))
                                    .children(family_ids.iter().copied().filter_map(|family_id| {
                                        let family = result
                                            .event_families
                                            .iter()
                                            .find(|family| family.id == family_id)?;
                                        let visible = family
                                            .event_indices
                                            .iter()
                                            .filter(|index| {
                                                result.hits.get(**index).is_some_and(|hit| {
                                                    spans_overlap(
                                                        hit.span,
                                                        visible_start,
                                                        visible_end,
                                                    )
                                                })
                                            })
                                            .count();
                                        let medoid_seconds = family.medoid.excerpt.start as f64
                                            / f64::from(sample_rate);
                                        let medoid_span = family.medoid.excerpt;
                                        Some(
                                            div()
                                                .id(("rhythm-family", family_id))
                                                .h(px(RHYTHM_ROW_HEIGHT))
                                                .flex_none()
                                                .px_3()
                                                .flex()
                                                .flex_col()
                                                .justify_center()
                                                .border_b_1()
                                                .border_color(rgb(BORDER))
                                                .cursor_pointer()
                                                .hover(|style| style.bg(rgb(PANEL)))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.audition_rhythm_family(family_id, cx)
                                                }))
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(cluster_color(family_id))
                                                        .child(format!(
                                                            "▶ Anonymous family {:02} · {visible}/{} visible",
                                                            family_id + 1,
                                                            family.event_indices.len()
                                                        )),
                                                )
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(rgb(MUTED))
                                                        .child(format!(
                                                            "mixed medoid {} · exact [{}..{})",
                                                            format_time(medoid_seconds),
                                                            medoid_span.start,
                                                            medoid_span.end
                                                        )),
                                                )
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(rgb(DIM))
                                                        .child(format!(
                                                            "{:.0}% cohesion evidence · click to audition",
                                                            family.evidence * 100.0
                                                        )),
                                                ),
                                        )
                                    })),
                            )
                            .child(
                                div()
                                    .relative()
                                    .flex_1()
                                    .h_full()
                                    .cursor_crosshair()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                            this.seek_from_pointer(event, cx)
                                        }),
                                    )
                                    .child(rhythm_deprojection_plot(
                                        result_for_plot,
                                        plot_family_ids,
                                        visible_start,
                                        visible_end,
                                        playhead,
                                    ))
                                    .child(timeline_overlay(timeline_bounds, playhead)),
                            ),
                    )
                    .child(
                        div()
                            .h(px(34.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px_4()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!(
                                "{} exact hits in view · family rows are recurring mixed excerpts, not isolated instrument identities; magenta marks pattern-start evidence.",
                                visible_hit_count(result, visible_start, visible_end)
                            )),
                    )
                    .child(lane(
                        "STEREO AMPLITUDE",
                        px(100.0),
                        waveform_plot(waveform, playhead),
                    ))
                    .child(lane(
                        "TRANSIENT FLUX",
                        px(72.0),
                        feature_plot(
                            features,
                            playhead,
                            |feature| feature.flux,
                            rgba(0xf6b760cc),
                        ),
                    ))
                    .into_any_element()
            }
        };
        div().flex_1().min_h_0().child(evidence)
    }

    fn render_components(
        &self,
        analysis: Arc<Analysis>,
        playhead: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let timeline_bounds = self.timeline_bounds.clone();
        let start_seconds = analysis.duration_seconds * self.time_start;
        let end_seconds = analysis.duration_seconds * self.time_end;
        let decomposition = analysis.components.clone();
        let components = decomposition.components.clone();
        let component_count = components.len().max(1);

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(38.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_4()
                    .gap_4()
                    .bg(rgb(PANEL_ALT))
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_color(rgb(CYAN))
                            .child(format!("{} components", components.len())),
                    )
                    .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                        "{:.0}% explained energy  ·  {:.1}% relative magnitude error  ·  {} iterations",
                        decomposition.explained_energy * 100.0,
                        decomposition.relative_error * 100.0,
                        decomposition.iterations_run
                    ))),
            )
            .child(time_ruler_range(start_seconds, end_seconds))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(360.0))
                    .flex()
                    .child(
                        div()
                            .w(px(210.0))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .bg(rgb(PANEL_ALT))
                            .border_r_1()
                            .border_color(rgb(BORDER))
                            .children(components.into_iter().enumerate().map(
                                move |(index, component)| {
                                    div()
                                        .h(relative(1.0 / component_count as f32))
                                        .px_2()
                                        .flex()
                                        .flex_col()
                                        .justify_center()
                                        .border_b_1()
                                        .border_color(rgb(BORDER))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(cluster_color(index))
                                                .child(format!("Component C{}", index + 1)),
                                        )
                                        .child(div().text_xs().text_color(rgb(DIM)).child(format!(
                                            "{:.1}% energy · {:.0}% distinct",
                                            component.energy_share * 100.0,
                                            component.spectral_distinctness * 100.0,
                                        )))
                                        .child(cluster_spectrum_plot(
                                            component.spectral_template,
                                            cluster_rgba(index),
                                        ))
                                },
                            )),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .h_full()
                            .cursor_crosshair()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                    this.seek_from_pointer(event, cx)
                                }),
                            )
                            .child(component_activation_plot(
                                decomposition,
                                self.time_start,
                                self.time_end,
                                playhead,
                            ))
                            .child(timeline_overlay(timeline_bounds, playhead)),
                    ),
            )
            .child(
                div()
                    .h(px(34.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_4()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child("NMF factors recurring mixed-audio magnitude shapes; components are hypotheses, not isolated sources or instrument labels."),
            )
    }

    fn render_separation(
        &self,
        analysis: Arc<Analysis>,
        playhead_seconds: f64,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match &self.hpss_state {
            HpssViewState::Idle => empty_state(
                "No selected-span decomposition yet",
                "Frame a span of at most 30 seconds, then choose Analyze view.",
            ),
            HpssViewState::Analyzing {
                start_seconds,
                end_seconds,
            } => empty_state(
                "Separating sustained and transient evidence…",
                &format!(
                    "Analyzing {}–{} with a reconstructible complex STFT and complementary soft masks.",
                    format_time(*start_seconds),
                    format_time(*end_seconds)
                ),
            ),
            HpssViewState::Failed(error) => {
                empty_state("The selected-span transform failed", error)
            }
            HpssViewState::Ready(result) => {
                let diagnostics = result.separation.diagnostics;
                let null_db = if diagnostics.relative_reconstruction_error <= 1.0e-9 {
                    -180.0
                } else {
                    20.0 * diagnostics.relative_reconstruction_error.log10()
                };
                let result_playhead = ((playhead_seconds - result.start_seconds)
                    / (result.end_seconds - result.start_seconds).max(f64::EPSILON))
                    as f32;
                let original = mono_waveform_bins(&result.original, 3_000);
                let harmonic = mono_waveform_bins(&result.separation.harmonic, 3_000);
                let percussive = mono_waveform_bins(&result.separation.percussive, 3_000);
                let residual = mono_waveform_bins(&result.separation.residual, 3_000);
                let result_span = (result.end_seconds - result.start_seconds).max(f64::EPSILON);
                let requested_start = analysis.duration_seconds * self.time_start;
                let requested_end = analysis.duration_seconds * self.time_end;
                let stale = (requested_start - result.start_seconds).abs() > result_span * 0.002
                    || (requested_end - result.end_seconds).abs() > result_span * 0.002;

                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(42.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px_4()
                            .gap_4()
                            .bg(rgb(PANEL_ALT))
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(
                                div().text_color(rgb(CYAN)).child(format!(
                                    "mask separation {:.0}%",
                                    diagnostics.mask_confidence * 100.0
                                )),
                            )
                            .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                                "mixture null {:.1} dB  ·  FFT {} / hop {}  ·  {}",
                                null_db,
                                result.separation.settings.fft_size,
                                result.separation.settings.hop_size,
                                if stale { "view changed — reanalyze to update" } else { "selected span is current" }
                            )))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(
                                        viz_control("hear-hpss-original", "Hear mix").px_2().on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.audition_hpss(HpssAudition::Original, cx)
                                            }),
                                        ),
                                    )
                                    .child(
                                        viz_control("hear-hpss-harmonic", "Hear sustained")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.audition_hpss(HpssAudition::Harmonic, cx)
                                            })),
                                    )
                                    .child(
                                        viz_control("hear-hpss-percussive", "Hear transient")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.audition_hpss(HpssAudition::Percussive, cx)
                                            })),
                                    )
                                    .child(
                                        viz_control("hear-hpss-residual", "Hear null")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.audition_hpss(HpssAudition::Residual, cx)
                                            })),
                                    ),
                            ),
                    )
                    .child(time_ruler_range(result.start_seconds, result.end_seconds))
                    .child(lane(
                        "ORIGINAL MIX / SELECTED ASPECT",
                        px(120.0),
                        waveform_plot(original, result_playhead),
                    ))
                    .child(lane(
                        "TONALLY SUSTAINED ESTIMATE",
                        px(120.0),
                        waveform_plot(harmonic, result_playhead),
                    ))
                    .child(lane(
                        "TRANSIENT ESTIMATE",
                        px(120.0),
                        waveform_plot(percussive, result_playhead),
                    ))
                    .child(lane(
                        "MIXTURE NULL (ORIGINAL − ESTIMATES)",
                        px(92.0),
                        waveform_plot(residual, result_playhead),
                    ))
                    .child(
                        div()
                            .h(px(38.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px_4()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("HPSS separates time-persistent from frequency-broad evidence. It is auditionable and additive, but it is not an instrument or vocal classifier."),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_loom(
        &self,
        _analysis: Arc<Analysis>,
        playhead_seconds: f64,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match &self.loom_state {
            LoomViewState::Idle => empty_state(
                "No editable reconstruction yet",
                "Infer recurring excerpts and their event sequence for this material.",
            ),
            LoomViewState::Inferring {
                start_seconds,
                end_seconds,
                event_count,
            } => empty_state(
                "Inferring reusable event templates…",
                &format!(
                    "Aligning {event_count} mixed-signal occurrences, then rendering {}–{}.",
                    format_time(*start_seconds),
                    format_time(*end_seconds)
                ),
            ),
            LoomViewState::Failed(error) => empty_state("The sequence hypothesis failed", error),
            LoomViewState::Ready(result) => {
                let cluster_count = result.sketch.clusters.len();
                let selected = result
                    .sketch
                    .clusters
                    .get(result.selected_cluster.min(cluster_count.saturating_sub(1)));
                let selected_cluster_id = selected
                    .map(|cluster| cluster.template.cluster_id)
                    .unwrap_or(0);
                let selected_events = result
                    .sketch
                    .events
                    .iter()
                    .filter(|event| event.cluster_id == selected_cluster_id)
                    .count();
                let selected_gain = selected.map_or(0.0, |cluster| cluster.gain);
                let selected_enabled = selected.is_some_and(|cluster| cluster.enabled);
                let agreement = selected.map_or(0.0, |cluster| cluster.template.exemplar_agreement);
                let template = selected
                    .map(|cluster| mono_waveform_bins(&cluster.template.samples, 1_200))
                    .unwrap_or_default();
                let local_playhead = ((playhead_seconds - result.start_seconds)
                    / (result.end_seconds - result.start_seconds).max(f64::EPSILON))
                    as f32;
                let original = mono_waveform_bins(&result.original, 2_400);
                let reconstruction = mono_waveform_bins(&result.reconstruction, 2_400);
                let residual = mono_waveform_bins(&result.residual, 2_400);
                let explained = result.fit.explained_energy * 100.0;

                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(42.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px_4()
                            .gap_3()
                            .bg(rgb(PANEL_ALT))
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(div().text_color(rgb(CYAN)).child(format!(
                                "{explained:.1}% source energy explained"
                            )))
                            .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                                "correlation {:+.3}  ·  {} templates / {} events  ·  editable overlap-add render",
                                result.fit.correlation,
                                cluster_count,
                                result.sketch.events.len(),
                            )))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(viz_control("hear-loom-mix", "Mix").px_2().on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.audition_loom(LoomAudition::Original, cx)
                                        }),
                                    ))
                                    .child(
                                        viz_control("hear-loom-render", "Render")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.audition_loom(
                                                    LoomAudition::Reconstruction,
                                                    cx,
                                                )
                                            })),
                                    )
                                    .child(
                                        viz_control("hear-loom-residual", "Residual")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.audition_loom(LoomAudition::Residual, cx)
                                            })),
                                    )
                                    .child(
                                        viz_control("hear-loom-template", "Template")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.audition_loom(LoomAudition::Template, cx)
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .h(px(42.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px_4()
                            .gap_1()
                            .bg(rgb(PANEL))
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(viz_control("loom-cluster-prev", "Cluster ‹").px_2().on_click(
                                cx.listener(|this, _, _, cx| this.cycle_loom_cluster(-1, cx)),
                            ))
                            .child(
                                div()
                                    .min_w(px(178.0))
                                    .px_2()
                                    .text_xs()
                                    .text_color(cluster_color(selected_cluster_id))
                                    .child(format!(
                                        "Cluster {} · {} events · {:.0}% agreement",
                                        selected_cluster_id + 1,
                                        selected_events,
                                        agreement * 100.0
                                    )),
                            )
                            .child(viz_control("loom-cluster-next", "Cluster ›").px_2().on_click(
                                cx.listener(|this, _, _, cx| this.cycle_loom_cluster(1, cx)),
                            ))
                            .child(viz_control("loom-cluster-toggle", "Mute/on").px_2().on_click(
                                cx.listener(|this, _, _, cx| this.toggle_loom_cluster(cx)),
                            ))
                            .child(viz_control("loom-cluster-gain-down", "Gain −").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.adjust_loom_cluster_gain(-0.1, cx)
                                }),
                            ))
                            .child(
                                div().min_w(px(48.0)).text_xs().text_color(if selected_enabled {
                                    rgb(TEXT)
                                } else {
                                    rgb(DIM)
                                }).child(format!("{selected_gain:.2}×")),
                            )
                            .child(viz_control("loom-cluster-gain-up", "Gain +").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.adjust_loom_cluster_gain(0.1, cx)
                                }),
                            ))
                            .child(div().w(px(10.0)))
                            .child(viz_control("loom-event-left", "Event −10ms").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.edit_nearest_loom_event(-0.010, 0.0, false, cx)
                                }),
                            ))
                            .child(viz_control("loom-event-right", "Event +10ms").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.edit_nearest_loom_event(0.010, 0.0, false, cx)
                                }),
                            ))
                            .child(viz_control("loom-event-gain-down", "Ev −").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.edit_nearest_loom_event(0.0, -0.1, false, cx)
                                }),
                            ))
                            .child(viz_control("loom-event-gain-up", "Ev +").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.edit_nearest_loom_event(0.0, 0.1, false, cx)
                                }),
                            ))
                            .child(viz_control("loom-event-toggle", "Ev on/off").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.edit_nearest_loom_event(0.0, 0.0, true, cx)
                                }),
                            )),
                    )
                    .child(time_ruler_range(result.start_seconds, result.end_seconds))
                    .child(lane(
                        "SELECTED REUSABLE MIXED-SIGNAL TEMPLATE",
                        px(78.0),
                        waveform_plot(template, -1.0),
                    ))
                    .child(lane(
                        "EDITABLE EVENT SEQUENCE · HEIGHT = GAIN · DIM = DISABLED",
                        px(150.0),
                        loom_event_plot(
                            result.sketch.clone(),
                            result.start_seconds,
                            result.end_seconds,
                            local_playhead,
                            selected_cluster_id,
                        ),
                    ))
                    .child(lane(
                        "ORIGINAL MIX",
                        px(78.0),
                        waveform_plot(original, local_playhead),
                    ))
                    .child(lane(
                        "EVENT-TEMPLATE RECONSTRUCTION",
                        px(78.0),
                        waveform_plot(reconstruction, local_playhead),
                    ))
                    .child(lane(
                        "UNEXPLAINED RESIDUAL · ORIGINAL − RECONSTRUCTION",
                        px(78.0),
                        waveform_plot(residual, local_playhead),
                    ))
                    .child(
                        div()
                            .h(px(34.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px_4()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("Edits target the selected cluster and its event nearest the shared playhead. Templates are real aligned excerpts from the mix, so overlapping voices and effects leak into them."),
                    )
                    .into_any_element()
            }
        }
    }
}

fn build_loom_result(
    sketch: SequenceSketch,
    source: &[f32],
    start_sample: usize,
    end_sample: usize,
    sample_rate: u32,
    selected_cluster: usize,
) -> LoomViewResult {
    let start_sample = start_sample.min(source.len());
    let end_sample = end_sample.min(source.len()).max(start_sample);
    let original = source[start_sample..end_sample].to_vec();
    let reconstruction = sketch.render_span(start_sample, original.len());
    let residual = original
        .iter()
        .zip(&reconstruction)
        .map(|(source, rendered)| source - rendered)
        .collect::<Vec<_>>();
    let fit = sketch.fit_span(source, start_sample, original.len());
    LoomViewResult {
        sketch,
        selected_cluster,
        start_sample,
        start_seconds: start_sample as f64 / f64::from(sample_rate),
        end_seconds: end_sample as f64 / f64::from(sample_rate),
        sample_rate,
        original,
        reconstruction,
        residual,
        fit,
    }
}

fn update_loom_render(
    result: &mut LoomViewResult,
    original: Vec<f32>,
    start_sample: usize,
    end_sample: usize,
    sample_rate: u32,
) {
    result.start_sample = start_sample;
    result.start_seconds = start_sample as f64 / f64::from(sample_rate);
    result.end_seconds = end_sample as f64 / f64::from(sample_rate);
    result.sample_rate = sample_rate;
    result.original = original;
    rebuild_loom_audio(result);
}

fn rebuild_loom_audio(result: &mut LoomViewResult) {
    result.reconstruction = result
        .sketch
        .render_span(result.start_sample, result.original.len());
    result.residual = result
        .original
        .iter()
        .zip(&result.reconstruction)
        .map(|(source, rendered)| source - rendered)
        .collect();
    result.fit = fit_rendered_span(
        &result.original,
        &result.reconstruction,
        result.start_sample,
    );
}

fn fit_rendered_span(source: &[f32], rendered: &[f32], start_sample: usize) -> FitMetrics {
    let mut source_energy = 0.0_f64;
    let mut rendered_energy = 0.0_f64;
    let mut residual_energy = 0.0_f64;
    let mut dot = 0.0_f64;
    for (&source, &rendered) in source.iter().zip(rendered) {
        let source = f64::from(source);
        let rendered = f64::from(rendered);
        let residual = source - rendered;
        source_energy += source * source;
        rendered_energy += rendered * rendered;
        residual_energy += residual * residual;
        dot += source * rendered;
    }
    let normalized_error = if source_energy <= f64::EPSILON {
        if residual_energy <= f64::EPSILON {
            0.0
        } else {
            f64::INFINITY
        }
    } else {
        residual_energy / source_energy
    };
    let correlation_denominator = (source_energy * rendered_energy).sqrt();
    let correlation = if correlation_denominator <= f64::EPSILON {
        if source_energy <= f64::EPSILON && rendered_energy <= f64::EPSILON {
            1.0
        } else {
            0.0
        }
    } else {
        (dot / correlation_denominator).clamp(-1.0, 1.0) as f32
    };
    FitMetrics {
        start_sample,
        sample_count: source.len().min(rendered.len()),
        source_energy,
        rendered_energy,
        residual_energy,
        normalized_error,
        explained_energy: 1.0 - normalized_error,
        correlation,
    }
}

fn selected_loom_cluster_id(result: &LoomViewResult) -> Option<usize> {
    result
        .sketch
        .clusters
        .get(result.selected_cluster)
        .map(|cluster| cluster.template.cluster_id)
}

fn nearest_loom_event(
    sketch: &SequenceSketch,
    cluster_id: usize,
    playhead_sample: i64,
) -> Option<u64> {
    sketch
        .events
        .iter()
        .filter(|event| event.cluster_id == cluster_id)
        .min_by_key(|event| event.sample_index.abs_diff(playhead_sample))
        .map(|event| event.id)
}

impl Focusable for Visualizer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Visualizer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (analysis, shared_spectrogram, playhead_seconds, is_playing) = {
            let workbench = self.workbench.read(cx);
            (
                match &workbench.state {
                    ProjectState::Ready(analysis) => Some(analysis.clone()),
                    _ => None,
                },
                workbench.spectrogram.clone(),
                workbench.playhead_seconds,
                workbench.transport_is_playing(),
            )
        };

        if let Some(analysis) = &analysis {
            self.follow_playhead_if_needed(analysis, playhead_seconds);
        }

        if let Some(analysis) = &analysis {
            if self.spectrogram_source.as_ref() != Some(&analysis.path) {
                self.spectrum_settings.db_ceiling = analysis.spectral_peak_db;
                self.spectrum_settings.db_range = 84.0;
                self.local_spectrogram = None;
                self.local_spectral_db = None;
                self.spectrum_transforming = false;
                self.hpss_state = HpssViewState::Idle;
                self.rhythm_state = RhythmViewState::Idle;
                self.loom_state = LoomViewState::Idle;
                if matches!(self.kind, VizKind::Separation | VizKind::Loom) {
                    let span =
                        (18.0 / analysis.duration_seconds.max(f64::EPSILON)).clamp(0.0025, 1.0);
                    let center = (playhead_seconds / analysis.duration_seconds.max(f64::EPSILON))
                        .clamp(0.0, 1.0);
                    self.time_start = (center - span * 0.5).clamp(0.0, 1.0 - span);
                    self.time_end = self.time_start + span;
                }
                self.spectrogram_source = Some(analysis.path.clone());
                if self.kind == VizKind::Rhythm {
                    self.refresh_rhythm(cx);
                }
            }
        }
        let spectrogram = self.local_spectrogram.clone().or(shared_spectrogram);

        let content = if let Some(analysis) = analysis {
            let global_playhead = playhead_seconds / analysis.duration_seconds.max(f64::EPSILON);
            let playhead = ((global_playhead - self.time_start) / self.time_span()) as f32;
            let body = match (self.kind, spectrogram) {
                (VizKind::Waterfall, Some(spectrogram)) => self
                    .render_waterfall(analysis.clone(), spectrogram, playhead, cx)
                    .into_any_element(),
                (VizKind::Rhythm, _) => self
                    .render_rhythm(analysis.clone(), playhead, cx)
                    .into_any_element(),
                (VizKind::Components, _) => self
                    .render_components(analysis.clone(), playhead, cx)
                    .into_any_element(),
                (VizKind::Separation, _) => {
                    self.render_separation(analysis.clone(), playhead_seconds, cx)
                }
                (VizKind::Loom, _) => self.render_loom(analysis.clone(), playhead_seconds, cx),
                _ => empty_state("The spectral image is unavailable.", "Reopen the material."),
            };
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(self.render_header(&analysis, playhead_seconds, is_playing, cx))
                .child(body)
                .into_any_element()
        } else {
            empty_state(
                self.kind.title(),
                "Load material in the workbench; this view will attach automatically.",
            )
        };

        div()
            .key_context("AudecLens")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_toggle))
            .on_action(cx.listener(Self::on_seek_backward))
            .on_action(cx.listener(Self::on_seek_forward))
            .on_action(cx.listener(Self::on_view_zoom_in))
            .on_action(cx.listener(Self::on_view_zoom_out))
            .on_action(cx.listener(Self::on_view_pan_left))
            .on_action(cx.listener(Self::on_view_pan_right))
            .on_action(cx.listener(Self::on_view_fit))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                let delta = event.delta.pixel_delta(window.line_height());
                let horizontal = delta.x / px(100.0);
                let vertical = delta.y / px(100.0);
                let dominant = if horizontal.abs() > vertical.abs() {
                    horizontal
                } else {
                    vertical
                };
                if dominant.abs() > 0.001 {
                    this.pan_time(-f64::from(dominant) * 0.18, cx);
                    cx.stop_propagation();
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .text_sm()
            .child(content)
    }
}

fn section_label(label: &'static str) -> impl IntoElement {
    div().mt_3().text_xs().text_color(rgb(DIM)).child(label)
}

fn viz_control(id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .min_w(px(25.0))
        .h(px(25.0))
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(rgb(BORDER))
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .text_color(rgb(MUTED))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(BORDER)).text_color(rgb(TEXT)))
        .child(label)
}

fn layer_row(label: &'static str, color: u32, active: bool) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .py_1()
        .text_xs()
        .text_color(if active { rgb(TEXT) } else { rgb(DIM) })
        .child(div().size(px(7.0)).rounded_full().bg(rgb(color)))
        .child(label)
}

fn metric(label: &'static str, value: String, color: u32) -> impl IntoElement {
    div()
        .py_2()
        .border_b_1()
        .border_color(rgb(BORDER))
        .flex()
        .items_baseline()
        .justify_between()
        .child(div().text_xs().text_color(rgb(DIM)).child(label))
        .child(div().text_sm().text_color(rgb(color)).child(value))
}

fn empty_state(title: &str, detail: &str) -> gpui::AnyElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .px_8()
        .child(div().text_lg().child(title.to_owned()))
        .child(
            div()
                .max_w(px(520.0))
                .text_color(rgb(MUTED))
                .text_center()
                .child(detail.to_owned()),
        )
        .into_any_element()
}

fn arrangement_ruler(
    start: f64,
    end: f64,
    viewport: TimelineViewport,
    follow: bool,
    loop_enabled: bool,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let zoom = if viewport.span() == 0 {
        1.0
    } else {
        viewport.total_samples.max(1) as f64 / viewport.span() as f64
    };
    div()
        .h(px(62.0))
        .flex_none()
        .flex()
        .border_b_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL_ALT))
        .child(
            div()
                .w(px(ARRANGEMENT_GUTTER))
                .h_full()
                .flex_none()
                .px_3()
                .border_r_1()
                .border_color(rgb(BORDER))
                .flex()
                .items_center()
                .justify_between()
                .child(div().text_xs().text_color(rgb(MUTED)).child("ARRANGEMENT"))
                .child(
                    div()
                        .text_xs()
                        .text_color(if follow { rgb(CYAN) } else { rgb(DIM) })
                        .child(if follow { "FOLLOW" } else { "FREE" }),
                ),
        )
        .child(
            div()
                .h_full()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(time_ruler_range(start, end))
                .child(
                    div()
                        .h(px(34.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_end()
                        .gap_1()
                        .px_2()
                        .bg(rgb(PANEL_ALT))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .px_2()
                                .text_xs()
                                .text_color(if loop_enabled { rgb(AMBER) } else { rgb(DIM) })
                                .child(format!(
                                    "{zoom:.1}× · {}",
                                    if loop_enabled { "LOOP ON" } else { "L loop" }
                                )),
                        )
                        .child(
                            viz_control("arrangement-zoom-out", "−").on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.zoom_timeline(this.playhead_sample(), 2.0, cx)
                                },
                            )),
                        )
                        .child(
                            viz_control("arrangement-zoom-in", "+").on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.zoom_timeline(this.playhead_sample(), 0.5, cx)
                                },
                            )),
                        )
                        .child(
                            viz_control("arrangement-fit", "Fit")
                                .on_click(cx.listener(|this, _, _, cx| this.fit_timeline(cx))),
                        )
                        .child(
                            viz_control("arrangement-follow", "Follow")
                                .on_click(cx.listener(|this, _, _, cx| this.follow_timeline(cx))),
                        ),
                ),
        )
}

fn arrangement_lane(
    label: &'static str,
    detail: &'static str,
    height: Pixels,
    plot: impl IntoElement,
) -> impl IntoElement {
    div()
        .h(height)
        .flex_none()
        .flex()
        .border_b_1()
        .border_color(rgb(BORDER))
        .child(
            div()
                .w(px(ARRANGEMENT_GUTTER))
                .h_full()
                .flex_none()
                .px_3()
                .border_r_1()
                .border_color(rgb(BORDER))
                .bg(rgb(PANEL_ALT))
                .flex()
                .flex_col()
                .justify_center()
                .gap_1()
                .child(div().text_xs().text_color(rgb(TEXT)).child(label))
                .child(div().text_xs().text_color(rgb(DIM)).child(detail)),
        )
        .child(div().relative().h_full().flex_1().min_w_0().child(plot))
}

fn range_fractions(range: SampleRange, viewport: TimelineViewport) -> Option<(f32, f32)> {
    let start = range.start.get().max(0) as u64;
    let end = range.end.get().max(0) as u64;
    if end < viewport.start_sample || start > viewport.end_sample {
        return None;
    }
    Some((viewport.fraction_of(start), viewport.fraction_of(end)))
}

fn arrangement_overlay(
    timeline_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    playhead: f32,
    selection: Option<(f32, f32)>,
    loop_range: Option<(f32, f32)>,
    loop_enabled: bool,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| {
            *timeline_bounds.lock().unwrap() = Some(bounds);
            bounds
        },
        move |bounds, _, window, _| {
            for fraction in [0.25_f32, 0.5, 0.75] {
                let x = bounds.origin.x + bounds.size.width * fraction;
                window.paint_quad(quad(
                    Bounds::new(
                        point(x, bounds.origin.y),
                        gpui::size(px(1.0), bounds.size.height),
                    ),
                    px(0.0),
                    rgba(0xffffff12),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }

            if let Some((start, end)) = selection {
                let left = bounds.origin.x + bounds.size.width * start.min(end);
                let right = bounds.origin.x + bounds.size.width * start.max(end);
                window.paint_quad(quad(
                    Bounds::new(
                        point(left, bounds.origin.y),
                        gpui::size((right - left).max(px(1.0)), bounds.size.height),
                    ),
                    px(0.0),
                    rgba(0x50d8d71f),
                    px(1.0),
                    rgba(0x50d8d7aa),
                    Default::default(),
                ));
            }

            if let Some((start, end)) = loop_range {
                let left = bounds.origin.x + bounds.size.width * start.min(end);
                let right = bounds.origin.x + bounds.size.width * start.max(end);
                let color = if loop_enabled {
                    rgba(0xf6b760ee)
                } else {
                    rgba(0x59657999)
                };
                window.paint_quad(quad(
                    Bounds::new(
                        point(left, bounds.origin.y),
                        gpui::size((right - left).max(px(1.0)), px(4.0)),
                    ),
                    px(1.0),
                    color,
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }

            paint_playhead(bounds, playhead, window);
        },
    )
    .absolute()
    .left(px(ARRANGEMENT_GUTTER))
    .right_0()
    .top_0()
    .bottom_0()
}

fn time_ruler_range(start: f64, end: f64) -> impl IntoElement {
    let ticks = 8;
    div()
        .h(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_between()
        .px_2()
        .border_b_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL_ALT))
        .children((0..=ticks).map(|tick| {
            div().text_xs().text_color(rgb(DIM)).child(format_time(
                start + (end - start) * tick as f64 / ticks as f64,
            ))
        }))
}

fn cropped_spectrogram(
    image: Arc<Image>,
    time_start: f64,
    time_end: f64,
    frequency_start: f32,
    frequency_end: f32,
) -> impl IntoElement {
    let time_span = (time_end - time_start).max(1.0e-6) as f32;
    let frequency_span = (frequency_end - frequency_start).max(1.0e-6);
    let source_top = 1.0 - frequency_end;
    img(image)
        .absolute()
        .left(relative(-(time_start as f32) / time_span))
        .top(relative(-source_top / frequency_span))
        .w(relative(1.0 / time_span))
        .h(relative(1.0 / frequency_span))
        .object_fit(ObjectFit::Fill)
}

fn slice_visible<T: Clone>(values: &[T], start: f64, end: f64) -> Vec<T> {
    if values.is_empty() {
        return Vec::new();
    }
    let first = (start.clamp(0.0, 1.0) * values.len() as f64).floor() as usize;
    let last = (end.clamp(0.0, 1.0) * values.len() as f64).ceil() as usize;
    values[first.min(values.len() - 1)..last.clamp(first + 1, values.len())].to_vec()
}

fn lane(label: &'static str, height: Pixels, plot: impl IntoElement) -> impl IntoElement {
    div()
        .relative()
        .h(height)
        .flex_none()
        .border_b_1()
        .border_color(rgb(BORDER))
        .child(plot)
        .child(
            div()
                .absolute()
                .top_2()
                .left_2()
                .px_2()
                .py_1()
                .rounded_sm()
                .bg(rgba(0x090b10cc))
                .text_xs()
                .text_color(rgb(MUTED))
                .child(label),
        )
}

fn waveform_plot(waveform: Vec<WaveformBin>, playhead: f32) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            if let Some(path) = waveform_envelope(&waveform, bounds, true) {
                window.paint_path(path, rgba(0x50d8d7a8));
            }
            if let Some(path) = waveform_envelope(&waveform, bounds, false) {
                window.paint_path(path, rgba(0xf172b69a));
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .size_full()
}

fn mono_waveform_bins(samples: &[f32], target_bins: usize) -> Vec<WaveformBin> {
    if samples.is_empty() || target_bins == 0 {
        return Vec::new();
    }
    let bin_count = target_bins.min(samples.len());
    (0..bin_count)
        .map(|bin| {
            let start = samples.len() * bin / bin_count;
            let end = samples.len() * (bin + 1) / bin_count;
            let mut minimum = f32::INFINITY;
            let mut maximum = f32::NEG_INFINITY;
            for sample in samples[start..end].iter().copied() {
                minimum = minimum.min(sample);
                maximum = maximum.max(sample);
            }
            WaveformBin {
                left_min: minimum,
                left_max: maximum,
                right_min: minimum,
                right_max: maximum,
            }
        })
        .collect()
}

fn waveform_envelope(
    waveform: &[WaveformBin],
    bounds: Bounds<Pixels>,
    left_channel: bool,
) -> Option<gpui::Path<Pixels>> {
    if waveform.len() < 2 {
        return None;
    }
    let center = bounds.origin.y + bounds.size.height * if left_channel { 0.28 } else { 0.72 };
    let amplitude = bounds.size.height * 0.20;
    let mut builder = PathBuilder::fill();
    for (index, bin) in waveform.iter().enumerate() {
        let fraction = index as f32 / (waveform.len() - 1) as f32;
        let value = if left_channel {
            bin.left_max
        } else {
            bin.right_max
        };
        let location = point(
            bounds.origin.x + bounds.size.width * fraction,
            center - amplitude * value.clamp(-1.0, 1.0),
        );
        if index == 0 {
            builder.move_to(location);
        } else {
            builder.line_to(location);
        }
    }
    for (index, bin) in waveform.iter().enumerate().rev() {
        let fraction = index as f32 / (waveform.len() - 1) as f32;
        let value = if left_channel {
            bin.left_min
        } else {
            bin.right_min
        };
        builder.line_to(point(
            bounds.origin.x + bounds.size.width * fraction,
            center - amplitude * value.clamp(-1.0, 1.0),
        ));
    }
    builder.close();
    builder.build().ok()
}

fn dual_feature_plot(
    features: Vec<FeatureFrame>,
    playhead: f32,
    first: fn(FeatureFrame) -> f32,
    second: fn(FeatureFrame) -> f32,
    first_color: gpui::Rgba,
    second_color: gpui::Rgba,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            if let Some(path) = feature_area(&features, bounds, first) {
                window.paint_path(path, first_color);
            }
            if let Some(path) = feature_line(&features, bounds, second) {
                window.paint_path(path, second_color);
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .size_full()
}

fn feature_plot(
    features: Vec<FeatureFrame>,
    playhead: f32,
    value: fn(FeatureFrame) -> f32,
    color: gpui::Rgba,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            if let Some(path) = feature_area(&features, bounds, value) {
                window.paint_path(path, color);
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .size_full()
}

fn rhythm_plot(
    rhythm: RhythmAnalysis,
    start_seconds: f64,
    end_seconds: f64,
    playhead: f32,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            let duration = (end_seconds - start_seconds).max(f64::EPSILON);
            for time in rhythm.beat_times.iter().copied() {
                if time < start_seconds || time > end_seconds {
                    continue;
                }
                let fraction = ((time - start_seconds) / duration) as f32;
                let x = bounds.origin.x + bounds.size.width * fraction;
                window.paint_quad(quad(
                    Bounds::new(
                        point(x, bounds.origin.y),
                        gpui::size(px(1.0), bounds.size.height),
                    ),
                    px(0.0),
                    rgba(0xffffff18),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }

            for onset in rhythm.onsets.iter().copied() {
                if onset.time_seconds < start_seconds || onset.time_seconds > end_seconds {
                    continue;
                }
                let fraction = ((onset.time_seconds - start_seconds) / duration) as f32;
                let x = bounds.origin.x + bounds.size.width * fraction;
                let (row, color) = onset_style(onset);
                let lane_height = bounds.size.height / 3.0;
                let max_height = lane_height * 0.84;
                let height = (max_height * onset.strength.max(0.12)).max(px(2.0));
                let bottom = bounds.origin.y + lane_height * (row as f32 + 0.92);
                window.paint_quad(quad(
                    Bounds::new(
                        point(x - px(1.0), bottom - height),
                        gpui::size(px(2.0), height),
                    ),
                    px(1.0),
                    color,
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .size_full()
}

fn rhythm_deprojection_plot(
    rhythm: Arc<RhythmDeprojection>,
    family_ids: Vec<usize>,
    visible_start: usize,
    visible_end: usize,
    playhead: f32,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            let row_height = px(RHYTHM_ROW_HEIGHT);
            for row in 1..=family_ids.len() {
                let y = bounds.origin.y + row_height * row as f32;
                window.paint_quad(quad(
                    Bounds::new(
                        point(bounds.origin.x, y),
                        gpui::size(bounds.size.width, px(1.0)),
                    ),
                    px(0.0),
                    rgba(0xffffff14),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }

            if let Some(phase) = rhythm.beat_phase_hypotheses.first() {
                for sample in phase.beat_samples.iter().copied() {
                    paint_sample_marker(
                        sample,
                        visible_start,
                        visible_end,
                        bounds,
                        px(1.0),
                        rgba(0xffffff18),
                        window,
                    );
                }
            }
            if let Some(downbeats) = rhythm.downbeat_hypotheses.first() {
                for sample in downbeats.downbeat_samples.iter().copied() {
                    paint_sample_marker(
                        sample,
                        visible_start,
                        visible_end,
                        bounds,
                        px(2.0),
                        rgba(0xf6b76055),
                        window,
                    );
                }
            }
            for occurrence in rhythm
                .patterns
                .iter()
                .take(4)
                .flat_map(|pattern| &pattern.occurrences)
            {
                paint_sample_marker(
                    occurrence.start_sample,
                    visible_start,
                    visible_end,
                    bounds,
                    px(1.0),
                    rgba(0xf172b650),
                    window,
                );
            }

            let peak_strength = rhythm
                .hits
                .iter()
                .filter(|hit| spans_overlap(hit.span, visible_start, visible_end))
                .map(|hit| hit.novelty_strength)
                .fold(1.0e-6_f32, f32::max);
            for hit in &rhythm.hits {
                let Some(family_id) = hit.family else {
                    continue;
                };
                let Some(row) = family_ids
                    .iter()
                    .position(|candidate| *candidate == family_id)
                else {
                    continue;
                };
                let Some((start, end)) = clip_sample_span(hit.span, visible_start, visible_end)
                else {
                    continue;
                };
                let x = bounds.origin.x + bounds.size.width * start;
                let right = bounds.origin.x + bounds.size.width * end;
                let width = (right - x).max(px(2.0));
                let strength = (hit.novelty_strength / peak_strength)
                    .sqrt()
                    .clamp(0.18, 1.0);
                let inset = px(5.0 + (1.0 - strength) * 15.0);
                let top = bounds.origin.y + row_height * row as f32 + inset;
                let height = (row_height - inset * 2.0).max(px(3.0));
                window.paint_quad(quad(
                    Bounds::new(point(x, top), gpui::size(width, height)),
                    px(2.0),
                    cluster_rgba(family_id),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .size_full()
}

fn paint_sample_marker(
    sample: usize,
    visible_start: usize,
    visible_end: usize,
    bounds: Bounds<Pixels>,
    width: Pixels,
    color: gpui::Rgba,
    window: &mut Window,
) {
    if sample < visible_start || sample >= visible_end || visible_end <= visible_start {
        return;
    }
    let fraction = (sample - visible_start) as f32 / (visible_end - visible_start) as f32;
    let x = bounds.origin.x + bounds.size.width * fraction;
    window.paint_quad(quad(
        Bounds::new(
            point(x - width * 0.5, bounds.origin.y),
            gpui::size(width, bounds.size.height),
        ),
        px(0.0),
        color,
        px(0.0),
        rgba(0x00000000),
        Default::default(),
    ));
}

fn spans_overlap(span: SampleSpan, visible_start: usize, visible_end: usize) -> bool {
    span.start < visible_end && span.end > visible_start && visible_start < visible_end
}

fn clip_sample_span(
    span: SampleSpan,
    visible_start: usize,
    visible_end: usize,
) -> Option<(f32, f32)> {
    if !spans_overlap(span, visible_start, visible_end) {
        return None;
    }
    let length = visible_end.saturating_sub(visible_start).max(1) as f32;
    let start = span.start.max(visible_start).saturating_sub(visible_start) as f32 / length;
    let end = span.end.min(visible_end).saturating_sub(visible_start) as f32 / length;
    Some((start.clamp(0.0, 1.0), end.clamp(0.0, 1.0)))
}

fn visible_hit_count(rhythm: &RhythmDeprojection, start: usize, end: usize) -> usize {
    rhythm
        .hits
        .iter()
        .filter(|hit| spans_overlap(hit.span, start, end))
        .count()
}

fn visible_rhythm_family_ids(
    rhythm: &RhythmDeprojection,
    start: usize,
    end: usize,
    maximum: usize,
) -> Vec<usize> {
    let mut families = rhythm
        .event_families
        .iter()
        .filter_map(|family| {
            let visible = family
                .event_indices
                .iter()
                .filter(|index| {
                    rhythm
                        .hits
                        .get(**index)
                        .is_some_and(|hit| spans_overlap(hit.span, start, end))
                })
                .count();
            (visible > 0).then_some((family.id, visible, family.evidence))
        })
        .collect::<Vec<_>>();
    families.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.total_cmp(&a.2))
            .then_with(|| a.0.cmp(&b.0))
    });
    families.truncate(maximum);
    families.into_iter().map(|family| family.0).collect()
}

fn tempo_hypotheses_summary(rhythm: &RhythmDeprojection) -> String {
    if rhythm.tempo_hypotheses.is_empty() {
        return "No stable tempo hypothesis · the pulse remains ambiguous".to_owned();
    }
    let candidates = rhythm
        .tempo_hypotheses
        .iter()
        .take(4)
        .map(|tempo| {
            let relation = match tempo.relation {
                TempoRelation::Independent => "",
                TempoRelation::HalfTimeOf(_) => " ½-time",
                TempoRelation::DoubleTimeOf(_) => " 2×-time",
            };
            format!(
                "#{rank} {bpm:.1} BPM {evidence:.0}%{relation}",
                rank = tempo.rank + 1,
                bpm = tempo.bpm,
                evidence = tempo.evidence * 100.0
            )
        })
        .collect::<Vec<_>>()
        .join("   ·   ");
    format!("Tempo alternatives: {candidates}")
}

fn loom_event_plot(
    sketch: SequenceSketch,
    start_seconds: f64,
    end_seconds: f64,
    playhead: f32,
    selected_cluster_id: usize,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            let duration = (end_seconds - start_seconds).max(f64::EPSILON);
            let rows = sketch.clusters.len().max(1);
            let row_height = bounds.size.height / rows as f32;
            if let Some(selected_row) = sketch
                .clusters
                .iter()
                .position(|cluster| cluster.template.cluster_id == selected_cluster_id)
            {
                window.paint_quad(quad(
                    Bounds::new(
                        point(
                            bounds.origin.x,
                            bounds.origin.y + row_height * selected_row as f32,
                        ),
                        gpui::size(bounds.size.width, row_height),
                    ),
                    px(0.0),
                    rgba(0x50d8d70d),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            for row in 1..rows {
                let y = bounds.origin.y + row_height * row as f32;
                window.paint_quad(quad(
                    Bounds::new(
                        point(bounds.origin.x, y),
                        gpui::size(bounds.size.width, px(1.0)),
                    ),
                    px(0.0),
                    rgba(0xffffff12),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            for event in &sketch.events {
                let seconds = event.sample_index as f64 / f64::from(sketch.sample_rate);
                if seconds < start_seconds || seconds > end_seconds {
                    continue;
                }
                let Some(row) = sketch
                    .clusters
                    .iter()
                    .position(|cluster| cluster.template.cluster_id == event.cluster_id)
                else {
                    continue;
                };
                let cluster_enabled = sketch.clusters[row].enabled;
                let fraction = ((seconds - start_seconds) / duration) as f32;
                let x = bounds.origin.x + bounds.size.width * fraction;
                let height = row_height * (0.18 + 0.68 * event.gain.abs().clamp(0.0, 1.6) / 1.6);
                let bottom = bounds.origin.y + row_height * (row as f32 + 0.90);
                let color = if event.enabled && cluster_enabled {
                    cluster_rgba(event.cluster_id)
                } else {
                    rgba(0x59657966)
                };
                window.paint_quad(quad(
                    Bounds::new(
                        point(x - px(1.5), bottom - height),
                        gpui::size(px(3.0), height),
                    ),
                    px(1.0),
                    color,
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .size_full()
}

fn component_activation_plot(
    decomposition: ComponentDecomposition,
    start: f64,
    end: f64,
    playhead: f32,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            let rows = decomposition.components.len().max(1);
            let row_height = bounds.size.height / rows as f32;
            let first = (start.clamp(0.0, 1.0) * decomposition.frames as f64).floor() as usize;
            let last = (end.clamp(0.0, 1.0) * decomposition.frames as f64).ceil() as usize;
            let first = first.min(decomposition.frames.saturating_sub(1));
            let last = last.clamp(first + 1, decomposition.frames);

            for row in 1..rows {
                let y = bounds.origin.y + row_height * row as f32;
                window.paint_quad(quad(
                    Bounds::new(
                        point(bounds.origin.x, y),
                        gpui::size(bounds.size.width, px(1.0)),
                    ),
                    px(0.0),
                    rgba(0xffffff14),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }

            for (row, component) in decomposition.components.iter().enumerate() {
                let values = &component.activation[first..last];
                if values.len() < 2 {
                    continue;
                }
                let peak = values.iter().copied().fold(0.0_f32, f32::max).max(1.0e-9);
                let top = bounds.origin.y + row_height * row as f32;
                let bottom = top + row_height;
                let mut builder = PathBuilder::fill();
                builder.move_to(point(bounds.origin.x, bottom));
                for (index, value) in values.iter().copied().enumerate() {
                    let fraction = index as f32 / (values.len() - 1) as f32;
                    builder.line_to(point(
                        bounds.origin.x + bounds.size.width * fraction,
                        bottom - row_height * 0.88 * (value / peak).sqrt().clamp(0.0, 1.0),
                    ));
                }
                builder.line_to(point(bounds.origin.x + bounds.size.width, bottom));
                builder.close();
                if let Ok(path) = builder.build() {
                    window.paint_path(path, cluster_rgba(row));
                }
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .size_full()
}

fn cluster_color(index: usize) -> gpui::Rgba {
    rgb([
        CYAN, MAGENTA, AMBER, LIME, 0x8e9cff, 0xe99172, 0x78d5a3, 0xd8a7ff,
    ][index % 8])
}

fn cluster_rgba(index: usize) -> gpui::Rgba {
    rgba(
        [
            0x50d8d7dd, 0xf172b6dd, 0xf6b760dd, 0xa7d877dd, 0x8e9cffdd, 0xe99172dd, 0x78d5a3dd,
            0xd8a7ffdd,
        ][index % 8],
    )
}

fn cluster_spectrum_plot(spectrum: Vec<f32>, color: gpui::Rgba) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            if spectrum.len() < 2 {
                return;
            }
            let peak = spectrum.iter().copied().fold(0.0_f32, f32::max).max(1.0e-8);
            let mut builder = PathBuilder::stroke(px(1.0));
            for (index, value) in spectrum.iter().copied().enumerate() {
                let x = bounds.origin.x
                    + bounds.size.width * index as f32 / (spectrum.len() - 1) as f32;
                let y = bounds.origin.y + bounds.size.height * (1.0 - value / peak);
                if index == 0 {
                    builder.move_to(point(x, y));
                } else {
                    builder.line_to(point(x, y));
                }
            }
            if let Ok(path) = builder.build() {
                window.paint_path(path, color);
            }
        },
    )
    .h(px(8.0))
    .w_full()
}

fn onset_style(onset: OnsetEvent) -> (usize, gpui::Rgba) {
    if onset.high >= onset.mid && onset.high >= onset.low {
        (0, rgba(0xf6b760dd))
    } else if onset.mid >= onset.low {
        (1, rgba(0xf172b6dd))
    } else {
        (2, rgba(0x50d8d7dd))
    }
}

fn feature_area(
    features: &[FeatureFrame],
    bounds: Bounds<Pixels>,
    value: fn(FeatureFrame) -> f32,
) -> Option<gpui::Path<Pixels>> {
    if features.len() < 2 {
        return None;
    }
    let bottom = bounds.origin.y + bounds.size.height;
    let mut builder = PathBuilder::fill();
    builder.move_to(point(bounds.origin.x, bottom));
    for (index, feature) in features.iter().copied().enumerate() {
        let fraction = index as f32 / (features.len() - 1) as f32;
        builder.line_to(point(
            bounds.origin.x + bounds.size.width * fraction,
            bottom - bounds.size.height * value(feature).clamp(0.0, 1.0),
        ));
    }
    builder.line_to(point(bounds.origin.x + bounds.size.width, bottom));
    builder.close();
    builder.build().ok()
}

fn feature_line(
    features: &[FeatureFrame],
    bounds: Bounds<Pixels>,
    value: fn(FeatureFrame) -> f32,
) -> Option<gpui::Path<Pixels>> {
    if features.len() < 2 {
        return None;
    }
    let mut builder = PathBuilder::stroke(px(1.5));
    for (index, feature) in features.iter().copied().enumerate() {
        let fraction = index as f32 / (features.len() - 1) as f32;
        let location = point(
            bounds.origin.x + bounds.size.width * fraction,
            bounds.origin.y + bounds.size.height * (1.0 - value(feature).clamp(0.0, 1.0)),
        );
        if index == 0 {
            builder.move_to(location);
        } else {
            builder.line_to(location);
        }
    }
    builder.build().ok()
}

fn timeline_overlay(
    timeline_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    playhead: f32,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| {
            *timeline_bounds.lock().unwrap() = Some(bounds);
            bounds
        },
        move |bounds, _, window, _| {
            for fraction in [0.25_f32, 0.5, 0.75] {
                let x = bounds.origin.x + bounds.size.width * fraction;
                window.paint_quad(quad(
                    Bounds::new(
                        point(x, bounds.origin.y),
                        gpui::size(px(1.0), bounds.size.height),
                    ),
                    px(0.0),
                    rgba(0xffffff14),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .absolute()
    .inset_0()
}

fn paint_playhead(bounds: Bounds<Pixels>, fraction: f32, window: &mut Window) {
    if !(0.0..=1.0).contains(&fraction) {
        return;
    }
    let x = bounds.origin.x + bounds.size.width * fraction;
    window.paint_quad(quad(
        Bounds::new(
            point(x, bounds.origin.y),
            gpui::size(px(1.0), bounds.size.height),
        ),
        px(0.0),
        rgba(0xe8edf5dd),
        px(0.0),
        rgba(0x00000000),
        Default::default(),
    ));
}

fn format_time(seconds: f64) -> String {
    if !seconds.is_finite() {
        return "0:00.0".to_owned();
    }
    let minutes = (seconds / 60.0).floor() as u64;
    let remainder = seconds - minutes as f64 * 60.0;
    format!("{minutes}:{remainder:04.1}")
}

fn stable_source_id(path: &str, frame_count: u64, sample_rate: u32) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in path
        .as_bytes()
        .iter()
        .copied()
        .chain(frame_count.to_le_bytes())
        .chain(sample_rate.to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn format_frequency(frequency: f32) -> String {
    if frequency >= 1_000.0 {
        format!("{:.2} kHz", frequency / 1_000.0)
    } else {
        format!("{frequency:.1} Hz")
    }
}

pub fn window_options(cx: &mut App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::centered(
            None,
            gpui::size(px(1_340.0), px(820.0)),
            cx,
        ))),
        window_min_size: Some(gpui::size(px(980.0), px(680.0))),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(SharedString::from("audec — reverse DAW")),
            appears_transparent: true,
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build the real dock/tab workspace around the existing workbench and lens
/// entities. The initial single-pane layout preserves the workbench's useful
/// vertical detail; Guise can then split, tab, and tear off these same entity
/// handles without resetting their view state.
pub struct DawWorkspace {
    workspace: Entity<DynamicWorkspaceRoot>,
    workbench: Entity<Workbench>,
    /// Latest portable layout publication. File actions can persist this in
    /// the existing project envelope once they own save/open coordination.
    workspace_document: Arc<Mutex<WorkspaceDocument>>,
}

impl DawWorkspace {
    pub fn workspace_document(&self) -> WorkspaceDocument {
        self.workspace_document
            .lock()
            .map(|document| document.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }
}

impl Render for DawWorkspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("Audec")
            .size_full()
            .on_action(cx.listener(|this, _: &OpenAudio, _, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.choose_audio(cx));
            }))
            .on_action(cx.listener(|this, _: &TogglePlayback, _, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.toggle_playback(cx));
            }))
            .on_action(cx.listener(|this, _: &SeekBackward, _, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.seek_relative(-5.0, cx));
            }))
            .on_action(cx.listener(|this, _: &SeekForward, _, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.seek_relative(5.0, cx));
            }))
            .on_action(cx.listener(|this, _: &OpenWaterfall, _, cx| {
                this.workbench.update(cx, |workbench, cx| {
                    workbench.open_visualizer(VizKind::Waterfall, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &OpenRhythm, _, cx| {
                this.workbench.update(cx, |workbench, cx| {
                    workbench.open_visualizer(VizKind::Rhythm, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &OpenComponents, _, cx| {
                this.workbench.update(cx, |workbench, cx| {
                    workbench.open_visualizer(VizKind::Components, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &OpenSeparation, _, cx| {
                this.workbench.update(cx, |workbench, cx| {
                    workbench.open_visualizer(VizKind::Separation, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &OpenLoom, _, cx| {
                this.workbench.update(cx, |workbench, cx| {
                    workbench.open_visualizer(VizKind::Loom, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &OpenArrangementEditor, _, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.open_arrangement_editor(cx));
            }))
            .on_action(cx.listener(|this, _: &OpenSequencerEditor, _, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.open_sequencer_editor(cx));
            }))
            .on_action(cx.listener(|this, _: &OpenMixer, _, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.open_mixer(cx));
            }))
            .on_action(cx.listener(|this, _: &OpenAutomation, _, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.open_automation(cx));
            }))
            .on_action(cx.listener(|this, _: &OpenAssets, _, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.open_assets(cx));
            }))
            .on_action(cx.listener(|this, _: &ViewZoomIn, _, cx| {
                this.workbench.update(cx, |workbench, cx| {
                    workbench.zoom_timeline(workbench.playhead_sample(), 0.5, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &ViewZoomOut, _, cx| {
                this.workbench.update(cx, |workbench, cx| {
                    workbench.zoom_timeline(workbench.playhead_sample(), 2.0, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &ViewPanLeft, _, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.pan_timeline(-0.2, cx));
            }))
            .on_action(cx.listener(|this, _: &ViewPanRight, _, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.pan_timeline(0.2, cx));
            }))
            .on_action(cx.listener(|this, _: &ViewFit, _, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.fit_timeline(cx));
            }))
            .on_action(cx.listener(|this, _: &ViewFollow, _, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.follow_timeline(cx));
            }))
            .on_action(cx.listener(|this, _: &SetLoopFromSelection, _, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.set_loop_from_selection(cx));
            }))
            .on_action(cx.listener(|this, _: &ToggleLoop, _, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.toggle_loop(cx));
            }))
            .child(self.workspace.clone())
    }
}

pub fn create_workspace(
    initial_path: Option<PathBuf>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<DawWorkspace> {
    let workbench = cx.new(|cx| Workbench::new(initial_path, cx));

    let waterfall = cx.new(|cx| Visualizer::new(VizKind::Waterfall, workbench.clone(), cx));
    let rhythm = cx.new(|cx| Visualizer::new(VizKind::Rhythm, workbench.clone(), cx));
    let components = cx.new(|cx| Visualizer::new(VizKind::Components, workbench.clone(), cx));
    let separation = cx.new(|cx| Visualizer::new(VizKind::Separation, workbench.clone(), cx));
    let loom = cx.new(|cx| Visualizer::new(VizKind::Loom, workbench.clone(), cx));

    let mut registry = PaneRegistry::new();
    registry
        .register_entity(
            BuiltinView::Track,
            "Arrangement + evidence",
            workbench.clone(),
        )
        .register_entity(BuiltinView::Waterfall, "Spectral waterfall", waterfall)
        .register_entity(BuiltinView::Rhythm, "Rhythm deprojection", rhythm)
        .register_entity(BuiltinView::Components, "Recurring components", components)
        .register_entity(BuiltinView::Separation, "Harmonic / transient", separation)
        .register_entity(BuiltinView::Loom, "Loom reconstruction", loom);

    let mut model = WorkspaceModel::new();
    let initial_tabs = WorkspaceLayout::Pane {
        items: BuiltinView::ALL.to_vec(),
        active: 0,
    }
    .to_guise();
    model
        .replace_main_layout(&initial_tabs)
        .expect("the built-in workspace layout is valid");

    let bootstrap = DynamicWorkspaceBootstrap::from_legacy_six(model, registry)
        .expect("the built-in workspace migrates to the dynamic document");
    let workspace_document = Arc::new(Mutex::new(bootstrap.document().clone()));
    let published_document = workspace_document.clone();
    let hooks = DynamicWorkspaceHooks::default()
        .on_snapshot(move |document, _cx| match published_document.lock() {
            Ok(mut published) => *published = document,
            Err(poisoned) => *poisoned.into_inner() = document,
        })
        .on_event(|event, _cx| match event {
            DynamicWorkspaceUiEvent::CloseDenied { view, message } => {
                eprintln!("workspace view {} remained open: {message}", view.0);
            }
            DynamicWorkspaceUiEvent::WindowOpenFailed { view, message } => {
                eprintln!("opening workspace view {}: {message}", view.0);
            }
            _ => {}
        });
    let workspace = cx.new(|cx| {
        bootstrap
            .build(
                None::<fn(&mut Window, &mut App) -> gpui::AnyElement>,
                hooks,
                window,
                cx,
            )
            .expect("the migrated dynamic workspace is valid")
    });
    // Guise creates and focuses its pane group during DynamicWorkspaceRoot::new.
    // Restore focus to the active workbench after the workspace exists so its
    // transport/editor shortcuts are live immediately.
    window.focus(&workbench.focus_handle(cx));
    cx.new(|_| DawWorkspace {
        workspace,
        workbench,
        workspace_document,
    })
}

fn visualizer_window_options(kind: VizKind, cx: &mut App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::centered(
            None,
            gpui::size(px(1_080.0), px(720.0)),
            cx,
        ))),
        window_min_size: Some(gpui::size(px(720.0), px(480.0))),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(SharedString::from(format!("audec — {}", kind.title()))),
            appears_transparent: true,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn editor_window_options(title: &str, cx: &mut App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::centered(
            None,
            gpui::size(px(1_280.0), px(760.0)),
            cx,
        ))),
        window_min_size: Some(gpui::size(px(900.0), px(560.0))),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(SharedString::from(format!("audec — {title}"))),
            appears_transparent: true,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn open_editor_entity<T>(entity: Entity<T>, title: &'static str, cx: &mut Context<Workbench>)
where
    T: Render + Focusable + 'static,
{
    let options = editor_window_options(title, cx);
    cx.defer(move |cx| {
        if let Err(error) = cx.open_window(options, move |window, cx| {
            window.focus(&entity.focus_handle(cx));
            entity.clone()
        }) {
            eprintln!("opening {title}: {error:#}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_transport_time() {
        assert_eq!(format_time(0.0), "0:00.0");
        assert_eq!(format_time(61.25), "1:01.2");
    }

    #[test]
    fn formats_frequency_scale() {
        assert_eq!(format_frequency(440.0), "440.0 Hz");
        assert_eq!(format_frequency(2_000.0), "2.00 kHz");
    }

    #[test]
    fn arrangement_ranges_are_relative_to_the_visible_viewport() {
        let viewport = TimelineViewport::around(1_000, 500, 200);
        let range = SampleRange::new(Sample::new(450), Sample::new(550));
        assert_eq!(range_fractions(range, viewport), Some((0.25, 0.75)));
    }

    #[test]
    fn arrangement_ranges_clip_at_viewport_edges() {
        let viewport = TimelineViewport::around(1_000, 500, 200);
        assert_eq!(
            range_fractions(
                SampleRange::new(Sample::new(300), Sample::new(450)),
                viewport
            ),
            Some((0.0, 0.25))
        );
        assert_eq!(
            range_fractions(
                SampleRange::new(Sample::new(700), Sample::new(800)),
                viewport
            ),
            None
        );
    }

    #[test]
    fn rhythm_hit_spans_clip_to_the_visible_sample_range() {
        assert_eq!(
            clip_sample_span(
                SampleSpan {
                    start: 50,
                    end: 150,
                },
                100,
                300,
            ),
            Some((0.0, 0.25))
        );
        assert_eq!(
            clip_sample_span(
                SampleSpan {
                    start: 300,
                    end: 340,
                },
                100,
                300,
            ),
            None
        );
    }

    #[test]
    fn rhythm_family_rows_are_ranked_by_visible_recurrence() {
        use crate::rhythm::{EventFamilyHypothesis, HitObservation};

        let mut rhythm = RhythmDeprojection {
            sample_frames: 1_000,
            ..RhythmDeprojection::default()
        };
        rhythm.hits = vec![
            HitObservation {
                span: SampleSpan {
                    start: 100,
                    end: 120,
                },
                family: Some(0),
                ..HitObservation::default()
            },
            HitObservation {
                span: SampleSpan {
                    start: 150,
                    end: 180,
                },
                family: Some(1),
                ..HitObservation::default()
            },
            HitObservation {
                span: SampleSpan {
                    start: 220,
                    end: 250,
                },
                family: Some(1),
                ..HitObservation::default()
            },
            HitObservation {
                span: SampleSpan {
                    start: 800,
                    end: 840,
                },
                family: Some(0),
                ..HitObservation::default()
            },
        ];
        rhythm.event_families = vec![
            EventFamilyHypothesis {
                id: 0,
                event_indices: vec![0, 3],
                evidence: 0.95,
                ..EventFamilyHypothesis::default()
            },
            EventFamilyHypothesis {
                id: 1,
                event_indices: vec![1, 2],
                evidence: 0.55,
                ..EventFamilyHypothesis::default()
            },
        ];

        assert_eq!(visible_rhythm_family_ids(&rhythm, 0, 400, 5), vec![1, 0]);
        assert_eq!(visible_rhythm_family_ids(&rhythm, 700, 900, 5), vec![0]);
    }

    #[test]
    fn rhythm_family_rows_obey_the_layout_cap() {
        use crate::rhythm::{EventFamilyHypothesis, HitObservation};

        let mut rhythm = RhythmDeprojection::default();
        for id in 0..8 {
            rhythm.hits.push(HitObservation {
                span: SampleSpan {
                    start: id * 10,
                    end: id * 10 + 5,
                },
                family: Some(id),
                ..HitObservation::default()
            });
            rhythm.event_families.push(EventFamilyHypothesis {
                id,
                event_indices: vec![id],
                ..EventFamilyHypothesis::default()
            });
        }
        assert_eq!(visible_rhythm_family_ids(&rhythm, 0, 100, 5).len(), 5);
        assert_eq!(
            RHYTHM_ROW_HEIGHT * RHYTHM_MAX_VISIBLE_FAMILIES as f32,
            290.0
        );
    }
}
