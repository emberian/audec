//! Immutable render schedules compiled from audec's editable DAW state.
//!
//! Editing models are intentionally rich, mutable, and backend independent.
//! This module is the control-thread boundary that validates those models and
//! lowers them into stable half-open frame windows suitable for realtime or
//! offline consumers. It also contains a deliberately small reference PCM
//! renderer. The reference path renders clips, fades, rational-rate resampling,
//! reverse playback, channel maps, automation, mixer faders, and sends. It does
//! **not** claim to host instruments, run plugins, or perform pitch-preserving
//! time stretching; those omissions produce deterministic diagnostics.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::f32::consts::FRAC_PI_4;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::arrangement::{
    self, AssetId, AudioLoopMode, ChannelMapping, ClipContent, ClipFades, ClipId, Fade, FadeCurve,
    StretchAlgorithm, TrackId,
};
use crate::audio::AudioFormat;
use crate::automation::{
    self, AutomationGraph, ClipParameter, CompiledAutomation, MixerTarget, ParameterAddress,
};
use crate::mixer::{BusId, LatencyPlan, MixerGraph, ProcessorId, RouteKind, SendTap};
use crate::sequencer::{self, ScheduledEvent, Sequencer};

/// A non-empty, signed, end-exclusive project-frame range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderWindow {
    pub start: i64,
    pub end: i64,
}

impl RenderWindow {
    pub fn new(start: i64, end: i64) -> Result<Self, CompileError> {
        if start >= end {
            return Err(CompileError::EmptyWindow { start, end });
        }
        Ok(Self { start, end })
    }

    pub const fn len(self) -> u64 {
        self.end.saturating_sub(self.start) as u64
    }

    pub const fn contains(self, frame: i64) -> bool {
        self.start <= frame && frame < self.end
    }

    pub const fn intersects(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }

    pub fn intersection(self, other: Self) -> Option<Self> {
        let start = self.start.max(other.start);
        let end = self.end.min(other.end);
        (start < end).then_some(Self { start, end })
    }
}

/// Runtime knowledge supplied by a plugin host adapter at compile time.
///
/// The reference renderer still bypasses every plugin. `available` describes
/// whether a future graph executor may instantiate it, while `tail_frames`
/// lets bounce/export UIs reserve an explicit tail without guessing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessorRuntimeInfo {
    pub available: bool,
    pub tail_frames: u64,
}

/// Control-thread inputs to [`compile_render_schedule`].
pub struct RenderCompileRequest<'a> {
    pub arrangement: &'a arrangement::ArrangementState,
    pub sequencer: &'a Sequencer,
    pub automation: &'a AutomationGraph,
    pub mixer: &'a MixerGraph,
    /// Explicit arrangement-track to mixer-bus bindings. Unbound audible
    /// tracks fall back to the master and receive a diagnostic.
    pub track_buses: &'a BTreeMap<TrackId, BusId>,
    /// Plugin availability and declared tails from the host. Missing entries
    /// mean unavailable with no known tail.
    pub processors: &'a BTreeMap<ProcessorId, ProcessorRuntimeInfo>,
    pub window: RenderWindow,
    pub output_channels: u16,
    pub block_frames: u32,
    /// Stable seed used for probability-bearing sequencer events.
    pub performance_seed: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RenderDiagnostic {
    TrackRoutedToMaster {
        track: TrackId,
    },
    MissingMixerBus {
        track: TrackId,
        bus: BusId,
    },
    UnsupportedTimeTransform {
        clip: ClipId,
        reason: &'static str,
    },
    ArrangementPatternNeedsInstrument {
        clip: ClipId,
        pattern: u64,
    },
    ArrangementAutomationRegionExternal {
        clip: ClipId,
        parameter: u64,
    },
    PluginUnavailable {
        processor: ProcessorId,
        identifier: String,
    },
    PluginBypassedByReferenceRenderer {
        processor: ProcessorId,
        identifier: String,
    },
    MissingAsset {
        clip: ClipId,
        asset: AssetId,
    },
    InvalidAssetFormat {
        clip: ClipId,
        asset: AssetId,
        reason: &'static str,
    },
    SequencerEventsNeedInstrument {
        count: usize,
    },
}

/// One audio clip lowered into frame-domain playback metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct CompiledAudioClip {
    pub id: ClipId,
    pub track: TrackId,
    pub bus: BusId,
    pub placement: RenderWindow,
    pub asset: AssetId,
    pub source_start: u64,
    pub source_end: u64,
    pub ratio_source_frames: u64,
    pub ratio_project_frames: u64,
    pub reverse: bool,
    pub channels: ChannelMapping,
    pub loop_mode: AudioLoopMode,
    pub fades: ClipFades,
    pub clip_gain_db: f32,
    pub track_gain_db: f32,
    pub track_pan: f32,
    pub renderable: bool,
}

/// A mixer route already resolved to its static tap and gain metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct CompiledRoute {
    pub to: BusId,
    pub kind: RouteKind,
    pub tap: SendTap,
    pub static_gain: f32,
    pub compensation_delay_frames: u64,
}

/// One bus in source-to-master topological order.
#[derive(Clone, Debug, PartialEq)]
pub struct CompiledBus {
    pub id: BusId,
    pub audible: bool,
    pub gain_db: f32,
    pub pan: f32,
    pub routes: Arc<[CompiledRoute]>,
    pub insert_latency_frames: u64,
}

/// Exact work assigned to one callback-sized half-open block.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderBlock {
    pub window: RenderWindow,
    /// Indices into [`RenderSchedule::audio_clips`].
    pub audio_clip_indices: Arc<[usize]>,
    /// Events use offsets relative to this block's start.
    pub sequencer_events: Arc<[ScheduledEvent]>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderTailMetadata {
    pub master_latency_frames: u64,
    pub maximum_declared_plugin_tail_frames: u64,
    pub scheduled_end_with_tail: i64,
}

