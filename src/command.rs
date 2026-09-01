//! Aggregate command envelope: the project's single edit language.
//!
//! Normative design: `docs/COMMAND_ENVELOPE.md`. Implementation: ENVELOPE
//! workstream in `docs/SWARM_PLAN.md`.
//!
//! An envelope routes through `DawProject::prepare_transaction`; it never
//! bypasses domain validation. It is not a scripting surface: terms are data,
//! application is atomic, and every applied envelope has an exact inverse.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::arrangement::{self};
use crate::assets::{self, AssetAvailability, AssetUsage, MediaAsset};
use crate::automation::{self, AutomationCommand};
pub use crate::change_set::{AudioRange, ChangeSet};
pub use crate::command_record::{
    AirAddress, AirEntityKind, BindingAddress, BindingAliasKind, CoalesceToken, CommandAddress,
    IdClaim,
};
use crate::daw_project::{
    BridgeError, DawProject, ProjectBindings, ProjectDomain, ProjectRevisions, ProjectState,
};
use crate::mixer::{self, MixerCommand};
use crate::ontology;
use crate::sample_kit::SampleKitPut;
use crate::sequencer::{self, SequencerCommand};

/// One user-meaningful, atomic, invertible aggregate edit.
///
/// Serialization is deliberately not derived here: the codec era keeps domain
/// types serde-free and rebuilds through checked APIs. The envelope's durable
/// form is a versioned DTO in `project_codecs`, added by the ENVELOPE lane.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandEnvelope {
    /// Undo-menu label, e.g. "Move 3 clips", "Apply rhythm proposal 4".
    pub label: String,
    /// Aggregate revision this envelope was built against.
    pub base_revision: u64,
    /// Same-key envelopes within a coalescing window merge on the undo stack.
    pub coalesce: Option<CoalesceToken>,
    /// Ordered domain commands. Application is all-or-nothing.
    pub commands: Vec<DomainCommand>,
    /// Every ID this envelope allocates, claimed explicitly up front so
    /// journal replay is deterministic.
    pub id_claims: BTreeSet<IdClaim>,
}

/// The revision-independent form retained by history and journal records.
pub type CommandBatch = crate::command_record::CommandBatch<DomainCommand>;

#[derive(Clone, Debug, PartialEq)]
pub enum DomainCommand {
    Arrangement(ArrangementCommand),
    Sequencer(SequencerCommand),
    Automation(AutomationCommand),
    Mixer(MixerCommand),
    SampleKits(SampleKitPut),
    Assets(AssetCommand),
    Bindings(BindingCommand),
    Air(AirCommand),
}

/// Put-style arrangement edits. `before: None` creates, `after: None`
/// deletes, both `Some` replaces; a mismatched `before` is a conflict error.
/// Granularity is one addressable entity, never a whole domain snapshot.
pub type ArrangementCommand = arrangement::ArrangementOperation;

/// Put-style media-pool edits over the registry's own record types.
#[derive(Clone, Debug, PartialEq)]
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
#[derive(Clone, Debug, PartialEq)]
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
    PutSampleTargetAlias {
        alias: sequencer::SampleAssetId,
        before: Option<crate::sample_kit::SampleTargetRef>,
        after: Option<crate::sample_kit::SampleTargetRef>,
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

#[derive(Clone, Debug, PartialEq)]
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

/// Compatibility adapter for callers which have not yet adopted the
/// address-bearing [`CoalesceToken`]. New code should not use this type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoalesceKey {
    /// Stable hash of (editor id, gesture kind, primary target id).
    pub key: u64,
}

/// Compatibility name for the typed claim set used by [`CommandBatch`].
pub type IdClaims = BTreeSet<IdClaim>;

