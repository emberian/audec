use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    actions, canvas, div, img, point, prelude::*, px, quad, relative, rgb, rgba, App, Bounds,
    Context, Entity, FocusHandle, Focusable, Image, ImageFormat, IntoElement, KeyBinding,
    KeyDownEvent, Menu, MenuItem, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ObjectFit, Path, PathBuilder, PathPromptOptions, Pixels, PromptButton, PromptLevel, Render,
    ScrollHandle, ScrollWheelEvent, SharedString, SystemMenuType, Task, WeakEntity, Window,
    WindowHandle, WindowOptions,
};

use crate::air_query::workbench::{
    FactKindDto, QueryDocument, QueryDocumentId, QueryTermDto, WorkbenchPaneFactory,
};
use crate::analysis::{
    analyze_file_base, encode_spectrogram, encode_spectrogram_field, spectral_projection, Analysis,
    FeatureFrame, OnsetEvent, RhythmAnalysis, WaveformBin, MAX_FREQUENCY, MIN_FREQUENCY,
};
use crate::analysis_product_runtime::{
    AnalysisProduct, AnalysisProductCancellation, AnalysisProductOwner, AnalysisProductRuntime,
    HpssAnalysisProduct, LoomAnalysisProduct,
};
use crate::arrangement::{
    ArrangementEditor, AssetId as ArrangementAssetId, Frame as ArrangementFrame,
    FrameRange as ArrangementFrameRange, Selection as ArrangementSelection,
    SourceRange as ArrangementSourceRange, TrackKind,
};
use crate::arrangement_interaction::{SelectionIntent, SelectionMode};
use crate::arrangement_view::{
    ArrangementTimelineEvent, ArrangementView, ArrangementViewEvent, ArrangementViewport,
    ArrangementWaveformProvider, ArrangementWaveformSource,
};
use crate::artifact_catalog::comparison_hydration::ArtifactComparisonPayload;
use crate::artifact_catalog::{
    sha256_content, ArtifactCatalog, ArtifactDescriptor, ArtifactId, ArtifactKind, ContentDigest,
};
use crate::artifact_promotion_bridge::{
    plan_artifact_promotion_comparison, ArtifactPromotionBridgeError,
    ArtifactPromotionComparisonResult,
};
use crate::aspect::{Aspect, FrameSpan, SignalLayer};
use crate::asset_view::{AssetBrowserEvent, AssetBrowserState, AssetBrowserView};
use crate::assets::{
    AbsolutePath, AssetId, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
    AssetRegistry, ContentFingerprint, DecodedAudioMetadata, ProjectRelativePath, SampleFrames,
};
use crate::audio::{AudioFormat, FrameRange, ProjectAudio, ProjectFrame, TransportMode};
use crate::audio_host::{ProjectAudioBackendPreference, ProjectAudioOutputHost};
use crate::comparison_controller::{
    ComparisonChannel, ComparisonController, ComparisonSelectionRequest,
};
use crate::comparison_runtime::executor::{
    ComparisonProductCompletion, ComparisonProductExecutor, ComparisonProductExecutorError,
    ComparisonProductRecipe, ComparisonSemanticSnapshot,
};
use crate::content_store::FsContentStore;
use crate::control_views::control_actions::{
    ControlAction, ControlRenderStatus, MixerMeterSnapshot,
};
use crate::control_views::{AutomationView, MixerView};
use crate::daw_engine::DawEngineConfig;
use crate::daw_render::{PcmAsset, RenderCancellation};
use crate::decomposition::ComponentDecomposition;
use crate::explanation::RenderedExplanation;
use crate::explanation_workbench_view::{
    ExplanationWorkbenchEvent, WorkbenchActionId, WorkbenchOperation, WorkbenchRevealTarget,
};
use crate::explorer_model::{
    ExplorerInput, ExplorerMode, ExplorerModel, ExplorerNode, ExplorerNodeId, ExplorerSelection,
    ExplorerSemanticCollections, ExplorerTarget, InspectorModel, InspectorReport,
};
use crate::export::{NoopExportObserver, WavExportRequest};
use crate::file_actions::ProjectFileActions;
use crate::hpss::HpssSettings;
use crate::interpretation::{InterpretationCommand, InterpretationStore};
use crate::live_project::{LiveProject, LiveProjectSnapshot, SourceMaterialMetadata};
use crate::loom::{EventObservation, FitMetrics, SequenceSketch, TemplateBuildConfig};
use crate::media_resolver::{
    CanonicalPcmMediaDecoder, DecodedMaterial, MediaDecodeError, MediaDecoder, ProjectRateMaterial,
    RubatoSampleRateConverter, SymphoniaMediaDecoder,
};
use crate::ontology::{Producer, Provenance};
use crate::pane_audio::result_lifecycle::{
    AnalysisDurableCompletion, AnalysisDurableIntent, AnalysisPromotionTarget,
    AnalysisResultBindings, AnalysisResultKind, TemporaryAnalysisResult,
};
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
use crate::project_controller::{
    apply_arrangement_reveal_selection, execute_arrangement_event_revealed,
    execute_control_action_revealed, hydrate_pattern_editor, recommend_asset,
    recommend_sample_result, AdoptTempoIntent, ArrangementExecution, FindingKind, FindingScope,
    InstrumentRef, LoomConstructionIntent, ObjectNavigator, ObjectRef, PadRef,
    PatternAuditionAdoption, PatternAuditionRequest, PatternAuditionSessionAdapter,
    PatternAuditionSessionInputs, PatternAuditionStartRequest, PatternWorkflowDispatchReceipt,
    PatternWorkflowOutcome, PatternWorkflowRequest, RevealIntent, RevealRecommendation,
    RevealRequest, RhythmTempoEvidence, SampleActionOutcome, SelectionConsequence,
    TempoAdoptionOutcome, WorkbenchSampleIntent, WorkspaceReveal,
};
use crate::project_format::ProjectPackage;
use crate::project_repository::{JsonAirPayloadCodec, ProjectRepository};
use crate::project_selection::{
    ObjectSelection, ProjectSelection, SelectableId, SelectionProvenance, SelectionSource,
};
use crate::project_session::deprojection_workspace_bridge::{
    AnalysisEvidenceDocumentSummary, AnalysisEvidenceKind, DeprojectionCandidateDocumentSummary,
    DeprojectionCandidateFreshness, DeprojectionWorkspaceTarget, LiveDeprojectionAnalysis,
};
use crate::project_session::reading_query::{
    ProjectQueryResolverInputs, ProjectReadingQuerySession,
};
use crate::project_session::{
    ProjectAudioStatus, ProjectDocumentLifecycle, ProjectEventFilter, ProjectEventSubscription,
    ProjectLifecycleError, ProjectPublication, ProjectReplacementDisposition, ProjectSession,
    ProjectSessionEvent, ProjectSessionId, RevealDisposition, RevealReceipt,
};
use crate::project_store::ProjectStore;
use crate::reading_effect_bridge::{
    reading_audition_owner, ReadingAuditionPlan, ReadingComparisonAuditionPlan,
    ReadingEffectSnapshot, ReadingSourceAuditionPlan,
};
use crate::reading_query_view::{ReadingQueryView, ReadingQueryViewEffect, ReadingQueryViewInputs};
use crate::render_plan::{
    DeterminismGrade, ExactDigest, OutputTailPolicy, RenderFormat, RenderScope, RenderSpan,
    Tileability,
};
use crate::render_runtime::{AuditionMix, AuditionOwner, AuditionSubject};
use crate::render_tiles::TileProductCache;
use crate::reverse_surface::{
    EditAuthority, ReverseSurfaceBody, ReverseSurfaceStore, SurfaceActionIntent,
    SurfaceAuditionIntent, CONSEQUENCE_APPLY_CONSTRUCTION, CONSEQUENCE_KEEP_FINDING,
};
use crate::reverse_surface_adapter::{keep_reverse_finding, project_reverse_surface_documents};
use crate::reverse_surface_view::{
    ReverseAnalysisResultEvent, ReverseSurfaceViewEvent, ReverseSurfaceViewFactory,
};
use crate::rhythm::{
    AnalysisStatus as RhythmAnalysisStatus, RhythmConfig as RhythmDeprojectionConfig,
    RhythmDeprojection, SampleSpan, TempoRelation,
};
use crate::rhythm_explanation::ExplainBudget;
use crate::runtime_command_codec::DeterministicRuntimeCommandCodec;
use crate::sample_actions::{
    resolve_active_sample_span, MakeBeatIntent, MakeBeatResultFocus, MaterialPoolSnapshot,
    ResolvedSampleSpan, SampleAction, SampleActionError, SampleActionExecutionClass,
    SampleActionRequest, SampleActionResult, SampleAuditionIntent, SampleChopIntent,
    SampleDispatchReceipt, SampleFocusCallback, SampleInstrumentDestination, SampleKitDestination,
    SamplePublishedResult, SampleRequestId, SampleResultFocus, SampleSelection, SampleSpanOrigin,
    SampleViewOutcome, SampleWorkflowCommand, SampleWorkflowSpec, SamplerTarget,
};
use crate::sample_kit::{KitId, PadId};
use crate::sample_material::{canonical_pcm_identity, DecodedPcmView, SourceMaterialRef};
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
use crate::ui_actions::{
    ids as action_ids, ActionCategory, ActionContext, ActionDescriptor, ActionFlags, ActionId,
    ActionInvocation, ActionParameterValue, ActionParameters, ActionProjectionSnapshot,
    ActionRegistry, ActionRequest, ActionScope, ContextEpoch, EditActionIntent, FileActionIntent,
    InvocationModifiers, InvocationOrigin, KeyChord, PaneOpenIntent, ProductActionIntent,
    ProjectionEpoch, SampleActionIntent, TransportActionIntent, UserKeymap, WorkspaceActionIntent,
};
use crate::waveform_proxy::WaveformAssetKey;
use crate::workspace::accessibility::{WorkspaceSemanticAction, WorkspaceSemanticNodeId};
use crate::workspace::{BuiltinView, WorkspaceLayout, WorkspaceModel};
use crate::workspace_document::EditorViewState;
use crate::workspace_document::{
    AnalysisLensKind, BeatViewport as WorkspaceBeatViewport, EditorTarget as WorkspaceTarget,
    EditorViewState as WorkspaceViewState, FrameViewport as WorkspaceFrameViewport,
    LinkFacets as WorkspaceLinkFacets, LinkGroupId as WorkspaceLinkGroupId, NewWorkspaceView,
    PatternEditorMode as WorkspacePatternMode, ViewLinkMembership as WorkspaceLinkMembership,
    ViewLocation, WorkspaceDocument, WorkspaceItemKind as WorkspaceKind, WorkspaceViewDescriptor,
    WorkspaceViewId,
};
use crate::workspace_items::{
    AnalysisViewKind as ActionAnalysisViewKind, EditorTarget as ActionEditorTarget,
    PatternEditorMode as ActionPatternEditorMode, WorkspaceItemKind as ActionWorkspaceKind,
};
use crate::workspace_presenter::{
    resolve_specialized_presenter, ExplanationWorkbenchViewFactory, SpecializedWorkspacePresenter,
};
use crate::workspace_session_layout::{PaneBindingEffect, PaneInstanceId, WorkspaceSessionLayout};
use crate::workspace_ui::{
    DynamicWorkspaceBootstrap, DynamicWorkspaceHooks, DynamicWorkspaceRoot,
    DynamicWorkspaceUiEvent, PaneRegistration, PaneRegistry,
};

