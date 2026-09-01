//! GPUI piano-roll and step-sequencer editor backed by [`crate::sequencer`].
//!
//! Project-backed instances render a short snapshot and may hold one optimistic
//! preview, but authored mutation crosses [`PatternActionCallback`]. Standalone
//! instances lower the same gestures to validated `SequencerCommand`s, so undo,
//! scheduling, and persistence still observe one revision stream.

mod piano_workflow;
mod step_workflow;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gpui::{
    actions, canvas, div, point, prelude::*, px, quad, rgb, rgba, App, Bounds, Context,
    FocusHandle, Focusable, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Render, ScrollWheelEvent, SharedString, Subscription, Window,
};

use crate::arrangement::TrackId;
use crate::pattern_actions::{
    CreatePatternIntent, PatternAction, PatternActionCallback, PatternActionIntent, PatternEdit,
    PatternEditIntent, PatternEditorMode as ActionEditorMode, PatternEditorTarget,
    TriggerTargetOption,
};
use crate::pattern_authoring::{self, DivergedOverwrite};
use crate::pattern_use_graph::{
    MakeOccurrenceUniqueIntent, PatternOccurrenceTarget, PatternRevealData, PatternUseSummary,
};
use crate::project_controller::{
    BeginPatternGestureIntent, PatternAuditionPad, PatternAuditionRequest, PatternAuditionScope,
    PatternAuditionSelection, PatternCyclePublication, PatternEditPublication,
    PatternEditorHydration, PatternGestureKind, PatternGestureReceipt, PatternLoopAuditionPlan,
    PatternWorkflowCallback, PatternWorkflowDispatchReceipt, PatternWorkflowError,
    PatternWorkflowIntent, PatternWorkflowOutcome, PatternWorkflowRequest,
    PatternWorkflowRequestId, SharedPatternAuditionCallback,
};
use crate::sample_kit::SampleTargetRef;
use crate::sequencer::{
    quantize_notes, Articulation, BeatDuration, BeatTime, NoteEvent, NoteId, NotePattern,
    NotePitch, PatternContent, PatternDefinition, PatternId, PatternOrigin, PerNoteExpression,
    QuantizeSpec, SampleAssetId, Sequencer, SequencerCommand, StepEvent, StepLane, StepLaneId,
    StepPattern, TempoMap, TriggerTarget, PPQ,
};
pub use piano_workflow::PitchScale;
use piano_workflow::{NoteBatch, NoteMarquee, PianoGestureResolution, PianoGestureTransaction};
pub use step_workflow::StepKey;
use step_workflow::StepPropertyDelta;

actions!(
    audec_sequencer,
    [
        ToggleEditorMode,
        EditorUndo,
        EditorRedo,
        EditorDelete,
        EditorMoveLeft,
        EditorMoveRight,
        EditorMoveUp,
        EditorMoveDown,
        EditorResizeLeft,
        EditorResizeRight,
        EditorZoomIn,
        EditorZoomOut,
        EditorPanLeft,
        EditorPanRight,
        EditorQuantize,
        EditorDuplicate,
        EditorSelectAll,
        EditorVelocityUp,
        EditorVelocityDown,
        EditorProbabilityUp,
        EditorProbabilityDown,
        EditorRatchetUp,
        EditorRatchetDown,
        EditorMicrotimingLater,
        EditorMicrotimingEarlier,
        EditorCycleScale,
        EditorAudition,
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

const LABEL_WIDTH: f32 = 112.0;
const PIANO_ROW_HEIGHT: f32 = 22.0;
const PIANO_ROWS: usize = 24;
const STEP_ROW_HEIGHT: f32 = 44.0;
const MIN_TICKS_PER_PIXEL: f64 = 0.25;
const MAX_TICKS_PER_PIXEL: f64 = 240.0;
static NEXT_EDITOR_SESSION: AtomicU64 = AtomicU64::new(1);

/// Install these once next to audec's other key bindings.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", ToggleEditorMode, Some("AudecSequencer")),
        KeyBinding::new("cmd-z", EditorUndo, Some("AudecSequencer")),
        KeyBinding::new("cmd-shift-z", EditorRedo, Some("AudecSequencer")),
        KeyBinding::new("backspace", EditorDelete, Some("AudecSequencer")),
        KeyBinding::new("delete", EditorDelete, Some("AudecSequencer")),
        KeyBinding::new("left", EditorMoveLeft, Some("AudecSequencer")),
        KeyBinding::new("right", EditorMoveRight, Some("AudecSequencer")),
        KeyBinding::new("up", EditorMoveUp, Some("AudecSequencer")),
        KeyBinding::new("down", EditorMoveDown, Some("AudecSequencer")),
        KeyBinding::new("shift-left", EditorResizeLeft, Some("AudecSequencer")),
        KeyBinding::new("shift-right", EditorResizeRight, Some("AudecSequencer")),
        KeyBinding::new("=", EditorZoomIn, Some("AudecSequencer")),
        KeyBinding::new("-", EditorZoomOut, Some("AudecSequencer")),
        KeyBinding::new("shift-[", EditorPanLeft, Some("AudecSequencer")),
        KeyBinding::new("shift-]", EditorPanRight, Some("AudecSequencer")),
        KeyBinding::new("q", EditorQuantize, Some("AudecSequencer")),
        KeyBinding::new("cmd-d", EditorDuplicate, Some("AudecSequencer")),
        KeyBinding::new("cmd-a", EditorSelectAll, Some("AudecSequencer")),
        KeyBinding::new("]", EditorVelocityUp, Some("AudecSequencer")),
        KeyBinding::new("[", EditorVelocityDown, Some("AudecSequencer")),
        KeyBinding::new("p", EditorProbabilityUp, Some("AudecSequencer")),
        KeyBinding::new("shift-p", EditorProbabilityDown, Some("AudecSequencer")),
        KeyBinding::new("r", EditorRatchetUp, Some("AudecSequencer")),
        KeyBinding::new("shift-r", EditorRatchetDown, Some("AudecSequencer")),
        KeyBinding::new(".", EditorMicrotimingLater, Some("AudecSequencer")),
        KeyBinding::new(",", EditorMicrotimingEarlier, Some("AudecSequencer")),
        KeyBinding::new("s", EditorCycleScale, Some("AudecSequencer")),
        KeyBinding::new("a", EditorAudition, Some("AudecSequencer")),
    ]);
}

/// Narrow host seam for a short, routed piano-key/note preview. The host owns
/// audio lifetime; the editor never reaches around the project controller.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PianoAuditionRequest {
    pub pattern: PatternId,
    pub occurrence: Option<PatternOccurrenceTarget>,
    pub track: Option<TrackId>,
    pub cycle_index: u64,
    pub performance_seed: u64,
    pub instrument: u64,
    pub midi_key: u8,
    pub velocity: f32,
    pub duration: BeatDuration,
}

pub type PianoAuditionCallback = Arc<dyn Fn(PianoAuditionRequest) + Send + Sync + 'static>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SequencerAuditionAvailability {
    Available,
    Unavailable { reason: SharedString },
}

impl SequencerAuditionAvailability {
    pub fn unavailable(reason: impl Into<SharedString>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }

    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    pub fn unavailable_reason(&self) -> Option<&str> {
        match self {
            Self::Available => None,
            Self::Unavailable { reason } => Some(reason.as_ref()),
        }
    }
}

impl Default for SequencerAuditionAvailability {
    fn default() -> Self {
        Self::unavailable("Shared pattern audition is not connected")
    }
}

/// Musical viewport state suitable for workspace/session persistence. Width
/// and height remain renderer concerns; this state is invariant to pane size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PianoViewportState {
    pub start_tick: i64,
    pub ticks_per_pixel: f64,
    pub top_midi_key: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditorMode {
    PianoRoll,
    Steps,
}

impl From<EditorMode> for ActionEditorMode {
    fn from(value: EditorMode) -> Self {
        match value {
            EditorMode::PianoRoll => Self::PianoRoll,
            EditorMode::Steps => Self::Steps,
        }
    }
}

impl From<ActionEditorMode> for EditorMode {
    fn from(value: ActionEditorMode) -> Self {
        match value {
            ActionEditorMode::PianoRoll => Self::PianoRoll,
            ActionEditorMode::Steps => Self::Steps,
        }
    }
}

