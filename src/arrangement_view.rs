//! A tactile GPUI arrangement surface over audec's sample-accurate core.
//!
//! The view owns geometry, pointer capture, selection, and optimistic previews.
//! It refuses to publish aggregate project truth: direct manipulation emits one
//! revision-guarded semantic commit through [`ArrangementViewCallback`]. A host
//! resolves that term through the command envelope and supplies the next
//! snapshot. Waveforms are read-only, visible-range source proxies and never
//! claim to be post-DSP renders.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use gpui::{
    actions, canvas, div, point, prelude::*, px, relative, rgb, rgba, App, Bounds, Context,
    FocusHandle, Focusable, IntoElement, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PathBuilder, Pixels, Render, ScrollWheelEvent, Subscription, Window,
};

use crate::arrangement::{
    ArrangementEditor, ArrangementState, AssetId, Clip, ClipContent, ClipFades, ClipId, Frame,
    FrameRange, ParameterId, PatternId, Selection, Track, TrackId, TrackKind,
};
use crate::arrangement_interaction::keyboard::{
    plan_duplicate_after, plan_move_to_adjacent_tracks, plan_nudge, plan_phrase_split,
    plan_phrase_trim, plan_selection_navigation, PhraseEditPlan, SelectionNavigation,
    TrackDirection,
};
use crate::arrangement_interaction::surface::{
    plan_musical_grid, ArrangementGestureIdentity, MusicalGridResolution, TimelineSelectionEdit,
    DEFAULT_GRID_LINE_LIMIT,
};
use crate::arrangement_interaction::{
    hit_test_clip, hit_test_track, ArrangementEdit, ArrangementEditIntent, ArrangementInteraction,
    CanvasPoint, CanvasRect, ClipInteractionLayout, GestureCommit, GestureConfig, GesturePhase,
    GestureResponse, MarqueePreview, PointerModifiers, PreviewChange, PreviewPatch,
    SelectionIntent, SelectionMode, SnapContext, SnapGuide, SnapGuideKind, TimelinePointer,
    TrackInteractionLayout, TrimEdge,
};
use crate::pyramid::{WaveformPyramid, WaveformQuery};
use crate::sequencer::{BeatTime, Tempo, TempoMap, TimeSignature};
use crate::ui_drag::{
    interpret_drop, AssetDrag, DragModifiers, DragPayload, DropIntent, DropTarget,
};
use crate::waveform_proxy::{
    plan_clip_waveform, ClipWaveformSpec, PixelTarget, WaveformAssetKey, WaveformProxyKey,
    WaveformProxyPlan,
};

actions!(
    audec_arrangement,
    [
        UndoArrangement,
        RedoArrangement,
        DuplicateClip,
        DeleteClip,
        SplitClip,
        SelectAllArrangementClips,
        SelectPreviousArrangementClip,
        SelectNextArrangementClip,
        NudgeClipLeft,
        NudgeClipRight,
        NudgeClipFineLeft,
        NudgeClipFineRight,
        MoveClipTrackUp,
        MoveClipTrackDown,
        TrimClipStart,
        TrimClipEnd,
        ToggleArrangementLoop,
        ZoomArrangementIn,
        ZoomArrangementOut,
        PanArrangementLeft,
        PanArrangementRight,
        FitArrangement,
        CycleArrangementSnap,
        CancelArrangementGesture,
    ]
);

const BACKGROUND: u32 = 0x090b10;
const PANEL: u32 = 0x10141d;
const PANEL_ALT: u32 = 0x0d1118;
const RAISED: u32 = 0x171d28;
const BORDER: u32 = 0x252c38;
const TEXT: u32 = 0xe8edf5;
const MUTED: u32 = 0x8c98a9;
const DIM: u32 = 0x596579;
const CYAN: u32 = 0x50d8d7;
const MAGENTA: u32 = 0xf172b6;
const AMBER: u32 = 0xf6b760;
const LIME: u32 = 0xa7d877;
const TRACK_GUTTER: f32 = 190.0;
const TRACK_HEIGHT: f32 = 72.0;
const RULER_HEIGHT: f32 = 42.0;
const RULER_LOOP_STRIP_HEIGHT: f32 = 10.0;
const RULER_LOOP_HANDLE_RADIUS: f32 = 7.0;
const RULER_DRAG_THRESHOLD: f32 = 3.0;
const EDGE_SCROLL_ZONE: f64 = 28.0;
const EDGE_SCROLL_MAX_FRACTION: f64 = 0.065;

/// A project-owned arrangement editor that can be used by multiple views or
/// controllers. `ArrangementView` takes short snapshots from this handle and
/// never retains its mutex while constructing GPUI elements. New aggregate
/// integrations should also attach a callback with `from_shared_sources`;
/// direct manipulation never writes this handle itself.
pub type SharedArrangementEditor = Arc<Mutex<ArrangementEditor>>;