static NEXT_VISUALIZER_AUDITION_OWNER: AtomicU64 = AtomicU64::new(1);
static NEXT_CONTEXTUAL_SAMPLE_REQUEST: AtomicU64 = AtomicU64::new(1);
static NEXT_QUERY_DOCUMENT: AtomicU64 = AtomicU64::new(1);

mod helpers;
mod lens_common;
mod lens_components;
mod lens_hpss;
mod lens_loom;
mod lens_rhythm;
mod lens_waterfall;
mod plots;
mod shell_actions;
mod shell_explorer;
mod shell_project;
mod workbench_editors;
mod workbench_events;
mod workbench_lifecycle;
mod workbench_panes;
mod workbench_project_io;
mod workbench_publication;
mod workbench_reading;
mod workbench_render;
mod workbench_reverse;
mod workbench_sampling;
mod workbench_timeline;
mod workbench_transport;

use helpers::*;
use plots::*;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct WaveformRenderKey {
    slot: u8,
    generation: u64,
    start: u64,
    end: u64,
}

impl WaveformRenderKey {
    const fn samples(slot: u8, generation: u64, start: u64, end: u64) -> Self {
        Self {
            slot,
            generation,
            start,
            end,
        }
    }

    fn fractions(slot: u8, generation: u64, start: f64, end: f64) -> Self {
        Self::samples(slot, generation, start.to_bits(), end.to_bits())
    }
}

#[derive(Clone)]
struct CachedWaveformGeometry {
    bounds: Bounds<Pixels>,
    left: Option<Path<Pixels>>,
    right: Option<Path<Pixels>>,
}

#[derive(Default)]
struct WaveformGeometryCache {
    entries: BTreeMap<WaveformRenderKey, CachedWaveformGeometry>,
}

impl WaveformGeometryCache {
    fn paths(
        &mut self,
        key: WaveformRenderKey,
        waveform: &[WaveformBin],
        bounds: Bounds<Pixels>,
    ) -> (Option<Path<Pixels>>, Option<Path<Pixels>>) {
        if let Some(entry) = self.entries.get(&key) {
            if entry.bounds == bounds {
                return (entry.left.clone(), entry.right.clone());
            }
        }
        // A pane only needs its current geometry and a handful of comparison
        // signals. Bound the retained tessellations when users scrub through
        // many viewports instead of turning navigation into an image cache.
        if self.entries.len() >= 32 && !self.entries.contains_key(&key) {
            self.entries.clear();
        }
        let entry = CachedWaveformGeometry {
            bounds,
            left: waveform_envelope(waveform, bounds, true),
            right: waveform_envelope(waveform, bounds, false),
        };
        let paths = (entry.left.clone(), entry.right.clone());
        self.entries.insert(key, entry);
        paths
    }
}

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
        MakeSampleFromActiveSpan,
        SliceActiveSpanToKit,
        MakeBeatFromActiveSpan,
        NextWorkspacePane,
        PreviousWorkspacePane,
        CloseWorkspacePane,
        FloatOrDockWorkspacePane,
        OpenCommandPalette,
    ]
);

