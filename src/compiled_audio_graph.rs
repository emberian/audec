//! Preallocated execution of one already-frozen DAW schedule.
//!
//! This module is deliberately downstream of [`DawEngineSchedule`]. It does
//! not inspect editable project state, schedule musical events, assign render
//! identity, discover plug-ins, or invent another audio truth. A
//! [`CompiledGraph`] is an executable lowering of the existing immutable
//! [`RenderPlan`] and [`DawEngineSchedule`]. Both the background/offline and
//! realtime contracts invoke the same [`ExecutionKernel`].
//!
//! The native vocabulary includes arrangement clips, explicitly-routed sampler
//! and synth event streams, compiled automation, ordered bus/send mixing,
//! bounded delay, sanitization and meter taps. Frozen PCM remains an explicit
//! adapter for external products. Unsupported processors remain typed refusals;
//! they never silently become an identity or a claimed render.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::automation;
use crate::daw_engine::{BuiltInInstrumentDefinition, DawEngineSchedule};
use crate::daw_render::{
    self, CompiledAudioClip, CompiledBus, CompiledRoute, MediaAssetDescriptor, MediaBlockDemand,
    MediaBlockProvider, MediaBlockSource, MediaPreparationError, MediaReadError,
    RenderCancellation, RenderWindow, ResidentPcmProvider,
};
use crate::instruments::BuiltInInstrument;
use crate::mixer::{BusId, RouteKind, SendTap};
use crate::render_plan::{
    ExactDigest, ProjectRevisionStamp, RenderFormat, RenderPlan, RenderScope, RenderSpan,
    Tileability,
};
use crate::sequencer::{ScheduledEvent, ScheduledKind, TriggerTarget};

pub const MAX_METER_CHANNELS: usize = 32;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GraphNodeId(u32);

impl GraphNodeId {
    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MeterTapId(pub u64);

/// A PCM product rendered from the frozen schedule and retained as an input
/// adapter while native source nodes are introduced incrementally.
#[derive(Clone, Debug)]
pub struct FrozenPcmProduct {
    pub scope: RenderScope,
    pub span: RenderSpan,
    pub content: ExactDigest,
    pub interleaved: Arc<[f32]>,
}

/// Deterministic ordering for events at the same project frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimestampedGain {
    pub project_frame: i64,
    pub sequence: u32,
    pub linear: f32,
}

/// Context and output extent a node needs beyond the requested body.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NodeTiming {
    pub latency_frames: u64,
    pub tail_frames: u64,
    pub lookbehind_frames: u64,
    pub lookahead_frames: u64,
}

impl NodeTiming {
    fn through(self, node: Self) -> Self {
        Self {
            latency_frames: self.latency_frames.saturating_add(node.latency_frames),
            tail_frames: self.tail_frames.saturating_add(node.tail_frames),
            lookbehind_frames: self
                .lookbehind_frames
                .saturating_add(node.lookbehind_frames),
            lookahead_frames: self.lookahead_frames.saturating_add(node.lookahead_frames),
        }
    }

