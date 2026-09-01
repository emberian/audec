//! Shared, editable runtime ownership for one audible DAW project.
//!
//! The domain models deliberately use independent identity spaces and are
//! useful to different editors.  [`LiveProject`] keeps those models behind
//! individually shareable compatibility locks, while [`ProjectController`]
//! owns the validated [`DawProject`] aggregate and publishes every durable
//! edit through [`CommandEnvelope`]. Reconciliation remains only for legacy
//! callers that have not entered command ownership; controller snapshots,
//! render, history, and persistence all read the aggregate publication.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::arrangement::{self, ArrangementEditor, Frame, FrameRange, SourceRange, TrackKind};
use crate::assets::{self, AssetRegistry, AssetUsageOwner};
use crate::automation::AutomationGraph;
use crate::change_set::ChangeSet;
use crate::command::{
    AppliedEnvelope, CommandBatch, CommandEnvelope, DomainCommand, EnvelopeError,
};
use crate::command_journal::{
    encode_runtime_records, CommandJournalRecord, CommandOperation, JournalFrameError,
    RuntimeCommandCodec, RuntimeJournalEncodeError,
};
use crate::daw_engine::{
    compile_daw_engine, AssetPcmMap, DawEngineConfig, DawEngineError, DawEngineSchedule,
};
use crate::daw_project::{
    BridgeError, DawProject, ProjectBindings, ProjectDomain, ProjectRevisions,
};
use crate::daw_render::{PcmAsset, RenderCancellation, RenderWindow};
use crate::mixer::{self, BusKind, MixerGraph};
use crate::sample_kit::SampleTargetRef;
use crate::sample_material::{
    canonical_pcm_eq, canonical_pcm_identity, extract_virtual_slice, DecodedPcmView,
    SourceMaterialRef,
};
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
    pub sample_pcm: Arc<Mutex<BTreeMap<SampleTargetRef, PcmAsset>>>,
}

/// A coherent, validated render input.  Both members were cloned while every
/// editable domain lock was held, so later editor changes cannot affect it.
#[derive(Clone, Debug)]
pub struct LiveProjectSnapshot {
    pub project: Arc<DawProject>,
    pub pcm: Arc<AssetPcmMap>,
    pub sample_pcm: Arc<BTreeMap<SampleTargetRef, PcmAsset>>,
}

#[derive(Clone, Debug)]
pub struct LiveProjectApplied {
    pub applied: AppliedEnvelope,
    pub snapshot: LiveProjectSnapshot,
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
    command_owned: BTreeSet<ProjectDomain>,
}

/// Mutation authority for a domain while the legacy editors are retired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectDomainOwnership {
    /// Compatibility editor locks may still be reconciled into the aggregate.
    LegacyMirror,
    /// Only aggregate commands mutate truth; the lock is a read mirror.
    CommandOwned,
}

const ALL_PROJECT_DOMAINS: [ProjectDomain; 8] = [
    ProjectDomain::Arrangement,
    ProjectDomain::Sequencer,
    ProjectDomain::Automation,
    ProjectDomain::Assets,
    ProjectDomain::Mixer,
    ProjectDomain::SampleKits,
    ProjectDomain::Air,
    ProjectDomain::Bindings,
];

