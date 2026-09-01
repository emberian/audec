//! GPUI piano-roll and step-sequencer editor backed by [`crate::sequencer`].
//!
//! Project-backed instances render a short snapshot and may hold one optimistic
//! preview, but authored mutation crosses [`PatternActionCallback`]. Standalone
//! instances lower the same gestures to validated `SequencerCommand`s, so undo,
//! scheduling, and persistence still observe one revision stream.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gpui::{
    actions, canvas, div, point, prelude::*, px, quad, rgb, rgba, App, Bounds, Context,
    FocusHandle, Focusable, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Render, ScrollWheelEvent, SharedString, Window,
};

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
    BeginPatternGestureIntent, PatternCyclePublication, PatternEditPublication,
    PatternEditorHydration, PatternGestureKind, PatternGestureReceipt, PatternLoopAuditionIntent,
    PatternLoopAuditionPlan, PatternWorkflowCallback, PatternWorkflowDispatchReceipt,
    PatternWorkflowError, PatternWorkflowIntent, PatternWorkflowOutcome, PatternWorkflowRequest,
    PatternWorkflowRequestId,
};
use crate::sample_kit::SampleTargetRef;
use crate::sequencer::{
    quantize_notes, Articulation, BeatDuration, BeatTime, NoteEvent, NoteId, NotePattern,
    NotePitch, PatternContent, PatternDefinition, PatternId, PatternOrigin, PerNoteExpression,
    QuantizeSpec, SampleAssetId, Sequencer, SequencerCommand, StepEvent, StepLane, StepLaneId,
    StepPattern, TempoMap, TriggerTarget, PPQ,
};

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
    ]);
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
    MoveNote {
        id: NoteId,
        origin_x: f32,
        origin_y: f32,
        original: NoteEvent,
    },
    ResizeNote {
        id: NoteId,
        origin_x: f32,
        original: NoteEvent,
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
    drag: Option<DragGesture>,
    grid_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    start_tick: i64,
    ticks_per_pixel: f64,
    top_midi_key: u8,
    quantize_grid: u64,
    swing: f32,
    expression: String,
    expression_focused: bool,
    expression_bindings: BTreeMap<String, TriggerTarget>,
    preview_cycle: u64,
    preview_seed: u64,
    status: Option<String>,
    focus_handle: FocusHandle,
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
            drag: None,
            grid_bounds: Arc::new(Mutex::new(None)),
            start_tick: 0,
            ticks_per_pixel: 24.0,
            top_midi_key: 83,
            quantize_grid: (PPQ / 4) as u64,
            swing,
            expression,
            expression_focused: false,
            expression_bindings,
            preview_cycle: 0,
            preview_seed: 0,
            status: None,
            focus_handle: cx.focus_handle(),
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
        self.drag = None;
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
        self.drag = None;
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
        if self.pattern_id_for(mode).is_some() {
            self.mode = mode;
            self.selection = None;
            self.drag = None;
            self.optimistic_pattern = None;
            self.preview_cycle = 0;
            self.reload_authoring_state();
            cx.notify();
        }
    }

    fn request_mode(&mut self, mode: EditorMode, cx: &mut Context<Self>) {
        let Some(pattern) = self.pattern_id_for(mode) else {
            self.status = Some("No pattern is retained for that editor mode".into());
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
        let stored = self.stored_active_pattern()?;
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
            .and_then(|sequencer| {
                sequencer
                    .patterns()
                    .patterns()
                    .filter_map(|pattern| match &pattern.content {
                        PatternContent::Notes(notes) => notes.notes.keys().map(|id| id.get()).max(),
                        PatternContent::Steps(_) => None,
                    })
                    .max()
            })
            .unwrap_or(0)
            .saturating_add(1);
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
        self.dispatch_workflow(
            PatternWorkflowIntent::Audition(PatternLoopAuditionIntent {
                expected_project_revision: self.expected_project_revision,
                occurrence,
                cycle_index: self.preview_cycle,
                performance_seed: self.preview_seed,
            }),
            format!(
                "Preparing placement cycle {} audition",
                self.preview_cycle + 1
            ),
            cx,
        );
    }

    fn cycle_preview(&mut self, direction: i64, cx: &mut Context<Self>) {
        self.preview_cycle = if direction < 0 {
            self.preview_cycle.saturating_sub(direction.unsigned_abs())
        } else {
            self.preview_cycle.saturating_add(direction as u64)
        };
        self.selection = None;
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
                self.status = Some(error.to_string());
                self.optimistic_pattern = None;
                self.active_gesture = None;
                cx.notify();
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
                self.drag = None;
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
        self.emit(
            PatternAction::Edit(PatternEditIntent {
                pattern: before.id,
                expected_pattern_revision: before.revision,
                edit: PatternEdit::AddLane {
                    name: format!("Lane {lane_number}"),
                    target,
                    choke_group: None,
                },
            }),
            "Adding pattern lane",
            cx,
        );
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
        self.emit(
            PatternAction::Edit(PatternEditIntent {
                pattern: before.id,
                expected_pattern_revision: before.revision,
                edit: PatternEdit::RemoveLane { lane },
            }),
            "Removing pattern lane",
            cx,
        );
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

    fn quantize(&mut self, cx: &mut Context<Self>) {
        let Some(before) = self.active_pattern() else {
            return;
        };
        let PatternContent::Notes(notes) = &before.content else {
            self.status = Some("Step events are already locked to their pattern grid".into());
            cx.notify();
            return;
        };
        match quantize_notes(
            notes,
            QuantizeSpec {
                grid: BeatDuration(self.quantize_grid),
                strength: 1.0,
            },
        ) {
            Ok(notes) => {
                let mut after = before.clone();
                after.content = PatternContent::Notes(notes);
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
                let geometry = self.piano_geometry(width);
                if let Some(note) = hit_note(notes, geometry, x, y) {
                    self.selection = Some(Selection::Note(note.id));
                    let end_x = geometry.x_for_tick(
                        note.start
                            .0
                            .saturating_add(note.duration.0.min(i64::MAX as u64) as i64),
                    );
                    let resizing = (end_x - x).abs() <= 8.0;
                    self.drag = Some(if resizing {
                        DragGesture::ResizeNote {
                            id: note.id,
                            origin_x: x,
                            original: note.clone(),
                        }
                    } else {
                        DragGesture::MoveNote {
                            id: note.id,
                            origin_x: x,
                            origin_y: y,
                            original: note.clone(),
                        }
                    });
                    self.begin_pattern_gesture(
                        before.id,
                        if resizing {
                            PatternGestureKind::ResizeNote
                        } else {
                            PatternGestureKind::MoveNote
                        },
                        cx,
                    );
                } else {
                    self.add_note(before, geometry, x, y, cx);
                    return;
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
                    self.selection = Some(Selection::Step(lane, index));
                    self.drag = Some(DragGesture::MoveStep {
                        lane,
                        index,
                        event: step.clone(),
                    });
                    self.begin_pattern_gesture(before.id, PatternGestureKind::MoveStep, cx);
                } else {
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
        self.delete_selection(cx);
    }

    fn add_note(
        &mut self,
        before: PatternDefinition,
        geometry: PianoGeometry,
        x: f32,
        y: f32,
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
        let start = geometry.snapped_tick_at_x(x, self.quantize_grid);
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
                midi_key: geometry.key_at_y(y),
                cents: 0.0,
            },
            velocity: 0.82,
            release_velocity: 0.5,
            pan: 0.0,
            probability: 1.0,
            micro_offset: swing_ticks,
            channel: 0,
            articulation: Articulation::Normal,
            expression: PerNoteExpression::default(),
        };
        let mut after = before.clone();
        if let PatternContent::Notes(notes) = &mut after.content {
            notes.notes.insert(id, note.clone());
        }
        self.selection = Some(Selection::Note(id));
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
        let mut after = before.clone();
        match gesture {
            DragGesture::MoveNote {
                id,
                origin_x,
                origin_y,
                original,
            } => {
                let geometry = self.piano_geometry(width);
                let tick_delta = snap_tick(
                    (f64::from(x - origin_x) * self.ticks_per_pixel).round() as i64,
                    self.quantize_grid,
                );
                let pitch_delta = ((origin_y - y) / PIANO_ROW_HEIGHT).round() as i16;
                let max_start = before
                    .length
                    .0
                    .saturating_sub(original.duration.0)
                    .min(i64::MAX as u64) as i64;
                let PatternContent::Notes(notes) = &mut after.content else {
                    return;
                };
                let Some(note) = notes.notes.get_mut(&id) else {
                    return;
                };
                note.start.0 = original
                    .start
                    .0
                    .saturating_add(tick_delta)
                    .clamp(0, max_start);
                note.pitch.midi_key =
                    (i16::from(original.pitch.midi_key) + pitch_delta).clamp(0, 127) as u8;
                let note = note.clone();
                let _ = geometry;
                self.execute_semantic_edit(
                    "Move note",
                    before,
                    after,
                    PatternEdit::PutNote { note },
                    cx,
                );
            }
            DragGesture::ResizeNote {
                id,
                origin_x,
                original,
            } => {
                let delta = snap_tick(
                    (f64::from(x - origin_x) * self.ticks_per_pixel).round() as i64,
                    self.quantize_grid,
                );
                let max_duration = before.length.0.saturating_sub(original.start.0 as u64);
                let duration = (original.duration.0 as i128 + delta as i128)
                    .clamp(self.quantize_grid as i128, max_duration as i128)
                    as u64;
                let PatternContent::Notes(notes) = &mut after.content else {
                    return;
                };
                let Some(note) = notes.notes.get_mut(&id) else {
                    return;
                };
                note.duration.0 = duration;
                let note = note.clone();
                self.execute_semantic_edit(
                    "Resize note",
                    before,
                    after,
                    PatternEdit::PutNote { note },
                    cx,
                );
            }
            DragGesture::MoveStep { lane, index, event } => {
                let PatternContent::Steps(steps) = &mut after.content else {
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
                let in_range = u128::from(next_index) * u128::from(steps.resolution.0)
                    < u128::from(before.length.0);
                if !in_range {
                    return;
                }
                if steps
                    .lanes
                    .get(&next_lane)
                    .is_some_and(|lane| lane.steps.contains_key(&next_index))
                {
                    return;
                }
                if let Some(value) = steps.lanes.get_mut(&lane) {
                    value.steps.remove(&index);
                }
                if let Some(value) = steps.lanes.get_mut(&next_lane) {
                    value.steps.insert(next_index, event.clone());
                }
                self.selection = Some(Selection::Step(next_lane, next_index));
                self.drag = Some(DragGesture::MoveStep {
                    lane: next_lane,
                    index: next_index,
                    event,
                });
                self.execute_semantic_edit(
                    "Move step",
                    before,
                    after,
                    PatternEdit::MoveStep {
                        from_lane: lane,
                        from_step: index,
                        to_lane: next_lane,
                        to_step: next_index,
                    },
                    cx,
                );
            }
        }
    }

    fn end_pointer(&mut self, _: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
            self.end_pattern_gesture(cx);
            cx.notify();
        }
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
                control_button("seq-cycle-audition", "AUDITION")
                    .on_click(cx.listener(|this, _, _, cx| this.audition_cycle(cx))),
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
        let meter = self
            .source
            .sequencer
            .lock()
            .ok()
            .map(|sequencer| sequencer.tempo_map().meter_at(BeatTime(self.start_tick)))
            .unwrap_or(crate::sequencer::TimeSignature {
                numerator: 4,
                denominator: 4,
            });
        let bar_ticks = meter.ticks_per_bar().max(1);
        let start_bar = self.start_tick.div_euclid(bar_ticks);
        let bar_width = bar_ticks as f64 / self.ticks_per_pixel;
        let count = ((1000.0 / bar_width).ceil() as usize + 2).clamp(2, 32);
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
                    .child(format!("{} bars", pattern.length.0 / bar_ticks as u64)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .relative()
                    .overflow_hidden()
                    .children((0..count).map(|index| {
                        let bar = start_bar + index as i64;
                        let x = ((bar * bar_ticks - self.start_tick) as f64 / self.ticks_per_pixel)
                            as f32;
                        div()
                            .absolute()
                            .left(px(x + 5.0))
                            .top(px(7.0))
                            .text_xs()
                            .text_color(rgb(if index == 0 { CYAN } else { MUTED }))
                            .child(format!("{}", bar + 1))
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
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.cycle_lane_target(lane_id, cx)),
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
                                .child(
                                    div()
                                        .text_color(rgb(DIM))
                                        .child(self.trigger_target_label(&lane.target)),
                                ),
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
        let bar_ticks = self
            .source
            .sequencer
            .lock()
            .ok()
            .map(|sequencer| {
                sequencer
                    .tempo_map()
                    .meter_at(BeatTime(start_tick))
                    .ticks_per_bar()
            })
            .unwrap_or(PPQ * 4)
            .max(1);
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
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
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
                            bar_ticks,
                            selection,
                            window,
                        );
                    },
                )
                .size_full(),
            )
    }

    fn render_inspector(&self, pattern: &PatternDefinition) -> gpui::AnyElement {
        let lines = inspector_lines(
            pattern,
            self.selection,
            self.source.sequencer.lock().ok().as_deref(),
        );
        div()
            .h(px(118.0))
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
}

impl Focusable for SequencerEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SequencerEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pattern = self.active_pattern();
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
                    .child(self.render_inspector(&pattern))
            })
            .when(self.active_pattern().is_none(), |this| {
                this.child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(rgb(MUTED))
                        .child("No compatible pattern is connected to this editor."),
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
    bar_ticks: i64,
    selection: Option<Selection>,
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
        let bar = tick.rem_euclid(bar_ticks) == 0;
        let beat = tick.rem_euclid(PPQ) == 0;
        let color = if bar {
            rgba(0x50d8d74d)
        } else if beat {
            rgba(0xffffff26)
        } else if division.rem_euclid(2) == 0 {
            rgba(0xffffff13)
        } else {
            rgba(0xffffff0b)
        };
        window.paint_quad(quad(
            Bounds::new(
                point(x, bounds.origin.y),
                gpui::size(px(if bar { 1.5 } else { 1.0 }), bounds.size.height),
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
                let selected = selection == Some(Selection::Note(note.id));
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
                    let selected = selection == Some(Selection::Step(lane_id, *index));
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
