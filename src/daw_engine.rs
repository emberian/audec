//! Audible execution bridge for the aggregate DAW project.
//!
//! [`DawProject`](crate::daw_project::DawProject) deliberately keeps editable
//! domain state separate from DSP. This module is the narrow bridge that
//! freezes that aggregate state with [`crate::daw_render`], resolves media-pool
//! identities to caller-supplied PCM, and produces audio suitable for
//! [`crate::audio_host::AudioHost`]. The resulting [`DawEngineSchedule`] owns
//! immutable, shared inputs; edits to the project or PCM map cannot alter a
//! schedule already handed to a render worker.
//!
//! The built-in path is intentionally honest about its current boundary. It
//! renders audio clips, rational resampling, fades, clip/track/bus gain and
//! pan, automation, main routes, and pre/post-fader sends. It also renders the
//! built-in instruments for explicitly addressed sequencer triggers. It does
//! not execute plugins, guess a destination for identity-free note events,
//! perform pitch-preserving stretching, apply plugin latency, or honor
//! per-clip mixer-bus overrides. Those cases remain silent or use the
//! documented fallback and always produce diagnostics.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::arrangement::{self, ClipId};
use crate::assets::{self, AssetAvailability};
use crate::audio::{AudioError, ProjectAudio};
use crate::daw_project::{BridgeError, DawProject, ProjectRevisions};
use crate::daw_render::{
    self, CompileError, PcmAsset, ProcessorRuntimeInfo, ReferenceRenderError, RenderCancellation,
    RenderCompileRequest, RenderDiagnostic, RenderSchedule, RenderWindow,
};
use crate::instruments::{
    BuiltInInstrument, InstrumentError, SampleData, Sampler, SamplerParams, SubtractiveSynth,
    SynthParams,
};
use crate::mixer::{BusId, ProcessorId};
use crate::render_plan::{BusTap, RenderScope};
use crate::sampler_runtime::{self, SamplerRuntimeDiagnostic};
use crate::sequencer::{ScheduledEvent, ScheduledKind, TriggerTarget};

#[allow(unused_imports)]
pub use crate::sampler_runtime::{
    build_authoritative_sampler_routes, ResolvedSamplerRoute, SamplerRouteBuild,
};

/// PCM supplied by the media decoder, keyed in the media-pool ID domain.
///
/// This intentionally does not use `arrangement::AssetId`: the project binding
/// table is the only authority allowed to cross that identity boundary.
pub type AssetPcmMap = BTreeMap<assets::AssetId, PcmAsset>;

/// A reconstructible built-in instrument.  Definitions, rather than live
/// voices, are frozen into a schedule so every offline render starts from the
/// same voice state and remains independent of block boundaries.
#[derive(Clone, Debug)]
pub enum BuiltInInstrumentDefinition {
    Subtractive(SynthParams),
    Sampler {
        sample: SampleData,
        params: SamplerParams,
    },
}

impl BuiltInInstrumentDefinition {
    fn instantiate(
        &self,
        sample_rate: u32,
        identity: u64,
    ) -> Result<BuiltInInstrument, InstrumentError> {
        match self {
            Self::Subtractive(params) => Ok(BuiltInInstrument::Subtractive(SubtractiveSynth::new(
                sample_rate,
                identity,
                params.clone(),
            )?)),
            Self::Sampler { sample, params } => Ok(BuiltInInstrument::Sampler(Sampler::new(
                sample_rate,
                sample.clone(),
                params.clone(),
            )?)),
        }
    }

    fn validate(&self, sample_rate: u32, identity: u64) -> Result<(), InstrumentError> {
        // Construction is the authoritative validation path and has no side
        // effects outside the temporary voice allocation.
        let _ = self.instantiate(sample_rate, identity)?;
        Ok(())
    }

    fn consumes(&self, identity: u64, event: &ScheduledEvent) -> bool {
        match (self, &event.kind) {
            (_, ScheduledKind::LoopBoundary) => true,
            (
                Self::Subtractive(_) | Self::Sampler { .. },
                ScheduledKind::NoteOn {
                    instrument: Some(instrument),
                    ..
                }
                | ScheduledKind::NoteOff {
                    instrument: Some(instrument),
                    ..
                }
                | ScheduledKind::NoteExpression {
                    instrument: Some(instrument),
                    ..
                },
            ) => *instrument == identity,
            (
                Self::Subtractive(_),
                ScheduledKind::Trigger {
                    target: TriggerTarget::InstrumentNote { instrument, .. },
                    ..
                },
            ) => *instrument == identity,
            (
                Self::Sampler { params, .. },
                ScheduledKind::Trigger {
                    target: TriggerTarget::Sample(asset),
                    ..
                },
            ) => params.trigger_asset == Some(asset.get()),
            _ => false,
        }
    }

    fn observes(&self, identity: u64, event: &ScheduledEvent) -> bool {
        self.consumes(identity, event)
            || matches!(
                (self, &event.kind),
                (
                    Self::Sampler { .. },
                    ScheduledKind::Trigger {
                        target: TriggerTarget::Sample(_),
                        choke_group: Some(_),
                        ..
                    }
                )
            )
    }
}

/// An explicit sequencer identity to mixer-bus assignment.  A definition only
/// consumes trigger events whose target carries its matching identity (or its
/// explicitly configured sampler asset alias).
#[derive(Clone, Debug)]
pub struct BuiltInInstrumentRoute {
    pub definition: BuiltInInstrumentDefinition,
    pub bus: BusId,
}

/// Fixed behavior of the current built-in engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineCapabilities {
    pub audio_clips: bool,
    pub rational_resampling: bool,
    pub clip_and_mixer_automation: bool,
    pub mixer_routes_and_sends: bool,
    pub per_clip_bus_overrides: bool,
    pub instruments: bool,
    pub plugins: bool,
    pub pitch_preserving_stretch: bool,
    pub plugin_latency_compensation: bool,
}

