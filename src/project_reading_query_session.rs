//! Production project/session bridge for reading and AIR-query work.
//!
//! A bridge captures one coherent project publication plus the artifact,
//! interpretation, selection, and resolver projections owned by the host. It
//! implements the read-only query traits over that frozen state. The only
//! mutation path is an ordinary [`CommandEnvelope`] submitted to
//! [`ProjectSession::execute_envelope`], retaining validation, journal, undo,
//! redo, and publication semantics.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use crate::air_query::workbench::protocol::{
    HeadlessDispatch, HeadlessOperation, HeadlessRequest, HeadlessSessionAdapter, ReadingInputDto,
};
use crate::air_query::workbench::{
    execute_query_page, lower_foreign_hypothesis_import, merge_as_coexisting_hypotheses,
    plan_reading_import, residual_guide, AuditionTarget, QueryDocument, QueryExecutionProvenance,
    QueryPageRequest, ReadingImportOptions, ReadingImportPlan, ReadingMergePlan, ResidualGuide,
    RevealTarget, UndoableForeignImport, UnknownSectionPolicy, WorkbenchError,
};
use crate::air_query::{AirFacts, FactKind, FactRef, QueryCancellation};
use crate::artifact_catalog::{sha256_content, ArtifactCatalog, ArtifactDescriptor, ArtifactId};
use crate::aspect::{
    AnalysisRef, Aspect, AspectResolver, BandSpan, ChannelMask, ConcreteAspect, ConcreteRegion,
    ExplanationRef, FrameSpan, SignalLayer,
};
use crate::command::CommandEnvelope;
use crate::coverage::CoverageField;
use crate::daw_project::ProjectRevisions;
use crate::interpretation::InterpretationStore;
use crate::ontology::{self, ChannelSelection, HypothesisClaim, ParameterOwner};
use crate::project_selection::{AirSelection, ProjectSelectionState};
use crate::project_session::{ProjectEditReceipt, ProjectSession, ProjectSessionError};
use crate::reading::{PortableDigest, QualifiedEntityId, ReadingId};
use crate::reconstruction::ReconstructionProposalId;

const SNAPSHOT_DIGEST_DOMAIN: &[u8] = b"audec:project-reading-query-snapshot:v1";

/// Resolver data whose durable identity lives outside the aggregate AIR.
/// Hosts publish exact extents from their current artifact/reconstruction
/// services; absence stays an unresolvable reference rather than a guess.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectQueryResolverInputs {
    pub analysis_families: BTreeMap<(AnalysisRef, usize), Vec<FrameSpan>>,
    pub proposal_extents: BTreeMap<ReconstructionProposalId, ConcreteAspect>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectQuerySnapshotStatus {
    pub document_generation: u64,
    pub revisions: ProjectRevisions,
    pub selection_revision: u64,
    pub fact_base_digest: PortableDigest,
    pub fact_count: usize,
    pub artifact_count: usize,
    pub explanation_count: usize,
    pub comparison_count: usize,
    pub selected_facts: Vec<FactRef>,
}

#[derive(Clone, Debug)]
pub struct ProjectReadingImportPlan {
    pub merge: ReadingMergePlan,
    pub lowered: UndoableForeignImport,
}

#[derive(Clone, Debug)]
pub struct ProjectReadingImportReceipt {
    pub mappings: Vec<crate::air_query::workbench::ForeignHypothesisMapping>,
    pub edit: ProjectEditReceipt,
    pub command_label: String,
}

#[derive(Clone, Debug)]
pub struct ProjectResidualIntents {
    pub guide: ResidualGuide,
    pub reveal: Option<RevealTarget>,
    pub auditions: Vec<AuditionTarget>,
}

#[derive(Clone, Debug)]
pub enum ProjectReadingQueryUpdate {
    Captured(ProjectQuerySnapshotStatus),
    QueryPage(QueryDocument),
    Observation(HeadlessDispatch),
    ImportPlanned {
        readings: usize,
        qualified_entities: usize,
        hypothesis_groups: usize,
        command_count: usize,
    },
    ImportApplied(ProjectReadingImportReceipt),
    ImportUndone(ProjectEditReceipt),
    Refused {
        operation: &'static str,
        message: String,
    },
}

pub type ProjectReadingQueryPublisher =
    Arc<dyn Fn(ProjectReadingQueryUpdate) + Send + Sync + 'static>;

/// Frozen production fact base and resolver. It contains no project mutation
/// handle and can therefore be moved to a query worker safely.
#[derive(Clone, Debug)]
pub struct ProjectReadingQuerySnapshot {
    document_generation: u64,
    revisions: ProjectRevisions,
    air: ontology::AuditoryIr,
    artifacts: BTreeMap<ArtifactId, ArtifactDescriptor>,
    interpretations: InterpretationStore,
    selection: ProjectSelectionState,
    resolver_inputs: ProjectQueryResolverInputs,
    universe: ConcreteAspect,
    digest: PortableDigest,
}

impl ProjectReadingQuerySnapshot {
    pub fn capture(
        session: &ProjectSession,
        artifacts: &ArtifactCatalog,
        interpretations: &InterpretationStore,
        resolver_inputs: ProjectQueryResolverInputs,
    ) -> Result<Self, ProjectReadingQueryError> {
        let project = session.project_snapshot()?;
        let air = project.project.state().domains.air.clone();
        let artifacts = artifacts
            .descriptors()
            .cloned()
            .map(|descriptor| (descriptor.id, descriptor))
            .collect::<BTreeMap<_, _>>();
        let selection = session.selection().clone();
        let universe = build_universe(
            &air,
            artifacts.values(),
            interpretations,
            session.snapshot().analysis.as_deref(),
        )?;
        let digest = snapshot_digest(
            session.document_generation(),
            project.revisions(),
            &air,
            &artifacts,
            interpretations,
            &selection,
            &resolver_inputs,
        )?;
        Ok(Self {
            document_generation: session.document_generation(),
            revisions: project.revisions(),
            air,
            artifacts,
            interpretations: interpretations.clone(),
            selection,
            resolver_inputs,
            universe,
            digest,
        })
    }

