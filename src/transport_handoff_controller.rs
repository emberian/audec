//! Transport continuity across the rare structural AudioHost replacement.
//!
//! Ordinary render publications swap cohorts inside one persistent renderer.
//! When format or compiled extent changes, the device/transport must be
//! recreated. This module maps the old transport through signed project-frame
//! coordinates and restores it on the new host; it never owns a transport or
//! creates another playback engine.

use std::error::Error;
use std::fmt;

use crate::audio::{
    AudioError, FrameRange, ProjectFrame, TransportHandle, TransportMode, TransportSnapshot,
};
use crate::project_audio_controller::ProjectTransportCommand;
use crate::project_selection::{ProjectSelection, SelectionTimelineError};
use crate::render_plan::{RenderFormat, RenderSpan};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransportEndpoint {
    pub timeline: RenderSpan,
    pub format: RenderFormat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectTransportHandoff {
    pub target_format: RenderFormat,
    pub target_frames: u64,
    pub mode: TransportMode,
    pub frame: ProjectFrame,
    pub loop_region: Option<FrameRange>,
    pub loop_enabled: bool,
    /// The old absolute playhead fell outside the new compiled extent.
    pub playhead_clamped: bool,
    /// Some of the old loop lay outside the new compiled extent.
    pub loop_clipped: bool,
    pub sample_rate_changed: bool,
}

/// Exact selection revision adopted by an explicit Set Loop action. It is a
/// receipt, not a live binding: later selection drags never mutate this loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoopSelectionAdoption {
    pub selection_revision: u64,
    pub project_span: crate::aspect::FrameSpan,
    pub transport_range: FrameRange,
}

/// Ordered transport effects emitted for one semantic workspace action.
/// Hosts apply them to the sole `ProjectAudioController` in this order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceTransportEffects {
    pub commands: Vec<ProjectTransportCommand>,
    pub selection_revision: Option<u64>,
    pub loop_adoption: Option<LoopSelectionAdoption>,
}

/// Selection-to-transport authority shared by arrangement and analysis panes.
///
/// It owns no audio handle. A drag commit updates only the transport's
/// selection candidate. Locating and loop adoption are separate explicit
/// methods, making it impossible for a mouse selection callback to resume at
/// a previous loop start or start playback as a side effect.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceTransportAuthority {
    endpoint: Option<TransportEndpoint>,
    selection_revision: Option<u64>,
    project_selection: Option<crate::aspect::FrameSpan>,
    transport_selection: Option<FrameRange>,
    loop_adoption: Option<LoopSelectionAdoption>,
}

impl WorkspaceTransportAuthority {
    pub fn new() -> Self {
        Self::default()
    }

    pub const fn selection_revision(&self) -> Option<u64> {
        self.selection_revision
    }

    pub const fn transport_selection(&self) -> Option<FrameRange> {
        self.transport_selection
    }

    pub const fn loop_adoption(&self) -> Option<LoopSelectionAdoption> {
        self.loop_adoption
    }

    /// Observe one authoritative project selection. The only possible audio
    /// command is `ReplaceSelection`; seek, play, and loop commands are absent
    /// by construction.
    pub fn selection_changed(
        &mut self,
        endpoint: TransportEndpoint,
        selection_revision: u64,
        selection: &ProjectSelection,
    ) -> Result<WorkspaceTransportEffects, TransportHandoffError> {
        if let Some(current) = self.selection_revision {
            if selection_revision == current && self.endpoint == Some(endpoint) {
                let span = selection.timeline_span()?;
                if span == self.project_selection {
                    return Ok(WorkspaceTransportEffects::default());
                }
                return Err(TransportHandoffError::SelectionRevisionCollision {
                    revision: selection_revision,
                });
            }
            if selection_revision != current && !revision_after(selection_revision, current) {
                return Err(TransportHandoffError::StaleSelectionRevision {
                    current,
                    received: selection_revision,
                });
            }
        }
        let project_selection = selection.timeline_span()?;
        let transport_selection = project_selection
            .map(|span| frame_span_to_transport(endpoint.timeline, span))
            .transpose()?;
        self.endpoint = Some(endpoint);
        self.selection_revision = Some(selection_revision);
        self.project_selection = project_selection;
        self.transport_selection = transport_selection;
        Ok(WorkspaceTransportEffects {
            commands: vec![ProjectTransportCommand::ReplaceSelection(
                transport_selection,
            )],
            selection_revision: Some(selection_revision),
            loop_adoption: self.loop_adoption,
        })
    }

