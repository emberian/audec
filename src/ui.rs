use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    actions, canvas, div, img, point, prelude::*, px, quad, relative, rgb, rgba, App, Bounds,
    Context, Entity, FocusHandle, Focusable, Image, ImageFormat, IntoElement, KeyBinding, Menu,
    MenuItem, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, PathBuilder,
    PathPromptOptions, Pixels, PromptButton, PromptLevel, Render, ScrollHandle, ScrollWheelEvent,
    SharedString, SystemMenuType, Task, WeakEntity, Window, WindowHandle, WindowOptions,
};

use crate::air_query::workbench::{
    FactKindDto, QueryDocument, QueryDocumentId, QueryTermDto, WorkbenchPaneFactory,
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
use crate::arrangement_interaction::{SelectionIntent, SelectionMode};
use crate::arrangement_view::{
    ArrangementView, ArrangementViewEvent, ArrangementViewport, ArrangementWaveformProvider,
    ArrangementWaveformSource,
};
use crate::artifact_catalog::comparison_hydration::ArtifactComparisonPayload;
use crate::artifact_catalog::{
    sha256_content, ArtifactCatalog, ArtifactDescriptor, ArtifactId, ArtifactKind, ContentDigest,
};
use crate::aspect::{Aspect, FrameSpan, SignalLayer};
use crate::asset_view::{AssetBrowserEvent, AssetBrowserState, AssetBrowserView};
use crate::assets::{
    AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration, AssetRegistry,
    ContentFingerprint, DecodedAudioMetadata, SampleFrames,
};
use crate::audio::{AudioFormat, FrameRange, ProjectAudio, ProjectFrame, TransportMode};
use crate::audio_host::AudioHost;
use crate::comparison_controller::ComparisonSelectionRequest;
use crate::comparison_runtime::executor::{
    ComparisonProductCompletion, ComparisonProductExecutor, ComparisonProductExecutorError,
    ComparisonProductRecipe, ComparisonSemanticSnapshot,
};
use crate::control_views::control_actions::ControlAction;
use crate::control_views::{AutomationView, MixerView};
use crate::daw_engine::DawEngineConfig;
use crate::daw_render::{PcmAsset, RenderCancellation};
use crate::decomposition::ComponentDecomposition;
use crate::explanation::RenderedExplanation;
use crate::explorer_model::{
    ExplorerInput, ExplorerMode, ExplorerModel, ExplorerNode, ExplorerNodeId, ExplorerSelection,
    ExplorerTarget, InspectorModel, InspectorReport,
};
use crate::export::{NoopExportObserver, RevisionPinnedAudio, WavExportRequest};
use crate::file_actions::ProjectFileActions;
use crate::hpss::{separate_harmonic_percussive, HpssResult, HpssSettings};
use crate::interpretation::{InterpretationCommand, InterpretationStore};
use crate::live_project::{LiveProject, LiveProjectSnapshot, SourceMaterialMetadata};
use crate::loom::{EventObservation, FitMetrics, SequenceSketch, TemplateBuildConfig};
use crate::media_resolver::{
    DecodedMaterial, MediaDecodeError, MediaDecoder, ProjectRateMaterial,
    RubatoSampleRateConverter, SymphoniaMediaDecoder,
};
use crate::ontology::{Producer, Provenance};
use crate::pane_audio::{
    workspace_audition_owner, AnalysisPaneBridge, PaneAudioKind, PaneAuditionContext,
    PaneSourcePin, PreviewController, SampleAuditionTicket, SamplePaneBridge,
};
use crate::pane_session_binding::{
    PaneSemanticSelection, PaneSessionBinding, PaneSessionDelivery, PaneSessionPayload,
    PaneSessionRegistration, PaneSessionTopics,
};
use crate::pattern_actions::{PatternEditorMode, PatternEditorTarget};
use crate::pattern_use_graph::PatternUseSnapshot;
use crate::product_input::{
    AccessibilitySnapshot, CloseChoice, CloseGuard, CloseGuardEffect, CloseGuardState,
    CloseRequestId, CloseScope, FocusTarget, ProductAction, ProductInputController, SemanticNode,
    SemanticRole,
};
use crate::project_audio_controller::{
    AuditionAlignment, ProjectAudioController, ProjectAudioControllerEffect,
    ProjectAudioControllerError, ProjectAudioPlanStamp, ProjectAudioRenderRecipe,
    ProjectTransportCommand, ProjectTransportFollowPolicy, ProjectTransportIntent,
};
use crate::project_controller::WorkbenchSampleIntent;
use crate::project_controller::{
    execute_arrangement_event, hydrate_pattern_editor, recommend_constructive,
    recommend_sample_result, ArrangementExecution, InstrumentRef, ObjectNavigator, ObjectRef,
    PadRef, PatternAuditionAdoption, PatternAuditionRequest, PatternAuditionSessionAdapter,
    PatternAuditionSessionInputs, PatternAuditionStartRequest, PatternWorkflowDispatchReceipt,
    PatternWorkflowRequest, RevealIntent, SampleActionOutcome, SelectionConsequence,
    WorkspaceReveal,
};
use crate::project_format::ProjectPackage;
use crate::project_repository::{JsonAirPayloadCodec, ProjectRepository};
use crate::project_selection::{
    ObjectSelection, ProjectSelection, SelectableId, SelectionProvenance, SelectionSource,
};
use crate::project_session::deprojection_workspace_bridge::LiveDeprojectionAnalysis;
use crate::project_session::reading_query::{
    ProjectQueryResolverInputs, ProjectReadingQuerySession,
};
use crate::project_session::{
    ProjectAudioStatus, ProjectDocumentLifecycle, ProjectEventFilter, ProjectEventSubscription,
    ProjectLifecycleError, ProjectPublication, ProjectReplacementDisposition, ProjectSession,
    ProjectSessionEvent, ProjectSessionId, RevealDisposition, RevealReceipt,
};
use crate::project_store::ProjectStore;
use crate::reading_query_view::{ReadingQueryView, ReadingQueryViewEffect, ReadingQueryViewInputs};
use crate::render_plan::{
    DeterminismGrade, ExactDigest, OutputTailPolicy, RenderFormat, RenderScope, RenderSpan,
    Tileability,
};
use crate::render_runtime::{AuditionMix, AuditionOwner, AuditionSubject};
use crate::reverse_surface::{
    ReverseSurfaceBody, ReverseSurfaceStore, SurfaceActionIntent, SurfaceAuditionIntent,
};
use crate::reverse_surface_view::{ReverseSurfaceViewEvent, ReverseSurfaceViewFactory};
use crate::rhythm::{
    analyze_mono as deproject_rhythm, AnalysisStatus as RhythmAnalysisStatus,
    RhythmConfig as RhythmDeprojectionConfig, RhythmDeprojection, SampleSpan, TempoRelation,
};
use crate::rhythm_explanation::ExplainBudget;
use crate::runtime_command_codec::DeterministicRuntimeCommandCodec;
use crate::sample_actions::{
    MakeBeatIntent, MakeBeatResultFocus, SampleAction, SampleActionError,
    SampleActionExecutionClass, SampleActionRequest, SampleActionResult, SampleAuditionIntent,
    SampleChopIntent, SampleDispatchReceipt, SampleFocusCallback, SampleKitDestination,
    SamplePublishedResult, SampleRequestId, SampleResultFocus, SampleSelection, SampleViewOutcome,
    SamplerTarget,
};
use crate::sample_kit::{KitId, PadId};
use crate::sample_material::SourceMaterialRef;
use crate::sampler_view::{SamplerBusOption, SamplerView, SamplerViewSource, SamplerViewState};
use crate::sequencer::PatternContent;
use crate::sequencer_view::{
    SequencerAuditionAvailability, SequencerEditor, SequencerEditorSource,
};
use crate::session::{Sample, SampleRange};
use crate::settings::SpectrumSettings;
use crate::spectral_tiles::{
    compute_spectral_tile, FrameRange as SpectralFrameRange, FrequencyRange, SourceStamp,
    SpectralCancellation, SpectralRecipe, SpectralTileKey, SpectralTilePlanner,
    SpectralTileRequest,
};
use crate::timeline::{
    FollowState as TimelineFollowState, LoopEditPolicy, LoopState as TimelineLoopState,
    PlaybackMode as TimelinePlaybackMode, TimelineControllerId, TimelineEffect,
    TimelineInteraction, TimelineInteractionEvent, TimelinePoint, TimelineRange, TimelineViewport,
    TransportEffect as TimelineTransportEffect,
};
use crate::transport_handoff_controller::{ProjectTransportHandoff, TransportEndpoint};
use crate::waveform_proxy::WaveformAssetKey;
use crate::workspace::accessibility::{WorkspaceSemanticAction, WorkspaceSemanticNodeId};
use crate::workspace::{BuiltinView, WorkspaceLayout, WorkspaceModel};
use crate::workspace_document::{
    AnalysisLensKind, BeatViewport as WorkspaceBeatViewport, EditorTarget as WorkspaceTarget,
    EditorViewState as WorkspaceViewState, FrameViewport as WorkspaceFrameViewport,
    LinkFacets as WorkspaceLinkFacets, LinkGroupId as WorkspaceLinkGroupId, NewWorkspaceView,
    PatternEditorMode as WorkspacePatternMode, ViewLinkMembership as WorkspaceLinkMembership,
    ViewLocation, WorkspaceDocument, WorkspaceItemKind as WorkspaceKind, WorkspaceViewDescriptor,
    WorkspaceViewId,
};
use crate::workspace_session_layout::{PaneBindingEffect, WorkspaceSessionLayout};
use crate::workspace_ui::{
    DynamicWorkspaceBootstrap, DynamicWorkspaceHooks, DynamicWorkspaceRoot,
    DynamicWorkspaceUiEvent, PaneRegistration, PaneRegistry,
};

static NEXT_VISUALIZER_AUDITION_OWNER: AtomicU64 = AtomicU64::new(1);
static NEXT_CONTEXTUAL_SAMPLE_REQUEST: AtomicU64 = AtomicU64::new(1);
static NEXT_QUERY_DOCUMENT: AtomicU64 = AtomicU64::new(1);

actions!(
    audec,
    [
        NewProject,
        OpenAudio,
        OpenProject,
        SaveProject,
        SaveProjectAs,
        OpenRecovery,
        ExportWav,
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
        OpenSampler,
        OpenReadingQuery,
        ViewZoomIn,
        ViewZoomOut,
        ViewPanLeft,
        ViewPanRight,
        ViewFit,
        ViewFollow,
        SetLoopFromSelection,
        ToggleLoop,
        NextWorkspacePane,
        PreviousWorkspacePane,
        CloseWorkspacePane,
        FloatOrDockWorkspacePane,
    ]
);

/// The native application menu. These actions are the same commands used by
/// the visible project bar and key bindings, so macOS never exposes a second
/// file-operation path with different behavior.
pub fn app_menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "audec".into(),
            items: vec![
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Quit audec", QuitAudec),
            ],
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("New Project", NewProject),
                MenuItem::separator(),
                MenuItem::action("Open Project…", OpenProject),
                MenuItem::action("Open Audio…", OpenAudio),
                MenuItem::separator(),
                MenuItem::action("Save Project", SaveProject),
                MenuItem::action("Save Project As…", SaveProjectAs),
                MenuItem::action("Open Recovery…", OpenRecovery),
                MenuItem::separator(),
                MenuItem::action("Export WAV…", ExportWav),
            ],
        },
        Menu {
            name: "Workspace".into(),
            items: vec![
                MenuItem::action("New Arrangement", OpenArrangementEditor),
                MenuItem::action("New Pattern Editor", OpenSequencerEditor),
                MenuItem::action("New Mixer", OpenMixer),
                MenuItem::action("New Automation Editor", OpenAutomation),
                MenuItem::action("New Media Pool", OpenAssets),
                MenuItem::action("New Sampler", OpenSampler),
                MenuItem::action("New Reading Query", OpenReadingQuery),
                MenuItem::separator(),
                MenuItem::action("Next Pane", NextWorkspacePane),
                MenuItem::action("Previous Pane", PreviousWorkspacePane),
                MenuItem::separator(),
                MenuItem::action("Float or Dock Active Pane", FloatOrDockWorkspacePane),
                MenuItem::action("Close Active Pane", CloseWorkspacePane),
            ],
        },
    ]
}

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

/// Keeps resolver identity checks on the native decode while retaining the
/// exact project-rate material produced from that same byte snapshot.
struct ProjectRateHydrationDecoder {
    decoder: SymphoniaMediaDecoder,
    converter: RubatoSampleRateConverter,
    project_sample_rate_hz: u32,
    material: Mutex<BTreeMap<ContentFingerprint, ProjectRateMaterial>>,
}

impl ProjectRateHydrationDecoder {
    fn new(project_sample_rate_hz: u32) -> Self {
        Self {
            decoder: SymphoniaMediaDecoder::default(),
            converter: RubatoSampleRateConverter::default(),
            project_sample_rate_hz,
            material: Mutex::new(BTreeMap::new()),
        }
    }
}

impl MediaDecoder for ProjectRateHydrationDecoder {
    fn decode(&self, path: &std::path::Path) -> Result<DecodedMaterial, MediaDecodeError> {
        let decoded = self.decoder.decode_provenanced(path)?;
        let project_rate = decoded
            .pcm_for_project_rate(self.project_sample_rate_hz, &self.converter)
            .map_err(|error| MediaDecodeError::InvalidOutput(error.to_string()))?;
        self.material
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(decoded.decoded.fingerprint, project_rate);
        Ok(decoded.decoded)
    }
}

fn within_interactive_sampling_limit(frames: u64, sample_rate: u32) -> bool {
    sample_rate > 0 && frames <= u64::from(sample_rate).saturating_mul(30)
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn sample_range_from_timeline(range: TimelineRange) -> SampleRange {
    SampleRange::new(
        Sample::new(range.start.get().min(i64::MAX as u64) as i64),
        Sample::new(range.end.get().min(i64::MAX as u64) as i64),
    )
}

fn timeline_playback_mode(mode: TransportMode) -> TimelinePlaybackMode {
    match mode {
        TransportMode::Stopped => TimelinePlaybackMode::Stopped,
        TransportMode::Paused => TimelinePlaybackMode::Paused,
        TransportMode::Playing => TimelinePlaybackMode::Playing,
        TransportMode::Ended => TimelinePlaybackMode::Ended,
    }
}

fn rhythm_artifact_descriptor(
    mono: &[f32],
    sample_rate: u32,
) -> Result<ArtifactDescriptor, String> {
    let extent = FrameSpan::new(
        0,
        i64::try_from(mono.len()).map_err(|_| "rhythm artifact is too long".to_owned())?,
    )
    .ok_or_else(|| "rhythm artifact extent is empty".to_owned())?;
    let mut pcm_bytes = Vec::with_capacity(mono.len().saturating_mul(4));
    for sample in mono {
        pcm_bytes.extend_from_slice(&sample.to_bits().to_le_bytes());
    }
    let source_digest = sha256_content(b"audec:decoded-mono:v1", &[&pcm_bytes]);
    let recipe_digest = sha256_content(
        b"audec:rhythm-deprojection-recipe:v1",
        &[
            env!("CARGO_PKG_VERSION").as_bytes(),
            format!("{:?}", RhythmDeprojectionConfig::default()).as_bytes(),
        ],
    );
    // The analyzer is deterministic for canonical mono PCM and its normalized
    // recipe, so those two strong identities are the portable output key.
    let output_digest = sha256_content(
        b"audec:rhythm-deprojection-output:v1",
        &[&source_digest.bytes, &recipe_digest.bytes],
    );
    Ok(ArtifactDescriptor {
        id: ArtifactId(output_digest),
        kind: ArtifactKind::ModelClaim,
        source_digest,
        recipe_digest,
        output_digest,
        extent,
        sample_rate,
        channels: 1,
        provenance: Provenance {
            producer: Producer::Analyzer {
                name: "audec rhythm deprojection".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                configuration_digest: Some(content_digest_hex(recipe_digest)),
            },
            // Analysis identity must not vary with wall-clock completion.
            created_unix_ms: None,
            source_revision: None,
            note: Some("live deterministic rhythm analysis".into()),
        },
    })
}

fn content_digest_hex(digest: ContentDigest) -> String {
    let mut value = String::with_capacity(64);
    for byte in digest.bytes {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    value
}

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
    cx.bind_keys([
        KeyBinding::new("cmd-q", QuitAudec, None),
        KeyBinding::new("cmd-n", NewProject, Some("Audec")),
        KeyBinding::new("cmd-o", OpenProject, Some("Audec")),
        KeyBinding::new("cmd-shift-o", OpenAudio, Some("Audec")),
        KeyBinding::new("cmd-s", SaveProject, Some("Audec")),
        KeyBinding::new("cmd-shift-s", SaveProjectAs, Some("Audec")),
        KeyBinding::new("cmd-alt-s", OpenRecovery, Some("Audec")),
        KeyBinding::new("cmd-e", ExportWav, Some("Audec")),
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
        KeyBinding::new("cmd-shift-b", OpenSampler, Some("Audec")),
        KeyBinding::new("cmd-shift-r", OpenReadingQuery, Some("Audec")),
        KeyBinding::new("=", ViewZoomIn, Some("Audec")),
        KeyBinding::new("-", ViewZoomOut, Some("Audec")),
        KeyBinding::new("shift-left", ViewPanLeft, Some("Audec")),
        KeyBinding::new("shift-right", ViewPanRight, Some("Audec")),
        KeyBinding::new("0", ViewFit, Some("Audec")),
        KeyBinding::new("f", ViewFollow, Some("Audec")),
        KeyBinding::new("cmd-l", SetLoopFromSelection, Some("Audec")),
        KeyBinding::new("l", ToggleLoop, Some("Audec")),
        KeyBinding::new("ctrl-tab", NextWorkspacePane, Some("Audec")),
        KeyBinding::new("ctrl-shift-tab", PreviousWorkspacePane, Some("Audec")),
        KeyBinding::new("cmd-shift-w", CloseWorkspacePane, Some("Audec")),
        KeyBinding::new("cmd-alt-w", FloatOrDockWorkspacePane, Some("Audec")),
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

#[derive(Clone, Debug)]
enum ProjectIoStatus {
    Idle,
    Opening(PathBuf),
    Saving(PathBuf),
    Saved(PathBuf),
    RecoveryAvailable { count: usize },
    Exporting(PathBuf),
    Exported(PathBuf),
    Failed(String),
}

impl ProjectIoStatus {
    fn label(&self) -> Option<String> {
        match self {
            Self::Idle => None,
            Self::Opening(path) => Some(format!("OPENING · {}", path.display())),
            Self::Saving(path) => Some(format!("SAVING · {}", path.display())),
            Self::Saved(path) => Some(format!("SAVED · {}", path.display())),
            Self::RecoveryAvailable { count } => Some(format!("RECOVERY AVAILABLE · {count}")),
            Self::Exporting(path) => Some(format!("EXPORTING · {}", path.display())),
            Self::Exported(path) => Some(format!("EXPORTED · {}", path.display())),
            Self::Failed(message) => Some(format!("FILE ERROR · {message}")),
        }
    }
}

const AUTOSAVE_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone)]
enum ProjectReplacementIntent {
    NewProject,
    ChooseAudio,
    ChooseProject,
    ChooseRecovery,
    OpenRecovery {
        package_root: PathBuf,
        checkpoint: crate::project_store::RecoveryCheckpoint,
    },
}

#[derive(Clone)]
enum PostSaveAction {
    Quit,
    Replace {
        intent: ProjectReplacementIntent,
        window: WindowHandle<DawWorkspace>,
    },
}

#[derive(Clone)]
enum SampleCompletionTarget {
    Browser(Entity<AssetBrowserView>),
    Sampler(Entity<SamplerView>),
}

struct PendingSampleRequest {
    request: SampleActionRequest,
    completion: Option<SampleCompletionTarget>,
    source: Option<WorkspaceViewId>,
}

struct PendingPatternWorkflow {
    request: PatternWorkflowRequest,
    completion: Entity<SequencerEditor>,
}

struct PendingPatternAudition {
    request: PatternAuditionRequest,
    owner: AuditionOwner,
}

struct PendingReadingQueryEffect {
    source: WorkspaceViewId,
    effect: ReadingQueryViewEffect,
}

struct PendingControlAction {
    editor_session: u64,
    action: ControlAction,
}

#[derive(Clone, Copy)]
struct PendingSampleFocus {
    source: Option<WorkspaceViewId>,
    focus: SampleResultFocus,
}

#[derive(Clone)]
struct PendingObjectReveal {
    receipt: RevealReceipt,
    diagnostics: Vec<crate::project_controller::RevealDiagnostic>,
    headline: String,
}

#[derive(Clone)]
enum WorkspacePaneContent {
    Overview(Entity<Workbench>),
    Arrangement(Entity<ArrangementView>),
    Browser(Entity<AssetBrowserView>),
    Pattern(Entity<SequencerEditor>),
    Mixer(Entity<MixerView>),
    Automation(Entity<AutomationView>),
    Analysis(Entity<Visualizer>),
    Sampler(Entity<SamplerView>),
    ReadingQuery(Entity<ReadingQueryView>),
    Notice(Entity<WorkspaceNotice>),
}

struct WorkspacePaneHost {
    descriptor: WorkspaceViewDescriptor,
    content: WorkspacePaneContent,
    workbench: WeakEntity<Workbench>,
    project_generation: Option<u64>,
    project_revisions: Option<crate::daw_project::ProjectRevisions>,
    audio: ProjectAudioStatus,
    semantic_selection: Option<PaneSemanticSelection>,
    completion: Option<RevealCompletion>,
    focus_handle: FocusHandle,
}

impl WorkspacePaneHost {
    fn new(
        descriptor: WorkspaceViewDescriptor,
        content: WorkspacePaneContent,
        workbench: WeakEntity<Workbench>,
        focus_handle: FocusHandle,
    ) -> Self {
        Self {
            descriptor,
            content,
            workbench,
            project_generation: None,
            project_revisions: None,
            audio: ProjectAudioStatus::default(),
            semantic_selection: None,
            completion: None,
            focus_handle,
        }
    }

    fn set_audio(&mut self, audio: ProjectAudioStatus, cx: &mut Context<Self>) {
        self.audio = audio.clone();
        if let WorkspacePaneContent::Arrangement(view) = &self.content {
            let playhead =
                ArrangementFrame::new(i64::try_from(audio.transport.frame.0).unwrap_or(i64::MAX));
            let playing = audio.transport.mode == TransportMode::Playing;
            view.update(cx, |view, cx| view.set_playhead(playhead, playing, cx));
        }
        cx.notify();
    }

    fn set_semantic_selection(&mut self, selection: PaneSemanticSelection, cx: &mut Context<Self>) {
        if let WorkspacePaneContent::Arrangement(view) = &self.content {
            let arrangement = arrangement_selection_from_project(&selection.selection);
            view.update(cx, |view, cx| view.set_selection(arrangement, cx));
        }
        self.semantic_selection = Some(selection);
        cx.notify();
    }

    fn set_completion(&mut self, completion: RevealCompletion, cx: &mut Context<Self>) {
        self.completion = Some(completion);
        cx.notify();
    }
}

impl Render for WorkspacePaneHost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.content {
            WorkspacePaneContent::Overview(view) => view.clone().into_any_element(),
            WorkspacePaneContent::Arrangement(view) => view.clone().into_any_element(),
            WorkspacePaneContent::Browser(view) => view.clone().into_any_element(),
            WorkspacePaneContent::Pattern(view) => view.clone().into_any_element(),
            WorkspacePaneContent::Mixer(view) => view.clone().into_any_element(),
            WorkspacePaneContent::Automation(view) => view.clone().into_any_element(),
            WorkspacePaneContent::Analysis(view) => view.clone().into_any_element(),
            WorkspacePaneContent::Sampler(view) => view.clone().into_any_element(),
            WorkspacePaneContent::ReadingQuery(view) => view.clone().into_any_element(),
            WorkspacePaneContent::Notice(view) => view.clone().into_any_element(),
        };
        div()
            .track_focus(&self.focus_handle)
            .tab_group()
            .size_full()
            .flex()
            .flex_col()
            .when_some(self.completion.clone(), |pane, completion| {
                pane.child(
                    div()
                        .id("contextual-object-completion")
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .px_3()
                        .py_2()
                        .border_b_1()
                        .border_color(rgb(BORDER))
                        .bg(rgb(PANEL_ALT))
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(rgb(TEXT))
                                        .child(completion.headline),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(CYAN))
                                        .child(completion.breadcrumb),
                                )
                                .when_some(completion.diagnostic, |column, diagnostic| {
                                    column.child(
                                        div().text_xs().text_color(rgb(AMBER)).child(diagnostic),
                                    )
                                }),
                        )
                        .child(
                            viz_control("dismiss-contextual-completion", "Dismiss").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.completion = None;
                                    cx.notify();
                                }),
                            ),
                        ),
                )
            })
            .when(
                matches!(&self.content, WorkspacePaneContent::Sampler(_)),
                |pane| {
                    pane.child(
                        div()
                            .id("sampler-forward-action")
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .px_3()
                            .py_2()
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .bg(rgb(PANEL))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(TEXT))
                                            .child("Continue with this instrument"),
                                    )
                                    .child(
                                        div().text_xs().text_color(rgb(DIM)).child(
                                            "Selected zone → playable beat → pattern editor",
                                        ),
                                    ),
                            )
                            .child(viz_control("sampler-make-beat", "Make beat").on_click(
                                cx.listener(|this, _, _, cx| {
                                    let Some(workbench) = this.workbench.upgrade() else {
                                        return;
                                    };
                                    let view = this.descriptor.id;
                                    workbench.update(cx, |workbench, cx| {
                                        workbench.make_beat_from_sampler(view, cx)
                                    });
                                }),
                            )),
                    )
                },
            )
            .child(div().flex_1().min_h_0().child(content))
    }
}