    pub fn status(&self) -> ProjectQuerySnapshotStatus {
        ProjectQuerySnapshotStatus {
            document_generation: self.document_generation,
            revisions: self.revisions,
            selection_revision: self.selection.revision,
            fact_base_digest: self.digest,
            fact_count: self.fact_count(),
            artifact_count: self.artifacts.len(),
            explanation_count: self.interpretations.explanations().len(),
            comparison_count: self.interpretations.comparisons().len(),
            selected_facts: self.selected_facts(),
        }
    }

    pub fn provenance(&self) -> QueryExecutionProvenance {
        QueryExecutionProvenance {
            // Revision zero is an honest pristine aggregate identity. Query
            // execution currently refuses it rather than minting revision 1.
            fact_base_revision: self.revisions.aggregate,
            fact_base_digest: self.digest,
            source_revision: Some(format!(
                "project-generation:{}:aggregate:{}:air:{}:selection:{}",
                self.document_generation,
                self.revisions.aggregate,
                self.revisions.air,
                self.selection.revision
            )),
            executed_unix_ms: None,
        }
    }

    pub fn execute_page<'a>(
        &self,
        document: &'a mut QueryDocument,
        request: QueryPageRequest,
        cancellation: &dyn QueryCancellation,
    ) -> Result<&'a crate::air_query::workbench::QueryResultSnapshot, ProjectReadingQueryError>
    {
        execute_query_page(
            document,
            self,
            self,
            self.provenance(),
            request,
            cancellation,
        )
        .map_err(Into::into)
    }

    /// Execute the same typed request used by the JSONL and GPUI adapters.
    pub fn dispatch(
        &self,
        request: HeadlessRequest,
        cancellation: &dyn QueryCancellation,
    ) -> Result<HeadlessDispatch, ProjectReadingQueryError> {
        if let HeadlessOperation::QueryPage { provenance, .. } = &request.operation {
            let captured = self.provenance();
            if provenance != &captured {
                return Err(ProjectReadingQueryError::QueryProvenanceMismatch {
                    captured,
                    requested: provenance.clone(),
                });
            }
        }
        HeadlessSessionAdapter::new(self, self)
            .dispatch(request, cancellation)
            .map_err(|error| ProjectReadingQueryError::Protocol(error.to_string()))
    }

    pub fn selected_extent(&self) -> Result<Option<ConcreteAspect>, ProjectReadingQueryError> {
        let selection = &self.selection.selection;
        let mut terms = Vec::new();
        if let Some(aspect) = &selection.aspect {
            terms.push(aspect.clone());
        }
        if let Some(time) = selection.time {
            terms.push(Aspect::Time(time));
        }
        for selected in &selection.air {
            if let AirSelection::Object(object) = selected {
                terms.push(Aspect::Object(*object));
            }
        }
        if terms.is_empty() {
            return Ok(None);
        }
        let aspect = if terms.len() == 1 {
            terms.pop().expect("one selection term")
        } else {
            Aspect::Union(terms)
        };
        crate::aspect::evaluate(&aspect, self)
            .map(Some)
            .map_err(|error| ProjectReadingQueryError::Aspect(error.to_string()))
    }

    pub fn residual_intents(
        &self,
        document_id: crate::air_query::workbench::QueryDocumentId,
        title: impl Into<String>,
        field: &CoverageField,
        comparison_id: u64,
        proposal_id: u64,
        limit: usize,
    ) -> Result<ProjectResidualIntents, ProjectReadingQueryError> {
        let guide = residual_guide(document_id, title, field, comparison_id, proposal_id, limit)?;
        let auditions = guide.auditions.clone();
        let reveal = auditions.first().map(|target| RevealTarget {
            entity: target.entity.clone(),
            extent: Some(target.extent.clone()),
        });
        Ok(ProjectResidualIntents {
            guide,
            reveal,
            auditions,
        })
    }

    pub fn existing_foreign_entities(&self) -> BTreeSet<QualifiedEntityId> {
        self.air
            .hypotheses
            .values()
            .filter_map(|hypothesis| {
                hypothesis
                    .provenance
                    .note
                    .as_deref()
                    .and_then(parse_foreign_note)
            })
            .collect()
    }

    pub fn plan_reading_import(
        &self,
        readings: &[ReadingInputDto],
        unknown_sections: UnknownSectionPolicy,
    ) -> Result<ProjectReadingImportPlan, ProjectReadingQueryError> {
        if readings.is_empty() {
            return Err(ProjectReadingQueryError::InvalidImport(
                "at least one reading is required".into(),
            ));
        }
        let existing = self.existing_foreign_entities();
        let plans = readings
            .iter()
            .map(|input| {
                let local = input.local_source.map(Into::into);
                plan_reading_import(
                    &input.reading,
                    local.as_ref(),
                    &existing,
                    ReadingImportOptions {
                        unknown_sections,
                        require_entity_section: true,
                    },
                )
                .map_err(|error| ProjectReadingQueryError::InvalidImport(format!("{error:?}")))
            })
            .collect::<Result<Vec<ReadingImportPlan>, _>>()?;
        let merge = merge_as_coexisting_hypotheses(&plans)
            .map_err(|error| ProjectReadingQueryError::InvalidImport(format!("{error:?}")))?;
        let pending = merge
            .entities
            .iter()
            .filter(|entity| {
                entity.disposition != crate::air_query::workbench::ImportDisposition::AlreadyPresent
            })
            .map(|entity| entity.id.clone())
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return Err(ProjectReadingQueryError::AlreadyImported {
                qualified_entities: merge.entities.len(),
            });
        }
        let hypothesis_allocations = allocate_hypotheses(&self.air, &pending)?;
        let set_allocations = allocate_hypothesis_sets(&self.air, &merge, &hypothesis_allocations)?;
        let lowered = lower_foreign_hypothesis_import(
            &merge,
            self.revisions.aggregate,
            &hypothesis_allocations,
            &set_allocations,
        )
        .map_err(|error| ProjectReadingQueryError::InvalidImport(format!("{error:?}")))?;
        Ok(ProjectReadingImportPlan { merge, lowered })
    }

    fn fact_count(&self) -> usize {
        self.air.objects.len()
            + self.air.sources.len()
            + self.air.parameters.len()
            + self.air.hypotheses.len()
    }

    fn selected_facts(&self) -> Vec<FactRef> {
        let mut selected = self
            .selection
            .selection
            .air
            .iter()
            .filter_map(|value| match value {
                AirSelection::Object(id) if self.air.objects.contains_key(id) => {
                    Some(FactRef::Object(*id))
                }
                AirSelection::Hypothesis(id) if self.air.hypotheses.contains_key(id) => {
                    Some(FactRef::Hypothesis(*id))
                }
                AirSelection::Evidence(_)
                | AirSelection::Object(_)
                | AirSelection::Hypothesis(_) => None,
            })
            .collect::<Vec<_>>();
        selected.sort();
        selected.dedup();
        selected
    }
}

