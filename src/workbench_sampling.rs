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
    MakeBeatIntent, MakeBeatResultFocus, SampleChopIntent, SampleKitDestination,
    SamplePublishedResult, SampleResultFocus, SampleResultProvenance, SampleSelection,
    SampleWorkflowAfter, SampleWorkflowPlanIntent, SampleWorkflowProduct, SampleWorkflowReceipt,
    SampleWorkflowSpec, SampleWorkflowValidationError, SamplerTarget, SamplerViewDisposition,
};
use crate::session::SampleRange;

use super::constructive_controller::{
    apply_make_beat_focus, ConstructiveControllerError, ConstructiveOutcome,
    ConstructivePublication, ConstructivePublishedFocus,
};

#[derive(Clone, Debug, PartialEq)]
pub enum WorkbenchSampleIntent {
    /// Product-facing path used by the visible active-selection/loop actions.
    /// It keeps naming and the requested landing intact while crossing the
    /// existing `ProjectSession::publish_*_workbench_range` boundary.
    Workflow(SampleWorkflowSpec),
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

/// Cohesive completion for the product-facing sample workflow. The legacy
/// `WorkbenchSampleOutcome` remains available to narrow source actions, while
/// this receipt keeps the names, library samples, audition target, and visible
/// landing together.
#[derive(Clone, Debug)]
pub struct WorkbenchSampleWorkflowOutcome {
    pub source: SampleSelection,
    pub constructive: ConstructiveOutcome,
    pub receipt: SampleWorkflowReceipt,
}

impl ProjectController {
    /// Turn the primary source's selected or looped span into the explicitly
    /// named product described by `spec`.
    pub fn publish_primary_sample_workflow(
        &mut self,
        range: SampleRange,
        spec: SampleWorkflowSpec,
    ) -> Result<WorkbenchSampleWorkflowOutcome, WorkbenchSamplingError> {
        let asset = self
            .live_project()
            .primary_source_ids()
            .map(|ids| ids.registry_asset)
            .ok_or(WorkbenchSamplingError::NoPrimarySource)?;
        self.publish_sample_workflow(asset, range, spec)
    }

    /// Explicit-asset form used by a Material surface. `spec.span_origin`
    /// distinguishes an active loop from a free selection without changing the
    /// exact source-frame interpretation.
    pub fn publish_sample_workflow(
        &mut self,
        asset: AssetId,
        range: SampleRange,
        spec: SampleWorkflowSpec,
    ) -> Result<WorkbenchSampleWorkflowOutcome, WorkbenchSamplingError> {
        spec.validate()?;
        let source = SampleSelection {
            asset,
            source_range: Some(asset_range(range)?),
        };
        let plan_intent = spec.plan_intent(source)?;
        let chop = match &plan_intent {
            SampleWorkflowPlanIntent::BuildInstrument { chop, .. } => chop.clone(),
            SampleWorkflowPlanIntent::MakeBeat(intent) => intent.chop.clone(),
        };
        let mut plan = match plan_intent {
            SampleWorkflowPlanIntent::BuildInstrument {
                chop,
                kit,
                target_bus,
            } => self.plan_sample_kit(source, chop, kit, target_bus, spec.product.label())?,
            SampleWorkflowPlanIntent::MakeBeat(intent) => self.plan_make_beat(intent)?,
        };
        apply_workflow_names(&mut plan, &spec)?;
        let mut constructive = self.execute_constructive_plan(plan)?;
        apply_workflow_landing(&mut constructive.publication, &spec)?;
        let publication = sample_workflow_publication(&constructive.publication, source, chop);
        let receipt = SampleWorkflowReceipt::from_project(
            &spec,
            source,
            publication,
            &self.snapshot().project,
        )?;
        Ok(WorkbenchSampleWorkflowOutcome {
            source,
            constructive,
            receipt,
        })
    }

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
        let mut requested_focus = None;
        let mut workflow = None;
        let plan = match intent {
            WorkbenchSampleIntent::Workflow(spec) => {
                spec.validate()?;
                let plan_intent = spec.plan_intent(source)?;
                let mut plan = match plan_intent {
                    SampleWorkflowPlanIntent::BuildInstrument {
                        chop,
                        kit,
                        target_bus,
                    } => {
                        self.plan_sample_kit(source, chop, kit, target_bus, spec.product.label())?
                    }
                    SampleWorkflowPlanIntent::MakeBeat(intent) => self.plan_make_beat(intent)?,
                };
                apply_workflow_names(&mut plan, &spec)?;
                workflow = Some(spec);
                plan
            }
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
            } => {
                requested_focus = Some(result_focus);
                self.plan_make_beat(MakeBeatIntent {
                    source,
                    chop,
                    kit,
                    target_bus,
                    bars,
                    quantize_ticks,
                    result_focus,
                })?
            }
        };
        let mut constructive = self.execute_constructive_plan(plan)?;
        if let Some(result_focus) = requested_focus {
            apply_make_beat_focus(&mut constructive.publication, result_focus)?;
        }
        if let Some(spec) = &workflow {
            apply_workflow_landing(&mut constructive.publication, spec)?;
        }
        Ok(WorkbenchSampleOutcome {
            source,
            constructive,
        })
    }
}

