//! Shared Visualizer lifecycle, viewport gestures, header, and GPUI trait impls.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::lens_hpss::DEFAULT_HPSS_SPAN_SECONDS;
use super::lens_loom::update_loom_render;
use super::*;

/// A pointer gesture named from outside the window, in fractions of the plot
/// a lens has painted: `pointer-click@0.25`, `pointer-drag@0.20:0.60`,
/// `pointer-alt-drag@0.20:0.60`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum LensPointerControl {
    Click(f64),
    Drag { from: f64, to: f64, alt: bool },
}

impl LensPointerControl {
    pub(super) fn parse(control: &str) -> Option<Self> {
        let (name, arguments) = control.split_once('@')?;
        let fraction = |text: &str| text.trim().parse::<f64>().ok().filter(|v| v.is_finite());
        match name {
            "pointer-click" => Some(Self::Click(fraction(arguments)?)),
            "pointer-drag" | "pointer-alt-drag" => {
                let (from, to) = arguments.split_once(':')?;
                Some(Self::Drag {
                    from: fraction(from)?,
                    to: fraction(to)?,
                    alt: name == "pointer-alt-drag",
                })
            }
            _ => None,
        }
    }
}

/// x, as a fraction of the painted plot, mapped into a sample of the material
/// through the window that plot is drawing.
///
/// Kept free of the toolkit so the arithmetic every lens gesture stands on can
/// be checked without a window.
pub(super) fn sample_in_window(raw_fraction: f64, window: (f64, f64), total_samples: u64) -> u64 {
    let (start, end) = window;
    let fraction = (start + raw_fraction.clamp(0.0, 1.0) * (end - start)).clamp(0.0, 1.0);
    (fraction * total_samples as f64).round() as u64
}

impl Visualizer {
    /// Build a lens by reading the Workbench entity. Only valid while no
    /// update lease on the Workbench is held; from inside a Workbench update
    /// use [`Self::with_seed`] with values taken from `&self`.
    pub(super) fn new(kind: VizKind, workbench: Entity<Workbench>, cx: &mut Context<Self>) -> Self {
        let (analysis, playhead) = {
            let workbench = workbench.read(cx);
            (
                workbench.analysis_arc(),
                workbench.playhead_fraction() as f64,
            )
        };
        Self::with_seed(kind, workbench, analysis, playhead, cx)
    }

