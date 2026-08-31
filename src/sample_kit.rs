//! Persistent sample-kit identities and routing intent.
//!
//! This module records which exact source material a pad addresses and where
//! the kit intends to route. It deliberately does not decode PCM, render DSP,
//! infer instrument names, acquire locks, or own UI state.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::mixer::BusId;
use crate::sample_material::SourceMaterialRef;

macro_rules! typed_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            pub const fn from_raw(raw: u64) -> Self {
                Self(raw)
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

typed_id!(KitId);
typed_id!(PadId);
typed_id!(ZoneId);
typed_id!(SampleProvenanceRef);
typed_id!(SampleEvidenceRef);

/// A stable logical target that a project bridge may resolve to a
/// `sequencer::TriggerTarget::Sample` alias. It intentionally contains no
/// runtime instrument or PCM identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SampleTargetRef {
    pub kit: KitId,
    pub pad: PadId,
    pub zone: ZoneId,
}

/// Explicit output intent. Bus existence and kind are aggregate-project
/// concerns; this domain only rejects the reserved zero identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleRouteIntent {
    pub bus: BusId,
}

impl SampleRouteIntent {
    pub fn new(bus: BusId) -> Result<Self, SampleKitError> {
        if bus.get() == 0 {
            return Err(SampleKitError::ZeroBusId);
        }
        Ok(Self { bus })
    }
}

/// One exact source-material zone. Evidence references remain opaque and do
/// not assert that an anonymous recurrence family is a physical instrument.
#[derive(Clone, Debug, PartialEq)]
pub struct SampleZone {
    pub id: ZoneId,
    pub pad: PadId,
    pub material: SourceMaterialRef,
    pub gain_db: f32,
    pub pan: f32,
    pub tuning_cents: f32,
    pub provenance: Option<SampleProvenanceRef>,
    pub evidence: BTreeSet<SampleEvidenceRef>,
}

impl SampleZone {
    pub fn new(id: ZoneId, pad: PadId, material: SourceMaterialRef) -> Self {
        Self {
            id,
            pad,
            material,
            gain_db: 0.0,
            pan: 0.0,
            tuning_cents: 0.0,
            provenance: None,
            evidence: BTreeSet::new(),
        }
    }
}

/// A pad is an ordered collection of zones. Empty pads are valid so a kit can
/// be authored before material is dropped onto it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SamplePad {
    pub id: PadId,
    pub name: String,
    pub choke_group: Option<u32>,
    pub zone_order: Vec<ZoneId>,
}

impl SamplePad {
    pub fn new(id: PadId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            choke_group: None,
            zone_order: Vec::new(),
        }
    }
}

/// Persistable kit state. Maps give deterministic lookup; the explicit order
/// vectors preserve authored presentation and trigger priority.
#[derive(Clone, Debug, PartialEq)]
pub struct SampleKit {
    pub id: KitId,
    pub name: String,
    pub output: SampleRouteIntent,
    pub pads: BTreeMap<PadId, SamplePad>,
    pub pad_order: Vec<PadId>,
    pub zones: BTreeMap<ZoneId, SampleZone>,
    pub revision: u64,
}

impl SampleKit {
    pub fn new(id: KitId, name: impl Into<String>, output: SampleRouteIntent) -> Self {
        Self {
            id,
            name: name.into(),
            output,
            pads: BTreeMap::new(),
            pad_order: Vec::new(),
            zones: BTreeMap::new(),
            revision: 0,
        }
    }

    pub fn ordered_pads(&self) -> impl Iterator<Item = &SamplePad> {
        self.pad_order.iter().filter_map(|id| self.pads.get(id))
    }

    pub fn ordered_zones(&self, pad: PadId) -> impl Iterator<Item = &SampleZone> {
        self.pads
            .get(&pad)
            .into_iter()
            .flat_map(|pad| pad.zone_order.iter())
            .filter_map(|id| self.zones.get(id))
    }