/// Semantic messages emitted at the view/controller boundary.
#[derive(Clone, Debug, PartialEq)]
pub enum ArrangementViewEvent {
    /// One pointer gesture, including its ephemeral selection consequence.
    Commit(GestureCommit),
    /// Ruler/canvas transport positioning remains a controller concern.
    SeekRequested(Frame),
    Action(ArrangementActionIntent),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArrangementActionIntent {
    pub expected_revision: u64,
    pub action: ArrangementAction,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ArrangementAction {
    Undo,
    Redo,
    DeleteClips(BTreeSet<ClipId>),
    SplitClip { clip: ClipId, at: Frame },
    CreateTrack { kind: TrackKind },
    Drop(DropIntent),
}

pub type ArrangementViewCallback = Arc<dyn Fn(ArrangementViewEvent) + Send + Sync + 'static>;

/// Ephemeral timeline state emitted separately from durable arrangement edits.
/// Hosts connect this to the shared selection/transport controllers; no loop
/// or selection range is smuggled into the command journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrangementTimelineEvent {
    TimeSelectionChanged(Option<FrameRange>),
    LoopChanged(Option<FrameRange>),
}

pub type ArrangementTimelineCallback =
    Arc<dyn Fn(ArrangementTimelineEvent) + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrangementGestureBoundary {
    Begin(ArrangementGestureIdentity),
    End {
        gesture: ArrangementGestureIdentity,
        cancelled: bool,
    },
}

pub type ArrangementGestureCallback =
    Arc<dyn Fn(ArrangementGestureBoundary) + Send + Sync + 'static>;

/// A controller-resolved immutable source for clip waveform navigation.
///
/// `key.asset` is the media-pool identity; the provider is responsible for
/// resolving the arrangement-local asset alias without comparing raw IDs.
#[derive(Clone)]
pub struct ArrangementWaveformSource {
    pub key: WaveformAssetKey,
    pub pyramid: Arc<WaveformPyramid>,
}

pub type ArrangementWaveformProvider =
    Arc<dyn Fn(AssetId) -> Option<ArrangementWaveformSource> + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ArrangementPatternPulse {
    pub offset_frames: u64,
    pub duration_frames: u64,
    pub velocity: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArrangementPatternPreview {
    pub length_frames: u64,
    pub pulses: Vec<ArrangementPatternPulse>,
}

/// Cross-domain read resolver used only for drag ghosts and clip decoration.
/// Implementations retain the real typed IDs and perform binding lookup; the
/// arrangement view never equates raw ID values across domains.
pub trait ArrangementPreviewResolver: Send + Sync {
    fn media_asset(&self, asset: crate::assets::AssetId) -> Option<ArrangementWaveformSource>;

    fn dropped_pattern(
        &self,
        pattern: crate::sequencer::PatternId,
    ) -> Option<ArrangementPatternPreview>;

    fn placed_pattern(
        &self,
        pattern: crate::arrangement::PatternId,
    ) -> Option<ArrangementPatternPreview>;
}

pub type SharedArrangementPreviewResolver = Arc<dyn ArrangementPreviewResolver>;

#[derive(Clone, Debug, PartialEq)]
pub struct ArrangementDropPreview {
    pub target: DropTarget,
    pub intent: Result<DropIntent, String>,
    pub placement: Option<FrameRange>,
    pub create_track: Option<TrackKind>,
    pub label: String,
}

#[derive(Clone, Debug)]
struct OptimisticPreview {
    patch: PreviewPatch,
}

#[derive(Default)]
struct WaveformPaintCache {
    entries: HashMap<WaveformProxyKey, Arc<WaveformQuery>>,
}

impl WaveformPaintCache {
    fn get_or_query(
        &mut self,
        request: &crate::waveform_proxy::ReadyWaveformRequest,
        pyramid: &WaveformPyramid,
    ) -> Result<Arc<WaveformQuery>, crate::waveform_proxy::WaveformProxyError> {
        if let Some(query) = self.entries.get(&request.key) {
            return Ok(Arc::clone(query));
        }
        let query = Arc::new(request.query_pyramid(pyramid)?);
        // A bounded epoch cache keeps scrub/follow repaint stable without
        // retaining every zoom range visited during a session.
        if self.entries.len() >= 256 {
            self.entries.clear();
        }
        self.entries.insert(request.key.clone(), Arc::clone(&query));
        Ok(query)
    }
}

fn lock_editor(editor: &SharedArrangementEditor) -> std::sync::MutexGuard<'_, ArrangementEditor> {
    editor
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn mutate_shared_editor<T>(
    editor: &SharedArrangementEditor,
    edit: impl FnOnce(&mut ArrangementEditor) -> Result<T, crate::arrangement::ArrangementError>,
) -> (
    Result<T, crate::arrangement::ArrangementError>,
    ArrangementEditor,
) {
    let mut editor = lock_editor(editor);
    let result = edit(&mut editor);
    // Clone while still holding the lock so the caller's render snapshot is
    // exactly the editor version that was just atomically published.
    (result, editor.clone())
}

/// Register these once during application startup if the view should respond
/// to its DAW keyboard shortcuts.
pub fn bind_arrangement_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-z", UndoArrangement, Some("AudecArrangement")),
        KeyBinding::new("shift-cmd-z", RedoArrangement, Some("AudecArrangement")),
        KeyBinding::new("cmd-d", DuplicateClip, Some("AudecArrangement")),
        KeyBinding::new("backspace", DeleteClip, Some("AudecArrangement")),
        KeyBinding::new("delete", DeleteClip, Some("AudecArrangement")),
        KeyBinding::new("cmd-e", SplitClip, Some("AudecArrangement")),
        KeyBinding::new("cmd-a", SelectAllArrangementClips, Some("AudecArrangement")),
        KeyBinding::new(
            "shift-tab",
            SelectPreviousArrangementClip,
            Some("AudecArrangement"),
        ),
        KeyBinding::new("tab", SelectNextArrangementClip, Some("AudecArrangement")),
        KeyBinding::new("left", NudgeClipLeft, Some("AudecArrangement")),
        KeyBinding::new("right", NudgeClipRight, Some("AudecArrangement")),
        KeyBinding::new("alt-left", NudgeClipFineLeft, Some("AudecArrangement")),
        KeyBinding::new("alt-right", NudgeClipFineRight, Some("AudecArrangement")),
        KeyBinding::new("alt-up", MoveClipTrackUp, Some("AudecArrangement")),
        KeyBinding::new("alt-down", MoveClipTrackDown, Some("AudecArrangement")),
        KeyBinding::new("[", TrimClipStart, Some("AudecArrangement")),
        KeyBinding::new("]", TrimClipEnd, Some("AudecArrangement")),
        KeyBinding::new("cmd-l", ToggleArrangementLoop, Some("AudecArrangement")),
        KeyBinding::new("=", ZoomArrangementIn, Some("AudecArrangement")),
        KeyBinding::new("-", ZoomArrangementOut, Some("AudecArrangement")),
        KeyBinding::new("shift-left", PanArrangementLeft, Some("AudecArrangement")),
        KeyBinding::new("shift-right", PanArrangementRight, Some("AudecArrangement")),
        KeyBinding::new("0", FitArrangement, Some("AudecArrangement")),
        KeyBinding::new("s", CycleArrangementSnap, Some("AudecArrangement")),
        KeyBinding::new("escape", CancelArrangementGesture, Some("AudecArrangement")),
    ]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapDivision {
    Off,
    Bar,
    Beat,
    Eighth,
    Sixteenth,
}

impl SnapDivision {
    fn next(self) -> Self {
        match self {
            Self::Off => Self::Bar,
            Self::Bar => Self::Beat,
            Self::Beat => Self::Eighth,
            Self::Eighth => Self::Sixteenth,
            Self::Sixteenth => Self::Off,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Off => "SNAP OFF",
            Self::Bar => "SNAP 1 BAR",
            Self::Beat => "SNAP 1/4",
            Self::Eighth => "SNAP 1/8",
            Self::Sixteenth => "SNAP 1/16",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArrangementViewport {
    pub start: Frame,
    pub end: Frame,
    pub minimum_span: u64,
}

impl ArrangementViewport {
    pub fn new(start: Frame, end: Frame, minimum_span: u64) -> Self {
        let minimum_span = minimum_span.max(1).min(i64::MAX as u64);
        let end = if end <= start {
            Frame(start.0.saturating_add(minimum_span as i64))
        } else {
            end
        };
        Self {
            start,
            end,
            minimum_span,
        }
    }

    pub fn span(self) -> u64 {
        self.end.0.saturating_sub(self.start.0) as u64
    }

    pub fn fraction(self, frame: Frame) -> f32 {
        (frame.0.saturating_sub(self.start.0) as f64 / self.span().max(1) as f64) as f32
    }

    pub fn frame_at_fraction(self, fraction: f64) -> Frame {
        let offset = (fraction.clamp(0.0, 1.0) * self.span() as f64).round();
        Frame(self.start.0.saturating_add(offset as i64))
    }

    /// Maps a pointer beyond the viewport edges without folding it back into
    /// the visible interval. This keeps a drag's timeline direction honest;
    /// an eventual edge-scroll adapter may consume the same coordinate.
    pub fn frame_at_unclamped_fraction(self, fraction: f64) -> Frame {
        if !fraction.is_finite() {
            return self.start;
        }
        let offset = (fraction * self.span() as f64).round();
        Frame(self.start.0.saturating_add(offset as i64))
    }

    pub fn pan(&mut self, fraction: f64) {
        if !fraction.is_finite() {
            return;
        }
        let delta = (self.span() as f64 * fraction).round() as i64;
        self.start.0 = self.start.0.saturating_add(delta);
        self.end.0 = self.end.0.saturating_add(delta);
    }

    pub fn zoom_around(&mut self, anchor: Frame, scale: f64) {
        if !scale.is_finite() || scale <= 0.0 {
            return;
        }
        let old_span = self.span().max(1);
        let new_span =
            ((old_span as f64 * scale).round() as u64).clamp(self.minimum_span, i64::MAX as u64);
        let anchor_fraction =
            (anchor.0.saturating_sub(self.start.0) as f64 / old_span as f64).clamp(0.0, 1.0);
        let left = (new_span as f64 * anchor_fraction).round() as i64;
        self.start = Frame(anchor.0.saturating_sub(left));
        self.end = Frame(self.start.0.saturating_add(new_span as i64));
    }

    /// Keeps the playhead inside a stable lead/trail margin while preserving
    /// the user's zoom. It never fits the whole song or assumes frame zero.
    pub fn ensure_visible(&mut self, frame: Frame, lead_fraction: f64) -> bool {
        let span = self.span().max(1);
        let lead = (span as f64 * lead_fraction.clamp(0.0, 0.45)).round() as i64;
        let left_guard = Frame(self.start.0.saturating_add(lead));
        let right_guard = Frame(self.end.0.saturating_sub(lead));
        if frame >= left_guard && frame < right_guard {
            return false;
        }
        self.start = Frame(frame.0.saturating_sub(lead));
        self.end = Frame(
            self.start
                .0
                .saturating_add(span.min(i64::MAX as u64) as i64),
        );
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct EdgeScrollPlan {
    pan_fraction: f64,
}

/// Plan pane-local horizontal autoscroll from a captured pointer. The
/// intensity ramps toward and beyond the edge, but remains bounded so one
/// coarse pointer event cannot throw the musician across the song.
fn plan_edge_scroll(left: f64, width: f64, pointer_x: f64) -> Option<EdgeScrollPlan> {
    if !left.is_finite() || !width.is_finite() || !pointer_x.is_finite() || width <= 0.0 {
        return None;
    }
    let zone = EDGE_SCROLL_ZONE.min(width / 3.0).max(1.0);
    let right = left + width;
    let intensity = if pointer_x < left + zone {
        -((left + zone - pointer_x) / zone).clamp(0.0, 1.0)
    } else if pointer_x > right - zone {
        ((pointer_x - (right - zone)) / zone).clamp(0.0, 1.0)
    } else {
        return None;
    };
    Some(EdgeScrollPlan {
        pan_fraction: intensity * EDGE_SCROLL_MAX_FRACTION,
    })
}

/// A dock/window-ready GPUI entity over the persistent arrangement core.
pub struct ArrangementView {
    // Rendering always works from this local snapshot. When `shared_editor` is
    // present, edits are applied while holding the shared editor's lock and
    // this snapshot is replaced before releasing that lock.
    editor: ArrangementEditor,
    shared_editor: Option<SharedArrangementEditor>,
    selection: Selection,
    expected_project_revision: u64,
    pending_editor_snapshot: Option<ArrangementEditor>,
    pending_project_revision: Option<u64>,
    viewport: ArrangementViewport,
    focus_handle: FocusHandle,
    timeline_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    track_bounds: Arc<Mutex<BTreeMap<crate::arrangement::TrackId, Bounds<Pixels>>>>,
    interaction: ArrangementInteraction,
    optimistic_preview: Option<OptimisticPreview>,
    callback: Option<ArrangementViewCallback>,
    timeline_callback: Option<ArrangementTimelineCallback>,
    gesture_callback: Option<ArrangementGestureCallback>,
    editor_session: u64,
    next_gesture_series: u64,
    active_gesture_identity: Option<ArrangementGestureIdentity>,
    waveform_provider: Option<ArrangementWaveformProvider>,
    preview_resolver: Option<SharedArrangementPreviewResolver>,
    drop_preview: Arc<Mutex<Option<ArrangementDropPreview>>>,
    waveform_cache: Arc<Mutex<WaveformPaintCache>>,
    focus_subscription: Option<Subscription>,
    playhead: Frame,
    transport_playing: bool,
    follow_playhead: bool,
    tempo_map: TempoMap,
    bpm: f64,
    beats_per_bar: u8,
    snap: SnapDivision,
    loop_range: Option<FrameRange>,
    ruler_gesture: Option<RulerGesture>,
    status: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RulerGestureMode {
    TimeSelection {
        anchor: Frame,
        original: Option<FrameRange>,
    },
    LoopStart {
        original: FrameRange,
    },
    LoopEnd {
        original: FrameRange,
    },
    LoopMove {
        original: FrameRange,
        grabbed_at: Frame,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct RulerGesture {
    mode: RulerGestureMode,
    current: Frame,
    press_x: f32,
    dragged: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RulerGesturePreview {
    time: Option<FrameRange>,
    loop_range: Option<FrameRange>,
}

impl RulerGesture {
    fn update(&mut self, frame: Frame, x: f32) -> RulerGesturePreview {
        self.current = frame;
        self.dragged |= (x - self.press_x).abs() >= RULER_DRAG_THRESHOLD;
        if !self.dragged {
            return RulerGesturePreview::default();
        }
        match self.mode {
            RulerGestureMode::TimeSelection { anchor, .. } => RulerGesturePreview {
                time: FrameRange::new(anchor.min(frame), anchor.max(frame)).ok(),
                loop_range: None,
            },
            RulerGestureMode::LoopStart { original } => {
                let latest_start = Frame(original.end.0.saturating_sub(1));
                RulerGesturePreview {
                    time: None,
                    loop_range: FrameRange::new(frame.min(latest_start), original.end).ok(),
                }
            }
            RulerGestureMode::LoopEnd { original } => {
                let earliest_end = Frame(original.start.0.saturating_add(1));
                RulerGesturePreview {
                    time: None,
                    loop_range: FrameRange::new(original.start, frame.max(earliest_end)).ok(),
                }
            }
            RulerGestureMode::LoopMove {
                original,
                grabbed_at,
            } => {
                let delta = frame.0.saturating_sub(grabbed_at.0);
                let start = Frame(original.start.0.saturating_add(delta));
                RulerGesturePreview {
                    time: None,
                    loop_range: FrameRange::from_start_and_len(start, original.len()).ok(),
                }
            }
        }
    }

    const fn edits_loop(self) -> bool {
        !matches!(self.mode, RulerGestureMode::TimeSelection { .. })
    }
}

impl ArrangementView {
    /// Construct from project arrangement state. An entirely empty state is
    /// seeded with a compact three-track demonstration so this component is
    /// immediately inspectable before a project adapter lands.
    pub fn new(mut editor: ArrangementEditor, cx: &mut Context<Self>) -> Self {
        let seeded = editor.state().track_order.is_empty();
        if seeded {
            seed_demo(&mut editor).expect("the built-in arrangement demo must be valid");
            editor.mark_saved();
        }
        Self::from_snapshot(editor, None, seeded, cx)
    }

    /// Construct a view over a project-owned editor shared with other
    /// controllers. The view snapshots the editor for rendering; successful
    /// edits, undos, and redos are performed and published under this mutex.
    pub fn from_shared_editor(editor: SharedArrangementEditor, cx: &mut Context<Self>) -> Self {
        let (snapshot, seeded) = {
            let mut editor = lock_editor(&editor);
            let seeded = editor.state().track_order.is_empty();
            if seeded {
                seed_demo(&mut editor).expect("the built-in arrangement demo must be valid");
                editor.mark_saved();
            }
            (editor.clone(), seeded)
        };
        Self::from_snapshot(snapshot, Some(editor), seeded, cx)
    }

    /// Alias for [`Self::from_shared_editor`] for concise call sites.
    pub fn from_shared(editor: SharedArrangementEditor, cx: &mut Context<Self>) -> Self {
        Self::from_shared_editor(editor, cx)
    }

    /// Construct the project-backed surface with its mutation and waveform
    /// seams attached up front. This is the preferred aggregate integration;
    /// `from_shared_editor` remains for compatibility with existing hosts.
    pub fn from_shared_sources(
        editor: SharedArrangementEditor,
        expected_project_revision: u64,
        callback: ArrangementViewCallback,
        waveform_provider: Option<ArrangementWaveformProvider>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::from_shared_editor(editor, cx);
        view.expected_project_revision = expected_project_revision;
        view.callback = Some(callback);
        view.waveform_provider = waveform_provider;
        view
    }

    /// Alias for [`Self::from_shared_editor`] that makes the injected dependency
    /// explicit at call sites.
    pub fn with_shared_editor(editor: SharedArrangementEditor, cx: &mut Context<Self>) -> Self {
        Self::from_shared_editor(editor, cx)
    }

    fn from_snapshot(
        editor: ArrangementEditor,
        shared_editor: Option<SharedArrangementEditor>,
        seeded: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let bpm = 120.0;
        let beats_per_bar = 4;
        let tempo_map = TempoMap::common_time(editor.state().sample_rate, bpm)
            .expect("arrangement editor sample rate and default tempo are valid");
        let viewport = fit_viewport(&editor, bpm, beats_per_bar);
        let selection = editor.selection.clone();
        Self {
            editor,
            shared_editor,
            selection,
            expected_project_revision: 0,
            pending_editor_snapshot: None,
            pending_project_revision: None,
            viewport,
            focus_handle: cx.focus_handle(),
            timeline_bounds: Arc::new(Mutex::new(None)),
            track_bounds: Arc::new(Mutex::new(BTreeMap::new())),
            interaction: ArrangementInteraction::default(),
            optimistic_preview: None,
            callback: None,
            timeline_callback: None,
            gesture_callback: None,
            editor_session: 1,
            next_gesture_series: 1,
            active_gesture_identity: None,
            waveform_provider: None,
            preview_resolver: None,
            drop_preview: Arc::new(Mutex::new(None)),
            waveform_cache: Arc::new(Mutex::new(WaveformPaintCache::default())),
            focus_subscription: None,
            playhead: Frame::ZERO,
            transport_playing: false,
            follow_playhead: true,
            tempo_map,
            bpm,
            beats_per_bar,
            snap: SnapDivision::Beat,
            loop_range: None,
            ruler_gesture: None,
            status: if seeded {
                "Demo arrangement · select a clip to edit exact project metadata".into()
            } else {
                "Arrangement ready".into()
            },
        }
    }

    pub fn demo(cx: &mut Context<Self>) -> Self {
        Self::new(
            ArrangementEditor::new(48_000).expect("fixed demo sample rate is valid"),
            cx,
        )
    }

    pub fn editor(&self) -> &ArrangementEditor {
        &self.editor
    }

    pub fn editor_mut(&mut self) -> &mut ArrangementEditor {
        &mut self.editor
    }

    /// Returns the injected shared editor, if this view was constructed with
    /// one. Callers that modify it directly should let the next render take a
    /// fresh snapshot.
    pub fn shared_editor(&self) -> Option<&SharedArrangementEditor> {
        self.shared_editor.as_ref()
    }

    pub fn set_callback(&mut self, callback: Option<ArrangementViewCallback>) {
        self.callback = callback;
    }

    pub fn set_timeline_callback(&mut self, callback: Option<ArrangementTimelineCallback>) {
        self.timeline_callback = callback;
    }

    pub fn set_gesture_callback(
        &mut self,
        editor_session: u64,
        callback: Option<ArrangementGestureCallback>,
    ) {
        self.editor_session = editor_session.max(1);
        self.gesture_callback = callback;
    }

    pub fn set_waveform_provider(&mut self, provider: Option<ArrangementWaveformProvider>) {
        self.waveform_provider = provider;
    }

    /// Attach cross-domain read adapters for exact media ghosts and pattern
    /// decorations. The resolver is never used to construct a command: drops
    /// retain their source-domain IDs for the aggregate controller.
    pub fn set_preview_resolver(
        &mut self,
        resolver: Option<SharedArrangementPreviewResolver>,
        cx: &mut Context<Self>,
    ) {
        self.preview_resolver = resolver;
        if let Ok(mut preview) = self.drop_preview.lock() {
            *preview = None;
        }
        cx.notify();
    }

    /// Replace the read snapshot after the controller has committed an emitted
    /// intent. An active pointer gesture retains its immutable press baseline;
    /// the newer publication is installed as soon as that gesture commits or
    /// cancels. This does not alter the independent horizontal viewport.
    pub fn set_editor_snapshot(&mut self, editor: ArrangementEditor, cx: &mut Context<Self>) {
        self.pending_editor_snapshot = Some(editor);
        self.flush_project_publication(cx);
    }

    pub fn set_project_revision(&mut self, revision: u64, cx: &mut Context<Self>) {
        self.pending_project_revision = Some(revision);
        self.flush_project_publication(cx);
    }

    /// Atomically supply the aggregate state and revision. Hosts should prefer
    /// this over the compatibility setters so a deferred drag refresh cannot
    /// momentarily pair new entities with an old revision token.
    pub fn set_project_snapshot(
        &mut self,
        editor: ArrangementEditor,
        revision: u64,
        cx: &mut Context<Self>,
    ) {
        self.pending_editor_snapshot = Some(editor);
        self.pending_project_revision = Some(revision);
        self.flush_project_publication(cx);
    }

    pub fn has_active_gesture(&self) -> bool {
        self.interaction.phase() != GesturePhase::Idle
    }

    fn flush_project_publication(&mut self, cx: &mut Context<Self>) {
        if self.has_active_gesture() {
            return;
        }
        let editor = self.pending_editor_snapshot.take();
        let revision = self.pending_project_revision.take();
        if editor.is_none() && revision.is_none() {
            return;
        }
        if let Some(editor) = editor {
            if let Some(shared_editor) = &self.shared_editor {
                *lock_editor(shared_editor) = editor.clone();
            }
            self.editor = editor;
            if self.callback.is_none() {
                self.selection = self.editor.selection.clone();
            } else {
                self.selection
                    .clips
                    .retain(|clip| self.editor.state().clip(*clip).is_some());
                self.selection.tracks = self
                    .selection
                    .clips
                    .iter()
                    .filter_map(|clip| self.editor.state().clip(*clip).map(|clip| clip.track_id))
                    .collect();
                // Object selection and time selection are independent DAW
                // concepts. Project refresh prunes objects but never moves a
                // musician-authored ruler selection or transport loop.
            }
        }
        if let Some(revision) = revision {
            self.expected_project_revision = revision;
        }
        self.optimistic_preview = None;
        if let Ok(mut preview) = self.drop_preview.lock() {
            *preview = None;
        }
        cx.notify();
    }

    pub fn set_selection(&mut self, selection: Selection, cx: &mut Context<Self>) {
        self.selection = selection;
        cx.notify();
    }

    pub fn set_loop_range(&mut self, range: Option<FrameRange>, cx: &mut Context<Self>) {
        self.loop_range = range;
        cx.notify();
    }

    pub fn viewport(&self) -> ArrangementViewport {
        self.viewport
    }

    pub fn set_viewport(&mut self, viewport: ArrangementViewport, cx: &mut Context<Self>) {
        self.viewport = viewport;
        self.follow_playhead = false;
        self.status = "Timeline view positioned independently".into();
        cx.notify();
    }

    /// Update transport presentation. Follow keeps the current project sample
    /// visible regardless of whether the transport is playing, paused, or
    /// stopped. Manual pane navigation disables Follow, so a detached view is
    /// never pulled back by this publication.
    pub fn set_playhead(&mut self, playhead: Frame, playing: bool, cx: &mut Context<Self>) {
        self.playhead = playhead;
        self.transport_playing = playing;
        if self.follow_playhead {
            self.viewport.ensure_visible(playhead, 0.16);
        }
        cx.notify();
    }

    pub fn set_follow_playhead(&mut self, follow: bool, cx: &mut Context<Self>) {
        self.follow_playhead = follow;
        if follow {
            self.viewport.ensure_visible(self.playhead, 0.16);
            self.status = "Following playhead".into();
        } else {
            self.status = "Timeline scroll detached from playhead".into();
        }
        cx.notify();
    }

    fn refresh_editor_snapshot(&mut self) {
        if self.has_active_gesture() {
            return;
        }
        if let Some(shared_editor) = &self.shared_editor {
            self.editor = lock_editor(shared_editor).clone();
        }
        if self.callback.is_none() {
            self.selection = self.editor.selection.clone();
        }
    }

    /// Apply an editor operation atomically when backed by a shared editor,
    /// then capture its post-operation snapshot for the next render. This is
    /// deliberately the only route arrangement commands take to mutate state.
    fn mutate_editor<T>(
        &mut self,
        edit: impl FnOnce(&mut ArrangementEditor) -> Result<T, crate::arrangement::ArrangementError>,
    ) -> Result<T, crate::arrangement::ArrangementError> {
        if let Some(shared_editor) = &self.shared_editor {
            let (result, snapshot) = mutate_shared_editor(shared_editor, edit);
            self.editor = snapshot;
            result
        } else {
            edit(&mut self.editor)
        }
    }

    /// Publish a non-transactional editor update, such as the UI selection,
    /// without replacing a newer shared project snapshot.
    fn update_editor(&mut self, update: impl FnOnce(&mut ArrangementEditor)) {
        if let Some(shared_editor) = &self.shared_editor {
            let snapshot = {
                let mut editor = lock_editor(shared_editor);
                update(&mut editor);
                editor.clone()
            };
            self.editor = snapshot;
        } else {
            update(&mut self.editor);
        }
    }

    pub fn set_tempo(&mut self, bpm: f64, beats_per_bar: u8, cx: &mut Context<Self>) {
        if bpm.is_finite() && bpm > 0.0 && beats_per_bar > 0 {
            if let (Ok(tempo), Ok(meter)) = (
                Tempo::from_bpm(bpm),
                TimeSignature::new(u16::from(beats_per_bar), 4),
            ) {
                if let Ok(map) = TempoMap::new(self.editor.state().sample_rate, tempo, meter) {
                    self.tempo_map = map;
                }
            }
            self.bpm = bpm;
            self.beats_per_bar = beats_per_bar;
            self.status = format!("Grid set to {bpm:.2} BPM · {beats_per_bar}/4");
            cx.notify();
        }
    }

    /// Install the authoritative project tempo/meter map. This is the path
    /// that keeps ruler lines and snapping correct across tempo changes.
    pub fn set_tempo_map(&mut self, map: TempoMap, cx: &mut Context<Self>) {
        if map.sample_rate() != self.editor.state().sample_rate {
            self.status = "Tempo map refused · sample-rate mismatch".into();
            cx.notify();
            return;
        }
        self.bpm = map.tempo_at(BeatTime::ZERO).bpm();
        self.beats_per_bar = map
            .meter_at(BeatTime::ZERO)
            .numerator
            .min(u16::from(u8::MAX)) as u8;
        self.tempo_map = map;
        self.status = "Project tempo map installed".into();
        cx.notify();
    }

    fn selected_clip_id(&self) -> Option<ClipId> {
        self.selection.clips.iter().next().copied()
    }

    fn pointer_at(
        &self,
        position: gpui::Point<Pixels>,
        modifiers: gpui::Modifiers,
    ) -> Option<TimelinePointer> {
        let bounds = (*self.timeline_bounds.lock().ok()?)?;
        let width = f64::from(f32::from(bounds.size.width));
        if width <= 0.0 {
            return None;
        }
        let fraction = f64::from(f32::from(position.x - bounds.origin.x)) / width;
        let track_layouts = self.track_interaction_layouts();
        Some(TimelinePointer {
            canvas: CanvasPoint::new(
                f64::from(f32::from(position.x)),
                f64::from(f32::from(position.y)),
            ),
            frame: self.viewport.frame_at_unclamped_fraction(fraction),
            track: hit_test_track(
                self.editor.state(),
                &track_layouts,
                CanvasPoint::new(
                    f64::from(f32::from(position.x)),
                    f64::from(f32::from(position.y)),
                ),
            ),
            modifiers: PointerModifiers {
                shift: modifiers.shift,
                command: modifiers.secondary(),
                option: modifiers.alt,
                control: modifiers.control,
            },
        })
    }

    fn edge_scroll_for_pointer(&mut self, x: Pixels) -> bool {
        let plan = self
            .timeline_bounds
            .lock()
            .ok()
            .and_then(|bounds| *bounds)
            .and_then(|bounds| {
                plan_edge_scroll(
                    f64::from(f32::from(bounds.origin.x)),
                    f64::from(f32::from(bounds.size.width)),
                    f64::from(f32::from(x)),
                )
            });
        let Some(plan) = plan else {
            return false;
        };
        self.viewport.pan(plan.pan_fraction);
        self.follow_playhead = false;
        true
    }

    fn track_interaction_layouts(&self) -> Vec<TrackInteractionLayout> {
        self.track_bounds
            .lock()
            .map(|bounds| {
                self.editor
                    .state()
                    .track_order
                    .iter()
                    .enumerate()
                    .filter_map(|(index, id)| {
                        let bounds = bounds.get(id)?;
                        Some(TrackInteractionLayout {
                            track_id: *id,
                            bounds: canvas_rect(*bounds),
                            z_order: index.min(i32::MAX as usize) as i32,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn clip_interaction_layouts(&self) -> Vec<ClipInteractionLayout> {
        let Ok(track_bounds) = self.track_bounds.lock() else {
            return Vec::new();
        };
        let mut result = Vec::new();
        for (z_order, clip) in self.editor.state().clips.values().enumerate() {
            let Some(bounds) = track_bounds.get(&clip.track_id) else {
                continue;
            };
            if clip.placement.end <= self.viewport.start
                || clip.placement.start >= self.viewport.end
            {
                continue;
            }
            // Keep true offscreen edges outside the hit rectangle. Clamping
            // them to the viewport would manufacture trim/fade handles at the
            // screen boundary for an edge the musician cannot actually see.
            let left =
                bounds.origin.x + bounds.size.width * self.viewport.fraction(clip.placement.start);
            let mut right =
                bounds.origin.x + bounds.size.width * self.viewport.fraction(clip.placement.end);
            if clip.placement.start >= self.viewport.start
                && clip.placement.end <= self.viewport.end
            {
                right = right.max(left + px(7.0));
            }
            let top = bounds.origin.y + px(7.0);
            let bottom = bounds.origin.y + bounds.size.height - px(7.0);
            let true_right_edge_visible =
                clip.placement.end > self.viewport.start && clip.placement.end <= self.viewport.end;
            let repeat_handle = (true_right_edge_visible && clip_repeat_capable(clip)).then(|| {
                CanvasRect::new(
                    f64::from(f32::from(right - px(7.0))),
                    f64::from(f32::from(bottom - px(18.0))),
                    f64::from(f32::from(right + px(7.0))),
                    f64::from(f32::from(bottom - px(4.0))),
                )
            });
            result.push(ClipInteractionLayout {
                clip_id: clip.id,
                bounds: CanvasRect::new(
                    f64::from(f32::from(left)),
                    f64::from(f32::from(top)),
                    f64::from(f32::from(right)),
                    f64::from(f32::from(bottom)),
                ),
                repeat_handle,
                z_order: z_order.min(i32::MAX as usize) as i32,
            });
        }
        result
    }

    fn snap_context(&self) -> SnapContext {
        let tolerance_frames = self
            .timeline_bounds
            .lock()
            .ok()
            .and_then(|bounds| *bounds)
            .map(|bounds| {
                let width = f64::from(f32::from(bounds.size.width)).max(1.0);
                (self.viewport.span() as f64 * 8.0 / width).ceil() as u64
            })
            .unwrap_or(1);
        let mut guides = self
            .musical_grid(tolerance_frames)
            .map_or_else(Vec::new, |grid| grid.snap.guides);
        guides.reserve(self.editor.state().clips.len() * 2 + 3);
        guides.push(SnapGuide {
            frame: self.playhead,
            kind: SnapGuideKind::Playhead,
            key: 0,
        });
        for clip in self.editor.state().clips.values() {
            guides.push(SnapGuide {
                frame: clip.placement.start,
                kind: SnapGuideKind::ClipStart(clip.id),
                key: clip.id.get(),
            });
            guides.push(SnapGuide {
                frame: clip.placement.end,
                kind: SnapGuideKind::ClipEnd(clip.id),
                key: clip.id.get(),
            });
        }
        if let Some(range) = self.loop_range {
            guides.push(SnapGuide {
                frame: range.start,
                kind: SnapGuideKind::LoopBoundary,
                key: 0,
            });
            guides.push(SnapGuide {
                frame: range.end,
                kind: SnapGuideKind::LoopBoundary,
                key: 1,
            });
        }
        SnapContext {
            grid_quantum: None,
            tolerance_frames,
            guides,
        }
    }

    fn musical_grid(
        &self,
        tolerance_frames: u64,
    ) -> Option<crate::arrangement_interaction::surface::MusicalGridPlan> {
        let resolution = match self.snap {
            SnapDivision::Off => return None,
            SnapDivision::Bar => MusicalGridResolution::Bar,
            SnapDivision::Beat => MusicalGridResolution::Beat,
            SnapDivision::Eighth => MusicalGridResolution::Eighth,
            SnapDivision::Sixteenth => MusicalGridResolution::Sixteenth,
        };
        let visible = FrameRange::new(self.viewport.start, self.viewport.end).ok()?;
        let mut grid = plan_musical_grid(
            &self.tempo_map,
            visible,
            resolution,
            tolerance_frames,
            DEFAULT_GRID_LINE_LIMIT,
        );
        if self.snap == SnapDivision::Bar {
            grid.lines.retain(|line| line.kind == SnapGuideKind::Bar);
            grid.snap
                .guides
                .retain(|guide| guide.kind == SnapGuideKind::Bar);
        }
        Some(grid)
    }

    fn begin_arrangement_pointer(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        window.focus(&self.focus_handle);
        self.refresh_editor_snapshot();
        self.optimistic_preview = None;
        let Some(pointer) = self.pointer_at(event.position, event.modifiers) else {
            return;
        };
        let layouts = self.clip_interaction_layouts();
        let hit = hit_test_clip(
            self.editor.state(),
            &layouts,
            pointer.canvas,
            pointer.modifiers,
            GestureConfig::default().hit_metrics,
        );
        let response = self.interaction.pointer_down(
            self.editor.state(),
            &self.selection,
            self.expected_project_revision,
            &layouts,
            pointer,
            GestureConfig::default(),
        );
        if matches!(response, GestureResponse::Pressed { .. }) {
            if let Some(hit) = hit {
                self.begin_project_gesture(hit.clip_id);
            }
        }
        self.describe_gesture_response(&response);
        cx.notify();
    }

    fn request_seek(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some(frame) = self.frame_at_timeline_x(position.x, false) else {
            return;
        };
        self.playhead = frame;
        if self.follow_playhead {
            self.viewport.ensure_visible(frame, 0.16);
        }
        if let Some(callback) = self.callback.as_ref() {
            callback(ArrangementViewEvent::SeekRequested(frame));
        }
        self.status = format!("Seek requested · sample {}", grouped_i64(frame.0));
        cx.notify();
    }

    fn frame_at_timeline_x(&self, x: Pixels, unclamped: bool) -> Option<Frame> {
        let bounds = self
            .timeline_bounds
            .lock()
            .ok()
            .and_then(|bounds| *bounds)?;
        let width = f64::from(f32::from(bounds.size.width)).max(1.0);
        let fraction = f64::from(f32::from(x - bounds.origin.x)) / width;
        Some(if unclamped {
            self.viewport.frame_at_unclamped_fraction(fraction)
        } else {
            self.viewport.frame_at_fraction(fraction)
        })
    }

    fn snapped_ruler_frame(&self, frame: Frame, suppress_snap: bool) -> Frame {
        if suppress_snap {
            return frame;
        }
        let Some(quantum) = snap_frames(
            self.editor.state().sample_rate,
            self.bpm,
            self.beats_per_bar,
            self.snap,
        ) else {
            return frame;
        };
        Frame(snap_frame(frame.0, quantum.min(i64::MAX as u64) as i64))
    }

    /// The lower ruler strip belongs to loop editing. Keeping it separate
    /// from the main ruler preserves the familiar click-to-locate gesture
    /// inside an active loop while still exposing large, stable loop handles.
    fn ruler_loop_gesture(&self, event: &MouseDownEvent, at: Frame) -> Option<RulerGestureMode> {
        let bounds = self
            .timeline_bounds
            .lock()
            .ok()
            .and_then(|bounds| *bounds)?;
        let local_y = f32::from(event.position.y - bounds.origin.y);
        if !(RULER_HEIGHT - RULER_LOOP_STRIP_HEIGHT..=RULER_HEIGHT).contains(&local_y) {
            return None;
        }
        let range = self.loop_range?;
        let width = f32::from(bounds.size.width).max(1.0);
        let local_x = f32::from(event.position.x - bounds.origin.x);
        let start_x = self.viewport.fraction(range.start) * width;
        let end_x = self.viewport.fraction(range.end) * width;
        let start_visible = self.viewport.start <= range.start && range.start < self.viewport.end;
        let end_visible = self.viewport.start < range.end && range.end <= self.viewport.end;
        if start_visible && (local_x - start_x).abs() <= RULER_LOOP_HANDLE_RADIUS {
            Some(RulerGestureMode::LoopStart { original: range })
        } else if end_visible && (local_x - end_x).abs() <= RULER_LOOP_HANDLE_RADIUS {
            Some(RulerGestureMode::LoopEnd { original: range })
        } else if local_x >= start_x.max(0.0) && local_x <= end_x.min(width) {
            Some(RulerGestureMode::LoopMove {
                original: range,
                grabbed_at: at,
            })
        } else {
            None
        }
    }

    fn begin_ruler_selection(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some(raw_frame) = self.frame_at_timeline_x(event.position.x, false) else {
            return;
        };
        let frame = self.snapped_ruler_frame(raw_frame, event.modifiers.shift);
        let mode =
            self.ruler_loop_gesture(event, frame)
                .unwrap_or(RulerGestureMode::TimeSelection {
                    anchor: frame,
                    original: self.selection.time,
                });
        self.ruler_gesture = Some(RulerGesture {
            mode,
            current: frame,
            press_x: f32::from(event.position.x),
            dragged: false,
        });
        self.status = if self.ruler_gesture.is_some_and(RulerGesture::edits_loop) {
            "Drag the loop edge or lower strip · Shift bypasses snap".into()
        } else {
            "Drag ruler for time selection · click seeks".into()
        };
        cx.notify();
    }

    fn drag_ruler_selection(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !event.dragging() || self.ruler_gesture.is_none() {
            return;
        }
        let Some(raw_frame) = self.frame_at_timeline_x(event.position.x, true) else {
            return;
        };
        let mut frame = self.snapped_ruler_frame(raw_frame, event.modifiers.shift);
        let mut preview = self
            .ruler_gesture
            .as_mut()
            .unwrap()
            .update(frame, f32::from(event.position.x));
        let edge_scrolled = self.ruler_gesture.is_some_and(|gesture| gesture.dragged)
            && self.edge_scroll_for_pointer(event.position.x);
        if edge_scrolled {
            if let Some(scrolled_frame) = self.frame_at_timeline_x(event.position.x, true) {
                frame = self.snapped_ruler_frame(scrolled_frame, event.modifiers.shift);
                preview = self
                    .ruler_gesture
                    .as_mut()
                    .unwrap()
                    .update(frame, f32::from(event.position.x));
            }
        }
        let gesture = self.ruler_gesture.as_ref().unwrap();
        let dragged_time_selection = gesture.dragged && !gesture.edits_loop();
        if let Some(range) = preview.time {
            self.apply_timeline_selection(TimelineSelectionEdit::SetTime(range), false);
            self.status = format!(
                "Time selection · {}..{}",
                grouped_i64(range.start.0),
                grouped_i64(range.end.0)
            );
        } else if dragged_time_selection {
            self.apply_timeline_selection(TimelineSelectionEdit::ClearTime, false);
            self.status = "Time selection collapsed · release to clear".into();
        } else if let Some(range) = preview.loop_range {
            self.apply_timeline_selection(TimelineSelectionEdit::SetLoop(range), false);
            self.status = format!(
                "Loop · {}..{} · release to apply",
                grouped_i64(range.start.0),
                grouped_i64(range.end.0)
            );
        }
        if edge_scrolled {
            self.status.push_str(" · edge scroll");
        }
        cx.notify();
    }

    fn end_ruler_selection(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.ruler_gesture.is_none() {
            return;
        }
        if let Some(raw_frame) = self.frame_at_timeline_x(event.position.x, true) {
            let frame = self.snapped_ruler_frame(raw_frame, event.modifiers.shift);
            if let Some(gesture) = self.ruler_gesture.as_mut() {
                let preview = gesture.update(frame, f32::from(event.position.x));
                if gesture.dragged && !gesture.edits_loop() {
                    self.selection.time = preview.time;
                } else if let Some(range) = preview.loop_range {
                    self.loop_range = Some(range);
                }
            }
        }
        let gesture = self.ruler_gesture.take().unwrap();
        if gesture.dragged {
            if let Some(callback) = &self.timeline_callback {
                if gesture.edits_loop() {
                    callback(ArrangementTimelineEvent::LoopChanged(self.loop_range));
                } else {
                    callback(ArrangementTimelineEvent::TimeSelectionChanged(
                        self.selection.time,
                    ));
                }
            }
        } else {
            // A click is a locate, not a zero-width drag. Clear only the time
            // selection; the independently authored loop remains untouched.
            if self.selection.time.is_some() {
                self.apply_timeline_selection(TimelineSelectionEdit::ClearTime, true);
            }
            self.request_seek(event.position, cx);
        }
        cx.notify();
    }

    fn apply_timeline_selection(&mut self, edit: TimelineSelectionEdit, publish: bool) {
        match edit {
            TimelineSelectionEdit::SetTime(range) => self.selection.time = Some(range),
            TimelineSelectionEdit::ClearTime => self.selection.time = None,
            TimelineSelectionEdit::SetLoop(range) => self.loop_range = Some(range),
            TimelineSelectionEdit::SetLoopFromTime => self.loop_range = self.selection.time,
            TimelineSelectionEdit::ClearLoop => self.loop_range = None,
        }
        if publish {
            if let Some(callback) = &self.timeline_callback {
                match edit {
                    TimelineSelectionEdit::SetTime(_) | TimelineSelectionEdit::ClearTime => {
                        callback(ArrangementTimelineEvent::TimeSelectionChanged(
                            self.selection.time,
                        ))
                    }
                    TimelineSelectionEdit::SetLoop(_)
                    | TimelineSelectionEdit::SetLoopFromTime
                    | TimelineSelectionEdit::ClearLoop => {
                        callback(ArrangementTimelineEvent::LoopChanged(self.loop_range))
                    }
                }
            }
        }
    }

    fn toggle_loop_from_time_selection(&mut self, cx: &mut Context<Self>) {
        if self.loop_range.is_some() {
            self.apply_timeline_selection(TimelineSelectionEdit::ClearLoop, true);
            self.status = "Arrangement loop off".into();
        } else if self.selection.time.is_some() {
            self.apply_timeline_selection(TimelineSelectionEdit::SetLoopFromTime, true);
            self.status = "Arrangement loop set from ruler selection".into();
        } else {
            self.status = "Drag a ruler time selection before enabling loop".into();
        }
        cx.notify();
    }

    fn cancel_ruler_gesture(&mut self) -> bool {
        let Some(gesture) = self.ruler_gesture.take() else {
            return false;
        };
        match gesture.mode {
            RulerGestureMode::TimeSelection { original, .. } => self.selection.time = original,
            RulerGestureMode::LoopStart { original }
            | RulerGestureMode::LoopEnd { original }
            | RulerGestureMode::LoopMove { original, .. } => self.loop_range = Some(original),
        }
        true
    }

    fn drag_arrangement_pointer(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !event.dragging() {
            return;
        }
        let Some(pointer) = self.pointer_at(event.position, event.modifiers) else {
            return;
        };
        let snap = self.snap_context();
        let mut response = self.interaction.pointer_move(
            self.editor.state(),
            pointer,
            &snap,
            GestureConfig::default(),
        );
        let edge_scrolled = self.interaction.phase() == GesturePhase::Dragging
            && self.edge_scroll_for_pointer(event.position.x);
        if edge_scrolled {
            if let Some(pointer) = self.pointer_at(event.position, event.modifiers) {
                let snap = self.snap_context();
                response = self.interaction.pointer_move(
                    self.editor.state(),
                    pointer,
                    &snap,
                    GestureConfig::default(),
                );
            }
        }
        self.describe_gesture_response(&response);
        if edge_scrolled {
            self.status.push_str(" · edge scroll");
        }
        cx.notify();
    }

    fn end_arrangement_pointer(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        if event.button != MouseButton::Left {
            return;
        }
        let Some(pointer) = self.pointer_at(event.position, event.modifiers) else {
            self.interaction.cancel();
            self.end_project_gesture(true);
            self.flush_project_publication(cx);
            cx.notify();
            return;
        };
        let snap = self.snap_context();
        // Recompute once at the exact release coordinate so the optimistic
        // visual and the emitted semantic boundary cannot disagree.
        let _ = self.interaction.pointer_move(
            self.editor.state(),
            pointer,
            &snap,
            GestureConfig::default(),
        );
        let optimistic = self.interaction.preview().cloned();
        let response = self.interaction.pointer_up(
            self.editor.state(),
            pointer,
            &snap,
            GestureConfig::default(),
        );
        match response {
            GestureResponse::Commit(commit) => self.accept_gesture_commit(commit, optimistic, cx),
            other => {
                self.describe_gesture_response(&other);
                self.end_project_gesture(true);
                cx.notify();
            }
        }
        self.flush_project_publication(cx);
    }

    fn accept_gesture_commit(
        &mut self,
        commit: GestureCommit,
        optimistic: Option<PreviewPatch>,
        cx: &mut Context<Self>,
    ) {
        if let Some(selection) = commit.selection.clone() {
            self.apply_selection_intent(selection);
        }
        if let Some(callback) = self.callback.as_ref() {
            if commit.edit.is_some() {
                self.optimistic_preview = optimistic.map(|patch| OptimisticPreview { patch });
            }
            callback(ArrangementViewEvent::Commit(commit.clone()));
            self.status = if commit.edit.is_some() {
                "Edit sent to project command controller".into()
            } else {
                "Selection updated".into()
            };
        } else if commit.edit.is_some() {
            // A shared aggregate must never be mutated by an unbound view.
            self.status = "Edit preview complete · no project command adapter attached".into();
        } else {
            self.status = "Selection updated".into();
        }
        self.end_project_gesture(false);
        cx.notify();
    }

    fn begin_project_gesture(&mut self, primary_clip: ClipId) {
        self.end_project_gesture(true);
        let gesture = ArrangementGestureIdentity {
            editor_session: self.editor_session,
            series: self.next_gesture_series,
            primary_clip,
        };
        self.next_gesture_series = self.next_gesture_series.saturating_add(1).max(1);
        self.active_gesture_identity = Some(gesture);
        if let Some(callback) = &self.gesture_callback {
            callback(ArrangementGestureBoundary::Begin(gesture));
        }
    }

    fn end_project_gesture(&mut self, cancelled: bool) {
        let Some(gesture) = self.active_gesture_identity.take() else {
            return;
        };
        if let Some(callback) = &self.gesture_callback {
            callback(ArrangementGestureBoundary::End { gesture, cancelled });
        }
    }

    fn apply_selection_intent(&mut self, intent: SelectionIntent) {
        let mut clips = self.selection.clips.clone();
        let clear_all = matches!(intent, SelectionIntent::ClearObjects);
        match intent {
            SelectionIntent::Clips { ids, mode, .. } => {
                apply_id_selection(&mut clips, ids, mode);
            }
            SelectionIntent::Marquee {
                range,
                tracks,
                mode,
            } => {
                let ids = self
                    .editor
                    .state()
                    .clips
                    .values()
                    .filter(|clip| {
                        (tracks.is_empty() || tracks.contains(&clip.track_id))
                            && clip.placement.intersects(range)
                    })
                    .map(|clip| clip.id)
                    .collect();
                apply_id_selection(&mut clips, ids, mode);
            }
            SelectionIntent::ClearObjects => clips.clear(),
        }
        let tracks = clips
            .iter()
            .filter_map(|id| self.editor.state().clip(*id).map(|clip| clip.track_id))
            .collect();
        self.selection.clips = clips;
        self.selection.tracks = tracks;
        if clear_all {
            self.selection.clips.clear();
            self.selection.tracks.clear();
        }
        if self.callback.is_none() {
            let selection = self.selection.clone();
            self.update_editor(move |editor| editor.selection = selection);
        }
    }

    fn describe_gesture_response(&mut self, response: &GestureResponse) {
        self.status = match response {
            GestureResponse::Pressed { .. } => "Drag clip body or handles · Esc cancels".into(),
            GestureResponse::Preview(preview) if preview.diagnostics.is_empty() => {
                preview_status(preview)
            }
            GestureResponse::Preview(preview) => {
                format!("Preview refused · {:?}", preview.diagnostics[0])
            }
            GestureResponse::Refused(diagnostics) => diagnostics
                .first()
                .map(|diagnostic| format!("Gesture refused · {diagnostic:?}"))
                .unwrap_or_else(|| "Gesture refused".into()),
            GestureResponse::Cancelled => "Gesture cancelled".into(),
            GestureResponse::Commit(_) => "Gesture committed".into(),
            GestureResponse::Idle => "Arrangement ready".into(),
        };
    }

    fn edit(
        &mut self,
        result: Result<(), crate::arrangement::ArrangementError>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(()) => self.status = "Edit committed · exact source mapping preserved".into(),
            Err(error) => self.status = format!("Edit refused: {error}"),
        }
        cx.notify();
    }

    fn emit_arrangement_edit(
        &mut self,
        edit: ArrangementEdit,
        status: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(callback) = self.callback.as_ref() else {
            return false;
        };
        callback(ArrangementViewEvent::Commit(GestureCommit {
            selection: None,
            edit: Some(ArrangementEditIntent {
                expected_revision: self.expected_project_revision,
                edit,
            }),
        }));
        self.status = status.into();
        cx.notify();
        true
    }

    fn emit_phrase_plan(
        &mut self,
        plan: PhraseEditPlan,
        status: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(callback) = self.callback.as_ref().cloned() else {
            return false;
        };
        self.apply_selection_intent(plan.selection.clone());
        self.reveal_phrase_range(plan.reveal.range);
        callback(ArrangementViewEvent::Commit(GestureCommit {
            selection: Some(plan.selection),
            edit: Some(plan.intent),
        }));
        self.status = status.into();
        cx.notify();
        true
    }

    fn reveal_phrase_range(&mut self, range: FrameRange) {
        if range.start >= self.viewport.start && range.end <= self.viewport.end {
            return;
        }
        let span = self.viewport.span().max(1);
        if range.len() >= span {
            self.viewport.start = range.start;
            self.viewport.end = range.end;
        } else if range.start < self.viewport.start {
            self.viewport.start = range.start;
            self.viewport.end = Frame(range.start.0.saturating_add(span as i64));
        } else {
            self.viewport.end = range.end;
            self.viewport.start = Frame(range.end.0.saturating_sub(span as i64));
        }
        self.follow_playhead = false;
    }

    fn emit_action(
        &mut self,
        action: ArrangementAction,
        status: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(callback) = self.callback.as_ref() else {
            return false;
        };
        callback(ArrangementViewEvent::Action(ArrangementActionIntent {
            expected_revision: self.expected_project_revision,
            action,
        }));
        self.status = status.into();
        cx.notify();
        true
    }

    fn drop_target_at(
        &self,
        track: Option<(TrackId, TrackKind)>,
        position: gpui::Point<Pixels>,
        modifiers: gpui::Modifiers,
    ) -> Option<(DropTarget, DragModifiers)> {
        let bounds = self
            .timeline_bounds
            .lock()
            .ok()
            .and_then(|bounds| *bounds)?;
        let width = f64::from(f32::from(bounds.size.width));
        if width <= 0.0 {
            return None;
        }
        let fraction = f64::from(f32::from(position.x - bounds.origin.x)) / width;
        let drag_modifiers = drag_modifiers(modifiers);
        let raw = self.viewport.frame_at_fraction(fraction);
        let at = if drag_modifiers.suppress_snap {
            raw
        } else {
            let quantum = snap_frames(
                self.editor.state().sample_rate,
                self.bpm,
                self.beats_per_bar,
                self.snap,
            )
            .unwrap_or(1)
            .min(i64::MAX as u64) as i64;
            Frame(snap_frame(raw.0, quantum.max(1)))
        };
        let target = match track {
            Some((track, kind)) => DropTarget::ArrangementTrack { track, kind, at },
            None => DropTarget::ArrangementCanvas { at },
        };
        Some((target, drag_modifiers))
    }

    fn accept_drop(
        &mut self,
        payload: DragPayload,
        track: Option<(TrackId, TrackKind)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.refresh_editor_snapshot();
        let Some((target, modifiers)) =
            self.drop_target_at(track, window.mouse_position(), window.modifiers())
        else {
            self.status = "Drop refused · arrangement bounds unavailable".into();
            cx.notify();
            return;
        };
        if let Ok(mut preview) = self.drop_preview.lock() {
            *preview = None;
        }
        match interpret_drop(payload, target, modifiers) {
            Ok(intent) => {
                let label = drop_intent_label(&intent);
                if !self.emit_action(
                    ArrangementAction::Drop(intent),
                    format!("{label} sent to project command controller"),
                    cx,
                ) {
                    self.status = format!("{label} ready · no project command adapter attached");
                    cx.notify();
                }
            }
            Err(error) => {
                self.status = format!("Drop refused · {error}");
                cx.notify();
            }
        }
    }

    fn nudge_selected(&mut self, direction: i64, cx: &mut Context<Self>) {
        let quantum = self.edit_step().min(i64::MAX as u64) as i64;
        self.nudge_selected_by(direction.saturating_mul(quantum), cx);
    }

    fn nudge_selected_by(&mut self, delta_frames: i64, cx: &mut Context<Self>) {
        self.refresh_editor_snapshot();
        let Some(id) = self.selected_clip_id() else {
            self.status = "Select a clip before nudging".into();
            cx.notify();
            return;
        };
        match plan_nudge(
            self.editor.state(),
            &self.selection.clips,
            self.expected_project_revision,
            delta_frames,
        ) {
            Ok(intent) => {
                if self.emit_arrangement_edit(
                    intent.edit,
                    format!(
                        "Nudged {} clip{} through project command controller",
                        self.selection.clips.len(),
                        if self.selection.clips.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ),
                    cx,
                ) {
                    return;
                }
            }
            Err(error) => {
                self.status = format!("Nudge refused: {error}");
                cx.notify();
                return;
            }
        }
        // The detached demo editor predates aggregate batch commands. Retain
        // its useful one-clip fallback while project-backed surfaces always
        // use the exact multi-selection term planned above.
        let Some(clip) = self.editor.state().clip(id).cloned() else {
            self.status = "Selected clip disappeared".into();
            cx.notify();
            return;
        };
        let start = Frame(clip.placement.start.0.saturating_add(delta_frames));
        let result = self.mutate_editor(|editor| editor.move_clip(id, clip.track_id, start));
        self.edit(result, cx);
    }

    fn move_selected_tracks(&mut self, direction: TrackDirection, cx: &mut Context<Self>) {
        self.refresh_editor_snapshot();
        let count = self.selection.clips.len();
        match plan_move_to_adjacent_tracks(
            self.editor.state(),
            &self.selection.clips,
            self.expected_project_revision,
            direction,
        ) {
            Ok(intent) => {
                if self.emit_arrangement_edit(
                    intent.edit,
                    format!(
                        "Moved {count} clip{} one track {}",
                        if count == 1 { "" } else { "s" },
                        match direction {
                            TrackDirection::Previous => "up",
                            TrackDirection::Next => "down",
                        }
                    ),
                    cx,
                ) {
                    return;
                }
                self.status = "Vertical move needs a project command adapter".into();
            }
            Err(error) => self.status = format!("Vertical move refused: {error}"),
        }
        cx.notify();
    }

    fn navigate_selection(&mut self, navigation: SelectionNavigation, cx: &mut Context<Self>) {
        self.refresh_editor_snapshot();
        let intent =
            plan_selection_navigation(self.editor.state(), &self.selection.clips, navigation);
        self.apply_selection_intent(intent.clone());
        if let Some(callback) = &self.callback {
            callback(ArrangementViewEvent::Commit(GestureCommit {
                selection: Some(intent),
                edit: None,
            }));
        }
        self.status = match navigation {
            SelectionNavigation::All => {
                format!("Selected all {} clips", self.selection.clips.len())
            }
            SelectionNavigation::Clear => "Object selection cleared".into(),
            _ if self.selection.clips.is_empty() => "Arrangement contains no clips".into(),
            _ => "Clip focus moved in visual order".into(),
        };
        cx.notify();
    }

    fn trim_start(&mut self, cx: &mut Context<Self>) {
        self.refresh_editor_snapshot();
        let Some(anchor) = self.selected_clip_id() else {
            self.status = "Select a clip before trimming".into();
            cx.notify();
            return;
        };
        let step = self.edit_step();
        let Some(clip) = self.editor.state().clip(anchor) else {
            return;
        };
        let step = step.min(clip.placement.len().saturating_sub(1)) as i64;
        let boundary = Frame(clip.placement.start.0.saturating_add(step));
        let snap = self.snap_context();
        match plan_phrase_trim(
            self.editor.state(),
            &self.selection.clips,
            self.expected_project_revision,
            anchor,
            TrimEdge::Left,
            boundary,
            Some(&snap),
        ) {
            Ok(plan) => {
                if self.emit_phrase_plan(
                    plan,
                    format!("Trimmed {} clips as one phrase", self.selection.clips.len()),
                    cx,
                ) {
                    return;
                }
            }
            Err(error) => {
                self.status = format!("Phrase trim refused: {error}");
                cx.notify();
                return;
            }
        }
        if self.selection.clips.len() != 1 {
            self.status = "Phrase trim needs a project command adapter".into();
            cx.notify();
            return;
        }
        let result = self.mutate_editor(|editor| editor.trim_left(anchor, boundary));
        self.edit(result, cx);
    }

    fn trim_end(&mut self, cx: &mut Context<Self>) {
        self.refresh_editor_snapshot();
        let Some(anchor) = self.selected_clip_id() else {
            self.status = "Select a clip before trimming".into();
            cx.notify();
            return;
        };
        let step = self.edit_step();
        let Some(clip) = self.editor.state().clip(anchor) else {
            return;
        };
        let step = step.min(clip.placement.len().saturating_sub(1)) as i64;
        let boundary = Frame(clip.placement.end.0.saturating_sub(step));
        let snap = self.snap_context();
        match plan_phrase_trim(
            self.editor.state(),
            &self.selection.clips,
            self.expected_project_revision,
            anchor,
            TrimEdge::Right,
            boundary,
            Some(&snap),
        ) {
            Ok(plan) => {
                if self.emit_phrase_plan(
                    plan,
                    format!("Trimmed {} clips as one phrase", self.selection.clips.len()),
                    cx,
                ) {
                    return;
                }
            }
            Err(error) => {
                self.status = format!("Phrase trim refused: {error}");
                cx.notify();
                return;
            }
        }
        if self.selection.clips.len() != 1 {
            self.status = "Phrase trim needs a project command adapter".into();
            cx.notify();
            return;
        }
        let result = self.mutate_editor(|editor| editor.trim_right(anchor, boundary));
        self.edit(result, cx);
    }

    fn split_selected(&mut self, cx: &mut Context<Self>) {
        self.refresh_editor_snapshot();
        let Some(anchor) = self.selected_clip_id() else {
            self.status = "Select a clip before splitting".into();
            cx.notify();
            return;
        };
        let Some(clip) = self.editor.state().clip(anchor).cloned() else {
            return;
        };
        let midpoint = Frame(
            clip.placement
                .start
                .0
                .saturating_add((clip.placement.len() / 2) as i64),
        );
        let proposed =
            if clip.placement.contains(self.playhead) && self.playhead != clip.placement.start {
                self.playhead
            } else {
                midpoint
            };
        let snap = self.snap_context();
        match plan_phrase_split(
            self.editor.state(),
            &self.selection.clips,
            self.expected_project_revision,
            proposed,
            Some(&snap),
        ) {
            Ok(plan) => {
                if self.emit_phrase_plan(
                    plan,
                    format!("Split {} clips as one phrase", self.selection.clips.len()),
                    cx,
                ) {
                    return;
                }
            }
            Err(error) => {
                self.status = format!("Phrase split refused: {error}");
                cx.notify();
                return;
            }
        }
        if self.selection.clips.len() != 1 {
            self.status = "Phrase split needs a project command adapter".into();
            cx.notify();
            return;
        }
        match self.mutate_editor(|editor| {
            let right = editor.split_clip(anchor, proposed)?;
            editor.selection.clips.clear();
            editor.selection.clips.insert(right);
            Ok((right, proposed.0))
        }) {
            Ok((_, at)) => {
                self.status = format!("Split at sample {} · selected right clip", grouped_i64(at));
            }
            Err(error) => self.status = format!("Split refused: {error}"),
        }
        cx.notify();
    }

    fn duplicate_selected(&mut self, cx: &mut Context<Self>) {
        self.refresh_editor_snapshot();
        let Some(id) = self.selected_clip_id() else {
            self.status = "Select a clip before duplicating".into();
            cx.notify();
            return;
        };
        let step = self.edit_step() as i64;
        match plan_duplicate_after(
            self.editor.state(),
            &self.selection.clips,
            self.expected_project_revision,
            step.max(1) as u64,
        ) {
            Ok(intent) => {
                if self.emit_arrangement_edit(
                    intent.edit,
                    format!(
                        "Duplicated {} clip{} as one phrase",
                        self.selection.clips.len(),
                        if self.selection.clips.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ),
                    cx,
                ) {
                    return;
                }
            }
            Err(error) => {
                self.status = format!("Duplicate refused: {error}");
                cx.notify();
                return;
            }
        }
        let Some(clip) = self.editor.state().clip(id).cloned() else {
            return;
        };
        let start = Frame(snap_frame(clip.placement.end.0, step));
        match self.mutate_editor(|editor| {
            let copy = editor.duplicate_clip(id, start)?;
            editor.selection.clips.clear();
            editor.selection.clips.insert(copy);
            Ok(copy)
        }) {
            Ok(_) => {
                self.status = format!("Duplicated clip #{} · shared content identity", id.get());
            }
            Err(error) => self.status = format!("Duplicate refused: {error}"),
        }
        cx.notify();
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        self.refresh_editor_snapshot();
        let Some(id) = self.selected_clip_id() else {
            self.status = "Nothing selected to delete".into();
            cx.notify();
            return;
        };
        if self.emit_action(
            ArrangementAction::DeleteClips(self.selection.clips.clone()),
            "Delete sent to project command controller",
            cx,
        ) {
            return;
        }
        let result = self.mutate_editor(|editor| editor.delete_clip(id));
        self.edit(result, cx);
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        self.refresh_editor_snapshot();
        if self.emit_action(
            ArrangementAction::Undo,
            "Undo sent to project command controller",
            cx,
        ) {
            return;
        }
        let label = self.editor.undo_label().unwrap_or("edit").to_owned();
        match self.mutate_editor(ArrangementEditor::undo) {
            Ok(Some(_)) => self.status = format!("Undid {label}"),
            Ok(None) => self.status = "Nothing to undo".into(),
            Err(error) => self.status = format!("Undo refused: {error}"),
        }
        cx.notify();
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        self.refresh_editor_snapshot();
        if self.emit_action(
            ArrangementAction::Redo,
            "Redo sent to project command controller",
            cx,
        ) {
            return;
        }
        let label = self.editor.redo_label().unwrap_or("edit").to_owned();
        match self.mutate_editor(ArrangementEditor::redo) {
            Ok(Some(_)) => self.status = format!("Redid {label}"),
            Ok(None) => self.status = "Nothing to redo".into(),
            Err(error) => self.status = format!("Redo refused: {error}"),
        }
        cx.notify();
    }

    fn edit_step(&self) -> u64 {
        snap_frames(
            self.editor.state().sample_rate,
            self.bpm,
            self.beats_per_bar,
            self.snap,
        )
        .unwrap_or(1)
    }

    fn fit(&mut self, cx: &mut Context<Self>) {
        self.viewport = fit_viewport(&self.editor, self.bpm, self.beats_per_bar);
        self.follow_playhead = false;
        self.status = "Fit project extent".into();
        cx.notify();
    }

    fn zoom(&mut self, scale: f64, cx: &mut Context<Self>) {
        let center = self.viewport.frame_at_fraction(0.5);
        self.viewport.zoom_around(center, scale);
        self.follow_playhead = false;
        self.status = format!(
            "Visible span · {} samples",
            grouped_u64(self.viewport.span())
        );
        cx.notify();
    }

    fn pan(&mut self, fraction: f64, cx: &mut Context<Self>) {
        self.viewport.pan(fraction);
        self.follow_playhead = false;
        self.status = format!("Panned to sample {}", grouped_i64(self.viewport.start.0));
        cx.notify();
    }

    fn cycle_snap(&mut self, cx: &mut Context<Self>) {
        self.snap = self.snap.next();
        self.status = self.snap.label().into();
        cx.notify();
    }

    fn add_track(&mut self, kind: TrackKind, cx: &mut Context<Self>) {
        if self.emit_action(
            ArrangementAction::CreateTrack { kind },
            "Track creation sent to project command controller",
            cx,
        ) {
            return;
        }
        match self.mutate_editor(|editor| {
            let number = editor.state().track_order.len() + 1;
            editor.create_track(format!("{} {number}", track_kind_name(kind)), kind)
        }) {
            Ok(track) => {
                self.status = format!("Created {} track #{}", track_kind_name(kind), track.get())
            }
            Err(error) => self.status = format!("Create track refused: {error}"),
        }
        cx.notify();
    }

    fn on_undo(&mut self, _: &UndoArrangement, _: &mut Window, cx: &mut Context<Self>) {
        self.undo(cx);
    }
    fn on_redo(&mut self, _: &RedoArrangement, _: &mut Window, cx: &mut Context<Self>) {
        self.redo(cx);
    }
    fn on_duplicate(&mut self, _: &DuplicateClip, _: &mut Window, cx: &mut Context<Self>) {
        self.duplicate_selected(cx);
    }
    fn on_delete(&mut self, _: &DeleteClip, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_selected(cx);
    }
    fn on_split(&mut self, _: &SplitClip, _: &mut Window, cx: &mut Context<Self>) {
        self.split_selected(cx);
    }
    fn on_select_all(
        &mut self,
        _: &SelectAllArrangementClips,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate_selection(SelectionNavigation::All, cx);
    }
    fn on_select_previous(
        &mut self,
        _: &SelectPreviousArrangementClip,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate_selection(SelectionNavigation::Previous, cx);
    }
    fn on_select_next(
        &mut self,
        _: &SelectNextArrangementClip,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate_selection(SelectionNavigation::Next, cx);
    }
    fn on_nudge_left(&mut self, _: &NudgeClipLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.nudge_selected(-1, cx);
    }
    fn on_nudge_right(&mut self, _: &NudgeClipRight, _: &mut Window, cx: &mut Context<Self>) {
        self.nudge_selected(1, cx);
    }
    fn on_nudge_fine_left(
        &mut self,
        _: &NudgeClipFineLeft,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.nudge_selected_by(-1, cx);
    }
    fn on_nudge_fine_right(
        &mut self,
        _: &NudgeClipFineRight,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.nudge_selected_by(1, cx);
    }
    fn on_move_track_up(&mut self, _: &MoveClipTrackUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selected_tracks(TrackDirection::Previous, cx);
    }
    fn on_move_track_down(
        &mut self,
        _: &MoveClipTrackDown,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_selected_tracks(TrackDirection::Next, cx);
    }
    fn on_trim_start(&mut self, _: &TrimClipStart, _: &mut Window, cx: &mut Context<Self>) {
        self.trim_start(cx);
    }
    fn on_trim_end(&mut self, _: &TrimClipEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.trim_end(cx);
    }
    fn on_toggle_loop(
        &mut self,
        _: &ToggleArrangementLoop,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_loop_from_time_selection(cx);
    }
    fn on_zoom_in(&mut self, _: &ZoomArrangementIn, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom(0.5, cx);
    }
    fn on_zoom_out(&mut self, _: &ZoomArrangementOut, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom(2.0, cx);
    }
    fn on_pan_left(&mut self, _: &PanArrangementLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.pan(-0.35, cx);
    }
    fn on_pan_right(&mut self, _: &PanArrangementRight, _: &mut Window, cx: &mut Context<Self>) {
        self.pan(0.35, cx);
    }
    fn on_fit(&mut self, _: &FitArrangement, _: &mut Window, cx: &mut Context<Self>) {
        self.fit(cx);
    }
    fn on_snap(&mut self, _: &CycleArrangementSnap, _: &mut Window, cx: &mut Context<Self>) {
        self.cycle_snap(cx);
    }
    fn on_cancel(&mut self, _: &CancelArrangementGesture, _: &mut Window, cx: &mut Context<Self>) {
        let ruler_cancelled = self.cancel_ruler_gesture();
        if !matches!(self.interaction.cancel(), GestureResponse::Idle) {
            self.end_project_gesture(true);
            self.optimistic_preview = None;
            self.status = "Gesture cancelled".into();
            cx.notify();
        } else if ruler_cancelled {
            self.status = "Ruler gesture cancelled".into();
            cx.notify();
        } else if !self.selection.clips.is_empty() {
            self.navigate_selection(SelectionNavigation::Clear, cx);
        }
        self.flush_project_publication(cx);
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected_clip_id().is_some();
        let undo = self.editor.undo_label().map(str::to_owned);
        let redo = self.editor.redo_label().map(str::to_owned);
        div()
            .h(px(50.0))
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .px_3()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(
                tool_button("arr-undo", "↶", undo.is_some())
                    .on_click(cx.listener(|this, _, _, cx| this.undo(cx))),
            )
            .child(
                tool_button("arr-redo", "↷", redo.is_some())
                    .on_click(cx.listener(|this, _, _, cx| this.redo(cx))),
            )
            .child(separator())
            .child(
                tool_button("arr-move-left", "← Nudge", selected)
                    .on_click(cx.listener(|this, _, _, cx| this.nudge_selected(-1, cx))),
            )
            .child(
                tool_button("arr-move-right", "Nudge →", selected)
                    .on_click(cx.listener(|this, _, _, cx| this.nudge_selected(1, cx))),
            )
            .child(
                tool_button("arr-trim-left", "Trim L", selected)
                    .on_click(cx.listener(|this, _, _, cx| this.trim_start(cx))),
            )
            .child(
                tool_button("arr-trim-right", "Trim R", selected)
                    .on_click(cx.listener(|this, _, _, cx| this.trim_end(cx))),
            )
            .child(
                tool_button("arr-split", "Split", selected)
                    .on_click(cx.listener(|this, _, _, cx| this.split_selected(cx))),
            )
            .child(
                tool_button("arr-duplicate", "Duplicate", selected)
                    .on_click(cx.listener(|this, _, _, cx| this.duplicate_selected(cx))),
            )
            .child(
                tool_button("arr-delete", "Delete", selected)
                    .on_click(cx.listener(|this, _, _, cx| this.delete_selected(cx))),
            )
            .child(separator())
            .child(
                tool_button("arr-add-audio", "+ Audio", true)
                    .on_click(cx.listener(|this, _, _, cx| this.add_track(TrackKind::Audio, cx))),
            )
            .child(
                tool_button("arr-add-pattern", "+ Pattern", true)
                    .on_click(cx.listener(|this, _, _, cx| this.add_track(TrackKind::Pattern, cx))),
            )
            .child(
                tool_button("arr-add-automation", "+ Auto", true).on_click(
                    cx.listener(|this, _, _, cx| this.add_track(TrackKind::Automation, cx)),
                ),
            )
            .child(div().flex_1())
            .child(
                tool_button(
                    "arr-loop",
                    if self.loop_range.is_some() {
                        "LOOP ON"
                    } else {
                        "LOOP OFF"
                    },
                    self.loop_range.is_some() || self.selection.time.is_some(),
                )
                .text_color(rgb(if self.loop_range.is_some() {
                    AMBER
                } else {
                    MUTED
                }))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_loop_from_time_selection(cx))),
            )
            .child(
                tool_button(
                    "arr-follow",
                    if self.follow_playhead {
                        "FOLLOW ON"
                    } else {
                        "FOLLOW OFF"
                    },
                    true,
                )
                .text_color(rgb(if self.follow_playhead { LIME } else { MUTED }))
                .on_click(
                    cx.listener(|this, _, _, cx| {
                        this.set_follow_playhead(!this.follow_playhead, cx)
                    }),
                ),
            )
            .child(
                tool_button("arr-snap", self.snap.label(), true)
                    .text_color(rgb(CYAN))
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_snap(cx))),
            )
            .child(
                tool_button("arr-zoom-out", "−", true)
                    .on_click(cx.listener(|this, _, _, cx| this.zoom(2.0, cx))),
            )
            .child(
                tool_button("arr-fit", "FIT", true)
                    .on_click(cx.listener(|this, _, _, cx| this.fit(cx))),
            )
            .child(
                tool_button("arr-zoom-in", "+", true)
                    .on_click(cx.listener(|this, _, _, cx| this.zoom(0.5, cx))),
            )
    }

    fn render_ruler(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ticks = tempo_ruler_ticks(&self.tempo_map, self.viewport);
        let time_selection = self
            .selection
            .time
            .and_then(|range| visible_clip(range, self.viewport));
        let loop_selection = self
            .loop_range
            .and_then(|range| visible_clip(range, self.viewport));
        div()
            .h(px(RULER_HEIGHT))
            .flex_none()
            .flex()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL_ALT))
            .child(
                div()
                    .w(px(TRACK_GUTTER))
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
                            .text_color(rgb(CYAN))
                            .child(format!("{:.1} BPM", self.bpm)),
                    ),
            )
            .child(
                div()
                    .relative()
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .children(ticks.into_iter().map(|tick| {
                        let fraction = self.viewport.fraction(tick.frame);
                        div()
                            .absolute()
                            .left(relative(fraction))
                            .top_0()
                            .bottom_0()
                            .border_l_1()
                            .border_color(if tick.major { rgb(CYAN) } else { rgb(BORDER) })
                            .pl_1()
                            .pt_1()
                            .text_xs()
                            .text_color(if tick.major { rgb(TEXT) } else { rgb(DIM) })
                            .child(tick.label)
                    }))
                    .when_some(time_selection, |ruler, selection| {
                        ruler.child(
                            div()
                                .absolute()
                                .left(relative(selection.left))
                                .top_0()
                                .w(relative(selection.width.max(0.001)))
                                .h_full()
                                .border_l_1()
                                .border_r_1()
                                .border_color(rgba(0x50d8d7cc))
                                .bg(rgba(0x50d8d728)),
                        )
                    })
                    .when_some(loop_selection, |ruler, selection| {
                        let start_handle = selection.left > 0.0;
                        let end = selection.left + selection.width;
                        let end_handle = end < 1.0;
                        ruler
                            .child(
                                div()
                                    .absolute()
                                    .left(relative(selection.left))
                                    .bottom_0()
                                    .w(relative(selection.width.max(0.001)))
                                    .h(px(RULER_LOOP_STRIP_HEIGHT))
                                    .border_t_1()
                                    .border_color(rgba(0xf6b760ee))
                                    .cursor_grab()
                                    .bg(rgba(0xf6b76044)),
                            )
                            .when(start_handle, |ruler| {
                                ruler.child(
                                    div()
                                        .absolute()
                                        .left(relative(selection.left))
                                        .bottom_0()
                                        .w(px(5.0))
                                        .h(px(RULER_LOOP_STRIP_HEIGHT + 4.0))
                                        .cursor_ew_resize()
                                        .bg(rgb(AMBER)),
                                )
                            })
                            .when(end_handle, |ruler| {
                                ruler.child(
                                    div()
                                        .absolute()
                                        .left(relative(end))
                                        .bottom_0()
                                        .w(px(5.0))
                                        .h(px(RULER_LOOP_STRIP_HEIGHT + 4.0))
                                        .cursor_ew_resize()
                                        .bg(rgb(AMBER)),
                                )
                            })
                    })
                    .when(
                        self.viewport.start <= self.playhead && self.playhead < self.viewport.end,
                        |ruler| {
                            ruler.child(
                                div()
                                    .absolute()
                                    .left(relative(self.viewport.fraction(self.playhead)))
                                    .top_0()
                                    .bottom_0()
                                    .w(px(2.0))
                                    .bg(rgb(MAGENTA)),
                            )
                        },
                    )
                    .child(
                        div()
                            .absolute()
                            .right_2()
                            .bottom_1()
                            .text_xs()
                            .text_color(rgb(DIM))
                            .child("BAR.BEAT · EXACT SAMPLE"),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.begin_ruler_selection(event, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                        this.drag_ruler_selection(event, cx)
                    }))
                    .capture_any_mouse_up(cx.listener(|this, event: &MouseUpEvent, _, cx| {
                        if event.button == MouseButton::Left {
                            this.end_ruler_selection(event, cx);
                            cx.stop_propagation();
                        }
                    })),
            )
    }

    fn render_track(&self, track: &Track, cx: &mut Context<Self>) -> impl IntoElement {
        let color = track_color(track.kind);
        let preview = self.interaction.preview().or_else(|| {
            self.optimistic_preview
                .as_ref()
                .map(|pending| &pending.patch)
        });
        let clips: Vec<_> = self
            .editor
            .state()
            .clips
            .values()
            .filter_map(|clip| {
                let visual = previewed_clip(clip, preview);
                (visual.track == track.id)
                    .then(|| visible_clip(visual.placement, self.viewport))
                    .flatten()
                    .map(|visible| (clip.clone(), visual, visible))
            })
            .collect();
        let ticks = tempo_ruler_ticks(&self.tempo_map, self.viewport);
        let lane_bounds = self.track_bounds.clone();
        let track_id = track.id;
        let marquee = preview
            .and_then(|preview| preview.marquee)
            .filter(|marquee| marquee_tracks_include(self.editor.state(), *marquee, track.id))
            .and_then(|marquee| marquee.range())
            .and_then(|range| visible_clip(range, self.viewport));
        let time_selection = self
            .selection
            .time
            .and_then(|range| visible_clip(range, self.viewport));
        let loop_selection = self
            .loop_range
            .and_then(|range| visible_clip(range, self.viewport));
        let snap_fraction = preview
            .and_then(|preview| preview.snap)
            .map(|snap| self.viewport.fraction(snap.snapped))
            .filter(|fraction| (0.0..=1.0).contains(fraction));
        let playhead_fraction = (self.viewport.start <= self.playhead
            && self.playhead < self.viewport.end)
            .then(|| self.viewport.fraction(self.playhead));
        let drop_preview = self
            .drop_preview
            .lock()
            .ok()
            .and_then(|preview| preview.clone())
            .filter(|preview| {
                matches!(
                    preview.target,
                    DropTarget::ArrangementTrack { track: target, .. } if target == track.id
                )
            });
        let drop_placement = drop_preview
            .as_ref()
            .and_then(|preview| preview.placement)
            .and_then(|range| visible_clip(range, self.viewport));
        let drop_compatible = drop_preview
            .as_ref()
            .is_some_and(|preview| preview.intent.is_ok());
        let drop_anchor_fraction = drop_preview
            .as_ref()
            .and_then(|preview| match preview.target {
                DropTarget::ArrangementTrack { at, .. }
                    if self.viewport.start <= at && at < self.viewport.end =>
                {
                    Some(self.viewport.fraction(at))
                }
                _ => None,
            });
        let track_drop = Some((track.id, track.kind));
        let snap_quantum = snap_frames(
            self.editor.state().sample_rate,
            self.bpm,
            self.beats_per_bar,
            self.snap,
        );
        let preview_store = Arc::clone(&self.drop_preview);
        let preview_resolver = self.preview_resolver.clone();
        let timeline_bounds = Arc::clone(&self.timeline_bounds);
        let viewport = self.viewport;
        let project_sample_rate = self.editor.state().sample_rate;
        div()
            .h(px(TRACK_HEIGHT))
            .flex_none()
            .flex()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(track_header(track, color))
            .child(
                div()
                    .relative()
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .bg(rgb(BACKGROUND))
                    .drag_over::<AssetDrag>({
                        let preview_store = Arc::clone(&preview_store);
                        let preview_resolver = preview_resolver.clone();
                        let timeline_bounds = Arc::clone(&timeline_bounds);
                        move |style, source, window, cx| {
                            let compatible = update_drop_preview(
                                DragPayload::Asset(*source),
                                track_drop,
                                viewport,
                                &timeline_bounds,
                                snap_quantum,
                                project_sample_rate,
                                preview_resolver.as_ref(),
                                &preview_store,
                                window,
                                cx,
                            );
                            drop_hover_style(style, compatible, color)
                        }
                    })
                    .drag_over::<crate::sequencer::PatternId>({
                        let preview_store = Arc::clone(&preview_store);
                        let preview_resolver = preview_resolver.clone();
                        let timeline_bounds = Arc::clone(&timeline_bounds);
                        move |style, pattern, window, cx| {
                            let compatible = update_drop_preview(
                                DragPayload::Pattern(*pattern),
                                track_drop,
                                viewport,
                                &timeline_bounds,
                                snap_quantum,
                                project_sample_rate,
                                preview_resolver.as_ref(),
                                &preview_store,
                                window,
                                cx,
                            );
                            drop_hover_style(style, compatible, color)
                        }
                    })
                    .drag_over::<DragPayload>({
                        let preview_store = Arc::clone(&preview_store);
                        let preview_resolver = preview_resolver.clone();
                        let timeline_bounds = Arc::clone(&timeline_bounds);
                        move |style, payload, window, cx| {
                            let compatible = update_drop_preview(
                                payload.clone(),
                                track_drop,
                                viewport,
                                &timeline_bounds,
                                snap_quantum,
                                project_sample_rate,
                                preview_resolver.as_ref(),
                                &preview_store,
                                window,
                                cx,
                            );
                            drop_hover_style(style, compatible, color)
                        }
                    })
                    // Bounds collection is painted first so its transparent
                    // canvas can never sit above clip hit targets.
                    .child(
                        canvas(
                            move |bounds, _, _| {
                                if let Ok(mut lanes) = lane_bounds.lock() {
                                    lanes.insert(track_id, bounds);
                                }
                                bounds
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .left_0()
                        .right_0()
                        .top_0()
                        .bottom_0(),
                    )
                    .children(ticks.into_iter().map(|tick| {
                        div()
                            .absolute()
                            .left(relative(self.viewport.fraction(tick.frame)))
                            .top_0()
                            .bottom_0()
                            .border_l_1()
                            .border_color(if tick.major {
                                rgba(0x50d8d724)
                            } else {
                                rgba(0xffffff0d)
                            })
                    }))
                    .when_some(time_selection, |lane, selection| {
                        lane.child(
                            div()
                                .absolute()
                                .left(relative(selection.left))
                                .top_0()
                                .w(relative(selection.width.max(0.001)))
                                .h_full()
                                .bg(rgba(0x50d8d70d)),
                        )
                    })
                    .when_some(loop_selection, |lane, selection| {
                        lane.child(
                            div()
                                .absolute()
                                .left(relative(selection.left))
                                .top_0()
                                .w(relative(selection.width.max(0.001)))
                                .h(px(2.0))
                                .bg(rgba(0xf6b760cc)),
                        )
                    })
                    .when_some(marquee, |lane, marquee| {
                        lane.child(
                            div()
                                .absolute()
                                .left(relative(marquee.left))
                                .top_0()
                                .w(relative(marquee.width.max(0.001)))
                                .h_full()
                                .border_1()
                                .border_color(rgba(0x50d8d7bb))
                                .bg(rgba(0x50d8d725)),
                        )
                    })
                    .when_some(drop_placement, |lane, preview| {
                        lane.child(drop_preview_block(
                            preview,
                            drop_compatible,
                            drop_preview
                                .as_ref()
                                .map(|preview| preview.label.clone())
                                .unwrap_or_default(),
                        ))
                    })
                    .when_some(drop_anchor_fraction, |lane, fraction| {
                        lane.child(
                            div()
                                .absolute()
                                .left(relative(fraction))
                                .top_0()
                                .bottom_0()
                                .w(px(2.0))
                                .bg(rgba(if drop_compatible {
                                    0x50d8d7dd
                                } else {
                                    0xf172b6dd
                                })),
                        )
                    })
                    .children(clips.into_iter().map(|(clip, visual, visible)| {
                        let id = clip.id;
                        let selected = self.selection.clips.contains(&id);
                        clip_block(
                            clip,
                            visual,
                            visible,
                            selected,
                            color,
                            self.viewport,
                            self.waveform_provider.clone(),
                            self.preview_resolver.clone(),
                            Arc::clone(&self.waveform_cache),
                        )
                    }))
                    .when_some(playhead_fraction, |lane, fraction| {
                        lane.child(
                            div()
                                .absolute()
                                .left(relative(fraction))
                                .top_0()
                                .bottom_0()
                                .w(px(1.0))
                                .bg(rgba(if self.transport_playing {
                                    0xf172b6dd
                                } else {
                                    0xf172b688
                                })),
                        )
                    })
                    .when_some(snap_fraction, |lane, fraction| {
                        lane.child(
                            div()
                                .absolute()
                                .left(relative(fraction))
                                .top_0()
                                .bottom_0()
                                .w(px(1.0))
                                .bg(rgba(0xf6b760ee)),
                        )
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            this.begin_arrangement_pointer(event, window, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                        this.drag_arrangement_pointer(event, cx)
                    }))
                    .capture_any_mouse_up(cx.listener(|this, event: &MouseUpEvent, _, cx| {
                        this.end_arrangement_pointer(event, cx)
                    }))
                    .on_drop(cx.listener(move |this, source: &AssetDrag, window, cx| {
                        this.accept_drop(DragPayload::Asset(*source), track_drop, window, cx);
                        cx.stop_propagation();
                    }))
                    .on_drop(cx.listener(
                        move |this, pattern: &crate::sequencer::PatternId, window, cx| {
                            this.accept_drop(
                                DragPayload::Pattern(*pattern),
                                track_drop,
                                window,
                                cx,
                            );
                            cx.stop_propagation();
                        },
                    ))
                    .on_drop(cx.listener(move |this, payload: &DragPayload, window, cx| {
                        this.accept_drop(payload.clone(), track_drop, window, cx);
                        cx.stop_propagation();
                    })),
            )
    }

    fn render_inspector(&self) -> impl IntoElement {
        let selected = self
            .selected_clip_id()
            .and_then(|id| self.editor.state().clip(id));
        let content = selected.map(inspector_content);
        div()
            .w(px(292.0))
            .flex_none()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL_ALT))
            .p_3()
            .gap_2()
            .child(section_label("CLIP INSPECTOR"))
            .when_some(selected, |panel, clip| {
                panel
                    .child(div().text_lg().text_color(rgb(track_color(clip.content.kind()))).child(clip.name.clone()))
                    .child(inspector_metric("CLIP ID", format!("#{}", clip.id.get()), TEXT))
                    .child(inspector_metric("TRACK", format!("#{} · {}", clip.track_id.get(), track_kind_name(clip.content.kind())), TEXT))
                    .child(inspector_metric("PLACEMENT", format!("{} → {}", grouped_i64(clip.placement.start.0), grouped_i64(clip.placement.end.0)), CYAN))
                    .child(inspector_metric("LENGTH", format!("{} frames", grouped_u64(clip.placement.len())), CYAN))
                    .child(inspector_metric("GAIN", format!("{:+.2} dB", clip.gain_db), AMBER))
                    .child(div().mt_2().text_xs().text_color(rgb(DIM)).child(
                        "Body drag moves · edges trim · top corners fade\nOption+upper drag duplicates · Option+lower drag slips\nControl+right edge stretches · Shift fine · Cmd unsnapped",
                    ))
            })
            .when_some(content, |panel, content| {
                panel
                    .child(section_label("SOURCE MAPPING"))
                    .child(div().p_2().rounded_sm().bg(rgb(PANEL)).border_1().border_color(rgb(BORDER)).text_xs().text_color(rgb(TEXT)).child(content.mapping))
                    .child(section_label("PLAYBACK CONTRACT"))
                    .child(div().text_xs().text_color(rgb(MUTED)).child(content.transform))
                    .child(
                        div()
                            .mt_2()
                            .p_2()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(AMBER))
                            .bg(rgba(0xf6b76012))
                            .text_xs()
                            .text_color(rgb(AMBER))
                            .child(content.caveat),
                    )
            })
            .when(selected.is_none(), |panel| {
                panel
                    .child(div().mt_3().text_color(rgb(MUTED)).child("Select a clip to inspect exact timeline placement, reusable content identity, and source-frame provenance."))
                    .child(div().mt_2().text_xs().text_color(rgb(DIM)).child("Drag edges to trim · Control-drag right edge to stretch\nOption-drag lower audio body to slip · Option-drag upper body to duplicate\n← / → nudge · ⌘E split · Delete remove · ⌘Z undo"))
            })
            .child(div().flex_1())
            .child(section_label("PROJECT TRUTH"))
            .child(div().text_xs().text_color(rgb(DIM)).child(format!(
                "{} Hz · revision {}{}",
                self.editor.state().sample_rate,
                self.editor.revision(),
                if self.editor.is_dirty() { " · MODIFIED" } else { " · SAVED" }
            )))
    }
}

impl Focusable for ArrangementView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ArrangementView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Take the snapshot before creating any elements, so no mutex guard can
        // leak into GPUI's layout or paint work.
        self.flush_project_publication(cx);
        self.refresh_editor_snapshot();
        if !cx.has_active_drag() {
            if let Ok(mut preview) = self.drop_preview.lock() {
                *preview = None;
            }
        }
        if self.focus_subscription.is_none() {
            let focus = self.focus_handle.clone();
            self.focus_subscription = Some(cx.on_focus_out(&focus, window, |this, _, _, cx| {
                let ruler_cancelled = this.cancel_ruler_gesture();
                if !matches!(this.interaction.cancel(), GestureResponse::Idle) {
                    this.end_project_gesture(true);
                    this.status = "Gesture cancelled when arrangement lost focus".into();
                    cx.notify();
                } else if ruler_cancelled {
                    this.status = "Ruler gesture cancelled when arrangement lost focus".into();
                    cx.notify();
                }
                this.flush_project_publication(cx);
                if let Ok(mut preview) = this.drop_preview.lock() {
                    *preview = None;
                }
            }));
        }
        let bounds = self.timeline_bounds.clone();
        let tracks: Vec<_> = self
            .editor
            .state()
            .track_order
            .iter()
            .filter_map(|id| self.editor.state().track(*id).cloned())
            .collect();
        let canvas_preview_store = Arc::clone(&self.drop_preview);
        let canvas_preview_resolver = self.preview_resolver.clone();
        let canvas_timeline_bounds = Arc::clone(&self.timeline_bounds);
        let canvas_track_bounds = Arc::clone(&self.track_bounds);
        let canvas_viewport = self.viewport;
        let canvas_snap_quantum = snap_frames(
            self.editor.state().sample_rate,
            self.bpm,
            self.beats_per_bar,
            self.snap,
        );
        let canvas_sample_rate = self.editor.state().sample_rate;
        let canvas_drop_preview = self
            .drop_preview
            .lock()
            .ok()
            .and_then(|preview| preview.clone())
            .filter(|preview| matches!(preview.target, DropTarget::ArrangementCanvas { .. }));
        div()
            .key_context("AudecArrangement")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_undo))
            .on_action(cx.listener(Self::on_redo))
            .on_action(cx.listener(Self::on_duplicate))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_split))
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_select_previous))
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_nudge_left))
            .on_action(cx.listener(Self::on_nudge_right))
            .on_action(cx.listener(Self::on_nudge_fine_left))
            .on_action(cx.listener(Self::on_nudge_fine_right))
            .on_action(cx.listener(Self::on_move_track_up))
            .on_action(cx.listener(Self::on_move_track_down))
            .on_action(cx.listener(Self::on_trim_start))
            .on_action(cx.listener(Self::on_trim_end))
            .on_action(cx.listener(Self::on_toggle_loop))
            .on_action(cx.listener(Self::on_zoom_in))
            .on_action(cx.listener(Self::on_zoom_out))
            .on_action(cx.listener(Self::on_pan_left))
            .on_action(cx.listener(Self::on_pan_right))
            .on_action(cx.listener(Self::on_fit))
            .on_action(cx.listener(Self::on_snap))
            .on_action(cx.listener(Self::on_cancel))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .text_sm()
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .overflow_hidden()
                            .child(
                                canvas(
                                    move |canvas_bounds, _, _| {
                                        let content = Bounds::new(
                                            point(
                                                canvas_bounds.origin.x + px(TRACK_GUTTER),
                                                canvas_bounds.origin.y,
                                            ),
                                            gpui::size(
                                                (canvas_bounds.size.width - px(TRACK_GUTTER))
                                                    .max(px(1.0)),
                                                canvas_bounds.size.height,
                                            ),
                                        );
                                        *bounds.lock().unwrap() = Some(content);
                                        canvas_bounds
                                    },
                                    |_, _, _, _| {},
                                )
                                .absolute()
                                .left_0()
                                .right_0()
                                .top_0()
                                .bottom_0(),
                            )
                            .on_scroll_wheel(cx.listener(
                                |this, event: &ScrollWheelEvent, window, cx| {
                                    let delta = event.delta.pixel_delta(window.line_height());
                                    let mut handled = false;
                                    if event.modifiers.secondary() || event.modifiers.control {
                                        let wheel = if delta.y.abs() >= delta.x.abs() {
                                            delta.y
                                        } else {
                                            delta.x
                                        };
                                        let amount = f64::from(wheel / px(180.0));
                                        if amount.abs() > 0.0001 {
                                            let anchor = this
                                                .timeline_bounds
                                                .lock()
                                                .unwrap()
                                                .and_then(|bounds| {
                                                    bounds.contains(&event.position).then(|| {
                                                        let fraction = f64::from(
                                                            (event.position.x - bounds.origin.x)
                                                                / bounds.size.width,
                                                        );
                                                        this.viewport.frame_at_fraction(fraction)
                                                    })
                                                })
                                                .unwrap_or_else(|| {
                                                    this.viewport.frame_at_fraction(0.5)
                                                });
                                            this.viewport.zoom_around(anchor, amount.exp());
                                            this.follow_playhead = false;
                                            this.status = format!(
                                                "Zoom · {} samples visible",
                                                grouped_u64(this.viewport.span())
                                            );
                                            handled = true;
                                            cx.notify();
                                        }
                                    } else if event.modifiers.shift || delta.x.abs() > px(0.01) {
                                        let wheel = if delta.x.abs() > px(0.01) {
                                            delta.x
                                        } else {
                                            delta.y
                                        };
                                        let amount = -f64::from(wheel / px(520.0));
                                        if amount.abs() > 0.0001 {
                                            this.viewport.pan(amount);
                                            this.follow_playhead = false;
                                            this.status = format!(
                                                "Pan · starts at sample {}",
                                                grouped_i64(this.viewport.start.0)
                                            );
                                            handled = true;
                                            cx.notify();
                                        }
                                    }
                                    // Unmodified vertical wheels remain owned by
                                    // the track scroller. Timeline pan is thus
                                    // independent rather than stealing track
                                    // navigation or a global analysis viewport.
                                    if handled {
                                        cx.stop_propagation();
                                    }
                                },
                            ))
                            .child(self.render_ruler(cx))
                            .child(
                                div()
                                    .id("arrangement-track-scroll")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .drag_over::<AssetDrag>({
                                        let preview_store = Arc::clone(&canvas_preview_store);
                                        let preview_resolver = canvas_preview_resolver.clone();
                                        let timeline_bounds = Arc::clone(&canvas_timeline_bounds);
                                        let track_bounds = Arc::clone(&canvas_track_bounds);
                                        move |style, source, window, cx| {
                                            if point_over_any_track(
                                                &track_bounds,
                                                window.mouse_position(),
                                            ) {
                                                return style;
                                            }
                                            let compatible = update_drop_preview(
                                                DragPayload::Asset(*source),
                                                None,
                                                canvas_viewport,
                                                &timeline_bounds,
                                                canvas_snap_quantum,
                                                canvas_sample_rate,
                                                preview_resolver.as_ref(),
                                                &preview_store,
                                                window,
                                                cx,
                                            );
                                            drop_hover_style(style, compatible, CYAN)
                                        }
                                    })
                                    .drag_over::<crate::sequencer::PatternId>({
                                        let preview_store = Arc::clone(&canvas_preview_store);
                                        let preview_resolver = canvas_preview_resolver.clone();
                                        let timeline_bounds = Arc::clone(&canvas_timeline_bounds);
                                        let track_bounds = Arc::clone(&canvas_track_bounds);
                                        move |style, pattern, window, cx| {
                                            if point_over_any_track(
                                                &track_bounds,
                                                window.mouse_position(),
                                            ) {
                                                return style;
                                            }
                                            let compatible = update_drop_preview(
                                                DragPayload::Pattern(*pattern),
                                                None,
                                                canvas_viewport,
                                                &timeline_bounds,
                                                canvas_snap_quantum,
                                                canvas_sample_rate,
                                                preview_resolver.as_ref(),
                                                &preview_store,
                                                window,
                                                cx,
                                            );
                                            drop_hover_style(style, compatible, CYAN)
                                        }
                                    })
                                    .drag_over::<DragPayload>({
                                        let preview_store = Arc::clone(&canvas_preview_store);
                                        let preview_resolver = canvas_preview_resolver.clone();
                                        let timeline_bounds = Arc::clone(&canvas_timeline_bounds);
                                        let track_bounds = Arc::clone(&canvas_track_bounds);
                                        move |style, payload, window, cx| {
                                            if point_over_any_track(
                                                &track_bounds,
                                                window.mouse_position(),
                                            ) {
                                                return style;
                                            }
                                            let compatible = update_drop_preview(
                                                payload.clone(),
                                                None,
                                                canvas_viewport,
                                                &timeline_bounds,
                                                canvas_snap_quantum,
                                                canvas_sample_rate,
                                                preview_resolver.as_ref(),
                                                &preview_store,
                                                window,
                                                cx,
                                            );
                                            drop_hover_style(style, compatible, CYAN)
                                        }
                                    })
                                    .children(
                                        tracks.iter().map(|track| self.render_track(track, cx)),
                                    )
                                    .when_some(canvas_drop_preview, |body, preview| {
                                        body.child(drop_track_creation_preview(preview))
                                    })
                                    .when(tracks.is_empty(), |body| {
                                        body.child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .text_color(rgb(MUTED))
                                                .child(
                                                    "No tracks · use + Audio, + Pattern, or + Auto",
                                                ),
                                        )
                                    })
                                    .on_drop(cx.listener(|this, source: &AssetDrag, window, cx| {
                                        this.accept_drop(
                                            DragPayload::Asset(*source),
                                            None,
                                            window,
                                            cx,
                                        )
                                    }))
                                    .on_drop(cx.listener(
                                        |this,
                                         pattern: &crate::sequencer::PatternId,
                                         window,
                                         cx| {
                                            this.accept_drop(
                                                DragPayload::Pattern(*pattern),
                                                None,
                                                window,
                                                cx,
                                            )
                                        },
                                    ))
                                    .on_drop(cx.listener(
                                        |this, payload: &DragPayload, window, cx| {
                                            this.accept_drop(payload.clone(), None, window, cx)
                                        },
                                    )),
                            ),
                    )
                    .child(self.render_inspector()),
            )
            .child(
                div()
                    .h(px(26.0))
                    .flex_none()
                    .px_3()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL))
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(self.status.clone())
                    .child(format!(
                        "{} tracks · {} clips",
                        self.editor.state().tracks.len(),
                        self.editor.state().clips.len()
                    )),
            )
    }
}

