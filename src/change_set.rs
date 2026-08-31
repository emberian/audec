//! Typed, normalized invalidation produced by aggregate project commands.
//!
//! A change set is a conservative statement about consequences, not a claim
//! that unchanged regions are perceptually irrelevant. Producers may widen an
//! impact to a whole bus whenever dependency resolution is incomplete; they
//! must never omit an audible consequence. This module normalizes and unions
//! impacts but deliberately does not infer them from domain commands—the
//! convergence layer has the required before/after project states and typed
//! bindings.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::daw_project::ProjectDomain;
use crate::mixer::BusId;

/// A non-empty, half-open range in signed project frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AudioRange {
    pub start: i64,
    pub end: i64,
}

impl AudioRange {
    pub fn new(start: i64, end: i64) -> Result<Self, InvalidAudioRange> {
        if start >= end {
            return Err(InvalidAudioRange { start, end });
        }
        Ok(Self { start, end })
    }

    pub fn intersects_or_touches(self, other: Self) -> bool {
        self.start <= other.end && other.start <= self.end
    }

    pub fn union(self, other: Self) -> Option<Self> {
        self.intersects_or_touches(other).then(|| Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidAudioRange {
    pub start: i64,
    pub end: i64,
}

impl fmt::Display for InvalidAudioRange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "audio invalidation range {}..{} is empty or reversed",
            self.start, self.end
        )
    }
}

impl Error for InvalidAudioRange {}

/// Audio invalidation for one mixer bus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusImpact {
    /// Every frame on the bus may have changed.
    Whole,
    /// Sorted, non-overlapping, non-adjacent half-open frame ranges.
    Ranges(Vec<AudioRange>),
}

impl BusImpact {
    pub fn ranges(ranges: impl IntoIterator<Item = AudioRange>) -> Self {
        Self::Ranges(normalize_ranges(ranges))
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Ranges(ranges) if ranges.is_empty())
    }

    pub fn merge(&mut self, other: &Self) {
        match (&mut *self, other) {
            (Self::Whole, _) => {}
            (slot, Self::Whole) => *slot = Self::Whole,
            (Self::Ranges(left), Self::Ranges(right)) => {
                left.extend(right.iter().copied());
                *left = normalize_ranges(left.drain(..));
            }
        }
    }

    pub fn covers(&self, range: AudioRange) -> bool {
        match self {
            Self::Whole => true,
            Self::Ranges(ranges) => ranges
                .iter()
                .any(|candidate| candidate.start <= range.start && candidate.end >= range.end),
        }
    }
}

/// Cache/tile invalidation at aggregate-command granularity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeSet {
    pub domains: BTreeSet<ProjectDomain>,
    pub audio: BTreeMap<BusId, BusImpact>,
    /// A structural routing change may affect every downstream bus. The tile
    /// dependency resolver expands this flag from the before/after graphs.
    pub routing_changed: bool,
}

impl ChangeSet {
    pub fn touch(&mut self, domain: ProjectDomain) -> &mut Self {
        self.domains.insert(domain);
        self
    }

    pub fn invalidate_range(&mut self, bus: BusId, range: AudioRange) -> &mut Self {
        self.audio
            .entry(bus)
            .and_modify(|impact| impact.merge(&BusImpact::Ranges(vec![range])))
            .or_insert_with(|| BusImpact::Ranges(vec![range]));
        self
    }

    pub fn invalidate_ranges(
        &mut self,
        bus: BusId,
        ranges: impl IntoIterator<Item = AudioRange>,
    ) -> &mut Self {
        let incoming = BusImpact::ranges(ranges);
        if incoming.is_empty() {
            return self;
        }
        self.audio
            .entry(bus)
            .and_modify(|impact| impact.merge(&incoming))
            .or_insert(incoming);
        self
    }

    pub fn invalidate_bus(&mut self, bus: BusId) -> &mut Self {
        self.audio.insert(bus, BusImpact::Whole);
        self
    }

    pub fn mark_routing_changed(&mut self) -> &mut Self {
        self.routing_changed = true;
        self
    }

    pub fn merge(&mut self, other: &Self) {
        self.domains.extend(other.domains.iter().copied());
        self.routing_changed |= other.routing_changed;
        for (&bus, incoming) in &other.audio {
            self.audio
                .entry(bus)
                .and_modify(|impact| impact.merge(incoming))
                .or_insert_with(|| incoming.clone());
        }
    }

    pub fn union(mut self, other: &Self) -> Self {
        self.merge(other);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.domains.is_empty() && self.audio.is_empty() && !self.routing_changed
    }
}

fn normalize_ranges(ranges: impl IntoIterator<Item = AudioRange>) -> Vec<AudioRange> {
    let mut ranges = ranges.into_iter().collect::<Vec<_>>();
    ranges.sort_unstable();
    let mut normalized: Vec<AudioRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(last) = normalized.last_mut() {
            if let Some(merged) = last.union(range) {
                *last = merged;
                continue;
            }
        }
        normalized.push(range);
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: i64, end: i64) -> AudioRange {
        AudioRange::new(start, end).unwrap()
    }

    #[test]
    fn range_normalization_sorts_and_merges_overlap_and_adjacency() {
        let impact = BusImpact::ranges([
            range(20, 30),
            range(-5, 0),
            range(10, 20),
            range(12, 18),
            range(1, 4),
        ]);
        assert_eq!(
            impact,
            BusImpact::Ranges(vec![range(-5, 0), range(1, 4), range(10, 30)])
        );
    }

    #[test]
    fn whole_bus_dominates_range_impacts() {
        let bus = BusId::from_raw(7);
        let mut changes = ChangeSet::default();
        changes.invalidate_range(bus, range(3, 9));
        changes.invalidate_bus(bus);
        changes.invalidate_range(bus, range(100, 200));
        assert_eq!(changes.audio[&bus], BusImpact::Whole);
    }

    #[test]
    fn union_is_commutative_after_normalization() {
        let bus = BusId::from_raw(3);
        let mut left = ChangeSet::default();
        left.touch(ProjectDomain::Arrangement)
            .invalidate_range(bus, range(0, 8));
        let mut right = ChangeSet::default();
        right
            .touch(ProjectDomain::Sequencer)
            .invalidate_range(bus, range(8, 16));
        assert_eq!(left.clone().union(&right), right.clone().union(&left));
        assert_eq!(
            left.union(&right).audio[&bus],
            BusImpact::Ranges(vec![range(0, 16)])
        );
    }
}
