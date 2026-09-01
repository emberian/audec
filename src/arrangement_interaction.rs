//! Pure pointer interaction semantics for the arrangement canvas.
//!
//! This module refuses to own project truth, selection truth, GPUI entities,
//! or pointer capture.  It consumes immutable arrangement state plus exact
//! timeline coordinates and produces transient preview patches followed by at
//! most one semantic commit.  A controller remains responsible for resolving
//! that commit into the aggregate command algebra.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use crate::arrangement::{
    ArrangementState, ClipContent, ClipFades, ClipId, Fade, FadeCurve, Frame, FrameRange,
    OverlapPolicy, Selection, StretchAlgorithm, TrackId, TrackKind,
};

#[path = "arrangement_surface.rs"]
pub mod surface;

#[path = "arrangement_keyboard.rs"]
pub mod keyboard;

/// Canvas coordinates in the caller's device-independent coordinate space.
/// Timeline arithmetic never derives from these floating-point values.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CanvasPoint {
    pub x: f64,
    pub y: f64,
}

impl CanvasPoint {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    fn distance_squared(self, other: Self) -> f64 {
        let x = self.x - other.x;
        let y = self.y - other.y;
        x.mul_add(x, y * y)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CanvasRect {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

impl CanvasRect {
    pub const fn new(left: f64, top: f64, right: f64, bottom: f64) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub fn width(self) -> f64 {
        (self.right - self.left).max(0.0)
    }

    pub fn height(self) -> f64 {
        (self.bottom - self.top).max(0.0)
    }

    /// Inclusive edges make a rendered one-pixel boundary remain hittable.
    pub fn contains(self, point: CanvasPoint) -> bool {
        point.x >= self.left
            && point.x <= self.right
            && point.y >= self.top
            && point.y <= self.bottom
    }
}

/// Geometry supplied by the arrangement renderer.  `repeat_handle` is
/// explicit because current audio, pattern, and automation regions encode
/// looping differently; the interaction kernel must not invent a common
/// project representation before one exists.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipInteractionLayout {
    pub clip_id: ClipId,
    pub bounds: CanvasRect,
    pub repeat_handle: Option<CanvasRect>,
    pub z_order: i32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackInteractionLayout {
    pub track_id: TrackId,
    pub bounds: CanvasRect,
    pub z_order: i32,
}

/// Hit targets are deliberately larger than their rendered marks.  A caller
/// using physical pixels may scale this value for its display coordinate
/// space without changing gesture semantics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HitTestMetrics {
    pub trim_width: f64,
    pub fade_corner_size: f64,
}

impl Default for HitTestMetrics {
    fn default() -> Self {
        Self {
            trim_width: 6.0,
            fade_corner_size: 12.0,
        }
    }
}

impl HitTestMetrics {
    pub fn scaled(self, scale: f64) -> Self {
        if !scale.is_finite() || scale <= 0.0 {
            return self;
        }
        Self {
            trim_width: self.trim_width * scale,
            fade_corner_size: self.fade_corner_size * scale,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PointerModifiers {
    pub shift: bool,
    pub command: bool,
    pub option: bool,
    pub control: bool,
}

/// The view performs its signed viewport mapping once and supplies this exact
/// target with every pointer event.  No float-to-frame conversion occurs here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimelinePointer {
    pub canvas: CanvasPoint,
    pub frame: Frame,
    pub track: Option<TrackId>,
    pub modifiers: PointerModifiers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrimEdge {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FadeEdge {
    In,
    Out,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipHitZone {
    Body,
    SlipBody,
    Trim(TrimEdge),
    Stretch,
    Fade(FadeEdge),
    RepeatBoundary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipHit {
    pub clip_id: ClipId,
    pub track_id: TrackId,
    pub zone: ClipHitZone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorHint {
    Move,
    Slip,
    TrimLeft,
    TrimRight,
    FadeIn,
    FadeOut,
    Repeat,
    Stretch,
    Marquee,
}

impl ClipHitZone {
    pub const fn cursor(self) -> CursorHint {
        match self {
            Self::Body => CursorHint::Move,
            Self::SlipBody => CursorHint::Slip,
            Self::Trim(TrimEdge::Left) => CursorHint::TrimLeft,
            Self::Trim(TrimEdge::Right) => CursorHint::TrimRight,
            Self::Stretch => CursorHint::Stretch,
            Self::Fade(FadeEdge::In) => CursorHint::FadeIn,
            Self::Fade(FadeEdge::Out) => CursorHint::FadeOut,
            Self::RepeatBoundary => CursorHint::Repeat,
        }
    }
}

/// Return the topmost stable semantic zone.  Zone choice occurs once on
/// pointer-down; crossing another zone during a drag never changes the edit.
pub fn hit_test_clip(
    state: &ArrangementState,
    layouts: &[ClipInteractionLayout],
    point: CanvasPoint,
    modifiers: PointerModifiers,
    metrics: HitTestMetrics,
) -> Option<ClipHit> {
    let mut candidates: Vec<_> = layouts
        .iter()
        .filter(|layout| {
            layout.bounds.contains(point)
                || layout
                    .repeat_handle
                    .is_some_and(|handle| handle.contains(point))
        })
        .filter_map(|layout| state.clip(layout.clip_id).map(|clip| (layout, clip)))
        .collect();

    candidates.sort_by(|(left_layout, _), (right_layout, _)| {
        left_layout
            .z_order
            .cmp(&right_layout.z_order)
            .then_with(|| left_layout.clip_id.cmp(&right_layout.clip_id))
    });
    let (layout, clip) = candidates.pop()?;

    let zone = if layout
        .repeat_handle
        .is_some_and(|handle| handle.contains(point))
    {
        ClipHitZone::RepeatBoundary
    } else {
        zone_inside_clip(clip, layout.bounds, point, modifiers, metrics)
    };
    Some(ClipHit {
        clip_id: clip.id,
        track_id: clip.track_id,
        zone,
    })
}

/// Resolve a track hit independently from clip z-order.  Callers generally use
/// this to populate [`TimelinePointer::track`].
pub fn hit_test_track(
    state: &ArrangementState,
    layouts: &[TrackInteractionLayout],
    point: CanvasPoint,
) -> Option<TrackId> {
    layouts
        .iter()
        .filter(|layout| state.track(layout.track_id).is_some() && layout.bounds.contains(point))
        .max_by(|left, right| {
            left.z_order
                .cmp(&right.z_order)
                .then_with(|| left.track_id.cmp(&right.track_id))
        })
        .map(|layout| layout.track_id)
}

fn zone_inside_clip(
    clip: &crate::arrangement::Clip,
    bounds: CanvasRect,
    point: CanvasPoint,
    modifiers: PointerModifiers,
    metrics: HitTestMetrics,
) -> ClipHitZone {
    let width = bounds.width();
    let height = bounds.height();
    let corner = metrics
        .fade_corner_size
        .max(0.0)
        .min(width / 2.0)
        .min(height);

    // Fade corners outrank trim strips so the cursor is stable at their small
    // shared boundary.  Fades currently have honest render semantics only for
    // audio clips.
    if matches!(clip.content, ClipContent::Audio(_)) && point.y <= bounds.top + corner {
        if point.x <= bounds.left + corner {
            return ClipHitZone::Fade(FadeEdge::In);
        }
        if point.x >= bounds.right - corner {
            return ClipHitZone::Fade(FadeEdge::Out);
        }
    }

    let trim = metrics.trim_width.max(0.0).min(width / 2.0);
    if point.x <= bounds.left + trim && point.x <= bounds.left + width / 2.0 {
        return ClipHitZone::Trim(TrimEdge::Left);
    }
    if point.x >= bounds.right - trim
        && modifiers.control
        && matches!(clip.content, ClipContent::Audio(_))
    {
        return ClipHitZone::Stretch;
    }
    if point.x >= bounds.right - trim {
        return ClipHitZone::Trim(TrimEdge::Right);
    }

    if modifiers.option
        && matches!(clip.content, ClipContent::Audio(_))
        && point.y >= bounds.top + height / 2.0
    {
        ClipHitZone::SlipBody
    } else {
        ClipHitZone::Body
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SnapGuideKind {
    Marker,
    LoopBoundary,
    PunchBoundary,
    Transient,
    Event,
    Playhead,
    ClipStart(ClipId),
    ClipEnd(ClipId),
    Bar,
    Beat,
    Grid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapGuide {
    pub frame: Frame,
    pub kind: SnapGuideKind,
    /// Stable tie-breaker for multiple semantic guides at the same frame.
    pub key: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnapContext {
    /// A zero or absent quantum disables grid candidates.
    pub grid_quantum: Option<u64>,
    /// Frame tolerance should be derived from the desired screen-space radius
    /// by the viewport mapper.
    pub tolerance_frames: u64,
    pub guides: Vec<SnapGuide>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapResult {
    pub proposed: Frame,
    pub snapped: Frame,
    pub guide: SnapGuide,
}

impl SnapResult {
    pub fn adjustment(self) -> i64 {
        self.snapped.0.saturating_sub(self.proposed.0)
    }
}

impl SnapContext {
    pub fn resolve(
        &self,
        proposed: Frame,
        excluded_clips: &BTreeSet<ClipId>,
    ) -> Option<SnapResult> {
        let mut candidates = self.guides.clone();
        if let Some(quantum) = self.grid_quantum.filter(|quantum| *quantum > 0) {
            let quantum = quantum.min(i64::MAX as u64) as i64;
            let remainder = proposed.0.rem_euclid(quantum);
            let lower = proposed.0.saturating_sub(remainder);
            let upper = lower.saturating_add(quantum);
            candidates.push(SnapGuide {
                frame: Frame(lower),
                kind: SnapGuideKind::Grid,
                key: 0,
            });
            if upper != lower {
                candidates.push(SnapGuide {
                    frame: Frame(upper),
                    kind: SnapGuideKind::Grid,
                    key: 1,
                });
            }
        }

        candidates
            .into_iter()
            .filter(|guide| !guide_belongs_to_any(*guide, excluded_clips))
            .filter_map(|guide| {
                let distance = proposed.0.abs_diff(guide.frame.0);
                (distance <= self.tolerance_frames).then_some((distance, guide))
            })
            .min_by(|(left_distance, left), (right_distance, right)| {
                left_distance
                    .cmp(right_distance)
                    .then_with(|| left.kind.cmp(&right.kind))
                    .then_with(|| left.frame.cmp(&right.frame))
                    .then_with(|| left.key.cmp(&right.key))
            })
            .map(|(_, guide)| SnapResult {
                proposed,
                snapped: guide.frame,
                guide,
            })
    }
}

fn guide_belongs_to_any(guide: SnapGuide, excluded: &BTreeSet<ClipId>) -> bool {
    match guide.kind {
        SnapGuideKind::ClipStart(id) | SnapGuideKind::ClipEnd(id) => excluded.contains(&id),
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionMode {
    Replace,
    Add,
    Toggle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectionIntent {
    Clips {
        ids: BTreeSet<ClipId>,
        primary: Option<ClipId>,
        mode: SelectionMode,
    },
    Marquee {
        range: FrameRange,
        tracks: BTreeSet<TrackId>,
        mode: SelectionMode,
    },
    ClearObjects,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipMove {
    pub clip_id: ClipId,
    pub from_track: TrackId,
    pub to_track: TrackId,
    pub from: FrameRange,
    pub to: FrameRange,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ArrangementEdit {
    MoveClips {
        moves: Vec<ClipMove>,
        /// The controller allocates fresh IDs and retains content identity.
        duplicate: bool,
    },
    TrimClip {
        clip_id: ClipId,
        edge: TrimEdge,
        boundary: Frame,
    },
    SlipClip {
        clip_id: ClipId,
        project_delta: i64,
    },
    /// Resize an audio occurrence while retaining its exact source range.
    ///
    /// This is intentionally distinct from trim: trim changes which source
    /// frames participate, while stretch changes their project-time mapping.
    /// The aggregate lowering layer refuses warp-marker clips until the
    /// piecewise mapping compiler can preserve those markers honestly.
    StretchClip {
        clip_id: ClipId,
        boundary: Frame,
        algorithm: StretchAlgorithm,
        preserve_pitch: bool,
    },
    SetClipFades {
        clip_id: ClipId,
        fades: ClipFades,
    },
    /// A future aggregate adapter owns the difference between pattern repeat,
    /// automation repeat, and audio loop-source extent.
    SetRepeatBoundary {
        clip_id: ClipId,
        boundary: Frame,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArrangementEditIntent {
    pub expected_revision: u64,
    pub edit: ArrangementEdit,
}

/// One pointer-up may adjust ephemeral selection and commit one project edit.
/// Keeping these separate prevents selection from dirtying the document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GestureCommit {
    pub selection: Option<SelectionIntent>,
    pub edit: Option<ArrangementEditIntent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarqueePreview {
    pub anchor_frame: Frame,
    pub focus_frame: Frame,
    pub anchor_track: Option<TrackId>,
    pub focus_track: Option<TrackId>,
}

impl MarqueePreview {
    pub fn range(self) -> Option<FrameRange> {
        FrameRange::new(
            self.anchor_frame.min(self.focus_frame),
            self.anchor_frame.max(self.focus_frame),
        )
        .ok()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PreviewChange {
    Move(ClipMove),
    Trim {
        clip_id: ClipId,
        before: FrameRange,
        after: FrameRange,
        edge: TrimEdge,
    },
    Slip {
        clip_id: ClipId,
        placement: FrameRange,
        project_delta: i64,
    },
    Stretch {
        clip_id: ClipId,
        before: FrameRange,
        after: FrameRange,
        algorithm: StretchAlgorithm,
        preserve_pitch: bool,
    },
    Fade {
        clip_id: ClipId,
        placement: FrameRange,
        fades: ClipFades,
    },
    RepeatBoundary {
        clip_id: ClipId,
        placement: FrameRange,
        boundary: Frame,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GestureDiagnostic {
    MissingClip(ClipId),
    MissingTrack(TrackId),
    LockedClip(ClipId),
    LockedTrack(TrackId),
    IncompatibleTrack {
        clip_id: ClipId,
        track_id: TrackId,
    },
    TrackMappingOutOfRange {
        clip_id: ClipId,
    },
    TimeOverflow,
    InvalidBoundary,
    RejectingOverlap {
        track_id: TrackId,
        first: ClipId,
        second: ClipId,
    },
    UnsupportedFade(ClipId),
    UnsupportedStretch(ClipId),
    WarpedStretchRequiresCompiler(ClipId),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PreviewPatch {
    pub changes: Vec<PreviewChange>,
    pub marquee: Option<MarqueePreview>,
    pub snap: Option<SnapResult>,
    pub diagnostics: Vec<GestureDiagnostic>,
}

impl PreviewPatch {
    pub fn is_valid(&self) -> bool {
        self.diagnostics.is_empty()
    }

    pub fn is_noop(&self) -> bool {
        self.changes.iter().all(|change| match change {
            PreviewChange::Move(change) => {
                change.from == change.to && change.from_track == change.to_track
            }
            PreviewChange::Trim { before, after, .. } => before == after,
            PreviewChange::Slip { project_delta, .. } => *project_delta == 0,
            PreviewChange::Stretch { before, after, .. } => before == after,
            PreviewChange::Fade { .. } | PreviewChange::RepeatBoundary { .. } => false,
        }) && self.marquee.is_none()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GesturePhase {
    Idle,
    Pressed,
    Dragging,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GestureResponse {
    Pressed { cursor: CursorHint },
    Preview(PreviewPatch),
    Commit(GestureCommit),
    Cancelled,
    Refused(Vec<GestureDiagnostic>),
    Idle,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GestureConfig {
    pub drag_threshold: f64,
    pub hit_metrics: HitTestMetrics,
}

impl Default for GestureConfig {
    fn default() -> Self {
        Self {
            drag_threshold: 3.0,
            hit_metrics: HitTestMetrics::default(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ClipBaseline {
    id: ClipId,
    track: TrackId,
    placement: FrameRange,
    kind: TrackKind,
    locked: bool,
    fades: ClipFades,
    stretch: Option<(StretchAlgorithm, bool, bool)>,
}

#[derive(Clone, Debug)]
enum ActiveKind {
    Move {
        anchor: ClipId,
        clips: Vec<ClipBaseline>,
        duplicate: bool,
    },
    Trim {
        clip: ClipBaseline,
        edge: TrimEdge,
    },
    Slip {
        clip: ClipBaseline,
    },
    Stretch {
        clip: ClipBaseline,
    },
    Fade {
        clip: ClipBaseline,
        edge: FadeEdge,
    },
    Repeat {
        clip: ClipBaseline,
    },
    Marquee,
}

#[derive(Clone, Debug)]
struct ActiveGesture {
    expected_revision: u64,
    press: TimelinePointer,
    current: TimelinePointer,
    kind: ActiveKind,
    click_selection: Option<SelectionIntent>,
    drag_selection: Option<SelectionIntent>,
    dragged: bool,
    preview: Option<PreviewPatch>,
}

/// Pointer gesture state machine.  It owns only small immutable baselines and
/// transient preview state, never an [`ArrangementState`] clone.
#[derive(Clone, Debug, Default)]
pub struct ArrangementInteraction {
    active: Option<ActiveGesture>,
}

impl ArrangementInteraction {
    pub fn phase(&self) -> GesturePhase {
        match &self.active {
            None => GesturePhase::Idle,
            Some(active) if active.dragged => GesturePhase::Dragging,
            Some(_) => GesturePhase::Pressed,
        }
    }

    pub fn preview(&self) -> Option<&PreviewPatch> {
        self.active
            .as_ref()
            .and_then(|active| active.preview.as_ref())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn pointer_down(
        &mut self,
        state: &ArrangementState,
        selection: &Selection,
        expected_revision: u64,
        layouts: &[ClipInteractionLayout],
        pointer: TimelinePointer,
        config: GestureConfig,
    ) -> GestureResponse {
        if self.active.is_some() {
            return GestureResponse::Refused(Vec::new());
        }

        let hit = hit_test_clip(
            state,
            layouts,
            pointer.canvas,
            pointer.modifiers,
            config.hit_metrics,
        );
        let (kind, click_selection_intent, drag_selection_intent, cursor) = match hit {
            Some(hit) => {
                let Some(clip) = baseline(state, hit.clip_id) else {
                    return GestureResponse::Refused(vec![GestureDiagnostic::MissingClip(
                        hit.clip_id,
                    )]);
                };
                let selection_intent = click_selection(selection, hit.clip_id, pointer.modifiers);
                let kind = match hit.zone {
                    ClipHitZone::Body => {
                        let ids =
                            effective_drag_selection(selection, hit.clip_id, pointer.modifiers);
                        let clips = ids
                            .into_iter()
                            .filter_map(|id| baseline(state, id))
                            .collect();
                        ActiveKind::Move {
                            anchor: hit.clip_id,
                            clips,
                            // Option on the upper body means linked duplicate;
                            // Option on the lower audio body hit-tests as slip.
                            duplicate: pointer.modifiers.option,
                        }
                    }
                    ClipHitZone::SlipBody => ActiveKind::Slip { clip },
                    ClipHitZone::Trim(edge) => ActiveKind::Trim { clip, edge },
                    ClipHitZone::Stretch => ActiveKind::Stretch { clip },
                    ClipHitZone::Fade(edge) => ActiveKind::Fade { clip, edge },
                    ClipHitZone::RepeatBoundary => ActiveKind::Repeat { clip },
                };
                let drag_selection_intent =
                    drag_selection(selection, hit.clip_id, pointer.modifiers);
                (
                    kind,
                    selection_intent,
                    drag_selection_intent,
                    hit.zone.cursor(),
                )
            }
            None => (ActiveKind::Marquee, None, None, CursorHint::Marquee),
        };

        self.active = Some(ActiveGesture {
            expected_revision,
            press: pointer,
            current: pointer,
            kind,
            click_selection: click_selection_intent,
            drag_selection: drag_selection_intent,
            dragged: false,
            preview: None,
        });
        GestureResponse::Pressed { cursor }
    }

    pub fn pointer_move(
        &mut self,
        state: &ArrangementState,
        pointer: TimelinePointer,
        snap: &SnapContext,
        config: GestureConfig,
    ) -> GestureResponse {
        let Some(active) = &mut self.active else {
            return GestureResponse::Idle;
        };
        active.current = pointer;
        let threshold = config.drag_threshold.max(0.0);
        if !active.dragged
            && active.press.canvas.distance_squared(pointer.canvas) < threshold * threshold
        {
            return GestureResponse::Pressed {
                cursor: cursor_for_kind(&active.kind),
            };
        }
        active.dragged = true;
        let preview = preview_active(state, active, snap);
        active.preview = Some(preview.clone());
        GestureResponse::Preview(preview)
    }

    pub fn pointer_up(
        &mut self,
        state: &ArrangementState,
        pointer: TimelinePointer,
        snap: &SnapContext,
        config: GestureConfig,
    ) -> GestureResponse {
        let Some(mut active) = self.active.take() else {
            return GestureResponse::Idle;
        };
        active.current = pointer;
        let threshold = config.drag_threshold.max(0.0);
        if !active.dragged
            && active.press.canvas.distance_squared(pointer.canvas) >= threshold * threshold
        {
            active.dragged = true;
        }

        if !active.dragged {
            let selection = match active.kind {
                ActiveKind::Marquee => Some(SelectionIntent::ClearObjects),
                _ => active.click_selection,
            };
            return GestureResponse::Commit(GestureCommit {
                selection,
                edit: None,
            });
        }

        let preview = preview_active(state, &active, snap);
        if !preview.is_valid() {
            return GestureResponse::Refused(preview.diagnostics);
        }
        let commit = commit_from_preview(state, &active, preview);
        GestureResponse::Commit(commit)
    }

    pub fn cancel(&mut self) -> GestureResponse {
        if self.active.take().is_some() {
            GestureResponse::Cancelled
        } else {
            GestureResponse::Idle
        }
    }
}

fn cursor_for_kind(kind: &ActiveKind) -> CursorHint {
    match kind {
        ActiveKind::Move { .. } => CursorHint::Move,
        ActiveKind::Trim {
            edge: TrimEdge::Left,
            ..
        } => CursorHint::TrimLeft,
        ActiveKind::Trim {
            edge: TrimEdge::Right,
            ..
        } => CursorHint::TrimRight,
        ActiveKind::Slip { .. } => CursorHint::Slip,
        ActiveKind::Stretch { .. } => CursorHint::Stretch,
        ActiveKind::Fade {
            edge: FadeEdge::In, ..
        } => CursorHint::FadeIn,
        ActiveKind::Fade {
            edge: FadeEdge::Out,
            ..
        } => CursorHint::FadeOut,
        ActiveKind::Repeat { .. } => CursorHint::Repeat,
        ActiveKind::Marquee => CursorHint::Marquee,
    }
}

fn baseline(state: &ArrangementState, id: ClipId) -> Option<ClipBaseline> {
    let clip = state.clip(id)?;
    let stretch = match &clip.content {
        ClipContent::Audio(audio) => Some((
            audio.playback.algorithm,
            audio.playback.preserve_pitch,
            !audio.playback.warp_markers.is_empty(),
        )),
        _ => None,
    };
    Some(ClipBaseline {
        id,
        track: clip.track_id,
        placement: clip.placement,
        kind: clip.content.kind(),
        locked: clip.locked,
        fades: clip.fades,
        stretch,
    })
}

fn click_selection(
    selection: &Selection,
    clip_id: ClipId,
    modifiers: PointerModifiers,
) -> Option<SelectionIntent> {
    let mode = if modifiers.command {
        SelectionMode::Toggle
    } else if modifiers.shift {
        SelectionMode::Add
    } else if selection.clips.len() == 1 && selection.clips.contains(&clip_id) {
        return None;
    } else {
        SelectionMode::Replace
    };
    Some(SelectionIntent::Clips {
        ids: BTreeSet::from([clip_id]),
        primary: Some(clip_id),
        mode,
    })
}

fn drag_selection(
    selection: &Selection,
    clip_id: ClipId,
    modifiers: PointerModifiers,
) -> Option<SelectionIntent> {
    if selection.clips.contains(&clip_id) {
        // Command is snap suppression once the pointer crosses the drag
        // threshold; it must not remove the clip being dragged.
        return None;
    }
    Some(SelectionIntent::Clips {
        ids: BTreeSet::from([clip_id]),
        primary: Some(clip_id),
        mode: if modifiers.shift || modifiers.command {
            SelectionMode::Add
        } else {
            SelectionMode::Replace
        },
    })
}

fn effective_drag_selection(
    selection: &Selection,
    hit: ClipId,
    modifiers: PointerModifiers,
) -> BTreeSet<ClipId> {
    if selection.clips.contains(&hit) {
        return selection.clips.clone();
    }
    if modifiers.shift || modifiers.command {
        let mut selected = selection.clips.clone();
        selected.insert(hit);
        selected
    } else {
        BTreeSet::from([hit])
    }
}

fn preview_active(
    state: &ArrangementState,
    active: &ActiveGesture,
    snap: &SnapContext,
) -> PreviewPatch {
    match &active.kind {
        ActiveKind::Move { anchor, clips, .. } => preview_move(state, active, *anchor, clips, snap),
        ActiveKind::Trim { clip, edge } => preview_trim(active, *clip, *edge, snap),
        ActiveKind::Slip { clip } => preview_slip(active, *clip, snap),
        ActiveKind::Stretch { clip } => preview_stretch(active, *clip, snap),
        ActiveKind::Fade { clip, edge } => preview_fade(active, *clip, *edge),
        ActiveKind::Repeat { clip } => preview_repeat(active, *clip, snap),
        ActiveKind::Marquee => PreviewPatch {
            marquee: Some(MarqueePreview {
                anchor_frame: active.press.frame,
                focus_frame: active.current.frame,
                anchor_track: active.press.track,
                focus_track: active.current.track,
            }),
            ..PreviewPatch::default()
        },
    }
}

fn preview_move(
    state: &ArrangementState,
    active: &ActiveGesture,
    anchor_id: ClipId,
    clips: &[ClipBaseline],
    snap: &SnapContext,
) -> PreviewPatch {
    let mut patch = PreviewPatch::default();
    let moving: BTreeSet<_> = clips.iter().map(|clip| clip.id).collect();
    let raw_delta = motion_delta(active);
    let (delta, snap_result) = if active.current.modifiers.command {
        (raw_delta, None)
    } else {
        snap_moving_edges(clips, raw_delta, snap, &moving)
    };
    patch.snap = snap_result;

    let anchor_track = clips
        .iter()
        .find(|clip| clip.id == anchor_id)
        .map(|clip| clip.track);
    let requested_track = active.current.track.or(anchor_track);
    let track_delta = match (anchor_track, requested_track) {
        (Some(from), Some(to)) => track_index(state, to)
            .zip(track_index(state, from))
            .map(|(to, from)| to as isize - from as isize),
        _ => Some(0),
    };

    for clip in clips {
        validate_editable(state, *clip, &mut patch.diagnostics);
        let Some(track_delta) = track_delta else {
            patch.diagnostics.push(GestureDiagnostic::MissingTrack(
                requested_track.unwrap_or(clip.track),
            ));
            continue;
        };
        let Some(source_index) = track_index(state, clip.track) else {
            patch
                .diagnostics
                .push(GestureDiagnostic::MissingTrack(clip.track));
            continue;
        };
        let destination_index = source_index as isize + track_delta;
        let Some(destination) = usize::try_from(destination_index)
            .ok()
            .and_then(|index| state.track_order.get(index).copied())
        else {
            patch
                .diagnostics
                .push(GestureDiagnostic::TrackMappingOutOfRange { clip_id: clip.id });
            continue;
        };
        validate_destination(state, *clip, destination, &mut patch.diagnostics);
        let Some(start) = clip.placement.start.0.checked_add(delta) else {
            patch.diagnostics.push(GestureDiagnostic::TimeOverflow);
            continue;
        };
        let Some(end) = clip.placement.end.0.checked_add(delta) else {
            patch.diagnostics.push(GestureDiagnostic::TimeOverflow);
            continue;
        };
        let Ok(to) = FrameRange::new(Frame(start), Frame(end)) else {
            patch.diagnostics.push(GestureDiagnostic::TimeOverflow);
            continue;
        };
        patch.changes.push(PreviewChange::Move(ClipMove {
            clip_id: clip.id,
            from_track: clip.track,
            to_track: destination,
            from: clip.placement,
            to,
        }));
    }
    validate_move_overlaps(state, &moving, &mut patch);
    patch
}

fn motion_delta(active: &ActiveGesture) -> i64 {
    let raw = active.current.frame.0.saturating_sub(active.press.frame.0);
    if active.current.modifiers.shift {
        raw / 10
    } else {
        raw
    }
}

fn snap_moving_edges(
    clips: &[ClipBaseline],
    raw_delta: i64,
    snap: &SnapContext,
    moving: &BTreeSet<ClipId>,
) -> (i64, Option<SnapResult>) {
    let mut best: Option<(u64, ClipId, u8, SnapResult)> = None;
    for clip in clips {
        for (edge_order, edge) in [clip.placement.start, clip.placement.end]
            .into_iter()
            .enumerate()
        {
            let proposed = Frame(edge.0.saturating_add(raw_delta));
            let Some(result) = snap.resolve(proposed, moving) else {
                continue;
            };
            let candidate = (
                result.adjustment().unsigned_abs(),
                clip.id,
                edge_order as u8,
                result,
            );
            if best.as_ref().map_or(true, |current| {
                compare_snap_choice(&candidate, current) == Ordering::Less
            }) {
                best = Some(candidate);
            }
        }
    }
    match best {
        Some((_, _, _, result)) => (raw_delta.saturating_add(result.adjustment()), Some(result)),
        None => (raw_delta, None),
    }
}

fn compare_snap_choice(
    left: &(u64, ClipId, u8, SnapResult),
    right: &(u64, ClipId, u8, SnapResult),
) -> Ordering {
    left.0
        .cmp(&right.0)
        .then_with(|| left.1.cmp(&right.1))
        .then_with(|| left.2.cmp(&right.2))
        .then_with(|| left.3.guide.kind.cmp(&right.3.guide.kind))
        .then_with(|| left.3.guide.frame.cmp(&right.3.guide.frame))
        .then_with(|| left.3.guide.key.cmp(&right.3.guide.key))
}

fn preview_trim(
    active: &ActiveGesture,
    clip: ClipBaseline,
    edge: TrimEdge,
    snap: &SnapContext,
) -> PreviewPatch {
    let mut patch = PreviewPatch::default();
    if clip.locked {
        patch
            .diagnostics
            .push(GestureDiagnostic::LockedClip(clip.id));
    }
    let proposed = fine_frame(active);
    let excluded = BTreeSet::from([clip.id]);
    let snap_result = (!active.current.modifiers.command)
        .then(|| snap.resolve(proposed, &excluded))
        .flatten();
    let boundary = snap_result.map_or(proposed, |result| result.snapped);
    patch.snap = snap_result;
    let after = match edge {
        TrimEdge::Left => FrameRange::new(boundary, clip.placement.end),
        TrimEdge::Right => FrameRange::new(clip.placement.start, boundary),
    };
    match after {
        Ok(after) => patch.changes.push(PreviewChange::Trim {
            clip_id: clip.id,
            before: clip.placement,
            after,
            edge,
        }),
        Err(_) => patch.diagnostics.push(GestureDiagnostic::InvalidBoundary),
    }
    patch
}

fn preview_slip(active: &ActiveGesture, clip: ClipBaseline, snap: &SnapContext) -> PreviewPatch {
    let mut patch = PreviewPatch::default();
    if clip.locked {
        patch
            .diagnostics
            .push(GestureDiagnostic::LockedClip(clip.id));
    }
    let raw_delta = motion_delta(active);
    let proposed = Frame(clip.placement.start.0.saturating_add(raw_delta));
    let excluded = BTreeSet::from([clip.id]);
    let snap_result = (!active.current.modifiers.command)
        .then(|| snap.resolve(proposed, &excluded))
        .flatten();
    let delta = snap_result.map_or(raw_delta, |result| {
        raw_delta.saturating_add(result.adjustment())
    });
    patch.snap = snap_result;
    patch.changes.push(PreviewChange::Slip {
        clip_id: clip.id,
        placement: clip.placement,
        project_delta: delta,
    });
    patch
}

fn preview_stretch(active: &ActiveGesture, clip: ClipBaseline, snap: &SnapContext) -> PreviewPatch {
    let mut patch = PreviewPatch::default();
    if clip.locked {
        patch
            .diagnostics
            .push(GestureDiagnostic::LockedClip(clip.id));
    }
    let Some((algorithm, preserve_pitch, warped)) = clip.stretch else {
        patch
            .diagnostics
            .push(GestureDiagnostic::UnsupportedStretch(clip.id));
        return patch;
    };
    if warped {
        patch
            .diagnostics
            .push(GestureDiagnostic::WarpedStretchRequiresCompiler(clip.id));
        return patch;
    }
    let proposed = fine_frame(active);
    let excluded = BTreeSet::from([clip.id]);
    let snap_result = (!active.current.modifiers.command)
        .then(|| snap.resolve(proposed, &excluded))
        .flatten();
    let boundary = snap_result.map_or(proposed, |result| result.snapped);
    patch.snap = snap_result;
    let Ok(after) = FrameRange::new(clip.placement.start, boundary) else {
        patch.diagnostics.push(GestureDiagnostic::InvalidBoundary);
        return patch;
    };
    patch.changes.push(PreviewChange::Stretch {
        clip_id: clip.id,
        before: clip.placement,
        after,
        algorithm,
        preserve_pitch,
    });
    patch
}

fn preview_fade(active: &ActiveGesture, clip: ClipBaseline, edge: FadeEdge) -> PreviewPatch {
    let mut patch = PreviewPatch::default();
    if clip.locked {
        patch
            .diagnostics
            .push(GestureDiagnostic::LockedClip(clip.id));
    }
    if clip.kind != TrackKind::Audio {
        patch
            .diagnostics
            .push(GestureDiagnostic::UnsupportedFade(clip.id));
        return patch;
    }
    let frame = fine_frame(active);
    let duration = match edge {
        FadeEdge::In => frame
            .0
            .saturating_sub(clip.placement.start.0)
            .clamp(0, clip.placement.len() as i64) as u64,
        FadeEdge::Out => clip
            .placement
            .end
            .0
            .saturating_sub(frame.0)
            .clamp(0, clip.placement.len() as i64) as u64,
    };
    let mut fades = clip.fades;
    let slot = match edge {
        FadeEdge::In => &mut fades.fade_in,
        FadeEdge::Out => &mut fades.fade_out,
    };
    let curve = slot.map_or(FadeCurve::EqualPower, |fade| fade.curve);
    *slot = (duration > 0).then(|| Fade::full(duration, curve));
    patch.changes.push(PreviewChange::Fade {
        clip_id: clip.id,
        placement: clip.placement,
        fades,
    });
    patch
}

fn preview_repeat(active: &ActiveGesture, clip: ClipBaseline, snap: &SnapContext) -> PreviewPatch {
    let mut patch = PreviewPatch::default();
    if clip.locked {
        patch
            .diagnostics
            .push(GestureDiagnostic::LockedClip(clip.id));
    }
    let proposed = fine_frame(active);
    let excluded = BTreeSet::from([clip.id]);
    let snap_result = (!active.current.modifiers.command)
        .then(|| snap.resolve(proposed, &excluded))
        .flatten();
    let boundary = snap_result.map_or(proposed, |result| result.snapped);
    patch.snap = snap_result;
    if boundary <= clip.placement.start {
        patch.diagnostics.push(GestureDiagnostic::InvalidBoundary);
    } else {
        patch.changes.push(PreviewChange::RepeatBoundary {
            clip_id: clip.id,
            placement: clip.placement,
            boundary,
        });
    }
    patch
}

fn fine_frame(active: &ActiveGesture) -> Frame {
    let delta = active.current.frame.0.saturating_sub(active.press.frame.0);
    if active.current.modifiers.shift {
        Frame(active.press.frame.0.saturating_add(delta / 10))
    } else {
        active.current.frame
    }
}

fn track_index(state: &ArrangementState, track: TrackId) -> Option<usize> {
    state
        .track_order
        .iter()
        .position(|candidate| *candidate == track)
}

fn validate_editable(
    state: &ArrangementState,
    clip: ClipBaseline,
    diagnostics: &mut Vec<GestureDiagnostic>,
) {
    if clip.locked {
        diagnostics.push(GestureDiagnostic::LockedClip(clip.id));
    }
    match state.track(clip.track) {
        Some(track) if track.locked => diagnostics.push(GestureDiagnostic::LockedTrack(clip.track)),
        Some(_) => {}
        None => diagnostics.push(GestureDiagnostic::MissingTrack(clip.track)),
    }
}

fn validate_destination(
    state: &ArrangementState,
    clip: ClipBaseline,
    destination: TrackId,
    diagnostics: &mut Vec<GestureDiagnostic>,
) {
    match state.track(destination) {
        Some(track) => {
            if track.locked {
                diagnostics.push(GestureDiagnostic::LockedTrack(destination));
            }
            if track.kind != TrackKind::Hybrid && track.kind != clip.kind {
                diagnostics.push(GestureDiagnostic::IncompatibleTrack {
                    clip_id: clip.id,
                    track_id: destination,
                });
            }
        }
        None => diagnostics.push(GestureDiagnostic::MissingTrack(destination)),
    }
}

fn validate_move_overlaps(
    state: &ArrangementState,
    moving: &BTreeSet<ClipId>,
    patch: &mut PreviewPatch,
) {
    let previews: Vec<_> = patch
        .changes
        .iter()
        .filter_map(|change| match change {
            PreviewChange::Move(change) => Some(*change),
            _ => None,
        })
        .collect();
    for (index, moved) in previews.iter().enumerate() {
        let Some(track) = state.track(moved.to_track) else {
            continue;
        };
        if track.overlap != OverlapPolicy::Reject {
            continue;
        }
        for other in state.clips_on_track(track.id) {
            if !moving.contains(&other.id) && moved.to.intersects(other.placement) {
                patch.diagnostics.push(GestureDiagnostic::RejectingOverlap {
                    track_id: track.id,
                    first: moved.clip_id.min(other.id),
                    second: moved.clip_id.max(other.id),
                });
            }
        }
        for other in previews.iter().skip(index + 1) {
            if other.to_track == moved.to_track && moved.to.intersects(other.to) {
                patch.diagnostics.push(GestureDiagnostic::RejectingOverlap {
                    track_id: track.id,
                    first: moved.clip_id.min(other.clip_id),
                    second: moved.clip_id.max(other.clip_id),
                });
            }
        }
    }
    patch.diagnostics.sort_by_key(diagnostic_sort_key);
    patch.diagnostics.dedup();
}

fn diagnostic_sort_key(diagnostic: &GestureDiagnostic) -> (u8, u64, u64) {
    match *diagnostic {
        GestureDiagnostic::MissingClip(id) => (0, id.get(), 0),
        GestureDiagnostic::MissingTrack(id) => (1, id.get(), 0),
        GestureDiagnostic::LockedClip(id) => (2, id.get(), 0),
        GestureDiagnostic::LockedTrack(id) => (3, id.get(), 0),
        GestureDiagnostic::IncompatibleTrack { clip_id, track_id } => {
            (4, clip_id.get(), track_id.get())
        }
        GestureDiagnostic::TrackMappingOutOfRange { clip_id } => (5, clip_id.get(), 0),
        GestureDiagnostic::TimeOverflow => (6, 0, 0),
        GestureDiagnostic::InvalidBoundary => (7, 0, 0),
        GestureDiagnostic::RejectingOverlap { first, second, .. } => (8, first.get(), second.get()),
        GestureDiagnostic::UnsupportedFade(id) => (9, id.get(), 0),
        GestureDiagnostic::UnsupportedStretch(id) => (10, id.get(), 0),
        GestureDiagnostic::WarpedStretchRequiresCompiler(id) => (11, id.get(), 0),
    }
}

fn commit_from_preview(
    state: &ArrangementState,
    active: &ActiveGesture,
    preview: PreviewPatch,
) -> GestureCommit {
    if let Some(marquee) = preview.marquee {
        let selection = marquee.range().map(|range| SelectionIntent::Marquee {
            range,
            tracks: track_span(state, marquee),
            mode: selection_mode(active.press.modifiers),
        });
        return GestureCommit {
            selection: selection.or_else(|| {
                (selection_mode(active.press.modifiers) == SelectionMode::Replace)
                    .then_some(SelectionIntent::ClearObjects)
            }),
            edit: None,
        };
    }

    let edit = match (&active.kind, preview.changes.as_slice()) {
        (ActiveKind::Move { duplicate, .. }, changes) => {
            let moves: Vec<_> = changes
                .iter()
                .filter_map(|change| match change {
                    PreviewChange::Move(change)
                        if change.from != change.to || change.from_track != change.to_track =>
                    {
                        Some(*change)
                    }
                    _ => None,
                })
                .collect();
            (!moves.is_empty()).then_some(ArrangementEdit::MoveClips {
                moves,
                duplicate: *duplicate,
            })
        }
        (
            ActiveKind::Trim { edge, .. },
            [PreviewChange::Trim {
                clip_id,
                before,
                after,
                ..
            }],
        ) => (before != after).then_some(ArrangementEdit::TrimClip {
            clip_id: *clip_id,
            edge: *edge,
            boundary: match edge {
                TrimEdge::Left => after.start,
                TrimEdge::Right => after.end,
            },
        }),
        (
            ActiveKind::Slip { .. },
            [PreviewChange::Slip {
                clip_id,
                project_delta,
                ..
            }],
        ) => (*project_delta != 0).then_some(ArrangementEdit::SlipClip {
            clip_id: *clip_id,
            project_delta: *project_delta,
        }),
        (
            ActiveKind::Stretch { .. },
            [PreviewChange::Stretch {
                clip_id,
                before,
                after,
                algorithm,
                preserve_pitch,
            }],
        ) => (before != after).then_some(ArrangementEdit::StretchClip {
            clip_id: *clip_id,
            boundary: after.end,
            algorithm: *algorithm,
            preserve_pitch: *preserve_pitch,
        }),
        (ActiveKind::Fade { .. }, [PreviewChange::Fade { clip_id, fades, .. }]) => {
            Some(ArrangementEdit::SetClipFades {
                clip_id: *clip_id,
                fades: *fades,
            })
        }
        (
            ActiveKind::Repeat { .. },
            [PreviewChange::RepeatBoundary {
                clip_id, boundary, ..
            }],
        ) => Some(ArrangementEdit::SetRepeatBoundary {
            clip_id: *clip_id,
            boundary: *boundary,
        }),
        _ => None,
    };

    GestureCommit {
        selection: active.drag_selection.clone(),
        edit: edit.map(|edit| ArrangementEditIntent {
            expected_revision: active.expected_revision,
            edit,
        }),
    }
}

fn selection_mode(modifiers: PointerModifiers) -> SelectionMode {
    if modifiers.command {
        SelectionMode::Toggle
    } else if modifiers.shift {
        SelectionMode::Add
    } else {
        SelectionMode::Replace
    }
}

fn track_span(state: &ArrangementState, marquee: MarqueePreview) -> BTreeSet<TrackId> {
    match (marquee.anchor_track, marquee.focus_track) {
        (Some(anchor), Some(focus)) => {
            match (track_index(state, anchor), track_index(state, focus)) {
                (Some(anchor_index), Some(focus_index)) => {
                    let first = anchor_index.min(focus_index);
                    let last = anchor_index.max(focus_index);
                    state.track_order[first..=last].iter().copied().collect()
                }
                _ => BTreeSet::from([anchor, focus]),
            }
        }
        (Some(track), None) | (None, Some(track)) => BTreeSet::from([track]),
        (None, None) => BTreeSet::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrangement::{ArrangementEditor, AssetId, SourceRange};

    fn range(start: i64, len: u64) -> FrameRange {
        FrameRange::from_start_and_len(Frame(start), len).unwrap()
    }

    fn fixture() -> (ArrangementEditor, TrackId, TrackId, ClipId, ClipId) {
        let mut editor = ArrangementEditor::new(48_000).unwrap();
        let first_track = editor.create_track("one", TrackKind::Audio).unwrap();
        let second_track = editor.create_track("two", TrackKind::Audio).unwrap();
        let first = editor
            .create_audio_clip(
                first_track,
                "first",
                range(100, 100),
                AssetId::from_raw(1),
                SourceRange::new(1_000, 1_100).unwrap(),
            )
            .unwrap();
        let second = editor
            .create_audio_clip(
                second_track,
                "second",
                range(300, 50),
                AssetId::from_raw(2),
                SourceRange::new(2_000, 2_050).unwrap(),
            )
            .unwrap();
        (editor, first_track, second_track, first, second)
    }

    fn clip_layout(clip: ClipId) -> ClipInteractionLayout {
        ClipInteractionLayout {
            clip_id: clip,
            bounds: CanvasRect::new(10.0, 10.0, 210.0, 70.0),
            repeat_handle: Some(CanvasRect::new(202.0, 35.0, 218.0, 51.0)),
            z_order: 0,
        }
    }

    fn pointer(x: f64, y: f64, frame: i64, track: Option<TrackId>) -> TimelinePointer {
        TimelinePointer {
            canvas: CanvasPoint::new(x, y),
            frame: Frame(frame),
            track,
            modifiers: PointerModifiers::default(),
        }
    }

    #[test]
    fn hit_zone_precedence_is_stable_and_slip_is_fixed_at_pointer_down() {
        let (editor, track, _, clip, _) = fixture();
        let layout = clip_layout(clip);
        let metrics = HitTestMetrics::default();
        assert_eq!(
            hit_test_clip(
                editor.state(),
                &[layout],
                CanvasPoint::new(11.0, 11.0),
                PointerModifiers::default(),
                metrics,
            )
            .unwrap()
            .zone,
            ClipHitZone::Fade(FadeEdge::In)
        );
        assert_eq!(
            hit_test_clip(
                editor.state(),
                &[layout],
                CanvasPoint::new(11.0, 50.0),
                PointerModifiers::default(),
                metrics,
            )
            .unwrap()
            .zone,
            ClipHitZone::Trim(TrimEdge::Left)
        );
        assert_eq!(
            hit_test_clip(
                editor.state(),
                &[layout],
                CanvasPoint::new(100.0, 60.0),
                PointerModifiers {
                    option: true,
                    ..PointerModifiers::default()
                },
                metrics,
            )
            .unwrap()
            .zone,
            ClipHitZone::SlipBody
        );

        let mut interaction = ArrangementInteraction::default();
        let mut down = pointer(100.0, 60.0, 150, Some(track));
        down.modifiers.option = true;
        interaction.pointer_down(
            editor.state(),
            &editor.selection,
            7,
            &[layout],
            down,
            GestureConfig::default(),
        );
        let moved = pointer(150.0, 20.0, 170, Some(track));
        let response = interaction.pointer_move(
            editor.state(),
            moved,
            &SnapContext::default(),
            GestureConfig::default(),
        );
        assert!(matches!(
            response,
            GestureResponse::Preview(PreviewPatch {
                changes,
                ..
            }) if matches!(changes.as_slice(), [PreviewChange::Slip { project_delta: 20, .. }])
        ));
    }

    #[test]
    fn snapping_is_deterministic_at_negative_ties_and_excludes_moving_edges() {
        let moving = ClipId::from_raw(4);
        let context = SnapContext {
            grid_quantum: Some(100),
            tolerance_frames: 60,
            guides: vec![SnapGuide {
                frame: Frame(0),
                kind: SnapGuideKind::ClipStart(moving),
                key: 0,
            }],
        };
        let result = context
            .resolve(Frame(-50), &BTreeSet::from([moving]))
            .unwrap();
        // Equal-distance grid ties use exact frame order, including preroll.
        assert_eq!(result.snapped, Frame(-100));
        assert_eq!(result.guide.kind, SnapGuideKind::Grid);
    }

    #[test]
    fn control_right_edge_is_an_honest_audio_stretch_not_a_trim() {
        let (editor, track, _, clip, _) = fixture();
        let mut interaction = ArrangementInteraction::default();
        let mut down = pointer(209.0, 60.0, 200, Some(track));
        down.modifiers.control = true;
        let pressed = interaction.pointer_down(
            editor.state(),
            &editor.selection,
            12,
            &[clip_layout(clip)],
            down,
            GestureConfig::default(),
        );
        assert_eq!(
            pressed,
            GestureResponse::Pressed {
                cursor: CursorHint::Stretch
            }
        );
        let response = interaction.pointer_up(
            editor.state(),
            pointer(260.0, 60.0, 260, Some(track)),
            &SnapContext::default(),
            GestureConfig::default(),
        );
        assert!(matches!(
            response,
            GestureResponse::Commit(GestureCommit {
                edit: Some(ArrangementEditIntent {
                    expected_revision: 12,
                    edit: ArrangementEdit::StretchClip {
                        clip_id,
                        boundary: Frame(260),
                        ..
                    },
                }),
                ..
            }) if clip_id == clip
        ));
    }

    #[test]
    fn selected_clips_move_as_one_patch_with_relative_track_offsets() {
        let (mut editor, first_track, second_track, first, second) = fixture();
        editor.selection.clips = BTreeSet::from([first, second]);
        let before = editor.state().clone();
        let mut interaction = ArrangementInteraction::default();
        interaction.pointer_down(
            editor.state(),
            &editor.selection,
            42,
            &[clip_layout(first)],
            pointer(100.0, 40.0, 150, Some(first_track)),
            GestureConfig::default(),
        );
        let response = interaction.pointer_move(
            editor.state(),
            pointer(140.0, 40.0, 175, Some(first_track)),
            &SnapContext::default(),
            GestureConfig::default(),
        );
        let GestureResponse::Preview(preview) = response else {
            panic!("expected preview");
        };
        assert!(preview.is_valid());
        let moves: Vec<_> = preview
            .changes
            .iter()
            .filter_map(|change| match change {
                PreviewChange::Move(change) => Some(change),
                _ => None,
            })
            .collect();
        assert_eq!(moves.len(), 2);
        assert_eq!(moves[0].to.start, Frame(125));
        assert_eq!(moves[1].to.start, Frame(325));
        assert_eq!(moves[0].to_track, first_track);
        assert_eq!(moves[1].to_track, second_track);
        // Previewing cannot change project or editor-local selection truth.
        assert_eq!(editor.state(), &before);
        assert_eq!(editor.selection.clips, BTreeSet::from([first, second]));
    }

    #[test]
    fn pointer_up_coalesces_to_one_edit_and_then_returns_idle() {
        let (editor, track, _, clip, _) = fixture();
        let mut interaction = ArrangementInteraction::default();
        interaction.pointer_down(
            editor.state(),
            &editor.selection,
            91,
            &[clip_layout(clip)],
            pointer(100.0, 40.0, 150, Some(track)),
            GestureConfig::default(),
        );
        interaction.pointer_move(
            editor.state(),
            pointer(120.0, 40.0, 170, Some(track)),
            &SnapContext::default(),
            GestureConfig::default(),
        );
        interaction.pointer_move(
            editor.state(),
            pointer(140.0, 40.0, 190, Some(track)),
            &SnapContext::default(),
            GestureConfig::default(),
        );
        let response = interaction.pointer_up(
            editor.state(),
            pointer(150.0, 40.0, 200, Some(track)),
            &SnapContext::default(),
            GestureConfig::default(),
        );
        let GestureResponse::Commit(commit) = response else {
            panic!("expected commit");
        };
        assert!(matches!(
            commit.edit,
            Some(ArrangementEditIntent {
                expected_revision: 91,
                edit: ArrangementEdit::MoveClips { ref moves, duplicate: false },
            }) if moves.len() == 1 && moves[0].to.start == Frame(150)
        ));
        assert_eq!(interaction.phase(), GesturePhase::Idle);
        assert_eq!(
            interaction.pointer_up(
                editor.state(),
                pointer(150.0, 40.0, 200, Some(track)),
                &SnapContext::default(),
                GestureConfig::default(),
            ),
            GestureResponse::Idle
        );
    }

    #[test]
    fn cancel_discards_preview_and_marquee_preserves_exact_half_open_range() {
        let (editor, first_track, second_track, _, _) = fixture();
        let mut interaction = ArrangementInteraction::default();
        interaction.pointer_down(
            editor.state(),
            &editor.selection,
            1,
            &[],
            pointer(0.0, 0.0, 400, Some(second_track)),
            GestureConfig::default(),
        );
        let response = interaction.pointer_move(
            editor.state(),
            pointer(100.0, 100.0, 120, Some(first_track)),
            &SnapContext::default(),
            GestureConfig::default(),
        );
        assert!(matches!(
            response,
            GestureResponse::Preview(PreviewPatch {
                marquee: Some(MarqueePreview {
                    anchor_frame: Frame(400),
                    focus_frame: Frame(120),
                    ..
                }),
                ..
            })
        ));
        assert_eq!(interaction.cancel(), GestureResponse::Cancelled);
        assert!(interaction.preview().is_none());

        interaction.pointer_down(
            editor.state(),
            &editor.selection,
            2,
            &[],
            pointer(0.0, 0.0, 400, Some(second_track)),
            GestureConfig::default(),
        );
        let response = interaction.pointer_up(
            editor.state(),
            pointer(100.0, 100.0, 120, Some(first_track)),
            &SnapContext::default(),
            GestureConfig::default(),
        );
        assert!(matches!(
            response,
            GestureResponse::Commit(GestureCommit {
                selection: Some(SelectionIntent::Marquee { range, tracks, .. }),
                edit: None,
            }) if range == FrameRange::new(Frame(120), Frame(400)).unwrap()
                && tracks == BTreeSet::from([first_track, second_track])
        ));
    }

    #[test]
    fn invalid_trim_never_becomes_a_commit() {
        let (editor, track, _, clip, _) = fixture();
        let mut interaction = ArrangementInteraction::default();
        interaction.pointer_down(
            editor.state(),
            &editor.selection,
            3,
            &[clip_layout(clip)],
            pointer(11.0, 50.0, 100, Some(track)),
            GestureConfig::default(),
        );
        let response = interaction.pointer_up(
            editor.state(),
            pointer(100.0, 50.0, 200, Some(track)),
            &SnapContext::default(),
            GestureConfig::default(),
        );
        assert_eq!(
            response,
            GestureResponse::Refused(vec![GestureDiagnostic::InvalidBoundary])
        );
        assert_eq!(interaction.phase(), GesturePhase::Idle);
    }
}
