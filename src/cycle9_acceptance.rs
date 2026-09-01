//! Adversarial, cross-boundary acceptance corpus for Cycle 9.
//!
//! This file deliberately lives outside the production module tree.  The
//! convergence owner registers it under `#[cfg(test)]` once the Cycle 9
//! public seams settle.  Keeping it here makes the acceptance contract easy
//! to audit without granting a test-only shortcut to production code.
//!
//! Current gap matrix (updated as Cycle 9 seams arrive):
//!
//! | Promise | Acceptance boundary | State |
//! | --- | --- | --- |
//! | Pattern session operations | `pattern_workflow` controller | Existing lane exercises lifecycle / gestures; add a session-host corpus when its public seam lands. |
//! | Comparison alignment / cancellation | source resolver + runtime | Covered below. |
//! | Durable command codec | reconstructed codec state | Await concrete runtime codec; do not test a guessed registry. |
//! | Edit while looping | render service / cohort handoff | Covered below. |
//! | Mixer / automation coalescing | control adapter -> project controller -> undo | Covered below. |
//! | `SourceProgram` atomic promotion | promotion compiler boundary | Await compiler/promotion receipt API. |
//! | Reading/query provenance | query result / reading import boundary | Await the non-skeleton query API. |
//! | Plugin crash isolation | host worker supervision boundary | Await host-level crash receipt API. |

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::assets::{
    AbsolutePath, AssetFrameRange, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
    AssetRegistry, ContentFingerprint, DecodedAudioMetadata, ProjectRelativePath, SampleFrames,
};
use crate::audio::AudioFormat;
use crate::automation::{
    MixerTarget, ParameterAddress, ParameterDescriptor, ParameterUnit, SegmentShape,
    SmoothingPolicy, TimeDomain, TimePosition, ValueMapping,
};
use crate::command_journal::{decode_runtime_frame, encode_runtime_record, recover_prefix};
use crate::comparison::SourceCitation;
use crate::comparison_runtime::{
    ComparisonRuntimeError, ComparisonSourceResolver, PcmComparisonSourceResolver,
};
use crate::control_views::control_actions::{
    AutomationAction, AutomationActionIntent, ControlAction, ControlEdit, ControlSessionAdapter,
    ControlSessionOperation, MixerAction, MixerActionIntent,
};
use crate::daw_engine::AssetPcmMap;
use crate::daw_project::{DawProject, ProjectDomain};
use crate::daw_render::{PcmAsset, RenderCancellation};
use crate::live_project::{LiveProject, ProjectController};
use crate::render_plan::{
    DeterminismGrade, EngineRecipeStamp, ExactDigest, ProjectRevisionStamp, RenderFormat,
    RenderPlan, RenderPlanId, RenderScope, RenderSpan, Tileability,
};
use crate::render_products::{
    CohortProduct, CohortProductProvenance, PlaybackCohort, PlaybackCohortId, ProductPartition,
    RenderProduct, RenderProductKey, RenderSlot,
};
use crate::render_service::{
    PublicationAction, PublicationBoundary, PublicationGate, PublicationTransport, RenderService,
};
use crate::runtime_command_codec::DeterministicRuntimeCommandCodec;

const RATE: u32 = 48_000;

fn comparison_fixture() -> (AssetRegistry, AssetPcmMap, crate::assets::AssetId) {
    let location = AssetLocation::new(
        Some(AbsolutePath::parse("/cycle9/source.wav").unwrap()),
        Some(ProjectRelativePath::parse("media/source.wav").unwrap()),
    )
    .unwrap();
    let mut registry = AssetRegistry::new();
    let asset = registry
        .register(AssetRegistration {
            name: "comparison source".into(),
            location: location.clone(),
            metadata: DecodedAudioMetadata {
                sample_rate_hz: RATE,
                channels: 2,
                frame_count: SampleFrames(3),
                container: Some("wav".into()),
                codec: Some("pcm_f32le".into()),
                bit_depth: Some(32),
            },
            content: ContentFingerprint::from_bytes(b"cycle9 comparison source"),
            provenance: AssetProvenance::new(
                1,
                AssetOrigin::Generated {
                    generator: "cycle9 acceptance".into(),
                },
                location,
            ),
            tags: BTreeSet::new(),
            favorite: false,
        })
        .unwrap();
    let pcm = BTreeMap::from([(
        asset,
        PcmAsset::new(
            AudioFormat::new(RATE, 2).unwrap(),
            Arc::from([1.0, 10.0, 2.0, 20.0, 3.0, 30.0]),
        )
        .unwrap(),
    )]);
    (registry, pcm, asset)
}