    /// Snapshot the current candidate as the loop. Re-sending
    /// `ReplaceSelection` first makes this atomic at the application adapter
    /// even if UI delivery of the prior selection effect was delayed.
    pub fn set_loop_from_selection(
        &mut self,
    ) -> Result<WorkspaceTransportEffects, TransportHandoffError> {
        let selection_revision = self
            .selection_revision
            .ok_or(TransportHandoffError::NoTransportSelection)?;
        let project_span = self
            .project_selection
            .ok_or(TransportHandoffError::NoTransportSelection)?;
        let transport_range = self
            .transport_selection
            .ok_or(TransportHandoffError::NoTransportSelection)?;
        let adoption = LoopSelectionAdoption {
            selection_revision,
            project_span,
            transport_range,
        };
        self.loop_adoption = Some(adoption);
        Ok(WorkspaceTransportEffects {
            commands: vec![
                ProjectTransportCommand::ReplaceSelection(Some(transport_range)),
                ProjectTransportCommand::SetLoopFromSelection,
            ],
            selection_revision: Some(selection_revision),
            loop_adoption: Some(adoption),
        })
    }

    /// Locate is explicit and independent of selection. The audio controller
    /// already disables an enabled loop when locating outside it, preventing a
    /// later Play from jumping to stale bounds.
    pub fn locate(&self, frame: ProjectFrame) -> WorkspaceTransportEffects {
        WorkspaceTransportEffects {
            commands: vec![ProjectTransportCommand::Seek(frame)],
            selection_revision: self.selection_revision,
            loop_adoption: self.loop_adoption,
        }
    }

    pub fn clear_loop(&mut self) -> WorkspaceTransportEffects {
        self.loop_adoption = None;
        WorkspaceTransportEffects {
            commands: vec![ProjectTransportCommand::ClearLoop],
            selection_revision: self.selection_revision,
            loop_adoption: None,
        }
    }

    /// Enabling a retained loop never consults the *current* selection. Only a
    /// fresh Set Loop action changes the adoption receipt.
    pub fn set_loop_enabled(&self, enabled: bool) -> WorkspaceTransportEffects {
        WorkspaceTransportEffects {
            commands: vec![ProjectTransportCommand::SetLoopEnabled(enabled)],
            selection_revision: self.selection_revision,
            loop_adoption: self.loop_adoption,
        }
    }
}

fn revision_after(candidate: u64, current: u64) -> bool {
    let delta = candidate.wrapping_sub(current);
    delta != 0 && delta < (1_u64 << 63)
}

fn frame_span_to_transport(
    timeline: RenderSpan,
    span: crate::aspect::FrameSpan,
) -> Result<FrameRange, TransportHandoffError> {
    if span.start < timeline.start || span.end > timeline.end {
        return Err(TransportHandoffError::SelectionOutsideTimeline { span, timeline });
    }
    FrameRange::new(
        ProjectFrame(relative_exact(timeline, span.start)?),
        ProjectFrame(relative_exact(timeline, span.end)?),
    )
    .map_err(Into::into)
}

