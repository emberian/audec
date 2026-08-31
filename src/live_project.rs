//! Shared, editable runtime ownership for one audible DAW project.
//!
//! The domain models deliberately use independent identity spaces and are
//! useful to different editors.  [`LiveProject`] keeps those models behind
//! individually shareable locks, then reconciles an all-locks-held view into
//! the validated [`DawProject`] aggregate before publishing a snapshot or an
//! engine schedule.  A caller can therefore inject just the domain an editor
//! needs without weakening the aggregate validation boundary used by render.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::arrangement::{self, ArrangementEditor, Frame, FrameRange, SourceRange, TrackKind};
use crate::assets::{self, AssetRegistry, AssetUsageOwner};
use crate::automation::AutomationGraph;
use crate::daw_engine::{
    compile_daw_engine, AssetPcmMap, DawEngineConfig, DawEngineError, DawEngineSchedule,
};
use crate::daw_project::{
    BridgeError, DawProject, ProjectBindings, ProjectDomain, ProjectRevisions,
};
use crate::daw_render::{PcmAsset, RenderCancellation, RenderWindow};
use crate::mixer::{self, BusKind, MixerGraph};
use crate::sequencer::Sequencer;

/// Human and timeline facts needed to turn an existing media-pool entry into
/// the initial source clip.  Immutable audio facts continue to live in the
/// registry record and are not duplicated here.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceMaterialMetadata {
    pub project_name: String,
    pub track_name: String,
    pub clip_name: String,
    pub timeline_start: Frame,
    /// `None` means the complete decoded asset.
    pub source_range: Option<assets::AssetFrameRange>,
    pub initial_bpm: f64,
}

impl SourceMaterialMetadata {
    pub fn new(project_name: impl Into<String>, source_name: impl Into<String>) -> Self {
        let source_name = source_name.into();
        Self {
            project_name: project_name.into(),
            track_name: source_name.clone(),
            clip_name: source_name,
            timeline_start: Frame::ZERO,
            source_range: None,
            initial_bpm: 120.0,
        }
    }
}

/// Every identity allocated while crossing from a registry asset to the
/// arrangement and mixer.  The fields intentionally retain their domain types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceMaterialIds {
    pub registry_asset: assets::AssetId,
    pub arrangement_asset: arrangement::AssetId,
    pub asset_usage: assets::AssetUsageId,
    pub track: arrangement::TrackId,
    pub clip: arrangement::ClipId,
    pub bus: mixer::BusId,
}

/// Independently injectable editor domains.
///
/// Consumers should not hold one of these locks while calling a method on
/// [`LiveProject`].  Controller operations acquire all locks in the field order
/// below to take a coherent cross-domain view.
#[derive(Clone, Debug)]
pub struct LiveProjectDomains {
    pub arrangement: Arc<Mutex<ArrangementEditor>>,
    pub sequencer: Arc<Mutex<Sequencer>>,
    pub automation: Arc<Mutex<AutomationGraph>>,
    pub assets: Arc<Mutex<AssetRegistry>>,
    pub mixer: Arc<Mutex<MixerGraph>>,
    pub bindings: Arc<Mutex<ProjectBindings>>,
    /// Decoder/runtime data, keyed only by registry IDs.
    pub pcm: Arc<Mutex<AssetPcmMap>>,
}

/// A coherent, validated render input.  Both members were cloned while every
/// editable domain lock was held, so later editor changes cannot affect it.
#[derive(Clone, Debug)]
pub struct LiveProjectSnapshot {
    pub project: Arc<DawProject>,
    pub pcm: Arc<AssetPcmMap>,
}

impl LiveProjectSnapshot {
    pub fn revisions(&self) -> ProjectRevisions {
        self.project.revisions()
    }

    pub fn is_dirty(&self) -> bool {
        self.project.is_dirty()
    }
}

#[derive(Debug)]
struct PublishedState {
    project: DawProject,
}

/// Runtime project controller shared by the workspace and its editors.
#[derive(Clone, Debug)]
pub struct LiveProject {
    domains: LiveProjectDomains,
    source: SourceMaterialIds,
    published: Arc<Mutex<PublishedState>>,
}

