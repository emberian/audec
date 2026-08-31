//! Aspect algebra: compositional, read-only selection terms.
//!
//! An aspect selects geometry; it does not identify a physical source or make
//! a correctness claim. Evaluated geometry is a canonical union of regions,
//! rather than independent vectors whose Cartesian product would invent
//! coverage. The signal layer (source, one explanation, or one residual) is
//! carried separately so equal extents do not collapse distinct signals.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::ontology;
use crate::reconstruction::ReconstructionProposalId;

/// Half-open time span in signed project frames. `start < end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameSpan {
    pub start: i64,
    pub end: i64,
}

impl FrameSpan {
    pub const fn new(start: i64, end: i64) -> Option<Self> {
        if start < end {
            Some(Self { start, end })
        } else {
            None
        }
    }

    pub const fn intersect(self, other: Self) -> Option<Self> {
        Self::new(
            if self.start > other.start {
                self.start
            } else {
                other.start
            },
            if self.end < other.end {
                self.end
            } else {
                other.end
            },
        )
    }
}

/// Frequency extent in Hz. Ordering and hashing preserve exact IEEE bits.
#[derive(Clone, Copy, Debug)]
pub struct BandSpan {
    pub min_hz: f32,
    pub max_hz: f32,
}

impl BandSpan {
    pub fn new(min_hz: f32, max_hz: f32) -> Option<Self> {
        (min_hz.is_finite() && max_hz.is_finite() && min_hz >= 0.0 && min_hz < max_hz)
            .then_some(Self { min_hz, max_hz })
    }

    pub fn intersect(self, other: Self) -> Option<Self> {
        Self::new(self.min_hz.max(other.min_hz), self.max_hz.min(other.max_hz))
    }
}

impl PartialEq for BandSpan {
    fn eq(&self, other: &Self) -> bool {
        self.min_hz.to_bits() == other.min_hz.to_bits()
            && self.max_hz.to_bits() == other.max_hz.to_bits()
    }
}

impl Eq for BandSpan {}

impl PartialOrd for BandSpan {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BandSpan {
    fn cmp(&self, other: &Self) -> Ordering {
        self.min_hz
            .total_cmp(&other.min_hz)
            .then_with(|| self.max_hz.total_cmp(&other.max_hz))
    }
}

impl Hash for BandSpan {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.min_hz.to_bits().hash(state);
        self.max_hz.to_bits().hash(state);
    }
}

/// Bitset over source channels; bit 0 is channel 0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelMask(pub u16);

impl ChannelMask {
    pub const fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// Identity of the content-addressed analysis run a family selector names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AnalysisRef {
    pub source: u64,
    pub recipe: u64,
}

/// A durable explanation reference. Proposal references preserve the current
/// reconstruction bridge; definition/comparison references are the stable
/// project identities used by the interpretation substrate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExplanationRef {
    Definition(u64),
    Proposal(ReconstructionProposalId),
    Comparison(u64),
}

/// Which signal is selected inside otherwise identical geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SignalLayer {
    Source,
    Explanation(ExplanationRef),
    Residual(ExplanationRef),
}

impl Default for SignalLayer {
    fn default() -> Self {
        Self::Source
    }
}

/// A pure selection term. Empty is explicit so normalization is total.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Aspect {
    Empty,
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

/// One rectangular region in project-frame, Hz, and channel coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConcreteRegion {
    pub time: FrameSpan,
    pub band: BandSpan,
    pub channels: ChannelMask,
}

impl ConcreteRegion {
    pub fn intersect(self, other: Self) -> Option<Self> {
        let channels = self.channels.intersect(other.channels);
        if channels.is_empty() {
            return None;
        }
        Some(Self {
            time: self.time.intersect(other.time)?,
            band: self.band.intersect(other.band)?,
            channels,
        })
    }

    fn is_valid(self) -> bool {
        self.time.start < self.time.end
            && BandSpan::new(self.band.min_hz, self.band.max_hz).is_some()
            && !self.channels.is_empty()
    }
}