fn apply_workflow_names(
    plan: &mut crate::constructive::ConstructiveEditPlan,
    spec: &SampleWorkflowSpec,
) -> Result<(), WorkbenchSamplingError> {
    if let crate::sample_actions::SampleInstrumentDestination::New { name } = &spec.destination {
        plan.kit.after.name = name.trim().to_owned();
    }
    let pads = plan
        .materials
        .iter()
        .filter_map(|material| {
            plan.kit
                .after
                .zones
                .get(&material.zone)
                .map(|zone| zone.pad)
        })
        .fold(Vec::new(), |mut pads, pad| {
            if !pads.contains(&pad) {
                pads.push(pad);
            }
            pads
        });
    let count = pads.len();
    for (index, pad) in pads.into_iter().enumerate() {
        let value = plan
            .kit
            .after
            .pads
            .get_mut(&pad)
            .ok_or(WorkbenchSamplingError::MissingPlannedPad(pad))?;
        value.name = spec.product.sample_name(index, count).trim().to_owned();
    }
    if let Some(pattern) = &mut plan.pattern {
        pattern.name = spec
            .product
            .pattern_name()
            .ok_or(WorkbenchSamplingError::MissingWorkflowPatternName)?
            .trim()
            .to_owned();
    }
    plan.label = match &spec.product {
        SampleWorkflowProduct::OneSample { name } => format!("Make sample “{}”", name.trim()),
        SampleWorkflowProduct::SliceToKit { sample_name, .. } => {
            format!("Slice to pads “{}”", sample_name.trim())
        }
        SampleWorkflowProduct::MakeBeat { pattern_name, .. } => {
            format!("Make beat “{}”", pattern_name.trim())
        }
    };
    plan.validate()
        .map_err(|error| WorkbenchSamplingError::NamedPlan(error.to_string()))
}

fn apply_workflow_landing(
    publication: &mut ConstructivePublication,
    spec: &SampleWorkflowSpec,
) -> Result<(), WorkbenchSamplingError> {
    match spec.after {
        SampleWorkflowAfter::Stay => publication.focus = ConstructivePublishedFocus::Stay,
        SampleWorkflowAfter::OpenInstrument => {
            if matches!(spec.product, SampleWorkflowProduct::OneSample { .. }) {
                let pad = publication
                    .created_pads
                    .first()
                    .copied()
                    .ok_or(WorkbenchSamplingError::MissingPublishedPad)?;
                publication.pad = Some(pad);
                publication.focus = ConstructivePublishedFocus::Pad {
                    kit: publication.kit,
                    pad,
                };
            } else {
                publication.pad = None;
                publication.focus = ConstructivePublishedFocus::Sampler {
                    kit: publication.kit,
                    disposition: SamplerViewDisposition::RetargetCurrent,
                };
            }
        }
        SampleWorkflowAfter::OpenPattern | SampleWorkflowAfter::OpenArrangement => {
            apply_make_beat_focus(&mut *publication, spec.after.make_beat_focus())?;
        }
    }
    Ok(())
}

