//! Session-owned live-analysis to deprojection-workspace bridge.
//!
//! Runtime PCM stays in [`ArtifactComparisonPayload`]. Candidate documents
//! retain only content identities, source claims, evidence, and immutable
//! semantic recipes. Every lookup rechecks document, publication, aggregate,
//! and selection pins before returning a promotion/comparison request.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::Arc;

use crate::arrangement::{AudioLoopMode, ClipContent};
use crate::artifact_catalog::comparison_hydration::{
    insert_artifact_comparison_payload, ArtifactComparisonPayload, ArtifactComparisonPin,
    ArtifactHydrationContext,
};
use crate::artifact_catalog::{
    sha256_content, ArtifactCatalog, ArtifactDescriptor, ArtifactId, ArtifactKind, ContentDigest,
    DigestAlgorithm,
};
use crate::artifact_promotion_bridge;
use crate::aspect::{Aspect, ChannelMask, FrameSpan};
use crate::assets::{AssetFrameRange, SampleFrames};
use crate::comparison::{ComparisonDefinition, ComparisonId, SourceCitation};
use crate::comparison_runtime::executor::ComparisonProductRecipe;
use crate::daw_project::ProjectRevisions;
use crate::daw_render::RenderCancellation;
use crate::deprojection_execution::promotion::{
    PromotionBindings, PromotionPlacement, ResolvedSourceAsset,
};
use crate::deprojection_program::{
    candidate_from_model_output, candidates_from_rhythm_explanations, DeprojectionCandidate,
    EvidenceRef, MaterialSpan, PublishedModelOutput, SourceClaim, StructuralScorePolicy,
};
use crate::explanation::{
    ExplanationDefinition, ExplanationEvidenceRef, ExplanationId, ExplanationScope,
    HpssComponentKind, RenderedExplanation,
};
use crate::hpss::HpssResult;
use crate::interpretation::{InterpretationCommand, InterpretationStore};
use crate::loom::SequenceSketch;
use crate::model_claim::ModelClaimBundle;
use crate::project_controller::ObjectRef;
use crate::project_session::{ProjectSession, ProjectSessionError};
use crate::rhythm::RhythmDeprojection;
use crate::rhythm_explanation::{explain_rhythm, ExplainBudget};
use crate::workspace_items::WorkspaceViewId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeprojectionCandidateDocumentId(pub u64);

pub type DeprojectionWorkspacePin = artifact_promotion_bridge::ArtifactPromotionWorkspacePin;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeprojectionCandidateFreshness {
    Current,
    Invalidated,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeprojectionCandidateDocumentSummary {
    pub id: DeprojectionCandidateDocumentId,
    pub artifact: ArtifactId,
    pub candidate: crate::deprojection_program::DeprojectionCandidateId,
    pub label: String,
    pub comparison: ComparisonId,
    pub explanation: ExplanationId,
    pub pin: DeprojectionWorkspacePin,
    pub freshness: DeprojectionCandidateFreshness,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeprojectionWorkspaceTarget {
    Object(ObjectRef),
    View(WorkspaceViewId),
}

/// Complete resolver result expected by an explanation-workbench pane. The
/// caller does not manufacture any field of the request.
#[derive(Clone, Debug)]
pub struct ResolvedDeprojectionWorkspaceRequest {
    pub document: DeprojectionCandidateDocumentId,
    pub descriptor: ArtifactDescriptor,
    pub payload: Arc<ArtifactComparisonPayload>,
    pub request: artifact_promotion_bridge::ArtifactPromotionComparisonRequest,
}

/// Actual live analysis products accepted by the bridge. HPSS/Loom accept a
/// planner-produced candidate because those analyses alone do not assert a
/// constructive cause. Rhythm and model products use their canonical adapters.
#[derive(Clone, Debug)]
pub enum LiveDeprojectionAnalysis {
    Rhythm {
        descriptor: ArtifactDescriptor,
        deprojection: RhythmDeprojection,
        budget: ExplainBudget,
        rendered: RenderedExplanation,
    },
    Hpss {
        descriptor: ArtifactDescriptor,
        result: HpssResult,
        component: HpssComponentKind,
        candidate: DeprojectionCandidate,
    },
    Loom {
        descriptor: ArtifactDescriptor,
        sketch: SequenceSketch,
        source_start_frame: u64,
        clusters: Vec<usize>,
        candidate: DeprojectionCandidate,
    },
    Model {
        descriptor: ArtifactDescriptor,
        claim: ModelClaimBundle,
        output_name: String,
        published: PublishedModelOutput,
        rendered: RenderedExplanation,
    },
}

impl LiveDeprojectionAnalysis {
    /// Lossless adapter for the retained output of `rhythm::analyze_*` and its
    /// exact timeline render. No reconstruction candidate is supplied by the
    /// caller: canonical rhythm candidates are derived inside the bridge.
    pub fn from_rhythm(
        descriptor: ArtifactDescriptor,
        deprojection: RhythmDeprojection,
        budget: ExplainBudget,
        rendered: RenderedExplanation,
    ) -> Self {
        Self::Rhythm {
            descriptor,
            deprojection,
            budget,
            rendered,
        }
    }

    /// Lossless HPSS adapter. The candidate remains explicit because HPSS
    /// component evidence alone does not establish a constructive cause.
    pub fn from_hpss(
        descriptor: ArtifactDescriptor,
        result: HpssResult,
        component: HpssComponentKind,
        candidate: DeprojectionCandidate,
    ) -> Self {
        Self::Hpss {
            descriptor,
            result,
            component,
            candidate,
        }
    }

    /// Lossless Loom adapter. The chosen cluster identities and source origin
    /// are retained exactly; the caller must provide a separately planned
    /// candidate rather than turning recurrence into source identity.
    pub fn from_loom(
        descriptor: ArtifactDescriptor,
        sketch: SequenceSketch,
        source_start_frame: u64,
        clusters: Vec<usize>,
        candidate: DeprojectionCandidate,
    ) -> Self {
        Self::Loom {
            descriptor,
            sketch,
            source_start_frame,
            clusters,
            candidate,
        }
    }

    /// Lossless verified-model adapter. `candidate_from_model_output` remains
    /// authoritative for distinguishing constructive outputs from evidence-only
    /// artifacts.
    pub fn from_model(
        descriptor: ArtifactDescriptor,
        claim: ModelClaimBundle,
        output_name: impl Into<String>,
        published: PublishedModelOutput,
        rendered: RenderedExplanation,
    ) -> Self {
        Self::Model {
            descriptor,
            claim,
            output_name: output_name.into(),
            published,
            rendered,
        }
    }
}

#[derive(Debug)]
pub enum DeprojectionWorkspaceBridgeError {
    Session(ProjectSessionError),
    Invalid(String),
    Analysis(String),
    Catalog(String),
    Interpretation(String),
    NoExecutableCandidate,
    UnknownObject(ObjectRef),
    UnknownView(WorkspaceViewId),
    Invalidated(DeprojectionCandidateDocumentId),
    SelectionDoesNotAddressArtifact {
        selected: FrameSpan,
        artifact: FrameSpan,
    },
}

impl fmt::Display for DeprojectionWorkspaceBridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(error) => error.fmt(formatter),
            Self::Invalid(detail)
            | Self::Analysis(detail)
            | Self::Catalog(detail)
            | Self::Interpretation(detail) => formatter.write_str(detail),
            Self::NoExecutableCandidate => {
                formatter.write_str("analysis produced no promotable deprojection candidate")
            }
            Self::UnknownObject(object) => write!(formatter, "unknown candidate object {object:?}"),
            Self::UnknownView(view) => write!(
                formatter,
                "workspace view {} has no selected candidate",
                view.0
            ),
            Self::Invalidated(document) => write!(
                formatter,
                "deprojection candidate document {} was invalidated by session state",
                document.0
            ),
            Self::SelectionDoesNotAddressArtifact { selected, artifact } => write!(
                formatter,
                "selected span {selected:?} does not equal live artifact span {artifact:?}"
            ),
        }
    }
}