/// Immutable snapshot consumed by realtime and offline graph executors.
#[derive(Clone, Debug)]
pub struct RenderSchedule {
    format: AudioFormat,
    window: RenderWindow,
    block_frames: u32,
    master: BusId,
    audio_clips: Arc<[CompiledAudioClip]>,
    blocks: Arc<[RenderBlock]>,
    buses: Arc<[CompiledBus]>,
    automation: CompiledAutomation,
    latency: LatencyPlan,
    tail: RenderTailMetadata,
    diagnostics: Arc<[RenderDiagnostic]>,
}

impl RenderSchedule {
    pub const fn format(&self) -> AudioFormat {
        self.format
    }

    pub const fn window(&self) -> RenderWindow {
        self.window
    }

    pub const fn block_frames(&self) -> u32 {
        self.block_frames
    }

    pub const fn master(&self) -> BusId {
        self.master
    }

    pub fn audio_clips(&self) -> &[CompiledAudioClip] {
        &self.audio_clips
    }

    pub fn blocks(&self) -> &[RenderBlock] {
        &self.blocks
    }

    pub fn buses(&self) -> &[CompiledBus] {
        &self.buses
    }

    pub fn automation(&self) -> &CompiledAutomation {
        &self.automation
    }

    pub fn latency(&self) -> &LatencyPlan {
        &self.latency
    }

    pub const fn tail(&self) -> RenderTailMetadata {
        self.tail
    }

    pub fn diagnostics(&self) -> &[RenderDiagnostic] {
        &self.diagnostics
    }

    /// Returns the precompiled block containing `frame`, if it is scheduled.
    pub fn block_at(&self, frame: i64) -> Option<&RenderBlock> {
        let index = self
            .blocks
            .partition_point(|block| block.window.end <= frame);
        self.blocks
            .get(index)
            .filter(|block| block.window.contains(frame))
    }
}

#[derive(Clone, Debug, Default)]
pub struct RenderCancellation {
    cancelled: Arc<AtomicBool>,
}

impl RenderCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn check(&self) -> Result<(), CompileError> {
        if self.is_cancelled() {
            Err(CompileError::Cancelled)
        } else {
            Ok(())
        }
    }
}

struct AutomationTempoMap<'a>(&'a sequencer::TempoMap);

impl automation::BeatFrameMap for AutomationTempoMap<'_> {
    fn beat_to_frame(&self, beat: automation::BeatTime) -> automation::ProjectFrame {
        automation::ProjectFrame(self.0.beat_to_frame(sequencer::BeatTime(beat.0)).0)
    }
}

/// Validate and freeze all editable DAW state into one deterministic schedule.
pub fn compile_render_schedule(
    request: RenderCompileRequest<'_>,
    cancellation: &RenderCancellation,
) -> Result<RenderSchedule, CompileError> {
    cancellation.check()?;
    if request.window.start >= request.window.end {
        return Err(CompileError::EmptyWindow {
            start: request.window.start,
            end: request.window.end,
        });
    }
    if request.block_frames == 0 {
        return Err(CompileError::ZeroBlockFrames);
    }
    if !matches!(request.output_channels, 1 | 2) {
        return Err(CompileError::UnsupportedOutputChannels(
            request.output_channels,
        ));
    }
    request
        .arrangement
        .validate()
        .map_err(|error| CompileError::Arrangement(error.to_string()))?;
    request
        .sequencer
        .validate()
        .map_err(|error| CompileError::Sequencer(error.to_string()))?;
    request
        .automation
        .validate()
        .map_err(|error| CompileError::Automation(error.to_string()))?;
    request
        .mixer
        .validate()
        .map_err(|error| CompileError::Mixer(error.to_string()))?;
    if request.arrangement.sample_rate != request.sequencer.tempo_map().sample_rate() {
        return Err(CompileError::SampleRateMismatch {
            arrangement: request.arrangement.sample_rate,
            sequencer: request.sequencer.tempo_map().sample_rate(),
        });
    }

    let format = AudioFormat::new(request.arrangement.sample_rate, request.output_channels)
        .map_err(|error| CompileError::AudioFormat(error.to_string()))?;
    let automation = request
        .automation
        .compile(&AutomationTempoMap(request.sequencer.tempo_map()))
        .map_err(|error| CompileError::Automation(error.to_string()))?;
    let latency = request
        .mixer
        .latency_plan()
        .map_err(|error| CompileError::Mixer(error.to_string()))?;
    let master = request.mixer.master();
    let effective = request.mixer.effective_states();
    let mut diagnostics = Vec::new();

    let any_track_solo = request.arrangement.tracks.values().any(|track| track.solo);
    let mut clips = Vec::new();
    for track_id in &request.arrangement.track_order {
        cancellation.check()?;
        let track = &request.arrangement.tracks[track_id];
        let track_audible = !track.muted && (!any_track_solo || track.solo);
        let bus = match request.track_buses.get(track_id).copied() {
            Some(bus) if request.mixer.bus(bus).is_some() => bus,
            Some(bus) => {
                diagnostics.push(RenderDiagnostic::MissingMixerBus {
                    track: *track_id,
                    bus,
                });
                master
            }
            None => {
                diagnostics.push(RenderDiagnostic::TrackRoutedToMaster { track: *track_id });
                master
            }
        };
        for clip in request.arrangement.clips_on_track(*track_id) {
            if clip.muted || !track_audible {
                continue;
            }
            let placement = RenderWindow {
                start: clip.placement.start.get(),
                end: clip.placement.end.get(),
            };
            if !placement.intersects(request.window) {
                continue;
            }
            match &clip.content {
                ClipContent::Audio(audio) => {
                    let (renderable, reason) = supported_pcm_transform(audio);
                    if let Some(reason) = reason {
                        diagnostics.push(RenderDiagnostic::UnsupportedTimeTransform {
                            clip: clip.id,
                            reason,
                        });
                    }
                    clips.push(CompiledAudioClip {
                        id: clip.id,
                        track: *track_id,
                        bus,
                        placement,
                        asset: audio.asset,
                        source_start: audio.source.start,
                        source_end: audio.source.end,
                        ratio_source_frames: audio.playback.ratio.source_frames,
                        ratio_project_frames: audio.playback.ratio.project_frames,
                        reverse: audio.playback.reverse,
                        channels: audio.channels.clone(),
                        loop_mode: audio.loop_mode,
                        fades: clip.fades,
                        clip_gain_db: clip.gain_db,
                        track_gain_db: track.gain_db,
                        track_pan: track.pan,
                        renderable,
                    });
                }
                ClipContent::Pattern(pattern) => {
                    diagnostics.push(RenderDiagnostic::ArrangementPatternNeedsInstrument {
                        clip: clip.id,
                        pattern: pattern.pattern.get(),
                    });
                }
                ClipContent::Automation(region) => {
                    diagnostics.push(RenderDiagnostic::ArrangementAutomationRegionExternal {
                        clip: clip.id,
                        parameter: region.parameter.get(),
                    })
                }
            }
        }
    }
    clips.sort_by_key(|clip| (clip.placement.start, clip.track, clip.id));

    let buses = compile_buses(
        request.mixer,
        &latency,
        &effective,
        request.processors,
        &mut diagnostics,
    )?;
    let maximum_tail = request
        .processors
        .values()
        .map(|processor| processor.tail_frames)
        .max()
        .unwrap_or(0);
    let tail = RenderTailMetadata {
        master_latency_frames: latency.master_output_latency_samples,
        maximum_declared_plugin_tail_frames: maximum_tail,
        scheduled_end_with_tail: request
            .window
            .end
            .saturating_add(maximum_tail.min(i64::MAX as u64) as i64),
    };

    let mut blocks = Vec::new();
    let mut start = request.window.start;
    while start < request.window.end {
        cancellation.check()?;
        let end = start
            .saturating_add(i64::from(request.block_frames))
            .min(request.window.end);
        let window = RenderWindow { start, end };
        let audio_clip_indices: Arc<[usize]> = clips
            .iter()
            .enumerate()
            .filter_map(|(index, clip)| clip.placement.intersects(window).then_some(index))
            .collect::<Vec<_>>()
            .into();
        let sequencer_range = sequencer::FrameRange::new(
            sequencer::ProjectFrame(start),
            sequencer::ProjectFrame(end),
        )
        .map_err(|error| CompileError::Sequencer(error.to_string()))?;
        let events: Arc<[ScheduledEvent]> = request
            .sequencer
            .schedule_project_window(sequencer_range, request.performance_seed)
            .into();
        blocks.push(RenderBlock {
            window,
            audio_clip_indices,
            sequencer_events: events,
        });
        start = end;
    }

    Ok(RenderSchedule {
        format,
        window: request.window,
        block_frames: request.block_frames,
        master,
        audio_clips: clips.into(),
        blocks: blocks.into(),
        buses: buses.into(),
        automation,
        latency,
        tail,
        diagnostics: diagnostics.into(),
    })
}

