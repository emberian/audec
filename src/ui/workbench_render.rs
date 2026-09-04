//! Workbench GPUI rendering: header, sidebar, inspector, timeline.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

impl Workbench {
    pub(super) fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_playing = self.transport_is_playing();
        let transport_enabled =
            self.audio.is_some() || self.session.read(cx).project_snapshot().is_ok();
        let musical_time = self.project_base_musical_time(cx);
        let title = self
            .analysis()
            .map(|analysis| analysis.title.clone())
            .unwrap_or_else(|| "No material loaded".to_owned());
        let duration = self.audio.as_ref().map_or_else(
            || {
                self.analysis()
                    .map_or(0.0, |analysis| analysis.duration_seconds)
            },
            |audio| {
                let transport = audio.transport();
                transport.format().seconds_at_frame(transport.length())
            },
        );

        div()
            .h(px(54.0))
            .flex_none()
            .flex()
            .items_center()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(
                div()
                    .w(px(220.0))
                    .pl(px(82.0))
                    .pr_4()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .child(div().font_weight(gpui::FontWeight::BOLD).child("audec"))
                    .child(div().text_xs().text_color(rgb(MUTED)).child("reverse DAW")),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        div()
                            .id("seek-back")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .text_color(if transport_enabled {
                                rgb(TEXT)
                            } else {
                                rgb(DIM)
                            })
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| this.seek_relative(-5.0, cx)))
                            .child("−5s"),
                    )
                    .child(
                        div()
                            .id("play-pause")
                            .size(px(34.0))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(if transport_enabled {
                                rgb(TEXT)
                            } else {
                                rgb(BORDER)
                            })
                            .text_color(rgb(BACKGROUND))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_playback(cx)))
                            .child(if self.audio_rendering {
                                "…"
                            } else if is_playing {
                                "❚❚"
                            } else {
                                "▶"
                            }),
                    )
                    .child(
                        div()
                            .id("seek-forward")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .text_color(if transport_enabled {
                                rgb(TEXT)
                            } else {
                                rgb(DIM)
                            })
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| this.seek_relative(5.0, cx)))
                            .child("+5s"),
                    )
                    .child(
                        div()
                            .ml_3()
                            .min_w(px(92.0))
                            .text_sm()
                            .text_color(rgb(CYAN))
                            .child(format!(
                                "{} / {}",
                                format_time(self.playhead_seconds),
                                format_time(duration)
                            )),
                    )
                    .child(
                        div()
                            .ml_2()
                            .flex()
                            .items_center()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .text_xs()
                            .child(
                                div()
                                    .id("tempo-down")
                                    .px_2()
                                    .py_1()
                                    .text_color(if musical_time.is_some() {
                                        rgb(TEXT)
                                    } else {
                                        rgb(DIM)
                                    })
                                    .cursor_pointer()
                                    .hover(|style| style.bg(rgb(BORDER)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.adjust_project_tempo(-1.0, cx)
                                    }))
                                    .child("−"),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .border_l_1()
                                    .border_r_1()
                                    .border_color(rgb(BORDER))
                                    .text_color(rgb(CYAN))
                                    .child(musical_time.map_or_else(
                                        || "— BPM".to_owned(),
                                        |(bpm, _, _)| format!("{bpm:.2} BPM"),
                                    )),
                            )
                            .child(
                                div()
                                    .id("tempo-up")
                                    .px_2()
                                    .py_1()
                                    .text_color(if musical_time.is_some() {
                                        rgb(TEXT)
                                    } else {
                                        rgb(DIM)
                                    })
                                    .cursor_pointer()
                                    .hover(|style| style.bg(rgb(BORDER)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.adjust_project_tempo(1.0, cx)
                                    }))
                                    .child("+"),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .border_l_1()
                                    .border_color(rgb(BORDER))
                                    .text_color(rgb(MUTED))
                                    .child(musical_time.map_or_else(
                                        || "—/—".to_owned(),
                                        |(_, numerator, denominator)| {
                                            format!("{numerator}/{denominator}")
                                        },
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .ml_2()
                            .min_w_0()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .child(if self.audio_rendering {
                                format!("{title} · rendering edits…")
                            } else {
                                title
                            }),
                    ),
            )
            .child(
                div().w(px(220.0)).px_4().flex().justify_end().child(
                    div()
                        .id("open-audio")
                        .px_3()
                        .py_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(rgb(BORDER))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(BORDER)))
                        .on_click(cx.listener(|_this, _, window, cx| {
                            if let Some(handle) = window.window_handle().downcast::<DawWorkspace>()
                            {
                                let _ = handle.update(cx, |workspace, window, cx| {
                                    workspace.request_project_replacement(
                                        ProjectReplacementIntent::ChooseAudio,
                                        window,
                                        cx,
                                    )
                                });
                            }
                        }))
                        .child("Open audio…"),
                ),
            )
    }

    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (album, title, path) = self.analysis().map_or_else(
            || {
                (
                    "SESSION".to_owned(),
                    "Empty".to_owned(),
                    "Choose a FLAC to begin".to_owned(),
                )
            },
            |analysis| {
                (
                    analysis.album.clone(),
                    analysis.title.clone(),
                    analysis.path.display().to_string(),
                )
            },
        );
        let active_sample = self.active_sample_span();
        let sample_workflow_heading =
            if active_sample.is_some_and(|scope| scope.origin() == SampleSpanOrigin::Loop) {
                "MAKE FROM LOOP"
            } else {
                "MAKE FROM SELECTION"
            };
        let active_sample_label = active_sample.map_or_else(
            || "Enable a loop or drag a source range first".to_owned(),
            |scope| self.active_sample_span_label(scope),
        );
        let source_name = sample_workflow_name_stem(&title);
        let sample_instrument =
            sample_workflow_instrument_name(SampleWorkflowCommand::MakeSample, &source_name);
        let kit_instrument =
            sample_workflow_instrument_name(SampleWorkflowCommand::SliceToPads, &source_name);
        let destination_summary = format!(
            "Destinations · Instrument “{sample_instrument}” · Instrument “{kit_instrument}” · beat opens Pattern “{source_name} beat”"
        );

        div()
            .id("workbench-material-rail")
            .w(px(220.0))
            .h_full()
            .flex_none()
            // The workbench can be hosted in an arbitrarily short split pane.
            // Keep the rail inside that allocation and let every command stay
            // reachable instead of painting beneath the window edge.
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.material_rail_scroll)
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL_ALT))
            .p_3()
            .gap_2()
            .child(section_label("MATERIAL"))
            .child(
                div()
                    .mt_1()
                    .p_3()
                    .rounded_md()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_xs().text_color(rgb(MAGENTA)).child(album))
                    .child(div().text_sm().child(title))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(DIM))
                            .line_clamp(2)
                            .child(path),
                    ),
            )
            .child(section_label("LAYERS"))
            .child(layer_row("Stereo waveform", CYAN, true))
            .child(layer_row("Log-frequency energy", MAGENTA, true))
            .child(layer_row("Transient flux", AMBER, true))
            .child(layer_row("Pulse / onset evidence", CYAN, true))
            .child(layer_row("Stereo field", LIME, true))
            .child(div().when(!self.product_shell_hosted, |editors| {
                editors.child(section_label("EDIT / RECONSTRUCT")).child(
                div()
                    .id("open-arrangement-editor")
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(CYAN))
                    .text_color(rgb(CYAN))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(BORDER)).text_color(rgb(TEXT)))
                    .on_click(cx.listener(|this, _, _, cx| this.open_arrangement_editor(cx)))
                    .child("Arrangement editor"),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .id("open-sequencer-editor")
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_sequencer_editor(cx)
                            }))
                            .child("Piano / drums"),
                    )
                    .child(
                        div()
                            .id("open-mixer")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| this.open_mixer(cx)))
                            .child("Mixer"),
                    ),
            )
            .child(
                div()
                    .id("open-automation")
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(BORDER)))
                    .on_click(cx.listener(|this, _, _, cx| this.open_automation(cx)))
                    .child("Automation editor"),
            )
            .child(
                div()
                    .id("open-assets")
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(BORDER)))
                    .on_click(cx.listener(|this, _, _, cx| this.open_assets(cx)))
                    .child("Media pool"),
            )
            }))
            .child(section_label(sample_workflow_heading))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(if active_sample.is_some() {
                        MUTED
                    } else {
                        DIM
                    }))
                    .child(active_sample_label),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .id("selection-one-shot")
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.make_sample_from_active_span(cx)
                            }))
                            .child("Make sample"),
                    )
                    .child(
                        div()
                            .id("selection-chop-pads")
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.slice_active_span_to_kit(cx)
                            }))
                            .child("Slice to kit"),
                    ),
            )
            .child(
                div()
                    .id("selection-make-beat")
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(CYAN))
                    .text_color(rgb(CYAN))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(BORDER)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.make_beat_from_active_span(cx)
                    }))
                    .child("Make beat"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(DIM))
                    .child("Shortcuts · S sample · ⇧S slice · B beat"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(DIM))
                    .child(destination_summary),
            )
            .when_some(self.constructive_status.clone(), |panel, status| {
                panel.child(div().text_xs().text_color(rgb(MUTED)).child(status))
            })
            .child(section_label("OPEN VIEWS"))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .id("open-waterfall")
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_visualizer(VizKind::Waterfall, cx)
                            }))
                            .child("Waterfall"),
                    )
                    .child(
                        div()
                            .id("open-rhythm")
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_visualizer(VizKind::Rhythm, cx)
                            }))
                            .child("Rhythm"),
                    ),
            )
            .child(
                div()
                    .id("open-components")
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(BORDER)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_visualizer(VizKind::Components, cx)
                    }))
                    .child("Components"),
            )
            .child(
                div()
                    .id("open-separation")
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(BORDER)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_visualizer(VizKind::Separation, cx)
                    }))
                    .child("Decompose selected span"),
            )
            .child(
                div()
                    .id("open-loom")
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(BORDER)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_visualizer(VizKind::Loom, cx)
                    }))
                    .child("Loom · reconstruct events"),
            )
            .child(
                div()
                    .mt_auto()
                    .pt_3()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .text_xs()
                    .text_color(rgb(DIM))
                    .child(
                        "Space  play/pause\n← →  seek 5 seconds\n= / −  zoom · ⇧← ⇧→  pan\n0  fit · F  follow\nDrag  select · ⌘L  set loop · L  toggle\n⌘1…⌘5  aspects · ⌘6…⌘9  editors · ⌘B pool",
                    ),
            )
    }

    pub(super) fn render_inspector(&self, cx: &App) -> impl IntoElement {
        let feature = self.current_feature().unwrap_or_default();
        let (tempo, pulse_support, beat) = self.analysis().map_or_else(
            || ("—".to_owned(), "—".to_owned(), "—".to_owned()),
            |analysis| {
                let beat = analysis
                    .rhythm
                    .beat_times
                    .partition_point(|time| *time <= self.playhead_seconds);
                (
                    format!("{:.1} BPM", analysis.rhythm.tempo_bpm),
                    format!("{:.0}%", analysis.rhythm.pulse_contrast * 100.0),
                    format!("{}", beat + 1),
                )
            },
        );
        let metadata = self.analysis().map(|analysis| {
            format!(
                "{} Hz  ·  {}-bit  ·  {} ch",
                analysis.sample_rate, analysis.bits_per_sample, analysis.channels
            )
        });
        let audio_status = self.session.read(cx).audio_status().clone();
        let audio_runtime = audio_status.scoped_audition.map_or_else(
            || format!("{:?}", audio_status.render),
            |audition| {
                format!(
                    "{:?} · {:?} {:?}",
                    audio_status.render, audition.subject, audition.phase
                )
            },
        );
        let audio_backend = self.audio.as_ref().map_or_else(
            || "—".to_owned(),
            |audio| format!("{:?}", audio.backend_kind()),
        );
        let selection = self
            .timeline_selection
            .filter(|range| !range.is_empty())
            .map_or_else(
                || "—".to_owned(),
                |range| {
                    format!(
                        "{} — {}",
                        format_time(self.seconds_for_sample(range.start.get().max(0) as u64)),
                        format_time(self.seconds_for_sample(range.end.get().max(0) as u64))
                    )
                },
            );
        let loop_status = self.loop_range.map_or_else(
            || "—".to_owned(),
            |range| {
                format!(
                    "{} {} — {}",
                    if self.loop_enabled { "ON" } else { "OFF" },
                    format_time(self.seconds_for_sample(range.start.get().max(0) as u64)),
                    format_time(self.seconds_for_sample(range.end.get().max(0) as u64))
                )
            },
        );
        div()
            .id("workbench-inspector-rail")
            .w(px(220.0))
            .h_full()
            .flex_none()
            // Mirrors the material rail: inspector metadata and diagnostics
            // remain bounded and scrollable in short/tiled workspaces.
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.inspector_rail_scroll)
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL_ALT))
            .p_3()
            .gap_2()
            .child(section_label("AT PLAYHEAD"))
            .child(metric("PULSE", tempo, CYAN))
            .child(metric("PULSE CONTRAST", pulse_support, CYAN))
            .child(metric("BEAT", beat, CYAN))
            .child(metric(
                "DOMINANT",
                format_frequency(feature.dominant_hz),
                MAGENTA,
            ))
            .child(metric(
                "LOUDNESS",
                format!("{:.0}%", feature.loudness * 100.0),
                CYAN,
            ))
            .child(metric(
                "BRIGHTNESS",
                format!("{:.0}%", feature.brightness * 100.0),
                AMBER,
            ))
            .child(metric(
                "TRANSIENT",
                format!("{:.0}%", feature.flux * 100.0),
                AMBER,
            ))
            .child(metric(
                "STEREO WIDTH",
                format!("{:.0}%", feature.stereo_width * 100.0),
                LIME,
            ))
            .child(metric(
                "CORRELATION",
                format!("{:+.2}", feature.correlation),
                LIME,
            ))
            .child(section_label("EDIT RANGE"))
            .child(metric("SELECTION", selection, CYAN))
            .child(metric("LOOP", loop_status, AMBER))
            .child(section_label("PROJECT AUDIO"))
            .child(metric("BACKEND", audio_backend, CYAN))
            .child(metric("RUNTIME", audio_runtime, LIME))
            .when_some(self.audio_device_status.clone(), |this, status| {
                this.child(div().text_xs().text_color(rgb(DIM)).child(status))
            })
            .when_some(metadata, |this, metadata| {
                this.child(
                    div()
                        .mt_auto()
                        .pt_3()
                        .border_t_1()
                        .border_color(rgb(BORDER))
                        .text_xs()
                        .text_color(rgb(DIM))
                        .child(metadata),
                )
            })
            .when_some(self.audio_error.clone(), |this, error| {
                this.child(div().text_xs().text_color(rgb(MAGENTA)).child(error))
            })
            .when_some(self.project_io_status.label(), |this, status| {
                this.child(div().text_xs().text_color(rgb(AMBER)).child(status))
            })
    }

    pub(super) fn render_timeline(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        match &self.state {
            ProjectState::Empty => empty_state(
                "Sound becomes material here.",
                "Open a FLAC to build a persistent waveform, log-frequency map, and perceptual feature lanes.",
            ),
            ProjectState::Loading(path) => empty_state(
                "Decompiling audio…",
                &format!("Reading {}, then projecting it into inspectable layers.", path.display()),
            ),
            ProjectState::Failed(error) => empty_state("The material would not open.", error),
            ProjectState::Ready(analysis) => {
                let fraction = self.visible_playhead_fraction();
                let spectrogram = self.spectrogram.clone().unwrap();
                let spectrogram_detail = self.spectrogram_detail.clone();
                let spectrogram_refining = self.spectrogram_refining;
                let total_samples = self.timeline_viewport.total_samples.max(1);
                let (time_start, time_end) = self.visible_seconds();
                let normalized_start =
                    self.timeline_viewport.start_sample as f64 / total_samples as f64;
                let normalized_end =
                    self.timeline_viewport.end_sample as f64 / total_samples as f64;
                let waveform = analysis.waveform_range(normalized_start, normalized_end, 2_048);
                let features = slice_visible(
                    &analysis.features,
                    normalized_start,
                    normalized_end,
                );
                let rhythm = analysis.rhythm.clone();
                let timeline_bounds = self.timeline_bounds.clone();
                let selection = self
                    .timeline_selection
                    .and_then(|range| range_fractions(range, self.timeline_viewport));
                let loop_range = self
                    .loop_range
                    .and_then(|range| range_fractions(range, self.timeline_viewport));
                let loop_enabled = self.loop_enabled;
                let loop_label = self.loop_range.map_or_else(
                    || "NO LOOP".to_owned(),
                    |range| {
                        format!(
                            "{} — {}",
                            format_time(self.seconds_for_sample(range.start.get().max(0) as u64)),
                            format_time(self.seconds_for_sample(range.end.get().max(0) as u64))
                        )
                    },
                );
                let material_scope_label = self
                    .active_sample_span()
                    .map(|scope| self.active_sample_span_label(scope));
                let viewport = self.timeline_viewport;
                let follow = self.timeline_follow;

                div()
                    .id("arrangement-timeline")
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .relative()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .bg(rgb(BACKGROUND))
                    .cursor_crosshair()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.begin_timeline_selection(event, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_move(cx.listener(
                        |this, event: &MouseMoveEvent, _, cx| {
                            this.extend_timeline_selection(event, cx);
                        },
                    ))
                    .capture_any_mouse_up(cx.listener(
                        |this, event: &MouseUpEvent, _, cx| {
                            this.end_timeline_selection(event, cx);
                        },
                    ))
                    .on_scroll_wheel(cx.listener(
                        |this, event: &ScrollWheelEvent, window, cx| {
                            let Some(bounds) = *this.timeline_bounds.lock().unwrap() else {
                                return;
                            };
                            if !bounds.contains(&event.position) {
                                return;
                            }
                            let delta = event.delta.pixel_delta(window.line_height());
                            let command_zoom =
                                event.modifiers.secondary() || event.modifiers.control;
                            if command_zoom {
                                let wheel = if delta.y.abs() >= delta.x.abs() {
                                    delta.y
                                } else {
                                    delta.x
                                };
                                let amount = f64::from(wheel / px(180.0));
                                if amount.abs() > 0.0001 {
                                    if let Some(anchor) =
                                        this.sample_from_x(event.position.x, true)
                                    {
                                        this.zoom_timeline(anchor, amount.exp(), cx);
                                        cx.stop_propagation();
                                    }
                                }
                            } else if delta.x.abs() > px(0.01) || event.modifiers.shift {
                                let wheel = if delta.x.abs() > px(0.01) {
                                    delta.x
                                } else {
                                    delta.y
                                };
                                let amount = f64::from(wheel / px(480.0));
                                if amount.abs() > 0.0001 {
                                    this.pan_timeline(-amount, cx);
                                    cx.stop_propagation();
                                }
                            }
                        },
                    ))
                    .child(arrangement_ruler(
                        time_start,
                        time_end,
                        viewport,
                        follow,
                        loop_enabled,
                        loop_label,
                        material_scope_label,
                        cx,
                    ))
                    .child(arrangement_lane(
                        "STEREO AMPLITUDE",
                        "retained PCM · L / R",
                        px(100.0),
                        waveform_plot(
                            waveform,
                            fraction,
                            Arc::clone(&self.timeline_waveform_geometry),
                            WaveformRenderKey::samples(
                                0,
                                self.document_epoch.get(),
                                viewport.start_sample,
                                viewport.end_sample,
                            ),
                        ),
                    ))
                    .child(arrangement_lane(
                        "LOG-FREQUENCY ENERGY",
                        if spectrogram_refining {
                            "32.7 Hz — 16 kHz · refining visible resolution"
                        } else {
                            "32.7 Hz — 16 kHz · viewport-native detail"
                        },
                        px(250.0),
                        div()
                            .relative()
                            .size_full()
                            .overflow_hidden()
                            .child(if let Some(detail) = spectrogram_detail {
                                img(detail)
                                    .size_full()
                                    .object_fit(ObjectFit::Fill)
                                    .into_any_element()
                            } else {
                                cropped_spectrogram(
                                    spectrogram,
                                    normalized_start,
                                    normalized_end,
                                    0.0,
                                    1.0,
                                )
                                .into_any_element()
                            }),
                    ))
                    .child(arrangement_lane(
                        "PULSE / ONSETS",
                        "low · mid · high evidence",
                        px(92.0),
                        rhythm_plot(rhythm, time_start, time_end, fraction),
                    ))
                    .child(arrangement_lane(
                        "LOUDNESS / BRIGHTNESS",
                        "cyan energy · amber centroid",
                        px(72.0),
                        dual_feature_plot(
                            features.clone(),
                            fraction,
                            |feature| feature.loudness,
                            |feature| feature.brightness,
                            rgba(0x50d8d7cc),
                            rgba(0xf6b76099),
                        )
                    ))
                    .child(arrangement_lane(
                        "TRANSIENT FLUX",
                        "positive spectral change",
                        px(64.0),
                        feature_plot(
                            features.clone(),
                            fraction,
                            |feature| feature.flux,
                            rgba(0xf6b760cc),
                        ),
                    ))
                    .child(arrangement_lane(
                        "STEREO WIDTH",
                        "mid / side energy ratio",
                        px(64.0),
                        feature_plot(
                            features,
                            fraction,
                            |feature| feature.stereo_width,
                            rgba(0xa7d877cc),
                        ),
                    ))
                    .child(arrangement_overlay(
                        timeline_bounds,
                        fraction,
                        selection,
                        loop_range,
                        loop_enabled,
                    ))
                    .into_any_element()
            }
        }
    }
}

impl Focusable for Workbench {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workbench {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("Audec")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .text_sm()
            .child(self.render_header(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .when(!self.product_shell_hosted, |row| {
                        row.child(self.render_sidebar(cx))
                    })
                    .child(self.render_timeline(cx))
                    .when(!self.product_shell_hosted, |row| {
                        row.child(self.render_inspector(cx))
                    }),
            )
    }
}