/// One epoch-bearing action crossing from a projected menu/palette/context
/// surface into the project window. The registry validates this again at the
/// authority boundary before any operation runs.
#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = audec, no_json)]
struct InvokeProjectedAction {
    request: ActionRequest,
}

/// GPUI 0.2.2 derives native menu enablement from whether an action has a
/// handler. Disabled registry entries use this intentionally unhandled action;
/// the label carries the same reason visible in the in-window palette.
#[derive(Clone, Debug, Default, PartialEq, gpui::Action)]
#[action(namespace = audec, no_json)]
struct UnavailableProjectedAction;

mod surface_ids {
    use super::ActionId;

    pub const FILE_NEW: ActionId = ActionId::new("audec.file.new");
    pub const FILE_OPEN_AUDIO: ActionId = ActionId::new("audec.file.open_audio");
    pub const FILE_SAVE_AS: ActionId = ActionId::new("audec.file.save_as");
    pub const FILE_RECOVERY: ActionId = ActionId::new("audec.file.recovery");
    pub const ANALYSIS_WATERFALL: ActionId = ActionId::new("audec.analysis.waterfall");
    pub const ANALYSIS_RHYTHM: ActionId = ActionId::new("audec.analysis.rhythm");
    pub const ANALYSIS_COMPONENTS: ActionId = ActionId::new("audec.analysis.components");
    pub const ANALYSIS_SEPARATION: ActionId = ActionId::new("audec.analysis.separation");
    pub const ANALYSIS_LOOM: ActionId = ActionId::new("audec.analysis.loom");
    pub const VIEW_ZOOM_IN: ActionId = ActionId::new("audec.view.zoom_in");
    pub const VIEW_ZOOM_OUT: ActionId = ActionId::new("audec.view.zoom_out");
    pub const VIEW_PAN_LEFT: ActionId = ActionId::new("audec.view.pan_left");
    pub const VIEW_PAN_RIGHT: ActionId = ActionId::new("audec.view.pan_right");
    pub const VIEW_FIT: ActionId = ActionId::new("audec.view.fit");
    pub const VIEW_FOLLOW: ActionId = ActionId::new("audec.view.follow");
    pub const LOOP_FROM_SELECTION: ActionId = ActionId::new("audec.loop.from_selection");
    pub const SAMPLE_MAKE: ActionId = ActionId::new("audec.sample.make");
    pub const SAMPLE_SLICE_KIT: ActionId = ActionId::new("audec.sample.slice_kit");
    pub const SAMPLE_MAKE_BEAT: ActionId = ActionId::new("audec.sample.make_beat");
    pub const EDITOR_ASSETS: ActionId = ActionId::new("audec.editor.assets");
    pub const EDITOR_SAMPLER: ActionId = ActionId::new("audec.editor.sampler");
    pub const EDITOR_READING_QUERY: ActionId = ActionId::new("audec.editor.reading_query");
    pub const WORKSPACE_NEXT: ActionId = ActionId::new("audec.workspace.next");
    pub const WORKSPACE_PREVIOUS: ActionId = ActionId::new("audec.workspace.previous");
    pub const WORKSPACE_CLOSE: ActionId = ActionId::new("audec.workspace.close");
    pub const WORKSPACE_FLOAT_DOCK: ActionId = ActionId::new("audec.workspace.float_dock");
}

fn audec_action_registry() -> ActionRegistry {
    const PROJECT: ActionFlags = ActionFlags::REQUIRES_PROJECT;
    const PROJECT_SELECTION: ActionFlags =
        ActionFlags::REQUIRES_PROJECT.union(ActionFlags::REQUIRES_SELECTION);
    const TEXT_SAFE: ActionFlags = ActionFlags::ALLOW_IN_TEXT_INPUT;
    let mut registry = ActionRegistry::audec_defaults();
    let descriptors = [
        surface_action(
            surface_ids::FILE_NEW,
            "New Project",
            ActionCategory::File,
            ActionScope::Application,
            &["cmd-n"],
            TEXT_SAFE,
        ),
        surface_action(
            surface_ids::FILE_OPEN_AUDIO,
            "Open Audio…",
            ActionCategory::File,
            ActionScope::Application,
            &["cmd-shift-o"],
            TEXT_SAFE,
        ),
        surface_action(
            surface_ids::FILE_SAVE_AS,
            "Save As…",
            ActionCategory::File,
            ActionScope::Project,
            &["cmd-shift-s"],
            PROJECT.union(TEXT_SAFE),
        ),
        surface_action(
            surface_ids::FILE_RECOVERY,
            "Open Recovery…",
            ActionCategory::File,
            ActionScope::Project,
            &["cmd-option-s"],
            PROJECT.union(TEXT_SAFE),
        ),
        surface_action(
            surface_ids::ANALYSIS_WATERFALL,
            "Spectral Waterfall",
            ActionCategory::Analysis,
            ActionScope::Workspace,
            &["cmd-1"],
            PROJECT,
        ),
        surface_action(
            surface_ids::ANALYSIS_RHYTHM,
            "Rhythm Deprojection",
            ActionCategory::Analysis,
            ActionScope::Workspace,
            &["cmd-2"],
            PROJECT,
        ),
        surface_action(
            surface_ids::ANALYSIS_COMPONENTS,
            "Recurring Components",
            ActionCategory::Analysis,
            ActionScope::Workspace,
            &["cmd-3"],
            PROJECT,
        ),
        surface_action(
            surface_ids::ANALYSIS_SEPARATION,
            "Harmonic / Transient",
            ActionCategory::Analysis,
            ActionScope::Workspace,
            &["cmd-4"],
            PROJECT,
        ),
        surface_action(
            surface_ids::ANALYSIS_LOOM,
            "Loom Reconstruction",
            ActionCategory::Analysis,
            ActionScope::Workspace,
            &["cmd-5"],
            PROJECT,
        ),
        surface_action(
            surface_ids::VIEW_ZOOM_IN,
            "Zoom In",
            ActionCategory::View,
            ActionScope::Workspace,
            &["="],
            PROJECT,
        ),
        surface_action(
            surface_ids::VIEW_ZOOM_OUT,
            "Zoom Out",
            ActionCategory::View,
            ActionScope::Workspace,
            &["-"],
            PROJECT,
        ),
        surface_action(
            surface_ids::VIEW_PAN_LEFT,
            "Pan Left",
            ActionCategory::View,
            ActionScope::Workspace,
            &["shift-left"],
            PROJECT,
        ),
        surface_action(
            surface_ids::VIEW_PAN_RIGHT,
            "Pan Right",
            ActionCategory::View,
            ActionScope::Workspace,
            &["shift-right"],
            PROJECT,
        ),
        surface_action(
            surface_ids::VIEW_FIT,
            "Fit Timeline",
            ActionCategory::View,
            ActionScope::Workspace,
            &["0"],
            PROJECT,
        ),
        surface_action(
            surface_ids::VIEW_FOLLOW,
            "Follow Playhead",
            ActionCategory::View,
            ActionScope::Workspace,
            &["f"],
            PROJECT,
        ),
        surface_action(
            surface_ids::LOOP_FROM_SELECTION,
            "Loop Selection",
            ActionCategory::Transport,
            ActionScope::Project,
            &["cmd-l"],
            PROJECT_SELECTION,
        ),
        surface_action(
            surface_ids::SAMPLE_MAKE,
            "Make Sample from Active Span",
            ActionCategory::Clip,
            ActionScope::Project,
            &["s"],
            PROJECT_SELECTION,
        ),
        surface_action(
            surface_ids::SAMPLE_SLICE_KIT,
            "Slice Active Span to Kit",
            ActionCategory::Clip,
            ActionScope::Project,
            &["shift-s"],
            PROJECT_SELECTION,
        ),
        surface_action(
            surface_ids::SAMPLE_MAKE_BEAT,
            "Make Beat from Active Span",
            ActionCategory::Pattern,
            ActionScope::Project,
            &["b"],
            PROJECT_SELECTION,
        ),
        surface_action(
            surface_ids::EDITOR_ASSETS,
            "Media Pool",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-b"],
            PROJECT,
        ),
        surface_action(
            surface_ids::EDITOR_SAMPLER,
            "Sampler",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-shift-b"],
            PROJECT,
        ),
        surface_action(
            surface_ids::EDITOR_READING_QUERY,
            "Reading Query",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-shift-r"],
            PROJECT,
        ),
        surface_action(
            surface_ids::WORKSPACE_NEXT,
            "Next Pane",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["ctrl-tab"],
            PROJECT,
        ),
        surface_action(
            surface_ids::WORKSPACE_PREVIOUS,
            "Previous Pane",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["ctrl-shift-tab"],
            PROJECT,
        ),
        surface_action(
            surface_ids::WORKSPACE_CLOSE,
            "Close Pane",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-shift-w"],
            PROJECT,
        ),
        surface_action(
            surface_ids::WORKSPACE_FLOAT_DOCK,
            "Float or Dock Pane",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-option-w"],
            PROJECT,
        ),
    ];
    for descriptor in descriptors {
        registry
            .register(descriptor)
            .expect("Audec application action IDs and shortcuts are valid");
    }
    registry
}