#[derive(Clone, Debug, PartialEq)]
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
            Self::PutSampleTargetAlias {
                alias,
                before,
                after,
            } => Self::PutSampleTargetAlias {
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
    pub fn inverse(&self) -> Self {
        match self {
            Self::Arrangement(command) => Self::Arrangement(command.inverse()),
            Self::Sequencer(command) => Self::Sequencer(command.inverse()),
            Self::Automation(command) => Self::Automation(command.inverse()),
            Self::Mixer(command) => Self::Mixer(command.inverse()),
            Self::SampleKits(command) => Self::SampleKits(command.inverse()),
            Self::Assets(command) => Self::Assets(command.inverse()),
            Self::Bindings(command) => Self::Bindings(command.inverse()),
            Self::Air(command) => Self::Air(command.inverse()),
        }
    }

    pub fn domain(&self) -> ProjectDomain {
        match self {
            Self::Arrangement(_) => ProjectDomain::Arrangement,
            Self::Sequencer(_) => ProjectDomain::Sequencer,
            Self::Automation(_) => ProjectDomain::Automation,
            Self::Mixer(_) => ProjectDomain::Mixer,
            Self::SampleKits(_) => ProjectDomain::SampleKits,
            Self::Assets(_) => ProjectDomain::Assets,
            Self::Bindings(_) => ProjectDomain::Bindings,
            Self::Air(_) => ProjectDomain::Air,
        }
    }

    /// Stable addresses affected by this command. History uses these to
    /// ensure a shared gesture token cannot merge unrelated edits.
    pub fn addresses(&self) -> BTreeSet<CommandAddress> {
        let mut addresses = BTreeSet::new();
        match self {
            Self::Arrangement(command) => match command {
                ArrangementCommand::PutTrack { before, after } => {
                    if let Some(track) = before.as_ref().or(after.as_ref()) {
                        addresses.insert(CommandAddress::ArrangementTrack(track.id));
                    }
                }
                ArrangementCommand::PutClip { before, after } => {
                    if let Some(clip) = before.as_ref().or(after.as_ref()) {
                        addresses.insert(CommandAddress::ArrangementClip(clip.id));
                    }
                }
                ArrangementCommand::SetTrackOrder { .. } => {
                    addresses.insert(CommandAddress::ArrangementTrackOrder);
                }
            },
            Self::Sequencer(command) => match command {
                SequencerCommand::PutPattern { before, after } => {
                    if let Some(pattern) = before.as_ref().or(after.as_ref()) {
                        addresses.insert(CommandAddress::SequencerPattern(pattern.id));
                    }
                }
                SequencerCommand::PutClip { before, after } => {
                    if let Some(clip) = before.as_ref().or(after.as_ref()) {
                        addresses.insert(CommandAddress::SequencerClip(clip.id));
                    }
                }
                SequencerCommand::SetTempoMap { .. } => {
                    addresses.insert(CommandAddress::SequencerTempoMap);
                }
            },
            Self::Automation(command) => {
                for change in &command.changes {
                    if let Some(lane) = change.before.as_ref().or(change.after.as_ref()) {
                        addresses.insert(CommandAddress::AutomationLane(lane.id));
                        for point in lane.points() {
                            addresses.insert(CommandAddress::AutomationPoint(point.id));
                        }
                    }
                }
            }
            Self::Mixer(_) => {
                addresses.insert(CommandAddress::WholeDomain(ProjectDomain::Mixer));
            }
            Self::SampleKits(command) => {
                if let Ok(id) = command.id() {
                    addresses.insert(CommandAddress::SampleKit(id));
                }
            }
            Self::Assets(command) => match command {
                AssetCommand::PutAsset { id, .. }
                | AssetCommand::PutAvailability { asset: id, .. } => {
                    addresses.insert(CommandAddress::Asset(*id));
                }
                AssetCommand::PutUsage { asset, usage, .. } => {
                    addresses.insert(CommandAddress::AssetUsage {
                        asset: *asset,
                        usage: *usage,
                    });
                }
            },
            Self::Bindings(command) => {
                let address = match command {
                    BindingCommand::PutMediaAssetAlias { alias, .. } => {
                        BindingAddress::ArrangementAsset(*alias)
                    }
                    BindingCommand::PutSequencerSampleAlias { alias, .. } => {
                        BindingAddress::SequencerSample(*alias)
                    }
                    BindingCommand::PutSampleTargetAlias { alias, .. } => {
                        BindingAddress::SequencerSample(*alias)
                    }
                    BindingCommand::PutPatternDefinitionAlias { alias, .. } => {
                        BindingAddress::ArrangementPattern(*alias)
                    }
                    BindingCommand::PutPatternPlacement { clip, .. } => {
                        BindingAddress::PatternPlacement(*clip)
                    }
                    BindingCommand::PutAutomationLaneAlias { alias, .. } => {
                        BindingAddress::ArrangementParameter(*alias)
                    }
                    BindingCommand::PutTrackBus { track, .. } => BindingAddress::TrackBus(*track),
                    BindingCommand::PutClipBusOverride { clip, .. } => {
                        BindingAddress::ClipBusOverride(*clip)
                    }
                    BindingCommand::PutClipObjectLink { clip, .. } => {
                        BindingAddress::ClipObject(*clip)
                    }
                    BindingCommand::PutAssetSourceLink { asset, .. } => {
                        BindingAddress::AssetSource(*asset)
                    }
                    BindingCommand::PutAutomationParameterLink { lane, .. } => {
                        BindingAddress::AutomationParameter(*lane)
                    }
                    BindingCommand::PutPatternObjectLink { pattern, .. } => {
                        BindingAddress::PatternObject(*pattern)
                    }
                };
                addresses.insert(CommandAddress::Binding(address));
            }
            Self::Air(command) => {
                let address = match command {
                    AirCommand::PutSource { before, after } => before
                        .as_ref()
                        .or(after.as_ref())
                        .map(|value| AirAddress::Source(value.id)),
                    AirCommand::PutSpan { before, after } => before
                        .as_ref()
                        .or(after.as_ref())
                        .map(|value| AirAddress::Span(value.id)),
                    AirCommand::PutObject { before, after } => before
                        .as_ref()
                        .or(after.as_ref())
                        .map(|value| AirAddress::Object(value.id)),
                    AirCommand::PutTransform { before, after } => before
                        .as_ref()
                        .or(after.as_ref())
                        .map(|value| AirAddress::Transform(value.id)),
                    AirCommand::PutParameter { before, after } => before
                        .as_ref()
                        .or(after.as_ref())
                        .map(|value| AirAddress::Parameter(value.id)),
                    AirCommand::PutAutomation { before, after } => before
                        .as_ref()
                        .or(after.as_ref())
                        .map(|value| AirAddress::Automation(value.id)),
                    AirCommand::PutModulation { before, after } => before
                        .as_ref()
                        .or(after.as_ref())
                        .map(|value| AirAddress::Modulation(value.id)),
                    AirCommand::PutRelation { before, after } => before
                        .as_ref()
                        .or(after.as_ref())
                        .map(|value| AirAddress::Relation(value.id)),
                    AirCommand::PutEvidence { before, after } => before
                        .as_ref()
                        .or(after.as_ref())
                        .map(|value| AirAddress::Evidence(value.id)),
                    AirCommand::PutHypothesis { before, after } => before
                        .as_ref()
                        .or(after.as_ref())
                        .map(|value| AirAddress::Hypothesis(value.id)),
                    AirCommand::PutHypothesisSet { before, after } => before
                        .as_ref()
                        .or(after.as_ref())
                        .map(|value| AirAddress::HypothesisSet(value.id)),
                };
                if let Some(address) = address {
                    addresses.insert(CommandAddress::Air(address));
                }
            }
        }
        addresses
    }

    /// Compose two consecutive same-address puts into their earliest-before /
    /// latest-after form. `None` means the command family is not safely
    /// composable and the controller must keep a separate undo entry.
    pub fn compose(&self, next: &Self) -> Option<Self> {
        match (self, next) {
            (
                Self::Arrangement(ArrangementCommand::PutTrack { before, after }),
                Self::Arrangement(ArrangementCommand::PutTrack {
                    before: next_before,
                    after: next_after,
                }),
            ) if after == next_before => Some(Self::Arrangement(ArrangementCommand::PutTrack {
                before: before.clone(),
                after: next_after.clone(),
            })),
            (
                Self::Arrangement(ArrangementCommand::PutClip { before, after }),
                Self::Arrangement(ArrangementCommand::PutClip {
                    before: next_before,
                    after: next_after,
                }),
            ) if after == next_before => Some(Self::Arrangement(ArrangementCommand::PutClip {
                before: before.clone(),
                after: next_after.clone(),
            })),
            (
                Self::Arrangement(ArrangementCommand::SetTrackOrder { before, after }),
                Self::Arrangement(ArrangementCommand::SetTrackOrder {
                    before: next_before,
                    after: next_after,
                }),
            ) if after == next_before => {
                Some(Self::Arrangement(ArrangementCommand::SetTrackOrder {
                    before: before.clone(),
                    after: next_after.clone(),
                }))
            }
            (
                Self::Sequencer(SequencerCommand::PutPattern { before, after }),
                Self::Sequencer(SequencerCommand::PutPattern {
                    before: next_before,
                    after: next_after,
                }),
            ) if after == next_before => Some(Self::Sequencer(SequencerCommand::PutPattern {
                before: before.clone(),
                after: next_after.clone(),
            })),
            (
                Self::Sequencer(SequencerCommand::PutClip { before, after }),
                Self::Sequencer(SequencerCommand::PutClip {
                    before: next_before,
                    after: next_after,
                }),
            ) if after == next_before => Some(Self::Sequencer(SequencerCommand::PutClip {
                before: before.clone(),
                after: next_after.clone(),
            })),
            (
                Self::Sequencer(SequencerCommand::SetTempoMap { before, after }),
                Self::Sequencer(SequencerCommand::SetTempoMap {
                    before: next_before,
                    after: next_after,
                }),
            ) if after == next_before => Some(Self::Sequencer(SequencerCommand::SetTempoMap {
                before: before.clone(),
                after: next_after.clone(),
            })),
            (Self::Automation(left), Self::Automation(right))
                if left.changes.len() == right.changes.len()
                    && left
                        .changes
                        .iter()
                        .zip(&right.changes)
                        .all(|(left, right)| left.after == right.before) =>
            {
                Some(Self::Automation(AutomationCommand {
                    label: right.label.clone(),
                    changes: left
                        .changes
                        .iter()
                        .zip(&right.changes)
                        .map(|(left, right)| automation::LaneChange {
                            before: left.before.clone(),
                            after: right.after.clone(),
                        })
                        .collect(),
                }))
            }
            (Self::Mixer(left), Self::Mixer(right)) => {
                left.compose_consecutive(right).map(Self::Mixer)
            }
            (Self::SampleKits(left), Self::SampleKits(right)) if left.after == right.before => {
                Some(Self::SampleKits(SampleKitPut {
                    before: left.before.clone(),
                    after: right.after.clone(),
                }))
            }
            (
                Self::Assets(AssetCommand::PutAsset { id, before, after }),
                Self::Assets(AssetCommand::PutAsset {
                    id: next_id,
                    before: next_before,
                    after: next_after,
                }),
            ) if id == next_id && after == next_before => {
                Some(Self::Assets(AssetCommand::PutAsset {
                    id: *id,
                    before: before.clone(),
                    after: next_after.clone(),
                }))
            }
            (
                Self::Assets(AssetCommand::PutUsage {
                    asset,
                    usage,
                    before,
                    after,
                }),
                Self::Assets(AssetCommand::PutUsage {
                    asset: next_asset,
                    usage: next_usage,
                    before: next_before,
                    after: next_after,
                }),
            ) if asset == next_asset && usage == next_usage && after == next_before => {
                Some(Self::Assets(AssetCommand::PutUsage {
                    asset: *asset,
                    usage: *usage,
                    before: before.clone(),
                    after: next_after.clone(),
                }))
            }
            (
                Self::Assets(AssetCommand::PutAvailability {
                    asset,
                    before,
                    after,
                }),
                Self::Assets(AssetCommand::PutAvailability {
                    asset: next_asset,
                    before: next_before,
                    after: next_after,
                }),
            ) if asset == next_asset && after == next_before => {
                Some(Self::Assets(AssetCommand::PutAvailability {
                    asset: *asset,
                    before: before.clone(),
                    after: next_after.clone(),
                }))
            }
            _ => None,
        }
    }

    pub fn is_noop(&self) -> bool {
        match self {
            Self::Arrangement(ArrangementCommand::PutTrack { before, after }) => before == after,
            Self::Arrangement(ArrangementCommand::PutClip { before, after }) => before == after,
            Self::Arrangement(ArrangementCommand::SetTrackOrder { before, after }) => {
                before == after
            }
            Self::Sequencer(SequencerCommand::PutPattern { before, after }) => before == after,
            Self::Sequencer(SequencerCommand::PutClip { before, after }) => before == after,
            Self::Sequencer(SequencerCommand::SetTempoMap { before, after }) => before == after,
            Self::Automation(command) => command
                .changes
                .iter()
                .all(|change| change.before == change.after),
            Self::Mixer(command) => command.before() == command.after(),
            Self::SampleKits(command) => command.before == command.after,
            Self::Assets(AssetCommand::PutAsset { before, after, .. }) => before == after,
            Self::Assets(AssetCommand::PutUsage { before, after, .. }) => before == after,
            Self::Assets(AssetCommand::PutAvailability { before, after, .. }) => before == after,
            _ => false,
        }
    }
}

