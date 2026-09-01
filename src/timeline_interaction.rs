//! Musician-facing timeline interaction semantics.
//!
//! This controller keeps selection, edit cursor, loop bounds, playhead/resume,
//! follow, and the local viewport as distinct state. It emits ordered effects
//! for a host adapter; it does not call an audio transport, mutate project
//! selection, know GPUI pointer types, or share a viewport between panes.

use super::TimelineViewport;

/// Identifies the pane-local viewport that an effect belongs to. It is not a
/// project object identity and must never be persisted as one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimelineControllerId(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimelinePoint(pub u64);

impl TimelinePoint {
    pub const ZERO: Self = Self(0);

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A non-empty, end-exclusive time range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimelineRange {
    pub start: TimelinePoint,
    pub end: TimelinePoint,
}

impl TimelineRange {
    pub fn new(start: TimelinePoint, end: TimelinePoint) -> Option<Self> {
        (start < end).then_some(Self { start, end })
    }

    pub fn between(first: TimelinePoint, second: TimelinePoint) -> Option<Self> {
        Self::new(first.min(second), first.max(second))
    }

    pub const fn span(self) -> u64 {
        self.end.0 - self.start.0
    }

    pub const fn contains(self, point: TimelinePoint) -> bool {
        self.start.0 <= point.0 && point.0 < self.end.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimelineSelection {
    pub range: Option<TimelineRange>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoopState {
    pub range: Option<TimelineRange>,
    pub enabled: bool,
}

impl LoopState {
    pub const fn disabled(range: Option<TimelineRange>) -> Self {
        Self {
            range,
            enabled: false,
        }
    }

    pub const fn active(range: TimelineRange) -> Self {
        Self {
            range: Some(range),
            enabled: true,
        }
    }

    fn normalize(mut self) -> Self {
        self.enabled &= self.range.is_some();
        self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlaybackMode {
    #[default]
    Stopped,
    Paused,
    Playing,
    Ended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackResume {
    /// Resume from the last transport publication or explicit locate.
    Playhead(TimelinePoint),
    /// A newly authored active loop owns the next start point. This replaces,
    /// rather than composes with, any older loop start.
    LoopStart(TimelinePoint),
}

impl PlaybackResume {
    pub const fn point(self) -> TimelinePoint {
        match self {
            Self::Playhead(point) | Self::LoopStart(point) => point,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FollowState {
    Off,
    Playhead { margin_fraction: f64 },
}

impl Default for FollowState {
    fn default() -> Self {
        Self::Playhead {
            margin_fraction: 0.16,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopEditPolicy {
    /// Selection and loop bounds are independent.
    Preserve,
    /// Replace bounds only if looping was active at pointer-down. Capturing
    /// this at gesture start makes an asynchronous transport publication
    /// unable to change the meaning of mouse-up.
    ReplaceIfEnabled,
    /// The gesture explicitly authors and enables a loop.
    ReplaceAndEnable,
}

impl LoopEditPolicy {
    /// Choose the musician-facing policy for a range gesture.
    ///
    /// An ordinary drag edits the bounds of an already-active loop but remains
    /// a plain selection when looping is off. The explicit authoring modifier
    /// creates and enables a loop in either state. Keeping this decision in
    /// the interaction kernel prevents host adapters from accidentally making
    /// an active loop impossible to reshape.
    pub const fn for_range_gesture(explicit_loop_authoring: bool) -> Self {
        if explicit_loop_authoring {
            Self::ReplaceAndEnable
        } else {
            Self::ReplaceIfEnabled
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerGesture {
    pub anchor: TimelinePoint,
    pub current: TimelinePoint,
    pub loop_policy: LoopEditPolicy,
    replace_loop: bool,
}

impl PointerGesture {
    pub fn preview(self) -> Option<TimelineRange> {
        TimelineRange::between(self.anchor, self.current)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewportRetention {
    /// Keep the pane's scroll and zoom when possible. This is the normal
    /// response to a longer bounce or a project publication.
    PreserveLocal,
    /// Fit the complete extent.
    Fit,
    /// Preserve zoom but center the specified product/playhead location.
    Center(TimelinePoint),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimelineInteractionSnapshot {
    pub owner: TimelineControllerId,
    pub total_samples: u64,
    pub selection: TimelineSelection,
    pub cursor: TimelinePoint,
    pub loop_state: LoopState,
    pub playhead: TimelinePoint,
    pub playback: PlaybackMode,
    pub resume: PlaybackResume,
    pub viewport: TimelineViewport,
    pub follow: FollowState,
    pub pointer: Option<PointerGesture>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TimelineInteractionEvent {
    PointerDown {
        at: TimelinePoint,
        loop_policy: LoopEditPolicy,
    },
    PointerMove {
        at: TimelinePoint,
    },
    PointerUp {
        at: TimelinePoint,
    },
    CancelPointer,
    ReplaceSelection(Option<TimelineRange>),
    ClearSelection,
    SetLoopFromSelection,
    ReplaceLoop(LoopState),
    ClearLoop,
    ToggleLoop,
    TransportObserved {
        playhead: TimelinePoint,
        mode: PlaybackMode,
    },
    PlayRequested,
    PauseRequested,
    StopRequested,
    PanFraction(f64),
    ZoomAround {
        anchor: TimelinePoint,
        scale: f64,
    },
    Fit,
    SetFollow(FollowState),
    SetExtent {
        total_samples: u64,
        retention: ViewportRetention,
    },
}

/// Audio effects are deliberately transport-shaped but backend-neutral. The
/// host applies them in emitted order. `preserve_playback` means seeking while
/// playing must remain playing; it does not mean an implicit Play command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportEffect {
    SetLoop(LoopState),
    Seek {
        to: TimelinePoint,
        preserve_playback: bool,
    },
    Play,
    Pause,
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TimelineEffect {
    SelectionChanged(TimelineSelection),
    SelectionPreview(Option<TimelineRange>),
    CursorChanged(TimelinePoint),
    LoopChanged(LoopState),
    Transport(TransportEffect),
    ViewportChanged {
        owner: TimelineControllerId,
        viewport: TimelineViewport,
    },
    FollowChanged(FollowState),
}

#[derive(Clone, Debug)]
pub struct TimelineInteraction {
    owner: TimelineControllerId,
    total_samples: u64,
    selection: TimelineSelection,
    cursor: TimelinePoint,
    loop_state: LoopState,
    playhead: TimelinePoint,
    playback: PlaybackMode,
    resume: PlaybackResume,
    viewport: TimelineViewport,
    follow: FollowState,
    pointer: Option<PointerGesture>,
}

impl TimelineInteraction {
    /// Create a pane-local timeline around the meaningful initial location.
    /// This intentionally does not assume that a track or clip begins at zero.
    pub fn new(
        owner: TimelineControllerId,
        total_samples: u64,
        initial: TimelinePoint,
        preferred_span: u64,
        minimum_span: u64,
    ) -> Self {
        let initial = clamp_point(initial, total_samples);
        let minimum_span = minimum_span.max(1).min(total_samples.max(1));
        let mut viewport =
            TimelineViewport::around(total_samples, initial.0, preferred_span.max(minimum_span));
        viewport.minimum_span = minimum_span;
        Self {
            owner,
            total_samples,
            selection: TimelineSelection::default(),
            cursor: initial,
            loop_state: LoopState::default(),
            playhead: initial,
            playback: PlaybackMode::Stopped,
            resume: PlaybackResume::Playhead(initial),
            viewport,
            follow: FollowState::default(),
            pointer: None,
        }
    }

    /// Restore one pane's persisted local viewport without borrowing scroll
    /// state from any other timeline. Invalid legacy bounds are normalized
    /// against the descriptor's own total extent.
    pub fn from_viewport(
        owner: TimelineControllerId,
        viewport: TimelineViewport,
        cursor: TimelinePoint,
    ) -> Self {
        let total_samples = viewport.total_samples;
        let minimum_span = viewport.minimum_span.max(1).min(total_samples.max(1));
        let span = viewport
            .end_sample
            .saturating_sub(viewport.start_sample)
            .max(minimum_span)
            .min(total_samples.max(1));
        let center = viewport
            .start_sample
            .saturating_add(span / 2)
            .min(total_samples);
        let mut result = Self::new(owner, total_samples, cursor, span, minimum_span);
        result.viewport = TimelineViewport::around(total_samples, center, span);
        result.viewport.minimum_span = minimum_span;
        result
    }

    pub fn snapshot(&self) -> TimelineInteractionSnapshot {
        TimelineInteractionSnapshot {
            owner: self.owner,
            total_samples: self.total_samples,
            selection: self.selection,
            cursor: self.cursor,
            loop_state: self.loop_state,
            playhead: self.playhead,
            playback: self.playback,
            resume: self.resume,
            viewport: self.viewport,
            follow: self.follow,
            pointer: self.pointer,
        }
    }

    pub fn apply(&mut self, event: TimelineInteractionEvent) -> Vec<TimelineEffect> {
        let mut effects = Vec::new();
        match event {
            TimelineInteractionEvent::PointerDown { at, loop_policy } => {
                let at = self.clamp(at);
                self.pointer = Some(PointerGesture {
                    anchor: at,
                    current: at,
                    loop_policy,
                    replace_loop: match loop_policy {
                        LoopEditPolicy::Preserve => false,
                        LoopEditPolicy::ReplaceIfEnabled => self.loop_state.enabled,
                        LoopEditPolicy::ReplaceAndEnable => true,
                    },
                });
                effects.push(TimelineEffect::SelectionPreview(None));
            }
            TimelineInteractionEvent::PointerMove { at } => {
                let at = self.clamp(at);
                if let Some(pointer) = self.pointer.as_mut() {
                    pointer.current = at;
                    effects.push(TimelineEffect::SelectionPreview(pointer.preview()));
                }
            }
            TimelineInteractionEvent::PointerUp { at } => {
                let at = self.clamp(at);
                if let Some(mut pointer) = self.pointer.take() {
                    pointer.current = at;
                    effects.push(TimelineEffect::SelectionPreview(None));
                    if let Some(range) = pointer.preview() {
                        self.commit_selection(range, pointer.replace_loop, &mut effects);
                    } else {
                        self.commit_locate(at, &mut effects);
                    }
                }
            }
            TimelineInteractionEvent::CancelPointer => {
                if self.pointer.take().is_some() {
                    // `None` during pointer-down means the in-progress drag is
                    // still collapsed; cancellation instead republishes the
                    // committed range so a view cannot visually lose it.
                    effects.push(TimelineEffect::SelectionPreview(self.selection.range));
                }
            }
            TimelineInteractionEvent::ReplaceSelection(range) => {
                let next = range.and_then(|range| self.clamp_range(range));
                self.selection = TimelineSelection { range: next };
                effects.push(TimelineEffect::SelectionChanged(self.selection));
            }
            TimelineInteractionEvent::ClearSelection => {
                if self.selection.range.take().is_some() {
                    effects.push(TimelineEffect::SelectionChanged(self.selection));
                }
            }
            TimelineInteractionEvent::SetLoopFromSelection => {
                if let Some(range) = self.selection.range {
                    self.install_loop(range, true, &mut effects);
                }
            }
            TimelineInteractionEvent::ReplaceLoop(loop_state) => {
                let range = loop_state.range.and_then(|range| self.clamp_range(range));
                self.loop_state = LoopState {
                    range,
                    enabled: loop_state.enabled,
                }
                .normalize();
                if let Some(range) = self.loop_state.range.filter(|_| self.loop_state.enabled) {
                    self.resume = PlaybackResume::LoopStart(range.start);
                } else {
                    self.resume = PlaybackResume::Playhead(self.playhead);
                }
                push_loop_effects(self.loop_state, &mut effects);
            }
            TimelineInteractionEvent::ClearLoop => {
                self.loop_state = LoopState::default();
                self.resume = PlaybackResume::Playhead(self.playhead);
                push_loop_effects(self.loop_state, &mut effects);
            }
            TimelineInteractionEvent::ToggleLoop => {
                if self.loop_state.range.is_none() {
                    self.loop_state.range = self.selection.range.or_else(|| self.viewport_range());
                }
                self.loop_state.enabled =
                    self.loop_state.range.is_some() && !self.loop_state.enabled;
                if let Some(range) = self.loop_state.range.filter(|_| self.loop_state.enabled) {
                    self.resume = PlaybackResume::LoopStart(range.start);
                } else {
                    self.resume = PlaybackResume::Playhead(self.playhead);
                }
                push_loop_effects(self.loop_state, &mut effects);
            }
            TimelineInteractionEvent::TransportObserved { playhead, mode } => {
                self.playhead = self.clamp(playhead);
                self.playback = mode;
                if mode != PlaybackMode::Playing {
                    self.resume = self
                        .loop_state
                        .range
                        .filter(|range| self.loop_state.enabled && !range.contains(self.playhead))
                        .map_or(PlaybackResume::Playhead(self.playhead), |range| {
                            PlaybackResume::LoopStart(range.start)
                        });
                }
                if mode == PlaybackMode::Playing {
                    self.follow_playhead(&mut effects);
                }
            }
            TimelineInteractionEvent::PlayRequested => self.play(&mut effects),
            TimelineInteractionEvent::PauseRequested => {
                if self.playback == PlaybackMode::Playing {
                    self.playback = PlaybackMode::Paused;
                    self.resume = PlaybackResume::Playhead(self.playhead);
                    effects.push(TimelineEffect::Transport(TransportEffect::Pause));
                }
            }
            TimelineInteractionEvent::StopRequested => {
                self.playback = PlaybackMode::Stopped;
                self.playhead = TimelinePoint::ZERO;
                self.resume = PlaybackResume::Playhead(self.cursor);
                effects.push(TimelineEffect::Transport(TransportEffect::Stop));
            }
            TimelineInteractionEvent::PanFraction(fraction) => {
                if !fraction.is_finite() {
                    return effects;
                }
                let before = self.viewport;
                self.viewport.pan_fraction(fraction);
                self.disengage_follow(&mut effects);
                if self.viewport != before {
                    self.push_viewport(&mut effects);
                }
            }
            TimelineInteractionEvent::ZoomAround { anchor, scale } => {
                if !scale.is_finite() || scale <= 0.0 {
                    return effects;
                }
                let before = self.viewport;
                self.viewport.zoom_around(self.clamp(anchor).0, scale);
                self.disengage_follow(&mut effects);
                if self.viewport != before {
                    self.push_viewport(&mut effects);
                }
            }
            TimelineInteractionEvent::Fit => {
                let minimum_span = self.viewport.minimum_span;
                self.viewport = TimelineViewport::fit(self.total_samples);
                self.viewport.minimum_span = minimum_span;
                self.disengage_follow(&mut effects);
                self.push_viewport(&mut effects);
            }
            TimelineInteractionEvent::SetFollow(follow) => {
                self.follow = normalize_follow(follow);
                effects.push(TimelineEffect::FollowChanged(self.follow));
                self.follow_playhead(&mut effects);
            }
            TimelineInteractionEvent::SetExtent {
                total_samples,
                retention,
            } => self.set_extent(total_samples, retention, &mut effects),
        }
        effects
    }

    fn commit_locate(&mut self, at: TimelinePoint, effects: &mut Vec<TimelineEffect>) {
        self.cursor = at;
        self.playhead = at;
        self.resume = PlaybackResume::Playhead(at);
        if self.selection.range.take().is_some() {
            effects.push(TimelineEffect::SelectionChanged(self.selection));
        }
        effects.push(TimelineEffect::CursorChanged(at));
        if self
            .loop_state
            .range
            .is_some_and(|range| self.loop_state.enabled && !range.contains(at))
        {
            // The audio transport intentionally starts a paused out-of-loop
            // Play at loop start. An explicit musician locate must not leave
            // an apparently active stale loop behind. Retain its editable
            // bounds, disable it, then seek in that order.
            self.loop_state.enabled = false;
            push_loop_effects(self.loop_state, effects);
        }
        effects.push(TimelineEffect::Transport(TransportEffect::Seek {
            to: at,
            preserve_playback: self.playback == PlaybackMode::Playing,
        }));
    }

    fn commit_selection(
        &mut self,
        range: TimelineRange,
        replace_loop: bool,
        effects: &mut Vec<TimelineEffect>,
    ) {
        self.selection = TimelineSelection { range: Some(range) };
        effects.push(TimelineEffect::SelectionChanged(self.selection));
        if replace_loop {
            self.install_loop(range, true, effects);
        }
    }

    /// Installing a loop is atomic at this semantic boundary: the host sees
    /// the new bounds before the seek to their start. This is the ordering that
    /// prevents an audio backend from resuming at an older loop start.
    fn install_loop(
        &mut self,
        range: TimelineRange,
        relocate: bool,
        effects: &mut Vec<TimelineEffect>,
    ) {
        self.loop_state = LoopState::active(range);
        self.resume = PlaybackResume::LoopStart(range.start);
        push_loop_effects(self.loop_state, effects);
        if relocate {
            self.playhead = range.start;
            effects.push(TimelineEffect::Transport(TransportEffect::Seek {
                to: range.start,
                preserve_playback: self.playback == PlaybackMode::Playing,
            }));
        }
    }

    fn play(&mut self, effects: &mut Vec<TimelineEffect>) {
        if self.playback == PlaybackMode::Playing {
            return;
        }
        let target = self.resume.point();
        if target != self.playhead || self.playback == PlaybackMode::Ended {
            self.playhead = target;
            effects.push(TimelineEffect::Transport(TransportEffect::Seek {
                to: target,
                preserve_playback: false,
            }));
        }
        self.playback = PlaybackMode::Playing;
        effects.push(TimelineEffect::Transport(TransportEffect::Play));
    }

    fn set_extent(
        &mut self,
        total_samples: u64,
        retention: ViewportRetention,
        effects: &mut Vec<TimelineEffect>,
    ) {
        let previous_selection = self.selection;
        let previous_cursor = self.cursor;
        let previous_loop = self.loop_state;
        let previous_playhead = self.playhead;
        self.total_samples = total_samples;
        self.cursor = self.clamp(self.cursor);
        self.playhead = self.clamp(self.playhead);
        self.selection.range = self
            .selection
            .range
            .and_then(|range| self.clamp_range(range));
        self.loop_state.range = self
            .loop_state
            .range
            .and_then(|range| self.clamp_range(range));
        self.loop_state = self.loop_state.normalize();
        if let Some(pointer) = self.pointer.as_mut() {
            pointer.anchor = clamp_point(pointer.anchor, total_samples);
            pointer.current = clamp_point(pointer.current, total_samples);
        }
        let old_span = self.viewport.span();
        match retention {
            ViewportRetention::PreserveLocal => self.viewport.set_total_samples(total_samples),
            ViewportRetention::Fit => {
                let minimum_span = self.viewport.minimum_span;
                self.viewport = TimelineViewport::fit(total_samples);
                self.viewport.minimum_span = minimum_span.min(total_samples.max(1));
            }
            ViewportRetention::Center(point) => {
                self.viewport.set_total_samples(total_samples);
                self.viewport
                    .set_span_around(self.clamp(point).0, old_span.max(1));
            }
        }
        self.resume = self
            .loop_state
            .range
            .filter(|_| self.loop_state.enabled)
            .map_or(PlaybackResume::Playhead(self.playhead), |range| {
                PlaybackResume::LoopStart(range.start)
            });
        if self.selection != previous_selection {
            effects.push(TimelineEffect::SelectionChanged(self.selection));
        }
        if self.cursor != previous_cursor {
            effects.push(TimelineEffect::CursorChanged(self.cursor));
        }
        if self.loop_state != previous_loop {
            push_loop_effects(self.loop_state, effects);
        }
        if self.playhead != previous_playhead {
            effects.push(TimelineEffect::Transport(TransportEffect::Seek {
                to: self.playhead,
                preserve_playback: self.playback == PlaybackMode::Playing,
            }));
        }
        self.push_viewport(effects);
    }

    fn follow_playhead(&mut self, effects: &mut Vec<TimelineEffect>) {
        let FollowState::Playhead { margin_fraction } = self.follow else {
            return;
        };
        if self
            .viewport
            .ensure_visible(self.playhead.0, margin_fraction)
        {
            self.push_viewport(effects);
        }
    }

    fn disengage_follow(&mut self, effects: &mut Vec<TimelineEffect>) {
        if self.follow != FollowState::Off {
            self.follow = FollowState::Off;
            effects.push(TimelineEffect::FollowChanged(FollowState::Off));
        }
    }

    fn push_viewport(&self, effects: &mut Vec<TimelineEffect>) {
        effects.push(TimelineEffect::ViewportChanged {
            owner: self.owner,
            viewport: self.viewport,
        });
    }

    fn viewport_range(&self) -> Option<TimelineRange> {
        TimelineRange::new(
            TimelinePoint(self.viewport.start_sample),
            TimelinePoint(self.viewport.end_sample),
        )
    }

    fn clamp(&self, point: TimelinePoint) -> TimelinePoint {
        clamp_point(point, self.total_samples)
    }

    fn clamp_range(&self, range: TimelineRange) -> Option<TimelineRange> {
        TimelineRange::new(self.clamp(range.start), self.clamp(range.end))
    }
}

fn push_loop_effects(loop_state: LoopState, effects: &mut Vec<TimelineEffect>) {
    effects.push(TimelineEffect::LoopChanged(loop_state));
    effects.push(TimelineEffect::Transport(TransportEffect::SetLoop(
        loop_state,
    )));
}

fn clamp_point(point: TimelinePoint, total_samples: u64) -> TimelinePoint {
    TimelinePoint(point.0.min(total_samples))
}

fn normalize_follow(follow: FollowState) -> FollowState {
    match follow {
        FollowState::Off => FollowState::Off,
        FollowState::Playhead { margin_fraction } => FollowState::Playhead {
            margin_fraction: if margin_fraction.is_finite() {
                margin_fraction.clamp(0.0, 0.49)
            } else {
                0.16
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(value: u64) -> TimelinePoint {
        TimelinePoint(value)
    }

    fn range(start: u64, end: u64) -> TimelineRange {
        TimelineRange::new(point(start), point(end)).unwrap()
    }

    fn controller(owner: u64) -> TimelineInteraction {
        TimelineInteraction::new(TimelineControllerId(owner), 10_000, point(5_000), 1_000, 10)
    }

    #[test]
    fn active_loop_selection_replaces_bounds_before_relocating() {
        let mut timeline = controller(1);
        timeline.apply(TimelineInteractionEvent::ReplaceLoop(LoopState::active(
            range(100, 300),
        )));
        timeline.apply(TimelineInteractionEvent::PointerDown {
            at: point(700),
            loop_policy: LoopEditPolicy::ReplaceIfEnabled,
        });
        let effects = timeline.apply(TimelineInteractionEvent::PointerUp { at: point(900) });

        assert_eq!(timeline.snapshot().selection.range, Some(range(700, 900)));
        assert_eq!(
            timeline.snapshot().loop_state,
            LoopState::active(range(700, 900))
        );
        assert_eq!(
            timeline.snapshot().resume,
            PlaybackResume::LoopStart(point(700))
        );
        assert_eq!(
            effects,
            vec![
                TimelineEffect::SelectionPreview(None),
                TimelineEffect::SelectionChanged(TimelineSelection {
                    range: Some(range(700, 900))
                }),
                TimelineEffect::LoopChanged(LoopState::active(range(700, 900))),
                TimelineEffect::Transport(TransportEffect::SetLoop(LoopState::active(range(
                    700, 900
                )))),
                TimelineEffect::Transport(TransportEffect::Seek {
                    to: point(700),
                    preserve_playback: false,
                }),
            ]
        );
    }

    #[test]
    fn musician_range_policy_edits_an_active_loop_but_not_an_inactive_one() {
        let mut active = controller(1);
        active.apply(TimelineInteractionEvent::ReplaceLoop(LoopState::active(
            range(100, 300),
        )));
        active.apply(TimelineInteractionEvent::PointerDown {
            at: point(700),
            loop_policy: LoopEditPolicy::for_range_gesture(false),
        });
        active.apply(TimelineInteractionEvent::PointerUp { at: point(900) });
        assert_eq!(
            active.snapshot().loop_state,
            LoopState::active(range(700, 900))
        );

        let mut inactive = controller(2);
        inactive.apply(TimelineInteractionEvent::PointerDown {
            at: point(700),
            loop_policy: LoopEditPolicy::for_range_gesture(false),
        });
        inactive.apply(TimelineInteractionEvent::PointerUp { at: point(900) });
        assert_eq!(inactive.snapshot().selection.range, Some(range(700, 900)));
        assert_eq!(inactive.snapshot().loop_state, LoopState::default());
    }

    #[test]
    fn explicit_loop_range_policy_authors_a_loop_from_the_inactive_state() {
        let mut timeline = controller(1);
        timeline.apply(TimelineInteractionEvent::PointerDown {
            at: point(700),
            loop_policy: LoopEditPolicy::for_range_gesture(true),
        });
        timeline.apply(TimelineInteractionEvent::PointerUp { at: point(900) });
        assert_eq!(
            timeline.snapshot().loop_state,
            LoopState::active(range(700, 900))
        );
    }

    #[test]
    fn replacement_policy_is_captured_at_pointer_down() {
        let mut timeline = controller(1);
        timeline.apply(TimelineInteractionEvent::ReplaceLoop(LoopState::active(
            range(100, 200),
        )));
        timeline.apply(TimelineInteractionEvent::PointerDown {
            at: point(500),
            loop_policy: LoopEditPolicy::ReplaceIfEnabled,
        });
        timeline.apply(TimelineInteractionEvent::ReplaceLoop(LoopState::disabled(
            Some(range(100, 200)),
        )));
        timeline.apply(TimelineInteractionEvent::PointerUp { at: point(600) });
        assert_eq!(
            timeline.snapshot().loop_state,
            LoopState::active(range(500, 600))
        );
    }

    #[test]
    fn ordinary_range_selection_never_locates_or_enables_a_loop() {
        let mut timeline = controller(1);
        timeline.apply(TimelineInteractionEvent::PointerDown {
            at: point(900),
            loop_policy: LoopEditPolicy::Preserve,
        });
        let effects = timeline.apply(TimelineInteractionEvent::PointerUp { at: point(600) });
        assert_eq!(timeline.snapshot().selection.range, Some(range(600, 900)));
        assert_eq!(timeline.snapshot().playhead, point(5_000));
        assert_eq!(timeline.snapshot().loop_state, LoopState::default());
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            TimelineEffect::Transport(TransportEffect::Seek { .. })
                | TimelineEffect::Transport(TransportEffect::SetLoop(_))
        )));
    }

    #[test]
    fn click_locates_and_clears_time_selection_without_touching_loop() {
        let mut timeline = controller(1);
        timeline.apply(TimelineInteractionEvent::ReplaceSelection(Some(range(
            20, 40,
        ))));
        timeline.apply(TimelineInteractionEvent::ReplaceLoop(LoopState::active(
            range(100, 200),
        )));
        timeline.apply(TimelineInteractionEvent::PointerDown {
            at: point(150),
            loop_policy: LoopEditPolicy::ReplaceIfEnabled,
        });
        let effects = timeline.apply(TimelineInteractionEvent::PointerUp { at: point(150) });
        assert_eq!(timeline.snapshot().cursor, point(150));
        assert_eq!(timeline.snapshot().playhead, point(150));
        assert_eq!(timeline.snapshot().selection.range, None);
        assert_eq!(
            timeline.snapshot().loop_state,
            LoopState::active(range(100, 200))
        );
        assert!(
            effects.contains(&TimelineEffect::Transport(TransportEffect::Seek {
                to: point(150),
                preserve_playback: false,
            }))
        );
    }

    #[test]
    fn click_outside_active_loop_disables_stale_loop_before_seek() {
        let mut timeline = controller(1);
        timeline.apply(TimelineInteractionEvent::ReplaceLoop(LoopState::active(
            range(100, 200),
        )));
        timeline.apply(TimelineInteractionEvent::PointerDown {
            at: point(800),
            loop_policy: LoopEditPolicy::ReplaceIfEnabled,
        });
        let effects = timeline.apply(TimelineInteractionEvent::PointerUp { at: point(800) });
        let disabled = LoopState::disabled(Some(range(100, 200)));
        assert_eq!(timeline.snapshot().loop_state, disabled);
        assert_eq!(
            timeline.snapshot().resume,
            PlaybackResume::Playhead(point(800))
        );
        let loop_index = effects
            .iter()
            .position(|effect| {
                *effect == TimelineEffect::Transport(TransportEffect::SetLoop(disabled))
            })
            .unwrap();
        let seek_index = effects
            .iter()
            .position(|effect| {
                *effect
                    == TimelineEffect::Transport(TransportEffect::Seek {
                        to: point(800),
                        preserve_playback: false,
                    })
            })
            .unwrap();
        assert!(loop_index < seek_index);
    }

    #[test]
    fn new_loop_owns_play_resume_instead_of_stale_loop_start() {
        let mut timeline = controller(1);
        timeline.apply(TimelineInteractionEvent::ReplaceLoop(LoopState::active(
            range(100, 200),
        )));
        timeline.apply(TimelineInteractionEvent::ReplaceSelection(Some(range(
            800, 900,
        ))));
        timeline.apply(TimelineInteractionEvent::SetLoopFromSelection);
        timeline.apply(TimelineInteractionEvent::PauseRequested);
        let effects = timeline.apply(TimelineInteractionEvent::PlayRequested);
        assert_eq!(
            timeline.snapshot().loop_state,
            LoopState::active(range(800, 900))
        );
        assert_eq!(timeline.snapshot().playhead, point(800));
        assert_eq!(
            effects,
            vec![TimelineEffect::Transport(TransportEffect::Play)]
        );
    }

    #[test]
    fn active_playback_survives_loop_replacement_with_explicit_seek_semantics() {
        let mut timeline = controller(1);
        timeline.apply(TimelineInteractionEvent::TransportObserved {
            playhead: point(5_100),
            mode: PlaybackMode::Playing,
        });
        timeline.apply(TimelineInteractionEvent::ReplaceSelection(Some(range(
            7_000, 7_500,
        ))));
        let effects = timeline.apply(TimelineInteractionEvent::SetLoopFromSelection);
        assert!(
            effects.contains(&TimelineEffect::Transport(TransportEffect::Seek {
                to: point(7_000),
                preserve_playback: true,
            }))
        );
        assert_eq!(timeline.snapshot().playback, PlaybackMode::Playing);
    }

    #[test]
    fn manual_pan_disengages_follow_and_transport_cannot_snap_it_back() {
        let mut timeline = controller(1);
        let original_playhead = timeline.snapshot().playhead;
        let original_selection = timeline.snapshot().selection;
        let original_loop = timeline.snapshot().loop_state;
        timeline.apply(TimelineInteractionEvent::PanFraction(0.5));
        let panned = timeline.snapshot().viewport;
        assert_eq!(timeline.snapshot().follow, FollowState::Off);
        timeline.apply(TimelineInteractionEvent::TransportObserved {
            playhead: point(9_000),
            mode: PlaybackMode::Playing,
        });
        assert_eq!(timeline.snapshot().viewport, panned);
        assert_ne!(timeline.snapshot().playhead, original_playhead);
        assert_eq!(timeline.snapshot().selection, original_selection);
        assert_eq!(timeline.snapshot().loop_state, original_loop);
    }

    #[test]
    fn follow_preserves_zoom_and_moves_only_its_owner_viewport() {
        let mut first = controller(1);
        let second = controller(2);
        let second_before = second.snapshot().viewport;
        let span = first.snapshot().viewport.span();
        first.apply(TimelineInteractionEvent::TransportObserved {
            playhead: point(9_000),
            mode: PlaybackMode::Playing,
        });
        assert_eq!(first.snapshot().viewport.span(), span);
        assert!(first.snapshot().viewport.contains(9_000));
        assert_eq!(second.snapshot().viewport, second_before);
    }

    #[test]
    fn project_growth_preserves_local_scroll_instead_of_returning_to_zero() {
        let mut timeline = controller(1);
        timeline.apply(TimelineInteractionEvent::PanFraction(1.5));
        let before = timeline.snapshot().viewport;
        assert!(before.start_sample > 0);
        timeline.apply(TimelineInteractionEvent::SetExtent {
            total_samples: 20_000,
            retention: ViewportRetention::PreserveLocal,
        });
        let after = timeline.snapshot().viewport;
        assert_eq!(after.start_sample, before.start_sample);
        assert_eq!(after.span(), before.span());
    }

    #[test]
    fn new_timeline_centers_meaningful_location_not_song_start() {
        let timeline =
            TimelineInteraction::new(TimelineControllerId(4), 100_000, point(70_000), 10_000, 100);
        assert_eq!(
            (
                timeline.snapshot().viewport.start_sample,
                timeline.snapshot().viewport.end_sample
            ),
            (65_000, 75_000)
        );
    }

    #[test]
    fn persisted_viewport_restores_its_own_scroll_independent_of_cursor() {
        let timeline = TimelineInteraction::from_viewport(
            TimelineControllerId(9),
            TimelineViewport {
                start_sample: 30_000,
                end_sample: 40_000,
                total_samples: 100_000,
                minimum_span: 100,
            },
            point(5_000),
        );
        assert_eq!(timeline.snapshot().cursor, point(5_000));
        assert_eq!(
            (
                timeline.snapshot().viewport.start_sample,
                timeline.snapshot().viewport.end_sample,
            ),
            (30_000, 40_000)
        );
    }

    #[test]
    fn cancelling_drag_restores_committed_selection_and_never_edits_loop() {
        let mut timeline = controller(1);
        timeline.apply(TimelineInteractionEvent::ReplaceSelection(Some(range(
            10, 20,
        ))));
        timeline.apply(TimelineInteractionEvent::ReplaceLoop(LoopState::active(
            range(100, 200),
        )));
        timeline.apply(TimelineInteractionEvent::PointerDown {
            at: point(500),
            loop_policy: LoopEditPolicy::ReplaceIfEnabled,
        });
        timeline.apply(TimelineInteractionEvent::PointerMove { at: point(700) });
        let effects = timeline.apply(TimelineInteractionEvent::CancelPointer);
        assert_eq!(timeline.snapshot().selection.range, Some(range(10, 20)));
        assert_eq!(
            timeline.snapshot().loop_state,
            LoopState::active(range(100, 200))
        );
        assert_eq!(
            effects,
            vec![TimelineEffect::SelectionPreview(Some(range(10, 20)))]
        );
    }

    #[test]
    fn shrinking_extent_clamps_every_coordinate_and_disables_empty_loop() {
        let mut timeline = controller(1);
        timeline.apply(TimelineInteractionEvent::ReplaceSelection(Some(range(
            8_000, 9_000,
        ))));
        timeline.apply(TimelineInteractionEvent::ReplaceLoop(LoopState::active(
            range(8_000, 9_000),
        )));
        timeline.apply(TimelineInteractionEvent::SetExtent {
            total_samples: 7_000,
            retention: ViewportRetention::PreserveLocal,
        });
        let snapshot = timeline.snapshot();
        assert_eq!(snapshot.selection.range, None);
        assert_eq!(snapshot.loop_state, LoopState::default());
        assert!(snapshot.cursor.0 <= 7_000);
        assert!(snapshot.playhead.0 <= 7_000);
        assert!(snapshot.viewport.end_sample <= 7_000);
    }

    #[test]
    fn non_finite_navigation_is_a_noop_and_does_not_disable_follow() {
        let mut timeline = controller(1);
        let before = timeline.snapshot();
        assert!(timeline
            .apply(TimelineInteractionEvent::PanFraction(f64::NAN))
            .is_empty());
        assert!(timeline
            .apply(TimelineInteractionEvent::ZoomAround {
                anchor: point(4_000),
                scale: f64::INFINITY,
            })
            .is_empty());
        assert_eq!(timeline.snapshot(), before);
    }
}