/// Evaluated selection. `regions` is an ordered union, not a Cartesian
/// product. `objects` records which object selectors contributed to the
/// resolution; it is provenance, not another geometric axis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConcreteAspect {
    pub regions: Vec<ConcreteRegion>,
    pub signal: SignalLayer,
    pub objects: Vec<ontology::ObjectId>,
}

impl Default for ConcreteAspect {
    fn default() -> Self {
        Self {
            regions: Vec::new(),
            signal: SignalLayer::Source,
            objects: Vec::new(),
        }
    }
}

impl ConcreteAspect {
    pub fn new(regions: Vec<ConcreteRegion>, signal: SignalLayer) -> Result<Self, AspectError> {
        if let Some(region) = regions.iter().copied().find(|region| !region.is_valid()) {
            return Err(AspectError::InvalidRegion(region));
        }
        let mut value = Self {
            regions,
            signal,
            objects: Vec::new(),
        };
        normalize_concrete(&mut value);
        Ok(value)
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }
}

/// Read-only resolution of project-scoped references.
pub trait AspectResolver {
    fn universe(&self) -> ConcreteAspect;
    fn family_spans(&self, analysis: &AnalysisRef, id: usize) -> Option<Vec<FrameSpan>>;
    fn object_extent(&self, object: ontology::ObjectId) -> Option<ConcreteAspect>;
    fn explanation_extent(&self, reference: &ExplanationRef) -> Option<ConcreteAspect>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AspectError {
    Unresolvable(String),
    InvalidSpan {
        start: i64,
        end: i64,
    },
    InvalidBand {
        min_bits: u32,
        max_bits: u32,
    },
    InvalidRegion(ConcreteRegion),
    InvalidUniverse(String),
    IncompatibleSignalLayers {
        left: SignalLayer,
        right: SignalLayer,
    },
}

impl fmt::Display for AspectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unresolvable(what) => write!(formatter, "unresolvable aspect reference: {what}"),
            Self::InvalidSpan { start, end } => write!(formatter, "invalid span {start}..{end}"),
            Self::InvalidBand { min_bits, max_bits } => write!(
                formatter,
                "invalid frequency band with IEEE bits {min_bits:#x}..{max_bits:#x}"
            ),
            Self::InvalidRegion(region) => write!(formatter, "invalid concrete region {region:?}"),
            Self::InvalidUniverse(message) => {
                write!(formatter, "invalid aspect universe: {message}")
            }
            Self::IncompatibleSignalLayers { left, right } => {
                write!(
                    formatter,
                    "cannot combine signal layers {left:?} and {right:?}"
                )
            }
        }
    }
}

impl std::error::Error for AspectError {}

/// Canonical structural form. Geometry that depends on a resolver is
/// normalized later by [`evaluate`].
pub fn normalize(aspect: Aspect) -> Aspect {
    match aspect {
        Aspect::Union(children) => normalize_union(children),
        Aspect::Intersect(children) => normalize_intersection(children),
        Aspect::Complement(child) => match normalize(*child) {
            Aspect::Empty => Aspect::All,
            Aspect::All => Aspect::Empty,
            Aspect::Complement(grandchild) => *grandchild,
            child => Aspect::Complement(Box::new(child)),
        },
        other => other,
    }
}

fn normalize_union(children: Vec<Aspect>) -> Aspect {
    let mut flat = Vec::new();
    for child in children.into_iter().map(normalize) {
        match child {
            Aspect::All => return Aspect::All,
            Aspect::Empty => {}
            Aspect::Union(nested) => flat.extend(nested),
            child => flat.push(child),
        }
    }
    flat.sort();
    flat.dedup();
    absorb_intersections(&mut flat);
    match flat.len() {
        0 => Aspect::Empty,
        1 => flat.pop().expect("one child"),
        _ => Aspect::Union(flat),
    }
}