impl std::error::Error for DeprojectionWorkspaceBridgeError {}

impl From<ProjectSessionError> for DeprojectionWorkspaceBridgeError {
    fn from(value: ProjectSessionError) -> Self {
        Self::Session(value)
    }
}

#[derive(Clone, Debug)]
struct CandidateDocument {
    id: DeprojectionCandidateDocumentId,
    descriptor: ArtifactDescriptor,
    payload: Arc<ArtifactComparisonPayload>,
    candidate: DeprojectionCandidate,
    bindings: PromotionBindings,
    placement: PromotionPlacement,
    target: artifact_promotion_bridge::ArtifactPromotionComparisonTarget,
    recipe: ComparisonProductRecipe,
    pin: DeprojectionWorkspacePin,
}

impl CandidateDocument {
    fn summary(
        &self,
        freshness: DeprojectionCandidateFreshness,
    ) -> DeprojectionCandidateDocumentSummary {
        DeprojectionCandidateDocumentSummary {
            id: self.id,
            artifact: self.descriptor.id,
            candidate: self.candidate.id,
            label: self.candidate.label.clone(),
            comparison: self.target.comparison,
            explanation: self.target.explanation,
            pin: self.pin,
            freshness,
        }
    }
}

pub struct DeprojectionWorkspaceBridge {
    documents: BTreeMap<DeprojectionCandidateDocumentId, CandidateDocument>,
    objects: HashMap<ObjectRef, DeprojectionCandidateDocumentId>,
    selected_views: BTreeMap<WorkspaceViewId, DeprojectionCandidateDocumentId>,
    catalog: ArtifactCatalog,
    interpretations: InterpretationStore,
    next_document: u64,
    catalog_generation: u64,
}

impl fmt::Debug for DeprojectionWorkspaceBridge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeprojectionWorkspaceBridge")
            .field("documents", &self.documents.keys().collect::<Vec<_>>())
            .field("catalog", &self.catalog)
            .field("catalog_generation", &self.catalog_generation)
            .finish()
    }
}

impl DeprojectionWorkspaceBridge {
    pub fn new() -> Self {
        Self {
            documents: BTreeMap::new(),
            objects: HashMap::new(),
            selected_views: BTreeMap::new(),
            catalog: ArtifactCatalog::new(),
            interpretations: InterpretationStore::new(),
            next_document: 1,
            catalog_generation: 0,
        }
    }

    pub fn clear(&mut self) {
        *self = Self::new();
    }

    pub fn catalog(&self) -> &ArtifactCatalog {
        &self.catalog
    }

    pub fn interpretations(&self) -> &InterpretationStore {
        &self.interpretations
    }
}

impl Default for DeprojectionWorkspaceBridge {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
struct SessionContext {
    document_generation: u64,
    publication_generation: u64,
    revisions: ProjectRevisions,
    selection_revision: u64,
    selection_time: Option<FrameSpan>,
    selected_object: Option<ObjectRef>,
    snapshot: crate::live_project::LiveProjectSnapshot,
    primary_asset: crate::assets::AssetId,
    primary_clip: crate::arrangement::ClipId,
}

impl SessionContext {
    fn capture(session: &ProjectSession) -> Result<Self, DeprojectionWorkspaceBridgeError> {
        let live = session
            .live_project()
            .ok_or(ProjectSessionError::NoProject)?;
        let source = live.primary_source_ids().ok_or_else(|| {
            DeprojectionWorkspaceBridgeError::Invalid(
                "project has no primary source material".into(),
            )
        })?;
        Ok(Self {
            document_generation: session.document_generation(),
            publication_generation: session.snapshot().generation,
            revisions: session.project_snapshot()?.revisions(),
            selection_revision: session.selection().revision,
            selection_time: session.selection().selection.time,
            selected_object: session
                .selection()
                .selection
                .objects
                .inspector_target()
                .cloned(),
            snapshot: session.project_snapshot()?.clone(),
            primary_asset: source.registry_asset,
            primary_clip: source.clip,
        })
    }

    fn pin(
        &self,
        catalog_generation: u64,
        catalog_digest: ContentDigest,
    ) -> DeprojectionWorkspacePin {
        DeprojectionWorkspacePin {
            document_generation: self.document_generation,
            publication_generation: self.publication_generation,
            project_revisions: self.revisions,
            selection_revision: self.selection_revision,
            catalog_generation,
            catalog_digest,
        }
    }
}

impl ProjectSession {
    /// Publish real live analysis into session-owned candidate documents.
    pub fn publish_deprojection_analysis(
        &mut self,
        analysis: LiveDeprojectionAnalysis,
        cancellation: &RenderCancellation,
    ) -> Result<Vec<DeprojectionCandidateDocumentSummary>, DeprojectionWorkspaceBridgeError> {
        let context = SessionContext::capture(self)?;
        self.deprojection_workspace
            .publish(context, analysis, cancellation)
    }

