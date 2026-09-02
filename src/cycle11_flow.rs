//! Cycle 11 flow-level semantic tests for major musician actions.
//!
//! This module uses no UI entities and does not treat demo constructors as
//! success. A green test names created [`ObjectRef`]s, undo cohesion, and
//! non-silent PCM — not a revision number.
//!
//! | Action | Boundary exercised here | Failure this would catch |
//! | --- | --- | --- |
//! | Sample / Slice / Beat | workbench → `ConstructivePublication` | Beat succeeding with no pattern id |
//! | Undo Beat | session history → kit / pattern / occurrence | orphan kit or pattern after undo |
//! | Star material | `toggle_asset_favorite` → Inspector `ObjectRef::Material` | favorite lost after an unrelated command |
//! | Automation lane | control adapter → `execute_envelope_revealed` | lane creation with no `ObjectRef::Automation` |
//! | Arrangement duplicate | `execute_arrangement_event_revealed` | receipt naming the source clip |
//! | Pattern cycle audition | `PatternAuditionSessionAdapter` | cycle 0 and 1 rendering identical PCM |
//! | Mixer add return | `execute_control_action_revealed` | return bus with no `ObjectRef::Bus` |
//! | Mixer + insert | `MixerAction::RequestInsert` | silent processor identity without DSP |
//! | Components Keep | `publish_components_evidence` → `keep_reverse_finding` | NMF result with no Finding |

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::arrangement::{ClipId, Frame, TrackKind};
use crate::arrangement_interaction::keyboard::plan_duplicate_after;
use crate::arrangement_interaction::{ArrangementEdit, ArrangementEditIntent, GestureCommit};
use crate::arrangement_view::{ArrangementAction, ArrangementActionIntent, ArrangementViewEvent};
use crate::artifact_catalog::{
    ArtifactDescriptor, ArtifactId, ArtifactKind, ContentDigest, DigestAlgorithm,
};
use crate::aspect::{Aspect, ChannelMask, FrameSpan};
use crate::assets::{
    AbsolutePath, AssetFrameRange, AssetId, AssetLocation, AssetOrigin, AssetProvenance,
    AssetRegistration, AssetRegistry, ContentFingerprint, DecodedAudioMetadata, SampleFrames,
};
use crate::audio::{AudioFormat, ProjectAudio};
use crate::automation::{BindingMode, MixerTarget, ParameterAddress, TimeDomain};
use crate::comparison::{ComparisonDefinition, ComparisonId, SourceCitation};
use crate::control_views::control_actions::{
    AutomationAction, AutomationActionIntent, ControlAction, ControlSessionAdapter,
    ControlSessionOperation, MixerAction, MixerActionIntent, MixerMeterSnapshot,
};
use crate::daw_engine::{
    compile_daw_engine, BuiltInInstrumentDefinition, BuiltInInstrumentRoute, DawEngineConfig,
};
use crate::daw_project::DawProject;
use crate::daw_render::{PcmAsset, RenderCancellation, RenderWindow};
use crate::decomposition::{ComponentDecomposition, ComponentHypothesis};
use crate::explanation::{ExplanationDefinition, ExplanationId, ExplanationScope};
use crate::explorer_model::{
    ExplorerCategory, ExplorerInput, ExplorerMode, ExplorerModel, ExplorerNode,
    ExplorerSemanticCollections, ExplorerTarget,
};
use crate::instruments::{SynthParams, Waveform};
use crate::interpretation::{InterpretationCommand, InterpretationStore};
use crate::live_project::{LiveProject, SourceMaterialMetadata};
use crate::mixer::{BusId, BusKind};
use crate::ontology::{Producer, Provenance};
use crate::pattern_actions::{
    CreatePatternIntent, PatternAction, PatternActionIntent, PatternEdit, PatternEditIntent,
    PatternEditorMode, PatternEditorTarget,
};
use crate::pattern_authoring::{DivergedOverwrite, ExpressionRealizationContext};
use crate::pattern_use_graph::{PatternOccurrenceTarget, PatternUseGraph, PatternUseSnapshot};
use crate::project_audio_controller::AuditionAlignment;
use crate::project_controller::{
    execute_arrangement_event_revealed, execute_control_action_revealed, execute_envelope_revealed,
    execute_pattern_action_revealed, recommend_constructive, FindingKind, FindingLocalId,
    FindingRef, FindingScope, InstrumentRef, ObjectKind, ObjectNavigator, ObjectRef,
    PatternAuditionRequest, PatternAuditionScope, PatternAuditionSessionAdapter,
    PatternAuditionSessionInputs, PatternAuditionStartRequest, PatternOccurrenceRef,
    PatternRevealExecution, RevealRecommendation, WorkbenchSampleIntent, WorkbenchSampleOutcome,
};
use crate::project_selection::{ObjectSelection, SelectionProvenance, SelectionSource};
use crate::project_session::deprojection_workspace_bridge::AnalysisEvidenceKind;
use crate::project_session::{ProjectSession, ProjectSessionId};
use crate::reading::{
    PortableDigest, PortableDigestAlgorithm, ProducerDto, ProvenanceDto, ReadingFile, ReadingId,
    ReadingSource, VerificationTier, READING_FORMAT, READING_FORMAT_VERSION,
};
use crate::render_plan::{
    EngineRecipeStamp, ExactDigest, ProjectRevisionStamp, RenderFormat, RenderPlanId, RenderScope,
    RenderSpan,
};
use crate::render_products::{
    CohortProduct, CohortProductProvenance, PlaybackCohort, PlaybackCohortId, ProductPartition,
    RenderProduct, RenderProductKey, RenderSlot,
};
use crate::render_runtime::{AuditionMix, AuditionOwner, AuditionSubject};
use crate::reverse_surface::{
    ComparisonSurfaceDocument, FindingSurfaceDocument, ReverseSurfaceBody, ReverseSurfaceDocument,
    ReverseSurfaceStore,
};
use crate::reverse_surface_adapter::{keep_reverse_finding, ReverseSurfaceEditKind};
use crate::sample_actions::{
    MakeBeatResultFocus, SampleChopIntent, SampleKitDestination, SampleResultFocus,
};
use crate::sample_material::DerivationScope;
use crate::sequencer::{BeatDuration, TriggerTarget, PPQ};
use crate::session::{Sample, SampleRange};
use crate::ui_drag::DropIntent;
use crate::workspace_document::WorkspaceDocument;
use crate::workspace_items::WorkspaceViewId;

const RATE: u32 = 48_000;
const RENDER_END: i64 = 24_032;

fn aggregate_revision(session: &ProjectSession) -> u64 {
    session.project_snapshot().unwrap().revisions().aggregate
}