pub const BUILTIN_ENGINE_CAPABILITIES: EngineCapabilities = EngineCapabilities {
    audio_clips: true,
    // Clip-rate interpolation is supported only inside the frozen project
    // sample rate. Cross-rate asset conversion is not implemented; immutable
    // registry/PCM rate disagreement is diagnosed and export-refused.
    rational_resampling: false,
    clip_and_mixer_automation: true,
    mixer_routes_and_sends: true,
    per_clip_bus_overrides: false,
    instruments: true,
    plugins: false,
    pitch_preserving_stretch: false,
    plugin_latency_compensation: false,
};

/// Stable compile settings; processor facts normally come from the plugin
/// host's control thread.
#[derive(Clone, Debug)]
pub struct DawEngineConfig {
    pub output_channels: u16,
    pub block_frames: u32,
    pub performance_seed: u64,
    pub processors: BTreeMap<ProcessorId, ProcessorRuntimeInfo>,
    /// Explicit sequencer identities and their mixer destinations.  This is
    /// deliberately configuration supplied by the audio graph, not inferred
    /// from a pattern name or track label.
    pub instruments: BTreeMap<u64, BuiltInInstrumentRoute>,
}

impl Default for DawEngineConfig {
    fn default() -> Self {
        Self {
            output_channels: 2,
            block_frames: 512,
            performance_seed: 0,
            processors: BTreeMap::new(),
            instruments: BTreeMap::new(),
        }
    }
}

/// Diagnostics introduced while crossing aggregate-domain boundaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineDiagnostic {
    /// The registry record is deliberately offline even if stale PCM was
    /// supplied by a decoder cache, so the engine refuses to use it.
    RegistryAssetOffline { asset: assets::AssetId },
    /// A bound, present media asset has no decoded PCM in the supplied map.
    PcmNotSupplied {
        asset: assets::AssetId,
        arrangement_alias: arrangement::AssetId,
    },
    /// PCM facts disagree with immutable import metadata. The bridge refuses
    /// the affected PCM rather than rendering a potentially shifted source.
    PcmMetadataMismatch {
        asset: assets::AssetId,
        arrangement_alias: arrangement::AssetId,
        registry_sample_rate: u32,
        pcm_sample_rate: u32,
        registry_channels: u16,
        pcm_channels: u16,
        registry_frames: u64,
        pcm_frames: u64,
    },
    /// The reference schedule currently binds at track granularity. A
    /// different per-clip destination therefore falls back to the track bus
    /// (or master when the track itself is unbound).
    ClipBusOverrideUnsupported {
        clip: ClipId,
        requested: BusId,
        rendered_to: BusId,
    },
    /// An instrument definition must always name an extant bus; silently
    /// falling back to master would make a routing error audible but hidden.
    InstrumentBusMissing { instrument: u64, bus: BusId },
    /// A legacy note event has no target/instrument identity, so this engine
    /// deliberately does not broadcast it to instruments.
    IdentityFreeNoteEvents { count: usize },
    /// A target referred to a built-in instrument identity that was not
    /// supplied in [`DawEngineConfig::instruments`].
    InstrumentNotSupplied { instrument: u64 },
    /// Trigger types without a configured built-in identity (drum racks,
    /// analysis templates, or unbound sample targets) are not guessed.
    UnroutableSequencerEvents { count: usize },
    /// A persistent sample target could not be proven against supplied PCM.
    SamplerRuntime(SamplerRuntimeDiagnostic),
    /// Only one sampler may consume a sequencer sample alias. The lowest
    /// explicitly configured identity supplies behavioral overrides and any
    /// later duplicate is suppressed deterministically.
    DuplicateSamplerConsumerSuppressed {
        sample_alias: u64,
        retained_instrument: u64,
        suppressed_instrument: u64,
    },
}

/// A fully frozen control-thread product. Both schedule and media are shared
/// immutably, so rendering a subwindow cannot observe later project edits or
/// decoder-cache replacement.
#[derive(Clone, Debug)]
pub struct DawEngineSchedule {
    project_revision: ProjectRevisions,
    schedule: Arc<RenderSchedule>,
    assets: Arc<BTreeMap<arrangement::AssetId, PcmAsset>>,
    instruments: Arc<BTreeMap<u64, BuiltInInstrumentRoute>>,
    diagnostics: Arc<[EngineDiagnostic]>,
}

impl DawEngineSchedule {
    pub const fn project_revision(&self) -> ProjectRevisions {
        self.project_revision
    }

    pub fn render_schedule(&self) -> &RenderSchedule {
        &self.schedule
    }

    pub fn engine_diagnostics(&self) -> &[EngineDiagnostic] {
        &self.diagnostics
    }

    pub fn render_diagnostics(&self) -> &[RenderDiagnostic] {
        self.schedule.diagnostics()
    }

    pub const fn capabilities(&self) -> EngineCapabilities {
        BUILTIN_ENGINE_CAPABILITIES
    }

    /// Render any exact, half-open subwindow of the compiled schedule.
    pub fn render(
        &self,
        window: RenderWindow,
        cancellation: &RenderCancellation,
    ) -> Result<DawEngineRender, DawEngineError> {
        let scoped = self.render_scopes(window, &[RenderScope::Master], cancellation)?;
        let audio = ProjectAudio::new(
            scoped.format,
            scoped
                .output(&RenderScope::Master)
                .expect("master was explicitly requested"),
        )?;
        Ok(DawEngineRender {
            origin_frame: window.start,
            audio,
            engine_diagnostics: scoped.engine_diagnostics,
            render_diagnostics: scoped.render_diagnostics,
        })
    }

