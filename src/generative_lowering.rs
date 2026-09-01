//! Atomic lowering from generative terms into the constructive project.
//!
//! This module turns one compiled patterned-voice term into one command
//! envelope plus an immutable runtime construction root.  Durable identities
//! are allocated only through cloned project allocators and are claimed by the
//! envelope.  Runtime-only instrument identities are content-addressed and
//! remain explicitly typed here; they are never presented as AIR identities.
//!
//! The current engine cannot execute every generator or processor in the term
//! ontology.  Such terms remain in the construction root and produce typed
//! diagnostics.  In particular, a bypassed placeholder insert is never
//! reported as an audible explanation and an unbound curve is never silently
//! flattened to a constant.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::arrangement::{
    self, Clip, ClipContent, ClipFades, Frame, FrameRange, OverlapPolicy, PatternRegion, Track,
    TrackKind,
};
use crate::automation::{
    AutomationCommand, AutomationLane, BindingMode, LaneChange, ParameterAddress, TimeDomain,
};
use crate::command::{claims_for_commands, BindingCommand, CommandEnvelope, DomainCommand};
use crate::curve_lang::{self, CurveExpr};
use crate::daw_engine::{BuiltInInstrumentDefinition, BuiltInInstrumentRoute};
use crate::daw_project::{DawProject, ProjectState};
use crate::explanation::{ExplanationEvidenceRef, ExplanationScope};
use crate::generative_ontology::{
    CompiledControl, CompiledLayer, CompiledPatternedVoiceProgram, CompiledPitch,
    CompiledProcessor, CompiledVoiceProgram, ConstructionTermRef, ControlProgramId, ControlTarget,
    ControlUnit, GeneratorTerm, NoiseColor, PatternedProgramId, TermEvidenceRef, TermOrigin,
    VoiceProgramId,
};
use crate::instruments::{Adsr, SynthParams, Waveform};
use crate::mixer::{BusId, BusKind, MixerCommand, PluginDescriptor, ProcessorId};
use crate::ontology::{EffectKind, FilterShape, OscillatorShape};
use crate::pattern_lang::{self, PatternEvalDiagnostic};
use crate::sample_kit::{
    KitId, PadId, SampleKit, SampleKitPut, SamplePad, SampleRouteIntent, SampleTargetRef,
    SampleZone, ZoneId,
};
use crate::sample_material::{SampleMaterialProvenance, SourceMaterialRef};
use crate::sequencer::{
    self, BeatDuration, BeatTime, PatternClip, PatternContent, PatternDefinition, PatternOrigin,
    StepPattern, TriggerTarget,
};

const AUTOMATION_RESOLUTION: BeatDuration = BeatDuration(60);

/// Stable, runtime-only identity for a compiled voice.  It is derived from the
/// complete voice-program digest and collision-checked against existing
/// sequencer targets before publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConstructionVoiceId(pub u64);