fn normalize_intersection(children: Vec<Aspect>) -> Aspect {
    let mut flat = Vec::new();
    for child in children.into_iter().map(normalize) {
        match child {
            Aspect::Empty => return Aspect::Empty,
            Aspect::All => {}
            Aspect::Intersect(nested) => flat.extend(nested),
            child => flat.push(child),
        }
    }
    flat.sort();
    flat.dedup();
    absorb_unions(&mut flat);
    match flat.len() {
        0 => Aspect::All,
        1 => flat.pop().expect("one child"),
        _ => Aspect::Intersect(flat),
    }
}

fn absorb_intersections(children: &mut Vec<Aspect>) {
    let atoms = children.clone();
    children.retain(|candidate| {
        !matches!(candidate, Aspect::Intersect(parts) if parts.iter().any(|part| atoms.contains(part)))
    });
}

fn absorb_unions(children: &mut Vec<Aspect>) {
    let atoms = children.clone();
    children.retain(|candidate| {
        !matches!(candidate, Aspect::Union(parts) if parts.iter().any(|part| atoms.contains(part)))
    });
}

pub fn evaluate(
    aspect: &Aspect,
    resolver: &dyn AspectResolver,
) -> Result<ConcreteAspect, AspectError> {
    let mut universe = resolver.universe();
    validate_universe(&universe)?;
    normalize_concrete(&mut universe);
    let value = evaluate_inner(&normalize(aspect.clone()), resolver, &universe)?;
    let mut result = value.aspect;
    result.signal = value.layer.unwrap_or(SignalLayer::Source);
    normalize_concrete(&mut result);
    Ok(result)
}

#[derive(Clone)]
struct Evaluated {
    aspect: ConcreteAspect,
    /// None means geometry-only and can be applied to any one signal layer.
    layer: Option<SignalLayer>,
}

fn evaluate_inner(
    aspect: &Aspect,
    resolver: &dyn AspectResolver,
    universe: &ConcreteAspect,
) -> Result<Evaluated, AspectError> {
    match aspect {
        Aspect::Empty => Ok(geometry_only(ConcreteAspect::default())),
        Aspect::All => Ok(geometry_only(universe.clone())),
        Aspect::Time(span) => {
            if span.start >= span.end {
                return Err(AspectError::InvalidSpan {
                    start: span.start,
                    end: span.end,
                });
            }
            Ok(geometry_only(filter_regions(universe, |region| {
                region
                    .time
                    .intersect(*span)
                    .map(|time| ConcreteRegion { time, ..region })
            })))
        }
        Aspect::Band(band) => {
            if BandSpan::new(band.min_hz, band.max_hz).is_none() {
                return Err(AspectError::InvalidBand {
                    min_bits: band.min_hz.to_bits(),
                    max_bits: band.max_hz.to_bits(),
                });
            }
            Ok(geometry_only(filter_regions(universe, |region| {
                region
                    .band
                    .intersect(*band)
                    .map(|band| ConcreteRegion { band, ..region })
            })))
        }
        Aspect::Channels(channels) => Ok(geometry_only(filter_regions(universe, |region| {
            let channels = region.channels.intersect(*channels);
            (!channels.is_empty()).then_some(ConcreteRegion { channels, ..region })
        }))),
        Aspect::Family { analysis, id } => {
            let spans = resolver.family_spans(analysis, *id).ok_or_else(|| {
                AspectError::Unresolvable(format!("analysis {analysis:?} family {id}"))
            })?;
            evaluate_inner(
                &Aspect::Union(spans.into_iter().map(Aspect::Time).collect()),
                resolver,
                universe,
            )
        }
        Aspect::Object(object) => {
            let mut extent = resolver
                .object_extent(*object)
                .ok_or_else(|| AspectError::Unresolvable(format!("AIR object {object}")))?;
            validate_resolved(&extent)?;
            extent = intersect_concrete(&extent, universe);
            extent.objects.push(*object);
            extent.objects.sort();
            extent.objects.dedup();
            Ok(geometry_only(extent))
        }
        Aspect::ExplainedBy(reference) | Aspect::ResidualOf(reference) => {
            let mut extent = resolver
                .explanation_extent(reference)
                .ok_or_else(|| AspectError::Unresolvable(format!("explanation {reference:?}")))?;
            validate_resolved(&extent)?;
            extent = intersect_concrete(&extent, universe);
            let layer = if matches!(aspect, Aspect::ExplainedBy(_)) {
                SignalLayer::Explanation(*reference)
            } else {
                SignalLayer::Residual(*reference)
            };
            extent.signal = layer;
            Ok(Evaluated {
                aspect: extent,
                layer: Some(layer),
            })
        }
        Aspect::Union(children) => {
            let mut output = ConcreteAspect::default();
            let mut layer = None;
            for child in children {
                let value = evaluate_inner(child, resolver, universe)?;
                layer = merge_layers(layer, value.layer)?;
                output.regions.extend(value.aspect.regions);
                output.objects.extend(value.aspect.objects);
            }
            normalize_concrete(&mut output);
            Ok(Evaluated {
                aspect: output,
                layer,
            })
        }
        Aspect::Intersect(children) => {
            let mut output = universe.clone();
            let mut layer = None;
            for child in children {
                let value = evaluate_inner(child, resolver, universe)?;
                layer = merge_layers(layer, value.layer)?;
                output = intersect_concrete(&output, &value.aspect);
            }
            Ok(Evaluated {
                aspect: output,
                layer,
            })
        }
        Aspect::Complement(child) => {
            let value = evaluate_inner(child, resolver, universe)?;
            let mut output = subtract_concrete(universe, &value.aspect);
            output.objects.clear();
            Ok(Evaluated {
                aspect: output,
                layer: value.layer,
            })
        }
    }
}

