//! GPUI piano-roll and step-sequencer editor backed by [`crate::sequencer`].
//!
//! The view deliberately owns no shadow copy of musical data: every edit is a
//! validated `SequencerCommand`, so undo, scheduling, persistence adapters, and
//! future ProjectDocument bridges all observe the same revision stream.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use gpui::{
    actions, canvas, div, point, prelude::*, px, quad, rgb, rgba, App, Bounds, Context,
    FocusHandle, Focusable, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Render, ScrollWheelEvent, SharedString, Window,
};

use crate::pattern_authoring::{self, DivergedOverwrite};
use crate::sequencer::{
    quantize_notes, Articulation, BeatDuration, BeatTime, NoteEvent, NoteId, NotePattern,
    NotePitch, PatternContent, PatternDefinition, PatternId, PatternOrigin, PerNoteExpression,
    QuantizeSpec, Sequencer, SequencerCommand, StepEvent, StepLane, StepLaneId, StepPattern,
    TempoMap, TriggerTarget, PPQ,
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

/// Injection boundary used by a pane, floating window, or ProjectDocument
/// adapter. Either pattern may be omitted; mode switching then stays on the
/// available editor.
#[derive(Clone)]
pub struct SequencerEditorSource {
    pub sequencer: Arc<Mutex<Sequencer>>,
    pub note_pattern: Option<PatternId>,
    pub step_pattern: Option<PatternId>,
    pub title: SharedString,
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
        Self {
            source,
            mode,
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

    /// Host-facing retargeting surface. Updating a binding is deliberately a
    /// draft operation; Enter in the expression field applies it atomically.
    pub fn retarget_expression_binding(
        &mut self,
        name: impl Into<String>,
        target: TriggerTarget,
        cx: &mut Context<Self>,
    ) {
        self.expression_bindings.insert(name.into(), target);
        self.status = Some("Binding retargeted; press Enter to regenerate".into());
        cx.notify();
    }

    pub fn expression_bindings(&self) -> &BTreeMap<String, TriggerTarget> {
        &self.expression_bindings
    }

    fn cycle_expression_binding(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(pattern) = self.active_pattern() else {
            return;
        };
        let PatternContent::Steps(steps) = pattern.content else {
            return;
        };
        let mut targets = self
            .expression_bindings
            .values()
            .cloned()
            .chain(steps.lanes.values().map(|lane| lane.target.clone()))
            .collect::<Vec<_>>();
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
            cx.notify();
        }
    }

    fn pattern_id_for(&self, mode: EditorMode) -> Option<PatternId> {
        match mode {
            EditorMode::PianoRoll => self.source.note_pattern,
            EditorMode::Steps => self.source.step_pattern,
        }
    }

    fn active_pattern(&self) -> Option<PatternDefinition> {
        let id = self.pattern_id_for(self.mode)?;
        self.source
            .sequencer
            .lock()
            .ok()?
            .patterns()
            .get(id)
            .cloned()
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
                            before: Some(before),
                            after: Some(after),
                        }],
                    )
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            });
        self.status = result.err();
        cx.notify();
    }

    fn apply_expression(&mut self, overwrite: DivergedOverwrite, cx: &mut Context<Self>) {
        let Some(before) = self.source.step_pattern.and_then(|id| {
            self.source
                .sequencer
                .lock()
                .ok()?
                .patterns()
                .get(id)
                .cloned()
        }) else {
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
                self.execute_pattern(
                    "Apply pattern expression",
                    before,
                    application.definition,
                    cx,
                );
                self.status = if diagnostics.is_empty() {
                    Some("Expression applied; loop placements vary by cycle".into())
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
                self.status = Some("Draft changed; press Enter to apply".into());
            }
            "left" | "right" | "up" | "down" | "tab" => {}
            _ if !event.keystroke.modifiers.platform && !event.keystroke.modifiers.control => {
                if let Some(text) = event.keystroke.key_char.as_deref() {
                    if text.chars().all(|character| !character.is_control()) {
                        self.expression.push_str(text);
                        self.status = Some("Draft changed; press Enter to apply".into());
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
        self.set_mode(next, cx);
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
                let mut after = before.clone();
                after.content = PatternContent::Steps(steps);
                self.execute_pattern("Set pattern swing", before, after, cx);
                return;
            }
        }
        cx.notify();
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
        let removed = match (&mut after.content, selection) {
            (PatternContent::Notes(notes), Selection::Note(id)) => {
                notes.notes.remove(&id).is_some()
            }
            (PatternContent::Steps(steps), Selection::Step(lane, index)) => steps
                .lanes
                .get_mut(&lane)
                .and_then(|lane| lane.steps.remove(&index))
                .is_some(),
            _ => false,
        };
        if removed {
            self.selection = None;
            self.execute_pattern("Delete sequencer event", before, after, cx);
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
                    self.drag = Some(if (end_x - x).abs() <= 8.0 {
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
        let id = match self.source.sequencer.lock() {
            Ok(mut sequencer) => sequencer.allocate_note_id(),
            Err(_) => return,
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
            notes.notes.insert(id, note);
        }
        self.selection = Some(Selection::Note(id));
        self.execute_pattern("Add note", before, after, cx);
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
            value.steps.insert(index, event);
        }
        self.selection = Some(Selection::Step(lane, index));
        self.execute_pattern("Add step", before, after, cx);
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
                if let PatternContent::Notes(notes) = &mut after.content {
                    if let Some(note) = notes.notes.get_mut(&id) {
                        note.start.0 = original
                            .start
                            .0
                            .saturating_add(tick_delta)
                            .clamp(0, max_start);
                        note.pitch.midi_key =
                            (i16::from(original.pitch.midi_key) + pitch_delta).clamp(0, 127) as u8;
                    }
                }
                let _ = geometry;
                self.execute_pattern("Move note", before, after, cx);
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
                if let PatternContent::Notes(notes) = &mut after.content {
                    if let Some(note) = notes.notes.get_mut(&id) {
                        note.duration.0 = duration;
                    }
                }
                self.execute_pattern("Resize note", before, after, cx);
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
                self.execute_pattern("Move step", before, after, cx);
            }
        }
    }

    fn end_pointer(&mut self, _: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
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
        let changed = match (&mut after.content, selection) {
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
                true
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
                }
                true
            }
            _ => false,
        };
        if changed {
            self.execute_pattern("Edit sequencer event", before, after, cx);
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
                toggle_button(
                    "seq-mode-notes",
                    "PIANO ROLL",
                    self.mode == EditorMode::PianoRoll,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_mode(EditorMode::PianoRoll, cx))),
            )
            .child(
                toggle_button(
                    "seq-mode-steps",
                    "DRUM / STEPS",
                    self.mode == EditorMode::Steps,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_mode(EditorMode::Steps, cx))),
            )
            .child(div().flex_1())
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
                                .child(format!("{name}→{}", target_label(target)))
                        }),
                    ))
                    .child(
                        div()
                            .text_color(rgb(if diverged { MAGENTA } else { DIM }))
                            .child(if diverged {
                                "⌘Enter replaces manual edits"
                            } else {
                                "Enter applies · click a binding to retarget"
                            }),
                    ),
            )
            .into_any_element()
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

    fn render_labels(&self, pattern: &PatternDefinition) -> gpui::AnyElement {
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
                    div()
                        .h(px(STEP_ROW_HEIGHT))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .border_b_1()
                        .border_color(rgba(0xffffff10))
                        .child(
                            div()
                                .size(px(7.0))
                                .rounded_full()
                                .bg(rgb(lane_color(lane.id))),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .text_xs()
                                .text_color(rgb(TEXT))
                                .child(lane.name.clone()),
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
                            .child(self.render_labels(&pattern))
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