impl LiveProject {
    /// Build an immediately audible one-track project from an existing,
    /// registered media asset and its decoded PCM.
    ///
    /// The project sample rate is the source asset's rate.  This keeps the
    /// initial placement sample-exact; later imports remain free to use the
    /// renderer's rational resampling path.
    pub fn from_source_material(
        metadata: SourceMaterialMetadata,
        mut registry: AssetRegistry,
        asset: assets::AssetId,
        pcm: PcmAsset,
    ) -> Result<Self, LiveProjectError> {
        validate_source_names(&metadata)?;
        let record = registry
            .get(asset)
            .ok_or(LiveProjectError::MissingAsset(asset))?;
        let decoded = record.metadata().clone();
        validate_pcm(asset, &decoded, &pcm)?;

        let source_range = metadata.source_range.unwrap_or(assets::AssetFrameRange {
            start: assets::SampleFrames(0),
            end: decoded.frame_count,
        });
        if !source_range.is_within(decoded.frame_count) || source_range.start >= source_range.end {
            return Err(LiveProjectError::InvalidSourceRange {
                asset,
                start: source_range.start.0,
                end: source_range.end.0,
                frames: decoded.frame_count.0,
            });
        }

        let mut project = DawProject::new(
            metadata.project_name.clone(),
            decoded.sample_rate_hz,
            metadata.initial_bpm,
        )?;
        let mut source_ids = None;
        project.transact(
            "Create source material",
            project.revisions().aggregate,
            BTreeSet::from([
                ProjectDomain::Arrangement,
                ProjectDomain::Assets,
                ProjectDomain::Mixer,
                ProjectDomain::Bindings,
            ]),
            |state| -> Result<(), String> {
                let arrangement_asset = state
                    .bindings
                    .bind_media_asset(asset)
                    .map_err(|error| error.to_string())?;
                let mut editor = ArrangementEditor::from_state(state.domains.arrangement.clone())
                    .map_err(|error| error.to_string())?;
                let track = editor
                    .create_track(metadata.track_name.clone(), TrackKind::Audio)
                    .map_err(|error| error.to_string())?;
                let placement =
                    FrameRange::from_start_and_len(metadata.timeline_start, source_range.len().0)
                        .map_err(|error| error.to_string())?;
                let clip = editor
                    .create_audio_clip(
                        track,
                        metadata.clip_name.clone(),
                        placement,
                        arrangement_asset,
                        SourceRange::new(source_range.start.0, source_range.end.0)
                            .map_err(|error| error.to_string())?,
                    )
                    .map_err(|error| error.to_string())?;
                let bus = state
                    .domains
                    .mixer
                    .add_bus(BusKind::Source, metadata.track_name.clone())
                    .map_err(|error| error.to_string())?;
                state.bindings.mixer.tracks.insert(track, bus);
                let usage = registry
                    .add_usage(
                        asset,
                        AssetUsageOwner::AudioClip {
                            persistent_id: clip.get(),
                        },
                        Some(source_range),
                        metadata.clip_name.clone(),
                    )
                    .map_err(|error| error.to_string())?;
                state.domains.arrangement = editor.state().clone();
                state.domains.assets = registry.clone();
                source_ids = Some(SourceMaterialIds {
                    registry_asset: asset,
                    arrangement_asset,
                    asset_usage: usage,
                    track,
                    clip,
                    bus,
                });
                Ok(())
            },
        )?;

        let arrangement =
            ArrangementEditor::from_state(project.state().domains.arrangement.clone())
                .map_err(|error| LiveProjectError::Domain(error.to_string()))?;
        let domains = LiveProjectDomains {
            arrangement: Arc::new(Mutex::new(arrangement)),
            sequencer: Arc::new(Mutex::new(project.state().domains.sequencer.clone())),
            automation: Arc::new(Mutex::new(project.state().domains.automation.clone())),
            assets: Arc::new(Mutex::new(project.state().domains.assets.clone())),
            mixer: Arc::new(Mutex::new(project.state().domains.mixer.clone())),
            bindings: Arc::new(Mutex::new(project.state().bindings.clone())),
            pcm: Arc::new(Mutex::new(AssetPcmMap::from([(asset, pcm)]))),
        };

        Ok(Self {
            domains,
            source: source_ids.expect("source IDs are assigned by the committed transaction"),
            published: Arc::new(Mutex::new(PublishedState { project })),
        })
    }

