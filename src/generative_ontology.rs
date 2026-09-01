//! Typed generative terms shared by production and deprojection.
//!
//! A voice program says how an editable construction may sound: material or
//! oscillator layers at explicit pitches, ordered processors, and modulation
//! routed to semantic controls. It does not identify a physical instrument,
//! assert that an inferred patch is true, render a second audio graph, or let
//! residual audio enter the constructive sum. Compilation is pure: it resolves
//! evidence-backed curves, validates units and references, canonicalizes
//! provenance, and freezes a content-addressed topology for the existing DAW
//! compiler to lower later.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::artifact_catalog::{sha256_content, ArtifactId, ContentDigest, DigestAlgorithm};
use crate::aspect::{self, Aspect};
use crate::curve_lang::{self, CurveExpr};
use crate::ontology::{
    EffectKind, EvidenceId, FilterShape, HypothesisId, HypothesisSetId, OscillatorShape, TailExtent,
};
use crate::pattern_lang;
use crate::reconstruction::ReconstructionEvidenceId;
use crate::sample_material::SourceMaterialRef;
use crate::sequencer::BeatDuration;

const VOICE_DOMAIN: &[u8] = b"audec:generative-voice:v1";
const CONTROL_DOMAIN: &[u8] = b"audec:generative-control:v1";
const PATTERN_DOMAIN: &[u8] = b"audec:generative-pattern:v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VoiceProgramId(pub ContentDigest);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ControlProgramId(pub ContentDigest);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PatternedProgramId(pub ContentDigest);

/// Evidence that justifies a proposed term. None of these references grants
/// the term source-identity status.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TermEvidenceRef {
    Air(EvidenceId),
    Artifact(ArtifactId),
    Reconstruction {
        artifact: ArtifactId,
        evidence: ReconstructionEvidenceId,
    },
    NativeLocator {
        analyzer: String,
        version: String,
        locator: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HypothesisRef {
    pub set: HypothesisSetId,
    pub hypothesis: HypothesisId,
}

/// Authorship and inference are orthogonal to playability. Editing an inferred
/// term records divergence; it does not silently promote the hypothesis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermOrigin {
    Authored {
        author: Option<String>,
    },
    Inferred {
        producer: String,
        evidence: Vec<TermEvidenceRef>,
        hypothesis: Option<HypothesisRef>,
        diverged: bool,
    },
}

impl TermOrigin {
    pub fn mark_diverged(&mut self) {
        if let Self::Inferred { diverged, .. } = self {
            *diverged = true;
        }
    }