    /// Compatibility spelling retained for early workspace integrations.
    pub fn publish_live_deprojection_analysis(
        &mut self,
        analysis: LiveDeprojectionAnalysis,
        cancellation: &RenderCancellation,
    ) -> Result<Vec<DeprojectionCandidateDocumentSummary>, DeprojectionWorkspaceBridgeError> {
        self.publish_deprojection_analysis(analysis, cancellation)
    }

    pub fn list_deprojection_workspace_candidates(
        &self,
    ) -> Result<Vec<DeprojectionCandidateDocumentSummary>, DeprojectionWorkspaceBridgeError> {
        let context = SessionContext::capture(self)?;
        Ok(self
            .deprojection_workspace
            .documents
            .values()
            .map(|document| {
                document.summary(
                    if self
                        .deprojection_workspace
                        .is_current(&context, document.pin)
                    {
                        DeprojectionCandidateFreshness::Current
                    } else {
                        DeprojectionCandidateFreshness::Invalidated
                    },
                )
            })
            .collect())
    }

    pub fn select_deprojection_workspace_candidate(
        &mut self,
        view: WorkspaceViewId,
        object: ObjectRef,
    ) -> Result<DeprojectionCandidateDocumentSummary, DeprojectionWorkspaceBridgeError> {
        let context = SessionContext::capture(self)?;
        self.deprojection_workspace.select(context, view, object)
    }

    pub fn resolve_deprojection_workspace_request(
        &self,
        target: DeprojectionWorkspaceTarget,
    ) -> Result<ResolvedDeprojectionWorkspaceRequest, DeprojectionWorkspaceBridgeError> {
        let context = SessionContext::capture(self)?;
        self.deprojection_workspace.resolve(context, target)
    }

    pub fn deprojection_workspace_artifacts(&self) -> &ArtifactCatalog {
        self.deprojection_workspace.catalog()
    }

    pub fn deprojection_workspace_interpretations(&self) -> &InterpretationStore {
        self.deprojection_workspace.interpretations()
    }

    pub(crate) fn require_deprojection_promotion_cohort(
        &self,
        pin: artifact_promotion_bridge::ArtifactPromotionWorkspacePin,
        descriptor: &ArtifactDescriptor,
        payload: &Arc<ArtifactComparisonPayload>,
        candidate: crate::deprojection_program::DeprojectionCandidateId,
    ) -> Result<(), artifact_promotion_bridge::ArtifactPromotionBridgeError> {
        let current_document_generation = self.document_generation();
        if current_document_generation != pin.document_generation {
            return Err(
                artifact_promotion_bridge::ArtifactPromotionBridgeError::DocumentSuperseded {
                    pinned: pin.document_generation,
                    current: current_document_generation,
                },
            );
        }
        let current_publication_generation = self.snapshot().generation;
        if current_publication_generation != pin.publication_generation {
            return Err(
                artifact_promotion_bridge::ArtifactPromotionBridgeError::PublicationSuperseded {
                    pinned: pin.publication_generation,
                    current: current_publication_generation,
                },
            );
        }
        let current_revisions = self.project_snapshot()?.revisions();
        if current_revisions != pin.project_revisions {
            return Err(
                artifact_promotion_bridge::ArtifactPromotionBridgeError::StaleArtifactRevision {
                    pinned: pin.project_revisions,
                    current: current_revisions,
                },
            );
        }
        let current_selection_revision = self.selection().revision;
        if current_selection_revision != pin.selection_revision {
            return Err(
                artifact_promotion_bridge::ArtifactPromotionBridgeError::SelectionSuperseded {
                    pinned: pin.selection_revision,
                    current: current_selection_revision,
                },
            );
        }
        let current_catalog_digest = catalog_digest(&self.deprojection_workspace.catalog);
        if self.deprojection_workspace.catalog_generation != pin.catalog_generation
            || current_catalog_digest != pin.catalog_digest
        {
            return Err(
                artifact_promotion_bridge::ArtifactPromotionBridgeError::ArtifactCatalogSuperseded {
                    pinned_generation: pin.catalog_generation,
                    current_generation: self.deprojection_workspace.catalog_generation,
                    pinned_digest: pin.catalog_digest,
                    current_digest: current_catalog_digest,
                },
            );
        }
        let Some(document) = self
            .deprojection_workspace
            .documents
            .values()
            .find(|document| {
                document.pin == pin
                    && document.descriptor.id == descriptor.id
                    && document.candidate.id == candidate
            })
        else {
            return Err(
                artifact_promotion_bridge::ArtifactPromotionBridgeError::WorkspaceDocumentMissing {
                    artifact: descriptor.id,
                    candidate,
                },
            );
        };
        let Some(owned_descriptor) = self
            .deprojection_workspace
            .catalog
            .descriptor(descriptor.id)
        else {
            return Err(
                artifact_promotion_bridge::ArtifactPromotionBridgeError::MissingArtifact(
                    descriptor.id,
                ),
            );
        };
        if owned_descriptor != descriptor || &document.descriptor != descriptor {
            return Err(
                artifact_promotion_bridge::ArtifactPromotionBridgeError::WorkspaceArtifactDescriptorMismatch(
                    descriptor.id,
                ),
            );
        }
        let owned_payload = self
            .deprojection_workspace
            .catalog
            .get::<ArtifactComparisonPayload>(descriptor.id)
            .map_err(|_| {
                artifact_promotion_bridge::ArtifactPromotionBridgeError::PayloadTypeMismatch(
                    descriptor.id,
                )
            })?;
        if !Arc::ptr_eq(&owned_payload, payload) || !Arc::ptr_eq(&document.payload, payload) {
            return Err(
                artifact_promotion_bridge::ArtifactPromotionBridgeError::WorkspaceArtifactPayloadMismatch(
                    descriptor.id,
                ),
            );
        }
        Ok(())
    }
}

impl DeprojectionWorkspaceBridge {
    fn publish(
        &mut self,
        context: SessionContext,
        analysis: LiveDeprojectionAnalysis,
        cancellation: &RenderCancellation,
    ) -> Result<Vec<DeprojectionCandidateDocumentSummary>, DeprojectionWorkspaceBridgeError> {
        if cancellation.is_cancelled() {
            return Err(DeprojectionWorkspaceBridgeError::Analysis(
                "analysis publication cancelled".into(),
            ));
        }
        let next_catalog_generation = self.catalog_generation.saturating_add(1).max(1);
        let (descriptor, payload, candidates, scope) =
            build_analysis_publication(&context, next_catalog_generation, analysis, cancellation)?;
        insert_artifact_comparison_payload(
            &mut self.catalog,
            descriptor.clone(),
            Arc::clone(&payload),
        )
        .map_err(|error| DeprojectionWorkspaceBridgeError::Catalog(error.to_string()))?;
        self.catalog_generation = next_catalog_generation;

        let pin = context.pin(self.catalog_generation, catalog_digest(&self.catalog));
        let source = source_context(&context, descriptor.extent)?;
        let mut summaries = Vec::with_capacity(candidates.len());
        for mut candidate in candidates {
            attach_artifact_claim(&descriptor, &mut candidate)?;
            let bindings = promotion_bindings(&context, &candidate, source.resolved);
            let placement = PromotionPlacement {
                start_frame: descriptor.extent.start,
                ..PromotionPlacement::default()
            };
            let explanation = self
                .interpretations
                .allocate_explanation_id()
                .map_err(|error| {
                    DeprojectionWorkspaceBridgeError::Interpretation(error.to_string())
                })?;
            let comparison = self
                .interpretations
                .allocate_comparison_id()
                .map_err(|error| {
                    DeprojectionWorkspaceBridgeError::Interpretation(error.to_string())
                })?;
            let label = format!("Promoted {}", candidate.label);
            let mut explanation_definition = ExplanationDefinition {
                id: explanation,
                label: label.clone(),
                scope: scope.clone(),
                extent: Aspect::Time(descriptor.extent),
                evidence: vec![ExplanationEvidenceRef::Artifact(descriptor.id)],
                provenance: descriptor.provenance.clone(),
            };
            explanation_definition
                .normalize_and_validate()
                .map_err(|error| {
                    DeprojectionWorkspaceBridgeError::Interpretation(error.to_string())
                })?;
            let comparison_definition = ComparisonDefinition {
                id: comparison,
                label: label.clone(),
                source: source.citation,
                explanation,
                provenance: descriptor.provenance.clone(),
            };
            self.interpretations
                .apply(&[
                    InterpretationCommand::PutExplanation {
                        before: None,
                        after: Some(explanation_definition),
                    },
                    InterpretationCommand::PutComparison {
                        before: None,
                        after: Some(comparison_definition),
                    },
                ])
                .map_err(|error| {
                    DeprojectionWorkspaceBridgeError::Interpretation(error.to_string())
                })?;
            let id = DeprojectionCandidateDocumentId(self.next_document);
            self.next_document = self.next_document.checked_add(1).ok_or_else(|| {
                DeprojectionWorkspaceBridgeError::Invalid(
                    "candidate document identity exhausted".into(),
                )
            })?;
            let document = CandidateDocument {
                id,
                descriptor: descriptor.clone(),
                payload: Arc::clone(&payload),
                candidate,
                bindings,
                placement,
                target: artifact_promotion_bridge::ArtifactPromotionComparisonTarget {
                    comparison,
                    explanation,
                    label,
                    source: source.citation,
                    provenance: descriptor.provenance.clone(),
                },
                recipe: ComparisonProductRecipe::default(),
                pin,
            };
            self.objects.insert(ObjectRef::Explanation(explanation), id);
            self.objects.insert(ObjectRef::Comparison(comparison), id);
            summaries.push(document.summary(DeprojectionCandidateFreshness::Current));
            self.documents.insert(id, document);
        }
        Ok(summaries)
    }