impl CommandEnvelope {
    pub fn from_batch(base_revision: u64, batch: CommandBatch) -> Self {
        Self {
            label: batch.label,
            base_revision,
            coalesce: batch.coalesce,
            commands: batch.commands,
            id_claims: batch.id_claims,
        }
    }

    pub fn into_batch(self) -> CommandBatch {
        CommandBatch {
            label: self.label,
            coalesce: self.coalesce,
            commands: self.commands,
            id_claims: self.id_claims,
        }
    }

    pub fn as_batch(&self) -> CommandBatch {
        CommandBatch {
            label: self.label.clone(),
            coalesce: self.coalesce.clone(),
            commands: self.commands.clone(),
            id_claims: self.id_claims.clone(),
        }
    }

    /// Rebase command guards which are explicitly runtime-ephemeral after a
    /// durable checkpoint is decoded. Aggregate `base_revision`, put-style
    /// preconditions, ID claims, and durable content are unchanged.
    ///
    /// This is deliberately replay-only. Interactive callers must retain the
    /// exact optimistic guards they observed.
    pub(crate) fn rebase_ephemeral_guards_for_replay(
        self,
        project: &DawProject,
    ) -> Result<Self, EnvelopeError> {
        self.rebase_ephemeral_mixer_guards(project, "mixer replay guard")
    }