    /// Execute the frozen sources and mixer once, then project any requested
    /// semantic outputs from that traversal. Scope order and duplication do
    /// not affect PCM. Unsupported or unknown scopes fail explicitly.
    pub fn render_scopes(
        &self,
        window: RenderWindow,
        scopes: &[RenderScope],
        cancellation: &RenderCancellation,
    ) -> Result<DawEngineScopedRender, DawEngineError> {
        cancellation_check(cancellation)?;
        let instrument_sources = render_built_in_instrument_sources(
            &self.schedule,
            &self.instruments,
            window,
            cancellation,
        )?;
        let mut rendered = daw_render::render_pcm_reference_with_bus_sources(
            &self.schedule,
            &self.assets,
            window,
            &instrument_sources,
            cancellation,
        )?;
        // `render_pcm_reference` correctly reports that it did not itself
        // execute sequencer events or arrangement pattern clips. This bridge
        // consumes their linked, explicitly routable subset immediately
        // below, so replace those broad diagnostics with the precise
        // compile-time engine diagnostics instead.
        rendered.diagnostics.retain(|diagnostic| {
            !matches!(
                diagnostic,
                RenderDiagnostic::SequencerEventsNeedInstrument { .. }
                    | RenderDiagnostic::ArrangementPatternNeedsInstrument { .. }
            )
        });
        let mut outputs = BTreeMap::new();
        for scope in scopes {
            if outputs.contains_key(scope) {
                continue;
            }
            let pcm: Arc<[f32]> = match scope {
                RenderScope::Master => rendered.interleaved.clone().into(),
                RenderScope::Bus { bus, tap } => {
                    let bus = BusId::from_raw(*bus);
                    let taps = rendered
                        .bus_taps
                        .get(&bus)
                        .ok_or(DawEngineError::UnknownRenderBus(bus))?;
                    match tap {
                        BusTap::PreFader => taps.pre_fader.clone().into(),
                        BusTap::PostFader => taps.post_fader.clone().into(),
                        BusTap::Output => taps.output.clone().into(),
                    }
                }
                RenderScope::Track(track) => rendered
                    .track_stems
                    .get(&arrangement::TrackId::from_raw(*track))
                    .cloned()
                    .ok_or(DawEngineError::UnknownRenderTrack(*track))?
                    .into(),
                RenderScope::Explanation(_) => {
                    return Err(DawEngineError::UnsupportedRenderScope(scope.clone()))
                }
            };
            outputs.insert(scope.clone(), pcm);
        }
        Ok(DawEngineScopedRender {
            origin_frame: window.start,
            format: rendered.format,
            outputs,
            engine_diagnostics: Arc::clone(&self.diagnostics),
            render_diagnostics: rendered.diagnostics.into(),
        })
    }

    /// Render the complete schedule as a finite buffer ready for
    /// `AudioHost::open`. `origin_frame` retains the signed project coordinate
    /// because `ProjectAudio` itself is necessarily zero-based.
    pub fn render_for_audition(
        &self,
        cancellation: &RenderCancellation,
    ) -> Result<DawEngineRender, DawEngineError> {
        self.render(self.schedule.window(), cancellation)
    }
}

/// Rendered PCM and both layers of diagnostics.
#[derive(Clone, Debug)]
pub struct DawEngineRender {
    pub origin_frame: i64,
    pub audio: ProjectAudio,
    pub engine_diagnostics: Arc<[EngineDiagnostic]>,
    pub render_diagnostics: Arc<[RenderDiagnostic]>,
}

#[derive(Clone, Debug)]
pub struct DawEngineScopedRender {
    pub origin_frame: i64,
    pub format: crate::audio::AudioFormat,
    outputs: BTreeMap<RenderScope, Arc<[f32]>>,
    pub engine_diagnostics: Arc<[EngineDiagnostic]>,
    pub render_diagnostics: Arc<[RenderDiagnostic]>,
}

impl DawEngineScopedRender {
    pub fn output(&self, scope: &RenderScope) -> Option<Arc<[f32]>> {
        self.outputs.get(scope).cloned()
    }

    pub fn scopes(&self) -> impl ExactSizeIterator<Item = &RenderScope> {
        self.outputs.keys()
    }
}

impl DawEngineRender {
    /// Consume the result for `AudioHost::open` or another transport adapter.
    pub fn into_project_audio(self) -> ProjectAudio {
        self.audio
    }
}

