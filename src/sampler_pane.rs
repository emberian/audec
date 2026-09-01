//! UI-neutral sampler pane projection and interaction controller.
//!
//! The model stores only transient selection and correlation tokens. Pad,
//! zone, routing, and parameter values are projected from an authoritative
//! [`SampleKit`] supplied by the session adapter on every refresh.

use std::error::Error;
use std::fmt;
use std::num::NonZeroU32;

use crate::assets::AssetFrameRange;
use crate::mixer::BusId;
use crate::sample_actions::{
    CreatePatternFromPadsIntent, SampleAction, SampleAuditionIntent, SamplePublishedResult,
    SampleResultFocus, SamplerTarget, ZoneEditIntent, ZoneEditTarget,
};
use crate::sample_kit::{KitId, PadId, SampleKit, SampleTargetRef, ZoneId};
use crate::sample_material::{SampleMaterialProvenance, SourceMaterialRef};
use crate::ui_drag::{
    interpret_drop, AssetDrag, DragContractError, DragModifiers, DragPayload, DropIntent,
    DropTarget,
};

pub const SAMPLER_KEYBOARD_BANK_SIZE: usize = 16;
pub const SAMPLER_KEYBOARD_KEYS: [&str; SAMPLER_KEYBOARD_BANK_SIZE] = [
    "1", "2", "3", "4", "q", "w", "e", "r", "a", "s", "d", "f", "z", "x", "c", "v",
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SamplerPaneSelection {
    pub pad: Option<PadId>,
    pub zone: Option<ZoneId>,
    pub bank: u16,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SamplerZoneProjection {
    pub id: ZoneId,
    pub material: SourceMaterialRef,
    pub gain_db: f32,
    pub pan: f32,
    pub tuning_cents: f32,
    pub provenance: SampleMaterialProvenance,
    pub evidence_count: usize,
    pub selected: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SamplerPadProjection {
    pub id: PadId,
    pub name: String,
    pub choke_group: Option<u32>,
    pub keyboard_key: Option<&'static str>,
    pub selected: bool,
    pub zones: Vec<SamplerZoneProjection>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SamplerKitProjection {
    pub id: KitId,
    pub name: String,
    pub revision: u64,
    pub output_bus: BusId,
    pub bank: u16,
    pub bank_count: u16,
    pub pads: Vec<SamplerPadProjection>,
}

/// A sample kit is the persisted instrument object; this alias lets generic
/// instrument panes name that role without introducing a second model.
pub type SamplerInstrumentProjection = SamplerKitProjection;

impl SamplerKitProjection {
    pub fn from_authoritative(kit: &SampleKit, selection: SamplerPaneSelection) -> Self {
        let bank_start = usize::from(selection.bank).saturating_mul(SAMPLER_KEYBOARD_BANK_SIZE);
        let pads = kit
            .ordered_pads()
            .enumerate()
            .map(|(ordinal, pad)| {
                let keyboard_key = ordinal
                    .checked_sub(bank_start)
                    .filter(|offset| *offset < SAMPLER_KEYBOARD_BANK_SIZE)
                    .map(|offset| SAMPLER_KEYBOARD_KEYS[offset]);
                let zones = kit
                    .ordered_zones(pad.id)
                    .map(|zone| SamplerZoneProjection {
                        id: zone.id,
                        material: zone.material,
                        gain_db: zone.gain_db,
                        pan: zone.pan,
                        tuning_cents: zone.tuning_cents,
                        provenance: zone.provenance.clone(),
                        evidence_count: zone.evidence.len(),
                        selected: selection.zone == Some(zone.id),
                    })
                    .collect();
                SamplerPadProjection {
                    id: pad.id,
                    name: pad.name.clone(),
                    choke_group: pad.choke_group,
                    keyboard_key,
                    selected: selection.pad == Some(pad.id),
                    zones,
                }
            })
            .collect();
        let bank_count = kit
            .pad_order
            .len()
            .max(1)
            .div_ceil(SAMPLER_KEYBOARD_BANK_SIZE)
            .min(usize::from(u16::MAX)) as u16;
        Self {
            id: kit.id,
            name: kit.name.clone(),
            revision: kit.revision,
            output_bus: kit.output.bus,
            bank: selection.bank.min(bank_count.saturating_sub(1)),
            bank_count,
            pads,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChopResultSelection {
    pub kit: KitId,
    pub pads: Vec<PadId>,
    pub zones: Vec<SampleTargetRef>,
    pub selected: Option<PadId>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SamplerGateId(pub u64);

/// One exact semantic gate. The UI stores this together with the
/// `SampleAuditionTicket` returned by `SamplePaneBridge`; release therefore
/// reuses both the exact kit/pad and exact preview generation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SamplerGatePress {
    pub id: SamplerGateId,
    pub kit: KitId,
    pub pad: PadId,
    pub velocity: f32,
}

impl SamplerGatePress {
    pub const fn press_action(self) -> SampleAction {
        SampleAction::Audition(SampleAuditionIntent::PadGate {
            kit: self.kit,
            pad: self.pad,
            velocity: self.velocity,
            pressed: true,
        })
    }

    pub const fn release_action(self) -> SampleAction {
        SampleAction::Audition(SampleAuditionIntent::PadGate {
            kit: self.kit,
            pad: self.pad,
            velocity: self.velocity,
            pressed: false,
        })
    }
}

#[derive(Clone, Debug)]
pub struct SamplerPaneModel {
    target: SamplerTarget,
    selection: SamplerPaneSelection,
    next_gate_id: u64,
    chop_result: Option<ChopResultSelection>,
}

impl SamplerPaneModel {
    pub const fn new(target: SamplerTarget) -> Self {
        Self {
            target,
            selection: SamplerPaneSelection {
                pad: target.pad(),
                zone: None,
                bank: 0,
            },
            next_gate_id: 0,
            chop_result: None,
        }
    }

    pub const fn target(&self) -> SamplerTarget {
        self.target
    }

    pub const fn selection(&self) -> SamplerPaneSelection {
        self.selection
    }

    pub fn retarget(&mut self, target: SamplerTarget) {
        self.target = target;
        self.selection.pad = target.pad();
        self.selection.zone = None;
        self.selection.bank = 0;
        self.chop_result = None;
    }

    pub fn project(&mut self, kit: &SampleKit) -> Result<SamplerKitProjection, SamplerPaneError> {
        self.require_target_kit(kit)?;
        self.reconcile(kit);
        Ok(SamplerKitProjection::from_authoritative(
            kit,
            self.selection,
        ))
    }

    pub fn select_pad(&mut self, kit: &SampleKit, pad: PadId) -> Result<(), SamplerPaneError> {
        self.require_target_kit(kit)?;
        let pad = kit
            .pads
            .get(&pad)
            .ok_or(SamplerPaneError::MissingPad { kit: kit.id, pad })?;
        self.selection.pad = Some(pad.id);
        self.selection.zone = pad.zone_order.first().copied();
        Ok(())
    }

    pub fn select_zone(&mut self, kit: &SampleKit, zone: ZoneId) -> Result<(), SamplerPaneError> {
        self.require_target_kit(kit)?;
        let zone = kit
            .zones
            .get(&zone)
            .ok_or(SamplerPaneError::MissingZone { kit: kit.id, zone })?;
        self.selection.pad = Some(zone.pad);
        self.selection.zone = Some(zone.id);
        Ok(())
    }

    pub fn set_bank(&mut self, kit: &SampleKit, bank: u16) -> Result<(), SamplerPaneError> {
        self.require_target_kit(kit)?;
        let count = kit
            .pad_order
            .len()
            .max(1)
            .div_ceil(SAMPLER_KEYBOARD_BANK_SIZE);
        if usize::from(bank) >= count {
            return Err(SamplerPaneError::MissingBank { bank, count });
        }
        self.selection.bank = bank;
        Ok(())
    }

    pub fn press_pad(
        &mut self,
        kit: &SampleKit,
        pad: PadId,
        velocity: f32,
    ) -> Result<SamplerGatePress, SamplerPaneError> {
        self.require_target_kit(kit)?;
        if !velocity.is_finite() || !(0.0..=1.0).contains(&velocity) {
            return Err(SamplerPaneError::InvalidVelocity);
        }
        if !kit.pads.contains_key(&pad) {
            return Err(SamplerPaneError::MissingPad { kit: kit.id, pad });
        }
        if kit.primary_target(pad).is_none() {
            return Err(SamplerPaneError::EmptyPad { kit: kit.id, pad });
        }
        self.next_gate_id = self
            .next_gate_id
            .checked_add(1)
            .ok_or(SamplerPaneError::GateIdExhausted)?;
        self.selection.pad = Some(pad);
        Ok(SamplerGatePress {
            id: SamplerGateId(self.next_gate_id),
            kit: kit.id,
            pad,
            velocity,
        })
    }

    pub fn map_browser_to_pad(
        &mut self,
        kit: &SampleKit,
        pad: PadId,
        source: AssetDrag,
        modifiers: DragModifiers,
    ) -> Result<SampleAction, SamplerPaneError> {
        self.require_target_kit(kit)?;
        if !kit.pads.contains_key(&pad) {
            return Err(SamplerPaneError::MissingPad { kit: kit.id, pad });
        }
        let intent = interpret_drop(
            DragPayload::Asset(source),
            DropTarget::SamplerPad { kit: kit.id, pad },
            modifiers,
        )?;
        debug_assert!(matches!(intent, DropIntent::MapAssetToPad { .. }));
        self.selection.pad = Some(pad);
        self.selection.zone = None;
        Ok(SampleAction::ApplyDrop(intent))
    }

    pub fn set_zone_range(
        &self,
        kit: &SampleKit,
        zone: ZoneId,
        source_range: AssetFrameRange,
    ) -> Result<SampleAction, SamplerPaneError> {
        let target = self.zone_target(kit, zone)?;
        Ok(SampleAction::EditZone(ZoneEditIntent::Trim {
            target,
            source_range,
        }))
    }

    pub fn set_zone_playback(
        &self,
        kit: &SampleKit,
        zone: ZoneId,
        gain_db: f32,
        pan: f32,
        tuning_cents: f32,
    ) -> Result<SampleAction, SamplerPaneError> {
        if !gain_db.is_finite()
            || !(-144.0..=48.0).contains(&gain_db)
            || !pan.is_finite()
            || !(-1.0..=1.0).contains(&pan)
            || !tuning_cents.is_finite()
            || !(-9_600.0..=9_600.0).contains(&tuning_cents)
        {
            return Err(SamplerPaneError::InvalidPlaybackParameters);
        }
        let target = self.zone_target(kit, zone)?;
        Ok(SampleAction::EditZone(ZoneEditIntent::SetPlayback {
            target,
            gain_db,
            pan,
            tuning_cents,
        }))
    }

    pub fn set_pad_choke(
        &self,
        kit: &SampleKit,
        pad: PadId,
        choke_group: Option<NonZeroU32>,
    ) -> Result<SampleAction, SamplerPaneError> {
        self.require_target_kit(kit)?;
        if !kit.pads.contains_key(&pad) {
            return Err(SamplerPaneError::MissingPad { kit: kit.id, pad });
        }
        Ok(SampleAction::SetPadChoke {
            kit: kit.id,
            pad,
            choke_group,
            expected_revision: kit.revision,
        })
    }

    pub fn create_pattern_from_pads(
        &self,
        kit: &SampleKit,
    ) -> Result<SampleAction, SamplerPaneError> {
        self.require_target_kit(kit)?;
        let intent = CreatePatternFromPadsIntent::from_kit(kit)
            .map_err(|_| SamplerPaneError::EmptyInstrument { kit: kit.id })?;
        Ok(SampleAction::CreatePatternFromPads(intent))
    }

    /// Reconcile any controller publication against the authoritative kit.
    /// Browser-to-pad publications select their exact new zone; multi-pad
    /// chop publications additionally expose a selectable result set.
    pub fn apply_publication(
        &mut self,
        receipt: &SamplePublishedResult,
        kit: &SampleKit,
    ) -> Result<(), SamplerPaneError> {
        if receipt.kit != kit.id {
            return Err(SamplerPaneError::PublicationKitMismatch {
                receipt: receipt.kit,
                snapshot: kit.id,
            });
        }
        if let Some(pad) = receipt
            .created_pads
            .iter()
            .find(|pad| !kit.pads.contains_key(pad))
        {
            return Err(SamplerPaneError::PublicationMissingPad {
                kit: kit.id,
                pad: *pad,
            });
        }
        if let Some(target) = receipt
            .created_zones
            .iter()
            .find(|target| kit.zone_for_target(**target).is_none())
        {
            return Err(SamplerPaneError::PublicationMissingZone(*target));
        }
        let pads = receipt.created_pads.clone();
        let selected = receipt
            .pad
            .filter(|pad| kit.pads.contains_key(pad))
            .or_else(|| pads.first().copied());
        self.target = match receipt.focus {
            SampleResultFocus::Pad {
                kit: focus_kit,
                pad,
            } if focus_kit == kit.id => SamplerTarget::Pad {
                kit: focus_kit,
                pad,
            },
            SampleResultFocus::Kit(focus_kit) if focus_kit == kit.id => {
                SamplerTarget::Kit(focus_kit)
            }
            SampleResultFocus::Sampler { target, .. } if target.kit() == Some(kit.id) => target,
            _ => SamplerTarget::Kit(kit.id),
        };
        self.selection.pad = selected;
        self.selection.zone = selected.and_then(|pad| {
            receipt
                .created_zones
                .iter()
                .find(|target| target.pad == pad)
                .map(|target| target.zone)
                .or_else(|| {
                    kit.pads
                        .get(&pad)
                        .and_then(|pad| pad.zone_order.first().copied())
                })
        });
        self.chop_result = (!pads.is_empty()).then(|| ChopResultSelection {
            kit: kit.id,
            pads,
            zones: receipt.created_zones.clone(),
            selected,
        });
        Ok(())
    }

    /// Multi-pad convenience returning the canonical created set after the
    /// general publication reconciliation.
    pub fn apply_chop_publication(
        &mut self,
        receipt: &SamplePublishedResult,
        kit: &SampleKit,
    ) -> Result<&ChopResultSelection, SamplerPaneError> {
        self.apply_publication(receipt, kit)?;
        self.chop_result
            .as_ref()
            .ok_or(SamplerPaneError::NoChopResult)
    }

    pub fn select_chop_result_pad(
        &mut self,
        kit: &SampleKit,
        pad: PadId,
    ) -> Result<(), SamplerPaneError> {
        let result = self
            .chop_result
            .as_mut()
            .ok_or(SamplerPaneError::NoChopResult)?;
        if result.kit != kit.id || !result.pads.contains(&pad) {
            return Err(SamplerPaneError::PadNotInChopResult { kit: kit.id, pad });
        }
        result.selected = Some(pad);
        self.selection.pad = Some(pad);
        self.selection.zone = kit
            .pads
            .get(&pad)
            .and_then(|pad| pad.zone_order.first().copied());
        Ok(())
    }

    pub fn chop_result(&self) -> Option<&ChopResultSelection> {
        self.chop_result.as_ref()
    }

    fn reconcile(&mut self, kit: &SampleKit) {
        let banks = kit
            .pad_order
            .len()
            .max(1)
            .div_ceil(SAMPLER_KEYBOARD_BANK_SIZE);
        self.selection.bank = self
            .selection
            .bank
            .min(banks.saturating_sub(1).min(usize::from(u16::MAX)) as u16);
        if self
            .selection
            .pad
            .is_none_or(|pad| !kit.pads.contains_key(&pad))
        {
            self.selection.pad = kit.pad_order.first().copied();
        }
        let selected_pad = self.selection.pad;
        if self.selection.zone.is_none_or(|zone| {
            kit.zones
                .get(&zone)
                .is_none_or(|zone| Some(zone.pad) != selected_pad)
        }) {
            self.selection.zone = selected_pad.and_then(|pad| {
                kit.pads
                    .get(&pad)
                    .and_then(|pad| pad.zone_order.first().copied())
            });
        }
    }

    fn zone_target(
        &self,
        kit: &SampleKit,
        zone: ZoneId,
    ) -> Result<ZoneEditTarget, SamplerPaneError> {
        self.require_target_kit(kit)?;
        let zone = kit
            .zones
            .get(&zone)
            .ok_or(SamplerPaneError::MissingZone { kit: kit.id, zone })?;
        Ok(ZoneEditTarget {
            kit: kit.id,
            pad: zone.pad,
            zone: zone.id,
            expected_revision: kit.revision,
        })
    }

    fn require_target_kit(&self, kit: &SampleKit) -> Result<(), SamplerPaneError> {
        match self.target.kit() {
            Some(target) if target == kit.id => Ok(()),
            expected => Err(SamplerPaneError::TargetKitMismatch {
                expected,
                actual: kit.id,
            }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SamplerPaneError {
    TargetKitMismatch {
        expected: Option<KitId>,
        actual: KitId,
    },
    PublicationKitMismatch {
        receipt: KitId,
        snapshot: KitId,
    },
    PublicationMissingPad {
        kit: KitId,
        pad: PadId,
    },
    PublicationMissingZone(SampleTargetRef),
    MissingPad {
        kit: KitId,
        pad: PadId,
    },
    EmptyPad {
        kit: KitId,
        pad: PadId,
    },
    MissingZone {
        kit: KitId,
        zone: ZoneId,
    },
    MissingBank {
        bank: u16,
        count: usize,
    },
    InvalidVelocity,
    InvalidPlaybackParameters,
    GateIdExhausted,
    NoChopResult,
    PadNotInChopResult {
        kit: KitId,
        pad: PadId,
    },
    EmptyInstrument {
        kit: KitId,
    },
    Drag(DragContractError),
}

impl fmt::Display for SamplerPaneError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyInstrument { .. } => {
                formatter.write_str("This instrument has no pads to make a pattern from")
            }
            other => write!(formatter, "{other:?}"),
        }
    }
}

impl Error for SamplerPaneError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Drag(error) => Some(error),
            _ => None,
        }
    }
}

impl From<DragContractError> for SamplerPaneError {
    fn from(error: DragContractError) -> Self {
        Self::Drag(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{AssetId, SampleFrames};
    use crate::sample_actions::SampleResultFocus;
    use crate::sample_kit::{SamplePad, SampleRouteIntent, SampleZone};

    fn kit_with_pads(count: u64) -> SampleKit {
        let kit_id = KitId::from_raw(10);
        let mut kit = SampleKit::new(
            kit_id,
            "Chopped kit",
            SampleRouteIntent::new(BusId::from_raw(2)).unwrap(),
        );
        kit.revision = 7;
        for raw in 1..=count {
            let pad = PadId::from_raw(raw);
            let zone = ZoneId::from_raw(raw + 100);
            let mut sample_pad = SamplePad::new(pad, format!("Pad {raw}"));
            sample_pad.zone_order.push(zone);
            let mut sample_zone =
                SampleZone::new(zone, pad, SourceMaterialRef::Asset(AssetId(raw)));
            sample_zone.gain_db = raw as f32;
            sample_zone.pan = raw as f32 / count.max(1) as f32;
            sample_zone.tuning_cents = raw as f32 * 100.0;
            kit.pad_order.push(pad);
            kit.pads.insert(pad, sample_pad);
            kit.zones.insert(zone, sample_zone);
        }
        kit
    }

    #[test]
    fn authoritative_projection_enumerates_every_pad_zone_and_keyboard_bank() {
        let kit = kit_with_pads(18);
        let mut model = SamplerPaneModel::new(SamplerTarget::Kit(kit.id));
        model.set_bank(&kit, 1).unwrap();
        model.select_zone(&kit, ZoneId::from_raw(118)).unwrap();
        let projection = model.project(&kit).unwrap();

        assert_eq!(projection.pads.len(), 18);
        assert_eq!(projection.bank_count, 2);
        assert_eq!(projection.pads[16].keyboard_key, Some("1"));
        assert_eq!(projection.pads[17].keyboard_key, Some("2"));
        assert_eq!(projection.pads[0].keyboard_key, None);
        assert_eq!(projection.pads[17].zones.len(), 1);
        assert!(projection.pads[17].zones[0].selected);
        assert_eq!(projection.pads[17].zones[0].gain_db, 18.0);
    }

    #[test]
    fn chop_result_consumes_exact_canonical_created_identities_and_is_selectable() {
        let mut after = kit_with_pads(5);
        after.revision = 8;
        let receipt = SamplePublishedResult {
            revision: 22,
            kit: after.id,
            created_pads: vec![PadId::from_raw(3), PadId::from_raw(4), PadId::from_raw(5)],
            created_zones: vec![
                SampleTargetRef {
                    kit: after.id,
                    pad: PadId::from_raw(3),
                    zone: ZoneId::from_raw(103),
                },
                SampleTargetRef {
                    kit: after.id,
                    pad: PadId::from_raw(4),
                    zone: ZoneId::from_raw(104),
                },
                SampleTargetRef {
                    kit: after.id,
                    pad: PadId::from_raw(5),
                    zone: ZoneId::from_raw(105),
                },
            ],
            pad: None,
            pattern: None,
            sequencer_clip: None,
            arrangement_clip: None,
            arrangement_track: None,
            output_bus: None,
            focus: SampleResultFocus::Kit(after.id),
            provenance: None,
        };
        let mut model = SamplerPaneModel::new(SamplerTarget::Kit(after.id));
        let result = model.apply_chop_publication(&receipt, &after).unwrap();
        assert_eq!(
            result.pads,
            vec![PadId::from_raw(3), PadId::from_raw(4), PadId::from_raw(5)]
        );
        assert_eq!(result.zones, receipt.created_zones);
        assert_eq!(result.selected, Some(PadId::from_raw(3)));

        model
            .select_chop_result_pad(&after, PadId::from_raw(5))
            .unwrap();
        assert_eq!(model.selection().pad, Some(PadId::from_raw(5)));
        assert_eq!(model.selection().zone, Some(ZoneId::from_raw(105)));
    }

    #[test]
    fn gate_release_retains_the_exact_press_identity() {
        let kit = kit_with_pads(1);
        let mut model = SamplerPaneModel::new(SamplerTarget::Kit(kit.id));
        let gate = model.press_pad(&kit, PadId::from_raw(1), 0.72).unwrap();
        assert_eq!(gate.id, SamplerGateId(1));
        assert_eq!(
            gate.press_action(),
            SampleAction::Audition(SampleAuditionIntent::PadGate {
                kit: kit.id,
                pad: PadId::from_raw(1),
                velocity: 0.72,
                pressed: true,
            })
        );
        assert_eq!(
            gate.release_action(),
            SampleAction::Audition(SampleAuditionIntent::PadGate {
                kit: kit.id,
                pad: PadId::from_raw(1),
                velocity: 0.72,
                pressed: false,
            })
        );
    }

    #[test]
    fn browser_mapping_and_parameter_edits_preserve_exact_targets_and_range() {
        let kit = kit_with_pads(1);
        let pad = PadId::from_raw(1);
        let zone = ZoneId::from_raw(101);
        let range = AssetFrameRange::new(SampleFrames(40), SampleFrames(90)).unwrap();
        let mut model = SamplerPaneModel::new(SamplerTarget::Kit(kit.id));

        assert_eq!(
            model
                .map_browser_to_pad(
                    &kit,
                    pad,
                    AssetDrag {
                        asset: AssetId(88),
                        source_range: Some(range),
                    },
                    DragModifiers::default(),
                )
                .unwrap(),
            SampleAction::ApplyDrop(DropIntent::MapAssetToPad {
                source: AssetDrag {
                    asset: AssetId(88),
                    source_range: Some(range),
                },
                kit: kit.id,
                pad,
            })
        );
        assert!(matches!(
            model
                .set_zone_range(&kit, zone, range)
                .unwrap(),
            SampleAction::EditZone(ZoneEditIntent::Trim {
                target: ZoneEditTarget {
                    kit: target_kit,
                    pad: target_pad,
                    zone: target_zone,
                    expected_revision: 7,
                },
                source_range,
            }) if target_kit == kit.id && target_pad == pad && target_zone == zone && source_range == range
        ));
        assert!(matches!(
            model
                .set_zone_playback(&kit, zone, -3.0, -0.25, 700.0)
                .unwrap(),
            SampleAction::EditZone(ZoneEditIntent::SetPlayback {
                target: ZoneEditTarget {
                    expected_revision: 7,
                    ..
                },
                gain_db: -3.0,
                pan: -0.25,
                tuning_cents: 700.0,
            })
        ));
        assert!(matches!(
            model
                .set_pad_choke(&kit, pad, NonZeroU32::new(4))
                .unwrap(),
            SampleAction::SetPadChoke {
                kit: target_kit,
                pad: target_pad,
                choke_group: Some(group),
                expected_revision: 7,
            } if target_kit == kit.id && target_pad == pad && group.get() == 4
        ));

        let receipt = SamplePublishedResult {
            revision: 31,
            kit: kit.id,
            created_pads: Vec::new(),
            created_zones: vec![SampleTargetRef {
                kit: kit.id,
                pad,
                zone,
            }],
            pad: Some(pad),
            pattern: None,
            sequencer_clip: None,
            arrangement_clip: None,
            arrangement_track: None,
            output_bus: Some(kit.output.bus),
            focus: SampleResultFocus::Pad { kit: kit.id, pad },
            provenance: None,
        };
        model.apply_publication(&receipt, &kit).unwrap();
        assert_eq!(model.target(), SamplerTarget::Pad { kit: kit.id, pad });
        assert_eq!(model.selection().pad, Some(pad));
        assert_eq!(model.selection().zone, Some(zone));
        assert!(model.chop_result().is_none());
    }

    #[test]
    fn create_pattern_from_pads_emits_the_kit_local_action_and_refuses_an_empty_instrument() {
        let kit = kit_with_pads(4);
        let model = SamplerPaneModel::new(SamplerTarget::Kit(kit.id));
        assert!(matches!(
            model.create_pattern_from_pads(&kit).unwrap(),
            SampleAction::CreatePatternFromPads(intent)
                if intent.kit == kit.id
                    && intent.expected_revision == kit.revision
                    && intent.result_focus
                        == crate::sample_actions::MakeBeatResultFocus::PatternEditor
        ));

        let empty = SampleKit::new(
            kit.id,
            "Empty",
            SampleRouteIntent::new(BusId::from_raw(2)).unwrap(),
        );
        assert_eq!(
            model.create_pattern_from_pads(&empty).unwrap_err(),
            SamplerPaneError::EmptyInstrument { kit: kit.id }
        );
        assert!(model
            .create_pattern_from_pads(&empty)
            .unwrap_err()
            .to_string()
            .contains("no pads"));
    }
}