/// Runtime project controller shared by the workspace and its editors.
#[derive(Clone, Debug)]
pub struct LiveProject {
    domains: LiveProjectDomains,
    source: Option<SourceMaterialIds>,
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
            sample_pcm: Arc::new(Mutex::new(BTreeMap::new())),
        };

        Ok(Self {
            domains,
            source: Some(source_ids.expect("source IDs are assigned by the committed transaction")),
            published: Arc::new(Mutex::new(PublishedState {
                project,
                command_owned: BTreeSet::new(),
            })),
        })
    }

    pub fn domains(&self) -> LiveProjectDomains {
        self.domains.clone()
    }

    /// Compatibility accessor for the one-source import path.
    pub fn source_ids(&self) -> SourceMaterialIds {
        self.source
            .expect("this project was not created by from_source_material")
    }

    pub const fn primary_source_ids(&self) -> Option<SourceMaterialIds> {
        self.source
    }

    /// Hydrate an already validated aggregate and its resolved runtime media.
    /// Persistence remains responsible for resolving missing media; this
    /// constructor only verifies PCM which was actually supplied.
    pub fn from_project(project: DawProject, pcm: AssetPcmMap) -> Result<Self, LiveProjectError> {
        project.require_valid()?;
        validate_supplied_pcm(&project.state().domains.assets, &pcm)?;
        let sample_pcm = materialize_resolved_samples(&project, &pcm)?;
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
            pcm: Arc::new(Mutex::new(pcm)),
            sample_pcm: Arc::new(Mutex::new(sample_pcm)),
        };
        Ok(Self {
            domains,
            source: None,
            published: Arc::new(Mutex::new(PublishedState {
                project,
                command_owned: BTreeSet::new(),
            })),
        })
    }

    /// Freeze the authoritative aggregate. Domains which have not yet moved
    /// to command ownership retain the compatibility reconciliation path;
    /// command-owned mirrors can never overwrite aggregate truth.
    pub fn snapshot(&self) -> Result<LiveProjectSnapshot, LiveProjectError> {
        let held = self.lock_domains()?;
        validate_supplied_pcm(&held.assets, &held.pcm)?;
        let mut published = lock(&self.published, "published project")?;
        let command_owned = published.command_owned.clone();
        reconcile_legacy(&mut published.project, &held, &command_owned)?;
        Ok(LiveProjectSnapshot {
            project: Arc::new(published.project.clone()),
            pcm: Arc::new(held.pcm.clone()),
            sample_pcm: Arc::new(held.sample_pcm.clone()),
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
        let command_owned = published.command_owned.clone();
        reconcile_legacy(&mut published.project, &held, &command_owned)?;
        published.project.mark_saved();
        Ok(published.project.revisions())
    }

    pub fn mark_saved_if_revision(&self, revision: u64) -> Result<bool, LiveProjectError> {
        let mut published = lock(&self.published, "published project")?;
        Ok(published.project.mark_saved_if_revision(revision))
    }

    pub fn ownership(
        &self,
        domain: ProjectDomain,
    ) -> Result<ProjectDomainOwnership, LiveProjectError> {
        let published = lock(&self.published, "published project")?;
        Ok(if published.command_owned.contains(&domain) {
            ProjectDomainOwnership::CommandOwned
        } else {
            ProjectDomainOwnership::LegacyMirror
        })
    }

    /// Move domains to command authority after first reconciling their current
    /// compatibility mirrors. This transition is one-way for a live session.
    pub fn assume_command_ownership(
        &self,
        domains: impl IntoIterator<Item = ProjectDomain>,
    ) -> Result<LiveProjectSnapshot, LiveProjectError> {
        let mut held = self.lock_domains()?;
        validate_supplied_pcm(&held.assets, &held.pcm)?;
        let mut published = lock(&self.published, "published project")?;
        let previously_owned = published.command_owned.clone();
        reconcile_legacy(&mut published.project, &held, &previously_owned)?;
        published.command_owned.extend(domains);
        sync_command_mirrors(&published.project, &mut held, &published.command_owned)?;
        Ok(LiveProjectSnapshot {
            project: Arc::new(published.project.clone()),
            pcm: Arc::new(held.pcm.clone()),
            sample_pcm: Arc::new(held.sample_pcm.clone()),
        })
    }

    /// Apply one aggregate envelope and update compatibility mirrors only
    /// after the validated aggregate commit succeeds.
    pub fn apply_envelope(
        &self,
        envelope: CommandEnvelope,
    ) -> Result<LiveProjectApplied, LiveProjectError> {
        self.apply_envelope_with_sample_pcm(envelope, BTreeMap::new())
    }

    /// Publish a durable edit and its exact sampler material as one coherent
    /// revision. A preview aggregate and prospective PCM map are fully
    /// validated before the authoritative aggregate is committed.
    pub fn apply_envelope_with_sample_pcm(
        &self,
        envelope: CommandEnvelope,
        sample_pcm_patch: BTreeMap<SampleTargetRef, Option<PcmAsset>>,
    ) -> Result<LiveProjectApplied, LiveProjectError> {
        let mut held = self.lock_domains()?;
        validate_supplied_pcm(&held.assets, &held.pcm)?;
        let mut published = lock(&self.published, "published project")?;
        let command_owned = published.command_owned.clone();
        reconcile_legacy(&mut published.project, &held, &command_owned)?;
        let touched = envelope.touched_domains();
        let mut preview = published.project.clone();
        envelope
            .clone()
            .apply(&mut preview)
            .map_err(LiveProjectError::Envelope)?;
        let supplied_sample_pcm = sample_pcm_patch
            .into_iter()
            .filter_map(|(target, pcm)| pcm.map(|pcm| (target, pcm)))
            .collect::<BTreeMap<_, _>>();
        // Supplied PCM is a revision-pinned publication aid, not independent
        // project truth. Prove it against durable zone provenance, then
        // rebuild the complete runtime cohort so journal replay (which stores
        // only commands) is audibly equivalent to direct execution.
        validate_sample_pcm(&preview, &held.pcm, &supplied_sample_pcm)?;
        let next_sample_pcm = if touched.contains(&ProjectDomain::SampleKits)
            || touched.contains(&ProjectDomain::Assets)
        {
            materialize_resolved_samples(&preview, &held.pcm)?
        } else {
            held.sample_pcm.clone()
        };
        let applied = envelope
            .apply(&mut published.project)
            .map_err(LiveProjectError::Envelope)?;
        *held.sample_pcm = next_sample_pcm;
        sync_command_mirrors(&published.project, &mut held, &applied.change_set.domains)?;
        #[cfg(debug_assertions)]
        {
            let mismatches = mirror_mismatches(&published.project, &held, &published.command_owned);
            debug_assert!(
                mismatches.is_empty(),
                "command application left compatibility mirrors divergent: {mismatches:?}"
            );
        }
        let snapshot = LiveProjectSnapshot {
            project: Arc::new(published.project.clone()),
            pcm: Arc::new(held.pcm.clone()),
            sample_pcm: Arc::new(held.sample_pcm.clone()),
        };
        Ok(LiveProjectApplied { applied, snapshot })
    }

    /// Deep comparison survives as an explicit integrity diagnostic. It is
    /// not called by the normal snapshot path to decide project truth.
    pub fn debug_assert_state_consistent(&self) -> Result<(), LiveProjectError> {
        let held = self.lock_domains()?;
        let published = lock(&self.published, "published project")?;
        let mismatches = mirror_mismatches(&published.project, &held, &published.command_owned);
        if mismatches.is_empty() {
            Ok(())
        } else {
            Err(LiveProjectError::MirrorDiverged(mismatches))
        }
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
            sample_pcm: lock(&self.domains.sample_pcm, "sample PCM")?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectControllerConfig {
    pub history_limit: usize,
    /// Applied-command count, not wall time. Zero disables cross-call merges.
    pub coalesce_window: u64,
}

impl Default for ProjectControllerConfig {
    fn default() -> Self {
        Self {
            history_limit: 256,
            coalesce_window: 32,
        }
    }
}

#[derive(Clone, Debug)]
struct AggregateHistoryEntry {
    forward: CommandBatch,
    inverse: CommandBatch,
    change_set: ChangeSet,
    gesture_epoch: u64,
    last_sequence: u64,
    addresses: BTreeSet<crate::command_record::CommandAddress>,
    sample_pcm_before: BTreeMap<SampleTargetRef, Option<PcmAsset>>,
    sample_pcm_after: BTreeMap<SampleTargetRef, Option<PcmAsset>>,
}

#[derive(Clone, Debug)]
pub struct ProjectControllerUpdate {
    pub operation: CommandOperation,
    pub snapshot: LiveProjectSnapshot,
    pub change_set: ChangeSet,
    pub journal_sequence: u64,
    pub applied: AppliedEnvelope,
}

/// Deterministic controller-issued gesture boundary. Envelopes executed with
/// this handle receive the same coalescing token and may merge into one undo
/// entry. A handle becomes stale as soon as its gesture is ended or another
/// gesture begins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectGesture {
    epoch: u64,
    pub coalesce: crate::command_record::CoalesceToken,
}

impl ProjectGesture {
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }
}

/// The durable command prefix already covered by a checkpoint or acknowledged
/// journal segment. Project dirty state is deliberately separate: autosave
/// acknowledgement never marks a project as explicitly saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectJournalCheckpoint {
    pub through_sequence: u64,
    pub project_revision: u64,
}

