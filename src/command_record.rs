//! Revision-independent command records shared by history and persistence.
//!
//! This module deliberately does not know how to mutate a project. Domain
//! command kernels own application and validation; the aggregate controller
//! owns revision checks, undo, and journaling. Keeping a command batch free of
//! a base revision lets the same exact term be attempted initially, inverted
//! for undo, and rebased for redo without rewriting its durable meaning.
//!
//! Durable command payloads remain opaque here. A codec registry may decode a
//! known `(domain, kind, schema_version)` tuple, but an unknown tuple is still
//! round-trippable data and is never silently discarded or executed.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::arrangement;
use crate::assets;
use crate::automation;
use crate::daw_project::ProjectDomain;
use crate::mixer;
use crate::ontology;
use crate::sequencer;

/// A revision-independent, user-meaningful aggregate edit.
///
/// `C` is the runtime aggregate command enum supplied by the convergence
/// layer. The batch itself can live on undo/redo stacks without retaining a
/// stale optimistic-concurrency token.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandBatch<C> {
    pub label: String,
    pub coalesce: Option<CoalesceToken>,
    pub commands: Vec<C>,
    pub id_claims: BTreeSet<IdClaim>,
}

impl<C> CommandBatch<C> {
    pub fn new(label: impl Into<String>, commands: Vec<C>) -> Self {
        Self {
            label: label.into(),
            coalesce: None,
            commands,
            id_claims: BTreeSet::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    pub fn at_revision(self, base_revision: u64) -> CommandAttempt<C> {
        CommandAttempt {
            base_revision,
            batch: self,
        }
    }

    pub fn map_commands<D>(self, mut map: impl FnMut(C) -> D) -> CommandBatch<D> {
        CommandBatch {
            label: self.label,
            coalesce: self.coalesce,
            commands: self.commands.into_iter().map(&mut map).collect(),
            id_claims: self.id_claims,
        }
    }
}

/// One optimistic attempt to apply a revision-independent batch.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandAttempt<C> {
    pub base_revision: u64,
    pub batch: CommandBatch<C>,
}

impl<C> CommandAttempt<C> {
    pub fn rebase(self, base_revision: u64) -> Self {
        Self {
            base_revision,
            batch: self.batch,
        }
    }
}

/// A deterministic coalescing identity supplied by an editor gesture.
///
/// The controller may merge only compatible commands which share the entire
/// token. A matching token is permission to attempt composition, not proof
/// that arbitrary command lists are composable.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CoalesceToken {
    pub editor_session: u64,
    pub gesture_kind: u64,
    pub primary: CommandAddress,
}