/// Injection boundary used by a pane, floating window, or ProjectDocument
/// adapter. Either pattern may be omitted; mode switching then stays on the
/// available editor.
#[derive(Clone)]
pub struct SequencerEditorSource {
    pub sequencer: Arc<Mutex<Sequencer>>,
    pub note_pattern: Option<PatternId>,
    pub step_pattern: Option<PatternId>,
    pub title: SharedString,
    /// Controller-resolved destinations for expression bindings. Picker order
    /// is presentation-only; `TriggerTarget` remains the stable selection.
    pub trigger_targets: Vec<TriggerTargetOption>,
    /// Exact sampler destinations retain kit/pad/zone identity; the controller
    /// resolves or allocates their durable sequencer alias on commit.
    pub pad_targets: Vec<PatternPadTargetOption>,
    /// Optional authoritative occurrence/use data for navigation, Make Unique,
    /// and placement-cycle audition.
    pub workflow: Option<PatternEditorWorkflowContext>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatternPadTargetOption {
    pub target: SampleTargetRef,
    pub sequencer_alias: Option<SampleAssetId>,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternEditorWorkflowContext {
    pub occurrence: Option<PatternOccurrenceTarget>,
    pub uses: PatternUseSummary,
    pub reveal: PatternRevealData,
}

impl SequencerEditorSource {
    pub fn new(
        sequencer: Arc<Mutex<Sequencer>>,
        note_pattern: Option<PatternId>,
        step_pattern: Option<PatternId>,
        title: impl Into<SharedString>,
    ) -> Self {
        Self {
            sequencer,
            note_pattern,
            step_pattern,
            title: title.into(),
            trigger_targets: Vec::new(),
            pad_targets: Vec::new(),
            workflow: None,
        }
    }

    pub fn with_trigger_targets(mut self, targets: Vec<TriggerTargetOption>) -> Self {
        self.trigger_targets = targets;
        self
    }

    pub fn with_pad_targets(mut self, targets: Vec<PatternPadTargetOption>) -> Self {
        self.pad_targets = targets;
        self
    }

    pub fn with_workflow_context(mut self, context: PatternEditorWorkflowContext) -> Self {
        self.workflow = Some(context);
        self
    }

    pub fn from_workflow_hydration(
        sequencer: Arc<Mutex<Sequencer>>,
        hydration: PatternEditorHydration,
        title: impl Into<SharedString>,
    ) -> Self {
        let mut source = Self::targeted(sequencer, hydration.target, title);
        source.workflow = Some(PatternEditorWorkflowContext {
            occurrence: hydration.occurrence,
            uses: hydration.uses,
            reveal: hydration.reveal,
        });
        source
    }

    /// Construct the single-target source used by a dynamic workspace item.
    /// The legacy paired-pattern constructor remains useful for the demo and
    /// for hosts that intentionally offer an in-place mode switch.
    pub fn targeted(
        sequencer: Arc<Mutex<Sequencer>>,
        target: PatternEditorTarget,
        title: impl Into<SharedString>,
    ) -> Self {
        match target.mode {
            ActionEditorMode::PianoRoll => Self::new(sequencer, Some(target.pattern), None, title),
            ActionEditorMode::Steps => Self::new(sequencer, None, Some(target.pattern), title),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Selection {
    Note(NoteId),
    Step(StepLaneId, u32),
}

#[derive(Clone, Debug)]
enum DragGesture {
    MoveNotes {
        origin_x: f32,
        origin_y: f32,
        original: NoteBatch,
    },
    ResizeNotes {
        origin_x: f32,
        original: NoteBatch,
    },
    VelocityNotes {
        origin_y: f32,
        original: NoteBatch,
    },
    MarqueeNotes {
        origin_x: f32,
        origin_y: f32,
        current_x: f32,
        current_y: f32,
        baseline: BTreeSet<NoteId>,
    },
    MarqueeSteps {
        origin_step: u32,
        origin_lane: usize,
        baseline: BTreeSet<StepKey>,
    },
    MoveStep {
        lane: StepLaneId,
        index: u32,
        event: StepEvent,
    },
}

#[derive(Clone)]
enum LaneTargetChoice {
    Trigger(TriggerTarget),
    Pad(SampleTargetRef, Option<SampleAssetId>),
}

/// Pure time/pitch geometry shared by rendering, hit testing, and tests.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PianoGeometry {
    pub width: f32,
    pub start_tick: i64,
    pub ticks_per_pixel: f64,
    pub top_midi_key: u8,
    pub row_height: f32,
    pub rows: usize,
}

impl PianoGeometry {
    pub fn x_for_tick(self, tick: i64) -> f32 {
        ((tick - self.start_tick) as f64 / self.ticks_per_pixel) as f32
    }

    pub fn tick_at_x(self, x: f32) -> i64 {
        self.start_tick
            .saturating_add((f64::from(x) * self.ticks_per_pixel).floor() as i64)
    }

    pub fn snapped_tick_at_x(self, x: f32, grid: u64) -> i64 {
        snap_tick(self.tick_at_x(x), grid).max(0)
    }

    pub fn y_for_key(self, midi_key: u8) -> f32 {
        (i16::from(self.top_midi_key) - i16::from(midi_key)) as f32 * self.row_height
    }

    pub fn key_at_y(self, y: f32) -> u8 {
        let row = (y / self.row_height).floor().max(0.0) as i16;
        (i16::from(self.top_midi_key) - row).clamp(0, 127) as u8
    }

    pub fn visible_end_tick(self) -> i64 {
        self.tick_at_x(self.width)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StepGeometry {
    pub width: f32,
    pub start_tick: i64,
    pub ticks_per_pixel: f64,
    pub resolution: u64,
    pub row_height: f32,
    pub rows: usize,
}

impl StepGeometry {
    pub fn x_for_step(self, step: u32) -> f32 {
        (((u64::from(step) * self.resolution) as i128 - self.start_tick as i128) as f64
            / self.ticks_per_pixel) as f32
    }

    pub fn step_at_x(self, x: f32) -> u32 {
        let tick = self
            .start_tick
            .saturating_add((f64::from(x) * self.ticks_per_pixel).floor() as i64)
            .max(0) as u64;
        (tick / self.resolution.max(1)).min(u64::from(u32::MAX)) as u32
    }

    pub fn lane_at_y(self, y: f32) -> Option<usize> {
        let lane = (y / self.row_height).floor() as isize;
        (lane >= 0 && lane < self.rows as isize).then_some(lane as usize)
    }
}

pub struct SequencerEditor {
    source: SequencerEditorSource,
    mode: EditorMode,
    expected_project_revision: u64,
    callback: Option<PatternActionCallback>,
    workflow_callback: Option<PatternWorkflowCallback>,
    editor_session: u64,
    next_workflow_request: u64,
    pending_workflow: BTreeSet<PatternWorkflowRequestId>,
    active_gesture: Option<PatternGestureReceipt>,
    last_publication: Option<PatternEditPublication>,
    cycle_publication: Option<PatternCyclePublication>,
    audition_plan: Option<PatternLoopAuditionPlan>,
    reveal: Option<PatternRevealData>,
    optimistic_pattern: Option<PatternDefinition>,
    selection: Option<Selection>,
    selected_notes: BTreeSet<NoteId>,
    selected_steps: BTreeSet<StepKey>,
    drag: Option<DragGesture>,
    piano_gesture: Option<PianoGestureTransaction>,
    grid_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    start_tick: i64,
    ticks_per_pixel: f64,
    top_midi_key: u8,
    quantize_grid: u64,
    pitch_scale: PitchScale,
    note_instrument: Option<u64>,
    audition_callback: Option<PianoAuditionCallback>,
    shared_audition_callback: Option<SharedPatternAuditionCallback>,
    audition_availability: SequencerAuditionAvailability,
    swing: f32,
    expression: String,
    expression_focused: bool,
    expression_bindings: BTreeMap<String, TriggerTarget>,
    preview_cycle: u64,
    preview_seed: u64,
    status: Option<String>,
    focus_handle: FocusHandle,
    focus_subscription: Option<Subscription>,
}

fn complete_external_workflow_failure<Optimistic, Gesture, Drag>(
    pending: &mut BTreeSet<PatternWorkflowRequestId>,
    request: PatternWorkflowRequestId,
    status: &mut Option<String>,
    optimistic: &mut Option<Optimistic>,
    active_gesture: &mut Option<Gesture>,
    drag: &mut Option<Drag>,
    display_message: String,
) -> bool {
    if !pending.remove(&request) {
        return false;
    }
    *status = Some(display_message);
    *optimistic = None;
    *active_gesture = None;
    *drag = None;
    true
}

impl SequencerEditor {
    pub fn new(source: SequencerEditorSource, cx: &mut Context<Self>) -> Self {
        let mode = if source.note_pattern.is_some() {
            EditorMode::PianoRoll
        } else {
            EditorMode::Steps
        };
        let swing = source
            .step_pattern
            .and_then(|id| {
                source
                    .sequencer
                    .lock()
                    .ok()?
                    .patterns()
                    .get(id)
                    .and_then(|pattern| match &pattern.content {
                        PatternContent::Steps(steps) => Some(steps.swing),
                        _ => None,
                    })
            })
            .unwrap_or(0.0);
        let expression_pattern = source
            .step_pattern
            .and_then(|id| source.sequencer.lock().ok()?.patterns().get(id).cloned());
        let expression = expression_pattern
            .as_ref()
            .and_then(|pattern| match &pattern.origin {
                PatternOrigin::Expression { source, .. } => Some(source.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let expression_bindings = expression_pattern
            .as_ref()
            .map(pattern_authoring::bindings_for_pattern)
            .unwrap_or_default();
        let reveal = source
            .workflow
            .as_ref()
            .map(|context| context.reveal.clone());
        let note_instrument =
            source
                .trigger_targets
                .iter()
                .find_map(|option| match &option.target {
                    TriggerTarget::InstrumentNote { instrument, .. } => Some(*instrument),
                    _ => None,
                });
        Self {
            source,
            mode,
            expected_project_revision: 0,
            callback: None,
            workflow_callback: None,
            editor_session: NEXT_EDITOR_SESSION.fetch_add(1, Ordering::Relaxed),
            next_workflow_request: 1,
            pending_workflow: BTreeSet::new(),
            active_gesture: None,
            last_publication: None,
            cycle_publication: None,
            audition_plan: None,
            reveal,
            optimistic_pattern: None,
            selection: None,
            selected_notes: BTreeSet::new(),
            selected_steps: BTreeSet::new(),
            drag: None,
            piano_gesture: None,
            grid_bounds: Arc::new(Mutex::new(None)),
            start_tick: 0,
            ticks_per_pixel: 24.0,
            top_midi_key: 83,
            quantize_grid: (PPQ / 4) as u64,
            pitch_scale: PitchScale::default(),
            note_instrument,
            audition_callback: None,
            shared_audition_callback: None,
            audition_availability: SequencerAuditionAvailability::default(),
            swing,
            expression,
            expression_focused: false,
            expression_bindings,
            preview_cycle: 0,
            preview_seed: 0,
            status: None,
            focus_handle: cx.focus_handle(),
            focus_subscription: None,
        }
    }

    /// A musically varied backend-backed project for embedding before the
    /// ProjectDocument adapter supplies real patterns.
    pub fn demo(cx: &mut Context<Self>) -> Self {
        Self::new(demo_source(), cx)
    }

    pub fn source(&self) -> &SequencerEditorSource {
        &self.source
    }

    pub fn mode(&self) -> EditorMode {
        self.mode
    }

    /// Preferred aggregate-project constructor. The shared value is a read
    /// source; every user mutation is emitted through `callback`.
    pub fn from_project_source(
        source: SequencerEditorSource,
        expected_project_revision: u64,
        callback: PatternActionCallback,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut editor = Self::new(source, cx);
        editor.expected_project_revision = expected_project_revision;
        editor.callback = Some(callback);
        editor
    }

    /// Workflow-native project constructor. Durable actions, previews,
    /// audition, use navigation, and pointer gestures all cross one typed
    /// controller seam and return authoritative completion data.
    pub fn from_workflow_source(
        source: SequencerEditorSource,
        expected_project_revision: u64,
        callback: PatternWorkflowCallback,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut editor = Self::new(source, cx);
        editor.expected_project_revision = expected_project_revision;
        editor.workflow_callback = Some(callback);
        editor
    }

    pub fn set_callback(&mut self, callback: Option<PatternActionCallback>) {
        self.callback = callback;
    }

    pub fn set_workflow_callback(&mut self, callback: Option<PatternWorkflowCallback>) {
        self.workflow_callback = callback;
    }

    pub fn set_piano_audition_callback(&mut self, callback: Option<PianoAuditionCallback>) {
        self.audition_callback = callback;
    }

    /// Exact pattern/note/pad request seam for the shared project renderer.
    /// `PianoAuditionCallback` remains only for transient keyboard-key preview.
    pub fn set_shared_pattern_audition_callback(
        &mut self,
        callback: Option<SharedPatternAuditionCallback>,
    ) {
        self.shared_audition_callback = callback;
    }

    /// Host-owned truth about whether pattern/note audition can reach the
    /// shared renderer. This never installs playback behavior in the view.
    pub fn set_audition_availability(
        &mut self,
        availability: SequencerAuditionAvailability,
        cx: &mut Context<Self>,
    ) {
        if self.audition_availability == availability {
            return;
        }
        if let Some(reason) = availability.unavailable_reason() {
            self.status = Some(reason.to_owned());
        }
        self.audition_availability = availability;
        cx.notify();
    }

    pub fn audition_availability(&self) -> &SequencerAuditionAvailability {
        &self.audition_availability
    }

    pub fn selected_note_ids(&self) -> &BTreeSet<NoteId> {
        &self.selected_notes
    }

    pub fn selected_step_keys(&self) -> &BTreeSet<StepKey> {
        &self.selected_steps
    }

    pub fn piano_scale(&self) -> PitchScale {
        self.pitch_scale
    }

    pub fn piano_viewport(&self) -> PianoViewportState {
        PianoViewportState {
            start_tick: self.start_tick,
            ticks_per_pixel: self.ticks_per_pixel,
            top_midi_key: self.top_midi_key,
        }
    }

    pub fn set_piano_viewport(&mut self, viewport: PianoViewportState, cx: &mut Context<Self>) {
        self.start_tick = viewport.start_tick.max(0);
        self.ticks_per_pixel = if viewport.ticks_per_pixel.is_finite() {
            viewport
                .ticks_per_pixel
                .clamp(MIN_TICKS_PER_PIXEL, MAX_TICKS_PER_PIXEL)
        } else {
            self.ticks_per_pixel
        };
        self.top_midi_key = viewport.top_midi_key.clamp(23, 127);
        cx.notify();
    }

    pub fn piano_instrument(&self) -> Option<u64> {
        self.note_instrument
    }

    pub fn piano_gesture_preview(&self) -> Option<&NotePattern> {
        self.piano_gesture
            .as_ref()
            .map(PianoGestureTransaction::preview)
    }

    pub fn rollback_piano_gesture(&mut self, cx: &mut Context<Self>) -> bool {
        if self.piano_gesture.is_none() {
            return false;
        }
        self.cancel_editor_gesture("Piano gesture rolled back by host", cx);
        true
    }

    pub fn pending_workflow_requests(&self) -> usize {
        self.pending_workflow.len()
    }

    pub fn last_pattern_publication(&self) -> Option<&PatternEditPublication> {
        self.last_publication.as_ref()
    }

    pub fn cycle_publication(&self) -> Option<&PatternCyclePublication> {
        self.cycle_publication.as_ref()
    }

    pub fn audition_plan(&self) -> Option<&PatternLoopAuditionPlan> {
        self.audition_plan.as_ref()
    }

    pub fn reveal_data(&self) -> Option<&PatternRevealData> {
        self.reveal.as_ref()
    }

    pub fn use_summary(&self) -> Option<&PatternUseSummary> {
        self.source.workflow.as_ref().map(|context| &context.uses)
    }

    /// Deliver an asynchronously accepted workflow result. Unknown/stale IDs
    /// cannot mutate the editor.
    pub fn complete_workflow(
        &mut self,
        request: PatternWorkflowRequestId,
        result: Result<PatternWorkflowOutcome, PatternWorkflowError>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.pending_workflow.remove(&request) {
            return false;
        }
        self.apply_workflow_result(result, cx);
        true
    }

    /// Complete a request which crossed an adapter that can no longer retain
    /// [`PatternWorkflowError`] (for example `ProjectSession`, whose public
    /// error includes broader lifecycle failures). The message is explicitly
    /// external failure detail; it is never parsed back into a typed workflow
    /// error. Unknown, stale, and duplicate request IDs are inert.
    pub fn complete_workflow_failure(
        &mut self,
        request: PatternWorkflowRequestId,
        display_message: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let completed = complete_external_workflow_failure(
            &mut self.pending_workflow,
            request,
            &mut self.status,
            &mut self.optimistic_pattern,
            &mut self.active_gesture,
            &mut self.drag,
            display_message.into(),
        );
        if completed {
            cx.notify();
        }
        completed
    }

    pub fn set_project_revision(&mut self, revision: u64, cx: &mut Context<Self>) {
        if self.expected_project_revision != revision {
            self.expected_project_revision = revision;
            self.optimistic_pattern = None;
            cx.notify();
        }
    }

    /// Replace the project-owned read source after a controller commit.
    pub fn set_source_snapshot(
        &mut self,
        source: SequencerEditorSource,
        expected_project_revision: u64,
        cx: &mut Context<Self>,
    ) {
        self.source = source;
        self.reveal = self
            .source
            .workflow
            .as_ref()
            .map(|context| context.reveal.clone());
        if self.pattern_id_for(self.mode).is_none() {
            self.mode = if self.source.note_pattern.is_some() {
                EditorMode::PianoRoll
            } else {
                EditorMode::Steps
            };
        }
        self.expected_project_revision = expected_project_revision;
        self.optimistic_pattern = None;
        self.cycle_publication = None;
        self.audition_plan = None;
        self.selection = None;
        self.selected_notes.clear();
        self.selected_steps.clear();
        self.drag = None;
        self.piano_gesture = None;
        self.expression_focused = false;
        self.status = None;
        self.reload_authoring_state();
        cx.notify();
    }

    pub fn target(&self) -> Option<PatternEditorTarget> {
        self.pattern_id_for(self.mode)
            .map(|pattern| PatternEditorTarget {
                pattern,
                mode: self.mode.into(),
            })
    }

    /// Match the performance context used by a concrete arrangement
    /// placement. This changes only what the editor previews.
    pub fn set_preview_context(
        &mut self,
        cycle_index: u64,
        performance_seed: u64,
        cx: &mut Context<Self>,
    ) {
        self.preview_cycle = cycle_index;
        self.preview_seed = performance_seed;
        self.selection = None;
        self.selected_notes.clear();
        self.selected_steps.clear();
        cx.notify();
    }

    pub fn preview_context(&self) -> (u64, u64) {
        (self.preview_cycle, self.preview_seed)
    }

    /// Retarget this persistent workspace item without reconstructing its GPUI
    /// entity, focus handle, viewport, or input state.
    pub fn retarget_pattern(&mut self, target: PatternEditorTarget, cx: &mut Context<Self>) {
        self.mode = target.mode.into();
        match self.mode {
            EditorMode::PianoRoll => self.source.note_pattern = Some(target.pattern),
            EditorMode::Steps => self.source.step_pattern = Some(target.pattern),
        }
        self.selection = None;
        self.selected_notes.clear();
        self.selected_steps.clear();
        self.drag = None;
        self.piano_gesture = None;
        self.optimistic_pattern = None;
        self.preview_cycle = 0;
        self.reload_authoring_state();
        cx.notify();
    }

    fn reload_authoring_state(&mut self) {
        let pattern = self.stored_active_pattern();
        self.swing = pattern
            .as_ref()
            .and_then(|pattern| match &pattern.content {
                PatternContent::Steps(steps) => Some(steps.swing),
                PatternContent::Notes(_) => None,
            })
            .unwrap_or(0.0);
        self.expression = pattern
            .as_ref()
            .and_then(|pattern| match &pattern.origin {
                PatternOrigin::Expression { source, .. } => Some(source.clone()),
                _ => None,
            })
            .unwrap_or_default();
        self.expression_bindings = pattern
            .as_ref()
            .map(pattern_authoring::bindings_for_pattern)
            .unwrap_or_default();
    }

    fn prune_event_selection(&mut self) {
        let Some(pattern) = self.stored_active_pattern() else {
            self.selection = None;
            self.selected_notes.clear();
            self.selected_steps.clear();
            return;
        };
        match pattern.content {
            PatternContent::Notes(notes) => {
                self.selected_steps.clear();
                self.selected_notes
                    .retain(|note| notes.notes.contains_key(note));
                if !matches!(self.selection, Some(Selection::Note(note)) if notes.notes.contains_key(&note))
                {
                    self.selection = self
                        .selected_notes
                        .iter()
                        .next()
                        .copied()
                        .map(Selection::Note);
                }
            }
            PatternContent::Steps(steps) => {
                self.selected_notes.clear();
                self.selected_steps.retain(|(lane, step)| {
                    steps
                        .lanes
                        .get(lane)
                        .is_some_and(|lane| lane.steps.contains_key(step))
                });
                if !matches!(self.selection, Some(Selection::Step(lane, step)) if steps
                    .lanes
                    .get(&lane)
                    .is_some_and(|lane| lane.steps.contains_key(&step)))
                {
                    self.selection = self
                        .selected_steps
                        .iter()
                        .next()
                        .copied()
                        .map(|(lane, step)| Selection::Step(lane, step));
                }
            }
        }
    }

    /// Host-facing retargeting surface. Updating a binding is deliberately a
    /// draft operation; Enter in the expression field applies it atomically.
    pub fn retarget_expression_binding(
        &mut self,
        name: impl Into<String>,
        target: TriggerTarget,
        cx: &mut Context<Self>,
    ) {
        self.expression_bindings.insert(name.into(), target);
        self.update_expression_diagnostics();
        cx.notify();
    }

    pub fn expression_bindings(&self) -> &BTreeMap<String, TriggerTarget> {
        &self.expression_bindings
    }

    fn expression_target_choices(&self) -> Vec<TriggerTarget> {
        let mut targets = self
            .source
            .trigger_targets
            .iter()
            .map(|option| option.target.clone())
            .chain(self.expression_bindings.values().cloned())
            .chain(
                self.stored_active_pattern()
                    .into_iter()
                    .filter_map(|pattern| match pattern.content {
                        PatternContent::Steps(steps) => Some(steps),
                        PatternContent::Notes(_) => None,
                    })
                    .flat_map(|steps| {
                        steps
                            .lanes
                            .into_values()
                            .map(|lane| lane.target)
                            .collect::<Vec<_>>()
                    }),
            )
            .collect::<Vec<_>>();
        targets.sort();
        targets.dedup();
        targets
    }

    fn refresh_draft_bindings(&mut self) {
        let Ok(term) = crate::pattern_lang::parse(&self.expression) else {
            return;
        };
        let choices = self.expression_target_choices();
        let Some(default) = choices.first().cloned() else {
            return;
        };
        for name in crate::pattern_lang::referenced_bindings(&term) {
            self.expression_bindings
                .entry(name)
                .or_insert_with(|| default.clone());
        }
    }

    fn update_expression_diagnostics(&mut self) {
        if self.expression.trim().is_empty() {
            self.status = Some("Enter a pattern term, then Apply".into());
            return;
        }
        let term = match crate::pattern_lang::parse(&self.expression) {
            Ok(term) => term,
            Err(error) => {
                self.status = Some(format!("Draft parse: {error}"));
                return;
            }
        };
        let missing = crate::pattern_lang::referenced_bindings(&term)
            .into_iter()
            .filter(|name| !self.expression_bindings.contains_key(name))
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            self.status = Some(format!(
                "Unbound {} · choose a stable pad/target",
                missing.join(", ")
            ));
            return;
        }
        let Some(pattern) = self
            .stored_active_pattern()
            .filter(|pattern| matches!(pattern.content, PatternContent::Steps(_)))
        else {
            self.status = Some("Pattern terms require a step pattern target".into());
            return;
        };
        match crate::pattern_lang::eval_steps(
            &term,
            &crate::pattern_lang::EvalContext {
                bindings: &self.expression_bindings,
                cycle: pattern.length,
                seed: self.preview_seed,
                cycle_index: self.preview_cycle,
            },
        ) {
            Ok(output) if output.diagnostics.is_empty() => {
                self.status = Some(format!(
                    "Draft valid for placement cycle {} · Apply to commit",
                    self.preview_cycle + 1
                ));
            }
            Ok(output) => {
                self.status = Some(
                    output
                        .diagnostics
                        .into_iter()
                        .map(pattern_authoring::format_diagnostic)
                        .collect::<Vec<_>>()
                        .join(" · "),
                );
            }
            Err(error) => self.status = Some(format!("Draft evaluation: {error}")),
        }
    }

    fn cycle_expression_binding(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(pattern) = self.stored_active_pattern() else {
            return;
        };
        let PatternContent::Steps(steps) = pattern.content else {
            return;
        };
        let mut targets = self.expression_target_choices();
        targets.extend(steps.lanes.values().map(|lane| lane.target.clone()));
        targets.sort();
        targets.dedup();
        if targets.is_empty() {
            return;
        }
        let current = self.expression_bindings.get(name);
        let next = current
            .and_then(|current| targets.iter().position(|target| target == current))
            .map_or(0, |index| (index + 1) % targets.len());
        self.retarget_expression_binding(name.to_owned(), targets[next].clone(), cx);
    }

    pub fn set_mode(&mut self, mode: EditorMode, cx: &mut Context<Self>) {
        self.mode = mode;
        self.selection = None;
        self.selected_notes.clear();
        self.selected_steps.clear();
        self.drag = None;
        self.piano_gesture = None;
        self.optimistic_pattern = None;
        self.preview_cycle = 0;
        self.reload_authoring_state();
        cx.notify();
    }

    fn request_mode(&mut self, mode: EditorMode, cx: &mut Context<Self>) {
        let Some(pattern) = self.pattern_id_for(mode) else {
            self.set_mode(mode, cx);
            self.status = Some(format!(
                "No {} exists yet · press + NEW to create one",
                match mode {
                    EditorMode::PianoRoll => "note pattern",
                    EditorMode::Steps => "step pattern",
                }
            ));
            cx.notify();
            return;
        };
        self.request_retarget(PatternEditorTarget::new(pattern, mode.into()), cx);
    }

    fn pattern_id_for(&self, mode: EditorMode) -> Option<PatternId> {
        match mode {
            EditorMode::PianoRoll => self.source.note_pattern,
            EditorMode::Steps => self.source.step_pattern,
        }
    }

    fn stored_active_pattern(&self) -> Option<PatternDefinition> {
        let id = self.pattern_id_for(self.mode)?;
        if self
            .optimistic_pattern
            .as_ref()
            .is_some_and(|pattern| pattern.id == id)
        {
            return self.optimistic_pattern.clone();
        }
        self.source
            .sequencer
            .lock()
            .ok()?
            .patterns()
            .get(id)
            .cloned()
    }

    fn active_pattern(&self) -> Option<PatternDefinition> {
        let mut stored = self.stored_active_pattern()?;
        if let Some(gesture) = &self.piano_gesture {
            stored.content = PatternContent::Notes(gesture.preview().clone());
            return Some(stored);
        }
        if let Some(publication) = self.cycle_publication.as_ref().filter(|publication| {
            publication.target.pattern == stored.id
                && publication.cycle_index == self.preview_cycle
                && publication.performance_seed == self.preview_seed
        }) {
            return Some(publication.definition.clone());
        }
        if self.workflow_callback.is_some() {
            return Some(stored);
        }
        pattern_authoring::preview_expression_placement(
            &stored,
            self.preview_cycle,
            self.preview_seed,
        )
        .map(|preview| preview.definition)
        .ok()
        .or(Some(stored))
    }

    fn next_available_note_id(&self) -> NoteId {
        let next = self
            .source
            .sequencer
            .lock()
            .ok()
            .map(|sequencer| sequencer.allocator_state().next_note_id)
            .unwrap_or(1);
        NoteId::from_raw(next)
    }

    fn available_pattern_targets(&self) -> Vec<PatternEditorTarget> {
        let mut targets = self
            .source
            .sequencer
            .lock()
            .ok()
            .map(|sequencer| {
                sequencer
                    .patterns()
                    .patterns()
                    .map(|pattern| PatternEditorTarget {
                        pattern: pattern.id,
                        mode: match pattern.content {
                            PatternContent::Notes(_) => ActionEditorMode::PianoRoll,
                            PatternContent::Steps(_) => ActionEditorMode::Steps,
                        },
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        targets.sort_by_key(|target| (target.pattern, target.mode == ActionEditorMode::Steps));
        targets
    }

    fn request_retarget(&mut self, target: PatternEditorTarget, cx: &mut Context<Self>) {
        if self.workflow_callback.is_some() {
            self.emit(
                PatternAction::Retarget(target),
                format!("Targeting pattern #{}", target.pattern.get()),
                cx,
            );
            return;
        }
        self.retarget_pattern(target, cx);
        self.emit(
            PatternAction::Retarget(target),
            format!("Targeted pattern #{}", target.pattern.get()),
            cx,
        );
    }

    fn cycle_pattern_target(&mut self, cx: &mut Context<Self>) {
        let targets = self.available_pattern_targets();
        if targets.is_empty() {
            self.status = Some("No patterns exist; create one first".into());
            cx.notify();
            return;
        }
        let current = self.target();
        let next = current
            .and_then(|current| targets.iter().position(|target| *target == current))
            .map_or(0, |index| (index + 1) % targets.len());
        self.request_retarget(targets[next], cx);
    }

    fn reveal_pattern_uses(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.target() else {
            return;
        };
        if !self.emit(
            PatternAction::Retarget(target),
            "Resolving pattern uses and reveal target",
            cx,
        ) {
            self.status = Some("Pattern use navigation requires a project workflow".into());
            cx.notify();
        }
    }

    fn make_occurrence_unique(&mut self, cx: &mut Context<Self>) {
        let Some(context) = self.source.workflow.as_ref() else {
            self.status = Some("Select a placed pattern occurrence to make it unique".into());
            cx.notify();
            return;
        };
        let Some(occurrence) = context.occurrence else {
            self.status = Some("This editor targets the definition, not one occurrence".into());
            cx.notify();
            return;
        };
        if context.uses.occurrences.len() <= 1 {
            self.status = Some("This occurrence is already unique".into());
            cx.notify();
            return;
        }
        let name = self
            .stored_active_pattern()
            .map(|pattern| format!("{} unique", pattern.name));
        self.dispatch_workflow(
            PatternWorkflowIntent::MakeOccurrenceUnique(MakeOccurrenceUniqueIntent {
                expected_project_revision: self.expected_project_revision,
                occurrence,
                name,
            }),
            "Making this pattern occurrence unique",
            cx,
        );
    }

    fn audition_cycle(&mut self, cx: &mut Context<Self>) {
        if !self.require_audition_available(cx) {
            return;
        }
        let Some(occurrence) = self
            .source
            .workflow
            .as_ref()
            .and_then(|context| context.occurrence)
        else {
            self.status = Some("Select a placed occurrence to audition its real cycle".into());
            cx.notify();
            return;
        };
        let Some(callback) = self.shared_audition_callback.clone() else {
            self.status = Some("Shared pattern audition callback is not connected".into());
            cx.notify();
            return;
        };
        callback(PatternAuditionRequest {
            expected_project_revision: self.expected_project_revision,
            occurrence,
            cycle_index: self.preview_cycle,
            performance_seed: self.preview_seed,
            scope: PatternAuditionScope::Pattern,
        });
        self.status = Some(format!(
            "Preparing placement cycle {} audition",
            self.preview_cycle + 1
        ));
        cx.notify();
    }

    fn cycle_preview(&mut self, direction: i64, cx: &mut Context<Self>) {
        self.preview_cycle = if direction < 0 {
            self.preview_cycle.saturating_sub(direction.unsigned_abs())
        } else {
            self.preview_cycle.saturating_add(direction as u64)
        };
        self.selection = None;
        self.selected_notes.clear();
        self.selected_steps.clear();
        if let Some(target) = self.target() {
            let emitted = self.emit(
                PatternAction::PreviewCycle {
                    target,
                    cycle_index: self.preview_cycle,
                    performance_seed: self.preview_seed,
                },
                format!("Previewing placement cycle {}", self.preview_cycle + 1),
                cx,
            );
            if !emitted {
                self.status = self
                    .stored_active_pattern()
                    .and_then(|pattern| {
                        pattern_authoring::preview_expression_placement(
                            &pattern,
                            self.preview_cycle,
                            self.preview_seed,
                        )
                        .ok()
                    })
                    .and_then(|preview| preview.diagnostics.first().copied())
                    .map(pattern_authoring::format_diagnostic)
                    .or_else(|| {
                        Some(format!(
                            "Previewing actual placement cycle {}",
                            self.preview_cycle + 1
                        ))
                    });
            }
        }
        cx.notify();
    }

    fn create_pattern(&mut self, cx: &mut Context<Self>) {
        let mode: ActionEditorMode = self.mode.into();
        let length = self
            .stored_active_pattern()
            .map(|pattern| pattern.length)
            .unwrap_or(BeatDuration((PPQ * 4) as u64));
        let initial_target = self.expression_target_choices().first().cloned();
        let create = CreatePatternIntent {
            mode,
            name: match mode {
                ActionEditorMode::PianoRoll => "New note pattern".into(),
                ActionEditorMode::Steps => "New step pattern".into(),
            },
            length,
            step_resolution: BeatDuration((PPQ / 4) as u64),
            initial_target,
        };
        if self.emit(
            PatternAction::Create(create.clone()),
            "Pattern creation sent to project controller",
            cx,
        ) {
            return;
        }
        let Some(mut sequencer) = self.source.sequencer.lock().ok() else {
            return;
        };
        let id = sequencer.allocate_pattern_id();
        let content = match create.mode {
            ActionEditorMode::PianoRoll => PatternContent::Notes(NotePattern::default()),
            ActionEditorMode::Steps => {
                let mut lanes = BTreeMap::new();
                if let Some(target) = create.initial_target {
                    let lane = sequencer.allocate_step_lane_id();
                    lanes.insert(
                        lane,
                        StepLane {
                            id: lane,
                            name: "Lane 1".into(),
                            target,
                            choke_group: None,
                            steps: BTreeMap::new(),
                        },
                    );
                }
                PatternContent::Steps(StepPattern {
                    resolution: create.step_resolution,
                    swing: 0.0,
                    lanes,
                })
            }
        };
        let definition = PatternDefinition {
            id,
            name: create.name,
            length: create.length,
            content,
            origin: PatternOrigin::Authored,
            revision: 0,
        };
        let result = sequencer.execute(
            "Create pattern",
            vec![SequencerCommand::PutPattern {
                before: None,
                after: Some(definition),
            }],
        );
        drop(sequencer);
        match result {
            Ok(_) => self.request_retarget(PatternEditorTarget { pattern: id, mode }, cx),
            Err(error) => {
                self.status = Some(error.to_string());
                cx.notify();
            }
        }
    }

    fn duplicate_pattern(&mut self, cx: &mut Context<Self>) {
        let Some(before) = self.stored_active_pattern() else {
            return;
        };
        let name = format!("{} copy", before.name);
        if self.emit(
            PatternAction::Duplicate {
                source: before.id,
                expected_pattern_revision: before.revision,
                name: name.clone(),
            },
            "Pattern duplication sent to project controller",
            cx,
        ) {
            return;
        }
        let Some(mut sequencer) = self.source.sequencer.lock().ok() else {
            return;
        };
        let id = sequencer.allocate_pattern_id();
        let mut after = before;
        after.id = id;
        after.name = name;
        after.revision = 0;
        let mode: ActionEditorMode = match &after.content {
            PatternContent::Notes(_) => ActionEditorMode::PianoRoll,
            PatternContent::Steps(_) => ActionEditorMode::Steps,
        };
        let result = sequencer.execute(
            "Duplicate pattern",
            vec![SequencerCommand::PutPattern {
                before: None,
                after: Some(after),
            }],
        );
        drop(sequencer);
        match result {
            Ok(_) => self.request_retarget(PatternEditorTarget { pattern: id, mode }, cx),
            Err(error) => {
                self.status = Some(error.to_string());
                cx.notify();
            }
        }
    }

    fn delete_pattern(&mut self, cx: &mut Context<Self>) {
        let Some(before) = self.stored_active_pattern() else {
            return;
        };
        if self.emit(
            PatternAction::Delete {
                pattern: before.id,
                expected_pattern_revision: before.revision,
            },
            "Pattern deletion sent to project controller",
            cx,
        ) {
            return;
        }
        let result = self
            .source
            .sequencer
            .lock()
            .map_err(|_| "sequencer lock poisoned".to_owned())
            .and_then(|mut sequencer| {
                sequencer
                    .execute(
                        "Delete pattern",
                        vec![SequencerCommand::PutPattern {
                            before: Some(before),
                            after: None,
                        }],
                    )
                    .map_err(|error| error.to_string())
            });
        match result {
            Ok(_) => {
                match self.mode {
                    EditorMode::PianoRoll => self.source.note_pattern = None,
                    EditorMode::Steps => self.source.step_pattern = None,
                }
                if let Some(target) = self.available_pattern_targets().first().copied() {
                    self.request_retarget(target, cx);
                } else {
                    self.status = Some("Pattern deleted".into());
                    cx.notify();
                }
            }
            Err(error) => {
                self.status = Some(format!("Delete refused: {error}"));
                cx.notify();
            }
        }
    }

    fn dispatch_workflow(
        &mut self,
        intent: PatternWorkflowIntent,
        status: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(callback) = self.workflow_callback.clone() else {
            return false;
        };
        let request = PatternWorkflowRequestId::from_raw(self.next_workflow_request);
        self.next_workflow_request = self.next_workflow_request.saturating_add(1);
        let receipt = callback(PatternWorkflowRequest {
            id: request,
            intent,
        });
        match receipt {
            PatternWorkflowDispatchReceipt::Accepted(accepted) if accepted == request => {
                self.pending_workflow.insert(request);
                self.status = Some(status.into());
            }
            PatternWorkflowDispatchReceipt::Completed {
                request: completed,
                result,
            } if completed == request => self.apply_workflow_result(result, cx),
            PatternWorkflowDispatchReceipt::Accepted(accepted)
            | PatternWorkflowDispatchReceipt::Completed {
                request: accepted, ..
            } => {
                self.status = Some(format!(
                    "Pattern workflow returned request #{} for #{}",
                    accepted.get(),
                    request.get()
                ));
            }
        }
        cx.notify();
        true
    }

    fn apply_workflow_result(
        &mut self,
        result: Result<PatternWorkflowOutcome, PatternWorkflowError>,
        cx: &mut Context<Self>,
    ) {
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                self.apply_workflow_failure(error.to_string(), cx);
                return;
            }
        };
        match outcome {
            PatternWorkflowOutcome::Published {
                update,
                publication,
            } => {
                self.expected_project_revision = publication.revision;
                self.source.sequencer = Arc::new(Mutex::new(
                    update.snapshot.project.state().domains.sequencer.clone(),
                ));
                self.optimistic_pattern = None;
                self.cycle_publication = None;
                self.reveal = publication.reveal.clone();
                let current_occurrence = self
                    .source
                    .workflow
                    .as_ref()
                    .and_then(|context| context.occurrence);
                self.source.workflow = publication
                    .uses
                    .clone()
                    .zip(publication.reveal.clone())
                    .map(|(uses, reveal)| PatternEditorWorkflowContext {
                        occurrence: uses
                            .occurrences
                            .iter()
                            .find(|occurrence| {
                                current_occurrence.is_some_and(|current| {
                                    current.arrangement_clip == occurrence.target.arrangement_clip
                                })
                            })
                            .or_else(|| uses.occurrences.first())
                            .map(|occurrence| occurrence.target),
                        uses,
                        reveal,
                    });
                match publication.definition.as_ref() {
                    Some(definition) => {
                        let mode = match definition.content {
                            PatternContent::Notes(_) => EditorMode::PianoRoll,
                            PatternContent::Steps(_) => EditorMode::Steps,
                        };
                        self.mode = mode;
                        match mode {
                            EditorMode::PianoRoll => self.source.note_pattern = Some(definition.id),
                            EditorMode::Steps => self.source.step_pattern = Some(definition.id),
                        }
                        self.reload_authoring_state();
                        self.prune_event_selection();
                        self.status = Some(format!(
                            "Pattern #{} published at project revision {}",
                            definition.id.get(),
                            publication.revision
                        ));
                    }
                    None => {
                        if self.pattern_id_for(self.mode) == Some(publication.pattern) {
                            match self.mode {
                                EditorMode::PianoRoll => self.source.note_pattern = None,
                                EditorMode::Steps => self.source.step_pattern = None,
                            }
                        }
                        self.status = Some("Pattern deleted".into());
                    }
                }
                self.last_publication = Some(publication);
            }
            PatternWorkflowOutcome::History(update) => {
                if let Some(update) = update {
                    self.expected_project_revision = update.revisions().aggregate;
                    self.source.sequencer = Arc::new(Mutex::new(
                        update.snapshot.project.state().domains.sequencer.clone(),
                    ));
                    self.optimistic_pattern = None;
                    self.cycle_publication = None;
                    self.reload_authoring_state();
                    self.prune_event_selection();
                }
                self.status = Some("Pattern history updated".into());
            }
            PatternWorkflowOutcome::Navigate(reveal) => {
                self.reveal = Some(reveal);
                self.status = Some("Pattern uses revealed".into());
            }
            PatternWorkflowOutcome::Targeted(hydration) => {
                self.expected_project_revision = hydration.revision;
                self.mode = hydration.target.mode.into();
                match self.mode {
                    EditorMode::PianoRoll => {
                        self.source.note_pattern = Some(hydration.target.pattern)
                    }
                    EditorMode::Steps => self.source.step_pattern = Some(hydration.target.pattern),
                }
                self.source.workflow = Some(PatternEditorWorkflowContext {
                    occurrence: hydration.occurrence,
                    uses: hydration.uses,
                    reveal: hydration.reveal.clone(),
                });
                self.reveal = Some(hydration.reveal);
                self.optimistic_pattern = None;
                self.cycle_publication = None;
                self.preview_cycle = 0;
                self.selection = None;
                self.selected_notes.clear();
                self.selected_steps.clear();
                self.drag = None;
                self.piano_gesture = None;
                self.reload_authoring_state();
                self.status = Some(format!(
                    "Targeted pattern #{}",
                    hydration.target.pattern.get()
                ));
            }
            PatternWorkflowOutcome::Preview(publication) => {
                self.expected_project_revision = publication.revision;
                self.status = Some(if publication.diagnostics.is_empty() {
                    format!("Previewing placement cycle {}", publication.cycle_index + 1)
                } else {
                    publication
                        .diagnostics
                        .iter()
                        .copied()
                        .map(pattern_authoring::format_diagnostic)
                        .collect::<Vec<_>>()
                        .join(" · ")
                });
                self.reveal = Some(publication.reveal.clone());
                self.cycle_publication = Some(publication);
            }
            PatternWorkflowOutcome::Audition(plan) => {
                self.status = Some(format!(
                    "Audition cycle {} · {} scheduled events",
                    plan.cycle_index + 1,
                    plan.events.len()
                ));
                self.reveal = Some(plan.reveal.clone());
                self.audition_plan = Some(plan);
            }
            PatternWorkflowOutcome::GestureBegan(receipt) => {
                if self.drag.is_none() {
                    self.dispatch_workflow(
                        PatternWorkflowIntent::EndGesture(receipt),
                        "Closing completed pointer gesture",
                        cx,
                    );
                    return;
                }
                self.active_gesture = Some(receipt);
                self.status = Some("Pattern gesture active".into());
            }
            PatternWorkflowOutcome::GestureEnded => {
                self.active_gesture = None;
                self.status = Some("Pattern gesture committed".into());
            }
        }
        cx.notify();
    }

    fn apply_workflow_failure(&mut self, display_message: String, cx: &mut Context<Self>) {
        self.status = Some(display_message);
        self.optimistic_pattern = None;
        self.active_gesture = None;
        self.drag = None;
        self.piano_gesture = None;
        cx.notify();
    }

    fn emit(
        &mut self,
        action: PatternAction,
        status: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.workflow_callback.is_some() {
            let action = PatternActionIntent {
                expected_project_revision: self.expected_project_revision,
                action,
            };
            let intent = match self
                .active_gesture
                .clone()
                .filter(|_| matches!(&action.action, PatternAction::Edit(_)))
            {
                Some(receipt) => PatternWorkflowIntent::GestureEdit { receipt, action },
                None => PatternWorkflowIntent::Action(action),
            };
            return self.dispatch_workflow(intent, status, cx);
        }
        let Some(callback) = self.callback.as_ref() else {
            return false;
        };
        callback(PatternActionIntent {
            expected_project_revision: self.expected_project_revision,
            action,
        });
        self.status = Some(status.into());
        cx.notify();
        true
    }

    fn project_backed(&self) -> bool {
        self.callback.is_some() || self.workflow_callback.is_some()
    }

    fn execute_pattern(
        &mut self,
        label: &'static str,
        before: PatternDefinition,
        mut after: PatternDefinition,
        cx: &mut Context<Self>,
    ) {
        if before == after {
            return;
        }
        after.revision = before.revision.saturating_add(1);
        if before.content != after.content && before.origin == after.origin {
            after.origin.mark_diverged();
        }
        if self.project_backed() {
            let intent = PatternEditIntent::replace_content(&before, after.content.clone());
            self.optimistic_pattern = Some(after);
            self.emit(
                PatternAction::Edit(intent),
                format!("{label} sent to project controller"),
                cx,
            );
            return;
        }
        let stored_before = self
            .source
            .sequencer
            .lock()
            .ok()
            .and_then(|sequencer| sequencer.patterns().get(before.id).cloned());
        let Some(stored_before) = stored_before else {
            self.status = Some("Pattern is no longer available".into());
            cx.notify();
            return;
        };
        after.revision = stored_before.revision.saturating_add(1);
        let result = self
            .source
            .sequencer
            .lock()
            .map_err(|_| "sequencer lock poisoned".to_owned())
            .and_then(|mut sequencer| {
                sequencer
                    .execute(
                        label,
                        vec![SequencerCommand::PutPattern {
                            before: Some(stored_before),
                            after: Some(after),
                        }],
                    )
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            });
        self.status = result.err();
        cx.notify();
    }

    fn execute_semantic_edit(
        &mut self,
        label: &'static str,
        before: PatternDefinition,
        mut after: PatternDefinition,
        edit: PatternEdit,
        cx: &mut Context<Self>,
    ) {
        if before == after {
            return;
        }
        if self.project_backed() {
            after.revision = before.revision.saturating_add(1);
            if before.content != after.content && before.origin == after.origin {
                after.origin.mark_diverged();
            }
            self.optimistic_pattern = Some(after);
            self.cycle_publication = None;
            self.emit(
                PatternAction::Edit(PatternEditIntent {
                    pattern: before.id,
                    expected_pattern_revision: before.revision,
                    edit,
                }),
                format!("{label} sent to project controller"),
                cx,
            );
        } else {
            self.execute_pattern(label, before, after, cx);
        }
    }

    fn apply_expression(&mut self, overwrite: DivergedOverwrite, cx: &mut Context<Self>) {
        let Some(before) = self
            .stored_active_pattern()
            .filter(|pattern| matches!(pattern.content, PatternContent::Steps(_)))
        else {
            self.status = Some("No step pattern is connected".into());
            cx.notify();
            return;
        };
        match pattern_authoring::apply_expression(
            &before,
            &self.expression,
            self.expression_bindings.clone(),
            overwrite,
        ) {
            Ok(application) => {
                if let PatternContent::Steps(steps) = &application.definition.content {
                    self.swing = steps.swing;
                }
                let diagnostics = application
                    .diagnostics
                    .iter()
                    .copied()
                    .map(pattern_authoring::format_diagnostic)
                    .collect::<Vec<_>>();
                if self.project_backed() {
                    self.optimistic_pattern = Some(application.definition);
                    self.emit(
                        PatternAction::Edit(PatternEditIntent {
                            pattern: before.id,
                            expected_pattern_revision: before.revision,
                            edit: PatternEdit::ApplyExpression {
                                source: self.expression.clone(),
                                bindings: self.expression_bindings.clone(),
                                overwrite,
                                realization: pattern_authoring::ExpressionRealizationContext {
                                    cycle_index: self.preview_cycle,
                                    performance_seed: self.preview_seed,
                                },
                            },
                        }),
                        "Expression regeneration sent to project controller",
                        cx,
                    );
                } else {
                    self.execute_pattern(
                        "Apply pattern expression",
                        before,
                        application.definition,
                        cx,
                    );
                }
                self.status = if diagnostics.is_empty() {
                    Some(if self.project_backed() {
                        "Expression queued; previewing the requested realization".into()
                    } else {
                        "Expression applied; loop placements vary by cycle".into()
                    })
                } else {
                    Some(diagnostics.join(" · "))
                };
            }
            Err(error) => {
                self.status = Some(match error {
                    pattern_authoring::PatternAuthoringError::Evaluate(
                        crate::pattern_lang::PatternEvalError::UnboundName(name),
                    ) => format!(
                        "Unbound name {name:?}; retarget it to a lane target before applying"
                    ),
                    other => other.to_string(),
                });
                cx.notify();
            }
        }
    }

    fn expression_key_down(
        &mut self,
        event: &KeyDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.expression_focused {
            if event.keystroke.key.as_str() == "escape" {
                self.cancel_piano_gesture("Piano gesture cancelled", cx);
                cx.stop_propagation();
            }
            return;
        }
        let key = event.keystroke.key.as_str();
        match key {
            "escape" => {
                self.expression_focused = false;
                self.status = None;
            }
            "enter" => {
                let overwrite = if event.keystroke.modifiers.platform {
                    DivergedOverwrite::Confirmed
                } else {
                    DivergedOverwrite::Refuse
                };
                self.apply_expression(overwrite, cx);
            }
            "backspace" | "delete" => {
                self.expression.pop();
                self.refresh_draft_bindings();
                self.update_expression_diagnostics();
            }
            "left" | "right" | "up" | "down" | "tab" => {}
            _ if !event.keystroke.modifiers.platform && !event.keystroke.modifiers.control => {
                if let Some(text) = event.keystroke.key_char.as_deref() {
                    if text.chars().all(|character| !character.is_control()) {
                        self.expression.push_str(text);
                        self.refresh_draft_bindings();
                        self.update_expression_diagnostics();
                    }
                }
            }
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn toggle_mode(&mut self, cx: &mut Context<Self>) {
        let next = match self.mode {
            EditorMode::PianoRoll => EditorMode::Steps,
            EditorMode::Steps => EditorMode::PianoRoll,
        };
        self.request_mode(next, cx);
    }

    fn cycle_grid(&mut self, cx: &mut Context<Self>) {
        self.quantize_grid = match self.quantize_grid {
            value if value == PPQ as u64 => (PPQ / 2) as u64,
            value if value == (PPQ / 2) as u64 => (PPQ / 4) as u64,
            value if value == (PPQ / 4) as u64 => (PPQ / 8) as u64,
            _ => PPQ as u64,
        };
        cx.notify();
    }

    fn cycle_swing(&mut self, cx: &mut Context<Self>) {
        self.swing = if self.swing < 0.01 {
            0.25
        } else if self.swing < 0.4 {
            0.5
        } else if self.swing < 0.6 {
            0.66
        } else {
            0.0
        };
        if let Some(before) = self.active_pattern() {
            if let PatternContent::Steps(mut steps) = before.content.clone() {
                steps.swing = self.swing;
                if self.project_backed() {
                    let mut optimistic = before.clone();
                    optimistic.content = PatternContent::Steps(steps);
                    optimistic.origin.mark_diverged();
                    optimistic.revision = before.revision.saturating_add(1);
                    self.optimistic_pattern = Some(optimistic);
                    self.emit(
                        PatternAction::Edit(PatternEditIntent {
                            pattern: before.id,
                            expected_pattern_revision: before.revision,
                            edit: PatternEdit::SetSwing(self.swing),
                        }),
                        "Pattern swing sent to project controller",
                        cx,
                    );
                    return;
                }
                let mut after = before.clone();
                after.content = PatternContent::Steps(steps);
                self.execute_pattern("Set pattern swing", before, after, cx);
                return;
            }
        }
        cx.notify();
    }

    fn add_lane(&mut self, cx: &mut Context<Self>) {
        let Some(before) = self
            .stored_active_pattern()
            .filter(|pattern| matches!(pattern.content, PatternContent::Steps(_)))
        else {
            return;
        };
        let target = self
            .expression_target_choices()
            .into_iter()
            .next()
            .unwrap_or(TriggerTarget::AnalysisTemplate(0));
        let lane_number = match &before.content {
            PatternContent::Steps(steps) => steps.lanes.len() + 1,
            PatternContent::Notes(_) => unreachable!(),
        };
        let name = format!("Lane {lane_number}");
        let edit = PatternEdit::AddLane {
            name: name.clone(),
            target: target.clone(),
            choke_group: None,
        };
        if self.project_backed() {
            self.emit(
                PatternAction::Edit(PatternEditIntent {
                    pattern: before.id,
                    expected_pattern_revision: before.revision,
                    edit,
                }),
                "Adding pattern lane",
                cx,
            );
            return;
        }
        let lane = match self.source.sequencer.lock() {
            Ok(mut sequencer) => sequencer.allocate_step_lane_id(),
            Err(_) => return,
        };
        let mut after = before.clone();
        let PatternContent::Steps(steps) = &mut after.content else {
            return;
        };
        steps.lanes.insert(
            lane,
            StepLane {
                id: lane,
                name,
                target,
                choke_group: None,
                steps: BTreeMap::new(),
            },
        );
        self.execute_pattern("Add pattern lane", before, after, cx);
    }

    fn remove_lane(&mut self, cx: &mut Context<Self>) {
        let Some(before) = self
            .stored_active_pattern()
            .filter(|pattern| matches!(pattern.content, PatternContent::Steps(_)))
        else {
            return;
        };
        let PatternContent::Steps(steps) = &before.content else {
            unreachable!()
        };
        let lane = match self.selection {
            Some(Selection::Step(lane, _)) => Some(lane),
            _ => steps.lanes.keys().next_back().copied(),
        };
        let Some(lane) = lane else {
            self.status = Some("No lane to remove".into());
            cx.notify();
            return;
        };
        self.selection = None;
        self.selected_notes.clear();
        self.selected_steps.clear();
        let edit = PatternEdit::RemoveLane { lane };
        if self.project_backed() {
            self.emit(
                PatternAction::Edit(PatternEditIntent {
                    pattern: before.id,
                    expected_pattern_revision: before.revision,
                    edit,
                }),
                "Removing pattern lane",
                cx,
            );
            return;
        }
        let mut after = before.clone();
        let PatternContent::Steps(steps) = &mut after.content else {
            return;
        };
        steps.lanes.remove(&lane);
        self.execute_pattern("Remove pattern lane", before, after, cx);
    }

    fn cycle_lane_target(&mut self, lane: StepLaneId, cx: &mut Context<Self>) {
        let Some(before) = self.stored_active_pattern() else {
            return;
        };
        let PatternContent::Steps(steps) = &before.content else {
            return;
        };
        let Some(current) = steps.lanes.get(&lane).map(|lane| lane.target.clone()) else {
            return;
        };
        let mut choices = self
            .source
            .trigger_targets
            .iter()
            .map(|option| LaneTargetChoice::Trigger(option.target.clone()))
            .chain(
                self.source
                    .pad_targets
                    .iter()
                    .map(|option| LaneTargetChoice::Pad(option.target, option.sequencer_alias)),
            )
            .collect::<Vec<_>>();
        if choices.is_empty() {
            self.status = Some("No stable lane or pad destinations are available".into());
            cx.notify();
            return;
        }
        let current_index = choices.iter().position(|choice| match choice {
            LaneTargetChoice::Trigger(target) => *target == current,
            LaneTargetChoice::Pad(_, Some(alias)) => current == TriggerTarget::Sample(*alias),
            LaneTargetChoice::Pad(_, None) => false,
        });
        let choice = choices.remove(current_index.map_or(0, |index| (index + 1) % choices.len()));
        let edit = match choice {
            LaneTargetChoice::Trigger(target) => PatternEdit::SetLaneTarget { lane, target },
            LaneTargetChoice::Pad(target, _) => PatternEdit::MapLaneToPad { lane, target },
        };
        self.emit(
            PatternAction::Edit(PatternEditIntent {
                pattern: before.id,
                expected_pattern_revision: before.revision,
                edit,
            }),
            "Retargeting pattern lane",
            cx,
        );
    }

    fn cycle_scale(&mut self, cx: &mut Context<Self>) {
        self.pitch_scale.kind = self.pitch_scale.kind.next();
        self.status = Some(format!(
            "Pitch draw and keyboard moves constrained to C {}",
            self.pitch_scale.kind.label().to_lowercase()
        ));
        cx.notify();
    }

    fn available_note_instruments(&self) -> Vec<u64> {
        let mut instruments = self
            .source
            .trigger_targets
            .iter()
            .filter_map(|option| match &option.target {
                TriggerTarget::InstrumentNote { instrument, .. } => Some(*instrument),
                _ => None,
            })
            .chain(self.active_pattern().into_iter().flat_map(|pattern| {
                match pattern.content {
                    PatternContent::Notes(notes) => notes
                        .notes
                        .into_values()
                        .filter_map(|note| note.instrument)
                        .collect::<Vec<_>>(),
                    PatternContent::Steps(_) => Vec::new(),
                }
            }))
            .collect::<Vec<_>>();
        instruments.sort_unstable();
        instruments.dedup();
        instruments
    }

    fn cycle_note_instrument(&mut self, cx: &mut Context<Self>) {
        let instruments = self.available_note_instruments();
        if instruments.is_empty() {
            self.status = Some("No stable instrument destinations are available".into());
            cx.notify();
            return;
        }
        let next = self
            .note_instrument
            .and_then(|current| instruments.iter().position(|value| *value == current))
            .map_or(0, |index| (index + 1) % instruments.len());
        let instrument = instruments[next];
        self.note_instrument = Some(instrument);
        let Some(before) = self.active_pattern() else {
            return;
        };
        let PatternContent::Notes(mut notes) = before.content.clone() else {
            return;
        };
        if self.selected_notes.is_empty() {
            self.status = Some(format!("New notes route to instrument #{instrument}"));
            cx.notify();
            return;
        }
        for id in &self.selected_notes {
            if let Some(note) = notes.notes.get_mut(id) {
                note.instrument = Some(instrument);
            }
        }
        let mut after = before.clone();
        after.content = PatternContent::Notes(notes);
        self.execute_pattern("Route piano notes", before, after, cx);
    }

    fn audition_selected(&mut self, cx: &mut Context<Self>) {
        if !self.require_audition_available(cx) {
            return;
        }
        let Some(callback) = self.shared_audition_callback.clone() else {
            self.status = Some("Shared pattern audition callback is not connected".into());
            cx.notify();
            return;
        };
        let scope = match self.mode {
            EditorMode::PianoRoll => {
                let selected = if self.selected_notes.is_empty() {
                    self.selection
                        .and_then(|selection| match selection {
                            Selection::Note(id) => Some(BTreeSet::from([id])),
                            Selection::Step(_, _) => None,
                        })
                        .unwrap_or_default()
                } else {
                    self.selected_notes.clone()
                };
                if selected.is_empty() {
                    self.status = Some("Select one or more notes to audition".into());
                    cx.notify();
                    return;
                }
                PatternAuditionSelection::Notes(selected)
            }
            EditorMode::Steps => {
                let selected = if self.selected_steps.is_empty() {
                    self.selection
                        .and_then(|selection| match selection {
                            Selection::Step(lane, step) => Some(BTreeSet::from([(lane, step)])),
                            Selection::Note(_) => None,
                        })
                        .unwrap_or_default()
                } else {
                    self.selected_steps.clone()
                };
                if selected.is_empty() {
                    self.status = Some("Select one or more steps to audition".into());
                    cx.notify();
                    return;
                }
                PatternAuditionSelection::Steps(selected)
            }
        };
        let Some(occurrence) = self
            .source
            .workflow
            .as_ref()
            .and_then(|context| context.occurrence)
        else {
            self.status = Some("Select a placed occurrence to audition notes".into());
            cx.notify();
            return;
        };
        let count = match &scope {
            PatternAuditionSelection::Notes(selected) => selected.len(),
            PatternAuditionSelection::Steps(selected) => selected.len(),
        };
        callback(PatternAuditionRequest {
            expected_project_revision: self.expected_project_revision,
            occurrence,
            cycle_index: self.preview_cycle,
            performance_seed: self.preview_seed,
            scope: PatternAuditionScope::Selection(scope),
        });
        self.status = Some(format!(
            "Preparing {count} selected {}{}",
            if self.mode == EditorMode::PianoRoll {
                "note"
            } else {
                "step"
            },
            if count == 1 { "" } else { "s" }
        ));
        cx.notify();
    }

    /// Exact step-pad audition request used by pad/lane controls. Both lane
    /// and target are retained so stale picker state is refused downstream.
    pub fn request_pad_audition(
        &mut self,
        lane: StepLaneId,
        target: TriggerTarget,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.require_audition_available(cx) {
            return false;
        }
        let Some(callback) = self.shared_audition_callback.clone() else {
            self.status = Some("Shared pattern audition callback is not connected".into());
            cx.notify();
            return false;
        };
        let Some(occurrence) = self
            .source
            .workflow
            .as_ref()
            .and_then(|context| context.occurrence)
        else {
            self.status = Some("Select a placed occurrence to audition a pad".into());
            cx.notify();
            return false;
        };
        callback(PatternAuditionRequest {
            expected_project_revision: self.expected_project_revision,
            occurrence,
            cycle_index: self.preview_cycle,
            performance_seed: self.preview_seed,
            scope: PatternAuditionScope::Pad(PatternAuditionPad { lane, target }),
        });
        self.status = Some(format!("Preparing lane #{} audition", lane.get()));
        cx.notify();
        true
    }

    fn audition_key(&mut self, midi_key: u8, cx: &mut Context<Self>) {
        if !self.require_audition_available(cx) {
            return;
        }
        let (Some(callback), Some(instrument)) =
            (self.audition_callback.clone(), self.note_instrument)
        else {
            self.status = Some("Choose a routed instrument and connect host audition".into());
            cx.notify();
            return;
        };
        let Some(pattern) = self.pattern_id_for(EditorMode::PianoRoll) else {
            return;
        };
        callback(PianoAuditionRequest {
            pattern,
            occurrence: self
                .source
                .workflow
                .as_ref()
                .and_then(|context| context.occurrence),
            track: self.piano_occurrence_track(),
            cycle_index: self.preview_cycle,
            performance_seed: self.preview_seed,
            instrument,
            midi_key,
            velocity: 0.82,
            duration: BeatDuration(self.quantize_grid),
        });
        self.status = Some(format!(
            "Audition {} on instrument #{instrument}",
            note_name(midi_key)
        ));
        cx.notify();
    }

    fn require_audition_available(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(reason) = self
            .audition_availability
            .unavailable_reason()
            .map(str::to_owned)
        else {
            return true;
        };
        self.status = Some(reason);
        cx.notify();
        false
    }

    fn piano_occurrence_track(&self) -> Option<TrackId> {
        let context = self.source.workflow.as_ref()?;
        let target = context.occurrence?;
        context
            .uses
            .occurrences
            .iter()
            .find(|occurrence| occurrence.target == target)
            .map(|occurrence| occurrence.track)
    }

    fn select_all_events(&mut self, cx: &mut Context<Self>) {
        let Some(pattern) = self.active_pattern() else {
            return;
        };
        match pattern.content {
            PatternContent::Notes(notes) => {
                self.selected_steps.clear();
                self.selected_notes = notes.notes.keys().copied().collect();
                self.selection = self
                    .selected_notes
                    .iter()
                    .next()
                    .copied()
                    .map(Selection::Note);
            }
            PatternContent::Steps(steps) => {
                self.selected_notes.clear();
                self.selected_steps = step_workflow::all_steps(&steps);
                self.selection = self
                    .selected_steps
                    .iter()
                    .next()
                    .copied()
                    .map(|(lane, step)| Selection::Step(lane, step));
            }
        }
        cx.notify();
    }

    fn duplicate_selection(&mut self, cx: &mut Context<Self>) {
        let Some(before) = self.active_pattern() else {
            return;
        };
        let mut after = before.clone();
        let label = match &before.content {
            PatternContent::Notes(notes) => {
                if self.selected_notes.is_empty() {
                    return;
                }
                let (notes, selected) = piano_workflow::duplicate_notes(
                    notes,
                    &self.selected_notes,
                    self.next_available_note_id().get(),
                    self.quantize_grid as i64,
                    before.length,
                );
                after.content = PatternContent::Notes(notes);
                self.selected_notes = selected;
                self.selection = self
                    .selected_notes
                    .iter()
                    .next()
                    .copied()
                    .map(Selection::Note);
                "Duplicate piano notes"
            }
            PatternContent::Steps(steps) => {
                if self.selected_steps.is_empty() {
                    return;
                }
                let offset = step_workflow::duplication_offset(&self.selected_steps);
                let (steps, selected) = step_workflow::duplicate_steps(
                    steps,
                    &self.selected_steps,
                    offset,
                    before.length,
                );
                if selected.is_empty() {
                    self.status =
                        Some("Steps cannot duplicate without overwriting an occupied cell".into());
                    cx.notify();
                    return;
                }
                after.content = PatternContent::Steps(steps);
                self.selected_steps = selected;
                self.selection = self
                    .selected_steps
                    .iter()
                    .next()
                    .copied()
                    .map(|(lane, step)| Selection::Step(lane, step));
                "Duplicate drum steps"
            }
        };
        self.execute_pattern(label, before, after, cx);
    }

    fn adjust_selected_velocity(&mut self, delta: f32, cx: &mut Context<Self>) {
        let Some(before) = self.active_pattern() else {
            return;
        };
        let mut after = before.clone();
        let label = match &before.content {
            PatternContent::Notes(notes) => {
                if self.selected_notes.is_empty() {
                    self.status = Some("Select one or more piano notes first".into());
                    cx.notify();
                    return;
                }
                let batch = NoteBatch::capture(notes, &self.selected_notes);
                after.content = PatternContent::Notes(piano_workflow::replace_notes(
                    notes,
                    batch.velocity_scaled(delta),
                ));
                "Adjust note velocity"
            }
            PatternContent::Steps(steps) => {
                if self.selected_steps.is_empty() {
                    self.status = Some("Select one or more drum steps first".into());
                    cx.notify();
                    return;
                }
                after.content = PatternContent::Steps(step_workflow::adjust_steps(
                    steps,
                    &self.selected_steps,
                    StepPropertyDelta::Velocity(delta),
                ));
                "Adjust step velocity"
            }
        };
        self.execute_pattern(label, before, after, cx);
    }

    fn adjust_selected_probability(&mut self, delta: f32, cx: &mut Context<Self>) {
        let Some(before) = self.active_pattern() else {
            return;
        };
        let mut after = before.clone();
        let label = match &before.content {
            PatternContent::Notes(notes) => {
                if self.selected_notes.is_empty() {
                    self.status = Some("Select one or more piano notes first".into());
                    cx.notify();
                    return;
                }
                let batch = NoteBatch::capture(notes, &self.selected_notes);
                after.content = PatternContent::Notes(piano_workflow::replace_notes(
                    notes,
                    batch.probability_scaled(delta),
                ));
                "Adjust note probability"
            }
            PatternContent::Steps(steps) => {
                if self.selected_steps.is_empty() {
                    self.status = Some("Select one or more drum steps first".into());
                    cx.notify();
                    return;
                }
                after.content = PatternContent::Steps(step_workflow::adjust_steps(
                    steps,
                    &self.selected_steps,
                    StepPropertyDelta::Probability(delta),
                ));
                "Adjust step probability"
            }
        };
        self.execute_pattern(label, before, after, cx);
    }

    fn adjust_selected_microtiming(&mut self, delta: i32, cx: &mut Context<Self>) {
        let Some(before) = self.active_pattern() else {
            return;
        };
        let mut after = before.clone();
        let label = match &before.content {
            PatternContent::Notes(notes) => {
                if self.selected_notes.is_empty() {
                    self.status = Some("Select one or more piano notes first".into());
                    cx.notify();
                    return;
                }
                let batch = NoteBatch::capture(notes, &self.selected_notes);
                let maximum = (self.quantize_grid / 2).min(i32::MAX as u64) as i32;
                after.content = PatternContent::Notes(piano_workflow::replace_notes(
                    notes,
                    batch.microtiming_shifted(delta, maximum),
                ));
                "Adjust note microtiming"
            }
            PatternContent::Steps(steps) => {
                if self.selected_steps.is_empty() {
                    self.status = Some("Select one or more drum steps first".into());
                    cx.notify();
                    return;
                }
                after.content = PatternContent::Steps(step_workflow::adjust_steps(
                    steps,
                    &self.selected_steps,
                    StepPropertyDelta::MicroOffset(delta),
                ));
                "Adjust step microtiming"
            }
        };
        self.execute_pattern(label, before, after, cx);
    }

    fn adjust_selected_step_property(
        &mut self,
        delta: StepPropertyDelta,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        if self.selected_steps.is_empty() {
            self.status = Some("Select one or more drum steps first".into());
            cx.notify();
            return;
        }
        let Some(before) = self.active_pattern() else {
            return;
        };
        let PatternContent::Steps(steps) = &before.content else {
            return;
        };
        let mut after = before.clone();
        after.content = PatternContent::Steps(step_workflow::adjust_steps(
            steps,
            &self.selected_steps,
            delta,
        ));
        self.execute_pattern(label, before, after, cx);
    }

    fn quantize(&mut self, cx: &mut Context<Self>) {
        let Some(before) = self.active_pattern() else {
            return;
        };
        let PatternContent::Notes(notes) = &before.content else {
            self.status = Some("Step events are already locked to their pattern grid".into());
            cx.notify();
            return;
        };
        let selected_only = !self.selected_notes.is_empty();
        let input = if selected_only {
            NotePattern {
                notes: self
                    .selected_notes
                    .iter()
                    .filter_map(|id| notes.notes.get(id).cloned().map(|note| (*id, note)))
                    .collect(),
            }
        } else {
            notes.clone()
        };
        match quantize_notes(
            &input,
            QuantizeSpec {
                grid: BeatDuration(self.quantize_grid),
                strength: 1.0,
            },
        ) {
            Ok(quantized) => {
                let mut after = before.clone();
                after.content = PatternContent::Notes(if selected_only {
                    piano_workflow::replace_notes(notes, quantized.notes)
                } else {
                    quantized
                });
                self.execute_pattern("Quantize notes", before, after, cx);
            }
            Err(error) => {
                self.status = Some(error.to_string());
                cx.notify();
            }
        }
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        self.drag = None;
        self.piano_gesture = None;
        if self.emit(PatternAction::Undo, "Undo requested", cx) {
            return;
        }
        self.status = self
            .source
            .sequencer
            .lock()
            .map_err(|_| "sequencer lock poisoned".to_owned())
            .and_then(|mut value| value.undo().map(|_| ()).map_err(|error| error.to_string()))
            .err();
        cx.notify();
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        self.drag = None;
        self.piano_gesture = None;
        if self.emit(PatternAction::Redo, "Redo requested", cx) {
            return;
        }
        self.status = self
            .source
            .sequencer
            .lock()
            .map_err(|_| "sequencer lock poisoned".to_owned())
            .and_then(|mut value| value.redo().map(|_| ()).map_err(|error| error.to_string()))
            .err();
        cx.notify();
    }

    fn delete_selection(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.selection else {
            return;
        };
        let Some(before) = self.active_pattern() else {
            return;
        };
        if matches!(selection, Selection::Note(_)) && !self.selected_notes.is_empty() {
            let PatternContent::Notes(notes) = &before.content else {
                return;
            };
            let mut after = before.clone();
            after.content =
                PatternContent::Notes(piano_workflow::remove_notes(notes, &self.selected_notes));
            self.selection = None;
            self.selected_notes.clear();
            self.execute_pattern("Delete piano notes", before, after, cx);
            return;
        }
        if matches!(selection, Selection::Step(_, _)) && !self.selected_steps.is_empty() {
            let PatternContent::Steps(steps) = &before.content else {
                return;
            };
            let mut after = before.clone();
            after.content =
                PatternContent::Steps(step_workflow::remove_steps(steps, &self.selected_steps));
            self.selection = None;
            self.selected_steps.clear();
            self.execute_pattern("Delete drum steps", before, after, cx);
            return;
        }
        let mut after = before.clone();
        let edit = match (&mut after.content, selection) {
            (PatternContent::Notes(notes), Selection::Note(id)) => notes
                .notes
                .remove(&id)
                .map(|_| PatternEdit::RemoveNote { note: id }),
            (PatternContent::Steps(steps), Selection::Step(lane, index)) => steps
                .lanes
                .get_mut(&lane)
                .and_then(|lane| lane.steps.remove(&index))
                .map(|_| PatternEdit::RemoveStep { lane, step: index }),
            _ => None,
        };
        if let Some(edit) = edit {
            self.selection = None;
            self.selected_notes.clear();
            self.selected_steps.clear();
            self.execute_semantic_edit("Delete sequencer event", before, after, edit, cx);
        }
    }

    fn grid_local(&self, position: gpui::Point<Pixels>) -> Option<(f32, f32, f32, f32)> {
        let bounds = *self.grid_bounds.lock().ok()?;
        let bounds = bounds?;
        if !bounds.contains(&position) {
            return None;
        }
        Some((
            f32::from(position.x - bounds.origin.x),
            f32::from(position.y - bounds.origin.y),
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
        ))
    }

    fn piano_geometry(&self, width: f32) -> PianoGeometry {
        PianoGeometry {
            width,
            start_tick: self.start_tick,
            ticks_per_pixel: self.ticks_per_pixel,
            top_midi_key: self.top_midi_key,
            row_height: PIANO_ROW_HEIGHT,
            rows: PIANO_ROWS,
        }
    }

    fn begin_pattern_gesture(
        &mut self,
        pattern: PatternId,
        kind: PatternGestureKind,
        cx: &mut Context<Self>,
    ) {
        if self.workflow_callback.is_none() {
            return;
        }
        self.dispatch_workflow(
            PatternWorkflowIntent::BeginGesture(BeginPatternGestureIntent {
                expected_project_revision: self.expected_project_revision,
                editor_session: self.editor_session,
                pattern,
                kind,
            }),
            "Beginning pattern gesture",
            cx,
        );
    }

    fn end_pattern_gesture(&mut self, cx: &mut Context<Self>) {
        let Some(receipt) = self.active_gesture.take() else {
            return;
        };
        self.dispatch_workflow(
            PatternWorkflowIntent::EndGesture(receipt),
            "Committing pattern gesture",
            cx,
        );
    }

    fn step_geometry(&self, width: f32, resolution: u64, rows: usize) -> StepGeometry {
        StepGeometry {
            width,
            start_tick: self.start_tick,
            ticks_per_pixel: self.ticks_per_pixel,
            resolution,
            row_height: STEP_ROW_HEIGHT,
            rows,
        }
    }

    fn begin_pointer(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some((x, y, width, _)) = self.grid_local(event.position) else {
            return;
        };
        let Some(before) = self.active_pattern() else {
            return;
        };
        match &before.content {
            PatternContent::Notes(notes) => {
                self.selected_steps.clear();
                let geometry = self.piano_geometry(width);
                if let Some(note) = hit_note(notes, geometry, x, y) {
                    if event.modifiers.shift {
                        if !self.selected_notes.insert(note.id) {
                            self.selected_notes.remove(&note.id);
                        }
                    } else if !self.selected_notes.contains(&note.id) {
                        self.selected_notes = BTreeSet::from([note.id]);
                    }
                    self.selection = self
                        .selected_notes
                        .iter()
                        .next()
                        .copied()
                        .map(Selection::Note);
                    if !self.selected_notes.contains(&note.id) {
                        self.drag = None;
                        cx.notify();
                        return;
                    }
                    self.note_instrument = note.instrument.or(self.note_instrument);
                    let end_x = geometry.x_for_tick(
                        note.start
                            .0
                            .saturating_add(note.duration.0.min(i64::MAX as u64) as i64),
                    );
                    let resizing = (end_x - x).abs() <= 8.0;
                    let batch = NoteBatch::capture(notes, &self.selected_notes);
                    self.piano_gesture = Some(PianoGestureTransaction::begin(notes));
                    self.drag = Some(if event.modifiers.control {
                        DragGesture::VelocityNotes {
                            origin_y: y,
                            original: batch,
                        }
                    } else if resizing {
                        DragGesture::ResizeNotes {
                            origin_x: x,
                            original: batch,
                        }
                    } else {
                        DragGesture::MoveNotes {
                            origin_x: x,
                            origin_y: y,
                            original: batch,
                        }
                    });
                    self.begin_pattern_gesture(
                        before.id,
                        if event.modifiers.control {
                            PatternGestureKind::AdjustEvent
                        } else if resizing {
                            PatternGestureKind::ResizeNote
                        } else {
                            PatternGestureKind::MoveNote
                        },
                        cx,
                    );
                } else {
                    if !event.modifiers.shift {
                        self.selected_notes.clear();
                        self.selected_steps.clear();
                        self.selection = None;
                    }
                    self.drag = Some(DragGesture::MarqueeNotes {
                        origin_x: x,
                        origin_y: y,
                        current_x: x,
                        current_y: y,
                        baseline: if event.modifiers.shift {
                            self.selected_notes.clone()
                        } else {
                            BTreeSet::new()
                        },
                    });
                }
            }
            PatternContent::Steps(steps) => {
                let lanes = lane_ids(steps);
                let geometry = self.step_geometry(width, steps.resolution.0, lanes.len());
                let Some(row) = geometry.lane_at_y(y) else {
                    return;
                };
                let lane = lanes[row];
                let index = geometry.step_at_x(x);
                if let Some(step) = steps
                    .lanes
                    .get(&lane)
                    .and_then(|lane| lane.steps.get(&index))
                {
                    self.selected_notes.clear();
                    let key = (lane, index);
                    if event.modifiers.shift {
                        if !self.selected_steps.insert(key) {
                            self.selected_steps.remove(&key);
                        }
                    } else if !self.selected_steps.contains(&key) {
                        self.selected_steps = BTreeSet::from([key]);
                    }
                    self.selection = self
                        .selected_steps
                        .iter()
                        .next()
                        .copied()
                        .map(|(lane, step)| Selection::Step(lane, step));
                    if !self.selected_steps.contains(&key) {
                        self.drag = None;
                        cx.notify();
                        return;
                    }
                    self.drag = Some(DragGesture::MoveStep {
                        lane,
                        index,
                        event: step.clone(),
                    });
                    self.begin_pattern_gesture(before.id, PatternGestureKind::MoveStep, cx);
                } else {
                    self.selected_notes.clear();
                    if event.modifiers.shift {
                        self.drag = Some(DragGesture::MarqueeSteps {
                            origin_step: index,
                            origin_lane: row,
                            baseline: self.selected_steps.clone(),
                        });
                        cx.notify();
                        return;
                    }
                    self.selected_steps.clear();
                    self.add_step(before, lane, index, cx);
                    return;
                }
            }
        }
        cx.notify();
    }

    fn remove_at_pointer(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some((x, y, width, _)) = self.grid_local(event.position) else {
            return;
        };
        let Some(before) = self.active_pattern() else {
            return;
        };
        self.selection = match &before.content {
            PatternContent::Notes(notes) => hit_note(notes, self.piano_geometry(width), x, y)
                .map(|note| Selection::Note(note.id)),
            PatternContent::Steps(steps) => {
                let lanes = lane_ids(steps);
                let geometry = self.step_geometry(width, steps.resolution.0, lanes.len());
                geometry.lane_at_y(y).and_then(|row| {
                    let lane = lanes[row];
                    let index = geometry.step_at_x(x);
                    steps
                        .lanes
                        .get(&lane)?
                        .steps
                        .contains_key(&index)
                        .then_some(Selection::Step(lane, index))
                })
            }
        };
        self.selected_notes = match self.selection {
            Some(Selection::Note(id)) => BTreeSet::from([id]),
            _ => BTreeSet::new(),
        };
        self.selected_steps = match self.selection {
            Some(Selection::Step(lane, step)) => BTreeSet::from([(lane, step)]),
            _ => BTreeSet::new(),
        };
        self.delete_selection(cx);
    }

    fn add_note(
        &mut self,
        before: PatternDefinition,
        geometry: PianoGeometry,
        x: f32,
        y: f32,
        bypass_snap: bool,
        cx: &mut Context<Self>,
    ) {
        let id = if self.project_backed() {
            self.next_available_note_id()
        } else {
            match self.source.sequencer.lock() {
                Ok(mut sequencer) => sequencer.allocate_note_id(),
                Err(_) => return,
            }
        };
        let start = if bypass_snap {
            geometry.tick_at_x(x).max(0)
        } else {
            geometry.snapped_tick_at_x(x, self.quantize_grid)
        };
        if start >= before.length.0.min(i64::MAX as u64) as i64 {
            return;
        }
        let step_index = start.div_euclid(self.quantize_grid as i64);
        let swing_ticks = if step_index.rem_euclid(2) == 1 {
            (self.quantize_grid as f32 * 0.5 * self.swing).round() as i32
        } else {
            0
        };
        let note = NoteEvent {
            id,
            start: BeatTime(start),
            duration: BeatDuration(self.quantize_grid),
            pitch: NotePitch {
                midi_key: self.pitch_scale.constrain(i16::from(geometry.key_at_y(y))),
                cents: 0.0,
            },
            velocity: 0.82,
            release_velocity: 0.5,
            pan: 0.0,
            probability: 1.0,
            micro_offset: swing_ticks,
            channel: 0,
            instrument: self.note_instrument,
            articulation: Articulation::Normal,
            expression: PerNoteExpression::default(),
        };
        let mut after = before.clone();
        if let PatternContent::Notes(notes) = &mut after.content {
            notes.notes.insert(id, note.clone());
        }
        self.selection = Some(Selection::Note(id));
        self.selected_notes = BTreeSet::from([id]);
        self.selected_steps.clear();
        self.execute_semantic_edit("Add note", before, after, PatternEdit::PutNote { note }, cx);
    }

    fn add_step(
        &mut self,
        before: PatternDefinition,
        lane: StepLaneId,
        index: u32,
        cx: &mut Context<Self>,
    ) {
        let mut after = before.clone();
        let PatternContent::Steps(steps) = &mut after.content else {
            return;
        };
        if u128::from(index) * u128::from(steps.resolution.0) >= u128::from(before.length.0) {
            return;
        }
        let event = StepEvent {
            velocity: 0.86,
            probability: 1.0,
            micro_offset: 0,
            gate: steps.resolution,
            ratchets: 1,
            pitch_semitones: 0.0,
            pan: 0.0,
        };
        if let Some(value) = steps.lanes.get_mut(&lane) {
            value.steps.insert(index, event.clone());
        }
        self.selection = Some(Selection::Step(lane, index));
        self.selected_steps.insert((lane, index));
        self.execute_semantic_edit(
            "Add step",
            before,
            after,
            PatternEdit::PutStep {
                lane,
                step: index,
                event,
            },
            cx,
        );
    }

    fn drag_pointer(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !event.dragging() {
            return;
        }
        let Some(gesture) = self.drag.clone() else {
            return;
        };
        let Some((x, y, width, _)) = self.grid_local(event.position) else {
            return;
        };
        let Some(before) = self.active_pattern() else {
            return;
        };
        match gesture {
            DragGesture::MoveNotes {
                origin_x,
                origin_y,
                original,
            } => {
                let raw_delta = (f64::from(x - origin_x) * self.ticks_per_pixel).round() as i64;
                let tick_delta = piano_workflow::gesture_tick_delta(
                    raw_delta,
                    self.quantize_grid,
                    event.modifiers.platform,
                );
                let pitch_steps = ((origin_y - y) / PIANO_ROW_HEIGHT).round() as i32;
                let replacement =
                    original.moved(before.length, tick_delta, pitch_steps, self.pitch_scale);
                if let Some(gesture) = self.piano_gesture.as_mut() {
                    gesture.preview_replacement(replacement);
                    cx.notify();
                }
            }
            DragGesture::ResizeNotes { origin_x, original } => {
                let raw_delta = (f64::from(x - origin_x) * self.ticks_per_pixel).round() as i64;
                let delta = piano_workflow::gesture_tick_delta(
                    raw_delta,
                    self.quantize_grid,
                    event.modifiers.platform,
                );
                let replacement =
                    original.resized(before.length, delta, BeatDuration(self.quantize_grid));
                if let Some(gesture) = self.piano_gesture.as_mut() {
                    gesture.preview_replacement(replacement);
                    cx.notify();
                }
            }
            DragGesture::VelocityNotes { origin_y, original } => {
                let delta = ((origin_y - y) / 160.0).clamp(-1.0, 1.0);
                if let Some(gesture) = self.piano_gesture.as_mut() {
                    gesture.preview_replacement(original.velocity_scaled(delta));
                    cx.notify();
                }
            }
            DragGesture::MarqueeNotes {
                origin_x,
                origin_y,
                baseline,
                ..
            } => {
                let PatternContent::Notes(notes) = &before.content else {
                    return;
                };
                let geometry = self.piano_geometry(width);
                let selected = NoteMarquee {
                    start_tick: geometry.tick_at_x(origin_x),
                    end_tick: geometry.tick_at_x(x),
                    low_key: geometry.key_at_y(origin_y),
                    high_key: geometry.key_at_y(y),
                }
                .select(notes);
                self.selected_notes = baseline.union(&selected).copied().collect();
                self.selection = self
                    .selected_notes
                    .iter()
                    .next()
                    .copied()
                    .map(Selection::Note);
                self.drag = Some(DragGesture::MarqueeNotes {
                    origin_x,
                    origin_y,
                    current_x: x,
                    current_y: y,
                    baseline,
                });
                cx.notify();
            }
            DragGesture::MarqueeSteps {
                origin_step,
                origin_lane,
                baseline,
                ..
            } => {
                let PatternContent::Steps(steps) = &before.content else {
                    return;
                };
                let lanes = lane_ids(steps);
                let geometry = self.step_geometry(width, steps.resolution.0, lanes.len());
                let Some(current_lane) = geometry.lane_at_y(y) else {
                    return;
                };
                let current_step = geometry.step_at_x(x);
                let mut selected = baseline.clone();
                selected.extend(
                    step_workflow::StepMarquee {
                        start_step: origin_step,
                        end_step: current_step,
                        start_lane: origin_lane,
                        end_lane: current_lane,
                    }
                    .select(steps),
                );
                self.selected_steps = selected;
                self.selection = self
                    .selected_steps
                    .iter()
                    .next()
                    .copied()
                    .map(|(lane, step)| Selection::Step(lane, step));
                self.drag = Some(DragGesture::MarqueeSteps {
                    origin_step,
                    origin_lane,
                    baseline,
                });
                cx.notify();
            }
            DragGesture::MoveStep { lane, index, event } => {
                let PatternContent::Steps(steps) = &before.content else {
                    return;
                };
                let lanes = lane_ids(steps);
                let geometry = self.step_geometry(width, steps.resolution.0, lanes.len());
                let Some(row) = geometry.lane_at_y(y) else {
                    return;
                };
                let next_lane = lanes[row];
                let next_index = geometry.step_at_x(x);
                if next_lane == lane && next_index == index {
                    return;
                }
                let from_row = lanes
                    .iter()
                    .position(|candidate| *candidate == lane)
                    .unwrap_or(row);
                let selected = if self.selected_steps.is_empty() {
                    BTreeSet::from([(lane, index)])
                } else {
                    self.selected_steps.clone()
                };
                let Some((moved, moved_selection)) = step_workflow::move_steps(
                    steps,
                    &selected,
                    i64::from(next_index).saturating_sub(i64::from(index)),
                    row as i32 - from_row as i32,
                    before.length,
                ) else {
                    return;
                };
                let mut after = before.clone();
                after.content = PatternContent::Steps(moved);
                self.selected_steps = moved_selection;
                self.selection = self
                    .selected_steps
                    .iter()
                    .next()
                    .copied()
                    .map(|(lane, step)| Selection::Step(lane, step));
                self.drag = Some(DragGesture::MoveStep {
                    lane: next_lane,
                    index: next_index,
                    event,
                });
                self.execute_pattern("Move drum steps", before, after, cx);
            }
        }
    }

    fn end_pointer(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        let Some(gesture) = self.drag.take() else {
            return;
        };
        if let DragGesture::MarqueeNotes {
            origin_x,
            origin_y,
            current_x,
            current_y,
            ..
        } = gesture
        {
            if (current_x - origin_x).abs() < 3.0 && (current_y - origin_y).abs() < 3.0 {
                if let (Some((x, y, width, _)), Some(before)) =
                    (self.grid_local(event.position), self.active_pattern())
                {
                    self.add_note(
                        before,
                        self.piano_geometry(width),
                        x,
                        y,
                        event.modifiers.platform,
                        cx,
                    );
                }
            }
        } else {
            if let Some(transaction) = self.piano_gesture.take() {
                if let PianoGestureResolution::Commit(notes) = transaction.finish() {
                    if let Some(before) = self.stored_active_pattern() {
                        let mut after = before.clone();
                        after.content = PatternContent::Notes(notes);
                        self.execute_pattern("Edit piano notes", before, after, cx);
                    }
                }
            }
            self.end_pattern_gesture(cx);
        }
        cx.notify();
    }

    fn cancel_editor_gesture(&mut self, status: &'static str, cx: &mut Context<Self>) {
        let transaction = self.piano_gesture.take();
        let had_drag = self.drag.take().is_some();
        if transaction.is_none() && !had_drag {
            return;
        }
        if let Some(transaction) = transaction {
            let _ = transaction.rollback();
        }
        self.end_pattern_gesture(cx);
        self.status = Some(status.into());
        cx.notify();
    }

    fn selected_edit(
        &mut self,
        time_steps: i64,
        pitch_or_lane: i32,
        resize: i64,
        cx: &mut Context<Self>,
    ) {
        let Some(selection) = self.selection else {
            return;
        };
        let Some(before) = self.active_pattern() else {
            return;
        };
        if matches!(selection, Selection::Note(_)) && !self.selected_notes.is_empty() {
            let PatternContent::Notes(notes) = &before.content else {
                return;
            };
            let batch = NoteBatch::capture(notes, &self.selected_notes);
            let replacement = if resize != 0 {
                batch.resized(
                    before.length,
                    resize.saturating_mul(self.quantize_grid as i64),
                    BeatDuration(self.quantize_grid),
                )
            } else {
                batch.moved(
                    before.length,
                    time_steps.saturating_mul(self.quantize_grid as i64),
                    pitch_or_lane,
                    self.pitch_scale,
                )
            };
            let mut after = before.clone();
            after.content =
                PatternContent::Notes(piano_workflow::replace_notes(notes, replacement));
            self.execute_pattern("Edit piano notes", before, after, cx);
            return;
        }
        if matches!(selection, Selection::Step(_, _)) && !self.selected_steps.is_empty() {
            let PatternContent::Steps(steps) = &before.content else {
                return;
            };
            let mut after = before.clone();
            if resize != 0 {
                after.content = PatternContent::Steps(step_workflow::adjust_steps(
                    steps,
                    &self.selected_steps,
                    StepPropertyDelta::Gate(
                        resize.saturating_mul(steps.resolution.0.min(i64::MAX as u64) as i64),
                    ),
                ));
            } else {
                let Some((moved, selected)) = step_workflow::move_steps(
                    steps,
                    &self.selected_steps,
                    time_steps,
                    pitch_or_lane.saturating_neg(),
                    before.length,
                ) else {
                    return;
                };
                after.content = PatternContent::Steps(moved);
                self.selected_steps = selected;
                self.selection = self
                    .selected_steps
                    .iter()
                    .next()
                    .copied()
                    .map(|(lane, step)| Selection::Step(lane, step));
            }
            self.execute_pattern("Edit drum steps", before, after, cx);
            return;
        }
        let mut after = before.clone();
        let edit = match (&mut after.content, selection) {
            (PatternContent::Notes(notes), Selection::Note(id)) => {
                let Some(note) = notes.notes.get_mut(&id) else {
                    return;
                };
                if resize != 0 {
                    let delta = i128::from(resize) * i128::from(self.quantize_grid);
                    let max = before.length.0.saturating_sub(note.start.0 as u64);
                    note.duration.0 = (i128::from(note.duration.0) + delta)
                        .clamp(i128::from(self.quantize_grid), i128::from(max))
                        as u64;
                } else {
                    let delta = time_steps.saturating_mul(self.quantize_grid as i64);
                    let max = before.length.0.saturating_sub(note.duration.0) as i64;
                    note.start.0 = note.start.0.saturating_add(delta).clamp(0, max);
                    note.pitch.midi_key =
                        (i32::from(note.pitch.midi_key) + pitch_or_lane).clamp(0, 127) as u8;
                }
                Some(PatternEdit::PutNote { note: note.clone() })
            }
            (PatternContent::Steps(steps), Selection::Step(lane, index)) => {
                if resize != 0 {
                    let Some(event) = steps
                        .lanes
                        .get_mut(&lane)
                        .and_then(|lane| lane.steps.get_mut(&index))
                    else {
                        return;
                    };
                    let delta = i128::from(resize) * i128::from(steps.resolution.0);
                    event.gate.0 = (i128::from(event.gate.0) + delta)
                        .max(i128::from(steps.resolution.0))
                        as u64;
                    Some(PatternEdit::PutStep {
                        lane,
                        step: index,
                        event: event.clone(),
                    })
                } else {
                    let lanes = lane_ids(steps);
                    let row = lanes.iter().position(|value| *value == lane).unwrap_or(0) as i32;
                    let next_row =
                        (row - pitch_or_lane).clamp(0, lanes.len().saturating_sub(1) as i32);
                    let max_step = before.length.0.saturating_sub(1) / steps.resolution.0;
                    let next_index =
                        (i64::from(index) + time_steps).clamp(0, max_step as i64) as u32;
                    let next_lane = lanes[next_row as usize];
                    if (next_lane != lane || next_index != index)
                        && steps
                            .lanes
                            .get(&next_lane)
                            .is_some_and(|lane| lane.steps.contains_key(&next_index))
                    {
                        return;
                    }
                    let Some(event) = steps
                        .lanes
                        .get_mut(&lane)
                        .and_then(|lane| lane.steps.remove(&index))
                    else {
                        return;
                    };
                    steps
                        .lanes
                        .get_mut(&next_lane)
                        .unwrap()
                        .steps
                        .insert(next_index, event);
                    self.selection = Some(Selection::Step(next_lane, next_index));
                    Some(PatternEdit::MoveStep {
                        from_lane: lane,
                        from_step: index,
                        to_lane: next_lane,
                        to_step: next_index,
                    })
                }
            }
            _ => None,
        };
        if let Some(edit) = edit {
            self.execute_semantic_edit("Edit sequencer event", before, after, edit, cx);
        }
    }

    fn zoom(&mut self, factor: f64, cx: &mut Context<Self>) {
        let width = self
            .grid_bounds
            .lock()
            .ok()
            .and_then(|value| value.as_ref().map(|bounds| f64::from(bounds.size.width)))
            .unwrap_or(900.0);
        let center = self.start_tick as f64 + width * self.ticks_per_pixel * 0.5;
        self.ticks_per_pixel =
            (self.ticks_per_pixel * factor).clamp(MIN_TICKS_PER_PIXEL, MAX_TICKS_PER_PIXEL);
        self.start_tick = (center - width * self.ticks_per_pixel * 0.5)
            .round()
            .max(0.0) as i64;
        cx.notify();
    }

    fn pan(&mut self, fraction: f64, cx: &mut Context<Self>) {
        let width = self
            .grid_bounds
            .lock()
            .ok()
            .and_then(|value| value.as_ref().map(|bounds| f64::from(bounds.size.width)))
            .unwrap_or(900.0);
        let delta = (width * self.ticks_per_pixel * fraction).round() as i64;
        self.start_tick = self.start_tick.saturating_add(delta).max(0);
        cx.notify();
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(window.line_height());
        if event.modifiers.secondary() || event.modifiers.control {
            let wheel = if delta.y.abs() >= delta.x.abs() {
                delta.y
            } else {
                delta.x
            };
            self.zoom((-f64::from(wheel) / 240.0).exp(), cx);
        } else if event.modifiers.alt && self.mode == EditorMode::PianoRoll {
            let rows = (f32::from(delta.y) / PIANO_ROW_HEIGHT).round() as i16;
            self.top_midi_key = (i16::from(self.top_midi_key) + rows).clamp(23, 127) as u8;
            cx.notify();
        } else {
            let amount = if delta.x.abs() > px(0.1) || event.modifiers.shift {
                -f64::from(if delta.x.abs() > px(0.1) {
                    delta.x
                } else {
                    delta.y
                })
            } else {
                -f64::from(delta.y)
            };
            self.start_tick = self
                .start_tick
                .saturating_add((amount * self.ticks_per_pixel).round() as i64)
                .max(0);
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (bpm, meter) = self
            .source
            .sequencer
            .lock()
            .ok()
            .map(|sequencer| {
                let tempo = sequencer.tempo_map().tempo_at(BeatTime(self.start_tick));
                let meter = sequencer.tempo_map().meter_at(BeatTime(self.start_tick));
                (tempo.bpm(), meter)
            })
            .unwrap_or((
                0.0,
                crate::sequencer::TimeSignature {
                    numerator: 4,
                    denominator: 4,
                },
            ));
        let grid_label = grid_name(self.quantize_grid);
        let target_label = self
            .active_pattern()
            .map(|pattern| format!("#{} · {}", pattern.id.get(), pattern.name))
            .unwrap_or_else(|| "NO PATTERN".into());
        let use_count = self
            .source
            .workflow
            .as_ref()
            .map_or(0, |context| context.uses.occurrences.len());
        let instrument_label = self
            .note_instrument
            .map(|instrument| format!("INST #{instrument}"))
            .unwrap_or_else(|| "INST UNROUTED".into());
        let audition_available = self.audition_availability.is_available();
        div()
            .h(px(48.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(TEXT))
                    .child(self.source.title.clone()),
            )
            .child(div().w(px(1.0)).h(px(20.0)).bg(rgb(BORDER)))
            .child(
                control_button("seq-pattern-target", target_label)
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_pattern_target(cx))),
            )
            .child(
                control_button("seq-pattern-new", "+ NEW")
                    .on_click(cx.listener(|this, _, _, cx| this.create_pattern(cx))),
            )
            .child(
                control_button("seq-pattern-duplicate", "DUP")
                    .on_click(cx.listener(|this, _, _, cx| this.duplicate_pattern(cx))),
            )
            .child(
                control_button("seq-pattern-unique", "UNIQUE")
                    .on_click(cx.listener(|this, _, _, cx| this.make_occurrence_unique(cx))),
            )
            .child(
                control_button("seq-pattern-delete", "DEL")
                    .on_click(cx.listener(|this, _, _, cx| this.delete_pattern(cx))),
            )
            .child(
                toggle_button(
                    "seq-mode-notes",
                    "PIANO ROLL",
                    self.mode == EditorMode::PianoRoll,
                )
                .on_click(
                    cx.listener(|this, _, _, cx| this.request_mode(EditorMode::PianoRoll, cx)),
                ),
            )
            .child(
                toggle_button(
                    "seq-mode-steps",
                    "DRUM / STEPS",
                    self.mode == EditorMode::Steps,
                )
                .on_click(cx.listener(|this, _, _, cx| this.request_mode(EditorMode::Steps, cx))),
            )
            .when(self.mode == EditorMode::Steps, |this| {
                this.child(
                    control_button("seq-lane-add", "+ LANE")
                        .on_click(cx.listener(|this, _, _, cx| this.add_lane(cx))),
                )
                .child(
                    control_button("seq-lane-remove", "− LANE")
                        .on_click(cx.listener(|this, _, _, cx| this.remove_lane(cx))),
                )
                .child(
                    control_button("seq-step-duplicate", "DUP STEPS")
                        .on_click(cx.listener(|this, _, _, cx| this.duplicate_selection(cx))),
                )
                .child(
                    audition_button("seq-step-audition", "PLAY STEPS", audition_available).when(
                        audition_available,
                        |button| {
                            button
                                .on_click(cx.listener(|this, _, _, cx| this.audition_selected(cx)))
                        },
                    ),
                )
            })
            .when(self.mode == EditorMode::PianoRoll, |this| {
                this.child(
                    control_button(
                        "seq-piano-scale",
                        format!("C {}", self.pitch_scale.kind.label()),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_scale(cx))),
                )
                .child(
                    control_button("seq-piano-instrument", instrument_label)
                        .on_click(cx.listener(|this, _, _, cx| this.cycle_note_instrument(cx))),
                )
                .child(
                    control_button("seq-note-duplicate", "DUP NOTES")
                        .on_click(cx.listener(|this, _, _, cx| this.duplicate_selection(cx))),
                )
                .child(
                    audition_button("seq-note-audition", "PLAY NOTES", audition_available).when(
                        audition_available,
                        |button| {
                            button
                                .on_click(cx.listener(|this, _, _, cx| this.audition_selected(cx)))
                        },
                    ),
                )
            })
            .child(div().flex_1())
            .child(
                control_button("seq-pattern-uses", format!("USES {use_count}"))
                    .on_click(cx.listener(|this, _, _, cx| this.reveal_pattern_uses(cx))),
            )
            .child(readout(format!("{bpm:.2} BPM")))
            .child(readout(format!(
                "{}/{}",
                meter.numerator, meter.denominator
            )))
            .child(
                control_button("seq-grid", format!("GRID {grid_label}"))
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_grid(cx))),
            )
            .child(
                control_button(
                    "seq-swing",
                    format!("SWING {:02}%", (self.swing * 100.0).round() as u8),
                )
                .on_click(cx.listener(|this, _, _, cx| this.cycle_swing(cx))),
            )
            .child(
                control_button("seq-cycle-prev", "‹")
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_preview(-1, cx))),
            )
            .child(readout(format!("CYCLE {}", self.preview_cycle + 1)))
            .child(
                control_button("seq-cycle-next", "›")
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_preview(1, cx))),
            )
            .child(
                audition_button("seq-cycle-audition", "AUDITION", audition_available)
                    .when(audition_available, |button| {
                        button.on_click(cx.listener(|this, _, _, cx| this.audition_cycle(cx)))
                    }),
            )
            .child(
                control_button("seq-quantize", "QUANTIZE")
                    .on_click(cx.listener(|this, _, _, cx| this.quantize(cx))),
            )
    }

    fn render_expression(
        &self,
        pattern: &PatternDefinition,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if !matches!(pattern.content, PatternContent::Steps(_)) {
            return div().into_any_element();
        }
        let (origin_label, origin_color, diverged) = match &pattern.origin {
            PatternOrigin::Authored => ("AUTHORED", MUTED, false),
            PatternOrigin::Expression { diverged, .. } => (
                if *diverged {
                    "EXPR · DIVERGED"
                } else {
                    "EXPRESSION"
                },
                if *diverged { MAGENTA } else { CYAN },
                *diverged,
            ),
            PatternOrigin::Deprojected { diverged, .. } => (
                if *diverged {
                    "DEPROJECTED · DIVERGED"
                } else {
                    "DEPROJECTED"
                },
                if *diverged { MAGENTA } else { AMBER },
                *diverged,
            ),
        };
        let content = if self.expression.is_empty() {
            "type a pattern, e.g. swing(0.25, kick^0.9 ~ snare <hat hat:2>)".to_owned()
        } else if self.expression_focused {
            format!("{}▏", self.expression)
        } else {
            self.expression.clone()
        };
        div()
            .h(px(58.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL_ALT))
            .child(
                div()
                    .w(px(105.0))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(origin_color))
                            .child(origin_label),
                    )
                    .child(div().text_xs().text_color(rgb(DIM)).child("PATTERN TERM")),
            )
            .child(
                div()
                    .id("sequencer-expression-input")
                    .h(px(34.0))
                    .flex_1()
                    .min_w_0()
                    .px_3()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(if self.expression_focused {
                        CYAN
                    } else {
                        BORDER
                    }))
                    .bg(rgb(BACKGROUND))
                    .text_sm()
                    .text_color(rgb(if self.expression.is_empty() {
                        DIM
                    } else {
                        TEXT
                    }))
                    .cursor_text()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.expression_focused = true;
                        window.focus(&this.focus_handle);
                        cx.notify();
                    }))
                    .child(content),
            )
            .child(
                div()
                    .w(px(300.0))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .text_xs()
                    .child(div().flex().gap_1().overflow_hidden().children(
                        self.expression_bindings.iter().map(|(name, target)| {
                            let binding = name.clone();
                            div()
                                .id(SharedString::from(format!("expr-binding-{name}")))
                                .px_1()
                                .rounded_sm()
                                .border_1()
                                .border_color(rgb(BORDER))
                                .text_color(rgb(AMBER))
                                .cursor_pointer()
                                .hover(|style| style.border_color(rgb(CYAN)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.cycle_expression_binding(&binding, cx)
                                }))
                                .child(format!("{name}→{}", self.trigger_target_label(target)))
                        }),
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_color(rgb(if diverged { MAGENTA } else { DIM }))
                            .child(if diverged {
                                "Manual grid differs from its term"
                            } else {
                                "Bindings use stable pad/target IDs"
                            })
                            .child(control_button("seq-expression-apply", "APPLY").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.apply_expression(DivergedOverwrite::Refuse, cx)
                                }),
                            ))
                            .when(diverged, |this| {
                                this.child(
                                    control_button("seq-expression-overwrite", "OVERWRITE")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.apply_expression(DivergedOverwrite::Confirmed, cx)
                                        })),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }

    fn trigger_target_label(&self, target: &TriggerTarget) -> String {
        self.source
            .trigger_targets
            .iter()
            .find(|option| option.target == *target)
            .map(|option| option.label.clone())
            .unwrap_or_else(|| target_label(target))
    }

    fn render_ruler(&self, pattern: &PatternDefinition) -> impl IntoElement {
        let map = self
            .source
            .sequencer
            .lock()
            .ok()
            .map(|sequencer| sequencer.tempo_map().clone());
        let visible_end = self
            .start_tick
            .saturating_add((1_200.0 * self.ticks_per_pixel).ceil() as i64);
        let markers = map
            .as_ref()
            .map(|map| piano_workflow::visible_bar_markers(map, self.start_tick, visible_end, 64))
            .unwrap_or_default();
        let total_bars = map.as_ref().map_or(0, |map| {
            map.musical_position(BeatTime(pattern.length.0.min(i64::MAX as u64) as i64))
                .bar
                .max(0)
        });
        div()
            .h(px(30.0))
            .flex_shrink_0()
            .flex()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL_ALT))
            .child(
                div()
                    .w(px(LABEL_WIDTH))
                    .flex_shrink_0()
                    .px_3()
                    .flex()
                    .items_center()
                    .text_xs()
                    .text_color(rgb(DIM))
                    .child(format!("{total_bars} bars · meter map")),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .relative()
                    .overflow_hidden()
                    .children(markers.into_iter().enumerate().map(|(index, marker)| {
                        let x =
                            ((marker.tick - self.start_tick) as f64 / self.ticks_per_pixel) as f32;
                        div()
                            .absolute()
                            .left(px(x + 5.0))
                            .top(px(7.0))
                            .text_xs()
                            .text_color(rgb(if index == 0 { CYAN } else { MUTED }))
                            .child(format!(
                                "{}  {}/{}",
                                marker.bar + 1,
                                marker.signature.numerator,
                                marker.signature.denominator
                            ))
                    })),
            )
    }

    fn render_labels(
        &self,
        pattern: &PatternDefinition,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match &pattern.content {
            PatternContent::Notes(_) => div()
                .w(px(LABEL_WIDTH))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .bg(rgb(PANEL_ALT))
                .children((0..PIANO_ROWS).map(|row| {
                    let key = (i16::from(self.top_midi_key) - row as i16).clamp(0, 127) as u8;
                    let black = is_black_key(key);
                    div()
                        .id(SharedString::from(format!("sequencer-key-{key}")))
                        .h(px(PIANO_ROW_HEIGHT))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_2()
                        .border_b_1()
                        .border_color(rgba(0xffffff0b))
                        .bg(rgb(if black { 0x0a0d13 } else { PANEL_ALT }))
                        .text_xs()
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| this.audition_key(key, cx)))
                        .text_color(rgb(if key % 12 == 0 { CYAN } else { MUTED }))
                        .child(note_name(key))
                        .child(format!("{key}"))
                }))
                .into_any_element(),
            PatternContent::Steps(steps) => div()
                .w(px(LABEL_WIDTH))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .bg(rgb(PANEL_ALT))
                .children(steps.lanes.values().map(|lane| {
                    let lane_id = lane.id;
                    let lane_target = lane.target.clone();
                    div()
                        .id(SharedString::from(format!(
                            "sequencer-lane-{}",
                            lane_id.get()
                        )))
                        .h(px(STEP_ROW_HEIGHT))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .border_b_1()
                        .border_color(rgba(0xffffff10))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(BORDER)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                if event.modifiers.platform {
                                    this.cycle_lane_target(lane_id, cx);
                                } else {
                                    this.request_pad_audition(lane_id, lane_target.clone(), cx);
                                }
                                cx.stop_propagation();
                            }),
                        )
                        .child(
                            div()
                                .size(px(7.0))
                                .rounded_full()
                                .bg(rgb(lane_color(lane.id))),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .text_xs()
                                .text_color(rgb(TEXT))
                                .child(lane.name.clone())
                                .child(div().text_color(rgb(DIM)).child(format!(
                                    "{} · click audition · ⌘ map",
                                    self.trigger_target_label(&lane.target)
                                ))),
                        )
                }))
                .into_any_element(),
        }
    }

    fn render_grid(&self, pattern: PatternDefinition, cx: &mut Context<Self>) -> impl IntoElement {
        let bounds_store = self.grid_bounds.clone();
        let start_tick = self.start_tick;
        let ticks_per_pixel = self.ticks_per_pixel;
        let top_key = self.top_midi_key;
        let quantize = self.quantize_grid;
        let selection = self.selection;
        let selected_notes = self.selected_notes.clone();
        let selected_steps = self.selected_steps.clone();
        let marquee = self.drag.as_ref().and_then(|gesture| match gesture {
            DragGesture::MarqueeNotes {
                origin_x,
                origin_y,
                current_x,
                current_y,
                ..
            } => Some((*origin_x, *origin_y, *current_x, *current_y)),
            _ => None,
        });
        let tempo_map = self
            .source
            .sequencer
            .lock()
            .ok()
            .map(|sequencer| sequencer.tempo_map().clone());
        let height = match &pattern.content {
            PatternContent::Notes(_) => PIANO_ROW_HEIGHT * PIANO_ROWS as f32,
            PatternContent::Steps(steps) => STEP_ROW_HEIGHT * steps.lanes.len().max(1) as f32,
        };
        div()
            .id("sequencer-event-grid")
            .relative()
            .flex_1()
            .min_w_0()
            .h(px(height))
            .overflow_hidden()
            .bg(rgb(BACKGROUND))
            .cursor_crosshair()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus_handle);
                    this.begin_pointer(event, cx);
                    cx.stop_propagation();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    this.remove_at_pointer(event, cx);
                    cx.stop_propagation();
                }),
            )
            .on_mouse_move(
                cx.listener(|this, event: &MouseMoveEvent, _, cx| this.drag_pointer(event, cx)),
            )
            .capture_any_mouse_up(
                cx.listener(|this, event: &MouseUpEvent, _, cx| this.end_pointer(event, cx)),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                this.scroll(event, window, cx)
            }))
            .child(
                canvas(
                    move |bounds, _, _| {
                        *bounds_store.lock().unwrap() = Some(bounds);
                        bounds
                    },
                    move |bounds, _, window, _| {
                        paint_editor_grid(
                            bounds,
                            &pattern,
                            start_tick,
                            ticks_per_pixel,
                            top_key,
                            quantize,
                            tempo_map.as_ref(),
                            selection,
                            &selected_notes,
                            &selected_steps,
                            marquee,
                            window,
                        );
                    },
                )
                .size_full(),
            )
    }

    fn render_inspector(
        &self,
        pattern: &PatternDefinition,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let lines = inspector_lines(
            pattern,
            self.selection,
            self.source.sequencer.lock().ok().as_deref(),
        );
        let inspector_height = if matches!(pattern.content, PatternContent::Steps(_)) {
            154.0
        } else {
            118.0
        };
        div()
            .h(px(inspector_height))
            .flex_shrink_0()
            .flex()
            .border_t_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(
                div()
                    .w(px(LABEL_WIDTH))
                    .flex_shrink_0()
                    .p_3()
                    .text_xs()
                    .text_color(rgb(DIM))
                    .child("EVENT INSPECTOR"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .p_3()
                    .flex()
                    .flex_wrap()
                    .content_start()
                    .gap_x_6()
                    .gap_y_2()
                    .children(lines.into_iter().map(|(label, value)| {
                        div()
                            .w(px(190.0))
                            .flex()
                            .justify_between()
                            .gap_2()
                            .text_xs()
                            .child(div().text_color(rgb(DIM)).child(label))
                            .child(div().text_color(rgb(TEXT)).child(value))
                    })),
            )
            .when(
                matches!(pattern.content, PatternContent::Notes(_)),
                |this| {
                    this.child(
                        div()
                            .w(px(225.0))
                            .flex_shrink_0()
                            .p_3()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .text_xs()
                            .text_color(rgb(DIM))
                            .child(format!(
                                "{} NOTE{} SELECTED",
                                self.selected_notes.len(),
                                if self.selected_notes.len() == 1 {
                                    ""
                                } else {
                                    "S"
                                }
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .gap_1()
                                    .child(
                                        control_button("seq-note-velocity-down", "VEL −").on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.adjust_selected_velocity(-0.05, cx)
                                            }),
                                        ),
                                    )
                                    .child(
                                        control_button("seq-note-velocity-up", "VEL +").on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.adjust_selected_velocity(0.05, cx)
                                            }),
                                        ),
                                    )
                                    .child(
                                        control_button("seq-note-probability-down", "PROB −")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.adjust_selected_probability(-0.05, cx)
                                            })),
                                    )
                                    .child(
                                        control_button("seq-note-probability-up", "PROB +")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.adjust_selected_probability(0.05, cx)
                                            })),
                                    )
                                    .child(
                                        control_button("seq-note-microtiming-earlier", "TIME −")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.adjust_selected_microtiming(
                                                    -(PPQ / 96) as i32,
                                                    cx,
                                                )
                                            })),
                                    )
                                    .child(
                                        control_button("seq-note-microtiming-later", "TIME +")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.adjust_selected_microtiming(
                                                    (PPQ / 96) as i32,
                                                    cx,
                                                )
                                            })),
                                    ),
                            ),
                    )
                },
            )
            .when(
                matches!(pattern.content, PatternContent::Steps(_)),
                |this| {
                    this.child(
                        div()
                            .w(px(250.0))
                            .flex_shrink_0()
                            .p_3()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .text_xs()
                            .text_color(rgb(DIM))
                            .child(format!(
                                "{} STEP{} SELECTED",
                                self.selected_steps.len(),
                                if self.selected_steps.len() == 1 {
                                    ""
                                } else {
                                    "S"
                                }
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .gap_1()
                                    .child(
                                        control_button("seq-step-velocity-down", "VEL −").on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.adjust_selected_velocity(-0.05, cx)
                                            }),
                                        ),
                                    )
                                    .child(
                                        control_button("seq-step-velocity-up", "VEL +").on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.adjust_selected_velocity(0.05, cx)
                                            }),
                                        ),
                                    )
                                    .child(
                                        control_button("seq-step-probability-down", "PROB −")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.adjust_selected_probability(-0.05, cx)
                                            })),
                                    )
                                    .child(
                                        control_button("seq-step-probability-up", "PROB +")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.adjust_selected_probability(0.05, cx)
                                            })),
                                    )
                                    .child(
                                        control_button("seq-step-ratchet-down", "RATCH −")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.adjust_selected_step_property(
                                                    StepPropertyDelta::Ratchets(-1),
                                                    "Decrease step ratchets",
                                                    cx,
                                                )
                                            })),
                                    )
                                    .child(
                                        control_button("seq-step-ratchet-up", "RATCH +").on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.adjust_selected_step_property(
                                                    StepPropertyDelta::Ratchets(1),
                                                    "Increase step ratchets",
                                                    cx,
                                                )
                                            }),
                                        ),
                                    )
                                    .child(
                                        control_button("seq-step-microtiming-earlier", "TIME −")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.adjust_selected_microtiming(
                                                    -(PPQ / 96) as i32,
                                                    cx,
                                                )
                                            })),
                                    )
                                    .child(
                                        control_button("seq-step-microtiming-later", "TIME +")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.adjust_selected_microtiming(
                                                    (PPQ / 96) as i32,
                                                    cx,
                                                )
                                            })),
                                    )
                                    .child(
                                        control_button("seq-step-pitch-down", "PITCH −").on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.adjust_selected_step_property(
                                                    StepPropertyDelta::Pitch(-1.0),
                                                    "Lower step pitch",
                                                    cx,
                                                )
                                            }),
                                        ),
                                    )
                                    .child(control_button("seq-step-pitch-up", "PITCH +").on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.adjust_selected_step_property(
                                                StepPropertyDelta::Pitch(1.0),
                                                "Raise step pitch",
                                                cx,
                                            )
                                        }),
                                    ))
                                    .child(control_button("seq-step-pan-left", "PAN ←").on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.adjust_selected_step_property(
                                                StepPropertyDelta::Pan(-0.1),
                                                "Pan steps left",
                                                cx,
                                            )
                                        }),
                                    ))
                                    .child(control_button("seq-step-pan-right", "PAN →").on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.adjust_selected_step_property(
                                                StepPropertyDelta::Pan(0.1),
                                                "Pan steps right",
                                                cx,
                                            )
                                        }),
                                    )),
                            ),
                    )
                },
            )
            .when_some(self.status.clone(), |this, status| {
                this.child(
                    div()
                        .w(px(260.0))
                        .p_3()
                        .text_xs()
                        .text_color(rgb(MAGENTA))
                        .child(status),
                )
            })
            .into_any_element()
    }

    fn on_toggle_mode(&mut self, _: &ToggleEditorMode, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_mode(cx);
    }
    fn on_undo(&mut self, _: &EditorUndo, _: &mut Window, cx: &mut Context<Self>) {
        self.undo(cx);
    }
    fn on_redo(&mut self, _: &EditorRedo, _: &mut Window, cx: &mut Context<Self>) {
        self.redo(cx);
    }
    fn on_delete(&mut self, _: &EditorDelete, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_selection(cx);
    }
    fn on_left(&mut self, _: &EditorMoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_edit(-1, 0, 0, cx);
    }
    fn on_right(&mut self, _: &EditorMoveRight, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_edit(1, 0, 0, cx);
    }
    fn on_up(&mut self, _: &EditorMoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_edit(0, 1, 0, cx);
    }
    fn on_down(&mut self, _: &EditorMoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_edit(0, -1, 0, cx);
    }
    fn on_resize_left(&mut self, _: &EditorResizeLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_edit(0, 0, -1, cx);
    }
    fn on_resize_right(&mut self, _: &EditorResizeRight, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_edit(0, 0, 1, cx);
    }
    fn on_zoom_in(&mut self, _: &EditorZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom(0.72, cx);
    }
    fn on_zoom_out(&mut self, _: &EditorZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom(1.38, cx);
    }
    fn on_pan_left(&mut self, _: &EditorPanLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.pan(-0.18, cx);
    }
    fn on_pan_right(&mut self, _: &EditorPanRight, _: &mut Window, cx: &mut Context<Self>) {
        self.pan(0.18, cx);
    }
    fn on_quantize(&mut self, _: &EditorQuantize, _: &mut Window, cx: &mut Context<Self>) {
        self.quantize(cx);
    }
    fn on_duplicate(&mut self, _: &EditorDuplicate, _: &mut Window, cx: &mut Context<Self>) {
        self.duplicate_selection(cx);
    }
    fn on_select_all(&mut self, _: &EditorSelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select_all_events(cx);
    }
    fn on_velocity_up(&mut self, _: &EditorVelocityUp, _: &mut Window, cx: &mut Context<Self>) {
        self.adjust_selected_velocity(0.05, cx);
    }
    fn on_velocity_down(&mut self, _: &EditorVelocityDown, _: &mut Window, cx: &mut Context<Self>) {
        self.adjust_selected_velocity(-0.05, cx);
    }
    fn on_probability_up(
        &mut self,
        _: &EditorProbabilityUp,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.adjust_selected_probability(0.05, cx);
    }
    fn on_probability_down(
        &mut self,
        _: &EditorProbabilityDown,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.adjust_selected_probability(-0.05, cx);
    }
    fn on_ratchet_up(&mut self, _: &EditorRatchetUp, _: &mut Window, cx: &mut Context<Self>) {
        self.adjust_selected_step_property(
            StepPropertyDelta::Ratchets(1),
            "Increase step ratchets",
            cx,
        );
    }
    fn on_ratchet_down(&mut self, _: &EditorRatchetDown, _: &mut Window, cx: &mut Context<Self>) {
        self.adjust_selected_step_property(
            StepPropertyDelta::Ratchets(-1),
            "Decrease step ratchets",
            cx,
        );
    }
    fn on_microtiming_later(
        &mut self,
        _: &EditorMicrotimingLater,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.adjust_selected_microtiming((PPQ / 96) as i32, cx);
    }
    fn on_microtiming_earlier(
        &mut self,
        _: &EditorMicrotimingEarlier,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.adjust_selected_microtiming(-(PPQ / 96) as i32, cx);
    }
    fn on_cycle_scale(&mut self, _: &EditorCycleScale, _: &mut Window, cx: &mut Context<Self>) {
        self.cycle_scale(cx);
    }
    fn on_audition(&mut self, _: &EditorAudition, _: &mut Window, cx: &mut Context<Self>) {
        self.audition_selected(cx);
    }
}