const fn surface_action(
    id: ActionId,
    label: &'static str,
    category: ActionCategory,
    scope: ActionScope,
    default_keys: &'static [&'static str],
    flags: ActionFlags,
) -> ActionDescriptor {
    ActionDescriptor {
        id,
        label,
        category,
        scope,
        default_keys,
        flags,
    }
}

fn audec_keymap() -> UserKeymap {
    let mut keymap = UserKeymap::default();
    // Preserve Audec's established analysis/editor number row while the
    // registry becomes the source used by every presentation surface.
    for (id, keys) in [
        (action_ids::FILE_EXPORT, &["cmd-e"][..]),
        (action_ids::EDITOR_ARRANGEMENT, &["cmd-6"][..]),
        (action_ids::EDITOR_PIANO_ROLL, &["cmd-7"][..]),
        (action_ids::EDITOR_DRUMS, &[][..]),
        (action_ids::EDITOR_MIXER, &["cmd-8"][..]),
        (action_ids::EDITOR_AUTOMATION, &["cmd-9"][..]),
    ] {
        keymap.set(
            id.as_str(),
            keys.iter()
                .map(|key| KeyChord::parse(key).expect("Audec keymap chord is valid"))
                .collect(),
        );
    }
    keymap
}

fn action_request_unchecked(
    snapshot: &ActionProjectionSnapshot,
    action: ActionId,
    origin: InvocationOrigin,
) -> ActionRequest {
    ActionRequest {
        invocation: ActionInvocation {
            action,
            origin,
            view: snapshot.active_view,
            target: snapshot.target.clone(),
            modifiers: InvocationModifiers::default(),
        },
        parameters: ActionParameters::default(),
        projected_at: snapshot.epoch,
    }
}

fn projected_menu_item(snapshot: &ActionProjectionSnapshot, action: ActionId) -> Option<MenuItem> {
    let projected = snapshot.get(action)?;
    let mut label = projected.descriptor.label.to_owned();
    if projected.state.checked {
        label = format!("✓ {label}");
    }
    if !projected.state.enabled {
        if let Some(reason) = projected.state.disabled_reason {
            label = format!("{label} — {reason}");
        }
        return Some(MenuItem::action(label, UnavailableProjectedAction));
    }
    Some(MenuItem::action(
        label,
        InvokeProjectedAction {
            request: action_request_unchecked(snapshot, action, InvocationOrigin::Menu),
        },
    ))
}

fn projected_items(snapshot: &ActionProjectionSnapshot, ids: &[Option<ActionId>]) -> Vec<MenuItem> {
    ids.iter()
        .filter_map(|id| match id {
            Some(id) => projected_menu_item(snapshot, *id),
            None => Some(MenuItem::separator()),
        })
        .collect()
}

fn projected_app_menus(snapshot: &ActionProjectionSnapshot) -> Vec<Menu> {
    vec![
        Menu {
            name: "audec".into(),
            disabled: false,
            items: vec![
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Quit audec", QuitAudec),
            ],
        },
        Menu {
            name: "File".into(),
            disabled: false,
            items: projected_items(
                snapshot,
                &[
                    Some(surface_ids::FILE_NEW),
                    None,
                    Some(action_ids::FILE_OPEN),
                    Some(surface_ids::FILE_OPEN_AUDIO),
                    None,
                    Some(action_ids::FILE_SAVE),
                    Some(surface_ids::FILE_SAVE_AS),
                    Some(surface_ids::FILE_RECOVERY),
                    None,
                    Some(action_ids::FILE_EXPORT),
                ],
            ),
        },
        Menu {
            name: "Edit".into(),
            disabled: false,
            items: projected_items(
                snapshot,
                &[
                    Some(action_ids::EDIT_UNDO),
                    Some(action_ids::EDIT_REDO),
                    None,
                    Some(action_ids::EDIT_DUPLICATE),
                    Some(action_ids::EDIT_DELETE),
                    Some(action_ids::CLIP_SPLIT),
                ],
            ),
        },
        Menu {
            name: "Transport".into(),
            disabled: false,
            items: projected_items(
                snapshot,
                &[
                    Some(action_ids::TRANSPORT_TOGGLE),
                    Some(action_ids::TRANSPORT_STOP),
                    None,
                    Some(surface_ids::LOOP_FROM_SELECTION),
                    Some(action_ids::LOOP_TOGGLE),
                ],
            ),
        },
        Menu {
            name: "Workspace".into(),
            disabled: false,
            items: projected_items(
                snapshot,
                &[
                    Some(action_ids::EDITOR_ARRANGEMENT),
                    Some(action_ids::EDITOR_PIANO_ROLL),
                    Some(action_ids::EDITOR_DRUMS),
                    Some(action_ids::EDITOR_MIXER),
                    Some(action_ids::EDITOR_AUTOMATION),
                    Some(surface_ids::EDITOR_ASSETS),
                    Some(surface_ids::EDITOR_SAMPLER),
                    Some(surface_ids::EDITOR_READING_QUERY),
                    None,
                    Some(surface_ids::WORKSPACE_NEXT),
                    Some(surface_ids::WORKSPACE_PREVIOUS),
                    Some(surface_ids::WORKSPACE_FLOAT_DOCK),
                    Some(surface_ids::WORKSPACE_CLOSE),
                    None,
                    Some(action_ids::PALETTE_OPEN),
                ],
            ),
        },
    ]
}

/// Startup menu projection. Once a project window renders it replaces this
/// with the epoch-bearing current projection.
pub fn app_menus() -> Vec<Menu> {
    let registry = audec_action_registry();
    let snapshot = registry.project(&ActionContext::default(), &audec_keymap());
    projected_app_menus(&snapshot)
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
    canonical: CanonicalPcmMediaDecoder,
    decoder: SymphoniaMediaDecoder,
    converter: RubatoSampleRateConverter,
    project_sample_rate_hz: u32,
    material: Mutex<BTreeMap<ContentFingerprint, ProjectRateMaterial>>,
}

