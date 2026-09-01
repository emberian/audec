//! Cycle 10 adversarial musician / reverse-production acceptance corpus.
//!
//! This module uses no UI entities or demo constructors.
//!
//! | Goal | Boundary exercised here | Status |
//! | --- | --- | --- |
//! | selected material → chop → pads → pattern → arrangement | `SampleAction` → `ProjectController` → DAW engine | covered |
//! | mixer / shared audition / revision-pinned export / reopen | control adapter → DAW engine → export + constructive codec | covered |
//! | pattern occurrence audition | pinned pattern render job → shared-engine adoption pin | covered without opening an audio device |
//! | evidence → candidate → atomic promotion → residual/excess | artifact promotion/comparison bridge | covered |
//! | reading export/import/query | reading/query workbench → project-session aggregate import | covered |
//! | plugin crash / recovery | out-of-process supervisor + actual process fixture | ignored: host fixture is integration-test-only, not a reusable crate service |
//! | loop cohort handoff | render service / renderer acknowledgement | covered by Cycle 9 corpus; this fixture deliberately keeps the one-renderer path |

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::air_query::workbench::protocol::{LocalSourceDto, ReadingInputDto};
use crate::air_query::workbench::{
    PortableEntityRecord, PortableEntityRole, PortableEntitySection, PortableHypothesisSemantics,
    QueryDocument, QueryDocumentId, QueryPageRequest, QueryTermDto, UnknownSectionPolicy,
};
use crate::arrangement::ArrangementOperation;
use crate::artifact_catalog::{
    ArtifactCatalog, ArtifactDescriptor, ArtifactId, ArtifactKind, ContentDigest, DigestAlgorithm,
};
use crate::artifact_promotion_bridge::plan_artifact_promotion_comparison;
use crate::aspect::FrameSpan;
use crate::assets::{
    AbsolutePath, AssetFrameRange, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
    AssetRegistry, ContentFingerprint, DecodedAudioMetadata, SampleFrames,
};
use crate::audio::AudioFormat;
use crate::command::{claims_for_commands, CommandEnvelope, DomainCommand};
use crate::comparison_controller::{ComparisonChannel, ComparisonController};
use crate::comparison_runtime::executor::ComparisonProductExecutor;
use crate::control_views::control_actions::{
    ControlAction, ControlEdit, ControlSessionAdapter, ControlSessionOperation, MixerAction,
    MixerActionIntent,
};
use crate::daw_engine::{compile_daw_engine, DawEngineConfig};
use crate::daw_project::{DawProject, ProjectDomain};
use crate::daw_render::{PcmAsset, RenderCancellation, RenderWindow};
use crate::deprojection_execution::promotion::CreatedObject;
use crate::deprojection_program::{EditableTermKind, EvidenceRef};
use crate::explanation::RenderedExplanation;
use crate::export::{
    export_revision_pinned_audio_to_wav, NoopExportObserver, RevisionPinnedAudio, WavExportRequest,
};
use crate::interpretation::InterpretationStore;
use crate::live_project::{LiveProject, ProjectController, SourceMaterialMetadata};
use crate::ontology;
use crate::ontology::{Producer, Provenance};
use crate::pattern_use_graph::{PatternUseGraph, PatternUseSnapshot};
use crate::project_audio_controller::{
    ProjectAudioController, ProjectAudioControllerEffect, ProjectAudioPlanStamp,
    ProjectAudioRenderRecipe,
};
use crate::project_codecs::{decode_constructive, encode_constructive};
use crate::project_controller::SampleActionOutcome;
use crate::project_controller::{
    PatternAuditionAdapter, PatternAuditionError, PatternAuditionRenderInputs,
    PatternAuditionRequest, PatternAuditionScope,
};
use crate::project_format::{PreservedProjectData, ProjectPackage};
use crate::project_io::ProjectFile;
use crate::project_repository::{JsonAirPayloadCodec, ProjectRepository};
use crate::project_session::reading_query::{
    ProjectQueryResolverInputs, ProjectReadingQuerySession,
};
use crate::project_session::{ProjectSession, ProjectSessionId};
use crate::project_store::ProjectStore;
use crate::reading::{
    PortableDigest, PortableDigestAlgorithm, ProducerDto, ProvenanceDto, ReadingFile, ReadingId,
    ReadingSection, ReadingSource, READING_FORMAT, READING_FORMAT_VERSION,
};
use crate::render_plan::{DeterminismGrade, ExactDigest, RenderSpan, Tileability};
use crate::sample_actions::{
    MakeBeatIntent, MakeBeatResultFocus, SampleAction, SampleChopIntent, SampleKitDestination,
    SampleSelection,
};
use crate::sample_material::SourceMaterialRef;