/// Immutable, contiguous command records eligible for background autosave.
/// A caller persists these records, then acknowledges this exact value; edits
/// that arrive while I/O runs remain pending in the next delta.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectJournalDelta {
    pub checkpoint: ProjectJournalCheckpoint,
    pub through_sequence: u64,
    pub resulting_revision: u64,
    pub records: Vec<CommandJournalRecord>,
}

impl ProjectJournalDelta {
    /// Encode exactly this captured suffix for `write_journal_segment`. The
    /// controller remains codec-independent; persistence chooses the codec at
    /// the edge and acknowledges only after the returned bytes are durable.
    pub fn encode<C: RuntimeCommandCodec>(
        &self,
        codec: &C,
    ) -> Result<Vec<u8>, RuntimeJournalEncodeError<C::Error>> {
        encode_runtime_records(&self.records, codec)
    }
}

impl ProjectControllerUpdate {
    pub fn revisions(&self) -> ProjectRevisions {
        self.snapshot.revisions()
    }
}

/// UI-independent owner of the authoritative aggregate, aggregate history,
/// journal sequence, and immutable read publication.
pub struct ProjectController {
    live: LiveProject,
    published: LiveProjectSnapshot,
    undo: VecDeque<AggregateHistoryEntry>,
    redo: Vec<AggregateHistoryEntry>,
    journal: Vec<CommandJournalRecord>,
    next_journal_sequence: u64,
    journal_checkpoint: ProjectJournalCheckpoint,
    gesture_epoch: u64,
    config: ProjectControllerConfig,
}

impl ProjectController {
    pub fn new(live: LiveProject) -> Result<Self, ProjectControllerError> {
        Self::with_config(live, ProjectControllerConfig::default())
    }

    pub fn with_config(
        live: LiveProject,
        config: ProjectControllerConfig,
    ) -> Result<Self, ProjectControllerError> {
        if config.history_limit == 0 {
            return Err(ProjectControllerError::InvalidHistoryLimit);
        }
        let published = live
            .assume_command_ownership(ALL_PROJECT_DOMAINS)
            .map_err(ProjectControllerError::Project)?;
        let checkpoint_revision = published.revisions().aggregate;
        Ok(Self {
            live,
            published,
            undo: VecDeque::new(),
            redo: Vec::new(),
            journal: Vec::new(),
            next_journal_sequence: 1,
            journal_checkpoint: ProjectJournalCheckpoint {
                through_sequence: 0,
                project_revision: checkpoint_revision,
            },
            gesture_epoch: 1,
            config,
        })
    }

    /// Seed replay above a checkpoint whose earlier journal prefix has
    /// already been compacted. Must be called before any command is applied.
    pub fn begin_journal_replay(
        &mut self,
        next_sequence: u64,
    ) -> Result<(), ProjectControllerError> {
        if next_sequence == 0 {
            return Err(ProjectControllerError::JournalSequence {
                expected: 1,
                actual: 0,
            });
        }
        if !self.journal.is_empty() || !self.undo.is_empty() || !self.redo.is_empty() {
            return Err(ProjectControllerError::RecoveryAlreadyStarted);
        }
        self.next_journal_sequence = next_sequence;
        self.journal_checkpoint = ProjectJournalCheckpoint {
            through_sequence: next_sequence - 1,
            project_revision: self.revisions().aggregate,
        };
        self.commit_gesture();
        Ok(())
    }

    pub fn live_project(&self) -> &LiveProject {
        &self.live
    }

    pub fn snapshot(&self) -> &LiveProjectSnapshot {
        &self.published
    }

    pub fn revisions(&self) -> ProjectRevisions {
        self.published.revisions()
    }

    pub fn is_dirty(&self) -> bool {
        self.published.is_dirty()
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo_label(&self) -> Option<&str> {
        self.undo.back().map(|entry| entry.forward.label.as_str())
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|entry| entry.forward.label.as_str())
    }

    pub fn journal_records(&self) -> &[CommandJournalRecord] {
        &self.journal
    }

    pub fn journal_records_from(&self, sequence: u64) -> &[CommandJournalRecord] {
        let index = self
            .journal
            .partition_point(|record| record.sequence < sequence);
        &self.journal[index..]
    }

    pub const fn journal_checkpoint(&self) -> ProjectJournalCheckpoint {
        self.journal_checkpoint
    }

    /// Capture a contiguous journal suffix for a background autosave. The
    /// returned delta owns its records and does not borrow the controller.
    pub fn pending_journal_delta(&self) -> Option<ProjectJournalDelta> {
        let records =
            self.journal_records_from(self.journal_checkpoint.through_sequence.saturating_add(1));
        let first = records.first()?;
        let last = records
            .last()
            .expect("non-empty journal suffix has a last record");
        debug_assert_eq!(
            first.base_revision,
            self.journal_checkpoint.project_revision
        );
        Some(ProjectJournalDelta {
            checkpoint: self.journal_checkpoint,
            through_sequence: last.sequence,
            resulting_revision: last.resulting_revision,
            records: records.to_vec(),
        })
    }

    /// Advance the durable journal cursor only after the exact captured delta
    /// has been persisted. Newer records are intentionally left pending.
    pub fn acknowledge_journal_delta(
        &mut self,
        delta: &ProjectJournalDelta,
    ) -> Result<(), ProjectControllerError> {
        if delta.checkpoint != self.journal_checkpoint {
            return Err(ProjectControllerError::JournalCheckpoint {
                expected: self.journal_checkpoint,
                actual: delta.checkpoint,
            });
        }
        let current =
            self.journal_records_from(delta.checkpoint.through_sequence.saturating_add(1));
        if delta.records.is_empty()
            || current.len() < delta.records.len()
            || current[..delta.records.len()] != delta.records
            || delta.records.last().is_none_or(|record| {
                record.sequence != delta.through_sequence
                    || record.resulting_revision != delta.resulting_revision
            })
        {
            return Err(ProjectControllerError::JournalDeltaMismatch);
        }
        self.journal_checkpoint = ProjectJournalCheckpoint {
            through_sequence: delta.through_sequence,
            project_revision: delta.resulting_revision,
        };
        Ok(())
    }

    /// Begin a coalescible gesture at a hard history boundary.
    pub fn begin_gesture(
        &mut self,
        coalesce: crate::command_record::CoalesceToken,
    ) -> ProjectGesture {
        self.commit_gesture();
        ProjectGesture {
            epoch: self.gesture_epoch,
            coalesce,
        }
    }

