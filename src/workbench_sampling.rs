//! GPUI-free adapter from the workbench source timeline to constructive edits.
//!
//! A workbench range is in decoded source-frame coordinates. This adapter
//! resolves the already-registered source asset, converts the half-open range
//! without resampling, and hands one complete plan to `ProjectController`.

use std::error::Error;
use std::fmt;

use crate::assets::{AssetFrameRange, AssetId, SampleFrames};
use crate::live_project::ProjectController;
use crate::mixer::BusId;
use crate::sample_actions::{
    MakeBeatIntent, MakeBeatResultFocus, SampleChopIntent, SampleKitDestination, SampleSelection,
};
use crate::session::SampleRange;

use super::constructive_controller::{ConstructiveControllerError, ConstructiveOutcome};

#[derive(Clone, Debug, PartialEq)]
pub enum WorkbenchSampleIntent {
    OneShot {
        kit: SampleKitDestination,
        target_bus: Option<BusId>,
    },
    Chop {
        chop: SampleChopIntent,
        kit: SampleKitDestination,
        target_bus: Option<BusId>,
    },
    MakeBeat {
        chop: SampleChopIntent,
        kit: SampleKitDestination,
        target_bus: Option<BusId>,
        bars: u16,
        quantize_ticks: u64,
        result_focus: MakeBeatResultFocus,
    },
}

#[derive(Clone, Debug)]
pub struct WorkbenchSampleOutcome {
    pub source: SampleSelection,
    pub constructive: ConstructiveOutcome,
}

impl ProjectController {
    /// Publish from the source asset created by `LiveProject::from_source_material`.
    pub fn publish_primary_workbench_range(
        &mut self,
        range: SampleRange,
        intent: WorkbenchSampleIntent,
    ) -> Result<WorkbenchSampleOutcome, WorkbenchSamplingError> {
        let asset = self
            .live_project()
            .primary_source_ids()
            .map(|ids| ids.registry_asset)
            .ok_or(WorkbenchSamplingError::NoPrimarySource)?;
        self.publish_workbench_range(asset, range, intent)
    }

    /// Explicit-asset form for reopened projects and future multi-source
    /// workbenches. Neither form reads or mutates compatibility mirrors.
    pub fn publish_workbench_range(
        &mut self,
        asset: AssetId,
        range: SampleRange,
        intent: WorkbenchSampleIntent,
    ) -> Result<WorkbenchSampleOutcome, WorkbenchSamplingError> {
        let source = SampleSelection {
            asset,
            source_range: Some(asset_range(range)?),
        };
        let plan = match intent {
            WorkbenchSampleIntent::OneShot { kit, target_bus } => self.plan_sample_kit(
                source,
                SampleChopIntent::OneShot,
                kit,
                target_bus,
                "Create one-shot sample",
            )?,
            WorkbenchSampleIntent::Chop {
                chop,
                kit,
                target_bus,
            } => {
                if matches!(chop, SampleChopIntent::OneShot) {
                    return Err(WorkbenchSamplingError::ChopRequiresSlices);
                }
                self.plan_sample_kit(source, chop, kit, target_bus, "Chop source selection")?
            }
            WorkbenchSampleIntent::MakeBeat {
                chop,
                kit,
                target_bus,
                bars,
                quantize_ticks,
                result_focus,
            } => self.plan_make_beat(MakeBeatIntent {
                source,
                chop,
                kit,
                target_bus,
                bars,
                quantize_ticks,
                result_focus,
            })?,
        };
        let constructive = self.execute_constructive_plan(plan)?;
        Ok(WorkbenchSampleOutcome {
            source,
            constructive,
        })
    }
}

fn asset_range(range: SampleRange) -> Result<AssetFrameRange, WorkbenchSamplingError> {
    let start =
        u64::try_from(range.start.get()).map_err(|_| WorkbenchSamplingError::NegativeRange)?;
    let end = u64::try_from(range.end.get()).map_err(|_| WorkbenchSamplingError::NegativeRange)?;
    AssetFrameRange::new(SampleFrames(start), SampleFrames(end))
        .map_err(|_| WorkbenchSamplingError::EmptyRange)
}

#[derive(Debug)]
pub enum WorkbenchSamplingError {
    NoPrimarySource,
    NegativeRange,
    EmptyRange,
    ChopRequiresSlices,
    Constructive(ConstructiveControllerError),
}

impl fmt::Display for WorkbenchSamplingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl Error for WorkbenchSamplingError {}