/// The stable entity or singleton addressed by a command.
///
/// This is intentionally more precise than a hash-only coalescing key. It can
/// also name the command in conflict diagnostics without relying on indexes or
/// display names.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CommandAddress {
    ArrangementTrack(arrangement::TrackId),
    ArrangementClip(arrangement::ClipId),
    ArrangementTrackOrder,
    SequencerPattern(sequencer::PatternId),
    SequencerClip(sequencer::PatternClipId),
    SequencerTempoMap,
    AutomationLane(automation::AutomationLaneId),
    AutomationPoint(automation::AutomationPointId),
    MixerBus(mixer::BusId),
    MixerNode(mixer::NodeId),
    MixerSend(mixer::SendId),
    MixerProcessor(mixer::ProcessorId),
    MixerParameter(mixer::ParameterId),
    Asset(assets::AssetId),
    AssetUsage {
        asset: assets::AssetId,
        usage: assets::AssetUsageId,
    },
    Binding(BindingAddress),
    Air(AirAddress),
    WholeDomain(ProjectDomain),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BindingAddress {
    ArrangementAsset(arrangement::AssetId),
    SequencerSample(sequencer::SampleAssetId),
    ArrangementPattern(arrangement::PatternId),
    PatternPlacement(arrangement::ClipId),
    ArrangementParameter(arrangement::ParameterId),
    TrackBus(arrangement::TrackId),
    ClipBusOverride(arrangement::ClipId),
    ClipObject(arrangement::ClipId),
    AssetSource(assets::AssetId),
    AutomationParameter(automation::AutomationLaneId),
    PatternObject(sequencer::PatternId),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AirAddress {
    Source(ontology::SourceId),
    Span(ontology::SpanId),
    Object(ontology::ObjectId),
    Transform(ontology::TransformId),
    Parameter(ontology::ParameterId),
    Automation(ontology::AutomationId),
    Modulation(ontology::ModulationId),
    Relation(ontology::RelationId),
    Evidence(ontology::EvidenceId),
    Hypothesis(ontology::HypothesisId),
    HypothesisSet(ontology::HypothesisSetId),
}

/// Every monotonic identity category that a command may introduce.
///
/// Claims are checked against IDs introduced by before/after command data.
/// They do not mean "the raw value has never existed": redo legitimately
/// recreates an entity below the allocator high-water mark after undo.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdClaim {
    ArrangementTrack(arrangement::TrackId),
    ArrangementClip(arrangement::ClipId),
    SequencerPattern(sequencer::PatternId),
    SequencerClip(sequencer::PatternClipId),
    SequencerLane(sequencer::StepLaneId),
    SequencerNote(sequencer::NoteId),
    AutomationLane(automation::AutomationLaneId),
    AutomationPoint(automation::AutomationPointId),
    MixerBus(mixer::BusId),
    MixerNode(mixer::NodeId),
    MixerSend(mixer::SendId),
    MixerProcessor(mixer::ProcessorId),
    MixerParameter(mixer::ParameterId),
    Asset(assets::AssetId),
    AssetUsage(assets::AssetUsageId),
    BindingAlias {
        kind: BindingAliasKind,
        raw: u64,
    },
    Air {
        kind: AirEntityKind,
        raw: u64,
    },
    /// A portable name owned by another reading. The kind is part of the name;
    /// `(reading, 1, object)` and `(reading, 1, evidence)` cannot collide.
    Foreign {
        reading: u128,
        kind: ForeignEntityKind,
        local: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BindingAliasKind {
    ArrangementAsset,
    SequencerSample,
    ArrangementPattern,
    ArrangementParameter,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AirEntityKind {
    Source,
    Span,
    Object,
    Transform,
    Parameter,
    Automation,
    Modulation,
    Relation,
    Evidence,
    Hypothesis,
    HypothesisSet,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ForeignEntityKind {
    Air(AirEntityKind),
    Pattern,
    PatternClip,
    AutomationLane,
    Comparison,
    LexiconEntry,
    Annotation,
}

/// Persistence-neutral command data encoded by domain codecs.
///
/// `payload` and flattened extensions are retained even when this build has
/// no decoder for the command kind. Replay code must resolve the tuple through
/// a registry before execution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpaqueCommandRecord {
    pub domain: String,
    pub kind: String,
    pub schema_version: u32,
    pub payload: Value,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, Value>,
}

impl OpaqueCommandRecord {
    pub fn codec_key(&self) -> (&str, &str, u32) {
        (&self.domain, &self.kind, self.schema_version)
    }
}

/// Durable form of an ID claim. Textual components make unknown future
/// namespaces and full `u128` reading IDs round-trip through JSON losslessly.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DurableIdClaim {
    pub namespace: String,
    pub kind: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableCoalesceToken {
    pub editor_session: u64,
    pub gesture_kind: u64,
    /// Canonical address text is codec-owned; the runtime address remains
    /// strongly typed and is reconstructed only by a matching codec.
    pub primary: String,
}

/// Versioned, opaque durable batch used by the append-only journal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DurableCommandBatch {
    pub schema_version: u32,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coalesce: Option<DurableCoalesceToken>,
    #[serde(default)]
    pub commands: Vec<OpaqueCommandRecord>,
    #[serde(default)]
    pub id_claims: Vec<DurableIdClaim>,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, Value>,
}

impl DurableCommandBatch {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn new(label: impl Into<String>, commands: Vec<OpaqueCommandRecord>) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            label: label.into(),
            coalesce: None,
            commands,
            id_claims: Vec::new(),
            extensions: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_rebase_without_rewriting_the_term() {
        let batch = CommandBatch::new("edit", vec![7_u8]);
        let attempt = batch.clone().at_revision(12);
        let rebased = attempt.rebase(31);
        assert_eq!(rebased.base_revision, 31);
        assert_eq!(rebased.batch, batch);
    }

    #[test]
    fn opaque_commands_retain_unknown_fields() {
        let json = br#"{
            "domain":"future",
            "kind":"put_star",
            "schema_version":9,
            "payload":{"brightness":3},
            "future_flag":{"mode":"violet"}
        }"#;
        let record: OpaqueCommandRecord = serde_json::from_slice(json).unwrap();
        assert_eq!(record.codec_key(), ("future", "put_star", 9));
        assert_eq!(record.extensions["future_flag"]["mode"], "violet");
        let again: OpaqueCommandRecord =
            serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
        assert_eq!(again, record);
    }
}