    /// Build a lens from an explicit seed instead of reading the Workbench,
    /// so a Workbench that is itself being updated can create its own lenses
    /// (GPUI refuses a read while an update lease is held).
    pub(super) fn with_seed(
        kind: VizKind,
        workbench: Entity<Workbench>,
        analysis: Option<Arc<Analysis>>,
        playhead: f64,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&workbench, |_, _, cx| cx.notify()).detach();
        let (mut spectrum_settings, spectrogram_source, playhead, duration) = {
            if let Some(analysis) = analysis.as_ref() {
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
                    playhead,
                    analysis.duration_seconds,
                )
            } else {
                (SpectrumSettings::default(), None, 0.0, 0.0)
            }
        };
        let mut rhythm_settings = RhythmLensSettings::default();
        let mut loom_settings = LoomLensSettings::default();
        let mut hpss_settings = HpssSettings::default();
        let mut hpss_span_limit_seconds = DEFAULT_HPSS_SPAN_SECONDS;
        match crate::preferences::load() {
            Ok(preferences) => {
                preferences.apply_spectrum(&mut spectrum_settings);
                preferences.apply_rhythm(&mut rhythm_settings);
                preferences.apply_loom(&mut loom_settings);
                preferences.apply_separation(&mut hpss_settings, &mut hpss_span_limit_seconds);
            }
            Err(error) => eprintln!("preferences not applied: {error}"),
        }
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
            waveform_geometry: Arc::new(Mutex::new(WaveformGeometryCache::default())),
            focus_handle: cx.focus_handle(),
            time_start,
            time_end,
            follow_playhead: true,
            frequency_start: 0.0,
            frequency_end: 1.0,
            spectrum_settings,
            spectrum_refusal: None,
            selected_finding: 0,
            local_spectrogram: None,
            local_spectral_db: None,
            spectrogram_source,
            waterfall_freshness: Freshness::new(Authority::Lens(LensJob::Waterfall)),
            spectrum_transforming: false,
            hpss_state: HpssViewState::Idle,
            hpss_settings,
            hpss_span_limit_seconds,
            hpss_freshness: Freshness::new(Authority::Lens(LensJob::Hpss)),
            hpss_cancellation: None,
            rhythm_state: RhythmViewState::Idle,
            rhythm_freshness: Freshness::new(Authority::Lens(LensJob::Rhythm)),
            rhythm_cancellation: None,
            rhythm_settings,
            loom_state: LoomViewState::Idle,
            loom_freshness: Freshness::new(Authority::Lens(LensJob::Loom)),
            loom_cancellation: None,
            loom_settings,
        }
    }

    pub(super) fn set_project_generation(&mut self, generation: u64, cx: &mut Context<Self>) {
        self.session_project_generation = Some(generation);
        cx.notify();
    }

    pub(super) fn set_workspace_view_id(&mut self, view: WorkspaceViewId) {
        if let Ok(owner) = workspace_audition_owner(view) {
            self.audition_owner = owner;
        }
    }

    pub(super) fn set_session_audio(&mut self, audio: ProjectAudioStatus, cx: &mut Context<Self>) {
        self.session_audio = audio;
        cx.notify();
    }

    pub(super) fn set_semantic_selection(
        &mut self,
        selection: PaneSemanticSelection,
        cx: &mut Context<Self>,
    ) {
        self.semantic_selection = Some(selection);
        // Selection attention never changes this pane's viewport or follow
        // policy. Those are pane-local presentation facts by contract.
        cx.notify();
    }

    pub(super) fn cancel_background_work(&mut self, cx: &mut Context<Self>) {
        self.invalidate_background_work();
        if matches!(self.hpss_state, HpssViewState::Analyzing { .. }) {
            self.hpss_state = HpssViewState::Idle;
        }
        if matches!(self.rhythm_state, RhythmViewState::Analyzing) {
            self.rhythm_state = RhythmViewState::Idle;
        }
        if matches!(self.loom_state, LoomViewState::Inferring { .. }) {
            self.loom_state = LoomViewState::Idle;
        }
        cx.notify();
    }

    pub(super) fn invalidate_background_work(&mut self) {
        self.cancel_hpss_job();
        self.cancel_rhythm_job();
        self.cancel_loom_job();
    }

    /// One line for the musician, in the Workbench's notice channel. Every
    /// lens answers a press or a refusal through this, so a script reads the
    /// same words the header shows.
    pub(super) fn say(&self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.workbench.update(cx, |workbench, cx| {
            workbench.constructive_status = Some(message.into());
            cx.notify();
        });
    }

    /// What a press means when it landed on no mark. `bounds` is the plot the
    /// press was already resolved against, so a lens that hit tests its own
    /// marks first and one that does not map x the same way.
    pub(super) fn seek_within(
        &mut self,
        bounds: Bounds<Pixels>,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let duration = self
            .workbench
            .read(cx)
            .analysis()
            .map_or(0.0, |analysis| analysis.duration_seconds);
        let fraction = ((position.x - bounds.origin.x) / bounds.size.width).clamp(0.0, 1.0);
        let global_fraction = self.time_start + f64::from(fraction) * self.time_span();
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| {
            workbench.seek_to(duration * global_fraction, cx)
        });
    }

    /// The window of the material this lens is actually drawing, as fractions.
    ///
    /// For every lens that is its own viewport this is the viewport. The
    /// Separation lens is the exception: it draws the window its result was
    /// computed over, which is the viewport only while the view has not moved
    /// since — the header says "view changed — reanalyze to update" for
    /// exactly the case where these differ, and a pointer must land on the
    /// waveform the musician can see, not on the one that would be recomputed.
    pub(super) fn pointer_window(&self, total_samples: u64) -> (f64, f64) {
        match (self.kind, &self.hpss_state) {
            (VizKind::Separation, HpssViewState::Ready(result)) if total_samples > 0 => {
                let total = total_samples as f64;
                (
                    (result.start_frame as f64 / total).clamp(0.0, 1.0),
                    (result.end_frame as f64 / total).clamp(0.0, 1.0),
                )
            }
            _ => (self.time_start, self.time_end),
        }
    }

    /// Map a pointer x to a sample of the material through the window this
    /// lens draws.
    ///
    /// `Err` is a refusal to put in front of the musician; `Ok(None)` is a
    /// press that landed outside the plot, which is not a refusal but a press
    /// this lens does not own.
    pub(super) fn pointer_sample(
        &self,
        x: Pixels,
        clamp: bool,
        cx: &App,
    ) -> Result<Option<u64>, String> {
        let Some(bounds) = *self.timeline_bounds.lock().unwrap() else {
            return Err(format!(
                "{} has not painted a timeline yet · a pointer position becomes a moment only once the view has drawn one, so this gesture reached no time",
                self.kind.title()
            ));
        };
        if bounds.size.width <= px(0.0) {
            return Err(format!(
                "{} painted a timeline no pixels wide · there is no moment under the pointer to name",
                self.kind.title()
            ));
        }
        let total = self.workbench.read(cx).total_samples();
        if total == 0 {
            return Err(format!(
                "{} has no material behind it · a pointer position names a moment only in a song",
                self.kind.title()
            ));
        }
        let raw = f64::from((x - bounds.origin.x) / bounds.size.width);
        if !clamp && !(0.0..=1.0).contains(&raw) {
            return Ok(None);
        }
        Ok(Some(sample_in_window(
            raw,
            self.pointer_window(total),
            total,
        )))
    }

    /// The same mapping, reported rather than acted on: the refusal a gesture
    /// would have shown, said in the notice channel as well as returned.
    fn pointer_sample_or_say(&mut self, x: Pixels, cx: &mut Context<Self>) -> Result<u64, String> {
        let outcome = self.pointer_sample(x, true, cx);
        match outcome {
            Ok(Some(sample)) => Ok(sample),
            Ok(None) => {
                let refusal = format!(
                    "{} was asked for a pointer position outside its own plot",
                    self.kind.title()
                );
                self.say(refusal.clone(), cx);
                Err(refusal)
            }
            Err(refusal) => {
                self.say(refusal.clone(), cx);
                Err(refusal)
            }
        }
    }

    /// Run a pointer gesture named from outside the window, in fractions of
    /// the plot this lens draws.
    ///
    /// A fraction of the plot is a fraction of the drawn window by
    /// construction, so this does not need pixels — and it must not, because
    /// a lens pane in a scripted session is never painted (`Render` for a
    /// `Visualizer` is not called once in a headless-driven run; see the
    /// scenario's own output). When the plot *has* painted, the fraction is
    /// turned into an x first and goes through exactly the mapping the mouse
    /// uses, so the two agree by running the same code rather than by
    /// resembling it. The basis is reported either way.
    pub(super) fn apply_pointer_control(
        &mut self,
        control: LensPointerControl,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let (from, to, alt, is_drag) = match control {
            LensPointerControl::Click(at) => (at, at, false, false),
            LensPointerControl::Drag { from, to, alt } => (from, to, alt, true),
        };
        let painted = *self.timeline_bounds.lock().unwrap();
        let total = self.workbench.read(cx).total_samples();
        if total == 0 {
            let refusal = format!(
                "{} has no material behind it · a pointer position names a moment only in a song",
                self.kind.title()
            );
            self.say(refusal.clone(), cx);
            return Err(refusal);
        }
        let (anchor, release, basis) = match painted {
            Some(bounds) => {
                let origin = bounds.origin.x;
                let width = bounds.size.width;
                let x_of = |fraction: f64| origin + width * (fraction.clamp(0.0, 1.0) as f32);
                let anchor = self.pointer_sample_or_say(x_of(from), cx)?;
                let release = if is_drag {
                    self.pointer_sample_or_say(x_of(to), cx)?
                } else {
                    anchor
                };
                (anchor, release, "painted plot")
            }
            None => {
                let window = self.pointer_window(total);
                let anchor = sample_in_window(from, window, total);
                let release = if is_drag {
                    sample_in_window(to, window, total)
                } else {
                    anchor
                };
                (
                    anchor,
                    release,
                    "the drawn window; this lens has painted no plot in this session",
                )
            }
        };
        self.say(
            format!(
                "{} placed a pointer gesture from {basis} · {anchor} to {release}",
                self.kind.title()
            ),
            cx,
        );
        self.dispatch_pointer(
            TimelineInteractionEvent::PointerDown {
                at: TimelinePoint(anchor),
                loop_policy: LoopEditPolicy::for_range_gesture(alt),
            },
            cx,
        );
        if is_drag {
            self.dispatch_pointer(
                TimelineInteractionEvent::PointerMove {
                    at: TimelinePoint(release),
                },
                cx,
            );
        }
        self.dispatch_pointer(
            TimelineInteractionEvent::PointerUp {
                at: TimelinePoint(release),
            },
            cx,
        );
        Ok(())
    }

    /// Begin a range gesture in this lens. The alt modifier authors a loop,
    /// exactly as it does in the overview: a lens does not get its own
    /// selection vocabulary, it reaches the one authority through the same
    /// events the overview sends (`TimelineInteractionEvent`).
    pub(super) fn lens_pointer_down(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        match self.pointer_sample(event.position.x, false, cx) {
            Ok(Some(sample)) => self.dispatch_pointer(
                TimelineInteractionEvent::PointerDown {
                    at: TimelinePoint(sample),
                    loop_policy: LoopEditPolicy::for_range_gesture(event.modifiers.alt),
                },
                cx,
            ),
            Ok(None) => {}
            Err(refusal) => self.say(refusal, cx),
        }
    }

    pub(super) fn lens_pointer_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !event.dragging() {
            return;
        }
        // A move with no gesture in flight is not this lens's business, and a
        // refusal for every pixel of a stray drag would be noise, not words.
        if self
            .workbench
            .read(cx)
            .timeline_interaction
            .snapshot()
            .pointer
            .is_none()
        {
            return;
        }
        if let Ok(Some(sample)) = self.pointer_sample(event.position.x, true, cx) {
            self.dispatch_pointer(
                TimelineInteractionEvent::PointerMove {
                    at: TimelinePoint(sample),
                },
                cx,
            );
        }
    }

    pub(super) fn lens_pointer_up(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        let Some(gesture) = self
            .workbench
            .read(cx)
            .timeline_interaction
            .snapshot()
            .pointer
        else {
            return;
        };
        let release = match self.pointer_sample(event.position.x, true, cx) {
            Ok(Some(sample)) => sample,
            // A release this lens cannot place still has to end the gesture,
            // or the next click would extend a range nobody is holding. The
            // anchor collapses it into the locate the press already meant.
            Ok(None) => gesture.anchor.get(),
            Err(refusal) => {
                self.say(refusal, cx);
                gesture.anchor.get()
            }
        };
        self.dispatch_pointer(
            TimelineInteractionEvent::PointerUp {
                at: TimelinePoint(release),
            },
            cx,
        );
    }

    fn dispatch_pointer(&mut self, event: TimelineInteractionEvent, cx: &mut Context<Self>) {
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| {
            workbench.dispatch_timeline_event(event, cx);
        });
        cx.notify();
    }

    /// A press with no drag behind it. Kept as the name every lens already
    /// wires so that a lens which has not been given move/up handlers still
    /// locates: press and release at one sample is what the overview does for
    /// a click, so the kernel gives the same answer either way.
    pub(super) fn seek_from_pointer(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let sample = match self.pointer_sample(event.position.x, false, cx) {
            Ok(Some(sample)) => sample,
            Ok(None) => return,
            Err(refusal) => return self.say(refusal, cx),
        };
        let at = TimelinePoint(sample);
        self.dispatch_pointer(
            TimelineInteractionEvent::PointerDown {
                at,
                loop_policy: LoopEditPolicy::for_range_gesture(false),
            },
            cx,
        );
        self.dispatch_pointer(TimelineInteractionEvent::PointerUp { at }, cx);
    }

    pub(super) fn time_span(&self) -> f64 {
        (self.time_end - self.time_start).max(1.0e-6)
    }

    pub(super) fn follow_playhead_if_needed(&mut self, analysis: &Analysis, playhead_seconds: f64) {
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

    pub(super) fn center_time_on_playhead(&mut self, cx: &mut Context<Self>) {
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

    pub(super) fn zoom_time(&mut self, scale: f64, cx: &mut Context<Self>) {
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

    pub(super) fn pan_time(&mut self, amount: f64, cx: &mut Context<Self>) {
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

    pub(super) fn zoom_frequency(&mut self, scale: f32, cx: &mut Context<Self>) {
        let center = (self.frequency_start + self.frequency_end) * 0.5;
        let span = ((self.frequency_end - self.frequency_start) * scale).clamp(0.05, 1.0);
        let start = (center - span * 0.5).clamp(0.0, 1.0 - span);
        self.frequency_start = start;
        self.frequency_end = start + span;
        cx.notify();
    }

    pub(super) fn reset_view(&mut self, cx: &mut Context<Self>) {
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

    pub(super) fn on_toggle(&mut self, _: &TogglePlayback, _: &mut Window, cx: &mut Context<Self>) {
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| workbench.toggle_playback(cx));
    }

    pub(super) fn on_seek_backward(
        &mut self,
        _: &SeekBackward,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| workbench.seek_relative(-5.0, cx));
    }

    pub(super) fn on_seek_forward(
        &mut self,
        _: &SeekForward,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| workbench.seek_relative(5.0, cx));
    }

    pub(super) fn on_view_zoom_in(
        &mut self,
        _: &ViewZoomIn,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.zoom_time(0.5, cx);
    }

    pub(super) fn on_view_zoom_out(
        &mut self,
        _: &ViewZoomOut,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.zoom_time(2.0, cx);
    }

    pub(super) fn on_view_pan_left(
        &mut self,
        _: &ViewPanLeft,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pan_time(-0.7, cx);
    }

    pub(super) fn on_view_pan_right(
        &mut self,
        _: &ViewPanRight,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pan_time(0.7, cx);
    }

    pub(super) fn on_view_fit(&mut self, _: &ViewFit, _: &mut Window, cx: &mut Context<Self>) {
        self.reset_view(cx);
    }

    pub(super) fn render_header(
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
                                .child(self.spectrum_readout()),
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
                        .child(
                            viz_control(
                                "spectral-transform",
                                self.spectrum_settings.transform.label(),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.cycle_transform(cx))),
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
}

impl Drop for Visualizer {
    fn drop(&mut self) {
        if let Some(cancellation) = self.hpss_cancellation.take() {
            cancellation.cancel();
        }
        if let Some(cancellation) = self.rhythm_cancellation.take() {
            cancellation.cancel();
        }
        if let Some(cancellation) = self.loom_cancellation.take() {
            cancellation.cancel();
        }
    }
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
                self.invalidate_background_work();
                self.spectrum_settings.db_ceiling = analysis.spectral_peak_db;
                self.spectrum_settings.db_range = 84.0;
                if let Ok(preferences) = crate::preferences::load() {
                    preferences.apply_spectrum(&mut self.spectrum_settings);
                }
                // The shared field was computed with the FFT defaults; a
                // remembered transform, window, or size needs its own field.
                let baked = SpectrumSettings::default();
                if self.kind == VizKind::Waterfall
                    && (self.spectrum_settings.transform != SpectralTransform::Fft
                        || self.spectrum_settings.window != baked.window
                        || self.spectrum_settings.fft_size != baked.fft_size)
                {
                    self.rerun_spectrum(cx);
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn kernel() -> TimelineInteraction {
        TimelineInteraction::new(
            TimelineControllerId(1),
            10_000,
            TimelinePoint(5_000),
            1_000,
            10,
        )
    }

    /// The events one lens gesture sends, in order: the same three the
    /// overview sends and the same three the `click`/`drag` socket verbs send.
    fn lens_gesture(from: u64, to: u64, alt: bool) -> Vec<TimelineInteractionEvent> {
        let mut events = vec![TimelineInteractionEvent::PointerDown {
            at: TimelinePoint(from),
            loop_policy: LoopEditPolicy::for_range_gesture(alt),
        }];
        if from != to {
            events.push(TimelineInteractionEvent::PointerMove {
                at: TimelinePoint(to),
            });
        }
        events.push(TimelineInteractionEvent::PointerUp {
            at: TimelinePoint(to),
        });
        events
    }

    fn run(kernel: &mut TimelineInteraction, events: Vec<TimelineInteractionEvent>) {
        for event in events {
            kernel.apply(event);
        }
    }

    #[test]
    fn a_lens_press_maps_x_through_the_window_the_lens_is_drawing() {
        // A waterfall drawing the whole song: the middle of the plot is the
        // middle of the song.
        assert_eq!(sample_in_window(0.5, (0.0, 1.0), 10_000), 5_000);
        // The same press in a lens zoomed to the last tenth is in that tenth,
        // not in the middle of the song. This is the bug a viewport-blind
        // mapping would have: the Separation lens is almost never showing the
        // whole song.
        assert_eq!(sample_in_window(0.5, (0.9, 1.0), 10_000), 9_500);
        assert_eq!(sample_in_window(0.0, (0.9, 1.0), 10_000), 9_000);
        assert_eq!(sample_in_window(1.0, (0.9, 1.0), 10_000), 10_000);
        // Out-of-range x is clamped into the window rather than off the song.
        assert_eq!(sample_in_window(-4.0, (0.2, 0.4), 10_000), 2_000);
        assert_eq!(sample_in_window(9.0, (0.2, 0.4), 10_000), 4_000);
    }

    #[test]
    fn a_click_in_a_lens_is_the_same_answer_the_overview_gives() {
        // The claim the wiring rests on: a press with no drag behind it is
        // PointerDown + PointerUp at one sample, which is exactly what the
        // overview does for a click, so the one authority answers both the
        // same way. Run both through the kernel and compare the snapshots.
        let mut overview = kernel();
        overview.apply(TimelineInteractionEvent::PointerDown {
            at: TimelinePoint(3_210),
            loop_policy: LoopEditPolicy::for_range_gesture(false),
        });
        overview.apply(TimelineInteractionEvent::PointerUp {
            at: TimelinePoint(3_210),
        });

        let mut lens = kernel();
        run(&mut lens, lens_gesture(3_210, 3_210, false));

        assert_eq!(lens.snapshot(), overview.snapshot());
        assert_eq!(lens.snapshot().playhead, TimelinePoint(3_210));
        assert_eq!(lens.snapshot().selection.range, None);
    }

    #[test]
    fn a_drag_in_a_lens_selects_and_an_alt_drag_authors_a_loop() {
        let mut selecting = kernel();
        run(&mut selecting, lens_gesture(2_000, 6_000, false));
        let selected = selecting.snapshot();
        assert_eq!(
            selected.selection.range,
            Some(TimelineRange::new(TimelinePoint(2_000), TimelinePoint(6_000)).unwrap())
        );
        assert!(
            !selected.loop_state.enabled,
            "a plain drag with no active loop selects only"
        );

        let mut looping = kernel();
        run(&mut looping, lens_gesture(2_000, 6_000, true));
        let looped = looping.snapshot();
        assert_eq!(looped.selection.range, selected.selection.range);
        assert!(looped.loop_state.enabled, "alt authors and enables a loop");
        assert_eq!(looped.loop_state.range, selected.selection.range);

        // Backwards is the same range: a drag right-to-left is a drag.
        let mut backwards = kernel();
        run(&mut backwards, lens_gesture(6_000, 2_000, false));
        assert_eq!(
            backwards.snapshot().selection.range,
            selected.selection.range
        );
    }

    #[test]
    fn a_pointer_control_names_a_gesture_in_fractions_of_the_painted_plot() {
        assert_eq!(
            LensPointerControl::parse("pointer-click@0.25"),
            Some(LensPointerControl::Click(0.25))
        );
        assert_eq!(
            LensPointerControl::parse("pointer-drag@0.2:0.6"),
            Some(LensPointerControl::Drag {
                from: 0.2,
                to: 0.6,
                alt: false
            })
        );
        assert_eq!(
            LensPointerControl::parse("pointer-alt-drag@0.2:0.6"),
            Some(LensPointerControl::Drag {
                from: 0.2,
                to: 0.6,
                alt: true
            })
        );
        // Anything that is not a gesture stays an unknown control, so the
        // socket keeps naming it rather than silently doing nothing.
        assert_eq!(LensPointerControl::parse("refresh"), None);
        assert_eq!(LensPointerControl::parse("pointer-drag@0.2"), None);
        assert_eq!(LensPointerControl::parse("pointer-click@later"), None);
        assert_eq!(LensPointerControl::parse("pointer-click@inf"), None);
    }
}