/// Freeze one validated aggregate project and resolve its bound PCM.
///
/// The media input is borrowed only for this call. Successful compilation
/// clones the referenced `Arc`-backed [`PcmAsset`] values into the immutable
/// engine schedule; unbound decoder-cache entries are ignored.
pub fn compile_daw_engine(
    project: &DawProject,
    pcm: &AssetPcmMap,
    window: RenderWindow,
    config: &DawEngineConfig,
    cancellation: &RenderCancellation,
) -> Result<DawEngineSchedule, DawEngineError> {
    if cancellation.is_cancelled() {
        return Err(DawEngineError::Cancelled);
    }
    project.require_valid()?;
    let state = project.state();
    let master = state.domains.mixer.master();
    let mut engine_diagnostics = Vec::new();

    let sampler_routes = sampler_runtime::build_authoritative_sampler_routes(project, pcm)?;
    engine_diagnostics.extend(
        sampler_routes
            .diagnostics
            .into_iter()
            .map(EngineDiagnostic::SamplerRuntime),
    );
    let instruments = merge_instrument_routes(
        &config.instruments,
        sampler_routes.routes,
        &mut engine_diagnostics,
    );

    for (&identity, route) in &instruments {
        if cancellation.is_cancelled() {
            return Err(DawEngineError::Cancelled);
        }
        if state.domains.mixer.bus(route.bus).is_none() {
            engine_diagnostics.push(EngineDiagnostic::InstrumentBusMissing {
                instrument: identity,
                bus: route.bus,
            });
            continue;
        }
        route
            .definition
            .validate(state.domains.arrangement.sample_rate, identity)?;
    }

    // Resolve only through explicit cross-domain bindings. Aliases are the
    // IDs consumed by daw_render; registry IDs are the public decoder API.
    let mut aliased_pcm = BTreeMap::new();
    for (&alias, &registry_id) in &state.bindings.assets.arrangement_assets {
        if cancellation.is_cancelled() {
            return Err(DawEngineError::Cancelled);
        }
        let registry_asset = state
            .domains
            .assets
            .get(registry_id)
            .expect("aggregate validation guarantees bound media");
        if matches!(
            registry_asset.availability(),
            AssetAvailability::Missing { .. }
        ) {
            engine_diagnostics.push(EngineDiagnostic::RegistryAssetOffline { asset: registry_id });
            continue;
        }
        let Some(asset_pcm) = pcm.get(&registry_id) else {
            engine_diagnostics.push(EngineDiagnostic::PcmNotSupplied {
                asset: registry_id,
                arrangement_alias: alias,
            });
            continue;
        };
        let metadata = registry_asset.metadata();
        let pcm_sample_rate = asset_pcm.format.sample_rate.get();
        let pcm_channels = asset_pcm.format.channels.get();
        let pcm_frames = asset_pcm.frame_count();
        if metadata.sample_rate_hz != pcm_sample_rate
            || metadata.channels != pcm_channels
            || metadata.frame_count.0 != pcm_frames
        {
            engine_diagnostics.push(EngineDiagnostic::PcmMetadataMismatch {
                asset: registry_id,
                arrangement_alias: alias,
                registry_sample_rate: metadata.sample_rate_hz,
                pcm_sample_rate,
                registry_channels: metadata.channels,
                pcm_channels,
                registry_frames: metadata.frame_count.0,
                pcm_frames,
            });
            continue;
        }
        aliased_pcm.insert(alias, asset_pcm.clone());
    }

    for (&clip, &requested) in &state.bindings.mixer.clip_overrides {
        let Some(arrangement_clip) = state.domains.arrangement.clips.get(&clip) else {
            continue;
        };
        let rendered_to = state
            .bindings
            .mixer
            .tracks
            .get(&arrangement_clip.track_id)
            .copied()
            .unwrap_or(master);
        if requested != rendered_to {
            engine_diagnostics.push(EngineDiagnostic::ClipBusOverrideUnsupported {
                clip,
                requested,
                rendered_to,
            });
        }
    }

    let schedule = daw_render::compile_render_schedule(
        RenderCompileRequest {
            arrangement: &state.domains.arrangement,
            sequencer: &state.domains.sequencer,
            automation: &state.domains.automation,
            mixer: &state.domains.mixer,
            track_buses: &state.bindings.mixer.tracks,
            processors: &config.processors,
            window,
            output_channels: config.output_channels,
            block_frames: config.block_frames,
            performance_seed: config.performance_seed,
        },
        cancellation,
    )?;

    let mut identity_free_note_events = 0_usize;
    let mut unroutable_sequencer_events = 0_usize;
    let mut unresolved_instruments = BTreeSet::new();
    for block in schedule.blocks() {
        if cancellation.is_cancelled() {
            return Err(DawEngineError::Cancelled);
        }
        for event in block.sequencer_events.iter() {
            match event.kind.clone() {
                ScheduledKind::NoteOn {
                    instrument: None, ..
                }
                | ScheduledKind::NoteOff {
                    instrument: None, ..
                }
                | ScheduledKind::NoteExpression {
                    instrument: None, ..
                } => {
                    identity_free_note_events = identity_free_note_events.saturating_add(1);
                }
                ScheduledKind::NoteOn {
                    instrument: Some(instrument),
                    ..
                }
                | ScheduledKind::NoteOff {
                    instrument: Some(instrument),
                    ..
                }
                | ScheduledKind::NoteExpression {
                    instrument: Some(instrument),
                    ..
                } if !instruments.contains_key(&instrument) => {
                    unresolved_instruments.insert(instrument);
                }
                ScheduledKind::Trigger {
                    target: TriggerTarget::InstrumentNote { instrument, .. },
                    ..
                } if !instruments
                    .iter()
                    .any(|(&identity, route)| route.definition.consumes(identity, event)) =>
                {
                    unresolved_instruments.insert(instrument);
                }
                ScheduledKind::Trigger { .. }
                    if !instruments
                        .iter()
                        .any(|(&identity, route)| route.definition.consumes(identity, event)) =>
                {
                    unroutable_sequencer_events = unroutable_sequencer_events.saturating_add(1);
                }
                _ => {}
            }
        }
    }
    if identity_free_note_events > 0 {
        engine_diagnostics.push(EngineDiagnostic::IdentityFreeNoteEvents {
            count: identity_free_note_events,
        });
    }
    if unroutable_sequencer_events > 0 {
        engine_diagnostics.push(EngineDiagnostic::UnroutableSequencerEvents {
            count: unroutable_sequencer_events,
        });
    }
    engine_diagnostics.extend(
        unresolved_instruments
            .into_iter()
            .map(|instrument| EngineDiagnostic::InstrumentNotSupplied { instrument }),
    );

    // Freeze only routes whose buses exist. A bad configuration is
    // represented as silence with a deterministic diagnostic above rather
    // than a later, potentially stale, fallback.
    let instruments = instruments
        .into_iter()
        .filter(|(_, route)| state.domains.mixer.bus(route.bus).is_some())
        .collect();

    Ok(DawEngineSchedule {
        project_revision: project.revisions(),
        schedule: Arc::new(schedule),
        assets: Arc::new(aliased_pcm),
        instruments: Arc::new(instruments),
        diagnostics: engine_diagnostics.into(),
    })
}