fn drag_modifiers(modifiers: gpui::Modifiers) -> DragModifiers {
    DragModifiers {
        // Option-drag duplicates arrangement objects and requests an unlinked
        // pattern instance; Shift is the conventional temporary snap bypass.
        duplicate: modifiers.alt,
        make_unique: modifiers.alt,
        suppress_snap: modifiers.shift,
    }
}

fn rendered_drop_target(
    track: Option<(TrackId, TrackKind)>,
    viewport: ArrangementViewport,
    timeline_bounds: &Arc<Mutex<Option<Bounds<Pixels>>>>,
    snap_quantum: Option<u64>,
    position: gpui::Point<Pixels>,
    modifiers: DragModifiers,
) -> Option<DropTarget> {
    let bounds = timeline_bounds.lock().ok().and_then(|bounds| *bounds)?;
    let width = f64::from(f32::from(bounds.size.width));
    if width <= 0.0 {
        return None;
    }
    let fraction = f64::from(f32::from(position.x - bounds.origin.x)) / width;
    let raw = viewport.frame_at_fraction(fraction);
    let at = if modifiers.suppress_snap {
        raw
    } else {
        let quantum = snap_quantum.unwrap_or(1).min(i64::MAX as u64) as i64;
        Frame(snap_frame(raw.0, quantum.max(1)))
    };
    Some(match track {
        Some((track, kind)) => DropTarget::ArrangementTrack { track, kind, at },
        None => DropTarget::ArrangementCanvas { at },
    })
}