    pub fn targets(&self) -> impl Iterator<Item = SampleTargetRef> + '_ {
        let kit = self.id;
        self.pad_order.iter().flat_map(move |pad_id| {
            self.pads.get(pad_id).into_iter().flat_map(move |pad| {
                pad.zone_order.iter().map(move |zone_id| SampleTargetRef {
                    kit,
                    pad: *pad_id,
                    zone: *zone_id,
                })
            })
        })
    }

    pub fn primary_target(&self, pad: PadId) -> Option<SampleTargetRef> {
        let zone = *self.pads.get(&pad)?.zone_order.first()?;
        Some(SampleTargetRef {
            kit: self.id,
            pad,
            zone,
        })
    }

    pub fn zone_for_target(&self, target: SampleTargetRef) -> Option<&SampleZone> {
        if target.kit != self.id {
            return None;
        }
        let zone = self.zones.get(&target.zone)?;
        (zone.pad == target.pad).then_some(zone)
    }

    pub fn validate(&self) -> Result<(), SampleKitError> {
        if self.id.get() == 0 {
            return Err(SampleKitError::ZeroKitId);
        }
        if self.name.trim().is_empty() {
            return Err(SampleKitError::EmptyKitName(self.id));
        }
        if self.output.bus.get() == 0 {
            return Err(SampleKitError::ZeroBusId);
        }
        validate_order(&self.pad_order, self.pads.keys().copied(), "pad order")?;

        for (id, pad) in &self.pads {
            if id.get() == 0 || *id != pad.id {
                return Err(SampleKitError::InvalidPad(*id));
            }
            if pad.name.trim().is_empty() {
                return Err(SampleKitError::EmptyPadName(*id));
            }
            validate_order(
                &pad.zone_order,
                self.zones
                    .values()
                    .filter(|zone| zone.pad == *id)
                    .map(|zone| zone.id),
                "pad zone order",
            )?;
        }
        for (id, zone) in &self.zones {
            if id.get() == 0 || *id != zone.id || !self.pads.contains_key(&zone.pad) {
                return Err(SampleKitError::InvalidZone(*id));
            }
            if zone.material.validate().is_err()
                || !valid_db(zone.gain_db)
                || !(-1.0..=1.0).contains(&zone.pan)
                || !zone.tuning_cents.is_finite()
                || !(-9_600.0..=9_600.0).contains(&zone.tuning_cents)
                || zone.provenance.is_some_and(|id| id.get() == 0)
                || zone.evidence.iter().any(|id| id.get() == 0)
            {
                return Err(SampleKitError::InvalidZone(*id));
            }
        }
        Ok(())
    }
}

/// Optimistically guarded whole-kit replacement. Whole-kit puts keep pad and
/// zone publication atomic while remaining easy for constructive planners to
/// aggregate and invert.
#[derive(Clone, Debug, PartialEq)]
pub struct SampleKitPut {
    pub before: Option<SampleKit>,
    pub after: Option<SampleKit>,
}

impl SampleKitPut {
    pub fn inverse(&self) -> Self {
        Self {
            before: self.after.clone(),
            after: self.before.clone(),
        }
    }

    pub fn id(&self) -> Result<KitId, SampleKitError> {
        let before = self.before.as_ref().map(|kit| kit.id);
        let after = self.after.as_ref().map(|kit| kit.id);
        match (before, after) {
            (None, None) => Err(SampleKitError::EmptyPut),
            (Some(left), Some(right)) if left != right => Err(SampleKitError::PutIdMismatch),
            (Some(id), _) | (_, Some(id)) => Ok(id),
        }
    }
}

/// Deterministic kit collection with library-wide, monotonic identities.
#[derive(Clone, Debug, PartialEq)]
pub struct SampleKitLibrary {
    pub kits: BTreeMap<KitId, SampleKit>,
    pub next_kit_id: u64,
    pub next_pad_id: u64,
    pub next_zone_id: u64,
    pub revision: u64,
}

impl Default for SampleKitLibrary {
    fn default() -> Self {
        Self::new()
    }
}

impl SampleKitLibrary {
    pub fn new() -> Self {
        Self {
            kits: BTreeMap::new(),
            next_kit_id: 1,
            next_pad_id: 1,
            next_zone_id: 1,
            revision: 0,
        }
    }

    pub fn allocate_kit_id(&mut self) -> Result<KitId, SampleKitError> {
        allocate(&mut self.next_kit_id).map(KitId::from_raw)
    }

