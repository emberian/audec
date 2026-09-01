//! Transport commands, timeline effect application, and playhead queries.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

impl Workbench {
    pub(super) fn toggle_playback(&mut self, cx: &mut Context<Self>) {
        let event = if self
            .audio_controller
            .transport_session()
            .snapshot()
            .transport
            .mode
            == TransportMode::Playing
        {
            TimelineInteractionEvent::PauseRequested
        } else {
            TimelineInteractionEvent::PlayRequested
        };
        self.dispatch_timeline_event(event, cx);
    }

    pub(super) fn seek_to(&mut self, seconds: f64, cx: &mut Context<Self>) {
        let duration = self
            .analysis()
            .map_or(0.0, |analysis| analysis.duration_seconds);
        let seconds = seconds.clamp(0.0, duration);
        self.playhead_seconds = seconds;
        if let Some(audio) = &self.audio {
            self.preview_controller.cancel_all(audio);
            self.pad_preview_tickets.clear();
            match audio.transport().format().frame_at_seconds(seconds) {
                Ok(frame) => {
                    if let Err(error) = self
                        .audio_controller
                        .apply_transport_intent(audio, ProjectTransportIntent::Seek(frame))
                    {
                        self.audio_error = Some(format!("{error:#}"));
                    }
                }
                Err(error) => self.audio_error = Some(error.to_string()),
            }
        }
        let playing = self.transport_is_playing();
        self.sync_arrangement_playhead(playing, cx);
        self.sync_pattern_placement_frame(cx);
        cx.notify();
    }

    pub(super) fn seek_relative(&mut self, delta: f64, cx: &mut Context<Self>) {
        self.seek_to(self.playhead_seconds + delta, cx);
    }

    pub(super) fn project_base_musical_time(&self, cx: &App) -> Option<(f64, u16, u16)> {
        self.session
            .read(cx)
            .project_snapshot()
            .ok()
            .map(|snapshot| {
                let tempo_map = snapshot.project.state().domains.sequencer.tempo_map();
                let meter = tempo_map.meter_at(crate::sequencer::BeatTime::ZERO);
                (
                    tempo_map.tempo_at(crate::sequencer::BeatTime::ZERO).bpm(),
                    meter.numerator,
                    meter.denominator,
                )
            })
    }

    pub(super) fn adjust_project_tempo(&mut self, delta_bpm: f64, cx: &mut Context<Self>) {
        let intent = {
            let session = self.session.read(cx);
            session.project_snapshot().ok().map(|snapshot| {
                let current_bpm = snapshot
                    .project
                    .state()
                    .domains
                    .sequencer
                    .tempo_map()
                    .tempo_at(crate::sequencer::BeatTime::ZERO)
                    .bpm();
                AdoptTempoIntent {
                    expected_project_revision: snapshot.revisions().aggregate,
                    bpm: (current_bpm + delta_bpm).max(1.0),
                    source: None,
                }
            })
        };
        let Some(intent) = intent else {
            self.constructive_status = Some("Project tempo is unavailable".into());
            cx.notify();
            return;
        };

        let adjustment = self
            .session
            .update(cx, |session, _| session.adopt_project_tempo(intent));
        self.constructive_status = Some(match adjustment {
            Ok(TempoAdoptionOutcome::Published { publication, .. }) => format!(
                "Project tempo {:.3} → {:.3} BPM · undoable",
                publication.previous_bpm, publication.adopted_bpm
            ),
            Ok(TempoAdoptionOutcome::Unchanged(publication)) => {
                format!(
                    "Project tempo is already {:.3} BPM",
                    publication.adopted_bpm
                )
            }
            Err(error) => format!("Tempo adjustment failed · {error}"),
        });
        cx.notify();
    }

    pub(super) fn total_samples(&self) -> u64 {
        self.analysis()
            .map_or(0, |analysis| analysis.waveform_pyramid.frame_count() as u64)
    }

    pub(super) fn playhead_sample(&self) -> u64 {
        let Some(analysis) = self.analysis() else {
            return 0;
        };
        (self.playhead_seconds.max(0.0) * f64::from(analysis.sample_rate))
            .round()
            .clamp(0.0, self.total_samples() as f64) as u64
    }

    pub(super) fn dispatch_timeline_event(
        &mut self,
        event: TimelineInteractionEvent,
        cx: &mut Context<Self>,
    ) {
        let effects = self.timeline_interaction.apply(event);
        self.apply_timeline_effects(effects, cx);
    }

