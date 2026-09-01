//! Deterministic durable codec for the aggregate runtime command algebra.
//!
//! The journal owns opaque records; this codec executes only the exact v1
//! tuples it knows. Unknown versions, kinds, extension members, claims, and
//! addresses are refused rather than partially interpreted.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::arrangement::{self, ArrangementOperation};
use crate::assets;
use crate::automation::{AutomationCommand, LaneChange};
use crate::command::{AirCommand, AssetCommand, BindingCommand, CommandBatch, DomainCommand};
use crate::command_journal::RuntimeCommandCodec;
use crate::command_record::{
    AirAddress, AirEntityKind, BindingAddress, BindingAliasKind, CoalesceToken, CommandAddress,
    DurableCoalesceToken, DurableCommandBatch, DurableIdClaim, ForeignEntityKind, IdClaim,
    OpaqueCommandRecord,
};
use crate::daw_project::ProjectDomain;
use crate::mixer::{self, MixerCommand};
use crate::ontology;
use crate::project_codecs::{self, CodecError};
use crate::sample_kit::{self, SampleKitPut, SampleTargetRef};
use crate::sequencer::{self, SequencerCommand};

pub const RUNTIME_COMMAND_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default)]
pub struct DeterministicRuntimeCommandCodec;

#[derive(Debug)]
pub enum RuntimeCommandCodecError {
    UnsupportedBatchVersion(u32),
    BatchExtensions,
    CommandExtensions {
        domain: String,
        kind: String,
    },
    UnknownCommand {
        domain: String,
        kind: String,
        version: u32,
    },
    InvalidPayload {
        domain: String,
        kind: String,
        message: String,
    },
    InvalidClaim(String),
    DuplicateClaim(DurableIdClaim),
    InvalidAddress(String),
    Codec(CodecError),
}

impl fmt::Display for RuntimeCommandCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedBatchVersion(version) => {
                write!(f, "unsupported runtime command batch schema {version}")
            }
            Self::BatchExtensions => f.write_str("runtime command batch has unknown extensions"),
            Self::CommandExtensions { domain, kind } => {
                write!(f, "runtime command {domain}/{kind} has unknown extensions")
            }
            Self::UnknownCommand {
                domain,
                kind,
                version,
            } => {
                write!(
                    f,
                    "unknown runtime command {domain}/{kind} schema {version}"
                )
            }
            Self::InvalidPayload {
                domain,
                kind,
                message,
            } => {
                write!(f, "invalid runtime command {domain}/{kind}: {message}")
            }
            Self::InvalidClaim(message) => write!(f, "invalid runtime ID claim: {message}"),
            Self::DuplicateClaim(claim) => write!(f, "duplicate runtime ID claim: {claim:?}"),
            Self::InvalidAddress(message) => write!(f, "invalid coalescing address: {message}"),
            Self::Codec(error) => error.fmt(f),
        }
    }
}

impl Error for RuntimeCommandCodecError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Codec(error) => Some(error),
            _ => None,
        }
    }
}

impl From<CodecError> for RuntimeCommandCodecError {
    fn from(value: CodecError) -> Self {
        Self::Codec(value)
    }
}

impl RuntimeCommandCodec for DeterministicRuntimeCommandCodec {
    type Error = RuntimeCommandCodecError;

    fn encode_batch(&self, batch: &CommandBatch) -> Result<DurableCommandBatch, Self::Error> {
        let commands = batch
            .commands
            .iter()
            .map(encode_command)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(DurableCommandBatch {
            schema_version: RUNTIME_COMMAND_SCHEMA_VERSION,
            label: batch.label.clone(),
            coalesce: batch.coalesce.as_ref().map(encode_coalesce).transpose()?,
            commands,
            id_claims: batch.id_claims.iter().map(encode_claim).collect(),
            extensions: BTreeMap::new(),
        })
    }

    fn decode_batch(&self, batch: &DurableCommandBatch) -> Result<CommandBatch, Self::Error> {
        if batch.schema_version != RUNTIME_COMMAND_SCHEMA_VERSION {
            return Err(RuntimeCommandCodecError::UnsupportedBatchVersion(
                batch.schema_version,
            ));
        }
        if !batch.extensions.is_empty() {
            return Err(RuntimeCommandCodecError::BatchExtensions);
        }
        let mut claims = BTreeSet::new();
        for durable in &batch.id_claims {
            let claim = decode_claim(durable)?;
            if !claims.insert(claim) {
                return Err(RuntimeCommandCodecError::DuplicateClaim(durable.clone()));
            }
        }
        Ok(CommandBatch {
            label: batch.label.clone(),
            coalesce: batch.coalesce.as_ref().map(decode_coalesce).transpose()?,
            commands: batch
                .commands
                .iter()
                .map(decode_command)
                .collect::<Result<Vec<_>, _>>()?,
            id_claims: claims,
        })
    }
}

fn record(domain: &str, kind: &str, payload: Value) -> OpaqueCommandRecord {
    OpaqueCommandRecord {
        domain: domain.into(),
        kind: kind.into(),
        schema_version: RUNTIME_COMMAND_SCHEMA_VERSION,
        payload,
        extensions: BTreeMap::new(),
    }
}

fn value<T: Serialize>(
    domain: &str,
    kind: &str,
    value: &T,
) -> Result<Value, RuntimeCommandCodecError> {
    serde_json::to_value(value).map_err(|error| RuntimeCommandCodecError::InvalidPayload {
        domain: domain.into(),
        kind: kind.into(),
        message: error.to_string(),
    })
}