    /// Rebase only runtime-ephemeral mixer revisions before applying an
    /// authoritative Undo/Redo batch. Later mixer history applications have
    /// advanced that token even when the durable graph content has returned
    /// exactly to this entry's precondition. Durable content is still proved
    /// byte-for-byte by `MixerCommand` before any guard is changed.
    pub(crate) fn rebase_ephemeral_guards_for_history(
        self,
        project: &DawProject,
    ) -> Result<Self, EnvelopeError> {
        self.rebase_ephemeral_mixer_guards(project, "mixer history guard")
    }

    fn rebase_ephemeral_mixer_guards(
        mut self,
        project: &DawProject,
        context: &str,
    ) -> Result<Self, EnvelopeError> {
        let mut mixer = project.state().domains.mixer.clone();
        for (command_index, command) in self.commands.iter_mut().enumerate() {
            let DomainCommand::Mixer(original) = command else {
                continue;
            };
            let rebased = original
                .rebase_ephemeral_revision_for_replay(&mixer)
                .map_err(|error| EnvelopeError::Precondition {
                    command_index,
                    detail: format!("{context}: {error}"),
                })?;
            rebased
                .apply(&mut mixer)
                .map_err(|error| EnvelopeError::Precondition {
                    command_index,
                    detail: format!("{context}: {error}"),
                })?;
            *original = rebased;
        }
        Ok(self)
    }