/// Merge caller-supplied instruments with authoritative kit routes.
///
/// Persisted material, bus, gain, pan, tuning, and pad choke intent always
/// win for typed sample targets. An explicit sampler for the same alias may
/// still choose one-shot versus gated behavior and voice capacity. Exactly
/// one definition consumes each sample alias.
fn merge_instrument_routes(
    configured: &BTreeMap<u64, BuiltInInstrumentRoute>,
    authoritative: Vec<ResolvedSamplerRoute>,
    diagnostics: &mut Vec<EngineDiagnostic>,
) -> BTreeMap<u64, BuiltInInstrumentRoute> {
    let authoritative_aliases: BTreeSet<_> = authoritative
        .iter()
        .map(|route| route.sample_alias.get())
        .collect();
    let mut sampler_behavior = BTreeMap::<u64, (u64, SamplerParams)>::new();
    let mut merged = BTreeMap::new();

    for (&identity, route) in configured {
        let Some(alias) = (match &route.definition {
            BuiltInInstrumentDefinition::Sampler { params, .. } => params.trigger_asset,
            BuiltInInstrumentDefinition::Subtractive(_) => None,
        }) else {
            merged.insert(identity, route.clone());
            continue;
        };
        if let Some((retained, _)) = sampler_behavior.get(&alias) {
            diagnostics.push(EngineDiagnostic::DuplicateSamplerConsumerSuppressed {
                sample_alias: alias,
                retained_instrument: *retained,
                suppressed_instrument: identity,
            });
            continue;
        }
        let BuiltInInstrumentDefinition::Sampler { params, .. } = &route.definition else {
            unreachable!("sample alias only came from a sampler")
        };
        sampler_behavior.insert(alias, (identity, params.clone()));
        if !authoritative_aliases.contains(&alias) {
            merged.insert(identity, route.clone());
        }
    }

    let mut generated_identity = u64::MAX;
    for route in authoritative {
        let alias = route.sample_alias.get();
        let (identity, behavior) = if let Some((identity, params)) = sampler_behavior.get(&alias) {
            (*identity, Some(params))
        } else {
            while merged.contains_key(&generated_identity) {
                generated_identity = generated_identity.saturating_sub(1);
            }
            let identity = generated_identity;
            generated_identity = generated_identity.saturating_sub(1);
            (identity, None)
        };
        let mut params = route.params;
        if let Some(behavior) = behavior {
            params.mode = behavior.mode;
            params.maximum_voices = behavior.maximum_voices;
        }
        merged.insert(
            identity,
            BuiltInInstrumentRoute {
                definition: BuiltInInstrumentDefinition::Sampler {
                    sample: route.sample,
                    params,
                },
                bus: route.bus,
            },
        );
    }
    merged
}

/// Render explicitly addressed built-ins only to their frozen source buses.
/// The reference executor then combines these with clip sources before one
/// authoritative fader/route traversal, so every master/bus/stem observation
/// comes from the same execution.
fn render_built_in_instrument_sources(
    schedule: &RenderSchedule,
    routes: &BTreeMap<u64, BuiltInInstrumentRoute>,
    window: RenderWindow,
    cancellation: &RenderCancellation,
) -> Result<BTreeMap<BusId, Vec<f32>>, DawEngineError> {
    let format = schedule.format();
    let channels = usize::from(format.channels.get());
    let frame_count = usize::try_from(window.len())
        .map_err(|_| DawEngineError::Render(ReferenceRenderError::RenderTooLarge))?;
    let sample_count = frame_count
        .checked_mul(channels)
        .ok_or(DawEngineError::Render(ReferenceRenderError::RenderTooLarge))?;
    if routes.is_empty() {
        return Ok(BTreeMap::new());
    }

    let mut instruments = Vec::with_capacity(routes.len());
    for (&identity, route) in routes {
        cancellation_check(cancellation)?;
        instruments.push((
            identity,
            route.bus,
            route.definition.clone(),
            route
                .definition
                .instantiate(format.sample_rate.get(), identity)?,
        ));
    }
    let sampler_choke_groups = sampler_runtime::route_choke_groups(routes);
    let mut bus_audio = BTreeMap::<BusId, Vec<f32>>::new();

    // Always run from the beginning of the frozen schedule, even for a
    // subwindow, so envelopes and sampler voices which began earlier retain
    // their exact state at the requested window start.
    for block in schedule.blocks() {
        if block.window.start >= window.end {
            break;
        }
        cancellation_check(cancellation)?;
        let block_frames = usize::try_from(block.window.len())
            .map_err(|_| DawEngineError::Render(ReferenceRenderError::RenderTooLarge))?;
        let routed_events: Vec<_> = block
            .sequencer_events
            .iter()
            .cloned()
            .map(|mut event| {
                if let ScheduledKind::Trigger {
                    target: TriggerTarget::Sample(alias),
                    choke_group,
                    ..
                } = &mut event.kind
                {
                    if choke_group.is_none() {
                        *choke_group = sampler_choke_groups.get(&alias.get()).copied().flatten();
                    }
                }
                event
            })
            .collect();
        for (identity, bus, definition, instrument) in &mut instruments {
            let accepted: Vec<_> = routed_events
                .iter()
                .filter(|event| definition.observes(*identity, event))
                .cloned()
                .collect();
            let mut rendered = vec![0.0_f32; block_frames.saturating_mul(2)];
            instrument.render_scheduled_block(block.window.start, &accepted, &mut rendered)?;
            let Some(overlap) = block.window.intersection(window) else {
                continue;
            };
            let target = bus_audio
                .entry(*bus)
                .or_insert_with(|| vec![0.0; sample_count]);
            for frame in overlap.start..overlap.end {
                let source_index = usize::try_from(frame - block.window.start).unwrap() * 2;
                let target_frame = usize::try_from(frame - window.start).unwrap();
                if channels == 1 {
                    target[target_frame] +=
                        (rendered[source_index] + rendered[source_index + 1]) * 0.5;
                } else {
                    let target_index = target_frame * 2;
                    target[target_index] += rendered[source_index];
                    target[target_index + 1] += rendered[source_index + 1];
                }
            }
        }
    }

    Ok(bus_audio)
}

