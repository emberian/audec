//! Headless JSONL boundary for reading/query work.
//!
//! Requests may observe, plan a render, or produce a command envelope. This
//! module never applies a command and exposes no alternate project mutation
//! path; a host must route [`HeadlessDispatch::Command`] through the aggregate
//! command executor to obtain validation, journaling, and undo.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use super::{
    execute_query_page, lower_foreign_hypothesis_import, merge_as_coexisting_hypotheses,
    plan_reading_import, validate_geometry, AuditionTarget, ForeignHypothesisMapping,
    QueryCancellation, QueryDocument, QueryExecutionProvenance, QueryPageRequest,
    ReadingImportOptions, ReadingImportPlan, UnknownSectionPolicy, WorkbenchError,
};
use crate::air_query::AirFacts;
use crate::aspect::AspectResolver;
use crate::command::{CommandBatch, CommandEnvelope};
use crate::command_journal::RuntimeCommandCodec;
use crate::command_record::DurableCommandBatch;
use crate::ontology;
use crate::reading::{
    LocalSourceDescriptor, PortableDigest, QualifiedEntityId, ReadingFile, ReadingId,
    ReadingSection, VerificationTier,
};
use crate::runtime_command_codec::{DeterministicRuntimeCommandCodec, RuntimeCommandCodecError};

pub const HEADLESS_PROTOCOL: &str = "audec-reading-query-jsonl-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalSourceDto {
    pub digest: PortableDigest,
    pub sample_rate: u32,
    pub channels: u16,
    pub frame_count: u64,
}