const RATE: u32 = 48_000;

/// The workbench's convenience token is private to its implementation.  Keep
/// this acceptance fixture on the public cancellation contract instead.
struct UncancelledQuery;

impl crate::air_query::QueryCancellation for UncancelledQuery {
    fn is_cancelled(&self) -> bool {
        false
    }
}

fn controller_with_distinct_source() -> (ProjectController, crate::assets::AssetId) {
    let location = AssetLocation::new(
        Some(AbsolutePath::parse("/cycle10/distinct-source.wav").unwrap()),
        None,
    )
    .unwrap();
    let mut registry = AssetRegistry::new();
    let asset = registry
        .register(AssetRegistration {
            name: "distinct cycle10 source".into(),
            location: location.clone(),
            metadata: DecodedAudioMetadata {
                sample_rate_hz: RATE,
                channels: 1,
                frame_count: SampleFrames(8),
                container: Some("wav".into()),
                codec: Some("pcm_f32le".into()),
                bit_depth: Some(32),
            },
            content: ContentFingerprint::from_bytes(b"cycle10:distinct:pcm"),
            provenance: AssetProvenance::new(
                10,
                AssetOrigin::Generated {
                    generator: "cycle10 acceptance corpus".into(),
                },
                location,
            ),
            tags: BTreeSet::from(["cycle10".into(), "adversarial".into()]),
            favorite: false,
        })
        .unwrap();
    // Deliberately asymmetric values make an accidental full-source reuse,
    // reversed range, or silent zero fill immediately observable.
    let pcm = PcmAsset::new(
        AudioFormat::new(RATE, 1).unwrap(),
        Arc::from([0.0, 0.91, -0.32, 0.17, 0.0, 0.63, -0.48, 0.0]),
    )
    .unwrap();
    let live = LiveProject::from_source_material(
        SourceMaterialMetadata::new("Cycle 10", "Distinct source"),
        registry,
        asset,
        pcm,
    )
    .unwrap();
    (ProjectController::new(live).unwrap(), asset)
}

fn execute_mixer_gain(controller: &mut ProjectController, gain_db: f32) {
    let snapshot = controller.snapshot();
    let master = snapshot.project.state().domains.mixer.master();
    let adapter = ControlSessionAdapter::new(
        controller.revisions().aggregate,
        1_010,
        &snapshot.project.state().domains.mixer,
        &snapshot.project.state().domains.automation,
    );
    let action = ControlAction::Mixer(
        MixerActionIntent::new(
            snapshot.project.state().domains.mixer.revision(),
            MixerAction::SetGainDb {
                bus: master,
                gain_db,
            },
        )
        .with_edit(ControlEdit::Numeric),
    );
    let ControlSessionOperation::Execute(envelope) = adapter.adapt(&action).unwrap() else {
        panic!("a mixer value edit must lower to the aggregate command language")
    };
    controller.execute(envelope).unwrap();
}