    pub fn execute_in_gesture(
        &mut self,
        gesture: &ProjectGesture,
        mut envelope: CommandEnvelope,
    ) -> Result<ProjectControllerUpdate, ProjectControllerError> {
        self.require_current_gesture(gesture)?;
        envelope.coalesce = Some(gesture.coalesce.clone());
        self.execute(envelope)
    }

    pub fn end_gesture(&mut self, gesture: &ProjectGesture) -> Result<(), ProjectControllerError> {
        self.require_current_gesture(gesture)?;
        self.commit_gesture();
        Ok(())
    }

    fn require_current_gesture(
        &self,
        gesture: &ProjectGesture,
    ) -> Result<(), ProjectControllerError> {
        if gesture.epoch != self.gesture_epoch {
            return Err(ProjectControllerError::StaleGesture {
                expected_epoch: self.gesture_epoch,
                actual_epoch: gesture.epoch,
            });
        }
        Ok(())
    }

    pub fn execute(
        &mut self,
        envelope: CommandEnvelope,
    ) -> Result<ProjectControllerUpdate, ProjectControllerError> {
        self.execute_with_sample_pcm(envelope, BTreeMap::new())
    }

    /// Execute a durable envelope with exact sampler PCM which becomes
    /// visible only at the envelope's resulting aggregate revision.
    pub fn execute_with_sample_pcm(
        &mut self,
        envelope: CommandEnvelope,
        sample_pcm: BTreeMap<SampleTargetRef, PcmAsset>,
    ) -> Result<ProjectControllerUpdate, ProjectControllerError> {
        self.execute_with_sample_pcm_patch(
            envelope,
            sample_pcm
                .into_iter()
                .map(|(target, pcm)| (target, Some(pcm)))
                .collect(),
        )
    }

    /// Controller-internal form which also supports removing runtime material.
    /// The before/after PCM cohort is retained with aggregate history so a
    /// zone deletion remains exactly one undoable operation.
    pub(crate) fn execute_with_sample_pcm_patch(
        &mut self,
        envelope: CommandEnvelope,
        sample_pcm_after: BTreeMap<SampleTargetRef, Option<PcmAsset>>,
    ) -> Result<ProjectControllerUpdate, ProjectControllerError> {
        let batch = envelope.as_batch();
        let base_revision = envelope.base_revision;
        let sample_pcm_before = sample_pcm_after
            .keys()
            .map(|target| (*target, self.published.sample_pcm.get(target).cloned()))
            .collect::<BTreeMap<_, _>>();
        let update = self.apply_and_record_with_pcm(
            envelope,
            CommandOperation::Execute,
            sample_pcm_after.clone(),
        )?;
        // Use the inverse produced by the exact applied command path, including
        // synthesized recreation claims, rather than rebuilding state here.
        let applied_record = self
            .journal
            .last()
            .expect("successful apply always appends a journal record");
        debug_assert_eq!(applied_record.base_revision, base_revision);
        let inverse_batch = update.applied.inverse.clone().into_batch();
        let entry = AggregateHistoryEntry {
            addresses: batch_addresses(&batch),
            forward: batch,
            inverse: inverse_batch,
            change_set: update.change_set.clone(),
            gesture_epoch: self.gesture_epoch,
            last_sequence: update.journal_sequence,
            sample_pcm_before,
            sample_pcm_after,
        };
        self.push_or_coalesce(entry);
        self.redo.clear();
        Ok(update)
    }

    pub fn execute_batch(
        &mut self,
        attempt: crate::command_record::CommandAttempt<DomainCommand>,
    ) -> Result<ProjectControllerUpdate, ProjectControllerError> {
        self.execute(CommandEnvelope::from_batch(
            attempt.base_revision,
            attempt.batch,
        ))
    }

    pub fn undo(&mut self) -> Result<Option<ProjectControllerUpdate>, ProjectControllerError> {
        let Some(mut entry) = self.undo.pop_back() else {
            return Ok(None);
        };
        let envelope =
            CommandEnvelope::from_batch(self.revisions().aggregate, entry.inverse.clone());
        match self.apply_and_record_with_pcm(
            envelope,
            CommandOperation::Undo,
            entry.sample_pcm_before.clone(),
        ) {
            Ok(update) => {
                // Stateful domain commands (notably MixerCommand) advance
                // their own revision while reverting. The exact inverse of
                // the command that just applied is therefore the only valid
                // redo command; the originally stored forward command still
                // carries its pre-undo internal revision.
                entry.forward = update.applied.inverse.clone().into_batch();
                self.redo.push(entry);
                self.commit_gesture();
                Ok(Some(update))
            }
            Err(error) => {
                self.undo.push_back(entry);
                Err(error)
            }
        }
    }

    pub fn redo(&mut self) -> Result<Option<ProjectControllerUpdate>, ProjectControllerError> {
        let Some(mut entry) = self.redo.pop() else {
            return Ok(None);
        };
        let envelope =
            CommandEnvelope::from_batch(self.revisions().aggregate, entry.forward.clone());
        match self.apply_and_record_with_pcm(
            envelope,
            CommandOperation::Redo,
            entry.sample_pcm_after.clone(),
        ) {
            Ok(update) => {
                // Refresh the next undo for the same reason as above. This
                // also makes repeated undo/redo cycles rebase every domain's
                // optimistic preconditions, not just the aggregate token.
                entry.inverse = update.applied.inverse.clone().into_batch();
                self.undo.push_back(entry);
                self.commit_gesture();
                Ok(Some(update))
            }
            Err(error) => {
                self.redo.push(entry);
                Err(error)
            }
        }
    }

    /// Ends the current deterministic coalescing session.
    pub fn commit_gesture(&mut self) {
        self.gesture_epoch = self.gesture_epoch.wrapping_add(1).max(1);
    }

    pub fn mark_saved_if_revision(
        &mut self,
        revision: u64,
    ) -> Result<bool, ProjectControllerError> {
        let marked = self
            .live
            .mark_saved_if_revision(revision)
            .map_err(ProjectControllerError::Project)?;
        if marked {
            self.published = self
                .live
                .snapshot()
                .map_err(ProjectControllerError::Project)?;
            self.journal_checkpoint = ProjectJournalCheckpoint {
                through_sequence: self
                    .journal
                    .last()
                    .map_or(self.journal_checkpoint.through_sequence, |record| {
                        record.sequence
                    }),
                project_revision: revision,
            };
        }
        Ok(marked)
    }

