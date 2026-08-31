//! Aggregate command envelope: the serializable edit language (skeleton).
//!
//! Normative design: `docs/COMMAND_ENVELOPE.md`. Implementation: ENVELOPE
//! workstream in `docs/SWARM_PLAN.md`. This skeleton exists so every lane
//! compiles against real types instead of prose; `todo!()` bodies are the
//! ENVELOPE lane's work and nothing else may call them until they land.
//!
//! An envelope routes through `DawProject::prepare_transaction`; it never
//! bypasses domain validation. It is not a scripting surface: terms are data,
//! application is atomic, and every applied envelope has an exact inverse.
#![allow(dead_code, unused_variables)]

use std::collections::{BTreeMap, BTreeSet};

use crate::arrangement::{self, Clip, Track};
use crate::assets::{self, AssetAvailability, AssetUsage, MediaAsset};
use crate::automation::{self, AutomationCommand};
use crate::daw_project::{DawProject, ProjectDomain, ProjectRevisions};
use crate::mixer::{self, MixerCommand};
use crate::ontology;
use crate::sequencer::{self, SequencerCommand};

/// One user-meaningful, atomic, invertible aggregate edit.
///
/// Serialization is deliberately not derived here: the codec era keeps domain
/// types serde-free and rebuilds through checked APIs. The envelope's durable
/// form is a versioned DTO in `project_codecs`, added by the ENVELOPE lane.
#[derive(Clone, Debug)]
pub struct CommandEnvelope {
    /// Undo-menu label, e.g. "Move 3 clips", "Apply rhythm proposal 4".
    pub label: String,
    /// Aggregate revision this envelope was built against.
    pub base_revision: u64,
    /// Same-key envelopes within a coalescing window merge on the undo stack.
    pub coalesce: Option<CoalesceKey>,
    /// Ordered domain commands. Application is all-or-nothing.
    pub commands: Vec<DomainCommand>,
    /// Every ID this envelope allocates, claimed explicitly up front so
    /// journal replay is deterministic.
    pub id_claims: IdClaims,
}

#[derive(Clone, Debug)]
pub enum DomainCommand {
    Arrangement(ArrangementCommand),
    Sequencer(SequencerCommand),
    Automation(AutomationCommand),
    Mixer(MixerCommand),
    Assets(AssetCommand),
    Bindings(BindingCommand),
    Air(AirCommand),
}

/// Put-style arrangement edits. `before: None` creates, `after: None`
/// deletes, both `Some` replaces; a mismatched `before` is a conflict error.
/// Granularity is one addressable entity, never a whole domain snapshot.
#[derive(Clone, Debug)]
pub enum ArrangementCommand {
    PutTrack {
        before: Option<Track>,
        after: Option<Track>,
    },
    PutClip {
        before: Option<Clip>,
        after: Option<Clip>,
    },
    PutTrackOrder {
        before: Vec<arrangement::TrackId>,
        after: Vec<arrangement::TrackId>,
    },
}

/// Put-style media-pool edits over the registry's own record types.
#[derive(Clone, Debug)]
pub enum AssetCommand {
    PutAsset {
        id: assets::AssetId,
        before: Option<MediaAsset>,
        after: Option<MediaAsset>,
    },
    PutUsage {
        asset: assets::AssetId,
        usage: assets::AssetUsageId,
        before: Option<AssetUsage>,
        after: Option<AssetUsage>,
    },
    PutAvailability {
        asset: assets::AssetId,
        before: AssetAvailability,
        after: AssetAvailability,
    },
}

/// Single-entry puts over the typed binding maps in
/// `daw_project::ProjectBindings`. Bindings are the only place identities
/// cross domains, so binding edits are first-class commands, not side
/// effects.
#[derive(Clone, Debug)]
pub enum BindingCommand {
    PutMediaAssetAlias {
        alias: arrangement::AssetId,
        before: Option<assets::AssetId>,
        after: Option<assets::AssetId>,
    },
    PutSequencerSampleAlias {
        alias: sequencer::SampleAssetId,
        before: Option<assets::AssetId>,
        after: Option<assets::AssetId>,
    },
    PutPatternDefinitionAlias {
        alias: arrangement::PatternId,
        before: Option<sequencer::PatternId>,
        after: Option<sequencer::PatternId>,
    },
    PutPatternPlacement {
        clip: arrangement::ClipId,
        before: Option<sequencer::PatternClipId>,
        after: Option<sequencer::PatternClipId>,
    },
    PutAutomationLaneAlias {
        alias: arrangement::ParameterId,
        before: Option<automation::AutomationLaneId>,
        after: Option<automation::AutomationLaneId>,
    },
    PutTrackBus {
        track: arrangement::TrackId,
        before: Option<mixer::BusId>,
        after: Option<mixer::BusId>,
    },
    PutClipBusOverride {
        clip: arrangement::ClipId,
        before: Option<mixer::BusId>,
        after: Option<mixer::BusId>,
    },
    PutClipObjectLink {
        clip: arrangement::ClipId,
        before: Option<ontology::ObjectId>,
        after: Option<ontology::ObjectId>,
    },
    PutAssetSourceLink {
        asset: assets::AssetId,
        before: Option<ontology::SourceId>,
        after: Option<ontology::SourceId>,
    },
    PutAutomationParameterLink {
        lane: automation::AutomationLaneId,
        before: Option<ontology::ParameterId>,
        after: Option<ontology::ParameterId>,
    },
    PutPatternObjectLink {
        pattern: sequencer::PatternId,
        before: Option<ontology::ObjectId>,
        after: Option<ontology::ObjectId>,
    },
}