impl Focusable for WorkspacePaneHost {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[derive(Clone)]
enum WorkspacePaneRuntime {
    Overview,
    Analysis(WeakEntity<Visualizer>),
    Reverse,
    Hosted(WeakEntity<WorkspacePaneHost>),
}

struct PendingArrangementEvent {
    source: Option<WorkspaceViewId>,
    event: ArrangementViewEvent,
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
    arrangement_events: Arc<Mutex<Vec<PendingArrangementEvent>>>,
    sample_actions: Arc<Mutex<Vec<PendingSampleRequest>>>,
    sample_focuses: Arc<Mutex<Vec<PendingSampleFocus>>>,
    object_reveals: Arc<Mutex<Vec<PendingObjectReveal>>>,
    reverse_surface_events: Arc<Mutex<Vec<ReverseSurfaceViewEvent>>>,
    reverse_surface_store: Arc<Mutex<ReverseSurfaceStore>>,
    reverse_surface_factory: ReverseSurfaceViewFactory,
    comparison_executor: ComparisonProductExecutor,
    control_actions: Arc<Mutex<Vec<PendingControlAction>>>,
    pattern_workflows: Arc<Mutex<Vec<PendingPatternWorkflow>>>,
    pattern_auditions: Arc<Mutex<Vec<PendingPatternAudition>>>,
    pattern_audition: PatternAuditionSessionAdapter,
    pattern_audition_owner: Option<AuditionOwner>,
    reading_query_effects: Rc<RefCell<Vec<PendingReadingQueryEffect>>>,
    reading_query_documents: BTreeMap<WorkspaceViewId, QueryDocument>,
    sequencer_view: Option<Entity<SequencerEditor>>,
    mixer_view: Option<Entity<MixerView>>,
    automation_view: Option<Entity<AutomationView>>,
    asset_registry: Arc<Mutex<AssetRegistry>>,
    asset_view: Option<Entity<AssetBrowserView>>,
    asset_events: Arc<Mutex<Vec<AssetBrowserEvent>>>,
    session: Entity<ProjectSession>,
    session_events: ProjectEventSubscription,
    pane_session_binding: PaneSessionBinding,
    workspace_panes: BTreeMap<WorkspaceViewId, WorkspacePaneRuntime>,
    active_workspace_view: Option<WorkspaceViewId>,
    sampler_selection_cache: BTreeMap<WorkspaceViewId, SamplerViewState>,
    project_lifecycle: ProjectDocumentLifecycle<JsonAirPayloadCodec>,
    project_io_status: ProjectIoStatus,
    open_generation: u64,
    save_generation: u64,
    autosave_last_attempt: Instant,
    autosave_in_flight: bool,
    pending_export_destination: Option<PathBuf>,
    pending_workspace_import: Option<WorkspaceDocument>,
    audition_audio: Option<ProjectAudio>,
    audio: Option<AudioHost>,
    audio_controller: ProjectAudioController,
    preview_controller: PreviewController,
    pad_preview_tickets: BTreeMap<(WorkspaceViewId, KitId, PadId), SampleAuditionTicket>,
    audio_render_cancellation: Option<RenderCancellation>,
    audio_snapshot_digest: Option<ExactDigest>,
    audio_rendering: bool,
    audio_error: Option<String>,
    constructive_status: Option<String>,
    primary_source_timeline_aligned: bool,
    playhead_seconds: f64,
    timeline_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    timeline_interaction: TimelineInteraction,
    timeline_viewport: TimelineViewport,
    timeline_follow: bool,
    timeline_selection: Option<SampleRange>,
    timeline_signal: SignalLayer,
    loop_range: Option<SampleRange>,
    loop_enabled: bool,
    material_rail_scroll: ScrollHandle,
    inspector_rail_scroll: ScrollHandle,
    product_shell_hosted: bool,
    focus_handle: FocusHandle,
    _ticker: Task<()>,
}

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
        let ticker = cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(33))
                .await;
            if this
                .update(cx, |this, cx| {
                    this.handle_asset_events(cx);
                    this.handle_arrangement_events(cx);
                    this.handle_sample_actions(cx);
                    this.handle_control_actions(cx);
                    this.handle_pattern_auditions(cx);
                    this.handle_reading_query_effects(cx);
                    this.handle_reverse_surface_events(cx);
                    this.handle_session_events(cx);
                    this.sync_active_sampler_selection(cx);
                    this.tick_project_audio(cx);
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
            sample_actions: Arc::new(Mutex::new(Vec::new())),
            sample_focuses: Arc::new(Mutex::new(Vec::new())),
            object_reveals: Arc::new(Mutex::new(Vec::new())),
            reverse_surface_events,
            reverse_surface_store,
            reverse_surface_factory,
            comparison_executor: ComparisonProductExecutor::new(),
            control_actions: Arc::new(Mutex::new(Vec::new())),
            pattern_workflows: Arc::new(Mutex::new(Vec::new())),
            pattern_auditions: Arc::new(Mutex::new(Vec::new())),
            pattern_audition: PatternAuditionSessionAdapter::default(),
            pattern_audition_owner: None,
            reading_query_effects: Rc::new(RefCell::new(Vec::new())),
            reading_query_documents: BTreeMap::new(),
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
            save_generation: 0,
            autosave_last_attempt: Instant::now(),
            autosave_in_flight: false,
            pending_export_destination: None,
            pending_workspace_import: None,
            audition_audio: None,
            audio: None,
            audio_controller: ProjectAudioController::new(),
            preview_controller: PreviewController::default(),
            pad_preview_tickets: BTreeMap::new(),
            audio_render_cancellation: None,
            audio_snapshot_digest: None,
            audio_rendering: false,
            audio_error: None,
            constructive_status: None,
            primary_source_timeline_aligned: false,
            playhead_seconds: 0.0,
            timeline_bounds: Arc::new(Mutex::new(None)),
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

    fn load_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
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
            (analyze_file(&analysis_path), fingerprint)
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

    fn prepare_for_document_install(&mut self, cx: &mut Context<Self>) {
        self.reset_project_runtime_bridges(cx);
        if let Some(audio) = self.audio.as_ref() {
            self.preview_controller.cancel_all(audio);
        }
        self.pad_preview_tickets.clear();
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
        self.audio_controller = ProjectAudioController::new();
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

    fn reset_project_runtime_bridges(&mut self, cx: &mut Context<Self>) {
        match self.reverse_surface_events.lock() {
            Ok(mut events) => events.clear(),
            Err(poisoned) => poisoned.into_inner().clear(),
        }
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
                            LiveProject::from_source_material(metadata, registry, asset, pcm)
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
                AssetBrowserEvent::Activate(asset)
                    if self
                        .asset_registry
                        .lock()
                        .is_ok_and(|registry| registry.get(asset).is_some()) =>
                {
                    self.open_arrangement_editor(cx);
                }
                // Connected Browser panes send exact material/range audition
                // through SamplePaneBridge. Ignore the legacy event rather
                // than silently playing the unrelated primary source.
                AssetBrowserEvent::Audition(_) => {}
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
        for pending in events {
            let selection_intent = match &pending.event {
                ArrangementViewEvent::Commit(commit) => commit.selection.clone(),
                _ => None,
            };
            let execution = self.session.update(cx, |session, _| {
                execute_arrangement_event(session, pending.event)
            });
            match execution {
                Ok(ArrangementExecution::Seek(frame)) => {
                    self.seek_to_sample(u64::try_from(frame.get()).unwrap_or(0), cx);
                }
                Ok(
                    ArrangementExecution::ProjectChanged { .. }
                    | ArrangementExecution::SelectionOnly
                    | ArrangementExecution::HistoryUnchanged(_),
                ) => {}
                Err(error) => {
                    self.constructive_status = Some(format!("Arrangement edit failed · {error}"));
                }
            }
            if let (Some(source), Some(intent)) = (pending.source, selection_intent) {
                self.publish_arrangement_selection(source, intent, cx);
            }
        }
        self.handle_session_events(cx);
    }

    fn handle_sample_actions(&mut self, cx: &mut Context<Self>) {
        let actions = self
            .sample_actions
            .lock()
            .map(|mut actions| std::mem::take(&mut *actions))
            .unwrap_or_default();
        for pending in actions {
            match pending.request.action.execution_class() {
                SampleActionExecutionClass::Immediate => {
                    let request_id = pending.request.id;
                    let action = pending.request.action.clone();
                    let bridge = match self.begin_sample_audition(pending.source, &action) {
                        Ok(bridge) => bridge,
                        Err(error) => {
                            self.complete_sample_request(
                                request_id,
                                Err(error),
                                pending.completion,
                                pending.source,
                                cx,
                            );
                            continue;
                        }
                    };
                    let result = match self.session.update(cx, |session, _| {
                        session.execute_sample_action(action.clone())
                    }) {
                        Ok(outcome) => self
                            .resolve_sample_pane_outcome(bridge.0, &action, outcome, bridge.1, cx),
                        Err(error) => {
                            self.cancel_sample_pane(bridge.0);
                            Err(SampleActionError::new("session", error.to_string())
                                .retryable(true))
                        }
                    };
                    self.complete_sample_request(
                        request_id,
                        result,
                        pending.completion,
                        pending.source,
                        cx,
                    );
                }
                SampleActionExecutionClass::BackgroundPlanning => {
                    self.dispatch_background_sample_request(pending, cx);
                }
            }
        }
        self.handle_session_events(cx);
    }

    fn dispatch_background_sample_request(
        &mut self,
        pending: PendingSampleRequest,
        cx: &mut Context<Self>,
    ) {
        let request_id = pending.request.id;
        let action = pending.request.action.clone();
        let work = self
            .session
            .read(cx)
            .capture_sample_action_work(pending.request);
        let work = match work {
            Ok(work) => work,
            Err(error) => {
                self.complete_sample_request(
                    request_id,
                    Err(SampleActionError::new("session", error.to_string()).retryable(true)),
                    pending.completion,
                    pending.source,
                    cx,
                );
                return;
            }
        };
        let target = pending.completion;
        let source = pending.source;
        let bridge = SamplePaneBridge::new(source.unwrap_or(WorkspaceViewId::TRACK_OVERVIEW));
        let preparation = cx.background_spawn(async move { work.prepare() });
        cx.spawn(async move |this, cx| {
            let prepared = preparation.await;
            let _ = this.update(cx, |this, cx| {
                let result = match prepared {
                    Ok(prepared) => match this.session.update(cx, |session, _| {
                        session.commit_prepared_sample_action(prepared)
                    }) {
                        Ok(outcome) => match bridge {
                            Ok(bridge) => {
                                this.resolve_sample_pane_outcome(bridge, &action, outcome, None, cx)
                            }
                            Err(error) => Err(SampleActionError::new("preview", error.to_string())),
                        },
                        Err(error) => {
                            Err(SampleActionError::new("commit", error.to_string()).retryable(true))
                        }
                    },
                    Err(error) => {
                        Err(SampleActionError::new("planning", error.to_string()).retryable(true))
                    }
                };
                this.complete_sample_request(request_id, result, target, source, cx);
                this.handle_session_events(cx);
            });
        })
        .detach();
    }

    fn begin_sample_audition(
        &mut self,
        source: Option<WorkspaceViewId>,
        action: &SampleAction,
    ) -> Result<(SamplePaneBridge, Option<SampleAuditionTicket>), SampleActionError> {
        let view = source.unwrap_or(WorkspaceViewId::TRACK_OVERVIEW);
        let bridge = SamplePaneBridge::new(view)
            .map_err(|error| SampleActionError::new("preview", error.to_string()))?;
        let SampleAction::Audition(intent) = action else {
            return Ok((bridge, None));
        };
        let ticket = match *intent {
            SampleAuditionIntent::MaterialOneShot { .. } => bridge
                .begin_audition(&mut self.preview_controller, *intent)
                .map_err(|error| SampleActionError::new("preview", error.to_string()))?,
            SampleAuditionIntent::PadGate {
                kit,
                pad,
                pressed: true,
                ..
            } => {
                let ticket = bridge
                    .begin_audition(&mut self.preview_controller, *intent)
                    .map_err(|error| SampleActionError::new("preview", error.to_string()))?;
                self.pad_preview_tickets.insert((view, kit, pad), ticket);
                ticket
            }
            SampleAuditionIntent::PadGate {
                kit,
                pad,
                pressed: false,
                ..
            } => self
                .pad_preview_tickets
                .remove(&(view, kit, pad))
                .ok_or_else(|| {
                    SampleActionError::new(
                        "preview.stale-release",
                        "This pad release no longer matches an active press",
                    )
                })?,
        };
        Ok((bridge, Some(ticket)))
    }

    fn resolve_sample_pane_outcome(
        &mut self,
        bridge: SamplePaneBridge,
        action: &SampleAction,
        outcome: SampleActionOutcome,
        ticket: Option<SampleAuditionTicket>,
        cx: &mut Context<Self>,
    ) -> SampleActionResult {
        let snapshot = self
            .session
            .read(cx)
            .project_snapshot()
            .cloned()
            .map_err(|error| SampleActionError::new("preview.snapshot", error.to_string()))?;
        let outcome = bridge
            .resolve_outcome(&snapshot, action, outcome, ticket)
            .map_err(|error| SampleActionError::new("preview.resolve", error.to_string()))?;
        if let Some(effect) = outcome.preview {
            let Some(audio) = self.audio.as_ref() else {
                return Err(SampleActionError::new(
                    "preview.host",
                    "The project preview bus is not ready",
                ));
            };
            effect.apply(&mut self.preview_controller, audio);
        }
        outcome.result
    }

    fn cancel_sample_pane(&mut self, bridge: SamplePaneBridge) {
        if let Some(audio) = self.audio.as_ref() {
            bridge
                .dispose_effect()
                .apply(&mut self.preview_controller, audio);
        }
        let view = WorkspaceViewId(bridge.owner().local);
        self.pad_preview_tickets
            .retain(|(owner, _, _), _| *owner != view);
    }

    fn complete_sample_request(
        &mut self,
        request_id: crate::sample_actions::SampleRequestId,
        result: SampleActionResult,
        target: Option<SampleCompletionTarget>,
        source: Option<WorkspaceViewId>,
        cx: &mut Context<Self>,
    ) {
        let publication = result.as_ref().ok().and_then(|outcome| match outcome {
            SampleViewOutcome::Published(publication) => Some(publication.clone()),
            _ => None,
        });
        match target {
            Some(SampleCompletionTarget::Browser(browser)) => {
                browser.update(cx, |browser, cx| {
                    browser.complete_request(request_id, result, cx);
                });
            }
            Some(SampleCompletionTarget::Sampler(sampler)) => {
                sampler.update(cx, |sampler, cx| {
                    sampler.complete_request(request_id, result, cx);
                });
            }
            None => {
                if let Err(error) = result {
                    self.audio_error = Some(error.message);
                }
            }
        }
        if let Some(publication) = publication {
            self.enqueue_sample_reveal(publication, source, cx);
        }
    }

    fn enqueue_sample_reveal(
        &mut self,
        publication: SamplePublishedResult,
        source: Option<WorkspaceViewId>,
        cx: &mut Context<Self>,
    ) {
        // SampleFocusCallback is the view-owned signal that this correlated
        // result wants navigation. Pair it here with the full publication so
        // kit/pad/pattern identities and provenance are not lost.
        if publication.focus != SampleResultFocus::Stay {
            match self.sample_focuses.lock() {
                Ok(mut focuses) => {
                    if let Some(index) = focuses.iter().position(|pending| {
                        pending.source == source && pending.focus == publication.focus
                    }) {
                        focuses.remove(index);
                    }
                }
                Err(poisoned) => {
                    let mut focuses = poisoned.into_inner();
                    if let Some(index) = focuses.iter().position(|pending| {
                        pending.source == source && pending.focus == publication.focus
                    }) {
                        focuses.remove(index);
                    }
                }
            }
        }
        let mut recommendation = recommend_sample_result(&publication);
        recommendation.request.current_view = source;
        let headline = match &recommendation.request.object {
            ObjectRef::Pattern(_) | ObjectRef::PatternOccurrence(_) => "Beat created",
            ObjectRef::AutomationOccurrence(_) => "Automation edit created",
            ObjectRef::Instrument(_) | ObjectRef::Pad(_) => "Instrument created",
            _ => "Sample action completed",
        };
        let receipt = match self.session.read(cx).issue_reveal(recommendation.request) {
            Ok(receipt) => receipt,
            Err(error) => {
                self.constructive_status = Some(format!("Reveal unavailable · {error}"));
                cx.notify();
                return;
            }
        };
        if let Ok(mut reveals) = self.object_reveals.lock() {
            reveals.push(PendingObjectReveal {
                receipt,
                diagnostics: recommendation.diagnostics,
                headline: headline.into(),
            });
        }
    }

    fn apply_object_reveal_selection(
        &mut self,
        view: Option<WorkspaceViewId>,
        consequence: &SelectionConsequence,
        cx: &mut Context<Self>,
    ) {
        let project = self
            .session
            .read(cx)
            .project_snapshot()
            .ok()
            .map(|snapshot| snapshot.project.clone());
        if let Some(project) = project.as_ref() {
            let guard = self.session.read(cx).current_selection_guard();
            match guard {
                Ok(guard) => {
                    let mut selection = ProjectSelection::from_reveal(
                        consequence.primary.clone(),
                        consequence.related.iter().cloned(),
                        guard,
                        view,
                    );
                    for object in std::iter::once(&consequence.primary).chain(&consequence.related)
                    {
                        add_product_object_to_selection(&mut selection, object, project);
                    }
                    if let Err(error) = self.session.update(cx, |session, _| {
                        session.replace_guarded_selection(selection)
                    }) {
                        self.constructive_status =
                            Some(format!("Created object selection was stale · {error}"));
                    }
                }
                Err(error) => {
                    self.constructive_status =
                        Some(format!("Created object selection unavailable · {error}"));
                }
            }
        }

        let Some(view) = view else {
            self.handle_session_events(cx);
            return;
        };
        let Some(WorkspacePaneRuntime::Hosted(host)) = self.workspace_panes.get(&view).cloned()
        else {
            self.handle_session_events(cx);
            return;
        };
        let Some(host) = host.upgrade() else {
            self.handle_session_events(cx);
            return;
        };
        let primary = consequence.primary.clone();
        let project = project.clone();
        host.update(cx, |host, cx| match &host.content {
            WorkspacePaneContent::Browser(browser) => {
                if let Some(asset) = object_asset(&primary) {
                    browser.update(cx, |browser, cx| {
                        let mut state = browser.state().clone();
                        state.selected = Some(asset);
                        browser.set_state(state, cx);
                    });
                }
            }
            WorkspacePaneContent::Sampler(sampler) => match primary {
                ObjectRef::Instrument(InstrumentRef::SampleKit(kit)) => {
                    sampler.update(cx, |sampler, cx| {
                        sampler.retarget(SamplerTarget::Kit(kit), cx)
                    });
                }
                ObjectRef::Pad(pad) => {
                    sampler.update(cx, |sampler, cx| {
                        sampler.retarget(
                            SamplerTarget::Pad {
                                kit: pad.kit,
                                pad: pad.pad,
                            },
                            cx,
                        )
                    });
                }
                _ => {}
            },
            WorkspacePaneContent::Arrangement(arrangement) => {
                if let Some(project) = project.as_ref() {
                    let state = &project.state().domains.arrangement;
                    let mut selected = ArrangementSelection::default();
                    match primary {
                        ObjectRef::PatternOccurrence(occurrence) => {
                            selected.clips.insert(occurrence.arrangement_clip);
                        }
                        ObjectRef::AudioClip(clip) => {
                            selected.clips.insert(clip);
                        }
                        ObjectRef::AutomationOccurrence(occurrence) => {
                            selected.clips.insert(occurrence.arrangement_clip);
                        }
                        ObjectRef::Track(track) => {
                            selected.tracks.insert(track);
                        }
                        _ => {}
                    }
                    for clip in &selected.clips {
                        if let Some(clip) = state.clip(*clip) {
                            selected.tracks.insert(clip.track_id);
                            selected.time = Some(clip.placement);
                        }
                    }
                    arrangement.update(cx, |arrangement, cx| {
                        arrangement.set_selection(selected.clone(), cx);
                        if let Some(range) = selected.time {
                            let mut viewport = arrangement.viewport();
                            if viewport.ensure_visible(range.start, 0.18) {
                                arrangement.set_viewport(viewport, cx);
                            }
                        }
                    });
                }
            }
            _ => {}
        });
        self.handle_session_events(cx);
    }

    fn sample_focus_callback(&self, source: Option<WorkspaceViewId>) -> SampleFocusCallback {
        let focuses = Arc::clone(&self.sample_focuses);
        Arc::new(move |focus| {
            if let Ok(mut focuses) = focuses.lock() {
                focuses.push(PendingSampleFocus { source, focus });
            }
        })
    }

    fn install_browser_sample_callbacks(
        &self,
        browser: &Entity<AssetBrowserView>,
        source: Option<WorkspaceViewId>,
        cx: &mut Context<Self>,
    ) {
        let actions = Arc::clone(&self.sample_actions);
        let completion = browser.clone();
        let callback = Arc::new(move |request: SampleActionRequest| {
            let receipt = SampleDispatchReceipt::accepted(&request);
            if let Ok(mut actions) = actions.lock() {
                actions.push(PendingSampleRequest {
                    request,
                    completion: Some(SampleCompletionTarget::Browser(completion.clone())),
                    source,
                });
            }
            receipt
        });
        let focus = self.sample_focus_callback(source);
        browser.update(cx, |browser, _| {
            browser.set_sample_callback(Some(callback));
            browser.set_sample_focus_callback(Some(focus));
        });
    }

    fn install_sampler_sample_callbacks(
        &self,
        sampler: &Entity<SamplerView>,
        source: Option<WorkspaceViewId>,
        cx: &mut Context<Self>,
    ) {
        let actions = Arc::clone(&self.sample_actions);
        let completion = sampler.clone();
        let callback = Arc::new(move |request: SampleActionRequest| {
            let receipt = SampleDispatchReceipt::accepted(&request);
            if let Ok(mut actions) = actions.lock() {
                actions.push(PendingSampleRequest {
                    request,
                    completion: Some(SampleCompletionTarget::Sampler(completion.clone())),
                    source,
                });
            }
            receipt
        });
        let focus = self.sample_focus_callback(source);
        sampler.update(cx, |sampler, _| {
            sampler.set_callback(Some(callback));
            sampler.set_focus_callback(Some(focus));
        });
    }

    fn install_pattern_workflow_callback(
        &self,
        editor: &Entity<SequencerEditor>,
        revision: u64,
        source: Option<WorkspaceViewId>,
        cx: &mut Context<Self>,
    ) {
        let workflows = Arc::clone(&self.pattern_workflows);
        let completion = editor.clone();
        let callback = Arc::new(move |request: PatternWorkflowRequest| {
            let id = request.id;
            workflows
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(PendingPatternWorkflow {
                    request,
                    completion: completion.clone(),
                });
            PatternWorkflowDispatchReceipt::accepted(id)
        });
        let shared_audition = source.and_then(|view| {
            let owner = workspace_audition_owner(view).ok()?;
            let auditions = Arc::clone(&self.pattern_auditions);
            Some(Arc::new(move |request: PatternAuditionRequest| {
                auditions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(PendingPatternAudition { request, owner });
            })
                as crate::project_controller::SharedPatternAuditionCallback)
        });
        editor.update(cx, |editor, cx| {
            editor.set_project_revision(revision, cx);
            editor.set_workflow_callback(Some(callback));
            editor.set_shared_pattern_audition_callback(shared_audition.clone());
            editor.set_audition_availability(
                if shared_audition.is_some() {
                    SequencerAuditionAvailability::Available
                } else {
                    SequencerAuditionAvailability::unavailable(
                        "Pattern audition requires a project workspace pane",
                    )
                },
                cx,
            );
        });
    }

    fn handle_pattern_auditions(&mut self, cx: &mut Context<Self>) {
        let requests = std::mem::take(
            &mut *self
                .pattern_auditions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for pending in requests {
            if self.audio.is_none() {
                self.constructive_status =
                    Some("Pattern audition unavailable · project audio is not ready".into());
                continue;
            }
            if let Some(previous) = self.pattern_audition_owner.take() {
                let session = self.session.clone();
                let _ = session.update(cx, |session, _| {
                    self.pattern_audition
                        .stop(session, &mut self.audio_controller, previous)
                });
            }
            let start = PatternAuditionStartRequest {
                audition: pending.request,
                adoption: PatternAuditionAdoption {
                    owner: pending.owner,
                    subject: AuditionSubject::Construction,
                    mix: AuditionMix::Replace,
                    alignment: AuditionAlignment::LoopSpan { play: true },
                },
            };
            let session = self.session.clone();
            let prepared = session.update(cx, |session, _| {
                self.pattern_audition.prepare(
                    session,
                    start,
                    PatternAuditionSessionInputs::new(Arc::new(DawEngineConfig::default())),
                )
            });
            match prepared {
                Ok(job) => {
                    self.pattern_audition_owner = Some(pending.owner);
                    self.constructive_status = Some("Rendering exact pattern audition".into());
                    let execution = cx.background_spawn(async move { job.execute() });
                    cx.spawn(async move |this, cx| {
                        let work = execution.await;
                        let _ = this.update(cx, |this, cx| {
                            let session = this.session.clone();
                            let Some(host) = this.audio.as_ref() else {
                                return;
                            };
                            match session.update(cx, |session, _| {
                                this.pattern_audition.complete(
                                    session,
                                    &mut this.audio_controller,
                                    host,
                                    work,
                                )
                            }) {
                                Ok(_) => {
                                    this.constructive_status =
                                        Some("Playing exact pattern audition".into());
                                    this.publish_audio_status(cx);
                                }
                                Err(error) => {
                                    this.constructive_status =
                                        Some(format!("Pattern audition refused · {error}"));
                                }
                            }
                            cx.notify();
                        });
                    })
                    .detach();
                }
                Err(error) => {
                    self.constructive_status = Some(format!("Pattern audition refused · {error}"));
                }
            }
        }
    }

    fn handle_reading_query_effects(&mut self, cx: &mut Context<Self>) {
        let effects = std::mem::take(&mut *self.reading_query_effects.borrow_mut());
        if effects.is_empty() {
            return;
        }
        for pending in effects {
            let source = pending.source;
            match pending.effect {
                ReadingQueryViewEffect::Command(envelope) => {
                    let bridge = self.capture_reading_query_session(cx);
                    let result = bridge.and_then(|bridge| {
                        let session = self.session.clone();
                        session
                            .update(cx, |session, _| bridge.apply_command(session, envelope))
                            .map_err(|error| error.to_string())
                    });
                    let committed = result.is_ok();
                    self.constructive_status = Some(match &result {
                        Ok(receipt) => format!(
                            "Reading import committed · project revision {}",
                            receipt.publication.revisions.aggregate
                        ),
                        Err(error) => format!("Reading import refused · {error}"),
                    });
                    if committed {
                        if let Some(view) = self.reading_query_view(source, cx) {
                            self.refresh_reading_query_inputs(&view, cx);
                        }
                    }
                }
                ReadingQueryViewEffect::Observation {
                    request,
                    cancellation,
                } => {
                    let request_id = request.request_id.clone();
                    match self.capture_reading_query_session(cx) {
                        Ok(bridge) => {
                            let execution = cx.background_spawn(async move {
                                bridge.dispatch(request, &cancellation)
                            });
                            cx.spawn(async move |this, cx| {
                                let result = execution.await;
                                let _ = this.update(cx, |this, cx| {
                                    let Some(view) = this.reading_query_view(source, cx) else {
                                        return;
                                    };
                                    match result {
                                        Ok(dispatch) => view.update(cx, |view, cx| {
                                            view.accept_dispatch(dispatch, cx)
                                        }),
                                        Err(error) => {
                                            this.constructive_status =
                                                Some(format!("Reading query refused · {error}"));
                                            view.update(cx, |view, cx| {
                                                view.complete_external_failure(
                                                    &request_id,
                                                    error.to_string(),
                                                    cx,
                                                );
                                            });
                                        }
                                    }
                                });
                            })
                            .detach();
                        }
                        Err(error) => {
                            cancellation.cancel();
                            self.constructive_status =
                                Some(format!("Reading query unavailable · {error}"));
                            if let Some(view) = self.reading_query_view(source, cx) {
                                view.update(cx, |view, cx| {
                                    view.complete_external_failure(&request_id, error, cx);
                                });
                            }
                        }
                    }
                }
                ReadingQueryViewEffect::DocumentChanged(changed) => {
                    self.reading_query_documents
                        .insert(source, changed.document);
                    if let Some(view) = self.reading_query_view(source, cx) {
                        self.refresh_reading_query_inputs(&view, cx);
                    }
                    self.constructive_status = Some(match changed.reason {
                        crate::reading_query_view::QueryDocumentChangeReason::ResidualGuideInstalled => {
                            "Reading document updated · residual guide retained in workspace".into()
                        }
                        crate::reading_query_view::QueryDocumentChangeReason::QueryPageObserved => {
                            "Reading document updated · query result and provenance retained in workspace".into()
                        }
                    });
                }
                ReadingQueryViewEffect::Render(_) => {
                    self.constructive_status = Some(
                        "Reading audition unavailable · no shared reading render adapter is attached"
                            .into(),
                    );
                }
                ReadingQueryViewEffect::Reveal(_) => {
                    self.constructive_status = Some(format!(
                        "Reading result reveal from pane {} awaits the typed entity bridge",
                        source.0
                    ));
                }
            }
        }
        cx.notify();
    }

    fn take_reading_query_documents(&mut self) -> BTreeMap<WorkspaceViewId, QueryDocument> {
        std::mem::take(&mut self.reading_query_documents)
    }

    fn restore_reading_query_documents(
        &mut self,
        documents: BTreeMap<WorkspaceViewId, QueryDocument>,
    ) {
        // A newer pane publication wins if one arrived while persistence was
        // attempted. Otherwise retain the failed update for the next drain.
        for (view, document) in documents {
            self.reading_query_documents.entry(view).or_insert(document);
        }
    }

    fn capture_reading_query_session(
        &self,
        cx: &App,
    ) -> Result<ProjectReadingQuerySession, String> {
        let session = self.session.read(cx);
        ProjectReadingQuerySession::new(
            session,
            session.deprojection_workspace_artifacts(),
            session.deprojection_workspace_interpretations(),
            ProjectQueryResolverInputs::default(),
            Arc::new(|_| {}),
        )
        .map_err(|error| error.to_string())
    }

    fn reading_query_view(
        &self,
        source: WorkspaceViewId,
        cx: &App,
    ) -> Option<Entity<ReadingQueryView>> {
        let WorkspacePaneRuntime::Hosted(host) = self.workspace_panes.get(&source)?.clone() else {
            return None;
        };
        let host = host.upgrade()?;
        let WorkspacePaneContent::ReadingQuery(view) = &host.read(cx).content else {
            return None;
        };
        Some(view.clone())
    }

    fn refresh_reading_query_inputs(
        &self,
        view: &Entity<ReadingQueryView>,
        cx: &mut Context<Self>,
    ) {
        let Ok(bridge) = self.capture_reading_query_session(cx) else {
            return;
        };
        let inputs = ReadingQueryViewInputs {
            query_provenance: Some(bridge.snapshot().provenance()),
            existing_entities: bridge
                .snapshot()
                .existing_foreign_entities()
                .into_iter()
                .collect(),
            base_revision: self
                .session
                .read(cx)
                .project_snapshot()
                .ok()
                .map(|snapshot| snapshot.revisions().aggregate),
            ..ReadingQueryViewInputs::default()
        };
        view.update(cx, |view, cx| view.observe_inputs(inputs, cx));
    }

    fn handle_pattern_workflows(&mut self, cx: &mut Context<Self>) {
        let workflows = std::mem::take(
            &mut *self
                .pattern_workflows
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for pending in workflows {
            let request = pending.request.id;
            let result = self.session.update(cx, |session, _| {
                session.execute_pattern_workflow(pending.request.intent)
            });
            match result {
                Ok(outcome) => {
                    pending.completion.update(cx, |editor, cx| {
                        editor.complete_workflow(request, Ok(outcome), cx);
                    });
                }
                Err(error) => {
                    self.constructive_status = Some(format!("Pattern workflow failed · {error}"));
                    pending.completion.update(cx, |editor, cx| {
                        editor.complete_workflow_failure(request, error.to_string(), cx);
                    });
                }
            }
        }
    }

    fn handle_control_actions(&mut self, cx: &mut Context<Self>) {
        let actions = self
            .control_actions
            .lock()
            .map(|mut actions| std::mem::take(&mut *actions))
            .unwrap_or_default();
        for pending in actions {
            if let Err(error) = self.session.update(cx, |session, _| {
                session.execute_control_action_for_editor(pending.editor_session, pending.action)
            }) {
                self.audio_error = Some(error.to_string());
            }
        }
        self.handle_pattern_workflows(cx);
        self.handle_session_events(cx);
    }

    fn handle_reverse_surface_events(&mut self, cx: &mut Context<Self>) {
        let events = self
            .reverse_surface_events
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default();
        for event in events {
            match event {
                ReverseSurfaceViewEvent::Action {
                    view,
                    intent: SurfaceActionIntent::Reveal(mut request),
                } => {
                    request.current_view = Some(view);
                    match self.session.read(cx).issue_reveal(request) {
                        Ok(receipt) => {
                            if let Ok(mut reveals) = self.object_reveals.lock() {
                                reveals.push(PendingObjectReveal {
                                    receipt,
                                    diagnostics: Vec::new(),
                                    headline: "Evidence selected".into(),
                                });
                            }
                        }
                        Err(error) => {
                            self.constructive_status =
                                Some(format!("Evidence reveal unavailable · {error}"));
                        }
                    }
                }
                ReverseSurfaceViewEvent::Action {
                    intent:
                        SurfaceActionIntent::ApplyExplicitConsequence {
                            consequence,
                            requested_at,
                            ..
                        },
                    ..
                } => {
                    let current = self
                        .session
                        .read(cx)
                        .project_snapshot()
                        .ok()
                        .map(|snapshot| snapshot.revisions());
                    self.constructive_status = Some(
                        if requested_at.is_some() && requested_at != current {
                            "Reverse edit was not applied because its project receipt is stale"
                                .into()
                        } else {
                            format!(
                            "{} · {:?} lowering is not connected yet; project state was not changed",
                            consequence.label, consequence.authority
                        )
                        },
                    );
                }
                ReverseSurfaceViewEvent::Audition { view, intent } => {
                    let request = match intent {
                        SurfaceAuditionIntent::Signal(request) => request,
                        SurfaceAuditionIntent::InspectExcess { controller, .. } => controller,
                    };
                    self.request_comparison_product(view, request, cx);
                }
            }
        }
        cx.notify();
    }

    fn request_comparison_product(
        &mut self,
        view: WorkspaceViewId,
        request: ComparisonSelectionRequest,
        cx: &mut Context<Self>,
    ) {
        let Some(controller) = self.reverse_surface_factory.controller(view) else {
            return;
        };
        let owner = controller
            .lock()
            .map(|controller| controller.owner())
            .unwrap_or_else(|poisoned| poisoned.into_inner().owner());
        let _ = self.audio_controller.stop_scoped_audition(owner);
        let semantics = match self.comparison_semantics_for(&request, cx) {
            Ok(semantics) => semantics,
            Err(message) => {
                if let Ok(mut controller) = controller.lock() {
                    let _ = controller.fail_request(&request, message.clone());
                }
                self.constructive_status = Some(message);
                self.reverse_surface_factory.refresh_controller(view, cx);
                self.publish_audio_status(cx);
                return;
            }
        };
        let capture = self.comparison_executor.capture(
            owner,
            request.clone(),
            self.session.read(cx),
            &self.audio_controller,
            semantics,
            ComparisonProductRecipe::default(),
        );
        match capture {
            Ok(job) => {
                self.constructive_status = Some(format!(
                    "Rendering aligned comparison {:?} {:?}",
                    request.comparison, request.channel
                ));
                let execution = cx.background_spawn(async move { job.execute() });
                cx.spawn(async move |this, cx| {
                    let result = execution.await;
                    let _ = this.update(cx, |this, cx| {
                        this.complete_comparison_product(view, owner, request, result, cx)
                    });
                })
                .detach();
            }
            Err(error) => {
                if let Ok(mut controller) = controller.lock() {
                    let _ = controller.fail_request(&request, error.to_string());
                }
                self.constructive_status = Some(error.to_string());
            }
        }
        self.reverse_surface_factory.refresh_controller(view, cx);
        self.publish_audio_status(cx);
    }

    fn comparison_semantics_for(
        &self,
        request: &ComparisonSelectionRequest,
        cx: &App,
    ) -> Result<ComparisonSemanticSnapshot, String> {
        let store = self
            .reverse_surface_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let comparison = store
            .get(&ObjectRef::Comparison(request.comparison))
            .and_then(|document| match &document.body {
                ReverseSurfaceBody::Comparison(comparison) => Some(comparison.clone()),
                _ => None,
            })
            .ok_or_else(|| {
                format!(
                    "Comparison {:?} has no hydrated semantic document",
                    request.comparison
                )
            })?;
        let explanation = store
            .get(&ObjectRef::Explanation(comparison.definition.explanation))
            .and_then(|document| match &document.body {
                ReverseSurfaceBody::Explanation(explanation) => {
                    Some(explanation.definition.clone())
                }
                _ => None,
            })
            .ok_or_else(|| {
                format!(
                    "Explanation {:?} has no hydrated semantic document",
                    comparison.definition.explanation
                )
            })?;
        let observation = comparison.observation.ok_or_else(|| {
            format!(
                "Comparison {:?} has no recorded observation",
                request.comparison
            )
        })?;
        drop(store);

        let mut interpretations = InterpretationStore::new();
        interpretations
            .apply(&[
                InterpretationCommand::PutExplanation {
                    before: None,
                    after: Some(explanation),
                },
                InterpretationCommand::PutComparison {
                    before: None,
                    after: Some(comparison.definition),
                },
                InterpretationCommand::PutObservation {
                    comparison: request.comparison,
                    before: None,
                    after: Some(observation),
                },
            ])
            .map_err(|error| format!("Comparison semantic hydration failed · {error}"))?;
        let source_artifacts = self.session.read(cx);
        let source_artifacts = source_artifacts.deprojection_workspace_artifacts();
        let mut artifacts = ArtifactCatalog::new();
        for descriptor in source_artifacts.descriptors().cloned() {
            let payload = source_artifacts
                .get::<ArtifactComparisonPayload>(descriptor.id)
                .map_err(|error| format!("Comparison artifact hydration failed · {error}"))?;
            artifacts
                .insert(descriptor, payload)
                .map_err(|error| format!("Comparison artifact hydration failed · {error}"))?;
        }
        Ok(ComparisonSemanticSnapshot {
            interpretations: Arc::new(interpretations),
            artifacts: Arc::new(artifacts),
        })
    }

    fn complete_comparison_product(
        &mut self,
        view: WorkspaceViewId,
        owner: AuditionOwner,
        request: ComparisonSelectionRequest,
        result: Result<ComparisonProductCompletion, ComparisonProductExecutorError>,
        cx: &mut Context<Self>,
    ) {
        let Some(shared_controller) = self.reverse_surface_factory.controller(view) else {
            self.comparison_executor.cancel_owner(owner);
            return;
        };
        let mut controller = shared_controller
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match result {
            Ok(completion) => {
                match self.comparison_executor.publish(
                    self.session.read(cx),
                    &mut controller,
                    completion,
                ) {
                    Ok(published) => {
                        let applied = self.audio.as_ref().ok_or_else(|| {
                        "comparison product is ready, but the project audio host is unavailable"
                            .to_owned()
                    }).and_then(|host| {
                        controller
                            .apply_audio_effect(
                                &mut self.audio_controller,
                                host,
                                published.effect,
                                AuditionAlignment::SeekToStart { play: true },
                            )
                            .map_err(|error| error.to_string())
                    });
                        self.constructive_status = Some(match applied {
                            Ok(()) => format!(
                                "Comparison {:?} {:?} is aligned to the project transport",
                                request.comparison, request.channel
                            ),
                            Err(error) => {
                                let _ = controller.fail_request(&request, error.clone());
                                error
                            }
                        });
                    }
                    Err(error) => {
                        let _ = controller.fail_request(&request, error.to_string());
                        self.constructive_status = Some(error.to_string());
                    }
                }
            }
            Err(error) => {
                let _ = controller.fail_request(&request, error.to_string());
                self.constructive_status = Some(error.to_string());
            }
        }
        drop(controller);
        self.reverse_surface_factory.refresh_controller(view, cx);
        self.publish_audio_status(cx);
        cx.notify();
    }

    fn handle_session_events(&mut self, cx: &mut Context<Self>) {
        let batch = self.session.read(cx).poll_events(&mut self.session_events);
        let deliveries = {
            let session = self.session.clone();
            self.pane_session_binding
                .consume_batch(session.read(cx), batch.clone())
        };
        if batch.missed_events {
            if let Ok(snapshot) = self.session.read(cx).project_snapshot() {
                let publication = ProjectPublication {
                    generation: self.session.read(cx).snapshot().generation,
                    revisions: snapshot.revisions(),
                    snapshot: snapshot.clone(),
                    change_set: None,
                };
                self.accept_project_publication(publication, cx);
            }
        } else {
            for event in batch.events {
                if let ProjectSessionEvent::ProjectPublished(publication) = event {
                    self.accept_project_publication(publication, cx);
                }
            }
        }
        match deliveries {
            Ok(deliveries) => {
                for delivery in deliveries {
                    self.apply_pane_session_delivery(delivery, cx);
                }
            }
            Err(error) => {
                self.constructive_status =
                    Some(format!("Workspace session fanout failed · {error}"));
            }
        }
    }

    fn register_workspace_runtime(
        &mut self,
        descriptor: &WorkspaceViewDescriptor,
        runtime: WorkspacePaneRuntime,
        cx: &mut Context<Self>,
    ) -> Result<(), SharedString> {
        self.install_unbound_workspace_runtime(descriptor.id, runtime, cx);
        self.attach_workspace_pane(descriptor, cx)
    }

    fn install_unbound_workspace_runtime(
        &mut self,
        view: WorkspaceViewId,
        runtime: WorkspacePaneRuntime,
        cx: &mut Context<Self>,
    ) {
        self.unregister_workspace_pane(view, cx);
        self.workspace_panes.insert(view, runtime);
    }

    fn set_workspace_completion(
        &mut self,
        view: WorkspaceViewId,
        completion: RevealCompletion,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(WorkspacePaneRuntime::Hosted(host)) = self.workspace_panes.get(&view).cloned()
        else {
            return false;
        };
        let Some(host) = host.upgrade() else {
            return false;
        };
        host.update(cx, |host, cx| host.set_completion(completion, cx));
        true
    }

    fn select_workspace_target(&mut self, view: WorkspaceViewId, cx: &mut Context<Self>) {
        let Some(WorkspacePaneRuntime::Hosted(host)) = self.workspace_panes.get(&view).cloned()
        else {
            return;
        };
        let Some(host) = host.upgrade() else {
            return;
        };
        let target = match &host.read(cx).content {
            WorkspacePaneContent::Sampler(sampler) => {
                let sampler = sampler.read(cx);
                let source = sampler.source().clone();
                let state = sampler.state();
                source.kits.lock().ok().and_then(|library| {
                    let kit = library.kits.get(&source.kit)?;
                    let primary = state.selected_pad.map_or_else(
                        || ObjectRef::Instrument(InstrumentRef::SampleKit(kit.id)),
                        |pad| {
                            ObjectRef::Pad(PadRef {
                                kit: kit.id,
                                pad,
                                zone: state.selected_zone,
                            })
                        },
                    );
                    let mut related = vec![ObjectRef::Instrument(InstrumentRef::SampleKit(kit.id))];
                    if let Some(material) = state
                        .selected_zone
                        .and_then(|zone| kit.zones.get(&zone))
                        .map(|zone| zone.material)
                    {
                        related.push(ObjectRef::Sample(material));
                    }
                    Some((primary, related, SelectionSource::Sampler))
                })
            }
            WorkspacePaneContent::Pattern(editor) => editor.read(cx).target().map(|target| {
                (
                    ObjectRef::Pattern(target.pattern),
                    Vec::new(),
                    SelectionSource::PatternEditor,
                )
            }),
            WorkspacePaneContent::Arrangement(arrangement) => {
                let arrangement = arrangement.read(cx);
                let editor = arrangement.editor();
                editor
                    .selection
                    .clips
                    .iter()
                    .next()
                    .copied()
                    .map(|primary| {
                        (
                            ObjectRef::AudioClip(primary),
                            editor
                                .selection
                                .clips
                                .iter()
                                .copied()
                                .filter(|clip| *clip != primary)
                                .map(ObjectRef::AudioClip)
                                .collect(),
                            SelectionSource::Arrangement,
                        )
                    })
            }
            _ => None,
        };
        let Some((primary, related, source)) = target else {
            return;
        };
        let guard = match self.session.read(cx).current_selection_guard() {
            Ok(guard) => guard,
            Err(error) => {
                self.constructive_status = Some(format!("Editor selection unavailable · {error}"));
                return;
            }
        };
        let previous = self.session.read(cx).selection().selection.clone();
        let mut selection = ProjectSelection::from_reveal(primary, related, guard, Some(view));
        selection.objects.provenance = SelectionProvenance {
            source,
            source_view: Some(view),
        };
        selection.time = previous.time;
        selection.aspect = previous.aspect;
        selection.signal = previous.signal;
        if let Err(error) = self.session.update(cx, |session, _| {
            session.replace_guarded_selection(selection)
        }) {
            self.constructive_status = Some(format!("Editor selection was stale · {error}"));
        }
    }

    fn activate_workspace_target(&mut self, view: WorkspaceViewId, cx: &mut Context<Self>) {
        self.active_workspace_view = Some(view);
        self.select_workspace_target(view, cx);
        if let Some(WorkspacePaneRuntime::Hosted(host)) = self.workspace_panes.get(&view).cloned() {
            if let Some(host) = host.upgrade() {
                if let WorkspacePaneContent::Sampler(sampler) = &host.read(cx).content {
                    self.sampler_selection_cache
                        .insert(view, sampler.read(cx).state());
                }
            }
        }
    }

    fn active_workspace_view(&self) -> Option<WorkspaceViewId> {
        self.active_workspace_view
    }

    /// The sampler currently owns pad focus internally. Until its view event
    /// surface carries that focus, observe only the active durable workspace
    /// pane and publish changes into the canonical project selection.
    fn sync_active_sampler_selection(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.active_workspace_view else {
            return;
        };
        let Some(WorkspacePaneRuntime::Hosted(host)) = self.workspace_panes.get(&view).cloned()
        else {
            return;
        };
        let Some(host) = host.upgrade() else {
            return;
        };
        let WorkspacePaneContent::Sampler(sampler) = &host.read(cx).content else {
            return;
        };
        let state = sampler.read(cx).state();
        if self.sampler_selection_cache.get(&view).copied() == Some(state) {
            return;
        }
        self.sampler_selection_cache.insert(view, state);
        self.select_workspace_target(view, cx);
    }

    fn attach_workspace_pane(
        &mut self,
        descriptor: &WorkspaceViewDescriptor,
        cx: &mut Context<Self>,
    ) -> Result<(), SharedString> {
        let registration = PaneSessionRegistration {
            view: descriptor.id,
            links: descriptor.links,
            topics: PaneSessionTopics::ALL,
        };
        let session = self.session.clone();
        let delivery = session
            .update(cx, |session, _| {
                self.pane_session_binding
                    .register_pane(session, registration)
            })
            .map_err(|error| SharedString::from(error.to_string()))?;
        self.apply_pane_session_delivery(delivery, cx);
        Ok(())
    }

    fn apply_workspace_binding_effect(
        &mut self,
        effect: PaneBindingEffect,
        cx: &mut Context<Self>,
    ) -> Result<(), SharedString> {
        let detached = match effect {
            PaneBindingEffect::Detach(pane) => Some(pane.0),
            PaneBindingEffect::Attach(_) => None,
        };
        let session = self.session.clone();
        let delivery = session
            .update(cx, |session, _| {
                effect.apply(&mut self.pane_session_binding, session)
            })
            .map_err(|error| SharedString::from(error.to_string()))?;
        if let Some(delivery) = delivery {
            self.apply_pane_session_delivery(delivery, cx);
        }
        if let Some(view) = detached {
            self.detach_workspace_pane(view, cx);
        }
        Ok(())
    }

    fn detach_workspace_pane(&mut self, view: WorkspaceViewId, cx: &mut Context<Self>) {
        if let Some(WorkspacePaneRuntime::Analysis(analysis)) =
            self.workspace_panes.get(&view).cloned()
        {
            if let Some(analysis) = analysis.upgrade() {
                let owner = analysis.read(cx).audition_owner;
                let _ = self.audio_controller.stop_scoped_audition(owner);
                if let Some(audio) = self.audio.as_ref() {
                    AnalysisPaneBridge::from_owner(owner)
                        .dispose_preview_effect()
                        .apply(&mut self.preview_controller, audio);
                }
            }
        }
        if workspace_audition_owner(view).ok() == self.pattern_audition_owner {
            if let Some(owner) = self.pattern_audition_owner.take() {
                let session = self.session.clone();
                let _ = session.update(cx, |session, _| {
                    self.pattern_audition
                        .stop(session, &mut self.audio_controller, owner)
                });
            }
        }
        if let Some(controller) = self.reverse_surface_factory.controller(view) {
            let owner = controller
                .lock()
                .map(|controller| controller.owner())
                .unwrap_or_else(|poisoned| poisoned.into_inner().owner());
            self.comparison_executor.cancel_owner(owner);
            let _ = self.audio_controller.stop_scoped_audition(owner);
        }
        if let Ok(bridge) = SamplePaneBridge::new(view) {
            if let Some(audio) = self.audio.as_ref() {
                bridge
                    .dispose_effect()
                    .apply(&mut self.preview_controller, audio);
            }
            let _ = self.audio_controller.stop_scoped_audition(bridge.owner());
        }
        self.pad_preview_tickets
            .retain(|(owner, _, _), _| *owner != view);
        let session = self.session.clone();
        session.update(cx, |session, _| {
            self.pane_session_binding.unregister_pane(session, view);
        });
    }

    fn unregister_workspace_pane(&mut self, view: WorkspaceViewId, cx: &mut Context<Self>) {
        self.detach_workspace_pane(view, cx);
        self.workspace_panes.remove(&view);
        self.sampler_selection_cache.remove(&view);
        if self.active_workspace_view == Some(view) {
            self.active_workspace_view = None;
        }
        let _ = self.reverse_surface_factory.release(view);
        self.reverse_surface_factory.remove_released();
    }

    fn reconcile_workspace_pane_visibility(
        &mut self,
        document: &WorkspaceDocument,
        cx: &mut Context<Self>,
    ) {
        self.retain_workspace_panes(document, cx);
        let panes = self
            .workspace_panes
            .iter()
            .map(|(&view, runtime)| (view, runtime.clone()))
            .collect::<Vec<_>>();
        for (view, _) in panes {
            let visible = document.location(view).is_ok_and(|location| {
                !matches!(location, crate::workspace_document::ViewLocation::Hidden)
            });
            let attached = self.pane_session_binding.contains(view);
            if visible && !attached {
                if let Some(descriptor) = document.views.get(&view) {
                    if let Err(error) = self.attach_workspace_pane(descriptor, cx) {
                        self.constructive_status =
                            Some(format!("Workspace pane attach failed · {error}"));
                    }
                }
            } else if !visible && attached {
                self.detach_workspace_pane(view, cx);
            }
        }
    }

    fn retain_workspace_panes(&mut self, document: &WorkspaceDocument, cx: &mut Context<Self>) {
        let stale = self
            .workspace_panes
            .keys()
            .copied()
            .filter(|view| !document.views.contains_key(view))
            .collect::<Vec<_>>();
        for view in stale {
            self.unregister_workspace_pane(view, cx);
        }
    }

    fn apply_pane_session_delivery(
        &mut self,
        delivery: PaneSessionDelivery,
        cx: &mut Context<Self>,
    ) {
        let Some(runtime) = self.workspace_panes.get(&delivery.recipient).cloned() else {
            return;
        };
        if matches!(runtime, WorkspacePaneRuntime::Reverse) {
            let _ = self
                .reverse_surface_factory
                .deliver(delivery.recipient, delivery.payload, cx);
            return;
        }
        match delivery.payload {
            PaneSessionPayload::FullState(snapshot) => {
                if let Some(publication) = snapshot.project {
                    self.apply_project_to_workspace_pane(
                        delivery.recipient,
                        &runtime,
                        publication,
                        cx,
                    );
                }
                self.apply_audio_to_workspace_pane(&runtime, snapshot.audio, cx);
                self.apply_selection_to_workspace_pane(
                    &runtime,
                    PaneSemanticSelection {
                        selection: snapshot.selection,
                        signal: snapshot.signal,
                        group: WorkspaceLinkGroupId::UNLINKED,
                        link_revision: snapshot.selection_revision,
                    },
                    cx,
                );
            }
            PaneSessionPayload::ProjectPublished(publication) => {
                self.apply_project_to_workspace_pane(delivery.recipient, &runtime, publication, cx);
            }
            PaneSessionPayload::SemanticSelection(selection) => {
                self.apply_selection_to_workspace_pane(&runtime, selection, cx);
            }
            PaneSessionPayload::AudioChanged(audio) => {
                self.apply_audio_to_workspace_pane(&runtime, audio, cx);
            }
        }
    }

    fn apply_audio_to_workspace_pane(
        &mut self,
        runtime: &WorkspacePaneRuntime,
        audio: ProjectAudioStatus,
        cx: &mut Context<Self>,
    ) {
        match runtime {
            WorkspacePaneRuntime::Overview => {
                self.observe_timeline_audio(&audio, cx);
            }
            WorkspacePaneRuntime::Analysis(view) => {
                let _ = view.update(cx, |view, cx| view.set_session_audio(audio, cx));
            }
            WorkspacePaneRuntime::Reverse => {}
            WorkspacePaneRuntime::Hosted(host) => {
                let _ = host.update(cx, |host, cx| host.set_audio(audio, cx));
            }
        }
    }

    fn apply_selection_to_workspace_pane(
        &mut self,
        runtime: &WorkspacePaneRuntime,
        selection: PaneSemanticSelection,
        cx: &mut Context<Self>,
    ) {
        match runtime {
            WorkspacePaneRuntime::Overview => {
                self.timeline_signal = selection.signal;
                let range = selection.selection.time.and_then(|range| {
                    TimelineRange::between(
                        TimelinePoint(range.start.max(0) as u64),
                        TimelinePoint(range.end.max(0) as u64),
                    )
                });
                let _ = self
                    .timeline_interaction
                    .apply(TimelineInteractionEvent::ReplaceSelection(range));
                self.sync_timeline_presentation();
                cx.notify();
            }
            WorkspacePaneRuntime::Analysis(view) => {
                let _ = view.update(cx, |view, cx| view.set_semantic_selection(selection, cx));
            }
            WorkspacePaneRuntime::Reverse => {}
            WorkspacePaneRuntime::Hosted(host) => {
                let _ = host.update(cx, |host, cx| host.set_semantic_selection(selection, cx));
            }
        }
    }

    fn apply_project_to_workspace_pane(
        &mut self,
        view_id: WorkspaceViewId,
        runtime: &WorkspacePaneRuntime,
        publication: ProjectPublication,
        cx: &mut Context<Self>,
    ) {
        match runtime {
            WorkspacePaneRuntime::Overview => {}
            WorkspacePaneRuntime::Analysis(view) => {
                let generation = publication.generation;
                let _ = view.update(cx, |view, cx| view.set_project_generation(generation, cx));
            }
            WorkspacePaneRuntime::Reverse => {}
            WorkspacePaneRuntime::Hosted(host) => {
                self.apply_project_to_host(view_id, host, publication, cx);
            }
        }
    }

    fn apply_project_to_host(
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
                if previous.is_none_or(|previous| previous.arrangement != revisions.arrangement) {
                    if let Ok(editor) = ArrangementEditor::from_state(domains.arrangement.clone()) {
                        let waveform = self.arrangement_waveform_provider(&publication.snapshot);
                        view.update(cx, |view, cx| {
                            view.set_waveform_provider(waveform);
                            view.set_project_snapshot(editor, revisions.aggregate, cx);
                        });
                    }
                }
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
                if previous.is_none_or(|previous| previous.automation != revisions.automation) {
                    view.update(cx, |view, cx| {
                        view.set_controller_snapshot(domains.automation.clone(), cx)
                    });
                }
            }
            WorkspacePaneContent::Analysis(view) => {
                view.update(cx, |view, cx| {
                    view.set_project_generation(publication.generation, cx)
                });
            }
            WorkspacePaneContent::Browser(view) => {
                if previous.is_none_or(|previous| previous.assets != revisions.assets) {
                    let state = view.read(cx).state().clone();
                    let events = Arc::clone(&self.asset_events);
                    let callback = Arc::new(move |event| {
                        if let Ok(mut events) = events.lock() {
                            events.push(event);
                        }
                    });
                    let registry = Arc::new(Mutex::new(domains.assets.clone()));
                    let replacement = cx.new(|cx| {
                        let mut view = AssetBrowserView::with_callback(
                            Arc::clone(&registry),
                            Some(callback),
                            cx,
                        );
                        view.set_state(state, cx);
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
                        let view = cx.new(|cx| {
                            AssetBrowserView::with_callback(registry, Some(callback), cx)
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
                    WorkspaceKind::AutomationEditor => domains
                        .automation
                        .lanes()
                        .next()
                        .map(|lane| lane.id)
                        .map(|target| {
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
                                AutomationView::from_controller_snapshot(
                                    domains.automation.clone(),
                                    target,
                                    callback,
                                    cx,
                                )
                            }))
                        }),
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

    fn pattern_view_for_publication(
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

    fn sampler_view_for_publication(
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

    fn accept_project_publication(
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

        if let Some(view) = self.arrangement_view.as_ref() {
            if let Ok(editor) = ArrangementEditor::from_state(domains.arrangement.clone()) {
                view.update(cx, |view, cx| {
                    view.set_project_snapshot(editor, publication.revisions.aggregate, cx);
                });
            }
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
                view.set_controller_snapshot(domains.automation.clone(), cx)
            });
        }

        self.request_project_audio(publication, cx);
        cx.notify();
    }

    fn request_project_audio(&mut self, publication: ProjectPublication, cx: &mut Context<Self>) {
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
                            match AudioHost::open_renderer(renderer) {
                                Ok(host) => {
                                    if let Err(error) = this.audio_controller.bind_audio_host(&host)
                                    {
                                        this.audio_controller = ProjectAudioController::new();
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
                                    let loop_state =
                                        this.timeline_interaction.snapshot().loop_state;
                                    this.apply_timeline_transport_effect(
                                        TimelineTransportEffect::SetLoop(loop_state),
                                        cx,
                                    );
                                }
                                Err(error) => {
                                    this.audio_controller = ProjectAudioController::new();
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
                            match AudioHost::open_renderer(renderer) {
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
                                                    ProjectAudioController::new();
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
                                        }
                                        Err(error) => {
                                            this.audio_controller = ProjectAudioController::new();
                                            this.audio_snapshot_digest = None;
                                            this.audio_error = Some(error.to_string());
                                        }
                                    }
                                }
                                Err(error) => {
                                    this.audio_controller = ProjectAudioController::new();
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
                if let Some(destination) = this.pending_export_destination.take() {
                    this.start_export_to(destination, cx);
                }
                cx.notify();
            });
        })
        .detach();
        self.publish_audio_status(cx);
    }

    fn tick_project_audio(&mut self, cx: &mut Context<Self>) {
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

    fn publish_audio_status(&mut self, cx: &mut Context<Self>) {
        let status = self.audio_controller.status();
        self.session.update(cx, |session, _| {
            session.set_audio_status(status);
        });
    }

    fn refresh_audible_export_audio(&mut self) {
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

    fn choose_project(&mut self, cx: &mut Context<Self>) {
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: Some(SharedString::from("Open audec project")),
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

    fn new_project(&mut self, cx: &mut Context<Self>) {
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

    fn package_root(&self) -> Option<PathBuf> {
        self.project_lifecycle
            .manifest_path()
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf)
    }

    fn observe_workspace(&mut self, document: WorkspaceDocument) {
        self.project_lifecycle.replace_workspace(Some(document));
    }

    fn open_project_package(
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

    fn save_project(
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

    fn save_as(
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

    fn export_wav(&mut self, cx: &mut Context<Self>) {
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

    fn start_export_to(&mut self, destination: PathBuf, cx: &mut Context<Self>) {
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
            let pinned = this.update(cx, |this, _cx| {
                this.audio_controller
                    .complete_current_export(completion)
                    .map(|rendered| RevisionPinnedAudio::new(revision, rendered.audio))
                    .map_err(|error| error.to_string())
            });
            let Ok(pinned) = pinned else {
                return;
            };
            let pinned = match pinned {
                Ok(pinned) => pinned,
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
                let package_root = destination
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .join(".audec-export-context");
                let package =
                    ProjectPackage::new(package_root).map_err(|error| error.to_string())?;
                ProjectFileActions::new(ProjectRepository::new(
                    ProjectStore::new(package),
                    JsonAirPayloadCodec,
                ))
                .export(
                    pinned,
                    &WavExportRequest::new(destination),
                    &mut NoopExportObserver,
                )
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

    fn is_project_dirty(&self, cx: &App) -> bool {
        self.project_lifecycle
            .is_dirty(self.session.read(cx))
            .unwrap_or(false)
    }

    fn replacement_disposition(&self, cx: &App) -> ProjectReplacementDisposition {
        self.project_lifecycle
            .replacement_disposition(self.session.read(cx))
            .unwrap_or(ProjectReplacementDisposition::Dirty)
    }

    fn maybe_autosave(&mut self, cx: &mut Context<Self>) {
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

    fn first_pattern_id(&self, cx: &App) -> u64 {
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

    fn first_automation_lane_id(&self, cx: &App) -> u64 {
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

    fn take_workspace_import(&mut self) -> Option<WorkspaceDocument> {
        self.pending_workspace_import.take()
    }

    fn set_product_shell_hosted(&mut self, hosted: bool, cx: &mut Context<Self>) {
        self.product_shell_hosted = hosted;
        cx.notify();
    }

    fn toggle_playback(&mut self, cx: &mut Context<Self>) {
        let event = if self
            .audio_controller
            .transport_session()
            .snapshot()
            .transport
            .mode
            == TransportMode::Playing
        {
            TimelineInteractionEvent::PauseRequested
        } else {
            TimelineInteractionEvent::PlayRequested
        };
        self.dispatch_timeline_event(event, cx);
    }

    fn seek_to(&mut self, seconds: f64, cx: &mut Context<Self>) {
        let duration = self
            .analysis()
            .map_or(0.0, |analysis| analysis.duration_seconds);
        let seconds = seconds.clamp(0.0, duration);
        self.playhead_seconds = seconds;
        if let Some(audio) = &self.audio {
            self.preview_controller.cancel_all(audio);
            self.pad_preview_tickets.clear();
            match audio.transport().format().frame_at_seconds(seconds) {
                Ok(frame) => {
                    if let Err(error) = self
                        .audio_controller
                        .apply_transport_intent(audio, ProjectTransportIntent::Seek(frame))
                    {
                        self.audio_error = Some(format!("{error:#}"));
                    }
                }
                Err(error) => self.audio_error = Some(error.to_string()),
            }
        }
        let playing = self.transport_is_playing();
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

    fn dispatch_timeline_event(&mut self, event: TimelineInteractionEvent, cx: &mut Context<Self>) {
        let effects = self.timeline_interaction.apply(event);
        self.apply_timeline_effects(effects, cx);
    }

    fn apply_timeline_effects(&mut self, effects: Vec<TimelineEffect>, cx: &mut Context<Self>) {
        let selection = effects.iter().find_map(|effect| match effect {
            TimelineEffect::SelectionChanged(selection) => selection.range,
            _ => None,
        });
        let authored_loop = effects.iter().find_map(|effect| match effect {
            TimelineEffect::LoopChanged(loop_state) if loop_state.enabled => loop_state.range,
            _ => None,
        });
        let atomic_selection_loop = selection.filter(|range| Some(*range) == authored_loop);
        let collapsed_seek = selection.is_none()
            && effects.iter().any(|effect| {
                matches!(
                    effect,
                    TimelineEffect::Transport(TimelineTransportEffect::Seek { .. })
                )
            });
        if let Some(range) = atomic_selection_loop {
            if let Ok(range) = FrameRange::new(
                ProjectFrame(range.start.get()),
                ProjectFrame(range.end.get()),
            ) {
                self.apply_project_transport_command(
                    ProjectTransportCommand::ReplaceSelectionAndLoop(range),
                    cx,
                );
            }
        }
        for effect in effects {
            match effect {
                TimelineEffect::SelectionPreview(range) => {
                    self.timeline_selection = range.map(sample_range_from_timeline);
                    cx.notify();
                }
                TimelineEffect::SelectionChanged(selection) => {
                    self.timeline_selection = selection.range.map(sample_range_from_timeline);
                    self.publish_overview_semantic_selection(self.timeline_selection, cx);
                    if atomic_selection_loop.is_none() {
                        let selection = selection.range.and_then(|range| {
                            FrameRange::new(
                                ProjectFrame(range.start.get()),
                                ProjectFrame(range.end.get()),
                            )
                            .ok()
                        });
                        self.apply_project_transport_command(
                            ProjectTransportCommand::ReplaceSelection(selection),
                            cx,
                        );
                    }
                    cx.notify();
                }
                TimelineEffect::CursorChanged(_) => {}
                TimelineEffect::LoopChanged(loop_state) => {
                    self.loop_range = loop_state.range.map(sample_range_from_timeline);
                    self.loop_enabled = loop_state.enabled;
                    cx.notify();
                }
                TimelineEffect::Transport(effect) => {
                    let redundant_atomic_transport = match effect {
                        TimelineTransportEffect::SetLoop(_) => atomic_selection_loop.is_some(),
                        TimelineTransportEffect::Seek { to, .. } => {
                            atomic_selection_loop.is_some_and(|range| to == range.start)
                        }
                        _ => false,
                    };
                    let collapsed_click_loop_update =
                        collapsed_seek && matches!(effect, TimelineTransportEffect::SetLoop(_));
                    if !redundant_atomic_transport && !collapsed_click_loop_update {
                        self.apply_timeline_transport_effect(effect, cx)
                    }
                }
                TimelineEffect::ViewportChanged { owner, viewport }
                    if owner == TimelineControllerId(WorkspaceViewId::TRACK_OVERVIEW.0) =>
                {
                    self.timeline_viewport = viewport;
                    self.refresh_spectrogram_detail(cx);
                    cx.notify();
                }
                TimelineEffect::ViewportChanged { .. } => {}
                TimelineEffect::FollowChanged(follow) => {
                    self.timeline_follow = !matches!(follow, TimelineFollowState::Off);
                    self.apply_project_transport_command(
                        ProjectTransportCommand::SetFollow(if self.timeline_follow {
                            ProjectTransportFollowPolicy::Playhead
                        } else {
                            ProjectTransportFollowPolicy::Off
                        }),
                        cx,
                    );
                    cx.notify();
                }
            }
        }
    }

    fn apply_project_transport_command(
        &mut self,
        command: ProjectTransportCommand,
        cx: &mut Context<Self>,
    ) {
        let Some(audio) = self.audio.as_ref() else {
            return;
        };
        self.preview_controller.cancel_all(audio);
        self.pad_preview_tickets.clear();
        if let Err(error) = self
            .audio_controller
            .apply_transport_command(audio, command)
        {
            self.audio_error = Some(error.to_string());
        }
        self.publish_audio_status(cx);
    }

    fn apply_timeline_transport_effect(
        &mut self,
        effect: TimelineTransportEffect,
        cx: &mut Context<Self>,
    ) {
        let Some(audio) = self.audio.as_ref() else {
            return;
        };
        self.preview_controller.cancel_all(audio);
        self.pad_preview_tickets.clear();
        let intent = match effect {
            TimelineTransportEffect::SetLoop(loop_state) => {
                if let Some(range) = loop_state.range {
                    let Ok(range) = FrameRange::new(
                        ProjectFrame(range.start.get()),
                        ProjectFrame(range.end.get()),
                    ) else {
                        self.audio_error = Some("Loop range is empty".into());
                        return;
                    };
                    ProjectTransportIntent::SetLoop {
                        range,
                        enabled: loop_state.enabled,
                    }
                } else {
                    ProjectTransportIntent::ClearLoop
                }
            }
            TimelineTransportEffect::Seek { to, .. } => {
                ProjectTransportIntent::Seek(ProjectFrame(to.get()))
            }
            TimelineTransportEffect::Play => ProjectTransportIntent::Play,
            TimelineTransportEffect::Pause => ProjectTransportIntent::Pause,
            TimelineTransportEffect::Stop => ProjectTransportIntent::Stop,
        };
        if let Err(error) = self.audio_controller.apply_transport_intent(audio, intent) {
            self.audio_error = Some(error.to_string());
        }
        self.publish_audio_status(cx);
        cx.notify();
    }

    fn observe_timeline_audio(&mut self, audio: &ProjectAudioStatus, cx: &mut Context<Self>) {
        let loop_state = TimelineLoopState {
            range: audio.transport.loop_region.and_then(|range| {
                TimelineRange::new(TimelinePoint(range.start.0), TimelinePoint(range.end.0))
            }),
            enabled: audio.transport.loop_enabled,
        };
        let _ = self
            .timeline_interaction
            .apply(TimelineInteractionEvent::ReplaceLoop(loop_state));
        let effects =
            self.timeline_interaction
                .apply(TimelineInteractionEvent::TransportObserved {
                    playhead: TimelinePoint(audio.transport.frame.0),
                    mode: timeline_playback_mode(audio.transport.mode),
                });
        self.sync_timeline_presentation();
        // Only pane-local follow/viewport effects are applied from a transport
        // observation. The project-audio publication is already authoritative
        // and must not be echoed back into the host.
        for effect in effects {
            match effect {
                TimelineEffect::ViewportChanged { owner, viewport }
                    if owner == TimelineControllerId(WorkspaceViewId::TRACK_OVERVIEW.0) =>
                {
                    self.timeline_viewport = viewport;
                    self.refresh_spectrogram_detail(cx);
                }
                TimelineEffect::FollowChanged(follow) => {
                    self.timeline_follow = !matches!(follow, TimelineFollowState::Off)
                }
                _ => {}
            }
        }
    }

    fn sync_timeline_presentation(&mut self) {
        let snapshot = self.timeline_interaction.snapshot();
        self.timeline_viewport = snapshot.viewport;
        self.timeline_follow = !matches!(snapshot.follow, TimelineFollowState::Off);
        self.timeline_selection = snapshot.selection.range.map(sample_range_from_timeline);
        self.loop_range = snapshot.loop_state.range.map(sample_range_from_timeline);
        self.loop_enabled = snapshot.loop_state.enabled;
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
        self.dispatch_timeline_event(
            TimelineInteractionEvent::PointerDown {
                at: TimelinePoint(sample),
                loop_policy: LoopEditPolicy::for_range_gesture(event.modifiers.alt),
            },
            cx,
        );
    }

    fn extend_timeline_selection(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !event.dragging() {
            return;
        }
        let Some(sample) = self.sample_from_x(event.position.x, true) else {
            return;
        };
        self.dispatch_timeline_event(
            TimelineInteractionEvent::PointerMove {
                at: TimelinePoint(sample),
            },
            cx,
        );
    }

    fn end_timeline_selection(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.timeline_interaction.snapshot().pointer.is_none() {
            return;
        }
        let release = self
            .sample_from_x(event.position.x, true)
            .unwrap_or_else(|| {
                self.timeline_interaction
                    .snapshot()
                    .pointer
                    .unwrap()
                    .anchor
                    .get()
            });
        self.dispatch_timeline_event(
            TimelineInteractionEvent::PointerUp {
                at: TimelinePoint(release),
            },
            cx,
        );
    }

    fn publish_overview_semantic_selection(
        &mut self,
        range: Option<SampleRange>,
        cx: &mut Context<Self>,
    ) {
        let mut selection = self.session.read(cx).selection().selection.clone();
        selection.time = range.map(|range| FrameSpan {
            start: range.start.get(),
            end: range.end.get(),
        });
        selection.aspect = selection.time.map(Aspect::Time);
        selection.signal = Some(self.timeline_signal);
        let session = self.session.clone();
        if let Err(error) = session.update(cx, |session, _| {
            self.pane_session_binding.publish_semantic_selection(
                session,
                WorkspaceViewId::TRACK_OVERVIEW,
                selection,
            )
        }) {
            self.constructive_status =
                Some(format!("Timeline selection was not published · {error}"));
        }
    }

    fn publish_arrangement_selection(
        &mut self,
        source: WorkspaceViewId,
        intent: SelectionIntent,
        cx: &mut Context<Self>,
    ) {
        let Ok(snapshot) = self.session.read(cx).project_snapshot().cloned() else {
            return;
        };
        let arrangement = &snapshot.project.state().domains.arrangement;
        let mut selection = self.session.read(cx).selection().selection.clone();
        let mut primary_clip = None;
        match intent {
            SelectionIntent::Clips { ids, primary, mode } => {
                apply_project_id_selection(&mut selection.clips, ids, mode);
                primary_clip = primary.filter(|clip| selection.clips.contains(clip));
            }
            SelectionIntent::Marquee {
                range,
                tracks,
                mode,
            } => {
                let ids = arrangement
                    .clips
                    .values()
                    .filter(|clip| {
                        (tracks.is_empty() || tracks.contains(&clip.track_id))
                            && clip.placement.intersects(range)
                    })
                    .map(|clip| clip.id)
                    .collect();
                apply_project_id_selection(&mut selection.clips, ids, mode);
            }
            SelectionIntent::ClearObjects => selection.clear_objects(),
        }
        primary_clip = primary_clip.or_else(|| selection.clips.iter().next().copied());
        selection.primary = primary_clip.map(SelectableId::Clip);
        selection.tracks = selection
            .clips
            .iter()
            .filter_map(|clip| arrangement.clip(*clip).map(|clip| clip.track_id))
            .collect();
        selection.time = selected_arrangement_frame_span(arrangement, &selection.clips);
        selection.aspect = selection.time.map(Aspect::Time);
        let session = self.session.clone();
        if let Err(error) = session.update(cx, |session, _| {
            if let Some(primary) = primary_clip {
                let guard = session.current_selection_guard()?;
                selection.objects = ObjectSelection::guarded(
                    ObjectRef::AudioClip(primary),
                    selection
                        .clips
                        .iter()
                        .copied()
                        .filter(|clip| *clip != primary)
                        .map(ObjectRef::AudioClip),
                    guard,
                    SelectionProvenance {
                        source: SelectionSource::Arrangement,
                        source_view: Some(source),
                    },
                );
                session.replace_guarded_selection(selection.clone())?;
            } else {
                selection.objects = ObjectSelection::default();
            }
            self.pane_session_binding
                .publish_semantic_selection(session, source, selection)
        }) {
            self.constructive_status =
                Some(format!("Arrangement selection was not published · {error}"));
        }
    }

    fn zoom_timeline(&mut self, anchor: u64, scale: f64, cx: &mut Context<Self>) {
        self.dispatch_timeline_event(
            TimelineInteractionEvent::ZoomAround {
                anchor: TimelinePoint(anchor),
                scale,
            },
            cx,
        );
    }

    fn pan_timeline(&mut self, fraction: f64, cx: &mut Context<Self>) {
        self.dispatch_timeline_event(TimelineInteractionEvent::PanFraction(fraction), cx);
    }

    fn fit_timeline(&mut self, cx: &mut Context<Self>) {
        self.dispatch_timeline_event(TimelineInteractionEvent::Fit, cx);
    }

    fn follow_timeline(&mut self, cx: &mut Context<Self>) {
        self.dispatch_timeline_event(
            TimelineInteractionEvent::SetFollow(TimelineFollowState::Playhead {
                margin_fraction: 0.16,
            }),
            cx,
        );
    }

    fn set_loop_from_selection(&mut self, cx: &mut Context<Self>) {
        self.apply_project_transport_command(ProjectTransportCommand::SetLoopFromSelection, cx);
        let effects = self
            .timeline_interaction
            .apply(TimelineInteractionEvent::SetLoopFromSelection)
            .into_iter()
            .filter(|effect| !matches!(effect, TimelineEffect::Transport(_)))
            .collect();
        self.apply_timeline_effects(effects, cx);
    }

    fn toggle_loop(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.audio_controller.transport_session().snapshot();
        let command = if snapshot.transport.loop_region.is_some() {
            Some(ProjectTransportCommand::SetLoopEnabled(
                !snapshot.transport.loop_enabled,
            ))
        } else if snapshot.selection.is_some() {
            Some(ProjectTransportCommand::SetLoopFromSelection)
        } else {
            None
        };
        if let Some(command) = command {
            self.apply_project_transport_command(command, cx);
            let status = self.audio_controller.transport_session().snapshot();
            let loop_state = TimelineLoopState {
                range: status.transport.loop_region.and_then(|range| {
                    TimelineRange::new(TimelinePoint(range.start.0), TimelinePoint(range.end.0))
                }),
                enabled: status.transport.loop_enabled,
            };
            let _ = self
                .timeline_interaction
                .apply(TimelineInteractionEvent::ReplaceLoop(loop_state));
            self.sync_timeline_presentation();
        } else {
            self.audio_error = Some("Select a range before enabling loop".into());
        }
        cx.notify();
    }

    fn publish_timeline_sample(
        &mut self,
        intent: WorkbenchSampleIntent,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        let Some(range) = self.timeline_selection.filter(|range| !range.is_empty()) else {
            self.constructive_status = Some("Select a non-empty source range first".into());
            cx.notify();
            return;
        };
        if let Some(analysis) = self.analysis() {
            let frames = range.len();
            if !within_interactive_sampling_limit(frames, analysis.sample_rate) {
                self.constructive_status = Some(
                    "Interactive sampling is currently limited to 30-second selections".into(),
                );
                cx.notify();
                return;
            }
        }
        match self.session.update(cx, |session, _| {
            session.publish_primary_workbench_range(range, intent)
        }) {
            Ok(outcome) => {
                let revision = outcome.constructive.update.revisions().aggregate;
                self.constructive_status = Some(format!("{label} · revision {revision}"));
                let mut recommendation = recommend_constructive(&outcome.constructive.publication);
                recommendation.request.current_view = Some(WorkspaceViewId::TRACK_OVERVIEW);
                match self.session.read(cx).issue_reveal(recommendation.request) {
                    Ok(receipt) => {
                        if let Ok(mut reveals) = self.object_reveals.lock() {
                            reveals.push(PendingObjectReveal {
                                receipt,
                                diagnostics: recommendation.diagnostics,
                                headline: label.into(),
                            });
                        }
                    }
                    Err(error) => {
                        self.constructive_status =
                            Some(format!("{label} · reveal unavailable · {error}"));
                    }
                }
                self.handle_session_events(cx);
            }
            Err(error) => self.constructive_status = Some(format!("{label} failed · {error}")),
        }
        cx.notify();
    }

    fn save_selection_as_one_shot(&mut self, cx: &mut Context<Self>) {
        self.publish_timeline_sample(
            WorkbenchSampleIntent::OneShot {
                kit: SampleKitDestination::NewKit,
                target_bus: None,
            },
            "Sample created",
            cx,
        );
    }

    fn chop_selection_to_pads(&mut self, cx: &mut Context<Self>) {
        self.publish_timeline_sample(
            WorkbenchSampleIntent::Chop {
                chop: SampleChopIntent::EqualSlices { count: 8 },
                kit: SampleKitDestination::NewKit,
                target_bus: None,
            },
            "Kit created",
            cx,
        );
    }

    fn make_beat_from_selection(&mut self, cx: &mut Context<Self>) {
        self.publish_timeline_sample(
            WorkbenchSampleIntent::MakeBeat {
                chop: SampleChopIntent::EqualSlices { count: 8 },
                kit: SampleKitDestination::NewKit,
                target_bus: None,
                bars: 1,
                quantize_ticks: (crate::sequencer::PPQ / 4) as u64,
                result_focus: MakeBeatResultFocus::PatternEditor,
            },
            "Beat placed",
            cx,
        );
    }

    fn make_beat_from_sampler(&mut self, view: WorkspaceViewId, cx: &mut Context<Self>) {
        let sampler = match self.workspace_panes.get(&view).cloned() {
            Some(WorkspacePaneRuntime::Hosted(host)) => {
                host.upgrade()
                    .and_then(|host| match &host.read(cx).content {
                        WorkspacePaneContent::Sampler(sampler) => Some(sampler.clone()),
                        _ => None,
                    })
            }
            _ => None,
        };
        let Some(sampler) = sampler else {
            self.constructive_status = Some("The instrument editor is no longer available".into());
            cx.notify();
            return;
        };
        let (source, state) = {
            let sampler = sampler.read(cx);
            (sampler.source().clone(), sampler.state())
        };
        let resolved = source
            .kits
            .lock()
            .map_err(|_| "The instrument library is busy".to_owned())
            .and_then(|library| {
                let kit = library
                    .kits
                    .get(&source.kit)
                    .ok_or_else(|| "The visible instrument is no longer current".to_owned())?;
                let zone = state
                    .selected_zone
                    .and_then(|zone| kit.zones.get(&zone))
                    .or_else(|| {
                        state
                            .selected_pad
                            .and_then(|pad| kit.ordered_zones(pad).next())
                    })
                    .ok_or_else(|| "Select a playable zone before making a beat".to_owned())?;
                let selection = match zone.material {
                    SourceMaterialRef::Asset(asset) => SampleSelection::whole_asset(asset),
                    SourceMaterialRef::VirtualSlice(slice) => SampleSelection {
                        asset: slice.source_asset,
                        source_range: Some(slice.source_range),
                    },
                };
                Ok((selection, kit.revision))
            });
        let (selection, expected_revision) = match resolved {
            Ok(resolved) => resolved,
            Err(message) => {
                let _ = self.set_workspace_completion(
                    view,
                    RevealCompletion {
                        headline: "Beat not created".into(),
                        breadcrumb: "Instrument › selected zone".into(),
                        diagnostic: Some(message),
                    },
                    cx,
                );
                return;
            }
        };
        let id = NEXT_CONTEXTUAL_SAMPLE_REQUEST.fetch_add(1, Ordering::Relaxed);
        let request = SampleActionRequest {
            id: SampleRequestId(id.max(1)),
            action: SampleAction::MakeBeat(MakeBeatIntent {
                source: selection,
                chop: SampleChopIntent::OneShot,
                kit: SampleKitDestination::ExistingKit {
                    kit: source.kit,
                    expected_revision,
                },
                target_bus: None,
                bars: 1,
                quantize_ticks: (crate::sequencer::PPQ / 4) as u64,
                result_focus: MakeBeatResultFocus::PatternEditor,
            }),
        };
        if let Ok(mut actions) = self.sample_actions.lock() {
            actions.push(PendingSampleRequest {
                request,
                completion: None,
                source: Some(view),
            });
        }
        let _ = self.set_workspace_completion(
            view,
            RevealCompletion {
                headline: "Making beat from selected zone…".into(),
                breadcrumb: "Instrument → Beat → Pattern".into(),
                diagnostic: None,
            },
            cx,
        );
    }

    fn capture_pane_source(
        &self,
        span: RenderSpan,
        sample_rate: u32,
        source_mono: &[f32],
        cx: &App,
    ) -> Result<PaneSourcePin, String> {
        let session = self.session.read(cx);
        let revisions = session
            .project_snapshot()
            .map_err(|error| error.to_string())?
            .revisions();
        let format = RenderFormat::new(sample_rate, 1).map_err(|error| error.to_string())?;
        PaneSourcePin::new(
            session.document_generation(),
            session.snapshot().generation,
            revisions,
            None,
            span,
            format,
            source_mono,
        )
        .map_err(|error| error.to_string())
    }

    fn pane_audition_context(&self, cx: &App) -> Result<PaneAuditionContext, String> {
        let session = self.session.read(cx);
        let revisions = session
            .project_snapshot()
            .map_err(|error| error.to_string())?
            .revisions();
        Ok(PaneAuditionContext {
            document_generation: session.document_generation(),
            publication_generation: session.snapshot().generation,
            revisions,
            audible_cohort: self
                .audio_controller
                .transport_session()
                .snapshot()
                .audible_cohort,
        })
    }

    fn audition_pane_timeline(
        &mut self,
        owner: AuditionOwner,
        kind: PaneAudioKind,
        source: PaneSourcePin,
        mono: Arc<[f32]>,
        cx: &mut Context<Self>,
    ) {
        let Some(audio) = self.audio.as_ref() else {
            self.audio_error = Some("Project audio is not ready for aligned audition".into());
            cx.notify();
            return;
        };
        if !self.primary_source_timeline_aligned {
            self.audio_error = Some(
                "This analysis is not mapped to an exact project placement; aligned timeline audition is unavailable"
                    .into(),
            );
            cx.notify();
            return;
        }
        let Some(control) = self.audio_controller.renderer_control() else {
            self.audio_error = Some("Project renderer is not ready for aligned audition".into());
            cx.notify();
            return;
        };
        let current = match self.pane_audition_context(cx) {
            Ok(current) => current,
            Err(error) => {
                self.audio_error = Some(error);
                cx.notify();
                return;
            }
        };
        let effect = AnalysisPaneBridge::from_owner(owner).timeline_mono(
            kind,
            source,
            control.format(),
            mono,
            AuditionAlignment::SeekToStart { play: true },
        );
        let result =
            effect.and_then(|effect| effect.apply(&mut self.audio_controller, audio, &current));
        match result {
            Ok(()) => {
                self.publish_audio_status(cx);
                cx.notify();
            }
            Err(error) => {
                self.audio_error = Some(error.to_string());
                cx.notify();
            }
        }
    }

    fn preview_pane_mono(
        &mut self,
        owner: AuditionOwner,
        kind: PaneAudioKind,
        source: &PaneSourcePin,
        sample_rate: u32,
        mono: Arc<[f32]>,
        cx: &mut Context<Self>,
    ) {
        let Some(audio) = self.audio.as_ref() else {
            self.audio_error = Some("Project preview bus is not ready".into());
            cx.notify();
            return;
        };
        let current = match self.pane_audition_context(cx) {
            Ok(current) => current,
            Err(error) => {
                self.audio_error = Some(error);
                cx.notify();
                return;
            }
        };
        let effect = AnalysisPaneBridge::from_owner(owner).short_preview_mono(
            &mut self.preview_controller,
            kind,
            source,
            &current,
            sample_rate,
            mono,
        );
        match effect {
            Ok(effect) => {
                effect.apply(&mut self.preview_controller, audio);
                self.audio_error = None;
            }
            Err(error) => self.audio_error = Some(error.to_string()),
        }
        cx.notify();
    }

    fn analysis(&self) -> Option<&Analysis> {
        match &self.state {
            ProjectState::Ready(analysis) => Some(analysis),
            _ => None,
        }
    }

    fn transport_is_playing(&self) -> bool {
        self.audio_controller
            .transport_session()
            .snapshot()
            .transport
            .mode
            == TransportMode::Playing
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

    fn create_arrangement_view(
        &mut self,
        source: Option<WorkspaceViewId>,
        cx: &mut Context<Self>,
    ) -> Entity<ArrangementView> {
        if let Ok(snapshot) = self.session.read(cx).project_snapshot().cloned() {
            let domains = &snapshot.project.state().domains;
            let aggregate_revision = snapshot.revisions().aggregate;
            let map = domains.sequencer.tempo_map();
            let bpm = map
                .tempo_points()
                .first()
                .map_or(120.0, |point| point.tempo.bpm());
            let beats_per_bar = map.meter_points().first().map_or(4, |point| {
                point.signature.numerator.min(u16::from(u8::MAX)) as u8
            });
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
            let playhead =
                ArrangementFrame::new(i64::try_from(self.playhead_sample()).unwrap_or(i64::MAX));
            let playing = self.transport_is_playing();
            entity.update(cx, |editor, cx| {
                editor.set_tempo(bpm, beats_per_bar, cx);
                editor.set_project_revision(aggregate_revision, cx);
                editor.set_selection(selection, cx);
                editor.set_playhead(playhead, playing, cx);
            });
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
            entity
        }
    }

    fn open_arrangement_editor(&mut self, cx: &mut Context<Self>) {
        let editor = self.arrangement_view.clone().unwrap_or_else(|| {
            let editor = self.create_arrangement_view(None, cx);
            self.arrangement_view = Some(editor.clone());
            editor
        });
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

    fn open_mixer(&mut self, cx: &mut Context<Self>) {
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
            let mixer = cx.new(MixerView::demo);
            self.mixer_view = Some(mixer.clone());
            mixer
        };
        open_editor_entity(mixer, "Mixer", cx);
    }

    fn open_automation(&mut self, cx: &mut Context<Self>) {
        let automation = if let Some(automation) = &self.automation_view {
            automation.clone()
        } else if let Ok(snapshot) = self.session.read(cx).project_snapshot().cloned() {
            let graph = snapshot.project.state().domains.automation.clone();
            let Some(target) = graph.lanes().next().map(|lane| lane.id) else {
                let automation = cx.new(AutomationView::demo);
                self.automation_view = Some(automation.clone());
                open_editor_entity(automation, "Automation", cx);
                return;
            };
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
                cx.new(|cx| AutomationView::from_controller_snapshot(graph, target, callback, cx));
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
        self.install_browser_sample_callbacks(&browser, None, cx);
        open_editor_entity(browser, "Media pool", cx);
    }

    fn create_workspace_pane(
        &mut self,
        descriptor: &WorkspaceViewDescriptor,
        cx: &mut Context<Self>,
    ) -> Result<PaneRegistration, SharedString> {
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
                let view = if let Ok(snapshot) = self.session.read(cx).project_snapshot().cloned() {
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
                    cx.new(|cx| MixerView::from_controller_snapshot(graph, target, callback, cx))
                } else {
                    cx.new(MixerView::demo)
                };
                WorkspacePaneContent::Mixer(view)
            }
            WorkspaceKind::AutomationEditor => {
                let view = if let Ok(snapshot) = self.session.read(cx).project_snapshot().cloned() {
                    let graph = snapshot.project.state().domains.automation.clone();
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
                    if let Some(target) = target {
                        cx.new(|cx| {
                            AutomationView::from_controller_snapshot(graph, target, callback, cx)
                        })
                    } else {
                        cx.new(AutomationView::demo)
                    }
                } else {
                    cx.new(AutomationView::demo)
                };
                WorkspacePaneContent::Automation(view)
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
                let workbench = cx.entity();
                let view = cx.new(|cx| Visualizer::new(kind, workbench, cx));
                view.update(cx, |view, _| view.set_workspace_view_id(descriptor.id));
                if kind == VizKind::Rhythm {
                    view.update(cx, |view, cx| view.refresh_rhythm(cx));
                } else if kind == VizKind::Separation {
                    view.update(cx, |view, cx| view.refresh_hpss(cx));
                } else if kind == VizKind::Loom {
                    view.update(cx, |view, cx| view.refresh_loom(cx));
                }
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

    fn finish_workspace_pane(
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

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_playing = self.transport_is_playing();
        let transport_enabled =
            self.audio.is_some() || self.session.read(cx).project_snapshot().is_ok();
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
                        .on_click(cx.listener(|_this, _, window, cx| {
                            if let Some(handle) = window.window_handle().downcast::<DawWorkspace>()
                            {
                                let _ = handle.update(cx, |workspace, window, cx| {
                                    workspace.request_project_replacement(
                                        ProjectReplacementIntent::ChooseAudio,
                                        window,
                                        cx,
                                    )
                                });
                            }
                        }))
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
            .id("workbench-material-rail")
            .w(px(220.0))
            .h_full()
            .flex_none()
            // The workbench can be hosted in an arbitrarily short split pane.
            // Keep the rail inside that allocation and let every command stay
            // reachable instead of painting beneath the window edge.
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.material_rail_scroll)
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
            .child(div().when(!self.product_shell_hosted, |editors| {
                editors.child(section_label("EDIT / RECONSTRUCT")).child(
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
            }))
            .child(section_label("MAKE FROM SELECTION"))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(if self
                        .timeline_selection
                        .is_some_and(|range| !range.is_empty())
                    {
                        MUTED
                    } else {
                        DIM
                    }))
                    .child(self.timeline_selection.filter(|range| !range.is_empty()).map_or_else(
                        || "Drag a source range first".to_owned(),
                        |range| {
                            format!(
                                "{} – {}",
                                format_time(self.seconds_for_sample(range.start.get().max(0) as u64)),
                                format_time(self.seconds_for_sample(range.end.get().max(0) as u64))
                            )
                        },
                    )),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .id("selection-one-shot")
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.save_selection_as_one_shot(cx)
                            }))
                            .child("Make sample"),
                    )
                    .child(
                        div()
                            .id("selection-chop-pads")
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.chop_selection_to_pads(cx)
                            }))
                            .child("Slice to kit"),
                    ),
            )
            .child(
                div()
                    .id("selection-make-beat")
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(CYAN))
                    .text_color(rgb(CYAN))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(BORDER)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.make_beat_from_selection(cx)
                    }))
                    .child("Make beat"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(DIM))
                    .child("Sample/kit → open Instrument · Beat → open Pattern"),
            )
            .when_some(self.constructive_status.clone(), |panel, status| {
                panel.child(div().text_xs().text_color(rgb(MUTED)).child(status))
            })
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

    fn render_inspector(&self, cx: &App) -> impl IntoElement {
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
        let audio_status = self.session.read(cx).audio_status().clone();
        let audio_runtime = audio_status.scoped_audition.map_or_else(
            || format!("{:?}", audio_status.render),
            |audition| {
                format!(
                    "{:?} · {:?} {:?}",
                    audio_status.render, audition.subject, audition.phase
                )
            },
        );
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
            .id("workbench-inspector-rail")
            .w(px(220.0))
            .h_full()
            .flex_none()
            // Mirrors the material rail: inspector metadata and diagnostics
            // remain bounded and scrollable in short/tiled workspaces.
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.inspector_rail_scroll)
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
            .child(section_label("PROJECT AUDIO"))
            .child(metric("RUNTIME", audio_runtime, LIME))
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
            .when_some(self.project_io_status.label(), |this, status| {
                this.child(div().text_xs().text_color(rgb(AMBER)).child(status))
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
                let loop_label = self.loop_range.map_or_else(
                    || "NO LOOP".to_owned(),
                    |range| {
                        format!(
                            "{} — {}",
                            format_time(self.seconds_for_sample(range.start.get().max(0) as u64)),
                            format_time(self.seconds_for_sample(range.end.get().max(0) as u64))
                        )
                    },
                );
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
                        loop_label,
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
                    .when(!self.product_shell_hosted, |row| {
                        row.child(self.render_sidebar(cx))
                    })
                    .child(self.render_timeline(cx))
                    .when(!self.product_shell_hosted, |row| {
                        row.child(self.render_inspector(cx))
                    }),
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
    source: PaneSourcePin,
    start_frame: u64,
    end_frame: u64,
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
    Ready(Arc<RhythmViewResult>),
    Failed(String),
}

struct RhythmViewResult {
    source: PaneSourcePin,
    source_pcm: Arc<[f32]>,
    deprojection: Arc<RhythmDeprojection>,
}

impl std::ops::Deref for RhythmViewResult {
    type Target = RhythmDeprojection;

    fn deref(&self) -> &Self::Target {
        &self.deprojection
    }
}

struct LoomViewResult {
    source: PaneSourcePin,
    template_source: PaneSourcePin,
    sketch: SequenceSketch,
    selected_cluster: usize,
    start_sample: usize,
    end_sample: usize,
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
    audition_owner: AuditionOwner,
    session_project_generation: Option<u64>,
    session_audio: ProjectAudioStatus,
    semantic_selection: Option<PaneSemanticSelection>,
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
            audition_owner: AuditionOwner {
                namespace: 0x6175_6465_633a_7669_7a,
                local: NEXT_VISUALIZER_AUDITION_OWNER.fetch_add(1, Ordering::Relaxed),
            },
            session_project_generation: None,
            session_audio: ProjectAudioStatus::default(),
            semantic_selection: None,
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

    fn set_project_generation(&mut self, generation: u64, cx: &mut Context<Self>) {
        self.session_project_generation = Some(generation);
        cx.notify();
    }

    fn set_workspace_view_id(&mut self, view: WorkspaceViewId) {
        if let Ok(owner) = workspace_audition_owner(view) {
            self.audition_owner = owner;
        }
    }

    fn set_session_audio(&mut self, audio: ProjectAudioStatus, cx: &mut Context<Self>) {
        self.session_audio = audio;
        cx.notify();
    }

    fn set_semantic_selection(&mut self, selection: PaneSemanticSelection, cx: &mut Context<Self>) {
        self.semantic_selection = Some(selection);
        // Selection attention never changes this pane's viewport or follow
        // policy. Those are pane-local presentation facts by contract.
        cx.notify();
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
        let source = {
            let workbench = self.workbench.read(cx);
            workbench.analysis().and_then(|analysis| {
                let session = workbench.session.read(cx);
                let revisions = session.project_snapshot().ok()?.revisions();
                Some((
                    analysis.mono_pcm.clone(),
                    analysis.sample_rate,
                    analysis.path.clone(),
                    session.snapshot().generation,
                    revisions,
                ))
            })
        };
        let Some((mono, sample_rate, path, publication_generation, project_revisions)) = source
        else {
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
            (path, mono, Arc::new(result))
        });
        cx.spawn(async move |this, cx| {
            let (path, mono, result) = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.rhythm_generation != generation
                    || this.spectrogram_source.as_ref() != Some(&path)
                {
                    return;
                }
                this.rhythm_state = match result.status {
                    RhythmAnalysisStatus::Complete => {
                        let workbench = this.workbench.clone();
                        let publication = workbench.update(cx, |workbench, cx| {
                            let current = workbench.session.read(cx);
                            let current_revisions = current
                                .project_snapshot()
                                .ok()
                                .map(|snapshot| snapshot.revisions());
                            if current.snapshot().generation != publication_generation
                                || current_revisions != Some(project_revisions)
                            {
                                return Err(
                                    "rhythm analysis completed after its project publication was superseded"
                                        .to_owned(),
                                );
                            }
                            let descriptor = rhythm_artifact_descriptor(&mono, sample_rate)?;
                            let rendered = RenderedExplanation {
                                origin_frame: descriptor.extent.start,
                                audio: ProjectAudio::from_interleaved(
                                    AudioFormat::new(sample_rate, 1)
                                        .map_err(|error| error.to_string())?,
                                    mono.as_ref().to_vec(),
                                )
                                .map_err(|error| error.to_string())?,
                            };
                            let cancellation = RenderCancellation::new();
                            let session = workbench.session.clone();
                            session
                                .update(cx, |session, _| {
                                    session.publish_live_deprojection_analysis(
                                        LiveDeprojectionAnalysis::from_rhythm(
                                            descriptor,
                                            result.as_ref().clone(),
                                            ExplainBudget::default(),
                                            rendered,
                                        ),
                                        &cancellation,
                                    )
                                })
                                .map_err(|error| error.to_string())
                        });
                        match publication {
                            Ok(candidates) => {
                                this.workbench.update(cx, |workbench, cx| {
                                    workbench.constructive_status = Some(format!(
                                        "Published {} live rhythm deprojection candidate(s)",
                                        candidates.len()
                                    ));
                                    cx.notify();
                                });
                            }
                            Err(error) => {
                                this.workbench.update(cx, |workbench, cx| {
                                    workbench.constructive_status = Some(format!(
                                        "Rhythm deprojection was analyzed but not published · {error}"
                                    ));
                                    cx.notify();
                                });
                            }
                        }
                        let source = i64::try_from(mono.len())
                            .map_err(|_| "rhythm source is too large".to_owned())
                            .and_then(|end| {
                                RenderSpan::new(0, end).map_err(|error| error.to_string())
                            })
                            .and_then(|span| {
                                this.workbench.read(cx).capture_pane_source(
                                    span,
                                    sample_rate,
                                    mono.as_ref(),
                                    cx,
                                )
                            });
                        match source {
                            Ok(source) => RhythmViewState::Ready(Arc::new(RhythmViewResult {
                                source,
                                source_pcm: Arc::clone(&mono),
                                deprojection: result,
                            })),
                            Err(error) => RhythmViewState::Failed(format!(
                                "Rhythm result could not retain its project receipt · {error}"
                            )),
                        }
                    }
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
        let Some(samples) = result.source_pcm.get(span.start..span.end) else {
            return;
        };
        let owner = self.audition_owner;
        let source = result.source.clone();
        let sample_rate = result.sample_rate;
        let samples: Arc<[f32]> = Arc::from(samples);
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| {
            workbench.preview_pane_mono(
                owner,
                PaneAudioKind::RhythmFamilyMedoid,
                &source,
                sample_rate,
                samples,
                cx,
            )
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
        let source = i64::try_from(start_frame)
            .map_err(|_| "HPSS start frame exceeds the signed project timeline".to_owned())
            .and_then(|start| {
                i64::try_from(end_frame)
                    .map(|end| (start, end))
                    .map_err(|_| "HPSS end frame exceeds the signed project timeline".to_owned())
            })
            .and_then(|(start, end)| RenderSpan::new(start, end).map_err(|error| error.to_string()))
            .and_then(|span| {
                self.workbench
                    .read(cx)
                    .capture_pane_source(span, sample_rate, &original, cx)
            });
        let source = match source {
            Ok(source) => source,
            Err(error) => {
                self.hpss_state = HpssViewState::Failed(format!(
                    "Selected-span transform could not retain its project receipt · {error}"
                ));
                cx.notify();
                return;
            }
        };

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
                        source,
                        start_frame: start_frame as u64,
                        end_frame: end_frame as u64,
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
        let (samples, audio_kind) = match kind {
            HpssAudition::Original => (result.original.clone(), PaneAudioKind::HpssSource),
            HpssAudition::Harmonic => (
                result.separation.harmonic.clone(),
                PaneAudioKind::HpssHarmonic,
            ),
            HpssAudition::Percussive => (
                result.separation.percussive.clone(),
                PaneAudioKind::HpssTransient,
            ),
            HpssAudition::Residual => (
                result.separation.residual.clone(),
                PaneAudioKind::HpssResidual,
            ),
        };
        let owner = self.audition_owner;
        let source = result.source.clone();
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| {
            workbench.audition_pane_timeline(owner, audio_kind, source, Arc::from(samples), cx)
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
        let pins = i64::try_from(frame_count)
            .map_err(|_| "Loom source exceeds the signed project timeline".to_owned())
            .and_then(|end| RenderSpan::new(0, end).map_err(|error| error.to_string()))
            .and_then(|full_span| {
                let workbench = self.workbench.read(cx);
                let template_source =
                    workbench.capture_pane_source(full_span, sample_rate, &mono, cx)?;
                let start = i64::try_from(start_sample)
                    .map_err(|_| "Loom span start exceeds the signed timeline".to_owned())?;
                let end = i64::try_from(end_sample)
                    .map_err(|_| "Loom span end exceeds the signed timeline".to_owned())?;
                let span = RenderSpan::new(start, end).map_err(|error| error.to_string())?;
                let original = mono
                    .get(start_sample..end_sample)
                    .ok_or_else(|| "Loom span lies outside retained PCM".to_owned())?;
                let source = workbench.capture_pane_source(span, sample_rate, original, cx)?;
                Ok((source, template_source))
            });
        let (source_pin, template_source_pin) = match pins {
            Ok(pins) => pins,
            Err(error) => {
                self.loom_state = LoomViewState::Failed(format!(
                    "Loom inference could not retain its project receipt · {error}"
                ));
                cx.notify();
                return;
            }
        };
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
            let result = build_loom_result(
                sketch,
                &mono,
                start_sample,
                end_sample,
                sample_rate,
                0,
                source_pin,
                template_source_pin,
            );
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
            workbench
                .analysis()
                .map(|analysis| -> Result<_, String> {
                    let frame_count = analysis.waveform_pyramid.frame_count();
                    let start_sample = (self.time_start * frame_count as f64).floor() as usize;
                    let end_sample = (self.time_end * frame_count as f64).ceil() as usize;
                    let original = analysis.mono_range(start_sample, end_sample);
                    let start = i64::try_from(start_sample)
                        .map_err(|_| "Loom span start exceeds the signed timeline".to_owned())?;
                    let end = i64::try_from(end_sample)
                        .map_err(|_| "Loom span end exceeds the signed timeline".to_owned())?;
                    let span = RenderSpan::new(start, end).map_err(|error| error.to_string())?;
                    let source =
                        workbench.capture_pane_source(span, analysis.sample_rate, &original, cx)?;
                    let current = workbench.pane_audition_context(cx)?;
                    Ok((
                        start_sample,
                        end_sample,
                        analysis.sample_rate,
                        original,
                        source,
                        current,
                    ))
                })
                .transpose()
        };
        let Ok(Some((start_sample, end_sample, sample_rate, original, source, current))) = source
        else {
            return;
        };
        let LoomViewState::Ready(result) = &mut self.loom_state else {
            return;
        };
        if result
            .template_source
            .validate_current(
                current.document_generation,
                current.publication_generation,
                current.revisions,
                current.audible_cohort.as_ref(),
            )
            .is_err()
        {
            return;
        }
        result.source = source;
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
        let sample_rate = result.sample_rate;
        let owner = self.audition_owner;
        let aligned = match kind {
            LoomAudition::Original => Some((result.original.clone(), PaneAudioKind::LoomSource)),
            LoomAudition::Reconstruction => Some((
                result.reconstruction.clone(),
                PaneAudioKind::LoomConstruction,
            )),
            LoomAudition::Residual => Some((result.residual.clone(), PaneAudioKind::LoomResidual)),
            LoomAudition::Template => None,
        };
        let source = result.source.clone();
        let template_source = result.template_source.clone();
        let template = selected_loom_cluster_id(result)
            .and_then(|cluster_id| result.sketch.cluster(cluster_id))
            .map(|cluster| Arc::<[f32]>::from(cluster.template.samples.clone()));
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| match (aligned, template) {
            (Some((samples, kind)), _) => {
                workbench.audition_pane_timeline(owner, kind, source, Arc::from(samples), cx)
            }
            (None, Some(template)) => workbench.preview_pane_mono(
                owner,
                PaneAudioKind::LoomTemplate,
                &template_source,
                sample_rate,
                template,
                cx,
            ),
            (None, None) => {
                workbench.audio_error = Some("The selected Loom template is empty".into());
                cx.notify();
            }
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
                let result_for_plot = Arc::clone(&result.deprojection);
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
                    .child("NMF factors recurring mixed-audio magnitude shapes. These are evidence-only: phase was not retained, so audec will not pretend they are auditionable isolated sources or instrument labels."),
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
    source_pin: PaneSourcePin,
    template_source_pin: PaneSourcePin,
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
        source: source_pin,
        template_source: template_source_pin,
        sketch,
        selected_cluster,
        start_sample,
        end_sample,
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
    _end_sample: usize,
    sample_rate: u32,
) {
    let end_sample = start_sample.saturating_add(original.len());
    result.start_sample = start_sample;
    result.end_sample = end_sample;
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
    loop_label: String,
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
                                    "{zoom:.1}× · {} · {loop_label}",
                                    if loop_enabled { "LOOP ON" } else { "LOOP OFF" }
                                )),
                        )
                        .child(viz_control("arrangement-set-loop", "Set loop").on_click(
                            cx.listener(|this, _, _, cx| this.set_loop_from_selection(cx)),
                        ))
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

fn arrangement_selection_from_project(selection: &ProjectSelection) -> ArrangementSelection {
    ArrangementSelection {
        clips: selection.clips.clone(),
        tracks: selection.tracks.clone(),
        time: selection.time.and_then(|range| {
            ArrangementFrameRange::new(
                ArrangementFrame::new(range.start),
                ArrangementFrame::new(range.end),
            )
            .ok()
        }),
    }
}

fn apply_project_id_selection(
    current: &mut BTreeSet<crate::arrangement::ClipId>,
    incoming: BTreeSet<crate::arrangement::ClipId>,
    mode: SelectionMode,
) {
    match mode {
        SelectionMode::Replace => *current = incoming,
        SelectionMode::Add => current.extend(incoming),
        SelectionMode::Toggle => {
            for id in incoming {
                if !current.remove(&id) {
                    current.insert(id);
                }
            }
        }
    }
}

fn selected_arrangement_frame_span(
    arrangement: &crate::arrangement::ArrangementState,
    clips: &BTreeSet<crate::arrangement::ClipId>,
) -> Option<FrameSpan> {
    let mut selected = clips
        .iter()
        .filter_map(|clip| arrangement.clip(*clip))
        .map(|clip| clip.placement);
    let first = selected.next()?;
    let (start, end) = selected.fold(
        (first.start.get(), first.end.get()),
        |(start, end), range| (start.min(range.start.get()), end.max(range.end.get())),
    );
    Some(FrameSpan { start, end })
}

fn workspace_pattern_source(
    descriptor: &WorkspaceViewDescriptor,
    project: &LiveProjectSnapshot,
    preferred_occurrence: Option<crate::pattern_use_graph::PatternOccurrenceTarget>,
) -> SequencerEditorSource {
    let sequencer = project.project.state().domains.sequencer.clone();
    let requested = match descriptor.target {
        WorkspaceTarget::PatternDefinition { id } if id != 0 => {
            Some(crate::sequencer::PatternId::from_raw(id))
        }
        _ => None,
    };
    let selected = requested
        .filter(|id| sequencer.patterns().get(*id).is_some())
        .or_else(|| {
            sequencer
                .patterns()
                .patterns()
                .next()
                .map(|pattern| pattern.id)
        });
    let Some(pattern) = selected else {
        return SequencerEditorSource::new(
            Arc::new(Mutex::new(sequencer)),
            None,
            None,
            workspace_view_title(descriptor),
        );
    };
    let mode = match sequencer
        .patterns()
        .get(pattern)
        .map(|pattern| &pattern.content)
    {
        Some(PatternContent::Notes(_)) => PatternEditorMode::PianoRoll,
        Some(PatternContent::Steps(_)) => PatternEditorMode::Steps,
        None => {
            return SequencerEditorSource::new(
                Arc::new(Mutex::new(sequencer)),
                None,
                None,
                workspace_view_title(descriptor),
            );
        }
    };
    hydrated_pattern_source(
        project,
        sequencer,
        PatternEditorTarget::new(pattern, mode),
        preferred_occurrence,
        workspace_view_title(descriptor),
    )
}

fn hydrated_pattern_source(
    project: &LiveProjectSnapshot,
    sequencer: crate::sequencer::Sequencer,
    target: PatternEditorTarget,
    preferred_occurrence: Option<crate::pattern_use_graph::PatternOccurrenceTarget>,
    title: SharedString,
) -> SequencerEditorSource {
    let snapshot = PatternUseSnapshot::from_project(&project.project);
    let hydration = hydrate_pattern_editor(snapshot, target, None).and_then(|definition| {
        preferred_occurrence
            .filter(|preferred| {
                definition
                    .uses
                    .occurrences
                    .iter()
                    .any(|occurrence| occurrence.target == *preferred)
            })
            .or_else(|| {
                definition
                    .uses
                    .occurrences
                    .first()
                    .map(|occurrence| occurrence.target)
            })
            .map(|occurrence| hydrate_pattern_editor(snapshot, target, Some(occurrence)))
            .unwrap_or(Ok(definition))
    });
    match hydration {
        Ok(hydration) => SequencerEditorSource::from_workflow_hydration(
            Arc::new(Mutex::new(sequencer)),
            hydration,
            title,
        ),
        Err(error) => {
            eprintln!("hydrating pattern editor: {error}");
            SequencerEditorSource::targeted(Arc::new(Mutex::new(sequencer)), target, title)
        }
    }
}

fn browser_state_from_descriptor(
    descriptor: &WorkspaceViewDescriptor,
) -> Option<AssetBrowserState> {
    let WorkspaceViewState::Browser {
        search,
        selected_asset_id,
    } = &descriptor.state
    else {
        return None;
    };
    let mut state = AssetBrowserState::default();
    state.search = search.clone();
    state.selected = selected_asset_id.map(crate::assets::AssetId);
    Some(state)
}

fn sampler_target_from_descriptor(descriptor: &WorkspaceViewDescriptor) -> Option<SamplerTarget> {
    let WorkspaceTarget::Extension { namespace, key } = &descriptor.target else {
        return None;
    };
    if namespace != "audec" {
        return None;
    }
    key.strip_prefix("kit:")
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|raw| *raw != 0)
        .map(crate::sample_kit::KitId::from_raw)
        .map(SamplerTarget::Kit)
}

fn add_product_object_to_selection(
    selection: &mut ProjectSelection,
    object: &ObjectRef,
    project: &crate::daw_project::DawProject,
) {
    let arrangement = &project.state().domains.arrangement;
    match object {
        ObjectRef::Material(asset) => {
            selection.assets.insert(*asset);
        }
        ObjectRef::Sample(material) => {
            selection.assets.insert(match material {
                SourceMaterialRef::Asset(asset) => *asset,
                SourceMaterialRef::VirtualSlice(slice) => slice.source_asset,
            });
        }
        ObjectRef::Pattern(pattern) => {
            selection.patterns.insert(*pattern);
        }
        ObjectRef::PatternOccurrence(occurrence) => {
            selection.clips.insert(occurrence.arrangement_clip);
            if let Some(clip) = arrangement.clip(occurrence.arrangement_clip) {
                selection.tracks.insert(clip.track_id);
                selection.time = Some(FrameSpan {
                    start: clip.placement.start.get(),
                    end: clip.placement.end.get(),
                });
                selection.aspect = selection.time.map(Aspect::Time);
            }
        }
        ObjectRef::AudioClip(clip) => {
            selection.clips.insert(*clip);
            if let Some(clip) = arrangement.clip(*clip) {
                selection.tracks.insert(clip.track_id);
                selection.time = Some(FrameSpan {
                    start: clip.placement.start.get(),
                    end: clip.placement.end.get(),
                });
                selection.aspect = selection.time.map(Aspect::Time);
            }
        }
        ObjectRef::AutomationOccurrence(occurrence) => {
            selection.clips.insert(occurrence.arrangement_clip);
            selection.automation_lanes.insert(occurrence.lane);
            if let Some(clip) = arrangement.clip(occurrence.arrangement_clip) {
                selection.tracks.insert(clip.track_id);
                selection.time = Some(FrameSpan {
                    start: clip.placement.start.get(),
                    end: clip.placement.end.get(),
                });
                selection.aspect = selection.time.map(Aspect::Time);
            }
        }
        ObjectRef::Track(track) => {
            selection.tracks.insert(*track);
        }
        ObjectRef::Bus(bus) => {
            selection.mixer_buses.insert(*bus);
        }
        ObjectRef::Automation(lane) => {
            selection.automation_lanes.insert(*lane);
        }
        ObjectRef::Instrument(_)
        | ObjectRef::Pad(_)
        | ObjectRef::Finding(_)
        | ObjectRef::Explanation(_)
        | ObjectRef::Comparison(_)
        | ObjectRef::Reading(_) => {}
    }
}

fn object_asset(object: &ObjectRef) -> Option<crate::assets::AssetId> {
    match object {
        ObjectRef::Material(asset) => Some(*asset),
        ObjectRef::Sample(SourceMaterialRef::Asset(asset)) => Some(*asset),
        ObjectRef::Sample(SourceMaterialRef::VirtualSlice(slice)) => Some(slice.source_asset),
        _ => None,
    }
}

fn project_contains_object(project: &crate::daw_project::DawProject, object: &ObjectRef) -> bool {
    let domains = &project.state().domains;
    match object {
        ObjectRef::Material(asset) => domains.assets.get(*asset).is_some(),
        ObjectRef::Sample(material) => domains.assets.get(material.asset_id()).is_some(),
        ObjectRef::Instrument(InstrumentRef::SampleKit(kit)) => {
            domains.sample_kits.kits.contains_key(kit)
        }
        ObjectRef::Pad(pad) => domains.sample_kits.kits.get(&pad.kit).is_some_and(|kit| {
            kit.pads.contains_key(&pad.pad)
                && pad
                    .zone
                    .is_none_or(|zone| kit.zones.get(&zone).is_some_and(|zone| zone.pad == pad.pad))
        }),
        ObjectRef::Pattern(pattern) => domains.sequencer.patterns().get(*pattern).is_some(),
        ObjectRef::PatternOccurrence(occurrence) => {
            domains
                .arrangement
                .clip(occurrence.arrangement_clip)
                .is_some()
                && occurrence
                    .sequencer_clip
                    .is_none_or(|clip| domains.sequencer.clip(clip).is_some())
                && occurrence
                    .pattern
                    .is_none_or(|pattern| domains.sequencer.patterns().get(pattern).is_some())
        }
        ObjectRef::AudioClip(clip) => domains.arrangement.clip(*clip).is_some(),
        ObjectRef::Track(track) => domains.arrangement.track(*track).is_some(),
        ObjectRef::Bus(bus) => domains.mixer.bus(*bus).is_some(),
        ObjectRef::Automation(lane) => domains.automation.lane(*lane).is_some(),
        ObjectRef::AutomationOccurrence(occurrence) => {
            domains
                .arrangement
                .clip(occurrence.arrangement_clip)
                .is_some()
                && domains.automation.lane(occurrence.lane).is_some()
        }
        // These product lanes have their own durable catalogs. The project
        // selection boundary must not erase them merely because this view's
        // project aggregate cannot authoritatively query those stores.
        ObjectRef::Finding(_)
        | ObjectRef::Explanation(_)
        | ObjectRef::Comparison(_)
        | ObjectRef::Reading(_) => true,
    }
}

fn reveal_breadcrumb(object: &ObjectRef) -> &'static str {
    match object {
        ObjectRef::Material(_) => "Library › selected material",
        ObjectRef::Sample(_) => "Library › selected sample",
        ObjectRef::Instrument(_) => "Instrument › new kit",
        ObjectRef::Pad(_) => "Instrument › new kit › selected pad",
        ObjectRef::Pattern(_) => "Pattern › new pattern",
        ObjectRef::PatternOccurrence(_) => "Arrange › selected pattern occurrence",
        ObjectRef::AudioClip(_) => "Arrange › selected audio clip",
        ObjectRef::AutomationOccurrence(_) => "Arrange › selected automation occurrence",
        ObjectRef::Track(_) => "Arrange › selected track",
        ObjectRef::Bus(_) => "Mixer › selected bus",
        ObjectRef::Automation(_) => "Automation › selected lane",
        ObjectRef::Finding(_) => "Findings › selected finding",
        ObjectRef::Explanation(_) => "Explanation › selected construction",
        ObjectRef::Comparison(_) => "Compare › selected comparison",
        ObjectRef::Reading(_) => "Reading › selected reading",
    }
}

fn project_audio_recipe(
    publication: &ProjectPublication,
    session: ProjectSessionId,
) -> Result<ProjectAudioRenderRecipe, String> {
    let payloads = crate::project_codecs::encode_constructive(&publication.snapshot.project)
        .map_err(|error| error.to_string())?;
    let canonical = serde_json::to_vec(&payloads.0).map_err(|error| error.to_string())?;
    let snapshot = sha256_content(b"audec:project-audio-snapshot:v1", &[&canonical]);
    let configuration = sha256_content(
        b"audec:daw-engine-configuration:v1",
        &[b"DawEngineConfig::default"],
    );
    // Stable for this open project session and deliberately independent of
    // the edited snapshot. Revisions remain in the plan/product keys; making
    // the namespace revision-shaped would defeat cross-revision tile reuse.
    let project_namespace = u128::from_be_bytes(*b"audec-session-v1") ^ u128::from(session.0);
    ProjectAudioRenderRecipe::audition(
        publication,
        Arc::new(DawEngineConfig::default()),
        ProjectAudioPlanStamp {
            project_namespace,
            snapshot: ExactDigest::new(snapshot.bytes),
            engine_abi: 1,
            engine_configuration: ExactDigest::new(configuration.bytes),
            dependencies: Vec::new(),
            determinism: DeterminismGrade::BitExact,
            // DawEngineSchedule's public subwindow render is exact: built-in
            // stateful instruments replay from the frozen schedule start.
            // This is potentially O(playhead), but semantically stateless at
            // the render-call boundary and therefore safe for exact tiles.
            tileability: Tileability::Stateless,
        },
    )
    .map_err(|error| error.to_string())
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

struct WorkspaceNotice {
    message: SharedString,
}

impl WorkspaceNotice {
    fn new(message: impl Into<SharedString>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Render for WorkspaceNotice {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(rgb(MUTED))
            .child(self.message.clone())
    }
}

fn workspace_view_title(descriptor: &WorkspaceViewDescriptor) -> SharedString {
    descriptor
        .title_override
        .clone()
        .unwrap_or_else(|| match &descriptor.kind {
            WorkspaceKind::Overview => "Overview".into(),
            WorkspaceKind::Arrangement => "Arrangement".into(),
            WorkspaceKind::Browser => "Media pool".into(),
            WorkspaceKind::Inspector => "Inspector".into(),
            WorkspaceKind::PatternEditor {
                mode: WorkspacePatternMode::PianoRoll,
            } => "Piano roll".into(),
            WorkspaceKind::PatternEditor {
                mode: WorkspacePatternMode::Steps,
            } => "Step sequencer".into(),
            WorkspaceKind::AutomationEditor => "Automation".into(),
            WorkspaceKind::Mixer => "Mixer".into(),
            WorkspaceKind::AnalysisLens { lens } => format!("{lens:?}"),
            WorkspaceKind::Render => "Render comparison".into(),
            WorkspaceKind::Extension { namespace, name }
                if namespace == "audec" && name == "sampler" =>
            {
                "Instrument".into()
            }
            WorkspaceKind::Extension { name, .. } => name.clone(),
        })
        .into()
}

fn default_view(kind: WorkspaceKind, target: WorkspaceTarget) -> NewWorkspaceView {
    let state = match &kind {
        WorkspaceKind::Overview => WorkspaceViewState::Overview {
            viewport: WorkspaceFrameViewport { start: 0, end: 1 },
            follow: true,
        },
        WorkspaceKind::Arrangement => WorkspaceViewState::Arrangement {
            viewport: WorkspaceFrameViewport { start: 0, end: 1 },
            follow: true,
            header_width: Some(190.0),
        },
        WorkspaceKind::Browser => WorkspaceViewState::Browser {
            search: String::new(),
            selected_asset_id: None,
        },
        WorkspaceKind::Inspector => WorkspaceViewState::Inspector,
        WorkspaceKind::PatternEditor { .. } => WorkspaceViewState::Pattern {
            viewport: WorkspaceBeatViewport {
                start_tick: 0,
                end_tick: crate::sequencer::PPQ * 16,
            },
            vertical_origin: None,
        },
        WorkspaceKind::AutomationEditor => WorkspaceViewState::Automation {
            viewport: WorkspaceBeatViewport {
                start_tick: 0,
                end_tick: crate::sequencer::PPQ * 16,
            },
        },
        WorkspaceKind::Mixer => WorkspaceViewState::Mixer,
        WorkspaceKind::AnalysisLens { .. } => WorkspaceViewState::Analysis {
            viewport: WorkspaceFrameViewport { start: 0, end: 1 },
            follow: true,
            min_frequency_hz: Some(MIN_FREQUENCY),
            max_frequency_hz: Some(MAX_FREQUENCY),
            recipe_fingerprint: None,
        },
        WorkspaceKind::Render => WorkspaceViewState::Render,
        WorkspaceKind::Extension { .. } => WorkspaceViewState::Extension {
            data: serde_json::Value::Null,
        },
    };
    NewWorkspaceView {
        kind,
        target,
        title_override: None,
        links: WorkspaceLinkMembership {
            group: WorkspaceLinkGroupId::UNLINKED,
            facets: WorkspaceLinkFacets::NONE,
        },
        state,
        extensions: Default::default(),
    }
}

fn analysis_view(lens: AnalysisLensKind) -> NewWorkspaceView {
    default_view(
        WorkspaceKind::AnalysisLens { lens },
        WorkspaceTarget::Analysis { source_id: None },
    )
}

/// Build the real dock/tab workspace around the existing workbench and lens
/// entities. The initial single-pane layout preserves the workbench's useful
/// vertical detail; Guise can then split, tab, and tear off these same entity
/// handles without resetting their view state.
#[derive(Clone)]
struct RevealCompletion {
    headline: String,
    breadcrumb: String,
    diagnostic: Option<String>,
}

const WORKSPACE_SESSION_LAYOUT_EXTENSION: &str = "audec.workspace-session-layout.v1";

fn workspace_document_from_layout(
    layout: &Arc<Mutex<WorkspaceSessionLayout>>,
) -> WorkspaceDocument {
    layout
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

fn replace_workspace_layout_document(
    published: &Arc<Mutex<WorkspaceSessionLayout>>,
    mut document: WorkspaceDocument,
    preserve_presentation: bool,
) {
    let mut layout = published
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if preserve_presentation {
        if let Ok(previous) = layout.export_document() {
            if let Some(metadata) = previous.extensions.get(WORKSPACE_SESSION_LAYOUT_EXTENSION) {
                document
                    .extensions
                    .insert(WORKSPACE_SESSION_LAYOUT_EXTENSION.into(), metadata.clone());
            }
        }
    }
    match WorkspaceSessionLayout::from_document(layout.session_id(), document) {
        Ok(next) => *layout = next,
        Err(error) => eprintln!("publishing workspace session layout: {error}"),
    }
}

fn workspace_focus_target(descriptor: &WorkspaceViewDescriptor) -> Option<FocusTarget> {
    match descriptor.kind {
        WorkspaceKind::Overview | WorkspaceKind::Arrangement => {
            Some(FocusTarget::ArrangementSurface(descriptor.id))
        }
        WorkspaceKind::Browser => Some(FocusTarget::ExplorerSurface(descriptor.id)),
        WorkspaceKind::PatternEditor { .. } => match descriptor.target {
            WorkspaceTarget::PatternDefinition { id } if id != 0 => {
                Some(FocusTarget::PatternSurface {
                    view: descriptor.id,
                    pattern: crate::sequencer::PatternId::from_raw(id),
                })
            }
            _ => None,
        },
        WorkspaceKind::Extension {
            ref namespace,
            ref name,
        } if namespace == "audec" && name == "sampler" => {
            Some(FocusTarget::SamplerSurface(descriptor.id))
        }
        _ => None,
    }
}

fn workspace_input_snapshot(
    document: &WorkspaceDocument,
    close_request: Option<CloseRequestId>,
) -> AccessibilitySnapshot {
    let mut roots = document
        .views
        .values()
        .filter(|descriptor| !matches!(document.location(descriptor.id), Ok(ViewLocation::Hidden)))
        .filter_map(|descriptor| {
            let target = workspace_focus_target(descriptor)?;
            let role = match descriptor.kind {
                WorkspaceKind::PatternEditor { .. } => SemanticRole::Grid,
                WorkspaceKind::Browser => SemanticRole::Tree,
                _ => SemanticRole::Region,
            };
            let mut node =
                SemanticNode::leaf(target, role, workspace_view_title(descriptor).to_string());
            node.tab_stop = true;
            Some(node)
        })
        .collect::<Vec<_>>();
    if let Some(request) = close_request {
        for (choice, label) in [
            (CloseChoice::Save, "Save"),
            (CloseChoice::Discard, "Discard"),
            (CloseChoice::Cancel, "Cancel"),
        ] {
            let target = FocusTarget::ClosePrompt { request, choice };
            let mut node = SemanticNode::leaf(target, SemanticRole::Button, label)
                .with_default_action(ProductAction::CloseChoice { request, choice });
            node.tab_stop = true;
            roots.push(node);
        }
    }
    AccessibilitySnapshot { roots }
}

pub struct DawWorkspace {
    workspace: Entity<DynamicWorkspaceRoot>,
    workbench: Entity<Workbench>,
    object_reveals: Arc<Mutex<Vec<PendingObjectReveal>>>,
    explorer_model: Option<ExplorerModel>,
    explorer_selection: ExplorerSelection,
    explorer_breadcrumb: Vec<String>,
    explorer_diagnostic: Option<String>,
    inspector_report: Option<InspectorReport>,
    explorer_scroll: ScrollHandle,
    product_inspector_scroll: ScrollHandle,
    close_guard: Arc<Mutex<CloseGuard>>,
    product_input: Arc<Mutex<ProductInputController>>,
    focus_handle: FocusHandle,
    /// Latest portable layout publication. File actions can persist this in
    /// the existing project envelope once they own save/open coordination.
    workspace_layout: Arc<Mutex<WorkspaceSessionLayout>>,
}

impl DawWorkspace {
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

    fn create_dynamic(&mut self, descriptor: NewWorkspaceView, cx: &mut Context<Self>) {
        if let Err(error) = self.workspace.update(cx, |workspace, cx| {
            workspace.create_view(descriptor, None, cx)
        }) {
            eprintln!("creating workspace item: {error:#}");
        }
    }

    fn request_project_replacement(
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

    fn perform_project_replacement(
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

    fn create_reading_query(&mut self, cx: &mut Context<Self>) {
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

    fn execute_workspace_semantic(
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

    fn execute_active_workspace_semantic(
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

    fn choose_recovery(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    fn enter_close_modal(&self, request: CloseRequestId) {
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

    fn leave_close_modal(&self) {
        let document = self.workspace_document();
        if let Ok(mut input) = self.product_input.lock() {
            let _ = input.leave_modal();
            let _ = input.replace_snapshot(workspace_input_snapshot(&document, None));
        }
    }

    fn request_application_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    fn recover_failed_close_guard(&mut self, cx: &App) {
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

    fn save(&mut self, save_as: bool, post_save: Option<PostSaveAction>, cx: &mut Context<Self>) {
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

    fn import_pending_workspace(&mut self, cx: &mut Context<Self>) {
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

    fn persist_reading_query_documents(&mut self, cx: &mut Context<Self>) {
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

    fn handle_object_reveals(&mut self, cx: &mut Context<Self>) {
        let pending = match self.object_reveals.lock() {
            Ok(mut reveals) => std::mem::take(&mut *reveals),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };
        for pending in pending {
            self.apply_object_reveal(pending, cx);
        }
    }

    fn refresh_product_shell(&mut self, cx: &mut Context<Self>) {
        let (project, selected_object) = {
            let workbench = self.workbench.read(cx);
            let session = workbench.session.read(cx);
            (
                session
                    .project_snapshot()
                    .ok()
                    .map(|snapshot| snapshot.project.clone()),
                session
                    .selection()
                    .selection
                    .objects
                    .inspector_target()
                    .cloned(),
            )
        };
        let Some(project) = project else {
            self.explorer_model = None;
            self.inspector_report = None;
            self.explorer_breadcrumb.clear();
            return;
        };
        let rebuild = !self
            .explorer_model
            .as_ref()
            .is_some_and(|model| model.revision() == project.revisions().aggregate);
        if rebuild {
            let model = ExplorerModel::build(ExplorerInput::project(project.as_ref()));
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

    fn set_explorer_mode(&mut self, mode: ExplorerMode, cx: &mut Context<Self>) {
        self.explorer_selection.mode = mode;
        self.explorer_diagnostic = None;
        cx.notify();
    }

    fn select_explorer_node(&mut self, id: ExplorerNodeId, cx: &mut Context<Self>) {
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

    fn reveal_explorer_selection(&mut self, cx: &mut Context<Self>) {
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

    fn reveal_inspector_object(&mut self, object: ObjectRef, cx: &mut Context<Self>) {
        self.queue_direct_reveal(
            crate::project_controller::RevealRequest::new(object, RevealIntent::ActivateExisting),
            "Opened from Inspector",
            cx,
        );
    }

    fn queue_direct_reveal(
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

    fn apply_object_reveal(&mut self, pending: PendingObjectReveal, cx: &mut Context<Self>) {
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

        let document = self.workspace_document();
        // The session resolver revalidated this request against `guard`; pin
        // the planner to that exact current revision rather than its original
        // publication when it selected a surviving object or predecessor.
        request.expected_project_revision = Some(guard.project_revision);
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
        let guard_is_current = {
            let workbench = self.workbench.read(cx);
            workbench.session.read(cx).reveal_guard_is_current(guard)
        };
        if !guard_is_current {
            self.explorer_diagnostic = Some(format!(
                "{} · project changed while its destination was being prepared",
                pending.headline
            ));
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
            WorkspaceReveal::None => Ok(None),
            WorkspaceReveal::Unsupported => {
                diagnostic.get_or_insert_with(|| {
                    "This object has no reachable workspace surface in this build.".into()
                });
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

    fn render_project_commands(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let workbench = self.workbench.read(cx);
        let project = workbench
            .package_root()
            .as_deref()
            .and_then(std::path::Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or("Untitled project")
            .to_owned();
        let dirty = workbench.is_project_dirty(cx);
        let status = workbench.project_io_status.label();

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
                viz_control("project-reading-query", "Reading query")
                    .on_click(cx.listener(|this, _, _, cx| this.create_reading_query(cx))),
            )
            .child(
                viz_control("project-arrangement", "Arrangement").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.create_dynamic(
                            default_view(WorkspaceKind::Arrangement, WorkspaceTarget::Arrangement),
                            cx,
                        )
                    },
                )),
            )
            .child(
                viz_control("project-pattern", "Pattern").on_click(cx.listener(
                    |this, _, _, cx| {
                        let pattern = this.workbench.read(cx).first_pattern_id(cx);
                        this.create_dynamic(
                            default_view(
                                WorkspaceKind::PatternEditor {
                                    mode: WorkspacePatternMode::Steps,
                                },
                                WorkspaceTarget::PatternDefinition { id: pattern },
                            ),
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

    fn render_product_explorer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selected = self.explorer_selection.selected.clone();
        let root = self.explorer_model.as_ref().map(|model| {
            model.filtered(
                self.explorer_selection.mode,
                &self.explorer_selection.filter,
            )
        });
        let selection_label = self
            .workbench
            .read(cx)
            .timeline_selection
            .filter(|range| !range.is_empty())
            .map_or_else(
                || "Select a source range to make material".to_owned(),
                |range| {
                    let workbench = self.workbench.read(cx);
                    format!(
                        "{} – {}",
                        format_time(workbench.seconds_for_sample(range.start.get().max(0) as u64)),
                        format_time(workbench.seconds_for_sample(range.end.get().max(0) as u64))
                    )
                },
            );
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
                                .child(section_label("MAKE FROM SELECTION"))
                                .child(
                                    div()
                                        .mt_1()
                                        .text_xs()
                                        .text_color(rgb(MUTED))
                                        .child(selection_label),
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
                                                            workbench.save_selection_as_one_shot(cx)
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
                                                            workbench.chop_selection_to_pads(cx)
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
                                                    workbench.make_beat_from_selection(cx)
                                                })
                                            }
                                        }),
                                )
                                .child(
                                    div()
                                        .mt_1()
                                        .text_xs()
                                        .text_color(rgb(DIM))
                                        .child("Sample/kit → Instrument · Beat → Pattern"),
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
                                            "No live deprojection candidate is published".to_owned()
                                        } else {
                                            format!("{} live candidate(s) ready", candidates.len())
                                        }),
                                )
                                .children(candidates.into_iter().map(|candidate| {
                                    let comparison = candidate.comparison;
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
                                        .text_color(rgb(TEXT))
                                        .cursor_pointer()
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.queue_direct_reveal(
                                                crate::project_controller::RevealRequest::new(
                                                    ObjectRef::Comparison(comparison),
                                                    RevealIntent::OpenNew,
                                                ),
                                                "Opened live deprojection candidate",
                                                cx,
                                            )
                                        }))
                                        .child(candidate.label)
                                }))
                        },
                    ),
            )
            .into_any_element()
    }

    fn render_product_inspector(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
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

fn render_explorer_node(
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

impl Render for DawWorkspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.import_pending_workspace(cx);
        self.persist_reading_query_documents(cx);
        self.handle_object_reveals(cx);
        self.refresh_product_shell(cx);
        self.recover_failed_close_guard(cx);
        div()
            .key_context("Audec")
            .track_focus(&self.focus_handle)
            .tab_group()
            .size_full()
            .flex()
            .flex_col()
            .on_action(cx.listener(|this, _: &QuitAudec, window, cx| {
                this.request_application_close(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewProject, window, cx| {
                this.request_project_replacement(ProjectReplacementIntent::NewProject, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenAudio, window, cx| {
                this.request_project_replacement(ProjectReplacementIntent::ChooseAudio, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenProject, window, cx| {
                this.request_project_replacement(
                    ProjectReplacementIntent::ChooseProject,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &SaveProject, _, cx| {
                this.save(false, None, cx);
            }))
            .on_action(cx.listener(|this, _: &SaveProjectAs, _, cx| {
                this.save(true, None, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenRecovery, window, cx| {
                this.request_project_replacement(
                    ProjectReplacementIntent::ChooseRecovery,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &ExportWav, _, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.export_wav(cx));
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
                this.create_dynamic(analysis_view(AnalysisLensKind::Waterfall), cx);
            }))
            .on_action(cx.listener(|this, _: &OpenRhythm, _, cx| {
                this.create_dynamic(analysis_view(AnalysisLensKind::Rhythm), cx);
            }))
            .on_action(cx.listener(|this, _: &OpenComponents, _, cx| {
                this.create_dynamic(analysis_view(AnalysisLensKind::Components), cx);
            }))
            .on_action(cx.listener(|this, _: &OpenSeparation, _, cx| {
                this.create_dynamic(analysis_view(AnalysisLensKind::Separation), cx);
            }))
            .on_action(cx.listener(|this, _: &OpenLoom, _, cx| {
                this.create_dynamic(analysis_view(AnalysisLensKind::Loom), cx);
            }))
            .on_action(cx.listener(|this, _: &OpenArrangementEditor, _, cx| {
                this.create_dynamic(
                    default_view(WorkspaceKind::Arrangement, WorkspaceTarget::Arrangement),
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenSequencerEditor, _, cx| {
                let pattern = this.workbench.read(cx).first_pattern_id(cx);
                this.create_dynamic(
                    default_view(
                        WorkspaceKind::PatternEditor {
                            mode: WorkspacePatternMode::Steps,
                        },
                        WorkspaceTarget::PatternDefinition { id: pattern },
                    ),
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenMixer, _, cx| {
                this.create_dynamic(
                    default_view(
                        WorkspaceKind::Mixer,
                        WorkspaceTarget::Mixer { bus_id: None },
                    ),
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenAutomation, _, cx| {
                let lane = this.workbench.read(cx).first_automation_lane_id(cx);
                this.create_dynamic(
                    default_view(
                        WorkspaceKind::AutomationEditor,
                        WorkspaceTarget::AutomationLane { id: lane },
                    ),
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenAssets, _, cx| {
                this.create_dynamic(
                    default_view(WorkspaceKind::Browser, WorkspaceTarget::Assets),
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenSampler, _, cx| {
                this.create_dynamic(
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
                );
            }))
            .on_action(cx.listener(|this, _: &OpenReadingQuery, _, cx| {
                this.create_reading_query(cx);
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
            .on_action(cx.listener(|this, _: &NextWorkspacePane, _, cx| {
                this.execute_workspace_semantic(
                    WorkspaceSemanticNodeId::Workspace,
                    WorkspaceSemanticAction::NextPane,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &PreviousWorkspacePane, _, cx| {
                this.execute_workspace_semantic(
                    WorkspaceSemanticNodeId::Workspace,
                    WorkspaceSemanticAction::PreviousPane,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &CloseWorkspacePane, _, cx| {
                this.execute_active_workspace_semantic(WorkspaceSemanticAction::Close, cx);
            }))
            .on_action(cx.listener(|this, _: &FloatOrDockWorkspacePane, _, cx| {
                this.execute_active_workspace_semantic(WorkspaceSemanticAction::FloatOrDock, cx);
            }))
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
    }
}

pub fn create_workspace(
    initial_path: Option<PathBuf>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<DawWorkspace> {
    let workbench = cx.new(|cx| Workbench::new(initial_path, cx));
    workbench.update(cx, |workbench, cx| {
        workbench.set_product_shell_hosted(true, cx)
    });

    let waterfall = cx.new(|cx| Visualizer::new(VizKind::Waterfall, workbench.clone(), cx));
    let rhythm = cx.new(|cx| Visualizer::new(VizKind::Rhythm, workbench.clone(), cx));
    let components = cx.new(|cx| Visualizer::new(VizKind::Components, workbench.clone(), cx));
    let separation = cx.new(|cx| Visualizer::new(VizKind::Separation, workbench.clone(), cx));
    let loom = cx.new(|cx| Visualizer::new(VizKind::Loom, workbench.clone(), cx));
    for (view, entity) in [
        (WorkspaceViewId::WATERFALL, waterfall.clone()),
        (WorkspaceViewId::RHYTHM, rhythm.clone()),
        (WorkspaceViewId::COMPONENTS, components.clone()),
        (WorkspaceViewId::SEPARATION, separation.clone()),
        (WorkspaceViewId::LOOM, loom.clone()),
    ] {
        entity.update(cx, |entity, _| entity.set_workspace_view_id(view));
    }

    let mut registry = PaneRegistry::new();
    registry
        .register_entity(
            BuiltinView::Track,
            "Arrangement + evidence",
            workbench.clone(),
        )
        .register_entity(
            BuiltinView::Waterfall,
            "Spectral waterfall",
            waterfall.clone(),
        )
        .register_entity(BuiltinView::Rhythm, "Rhythm deprojection", rhythm.clone())
        .register_entity(
            BuiltinView::Components,
            "Recurring components",
            components.clone(),
        )
        .register_entity(
            BuiltinView::Separation,
            "Harmonic / transient",
            separation.clone(),
        )
        .register_entity(BuiltinView::Loom, "Loom reconstruction", loom.clone());

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
    let session_id = workbench.read(cx).session.read(cx).id();
    let session_layout =
        WorkspaceSessionLayout::from_document(session_id, bootstrap.document().clone())
            .expect("the migrated workspace attaches to the project session");
    let workspace_layout = Arc::new(Mutex::new(session_layout.clone()));
    let factory_workbench = workbench.clone();
    let bootstrap = bootstrap
        .with_session_layout(session_layout)
        .expect("the dynamic workspace installs project-session layout authority")
        .with_factory(move |descriptor, cx| {
            factory_workbench
                .update(cx, |workbench, cx| {
                    workbench.create_workspace_pane(descriptor, cx)
                })
                .map_err(|error| SharedString::from(error.to_string()))
        });
    let legacy_runtimes = [
        (
            WorkspaceViewId::TRACK_OVERVIEW,
            WorkspacePaneRuntime::Overview,
        ),
        (
            WorkspaceViewId::WATERFALL,
            WorkspacePaneRuntime::Analysis(waterfall.downgrade()),
        ),
        (
            WorkspaceViewId::RHYTHM,
            WorkspacePaneRuntime::Analysis(rhythm.downgrade()),
        ),
        (
            WorkspaceViewId::COMPONENTS,
            WorkspacePaneRuntime::Analysis(components.downgrade()),
        ),
        (
            WorkspaceViewId::SEPARATION,
            WorkspacePaneRuntime::Analysis(separation.downgrade()),
        ),
        (
            WorkspaceViewId::LOOM,
            WorkspacePaneRuntime::Analysis(loom.downgrade()),
        ),
    ];
    for (view, runtime) in legacy_runtimes {
        if let Some(descriptor) = bootstrap.document().views.get(&view) {
            workbench
                .update(cx, |workbench, cx| {
                    workbench.register_workspace_runtime(descriptor, runtime, cx)
                })
                .expect("legacy workspace pane binds to the project session");
        }
    }
    let product_input = Arc::new(Mutex::new(ProductInputController::new(
        workspace_input_snapshot(bootstrap.document(), None),
    )));
    let published_layout = workspace_layout.clone();
    let snapshot_product_input = Arc::clone(&product_input);
    let snapshot_workbench = workbench.clone();
    let binding_workbench = workbench.clone();
    let event_workbench = workbench.clone();
    let event_layout = workspace_layout.clone();
    let event_product_input = Arc::clone(&product_input);
    let close_workbench = workbench.clone();
    let close_layout = workspace_layout.clone();
    let close_guard = Arc::new(Mutex::new(CloseGuard::default()));
    let snapshot_close_guard = Arc::clone(&close_guard);
    let native_close_guard = Arc::clone(&close_guard);
    let native_product_input = Arc::clone(&product_input);
    let hooks = DynamicWorkspaceHooks::default()
        .on_binding_effect(move |effect, cx| {
            binding_workbench.update(cx, |workbench, cx| {
                workbench.apply_workspace_binding_effect(effect, cx)
            })
        })
        .on_snapshot(move |document, cx| {
            let _ = snapshot_workbench.update(cx, |workbench, cx| {
                workbench.observe_workspace(document.clone());
                workbench.reconcile_workspace_pane_visibility(&document, cx)
            });
            if let Ok(mut input) = snapshot_product_input.lock() {
                let close_request = snapshot_close_guard
                    .lock()
                    .ok()
                    .and_then(|guard| match guard.state() {
                        CloseGuardState::Prompting { request, .. }
                        | CloseGuardState::Saving { request, .. } => Some(request),
                        CloseGuardState::Idle => None,
                    });
                let _ = input.replace_snapshot(workspace_input_snapshot(&document, close_request));
            }
            replace_workspace_layout_document(&published_layout, document, true);
        })
        .on_event(move |event, cx| match event {
            DynamicWorkspaceUiEvent::Activated(view) => {
                let _ = event_workbench.update(cx, |workbench, cx| {
                    workbench.activate_workspace_target(view, cx)
                });
                let target = event_layout.lock().ok().and_then(|layout| {
                    layout
                        .document()
                        .views
                        .get(&view)
                        .and_then(workspace_focus_target)
                });
                if let Some(target) = target {
                    if let Ok(mut input) = event_product_input.lock() {
                        let _ = input.focus(target);
                    }
                }
            }
            DynamicWorkspaceUiEvent::CloseDenied { view, message } => {
                eprintln!("workspace view {} remained open: {message}", view.0);
            }
            DynamicWorkspaceUiEvent::WindowOpenFailed { view, message } => {
                eprintln!("opening workspace view {}: {message}", view.0);
            }
            DynamicWorkspaceUiEvent::Removed(view) => {
                let _ = event_workbench.update(cx, |workbench, cx| {
                    workbench.unregister_workspace_pane(view, cx)
                });
            }
            _ => {}
        })
        .on_project_window_close(move |window, cx| {
            let dirty = close_workbench.read(cx).is_project_dirty(cx);
            let effect = native_close_guard
                .lock()
                .map(|mut guard| guard.request(CloseScope::Application, dirty))
                .unwrap_or(CloseGuardEffect::KeepOpen);
            match effect {
                CloseGuardEffect::CloseNow(CloseScope::Application) => {
                    cx.quit();
                    true
                }
                CloseGuardEffect::OpenPrompt { request, .. } => {
                    if let Ok(mut input) = native_product_input.lock() {
                        let document = workspace_document_from_layout(&close_layout);
                        let _ = input
                            .replace_snapshot(workspace_input_snapshot(&document, Some(request)));
                        let _ = input.enter_modal(
                            request,
                            FocusTarget::ClosePrompt {
                                request,
                                choice: CloseChoice::Cancel,
                            },
                        );
                    }
                    let prompt = window.prompt(
                        PromptLevel::Warning,
                        "Save changes before closing?",
                        Some("The project has edits newer than its last durable checkpoint."),
                        &[
                            PromptButton::ok("Save"),
                            PromptButton::new("Discard"),
                            PromptButton::cancel("Cancel"),
                        ],
                        cx,
                    );
                    let workbench = close_workbench.clone();
                    let layout = close_layout.clone();
                    let guard = Arc::clone(&native_close_guard);
                    let input = Arc::clone(&native_product_input);
                    cx.spawn(async move |cx| {
                        let choice = match prompt.await.unwrap_or(2) {
                            0 => CloseChoice::Save,
                            1 => CloseChoice::Discard,
                            _ => CloseChoice::Cancel,
                        };
                        if let Ok(mut input) = input.lock() {
                            let _ = input.leave_modal();
                            let document = workspace_document_from_layout(&layout);
                            let _ =
                                input.replace_snapshot(workspace_input_snapshot(&document, None));
                        }
                        let effect = guard
                            .lock()
                            .map(|mut guard| guard.choose(request, choice))
                            .unwrap_or(CloseGuardEffect::KeepOpen);
                        match effect {
                            CloseGuardEffect::SaveProject { request } => {
                                let workspace = workspace_document_from_layout(&layout);
                                let _ = workbench.update(cx, |workbench, cx| {
                                    workbench.observe_workspace(workspace.clone());
                                    if let Some(path) = workbench.package_root() {
                                        workbench.save_project(
                                            path,
                                            workspace,
                                            Some(PostSaveAction::Quit),
                                            cx,
                                        );
                                    } else {
                                        workbench.save_as(
                                            workspace,
                                            Some(PostSaveAction::Quit),
                                            cx,
                                        );
                                    }
                                });
                                if let Ok(mut guard) = guard.lock() {
                                    let _ = guard.save_finished(request, false);
                                }
                            }
                            CloseGuardEffect::CloseNow(CloseScope::Application) => {
                                let _ = cx.update(|cx| cx.quit());
                            }
                            CloseGuardEffect::KeepOpen
                            | CloseGuardEffect::OpenPrompt { .. }
                            | CloseGuardEffect::CloseNow(_) => {}
                        }
                    })
                    .detach();
                    false
                }
                CloseGuardEffect::KeepOpen
                | CloseGuardEffect::SaveProject { .. }
                | CloseGuardEffect::CloseNow(_) => false,
            }
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
    let object_reveals = Arc::clone(&workbench.read(cx).object_reveals);
    cx.new(|cx| DawWorkspace {
        workspace,
        workbench,
        object_reveals,
        explorer_model: None,
        explorer_selection: ExplorerSelection::default(),
        explorer_breadcrumb: Vec::new(),
        explorer_diagnostic: None,
        inspector_report: None,
        explorer_scroll: ScrollHandle::new(),
        product_inspector_scroll: ScrollHandle::new(),
        close_guard,
        product_input,
        focus_handle: cx.focus_handle().tab_stop(true),
        workspace_layout,
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
    fn interactive_sampling_bound_is_exact_and_rejects_whole_song_work() {
        assert!(within_interactive_sampling_limit(30 * 48_000, 48_000));
        assert!(!within_interactive_sampling_limit(30 * 48_000 + 1, 48_000));
        assert!(!within_interactive_sampling_limit(0, 0));
    }

    #[test]
    fn restored_navigation_descriptors_keep_exact_sample_targets() {
        let sampler = WorkspaceViewDescriptor {
            id: WorkspaceViewId(901),
            kind: WorkspaceKind::Extension {
                namespace: "audec".into(),
                name: "sampler".into(),
            },
            target: WorkspaceTarget::Extension {
                namespace: "audec".into(),
                key: "kit:42".into(),
            },
            title_override: None,
            links: WorkspaceLinkMembership::default(),
            state: WorkspaceViewState::Extension {
                data: serde_json::Value::Null,
            },
            extensions: BTreeMap::new(),
        };
        assert_eq!(
            sampler_target_from_descriptor(&sampler),
            Some(SamplerTarget::Kit(crate::sample_kit::KitId::from_raw(42)))
        );

        let browser = WorkspaceViewDescriptor {
            id: WorkspaceViewId(902),
            kind: WorkspaceKind::Browser,
            target: WorkspaceTarget::Assets,
            title_override: None,
            links: WorkspaceLinkMembership::default(),
            state: WorkspaceViewState::Browser {
                search: "break".into(),
                selected_asset_id: Some(17),
            },
            extensions: BTreeMap::new(),
        };
        let restored = browser_state_from_descriptor(&browser).unwrap();
        assert_eq!(restored.search, "break");
        assert_eq!(restored.selected, Some(crate::assets::AssetId(17)));
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