fn citation(asset: crate::assets::AssetId, source_end: u64, project_end: i64) -> SourceCitation {
    SourceCitation {
        asset,
        source_range: AssetFrameRange::new(SampleFrames(0), SampleFrames(source_end)).unwrap(),
        project_span: crate::aspect::FrameSpan {
            start: 100,
            end: project_end,
        },
        channels: crate::aspect::ChannelMask(0b10),
    }
}

#[test]
fn comparison_source_refuses_implicit_resampling_and_keeps_exact_channel_alignment() {
    let (registry, pcm, asset) = comparison_fixture();
    let resolver = PcmComparisonSourceResolver {
        assets: &registry,
        pcm: &pcm,
    };

    // A project span which differs by one frame must not be silently fitted or
    // resampled just because it would make subtraction convenient.
    assert!(matches!(
        resolver.resolve_source(citation(asset, 3, 102), &RenderCancellation::new()),
        Err(ComparisonRuntimeError::SourceResamplingRequired {
            source_frames: 3,
            project_frames: 2,
        })
    ));

    // Selected channels are packed in source-channel order and retain the
    // citation's project origin.  This rejects a common off-by-channel bug
    // that can make a residual look plausible while comparing the wrong side.
    let resolved = resolver
        .resolve_source(citation(asset, 3, 103), &RenderCancellation::new())
        .unwrap();
    assert_eq!(resolved.origin_frame, 100);
    assert_eq!(resolved.audio.interleaved(), &[10.0, 20.0, 30.0]);
    assert_eq!(resolved.audio.format().channels.get(), 1);
}

#[test]
fn comparison_source_observes_cancellation_before_touching_pcm() {
    let (registry, pcm, asset) = comparison_fixture();
    let resolver = PcmComparisonSourceResolver {
        assets: &registry,
        pcm: &pcm,
    };
    let cancellation = RenderCancellation::new();
    cancellation.cancel();

    assert!(matches!(
        resolver.resolve_source(citation(asset, 3, 103), &cancellation),
        Err(ComparisonRuntimeError::Cancelled)
    ));
}

fn project_with_automation_lane() -> (
    ProjectController,
    crate::automation::AutomationLaneId,
    crate::automation::AutomationPointId,
) {
    let mut project = DawProject::new("cycle9 controls", RATE, 120.0).unwrap();
    let master = project.state().domains.mixer.master();
    let mut created = None;
    project
        .transact(
            "install automation fixture",
            project.revisions().aggregate,
            BTreeSet::from([ProjectDomain::Automation]),
            |state| -> Result<(), String> {
                let address = ParameterAddress::Mixer(MixerTarget::BusGain(master.get()));
                state
                    .domains
                    .automation
                    .register_parameter(ParameterDescriptor {
                        address: address.clone(),
                        name: "Master gain".into(),
                        unit: ParameterUnit::Decibels,
                        minimum: -60.0,
                        maximum: 12.0,
                        default: 0.0,
                        mapping: ValueMapping::Linear,
                        smoothing: SmoothingPolicy::LinearFrames(16),
                    })
                    .map_err(|error| error.to_string())?;
                let lane = state
                    .domains
                    .automation
                    .create_lane("Master gain", address, TimeDomain::Frames)
                    .map_err(|error| error.to_string())?;
                let point = state
                    .domains
                    .automation
                    .insert_point(
                        lane,
                        TimePosition::Frames(crate::automation::ProjectFrame(0)),
                        0.0,
                        SegmentShape::Linear,
                    )
                    .map_err(|error| error.to_string())?;
                created = Some((lane, point));
                Ok(())
            },
        )
        .unwrap();
    let (lane, point) = created.unwrap();
    let live = LiveProject::from_project(project, BTreeMap::new()).unwrap();
    (ProjectController::new(live).unwrap(), lane, point)
}

fn dispatch_control(controller: &mut ProjectController, action: ControlAction) {
    let snapshot = controller.snapshot();
    let adapter = ControlSessionAdapter::new(
        controller.revisions().aggregate,
        901,
        &snapshot.project.state().domains.mixer,
        &snapshot.project.state().domains.automation,
    );
    let ControlSessionOperation::Execute(envelope) = adapter.adapt(&action).unwrap() else {
        panic!("fixture only dispatches edit operations")
    };
    controller.execute(envelope).unwrap();
}