fn session_with_source(id: u64) -> (ProjectSession, AssetId) {
    let location = AssetLocation::new(
        Some(AbsolutePath::parse("/cycle11/distinct-source.wav").unwrap()),
        None,
    )
    .unwrap();
    let mut registry = AssetRegistry::new();
    let asset = registry
        .register(AssetRegistration {
            name: "cycle11 source".into(),
            location: location.clone(),
            metadata: DecodedAudioMetadata {
                sample_rate_hz: RATE,
                channels: 1,
                frame_count: SampleFrames(8),
                container: Some("wav".into()),
                codec: Some("pcm_f32le".into()),
                bit_depth: Some(32),
            },
            content: ContentFingerprint::from_bytes(b"cycle11:distinct:pcm"),
            provenance: AssetProvenance::new(
                11,
                AssetOrigin::Generated {
                    generator: "cycle11 flow corpus".into(),
                },
                location,
            ),
            tags: BTreeSet::from(["cycle11".into()]),
            favorite: false,
        })
        .unwrap();
    let pcm = PcmAsset::new(
        AudioFormat::new(RATE, 1).unwrap(),
        Arc::from([0.0, 0.91, -0.32, 0.17, 0.0, 0.63, -0.48, 0.0]),
    )
    .unwrap();
    let live = LiveProject::from_source_material(
        SourceMaterialMetadata::new("Cycle 11", "Distinct source"),
        registry,
        asset,
        pcm,
    )
    .unwrap();
    let mut session = ProjectSession::new(ProjectSessionId(id)).unwrap();
    session.install(live, None).unwrap();
    (session, asset)
}

fn workbench_range() -> SampleRange {
    SampleRange::new(Sample::new(1), Sample::new(7))
}

fn publish_workbench(
    session: &mut ProjectSession,
    intent: WorkbenchSampleIntent,
) -> WorkbenchSampleOutcome {
    session
        .publish_primary_workbench_range(workbench_range(), intent)
        .unwrap()
}

fn sample_slice_beat(
    session: &mut ProjectSession,
) -> (
    WorkbenchSampleOutcome,
    WorkbenchSampleOutcome,
    WorkbenchSampleOutcome,
) {
    let one_shot = publish_workbench(
        session,
        WorkbenchSampleIntent::OneShot {
            kit: SampleKitDestination::NewKit,
            target_bus: None,
        },
    );
    let chop = publish_workbench(
        session,
        WorkbenchSampleIntent::Chop {
            chop: SampleChopIntent::EqualSlices { count: 2 },
            kit: SampleKitDestination::NewKit,
            target_bus: None,
        },
    );
    let beat = publish_workbench(
        session,
        WorkbenchSampleIntent::MakeBeat {
            chop: SampleChopIntent::EqualSlices { count: 2 },
            kit: SampleKitDestination::NewKit,
            target_bus: None,
            bars: 1,
            quantize_ticks: PPQ as u64,
            result_focus: MakeBeatResultFocus::PatternEditor,
        },
    );
    (one_shot, chop, beat)
}

fn asset_is_favorite(session: &ProjectSession, asset: AssetId) -> bool {
    session
        .project_snapshot()
        .unwrap()
        .project
        .state()
        .domains
        .assets
        .get(asset)
        .unwrap()
        .is_favorite()
}

fn create_master_gain_lane(session: &mut ProjectSession) -> RevealRecommendation {
    let action = {
        let snapshot = session.project_snapshot().unwrap();
        let domains = &snapshot.project.state().domains;
        let master = domains.mixer.master();
        ControlAction::Automation(AutomationActionIntent::new(
            domains.automation.revision(),
            AutomationAction::CreateLane {
                name: "Cycle 11 master gain".into(),
                target: ParameterAddress::Mixer(MixerTarget::BusGain(master.get())),
                domain: TimeDomain::Frames,
                binding: BindingMode::Replace,
            },
        ))
    };
    let envelope = {
        let snapshot = session.project_snapshot().unwrap();
        let domains = &snapshot.project.state().domains;
        let adapter = ControlSessionAdapter::new(
            snapshot.revisions().aggregate,
            11_010,
            &domains.mixer,
            &domains.automation,
        );
        match adapter.adapt(&action).unwrap() {
            ControlSessionOperation::Execute(envelope) => envelope,
            ControlSessionOperation::History { .. } => {
                panic!("lane creation must lower to an envelope, not history")
            }
        }
    };
    let receipt = execute_envelope_revealed(session, envelope).unwrap();
    receipt
        .reveal
        .expect("creating an automation lane must reveal ObjectRef::Automation")
}

fn duplicate_clip(session: &mut ProjectSession, clip: ClipId) -> RevealRecommendation {
    let event = {
        let snapshot = session.project_snapshot().unwrap();
        let intent = plan_duplicate_after(
            &snapshot.project.state().domains.arrangement,
            &BTreeSet::from([clip]),
            snapshot.revisions().aggregate,
            1,
        )
        .unwrap();
        ArrangementViewEvent::Commit(GestureCommit {
            selection: None,
            edit: Some(intent),
        })
    };
    let receipt = execute_arrangement_event_revealed(session, event).unwrap();
    receipt
        .reveal
        .expect("arrangement duplicate must reveal the new clip, not a revision number")
}

fn render_interleaved(session: &ProjectSession) -> Vec<f32> {
    let snapshot = session.project_snapshot().unwrap().clone();
    let cancellation = RenderCancellation::new();
    let schedule = compile_daw_engine(
        &snapshot.project,
        &snapshot.pcm,
        RenderWindow::new(0, RENDER_END).unwrap(),
        &DawEngineConfig::default(),
        &cancellation,
    )
    .unwrap();
    schedule
        .render_for_audition(&cancellation)
        .unwrap()
        .audio
        .interleaved()
        .to_vec()
}

fn assert_non_silent(pcm: &[f32], context: &str) {
    assert!(
        pcm.iter().any(|sample| sample.abs() > 0.05),
        "{context} was silent"
    );
}

fn render_audio(session: &ProjectSession, end: i64) -> ProjectAudio {
    let snapshot = session.project_snapshot().unwrap().clone();
    let cancellation = RenderCancellation::new();
    let schedule = compile_daw_engine(
        &snapshot.project,
        &snapshot.pcm,
        RenderWindow::new(0, end).unwrap(),
        &DawEngineConfig::default(),
        &cancellation,
    )
    .unwrap();
    schedule.render_for_audition(&cancellation).unwrap().audio
}

