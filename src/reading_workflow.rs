//! End-to-end, UI-neutral reading exchange workflow.
//!
//! This module composes the reading codec, exact local-source resolver,
//! semantic diff, qualified import inventory, query-style result navigation,
//! and command-envelope lowering. It returns data and effects only: applying
//! the one command envelope remains the aggregate session's responsibility.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::Value;

use super::{
    diff_readings, lower_foreign_hypothesis_import, merge_as_coexisting_hypotheses,
    plan_reading_import, AuditionTarget, ForeignImportRefusal, ImportDisposition,
    PortableEntityRecord, PortableEntityRole, PortableEntitySection, PortableHypothesisSemantics,
    ReadingDiff, ReadingImportOptions, ReadingImportPlan, ReadingImportRefusal, ReadingMergePlan,
    ReadingMergeRefusal, RevealTarget, UndoableForeignImport, UnknownSectionPolicy, WorkbenchError,
    ENTITY_SECTION_MAJOR, ENTITY_SECTION_NAME,
};
use crate::interpretation_navigation::{
    AspectGeometryDto, EntityRefDto, QueryDerivationDto, QueryHitDto, SignalLayerDto,
};
use crate::ontology::{HypothesisId, HypothesisSetId};
use crate::reading::{
    LocalSourceDescriptor, PortableDigest, ProvenanceDto, QualifiedEntityId, ReadingAttachmentRef,
    ReadingError, ReadingFile, ReadingId, ReadingSection, ReadingSource,
    ReadingVerificationRefusal, ReadingVersionRef, VerificationTier, READING_FORMAT,
    READING_FORMAT_VERSION,
};
use crate::reading_codec::{
    decode_verified_reading, encode_reading_with_manifest, EncodedReading, ReadingCodecError,
};