    pub fn domains(&self) -> LiveProjectDomains {
        self.domains.clone()
    }

    pub const fn source_ids(&self) -> SourceMaterialIds {
        self.source
    }

    /// Reconcile editor state, validate all domain bindings, and freeze it.
    /// Invalid edits are reported without replacing the last valid aggregate.
    pub fn snapshot(&self) -> Result<LiveProjectSnapshot, LiveProjectError> {
        let held = self.lock_domains()?;
        validate_supplied_pcm(&held.assets, &held.pcm)?;
        let mut published = lock(&self.published, "published project")?;
        reconcile(&mut published.project, &held)?;
        Ok(LiveProjectSnapshot {
            project: Arc::new(published.project.clone()),
            pcm: Arc::new(held.pcm.clone()),
        })
    }

    /// Validate the exact currently edited state.
    pub fn require_valid(&self) -> Result<(), LiveProjectError> {
        self.snapshot().map(|_| ())
    }

    /// Revision queries reconcile first, so mutations made through an injected
    /// editor domain are reflected in the returned aggregate token.
    pub fn revisions(&self) -> Result<ProjectRevisions, LiveProjectError> {
        Ok(self.snapshot()?.revisions())
    }

    pub fn is_dirty(&self) -> Result<bool, LiveProjectError> {
        Ok(self.snapshot()?.is_dirty())
    }

    pub fn mark_saved(&self) -> Result<ProjectRevisions, LiveProjectError> {
        let held = self.lock_domains()?;
        validate_supplied_pcm(&held.assets, &held.pcm)?;
        let mut published = lock(&self.published, "published project")?;
        reconcile(&mut published.project, &held)?;
        published.project.mark_saved();
        Ok(published.project.revisions())
    }

    /// Compile a frozen engine schedule for an explicit project window.
    pub fn compile_engine(
        &self,
        window: RenderWindow,
        config: &DawEngineConfig,
        cancellation: &RenderCancellation,
    ) -> Result<DawEngineSchedule, LiveProjectError> {
        let snapshot = self.snapshot()?;
        Ok(compile_daw_engine(
            &snapshot.project,
            &snapshot.pcm,
            window,
            config,
            cancellation,
        )?)
    }

    /// Compile through the complete occupied arrangement range for audition.
    /// Non-negative projects retain a zero origin (and therefore any leading
    /// silence).  Negative material retains its signed origin in the schedule;
    /// [`crate::daw_engine::DawEngineRender::origin_frame`] lets a transport
    /// adapter decide how to handle that preroll.
    pub fn compile_audition(
        &self,
        config: &DawEngineConfig,
        cancellation: &RenderCancellation,
    ) -> Result<DawEngineSchedule, LiveProjectError> {
        let snapshot = self.snapshot()?;
        let range = snapshot
            .project
            .state()
            .domains
            .arrangement
            .project_range()
            .ok_or(LiveProjectError::EmptyArrangement)?;
        let start = range.start.get().min(0);
        let window = RenderWindow::new(start, range.end.get())
            .map_err(|error| LiveProjectError::Domain(error.to_string()))?;
        Ok(compile_daw_engine(
            &snapshot.project,
            &snapshot.pcm,
            window,
            config,
            cancellation,
        )?)
    }

    fn lock_domains(&self) -> Result<HeldDomains<'_>, LiveProjectError> {
        Ok(HeldDomains {
            arrangement: lock(&self.domains.arrangement, "arrangement")?,
            sequencer: lock(&self.domains.sequencer, "sequencer")?,
            automation: lock(&self.domains.automation, "automation")?,
            assets: lock(&self.domains.assets, "assets")?,
            mixer: lock(&self.domains.mixer, "mixer")?,
            bindings: lock(&self.domains.bindings, "bindings")?,
            pcm: lock(&self.domains.pcm, "PCM")?,
        })
    }
}

