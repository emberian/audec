//! Working-surface documents for reverse-analysis objects.
//!
//! These models are deliberately GPUI-neutral. They retain semantic content,
//! evidence, edit consequences, comparison measurements, and cheap receipts
//! from the one project session. They never own project truth, transport,
//! rendered PCM, an audio device, or an interpretation command executor.
//! Every mutation or audition leaves the pane as an explicit typed request.
//!
//! Host integration is intentionally small:
//! 1. A workspace factory calls [`ReverseSurfacePaneModel::reopen`] with its
//!    durable descriptor and a shared [`ReverseSurfaceStore`].
//! 2. Register the view for project, selection, and audio topics in
//!    `PaneSessionBinding`, then feed each addressed [`PaneSessionPayload`] to
//!    [`ReverseSurfacePaneModel::observe_delivery`].
//! 3. Render only [`ReverseSurfacePaneModel::snapshot`]; viewport/follow stay
//!    in the host entity.
//! 4. Send `SurfaceAuditionIntent::Signal` through the existing comparison
//!    render/controller/audio pipeline. `InspectExcess` changes coverage
//!    presentation only and never invents PCM.
//! 5. Route `SurfaceActionIntent::Reveal` through `ObjectNavigator`; lower an
//!    explicit edit consequence through its declared authority. The pane does
//!    neither operation itself.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use crate::artifact_catalog::ArtifactDescriptor;
use crate::aspect::{FrameSpan, SignalLayer};
use crate::comparison::{
    ComparisonDefinition, ComparisonId, ComparisonMetrics, ComparisonObservation,
};
use crate::comparison_controller::{
    ComparisonChannel, ComparisonController, ComparisonControllerError, ComparisonControllerPhase,
    ComparisonControllerStatus, ComparisonSelectionRequest,
};
use crate::coverage::CoverageSummary;
use crate::daw_project::{ProjectDomain, ProjectRevisions};
use crate::explanation::{ExplanationDefinition, ExplanationEvidenceRef};
use crate::pane_session_binding::{PaneSemanticSelection, PaneSessionPayload, PaneSessionSnapshot};
use crate::project_controller::{FindingRef, ObjectRef, RevealIntent, RevealRequest};
use crate::project_selection::ProjectSelection;
use crate::project_session::{ProjectAudioStatus, ProjectPublication};
use crate::reading::{ReadingFile, ReadingId, ReadingVerificationRefusal, VerificationTier};
use crate::workspace_document::{LinkGroupId, WorkspaceViewDescriptor, WorkspaceViewId};

/// Evidence presented by a reverse working surface. The key is local to the
/// document; any cross-surface identity remains typed in `object`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceEvidence {
    pub key: String,
    pub label: String,
    pub object: Option<ObjectRef>,
    pub extent: Option<FrameSpan>,
    pub derivation: Vec<ObjectRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditAuthority {
    /// Inspecting, auditioning, or selecting; no durable state changes.
    None,
    /// Must be lowered through the interpretation command vocabulary.
    InterpretationCommand,
    /// Must be lowered through the aggregate project command vocabulary.
    ProjectCommand,
    /// Imported reading content is immutable; editing begins a qualified fork.
    ReadingFork(ReadingId),
}