fn known<T: DeserializeOwned + Serialize>(
    record: &OpaqueCommandRecord,
) -> Result<T, RuntimeCommandCodecError> {
    let decoded = serde_json::from_value::<T>(record.payload.clone()).map_err(|error| {
        RuntimeCommandCodecError::InvalidPayload {
            domain: record.domain.clone(),
            kind: record.kind.clone(),
            message: error.to_string(),
        }
    })?;
    let canonical = value(&record.domain, &record.kind, &decoded)?;
    if canonical != record.payload {
        return Err(RuntimeCommandCodecError::InvalidPayload {
            domain: record.domain.clone(),
            kind: record.kind.clone(),
            message: "payload contains unknown or non-canonical members".into(),
        });
    }
    Ok(decoded)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PutValue {
    before: Option<Value>,
    after: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct IdPutValue {
    id: u64,
    before: Option<Value>,
    after: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct UsagePutValue {
    asset: u64,
    usage: u64,
    before: Option<Value>,
    after: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AvailabilityPutValue {
    asset: u64,
    before: Value,
    after: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LabeledChangesValue {
    label: String,
    changes: Vec<PutValue>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct MixerValue {
    label: String,
    before: Value,
    after: Value,
}

fn encode_optional<T>(
    value: Option<&T>,
    encode: impl Fn(&T) -> Result<Value, CodecError>,
) -> Result<Option<Value>, RuntimeCommandCodecError> {
    value.map(encode).transpose().map_err(Into::into)
}

fn decode_optional<T>(
    value: Option<Value>,
    decode: impl Fn(Value) -> Result<T, CodecError>,
) -> Result<Option<T>, RuntimeCommandCodecError> {
    value.map(decode).transpose().map_err(Into::into)
}

fn encode_command(
    command: &DomainCommand,
) -> Result<OpaqueCommandRecord, RuntimeCommandCodecError> {
    match command {
        DomainCommand::Arrangement(command) => {
            let kind = match command {
                ArrangementOperation::PutTrack { .. } => "put_track",
                ArrangementOperation::PutClip { .. } => "put_clip",
                ArrangementOperation::SetTrackOrder { .. } => "set_track_order",
            };
            Ok(record(
                "arrangement",
                kind,
                value("arrangement", kind, command)?,
            ))
        }
        DomainCommand::Sequencer(command) => encode_sequencer(command),
        DomainCommand::Automation(command) => {
            let changes = command
                .changes
                .iter()
                .map(|change| {
                    Ok(PutValue {
                        before: encode_optional(
                            change.before.as_ref(),
                            project_codecs::encode_command_lane,
                        )?,
                        after: encode_optional(
                            change.after.as_ref(),
                            project_codecs::encode_command_lane,
                        )?,
                    })
                })
                .collect::<Result<Vec<_>, RuntimeCommandCodecError>>()?;
            let payload = LabeledChangesValue {
                label: command.label.clone(),
                changes,
            };
            Ok(record(
                "automation",
                "put_lanes",
                value("automation", "put_lanes", &payload)?,
            ))
        }
        DomainCommand::Mixer(command) => {
            let payload = MixerValue {
                label: command.label().into(),
                before: project_codecs::encode_command_mixer_graph(command.before())?,
                after: project_codecs::encode_command_mixer_graph(command.after())?,
            };
            Ok(record(
                "mixer",
                "replace_graph",
                value("mixer", "replace_graph", &payload)?,
            ))
        }
        DomainCommand::SampleKits(command) => {
            let payload = PutValue {
                before: encode_optional(
                    command.before.as_ref(),
                    project_codecs::encode_command_sample_kit,
                )?,
                after: encode_optional(
                    command.after.as_ref(),
                    project_codecs::encode_command_sample_kit,
                )?,
            };
            Ok(record(
                "sample_kits",
                "put_kit",
                value("sample_kits", "put_kit", &payload)?,
            ))
        }
        DomainCommand::Assets(command) => encode_asset(command),
        DomainCommand::Bindings(command) => encode_binding(command),
        DomainCommand::Air(command) => encode_air(command),
    }
}

fn encode_sequencer(
    command: &SequencerCommand,
) -> Result<OpaqueCommandRecord, RuntimeCommandCodecError> {
    let (kind, payload) = match command {
        SequencerCommand::PutPattern { before, after } => (
            "put_pattern",
            PutValue {
                before: encode_optional(before.as_ref(), project_codecs::encode_command_pattern)?,
                after: encode_optional(after.as_ref(), project_codecs::encode_command_pattern)?,
            },
        ),
        SequencerCommand::PutClip { before, after } => (
            "put_clip",
            PutValue {
                before: encode_optional(
                    before.as_ref(),
                    project_codecs::encode_command_pattern_clip,
                )?,
                after: encode_optional(
                    after.as_ref(),
                    project_codecs::encode_command_pattern_clip,
                )?,
            },
        ),
        SequencerCommand::SetTempoMap { before, after } => (
            "set_tempo_map",
            PutValue {
                before: Some(project_codecs::encode_command_tempo_map(before)?),
                after: Some(project_codecs::encode_command_tempo_map(after)?),
            },
        ),
    };
    Ok(record(
        "sequencer",
        kind,
        value("sequencer", kind, &payload)?,
    ))
}

fn encode_asset(command: &AssetCommand) -> Result<OpaqueCommandRecord, RuntimeCommandCodecError> {
    match command {
        AssetCommand::PutAsset { id, before, after } => {
            let payload = IdPutValue {
                id: id.0,
                before: encode_optional(before.as_ref(), project_codecs::encode_command_asset)?,
                after: encode_optional(after.as_ref(), project_codecs::encode_command_asset)?,
            };
            Ok(record(
                "assets",
                "put_asset",
                value("assets", "put_asset", &payload)?,
            ))
        }
        AssetCommand::PutUsage {
            asset,
            usage,
            before,
            after,
        } => {
            let payload = UsagePutValue {
                asset: asset.0,
                usage: usage.0,
                before: encode_optional(before.as_ref(), project_codecs::encode_command_usage)?,
                after: encode_optional(after.as_ref(), project_codecs::encode_command_usage)?,
            };
            Ok(record(
                "assets",
                "put_usage",
                value("assets", "put_usage", &payload)?,
            ))
        }
        AssetCommand::PutAvailability {
            asset,
            before,
            after,
        } => {
            let payload = AvailabilityPutValue {
                asset: asset.0,
                before: project_codecs::encode_command_availability(before)?,
                after: project_codecs::encode_command_availability(after)?,
            };
            Ok(record(
                "assets",
                "put_availability",
                value("assets", "put_availability", &payload)?,
            ))
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BindingValue {
    MediaAssetAlias {
        alias: u64,
        before: Option<u64>,
        after: Option<u64>,
    },
    SequencerSampleAlias {
        alias: u64,
        before: Option<u64>,
        after: Option<u64>,
    },
    SampleTargetAlias {
        alias: u64,
        before: Option<SampleTargetValue>,
        after: Option<SampleTargetValue>,
    },
    PatternDefinitionAlias {
        alias: u64,
        before: Option<u64>,
        after: Option<u64>,
    },
    PatternPlacement {
        clip: u64,
        before: Option<u64>,
        after: Option<u64>,
    },
    AutomationLaneAlias {
        alias: u64,
        before: Option<u64>,
        after: Option<u64>,
    },
    TrackBus {
        track: u64,
        before: Option<u64>,
        after: Option<u64>,
    },
    ClipBusOverride {
        clip: u64,
        before: Option<u64>,
        after: Option<u64>,
    },
    ClipObjectLink {
        clip: u64,
        before: Option<u64>,
        after: Option<u64>,
    },
    AssetSourceLink {
        asset: u64,
        before: Option<u64>,
        after: Option<u64>,
    },
    AutomationParameterLink {
        lane: u64,
        before: Option<u64>,
        after: Option<u64>,
    },
    PatternObjectLink {
        pattern: u64,
        before: Option<u64>,
        after: Option<u64>,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct SampleTargetValue {
    kit: u64,
    pad: u64,
    zone: u64,
}

fn sample_target(value: SampleTargetRef) -> SampleTargetValue {
    SampleTargetValue {
        kit: value.kit.get(),
        pad: value.pad.get(),
        zone: value.zone.get(),
    }
}

fn encode_binding(
    command: &BindingCommand,
) -> Result<OpaqueCommandRecord, RuntimeCommandCodecError> {
    let payload = match command {
        BindingCommand::PutMediaAssetAlias {
            alias,
            before,
            after,
        } => BindingValue::MediaAssetAlias {
            alias: alias.get(),
            before: before.map(|id| id.0),
            after: after.map(|id| id.0),
        },
        BindingCommand::PutSequencerSampleAlias {
            alias,
            before,
            after,
        } => BindingValue::SequencerSampleAlias {
            alias: alias.get(),
            before: before.map(|id| id.0),
            after: after.map(|id| id.0),
        },
        BindingCommand::PutSampleTargetAlias {
            alias,
            before,
            after,
        } => BindingValue::SampleTargetAlias {
            alias: alias.get(),
            before: before.map(sample_target),
            after: after.map(sample_target),
        },
        BindingCommand::PutPatternDefinitionAlias {
            alias,
            before,
            after,
        } => BindingValue::PatternDefinitionAlias {
            alias: alias.get(),
            before: before.map(|id| id.get()),
            after: after.map(|id| id.get()),
        },
        BindingCommand::PutPatternPlacement {
            clip,
            before,
            after,
        } => BindingValue::PatternPlacement {
            clip: clip.get(),
            before: before.map(|id| id.get()),
            after: after.map(|id| id.get()),
        },
        BindingCommand::PutAutomationLaneAlias {
            alias,
            before,
            after,
        } => BindingValue::AutomationLaneAlias {
            alias: alias.get(),
            before: before.map(|id| id.get()),
            after: after.map(|id| id.get()),
        },
        BindingCommand::PutTrackBus {
            track,
            before,
            after,
        } => BindingValue::TrackBus {
            track: track.get(),
            before: before.map(|id| id.get()),
            after: after.map(|id| id.get()),
        },
        BindingCommand::PutClipBusOverride {
            clip,
            before,
            after,
        } => BindingValue::ClipBusOverride {
            clip: clip.get(),
            before: before.map(|id| id.get()),
            after: after.map(|id| id.get()),
        },
        BindingCommand::PutClipObjectLink {
            clip,
            before,
            after,
        } => BindingValue::ClipObjectLink {
            clip: clip.get(),
            before: before.map(|id| id.get()),
            after: after.map(|id| id.get()),
        },
        BindingCommand::PutAssetSourceLink {
            asset,
            before,
            after,
        } => BindingValue::AssetSourceLink {
            asset: asset.0,
            before: before.map(|id| id.get()),
            after: after.map(|id| id.get()),
        },
        BindingCommand::PutAutomationParameterLink {
            lane,
            before,
            after,
        } => BindingValue::AutomationParameterLink {
            lane: lane.get(),
            before: before.map(|id| id.get()),
            after: after.map(|id| id.get()),
        },
        BindingCommand::PutPatternObjectLink {
            pattern,
            before,
            after,
        } => BindingValue::PatternObjectLink {
            pattern: pattern.get(),
            before: before.map(|id| id.get()),
            after: after.map(|id| id.get()),
        },
    };
    Ok(record(
        "bindings",
        "put_entry",
        value("bindings", "put_entry", &payload)?,
    ))
}

fn encode_air(command: &AirCommand) -> Result<OpaqueCommandRecord, RuntimeCommandCodecError> {
    macro_rules! air {
        ($kind:literal, $before:expr, $after:expr) => {{
            let payload = value("air", $kind, &($before, $after))?;
            Ok(record("air", $kind, payload))
        }};
    }
    match command {
        AirCommand::PutSource { before, after } => air!("put_source", before, after),
        AirCommand::PutSpan { before, after } => air!("put_span", before, after),
        AirCommand::PutObject { before, after } => air!("put_object", before, after),
        AirCommand::PutTransform { before, after } => air!("put_transform", before, after),
        AirCommand::PutParameter { before, after } => air!("put_parameter", before, after),
        AirCommand::PutAutomation { before, after } => air!("put_automation", before, after),
        AirCommand::PutModulation { before, after } => air!("put_modulation", before, after),
        AirCommand::PutRelation { before, after } => air!("put_relation", before, after),
        AirCommand::PutEvidence { before, after } => air!("put_evidence", before, after),
        AirCommand::PutHypothesis { before, after } => air!("put_hypothesis", before, after),
        AirCommand::PutHypothesisSet { before, after } => air!("put_hypothesis_set", before, after),
    }
}

fn decode_command(record: &OpaqueCommandRecord) -> Result<DomainCommand, RuntimeCommandCodecError> {
    if record.schema_version != RUNTIME_COMMAND_SCHEMA_VERSION || !record.extensions.is_empty() {
        if !record.extensions.is_empty() {
            return Err(RuntimeCommandCodecError::CommandExtensions {
                domain: record.domain.clone(),
                kind: record.kind.clone(),
            });
        }
        return Err(unknown(record));
    }
    match (record.domain.as_str(), record.kind.as_str()) {
        ("arrangement", "put_track")
        | ("arrangement", "put_clip")
        | ("arrangement", "set_track_order") => {
            let command = known::<ArrangementOperation>(record)?;
            let expected = match command {
                ArrangementOperation::PutTrack { .. } => "put_track",
                ArrangementOperation::PutClip { .. } => "put_clip",
                ArrangementOperation::SetTrackOrder { .. } => "set_track_order",
            };
            if expected != record.kind {
                return Err(unknown(record));
            }
            Ok(DomainCommand::Arrangement(command))
        }
        ("sequencer", _) => decode_sequencer(record).map(DomainCommand::Sequencer),
        ("automation", "put_lanes") => {
            let payload = known::<LabeledChangesValue>(record)?;
            let changes = payload
                .changes
                .into_iter()
                .map(|change| {
                    Ok(LaneChange {
                        before: decode_optional(
                            change.before,
                            project_codecs::decode_command_lane,
                        )?,
                        after: decode_optional(change.after, project_codecs::decode_command_lane)?,
                    })
                })
                .collect::<Result<Vec<_>, RuntimeCommandCodecError>>()?;
            Ok(DomainCommand::Automation(AutomationCommand {
                label: payload.label,
                changes,
            }))
        }
        ("mixer", "replace_graph") => {
            let payload = known::<MixerValue>(record)?;
            let before = project_codecs::decode_command_mixer_graph(payload.before)?;
            let after = project_codecs::decode_command_mixer_graph(payload.after)?;
            Ok(DomainCommand::Mixer(
                MixerCommand::from_codec_parts(payload.label, before, after)
                    .map_err(|error| invalid(record, error))?,
            ))
        }
        ("sample_kits", "put_kit") => {
            let payload = known::<PutValue>(record)?;
            Ok(DomainCommand::SampleKits(SampleKitPut {
                before: decode_optional(payload.before, project_codecs::decode_command_sample_kit)?,
                after: decode_optional(payload.after, project_codecs::decode_command_sample_kit)?,
            }))
        }
        ("assets", _) => decode_asset(record).map(DomainCommand::Assets),
        ("bindings", "put_entry") => decode_binding(record).map(DomainCommand::Bindings),
        ("air", _) => decode_air(record).map(DomainCommand::Air),
        _ => Err(unknown(record)),
    }
}

fn decode_sequencer(
    record: &OpaqueCommandRecord,
) -> Result<SequencerCommand, RuntimeCommandCodecError> {
    let payload = known::<PutValue>(record)?;
    match record.kind.as_str() {
        "put_pattern" => Ok(SequencerCommand::PutPattern {
            before: decode_optional(payload.before, project_codecs::decode_command_pattern)?,
            after: decode_optional(payload.after, project_codecs::decode_command_pattern)?,
        }),
        "put_clip" => Ok(SequencerCommand::PutClip {
            before: decode_optional(payload.before, project_codecs::decode_command_pattern_clip)?,
            after: decode_optional(payload.after, project_codecs::decode_command_pattern_clip)?,
        }),
        "set_tempo_map" => Ok(SequencerCommand::SetTempoMap {
            before: project_codecs::decode_command_tempo_map(
                payload
                    .before
                    .ok_or_else(|| invalid(record, "missing before tempo map"))?,
            )?,
            after: project_codecs::decode_command_tempo_map(
                payload
                    .after
                    .ok_or_else(|| invalid(record, "missing after tempo map"))?,
            )?,
        }),
        _ => Err(unknown(record)),
    }
}

fn decode_asset(record: &OpaqueCommandRecord) -> Result<AssetCommand, RuntimeCommandCodecError> {
    match record.kind.as_str() {
        "put_asset" => {
            let payload = known::<IdPutValue>(record)?;
            Ok(AssetCommand::PutAsset {
                id: assets::AssetId(payload.id),
                before: decode_optional(payload.before, project_codecs::decode_command_asset)?,
                after: decode_optional(payload.after, project_codecs::decode_command_asset)?,
            })
        }
        "put_usage" => {
            let payload = known::<UsagePutValue>(record)?;
            Ok(AssetCommand::PutUsage {
                asset: assets::AssetId(payload.asset),
                usage: assets::AssetUsageId(payload.usage),
                before: decode_optional(payload.before, project_codecs::decode_command_usage)?,
                after: decode_optional(payload.after, project_codecs::decode_command_usage)?,
            })
        }
        "put_availability" => {
            let payload = known::<AvailabilityPutValue>(record)?;
            Ok(AssetCommand::PutAvailability {
                asset: assets::AssetId(payload.asset),
                before: project_codecs::decode_command_availability(payload.before)?,
                after: project_codecs::decode_command_availability(payload.after)?,
            })
        }
        _ => Err(unknown(record)),
    }
}

fn decode_binding(
    record: &OpaqueCommandRecord,
) -> Result<BindingCommand, RuntimeCommandCodecError> {
    let target = |value: SampleTargetValue| SampleTargetRef {
        kit: sample_kit::KitId::from_raw(value.kit),
        pad: sample_kit::PadId::from_raw(value.pad),
        zone: sample_kit::ZoneId::from_raw(value.zone),
    };
    Ok(match known::<BindingValue>(record)? {
        BindingValue::MediaAssetAlias {
            alias,
            before,
            after,
        } => BindingCommand::PutMediaAssetAlias {
            alias: arrangement::AssetId::from_raw(alias),
            before: before.map(assets::AssetId),
            after: after.map(assets::AssetId),
        },
        BindingValue::SequencerSampleAlias {
            alias,
            before,
            after,
        } => BindingCommand::PutSequencerSampleAlias {
            alias: sequencer::SampleAssetId::from_raw(alias),
            before: before.map(assets::AssetId),
            after: after.map(assets::AssetId),
        },
        BindingValue::SampleTargetAlias {
            alias,
            before,
            after,
        } => BindingCommand::PutSampleTargetAlias {
            alias: sequencer::SampleAssetId::from_raw(alias),
            before: before.map(target),
            after: after.map(target),
        },
        BindingValue::PatternDefinitionAlias {
            alias,
            before,
            after,
        } => BindingCommand::PutPatternDefinitionAlias {
            alias: arrangement::PatternId::from_raw(alias),
            before: before.map(sequencer::PatternId::from_raw),
            after: after.map(sequencer::PatternId::from_raw),
        },
        BindingValue::PatternPlacement {
            clip,
            before,
            after,
        } => BindingCommand::PutPatternPlacement {
            clip: arrangement::ClipId::from_raw(clip),
            before: before.map(sequencer::PatternClipId::from_raw),
            after: after.map(sequencer::PatternClipId::from_raw),
        },
        BindingValue::AutomationLaneAlias {
            alias,
            before,
            after,
        } => BindingCommand::PutAutomationLaneAlias {
            alias: arrangement::ParameterId::from_raw(alias),
            before: before.map(crate::automation::AutomationLaneId::from_raw),
            after: after.map(crate::automation::AutomationLaneId::from_raw),
        },
        BindingValue::TrackBus {
            track,
            before,
            after,
        } => BindingCommand::PutTrackBus {
            track: arrangement::TrackId::from_raw(track),
            before: before.map(mixer::BusId::from_raw),
            after: after.map(mixer::BusId::from_raw),
        },
        BindingValue::ClipBusOverride {
            clip,
            before,
            after,
        } => BindingCommand::PutClipBusOverride {
            clip: arrangement::ClipId::from_raw(clip),
            before: before.map(mixer::BusId::from_raw),
            after: after.map(mixer::BusId::from_raw),
        },
        BindingValue::ClipObjectLink {
            clip,
            before,
            after,
        } => BindingCommand::PutClipObjectLink {
            clip: arrangement::ClipId::from_raw(clip),
            before: before.map(ontology::ObjectId::new),
            after: after.map(ontology::ObjectId::new),
        },
        BindingValue::AssetSourceLink {
            asset,
            before,
            after,
        } => BindingCommand::PutAssetSourceLink {
            asset: assets::AssetId(asset),
            before: before.map(ontology::SourceId::new),
            after: after.map(ontology::SourceId::new),
        },
        BindingValue::AutomationParameterLink {
            lane,
            before,
            after,
        } => BindingCommand::PutAutomationParameterLink {
            lane: crate::automation::AutomationLaneId::from_raw(lane),
            before: before.map(ontology::ParameterId::new),
            after: after.map(ontology::ParameterId::new),
        },
        BindingValue::PatternObjectLink {
            pattern,
            before,
            after,
        } => BindingCommand::PutPatternObjectLink {
            pattern: sequencer::PatternId::from_raw(pattern),
            before: before.map(ontology::ObjectId::new),
            after: after.map(ontology::ObjectId::new),
        },
    })
}

fn decode_air(record: &OpaqueCommandRecord) -> Result<AirCommand, RuntimeCommandCodecError> {
    macro_rules! decode {
        ($ty:ty, $variant:ident) => {{
            let (before, after) = known::<(Option<$ty>, Option<$ty>)>(record)?;
            Ok(AirCommand::$variant { before, after })
        }};
    }
    match record.kind.as_str() {
        "put_source" => decode!(ontology::AudioSource, PutSource),
        "put_span" => decode!(ontology::SourceSpan, PutSpan),
        "put_object" => decode!(ontology::AuditoryObject, PutObject),
        "put_transform" => decode!(ontology::Transform, PutTransform),
        "put_parameter" => decode!(ontology::Parameter, PutParameter),
        "put_automation" => decode!(ontology::Automation, PutAutomation),
        "put_modulation" => decode!(ontology::Modulation, PutModulation),
        "put_relation" => decode!(ontology::ObjectRelation, PutRelation),
        "put_evidence" => decode!(ontology::Evidence, PutEvidence),
        "put_hypothesis" => decode!(ontology::Hypothesis, PutHypothesis),
        "put_hypothesis_set" => decode!(ontology::HypothesisSet, PutHypothesisSet),
        _ => Err(unknown(record)),
    }
}

fn invalid(record: &OpaqueCommandRecord, message: impl fmt::Display) -> RuntimeCommandCodecError {
    RuntimeCommandCodecError::InvalidPayload {
        domain: record.domain.clone(),
        kind: record.kind.clone(),
        message: message.to_string(),
    }
}

fn unknown(record: &OpaqueCommandRecord) -> RuntimeCommandCodecError {
    RuntimeCommandCodecError::UnknownCommand {
        domain: record.domain.clone(),
        kind: record.kind.clone(),
        version: record.schema_version,
    }
}

fn encode_claim(claim: &IdClaim) -> DurableIdClaim {
    let raw = |id: u64| id.to_string();
    match claim {
        IdClaim::ArrangementTrack(id) => durable_claim("arrangement", "track", raw(id.get()), None),
        IdClaim::ArrangementClip(id) => durable_claim("arrangement", "clip", raw(id.get()), None),
        IdClaim::SequencerPattern(id) => durable_claim("sequencer", "pattern", raw(id.get()), None),
        IdClaim::SequencerClip(id) => durable_claim("sequencer", "clip", raw(id.get()), None),
        IdClaim::SequencerLane(id) => durable_claim("sequencer", "lane", raw(id.get()), None),
        IdClaim::SequencerNote(id) => durable_claim("sequencer", "note", raw(id.get()), None),
        IdClaim::AutomationLane(id) => durable_claim("automation", "lane", raw(id.get()), None),
        IdClaim::AutomationPoint(id) => durable_claim("automation", "point", raw(id.get()), None),
        IdClaim::MixerBus(id) => durable_claim("mixer", "bus", raw(id.get()), None),
        IdClaim::MixerNode(id) => durable_claim("mixer", "node", raw(id.get()), None),
        IdClaim::MixerSend(id) => durable_claim("mixer", "send", raw(id.get()), None),
        IdClaim::MixerProcessor(id) => durable_claim("mixer", "processor", raw(id.get()), None),
        IdClaim::MixerParameter(id) => durable_claim("mixer", "parameter", raw(id.get()), None),
        IdClaim::SampleKit(id) => durable_claim("sample_kits", "kit", raw(id.get()), None),
        IdClaim::SamplePad(id) => durable_claim("sample_kits", "pad", raw(id.get()), None),
        IdClaim::SampleZone(id) => durable_claim("sample_kits", "zone", raw(id.get()), None),
        IdClaim::Asset(id) => durable_claim("assets", "asset", raw(id.0), None),
        IdClaim::AssetUsage(id) => durable_claim("assets", "usage", raw(id.0), None),
        IdClaim::BindingAlias { kind, raw: id } => {
            durable_claim("bindings", binding_alias_name(*kind), raw(*id), None)
        }
        IdClaim::Air { kind, raw: id } => {
            durable_claim("air", air_kind_name(*kind), raw(*id), None)
        }
        IdClaim::Foreign {
            reading,
            kind,
            local,
        } => durable_claim(
            "foreign",
            foreign_kind_name(*kind),
            raw(*local),
            Some(reading.to_string()),
        ),
    }
}

fn durable_claim(namespace: &str, kind: &str, id: String, scope: Option<String>) -> DurableIdClaim {
    DurableIdClaim {
        namespace: namespace.into(),
        kind: kind.into(),
        id,
        scope,
    }
}

fn parse_id(claim: &DurableIdClaim) -> Result<u64, RuntimeCommandCodecError> {
    claim.id.parse().map_err(|_| {
        RuntimeCommandCodecError::InvalidClaim(format!("non-u64 identity {:?}", claim.id))
    })
}

fn no_scope(claim: &DurableIdClaim) -> Result<(), RuntimeCommandCodecError> {
    if claim.scope.is_none() {
        Ok(())
    } else {
        Err(RuntimeCommandCodecError::InvalidClaim(format!(
            "unexpected scope for {}/{}",
            claim.namespace, claim.kind
        )))
    }
}

fn decode_claim(claim: &DurableIdClaim) -> Result<IdClaim, RuntimeCommandCodecError> {
    let id = parse_id(claim)?;
    if claim.namespace != "foreign" {
        no_scope(claim)?;
    }
    Ok(match (claim.namespace.as_str(), claim.kind.as_str()) {
        ("arrangement", "track") => IdClaim::ArrangementTrack(arrangement::TrackId::from_raw(id)),
        ("arrangement", "clip") => IdClaim::ArrangementClip(arrangement::ClipId::from_raw(id)),
        ("sequencer", "pattern") => IdClaim::SequencerPattern(sequencer::PatternId::from_raw(id)),
        ("sequencer", "clip") => IdClaim::SequencerClip(sequencer::PatternClipId::from_raw(id)),
        ("sequencer", "lane") => IdClaim::SequencerLane(sequencer::StepLaneId::from_raw(id)),
        ("sequencer", "note") => IdClaim::SequencerNote(sequencer::NoteId::from_raw(id)),
        ("automation", "lane") => {
            IdClaim::AutomationLane(crate::automation::AutomationLaneId::from_raw(id))
        }
        ("automation", "point") => {
            IdClaim::AutomationPoint(crate::automation::AutomationPointId::from_raw(id))
        }
        ("mixer", "bus") => IdClaim::MixerBus(mixer::BusId::from_raw(id)),
        ("mixer", "node") => IdClaim::MixerNode(mixer::NodeId::from_raw(id)),
        ("mixer", "send") => IdClaim::MixerSend(mixer::SendId::from_raw(id)),
        ("mixer", "processor") => IdClaim::MixerProcessor(mixer::ProcessorId::from_raw(id)),
        ("mixer", "parameter") => IdClaim::MixerParameter(mixer::ParameterId::from_raw(id)),
        ("sample_kits", "kit") => IdClaim::SampleKit(sample_kit::KitId::from_raw(id)),
        ("sample_kits", "pad") => IdClaim::SamplePad(sample_kit::PadId::from_raw(id)),
        ("sample_kits", "zone") => IdClaim::SampleZone(sample_kit::ZoneId::from_raw(id)),
        ("assets", "asset") => IdClaim::Asset(assets::AssetId(id)),
        ("assets", "usage") => IdClaim::AssetUsage(assets::AssetUsageId(id)),
        ("bindings", kind) => IdClaim::BindingAlias {
            kind: parse_binding_alias(kind)?,
            raw: id,
        },
        ("air", kind) => IdClaim::Air {
            kind: parse_air_kind(kind)?,
            raw: id,
        },
        ("foreign", kind) => IdClaim::Foreign {
            reading: claim
                .scope
                .as_ref()
                .ok_or_else(|| {
                    RuntimeCommandCodecError::InvalidClaim(
                        "foreign claim has no reading scope".into(),
                    )
                })?
                .parse()
                .map_err(|_| {
                    RuntimeCommandCodecError::InvalidClaim(
                        "foreign reading scope is not u128".into(),
                    )
                })?,
            kind: parse_foreign_kind(kind)?,
            local: id,
        },
        _ => {
            return Err(RuntimeCommandCodecError::InvalidClaim(format!(
                "unknown namespace/kind {}/{}",
                claim.namespace, claim.kind
            )))
        }
    })
}

fn binding_alias_name(kind: BindingAliasKind) -> &'static str {
    match kind {
        BindingAliasKind::ArrangementAsset => "arrangement_asset",
        BindingAliasKind::SequencerSample => "sequencer_sample",
        BindingAliasKind::ArrangementPattern => "arrangement_pattern",
        BindingAliasKind::ArrangementParameter => "arrangement_parameter",
    }
}
fn parse_binding_alias(kind: &str) -> Result<BindingAliasKind, RuntimeCommandCodecError> {
    match kind {
        "arrangement_asset" => Ok(BindingAliasKind::ArrangementAsset),
        "sequencer_sample" => Ok(BindingAliasKind::SequencerSample),
        "arrangement_pattern" => Ok(BindingAliasKind::ArrangementPattern),
        "arrangement_parameter" => Ok(BindingAliasKind::ArrangementParameter),
        _ => Err(RuntimeCommandCodecError::InvalidClaim(format!(
            "unknown binding alias kind {kind}"
        ))),
    }
}
fn air_kind_name(kind: AirEntityKind) -> &'static str {
    match kind {
        AirEntityKind::Source => "source",
        AirEntityKind::Span => "span",
        AirEntityKind::Object => "object",
        AirEntityKind::Transform => "transform",
        AirEntityKind::Parameter => "parameter",
        AirEntityKind::Automation => "automation",
        AirEntityKind::Modulation => "modulation",
        AirEntityKind::Relation => "relation",
        AirEntityKind::Evidence => "evidence",
        AirEntityKind::Hypothesis => "hypothesis",
        AirEntityKind::HypothesisSet => "hypothesis_set",
    }
}
fn parse_air_kind(kind: &str) -> Result<AirEntityKind, RuntimeCommandCodecError> {
    match kind {
        "source" => Ok(AirEntityKind::Source),
        "span" => Ok(AirEntityKind::Span),
        "object" => Ok(AirEntityKind::Object),
        "transform" => Ok(AirEntityKind::Transform),
        "parameter" => Ok(AirEntityKind::Parameter),
        "automation" => Ok(AirEntityKind::Automation),
        "modulation" => Ok(AirEntityKind::Modulation),
        "relation" => Ok(AirEntityKind::Relation),
        "evidence" => Ok(AirEntityKind::Evidence),
        "hypothesis" => Ok(AirEntityKind::Hypothesis),
        "hypothesis_set" => Ok(AirEntityKind::HypothesisSet),
        _ => Err(RuntimeCommandCodecError::InvalidClaim(format!(
            "unknown AIR kind {kind}"
        ))),
    }
}
fn foreign_kind_name(kind: ForeignEntityKind) -> &'static str {
    match kind {
        ForeignEntityKind::Air(kind) => air_kind_name(kind),
        ForeignEntityKind::Pattern => "pattern",
        ForeignEntityKind::PatternClip => "pattern_clip",
        ForeignEntityKind::AutomationLane => "automation_lane",
        ForeignEntityKind::Comparison => "comparison",
        ForeignEntityKind::LexiconEntry => "lexicon_entry",
        ForeignEntityKind::Annotation => "annotation",
    }
}
fn parse_foreign_kind(kind: &str) -> Result<ForeignEntityKind, RuntimeCommandCodecError> {
    Ok(match kind {
        "pattern" => ForeignEntityKind::Pattern,
        "pattern_clip" => ForeignEntityKind::PatternClip,
        "automation_lane" => ForeignEntityKind::AutomationLane,
        "comparison" => ForeignEntityKind::Comparison,
        "lexicon_entry" => ForeignEntityKind::LexiconEntry,
        "annotation" => ForeignEntityKind::Annotation,
        other => ForeignEntityKind::Air(parse_air_kind(other)?),
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AddressValue {
    Id {
        namespace: String,
        entity: String,
        id: u64,
    },
    Singleton {
        namespace: String,
        entity: String,
    },
    AssetUsage {
        asset: u64,
        usage: u64,
    },
    SamplePad {
        kit: u64,
        pad: u64,
    },
    SampleZone {
        kit: u64,
        zone: u64,
    },
    Binding {
        address: BindingAddressValue,
    },
    Air {
        entity: String,
        id: u64,
    },
    WholeDomain {
        domain: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BindingAddressValue {
    kind: String,
    id: u64,
}

fn encode_coalesce(
    token: &CoalesceToken,
) -> Result<DurableCoalesceToken, RuntimeCommandCodecError> {
    let address = encode_address(&token.primary);
    let primary = serde_json::to_string(&address)
        .map_err(|error| RuntimeCommandCodecError::InvalidAddress(error.to_string()))?;
    Ok(DurableCoalesceToken {
        editor_session: token.editor_session,
        gesture_kind: token.gesture_kind,
        primary,
    })
}

fn decode_coalesce(
    token: &DurableCoalesceToken,
) -> Result<CoalesceToken, RuntimeCommandCodecError> {
    let address = serde_json::from_str::<AddressValue>(&token.primary)
        .map_err(|error| RuntimeCommandCodecError::InvalidAddress(error.to_string()))?;
    let canonical = serde_json::to_string(&address)
        .map_err(|error| RuntimeCommandCodecError::InvalidAddress(error.to_string()))?;
    if canonical != token.primary {
        return Err(RuntimeCommandCodecError::InvalidAddress(
            "address is not canonical".into(),
        ));
    }
    Ok(CoalesceToken {
        editor_session: token.editor_session,
        gesture_kind: token.gesture_kind,
        primary: decode_address(address)?,
    })
}

fn encode_address(address: &CommandAddress) -> AddressValue {
    let id = |namespace: &str, entity: &str, id| AddressValue::Id {
        namespace: namespace.into(),
        entity: entity.into(),
        id,
    };
    match address {
        CommandAddress::ArrangementTrack(v) => id("arrangement", "track", v.get()),
        CommandAddress::ArrangementClip(v) => id("arrangement", "clip", v.get()),
        CommandAddress::ArrangementTrackOrder => AddressValue::Singleton {
            namespace: "arrangement".into(),
            entity: "track_order".into(),
        },
        CommandAddress::SequencerPattern(v) => id("sequencer", "pattern", v.get()),
        CommandAddress::SequencerClip(v) => id("sequencer", "clip", v.get()),
        CommandAddress::SequencerTempoMap => AddressValue::Singleton {
            namespace: "sequencer".into(),
            entity: "tempo_map".into(),
        },
        CommandAddress::AutomationLane(v) => id("automation", "lane", v.get()),
        CommandAddress::AutomationPoint(v) => id("automation", "point", v.get()),
        CommandAddress::MixerBus(v) => id("mixer", "bus", v.get()),
        CommandAddress::MixerNode(v) => id("mixer", "node", v.get()),
        CommandAddress::MixerSend(v) => id("mixer", "send", v.get()),
        CommandAddress::MixerProcessor(v) => id("mixer", "processor", v.get()),
        CommandAddress::MixerParameter(v) => id("mixer", "parameter", v.get()),
        CommandAddress::SampleKit(v) => id("sample_kits", "kit", v.get()),
        CommandAddress::SamplePad { kit, pad } => AddressValue::SamplePad {
            kit: kit.get(),
            pad: pad.get(),
        },
        CommandAddress::SampleZone { kit, zone } => AddressValue::SampleZone {
            kit: kit.get(),
            zone: zone.get(),
        },
        CommandAddress::Asset(v) => id("assets", "asset", v.0),
        CommandAddress::AssetUsage { asset, usage } => AddressValue::AssetUsage {
            asset: asset.0,
            usage: usage.0,
        },
        CommandAddress::Binding(v) => AddressValue::Binding {
            address: encode_binding_address(v),
        },
        CommandAddress::Air(v) => AddressValue::Air {
            entity: air_address_name(v).0.into(),
            id: air_address_name(v).1,
        },
        CommandAddress::WholeDomain(v) => AddressValue::WholeDomain {
            domain: domain_name(*v).into(),
        },
    }
}

fn decode_address(value: AddressValue) -> Result<CommandAddress, RuntimeCommandCodecError> {
    Ok(match value {
        AddressValue::Id {
            namespace,
            entity,
            id,
        } => match (namespace.as_str(), entity.as_str()) {
            ("arrangement", "track") => {
                CommandAddress::ArrangementTrack(arrangement::TrackId::from_raw(id))
            }
            ("arrangement", "clip") => {
                CommandAddress::ArrangementClip(arrangement::ClipId::from_raw(id))
            }
            ("sequencer", "pattern") => {
                CommandAddress::SequencerPattern(sequencer::PatternId::from_raw(id))
            }
            ("sequencer", "clip") => {
                CommandAddress::SequencerClip(sequencer::PatternClipId::from_raw(id))
            }
            ("automation", "lane") => {
                CommandAddress::AutomationLane(crate::automation::AutomationLaneId::from_raw(id))
            }
            ("automation", "point") => {
                CommandAddress::AutomationPoint(crate::automation::AutomationPointId::from_raw(id))
            }
            ("mixer", "bus") => CommandAddress::MixerBus(mixer::BusId::from_raw(id)),
            ("mixer", "node") => CommandAddress::MixerNode(mixer::NodeId::from_raw(id)),
            ("mixer", "send") => CommandAddress::MixerSend(mixer::SendId::from_raw(id)),
            ("mixer", "processor") => {
                CommandAddress::MixerProcessor(mixer::ProcessorId::from_raw(id))
            }
            ("mixer", "parameter") => {
                CommandAddress::MixerParameter(mixer::ParameterId::from_raw(id))
            }
            ("sample_kits", "kit") => CommandAddress::SampleKit(sample_kit::KitId::from_raw(id)),
            ("assets", "asset") => CommandAddress::Asset(assets::AssetId(id)),
            _ => {
                return Err(RuntimeCommandCodecError::InvalidAddress(format!(
                    "unknown {namespace}/{entity}"
                )))
            }
        },
        AddressValue::Singleton { namespace, entity } => {
            match (namespace.as_str(), entity.as_str()) {
                ("arrangement", "track_order") => CommandAddress::ArrangementTrackOrder,
                ("sequencer", "tempo_map") => CommandAddress::SequencerTempoMap,
                _ => {
                    return Err(RuntimeCommandCodecError::InvalidAddress(format!(
                        "unknown singleton {namespace}/{entity}"
                    )))
                }
            }
        }
        AddressValue::AssetUsage { asset, usage } => CommandAddress::AssetUsage {
            asset: assets::AssetId(asset),
            usage: assets::AssetUsageId(usage),
        },
        AddressValue::SamplePad { kit, pad } => CommandAddress::SamplePad {
            kit: sample_kit::KitId::from_raw(kit),
            pad: sample_kit::PadId::from_raw(pad),
        },
        AddressValue::SampleZone { kit, zone } => CommandAddress::SampleZone {
            kit: sample_kit::KitId::from_raw(kit),
            zone: sample_kit::ZoneId::from_raw(zone),
        },
        AddressValue::Binding { address } => {
            CommandAddress::Binding(decode_binding_address(address)?)
        }
        AddressValue::Air { entity, id } => CommandAddress::Air(decode_air_address(&entity, id)?),
        AddressValue::WholeDomain { domain } => CommandAddress::WholeDomain(parse_domain(&domain)?),
    })
}

fn encode_binding_address(value: &BindingAddress) -> BindingAddressValue {
    let (kind, id) = match value {
        BindingAddress::ArrangementAsset(v) => ("arrangement_asset", v.get()),
        BindingAddress::SequencerSample(v) => ("sequencer_sample", v.get()),
        BindingAddress::ArrangementPattern(v) => ("arrangement_pattern", v.get()),
        BindingAddress::PatternPlacement(v) => ("pattern_placement", v.get()),
        BindingAddress::ArrangementParameter(v) => ("arrangement_parameter", v.get()),
        BindingAddress::TrackBus(v) => ("track_bus", v.get()),
        BindingAddress::ClipBusOverride(v) => ("clip_bus_override", v.get()),
        BindingAddress::ClipObject(v) => ("clip_object", v.get()),
        BindingAddress::AssetSource(v) => ("asset_source", v.0),
        BindingAddress::AutomationParameter(v) => ("automation_parameter", v.get()),
        BindingAddress::PatternObject(v) => ("pattern_object", v.get()),
    };
    BindingAddressValue {
        kind: kind.into(),
        id,
    }
}
fn decode_binding_address(
    value: BindingAddressValue,
) -> Result<BindingAddress, RuntimeCommandCodecError> {
    Ok(match value.kind.as_str() {
        "arrangement_asset" => {
            BindingAddress::ArrangementAsset(arrangement::AssetId::from_raw(value.id))
        }
        "sequencer_sample" => {
            BindingAddress::SequencerSample(sequencer::SampleAssetId::from_raw(value.id))
        }
        "arrangement_pattern" => {
            BindingAddress::ArrangementPattern(arrangement::PatternId::from_raw(value.id))
        }
        "pattern_placement" => {
            BindingAddress::PatternPlacement(arrangement::ClipId::from_raw(value.id))
        }
        "arrangement_parameter" => {
            BindingAddress::ArrangementParameter(arrangement::ParameterId::from_raw(value.id))
        }
        "track_bus" => BindingAddress::TrackBus(arrangement::TrackId::from_raw(value.id)),
        "clip_bus_override" => {
            BindingAddress::ClipBusOverride(arrangement::ClipId::from_raw(value.id))
        }
        "clip_object" => BindingAddress::ClipObject(arrangement::ClipId::from_raw(value.id)),
        "asset_source" => BindingAddress::AssetSource(assets::AssetId(value.id)),
        "automation_parameter" => BindingAddress::AutomationParameter(
            crate::automation::AutomationLaneId::from_raw(value.id),
        ),
        "pattern_object" => BindingAddress::PatternObject(sequencer::PatternId::from_raw(value.id)),
        _ => {
            return Err(RuntimeCommandCodecError::InvalidAddress(format!(
                "unknown binding address {}",
                value.kind
            )))
        }
    })
}
fn air_address_name(value: &AirAddress) -> (&'static str, u64) {
    match value {
        AirAddress::Source(v) => ("source", v.get()),
        AirAddress::Span(v) => ("span", v.get()),
        AirAddress::Object(v) => ("object", v.get()),
        AirAddress::Transform(v) => ("transform", v.get()),
        AirAddress::Parameter(v) => ("parameter", v.get()),
        AirAddress::Automation(v) => ("automation", v.get()),
        AirAddress::Modulation(v) => ("modulation", v.get()),
        AirAddress::Relation(v) => ("relation", v.get()),
        AirAddress::Evidence(v) => ("evidence", v.get()),
        AirAddress::Hypothesis(v) => ("hypothesis", v.get()),
        AirAddress::HypothesisSet(v) => ("hypothesis_set", v.get()),
    }
}
fn decode_air_address(kind: &str, id: u64) -> Result<AirAddress, RuntimeCommandCodecError> {
    Ok(match kind {
        "source" => AirAddress::Source(ontology::SourceId::new(id)),
        "span" => AirAddress::Span(ontology::SpanId::new(id)),
        "object" => AirAddress::Object(ontology::ObjectId::new(id)),
        "transform" => AirAddress::Transform(ontology::TransformId::new(id)),
        "parameter" => AirAddress::Parameter(ontology::ParameterId::new(id)),
        "automation" => AirAddress::Automation(ontology::AutomationId::new(id)),
        "modulation" => AirAddress::Modulation(ontology::ModulationId::new(id)),
        "relation" => AirAddress::Relation(ontology::RelationId::new(id)),
        "evidence" => AirAddress::Evidence(ontology::EvidenceId::new(id)),
        "hypothesis" => AirAddress::Hypothesis(ontology::HypothesisId::new(id)),
        "hypothesis_set" => AirAddress::HypothesisSet(ontology::HypothesisSetId::new(id)),
        _ => {
            return Err(RuntimeCommandCodecError::InvalidAddress(format!(
                "unknown AIR address {kind}"
            )))
        }
    })
}
fn domain_name(value: ProjectDomain) -> &'static str {
    match value {
        ProjectDomain::Arrangement => "arrangement",
        ProjectDomain::Sequencer => "sequencer",
        ProjectDomain::Automation => "automation",
        ProjectDomain::Assets => "assets",
        ProjectDomain::Mixer => "mixer",
        ProjectDomain::SampleKits => "sample_kits",
        ProjectDomain::Bindings => "bindings",
        ProjectDomain::Air => "air",
    }
}
fn parse_domain(value: &str) -> Result<ProjectDomain, RuntimeCommandCodecError> {
    match value {
        "arrangement" => Ok(ProjectDomain::Arrangement),
        "sequencer" => Ok(ProjectDomain::Sequencer),
        "automation" => Ok(ProjectDomain::Automation),
        "assets" => Ok(ProjectDomain::Assets),
        "mixer" => Ok(ProjectDomain::Mixer),
        "sample_kits" => Ok(ProjectDomain::SampleKits),
        "bindings" => Ok(ProjectDomain::Bindings),
        "air" => Ok(ProjectDomain::Air),
        _ => Err(RuntimeCommandCodecError::InvalidAddress(format!(
            "unknown project domain {value}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        ContentFingerprint, DecodedAudioMetadata, MediaAsset, ProjectRelativePath, SampleFrames,
    };
    use crate::automation::{AutomationLane, AutomationLaneId, ParameterAddress, TimeDomain};
    use crate::command_record::DurableCommandBatch;
    use crate::mixer::{BusId, BusKind, MixerGraph, SendTap};
    use crate::sample_kit::{KitId, SampleKit, SampleRouteIntent};
    use crate::sequencer::{Tempo, TempoMap, TimeSignature};

    fn round_trip(batch: &CommandBatch) -> CommandBatch {
        let durable = DeterministicRuntimeCommandCodec
            .encode_batch(batch)
            .unwrap();
        // This is deliberately a byte boundary and a fresh stateless codec,
        // unlike the former lifecycle test codec's in-memory key lookup.
        let bytes = serde_json::to_vec(&durable).unwrap();
        let reconstructed: DurableCommandBatch = serde_json::from_slice(&bytes).unwrap();
        DeterministicRuntimeCommandCodec
            .decode_batch(&reconstructed)
            .unwrap()
    }

    fn present_asset() -> (assets::AssetId, MediaAsset) {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/tmp/audec-codec.wav").unwrap()),
            Some(ProjectRelativePath::parse("media/audec-codec.wav").unwrap()),
        )
        .unwrap();
        let mut registry = assets::AssetRegistry::new();
        let id = registry
            .register(AssetRegistration {
                name: "hydrated source".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: 48_000,
                    channels: 2,
                    frame_count: SampleFrames(4),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"interleaved-pcm-identity"),
                provenance: AssetProvenance::new(
                    17,
                    AssetOrigin::ImportedFile {
                        importer: "codec-test".into(),
                    },
                    location,
                ),
                tags: BTreeSet::from(["hydrated".into()]),
                favorite: true,
            })
            .unwrap();
        (id, registry.get(id).unwrap().clone())
    }

    #[test]
    fn process_like_round_trip_spans_every_runtime_domain() {
        let tempo = TempoMap::new(
            48_000,
            Tempo::from_bpm(120.0).unwrap(),
            TimeSignature::new(4, 4).unwrap(),
        )
        .unwrap();
        let lane = AutomationLane::new(
            AutomationLaneId::from_raw(71),
            "macro",
            ParameterAddress::Custom {
                namespace: "test".into(),
                entity: "voice".into(),
                parameter: "brightness".into(),
            },
            TimeDomain::Frames,
        );
        let mut mixer = MixerGraph::new("Master");
        let discarded = mixer.add_bus(BusKind::Source, "discarded").unwrap();
        mixer.set_output(discarded, mixer.master()).unwrap();
        mixer.remove_bus(discarded).unwrap();
        let first = MixerCommand::build("add source and return", &mixer, |graph| {
            let bus = graph.add_bus(BusKind::Source, "source")?;
            let room = graph.add_bus(BusKind::Return, "room")?;
            graph.add_send(bus, room, SendTap::PostFader, -12.0)?;
            graph.set_output(bus, graph.master())
        })
        .unwrap();
        let source_bus = first
            .after()
            .buses()
            .find(|bus| bus.kind() == BusKind::Source)
            .unwrap()
            .id();
        let return_bus = first
            .after()
            .buses()
            .find(|bus| bus.kind() == BusKind::Return)
            .unwrap()
            .id();
        assert_eq!(source_bus, BusId::from_raw(3));
        assert_eq!(return_bus, BusId::from_raw(4));
        let second = MixerCommand::build("trim source", first.after(), |graph| {
            graph.set_gain_db(source_bus, -3.0)
        })
        .unwrap();
        let kit = SampleKit::new(
            KitId::from_raw(9),
            "kit",
            SampleRouteIntent::new(mixer.master()).unwrap(),
        );
        let (asset_id, asset) = present_asset();
        let source = ontology::AudioSource {
            id: ontology::SourceId::new(33),
            uri: "asset:1".into(),
            content_digest: Some("exact".into()),
            sample_rate: 48_000,
            channels: 2,
            frame_count: 4,
        };
        let batch = CommandBatch {
            label: "cross-domain recovery".into(),
            coalesce: Some(CoalesceToken {
                editor_session: 4,
                gesture_kind: 8,
                primary: CommandAddress::MixerBus(source_bus),
            }),
            commands: vec![
                DomainCommand::Arrangement(ArrangementOperation::SetTrackOrder {
                    before: vec![],
                    after: vec![],
                }),
                DomainCommand::Sequencer(SequencerCommand::SetTempoMap {
                    before: tempo.clone(),
                    after: tempo,
                }),
                DomainCommand::Automation(AutomationCommand {
                    label: "create lane".into(),
                    changes: vec![LaneChange {
                        before: None,
                        after: Some(lane),
                    }],
                }),
                DomainCommand::Mixer(first),
                DomainCommand::Mixer(second),
                DomainCommand::SampleKits(SampleKitPut {
                    before: None,
                    after: Some(kit),
                }),
                DomainCommand::Assets(AssetCommand::PutAsset {
                    id: asset_id,
                    before: None,
                    after: Some(asset),
                }),
                DomainCommand::Bindings(BindingCommand::PutAssetSourceLink {
                    asset: asset_id,
                    before: None,
                    after: Some(source.id),
                }),
                DomainCommand::Air(AirCommand::PutSource {
                    before: None,
                    after: Some(source),
                }),
            ],
            id_claims: BTreeSet::from([
                IdClaim::MixerBus(source_bus),
                IdClaim::MixerBus(return_bus),
                IdClaim::Air {
                    kind: AirEntityKind::Source,
                    raw: 33,
                },
                IdClaim::Foreign {
                    reading: u128::MAX - 3,
                    kind: ForeignEntityKind::Annotation,
                    local: 91,
                },
            ]),
        };
        assert_eq!(round_trip(&batch), batch);
    }

    #[test]
    fn unknown_command_and_unknown_members_are_refused() {
        let mut durable = DurableCommandBatch::new(
            "future",
            vec![OpaqueCommandRecord {
                domain: "future".into(),
                kind: "put_star".into(),
                schema_version: 7,
                payload: serde_json::json!({"brightness": 1}),
                extensions: BTreeMap::new(),
            }],
        );
        assert!(matches!(
            DeterministicRuntimeCommandCodec.decode_batch(&durable),
            Err(RuntimeCommandCodecError::UnknownCommand { .. })
        ));
        durable.commands[0] = OpaqueCommandRecord {
            domain: "arrangement".into(),
            kind: "set_track_order".into(),
            schema_version: 1,
            payload: serde_json::json!({
                "SetTrackOrder": {"before": [], "after": []},
                "future": true
            }),
            extensions: BTreeMap::new(),
        };
        assert!(matches!(
            DeterministicRuntimeCommandCodec.decode_batch(&durable),
            Err(RuntimeCommandCodecError::InvalidPayload { .. })
        ));
    }
}
