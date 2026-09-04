//! Overview timeline gestures: selection, loop, zoom, pan, and spectrogram detail.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

impl Workbench {
    pub(super) fn refresh_spectrogram_detail(&mut self, cx: &mut Context<Self>) {
        let target_width = self
            .timeline_bounds
            .lock()
            .unwrap()
            .as_ref()
            .map(|bounds| f32::from(bounds.size.width).round() as usize)
            .unwrap_or(1_200)
            .clamp(256, 4_096);
        let Some((mono, source, db_ceiling)) = self.analysis().map(|analysis| {
            let frame_count = analysis.waveform_pyramid.frame_count() as u64;
            (
                Arc::clone(&analysis.mono_pcm),
                SourceStamp {
                    content: stable_source_id(
                        &analysis.path.to_string_lossy(),
                        frame_count,
                        analysis.sample_rate,
                    ),
                    revision: 0,
                    generation: 0,
                    sample_rate: analysis.sample_rate,
                    frame_count,
                },
                analysis.spectral_peak_db,
            )
        }) else {
            return;
        };
        let request = SpectralTileRequest {
            source,
            frames: SpectralFrameRange::new(
                self.timeline_viewport.start_sample,
                self.timeline_viewport.end_sample,
            ),
            target_pixel_width: target_width,
            frequencies: FrequencyRange::logarithmic(MIN_FREQUENCY, MAX_FREQUENCY),
            recipe: SpectralRecipe {
                fft_size: 8_192,
                min_fft_size: 256,
                max_window_columns: 4,
                window: crate::settings::WindowFunction::Hann,
                frequency_bins: 256,
                db_ceiling,
                db_range: 84.0,
            },
        };
        let key = SpectralTilePlanner::default().plan(request).final_key;
        if self.spectrogram_detail_key == Some(key) && self.spectrogram_detail.is_some() {
            return;
        }

        if let Some(cancellation) = self.spectrogram_cancellation.take() {
            cancellation.cancel();
        }
        let cancellation = SpectralCancellation::default();
        self.spectrogram_cancellation = Some(cancellation.clone());
        self.spectrogram_request = Some(key);
        self.spectrogram_detail = None;
        self.spectrogram_detail_key = None;
        self.spectrogram_refining = true;

        let task = cx.background_spawn(async move {
            let tile = compute_spectral_tile(&mono, key, &cancellation)
                .map_err(|error| error.to_string())?;
            let png = encode_spectrogram_field(
                &tile.db,
                tile.scalar.width,
                tile.scalar.height,
                key.db_ceiling,
                key.db_range,
            )
            .map_err(|error| format!("encoding spectral tile: {error:#}"))?;
            Ok::<_, String>((key, png))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                // The tile is truth only while it is still the tile wanted:
                // the requested key is the identity, so no counter has to be
                // kept in step with it.
                if this.spectrogram_request != Some(key) {
                    return;
                }
                this.spectrogram_refining = false;
                match result {
                    Ok((key, png)) => {
                        this.spectrogram_detail =
                            Some(Arc::new(Image::from_bytes(ImageFormat::Png, png)));
                        this.spectrogram_detail_key = Some(key);
                    }
                    Err(error) if error != "spectral tile computation was cancelled" => {
                        eprintln!("refining workbench spectrum: {error}");
                    }
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn sample_from_x(&self, x: Pixels, clamp: bool) -> Option<u64> {
        let bounds = (*self.timeline_bounds.lock().unwrap())?;
        if bounds.size.width <= px(0.0) {
            return None;
        }
        let raw_fraction = f64::from((x - bounds.origin.x) / bounds.size.width);
        if !clamp && !(0.0..=1.0).contains(&raw_fraction) {
            return None;
        }
        Some(
            self.timeline_viewport
                .sample_at_fraction(raw_fraction.clamp(0.0, 1.0)),
        )
    }

    pub(super) fn seek_to_sample(&mut self, sample: u64, cx: &mut Context<Self>) {
        self.seek_to(self.seconds_for_sample(sample), cx);
    }

    pub(super) fn begin_timeline_selection(
        &mut self,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(sample) = self.sample_from_x(event.position.x, false) else {
            return;
        };
        self.dispatch_timeline_event(
            TimelineInteractionEvent::PointerDown {
                at: TimelinePoint(sample),
                loop_policy: LoopEditPolicy::for_range_gesture(event.modifiers.alt),
            },
            cx,
        );
    }

    pub(super) fn extend_timeline_selection(
        &mut self,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        if !event.dragging() {
            return;
        }
        let Some(sample) = self.sample_from_x(event.position.x, true) else {
            return;
        };
        self.dispatch_timeline_event(
            TimelineInteractionEvent::PointerMove {
                at: TimelinePoint(sample),
            },
            cx,
        );
    }

    pub(super) fn end_timeline_selection(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.timeline_interaction.snapshot().pointer.is_none() {
            return;
        }
        let release = self
            .sample_from_x(event.position.x, true)
            .unwrap_or_else(|| {
                self.timeline_interaction
                    .snapshot()
                    .pointer
                    .unwrap()
                    .anchor
                    .get()
            });
        self.dispatch_timeline_event(
            TimelineInteractionEvent::PointerUp {
                at: TimelinePoint(release),
            },
            cx,
        );
    }

    pub(super) fn publish_overview_semantic_selection(
        &mut self,
        range: Option<SampleRange>,
        cx: &mut Context<Self>,
    ) {
        let mut selection = self.session.read(cx).selection().selection.clone();
        selection.time = range.map(|range| FrameSpan {
            start: range.start.get(),
            end: range.end.get(),
        });
        selection.aspect = selection.time.map(Aspect::Time);
        selection.signal = Some(self.timeline_signal);
        let session = self.session.clone();
        if let Err(error) = session.update(cx, |session, _| {
            self.pane_session_binding.publish_semantic_selection(
                session,
                WorkspaceViewId::TRACK_OVERVIEW,
                selection,
            )
        }) {
            self.constructive_status =
                Some(format!("Timeline selection was not published · {error}"));
        }
    }

    pub(super) fn publish_arrangement_selection(
        &mut self,
        source: WorkspaceViewId,
        intent: SelectionIntent,
        cx: &mut Context<Self>,
    ) {
        let Ok(snapshot) = self.session.read(cx).project_snapshot().cloned() else {
            return;
        };
        let arrangement = &snapshot.project.state().domains.arrangement;
        let mut selection = self.session.read(cx).selection().selection.clone();
        let mut primary_clip = None;
        match intent {
            SelectionIntent::Clips { ids, primary, mode } => {
                apply_project_id_selection(&mut selection.clips, ids, mode);
                primary_clip = primary.filter(|clip| selection.clips.contains(clip));
            }
            SelectionIntent::Marquee {
                range,
                tracks,
                mode,
            } => {
                let ids = arrangement
                    .clips
                    .values()
                    .filter(|clip| {
                        (tracks.is_empty() || tracks.contains(&clip.track_id))
                            && clip.placement.intersects(range)
                    })
                    .map(|clip| clip.id)
                    .collect();
                apply_project_id_selection(&mut selection.clips, ids, mode);
            }
            SelectionIntent::ClearObjects => selection.clear_objects(),
        }
        primary_clip = primary_clip.or_else(|| selection.clips.iter().next().copied());
        selection.primary = primary_clip.map(SelectableId::Clip);
        selection.tracks = selection
            .clips
            .iter()
            .filter_map(|clip| arrangement.clip(*clip).map(|clip| clip.track_id))
            .collect();
        selection.time = selected_arrangement_frame_span(arrangement, &selection.clips);
        selection.aspect = selection.time.map(Aspect::Time);
        let session = self.session.clone();
        if let Err(error) = session.update(cx, |session, _| {
            if let Some(primary) = primary_clip {
                let guard = session.current_selection_guard()?;
                selection.objects = ObjectSelection::guarded(
                    ObjectRef::AudioClip(primary),
                    selection
                        .clips
                        .iter()
                        .copied()
                        .filter(|clip| *clip != primary)
                        .map(ObjectRef::AudioClip),
                    guard,
                    SelectionProvenance {
                        source: SelectionSource::Arrangement,
                        source_view: Some(source),
                    },
                );
                session.replace_guarded_selection(selection.clone())?;
            } else {
                selection.objects = ObjectSelection::default();
            }
            self.pane_session_binding
                .publish_semantic_selection(session, source, selection)
        }) {
            self.constructive_status =
                Some(format!("Arrangement selection was not published · {error}"));
        }
    }

    pub(super) fn zoom_timeline(&mut self, anchor: u64, scale: f64, cx: &mut Context<Self>) {
        self.dispatch_timeline_event(
            TimelineInteractionEvent::ZoomAround {
                anchor: TimelinePoint(anchor),
                scale,
            },
            cx,
        );
    }

    pub(super) fn pan_timeline(&mut self, fraction: f64, cx: &mut Context<Self>) {
        self.dispatch_timeline_event(TimelineInteractionEvent::PanFraction(fraction), cx);
    }

    pub(super) fn fit_timeline(&mut self, cx: &mut Context<Self>) {
        self.dispatch_timeline_event(TimelineInteractionEvent::Fit, cx);
    }

    pub(super) fn follow_timeline(&mut self, cx: &mut Context<Self>) {
        self.dispatch_timeline_event(
            TimelineInteractionEvent::SetFollow(TimelineFollowState::Playhead {
                margin_fraction: 0.16,
            }),
            cx,
        );
    }

    pub(super) fn set_loop_from_selection(&mut self, cx: &mut Context<Self>) {
        self.apply_project_transport_command(ProjectTransportCommand::SetLoopFromSelection, cx);
        let effects = self
            .timeline_interaction
            .apply(TimelineInteractionEvent::SetLoopFromSelection)
            .into_iter()
            .filter(|effect| !matches!(effect, TimelineEffect::Transport(_)))
            .collect();
        self.apply_timeline_effects(effects, cx);
    }

    pub(super) fn toggle_loop(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.audio_controller.transport_session().snapshot();
        let command = if snapshot.transport.loop_region.is_some() {
            Some(ProjectTransportCommand::SetLoopEnabled(
                !snapshot.transport.loop_enabled,
            ))
        } else if snapshot.selection.is_some() {
            Some(ProjectTransportCommand::SetLoopFromSelection)
        } else {
            None
        };
        if let Some(command) = command {
            self.apply_project_transport_command(command, cx);
            let status = self.audio_controller.transport_session().snapshot();
            let loop_state = TimelineLoopState {
                range: status.transport.loop_region.and_then(|range| {
                    TimelineRange::new(TimelinePoint(range.start.0), TimelinePoint(range.end.0))
                }),
                enabled: status.transport.loop_enabled,
            };
            let _ = self
                .timeline_interaction
                .apply(TimelineInteractionEvent::ReplaceLoop(loop_state));
            self.sync_timeline_presentation();
            self.sync_arrangement_timeline_views(cx);
        } else {
            self.audio_error = Some("Select a range before enabling loop".into());
        }
        cx.notify();
    }
}