/// A visible account of what an edit would affect. This is descriptive data,
/// not permission to perform the edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceEditConsequence {
    pub key: String,
    pub label: String,
    pub authority: EditAuthority,
    pub invalidates: Vec<ObjectRef>,
    pub creates: Vec<ObjectRef>,
    pub retains_evidence: Vec<ObjectRef>,
    pub affected_domains: BTreeSet<ProjectDomain>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FindingSurfaceDocument {
    pub finding: FindingRef,
    pub label: String,
    pub artifact: Option<ArtifactDescriptor>,
    pub extent: Option<FrameSpan>,
    pub statements: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExplanationSurfaceDocument {
    pub definition: ExplanationDefinition,
    pub dependent_comparisons: Vec<ComparisonId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ComparisonSurfaceDocument {
    pub definition: ComparisonDefinition,
    pub observation: Option<ComparisonObservation>,
    pub coverage: Option<CoverageSummary>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReadingSurfaceDocument {
    pub reading: ReadingFile,
    pub verification: Result<VerificationTier, ReadingVerificationRefusal>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ReverseSurfaceBody {
    Finding(FindingSurfaceDocument),
    Explanation(ExplanationSurfaceDocument),
    Comparison(ComparisonSurfaceDocument),
    Reading(ReadingSurfaceDocument),
}

/// One actual reverse-object document. `comparisons` allows a finding,
/// explanation, or reading surface to host the same four-channel experiment
/// strip as a comparison surface without copying transport or audio state.
#[derive(Clone, Debug, PartialEq)]
pub struct ReverseSurfaceDocument {
    pub object: ObjectRef,
    pub title: String,
    pub body: ReverseSurfaceBody,
    pub evidence: Vec<SurfaceEvidence>,
    pub edit_consequences: Vec<SurfaceEditConsequence>,
    pub comparisons: Vec<ComparisonSurfaceDocument>,
}

impl ReverseSurfaceDocument {
    pub fn finding(
        finding: FindingSurfaceDocument,
        evidence: Vec<SurfaceEvidence>,
        edit_consequences: Vec<SurfaceEditConsequence>,
        comparisons: Vec<ComparisonSurfaceDocument>,
    ) -> Result<Self, ReverseSurfaceError> {
        if let Some(artifact) = &finding.artifact {
            artifact
                .validate()
                .map_err(|error| ReverseSurfaceError::InvalidArtifact(error.to_string()))?;
            if !matches!(
                finding.finding.scope,
                crate::project_controller::FindingScope::Artifact(id) if id == artifact.id
            ) {
                return Err(ReverseSurfaceError::FindingArtifactScopeMismatch);
            }
        }
        Self::validated(Self {
            object: ObjectRef::Finding(finding.finding),
            title: finding.label.clone(),
            body: ReverseSurfaceBody::Finding(finding),
            evidence,
            edit_consequences,
            comparisons,
        })
    }

    pub fn explanation(
        mut definition: ExplanationDefinition,
        comparisons: Vec<ComparisonSurfaceDocument>,
    ) -> Result<Self, ReverseSurfaceError> {
        definition
            .normalize_and_validate()
            .map_err(|error| ReverseSurfaceError::InvalidExplanation(error.to_string()))?;
        let evidence: Vec<SurfaceEvidence> = definition
            .evidence
            .iter()
            .enumerate()
            .map(|(index, evidence)| explanation_evidence(index, evidence))
            .collect();
        let invalidates = comparisons
            .iter()
            .map(|comparison| ObjectRef::Comparison(comparison.definition.id))
            .collect::<Vec<_>>();
        let consequence = SurfaceEditConsequence {
            key: "edit-definition".into(),
            label: "Edit explanation recipe".into(),
            authority: EditAuthority::InterpretationCommand,
            invalidates: invalidates.clone(),
            creates: Vec::new(),
            retains_evidence: evidence
                .iter()
                .filter_map(|evidence: &SurfaceEvidence| evidence.object.clone())
                .collect(),
            affected_domains: definition.scope.project_dependencies(),
        };
        let document = ExplanationSurfaceDocument {
            dependent_comparisons: comparisons
                .iter()
                .map(|comparison| comparison.definition.id)
                .collect(),
            definition: definition.clone(),
        };
        Self::validated(Self {
            object: ObjectRef::Explanation(definition.id),
            title: definition.label.clone(),
            body: ReverseSurfaceBody::Explanation(document),
            evidence,
            edit_consequences: vec![consequence],
            comparisons,
        })
    }

    pub fn from_comparison(
        comparison: ComparisonSurfaceDocument,
    ) -> Result<Self, ReverseSurfaceError> {
        let object = ObjectRef::Comparison(comparison.definition.id);
        let title = comparison.definition.label.clone();
        let evidence = vec![SurfaceEvidence {
            key: "explanation".into(),
            label: "Construction recipe".into(),
            object: Some(ObjectRef::Explanation(comparison.definition.explanation)),
            extent: Some(comparison.definition.source.project_span),
            derivation: Vec::new(),
        }];
        let edit_consequences = vec![SurfaceEditConsequence {
            key: "refresh-observation".into(),
            label: "Refresh comparison measurement".into(),
            authority: EditAuthority::InterpretationCommand,
            invalidates: vec![object.clone()],
            creates: Vec::new(),
            retains_evidence: vec![ObjectRef::Explanation(comparison.definition.explanation)],
            affected_domains: comparison
                .observation
                .as_ref()
                .map(|observation| {
                    observation
                        .dependencies
                        .project
                        .iter()
                        .map(|(domain, _)| *domain)
                        .collect()
                })
                .unwrap_or_default(),
        }];
        Self::validated(Self {
            object,
            title,
            body: ReverseSurfaceBody::Comparison(comparison.clone()),
            evidence,
            edit_consequences,
            comparisons: vec![comparison],
        })
    }

    pub fn reading(
        reading: ReadingFile,
        verification: Result<VerificationTier, ReadingVerificationRefusal>,
    ) -> Result<Self, ReverseSurfaceError> {
        reading
            .validate()
            .map_err(|error| ReverseSurfaceError::InvalidReading(error.to_string()))?;
        let id = reading.reading_id;
        let document = ReadingSurfaceDocument {
            reading: reading.clone(),
            verification,
        };
        Self::validated(Self {
            object: ObjectRef::Reading(id),
            title: reading
                .source
                .declared_title
                .clone()
                .unwrap_or_else(|| format!("Reading {id}")),
            body: ReverseSurfaceBody::Reading(document),
            evidence: reading
                .attachments
                .iter()
                .enumerate()
                .map(|(index, attachment)| SurfaceEvidence {
                    key: format!("attachment:{index}"),
                    label: format!("{} ({})", attachment.role, attachment.media_type),
                    object: None,
                    extent: None,
                    derivation: Vec::new(),
                })
                .collect(),
            edit_consequences: vec![SurfaceEditConsequence {
                key: "fork-reading".into(),
                label: "Fork reading before editing".into(),
                authority: EditAuthority::ReadingFork(id),
                invalidates: Vec::new(),
                creates: Vec::new(),
                retains_evidence: vec![ObjectRef::Reading(id)],
                affected_domains: BTreeSet::new(),
            }],
            comparisons: Vec::new(),
        })
    }

    fn validated(mut document: Self) -> Result<Self, ReverseSurfaceError> {
        if document.title.trim().is_empty() {
            return Err(ReverseSurfaceError::EmptyTitle);
        }
        validate_body_identity(&document.object, &document.body)?;
        normalize_evidence(&mut document.evidence)?;
        normalize_consequences(&mut document.edit_consequences)?;
        normalize_comparisons(&mut document.comparisons)?;
        Ok(document)
    }

    pub fn comparison(&self, id: ComparisonId) -> Option<&ComparisonSurfaceDocument> {
        self.comparisons
            .iter()
            .find(|comparison| comparison.definition.id == id)
    }
}

#[derive(Clone, Debug, Default)]
pub struct ReverseSurfaceStore {
    documents: BTreeMap<String, Arc<ReverseSurfaceDocument>>,
}

impl ReverseSurfaceStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        document: ReverseSurfaceDocument,
    ) -> Result<Arc<ReverseSurfaceDocument>, ReverseSurfaceError> {
        let key = document.object.address();
        if let Some(existing) = self.documents.get(&key) {
            return if existing.as_ref() == &document {
                Ok(Arc::clone(existing))
            } else {
                Err(ReverseSurfaceError::DocumentConflict(document.object))
            };
        }
        let document = Arc::new(document);
        self.documents.insert(key, Arc::clone(&document));
        Ok(document)
    }

    pub fn get(&self, object: &ObjectRef) -> Option<Arc<ReverseSurfaceDocument>> {
        self.documents
            .get(&object.address())
            .and_then(|document| (document.object == *object).then(|| Arc::clone(document)))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ReverseSurfaceLoad {
    Ready(Arc<ReverseSurfaceDocument>),
    Missing(ObjectRef),
    UnsupportedTarget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceChannelSemantic {
    Source,
    Construction,
    ExactResidual,
    SpectralExcess,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceChannelAvailability {
    AwaitingObservation,
    Auditionable,
    CoverageOnly,
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SurfaceChannelMeasurement {
    SampleEnergy(f64),
    SpectralExcess {
        ratio: f64,
        source_power: f64,
        construction_power: f64,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceChannelState {
    pub channel: ComparisonChannel,
    pub semantic: SurfaceChannelSemantic,
    pub availability: SurfaceChannelAvailability,
    pub selected: bool,
    pub active: bool,
    pub measurement: Option<SurfaceChannelMeasurement>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReverseSurfaceSnapshot {
    pub view: WorkspaceViewId,
    pub load: ReverseSurfaceLoad,
    pub selected_comparison: Option<ComparisonId>,
    pub channels: Vec<SurfaceChannelState>,
    pub publication: Option<SurfacePublicationReceipt>,
    pub audio: ProjectAudioStatus,
    pub selection: ProjectSelection,
    pub signal: SignalLayer,
    pub semantic: Option<SurfaceSemanticReceipt>,
    pub controller_phase: Option<ComparisonControllerPhase>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfacePublicationReceipt {
    pub generation: u64,
    pub revisions: ProjectRevisions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceSemanticReceipt {
    pub group: LinkGroupId,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SurfaceAuditionIntent {
    Signal(ComparisonSelectionRequest),
    InspectExcess {
        comparison: ComparisonId,
        coverage: CoverageSummary,
        /// Selecting spectral excess still advances the shared comparison
        /// controller generation. Hosts use this token to clear any older
        /// time-domain audition rather than leaving residual PCM sounding
        /// while the pane displays an excess map.
        controller: ComparisonSelectionRequest,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceActionIntent {
    Reveal(RevealRequest),
    ApplyExplicitConsequence {
        document: ObjectRef,
        consequence: SurfaceEditConsequence,
        requested_at: Option<ProjectRevisions>,
    },
}

/// One runtime pane projection. It stores only immutable documents and cheap
/// session receipts; all authoritative controllers remain outside it.
pub struct ReverseSurfacePaneModel {
    view: WorkspaceViewId,
    load: ReverseSurfaceLoad,
    selected_comparison: Option<ComparisonId>,
    publication: Option<SurfacePublicationReceipt>,
    audio: ProjectAudioStatus,
    selection: ProjectSelection,
    signal: SignalLayer,
    semantic: Option<SurfaceSemanticReceipt>,
    accepted_links: BTreeMap<LinkGroupId, u64>,
    controller: Option<ComparisonControllerStatus>,
}

impl ReverseSurfacePaneModel {
    pub fn open(view: WorkspaceViewId, object: ObjectRef, store: &ReverseSurfaceStore) -> Self {
        let load = store.get(&object).map_or_else(
            || ReverseSurfaceLoad::Missing(object),
            ReverseSurfaceLoad::Ready,
        );
        let selected_comparison = first_comparison(&load);
        Self {
            view,
            load,
            selected_comparison,
            publication: None,
            audio: ProjectAudioStatus::default(),
            selection: ProjectSelection::default(),
            signal: SignalLayer::Source,
            semantic: None,
            accepted_links: BTreeMap::new(),
            controller: None,
        }
    }

    /// Reopen a persisted target through the navigation layer's canonical
    /// descriptor hydrator. Missing content remains an explicit working state.
    pub fn reopen(
        descriptor: &WorkspaceViewDescriptor,
        store: &ReverseSurfaceStore,
    ) -> Result<Self, ReverseSurfaceError> {
        match crate::project_controller::object_from_descriptor(descriptor)
            .map_err(|error| ReverseSurfaceError::TargetHydration(error.to_string()))?
        {
            Some(
                object @ (ObjectRef::Finding(_)
                | ObjectRef::Explanation(_)
                | ObjectRef::Comparison(_)
                | ObjectRef::Reading(_)),
            ) => Ok(Self::open(descriptor.id, object, store)),
            Some(_) | None => Ok(Self {
                view: descriptor.id,
                load: ReverseSurfaceLoad::UnsupportedTarget,
                selected_comparison: None,
                publication: None,
                audio: ProjectAudioStatus::default(),
                selection: ProjectSelection::default(),
                signal: SignalLayer::Source,
                semantic: None,
                accepted_links: BTreeMap::new(),
                controller: None,
            }),
        }
    }

    pub fn retarget(&mut self, object: ObjectRef, store: &ReverseSurfaceStore) {
        self.load = store.get(&object).map_or_else(
            || ReverseSurfaceLoad::Missing(object),
            ReverseSurfaceLoad::Ready,
        );
        self.selected_comparison = first_comparison(&self.load);
        self.controller = None;
    }

    pub fn observe_full_state(
        &mut self,
        snapshot: &PaneSessionSnapshot,
        mut controller: Option<&mut ComparisonController>,
    ) {
        if let Some(publication) = &snapshot.project {
            self.observe_project_publication(publication, controller.as_deref_mut());
        }
        self.selection = snapshot.selection.clone();
        self.signal = snapshot.signal;
        self.observe_audio_status(&snapshot.audio, controller);
    }

    pub fn observe_project_publication(
        &mut self,
        publication: &ProjectPublication,
        controller: Option<&mut ComparisonController>,
    ) {
        self.publication = Some(SurfacePublicationReceipt {
            generation: publication.generation,
            revisions: publication.revisions,
        });
        if let Some(controller) = controller {
            controller.observe_publication(publication);
            self.controller = Some(controller.status());
        }
    }

    pub fn observe_audio_status(
        &mut self,
        status: &ProjectAudioStatus,
        controller: Option<&mut ComparisonController>,
    ) {
        self.audio = status.clone();
        if let Some(controller) = controller {
            controller.observe_audio_status(status);
            self.controller = Some(controller.status());
        }
    }

    pub fn observe_semantic_selection(&mut self, delivery: &PaneSemanticSelection) -> bool {
        if self
            .accepted_links
            .get(&delivery.group)
            .is_some_and(|revision| *revision >= delivery.link_revision)
        {
            return false;
        }
        self.accepted_links
            .insert(delivery.group, delivery.link_revision);
        self.selection = delivery.selection.clone();
        self.signal = delivery.signal;
        self.semantic = Some(SurfaceSemanticReceipt {
            group: delivery.group,
            revision: delivery.link_revision,
        });
        true
    }

    pub fn observe_controller(&mut self, controller: &ComparisonController) {
        self.controller = Some(controller.status());
    }

    pub fn observe_delivery(
        &mut self,
        payload: &PaneSessionPayload,
        controller: Option<&mut ComparisonController>,
    ) {
        match payload {
            PaneSessionPayload::FullState(snapshot) => {
                self.observe_full_state(snapshot, controller)
            }
            PaneSessionPayload::ProjectPublished(publication) => {
                self.observe_project_publication(publication, controller)
            }
            PaneSessionPayload::SemanticSelection(delivery) => {
                self.observe_semantic_selection(delivery);
            }
            PaneSessionPayload::AudioChanged(status) => {
                self.observe_audio_status(status, controller)
            }
        }
    }

    pub fn select_comparison(
        &mut self,
        comparison: ComparisonId,
    ) -> Result<(), ReverseSurfaceError> {
        self.comparison(comparison)?;
        self.selected_comparison = Some(comparison);
        Ok(())
    }

    pub fn request_channel(
        &mut self,
        channel: ComparisonChannel,
        controller: &mut ComparisonController,
    ) -> Result<SurfaceAuditionIntent, ReverseSurfaceError> {
        let id = self
            .selected_comparison
            .ok_or(ReverseSurfaceError::NoComparison)?;
        let comparison = self.comparison(id)?.clone();
        let observation = comparison
            .observation
            .as_ref()
            .ok_or(ReverseSurfaceError::MissingObservation(id))?;
        if channel == ComparisonChannel::Excess {
            let coverage = comparison
                .coverage
                .ok_or(ReverseSurfaceError::MissingCoverage(id))?;
            let revisions = self
                .publication
                .ok_or(ReverseSurfaceError::MissingPublication)?
                .revisions;
            let request = controller
                .select(&comparison.definition, observation, revisions, channel)
                .map_err(ReverseSurfaceError::Controller)?;
            self.controller = Some(controller.status());
            return Ok(SurfaceAuditionIntent::InspectExcess {
                comparison: id,
                coverage,
                controller: request,
            });
        }
        let revisions = self
            .publication
            .ok_or(ReverseSurfaceError::MissingPublication)?
            .revisions;
        let request = controller
            .select(&comparison.definition, observation, revisions, channel)
            .map_err(ReverseSurfaceError::Controller)?;
        self.controller = Some(controller.status());
        Ok(SurfaceAuditionIntent::Signal(request))
    }

    pub fn reveal_evidence(&self, key: &str) -> Result<SurfaceActionIntent, ReverseSurfaceError> {
        let document = self.document()?;
        let evidence = document
            .evidence
            .iter()
            .find(|evidence| evidence.key == key)
            .ok_or_else(|| ReverseSurfaceError::UnknownEvidence(key.into()))?;
        let object = evidence
            .object
            .clone()
            .ok_or_else(|| ReverseSurfaceError::EvidenceHasNoObject(key.into()))?;
        Ok(SurfaceActionIntent::Reveal(RevealRequest::new(
            object,
            RevealIntent::ActivateExisting,
        )))
    }

    pub fn request_edit(&self, key: &str) -> Result<SurfaceActionIntent, ReverseSurfaceError> {
        let document = self.document()?;
        let consequence = document
            .edit_consequences
            .iter()
            .find(|consequence| consequence.key == key)
            .ok_or_else(|| ReverseSurfaceError::UnknownConsequence(key.into()))?
            .clone();
        let requested_at = self.publication.map(|receipt| receipt.revisions);
        if consequence.authority == EditAuthority::ProjectCommand && requested_at.is_none() {
            return Err(ReverseSurfaceError::MissingPublication);
        }
        Ok(SurfaceActionIntent::ApplyExplicitConsequence {
            document: document.object.clone(),
            consequence,
            requested_at,
        })
    }

    pub fn snapshot(&self) -> ReverseSurfaceSnapshot {
        ReverseSurfaceSnapshot {
            view: self.view,
            load: self.load.clone(),
            selected_comparison: self.selected_comparison,
            channels: self
                .selected_comparison
                .and_then(|id| self.comparison(id).ok())
                .map_or_else(Vec::new, |comparison| self.channels(comparison)),
            publication: self.publication,
            audio: self.audio.clone(),
            selection: self.selection.clone(),
            signal: self.signal,
            semantic: self.semantic,
            controller_phase: self.controller.as_ref().map(|status| status.phase.clone()),
        }
    }

    fn document(&self) -> Result<&ReverseSurfaceDocument, ReverseSurfaceError> {
        match &self.load {
            ReverseSurfaceLoad::Ready(document) => Ok(document),
            ReverseSurfaceLoad::Missing(object) => {
                Err(ReverseSurfaceError::MissingDocument(object.clone()))
            }
            ReverseSurfaceLoad::UnsupportedTarget => Err(ReverseSurfaceError::UnsupportedTarget),
        }
    }

    fn comparison(
        &self,
        id: ComparisonId,
    ) -> Result<&ComparisonSurfaceDocument, ReverseSurfaceError> {
        self.document()?
            .comparison(id)
            .ok_or(ReverseSurfaceError::UnknownComparison(id))
    }

    fn channels(&self, comparison: &ComparisonSurfaceDocument) -> Vec<SurfaceChannelState> {
        let metrics = comparison.observation.as_ref().map(|value| value.metrics);
        let controller = self.controller.as_ref().filter(|status| {
            status
                .selection
                .as_ref()
                .is_some_and(|selection| selection.comparison == comparison.definition.id)
        });
        let selected = controller.and_then(|status| status.selection.as_ref().map(|s| s.channel));
        let active = controller.and_then(|status| {
            (status.phase == ComparisonControllerPhase::Active)
                .then(|| status.selection.as_ref().map(|s| s.channel))
                .flatten()
        });
        [
            ComparisonChannel::Source,
            ComparisonChannel::Construction,
            ComparisonChannel::Residual,
            ComparisonChannel::Excess,
        ]
        .into_iter()
        .map(|channel| {
            channel_state(
                channel,
                metrics,
                comparison.coverage,
                selected,
                active,
                controller,
            )
        })
        .collect()
    }
}

fn first_comparison(load: &ReverseSurfaceLoad) -> Option<ComparisonId> {
    match load {
        ReverseSurfaceLoad::Ready(document) => document
            .comparisons
            .first()
            .map(|comparison| comparison.definition.id),
        ReverseSurfaceLoad::Missing(_) | ReverseSurfaceLoad::UnsupportedTarget => None,
    }
}

fn channel_state(
    channel: ComparisonChannel,
    metrics: Option<ComparisonMetrics>,
    coverage: Option<CoverageSummary>,
    selected: Option<ComparisonChannel>,
    active: Option<ComparisonChannel>,
    controller: Option<&ComparisonControllerStatus>,
) -> SurfaceChannelState {
    let stale = controller
        .is_some_and(|status| matches!(status.phase, ComparisonControllerPhase::Stale(_)));
    let (semantic, availability, measurement) = match channel {
        ComparisonChannel::Source => (
            SurfaceChannelSemantic::Source,
            signal_availability(metrics, stale),
            metrics.map(|metrics| SurfaceChannelMeasurement::SampleEnergy(metrics.source_energy)),
        ),
        ComparisonChannel::Construction => (
            SurfaceChannelSemantic::Construction,
            signal_availability(metrics, stale),
            metrics.map(|metrics| {
                SurfaceChannelMeasurement::SampleEnergy(metrics.construction_energy)
            }),
        ),
        ComparisonChannel::Residual => (
            SurfaceChannelSemantic::ExactResidual,
            signal_availability(metrics, stale),
            metrics.map(|metrics| SurfaceChannelMeasurement::SampleEnergy(metrics.residual_energy)),
        ),
        ComparisonChannel::Excess => (
            SurfaceChannelSemantic::SpectralExcess,
            if stale {
                SurfaceChannelAvailability::Stale
            } else if coverage.is_some() {
                SurfaceChannelAvailability::CoverageOnly
            } else {
                SurfaceChannelAvailability::AwaitingObservation
            },
            coverage.map(|coverage| SurfaceChannelMeasurement::SpectralExcess {
                ratio: coverage.excess_energy_ratio,
                source_power: coverage.source_power,
                construction_power: coverage.construction_power,
            }),
        ),
    };
    SurfaceChannelState {
        channel,
        semantic,
        availability,
        selected: selected == Some(channel),
        active: active == Some(channel),
        measurement,
    }
}

fn signal_availability(
    metrics: Option<ComparisonMetrics>,
    stale: bool,
) -> SurfaceChannelAvailability {
    if stale {
        SurfaceChannelAvailability::Stale
    } else if metrics.is_some() {
        SurfaceChannelAvailability::Auditionable
    } else {
        SurfaceChannelAvailability::AwaitingObservation
    }
}

fn explanation_evidence(index: usize, evidence: &ExplanationEvidenceRef) -> SurfaceEvidence {
    let (label, object) = match evidence {
        ExplanationEvidenceRef::Air(id) => (format!("AIR evidence {}", id.get()), None),
        ExplanationEvidenceRef::Reconstruction { artifact, evidence } => (
            format!(
                "reconstruction evidence {} in {:?}",
                evidence.get(),
                artifact
            ),
            None,
        ),
        ExplanationEvidenceRef::Artifact(artifact) => {
            (format!("analysis artifact {:?}", artifact), None)
        }
    };
    SurfaceEvidence {
        key: format!("evidence:{index}"),
        label,
        object,
        extent: None,
        derivation: Vec::new(),
    }
}

fn validate_body_identity(
    object: &ObjectRef,
    body: &ReverseSurfaceBody,
) -> Result<(), ReverseSurfaceError> {
    let matches = match (object, body) {
        (ObjectRef::Finding(id), ReverseSurfaceBody::Finding(document)) => *id == document.finding,
        (ObjectRef::Explanation(id), ReverseSurfaceBody::Explanation(document)) => {
            *id == document.definition.id
        }
        (ObjectRef::Comparison(id), ReverseSurfaceBody::Comparison(document)) => {
            *id == document.definition.id
        }
        (ObjectRef::Reading(id), ReverseSurfaceBody::Reading(document)) => {
            *id == document.reading.reading_id
        }
        _ => false,
    };
    if matches {
        Ok(())
    } else {
        Err(ReverseSurfaceError::BodyIdentityMismatch)
    }
}

fn normalize_evidence(evidence: &mut Vec<SurfaceEvidence>) -> Result<(), ReverseSurfaceError> {
    evidence.sort_by(|left, right| left.key.cmp(&right.key));
    if evidence
        .iter()
        .any(|evidence| evidence.key.trim().is_empty())
    {
        return Err(ReverseSurfaceError::EmptyEvidenceKey);
    }
    if evidence.windows(2).any(|pair| pair[0].key == pair[1].key) {
        return Err(ReverseSurfaceError::DuplicateEvidenceKey);
    }
    Ok(())
}

fn normalize_consequences(
    consequences: &mut Vec<SurfaceEditConsequence>,
) -> Result<(), ReverseSurfaceError> {
    consequences.sort_by(|left, right| left.key.cmp(&right.key));
    if consequences
        .iter()
        .any(|consequence| consequence.key.trim().is_empty())
    {
        return Err(ReverseSurfaceError::EmptyConsequenceKey);
    }
    if consequences
        .windows(2)
        .any(|pair| pair[0].key == pair[1].key)
    {
        return Err(ReverseSurfaceError::DuplicateConsequenceKey);
    }
    Ok(())
}

fn normalize_comparisons(
    comparisons: &mut Vec<ComparisonSurfaceDocument>,
) -> Result<(), ReverseSurfaceError> {
    comparisons.sort_by_key(|comparison| comparison.definition.id);
    for comparison in comparisons.iter() {
        comparison
            .definition
            .validate()
            .map_err(|error| ReverseSurfaceError::InvalidComparison(error.to_string()))?;
        if comparison.coverage.is_some() && comparison.observation.is_none() {
            return Err(ReverseSurfaceError::CoverageWithoutObservation(
                comparison.definition.id,
            ));
        }
    }
    if comparisons
        .windows(2)
        .any(|pair| pair[0].definition.id == pair[1].definition.id)
    {
        return Err(ReverseSurfaceError::DuplicateComparison);
    }
    Ok(())
}

#[derive(Debug)]
pub enum ReverseSurfaceError {
    EmptyTitle,
    BodyIdentityMismatch,
    EmptyEvidenceKey,
    DuplicateEvidenceKey,
    EmptyConsequenceKey,
    DuplicateConsequenceKey,
    DuplicateComparison,
    InvalidComparison(String),
    InvalidExplanation(String),
    InvalidArtifact(String),
    FindingArtifactScopeMismatch,
    InvalidReading(String),
    CoverageWithoutObservation(ComparisonId),
    DocumentConflict(ObjectRef),
    TargetHydration(String),
    UnsupportedTarget,
    MissingDocument(ObjectRef),
    NoComparison,
    UnknownComparison(ComparisonId),
    MissingObservation(ComparisonId),
    MissingCoverage(ComparisonId),
    MissingPublication,
    UnknownEvidence(String),
    EvidenceHasNoObject(String),
    UnknownConsequence(String),
    Controller(ComparisonControllerError),
}

impl fmt::Display for ReverseSurfaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ReverseSurfaceError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::artifact_catalog::sha256_content;
    use crate::aspect::{Aspect, ChannelMask};
    use crate::assets::{AssetFrameRange, AssetId, SampleFrames};
    use crate::comparison::{ExactRenderDigest, SourceCitation};
    use crate::explanation::{ExplanationDependencyPin, ExplanationId, ExplanationScope};
    use crate::live_project::LiveProject;
    use crate::ontology::{Producer, Provenance};
    use crate::project_controller::{FindingKind, FindingLocalId, FindingScope};
    use crate::reading::{
        PortableDigest, PortableDigestAlgorithm, ProducerDto, ProvenanceDto, ReadingSource,
        READING_FORMAT, READING_FORMAT_VERSION,
    };
    use crate::render_validation::GoldenFingerprint;
    use crate::workspace_document::WorkspaceDocument;

    fn provenance() -> Provenance {
        Provenance {
            producer: Producer::Human { name: None },
            created_unix_ms: None,
            source_revision: None,
            note: None,
        }
    }

    fn observation() -> ComparisonObservation {
        let digest =
            |name: &'static [u8]| ExactRenderDigest::new(sha256_content(name, &[name])).unwrap();
        ComparisonObservation {
            dependencies: ExplanationDependencyPin::default(),
            source_digest: digest(b"source"),
            construction_digest: digest(b"construction"),
            residual_digest: digest(b"residual"),
            construction_fingerprint: fingerprint(),
            residual_fingerprint: fingerprint(),
            metrics: ComparisonMetrics {
                source_energy: 10.0,
                construction_energy: 12.0,
                residual_energy: 2.0,
                ..ComparisonMetrics::default()
            },
        }
    }

    fn fingerprint() -> GoldenFingerprint {
        GoldenFingerprint {
            version: GoldenFingerprint::VERSION,
            sample_rate: 8_000,
            channels: 1,
            frames: 8,
            first_active_offset: Some(0),
            last_active_offset: Some(7),
            peak_millionths: 1,
            rms_millionths: 1,
            dc_millionths: 0,
            block_energy_hash: 1,
        }
    }

    fn comparison() -> ComparisonSurfaceDocument {
        ComparisonSurfaceDocument {
            definition: ComparisonDefinition {
                id: ComparisonId(3),
                label: "null test".into(),
                source: SourceCitation {
                    asset: AssetId(1),
                    source_range: AssetFrameRange::new(SampleFrames(0), SampleFrames(8)).unwrap(),
                    project_span: FrameSpan { start: 4, end: 12 },
                    channels: ChannelMask(1),
                },
                explanation: ExplanationId(2),
                provenance: provenance(),
            },
            observation: Some(observation()),
            coverage: Some(CoverageSummary {
                source_power: 10.0,
                construction_power: 12.0,
                excess_energy_ratio: 0.2,
                ..CoverageSummary::default()
            }),
        }
    }

    fn publication() -> ProjectPublication {
        let live = LiveProject::from_project(
            crate::daw_project::DawProject::new("surface", 8_000, 120.0).unwrap(),
            BTreeMap::new(),
        )
        .unwrap();
        let snapshot = live.snapshot().unwrap();
        ProjectPublication {
            generation: 4,
            revisions: snapshot.revisions(),
            snapshot,
            change_set: None,
        }
    }

    fn reading(reading_id: ReadingId) -> ReadingFile {
        ReadingFile {
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
            source: ReadingSource {
                fingerprints: vec![PortableDigest {
                    algorithm: PortableDigestAlgorithm::Sha256,
                    bytes: [7; 32],
                }],
                sample_rate: 48_000,
                channels: 2,
                frame_count: 100,
                declared_title: Some("portable reading".into()),
                extensions: BTreeMap::new(),
            },
            sections: Vec::new(),
            attachments: Vec::new(),
            extensions: BTreeMap::new(),
        }
    }

    #[test]
    fn explanation_document_keeps_evidence_and_explicit_edit_consequences() {
        let definition = ExplanationDefinition {
            id: ExplanationId(2),
            label: "three-hit reading".into(),
            scope: ExplanationScope::ModelClaim {
                artifact: crate::artifact_catalog::ArtifactId(sha256_content(b"a", &[b"a"])),
                claim: 8,
            },
            extent: Aspect::Time(FrameSpan { start: 4, end: 12 }),
            evidence: vec![ExplanationEvidenceRef::Artifact(
                crate::artifact_catalog::ArtifactId(sha256_content(b"a", &[b"a"])),
            )],
            provenance: provenance(),
        };
        let document = ReverseSurfaceDocument::explanation(definition, vec![comparison()]).unwrap();
        assert_eq!(document.evidence.len(), 1);
        assert_eq!(document.edit_consequences.len(), 1);
        assert_eq!(
            document.edit_consequences[0].invalidates,
            vec![ObjectRef::Comparison(ComparisonId(3))]
        );
        assert_eq!(
            document.edit_consequences[0].authority,
            EditAuthority::InterpretationCommand
        );
    }

    #[test]
    fn pane_shares_session_receipts_and_preserves_exact_channel_semantics() {
        let document = ReverseSurfaceDocument::from_comparison(comparison()).unwrap();
        let mut store = ReverseSurfaceStore::new();
        store.insert(document).unwrap();
        let mut pane = ReverseSurfacePaneModel::open(
            WorkspaceViewId(8),
            ObjectRef::Comparison(ComparisonId(3)),
            &store,
        );
        let mut controller = ComparisonController::new(8).unwrap();
        let mut selection = ProjectSelection::default();
        selection.time = Some(FrameSpan { start: 6, end: 9 });
        let signal = SignalLayer::Residual(crate::aspect::ExplanationRef::Definition(2));
        let mut audio = ProjectAudioStatus::default();
        audio.transport.frame = crate::audio::ProjectFrame(7);
        pane.observe_full_state(
            &PaneSessionSnapshot {
                project: Some(publication()),
                selection: selection.clone(),
                signal,
                selection_revision: 3,
                audio: audio.clone(),
            },
            Some(&mut controller),
        );
        let snapshot = pane.snapshot();
        assert_eq!(snapshot.publication.unwrap().generation, 4);
        assert_eq!(snapshot.selection, selection);
        assert_eq!(snapshot.signal, signal);
        assert_eq!(snapshot.audio, audio);
        let residual = snapshot
            .channels
            .iter()
            .find(|channel| channel.channel == ComparisonChannel::Residual)
            .unwrap();
        assert_eq!(residual.semantic, SurfaceChannelSemantic::ExactResidual);
        assert_eq!(
            residual.availability,
            SurfaceChannelAvailability::Auditionable
        );
        let excess = snapshot
            .channels
            .iter()
            .find(|channel| channel.channel == ComparisonChannel::Excess)
            .unwrap();
        assert_eq!(excess.semantic, SurfaceChannelSemantic::SpectralExcess);
        assert_eq!(
            excess.availability,
            SurfaceChannelAvailability::CoverageOnly
        );

        assert!(matches!(
            pane.request_channel(ComparisonChannel::Residual, &mut controller)
                .unwrap(),
            SurfaceAuditionIntent::Signal(ComparisonSelectionRequest {
                channel: ComparisonChannel::Residual,
                ..
            })
        ));
        let excess = pane
            .request_channel(ComparisonChannel::Excess, &mut controller)
            .unwrap();
        assert!(matches!(
            excess,
            SurfaceAuditionIntent::InspectExcess {
                comparison: ComparisonId(3),
                controller: ComparisonSelectionRequest {
                    channel: ComparisonChannel::Excess,
                    ..
                },
                ..
            }
        ));
        assert_eq!(
            controller.status().selection.unwrap().channel,
            ComparisonChannel::Excess
        );
    }

    #[test]
    fn finding_reveal_descriptor_reopens_the_same_working_document() {
        let finding = FindingRef {
            kind: FindingKind::Rhythm,
            scope: FindingScope::Derivation(crate::sample_material::DerivationScope(42)),
            local: FindingLocalId::Claim(7),
        };
        let document = ReverseSurfaceDocument::finding(
            FindingSurfaceDocument {
                finding,
                label: "syncopation candidate".into(),
                artifact: None,
                extent: Some(FrameSpan { start: 10, end: 20 }),
                statements: vec!["anonymous family 2 repeats".into()],
            },
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        let mut store = ReverseSurfaceStore::new();
        store.insert(document).unwrap();
        let request =
            RevealRequest::new(ObjectRef::Finding(finding), RevealIntent::ActivateExisting);
        let plan = crate::project_controller::ObjectNavigator::plan(
            &WorkspaceDocument::default(),
            request,
        );
        let descriptor = match plan.workspace {
            crate::project_controller::WorkspaceReveal::Create(descriptor) => {
                let mut workspace = WorkspaceDocument::default();
                let id = workspace.create_view(descriptor).unwrap();
                workspace.views[&id].clone()
            }
            crate::project_controller::WorkspaceReveal::Retarget { descriptor, .. } => descriptor,
            crate::project_controller::WorkspaceReveal::Activate { view, .. } => {
                WorkspaceDocument::default().views[&view].clone()
            }
            other => panic!("unexpected reveal {other:?}"),
        };
        let reopened = ReverseSurfacePaneModel::reopen(&descriptor, &store).unwrap();
        assert!(matches!(
            reopened.snapshot().load,
            ReverseSurfaceLoad::Ready(_)
        ));
    }

    #[test]
    fn reading_edit_is_an_explicit_fork_and_missing_content_is_not_a_placeholder() {
        let reading_id = ReadingId::new([5; 16]).unwrap();
        let mut store = ReverseSurfaceStore::new();
        store
            .insert(
                ReverseSurfaceDocument::reading(
                    reading(reading_id),
                    Ok(VerificationTier::GraphOnly),
                )
                .unwrap(),
            )
            .unwrap();
        let pane = ReverseSurfacePaneModel::open(
            WorkspaceViewId(9),
            ObjectRef::Reading(reading_id),
            &store,
        );
        assert!(matches!(pane.snapshot().load, ReverseSurfaceLoad::Ready(_)));
        assert!(matches!(
            pane.request_edit("fork-reading").unwrap(),
            SurfaceActionIntent::ApplyExplicitConsequence {
                consequence: SurfaceEditConsequence {
                    authority: EditAuthority::ReadingFork(actual),
                    ..
                },
                requested_at: None,
                ..
            } if actual == reading_id
        ));

        let missing_id = ReadingId::new([6; 16]).unwrap();
        let missing = ReverseSurfacePaneModel::open(
            WorkspaceViewId(10),
            ObjectRef::Reading(missing_id),
            &store,
        );
        assert_eq!(
            missing.snapshot().load,
            ReverseSurfaceLoad::Missing(ObjectRef::Reading(missing_id))
        );
    }
}
