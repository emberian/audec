//! Persistent, toolkit-neutral workbench flows for AIR queries and readings.
//!
//! The module owns portable query documents, frozen result snapshots, action
//! targets, and import plans. It deliberately does not render UI or mutate an
//! AIR graph. Hosts can persist the models directly and turn the returned
//! workspace extension descriptor into a GPUI pane when a renderer exists.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{AirFacts, FactKind, FactRef, NeverCancel, Query, QueryCancellation, QueryError};
use crate::artifact_catalog::sha256_content;
use crate::aspect::{Aspect, AspectResolver, BandSpan, ChannelMask, ExplanationRef, FrameSpan};
use crate::command::{claims_for_commands, AirCommand, CommandEnvelope, DomainCommand};
use crate::command_record::{AirEntityKind, ForeignEntityKind, IdClaim};
use crate::coverage::CoverageField;
use crate::interpretation_navigation::{
    rank_coverage_hotspots, AspectGeometryDto, EntityRefDto, QueryDerivationDto, QueryHitDto,
    QueryResultPageDto, RegionDto, SignalLayerDto,
};
use crate::reading::{
    replication_tier, LocalSourceDescriptor, PortableDigest, PortableDigestAlgorithm,
    QualifiedEntityId, ReadingError, ReadingFile, ReadingId, ReadingVerificationRefusal,
    ReplicationCheck, VerificationTier,
};
use crate::reading_codec::{decode_section, ReadingCodecError};
use crate::reconstruction::ReconstructionProposalId;
use crate::workspace_document::{
    EditorTarget, EditorViewState, LinkFacets, LinkGroupId, NewWorkspaceView, ViewLinkMembership,
    WorkspaceItemKind,
};

#[path = "reading_query_protocol.rs"]
pub mod protocol;
#[path = "reading_workflow.rs"]
pub mod reading_workflow;
use crate::{ontology, reading::ReadingSection};
pub use reading_workflow::*;

pub const QUERY_DOCUMENT_FORMAT: &str = "audec-query-document";
pub const QUERY_DOCUMENT_VERSION: u32 = 2;
pub const WORKBENCH_NAMESPACE: &str = "audec.interpretation";
pub const WORKBENCH_VIEW_NAME: &str = "reading-query-workbench";
pub const ENTITY_SECTION_NAME: &str = "entities";
pub const ENTITY_SECTION_MAJOR: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QueryDocumentId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactKindDto {
    Object,
    Source,
    Parameter,
    Hypothesis,
}

impl From<FactKindDto> for FactKind {
    fn from(value: FactKindDto) -> Self {
        match value {
            FactKindDto::Object => Self::Object,
            FactKindDto::Source => Self::Source,
            FactKindDto::Parameter => Self::Parameter,
            FactKindDto::Hypothesis => Self::Hypothesis,
        }
    }
}

/// Serializable counterpart of [`Query`]. Concrete geometry is used for a
/// `within` selector so query documents do not retain runtime resolver handles.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operator", rename_all = "snake_case")]
pub enum QueryTermDto {
    Kind { kind: FactKindDto },
    Within { aspect: AspectGeometryDto },
    Related { to: Box<QueryTermDto> },
    NotExplainedBy { proposal_id: u64 },
    And { terms: Vec<QueryTermDto> },
    Or { terms: Vec<QueryTermDto> },
    Not { term: Box<QueryTermDto> },
}

impl QueryTermDto {
    pub fn compile(&self) -> Result<Query, WorkbenchError> {
        match self {
            Self::Kind { kind } => Ok(Query::Kind((*kind).into())),
            Self::Within { aspect } => Ok(Query::Within(compile_aspect(aspect)?)),
            Self::Related { to } => Ok(Query::Related {
                to: Box::new(to.compile()?),
            }),
            Self::NotExplainedBy { proposal_id } if *proposal_id != 0 => Ok(Query::NotExplainedBy(
                ReconstructionProposalId::from_raw(*proposal_id),
            )),
            Self::NotExplainedBy { .. } => Err(WorkbenchError::InvalidQuery(
                "a reconstruction proposal id cannot be zero".into(),
            )),
            Self::And { terms } => Ok(Query::And(
                terms
                    .iter()
                    .map(Self::compile)
                    .collect::<Result<Vec<_>, _>>()?,
            )),
            Self::Or { terms } => Ok(Query::Or(
                terms
                    .iter()
                    .map(Self::compile)
                    .collect::<Result<Vec<_>, _>>()?,
            )),
            Self::Not { term } => Ok(Query::Not(Box::new(term.compile()?))),
        }
    }

    pub fn stable_label(&self) -> String {
        match self {
            Self::Kind { kind } => format!("kind:{kind:?}").to_ascii_lowercase(),
            Self::Within { aspect } => format!("within:{}-regions", aspect.regions.len()),
            Self::Related { to } => format!("related({})", to.stable_label()),
            Self::NotExplainedBy { proposal_id } => {
                format!("not-explained-by:proposal:{proposal_id}")
            }
            Self::And { terms } => stable_join("and", terms),
            Self::Or { terms } => stable_join("or", terms),
            Self::Not { term } => format!("not({})", term.stable_label()),
        }
    }
}

