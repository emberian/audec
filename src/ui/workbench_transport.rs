//! Transport commands, timeline effect application, and playhead queries.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

use crate::project_controller::{
    MeterPointIntent, MusicalPointError, MusicalPointPlan, TempoPointIntent,
};
use crate::sequencer::{BeatTime, TimeSignature};

/// The signatures the transport chip cycles through: common time, a waltz, a
/// compound six, and the two odd meters a musician reaches for most. The
/// sequencer accepts any signature it can validate; this is the short list
/// one button can offer without a picker.
const METER_CYCLE: [(u16, u16); 5] = [(4, 4), (3, 4), (6, 8), (5, 4), (7, 8)];

/// What the transport bar says about musical time, read *at the playhead*
/// rather than at tick zero, together with the two positions an edit lands
/// on. The map is asked for both; the shell never derives a bar itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PlayheadMusicalTime {
    pub revision: u64,
    /// Tempo under the playhead: what the transport bar reads out.
    pub bpm: f64,
    /// Tempo in force at `bar_start`, which is what a mark there carries. It
    /// differs from `bpm` only when a tempo point sits inside this bar, and a
    /// mark must not quietly move the head of the bar to a later tempo.
    pub bar_bpm: f64,
    pub signature: TimeSignature,
    /// One-based bar containing the playhead, as the ruler labels it.
    pub bar: i64,
    /// Start of that bar: where a tempo or meter point is authored.
    pub bar_start: BeatTime,
    /// Start of the tempo segment in force: where a ± nudge lands.
    pub segment_start: BeatTime,
}

