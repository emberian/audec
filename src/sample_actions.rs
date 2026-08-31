//! Controller-facing intents emitted by audec's sample browser and pad editor.
//!
//! These values are deliberately project-model agnostic. Views may select and
//! inspect authoritative [`SampleKit`](crate::sample_kit::SampleKit) snapshots,
//! but every audible or authored consequence crosses this typed callback seam.
//! A controller is responsible for validating revisions, allocating IDs,
//! constructing commands, and publishing constructive plans atomically.

use std::sync::Arc;

use crate::assets::{AssetFrameRange, AssetId};
use crate::mixer::BusId;
use crate::sample_kit::{KitId, PadId, ZoneId};
use crate::sample_material::{
    SampleMaterialProvenance, ScopedEvidenceRef, SourceMaterialRef, VirtualSliceRef,
};
use crate::ui_drag::DropIntent;

/// The exact source material under the browser's playhead or range selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SampleSelection {
    pub asset: AssetId,
    pub source_range: Option<AssetFrameRange>,
}

impl SampleSelection {
    pub const fn whole_asset(asset: AssetId) -> Self {
        Self {
            asset,
            source_range: None,
        }
    }

    pub const fn material(self) -> SourceMaterialRef {
        match self.source_range {
            Some(source_range) => SourceMaterialRef::VirtualSlice(VirtualSliceRef {
                source_asset: self.asset,
                source_range,
            }),
            None => SourceMaterialRef::Asset(self.asset),
        }
    }
}

/// The audition engine receives semantic starts/stops rather than UI events.
/// A one-shot may naturally finish without a later stop; gate mode must stop
/// when its pointer/key is released or the view loses focus.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SampleAuditionIntent {
    MaterialOneShot {
        material: SourceMaterialRef,
        velocity: f32,
    },
    PadGate {
        kit: KitId,
        pad: PadId,
        velocity: f32,
        pressed: bool,
    },
}

/// How a selected span should become playable zones and initial beat events.
#[derive(Clone, Debug, PartialEq)]
pub enum SampleChopIntent {
    OneShot,
    EqualSlices {
        count: u16,
    },
    DetectOnsets {
        analyzer: String,
        sensitivity: f32,
        minimum_gap_frames: u64,
    },
}

impl Default for SampleChopIntent {
    fn default() -> Self {
        Self::EqualSlices { count: 8 }
    }
}

/// Where a constructive adapter should publish the pads it creates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleKitDestination {
    NewKit,
    ExistingKit { kit: KitId, expected_revision: u64 },
}

/// Complete user intent behind “Sample selection & make beat”.
///
/// Timing, onset evidence, ID allocation, material fingerprints, and the
/// resulting `ConstructiveEditPlan` remain controller/constructive concerns.
#[derive(Clone, Debug, PartialEq)]
pub struct MakeBeatIntent {
    pub source: SampleSelection,
    pub chop: SampleChopIntent,
    pub kit: SampleKitDestination,
    pub target_bus: Option<BusId>,
    pub bars: u16,
    pub quantize_ticks: u64,
}

/// A stable target for provenance/evidence disclosure in an inspector pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SampleInspectTarget {
    Material(SourceMaterialRef),
    Zone { kit: KitId, zone: ZoneId },
    Provenance(SampleMaterialProvenance),
    Evidence(ScopedEvidenceRef),
}

/// All semantic output from sampler-facing views.
#[derive(Clone, Debug, PartialEq)]
pub enum SampleAction {
    Audition(SampleAuditionIntent),
    /// A drag/drop interpretation retaining its exact source range.
    ApplyDrop(DropIntent),
    SetKitOutput {
        kit: KitId,
        bus: BusId,
        expected_revision: u64,
    },
    RemoveZone {
        kit: KitId,
        pad: PadId,
        zone: ZoneId,
        expected_revision: u64,
    },
    Inspect(SampleInspectTarget),
    MakeBeat(MakeBeatIntent),
}

pub type SampleActionCallback = Arc<dyn Fn(SampleAction) + Send + Sync + 'static>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::SampleFrames;

    #[test]
    fn selection_material_preserves_an_exact_half_open_range() {
        let range = AssetFrameRange::new(SampleFrames(120), SampleFrames(960)).unwrap();
        let selection = SampleSelection {
            asset: AssetId(7),
            source_range: Some(range),
        };

        assert_eq!(
            selection.material(),
            SourceMaterialRef::VirtualSlice(VirtualSliceRef {
                source_asset: AssetId(7),
                source_range: range,
            })
        );
    }
}