fn cancellation_check(cancellation: &RenderCancellation) -> Result<(), DawEngineError> {
    if cancellation.is_cancelled() {
        Err(DawEngineError::Cancelled)
    } else {
        Ok(())
    }
}

#[derive(Debug)]
pub enum DawEngineError {
    Project(BridgeError),
    Compile(CompileError),
    Render(ReferenceRenderError),
    Audio(AudioError),
    Instrument(InstrumentError),
    UnknownRenderBus(BusId),
    UnknownRenderTrack(u64),
    UnsupportedRenderScope(RenderScope),
    Cancelled,
}

impl fmt::Display for DawEngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Project(error) => write!(formatter, "project cannot be rendered: {error}"),
            Self::Compile(error) => {
                write!(formatter, "render schedule compilation failed: {error}")
            }
            Self::Render(error) => write!(formatter, "PCM render failed: {error}"),
            Self::Audio(error) => {
                write!(formatter, "rendered transport buffer is invalid: {error}")
            }
            Self::Instrument(error) => write!(formatter, "built-in instrument is invalid: {error}"),
            Self::UnknownRenderBus(bus) => {
                write!(
                    formatter,
                    "render scope names unknown mixer bus {}",
                    bus.get()
                )
            }
            Self::UnknownRenderTrack(track) => {
                write!(
                    formatter,
                    "render scope names unknown arrangement track {track}"
                )
            }
            Self::UnsupportedRenderScope(scope) => {
                write!(
                    formatter,
                    "DAW engine cannot execute render scope {scope:?}"
                )
            }
            Self::Cancelled => write!(formatter, "DAW engine operation cancelled"),
        }
    }
}

impl Error for DawEngineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Project(error) => Some(error),
            Self::Compile(error) => Some(error),
            Self::Render(error) => Some(error),
            Self::Audio(error) => Some(error),
            Self::Instrument(error) => Some(error),
            Self::UnknownRenderBus(_)
            | Self::UnknownRenderTrack(_)
            | Self::UnsupportedRenderScope(_)
            | Self::Cancelled => None,
        }
    }
}

impl From<BridgeError> for DawEngineError {
    fn from(error: BridgeError) -> Self {
        Self::Project(error)
    }
}

impl From<CompileError> for DawEngineError {
    fn from(error: CompileError) -> Self {
        if error == CompileError::Cancelled {
            Self::Cancelled
        } else {
            Self::Compile(error)
        }
    }
}

impl From<ReferenceRenderError> for DawEngineError {
    fn from(error: ReferenceRenderError) -> Self {
        if error == ReferenceRenderError::Cancelled {
            Self::Cancelled
        } else {
            Self::Render(error)
        }
    }
}

impl From<AudioError> for DawEngineError {
    fn from(error: AudioError) -> Self {
        Self::Audio(error)
    }
}