fn master_cohort_from_audio(audio: &ProjectAudio, sequence: u64) -> PlaybackCohort {
    let channels = audio.format().channels.get();
    let samples = audio.interleaved();
    let frames = i64::try_from(samples.len() / usize::from(channels)).unwrap();
    assert!(
        frames > 0,
        "meter cohort requires at least one rendered frame"
    );
    let format = RenderFormat::new(audio.format().sample_rate.get(), channels).unwrap();
    let engine = EngineRecipeStamp::new(1, format, 512, 0, ExactDigest::new([0xC1; 32])).unwrap();
    let span = RenderSpan::new(0, frames).unwrap();
    let plan = RenderPlanId::new(
        11,
        ExactDigest::new([0xC1; 32]),
        ProjectRevisionStamp {
            aggregate: 11,
            ..ProjectRevisionStamp::default()
        },
        span,
        engine,
        Vec::new(),
    )
    .unwrap();
    let slot = RenderSlot {
        scope: RenderScope::Master,
        span,
    };
    let key = RenderProductKey::new(
        plan.clone(),
        RenderScope::Master,
        span,
        ProductPartition::WholeBounce,
        ExactDigest::new([0x11; 32]),
    )
    .unwrap();
    let product = Arc::new(
        RenderProduct::new(
            ExactDigest::new([0x11; 32]),
            key,
            Arc::from(samples.to_vec()),
        )
        .unwrap(),
    );
    PlaybackCohort::new(
        PlaybackCohortId { plan, sequence },
        None,
        vec![slot.clone()],
        vec![CohortProduct {
            slot,
            product,
            provenance: CohortProductProvenance::RenderedForTarget,
        }],
    )
    .unwrap()
}

fn master_meter(audio: &ProjectAudio, master: BusId, sequence: u64) -> MixerMeterSnapshot {
    MixerMeterSnapshot::from_audible_cohort(&master_cohort_from_audio(audio, sequence), master)
}

fn human_provenance() -> Provenance {
    Provenance {
        producer: Producer::Human { name: None },
        created_unix_ms: None,
        source_revision: None,
        note: None,
    }
}

fn category_objects(root: &ExplorerNode, category: ExplorerCategory) -> Vec<ObjectRef> {
    let child = root
        .children
        .iter()
        .find(|node| node.target == ExplorerTarget::Category(category))
        .unwrap_or_else(|| panic!("explorer is missing category {category:?}"));
    child
        .children
        .iter()
        .filter_map(|node| node.as_object().cloned())
        .collect()
}

fn assert_listed_under(
    model: &ExplorerModel,
    object: &ObjectRef,
    mode: ExplorerMode,
    category: ExplorerCategory,
) {
    let id = model
        .object_node(object)
        .unwrap_or_else(|| panic!("{object:?} missing from explorer"));
    let crumb = model.breadcrumb(id);
    assert_eq!(
        crumb.first().map(String::as_str),
        Some(mode.label()),
        "{object:?} listed under {crumb:?}, not {}",
        mode.label()
    );
    assert_eq!(
        crumb.get(1).map(String::as_str),
        Some(category.label()),
        "{object:?} listed under {crumb:?}, not {}",
        category.label()
    );
    assert_eq!(
        category_objects(model.root(mode), category),
        vec![object.clone()],
        "{object:?} must be the sole {category:?} child"
    );
}

fn beat_occurrence(
    session: &ProjectSession,
    pattern: crate::sequencer::PatternId,
) -> PatternOccurrenceTarget {
    PatternUseGraph::build(PatternUseSnapshot::from_project(
        &session.project_snapshot().unwrap().project,
    ))
    .unwrap()
    .pattern(pattern)
    .unwrap()
    .occurrences[0]
        .target
}

fn audition_cycle_pcm(
    session: &mut ProjectSession,
    occurrence: PatternOccurrenceTarget,
    cycle_index: u64,
    inputs: PatternAuditionSessionInputs,
) -> Vec<f32> {
    let revision = session.project_snapshot().unwrap().revisions().aggregate;
    let preview = execute_pattern_action_revealed(
        session,
        &PatternActionIntent {
            expected_project_revision: revision,
            action: PatternAction::PreviewCycle {
                target: PatternEditorTarget {
                    pattern: occurrence.pattern,
                    mode: PatternEditorMode::Steps,
                },
                cycle_index,
                performance_seed: 0xC11,
            },
        },
    )
    .unwrap();
    let PatternRevealExecution::PreviewCycle {
        cycle_index: previewed_cycle,
        performance_seed,
        ..
    } = preview
    else {
        panic!("PreviewCycle must remain a non-mutating pattern action")
    };
    assert_eq!(previewed_cycle, cycle_index);
    let request = PatternAuditionStartRequest::new(
        PatternAuditionRequest {
            expected_project_revision: aggregate_revision(session),
            occurrence,
            cycle_index: previewed_cycle,
            performance_seed,
            scope: PatternAuditionScope::Pattern,
        },
        AuditionOwner {
            namespace: 11,
            local: cycle_index + 1,
        },
        AuditionSubject::Construction,
        AuditionMix::Replace,
    );
    let mut adapter = PatternAuditionSessionAdapter::default();
    adapter
        .prepare(session, request, inputs)
        .unwrap()
        .execute()
        .result
        .unwrap()
        .render
        .audio
        .interleaved()
        .to_vec()
}

fn instrument_note_inputs(session: &ProjectSession) -> PatternAuditionSessionInputs {
    let master = session
        .project_snapshot()
        .unwrap()
        .project
        .state()
        .domains
        .mixer
        .master();
    let mut saw = SynthParams::default();
    saw.waveform = Waveform::Saw;
    let mut sine = SynthParams::default();
    sine.waveform = Waveform::Sine;
    let mut engine = DawEngineConfig::default();
    engine.instruments.insert(
        11,
        BuiltInInstrumentRoute {
            definition: BuiltInInstrumentDefinition::Subtractive(saw),
            bus: master,
        },
    );
    engine.instruments.insert(
        22,
        BuiltInInstrumentRoute {
            definition: BuiltInInstrumentDefinition::Subtractive(sine),
            bus: master,
        },
    );
    PatternAuditionSessionInputs::from_session_and_engine(session, Arc::new(engine)).unwrap()
}

fn inspect_material(session: &mut ProjectSession, asset: AssetId) {
    assert!(session
        .replace_object_selection(
            ObjectSelection {
                primary: Some(ObjectRef::Material(asset)),
                ..ObjectSelection::default()
            },
            SelectionProvenance {
                source: SelectionSource::Inspector,
                source_view: Some(WorkspaceViewId(11_001)),
            },
        )
        .unwrap());
    assert_eq!(
        session.selection().selection.objects.primary,
        Some(ObjectRef::Material(asset))
    );
}