#[test]
fn durable_codec_replays_through_fresh_codec_and_reconstructed_project_state() {
    let (mut controller, _, _) = project_with_automation_lane();
    let reconstructed_before_edit = controller.snapshot().project.as_ref().clone();
    let master = controller.snapshot().project.state().domains.mixer.master();
    let mixer_revision = controller
        .snapshot()
        .project
        .state()
        .domains
        .mixer
        .revision();
    dispatch_control(
        &mut controller,
        ControlAction::Mixer(
            MixerActionIntent::new(
                mixer_revision,
                MixerAction::SetGainDb {
                    bus: master,
                    gain_db: -6.0,
                },
            )
            .with_edit(ControlEdit::Numeric),
        ),
    );
    let record = controller.journal_records()[0].clone();

    // A journal byte stream must not depend on codec instance memory.  Decode
    // with a fresh value, then apply it to a separately reconstructed project
    // rather than to the controller which originally emitted it.
    let bytes = encode_runtime_record(&record, &DeterministicRuntimeCommandCodec).unwrap();
    let recovery = recover_prefix(&bytes);
    assert!(recovery.is_complete());
    assert_eq!(recovery.frames.len(), 1);
    let decoded =
        decode_runtime_frame(&recovery.frames[0], &DeterministicRuntimeCommandCodec).unwrap();
    assert_eq!(decoded, record);

    let live = LiveProject::from_project(reconstructed_before_edit, BTreeMap::new()).unwrap();
    let mut replay = ProjectController::new(live).unwrap();
    replay.replay_record(&decoded).unwrap();
    assert_eq!(
        replay
            .snapshot()
            .project
            .state()
            .domains
            .mixer
            .bus(master)
            .unwrap()
            .fader()
            .gain_db(),
        -6.0
    );
}

#[test]
fn controller_coalesces_only_the_same_mixer_or_automation_gesture_series() {
    let (mut controller, lane, point_id) = project_with_automation_lane();
    let master = controller.snapshot().project.state().domains.mixer.master();

    // Two gain updates are one gesture, while an adjacent pan update is a
    // different semantic control and therefore a distinct undo boundary.
    for gain_db in [-9.0, -3.0] {
        let revision = controller
            .snapshot()
            .project
            .state()
            .domains
            .mixer
            .revision();
        dispatch_control(
            &mut controller,
            ControlAction::Mixer(
                MixerActionIntent::new(
                    revision,
                    MixerAction::SetGainDb {
                        bus: master,
                        gain_db,
                    },
                )
                .with_edit(ControlEdit::Gesture { series: 41 }),
            ),
        );
    }
    let revision = controller
        .snapshot()
        .project
        .state()
        .domains
        .mixer
        .revision();
    dispatch_control(
        &mut controller,
        ControlAction::Mixer(
            MixerActionIntent::new(
                revision,
                MixerAction::SetPan {
                    bus: master,
                    pan: 0.5,
                },
            )
            .with_edit(ControlEdit::Gesture { series: 41 }),
        ),
    );
    controller.undo().unwrap().unwrap();
    let fader = controller
        .snapshot()
        .project
        .state()
        .domains
        .mixer
        .bus(master)
        .unwrap()
        .fader();
    assert_eq!(fader.gain_db(), -3.0, "pan undo must preserve gain gesture");
    assert_eq!(fader.pan(), 0.0);
    controller.undo().unwrap().unwrap();
    assert_eq!(
        controller
            .snapshot()
            .project
            .state()
            .domains
            .mixer
            .bus(master)
            .unwrap()
            .fader()
            .gain_db(),
        0.0,
        "coalesced gain gesture must undo to its first before-state"
    );

    // The same invariant applies to automation: repeated point movement is
    // one operation, but changing its segment shape cannot compose with it.
    for coordinate in [4, 8] {
        let (automation_revision, mut point) = {
            let snapshot = controller.snapshot();
            let automation = &snapshot.project.state().domains.automation;
            let point = automation
                .lane(lane)
                .unwrap()
                .points()
                .iter()
                .find(|point| point.id == point_id)
                .unwrap()
                .clone();
            (automation.revision(), point)
        };
        point.position = TimePosition::Frames(crate::automation::ProjectFrame(coordinate));
        dispatch_control(
            &mut controller,
            ControlAction::Automation(
                AutomationActionIntent::new(
                    automation_revision,
                    AutomationAction::MovePoint { lane, point },
                )
                .with_edit(ControlEdit::Gesture { series: 7 }),
            ),
        );
    }
    let automation_revision = controller
        .snapshot()
        .project
        .state()
        .domains
        .automation
        .revision();
    dispatch_control(
        &mut controller,
        ControlAction::Automation(
            AutomationActionIntent::new(
                automation_revision,
                AutomationAction::SetPointShape {
                    lane,
                    point: point_id,
                    shape: SegmentShape::Hold,
                },
            )
            .with_edit(ControlEdit::Gesture { series: 7 }),
        ),
    );
    controller.undo().unwrap().unwrap();
    let after_shape_undo = controller
        .snapshot()
        .project
        .state()
        .domains
        .automation
        .lane(lane)
        .unwrap();
    let point = after_shape_undo
        .points()
        .iter()
        .find(|point| point.id == point_id)
        .unwrap();
    assert_eq!(
        point.position,
        TimePosition::Frames(crate::automation::ProjectFrame(8))
    );
    assert_eq!(point.outgoing, SegmentShape::Linear);
    controller.undo().unwrap().unwrap();
    let after_move_undo = controller
        .snapshot()
        .project
        .state()
        .domains
        .automation
        .lane(lane)
        .unwrap();
    assert_eq!(
        after_move_undo
            .points()
            .iter()
            .find(|point| point.id == point_id)
            .unwrap()
            .position,
        TimePosition::Frames(crate::automation::ProjectFrame(0))
    );
}