fn point_over_any_track(
    track_bounds: &Arc<Mutex<BTreeMap<TrackId, Bounds<Pixels>>>>,
    position: gpui::Point<Pixels>,
) -> bool {
    track_bounds
        .lock()
        .map(|bounds| bounds.values().any(|bounds| bounds.contains(&position)))
        .unwrap_or(false)
}

#[allow(clippy::too_many_arguments)]
fn update_drop_preview(
    payload: DragPayload,
    track: Option<(TrackId, TrackKind)>,
    viewport: ArrangementViewport,
    timeline_bounds: &Arc<Mutex<Option<Bounds<Pixels>>>>,
    snap_quantum: Option<u64>,
    project_sample_rate: u32,
    resolver: Option<&SharedArrangementPreviewResolver>,
    preview_store: &Arc<Mutex<Option<ArrangementDropPreview>>>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let modifiers = drag_modifiers(window.modifiers());
    let Some(target) = rendered_drop_target(
        track,
        viewport,
        timeline_bounds,
        snap_quantum,
        window.mouse_position(),
        modifiers,
    ) else {
        return false;
    };
    let preview = build_drop_preview(
        payload,
        target,
        modifiers,
        project_sample_rate,
        resolver.map(Arc::as_ref),
    );
    let compatible = preview.intent.is_ok();
    if let Ok(mut current) = preview_store.lock() {
        if current.as_ref() != Some(&preview) {
            *current = Some(preview);
            cx.refresh_windows();
        }
    }
    compatible
}