#[test]
fn headless_session_sample_slice_beat_star_automate_and_audition_pattern_cycle() {
    let (mut session, asset) = session_with_source(11_011);
    let (one_shot, chop, beat) = sample_slice_beat(&mut session);

    assert!(one_shot.constructive.publication.pattern.is_none());
    assert!(one_shot.constructive.publication.arrangement_clip.is_none());
    assert_eq!(one_shot.constructive.publication.created_zones.len(), 1);
    let one_shot_reveal = recommend_constructive(&one_shot.constructive.publication);
    assert!(matches!(
        one_shot_reveal.request.object,
        ObjectRef::Pad(_) | ObjectRef::Instrument(InstrumentRef::SampleKit(_))
    ));

    assert!(chop.constructive.publication.pattern.is_none());
    assert!(chop.constructive.publication.arrangement_clip.is_none());
    assert_eq!(chop.constructive.publication.created_zones.len(), 2);
    assert_ne!(
        chop.constructive.publication.kit,
        one_shot.constructive.publication.kit
    );

    let publication = &beat.constructive.publication;
    let pattern = publication.pattern.expect("Beat must publish a pattern id");
    let arrangement_clip = publication
        .arrangement_clip
        .expect("Beat must publish an arrangement occurrence");
    assert_ne!(publication.kit, chop.constructive.publication.kit);
    assert_eq!(publication.created_zones.len(), 2);
    assert!(session
        .project_snapshot()
        .unwrap()
        .project
        .state()
        .domains
        .sequencer
        .patterns()
        .get(pattern)
        .is_some());

    let recommendation = recommend_constructive(publication);
    assert_eq!(recommendation.request.object, ObjectRef::Pattern(pattern));
    assert!(recommendation
        .request
        .related
        .contains(&ObjectRef::PatternOccurrence(PatternOccurrenceRef {
            arrangement_clip,
            sequencer_clip: publication.sequencer_clip,
            pattern: Some(pattern),
        })));
    assert!(recommendation
        .request
        .related
        .contains(&ObjectRef::Instrument(InstrumentRef::SampleKit(
            publication.kit
        ))));
    let planned = ObjectNavigator::plan(&WorkspaceDocument::default(), recommendation.request);
    assert_eq!(planned.selection.primary, ObjectRef::Pattern(pattern));
    assert_eq!(SampleResultFocus::Pattern(pattern).sampler_retarget(), None);

    assert_non_silent(&render_interleaved(&session), "Beat arrangement render");

    session.toggle_asset_favorite(asset).unwrap();
    assert!(asset_is_favorite(&session, asset));

    let lane_reveal = create_master_gain_lane(&mut session);
    let ObjectRef::Automation(lane) = lane_reveal.request.object else {
        panic!(
            "automation lane must reveal ObjectRef::Automation, got {:?}",
            lane_reveal.request.object
        )
    };
    assert!(session
        .project_snapshot()
        .unwrap()
        .project
        .state()
        .domains
        .automation
        .lane(lane)
        .is_some());
    assert!(
        asset_is_favorite(&session, asset),
        "starring must survive a subsequent automation command"
    );
    inspect_material(&mut session, asset);

    let occurrence = beat_occurrence(&session, pattern);
    let inputs = PatternAuditionSessionInputs::from_session(&session).unwrap();
    let cycle = audition_cycle_pcm(&mut session, occurrence, 0, inputs);
    assert_non_silent(&cycle, "Beat pattern cycle 0 audition");
}

#[test]
fn undoing_beat_removes_kit_pattern_and_occurrence_together() {
    let (mut session, _) = session_with_source(11_012);
    let (one_shot, chop, beat) = sample_slice_beat(&mut session);
    let publication = beat.constructive.publication.clone();
    let pattern = publication.pattern.expect("Beat must publish a pattern id");
    let arrangement_clip = publication
        .arrangement_clip
        .expect("Beat must publish an arrangement occurrence");
    let beat_kit = publication.kit;
    let chop_kit = chop.constructive.publication.kit;
    let one_shot_kit = one_shot.constructive.publication.kit;
    let kits_after_beat = session
        .project_snapshot()
        .unwrap()
        .project
        .state()
        .domains
        .sample_kits
        .kits
        .len();
    assert_eq!(kits_after_beat, 3);

    session.undo().unwrap().unwrap();

    let snapshot = session.project_snapshot().unwrap().clone();
    let domains = &snapshot.project.state().domains;
    assert!(
        domains.sample_kits.kits.get(&beat_kit).is_none(),
        "undo of Beat left its kit"
    );
    assert!(domains.sample_kits.kits.contains_key(&chop_kit));
    assert!(domains.sample_kits.kits.contains_key(&one_shot_kit));
    assert_eq!(domains.sample_kits.kits.len(), 2);
    assert!(
        domains.sequencer.patterns().get(pattern).is_none(),
        "undo of Beat left its pattern"
    );
    assert!(domains.arrangement.clip(arrangement_clip).is_none());
    if let Some(sequencer_clip) = publication.sequencer_clip {
        assert!(domains.sequencer.clip(sequencer_clip).is_none());
    }
    for clip in domains.sequencer.clips() {
        assert_ne!(
            clip.pattern, pattern,
            "undo of Beat left a sequencer clip bound to the deleted pattern"
        );
    }
    for target in &publication.created_zones {
        assert!(
            !snapshot.sample_pcm.contains_key(target),
            "undo of Beat left zone PCM for {target:?}"
        );
    }
}

#[test]
fn starred_material_survives_unrelated_command_and_inspector_selects_material_object_ref() {
    let (mut session, asset) = session_with_source(11_013);
    assert!(!asset_is_favorite(&session, asset));

    session.toggle_asset_favorite(asset).unwrap();
    assert!(asset_is_favorite(&session, asset));
    inspect_material(&mut session, asset);

    let expected_revision = aggregate_revision(&session);
    let track_receipt = execute_arrangement_event_revealed(
        &mut session,
        ArrangementViewEvent::Action(ArrangementActionIntent {
            expected_revision,
            action: ArrangementAction::CreateTrack {
                kind: TrackKind::Audio,
            },
        }),
    )
    .unwrap();
    let ObjectRef::Track(track) = track_receipt
        .reveal
        .as_ref()
        .expect("track creation must reveal ObjectRef::Track")
        .request
        .object
    else {
        panic!(
            "track creation must name the new track, got {:?}",
            track_receipt.reveal.unwrap().request.object
        )
    };
    assert!(session
        .project_snapshot()
        .unwrap()
        .project
        .state()
        .domains
        .arrangement
        .track(track)
        .is_some());
    assert!(
        asset_is_favorite(&session, asset),
        "favorite was lost after an unrelated arrangement command"
    );

    inspect_material(&mut session, asset);
    session.undo().unwrap().unwrap();
    assert!(asset_is_favorite(&session, asset));
    session.undo().unwrap().unwrap();
    assert!(!asset_is_favorite(&session, asset));
    assert_eq!(
        session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .assets
            .get(asset)
            .unwrap()
            .id(),
        asset
    );
}