impl From<ConstructiveControllerError> for WorkbenchSamplingError {
    fn from(error: ConstructiveControllerError) -> Self {
        Self::Constructive(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        AssetRegistry, ContentFingerprint, DecodedAudioMetadata,
    };
    use crate::audio::AudioFormat;
    use crate::daw_render::PcmAsset;
    use crate::live_project::{LiveProject, SourceMaterialMetadata};
    use crate::sample_actions::SamplerViewDisposition;
    use crate::sample_material::{SampleMaterialProvenance, SourceMaterialRef};
    use crate::sequencer;
    use crate::session::Sample;

    fn controller() -> ProjectController {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/audio/workbench-source.wav").unwrap()),
            None,
        )
        .unwrap();
        let mut registry = AssetRegistry::new();
        let asset = registry
            .register(AssetRegistration {
                name: "workbench source".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: 48_000,
                    channels: 1,
                    frame_count: SampleFrames(12),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32le".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"workbench sampling source"),
                provenance: AssetProvenance::new(
                    1,
                    AssetOrigin::ImportedFile {
                        importer: "test".into(),
                    },
                    location,
                ),
                tags: BTreeSet::new(),
                favorite: false,
            })
            .unwrap();
        let pcm = PcmAsset::new(
            AudioFormat::new(48_000, 1).unwrap(),
            Arc::from([0.0, 0.0, 1.0, 0.1, 0.0, 0.0, 0.8, 0.1, 0.0, 0.0, 0.0, 0.0]),
        )
        .unwrap();
        ProjectController::new(
            LiveProject::from_source_material(
                SourceMaterialMetadata::new("Workbench", "Source"),
                registry,
                asset,
                pcm,
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn range() -> SampleRange {
        SampleRange::new(Sample::new(1), Sample::new(10))
    }

    #[test]
    fn timeline_range_becomes_exact_one_shot_without_pattern_and_undoes_once() {
        let mut controller = controller();
        let outcome = controller
            .publish_primary_workbench_range(
                range(),
                WorkbenchSampleIntent::OneShot {
                    kit: SampleKitDestination::NewKit,
                    target_bus: None,
                },
            )
            .unwrap();
        assert_eq!(
            outcome.source.source_range,
            Some(AssetFrameRange {
                start: SampleFrames(1),
                end: SampleFrames(10),
            })
        );
        assert!(outcome.constructive.publication.pattern.is_none());
        assert!(outcome.constructive.publication.arrangement_clip.is_none());
        assert_eq!(controller.snapshot().sample_pcm.len(), 1);
        let kit = &controller
            .snapshot()
            .project
            .state()
            .domains
            .sample_kits
            .kits[&outcome.constructive.publication.kit];
        let zone = kit.zones.values().next().unwrap();
        assert!(matches!(
            zone.material,
            SourceMaterialRef::VirtualSlice(slice)
                if slice.source_range.start == SampleFrames(1)
                    && slice.source_range.end == SampleFrames(10)
        ));
        assert_eq!(zone.provenance, SampleMaterialProvenance::ManualSelection);
        controller.undo().unwrap().unwrap();
        assert!(controller.snapshot().sample_pcm.is_empty());
        assert!(!controller.can_undo());
    }

    #[test]
    fn equal_onset_and_make_beat_requests_share_the_authoritative_path() {
        let cases = [
            SampleChopIntent::EqualSlices { count: 3 },
            SampleChopIntent::DetectOnsets {
                analyzer: "workbench-onsets-v1".into(),
                sensitivity: 0.5,
                minimum_gap_frames: 2,
            },
        ];
        for chop in cases {
            let onset = matches!(chop, SampleChopIntent::DetectOnsets { .. });
            let mut controller = controller();
            let outcome = controller
                .publish_primary_workbench_range(
                    range(),
                    WorkbenchSampleIntent::Chop {
                        chop,
                        kit: SampleKitDestination::NewKit,
                        target_bus: None,
                    },
                )
                .unwrap();
            assert!(outcome.constructive.publication.pattern.is_none());
            assert!(controller.snapshot().sample_pcm.len() >= 2);
            if onset {
                let kit = &controller
                    .snapshot()
                    .project
                    .state()
                    .domains
                    .sample_kits
                    .kits[&outcome.constructive.publication.kit];
                assert!(kit.zones.values().all(|zone| matches!(
                    zone.provenance,
                    SampleMaterialProvenance::OnsetChop { .. }
                )));
            }
        }

        let mut controller = controller();
        let outcome = controller
            .publish_primary_workbench_range(
                range(),
                WorkbenchSampleIntent::MakeBeat {
                    chop: SampleChopIntent::EqualSlices { count: 3 },
                    kit: SampleKitDestination::NewKit,
                    target_bus: None,
                    bars: 1,
                    quantize_ticks: sequencer::PPQ as u64,
                    result_focus: MakeBeatResultFocus::Sampler(
                        SamplerViewDisposition::RetargetCurrent,
                    ),
                },
            )
            .unwrap();
        assert!(outcome.constructive.publication.pattern.is_some());
        assert!(outcome.constructive.publication.arrangement_clip.is_some());
        assert_eq!(controller.journal_records().len(), 1);
    }

    #[test]
    fn invalid_signed_timeline_ranges_never_reach_the_project() {
        assert!(matches!(
            asset_range(SampleRange::new(Sample::new(-2), Sample::new(4))),
            Err(WorkbenchSamplingError::NegativeRange)
        ));
        assert!(matches!(
            asset_range(SampleRange::empty(Sample::new(4))),
            Err(WorkbenchSamplingError::EmptyRange)
        ));
    }
}