fn build_drop_preview(
    payload: DragPayload,
    target: DropTarget,
    modifiers: DragModifiers,
    project_sample_rate: u32,
    resolver: Option<&dyn ArrangementPreviewResolver>,
) -> ArrangementDropPreview {
    let intent = interpret_drop(payload, target, modifiers).map_err(|error| error.to_string());
    let (placement, create_track, label) = match intent.as_ref() {
        Ok(DropIntent::InsertAudio { source, track, at }) => {
            let create_track = track.is_none().then_some(TrackKind::Audio);
            let resolved = resolver.and_then(|resolver| resolver.media_asset(source.asset));
            let placement = resolved.as_ref().and_then(|resolved| {
                let range = source
                    .source_range
                    .unwrap_or(crate::assets::AssetFrameRange {
                        start: crate::assets::SampleFrames(0),
                        end: resolved.key.frame_count,
                    });
                range
                    .is_within(resolved.key.frame_count)
                    .then(|| {
                        project_frame_count(
                            range.len().0,
                            resolved.key.sample_rate_hz,
                            project_sample_rate,
                        )
                    })
                    .flatten()
                    .and_then(|length| FrameRange::from_start_and_len(*at, length).ok())
            });
            let source_label = source
                .source_range
                .map(|range| {
                    format!(
                        "frames {}..{}",
                        grouped_u64(range.start.0),
                        grouped_u64(range.end.0)
                    )
                })
                .unwrap_or_else(|| "whole asset".into());
            (
                placement,
                create_track,
                format!("Audio asset #{} · {source_label}", source.asset.0),
            )
        }
        Ok(DropIntent::InsertPattern {
            pattern, track, at, ..
        }) => {
            let placement = resolver
                .and_then(|resolver| resolver.dropped_pattern(*pattern))
                .and_then(|preview| {
                    FrameRange::from_start_and_len(*at, preview.length_frames).ok()
                });
            (
                placement,
                track.is_none().then_some(TrackKind::Pattern),
                format!("Pattern #{}", pattern.get()),
            )
        }
        Ok(intent) => (None, None, drop_intent_label(intent).into()),
        Err(error) => (None, None, error.clone()),
    };
    ArrangementDropPreview {
        target,
        intent,
        placement,
        create_track,
        label,
    }
}

