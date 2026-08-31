//! Deterministic musician gate for the sampling-to-playback path.
//!
//! This harness refuses to model widgets. It composes the real project
//! controller, constructive sampler path, object navigator, preview bridge,
//! DAW compiler, and transport source around one headless host. That makes a
//! failure name the broken architectural seam rather than a GPUI gesture.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::sync::Arc;

use super::{
    PreviewBus, PreviewController, PreviewOutcome, SamplePaneBridge, SamplePanePreviewOutcome,
};
use crate::assets::{
    AbsolutePath, AssetId, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
    AssetRegistry, ContentFingerprint, DecodedAudioMetadata, SampleFrames,
};
use crate::audio::{
    AudioFormat, PcmRenderer, ProjectAudio, ProjectFrame, TransportHandle, TransportSource,
};
use crate::audio_host::AuditionClip;
use crate::daw_engine::{compile_daw_engine, DawEngineConfig};
use crate::daw_render::{PcmAsset, RenderCancellation, RenderWindow};
use crate::live_project::{LiveProject, LiveProjectSnapshot, SourceMaterialMetadata};
use crate::project_controller::{
    recommend_constructive, ObjectNavigator, ObjectRef, PatternOccurrenceRef, ProjectController,
    WorkbenchSampleIntent,
};
use crate::sample_actions::{
    MakeBeatResultFocus, SampleAction, SampleAuditionIntent, SampleChopIntent, SampleKitDestination,
};
use crate::sample_material::SourceMaterialRef;
use crate::sequencer;
use crate::session::{Sample, SampleRange};
use crate::workspace_document::WorkspaceDocument;
use crate::workspace_items::WorkspaceViewId;

const SOURCE_RATE: u32 = 48_000;
const SECOND_TRIGGER_FRAME: u64 = 24_000;

/// The hardware-free analogue of `AudioHost`: one persistent project
/// transport plus one replaceable finite-preview bus.
struct OneHost {
    transport: TransportHandle,
    source: RefCell<TransportSource<PcmRenderer>>,
    preview: RefCell<Option<AuditionClip>>,
    preview_stops: Cell<u32>,
}

impl OneHost {
    fn new(project: ProjectAudio) -> Self {
        let (transport, source) = TransportSource::new(PcmRenderer::new(project));
        Self {
            transport,
            source: RefCell::new(source),
            preview: RefCell::new(None),
            preview_stops: Cell::new(0),
        }
    }

    fn preview_samples(&self) -> Vec<f32> {
        self.preview
            .borrow()
            .as_ref()
            .map(|clip| clip.interleaved().to_vec())
            .unwrap_or_default()
    }

    fn play_project_window(&self, start: u64, frames: usize) -> Vec<f32> {
        self.transport.seek(ProjectFrame(start));
        self.transport.play();
        let channels = usize::from(self.transport.format().channels.get());
        self.source
            .borrow_mut()
            .by_ref()
            .take(frames * channels)
            .collect()
    }
}

impl PreviewBus for OneHost {
    fn play_preview(&self, clip: AuditionClip) {
        *self.preview.borrow_mut() = Some(clip);
    }

    fn stop_preview(&self) {
        self.preview.borrow_mut().take();
        self.preview_stops
            .set(self.preview_stops.get().saturating_add(1));
    }
}

struct MusicianGate {
    controller: ProjectController,
    range: SampleRange,
}