struct HeldDomains<'a> {
    arrangement: MutexGuard<'a, ArrangementEditor>,
    sequencer: MutexGuard<'a, Sequencer>,
    automation: MutexGuard<'a, AutomationGraph>,
    assets: MutexGuard<'a, AssetRegistry>,
    mixer: MutexGuard<'a, MixerGraph>,
    bindings: MutexGuard<'a, ProjectBindings>,
    pcm: MutexGuard<'a, AssetPcmMap>,
}

fn reconcile(project: &mut DawProject, held: &HeldDomains<'_>) -> Result<(), LiveProjectError> {
    let current = project.state();
    let arrangement = held.arrangement.state();
    let mut touched = BTreeSet::new();
    if &current.domains.arrangement != arrangement {
        touched.insert(ProjectDomain::Arrangement);
    }
    if !sequencers_equal(&current.domains.sequencer, &held.sequencer) {
        touched.insert(ProjectDomain::Sequencer);
    }
    if current.domains.automation != *held.automation {
        touched.insert(ProjectDomain::Automation);
    }
    if current.domains.assets != *held.assets {
        touched.insert(ProjectDomain::Assets);
    }
    if current.domains.mixer != *held.mixer {
        touched.insert(ProjectDomain::Mixer);
    }
    if current.bindings != *held.bindings {
        touched.insert(ProjectDomain::Bindings);
    }
    if touched.is_empty() {
        project.require_valid()?;
        return Ok(());
    }

    let arrangement = arrangement.clone();
    let sequencer = held.sequencer.clone();
    let automation = held.automation.clone();
    let assets = held.assets.clone();
    let mixer = held.mixer.clone();
    let bindings = held.bindings.clone();
    project.transact(
        "Synchronize live editors",
        project.revisions().aggregate,
        touched,
        move |state| -> Result<(), String> {
            state.domains.arrangement = arrangement;
            state.domains.sequencer = sequencer;
            state.domains.automation = automation;
            state.domains.assets = assets;
            state.domains.mixer = mixer;
            state.bindings = bindings;
            Ok(())
        },
    )?;
    Ok(())
}

fn sequencers_equal(left: &Sequencer, right: &Sequencer) -> bool {
    left.revision() == right.revision()
        && left.tempo_map() == right.tempo_map()
        && left.patterns().patterns().eq(right.patterns().patterns())
        && left.clips().eq(right.clips())
}

fn validate_source_names(metadata: &SourceMaterialMetadata) -> Result<(), LiveProjectError> {
    for (field, value) in [
        ("project_name", metadata.project_name.as_str()),
        ("track_name", metadata.track_name.as_str()),
        ("clip_name", metadata.clip_name.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(LiveProjectError::EmptyField(field));
        }
    }
    Ok(())
}

fn validate_supplied_pcm(
    registry: &AssetRegistry,
    pcm: &AssetPcmMap,
) -> Result<(), LiveProjectError> {
    for (&asset, decoded_pcm) in pcm {
        let record = registry
            .get(asset)
            .ok_or(LiveProjectError::PcmForUnknownAsset(asset))?;
        validate_pcm(asset, record.metadata(), decoded_pcm)?;
    }
    Ok(())
}

fn validate_pcm(
    asset: assets::AssetId,
    metadata: &assets::DecodedAudioMetadata,
    pcm: &PcmAsset,
) -> Result<(), LiveProjectError> {
    let actual_sample_rate = pcm.format.sample_rate.get();
    let actual_channels = pcm.format.channels.get();
    let actual_frames = pcm.frame_count();
    if metadata.sample_rate_hz != actual_sample_rate
        || metadata.channels != actual_channels
        || metadata.frame_count.0 != actual_frames
    {
        return Err(LiveProjectError::PcmMetadataMismatch {
            asset,
            expected_sample_rate: metadata.sample_rate_hz,
            actual_sample_rate,
            expected_channels: metadata.channels,
            actual_channels,
            expected_frames: metadata.frame_count.0,
            actual_frames,
        });
    }
    Ok(())
}