impl ProjectTransportHandoff {
    /// Map one observed transport into a replacement renderer. Relative host
    /// frames are first lifted to signed project frames, then rescaled in time
    /// if the project format changed, and finally projected into the new host.
    pub fn plan(
        previous: TransportEndpoint,
        snapshot: TransportSnapshot,
        next: TransportEndpoint,
    ) -> Result<Self, TransportHandoffError> {
        if snapshot.frame.0 > previous.timeline.len() {
            return Err(TransportHandoffError::SnapshotOutsidePreviousTimeline {
                frame: snapshot.frame,
                timeline: previous.timeline,
            });
        }
        let previous_rate = previous.format.sample_rate.get();
        let next_rate = next.format.sample_rate.get();
        let old_absolute = add_relative(previous.timeline.start, snapshot.frame.0)?;
        let mapped_absolute = scale_nearest(old_absolute, previous_rate, next_rate)?;
        let (frame, playhead_clamped) = relative_clamped(next.timeline, mapped_absolute)?;

        let (loop_region, loop_clipped) = match snapshot.loop_region {
            Some(region) => {
                if region.end.0 > previous.timeline.len() || region.is_empty() {
                    return Err(TransportHandoffError::LoopOutsidePreviousTimeline {
                        range: region,
                        timeline: previous.timeline,
                    });
                }
                let old_start = add_relative(previous.timeline.start, region.start.0)?;
                let old_end = add_relative(previous.timeline.start, region.end.0)?;
                let mapped_start = scale_floor(old_start, previous_rate, next_rate)?;
                let mapped_end = scale_ceil(old_end, previous_rate, next_rate)?;
                let clipped_start = mapped_start.max(next.timeline.start);
                let clipped_end = mapped_end.min(next.timeline.end);
                if clipped_start >= clipped_end {
                    (None, true)
                } else {
                    let start = relative_exact(next.timeline, clipped_start)?;
                    let end = relative_exact(next.timeline, clipped_end)?;
                    (
                        Some(FrameRange::new(ProjectFrame(start), ProjectFrame(end))?),
                        clipped_start != mapped_start || clipped_end != mapped_end,
                    )
                }
            }
            None => (None, false),
        };

        Ok(Self {
            target_format: next.format,
            target_frames: next.timeline.len(),
            mode: normalize_terminal_mode(snapshot.mode),
            frame,
            loop_region,
            loop_enabled: snapshot.loop_enabled && loop_region.is_some(),
            playhead_clamped,
            loop_clipped,
            sample_rate_changed: previous_rate != next_rate,
        })
    }

    /// Restore the planned state onto the newly opened host transport. Loop
    /// state and the locate cross the realtime boundary as one control tuple,
    /// so the replacement host can never briefly combine the inherited loop
    /// start with its default playhead. The transaction does not start audio.
    pub fn apply(self, transport: &TransportHandle) -> Result<(), TransportHandoffError> {
        let actual_format = transport.format();
        if actual_format.sample_rate != self.target_format.sample_rate
            || actual_format.channels != self.target_format.channels
            || transport.length().0 != self.target_frames
        {
            return Err(TransportHandoffError::TargetTransportMismatch {
                expected_format: self.target_format,
                expected_frames: self.target_frames,
                actual_sample_rate: actual_format.sample_rate.get(),
                actual_channels: actual_format.channels.get(),
                actual_frames: transport.length().0,
            });
        }
        transport.set_loop_state(
            self.loop_region,
            self.loop_enabled,
            Some(ProjectFrame(self.frame.0.min(transport.length().0))),
        )?;
        match self.mode {
            TransportMode::Stopped => transport.stop(),
            TransportMode::Paused | TransportMode::Ended => {
                // A new transport is Stopped, and seeking to zero preserves
                // that mode. A control-only play/pause pair establishes an
                // exact Paused state without waiting for an audio callback.
                transport.play();
                transport.pause();
            }
            TransportMode::Playing => transport.play(),
        }
        Ok(())
    }
}

fn normalize_terminal_mode(mode: TransportMode) -> TransportMode {
    match mode {
        TransportMode::Ended => TransportMode::Paused,
        other => other,
    }
}

fn add_relative(origin: i64, relative: u64) -> Result<i64, TransportHandoffError> {
    i128::from(origin)
        .checked_add(i128::from(relative))
        .and_then(|value| i64::try_from(value).ok())
        .ok_or(TransportHandoffError::CoordinateOverflow)
}

