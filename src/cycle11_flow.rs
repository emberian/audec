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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::arrangement::{ClipId, Frame, TrackKind};
use crate::arrangement_interaction::keyboard::plan_duplicate_after;
use crate::arrangement_interaction::{ArrangementEdit, ArrangementEditIntent, GestureCommit};
use crate::arrangement_view::{ArrangementAction, ArrangementActionIntent, ArrangementViewEvent};
use crate::assets::{
    AbsolutePath, AssetId, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
    AssetRegistry, ContentFingerprint, DecodedAudioMetadata, SampleFrames,
};
use crate::audio::AudioFormat;
use crate::automation::{BindingMode, MixerTarget, ParameterAddress, TimeDomain};
use crate::control_views::control_actions::{
    AutomationAction, AutomationActionIntent, ControlAction, ControlSessionAdapter,
    ControlSessionOperation,
};
use crate::daw_engine::{
    compile_daw_engine, BuiltInInstrumentDefinition, BuiltInInstrumentRoute, DawEngineConfig,
};
use crate::daw_project::DawProject;
use crate::daw_render::{PcmAsset, RenderCancellation, RenderWindow};
use crate::instruments::{SynthParams, Waveform};
use crate::live_project::{LiveProject, SourceMaterialMetadata};
use crate::pattern_actions::{
    CreatePatternIntent, PatternAction, PatternActionIntent, PatternEdit, PatternEditIntent,
    PatternEditorMode, PatternEditorTarget,
};
use crate::pattern_authoring::{DivergedOverwrite, ExpressionRealizationContext};
use crate::pattern_use_graph::{PatternOccurrenceTarget, PatternUseGraph, PatternUseSnapshot};
use crate::project_controller::{
    execute_arrangement_event_revealed, execute_envelope_revealed, execute_pattern_action_revealed,
    recommend_constructive, InstrumentRef, ObjectNavigator, ObjectRef, PatternAuditionRequest,
    PatternAuditionScope, PatternAuditionSessionAdapter, PatternAuditionSessionInputs,
    PatternAuditionStartRequest, PatternOccurrenceRef, PatternRevealExecution,
    RevealRecommendation, WorkbenchSampleIntent, WorkbenchSampleOutcome,
};
use crate::project_selection::{ObjectSelection, SelectionProvenance, SelectionSource};
use crate::project_session::{ProjectSession, ProjectSessionId};
use crate::render_runtime::{AuditionMix, AuditionOwner, AuditionSubject};
use crate::sample_actions::{
    MakeBeatResultFocus, SampleChopIntent, SampleKitDestination, SampleResultFocus,
};
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