impl From<LocalSourceDto> for LocalSourceDescriptor {
    fn from(value: LocalSourceDto) -> Self {
        Self {
            digest: value.digest,
            sample_rate: value.sample_rate,
            channels: value.channels,
            frame_count: value.frame_count,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadingInputDto {
    pub reading: ReadingFile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_source: Option<LocalSourceDto>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignHypothesisAllocationDto {
    pub foreign: QualifiedEntityId,
    pub project_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HypothesisSetAllocationDto {
    pub group: String,
    pub project_id: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum HeadlessOperation {
    QueryPage {
        document: QueryDocument,
        provenance: QueryExecutionProvenance,
        page: QueryPageRequest,
    },
    VerifyReading {
        reading: ReadingFile,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        local_source: Option<LocalSourceDto>,
    },
    PlanReadingImport {
        reading: ReadingFile,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        local_source: Option<LocalSourceDto>,
        #[serde(default)]
        existing: Vec<QualifiedEntityId>,
        unknown_sections: UnknownSectionPolicy,
    },
    ImportHypotheses {
        readings: Vec<ReadingInputDto>,
        #[serde(default)]
        existing: Vec<QualifiedEntityId>,
        unknown_sections: UnknownSectionPolicy,
        base_revision: u64,
        hypothesis_allocations: Vec<ForeignHypothesisAllocationDto>,
        #[serde(default)]
        set_allocations: Vec<HypothesisSetAllocationDto>,
    },
    Command {
        base_revision: u64,
        batch: DurableCommandBatch,
    },
    Render {
        target: AuditionTarget,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadlessRequest {
    pub protocol: String,
    pub request_id: String,
    #[serde(flatten)]
    pub operation: HeadlessOperation,
}

impl HeadlessRequest {
    pub fn from_jsonl(line: &str) -> Result<Self, HeadlessProtocolError> {
        if line.contains(['\n', '\r']) {
            return Err(HeadlessProtocolError::Framing(
                "one JSONL request must occupy exactly one line".into(),
            ));
        }
        let request = serde_json::from_str::<Self>(line)
            .map_err(|error| HeadlessProtocolError::Json(error.to_string()))?;
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> Result<(), HeadlessProtocolError> {
        if self.protocol != HEADLESS_PROTOCOL {
            return Err(HeadlessProtocolError::UnsupportedProtocol(
                self.protocol.clone(),
            ));
        }
        if self.request_id.trim().is_empty() {
            return Err(HeadlessProtocolError::InvalidRequest(
                "request_id cannot be blank".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationTierDto {
    GraphOnly,
    SourceMatched,
    Replicated,
}

impl From<VerificationTier> for VerificationTierDto {
    fn from(value: VerificationTier) -> Self {
        match value {
            VerificationTier::GraphOnly => Self::GraphOnly,
            VerificationTier::SourceMatched => Self::SourceMatched,
            VerificationTier::Replicated => Self::Replicated,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportPlanSummaryDto {
    pub reading_id: ReadingId,
    pub reading_revision: u64,
    pub verification: VerificationTierDto,
    pub entities: Vec<QualifiedEntityId>,
    pub preserved_sections: Vec<ReadingSection>,
}

impl From<&ReadingImportPlan> for ImportPlanSummaryDto {
    fn from(value: &ReadingImportPlan) -> Self {
        Self {
            reading_id: value.reading_id,
            reading_revision: value.reading_revision,
            verification: value.verification.into(),
            entities: value
                .entities
                .iter()
                .map(|entity| entity.id.clone())
                .collect(),
            preserved_sections: value.preserved_sections.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignMappingDto {
    pub foreign: QualifiedEntityId,
    pub project_id: u64,
}

impl From<&ForeignHypothesisMapping> for ForeignMappingDto {
    fn from(value: &ForeignHypothesisMapping) -> Self {
        Self {
            foreign: value.foreign.clone(),
            project_id: value.project.get(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreservedSectionDto {
    pub reading_id: ReadingId,
    pub reading_revision: u64,
    pub section: ReadingSection,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum HeadlessResponseBody {
    QueryPage {
        document: QueryDocument,
    },
    ReadingVerified {
        tier: VerificationTierDto,
    },
    ImportPlanned {
        plan: ImportPlanSummaryDto,
    },
    CommandPlanned {
        base_revision: u64,
        batch: DurableCommandBatch,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mappings: Vec<ForeignMappingDto>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        preserved_sections: Vec<PreservedSectionDto>,
    },
    RenderPlanned {
        target: AuditionTarget,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadlessResponse {
    pub protocol: String,
    pub request_id: String,
    #[serde(flatten)]
    pub body: HeadlessResponseBody,
}

impl HeadlessResponse {
    pub fn to_jsonl(&self) -> Result<String, HeadlessProtocolError> {
        let mut line = serde_json::to_string(self)
            .map_err(|error| HeadlessProtocolError::Json(error.to_string()))?;
        line.push('\n');
        Ok(line)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum HeadlessDispatch {
    Observation(HeadlessResponse),
    Command {
        response: HeadlessResponse,
        envelope: CommandEnvelope,
    },
    Render {
        response: HeadlessResponse,
        target: AuditionTarget,
    },
}

impl HeadlessDispatch {
    pub fn response(&self) -> &HeadlessResponse {
        match self {
            Self::Observation(response)
            | Self::Command { response, .. }
            | Self::Render { response, .. } => response,
        }
    }
}

/// Stateless adapter suitable for a CLI loop or a later pane/session bridge.
/// It returns effects but owns no project and therefore cannot apply them.
pub struct HeadlessSessionAdapter<'a> {
    facts: &'a dyn AirFacts,
    resolver: &'a dyn AspectResolver,
}

impl<'a> HeadlessSessionAdapter<'a> {
    pub fn new(facts: &'a dyn AirFacts, resolver: &'a dyn AspectResolver) -> Self {
        Self { facts, resolver }
    }

    pub fn dispatch_jsonl(
        &self,
        line: &str,
        cancellation: &dyn QueryCancellation,
    ) -> Result<HeadlessDispatch, HeadlessProtocolError> {
        self.dispatch(HeadlessRequest::from_jsonl(line)?, cancellation)
    }

    pub fn dispatch(
        &self,
        request: HeadlessRequest,
        cancellation: &dyn QueryCancellation,
    ) -> Result<HeadlessDispatch, HeadlessProtocolError> {
        request.validate()?;
        let request_id = request.request_id;
        let response = |body| HeadlessResponse {
            protocol: HEADLESS_PROTOCOL.into(),
            request_id: request_id.clone(),
            body,
        };
        match request.operation {
            HeadlessOperation::QueryPage {
                mut document,
                provenance,
                page,
            } => {
                execute_query_page(
                    &mut document,
                    self.facts,
                    self.resolver,
                    provenance,
                    page,
                    cancellation,
                )?;
                Ok(HeadlessDispatch::Observation(response(
                    HeadlessResponseBody::QueryPage { document },
                )))
            }
            HeadlessOperation::VerifyReading {
                reading,
                local_source,
            } => {
                reading
                    .validate()
                    .map_err(|error| HeadlessProtocolError::Reading(error.to_string()))?;
                let local = local_source.map(Into::into);
                let tier = reading
                    .verify_source(local.as_ref())
                    .map_err(|error| HeadlessProtocolError::Reading(format!("{error:?}")))?;
                Ok(HeadlessDispatch::Observation(response(
                    HeadlessResponseBody::ReadingVerified { tier: tier.into() },
                )))
            }
            HeadlessOperation::PlanReadingImport {
                reading,
                local_source,
                existing,
                unknown_sections,
            } => {
                let local = local_source.map(Into::into);
                let plan = plan_reading_import(
                    &reading,
                    local.as_ref(),
                    &existing.into_iter().collect(),
                    ReadingImportOptions {
                        unknown_sections,
                        require_entity_section: true,
                    },
                )
                .map_err(|error| HeadlessProtocolError::Reading(format!("{error:?}")))?;
                Ok(HeadlessDispatch::Observation(response(
                    HeadlessResponseBody::ImportPlanned {
                        plan: (&plan).into(),
                    },
                )))
            }
            HeadlessOperation::ImportHypotheses {
                readings,
                existing,
                unknown_sections,
                base_revision,
                hypothesis_allocations,
                set_allocations,
            } => {
                if readings.is_empty() {
                    return Err(HeadlessProtocolError::InvalidRequest(
                        "an import requires at least one reading".into(),
                    ));
                }
                let existing = existing.into_iter().collect::<BTreeSet<_>>();
                let mut plans = Vec::with_capacity(readings.len());
                for input in readings {
                    let local = input.local_source.map(Into::into);
                    plans.push(
                        plan_reading_import(
                            &input.reading,
                            local.as_ref(),
                            &existing,
                            ReadingImportOptions {
                                unknown_sections,
                                require_entity_section: true,
                            },
                        )
                        .map_err(|error| HeadlessProtocolError::Reading(format!("{error:?}")))?,
                    );
                }
                let merge = merge_as_coexisting_hypotheses(&plans)
                    .map_err(|error| HeadlessProtocolError::Reading(format!("{error:?}")))?;
                let hypothesis_allocations =
                    collect_hypothesis_allocations(hypothesis_allocations)?;
                let set_allocations = collect_set_allocations(set_allocations)?;
                let lowered = lower_foreign_hypothesis_import(
                    &merge,
                    base_revision,
                    &hypothesis_allocations,
                    &set_allocations,
                )
                .map_err(|error| HeadlessProtocolError::Command(format!("{error:?}")))?;
                let batch = lowered.envelope.as_batch();
                let durable = DeterministicRuntimeCommandCodec.encode_batch(&batch)?;
                let preserved_sections = merge
                    .preserved_sections
                    .into_iter()
                    .flat_map(|((reading_id, reading_revision), sections)| {
                        sections
                            .into_iter()
                            .map(move |section| PreservedSectionDto {
                                reading_id,
                                reading_revision,
                                section,
                            })
                    })
                    .collect();
                let command_response = response(HeadlessResponseBody::CommandPlanned {
                    base_revision,
                    batch: durable,
                    mappings: lowered.mappings.iter().map(Into::into).collect(),
                    preserved_sections,
                });
                Ok(HeadlessDispatch::Command {
                    response: command_response,
                    envelope: lowered.envelope,
                })
            }
            HeadlessOperation::Command {
                base_revision,
                batch,
            } => {
                let decoded: CommandBatch =
                    DeterministicRuntimeCommandCodec.decode_batch(&batch)?;
                if decoded.label.trim().is_empty() || decoded.commands.is_empty() {
                    return Err(HeadlessProtocolError::Command(
                        "command batch needs a label and at least one command".into(),
                    ));
                }
                let canonical = DeterministicRuntimeCommandCodec.encode_batch(&decoded)?;
                let envelope = CommandEnvelope::from_batch(base_revision, decoded);
                Ok(HeadlessDispatch::Command {
                    response: response(HeadlessResponseBody::CommandPlanned {
                        base_revision,
                        batch: canonical,
                        mappings: Vec::new(),
                        preserved_sections: Vec::new(),
                    }),
                    envelope,
                })
            }
            HeadlessOperation::Render { target } => {
                target
                    .entity
                    .validate()
                    .map_err(|error| HeadlessProtocolError::Render(format!("{error:?}")))?;
                validate_geometry(&target.extent)?;
                Ok(HeadlessDispatch::Render {
                    response: response(HeadlessResponseBody::RenderPlanned {
                        target: target.clone(),
                    }),
                    target,
                })
            }
        }
    }
}

fn collect_hypothesis_allocations(
    values: Vec<ForeignHypothesisAllocationDto>,
) -> Result<BTreeMap<QualifiedEntityId, ontology::HypothesisId>, HeadlessProtocolError> {
    let mut output = BTreeMap::new();
    for value in values {
        let id = ontology::HypothesisId::new(value.project_id);
        if output.insert(value.foreign.clone(), id).is_some() {
            return Err(HeadlessProtocolError::InvalidRequest(format!(
                "duplicate allocation for {:?}",
                value.foreign
            )));
        }
    }
    Ok(output)
}

fn collect_set_allocations(
    values: Vec<HypothesisSetAllocationDto>,
) -> Result<BTreeMap<String, ontology::HypothesisSetId>, HeadlessProtocolError> {
    let mut output = BTreeMap::new();
    for value in values {
        if value.group.trim().is_empty()
            || output
                .insert(
                    value.group.clone(),
                    ontology::HypothesisSetId::new(value.project_id),
                )
                .is_some()
        {
            return Err(HeadlessProtocolError::InvalidRequest(format!(
                "invalid or duplicate hypothesis group {:?}",
                value.group
            )));
        }
    }
    Ok(output)
}

#[derive(Debug)]
pub enum HeadlessProtocolError {
    Framing(String),
    Json(String),
    UnsupportedProtocol(String),
    InvalidRequest(String),
    Reading(String),
    Command(String),
    Render(String),
    Workbench(WorkbenchError),
    Codec(RuntimeCommandCodecError),
}

impl From<WorkbenchError> for HeadlessProtocolError {
    fn from(value: WorkbenchError) -> Self {
        Self::Workbench(value)
    }
}

impl From<RuntimeCommandCodecError> for HeadlessProtocolError {
    fn from(value: RuntimeCommandCodecError) -> Self {
        Self::Codec(value)
    }
}

impl fmt::Display for HeadlessProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "headless reading/query protocol error: {self:?}")
    }
}

impl std::error::Error for HeadlessProtocolError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::air_query::{FactKind, FactRef, NeverCancel};
    use crate::aspect::{
        BandSpan, ChannelMask, ConcreteAspect, ConcreteRegion, ExplanationRef, FrameSpan,
        SignalLayer,
    };
    use crate::command::{claims_for_commands, AirCommand, DomainCommand};
    use crate::interpretation_navigation::{
        AspectGeometryDto, EntityRefDto, RegionDto, SignalLayerDto,
    };

    struct EmptyProject;

    impl AirFacts for EmptyProject {
        fn facts(&self, _kind: FactKind) -> Vec<FactRef> {
            Vec::new()
        }

        fn evidence_of(&self, _fact: FactRef) -> Vec<FactRef> {
            Vec::new()
        }

        fn related(&self, _fact: FactRef) -> Vec<FactRef> {
            Vec::new()
        }

        fn extent(&self, _fact: FactRef) -> Option<ConcreteAspect> {
            None
        }
    }

    impl AspectResolver for EmptyProject {
        fn universe(&self) -> ConcreteAspect {
            ConcreteAspect::new(
                vec![ConcreteRegion {
                    time: FrameSpan::new(0, 100).unwrap(),
                    band: BandSpan::new(0.0, 24_000.0).unwrap(),
                    channels: ChannelMask(1),
                }],
                SignalLayer::Source,
            )
            .unwrap()
        }

        fn family_spans(
            &self,
            _analysis: &crate::aspect::AnalysisRef,
            _id: usize,
        ) -> Option<Vec<FrameSpan>> {
            None
        }

        fn object_extent(&self, _object: ontology::ObjectId) -> Option<ConcreteAspect> {
            None
        }

        fn explanation_extent(&self, _reference: &ExplanationRef) -> Option<ConcreteAspect> {
            None
        }
    }

    fn target() -> AuditionTarget {
        let entity = EntityRefDto::Project {
            kind: "comparison".into(),
            local_id: 7,
        };
        AuditionTarget {
            entity: entity.clone(),
            extent: AspectGeometryDto {
                regions: vec![RegionDto {
                    start_frame: 0,
                    end_frame: 10,
                    min_hz_bits: 0.0_f32.to_bits(),
                    max_hz_bits: 1_000.0_f32.to_bits(),
                    channels: 1,
                }],
                objects: Vec::new(),
                signal: SignalLayerDto::Residual { reference: entity },
            },
        }
    }

    #[test]
    fn jsonl_is_one_line_strict_and_render_is_only_a_typed_effect() {
        let project = EmptyProject;
        let adapter = HeadlessSessionAdapter::new(&project, &project);
        let request = HeadlessRequest {
            protocol: HEADLESS_PROTOCOL.into(),
            request_id: "render-1".into(),
            operation: HeadlessOperation::Render { target: target() },
        };
        let line = serde_json::to_string(&request).unwrap();
        let dispatch = adapter.dispatch_jsonl(&line, &NeverCancel).unwrap();
        assert!(matches!(dispatch, HeadlessDispatch::Render { .. }));
        let response = dispatch.response().to_jsonl().unwrap();
        assert!(response.ends_with('\n'));
        assert_eq!(response.matches('\n').count(), 1);
        assert!(HeadlessRequest::from_jsonl(&(line + "\n")).is_err());

        let unknown = format!(
            r#"{{"protocol":"{HEADLESS_PROTOCOL}","request_id":"x","operation":"render","target":{},"surprise":true}}"#,
            serde_json::to_string(&target()).unwrap()
        );
        assert!(HeadlessRequest::from_jsonl(&unknown).is_err());
    }

    #[test]
    fn command_request_decodes_to_envelope_without_applying_project_state() {
        let project = EmptyProject;
        let adapter = HeadlessSessionAdapter::new(&project, &project);
        let hypothesis = ontology::Hypothesis {
            id: ontology::HypothesisId::new(9),
            label: "foreign possibility".into(),
            claims: Vec::new(),
            support: 0.4,
            evidence: Vec::new(),
            provenance: ontology::Provenance {
                producer: ontology::Producer::Human { name: None },
                created_unix_ms: None,
                source_revision: None,
                note: None,
            },
        };
        let commands = vec![DomainCommand::Air(AirCommand::PutHypothesis {
            before: None,
            after: Some(hypothesis),
        })];
        let batch = CommandBatch {
            label: "import possibility".into(),
            coalesce: None,
            id_claims: claims_for_commands(&commands),
            commands,
        };
        let durable = DeterministicRuntimeCommandCodec
            .encode_batch(&batch)
            .unwrap();
        let request = HeadlessRequest {
            protocol: HEADLESS_PROTOCOL.into(),
            request_id: "command-1".into(),
            operation: HeadlessOperation::Command {
                base_revision: 12,
                batch: durable,
            },
        };
        let dispatch = adapter.dispatch(request, &NeverCancel).unwrap();
        let HeadlessDispatch::Command { envelope, .. } = dispatch else {
            panic!("command requests must return only a command effect")
        };
        assert_eq!(envelope.base_revision, 12);
        assert_eq!(envelope.commands.len(), 1);
    }
}