fn supported_pcm_transform(audio: &arrangement::AudioRegion) -> (bool, Option<&'static str>) {
    if !audio.playback.warp_markers.is_empty() {
        return (false, Some("warp-marker DSP is not implemented"));
    }
    if audio.playback.pitch_semitones != 0.0 {
        return (false, Some("pitch-shift DSP is not implemented"));
    }
    let unity = audio.playback.ratio == arrangement::StretchRatio::unity();
    if unity {
        return (true, None);
    }
    if audio.playback.algorithm == StretchAlgorithm::Resample && !audio.playback.preserve_pitch {
        (true, None)
    } else {
        (
            false,
            Some("pitch-preserving time-stretch DSP is not implemented"),
        )
    }
}

fn compile_buses(
    mixer: &MixerGraph,
    latency: &LatencyPlan,
    effective: &BTreeMap<BusId, crate::mixer::EffectiveBusState>,
    runtimes: &BTreeMap<ProcessorId, ProcessorRuntimeInfo>,
    diagnostics: &mut Vec<RenderDiagnostic>,
) -> Result<Vec<CompiledBus>, CompileError> {
    let routes = mixer.routes();
    let mut indegree: BTreeMap<BusId, usize> = mixer.buses().map(|bus| (bus.id(), 0)).collect();
    for route in &routes {
        *indegree.get_mut(&route.to).expect("validated mixer route") += 1;
    }
    let mut ready: BTreeSet<_> = indegree
        .iter()
        .filter_map(|(&bus, &degree)| (degree == 0).then_some(bus))
        .collect();
    let mut order = Vec::with_capacity(indegree.len());
    while let Some(bus) = ready.pop_first() {
        order.push(bus);
        for route in routes.iter().filter(|route| route.from == bus) {
            let degree = indegree.get_mut(&route.to).expect("validated mixer route");
            *degree -= 1;
            if *degree == 0 {
                ready.insert(route.to);
            }
        }
    }
    if order.len() != indegree.len() {
        return Err(CompileError::Mixer("routing graph contains a cycle".into()));
    }

    let mut result = Vec::with_capacity(order.len());
    for id in order {
        let bus = mixer.bus(id).expect("topological bus exists");
        for slot in bus.inserts() {
            if slot.bypassed() {
                continue;
            }
            let processor = mixer
                .processor(slot.processor_id())
                .expect("validated insert processor");
            let runtime = runtimes.get(&processor.id()).copied().unwrap_or_default();
            let identifier = processor.descriptor().identifier.clone();
            diagnostics.push(if runtime.available {
                RenderDiagnostic::PluginBypassedByReferenceRenderer {
                    processor: processor.id(),
                    identifier,
                }
            } else {
                RenderDiagnostic::PluginUnavailable {
                    processor: processor.id(),
                    identifier,
                }
            });
        }
        let mut compiled_routes = Vec::new();
        if let Some(to) = bus.output() {
            let edge = crate::mixer::RouteEdge {
                from: id,
                to,
                kind: RouteKind::Main,
            };
            compiled_routes.push(CompiledRoute {
                to,
                kind: RouteKind::Main,
                tap: SendTap::PostFader,
                static_gain: 1.0,
                compensation_delay_frames: latency.routes[&edge].compensation_delay_samples,
            });
        }
        for send in bus.sends() {
            let kind = RouteKind::Send(send.id());
            let edge = crate::mixer::RouteEdge {
                from: id,
                to: send.target(),
                kind,
            };
            compiled_routes.push(CompiledRoute {
                to: send.target(),
                kind,
                tap: send.tap(),
                static_gain: if send.muted() {
                    0.0
                } else {
                    db_to_linear(send.level_db())
                },
                compensation_delay_frames: latency.routes[&edge].compensation_delay_samples,
            });
        }
        compiled_routes.sort_by_key(|route| (route.to, route.kind));
        result.push(CompiledBus {
            id,
            audible: effective[&id].audible,
            gain_db: bus.fader().gain_db(),
            pan: bus.fader().pan(),
            routes: compiled_routes.into(),
            insert_latency_frames: latency.buses[&id].insert_latency_samples,
        });
    }
    Ok(result)
}