    fn select(
        &mut self,
        context: SessionContext,
        view: WorkspaceViewId,
        object: ObjectRef,
    ) -> Result<DeprojectionCandidateDocumentSummary, DeprojectionWorkspaceBridgeError> {
        let id = *self
            .objects
            .get(&object)
            .ok_or_else(|| DeprojectionWorkspaceBridgeError::UnknownObject(object.clone()))?;
        let document = &self.documents[&id];
        if !self.is_current(&context, document.pin) {
            return Err(DeprojectionWorkspaceBridgeError::Invalidated(id));
        }
        self.selected_views.insert(view, id);
        Ok(document.summary(DeprojectionCandidateFreshness::Current))
    }

    fn resolve(
        &self,
        context: SessionContext,
        target: DeprojectionWorkspaceTarget,
    ) -> Result<ResolvedDeprojectionWorkspaceRequest, DeprojectionWorkspaceBridgeError> {
        let id = match target {
            DeprojectionWorkspaceTarget::Object(object) => *self
                .objects
                .get(&object)
                .ok_or(DeprojectionWorkspaceBridgeError::UnknownObject(object))?,
            DeprojectionWorkspaceTarget::View(view) => *self
                .selected_views
                .get(&view)
                .ok_or(DeprojectionWorkspaceBridgeError::UnknownView(view))?,
        };
        let document = &self.documents[&id];
        if !self.is_current(&context, document.pin) {
            return Err(DeprojectionWorkspaceBridgeError::Invalidated(id));
        }
        Ok(ResolvedDeprojectionWorkspaceRequest {
            document: id,
            descriptor: document.descriptor.clone(),
            payload: Arc::clone(&document.payload),
            request: artifact_promotion_bridge::ArtifactPromotionComparisonRequest {
                artifact: document.descriptor.id,
                artifact_pin: document.payload.pin,
                workspace_pin: document.pin,
                candidate: document.candidate.clone(),
                bindings: document.bindings.clone(),
                placement: document.placement,
                target: document.target.clone(),
                recipe: document.recipe.clone(),
            },
        })
    }

    fn current_pin(&self, context: &SessionContext) -> DeprojectionWorkspacePin {
        context.pin(self.catalog_generation, catalog_digest(&self.catalog))
    }