fn next_signature(current: TimeSignature) -> TimeSignature {
    let index = METER_CYCLE
        .iter()
        .position(|(numerator, denominator)| {
            *numerator == current.numerator && *denominator == current.denominator
        })
        .map_or(0, |index| (index + 1) % METER_CYCLE.len());
    let (numerator, denominator) = METER_CYCLE[index];
    TimeSignature::new(numerator, denominator).expect("the cycle list holds valid signatures")
}

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
        if self.audio.is_none() {
            // No host yet: the kernel is the transport authority until one
            // opens, so it must learn the requested playhead too.
            let sample_rate = self
                .analysis()
                .map_or(0.0, |analysis| f64::from(analysis.sample_rate));
            let playhead = TimelinePoint((seconds * sample_rate).round().max(0.0) as u64);
            let mode = self.timeline_interaction.snapshot().playback;
            self.dispatch_timeline_event(
                TimelineInteractionEvent::TransportObserved { playhead, mode },
                cx,
            );
        }
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

    /// Tempo and meter where the playhead is standing. A project with one
    /// tempo point answers exactly what tick zero used to answer; a project
    /// with a tempo or meter change answers the truth for this position.
    pub(super) fn playhead_musical_time(&self, cx: &App) -> Option<PlayheadMusicalTime> {
        let session = self.session.read(cx);
        let snapshot = session.project_snapshot().ok()?;
        let tempo_map = snapshot.project.state().domains.sequencer.tempo_map();
        let at = tempo_map.frame_to_beat_floor(crate::sequencer::ProjectFrame(
            i64::try_from(self.playhead_sample()).unwrap_or(i64::MAX),
        ));
        let bar_start = tempo_map.bar_start(at);
        Some(PlayheadMusicalTime {
            revision: snapshot.revisions().aggregate,
            bpm: tempo_map.tempo_at(at).bpm(),
            bar_bpm: tempo_map.tempo_at(bar_start).bpm(),
            signature: tempo_map.meter_at(at),
            bar: tempo_map.musical_position(bar_start).bar + 1,
            bar_start,
            segment_start: tempo_map.tempo_segment_start(at),
        })
    }

    /// Place a tempo point at the start of the bar the playhead is in,
    /// carrying the tempo already in force there. It is a marker: the ± keys
    /// then bend that segment without touching the music before it.
    pub(super) fn mark_tempo_at_playhead(&mut self, cx: &mut Context<Self>) {
        let Some(time) = self.playhead_musical_time(cx) else {
            self.constructive_status = Some("Tempo point needs an open project".into());
            cx.notify();
            return;
        };
        let intent = TempoPointIntent {
            expected_project_revision: time.revision,
            at: time.bar_start,
            bpm: time.bar_bpm,
        };
        let status = match self.plan_tempo_point(intent, cx) {
            Err(error) => format!("Tempo point refused · {error}"),
            Ok(MusicalPointPlan::Unchanged(publication)) => format!(
                "Bar {} already carries a tempo point · {:.2} BPM",
                publication.bar, publication.adopted_bpm
            ),
            Ok(MusicalPointPlan::Change {
                envelope,
                publication,
            }) => match self
                .session
                .update(cx, |session, _| session.execute_envelope(envelope))
            {
                Ok(_) => format!(
                    "Tempo point at bar {} · {:.2} BPM · undoable",
                    publication.bar, publication.adopted_bpm
                ),
                Err(error) => format!("Tempo point refused · {error}"),
            },
        };
        self.constructive_status = Some(status);
        cx.notify();
    }

    /// Cycle the time signature at the start of the bar the playhead is in.
    /// A bar start is the only position `TempoMap::set_meter` accepts, and
    /// when a later meter point makes even that illegal the map's own reason
    /// is what the musician reads.
    pub(super) fn cycle_meter_at_playhead(&mut self, cx: &mut Context<Self>) {
        let Some(time) = self.playhead_musical_time(cx) else {
            self.constructive_status = Some("Time signature needs an open project".into());
            cx.notify();
            return;
        };
        let signature = next_signature(time.signature);
        let intent = MeterPointIntent {
            expected_project_revision: time.revision,
            at: time.bar_start,
            signature,
        };
        let status = match self.plan_meter_point(intent, cx) {
            Err(error) => format!("Time signature refused · {error}"),
            Ok(MusicalPointPlan::Unchanged(publication)) => format!(
                "Bar {} is already {}/{}",
                publication.bar, publication.adopted.numerator, publication.adopted.denominator
            ),
            Ok(MusicalPointPlan::Change {
                envelope,
                publication,
            }) => match self
                .session
                .update(cx, |session, _| session.execute_envelope(envelope))
            {
                Ok(_) => format!(
                    "Time signature at bar {} · {}/{} · undoable",
                    publication.bar, publication.adopted.numerator, publication.adopted.denominator
                ),
                Err(error) => format!("Time signature refused · {error}"),
            },
        };
        self.constructive_status = Some(status);
        cx.notify();
    }

    fn plan_tempo_point(
        &self,
        intent: TempoPointIntent,
        cx: &App,
    ) -> Result<MusicalPointPlan<crate::project_controller::TempoPointPublication>, MusicalPointError>
    {
        self.session
            .read(cx)
            .project_controller()
            .ok_or(MusicalPointError::NoProject)?
            .plan_tempo_point(intent)
    }

    fn plan_meter_point(
        &self,
        intent: MeterPointIntent,
        cx: &App,
    ) -> Result<MusicalPointPlan<crate::project_controller::MeterPointPublication>, MusicalPointError>
    {
        self.session
            .read(cx)
            .project_controller()
            .ok_or(MusicalPointError::NoProject)?
            .plan_meter_point(intent)
    }

    /// Nudge the tempo of the segment the playhead is in. With a single
    /// tempo point that segment starts at tick zero, so a project that never
    /// marked a change behaves exactly as it did before; with a marked change
    /// the nudge bends that section and leaves the earlier music alone.
    pub(super) fn adjust_project_tempo(&mut self, delta_bpm: f64, cx: &mut Context<Self>) {
        let Some(time) = self.playhead_musical_time(cx) else {
            self.constructive_status = Some("Project tempo is unavailable".into());
            cx.notify();
            return;
        };
        let intent = TempoPointIntent {
            expected_project_revision: time.revision,
            at: time.segment_start,
            bpm: (time.bpm + delta_bpm).max(1.0),
        };
        let status = match self.plan_tempo_point(intent, cx) {
            Err(error) => format!("Tempo adjustment failed · {error}"),
            Ok(MusicalPointPlan::Unchanged(publication)) => format!(
                "Project tempo is already {:.3} BPM",
                publication.adopted_bpm
            ),
            Ok(MusicalPointPlan::Change {
                envelope,
                publication,
            }) => {
                let subject = if publication.at == BeatTime::ZERO {
                    "Project tempo".to_owned()
                } else {
                    format!("Tempo from bar {}", publication.bar)
                };
                match self
                    .session
                    .update(cx, |session, _| session.execute_envelope(envelope))
                {
                    Ok(_) => format!(
                        "{subject} {:.3} → {:.3} BPM · undoable",
                        publication.previous_bpm, publication.adopted_bpm
                    ),
                    Err(error) => format!("Tempo adjustment failed · {error}"),
                }
            }
        };
        self.constructive_status = Some(status);
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
            // See apply_timeline_transport_effect: the kernel keeps the intent
            // until the host opens and restores it from the snapshot.
            if self.audio_error.is_none() {
                self.audio_device_status =
                    Some("Preparing audio · transport request queued".into());
            }
            cx.notify();
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
            // No host yet: the opening bounce is still rendering. The kernel
            // already holds the requested playhead, loop, and playback mode;
            // the host restores all three from its snapshot when it opens.
            if self.audio_error.is_none() {
                self.audio_device_status =
                    Some("Preparing audio · transport request queued".into());
            }
            cx.notify();
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

    /// Shared handle to the loaded analysis, for lenses seeded from `&self`.
    pub(super) fn analysis_arc(&self) -> Option<Arc<Analysis>> {
        match &self.state {
            ProjectState::Ready(analysis) => Some(Arc::clone(analysis)),
            _ => None,
        }
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
