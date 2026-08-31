//! Aspect algebra: compositional selection terms (skeleton).
//!
//! Normative design: `docs/LANGUAGES.md` §1. Implementation: ASPECT
//! workstream in `docs/SWARM_PLAN.md`. An aspect is a pure *term* denoting a
//! selection over time, frequency, channel, and inferred-object coordinates.
//! It asserts nothing about what produced the selected sound, and evaluating
//! it never touches project state except through the read-only resolver.
#![allow(dead_code, unused_variables)]

use std::fmt;

use crate::ontology;
use crate::reconstruction::ReconstructionProposalId;

/// Half-open time span in signed project frames. `start < end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameSpan {
    pub start: i64,
    pub end: i64,
}

/// Frequency extent in Hz. Equality preserves exact IEEE bits so normalized
/// terms hash deterministically (the `spectral_tiles::FrequencyRange` idiom).
#[derive(Clone, Copy, Debug)]
pub struct BandSpan {
    pub min_hz: f32,
    pub max_hz: f32,
}

impl PartialEq for BandSpan {
    fn eq(&self, other: &Self) -> bool {
        self.min_hz.to_bits() == other.min_hz.to_bits()
            && self.max_hz.to_bits() == other.max_hz.to_bits()
    }
}

impl Eq for BandSpan {}

/// Bitset over source channels; bit 0 is channel 0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ChannelMask(pub u16);

/// Identity of the analysis run an inference-scoped selector refers to.
/// Family IDs are meaningless without the run that produced them.
///
/// The concrete derivation (source content identity + recipe hash) is fixed
/// by the ASPECT lane; the type exists now so signatures are stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AnalysisRef {
    pub source: u64,
    pub recipe: u64,
}

/// A deferred reference to a project explanation. Resolution happens at
/// evaluation against the resolver, never inside the term.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplanationRef {
    Proposal(ReconstructionProposalId),
}

/// A pure selection term. See `docs/LANGUAGES.md` for the algebraic laws.
#[derive(Clone, Debug, PartialEq)]
pub enum Aspect {
    All,
    Time(FrameSpan),
    Band(BandSpan),
    Channels(ChannelMask),
    Family { analysis: AnalysisRef, id: usize },
    Object(ontology::ObjectId),
    Union(Vec<Aspect>),
    Intersect(Vec<Aspect>),
    Complement(Box<Aspect>),
    ExplainedBy(ExplanationRef),
    ResidualOf(ExplanationRef),
}

/// The evaluated shape: disjoint sorted spans and bands, a channel mask, and
/// the objects that contributed. Fields are concrete so consumers (sampling,
/// masking, coverage) do not re-derive geometry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConcreteAspect {
    pub time: Vec<FrameSpan>,
    pub bands: Vec<BandSpan>,
    pub channels: ChannelMask,
    pub objects: Vec<ontology::ObjectId>,
}

/// Read-only resolution of project-scoped references. `aspect.rs` itself
/// never holds project state.
pub trait AspectResolver {
    fn universe(&self) -> ConcreteAspect;
    fn family_spans(&self, analysis: &AnalysisRef, id: usize) -> Option<Vec<FrameSpan>>;
    fn object_extent(&self, object: ontology::ObjectId) -> Option<ConcreteAspect>;
    fn explanation_extent(&self, reference: &ExplanationRef) -> Option<ConcreteAspect>;
}

#[derive(Clone, Debug, PartialEq)]
pub enum AspectError {
    /// The term referenced an analysis, object, or explanation the resolver
    /// does not know. Never silently widened to `All`.
    Unresolvable(String),
    InvalidSpan { start: i64, end: i64 },
}

impl fmt::Display for AspectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unresolvable(what) => write!(formatter, "unresolvable aspect reference: {what}"),
            Self::InvalidSpan { start, end } => {
                write!(formatter, "invalid span {start}..{end}")
            }
        }
    }
}

impl std::error::Error for AspectError {}

/// Canonical form: unions/intersections flattened and sorted, sibling spans
/// interval-merged, `All`/empty absorbed, double complements eliminated.
/// Idempotent; `evaluate(normalize(a)) == evaluate(a)`.
pub fn normalize(aspect: Aspect) -> Aspect {
    todo!("ASPECT lane: docs/LANGUAGES.md section 1")
}

pub fn evaluate(
    aspect: &Aspect,
    resolver: &dyn AspectResolver,
) -> Result<ConcreteAspect, AspectError> {
    todo!("ASPECT lane: docs/LANGUAGES.md section 1")
}
