//! Persistent sample-kit identities and routing intent.
//!
//! This module records which exact source material a pad addresses and where
//! the kit intends to route. It deliberately does not decode PCM, render DSP,
//! infer instrument names, acquire locks, or own UI state.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::assets::AssetFrameRange;
use crate::instruments::{SampleEnvelope, SampleLoopMode};
use crate::mixer::BusId;
use crate::sample_material::{
    CanonicalPcmIdentity, SampleMaterialProvenance, ScopedEvidenceRef, SourceMaterialRef,
};

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

/// A sustaining region inside a zone's material, in *source asset* frames —
/// the same coordinate space as the zone's material — so a loop survives a
/// trim that re-derives the slice and can be shown next to the range bar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SampleLoop {
    pub range: AssetFrameRange,
    pub mode: SampleLoopMode,
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
    /// Absent means the zone plays once to the end of its material.
    pub loop_region: Option<SampleLoop>,
    /// Amplitude shape applied by the sampler voice. The default is the
    /// pass-through gate, which is what an unshaped zone has always rendered.
    pub envelope: SampleEnvelope,
    /// Canonical decoded identity expected from `material`. This does not
    /// embed PCM and is never sufficient to authorize reuse without an exact
    /// comparison by `sample_material`.
    pub decoded_pcm: Option<CanonicalPcmIdentity>,
    pub provenance: SampleMaterialProvenance,
    pub evidence: BTreeSet<ScopedEvidenceRef>,
}

impl SampleZone {
    pub fn new(id: ZoneId, pad: PadId, material: SourceMaterialRef) -> Self {
        let provenance = match material {
            SourceMaterialRef::Asset(_) => SampleMaterialProvenance::ExistingAsset,
            SourceMaterialRef::VirtualSlice(_) => SampleMaterialProvenance::ManualSelection,
        };
        Self {
            id,
            pad,
            material,
            gain_db: 0.0,
            pan: 0.0,
            tuning_cents: 0.0,
            loop_region: None,
            envelope: SampleEnvelope::default(),
            decoded_pcm: None,
            provenance,
            evidence: BTreeSet::new(),
        }
    }

    /// The half-open source range this zone reads, in source-asset frames. A
    /// whole-asset zone has no stored bound here; only a slice does.
    pub fn material_range(&self) -> Option<AssetFrameRange> {
        self.material
            .virtual_slice()
            .map(|slice| slice.source_range)
    }

    /// A loop is legal when it is non-empty and, for a sliced zone, lies
    /// inside the slice the voice will actually read.
    pub fn loop_is_within_material(&self) -> bool {
        let Some(region) = self.loop_region else {
            return true;
        };
        if region.range.start >= region.range.end {
            return false;
        }
        self.material_range().is_none_or(|material| {
            region.range.start >= material.start && region.range.end <= material.end
        })
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
                || zone.provenance.validate_for(zone.material).is_err()
                || !valid_db(zone.gain_db)
                || !(-1.0..=1.0).contains(&zone.pan)
                || !zone.tuning_cents.is_finite()
                || !(-9_600.0..=9_600.0).contains(&zone.tuning_cents)
                || !zone.loop_is_within_material()
                || !zone.envelope.is_valid()
                || zone.evidence.iter().any(|id| id.local == 0)
                || zone.decoded_pcm.is_some_and(|identity| {
                    zone.material
                        .virtual_slice()
                        .is_some_and(|slice| identity.frame_count != slice.frame_count())
                })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{AssetId, SampleFrames};
    use crate::sample_material::VirtualSliceRef;

    fn sliced_kit(start: u64, end: u64) -> (SampleKit, ZoneId) {
        let slice = VirtualSliceRef::new(
            AssetId(3),
            AssetFrameRange::new(SampleFrames(start), SampleFrames(end)).unwrap(),
        )
        .unwrap();
        let mut kit = SampleKit::new(
            KitId::from_raw(1),
            "kit",
            SampleRouteIntent::new(BusId::from_raw(1)).unwrap(),
        );
        let pad = PadId::from_raw(1);
        let zone = ZoneId::from_raw(1);
        let mut sample_pad = SamplePad::new(pad, "Pad 1");
        sample_pad.zone_order.push(zone);
        kit.pads.insert(pad, sample_pad);
        kit.pad_order.push(pad);
        kit.zones.insert(
            zone,
            SampleZone::new(zone, pad, SourceMaterialRef::VirtualSlice(slice)),
        );
        (kit, zone)
    }

    #[test]
    fn a_new_zone_plays_once_with_no_shaping() {
        let (kit, zone) = sliced_kit(100, 200);
        let zone = &kit.zones[&zone];
        assert!(zone.loop_region.is_none());
        assert!(zone.envelope.is_passthrough());
        assert!(kit.validate().is_ok());
    }

    #[test]
    fn a_loop_must_lie_inside_the_zone_material() {
        let (mut kit, zone) = sliced_kit(100, 200);
        kit.zones.get_mut(&zone).unwrap().loop_region = Some(SampleLoop {
            range: AssetFrameRange::new(SampleFrames(120), SampleFrames(180)).unwrap(),
            mode: SampleLoopMode::Forward,
        });
        assert!(kit.validate().is_ok());

        kit.zones.get_mut(&zone).unwrap().loop_region = Some(SampleLoop {
            range: AssetFrameRange::new(SampleFrames(120), SampleFrames(240)).unwrap(),
            mode: SampleLoopMode::Forward,
        });
        assert_eq!(kit.validate(), Err(SampleKitError::InvalidZone(zone)));

        kit.zones.get_mut(&zone).unwrap().loop_region = Some(SampleLoop {
            range: AssetFrameRange::new(SampleFrames(40), SampleFrames(150)).unwrap(),
            mode: SampleLoopMode::PingPong,
        });
        assert_eq!(kit.validate(), Err(SampleKitError::InvalidZone(zone)));
    }

    #[test]
    fn an_envelope_sustain_outside_zero_to_one_is_refused() {
        let (mut kit, zone) = sliced_kit(0, 64);
        kit.zones.get_mut(&zone).unwrap().envelope = SampleEnvelope {
            sustain: 1.5,
            ..SampleEnvelope::percussive()
        };
        assert_eq!(kit.validate(), Err(SampleKitError::InvalidZone(zone)));

        kit.zones.get_mut(&zone).unwrap().envelope = SampleEnvelope::percussive();
        assert!(kit.validate().is_ok());
    }
}