#[test]
fn selected_chop_to_pads_pattern_arrangement_render_export_and_reopen_stays_one_truth() {
    let (mut controller, source) = controller_with_distinct_source();
    let selection = SampleSelection {
        asset: source,
        source_range: Some(
            AssetFrameRange::new(SampleFrames(1), SampleFrames(7))
                .expect("fixture selection is a non-empty half-open range"),
        ),
    };
    let outcome = controller
        .execute_sample_action(SampleAction::MakeBeat(MakeBeatIntent {
            source: selection,
            chop: SampleChopIntent::EqualSlices { count: 2 },
            kit: SampleKitDestination::NewKit,
            target_bus: None,
            bars: 1,
            quantize_ticks: crate::sequencer::PPQ as u64,
            result_focus: MakeBeatResultFocus::PatternEditor,
        }))
        .unwrap();
    let SampleActionOutcome::Published(beat) = outcome else {
        panic!("selected material must publish one constructive project edit")
    };
    let publication = &beat.publication;
    assert!(publication.pattern.is_some());
    assert!(publication.arrangement_clip.is_some());
    assert_eq!(publication.created_zones.len(), 2);

    let snapshot = controller.snapshot().clone();
    let kit = &snapshot.project.state().domains.sample_kits.kits[&publication.kit];
    let ranges = publication
        .created_zones
        .iter()
        .map(|target| match &kit.zones[&target.zone].material {
            SourceMaterialRef::VirtualSlice(slice) => slice.source_range,
            other => panic!("selection chop must retain slices, got {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ranges,
        vec![
            AssetFrameRange::new(SampleFrames(1), SampleFrames(4)).unwrap(),
            AssetFrameRange::new(SampleFrames(4), SampleFrames(7)).unwrap(),
        ]
    );
    assert_eq!(
        snapshot.sample_pcm.len(),
        2,
        "pad PCM is part of the same publication"
    );

    // The mixer is edited through the same aggregate controller immediately
    // after construction; rendering must observe this durable graph, not a
    // demo mixer or retained source-only host.
    execute_mixer_gain(&mut controller, -3.0);
    let snapshot = controller.snapshot().clone();
    let cancellation = RenderCancellation::new();
    let schedule = compile_daw_engine(
        &snapshot.project,
        &snapshot.pcm,
        RenderWindow::new(0, 24_032).unwrap(),
        &DawEngineConfig::default(),
        &cancellation,
    )
    .unwrap();
    let rendered = schedule.render_for_audition(&cancellation).unwrap();
    assert!(
        rendered
            .audio
            .interleaved()
            .iter()
            .any(|sample| sample.abs() > 0.05),
        "the shared audition render must contain the authored, non-silent material"
    );

    let project = snapshot.project.as_ref();
    let file = ProjectFile::from_project(project, None);
    let payloads = encode_constructive(project).unwrap();
    let reopened = decode_constructive(&file, &payloads, project.state().domains.air.clone())
        .expect("save/reopen must retain the editable constructive graph");
    assert_eq!(reopened.aggregate_revision, project.revisions().aggregate);
    assert_eq!(
        reopened.state.domains.sample_kits.kits.len(),
        project.state().domains.sample_kits.kits.len()
    );
    assert_eq!(
        reopened.state.domains.sequencer.patterns().patterns().len(),
        project
            .state()
            .domains
            .sequencer
            .patterns()
            .patterns()
            .len()
    );
    let arrangement_clip = publication.arrangement_clip.unwrap();
    assert_eq!(
        reopened
            .state
            .domains
            .arrangement
            .clip(arrangement_clip)
            .unwrap()
            .placement,
        project
            .state()
            .domains
            .arrangement
            .clip(arrangement_clip)
            .unwrap()
            .placement
    );

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let destination = std::env::temp_dir().join(format!("audec-cycle10-{nonce}.wav"));
    let request = WavExportRequest::new(&destination);
    let report = export_revision_pinned_audio_to_wav(
        RevisionPinnedAudio::new(project.revisions().aggregate, rendered.into_project_audio()),
        &request,
        &mut NoopExportObserver,
    )
    .unwrap();
    assert_eq!(report.aggregate_revision, project.revisions().aggregate);
    assert!(
        report.wav.bytes_written > 44,
        "export must contain PCM beyond a WAV header"
    );
    assert_eq!(
        fs::metadata(&destination).unwrap().len(),
        report.wav.bytes_written
    );
    fs::remove_file(destination).unwrap();
}

#[test]
fn pattern_occurrence_audition_uses_the_shared_engine_pin_and_cancels_stale_work() {
    let (mut controller, source) = controller_with_distinct_source();
    let beat = controller
        .execute_sample_action(SampleAction::MakeBeat(MakeBeatIntent {
            source: SampleSelection {
                asset: source,
                source_range: Some(AssetFrameRange::new(SampleFrames(1), SampleFrames(7)).unwrap()),
            },
            chop: SampleChopIntent::EqualSlices { count: 2 },
            kit: SampleKitDestination::NewKit,
            target_bus: None,
            bars: 1,
            quantize_ticks: crate::sequencer::PPQ as u64,
            result_focus: MakeBeatResultFocus::PatternEditor,
        }))
        .unwrap();
    let SampleActionOutcome::Published(beat) = beat else {
        panic!("make beat must publish a pattern occurrence")
    };
    let snapshot = controller.snapshot().clone();
    let pattern = beat
        .publication
        .pattern
        .expect("make beat must create a pattern");
    let occurrence = PatternUseGraph::build(PatternUseSnapshot::from_project(&snapshot.project))
        .unwrap()
        .pattern(pattern)
        .unwrap()
        .occurrences[0]
        .target;
    let request = PatternAuditionRequest {
        expected_project_revision: snapshot.revisions().aggregate,
        occurrence,
        cycle_index: 0,
        performance_seed: 0xC10,
        scope: PatternAuditionScope::Pattern,
    };
    let inputs = || {
        PatternAuditionRenderInputs::new(
            Arc::clone(&snapshot.pcm),
            Arc::new(DawEngineConfig::default()),
        )
    };
    let mut adapter = PatternAuditionAdapter::default();
    let superseded = adapter
        .prepare(&snapshot.project, &request, inputs())
        .unwrap();
    let current = adapter
        .prepare(&snapshot.project, &request, inputs())
        .unwrap();
    assert!(matches!(
        superseded.execute(),
        Err(PatternAuditionError::Cancelled)
    ));

    let completion = current.execute().unwrap();
    assert_eq!(completion.pin.occurrence, occurrence);
    assert_eq!(completion.pin.revisions, snapshot.revisions());
    assert!(completion
        .render
        .audio
        .interleaved()
        .iter()
        .any(|sample| sample.abs() > 0.05));
    let adoption = completion.project_audio_pin().unwrap();
    assert_eq!(adoption.revision, snapshot.revisions().aggregate);
    assert_eq!(adoption.span.start, completion.render.origin_frame);

    execute_mixer_gain(&mut controller, -1.0);
    assert!(matches!(
        adapter.finish(completion, controller.revisions().aggregate),
        Err(PatternAuditionError::StaleRevision { .. })
    ));
}

fn promotion_digest(byte: u8) -> ContentDigest {
    ContentDigest::new(DigestAlgorithm::Sha256, [byte; 32])
}

fn promotion_provenance() -> Provenance {
    Provenance {
        producer: Producer::Analyzer {
            name: "cycle10 acceptance".into(),
            version: "1".into(),
            configuration_digest: None,
        },
        created_unix_ms: None,
        source_revision: None,
        note: None,
    }
}

fn promotion_session() -> (ProjectSession, crate::assets::AssetId, Vec<f32>) {
    let location = AssetLocation::new(
        Some(AbsolutePath::parse("/cycle10/artifact-promotion.wav").unwrap()),
        None,
    )
    .unwrap();
    let mut registry = AssetRegistry::new();
    let asset = registry
        .register(AssetRegistration {
            name: "cycle10 artifact source".into(),
            location: location.clone(),
            metadata: DecodedAudioMetadata {
                sample_rate_hz: 8_000,
                channels: 1,
                frame_count: SampleFrames(8),
                container: Some("wav".into()),
                codec: Some("pcm_f32le".into()),
                bit_depth: Some(32),
            },
            content: ContentFingerprint::from_bytes(b"cycle10:artifact:pcm"),
            provenance: AssetProvenance::new(
                10,
                AssetOrigin::Generated {
                    generator: "cycle10 acceptance".into(),
                },
                location,
            ),
            tags: BTreeSet::from(["cycle10".into()]),
            favorite: false,
        })
        .unwrap();
    let samples = vec![0.125, -0.75, 0.375, 0.9, -0.2, 0.55, -0.95, 0.3];
    let live = LiveProject::from_source_material(
        SourceMaterialMetadata::new("Cycle 10", "Artifact promotion"),
        registry,
        asset,
        PcmAsset::new(
            AudioFormat::new(8_000, 1).unwrap(),
            Arc::from(samples.clone()),
        )
        .unwrap(),
    )
    .unwrap();
    let source_clip = live.primary_source_ids().unwrap().clip;
    let mut session = ProjectSession::new(ProjectSessionId(10_012)).unwrap();
    session.install(live, None).unwrap();
    let before = session
        .project_snapshot()
        .unwrap()
        .project
        .state()
        .domains
        .arrangement
        .clip(source_clip)
        .unwrap()
        .clone();
    let mut after = before.clone();
    after.muted = true;
    let commands = vec![DomainCommand::Arrangement(ArrangementOperation::PutClip {
        before: Some(before),
        after: Some(after),
    })];
    session
        .execute_envelope(CommandEnvelope {
            label: "Mute source before promotion comparison".into(),
            base_revision: session.project_snapshot().unwrap().revisions().aggregate,
            coalesce: None,
            id_claims: claims_for_commands(&commands),
            commands,
        })
        .unwrap();
    (session, asset, samples)
}

#[test]
fn evidence_candidate_atomic_promotion_updates_editable_render_and_residual() {
    let (mut session, _asset, samples) = promotion_session();
    let descriptor = ArtifactDescriptor {
        id: ArtifactId(promotion_digest(0x44)),
        kind: ArtifactKind::ModelClaim,
        source_digest: promotion_digest(0x11),
        recipe_digest: promotion_digest(0x22),
        output_digest: promotion_digest(0x44),
        extent: FrameSpan::new(0, 8).unwrap(),
        sample_rate: 8_000,
        channels: 1,
        provenance: promotion_provenance(),
    };
    let cancellation = RenderCancellation::new();
    let analysis = crate::rhythm::analyze_mono(
        &samples,
        descriptor.sample_rate,
        &crate::rhythm::RhythmConfig::default(),
    );
    let summaries = session
        .publish_deprojection_analysis(
            crate::project_session::deprojection_workspace_bridge::LiveDeprojectionAnalysis::from_rhythm(
                descriptor.clone(),
                analysis,
                crate::rhythm_explanation::ExplainBudget::default(),
                RenderedExplanation {
                    origin_frame: 0,
                    audio: crate::audio::ProjectAudio::from_interleaved(
                        AudioFormat::new(8_000, 1).unwrap(),
                        samples.clone(),
                    )
                    .unwrap(),
                },
            ),
            &cancellation,
        )
        .unwrap();
    let (resolved, term) = summaries
        .iter()
        .find_map(|summary| {
            let resolved = session
                .resolve_deprojection_workspace_request(
                    crate::project_session::deprojection_workspace_bridge::DeprojectionWorkspaceTarget::Object(
                        crate::project_controller::ObjectRef::Comparison(summary.comparison),
                    ),
                )
                .ok()?;
            resolved
                .request
                .candidate
                .program
                .roots
                .iter()
                .find(|root| {
                    matches!(
                        resolved.request.candidate.program.terms[root].kind,
                        EditableTermKind::ExactAudioReference { .. }
                    )
                })
                .copied()
                .map(|term| (resolved, term))
        })
        .expect("literal workspace candidate");
    let result = plan_artifact_promotion_comparison(
        &session,
        session.deprojection_workspace_artifacts(),
        resolved.request,
        &cancellation,
    )
    .unwrap()
    .execute(&mut session, &cancellation)
    .unwrap();
    assert!(result.promotion.provenance[&term]
        .evidence
        .contains(&EvidenceRef::Artifact(descriptor.id)));
    let promoted_clip = result
        .promotion
        .created
        .iter()
        .find_map(|object| match object {
            CreatedObject::ExactAudioFallbackClip(clip) => Some(*clip),
            _ => None,
        })
        .expect("exact candidate must create an editable fallback clip");

    let mut audio = ProjectAudioController::new();
    audio.set_tile_policy(None);
    let render = result
        .request_shared_render(
            &session,
            &mut audio,
            ProjectAudioRenderRecipe {
                extent: RenderSpan::new(0, 8).unwrap(),
                engine: Arc::new(DawEngineConfig {
                    output_channels: 1,
                    block_frames: 4,
                    ..DawEngineConfig::default()
                }),
                stamp: ProjectAudioPlanStamp {
                    project_namespace: 10_012,
                    snapshot: ExactDigest::new([0x61; 32]),
                    engine_abi: 1,
                    engine_configuration: ExactDigest::new([0x62; 32]),
                    dependencies: Vec::new(),
                    determinism: DeterminismGrade::BitExact,
                    tileability: Tileability::Stateless,
                },
            },
            &cancellation,
        )
        .unwrap();
    assert!(matches!(
        audio
            .complete_render(render.execute(&cancellation).unwrap())
            .unwrap(),
        ProjectAudioControllerEffect::OpenHost(_)
    ));
    let mut comparison_controller = ComparisonController::new(10_015).unwrap();
    let mut comparison_executor = ComparisonProductExecutor::new();
    let comparison = result
        .capture_updated_comparison(
            &session,
            &audio,
            &mut comparison_controller,
            &mut comparison_executor,
            ComparisonChannel::Residual,
            &cancellation,
        )
        .unwrap()
        .job
        .execute()
        .unwrap();
    assert_eq!(comparison.execution.rendered.source.interleaved(), samples);
    assert_eq!(
        comparison.execution.rendered.construction.interleaved(),
        samples
    );
    assert!(comparison
        .execution
        .rendered
        .residual
        .interleaved()
        .iter()
        .all(|sample| sample.abs() <= f32::EPSILON));

    result.undo(&mut session).unwrap();
    assert!(session
        .project_snapshot()
        .unwrap()
        .project
        .state()
        .domains
        .arrangement
        .clip(promoted_clip)
        .is_none());
}

fn query_session_with_source() -> ProjectSession {
    let mut project = DawProject::new("Cycle 10 reading", RATE, 120.0).unwrap();
    project
        .transact(
            "Cycle 10 AIR source",
            0,
            BTreeSet::from([ProjectDomain::Air]),
            |state| {
                state
                    .domains
                    .air
                    .insert_source(ontology::AudioSource {
                        id: ontology::SourceId::new(7),
                        uri: "asset:cycle10".into(),
                        content_digest: Some("sha256:cycle10".into()),
                        sample_rate: RATE,
                        channels: 1,
                        frame_count: 8,
                    })
                    .map_err(|error| error.to_string())
            },
        )
        .unwrap();
    let live = LiveProject::from_project(project, BTreeMap::new()).unwrap();
    let mut session = ProjectSession::new(ProjectSessionId(10_010)).unwrap();
    session.install(live, None).unwrap();
    session
}

fn reading_digest(byte: u8) -> PortableDigest {
    PortableDigest {
        algorithm: PortableDigestAlgorithm::Sha256,
        bytes: [byte; 32],
    }
}

fn portable_reading(id: u8, group: &str) -> ReadingInputDto {
    let reading = ReadingFile {
        format: READING_FORMAT.into(),
        version: READING_FORMAT_VERSION,
        reading_id: ReadingId::new([id; 16]).unwrap(),
        revision: 1,
        parents: Vec::new(),
        author: ProvenanceDto {
            producer: ProducerDto::Human { name: None },
            created_unix_ms: None,
            source_revision: None,
            note: None,
        },
        source: ReadingSource {
            fingerprints: vec![reading_digest(0xC1)],
            sample_rate: RATE,
            channels: 1,
            frame_count: 8,
            declared_title: Some("Cycle 10 source".into()),
            extensions: BTreeMap::new(),
        },
        sections: vec![ReadingSection {
            name: crate::air_query::workbench::ENTITY_SECTION_NAME.into(),
            schema_major: crate::air_query::workbench::ENTITY_SECTION_MAJOR,
            schema_minor: 0,
            payload: serde_json::to_value(PortableEntitySection {
                entities: vec![PortableEntityRecord {
                    kind: "hypothesis".into(),
                    local_id: 1,
                    label: format!("Cycle 10 alternative {id}"),
                    role: PortableEntityRole::Hypothesis,
                    hypothesis: Some(PortableHypothesisSemantics {
                        support: 0.5,
                        description: Some(format!("portable reading {id}")),
                    }),
                    hypothesis_group: Some(group.into()),
                    extent: None,
                    extensions: BTreeMap::new(),
                }],
                extensions: BTreeMap::new(),
            })
            .unwrap(),
            extensions: BTreeMap::new(),
        }],
        attachments: Vec::new(),
        extensions: BTreeMap::new(),
    };
    ReadingInputDto {
        reading,
        local_source: Some(LocalSourceDto {
            digest: reading_digest(0xC1),
            sample_rate: RATE,
            channels: 1,
            frame_count: 8,
        }),
    }
}

fn reading_bridge(session: &ProjectSession) -> ProjectReadingQuerySession {
    ProjectReadingQuerySession::new(
        session,
        &ArtifactCatalog::new(),
        &InterpretationStore::new(),
        ProjectQueryResolverInputs::default(),
        Arc::new(|_| {}),
    )
    .unwrap()
}

#[test]
fn reading_export_import_and_query_preserve_qualified_provenance() {
    let mut session = query_session_with_source();
    let bridge = reading_bridge(&session);
    let mut query = QueryDocument::new(
        QueryDocumentId(10),
        "Cycle 10 sources",
        QueryTermDto::Kind {
            kind: crate::air_query::workbench::FactKindDto::Source,
        },
    );
    bridge
        .execute_page(&mut query, QueryPageRequest::default(), &UncancelledQuery)
        .unwrap();
    let result = query.latest_result().unwrap();
    assert!(result.provenance.fact_base_digest.is_strong());
    assert_eq!(result.page.hits.len(), 1);
    assert!(matches!(
        result.page.hits[0].fact,
        crate::interpretation_navigation::EntityRefDto::Project {
            ref kind,
            local_id: 7
        } if kind == "air-source"
    ));

    let exported = portable_reading(1, "cycle10 identity");
    let encoded = serde_json::to_vec(&exported.reading).unwrap();
    let decoded: ReadingFile = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded, exported.reading);
    let second = portable_reading(2, "cycle10 identity");
    let plan = bridge
        .plan_import(&[exported, second], UnknownSectionPolicy::PreserveOpaque)
        .unwrap();
    assert_eq!(plan.lowered.envelope.commands.len(), 3);
    let receipt = bridge.apply_import(&mut session, plan).unwrap();
    assert_eq!(receipt.mappings.len(), 2);
    assert_ne!(receipt.mappings[0].foreign, receipt.mappings[1].foreign);
    assert_eq!(
        session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .air
            .hypotheses
            .len(),
        2
    );

    let frozen = session.project_snapshot().unwrap().clone();
    let reopened_live =
        LiveProject::from_project((*frozen.project).clone(), (*frozen.pcm).clone()).unwrap();
    let mut reopened = ProjectSession::new(ProjectSessionId(10_011)).unwrap();
    reopened.install(reopened_live, None).unwrap();
    assert_eq!(
        reading_bridge(&reopened)
            .snapshot()
            .existing_foreign_entities()
            .len(),
        2
    );

    bridge.undo_import(&mut session, &receipt).unwrap();
    assert!(session
        .project_snapshot()
        .unwrap()
        .project
        .state()
        .domains
        .air
        .hypotheses
        .is_empty());
}

#[test]
fn production_air_codec_save_reopen_preserves_project_query_provenance() {
    let session = query_session_with_source();
    let snapshot = session.project_snapshot().unwrap();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let package_path = std::env::temp_dir().join(format!(
        "audec-cycle10-air-codec-{}-{nonce}.audec",
        std::process::id()
    ));
    fs::create_dir_all(&package_path).unwrap();

    let repository = ProjectRepository::new(
        ProjectStore::new(ProjectPackage::new(&package_path).unwrap()),
        JsonAirPayloadCodec,
    );
    repository
        .save_primary(&snapshot.project, PreservedProjectData::default())
        .unwrap();

    let reopened = ProjectRepository::new(
        ProjectStore::new(ProjectPackage::new(&package_path).unwrap()),
        JsonAirPayloadCodec,
    )
    .open_primary()
    .unwrap();
    assert_eq!(
        reopened.project.state().domains.air,
        snapshot.project.state().domains.air,
        "a fresh production codec must preserve the AIR graph used for project queries"
    );
    assert_eq!(reopened.project.revisions(), snapshot.project.revisions());
    fs::remove_dir_all(package_path).unwrap();
}

#[test]
#[ignore = "the hostile plugin worker process fixture is integration-test-only and not exposed as a crate-level supervisor service"]
fn plugin_crash_recovery_keeps_the_project_renderer_alive() {
    panic!("enable when the process-fixture supervisor is reusable by the main acceptance binary")
}