    /// The touched-domain set is derived mechanically from the command list;
    /// callers never declare it.
    pub fn touched_domains(&self) -> BTreeSet<ProjectDomain> {
        self.commands.iter().map(DomainCommand::domain).collect()
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
        if self.label.trim().is_empty() {
            return Err(EnvelopeError::Domain("command label is empty".into()));
        }
        if self.commands.is_empty() {
            return Err(EnvelopeError::Domain("command batch is empty".into()));
        }
        let required = claims_for_commands(&self.commands);
        if let Some(missing) = required.difference(&self.id_claims).next() {
            return Err(EnvelopeError::IdClaim(format!(
                "missing allocation claim {missing:?}"
            )));
        }
        let touched = self.touched_domains();
        let before_state = project.state().clone();

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
        let inverse_commands = self
            .commands
            .iter()
            .rev()
            .map(DomainCommand::inverse)
            .collect::<Vec<_>>();
        let mut inverse_claims = self.id_claims.clone();
        inverse_claims.extend(claims_for_commands(&inverse_commands));
        let inverse = CommandEnvelope {
            label: format!("Undo {}", self.label),
            base_revision: revisions.aggregate,
            coalesce: None,
            commands: inverse_commands,
            id_claims: inverse_claims,
        };
        let change_set = derive_change_set(
            &before_state,
            project.state(),
            &self.commands,
            touched.clone(),
        );
        Ok(AppliedEnvelope {
            envelope: self,
            inverse,
            revisions,
            change_set,
        })
    }
}

