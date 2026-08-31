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
use std::error::Error;
use std::fmt;

use crate::arrangement::{self};
use crate::assets::{self, AssetAvailability, AssetUsage, MediaAsset};
use crate::automation::{self, AutomationCommand};
pub use crate::change_set::ChangeSet;
use crate::daw_project::{
    BridgeError, DawProject, ProjectBindings, ProjectDomain, ProjectRevisions, ProjectState,
};
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
pub type ArrangementCommand = arrangement::ArrangementOperation;

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

#[derive(Clone, Debug)]
pub enum AirCommand {
    PutSource {
        before: Option<ontology::AudioSource>,
        after: Option<ontology::AudioSource>,
    },
    PutSpan {
        before: Option<ontology::SourceSpan>,
        after: Option<ontology::SourceSpan>,
    },
    PutObject {
        before: Option<ontology::AuditoryObject>,
        after: Option<ontology::AuditoryObject>,
    },
    PutTransform {
        before: Option<ontology::Transform>,
        after: Option<ontology::Transform>,
    },
    PutParameter {
        before: Option<ontology::Parameter>,
        after: Option<ontology::Parameter>,
    },
    PutAutomation {
        before: Option<ontology::Automation>,
        after: Option<ontology::Automation>,
    },
    PutModulation {
        before: Option<ontology::Modulation>,
        after: Option<ontology::Modulation>,
    },
    PutRelation {
        before: Option<ontology::ObjectRelation>,
        after: Option<ontology::ObjectRelation>,
    },
    PutEvidence {
        before: Option<ontology::Evidence>,
        after: Option<ontology::Evidence>,
    },
    PutHypothesis {
        before: Option<ontology::Hypothesis>,
        after: Option<ontology::Hypothesis>,
    },
    PutHypothesisSet {
        before: Option<ontology::HypothesisSet>,
        after: Option<ontology::HypothesisSet>,
    },
}

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
    Precondition {
        command_index: usize,
        detail: String,
    },
    /// The underlying domain or bridge rejected the mutation.
    Domain(String),
    /// A claimed ID was already allocated or a needed claim was missing.
    IdClaim(String),
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RevisionConflict { expected, actual } => write!(
                formatter,
                "command revision conflict: expected {expected}, actual {actual}"
            ),
            Self::Precondition {
                command_index,
                detail,
            } => write!(
                formatter,
                "command {command_index} precondition failed: {detail}"
            ),
            Self::Domain(detail) => write!(formatter, "command domain error: {detail}"),
            Self::IdClaim(detail) => write!(formatter, "command ID claim error: {detail}"),
        }
    }
}

impl Error for EnvelopeError {}