    fn is_current(&self, context: &SessionContext, pin: DeprojectionWorkspacePin) -> bool {
        self.current_pin(context) == pin
    }
}

fn catalog_digest(catalog: &ArtifactCatalog) -> ContentDigest {
    let mut identities = Vec::with_capacity(catalog.len().saturating_mul(33));
    for descriptor in catalog.descriptors() {
        identities.push(match descriptor.id.0.algorithm {
            DigestAlgorithm::Sha256 => 1,
            DigestAlgorithm::Blake3 => 2,
            DigestAlgorithm::StableNonCryptographic => 3,
        });
        identities.extend_from_slice(&descriptor.id.0.bytes);
    }
    sha256_content(b"audec:deprojection-workspace-catalog:v1", &[&identities])
}

#[derive(Clone, Copy)]
struct SourceContext {
    citation: SourceCitation,
    resolved: ResolvedSourceAsset,
}

fn source_context(
    context: &SessionContext,
    extent: FrameSpan,
) -> Result<SourceContext, DeprojectionWorkspaceBridgeError> {
    if let Some(selected) = context.selection_time {
        if selected != extent {
            return Err(
                DeprojectionWorkspaceBridgeError::SelectionDoesNotAddressArtifact {
                    selected,
                    artifact: extent,
                },
            );
        }
    }
    let clip = context
        .snapshot
        .project
        .state()
        .domains
        .arrangement
        .clip(context.primary_clip)
        .ok_or_else(|| {
            DeprojectionWorkspaceBridgeError::Invalid("primary source clip is missing".into())
        })?;
    let ClipContent::Audio(region) = &clip.content else {
        return Err(DeprojectionWorkspaceBridgeError::Invalid(
            "primary source clip is not audio".into(),
        ));
    };
    if region.playback.reverse
        || !region.playback.warp_markers.is_empty()
        || !matches!(region.loop_mode, AudioLoopMode::Off)
        || extent.start < clip.placement.start.0
        || extent.end > clip.placement.end.0
    {
        return Err(DeprojectionWorkspaceBridgeError::Invalid(
            "selected analysis span has no exact primary-source mapping".into(),
        ));
    }
    let project_start = u64::try_from(extent.start - clip.placement.start.0)
        .map_err(|_| DeprojectionWorkspaceBridgeError::Invalid("negative source offset".into()))?;
    let project_end = u64::try_from(extent.end - clip.placement.start.0)
        .map_err(|_| DeprojectionWorkspaceBridgeError::Invalid("negative source offset".into()))?;
    let source_start = region.source.start
        + region
            .playback
            .ratio
            .source_offset(project_start)
            .map_err(|error| DeprojectionWorkspaceBridgeError::Invalid(error.to_string()))?;
    let source_end = region.source.start
        + region
            .playback
            .ratio
            .source_offset(project_end)
            .map_err(|error| DeprojectionWorkspaceBridgeError::Invalid(error.to_string()))?;
    let source_range =
        AssetFrameRange::new(SampleFrames(source_start), SampleFrames(source_end))
            .map_err(|error| DeprojectionWorkspaceBridgeError::Invalid(error.to_string()))?;
    let channels = context
        .snapshot
        .project
        .state()
        .domains
        .assets
        .get(context.primary_asset)
        .map(|record| record.metadata().channels)
        .ok_or_else(|| {
            DeprojectionWorkspaceBridgeError::Invalid("primary asset is missing".into())
        })?;
    if channels == 0 || channels > 16 {
        return Err(DeprojectionWorkspaceBridgeError::Invalid(
            "comparison channel mask cannot represent source".into(),
        ));
    }
    let channel_mask = ChannelMask(if channels == 16 {
        u16::MAX
    } else {
        (1_u16 << channels) - 1
    });
    Ok(SourceContext {
        citation: SourceCitation {
            asset: context.primary_asset,
            source_range,
            project_span: extent,
            channels: channel_mask,
        },
        resolved: ResolvedSourceAsset {
            asset: context.primary_asset,
            claim_frame_zero: source_start,
            frame_count: source_end - source_start,
        },
    })
}

fn build_analysis_publication(
    context: &SessionContext,
    catalog_generation: u64,
    analysis: LiveDeprojectionAnalysis,
    cancellation: &RenderCancellation,
) -> Result<
    (
        ArtifactDescriptor,
        Arc<ArtifactComparisonPayload>,
        Vec<DeprojectionCandidate>,
        ExplanationScope,
    ),
    DeprojectionWorkspaceBridgeError,
> {
    let hydration = ArtifactHydrationContext {
        project_revisions: context.revisions,
        publication_generation: context.publication_generation,
        catalog_generation,
    };
    match analysis {
        LiveDeprojectionAnalysis::Rhythm {
            descriptor,
            deprojection,
            budget,
            rendered,
        } => {
            require_kind(&descriptor, ArtifactKind::ModelClaim)?;
            let source = material_span(&descriptor, source_context(context, descriptor.extent)?)?;
            let mut claim = SourceClaim::literal(source.clone(), descriptor.output_digest)
                .map_err(|error| DeprojectionWorkspaceBridgeError::Analysis(error.to_string()))?;
            attach_claim_descriptor(&descriptor, &mut claim);
            let explanations = explain_rhythm(&deprojection, &[], budget)
                .map_err(|error| DeprojectionWorkspaceBridgeError::Analysis(error.to_string()))?;
            let candidates = candidates_from_rhythm_explanations(
                source,
                claim,
                &explanations,
                StructuralScorePolicy::default(),
            )
            .map_err(|error| DeprojectionWorkspaceBridgeError::Analysis(error.to_string()))?;
            if candidates.is_empty() {
                return Err(DeprojectionWorkspaceBridgeError::NoExecutableCandidate);
            }
            let claim_key = artifact_local_id(descriptor.id);
            let pin = ArtifactComparisonPin::from_descriptor(
                &descriptor,
                hydration.project_revisions,
                hydration.publication_generation,
                hydration.catalog_generation,
            );
            let payload = Arc::new(
                ArtifactComparisonPayload::from_rhythm_render(
                    &descriptor,
                    pin,
                    claim_key,
                    rendered,
                    cancellation,
                )
                .map_err(|error| DeprojectionWorkspaceBridgeError::Analysis(error.to_string()))?,
            );
            Ok((
                descriptor.clone(),
                payload,
                candidates,
                ExplanationScope::ModelClaim {
                    artifact: descriptor.id,
                    claim: claim_key,
                },
            ))
        }
        LiveDeprojectionAnalysis::Hpss {
            descriptor,
            result,
            component,
            candidate,
        } => {
            require_kind(&descriptor, ArtifactKind::Hpss)?;
            let pin = ArtifactComparisonPin::from_descriptor(
                &descriptor,
                hydration.project_revisions,
                hydration.publication_generation,
                hydration.catalog_generation,
            );
            let payload = Arc::new(
                ArtifactComparisonPayload::from_hpss(&descriptor, pin, &result, cancellation)
                    .map_err(|error| {
                        DeprojectionWorkspaceBridgeError::Analysis(error.to_string())
                    })?,
            );
            Ok((
                descriptor.clone(),
                payload,
                vec![candidate],
                ExplanationScope::HpssComponent {
                    artifact: descriptor.id,
                    component,
                },
            ))
        }
        LiveDeprojectionAnalysis::Loom {
            descriptor,
            sketch,
            source_start_frame,
            mut clusters,
            candidate,
        } => {
            require_kind(&descriptor, ArtifactKind::LoomSketch)?;
            clusters.sort_unstable();
            clusters.dedup();
            if clusters.is_empty() {
                return Err(DeprojectionWorkspaceBridgeError::Analysis(
                    "Loom selection has no clusters".into(),
                ));
            }
            let pin = ArtifactComparisonPin::from_descriptor(
                &descriptor,
                hydration.project_revisions,
                hydration.publication_generation,
                hydration.catalog_generation,
            );
            let payload = Arc::new(
                ArtifactComparisonPayload::from_loom(
                    &descriptor,
                    pin,
                    &sketch,
                    source_start_frame,
                    cancellation,
                )
                .map_err(|error| DeprojectionWorkspaceBridgeError::Analysis(error.to_string()))?,
            );
            Ok((
                descriptor.clone(),
                payload,
                vec![candidate],
                ExplanationScope::LoomSketch {
                    artifact: descriptor.id,
                    clusters,
                },
            ))
        }
        LiveDeprojectionAnalysis::Model {
            descriptor,
            claim,
            output_name,
            published,
            rendered,
        } => {
            require_kind(&descriptor, ArtifactKind::ModelClaim)?;
            if published.artifact != descriptor.id
                || published.output_digest != descriptor.output_digest
            {
                return Err(DeprojectionWorkspaceBridgeError::Analysis(
                    "model publication does not match artifact descriptor".into(),
                ));
            }
            let adapted = candidate_from_model_output(
                &claim,
                &output_name,
                published,
                StructuralScorePolicy::default(),
            )
            .map_err(|error| DeprojectionWorkspaceBridgeError::Analysis(error.to_string()))?;
            let candidate = adapted
                .candidate
                .ok_or(DeprojectionWorkspaceBridgeError::NoExecutableCandidate)?;
            let claim_key = artifact_local_id(descriptor.id);
            let pin = ArtifactComparisonPin::from_descriptor(
                &descriptor,
                hydration.project_revisions,
                hydration.publication_generation,
                hydration.catalog_generation,
            );
            let payload = Arc::new(
                ArtifactComparisonPayload::from_model_render(
                    &descriptor,
                    pin,
                    claim_key,
                    rendered,
                    cancellation,
                )
                .map_err(|error| DeprojectionWorkspaceBridgeError::Analysis(error.to_string()))?,
            );
            Ok((
                descriptor.clone(),
                payload,
                vec![candidate],
                ExplanationScope::ModelClaim {
                    artifact: descriptor.id,
                    claim: claim_key,
                },
            ))
        }
    }
}

fn require_kind(
    descriptor: &ArtifactDescriptor,
    kind: ArtifactKind,
) -> Result<(), DeprojectionWorkspaceBridgeError> {
    descriptor
        .validate()
        .map_err(|error| DeprojectionWorkspaceBridgeError::Analysis(error.to_string()))?;
    if descriptor.kind != kind {
        return Err(DeprojectionWorkspaceBridgeError::Analysis(format!(
            "artifact {:?} cannot hydrate {:?}",
            descriptor.kind, kind
        )));
    }
    Ok(())
}

fn material_span(
    descriptor: &ArtifactDescriptor,
    source: SourceContext,
) -> Result<MaterialSpan, DeprojectionWorkspaceBridgeError> {
    let frames = source.citation.source_range.len().0;
    Ok(MaterialSpan {
        material_sha256: hex_digest(descriptor.source_digest.bytes),
        start_frame: source.citation.source_range.start.0,
        frame_count: frames,
        sample_rate_hz: descriptor.sample_rate,
        channels: descriptor.channels,
    })
}

fn attach_artifact_claim(
    descriptor: &ArtifactDescriptor,
    candidate: &mut DeprojectionCandidate,
) -> Result<(), DeprojectionWorkspaceBridgeError> {
    let Some(claim_id) = candidate
        .source_claims
        .iter()
        .find(|claim| claim.source == candidate.program.source)
        .map(|claim| claim.id)
    else {
        return Err(DeprojectionWorkspaceBridgeError::Analysis(
            "candidate has no source claim for its program material".into(),
        ));
    };
    for claim in candidate
        .source_claims
        .iter_mut()
        .filter(|claim| claim.source == candidate.program.source)
    {
        attach_claim_descriptor(descriptor, claim);
    }
    let evidence = [
        EvidenceRef::Artifact(descriptor.id),
        EvidenceRef::SourceClaim(claim_id),
    ];
    for term in candidate.program.terms.values_mut() {
        term.evidence.extend(evidence.iter().cloned());
        term.evidence.sort();
        term.evidence.dedup();
        term.derivation.premises.extend(evidence.iter().cloned());
        term.derivation.premises.sort();
        term.derivation.premises.dedup();
    }
    Ok(())
}

fn attach_claim_descriptor(descriptor: &ArtifactDescriptor, claim: &mut SourceClaim) {
    claim.artifact = Some(descriptor.id);
    claim.output_digest = descriptor.output_digest;
    claim.producer_recipe = descriptor.recipe_digest;
    claim.producer = match &descriptor.provenance.producer {
        crate::ontology::Producer::Analyzer { name, version, .. } => format!("{name}@{version}"),
        crate::ontology::Producer::Human { .. } => "authored-analysis".into(),
        crate::ontology::Producer::Importer { format, version } => {
            format!("import:{format}@{version}")
        }
    };
}

fn promotion_bindings(
    context: &SessionContext,
    candidate: &DeprojectionCandidate,
    source: ResolvedSourceAsset,
) -> PromotionBindings {
    let source_assets = candidate
        .source_claims
        .iter()
        .filter(|claim| claim.source == candidate.program.source)
        .map(|claim| (claim.id, source))
        .collect();
    let mut curve_targets = BTreeMap::new();
    if let Some(ObjectRef::Automation(lane)) = context.selected_object {
        if let Some(target) = context
            .snapshot
            .project
            .state()
            .domains
            .automation
            .lane(lane)
            .map(|lane| lane.target.clone())
        {
            let curve_roots = candidate
                .program
                .roots
                .iter()
                .filter(|root| {
                    matches!(
                        candidate.program.terms.get(root).map(|term| &term.kind),
                        Some(crate::deprojection_program::EditableTermKind::Curve { .. })
                    )
                })
                .copied()
                .collect::<Vec<_>>();
            if curve_roots.len() == 1 {
                curve_targets.insert(curve_roots[0], target);
            }
        }
    }
    PromotionBindings {
        source_assets,
        curve_targets,
        ..PromotionBindings::default()
    }
}

fn artifact_local_id(artifact: ArtifactId) -> u64 {
    u64::from_le_bytes(artifact.0.bytes[..8].try_into().expect("digest prefix")) | 1
}

fn hex_digest(bytes: [u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::artifact_catalog::{ContentDigest, DigestAlgorithm};
    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        ContentFingerprint, DecodedAudioMetadata,
    };
    use crate::audio::{AudioFormat, ProjectAudio};
    use crate::daw_render::PcmAsset;
    use crate::deprojection_program::EditableTermKind;
    use crate::live_project::{LiveProject, SourceMaterialMetadata};
    use crate::ontology::{Producer, Provenance};
    use crate::project_selection::{EditCursor, ProjectSelection};
    use crate::project_session::ProjectSessionId;
    use crate::rhythm::{analyze_mono, RhythmConfig};

    fn digest(byte: u8) -> ContentDigest {
        ContentDigest::new(DigestAlgorithm::Sha256, [byte; 32])
    }

    fn provenance() -> Provenance {
        Provenance {
            producer: Producer::Analyzer {
                name: "live-rhythm".into(),
                version: "1".into(),
                configuration_digest: Some("22".repeat(32)),
            },
            created_unix_ms: Some(1_700_000_000_000),
            source_revision: None,
            note: None,
        }
    }

    fn rhythm_fixture() -> (ProjectSession, Vec<f32>, ArtifactDescriptor) {
        let sample_rate = 8_000;
        let frame_count = 8_000;
        let mut samples = vec![0.0; frame_count];
        for onset in (400..frame_count).step_by(1_000) {
            for offset in 0..80 {
                samples[onset + offset] =
                    (1.0 - offset as f32 / 80.0) * if offset % 2 == 0 { 0.9 } else { -0.9 };
            }
        }
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/audio/live-rhythm.wav").unwrap()),
            None,
        )
        .unwrap();
        let mut registry = crate::assets::AssetRegistry::new();
        let asset = registry
            .register(AssetRegistration {
                name: "live rhythm".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: sample_rate,
                    channels: 1,
                    frame_count: SampleFrames(frame_count as u64),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32le".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"live rhythm fixture"),
                provenance: AssetProvenance::new(
                    1,
                    AssetOrigin::ImportedFile {
                        importer: "test".into(),
                    },
                    location,
                ),
                tags: BTreeSet::new(),
                favorite: false,
            })
            .unwrap();
        let pcm = PcmAsset::new(
            AudioFormat::new(sample_rate, 1).unwrap(),
            Arc::from(samples.clone()),
        )
        .unwrap();
        let live = LiveProject::from_source_material(
            SourceMaterialMetadata::new("Live analysis", "Rhythm source"),
            registry,
            asset,
            pcm,
        )
        .unwrap();
        let mut session = ProjectSession::new(ProjectSessionId(812)).unwrap();
        session.install(live, None).unwrap();
        let extent = FrameSpan::new(0, frame_count as i64).unwrap();
        session.replace_selection(ProjectSelection {
            time: Some(extent),
            aspect: Some(Aspect::Time(extent)),
            ..ProjectSelection::default()
        });
        let descriptor = ArtifactDescriptor {
            id: ArtifactId(digest(0x44)),
            kind: ArtifactKind::ModelClaim,
            source_digest: digest(0x11),
            recipe_digest: digest(0x22),
            output_digest: digest(0x44),
            extent,
            sample_rate,
            channels: 1,
            provenance: provenance(),
        };
        (session, samples, descriptor)
    }