impl AirFacts for ProjectReadingQuerySnapshot {
    fn facts(&self, kind: FactKind) -> Vec<FactRef> {
        match kind {
            FactKind::Object => self
                .air
                .objects
                .keys()
                .copied()
                .map(FactRef::Object)
                .collect(),
            FactKind::Source => self
                .air
                .sources
                .keys()
                .copied()
                .map(FactRef::Source)
                .collect(),
            FactKind::Parameter => self
                .air
                .parameters
                .keys()
                .copied()
                .map(FactRef::Parameter)
                .collect(),
            FactKind::Hypothesis => self
                .air
                .hypotheses
                .keys()
                .copied()
                .map(FactRef::Hypothesis)
                .collect(),
        }
    }

    fn evidence_of(&self, fact: FactRef) -> Vec<FactRef> {
        // AIR evidence records are not query FactRefs. Return only typed AIR
        // facts which the durable graph explicitly cites as support/context.
        let mut evidence = match fact {
            FactRef::Object(object) => {
                let mut values = self.object_sources(object);
                values.extend(
                    self.air
                        .hypotheses
                        .values()
                        .filter(|hypothesis| {
                            hypothesis_objects(&self.air, hypothesis).contains(&object)
                        })
                        .map(|hypothesis| FactRef::Hypothesis(hypothesis.id)),
                );
                values
            }
            FactRef::Source(source) => self
                .air
                .objects
                .keys()
                .copied()
                .filter(|object| {
                    self.object_sources(*object)
                        .contains(&FactRef::Source(source))
                })
                .map(FactRef::Object)
                .collect(),
            FactRef::Parameter(parameter) => self.parameter_owner(parameter).into_iter().collect(),
            FactRef::Hypothesis(hypothesis) => self
                .air
                .hypotheses
                .get(&hypothesis)
                .map(|hypothesis| hypothesis_objects(&self.air, hypothesis))
                .unwrap_or_default()
                .into_iter()
                .map(FactRef::Object)
                .collect(),
        };
        evidence.sort();
        evidence.dedup();
        evidence
    }

    fn related(&self, fact: FactRef) -> Vec<FactRef> {
        let mut related = self.evidence_of(fact);
        if let FactRef::Object(object) = fact {
            for relation in self.air.relations.values() {
                if relation.from == object {
                    related.push(FactRef::Object(relation.to));
                } else if relation.to == object {
                    related.push(FactRef::Object(relation.from));
                }
            }
            if let Some(value) = self.air.objects.get(&object) {
                related.extend(value.kind.members().iter().copied().map(FactRef::Object));
            }
        }
        related.sort();
        related.dedup();
        related
    }

    fn extent(&self, fact: FactRef) -> Option<ConcreteAspect> {
        match fact {
            FactRef::Object(object) => self.object_extent(object),
            FactRef::Source(source) => self.source_extent(source),
            FactRef::Parameter(parameter) => {
                self.parameter_owner(parameter)
                    .and_then(|owner| match owner {
                        FactRef::Object(object) => self.object_extent(object),
                        _ => None,
                    })
            }
            FactRef::Hypothesis(hypothesis) => self.hypothesis_extent(hypothesis),
        }
    }
}

impl AspectResolver for ProjectReadingQuerySnapshot {
    fn universe(&self) -> ConcreteAspect {
        self.universe.clone()
    }

    fn family_spans(&self, analysis: &AnalysisRef, id: usize) -> Option<Vec<FrameSpan>> {
        self.resolver_inputs
            .analysis_families
            .get(&(*analysis, id))
            .cloned()
    }