#[derive(Clone, Debug, Default)]
pub struct GenerativeLoweringOptions {
    /// Musical position for both linked placements.
    pub start: BeatTime,
    /// `None` places exactly one generator cycle.
    pub placement_length: Option<BeatDuration>,
    /// Existing parameter addresses authorized to receive a generated curve.
    /// The automation graph must already contain a descriptor for each value.
    pub control_bindings: BTreeMap<ControlProgramId, ParameterAddress>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoweringSeverity {
    Notice,
    Incomplete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GenerativeLoweringDiagnostic {
    PatternTiming(PatternEvalDiagnosticKind),
    MultiLayerVoice {
        voice: VoiceProgramId,
        layers: usize,
    },
    UnsupportedGenerator {
        voice: VoiceProgramId,
        layer: usize,
        generator: &'static str,
    },
    UnsupportedProcessor {
        voice: VoiceProgramId,
        layer: usize,
        processor: &'static str,
    },
    DeferredPluginProcessor {
        voice: VoiceProgramId,
        layer: usize,
        processor: ProcessorId,
    },
    UnboundControl {
        voice: VoiceProgramId,
        control: ControlProgramId,
        target: ControlTarget,
    },
    MissingAutomationDescriptor {
        control: ControlProgramId,
        address: ParameterAddress,
    },
    RuntimeIdentityCollision {
        voice: VoiceProgramId,
        attempted: u64,
        allocated: u64,
    },
}

impl GenerativeLoweringDiagnostic {
    pub const fn severity(&self) -> LoweringSeverity {
        match self {
            Self::PatternTiming(_) | Self::RuntimeIdentityCollision { .. } => {
                LoweringSeverity::Notice
            }
            Self::MultiLayerVoice { .. }
            | Self::UnsupportedGenerator { .. }
            | Self::UnsupportedProcessor { .. }
            | Self::DeferredPluginProcessor { .. }
            | Self::UnboundControl { .. }
            | Self::MissingAutomationDescriptor { .. } => LoweringSeverity::Incomplete,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatternEvalDiagnosticKind {
    RoundedToTick,
    RatchetSpacingTruncated,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermProvenanceReceipt {
    pub term: ConstructionTermRef,
    pub origin: TermOrigin,
    pub evidence: Vec<TermEvidenceRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutomationBindingReceipt {
    pub voice: VoiceProgramId,
    pub control: ControlProgramId,
    pub target: ControlTarget,
    pub address: ParameterAddress,
    pub lane: crate::automation::AutomationLaneId,
}

#[derive(Clone, Debug)]
pub enum RuntimeLayerPlan {
    /// Authoritative sampler data is resolved from this exact kit target by
    /// the existing sampler runtime.
    Sampler {
        material: SourceMaterialRef,
        target: SampleTargetRef,
        alias: sequencer::SampleAssetId,
    },
    BuiltInSynth(SynthParams),
    Unsupported(GeneratorTerm),
}

#[derive(Clone, Debug)]
pub struct RuntimeVoiceRoot {
    pub id: ConstructionVoiceId,
    pub term: VoiceProgramId,
    pub trigger: TriggerTarget,
    pub bus: BusId,
    pub layers: Vec<RuntimeLayerPlan>,
}

/// Exact constructive address that an explanation definition can adopt after
/// the envelope commits.  `PatternClip` is already understood by the normal
/// explanation compiler and therefore by `ComparisonRuntime`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComparisonConstructionRoot {
    pub scope: ExplanationScope,
    pub project_span: crate::aspect::FrameSpan,
    pub evidence: Vec<ExplanationEvidenceRef>,
}

#[derive(Clone, Debug)]
pub struct GenerativeConstructionRoot {
    pub term: PatternedProgramId,
    pub pattern: sequencer::PatternId,
    pub pattern_clip: sequencer::PatternClipId,
    pub arrangement_track: arrangement::TrackId,
    pub arrangement_clip: arrangement::ClipId,
    pub output_bus: BusId,
    pub voices: BTreeMap<String, RuntimeVoiceRoot>,
    /// Immediately installable built-in synth routes. Sampler routes remain
    /// authoritative in the sample-kit bindings and are built by the sampler
    /// runtime from project state.
    pub built_in_routes: BTreeMap<u64, BuiltInInstrumentRoute>,
    pub automation: Vec<AutomationBindingReceipt>,
    pub provenance: Vec<TermProvenanceReceipt>,
    pub comparison: ComparisonConstructionRoot,
    pub diagnostics: Vec<GenerativeLoweringDiagnostic>,
}

impl GenerativeConstructionRoot {
    /// A comparison may use this root as a complete explanatory sum only when
    /// no known term has been omitted or bypassed.
    pub fn is_exactly_renderable(&self) -> bool {
        self.diagnostics
            .iter()
            .all(|item| item.severity() == LoweringSeverity::Notice)
    }
}

/// Prepared but unpublished atomic construction.  Dropping this value changes
/// no project state.
#[derive(Clone, Debug)]
pub struct PreparedGenerativeLowering {
    pub envelope: CommandEnvelope,
    pub construction: GenerativeConstructionRoot,
}

#[derive(Clone, Debug)]
pub struct GenerativeLoweringReceipt {
    pub project_revision: u64,
    pub construction: GenerativeConstructionRoot,
}

impl PreparedGenerativeLowering {
    pub fn commit(
        self,
        project: &mut DawProject,
    ) -> Result<GenerativeLoweringReceipt, GenerativeLoweringError> {
        let applied = self.envelope.apply(project)?;
        Ok(GenerativeLoweringReceipt {
            project_revision: applied.revisions.aggregate,
            construction: self.construction,
        })
    }
}

/// Lower a compiled term without mutating the project. All durable edits are
/// contained in exactly one envelope.
pub fn prepare_patterned_voice_lowering(
    project: &DawProject,
    program: &CompiledPatternedVoiceProgram,
    options: &GenerativeLoweringOptions,
) -> Result<PreparedGenerativeLowering, GenerativeLoweringError> {
    let length = options.placement_length.unwrap_or(program.cycle);
    if options.start.0 < 0 || length.0 == 0 || length.0 > i64::MAX as u64 {
        return Err(GenerativeLoweringError::InvalidPlacement);
    }
    let end_tick = options
        .start
        .0
        .checked_add(length.0 as i64)
        .ok_or(GenerativeLoweringError::InvalidPlacement)?;
    let tempo = project.state().domains.sequencer.tempo_map();
    let start_frame = tempo.beat_to_frame(options.start).0;
    let end_frame = tempo.beat_to_frame(BeatTime(end_tick)).0;
    let placement = FrameRange::new(Frame(start_frame), Frame(end_frame))?;

    let mut commands = Vec::new();
    let mut diagnostics = Vec::new();
    let mut voice_buses = BTreeMap::new();
    let mut plugin_receipts = Vec::new();
    let mixer_before = &project.state().domains.mixer;
    let output_label = format!("{} construction", short_label(&program.canonical_pattern));
    let mixer = MixerCommand::build("Route generative construction", mixer_before, |graph| {
        let output = graph.add_bus(BusKind::Group, output_label.clone())?;
        for (binding, voice) in &program.voices {
            let bus = graph.add_bus(BusKind::Component, format!("{binding} · {}", voice.label))?;
            graph.set_output(bus, output)?;
            voice_buses.insert(binding.clone(), bus);
            for (layer_index, layer) in voice.layers.iter().enumerate() {
                for processor in &layer.processors {
                    if processor_needs_plugin(processor) {
                        let descriptor = plugin_descriptor(voice.id, layer_index, processor);
                        let id = graph.insert_processor(bus, None, descriptor, 0)?;
                        // It is a durable, ordered placeholder, not fake DSP.
                        graph.set_insert_bypassed(id, true)?;
                        plugin_receipts.push((voice.id, layer_index, id));
                    }
                }
            }
        }
        Ok(())
    })?;
    let output_bus = mixer
        .after()
        .buses()
        .find(|bus| bus.name() == output_label)
        .map(|bus| bus.id())
        .ok_or_else(|| {
            GenerativeLoweringError::Internal("missing constructed output bus".into())
        })?;
    commands.push(DomainCommand::Mixer(mixer));
    diagnostics.extend(plugin_receipts.iter().map(|(voice, layer, processor)| {
        GenerativeLoweringDiagnostic::DeferredPluginProcessor {
            voice: *voice,
            layer: *layer,
            processor: *processor,
        }
    }));

    let mut bindings = project.state().bindings.clone();
    let mut kit_allocators = project.state().domains.sample_kits.clone();
    let mut occupied_runtime_ids = existing_instrument_targets(project.state());
    let mut trigger_bindings = BTreeMap::new();
    let mut roots = BTreeMap::new();
    let mut built_in_routes = BTreeMap::new();
    let mut provenance = Vec::new();
    for (binding, voice) in &program.voices {
        let bus = voice_buses[binding];
        let identity =
            allocate_runtime_identity(voice.id, &mut occupied_runtime_ids, &mut diagnostics);
        let mut layer_roots = Vec::new();
        let trigger = if voice.layers.len() == 1 {
            lower_single_layer_trigger(
                binding,
                voice,
                &voice.layers[0],
                identity,
                bus,
                project.state(),
                &mut kit_allocators,
                &mut bindings,
                &mut commands,
                &mut built_in_routes,
                &mut layer_roots,
                &mut diagnostics,
            )?
        } else {
            diagnostics.push(GenerativeLoweringDiagnostic::MultiLayerVoice {
                voice: voice.id,
                layers: voice.layers.len(),
            });
            for (index, layer) in voice.layers.iter().enumerate() {
                layer_roots.push(runtime_layer_without_trigger(
                    voice.id,
                    index,
                    layer,
                    &mut diagnostics,
                ));
            }
            TriggerTarget::InstrumentNote {
                instrument: identity.0,
                key: pitch_key(&voice.layers[0].pitch).0,
            }
        };
        trigger_bindings.insert(binding.clone(), trigger.clone());
        roots.insert(
            binding.clone(),
            RuntimeVoiceRoot {
                id: identity,
                term: voice.id,
                trigger,
                bus,
                layers: layer_roots,
            },
        );
        provenance.push(provenance_receipt(voice));
    }

    let term = pattern_lang::parse(&program.canonical_pattern)
        .map_err(|error| GenerativeLoweringError::Pattern(error.to_string()))?;
    let evaluated = pattern_lang::eval_steps(
        &term,
        &pattern_lang::EvalContext {
            bindings: &trigger_bindings,
            cycle: program.cycle,
            seed: program.seed,
            cycle_index: program.initial_cycle_index,
        },
    )?;
    diagnostics.extend(evaluated.diagnostics.iter().map(|item| {
        GenerativeLoweringDiagnostic::PatternTiming(match item {
            PatternEvalDiagnostic::RoundedToTick { .. } => PatternEvalDiagnosticKind::RoundedToTick,
            PatternEvalDiagnostic::RatchetSpacingTruncated { .. } => {
                PatternEvalDiagnosticKind::RatchetSpacingTruncated
            }
        })
    }));

    let mut sequencer_alloc = project.state().domains.sequencer.clone();
    let pattern_id = sequencer_alloc.allocate_pattern_id();
    let pattern = PatternDefinition {
        id: pattern_id,
        name: format!("Generated · {}", short_label(&program.canonical_pattern)),
        length: program.cycle,
        content: PatternContent::Steps(evaluated.pattern),
        origin: PatternOrigin::Expression {
            source: program.canonical_pattern.clone(),
            term_hash: program.pattern_hash,
            bindings_hash: pattern_lang::bindings_hash(&trigger_bindings),
            bindings: trigger_bindings,
            diverged: false,
        },
        revision: 0,
    };
    commands.push(DomainCommand::Sequencer(
        sequencer::SequencerCommand::PutPattern {
            before: None,
            after: Some(pattern),
        },
    ));
    let arrangement_pattern = bindings.bind_pattern_definition(pattern_id)?;
    commands.push(DomainCommand::Bindings(
        BindingCommand::PutPatternDefinitionAlias {
            alias: arrangement_pattern,
            before: None,
            after: Some(pattern_id),
        },
    ));

    let pattern_clip_id = sequencer_alloc.allocate_clip_id();
    let pattern_clip = PatternClip {
        id: pattern_clip_id,
        pattern: pattern_id,
        start: options.start,
        length,
        pattern_offset: BeatTime::ZERO,
        looped: length.0 > program.cycle.0,
        transpose_semitones: 0.0,
        gain: 1.0,
        muted: false,
    };
    commands.push(DomainCommand::Sequencer(
        sequencer::SequencerCommand::PutClip {
            before: None,
            after: Some(pattern_clip),
        },
    ));

    let arrangement_state = &project.state().domains.arrangement;
    let track_id = arrangement::TrackId::from_raw(arrangement_state.next_track_id);
    let clip_id = arrangement::ClipId::from_raw(arrangement_state.next_clip_id);
    commands.push(DomainCommand::Arrangement(
        arrangement::ArrangementOperation::PutTrack {
            before: None,
            after: Some(Track {
                id: track_id,
                name: "Generative construction".into(),
                kind: TrackKind::Pattern,
                overlap: OverlapPolicy::Mix,
                clip_ids: Vec::new(),
                muted: false,
                solo: false,
                locked: false,
                gain_db: 0.0,
                pan: 0.0,
            }),
        },
    ));
    commands.push(DomainCommand::Bindings(BindingCommand::PutTrackBus {
        track: track_id,
        before: None,
        after: Some(output_bus),
    }));
    commands.push(DomainCommand::Arrangement(
        arrangement::ArrangementOperation::PutClip {
            before: None,
            after: Some(Clip {
                id: clip_id,
                track_id,
                name: "Generated pattern".into(),
                placement,
                content: ClipContent::Pattern(PatternRegion {
                    pattern: arrangement_pattern,
                    content_offset_frames: 0,
                    looped: length.0 > program.cycle.0,
                }),
                fades: ClipFades::default(),
                gain_db: 0.0,
                muted: false,
                locked: false,
            }),
        },
    ));
    commands.push(DomainCommand::Bindings(
        BindingCommand::PutPatternPlacement {
            clip: clip_id,
            before: None,
            after: Some(pattern_clip_id),
        },
    ));

    let automation = lower_automation(
        project.state(),
        program,
        options,
        &mut commands,
        &mut diagnostics,
    )?;
    let evidence = provenance
        .iter()
        .flat_map(|receipt| receipt.evidence.iter())
        .filter_map(explanation_evidence)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let project_span = crate::aspect::FrameSpan::new(start_frame, end_frame)
        .ok_or(GenerativeLoweringError::InvalidPlacement)?;
    let envelope = CommandEnvelope {
        label: format!(
            "Lower generative pattern {}",
            short_label(&program.canonical_pattern)
        ),
        base_revision: project.revisions().aggregate,
        coalesce: None,
        id_claims: claims_for_commands(&commands),
        commands,
    };
    // Full command preflight, including cross-domain validation, without
    // publishing a revision.
    let mut probe = project.clone();
    envelope.clone().apply(&mut probe)?;

    Ok(PreparedGenerativeLowering {
        envelope,
        construction: GenerativeConstructionRoot {
            term: program.id,
            pattern: pattern_id,
            pattern_clip: pattern_clip_id,
            arrangement_track: track_id,
            arrangement_clip: clip_id,
            output_bus,
            voices: roots,
            built_in_routes,
            automation,
            provenance,
            comparison: ComparisonConstructionRoot {
                scope: ExplanationScope::PatternClip(pattern_clip_id),
                project_span,
                evidence,
            },
            diagnostics,
        },
    })
}

#[allow(clippy::too_many_arguments)]
fn lower_single_layer_trigger(
    binding: &str,
    voice: &CompiledVoiceProgram,
    layer: &CompiledLayer,
    identity: ConstructionVoiceId,
    bus: BusId,
    state: &ProjectState,
    kit_allocators: &mut crate::sample_kit::SampleKitLibrary,
    bindings: &mut crate::daw_project::ProjectBindings,
    commands: &mut Vec<DomainCommand>,
    routes: &mut BTreeMap<u64, BuiltInInstrumentRoute>,
    roots: &mut Vec<RuntimeLayerPlan>,
    diagnostics: &mut Vec<GenerativeLoweringDiagnostic>,
) -> Result<TriggerTarget, GenerativeLoweringError> {
    match layer.generator {
        GeneratorTerm::Material(material) => {
            if state.domains.assets.get(material.asset_id()).is_none() {
                return Err(GenerativeLoweringError::MissingMaterial(
                    material.asset_id(),
                ));
            }
            let kit_id = kit_allocators.allocate_kit_id()?;
            let pad_id = kit_allocators.allocate_pad_id()?;
            let zone_id = kit_allocators.allocate_zone_id()?;
            let mut kit = SampleKit::new(
                kit_id,
                format!("{binding} · {}", voice.label),
                SampleRouteIntent::new(bus)?,
            );
            let mut pad = SamplePad::new(pad_id, binding);
            pad.zone_order.push(zone_id);
            let mut zone = SampleZone::new(zone_id, pad_id, material);
            zone.provenance = match material {
                SourceMaterialRef::Asset(_) => SampleMaterialProvenance::ExistingAsset,
                SourceMaterialRef::VirtualSlice(_) => SampleMaterialProvenance::ManualSelection,
            };
            let (_, cents) = pitch_key(&layer.pitch);
            zone.tuning_cents = cents;
            kit.pad_order.push(pad_id);
            kit.pads.insert(pad_id, pad);
            kit.zones.insert(zone_id, zone);
            commands.push(DomainCommand::SampleKits(SampleKitPut {
                before: None,
                after: Some(kit),
            }));
            let target = SampleTargetRef {
                kit: kit_id,
                pad: pad_id,
                zone: zone_id,
            };
            let alias = bindings.bind_sample_target(target)?;
            commands.push(DomainCommand::Bindings(
                BindingCommand::PutSampleTargetAlias {
                    alias,
                    before: None,
                    after: Some(target),
                },
            ));
            roots.push(RuntimeLayerPlan::Sampler {
                material,
                target,
                alias,
            });
            Ok(TriggerTarget::Sample(alias))
        }
        GeneratorTerm::Oscillator(_) => {
            if let Some(params) = synth_params(voice.id, 0, layer, diagnostics) {
                routes.insert(
                    identity.0,
                    BuiltInInstrumentRoute {
                        definition: BuiltInInstrumentDefinition::Subtractive(params.clone()),
                        bus,
                    },
                );
                roots.push(RuntimeLayerPlan::BuiltInSynth(params));
            } else {
                roots.push(RuntimeLayerPlan::Unsupported(layer.generator.clone()));
            }
            let (key, cents) = pitch_key(&layer.pitch);
            if cents.abs() > 0.001 {
                diagnostics.push(GenerativeLoweringDiagnostic::UnsupportedProcessor {
                    voice: voice.id,
                    layer: 0,
                    processor: "fixed cents require per-event pitch lowering",
                });
            }
            Ok(TriggerTarget::InstrumentNote {
                instrument: identity.0,
                key,
            })
        }
        ref generator => {
            diagnostics.push(GenerativeLoweringDiagnostic::UnsupportedGenerator {
                voice: voice.id,
                layer: 0,
                generator: generator_name(generator),
            });
            roots.push(RuntimeLayerPlan::Unsupported(generator.clone()));
            Ok(TriggerTarget::InstrumentNote {
                instrument: identity.0,
                key: pitch_key(&layer.pitch).0,
            })
        }
    }
}

fn runtime_layer_without_trigger(
    voice: VoiceProgramId,
    layer_index: usize,
    layer: &CompiledLayer,
    diagnostics: &mut Vec<GenerativeLoweringDiagnostic>,
) -> RuntimeLayerPlan {
    match layer.generator {
        GeneratorTerm::Oscillator(_) => synth_params(voice, layer_index, layer, diagnostics)
            .map(RuntimeLayerPlan::BuiltInSynth)
            .unwrap_or_else(|| RuntimeLayerPlan::Unsupported(layer.generator.clone())),
        GeneratorTerm::Material(material) => {
            diagnostics.push(GenerativeLoweringDiagnostic::UnsupportedGenerator {
                voice,
                layer: layer_index,
                generator: "material layer inside a multi-layer voice",
            });
            RuntimeLayerPlan::Unsupported(GeneratorTerm::Material(material))
        }
        ref generator => {
            diagnostics.push(GenerativeLoweringDiagnostic::UnsupportedGenerator {
                voice,
                layer: layer_index,
                generator: generator_name(generator),
            });
            RuntimeLayerPlan::Unsupported(generator.clone())
        }
    }
}

fn synth_params(
    voice: VoiceProgramId,
    layer_index: usize,
    layer: &CompiledLayer,
    diagnostics: &mut Vec<GenerativeLoweringDiagnostic>,
) -> Option<SynthParams> {
    let GeneratorTerm::Oscillator(shape) = layer.generator else {
        return None;
    };
    let waveform = match shape {
        OscillatorShape::Sine => Waveform::Sine,
        OscillatorShape::Triangle => Waveform::Triangle,
        OscillatorShape::SawUp => Waveform::Saw,
        OscillatorShape::Square => Waveform::Square,
        OscillatorShape::SawDown | OscillatorShape::SampleAndHold => {
            diagnostics.push(GenerativeLoweringDiagnostic::UnsupportedGenerator {
                voice,
                layer: layer_index,
                generator: "oscillator shape unsupported by built-in synth",
            });
            return None;
        }
    };
    let mut params = SynthParams::default();
    params.waveform = waveform;
    if let Some(value) = constant(&layer.gain) {
        params.gain_db = match layer.gain.unit {
            ControlUnit::Decibels => value as f32,
            ControlUnit::Linear => (20.0 * value.max(1.0e-6).log10()) as f32,
            _ => params.gain_db,
        };
    }
    for processor in &layer.processors {
        match processor {
            CompiledProcessor::Envelope(control) => {
                if let CurveExpr::Env {
                    attack,
                    decay,
                    sustain,
                    release,
                } = control.expression
                {
                    params.envelope = Adsr {
                        attack_seconds: attack as f32,
                        decay_seconds: decay as f32,
                        sustain: sustain as f32,
                        release_seconds: release as f32,
                    };
                } else {
                    diagnostics.push(GenerativeLoweringDiagnostic::UnsupportedProcessor {
                        voice,
                        layer: layer_index,
                        processor: "non-ADSR envelope",
                    });
                }
            }
            CompiledProcessor::Gain(control) => {
                if let Some(value) = constant(control) {
                    params.gain_db += match control.unit {
                        ControlUnit::Decibels => value as f32,
                        ControlUnit::Linear => (20.0 * value.max(1.0e-6).log10()) as f32,
                        _ => 0.0,
                    };
                }
            }
            CompiledProcessor::Filter {
                shape: FilterShape::LowPass,
                cutoff,
                resonance,
                gain: None,
            } => {
                if let Some(value) = constant(cutoff) {
                    params.filter.cutoff_hz = value as f32;
                }
                if let Some(value) = resonance.as_ref().and_then(constant) {
                    params.filter.resonance = value.clamp(0.0, 1.0) as f32;
                }
            }
            CompiledProcessor::Spatial { pan, width: None } => {
                if let Some(value) = pan.as_ref().and_then(constant) {
                    params.pan = value.clamp(-1.0, 1.0) as f32;
                }
            }
            CompiledProcessor::Effect { .. }
            | CompiledProcessor::Filter { .. }
            | CompiledProcessor::Spatial { .. } => {}
        }
    }
    Some(params)
}

fn lower_automation(
    state: &ProjectState,
    program: &CompiledPatternedVoiceProgram,
    options: &GenerativeLoweringOptions,
    commands: &mut Vec<DomainCommand>,
    diagnostics: &mut Vec<GenerativeLoweringDiagnostic>,
) -> Result<Vec<AutomationBindingReceipt>, GenerativeLoweringError> {
    let descriptors = state
        .domains
        .automation
        .descriptors()
        .map(|descriptor| descriptor.address.clone())
        .collect::<BTreeSet<_>>();
    let mut graph = state.domains.automation.clone();
    let mut changes = Vec::new();
    let mut receipts = Vec::new();
    let mut seen = BTreeSet::new();
    for voice in program.voices.values() {
        for (control, target) in controls(voice) {
            if !seen.insert(control.id) || matches!(control.expression, CurveExpr::Const(_)) {
                continue;
            }
            let Some(address) = options.control_bindings.get(&control.id).cloned() else {
                diagnostics.push(GenerativeLoweringDiagnostic::UnboundControl {
                    voice: voice.id,
                    control: control.id,
                    target,
                });
                continue;
            };
            if !descriptors.contains(&address) {
                diagnostics.push(GenerativeLoweringDiagnostic::MissingAutomationDescriptor {
                    control: control.id,
                    address,
                });
                continue;
            }
            let lane_id = graph.create_lane(
                format!("{} · {target:?}", voice.label),
                address.clone(),
                TimeDomain::Beats,
            )?;
            let mut lane = AutomationLane::new(
                lane_id,
                format!("{} · {target:?}", voice.label),
                address.clone(),
                TimeDomain::Beats,
            );
            lane.binding = BindingMode::Replace;
            let points = curve_lang::compile_curve(
                &control.expression,
                (
                    options.start,
                    BeatTime(options.start.0 + program.cycle.0 as i64),
                ),
                AUTOMATION_RESOLUTION,
            )?;
            for mut point in points {
                point.id = graph.allocate_point_id()?;
                lane.insert_point(point)?;
            }
            changes.push(LaneChange {
                before: None,
                after: Some(lane),
            });
            receipts.push(AutomationBindingReceipt {
                voice: voice.id,
                control: control.id,
                target,
                address,
                lane: lane_id,
            });
        }
    }
    if !changes.is_empty() {
        commands.push(DomainCommand::Automation(AutomationCommand {
            label: "Lower generative controls".into(),
            parameters: Vec::new(),
            changes,
        }));
    }
    Ok(receipts)
}

fn controls(voice: &CompiledVoiceProgram) -> Vec<(&CompiledControl, ControlTarget)> {
    let mut result = Vec::new();
    for layer in &voice.layers {
        result.push((&layer.gain, ControlTarget::LayerGain));
        if let CompiledPitch::Curve(control) = &layer.pitch {
            result.push((control, ControlTarget::Pitch));
        }
        for processor in &layer.processors {
            match processor {
                CompiledProcessor::Envelope(control) => {
                    result.push((control, ControlTarget::Envelope));
                }
                CompiledProcessor::Gain(control) => {
                    result.push((control, ControlTarget::LayerGain));
                }
                CompiledProcessor::Filter {
                    cutoff,
                    resonance,
                    gain,
                    ..
                } => {
                    result.push((cutoff, ControlTarget::FilterCutoff));
                    if let Some(control) = resonance {
                        result.push((control, ControlTarget::FilterResonance));
                    }
                    if let Some(control) = gain {
                        result.push((control, ControlTarget::FilterGain));
                    }
                }
                CompiledProcessor::Effect { wet, .. } => {
                    result.push((wet, ControlTarget::EffectWet));
                }
                CompiledProcessor::Spatial { pan, width } => {
                    if let Some(control) = pan {
                        result.push((control, ControlTarget::Pan));
                    }
                    if let Some(control) = width {
                        result.push((control, ControlTarget::Width));
                    }
                }
            }
        }
        result.extend(
            layer
                .modulation
                .iter()
                .map(|binding| (&binding.source, binding.target)),
        );
    }
    result
}

fn provenance_receipt(voice: &CompiledVoiceProgram) -> TermProvenanceReceipt {
    let evidence = match &voice.origin {
        TermOrigin::Authored { .. } => Vec::new(),
        TermOrigin::Inferred { evidence, .. } => evidence.clone(),
    };
    TermProvenanceReceipt {
        term: ConstructionTermRef::Voice(voice.id),
        origin: voice.origin.clone(),
        evidence,
    }
}

fn explanation_evidence(reference: &TermEvidenceRef) -> Option<ExplanationEvidenceRef> {
    match reference {
        TermEvidenceRef::Air(id) => Some(ExplanationEvidenceRef::Air(*id)),
        TermEvidenceRef::Artifact(id) => Some(ExplanationEvidenceRef::Artifact(*id)),
        TermEvidenceRef::Reconstruction { artifact, evidence } => {
            Some(ExplanationEvidenceRef::Reconstruction {
                artifact: *artifact,
                evidence: *evidence,
            })
        }
        TermEvidenceRef::NativeLocator { .. } => None,
    }
}

fn existing_instrument_targets(state: &ProjectState) -> BTreeSet<u64> {
    state
        .domains
        .sequencer
        .patterns()
        .patterns()
        .flat_map(|pattern| match &pattern.content {
            PatternContent::Steps(pattern) => pattern
                .lanes
                .values()
                .filter_map(|lane| match lane.target {
                    TriggerTarget::InstrumentNote { instrument, .. } => Some(instrument),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            PatternContent::Notes(_) => Vec::new(),
        })
        .collect()
}

fn allocate_runtime_identity(
    voice: VoiceProgramId,
    occupied: &mut BTreeSet<u64>,
    diagnostics: &mut Vec<GenerativeLoweringDiagnostic>,
) -> ConstructionVoiceId {
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&voice.0.bytes[..8]);
    let attempted = u64::from_le_bytes(bytes).max(1);
    let mut candidate = attempted;
    while !occupied.insert(candidate) {
        candidate = candidate.wrapping_add(1).max(1);
    }
    if candidate != attempted {
        diagnostics.push(GenerativeLoweringDiagnostic::RuntimeIdentityCollision {
            voice,
            attempted,
            allocated: candidate,
        });
    }
    ConstructionVoiceId(candidate)
}

fn pitch_key(pitch: &CompiledPitch) -> (u8, f32) {
    match pitch {
        CompiledPitch::Midi { key, cents } => (*key, *cents as f32),
        CompiledPitch::FixedHz(hz) => {
            let midi = 69.0 + 12.0 * (hz / 440.0).log2();
            let key = midi.round().clamp(0.0, 127.0) as u8;
            (key, ((midi - f64::from(key)) * 100.0) as f32)
        }
        CompiledPitch::Unpitched | CompiledPitch::Curve(_) => (60, 0.0),
    }
}

fn constant(control: &CompiledControl) -> Option<f64> {
    match control.expression {
        CurveExpr::Const(value) => Some(value),
        _ => None,
    }
}

fn processor_needs_plugin(processor: &CompiledProcessor) -> bool {
    matches!(
        processor,
        CompiledProcessor::Effect { .. }
            | CompiledProcessor::Filter {
                shape: FilterShape::HighPass
                    | FilterShape::BandPass
                    | FilterShape::Notch
                    | FilterShape::Peak
                    | FilterShape::LowShelf
                    | FilterShape::HighShelf,
                ..
            }
    )
}

fn plugin_descriptor(
    voice: VoiceProgramId,
    layer: usize,
    processor: &CompiledProcessor,
) -> PluginDescriptor {
    let kind = match processor {
        CompiledProcessor::Effect { kind, .. } => match kind {
            EffectKind::Delay => "delay",
            EffectKind::Reverberation => "reverb",
            EffectKind::Convolution => "convolution",
            EffectKind::Diffusion => "diffusion",
            EffectKind::Resonator => "resonator",
            EffectKind::Distortion => "distortion",
            EffectKind::Dynamics => "dynamics",
            EffectKind::Custom => "custom-effect",
        },
        CompiledProcessor::Filter { .. } => "filter",
        _ => "processor",
    };
    let digest = voice
        .0
        .bytes
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    PluginDescriptor::new(
        "audec-term-v1",
        format!("{kind}:{digest}:{layer}"),
        format!("Generative {kind}"),
    )
}

fn generator_name(generator: &GeneratorTerm) -> &'static str {
    match generator {
        GeneratorTerm::Material(_) => "material",
        GeneratorTerm::Oscillator(_) => "oscillator",
        GeneratorTerm::Noise(NoiseColor::White) => "white noise",
        GeneratorTerm::Noise(NoiseColor::Pink) => "pink noise",
        GeneratorTerm::Noise(NoiseColor::Brown) => "brown noise",
        GeneratorTerm::AudioClaim(_) => "immutable audio claim",
        GeneratorTerm::Preset(_) => "preset artifact",
    }
}

fn short_label(source: &str) -> String {
    let mut value = source.chars().take(36).collect::<String>();
    if source.chars().count() > 36 {
        value.push('…');
    }
    value
}

#[derive(Debug)]
pub enum GenerativeLoweringError {
    InvalidPlacement,
    MissingMaterial(crate::assets::AssetId),
    Pattern(String),
    Internal(String),
    Domain(String),
}

impl fmt::Display for GenerativeLoweringError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPlacement => formatter.write_str("invalid generative placement"),
            Self::MissingMaterial(id) => {
                write!(formatter, "missing source material asset {}", id.0)
            }
            Self::Pattern(message) => write!(formatter, "pattern lowering failed: {message}"),
            Self::Internal(message) => {
                write!(formatter, "generative lowering invariant: {message}")
            }
            Self::Domain(message) => write!(formatter, "generative lowering failed: {message}"),
        }
    }
}

impl Error for GenerativeLoweringError {}

macro_rules! domain_error {
    ($type:ty) => {
        impl From<$type> for GenerativeLoweringError {
            fn from(error: $type) -> Self {
                Self::Domain(error.to_string())
            }
        }
    };
}

domain_error!(arrangement::ArrangementError);
domain_error!(crate::automation::AutomationError);
domain_error!(crate::command::EnvelopeError);
domain_error!(crate::curve_lang::CurveError);
domain_error!(crate::daw_project::BridgeError);
domain_error!(crate::mixer::MixerError);
domain_error!(crate::pattern_lang::PatternEvalError);
domain_error!(crate::sample_kit::SampleKitError);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curve_lang::LfoShape;
    use crate::generative_ontology::{
        compile_patterned_voices, ControlTerm, GeneratorTerm, NoCurveEvidence,
        PatternedVoiceProgram, PitchTerm, ProcessorTerm, VoiceLayer, VoiceProgram,
    };

    fn control(unit: ControlUnit, expression: CurveExpr) -> ControlTerm {
        ControlTerm { unit, expression }
    }

    fn synth_voice(with_effect: bool) -> VoiceProgram {
        let mut processors = vec![ProcessorTerm::Envelope(control(
            ControlUnit::Normalized,
            CurveExpr::Env {
                attack: 0.01,
                decay: 0.1,
                sustain: 0.7,
                release: 0.2,
            },
        ))];
        if with_effect {
            processors.push(ProcessorTerm::Effect {
                kind: EffectKind::Delay,
                wet: control(ControlUnit::Normalized, CurveExpr::Const(0.3)),
                tail: crate::ontology::TailExtent::FiniteFrames(4_800),
            });
        }
        VoiceProgram::authored(
            "content addressed saw",
            vec![VoiceLayer {
                generator: GeneratorTerm::Oscillator(OscillatorShape::SawUp),
                pitch: PitchTerm::Midi {
                    key: 48,
                    cents: 0.0,
                },
                gain: control(
                    ControlUnit::Linear,
                    CurveExpr::Lfo {
                        shape: LfoShape::Sine,
                        rate_hz: 1.0,
                        depth: 0.25,
                        phase: 0.0,
                    },
                ),
                processors,
                modulation: Vec::new(),
            }],
        )
    }

    fn compiled(with_effect: bool) -> CompiledPatternedVoiceProgram {
        compile_patterned_voices(
            &PatternedVoiceProgram {
                pattern_source: "bass ~ bass bass".into(),
                cycle: BeatDuration(4 * sequencer::PPQ as u64),
                seed: 9,
                initial_cycle_index: 3,
                voices: BTreeMap::from([("bass".into(), synth_voice(with_effect))]),
            },
            &NoCurveEvidence,
        )
        .unwrap()
    }

    #[test]
    fn one_envelope_publishes_a_playable_pattern_and_comparison_root() {
        let mut project = DawProject::new("lowering", 48_000, 120.0).unwrap();
        let prepared = prepare_patterned_voice_lowering(
            &project,
            &compiled(false),
            &GenerativeLoweringOptions::default(),
        )
        .unwrap();
        assert!(prepared.envelope.commands.len() >= 8);
        assert_eq!(
            prepared.construction.comparison.scope,
            ExplanationScope::PatternClip(prepared.construction.pattern_clip)
        );
        assert_eq!(prepared.construction.built_in_routes.len(), 1);
        let receipt = prepared.commit(&mut project).unwrap();
        assert_eq!(receipt.project_revision, 1);
        assert!(project
            .state()
            .domains
            .sequencer
            .patterns()
            .get(receipt.construction.pattern)
            .is_some());
        assert!(project
            .state()
            .domains
            .arrangement
            .clip(receipt.construction.arrangement_clip)
            .is_some());
    }

    #[test]
    fn unsupported_effect_is_retained_as_a_bypassed_plugin_and_blocks_exactness() {
        let project = DawProject::new("lowering", 48_000, 120.0).unwrap();
        let prepared = prepare_patterned_voice_lowering(
            &project,
            &compiled(true),
            &GenerativeLoweringOptions::default(),
        )
        .unwrap();
        assert!(!prepared.construction.is_exactly_renderable());
        assert!(prepared
            .construction
            .diagnostics
            .iter()
            .any(|item| matches!(
                item,
                GenerativeLoweringDiagnostic::DeferredPluginProcessor { .. }
            )));
        let mixer = prepared
            .envelope
            .commands
            .iter()
            .find_map(|command| match command {
                DomainCommand::Mixer(command) => Some(command.after()),
                _ => None,
            })
            .unwrap();
        let processor = mixer.processors().next().unwrap();
        assert_eq!(processor.descriptor().format, "audec-term-v1");
    }

    #[test]
    fn lowering_is_deterministic_against_the_same_snapshot() {
        let project = DawProject::new("lowering", 44_100, 125.0).unwrap();
        let program = compiled(false);
        let first = prepare_patterned_voice_lowering(
            &project,
            &program,
            &GenerativeLoweringOptions::default(),
        )
        .unwrap();
        let second = prepare_patterned_voice_lowering(
            &project,
            &program,
            &GenerativeLoweringOptions::default(),
        )
        .unwrap();
        assert_eq!(first.envelope, second.envelope);
        assert_eq!(
            first.construction.comparison,
            second.construction.comparison
        );
        assert_eq!(
            first.construction.voices["bass"].id,
            second.construction.voices["bass"].id
        );
    }
}