    /// Apply a verified recovery record without manufacturing undo history.
    /// A checkpoint may omit earlier history, so recovered sessions begin at
    /// a deliberate history boundary.
    pub fn replay_record(
        &mut self,
        record: &CommandJournalRecord,
    ) -> Result<ProjectControllerUpdate, ProjectControllerError> {
        record.validate().map_err(ProjectControllerError::Journal)?;
        if record.sequence != self.next_journal_sequence {
            return Err(ProjectControllerError::JournalSequence {
                expected: self.next_journal_sequence,
                actual: record.sequence,
            });
        }
        if record.base_revision != self.revisions().aggregate {
            return Err(ProjectControllerError::ReplayRevision {
                expected: self.revisions().aggregate,
                actual: record.base_revision,
            });
        }
        let next_sequence = record
            .sequence
            .checked_add(1)
            .ok_or(ProjectControllerError::JournalSequenceExhausted)?;
        let applied = self
            .live
            .apply_envelope(CommandEnvelope::from_batch(
                record.base_revision,
                record.batch.clone(),
            ))
            .map_err(ProjectControllerError::Project)?;
        if applied.snapshot.revisions().aggregate != record.resulting_revision {
            return Err(ProjectControllerError::ReplayRevision {
                expected: record.resulting_revision,
                actual: applied.snapshot.revisions().aggregate,
            });
        }
        self.published = applied.snapshot.clone();
        self.journal.push(record.clone());
        self.next_journal_sequence = next_sequence;
        self.journal_checkpoint = ProjectJournalCheckpoint {
            through_sequence: record.sequence,
            project_revision: record.resulting_revision,
        };
        self.undo.clear();
        self.redo.clear();
        Ok(ProjectControllerUpdate {
            operation: record.operation,
            snapshot: applied.snapshot,
            change_set: applied.applied.change_set.clone(),
            journal_sequence: record.sequence,
            applied: applied.applied,
        })
    }

    fn apply_and_record_with_pcm(
        &mut self,
        envelope: CommandEnvelope,
        operation: CommandOperation,
        sample_pcm_patch: BTreeMap<SampleTargetRef, Option<PcmAsset>>,
    ) -> Result<ProjectControllerUpdate, ProjectControllerError> {
        let sequence = self.next_journal_sequence;
        let next_sequence = sequence
            .checked_add(1)
            .ok_or(ProjectControllerError::JournalSequenceExhausted)?;
        let base_revision = envelope.base_revision;
        let batch = envelope.as_batch();
        let applied = self
            .live
            .apply_envelope_with_sample_pcm(envelope, sample_pcm_patch)
            .map_err(ProjectControllerError::Project)?;
        let resulting_revision = applied.snapshot.revisions().aggregate;
        let record = CommandJournalRecord::new(
            sequence,
            base_revision,
            resulting_revision,
            operation,
            batch,
        )
        .map_err(ProjectControllerError::Journal)?;
        self.journal.push(record);
        self.next_journal_sequence = next_sequence;
        self.published = applied.snapshot.clone();
        Ok(ProjectControllerUpdate {
            operation,
            snapshot: applied.snapshot,
            change_set: applied.applied.change_set.clone(),
            journal_sequence: sequence,
            applied: applied.applied,
        })
    }

    fn push_or_coalesce(&mut self, mut incoming: AggregateHistoryEntry) {
        let can_merge = self.undo.back().is_some_and(|previous| {
            self.config.coalesce_window > 0
                && previous.gesture_epoch == incoming.gesture_epoch
                && incoming
                    .last_sequence
                    .saturating_sub(previous.last_sequence)
                    <= self.config.coalesce_window
                && previous.forward.coalesce.is_some()
                && previous.forward.coalesce == incoming.forward.coalesce
                && previous.addresses == incoming.addresses
                && previous.sample_pcm_before.is_empty()
                && incoming.sample_pcm_before.is_empty()
                && previous
                    .forward
                    .coalesce
                    .as_ref()
                    .is_some_and(|token| previous.addresses.contains(&token.primary))
        });
        let composed = can_merge
            .then(|| {
                let previous = self.undo.back().expect("checked above");
                (previous.forward.commands.len() == incoming.forward.commands.len())
                    .then(|| {
                        previous
                            .forward
                            .commands
                            .iter()
                            .zip(&incoming.forward.commands)
                            .map(|(previous, incoming)| previous.compose(incoming))
                            .collect::<Option<Vec<_>>>()
                    })
                    .flatten()
            })
            .flatten();
        if let Some(mut commands) = composed {
            commands.retain(|command| !command.is_noop());
            if commands.is_empty() {
                self.undo.pop_back();
                return;
            }
            let previous = self.undo.back_mut().expect("checked above");
            previous.forward.commands = commands;
            previous
                .forward
                .id_claims
                .append(&mut incoming.forward.id_claims);
            incoming
                .inverse
                .id_claims
                .append(&mut previous.inverse.id_claims);
            previous.inverse = CommandBatch {
                label: format!("Undo {}", previous.forward.label),
                coalesce: None,
                commands: previous
                    .forward
                    .commands
                    .iter()
                    .rev()
                    .map(DomainCommand::inverse)
                    .collect(),
                id_claims: incoming.inverse.id_claims,
            };
            previous.change_set.merge(&incoming.change_set);
            previous.last_sequence = incoming.last_sequence;
            return;
        }
        self.undo.push_back(incoming);
        while self.undo.len() > self.config.history_limit {
            self.undo.pop_front();
        }
    }
}

fn batch_addresses(batch: &CommandBatch) -> BTreeSet<crate::command_record::CommandAddress> {
    batch
        .commands
        .iter()
        .flat_map(DomainCommand::addresses)
        .collect()
}

#[derive(Debug)]
pub enum ProjectControllerError {
    InvalidHistoryLimit,
    RecoveryAlreadyStarted,
    JournalSequenceExhausted,
    JournalSequence {
        expected: u64,
        actual: u64,
    },
    ReplayRevision {
        expected: u64,
        actual: u64,
    },
    StaleGesture {
        expected_epoch: u64,
        actual_epoch: u64,
    },
    JournalCheckpoint {
        expected: ProjectJournalCheckpoint,
        actual: ProjectJournalCheckpoint,
    },
    JournalDeltaMismatch,
    Journal(JournalFrameError),
    Project(LiveProjectError),
}