#[test]
fn arrangement_duplicate_receipt_names_the_new_clip_not_the_source() {
    let (mut session, _) = session_with_source(11_014);
    let source_clip = session.live_project().unwrap().source_ids().clip;
    let audio_reveal = duplicate_clip(&mut session, source_clip);
    let ObjectRef::AudioClip(duplicated_audio) = audio_reveal.request.object else {
        panic!(
            "audio clip duplicate must reveal ObjectRef::AudioClip, got {:?}",
            audio_reveal.request.object
        )
    };
    assert_ne!(
        duplicated_audio, source_clip,
        "duplicate receipt named the source audio clip"
    );
    let arrangement = &session
        .project_snapshot()
        .unwrap()
        .project
        .state()
        .domains
        .arrangement;
    assert!(arrangement.clip(source_clip).is_some());
    assert!(arrangement.clip(duplicated_audio).is_some());

    let (_, _, beat) = sample_slice_beat(&mut session);
    let publication = &beat.constructive.publication;
    let pattern = publication.pattern.expect("Beat must publish a pattern id");
    let arrangement_clip = publication
        .arrangement_clip
        .expect("Beat must publish an arrangement occurrence");
    let sequencer_clip = publication.sequencer_clip;
    let pattern_reveal = duplicate_clip(&mut session, arrangement_clip);
    let ObjectRef::PatternOccurrence(occurrence) = pattern_reveal.request.object else {
        panic!(
            "pattern clip duplicate must reveal ObjectRef::PatternOccurrence, got {:?}",
            pattern_reveal.request.object
        )
    };
    assert_ne!(
        occurrence.arrangement_clip, arrangement_clip,
        "duplicate receipt named the source pattern clip"
    );
    assert_eq!(occurrence.pattern, Some(pattern));
    assert_ne!(occurrence.sequencer_clip, sequencer_clip);
    assert!(occurrence.sequencer_clip.is_some());
    let domains = &session.project_snapshot().unwrap().project.state().domains;
    assert!(domains.arrangement.clip(arrangement_clip).is_some());
    assert!(domains
        .arrangement
        .clip(occurrence.arrangement_clip)
        .is_some());
    assert!(domains.sequencer.patterns().get(pattern).is_some());
}

#[test]
fn alternating_pattern_cycles_render_distinct_non_silent_pcm() {
    let project = DawProject::new("Cycle 11 alternation", RATE, 120.0).unwrap();
    let live = LiveProject::from_project(project, BTreeMap::new()).unwrap();
    let mut session = ProjectSession::new(ProjectSessionId(11_015)).unwrap();
    session.install(live, None).unwrap();

    let expected_revision = aggregate_revision(&session);
    let created = execute_pattern_action_revealed(
        &mut session,
        &PatternActionIntent {
            expected_project_revision: expected_revision,
            action: PatternAction::Create(CreatePatternIntent {
                mode: PatternEditorMode::Steps,
                name: "Cycle 11 alternating".into(),
                length: BeatDuration((PPQ * 4) as u64),
                step_resolution: BeatDuration((PPQ / 4) as u64),
                initial_target: None,
            }),
        },
    )
    .unwrap();
    let PatternRevealExecution::ProjectChanged(receipt) = created else {
        panic!("pattern create must publish")
    };
    let ObjectRef::Pattern(pattern) = receipt
        .reveal
        .expect("pattern create must reveal ObjectRef::Pattern")
        .request
        .object
    else {
        panic!("pattern create must name the new pattern")
    };
    let pattern_revision = session
        .project_snapshot()
        .unwrap()
        .project
        .state()
        .domains
        .sequencer
        .patterns()
        .get(pattern)
        .unwrap()
        .revision;

    let expected_revision = aggregate_revision(&session);
    execute_pattern_action_revealed(
        &mut session,
        &PatternActionIntent {
            expected_project_revision: expected_revision,
            action: PatternAction::Edit(PatternEditIntent {
                pattern,
                expected_pattern_revision: pattern_revision,
                edit: PatternEdit::ApplyExpression {
                    source: "<a b>".into(),
                    bindings: BTreeMap::from([
                        (
                            "a".into(),
                            TriggerTarget::InstrumentNote {
                                instrument: 11,
                                key: 48,
                            },
                        ),
                        (
                            "b".into(),
                            TriggerTarget::InstrumentNote {
                                instrument: 22,
                                key: 72,
                            },
                        ),
                    ]),
                    overwrite: DivergedOverwrite::Refuse,
                    realization: ExpressionRealizationContext::default(),
                },
            }),
        },
    )
    .unwrap();

    let expected_revision = aggregate_revision(&session);
    let drop = execute_arrangement_event_revealed(
        &mut session,
        ArrangementViewEvent::Action(ArrangementActionIntent {
            expected_revision,
            action: ArrangementAction::Drop(DropIntent::InsertPattern {
                pattern,
                track: None,
                at: Frame::ZERO,
                make_unique: false,
            }),
        }),
    )
    .unwrap();
    let ObjectRef::PatternOccurrence(placed) = drop
        .reveal
        .expect("pattern insert must reveal the new occurrence")
        .request
        .object
    else {
        panic!("pattern insert must name a PatternOccurrence")
    };
    assert_eq!(placed.pattern, Some(pattern));

    let occurrence = PatternUseGraph::build(PatternUseSnapshot::from_project(
        &session.project_snapshot().unwrap().project,
    ))
    .unwrap()
    .occurrence_for_clip(placed.arrangement_clip)
    .unwrap()
    .clone();
    let boundary = Frame(
        occurrence
            .placement
            .start
            .0
            .checked_add((occurrence.placement.len() as i64) * 2)
            .unwrap(),
    );
    let expected_revision = aggregate_revision(&session);
    execute_arrangement_event_revealed(
        &mut session,
        ArrangementViewEvent::Commit(GestureCommit {
            selection: None,
            edit: Some(ArrangementEditIntent {
                expected_revision,
                edit: ArrangementEdit::SetRepeatBoundary {
                    clip_id: occurrence.target.arrangement_clip,
                    boundary,
                },
            }),
        }),
    )
    .unwrap();
    let target = PatternUseGraph::build(PatternUseSnapshot::from_project(
        &session.project_snapshot().unwrap().project,
    ))
    .unwrap()
    .occurrence_for_clip(occurrence.target.arrangement_clip)
    .unwrap()
    .target;

    let inputs = instrument_note_inputs(&session);
    let cycle_0 = audition_cycle_pcm(&mut session, target, 0, inputs.clone());
    let cycle_1 = audition_cycle_pcm(&mut session, target, 1, inputs);
    assert_non_silent(&cycle_0, "alternating pattern cycle 0");
    assert_non_silent(&cycle_1, "alternating pattern cycle 1");
    assert_ne!(
        cycle_0, cycle_1,
        "alternating pattern cycles 0 and 1 rendered identical PCM"
    );
}

