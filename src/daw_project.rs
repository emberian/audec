//! Aggregate project boundary for audec's constructive and analytic domains.
//!
//! The DAW cores intentionally own independent ID spaces. An arrangement
//! `AssetId(4)` is not an asset-registry `AssetId(4)`, and neither may be
//! persisted as if it were the other. This module is the explicit anti-
//! corruption layer between those domains: typed maps associate identities,
//! validation follows every cross-domain reference, transactions publish a
//! fully validated candidate atomically, and render compilation produces an
//! immutable control-thread snapshot.
//!
//! This is orchestration, not DSP. In particular, compiling a snapshot does
//! not decode media, instantiate instruments/plugins, stretch audio, or mix a
//! sample. It prepares the exact references, events, automation and latency
//! plan that future realtime/offline graph code must consume.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::arrangement::{
    self, ArrangementEditor, ArrangementState, ClipContent, Frame, FrameRange, TrackKind,
};
use crate::assets::{self, AssetAvailability, AssetRegistry};
use crate::automation::{self, AutomationGraph, BeatFrameMap, ParameterAddress};
use crate::mixer::{self, BusKind, MixerGraph};
use crate::ontology::{self, AuditoryIr};
use crate::project::ProjectDocument;
use crate::sequencer::{self, Sequencer, TempoMap};
use crate::session;

pub const DAW_PROJECT_SCHEMA_VERSION: u32 = 1;

/// Domains whose independent generations contribute to one project revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProjectDomain {
    Arrangement,
    Sequencer,
    Automation,
    Assets,
    Mixer,
    Air,
    Bindings,
}

/// One aggregate revision plus per-domain generations.
///
/// The aggregate revision is the optimistic-concurrency token. Domain
/// generations permit caches to invalidate only the products they consume.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProjectRevisions {
    pub aggregate: u64,
    pub arrangement: u64,
    pub sequencer: u64,
    pub automation: u64,
    pub assets: u64,
    pub mixer: u64,
    pub air: u64,
    pub bindings: u64,
}

impl ProjectRevisions {
    fn advance(&mut self, touched: &BTreeSet<ProjectDomain>) -> Result<(), BridgeError> {
        self.aggregate = checked_revision(self.aggregate)?;
        for domain in touched {
            let revision = match domain {
                ProjectDomain::Arrangement => &mut self.arrangement,
                ProjectDomain::Sequencer => &mut self.sequencer,
                ProjectDomain::Automation => &mut self.automation,
                ProjectDomain::Assets => &mut self.assets,
                ProjectDomain::Mixer => &mut self.mixer,
                ProjectDomain::Air => &mut self.air,
                ProjectDomain::Bindings => &mut self.bindings,
            };
            *revision = checked_revision(*revision)?;
        }
        Ok(())
    }

    pub fn domain(self, domain: ProjectDomain) -> u64 {
        match domain {
            ProjectDomain::Arrangement => self.arrangement,
            ProjectDomain::Sequencer => self.sequencer,
            ProjectDomain::Automation => self.automation,
            ProjectDomain::Assets => self.assets,
            ProjectDomain::Mixer => self.mixer,
            ProjectDomain::Air => self.air,
            ProjectDomain::Bindings => self.bindings,
        }
    }
}

fn checked_revision(value: u64) -> Result<u64, BridgeError> {
    value.checked_add(1).ok_or(BridgeError::RevisionExhausted)
}

/// Independent project models. None of their identities are interchangeable.
#[derive(Clone, Debug)]
pub struct ProjectDomains {
    pub arrangement: ArrangementState,
    pub sequencer: Sequencer,
    pub automation: AutomationGraph,
    pub assets: AssetRegistry,
    pub mixer: MixerGraph,
    pub air: AuditoryIr,
}

/// Associations from arrangement-local source references to the media pool.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AssetBindings {
    pub arrangement_assets: BTreeMap<arrangement::AssetId, assets::AssetId>,
    pub sequencer_samples: BTreeMap<sequencer::SampleAssetId, assets::AssetId>,
}

/// Arrangement pattern references and placements mapped into the sequencer.
///
/// Arrangement placement is canonical. The linked sequencer clip must compile
/// to exactly the same frame range and must use the linked definition.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PatternBindings {
    pub definitions: BTreeMap<arrangement::PatternId, sequencer::PatternId>,
    pub placements: BTreeMap<arrangement::ClipId, sequencer::PatternClipId>,
}

/// Arrangement-local automation aliases mapped to reusable authored lanes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutomationBindings {
    pub lanes: BTreeMap<arrangement::ParameterId, automation::AutomationLaneId>,
}

/// Signal-flow destinations for timeline entities.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MixerBindings {
    pub tracks: BTreeMap<arrangement::TrackId, mixer::BusId>,
    /// Optional per-clip override. Otherwise the owning track bus is used.
    pub clip_overrides: BTreeMap<arrangement::ClipId, mixer::BusId>,
}

/// Optional analytic identity. User-authored material need not have AIR links.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AirBindings {
    pub clips: BTreeMap<arrangement::ClipId, ontology::ObjectId>,
    pub assets: BTreeMap<assets::AssetId, ontology::SourceId>,
    pub automation_lanes: BTreeMap<automation::AutomationLaneId, ontology::ParameterId>,
    pub patterns: BTreeMap<sequencer::PatternId, ontology::ObjectId>,
}