fn project_frame_count(source_frames: u64, source_rate: u32, project_rate: u32) -> Option<u64> {
    if source_frames == 0 || source_rate == 0 || project_rate == 0 {
        return None;
    }
    let numerator = u128::from(source_frames) * u128::from(project_rate);
    let denominator = u128::from(source_rate);
    let frames = numerator / denominator + u128::from(numerator % denominator != 0);
    u64::try_from(frames).ok().filter(|frames| *frames > 0)
}

fn drop_intent_label(intent: &DropIntent) -> &'static str {
    match intent {
        DropIntent::InsertAudio { .. } => "Insert audio",
        DropIntent::InsertPattern { .. } => "Insert pattern",
        DropIntent::MoveArrangementClips { .. } => "Move arrangement clips",
        DropIntent::MapAssetToStepPattern { .. } => "Map sample to pattern",
        DropIntent::MapAssetToPad { .. } => "Map sample to pad",
        DropIntent::AddPatternToLibrary { .. } => "Add pattern to library",
        DropIntent::PreviewAspectDeprojection { .. } => "Preview aspect deprojection",
        DropIntent::PreviewReconstruction { .. } => "Preview reconstruction",
        DropIntent::RouteBus { .. } => "Route mixer bus",
    }
}

fn drop_hover_style(
    style: gpui::StyleRefinement,
    compatible: bool,
    color: u32,
) -> gpui::StyleRefinement {
    if compatible {
        style.border_color(rgb(color)).bg(rgba((color << 8) | 0x22))
    } else {
        style.border_color(rgb(MAGENTA)).bg(rgba(0xf172b619))
    }
}

fn drop_preview_block(
    visible: VisibleClip,
    compatible: bool,
    label: String,
) -> gpui::Stateful<gpui::Div> {
    let color = if compatible { CYAN } else { MAGENTA };
    div()
        .id("arrangement-drop-preview")
        .absolute()
        .left(relative(visible.left))
        .top(px(4.0))
        .w(relative(visible.width.max(0.002)))
        .h(px(TRACK_HEIGHT - 8.0))
        .min_w(px(10.0))
        .overflow_hidden()
        .rounded_sm()
        .border_1()
        .border_color(rgb(color))
        .bg(rgba((color << 8) | 0x28))
        .px_2()
        .py_1()
        .text_xs()
        .text_color(rgb(TEXT))
        .child(label)
}

fn drop_track_creation_preview(preview: ArrangementDropPreview) -> impl IntoElement {
    let compatible = preview.intent.is_ok();
    let color = if compatible { CYAN } else { MAGENTA };
    let at = match preview.target {
        DropTarget::ArrangementCanvas { at } => at,
        DropTarget::ArrangementTrack { at, .. } => at,
        _ => Frame::ZERO,
    };
    let track = preview
        .create_track
        .map(track_kind_name)
        .unwrap_or("destination");
    div()
        .h(px(50.0))
        .mx_3()
        .my_2()
        .px_3()
        .flex()
        .items_center()
        .rounded_sm()
        .border_1()
        .border_color(rgb(color))
        .bg(rgba((color << 8) | 0x22))
        .text_xs()
        .text_color(rgb(if compatible { TEXT } else { MAGENTA }))
        .child(format!(
            "{} · create {track} track at sample {}",
            preview.label,
            grouped_i64(at.0)
        ))
}

#[derive(Clone)]
struct InspectorContent {
    mapping: String,
    transform: String,
    caveat: String,
}

fn inspector_content(clip: &Clip) -> InspectorContent {
    match &clip.content {
        ClipContent::Audio(audio) => InspectorContent {
            mapping: format!(
                "Asset #{}\nsource [{}..{})\n{} source frames → {} project frames\nchannels: {:?}",
                audio.asset.get(),
                grouped_u64(audio.source.start),
                grouped_u64(audio.source.end),
                grouped_u64(audio.playback.ratio.source_frames),
                grouped_u64(audio.playback.ratio.project_frames),
                audio.channels
            ),
            transform: format!(
                "{:?} · pitch {:+.2} st · preserve {} · reverse {} · {} warp markers",
                audio.playback.algorithm,
                audio.playback.pitch_semitones,
                yes_no(audio.playback.preserve_pitch),
                yes_no(audio.playback.reverse),
                audio.playback.warp_markers.len()
            ),
            caveat: "The visible waveform is an immutable-source navigation proxy. Audition/render remains authoritative for stretch, pitch, reverse, warp, fades, gain, and bus processing.".into(),
        },
        ClipContent::Pattern(pattern) => InspectorContent {
            mapping: format!(
                "Pattern #{}\ncontent offset {} frames\nreusable definition · loop {}",
                pattern.pattern.get(), grouped_u64(pattern.content_offset_frames), yes_no(pattern.looped)
            ),
            transform: "Pattern placement and offsets are exact project-frame metadata.".into(),
            caveat: "Pattern blocks show placement and repetition. Bounce-on-play remains the authoritative instrument render.".into(),
        },
        ClipContent::Automation(automation) => InspectorContent {
            mapping: format!(
                "Parameter #{}\ncurve offset {} frames\nreusable definition · loop {}",
                automation.parameter.get(), grouped_u64(automation.content_offset_frames), yes_no(automation.looped)
            ),
            transform: "Automation placement identifies a parameter curve in exact project frames.".into(),
            caveat: "The automation engine owns curve evaluation; target binding and DSP application require the project compiler.".into(),
        },
    }
}

fn track_header(track: &Track, color: u32) -> impl IntoElement {
    div()
        .w(px(TRACK_GUTTER))
        .h_full()
        .flex_none()
        .px_3()
        .border_r_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL_ALT))
        .flex()
        .items_center()
        .gap_2()
        .child(div().w(px(3.0)).h(px(38.0)).rounded_full().bg(rgb(color)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(TEXT))
                        .child(track.name.clone()),
                )
                .child(div().text_xs().text_color(rgb(DIM)).child(format!(
                    "{} · {:+.1} dB · pan {:+.2}",
                    track_kind_name(track.kind),
                    track.gain_db,
                    track.pan
                ))),
        )
        .child(
            div()
                .flex()
                .gap_1()
                .child(header_badge("M", track.muted, MAGENTA))
                .child(header_badge("S", track.solo, CYAN)),
        )
}

#[derive(Clone, Copy, Debug)]
struct ClipVisual {
    track: TrackId,
    placement: FrameRange,
    fades: ClipFades,
    source_proxy_valid: bool,
    previewing: bool,
    slipping: bool,
    stretching: bool,
}

fn clip_repeat_capable(clip: &Clip) -> bool {
    match &clip.content {
        ClipContent::Audio(audio) => {
            !matches!(audio.loop_mode, crate::arrangement::AudioLoopMode::Off)
        }
        ClipContent::Pattern(_) | ClipContent::Automation(_) => true,
    }
}

fn previewed_clip(clip: &Clip, preview: Option<&PreviewPatch>) -> ClipVisual {
    let mut visual = ClipVisual {
        track: clip.track_id,
        placement: clip.placement,
        fades: clip.fades,
        source_proxy_valid: true,
        previewing: false,
        slipping: false,
        stretching: false,
    };
    let Some(preview) = preview else {
        return visual;
    };
    for change in &preview.changes {
        match change {
            PreviewChange::Move(change) if change.clip_id == clip.id => {
                visual.track = change.to_track;
                visual.placement = change.to;
                visual.previewing = true;
            }
            PreviewChange::Trim { clip_id, after, .. } if *clip_id == clip.id => {
                visual.placement = *after;
                visual.source_proxy_valid = false;
                visual.previewing = true;
            }
            PreviewChange::Slip { clip_id, .. } if *clip_id == clip.id => {
                visual.source_proxy_valid = false;
                visual.previewing = true;
                visual.slipping = true;
            }
            PreviewChange::Stretch { clip_id, after, .. } if *clip_id == clip.id => {
                visual.placement = *after;
                visual.source_proxy_valid = false;
                visual.previewing = true;
                visual.stretching = true;
            }
            PreviewChange::Fade { clip_id, fades, .. } if *clip_id == clip.id => {
                visual.fades = *fades;
                visual.previewing = true;
            }
            PreviewChange::RepeatBoundary {
                clip_id, boundary, ..
            } if *clip_id == clip.id && *boundary > visual.placement.start => {
                visual.placement.end = *boundary;
                visual.source_proxy_valid = false;
                visual.previewing = true;
            }
            _ => {}
        }
    }
    visual
}

