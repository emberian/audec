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
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        ContentFingerprint, DecodedAudioMetadata, ProjectRelativePath, SampleFrames,
    };
    use crate::audio::AudioFormat;
    use crate::daw_engine::{
        compile_daw_engine, AssetPcmMap, BuiltInInstrumentDefinition, BuiltInInstrumentRoute,
        DawEngineConfig, EngineDiagnostic,
    };
    use crate::daw_project::{DawProject, ProjectDomain};
    use crate::daw_render::{PcmAsset, RenderCancellation, RenderDiagnostic, RenderWindow};
    use crate::instruments::{SampleData, SamplerParams, SynthParams};
    use crate::mixer::BusKind;
    use crate::sequencer::{
        BeatDuration, BeatTime, PatternClip, PatternContent, PatternDefinition, SequencerCommand,
        StepEvent, StepLane, StepPattern, TriggerTarget, PPQ,
    };

    const RATE: u32 = 1_000;

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
        let cancellation = RenderCancellation::new();
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
}