    fn object_extent(&self, object: ontology::ObjectId) -> Option<ConcreteAspect> {
        let object = self.air.objects.get(&object)?;
        let end = object.timeline.end()?;
        let time = FrameSpan::new(object.timeline.start, end)?;
        let channels = object
            .source_anchors
            .iter()
            .filter_map(|anchor| self.air.spans.get(&anchor.span))
            .fold(ChannelMask(0), |mask, span| {
                mask.union(channel_mask_for_selection(
                    &span.channels,
                    self.air
                        .sources
                        .get(&span.source)
                        .map_or(1, |source| source.channels),
                ))
            });
        let universe_region = self.universe.regions.first().copied()?;
        ConcreteAspect::new(
            vec![ConcreteRegion {
                time,
                band: universe_region.band,
                channels: if channels.is_empty() {
                    universe_region.channels
                } else {
                    channels
                },
            }],
            SignalLayer::Source,
        )
        .ok()
        .map(|mut extent| {
            extent.objects.push(object.id);
            extent
        })
    }

    fn explanation_extent(&self, reference: &ExplanationRef) -> Option<ConcreteAspect> {
        match reference {
            ExplanationRef::Proposal(id) => self.resolver_inputs.proposal_extents.get(id).cloned(),
            ExplanationRef::Comparison(raw) => {
                let comparison = self
                    .interpretations
                    .comparison(crate::comparison::ComparisonId(*raw))?;
                let universe = self.universe.regions.first().copied()?;
                ConcreteAspect::new(
                    vec![ConcreteRegion {
                        time: comparison.source.project_span,
                        band: universe.band,
                        channels: comparison.source.channels,
                    }],
                    SignalLayer::Explanation(*reference),
                )
                .ok()
            }
            ExplanationRef::Definition(raw) => {
                let definition = self
                    .interpretations
                    .explanation(crate::explanation::ExplanationId(*raw))?;
                if contains_signal_reference(&definition.extent)
                    || definition
                        .scope
                        .artifacts()
                        .iter()
                        .any(|artifact| !self.artifacts.contains_key(artifact))
                {
                    return None;
                }
                let mut extent = crate::aspect::evaluate(&definition.extent, self).ok()?;
                extent.signal = SignalLayer::Explanation(*reference);
                Some(extent)
            }
        }
    }
}

impl ProjectReadingQuerySnapshot {
    fn source_extent(&self, source: ontology::SourceId) -> Option<ConcreteAspect> {
        let source = self.air.sources.get(&source)?;
        ConcreteAspect::new(
            vec![ConcreteRegion {
                time: FrameSpan::new(0, i64::try_from(source.frame_count).ok()?)?,
                band: BandSpan::new(0.0, source.sample_rate as f32 / 2.0)?,
                channels: channel_mask(source.channels),
            }],
            SignalLayer::Source,
        )
        .ok()
    }

    fn object_sources(&self, object: ontology::ObjectId) -> Vec<FactRef> {
        let mut sources = self
            .air
            .objects
            .get(&object)
            .into_iter()
            .flat_map(|object| &object.source_anchors)
            .filter_map(|anchor| self.air.spans.get(&anchor.span))
            .map(|span| FactRef::Source(span.source))
            .collect::<Vec<_>>();
        sources.sort();
        sources.dedup();
        sources
    }

    fn parameter_owner(&self, parameter: ontology::ParameterId) -> Option<FactRef> {
        match self.air.parameters.get(&parameter)?.owner {
            ParameterOwner::Object(object) => Some(FactRef::Object(object)),
            ParameterOwner::Transform(transform) => self
                .air
                .transforms
                .get(&transform)
                .map(|transform| FactRef::Object(transform.owner)),
        }
    }

    fn hypothesis_extent(&self, hypothesis: ontology::HypothesisId) -> Option<ConcreteAspect> {
        let objects = hypothesis_objects(&self.air, self.air.hypotheses.get(&hypothesis)?);
        let mut regions = objects
            .iter()
            .filter_map(|object| self.object_extent(*object))
            .flat_map(|extent| extent.regions)
            .collect::<Vec<_>>();
        if regions.is_empty() {
            return None;
        }
        regions.sort();
        regions.dedup();
        ConcreteAspect::new(regions, SignalLayer::Source).ok()
    }
}

/// Session-facing facade and the constructor/callback seam used by GPUI.
pub struct ProjectReadingQuerySession {
    snapshot: ProjectReadingQuerySnapshot,
    publisher: ProjectReadingQueryPublisher,
}

impl ProjectReadingQuerySession {
    pub fn new(
        session: &ProjectSession,
        artifacts: &ArtifactCatalog,
        interpretations: &InterpretationStore,
        resolver_inputs: ProjectQueryResolverInputs,
        publisher: ProjectReadingQueryPublisher,
    ) -> Result<Self, ProjectReadingQueryError> {
        let snapshot = ProjectReadingQuerySnapshot::capture(
            session,
            artifacts,
            interpretations,
            resolver_inputs,
        )?;
        publisher(ProjectReadingQueryUpdate::Captured(snapshot.status()));
        Ok(Self {
            snapshot,
            publisher,
        })
    }

    pub fn snapshot(&self) -> &ProjectReadingQuerySnapshot {
        &self.snapshot
    }

    pub fn execute_page(
        &self,
        document: &mut QueryDocument,
        page: QueryPageRequest,
        cancellation: &dyn QueryCancellation,
    ) -> Result<(), ProjectReadingQueryError> {
        match self.snapshot.execute_page(document, page, cancellation) {
            Ok(_) => {
                (self.publisher)(ProjectReadingQueryUpdate::QueryPage(document.clone()));
                Ok(())
            }
            Err(error) => {
                self.publish_refusal("query", &error);
                Err(error)
            }
        }
    }