fn geometry_only(mut aspect: ConcreteAspect) -> Evaluated {
    aspect.signal = SignalLayer::Source;
    Evaluated {
        aspect,
        layer: None,
    }
}

fn merge_layers(
    left: Option<SignalLayer>,
    right: Option<SignalLayer>,
) -> Result<Option<SignalLayer>, AspectError> {
    match (left, right) {
        (None, layer) | (layer, None) => Ok(layer),
        (Some(left), Some(right)) if left == right => Ok(Some(left)),
        (Some(left), Some(right)) => Err(AspectError::IncompatibleSignalLayers { left, right }),
    }
}

fn validate_universe(value: &ConcreteAspect) -> Result<(), AspectError> {
    validate_resolved(value)?;
    if value.regions.is_empty() {
        return Err(AspectError::InvalidUniverse("contains no regions".into()));
    }
    Ok(())
}

fn validate_resolved(value: &ConcreteAspect) -> Result<(), AspectError> {
    if let Some(region) = value
        .regions
        .iter()
        .copied()
        .find(|region| !region.is_valid())
    {
        return Err(AspectError::InvalidRegion(region));
    }
    Ok(())
}

fn filter_regions(
    source: &ConcreteAspect,
    mut filter: impl FnMut(ConcreteRegion) -> Option<ConcreteRegion>,
) -> ConcreteAspect {
    let mut result = ConcreteAspect {
        regions: source
            .regions
            .iter()
            .copied()
            .filter_map(&mut filter)
            .collect(),
        signal: source.signal,
        objects: source.objects.clone(),
    };
    normalize_concrete(&mut result);
    result
}

fn intersect_concrete(left: &ConcreteAspect, right: &ConcreteAspect) -> ConcreteAspect {
    let mut regions = Vec::new();
    for left_region in &left.regions {
        for right_region in &right.regions {
            if let Some(region) = left_region.intersect(*right_region) {
                regions.push(region);
            }
        }
    }
    let mut objects = left.objects.clone();
    objects.extend(&right.objects);
    let mut result = ConcreteAspect {
        regions,
        signal: left.signal,
        objects,
    };
    normalize_concrete(&mut result);
    result
}