/// Opaque host identity for one locally resolvable copy of source material.
/// Paths deliberately do not cross the reading boundary.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalMaterialId(pub String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalReadingMaterial {
    pub id: LocalMaterialId,
    pub label: String,
    pub source: LocalSourceDescriptor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectedLocalMaterial {
    pub id: LocalMaterialId,
    pub label: String,
    pub reason: ReadingVerificationRefusal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingSourceDiagnostic {
    pub reading_id: ReadingId,
    pub declared_title: Option<String>,
    pub expected_fingerprints: Vec<PortableDigest>,
    pub sample_rate: u32,
    pub channels: u16,
    pub frame_count: u64,
    pub rejected: Vec<RejectedLocalMaterial>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadingSourceResolution {
    Matched {
        primary: LocalReadingMaterial,
        /// Other byte-identical local copies. They are not silently collapsed,
        /// but all have equal standing for source verification.
        equivalents: Vec<LocalReadingMaterial>,
        rejected: Vec<RejectedLocalMaterial>,
    },
    Missing(MissingSourceDiagnostic),
}

impl ReadingSourceResolution {
    pub fn verification(&self) -> VerificationTier {
        match self {
            Self::Matched { .. } => VerificationTier::SourceMatched,
            Self::Missing(_) => VerificationTier::GraphOnly,
        }
    }

    pub fn primary(&self) -> Option<&LocalReadingMaterial> {
        match self {
            Self::Matched { primary, .. } => Some(primary),
            Self::Missing(_) => None,
        }
    }

    pub fn rejected(&self) -> &[RejectedLocalMaterial] {
        match self {
            Self::Matched { rejected, .. } => rejected,
            Self::Missing(diagnostic) => &diagnostic.rejected,
        }
    }
}

/// Find exact local material without treating a near match as permission.
/// An empty/no-match catalog is a graph-only resolution; malformed catalog
/// identities and malformed readings are typed refusals.
pub fn resolve_reading_source(
    reading: &ReadingFile,
    materials: &[LocalReadingMaterial],
) -> Result<ReadingSourceResolution, ReadingWorkflowRefusal> {
    reading
        .validate()
        .map_err(ReadingWorkflowRefusal::InvalidReading)?;
    let mut seen = BTreeSet::new();
    let mut matched = Vec::new();
    let mut rejected = Vec::new();
    for material in materials {
        validate_local_material(material)?;
        if !seen.insert(material.id.clone()) {
            return Err(ReadingWorkflowRefusal::DuplicateLocalMaterial(
                material.id.clone(),
            ));
        }
        match reading.verify_source(Some(&material.source)) {
            Ok(VerificationTier::SourceMatched) => matched.push(material.clone()),
            Ok(other) => {
                return Err(ReadingWorkflowRefusal::UnexpectedVerificationTier(other));
            }
            Err(reason) => rejected.push(RejectedLocalMaterial {
                id: material.id.clone(),
                label: material.label.clone(),
                reason,
            }),
        }
    }
    matched.sort_by(|left, right| left.id.cmp(&right.id));
    rejected.sort_by(|left, right| left.id.cmp(&right.id));
    if matched.is_empty() {
        let diagnostic = MissingSourceDiagnostic {
            reading_id: reading.reading_id,
            declared_title: reading.source.declared_title.clone(),
            expected_fingerprints: reading.source.fingerprints.clone(),
            sample_rate: reading.source.sample_rate,
            channels: reading.source.channels,
            frame_count: reading.source.frame_count,
            rejected,
        };
        // No candidate is an honest tier-1 import. A candidate that was
        // supplied and failed exact verification is tier 3 and must stop the
        // import instead of being re-described as merely absent.
        if diagnostic.rejected.is_empty() {
            return Ok(ReadingSourceResolution::Missing(diagnostic));
        }
        return Err(ReadingWorkflowRefusal::SourceCandidatesRefused(diagnostic));
    }
    let primary = matched.remove(0);
    Ok(ReadingSourceResolution::Matched {
        primary,
        equivalents: matched,
        rejected,
    })
}

fn validate_local_material(material: &LocalReadingMaterial) -> Result<(), ReadingWorkflowRefusal> {
    if material.id.0.trim().is_empty() || material.label.trim().is_empty() {
        return Err(ReadingWorkflowRefusal::InvalidLocalMaterial {
            id: material.id.clone(),
            detail: "material identity and label must be non-blank".into(),
        });
    }
    if !material.source.digest.is_strong()
        || material.source.sample_rate == 0
        || material.source.channels == 0
        || material.source.frame_count == 0
    {
        return Err(ReadingWorkflowRefusal::InvalidLocalMaterial {
            id: material.id.clone(),
            detail: "material needs a strong digest and non-zero decoded geometry".into(),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReadingExportRequest {
    pub reading_id: ReadingId,
    pub revision: u64,
    pub parents: Vec<ReadingVersionRef>,
    pub author: ProvenanceDto,
    pub source: ReadingSource,
    /// Records use project-local references on input. Export qualifies every
    /// such reference with `reading_id` before any bytes are emitted.
    pub entities: Vec<PortableEntityRecord>,
    pub additional_sections: Vec<ReadingSection>,
    pub attachments: Vec<ReadingAttachmentRef>,
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExportedReading {
    pub reading: ReadingFile,
    pub encoded: EncodedReading,
}

impl ExportedReading {
    pub fn verify(&self) -> Result<ReadingFile, ReadingCodecError> {
        decode_verified_reading(&self.encoded.bytes, self.encoded.manifest_digest)
    }
}

/// Build the portable entity section, qualify references, validate source
/// coordinates, canonicalize the envelope, and return its detached manifest
/// identity. No attachment bytes or source PCM are accepted by this API.
pub fn export_reading(
    mut request: ReadingExportRequest,
) -> Result<ExportedReading, ReadingWorkflowRefusal> {
    if request
        .additional_sections
        .iter()
        .any(|section| section.name == ENTITY_SECTION_NAME)
    {
        return Err(ReadingWorkflowRefusal::ReservedSection(
            ENTITY_SECTION_NAME.into(),
        ));
    }

    let mut ids = BTreeSet::new();
    for record in &mut request.entities {
        let id = QualifiedEntityId::new(request.reading_id, record.kind.clone(), record.local_id)
            .map_err(ReadingWorkflowRefusal::InvalidReading)?;
        if !ids.insert(id.clone()) {
            return Err(ReadingWorkflowRefusal::DuplicateExportEntity(id));
        }
        validate_export_record(record, &id)?;
        if let Some(extent) = record.extent.take() {
            let extent = extent
                .qualify_project_ids(request.reading_id)
                .map_err(|error| ReadingWorkflowRefusal::InvalidExportGeometry {
                    entity: id.clone(),
                    detail: format!("{error:?}"),
                })?;
            validate_export_extent(&extent, &request.source, &id, request.reading_id)?;
            record.extent = Some(extent);
        }
    }
    request
        .entities
        .sort_by(|left, right| (&left.kind, left.local_id).cmp(&(&right.kind, right.local_id)));

    let entity_section = ReadingSection {
        name: ENTITY_SECTION_NAME.into(),
        schema_major: ENTITY_SECTION_MAJOR,
        schema_minor: 0,
        payload: serde_json::to_value(PortableEntitySection {
            entities: request.entities,
            extensions: BTreeMap::new(),
        })
        .map_err(|error| {
            ReadingWorkflowRefusal::Codec(ReadingCodecError::SectionJson {
                name: ENTITY_SECTION_NAME.into(),
                message: error.to_string(),
            })
        })?,
        extensions: BTreeMap::new(),
    };
    let mut sections = request.additional_sections;
    sections.push(entity_section);
    let reading = ReadingFile {
        format: READING_FORMAT.into(),
        version: READING_FORMAT_VERSION,
        reading_id: request.reading_id,
        revision: request.revision,
        parents: request.parents,
        author: request.author,
        source: request.source,
        sections,
        attachments: request.attachments,
        extensions: request.extensions,
    };
    reading
        .validate()
        .map_err(ReadingWorkflowRefusal::InvalidReading)?;
    // Reuse the import inventory as an export postcondition. This catches
    // payload/qualified-identity drift before the reading can leave the host.
    plan_reading_import(
        &reading,
        None,
        &BTreeSet::new(),
        ReadingImportOptions::default(),
    )
    .map_err(ReadingWorkflowRefusal::Import)?;
    let encoded = encode_reading_with_manifest(&reading).map_err(ReadingWorkflowRefusal::Codec)?;
    Ok(ExportedReading { reading, encoded })
}

fn validate_export_record(
    record: &PortableEntityRecord,
    id: &QualifiedEntityId,
) -> Result<(), ReadingWorkflowRefusal> {
    if record.label.trim().is_empty() {
        return Err(ReadingWorkflowRefusal::BlankExportLabel(id.clone()));
    }
    if record
        .hypothesis_group
        .as_ref()
        .is_some_and(|group| group.trim().is_empty())
    {
        return Err(ReadingWorkflowRefusal::BlankHypothesisGroup(id.clone()));
    }
    match (&record.role, &record.hypothesis) {
        (PortableEntityRole::Hypothesis, Some(semantics))
            if semantics.support.is_finite()
                && (0.0..=1.0).contains(&semantics.support)
                && semantics
                    .description
                    .as_ref()
                    .is_none_or(|description| !description.trim().is_empty()) =>
        {
            Ok(())
        }
        (PortableEntityRole::Hypothesis, None) => Ok(()),
        (PortableEntityRole::Hypothesis, Some(_)) | (_, Some(_)) => {
            Err(ReadingWorkflowRefusal::InvalidExportHypothesis(id.clone()))
        }
        (_, None) => Ok(()),
    }
}

fn validate_export_extent(
    extent: &AspectGeometryDto,
    source: &ReadingSource,
    entity: &QualifiedEntityId,
    reading: ReadingId,
) -> Result<(), ReadingWorkflowRefusal> {
    super::validate_geometry(extent).map_err(|error| {
        ReadingWorkflowRefusal::InvalidExportGeometry {
            entity: entity.clone(),
            detail: error.to_string(),
        }
    })?;
    for region in &extent.regions {
        let end = u64::try_from(region.end_frame).ok();
        if region.start_frame < 0 || end.is_none_or(|end| end > source.frame_count) {
            return Err(ReadingWorkflowRefusal::ExtentOutsideSource {
                entity: entity.clone(),
                start_frame: region.start_frame,
                end_frame: region.end_frame,
                source_frames: source.frame_count,
            });
        }
    }
    for reference in extent.objects.iter().chain(match &extent.signal {
        SignalLayerDto::Source => None,
        SignalLayerDto::Explanation { reference } | SignalLayerDto::Residual { reference } => {
            Some(reference)
        }
    }) {
        if let EntityRefDto::Reading(id) = reference {
            if id.reading != reading {
                return Err(ReadingWorkflowRefusal::ForeignReadingReference {
                    entity: entity.clone(),
                    reference: id.clone(),
                });
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticChangeKind {
    AddedQuestion,
    RemovedQuestion,
    EquivalentAlternatives,
    ChangedAlternatives,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SemanticAlternative {
    pub id: QualifiedEntityId,
    pub label: String,
    pub semantics: Option<PortableHypothesisSemantics>,
    pub extent: Option<AspectGeometryDto>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SemanticHypothesisChange {
    pub question: String,
    pub kind: SemanticChangeKind,
    pub left: Vec<SemanticAlternative>,
    pub right: Vec<SemanticAlternative>,
    pub extents_overlap: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SemanticReadingDiff {
    pub structural: ReadingDiff,
    pub hypotheses: Vec<SemanticHypothesisChange>,
}

/// Compare explicit question/group semantics across reading namespaces.
/// Qualified identities remain distinct; equivalence only describes the
/// authored alternative payload and never aliases or selects an entity.
pub fn semantic_diff_readings(
    left: &ReadingFile,
    right: &ReadingFile,
) -> Result<SemanticReadingDiff, ReadingWorkflowRefusal> {
    let structural = diff_readings(left, right).map_err(ReadingWorkflowRefusal::Import)?;
    let left = plan_reading_import(
        left,
        None,
        &BTreeSet::new(),
        ReadingImportOptions::default(),
    )
    .map_err(ReadingWorkflowRefusal::Import)?;
    let right = plan_reading_import(
        right,
        None,
        &BTreeSet::new(),
        ReadingImportOptions::default(),
    )
    .map_err(ReadingWorkflowRefusal::Import)?;
    let left_groups = semantic_groups(&left);
    let right_groups = semantic_groups(&right);
    let questions = left_groups
        .keys()
        .chain(right_groups.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut hypotheses = Vec::with_capacity(questions.len());
    for question in questions {
        let left = left_groups.get(&question).cloned().unwrap_or_default();
        let right = right_groups.get(&question).cloned().unwrap_or_default();
        let kind = match (left.is_empty(), right.is_empty()) {
            (true, false) => SemanticChangeKind::AddedQuestion,
            (false, true) => SemanticChangeKind::RemovedQuestion,
            (false, false) if alternatives_equivalent(&left, &right) => {
                SemanticChangeKind::EquivalentAlternatives
            }
            (false, false) => SemanticChangeKind::ChangedAlternatives,
            (true, true) => unreachable!("question came from at least one side"),
        };
        let extents_overlap = left.iter().any(|left| {
            right.iter().any(|right| {
                left.extent
                    .as_ref()
                    .zip(right.extent.as_ref())
                    .is_some_and(|(left, right)| geometry_overlaps(left, right))
            })
        });
        hypotheses.push(SemanticHypothesisChange {
            question,
            kind,
            left,
            right,
            extents_overlap,
        });
    }
    Ok(SemanticReadingDiff {
        structural,
        hypotheses,
    })
}

fn semantic_groups(plan: &ReadingImportPlan) -> BTreeMap<String, Vec<SemanticAlternative>> {
    let mut groups = BTreeMap::<String, Vec<SemanticAlternative>>::new();
    for entity in plan
        .entities
        .iter()
        .filter(|entity| entity.role == PortableEntityRole::Hypothesis)
    {
        let Some(question) = entity.hypothesis_group.as_ref() else {
            continue;
        };
        groups
            .entry(question.clone())
            .or_default()
            .push(SemanticAlternative {
                id: entity.id.clone(),
                label: entity.label.clone(),
                semantics: entity.hypothesis.clone(),
                extent: entity.extent.clone(),
            });
    }
    for alternatives in groups.values_mut() {
        alternatives.sort_by(|left, right| left.id.cmp(&right.id));
    }
    groups
}

fn alternatives_equivalent(left: &[SemanticAlternative], right: &[SemanticAlternative]) -> bool {
    let normalized = |values: &[SemanticAlternative]| {
        let mut values = values
            .iter()
            .map(|value| {
                (
                    value.label.clone(),
                    value
                        .semantics
                        .as_ref()
                        .map(|semantics| semantics.support.to_bits()),
                    value
                        .semantics
                        .as_ref()
                        .and_then(|semantics| semantics.description.clone()),
                    value.extent.clone(),
                )
            })
            .collect::<Vec<_>>();
        values.sort_by(|left, right| {
            (&left.0, left.1, &left.2)
                .cmp(&(&right.0, right.1, &right.2))
                .then_with(|| format!("{:?}", left.3).cmp(&format!("{:?}", right.3)))
        });
        values
    };
    normalized(left) == normalized(right)
}

fn geometry_overlaps(left: &AspectGeometryDto, right: &AspectGeometryDto) -> bool {
    left.regions.iter().any(|left| {
        right.regions.iter().any(|right| {
            left.start_frame < right.end_frame
                && right.start_frame < left.end_frame
                && left.min_hz() < right.max_hz()
                && right.min_hz() < left.max_hz()
                && left.channels & right.channels != 0
        })
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedAuditionRequest {
    pub material: LocalMaterialId,
    pub target: AuditionTarget,
}

/// Query-style row that all panes/headless clients can reveal, and can
/// audition only when exact local material is attached.
#[derive(Clone, Debug, PartialEq)]
pub struct ReadingResultRow {
    pub label: String,
    pub disposition: ImportDisposition,
    pub hit: QueryHitDto,
    pub reveal: RevealTarget,
    pub audition: Option<ResolvedAuditionRequest>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadingWorkflowDiagnosticCode {
    SourceMissing,
    LocalMaterialRejected,
    OpaqueSectionPreserved,
    AuditionExtentMissing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadingWorkflowDiagnostic {
    pub code: ReadingWorkflowDiagnosticCode,
    pub reading: ReadingId,
    pub entity: Option<QualifiedEntityId>,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadingCommandAllocations {
    pub hypotheses: BTreeMap<QualifiedEntityId, HypothesisId>,
    pub hypothesis_sets: BTreeMap<String, HypothesisSetId>,
}

pub struct ReadingWorkflowRequest<'a> {
    pub readings: &'a [ReadingFile],
    pub local_materials: &'a [LocalReadingMaterial],
    pub existing: &'a BTreeSet<QualifiedEntityId>,
    pub unknown_sections: UnknownSectionPolicy,
    pub base_revision: u64,
    pub allocations: &'a ReadingCommandAllocations,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReadingWorkflowPlan {
    pub resolutions: BTreeMap<ReadingId, ReadingSourceResolution>,
    pub semantic_diffs: Vec<SemanticReadingDiff>,
    pub merge: ReadingMergePlan,
    pub results: Vec<ReadingResultRow>,
    /// Exactly one aggregate command effect. The workflow cannot apply it.
    pub command: UndoableForeignImport,
    pub diagnostics: Vec<ReadingWorkflowDiagnostic>,
}

/// Compose resolution, graph-only fallback, semantic diff, qualified merge,
/// query-style navigation, source-bound audition, and command lowering into a
/// single inspectable plan and one atomic undo unit.
pub fn plan_reading_workflow(
    request: ReadingWorkflowRequest<'_>,
) -> Result<ReadingWorkflowPlan, ReadingWorkflowRefusal> {
    if request.readings.is_empty() {
        return Err(ReadingWorkflowRefusal::NoReadings);
    }
    let mut resolutions = BTreeMap::new();
    let mut plans = Vec::with_capacity(request.readings.len());
    let mut diagnostics = Vec::new();
    for reading in request.readings {
        let resolution = resolve_reading_source(reading, request.local_materials)?;
        if let ReadingSourceResolution::Missing(missing) = &resolution {
            diagnostics.push(ReadingWorkflowDiagnostic {
                code: ReadingWorkflowDiagnosticCode::SourceMissing,
                reading: reading.reading_id,
                entity: None,
                message: format!(
                    "source unavailable: expected {} Hz, {} channels, {} frames and one of {} strong fingerprints; graph import remains available",
                    missing.sample_rate,
                    missing.channels,
                    missing.frame_count,
                    missing.expected_fingerprints.len()
                ),
            });
        }
        for rejected in resolution.rejected() {
            diagnostics.push(ReadingWorkflowDiagnostic {
                code: ReadingWorkflowDiagnosticCode::LocalMaterialRejected,
                reading: reading.reading_id,
                entity: None,
                message: format!(
                    "local material '{}' ({}) refused: {}",
                    rejected.label,
                    rejected.id.0,
                    reading_source_refusal_message(&rejected.reason)
                ),
            });
        }
        let local = resolution.primary().map(|material| &material.source);
        let plan = plan_reading_import(
            reading,
            local,
            request.existing,
            ReadingImportOptions {
                unknown_sections: request.unknown_sections,
                require_entity_section: true,
            },
        )
        .map_err(ReadingWorkflowRefusal::Import)?;
        for name in &plan.opaque_sections {
            diagnostics.push(ReadingWorkflowDiagnostic {
                code: ReadingWorkflowDiagnosticCode::OpaqueSectionPreserved,
                reading: reading.reading_id,
                entity: None,
                message: format!("section '{name}' is preserved verbatim but not interpreted"),
            });
        }
        if resolutions.insert(reading.reading_id, resolution).is_some() {
            return Err(ReadingWorkflowRefusal::DuplicateReadingId(
                reading.reading_id,
            ));
        }
        plans.push(plan);
    }

    let mut semantic_diffs = Vec::new();
    if let Some(first) = request.readings.first() {
        for reading in &request.readings[1..] {
            semantic_diffs.push(semantic_diff_readings(first, reading)?);
        }
    }
    let merge = merge_as_coexisting_hypotheses(&plans).map_err(ReadingWorkflowRefusal::Merge)?;
    let command = lower_foreign_hypothesis_import(
        &merge,
        request.base_revision,
        &request.allocations.hypotheses,
        &request.allocations.hypothesis_sets,
    )
    .map_err(ReadingWorkflowRefusal::Command)?;

    let plan_by_reading = plans
        .iter()
        .map(|plan| (plan.reading_id, plan))
        .collect::<BTreeMap<_, _>>();
    let mut results = Vec::new();
    for entity in &merge.entities {
        let plan = plan_by_reading[&entity.id.reading];
        let reveal = plan
            .reveal_target(&entity.id)
            .map_err(|error| ReadingWorkflowRefusal::Navigation(format!("{error:?}")))?;
        let hit = QueryHitDto {
            fact: reveal.entity.clone(),
            extent: reveal.extent.clone(),
            derivation: QueryDerivationDto {
                rule: format!(
                    "reading:{}:revision:{}",
                    entity.id.reading, plan.reading_revision
                ),
                premises: Vec::new(),
            },
        };
        let audition = match (
            resolutions[&entity.id.reading].primary(),
            plan.audition_target(&entity.id),
        ) {
            (Some(material), Ok(target)) => Some(ResolvedAuditionRequest {
                material: material.id.clone(),
                target,
            }),
            (_, Err(super::ReadingActionRefusal::MissingExtent(_))) => {
                diagnostics.push(ReadingWorkflowDiagnostic {
                    code: ReadingWorkflowDiagnosticCode::AuditionExtentMissing,
                    reading: entity.id.reading,
                    entity: Some(entity.id.clone()),
                    message: "entity is revealable but has no audition extent".into(),
                });
                None
            }
            (_, Err(super::ReadingActionRefusal::SourceUnavailable)) | (None, Ok(_)) => None,
            (_, Err(error)) => {
                return Err(ReadingWorkflowRefusal::Navigation(format!("{error:?}")))
            }
        };
        results.push(ReadingResultRow {
            label: entity.label.clone(),
            disposition: entity.disposition,
            hit,
            reveal,
            audition,
        });
    }
    results.sort_by(|left, right| left.hit.fact.cmp(&right.hit.fact));
    diagnostics.sort_by(|left, right| {
        (left.reading, left.entity.clone(), left.message.clone()).cmp(&(
            right.reading,
            right.entity.clone(),
            right.message.clone(),
        ))
    });
    Ok(ReadingWorkflowPlan {
        resolutions,
        semantic_diffs,
        merge,
        results,
        command,
        diagnostics,
    })
}

pub fn reading_source_refusal_message(refusal: &ReadingVerificationRefusal) -> String {
    match refusal {
        ReadingVerificationRefusal::WeakLocalFingerprint => {
            "fingerprint is not collision-resistant".into()
        }
        ReadingVerificationRefusal::FingerprintMismatch { .. } => {
            "content fingerprint differs".into()
        }
        ReadingVerificationRefusal::SampleRate { expected, actual } => {
            format!("sample rate differs (expected {expected}, found {actual})")
        }
        ReadingVerificationRefusal::Channels { expected, actual } => {
            format!("channel count differs (expected {expected}, found {actual})")
        }
        ReadingVerificationRefusal::FrameCount {
            expected,
            actual,
            delta,
        } => format!("frame count differs (expected {expected}, found {actual}, delta {delta:+})"),
        ReadingVerificationRefusal::SourceNotMatched => "source was not matched".into(),
        ReadingVerificationRefusal::NoReplicationChecks => {
            "no replication checks were supplied".into()
        }
        ReadingVerificationRefusal::ReplicationMismatch(entity) => {
            format!("replication check failed for {entity:?}")
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ReadingWorkflowRefusal {
    NoReadings,
    InvalidReading(ReadingError),
    InvalidLocalMaterial {
        id: LocalMaterialId,
        detail: String,
    },
    DuplicateLocalMaterial(LocalMaterialId),
    SourceCandidatesRefused(MissingSourceDiagnostic),
    UnexpectedVerificationTier(VerificationTier),
    ReservedSection(String),
    DuplicateExportEntity(QualifiedEntityId),
    BlankExportLabel(QualifiedEntityId),
    BlankHypothesisGroup(QualifiedEntityId),
    InvalidExportHypothesis(QualifiedEntityId),
    InvalidExportGeometry {
        entity: QualifiedEntityId,
        detail: String,
    },
    ExtentOutsideSource {
        entity: QualifiedEntityId,
        start_frame: i64,
        end_frame: i64,
        source_frames: u64,
    },
    ForeignReadingReference {
        entity: QualifiedEntityId,
        reference: QualifiedEntityId,
    },
    Codec(ReadingCodecError),
    Import(ReadingImportRefusal),
    Merge(ReadingMergeRefusal),
    Command(ForeignImportRefusal),
    DuplicateReadingId(ReadingId),
    Navigation(String),
    Workbench(WorkbenchError),
}

impl fmt::Display for ReadingWorkflowRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoReadings => formatter.write_str("a reading workflow needs at least one reading"),
            Self::InvalidReading(error) => write!(formatter, "reading envelope refused: {error}"),
            Self::InvalidLocalMaterial { id, detail } => {
                write!(formatter, "local material {:?} is invalid: {detail}", id.0)
            }
            Self::DuplicateLocalMaterial(id) => {
                write!(formatter, "local material {:?} was supplied more than once", id.0)
            }
            Self::SourceCandidatesRefused(diagnostic) => {
                write!(
                    formatter,
                    "{} local source candidate(s) were refused for reading {}",
                    diagnostic.rejected.len(),
                    diagnostic.reading_id
                )?;
                for rejected in &diagnostic.rejected {
                    write!(
                        formatter,
                        "; {:?}: {}",
                        rejected.id.0,
                        reading_source_refusal_message(&rejected.reason)
                    )?;
                }
                Ok(())
            }
            Self::UnexpectedVerificationTier(tier) => {
                write!(formatter, "source resolver produced unexpected tier {tier:?}")
            }
            Self::ReservedSection(name) => {
                write!(formatter, "export section {name:?} is owned by the reading workflow")
            }
            Self::DuplicateExportEntity(entity) => {
                write!(formatter, "export contains duplicate entity {entity:?}")
            }
            Self::BlankExportLabel(entity) => {
                write!(formatter, "export entity {entity:?} has a blank label")
            }
            Self::BlankHypothesisGroup(entity) => {
                write!(formatter, "export hypothesis {entity:?} has a blank question/group")
            }
            Self::InvalidExportHypothesis(entity) => {
                write!(formatter, "export hypothesis {entity:?} has invalid semantics")
            }
            Self::InvalidExportGeometry { entity, detail } => {
                write!(formatter, "export entity {entity:?} has invalid geometry: {detail}")
            }
            Self::ExtentOutsideSource {
                entity,
                start_frame,
                end_frame,
                source_frames,
            } => write!(
                formatter,
                "export entity {entity:?} extent [{start_frame}, {end_frame}) is outside the {source_frames}-frame source"
            ),
            Self::ForeignReadingReference { entity, reference } => write!(
                formatter,
                "export entity {entity:?} refers to different reading entity {reference:?}"
            ),
            Self::Codec(error) => write!(formatter, "reading encoding refused: {error}"),
            Self::Import(error) => write!(formatter, "reading import refused: {error:?}"),
            Self::Merge(error) => write!(formatter, "reading merge refused: {error:?}"),
            Self::Command(error) => write!(formatter, "reading command plan refused: {error:?}"),
            Self::DuplicateReadingId(reading) => write!(
                formatter,
                "workflow contains multiple revisions of reading {reading}; diff them before choosing one revision to import"
            ),
            Self::Navigation(detail) => write!(formatter, "reading navigation refused: {detail}"),
            Self::Workbench(error) => write!(formatter, "reading workbench refused: {error}"),
        }
    }
}

impl std::error::Error for ReadingWorkflowRefusal {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::IdClaim;
    use crate::daw_project::DawProject;
    use crate::interpretation_navigation::RegionDto;
    use crate::reading::{
        PortableDigestAlgorithm, ProducerDto, ReadingSource, READING_FORMAT_VERSION,
    };

    fn digest(byte: u8) -> PortableDigest {
        PortableDigest {
            algorithm: PortableDigestAlgorithm::Sha256,
            bytes: [byte; 32],
        }
    }

    fn author(name: &str) -> ProvenanceDto {
        ProvenanceDto {
            producer: ProducerDto::Human {
                name: Some(name.into()),
            },
            created_unix_ms: None,
            source_revision: None,
            note: None,
        }
    }

    fn source() -> ReadingSource {
        ReadingSource {
            fingerprints: vec![digest(9)],
            sample_rate: 48_000,
            channels: 1,
            frame_count: 100,
            declared_title: Some("same recording".into()),
            extensions: BTreeMap::new(),
        }
    }

    fn extent() -> AspectGeometryDto {
        AspectGeometryDto {
            regions: vec![RegionDto {
                start_frame: 10,
                end_frame: 30,
                min_hz_bits: 100.0_f32.to_bits(),
                max_hz_bits: 8_000.0_f32.to_bits(),
                channels: 1,
            }],
            objects: vec![EntityRefDto::Project {
                kind: "air-object".into(),
                local_id: 4,
            }],
            signal: SignalLayerDto::Source,
        }
    }

    fn export(id_byte: u8, label: &str) -> ExportedReading {
        export_reading(ReadingExportRequest {
            reading_id: ReadingId::new([id_byte; 16]).unwrap(),
            revision: 1,
            parents: Vec::new(),
            author: author(label),
            source: source(),
            entities: vec![PortableEntityRecord {
                kind: "hypothesis".into(),
                local_id: 1,
                label: label.into(),
                role: PortableEntityRole::Hypothesis,
                hypothesis: Some(PortableHypothesisSemantics {
                    support: 0.5,
                    description: Some(label.into()),
                }),
                hypothesis_group: Some("what-makes-the-pulse".into()),
                extent: Some(extent()),
                extensions: BTreeMap::new(),
            }],
            additional_sections: vec![ReadingSection {
                name: "future-lexicon".into(),
                schema_major: 1,
                schema_minor: 0,
                payload: serde_json::json!({"term": "woolly"}),
                extensions: BTreeMap::new(),
            }],
            attachments: Vec::new(),
            extensions: BTreeMap::new(),
        })
        .unwrap()
    }

    #[test]
    fn export_qualifies_references_and_returns_a_verifiable_manifest() {
        let exported = export(1, "syncopated bass");
        assert_eq!(exported.reading.version, READING_FORMAT_VERSION);
        assert!(exported.encoded.manifest_digest.is_strong());
        let decoded = exported.verify().unwrap();
        let plan = plan_reading_import(
            &decoded,
            None,
            &BTreeSet::new(),
            ReadingImportOptions::default(),
        )
        .unwrap();
        assert!(matches!(
            &plan.entities[0].extent.as_ref().unwrap().objects[0],
            EntityRefDto::Reading(id) if id.reading == decoded.reading_id
        ));
    }

    #[test]
    fn exact_resolution_imports_two_level_alternatives_as_one_undoable_plan() {
        let left = export(2, "kick implication").reading;
        let right = export(3, "bass transient").reading;
        let left_id = QualifiedEntityId::new(left.reading_id, "hypothesis", 1).unwrap();
        let right_id = QualifiedEntityId::new(right.reading_id, "hypothesis", 1).unwrap();
        let materials = vec![
            LocalReadingMaterial {
                id: LocalMaterialId("wrong-master".into()),
                label: "lossy copy".into(),
                source: LocalSourceDescriptor {
                    digest: digest(8),
                    sample_rate: 48_000,
                    channels: 1,
                    frame_count: 100,
                },
            },
            LocalReadingMaterial {
                id: LocalMaterialId("asset:44".into()),
                label: "local WAV".into(),
                source: LocalSourceDescriptor {
                    digest: digest(9),
                    sample_rate: 48_000,
                    channels: 1,
                    frame_count: 100,
                },
            },
        ];
        let allocations = ReadingCommandAllocations {
            hypotheses: BTreeMap::from([
                (left_id.clone(), HypothesisId::new(101)),
                (right_id.clone(), HypothesisId::new(102)),
            ]),
            hypothesis_sets: BTreeMap::from([(
                "what-makes-the-pulse".into(),
                HypothesisSetId::new(201),
            )]),
        };
        let plan = plan_reading_workflow(ReadingWorkflowRequest {
            readings: &[left, right],
            local_materials: &materials,
            existing: &BTreeSet::new(),
            unknown_sections: UnknownSectionPolicy::PreserveOpaque,
            base_revision: 0,
            allocations: &allocations,
        })
        .unwrap();

        assert_ne!(left_id, right_id);
        assert_eq!(plan.command.mappings.len(), 2);
        assert_eq!(plan.command.envelope.commands.len(), 3);
        assert_eq!(plan.command.envelope.coalesce, None);
        assert_eq!(
            plan.command
                .envelope
                .id_claims
                .iter()
                .filter(|claim| matches!(claim, IdClaim::Foreign { .. }))
                .count(),
            2
        );
        assert_eq!(plan.results.len(), 2);
        assert!(plan.results.iter().all(|row| {
            matches!(&row.reveal.entity, EntityRefDto::Reading(_))
                && row
                    .audition
                    .as_ref()
                    .is_some_and(|request| request.material.0 == "asset:44")
        }));
        assert_eq!(plan.semantic_diffs[0].hypotheses.len(), 1);
        assert_eq!(
            plan.semantic_diffs[0].hypotheses[0].kind,
            SemanticChangeKind::ChangedAlternatives
        );
        assert!(plan.semantic_diffs[0].hypotheses[0].extents_overlap);
        assert!(plan.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == ReadingWorkflowDiagnosticCode::LocalMaterialRejected
                && diagnostic.message.contains("content fingerprint differs")
        }));

        let mut project = DawProject::new("reading exchange", 48_000, 120.0).unwrap();
        let applied = plan.command.envelope.apply(&mut project).unwrap();
        assert_eq!(project.state().domains.air.hypotheses.len(), 2);
        assert_eq!(project.state().domains.air.hypothesis_sets.len(), 1);
        applied.inverse.apply(&mut project).unwrap();
        assert!(project.state().domains.air.hypotheses.is_empty());
        assert!(project.state().domains.air.hypothesis_sets.is_empty());
    }

    #[test]
    fn missing_source_keeps_reveal_and_atomic_import_but_omits_audition() {
        let reading = export(4, "unheard interpretation").reading;
        let id = QualifiedEntityId::new(reading.reading_id, "hypothesis", 1).unwrap();
        let allocations = ReadingCommandAllocations {
            hypotheses: BTreeMap::from([(id, HypothesisId::new(111))]),
            hypothesis_sets: BTreeMap::new(),
        };
        let plan = plan_reading_workflow(ReadingWorkflowRequest {
            readings: &[reading],
            local_materials: &[],
            existing: &BTreeSet::new(),
            unknown_sections: UnknownSectionPolicy::PreserveOpaque,
            base_revision: 7,
            allocations: &allocations,
        })
        .unwrap();
        assert_eq!(plan.command.envelope.base_revision, 7);
        assert!(matches!(
            plan.resolutions.values().next().unwrap(),
            ReadingSourceResolution::Missing(_)
        ));
        assert!(plan.results[0].audition.is_none());
        assert!(matches!(
            &plan.results[0].reveal.entity,
            EntityRefDto::Reading(_)
        ));
        assert!(plan.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == ReadingWorkflowDiagnosticCode::SourceMissing
                && diagnostic
                    .message
                    .contains("graph import remains available")
        }));
    }

    #[test]
    fn supplied_mismatched_copy_is_refused_instead_of_downgraded_to_missing() {
        let reading = export(5, "must match").reading;
        let wrong = LocalReadingMaterial {
            id: LocalMaterialId("selected-lossy-copy".into()),
            label: "selected lossy copy".into(),
            source: LocalSourceDescriptor {
                digest: digest(7),
                sample_rate: 48_000,
                channels: 1,
                frame_count: 99,
            },
        };
        let refusal = resolve_reading_source(&reading, &[wrong]).unwrap_err();
        assert!(matches!(
            &refusal,
            ReadingWorkflowRefusal::SourceCandidatesRefused(diagnostic)
                if diagnostic.rejected.len() == 1
                    && matches!(
                        &diagnostic.rejected[0].reason,
                        ReadingVerificationRefusal::FrameCount { delta: -1, .. }
                    )
        ));
        assert!(refusal.to_string().contains("frame count differs"));
    }
}