    fn publish_exact_request(
        session: &mut ProjectSession,
        samples: &[f32],
        descriptor: ArtifactDescriptor,
    ) -> ResolvedDeprojectionWorkspaceRequest {
        let analysis = analyze_mono(samples, descriptor.sample_rate, &RhythmConfig::default());
        session
            .publish_deprojection_analysis(
                LiveDeprojectionAnalysis::from_rhythm(
                    descriptor.clone(),
                    analysis,
                    ExplainBudget::default(),
                    RenderedExplanation {
                        origin_frame: descriptor.extent.start,
                        audio: ProjectAudio::from_interleaved(
                            AudioFormat::new(descriptor.sample_rate, descriptor.channels).unwrap(),
                            samples.to_vec(),
                        )
                        .unwrap(),
                    },
                ),
                &RenderCancellation::new(),
            )
            .unwrap()
            .into_iter()
            .find_map(|summary| {
                let resolved = session
                    .resolve_deprojection_workspace_request(DeprojectionWorkspaceTarget::Object(
                        ObjectRef::Comparison(summary.comparison),
                    ))
                    .ok()?;
                resolved
                    .request
                    .candidate
                    .program
                    .roots
                    .iter()
                    .any(|root| {
                        matches!(
                            resolved.request.candidate.program.terms[root].kind,
                            EditableTermKind::ExactAudioReference { .. }
                        )
                    })
                    .then_some(resolved)
            })
            .expect("literal fallback candidate")
    }