    pub fn dispatch(
        &self,
        request: HeadlessRequest,
        cancellation: &dyn QueryCancellation,
    ) -> Result<HeadlessDispatch, ProjectReadingQueryError> {
        match self.snapshot.dispatch(request, cancellation) {
            Ok(dispatch) => {
                (self.publisher)(ProjectReadingQueryUpdate::Observation(dispatch.clone()));
                Ok(dispatch)
            }
            Err(error) => {
                self.publish_refusal("observation", &error);
                Err(error)
            }
        }
    }

    pub fn plan_import(
        &self,
        readings: &[ReadingInputDto],
        unknown_sections: UnknownSectionPolicy,
    ) -> Result<ProjectReadingImportPlan, ProjectReadingQueryError> {
        match self
            .snapshot
            .plan_reading_import(readings, unknown_sections)
        {
            Ok(plan) => {
                (self.publisher)(ProjectReadingQueryUpdate::ImportPlanned {
                    readings: plan.merge.readings.len(),
                    qualified_entities: plan.merge.entities.len(),
                    hypothesis_groups: plan.merge.hypothesis_groups.len(),
                    command_count: plan.lowered.envelope.commands.len(),
                });
                Ok(plan)
            }
            Err(error) => {
                self.publish_refusal("reading-import-plan", &error);
                Err(error)
            }
        }
    }

    pub fn apply_import(
        &self,
        session: &mut ProjectSession,
        plan: ProjectReadingImportPlan,
    ) -> Result<ProjectReadingImportReceipt, ProjectReadingQueryError> {
        self.require_current(session)?;
        if plan.lowered.envelope.base_revision != self.snapshot.revisions.aggregate {
            return Err(ProjectReadingQueryError::StaleSnapshot {
                expected: self.snapshot.revisions.aggregate,
                actual: plan.lowered.envelope.base_revision,
            });
        }
        let command_label = plan.lowered.envelope.label.clone();
        let mappings = plan.lowered.mappings;
        let edit = session.execute_envelope(plan.lowered.envelope)?;
        let receipt = ProjectReadingImportReceipt {
            mappings,
            edit,
            command_label,
        };
        (self.publisher)(ProjectReadingQueryUpdate::ImportApplied(receipt.clone()));
        Ok(receipt)
    }

    /// Undo exactly the just-applied import. A later edit or a different undo
    /// head is a refusal, never permission to undo unrelated user work.
    pub fn undo_import(
        &self,
        session: &mut ProjectSession,
        receipt: &ProjectReadingImportReceipt,
    ) -> Result<ProjectEditReceipt, ProjectReadingQueryError> {
        let current = session.project_snapshot()?.revisions().aggregate;
        let expected = receipt.edit.publication.revisions.aggregate;
        let history = session.history_status()?;
        if current != expected || history.undo_label.as_deref() != Some(&receipt.command_label) {
            return Err(ProjectReadingQueryError::UndoHeadMoved {
                expected_revision: expected,
                actual_revision: current,
            });
        }
        let undone = session
            .undo_with_receipt()?
            .ok_or(ProjectReadingQueryError::NothingToUndo)?;
        (self.publisher)(ProjectReadingQueryUpdate::ImportUndone(undone.clone()));
        Ok(undone)
    }

    /// Apply a typed command returned by the pane/headless adapter through the
    /// session's sole aggregate command path.
    pub fn apply_command(
        &self,
        session: &mut ProjectSession,
        envelope: CommandEnvelope,
    ) -> Result<ProjectEditReceipt, ProjectReadingQueryError> {
        self.require_current(session)?;
        session.execute_envelope(envelope).map_err(Into::into)
    }

    fn require_current(&self, session: &ProjectSession) -> Result<(), ProjectReadingQueryError> {
        if session.document_generation() != self.snapshot.document_generation {
            return Err(ProjectReadingQueryError::DocumentReplaced {
                expected: self.snapshot.document_generation,
                actual: session.document_generation(),
            });
        }
        let actual = session.project_snapshot()?.revisions().aggregate;
        if actual != self.snapshot.revisions.aggregate {
            return Err(ProjectReadingQueryError::StaleSnapshot {
                expected: self.snapshot.revisions.aggregate,
                actual,
            });
        }
        Ok(())
    }

    fn publish_refusal(&self, operation: &'static str, error: &ProjectReadingQueryError) {
        (self.publisher)(ProjectReadingQueryUpdate::Refused {
            operation,
            message: error.to_string(),
        });
    }
}

fn build_universe<'a>(
    air: &ontology::AuditoryIr,
    artifacts: impl Iterator<Item = &'a ArtifactDescriptor>,
    interpretations: &InterpretationStore,
    analysis: Option<&crate::analysis::Analysis>,
) -> Result<ConcreteAspect, ProjectReadingQueryError> {
    let mut start = 0_i64;
    let mut end = 1_i64;
    let mut sample_rate = air.sample_rate.max(1);
    let mut channels = 1_u16;
    for source in air.sources.values() {
        sample_rate = sample_rate.max(source.sample_rate);
        channels = channels.max(source.channels);
        end = end.max(i64::try_from(source.frame_count).unwrap_or(i64::MAX));
    }
    for object in air.objects.values() {
        start = start.min(object.timeline.start);
        if let Some(object_end) = object.timeline.end() {
            end = end.max(object_end);
        }
    }
    for artifact in artifacts {
        sample_rate = sample_rate.max(artifact.sample_rate);
        channels = channels.max(artifact.channels);
        start = start.min(artifact.extent.start);
        end = end.max(artifact.extent.end);
    }
    for comparison in interpretations.comparisons().values() {
        start = start.min(comparison.source.project_span.start);
        end = end.max(comparison.source.project_span.end);
        channels = channels.max(comparison.source.channels.0.count_ones() as u16);
    }
    if let Some(analysis) = analysis {
        sample_rate = sample_rate.max(analysis.sample_rate);
        channels = channels.max(u16::try_from(analysis.channels).unwrap_or(u16::MAX));
        let frames = (analysis.duration_seconds.max(0.0) * f64::from(analysis.sample_rate)) as u64;
        end = end.max(i64::try_from(frames).unwrap_or(i64::MAX));
    }
    ConcreteAspect::new(
        vec![ConcreteRegion {
            time: FrameSpan::new(start, end.max(start.saturating_add(1)))
                .ok_or_else(|| ProjectReadingQueryError::Aspect("empty project universe".into()))?,
            band: BandSpan::new(0.0, (sample_rate as f32 / 2.0).max(1.0))
                .ok_or_else(|| ProjectReadingQueryError::Aspect("invalid project band".into()))?,
            channels: channel_mask(channels),
        }],
        SignalLayer::Source,
    )
    .map_err(|error| ProjectReadingQueryError::Aspect(error.to_string()))
}