    pub fn allocate_pad_id(&mut self) -> Result<PadId, SampleKitError> {
        allocate(&mut self.next_pad_id).map(PadId::from_raw)
    }

    pub fn allocate_zone_id(&mut self) -> Result<ZoneId, SampleKitError> {
        allocate(&mut self.next_zone_id).map(ZoneId::from_raw)
    }

    pub fn apply_puts(&mut self, puts: &[SampleKitPut]) -> Result<u64, SampleKitError> {
        if puts.is_empty() {
            return Ok(self.revision);
        }
        let mut candidate = self.clone();
        for put in puts {
            let id = put.id()?;
            if candidate.kits.get(&id) != put.before.as_ref() {
                return Err(SampleKitError::StalePut(id));
            }
            match &put.after {
                Some(kit) => {
                    kit.validate()?;
                    advance_past(&mut candidate.next_kit_id, kit.id.get())?;
                    for pad in kit.pads.values() {
                        advance_past(&mut candidate.next_pad_id, pad.id.get())?;
                    }
                    for zone in kit.zones.values() {
                        advance_past(&mut candidate.next_zone_id, zone.id.get())?;
                    }
                    candidate.kits.insert(id, kit.clone());
                }
                None => {
                    candidate.kits.remove(&id);
                }
            }
        }
        candidate.validate()?;
        candidate.revision = self.revision.saturating_add(1);
        *self = candidate;
        Ok(self.revision)
    }

    pub fn validate(&self) -> Result<(), SampleKitError> {
        if self.next_kit_id == 0 || self.next_pad_id == 0 || self.next_zone_id == 0 {
            return Err(SampleKitError::InvalidIdCounter);
        }
        let mut pads = BTreeSet::new();
        let mut zones = BTreeSet::new();
        for (id, kit) in &self.kits {
            if *id != kit.id || id.get() >= self.next_kit_id {
                return Err(SampleKitError::InvalidKit(*id));
            }
            kit.validate()?;
            for pad in kit.pads.keys() {
                if pad.get() >= self.next_pad_id || !pads.insert(*pad) {
                    return Err(SampleKitError::DuplicatePad(*pad));
                }
            }
            for zone in kit.zones.keys() {
                if zone.get() >= self.next_zone_id || !zones.insert(*zone) {
                    return Err(SampleKitError::DuplicateZone(*zone));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SampleKitError {
    ZeroKitId,
    ZeroBusId,
    EmptyKitName(KitId),
    EmptyPadName(PadId),
    InvalidKit(KitId),
    InvalidPad(PadId),
    InvalidZone(ZoneId),
    DuplicatePad(PadId),
    DuplicateZone(ZoneId),
    InvalidOrder(&'static str),
    InvalidIdCounter,
    IdOverflow,
    EmptyPut,
    PutIdMismatch,
    StalePut(KitId),
}

impl fmt::Display for SampleKitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid sample-kit state: {self:?}")
    }
}

impl Error for SampleKitError {}

fn validate_order<T: Copy + Ord>(
    order: &[T],
    expected: impl Iterator<Item = T>,
    label: &'static str,
) -> Result<(), SampleKitError> {
    let actual: BTreeSet<_> = order.iter().copied().collect();
    let expected: BTreeSet<_> = expected.collect();
    if actual.len() != order.len() || actual != expected {
        return Err(SampleKitError::InvalidOrder(label));
    }
    Ok(())
}

fn allocate(next: &mut u64) -> Result<u64, SampleKitError> {
    if *next == 0 {
        return Err(SampleKitError::InvalidIdCounter);
    }
    let id = *next;
    *next = next.checked_add(1).ok_or(SampleKitError::IdOverflow)?;
    Ok(id)
}

fn advance_past(next: &mut u64, used: u64) -> Result<(), SampleKitError> {
    if used == 0 {
        return Err(SampleKitError::InvalidIdCounter);
    }
    *next = (*next).max(used.checked_add(1).ok_or(SampleKitError::IdOverflow)?);
    Ok(())
}

fn valid_db(value: f32) -> bool {
    value.is_finite() && (-144.0..=48.0).contains(&value)
}