fn lock<'a, T>(
    mutex: &'a Mutex<T>,
    domain: &'static str,
) -> Result<MutexGuard<'a, T>, LiveProjectError> {
    mutex
        .lock()
        .map_err(|_| LiveProjectError::LockPoisoned(domain))
}

#[derive(Debug)]
pub enum LiveProjectError {
    EmptyField(&'static str),
    MissingAsset(assets::AssetId),
    PcmForUnknownAsset(assets::AssetId),
    InvalidSourceRange {
        asset: assets::AssetId,
        start: u64,
        end: u64,
        frames: u64,
    },
    PcmMetadataMismatch {
        asset: assets::AssetId,
        expected_sample_rate: u32,
        actual_sample_rate: u32,
        expected_channels: u16,
        actual_channels: u16,
        expected_frames: u64,
        actual_frames: u64,
    },
    EmptyArrangement,
    LockPoisoned(&'static str),
    Domain(String),
    Project(BridgeError),
    Engine(DawEngineError),
}

impl fmt::Display for LiveProjectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField(field) => write!(formatter, "{field} cannot be empty"),
            Self::MissingAsset(asset) => write!(formatter, "media asset {} is not registered", asset.0),
            Self::PcmForUnknownAsset(asset) => {
                write!(formatter, "PCM was supplied for unknown media asset {}", asset.0)
            }
            Self::InvalidSourceRange {
                asset,
                start,
                end,
                frames,
            } => write!(
                formatter,
                "source range {start}..{end} is outside asset {} (0..{frames})",
                asset.0
            ),
            Self::PcmMetadataMismatch {
                asset,
                expected_sample_rate,
                actual_sample_rate,
                expected_channels,
                actual_channels,
                expected_frames,
                actual_frames,
            } => write!(
                formatter,
                "PCM for asset {} is {actual_sample_rate} Hz/{actual_channels} ch/{actual_frames} frames, expected {expected_sample_rate} Hz/{expected_channels} ch/{expected_frames} frames",
                asset.0
            ),
            Self::EmptyArrangement => write!(formatter, "the arrangement has no occupied range"),
            Self::LockPoisoned(domain) => write!(formatter, "the {domain} editor lock is poisoned"),
            Self::Domain(error) => formatter.write_str(error),
            Self::Project(error) => write!(formatter, "live project is invalid: {error}"),
            Self::Engine(error) => write!(formatter, "live project cannot be compiled: {error}"),
        }
    }
}

impl Error for LiveProjectError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Project(error) => Some(error),
            Self::Engine(error) => Some(error),
            _ => None,
        }
    }
}

impl From<BridgeError> for LiveProjectError {
    fn from(error: BridgeError) -> Self {
        Self::Project(error)
    }
}

