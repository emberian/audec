//! A self-contained GPUI arrangement editor for audec's sample-accurate core.
//!
//! The view intentionally owns no audio renderer. It edits [`ArrangementEditor`]
//! project truth, presents exact source mappings, and labels playback transform
//! fields as metadata until the render compiler consumes them.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use gpui::{
    actions, canvas, div, point, prelude::*, px, relative, rgb, rgba, App, Bounds, Context,
    FocusHandle, Focusable, IntoElement, KeyBinding, MouseButton, MouseDownEvent, Pixels, Render,
    ScrollWheelEvent, Window,
};

use crate::arrangement::{
    ArrangementEditor, AssetId, Clip, ClipContent, ClipId, Frame, FrameRange, ParameterId,
    PatternId, Track, TrackKind,
};

actions!(
    audec_arrangement,
    [
        UndoArrangement,
        RedoArrangement,
        DuplicateClip,
        DeleteClip,
        SplitClip,
        NudgeClipLeft,
        NudgeClipRight,
        TrimClipStart,
        TrimClipEnd,
        ZoomArrangementIn,
        ZoomArrangementOut,
        PanArrangementLeft,
        PanArrangementRight,
        FitArrangement,
        CycleArrangementSnap,
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

/// A project-owned arrangement editor that can be used by multiple views or
/// controllers. `ArrangementView` takes short snapshots from this handle and
/// never retains its mutex while constructing GPUI elements.
pub type SharedArrangementEditor = Arc<Mutex<ArrangementEditor>>;

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
        KeyBinding::new("left", NudgeClipLeft, Some("AudecArrangement")),
        KeyBinding::new("right", NudgeClipRight, Some("AudecArrangement")),
        KeyBinding::new("[", TrimClipStart, Some("AudecArrangement")),
        KeyBinding::new("]", TrimClipEnd, Some("AudecArrangement")),
        KeyBinding::new("=", ZoomArrangementIn, Some("AudecArrangement")),
        KeyBinding::new("-", ZoomArrangementOut, Some("AudecArrangement")),
        KeyBinding::new("shift-left", PanArrangementLeft, Some("AudecArrangement")),
        KeyBinding::new("shift-right", PanArrangementRight, Some("AudecArrangement")),
        KeyBinding::new("0", FitArrangement, Some("AudecArrangement")),
        KeyBinding::new("s", CycleArrangementSnap, Some("AudecArrangement")),
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
}

/// A dock/window-ready GPUI entity over the persistent arrangement core.
pub struct ArrangementView {
    // Rendering always works from this local snapshot. When `shared_editor` is
    // present, edits are applied while holding the shared editor's lock and
    // this snapshot is replaced before releasing that lock.
    editor: ArrangementEditor,
    shared_editor: Option<SharedArrangementEditor>,
    viewport: ArrangementViewport,
    focus_handle: FocusHandle,
    timeline_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    bpm: f64,
    beats_per_bar: u8,
    snap: SnapDivision,
    status: String,
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
        let viewport = fit_viewport(&editor, bpm, beats_per_bar);
        Self {
            editor,
            shared_editor,
            viewport,
            focus_handle: cx.focus_handle(),
            timeline_bounds: Arc::new(Mutex::new(None)),
            bpm,
            beats_per_bar,
            snap: SnapDivision::Beat,
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

    fn refresh_editor_snapshot(&mut self) {
        if let Some(shared_editor) = &self.shared_editor {
            self.editor = lock_editor(shared_editor).clone();
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
            self.bpm = bpm;
            self.beats_per_bar = beats_per_bar;
            self.status = format!("Grid set to {bpm:.2} BPM · {beats_per_bar}/4");
            cx.notify();
        }
    }

    fn selected_clip_id(&self) -> Option<ClipId> {
        self.editor.selection.clips.iter().next().copied()
    }

    fn select_clip(&mut self, id: ClipId, cx: &mut Context<Self>) {
        self.update_editor(|editor| {
            editor.selection.clips.clear();
            editor.selection.clips.insert(id);
            editor.selection.tracks.clear();
        });
        if let Some((track_id, placement, name)) = self
            .editor
            .state()
            .clip(id)
            .map(|clip| (clip.track_id, clip.placement, clip.name.clone()))
        {
            self.update_editor(|editor| {
                editor.selection.tracks.insert(track_id);
                editor.selection.time = Some(placement);
            });
            self.status = format!("Selected {name} · clip #{}", id.get());
        }
        cx.notify();
    }

    fn select_time(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some(bounds) = *self.timeline_bounds.lock().unwrap() else {
            return;
        };
        if !bounds.contains(&position) {
            return;
        }
        let fraction = f64::from((position.x - bounds.origin.x) / bounds.size.width);
        let frame = self.viewport.frame_at_fraction(fraction);
        self.update_editor(|editor| {
            editor.selection.clips.clear();
            editor.selection.tracks.clear();
            editor.selection.time = None;
        });
        self.status = format!(
            "Cursor {} · sample {}",
            musical_position(
                frame,
                self.editor.state().sample_rate,
                self.bpm,
                self.beats_per_bar
            ),
            grouped_i64(frame.0)
        );
        cx.notify();
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

    fn nudge_selected(&mut self, direction: i64, cx: &mut Context<Self>) {
        self.refresh_editor_snapshot();
        let Some(id) = self.selected_clip_id() else {
            self.status = "Select a clip before nudging".into();
            cx.notify();
            return;
        };
        let bpm = self.bpm;
        let beats_per_bar = self.beats_per_bar;
        let snap = self.snap;
        let result = self.mutate_editor(|editor| {
            let clip = editor
                .state()
                .clip(id)
                .cloned()
                .ok_or(crate::arrangement::ArrangementError::MissingClip(id))?;
            let quantum = snap_frames(editor.state().sample_rate, bpm, beats_per_bar, snap)
                .unwrap_or(1) as i64;
            let raw = clip
                .placement
                .start
                .0
                .saturating_add(direction.saturating_mul(quantum));
            editor.move_clip(id, clip.track_id, Frame(snap_frame(raw, quantum.max(1))))
        });
        self.edit(result, cx);
    }

    fn trim_start(&mut self, cx: &mut Context<Self>) {
        self.refresh_editor_snapshot();
        let Some(id) = self.selected_clip_id() else {
            self.status = "Select a clip before trimming".into();
            cx.notify();
            return;
        };
        let step = self.edit_step();
        let result = self.mutate_editor(|editor| {
            let clip = editor
                .state()
                .clip(id)
                .cloned()
                .ok_or(crate::arrangement::ArrangementError::MissingClip(id))?;
            let step = step.min(clip.placement.len().saturating_sub(1)) as i64;
            editor.trim_left(id, Frame(clip.placement.start.0.saturating_add(step)))
        });
        self.edit(result, cx);
    }

    fn trim_end(&mut self, cx: &mut Context<Self>) {
        self.refresh_editor_snapshot();
        let Some(id) = self.selected_clip_id() else {
            self.status = "Select a clip before trimming".into();
            cx.notify();
            return;
        };
        let step = self.edit_step();
        let result = self.mutate_editor(|editor| {
            let clip = editor
                .state()
                .clip(id)
                .cloned()
                .ok_or(crate::arrangement::ArrangementError::MissingClip(id))?;
            let step = step.min(clip.placement.len().saturating_sub(1)) as i64;
            editor.trim_right(id, Frame(clip.placement.end.0.saturating_sub(step)))
        });
        self.edit(result, cx);
    }

    fn split_selected(&mut self, cx: &mut Context<Self>) {
        self.refresh_editor_snapshot();
        let Some(id) = self.selected_clip_id() else {
            self.status = "Select a clip before splitting".into();
            cx.notify();
            return;
        };
        let step = self.edit_step() as i64;
        match self.mutate_editor(|editor| {
            let clip = editor
                .state()
                .clip(id)
                .cloned()
                .ok_or(crate::arrangement::ArrangementError::MissingClip(id))?;
            let midpoint = clip
                .placement
                .start
                .0
                .saturating_add((clip.placement.len() / 2) as i64);
            let mut at = snap_frame(midpoint, step.max(1));
            if at <= clip.placement.start.0 || at >= clip.placement.end.0 {
                at = midpoint;
            }
            let right = editor.split_clip(id, Frame(at))?;
            editor.selection.clips.clear();
            editor.selection.clips.insert(right);
            Ok((right, at))
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
        match self.mutate_editor(|editor| {
            let clip = editor
                .state()
                .clip(id)
                .cloned()
                .ok_or(crate::arrangement::ArrangementError::MissingClip(id))?;
            let start = snap_frame(clip.placement.end.0, step);
            let copy = editor.duplicate_clip(id, Frame(start))?;
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
        let result = self.mutate_editor(|editor| editor.delete_clip(id));
        self.edit(result, cx);
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        self.refresh_editor_snapshot();
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
        self.status = "Fit project extent".into();
        cx.notify();
    }

    fn zoom(&mut self, scale: f64, cx: &mut Context<Self>) {
        let center = self.viewport.frame_at_fraction(0.5);
        self.viewport.zoom_around(center, scale);
        self.status = format!(
            "Visible span · {} samples",
            grouped_u64(self.viewport.span())
        );
        cx.notify();
    }

    fn pan(&mut self, fraction: f64, cx: &mut Context<Self>) {
        self.viewport.pan(fraction);
        self.status = format!("Panned to sample {}", grouped_i64(self.viewport.start.0));
        cx.notify();
    }

    fn cycle_snap(&mut self, cx: &mut Context<Self>) {
        self.snap = self.snap.next();
        self.status = self.snap.label().into();
        cx.notify();
    }

    fn add_track(&mut self, kind: TrackKind, cx: &mut Context<Self>) {
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
    fn on_nudge_left(&mut self, _: &NudgeClipLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.nudge_selected(-1, cx);
    }
    fn on_nudge_right(&mut self, _: &NudgeClipRight, _: &mut Window, cx: &mut Context<Self>) {
        self.nudge_selected(1, cx);
    }
    fn on_trim_start(&mut self, _: &TrimClipStart, _: &mut Window, cx: &mut Context<Self>) {
        self.trim_start(cx);
    }
    fn on_trim_end(&mut self, _: &TrimClipEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.trim_end(cx);
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

    fn render_ruler(&self) -> impl IntoElement {
        let sample_rate = self.editor.state().sample_rate;
        let ticks = ruler_ticks(self.viewport, sample_rate, self.bpm, self.beats_per_bar);
        div()
            .h(px(42.0))
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
                    .child(
                        div()
                            .absolute()
                            .right_2()
                            .bottom_1()
                            .text_xs()
                            .text_color(rgb(DIM))
                            .child("BAR.BEAT · EXACT SAMPLE"),
                    ),
            )
    }

    fn render_track(&self, track: &Track, cx: &mut Context<Self>) -> impl IntoElement {
        let color = track_color(track.kind);
        let clips: Vec<_> = self
            .editor
            .state()
            .clips_on_track(track.id)
            .filter_map(|clip| {
                visible_clip(clip.placement, self.viewport).map(|visible| (clip.clone(), visible))
            })
            .collect();
        let ticks = ruler_ticks(
            self.viewport,
            self.editor.state().sample_rate,
            self.bpm,
            self.beats_per_bar,
        );
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
                    .children(clips.into_iter().map(|(clip, visible)| {
                        let id = clip.id;
                        let selected = self.editor.selection.clips.contains(&id);
                        clip_block(clip, visible, selected, color).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.select_clip(id, cx);
                                cx.stop_propagation();
                            },
                        ))
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.select_time(event.position, cx);
                        }),
                    ),
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
                    .child(div().mt_2().text_xs().text_color(rgb(DIM)).child("← / → nudge · [ / ] trim · ⌘E split\n⌘D duplicate · Delete remove · ⌘Z undo"))
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Take the snapshot before creating any elements, so no mutex guard can
        // leak into GPUI's layout or paint work.
        self.refresh_editor_snapshot();
        let bounds = self.timeline_bounds.clone();
        let tracks: Vec<_> = self
            .editor
            .state()
            .track_order
            .iter()
            .filter_map(|id| self.editor.state().track(*id).cloned())
            .collect();
        div()
            .key_context("AudecArrangement")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_undo))
            .on_action(cx.listener(Self::on_redo))
            .on_action(cx.listener(Self::on_duplicate))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_split))
            .on_action(cx.listener(Self::on_nudge_left))
            .on_action(cx.listener(Self::on_nudge_right))
            .on_action(cx.listener(Self::on_trim_start))
            .on_action(cx.listener(Self::on_trim_end))
            .on_action(cx.listener(Self::on_zoom_in))
            .on_action(cx.listener(Self::on_zoom_out))
            .on_action(cx.listener(Self::on_pan_left))
            .on_action(cx.listener(Self::on_pan_right))
            .on_action(cx.listener(Self::on_fit))
            .on_action(cx.listener(Self::on_snap))
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
                            .on_scroll_wheel(cx.listener(
                                |this, event: &ScrollWheelEvent, window, cx| {
                                    let delta = event.delta.pixel_delta(window.line_height());
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
                                            this.status = format!(
                                                "Zoom · {} samples visible",
                                                grouped_u64(this.viewport.span())
                                            );
                                            cx.notify();
                                        }
                                    } else {
                                        let wheel = if delta.x.abs() > px(0.01) {
                                            delta.x
                                        } else {
                                            delta.y
                                        };
                                        let amount = -f64::from(wheel / px(520.0));
                                        if amount.abs() > 0.0001 {
                                            this.viewport.pan(amount);
                                            this.status = format!(
                                                "Pan · starts at sample {}",
                                                grouped_i64(this.viewport.start.0)
                                            );
                                            cx.notify();
                                        }
                                    }
                                    cx.stop_propagation();
                                },
                            ))
                            .child(self.render_ruler())
                            .child(
                                div()
                                    .id("arrangement-track-scroll")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .children(
                                        tracks.iter().map(|track| self.render_track(track, cx)),
                                    )
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
                                    }),
                            )
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
            caveat: "Arrangement edits are live project metadata. Time-stretch, pitch, reverse, warp, fades, and gain are not audible until the render compiler applies this contract.".into(),
        },
        ClipContent::Pattern(pattern) => InspectorContent {
            mapping: format!(
                "Pattern #{}\ncontent offset {} frames\nreusable definition · loop {}",
                pattern.pattern.get(), grouped_u64(pattern.content_offset_frames), yes_no(pattern.looped)
            ),
            transform: "Pattern placement and offsets are exact project-frame metadata.".into(),
            caveat: "Pattern note evaluation and instrument synthesis are not performed by the arrangement core yet.".into(),
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

fn clip_block(
    clip: Clip,
    visible: VisibleClip,
    selected: bool,
    color: u32,
) -> gpui::Stateful<gpui::Div> {
    let left = visible.left;
    let width = visible.width;
    let kind = clip.content.kind();
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
        .border_color(if selected { rgb(TEXT) } else { rgb(color) })
        .bg(if selected {
            rgba((color << 8) | 0x55)
        } else {
            rgba((color << 8) | 0x2c)
        })
        .cursor_pointer()
        .hover(move |style| style.bg(rgba((color << 8) | 0x48)).border_color(rgb(TEXT)))
        .child(clip_texture(kind, color, clip.id.get()))
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
                .child(format!("{}f", grouped_u64(clip.placement.len()))),
        )
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
    fn snapping_handles_negative_frames_and_late_ties() {
        assert_eq!(snap_frame(149, 100), 100);
        assert_eq!(snap_frame(150, 100), 200);
        assert_eq!(snap_frame(-49, 100), 0);
        assert_eq!(snap_frame(-50, 100), 0);
        assert_eq!(snap_frame(-51, 100), -100);
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
}