fn digest(byte: u8) -> ExactDigest {
    ExactDigest::new([byte; 32])
}

fn render_plan(revision: u64) -> Arc<RenderPlan> {
    let format = RenderFormat::new(RATE, 2).unwrap();
    let engine = EngineRecipeStamp::new(1, format, 128, 0, digest(3)).unwrap();
    let id = RenderPlanId::new(
        77,
        digest(revision as u8),
        ProjectRevisionStamp {
            aggregate: revision,
            ..ProjectRevisionStamp::default()
        },
        RenderSpan::new(0, 64).unwrap(),
        engine,
        Vec::new(),
    )
    .unwrap();
    Arc::new(RenderPlan::new(
        id,
        DeterminismGrade::BitExact,
        Tileability::Stateless,
    ))
}

fn cohort(
    plan: &RenderPlan,
    sequence: u64,
    publication_loop: Option<RenderSpan>,
) -> Arc<PlaybackCohort> {
    let span = RenderSpan::new(0, 64).unwrap();
    let slot = RenderSlot {
        scope: RenderScope::Master,
        span,
    };
    let key = RenderProductKey::new(
        plan.id.clone(),
        RenderScope::Master,
        span,
        ProductPartition::WholeBounce,
        digest(4),
    )
    .unwrap();
    let product =
        Arc::new(RenderProduct::new(digest(sequence as u8), key, vec![0.0; 128].into()).unwrap());
    Arc::new(
        PlaybackCohort::new(
            PlaybackCohortId {
                plan: plan.id.clone(),
                sequence,
            },
            publication_loop,
            vec![slot.clone()],
            vec![CohortProduct {
                slot,
                product,
                provenance: CohortProductProvenance::RenderedForTarget,
            }],
        )
        .unwrap(),
    )
}

fn publish_initial(service: &mut RenderService, plan: &Arc<RenderPlan>) {
    service.submit_target(Arc::clone(plan)).unwrap();
    let action = service.stage_cohort(cohort(plan, 1, None)).unwrap();
    let id = action.cohort().unwrap().id.clone();
    service.acknowledge_publication(&id).unwrap();
}

#[test]
fn an_edit_while_looping_never_hybridizes_cohorts_or_accepts_the_wrong_wrap() {
    let old = render_plan(1);
    let edited = render_plan(2);
    let old_loop = RenderSpan::new(8, 24).unwrap();
    let new_loop = RenderSpan::new(16, 48).unwrap();
    let mut service = RenderService::default();
    publish_initial(&mut service, &old);

    service.update_transport(PublicationTransport {
        rolling: true,
        loop_region: Some(old_loop),
    });
    service.submit_target(Arc::clone(&edited)).unwrap();

    // A render completed for an obsolete loop remains staged, so the old
    // cohort stays audible instead of swapping into a mixed loop iteration.
    assert!(matches!(
        service
            .stage_cohort(cohort(&edited, 2, Some(new_loop)))
            .unwrap(),
        PublicationAction::None
    ));
    assert_eq!(service.active_cohort().unwrap().id.plan, old.id);

    // Once transport and complete candidate agree, only the exact wrap can
    // arm the new cohort.  A different loop boundary is not a valid swap.
    let action = service.update_transport(PublicationTransport {
        rolling: true,
        loop_region: Some(new_loop),
    });
    let ticket = action
        .ticket()
        .expect("matching cohort must arm at loop wrap");
    assert_eq!(ticket.gate, PublicationGate::LoopWrap(new_loop));
    assert!(!ticket.accepts(PublicationBoundary::LoopWrap { region: old_loop }));
    assert!(ticket.accepts(PublicationBoundary::LoopWrap { region: new_loop }));
    let next = ticket.cohort.id.clone();
    service.acknowledge_publication(&next).unwrap();
    assert_eq!(service.active_cohort().unwrap().id.plan, edited.id);
}