/// AIR edits. Deliberately uninhabited in the skeleton: the ENVELOPE lane
/// specifies put-style claim/object/relation edits against `ontology`'s real
/// entity types rather than this file guessing their shapes.
#[derive(Clone, Debug)]
pub enum AirCommand {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoalesceKey {
    /// Stable hash of (editor id, gesture kind, primary target id).
    pub key: u64,
}

/// Explicit allocation claims, filled from the project's serialized monotonic
/// allocators before application. Replay must not consult runtime state.
#[derive(Clone, Debug, Default)]
pub struct IdClaims {
    pub arrangement_tracks: Vec<arrangement::TrackId>,
    pub arrangement_clips: Vec<arrangement::ClipId>,
    pub sequencer_patterns: Vec<sequencer::PatternId>,
    pub sequencer_clips: Vec<sequencer::PatternClipId>,
    pub sequencer_lanes: Vec<sequencer::StepLaneId>,
    pub sequencer_notes: Vec<sequencer::NoteId>,
    pub automation_lanes: Vec<automation::AutomationLaneId>,
    pub mixer_buses: Vec<mixer::BusId>,
    pub assets: Vec<assets::AssetId>,
    pub asset_usages: Vec<assets::AssetUsageId>,
    /// Raw values for the binding-alias allocators in `ProjectBindings`.
    pub binding_aliases: Vec<u64>,
    /// Foreign-namespace claims for reading imports (`docs/READINGS.md`):
    /// an imported entity keeps its `(reading, local)` name forever, so
    /// import replay claims those names instead of minting local ones. One
    /// reading import is one envelope; the envelope label is the batch.
    pub imported: Vec<ForeignId>,
}

/// A two-level entity name from another reading's namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ForeignId {
    /// The originating reading's stable identity.
    pub reading: u128,
    /// The entity's ID inside that reading.
    pub local: u64,
}

/// What an envelope touched, at cache-invalidation granularity. Coarse is
/// legal; wrong is not: a change set MUST cover every audible consequence.
/// This is the contract `docs/RENDER_TILES.md` consumes.
#[derive(Clone, Debug, Default)]
pub struct ChangeSet {
    pub domains: BTreeSet<ProjectDomain>,
    /// Half-open project-frame ranges whose audio may have changed, per bus.
    /// `None` means "the whole bus".
    pub audio: BTreeMap<mixer::BusId, Option<Vec<(i64, i64)>>>,
    /// Structural mixer/routing change: all downstream audio is dirty.
    pub routing_changed: bool,
}

#[derive(Clone, Debug)]
pub struct AppliedEnvelope {
    pub envelope: CommandEnvelope,
    /// The exact inverse: commands reversed, each put swapped.
    pub inverse: CommandEnvelope,
    pub revisions: ProjectRevisions,
    pub change_set: ChangeSet,
}

#[derive(Debug)]
pub enum EnvelopeError {
    /// `base_revision` no longer matches the aggregate.
    RevisionConflict { expected: u64, actual: u64 },
    /// A put's `before` did not match the current entity.
    Precondition { command_index: usize, detail: String },
    /// The underlying domain or bridge rejected the mutation.
    Domain(String),
    /// A claimed ID was already allocated or a needed claim was missing.
    IdClaim(String),
}

impl CommandEnvelope {
    /// The touched-domain set is derived mechanically from the command list;
    /// callers never declare it.
    pub fn touched_domains(&self) -> BTreeSet<ProjectDomain> {
        self.commands
            .iter()
            .map(|command| match command {
                DomainCommand::Arrangement(_) => ProjectDomain::Arrangement,
                DomainCommand::Sequencer(_) => ProjectDomain::Sequencer,
                DomainCommand::Automation(_) => ProjectDomain::Automation,
                DomainCommand::Mixer(_) => ProjectDomain::Mixer,
                DomainCommand::Assets(_) => ProjectDomain::Assets,
                DomainCommand::Bindings(_) => ProjectDomain::Bindings,
                DomainCommand::Air(_) => ProjectDomain::Air,
            })
            .collect()
    }

    /// Apply atomically through `DawProject`'s validated transaction path,
    /// returning the applied record with inverse and change set.
    ///
    /// ENVELOPE lane. Must route through `prepare_transaction` /
    /// `commit_prepared`; must verify preconditions before mutation; must
    /// never partially apply.
    pub fn apply(self, project: &mut DawProject) -> Result<AppliedEnvelope, EnvelopeError> {
        todo!("ENVELOPE lane: docs/COMMAND_ENVELOPE.md")
    }
}