fn snapshot_digest(
    document_generation: u64,
    revisions: ProjectRevisions,
    air: &ontology::AuditoryIr,
    artifacts: &BTreeMap<ArtifactId, ArtifactDescriptor>,
    interpretations: &InterpretationStore,
    selection: &ProjectSelectionState,
    resolver_inputs: &ProjectQueryResolverInputs,
) -> Result<PortableDigest, ProjectReadingQueryError> {
    let air = serde_json::to_vec(air)
        .map_err(|error| ProjectReadingQueryError::Snapshot(error.to_string()))?;
    let revision = format!("{document_generation}:{revisions:?}");
    let artifacts = format!("{artifacts:?}");
    let interpretations = format!("{interpretations:?}");
    let selection = format!("{selection:?}");
    let resolver = format!("{resolver_inputs:?}");
    Ok(sha256_content(
        SNAPSHOT_DIGEST_DOMAIN,
        &[
            revision.as_bytes(),
            &air,
            artifacts.as_bytes(),
            interpretations.as_bytes(),
            selection.as_bytes(),
            resolver.as_bytes(),
        ],
    )
    .into())
}

fn allocate_hypotheses(
    air: &ontology::AuditoryIr,
    pending: &[QualifiedEntityId],
) -> Result<BTreeMap<QualifiedEntityId, ontology::HypothesisId>, ProjectReadingQueryError> {
    let mut next = air
        .hypotheses
        .keys()
        .map(|id| id.get())
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(ProjectReadingQueryError::IdentityExhausted)?;
    let mut result = BTreeMap::new();
    for foreign in pending {
        result.insert(foreign.clone(), ontology::HypothesisId::new(next));
        next = next
            .checked_add(1)
            .ok_or(ProjectReadingQueryError::IdentityExhausted)?;
    }
    Ok(result)
}

fn allocate_hypothesis_sets(
    air: &ontology::AuditoryIr,
    merge: &ReadingMergePlan,
    hypotheses: &BTreeMap<QualifiedEntityId, ontology::HypothesisId>,
) -> Result<BTreeMap<String, ontology::HypothesisSetId>, ProjectReadingQueryError> {
    let mut next = air
        .hypothesis_sets
        .keys()
        .map(|id| id.get())
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(ProjectReadingQueryError::IdentityExhausted)?;
    let mut result = BTreeMap::new();
    for group in &merge.hypothesis_groups {
        if group
            .alternatives
            .iter()
            .filter(|alternative| hypotheses.contains_key(*alternative))
            .count()
            < 2
        {
            continue;
        }
        result.insert(group.key.clone(), ontology::HypothesisSetId::new(next));
        next = next
            .checked_add(1)
            .ok_or(ProjectReadingQueryError::IdentityExhausted)?;
    }
    Ok(result)
}

fn parse_foreign_note(note: &str) -> Option<QualifiedEntityId> {
    let mut parts = note.strip_prefix("foreign:")?.split(':');
    let reading = ReadingId::from_str(parts.next()?).ok()?;
    let kind = parts.next()?.to_owned();
    let local = parts.next()?.parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    QualifiedEntityId::new(reading, kind, local).ok()
}

fn hypothesis_objects(
    air: &ontology::AuditoryIr,
    hypothesis: &ontology::Hypothesis,
) -> Vec<ontology::ObjectId> {
    let mut objects = Vec::new();
    for claim in &hypothesis.claims {
        match claim {
            HypothesisClaim::GroupsObjects(values)
            | HypothesisClaim::SeparatesObjects(values)
            | HypothesisClaim::FreeformPerceptualDescription {
                objects: values, ..
            } => objects.extend(values.iter().copied()),
            HypothesisClaim::PitchTrack { object, .. } => objects.push(*object),
            HypothesisClaim::Relation(relation) => {
                if let Some(relation) = air.relations.get(relation) {
                    objects.extend([relation.from, relation.to]);
                }
            }
            HypothesisClaim::TransformApplies(_) => {}
        }
    }
    objects.sort();
    objects.dedup();
    objects
}

fn channel_mask(channels: u16) -> ChannelMask {
    let channels = channels.clamp(1, 16);
    if channels == 16 {
        ChannelMask(u16::MAX)
    } else {
        ChannelMask((1_u16 << channels) - 1)
    }
}

fn channel_mask_for_selection(selection: &ChannelSelection, channels: u16) -> ChannelMask {
    match selection {
        ChannelSelection::All => channel_mask(channels),
        ChannelSelection::Channel(channel) => (channel < &16)
            .then(|| ChannelMask(1_u16 << channel))
            .unwrap_or_default(),
        ChannelSelection::Mid | ChannelSelection::Side => ChannelMask(0b11),
        ChannelSelection::Channels(values) => values
            .iter()
            .copied()
            .filter(|channel| *channel < 16)
            .fold(ChannelMask(0), |mask, channel| {
                mask.union(ChannelMask(1_u16 << channel))
            }),
    }
}