impl MusicianGate {
    fn new() -> Self {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/audio/musician-gate-source.wav").unwrap()),
            None,
        )
        .unwrap();
        let mut registry = AssetRegistry::new();
        let asset = registry
            .register(AssetRegistration {
                name: "musician gate source".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: SOURCE_RATE,
                    channels: 1,
                    frame_count: SampleFrames(16),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32le".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"musician gate primary source"),
                provenance: AssetProvenance::new(
                    1,
                    AssetOrigin::ImportedFile {
                        importer: "musician-gate".into(),
                    },
                    location,
                ),
                tags: BTreeSet::new(),
                favorite: false,
            })
            .unwrap();
        // Three equal slices inside [1, 10). The second slice's distinctive
        // impulse occurs two frames after its beat, making a later audible
        // assertion immune to the original 16-frame source clip.
        let pcm = PcmAsset::new(
            AudioFormat::new(SOURCE_RATE, 1).unwrap(),
            Arc::from([
                0.05, 0.0, 1.0, 0.25, 0.0, 0.0, 0.8, 0.2, 0.0, 0.6, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0,
            ]),
        )
        .unwrap();
        let live = LiveProject::from_source_material(
            SourceMaterialMetadata::new("Musician Gate", "Primary Source"),
            registry,
            asset,
            pcm,
        )
        .unwrap();
        Self {
            controller: ProjectController::new(live).unwrap(),
            range: SampleRange::new(Sample::new(1), Sample::new(10)),
        }
    }

    fn make_sample_chop_and_beat(&mut self) -> crate::project_controller::WorkbenchSampleOutcome {
        let one_shot = self
            .controller
            .publish_primary_workbench_range(
                self.range,
                WorkbenchSampleIntent::OneShot {
                    kit: SampleKitDestination::NewKit,
                    target_bus: None,
                },
            )
            .unwrap();
        assert!(one_shot.constructive.publication.pattern.is_none());
        assert!(one_shot.constructive.publication.arrangement_clip.is_none());

        let chop = self
            .controller
            .publish_primary_workbench_range(
                self.range,
                WorkbenchSampleIntent::Chop {
                    chop: SampleChopIntent::EqualSlices { count: 3 },
                    kit: SampleKitDestination::NewKit,
                    target_bus: None,
                },
            )
            .unwrap();
        assert!(chop.constructive.publication.pattern.is_none());
        assert!(chop.constructive.publication.arrangement_clip.is_none());

        let beat = self
            .controller
            .publish_primary_workbench_range(
                self.range,
                WorkbenchSampleIntent::MakeBeat {
                    chop: SampleChopIntent::EqualSlices { count: 3 },
                    kit: SampleKitDestination::NewKit,
                    target_bus: None,
                    bars: 1,
                    quantize_ticks: sequencer::PPQ as u64,
                    // This is the focus used by the musician-facing workbench
                    // action. The placement remains a related durable object.
                    result_focus: MakeBeatResultFocus::PatternEditor,
                },
            )
            .unwrap();
        assert!(beat.constructive.publication.pattern.is_some());
        assert!(beat.constructive.publication.arrangement_clip.is_some());
        assert_eq!(self.controller.journal_records().len(), 3);
        beat
    }

    fn compile_project_audio(&self) -> ProjectAudio {
        let snapshot = self.controller.snapshot();
        let cancellation = RenderCancellation::new();
        let schedule = compile_daw_engine(
            &snapshot.project,
            &snapshot.pcm,
            RenderWindow::new(0, 24_032).unwrap(),
            &DawEngineConfig::default(),
            &cancellation,
        )
        .unwrap();
        assert!(
            schedule.engine_diagnostics().is_empty(),
            "engine diagnostics: {:?}",
            schedule.engine_diagnostics()
        );
        // The reference schedule reports broad pattern/instrument work before
        // the engine consumes its explicitly linked sampler subset. Only the
        // final engine render is the musician-visible diagnostic boundary.
        let rendered = schedule.render_for_audition(&cancellation).unwrap();
        assert!(rendered.engine_diagnostics.is_empty());
        assert!(rendered.render_diagnostics.is_empty());
        rendered.into_project_audio()
    }
}

fn apply_audition(
    controller: &mut ProjectController,
    snapshot: &LiveProjectSnapshot,
    bridge: SamplePaneBridge,
    previews: &mut PreviewController,
    host: &OneHost,
    intent: SampleAuditionIntent,
) -> crate::pane_audio::SampleAuditionTicket {
    let action = SampleAction::Audition(intent);
    let ticket = bridge.begin_audition(previews, intent).unwrap();
    let outcome = controller.execute_sample_action(action.clone()).unwrap();
    let pane = bridge
        .resolve_outcome(snapshot, &action, outcome, Some(ticket))
        .unwrap();
    assert_eq!(
        pane.preview.unwrap().apply(previews, host),
        SamplePanePreviewOutcome::Preview(PreviewOutcome::Played(ticket.request))
    );
    ticket
}