    pub(super) fn apply_timeline_effects(
        &mut self,
        effects: Vec<TimelineEffect>,
        cx: &mut Context<Self>,
    ) {
        let selection = effects.iter().find_map(|effect| match effect {
            TimelineEffect::SelectionChanged(selection) => selection.range,
            _ => None,
        });
        let authored_loop = effects.iter().find_map(|effect| match effect {
            TimelineEffect::LoopChanged(loop_state) if loop_state.enabled => loop_state.range,
            _ => None,
        });
        let atomic_selection_loop = selection.filter(|range| Some(*range) == authored_loop);
        let collapsed_seek = selection.is_none()
            && effects.iter().any(|effect| {
                matches!(
                    effect,
                    TimelineEffect::Transport(TimelineTransportEffect::Seek { .. })
                )
            });
        if let Some(range) = atomic_selection_loop {
            if let Ok(range) = FrameRange::new(
                ProjectFrame(range.start.get()),
                ProjectFrame(range.end.get()),
            ) {
                self.apply_project_transport_command(
                    ProjectTransportCommand::ReplaceSelectionAndLoop(range),
                    cx,
                );
            }
        }
        for effect in effects {
            match effect {
                TimelineEffect::SelectionPreview(range) => {
                    self.timeline_selection = range.map(sample_range_from_timeline);
                    cx.notify();
                }
                TimelineEffect::SelectionChanged(selection) => {
                    self.timeline_selection = selection.range.map(sample_range_from_timeline);
                    self.publish_overview_semantic_selection(self.timeline_selection, cx);
                    if atomic_selection_loop.is_none() {
                        let selection = selection.range.and_then(|range| {
                            FrameRange::new(
                                ProjectFrame(range.start.get()),
                                ProjectFrame(range.end.get()),
                            )
                            .ok()
                        });
                        self.apply_project_transport_command(
                            ProjectTransportCommand::ReplaceSelection(selection),
                            cx,
                        );
                    }
                    cx.notify();
                }
                TimelineEffect::CursorChanged(_) => {}
                TimelineEffect::LoopChanged(loop_state) => {
                    self.loop_range = loop_state.range.map(sample_range_from_timeline);
                    self.loop_enabled = loop_state.enabled;
                    cx.notify();
                }
                TimelineEffect::Transport(effect) => {
                    let redundant_atomic_transport = match effect {
                        TimelineTransportEffect::SetLoop(_) => atomic_selection_loop.is_some(),
                        TimelineTransportEffect::Seek { to, .. } => {
                            atomic_selection_loop.is_some_and(|range| to == range.start)
                        }
                        _ => false,
                    };
                    let collapsed_click_loop_update =
                        collapsed_seek && matches!(effect, TimelineTransportEffect::SetLoop(_));
                    if !redundant_atomic_transport && !collapsed_click_loop_update {
                        self.apply_timeline_transport_effect(effect, cx)
                    }
                }
                TimelineEffect::ViewportChanged { owner, viewport }
                    if owner == TimelineControllerId(WorkspaceViewId::TRACK_OVERVIEW.0) =>
                {
                    self.timeline_viewport = viewport;
                    self.refresh_spectrogram_detail(cx);
                    cx.notify();
                }
                TimelineEffect::ViewportChanged { .. } => {}
                TimelineEffect::FollowChanged(follow) => {
                    self.timeline_follow = !matches!(follow, TimelineFollowState::Off);
                    self.apply_project_transport_command(
                        ProjectTransportCommand::SetFollow(if self.timeline_follow {
                            ProjectTransportFollowPolicy::Playhead
                        } else {
                            ProjectTransportFollowPolicy::Off
                        }),
                        cx,
                    );
                    cx.notify();
                }
            }
        }
        self.sync_arrangement_timeline_views(cx);
    }

    pub(super) fn apply_project_transport_command(
        &mut self,
        command: ProjectTransportCommand,
        cx: &mut Context<Self>,
    ) {
        let Some(audio) = self.audio.as_ref() else {
            return;
        };
        self.preview_controller.cancel_all(audio);
        self.pad_preview_tickets.clear();
        if let Err(error) = self
            .audio_controller
            .apply_transport_command(audio, command)
        {
            self.audio_error = Some(error.to_string());
        }
        self.publish_audio_status(cx);
    }