fn contains_signal_reference(aspect: &Aspect) -> bool {
    match aspect {
        Aspect::ExplainedBy(_) | Aspect::ResidualOf(_) => true,
        Aspect::Union(children) | Aspect::Intersect(children) => {
            children.iter().any(contains_signal_reference)
        }
        Aspect::Complement(child) => contains_signal_reference(child),
        Aspect::Empty
        | Aspect::All
        | Aspect::Time(_)
        | Aspect::Band(_)
        | Aspect::Channels(_)
        | Aspect::Family { .. }
        | Aspect::Object(_) => false,
    }
}

#[derive(Debug)]
pub enum ProjectReadingQueryError {
    Session(ProjectSessionError),
    Workbench(WorkbenchError),
    Protocol(String),
    Snapshot(String),
    Aspect(String),
    InvalidImport(String),
    AlreadyImported {
        qualified_entities: usize,
    },
    QueryProvenanceMismatch {
        captured: QueryExecutionProvenance,
        requested: QueryExecutionProvenance,
    },
    IdentityExhausted,
    DocumentReplaced {
        expected: u64,
        actual: u64,
    },
    StaleSnapshot {
        expected: u64,
        actual: u64,
    },
    UndoHeadMoved {
        expected_revision: u64,
        actual_revision: u64,
    },
    NothingToUndo,
}

impl From<ProjectSessionError> for ProjectReadingQueryError {
    fn from(value: ProjectSessionError) -> Self {
        Self::Session(value)
    }
}

impl From<WorkbenchError> for ProjectReadingQueryError {
    fn from(value: WorkbenchError) -> Self {
        Self::Workbench(value)
    }
}

impl fmt::Display for ProjectReadingQueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "project reading/query session error: {self:?}")
    }
}