impl fmt::Display for ProjectControllerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHistoryLimit => {
                formatter.write_str("project history limit must be non-zero")
            }
            Self::RecoveryAlreadyStarted => {
                formatter.write_str("journal replay must be seeded before commands are applied")
            }
            Self::JournalSequenceExhausted => {
                formatter.write_str("project journal sequence exhausted")
            }
            Self::JournalSequence { expected, actual } => write!(
                formatter,
                "project journal sequence conflict: expected {expected}, actual {actual}"
            ),
            Self::ReplayRevision { expected, actual } => write!(
                formatter,
                "project journal revision conflict: expected {expected}, actual {actual}"
            ),
            Self::StaleGesture {
                expected_epoch,
                actual_epoch,
            } => write!(
                formatter,
                "project gesture is stale: expected epoch {expected_epoch}, actual {actual_epoch}"
            ),
            Self::JournalCheckpoint { expected, actual } => write!(
                formatter,
                "project journal checkpoint conflict: expected sequence {} at revision {}, actual sequence {} at revision {}",
                expected.through_sequence,
                expected.project_revision,
                actual.through_sequence,
                actual.project_revision
            ),
            Self::JournalDeltaMismatch => {
                formatter.write_str("project journal delta does not match the controller log")
            }
            Self::Journal(error) => error.fmt(formatter),
            Self::Project(error) => error.fmt(formatter),
        }
    }
}

impl Error for ProjectControllerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Journal(error) => Some(error),
            Self::Project(error) => Some(error),
            _ => None,
        }
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
    sample_pcm: MutexGuard<'a, BTreeMap<SampleTargetRef, PcmAsset>>,
}

