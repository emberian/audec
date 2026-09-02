//! Shared Visualizer lifecycle, viewport gestures, header, and GPUI trait impls.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::lens_loom::update_loom_render;
use super::*;

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
        let (spectrum_settings, spectrogram_source, playhead, duration) = {
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
            local_spectrogram: None,
            local_spectral_db: None,
            spectrogram_source,
            spectrum_generation: 0,
            spectrum_transforming: false,
            hpss_state: HpssViewState::Idle,
            hpss_generation: 0,
            hpss_cancellation: None,
            rhythm_state: RhythmViewState::Idle,
            rhythm_generation: 0,
            rhythm_cancellation: None,
            loom_state: LoomViewState::Idle,
            loom_generation: 0,
            loom_cancellation: None,
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

    pub(super) fn seek_from_pointer(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
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
                                .child(format!(
                                    "{} {}{}",
                                    match self.spectrum_settings.transform {
                                        SpectralTransform::Fft => {
                                            self.spectrum_settings.fft_size.to_string()
                                        }
                                        SpectralTransform::ConstantQ => "24/oct".to_string(),
                                    },
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