impl ProjectRateHydrationDecoder {
    fn new(project_sample_rate_hz: u32) -> Self {
        Self {
            canonical: CanonicalPcmMediaDecoder::default(),
            decoder: SymphoniaMediaDecoder::default(),
            converter: RubatoSampleRateConverter::default(),
            project_sample_rate_hz,
            material: Mutex::new(BTreeMap::new()),
        }
    }
}

impl MediaDecoder for ProjectRateHydrationDecoder {
    fn decode(&self, path: &std::path::Path) -> Result<DecodedMaterial, MediaDecodeError> {
        if self.canonical.recognizes(path)? {
            return self.canonical.decode(path);
        }
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

fn sample_workflow_name_stem(source_name: &str) -> String {
    let source_name = source_name.trim();
    if source_name.is_empty() {
        return "Source".into();
    }
    // Keep every generated sample, instrument, and pattern name within the
    // workflow contract's 160-character product limit.
    source_name.chars().take(120).collect()
}

fn sample_workflow_instrument_name(command: SampleWorkflowCommand, source_name: &str) -> String {
    let stem = sample_workflow_name_stem(source_name);
    match command {
        SampleWorkflowCommand::MakeSample => format!("{stem} samples"),
        SampleWorkflowCommand::SliceToPads | SampleWorkflowCommand::MakeBeat => {
            format!("{stem} kit")
        }
    }
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

fn hpss_artifact_descriptor(
    mono: &[f32],
    source: &PaneSourcePin,
    settings: HpssSettings,
) -> Result<ArtifactDescriptor, String> {
    let mut pcm_bytes = Vec::with_capacity(mono.len().saturating_mul(4));
    for sample in mono {
        pcm_bytes.extend_from_slice(&sample.to_bits().to_le_bytes());
    }
    let source_digest = sha256_content(b"audec:decoded-mono:v1", &[&pcm_bytes]);
    let recipe_digest = sha256_content(
        b"audec:hpss-recipe:v1",
        &[
            env!("CARGO_PKG_VERSION").as_bytes(),
            format!("{settings:?}").as_bytes(),
        ],
    );
    let output_digest = sha256_content(
        b"audec:hpss-output:v1",
        &[&source_digest.bytes, &recipe_digest.bytes],
    );
    Ok(ArtifactDescriptor {
        id: ArtifactId(output_digest),
        kind: ArtifactKind::Hpss,
        source_digest,
        recipe_digest,
        output_digest,
        extent: FrameSpan::new(source.span.start, source.span.end)
            .ok_or_else(|| "HPSS artifact extent is empty".to_owned())?,
        sample_rate: source.source_format.sample_rate.get(),
        channels: 1,
        provenance: Provenance {
            producer: Producer::Analyzer {
                name: "audec HPSS".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                configuration_digest: Some(content_digest_hex(recipe_digest)),
            },
            created_unix_ms: None,
            source_revision: Some(source.revisions.aggregate.to_string()),
            note: Some(
                "phase-bearing sustained/transient evidence; no source identity asserted".into(),
            ),
        },
    })
}

fn loom_artifact_descriptor(
    mono: &[f32],
    source: &PaneSourcePin,
    config: TemplateBuildConfig,
) -> Result<ArtifactDescriptor, String> {
    let mut pcm_bytes = Vec::with_capacity(mono.len().saturating_mul(4));
    for sample in mono {
        pcm_bytes.extend_from_slice(&sample.to_bits().to_le_bytes());
    }
    let source_digest = sha256_content(b"audec:decoded-mono:v1", &[&pcm_bytes]);
    let recipe_digest = sha256_content(
        b"audec:loom-recipe:v1",
        &[
            env!("CARGO_PKG_VERSION").as_bytes(),
            format!("{config:?}").as_bytes(),
        ],
    );
    let extent = FrameSpan::new(source.span.start, source.span.end)
        .ok_or_else(|| "Loom artifact extent is empty".to_owned())?;
    let extent_start = extent.start.to_le_bytes();
    let extent_end = extent.end.to_le_bytes();
    let output_digest = sha256_content(
        b"audec:loom-output:v1",
        &[
            &source_digest.bytes,
            &recipe_digest.bytes,
            &extent_start,
            &extent_end,
        ],
    );
    Ok(ArtifactDescriptor {
        id: ArtifactId(output_digest),
        kind: ArtifactKind::LoomSketch,
        source_digest,
        recipe_digest,
        output_digest,
        extent,
        sample_rate: source.source_format.sample_rate.get(),
        channels: 1,
        provenance: Provenance {
            producer: Producer::Analyzer {
                name: "audec Loom".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                configuration_digest: Some(content_digest_hex(recipe_digest)),
            },
            created_unix_ms: None,
            source_revision: Some(source.revisions.aggregate.to_string()),
            note: Some(
                "phase-aware anonymous recurrence templates and editable event sequence".into(),
            ),
        },
    })
}

fn components_artifact_descriptor(
    mono: &[f32],
    source: &PaneSourcePin,
    decomposition: &ComponentDecomposition,
) -> Result<ArtifactDescriptor, String> {
    let mut pcm_bytes = Vec::with_capacity(mono.len().saturating_mul(4));
    for sample in mono {
        pcm_bytes.extend_from_slice(&sample.to_bits().to_le_bytes());
    }
    let source_digest = sha256_content(b"audec:decoded-mono:v1", &[&pcm_bytes]);
    let recipe_digest = sha256_content(
        b"audec:components-recipe:v1",
        &[
            env!("CARGO_PKG_VERSION").as_bytes(),
            &(decomposition.components.len() as u64).to_le_bytes(),
            &(decomposition.iterations_run as u64).to_le_bytes(),
            &decomposition.explained_energy.to_bits().to_le_bytes(),
            &decomposition.relative_error.to_bits().to_le_bytes(),
        ],
    );
    let output_digest = sha256_content(
        b"audec:components-output:v1",
        &[&source_digest.bytes, &recipe_digest.bytes],
    );
    Ok(ArtifactDescriptor {
        id: ArtifactId(output_digest),
        kind: ArtifactKind::Components,
        source_digest,
        recipe_digest,
        output_digest,
        extent: FrameSpan::new(source.span.start, source.span.end)
            .ok_or_else(|| "component artifact extent is empty".to_owned())?,
        sample_rate: source.source_format.sample_rate.get(),
        channels: 1,
        provenance: Provenance {
            producer: Producer::Analyzer {
                name: "audec components".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                configuration_digest: Some(content_digest_hex(recipe_digest)),
            },
            created_unix_ms: None,
            source_revision: Some(source.revisions.aggregate.to_string()),
            note: Some(
                "NMF magnitude factors; phase was not retained; not isolated sources or instrument labels".into(),
            ),
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
        KeyBinding::new("s", MakeSampleFromActiveSpan, Some("Audec")),
        KeyBinding::new("shift-s", SliceActiveSpanToKit, Some("Audec")),
        KeyBinding::new("b", MakeBeatFromActiveSpan, Some("Audec")),
        KeyBinding::new("ctrl-tab", NextWorkspacePane, Some("Audec")),
        KeyBinding::new("ctrl-shift-tab", PreviousWorkspacePane, Some("Audec")),
        KeyBinding::new("cmd-shift-w", CloseWorkspacePane, Some("Audec")),
        KeyBinding::new("cmd-alt-w", FloatOrDockWorkspacePane, Some("Audec")),
        KeyBinding::new("cmd-shift-p", OpenCommandPalette, Some("Audec")),
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

struct PendingExplanationWorkbenchEvent {
    source: WorkspaceViewId,
    event: ExplanationWorkbenchEvent,
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
    ExplanationWorkbench,
    Hosted(WeakEntity<WorkspacePaneHost>),
}

struct PendingArrangementEvent {
    source: Option<WorkspaceViewId>,
    event: ArrangementViewEvent,
}

struct PendingArrangementTimelineEvent {
    source: Option<WorkspaceViewId>,
    event: ArrangementTimelineEvent,
}

#[derive(Clone, Debug)]
struct AppliedReverseConstruction {
    artifact: ArtifactId,
    revision: u64,
    primary: ObjectRef,
    related: Vec<ObjectRef>,
}

#[derive(Clone, Debug)]
struct AnalysisPcmProduct {
    source: PaneSourcePin,
    sample_rate: u32,
    mono: Arc<[f32]>,
    label: String,
}

#[derive(Clone, Debug)]
struct LoomConstructionProduct {
    source: PaneSourcePin,
    sketch: SequenceSketch,
    label: String,
    finding: crate::project_controller::FindingRef,
    diverged_from_evidence: bool,
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
    arrangement_timeline_events: Arc<Mutex<Vec<PendingArrangementTimelineEvent>>>,
    sample_actions: Arc<Mutex<Vec<PendingSampleRequest>>>,
    sample_focuses: Arc<Mutex<Vec<PendingSampleFocus>>>,
    object_reveals: Arc<Mutex<Vec<PendingObjectReveal>>>,
    reverse_surface_events: Arc<Mutex<Vec<ReverseSurfaceViewEvent>>>,
    reverse_analysis_result_events: Arc<Mutex<Vec<ReverseAnalysisResultEvent>>>,
    analysis_pcm_products: BTreeMap<(ArtifactId, PaneAudioKind), AnalysisPcmProduct>,
    analysis_derived_pcm_products: BTreeMap<(ArtifactId, u64), AnalysisPcmProduct>,
    loom_construction_products: BTreeMap<ArtifactId, LoomConstructionProduct>,
    reverse_surface_store: Arc<Mutex<ReverseSurfaceStore>>,
    reverse_surface_factory: ReverseSurfaceViewFactory,
    reverse_promotion_waits: BTreeMap<WorkspaceViewId, Arc<ArtifactPromotionComparisonResult>>,
    explanation_workbench_events: Arc<Mutex<Vec<PendingExplanationWorkbenchEvent>>>,
    explanation_workbench_factory: ExplanationWorkbenchViewFactory,
    explanation_cancellations: BTreeMap<(WorkspaceViewId, WorkbenchActionId), RenderCancellation>,
    explanation_render_waits:
        BTreeMap<WorkspaceViewId, (WorkbenchActionId, Arc<ArtifactPromotionComparisonResult>)>,
    comparison_executor: ComparisonProductExecutor,
    control_actions: Arc<Mutex<Vec<PendingControlAction>>>,
    pattern_workflows: Arc<Mutex<Vec<PendingPatternWorkflow>>>,
    pattern_auditions: Arc<Mutex<Vec<PendingPatternAudition>>>,
    pattern_audition: PatternAuditionSessionAdapter,
    pattern_audition_owner: Option<AuditionOwner>,
    reading_query_effects: Rc<RefCell<Vec<PendingReadingQueryEffect>>>,
    reading_query_documents: BTreeMap<WorkspaceViewId, QueryDocument>,
    reading_audition_generations: BTreeMap<WorkspaceViewId, u64>,
    reading_comparison_controllers: BTreeMap<WorkspaceViewId, ComparisonController>,
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
    analysis_runtime: AnalysisProductRuntime,
    component_analysis_generation: u64,
    component_analysis_cancellation: Option<AnalysisProductCancellation>,
    component_analysis_pending: bool,
    save_generation: u64,
    autosave_last_attempt: Instant,
    autosave_in_flight: bool,
    pending_export_destination: Option<PathBuf>,
    pending_workspace_import: Option<WorkspaceDocument>,
    audition_audio: Option<ProjectAudio>,
    audio: Option<ProjectAudioOutputHost>,
    audio_controller: ProjectAudioController,
    render_tile_cache: Option<Arc<Mutex<TileProductCache>>>,
    preview_controller: PreviewController,
    pad_preview_tickets: BTreeMap<(WorkspaceViewId, KitId, PadId), SampleAuditionTicket>,
    audio_render_cancellation: Option<RenderCancellation>,
    audio_snapshot_digest: Option<ExactDigest>,
    audio_rendering: bool,
    audio_error: Option<String>,
    audio_device_status: Option<String>,
    constructive_status: Option<String>,
    primary_source_timeline_aligned: bool,
    playhead_seconds: f64,
    timeline_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    timeline_waveform_geometry: Arc<Mutex<WaveformGeometryCache>>,
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

fn open_application_tile_cache() -> Result<TileProductCache, String> {
    let root = dirs::cache_dir()
        .ok_or_else(|| "the operating system did not provide a cache directory".to_string())?
        .join("software.ember.audec")
        .join("render-products");
    TileProductCache::open(
        FsContentStore::new(root),
        format!("audec-ui-{}", std::process::id()),
    )
    .map_err(|error| error.to_string())
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
    product: Arc<HpssAnalysisProduct>,
    findings: Arc<[AnalysisEvidenceDocumentSummary]>,
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
    candidates: Arc<[DeprojectionCandidateDocumentSummary]>,
}

impl std::ops::Deref for RhythmViewResult {
    type Target = RhythmDeprojection;

    fn deref(&self) -> &Self::Target {
        &self.deprojection
    }
}

struct LoomViewResult {
    source: PaneSourcePin,
    /// Immutable source receipt of the published Finding. Viewport panning may
    /// change `source`, but promotion remains bounded to this artifact extent.
    artifact_source: PaneSourcePin,
    template_source: PaneSourcePin,
    sketch: SequenceSketch,
    selected_cluster: usize,
    start_sample: usize,
    end_sample: usize,
    start_seconds: f64,
    end_seconds: f64,
    sample_rate: u32,
    original: Arc<[f32]>,
    reconstruction: Arc<[f32]>,
    residual: Arc<[f32]>,
    original_waveform: Arc<[WaveformBin]>,
    reconstruction_waveform: Arc<[WaveformBin]>,
    residual_waveform: Arc<[WaveformBin]>,
    fit: FitMetrics,
    findings: Arc<[AnalysisEvidenceDocumentSummary]>,
    diverged_from_evidence: bool,
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
    waveform_geometry: Arc<Mutex<WaveformGeometryCache>>,
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
    hpss_cancellation: Option<AnalysisProductCancellation>,
    rhythm_state: RhythmViewState,
    rhythm_generation: u64,
    rhythm_cancellation: Option<AnalysisProductCancellation>,
    loom_state: LoomViewState,
    loom_generation: u64,
    loom_cancellation: Option<AnalysisProductCancellation>,
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActionContextSignature {
    document_generation: u64,
    project_generation: u64,
    selection_revision: u64,
    workspace_revision: Option<u64>,
    has_project: bool,
    has_selection: bool,
    active_view: Option<WorkspaceViewId>,
    active_kind: Option<ActionWorkspaceKind>,
    target: Option<ActionEditorTarget>,
    modal_active: bool,
    can_undo: bool,
    can_redo: bool,
    loop_enabled: bool,
    transport_playing: bool,
}

#[derive(Clone)]
struct CommandPaletteState {
    open: bool,
    query: String,
    selected: usize,
    snapshot: ActionProjectionSnapshot,
}

#[derive(Clone)]
struct PaneContextMenuState {
    view: WorkspaceViewId,
    position: gpui::Point<Pixels>,
    snapshot: ActionProjectionSnapshot,
}

fn action_workspace_kind(kind: &WorkspaceKind) -> Option<ActionWorkspaceKind> {
    Some(match kind {
        WorkspaceKind::Overview => ActionWorkspaceKind::Overview,
        WorkspaceKind::Arrangement => ActionWorkspaceKind::Arrangement,
        WorkspaceKind::Browser => ActionWorkspaceKind::Browser,
        WorkspaceKind::Inspector => ActionWorkspaceKind::Inspector,
        WorkspaceKind::PatternEditor { .. } => ActionWorkspaceKind::PatternEditor,
        WorkspaceKind::AutomationEditor => ActionWorkspaceKind::AutomationEditor,
        WorkspaceKind::Mixer => ActionWorkspaceKind::Mixer,
        WorkspaceKind::AnalysisLens { lens } => ActionWorkspaceKind::Analysis(match lens {
            AnalysisLensKind::Waveform => ActionAnalysisViewKind::Waveform,
            AnalysisLensKind::Spectrum => ActionAnalysisViewKind::Spectrum,
            AnalysisLensKind::Waterfall => ActionAnalysisViewKind::Waterfall,
            AnalysisLensKind::Rhythm => ActionAnalysisViewKind::Rhythm,
            AnalysisLensKind::Components => ActionAnalysisViewKind::Components,
            AnalysisLensKind::Separation => ActionAnalysisViewKind::Separation,
            AnalysisLensKind::Loom => ActionAnalysisViewKind::Loom,
            AnalysisLensKind::Coverage => ActionAnalysisViewKind::Coverage,
            AnalysisLensKind::Comparison => ActionAnalysisViewKind::Comparison,
            AnalysisLensKind::AirQuery => ActionAnalysisViewKind::AirQuery,
        }),
        WorkspaceKind::Extension { namespace, name }
            if namespace == "audec" && name == "sampler" =>
        {
            ActionWorkspaceKind::SamplerEditor
        }
        WorkspaceKind::Render | WorkspaceKind::Extension { .. } => return None,
    })
}

fn action_editor_target(descriptor: &WorkspaceViewDescriptor) -> ActionEditorTarget {
    match &descriptor.target {
        WorkspaceTarget::Project => ActionEditorTarget::Project,
        WorkspaceTarget::Arrangement => ActionEditorTarget::Arrangement,
        WorkspaceTarget::Assets => ActionEditorTarget::Assets,
        WorkspaceTarget::Inspector => ActionEditorTarget::Inspector,
        WorkspaceTarget::PatternDefinition { id } => ActionEditorTarget::Pattern {
            definition: crate::sequencer::PatternId::from_raw(*id),
            mode: match descriptor.kind {
                WorkspaceKind::PatternEditor {
                    mode: WorkspacePatternMode::PianoRoll,
                } => ActionPatternEditorMode::PianoRoll,
                _ => ActionPatternEditorMode::Steps,
            },
        },
        WorkspaceTarget::AutomationLane { id } => {
            ActionEditorTarget::AutomationLane(crate::automation::AutomationLaneId::from_raw(*id))
        }
        WorkspaceTarget::Mixer { bus_id } => ActionEditorTarget::Mixer {
            bus: bus_id.map(crate::mixer::BusId::from_raw),
        },
        WorkspaceTarget::Analysis { source_id } => ActionEditorTarget::Analysis {
            source: source_id.map(crate::ontology::SourceId::new),
            kind: match &descriptor.kind {
                WorkspaceKind::AnalysisLens { lens } => match lens {
                    AnalysisLensKind::Waveform => ActionAnalysisViewKind::Waveform,
                    AnalysisLensKind::Spectrum => ActionAnalysisViewKind::Spectrum,
                    AnalysisLensKind::Waterfall => ActionAnalysisViewKind::Waterfall,
                    AnalysisLensKind::Rhythm => ActionAnalysisViewKind::Rhythm,
                    AnalysisLensKind::Components => ActionAnalysisViewKind::Components,
                    AnalysisLensKind::Separation => ActionAnalysisViewKind::Separation,
                    AnalysisLensKind::Loom => ActionAnalysisViewKind::Loom,
                    AnalysisLensKind::Coverage => ActionAnalysisViewKind::Coverage,
                    AnalysisLensKind::Comparison => ActionAnalysisViewKind::Comparison,
                    AnalysisLensKind::AirQuery => ActionAnalysisViewKind::AirQuery,
                },
                _ => ActionAnalysisViewKind::Waveform,
            },
        },
        WorkspaceTarget::Explanation { proposal_id } => ActionEditorTarget::Explanation(
            crate::reconstruction::ReconstructionProposalId::from_raw(*proposal_id),
        ),
        // Render-comparison and extension targets do not yet have lossless
        // equivalents in the older action target vocabulary. The stable view
        // ID still carries the exact context; never manufacture an identity.
        WorkspaceTarget::Render { .. } | WorkspaceTarget::Extension { .. } => {
            ActionEditorTarget::Project
        }
    }
}

pub struct DawWorkspace {
    workspace: Entity<DynamicWorkspaceRoot>,
    workbench: Entity<Workbench>,
    object_reveals: Arc<Mutex<Vec<PendingObjectReveal>>>,
    explorer_model: Option<ExplorerModel>,
    explorer_semantic: Option<ExplorerSemanticCollections>,
    explorer_selection: ExplorerSelection,
    explorer_breadcrumb: Vec<String>,
    explorer_diagnostic: Option<String>,
    inspector_report: Option<InspectorReport>,
    explorer_scroll: ScrollHandle,
    product_inspector_scroll: ScrollHandle,
    command_palette_scroll: ScrollHandle,
    close_guard: Arc<Mutex<CloseGuard>>,
    product_input: Arc<Mutex<ProductInputController>>,
    focus_handle: FocusHandle,
    action_registry: ActionRegistry,
    action_keymap: UserKeymap,
    action_context_epoch: ContextEpoch,
    action_context_signature: Option<ActionContextSignature>,
    action_projection: ActionProjectionSnapshot,
    native_menu_epoch: Option<ProjectionEpoch>,
    command_palette: CommandPaletteState,
    pane_context_menu: Option<PaneContextMenuState>,
    pending_pane_context_menus: Rc<RefCell<Vec<(WorkspaceViewId, gpui::Point<Pixels>)>>>,
    /// Latest portable layout publication. File actions can persist this in
    /// the existing project envelope once they own save/open coordination.
    workspace_layout: Arc<Mutex<WorkspaceSessionLayout>>,
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
    let pending_pane_context_menus = Rc::new(RefCell::new(Vec::new()));
    let event_pane_context_menus = Rc::clone(&pending_pane_context_menus);
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
            DynamicWorkspaceUiEvent::ContextMenuRequested { view, position } => {
                event_pane_context_menus.borrow_mut().push((view, position));
                cx.refresh_windows();
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
    window.focus(&workbench.focus_handle(cx), cx);
    let object_reveals = Arc::clone(&workbench.read(cx).object_reveals);
    let action_registry = audec_action_registry();
    let action_keymap = audec_keymap();
    let action_projection = action_registry.project(&ActionContext::default(), &action_keymap);
    let command_palette = CommandPaletteState {
        open: false,
        query: String::new(),
        selected: 0,
        snapshot: action_projection.clone(),
    };
    cx.new(|cx| DawWorkspace {
        workspace,
        workbench,
        object_reveals,
        explorer_model: None,
        explorer_semantic: None,
        explorer_selection: ExplorerSelection::default(),
        explorer_breadcrumb: Vec::new(),
        explorer_diagnostic: None,
        inspector_report: None,
        explorer_scroll: ScrollHandle::new(),
        product_inspector_scroll: ScrollHandle::new(),
        command_palette_scroll: ScrollHandle::new(),
        close_guard,
        product_input,
        focus_handle: cx.focus_handle().tab_stop(true),
        action_registry,
        action_keymap,
        action_context_epoch: ContextEpoch::default(),
        action_context_signature: None,
        action_projection,
        native_menu_epoch: None,
        command_palette,
        pane_context_menu: None,
        pending_pane_context_menus,
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
            window.focus(&entity.focus_handle(cx), cx);
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
    fn visible_selection_overrides_a_different_active_loop_for_sampling() {
        let selection = SampleRange::new(Sample::new(100), Sample::new(200));
        let loop_range = SampleRange::new(Sample::new(400), Sample::new(800));
        assert_eq!(
            resolve_active_sample_span(Some(selection), true, Some(loop_range))
                .unwrap()
                .primary,
            crate::sample_actions::SampleSpanCandidate {
                range: selection,
                origin: SampleSpanOrigin::Selection,
            }
        );
        assert_eq!(
            resolve_active_sample_span(Some(selection), false, Some(loop_range))
                .unwrap()
                .primary
                .origin,
            SampleSpanOrigin::Selection
        );
        assert_eq!(
            resolve_active_sample_span(
                Some(selection),
                true,
                Some(SampleRange::empty(Sample::new(10))),
            )
            .unwrap()
            .primary
            .range,
            selection
        );
    }

    #[test]
    fn visible_sample_destinations_are_source_named_and_bounded() {
        assert_eq!(
            sample_workflow_instrument_name(SampleWorkflowCommand::MakeSample, "Amen break"),
            "Amen break samples"
        );
        assert_eq!(
            sample_workflow_instrument_name(SampleWorkflowCommand::SliceToPads, "Amen break"),
            "Amen break kit"
        );
        assert!(
            sample_workflow_instrument_name(SampleWorkflowCommand::MakeBeat, &"x".repeat(300))
                .chars()
                .count()
                <= 160
        );
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

    #[test]
    fn application_action_catalog_exposes_file_editor_and_reverse_workflows() {
        let registry = audec_action_registry();
        for id in [
            action_ids::FILE_OPEN,
            action_ids::FILE_SAVE,
            action_ids::FILE_EXPORT,
            action_ids::EDITOR_ARRANGEMENT,
            action_ids::EDITOR_PIANO_ROLL,
            surface_ids::ANALYSIS_RHYTHM,
            surface_ids::SAMPLE_MAKE_BEAT,
            surface_ids::WORKSPACE_FLOAT_DOCK,
        ] {
            assert!(registry.get(id).is_some(), "missing {}", id.as_str());
        }

        let snapshot = registry.project(&ActionContext::default(), &audec_keymap());
        let save = snapshot.get(action_ids::FILE_SAVE).unwrap();
        assert!(!save.state.enabled);
        assert_eq!(save.state.disabled_reason, Some("No project is open"));
        assert!(snapshot
            .palette("export audio")
            .iter()
            .any(|item| item.action == action_ids::FILE_EXPORT));
    }

    #[test]
    fn established_editor_shortcuts_are_projected_from_the_keymap() {
        let registry = audec_action_registry();
        let mut context = ActionContext {
            has_project: true,
            ..ActionContext::default()
        };
        context.epoch = ContextEpoch(8);
        let snapshot = registry.project(&context, &audec_keymap());
        let arrangement = snapshot.get(action_ids::EDITOR_ARRANGEMENT).unwrap();
        let mixer = snapshot.get(action_ids::EDITOR_MIXER).unwrap();
        assert_eq!(arrangement.bindings[0].chord.to_string(), "cmd-6");
        assert_eq!(mixer.bindings[0].chord.to_string(), "cmd-8");
        assert!(snapshot
            .get(action_ids::EDITOR_DRUMS)
            .unwrap()
            .bindings
            .is_empty());
    }

    #[test]
    fn project_audio_identity_changes_when_project_tempo_changes() {
        let project = crate::daw_project::DawProject::new("Tempo render", 48_000, 120.0).unwrap();
        let live = LiveProject::from_project(project, BTreeMap::new()).unwrap();
        let mut session = ProjectSession::new(ProjectSessionId(37)).unwrap();
        session.install(live, None).unwrap();

        let publication = |session: &ProjectSession| {
            let snapshot = session.project_snapshot().unwrap().clone();
            ProjectPublication {
                generation: session.snapshot().generation,
                revisions: snapshot.revisions(),
                snapshot,
                change_set: None,
            }
        };
        let before =
            project_audio_snapshot_digest(publication(&session).snapshot.project.as_ref()).unwrap();

        session
            .adopt_project_tempo(AdoptTempoIntent {
                expected_project_revision: session
                    .project_snapshot()
                    .unwrap()
                    .revisions()
                    .aggregate,
                bpm: 137.0,
                source: None,
            })
            .unwrap();
        let after =
            project_audio_snapshot_digest(publication(&session).snapshot.project.as_ref()).unwrap();

        assert_ne!(before, after, "tempo edits must invalidate audition audio");
    }

    #[test]
    fn pane_context_request_keeps_target_parameters_and_rejects_stale_epoch() {
        let registry = audec_action_registry();
        let view = WorkspaceViewId(91);
        let mut context = ActionContext {
            epoch: ContextEpoch(12),
            has_project: true,
            active_view: Some(view),
            active_kind: Some(ActionWorkspaceKind::Arrangement),
            target: Some(ActionEditorTarget::Arrangement),
            ..ActionContext::default()
        };
        let snapshot = registry.project(&context, &audec_keymap());
        let mut parameters = ActionParameters::default();
        parameters.insert("view_id", ActionParameterValue::Unsigned(view.0));
        let request = snapshot
            .request(
                surface_ids::WORKSPACE_CLOSE,
                InvocationOrigin::ContextMenu,
                InvocationModifiers::default(),
                parameters,
            )
            .unwrap();
        assert_eq!(request.invocation.view, Some(view));
        assert_eq!(
            request.parameters.get("view_id"),
            Some(&ActionParameterValue::Unsigned(91))
        );

        context.epoch = ContextEpoch(13);
        assert!(matches!(
            registry.validate_request(&request, &context),
            Err(crate::ui_actions::ActionDispatchError::StaleContext { .. })
        ));
    }
}