fn clip_block(
    clip: Clip,
    visual: ClipVisual,
    visible: VisibleClip,
    selected: bool,
    color: u32,
    viewport: ArrangementViewport,
    waveform_provider: Option<ArrangementWaveformProvider>,
    preview_resolver: Option<SharedArrangementPreviewResolver>,
    waveform_cache: Arc<Mutex<WaveformPaintCache>>,
) -> gpui::Stateful<gpui::Div> {
    let left = visible.left;
    let width = visible.width;
    let kind = clip.content.kind();
    let left_edge_visible =
        visual.placement.start >= viewport.start && visual.placement.start < viewport.end;
    let right_edge_visible =
        visual.placement.end > viewport.start && visual.placement.end <= viewport.end;
    let show_repeat = right_edge_visible && clip_repeat_capable(&clip);
    let waveform = if visual.source_proxy_valid {
        waveform_provider.and_then(|provider| {
            waveform_element(&clip, visual, viewport, color, provider, waveform_cache)
        })
    } else {
        None
    };
    let pattern_texture = match (&clip.content, preview_resolver.as_ref()) {
        (ClipContent::Pattern(region), Some(resolver)) => resolver
            .placed_pattern(region.pattern)
            .map(|pattern| pattern_step_texture(region, &pattern, visual, viewport, color)),
        _ => None,
    };
    let texture = if kind == TrackKind::Audio {
        unavailable_waveform_texture(color).into_any_element()
    } else if let Some(pattern) = pattern_texture {
        pattern
    } else {
        clip_texture(kind, color, clip.id.get()).into_any_element()
    };
    div()
        .id(("arrangement-clip", clip.id.get() as usize))
        .absolute()
        .left(relative(left))
        .top(px(7.0))
        .w(relative(width))
        .h(px(TRACK_HEIGHT - 14.0))
        .min_w(px(7.0))
        .overflow_hidden()
        .rounded_sm()
        .border_1()
        .border_color(if visual.previewing {
            rgb(AMBER)
        } else if selected {
            rgb(TEXT)
        } else {
            rgb(color)
        })
        .bg(if visual.previewing {
            rgba((color << 8) | 0x40)
        } else if selected {
            rgba((color << 8) | 0x55)
        } else {
            rgba((color << 8) | 0x2c)
        })
        .cursor_grab()
        .when(visual.previewing, |block| block.cursor_grabbing())
        .hover(move |style| style.bg(rgba((color << 8) | 0x48)).border_color(rgb(TEXT)))
        .child(texture)
        .when_some(waveform, |block, waveform| block.child(waveform))
        .when(left_edge_visible, |block| {
            block.child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(4.0))
                    .cursor_ew_resize()
                    .bg(rgba(0xffffff18)),
            )
        })
        .when(right_edge_visible, |block| {
            block.child(
                div()
                    .absolute()
                    .right_0()
                    .top_0()
                    .bottom_0()
                    .w(px(4.0))
                    .cursor_ew_resize()
                    .bg(rgba(0xffffff18)),
            )
        })
        .when(kind == TrackKind::Audio, |block| {
            block
                .when(left_edge_visible, |block| {
                    block.child(
                        div()
                            .absolute()
                            .left_0()
                            .top_0()
                            .size(px(7.0))
                            .cursor_crosshair()
                            .bg(rgba(0xf6b76099)),
                    )
                })
                .when(right_edge_visible, |block| {
                    block.child(
                        div()
                            .absolute()
                            .right_0()
                            .top_0()
                            .size(px(7.0))
                            .cursor_crosshair()
                            .bg(rgba(0xf6b76099)),
                    )
                })
        })
        .when(visual.slipping, |block| {
            block.child(
                div()
                    .absolute()
                    .left_2()
                    .top(px(22.0))
                    .text_xs()
                    .text_color(rgb(AMBER))
                    .child("SLIP ↔"),
            )
        })
        .when(visual.stretching, |block| {
            block.child(
                div()
                    .absolute()
                    .right_2()
                    .top(px(22.0))
                    .text_xs()
                    .text_color(rgb(AMBER))
                    .child("STRETCH ↔"),
            )
        })
        .when(show_repeat, |block| {
            block.child(
                div()
                    .absolute()
                    .right_0()
                    .bottom_0()
                    .size(px(8.0))
                    .cursor_ew_resize()
                    .border_l_1()
                    .border_t_1()
                    .border_color(rgb(AMBER))
                    .bg(rgba(0xf6b76066)),
            )
        })
        .child(
            div()
                .absolute()
                .left_2()
                .right_2()
                .top_1()
                .flex()
                .items_center()
                .justify_between()
                .text_xs()
                .child(div().font_weight(gpui::FontWeight::BOLD).child(clip.name))
                .child(
                    div()
                        .text_color(rgb(TEXT))
                        .child(format!("#{}", clip.id.get())),
                ),
        )
        .child(
            div()
                .absolute()
                .left_2()
                .bottom_1()
                .text_xs()
                .text_color(rgb(TEXT))
                .child(format!("{}f", grouped_u64(visual.placement.len()))),
        )
}

fn pattern_step_texture(
    region: &crate::arrangement::PatternRegion,
    pattern: &ArrangementPatternPreview,
    visual: ClipVisual,
    viewport: ArrangementViewport,
    color: u32,
) -> gpui::AnyElement {
    let Some(visible) = visual.placement.intersection(FrameRange {
        start: viewport.start,
        end: viewport.end,
    }) else {
        return div().into_any_element();
    };
    if pattern.length_frames == 0 || pattern.pulses.is_empty() {
        return clip_texture(TrackKind::Pattern, color, region.pattern.get()).into_any_element();
    }

    let visible_local_start = visible.start.0.saturating_sub(visual.placement.start.0) as u64;
    let visible_local_end = visible.end.0.saturating_sub(visual.placement.start.0) as u64;
    let stream_start = region
        .content_offset_frames
        .saturating_add(visible_local_start);
    let stream_end = region
        .content_offset_frames
        .saturating_add(visible_local_end);
    let first_cycle = if region.looped {
        stream_start / pattern.length_frames
    } else {
        0
    };
    let last_cycle = if region.looped {
        stream_end.saturating_add(pattern.length_frames.saturating_sub(1)) / pattern.length_frames
    } else {
        1
    };
    let visible_len = visible.len().max(1);
    let mut bars = Vec::new();
    'cycles: for cycle in first_cycle..last_cycle.max(first_cycle.saturating_add(1)) {
        for pulse in &pattern.pulses {
            if bars.len() >= 256 || !pulse.velocity.is_finite() {
                break 'cycles;
            }
            let stream_offset = cycle
                .saturating_mul(pattern.length_frames)
                .saturating_add(pulse.offset_frames);
            if stream_offset < region.content_offset_frames {
                continue;
            }
            let local = stream_offset - region.content_offset_frames;
            if local >= visual.placement.len() {
                continue;
            }
            let absolute = visual.placement.start.0.saturating_add(local as i64);
            let duration = pulse.duration_frames.max(1).min(i64::MAX as u64) as i64;
            let event_end = absolute.saturating_add(duration);
            if event_end <= visible.start.0 || absolute >= visible.end.0 {
                continue;
            }
            let clipped_start = absolute.max(visible.start.0);
            let clipped_end = event_end.min(visible.end.0);
            let left = clipped_start.saturating_sub(visible.start.0) as f32 / visible_len as f32;
            let width =
                clipped_end.saturating_sub(clipped_start).max(1) as f32 / visible_len as f32;
            let velocity = pulse.velocity.clamp(0.0, 1.0);
            bars.push(
                div()
                    .absolute()
                    .left(relative(left))
                    .bottom(px(8.0))
                    .w(relative(width.max(0.002)))
                    .h(relative((0.14 + velocity * 0.54).min(0.72)))
                    .rounded_sm()
                    .bg(rgba((color << 8) | (90.0 + velocity * 150.0) as u32)),
            );
        }
    }

    div()
        .absolute()
        .left_0()
        .right_0()
        .top_0()
        .bottom_0()
        .overflow_hidden()
        .children(bars)
        .into_any_element()
}

fn waveform_element(
    clip: &Clip,
    visual: ClipVisual,
    viewport: ArrangementViewport,
    color: u32,
    provider: ArrangementWaveformProvider,
    waveform_cache: Arc<Mutex<WaveformPaintCache>>,
) -> Option<gpui::AnyElement> {
    let ClipContent::Audio(audio) = &clip.content else {
        return None;
    };
    let clip_id = clip.id;
    let alias = audio.asset;
    let source = audio.source;
    let playback = audio.playback.clone();
    let channels = audio.channels.clone();
    let loop_mode = audio.loop_mode;
    let source_asset = provider(alias)?;
    let viewport = FrameRange::new(viewport.start, viewport.end).ok()?;
    Some(
        canvas(
            |bounds, _, _| bounds,
            move |bounds, _, window, _| {
                let Ok(pixels) = PixelTarget::new(
                    f64::from(f32::from(bounds.size.width)),
                    f64::from(window.scale_factor()),
                ) else {
                    return;
                };
                let spec = ClipWaveformSpec {
                    clip: clip_id,
                    asset: source_asset.key,
                    placement: visual.placement,
                    source,
                    playback: playback.clone(),
                    channels: channels.clone(),
                    loop_mode,
                };
                let Ok(WaveformProxyPlan::Ready(request)) =
                    plan_clip_waveform(&spec, viewport, pixels)
                else {
                    paint_waveform_unavailable(bounds, color, window);
                    return;
                };
                let Ok(query) = waveform_cache.lock().map_err(|_| ()).and_then(|mut cache| {
                    cache
                        .get_or_query(&request, &source_asset.pyramid)
                        .map_err(|_| ())
                }) else {
                    paint_waveform_unavailable(bounds, color, window);
                    return;
                };
                paint_waveform_query(&query, bounds, request.reverse_display, color, window);
                paint_fades(
                    visual.fades,
                    visual.placement,
                    request.visible_project,
                    bounds,
                    window,
                );
            },
        )
        .absolute()
        .left_1()
        .right_1()
        .top(px(18.0))
        .bottom(px(12.0))
        .into_any_element(),
    )
}

fn paint_waveform_query(
    query: &WaveformQuery,
    bounds: Bounds<Pixels>,
    reverse: bool,
    color: u32,
    window: &mut Window,
) {
    if query.bins.len() < 2 {
        return;
    }
    let center = bounds.origin.y + bounds.size.height * 0.5;
    let amplitude = bounds.size.height * 0.47;
    let mut builder = PathBuilder::fill();
    let bin_at = |index: usize| {
        &query.bins[if reverse {
            query.bins.len() - 1 - index
        } else {
            index
        }]
    };
    for index in 0..query.bins.len() {
        let bin = bin_at(index);
        let fraction = index as f32 / (query.bins.len() - 1) as f32;
        let maximum = bin
            .channels
            .iter()
            .map(|channel| channel.max)
            .fold(f32::NEG_INFINITY, f32::max);
        let maximum = if maximum.is_finite() { maximum } else { 0.0 }.clamp(-1.0, 1.0);
        let location = point(
            bounds.origin.x + bounds.size.width * fraction,
            center - amplitude * maximum,
        );
        if index == 0 {
            builder.move_to(location);
        } else {
            builder.line_to(location);
        }
    }
    for index in (0..query.bins.len()).rev() {
        let bin = bin_at(index);
        let fraction = index as f32 / (query.bins.len() - 1) as f32;
        let minimum = bin
            .channels
            .iter()
            .map(|channel| channel.min)
            .fold(f32::INFINITY, f32::min);
        let minimum = if minimum.is_finite() { minimum } else { 0.0 }.clamp(-1.0, 1.0);
        builder.line_to(point(
            bounds.origin.x + bounds.size.width * fraction,
            center - amplitude * minimum,
        ));
    }
    builder.close();
    if let Ok(path) = builder.build() {
        window.paint_path(path, rgba((color << 8) | 0xc8));
    }
}

fn paint_waveform_unavailable(bounds: Bounds<Pixels>, color: u32, window: &mut Window) {
    let center = bounds.origin.y + bounds.size.height * 0.5;
    let mut builder = PathBuilder::stroke(px(1.0));
    builder.move_to(point(bounds.origin.x, center));
    builder.line_to(point(bounds.origin.x + bounds.size.width, center));
    if let Ok(path) = builder.build() {
        window.paint_path(path, rgba((color << 8) | 0x66));
    }
}

fn paint_fades(
    fades: ClipFades,
    placement: FrameRange,
    visible: FrameRange,
    bounds: Bounds<Pixels>,
    window: &mut Window,
) {
    let x_for = |frame: Frame| {
        let offset = frame.0.saturating_sub(visible.start.0) as f64;
        let fraction = (offset / visible.len().max(1) as f64).clamp(0.0, 1.0) as f32;
        bounds.origin.x + bounds.size.width * fraction
    };
    for (fade, incoming) in [(fades.fade_in, true), (fades.fade_out, false)] {
        let Some(fade) = fade else {
            continue;
        };
        let boundary = if incoming {
            Frame(placement.start.0.saturating_add(fade.duration as i64))
        } else {
            Frame(placement.end.0.saturating_sub(fade.duration as i64))
        };
        let (start, end) = if incoming {
            (placement.start, boundary)
        } else {
            (boundary, placement.end)
        };
        if end <= visible.start || start >= visible.end {
            continue;
        }
        let mut builder = PathBuilder::stroke(px(1.0));
        if incoming {
            builder.move_to(point(x_for(start), bounds.origin.y + bounds.size.height));
            builder.line_to(point(x_for(end), bounds.origin.y));
        } else {
            builder.move_to(point(x_for(start), bounds.origin.y));
            builder.line_to(point(x_for(end), bounds.origin.y + bounds.size.height));
        }
        if let Ok(path) = builder.build() {
            window.paint_path(path, rgba(0xf6b760dd));
        }
    }
}

fn unavailable_waveform_texture(color: u32) -> impl IntoElement {
    div()
        .absolute()
        .left_1()
        .right_1()
        .top(px(18.0))
        .bottom(px(12.0))
        .flex()
        .items_center()
        .child(div().w_full().h(px(1.0)).bg(rgba((color << 8) | 0x44)))
}

fn clip_texture(kind: TrackKind, color: u32, seed: u64) -> impl IntoElement {
    let count = 24usize;
    div()
        .absolute()
        .left_1()
        .right_1()
        .top(px(20.0))
        .bottom(px(15.0))
        .flex()
        .items_end()
        .gap(px(1.0))
        .opacity(0.72)
        .children((0..count).map(move |index| {
            let phase = ((index as u64 * 17 + seed * 11) % 29) as f32 / 28.0;
            let height = match kind {
                TrackKind::Audio | TrackKind::Hybrid => 0.18 + phase * 0.75,
                TrackKind::Pattern => {
                    if index % 4 == 0 || index % 7 == 0 {
                        0.86
                    } else {
                        0.2
                    }
                }
                TrackKind::Automation => 0.15 + ((index as f32 / 4.0).sin() * 0.35 + 0.42),
                TrackKind::Group => 0.3,
            };
            div()
                .flex_1()
                .min_w(px(1.0))
                .h(relative(height))
                .rounded_sm()
                .bg(rgb(color))
        }))
}

fn tool_button(
    id: &'static str,
    label: impl Into<String>,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(27.0))
        .min_w(px(27.0))
        .px_2()
        .rounded_sm()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(if enabled { rgb(RAISED) } else { rgb(PANEL) })
        .text_xs()
        .text_color(if enabled { rgb(TEXT) } else { rgb(DIM) })
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(|style| style.bg(rgb(BORDER)).text_color(rgb(TEXT)))
        .child(label.into())
}

fn separator() -> impl IntoElement {
    div().mx_1().w(px(1.0)).h(px(22.0)).bg(rgb(BORDER))
}

fn header_badge(label: &'static str, active: bool, color: u32) -> impl IntoElement {
    div()
        .size(px(20.0))
        .rounded_sm()
        .border_1()
        .border_color(if active { rgb(color) } else { rgb(BORDER) })
        .bg(if active {
            rgba((color << 8) | 0x30)
        } else {
            rgb(PANEL)
        })
        .text_xs()
        .text_color(if active { rgb(color) } else { rgb(DIM) })
        .flex()
        .items_center()
        .justify_center()
        .child(label)
}

fn section_label(label: &'static str) -> impl IntoElement {
    div().mt_1().text_xs().text_color(rgb(DIM)).child(label)
}