impl Focusable for SequencerEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SequencerEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.focus_subscription.is_none() {
            let focus = self.focus_handle.clone();
            self.focus_subscription = Some(cx.on_focus_out(&focus, window, |this, _, _, cx| {
                this.cancel_editor_gesture("Editor gesture safely ended when focus moved", cx);
            }));
        }
        let pattern = self.active_pattern();
        let empty_message = match self.mode {
            EditorMode::PianoRoll => "No note pattern yet. Press + NEW to start a piano roll.",
            EditorMode::Steps => "No step pattern yet. Press + NEW to start a drum grid.",
        };
        div()
            .key_context("AudecSequencer")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::expression_key_down))
            .on_action(cx.listener(Self::on_toggle_mode))
            .on_action(cx.listener(Self::on_undo))
            .on_action(cx.listener(Self::on_redo))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_left))
            .on_action(cx.listener(Self::on_right))
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_resize_left))
            .on_action(cx.listener(Self::on_resize_right))
            .on_action(cx.listener(Self::on_zoom_in))
            .on_action(cx.listener(Self::on_zoom_out))
            .on_action(cx.listener(Self::on_pan_left))
            .on_action(cx.listener(Self::on_pan_right))
            .on_action(cx.listener(Self::on_quantize))
            .on_action(cx.listener(Self::on_duplicate))
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_velocity_up))
            .on_action(cx.listener(Self::on_velocity_down))
            .on_action(cx.listener(Self::on_probability_up))
            .on_action(cx.listener(Self::on_probability_down))
            .on_action(cx.listener(Self::on_ratchet_up))
            .on_action(cx.listener(Self::on_ratchet_down))
            .on_action(cx.listener(Self::on_microtiming_later))
            .on_action(cx.listener(Self::on_microtiming_earlier))
            .on_action(cx.listener(Self::on_cycle_scale))
            .on_action(cx.listener(Self::on_audition))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .child(self.render_toolbar(cx))
            .when_some(pattern, |this, pattern| {
                this.child(self.render_expression(&pattern, cx))
                    .child(self.render_ruler(&pattern))
                    .child(
                        div()
                            .id("sequencer-grid-scroll")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .flex()
                            .items_start()
                            .child(self.render_labels(&pattern, cx))
                            .child(self.render_grid(pattern.clone(), cx)),
                    )
                    .child(self.render_inspector(&pattern, cx))
            })
            .when(self.active_pattern().is_none(), |this| {
                this.child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(rgb(MUTED))
                        .child(empty_message),
                )
            })
    }
}