    #[test]
    fn actual_rhythm_analysis_publishes_selects_and_resolves_complete_request() {
        let (mut session, samples, descriptor) = rhythm_fixture();
        let analysis = analyze_mono(&samples, descriptor.sample_rate, &RhythmConfig::default());
        assert!(!analysis.silent);
        let cancellation = RenderCancellation::new();
        let summaries = session
            .publish_deprojection_analysis(
                LiveDeprojectionAnalysis::from_rhythm(
                    descriptor.clone(),
                    analysis,
                    ExplainBudget::default(),
                    RenderedExplanation {
                        origin_frame: descriptor.extent.start,
                        audio: ProjectAudio::from_interleaved(
                            AudioFormat::new(descriptor.sample_rate, descriptor.channels).unwrap(),
                            samples,
                        )
                        .unwrap(),
                    },
                ),
                &cancellation,
            )
            .unwrap();
        assert!(!summaries.is_empty());
        assert_eq!(session.deprojection_workspace_artifacts().len(), 1);

        // The analysis always publishes its explicitly distinct literal-audio
        // fallback, even when canonical pattern candidates are also present.
        let (summary, direct) = summaries
            .iter()
            .find_map(|summary| {
                let resolved = session
                    .resolve_deprojection_workspace_request(DeprojectionWorkspaceTarget::Object(
                        ObjectRef::Comparison(summary.comparison),
                    ))
                    .ok()?;
                let is_exact = resolved.request.candidate.program.roots.iter().any(|root| {
                    matches!(
                        resolved.request.candidate.program.terms[root].kind,
                        EditableTermKind::ExactAudioReference { .. }
                    )
                });
                is_exact.then_some((summary, resolved))
            })
            .expect("literal fallback candidate");
        assert_eq!(direct.descriptor, descriptor);
        assert_eq!(direct.request.artifact_pin, direct.payload.pin);
        assert_eq!(
            direct.request.placement.start_frame,
            descriptor.extent.start
        );
        assert!(direct
            .request
            .candidate
            .program
            .terms
            .values()
            .flat_map(|term| &term.evidence)
            .any(|evidence| *evidence == EvidenceRef::Artifact(descriptor.id)));
        assert!(direct
            .request
            .candidate
            .source_claims
            .iter()
            .all(|claim| claim.artifact == Some(descriptor.id)));
        assert_eq!(direct.request.bindings.source_assets.len(), 1);

        let view = WorkspaceViewId(91);
        session
            .select_deprojection_workspace_candidate(
                view,
                ObjectRef::Comparison(summary.comparison),
            )
            .unwrap();
        let selected = session
            .resolve_deprojection_workspace_request(DeprojectionWorkspaceTarget::View(view))
            .unwrap();
        assert_eq!(selected.document, direct.document);
        assert_eq!(selected.request, direct.request);
        crate::artifact_promotion_bridge::plan_artifact_promotion_comparison(
            &session,
            session.deprojection_workspace_artifacts(),
            selected.request,
            &cancellation,
        )
        .expect("resolver output is directly plan-ready");
    }

