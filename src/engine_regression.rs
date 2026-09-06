//! Cross-domain audible regression coverage for [`crate::daw_engine`].
//!
//! These tests deliberately build a complete [`crate::daw_project::DawProject`]
//! rather than testing the individual editor, mixer, sequencer, or instrument
//! modules in isolation.  A reverse DAW is only useful if an edit remains
//! audible after identities cross all those boundaries.

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;

    use crate::arrangement::{
        ArrangementEditor, Frame, FrameRange, SourceRange, StretchAlgorithm, TrackId, TrackKind,
    };
    use crate::assets::{
        AbsolutePath, AssetFrameRange, AssetLocation, AssetOrigin, AssetProvenance,
        AssetRegistration, ContentFingerprint, DecodedAudioMetadata, ProjectRelativePath,
        SampleFrames,
    };
    use crate::audio::AudioFormat;
    use crate::automation::{
        AutomationCommand, ClipParameter, LaneChange, ParameterAddress, ParameterDescriptor,
        ParameterUnit, ProjectFrame, SegmentShape, SmoothingPolicy, TimeDomain, TimePosition,
        ValueMapping,
    };
    use crate::compiled_audio_graph::{
        compile_native_daw_graph, GraphDiagnostic, RealtimeGraphExecutor,
    };
    use crate::constructive::{
        ConstructiveCause, ConstructiveEditPlan, ConstructiveFocus, KitMutation,
        MaterialReusePolicy, PatternPlacementIntent, PatternSeed, PlannedMaterial, PlannedPattern,
        PlannedPatternId, PlannedStep,
    };
    use crate::daw_engine::{
        compile_daw_engine, AssetPcmMap, BuiltInInstrumentDefinition, BuiltInInstrumentRoute,
        DawEngineConfig, EngineDiagnostic,
    };
    use crate::daw_project::{DawProject, ProjectDomain};
    use crate::daw_render::{PcmAsset, RenderCancellation, RenderDiagnostic, RenderWindow};
    use crate::instruments::{SampleData, SamplerParams, SynthParams};
    use crate::mixer::{BusKind, PluginDescriptor};
    use crate::pattern_lang::PatternOrigin;
    use crate::render_plan::{BusTap, RenderScope, RenderSpan};
    use crate::sample_kit::{SampleKit, SamplePad, SampleRouteIntent, SampleZone};
    use crate::sample_material::{extract_virtual_slice, SourceMaterialRef, VirtualSliceRef};
    use crate::sequencer::{
        BeatDuration, BeatTime, PatternClip, PatternContent, PatternDefinition, SequencerCommand,
        StepEvent, StepLane, StepPattern, TriggerTarget, PPQ,
    };

    const RATE: u32 = 1_000;

    /// The steady-source fixture: 64 frames at half amplitude, faded from
    /// 0 dB to -60 dB of clip gain across exactly that span.
    const FADE_FRAMES: i64 = 64;
    const AMPLITUDE: f32 = 0.5;
    const FADE_END_DB: f64 = -60.0;

    fn location() -> AssetLocation {
        AssetLocation::new(
            Some(AbsolutePath::parse("/fixture/hit.wav").unwrap()),
            Some(ProjectRelativePath::parse("media/hit.wav").unwrap()),
        )
        .unwrap()
    }

    fn registration(frames: u64) -> AssetRegistration {
        AssetRegistration {
            name: "fixture hit".into(),
            location: location(),
            metadata: DecodedAudioMetadata {
                sample_rate_hz: RATE,
                channels: 1,
                frame_count: SampleFrames(frames),
                container: Some("wav".into()),
                codec: Some("pcm_f32le".into()),
                bit_depth: Some(32),
            },
            content: ContentFingerprint::from_bytes(b"engine-regression-hit"),
            provenance: AssetProvenance::new(
                1,
                AssetOrigin::Generated {
                    generator: "engine-regression".into(),
                },
                location(),
            ),
            tags: BTreeSet::new(),
            favorite: false,
        }
    }

    /// A real bound source, placed from frame 4 through 12 on its own routed
    /// track.  The returned PCM has deliberately distinctive frame values so
    /// trim/slip mistakes cannot hide behind a constant sample.
    fn audio_project() -> (
        DawProject,
        crate::assets::AssetId,
        TrackId,
        crate::arrangement::ClipId,
        AssetPcmMap,
    ) {
        let mut project = DawProject::new("engine regression", RATE, 60.0).unwrap();
        let mut ids = None;
        project
            .transact(
                "install source",
                0,
                BTreeSet::from([
                    ProjectDomain::Arrangement,
                    ProjectDomain::Assets,
                    ProjectDomain::Mixer,
                    ProjectDomain::Bindings,
                ]),
                |state| -> Result<(), String> {
                    let media = state
                        .domains
                        .assets
                        .register(registration(8))
                        .map_err(|error| error.to_string())?;
                    let alias = state
                        .bindings
                        .bind_media_asset(media)
                        .map_err(|error| error.to_string())?;
                    let mut arrangement =
                        ArrangementEditor::from_state(state.domains.arrangement.clone())
                            .map_err(|error| error.to_string())?;
                    let track = arrangement
                        .create_track("source", TrackKind::Audio)
                        .map_err(|error| error.to_string())?;
                    let clip = arrangement
                        .create_audio_clip(
                            track,
                            "source",
                            FrameRange::new(Frame(4), Frame(12)).unwrap(),
                            alias,
                            SourceRange::new(0, 8).unwrap(),
                        )
                        .map_err(|error| error.to_string())?;
                    state.domains.arrangement = arrangement.state().clone();
                    let bus = state
                        .domains
                        .mixer
                        .add_bus(BusKind::Source, "source")
                        .map_err(|error| error.to_string())?;
                    state.bindings.mixer.tracks.insert(track, bus);
                    ids = Some((media, track, clip));
                    Ok(())
                },
            )
            .unwrap();
        let (media, track, clip) = ids.unwrap();
        let pcm = AssetPcmMap::from([(
            media,
            PcmAsset::new(
                AudioFormat::new(RATE, 1).unwrap(),
                Arc::from([0.10, 0.20, 0.30, 0.40, 0.50, 0.60, 0.70, 0.80]),
            )
            .unwrap(),
        )]);
        (project, media, track, clip, pcm)
    }

    fn render(
        project: &DawProject,
        pcm: &AssetPcmMap,
        start: i64,
        end: i64,
        config: &DawEngineConfig,
    ) -> crate::daw_engine::DawEngineRender {
        let cancellation = RenderCancellation::new();
        let schedule = compile_daw_engine(
            project,
            pcm,
            RenderWindow::new(start, end).unwrap(),
            config,
            &cancellation,
        )
        .unwrap();
        schedule.render_for_audition(&cancellation).unwrap()
    }

    fn assert_silent(samples: &[f32]) {
        assert!(samples.iter().all(|sample| *sample == 0.0), "{samples:?}");
    }

    #[test]
    fn clip_move_trim_duplicate_delete_preserve_the_exact_source_frames() {
        let (mut project, asset, track, clip, pcm) = audio_project();
        let revision = project.revisions().aggregate;
        project
            .transact(
                "perform ordinary timeline edits",
                revision,
                BTreeSet::from([ProjectDomain::Arrangement]),
                |state| -> Result<(), String> {
                    let mut arrangement =
                        ArrangementEditor::from_state(state.domains.arrangement.clone())
                            .map_err(|error| error.to_string())?;
                    arrangement
                        .move_clip(clip, track, Frame(10))
                        .map_err(|e| e.to_string())?;
                    arrangement
                        .trim_left(clip, Frame(12))
                        .map_err(|e| e.to_string())?;
                    arrangement
                        .trim_right(clip, Frame(16))
                        .map_err(|e| e.to_string())?;
                    let duplicate = arrangement
                        .duplicate_clip(clip, Frame(20))
                        .map_err(|e| e.to_string())?;
                    arrangement.delete_clip(clip).map_err(|e| e.to_string())?;
                    // The duplicate, not a copied media buffer, is the only
                    // remaining reference. Its range must still be 2..6.
                    let remaining = arrangement.state().clip(duplicate).unwrap();
                    assert_eq!(
                        remaining.placement,
                        FrameRange::new(Frame(20), Frame(24)).unwrap()
                    );
                    state.domains.arrangement = arrangement.state().clone();
                    Ok(())
                },
            )
            .unwrap();

        let first = render(&project, &pcm, 0, 28, &DawEngineConfig::default());
        let second = render(&project, &pcm, 0, 28, &DawEngineConfig::default());
        assert_eq!(first.audio.interleaved(), second.audio.interleaved());
        for frame in 0..28 {
            let expected = match frame {
                20 => 0.30,
                21 => 0.40,
                22 => 0.50,
                23 => 0.60,
                _ => 0.0,
            };
            let stereo = &first.audio.interleaved()[frame * 2..frame * 2 + 2];
            assert_eq!(stereo, &[expected, expected], "frame {frame}");
        }

        // A rendered schedule owns the resolved source snapshot. Replacing a
        // decoder cache entry later cannot corrupt a prior audition/export.
        let cancellation = RenderCancellation::new();
        let schedule = compile_daw_engine(
            &project,
            &pcm,
            RenderWindow::new(20, 24).unwrap(),
            &DawEngineConfig::default(),
            &cancellation,
        )
        .unwrap();
        let mut replacement = pcm;
        replacement.insert(
            asset,
            PcmAsset::new(AudioFormat::new(RATE, 1).unwrap(), Arc::from([9.0; 8])).unwrap(),
        );
        let frozen = schedule.render_for_audition(&cancellation).unwrap();
        assert_eq!(
            frozen.audio.interleaved(),
            &[0.30, 0.30, 0.40, 0.40, 0.50, 0.50, 0.60, 0.60]
        );
    }

    #[test]
    fn mixer_mute_gain_and_pan_survive_the_aggregate_render_boundary() {
        let (mut project, _asset, track, _clip, pcm) = audio_project();
        let source = project.state().bindings.mixer.tracks[&track];
        let revision = project.revisions().aggregate;
        project
            .transact(
                "make source right and half as loud",
                revision,
                BTreeSet::from([ProjectDomain::Mixer]),
                |state| -> Result<(), String> {
                    state
                        .domains
                        .mixer
                        .set_gain_db(source, -6.020_600_3)
                        .map_err(|e| e.to_string())?;
                    state
                        .domains
                        .mixer
                        .set_pan(source, 1.0)
                        .map_err(|e| e.to_string())?;
                    Ok(())
                },
            )
            .unwrap();
        let panned = render(&project, &pcm, 4, 5, &DawEngineConfig::default());
        assert!(panned.audio.interleaved()[0].abs() < 1e-6);
        // The mixer uses an equal-power pan law, so a hard-panned dual-mono
        // source retains sqrt(2) times either centered channel after gain.
        let expected_right = 0.05 * 2.0_f32.sqrt();
        assert!((panned.audio.interleaved()[1] - expected_right).abs() < 1e-6);

        let revision = project.revisions().aggregate;
        project
            .transact(
                "mute source",
                revision,
                BTreeSet::from([ProjectDomain::Mixer]),
                |state| -> Result<(), String> {
                    state
                        .domains
                        .mixer
                        .set_muted(source, true)
                        .map_err(|e| e.to_string())
                },
            )
            .unwrap();
        let muted = render(&project, &pcm, 4, 5, &DawEngineConfig::default());
        assert_silent(muted.audio.interleaved());
    }

    #[test]
    fn explicitly_addressed_synth_and_sampler_routes_render_without_guessing() {
        let mut project = DawProject::new("instrument identities", RATE, 60.0).unwrap();
        let mut ids = None;
        project
            .transact(
                "install two explicitly addressed triggers",
                0,
                BTreeSet::from([
                    ProjectDomain::Arrangement,
                    ProjectDomain::Assets,
                    ProjectDomain::Mixer,
                    ProjectDomain::Sequencer,
                    ProjectDomain::Bindings,
                ]),
                |state| -> Result<(), String> {
                    let media = state
                        .domains
                        .assets
                        .register(registration(8))
                        .map_err(|e| e.to_string())?;
                    let sample_alias = state
                        .bindings
                        .bind_sequencer_sample(media)
                        .map_err(|e| e.to_string())?;
                    let mut sequencer = state.domains.sequencer.clone();
                    let pattern_id = sequencer.allocate_pattern_id();
                    let sequence_clip = sequencer.allocate_clip_id();
                    let synth_lane = sequencer.allocate_step_lane_id();
                    let sampler_lane = sequencer.allocate_step_lane_id();
                    let hit = StepEvent {
                        velocity: 1.0,
                        probability: 1.0,
                        micro_offset: 0,
                        gate: BeatDuration(240),
                        ratchets: 1,
                        pitch_semitones: 0.0,
                        pan: 0.0,
                    };
                    let pattern = PatternDefinition {
                        id: pattern_id,
                        name: "identities".into(),
                        origin: PatternOrigin::Authored,
                        length: BeatDuration(PPQ as u64),
                        content: PatternContent::Steps(StepPattern {
                            resolution: BeatDuration(PPQ as u64),
                            swing: 0.0,
                            lanes: BTreeMap::from([
                                (
                                    synth_lane,
                                    StepLane {
                                        id: synth_lane,
                                        name: "synth".into(),
                                        target: TriggerTarget::InstrumentNote {
                                            instrument: 7,
                                            key: 60,
                                        },
                                        choke_group: None,
                                        steps: BTreeMap::from([(0, hit.clone())]),
                                    },
                                ),
                                (
                                    sampler_lane,
                                    StepLane {
                                        id: sampler_lane,
                                        name: "sample".into(),
                                        target: TriggerTarget::Sample(sample_alias),
                                        choke_group: None,
                                        steps: BTreeMap::from([(0, hit)]),
                                    },
                                ),
                            ]),
                        }),
                        revision: 0,
                    };
                    let placed = PatternClip {
                        id: sequence_clip,
                        pattern: pattern_id,
                        start: BeatTime::ZERO,
                        length: BeatDuration(PPQ as u64),
                        pattern_offset: BeatTime::ZERO,
                        looped: false,
                        transpose_semitones: 0.0,
                        gain: 1.0,
                        muted: false,
                    };
                    sequencer
                        .execute(
                            "two identities",
                            vec![
                                SequencerCommand::PutPattern {
                                    before: None,
                                    after: Some(pattern),
                                },
                                SequencerCommand::PutClip {
                                    before: None,
                                    after: Some(placed),
                                },
                            ],
                        )
                        .map_err(|e| e.to_string())?;
                    state.domains.sequencer = sequencer;

                    let mut arrangement =
                        ArrangementEditor::from_state(state.domains.arrangement.clone())
                            .map_err(|e| e.to_string())?;
                    let track = arrangement
                        .create_track("instruments", TrackKind::Pattern)
                        .map_err(|e| e.to_string())?;
                    let arrangement_pattern = state
                        .bindings
                        .bind_pattern_definition(pattern_id)
                        .map_err(|e| e.to_string())?;
                    let arrangement_clip = arrangement
                        .create_pattern_clip(
                            track,
                            "identities",
                            FrameRange::new(Frame(0), Frame(i64::from(RATE))).unwrap(),
                            arrangement_pattern,
                        )
                        .map_err(|e| e.to_string())?;
                    state.domains.arrangement = arrangement.state().clone();
                    state
                        .bindings
                        .patterns
                        .placements
                        .insert(arrangement_clip, sequence_clip);
                    let bus = state
                        .domains
                        .mixer
                        .add_bus(BusKind::Source, "instrument bus")
                        .map_err(|e| e.to_string())?;
                    state.bindings.mixer.tracks.insert(track, bus);
                    ids = Some((media, sample_alias, bus));
                    Ok(())
                },
            )
            .unwrap();
        let (media, sample_alias, bus) = ids.unwrap();
        let pcm = AssetPcmMap::from([(
            media,
            PcmAsset::new(AudioFormat::new(RATE, 1).unwrap(), Arc::from([0.0; 8])).unwrap(),
        )]);
        let sample = SampleData::from_interleaved(RATE, 1, vec![0.8, 0.4, 0.0], 60, 0.0).unwrap();
        let config = DawEngineConfig {
            instruments: BTreeMap::from([
                (
                    7,
                    BuiltInInstrumentRoute {
                        definition: BuiltInInstrumentDefinition::Subtractive(SynthParams::default()),
                        bus,
                    },
                ),
                (
                    9,
                    BuiltInInstrumentRoute {
                        definition: BuiltInInstrumentDefinition::Sampler {
                            sample,
                            params: SamplerParams {
                                trigger_asset: Some(sample_alias.get()),
                                ..SamplerParams::default()
                            },
                        },
                        bus,
                    },
                ),
            ]),
            ..DawEngineConfig::default()
        };
        let addressed = render(&project, &pcm, 0, 64, &config);
        assert!(addressed
            .audio
            .interleaved()
            .iter()
            .any(|sample| sample.abs() > 0.01));
        assert!(!addressed
            .engine_diagnostics
            .iter()
            .any(|diagnostic| matches!(
                diagnostic,
                EngineDiagnostic::InstrumentNotSupplied { .. }
                    | EngineDiagnostic::UnroutableSequencerEvents { .. }
            )));

        // The same sampler/synth nodes and event arrays execute under an
        // arbitrary device partition without changing the offline product.
        let cancellation = RenderCancellation::new();
        let scheduled = compile_daw_engine(
            &project,
            &pcm,
            RenderWindow::new(0, 64).unwrap(),
            &config,
            &cancellation,
        )
        .unwrap();
        let plan = scheduled.native_render_plan().unwrap();
        let graph = compile_native_daw_graph(plan, Arc::new(scheduled)).unwrap();
        let offline = graph
            .render_scopes(
                RenderSpan::new(0, 64).unwrap(),
                &[RenderScope::Master],
                &cancellation,
            )
            .unwrap();
        let expected = &offline.outputs[&RenderScope::Master];
        let mut realtime = RealtimeGraphExecutor::new(Arc::clone(graph.graph())).unwrap();
        let mut device = vec![0.0_f32; expected.len()];
        let mut rendered_frames = 0_usize;
        for frames in [13_usize, 2, 17, 7, 25] {
            let start = rendered_frames * 2;
            let end = start + frames * 2;
            realtime
                .process_interleaved(&mut device[start..end])
                .unwrap();
            rendered_frames += frames;
        }
        assert_eq!(&device, expected.as_ref());

        let wrong_identity = DawEngineConfig {
            instruments: BTreeMap::from([(
                8,
                BuiltInInstrumentRoute {
                    definition: BuiltInInstrumentDefinition::Subtractive(SynthParams::default()),
                    bus,
                },
            )]),
            ..DawEngineConfig::default()
        };
        let schedule = compile_daw_engine(
            &project,
            &pcm,
            RenderWindow::new(0, 64).unwrap(),
            &wrong_identity,
            &cancellation,
        )
        .unwrap();
        assert!(schedule
            .engine_diagnostics()
            .contains(&EngineDiagnostic::InstrumentNotSupplied { instrument: 7 }));
        assert!(schedule
            .engine_diagnostics()
            .contains(&EngineDiagnostic::UnroutableSequencerEvents { count: 1 }));
        assert_silent(
            schedule
                .render_for_audition(&cancellation)
                .unwrap()
                .audio
                .interleaved(),
        );
    }

    #[test]
    fn virtual_selection_kit_pad_pattern_renders_exact_source_frames_by_default() {
        let mut project = DawProject::new("selection to pads", RATE, 60.0).unwrap();
        let mut media = None;
        project
            .transact(
                "register selected source",
                0,
                BTreeSet::from([ProjectDomain::Assets]),
                |state| -> Result<(), String> {
                    media = Some(
                        state
                            .domains
                            .assets
                            .register(registration(8))
                            .map_err(|error| error.to_string())?,
                    );
                    Ok(())
                },
            )
            .unwrap();
        let media = media.unwrap();
        let source = PcmAsset::new(
            AudioFormat::new(RATE, 1).unwrap(),
            Arc::from([0.11, 0.22, 0.31, -0.47, 0.83, 0.66, 0.55, 0.44]),
        )
        .unwrap();
        let pcm = AssetPcmMap::from([(media, source.clone())]);
        let slice = VirtualSliceRef::new(
            media,
            AssetFrameRange::new(SampleFrames(2), SampleFrames(5)).unwrap(),
        )
        .unwrap();
        let extracted = extract_virtual_slice(slice, &source).unwrap();

        let mut kit_ids = project.state().domains.sample_kits.clone();
        let kit_id = kit_ids.allocate_kit_id().unwrap();
        let pad_id = kit_ids.allocate_pad_id().unwrap();
        let zone_id = kit_ids.allocate_zone_id().unwrap();
        let mut mixer_ids = project.state().domains.mixer.clone();
        let output_bus = mixer_ids.add_bus(BusKind::Source, "Pads").unwrap();

        let mut kit = SampleKit::new(kit_id, "Pads", SampleRouteIntent::new(output_bus).unwrap());
        let mut pad = SamplePad::new(pad_id, "selection");
        pad.zone_order.push(zone_id);
        let mut zone = SampleZone::new(zone_id, pad_id, SourceMaterialRef::VirtualSlice(slice));
        zone.decoded_pcm = Some(extracted.identity);
        kit.pad_order.push(pad_id);
        kit.pads.insert(pad_id, pad);
        kit.zones.insert(zone_id, zone);

        let planned_pattern_id = PlannedPatternId::from_raw(1);
        let plan = ConstructiveEditPlan::new(
            "Sample selection",
            project.revisions().aggregate,
            vec![ConstructiveCause::ManualSelection { material: slice }],
            vec![PlannedMaterial {
                zone: zone_id,
                slice,
                decoded_pcm: extracted.identity,
                reuse: MaterialReusePolicy::RequireNew,
            }],
            KitMutation {
                before: None,
                after: kit,
            },
            Some(PlannedPattern {
                id: planned_pattern_id,
                name: "selection beat".into(),
                cycle: BeatDuration(PPQ as u64),
                seed: PatternSeed::EmptyGrid {
                    resolution: BeatDuration(PPQ as u64),
                },
                bindings: BTreeMap::from([("selection".into(), pad_id)]),
                steps: vec![PlannedStep {
                    pad: pad_id,
                    at: BeatTime::ZERO,
                    gate: BeatDuration(PPQ as u64),
                    velocity: 1.0,
                    probability: 1.0,
                    ratchets: 1,
                    pitch_semitones: 0.0,
                    pan: 0.0,
                    micro_offset_ticks: 0,
                    original_micro_offset_frames: None,
                    exact_source_onset_frame: Some(2),
                    evidence: Vec::new(),
                }],
            }),
            Some(PatternPlacementIntent {
                pattern: planned_pattern_id,
                start: BeatTime::ZERO,
                length: BeatDuration(PPQ as u64),
                pattern_offset: BeatTime::ZERO,
                looped: false,
                transpose_semitones: 0.0,
                gain: 1.0,
            }),
            ConstructiveFocus::Pattern(planned_pattern_id),
        )
        .unwrap();
        plan.prepare(&project)
            .unwrap()
            .commit(&mut project)
            .unwrap();

        let routes = crate::daw_engine::build_authoritative_sampler_routes(&project, &pcm).unwrap();
        assert!(routes.diagnostics.is_empty(), "{:?}", routes.diagnostics);
        assert_eq!(routes.routes.len(), 1);
        assert_eq!(&*routes.routes[0].sample.interleaved, &[0.31, -0.47, 0.83]);

        let cancellation = RenderCancellation::new();
        let schedule = compile_daw_engine(
            &project,
            &pcm,
            RenderWindow::new(0, 8).unwrap(),
            &DawEngineConfig::default(),
            &cancellation,
        )
        .unwrap();
        assert!(
            schedule.engine_diagnostics().is_empty(),
            "{:?}",
            schedule.engine_diagnostics()
        );
        let rendered = schedule.render_for_audition(&cancellation).unwrap();
        assert!(rendered.render_diagnostics.is_empty());
        let left: Vec<_> = rendered
            .audio
            .interleaved()
            .chunks_exact(2)
            .map(|frame| frame[0])
            .collect();
        assert!(left[0] > 0.0 && left[1] < 0.0 && left[2] > left[0]);
        assert!((left[1] / left[0] - (-0.47 / 0.31)).abs() < 1e-5);
        assert!((left[2] / left[0] - (0.83 / 0.31)).abs() < 1e-5);
        assert!(left[3..].iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn exact_windows_and_unimplemented_features_are_explicit_not_silently_faked() {
        let (mut project, asset, track, clip, pcm) = audio_project();
        let track_bus = project.state().bindings.mixer.tracks[&track];
        let revision = project.revisions().aggregate;
        let mut requested = None;
        project
            .transact(
                "request non-reference behavior",
                revision,
                BTreeSet::from([
                    ProjectDomain::Arrangement,
                    ProjectDomain::Mixer,
                    ProjectDomain::Bindings,
                ]),
                |state| -> Result<(), String> {
                    let mut arrangement =
                        ArrangementEditor::from_state(state.domains.arrangement.clone())
                            .map_err(|e| e.to_string())?;
                    arrangement
                        .stretch_resize(clip, Frame(20), StretchAlgorithm::PreservePitch, true)
                        .map_err(|e| e.to_string())?;
                    state.domains.arrangement = arrangement.state().clone();
                    let alternate = state
                        .domains
                        .mixer
                        .add_bus(BusKind::Group, "per-clip request")
                        .map_err(|e| e.to_string())?;
                    state.bindings.mixer.clip_overrides.insert(clip, alternate);
                    requested = Some(alternate);
                    Ok(())
                },
            )
            .unwrap();
        let cancellation = RenderCancellation::new();
        let schedule = compile_daw_engine(
            &project,
            &pcm,
            RenderWindow::new(4, 20).unwrap(),
            &DawEngineConfig::default(),
            &cancellation,
        )
        .unwrap();
        assert!(schedule.render_diagnostics().iter().any(|diagnostic| matches!(
            diagnostic,
            RenderDiagnostic::UnsupportedTimeTransform { clip: candidate, .. } if *candidate == clip
        )));
        assert!(schedule.engine_diagnostics().contains(
            &EngineDiagnostic::ClipBusOverrideUnsupported {
                clip,
                requested: requested.unwrap(),
                rendered_to: track_bus,
            }
        ));
        // Half-open end: the last requested frame is 19, never 20. The
        // unsupported transform renders silence instead of lying about a
        // pitch-preserving implementation.
        assert_eq!(
            schedule
                .render_for_audition(&cancellation)
                .unwrap()
                .audio
                .frame_count()
                .0,
            16
        );
        assert_silent(
            schedule
                .render_for_audition(&cancellation)
                .unwrap()
                .audio
                .interleaved(),
        );

        // Missing decoder data is equally explicit, even though an unrelated
        // asset identity exists in the project.
        let missing = compile_daw_engine(
            &project,
            &AssetPcmMap::new(),
            RenderWindow::new(4, 20).unwrap(),
            &DawEngineConfig::default(),
            &cancellation,
        )
        .unwrap();
        let alias = project
            .state()
            .bindings
            .assets
            .arrangement_assets
            .iter()
            .find_map(|(&alias, &bound)| (bound == asset).then_some(alias))
            .unwrap();
        assert!(missing
            .engine_diagnostics()
            .contains(&EngineDiagnostic::PcmNotSupplied {
                asset,
                arrangement_alias: alias,
            }));
    }

    #[test]
    fn native_graph_partitions_and_semantic_taps_share_one_exact_execution() {
        let (project, _asset, track, _clip, pcm) = audio_project();
        let source = project.state().bindings.mixer.tracks[&track];
        let cancellation = RenderCancellation::new();
        let config = DawEngineConfig {
            block_frames: 7,
            ..DawEngineConfig::default()
        };
        let schedule = compile_daw_engine(
            &project,
            &pcm,
            RenderWindow::new(0, 16).unwrap(),
            &config,
            &cancellation,
        )
        .unwrap();
        let plan = schedule.native_render_plan().unwrap();
        let native = compile_native_daw_graph(Arc::clone(&plan), Arc::new(schedule)).unwrap();
        let span = RenderSpan::new(0, 16).unwrap();
        let scopes = [
            RenderScope::Master,
            RenderScope::Track(track.get()),
            RenderScope::Bus {
                bus: source.get(),
                tap: BusTap::PreFader,
            },
            RenderScope::Bus {
                bus: source.get(),
                tap: BusTap::PostFader,
            },
        ];
        let offline = native.render_scopes(span, &scopes, &cancellation).unwrap();
        let master = &offline.outputs[&RenderScope::Master];
        assert_eq!(master, &offline.outputs[&RenderScope::Track(track.get())]);
        assert_eq!(
            master,
            &offline.outputs[&RenderScope::Bus {
                bus: source.get(),
                tap: BusTap::PreFader,
            }]
        );

        let mut realtime = RealtimeGraphExecutor::new(Arc::clone(native.graph())).unwrap();
        let mut device = vec![0.0_f32; master.len()];
        let channels = 2;
        let mut frame = 0_usize;
        for frames in [3_usize, 7, 1, 5] {
            let start = frame * channels;
            let end = start + frames * channels;
            assert_eq!(
                realtime
                    .process_interleaved(&mut device[start..end])
                    .unwrap(),
                frames
            );
            frame += frames;
        }
        assert_eq!(&device, master.as_ref());
    }

    #[test]
    fn bypassed_plugin_compensation_is_diagnosed_without_changing_reference_audio() {
        let (mut project, _asset, track, _clip, pcm) = audio_project();
        let source = project.state().bindings.mixer.tracks[&track];
        let revision = project.revisions().aggregate;
        let mut processor = None;
        project
            .transact(
                "install a latency-bearing bypassed processor beside a fast route",
                revision,
                BTreeSet::from([ProjectDomain::Mixer]),
                |state| -> Result<(), String> {
                    processor = Some(
                        state
                            .domains
                            .mixer
                            .insert_processor(
                                source,
                                None,
                                PluginDescriptor::new("clap", "org.audec.fixture", "fixture"),
                                11,
                            )
                            .map_err(|error| error.to_string())?,
                    );
                    state
                        .domains
                        .mixer
                        .add_bus(BusKind::Source, "parallel fast path")
                        .map_err(|error| error.to_string())?;
                    Ok(())
                },
            )
            .unwrap();
        let cancellation = RenderCancellation::new();
        let schedule = compile_daw_engine(
            &project,
            &pcm,
            RenderWindow::new(0, 16).unwrap(),
            &DawEngineConfig::default(),
            &cancellation,
        )
        .unwrap();
        assert!(schedule
            .render_diagnostics()
            .iter()
            .any(|diagnostic| matches!(
                diagnostic,
                RenderDiagnostic::PluginUnavailable { processor: candidate, .. }
                    if Some(*candidate) == processor
            )));
        let plan = schedule.native_render_plan().unwrap();
        let native = compile_native_daw_graph(plan, Arc::new(schedule)).unwrap();
        assert!(native
            .graph()
            .diagnostics()
            .iter()
            .any(|diagnostic| matches!(
                diagnostic,
                GraphDiagnostic::CompensationBypassed { frames: 11, .. }
            )));
        let master = native
            .render_scopes(
                RenderSpan::new(0, 16).unwrap(),
                &[RenderScope::Master],
                &cancellation,
            )
            .unwrap();
        assert_eq!(master.outputs[&RenderScope::Master][8], 0.10);
    }
    /// A 64-frame constant-amplitude source at project frame zero on its own
    /// routed track. Nothing in the graph varies over the clip, so a
    /// difference between the render halves is a gain curve or nothing.
    fn steady_project() -> (DawProject, crate::arrangement::ClipId, AssetPcmMap) {
        let mut project = DawProject::new("clip gain automation", RATE, 60.0).unwrap();
        let mut ids = None;
        project
            .transact(
                "install a steady source",
                0,
                BTreeSet::from([
                    ProjectDomain::Arrangement,
                    ProjectDomain::Assets,
                    ProjectDomain::Mixer,
                    ProjectDomain::Bindings,
                ]),
                |state| -> Result<(), String> {
                    let media = state
                        .domains
                        .assets
                        .register(registration(FADE_FRAMES as u64))
                        .map_err(|error| error.to_string())?;
                    let alias = state
                        .bindings
                        .bind_media_asset(media)
                        .map_err(|error| error.to_string())?;
                    let mut arrangement =
                        ArrangementEditor::from_state(state.domains.arrangement.clone())
                            .map_err(|error| error.to_string())?;
                    let track = arrangement
                        .create_track("steady", TrackKind::Audio)
                        .map_err(|error| error.to_string())?;
                    let clip = arrangement
                        .create_audio_clip(
                            track,
                            "steady",
                            FrameRange::new(Frame(0), Frame(FADE_FRAMES))
                                .map_err(|error| error.to_string())?,
                            alias,
                            SourceRange::new(0, FADE_FRAMES as u64)
                                .map_err(|error| error.to_string())?,
                        )
                        .map_err(|error| error.to_string())?;
                    state.domains.arrangement = arrangement.state().clone();
                    let bus = state
                        .domains
                        .mixer
                        .add_bus(BusKind::Source, "steady")
                        .map_err(|error| error.to_string())?;
                    state.bindings.mixer.tracks.insert(track, bus);
                    ids = Some((media, clip));
                    Ok(())
                },
            )
            .unwrap();
        let (media, clip) = ids.unwrap();
        let pcm = AssetPcmMap::from([(
            media,
            PcmAsset::new(
                AudioFormat::new(RATE, 1).unwrap(),
                Arc::from(vec![AMPLITUDE; FADE_FRAMES as usize]),
            )
            .unwrap(),
        )]);
        (project, clip, pcm)
    }

    fn rms(values: &[f64]) -> f64 {
        (values.iter().map(|value| value * value).sum::<f64>() / values.len() as f64).sqrt()
    }

    /// Left-channel RMS of the first and second halves of one render.
    fn half_rms(interleaved: &[f32]) -> (f64, f64) {
        let left: Vec<f64> = interleaved
            .chunks_exact(2)
            .map(|frame| f64::from(frame[0]))
            .collect();
        let half = left.len() / 2;
        (rms(&left[..half]), rms(&left[half..]))
    }

    /// The automation coverage this file lacked: a lane on an address
    /// `automation::address_is_rendered` claims the renderer reads must be
    /// audible in the product a musician exports, and removing it must give
    /// the bytes back exactly.
    #[test]
    fn a_clip_gain_lane_fades_the_render_and_removing_it_restores_the_bytes() {
        let (mut project, clip, pcm) = steady_project();
        let flat = render(&project, &pcm, 0, FADE_FRAMES, &DawEngineConfig::default());
        let (flat_first, flat_second) = half_rms(flat.audio.interleaved());
        assert!((flat_first - 0.5).abs() < 1.0e-6, "{flat_first}");
        assert!((flat_second - 0.5).abs() < 1.0e-6, "{flat_second}");

        let address = ParameterAddress::Clip {
            clip_id: clip.get(),
            parameter: ClipParameter::Gain,
        };
        let revision = project.revisions().aggregate;
        project
            .transact(
                "author a clip gain fade",
                revision,
                BTreeSet::from([ProjectDomain::Automation]),
                |state| -> Result<(), String> {
                    state
                        .domains
                        .automation
                        .register_parameter(ParameterDescriptor {
                            address: address.clone(),
                            name: "Clip gain".into(),
                            unit: ParameterUnit::Decibels,
                            minimum: -144.0,
                            maximum: 48.0,
                            default: 0.0,
                            mapping: ValueMapping::Linear,
                            smoothing: SmoothingPolicy::None,
                        })
                        .map_err(|error| error.to_string())?;
                    let lane = state
                        .domains
                        .automation
                        .create_lane("Clip gain fade", address.clone(), TimeDomain::Frames)
                        .map_err(|error| error.to_string())?;
                    state
                        .domains
                        .automation
                        .insert_point(
                            lane,
                            TimePosition::Frames(ProjectFrame(0)),
                            0.0,
                            SegmentShape::Linear,
                        )
                        .map_err(|error| error.to_string())?;
                    state
                        .domains
                        .automation
                        .insert_point(
                            lane,
                            TimePosition::Frames(ProjectFrame(FADE_FRAMES - 1)),
                            FADE_END_DB,
                            SegmentShape::Linear,
                        )
                        .map_err(|error| error.to_string())?;
                    Ok(())
                },
            )
            .unwrap();

        let faded = render(&project, &pcm, 0, FADE_FRAMES, &DawEngineConfig::default());
        let (first, second) = half_rms(faded.audio.interleaved());
        // A -60 dB ramp read at every frame: the first half averages
        // 0.1991 and the second 0.005960, a ratio of 0.0299.
        assert!((first - 0.199_095_6).abs() < 1.0e-4, "{first}");
        assert!((second - 0.005_960_1).abs() < 1.0e-4, "{second}");
        assert!(second / first < 0.031, "{}", second / first);

        let lane = project
            .state()
            .domains
            .automation
            .lanes()
            .next()
            .expect("the fade lane")
            .clone();
        let revision = project.revisions().aggregate;
        project
            .transact(
                "remove the fade",
                revision,
                BTreeSet::from([ProjectDomain::Automation]),
                |state| -> Result<(), String> {
                    state
                        .domains
                        .automation
                        .apply(&AutomationCommand {
                            label: "Remove the fade".into(),
                            parameters: Vec::new(),
                            changes: vec![LaneChange {
                                before: Some(lane.clone()),
                                after: None,
                            }],
                        })
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                },
            )
            .unwrap();
        let restored = render(&project, &pcm, 0, FADE_FRAMES, &DawEngineConfig::default());
        assert_eq!(restored.audio.interleaved(), flat.audio.interleaved());
    }

    // ---- native inserts -------------------------------------------------
    //
    // The strip's inserts stopped saying "not rendered" when `NativeNode::Insert`
    // arrived. Two things must hold for that to be true rather than merely
    // claimed: the effect must change the audio a musician exports, and the
    // history bound it declares must be enough that a tiled render is the
    // whole render, bit for bit.

    const INSERT_RATE: u32 = 44_100;
    const INSERT_FRAMES: i64 = 16_384;

    /// Deterministic broadband stereo-summing noise: an xorshift sequence has
    /// energy across the whole band, so a low-pass has something to remove.
    fn insert_noise(frames: usize) -> Vec<f32> {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        (0..frames)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (((state >> 40) as f32 / 8_388_608.0) - 1.0) * 0.5
            })
            .collect()
    }

    /// One noise clip on one source bus routed to master, at a real sample
    /// rate, so a filter's cutoff means what it says.
    fn insert_project() -> (DawProject, AssetPcmMap) {
        let mut project = DawProject::new("native insert", INSERT_RATE, 120.0).unwrap();
        let mut media_id = None;
        project
            .transact(
                "install a broadband source",
                0,
                BTreeSet::from([
                    ProjectDomain::Arrangement,
                    ProjectDomain::Assets,
                    ProjectDomain::Mixer,
                    ProjectDomain::Bindings,
                ]),
                |state| -> Result<(), String> {
                    let location = location();
                    let media = state
                        .domains
                        .assets
                        .register(AssetRegistration {
                            name: "broadband".into(),
                            location: location.clone(),
                            metadata: DecodedAudioMetadata {
                                sample_rate_hz: INSERT_RATE,
                                channels: 1,
                                frame_count: SampleFrames(INSERT_FRAMES as u64),
                                container: Some("wav".into()),
                                codec: Some("pcm_f32le".into()),
                                bit_depth: Some(32),
                            },
                            content: ContentFingerprint::from_bytes(b"native-insert-noise"),
                            provenance: AssetProvenance::new(
                                1,
                                AssetOrigin::ImportedFile {
                                    importer: "engine regression".into(),
                                },
                                location,
                            ),
                            tags: BTreeSet::new(),
                            favorite: false,
                        })
                        .map_err(|error| error.to_string())?;
                    let alias = state
                        .bindings
                        .bind_media_asset(media)
                        .map_err(|error| error.to_string())?;
                    let mut arrangement =
                        ArrangementEditor::from_state(state.domains.arrangement.clone())
                            .map_err(|error| error.to_string())?;
                    let track = arrangement
                        .create_track("broadband", TrackKind::Audio)
                        .map_err(|error| error.to_string())?;
                    arrangement
                        .create_audio_clip(
                            track,
                            "broadband",
                            FrameRange::new(Frame(0), Frame(INSERT_FRAMES))
                                .map_err(|error| error.to_string())?,
                            alias,
                            SourceRange::new(0, INSERT_FRAMES as u64)
                                .map_err(|error| error.to_string())?,
                        )
                        .map_err(|error| error.to_string())?;
                    state.domains.arrangement = arrangement.state().clone();
                    let bus = state
                        .domains
                        .mixer
                        .add_bus(BusKind::Source, "broadband")
                        .map_err(|error| error.to_string())?;
                    state.bindings.mixer.tracks.insert(track, bus);
                    media_id = Some(media);
                    Ok(())
                },
            )
            .unwrap();
        let pcm = AssetPcmMap::from([(
            media_id.unwrap(),
            PcmAsset::new(
                AudioFormat::new(INSERT_RATE, 1).unwrap(),
                Arc::from(insert_noise(INSERT_FRAMES as usize)),
            )
            .unwrap(),
        )]);
        (project, pcm)
    }

    /// Add one native effect to the master bus and set the parameters named,
    /// by key, to their normalized positions.
    fn add_master_insert(
        project: &mut DawProject,
        kind: crate::mixer::NativeEffectKind,
        settings: &[(&str, f32)],
    ) {
        let revision = project.revisions().aggregate;
        project
            .transact(
                "add a native insert",
                revision,
                BTreeSet::from([ProjectDomain::Mixer]),
                |state| -> Result<(), String> {
                    let master = state.domains.mixer.master();
                    let processor =
                        crate::effects::insert_native_effect(&mut state.domains.mixer, master, None, kind)
                            .map_err(|error| error.to_string())?;
                    for (key, normalized) in settings {
                        let id = state
                            .domains
                            .mixer
                            .processor(processor)
                            .and_then(|owner| owner.parameter_by_key(key))
                            .map(|parameter| parameter.id())
                            .ok_or_else(|| format!("no parameter {key}"))?;
                        state
                            .domains
                            .mixer
                            .set_parameter_value(processor, id, *normalized)
                            .map_err(|error| error.to_string())?;
                    }
                    Ok(())
                },
            )
            .unwrap();
    }

    /// Magnitude-weighted mean frequency of one channel, in Hz.
    fn spectral_centroid_hz(interleaved: &[f32], channels: usize) -> f64 {
        use rustfft::num_complex::Complex;
        use rustfft::FftPlanner;
        let mono: Vec<f32> = interleaved
            .chunks_exact(channels)
            .map(|frame| frame[0])
            .collect();
        let size = 1 << 12;
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(size);
        let mut weighted = 0.0_f64;
        let mut total = 0.0_f64;
        for window in mono.chunks_exact(size) {
            let mut buffer: Vec<Complex<f32>> = window
                .iter()
                .enumerate()
                .map(|(index, sample)| {
                    let hann = 0.5
                        - 0.5
                            * (std::f32::consts::TAU * index as f32 / size as f32).cos();
                    Complex::new(sample * hann, 0.0)
                })
                .collect();
            fft.process(&mut buffer);
            for (bin, value) in buffer.iter().take(size / 2).enumerate() {
                let magnitude = f64::from(value.norm());
                let frequency = bin as f64 * f64::from(INSERT_RATE) / size as f64;
                weighted += magnitude * frequency;
                total += magnitude;
            }
        }
        if total > 0.0 {
            weighted / total
        } else {
            0.0
        }
    }

    fn interleaved_rms(samples: &[f32]) -> f64 {
        rms(&samples.iter().map(|value| f64::from(*value)).collect::<Vec<_>>())
    }

    #[test]
    fn a_low_pass_insert_on_the_master_drops_the_spectral_centroid() {
        let (mut project, pcm) = insert_project();
        let config = DawEngineConfig::default();
        let dry = render(&project, &pcm, 0, INSERT_FRAMES, &config);
        let dry_centroid = spectral_centroid_hz(dry.audio.interleaved(), 2);
        assert!(
            dry_centroid > 4_000.0,
            "the fixture must be broadband: {dry_centroid} Hz"
        );

        // 200 Hz: the bottom eighth of the cutoff range, lightly damped.
        let cutoff = ((200.0_f32 / 40.0).ln() / (18_000.0_f32 / 40.0).ln()).clamp(0.0, 1.0);
        add_master_insert(
            &mut project,
            crate::mixer::NativeEffectKind::Filter,
            &[("mode", 0.0), ("cutoff", cutoff), ("resonance", 0.0)],
        );
        let wet = render(&project, &pcm, 0, INSERT_FRAMES, &config);
        let wet_centroid = spectral_centroid_hz(wet.audio.interleaved(), 2);
        assert!(
            wet_centroid < dry_centroid / 8.0,
            "a 200 Hz low-pass must move the centroid: dry {dry_centroid} Hz, wet {wet_centroid} Hz"
        );
        assert!(
            interleaved_rms(wet.audio.interleaved()) > 1.0e-4,
            "a filter is not a mute"
        );
        assert!(
            !wet.render_diagnostics.iter().any(|diagnostic| matches!(
                diagnostic,
                RenderDiagnostic::PluginUnavailable { .. }
                    | RenderDiagnostic::PluginBypassedByReferenceRenderer { .. }
            )),
            "a native insert is rendered, so it reports no bypass: {:?}",
            wet.render_diagnostics
        );
    }

    #[test]
    fn a_bypassed_insert_returns_the_dry_bytes_exactly() {
        let (mut project, pcm) = insert_project();
        let config = DawEngineConfig::default();
        let dry = render(&project, &pcm, 0, INSERT_FRAMES, &config);
        add_master_insert(
            &mut project,
            crate::mixer::NativeEffectKind::Filter,
            &[("cutoff", 0.0)],
        );
        let wet = render(&project, &pcm, 0, INSERT_FRAMES, &config);
        assert_ne!(wet.audio.interleaved(), dry.audio.interleaved());
        let revision = project.revisions().aggregate;
        project
            .transact(
                "bypass the insert",
                revision,
                BTreeSet::from([ProjectDomain::Mixer]),
                |state| -> Result<(), String> {
                    let processor = state
                        .domains
                        .mixer
                        .processors()
                        .map(|processor| processor.id())
                        .next()
                        .ok_or("the insert exists")?;
                    state
                        .domains
                        .mixer
                        .set_insert_bypassed(processor, true)
                        .map_err(|error| error.to_string())
                },
            )
            .unwrap();
        let bypassed = render(&project, &pcm, 0, INSERT_FRAMES, &config);
        assert_eq!(
            bypassed.audio.interleaved(),
            dry.audio.interleaved(),
            "an authored bypass is not a node at all, so the bytes are the dry bytes"
        );
    }

    /// The tile law with an insert present: the concatenated tiles are the
    /// whole bounce, bit for bit. Each tile renders its own `context` and is
    /// cropped to its `core`, exactly as `ExecutableRenderPlan::render_tile`
    /// does; the context comes from the layout, which reads the tileability
    /// the compiled graph declared.
    #[test]
    fn a_filter_insert_renders_byte_identically_whole_and_tiled() {
        use crate::render_plan::{DeterminismGrade, RenderPlan, Tileability};
        use crate::render_products::TileGrid;
        use crate::render_tiles::{TileLayout, TileRenderPolicy};

        let (mut project, pcm) = insert_project();
        add_master_insert(
            &mut project,
            crate::mixer::NativeEffectKind::Filter,
            &[("cutoff", 0.2), ("resonance", 0.7)],
        );
        let cancellation = RenderCancellation::new();
        let config = DawEngineConfig::default();
        let schedule = Arc::new(
            compile_daw_engine(
                &project,
                &pcm,
                RenderWindow::new(0, INSERT_FRAMES).unwrap(),
                &config,
                &cancellation,
            )
            .unwrap(),
        );
        // The controller's two-pass probe: compile under the conservative
        // contract, then plan under what the graph says it needs.
        let probe = schedule.native_render_plan().unwrap();
        let native = compile_native_daw_graph(Arc::clone(&probe), Arc::clone(&schedule))
            .unwrap()
            .graph()
            .native_tileability();
        let Tileability::BoundedHistory {
            lookbehind_frames, ..
        } = native
        else {
            panic!("a filter insert is bounded history, not {native:?}");
        };
        assert!(
            lookbehind_frames > 0 && lookbehind_frames < INSERT_FRAMES as u64,
            "the declared bound must be real and finite: {lookbehind_frames}"
        );
        let plan = Arc::new(RenderPlan::new(
            probe.id.clone(),
            DeterminismGrade::BitExact,
            native,
        ));
        let graph = compile_native_daw_graph(Arc::clone(&plan), Arc::clone(&schedule)).unwrap();
        let extent = plan.extent();
        let whole = graph
            .render_scopes(extent, &[RenderScope::Master], &cancellation)
            .unwrap()
            .outputs
            .remove(&RenderScope::Master)
            .unwrap();

        let layout = TileLayout::new(
            &plan,
            TileRenderPolicy::new(TileGrid::new(2_048).unwrap(), lookbehind_frames, native)
                .unwrap(),
        )
        .unwrap();
        assert!(layout.tiles().len() > 4, "the fixture must span many tiles");
        let channels = usize::from(plan.format().channels.get());
        let mut assembled: Vec<f32> = Vec::with_capacity(whole.len());
        for spec in layout.tiles() {
            let rendered = graph
                .render_scopes(spec.context, &[RenderScope::Master], &cancellation)
                .unwrap();
            let source = &rendered.outputs[&RenderScope::Master];
            let start = (spec.core.start - spec.context.start) as usize * channels;
            let end = start + spec.core.len() as usize * channels;
            assembled.extend_from_slice(&source[start..end]);
        }
        assert_eq!(assembled.len(), whole.len());
        let differing = assembled
            .iter()
            .zip(whole.iter())
            .filter(|(tile, oracle)| tile.to_bits() != oracle.to_bits())
            .count();
        assert_eq!(
            differing, 0,
            "{differing} of {} samples differ between the tiled and whole renders",
            whole.len()
        );
    }
}