/// Legacy analytic identities that have no lossless constructive counterpart.
/// They remain typed and inspectable instead of being shoved into raw IDs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LegacyIdentityArchive {
    pub events: BTreeMap<session::EventId, ontology::ObjectId>,
    pub clusters: BTreeMap<session::ClusterId, ontology::HypothesisId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectBindings {
    pub assets: AssetBindings,
    pub patterns: PatternBindings,
    pub automation: AutomationBindings,
    pub mixer: MixerBindings,
    pub air: AirBindings,
    pub legacy_air: LegacyIdentityArchive,
    allocators: BindingAllocators,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BindingAllocators {
    next_arrangement_asset: u64,
    next_sequencer_sample: u64,
    next_arrangement_pattern: u64,
    next_arrangement_parameter: u64,
}

impl Default for ProjectBindings {
    fn default() -> Self {
        Self {
            assets: AssetBindings::default(),
            patterns: PatternBindings::default(),
            automation: AutomationBindings::default(),
            mixer: MixerBindings::default(),
            air: AirBindings::default(),
            legacy_air: LegacyIdentityArchive::default(),
            allocators: BindingAllocators {
                next_arrangement_asset: 1,
                next_sequencer_sample: 1,
                next_arrangement_pattern: 1,
                next_arrangement_parameter: 1,
            },
        }
    }
}

impl ProjectBindings {
    /// Return the existing arrangement alias or allocate a never-reused one.
    pub fn bind_media_asset(
        &mut self,
        asset: assets::AssetId,
    ) -> Result<arrangement::AssetId, BridgeError> {
        if let Some((reference, _)) = self
            .assets
            .arrangement_assets
            .iter()
            .find(|(_, candidate)| **candidate == asset)
        {
            return Ok(*reference);
        }
        let reference = arrangement::AssetId::from_raw(take_binding_id(
            &mut self.allocators.next_arrangement_asset,
        )?);
        self.assets.arrangement_assets.insert(reference, asset);
        Ok(reference)
    }

    pub fn bind_sequencer_sample(
        &mut self,
        asset: assets::AssetId,
    ) -> Result<sequencer::SampleAssetId, BridgeError> {
        if let Some((reference, _)) = self
            .assets
            .sequencer_samples
            .iter()
            .find(|(_, candidate)| **candidate == asset)
        {
            return Ok(*reference);
        }
        let reference = sequencer::SampleAssetId::from_raw(take_binding_id(
            &mut self.allocators.next_sequencer_sample,
        )?);
        self.assets.sequencer_samples.insert(reference, asset);
        Ok(reference)
    }

    pub fn bind_pattern_definition(
        &mut self,
        pattern: sequencer::PatternId,
    ) -> Result<arrangement::PatternId, BridgeError> {
        if let Some((reference, _)) = self
            .patterns
            .definitions
            .iter()
            .find(|(_, candidate)| **candidate == pattern)
        {
            return Ok(*reference);
        }
        let reference = arrangement::PatternId::from_raw(take_binding_id(
            &mut self.allocators.next_arrangement_pattern,
        )?);
        self.patterns.definitions.insert(reference, pattern);
        Ok(reference)
    }

    pub fn bind_automation_lane(
        &mut self,
        lane: automation::AutomationLaneId,
    ) -> Result<arrangement::ParameterId, BridgeError> {
        if let Some((reference, _)) = self
            .automation
            .lanes
            .iter()
            .find(|(_, candidate)| **candidate == lane)
        {
            return Ok(*reference);
        }
        let reference = arrangement::ParameterId::from_raw(take_binding_id(
            &mut self.allocators.next_arrangement_parameter,
        )?);
        self.automation.lanes.insert(reference, lane);
        Ok(reference)
    }
}

fn take_binding_id(next: &mut u64) -> Result<u64, BridgeError> {
    let id = *next;
    if id == 0 {
        return Err(BridgeError::IdentityExhausted);
    }
    *next = next.checked_add(1).ok_or(BridgeError::IdentityExhausted)?;
    Ok(id)
}

#[derive(Clone, Debug)]
pub struct ProjectState {
    pub domains: ProjectDomains,
    pub bindings: ProjectBindings,
}

/// The aggregate project publishes only validated states.
#[derive(Clone, Debug)]
pub struct DawProject {
    pub schema_version: u32,
    pub name: String,
    state: ProjectState,
    revisions: ProjectRevisions,
    saved_revision: u64,
    journal: Vec<TransactionRecord>,
}

impl DawProject {
    pub fn new(
        name: impl Into<String>,
        sample_rate: u32,
        initial_bpm: f64,
    ) -> Result<Self, BridgeError> {
        let project = Self {
            schema_version: DAW_PROJECT_SCHEMA_VERSION,
            name: name.into(),
            state: ProjectState {
                domains: ProjectDomains {
                    arrangement: ArrangementState::new(sample_rate)
                        .map_err(|error| BridgeError::Domain(error.to_string()))?,
                    sequencer: Sequencer::new(
                        TempoMap::common_time(sample_rate, initial_bpm)
                            .map_err(|error| BridgeError::Domain(error.to_string()))?,
                    ),
                    automation: AutomationGraph::new(),
                    assets: AssetRegistry::new(),
                    mixer: MixerGraph::new("Master"),
                    air: AuditoryIr::new(sample_rate),
                },
                bindings: ProjectBindings::default(),
            },
            revisions: ProjectRevisions::default(),
            saved_revision: 0,
            journal: Vec::new(),
        };
        project.require_valid()?;
        Ok(project)
    }

    pub fn state(&self) -> &ProjectState {
        &self.state
    }

    pub fn revisions(&self) -> ProjectRevisions {
        self.revisions
    }

    pub fn is_dirty(&self) -> bool {
        self.revisions.aggregate != self.saved_revision
    }

    pub fn mark_saved(&mut self) {
        self.saved_revision = self.revisions.aggregate;
    }

    pub fn journal(&self) -> &[TransactionRecord] {
        &self.journal
    }

    /// Clone, mutate and validate without changing published project state.
    /// Dropping the returned value is an explicit rollback.
    pub fn prepare_transaction<F, E>(
        &self,
        label: impl Into<String>,
        expected_revision: u64,
        touched: BTreeSet<ProjectDomain>,
        mutate: F,
    ) -> Result<PreparedProjectTransaction, BridgeError>
    where
        F: FnOnce(&mut ProjectState) -> Result<(), E>,
        E: fmt::Display,
    {
        if expected_revision != self.revisions.aggregate {
            return Err(BridgeError::RevisionConflict {
                expected: expected_revision,
                actual: self.revisions.aggregate,
            });
        }
        if touched.is_empty() {
            return Err(BridgeError::EmptyTransaction);
        }
        let mut candidate = self.state.clone();
        mutate(&mut candidate).map_err(|error| BridgeError::Mutation(error.to_string()))?;
        let actual = changed_domains(&self.state, &candidate);
        if actual != touched {
            return Err(BridgeError::TouchedDomainMismatch {
                declared: touched,
                actual,
            });
        }
        let issues = validate_project_state(self.schema_version, &candidate);
        if !issues.is_empty() {
            return Err(BridgeError::InvalidProject(issues));
        }
        Ok(PreparedProjectTransaction {
            label: label.into(),
            base_revision: expected_revision,
            touched,
            candidate,
        })
    }

    /// Publish a prepared candidate iff nobody committed since it was built.
    /// Revision overflow is checked before state replacement.
    pub fn commit_prepared(
        &mut self,
        prepared: PreparedProjectTransaction,
    ) -> Result<u64, BridgeError> {
        if prepared.base_revision != self.revisions.aggregate {
            return Err(BridgeError::RevisionConflict {
                expected: prepared.base_revision,
                actual: self.revisions.aggregate,
            });
        }
        let mut revisions = self.revisions;
        revisions.advance(&prepared.touched)?;
        self.state = prepared.candidate;
        self.revisions = revisions;
        self.journal.push(TransactionRecord {
            revision: revisions.aggregate,
            label: prepared.label,
            touched: prepared.touched,
        });
        Ok(revisions.aggregate)
    }

    pub fn transact<F, E>(
        &mut self,
        label: impl Into<String>,
        expected_revision: u64,
        touched: BTreeSet<ProjectDomain>,
        mutate: F,
    ) -> Result<u64, BridgeError>
    where
        F: FnOnce(&mut ProjectState) -> Result<(), E>,
        E: fmt::Display,
    {
        let prepared = self.prepare_transaction(label, expected_revision, touched, mutate)?;
        self.commit_prepared(prepared)
    }

    pub fn validate(&self) -> Vec<BridgeValidationIssue> {
        validate_project_state(self.schema_version, &self.state)
    }

    pub fn require_valid(&self) -> Result<(), BridgeError> {
        let issues = self.validate();
        if issues.is_empty() {
            Ok(())
        } else {
            Err(BridgeError::InvalidProject(issues))
        }
    }

    /// Compile immutable scheduling/control metadata for one exact frame range.
    /// No decoder, synth, plugin or mixer DSP is executed here.
    pub fn compile_snapshot(
        &self,
        range: FrameRange,
        performance_seed: u64,
    ) -> Result<RenderCompileSnapshot, BridgeError> {
        self.require_valid()?;
        if range.len() > u64::from(u32::MAX) {
            return Err(BridgeError::CompileRangeTooLong(range.len()));
        }
        let sequencer_range = sequencer::FrameRange::new(
            sequencer::ProjectFrame(range.start.get()),
            sequencer::ProjectFrame(range.end.get()),
        )
        .map_err(|error| BridgeError::Domain(error.to_string()))?;
        let scheduled_events = self
            .state
            .domains
            .sequencer
            .schedule_project_window(sequencer_range, performance_seed);
        let beat_map = SequencerBeatMap(&self.state.domains.sequencer);
        let automation = self
            .state
            .domains
            .automation
            .compile(&beat_map)
            .map_err(|error| BridgeError::Domain(error.to_string()))?;
        let latency = self
            .state
            .domains
            .mixer
            .latency_plan()
            .map_err(|error| BridgeError::Domain(error.to_string()))?;

        let mut clips = Vec::new();
        for clip in self.state.domains.arrangement.clips_intersecting(range) {
            let bus = self
                .state
                .bindings
                .mixer
                .clip_overrides
                .get(&clip.id)
                .or_else(|| self.state.bindings.mixer.tracks.get(&clip.track_id))
                .copied();
            let media = match &clip.content {
                ClipContent::Audio(audio) => {
                    let registry_id = self.state.bindings.assets.arrangement_assets[&audio.asset];
                    let asset = self
                        .state
                        .domains
                        .assets
                        .get(registry_id)
                        .expect("validated");
                    Some(CompiledMediaReference {
                        asset: registry_id,
                        available: !matches!(
                            asset.availability(),
                            AssetAvailability::Missing { .. }
                        ),
                    })
                }
                _ => None,
            };
            clips.push(CompiledClipReference {
                clip: clip.clone(),
                destination_bus: bus,
                media,
                air_object: self.state.bindings.air.clips.get(&clip.id).copied(),
            });
        }
        clips.sort_by_key(|compiled| (compiled.clip.placement.start, compiled.clip.id));

        Ok(RenderCompileSnapshot {
            project_revision: self.revisions,
            sample_rate: self.state.domains.arrangement.sample_rate,
            range,
            clips,
            scheduled_events,
            automation,
            latency,
            master_bus: self.state.domains.mixer.master(),
            // False until a downstream graph proves that all required media,
            // instruments, plugins, transforms and routes have DSP executors.
            dsp_graph_complete: false,
        })
    }

    /// Deterministic persistence envelope. Domain payloads remain separate so
    /// each core can acquire a versioned DTO without leaking runtime objects.
    pub fn save_intent(&self) -> ProjectSaveIntent {
        ProjectSaveIntent::from_project(self)
    }
}

fn changed_domains(before: &ProjectState, after: &ProjectState) -> BTreeSet<ProjectDomain> {
    let mut changed = BTreeSet::new();
    if before.domains.arrangement != after.domains.arrangement {
        changed.insert(ProjectDomain::Arrangement);
    }
    if !sequencers_equal(&before.domains.sequencer, &after.domains.sequencer) {
        changed.insert(ProjectDomain::Sequencer);
    }
    if before.domains.automation != after.domains.automation {
        changed.insert(ProjectDomain::Automation);
    }
    if before.domains.assets != after.domains.assets {
        changed.insert(ProjectDomain::Assets);
    }
    if before.domains.mixer != after.domains.mixer {
        changed.insert(ProjectDomain::Mixer);
    }
    if before.domains.air != after.domains.air {
        changed.insert(ProjectDomain::Air);
    }
    if before.bindings != after.bindings {
        changed.insert(ProjectDomain::Bindings);
    }
    changed
}

fn sequencers_equal(left: &Sequencer, right: &Sequencer) -> bool {
    left.revision() == right.revision()
        && left.tempo_map() == right.tempo_map()
        && left.patterns().patterns().eq(right.patterns().patterns())
        && left.clips().eq(right.clips())
}

#[derive(Clone, Debug)]
pub struct PreparedProjectTransaction {
    label: String,
    base_revision: u64,
    touched: BTreeSet<ProjectDomain>,
    candidate: ProjectState,
}

impl PreparedProjectTransaction {
    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn base_revision(&self) -> u64 {
        self.base_revision
    }

    pub fn touched(&self) -> &BTreeSet<ProjectDomain> {
        &self.touched
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionRecord {
    pub revision: u64,
    pub label: String,
    pub touched: BTreeSet<ProjectDomain>,
}

struct SequencerBeatMap<'a>(&'a Sequencer);

impl BeatFrameMap for SequencerBeatMap<'_> {
    fn beat_to_frame(&self, beat: automation::BeatTime) -> automation::ProjectFrame {
        let frame = self
            .0
            .tempo_map()
            .beat_to_frame(sequencer::BeatTime(beat.0));
        automation::ProjectFrame(frame.0)
    }
}

#[derive(Clone, Debug)]
pub struct CompiledMediaReference {
    pub asset: assets::AssetId,
    pub available: bool,
}

#[derive(Clone, Debug)]
pub struct CompiledClipReference {
    pub clip: arrangement::Clip,
    pub destination_bus: Option<mixer::BusId>,
    pub media: Option<CompiledMediaReference>,
    pub air_object: Option<ontology::ObjectId>,
}

/// Control-thread product suitable for publishing behind an `Arc`.
#[derive(Clone, Debug)]
pub struct RenderCompileSnapshot {
    pub project_revision: ProjectRevisions,
    pub sample_rate: u32,
    pub range: FrameRange,
    pub clips: Vec<CompiledClipReference>,
    pub scheduled_events: Vec<sequencer::ScheduledEvent>,
    pub automation: automation::CompiledAutomation,
    pub latency: mixer::LatencyPlan,
    pub master_bus: mixer::BusId,
    pub dsp_graph_complete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidationDomain {
    Project,
    Arrangement,
    Sequencer,
    Automation,
    Assets,
    Mixer,
    Air,
    Bindings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeValidationIssue {
    pub domain: ValidationDomain,
    pub path: String,
    pub message: String,
}

impl BridgeValidationIssue {
    fn new(domain: ValidationDomain, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            domain,
            path: path.into(),
            message: message.into(),
        }
    }
}

pub fn validate_project_state(
    schema_version: u32,
    state: &ProjectState,
) -> Vec<BridgeValidationIssue> {
    let mut issues = Vec::new();
    if schema_version != DAW_PROJECT_SCHEMA_VERSION {
        issues.push(BridgeValidationIssue::new(
            ValidationDomain::Project,
            "schema_version",
            format!("unsupported project schema {schema_version}"),
        ));
    }

    if let Err(error) = state.domains.arrangement.validate() {
        issues.push(issue(ValidationDomain::Arrangement, "state", error));
    }
    if let Err(error) = state.domains.sequencer.validate() {
        issues.push(issue(ValidationDomain::Sequencer, "state", error));
    }
    if let Err(error) = state.domains.automation.validate() {
        issues.push(issue(ValidationDomain::Automation, "state", error));
    }
    for error in state.domains.assets.validate() {
        issues.push(BridgeValidationIssue::new(
            ValidationDomain::Assets,
            "registry",
            format!("{error:?}"),
        ));
    }
    if let Err(error) = state.domains.mixer.validate() {
        issues.push(issue(ValidationDomain::Mixer, "graph", error));
    }
    for error in state.domains.air.validate() {
        issues.push(BridgeValidationIssue::new(
            ValidationDomain::Air,
            error.path,
            error.message,
        ));
    }

    let sample_rate = state.domains.arrangement.sample_rate;
    if state.domains.sequencer.tempo_map().sample_rate() != sample_rate {
        issues.push(binding_issue(
            "sample_rate",
            "arrangement and sequencer sample rates differ",
        ));
    }
    if state.domains.air.sample_rate != sample_rate {
        issues.push(binding_issue(
            "sample_rate",
            "arrangement and AIR sample rates differ",
        ));
    }

    validate_asset_bindings(state, &mut issues);
    validate_pattern_bindings(state, &mut issues);
    validate_automation_bindings(state, &mut issues);
    validate_mixer_bindings(state, &mut issues);
    validate_air_bindings(state, &mut issues);
    validate_automation_addresses(state, &mut issues);
    validate_binding_allocators(&state.bindings, &mut issues);
    issues
}

fn issue(
    domain: ValidationDomain,
    path: impl Into<String>,
    error: impl fmt::Display,
) -> BridgeValidationIssue {
    BridgeValidationIssue::new(domain, path, error.to_string())
}

fn binding_issue(path: impl Into<String>, message: impl Into<String>) -> BridgeValidationIssue {
    BridgeValidationIssue::new(ValidationDomain::Bindings, path, message)
}

fn validate_binding_allocators(
    bindings: &ProjectBindings,
    issues: &mut Vec<BridgeValidationIssue>,
) {
    let checks = [
        (
            "allocators.next_arrangement_asset",
            bindings.allocators.next_arrangement_asset,
            bindings
                .assets
                .arrangement_assets
                .keys()
                .map(|id| id.get())
                .max()
                .unwrap_or(0),
        ),
        (
            "allocators.next_sequencer_sample",
            bindings.allocators.next_sequencer_sample,
            bindings
                .assets
                .sequencer_samples
                .keys()
                .map(|id| id.get())
                .max()
                .unwrap_or(0),
        ),
        (
            "allocators.next_arrangement_pattern",
            bindings.allocators.next_arrangement_pattern,
            bindings
                .patterns
                .definitions
                .keys()
                .map(|id| id.get())
                .max()
                .unwrap_or(0),
        ),
        (
            "allocators.next_arrangement_parameter",
            bindings.allocators.next_arrangement_parameter,
            bindings
                .automation
                .lanes
                .keys()
                .map(|id| id.get())
                .max()
                .unwrap_or(0),
        ),
    ];
    for (path, next, maximum) in checks {
        if next == 0 || next <= maximum {
            issues.push(binding_issue(
                path,
                format!("allocator {next} is not ahead of maximum identity {maximum}"),
            ));
        }
    }
}

fn validate_asset_bindings(state: &ProjectState, issues: &mut Vec<BridgeValidationIssue>) {
    for (reference, asset) in &state.bindings.assets.arrangement_assets {
        if state.domains.assets.get(*asset).is_none() {
            issues.push(binding_issue(
                format!("assets.arrangement_assets[{reference}]"),
                format!("references missing media-pool asset {}", asset.0),
            ));
        }
    }
    for (reference, asset) in &state.bindings.assets.sequencer_samples {
        if state.domains.assets.get(*asset).is_none() {
            issues.push(binding_issue(
                format!("assets.sequencer_samples[{}]", reference.get()),
                format!("references missing media-pool asset {}", asset.0),
            ));
        }
    }
    for clip in state.domains.arrangement.clips.values() {
        let ClipContent::Audio(audio) = &clip.content else {
            continue;
        };
        let path = format!("arrangement.clips[{}].audio.asset", clip.id);
        let Some(asset_id) = state.bindings.assets.arrangement_assets.get(&audio.asset) else {
            issues.push(binding_issue(path, "has no media-pool binding"));
            continue;
        };
        let Some(asset) = state.domains.assets.get(*asset_id) else {
            continue;
        };
        if audio.source.end > asset.metadata().frame_count.0 {
            issues.push(binding_issue(
                path,
                "source range exceeds the bound asset's decoded frame count",
            ));
        }
    }

    for pattern in state.domains.sequencer.patterns().patterns() {
        if let sequencer::PatternContent::Steps(steps) = &pattern.content {
            for lane in steps.lanes.values() {
                if let sequencer::TriggerTarget::Sample(sample) = &lane.target {
                    if !state.bindings.assets.sequencer_samples.contains_key(sample) {
                        issues.push(binding_issue(
                            format!(
                                "sequencer.patterns[{}].lanes[{}].sample",
                                pattern.id.get(),
                                lane.id.get()
                            ),
                            "has no media-pool binding",
                        ));
                    }
                }
            }
        }
    }
}

fn validate_pattern_bindings(state: &ProjectState, issues: &mut Vec<BridgeValidationIssue>) {
    for (reference, pattern) in &state.bindings.patterns.definitions {
        if state.domains.sequencer.patterns().get(*pattern).is_none() {
            issues.push(binding_issue(
                format!("patterns.definitions[{reference}]"),
                format!("references missing sequencer pattern {}", pattern.get()),
            ));
        }
    }

    let mut linked_sequencer_clips = BTreeSet::new();
    for (arrangement_clip_id, sequencer_clip_id) in &state.bindings.patterns.placements {
        if !linked_sequencer_clips.insert(*sequencer_clip_id) {
            issues.push(binding_issue(
                format!("patterns.placements[{arrangement_clip_id}]"),
                "sequencer placement is linked more than once",
            ));
        }
        let Some(clip) = state.domains.arrangement.clip(*arrangement_clip_id) else {
            issues.push(binding_issue(
                format!("patterns.placements[{arrangement_clip_id}]"),
                "key is not an arrangement clip",
            ));
            continue;
        };
        let ClipContent::Pattern(region) = &clip.content else {
            issues.push(binding_issue(
                format!("patterns.placements[{arrangement_clip_id}]"),
                "key is not a pattern clip",
            ));
            continue;
        };
        let Some(seq_clip) = state.domains.sequencer.clip(*sequencer_clip_id) else {
            issues.push(binding_issue(
                format!("patterns.placements[{arrangement_clip_id}]"),
                "references a missing sequencer clip",
            ));
            continue;
        };
        if state.bindings.patterns.definitions.get(&region.pattern) != Some(&seq_clip.pattern) {
            issues.push(binding_issue(
                format!("patterns.placements[{arrangement_clip_id}]"),
                "definition link and sequencer clip pattern disagree",
            ));
        }
        let tempo = state.domains.sequencer.tempo_map();
        let seq_start = tempo.beat_to_frame(seq_clip.start).0;
        let seq_end = tempo.beat_to_frame(seq_clip.end()).0;
        if seq_start != clip.placement.start.get() || seq_end != clip.placement.end.get() {
            issues.push(binding_issue(
                format!("patterns.placements[{arrangement_clip_id}]"),
                "sequencer musical placement does not compile to the canonical arrangement range",
            ));
        }
        let offset = tempo.beat_to_frame(seq_clip.pattern_offset).0.max(0) as u64;
        if offset != region.content_offset_frames || seq_clip.looped != region.looped {
            issues.push(binding_issue(
                format!("patterns.placements[{arrangement_clip_id}]"),
                "content offset or loop mode disagrees with arrangement metadata",
            ));
        }
    }

    for clip in state.domains.arrangement.clips.values() {
        if matches!(&clip.content, ClipContent::Pattern(_))
            && !state.bindings.patterns.placements.contains_key(&clip.id)
        {
            issues.push(binding_issue(
                format!("arrangement.clips[{}].pattern", clip.id),
                "has no sequencer placement binding",
            ));
        }
    }
    for clip in state.domains.sequencer.clips() {
        if !linked_sequencer_clips.contains(&clip.id) {
            issues.push(binding_issue(
                format!("sequencer.clips[{}]", clip.id.get()),
                "has no canonical arrangement placement",
            ));
        }
    }
}

fn validate_automation_bindings(state: &ProjectState, issues: &mut Vec<BridgeValidationIssue>) {
    for (reference, lane) in &state.bindings.automation.lanes {
        if state.domains.automation.lane(*lane).is_none() {
            issues.push(binding_issue(
                format!("automation.lanes[{reference}]"),
                format!("references missing automation lane {}", lane.get()),
            ));
        }
    }
    for clip in state.domains.arrangement.clips.values() {
        let ClipContent::Automation(region) = &clip.content else {
            continue;
        };
        if !state
            .bindings
            .automation
            .lanes
            .contains_key(&region.parameter)
        {
            issues.push(binding_issue(
                format!("arrangement.clips[{}].automation", clip.id),
                "has no automation-lane binding",
            ));
        }
    }
}

fn validate_mixer_bindings(state: &ProjectState, issues: &mut Vec<BridgeValidationIssue>) {
    let mut assigned = BTreeSet::new();
    for (track_id, bus_id) in &state.bindings.mixer.tracks {
        if state.domains.arrangement.track(*track_id).is_none() {
            issues.push(binding_issue(
                format!("mixer.tracks[{track_id}]"),
                "key is not an arrangement track",
            ));
        }
        let Some(bus) = state.domains.mixer.bus(*bus_id) else {
            issues.push(binding_issue(
                format!("mixer.tracks[{track_id}]"),
                "references a missing mixer bus",
            ));
            continue;
        };
        if bus.kind() == BusKind::Master {
            issues.push(binding_issue(
                format!("mixer.tracks[{track_id}]"),
                "a timeline track cannot own the master bus",
            ));
        }
        if !assigned.insert(*bus_id) {
            issues.push(binding_issue(
                format!("mixer.tracks[{track_id}]"),
                "a source bus is already owned by another track",
            ));
        }
    }
    for track in state.domains.arrangement.tracks.values() {
        if track.kind != TrackKind::Automation
            && !state.bindings.mixer.tracks.contains_key(&track.id)
        {
            issues.push(binding_issue(
                format!("arrangement.tracks[{}]", track.id),
                "renderable track has no mixer-bus binding",
            ));
        }
    }
    for (clip, bus) in &state.bindings.mixer.clip_overrides {
        if state.domains.arrangement.clip(*clip).is_none() {
            issues.push(binding_issue(
                format!("mixer.clip_overrides[{clip}]"),
                "key is not an arrangement clip",
            ));
        }
        if state.domains.mixer.bus(*bus).is_none() {
            issues.push(binding_issue(
                format!("mixer.clip_overrides[{clip}]"),
                "references a missing mixer bus",
            ));
        }
    }
}

fn validate_air_bindings(state: &ProjectState, issues: &mut Vec<BridgeValidationIssue>) {
    for (clip, object) in &state.bindings.air.clips {
        if state.domains.arrangement.clip(*clip).is_none() {
            issues.push(binding_issue(
                format!("air.clips[{clip}]"),
                "key is not an arrangement clip",
            ));
        }
        if !state.domains.air.objects.contains_key(object) {
            issues.push(binding_issue(
                format!("air.clips[{clip}]"),
                "references a missing AIR object",
            ));
        }
    }
    for (asset, source) in &state.bindings.air.assets {
        if state.domains.assets.get(*asset).is_none() {
            issues.push(binding_issue(
                format!("air.assets[{}]", asset.0),
                "key is not a media-pool asset",
            ));
        }
        if !state.domains.air.sources.contains_key(source) {
            issues.push(binding_issue(
                format!("air.assets[{}]", asset.0),
                "references a missing AIR source",
            ));
        }
    }
    for (lane, parameter) in &state.bindings.air.automation_lanes {
        if state.domains.automation.lane(*lane).is_none() {
            issues.push(binding_issue(
                format!("air.automation_lanes[{}]", lane.get()),
                "key is not an automation lane",
            ));
        }
        if !state.domains.air.parameters.contains_key(parameter) {
            issues.push(binding_issue(
                format!("air.automation_lanes[{}]", lane.get()),
                "references a missing AIR parameter",
            ));
        }
    }
    for (pattern, object) in &state.bindings.air.patterns {
        if state.domains.sequencer.patterns().get(*pattern).is_none() {
            issues.push(binding_issue(
                format!("air.patterns[{}]", pattern.get()),
                "key is not a sequencer pattern",
            ));
        }
        if !state.domains.air.objects.contains_key(object) {
            issues.push(binding_issue(
                format!("air.patterns[{}]", pattern.get()),
                "references a missing AIR object",
            ));
        }
    }
    for (event, object) in &state.bindings.legacy_air.events {
        if !state.domains.air.objects.contains_key(object) {
            issues.push(binding_issue(
                format!("legacy_air.events[{}]", event.get()),
                "references a missing AIR object",
            ));
        }
    }
    for (cluster, hypothesis) in &state.bindings.legacy_air.clusters {
        if !state.domains.air.hypotheses.contains_key(hypothesis) {
            issues.push(binding_issue(
                format!("legacy_air.clusters[{}]", cluster.get()),
                "references a missing AIR hypothesis",
            ));
        }
    }
}

fn validate_automation_addresses(state: &ProjectState, issues: &mut Vec<BridgeValidationIssue>) {
    for descriptor in state.domains.automation.descriptors() {
        let missing = match &descriptor.address {
            ParameterAddress::Mixer(target) => match target {
                automation::MixerTarget::BusGain(id)
                | automation::MixerTarget::BusPan(id)
                | automation::MixerTarget::BusMute(id) => state
                    .domains
                    .mixer
                    .bus(mixer::BusId::from_raw(*id))
                    .is_none(),
                automation::MixerTarget::SendLevel(id) | automation::MixerTarget::SendMute(id) => {
                    !state
                        .domains
                        .mixer
                        .buses()
                        .any(|bus| bus.sends().iter().any(|send| send.id().get() == *id))
                }
                automation::MixerTarget::InsertWet(id)
                | automation::MixerTarget::InsertBypass(id) => state
                    .domains
                    .mixer
                    .processor(mixer::ProcessorId::from_raw(*id))
                    .is_none(),
            },
            ParameterAddress::Plugin { processor_id, .. } => state
                .domains
                .mixer
                .processor(mixer::ProcessorId::from_raw(*processor_id))
                .is_none(),
            ParameterAddress::Clip { clip_id, .. } => state
                .domains
                .arrangement
                .clip(arrangement::ClipId::from_raw(*clip_id))
                .is_none(),
            ParameterAddress::Decomposition(target) => match target {
                automation::DecompositionTarget::ComponentGain { component_id }
                | automation::DecompositionTarget::ComponentPan { component_id } => !state
                    .domains
                    .air
                    .objects
                    .contains_key(&ontology::ObjectId::new(*component_id)),
                automation::DecompositionTarget::ObjectTransformParameter {
                    object_id,
                    transform_id,
                    parameter_id,
                } => {
                    !state
                        .domains
                        .air
                        .objects
                        .contains_key(&ontology::ObjectId::new(*object_id))
                        || !state
                            .domains
                            .air
                            .transforms
                            .contains_key(&ontology::TransformId::new(*transform_id))
                        || !state
                            .domains
                            .air
                            .parameters
                            .contains_key(&ontology::ParameterId::new(*parameter_id))
                }
                automation::DecompositionTarget::HypothesisBlend { hypothesis_id } => !state
                    .domains
                    .air
                    .hypotheses
                    .contains_key(&ontology::HypothesisId::new(*hypothesis_id)),
                automation::DecompositionTarget::ResidualMix { hypothesis_set_id } => !state
                    .domains
                    .air
                    .hypothesis_sets
                    .contains_key(&ontology::HypothesisSetId::new(*hypothesis_set_id)),
            },
            ParameterAddress::AirParameter(id) => !state
                .domains
                .air
                .parameters
                .contains_key(&ontology::ParameterId::new(*id)),
            ParameterAddress::PerceptualLens { .. } | ParameterAddress::Custom { .. } => false,
        };
        if missing {
            issues.push(binding_issue(
                format!("automation.descriptors[{:?}]", descriptor.address),
                "parameter address does not resolve in the aggregate project",
            ));
        }
    }
}

/// Persistence is a versioned envelope of independently versioned sections.
/// `payload_key` names a required DTO/blob entry; it is not a Rust type name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DomainSaveSection {
    pub domain: ProjectDomain,
    pub schema_version: u32,
    pub revision: u64,
    pub payload_key: String,
    pub encoding: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingSaveRow {
    pub map: &'static str,
    pub left: u64,
    pub right: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectSaveIntent {
    pub schema_version: u32,
    pub name: String,
    pub revision: ProjectRevisions,
    pub sections: Vec<DomainSaveSection>,
    pub bindings: Vec<BindingSaveRow>,
}

impl ProjectSaveIntent {
    fn from_project(project: &DawProject) -> Self {
        let specs = [
            (
                ProjectDomain::Arrangement,
                arrangement::ArrangementState::SCHEMA_VERSION,
                "arrangement.json",
            ),
            (ProjectDomain::Sequencer, 1, "sequencer.json"),
            (ProjectDomain::Automation, 1, "automation.json"),
            (ProjectDomain::Assets, 1, "assets.json"),
            (ProjectDomain::Mixer, 1, "mixer.json"),
            (
                ProjectDomain::Air,
                ontology::AuditoryIr::CURRENT_SCHEMA_VERSION,
                "air.json",
            ),
            (ProjectDomain::Bindings, 1, "bindings.json"),
        ];
        let sections = specs
            .into_iter()
            .map(|(domain, schema_version, payload_key)| DomainSaveSection {
                domain,
                schema_version,
                revision: project.revisions.domain(domain),
                payload_key: payload_key.into(),
                encoding: "json".into(),
            })
            .collect();
        let b = &project.state.bindings;
        let mut bindings = Vec::new();
        bindings.extend(
            b.assets
                .arrangement_assets
                .iter()
                .map(|(a, z)| BindingSaveRow {
                    map: "arrangement_asset/media_asset",
                    left: a.get(),
                    right: z.0,
                }),
        );
        bindings.extend(
            b.assets
                .sequencer_samples
                .iter()
                .map(|(a, z)| BindingSaveRow {
                    map: "sequencer_sample/media_asset",
                    left: a.get(),
                    right: z.0,
                }),
        );
        bindings.extend(b.patterns.definitions.iter().map(|(a, z)| BindingSaveRow {
            map: "arrangement_pattern/sequencer_pattern",
            left: a.get(),
            right: z.get(),
        }));
        bindings.extend(b.patterns.placements.iter().map(|(a, z)| BindingSaveRow {
            map: "arrangement_clip/sequencer_clip",
            left: a.get(),
            right: z.get(),
        }));
        bindings.extend(b.automation.lanes.iter().map(|(a, z)| BindingSaveRow {
            map: "arrangement_parameter/automation_lane",
            left: a.get(),
            right: z.get(),
        }));
        bindings.extend(b.mixer.tracks.iter().map(|(a, z)| BindingSaveRow {
            map: "arrangement_track/mixer_bus",
            left: a.get(),
            right: z.get(),
        }));
        bindings.extend(b.mixer.clip_overrides.iter().map(|(a, z)| BindingSaveRow {
            map: "arrangement_clip/mixer_bus_override",
            left: a.get(),
            right: z.get(),
        }));
        bindings.extend(b.air.clips.iter().map(|(a, z)| BindingSaveRow {
            map: "arrangement_clip/air_object",
            left: a.get(),
            right: z.get(),
        }));
        bindings.extend(b.air.assets.iter().map(|(a, z)| BindingSaveRow {
            map: "media_asset/air_source",
            left: a.0,
            right: z.get(),
        }));
        bindings.extend(b.air.automation_lanes.iter().map(|(a, z)| BindingSaveRow {
            map: "automation_lane/air_parameter",
            left: a.get(),
            right: z.get(),
        }));
        bindings.extend(b.air.patterns.iter().map(|(a, z)| BindingSaveRow {
            map: "sequencer_pattern/air_object",
            left: a.get(),
            right: z.get(),
        }));
        bindings.sort_by_key(|row| (row.map, row.left, row.right));
        Self {
            schema_version: project.schema_version,
            name: project.name.clone(),
            revision: project.revisions,
            sections,
            bindings,
        }
    }
}

/// External facts needed for a lossless legacy audio-clip migration.
#[derive(Clone, Debug, Default)]
pub struct LegacyMigrationAssets {
    pub registry: AssetRegistry,
    pub clip_assets: BTreeMap<session::ClipId, assets::AssetId>,
    pub asset_sources: BTreeMap<assets::AssetId, ontology::SourceId>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LegacyMigrationReport {
    pub tracks: BTreeMap<session::TrackId, arrangement::TrackId>,
    pub clips: BTreeMap<session::ClipId, arrangement::ClipId>,
    /// Legacy analytical events remain in AIR and in the typed archive. They
    /// are not silently promoted to MIDI notes, drum hits or audio clips.
    pub archived_events: usize,
    pub archived_clusters: usize,
}

/// Migrate the legacy editor/AIR document without inventing source identity.
/// Every legacy audio clip therefore requires an explicit registry asset.
pub fn migrate_legacy_project(
    name: impl Into<String>,
    legacy: &ProjectDocument,
    inputs: LegacyMigrationAssets,
) -> Result<(DawProject, LegacyMigrationReport), BridgeError> {
    let legacy_issues = legacy.validate();
    if !legacy_issues.is_empty() {
        return Err(BridgeError::LegacyProjectInvalid(format!(
            "{} legacy validation issue(s)",
            legacy_issues.len()
        )));
    }
    let sample_rate = legacy.session.sample_rate();
    let tempo_map = TempoMap::common_time(sample_rate, 120.0)
        .map_err(|error| BridgeError::Domain(error.to_string()))?;
    let mut editor = ArrangementEditor::new(sample_rate)
        .map_err(|error| BridgeError::Domain(error.to_string()))?;
    let mut mixer = MixerGraph::new("Master");
    let mut bindings = ProjectBindings::default();
    let mut report = LegacyMigrationReport::default();

    for track in legacy.session.arrangement().tracks() {
        let kind = match track.kind {
            session::TrackKind::Audio => TrackKind::Audio,
            session::TrackKind::Events => TrackKind::Hybrid,
            session::TrackKind::Group => TrackKind::Group,
        };
        let track_id = editor
            .create_track(track.name.clone(), kind)
            .map_err(|error| BridgeError::Domain(error.to_string()))?;
        let bus_kind = match track.kind {
            session::TrackKind::Group => BusKind::Group,
            _ => BusKind::Source,
        };
        let bus_id = mixer
            .add_bus(bus_kind, track.name.clone())
            .map_err(|error| BridgeError::Domain(error.to_string()))?;
        bindings.mixer.tracks.insert(track_id, bus_id);
        report.tracks.insert(track.id, track_id);
    }

    for clip in legacy.session.arrangement().clips() {
        if clip.timeline.is_empty() {
            return Err(BridgeError::LegacyProjectInvalid(format!(
                "legacy clip {} is empty",
                clip.id.get()
            )));
        }
        let registry_asset = inputs
            .clip_assets
            .get(&clip.id)
            .copied()
            .ok_or(BridgeError::MissingLegacyAsset(clip.id))?;
        let asset = inputs
            .registry
            .get(registry_asset)
            .ok_or(BridgeError::MissingMediaAsset(registry_asset))?;
        let arrangement_asset = bindings.bind_media_asset(registry_asset)?;
        let end = clip
            .source_start
            .checked_add(clip.timeline.len())
            .ok_or(BridgeError::TimeOverflow)?;
        if end > asset.metadata().frame_count.0 {
            return Err(BridgeError::LegacyProjectInvalid(format!(
                "legacy clip {} exceeds media asset {}",
                clip.id.get(),
                registry_asset.0
            )));
        }
        let legacy_lane = legacy
            .session
            .arrangement()
            .lane(clip.lane_id)
            .ok_or_else(|| BridgeError::LegacyProjectInvalid("clip lane is missing".into()))?;
        let track_id = report.tracks[&legacy_lane.track_id];
        let placement = FrameRange::new(
            Frame::new(clip.timeline.start.get()),
            Frame::new(clip.timeline.end.get()),
        )
        .map_err(|error| BridgeError::Domain(error.to_string()))?;
        let source = arrangement::SourceRange::new(clip.source_start, end)
            .map_err(|error| BridgeError::Domain(error.to_string()))?;
        let clip_id = editor
            .create_audio_clip(
                track_id,
                clip.name.clone(),
                placement,
                arrangement_asset,
                source,
            )
            .map_err(|error| BridgeError::Domain(error.to_string()))?;
        report.clips.insert(clip.id, clip_id);
        if let Some(object) = legacy.identities.clip_objects.get(&clip.id) {
            bindings.air.clips.insert(clip_id, *object);
        }
    }

    for (asset, source) in inputs.asset_sources {
        bindings.air.assets.insert(asset, source);
    }
    bindings.legacy_air.events = legacy.identities.event_objects.clone();
    bindings.legacy_air.clusters = legacy.identities.cluster_hypotheses.clone();
    report.archived_events = bindings.legacy_air.events.len();
    report.archived_clusters = bindings.legacy_air.clusters.len();

    let project = DawProject {
        schema_version: DAW_PROJECT_SCHEMA_VERSION,
        name: name.into(),
        state: ProjectState {
            domains: ProjectDomains {
                arrangement: editor.state().clone(),
                sequencer: Sequencer::new(tempo_map),
                automation: AutomationGraph::new(),
                assets: inputs.registry,
                mixer,
                air: legacy.air.clone(),
            },
            bindings,
        },
        revisions: ProjectRevisions::default(),
        saved_revision: 0,
        journal: Vec::new(),
    };
    project.require_valid()?;
    Ok((project, report))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BridgeError {
    Domain(String),
    Mutation(String),
    InvalidProject(Vec<BridgeValidationIssue>),
    RevisionConflict {
        expected: u64,
        actual: u64,
    },
    TouchedDomainMismatch {
        declared: BTreeSet<ProjectDomain>,
        actual: BTreeSet<ProjectDomain>,
    },
    RevisionExhausted,
    IdentityExhausted,
    EmptyTransaction,
    CompileRangeTooLong(u64),
    LegacyProjectInvalid(String),
    MissingLegacyAsset(session::ClipId),
    MissingMediaAsset(assets::AssetId),
    TimeOverflow,
}

impl fmt::Display for BridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Domain(message) => write!(formatter, "domain error: {message}"),
            Self::Mutation(message) => write!(formatter, "transaction mutation failed: {message}"),
            Self::InvalidProject(issues) => {
                write!(
                    formatter,
                    "project has {} validation issue(s)",
                    issues.len()
                )
            }
            Self::RevisionConflict { expected, actual } => write!(
                formatter,
                "project revision conflict: expected {expected}, actual {actual}"
            ),
            Self::TouchedDomainMismatch { declared, actual } => write!(
                formatter,
                "transaction domain declaration {declared:?} does not match actual changes {actual:?}"
            ),
            Self::RevisionExhausted => write!(formatter, "project revision exhausted"),
            Self::IdentityExhausted => write!(formatter, "bridge identity allocator exhausted"),
            Self::EmptyTransaction => write!(formatter, "transaction touches no domains"),
            Self::CompileRangeTooLong(frames) => write!(
                formatter,
                "compile range of {frames} frames exceeds one snapshot window"
            ),
            Self::LegacyProjectInvalid(message) => {
                write!(formatter, "legacy project is not migratable: {message}")
            }
            Self::MissingLegacyAsset(clip) => {
                write!(
                    formatter,
                    "legacy clip {} has no explicit media asset",
                    clip.get()
                )
            }
            Self::MissingMediaAsset(asset) => {
                write!(formatter, "media asset {} does not exist", asset.0)
            }
            Self::TimeOverflow => write!(formatter, "project time overflow"),
        }
    }
}

impl Error for BridgeError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn touched(domains: &[ProjectDomain]) -> BTreeSet<ProjectDomain> {
        domains.iter().copied().collect()
    }

    #[test]
    fn empty_project_is_valid_and_compiles_metadata() {
        let project = DawProject::new("Test", 48_000, 120.0).unwrap();
        assert!(project.validate().is_empty());
        let snapshot = project
            .compile_snapshot(FrameRange::new(Frame::ZERO, Frame::new(512)).unwrap(), 7)
            .unwrap();
        assert_eq!(snapshot.sample_rate, 48_000);
        assert!(snapshot.clips.is_empty());
        assert!(!snapshot.dsp_graph_complete);
    }

    #[test]
    fn prepared_transaction_is_atomic_and_revision_checked() {
        let mut project = DawProject::new("Test", 48_000, 120.0).unwrap();
        let prepared = project
            .prepare_transaction(
                "Add track and bus",
                0,
                touched(&[
                    ProjectDomain::Arrangement,
                    ProjectDomain::Mixer,
                    ProjectDomain::Bindings,
                ]),
                |state| -> Result<(), BridgeError> {
                    let mut editor =
                        ArrangementEditor::from_state(state.domains.arrangement.clone())
                            .map_err(|error| BridgeError::Domain(error.to_string()))?;
                    let track = editor
                        .create_track("Audio", TrackKind::Audio)
                        .map_err(|error| BridgeError::Domain(error.to_string()))?;
                    let bus = state
                        .domains
                        .mixer
                        .add_bus(BusKind::Source, "Audio")
                        .map_err(|error| BridgeError::Domain(error.to_string()))?;
                    state.domains.arrangement = editor.state().clone();
                    state.bindings.mixer.tracks.insert(track, bus);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(project.state().domains.arrangement.tracks.len(), 0);
        assert_eq!(project.commit_prepared(prepared).unwrap(), 1);
        assert_eq!(project.state().domains.arrangement.tracks.len(), 1);
        assert_eq!(project.revisions().arrangement, 1);
        assert_eq!(project.revisions().assets, 0);

        let stale = project.prepare_transaction(
            "stale",
            0,
            touched(&[ProjectDomain::Bindings]),
            |_state| Ok::<_, BridgeError>(()),
        );
        assert!(matches!(stale, Err(BridgeError::RevisionConflict { .. })));
    }

    #[test]
    fn invalid_candidate_rolls_back_before_publication() {
        let project = DawProject::new("Test", 48_000, 120.0).unwrap();
        let result = project.prepare_transaction(
            "Break AIR rate",
            0,
            touched(&[ProjectDomain::Air]),
            |state| -> Result<(), BridgeError> {
                state.domains.air.sample_rate = 44_100;
                Ok(())
            },
        );
        assert!(matches!(result, Err(BridgeError::InvalidProject(_))));
        assert_eq!(project.state().domains.air.sample_rate, 48_000);
    }

    #[test]
    fn declared_domains_must_match_actual_changes() {
        let project = DawProject::new("Test", 48_000, 120.0).unwrap();
        let result = project.prepare_transaction(
            "Misdeclared",
            0,
            touched(&[ProjectDomain::Bindings]),
            |state| -> Result<(), BridgeError> {
                state.domains.air.sample_rate = 44_100;
                Ok(())
            },
        );
        assert!(matches!(
            result,
            Err(BridgeError::TouchedDomainMismatch { .. })
        ));
    }

    #[test]
    fn unmapped_renderable_track_is_rejected() {
        let mut project = DawProject::new("Test", 48_000, 120.0).unwrap();
        let result = project.transact(
            "Bad track",
            0,
            touched(&[ProjectDomain::Arrangement]),
            |state| -> Result<(), BridgeError> {
                let mut editor = ArrangementEditor::from_state(state.domains.arrangement.clone())
                    .map_err(|error| BridgeError::Domain(error.to_string()))?;
                editor
                    .create_track("No route", TrackKind::Audio)
                    .map_err(|error| BridgeError::Domain(error.to_string()))?;
                state.domains.arrangement = editor.state().clone();
                Ok(())
            },
        );
        assert!(matches!(result, Err(BridgeError::InvalidProject(_))));
        assert_eq!(project.revisions().aggregate, 0);
    }

    #[test]
    fn save_intent_uses_explicit_versioned_sections() {
        let project = DawProject::new("Test", 48_000, 120.0).unwrap();
        let intent = project.save_intent();
        assert_eq!(intent.schema_version, DAW_PROJECT_SCHEMA_VERSION);
        assert_eq!(intent.sections.len(), 7);
        assert!(intent.bindings.is_empty());
        assert!(intent
            .sections
            .iter()
            .all(|section| !section.payload_key.is_empty()));
    }

    #[test]
    fn bridge_aliases_are_idempotent_but_never_share_a_domain_type() {
        let mut bindings = ProjectBindings::default();
        let media_asset = assets::AssetId(41);

        // The values deliberately happen to have the same raw number. Their
        // types make an accidental cross-domain substitution impossible, and
        // the maps preserve two independent aliases for the two consumers.
        let arrangement_alias = bindings.bind_media_asset(media_asset).unwrap();
        let sample_alias = bindings.bind_sequencer_sample(media_asset).unwrap();
        assert_eq!(arrangement_alias.get(), 1);
        assert_eq!(sample_alias.get(), 1);
        assert_eq!(
            bindings.bind_media_asset(media_asset).unwrap(),
            arrangement_alias
        );
        assert_eq!(
            bindings.bind_sequencer_sample(media_asset).unwrap(),
            sample_alias
        );
        assert_eq!(bindings.assets.arrangement_assets.len(), 1);
        assert_eq!(bindings.assets.sequencer_samples.len(), 1);
    }
}