    #[test]
    fn selection_change_lists_then_refuses_invalidated_candidate() {
        let (mut session, samples, descriptor) = rhythm_fixture();
        let analysis = analyze_mono(&samples, descriptor.sample_rate, &RhythmConfig::default());
        let summaries = session
            .publish_deprojection_analysis(
                LiveDeprojectionAnalysis::from_rhythm(
                    descriptor.clone(),
                    analysis,
                    ExplainBudget::default(),
                    RenderedExplanation {
                        origin_frame: 0,
                        audio: ProjectAudio::from_interleaved(
                            AudioFormat::new(descriptor.sample_rate, 1).unwrap(),
                            samples,
                        )
                        .unwrap(),
                    },
                ),
                &RenderCancellation::new(),
            )
            .unwrap();
        let summary = summaries.first().unwrap();
        session.set_edit_cursor(EditCursor { frame: 17 });
        assert!(session
            .list_deprojection_workspace_candidates()
            .unwrap()
            .iter()
            .all(|candidate| candidate.freshness == DeprojectionCandidateFreshness::Invalidated));
        assert!(matches!(
            session.resolve_deprojection_workspace_request(DeprojectionWorkspaceTarget::Object(
                ObjectRef::Explanation(summary.explanation)
            )),
            Err(DeprojectionWorkspaceBridgeError::Invalidated(id)) if id == summary.id
        ));
    }

    #[test]
    fn planned_promotion_refuses_selection_only_staleness_before_commit() {
        let (mut session, samples, descriptor) = rhythm_fixture();
        let resolved = publish_exact_request(&mut session, &samples, descriptor);
        let pinned = resolved.request.workspace_pin;
        let plan = crate::artifact_promotion_bridge::plan_artifact_promotion_comparison(
            &session,
            session.deprojection_workspace_artifacts(),
            resolved.request,
            &RenderCancellation::new(),
        )
        .unwrap();
        assert_eq!(plan.workspace_pin(), pinned);
        let revisions_before = session.project_snapshot().unwrap().revisions();

        session.set_edit_cursor(EditCursor { frame: 17 });
        let current_selection = session.selection().revision;
        assert_eq!(
            session.project_snapshot().unwrap().revisions(),
            revisions_before
        );
        assert_eq!(session.snapshot().generation, pinned.publication_generation);
        assert_eq!(session.document_generation(), pinned.document_generation);

        assert!(matches!(
            plan.execute(&mut session, &RenderCancellation::new()),
            Err(crate::artifact_promotion_bridge::ArtifactPromotionBridgeError::SelectionSuperseded {
                pinned: refused_pin,
                current,
            }) if refused_pin == pinned.selection_revision && current == current_selection
        ));
        assert_eq!(
            session.project_snapshot().unwrap().revisions(),
            revisions_before
        );
    }

    #[test]
    fn planned_promotion_refuses_catalog_only_staleness_before_commit() {
        let (mut session, samples, descriptor) = rhythm_fixture();
        let resolved = publish_exact_request(&mut session, &samples, descriptor.clone());
        let pinned = resolved.request.workspace_pin;
        let plan = crate::artifact_promotion_bridge::plan_artifact_promotion_comparison(
            &session,
            session.deprojection_workspace_artifacts(),
            resolved.request,
            &RenderCancellation::new(),
        )
        .unwrap();
        let revisions_before = session.project_snapshot().unwrap().revisions();
        let selection_before = session.selection().revision;
        let mut next_descriptor = descriptor;
        next_descriptor.id = ArtifactId(digest(0x45));
        next_descriptor.output_digest = digest(0x45);
        next_descriptor.recipe_digest = digest(0x23);

        publish_exact_request(&mut session, &samples, next_descriptor);
        assert_eq!(
            session.project_snapshot().unwrap().revisions(),
            revisions_before
        );
        assert_eq!(session.selection().revision, selection_before);
        assert_eq!(session.snapshot().generation, pinned.publication_generation);
        assert_eq!(session.document_generation(), pinned.document_generation);

        assert!(matches!(
            plan.execute(&mut session, &RenderCancellation::new()),
            Err(crate::artifact_promotion_bridge::ArtifactPromotionBridgeError::ArtifactCatalogSuperseded {
                pinned_generation,
                current_generation,
                pinned_digest,
                current_digest,
            }) if pinned_generation == pinned.catalog_generation
                && current_generation > pinned_generation
                && pinned_digest == pinned.catalog_digest
                && current_digest != pinned_digest
        ));
        assert_eq!(
            session.project_snapshot().unwrap().revisions(),
            revisions_before
        );
    }
}