/// Immutable, interleaved PCM backing one arrangement asset.
#[derive(Clone, Debug)]
pub struct PcmAsset {
    pub format: AudioFormat,
    pub samples: Arc<[f32]>,
    frame_count: u64,
}

impl PcmAsset {
    pub fn new(format: AudioFormat, samples: Arc<[f32]>) -> Result<Self, ReferenceRenderError> {
        let channels = usize::from(format.channels.get());
        if samples.len() % channels != 0 {
            return Err(ReferenceRenderError::PartialAssetFrame {
                samples: samples.len(),
                channels,
            });
        }
        Ok(Self {
            format,
            frame_count: (samples.len() / channels) as u64,
            samples,
        })
    }

    pub const fn frame_count(&self) -> u64 {
        self.frame_count
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReferenceRender {
    pub format: AudioFormat,
    pub window: RenderWindow,
    pub interleaved: Vec<f32>,
    pub diagnostics: Vec<RenderDiagnostic>,
}

/// Render a window through the built-in, plugin-free PCM reference graph.
///
/// Missing assets, unavailable instruments, and unsupported DSP produce
/// silence plus diagnostics. Structural errors and cancellation are returned.
/// Declared plugin latency and tails remain metadata because bypassing a plugin
/// also bypasses its latency; a real plugin executor must implement the frozen
/// [`LatencyPlan`] carried by the schedule.
pub fn render_pcm_reference(
    schedule: &RenderSchedule,
    assets: &BTreeMap<AssetId, PcmAsset>,
    window: RenderWindow,
    cancellation: &RenderCancellation,
) -> Result<ReferenceRender, ReferenceRenderError> {
    if window.start >= window.end
        || window.start < schedule.window.start
        || window.end > schedule.window.end
    {
        return Err(ReferenceRenderError::WindowOutsideSchedule);
    }
    if cancellation.is_cancelled() {
        return Err(ReferenceRenderError::Cancelled);
    }
    let frames = usize::try_from(window.len()).map_err(|_| ReferenceRenderError::RenderTooLarge)?;
    let channels = usize::from(schedule.format.channels.get());
    let sample_count = frames
        .checked_mul(channels)
        .ok_or(ReferenceRenderError::RenderTooLarge)?;
    let mut bus_audio: BTreeMap<BusId, Vec<f32>> = schedule
        .buses
        .iter()
        .map(|bus| (bus.id, vec![0.0; sample_count]))
        .collect();
    let mut diagnostics = schedule.diagnostics.to_vec();
    let mut runtime_diagnostic_keys = BTreeSet::new();

    for clip in schedule
        .audio_clips
        .iter()
        .filter(|clip| clip.renderable && clip.placement.intersects(window))
    {
        if cancellation.is_cancelled() {
            return Err(ReferenceRenderError::Cancelled);
        }
        let Some(asset) = assets.get(&clip.asset) else {
            if runtime_diagnostic_keys.insert((clip.id, clip.asset, 0_u8)) {
                diagnostics.push(RenderDiagnostic::MissingAsset {
                    clip: clip.id,
                    asset: clip.asset,
                });
            }
            continue;
        };
        if asset.format.sample_rate != schedule.format.sample_rate {
            if runtime_diagnostic_keys.insert((clip.id, clip.asset, 1_u8)) {
                diagnostics.push(RenderDiagnostic::InvalidAssetFormat {
                    clip: clip.id,
                    asset: clip.asset,
                    reason: "asset and project sample rates differ",
                });
            }
            continue;
        }
        if clip.source_end > asset.frame_count || !valid_channel_map(&clip.channels, asset.format) {
            if runtime_diagnostic_keys.insert((clip.id, clip.asset, 2_u8)) {
                diagnostics.push(RenderDiagnostic::InvalidAssetFormat {
                    clip: clip.id,
                    asset: clip.asset,
                    reason: "source range or channel map exceeds the asset",
                });
            }
            continue;
        }
        let overlap = clip
            .placement
            .intersection(window)
            .expect("intersecting clip has overlap");
        let target = bus_audio
            .get_mut(&clip.bus)
            .expect("compiled clip bus exists");
        for project_frame in overlap.start..overlap.end {
            let project_offset = (project_frame - clip.placement.start) as u64;
            let Some(source_position) = source_position(clip, project_offset) else {
                continue;
            };
            let (mut left, mut right) = read_source_stereo(asset, &clip.channels, source_position);
            let clip_gain = automated_clip_value(
                &schedule.automation,
                clip.id,
                ClipParameter::Gain,
                project_frame,
                f64::from(clip.clip_gain_db),
            ) as f32;
            let clip_pan = automated_clip_value(
                &schedule.automation,
                clip.id,
                ClipParameter::Pan,
                project_frame,
                0.0,
            ) as f32;
            let fade = fade_gain(clip.fades, clip.placement.len(), project_offset) as f32;
            let gain = db_to_linear(clip.track_gain_db + clip_gain) * fade;
            (left, right) = pan_stereo(
                left * gain,
                right * gain,
                (clip.track_pan + clip_pan).clamp(-1.0, 1.0),
            );
            let output_frame = (project_frame - window.start) as usize;
            if channels == 1 {
                target[output_frame] += (left + right) * 0.5;
            } else {
                target[output_frame * 2] += left;
                target[output_frame * 2 + 1] += right;
            }
        }
    }

    let sequencer_count = schedule
        .blocks
        .iter()
        .filter(|block| block.window.intersects(window))
        .map(|block| block.sequencer_events.len())
        .sum();
    if sequencer_count > 0 {
        diagnostics.push(RenderDiagnostic::SequencerEventsNeedInstrument {
            count: sequencer_count,
        });
    }

    let mut master_output = vec![0.0_f32; sample_count];
    for bus in schedule.buses.iter() {
        if cancellation.is_cancelled() {
            return Err(ReferenceRenderError::Cancelled);
        }
        let pre_fader = bus_audio
            .remove(&bus.id)
            .expect("compiled bus buffer exists");
        for route in bus
            .routes
            .iter()
            .filter(|route| route.tap == SendTap::PreFader)
        {
            if bus.audible {
                add_automated_send(
                    schedule,
                    route,
                    Some(bus.id),
                    window,
                    channels,
                    bus_audio.get_mut(&route.to).expect("route target exists"),
                    &pre_fader,
                );
            }
        }
        let mut post_fader = pre_fader;
        apply_bus_fader(schedule, bus, window, channels, &mut post_fader);
        for route in bus
            .routes
            .iter()
            .filter(|route| route.tap == SendTap::PostFader)
        {
            if route.kind == RouteKind::Main {
                add_scaled(
                    bus_audio.get_mut(&route.to).expect("route target exists"),
                    &post_fader,
                    1.0,
                );
            } else {
                let target = bus_audio.get_mut(&route.to).expect("route target exists");
                add_automated_send(schedule, route, None, window, channels, target, &post_fader);
            }
        }
        if bus.id == schedule.master {
            master_output = post_fader;
        }
    }
    for sample in &mut master_output {
        if !sample.is_finite() {
            *sample = 0.0;
        }
    }
    Ok(ReferenceRender {
        format: schedule.format,
        window,
        interleaved: master_output,
        diagnostics,
    })
}

fn valid_channel_map(mapping: &ChannelMapping, format: AudioFormat) -> bool {
    let channels = format.channels.get();
    match mapping {
        ChannelMapping::All | ChannelMapping::MonoSum => true,
        ChannelMapping::Channels(selected) => selected.iter().all(|channel| *channel < channels),
        ChannelMapping::Mid | ChannelMapping::Side => channels >= 2,
    }
}

fn automated_clip_value(
    automation: &CompiledAutomation,
    clip: ClipId,
    parameter: ClipParameter,
    frame: i64,
    base: f64,
) -> f64 {
    automation
        .value_at(
            &ParameterAddress::Clip {
                clip_id: clip.get(),
                parameter,
            },
            automation::ProjectFrame(frame),
            base,
        )
        .unwrap_or(base)
}

fn source_position(clip: &CompiledAudioClip, project_offset: u64) -> Option<f64> {
    let source_len = clip.source_end.checked_sub(clip.source_start)?;
    let relative =
        project_offset as f64 * clip.ratio_source_frames as f64 / clip.ratio_project_frames as f64;
    if relative >= source_len as f64 {
        return None;
    }
    let mut absolute = clip.source_start as f64 + relative;
    absolute = match clip.loop_mode {
        AudioLoopMode::Off => absolute,
        AudioLoopMode::Forward(range) => {
            if absolute < range.start as f64 {
                absolute
            } else {
                range.start as f64 + (absolute - range.start as f64) % range.len() as f64
            }
        }
        AudioLoopMode::PingPong(range) => {
            if absolute < range.start as f64 {
                absolute
            } else {
                let span = range.len().saturating_sub(1) as f64;
                if span == 0.0 {
                    range.start as f64
                } else {
                    let phase = (absolute - range.start as f64) % (2.0 * span);
                    range.start as f64
                        + if phase <= span {
                            phase
                        } else {
                            2.0 * span - phase
                        }
                }
            }
        }
    };
    if clip.reverse {
        absolute = clip.source_start as f64 + (clip.source_end - 1) as f64 - absolute;
    }
    Some(absolute.clamp(clip.source_start as f64, (clip.source_end - 1) as f64))
}

fn read_source_stereo(asset: &PcmAsset, mapping: &ChannelMapping, position: f64) -> (f32, f32) {
    let lower = position.floor() as u64;
    let upper = lower
        .saturating_add(1)
        .min(asset.frame_count.saturating_sub(1));
    let fraction = (position - lower as f64) as f32;
    let sample = |frame: u64, channel: u16| {
        let index =
            frame as usize * usize::from(asset.format.channels.get()) + usize::from(channel);
        asset.samples[index]
    };
    let interpolate = |channel: u16| {
        let first = sample(lower, channel);
        first + (sample(upper, channel) - first) * fraction
    };
    let source_channels = asset.format.channels.get();
    match mapping {
        ChannelMapping::All if source_channels == 1 => {
            let value = interpolate(0);
            (value, value)
        }
        ChannelMapping::All => (interpolate(0), interpolate(1)),
        ChannelMapping::Channels(selected) if selected.len() == 1 => {
            let value = interpolate(selected[0]);
            (value, value)
        }
        ChannelMapping::Channels(selected) => (interpolate(selected[0]), interpolate(selected[1])),
        ChannelMapping::MonoSum => {
            let value =
                (0..source_channels).map(interpolate).sum::<f32>() / f32::from(source_channels);
            (value, value)
        }
        ChannelMapping::Mid => {
            let value = (interpolate(0) + interpolate(1)) * 0.5;
            (value, value)
        }
        ChannelMapping::Side => {
            let value = (interpolate(0) - interpolate(1)) * 0.5;
            (value, -value)
        }
    }
}

fn fade_gain(fades: ClipFades, clip_len: u64, offset: u64) -> f64 {
    let mut gain = 1.0;
    if let Some(fade) = fades.fade_in {
        if offset <= fade.duration {
            gain *= fade_amount(fade, offset);
        }
    }
    if let Some(fade) = fades.fade_out {
        let start = clip_len - fade.duration;
        if offset >= start {
            gain *= 1.0 - fade_amount(fade, offset - start);
        }
    }
    gain.clamp(0.0, 1.0)
}

fn fade_amount(fade: Fade, offset: u64) -> f64 {
    let t = (offset as f64 / fade.duration as f64).clamp(0.0, 1.0);
    let phase = fade.phase_start + (fade.phase_end - fade.phase_start) * t;
    match fade.curve {
        FadeCurve::Linear => phase,
        FadeCurve::EqualPower => (phase * std::f64::consts::FRAC_PI_2).sin(),
        FadeCurve::SmoothStep => phase * phase * (3.0 - 2.0 * phase),
    }
}

fn apply_bus_fader(
    schedule: &RenderSchedule,
    bus: &CompiledBus,
    window: RenderWindow,
    channels: usize,
    audio: &mut [f32],
) {
    for frame in 0..window.len() as usize {
        let absolute = window.start.saturating_add(frame as i64);
        let gain_db = schedule
            .automation
            .value_at(
                &ParameterAddress::Mixer(MixerTarget::BusGain(bus.id.get())),
                automation::ProjectFrame(absolute),
                f64::from(bus.gain_db),
            )
            .unwrap_or(f64::from(bus.gain_db)) as f32;
        let pan = schedule
            .automation
            .value_at(
                &ParameterAddress::Mixer(MixerTarget::BusPan(bus.id.get())),
                automation::ProjectFrame(absolute),
                f64::from(bus.pan),
            )
            .unwrap_or(f64::from(bus.pan)) as f32;
        let automation_muted = schedule
            .automation
            .value_at(
                &ParameterAddress::Mixer(MixerTarget::BusMute(bus.id.get())),
                automation::ProjectFrame(absolute),
                0.0,
            )
            .map(|value| value >= 0.5)
            .unwrap_or(false);
        let muted = !bus.audible || automation_muted;
        let gain = if muted { 0.0 } else { db_to_linear(gain_db) };
        if channels == 1 {
            audio[frame] *= gain;
        } else {
            let index = frame * 2;
            let (left, right) = pan_stereo(audio[index] * gain, audio[index + 1] * gain, pan);
            audio[index] = left;
            audio[index + 1] = right;
        }
    }
}

fn route_gain_at(schedule: &RenderSchedule, route: &CompiledRoute, frame: i64) -> f32 {
    match route.kind {
        RouteKind::Main => 1.0,
        RouteKind::Send(send) => {
            let level_db = linear_to_db(route.static_gain);
            let level = schedule
                .automation
                .value_at(
                    &ParameterAddress::Mixer(MixerTarget::SendLevel(send.get())),
                    automation::ProjectFrame(frame),
                    f64::from(level_db),
                )
                .unwrap_or(f64::from(level_db)) as f32;
            let muted = schedule
                .automation
                .value_at(
                    &ParameterAddress::Mixer(MixerTarget::SendMute(send.get())),
                    automation::ProjectFrame(frame),
                    if route.static_gain == 0.0 { 1.0 } else { 0.0 },
                )
                .map(|value| value >= 0.5)
                .unwrap_or(route.static_gain == 0.0);
            if muted {
                0.0
            } else {
                db_to_linear(level)
            }
        }
    }
}

fn add_automated_send(
    schedule: &RenderSchedule,
    route: &CompiledRoute,
    pre_fader_source: Option<BusId>,
    window: RenderWindow,
    channels: usize,
    target: &mut [f32],
    source: &[f32],
) {
    for frame in 0..window.len() as usize {
        let absolute = window.start + frame as i64;
        let source_muted = pre_fader_source.is_some_and(|bus| {
            schedule
                .automation
                .value_at(
                    &ParameterAddress::Mixer(MixerTarget::BusMute(bus.get())),
                    automation::ProjectFrame(absolute),
                    0.0,
                )
                .is_some_and(|value| value >= 0.5)
        });
        let gain = if source_muted {
            0.0
        } else {
            route_gain_at(schedule, route, absolute)
        };
        let start = frame * channels;
        for channel in 0..channels {
            target[start + channel] += source[start + channel] * gain;
        }
    }
}

fn add_scaled(target: &mut [f32], source: &[f32], gain: f32) {
    for (target, source) in target.iter_mut().zip(source) {
        *target += *source * gain;
    }
}

fn pan_stereo(left: f32, right: f32, pan: f32) -> (f32, f32) {
    let pan = pan.clamp(-1.0, 1.0);
    if pan == 0.0 {
        return (left, right);
    }
    let theta = (pan + 1.0) * FRAC_PI_4;
    let normalization = 2.0_f32.sqrt();
    (
        left * theta.cos() * normalization,
        right * theta.sin() * normalization,
    )
}

fn db_to_linear(db: f32) -> f32 {
    if db <= -144.0 {
        0.0
    } else {
        10.0_f32.powf(db / 20.0)
    }
}

fn linear_to_db(linear: f32) -> f32 {
    if linear <= 0.0 {
        -144.0
    } else {
        20.0 * linear.log10()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompileError {
    EmptyWindow { start: i64, end: i64 },
    ZeroBlockFrames,
    UnsupportedOutputChannels(u16),
    SampleRateMismatch { arrangement: u32, sequencer: u32 },
    Arrangement(String),
    Sequencer(String),
    Automation(String),
    Mixer(String),
    AudioFormat(String),
    Cancelled,
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyWindow { start, end } => write!(f, "render window is empty: {start}..{end}"),
            Self::ZeroBlockFrames => write!(f, "render block size must be nonzero"),
            Self::UnsupportedOutputChannels(channels) => {
                write!(f, "reference schedule supports mono or stereo, got {channels} channels")
            }
            Self::SampleRateMismatch { arrangement, sequencer } => write!(
                f,
                "arrangement sample rate {arrangement} differs from sequencer sample rate {sequencer}"
            ),
            Self::Arrangement(message) => write!(f, "invalid arrangement: {message}"),
            Self::Sequencer(message) => write!(f, "invalid sequencer: {message}"),
            Self::Automation(message) => write!(f, "invalid automation: {message}"),
            Self::Mixer(message) => write!(f, "invalid mixer: {message}"),
            Self::AudioFormat(message) => write!(f, "invalid render format: {message}"),
            Self::Cancelled => write!(f, "render schedule compilation cancelled"),
        }
    }
}

impl Error for CompileError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReferenceRenderError {
    WindowOutsideSchedule,
    PartialAssetFrame { samples: usize, channels: usize },
    RenderTooLarge,
    Cancelled,
}

impl fmt::Display for ReferenceRenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WindowOutsideSchedule => {
                write!(f, "render window is outside the compiled schedule")
            }
            Self::PartialAssetFrame { samples, channels } => write!(
                f,
                "asset has {samples} samples, not a whole number of {channels}-channel frames"
            ),
            Self::RenderTooLarge => write!(f, "render window is too large for this platform"),
            Self::Cancelled => write!(f, "reference render cancelled"),
        }
    }
}