    pub(super) fn apply_timeline_transport_effect(
        &mut self,
        effect: TimelineTransportEffect,
        cx: &mut Context<Self>,
    ) {
        let Some(audio) = self.audio.as_ref() else {
            return;
        };
        self.preview_controller.cancel_all(audio);
        self.pad_preview_tickets.clear();
        let intent = match effect {
            TimelineTransportEffect::SetLoop(loop_state) => {
                if let Some(range) = loop_state.range {
                    let Ok(range) = FrameRange::new(
                        ProjectFrame(range.start.get()),
                        ProjectFrame(range.end.get()),
                    ) else {
                        self.audio_error = Some("Loop range is empty".into());
                        return;
                    };
                    ProjectTransportIntent::SetLoop {
                        range,
                        enabled: loop_state.enabled,
                    }
                } else {
                    ProjectTransportIntent::ClearLoop
                }
            }
            TimelineTransportEffect::Seek { to, .. } => {
                ProjectTransportIntent::Seek(ProjectFrame(to.get()))
            }
            TimelineTransportEffect::Play => ProjectTransportIntent::Play,
            TimelineTransportEffect::Pause => ProjectTransportIntent::Pause,
            TimelineTransportEffect::Stop => ProjectTransportIntent::Stop,
        };
        if let Err(error) = self.audio_controller.apply_transport_intent(audio, intent) {
            self.audio_error = Some(error.to_string());
        }
        self.publish_audio_status(cx);
        cx.notify();
    }

    pub(super) fn observe_timeline_audio(
        &mut self,
        audio: &ProjectAudioStatus,
        cx: &mut Context<Self>,
    ) {
        let loop_state = TimelineLoopState {
            range: audio.transport.loop_region.and_then(|range| {
                TimelineRange::new(TimelinePoint(range.start.0), TimelinePoint(range.end.0))
            }),
            enabled: audio.transport.loop_enabled,
        };
        let _ = self
            .timeline_interaction
            .apply(TimelineInteractionEvent::ReplaceLoop(loop_state));
        let effects =
            self.timeline_interaction
                .apply(TimelineInteractionEvent::TransportObserved {
                    playhead: TimelinePoint(audio.transport.frame.0),
                    mode: timeline_playback_mode(audio.transport.mode),
                });
        self.sync_timeline_presentation();
        // Only pane-local follow/viewport effects are applied from a transport
        // observation. The project-audio publication is already authoritative
        // and must not be echoed back into the host.
        for effect in effects {
            match effect {
                TimelineEffect::ViewportChanged { owner, viewport }
                    if owner == TimelineControllerId(WorkspaceViewId::TRACK_OVERVIEW.0) =>
                {
                    self.timeline_viewport = viewport;
                    self.refresh_spectrogram_detail(cx);
                }
                TimelineEffect::FollowChanged(follow) => {
                    self.timeline_follow = !matches!(follow, TimelineFollowState::Off)
                }
                _ => {}
            }
        }
    }

    pub(super) fn sync_timeline_presentation(&mut self) {
        let snapshot = self.timeline_interaction.snapshot();
        self.timeline_viewport = snapshot.viewport;
        self.timeline_follow = !matches!(snapshot.follow, TimelineFollowState::Off);
        self.timeline_selection = snapshot.selection.range.map(sample_range_from_timeline);
        self.loop_range = snapshot.loop_state.range.map(sample_range_from_timeline);
        self.loop_enabled = snapshot.loop_state.enabled;
    }

    pub(super) fn seconds_for_sample(&self, sample: u64) -> f64 {
        self.analysis().map_or(0.0, |analysis| {
            sample.min(self.total_samples()) as f64 / f64::from(analysis.sample_rate)
        })
    }

    pub(super) fn visible_seconds(&self) -> (f64, f64) {
        (
            self.seconds_for_sample(self.timeline_viewport.start_sample),
            self.seconds_for_sample(self.timeline_viewport.end_sample),
        )
    }

    pub(super) fn analysis(&self) -> Option<&Analysis> {
        match &self.state {
            ProjectState::Ready(analysis) => Some(analysis),
            _ => None,
        }
    }

    pub(super) fn transport_is_playing(&self) -> bool {
        self.audio_controller
            .transport_session()
            .snapshot()
            .transport
            .mode
            == TransportMode::Playing
    }

    pub(super) fn playhead_fraction(&self) -> f32 {
        self.analysis()
            .map(|analysis| {
                (self.playhead_seconds / analysis.duration_seconds.max(f64::EPSILON)) as f32
            })
            .unwrap_or(0.0)
            .clamp(0.0, 1.0)
    }

    pub(super) fn visible_playhead_fraction(&self) -> f32 {
        let sample = self.playhead_sample();
        if sample < self.timeline_viewport.start_sample
            || sample > self.timeline_viewport.end_sample
        {
            return -1.0;
        }
        self.timeline_viewport.fraction_of(sample)
    }

    pub(super) fn current_feature(&self) -> Option<FeatureFrame> {
        let analysis = self.analysis()?;
        let index = (self.playhead_fraction() * analysis.features.len() as f32) as usize;
        analysis
            .features
            .get(index.min(analysis.features.len().saturating_sub(1)))
            .copied()
    }
}