    fn merge_parallel(self, other: Self) -> Self {
        Self {
            latency_frames: self.latency_frames.max(other.latency_frames),
            tail_frames: self.tail_frames.max(other.tail_frames),
            lookbehind_frames: self.lookbehind_frames.max(other.lookbehind_frames),
            lookahead_frames: self.lookahead_frames.max(other.lookahead_frames),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessorRefusalReason {
    UnsupportedProcessor,
    RealtimeUnsafe,
    UnknownLatency,
    UnknownTail,
    NonDeterministic,
    StateCannotCheckpoint,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphDiagnostic {
    FrozenPcmAdapter {
        node: GraphNodeId,
        scope: RenderScope,
        content: ExactDigest,
    },
    ProcessorRefused {
        label: Arc<str>,
        reason: ProcessorRefusalReason,
        required: bool,
    },
    PlanTileabilityMoreConservative {
        plan: Tileability,
        native: Tileability,
    },
    /// PDC metadata was retained, but its processor is bypassed by the native
    /// built-in graph, so applying the corresponding route delay would create
    /// latency which the audible source does not contain.
    CompensationBypassed {
        bus: u64,
        route_to: u64,
        frames: u64,
    },
}

/// Static callback promises. Storage is allocated by executor construction
/// and never grows in the processing method.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RealtimeContract {
    pub maximum_block_frames: u32,
    pub allocation_free_process: bool,
    pub lock_free_process: bool,
    pub io_free_process: bool,
    pub logging_free_process: bool,
    pub graph_mutation_free_process: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TileExecutionContract {
    pub preroll_frames: u64,
    pub postroll_frames: u64,
    pub checkpoint_required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TileRefusal {
    PrerollCeilingExceeded { required: u64, ceiling: u64 },
    LookaheadCeilingExceeded { required: u64, ceiling: u64 },
    CheckpointImplementationPending,
    SequentialOnly,
    RequiredProcessorRefused,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeterSnapshot {
    pub channels: u16,
    pub frames_observed: u64,
    pub latest_peak: [f32; MAX_METER_CHANNELS],
    pub integrated_peak: [f32; MAX_METER_CHANNELS],
    pub integrated_rms: [f32; MAX_METER_CHANNELS],
}

impl Default for MeterSnapshot {
    fn default() -> Self {
        Self {
            channels: 0,
            frames_observed: 0,
            latest_peak: [0.0; MAX_METER_CHANNELS],
            integrated_peak: [0.0; MAX_METER_CHANNELS],
            integrated_rms: [0.0; MAX_METER_CHANNELS],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeterReading {
    pub id: MeterTapId,
    pub snapshot: MeterSnapshot,
}

#[derive(Clone, Debug)]
struct MeterTap {
    id: MeterTapId,
    node: GraphNodeId,
}

#[derive(Clone, Debug)]
struct MixInput {
    node: GraphNodeId,
    gain: f32,
}

#[derive(Clone, Debug)]
enum NativeNode {
    Silence,
    FrozenPcm(FrozenPcmProduct),
    Gain {
        input: GraphNodeId,
        initial_linear: f32,
        events: Arc<[TimestampedGain]>,
    },
    Mix {
        inputs: Arc<[MixInput]>,
    },
    Delay {
        input: GraphNodeId,
        frames: u32,
    },
    /// One arrangement clip, its immutable media and compiled automation.
    AudioClip {
        clip: CompiledAudioClip,
        descriptor: MediaAssetDescriptor,
        automation: Arc<automation::CompiledAutomation>,
    },
    /// One explicitly-routed built-in voice bank. Events remain data in the
    /// graph and are rebased into the device block without callback allocation.
    Instrument {
        identity: u64,
        definition: BuiltInInstrumentDefinition,
        events: Arc<[ScheduledEvent]>,
    },
    BusFader {
        input: GraphNodeId,
        bus: CompiledBus,
        automation: Arc<automation::CompiledAutomation>,
    },
    Send {
        input: GraphNodeId,
        route: CompiledRoute,
        pre_fader_bus: Option<CompiledBus>,
        automation: Arc<automation::CompiledAutomation>,
    },
    Sanitize {
        input: GraphNodeId,
    },
}

impl NativeNode {
    fn timing(&self, prior: &[NodeTiming]) -> NodeTiming {
        match self {
            Self::Silence | Self::FrozenPcm(_) | Self::AudioClip { .. } => NodeTiming::default(),
            Self::Instrument { .. } => NodeTiming::default(),
            Self::Gain { input, .. } => prior[input.0 as usize],
            Self::Mix { inputs } => inputs.iter().fold(NodeTiming::default(), |timing, input| {
                timing.merge_parallel(prior[input.node.0 as usize])
            }),
            Self::Delay { input, frames } => prior[input.0 as usize].through(NodeTiming {
                latency_frames: u64::from(*frames),
                tail_frames: u64::from(*frames),
                lookbehind_frames: u64::from(*frames),
                lookahead_frames: 0,
            }),
            Self::BusFader { input, .. } | Self::Send { input, .. } | Self::Sanitize { input } => {
                prior[input.0 as usize]
            }
        }
    }
}

#[derive(Clone, Debug)]
enum ScheduleAnchor {
    Daw(Arc<DawEngineSchedule>),
    #[cfg(test)]
    KernelFixture,
}

/// Control-thread builder. Nodes can only read earlier nodes, making the
/// resulting array a deterministic execution order.
pub struct CompiledGraphBuilder {
    plan: Arc<RenderPlan>,
    source_schedule: ScheduleAnchor,
    nodes: Vec<NativeNode>,
    timings: Vec<NodeTiming>,
    output: Option<GraphNodeId>,
    meter_taps: Vec<MeterTap>,
    diagnostics: Vec<GraphDiagnostic>,
    required_refusals: usize,
    media_provider: Arc<dyn MediaBlockProvider>,
}

impl CompiledGraphBuilder {
    pub fn new(
        plan: Arc<RenderPlan>,
        source_schedule: Arc<DawEngineSchedule>,
    ) -> Result<Self, GraphCompileError> {
        let provider: Arc<dyn MediaBlockProvider> = Arc::new(ResidentPcmProvider::new(Arc::new(
            source_schedule.assets().clone(),
        )));
        Self::new_with_media(plan, source_schedule, provider)
    }

    pub fn new_with_media(
        plan: Arc<RenderPlan>,
        source_schedule: Arc<DawEngineSchedule>,
        media_provider: Arc<dyn MediaBlockProvider>,
    ) -> Result<Self, GraphCompileError> {
        validate_plan_schedule(&plan, &source_schedule)?;
        Ok(Self::with_anchor(
            plan,
            ScheduleAnchor::Daw(source_schedule),
            media_provider,
        ))
    }

    #[cfg(test)]
    fn for_test_plan(plan: Arc<RenderPlan>) -> Self {
        Self::with_anchor(
            plan,
            ScheduleAnchor::KernelFixture,
            Arc::new(ResidentPcmProvider::new(Arc::new(BTreeMap::new()))),
        )
    }

    fn with_anchor(
        plan: Arc<RenderPlan>,
        source_schedule: ScheduleAnchor,
        media_provider: Arc<dyn MediaBlockProvider>,
    ) -> Self {
        Self {
            plan,
            source_schedule,
            nodes: Vec::new(),
            timings: Vec::new(),
            output: None,
            meter_taps: Vec::new(),
            diagnostics: Vec::new(),
            required_refusals: 0,
            media_provider,
        }
    }

    fn add_silence(&mut self) -> Result<GraphNodeId, GraphCompileError> {
        self.push_node(NativeNode::Silence)
    }

    pub fn add_frozen_pcm(
        &mut self,
        product: FrozenPcmProduct,
    ) -> Result<GraphNodeId, GraphCompileError> {
        if !self.plan.extent().contains_span(product.span) {
            return Err(GraphCompileError::SourceOutsidePlan {
                source: product.span,
                plan: self.plan.extent(),
            });
        }
        let channels = usize::from(self.plan.format().channels.get());
        let expected = usize::try_from(product.span.len())
            .ok()
            .and_then(|frames| frames.checked_mul(channels))
            .ok_or(GraphCompileError::GraphTooLarge)?;
        if product.interleaved.len() != expected {
            return Err(GraphCompileError::SourceSampleCount {
                expected,
                actual: product.interleaved.len(),
            });
        }
        if let Some(index) = product
            .interleaved
            .iter()
            .position(|sample| !sample.is_finite())
        {
            return Err(GraphCompileError::NonFiniteSource { index });
        }
        let scope = product.scope.clone();
        let content = product.content;
        let id = self.push_node(NativeNode::FrozenPcm(product))?;
        self.diagnostics.push(GraphDiagnostic::FrozenPcmAdapter {
            node: id,
            scope,
            content,
        });
        Ok(id)
    }

    pub fn add_gain(
        &mut self,
        input: GraphNodeId,
        initial_linear: f32,
        mut events: Vec<TimestampedGain>,
    ) -> Result<GraphNodeId, GraphCompileError> {
        self.require_node(input)?;
        if !initial_linear.is_finite() {
            return Err(GraphCompileError::NonFiniteGain);
        }
        events.sort_by_key(|event| (event.project_frame, event.sequence));
        for event in &events {
            if !event.linear.is_finite() {
                return Err(GraphCompileError::NonFiniteGain);
            }
            if !self.plan.extent().contains(event.project_frame) {
                return Err(GraphCompileError::EventOutsidePlan {
                    frame: event.project_frame,
                    plan: self.plan.extent(),
                });
            }
        }
        for pair in events.windows(2) {
            if (pair[0].project_frame, pair[0].sequence)
                == (pair[1].project_frame, pair[1].sequence)
            {
                return Err(GraphCompileError::DuplicateEventOrder {
                    frame: pair[0].project_frame,
                    sequence: pair[0].sequence,
                });
            }
        }
        self.push_node(NativeNode::Gain {
            input,
            initial_linear,
            events: events.into(),
        })
    }

    pub fn add_mix(
        &mut self,
        inputs: impl IntoIterator<Item = (GraphNodeId, f32)>,
    ) -> Result<GraphNodeId, GraphCompileError> {
        let inputs: Vec<_> = inputs
            .into_iter()
            .map(|(node, gain)| MixInput { node, gain })
            .collect();
        if inputs.is_empty() {
            return Err(GraphCompileError::EmptyMix);
        }
        for input in &inputs {
            self.require_node(input.node)?;
            if !input.gain.is_finite() {
                return Err(GraphCompileError::NonFiniteGain);
            }
        }
        self.push_node(NativeNode::Mix {
            inputs: inputs.into(),
        })
    }

    pub fn add_delay(
        &mut self,
        input: GraphNodeId,
        frames: u32,
    ) -> Result<GraphNodeId, GraphCompileError> {
        self.require_node(input)?;
        if frames == 0 {
            return Err(GraphCompileError::ZeroDelay);
        }
        self.push_node(NativeNode::Delay { input, frames })
    }

    fn add_audio_clip(
        &mut self,
        clip: CompiledAudioClip,
        descriptor: MediaAssetDescriptor,
        automation: Arc<automation::CompiledAutomation>,
    ) -> Result<GraphNodeId, GraphCompileError> {
        self.push_node(NativeNode::AudioClip {
            clip,
            descriptor,
            automation,
        })
    }

    fn add_instrument(
        &mut self,
        identity: u64,
        definition: BuiltInInstrumentDefinition,
        events: Arc<[ScheduledEvent]>,
    ) -> Result<GraphNodeId, GraphCompileError> {
        // Retained voice state makes the node stateful; random access is
        // exact when a tile prerolls the longest voice this event stream can
        // leave sounding. A voice that can last indefinitely (a synth note
        // without note-off) keeps the honest ceiling: the whole extent.
        let extent = self.plan.extent().len();
        let lookbehind =
            instrument_history_bound(&definition, &events, self.plan.format().sample_rate.get())
                .map_or(extent, |bound| bound.min(extent));
        let node = self.push_node(NativeNode::Instrument {
            identity,
            definition,
            events,
        })?;
        self.timings[node.0 as usize].lookbehind_frames = lookbehind;
        Ok(node)
    }

    fn add_bus_fader(
        &mut self,
        input: GraphNodeId,
        bus: CompiledBus,
        automation: Arc<automation::CompiledAutomation>,
    ) -> Result<GraphNodeId, GraphCompileError> {
        self.require_node(input)?;
        self.push_node(NativeNode::BusFader {
            input,
            bus,
            automation,
        })
    }

    fn add_send(
        &mut self,
        input: GraphNodeId,
        route: CompiledRoute,
        pre_fader_bus: Option<CompiledBus>,
        automation: Arc<automation::CompiledAutomation>,
    ) -> Result<GraphNodeId, GraphCompileError> {
        self.require_node(input)?;
        self.push_node(NativeNode::Send {
            input,
            route,
            pre_fader_bus,
            automation,
        })
    }

    fn add_sanitize(&mut self, input: GraphNodeId) -> Result<GraphNodeId, GraphCompileError> {
        self.require_node(input)?;
        self.push_node(NativeNode::Sanitize { input })
    }

    pub fn set_output(&mut self, node: GraphNodeId) -> Result<(), GraphCompileError> {
        self.require_node(node)?;
        self.output = Some(node);
        Ok(())
    }

    pub fn add_meter_tap(
        &mut self,
        id: MeterTapId,
        node: GraphNodeId,
    ) -> Result<(), GraphCompileError> {
        self.require_node(node)?;
        if self.meter_taps.iter().any(|tap| tap.id == id) {
            return Err(GraphCompileError::DuplicateMeter(id));
        }
        self.meter_taps.push(MeterTap { id, node });
        Ok(())
    }

    pub fn refuse_processor(
        &mut self,
        label: impl Into<Arc<str>>,
        reason: ProcessorRefusalReason,
        required: bool,
    ) {
        if required {
            self.required_refusals = self.required_refusals.saturating_add(1);
        }
        self.diagnostics.push(GraphDiagnostic::ProcessorRefused {
            label: label.into(),
            reason,
            required,
        });
    }

    pub fn finish(mut self) -> Result<CompiledGraph, GraphCompileError> {
        let output = self.output.ok_or(GraphCompileError::MissingOutput)?;
        let channels = self.plan.format().channels.get();
        if usize::from(channels) > MAX_METER_CHANNELS {
            return Err(GraphCompileError::TooManyMeterChannels {
                channels,
                maximum: MAX_METER_CHANNELS as u16,
            });
        }
        let native_tileability = native_tileability(self.timings[output.0 as usize]);
        if !tileability_covers(self.plan.tileability, native_tileability) {
            return Err(GraphCompileError::TileabilityUnderdeclared {
                plan: self.plan.tileability,
                native: native_tileability,
            });
        }
        if self.plan.tileability != native_tileability {
            self.diagnostics
                .push(GraphDiagnostic::PlanTileabilityMoreConservative {
                    plan: self.plan.tileability,
                    native: native_tileability,
                });
        }
        Ok(CompiledGraph {
            plan: self.plan,
            source_schedule: self.source_schedule,
            nodes: self.nodes.into(),
            timings: self.timings.into(),
            output,
            meter_taps: self.meter_taps.into(),
            diagnostics: self.diagnostics.into(),
            required_refusals: self.required_refusals,
            media_provider: self.media_provider,
        })
    }

    fn push_node(&mut self, node: NativeNode) -> Result<GraphNodeId, GraphCompileError> {
        let raw = u32::try_from(self.nodes.len()).map_err(|_| GraphCompileError::GraphTooLarge)?;
        let timing = node.timing(&self.timings);
        self.nodes.push(node);
        self.timings.push(timing);
        Ok(GraphNodeId(raw))
    }

    fn require_node(&self, node: GraphNodeId) -> Result<(), GraphCompileError> {
        if usize::try_from(node.0)
            .ok()
            .is_some_and(|index| index < self.nodes.len())
        {
            Ok(())
        } else {
            Err(GraphCompileError::UnknownNode(node))
        }
    }
}

/// Immutable graph metadata and native execution order.
#[derive(Clone, Debug)]
pub struct CompiledGraph {
    plan: Arc<RenderPlan>,
    // Retaining the schedule is intentional: the executable lowering cannot
    // outlive or become detached from its frozen truth.
    source_schedule: ScheduleAnchor,
    nodes: Arc<[NativeNode]>,
    timings: Arc<[NodeTiming]>,
    output: GraphNodeId,
    meter_taps: Arc<[MeterTap]>,
    diagnostics: Arc<[GraphDiagnostic]>,
    required_refusals: usize,
    media_provider: Arc<dyn MediaBlockProvider>,
}

impl CompiledGraph {
    pub fn plan(&self) -> &RenderPlan {
        &self.plan
    }

    pub fn source_schedule(&self) -> &DawEngineSchedule {
        match &self.source_schedule {
            ScheduleAnchor::Daw(schedule) => schedule,
            #[cfg(test)]
            ScheduleAnchor::KernelFixture => {
                panic!("kernel fixture has no aggregate DAW schedule")
            }
        }
    }

    pub fn diagnostics(&self) -> &[GraphDiagnostic] {
        &self.diagnostics
    }

    pub fn output_timing(&self) -> NodeTiming {
        self.timings[self.output.0 as usize]
    }

    /// The partition contract this graph actually needs, independent of what
    /// its plan declared. A planner compiles a probe under `SequentialOnly`
    /// and then plans under `declared.covering(native_tileability())`.
    pub fn native_tileability(&self) -> Tileability {
        native_tileability(self.output_timing())
    }

    pub fn realtime_contract(&self) -> RealtimeContract {
        RealtimeContract {
            maximum_block_frames: self.plan.id.engine.canonical_block_frames.get(),
            allocation_free_process: true,
            lock_free_process: true,
            io_free_process: true,
            logging_free_process: true,
            graph_mutation_free_process: true,
        }
    }

    /// Exact source blocks needed before a realtime or offline window can run.
    /// This is a control-thread query: callers hydrate it and publish the
    /// resulting immutable provider before entering callback processing.
    pub fn media_demands(
        &self,
        span: RenderSpan,
    ) -> Result<Vec<MediaBlockDemand>, GraphExecutionError> {
        if !self.plan.extent().contains_span(span) {
            return Err(GraphExecutionError::SpanOutsidePlan {
                requested: span,
                plan: self.plan.extent(),
            });
        }
        Ok(daw_render::media_demands_for_window(
            self.source_schedule().render_schedule(),
            RenderWindow {
                start: span.start,
                end: span.end,
            },
        ))
    }

    pub fn tile_contract(
        &self,
        maximum_preroll: u64,
        maximum_lookahead: u64,
    ) -> Result<TileExecutionContract, TileRefusal> {
        if self.required_refusals > 0 {
            return Err(TileRefusal::RequiredProcessorRefused);
        }
        let timing = self.output_timing();
        match self.plan.tileability {
            Tileability::Stateless => Ok(TileExecutionContract {
                preroll_frames: 0,
                postroll_frames: timing.tail_frames,
                checkpoint_required: false,
            }),
            Tileability::BoundedHistory {
                lookbehind_frames,
                lookahead_frames,
            } => {
                if lookbehind_frames > maximum_preroll {
                    return Err(TileRefusal::PrerollCeilingExceeded {
                        required: lookbehind_frames,
                        ceiling: maximum_preroll,
                    });
                }
                if lookahead_frames > maximum_lookahead {
                    return Err(TileRefusal::LookaheadCeilingExceeded {
                        required: lookahead_frames,
                        ceiling: maximum_lookahead,
                    });
                }
                Ok(TileExecutionContract {
                    preroll_frames: lookbehind_frames,
                    postroll_frames: timing.tail_frames.max(lookahead_frames),
                    checkpoint_required: false,
                })
            }
            Tileability::Checkpointable => Err(TileRefusal::CheckpointImplementationPending),
            Tileability::SequentialOnly => Err(TileRefusal::SequentialOnly),
        }
    }
}

/// One aggregate lowering with semantic taps into the same immutable node
/// array. A multi-scope bounce executes the graph once and copies the named
/// node buffers, so stems and master cannot drift through separate engines.
#[derive(Clone, Debug)]
pub struct NativeDawGraph {
    graph: Arc<CompiledGraph>,
    outputs: BTreeMap<RenderScope, GraphNodeId>,
    render_diagnostics: Arc<[daw_render::RenderDiagnostic]>,
}

impl NativeDawGraph {
    pub fn graph(&self) -> &Arc<CompiledGraph> {
        &self.graph
    }

    pub fn output_node(&self, scope: &RenderScope) -> Option<GraphNodeId> {
        self.outputs.get(scope).copied()
    }

    pub fn render_diagnostics(&self) -> &[daw_render::RenderDiagnostic] {
        &self.render_diagnostics
    }

    pub fn render_scopes(
        &self,
        span: RenderSpan,
        scopes: &[RenderScope],
        cancellation: &RenderCancellation,
    ) -> Result<OfflineGraphOutputs, GraphExecutionError> {
        let mut selected = BTreeMap::new();
        for scope in scopes {
            let Some(node) = self.outputs.get(scope).copied() else {
                continue;
            };
            selected.entry(scope.clone()).or_insert(node);
        }
        OfflineGraphExecutor::new(Arc::clone(&self.graph))?.render_outputs(
            span,
            &selected,
            cancellation,
        )
    }

    /// Capture semantic scopes while synchronously hydrating only each
    /// execution window. This is the streaming equivalent of `render_scopes`;
    /// both traverse the same compiled nodes and differ only at media supply.
    pub fn render_scopes_with_media_source(
        &self,
        span: RenderSpan,
        scopes: &[RenderScope],
        cancellation: &RenderCancellation,
        media: &mut dyn MediaBlockSource,
    ) -> Result<OfflineGraphOutputs, GraphExecutionError> {
        let mut selected = BTreeMap::new();
        for scope in scopes {
            let Some(node) = self.outputs.get(scope).copied() else {
                continue;
            };
            selected.entry(scope.clone()).or_insert(node);
        }
        OfflineGraphExecutor::new(Arc::clone(&self.graph))?.render_outputs_with_media_source(
            span,
            &selected,
            cancellation,
            media,
        )
    }
}

/// Lower the already-frozen aggregate schedule into native source, automation,
/// mixer and routing nodes. The schedule remains the sole scheduling truth;
/// this compiler only chooses executable nodes and their semantic taps.
pub fn compile_native_daw_graph(
    plan: Arc<RenderPlan>,
    schedule: Arc<DawEngineSchedule>,
) -> Result<NativeDawGraph, GraphCompileError> {
    let provider: Arc<dyn MediaBlockProvider> = Arc::new(ResidentPcmProvider::new(Arc::new(
        schedule.assets().clone(),
    )));
    compile_native_daw_graph_with_media(plan, schedule, provider)
}

/// Lower a schedule against an immutable prepared-media snapshot. The
/// schedule may carry no resident `PcmAsset`s; source validity comes from the
/// provider descriptor catalog, while callback reads remain bounded to the
/// snapshot's prefetched blocks.
pub fn compile_native_daw_graph_with_media(
    plan: Arc<RenderPlan>,
    schedule: Arc<DawEngineSchedule>,
    media_provider: Arc<dyn MediaBlockProvider>,
) -> Result<NativeDawGraph, GraphCompileError> {
    let mut builder = CompiledGraphBuilder::new_with_media(
        plan,
        Arc::clone(&schedule),
        Arc::clone(&media_provider),
    )?;
    let render = schedule.render_schedule();
    let automation = Arc::new(render.automation().clone());
    let mut bus_inputs = render
        .buses()
        .iter()
        .map(|bus| (bus.id, Vec::<GraphNodeId>::new()))
        .collect::<BTreeMap<_, _>>();
    let mut track_inputs = render
        .tracks()
        .iter()
        .copied()
        .map(|track| (track, Vec::<GraphNodeId>::new()))
        .collect::<BTreeMap<_, _>>();
    let mut render_diagnostics = render.diagnostics().to_vec();

    for clip in render.audio_clips().iter().filter(|clip| clip.renderable) {
        let Some(descriptor) = media_provider.descriptor(clip.asset) else {
            render_diagnostics.push(daw_render::RenderDiagnostic::MissingAsset {
                clip: clip.id,
                asset: clip.asset,
            });
            continue;
        };
        let invalid = if descriptor.format.sample_rate != render.format().sample_rate {
            Some("asset and project sample rates differ")
        } else if clip.source_end > descriptor.frame_count
            || !daw_render::valid_channel_map(&clip.channels, descriptor.format)
        {
            Some("source range or channel map exceeds the asset")
        } else {
            None
        };
        if let Some(reason) = invalid {
            render_diagnostics.push(daw_render::RenderDiagnostic::InvalidAssetFormat {
                clip: clip.id,
                asset: clip.asset,
                reason,
            });
            continue;
        }
        let node = builder.add_audio_clip(clip.clone(), descriptor, Arc::clone(&automation))?;
        bus_inputs.entry(clip.bus).or_default().push(node);
        track_inputs.entry(clip.track).or_default().push(node);
    }

    let choke_groups = crate::sampler_runtime::route_choke_groups(schedule.instruments());
    let mut instrument_inputs = BTreeMap::<BusId, Vec<GraphNodeId>>::new();
    for (&identity, route) in schedule.instruments() {
        let mut events = Vec::new();
        for block in render.blocks() {
            for source in block.sequencer_events.iter() {
                let mut event = source.clone();
                if let ScheduledKind::Trigger {
                    target: TriggerTarget::Sample(alias),
                    choke_group,
                    ..
                } = &mut event.kind
                {
                    if choke_group.is_none() {
                        *choke_group = choke_groups.get(&alias.get()).copied().flatten();
                    }
                }
                if route.definition.observes(identity, &event) {
                    events.push(event);
                }
            }
        }
        let node = builder.add_instrument(identity, route.definition.clone(), events.into())?;
        instrument_inputs.entry(route.bus).or_default().push(node);
    }
    // The reference oracle accumulates already-routed instruments before
    // arrangement clips. Retain that exact floating-point summation order.
    for (bus, inputs) in &mut bus_inputs {
        let Some(mut instruments) = instrument_inputs.remove(bus) else {
            continue;
        };
        instruments.append(inputs);
        *inputs = instruments;
    }

    let silence = builder.add_silence()?;
    let mut scope_outputs = BTreeMap::new();
    for (&track, inputs) in &track_inputs {
        let node = mix_or_silence(&mut builder, inputs, silence)?;
        let node = builder.add_sanitize(node)?;
        scope_outputs.insert(RenderScope::Track(track.get()), node);
    }

    for bus in render.buses() {
        let inputs = bus_inputs.remove(&bus.id).unwrap_or_default();
        let pre = mix_or_silence(&mut builder, &inputs, silence)?;
        let captured_pre = builder.add_sanitize(pre)?;
        scope_outputs.insert(
            RenderScope::Bus {
                bus: bus.id.get(),
                tap: crate::render_plan::BusTap::PreFader,
            },
            captured_pre,
        );

        for route in bus
            .routes
            .iter()
            .filter(|route| route.tap == SendTap::PreFader)
        {
            let sent = builder.add_send(
                pre,
                route.clone(),
                Some(bus.clone()),
                Arc::clone(&automation),
            )?;
            note_bypassed_compensation(&mut builder, bus.id, route);
            bus_inputs.entry(route.to).or_default().push(sent);
        }

        let post = builder.add_bus_fader(pre, bus.clone(), Arc::clone(&automation))?;
        let captured_post = builder.add_sanitize(post)?;
        scope_outputs.insert(
            RenderScope::Bus {
                bus: bus.id.get(),
                tap: crate::render_plan::BusTap::PostFader,
            },
            captured_post,
        );
        scope_outputs.insert(
            RenderScope::Bus {
                bus: bus.id.get(),
                tap: crate::render_plan::BusTap::Output,
            },
            captured_post,
        );
        for route in bus
            .routes
            .iter()
            .filter(|route| route.tap == SendTap::PostFader)
        {
            let sent = if route.kind == RouteKind::Main {
                post
            } else {
                builder.add_send(post, route.clone(), None, Arc::clone(&automation))?
            };
            note_bypassed_compensation(&mut builder, bus.id, route);
            bus_inputs.entry(route.to).or_default().push(sent);
        }
    }

    let master = scope_outputs
        .get(&RenderScope::Bus {
            bus: render.master().get(),
            tap: crate::render_plan::BusTap::PostFader,
        })
        .copied()
        .unwrap_or(silence);
    scope_outputs.insert(RenderScope::Master, master);
    builder.set_output(master)?;
    let graph = Arc::new(builder.finish()?);
    render_diagnostics.retain(|diagnostic| match diagnostic {
        daw_render::RenderDiagnostic::SequencerEventsNeedInstrument { .. }
        | daw_render::RenderDiagnostic::ArrangementPatternNeedsInstrument { .. } => false,
        daw_render::RenderDiagnostic::MissingAsset { asset, .. } => {
            media_provider.descriptor(*asset).is_none()
        }
        _ => true,
    });
    Ok(NativeDawGraph {
        graph,
        outputs: scope_outputs,
        render_diagnostics: render_diagnostics.into(),
    })
}

fn mix_or_silence(
    builder: &mut CompiledGraphBuilder,
    inputs: &[GraphNodeId],
    silence: GraphNodeId,
) -> Result<GraphNodeId, GraphCompileError> {
    match inputs {
        [] => Ok(silence),
        [only] => Ok(*only),
        _ => builder.add_mix(inputs.iter().copied().map(|node| (node, 1.0))),
    }
}

fn note_bypassed_compensation(
    builder: &mut CompiledGraphBuilder,
    bus: BusId,
    route: &CompiledRoute,
) {
    if route.compensation_delay_frames > 0 {
        builder
            .diagnostics
            .push(GraphDiagnostic::CompensationBypassed {
                bus: bus.get(),
                route_to: route.to.get(),
                frames: route.compensation_delay_frames,
            });
    }
}

/// Background/offline contract. It may allocate the destination product, but
/// all DSP is delegated to the exact kernel used by realtime playback.
pub struct OfflineGraphExecutor {
    kernel: ExecutionKernel,
}

impl OfflineGraphExecutor {
    pub fn new(graph: Arc<CompiledGraph>) -> Result<Self, GraphExecutionError> {
        Ok(Self {
            kernel: ExecutionKernel::new(graph, MediaExecutionPolicy::RefuseUnavailable)?,
        })
    }

    /// Offline render with synchronous block hydration through the same
    /// immutable provider snapshots consumed by realtime execution.
    pub fn render_with_media_source(
        &mut self,
        span: RenderSpan,
        cancellation: &RenderCancellation,
        media: &mut dyn MediaBlockSource,
    ) -> Result<OfflineGraphRender, GraphExecutionError> {
        self.validate_span(span)?;
        self.seek_with_media_source(span.start, media)?;
        self.kernel.reset_meters();
        let channels = self.kernel.channels;
        let sample_count = usize::try_from(span.len())
            .ok()
            .and_then(|frames| frames.checked_mul(channels))
            .ok_or(GraphExecutionError::RenderTooLarge)?;
        let mut interleaved = vec![0.0_f32; sample_count];
        let mut frame = span.start;
        let mut target_offset = 0;
        while frame < span.end {
            if cancellation.is_cancelled() {
                return Err(GraphExecutionError::Cancelled);
            }
            let frames =
                usize::try_from((span.end - frame).min(self.kernel.maximum_block_frames as i64))
                    .unwrap();
            self.prepare_window(frame, frames, media)?;
            let samples = frames * channels;
            self.kernel.process(
                frame,
                &mut interleaved[target_offset..target_offset + samples],
                true,
            )?;
            frame += frames as i64;
            target_offset += samples;
        }
        Ok(OfflineGraphRender {
            plan: Arc::clone(&self.kernel.graph.plan),
            span,
            format: self.kernel.graph.plan.format(),
            interleaved: interleaved.into(),
            meters: self.kernel.meter_readings().to_vec().into(),
        })
    }

    fn validate_span(&self, span: RenderSpan) -> Result<(), GraphExecutionError> {
        if self.kernel.graph.plan.extent().contains_span(span) {
            Ok(())
        } else {
            Err(GraphExecutionError::SpanOutsidePlan {
                requested: span,
                plan: self.kernel.graph.plan.extent(),
            })
        }
    }

    fn prepare_window(
        &mut self,
        frame: i64,
        frames: usize,
        media: &mut dyn MediaBlockSource,
    ) -> Result<(), GraphExecutionError> {
        let window = RenderWindow {
            start: frame,
            end: frame.saturating_add(frames as i64),
        };
        let demands = daw_render::media_demands_for_window(
            self.kernel.graph.source_schedule().render_schedule(),
            window,
        );
        let provider = media.prepare(&demands)?;
        self.kernel.replace_media_provider(provider)
    }

    fn seek_with_media_source(
        &mut self,
        frame: i64,
        media: &mut dyn MediaBlockSource,
    ) -> Result<(), GraphExecutionError> {
        let extent = self.kernel.graph.plan.extent();
        if frame < extent.start || frame > extent.end {
            return Err(GraphExecutionError::SeekOutsidePlan {
                frame,
                plan: extent,
            });
        }
        let lookbehind = self.kernel.graph.output_timing().lookbehind_frames;
        let warm_start = frame.saturating_sub(lookbehind as i64).max(extent.start);
        self.kernel.reset_states(warm_start);
        self.kernel.position = warm_start;
        while self.kernel.position < frame {
            let frames = usize::try_from(
                (frame - self.kernel.position).min(self.kernel.maximum_block_frames as i64),
            )
            .unwrap();
            self.prepare_window(self.kernel.position, frames, media)?;
            self.kernel
                .process_internal(self.kernel.position, frames, false)?;
            self.kernel.position += frames as i64;
        }
        Ok(())
    }

    pub fn render(
        &mut self,
        span: RenderSpan,
        cancellation: &RenderCancellation,
    ) -> Result<OfflineGraphRender, GraphExecutionError> {
        if !self.kernel.graph.plan.extent().contains_span(span) {
            return Err(GraphExecutionError::SpanOutsidePlan {
                requested: span,
                plan: self.kernel.graph.plan.extent(),
            });
        }
        self.kernel.seek(span.start)?;
        self.kernel.reset_meters();
        let channels = self.kernel.channels;
        let sample_count = usize::try_from(span.len())
            .ok()
            .and_then(|frames| frames.checked_mul(channels))
            .ok_or(GraphExecutionError::RenderTooLarge)?;
        let mut interleaved = vec![0.0_f32; sample_count];
        let maximum = self.kernel.maximum_block_frames;
        let mut frame = span.start;
        let mut target_offset = 0;
        while frame < span.end {
            if cancellation.is_cancelled() {
                return Err(GraphExecutionError::Cancelled);
            }
            let frames = usize::try_from((span.end - frame).min(maximum as i64)).unwrap();
            let samples = frames * channels;
            self.kernel.process(
                frame,
                &mut interleaved[target_offset..target_offset + samples],
                true,
            )?;
            frame += frames as i64;
            target_offset += samples;
        }
        Ok(OfflineGraphRender {
            plan: Arc::clone(&self.kernel.graph.plan),
            span,
            format: self.kernel.graph.plan.format(),
            interleaved: interleaved.into(),
            meters: self.kernel.meter_readings().to_vec().into(),
        })
    }

    /// Capture several semantic nodes from one traversal. Allocation is
    /// confined to this background/offline boundary; the kernel and its node
    /// arena are exactly the ones used by [`RealtimeGraphExecutor`].
    pub fn render_outputs(
        &mut self,
        span: RenderSpan,
        outputs: &BTreeMap<RenderScope, GraphNodeId>,
        cancellation: &RenderCancellation,
    ) -> Result<OfflineGraphOutputs, GraphExecutionError> {
        if !self.kernel.graph.plan.extent().contains_span(span) {
            return Err(GraphExecutionError::SpanOutsidePlan {
                requested: span,
                plan: self.kernel.graph.plan.extent(),
            });
        }
        for node in outputs.values() {
            if node.0 as usize >= self.kernel.graph.nodes.len() {
                return Err(GraphExecutionError::UnknownOutputNode(*node));
            }
        }
        self.kernel.seek(span.start)?;
        self.kernel.reset_meters();
        let channels = self.kernel.channels;
        let sample_count = usize::try_from(span.len())
            .ok()
            .and_then(|frames| frames.checked_mul(channels))
            .ok_or(GraphExecutionError::RenderTooLarge)?;
        let mut rendered = outputs
            .keys()
            .cloned()
            .map(|scope| (scope, vec![0.0_f32; sample_count]))
            .collect::<BTreeMap<_, _>>();
        let mut frame = span.start;
        let mut target_offset = 0;
        while frame < span.end {
            if cancellation.is_cancelled() {
                return Err(GraphExecutionError::Cancelled);
            }
            let frames =
                usize::try_from((span.end - frame).min(self.kernel.maximum_block_frames as i64))
                    .unwrap();
            self.kernel.process_internal(frame, frames, true)?;
            let samples = frames * channels;
            for (scope, node) in outputs {
                let source = self.kernel.node_buffer(node.0 as usize, frames);
                rendered.get_mut(scope).expect("selected scope exists")
                    [target_offset..target_offset + samples]
                    .copy_from_slice(source);
            }
            self.kernel.position += frames as i64;
            frame += frames as i64;
            target_offset += samples;
        }
        Ok(OfflineGraphOutputs {
            plan: Arc::clone(&self.kernel.graph.plan),
            span,
            format: self.kernel.graph.plan.format(),
            outputs: rendered
                .into_iter()
                .map(|(scope, pcm)| (scope, Arc::<[f32]>::from(pcm)))
                .collect(),
            meters: self.kernel.meter_readings().to_vec().into(),
        })
    }

    /// Multi-scope offline render with synchronous media hydration. Provider
    /// publication happens before each kernel block; copying semantic taps
    /// remains part of the same traversal as the resident-media oracle.
    pub fn render_outputs_with_media_source(
        &mut self,
        span: RenderSpan,
        outputs: &BTreeMap<RenderScope, GraphNodeId>,
        cancellation: &RenderCancellation,
        media: &mut dyn MediaBlockSource,
    ) -> Result<OfflineGraphOutputs, GraphExecutionError> {
        self.validate_span(span)?;
        for node in outputs.values() {
            if node.0 as usize >= self.kernel.graph.nodes.len() {
                return Err(GraphExecutionError::UnknownOutputNode(*node));
            }
        }
        self.seek_with_media_source(span.start, media)?;
        self.kernel.reset_meters();
        let channels = self.kernel.channels;
        let sample_count = usize::try_from(span.len())
            .ok()
            .and_then(|frames| frames.checked_mul(channels))
            .ok_or(GraphExecutionError::RenderTooLarge)?;
        let mut rendered = outputs
            .keys()
            .cloned()
            .map(|scope| (scope, vec![0.0_f32; sample_count]))
            .collect::<BTreeMap<_, _>>();
        let mut frame = span.start;
        let mut target_offset = 0;
        while frame < span.end {
            if cancellation.is_cancelled() {
                return Err(GraphExecutionError::Cancelled);
            }
            let frames =
                usize::try_from((span.end - frame).min(self.kernel.maximum_block_frames as i64))
                    .unwrap();
            self.prepare_window(frame, frames, media)?;
            self.kernel.process_internal(frame, frames, true)?;
            let samples = frames * channels;
            for (scope, node) in outputs {
                let source = self.kernel.node_buffer(node.0 as usize, frames);
                rendered.get_mut(scope).expect("selected scope exists")
                    [target_offset..target_offset + samples]
                    .copy_from_slice(source);
            }
            self.kernel.position += frames as i64;
            frame += frames as i64;
            target_offset += samples;
        }
        Ok(OfflineGraphOutputs {
            plan: Arc::clone(&self.kernel.graph.plan),
            span,
            format: self.kernel.graph.plan.format(),
            outputs: rendered
                .into_iter()
                .map(|(scope, pcm)| (scope, Arc::<[f32]>::from(pcm)))
                .collect(),
            meters: self.kernel.meter_readings().to_vec().into(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct OfflineGraphRender {
    pub plan: Arc<RenderPlan>,
    pub span: RenderSpan,
    pub format: RenderFormat,
    pub interleaved: Arc<[f32]>,
    pub meters: Arc<[MeterReading]>,
}

#[derive(Clone, Debug)]
pub struct OfflineGraphOutputs {
    pub plan: Arc<RenderPlan>,
    pub span: RenderSpan,
    pub format: RenderFormat,
    pub outputs: BTreeMap<RenderScope, Arc<[f32]>>,
    pub meters: Arc<[MeterReading]>,
}

/// Realtime/device contract. Construction and seeking are control-thread
/// operations. Processing is continuous, bounded, and invokes no user code.
pub struct RealtimeGraphExecutor {
    kernel: ExecutionKernel,
}

impl RealtimeGraphExecutor {
    pub fn new(graph: Arc<CompiledGraph>) -> Result<Self, GraphExecutionError> {
        Ok(Self {
            kernel: ExecutionKernel::new(graph, MediaExecutionPolicy::ReportUnderrun)?,
        })
    }

    pub fn contract(&self) -> RealtimeContract {
        self.kernel.graph.realtime_contract()
    }

    pub fn position(&self) -> i64 {
        self.kernel.position
    }

    /// Control-boundary seek. Bounded state is reconstructed from declared
    /// lookbehind using the same kernel and preallocated arena.
    pub fn seek(&mut self, frame: i64) -> Result<(), GraphExecutionError> {
        self.kernel.seek(frame)
    }

    pub fn reset_meters(&mut self) {
        self.kernel.reset_meters();
    }

    pub fn meter_readings(&self) -> &[MeterReading] {
        self.kernel.meter_readings()
    }

    /// Control-boundary snapshot replacement. The provider is shape-checked
    /// before publication; callback processing only reads the resulting Arc.
    pub fn replace_media_provider(
        &mut self,
        provider: Arc<dyn MediaBlockProvider>,
    ) -> Result<(), GraphExecutionError> {
        self.kernel.replace_media_provider(provider)
    }

    pub const fn media_status(&self) -> MediaRuntimeStatus {
        self.kernel.media_status
    }

    pub fn process_interleaved(
        &mut self,
        output: &mut [f32],
    ) -> Result<usize, GraphExecutionError> {
        let channels = self.kernel.channels;
        if output.len() % channels != 0 {
            return Err(GraphExecutionError::PartialFrame {
                samples: output.len(),
                channels: channels as u16,
            });
        }
        let frames = output.len() / channels;
        if frames > self.kernel.maximum_block_frames {
            return Err(GraphExecutionError::BlockTooLarge {
                requested: frames,
                maximum: self.kernel.maximum_block_frames,
            });
        }
        let available = self
            .kernel
            .graph
            .plan
            .extent()
            .end
            .saturating_sub(self.kernel.position)
            .max(0) as usize;
        let rendered = frames.min(available);
        let rendered_samples = rendered * channels;
        let position = self.kernel.position;
        self.kernel
            .process(position, &mut output[..rendered_samples], true)?;
        output[rendered_samples..].fill(0.0);
        Ok(rendered)
    }

    #[cfg(test)]
    fn storage_fingerprint(&self) -> StorageFingerprint {
        self.kernel.storage_fingerprint()
    }
}

enum RuntimeNodeState {
    Stateless,
    Gain {
        current: f32,
        next_event: usize,
    },
    Delay {
        ring: Vec<f32>,
        cursor_frame: usize,
    },
    Instrument {
        instrument: BuiltInInstrument,
        events: Vec<ScheduledEvent>,
        stereo: Vec<f32>,
    },
}

#[derive(Clone, Copy, Debug)]
struct MeterAccumulator {
    snapshot: MeterSnapshot,
    square_sum: [f64; MAX_METER_CHANNELS],
}

impl MeterAccumulator {
    fn new(channels: u16) -> Self {
        Self {
            snapshot: MeterSnapshot {
                channels,
                ..MeterSnapshot::default()
            },
            square_sum: [0.0; MAX_METER_CHANNELS],
        }
    }

    fn reset(&mut self) {
        *self = Self::new(self.snapshot.channels);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MediaExecutionPolicy {
    RefuseUnavailable,
    ReportUnderrun,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MediaRuntimeStatus {
    pub underrun_events: u64,
    pub unavailable_source_frames: u64,
    pub currently_underrun: bool,
    pub last_error: Option<MediaReadError>,
}

struct ExecutionKernel {
    graph: Arc<CompiledGraph>,
    channels: usize,
    maximum_block_frames: usize,
    position: i64,
    arena: Vec<f32>,
    states: Vec<RuntimeNodeState>,
    meter_accumulators: Vec<MeterAccumulator>,
    meter_readings: Vec<MeterReading>,
    media_provider: Arc<dyn MediaBlockProvider>,
    media_policy: MediaExecutionPolicy,
    media_status: MediaRuntimeStatus,
}

impl ExecutionKernel {
    fn new(
        graph: Arc<CompiledGraph>,
        media_policy: MediaExecutionPolicy,
    ) -> Result<Self, GraphExecutionError> {
        if graph.required_refusals > 0 {
            return Err(GraphExecutionError::RequiredProcessorRefused {
                count: graph.required_refusals,
            });
        }
        let channels = usize::from(graph.plan.format().channels.get());
        let maximum_block_frames = graph.plan.id.engine.canonical_block_frames.get() as usize;
        let arena_samples = graph
            .nodes
            .len()
            .checked_mul(maximum_block_frames)
            .and_then(|value| value.checked_mul(channels))
            .ok_or(GraphExecutionError::RenderTooLarge)?;
        let mut states = Vec::with_capacity(graph.nodes.len());
        for node in graph.nodes.iter() {
            states.push(match node {
                NativeNode::Silence
                | NativeNode::FrozenPcm(_)
                | NativeNode::Mix { .. }
                | NativeNode::AudioClip { .. }
                | NativeNode::BusFader { .. }
                | NativeNode::Send { .. }
                | NativeNode::Sanitize { .. } => RuntimeNodeState::Stateless,
                NativeNode::Gain { initial_linear, .. } => RuntimeNodeState::Gain {
                    current: *initial_linear,
                    next_event: 0,
                },
                NativeNode::Delay { frames, .. } => RuntimeNodeState::Delay {
                    ring: vec![0.0; (*frames as usize).saturating_mul(channels)],
                    cursor_frame: 0,
                },
                NativeNode::Instrument {
                    identity,
                    definition,
                    events,
                } => RuntimeNodeState::Instrument {
                    instrument: definition
                        .instantiate(graph.plan.format().sample_rate.get(), *identity)
                        .map_err(|error| GraphExecutionError::Instrument(error.to_string()))?,
                    events: Vec::with_capacity(events.len()),
                    stereo: vec![0.0; maximum_block_frames.saturating_mul(2)],
                },
            });
        }
        let meter_accumulators = graph
            .meter_taps
            .iter()
            .map(|_| MeterAccumulator::new(channels as u16))
            .collect();
        let meter_readings = graph
            .meter_taps
            .iter()
            .map(|tap| MeterReading {
                id: tap.id,
                snapshot: MeterSnapshot {
                    channels: channels as u16,
                    ..MeterSnapshot::default()
                },
            })
            .collect();
        let position = graph.plan.extent().start;
        let media_provider = Arc::clone(&graph.media_provider);
        Ok(Self {
            graph,
            channels,
            maximum_block_frames,
            position,
            arena: vec![0.0; arena_samples],
            states,
            meter_accumulators,
            meter_readings,
            media_provider,
            media_policy,
            media_status: MediaRuntimeStatus::default(),
        })
    }

    fn replace_media_provider(
        &mut self,
        provider: Arc<dyn MediaBlockProvider>,
    ) -> Result<(), GraphExecutionError> {
        for node in self.graph.nodes.iter() {
            let NativeNode::AudioClip {
                clip, descriptor, ..
            } = node
            else {
                continue;
            };
            let actual = provider
                .descriptor(clip.asset)
                .ok_or(GraphExecutionError::MediaProviderMissingAsset(clip.asset))?;
            if actual != *descriptor {
                return Err(GraphExecutionError::MediaProviderShapeMismatch {
                    asset: clip.asset,
                    expected: *descriptor,
                    actual,
                });
            }
        }
        self.media_provider = provider;
        Ok(())
    }

    fn seek(&mut self, frame: i64) -> Result<(), GraphExecutionError> {
        let extent = self.graph.plan.extent();
        if frame < extent.start || frame > extent.end {
            return Err(GraphExecutionError::SeekOutsidePlan {
                frame,
                plan: extent,
            });
        }
        let lookbehind = self.graph.output_timing().lookbehind_frames;
        let warm_start = frame.saturating_sub(lookbehind as i64).max(extent.start);
        self.reset_states(warm_start);
        self.position = warm_start;
        while self.position < frame {
            let frames =
                usize::try_from((frame - self.position).min(self.maximum_block_frames as i64))
                    .unwrap();
            self.process_internal(self.position, frames, false)?;
            self.position += frames as i64;
        }
        Ok(())
    }

    fn reset_states(&mut self, at: i64) {
        self.media_status.currently_underrun = false;
        for (node, state) in self.graph.nodes.iter().zip(&mut self.states) {
            match (node, state) {
                (
                    NativeNode::Gain {
                        initial_linear,
                        events,
                        ..
                    },
                    RuntimeNodeState::Gain {
                        current,
                        next_event,
                    },
                ) => {
                    *current = *initial_linear;
                    *next_event = events.partition_point(|event| event.project_frame < at);
                    for event in &events[..*next_event] {
                        *current = event.linear;
                    }
                }
                (NativeNode::Delay { .. }, RuntimeNodeState::Delay { ring, cursor_frame }) => {
                    ring.fill(0.0);
                    *cursor_frame = 0;
                }
                (
                    NativeNode::Instrument {
                        identity,
                        definition,
                        ..
                    },
                    RuntimeNodeState::Instrument {
                        instrument,
                        events,
                        stereo,
                    },
                ) => {
                    *instrument = definition
                        .instantiate(self.graph.plan.format().sample_rate.get(), *identity)
                        .expect("instrument was validated when the schedule was compiled");
                    events.clear();
                    stereo.fill(0.0);
                }
                _ => {}
            }
        }
    }

    fn reset_meters(&mut self) {
        for accumulator in &mut self.meter_accumulators {
            accumulator.reset();
        }
        self.refresh_meter_readings();
    }

    fn process(
        &mut self,
        absolute_frame: i64,
        output: &mut [f32],
        measure: bool,
    ) -> Result<(), GraphExecutionError> {
        if absolute_frame != self.position {
            return Err(GraphExecutionError::DiscontinuousProcess {
                expected: self.position,
                actual: absolute_frame,
            });
        }
        if output.len() % self.channels != 0 {
            return Err(GraphExecutionError::PartialFrame {
                samples: output.len(),
                channels: self.channels as u16,
            });
        }
        let frames = output.len() / self.channels;
        if frames > self.maximum_block_frames {
            return Err(GraphExecutionError::BlockTooLarge {
                requested: frames,
                maximum: self.maximum_block_frames,
            });
        }
        self.process_internal(absolute_frame, frames, measure)?;
        let output_node = self.graph.output.0 as usize;
        let source = self.node_buffer(output_node, frames);
        output.copy_from_slice(source);
        self.position += frames as i64;
        Ok(())
    }

    fn process_internal(
        &mut self,
        absolute_frame: i64,
        frames: usize,
        measure: bool,
    ) -> Result<(), GraphExecutionError> {
        let samples = frames * self.channels;
        let active_samples = self.graph.nodes.len() * samples;
        self.arena[..active_samples].fill(0.0);
        let mut unavailable_source_frames = 0_u64;
        let mut first_media_error = None;

        for node_index in 0..self.graph.nodes.len() {
            match &self.graph.nodes[node_index] {
                NativeNode::Silence => {}
                NativeNode::FrozenPcm(product) => {
                    let overlap_start = absolute_frame.max(product.span.start);
                    let overlap_end = (absolute_frame + frames as i64).min(product.span.end);
                    if overlap_start < overlap_end {
                        let source_frame = (overlap_start - product.span.start) as usize;
                        let target_frame = (overlap_start - absolute_frame) as usize;
                        let overlap_frames = (overlap_end - overlap_start) as usize;
                        let source_start = source_frame * self.channels;
                        let target_start = target_frame * self.channels;
                        let count = overlap_frames * self.channels;
                        let target = node_buffer_mut(&mut self.arena, node_index, samples);
                        target[target_start..target_start + count].copy_from_slice(
                            &product.interleaved[source_start..source_start + count],
                        );
                    }
                }
                NativeNode::Gain { input, events, .. } => {
                    let input_index = input.0 as usize;
                    let (before, after) = self.arena.split_at_mut(node_index * samples);
                    let source = &before[input_index * samples..(input_index + 1) * samples];
                    let target = &mut after[..samples];
                    let RuntimeNodeState::Gain {
                        current,
                        next_event,
                    } = &mut self.states[node_index]
                    else {
                        unreachable!("compiled gain has gain state")
                    };
                    for frame_offset in 0..frames {
                        let project_frame = absolute_frame + frame_offset as i64;
                        while *next_event < events.len()
                            && events[*next_event].project_frame == project_frame
                        {
                            *current = events[*next_event].linear;
                            *next_event += 1;
                        }
                        let start = frame_offset * self.channels;
                        for channel in 0..self.channels {
                            target[start + channel] = source[start + channel] * *current;
                        }
                    }
                }
                NativeNode::Mix { inputs } => {
                    let (before, after) = self.arena.split_at_mut(node_index * samples);
                    let target = &mut after[..samples];
                    for input in inputs.iter() {
                        let source_index = input.node.0 as usize;
                        let source = &before[source_index * samples..(source_index + 1) * samples];
                        for (target, source) in target.iter_mut().zip(source) {
                            *target += *source * input.gain;
                        }
                    }
                }
                NativeNode::Delay {
                    input,
                    frames: delay,
                } => {
                    let input_index = input.0 as usize;
                    let (before, after) = self.arena.split_at_mut(node_index * samples);
                    let source = &before[input_index * samples..(input_index + 1) * samples];
                    let target = &mut after[..samples];
                    let RuntimeNodeState::Delay { ring, cursor_frame } =
                        &mut self.states[node_index]
                    else {
                        unreachable!("compiled delay has delay state")
                    };
                    let delay_frames = *delay as usize;
                    for frame_offset in 0..frames {
                        let ring_start = *cursor_frame * self.channels;
                        let block_start = frame_offset * self.channels;
                        for channel in 0..self.channels {
                            target[block_start + channel] = ring[ring_start + channel];
                            ring[ring_start + channel] = source[block_start + channel];
                        }
                        *cursor_frame += 1;
                        if *cursor_frame == delay_frames {
                            *cursor_frame = 0;
                        }
                    }
                }
                NativeNode::AudioClip {
                    clip,
                    descriptor,
                    automation,
                } => {
                    let target = node_buffer_mut(&mut self.arena, node_index, samples);
                    let report = daw_render::render_compiled_clip_from_provider_into(
                        clip,
                        *descriptor,
                        self.media_provider.as_ref(),
                        automation,
                        self.graph.plan.format().channels.get(),
                        RenderWindow {
                            start: absolute_frame,
                            end: absolute_frame + frames as i64,
                        },
                        target,
                    );
                    unavailable_source_frames =
                        unavailable_source_frames.saturating_add(report.unavailable_source_frames);
                    if first_media_error.is_none() {
                        first_media_error = report.first_error;
                    }
                }
                NativeNode::Instrument { events, .. } => {
                    let target = node_buffer_mut(&mut self.arena, node_index, samples);
                    let RuntimeNodeState::Instrument {
                        instrument,
                        events: scratch_events,
                        stereo,
                    } = &mut self.states[node_index]
                    else {
                        unreachable!("compiled instrument has instrument state")
                    };
                    scratch_events.clear();
                    let end = absolute_frame.saturating_add(frames as i64);
                    let first =
                        events.partition_point(|event| event.project_frame.0 < absolute_frame);
                    for source in &events[first..] {
                        if source.project_frame.0 >= end {
                            break;
                        }
                        let mut event = source.clone();
                        event.block_offset = u32::try_from(event.project_frame.0 - absolute_frame)
                            .expect("event lies inside maximum native block");
                        scratch_events.push(event);
                    }
                    let stereo = &mut stereo[..frames * 2];
                    stereo.fill(0.0);
                    instrument
                        .render_scheduled_block(absolute_frame, scratch_events, stereo)
                        .map_err(|error| GraphExecutionError::Instrument(error.to_string()))?;
                    if self.channels == 1 {
                        for frame in 0..frames {
                            target[frame] = (stereo[frame * 2] + stereo[frame * 2 + 1]) * 0.5;
                        }
                    } else {
                        target.copy_from_slice(stereo);
                    }
                }
                NativeNode::BusFader {
                    input,
                    bus,
                    automation,
                } => {
                    let input_index = input.0 as usize;
                    let (before, after) = self.arena.split_at_mut(node_index * samples);
                    let source = &before[input_index * samples..(input_index + 1) * samples];
                    let target = &mut after[..samples];
                    target.copy_from_slice(source);
                    daw_render::apply_compiled_bus_fader(
                        automation,
                        bus,
                        RenderWindow {
                            start: absolute_frame,
                            end: absolute_frame + frames as i64,
                        },
                        self.channels,
                        target,
                    );
                }
                NativeNode::Send {
                    input,
                    route,
                    pre_fader_bus,
                    automation,
                } => {
                    let input_index = input.0 as usize;
                    let (before, after) = self.arena.split_at_mut(node_index * samples);
                    let source = &before[input_index * samples..(input_index + 1) * samples];
                    let target = &mut after[..samples];
                    daw_render::add_compiled_send(
                        automation,
                        route,
                        pre_fader_bus.as_ref(),
                        RenderWindow {
                            start: absolute_frame,
                            end: absolute_frame + frames as i64,
                        },
                        self.channels,
                        target,
                        source,
                    );
                }
                NativeNode::Sanitize { input } => {
                    let input_index = input.0 as usize;
                    let (before, after) = self.arena.split_at_mut(node_index * samples);
                    let source = &before[input_index * samples..(input_index + 1) * samples];
                    let target = &mut after[..samples];
                    for (target, source) in target.iter_mut().zip(source) {
                        *target = if source.is_finite() { *source } else { 0.0 };
                    }
                }
            }
        }

        if unavailable_source_frames > 0 {
            let error = first_media_error.expect("an unavailable frame retains its first error");
            if self.media_policy == MediaExecutionPolicy::RefuseUnavailable {
                return Err(GraphExecutionError::MediaUnavailable(error));
            }
            if !self.media_status.currently_underrun {
                self.media_status.underrun_events =
                    self.media_status.underrun_events.saturating_add(1);
            }
            self.media_status.currently_underrun = true;
            self.media_status.unavailable_source_frames = self
                .media_status
                .unavailable_source_frames
                .saturating_add(unavailable_source_frames);
            self.media_status.last_error = Some(error);
        } else {
            self.media_status.currently_underrun = false;
        }

        if measure {
            self.measure(frames);
        }
        Ok(())
    }

    fn measure(&mut self, frames: usize) {
        let samples = frames * self.channels;
        for (tap_index, tap) in self.graph.meter_taps.iter().enumerate() {
            let node_index = tap.node.0 as usize;
            let source = &self.arena[node_index * samples..(node_index + 1) * samples];
            let accumulator = &mut self.meter_accumulators[tap_index];
            accumulator.snapshot.latest_peak.fill(0.0);
            for frame in 0..frames {
                for channel in 0..self.channels {
                    let sample = source[frame * self.channels + channel];
                    let amplitude = sample.abs();
                    accumulator.snapshot.latest_peak[channel] =
                        accumulator.snapshot.latest_peak[channel].max(amplitude);
                    accumulator.snapshot.integrated_peak[channel] =
                        accumulator.snapshot.integrated_peak[channel].max(amplitude);
                    accumulator.square_sum[channel] += f64::from(sample) * f64::from(sample);
                }
            }
            accumulator.snapshot.frames_observed = accumulator
                .snapshot
                .frames_observed
                .saturating_add(frames as u64);
            let denominator = accumulator.snapshot.frames_observed as f64;
            for channel in 0..self.channels {
                accumulator.snapshot.integrated_rms[channel] =
                    (accumulator.square_sum[channel] / denominator).sqrt() as f32;
            }
        }
        self.refresh_meter_readings();
    }

    fn refresh_meter_readings(&mut self) {
        for (reading, accumulator) in self.meter_readings.iter_mut().zip(&self.meter_accumulators) {
            reading.snapshot = accumulator.snapshot;
        }
    }

    fn node_buffer(&self, node: usize, frames: usize) -> &[f32] {
        let samples = frames * self.channels;
        &self.arena[node * samples..(node + 1) * samples]
    }

    fn meter_readings(&self) -> &[MeterReading] {
        &self.meter_readings
    }

    #[cfg(test)]
    fn storage_fingerprint(&self) -> StorageFingerprint {
        StorageFingerprint {
            arena: (self.arena.as_ptr(), self.arena.capacity()),
            states: (self.states.as_ptr(), self.states.capacity()),
            meters: (
                self.meter_accumulators.as_ptr(),
                self.meter_accumulators.capacity(),
            ),
            readings: (self.meter_readings.as_ptr(), self.meter_readings.capacity()),
            delay_rings: self
                .states
                .iter()
                .filter_map(|state| match state {
                    RuntimeNodeState::Delay { ring, .. } => Some((ring.as_ptr(), ring.capacity())),
                    _ => None,
                })
                .collect(),
        }
    }
}

fn node_buffer_mut(arena: &mut [f32], node: usize, samples: usize) -> &mut [f32] {
    &mut arena[node * samples..(node + 1) * samples]
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct StorageFingerprint {
    arena: (*const f32, usize),
    states: (*const RuntimeNodeState, usize),
    meters: (*const MeterAccumulator, usize),
    readings: (*const MeterReading, usize),
    delay_rings: Vec<(*const f32, usize)>,
}

fn native_tileability(timing: NodeTiming) -> Tileability {
    if timing.lookbehind_frames == 0 && timing.lookahead_frames == 0 {
        Tileability::Stateless
    } else {
        Tileability::BoundedHistory {
            lookbehind_frames: timing.lookbehind_frames,
            lookahead_frames: timing.lookahead_frames,
        }
    }
}

fn tileability_covers(plan: Tileability, native: Tileability) -> bool {
    plan.covers(native)
}

/// Longest voice, in output frames, that `definition` can still be sounding
/// after any event in `events`, or `None` when a voice can last indefinitely.
///
/// This is the honest random-access requirement for a retained-state
/// instrument: a tile that prerolls this many frames re-triggers every voice
/// that can still be audible at its first frame. Sampler voices end at the
/// sample's last frame (played at the event's rate) or at their gate; synth
/// voices end at note-off or gate plus the envelope's slowest release. A synth
/// note that never receives note-off within the schedule is unbounded.
fn instrument_history_bound(
    definition: &BuiltInInstrumentDefinition,
    events: &[ScheduledEvent],
    output_sample_rate: u32,
) -> Option<u64> {
    let output_rate = f64::from(output_sample_rate.max(1));
    match definition {
        BuiltInInstrumentDefinition::Sampler { sample, params } => {
            let frames = sample.frame_count() as f64;
            let voice_frames = |semitones: f64| -> u64 {
                let rate =
                    2.0_f64.powf(semitones / 12.0) * f64::from(sample.sample_rate) / output_rate;
                if !rate.is_finite() || rate <= 0.0 {
                    return 0;
                }
                (frames / rate).ceil().max(0.0) as u64
            };
            let mut bound = 0_u64;
            for event in events {
                match &event.kind {
                    ScheduledKind::NoteOn {
                        instrument: Some(_),
                        pitch,
                        ..
                    } => {
                        let semitones = f64::from(pitch.midi_key) - f64::from(sample.root_key)
                            + f64::from(pitch.cents + sample.tuning_cents) / 100.0;
                        bound = bound.max(voice_frames(semitones));
                    }
                    ScheduledKind::Trigger {
                        target: TriggerTarget::Sample(asset),
                        pitch_semitones,
                        gate_frames,
                        ..
                    } if params.trigger_asset == Some(asset.get()) => {
                        let semitones =
                            f64::from(*pitch_semitones) + f64::from(sample.tuning_cents) / 100.0;
                        let mut voice = voice_frames(semitones);
                        if params.mode == crate::instruments::SamplerMode::Gated {
                            voice = voice.min(*gate_frames);
                        }
                        bound = bound.max(voice);
                    }
                    _ => {}
                }
            }
            Some(bound)
        }
        BuiltInInstrumentDefinition::Subtractive(params) => {
            // The envelope stretches release by up to 1.25x at zero release
            // velocity; one extra frame absorbs the level-to-zero rounding.
            let release_frames =
                (f64::from(params.envelope.release_seconds) * 1.25 * output_rate).ceil() as u64 + 1;
            let mut longest_gate = 0_u64;
            let mut any_voice = false;
            for (index, event) in events.iter().enumerate() {
                match &event.kind {
                    ScheduledKind::NoteOn {
                        clip,
                        note,
                        instrument: Some(_),
                        channel,
                        ..
                    } => {
                        any_voice = true;
                        let off =
                            events[index + 1..]
                                .iter()
                                .find_map(|later| match &later.kind {
                                    ScheduledKind::NoteOff {
                                        clip: off_clip,
                                        note: off_note,
                                        channel: off_channel,
                                        ..
                                    } if off_clip == clip
                                        && off_note == note
                                        && off_channel == channel =>
                                    {
                                        Some(later.project_frame.0)
                                    }
                                    _ => None,
                                })?;
                        let held = off.saturating_sub(event.project_frame.0).max(0) as u64;
                        longest_gate = longest_gate.max(held);
                    }
                    ScheduledKind::Trigger {
                        target: TriggerTarget::InstrumentNote { .. },
                        gate_frames,
                        ..
                    } => {
                        any_voice = true;
                        longest_gate = longest_gate.max(*gate_frames);
                    }
                    _ => {}
                }
            }
            Some(if any_voice {
                longest_gate.saturating_add(release_frames)
            } else {
                0
            })
        }
    }
}

fn validate_plan_schedule(
    plan: &RenderPlan,
    schedule: &DawEngineSchedule,
) -> Result<(), GraphCompileError> {
    let revisions = schedule.project_revision();
    let actual_revisions = ProjectRevisionStamp {
        aggregate: revisions.aggregate,
        arrangement: revisions.arrangement,
        sequencer: revisions.sequencer,
        automation: revisions.automation,
        assets: revisions.assets,
        mixer: revisions.mixer,
        sample_kits: revisions.sample_kits,
        air: revisions.air,
        bindings: revisions.bindings,
    };
    if plan.id.revisions != actual_revisions {
        return Err(GraphCompileError::PlanRevisionMismatch {
            expected: plan.id.revisions,
            actual: actual_revisions,
        });
    }
    let actual_format = schedule.render_schedule().format();
    let actual_format = RenderFormat::new(
        actual_format.sample_rate.get(),
        actual_format.channels.get(),
    )
    .expect("validated AudioFormat is a valid RenderFormat");
    if plan.format() != actual_format {
        return Err(GraphCompileError::PlanFormatMismatch {
            expected: plan.format(),
            actual: actual_format,
        });
    }
    let window = schedule.render_schedule().window();
    let actual_extent = RenderSpan::new(window.start, window.end)
        .expect("compiled schedule always has a non-empty window");
    if plan.extent() != actual_extent {
        return Err(GraphCompileError::PlanExtentMismatch {
            expected: plan.extent(),
            actual: actual_extent,
        });
    }
    let expected_block = plan.id.engine.canonical_block_frames.get();
    let actual_block = schedule.render_schedule().block_frames();
    if expected_block != actual_block {
        return Err(GraphCompileError::PlanBlockMismatch {
            expected: expected_block,
            actual: actual_block,
        });
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphCompileError {
    PlanRevisionMismatch {
        expected: ProjectRevisionStamp,
        actual: ProjectRevisionStamp,
    },
    PlanFormatMismatch {
        expected: RenderFormat,
        actual: RenderFormat,
    },
    PlanExtentMismatch {
        expected: RenderSpan,
        actual: RenderSpan,
    },
    PlanBlockMismatch {
        expected: u32,
        actual: u32,
    },
    SourceOutsidePlan {
        source: RenderSpan,
        plan: RenderSpan,
    },
    SourceSampleCount {
        expected: usize,
        actual: usize,
    },
    NonFiniteSource {
        index: usize,
    },
    NonFiniteGain,
    EventOutsidePlan {
        frame: i64,
        plan: RenderSpan,
    },
    DuplicateEventOrder {
        frame: i64,
        sequence: u32,
    },
    UnknownNode(GraphNodeId),
    EmptyMix,
    ZeroDelay,
    MissingOutput,
    DuplicateMeter(MeterTapId),
    TooManyMeterChannels {
        channels: u16,
        maximum: u16,
    },
    TileabilityUnderdeclared {
        plan: Tileability,
        native: Tileability,
    },
    GraphTooLarge,
}

impl fmt::Display for GraphCompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlanRevisionMismatch { .. } => {
                write!(
                    formatter,
                    "render plan revisions differ from frozen schedule"
                )
            }
            Self::PlanFormatMismatch { .. } => {
                write!(formatter, "render plan format differs from frozen schedule")
            }
            Self::PlanExtentMismatch { .. } => {
                write!(formatter, "render plan extent differs from frozen schedule")
            }
            Self::PlanBlockMismatch { .. } => {
                write!(
                    formatter,
                    "render plan block size differs from frozen schedule"
                )
            }
            Self::SourceOutsidePlan { source, plan } => write!(
                formatter,
                "frozen PCM span {source:?} is outside plan {plan:?}"
            ),
            Self::SourceSampleCount { expected, actual } => write!(
                formatter,
                "frozen PCM has {actual} samples; expected {expected}"
            ),
            Self::NonFiniteSource { index } => {
                write!(formatter, "frozen PCM sample {index} is not finite")
            }
            Self::NonFiniteGain => write!(formatter, "native gain must be finite"),
            Self::EventOutsidePlan { frame, .. } => {
                write!(formatter, "native event frame {frame} is outside the plan")
            }
            Self::DuplicateEventOrder { frame, sequence } => write!(
                formatter,
                "native events duplicate ordering key ({frame}, {sequence})"
            ),
            Self::UnknownNode(node) => write!(formatter, "unknown graph node {}", node.get()),
            Self::EmptyMix => write!(formatter, "mix node requires at least one input"),
            Self::ZeroDelay => write!(formatter, "delay node requires at least one frame"),
            Self::MissingOutput => write!(formatter, "compiled graph has no output node"),
            Self::DuplicateMeter(id) => write!(formatter, "duplicate meter tap {}", id.0),
            Self::TooManyMeterChannels { channels, maximum } => write!(
                formatter,
                "{channels} channels exceed fixed meter capacity {maximum}"
            ),
            Self::TileabilityUnderdeclared { plan, native } => write!(
                formatter,
                "plan tileability {plan:?} does not cover native graph requirement {native:?}"
            ),
            Self::GraphTooLarge => write!(formatter, "compiled graph exceeds addressable storage"),
        }
    }
}

impl Error for GraphCompileError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphExecutionError {
    RequiredProcessorRefused {
        count: usize,
    },
    SpanOutsidePlan {
        requested: RenderSpan,
        plan: RenderSpan,
    },
    SeekOutsidePlan {
        frame: i64,
        plan: RenderSpan,
    },
    DiscontinuousProcess {
        expected: i64,
        actual: i64,
    },
    PartialFrame {
        samples: usize,
        channels: u16,
    },
    BlockTooLarge {
        requested: usize,
        maximum: usize,
    },
    RenderTooLarge,
    UnknownOutputNode(GraphNodeId),
    MediaPreparation(String),
    MediaProviderMissingAsset(crate::arrangement::AssetId),
    MediaProviderShapeMismatch {
        asset: crate::arrangement::AssetId,
        expected: MediaAssetDescriptor,
        actual: MediaAssetDescriptor,
    },
    MediaUnavailable(MediaReadError),
    Instrument(String),
    Cancelled,
}

impl fmt::Display for GraphExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequiredProcessorRefused { count } => {
                write!(formatter, "{count} required graph processors were refused")
            }
            Self::SpanOutsidePlan { .. } => write!(formatter, "render span is outside the plan"),
            Self::SeekOutsidePlan { frame, .. } => {
                write!(formatter, "seek frame {frame} is outside the plan")
            }
            Self::DiscontinuousProcess { expected, actual } => write!(
                formatter,
                "realtime graph expected frame {expected}, got {actual}"
            ),
            Self::PartialFrame { samples, channels } => write!(
                formatter,
                "{samples} samples do not contain complete {channels}-channel frames"
            ),
            Self::BlockTooLarge { requested, maximum } => write!(
                formatter,
                "callback requested {requested} frames; compiled maximum is {maximum}"
            ),
            Self::RenderTooLarge => write!(formatter, "graph render exceeds addressable storage"),
            Self::UnknownOutputNode(node) => {
                write!(formatter, "graph output names unknown node {}", node.get())
            }
            Self::MediaPreparation(message) => {
                write!(formatter, "media preparation failed: {message}")
            }
            Self::MediaProviderMissingAsset(asset) => {
                write!(formatter, "media provider has no asset {}", asset.get())
            }
            Self::MediaProviderShapeMismatch {
                asset,
                expected,
                actual,
            } => write!(
                formatter,
                "media provider shape for asset {} changed from {expected:?} to {actual:?}",
                asset.get()
            ),
            Self::MediaUnavailable(error) => write!(
                formatter,
                "media asset {} frame {} channel {} is unavailable ({:?})",
                error.asset.get(),
                error.frame,
                error.channel,
                error.failure
            ),
            Self::Instrument(message) => {
                write!(formatter, "instrument execution failed: {message}")
            }
            Self::Cancelled => write!(formatter, "graph render was cancelled"),
        }
    }
}

impl Error for GraphExecutionError {}

impl From<MediaPreparationError> for GraphExecutionError {
    fn from(error: MediaPreparationError) -> Self {
        Self::MediaPreparation(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrangement::{AssetId, AudioLoopMode, ChannelMapping, ClipFades, ClipId, TrackId};
    use crate::automation::{AutomationGraph, FixedTempo};
    use crate::render_plan::{
        DeterminismGrade, EngineRecipeStamp, ProjectRevisionStamp, RenderPlanId,
    };

    fn digest(byte: u8) -> ExactDigest {
        ExactDigest::new([byte; 32])
    }

    fn plan(tileability: Tileability) -> Arc<RenderPlan> {
        let extent = RenderSpan::new(-4, 20).unwrap();
        let format = RenderFormat::new(48_000, 2).unwrap();
        let engine = EngineRecipeStamp::new(1, format, 8, 17, digest(9)).unwrap();
        let id = RenderPlanId::new(
            3,
            digest(1),
            ProjectRevisionStamp::default(),
            extent,
            engine,
            Vec::new(),
        )
        .unwrap();
        Arc::new(RenderPlan::new(id, DeterminismGrade::BitExact, tileability))
    }

    fn product(scale: f32) -> FrozenPcmProduct {
        let mut samples = Vec::new();
        for frame in 0..24 {
            samples.push(frame as f32 * scale);
            samples.push(-(frame as f32) * scale);
        }
        FrozenPcmProduct {
            scope: RenderScope::Master,
            span: RenderSpan::new(-4, 20).unwrap(),
            content: digest((scale * 10.0) as u8),
            interleaved: samples.into(),
        }
    }

    fn graph(with_delay: bool) -> Arc<CompiledGraph> {
        let tileability = if with_delay {
            Tileability::BoundedHistory {
                lookbehind_frames: 3,
                lookahead_frames: 0,
            }
        } else {
            Tileability::Stateless
        };
        let mut builder = CompiledGraphBuilder::for_test_plan(plan(tileability));
        let source_a = builder.add_frozen_pcm(product(1.0)).unwrap();
        let source_b = builder.add_frozen_pcm(product(0.1)).unwrap();
        let gained = builder
            .add_gain(
                source_a,
                0.5,
                vec![
                    TimestampedGain {
                        project_frame: 3,
                        sequence: 1,
                        linear: 0.25,
                    },
                    TimestampedGain {
                        project_frame: 3,
                        sequence: 0,
                        linear: 0.75,
                    },
                    TimestampedGain {
                        project_frame: 11,
                        sequence: 0,
                        linear: -0.5,
                    },
                ],
            )
            .unwrap();
        let mixed = builder.add_mix([(gained, 1.0), (source_b, 2.0)]).unwrap();
        let output = if with_delay {
            builder.add_delay(mixed, 3).unwrap()
        } else {
            mixed
        };
        builder.add_meter_tap(MeterTapId(7), output).unwrap();
        builder.set_output(output).unwrap();
        Arc::new(builder.finish().unwrap())
    }

    fn realtime_render(graph: Arc<CompiledGraph>, partitions: &[usize]) -> Vec<f32> {
        let mut executor = RealtimeGraphExecutor::new(graph.clone()).unwrap();
        executor.seek(graph.plan().extent().start).unwrap();
        let channels = usize::from(graph.plan().format().channels.get());
        let mut result = Vec::new();
        let mut remaining = graph.plan().extent().len() as usize;
        let mut partition = 0;
        while remaining > 0 {
            let frames = partitions[partition % partitions.len()].min(remaining);
            let mut block = vec![99.0; frames * channels];
            assert_eq!(executor.process_interleaved(&mut block).unwrap(), frames);
            result.extend(block);
            remaining -= frames;
            partition += 1;
        }
        result
    }

    #[derive(Clone, Debug)]
    struct TestMediaProvider {
        asset: AssetId,
        descriptor: MediaAssetDescriptor,
        samples: Arc<[f32]>,
        available: std::ops::Range<u64>,
    }

    impl MediaBlockProvider for TestMediaProvider {
        fn descriptor(&self, asset: AssetId) -> Option<MediaAssetDescriptor> {
            (asset == self.asset).then_some(self.descriptor)
        }

        fn sample(&self, asset: AssetId, frame: u64, channel: u16) -> Result<f32, MediaReadError> {
            let failure = |failure| MediaReadError {
                asset,
                frame,
                channel,
                failure,
            };
            if asset != self.asset {
                return Err(failure(daw_render::MediaReadFailure::UnknownAsset));
            }
            if frame >= self.descriptor.frame_count {
                return Err(failure(daw_render::MediaReadFailure::FrameOutsideAsset));
            }
            if channel != 0 {
                return Err(failure(daw_render::MediaReadFailure::ChannelOutsideAsset));
            }
            if !self.available.contains(&frame) {
                return Err(failure(daw_render::MediaReadFailure::FrameUnavailable));
            }
            Ok(self.samples[frame as usize])
        }
    }

    fn media_graph(provider: Arc<dyn MediaBlockProvider>) -> Arc<CompiledGraph> {
        let plan = plan(Tileability::Stateless);
        let descriptor = provider.descriptor(AssetId::from_raw(41)).unwrap();
        let automation = AutomationGraph::new()
            .compile(&FixedTempo::new(48_000, 120_000_000).unwrap())
            .unwrap();
        let clip = CompiledAudioClip {
            id: ClipId::from_raw(3),
            track: TrackId::from_raw(5),
            bus: BusId::from_raw(7),
            placement: RenderWindow::new(-4, 20).unwrap(),
            asset: AssetId::from_raw(41),
            source_start: 0,
            source_end: 24,
            ratio_source_frames: 1,
            ratio_project_frames: 1,
            reverse: false,
            channels: ChannelMapping::All,
            loop_mode: AudioLoopMode::Off,
            fades: ClipFades::default(),
            clip_gain_db: 0.0,
            track_gain_db: 0.0,
            track_pan: 0.0,
            renderable: true,
        };
        let mut builder =
            CompiledGraphBuilder::with_anchor(plan, ScheduleAnchor::KernelFixture, provider);
        let output = builder
            .add_audio_clip(clip, descriptor, Arc::new(automation))
            .unwrap();
        builder.set_output(output).unwrap();
        Arc::new(builder.finish().unwrap())
    }

    #[test]
    fn gain_mix_and_same_frame_event_order_are_partition_invariant() {
        let graph = graph(false);
        let canonical = realtime_render(graph.clone(), &[8]);
        let adversarial = realtime_render(graph, &[1, 3, 2, 7, 4]);
        assert_eq!(canonical, adversarial);
        // Frame 3 is source frame 7 due the signed plan origin. Sequence 1 is
        // last and therefore supplies gain 0.25 at the exact event frame.
        let index = (3 - (-4)) as usize * 2;
        assert_eq!(canonical[index], 7.0 * 0.25 + 0.7 * 2.0);
    }

    #[test]
    fn offline_and_realtime_invoke_the_same_kernel_byte_exactly() {
        let graph = graph(true);
        let mut offline = OfflineGraphExecutor::new(graph.clone()).unwrap();
        let rendered = offline
            .render(graph.plan().extent(), &RenderCancellation::new())
            .unwrap();
        let realtime = realtime_render(graph, &[2, 5, 1, 8]);
        assert_eq!(&*rendered.interleaved, &realtime);
    }

    #[test]
    fn bounded_seek_reconstructs_delay_context() {
        let graph = graph(true);
        let whole = realtime_render(graph.clone(), &[8]);
        let mut executor = RealtimeGraphExecutor::new(graph.clone()).unwrap();
        executor.seek(6).unwrap();
        let mut suffix = vec![0.0; 6 * 2];
        executor.process_interleaved(&mut suffix).unwrap();
        let offset = (6 - graph.plan().extent().start) as usize * 2;
        assert_eq!(&suffix, &whole[offset..offset + suffix.len()]);
    }

    #[test]
    fn callback_storage_does_not_move_or_grow() {
        let graph = graph(true);
        let mut executor = RealtimeGraphExecutor::new(graph).unwrap();
        let before = executor.storage_fingerprint();
        for frames in [1, 8, 3, 7, 2] {
            let mut block = [0.0_f32; 16];
            executor
                .process_interleaved(&mut block[..frames * 2])
                .unwrap();
        }
        let after = executor.storage_fingerprint();
        assert_eq!(before, after);
    }

    #[test]
    fn realtime_missing_chunks_are_observable_silence_while_offline_refuses() {
        let asset = AssetId::from_raw(41);
        let descriptor = MediaAssetDescriptor {
            format: crate::audio::AudioFormat::new(48_000, 1).unwrap(),
            frame_count: 24,
        };
        let samples: Arc<[f32]> =
            Arc::from((0..24).map(|frame| frame as f32 + 0.25).collect::<Vec<_>>());
        let sparse: Arc<dyn MediaBlockProvider> = Arc::new(TestMediaProvider {
            asset,
            descriptor,
            samples: Arc::clone(&samples),
            available: 0..5,
        });
        let graph = media_graph(Arc::clone(&sparse));

        let mut realtime = RealtimeGraphExecutor::new(Arc::clone(&graph)).unwrap();
        let mut block = [99.0_f32; 16];
        assert_eq!(realtime.process_interleaved(&mut block).unwrap(), 8);
        assert_eq!(
            &block[..8],
            &[0.25, 0.25, 1.25, 1.25, 2.25, 2.25, 3.25, 3.25]
        );
        assert!(block[8..].iter().all(|sample| *sample == 0.0));
        assert_eq!(
            realtime.media_status(),
            MediaRuntimeStatus {
                underrun_events: 1,
                unavailable_source_frames: 4,
                currently_underrun: true,
                last_error: Some(MediaReadError {
                    asset,
                    frame: 4,
                    channel: 0,
                    failure: daw_render::MediaReadFailure::FrameUnavailable,
                }),
            }
        );

        let mut offline = OfflineGraphExecutor::new(graph).unwrap();
        assert!(matches!(
            offline.render(RenderSpan::new(-4, 4).unwrap(), &RenderCancellation::new()),
            Err(GraphExecutionError::MediaUnavailable(MediaReadError {
                failure: daw_render::MediaReadFailure::FrameUnavailable,
                ..
            }))
        ));

        let complete: Arc<dyn MediaBlockProvider> = Arc::new(TestMediaProvider {
            asset,
            descriptor,
            samples,
            available: 0..24,
        });
        realtime.replace_media_provider(complete).unwrap();
        let mut recovered = [0.0_f32; 16];
        realtime.process_interleaved(&mut recovered).unwrap();
        assert!(!realtime.media_status().currently_underrun);
        assert_eq!(realtime.media_status().underrun_events, 1);
        assert_eq!(recovered[0], 8.25);
    }

    #[test]
    fn meter_integrals_are_partition_invariant() {
        let graph = graph(false);
        let mut canonical = RealtimeGraphExecutor::new(graph.clone()).unwrap();
        let mut odd = RealtimeGraphExecutor::new(graph).unwrap();
        for frames in [8, 8, 8] {
            let mut block = vec![0.0; frames * 2];
            canonical.process_interleaved(&mut block).unwrap();
        }
        for frames in [1, 3, 2, 7, 4, 7] {
            let mut block = vec![0.0; frames * 2];
            odd.process_interleaved(&mut block).unwrap();
        }
        let left = canonical.meter_readings()[0].snapshot;
        let right = odd.meter_readings()[0].snapshot;
        assert_eq!(left.frames_observed, right.frames_observed);
        assert_eq!(left.integrated_peak, right.integrated_peak);
        assert_eq!(left.integrated_rms, right.integrated_rms);
    }

    #[test]
    fn tile_contract_is_explicit_about_context_and_refusals() {
        let graph = graph(true);
        assert_eq!(
            graph.tile_contract(3, 0).unwrap(),
            TileExecutionContract {
                preroll_frames: 3,
                postroll_frames: 3,
                checkpoint_required: false,
            }
        );
        assert_eq!(
            graph.tile_contract(2, 0),
            Err(TileRefusal::PrerollCeilingExceeded {
                required: 3,
                ceiling: 2,
            })
        );
    }

    #[test]
    fn required_refusal_prevents_both_executor_contracts() {
        let mut builder = CompiledGraphBuilder::for_test_plan(plan(Tileability::Stateless));
        let source = builder.add_frozen_pcm(product(1.0)).unwrap();
        builder.set_output(source).unwrap();
        builder.refuse_processor("CLAP mystery", ProcessorRefusalReason::RealtimeUnsafe, true);
        let graph = Arc::new(builder.finish().unwrap());
        assert!(matches!(
            RealtimeGraphExecutor::new(graph.clone()),
            Err(GraphExecutionError::RequiredProcessorRefused { count: 1 })
        ));
        assert!(matches!(
            OfflineGraphExecutor::new(graph),
            Err(GraphExecutionError::RequiredProcessorRefused { count: 1 })
        ));
    }

    #[test]
    fn nonfinite_pcm_is_quarantined_at_compile_time() {
        let mut builder = CompiledGraphBuilder::for_test_plan(plan(Tileability::Stateless));
        let mut bad = product(1.0);
        Arc::make_mut(&mut bad.interleaved)[9] = f32::NAN;
        assert!(matches!(
            builder.add_frozen_pcm(bad),
            Err(GraphCompileError::NonFiniteSource { index: 9 })
        ));
    }

    fn sampler_definition(
        frames: usize,
        root_key: u8,
        mode: crate::instruments::SamplerMode,
    ) -> BuiltInInstrumentDefinition {
        BuiltInInstrumentDefinition::Sampler {
            sample: crate::instruments::SampleData {
                sample_rate: 48_000,
                channels: 1,
                interleaved: vec![0.5; frames].into(),
                root_key,
                tuning_cents: 0.0,
            },
            params: crate::instruments::SamplerParams {
                mode,
                trigger_asset: Some(5),
                ..crate::instruments::SamplerParams::default()
            },
        }
    }

    fn trigger(frame: i64, pitch_semitones: f32, gate_frames: u64) -> ScheduledEvent {
        use crate::sequencer::{PatternClipId, SampleAssetId, StepLaneId};
        ScheduledEvent {
            block_offset: 0,
            project_frame: crate::sequencer::ProjectFrame(frame),
            kind: ScheduledKind::Trigger {
                clip: PatternClipId::from_raw(1),
                lane: StepLaneId::from_raw(1),
                target: TriggerTarget::Sample(SampleAssetId::from_raw(5)),
                choke_group: None,
                velocity: 1.0,
                pan: 0.0,
                pitch_semitones,
                gate_frames,
                ratchet: 1,
            },
        }
    }

    fn note(frame: i64, on: bool, midi_key: u8) -> ScheduledEvent {
        use crate::sequencer::{Articulation, NoteId, NotePitch, PatternClipId};
        ScheduledEvent {
            block_offset: 0,
            project_frame: crate::sequencer::ProjectFrame(frame),
            kind: if on {
                ScheduledKind::NoteOn {
                    clip: PatternClipId::from_raw(1),
                    note: NoteId::from_raw(1),
                    instrument: Some(7),
                    pitch: NotePitch {
                        midi_key,
                        cents: 0.0,
                    },
                    velocity: 1.0,
                    pan: 0.0,
                    channel: 0,
                    articulation: Articulation::Normal,
                }
            } else {
                ScheduledKind::NoteOff {
                    clip: PatternClipId::from_raw(1),
                    note: NoteId::from_raw(1),
                    instrument: Some(7),
                    release_velocity: 1.0,
                    channel: 0,
                }
            },
        }
    }

    fn instrument_lookbehind(
        definition: BuiltInInstrumentDefinition,
        events: Vec<ScheduledEvent>,
    ) -> u64 {
        let mut builder = CompiledGraphBuilder::for_test_plan(plan(Tileability::SequentialOnly));
        let node = builder
            .add_instrument(7, definition, events.into())
            .unwrap();
        builder.timings[node.0 as usize].lookbehind_frames
    }

    #[test]
    fn instrument_lookbehind_is_the_longest_voice_not_the_whole_extent() {
        use crate::instruments::SamplerMode;
        // Plan extent is 24 frames at 48 kHz; a 6-frame one-shot at root pitch
        // can only be sounding for 6 frames after its trigger.
        assert_eq!(
            instrument_lookbehind(
                sampler_definition(6, 60, SamplerMode::OneShot),
                vec![trigger(2, 0.0, 100)]
            ),
            6
        );
        // An octave down halves the playback rate and doubles the voice.
        assert_eq!(
            instrument_lookbehind(
                sampler_definition(6, 60, SamplerMode::OneShot),
                vec![note(2, true, 48)]
            ),
            12
        );
        // A gated pad ends at its gate when that is shorter than the sample.
        assert_eq!(
            instrument_lookbehind(
                sampler_definition(6, 60, SamplerMode::Gated),
                vec![trigger(2, 0.0, 4)]
            ),
            4
        );
        // No events that reach this instrument: nothing can be sounding.
        assert_eq!(
            instrument_lookbehind(sampler_definition(6, 60, SamplerMode::OneShot), Vec::new()),
            0
        );
        // The whole extent stays the ceiling for a voice longer than the plan.
        assert_eq!(
            instrument_lookbehind(
                sampler_definition(1_000, 60, SamplerMode::OneShot),
                vec![trigger(2, 0.0, 100)]
            ),
            24
        );
    }

    #[test]
    fn synth_lookbehind_adds_release_and_stays_unbounded_without_note_off() {
        let params = crate::instruments::SynthParams::default();
        let release =
            (f64::from(params.envelope.release_seconds) * 1.25 * 48_000.0).ceil() as u64 + 1;
        let held = instrument_lookbehind(
            BuiltInInstrumentDefinition::Subtractive(params.clone()),
            vec![note(2, true, 60), note(10, false, 60)],
        );
        // Bounded by the extent (24 frames) because the release is much longer
        // than the plan; the raw bound is the held length plus release.
        assert_eq!(held, (8 + release).min(24));
        let mut short = params.clone();
        short.envelope.release_seconds = 0.0;
        assert_eq!(
            instrument_lookbehind(
                BuiltInInstrumentDefinition::Subtractive(short),
                vec![note(2, true, 60), note(10, false, 60)],
            ),
            9
        );
        assert_eq!(
            instrument_lookbehind(
                BuiltInInstrumentDefinition::Subtractive(params),
                vec![note(2, true, 60)],
            ),
            24
        );
    }

    #[test]
    fn stateless_plan_is_tightened_to_the_native_requirement_not_refused() {
        let declared = Tileability::Stateless;
        let native = Tileability::BoundedHistory {
            lookbehind_frames: 6,
            lookahead_frames: 0,
        };
        assert_eq!(declared.covering(native), native);
        assert_eq!(native.covering(declared), native);
        assert_eq!(
            Tileability::SequentialOnly.covering(native),
            Tileability::SequentialOnly
        );
        let mut builder = CompiledGraphBuilder::for_test_plan(plan(declared.covering(native)));
        let node = builder
            .add_instrument(
                7,
                sampler_definition(6, 60, crate::instruments::SamplerMode::OneShot),
                vec![trigger(2, 0.0, 100)].into(),
            )
            .unwrap();
        builder.set_output(node).unwrap();
        let graph = builder.finish().unwrap();
        assert_eq!(graph.native_tileability(), native);
    }
}