fn subtract_concrete(left: &ConcreteAspect, right: &ConcreteAspect) -> ConcreteAspect {
    let mut regions = left.regions.clone();
    for remove in &right.regions {
        regions = regions
            .into_iter()
            .flat_map(|region| subtract_region(region, *remove))
            .collect();
    }
    let mut result = ConcreteAspect {
        regions,
        signal: left.signal,
        objects: left.objects.clone(),
    };
    normalize_concrete(&mut result);
    result
}

/// Disjoint rectangular difference in time, frequency, then channel slabs.
fn subtract_region(source: ConcreteRegion, remove: ConcreteRegion) -> Vec<ConcreteRegion> {
    let Some(overlap) = source.intersect(remove) else {
        return vec![source];
    };
    let mut out = Vec::with_capacity(5);
    if source.time.start < overlap.time.start {
        out.push(ConcreteRegion {
            time: FrameSpan {
                start: source.time.start,
                end: overlap.time.start,
            },
            ..source
        });
    }
    if overlap.time.end < source.time.end {
        out.push(ConcreteRegion {
            time: FrameSpan {
                start: overlap.time.end,
                end: source.time.end,
            },
            ..source
        });
    }
    let middle_time = overlap.time;
    if source.band.min_hz < overlap.band.min_hz {
        out.push(ConcreteRegion {
            time: middle_time,
            band: BandSpan {
                min_hz: source.band.min_hz,
                max_hz: overlap.band.min_hz,
            },
            channels: source.channels,
        });
    }
    if overlap.band.max_hz < source.band.max_hz {
        out.push(ConcreteRegion {
            time: middle_time,
            band: BandSpan {
                min_hz: overlap.band.max_hz,
                max_hz: source.band.max_hz,
            },
            channels: source.channels,
        });
    }
    let remaining_channels = ChannelMask(source.channels.0 & !overlap.channels.0);
    if !remaining_channels.is_empty() {
        out.push(ConcreteRegion {
            time: middle_time,
            band: overlap.band,
            channels: remaining_channels,
        });
    }
    out
}

fn normalize_concrete(value: &mut ConcreteAspect) {
    value.regions.retain(|region| region.is_valid());
    value.regions.sort();
    value.regions.dedup();
    value.objects.sort();
    value.objects.dedup();

    loop {
        let mut merged = false;
        'outer: for left in 0..value.regions.len() {
            for right in left + 1..value.regions.len() {
                if let Some(region) = merge_regions(value.regions[left], value.regions[right]) {
                    value.regions[left] = region;
                    value.regions.remove(right);
                    merged = true;
                    break 'outer;
                }
            }
        }
        if !merged {
            break;
        }
        value.regions.sort();
        value.regions.dedup();
    }
}