impl AssetCommand {
    fn inverse(&self) -> Self {
        match self {
            Self::PutAsset { id, before, after } => Self::PutAsset {
                id: *id,
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutUsage {
                asset,
                usage,
                before,
                after,
            } => Self::PutUsage {
                asset: *asset,
                usage: *usage,
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutAvailability {
                asset,
                before,
                after,
            } => Self::PutAvailability {
                asset: *asset,
                before: after.clone(),
                after: before.clone(),
            },
        }
    }
}

impl BindingCommand {
    fn inverse(&self) -> Self {
        match self {
            Self::PutMediaAssetAlias {
                alias,
                before,
                after,
            } => Self::PutMediaAssetAlias {
                alias: *alias,
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutSequencerSampleAlias {
                alias,
                before,
                after,
            } => Self::PutSequencerSampleAlias {
                alias: *alias,
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutPatternDefinitionAlias {
                alias,
                before,
                after,
            } => Self::PutPatternDefinitionAlias {
                alias: *alias,
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutPatternPlacement {
                clip,
                before,
                after,
            } => Self::PutPatternPlacement {
                clip: *clip,
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutAutomationLaneAlias {
                alias,
                before,
                after,
            } => Self::PutAutomationLaneAlias {
                alias: *alias,
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutTrackBus {
                track,
                before,
                after,
            } => Self::PutTrackBus {
                track: *track,
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutClipBusOverride {
                clip,
                before,
                after,
            } => Self::PutClipBusOverride {
                clip: *clip,
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutClipObjectLink {
                clip,
                before,
                after,
            } => Self::PutClipObjectLink {
                clip: *clip,
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutAssetSourceLink {
                asset,
                before,
                after,
            } => Self::PutAssetSourceLink {
                asset: *asset,
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutAutomationParameterLink {
                lane,
                before,
                after,
            } => Self::PutAutomationParameterLink {
                lane: *lane,
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutPatternObjectLink {
                pattern,
                before,
                after,
            } => Self::PutPatternObjectLink {
                pattern: *pattern,
                before: after.clone(),
                after: before.clone(),
            },
        }
    }
}

impl AirCommand {
    fn inverse(&self) -> Self {
        match self {
            Self::PutSource { before, after } => Self::PutSource {
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutSpan { before, after } => Self::PutSpan {
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutObject { before, after } => Self::PutObject {
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutTransform { before, after } => Self::PutTransform {
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutParameter { before, after } => Self::PutParameter {
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutAutomation { before, after } => Self::PutAutomation {
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutModulation { before, after } => Self::PutModulation {
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutRelation { before, after } => Self::PutRelation {
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutEvidence { before, after } => Self::PutEvidence {
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutHypothesis { before, after } => Self::PutHypothesis {
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutHypothesisSet { before, after } => Self::PutHypothesisSet {
                before: after.clone(),
                after: before.clone(),
            },
        }
    }
}

impl DomainCommand {
    fn inverse(&self) -> Self {
        match self {
            Self::Arrangement(command) => Self::Arrangement(command.inverse()),
            Self::Sequencer(command) => Self::Sequencer(command.inverse()),
            Self::Automation(command) => Self::Automation(command.inverse()),
            Self::Mixer(command) => Self::Mixer(command.inverse()),
            Self::Assets(command) => Self::Assets(command.inverse()),
            Self::Bindings(command) => Self::Bindings(command.inverse()),
            Self::Air(command) => Self::Air(command.inverse()),
        }
    }
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
        let actual = project.revisions().aggregate;
        if self.base_revision != actual {
            return Err(EnvelopeError::RevisionConflict {
                expected: self.base_revision,
                actual,
            });
        }
        let touched = self.touched_domains();

        // Preserve command-indexed conflicts before `prepare_transaction`
        // intentionally erases the mutation error type at its public boundary.
        let mut preflight = project.state().clone();
        for (command_index, command) in self.commands.iter().enumerate() {
            apply_domain_command(&mut preflight, command).map_err(|detail| {
                EnvelopeError::Precondition {
                    command_index,
                    detail,
                }
            })?;
        }

        let commands = self.commands.clone();
        let prepared = project
            .prepare_transaction(
                self.label.clone(),
                self.base_revision,
                touched.clone(),
                move |state| {
                    for command in &commands {
                        apply_domain_command(state, command)?;
                    }
                    Ok::<_, String>(())
                },
            )
            .map_err(map_bridge_error)?;
        project
            .commit_prepared(prepared)
            .map_err(map_bridge_error)?;

        let revisions = project.revisions();
        let inverse = CommandEnvelope {
            label: format!("Undo {}", self.label),
            base_revision: revisions.aggregate,
            coalesce: None,
            commands: self
                .commands
                .iter()
                .rev()
                .map(DomainCommand::inverse)
                .collect(),
            id_claims: self.id_claims.clone(),
        };
        let mut change_set = ChangeSet {
            domains: touched.clone(),
            routing_changed: touched.contains(&ProjectDomain::Mixer)
                || touched.contains(&ProjectDomain::Bindings),
            ..ChangeSet::default()
        };
        if touched.iter().any(|domain| *domain != ProjectDomain::Air) {
            change_set.invalidate_bus(project.state().domains.mixer.master());
        }
        Ok(AppliedEnvelope {
            envelope: self,
            inverse,
            revisions,
            change_set,
        })
    }
}

fn map_bridge_error(error: BridgeError) -> EnvelopeError {
    match error {
        BridgeError::RevisionConflict { expected, actual } => {
            EnvelopeError::RevisionConflict { expected, actual }
        }
        other => EnvelopeError::Domain(other.to_string()),
    }
}

fn apply_domain_command(state: &mut ProjectState, command: &DomainCommand) -> Result<(), String> {
    match command {
        DomainCommand::Arrangement(command) => state
            .domains
            .arrangement
            .apply_operations(std::slice::from_ref(command))
            .map_err(|error| error.to_string()),
        DomainCommand::Sequencer(command) => state
            .domains
            .sequencer
            .apply_without_history(std::slice::from_ref(command))
            .map(|_| ())
            .map_err(|error| error.to_string()),
        DomainCommand::Automation(command) => state
            .domains
            .automation
            .apply(command)
            .map(|_| ())
            .map_err(|error| error.to_string()),
        DomainCommand::Mixer(command) => command
            .apply(&mut state.domains.mixer)
            .map_err(|error| error.to_string()),
        DomainCommand::Assets(command) => apply_asset_command(&mut state.domains.assets, command),
        DomainCommand::Bindings(command) => apply_binding_command(&mut state.bindings, command),
        DomainCommand::Air(command) => apply_air_command(&mut state.domains.air, command),
    }
}

fn apply_asset_command(
    registry: &mut assets::AssetRegistry,
    command: &AssetCommand,
) -> Result<(), String> {
    match command {
        AssetCommand::PutAsset { id, before, after } => registry
            .put_asset(*id, before.as_ref(), after.clone())
            .map_err(|error| error.to_string()),
        AssetCommand::PutUsage {
            asset,
            usage,
            before,
            after,
        } => registry
            .put_usage(*asset, *usage, before.as_ref(), after.clone())
            .map_err(|error| error.to_string()),
        AssetCommand::PutAvailability {
            asset,
            before,
            after,
        } => registry
            .put_availability(*asset, before, after.clone())
            .map_err(|error| error.to_string()),
    }
}

fn apply_binding_command(
    bindings: &mut ProjectBindings,
    command: &BindingCommand,
) -> Result<(), String> {
    macro_rules! put {
        ($map:expr, $key:expr, $before:expr, $after:expr) => {
            put_map($map, $key, $before, $after)
        };
    }
    let alias_cursor = match command {
        BindingCommand::PutMediaAssetAlias {
            alias,
            before,
            after,
        } => {
            put!(
                &mut bindings.assets.arrangement_assets,
                *alias,
                before,
                after
            )?;
            Some((0, alias.get()))
        }
        BindingCommand::PutSequencerSampleAlias {
            alias,
            before,
            after,
        } => {
            put!(
                &mut bindings.assets.sequencer_samples,
                *alias,
                before,
                after
            )?;
            Some((1, alias.get()))
        }
        BindingCommand::PutPatternDefinitionAlias {
            alias,
            before,
            after,
        } => {
            put!(&mut bindings.patterns.definitions, *alias, before, after)?;
            Some((2, alias.get()))
        }
        BindingCommand::PutPatternPlacement {
            clip,
            before,
            after,
        } => {
            put!(&mut bindings.patterns.placements, *clip, before, after)?;
            None
        }
        BindingCommand::PutAutomationLaneAlias {
            alias,
            before,
            after,
        } => {
            put!(&mut bindings.automation.lanes, *alias, before, after)?;
            Some((3, alias.get()))
        }
        BindingCommand::PutTrackBus {
            track,
            before,
            after,
        } => {
            put!(&mut bindings.mixer.tracks, *track, before, after)?;
            None
        }
        BindingCommand::PutClipBusOverride {
            clip,
            before,
            after,
        } => {
            put!(&mut bindings.mixer.clip_overrides, *clip, before, after)?;
            None
        }
        BindingCommand::PutClipObjectLink {
            clip,
            before,
            after,
        } => {
            put!(&mut bindings.air.clips, *clip, before, after)?;
            None
        }
        BindingCommand::PutAssetSourceLink {
            asset,
            before,
            after,
        } => {
            put!(&mut bindings.air.assets, *asset, before, after)?;
            None
        }
        BindingCommand::PutAutomationParameterLink {
            lane,
            before,
            after,
        } => {
            put!(&mut bindings.air.automation_lanes, *lane, before, after)?;
            None
        }
        BindingCommand::PutPatternObjectLink {
            pattern,
            before,
            after,
        } => {
            put!(&mut bindings.air.patterns, *pattern, before, after)?;
            None
        }
    };
    if let Some((space, id)) = alias_cursor {
        let mut cursors = bindings.allocator_state();
        let next = id
            .checked_add(1)
            .ok_or_else(|| "binding alias ID exhausted".to_owned())?;
        match space {
            0 => cursors.next_arrangement_asset = cursors.next_arrangement_asset.max(next),
            1 => cursors.next_sequencer_sample = cursors.next_sequencer_sample.max(next),
            2 => cursors.next_arrangement_pattern = cursors.next_arrangement_pattern.max(next),
            3 => cursors.next_arrangement_parameter = cursors.next_arrangement_parameter.max(next),
            _ => unreachable!("binding alias spaces are exhaustive"),
        }
        bindings
            .restore_allocator_state(cursors)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn put_map<K, V>(
    map: &mut BTreeMap<K, V>,
    key: K,
    before: &Option<V>,
    after: &Option<V>,
) -> Result<(), String>
where
    K: Copy + Ord,
    V: Clone + PartialEq,
{
    if before.is_none() && after.is_none() {
        return Err("empty put".into());
    }
    if map.get(&key) != before.as_ref() {
        return Err("put before-state does not match project".into());
    }
    match after {
        Some(value) => {
            map.insert(key, value.clone());
        }
        None => {
            map.remove(&key);
        }
    }
    Ok(())
}

fn apply_air_command(air: &mut ontology::AuditoryIr, command: &AirCommand) -> Result<(), String> {
    macro_rules! put {
        ($map:expr, $before:expr, $after:expr, $field:ident) => {
            put_air($map, $before, $after, |value| value.$field)
        };
    }
    match command {
        AirCommand::PutSource { before, after } => put!(&mut air.sources, before, after, id),
        AirCommand::PutSpan { before, after } => put!(&mut air.spans, before, after, id),
        AirCommand::PutObject { before, after } => put!(&mut air.objects, before, after, id),
        AirCommand::PutTransform { before, after } => {
            put!(&mut air.transforms, before, after, id)
        }
        AirCommand::PutParameter { before, after } => {
            put!(&mut air.parameters, before, after, id)
        }
        AirCommand::PutAutomation { before, after } => {
            put!(&mut air.automations, before, after, id)
        }
        AirCommand::PutModulation { before, after } => {
            put!(&mut air.modulations, before, after, id)
        }
        AirCommand::PutRelation { before, after } => put!(&mut air.relations, before, after, id),
        AirCommand::PutEvidence { before, after } => put!(&mut air.evidence, before, after, id),
        AirCommand::PutHypothesis { before, after } => {
            put!(&mut air.hypotheses, before, after, id)
        }
        AirCommand::PutHypothesisSet { before, after } => {
            put!(&mut air.hypothesis_sets, before, after, id)
        }
    }
}

fn put_air<K, V>(
    map: &mut BTreeMap<K, V>,
    before: &Option<V>,
    after: &Option<V>,
    id: impl Fn(&V) -> K,
) -> Result<(), String>
where
    K: Copy + Ord + PartialEq,
    V: Clone + PartialEq,
{
    let key = before
        .as_ref()
        .map(&id)
        .or_else(|| after.as_ref().map(&id))
        .ok_or_else(|| "empty AIR put".to_owned())?;
    if before.as_ref().is_some_and(|value| id(value) != key)
        || after.as_ref().is_some_and(|value| id(value) != key)
    {
        return Err("AIR put identity mismatch".into());
    }
    put_map(map, key, before, after)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: u64) -> ontology::AudioSource {
        ontology::AudioSource {
            id: ontology::SourceId::new(id),
            uri: format!("memory://source/{id}"),
            content_digest: Some(format!("sha256:test-{id}")),
            sample_rate: 48_000,
            channels: 2,
            frame_count: 48_000,
        }
    }

    #[test]
    fn aggregate_envelope_applies_and_its_inverse_reverts() {
        let mut project = DawProject::new("Commands", 48_000, 120.0).unwrap();
        let entity = source(9);
        let applied = CommandEnvelope {
            label: "Add AIR source".into(),
            base_revision: 0,
            coalesce: None,
            commands: vec![DomainCommand::Air(AirCommand::PutSource {
                before: None,
                after: Some(entity.clone()),
            })],
            id_claims: IdClaims::default(),
        }
        .apply(&mut project)
        .unwrap();

        assert_eq!(applied.revisions.aggregate, 1);
        assert_eq!(applied.revisions.air, 1);
        assert_eq!(
            project.state().domains.air.sources.get(&entity.id),
            Some(&entity)
        );
        assert!(applied.change_set.audio.is_empty());

        let reverted = applied.inverse.apply(&mut project).unwrap();
        assert_eq!(reverted.revisions.aggregate, 2);
        assert!(!project.state().domains.air.sources.contains_key(&entity.id));
    }

    #[test]
    fn stale_before_state_is_reported_with_command_index() {
        let mut project = DawProject::new("Commands", 48_000, 120.0).unwrap();
        let error = CommandEnvelope {
            label: "Stale".into(),
            base_revision: 0,
            coalesce: None,
            commands: vec![DomainCommand::Air(AirCommand::PutSource {
                before: Some(source(3)),
                after: None,
            })],
            id_claims: IdClaims::default(),
        }
        .apply(&mut project)
        .unwrap_err();
        assert!(matches!(
            error,
            EnvelopeError::Precondition {
                command_index: 0,
                ..
            }
        ));
        assert_eq!(project.revisions().aggregate, 0);
    }
}