fn sample_workflow_publication(
    publication: &ConstructivePublication,
    source: SampleSelection,
    chop: SampleChopIntent,
) -> SamplePublishedResult {
    let focus = match publication.focus {
        ConstructivePublishedFocus::Stay => SampleResultFocus::Stay,
        ConstructivePublishedFocus::Kit(kit) => SampleResultFocus::Kit(kit),
        ConstructivePublishedFocus::Pad { kit, pad } => SampleResultFocus::Pad { kit, pad },
        ConstructivePublishedFocus::Pattern(pattern) => SampleResultFocus::Pattern(pattern),
        ConstructivePublishedFocus::Arrangement(arrangement_clip) => {
            SampleResultFocus::Arrangement {
                arrangement_clip,
                sequencer_clip: publication.sequencer_clip,
                pattern: publication.pattern,
            }
        }
        ConstructivePublishedFocus::Sampler { kit, disposition } => SampleResultFocus::Sampler {
            target: SamplerTarget::Kit(kit),
            disposition,
        },
    };
    SamplePublishedResult {
        revision: publication.revision,
        kit: publication.kit,
        created_pads: publication.created_pads.clone(),
        created_zones: publication.created_zones.clone(),
        pad: publication.pad,
        pattern: publication.pattern,
        sequencer_clip: publication.sequencer_clip,
        arrangement_clip: publication.arrangement_clip,
        arrangement_track: publication.arrangement_track,
        output_bus: publication.output_bus,
        focus,
        provenance: Some(SampleResultProvenance::Selection {
            source,
            chop: Some(chop),
        }),
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
    MissingPlannedPad(crate::sample_kit::PadId),
    MissingPublishedPad,
    MissingWorkflowPatternName,
    NamedPlan(String),
    Workflow(SampleWorkflowValidationError),
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

impl From<SampleWorkflowValidationError> for WorkbenchSamplingError {
    fn from(error: SampleWorkflowValidationError) -> Self {
        Self::Workflow(error)
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
    use crate::render_runtime::AuditionOwner;
    use crate::sample_actions::{
        resolve_sample_audition, SampleInstrumentDestination, SamplePreviewCommand,
        SamplePreviewToken, SampleSpanOrigin, SampleWorkflowAfter, SampleWorkflowProduct,
        SamplerViewDisposition,
    };
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
    fn make_beat_honors_every_requested_result_focus() {
        let cases = [
            MakeBeatResultFocus::Stay,
            MakeBeatResultFocus::Sampler(SamplerViewDisposition::RetargetCurrent),
            MakeBeatResultFocus::PatternEditor,
            MakeBeatResultFocus::Arrangement,
        ];
        for requested in cases {
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
                        result_focus: requested,
                    },
                )
                .unwrap();
            let publication = &outcome.constructive.publication;
            let expected = match requested {
                MakeBeatResultFocus::Stay => {
                    crate::project_controller::ConstructivePublishedFocus::Stay
                }
                MakeBeatResultFocus::Sampler(disposition) => {
                    crate::project_controller::ConstructivePublishedFocus::Sampler {
                        kit: publication.kit,
                        disposition,
                    }
                }
                MakeBeatResultFocus::PatternEditor => {
                    crate::project_controller::ConstructivePublishedFocus::Pattern(
                        publication.pattern.unwrap(),
                    )
                }
                MakeBeatResultFocus::Arrangement => {
                    crate::project_controller::ConstructivePublishedFocus::Arrangement(
                        publication.arrangement_clip.unwrap(),
                    )
                }
            };
            assert_eq!(publication.focus, expected);
        }
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

    #[test]
    fn active_loop_becomes_named_samples_playable_pads_and_a_visible_pattern() {
        let mut controller = controller();
        let outcome = controller
            .publish_primary_sample_workflow(
                range(),
                SampleWorkflowSpec {
                    span_origin: SampleSpanOrigin::Loop,
                    product: SampleWorkflowProduct::MakeBeat {
                        sample_name: "Loop chop".into(),
                        pattern_name: "Loop beat".into(),
                        chop: SampleChopIntent::EqualSlices { count: 3 },
                        bars: 1,
                        quantize_ticks: sequencer::PPQ as u64,
                    },
                    destination: SampleInstrumentDestination::New {
                        name: "Loop drums".into(),
                    },
                    target_bus: None,
                    after: SampleWorkflowAfter::OpenPattern,
                },
            )
            .unwrap();

        let snapshot = controller.snapshot();
        let kit = &snapshot.project.state().domains.sample_kits.kits
            [&outcome.constructive.publication.kit];
        assert_eq!(kit.name, "Loop drums");
        assert_eq!(
            kit.ordered_pads()
                .map(|pad| pad.name.as_str())
                .collect::<Vec<_>>(),
            ["Loop chop 01", "Loop chop 02", "Loop chop 03"]
        );
        let pattern = outcome.constructive.publication.pattern.unwrap();
        assert_eq!(
            snapshot
                .project
                .state()
                .domains
                .sequencer
                .patterns()
                .get(pattern)
                .unwrap()
                .name,
            "Loop beat"
        );
        assert_eq!(outcome.receipt.span_origin, SampleSpanOrigin::Loop);
        assert_eq!(outcome.receipt.samples.len(), 3);
        assert!(outcome.receipt.samples.iter().all(|sample| matches!(
            sample.material,
            SourceMaterialRef::VirtualSlice(slice)
                if slice.source_asset == outcome.source.asset
                    && slice.source_range.start >= outcome.source.source_range.unwrap().start
                    && slice.source_range.end <= outcome.source.source_range.unwrap().end
        )));
        assert!(matches!(
            outcome.receipt.landing,
            crate::sample_actions::SampleWorkflowLanding::Pattern {
                pattern: landed,
                ..
            } if landed == pattern
        ));
        let presentation = outcome.receipt.presentation();
        assert!(presentation.headline.contains("Loop beat"));
        assert!(presentation.detail.contains("from loop"));

        let audition = outcome.receipt.primary_audition(0.9, true).unwrap();
        let resolved = resolve_sample_audition(
            snapshot,
            SamplePreviewToken {
                owner: AuditionOwner {
                    namespace: 77,
                    local: 1,
                },
                generation: 1,
            },
            audition,
        )
        .unwrap();
        assert!(matches!(
            resolved.command,
            SamplePreviewCommand::Start { .. }
        ));
    }

    #[test]
    fn visible_workflow_crosses_the_existing_session_range_boundary() {
        let mut controller = controller();
        let spec = SampleWorkflowSpec::expected(
            crate::sample_actions::SampleWorkflowCommand::MakeSample,
            SampleSpanOrigin::Loop,
            "Workbench source",
            SampleInstrumentDestination::New {
                name: "Workbench sounds".into(),
            },
            None,
        );
        let outcome = controller
            .publish_primary_workbench_range(range(), WorkbenchSampleIntent::Workflow(spec))
            .unwrap();
        let publication = &outcome.constructive.publication;
        let pad = publication.created_pads[0];
        assert!(matches!(
            publication.focus,
            ConstructivePublishedFocus::Pad {
                kit,
                pad: focused
            } if kit == publication.kit && focused == pad
        ));
        assert!(publication.pattern.is_none());
        let samples = crate::sample_actions::named_sample_library(
            &controller.snapshot().project.state().domains.sample_kits,
        );
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].name, "Workbench source sample");
        assert_eq!(samples[0].target.pad, pad);
    }
}