fn relative_exact(timeline: RenderSpan, absolute: i64) -> Result<u64, TransportHandoffError> {
    u64::try_from(i128::from(absolute) - i128::from(timeline.start))
        .map_err(|_| TransportHandoffError::CoordinateOverflow)
}

fn relative_clamped(
    timeline: RenderSpan,
    absolute: i64,
) -> Result<(ProjectFrame, bool), TransportHandoffError> {
    let clamped = absolute.clamp(timeline.start, timeline.end);
    Ok((
        ProjectFrame(relative_exact(timeline, clamped)?),
        clamped != absolute,
    ))
}

fn scale_nearest(frame: i64, from_rate: u32, to_rate: u32) -> Result<i64, TransportHandoffError> {
    let numerator = i128::from(frame)
        .checked_mul(i128::from(to_rate))
        .ok_or(TransportHandoffError::CoordinateOverflow)?;
    let denominator = i128::from(from_rate);
    let quotient = numerator.div_euclid(denominator);
    let remainder = numerator.rem_euclid(denominator);
    let rounded = if remainder.saturating_mul(2) >= denominator {
        quotient
            .checked_add(1)
            .ok_or(TransportHandoffError::CoordinateOverflow)?
    } else {
        quotient
    };
    i64::try_from(rounded).map_err(|_| TransportHandoffError::CoordinateOverflow)
}

fn scale_floor(frame: i64, from_rate: u32, to_rate: u32) -> Result<i64, TransportHandoffError> {
    let numerator = i128::from(frame)
        .checked_mul(i128::from(to_rate))
        .ok_or(TransportHandoffError::CoordinateOverflow)?;
    i64::try_from(numerator.div_euclid(i128::from(from_rate)))
        .map_err(|_| TransportHandoffError::CoordinateOverflow)
}

fn scale_ceil(frame: i64, from_rate: u32, to_rate: u32) -> Result<i64, TransportHandoffError> {
    let numerator = i128::from(frame)
        .checked_mul(i128::from(to_rate))
        .ok_or(TransportHandoffError::CoordinateOverflow)?;
    let denominator = i128::from(from_rate);
    let floor = numerator.div_euclid(denominator);
    let value = if numerator.rem_euclid(denominator) == 0 {
        floor
    } else {
        floor
            .checked_add(1)
            .ok_or(TransportHandoffError::CoordinateOverflow)?
    };
    i64::try_from(value).map_err(|_| TransportHandoffError::CoordinateOverflow)
}

#[derive(Clone, Debug, PartialEq)]
pub enum TransportHandoffError {
    SnapshotOutsidePreviousTimeline {
        frame: ProjectFrame,
        timeline: RenderSpan,
    },
    LoopOutsidePreviousTimeline {
        range: FrameRange,
        timeline: RenderSpan,
    },
    SelectionOutsideTimeline {
        span: crate::aspect::FrameSpan,
        timeline: RenderSpan,
    },
    NoTransportSelection,
    StaleSelectionRevision {
        current: u64,
        received: u64,
    },
    SelectionRevisionCollision {
        revision: u64,
    },
    Selection(SelectionTimelineError),
    CoordinateOverflow,
    TargetTransportMismatch {
        expected_format: RenderFormat,
        expected_frames: u64,
        actual_sample_rate: u32,
        actual_channels: u16,
        actual_frames: u64,
    },
    Audio(AudioError),
}

impl fmt::Display for TransportHandoffError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "transport handoff: {self:?}")
    }
}

impl Error for TransportHandoffError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Audio(error) => Some(error),
            Self::Selection(error) => Some(error),
            _ => None,
        }
    }
}

impl From<AudioError> for TransportHandoffError {
    fn from(error: AudioError) -> Self {
        Self::Audio(error)
    }
}