#[test]
fn beat_master_meter_from_audible_cohort_is_non_silent() {
    let (mut session, _) = session_with_source(11_016);
    let (_, _, beat) = sample_slice_beat(&mut session);
    assert!(beat.constructive.publication.pattern.is_some());
    let master = session
        .project_snapshot()
        .unwrap()
        .project
        .state()
        .domains
        .mixer
        .master();
    let beat_audio = render_audio(&session, 64);
    assert_non_silent(beat_audio.interleaved(), "Beat master bounce");
    let snapshot = master_meter(&beat_audio, master, 11);
    let beat_meter = snapshot
        .buses
        .get(&master)
        .expect("master bus missing from audible-cohort meter");
    assert!(
        beat_meter.peak_db > -120.0,
        "Beat master meter was silent: peak_db={}",
        beat_meter.peak_db
    );
    assert!(
        beat_meter.peak_db.is_finite() && beat_meter.rms_db.is_finite(),
        "Beat master meter was not sanitizable: {beat_meter:?}"
    );
    assert!(
        !snapshot.products[&master].is_empty(),
        "Beat master meter named no rendered product"
    );

    let silent_project = DawProject::new("Cycle 11 silent master", RATE, 120.0).unwrap();
    let silent_master = silent_project.state().domains.mixer.master();
    let cancellation = RenderCancellation::new();
    let silent_audio = compile_daw_engine(
        &silent_project,
        &BTreeMap::new(),
        RenderWindow::new(0, 64).unwrap(),
        &DawEngineConfig::default(),
        &cancellation,
    )
    .unwrap()
    .render_for_audition(&cancellation)
    .unwrap()
    .audio;
    assert!(
        silent_audio
            .interleaved()
            .iter()
            .all(|sample| sample.abs() <= 1.0e-6),
        "empty project bounce was not silence: {:?}",
        silent_audio.interleaved()
    );
    let silent = master_meter(&silent_audio, silent_master, 12);
    let silent_meter = silent
        .buses
        .get(&silent_master)
        .expect("silent master bus dropped by sanitization");
    assert_eq!(
        silent_meter.peak_db, -120.0,
        "empty master bounce must meter as the silence floor, got {silent_meter:?}"
    );
}