fn stable_join(operator: &str, terms: &[QueryTermDto]) -> String {
    format!(
        "{operator}({})",
        terms
            .iter()
            .map(QueryTermDto::stable_label)
            .collect::<Vec<_>>()
            .join(",")
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryExecutionProvenance {
    /// Revision of the finite fact base used for this result.
    pub fact_base_revision: u64,
    /// Strong identity of the complete fact snapshot, not merely its counter.
    pub fact_base_digest: PortableDigest,
    /// Optional durable project/read snapshot label supplied by the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
    /// Omitted for reproducible/headless execution rather than fabricated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executed_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryResultSnapshot {
    pub document_revision: u64,
    pub content_address: PortableDigest,
    pub page_start: u64,
    pub provenance: QueryExecutionProvenance,
    pub page: QueryResultPageDto,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryPageRequest {
    pub limit: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl Default for QueryPageRequest {
    fn default() -> Self {
        Self {
            limit: 100,
            cursor: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct QueryCancellationToken(Arc<AtomicBool>);

impl QueryCancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

impl QueryCancellation for QueryCancellationToken {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryDocument {
    pub format: String,
    pub version: u32,
    pub id: QueryDocumentId,
    pub revision: u64,
    pub title: String,
    pub query: QueryTermDto,
    #[serde(default)]
    pub results: Vec<QueryResultSnapshot>,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, Value>,
}

impl QueryDocument {
    pub fn new(id: QueryDocumentId, title: impl Into<String>, query: QueryTermDto) -> Self {
        Self {
            format: QUERY_DOCUMENT_FORMAT.into(),
            version: QUERY_DOCUMENT_VERSION,
            id,
            revision: 1,
            title: title.into(),
            query,
            results: Vec::new(),
            extensions: BTreeMap::new(),
        }
    }

    pub fn validate(&self) -> Result<(), WorkbenchError> {
        if self.format != QUERY_DOCUMENT_FORMAT {
            return Err(WorkbenchError::WrongDocumentFormat(self.format.clone()));
        }
        if self.version != QUERY_DOCUMENT_VERSION {
            return Err(WorkbenchError::UnsupportedDocumentVersion(self.version));
        }
        if self.id.0 == 0 || self.revision == 0 || self.title.trim().is_empty() {
            return Err(WorkbenchError::InvalidDocument(
                "id, revision, and non-blank title are required".into(),
            ));
        }
        self.query.compile()?;
        let mut revisions = BTreeSet::new();
        for result in &self.results {
            if result.document_revision == 0
                || result.document_revision > self.revision
                || result.page.result_revision != result.provenance.fact_base_revision
                || !result.provenance.fact_base_digest.is_strong()
                || !result.content_address.is_strong()
                || !revisions.insert((
                    result.document_revision,
                    result.provenance.fact_base_revision,
                    result.content_address,
                    result.page_start,
                ))
            {
                return Err(WorkbenchError::InvalidDocument(
                    "result revisions are invalid or duplicated".into(),
                ));
            }
            validate_result_page(&result.page)?;
            if result.page.query_term.trim().is_empty() {
                return Err(WorkbenchError::InvalidResult(
                    "query term cannot be blank".into(),
                ));
            }
            if let Some(cursor) = &result.page.next_cursor {
                let expected = result
                    .page_start
                    .saturating_add(result.page.hits.len() as u64);
                if decode_cursor(cursor, result.content_address)? != expected {
                    return Err(WorkbenchError::InvalidResult(
                        "next cursor does not follow its page".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn replace_query(&mut self, query: QueryTermDto) -> Result<(), WorkbenchError> {
        query.compile()?;
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| WorkbenchError::InvalidDocument("revision exhausted".into()))?;
        self.query = query;
        Ok(())
    }

    pub fn latest_result(&self) -> Option<&QueryResultSnapshot> {
        self.results.last()
    }

    pub fn to_json(&self) -> Result<Vec<u8>, WorkbenchError> {
        self.validate()?;
        let mut bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| WorkbenchError::Json(error.to_string()))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, WorkbenchError> {
        let value = serde_json::from_slice::<Self>(bytes)
            .map_err(|error| WorkbenchError::Json(error.to_string()))?;
        value.validate()?;
        Ok(value)
    }
}

fn validate_result_page(page: &QueryResultPageDto) -> Result<(), WorkbenchError> {
    let mut facts = BTreeSet::new();
    for hit in &page.hits {
        hit.fact
            .validate()
            .map_err(|error| WorkbenchError::InvalidResult(format!("{error:?}")))?;
        if !facts.insert(hit.fact.clone()) {
            return Err(WorkbenchError::InvalidResult(
                "a result page contains a duplicate fact".into(),
            ));
        }
        for premise in &hit.derivation.premises {
            premise
                .validate()
                .map_err(|error| WorkbenchError::InvalidResult(format!("{error:?}")))?;
        }
        if hit.derivation.rule.trim().is_empty() {
            return Err(WorkbenchError::InvalidResult(
                "a derivation rule cannot be blank".into(),
            ));
        }
        if let Some(extent) = &hit.extent {
            validate_geometry(extent)?;
        }
    }
    Ok(())
}

/// Execute and freeze one result against an explicitly named fact-base
/// revision. Existing snapshots are history, not a cache entry to overwrite.
pub fn execute_query_document<'a>(
    document: &'a mut QueryDocument,
    facts: &dyn AirFacts,
    resolver: &dyn AspectResolver,
    provenance: QueryExecutionProvenance,
) -> Result<&'a QueryResultSnapshot, WorkbenchError> {
    execute_query_page(
        document,
        facts,
        resolver,
        provenance,
        QueryPageRequest {
            limit: u32::MAX,
            cursor: None,
        },
        &NeverCancel,
    )
}

/// Execute a content-addressed page. Cursors bind the offset to the exact
/// query/fact content address, so they cannot be replayed after either input
/// changes. Cancellation never appends a partial snapshot.
pub fn execute_query_page<'a>(
    document: &'a mut QueryDocument,
    facts: &dyn AirFacts,
    resolver: &dyn AspectResolver,
    provenance: QueryExecutionProvenance,
    request: QueryPageRequest,
    cancellation: &dyn QueryCancellation,
) -> Result<&'a QueryResultSnapshot, WorkbenchError> {
    document.validate()?;
    if provenance.fact_base_revision == 0 || !provenance.fact_base_digest.is_strong() {
        return Err(WorkbenchError::InvalidExecution(
            "fact-base revision must be nonzero and its digest must be strong".into(),
        ));
    }
    if request.limit == 0 {
        return Err(WorkbenchError::InvalidPage(
            "page limit cannot be zero".into(),
        ));
    }
    let content_address = query_content_address(&document.query, provenance.fact_base_digest)?;
    let page_start = request
        .cursor
        .as_deref()
        .map(|cursor| decode_cursor(cursor, content_address))
        .transpose()?
        .unwrap_or(0);
    if cancellation.is_cancelled() {
        return Err(WorkbenchError::Query(QueryError::Cancelled));
    }
    let query = document.query.compile()?;
    let rows = super::run_cancellable(&query, facts, resolver, cancellation)
        .map_err(WorkbenchError::Query)?;
    let total_rows = rows.len();
    let start = usize::try_from(page_start)
        .map_err(|_| WorkbenchError::InvalidPage("cursor offset exceeds this platform".into()))?;
    if start > rows.len() {
        return Err(WorkbenchError::InvalidPage(
            "cursor offset is beyond the result set".into(),
        ));
    }
    let end = start.saturating_add(request.limit as usize).min(rows.len());
    let mut hits = Vec::with_capacity(end.saturating_sub(start));
    for (fact, derivation) in rows.into_iter().skip(start).take(end - start) {
        if cancellation.is_cancelled() {
            return Err(WorkbenchError::Query(QueryError::Cancelled));
        }
        let fact_dto = project_fact(fact);
        let mut premises = derivation
            .premises
            .into_iter()
            .chain(facts.evidence_of(fact))
            .map(project_fact)
            .collect::<Vec<_>>();
        premises.sort();
        premises.dedup();
        hits.push(QueryHitDto {
            fact: fact_dto,
            extent: facts
                .extent(fact)
                .as_ref()
                .map(AspectGeometryDto::from_project),
            derivation: QueryDerivationDto {
                rule: derivation.rule.into(),
                premises,
            },
        });
    }
    let next_cursor = (end < total_rows).then(|| encode_cursor(content_address, end as u64));
    let snapshot = QueryResultSnapshot {
        document_revision: document.revision,
        content_address,
        page_start,
        page: QueryResultPageDto {
            query_term: document.query.stable_label(),
            result_revision: provenance.fact_base_revision,
            hits,
            next_cursor,
        },
        provenance,
    };
    validate_result_page(&snapshot.page)?;
    if document.results.iter().any(|existing| {
        existing.document_revision == snapshot.document_revision
            && existing.content_address == snapshot.content_address
            && existing.page_start == snapshot.page_start
    }) {
        return Err(WorkbenchError::DuplicatePage {
            address: snapshot.content_address,
            start: snapshot.page_start,
        });
    }
    document.results.push(snapshot);
    Ok(document.results.last().expect("a result was just inserted"))
}

fn query_content_address(
    query: &QueryTermDto,
    fact_base: PortableDigest,
) -> Result<PortableDigest, WorkbenchError> {
    let query =
        serde_json::to_vec(query).map_err(|error| WorkbenchError::Json(error.to_string()))?;
    let algorithm = match fact_base.algorithm {
        PortableDigestAlgorithm::Sha256 => b"sha256".as_slice(),
        PortableDigestAlgorithm::Blake3 => b"blake3".as_slice(),
        PortableDigestAlgorithm::StableNonCryptographic => b"stable".as_slice(),
    };
    Ok(sha256_content(
        b"audec-air-query-execution-v1",
        &[&query, algorithm, &fact_base.bytes],
    )
    .into())
}

fn encode_cursor(address: PortableDigest, offset: u64) -> String {
    format!("q1:{}:{offset}", hex_bytes(&address.bytes))
}

fn decode_cursor(cursor: &str, expected: PortableDigest) -> Result<u64, WorkbenchError> {
    let mut parts = cursor.split(':');
    let version = parts.next();
    let digest = parts.next();
    let offset = parts.next();
    if version != Some("q1")
        || parts.next().is_some()
        || digest != Some(&hex_bytes(&expected.bytes))
    {
        return Err(WorkbenchError::StaleCursor);
    }
    offset
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| WorkbenchError::InvalidPage("cursor offset is invalid".into()))
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn project_fact(fact: FactRef) -> EntityRefDto {
    let (kind, local_id) = match fact {
        FactRef::Object(id) => ("air-object", id.get()),
        FactRef::Source(id) => ("air-source", id.get()),
        FactRef::Parameter(id) => ("air-parameter", id.get()),
        FactRef::Hypothesis(id) => ("air-hypothesis", id.get()),
    };
    EntityRefDto::Project {
        kind: kind.into(),
        local_id,
    }
}

fn compile_aspect(dto: &AspectGeometryDto) -> Result<Aspect, WorkbenchError> {
    validate_geometry(dto)?;
    let mut terms = Vec::new();
    if !dto.regions.is_empty() {
        terms.push(Aspect::Union(
            dto.regions
                .iter()
                .map(|region| {
                    Ok(Aspect::Intersect(vec![
                        Aspect::Time(
                            FrameSpan::new(region.start_frame, region.end_frame)
                                .ok_or(WorkbenchError::InvalidRegion(*region))?,
                        ),
                        Aspect::Band(
                            BandSpan::new(region.min_hz(), region.max_hz())
                                .ok_or(WorkbenchError::InvalidRegion(*region))?,
                        ),
                        Aspect::Channels(ChannelMask(region.channels)),
                    ]))
                })
                .collect::<Result<Vec<_>, WorkbenchError>>()?,
        ));
    }
    if !dto.objects.is_empty() {
        terms.push(Aspect::Union(
            dto.objects
                .iter()
                .map(|reference| match reference {
                    EntityRefDto::Project { kind, local_id }
                        if kind == "air-object" && *local_id != 0 =>
                    {
                        Ok(Aspect::Object(crate::ontology::ObjectId::new(*local_id)))
                    }
                    EntityRefDto::Reading(_) => Err(
                        WorkbenchError::QualifiedReferenceInProjectQuery(reference.clone()),
                    ),
                    _ => Err(WorkbenchError::InvalidQuery(format!(
                        "{reference:?} is not an AIR object"
                    ))),
                })
                .collect::<Result<Vec<_>, _>>()?,
        ));
    }
    match &dto.signal {
        SignalLayerDto::Source => {}
        SignalLayerDto::Explanation { reference } => {
            terms.push(Aspect::ExplainedBy(project_explanation(reference)?));
        }
        SignalLayerDto::Residual { reference } => {
            terms.push(Aspect::ResidualOf(project_explanation(reference)?));
        }
    }
    Ok(match terms.len() {
        0 => Aspect::All,
        1 => terms.pop().expect("one aspect term"),
        _ => Aspect::Intersect(terms),
    })
}

fn project_explanation(reference: &EntityRefDto) -> Result<ExplanationRef, WorkbenchError> {
    match reference {
        EntityRefDto::Project { kind, local_id } if *local_id != 0 => match kind.as_str() {
            "explanation" => Ok(ExplanationRef::Definition(*local_id)),
            "reconstruction-proposal" => Ok(ExplanationRef::Proposal(
                ReconstructionProposalId::from_raw(*local_id),
            )),
            "comparison" => Ok(ExplanationRef::Comparison(*local_id)),
            _ => Err(WorkbenchError::InvalidQuery(format!(
                "unsupported signal reference kind {kind}"
            ))),
        },
        EntityRefDto::Reading(_) => Err(WorkbenchError::QualifiedReferenceInProjectQuery(
            reference.clone(),
        )),
        _ => Err(WorkbenchError::InvalidQuery(
            "signal reference id cannot be zero".into(),
        )),
    }
}

fn validate_geometry(dto: &AspectGeometryDto) -> Result<(), WorkbenchError> {
    for region in &dto.regions {
        region
            .validate()
            .map_err(|_| WorkbenchError::InvalidRegion(*region))?;
    }
    for object in &dto.objects {
        object
            .validate()
            .map_err(|_| WorkbenchError::InvalidEntity(object.clone()))?;
    }
    let reference = match &dto.signal {
        SignalLayerDto::Source => None,
        SignalLayerDto::Explanation { reference } | SignalLayerDto::Residual { reference } => {
            Some(reference)
        }
    };
    if let Some(reference) = reference {
        reference
            .validate()
            .map_err(|_| WorkbenchError::InvalidEntity(reference.clone()))?;
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevealTarget {
    pub entity: EntityRefDto,
    pub extent: Option<AspectGeometryDto>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditionTarget {
    pub entity: EntityRefDto,
    pub extent: AspectGeometryDto,
}

/// Renderer-agnostic pane state. A GPUI entity can own this type without
/// duplicating selection, refusal, or persistence logic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkbenchPaneModel {
    document: QueryDocument,
    selected_row: Option<usize>,
}

impl WorkbenchPaneModel {
    pub fn document(&self) -> &QueryDocument {
        &self.document
    }

    pub fn rows(&self) -> &[QueryHitDto] {
        self.document
            .latest_result()
            .map(|result| result.page.hits.as_slice())
            .unwrap_or_default()
    }

    pub fn selected_row(&self) -> Option<usize> {
        self.selected_row
    }

    pub fn select_row(&mut self, row: usize) -> Result<(), WorkbenchActionRefusal> {
        if row >= self.rows().len() {
            return Err(WorkbenchActionRefusal::UnknownRow(row));
        }
        self.selected_row = Some(row);
        Ok(())
    }

    pub fn reveal_target(&self) -> Result<RevealTarget, WorkbenchActionRefusal> {
        let hit = self.selected_hit()?;
        Ok(RevealTarget {
            entity: hit.fact.clone(),
            extent: hit.extent.clone(),
        })
    }

    pub fn audition_target(&self) -> Result<AuditionTarget, WorkbenchActionRefusal> {
        let hit = self.selected_hit()?;
        Ok(AuditionTarget {
            entity: hit.fact.clone(),
            extent: hit
                .extent
                .clone()
                .ok_or_else(|| WorkbenchActionRefusal::MissingExtent(hit.fact.clone()))?,
        })
    }

    fn selected_hit(&self) -> Result<&QueryHitDto, WorkbenchActionRefusal> {
        let row = self
            .selected_row
            .ok_or(WorkbenchActionRefusal::NoSelection)?;
        self.rows()
            .get(row)
            .ok_or(WorkbenchActionRefusal::UnknownRow(row))
    }
}

pub struct WorkbenchPaneFactory;

impl WorkbenchPaneFactory {
    pub fn model(document: QueryDocument) -> Result<WorkbenchPaneModel, WorkbenchError> {
        document.validate()?;
        Ok(WorkbenchPaneModel {
            document,
            selected_row: None,
        })
    }

    /// A durable extension view. The JSON state is enough for a future GPUI
    /// factory to recreate the pane; there are no runtime entity handles here.
    pub fn workspace_view(document: &QueryDocument) -> Result<NewWorkspaceView, WorkbenchError> {
        document.validate()?;
        Ok(NewWorkspaceView {
            kind: WorkspaceItemKind::Extension {
                namespace: WORKBENCH_NAMESPACE.into(),
                name: WORKBENCH_VIEW_NAME.into(),
            },
            target: EditorTarget::Extension {
                namespace: WORKBENCH_NAMESPACE.into(),
                key: format!("query-document:{}", document.id.0),
            },
            title_override: Some(document.title.clone()),
            links: ViewLinkMembership {
                group: LinkGroupId::UNLINKED,
                facets: LinkFacets::NONE,
            },
            state: EditorViewState::Extension {
                data: serde_json::to_value(document)
                    .map_err(|error| WorkbenchError::Json(error.to_string()))?,
            },
            extensions: BTreeMap::new(),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResidualGuide {
    pub query_document: QueryDocument,
    pub auditions: Vec<AuditionTarget>,
}

/// Build one query and audition queue from the same deterministic residual
/// ranking. Explained and excess values remain in the source coverage DTO;
/// this helper never turns rank into correctness or confidence.
pub fn residual_guide(
    id: QueryDocumentId,
    title: impl Into<String>,
    field: &CoverageField,
    comparison_id: u64,
    proposal_id: u64,
    limit: usize,
) -> Result<ResidualGuide, WorkbenchError> {
    if comparison_id == 0 || proposal_id == 0 {
        return Err(WorkbenchError::InvalidQuery(
            "comparison and proposal ids cannot be zero".into(),
        ));
    }
    let comparison = crate::comparison::ComparisonId(comparison_id);
    let hotspots = rank_coverage_hotspots(field, comparison, limit);
    let regions = hotspots
        .iter()
        .map(|hotspot| hotspot.region)
        .collect::<Vec<_>>();
    let geometry = AspectGeometryDto {
        regions: regions.clone(),
        objects: Vec::new(),
        signal: SignalLayerDto::Source,
    };
    let query = QueryTermDto::And {
        terms: vec![
            QueryTermDto::NotExplainedBy { proposal_id },
            QueryTermDto::Within { aspect: geometry },
        ],
    };
    let query_document = QueryDocument::new(id, title, query);
    query_document.validate()?;
    let entity = EntityRefDto::Project {
        kind: "comparison".into(),
        local_id: comparison_id,
    };
    let auditions = regions
        .into_iter()
        .map(|region| AuditionTarget {
            entity: entity.clone(),
            extent: AspectGeometryDto {
                regions: vec![region],
                objects: Vec::new(),
                signal: SignalLayerDto::Residual {
                    reference: entity.clone(),
                },
            },
        })
        .collect();
    Ok(ResidualGuide {
        query_document,
        auditions,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortableEntityRole {
    Object,
    Evidence,
    Hypothesis,
    Explanation,
    Comparison,
    Other,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PortableEntityRecord {
    pub kind: String,
    pub local_id: u64,
    pub label: String,
    pub role: PortableEntityRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hypothesis: Option<PortableHypothesisSemantics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hypothesis_group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extent: Option<AspectGeometryDto>,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PortableHypothesisSemantics {
    /// Relative support within the portable alternative group; not a
    /// probability and never an import-selection instruction.
    pub support: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PortableEntitySection {
    #[serde(default)]
    pub entities: Vec<PortableEntityRecord>,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownSectionPolicy {
    PreserveOpaque,
    Refuse,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadingImportOptions {
    pub unknown_sections: UnknownSectionPolicy,
    pub require_entity_section: bool,
}

impl Default for ReadingImportOptions {
    fn default() -> Self {
        Self {
            unknown_sections: UnknownSectionPolicy::PreserveOpaque,
            require_entity_section: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportDisposition {
    AddQualified,
    PreserveCoexistingHypothesis,
    AlreadyPresent,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlannedReadingEntity {
    pub id: QualifiedEntityId,
    pub label: String,
    pub role: PortableEntityRole,
    pub hypothesis: Option<PortableHypothesisSemantics>,
    pub hypothesis_group: Option<String>,
    pub extent: Option<AspectGeometryDto>,
    pub disposition: ImportDisposition,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReadingImportPlan {
    pub reading_id: ReadingId,
    pub reading_revision: u64,
    pub verification: VerificationTier,
    pub entities: Vec<PlannedReadingEntity>,
    /// Unknown sections remain in the reading envelope and are named here so
    /// a caller cannot mistake opacity for successful semantic import.
    pub opaque_sections: Vec<String>,
    /// Exact opaque values retained for persistence or re-export.
    pub preserved_sections: Vec<ReadingSection>,
}

impl ReadingImportPlan {
    pub fn record_replication(
        &mut self,
        checks: &[ReplicationCheck],
    ) -> Result<(), ReadingVerificationRefusal> {
        self.verification = replication_tier(self.verification, checks)?;
        Ok(())
    }

    pub fn reveal_target(
        &self,
        id: &QualifiedEntityId,
    ) -> Result<RevealTarget, ReadingActionRefusal> {
        let entity = self.entity(id)?;
        Ok(RevealTarget {
            entity: EntityRefDto::Reading(entity.id.clone()),
            extent: entity.extent.clone(),
        })
    }

    pub fn audition_target(
        &self,
        id: &QualifiedEntityId,
    ) -> Result<AuditionTarget, ReadingActionRefusal> {
        if self.verification == VerificationTier::GraphOnly {
            return Err(ReadingActionRefusal::SourceUnavailable);
        }
        let entity = self.entity(id)?;
        Ok(AuditionTarget {
            entity: EntityRefDto::Reading(entity.id.clone()),
            extent: entity
                .extent
                .clone()
                .ok_or_else(|| ReadingActionRefusal::MissingExtent(entity.id.clone()))?,
        })
    }

    fn entity(
        &self,
        id: &QualifiedEntityId,
    ) -> Result<&PlannedReadingEntity, ReadingActionRefusal> {
        self.entities
            .iter()
            .find(|entity| &entity.id == id)
            .ok_or_else(|| ReadingActionRefusal::UnknownEntity(id.clone()))
    }
}

/// Verify and inventory a reading without mutating project truth. Exact
/// qualified duplicates may be skipped; numerically equal local IDs minted by
/// another reading never collide.
pub fn plan_reading_import(
    reading: &ReadingFile,
    local_source: Option<&LocalSourceDescriptor>,
    existing: &BTreeSet<QualifiedEntityId>,
    options: ReadingImportOptions,
) -> Result<ReadingImportPlan, ReadingImportRefusal> {
    reading
        .validate()
        .map_err(ReadingImportRefusal::InvalidReading)?;
    let verification = reading
        .verify_source(local_source)
        .map_err(ReadingImportRefusal::SourceVerification)?;

    let mut opaque_sections = reading
        .sections
        .iter()
        .filter(|section| section.name != ENTITY_SECTION_NAME)
        .map(|section| section.name.clone())
        .collect::<Vec<_>>();
    opaque_sections.sort();
    if options.unknown_sections == UnknownSectionPolicy::Refuse {
        if let Some(name) = opaque_sections.first() {
            return Err(ReadingImportRefusal::UnknownSection(name.clone()));
        }
    }

    let section = reading
        .sections
        .iter()
        .find(|section| section.name == ENTITY_SECTION_NAME);
    let records = match section {
        Some(_) => {
            decode_section::<PortableEntitySection>(
                reading,
                ENTITY_SECTION_NAME,
                ENTITY_SECTION_MAJOR,
            )
            .map_err(ReadingImportRefusal::Codec)?
            .value
            .entities
        }
        None if options.require_entity_section => {
            return Err(ReadingImportRefusal::MissingSection(
                ENTITY_SECTION_NAME.into(),
            ))
        }
        None => Vec::new(),
    };

    let mut seen = BTreeSet::new();
    let mut entities = Vec::with_capacity(records.len());
    for record in records {
        let id = QualifiedEntityId::new(reading.reading_id, record.kind, record.local_id)
            .map_err(ReadingImportRefusal::InvalidReading)?;
        if !seen.insert(id.clone()) {
            return Err(ReadingImportRefusal::DuplicateEntity(id));
        }
        if record.label.trim().is_empty() {
            return Err(ReadingImportRefusal::BlankEntityLabel(id));
        }
        if let Some(hypothesis) = &record.hypothesis {
            if record.role != PortableEntityRole::Hypothesis
                || !hypothesis.support.is_finite()
                || !(0.0..=1.0).contains(&hypothesis.support)
                || hypothesis
                    .description
                    .as_ref()
                    .is_some_and(|description| description.trim().is_empty())
            {
                return Err(ReadingImportRefusal::InvalidHypothesis(id));
            }
        }
        let extent = record
            .extent
            .map(|extent| qualify_geometry(extent, reading.reading_id))
            .transpose()?;
        let disposition = if existing.contains(&id) {
            ImportDisposition::AlreadyPresent
        } else if record.role == PortableEntityRole::Hypothesis {
            ImportDisposition::PreserveCoexistingHypothesis
        } else {
            ImportDisposition::AddQualified
        };
        entities.push(PlannedReadingEntity {
            id,
            label: record.label,
            role: record.role,
            hypothesis: record.hypothesis,
            hypothesis_group: record.hypothesis_group,
            extent,
            disposition,
        });
    }
    entities.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(ReadingImportPlan {
        reading_id: reading.reading_id,
        reading_revision: reading.revision,
        verification,
        entities,
        opaque_sections,
        preserved_sections: reading
            .sections
            .iter()
            .filter(|section| section.name != ENTITY_SECTION_NAME)
            .cloned()
            .collect(),
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct CoexistingHypothesisGroup {
    pub key: String,
    pub alternatives: Vec<QualifiedEntityId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReadingMergePlan {
    pub readings: Vec<(ReadingId, u64, VerificationTier)>,
    pub entities: Vec<PlannedReadingEntity>,
    pub hypothesis_groups: Vec<CoexistingHypothesisGroup>,
    pub preserved_sections: BTreeMap<(ReadingId, u64), Vec<ReadingSection>>,
}

/// Merge inventories without electing a winner. Hypotheses sharing a semantic
/// group become explicit alternatives and remain qualified by their reading.
pub fn merge_as_coexisting_hypotheses(
    plans: &[ReadingImportPlan],
) -> Result<ReadingMergePlan, ReadingMergeRefusal> {
    let mut readings = Vec::new();
    let mut entities = BTreeMap::<QualifiedEntityId, PlannedReadingEntity>::new();
    let mut groups = BTreeMap::<String, Vec<QualifiedEntityId>>::new();
    let mut preserved_sections = BTreeMap::new();
    for plan in plans {
        if readings
            .iter()
            .any(|(id, revision, _)| *id == plan.reading_id && *revision == plan.reading_revision)
        {
            return Err(ReadingMergeRefusal::DuplicateReadingVersion {
                reading: plan.reading_id,
                revision: plan.reading_revision,
            });
        }
        readings.push((plan.reading_id, plan.reading_revision, plan.verification));
        preserved_sections.insert(
            (plan.reading_id, plan.reading_revision),
            plan.preserved_sections.clone(),
        );
        for entity in &plan.entities {
            if let Some(existing) = entities.get(&entity.id) {
                if existing != entity {
                    return Err(ReadingMergeRefusal::ConflictingQualifiedEntity(
                        entity.id.clone(),
                    ));
                }
                continue;
            }
            entities.insert(entity.id.clone(), entity.clone());
            if entity.role == PortableEntityRole::Hypothesis {
                let key = entity.hypothesis_group.clone().unwrap_or_else(|| {
                    format!(
                        "ungrouped:{}:{}:{}",
                        entity.id.reading, entity.id.kind, entity.id.local_id
                    )
                });
                groups.entry(key).or_default().push(entity.id.clone());
            }
        }
    }
    readings.sort_by_key(|(id, revision, _)| (*id, *revision));
    let hypothesis_groups = groups
        .into_iter()
        .map(|(key, mut alternatives)| {
            alternatives.sort();
            alternatives.dedup();
            CoexistingHypothesisGroup { key, alternatives }
        })
        .collect();
    Ok(ReadingMergePlan {
        readings,
        entities: entities.into_values().collect(),
        hypothesis_groups,
        preserved_sections,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct ForeignHypothesisMapping {
    pub foreign: QualifiedEntityId,
    pub project: ontology::HypothesisId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UndoableForeignImport {
    pub mappings: Vec<ForeignHypothesisMapping>,
    pub envelope: CommandEnvelope,
}

/// Lower a semantic merge to exactly one aggregate edit. Allocation remains a
/// caller responsibility and is explicit, while the command retains both the
/// new AIR claim and the reading-qualified foreign claim for durable replay.
pub fn lower_foreign_hypothesis_import(
    merge: &ReadingMergePlan,
    base_revision: u64,
    hypothesis_allocations: &BTreeMap<QualifiedEntityId, ontology::HypothesisId>,
    set_allocations: &BTreeMap<String, ontology::HypothesisSetId>,
) -> Result<UndoableForeignImport, ForeignImportRefusal> {
    let pending = merge
        .entities
        .iter()
        .filter(|entity| entity.disposition != ImportDisposition::AlreadyPresent)
        .collect::<Vec<_>>();
    if let Some(entity) = pending
        .iter()
        .find(|entity| entity.role != PortableEntityRole::Hypothesis)
    {
        return Err(ForeignImportRefusal::UnsupportedEntity(entity.id.clone()));
    }
    let expected = pending
        .iter()
        .map(|entity| entity.id.clone())
        .collect::<BTreeSet<_>>();
    if hypothesis_allocations
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>()
        != expected
    {
        return Err(ForeignImportRefusal::AllocationSetMismatch);
    }
    let project_ids = hypothesis_allocations
        .values()
        .copied()
        .collect::<BTreeSet<_>>();
    if project_ids.len() != hypothesis_allocations.len()
        || project_ids.iter().any(|id| id.get() == 0)
    {
        return Err(ForeignImportRefusal::DuplicateOrZeroAllocation);
    }

    let mut commands = Vec::new();
    let mut mappings = Vec::new();
    for entity in pending {
        let project = hypothesis_allocations[&entity.id];
        let reading_revision = merge
            .readings
            .iter()
            .find(|(id, _, _)| *id == entity.id.reading)
            .map(|(_, revision, _)| *revision)
            .ok_or_else(|| ForeignImportRefusal::MissingReadingVersion(entity.id.reading))?;
        let semantics = entity
            .hypothesis
            .clone()
            .unwrap_or(PortableHypothesisSemantics {
                support: 0.0,
                description: None,
            });
        let claims = semantics
            .description
            .into_iter()
            .map(
                |description| ontology::HypothesisClaim::FreeformPerceptualDescription {
                    objects: Vec::new(),
                    description,
                },
            )
            .collect();
        commands.push(DomainCommand::Air(AirCommand::PutHypothesis {
            before: None,
            after: Some(ontology::Hypothesis {
                id: project,
                label: entity.label.clone(),
                claims,
                support: semantics.support,
                evidence: Vec::new(),
                provenance: ontology::Provenance {
                    producer: ontology::Producer::Importer {
                        format: crate::reading::READING_FORMAT.into(),
                        version: crate::reading::READING_FORMAT_VERSION.to_string(),
                    },
                    created_unix_ms: None,
                    source_revision: Some(format!(
                        "reading:{}:{}",
                        entity.id.reading, reading_revision
                    )),
                    note: Some(format!(
                        "foreign:{}:{}:{}",
                        entity.id.reading, entity.id.kind, entity.id.local_id
                    )),
                },
            }),
        }));
        mappings.push(ForeignHypothesisMapping {
            foreign: entity.id.clone(),
            project,
        });
    }

    for group in &merge.hypothesis_groups {
        let alternatives = group
            .alternatives
            .iter()
            .filter_map(|id| hypothesis_allocations.get(id).copied())
            .collect::<Vec<_>>();
        if alternatives.len() < 2 {
            continue;
        }
        let Some(id) = set_allocations.get(&group.key).copied() else {
            return Err(ForeignImportRefusal::MissingSetAllocation(
                group.key.clone(),
            ));
        };
        if id.get() == 0 {
            return Err(ForeignImportRefusal::DuplicateOrZeroAllocation);
        }
        commands.push(DomainCommand::Air(AirCommand::PutHypothesisSet {
            before: None,
            after: Some(ontology::HypothesisSet {
                id,
                question: group.key.clone(),
                alternatives,
                selection: ontology::HypothesisSelection::Unresolved,
            }),
        }));
    }
    let expected_set_keys = merge
        .hypothesis_groups
        .iter()
        .filter(|group| {
            group
                .alternatives
                .iter()
                .filter(|id| hypothesis_allocations.contains_key(*id))
                .count()
                >= 2
        })
        .map(|group| group.key.clone())
        .collect::<BTreeSet<_>>();
    if set_allocations.keys().cloned().collect::<BTreeSet<_>>() != expected_set_keys {
        return Err(ForeignImportRefusal::AllocationSetMismatch);
    }
    let allocated_sets = set_allocations.values().copied().collect::<BTreeSet<_>>();
    if allocated_sets.len() != set_allocations.len()
        || allocated_sets.iter().any(|id| id.get() == 0)
    {
        return Err(ForeignImportRefusal::DuplicateOrZeroAllocation);
    }
    if commands.is_empty() {
        return Err(ForeignImportRefusal::NothingToImport);
    }
    let mut id_claims = claims_for_commands(&commands);
    for mapping in &mappings {
        id_claims.insert(IdClaim::Foreign {
            reading: u128::from_be_bytes(mapping.foreign.reading.bytes()),
            kind: ForeignEntityKind::Air(AirEntityKind::Hypothesis),
            local: mapping.foreign.local_id,
        });
    }
    mappings.sort_by(|left, right| left.foreign.cmp(&right.foreign));
    Ok(UndoableForeignImport {
        mappings,
        envelope: CommandEnvelope {
            label: format!("Import {} reading hypotheses", hypothesis_allocations.len()),
            base_revision,
            coalesce: None,
            commands,
            id_claims,
        },
    })
}

fn qualify_geometry(
    geometry: AspectGeometryDto,
    reading: ReadingId,
) -> Result<AspectGeometryDto, ReadingImportRefusal> {
    validate_geometry(&geometry).map_err(ReadingImportRefusal::Workbench)?;
    for reference in geometry.objects.iter().chain(match &geometry.signal {
        SignalLayerDto::Source => None,
        SignalLayerDto::Explanation { reference } | SignalLayerDto::Residual { reference } => {
            Some(reference)
        }
    }) {
        if let EntityRefDto::Reading(id) = reference {
            if id.reading != reading {
                return Err(ReadingImportRefusal::ForeignReadingIdentity(id.clone()));
            }
        }
    }
    geometry
        .qualify_project_ids(reading)
        .map_err(|error| ReadingImportRefusal::InvalidGeometry(format!("{error:?}")))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionChangeKind {
    Added,
    Removed,
    Modified,
    Unchanged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadingSectionChange {
    pub name: String,
    pub kind: SectionChangeKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadingEntityChange {
    pub id: QualifiedEntityId,
    pub kind: SectionChangeKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadingDiff {
    pub left: (ReadingId, u64),
    pub right: (ReadingId, u64),
    pub source_changed: bool,
    pub sections: Vec<ReadingSectionChange>,
    pub entities: Vec<ReadingEntityChange>,
}

pub fn diff_readings(
    left: &ReadingFile,
    right: &ReadingFile,
) -> Result<ReadingDiff, ReadingImportRefusal> {
    left.validate()
        .map_err(ReadingImportRefusal::InvalidReading)?;
    right
        .validate()
        .map_err(ReadingImportRefusal::InvalidReading)?;
    let left_sections = left
        .sections
        .iter()
        .map(|section| (section.name.clone(), section))
        .collect::<BTreeMap<_, _>>();
    let right_sections = right
        .sections
        .iter()
        .map(|section| (section.name.clone(), section))
        .collect::<BTreeMap<_, _>>();
    let section_names = left_sections
        .keys()
        .chain(right_sections.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let sections = section_names
        .into_iter()
        .map(|name| {
            let kind = match (left_sections.get(&name), right_sections.get(&name)) {
                (None, Some(_)) => SectionChangeKind::Added,
                (Some(_), None) => SectionChangeKind::Removed,
                (Some(left), Some(right)) if left == right => SectionChangeKind::Unchanged,
                (Some(_), Some(_)) => SectionChangeKind::Modified,
                (None, None) => unreachable!("name came from one side"),
            };
            ReadingSectionChange { name, kind }
        })
        .collect();

    let left_entities = entity_inventory(left)?;
    let right_entities = entity_inventory(right)?;
    let ids = left_entities
        .keys()
        .chain(right_entities.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let entities = ids
        .into_iter()
        .map(|id| {
            let kind = match (left_entities.get(&id), right_entities.get(&id)) {
                (None, Some(_)) => SectionChangeKind::Added,
                (Some(_), None) => SectionChangeKind::Removed,
                (Some(left), Some(right)) if left == right => SectionChangeKind::Unchanged,
                (Some(_), Some(_)) => SectionChangeKind::Modified,
                (None, None) => unreachable!("id came from one side"),
            };
            ReadingEntityChange { id, kind }
        })
        .collect();
    Ok(ReadingDiff {
        left: (left.reading_id, left.revision),
        right: (right.reading_id, right.revision),
        source_changed: left.source != right.source,
        sections,
        entities,
    })
}

fn entity_inventory(
    reading: &ReadingFile,
) -> Result<BTreeMap<QualifiedEntityId, PortableEntityRecord>, ReadingImportRefusal> {
    let Some(_) = reading
        .sections
        .iter()
        .find(|section| section.name == ENTITY_SECTION_NAME)
    else {
        return Ok(BTreeMap::new());
    };
    let entities =
        decode_section::<PortableEntitySection>(reading, ENTITY_SECTION_NAME, ENTITY_SECTION_MAJOR)
            .map_err(ReadingImportRefusal::Codec)?
            .value
            .entities;
    let mut inventory = BTreeMap::new();
    for record in entities {
        let id = QualifiedEntityId::new(reading.reading_id, record.kind.clone(), record.local_id)
            .map_err(ReadingImportRefusal::InvalidReading)?;
        if inventory.insert(id.clone(), record).is_some() {
            return Err(ReadingImportRefusal::DuplicateEntity(id));
        }
    }
    Ok(inventory)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkbenchActionRefusal {
    NoSelection,
    UnknownRow(usize),
    MissingExtent(EntityRefDto),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadingActionRefusal {
    SourceUnavailable,
    UnknownEntity(QualifiedEntityId),
    MissingExtent(QualifiedEntityId),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ReadingImportRefusal {
    InvalidReading(ReadingError),
    SourceVerification(ReadingVerificationRefusal),
    MissingSection(String),
    UnknownSection(String),
    Codec(ReadingCodecError),
    DuplicateEntity(QualifiedEntityId),
    BlankEntityLabel(QualifiedEntityId),
    InvalidHypothesis(QualifiedEntityId),
    ForeignReadingIdentity(QualifiedEntityId),
    InvalidGeometry(String),
    Workbench(WorkbenchError),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ReadingMergeRefusal {
    DuplicateReadingVersion { reading: ReadingId, revision: u64 },
    ConflictingQualifiedEntity(QualifiedEntityId),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ForeignImportRefusal {
    UnsupportedEntity(QualifiedEntityId),
    AllocationSetMismatch,
    DuplicateOrZeroAllocation,
    MissingSetAllocation(String),
    MissingReadingVersion(ReadingId),
    NothingToImport,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WorkbenchError {
    Json(String),
    WrongDocumentFormat(String),
    UnsupportedDocumentVersion(u32),
    InvalidDocument(String),
    InvalidQuery(String),
    InvalidResult(String),
    InvalidExecution(String),
    InvalidPage(String),
    StaleCursor,
    DuplicatePage { address: PortableDigest, start: u64 },
    InvalidEntity(EntityRefDto),
    InvalidRegion(RegionDto),
    QualifiedReferenceInProjectQuery(EntityRefDto),
    Query(QueryError),
}

impl fmt::Display for WorkbenchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "reading/query workbench error: {self:?}")
    }
}

impl std::error::Error for WorkbenchError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aspect::{ConcreteAspect, ConcreteRegion, SignalLayer};
    use crate::coverage::{CoverageRecipe, CoverageSummary};
    use crate::ontology::{HypothesisId, ObjectId, ParameterId, SourceId};
    use crate::reading::{
        PortableDigest, PortableDigestAlgorithm, ProducerDto, ProvenanceDto, ReadingSection,
        ReadingSource, READING_FORMAT, READING_FORMAT_VERSION,
    };
    use serde_json::json;

    fn digest(value: u8) -> PortableDigest {
        PortableDigest {
            algorithm: PortableDigestAlgorithm::Sha256,
            bytes: [value; 32],
        }
    }

    fn region(start: i64, end: i64) -> ConcreteAspect {
        ConcreteAspect::new(
            vec![ConcreteRegion {
                time: FrameSpan::new(start, end).unwrap(),
                band: BandSpan::new(0.0, 24_000.0).unwrap(),
                channels: ChannelMask(1),
            }],
            SignalLayer::Source,
        )
        .unwrap()
    }

    struct Fixture {
        extents: BTreeMap<FactRef, ConcreteAspect>,
        evidence: BTreeMap<FactRef, Vec<FactRef>>,
        related: BTreeMap<FactRef, Vec<FactRef>>,
        residual: ConcreteAspect,
    }

    impl AirFacts for Fixture {
        fn facts(&self, kind: FactKind) -> Vec<FactRef> {
            let mut facts = self
                .extents
                .keys()
                .copied()
                .filter(|fact| {
                    matches!(
                        (kind, fact),
                        (FactKind::Object, FactRef::Object(_))
                            | (FactKind::Source, FactRef::Source(_))
                            | (FactKind::Parameter, FactRef::Parameter(_))
                            | (FactKind::Hypothesis, FactRef::Hypothesis(_))
                    )
                })
                .collect::<Vec<_>>();
            facts.reverse();
            facts
        }

        fn evidence_of(&self, fact: FactRef) -> Vec<FactRef> {
            self.evidence.get(&fact).cloned().unwrap_or_default()
        }

        fn related(&self, fact: FactRef) -> Vec<FactRef> {
            self.related.get(&fact).cloned().unwrap_or_default()
        }

        fn extent(&self, fact: FactRef) -> Option<ConcreteAspect> {
            self.extents.get(&fact).cloned()
        }
    }

    impl AspectResolver for Fixture {
        fn universe(&self) -> ConcreteAspect {
            region(0, 100)
        }

        fn family_spans(
            &self,
            _analysis: &crate::aspect::AnalysisRef,
            _id: usize,
        ) -> Option<Vec<FrameSpan>> {
            None
        }

        fn object_extent(&self, object: ObjectId) -> Option<ConcreteAspect> {
            self.extents.get(&FactRef::Object(object)).cloned()
        }

        fn explanation_extent(&self, _reference: &ExplanationRef) -> Option<ConcreteAspect> {
            Some(self.residual.clone())
        }
    }

    fn fixture() -> Fixture {
        let object_1 = FactRef::Object(ObjectId::new(1));
        let object_2 = FactRef::Object(ObjectId::new(2));
        let hypothesis = FactRef::Hypothesis(HypothesisId::new(9));
        Fixture {
            extents: BTreeMap::from([
                (object_1, region(0, 20)),
                (object_2, region(60, 80)),
                (hypothesis, region(0, 80)),
            ]),
            evidence: BTreeMap::from([(object_1, vec![hypothesis])]),
            related: BTreeMap::from([(hypothesis, vec![object_1])]),
            residual: region(50, 100),
        }
    }

    fn geometry(start: i64, end: i64) -> AspectGeometryDto {
        AspectGeometryDto {
            regions: vec![RegionDto {
                start_frame: start,
                end_frame: end,
                min_hz_bits: 0.0_f32.to_bits(),
                max_hz_bits: 24_000.0_f32.to_bits(),
                channels: 1,
            }],
            objects: Vec::new(),
            signal: SignalLayerDto::Source,
        }
    }

    #[test]
    fn query_document_executes_deterministically_and_persists_derivation_provenance() {
        let fixture = fixture();
        let mut document = QueryDocument::new(
            QueryDocumentId(4),
            "early objects",
            QueryTermDto::And {
                terms: vec![
                    QueryTermDto::Kind {
                        kind: FactKindDto::Object,
                    },
                    QueryTermDto::Within {
                        aspect: geometry(0, 30),
                    },
                ],
            },
        );
        let snapshot = execute_query_document(
            &mut document,
            &fixture,
            &fixture,
            QueryExecutionProvenance {
                fact_base_revision: 7,
                fact_base_digest: digest(7),
                source_revision: Some("project:abc".into()),
                executed_unix_ms: None,
            },
        )
        .unwrap();
        assert_eq!(snapshot.page.hits.len(), 1);
        assert_eq!(snapshot.page.hits[0].derivation.rule, "and");
        assert!(snapshot.page.hits[0]
            .derivation
            .premises
            .contains(&EntityRefDto::Project {
                kind: "air-hypothesis".into(),
                local_id: 9,
            }));

        let decoded = QueryDocument::from_json(&document.to_json().unwrap()).unwrap();
        assert_eq!(decoded, document);
        let descriptor = WorkbenchPaneFactory::workspace_view(&decoded).unwrap();
        assert!(matches!(
            descriptor.kind,
            WorkspaceItemKind::Extension { .. }
        ));
        let mut model = WorkbenchPaneFactory::model(decoded).unwrap();
        model.select_row(0).unwrap();
        assert!(model.audition_target().is_ok());
        assert_eq!(
            model.reveal_target().unwrap().entity,
            EntityRefDto::Project {
                kind: "air-object".into(),
                local_id: 1,
            }
        );
    }

    #[test]
    fn content_addressed_pages_bind_cursors_and_cancel_atomically() {
        let fixture = fixture();
        let mut document = QueryDocument::new(
            QueryDocumentId(41),
            "objects",
            QueryTermDto::Kind {
                kind: FactKindDto::Object,
            },
        );
        let provenance = QueryExecutionProvenance {
            fact_base_revision: 3,
            fact_base_digest: digest(3),
            source_revision: Some("air:3".into()),
            executed_unix_ms: None,
        };
        let first = execute_query_page(
            &mut document,
            &fixture,
            &fixture,
            provenance.clone(),
            QueryPageRequest {
                limit: 1,
                cursor: None,
            },
            &NeverCancel,
        )
        .unwrap();
        let address = first.content_address;
        let cursor = first.page.next_cursor.clone().unwrap();
        assert_eq!(first.page.hits.len(), 1);
        let second = execute_query_page(
            &mut document,
            &fixture,
            &fixture,
            provenance.clone(),
            QueryPageRequest {
                limit: 1,
                cursor: Some(cursor.clone()),
            },
            &NeverCancel,
        )
        .unwrap();
        assert_eq!(second.content_address, address);
        assert_eq!(second.page_start, 1);
        assert!(second.page.next_cursor.is_none());

        let mut stale = QueryDocument::new(
            QueryDocumentId(42),
            "objects",
            QueryTermDto::Kind {
                kind: FactKindDto::Object,
            },
        );
        assert!(matches!(
            execute_query_page(
                &mut stale,
                &fixture,
                &fixture,
                QueryExecutionProvenance {
                    fact_base_digest: digest(4),
                    ..provenance.clone()
                },
                QueryPageRequest {
                    limit: 1,
                    cursor: Some(cursor),
                },
                &NeverCancel,
            ),
            Err(WorkbenchError::StaleCursor)
        ));

        let cancelled = QueryCancellationToken::default();
        cancelled.cancel();
        let mut cancelled_document = QueryDocument::new(
            QueryDocumentId(43),
            "objects",
            QueryTermDto::Kind {
                kind: FactKindDto::Object,
            },
        );
        assert!(matches!(
            execute_query_page(
                &mut cancelled_document,
                &fixture,
                &fixture,
                provenance,
                QueryPageRequest::default(),
                &cancelled,
            ),
            Err(WorkbenchError::Query(QueryError::Cancelled))
        ));
        assert!(cancelled_document.results.is_empty());
    }

    #[test]
    fn residual_query_and_missing_extent_have_typed_refusal_paths() {
        let fixture = fixture();
        let mut document = QueryDocument::new(
            QueryDocumentId(5),
            "residual",
            QueryTermDto::NotExplainedBy { proposal_id: 3 },
        );
        execute_query_document(
            &mut document,
            &fixture,
            &fixture,
            QueryExecutionProvenance {
                fact_base_revision: 8,
                fact_base_digest: digest(8),
                source_revision: None,
                executed_unix_ms: None,
            },
        )
        .unwrap();
        assert_eq!(
            document
                .latest_result()
                .unwrap()
                .page
                .hits
                .iter()
                .map(|hit| hit.fact.clone())
                .collect::<Vec<_>>(),
            vec![
                EntityRefDto::Project {
                    kind: "air-object".into(),
                    local_id: 2,
                },
                EntityRefDto::Project {
                    kind: "air-hypothesis".into(),
                    local_id: 9,
                },
            ]
        );

        let mut document = document;
        document.results.last_mut().unwrap().page.hits[0].extent = None;
        let mut model = WorkbenchPaneFactory::model(document).unwrap();
        model.select_row(0).unwrap();
        assert!(matches!(
            model.audition_target(),
            Err(WorkbenchActionRefusal::MissingExtent(_))
        ));
    }

    #[test]
    fn residual_guide_ranks_hotspots_and_keeps_residual_signal_explicit() {
        let field = CoverageField {
            origin_frame: 0,
            sample_rate: 48_000,
            channels: 1,
            frame_count: 4,
            recipe: CoverageRecipe {
                fft_size: 2,
                hop_size: 2,
                power_floor: 0.01,
            },
            columns: 2,
            bins: 2,
            source_power: vec![1.0; 4],
            construction_power: vec![0.0; 4],
            residual_power: vec![0.2, 0.9, 0.8, 0.1],
            explained: vec![0.8, 0.1, 0.2, 0.9],
            excess: vec![0.0; 4],
            summary: CoverageSummary::default(),
        };
        let guide = residual_guide(QueryDocumentId(8), "gaps", &field, 11, 12, 2).unwrap();
        assert_eq!(guide.auditions.len(), 2);
        assert_eq!(guide.auditions[0].extent.regions[0].start_frame, 0);
        assert!(matches!(
            guide.auditions[0].extent.signal,
            SignalLayerDto::Residual { .. }
        ));
        assert!(matches!(
            guide.query_document.query,
            QueryTermDto::And { .. }
        ));
    }

    fn reading(id_byte: u8, sections: Vec<ReadingSection>) -> ReadingFile {
        ReadingFile {
            format: READING_FORMAT.into(),
            version: READING_FORMAT_VERSION,
            reading_id: ReadingId::new([id_byte; 16]).unwrap(),
            revision: 1,
            parents: Vec::new(),
            author: ProvenanceDto {
                producer: ProducerDto::Human { name: None },
                created_unix_ms: None,
                source_revision: None,
                note: None,
            },
            source: ReadingSource {
                fingerprints: vec![digest(4)],
                sample_rate: 48_000,
                channels: 1,
                frame_count: 100,
                declared_title: None,
                extensions: BTreeMap::new(),
            },
            sections,
            attachments: Vec::new(),
            extensions: BTreeMap::new(),
        }
    }

    fn entity_section(label: &str) -> ReadingSection {
        ReadingSection {
            name: ENTITY_SECTION_NAME.into(),
            schema_major: ENTITY_SECTION_MAJOR,
            schema_minor: 0,
            payload: serde_json::to_value(PortableEntitySection {
                entities: vec![PortableEntityRecord {
                    kind: "hypothesis".into(),
                    local_id: 1,
                    label: label.into(),
                    role: PortableEntityRole::Hypothesis,
                    hypothesis: Some(PortableHypothesisSemantics {
                        support: 0.5,
                        description: Some(label.into()),
                    }),
                    hypothesis_group: Some("source-model".into()),
                    extent: Some(geometry(0, 20)),
                    extensions: BTreeMap::new(),
                }],
                extensions: BTreeMap::new(),
            })
            .unwrap(),
            extensions: BTreeMap::new(),
        }
    }

    #[test]
    fn missing_source_keeps_graph_import_but_refuses_audition() {
        let reading = reading(1, vec![entity_section("hypothesis A")]);
        let plan = plan_reading_import(
            &reading,
            None,
            &BTreeSet::new(),
            ReadingImportOptions::default(),
        )
        .unwrap();
        assert_eq!(plan.verification, VerificationTier::GraphOnly);
        assert_eq!(
            plan.entities[0].disposition,
            ImportDisposition::PreserveCoexistingHypothesis
        );
        assert!(matches!(
            plan.audition_target(&plan.entities[0].id),
            Err(ReadingActionRefusal::SourceUnavailable)
        ));
        assert!(matches!(
            plan.reveal_target(&plan.entities[0].id).unwrap().entity,
            EntityRefDto::Reading(_)
        ));
    }

    #[test]
    fn missing_and_unknown_sections_are_explicit_and_unknowns_can_be_preserved() {
        let missing = reading(2, Vec::new());
        assert!(matches!(
            plan_reading_import(
                &missing,
                None,
                &BTreeSet::new(),
                ReadingImportOptions::default(),
            ),
            Err(ReadingImportRefusal::MissingSection(name)) if name == ENTITY_SECTION_NAME
        ));

        let unknown = ReadingSection {
            name: "future-claims".into(),
            schema_major: 1,
            schema_minor: 0,
            payload: json!({"opaque": true}),
            extensions: BTreeMap::new(),
        };
        let reading = reading(3, vec![entity_section("h"), unknown]);
        assert!(matches!(
            plan_reading_import(
                &reading,
                None,
                &BTreeSet::new(),
                ReadingImportOptions {
                    unknown_sections: UnknownSectionPolicy::Refuse,
                    require_entity_section: true,
                },
            ),
            Err(ReadingImportRefusal::UnknownSection(name)) if name == "future-claims"
        ));
        let plan = plan_reading_import(
            &reading,
            None,
            &BTreeSet::new(),
            ReadingImportOptions::default(),
        )
        .unwrap();
        assert_eq!(plan.opaque_sections, vec!["future-claims"]);
        assert_eq!(plan.preserved_sections[0].payload["opaque"], true);
    }

    #[test]
    fn import_and_diff_never_collapse_equal_local_hypothesis_ids_across_readings() {
        let left = reading(7, vec![entity_section("A")]);
        let right = reading(8, vec![entity_section("B")]);
        let left_plan = plan_reading_import(
            &left,
            None,
            &BTreeSet::new(),
            ReadingImportOptions::default(),
        )
        .unwrap();
        let existing = BTreeSet::from([left_plan.entities[0].id.clone()]);
        let right_plan =
            plan_reading_import(&right, None, &existing, ReadingImportOptions::default()).unwrap();
        assert_ne!(left_plan.entities[0].id, right_plan.entities[0].id);
        assert_eq!(
            right_plan.entities[0].disposition,
            ImportDisposition::PreserveCoexistingHypothesis
        );

        let diff = diff_readings(&left, &right).unwrap();
        assert_eq!(diff.entities.len(), 2);
        assert!(diff
            .entities
            .iter()
            .any(|change| change.kind == SectionChangeKind::Removed));
        assert!(diff
            .entities
            .iter()
            .any(|change| change.kind == SectionChangeKind::Added));
    }

    #[test]
    fn semantic_merge_lowers_to_one_undoable_foreign_id_envelope() {
        use crate::daw_project::DawProject;

        let left = reading(10, vec![entity_section("alternative A")]);
        let right = reading(11, vec![entity_section("alternative B")]);
        let left_plan = plan_reading_import(
            &left,
            None,
            &BTreeSet::new(),
            ReadingImportOptions::default(),
        )
        .unwrap();
        let right_plan = plan_reading_import(
            &right,
            None,
            &BTreeSet::new(),
            ReadingImportOptions::default(),
        )
        .unwrap();
        let merge = merge_as_coexisting_hypotheses(&[left_plan, right_plan]).unwrap();
        assert_eq!(merge.hypothesis_groups.len(), 1);
        assert_eq!(merge.hypothesis_groups[0].alternatives.len(), 2);

        let hypothesis_allocations = BTreeMap::from([
            (merge.entities[0].id.clone(), HypothesisId::new(101)),
            (merge.entities[1].id.clone(), HypothesisId::new(102)),
        ]);
        let set_allocations = BTreeMap::from([(
            "source-model".into(),
            crate::ontology::HypothesisSetId::new(201),
        )]);
        let lowered =
            lower_foreign_hypothesis_import(&merge, 0, &hypothesis_allocations, &set_allocations)
                .unwrap();
        assert_eq!(lowered.mappings.len(), 2);
        assert_eq!(lowered.envelope.commands.len(), 3);
        assert_eq!(
            lowered
                .envelope
                .id_claims
                .iter()
                .filter(|claim| matches!(claim, IdClaim::Foreign { .. }))
                .count(),
            2
        );

        let mut project = DawProject::new("reading import", 48_000, 120.0).unwrap();
        let applied = lowered.envelope.apply(&mut project).unwrap();
        assert_eq!(project.state().domains.air.hypotheses.len(), 2);
        let set = project
            .state()
            .domains
            .air
            .hypothesis_sets
            .get(&crate::ontology::HypothesisSetId::new(201))
            .unwrap();
        assert_eq!(
            set.selection,
            crate::ontology::HypothesisSelection::Unresolved
        );
        applied.inverse.apply(&mut project).unwrap();
        assert!(project.state().domains.air.hypotheses.is_empty());
        assert!(project.state().domains.air.hypothesis_sets.is_empty());
    }

    #[test]
    fn source_mismatch_is_a_refusal_not_a_warning() {
        let reading = reading(9, vec![entity_section("h")]);
        let local = LocalSourceDescriptor {
            digest: digest(5),
            sample_rate: 48_000,
            channels: 1,
            frame_count: 100,
        };
        assert!(matches!(
            plan_reading_import(
                &reading,
                Some(&local),
                &BTreeSet::new(),
                ReadingImportOptions::default(),
            ),
            Err(ReadingImportRefusal::SourceVerification(
                ReadingVerificationRefusal::FingerprintMismatch { .. }
            ))
        ));
    }

    #[test]
    fn source_match_upgrades_only_after_explicit_replication_checks() {
        let reading = reading(12, vec![entity_section("replicable")]);
        let local = LocalSourceDescriptor {
            digest: digest(4),
            sample_rate: 48_000,
            channels: 1,
            frame_count: 100,
        };
        let mut plan = plan_reading_import(
            &reading,
            Some(&local),
            &BTreeSet::new(),
            ReadingImportOptions::default(),
        )
        .unwrap();
        assert_eq!(plan.verification, VerificationTier::SourceMatched);
        let subject = plan.entities[0].id.clone();
        plan.record_replication(&[ReplicationCheck {
            subject,
            matches: true,
        }])
        .unwrap();
        assert_eq!(plan.verification, VerificationTier::Replicated);
    }

    #[test]
    fn compile_rejects_reading_qualified_refs_in_a_project_query() {
        let reading_id = ReadingId::new([1; 16]).unwrap();
        let query = QueryTermDto::Within {
            aspect: AspectGeometryDto {
                regions: Vec::new(),
                objects: vec![EntityRefDto::Reading(
                    QualifiedEntityId::new(reading_id, "air-object", 1).unwrap(),
                )],
                signal: SignalLayerDto::Source,
            },
        };
        assert!(matches!(
            query.compile(),
            Err(WorkbenchError::QualifiedReferenceInProjectQuery(_))
        ));
    }

    #[allow(dead_code)]
    fn all_fact_variants_compile() {
        let _ = FactRef::Source(SourceId::new(1));
        let _ = FactRef::Parameter(ParameterId::new(1));
    }
}