impl std::error::Error for ProjectReadingQueryError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::air_query::workbench::protocol::LocalSourceDto;
    use crate::air_query::workbench::{
        FactKindDto, PortableEntityRecord, PortableEntityRole, PortableEntitySection,
        PortableHypothesisSemantics, QueryDocumentId, QueryTermDto, ENTITY_SECTION_MAJOR,
        ENTITY_SECTION_NAME,
    };
    use crate::air_query::NeverCancel;
    use crate::daw_engine::AssetPcmMap;
    use crate::daw_project::{DawProject, ProjectDomain};
    use crate::live_project::LiveProject;
    use crate::reading::{
        PortableDigestAlgorithm, ProducerDto, ProvenanceDto, ReadingFile, ReadingSection,
        ReadingSource, READING_FORMAT, READING_FORMAT_VERSION,
    };

    fn session_with_source() -> ProjectSession {
        let mut project = DawProject::new("query", 48_000, 120.0).unwrap();
        project
            .transact("source", 0, BTreeSet::from([ProjectDomain::Air]), |state| {
                state
                    .domains
                    .air
                    .insert_source(ontology::AudioSource {
                        id: ontology::SourceId::new(7),
                        uri: "asset:7".into(),
                        content_digest: Some("sha256:test".into()),
                        sample_rate: 48_000,
                        channels: 2,
                        frame_count: 96_000,
                    })
                    .map_err(|error| error.to_string())
            })
            .unwrap();
        let live = LiveProject::from_project(project, AssetPcmMap::new()).unwrap();
        let mut session = ProjectSession::new(crate::project_session::ProjectSessionId(8)).unwrap();
        session.install(live, None).unwrap();
        session
    }

    fn digest(byte: u8) -> PortableDigest {
        PortableDigest {
            algorithm: PortableDigestAlgorithm::Sha256,
            bytes: [byte; 32],
        }
    }

    fn reading(id: u8, group: &str) -> ReadingInputDto {
        let reading_id = ReadingId::new([id; 16]).unwrap();
        let source = ReadingSource {
            fingerprints: vec![digest(9)],
            sample_rate: 48_000,
            channels: 2,
            frame_count: 96_000,
            declared_title: None,
            extensions: BTreeMap::new(),
        };
        ReadingInputDto {
            reading: ReadingFile {
                format: READING_FORMAT.into(),
                version: READING_FORMAT_VERSION,
                reading_id,
                revision: 1,
                parents: Vec::new(),
                author: ProvenanceDto {
                    producer: ProducerDto::Human { name: None },
                    created_unix_ms: None,
                    source_revision: None,
                    note: None,
                },
                source,
                sections: vec![ReadingSection {
                    name: ENTITY_SECTION_NAME.into(),
                    schema_major: ENTITY_SECTION_MAJOR,
                    schema_minor: 0,
                    payload: serde_json::to_value(PortableEntitySection {
                        entities: vec![PortableEntityRecord {
                            kind: "hypothesis".into(),
                            local_id: 1,
                            label: format!("alternative-{id}"),
                            role: PortableEntityRole::Hypothesis,
                            hypothesis: Some(PortableHypothesisSemantics {
                                support: 0.5,
                                description: Some(format!("reading {id}")),
                            }),
                            hypothesis_group: Some(group.into()),
                            extent: None,
                            extensions: BTreeMap::new(),
                        }],
                        extensions: BTreeMap::new(),
                    })
                    .unwrap(),
                    extensions: BTreeMap::new(),
                }],
                attachments: Vec::new(),
                extensions: BTreeMap::new(),
            },
            local_source: Some(LocalSourceDto {
                digest: digest(9),
                sample_rate: 48_000,
                channels: 2,
                frame_count: 96_000,
            }),
        }
    }

    fn bridge(session: &ProjectSession) -> ProjectReadingQuerySession {
        ProjectReadingQuerySession::new(
            session,
            &ArtifactCatalog::new(),
            &InterpretationStore::new(),
            ProjectQueryResolverInputs::default(),
            Arc::new(|_| {}),
        )
        .unwrap()
    }

    fn source_query_document() -> QueryDocument {
        QueryDocument::new(
            QueryDocumentId(1),
            "sources",
            QueryTermDto::Kind {
                kind: FactKindDto::Source,
            },
        )
    }

    #[test]
    fn project_backed_query_uses_published_air_and_strong_provenance() {
        let session = session_with_source();
        let bridge = bridge(&session);
        let mut document = source_query_document();
        bridge
            .execute_page(&mut document, QueryPageRequest::default(), &NeverCancel)
            .unwrap();
        let result = document.latest_result().unwrap();
        assert!(result.provenance.fact_base_digest.is_strong());
        assert_eq!(result.page.hits.len(), 1);
        assert!(matches!(
            result.page.hits[0].fact,
            crate::interpretation_navigation::EntityRefDto::Project {
                ref kind,
                local_id: 7
            } if kind == "air-source"
        ));
    }

    #[test]
    fn headless_query_refuses_any_provenance_not_exactly_captured() {
        let session = session_with_source();
        let bridge = bridge(&session);
        let captured = bridge.snapshot().provenance();
        let request = |provenance| HeadlessRequest {
            protocol: crate::air_query::workbench::protocol::HEADLESS_PROTOCOL.into(),
            request_id: "captured-query".into(),
            operation: HeadlessOperation::QueryPage {
                document: source_query_document(),
                provenance,
                page: QueryPageRequest::default(),
            },
        };

        let mut stale_revision = captured.clone();
        stale_revision.fact_base_revision += 1;
        assert!(matches!(
            bridge
                .snapshot()
                .dispatch(request(stale_revision), &NeverCancel),
            Err(ProjectReadingQueryError::QueryProvenanceMismatch { .. })
        ));

        let mut wrong_digest = captured.clone();
        wrong_digest.fact_base_digest.bytes[0] ^= 0xff;
        assert!(matches!(
            bridge
                .snapshot()
                .dispatch(request(wrong_digest), &NeverCancel),
            Err(ProjectReadingQueryError::QueryProvenanceMismatch { .. })
        ));

        let mut wrong_source = captured;
        wrong_source.source_revision = Some("another-project-publication".into());
        assert!(matches!(
            bridge
                .snapshot()
                .dispatch(request(wrong_source), &NeverCancel),
            Err(ProjectReadingQueryError::QueryProvenanceMismatch { .. })
        ));
    }

    #[test]
    fn pristine_revision_zero_is_reported_and_refused_not_fabricated() {
        let project = DawProject::new("pristine", 48_000, 120.0).unwrap();
        let live = LiveProject::from_project(project, AssetPcmMap::new()).unwrap();
        let mut session =
            ProjectSession::new(crate::project_session::ProjectSessionId(10)).unwrap();
        session.install(live, None).unwrap();
        let bridge = bridge(&session);
        assert_eq!(bridge.snapshot().provenance().fact_base_revision, 0);

        let mut document = source_query_document();
        assert!(matches!(
            bridge.execute_page(&mut document, QueryPageRequest::default(), &NeverCancel),
            Err(ProjectReadingQueryError::Workbench(
                WorkbenchError::InvalidExecution(_)
            ))
        ));
        assert!(document.results.is_empty());
    }

    #[test]
    fn reading_import_is_one_session_command_and_guarded_undo() {
        let mut session = session_with_source();
        let bridge = bridge(&session);
        let plan = bridge
            .plan_import(
                &[reading(1, "identity"), reading(2, "identity")],
                UnknownSectionPolicy::PreserveOpaque,
            )
            .unwrap();
        assert_eq!(plan.lowered.envelope.commands.len(), 3);
        let receipt = bridge.apply_import(&mut session, plan).unwrap();
        let air = &session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .air;
        assert_eq!(air.hypotheses.len(), 2);
        assert_eq!(air.hypothesis_sets.len(), 1);
        bridge.undo_import(&mut session, &receipt).unwrap();
        let air = &session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .air;
        assert!(air.hypotheses.is_empty());
        assert!(air.hypothesis_sets.is_empty());
    }

    #[test]
    fn reopened_project_recovers_reading_qualified_ids_from_air_provenance() {
        let mut session = session_with_source();
        let initial_bridge = bridge(&session);
        let inputs = [reading(3, "identity"), reading(4, "identity")];
        let plan = initial_bridge
            .plan_import(&inputs, UnknownSectionPolicy::PreserveOpaque)
            .unwrap();
        initial_bridge.apply_import(&mut session, plan).unwrap();

        let frozen = session.project_snapshot().unwrap().clone();
        let reopened_live =
            LiveProject::from_project((*frozen.project).clone(), (*frozen.pcm).clone()).unwrap();
        let mut reopened =
            ProjectSession::new(crate::project_session::ProjectSessionId(9)).unwrap();
        reopened.install(reopened_live, None).unwrap();
        let reopened_bridge = bridge(&reopened);
        assert_eq!(
            reopened_bridge.snapshot().existing_foreign_entities().len(),
            2
        );
        assert!(matches!(
            reopened_bridge.plan_import(&inputs, UnknownSectionPolicy::PreserveOpaque),
            Err(ProjectReadingQueryError::AlreadyImported {
                qualified_entities: 2
            })
        ));
    }
}