fn inspector_metric(label: &'static str, value: String, color: u32) -> impl IntoElement {
    div()
        .py_1()
        .border_b_1()
        .border_color(rgb(BORDER))
        .flex()
        .items_baseline()
        .justify_between()
        .gap_2()
        .child(div().text_xs().text_color(rgb(DIM)).child(label))
        .child(div().text_xs().text_color(rgb(color)).child(value))
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct VisibleClip {
    left: f32,
    width: f32,
}

fn canvas_rect(bounds: Bounds<Pixels>) -> CanvasRect {
    CanvasRect::new(
        f64::from(f32::from(bounds.origin.x)),
        f64::from(f32::from(bounds.origin.y)),
        f64::from(f32::from(bounds.origin.x + bounds.size.width)),
        f64::from(f32::from(bounds.origin.y + bounds.size.height)),
    )
}

fn apply_id_selection(
    current: &mut BTreeSet<ClipId>,
    incoming: BTreeSet<ClipId>,
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

fn selected_time_range(state: &ArrangementState, clips: &BTreeSet<ClipId>) -> Option<FrameRange> {
    let mut placements = clips
        .iter()
        .filter_map(|id| state.clip(*id).map(|clip| clip.placement));
    let first = placements.next()?;
    let (start, end) = placements.fold((first.start, first.end), |(start, end), placement| {
        (start.min(placement.start), end.max(placement.end))
    });
    FrameRange::new(start, end).ok()
}

fn preview_status(preview: &PreviewPatch) -> String {
    if preview.marquee.is_some() {
        return "Marquee selection · release to select".into();
    }
    match preview.changes.first() {
        Some(PreviewChange::Move(_)) => "Moving clip selection · release to commit".into(),
        Some(PreviewChange::Trim { .. }) => {
            "Trimming clip edge · exact source mapping on commit".into()
        }
        Some(PreviewChange::Slip { project_delta, .. }) => {
            format!("Slipping source by {project_delta:+} project frames")
        }
        Some(PreviewChange::Stretch { .. }) => {
            "Stretching clip · source range preserved · release to commit".into()
        }
        Some(PreviewChange::Fade { .. }) => "Shaping clip fade · release to commit".into(),
        Some(PreviewChange::RepeatBoundary { .. }) => {
            "Setting repeat boundary · release to commit".into()
        }
        None => "Gesture preview".into(),
    }
}

fn marquee_tracks_include(
    state: &ArrangementState,
    marquee: MarqueePreview,
    track: TrackId,
) -> bool {
    match (marquee.anchor_track, marquee.focus_track) {
        (None, None) => true,
        (Some(anchor), Some(focus)) => {
            let anchor = state.track_order.iter().position(|id| *id == anchor);
            let focus = state.track_order.iter().position(|id| *id == focus);
            let candidate = state.track_order.iter().position(|id| *id == track);
            match (anchor, focus, candidate) {
                (Some(anchor), Some(focus), Some(candidate)) => {
                    (anchor.min(focus)..=anchor.max(focus)).contains(&candidate)
                }
                _ => {
                    track == marquee.anchor_track.unwrap_or(track)
                        || track == marquee.focus_track.unwrap_or(track)
                }
            }
        }
        (Some(only), None) | (None, Some(only)) => track == only,
    }
}

fn visible_clip(range: FrameRange, viewport: ArrangementViewport) -> Option<VisibleClip> {
    let start = range.start.max(viewport.start);
    let end = range.end.min(viewport.end);
    if start >= end {
        return None;
    }
    Some(VisibleClip {
        left: viewport.fraction(start).clamp(0.0, 1.0),
        width: (viewport.fraction(end) - viewport.fraction(start)).clamp(0.0, 1.0),
    })
}

#[derive(Clone, Debug, PartialEq)]
struct RulerTick {
    frame: Frame,
    label: String,
    major: bool,
}

fn tempo_ruler_ticks(tempo: &TempoMap, viewport: ArrangementViewport) -> Vec<RulerTick> {
    let Ok(visible) = FrameRange::new(viewport.start, viewport.end) else {
        return Vec::new();
    };
    let plan = plan_musical_grid(
        tempo,
        visible,
        MusicalGridResolution::Quarter,
        0,
        DEFAULT_GRID_LINE_LIMIT,
    );
    let mut lines = plan.lines;
    if lines.len() > 80 {
        lines.retain(|line| line.kind == SnapGuideKind::Bar);
    }
    if lines.len() > 80 {
        let stride = lines.len().div_ceil(80);
        lines = lines.into_iter().step_by(stride).collect();
    }
    lines
        .into_iter()
        .map(|line| {
            let major = line.kind == SnapGuideKind::Bar;
            RulerTick {
                frame: line.frame,
                label: if major {
                    format!("{}.1 · {}f", line.bar + 1, grouped_i64(line.frame.0))
                } else {
                    format!("{}.{}", line.bar + 1, u32::from(line.beat) + 1)
                },
                major,
            }
        })
        .collect()
}

fn ruler_ticks(
    viewport: ArrangementViewport,
    sample_rate: u32,
    bpm: f64,
    beats_per_bar: u8,
) -> Vec<RulerTick> {
    let beat = frames_per_beat(sample_rate, bpm).max(1);
    // Labels include both musical position and exact source frames. Keep the
    // density conservative enough for a narrow editor window; unlabeled grid
    // lines would otherwise turn these provenance-rich labels into a blur.
    let desired = viewport.span().max(1) / 6;
    let quarter_beat = (beat / 4).max(1);
    let half_beat = (beat / 2).max(1);
    let mut step = if desired <= quarter_beat {
        quarter_beat
    } else if desired <= half_beat {
        half_beat
    } else {
        // Once labels are sparser than a beat, grow from the exact beat
        // length so distant ticks stay musically aligned instead of drifting
        // because of truncated fractional-beat arithmetic.
        beat
    };
    while step < desired {
        let Some(next) = step.checked_mul(2) else {
            step = u64::MAX;
            break;
        };
        step = next;
    }
    let step = step.min(i64::MAX as u64) as i64;
    let first = viewport.start.0.div_euclid(step) * step;
    let mut ticks = Vec::new();
    let mut frame = first;
    while frame <= viewport.end.0 && ticks.len() < 80 {
        if frame >= viewport.start.0 {
            let beat_number = frame.div_euclid(beat as i64);
            let major = beat_number.rem_euclid(beats_per_bar as i64) == 0;
            ticks.push(RulerTick {
                frame: Frame(frame),
                label: if major {
                    format!(
                        "{}.1 · {}f",
                        beat_number.div_euclid(beats_per_bar as i64) + 1,
                        grouped_i64(frame)
                    )
                } else {
                    format!(
                        "{}.{}",
                        beat_number.div_euclid(beats_per_bar as i64) + 1,
                        beat_number.rem_euclid(beats_per_bar as i64) + 1
                    )
                },
                major,
            });
        }
        let Some(next) = frame.checked_add(step) else {
            break;
        };
        frame = next;
    }
    ticks
}

fn fit_viewport(editor: &ArrangementEditor, bpm: f64, beats_per_bar: u8) -> ArrangementViewport {
    let beat = frames_per_beat(editor.state().sample_rate, bpm);
    let bar = beat.saturating_mul(beats_per_bar as u64).max(1);
    let minimum = bar.saturating_mul(4);
    match editor.state().project_range() {
        Some(range) => {
            let start = Frame(range.start.0.min(0).saturating_sub(bar as i64));
            let content_end = range.end.0.saturating_add(bar as i64);
            let end = Frame(content_end.max(start.0.saturating_add(minimum as i64)));
            ArrangementViewport::new(start, end, (beat / 4).max(128))
        }
        None => ArrangementViewport::new(Frame::ZERO, Frame(minimum as i64), (beat / 4).max(128)),
    }
}

fn snap_frames(sample_rate: u32, bpm: f64, beats_per_bar: u8, snap: SnapDivision) -> Option<u64> {
    let beat = frames_per_beat(sample_rate, bpm);
    match snap {
        SnapDivision::Off => None,
        SnapDivision::Bar => Some(beat.saturating_mul(beats_per_bar as u64).max(1)),
        SnapDivision::Beat => Some(beat.max(1)),
        SnapDivision::Eighth => Some((beat / 2).max(1)),
        SnapDivision::Sixteenth => Some((beat / 4).max(1)),
    }
}

fn frames_per_beat(sample_rate: u32, bpm: f64) -> u64 {
    if sample_rate == 0 || !bpm.is_finite() || bpm <= 0.0 {
        return 1;
    }
    (sample_rate as f64 * 60.0 / bpm)
        .round()
        .clamp(1.0, u64::MAX as f64) as u64
}

fn snap_frame(frame: i64, spacing: i64) -> i64 {
    let spacing = spacing.max(1);
    let lower = frame.div_euclid(spacing) * spacing;
    let remainder = frame.rem_euclid(spacing);
    if remainder.saturating_mul(2) >= spacing {
        lower.saturating_add(spacing)
    } else {
        lower
    }
}

fn musical_position(frame: Frame, sample_rate: u32, bpm: f64, beats_per_bar: u8) -> String {
    let beat = frames_per_beat(sample_rate, bpm) as i64;
    let beat_number = frame.0.div_euclid(beat);
    let bar = beat_number.div_euclid(beats_per_bar as i64) + 1;
    let within = beat_number.rem_euclid(beats_per_bar as i64) + 1;
    format!("{bar}.{within}")
}

fn seed_demo(editor: &mut ArrangementEditor) -> Result<(), crate::arrangement::ArrangementError> {
    let beat = frames_per_beat(editor.state().sample_rate, 120.0);
    let bar = beat * 4;
    let audio = editor.create_track("Source mix", TrackKind::Audio)?;
    let percussion = editor.create_track("Deprojected hits", TrackKind::Pattern)?;
    let automation = editor.create_track("Spectral motion", TrackKind::Automation)?;
    let source = editor.create_audio_clip(
        audio,
        "Like a Pen · source",
        FrameRange::from_start_and_len(Frame::ZERO, bar * 8)?,
        AssetId::from_raw(1),
        crate::arrangement::SourceRange::new(0, bar * 8)?,
    )?;
    editor.create_audio_clip(
        audio,
        "reverb tail hypothesis",
        FrameRange::from_start_and_len(Frame((bar * 9) as i64), bar * 3)?,
        AssetId::from_raw(2),
        crate::arrangement::SourceRange::new(bar * 8, bar * 11)?,
    )?;
    editor.create_pattern_clip(
        percussion,
        "hit family A · 16 steps",
        FrameRange::from_start_and_len(Frame::ZERO, bar * 4)?,
        PatternId::from_raw(1),
    )?;
    editor.create_pattern_clip(
        percussion,
        "pitch-slide family",
        FrameRange::from_start_and_len(Frame((bar * 4) as i64), bar * 4)?,
        PatternId::from_raw(2),
    )?;
    editor.create_automation_clip(
        automation,
        "filter cutoff hypothesis",
        FrameRange::from_start_and_len(Frame::ZERO, bar * 12)?,
        ParameterId::from_raw(1),
    )?;
    editor.selection.clips = BTreeSet::from([source]);
    editor.selection.tracks = BTreeSet::from([audio]);
    Ok(())
}

fn track_kind_name(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Audio => "Audio",
        TrackKind::Pattern => "Pattern",
        TrackKind::Automation => "Automation",
        TrackKind::Hybrid => "Hybrid",
        TrackKind::Group => "Group",
    }
}

fn track_color(kind: TrackKind) -> u32 {
    match kind {
        TrackKind::Audio => CYAN,
        TrackKind::Pattern => MAGENTA,
        TrackKind::Automation => AMBER,
        TrackKind::Hybrid => LIME,
        TrackKind::Group => MUTED,
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn grouped_u64(value: u64) -> String {
    grouped_i64(value.min(i64::MAX as u64) as i64)
}

fn grouped_i64(value: i64) -> String {
    let negative = value < 0;
    let digits = value.unsigned_abs().to_string();
    let mut grouped =
        String::with_capacity(digits.len() + digits.len() / 3 + usize::from(negative));
    if negative {
        grouped.push('−');
    }
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(character);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrangement::TrackId;

    struct PreviewFixture;

    impl ArrangementPreviewResolver for PreviewFixture {
        fn media_asset(&self, _: crate::assets::AssetId) -> Option<ArrangementWaveformSource> {
            None
        }

        fn dropped_pattern(
            &self,
            _: crate::sequencer::PatternId,
        ) -> Option<ArrangementPatternPreview> {
            Some(ArrangementPatternPreview {
                length_frames: 96_000,
                pulses: Vec::new(),
            })
        }

        fn placed_pattern(
            &self,
            _: crate::arrangement::PatternId,
        ) -> Option<ArrangementPatternPreview> {
            None
        }
    }

    fn shared_audio_editor() -> (SharedArrangementEditor, TrackId, ClipId) {
        let mut editor = ArrangementEditor::new(48_000).unwrap();
        let track = editor.create_track("Audio", TrackKind::Audio).unwrap();
        let clip = editor
            .create_audio_clip(
                track,
                "source",
                FrameRange::from_start_and_len(Frame::ZERO, 48_000).unwrap(),
                AssetId::from_raw(1),
                crate::arrangement::SourceRange::new(0, 48_000).unwrap(),
            )
            .unwrap();
        (Arc::new(Mutex::new(editor)), track, clip)
    }

    #[test]
    fn shared_editor_publishes_a_successful_edit_atomically() {
        let (shared, track, clip) = shared_audio_editor();
        let (result, snapshot) = mutate_shared_editor(&shared, |editor| {
            editor.move_clip(clip, track, Frame(24_000))
        });

        result.unwrap();
        let published = lock_editor(&shared);
        assert_eq!(
            published.state().clip(clip).unwrap().placement.start,
            Frame(24_000)
        );
        assert_eq!(published.revision(), snapshot.revision());
    }

    #[test]
    fn shared_editor_publishes_successful_undo_and_redo_atomically() {
        let (shared, track, clip) = shared_audio_editor();
        mutate_shared_editor(&shared, |editor| {
            editor.move_clip(clip, track, Frame(24_000))
        })
        .0
        .unwrap();

        let (undo, undo_snapshot) = mutate_shared_editor(&shared, ArrangementEditor::undo);
        assert!(undo.unwrap().is_some());
        {
            let published = lock_editor(&shared);
            assert_eq!(
                published.state().clip(clip).unwrap().placement.start,
                Frame::ZERO
            );
            assert_eq!(published.revision(), undo_snapshot.revision());
        }

        let (redo, redo_snapshot) = mutate_shared_editor(&shared, ArrangementEditor::redo);
        assert!(redo.unwrap().is_some());
        let published = lock_editor(&shared);
        assert_eq!(
            published.state().clip(clip).unwrap().placement.start,
            Frame(24_000)
        );
        assert_eq!(published.revision(), redo_snapshot.revision());
    }

    #[test]
    fn visible_clip_is_clipped_to_the_viewport() {
        let viewport = ArrangementViewport::new(Frame(100), Frame(300), 10);
        let visible =
            visible_clip(FrameRange::new(Frame(50), Frame(200)).unwrap(), viewport).unwrap();
        assert!((visible.left - 0.0).abs() < 1.0e-6);
        assert!((visible.width - 0.5).abs() < 1.0e-6);
        assert!(visible_clip(FrameRange::new(Frame(0), Frame(100)).unwrap(), viewport).is_none());
    }

    #[test]
    fn zoom_preserves_anchor_fraction() {
        let mut viewport = ArrangementViewport::new(Frame(0), Frame(1_000), 10);
        viewport.zoom_around(Frame(250), 0.5);
        assert_eq!((viewport.start.0, viewport.end.0), (125, 625));
        assert!((viewport.fraction(Frame(250)) - 0.25).abs() < 1.0e-6);
    }

    #[test]
    fn offscreen_drag_mapping_does_not_fold_back_into_the_viewport() {
        let viewport = ArrangementViewport::new(Frame(10_000), Frame(20_000), 10);
        assert_eq!(viewport.frame_at_fraction(-0.25), Frame(10_000));
        assert_eq!(viewport.frame_at_fraction(1.25), Frame(20_000));
        assert_eq!(viewport.frame_at_unclamped_fraction(-0.25), Frame(7_500));
        assert_eq!(viewport.frame_at_unclamped_fraction(1.25), Frame(22_500));
    }

    #[test]
    fn edge_scroll_is_directional_bounded_and_absent_in_the_safe_center() {
        assert_eq!(plan_edge_scroll(100.0, 400.0, 300.0), None);
        let left = plan_edge_scroll(100.0, 400.0, 100.0).unwrap();
        let right = plan_edge_scroll(100.0, 400.0, 500.0).unwrap();
        assert_eq!(left.pan_fraction, -EDGE_SCROLL_MAX_FRACTION);
        assert_eq!(right.pan_fraction, EDGE_SCROLL_MAX_FRACTION);
        assert!(
            plan_edge_scroll(100.0, 400.0, 115.0)
                .unwrap()
                .pan_fraction
                .abs()
                < EDGE_SCROLL_MAX_FRACTION
        );
        assert_eq!(plan_edge_scroll(0.0, 0.0, 0.0), None);
        assert_eq!(plan_edge_scroll(0.0, 100.0, f64::NAN), None);
    }

    #[test]
    fn edge_scroll_pan_preserves_zoom_and_does_not_own_playhead_or_loop() {
        let mut viewport = ArrangementViewport::new(Frame(10_000), Frame(20_000), 10);
        let playhead = Frame(12_345);
        let loop_range = FrameRange::new(Frame(11_000), Frame(14_000)).unwrap();
        let plan = plan_edge_scroll(0.0, 500.0, 500.0).unwrap();
        viewport.pan(plan.pan_fraction);
        assert_eq!(viewport.span(), 10_000);
        assert_eq!(playhead, Frame(12_345));
        assert_eq!(
            loop_range,
            FrameRange::new(Frame(11_000), Frame(14_000)).unwrap()
        );
    }

    #[test]
    fn playhead_follow_preserves_zoom_and_never_assumes_song_start() {
        let mut viewport = ArrangementViewport::new(Frame(10_000), Frame(20_000), 10);
        assert!(viewport.ensure_visible(Frame(31_000), 0.2));
        assert_eq!(viewport.span(), 10_000);
        assert_eq!(
            (viewport.start, viewport.end),
            (Frame(29_000), Frame(39_000))
        );
        assert!(!viewport.ensure_visible(Frame(34_000), 0.2));
        assert_eq!(
            (viewport.start, viewport.end),
            (Frame(29_000), Frame(39_000))
        );
    }

    #[test]
    fn ruler_click_jitter_stays_a_click_instead_of_clearing_selection() {
        let original = FrameRange::new(Frame(100), Frame(300)).unwrap();
        let mut gesture = RulerGesture {
            mode: RulerGestureMode::TimeSelection {
                anchor: Frame(200),
                original: Some(original),
            },
            current: Frame(200),
            press_x: 80.0,
            dragged: false,
        };

        assert_eq!(
            gesture.update(Frame(201), 82.9),
            RulerGesturePreview::default()
        );
        assert!(!gesture.dragged);
        assert_eq!(
            gesture.update(Frame(260), 83.0).time,
            Some(FrameRange::new(Frame(200), Frame(260)).unwrap())
        );
        assert!(gesture.dragged);
    }

    #[test]
    fn loop_edge_drag_clamps_without_collapsing_or_replacing_time_selection() {
        let original = FrameRange::new(Frame(1_000), Frame(2_000)).unwrap();
        let time = FrameRange::new(Frame(400), Frame(800)).unwrap();
        let mut gesture = RulerGesture {
            mode: RulerGestureMode::LoopStart { original },
            current: original.start,
            press_x: 100.0,
            dragged: false,
        };

        let preview = gesture.update(Frame(3_000), 110.0);
        assert_eq!(
            preview.loop_range,
            Some(FrameRange::new(Frame(1_999), Frame(2_000)).unwrap())
        );
        assert_eq!(preview.time, None);
        assert_eq!(time, FrameRange::new(Frame(400), Frame(800)).unwrap());
    }

    #[test]
    fn dragging_loop_body_preserves_length_and_supports_negative_timeline_frames() {
        let original = FrameRange::new(Frame(1_000), Frame(2_500)).unwrap();
        let mut gesture = RulerGesture {
            mode: RulerGestureMode::LoopMove {
                original,
                grabbed_at: Frame(1_500),
            },
            current: Frame(1_500),
            press_x: 200.0,
            dragged: false,
        };

        let moved = gesture.update(Frame(-500), 160.0).loop_range.unwrap();
        assert_eq!(moved, FrameRange::new(Frame(-1_000), Frame(500)).unwrap());
        assert_eq!(moved.len(), original.len());
    }

    #[test]
    fn selection_modes_are_deterministic_and_typed() {
        let first = ClipId::from_raw(1);
        let second = ClipId::from_raw(2);
        let mut selection = BTreeSet::from([first]);
        apply_id_selection(&mut selection, BTreeSet::from([second]), SelectionMode::Add);
        assert_eq!(selection, BTreeSet::from([first, second]));
        apply_id_selection(
            &mut selection,
            BTreeSet::from([first, ClipId::from_raw(3)]),
            SelectionMode::Toggle,
        );
        assert_eq!(selection, BTreeSet::from([second, ClipId::from_raw(3)]));
    }

    #[test]
    fn snapping_handles_negative_frames_and_late_ties() {
        assert_eq!(snap_frame(149, 100), 100);
        assert_eq!(snap_frame(150, 100), 200);
        assert_eq!(snap_frame(-49, 100), 0);
        assert_eq!(snap_frame(-50, 100), 0);
        assert_eq!(snap_frame(-51, 100), -100);
    }

    #[test]
    fn drop_preview_preserves_typed_source_and_requests_track_creation() {
        let source = AssetDrag {
            asset: crate::assets::AssetId(12),
            source_range: Some(
                crate::assets::AssetFrameRange::new(
                    crate::assets::SampleFrames(100),
                    crate::assets::SampleFrames(400),
                )
                .unwrap(),
            ),
        };
        let preview = build_drop_preview(
            DragPayload::Asset(source),
            DropTarget::ArrangementCanvas { at: Frame(24_000) },
            DragModifiers::default(),
            48_000,
            None,
        );
        assert_eq!(preview.create_track, Some(TrackKind::Audio));
        assert_eq!(
            preview.placement, None,
            "unknown source rate is never guessed"
        );
        assert!(matches!(
            preview.intent,
            Ok(DropIntent::InsertAudio {
                source: actual,
                track: None,
                at: Frame(24_000),
            }) if actual == source
        ));
    }

    #[test]
    fn pattern_drop_preview_uses_resolved_definition_length() {
        let pattern = crate::sequencer::PatternId::from_raw(9);
        let preview = build_drop_preview(
            DragPayload::Pattern(pattern),
            DropTarget::ArrangementCanvas { at: Frame(48_000) },
            DragModifiers::default(),
            48_000,
            Some(&PreviewFixture),
        );
        assert_eq!(preview.create_track, Some(TrackKind::Pattern));
        assert_eq!(
            preview.placement,
            Some(FrameRange::new(Frame(48_000), Frame(144_000)).unwrap())
        );
    }

    #[test]
    fn source_duration_conversion_is_conservative_and_exact_at_equal_rates() {
        assert_eq!(project_frame_count(44_100, 44_100, 48_000), Some(48_000));
        assert_eq!(project_frame_count(1, 44_100, 48_000), Some(2));
        assert_eq!(project_frame_count(2_000, 48_000, 48_000), Some(2_000));
    }

    #[test]
    fn musical_grid_is_sample_rate_and_tempo_aware() {
        assert_eq!(frames_per_beat(48_000, 120.0), 24_000);
        assert_eq!(
            snap_frames(48_000, 120.0, 4, SnapDivision::Bar),
            Some(96_000)
        );
        assert_eq!(musical_position(Frame(120_000), 48_000, 120.0, 4), "2.2");
    }

    #[test]
    fn demo_contains_all_three_editable_clip_kinds() {
        let mut editor = ArrangementEditor::new(48_000).unwrap();
        seed_demo(&mut editor).unwrap();
        let kinds: BTreeSet<_> = editor
            .state()
            .clips
            .values()
            .map(|clip| match clip.content {
                ClipContent::Audio(_) => 0,
                ClipContent::Pattern(_) => 1,
                ClipContent::Automation(_) => 2,
            })
            .collect();
        assert_eq!(kinds, BTreeSet::from([0, 1, 2]));
        editor.state().validate().unwrap();
    }

    #[test]
    fn ruler_marks_bar_boundaries_and_exact_samples() {
        let viewport = ArrangementViewport::new(Frame(0), Frame(384_000), 1_000);
        let ticks = ruler_ticks(viewport, 48_000, 120.0, 4);
        let first = ticks.iter().find(|tick| tick.frame == Frame::ZERO).unwrap();
        assert!(first.major);
        assert!(first.label.contains("0f"));
        assert!(ticks
            .iter()
            .any(|tick| tick.frame == Frame(96_000) && tick.major));
    }

    #[test]
    fn ruler_density_remains_readable_for_a_full_song() {
        let viewport = ArrangementViewport::new(Frame(0), Frame(16_468_704), 441);
        let ticks = ruler_ticks(viewport, 44_100, 120.0, 4);
        assert!(
            ticks.len() <= 7,
            "{} ruler labels would overlap",
            ticks.len()
        );
        assert!(ticks.iter().all(|tick| tick.frame >= viewport.start));
        assert!(ticks.iter().all(|tick| tick.frame <= viewport.end));
        assert!(ticks.iter().all(|tick| tick.major));
    }

    #[test]
    fn tempo_ruler_tracks_project_tempo_changes_and_stays_bounded() {
        let mut tempo = TempoMap::common_time(48_000, 120.0).unwrap();
        tempo
            .set_tempo(
                BeatTime(4 * crate::sequencer::PPQ),
                crate::sequencer::Tempo::from_bpm(60.0).unwrap(),
            )
            .unwrap();
        let viewport = ArrangementViewport::new(Frame(0), Frame(384_000), 1_000);
        let ticks = tempo_ruler_ticks(&tempo, viewport);
        assert!(ticks.iter().any(|tick| tick.frame == Frame(96_000)));
        assert!(ticks.iter().any(|tick| tick.frame == Frame(144_000)));
        assert!(ticks.len() <= 80);
    }
}