/// Derive every identity introduced by a concrete command list. Builders use
/// this after lowering against a frozen snapshot so claims and commands can
/// never drift apart.
pub fn claims_for_commands(commands: &[DomainCommand]) -> BTreeSet<IdClaim> {
    let mut claims = BTreeSet::new();
    for command in commands {
        match command {
            DomainCommand::Arrangement(command) => match command {
                ArrangementCommand::PutTrack {
                    before: None,
                    after: Some(track),
                } => {
                    claims.insert(IdClaim::ArrangementTrack(track.id));
                }
                ArrangementCommand::PutClip {
                    before: None,
                    after: Some(clip),
                } => {
                    claims.insert(IdClaim::ArrangementClip(clip.id));
                }
                _ => {}
            },
            DomainCommand::Sequencer(command) => match command {
                SequencerCommand::PutPattern {
                    before,
                    after: Some(after),
                } => {
                    if before.as_ref().is_none_or(|before| before.id != after.id) {
                        claims.insert(IdClaim::SequencerPattern(after.id));
                    }
                    let (before_lanes, before_notes) = before
                        .as_ref()
                        .map_or_else(|| (BTreeSet::new(), BTreeSet::new()), sequencer_child_ids);
                    let (after_lanes, after_notes) = sequencer_child_ids(after);
                    claims.extend(
                        after_lanes
                            .difference(&before_lanes)
                            .copied()
                            .map(IdClaim::SequencerLane),
                    );
                    claims.extend(
                        after_notes
                            .difference(&before_notes)
                            .copied()
                            .map(IdClaim::SequencerNote),
                    );
                }
                SequencerCommand::PutClip {
                    before: None,
                    after: Some(clip),
                } => {
                    claims.insert(IdClaim::SequencerClip(clip.id));
                }
                _ => {}
            },
            DomainCommand::Automation(command) => {
                for change in &command.changes {
                    let before_points: BTreeSet<automation::AutomationPointId> = change
                        .before
                        .as_ref()
                        .map(|lane| lane.points().iter().map(|point| point.id).collect())
                        .unwrap_or_default();
                    if let Some(after) = &change.after {
                        if change.before.is_none() {
                            claims.insert(IdClaim::AutomationLane(after.id));
                        }
                        claims.extend(
                            after
                                .points()
                                .iter()
                                .map(|point| point.id)
                                .filter(|id| !before_points.contains(id))
                                .map(IdClaim::AutomationPoint),
                        );
                    }
                }
            }
            DomainCommand::Mixer(command) => {
                add_mixer_claims(command.before(), command.after(), &mut claims);
            }
            DomainCommand::SampleKits(command) => {
                if let Some(after) = &command.after {
                    if command.before.is_none() {
                        claims.insert(IdClaim::SampleKit(after.id));
                    }
                    let before_pads = command
                        .before
                        .as_ref()
                        .map(|kit| kit.pads.keys().copied().collect::<BTreeSet<_>>())
                        .unwrap_or_default();
                    let before_zones = command
                        .before
                        .as_ref()
                        .map(|kit| kit.zones.keys().copied().collect::<BTreeSet<_>>())
                        .unwrap_or_default();
                    claims.extend(
                        after
                            .pads
                            .keys()
                            .filter(|id| !before_pads.contains(id))
                            .copied()
                            .map(IdClaim::SamplePad),
                    );
                    claims.extend(
                        after
                            .zones
                            .keys()
                            .filter(|id| !before_zones.contains(id))
                            .copied()
                            .map(IdClaim::SampleZone),
                    );
                }
            }
            DomainCommand::Assets(command) => match command {
                AssetCommand::PutAsset {
                    before: None,
                    after: Some(_),
                    id,
                } => {
                    claims.insert(IdClaim::Asset(*id));
                }
                AssetCommand::PutUsage {
                    before: None,
                    after: Some(_),
                    usage,
                    ..
                } => {
                    claims.insert(IdClaim::AssetUsage(*usage));
                }
                _ => {}
            },
            DomainCommand::Bindings(command) => {
                let claim = match command {
                    BindingCommand::PutMediaAssetAlias {
                        alias,
                        before: None,
                        after: Some(_),
                    } => Some(IdClaim::BindingAlias {
                        kind: BindingAliasKind::ArrangementAsset,
                        raw: alias.get(),
                    }),
                    BindingCommand::PutSequencerSampleAlias {
                        alias,
                        before: None,
                        after: Some(_),
                    } => Some(IdClaim::BindingAlias {
                        kind: BindingAliasKind::SequencerSample,
                        raw: alias.get(),
                    }),
                    BindingCommand::PutSampleTargetAlias {
                        alias,
                        before: None,
                        after: Some(_),
                    } => Some(IdClaim::BindingAlias {
                        kind: BindingAliasKind::SequencerSample,
                        raw: alias.get(),
                    }),
                    BindingCommand::PutPatternDefinitionAlias {
                        alias,
                        before: None,
                        after: Some(_),
                    } => Some(IdClaim::BindingAlias {
                        kind: BindingAliasKind::ArrangementPattern,
                        raw: alias.get(),
                    }),
                    BindingCommand::PutAutomationLaneAlias {
                        alias,
                        before: None,
                        after: Some(_),
                    } => Some(IdClaim::BindingAlias {
                        kind: BindingAliasKind::ArrangementParameter,
                        raw: alias.get(),
                    }),
                    _ => None,
                };
                claims.extend(claim);
            }
            DomainCommand::Air(command) => {
                let claim = match command {
                    AirCommand::PutSource {
                        before: None,
                        after: Some(value),
                    } => Some((AirEntityKind::Source, value.id.get())),
                    AirCommand::PutSpan {
                        before: None,
                        after: Some(value),
                    } => Some((AirEntityKind::Span, value.id.get())),
                    AirCommand::PutObject {
                        before: None,
                        after: Some(value),
                    } => Some((AirEntityKind::Object, value.id.get())),
                    AirCommand::PutTransform {
                        before: None,
                        after: Some(value),
                    } => Some((AirEntityKind::Transform, value.id.get())),
                    AirCommand::PutParameter {
                        before: None,
                        after: Some(value),
                    } => Some((AirEntityKind::Parameter, value.id.get())),
                    AirCommand::PutAutomation {
                        before: None,
                        after: Some(value),
                    } => Some((AirEntityKind::Automation, value.id.get())),
                    AirCommand::PutModulation {
                        before: None,
                        after: Some(value),
                    } => Some((AirEntityKind::Modulation, value.id.get())),
                    AirCommand::PutRelation {
                        before: None,
                        after: Some(value),
                    } => Some((AirEntityKind::Relation, value.id.get())),
                    AirCommand::PutEvidence {
                        before: None,
                        after: Some(value),
                    } => Some((AirEntityKind::Evidence, value.id.get())),
                    AirCommand::PutHypothesis {
                        before: None,
                        after: Some(value),
                    } => Some((AirEntityKind::Hypothesis, value.id.get())),
                    AirCommand::PutHypothesisSet {
                        before: None,
                        after: Some(value),
                    } => Some((AirEntityKind::HypothesisSet, value.id.get())),
                    _ => None,
                };
                if let Some((kind, raw)) = claim {
                    claims.insert(IdClaim::Air { kind, raw });
                }
            }
        }
    }
    claims
}