fn reconcile_legacy(
    project: &mut DawProject,
    held: &HeldDomains<'_>,
    command_owned: &BTreeSet<ProjectDomain>,
) -> Result<(), LiveProjectError> {
    let current = project.state();
    let arrangement = held.arrangement.state();
    let mut touched = BTreeSet::new();
    if !command_owned.contains(&ProjectDomain::Arrangement)
        && &current.domains.arrangement != arrangement
    {
        touched.insert(ProjectDomain::Arrangement);
    }
    if !command_owned.contains(&ProjectDomain::Sequencer)
        && !sequencers_equal(&current.domains.sequencer, &held.sequencer)
    {
        touched.insert(ProjectDomain::Sequencer);
    }
    if !command_owned.contains(&ProjectDomain::Automation)
        && current.domains.automation != *held.automation
    {
        touched.insert(ProjectDomain::Automation);
    }
    if !command_owned.contains(&ProjectDomain::Assets) && current.domains.assets != *held.assets {
        touched.insert(ProjectDomain::Assets);
    }
    if !command_owned.contains(&ProjectDomain::Mixer) && current.domains.mixer != *held.mixer {
        touched.insert(ProjectDomain::Mixer);
    }
    if !command_owned.contains(&ProjectDomain::Bindings) && current.bindings != *held.bindings {
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

fn sync_command_mirrors(
    project: &DawProject,
    held: &mut HeldDomains<'_>,
    domains: &BTreeSet<ProjectDomain>,
) -> Result<(), LiveProjectError> {
    let state = project.state();
    if domains.contains(&ProjectDomain::Arrangement) {
        *held.arrangement = ArrangementEditor::from_state(state.domains.arrangement.clone())
            .map_err(|error| LiveProjectError::Domain(error.to_string()))?;
    }
    if domains.contains(&ProjectDomain::Sequencer) {
        *held.sequencer = state.domains.sequencer.clone();
    }
    if domains.contains(&ProjectDomain::Automation) {
        *held.automation = state.domains.automation.clone();
    }
    if domains.contains(&ProjectDomain::Assets) {
        *held.assets = state.domains.assets.clone();
    }
    if domains.contains(&ProjectDomain::Mixer) {
        *held.mixer = state.domains.mixer.clone();
    }
    if domains.contains(&ProjectDomain::Bindings) {
        *held.bindings = state.bindings.clone();
    }
    Ok(())
}

fn mirror_mismatches(
    project: &DawProject,
    held: &HeldDomains<'_>,
    domains: &BTreeSet<ProjectDomain>,
) -> BTreeSet<ProjectDomain> {
    let state = project.state();
    let mut mismatches = BTreeSet::new();
    if domains.contains(&ProjectDomain::Arrangement)
        && &state.domains.arrangement != held.arrangement.state()
    {
        mismatches.insert(ProjectDomain::Arrangement);
    }
    if domains.contains(&ProjectDomain::Sequencer)
        && !sequencers_equal(&state.domains.sequencer, &held.sequencer)
    {
        mismatches.insert(ProjectDomain::Sequencer);
    }
    if domains.contains(&ProjectDomain::Automation) && state.domains.automation != *held.automation
    {
        mismatches.insert(ProjectDomain::Automation);
    }
    if domains.contains(&ProjectDomain::Assets) && state.domains.assets != *held.assets {
        mismatches.insert(ProjectDomain::Assets);
    }
    if domains.contains(&ProjectDomain::Mixer) && state.domains.mixer != *held.mixer {
        mismatches.insert(ProjectDomain::Mixer);
    }
    if domains.contains(&ProjectDomain::Bindings) && state.bindings != *held.bindings {
        mismatches.insert(ProjectDomain::Bindings);
    }
    mismatches
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

fn validate_sample_pcm(
    project: &DawProject,
    source_pcm: &AssetPcmMap,
    sample_pcm: &BTreeMap<SampleTargetRef, PcmAsset>,
) -> Result<(), LiveProjectError> {
    for (target, materialized) in sample_pcm {
        let kit = project
            .state()
            .domains
            .sample_kits
            .kits
            .get(&target.kit)
            .ok_or(LiveProjectError::MissingSampleTarget(*target))?;
        let zone = kit
            .zones
            .get(&target.zone)
            .filter(|zone| zone.pad == target.pad)
            .ok_or(LiveProjectError::MissingSampleTarget(*target))?;
        let expected = match zone.material {
            SourceMaterialRef::Asset(asset) => source_pcm
                .get(&asset)
                .ok_or(LiveProjectError::MissingSampleSource(asset))?
                .clone(),
            SourceMaterialRef::VirtualSlice(slice) => {
                let source = source_pcm
                    .get(&slice.source_asset)
                    .ok_or(LiveProjectError::MissingSampleSource(slice.source_asset))?;
                extract_virtual_slice(slice, source)
                    .map_err(|error| LiveProjectError::SampleMaterial(error.to_string()))?
                    .to_pcm_asset()
            }
        };
        let exact = canonical_pcm_eq(
            DecodedPcmView::from_pcm_asset(&expected),
            DecodedPcmView::from_pcm_asset(materialized),
        )
        .map_err(|error| LiveProjectError::SampleMaterial(error.to_string()))?;
        if !exact {
            return Err(LiveProjectError::SamplePcmMismatch(*target));
        }
    }
    Ok(())
}

/// Rebuild sampler runtime material from durable zone provenance and hydrated
/// source assets. Zones whose source media is unresolved remain visible and
/// editable but have no runtime PCM until an explicit relink/hydration pass.
fn materialize_resolved_samples(
    project: &DawProject,
    source_pcm: &AssetPcmMap,
) -> Result<BTreeMap<SampleTargetRef, PcmAsset>, LiveProjectError> {
    let mut materialized = BTreeMap::new();
    for kit in project.state().domains.sample_kits.kits.values() {
        for target in kit.targets() {
            let zone = kit
                .zone_for_target(target)
                .ok_or(LiveProjectError::MissingSampleTarget(target))?;
            let pcm = match zone.material {
                SourceMaterialRef::Asset(asset) => match source_pcm.get(&asset) {
                    Some(pcm) => pcm.clone(),
                    None => continue,
                },
                SourceMaterialRef::VirtualSlice(slice) => {
                    let Some(source) = source_pcm.get(&slice.source_asset) else {
                        continue;
                    };
                    extract_virtual_slice(slice, source)
                        .map_err(|error| LiveProjectError::SampleMaterial(error.to_string()))?
                        .to_pcm_asset()
                }
            };
            if zone.decoded_pcm.is_some_and(|expected| {
                canonical_pcm_identity(DecodedPcmView::from_pcm_asset(&pcm))
                    .map(|actual| actual != expected)
                    .unwrap_or(true)
            }) {
                return Err(LiveProjectError::SamplePcmMismatch(target));
            }
            materialized.insert(target, pcm);
        }
    }
    validate_sample_pcm(project, source_pcm, &materialized)?;
    Ok(materialized)
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
    MissingSampleSource(assets::AssetId),
    MissingSampleTarget(SampleTargetRef),
    SamplePcmMismatch(SampleTargetRef),
    SampleMaterial(String),
    MirrorDiverged(BTreeSet<ProjectDomain>),
    Domain(String),
    Envelope(EnvelopeError),
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
            Self::MissingSampleSource(asset) => {
                write!(formatter, "sample material source asset {} has no decoded PCM", asset.0)
            }
            Self::MissingSampleTarget(target) => write!(
                formatter,
                "sample material target {}/{}/{} is absent",
                target.kit.get(), target.pad.get(), target.zone.get()
            ),
            Self::SamplePcmMismatch(target) => write!(
                formatter,
                "sample material target {}/{}/{} does not match its exact source range",
                target.kit.get(), target.pad.get(), target.zone.get()
            ),
            Self::SampleMaterial(error) => write!(formatter, "sample material failed: {error}"),
            Self::MirrorDiverged(domains) => {
                write!(formatter, "command-owned project mirrors diverged in {domains:?}")
            }
            Self::Domain(error) => formatter.write_str(error),
            Self::Envelope(error) => write!(formatter, "project command failed: {error}"),
            Self::Project(error) => write!(formatter, "live project is invalid: {error}"),
            Self::Engine(error) => write!(formatter, "live project cannot be compiled: {error}"),
        }
    }
}

impl Error for LiveProjectError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Envelope(error) => Some(error),
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
    use crate::command::{ArrangementCommand, CoalesceToken, CommandAddress, DomainCommand};
    use crate::sample_kit::{SampleKit, SampleKitPut, SamplePad, SampleRouteIntent, SampleZone};
    use crate::sample_material::VirtualSliceRef;

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
    fn from_project_rematerializes_resolved_persisted_sample_zones() {
        let live = live();
        let snapshot = live.snapshot().unwrap();
        let asset = live.source_ids().registry_asset;
        let mut project = snapshot.project.as_ref().clone();
        let mut target = None;
        project
            .transact(
                "persist sample zone",
                project.revisions().aggregate,
                BTreeSet::from([ProjectDomain::Mixer, ProjectDomain::SampleKits]),
                |state| -> Result<(), String> {
                    let sample_bus = state
                        .domains
                        .mixer
                        .add_bus(BusKind::Source, "Recovered samples")
                        .map_err(|e| e.to_string())?;
                    let library = &mut state.domains.sample_kits;
                    let kit_id = library.allocate_kit_id().map_err(|e| e.to_string())?;
                    let pad_id = library.allocate_pad_id().map_err(|e| e.to_string())?;
                    let zone_id = library.allocate_zone_id().map_err(|e| e.to_string())?;
                    let mut kit = SampleKit::new(
                        kit_id,
                        "Recovered kit",
                        SampleRouteIntent::new(sample_bus).map_err(|e| e.to_string())?,
                    );
                    let mut pad = SamplePad::new(pad_id, "Slice");
                    pad.zone_order.push(zone_id);
                    kit.pad_order.push(pad_id);
                    kit.pads.insert(pad_id, pad);
                    kit.zones.insert(
                        zone_id,
                        SampleZone::new(
                            zone_id,
                            pad_id,
                            SourceMaterialRef::VirtualSlice(
                                VirtualSliceRef::new(
                                    asset,
                                    assets::AssetFrameRange {
                                        start: assets::SampleFrames(1),
                                        end: assets::SampleFrames(3),
                                    },
                                )
                                .map_err(|e| e.to_string())?,
                            ),
                        ),
                    );
                    library
                        .apply_puts(&[SampleKitPut {
                            before: None,
                            after: Some(kit),
                        }])
                        .map_err(|e| e.to_string())?;
                    target = Some(SampleTargetRef {
                        kit: kit_id,
                        pad: pad_id,
                        zone: zone_id,
                    });
                    Ok(())
                },
            )
            .unwrap();

        let reopened = LiveProject::from_project(project, snapshot.pcm.as_ref().clone()).unwrap();
        let reopened = reopened.snapshot().unwrap();
        assert_eq!(
            reopened.sample_pcm[&target.unwrap()].samples.as_ref(),
            &[0.5, 0.75]
        );
    }

    fn move_clip_envelope(
        controller: &ProjectController,
        start: i64,
        coalesce: Option<CoalesceToken>,
    ) -> CommandEnvelope {
        let id = controller.live_project().source_ids().clip;
        let before = controller
            .snapshot()
            .project
            .state()
            .domains
            .arrangement
            .clip(id)
            .unwrap()
            .clone();
        let mut after = before.clone();
        after.placement =
            FrameRange::from_start_and_len(Frame::new(start), before.placement.len()).unwrap();
        CommandEnvelope {
            label: "Move source".into(),
            base_revision: controller.revisions().aggregate,
            coalesce,
            commands: vec![DomainCommand::Arrangement(ArrangementCommand::PutClip {
                before: Some(before),
                after: Some(after),
            })],
            id_claims: BTreeSet::new(),
        }
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

    #[test]
    fn controller_coalesces_gesture_and_undo_redo_journal_every_application() {
        let mut controller = ProjectController::new(live()).unwrap();
        let initial_revision = controller.revisions().aggregate;
        let clip = controller.live_project().source_ids().clip;
        let token = CoalesceToken {
            editor_session: 7,
            gesture_kind: 1,
            primary: CommandAddress::ArrangementClip(clip),
        };

        controller
            .execute(move_clip_envelope(&controller, 8, Some(token.clone())))
            .unwrap();
        controller
            .execute(move_clip_envelope(&controller, 16, Some(token)))
            .unwrap();
        assert_eq!(controller.revisions().aggregate, initial_revision + 2);
        assert_eq!(controller.journal_records().len(), 2);

        controller.undo().unwrap().unwrap();
        assert_eq!(
            controller
                .snapshot()
                .project
                .state()
                .domains
                .arrangement
                .clip(clip)
                .unwrap()
                .placement
                .start,
            Frame::ZERO
        );
        assert!(!controller.can_undo());
        assert!(controller.can_redo());

        controller.redo().unwrap().unwrap();
        assert_eq!(
            controller
                .snapshot()
                .project
                .state()
                .domains
                .arrangement
                .clip(clip)
                .unwrap()
                .placement
                .start,
            Frame::new(16)
        );
        assert_eq!(controller.journal_records().len(), 4);
        assert_eq!(
            controller
                .journal_records()
                .iter()
                .map(|record| record.operation)
                .collect::<Vec<_>>(),
            vec![
                CommandOperation::Execute,
                CommandOperation::Execute,
                CommandOperation::Undo,
                CommandOperation::Redo,
            ]
        );
    }

    #[test]
    fn issued_gesture_coalesces_and_rejects_late_commits() {
        let mut controller = ProjectController::new(live()).unwrap();
        let initial_revision = controller.revisions().aggregate;
        let clip = controller.live_project().source_ids().clip;
        let gesture = controller.begin_gesture(CoalesceToken {
            editor_session: 17,
            gesture_kind: 4,
            primary: CommandAddress::ArrangementClip(clip),
        });
        let first = move_clip_envelope(&controller, 4, None);
        controller.execute_in_gesture(&gesture, first).unwrap();
        let second = move_clip_envelope(&controller, 12, None);
        controller.execute_in_gesture(&gesture, second).unwrap();
        controller.end_gesture(&gesture).unwrap();

        assert_eq!(controller.revisions().aggregate, initial_revision + 2);
        controller.undo().unwrap().unwrap();
        assert_eq!(
            controller
                .snapshot()
                .project
                .state()
                .domains
                .arrangement
                .clip(clip)
                .unwrap()
                .placement
                .start,
            Frame::ZERO
        );
        let stale_envelope = move_clip_envelope(&controller, 20, None);
        assert!(matches!(
            controller.execute_in_gesture(&gesture, stale_envelope),
            Err(ProjectControllerError::StaleGesture { .. })
        ));
    }

    #[test]
    fn journal_delta_acknowledgement_leaves_raced_edits_pending() {
        let mut controller = ProjectController::new(live()).unwrap();
        let checkpoint = controller.journal_checkpoint();
        assert!(controller.pending_journal_delta().is_none());

        controller
            .execute(move_clip_envelope(&controller, 4, None))
            .unwrap();
        controller
            .execute(move_clip_envelope(&controller, 8, None))
            .unwrap();
        let captured = controller.pending_journal_delta().unwrap();
        assert_eq!(captured.checkpoint, checkpoint);
        assert_eq!(captured.records.len(), 2);
        assert_eq!(captured.through_sequence, 2);

        controller
            .execute(move_clip_envelope(&controller, 12, None))
            .unwrap();
        controller.acknowledge_journal_delta(&captured).unwrap();
        let raced = controller.pending_journal_delta().unwrap();
        assert_eq!(raced.checkpoint.through_sequence, 2);
        assert_eq!(
            raced.checkpoint.project_revision,
            captured.resulting_revision
        );
        assert_eq!(raced.records.len(), 1);
        assert_eq!(raced.records[0].sequence, 3);

        assert!(!controller
            .mark_saved_if_revision(captured.resulting_revision)
            .unwrap());
        assert!(controller.pending_journal_delta().is_some());
        let current = controller.revisions().aggregate;
        assert!(controller.mark_saved_if_revision(current).unwrap());
        assert!(controller.pending_journal_delta().is_none());

        controller.undo().unwrap().unwrap();
        let undo = controller.pending_journal_delta().unwrap();
        assert_eq!(undo.records.len(), 1);
        assert_eq!(undo.records[0].operation, CommandOperation::Undo);
    }

    #[test]
    fn command_owned_mirror_cannot_overwrite_aggregate() {
        let controller = ProjectController::new(live()).unwrap();
        let revision = controller.revisions();
        let ids = controller.live_project().source_ids();
        controller
            .live_project()
            .domains()
            .arrangement
            .lock()
            .unwrap()
            .move_clip(ids.clip, ids.track, Frame::new(24))
            .unwrap();

        let snapshot = controller.live_project().snapshot().unwrap();
        assert_eq!(snapshot.revisions(), revision);
        assert_eq!(
            snapshot
                .project
                .state()
                .domains
                .arrangement
                .clip(ids.clip)
                .unwrap()
                .placement
                .start,
            Frame::ZERO
        );
        assert!(matches!(
            controller.live_project().debug_assert_state_consistent(),
            Err(LiveProjectError::MirrorDiverged(domains))
                if domains.contains(&ProjectDomain::Arrangement)
        ));
    }
}
