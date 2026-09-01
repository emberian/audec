//! Unified, immutable projection of source media and authored instrument samples.
//!
//! A virtual slice is not silently promoted into a new media asset: its stable
//! identity remains `(kit, pad, zone)` and its exact source material remains an
//! asset plus half-open decoded-frame range. This projection merely makes both
//! durable kinds reachable from one material browser.

use std::error::Error;
use std::fmt;

use crate::assets::{AssetId, MediaAsset};
use crate::daw_project::DawProject;
use crate::sample_actions::{
    named_sample_library, NamedSampleAsset, SampleAuditionIntent, SamplePublishedResult,
    SampleResultFocus, SampleSelection,
};
use crate::sample_kit::SampleTargetRef;
use crate::sample_material::SourceMaterialRef;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum MaterialPoolItemId {
    Source(AssetId),
    InstrumentSample(SampleTargetRef),
}

#[derive(Clone, Copy, Debug)]
pub enum MaterialPoolItemRef<'a> {
    Source(&'a MediaAsset),
    InstrumentSample(&'a NamedSampleAsset),
}

impl MaterialPoolItemRef<'_> {
    pub fn id(self) -> MaterialPoolItemId {
        match self {
            Self::Source(asset) => MaterialPoolItemId::Source(asset.id()),
            Self::InstrumentSample(sample) => MaterialPoolItemId::InstrumentSample(sample.target),
        }
    }

    pub fn material(self) -> SourceMaterialRef {
        match self {
            Self::Source(asset) => SourceMaterialRef::Asset(asset.id()),
            Self::InstrumentSample(sample) => sample.material,
        }
    }

    pub fn selection(self) -> SampleSelection {
        match self.material() {
            SourceMaterialRef::Asset(asset) => SampleSelection::whole_asset(asset),
            SourceMaterialRef::VirtualSlice(slice) => SampleSelection {
                asset: slice.source_asset,
                source_range: Some(slice.source_range),
            },
        }
    }
}

/// Revision-pinned catalog used by media-pool panes and workflow receipts.
/// All records are projections of durable project state; PCM availability is
/// resolved at audition time by the ordinary preview/runtime boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct MaterialPoolSnapshot {
    pub project_revision: u64,
    pub sources: Vec<MediaAsset>,
    pub instrument_samples: Vec<NamedSampleAsset>,
}

impl MaterialPoolSnapshot {
    pub fn from_project(project: &DawProject) -> Self {
        let state = project.state();
        Self {
            project_revision: project.revisions().aggregate,
            sources: state.domains.assets.assets().values().cloned().collect(),
            instrument_samples: named_sample_library(&state.domains.sample_kits),
        }
    }