fn merge_regions(left: ConcreteRegion, right: ConcreteRegion) -> Option<ConcreteRegion> {
    if left.band == right.band
        && left.channels == right.channels
        && left.time.end >= right.time.start
        && right.time.end >= left.time.start
    {
        return Some(ConcreteRegion {
            time: FrameSpan {
                start: left.time.start.min(right.time.start),
                end: left.time.end.max(right.time.end),
            },
            ..left
        });
    }
    if left.time == right.time
        && left.channels == right.channels
        && left.band.max_hz >= right.band.min_hz
        && right.band.max_hz >= left.band.min_hz
    {
        return Some(ConcreteRegion {
            band: BandSpan {
                min_hz: left.band.min_hz.min(right.band.min_hz),
                max_hz: left.band.max_hz.max(right.band.max_hz),
            },
            ..left
        });
    }
    if left.time == right.time && left.band == right.band {
        return Some(ConcreteRegion {
            channels: left.channels.union(right.channels),
            ..left
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Resolver;

    fn region(start: i64, end: i64, low: f32, high: f32, channels: u16) -> ConcreteRegion {
        ConcreteRegion {
            time: FrameSpan { start, end },
            band: BandSpan {
                min_hz: low,
                max_hz: high,
            },
            channels: ChannelMask(channels),
        }
    }

    impl AspectResolver for Resolver {
        fn universe(&self) -> ConcreteAspect {
            ConcreteAspect::new(
                vec![region(0, 100, 0.0, 24_000.0, 0b11)],
                SignalLayer::Source,
            )
            .unwrap()
        }

        fn family_spans(&self, _: &AnalysisRef, id: usize) -> Option<Vec<FrameSpan>> {
            (id == 7).then_some(vec![FrameSpan { start: 10, end: 20 }])
        }

        fn object_extent(&self, object: ontology::ObjectId) -> Option<ConcreteAspect> {
            (object == ontology::ObjectId::new(9)).then(|| {
                ConcreteAspect::new(
                    vec![region(30, 40, 100.0, 1_000.0, 0b01)],
                    SignalLayer::Source,
                )
                .unwrap()
            })
        }

        fn explanation_extent(&self, _: &ExplanationRef) -> Option<ConcreteAspect> {
            ConcreteAspect::new(
                vec![region(20, 80, 0.0, 24_000.0, 0b11)],
                SignalLayer::Source,
            )
            .ok()
        }
    }

    #[test]
    fn normalization_is_order_independent_idempotent_and_absorbing() {
        let a = Aspect::Time(FrameSpan { start: 1, end: 2 });
        let b = Aspect::Band(BandSpan {
            min_hz: 100.0,
            max_hz: 200.0,
        });
        let left = normalize(Aspect::Union(vec![
            Aspect::Intersect(vec![a.clone(), b.clone()]),
            a.clone(),
        ]));
        let right = normalize(Aspect::Union(vec![b, a.clone()]));
        assert_eq!(left, a);
        assert_eq!(normalize(right.clone()), right);
    }

    #[test]
    fn union_is_regions_not_a_cartesian_product() {
        let result = evaluate(
            &Aspect::Union(vec![
                Aspect::Intersect(vec![
                    Aspect::Time(FrameSpan { start: 0, end: 10 }),
                    Aspect::Band(BandSpan {
                        min_hz: 0.0,
                        max_hz: 100.0,
                    }),
                ]),
                Aspect::Intersect(vec![
                    Aspect::Time(FrameSpan {
                        start: 90,
                        end: 100,
                    }),
                    Aspect::Band(BandSpan {
                        min_hz: 10_000.0,
                        max_hz: 20_000.0,
                    }),
                ]),
            ]),
            &Resolver,
        )
        .unwrap();
        assert_eq!(result.regions.len(), 2);
        assert!(!result
            .regions
            .contains(&region(0, 10, 10_000.0, 20_000.0, 0b11)));
    }

    #[test]
    fn complement_splits_geometry_without_leaking_the_removed_center() {
        let result = evaluate(
            &Aspect::Complement(Box::new(Aspect::Intersect(vec![
                Aspect::Time(FrameSpan { start: 25, end: 75 }),
                Aspect::Band(BandSpan {
                    min_hz: 1_000.0,
                    max_hz: 2_000.0,
                }),
                Aspect::Channels(ChannelMask(0b01)),
            ]))),
            &Resolver,
        )
        .unwrap();
        assert!(result.regions.iter().all(|candidate| {
            candidate
                .intersect(region(25, 75, 1_000.0, 2_000.0, 0b01))
                .is_none()
        }));
        assert!(!result.is_empty());
    }

    #[test]
    fn signal_layer_is_separate_and_conflicts_are_typed() {
        let reference = ExplanationRef::Definition(4);
        let explained = evaluate(
            &Aspect::Intersect(vec![
                Aspect::Time(FrameSpan { start: 25, end: 30 }),
                Aspect::ExplainedBy(reference),
            ]),
            &Resolver,
        )
        .unwrap();
        assert_eq!(explained.signal, SignalLayer::Explanation(reference));
        assert_eq!(explained.regions, vec![region(25, 30, 0.0, 24_000.0, 0b11)]);

        let error = evaluate(
            &Aspect::Union(vec![
                Aspect::ExplainedBy(reference),
                Aspect::ResidualOf(reference),
            ]),
            &Resolver,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            AspectError::IncompatibleSignalLayers { .. }
        ));
    }
}