    fn normalize_and_validate(&mut self) -> Result<(), CompileError> {
        match self {
            Self::Authored { author } => {
                if author.as_ref().is_some_and(|name| name.trim().is_empty()) {
                    return Err(CompileError::InvalidOrigin(
                        "an authored name cannot be blank".into(),
                    ));
                }
            }
            Self::Inferred {
                producer, evidence, ..
            } => {
                if producer.trim().is_empty() {
                    return Err(CompileError::InvalidOrigin(
                        "an inferred term needs a producer".into(),
                    ));
                }
                evidence.sort();
                evidence.dedup();
                if evidence.is_empty() {
                    return Err(CompileError::InvalidOrigin(
                        "an inferred term needs retained evidence".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ControlUnit {
    Linear,
    Decibels,
    Normalized,
    Bipolar,
    Hertz,
    Semitones,
    Cents,
    Ratio,
    Seconds,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ControlTarget {
    LayerGain,
    Pitch,
    FilterCutoff,
    FilterResonance,
    FilterGain,
    EffectWet,
    Pan,
    Width,
    Envelope,
    Custom(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ControlBindingMode {
    Replace,
    Add,
    Multiply,
    FrequencyModulation,
    AmplitudeModulation,
}

/// A curve plus its physical/semantic unit. Curve expressions are the same
/// terms emitted by pitch deprojection and authored in production.
#[derive(Clone, Debug, PartialEq)]
pub struct ControlTerm {
    pub unit: ControlUnit,
    pub expression: CurveExpr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoiseColor {
    White,
    Pink,
    Brown,
}

/// A playable generator description, never an instrument label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GeneratorTerm {
    Material(SourceMaterialRef),
    Oscillator(OscillatorShape),
    Noise(NoiseColor),
    /// An immutable model/native audio claim. Its additive contract remains
    /// owned by the deprojection graph that supplied this digest.
    AudioClaim(ContentDigest),
    Preset(ArtifactId),
}

#[derive(Clone, Debug, PartialEq)]
pub enum PitchTerm {
    Unpitched,
    FixedHz(f64),
    Midi {
        key: u8,
        cents: f64,
    },
    /// Absolute Hz or relative cents/semitones; any other unit is refused.
    Curve(ControlTerm),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModulationTerm {
    pub target: ControlTarget,
    pub source: ControlTerm,
    pub depth: f64,
    pub offset: f64,
    pub mode: ControlBindingMode,
}

/// Processor order is audible and therefore retained exactly.
#[derive(Clone, Debug, PartialEq)]
pub enum ProcessorTerm {
    Envelope(ControlTerm),
    Gain(ControlTerm),
    Filter {
        shape: FilterShape,
        cutoff: ControlTerm,
        resonance: Option<ControlTerm>,
        gain: Option<ControlTerm>,
    },
    Effect {
        kind: EffectKind,
        wet: ControlTerm,
        tail: TailExtent,
    },
    Spatial {
        pan: Option<ControlTerm>,
        width: Option<ControlTerm>,
    },
}

/// One independently pitched layer. A voice may contain several layers for
/// chords, octave stacks, detuning, or parallel material hypotheses.
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceLayer {
    pub generator: GeneratorTerm,
    pub pitch: PitchTerm,
    pub gain: ControlTerm,
    pub processors: Vec<ProcessorTerm>,
    pub modulation: Vec<ModulationTerm>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VoiceProgram {
    pub schema_version: u32,
    pub label: String,
    pub origin: TermOrigin,
    pub layers: Vec<VoiceLayer>,
}

impl VoiceProgram {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn authored(label: impl Into<String>, layers: Vec<VoiceLayer>) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            label: label.into(),
            origin: TermOrigin::Authored { author: None },
            layers,
        }
    }
}

/// Resolves measured curve placeholders into ordinary generator terms. The
/// resolver is read-only and compilation detects recursive evidence cycles.
pub trait CurveEvidenceResolver {
    fn curve_for(&self, evidence: ReconstructionEvidenceId) -> Option<CurveExpr>;
}

#[derive(Default)]
pub struct NoCurveEvidence;

impl CurveEvidenceResolver for NoCurveEvidence {
    fn curve_for(&self, _: ReconstructionEvidenceId) -> Option<CurveExpr> {
        None
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledControl {
    pub id: ControlProgramId,
    pub unit: ControlUnit,
    pub expression: CurveExpr,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CompiledPitch {
    Unpitched,
    FixedHz(f64),
    Midi { key: u8, cents: f64 },
    Curve(CompiledControl),
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledModulation {
    pub target: ControlTarget,
    pub source: CompiledControl,
    pub depth: f64,
    pub offset: f64,
    pub mode: ControlBindingMode,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CompiledProcessor {
    Envelope(CompiledControl),
    Gain(CompiledControl),
    Filter {
        shape: FilterShape,
        cutoff: CompiledControl,
        resonance: Option<CompiledControl>,
        gain: Option<CompiledControl>,
    },
    Effect {
        kind: EffectKind,
        wet: CompiledControl,
        tail: TailExtent,
    },
    Spatial {
        pan: Option<CompiledControl>,
        width: Option<CompiledControl>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledLayer {
    pub generator: GeneratorTerm,
    pub pitch: CompiledPitch,
    pub gain: CompiledControl,
    pub processors: Vec<CompiledProcessor>,
    pub modulation: Vec<CompiledModulation>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledVoiceProgram {
    pub id: VoiceProgramId,
    pub label: String,
    pub origin: TermOrigin,
    pub layers: Vec<CompiledLayer>,
    /// Size of the complete identity encoding, including provenance. This is
    /// not an MDL score; callers rank sound structure separately from labels
    /// and evidence bookkeeping.
    pub canonical_bytes: u64,
    pub free_controls: u32,
}

/// Compile a voice into an immutable topology. The returned ID covers the
/// resolved curves and provenance, so two different evidential realizations
/// cannot collide merely because they shared an `FromEvidence` placeholder.
pub fn compile_voice(
    program: &VoiceProgram,
    resolver: &dyn CurveEvidenceResolver,
) -> Result<CompiledVoiceProgram, CompileError> {
    let mut normalized = program.clone();
    if normalized.schema_version != VoiceProgram::SCHEMA_VERSION {
        return Err(CompileError::UnsupportedSchema(normalized.schema_version));
    }
    if normalized.label.trim().is_empty() {
        return Err(CompileError::EmptyLabel);
    }
    if normalized.layers.is_empty() {
        return Err(CompileError::EmptyVoice);
    }
    normalized.origin.normalize_and_validate()?;

    let mut canonical = Vec::new();
    push_str(&mut canonical, normalized.label.trim());
    push_origin(&mut canonical, &normalized.origin);
    let mut free_controls = 0_u32;
    let mut layers = Vec::with_capacity(normalized.layers.len());
    for (index, layer) in normalized.layers.iter().enumerate() {
        layers.push(compile_layer(
            layer,
            resolver,
            &mut canonical,
            &mut free_controls,
            index,
        )?);
    }
    let id = VoiceProgramId(sha256_content(VOICE_DOMAIN, &[&canonical]));
    Ok(CompiledVoiceProgram {
        id,
        label: normalized.label.trim().to_owned(),
        origin: normalized.origin,
        layers,
        canonical_bytes: canonical.len() as u64,
        free_controls,
    })
}

fn compile_layer(
    layer: &VoiceLayer,
    resolver: &dyn CurveEvidenceResolver,
    canonical: &mut Vec<u8>,
    free_controls: &mut u32,
    index: usize,
) -> Result<CompiledLayer, CompileError> {
    canonical.extend_from_slice(b"layer\0");
    canonical.extend_from_slice(&(index as u64).to_le_bytes());
    push_generator(canonical, &layer.generator)?;
    let pitch = match &layer.pitch {
        PitchTerm::Unpitched => {
            canonical.extend_from_slice(b"pitch:unpitched\0");
            CompiledPitch::Unpitched
        }
        PitchTerm::FixedHz(hz) => {
            if !hz.is_finite() || *hz <= 0.0 {
                return Err(CompileError::InvalidNumber("fixed pitch Hz"));
            }
            canonical.extend_from_slice(b"pitch:hz\0");
            canonical.extend_from_slice(&hz.to_bits().to_le_bytes());
            CompiledPitch::FixedHz(*hz)
        }
        PitchTerm::Midi { key, cents } => {
            if !cents.is_finite() {
                return Err(CompileError::InvalidNumber("MIDI pitch cents"));
            }
            canonical.extend_from_slice(b"pitch:midi\0");
            canonical.push(*key);
            canonical.extend_from_slice(&cents.to_bits().to_le_bytes());
            CompiledPitch::Midi {
                key: *key,
                cents: *cents,
            }
        }
        PitchTerm::Curve(control) => {
            require_unit(
                control.unit,
                &[
                    ControlUnit::Hertz,
                    ControlUnit::Cents,
                    ControlUnit::Semitones,
                ],
                "pitch curve",
            )?;
            canonical.extend_from_slice(b"pitch:curve\0");
            CompiledPitch::Curve(compile_control(
                control,
                resolver,
                canonical,
                free_controls,
            )?)
        }
    };
    require_unit(
        layer.gain.unit,
        &[ControlUnit::Linear, ControlUnit::Decibels],
        "layer gain",
    )?;
    let gain = compile_control(&layer.gain, resolver, canonical, free_controls)?;
    let mut processors = Vec::with_capacity(layer.processors.len());
    for processor in &layer.processors {
        processors.push(compile_processor(
            processor,
            resolver,
            canonical,
            free_controls,
        )?);
    }
    let mut modulation = Vec::with_capacity(layer.modulation.len());
    for binding in &layer.modulation {
        if !binding.depth.is_finite() || !binding.offset.is_finite() {
            return Err(CompileError::InvalidNumber("modulation depth/offset"));
        }
        canonical.extend_from_slice(b"mod\0");
        push_control_target(canonical, binding.target);
        canonical.extend_from_slice(&(binding.mode as u64).to_le_bytes());
        canonical.extend_from_slice(&binding.depth.to_bits().to_le_bytes());
        canonical.extend_from_slice(&binding.offset.to_bits().to_le_bytes());
        modulation.push(CompiledModulation {
            target: binding.target,
            source: compile_control(&binding.source, resolver, canonical, free_controls)?,
            depth: binding.depth,
            offset: binding.offset,
            mode: binding.mode,
        });
    }
    Ok(CompiledLayer {
        generator: layer.generator.clone(),
        pitch,
        gain,
        processors,
        modulation,
    })
}

fn compile_processor(
    processor: &ProcessorTerm,
    resolver: &dyn CurveEvidenceResolver,
    canonical: &mut Vec<u8>,
    free_controls: &mut u32,
) -> Result<CompiledProcessor, CompileError> {
    Ok(match processor {
        ProcessorTerm::Envelope(control) => {
            require_unit(control.unit, &[ControlUnit::Normalized], "envelope")?;
            canonical.extend_from_slice(b"processor:envelope\0");
            CompiledProcessor::Envelope(compile_control(
                control,
                resolver,
                canonical,
                free_controls,
            )?)
        }
        ProcessorTerm::Gain(control) => {
            require_unit(
                control.unit,
                &[ControlUnit::Linear, ControlUnit::Decibels],
                "gain processor",
            )?;
            canonical.extend_from_slice(b"processor:gain\0");
            CompiledProcessor::Gain(compile_control(
                control,
                resolver,
                canonical,
                free_controls,
            )?)
        }
        ProcessorTerm::Filter {
            shape,
            cutoff,
            resonance,
            gain,
        } => {
            require_unit(cutoff.unit, &[ControlUnit::Hertz], "filter cutoff")?;
            if let Some(control) = resonance {
                require_unit(
                    control.unit,
                    &[ControlUnit::Ratio, ControlUnit::Normalized],
                    "filter resonance",
                )?;
            }
            if let Some(control) = gain {
                require_unit(control.unit, &[ControlUnit::Decibels], "filter gain")?;
            }
            canonical.extend_from_slice(b"processor:filter\0");
            canonical.extend_from_slice(&(*shape as u64).to_le_bytes());
            CompiledProcessor::Filter {
                shape: *shape,
                cutoff: compile_control(cutoff, resolver, canonical, free_controls)?,
                resonance: resonance
                    .as_ref()
                    .map(|control| compile_control(control, resolver, canonical, free_controls))
                    .transpose()?,
                gain: gain
                    .as_ref()
                    .map(|control| compile_control(control, resolver, canonical, free_controls))
                    .transpose()?,
            }
        }
        ProcessorTerm::Effect { kind, wet, tail } => {
            require_unit(wet.unit, &[ControlUnit::Normalized], "effect wet")?;
            validate_tail(*tail)?;
            canonical.extend_from_slice(b"processor:effect\0");
            canonical.extend_from_slice(&(*kind as u64).to_le_bytes());
            push_tail(canonical, *tail);
            CompiledProcessor::Effect {
                kind: *kind,
                wet: compile_control(wet, resolver, canonical, free_controls)?,
                tail: *tail,
            }
        }
        ProcessorTerm::Spatial { pan, width } => {
            if let Some(control) = pan {
                require_unit(control.unit, &[ControlUnit::Bipolar], "pan")?;
            }
            if let Some(control) = width {
                require_unit(control.unit, &[ControlUnit::Normalized], "width")?;
            }
            canonical.extend_from_slice(b"processor:spatial\0");
            CompiledProcessor::Spatial {
                pan: pan
                    .as_ref()
                    .map(|control| compile_control(control, resolver, canonical, free_controls))
                    .transpose()?,
                width: width
                    .as_ref()
                    .map(|control| compile_control(control, resolver, canonical, free_controls))
                    .transpose()?,
            }
        }
    })
}

fn compile_control(
    control: &ControlTerm,
    resolver: &dyn CurveEvidenceResolver,
    canonical: &mut Vec<u8>,
    free_controls: &mut u32,
) -> Result<CompiledControl, CompileError> {
    let expression = resolve_curve(&control.expression, resolver, &mut BTreeSet::new())?;
    // This checks every numeric leaf and the expression's structural rules.
    curve_lang::evaluate_curve(&expression, 0.0, 1.0)
        .map_err(|error| CompileError::InvalidCurve(error.to_string()))?;
    let printed = curve_lang::print(&expression);
    let unit = format!("{:?}", control.unit);
    let id = ControlProgramId(sha256_content(
        CONTROL_DOMAIN,
        &[unit.as_bytes(), printed.as_bytes()],
    ));
    canonical.extend_from_slice(b"control\0");
    canonical.extend_from_slice(&id.0.bytes);
    *free_controls = free_controls
        .checked_add(1)
        .ok_or(CompileError::ControlCountOverflow)?;
    Ok(CompiledControl {
        id,
        unit: control.unit,
        expression,
    })
}

fn resolve_curve(
    expression: &CurveExpr,
    resolver: &dyn CurveEvidenceResolver,
    active: &mut BTreeSet<ReconstructionEvidenceId>,
) -> Result<CurveExpr, CompileError> {
    Ok(match expression {
        CurveExpr::FromEvidence(id) => {
            if !active.insert(*id) {
                return Err(CompileError::EvidenceCycle(*id));
            }
            let resolved = resolver
                .curve_for(*id)
                .ok_or(CompileError::UnresolvedEvidence(*id))?;
            let result = resolve_curve(&resolved, resolver, active)?;
            active.remove(id);
            result
        }
        CurveExpr::Sum(members) => CurveExpr::Sum(
            members
                .iter()
                .map(|member| resolve_curve(member, resolver, active))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        CurveExpr::Scale {
            input,
            multiply,
            add,
        } => CurveExpr::Scale {
            input: Box::new(resolve_curve(input, resolver, active)?),
            multiply: *multiply,
            add: *add,
        },
        CurveExpr::Clamp { input, min, max } => CurveExpr::Clamp {
            input: Box::new(resolve_curve(input, resolver, active)?),
            min: *min,
            max: *max,
        },
        other => other.clone(),
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct PatternedVoiceProgram {
    pub pattern_source: String,
    pub cycle: BeatDuration,
    pub seed: u64,
    pub initial_cycle_index: u64,
    /// Pattern-language binding name to a complete voice program.
    pub voices: BTreeMap<String, VoiceProgram>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledPatternedVoiceProgram {
    pub id: PatternedProgramId,
    pub canonical_pattern: String,
    pub pattern_hash: crate::sequencer::PatternTermHash,
    pub cycle: BeatDuration,
    pub seed: u64,
    pub initial_cycle_index: u64,
    pub voices: BTreeMap<String, CompiledVoiceProgram>,
}

/// Compile the same pattern syntax used by production/deprojection, while
/// keeping symbolic names bound to rich voice terms until project IDs are
/// allocated by the command boundary.
pub fn compile_patterned_voices(
    program: &PatternedVoiceProgram,
    resolver: &dyn CurveEvidenceResolver,
) -> Result<CompiledPatternedVoiceProgram, CompileError> {
    if program.cycle.0 == 0 || program.cycle.0 > i64::MAX as u64 {
        return Err(CompileError::InvalidCycle);
    }
    let parsed = pattern_lang::parse(&program.pattern_source)
        .map_err(|error| CompileError::InvalidPattern(error.to_string()))?;
    let referenced = pattern_lang::referenced_bindings(&parsed);
    let supplied = program.voices.keys().cloned().collect::<BTreeSet<_>>();
    if let Some(binding) = referenced.difference(&supplied).next() {
        return Err(CompileError::MissingPatternVoice(binding.clone()));
    }
    // Extra voices are an ordinary production palette, not an error. Only a
    // referenced name must resolve; keeping an unused pad ready is harmless.
    let canonical_pattern = pattern_lang::print(&parsed);
    let pattern_hash = pattern_lang::term_hash(&parsed);
    let mut voices = BTreeMap::new();
    let mut bytes = canonical_pattern.as_bytes().to_vec();
    bytes.extend_from_slice(&program.cycle.0.to_le_bytes());
    bytes.extend_from_slice(&program.seed.to_le_bytes());
    bytes.extend_from_slice(&program.initial_cycle_index.to_le_bytes());
    for (binding, voice) in &program.voices {
        let compiled = compile_voice(voice, resolver)?;
        push_str(&mut bytes, binding);
        bytes.extend_from_slice(&compiled.id.0.bytes);
        voices.insert(binding.clone(), compiled);
    }
    Ok(CompiledPatternedVoiceProgram {
        id: PatternedProgramId(sha256_content(PATTERN_DOMAIN, &[&bytes])),
        canonical_pattern,
        pattern_hash,
        cycle: program.cycle,
        seed: program.seed,
        initial_cycle_index: program.initial_cycle_index,
        voices,
    })
}

/// The equation records only construction roots. Residual and excess are
/// derived comparison channels, never terms that can be smuggled into the
/// sum and rewarded for trivially reproducing the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplanationEquation {
    pub source: ContentDigest,
    pub extent: Aspect,
    pub explanations: Vec<ConstructionTermRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConstructionTermRef {
    Voice(VoiceProgramId),
    Patterned(PatternedProgramId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DerivedComparisonSignal {
    ResidualSourceMinusConstruction,
    SpectralExcess,
}

impl ExplanationEquation {
    pub fn new(
        source: ContentDigest,
        extent: Aspect,
        mut explanations: Vec<ConstructionTermRef>,
    ) -> Result<Self, CompileError> {
        if explanations.is_empty() {
            return Err(CompileError::EmptyEquation);
        }
        explanations.sort();
        if explanations.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(CompileError::DuplicateExplanation);
        }
        Ok(Self {
            source,
            extent: aspect::normalize(extent),
            explanations,
        })
    }

    pub const fn derived_signals(&self) -> [DerivedComparisonSignal; 2] {
        [
            DerivedComparisonSignal::ResidualSourceMinusConstruction,
            DerivedComparisonSignal::SpectralExcess,
        ]
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompileError {
    UnsupportedSchema(u32),
    EmptyLabel,
    EmptyVoice,
    InvalidOrigin(String),
    InvalidNumber(&'static str),
    InvalidUnit {
        context: &'static str,
        actual: ControlUnit,
    },
    InvalidMaterial(String),
    InvalidTail,
    InvalidCurve(String),
    UnresolvedEvidence(ReconstructionEvidenceId),
    EvidenceCycle(ReconstructionEvidenceId),
    ControlCountOverflow,
    InvalidPattern(String),
    InvalidCycle,
    MissingPatternVoice(String),
    EmptyEquation,
    DuplicateExplanation,
}

impl fmt::Display for CompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported voice schema {version}")
            }
            Self::EmptyLabel => formatter.write_str("voice label is empty"),
            Self::EmptyVoice => formatter.write_str("voice contains no layers"),
            Self::InvalidOrigin(message) => write!(formatter, "invalid voice origin: {message}"),
            Self::InvalidNumber(name) => write!(formatter, "invalid numeric value for {name}"),
            Self::InvalidUnit { context, actual } => {
                write!(formatter, "{context} cannot use {actual:?}")
            }
            Self::InvalidMaterial(message) => {
                write!(formatter, "invalid generator material: {message}")
            }
            Self::InvalidTail => formatter.write_str("invalid effect tail"),
            Self::InvalidCurve(message) => write!(formatter, "invalid control curve: {message}"),
            Self::UnresolvedEvidence(id) => {
                write!(formatter, "unresolved curve evidence {}", id.get())
            }
            Self::EvidenceCycle(id) => write!(formatter, "curve evidence cycle at {}", id.get()),
            Self::ControlCountOverflow => formatter.write_str("voice control count overflow"),
            Self::InvalidPattern(message) => write!(formatter, "invalid pattern: {message}"),
            Self::InvalidCycle => formatter.write_str("pattern cycle is empty or too large"),
            Self::MissingPatternVoice(binding) => {
                write!(formatter, "pattern binding {binding} has no voice")
            }
            Self::EmptyEquation => {
                formatter.write_str("explanation equation has no construction roots")
            }
            Self::DuplicateExplanation => {
                formatter.write_str("explanation equation double-counts a construction root")
            }
        }
    }
}

impl Error for CompileError {}

fn require_unit(
    actual: ControlUnit,
    allowed: &[ControlUnit],
    context: &'static str,
) -> Result<(), CompileError> {
    allowed
        .contains(&actual)
        .then_some(())
        .ok_or(CompileError::InvalidUnit { context, actual })
}

fn validate_tail(tail: TailExtent) -> Result<(), CompileError> {
    match tail {
        TailExtent::UntilBelow {
            threshold_db,
            hold_frames,
            maximum_frames,
        } if !threshold_db.is_finite()
            || hold_frames == 0
            || maximum_frames == 0
            || hold_frames > maximum_frames =>
        {
            Err(CompileError::InvalidTail)
        }
        _ => Ok(()),
    }
}

fn push_generator(bytes: &mut Vec<u8>, generator: &GeneratorTerm) -> Result<(), CompileError> {
    match generator {
        GeneratorTerm::Material(material) => {
            material
                .validate()
                .map_err(|error| CompileError::InvalidMaterial(error.to_string()))?;
            bytes.extend_from_slice(b"generator:material\0");
            bytes.extend_from_slice(&material.asset_id().0.to_le_bytes());
            if let Some(slice) = material.virtual_slice() {
                bytes.push(1);
                bytes.extend_from_slice(&slice.source_range.start.0.to_le_bytes());
                bytes.extend_from_slice(&slice.source_range.end.0.to_le_bytes());
            } else {
                bytes.push(0);
            }
        }
        GeneratorTerm::Oscillator(shape) => {
            bytes.extend_from_slice(b"generator:oscillator\0");
            bytes.extend_from_slice(&(*shape as u64).to_le_bytes());
        }
        GeneratorTerm::Noise(color) => {
            bytes.extend_from_slice(b"generator:noise\0");
            bytes.extend_from_slice(&(*color as u64).to_le_bytes());
        }
        GeneratorTerm::AudioClaim(digest) => {
            bytes.extend_from_slice(b"generator:claim\0");
            push_digest(bytes, *digest);
        }
        GeneratorTerm::Preset(artifact) => {
            bytes.extend_from_slice(b"generator:preset\0");
            push_digest(bytes, artifact.0);
        }
    }
    Ok(())
}

fn push_origin(bytes: &mut Vec<u8>, origin: &TermOrigin) {
    match origin {
        TermOrigin::Authored { author } => {
            bytes.extend_from_slice(b"origin:authored\0");
            push_str(bytes, author.as_deref().unwrap_or(""));
        }
        TermOrigin::Inferred {
            producer,
            evidence,
            hypothesis,
            diverged,
        } => {
            bytes.extend_from_slice(b"origin:inferred\0");
            push_str(bytes, producer);
            bytes.push(u8::from(*diverged));
            if let Some(reference) = hypothesis {
                bytes.push(1);
                bytes.extend_from_slice(&reference.set.get().to_le_bytes());
                bytes.extend_from_slice(&reference.hypothesis.get().to_le_bytes());
            } else {
                bytes.push(0);
            }
            for item in evidence {
                push_evidence(bytes, item);
            }
        }
    }
}

fn push_tail(bytes: &mut Vec<u8>, tail: TailExtent) {
    match tail {
        TailExtent::None => bytes.extend_from_slice(b"tail:none\0"),
        TailExtent::FiniteFrames(frames) => {
            bytes.extend_from_slice(b"tail:finite\0");
            bytes.extend_from_slice(&frames.to_le_bytes());
        }
        TailExtent::UntilBelow {
            threshold_db,
            hold_frames,
            maximum_frames,
        } => {
            bytes.extend_from_slice(b"tail:threshold\0");
            bytes.extend_from_slice(&threshold_db.to_bits().to_le_bytes());
            bytes.extend_from_slice(&hold_frames.to_le_bytes());
            bytes.extend_from_slice(&maximum_frames.to_le_bytes());
        }
        TailExtent::Unbounded => bytes.extend_from_slice(b"tail:unbounded\0"),
    }
}

fn push_evidence(bytes: &mut Vec<u8>, evidence: &TermEvidenceRef) {
    match evidence {
        TermEvidenceRef::Air(id) => {
            bytes.extend_from_slice(b"evidence:air\0");
            bytes.extend_from_slice(&id.get().to_le_bytes());
        }
        TermEvidenceRef::Artifact(artifact) => {
            bytes.extend_from_slice(b"evidence:artifact\0");
            push_digest(bytes, artifact.0);
        }
        TermEvidenceRef::Reconstruction { artifact, evidence } => {
            bytes.extend_from_slice(b"evidence:reconstruction\0");
            push_digest(bytes, artifact.0);
            bytes.extend_from_slice(&evidence.get().to_le_bytes());
        }
        TermEvidenceRef::NativeLocator {
            analyzer,
            version,
            locator,
        } => {
            bytes.extend_from_slice(b"evidence:native\0");
            push_str(bytes, analyzer);
            push_str(bytes, version);
            push_str(bytes, locator);
        }
    }
}

fn push_digest(bytes: &mut Vec<u8>, digest: ContentDigest) {
    bytes.push(match digest.algorithm {
        DigestAlgorithm::Sha256 => 1,
        DigestAlgorithm::Blake3 => 2,
        DigestAlgorithm::StableNonCryptographic => 3,
    });
    bytes.extend_from_slice(&digest.bytes);
}

fn push_control_target(bytes: &mut Vec<u8>, target: ControlTarget) {
    match target {
        ControlTarget::LayerGain => bytes.extend_from_slice(b"target:layer-gain\0"),
        ControlTarget::Pitch => bytes.extend_from_slice(b"target:pitch\0"),
        ControlTarget::FilterCutoff => bytes.extend_from_slice(b"target:filter-cutoff\0"),
        ControlTarget::FilterResonance => bytes.extend_from_slice(b"target:filter-resonance\0"),
        ControlTarget::FilterGain => bytes.extend_from_slice(b"target:filter-gain\0"),
        ControlTarget::EffectWet => bytes.extend_from_slice(b"target:effect-wet\0"),
        ControlTarget::Pan => bytes.extend_from_slice(b"target:pan\0"),
        ControlTarget::Width => bytes.extend_from_slice(b"target:width\0"),
        ControlTarget::Envelope => bytes.extend_from_slice(b"target:envelope\0"),
        ControlTarget::Custom(value) => {
            bytes.extend_from_slice(b"target:custom\0");
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
}

fn push_str(bytes: &mut Vec<u8>, value: &str) {
    bytes.extend_from_slice(&(value.len() as u64).to_le_bytes());
    bytes.extend_from_slice(value.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_catalog::DigestAlgorithm;
    use crate::curve_lang::LfoShape;

    fn digest(byte: u8) -> ContentDigest {
        ContentDigest::new(DigestAlgorithm::Sha256, [byte; 32])
    }

    fn control(unit: ControlUnit, expression: CurveExpr) -> ControlTerm {
        ControlTerm { unit, expression }
    }

    fn synth_voice(label: &str) -> VoiceProgram {
        VoiceProgram::authored(
            label,
            vec![VoiceLayer {
                generator: GeneratorTerm::Oscillator(OscillatorShape::SawUp),
                pitch: PitchTerm::Midi {
                    key: 48,
                    cents: -7.0,
                },
                gain: control(ControlUnit::Linear, CurveExpr::Const(0.7)),
                processors: vec![
                    ProcessorTerm::Envelope(control(
                        ControlUnit::Normalized,
                        CurveExpr::Env {
                            attack: 0.01,
                            decay: 0.2,
                            sustain: 0.6,
                            release: 0.4,
                        },
                    )),
                    ProcessorTerm::Filter {
                        shape: FilterShape::LowPass,
                        cutoff: control(
                            ControlUnit::Hertz,
                            CurveExpr::Line {
                                from: 320.0,
                                to: 4_800.0,
                            },
                        ),
                        resonance: Some(control(ControlUnit::Ratio, CurveExpr::Const(0.71))),
                        gain: None,
                    },
                    ProcessorTerm::Effect {
                        kind: EffectKind::Reverberation,
                        wet: control(ControlUnit::Normalized, CurveExpr::Const(0.35)),
                        tail: TailExtent::FiniteFrames(44_100),
                    },
                ],
                modulation: vec![ModulationTerm {
                    target: ControlTarget::Pitch,
                    source: control(
                        ControlUnit::Cents,
                        CurveExpr::Lfo {
                            shape: LfoShape::Sine,
                            rate_hz: 6.0,
                            depth: 14.0,
                            phase: 0.0,
                        },
                    ),
                    depth: 1.0,
                    offset: 0.0,
                    mode: ControlBindingMode::Add,
                }],
            }],
        )
    }

    #[test]
    fn pitched_modulated_voice_compiles_deterministically() {
        let first = compile_voice(&synth_voice("acid recurrence A"), &NoCurveEvidence).unwrap();
        let second = compile_voice(&synth_voice("acid recurrence A"), &NoCurveEvidence).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.free_controls, 6);
        assert!(first.canonical_bytes > 100);
        assert!(matches!(
            first.layers[0].processors[1],
            CompiledProcessor::Filter { .. }
        ));
    }

    #[test]
    fn inferred_origin_is_canonical_but_never_becomes_authored() {
        let artifact = ArtifactId(digest(7));
        let mut voice = synth_voice("proposed bright recurrence");
        voice.origin = TermOrigin::Inferred {
            producer: "audec.pitch/1".into(),
            evidence: vec![
                TermEvidenceRef::Artifact(artifact),
                TermEvidenceRef::Artifact(artifact),
                TermEvidenceRef::Air(EvidenceId::new(3)),
            ],
            hypothesis: Some(HypothesisRef {
                set: HypothesisSetId::new(2),
                hypothesis: HypothesisId::new(9),
            }),
            diverged: false,
        };
        let mut reordered = voice.clone();
        let TermOrigin::Inferred { evidence, .. } = &mut reordered.origin else {
            unreachable!()
        };
        evidence.reverse();
        let mut compiled = compile_voice(&voice, &NoCurveEvidence).unwrap();
        let reordered = compile_voice(&reordered, &NoCurveEvidence).unwrap();
        assert_eq!(compiled.id, reordered.id);
        let TermOrigin::Inferred {
            evidence, diverged, ..
        } = &compiled.origin
        else {
            panic!("inference was silently promoted")
        };
        assert_eq!(evidence.len(), 2);
        assert!(!diverged);
        compiled.origin.mark_diverged();
        assert!(matches!(
            compiled.origin,
            TermOrigin::Inferred { diverged: true, .. }
        ));
    }

    struct Curves;

    impl CurveEvidenceResolver for Curves {
        fn curve_for(&self, evidence: ReconstructionEvidenceId) -> Option<CurveExpr> {
            match evidence.get() {
                1 => Some(CurveExpr::Lfo {
                    shape: LfoShape::Sine,
                    rate_hz: 5.8,
                    depth: 11.0,
                    phase: 0.25,
                }),
                2 => Some(CurveExpr::FromEvidence(ReconstructionEvidenceId::from_raw(
                    2,
                ))),
                _ => None,
            }
        }
    }

    #[test]
    fn evidence_curves_resolve_into_the_frozen_hash_and_cycles_refuse() {
        let mut voice = synth_voice("measured modulation");
        voice.layers[0].modulation[0].source.expression =
            CurveExpr::FromEvidence(ReconstructionEvidenceId::from_raw(1));
        let compiled = compile_voice(&voice, &Curves).unwrap();
        assert!(matches!(
            compiled.layers[0].modulation[0].source.expression,
            CurveExpr::Lfo { rate_hz, .. } if rate_hz == 5.8
        ));

        voice.layers[0].modulation[0].source.expression =
            CurveExpr::FromEvidence(ReconstructionEvidenceId::from_raw(2));
        assert_eq!(
            compile_voice(&voice, &Curves).unwrap_err(),
            CompileError::EvidenceCycle(ReconstructionEvidenceId::from_raw(2))
        );
    }

    #[test]
    fn one_pattern_language_binds_rich_voices_without_project_ids() {
        let program = PatternedVoiceProgram {
            pattern_source: "stack(e(3,8,fam0), fast(2, [~ fam1]))".into(),
            cycle: BeatDuration(3_840),
            seed: 42,
            initial_cycle_index: 7,
            voices: BTreeMap::from([
                ("fam0".into(), synth_voice("anonymous family zero")),
                ("fam1".into(), synth_voice("anonymous family one")),
            ]),
        };
        let compiled = compile_patterned_voices(&program, &NoCurveEvidence).unwrap();
        assert_eq!(compiled.voices.len(), 2);
        assert_eq!(compiled.initial_cycle_index, 7);
        assert_eq!(
            compiled.pattern_hash,
            pattern_lang::term_hash(&pattern_lang::parse(&compiled.canonical_pattern).unwrap())
        );

        let mut incomplete = program;
        incomplete.voices.remove("fam1");
        assert_eq!(
            compile_patterned_voices(&incomplete, &NoCurveEvidence).unwrap_err(),
            CompileError::MissingPatternVoice("fam1".into())
        );
    }

    #[test]
    fn units_are_checked_at_semantic_targets() {
        let mut voice = synth_voice("bad units");
        let ProcessorTerm::Filter { cutoff, .. } = &mut voice.layers[0].processors[1] else {
            unreachable!()
        };
        cutoff.unit = ControlUnit::Decibels;
        assert_eq!(
            compile_voice(&voice, &NoCurveEvidence).unwrap_err(),
            CompileError::InvalidUnit {
                context: "filter cutoff",
                actual: ControlUnit::Decibels,
            }
        );
    }

    #[test]
    fn residual_and_excess_are_derived_and_duplicate_roots_refuse() {
        let voice = compile_voice(&synth_voice("one explanation"), &NoCurveEvidence).unwrap();
        let extent = Aspect::Time(crate::aspect::FrameSpan::new(10, 20).unwrap());
        let root = ConstructionTermRef::Voice(voice.id);
        let equation = ExplanationEquation::new(digest(1), extent.clone(), vec![root]).unwrap();
        assert_eq!(equation.extent, aspect::normalize(extent));
        assert_eq!(
            equation.derived_signals(),
            [
                DerivedComparisonSignal::ResidualSourceMinusConstruction,
                DerivedComparisonSignal::SpectralExcess,
            ]
        );
        assert_eq!(
            ExplanationEquation::new(digest(1), Aspect::All, vec![root, root]).unwrap_err(),
            CompileError::DuplicateExplanation
        );
    }
}