    pub fn get(&self, id: MaterialPoolItemId) -> Option<MaterialPoolItemRef<'_>> {
        match id {
            MaterialPoolItemId::Source(id) => self
                .sources
                .iter()
                .find(|asset| asset.id() == id)
                .map(MaterialPoolItemRef::Source),
            MaterialPoolItemId::InstrumentSample(target) => self
                .instrument_samples
                .iter()
                .find(|sample| sample.target == target)
                .map(MaterialPoolItemRef::InstrumentSample),
        }
    }

    pub fn selection(&self, id: MaterialPoolItemId) -> Result<SampleSelection, MaterialPoolError> {
        self.get(id)
            .map(MaterialPoolItemRef::selection)
            .ok_or(MaterialPoolError::MissingItem(id))
    }

    /// Preview the exact material represented by a row. For layered pads this
    /// intentionally auditions only the selected zone's material, not every
    /// zone on the pad.
    pub fn audition_exact(
        &self,
        id: MaterialPoolItemId,
        velocity: f32,
    ) -> Result<SampleAuditionIntent, MaterialPoolError> {
        if !velocity.is_finite() || !(0.0..=1.0).contains(&velocity) {
            return Err(MaterialPoolError::InvalidVelocity);
        }
        let material = self
            .get(id)
            .map(MaterialPoolItemRef::material)
            .ok_or(MaterialPoolError::MissingItem(id))?;
        Ok(SampleAuditionIntent::MaterialOneShot { material, velocity })
    }

    /// Instrument-context audition is separate from exact-material preview:
    /// it deliberately triggers the whole pad, including layered zones.
    pub fn audition_pad(
        &self,
        target: SampleTargetRef,
        velocity: f32,
        pressed: bool,
    ) -> Result<SampleAuditionIntent, MaterialPoolError> {
        let sample = self
            .instrument_samples
            .iter()
            .find(|sample| sample.target == target)
            .ok_or(MaterialPoolError::MissingItem(
                MaterialPoolItemId::InstrumentSample(target),
            ))?;
        if !velocity.is_finite() || !(0.0..=1.0).contains(&velocity) {
            return Err(MaterialPoolError::InvalidVelocity);
        }
        Ok(sample.audition(velocity, pressed))
    }

    pub fn focus(
        &self,
        id: MaterialPoolItemId,
    ) -> Result<Option<SampleResultFocus>, MaterialPoolError> {
        match self.get(id).ok_or(MaterialPoolError::MissingItem(id))? {
            MaterialPoolItemRef::Source(_) => Ok(None),
            MaterialPoolItemRef::InstrumentSample(sample) => Ok(Some(SampleResultFocus::Pad {
                kit: sample.target.kit,
                pad: sample.target.pad,
            })),
        }
    }

    /// Resolve only samples created by one exact constructive publication.
    /// The publication order is retained so an onset chop keeps pad order.
    pub fn created_samples(
        &self,
        publication: &SamplePublishedResult,
    ) -> Result<Vec<&NamedSampleAsset>, MaterialPoolError> {
        publication
            .created_zones
            .iter()
            .map(|target| {
                self.instrument_samples
                    .iter()
                    .find(|sample| sample.target == *target)
                    .ok_or(MaterialPoolError::PublishedSampleMissing(*target))
            })
            .collect()
    }

    pub fn samples_for_material(
        &self,
        material: SourceMaterialRef,
    ) -> impl Iterator<Item = &NamedSampleAsset> {
        self.instrument_samples
            .iter()
            .filter(move |sample| sample.material == material)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaterialPoolError {
    MissingItem(MaterialPoolItemId),
    PublishedSampleMissing(SampleTargetRef),
    InvalidVelocity,
}

impl fmt::Display for MaterialPoolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingItem(id) => write!(formatter, "material-pool item {id:?} is missing"),
            Self::PublishedSampleMissing(target) => write!(
                formatter,
                "published sample {}:{}:{} is missing from durable kit state",
                target.kit.get(),
                target.pad.get(),
                target.zone.get()
            ),
            Self::InvalidVelocity => {
                formatter.write_str("material audition velocity must be finite and within 0..=1")
            }
        }
    }
}

impl Error for MaterialPoolError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{AssetFrameRange, SampleFrames};
    use crate::sample_material::VirtualSliceRef;

    #[test]
    fn exact_preview_and_whole_pad_audition_remain_distinct() {
        let target = SampleTargetRef {
            kit: crate::sample_kit::KitId::from_raw(2),
            pad: crate::sample_kit::PadId::from_raw(3),
            zone: crate::sample_kit::ZoneId::from_raw(4),
        };
        let material = SourceMaterialRef::VirtualSlice(
            VirtualSliceRef::new(
                AssetId(9),
                AssetFrameRange::new(SampleFrames(20), SampleFrames(80)).unwrap(),
            )
            .unwrap(),
        );
        let pool = MaterialPoolSnapshot {
            project_revision: 7,
            sources: Vec::new(),
            instrument_samples: vec![NamedSampleAsset {
                target,
                name: "Glass hit".into(),
                material,
                provenance: crate::sample_material::SampleMaterialProvenance::ManualSelection,
                instrument_name: "Glass kit".into(),
                output_bus: crate::mixer::BusId::from_raw(1),
            }],
        };
        assert_eq!(
            pool.selection(MaterialPoolItemId::InstrumentSample(target))
                .unwrap()
                .source_range,
            Some(AssetFrameRange {
                start: SampleFrames(20),
                end: SampleFrames(80)
            })
        );
        assert!(matches!(
            pool.audition_exact(MaterialPoolItemId::InstrumentSample(target), 0.7),
            Ok(SampleAuditionIntent::MaterialOneShot {
                material: selected,
                ..
            }) if selected == material
        ));
        assert!(matches!(
            pool.audition_pad(target, 0.7, true),
            Ok(SampleAuditionIntent::PadGate { kit, pad, .. })
                if kit == target.kit && pad == target.pad
        ));
    }
}