impl From<InstrumentError> for DawEngineError {
    fn from(error: InstrumentError) -> Self {
        Self::Instrument(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use crate::arrangement::{
        ArrangementEditor, Frame, FrameRange, SourceRange, TrackId, TrackKind,
    };
    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        ContentFingerprint, DecodedAudioMetadata, ProjectRelativePath, SampleFrames,
    };
    use crate::audio::AudioFormat;
    use crate::daw_project::ProjectDomain;
    use crate::mixer::BusKind;

    fn location() -> AssetLocation {
        AssetLocation::new(
            Some(AbsolutePath::parse("/audio/hit.wav").unwrap()),
            Some(ProjectRelativePath::parse("media/hit.wav").unwrap()),
        )
        .unwrap()
    }

    fn registration(frames: u64) -> AssetRegistration {
        AssetRegistration {
            name: "hit".into(),
            location: location(),
            metadata: DecodedAudioMetadata {
                sample_rate_hz: 48_000,
                channels: 1,
                frame_count: SampleFrames(frames),
                container: Some("wav".into()),
                codec: Some("pcm_f32le".into()),
                bit_depth: Some(32),
            },
            content: ContentFingerprint::from_bytes(b"test hit"),
            provenance: AssetProvenance::new(
                1,
                AssetOrigin::Generated {
                    generator: "test".into(),
                },
                location(),
            ),
            tags: BTreeSet::new(),
            favorite: false,
        }
    }

    fn project_with_clip() -> (DawProject, assets::AssetId, TrackId, ClipId) {
        let mut project = DawProject::new("audible", 48_000, 120.0).unwrap();
        let mut registry_id = None;
        let mut track_id = None;
        let mut clip_id = None;
        project
            .transact(
                "add audible clip",
                0,
                BTreeSet::from([
                    ProjectDomain::Arrangement,
                    ProjectDomain::Assets,
                    ProjectDomain::Mixer,
                    ProjectDomain::Bindings,
                ]),
                |state| -> Result<(), String> {
                    let media = state
                        .domains
                        .assets
                        .register(registration(4))
                        .map_err(|error| error.to_string())?;
                    let alias = state
                        .bindings
                        .bind_media_asset(media)
                        .map_err(|error| error.to_string())?;
                    let mut editor =
                        ArrangementEditor::from_state(state.domains.arrangement.clone())
                            .map_err(|error| error.to_string())?;
                    let track = editor
                        .create_track("source", TrackKind::Audio)
                        .map_err(|error| error.to_string())?;
                    let clip = editor
                        .create_audio_clip(
                            track,
                            "hit",
                            FrameRange::new(Frame(2), Frame(6)).unwrap(),
                            alias,
                            SourceRange::new(0, 4).unwrap(),
                        )
                        .map_err(|error| error.to_string())?;
                    state.domains.arrangement = editor.state().clone();
                    let bus = state
                        .domains
                        .mixer
                        .add_bus(BusKind::Source, "source")
                        .map_err(|error| error.to_string())?;
                    state.bindings.mixer.tracks.insert(track, bus);
                    registry_id = Some(media);
                    track_id = Some(track);
                    clip_id = Some(clip);
                    Ok(())
                },
            )
            .unwrap();
        (
            project,
            registry_id.unwrap(),
            track_id.unwrap(),
            clip_id.unwrap(),
        )
    }

    #[test]
    fn aggregate_asset_binding_renders_exact_half_open_frames() {
        let (project, asset, _, _) = project_with_clip();
        let format = AudioFormat::new(48_000, 1).unwrap();
        let pcm = AssetPcmMap::from([(
            asset,
            PcmAsset::new(format, Arc::from([0.25, 0.5, 0.75, 1.0])).unwrap(),
        )]);
        let cancellation = RenderCancellation::new();
        let schedule = compile_daw_engine(
            &project,
            &pcm,
            RenderWindow::new(0, 8).unwrap(),
            &DawEngineConfig::default(),
            &cancellation,
        )
        .unwrap();
        let rendered = schedule.render_for_audition(&cancellation).unwrap();
        assert_eq!(rendered.origin_frame, 0);
        assert_eq!(rendered.audio.frame_count().0, 8);
        assert_eq!(
            rendered.audio.interleaved(),
            &[0.0, 0.0, 0.0, 0.0, 0.25, 0.25, 0.5, 0.5, 0.75, 0.75, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0,]
        );
    }

    #[test]
    fn schedule_owns_pcm_snapshot_and_is_not_changed_by_caller_map() {
        let (project, asset, _, _) = project_with_clip();
        let format = AudioFormat::new(48_000, 1).unwrap();
        let mut pcm = AssetPcmMap::from([(
            asset,
            PcmAsset::new(format, Arc::from([1.0, 0.0, 0.0, 0.0])).unwrap(),
        )]);
        let cancellation = RenderCancellation::new();
        let schedule = compile_daw_engine(
            &project,
            &pcm,
            RenderWindow::new(2, 6).unwrap(),
            &DawEngineConfig::default(),
            &cancellation,
        )
        .unwrap();
        pcm.clear();
        let rendered = schedule.render_for_audition(&cancellation).unwrap();
        assert_eq!(
            rendered.audio.interleaved(),
            &[1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
        );
    }

    #[test]
    fn aggregate_mixer_routing_and_bus_gain_are_audible() {
        let (mut project, asset, track, _) = project_with_clip();
        let source = project.state().bindings.mixer.tracks[&track];
        let revision = project.revisions().aggregate;
        project
            .transact(
                "route through group",
                revision,
                BTreeSet::from([ProjectDomain::Mixer]),
                |state| -> Result<(), String> {
                    let group = state
                        .domains
                        .mixer
                        .add_bus(BusKind::Group, "half gain")
                        .map_err(|error| error.to_string())?;
                    state
                        .domains
                        .mixer
                        .set_gain_db(group, -6.020_600_3)
                        .map_err(|error| error.to_string())?;
                    state
                        .domains
                        .mixer
                        .set_output(source, group)
                        .map_err(|error| error.to_string())?;
                    Ok(())
                },
            )
            .unwrap();
        let format = AudioFormat::new(48_000, 1).unwrap();
        let pcm = AssetPcmMap::from([(
            asset,
            PcmAsset::new(format, Arc::from([1.0, 0.0, 0.0, 0.0])).unwrap(),
        )]);
        let cancellation = RenderCancellation::new();
        let schedule = compile_daw_engine(
            &project,
            &pcm,
            RenderWindow::new(2, 6).unwrap(),
            &DawEngineConfig::default(),
            &cancellation,
        )
        .unwrap();
        let rendered = schedule.render_for_audition(&cancellation).unwrap();
        assert!((rendered.audio.interleaved()[0] - 0.5).abs() < 1e-6);
        assert!((rendered.audio.interleaved()[1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn one_execution_exposes_distinct_group_return_and_track_stems() {
        let (mut project, asset, track, _) = project_with_clip();
        let source = project.state().bindings.mixer.tracks[&track];
        let mut group = None;
        let mut return_bus = None;
        let revision = project.revisions().aggregate;
        project
            .transact(
                "add group and return taps",
                revision,
                BTreeSet::from([ProjectDomain::Mixer]),
                |state| -> Result<(), String> {
                    let next_group = state
                        .domains
                        .mixer
                        .add_bus(BusKind::Group, "group")
                        .map_err(|error| error.to_string())?;
                    let next_return = state
                        .domains
                        .mixer
                        .add_bus(BusKind::Return, "return")
                        .map_err(|error| error.to_string())?;
                    state
                        .domains
                        .mixer
                        .set_output(source, next_group)
                        .map_err(|error| error.to_string())?;
                    state
                        .domains
                        .mixer
                        .set_gain_db(next_group, -6.020_600_3)
                        .map_err(|error| error.to_string())?;
                    state
                        .domains
                        .mixer
                        .set_gain_db(next_return, -6.020_600_3)
                        .map_err(|error| error.to_string())?;
                    state
                        .domains
                        .mixer
                        .add_send(
                            source,
                            next_return,
                            crate::mixer::SendTap::PostFader,
                            -6.020_600_3,
                        )
                        .map_err(|error| error.to_string())?;
                    group = Some(next_group);
                    return_bus = Some(next_return);
                    Ok(())
                },
            )
            .unwrap();
        let group = group.unwrap();
        let return_bus = return_bus.unwrap();
        let pcm = AssetPcmMap::from([(
            asset,
            PcmAsset::new(
                AudioFormat::new(48_000, 1).unwrap(),
                Arc::from([1.0, 0.0, 0.0, 0.0]),
            )
            .unwrap(),
        )]);
        let cancellation = RenderCancellation::new();
        let schedule = compile_daw_engine(
            &project,
            &pcm,
            RenderWindow::new(2, 6).unwrap(),
            &DawEngineConfig::default(),
            &cancellation,
        )
        .unwrap();
        let group_scope = RenderScope::Bus {
            bus: group.get(),
            tap: BusTap::Output,
        };
        let return_scope = RenderScope::Bus {
            bus: return_bus.get(),
            tap: BusTap::Output,
        };
        let track_scope = RenderScope::Track(track.get());
        let rendered = schedule
            .render_scopes(
                RenderWindow::new(2, 6).unwrap(),
                &[
                    RenderScope::Master,
                    group_scope.clone(),
                    return_scope.clone(),
                    track_scope.clone(),
                ],
                &cancellation,
            )
            .unwrap();
        assert_eq!(
            rendered.output(&track_scope).unwrap().as_ref(),
            &[1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
        );
        assert!((rendered.output(&group_scope).unwrap()[0] - 0.5).abs() < 1e-6);
        assert!((rendered.output(&return_scope).unwrap()[0] - 0.25).abs() < 1e-6);
        assert!((rendered.output(&RenderScope::Master).unwrap()[0] - 0.75).abs() < 1e-6);
    }

    #[test]
    fn missing_pcm_and_clip_override_are_explicit_diagnostics() {
        let (mut project, asset, track, clip) = project_with_clip();
        let master = project.state().domains.mixer.master();
        let mut alternate = None;
        let revision = project.revisions().aggregate;
        project
            .transact(
                "override destination",
                revision,
                BTreeSet::from([ProjectDomain::Mixer, ProjectDomain::Bindings]),
                |state| -> Result<(), String> {
                    let bus = state
                        .domains
                        .mixer
                        .add_bus(BusKind::Group, "alternate")
                        .map_err(|error| error.to_string())?;
                    state.bindings.mixer.clip_overrides.insert(clip, bus);
                    alternate = Some(bus);
                    Ok(())
                },
            )
            .unwrap();
        let cancellation = RenderCancellation::new();
        let alternate = alternate.unwrap();
        let schedule = compile_daw_engine(
            &project,
            &AssetPcmMap::new(),
            RenderWindow::new(0, 8).unwrap(),
            &DawEngineConfig::default(),
            &cancellation,
        )
        .unwrap();
        let track_bus = project.state().bindings.mixer.tracks[&track];
        assert_ne!(track_bus, master);
        assert!(schedule
            .engine_diagnostics()
            .contains(&EngineDiagnostic::PcmNotSupplied {
                asset,
                arrangement_alias: project
                    .state()
                    .bindings
                    .assets
                    .arrangement_assets
                    .iter()
                    .find_map(|(&alias, &id)| (id == asset).then_some(alias))
                    .unwrap(),
            }));
        assert!(schedule.engine_diagnostics().contains(
            &EngineDiagnostic::ClipBusOverrideUnsupported {
                clip,
                requested: alternate,
                rendered_to: track_bus,
            }
        ));
    }

    #[test]
    fn cancellation_is_observed_before_compile_and_during_render() {
        let (project, asset, _, _) = project_with_clip();
        let format = AudioFormat::new(48_000, 1).unwrap();
        let pcm = AssetPcmMap::from([(asset, PcmAsset::new(format, Arc::from([1.0; 4])).unwrap())]);
        let cancelled = RenderCancellation::new();
        cancelled.cancel();
        assert!(matches!(
            compile_daw_engine(
                &project,
                &pcm,
                RenderWindow::new(0, 8).unwrap(),
                &DawEngineConfig::default(),
                &cancelled,
            ),
            Err(DawEngineError::Cancelled)
        ));

        let active = RenderCancellation::new();
        let schedule = compile_daw_engine(
            &project,
            &pcm,
            RenderWindow::new(0, 8).unwrap(),
            &DawEngineConfig::default(),
            &active,
        )
        .unwrap();
        active.cancel();
        assert!(matches!(
            schedule.render_for_audition(&active),
            Err(DawEngineError::Cancelled)
        ));
    }

    #[test]
    fn built_in_trigger_routing_requires_the_exact_instrument_identity() {
        let definition = BuiltInInstrumentDefinition::Subtractive(SynthParams::default());
        let event = ScheduledEvent {
            block_offset: 0,
            project_frame: crate::sequencer::ProjectFrame(12),
            kind: ScheduledKind::Trigger {
                clip: crate::sequencer::PatternClipId::from_raw(1),
                lane: crate::sequencer::StepLaneId::from_raw(2),
                target: TriggerTarget::InstrumentNote {
                    instrument: 41,
                    key: 60,
                },
                choke_group: None,
                velocity: 1.0,
                pan: 0.0,
                pitch_semitones: 0.0,
                gate_frames: 16,
                ratchet: 0,
            },
        };
        assert!(definition.consumes(41, &event));
        assert!(!definition.consumes(42, &event));
    }
}