fn sequencer_child_ids(
    pattern: &sequencer::PatternDefinition,
) -> (BTreeSet<sequencer::StepLaneId>, BTreeSet<sequencer::NoteId>) {
    match &pattern.content {
        sequencer::PatternContent::Steps(pattern) => {
            (pattern.lanes.keys().copied().collect(), BTreeSet::new())
        }
        sequencer::PatternContent::Notes(pattern) => {
            (BTreeSet::new(), pattern.notes.keys().copied().collect())
        }
    }
}

fn add_mixer_claims(
    before: &mixer::MixerGraph,
    after: &mixer::MixerGraph,
    claims: &mut BTreeSet<IdClaim>,
) {
    let before_buses = before.buses().map(|bus| bus.id()).collect::<BTreeSet<_>>();
    let before_nodes = before
        .buses()
        .map(|bus| bus.node_id())
        .chain(before.processors().map(|processor| processor.node_id()))
        .collect::<BTreeSet<_>>();
    let before_sends = before
        .buses()
        .flat_map(|bus| bus.sends().iter().map(|send| send.id()))
        .collect::<BTreeSet<_>>();
    let before_processors = before
        .processors()
        .map(|processor| processor.id())
        .collect::<BTreeSet<_>>();
    let before_parameters = before
        .processors()
        .flat_map(|processor| processor.parameters().map(|parameter| parameter.id()))
        .collect::<BTreeSet<_>>();
    claims.extend(
        after
            .buses()
            .map(|bus| bus.id())
            .filter(|id| !before_buses.contains(id))
            .map(IdClaim::MixerBus),
    );
    claims.extend(
        after
            .buses()
            .map(|bus| bus.node_id())
            .chain(after.processors().map(|processor| processor.node_id()))
            .filter(|id| !before_nodes.contains(id))
            .map(IdClaim::MixerNode),
    );
    claims.extend(
        after
            .buses()
            .flat_map(|bus| bus.sends().iter().map(|send| send.id()))
            .filter(|id| !before_sends.contains(id))
            .map(IdClaim::MixerSend),
    );
    claims.extend(
        after
            .processors()
            .map(|processor| processor.id())
            .filter(|id| !before_processors.contains(id))
            .map(IdClaim::MixerProcessor),
    );
    claims.extend(
        after
            .processors()
            .flat_map(|processor| processor.parameters().map(|parameter| parameter.id()))
            .filter(|id| !before_parameters.contains(id))
            .map(IdClaim::MixerParameter),
    );
}