#[test]
fn selected_material_reveals_previews_and_plays_as_one_coherent_musician_path() {
    let mut gate = MusicianGate::new();
    let beat = gate.make_sample_chop_and_beat();
    let publication = &beat.constructive.publication;
    let arrangement_clip = publication.arrangement_clip.unwrap();
    let pattern = publication.pattern.unwrap();
    let kit = publication.kit;
    // Arrangement-focused receipts intentionally name the occurrence rather
    // than pretending one pad is the result. Audition the first durable pad
    // from the published kit itself.
    let pad = gate
        .controller
        .snapshot()
        .project
        .state()
        .domains
        .sample_kits
        .kits[&kit]
        .pad_order[0];

    let recommendation = recommend_constructive(publication);
    assert_eq!(recommendation.request.object, ObjectRef::Pattern(pattern));
    assert!(recommendation
        .request
        .related
        .contains(&ObjectRef::PatternOccurrence(PatternOccurrenceRef {
            arrangement_clip,
            sequencer_clip: None,
            pattern: Some(pattern),
        })));
    let reveal = ObjectNavigator::plan(&WorkspaceDocument::default(), recommendation.request);
    assert_eq!(reveal.selection.primary, ObjectRef::Pattern(pattern));
    assert!(reveal.diagnostics.is_empty());

    let project_audio = gate.compile_project_audio();
    let host = OneHost::new(project_audio);
    let bridge = SamplePaneBridge::new(WorkspaceViewId(81)).unwrap();
    let mut previews = PreviewController::default();

    // Browser playback must resolve the selected asset, even while a different
    // primary source owns the workbench. This augmented immutable publication
    // models a decoded media-pool selection without changing project truth.
    let browser_asset = AssetId(9_999);
    let browser_pcm = PcmAsset::new(
        AudioFormat::new(SOURCE_RATE, 1).unwrap(),
        Arc::from([0.91, 0.73]),
    )
    .unwrap();
    let mut browser_pcm_map = gate.controller.snapshot().pcm.as_ref().clone();
    browser_pcm_map.insert(browser_asset, browser_pcm);
    let browser_snapshot = LiveProjectSnapshot {
        project: Arc::clone(&gate.controller.snapshot().project),
        pcm: Arc::new(browser_pcm_map),
        sample_pcm: Arc::clone(&gate.controller.snapshot().sample_pcm),
    };
    let browser_intent = SampleAuditionIntent::MaterialOneShot {
        material: SourceMaterialRef::Asset(browser_asset),
        velocity: 1.0,
    };
    let _browser = apply_audition(
        &mut gate.controller,
        &browser_snapshot,
        bridge,
        &mut previews,
        &host,
        browser_intent,
    );
    assert!(host
        .preview_samples()
        .iter()
        .copied()
        .any(|sample| sample > 0.6));

    // The exact authored pad supersedes the browser preview on the same host.
    let project_snapshot = gate.controller.snapshot().clone();
    let pad_intent = SampleAuditionIntent::PadGate {
        kit,
        pad,
        velocity: 1.0,
        pressed: true,
    };
    let pad_ticket = apply_audition(
        &mut gate.controller,
        &project_snapshot,
        bridge,
        &mut previews,
        &host,
        pad_intent,
    );
    let pad_clip = host.preview.borrow().as_ref().unwrap().clone();
    assert!(pad_clip
        .interleaved()
        .iter()
        .copied()
        .any(|sample| sample > 0.6));

    // Global project play cancels both active and pending finite previews. A
    // completion already in flight cannot restart the preview afterward.
    assert!(previews.cancel_all(&host));
    assert_eq!(host.preview_stops.get(), 1);
    assert!(host.preview.borrow().is_none());
    assert_eq!(
        previews.complete(&host, pad_ticket.request, pad_clip),
        PreviewOutcome::IgnoredStale(pad_ticket.request)
    );
    assert!(host.preview.borrow().is_none());

    // Frame 24_002 is the distinctive impulse in slice two. The 16-frame
    // primary clip is long over, so non-silence here proves the authored pad,
    // pattern, placement, binding, sampler route, bounce, and transport.
    let played = host.play_project_window(SECOND_TRIGGER_FRAME - 4, 16);
    assert!(
        played.iter().copied().any(|sample| sample.abs() > 0.25),
        "programmed sample trigger was silent: {played:?}"
    );
    assert!(host.preview.borrow().is_none());
}