#[test]
fn reverse_documents_list_finding_explanation_comparison_and_reading_separately() {
    let (session, _) = session_with_source(11_017);
    let finding = FindingRef {
        kind: FindingKind::Rhythm,
        scope: FindingScope::Derivation(DerivationScope(11)),
        local: FindingLocalId::Claim(17),
    };
    let mut explanation = ExplanationDefinition {
        id: ExplanationId(11),
        label: "Cycle 11 construction".into(),
        scope: ExplanationScope::ArrangementClip(ClipId::from_raw(11)),
        extent: Aspect::All,
        evidence: Vec::new(),
        provenance: human_provenance(),
    };
    explanation.normalize_and_validate().unwrap();
    let comparison = ComparisonSurfaceDocument {
        definition: ComparisonDefinition {
            id: ComparisonId(22),
            label: "Cycle 11 null test".into(),
            source: SourceCitation {
                asset: AssetId(11),
                source_range: AssetFrameRange::new(SampleFrames(0), SampleFrames(8)).unwrap(),
                project_span: FrameSpan { start: 0, end: 8 },
                channels: ChannelMask(1),
            },
            explanation: explanation.id,
            provenance: human_provenance(),
        },
        observation: None,
        coverage: None,
    };
    let reading_id = ReadingId::new([0x11; 16]).unwrap();
    let documents = [
        ReverseSurfaceDocument::finding(
            FindingSurfaceDocument {
                finding,
                label: "Cycle 11 syncopation candidate".into(),
                artifact: None,
                extent: Some(FrameSpan { start: 0, end: 8 }),
                statements: vec!["anonymous family repeats".into()],
            },
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap(),
        ReverseSurfaceDocument::explanation(explanation.clone(), Vec::new()).unwrap(),
        ReverseSurfaceDocument::from_comparison(comparison.clone()).unwrap(),
        ReverseSurfaceDocument::reading(
            ReadingFile {
                format: READING_FORMAT.into(),
                version: READING_FORMAT_VERSION,
                reading_id,
                revision: 1,
                parents: Vec::new(),
                author: ProvenanceDto {
                    producer: ProducerDto::Human { name: None },
                    created_unix_ms: None,
                    source_revision: None,
                    note: None,
                },
                source: ReadingSource {
                    fingerprints: vec![PortableDigest {
                        algorithm: PortableDigestAlgorithm::Sha256,
                        bytes: [0x11; 32],
                    }],
                    sample_rate: RATE,
                    channels: 1,
                    frame_count: 8,
                    declared_title: Some("Cycle 11 portable reading".into()),
                    extensions: BTreeMap::new(),
                },
                sections: Vec::new(),
                attachments: Vec::new(),
                extensions: BTreeMap::new(),
            },
            Ok(VerificationTier::GraphOnly),
        )
        .unwrap(),
    ];
    assert_eq!(
        documents
            .iter()
            .map(|document| document.object.kind())
            .collect::<Vec<_>>(),
        vec![
            ObjectKind::Finding,
            ObjectKind::Explanation,
            ObjectKind::Comparison,
            ObjectKind::Reading,
        ]
    );

    let mut surface = ReverseSurfaceStore::new();
    for document in &documents {
        surface.insert(document.clone()).unwrap();
    }
    assert_eq!(surface.len(), 4);
    assert!(matches!(
        surface.get(&ObjectRef::Finding(finding)).unwrap().body,
        ReverseSurfaceBody::Finding(_)
    ));
    assert!(matches!(
        surface
            .get(&ObjectRef::Explanation(explanation.id))
            .unwrap()
            .body,
        ReverseSurfaceBody::Explanation(_)
    ));
    assert!(matches!(
        surface
            .get(&ObjectRef::Comparison(comparison.definition.id))
            .unwrap()
            .body,
        ReverseSurfaceBody::Comparison(_)
    ));
    assert!(matches!(
        surface.get(&ObjectRef::Reading(reading_id)).unwrap().body,
        ReverseSurfaceBody::Reading(_)
    ));

    let mut extra_explanation = ExplanationDefinition {
        id: ExplanationId(33),
        label: "Cycle 11 store-only construction".into(),
        scope: ExplanationScope::ArrangementClip(ClipId::from_raw(33)),
        extent: Aspect::All,
        evidence: Vec::new(),
        provenance: human_provenance(),
    };
    extra_explanation.normalize_and_validate().unwrap();
    let mut interpretations = InterpretationStore::new();
    interpretations
        .apply(&[
            InterpretationCommand::PutExplanation {
                before: None,
                after: Some(explanation.clone()),
            },
            InterpretationCommand::PutExplanation {
                before: None,
                after: Some(extra_explanation.clone()),
            },
            InterpretationCommand::PutComparison {
                before: None,
                after: Some(comparison.definition.clone()),
            },
        ])
        .unwrap();
    assert_eq!(interpretations.explanations().len(), 2);
    assert_eq!(interpretations.comparisons().len(), 1);
    assert!(
        interpretations
            .explanation(ExplanationId(comparison.definition.id.0))
            .is_none(),
        "comparison identity leaked into the explanation map"
    );

    let collections = ExplorerSemanticCollections::from_reverse_documents(&documents)
        .include_interpretations(&interpretations);
    assert_eq!(collections.findings, vec![finding]);
    assert_eq!(
        collections.explanations,
        vec![explanation.id, extra_explanation.id]
    );
    assert_eq!(collections.comparisons, vec![comparison.definition.id]);
    assert_eq!(collections.readings.len(), 1);
    assert_eq!(collections.readings[0].id, reading_id);
    assert_eq!(collections.readings[0].title, "Cycle 11 portable reading");
    assert_eq!(collections.readings[0].verification, "GraphOnly");

    let snapshot = session.project_snapshot().unwrap();
    let model = ExplorerModel::build(ExplorerInput::from_collections(
        snapshot.project.as_ref(),
        &collections,
    ));
    assert_listed_under(
        &model,
        &ObjectRef::Finding(finding),
        ExplorerMode::Investigate,
        ExplorerCategory::Findings,
    );
    assert_eq!(
        category_objects(
            model.root(ExplorerMode::Investigate),
            ExplorerCategory::Explanations
        ),
        vec![
            ObjectRef::Explanation(explanation.id),
            ObjectRef::Explanation(extra_explanation.id),
        ]
    );
    for object in [
        ObjectRef::Explanation(explanation.id),
        ObjectRef::Explanation(extra_explanation.id),
    ] {
        let crumb = model.breadcrumb(
            model
                .object_node(&object)
                .unwrap_or_else(|| panic!("{object:?} missing from explorer")),
        );
        assert_eq!(crumb.first().map(String::as_str), Some("Investigate"));
        assert_eq!(crumb.get(1).map(String::as_str), Some("Explanations"));
    }
    assert_listed_under(
        &model,
        &ObjectRef::Comparison(comparison.definition.id),
        ExplorerMode::Investigate,
        ExplorerCategory::Comparisons,
    );
    assert_listed_under(
        &model,
        &ObjectRef::Reading(reading_id),
        ExplorerMode::Readings,
        ExplorerCategory::ImportedReadings,
    );
    assert!(
        !model
            .root(ExplorerMode::Investigate)
            .children
            .iter()
            .any(|node| node.target == ExplorerTarget::Category(ExplorerCategory::ImportedReadings)),
        "readings collapsed into Investigate"
    );
    let project = model.root(ExplorerMode::Project);
    for category in [
        ExplorerCategory::Findings,
        ExplorerCategory::Explanations,
        ExplorerCategory::Comparisons,
        ExplorerCategory::ImportedReadings,
    ] {
        assert!(
            project
                .children
                .iter()
                .all(|node| node.target != ExplorerTarget::Category(category)),
            "{category:?} leaked into Project mode"
        );
    }
}

#[test]
fn preview_key_audition_adopts_preserve_transport() {
    assert_eq!(
        PatternAuditionSessionInputs::adoption_for_scope(&PatternAuditionScope::PreviewKey {
            instrument: 11,
            midi_key: 60,
            velocity_millis: 820,
            duration_ticks: 240,
        }),
        AuditionAlignment::PreserveTransport
    );
    assert_eq!(
        PatternAuditionSessionInputs::adoption_for_scope(&PatternAuditionScope::Pattern),
        AuditionAlignment::LoopSpan { play: true }
    );
}

#[test]
fn add_return_reveals_the_created_bus_and_undo_removes_it() {
    let (mut session, _) = session_with_source(11_031);
    let mixer = session
        .project_snapshot()
        .unwrap()
        .project
        .state()
        .domains
        .mixer
        .clone();
    let receipt = execute_control_action_revealed(
        &mut session,
        1,
        ControlAction::Mixer(MixerActionIntent::new(
            mixer.revision(),
            MixerAction::AddReturn {
                name: "Room".into(),
            },
        )),
    )
    .unwrap();
    let ObjectRef::Bus(id) = receipt.primary.clone().expect("AddReturn must name a bus") else {
        panic!("AddReturn primary was {:?}", receipt.primary);
    };
    assert_eq!(
        session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .mixer
            .bus(id)
            .unwrap()
            .kind(),
        BusKind::Return
    );
    session.undo().unwrap();
    assert!(session
        .project_snapshot()
        .unwrap()
        .project
        .state()
        .domains
        .mixer
        .bus(id)
        .is_none());
}

#[test]
fn request_insert_is_refused_without_allocating_a_processor() {
    let (mut session, _) = session_with_source(11_032);
    let mixer = session
        .project_snapshot()
        .unwrap()
        .project
        .state()
        .domains
        .mixer
        .clone();
    let bus = mixer
        .buses()
        .find(|bus| bus.kind() == BusKind::Source)
        .map(|bus| bus.id())
        .unwrap_or_else(|| mixer.master());
    let before_revision = mixer.revision();
    let before_inserts = mixer.buses().flat_map(|bus| bus.inserts().iter()).count();
    let error = execute_control_action_revealed(
        &mut session,
        1,
        ControlAction::Mixer(MixerActionIntent::new(
            mixer.revision(),
            MixerAction::RequestInsert { bus },
        )),
    )
    .expect_err("plugin insert must refuse while the reference renderer bypasses processors");
    assert!(
        error.to_string().contains("plugin host is not connected"),
        "insert refuse was not the plugin-host error: {error}"
    );
    let after = session
        .project_snapshot()
        .unwrap()
        .project
        .state()
        .domains
        .mixer
        .clone();
    assert_eq!(after.revision(), before_revision);
    assert_eq!(
        after.buses().flat_map(|bus| bus.inserts().iter()).count(),
        before_inserts
    );
}

#[test]
fn component_magnitude_keep_names_a_finding_without_a_promotion_candidate() {
    let (mut session, _) = session_with_source(11_033);
    let digest = ContentDigest::new(DigestAlgorithm::Sha256, [0x47; 32]);
    let descriptor = ArtifactDescriptor {
        id: ArtifactId(digest),
        kind: ArtifactKind::Components,
        source_digest: digest,
        recipe_digest: digest,
        output_digest: digest,
        extent: FrameSpan { start: 0, end: 8 },
        sample_rate: RATE,
        channels: 1,
        provenance: human_provenance(),
    };
    let decomposition = ComponentDecomposition {
        frequency_bins: 2,
        frames: 3,
        components: vec![
            ComponentHypothesis {
                spectral_template: vec![0.75, 0.25],
                activation: vec![1.0, 0.4, 0.0],
                energy_share: 0.62,
                spectral_distinctness: 0.35,
                confidence: 0.55,
            },
            ComponentHypothesis {
                spectral_template: vec![0.2, 0.8],
                activation: vec![0.0, 0.5, 1.0],
                energy_share: 0.38,
                spectral_distinctness: 0.35,
                confidence: 0.48,
            },
        ],
        iterations_run: 4,
        reconstruction_rmse: 0.02,
        relative_error: 0.05,
        explained_energy: 0.91,
        confidence: 0.5,
        silent: false,
        gestures: None,
    };
    let published = session
        .publish_components_evidence(
            descriptor.clone(),
            decomposition,
            &RenderCancellation::new(),
        )
        .unwrap();
    assert_eq!(published.len(), 2);
    assert_eq!(
        published[0].kind,
        AnalysisEvidenceKind::ComponentMagnitude { index: 0 }
    );
    assert!(published.iter().all(|summary| {
        summary.finding.kind == FindingKind::Components
            && summary.finding.scope == FindingScope::Artifact(descriptor.id)
    }));
    assert!(session
        .list_deprojection_workspace_candidates()
        .unwrap()
        .is_empty());
    let kept =
        keep_reverse_finding(&session, &ObjectRef::Finding(published[0].finding), None).unwrap();
    assert_eq!(kept.kind, ReverseSurfaceEditKind::Kept);
    assert_eq!(kept.primary, ObjectRef::Finding(published[0].finding));
}

// Skipped Cycle 11 flow cases:
// - execute_arrangement_event_revealed duplicate names the NEW clip — already covered by
//   arrangement_duplicate_receipt_names_the_new_clip_not_the_source.

/// The desktop app renders through `ProjectAudioController`, whose executable
/// plan compiles the native graph. Every other test here renders through the
/// reference schedule, which is why a made beat could be inaudible on the
/// desktop while headless stayed green: the recipe declared `Stateless` and
/// the graph's retained-voice instrument refused it.
#[test]
fn made_beat_renders_audibly_through_the_native_controller_path() {
    use crate::project_audio_controller::{
        ProjectAudioController, ProjectAudioRenderProducts, ProjectAudioRenderRecipe,
    };
    use crate::project_session::ProjectPublication;

    let (mut session, _asset) = session_with_source(11_077);
    let (_one_shot, _chop, beat) = sample_slice_beat(&mut session);
    assert!(beat.constructive.publication.pattern.is_some());

    let snapshot = session.project_snapshot().unwrap().clone();
    let publication = ProjectPublication {
        generation: session.snapshot().generation,
        revisions: session.snapshot().revisions().unwrap(),
        snapshot,
        change_set: None,
    };
    let recipe = ProjectAudioRenderRecipe::session_audition(&publication, session.id()).unwrap();
    let mut controller = ProjectAudioController::new();
    let job = controller.request_render(publication, recipe);
    let completion = job
        .execute(&RenderCancellation::new())
        .expect("a project with a sampler kit must still compile and render");
    assert!(
        completion
            .diagnostics
            .iter()
            .any(|line| line.contains("tileability tightened")),
        "the Stateless recipe must be tightened, not refused: {:?}",
        completion.diagnostics
    );
    let ProjectAudioRenderProducts::Whole { product } = &completion.products else {
        panic!("a cold controller render publishes one whole product");
    };
    assert_non_silent(product.interleaved(), "native-path master of the made beat");
}

/// Make beat places its pattern where the selected material sounds, so a
/// musician who loops a region and makes a beat from it hears the beat in
/// that region rather than at bar 1.
#[test]
fn make_beat_places_the_pattern_at_the_selection_not_bar_one() {
    use crate::sequencer::{BeatTime, ProjectFrame};
    let (mut session, _asset) = session_with_long_source(11_099, u64::from(RATE) * 4);
    let tempo = session
        .project_snapshot()
        .unwrap()
        .project
        .state()
        .domains
        .sequencer
        .tempo_map()
        .clone();
    // Two beats in: the selection starts on an exact beat boundary.
    let start = tempo.beat_to_frame(BeatTime(2 * PPQ)).0;
    let end = tempo.beat_to_frame(BeatTime(4 * PPQ)).0;
    let range = SampleRange::new(Sample::new(start), Sample::new(end));
    let beat = session
        .publish_primary_workbench_range(
            range,
            WorkbenchSampleIntent::MakeBeat {
                chop: SampleChopIntent::EqualSlices { count: 4 },
                kit: SampleKitDestination::NewKit,
                target_bus: None,
                bars: 1,
                quantize_ticks: PPQ as u64,
                result_focus: MakeBeatResultFocus::PatternEditor,
            },
        )
        .unwrap();
    let pattern = beat.constructive.publication.pattern.unwrap();
    let occurrence = beat_occurrence(&session, pattern);
    let snapshot = session.project_snapshot().unwrap();
    let clip = snapshot
        .project
        .state()
        .domains
        .arrangement
        .clip(occurrence.arrangement_clip)
        .unwrap();
    assert_eq!(
        clip.placement.start.get(),
        start,
        "beat occurrence must start where the selected material sounds"
    );
    assert_eq!(
        tempo.frame_to_beat_floor(ProjectFrame(clip.placement.start.get())),
        BeatTime(2 * PPQ)
    );
}

fn session_with_long_source(id: u64, frames: u64) -> (ProjectSession, AssetId) {
    let location = AssetLocation::new(
        Some(AbsolutePath::parse("/cycle11/long-source.wav").unwrap()),
        None,
    )
    .unwrap();
    let mut registry = AssetRegistry::new();
    let asset = registry
        .register(AssetRegistration {
            name: "cycle11 long source".into(),
            location: location.clone(),
            metadata: DecodedAudioMetadata {
                sample_rate_hz: RATE,
                channels: 1,
                frame_count: SampleFrames(frames),
                container: Some("wav".into()),
                codec: Some("pcm_f32le".into()),
                bit_depth: Some(32),
            },
            content: ContentFingerprint::from_bytes(b"cycle11:long:pcm"),
            provenance: AssetProvenance::new(
                11,
                AssetOrigin::Generated {
                    generator: "cycle11 flow corpus".into(),
                },
                location,
            ),
            tags: BTreeSet::from(["cycle11".into()]),
            favorite: false,
        })
        .unwrap();
    let samples = (0..frames)
        .map(|frame| if frame % 97 == 0 { 0.8 } else { 0.0 })
        .collect::<Vec<f32>>();
    let pcm = PcmAsset::new(AudioFormat::new(RATE, 1).unwrap(), Arc::from(samples)).unwrap();
    let live = LiveProject::from_source_material(
        SourceMaterialMetadata::new("Cycle 11", "Long source"),
        registry,
        asset,
        pcm,
    )
    .unwrap();
    let mut session = ProjectSession::new(ProjectSessionId(id)).unwrap();
    session.install(live, None).unwrap();
    (session, asset)
}