impl Error for ReferenceRenderError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrangement::{ArrangementEditor, Frame, FrameRange, SourceRange, TrackKind};
    use crate::automation::{
        ParameterDescriptor, ParameterUnit, SegmentShape, SmoothingPolicy, TimeDomain,
        TimePosition, ValueMapping,
    };
    use crate::mixer::{BusKind, PluginDescriptor};
    use crate::sequencer::TempoMap;

    struct Fixture {
        arrangement: arrangement::ArrangementState,
        sequencer: Sequencer,
        automation: AutomationGraph,
        mixer: MixerGraph,
        track_buses: BTreeMap<TrackId, BusId>,
        processors: BTreeMap<ProcessorId, ProcessorRuntimeInfo>,
        track: TrackId,
        clip: ClipId,
        asset: AssetId,
    }

    impl Fixture {
        fn new() -> Self {
            let mut editor = ArrangementEditor::new(48_000).unwrap();
            let track = editor.create_track("Audio", TrackKind::Audio).unwrap();
            let asset = AssetId::from_raw(9);
            let clip = editor
                .create_audio_clip(
                    track,
                    "Clip",
                    FrameRange::new(Frame(1), Frame(5)).unwrap(),
                    asset,
                    SourceRange::new(0, 4).unwrap(),
                )
                .unwrap();
            let mut mixer = MixerGraph::new("Master");
            let bus = mixer.add_bus(BusKind::Source, "Audio").unwrap();
            Self {
                arrangement: editor.state().clone(),
                sequencer: Sequencer::new(TempoMap::common_time(48_000, 120.0).unwrap()),
                automation: AutomationGraph::new(),
                mixer,
                track_buses: BTreeMap::from([(track, bus)]),
                processors: BTreeMap::new(),
                track,
                clip,
                asset,
            }
        }

        fn compile(&self, window: RenderWindow, block_frames: u32) -> RenderSchedule {
            compile_render_schedule(
                RenderCompileRequest {
                    arrangement: &self.arrangement,
                    sequencer: &self.sequencer,
                    automation: &self.automation,
                    mixer: &self.mixer,
                    track_buses: &self.track_buses,
                    processors: &self.processors,
                    window,
                    output_channels: 2,
                    block_frames,
                    performance_seed: 7,
                },
                &RenderCancellation::new(),
            )
            .unwrap()
        }
    }

    #[test]
    fn blocks_and_clip_membership_are_exact_and_end_exclusive() {
        let fixture = Fixture::new();
        let schedule = fixture.compile(RenderWindow::new(0, 6).unwrap(), 2);
        assert_eq!(schedule.blocks.len(), 3);
        assert_eq!(schedule.blocks[0].window, RenderWindow { start: 0, end: 2 });
        assert_eq!(&*schedule.blocks[0].audio_clip_indices, &[0]);
        assert_eq!(&*schedule.blocks[1].audio_clip_indices, &[0]);
        assert_eq!(&*schedule.blocks[2].audio_clip_indices, &[0]);
        assert!(schedule.block_at(5).is_some());
        assert!(schedule.block_at(6).is_none());
    }

    #[test]
    fn pcm_reference_obeys_placement_and_source_boundaries() {
        let fixture = Fixture::new();
        let schedule = fixture.compile(RenderWindow::new(0, 6).unwrap(), 3);
        let format = AudioFormat::new(48_000, 1).unwrap();
        let assets = BTreeMap::from([(
            fixture.asset,
            PcmAsset::new(format, Arc::from([1.0, 2.0, 3.0, 4.0])).unwrap(),
        )]);
        let rendered = render_pcm_reference(
            &schedule,
            &assets,
            schedule.window(),
            &RenderCancellation::new(),
        )
        .unwrap();
        let left: Vec<_> = rendered
            .interleaved
            .chunks_exact(2)
            .map(|frame| frame[0])
            .collect();
        assert_eq!(left, vec![0.0, 1.0, 2.0, 3.0, 4.0, 0.0]);
    }

    #[test]
    fn reverse_and_linear_fades_render_without_touching_assets() {
        let mut fixture = Fixture::new();
        let clip = fixture.arrangement.clips.get_mut(&fixture.clip).unwrap();
        let ClipContent::Audio(audio) = &mut clip.content else {
            unreachable!()
        };
        audio.playback.reverse = true;
        clip.fades.fade_in = Some(Fade::full(2, FadeCurve::Linear));
        clip.fades.fade_out = Some(Fade::full(2, FadeCurve::Linear));
        let schedule = fixture.compile(RenderWindow::new(1, 5).unwrap(), 4);
        let assets = BTreeMap::from([(
            fixture.asset,
            PcmAsset::new(
                AudioFormat::new(48_000, 1).unwrap(),
                Arc::from([1.0, 2.0, 3.0, 4.0]),
            )
            .unwrap(),
        )]);
        let rendered = render_pcm_reference(
            &schedule,
            &assets,
            schedule.window(),
            &RenderCancellation::new(),
        )
        .unwrap();
        let left: Vec<_> = rendered
            .interleaved
            .chunks_exact(2)
            .map(|frame| frame[0])
            .collect();
        assert_eq!(left, vec![0.0, 1.5, 2.0, 0.5]);
    }

    #[test]
    fn forward_loop_uses_half_open_source_range_without_endpoint_duplication() {
        let mut fixture = Fixture::new();
        let clip = fixture.arrangement.clips.get_mut(&fixture.clip).unwrap();
        let ClipContent::Audio(audio) = &mut clip.content else {
            unreachable!()
        };
        audio.loop_mode = AudioLoopMode::Forward(SourceRange::new(1, 3).unwrap());
        let schedule = fixture.compile(RenderWindow::new(1, 5).unwrap(), 4);
        let assets = BTreeMap::from([(
            fixture.asset,
            PcmAsset::new(
                AudioFormat::new(48_000, 1).unwrap(),
                Arc::from([1.0, 2.0, 3.0, 4.0]),
            )
            .unwrap(),
        )]);
        let rendered = render_pcm_reference(
            &schedule,
            &assets,
            schedule.window(),
            &RenderCancellation::new(),
        )
        .unwrap();
        let left: Vec<_> = rendered
            .interleaved
            .chunks_exact(2)
            .map(|frame| frame[0])
            .collect();
        assert_eq!(left, vec![1.0, 2.0, 3.0, 2.0]);
    }

    #[test]
    fn clip_gain_automation_is_bound_at_each_exact_project_frame() {
        let mut fixture = Fixture::new();
        let address = ParameterAddress::Clip {
            clip_id: fixture.clip.get(),
            parameter: ClipParameter::Gain,
        };
        fixture
            .automation
            .register_parameter(ParameterDescriptor {
                address: address.clone(),
                name: "Clip gain".into(),
                unit: ParameterUnit::Decibels,
                minimum: -144.0,
                maximum: 48.0,
                default: 0.0,
                mapping: ValueMapping::Linear,
                smoothing: SmoothingPolicy::None,
            })
            .unwrap();
        let lane = fixture
            .automation
            .create_lane("Gain", address, TimeDomain::Frames)
            .unwrap();
        fixture
            .automation
            .insert_point(
                lane,
                TimePosition::Frames(automation::ProjectFrame(1)),
                -6.0,
                SegmentShape::Hold,
            )
            .unwrap();
        let schedule = fixture.compile(RenderWindow::new(1, 5).unwrap(), 4);
        let assets = BTreeMap::from([(
            fixture.asset,
            PcmAsset::new(
                AudioFormat::new(48_000, 1).unwrap(),
                Arc::from([1.0, 1.0, 1.0, 1.0]),
            )
            .unwrap(),
        )]);
        let rendered = render_pcm_reference(
            &schedule,
            &assets,
            schedule.window(),
            &RenderCancellation::new(),
        )
        .unwrap();
        let expected = 10.0_f32.powf(-6.0 / 20.0);
        for frame in rendered.interleaved.chunks_exact(2) {
            assert!((frame[0] - expected).abs() < 1.0e-6);
        }
    }

    #[test]
    fn plugin_availability_and_tail_are_frozen_without_claiming_plugin_dsp() {
        let mut fixture = Fixture::new();
        let source = fixture.track_buses[&fixture.track];
        let processor = fixture
            .mixer
            .insert_processor(
                source,
                None,
                PluginDescriptor::new("clap", "org.example.echo", "Echo"),
                64,
            )
            .unwrap();
        fixture.processors.insert(
            processor,
            ProcessorRuntimeInfo {
                available: true,
                tail_frames: 480,
            },
        );
        let schedule = fixture.compile(RenderWindow::new(0, 6).unwrap(), 2);
        assert_eq!(schedule.tail().master_latency_frames, 64);
        assert_eq!(schedule.tail().maximum_declared_plugin_tail_frames, 480);
        assert_eq!(schedule.tail().scheduled_end_with_tail, 486);
        assert!(schedule.diagnostics().contains(
            &RenderDiagnostic::PluginBypassedByReferenceRenderer {
                processor,
                identifier: "org.example.echo".into(),
            }
        ));
    }

    #[test]
    fn missing_assets_are_deterministic_silence_with_a_diagnostic() {
        let fixture = Fixture::new();
        let schedule = fixture.compile(RenderWindow::new(0, 6).unwrap(), 2);
        let rendered = render_pcm_reference(
            &schedule,
            &BTreeMap::new(),
            schedule.window(),
            &RenderCancellation::new(),
        )
        .unwrap();
        assert!(rendered.interleaved.iter().all(|sample| *sample == 0.0));
        assert!(rendered
            .diagnostics
            .contains(&RenderDiagnostic::MissingAsset {
                clip: fixture.clip,
                asset: fixture.asset,
            }));
    }

    #[test]
    fn unsupported_stretch_is_compiled_as_explicit_silence() {
        let mut fixture = Fixture::new();
        let clip = fixture.arrangement.clips.get_mut(&fixture.clip).unwrap();
        let ClipContent::Audio(audio) = &mut clip.content else {
            unreachable!()
        };
        audio.playback.pitch_semitones = 3.0;
        let schedule = fixture.compile(RenderWindow::new(0, 6).unwrap(), 2);
        assert!(!schedule.audio_clips[0].renderable);
        assert!(matches!(
            schedule.diagnostics[0],
            RenderDiagnostic::UnsupportedTimeTransform { .. }
        ));
    }

    #[test]
    fn cancellation_stops_compile_and_render() {
        let fixture = Fixture::new();
        let cancellation = RenderCancellation::new();
        cancellation.cancel();
        let result = compile_render_schedule(
            RenderCompileRequest {
                arrangement: &fixture.arrangement,
                sequencer: &fixture.sequencer,
                automation: &fixture.automation,
                mixer: &fixture.mixer,
                track_buses: &fixture.track_buses,
                processors: &fixture.processors,
                window: RenderWindow::new(0, 6).unwrap(),
                output_channels: 2,
                block_frames: 2,
                performance_seed: 0,
            },
            &cancellation,
        );
        assert!(matches!(result, Err(CompileError::Cancelled)));
    }

    #[test]
    fn mixer_buses_are_compiled_in_stable_topological_order() {
        let mut fixture = Fixture::new();
        let group = fixture.mixer.add_bus(BusKind::Group, "Group").unwrap();
        let source = fixture.track_buses[&fixture.track];
        fixture.mixer.set_output(source, group).unwrap();
        let schedule = fixture.compile(RenderWindow::new(0, 6).unwrap(), 2);
        let order: Vec<_> = schedule.buses.iter().map(|bus| bus.id).collect();
        assert_eq!(order, vec![source, group, fixture.mixer.master()]);
    }
}