fn paint_editor_grid(
    bounds: Bounds<Pixels>,
    pattern: &PatternDefinition,
    start_tick: i64,
    ticks_per_pixel: f64,
    top_key: u8,
    quantize: u64,
    tempo_map: Option<&TempoMap>,
    selection: Option<Selection>,
    selected_notes: &BTreeSet<NoteId>,
    selected_steps: &BTreeSet<StepKey>,
    marquee: Option<(f32, f32, f32, f32)>,
    window: &mut Window,
) {
    let width = f32::from(bounds.size.width);
    let visible_end = start_tick.saturating_add((f64::from(width) * ticks_per_pixel).ceil() as i64);
    let grid = quantize.max(1) as i64;
    let first_grid = start_tick.div_euclid(grid).saturating_mul(grid);
    let mut tick = first_grid;
    while tick <= visible_end {
        let x = bounds.origin.x + px(((tick - start_tick) as f64 / ticks_per_pixel) as f32);
        let division = tick.div_euclid(grid);
        let beat = tempo_map.map_or_else(
            || tick.rem_euclid(PPQ) == 0,
            |map| map.musical_position(BeatTime(tick)).tick == 0,
        );
        let color = if beat {
            rgba(0xffffff26)
        } else if division.rem_euclid(2) == 0 {
            rgba(0xffffff13)
        } else {
            rgba(0xffffff0b)
        };
        window.paint_quad(quad(
            Bounds::new(
                point(x, bounds.origin.y),
                gpui::size(px(1.0), bounds.size.height),
            ),
            px(0.0),
            color,
            px(0.0),
            rgba(0x00000000),
            Default::default(),
        ));
        tick = tick.saturating_add(grid);
        if tick == i64::MAX {
            break;
        }
    }

    if let Some(map) = tempo_map {
        for marker in piano_workflow::visible_bar_markers(map, start_tick, visible_end, 128) {
            let x =
                bounds.origin.x + px(((marker.tick - start_tick) as f64 / ticks_per_pixel) as f32);
            window.paint_quad(quad(
                Bounds::new(
                    point(x, bounds.origin.y),
                    gpui::size(px(1.5), bounds.size.height),
                ),
                px(0.0),
                rgba(0x50d8d74d),
                px(0.0),
                rgba(0x00000000),
                Default::default(),
            ));
        }
    }

    match &pattern.content {
        PatternContent::Notes(notes) => {
            let geometry = PianoGeometry {
                width,
                start_tick,
                ticks_per_pixel,
                top_midi_key: top_key,
                row_height: PIANO_ROW_HEIGHT,
                rows: PIANO_ROWS,
            };
            for row in 0..=PIANO_ROWS {
                let y = bounds.origin.y + px(row as f32 * PIANO_ROW_HEIGHT);
                window.paint_quad(quad(
                    Bounds::new(
                        point(bounds.origin.x, y),
                        gpui::size(bounds.size.width, px(1.0)),
                    ),
                    px(0.0),
                    rgba(0xffffff0d),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            for note in notes.notes.values() {
                let rect = note_rect(note, geometry);
                if rect.0 + rect.2 < 0.0
                    || rect.0 > width
                    || rect.1 + rect.3 < 0.0
                    || rect.1 > f32::from(bounds.size.height)
                {
                    continue;
                }
                let selected = selected_notes.contains(&note.id)
                    || (selected_notes.is_empty() && selection == Some(Selection::Note(note.id)));
                let fill = if selected {
                    rgba(0xf6b760f2)
                } else {
                    rgba(0x50d8d7d9)
                };
                window.paint_quad(quad(
                    Bounds::new(
                        point(
                            bounds.origin.x + px(rect.0),
                            bounds.origin.y + px(rect.1 + 1.5),
                        ),
                        gpui::size(px(rect.2.max(3.0)), px((rect.3 - 3.0).max(2.0))),
                    ),
                    px(3.0),
                    fill,
                    px(if selected { 1.5 } else { 0.5 }),
                    rgba(if selected { 0xffe6a8ff } else { 0x8fffffff }),
                    Default::default(),
                ));
                let velocity_width = rect.2.max(3.0) * note.velocity.clamp(0.0, 1.0);
                window.paint_quad(quad(
                    Bounds::new(
                        point(
                            bounds.origin.x + px(rect.0),
                            bounds.origin.y + px(rect.1 + rect.3 - 3.5),
                        ),
                        gpui::size(px(velocity_width), px(2.0)),
                    ),
                    px(1.0),
                    rgba(0xffffffb8),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            if let Some((start_x, start_y, end_x, end_y)) = marquee {
                let left = start_x.min(end_x).clamp(0.0, width);
                let top = start_y.min(end_y).clamp(0.0, f32::from(bounds.size.height));
                let right = start_x.max(end_x).clamp(0.0, width);
                let bottom = start_y.max(end_y).clamp(0.0, f32::from(bounds.size.height));
                window.paint_quad(quad(
                    Bounds::new(
                        point(bounds.origin.x + px(left), bounds.origin.y + px(top)),
                        gpui::size(px((right - left).max(1.0)), px((bottom - top).max(1.0))),
                    ),
                    px(1.0),
                    rgba(0x50d8d72a),
                    px(1.0),
                    rgba(0x50d8d7e0),
                    Default::default(),
                ));
            }
        }
        PatternContent::Steps(steps) => {
            let lanes = lane_ids(steps);
            let geometry = StepGeometry {
                width,
                start_tick,
                ticks_per_pixel,
                resolution: steps.resolution.0,
                row_height: STEP_ROW_HEIGHT,
                rows: lanes.len(),
            };
            for row in 0..=lanes.len() {
                let y = bounds.origin.y + px(row as f32 * STEP_ROW_HEIGHT);
                window.paint_quad(quad(
                    Bounds::new(
                        point(bounds.origin.x, y),
                        gpui::size(bounds.size.width, px(1.0)),
                    ),
                    px(0.0),
                    rgba(0xffffff16),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            for (row, lane_id) in lanes.iter().copied().enumerate() {
                let Some(lane) = steps.lanes.get(&lane_id) else {
                    continue;
                };
                for (index, event) in &lane.steps {
                    let x = geometry.x_for_step(*index);
                    let step_width = (steps.resolution.0 as f64 / ticks_per_pixel) as f32;
                    if x + step_width < 0.0 || x > width {
                        continue;
                    }
                    let selected = selected_steps.contains(&(lane_id, *index))
                        || (selected_steps.is_empty()
                            && selection == Some(Selection::Step(lane_id, *index)));
                    let alpha = (0.34 + event.velocity.clamp(0.0, 1.0) * 0.66) * 255.0;
                    let color = with_alpha(lane_color(lane_id), alpha.round() as u8);
                    window.paint_quad(quad(
                        Bounds::new(
                            point(
                                bounds.origin.x + px(x + 3.0),
                                bounds.origin.y + px(row as f32 * STEP_ROW_HEIGHT + 6.0),
                            ),
                            gpui::size(px((step_width - 6.0).max(5.0)), px(STEP_ROW_HEIGHT - 12.0)),
                        ),
                        px(5.0),
                        rgba(if selected { 0xf6b760ff } else { color }),
                        px(if selected { 2.0 } else { 1.0 }),
                        rgba(0xffffff70),
                        Default::default(),
                    ));
                    if event.ratchets > 1 {
                        for ratchet in 1..event.ratchets {
                            let marker_x = x + step_width * ratchet as f32 / event.ratchets as f32;
                            window.paint_quad(quad(
                                Bounds::new(
                                    point(
                                        bounds.origin.x + px(marker_x),
                                        bounds.origin.y + px(row as f32 * STEP_ROW_HEIGHT + 10.0),
                                    ),
                                    gpui::size(px(1.0), px(STEP_ROW_HEIGHT - 20.0)),
                                ),
                                px(0.0),
                                rgba(0xffffffaa),
                                px(0.0),
                                rgba(0x00000000),
                                Default::default(),
                            ));
                        }
                    }
                }
            }
        }
    }
}

fn hit_note(notes: &NotePattern, geometry: PianoGeometry, x: f32, y: f32) -> Option<&NoteEvent> {
    notes.notes.values().rev().find(|note| {
        let (left, top, width, height) = note_rect(note, geometry);
        x >= left && x <= left + width.max(5.0) && y >= top && y <= top + height
    })
}

fn note_rect(note: &NoteEvent, geometry: PianoGeometry) -> (f32, f32, f32, f32) {
    let x = geometry.x_for_tick(note.start.0);
    let end = note
        .start
        .0
        .saturating_add(note.duration.0.min(i64::MAX as u64) as i64);
    (
        x,
        geometry.y_for_key(note.pitch.midi_key),
        geometry.x_for_tick(end) - x,
        geometry.row_height,
    )
}

fn lane_ids(pattern: &StepPattern) -> Vec<StepLaneId> {
    pattern.lanes.keys().copied().collect()
}

fn inspector_lines(
    pattern: &PatternDefinition,
    selection: Option<Selection>,
    sequencer: Option<&Sequencer>,
) -> Vec<(String, String)> {
    match (&pattern.content, selection) {
        (PatternContent::Notes(notes), Some(Selection::Note(id))) => {
            let Some(note) = notes.notes.get(&id) else {
                return empty_inspector();
            };
            let position = sequencer.map(|seq| seq.tempo_map().musical_position(note.start));
            vec![
                ("event".into(), format!("note #{}", id.get())),
                (
                    "position".into(),
                    position.map(format_position).unwrap_or_else(|| "—".into()),
                ),
                ("start".into(), format!("{} ticks", note.start.0)),
                ("duration".into(), format!("{} ticks", note.duration.0)),
                (
                    "pitch".into(),
                    format!(
                        "{} · {:.2} Hz",
                        note_name(note.pitch.midi_key),
                        midi_hz(note.pitch)
                    ),
                ),
                ("detune".into(), format!("{:+.2} cents", note.pitch.cents)),
                ("velocity".into(), format!("{:.3}", note.velocity)),
                (
                    "instrument".into(),
                    note.instrument
                        .map(|instrument| format!("#{instrument}"))
                        .unwrap_or_else(|| "unrouted".into()),
                ),
                (
                    "probability".into(),
                    format!("{:.1}%", note.probability * 100.0),
                ),
                (
                    "micro offset".into(),
                    format!("{:+} ticks", note.micro_offset),
                ),
                (
                    "pan / channel".into(),
                    format!("{:+.3} / {}", note.pan, note.channel + 1),
                ),
            ]
        }
        (PatternContent::Steps(steps), Some(Selection::Step(lane_id, index))) => {
            let Some(lane) = steps.lanes.get(&lane_id) else {
                return empty_inspector();
            };
            let Some(step) = lane.steps.get(&index) else {
                return empty_inspector();
            };
            let tick = u64::from(index).saturating_mul(steps.resolution.0);
            let position =
                sequencer.map(|seq| seq.tempo_map().musical_position(BeatTime(tick as i64)));
            vec![
                (
                    "event".into(),
                    format!("lane #{} · step {}", lane_id.get(), index + 1),
                ),
                ("lane".into(), lane.name.clone()),
                (
                    "position".into(),
                    position.map(format_position).unwrap_or_else(|| "—".into()),
                ),
                ("start".into(), format!("{tick} ticks")),
                ("gate".into(), format!("{} ticks", step.gate.0)),
                ("velocity".into(), format!("{:.3}", step.velocity)),
                (
                    "probability".into(),
                    format!("{:.1}%", step.probability * 100.0),
                ),
                ("ratchets".into(), step.ratchets.to_string()),
                ("pitch".into(), format!("{:+.2} st", step.pitch_semitones)),
                (
                    "offset / pan".into(),
                    format!("{:+} / {:+.3}", step.micro_offset, step.pan),
                ),
            ]
        }
        _ => empty_inspector(),
    }
}

fn empty_inspector() -> Vec<(String, String)> {
    vec![
        ("selection".into(), "none".into()),
        ("create".into(), "left-click empty grid".into()),
        ("edit".into(), "drag · arrows · Shift+←/→".into()),
        ("remove".into(), "right-click · Delete".into()),
    ]
}

fn format_position(position: crate::sequencer::MusicalPosition) -> String {
    format!(
        "{} · {} · {}",
        position.bar + 1,
        position.beat + 1,
        position.tick
    )
}

fn snap_tick(tick: i64, grid: u64) -> i64 {
    let grid = grid.max(1).min(i64::MAX as u64) as i64;
    let lower = tick.div_euclid(grid);
    let remainder = tick.rem_euclid(grid);
    lower
        .saturating_add(i64::from(remainder.saturating_mul(2) >= grid))
        .saturating_mul(grid)
}

fn midi_hz(pitch: NotePitch) -> f64 {
    440.0 * 2.0_f64.powf((f64::from(pitch.midi_key) - 69.0 + f64::from(pitch.cents) / 100.0) / 12.0)
}

fn note_name(key: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B",
    ];
    format!("{}{}", NAMES[key as usize % 12], i16::from(key) / 12 - 1)
}

fn is_black_key(key: u8) -> bool {
    matches!(key % 12, 1 | 3 | 6 | 8 | 10)
}

fn lane_color(id: StepLaneId) -> u32 {
    [
        CYAN, MAGENTA, AMBER, LIME, 0x8e9cff, 0xe99172, 0x78d5a3, 0xd8a7ff,
    ][id.get() as usize % 8]
}

fn target_label(target: &TriggerTarget) -> String {
    match target {
        TriggerTarget::InstrumentNote { instrument, key } => format!("inst{instrument}:{key}"),
        TriggerTarget::DrumPad { rack, pad } => format!("rack{rack}:{pad}"),
        TriggerTarget::Sample(asset) => format!("sample{}", asset.get()),
        TriggerTarget::AnalysisTemplate(template) => format!("family{template}"),
    }
}

fn with_alpha(rgb: u32, alpha: u8) -> u32 {
    (rgb << 8) | u32::from(alpha)
}

fn grid_name(ticks: u64) -> &'static str {
    match ticks {
        value if value == PPQ as u64 => "1/4",
        value if value == (PPQ / 2) as u64 => "1/8",
        value if value == (PPQ / 4) as u64 => "1/16",
        value if value == (PPQ / 8) as u64 => "1/32",
        _ => "custom",
    }
}

fn toggle_button(id: &'static str, label: &'static str, active: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(rgb(if active { CYAN } else { BORDER }))
        .bg(rgba(if active { 0x50d8d71c } else { 0x00000000 }))
        .text_xs()
        .text_color(rgb(if active { CYAN } else { MUTED }))
        .cursor_pointer()
        .hover(|style| style.bg(rgba(0xffffff0d)))
        .child(label)
}

fn control_button(id: &'static str, label: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL_ALT))
        .text_xs()
        .text_color(rgb(MUTED))
        .cursor_pointer()
        .hover(|style| style.border_color(rgb(CYAN)).text_color(rgb(TEXT)))
        .child(label.into())
}

fn audition_button(
    id: &'static str,
    label: &'static str,
    available: bool,
) -> gpui::Stateful<gpui::Div> {
    if available {
        return control_button(id, label);
    }
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL_ALT))
        .text_xs()
        .text_color(rgb(DIM))
        .child(format!("{label} · OFF"))
}

fn readout(label: impl Into<SharedString>) -> impl IntoElement {
    div()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(rgb(PANEL_ALT))
        .text_xs()
        .text_color(rgb(AMBER))
        .child(label.into())
}

fn demo_source() -> SequencerEditorSource {
    let mut sequencer = Sequencer::new(TempoMap::common_time(48_000, 122.0).unwrap());
    let notes_id = sequencer.allocate_pattern_id();
    let steps_id = sequencer.allocate_pattern_id();
    let length = BeatDuration((PPQ * 4 * 8) as u64);

    let mut notes = BTreeMap::new();
    for (start, duration, key, velocity) in [
        (0, 720, 48, 0.78),
        (960, 480, 55, 0.68),
        (1_920, 960, 60, 0.88),
        (3_120, 600, 63, 0.72),
        (3_840, 1_440, 67, 0.91),
        (5_520, 480, 70, 0.64),
        (6_240, 960, 72, 0.84),
        (7_440, 1_200, 67, 0.76),
    ] {
        let id = sequencer.allocate_note_id();
        notes.insert(
            id,
            NoteEvent {
                id,
                start: BeatTime(start),
                duration: BeatDuration(duration),
                pitch: NotePitch {
                    midi_key: key,
                    cents: 0.0,
                },
                velocity,
                release_velocity: 0.5,
                pan: 0.0,
                probability: 1.0,
                micro_offset: 0,
                channel: 0,
                instrument: Some(1),
                articulation: Articulation::Normal,
                expression: PerNoteExpression::default(),
            },
        );
    }
    let notes_pattern = PatternDefinition {
        id: notes_id,
        name: "Decompiled synth phrase".into(),
        length,
        content: PatternContent::Notes(NotePattern { notes }),
        origin: PatternOrigin::Authored,
        revision: 0,
    };

    let mut lanes = BTreeMap::new();
    for (name, pad, hits) in [
        ("Kick", 0, vec![0, 4, 8, 12]),
        ("Snare", 1, vec![4, 12]),
        ("Closed hat", 2, vec![0, 2, 4, 6, 8, 10, 12, 14]),
        ("Open hat", 3, vec![7, 15]),
        ("Percussion A", 4, vec![3, 6, 11]),
        ("Percussion B", 5, vec![5, 13]),
    ] {
        let id = sequencer.allocate_step_lane_id();
        let mut events = BTreeMap::new();
        for index in hits {
            events.insert(
                index,
                StepEvent {
                    velocity: if index % 4 == 0 { 0.92 } else { 0.68 },
                    probability: 1.0,
                    micro_offset: 0,
                    gate: BeatDuration((PPQ / 4) as u64),
                    ratchets: if name == "Percussion A" && index == 11 {
                        3
                    } else {
                        1
                    },
                    pitch_semitones: 0.0,
                    pan: 0.0,
                },
            );
        }
        lanes.insert(
            id,
            StepLane {
                id,
                name: name.into(),
                target: TriggerTarget::DrumPad { rack: 1, pad },
                choke_group: name.contains("hat").then_some(1),
                steps: events,
            },
        );
    }
    let steps_pattern = PatternDefinition {
        id: steps_id,
        name: "Deprojected transient families".into(),
        length,
        content: PatternContent::Steps(StepPattern {
            resolution: BeatDuration((PPQ / 4) as u64),
            swing: 0.25,
            lanes,
        }),
        origin: PatternOrigin::Authored,
        revision: 0,
    };
    sequencer
        .execute(
            "Create sequencer demo",
            vec![
                SequencerCommand::PutPattern {
                    before: None,
                    after: Some(notes_pattern),
                },
                SequencerCommand::PutPattern {
                    before: None,
                    after: Some(steps_pattern),
                },
            ],
        )
        .unwrap();
    SequencerEditorSource::new(
        Arc::new(Mutex::new(sequencer)),
        Some(notes_id),
        Some(steps_id),
        "SEQUENCER · reconstructed material",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audition_availability_defaults_to_explicit_shared_renderer_refusal() {
        let availability = SequencerAuditionAvailability::default();
        assert!(!availability.is_available());
        assert_eq!(
            availability.unavailable_reason(),
            Some("Shared pattern audition is not connected")
        );
        assert!(SequencerAuditionAvailability::Available.is_available());
    }

    fn pending_failure_state() -> (
        BTreeSet<PatternWorkflowRequestId>,
        Option<String>,
        Option<u8>,
        Option<u8>,
        Option<u8>,
    ) {
        (
            BTreeSet::from([PatternWorkflowRequestId::from_raw(1)]),
            Some("Waiting for project session".into()),
            Some(1),
            Some(2),
            Some(3),
        )
    }

    #[test]
    fn external_workflow_failure_consumes_request_and_clears_transient_state() {
        let (mut pending, mut status, mut optimistic, mut gesture, mut drag) =
            pending_failure_state();

        assert!(complete_external_workflow_failure(
            &mut pending,
            PatternWorkflowRequestId::from_raw(1),
            &mut status,
            &mut optimistic,
            &mut gesture,
            &mut drag,
            "Project session rejected the edit".into(),
        ));
        assert!(pending.is_empty());
        assert_eq!(status.as_deref(), Some("Project session rejected the edit"));
        assert_eq!((optimistic, gesture, drag), (None, None, None));
    }

    #[test]
    fn duplicate_external_workflow_failure_is_inert() {
        let (mut pending, mut status, mut optimistic, mut gesture, mut drag) =
            pending_failure_state();

        assert!(complete_external_workflow_failure(
            &mut pending,
            PatternWorkflowRequestId::from_raw(1),
            &mut status,
            &mut optimistic,
            &mut gesture,
            &mut drag,
            "First failure".into(),
        ));
        assert!(!complete_external_workflow_failure(
            &mut pending,
            PatternWorkflowRequestId::from_raw(1),
            &mut status,
            &mut optimistic,
            &mut gesture,
            &mut drag,
            "Duplicate failure".into(),
        ));
        assert_eq!(status.as_deref(), Some("First failure"));
        assert_eq!((optimistic, gesture, drag), (None, None, None));
    }

    #[test]
    fn stale_external_workflow_failure_cannot_mutate_pending_request() {
        let (mut pending, mut status, mut optimistic, mut gesture, mut drag) =
            pending_failure_state();

        assert!(!complete_external_workflow_failure(
            &mut pending,
            PatternWorkflowRequestId::from_raw(99),
            &mut status,
            &mut optimistic,
            &mut gesture,
            &mut drag,
            "Stale failure".into(),
        ));
        assert_eq!(pending.len(), 1);
        assert!(pending.contains(&PatternWorkflowRequestId::from_raw(1)));
        assert_eq!(status.as_deref(), Some("Waiting for project session"));
        assert_eq!((optimistic, gesture, drag), (Some(1), Some(2), Some(3)));
    }

    #[test]
    fn piano_time_mapping_round_trips_inside_a_pixel() {
        let geometry = PianoGeometry {
            width: 1_000.0,
            start_tick: 7_680,
            ticks_per_pixel: 2.75,
            top_midi_key: 84,
            row_height: 22.0,
            rows: 24,
        };
        for tick in [7_680, 8_000, 9_600, 10_211] {
            let reconstructed = geometry.tick_at_x(geometry.x_for_tick(tick));
            assert!((tick - reconstructed).abs() <= 1);
        }
    }

    #[test]
    fn pitch_rows_are_exact_and_clamped() {
        let geometry = PianoGeometry {
            width: 800.0,
            start_tick: 0,
            ticks_per_pixel: 4.0,
            top_midi_key: 83,
            row_height: 22.0,
            rows: 24,
        };
        assert_eq!(geometry.key_at_y(0.0), 83);
        assert_eq!(geometry.key_at_y(22.1), 82);
        assert_eq!(geometry.y_for_key(72), 242.0);
        assert_eq!(geometry.key_at_y(99_999.0), 0);
    }

    #[test]
    fn snapping_uses_nearest_grid_with_stable_half_tie() {
        assert_eq!(snap_tick(119, 240), 0);
        assert_eq!(snap_tick(120, 240), 240);
        assert_eq!(snap_tick(359, 240), 240);
        assert_eq!(snap_tick(360, 240), 480);
        assert_eq!(snap_tick(-120, 240), 0);
    }

    #[test]
    fn step_mapping_respects_viewport_offset() {
        let geometry = StepGeometry {
            width: 640.0,
            start_tick: 1_920,
            ticks_per_pixel: 3.0,
            resolution: 240,
            row_height: 44.0,
            rows: 6,
        };
        assert_eq!(geometry.x_for_step(8), 0.0);
        assert_eq!(geometry.step_at_x(80.0), 9);
        assert_eq!(geometry.lane_at_y(87.9), Some(1));
        assert_eq!(geometry.lane_at_y(264.0), None);
    }

    #[test]
    fn note_hit_testing_honors_time_and_pitch() {
        let mut notes = NotePattern::default();
        let id = NoteId::from_raw(9);
        notes.notes.insert(
            id,
            NoteEvent {
                id,
                start: BeatTime(960),
                duration: BeatDuration(480),
                pitch: NotePitch {
                    midi_key: 72,
                    cents: 0.0,
                },
                velocity: 1.0,
                release_velocity: 0.5,
                pan: 0.0,
                probability: 1.0,
                micro_offset: 0,
                channel: 0,
                instrument: None,
                articulation: Articulation::Normal,
                expression: PerNoteExpression::default(),
            },
        );
        let geometry = PianoGeometry {
            width: 1_000.0,
            start_tick: 0,
            ticks_per_pixel: 4.0,
            top_midi_key: 83,
            row_height: 22.0,
            rows: 24,
        };
        assert_eq!(
            hit_note(&notes, geometry, 250.0, 250.0).map(|note| note.id),
            Some(id)
        );
        assert!(hit_note(&notes, geometry, 100.0, 250.0).is_none());
    }

    #[test]
    fn midi_frequency_includes_cent_detune() {
        assert!(
            (midi_hz(NotePitch {
                midi_key: 69,
                cents: 0.0
            }) - 440.0)
                .abs()
                < 1.0e-9
        );
        assert!(
            (midi_hz(NotePitch {
                midi_key: 69,
                cents: 100.0
            }) - 466.163_761_5)
                .abs()
                < 1.0e-6
        );
    }
}