impl From<SelectionTimelineError> for TransportHandoffError {
    fn from(error: SelectionTimelineError) -> Self {
        Self::Selection(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{AudioFormat, PcmRenderer, ProjectAudio, TransportSource};

    fn format(rate: u32) -> RenderFormat {
        RenderFormat::new(rate, 2).unwrap()
    }

    fn snapshot(
        mode: TransportMode,
        frame: u64,
        loop_region: Option<(u64, u64)>,
        loop_enabled: bool,
    ) -> TransportSnapshot {
        TransportSnapshot {
            mode,
            frame: ProjectFrame(frame),
            loop_region: loop_region.map(|(start, end)| {
                FrameRange::new(ProjectFrame(start), ProjectFrame(end)).unwrap()
            }),
            loop_enabled,
            revision: 9,
        }
    }

    #[test]
    fn expanded_timeline_preserves_absolute_playhead_mode_and_loop() {
        let handoff = ProjectTransportHandoff::plan(
            TransportEndpoint {
                timeline: RenderSpan::new(100, 200).unwrap(),
                format: format(48_000),
            },
            snapshot(TransportMode::Playing, 30, Some((20, 60)), true),
            TransportEndpoint {
                timeline: RenderSpan::new(50, 250).unwrap(),
                format: format(48_000),
            },
        )
        .unwrap();
        assert_eq!(handoff.frame, ProjectFrame(80));
        assert_eq!(
            handoff.loop_region,
            Some(FrameRange::new(ProjectFrame(70), ProjectFrame(110)).unwrap())
        );
        assert_eq!(handoff.mode, TransportMode::Playing);
        assert!(!handoff.playhead_clamped);
        assert!(!handoff.loop_clipped);
    }

    #[test]
    fn shrink_clamps_playhead_and_clips_loop_without_inventing_bounds() {
        let handoff = ProjectTransportHandoff::plan(
            TransportEndpoint {
                timeline: RenderSpan::new(0, 1_000).unwrap(),
                format: format(48_000),
            },
            snapshot(TransportMode::Paused, 900, Some((700, 950)), true),
            TransportEndpoint {
                timeline: RenderSpan::new(0, 800).unwrap(),
                format: format(48_000),
            },
        )
        .unwrap();
        assert_eq!(handoff.frame, ProjectFrame(800));
        assert_eq!(
            handoff.loop_region,
            Some(FrameRange::new(ProjectFrame(700), ProjectFrame(800)).unwrap())
        );
        assert!(handoff.playhead_clamped);
        assert!(handoff.loop_clipped);
    }

    #[test]
    fn sample_rate_change_preserves_time_and_half_open_loop_coverage() {
        let handoff = ProjectTransportHandoff::plan(
            TransportEndpoint {
                timeline: RenderSpan::new(0, 48_000).unwrap(),
                format: format(48_000),
            },
            snapshot(TransportMode::Paused, 24_000, Some((12_000, 36_000)), true),
            TransportEndpoint {
                timeline: RenderSpan::new(0, 96_000).unwrap(),
                format: format(96_000),
            },
        )
        .unwrap();
        assert_eq!(handoff.frame, ProjectFrame(48_000));
        assert_eq!(
            handoff.loop_region,
            Some(FrameRange::new(ProjectFrame(24_000), ProjectFrame(72_000)).unwrap())
        );
        assert!(handoff.sample_rate_changed);
    }

    #[test]
    fn applying_handoff_restores_requested_control_state() {
        let format = AudioFormat::new(48_000, 2).unwrap();
        let audio = ProjectAudio::from_interleaved(format, vec![0.0; 400]).unwrap();
        let (transport, mut source) = TransportSource::new(PcmRenderer::new(audio));
        let handoff = ProjectTransportHandoff {
            target_format: RenderFormat::new(48_000, 2).unwrap(),
            target_frames: 200,
            mode: TransportMode::Playing,
            // A loop edit may leave already-running playback outside its new
            // bounds. Handoff must preserve that exact current sample instead
            // of silently resuming at the loop start.
            frame: ProjectFrame(90),
            loop_region: Some(FrameRange::new(ProjectFrame(20), ProjectFrame(60)).unwrap()),
            loop_enabled: true,
            playhead_clamped: false,
            loop_clipped: false,
            sample_rate_changed: false,
        };
        handoff.apply(&transport).unwrap();
        // The audio source has not run, so published position remains old;
        // loop control is immediately observable and the pending play/seek is
        // proven by the transport's existing play-after-seek regression suite.
        let snapshot = transport.snapshot();
        assert_eq!(snapshot.loop_region, handoff.loop_region);
        assert!(snapshot.loop_enabled);

        // The first realtime observation adopts the entire tuple. It starts
        // at the inherited playhead, not at the inherited loop start.
        assert_eq!(source.next(), Some(0.0));
        let applied = transport.snapshot();
        assert_eq!(applied.mode, TransportMode::Playing);
        assert_eq!(applied.frame, handoff.frame);
        assert_eq!(applied.loop_region, handoff.loop_region);
        assert!(applied.loop_enabled);
    }

    fn selected(start: i64, end: i64) -> ProjectSelection {
        ProjectSelection {
            time: Some(crate::aspect::FrameSpan { start, end }),
            aspect: Some(crate::aspect::Aspect::Time(crate::aspect::FrameSpan {
                start,
                end,
            })),
            ..ProjectSelection::default()
        }
    }

    #[test]
    fn drag_selection_cannot_seek_play_or_reuse_a_previous_loop() {
        let endpoint = TransportEndpoint {
            timeline: RenderSpan::new(0, 1_000).unwrap(),
            format: format(48_000),
        };
        let mut authority = WorkspaceTransportAuthority::new();
        let first = authority
            .selection_changed(endpoint, 1, &selected(100, 200))
            .unwrap();
        assert_eq!(
            first.commands,
            vec![ProjectTransportCommand::ReplaceSelection(Some(
                FrameRange::new(ProjectFrame(100), ProjectFrame(200)).unwrap()
            ))]
        );
        assert!(first.commands.iter().all(|command| !matches!(
            command,
            ProjectTransportCommand::Seek(_)
                | ProjectTransportCommand::Play
                | ProjectTransportCommand::TogglePlay
                | ProjectTransportCommand::SetLoopFromSelection
                | ProjectTransportCommand::ReplaceSelectionAndLoop(_)
        )));

        let adopted = authority.set_loop_from_selection().unwrap();
        assert_eq!(
            adopted.commands,
            vec![
                ProjectTransportCommand::ReplaceSelection(Some(
                    FrameRange::new(ProjectFrame(100), ProjectFrame(200)).unwrap()
                )),
                ProjectTransportCommand::SetLoopFromSelection,
            ]
        );
        let second = authority
            .selection_changed(endpoint, 2, &selected(400, 500))
            .unwrap();
        assert_eq!(
            second.commands,
            vec![ProjectTransportCommand::ReplaceSelection(Some(
                FrameRange::new(ProjectFrame(400), ProjectFrame(500)).unwrap()
            ))]
        );
        assert_eq!(
            authority.loop_adoption().unwrap().project_span,
            crate::aspect::FrameSpan {
                start: 100,
                end: 200
            }
        );
        assert_eq!(
            authority.set_loop_enabled(true).commands,
            vec![ProjectTransportCommand::SetLoopEnabled(true)]
        );
    }

    #[test]
    fn explicit_locate_is_the_only_selection_adjacent_seek() {
        let endpoint = TransportEndpoint {
            timeline: RenderSpan::new(-100, 900).unwrap(),
            format: format(48_000),
        };
        let mut authority = WorkspaceTransportAuthority::new();
        authority
            .selection_changed(endpoint, 8, &selected(0, 100))
            .unwrap();
        assert_eq!(
            authority.locate(ProjectFrame(350)).commands,
            vec![ProjectTransportCommand::Seek(ProjectFrame(350))]
        );
        assert_eq!(
            authority.transport_selection(),
            Some(FrameRange::new(ProjectFrame(100), ProjectFrame(200)).unwrap())
        );
    }
}