fn derive_change_set(
    before: &ProjectState,
    after: &ProjectState,
    commands: &[DomainCommand],
    touched: BTreeSet<ProjectDomain>,
) -> ChangeSet {
    let mut changes = ChangeSet {
        domains: touched.clone(),
        routing_changed: touched.contains(&ProjectDomain::Mixer)
            || touched.contains(&ProjectDomain::Bindings),
        ..ChangeSet::default()
    };
    if changes.routing_changed {
        for bus in before
            .domains
            .mixer
            .buses()
            .chain(after.domains.mixer.buses())
        {
            changes.invalidate_bus(bus.id());
        }
        return changes;
    }
    for command in commands {
        match command {
            DomainCommand::Air(_) => {}
            DomainCommand::Arrangement(ArrangementCommand::PutClip {
                before: old,
                after: new,
            }) => {
                invalidate_clip(&mut changes, before, old.as_ref());
                invalidate_clip(&mut changes, after, new.as_ref());
            }
            DomainCommand::Arrangement(_) => {
                changes.invalidate_bus(after.domains.mixer.master());
            }
            _ => {
                changes.invalidate_bus(after.domains.mixer.master());
            }
        }
    }
    changes
}

fn invalidate_clip(
    changes: &mut ChangeSet,
    state: &ProjectState,
    clip: Option<&arrangement::Clip>,
) {
    let Some(clip) = clip else { return };
    let bus = state
        .bindings
        .mixer
        .clip_overrides
        .get(&clip.id)
        .copied()
        .or_else(|| state.bindings.mixer.tracks.get(&clip.track_id).copied())
        .unwrap_or_else(|| state.domains.mixer.master());
    let start = clip.placement.start.get();
    let end = clip.placement.end.get();
    match AudioRange::new(start, end) {
        Ok(range) => {
            changes.invalidate_range(bus, range);
        }
        Err(_) => {
            changes.invalidate_bus(bus);
        }
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
        DomainCommand::SampleKits(command) => state
            .domains
            .sample_kits
            .apply_puts(std::slice::from_ref(command))
            .map(|_| ())
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
        BindingCommand::PutSampleTargetAlias {
            alias,
            before,
            after,
        } => {
            put!(&mut bindings.sample_targets.targets, *alias, before, after)?;
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
            id_claims: BTreeSet::from([IdClaim::Air {
                kind: AirEntityKind::Source,
                raw: entity.id.get(),
            }]),
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