impl From<DawEngineError> for LiveProjectError {
    fn from(error: DawEngineError) -> Self {
        Self::Engine(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        ContentFingerprint, DecodedAudioMetadata, SampleFrames,
    };
    use crate::audio::AudioFormat;

    fn source() -> (AssetRegistry, assets::AssetId, PcmAsset) {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/audio/source.wav").unwrap()),
            None,
        )
        .unwrap();
        let mut registry = AssetRegistry::new();
        let asset = registry
            .register(AssetRegistration {
                name: "source".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: 48_000,
                    channels: 1,
                    frame_count: SampleFrames(4),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32le".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"live source"),
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
            Arc::from([0.25, 0.5, 0.75, 1.0]),
        )
        .unwrap();
        (registry, asset, pcm)
    }

    fn live() -> LiveProject {
        let (registry, asset, pcm) = source();
        LiveProject::from_source_material(
            SourceMaterialMetadata::new("Song", "Source"),
            registry,
            asset,
            pcm,
        )
        .unwrap()
    }

    #[test]
    fn constructs_typed_cross_domain_source_and_valid_snapshot() {
        let live = live();
        let ids = live.source_ids();
        let snapshot = live.snapshot().unwrap();
        let state = snapshot.project.state();
        assert_eq!(
            state.bindings.assets.arrangement_assets[&ids.arrangement_asset],
            ids.registry_asset
        );
        assert_eq!(state.bindings.mixer.tracks[&ids.track], ids.bus);
        assert_eq!(
            state.domains.arrangement.clip(ids.clip).unwrap().track_id,
            ids.track
        );
        assert!(state
            .domains
            .assets
            .get(ids.registry_asset)
            .unwrap()
            .usages()
            .contains_key(&ids.asset_usage));
        assert_eq!(snapshot.pcm[&ids.registry_asset].frame_count(), 4);
        assert!(snapshot.project.validate().is_empty());
    }

    #[test]
    fn injected_editor_edits_advance_revision_and_dirty_state() {
        let live = live();
        let initial = live.mark_saved().unwrap();
        assert!(!live.is_dirty().unwrap());

        let domains = live.domains();
        domains
            .arrangement
            .lock()
            .unwrap()
            .move_clip(
                live.source_ids().clip,
                live.source_ids().track,
                Frame::new(8),
            )
            .unwrap();

        let changed = live.revisions().unwrap();
        assert_eq!(changed.aggregate, initial.aggregate + 1);
        assert_eq!(changed.arrangement, initial.arrangement + 1);
        assert!(live.is_dirty().unwrap());
        assert_eq!(
            live.snapshot()
                .unwrap()
                .project
                .state()
                .domains
                .arrangement
                .clip(live.source_ids().clip)
                .unwrap()
                .placement
                .start,
            Frame::new(8)
        );
    }

    #[test]
    fn invalid_cross_domain_edit_is_not_published() {
        let live = live();
        let valid_revision = live.revisions().unwrap();
        live.domains()
            .bindings
            .lock()
            .unwrap()
            .mixer
            .tracks
            .insert(live.source_ids().track, mixer::BusId::from_raw(999));
        assert!(matches!(live.snapshot(), Err(LiveProjectError::Project(_))));
        assert_eq!(
            lock(&live.published, "published")
                .unwrap()
                .project
                .revisions(),
            valid_revision
        );
    }

    #[test]
    fn compiles_and_renders_the_registered_source_pcm() {
        let live = live();
        let cancellation = RenderCancellation::new();
        let schedule = live
            .compile_audition(&DawEngineConfig::default(), &cancellation)
            .unwrap();
        let rendered = schedule.render_for_audition(&cancellation).unwrap();
        assert_eq!(rendered.audio.frame_count().0, 4);
        assert_eq!(
            rendered.audio.interleaved(),
            &[0.25, 0.25, 0.5, 0.5, 0.75, 0.75, 1.0, 1.0]
        );
    }

    #[test]
    fn audition_preserves_zero_based_leading_silence() {
        let live = live();
        let ids = live.source_ids();
        live.domains()
            .arrangement
            .lock()
            .unwrap()
            .move_clip(ids.clip, ids.track, Frame::new(8))
            .unwrap();
        let cancellation = RenderCancellation::new();
        let schedule = live
            .compile_audition(&DawEngineConfig::default(), &cancellation)
            .unwrap();
        let rendered = schedule.render_for_audition(&cancellation).unwrap();
        assert_eq!(rendered.origin_frame, 0);
        assert_eq!(rendered.audio.frame_count().0, 12);
        assert!(rendered.audio.interleaved()[..16]
            .iter()
            .all(|sample| *sample == 0.0));
        assert_eq!(
            &rendered.audio.interleaved()[16..],
            &[0.25, 0.25, 0.5, 0.5, 0.75, 0.75, 1.0, 1.0]
        );
    }

    #[test]
    fn rejects_pcm_that_disagrees_with_registry_metadata() {
        let (registry, asset, _) = source();
        let wrong =
            PcmAsset::new(AudioFormat::new(44_100, 1).unwrap(), Arc::from([0.0; 4])).unwrap();
        assert!(matches!(
            LiveProject::from_source_material(
                SourceMaterialMetadata::new("Song", "Source"),
                registry,
                asset,
                wrong,
            ),
            Err(LiveProjectError::PcmMetadataMismatch { .. })
        ));
    }
}
